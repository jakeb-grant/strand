//! `attach: top` concave fillets (design.md: "Concave fillets, so a
//! panel grows out of the bar or screen edge", `attach:` on a popup or
//! panel).
//!
//! A surface with `attach: <edge>` sits against that edge (the
//! surface manager's placement puts its box at its margin on that side,
//! 0 while unset, or over it when negative). Its fillet target
//! is the outermost box with a background touching that edge (the root
//! itself when it paints one): that box's corners on the attached side
//! are drawn square, and beside each of them, outside the box, a concave
//! quarter-circle fillet of that corner's radius joins the box's side to
//! the edge, in the box's paint. The fillets lie past the box, so render
//! adds their reach to the surface's overhang on the two sides along the
//! edge ([`grow`]); the input region stays the box. Nothing reaches past
//! the attached edge, so the overhang there is none ([`flush`]).
//!
//! (fillet-clip) The fillets lie outside the target's box, so outside a
//! clip above it that holds it to its box (a `clip: true` stack around
//! the card, a scroll, a box whose size springs): each such clip lets
//! the target's fill through at its fillets and the square corners they
//! meet, at the target's corners on the edge its box reaches and no
//! further from the edge than that box ([`corners`]), and what else it
//! holds stays inside its box.

use strand_scene::{Edge, Insets, LogicalRect, NodeId, NodeKind, Prop, PropValue, TokenScope};
use vello_cpu::kurbo::{BezPath, CubicBez, ParamCurve, Point, Rect, RoundedRectRadii};

use crate::layout::Boxes;
use crate::tree::SceneTree;

/// The cubic Bézier handle length of a quarter circle of radius 1.
const KAPPA: f64 = 0.552_284_749_830_793_4;

/// The fillet a surface draws: on which node, towards which edge, and the
/// target's two corner radii beside that edge (logical pixels, in the
/// order the edge runs: left to right for `top`/`bottom`, top to bottom
/// for `left`/`right`).
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct Fillet {
    pub node: NodeId,
    pub edge: Edge,
    pub radii: (f64, f64),
}

/// The edge `root` attaches to: `attach:` on a `panel` or `popup`.
pub(crate) fn attach_edge(tree: &SceneTree, root: NodeId) -> Option<Edge> {
    let node = tree.get(root)?;
    if !matches!(node.kind, NodeKind::Panel | NodeKind::Popup) {
        return None;
    }
    match node.get(Prop::Attach)? {
        PropValue::Keyword(k) => Edge::from_name(k),
        _ => None,
    }
}

/// The fillet of the surface rooted at `root`, as laid out in `boxes`:
/// `None` without `attach:` or without a painted box touching the edge.
pub(crate) fn find(tree: &SceneTree, boxes: &Boxes, root: NodeId) -> Option<Fillet> {
    let edge = attach_edge(tree, root)?;
    let outer = *boxes.rects.get(&root)?;
    // Breadth first: the outermost painted box touching the edge.
    let mut level = vec![root];
    for _ in 0..64 {
        let mut next = Vec::new();
        for id in level {
            let Some(node) = tree.get(id) else { continue };
            let Some(rect) = boxes.rects.get(&id) else {
                continue;
            };
            if !touches(rect, &outer, edge) {
                continue;
            }
            if node.get(Prop::Bg).is_some() {
                let r = corner_radii(tree, id, rect);
                let radii = match edge {
                    Edge::Top => (r.top_left, r.top_right),
                    Edge::Bottom => (r.bottom_left, r.bottom_right),
                    Edge::Left => (r.top_left, r.bottom_left),
                    Edge::Right => (r.top_right, r.bottom_right),
                };
                return Some(Fillet {
                    node: id,
                    edge,
                    radii,
                });
            }
            next.extend(node.children.iter().copied());
        }
        if next.is_empty() {
            return None;
        }
        level = next;
    }
    None
}

