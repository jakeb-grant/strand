//! `attach: top` concave fillets (design.md: "Concave fillets, so a
//! panel grows out of the bar or screen edge", `attach:` on a popup or
//! panel).
//!
//! A surface with `attach: <edge>` sits flush against that edge (the
//! surface manager's placement puts its box at gap 0). Its fillet target
//! is the outermost box with a background touching that edge (the root
//! itself when it paints one): that box's corners on the attached side
//! are drawn square, and beside each of them, outside the box, a concave
//! quarter-circle fillet of that corner's radius joins the box's side to
//! the edge, in the box's paint. The fillets lie past the box, so render
//! adds their reach to the surface's overhang on the two sides along the
//! edge ([`grow`]); the input region stays the box. Nothing reaches past
//! the attached edge, so the overhang there is none ([`flush`]).

use strand_scene::{Edge, Insets, LogicalRect, NodeId, NodeKind, Prop, PropValue, TokenScope};
use vello_cpu::kurbo::{BezPath, Point, Rect, RoundedRectRadii};

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
}