/// `rect` lies along `outer`'s `edge` (within half a pixel).
fn touches(rect: &LogicalRect, outer: &LogicalRect, edge: Edge) -> bool {
    let near = |a: f32, b: f32| (a - b).abs() <= 0.5;
    match edge {
        Edge::Top => near(rect.y, outer.y),
        Edge::Bottom => near(rect.y + rect.h, outer.y + outer.h),
        Edge::Left => near(rect.x, outer.x),
        Edge::Right => near(rect.x + rect.w, outer.x + outer.w),
    }
}

/// `id`'s corner radii in logical pixels, its `radius` resolved through
/// the tokens of its ancestors and shrunk to fit like render draws them.
fn corner_radii(tree: &SceneTree, id: NodeId, rect: &LogicalRect) -> RoundedRectRadii {
    let mut chain = Vec::new();
    let mut up = Some(id);
    while let Some(n) = up.and_then(|i| tree.get(i)) {
        chain.push(n);
        up = n.parent;
    }
    let mut levels = vec![&tree.tokens];
    for n in chain.iter().rev() {
        if let Some(PropValue::Tokens(t)) = n.get(Prop::Tokens) {
            levels.push(t);
        }
    }
    let scope = TokenScope::new(&levels);
    let value = tree
        .get(id)
        .and_then(|n| n.get(Prop::Radius))
        .and_then(|v| scope.resolve(v));
    let corners = crate::flatten::corners_of(value.as_deref(), rect.w, rect.h);
    crate::flatten::radii(corners, f64::from(rect.w), f64::from(rect.h), 1.0)
}

/// `o` grown by the fillets' reach past the root's box on the two sides
/// along the attached edge (a fillet beside a target inset from the
/// root's side reaches less far).
pub(crate) fn grow(tree: &SceneTree, boxes: &Boxes, root: NodeId, o: Insets) -> Insets {
    let Some(f) = find(tree, boxes, root) else {
        return o;
    };
    let (Some(outer), Some(t)) = (boxes.rects.get(&root), boxes.rects.get(&f.node)) else {
        return o;
    };
    let reach = |r: f64, inset: f32| ((r as f32) - inset).max(0.0).ceil();
    let mut o = o;
    match f.edge {
        Edge::Top | Edge::Bottom => {
            o.left = o.left.max(reach(f.radii.0, t.x - outer.x));
            o.right = o
                .right
                .max(reach(f.radii.1, (outer.x + outer.w) - (t.x + t.w)));
        }
        Edge::Left | Edge::Right => {
            o.top = o.top.max(reach(f.radii.0, t.y - outer.y));
            o.bottom = o
                .bottom
                .max(reach(f.radii.1, (outer.y + outer.h) - (t.y + t.h)));
        }
    }
    o
}

/// `o` with nothing past the attached edge: the box is flush with it, so
/// a shadow there would only be cut off by the screen's edge.
pub(crate) fn flush(tree: &SceneTree, root: NodeId, o: Insets) -> Insets {
    let mut o = o;
    match attach_edge(tree, root) {
        Some(Edge::Top) => o.top = 0.0,
        Some(Edge::Bottom) => o.bottom = 0.0,
        Some(Edge::Left) => o.left = 0.0,
        Some(Edge::Right) => o.right = 0.0,
        None => {}
    }
    o
}

/// The target's fill shape in physical pixels: `frame` with radii `r`,
/// its corners on `edge` square, plus a concave fillet of each of those
/// corners' radius beside it, outside the box.
pub(crate) fn path(frame: Rect, r: RoundedRectRadii, edge: Edge) -> BezPath {
    // Built for the top edge in (u, v): u along the edge, v away from it.
    let (len, depth) = match edge {
        Edge::Top | Edge::Bottom => (frame.width(), frame.height()),
        Edge::Left | Edge::Right => (frame.height(), frame.width()),
    };
    // a, b: the corners on the edge (the fillets); c, d: the far corners,
    // at the end and at the start of the edge.
    let (a, b, c, d) = match edge {
        Edge::Top => (r.top_left, r.top_right, r.bottom_right, r.bottom_left),
        Edge::Bottom => (r.bottom_left, r.bottom_right, r.top_right, r.top_left),
        Edge::Left => (r.top_left, r.bottom_left, r.bottom_right, r.top_right),
        Edge::Right => (r.top_right, r.bottom_right, r.bottom_left, r.top_left),
    };
    let map = |u: f64, v: f64| -> Point {
        match edge {
            Edge::Top => Point::new(frame.x0 + u, frame.y0 + v),
            Edge::Bottom => Point::new(frame.x0 + u, frame.y1 - v),
            Edge::Left => Point::new(frame.x0 + v, frame.y0 + u),
            Edge::Right => Point::new(frame.x1 - v, frame.y0 + u),
        }
    };
    let k = KAPPA;
    let mut p = BezPath::new();
    // Along the edge, from the start fillet's tip to the end one's.
    p.move_to(map(-a, 0.0));
    p.line_to(map(len + b, 0.0));
    // The end fillet: concave, centred at (len + b, b).
    if b > 0.0 {
        p.curve_to(map(len + b - k * b, 0.0), map(len, b - k * b), map(len, b));
    }
    // Down the end side to the far corner, rounded convexly.
    p.line_to(map(len, depth - c));
    if c > 0.0 {
        p.curve_to(
            map(len, depth - c + k * c),
            map(len - c + k * c, depth),
            map(len - c, depth),
        );
    }
    p.line_to(map(d, depth));
    if d > 0.0 {
        p.curve_to(
            map(d - k * d, depth),
            map(0.0, depth - d + k * d),
            map(0.0, depth - d),
        );
    }
    // Up the start side to the start fillet, concave, centred at (-a, a).
    p.line_to(map(0.0, a));
    if a > 0.0 {
        p.curve_to(map(0.0, a - k * a), map(-a + k * a, 0.0), map(-a, 0.0));
    }
    p.close_path();
    p
}

/// What a clip above the target lets through (fillet-clip): at each of
/// its corners on `edge` with a fillet that lies on or inside `clip`
/// (the clip's box in the target's space, within half a pixel), that
/// fillet (the concave region [`path`] adds outside `frame`) and the
/// square corner it meets, `side` on a side but at least the fillet's
/// radius, inside `frame`; both end where `clip` ends away from the
/// edge, and the square where it ends along it, so a clip never lets
/// through what lies past its other sides. One closed subpath each
/// (none for a square corner or one the clip does not reach).
pub(crate) fn corners(
    frame: Rect,
    r: RoundedRectRadii,
    edge: Edge,
    side: f64,
    clip: Rect,
) -> Vec<BezPath> {
    let (len, depth) = match edge {
        Edge::Top | Edge::Bottom => (frame.width(), frame.height()),
        Edge::Left | Edge::Right => (frame.height(), frame.width()),
    };
    let (a, b) = match edge {
        Edge::Top => (r.top_left, r.top_right),
        Edge::Bottom => (r.bottom_left, r.bottom_right),
        Edge::Left => (r.top_left, r.bottom_left),
        Edge::Right => (r.top_right, r.bottom_right),
    };
    let map = |u: f64, v: f64| -> Point {
        match edge {
            Edge::Top => Point::new(frame.x0 + u, frame.y0 + v),
            Edge::Bottom => Point::new(frame.x0 + u, frame.y1 - v),
            Edge::Left => Point::new(frame.x0 + v, frame.y0 + u),
            Edge::Right => Point::new(frame.x1 - v, frame.y0 + u),
        }
    };
    // The clip in (u, v): u along the edge, v away from it.
    let (u0, u1, v0, v1) = {
        let (p, q) = match edge {
            Edge::Top => (
                (clip.x0 - frame.x0, clip.y0 - frame.y0),
                (clip.x1 - frame.x0, clip.y1 - frame.y0),
            ),
            Edge::Bottom => (
                (clip.x0 - frame.x0, frame.y1 - clip.y1),
                (clip.x1 - frame.x0, frame.y1 - clip.y0),
            ),
            Edge::Left => (
                (clip.y0 - frame.y0, clip.x0 - frame.x0),
                (clip.y1 - frame.y0, clip.x1 - frame.x0),
            ),
            Edge::Right => (
                (clip.y0 - frame.y0, frame.x1 - clip.x1),
                (clip.y1 - frame.y0, frame.x1 - clip.x0),
            ),
        };
        (p.0.min(q.0), p.0.max(q.0), p.1.min(q.1), p.1.max(q.1))
    };
    const HALF: f64 = 0.5;
    let reaches = |u: f64| u0 - HALF <= u && u <= u1 + HALF && v0 - HALF <= 0.0 && 0.0 <= v1 + HALF;
    // How deep the clip lets the corner through, away from the edge.
    let deep = v1.min(depth);
    // The square corner's side: `side`, at least the radius, within the
    // box; and its reach along the edge, within the clip.
    let square = |radius: f64| side.max(radius).min(len).min(depth).max(0.0);
    let k = KAPPA;
    let mut out = Vec::new();
    // The start fillet, centred at (-a, a): along the edge from its tip
    // over the square corner, round the square, up the box's side to
    // the fillet (or to where the clip ends) and along its arc back to
    // the tip.
    if a > 0.0 && reaches(0.0) && deep > 0.0 {
        let s = square(a);
        let (su, sv) = (s.min(u1), s.min(deep));
        let arc = CubicBez::new(
            Point::new(0.0, a),
            Point::new(0.0, a - k * a),
            Point::new(-a + k * a, 0.0),
            Point::new(-a, 0.0),
        );
        // From where it leaves the box's side (v falls along it).
        let arc = if deep < a {
            arc.subsegment(at_depth(&arc, deep)..1.0)
        } else {
            arc
        };
        if su > 0.0 {
            let mut p = BezPath::new();
            p.move_to(map(-a, 0.0));
            p.line_to(map(su, 0.0));
            p.line_to(map(su, sv));
            p.line_to(map(0.0, sv));
            p.line_to(map(arc.p0.x, arc.p0.y));
            p.curve_to(
                map(arc.p1.x, arc.p1.y),
                map(arc.p2.x, arc.p2.y),
                map(-a, 0.0),
            );
            p.close_path();
            out.push(p);
        }
    }
    // The end fillet, centred at (len + b, b), the same way round.
    if b > 0.0 && reaches(len) && deep > 0.0 {
        let s = square(b);
        let (su, sv) = (s.min(len - u0), s.min(deep));
        let arc = CubicBez::new(
            Point::new(len + b, 0.0),
            Point::new(len + b - k * b, 0.0),
            Point::new(len, b - k * b),
            Point::new(len, b),
        );
        // To where it meets the box's side (v rises along it).
        let arc = if deep < b {
            arc.subsegment(0.0..at_depth(&arc, deep))
        } else {
            arc
        };
        if su > 0.0 {
            let mut p = BezPath::new();
            p.move_to(map(len - su, 0.0));
            p.line_to(map(len + b, 0.0));
            p.curve_to(
                map(arc.p1.x, arc.p1.y),
                map(arc.p2.x, arc.p2.y),
                map(arc.p3.x, arc.p3.y),
            );
            p.line_to(map(len, sv));
            p.line_to(map(len - su, sv));
            p.close_path();
            out.push(p);
        }
    }
    out
}

/// The parameter where fillet arc `arc`, monotonic in v (its `y`),
/// is `v` deep.
fn at_depth(arc: &CubicBez, v: f64) -> f64 {
    let rising = arc.p3.y > arc.p0.y;
    let (mut lo, mut hi) = (0.0, 1.0);
    for _ in 0..40 {
        let mid = 0.5 * (lo + hi);
        if (arc.eval(mid).y < v) == rising {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vello_cpu::kurbo::Shape;

    fn inside(p: &BezPath, x: f64, y: f64) -> bool {
        p.winding(Point::new(x, y)) != 0
    }

    /// Top: the fillets reach `radius` past the box on both sides, the
    /// box's top corners are square, its bottom ones still rounded, and
    /// each fillet is concave (empty near the edge's outer corner).
    #[test]
    fn the_top_path_has_square_corners_and_concave_fillets() {
        let frame = Rect::new(20.0, 0.0, 120.0, 50.0);
        let p = path(frame, RoundedRectRadii::from_single_radius(16.0), Edge::Top);
        let bb = p.bounding_box();
        assert!(
            (bb.x0 - 4.0).abs() < 1e-6 && (bb.x1 - 136.0).abs() < 1e-6,
            "{bb:?}"
        );
        assert!(bb.y0.abs() < 1e-6 && (bb.y1 - 50.0).abs() < 1e-6);
        assert!(inside(&p, 20.5, 0.5), "the top-left corner is square");
        assert!(inside(&p, 119.5, 0.5), "the top-right corner is square");
        assert!(!inside(&p, 20.5, 49.5), "the bottom-left stays round");
        assert!(inside(&p, 19.0, 0.5), "the fillet meets the edge");
        assert!(inside(&p, 121.0, 0.5));
        assert!(!inside(&p, 19.0, 14.0), "concave: empty by the box's side");
        assert!(!inside(&p, 122.0, 14.0));
        assert!(!inside(&p, 70.0, 51.0));
    }

    /// Each edge is the top one turned: the fillets go along that edge.
    #[test]
    fn every_edge_puts_the_fillets_along_it() {
        let frame = Rect::new(20.0, 20.0, 120.0, 70.0);
        let r = RoundedRectRadii::from_single_radius(10.0);
        let bottom = path(frame, r, Edge::Bottom);
        assert!(inside(&bottom, 15.0, 69.5) && !inside(&bottom, 15.0, 20.5));
        assert!(inside(&bottom, 125.0, 69.5));
        let left = path(frame, r, Edge::Left);
        assert!(inside(&left, 20.5, 15.0) && inside(&left, 20.5, 75.0));
        assert!(!inside(&left, 119.5, 15.0));
        let right = path(frame, r, Edge::Right);
        assert!(inside(&right, 119.5, 15.0) && inside(&right, 119.5, 75.0));
        assert!(!inside(&right, 20.5, 15.0));
        // Square corners make no fillets: the box itself.
        let square = path(frame, RoundedRectRadii::from_single_radius(0.0), Edge::Top);
        assert_eq!(square.bounding_box(), frame);
    }

    /// (fillet-clip) The corners are what the path adds beside the box
    /// and the square corners they meet: inside the path and outside
    /// the frame, or inside both squares' reach of the corner, wherever
    /// they are tested; the square is at least the fillet's radius.
    #[test]
    fn the_corners_are_the_fillets_and_the_square_corners_they_meet() {
        let frame = Rect::new(20.0, 20.0, 120.0, 70.0);
        let r = RoundedRectRadii::new(10.0, 6.0, 4.0, 8.0);
        for side in [0.0, 12.0] {
            for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
                let whole = path(frame, r, edge);
                let c = corners(frame, r, edge, side, frame.inflate(40.0, 40.0));
                assert_eq!(c.len(), 2, "{edge:?}");
                // The corners on the edge, with their radii.
                let (start, end) = match edge {
                    Edge::Top => ((20.0, 20.0, 10.0), (120.0, 20.0, 6.0)),
                    Edge::Bottom => ((20.0, 70.0, 8.0), (120.0, 70.0, 4.0)),
                    Edge::Left => ((20.0, 20.0, 10.0), (20.0, 70.0, 8.0)),
                    Edge::Right => ((120.0, 20.0, 6.0), (120.0, 70.0, 4.0)),
                };
                let near = |(cx, cy, rad): (f64, f64, f64), x: f64, y: f64| {
                    let s = f64::max(side, rad);
                    (x - cx).abs() < s && (y - cy).abs() < s
                };
                for y in 0..90 {
                    for x in 0..140 {
                        let (px, py) = (f64::from(x) + 0.5, f64::from(y) + 0.5);
                        let pt = Point::new(px, py);
                        let in_corner = c.iter().any(|p| inside(p, px, py));
                        let fillet = inside(&whole, px, py) && !frame.contains(pt);
                        let square =
                            frame.contains(pt) && (near(start, px, py) || near(end, px, py));
                        assert_eq!(
                            in_corner,
                            fillet || square,
                            "{edge:?} side {side} at ({px}, {py})"
                        );
                    }
                }
            }
        }
        let none = corners(
            frame,
            RoundedRectRadii::from_single_radius(0.0),
            Edge::Top,
            8.0,
            frame,
        );
        assert!(none.is_empty());
    }

    /// (fillet-clip) A clip lets through only the corners it reaches,
    /// and nothing past its box but the fillets beside them, cut where
    /// it ends away from the edge.
    #[test]
    fn a_clip_lets_through_only_the_corners_it_reaches() {
        let frame = Rect::new(20.0, 20.0, 120.0, 70.0);
        let r = RoundedRectRadii::from_single_radius(10.0);
        let any = |c: &[BezPath], x: f64, y: f64| c.iter().any(|p| inside(p, x, y));
        // The card's own box: both corners, whole.
        assert_eq!(corners(frame, r, Edge::Top, 12.0, frame).len(), 2);
        // Half a pixel short of a corner still reaches it.
        assert_eq!(
            corners(
                frame,
                r,
                Edge::Top,
                12.0,
                Rect::new(20.4, 20.4, 119.6, 70.0)
            )
            .len(),
            2
        );
        // Narrow: from the left side to x 25, the left corner alone,
        // its square within the clip.
        let narrow = Rect::new(20.0, 20.0, 25.0, 70.0);
        let c = corners(frame, r, Edge::Top, 12.0, narrow);
        assert_eq!(c.len(), 1);
        assert!(any(&c, 15.5, 20.5), "the left fillet");
        assert!(any(&c, 24.5, 20.5), "the square, within the clip");
        assert!(!any(&c, 25.5, 20.5), "not past the clip's right side");
        assert!(!any(&c, 119.5, 20.5), "nor at the right corner");
        // Clear of both corners: nothing.
        let middle = Rect::new(40.0, 20.0, 100.0, 70.0);
        assert!(corners(frame, r, Edge::Top, 12.0, middle).is_empty());
        // Off the edge: nothing.
        let below = Rect::new(0.0, 22.0, 140.0, 70.0);
        assert!(corners(frame, r, Edge::Top, 12.0, below).is_empty());
        // Short: 4 deep, so are the squares and the fillets, every edge.
        for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            let short = match edge {
                Edge::Top => Rect::new(20.0, 20.0, 120.0, 24.0),
                Edge::Bottom => Rect::new(20.0, 66.0, 120.0, 70.0),
                Edge::Left => Rect::new(20.0, 20.0, 24.0, 70.0),
                Edge::Right => Rect::new(116.0, 20.0, 120.0, 70.0),
            };
            let c = corners(frame, r, edge, 12.0, short);
            assert_eq!(c.len(), 2, "{edge:?}");
            let whole = path(frame, r, edge);
            for y in 0..90 {
                for x in 0..140 {
                    let (px, py) = (f64::from(x) + 0.5, f64::from(y) + 0.5);
                    let deep = match edge {
                        Edge::Top => py - 20.0,
                        Edge::Bottom => 70.0 - py,
                        Edge::Left => px - 20.0,
                        Edge::Right => 120.0 - px,
                    };
                    let along = match edge {
                        Edge::Top | Edge::Bottom => px - 20.0,
                        Edge::Left | Edge::Right => py - 20.0,
                    };
                    let len = match edge {
                        Edge::Top | Edge::Bottom => 100.0,
                        Edge::Left | Edge::Right => 50.0,
                    };
                    let in_corner = any(&c, px, py);
                    let want = inside(&whole, px, py)
                        && deep < 4.0
                        && (along < 12.0 || along > len - 12.0);
                    assert_eq!(in_corner, want, "{edge:?} at ({px}, {py})");
                }
            }
        }
    }
}
