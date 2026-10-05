//! Retained tree → display list for one surface, plus a per-node record of
//! physical bounds and a paint signature for damage diffing.
//!
//! Layout here is the M0 placeholder: nodes are placed at `x`/`y` inside
//! their parent and sized by `width`/`height`/`size` (text nodes by their
//! shaped layout). Flex layout with taffy replaces it in M2.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use strand_scene::{
    Border, Color, Corners, Font, Length, LogicalRect, NodeId, NodeKind, Paint, Prop, PropValue,
    Rect, Scale, Shadow, Size,
};
use strand_text::{TextAlign, TextLayout, TextStyle};
use vello_cpu::kurbo::{self, BezPath, RoundedRect, RoundedRectRadii, Shape};

use crate::tree::{Node, SceneTree};

/// Curve flattening tolerance in physical pixels.
const TOLERANCE: f64 = 0.1;

/// What a text node needs shaped.
#[derive(Clone, Debug, PartialEq)]
pub struct TextSpec {
    pub text: String,
    pub style: TextStyle,
    pub max_width: Option<f32>,
    pub scale: Scale,
}

/// One drawing command in physical pixels.
#[derive(Clone, Debug)]
pub enum Item {
    PushClip(BezPath),
    PopClip,
    PushOpacity(f32),
    PopOpacity,
    /// A blurred rounded rect, clipped to outside the casting box.
    Shadow {
        rect: kurbo::Rect,
        radius: f32,
        std_dev: f32,
        color: Color,
        /// Area the shadow may cover minus the casting box (even-odd).
        clip: BezPath,
    },
    Fill {
        shape: FillShape,
        paint: Paint,
        /// Box the paint's gradient geometry is relative to.
        frame: kurbo::Rect,
    },
    /// An even-odd ring.
    Border {
        path: BezPath,
        paint: Paint,
        frame: kurbo::Rect,
    },
    Glyphs {
        x: i32,
        y: i32,
        layout: Arc<TextLayout>,
        color: Color,
    },
}

#[derive(Clone, Debug)]
pub enum FillShape {
    Rect(kurbo::Rect),
    Path(BezPath),
}

/// A display item with the physical rectangle it can touch (push/pop items
/// span the whole surface, so culling never unbalances them).
#[derive(Clone, Debug)]
pub struct DisplayItem {
    pub item: Item,
    pub bounds: Rect,
}

/// What damage diffing remembers about a node between frames.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct NodeRecord {
    pub bounds: Rect,
    pub sig: u64,
}

#[derive(Debug, Default)]
pub struct Flattened {
    pub items: Vec<DisplayItem>,
    pub records: HashMap<NodeId, NodeRecord>,
    /// Text nodes and the shaping they need at this scale.
    pub text: Vec<(NodeId, TextSpec)>,
}

#[derive(Clone)]
struct Inherited {
    color: Color,
    font: Font,
    /// Hash of everything above this node that affects how it paints
    /// (opacity, clips, paint-order epochs).
    ctx: u64,
    /// Accumulated clip in physical pixels.
    clip: Rect,
}

/// Flattens the subtree under `root` for a surface of `size` at `scale`.
/// `layouts` holds the last delivered text layout per node.
pub fn flatten(
    tree: &SceneTree,
    root: NodeId,
    size: Size,
    scale: Scale,
    layouts: &HashMap<NodeId, Arc<TextLayout>>,
) -> Flattened {
    let mut out = Flattened::default();
    let Some(node) = tree.get(root) else {
        return out;
    };
    let full = Rect::from_size(size);
    let logical = scale.logical_size(size);
    let mut f = Flattener {
        tree,
        scale,
        surface: full,
        layouts,
        out: &mut out,
    };
    let inh = Inherited {
        color: Color::BLACK,
        font: Font::default(),
        ctx: 0,
        clip: full,
    };
    f.node(
        node,
        LogicalRect::new(0.0, 0.0, logical.w, logical.h),
        &inh,
        true,
    );
    out
}

struct Flattener<'a> {
    tree: &'a SceneTree,
    scale: Scale,
    surface: Rect,
    layouts: &'a HashMap<NodeId, Arc<TextLayout>>,
    out: &'a mut Flattened,
}

fn number(v: Option<&PropValue>) -> Option<f32> {
    match v? {
        PropValue::Number(n) => Some(*n),
        PropValue::Length(Length::Px(n)) => Some(*n),
        _ => None,
    }
}

fn length(v: Option<&PropValue>, reference: f32) -> Option<f32> {
    match v? {
        PropValue::Number(n) => Some(*n),
        PropValue::Length(Length::Px(n)) => Some(*n),
        PropValue::Length(Length::Percent(p)) => Some(reference * p / 100.0),
        _ => None,
    }
}

fn paint_of(v: Option<&PropValue>) -> Option<Paint> {
    match v? {
        PropValue::Color(c) => Some(Paint::Solid(*c)),
        PropValue::Paint(p) => Some(p.clone()),
        _ => None,
    }
}

fn corners_of(v: Option<&PropValue>) -> Corners {
    match v {
        Some(PropValue::Corners(c)) => *c,
        Some(PropValue::Number(n)) | Some(PropValue::Length(Length::Px(n))) => Corners::all(*n),
        _ => Corners::default(),
    }
}

/// Scales radii to physical pixels and shrinks them like CSS so adjacent
/// corners never overlap (`radius: full` becomes a pill).
fn radii(c: Corners, w: f64, h: f64, s: f64) -> RoundedRectRadii {
    let (tl, tr, br, bl) = (
        (c.top_left as f64 * s).max(0.0),
        (c.top_right as f64 * s).max(0.0),
        (c.bottom_right as f64 * s).max(0.0),
        (c.bottom_left as f64 * s).max(0.0),
    );
    let ratio = |side: f64, a: f64, b: f64| if a + b > side { side / (a + b) } else { 1.0 };
    let f = ratio(w, tl, tr)
        .min(ratio(w, bl, br))
        .min(ratio(h, tl, bl))
        .min(ratio(h, tr, br));
    RoundedRectRadii::new(tl * f, tr * f, br * f, bl * f)
}

fn radii_zero(r: &RoundedRectRadii) -> bool {
    r.top_left <= 0.0 && r.top_right <= 0.0 && r.bottom_right <= 0.0 && r.bottom_left <= 0.0
}

fn shape_path(rect: kurbo::Rect, r: RoundedRectRadii) -> BezPath {
    if radii_zero(&r) {
        rect.to_path(TOLERANCE)
    } else {
        RoundedRect::from_rect(rect, r).to_path(TOLERANCE)
    }
}

fn kurbo_rect(r: Rect) -> kurbo::Rect {
    kurbo::Rect::new(
        r.left() as f64,
        r.top() as f64,
        r.right() as f64,
        r.bottom() as f64,
    )
}

/// Smallest pixel rectangle covering a float rectangle.
fn cover(r: kurbo::Rect) -> Rect {
    Rect::from_edges(
        r.x0.floor() as i64,
        r.y0.floor() as i64,
        r.x1.ceil() as i64,
        r.y1.ceil() as i64,
    )
}

fn hash_f32(h: &mut impl Hasher, v: f32) {
    v.to_bits().hash(h);
}

fn hash_color(h: &mut impl Hasher, c: &Color) {
    for v in [c.r, c.g, c.b, c.a] {
        hash_f32(h, v);
    }
}

fn hash_stops(h: &mut impl Hasher, stops: &[strand_scene::GradientStop]) {
    stops.len().hash(h);
    for st in stops {
        hash_f32(h, st.offset);
        hash_color(h, &st.color);
    }
}

fn hash_paint(h: &mut impl Hasher, p: &Paint) {
    match p {
        Paint::Solid(c) => {
            0u8.hash(h);
            hash_color(h, c);
        }
        Paint::Linear { angle, stops } => {
            1u8.hash(h);
            hash_f32(h, *angle);
            hash_stops(h, stops);
        }
        Paint::Radial { stops } => {
            2u8.hash(h);
            hash_stops(h, stops);
        }
        Paint::Conic { from, stops } => {
            3u8.hash(h);
            hash_f32(h, *from);
            hash_stops(h, stops);
        }
    }
}

fn hash_rect(h: &mut impl Hasher, r: kurbo::Rect) {
    for v in [r.x0, r.y0, r.x1, r.y1] {
        v.to_bits().hash(h);
    }
}

fn hash_path(h: &mut impl Hasher, p: &BezPath) {
    use kurbo::PathEl;
    let mut pt = |p: kurbo::Point| {
        p.x.to_bits().hash(h);
        p.y.to_bits().hash(h);
    };
    for el in p.elements() {
        match *el {
            PathEl::MoveTo(a) => pt(a),
            PathEl::LineTo(a) => pt(a),
            PathEl::QuadTo(a, b) => {
                pt(a);
                pt(b);
            }
            PathEl::CurveTo(a, b, c) => {
                pt(a);
                pt(b);
                pt(c);
            }
            PathEl::ClosePath => pt(kurbo::Point::new(f64::NAN, 0.0)),
        }
    }
}

fn hash_item(h: &mut impl Hasher, item: &Item) {
    match item {
        Item::PushClip(p) => {
            0u8.hash(h);
            hash_path(h, p);
        }
        Item::PopClip => 1u8.hash(h),
        Item::PushOpacity(o) => {
            2u8.hash(h);
            hash_f32(h, *o);
        }
        Item::PopOpacity => 3u8.hash(h),
        Item::Shadow {
            rect,
            radius,
            std_dev,
            color,
            clip,
        } => {
            4u8.hash(h);
            hash_rect(h, *rect);
            hash_f32(h, *radius);
            hash_f32(h, *std_dev);
            hash_color(h, color);
            hash_path(h, clip);
        }
        Item::Fill {
            shape,
            paint,
            frame,
        } => {
            5u8.hash(h);
            match shape {
                FillShape::Rect(r) => hash_rect(h, *r),
                FillShape::Path(p) => hash_path(h, p),
            }
            hash_paint(h, paint);
            hash_rect(h, *frame);
        }
        Item::Border { path, paint, frame } => {
            6u8.hash(h);
            hash_path(h, path);
            hash_paint(h, paint);
            hash_rect(h, *frame);
        }
        Item::Glyphs {
            x,
            y,
            layout,
            color,
        } => {
            7u8.hash(h);
            (x, y, layout.key, layout.scale).hash(h);
            hash_color(h, color);
        }
    }
}

impl Flattener<'_> {
    fn push(&mut self, item: Item, bounds: Rect, sig: &mut DefaultHasher, ink: &mut Rect) {
        hash_item(sig, &item);
        *ink = ink.union(bounds);
        self.out.items.push(DisplayItem { item, bounds });
    }

    fn marker(&mut self, item: Item) {
        let bounds = self.surface;
        self.out.items.push(DisplayItem { item, bounds });
    }

    fn node(&mut self, node: &Node, parent: LogicalRect, inh: &Inherited, root: bool) {
        let s = self.scale.as_f64();
        let get = |p: Prop| node.get(p);

        // Inherited props.
        let color = match get(Prop::Color) {
            Some(PropValue::Color(c)) => *c,
            _ => inh.color,
        };
        let mut font = match get(Prop::Font) {
            Some(PropValue::Font(f)) => f.clone(),
            _ => inh.font.clone(),
        };
        if let Some(w) = number(get(Prop::Weight)) {
            font.weight = w.clamp(1.0, 1000.0) as u16;
        }

        // Text shaping need, and the layout to draw meanwhile.
        let is_text = matches!(node.kind, NodeKind::Text | NodeKind::Button);
        let explicit_w = length(get(Prop::Width), parent.w).or(number(get(Prop::Size)));
        let explicit_h = length(get(Prop::Height), parent.h).or(number(get(Prop::Size)));
        let mut layout = None;
        if is_text && let Some(PropValue::Text(text)) = get(Prop::Text) {
            let align = match get(Prop::Align) {
                Some(PropValue::Keyword(k)) if k == "center" => TextAlign::Center,
                Some(PropValue::Keyword(k)) if k == "end" => TextAlign::End,
                _ => TextAlign::Start,
            };
            let max_width = explicit_w.or(length(get(Prop::MaxWidth), parent.w));
            self.out.text.push((
                node.id,
                TextSpec {
                    text: text.clone(),
                    style: TextStyle {
                        font: font.clone(),
                        line_height: None,
                        align,
                    },
                    max_width,
                    scale: self.scale,
                },
            ));
            layout = self.layouts.get(&node.id).cloned();
        }

        // Geometry.
        let rect = if root {
            parent
        } else {
            let (lw, lh) = layout.as_ref().map_or((0.0, 0.0), |l| (l.size.w, l.size.h));
            let x = parent.x + length(get(Prop::X), parent.w).unwrap_or(0.0);
            let y = parent.y + length(get(Prop::Y), parent.h).unwrap_or(0.0);
            LogicalRect::new(
                x,
                y,
                explicit_w.unwrap_or(lw).max(0.0),
                explicit_h.unwrap_or(lh).max(0.0),
            )
        };
        let phys = self.scale.snap_rect(rect);
        let frame = kurbo_rect(phys);
        let opacity = number(get(Prop::Opacity)).unwrap_or(1.0).clamp(0.0, 1.0);
        if opacity <= 0.0 {
            return;
        }

        let mut sig = DefaultHasher::new();
        (inh.ctx, node.kind, node.epoch).hash(&mut sig);
        hash_f32(&mut sig, opacity);
        let mut ink = Rect::default();

        if opacity < 1.0 {
            self.marker(Item::PushOpacity(opacity));
        }
        let r = radii(
            corners_of(get(Prop::Radius)),
            frame.width(),
            frame.height(),
            s,
        );
        let box_path = shape_path(frame, r);
        let has_area = !phys.is_empty();

        // Shadows, under the box.
        if has_area && let Some(PropValue::Shadow(list)) = get(Prop::Shadow) {
            for sh in list {
                self.shadow(sh, frame, &r, &box_path, &mut sig, &mut ink);
            }
        }
        // Background.
        if has_area && let Some(paint) = paint_of(get(Prop::Bg)) {
            let shape = if radii_zero(&r) {
                FillShape::Rect(frame)
            } else {
                FillShape::Path(box_path.clone())
            };
            self.push(
                Item::Fill {
                    shape,
                    paint,
                    frame,
                },
                phys,
                &mut sig,
                &mut ink,
            );
        }
        // Border, drawn inside the box.
        if has_area
            && let Some(PropValue::Border(Border { width, paint })) = get(Prop::Border)
            && *width > 0.0
        {
            let bw = (*width as f64 * s).round().max(1.0);
            let inner = frame.inflate(-bw, -bw);
            let mut path = box_path.clone();
            if inner.width() > 0.0 && inner.height() > 0.0 {
                let ir = RoundedRectRadii::new(
                    (r.top_left - bw).max(0.0),
                    (r.top_right - bw).max(0.0),
                    (r.bottom_right - bw).max(0.0),
                    (r.bottom_left - bw).max(0.0),
                );
                path.extend(shape_path(inner, ir));
            }
            self.push(
                Item::Border {
                    path,
                    paint: paint.clone(),
                    frame,
                },
                phys,
                &mut sig,
                &mut ink,
            );
        }
        // Text.
        if let Some(l) = layout {
            // A layout from another scale is drawn resampled (see raster).
            let k = self.scale.as_f64() / l.scale.as_f64();
            let bounds = if k == 1.0 {
                l.ink.translate(phys.x, phys.y)
            } else {
                cover(kurbo::Rect::new(
                    phys.x as f64 + l.ink.left() as f64 * k,
                    phys.y as f64 + l.ink.top() as f64 * k,
                    phys.x as f64 + l.ink.right() as f64 * k,
                    phys.y as f64 + l.ink.bottom() as f64 * k,
                ))
                .inflate(1)
            };
            if !bounds.is_empty() {
                self.push(
                    Item::Glyphs {
                        x: phys.x,
                        y: phys.y,
                        layout: l,
                        color,
                    },
                    bounds,
                    &mut sig,
                    &mut ink,
                );
            }
        }

        let bounds = ink.intersect(inh.clip).unwrap_or_default();
        self.out.records.insert(
            node.id,
            NodeRecord {
                bounds,
                sig: sig.finish(),
            },
        );

        // Children.
        let clips = matches!(get(Prop::Clip), Some(PropValue::Bool(true)));
        let mut ctx = DefaultHasher::new();
        (inh.ctx, node.epoch).hash(&mut ctx);
        hash_f32(&mut ctx, opacity);
        let mut child_clip = inh.clip;
        if clips {
            hash_path(&mut ctx, &box_path);
            child_clip = phys.intersect(inh.clip).unwrap_or_default();
            self.marker(Item::PushClip(box_path));
        }
        let child_inh = Inherited {
            color,
            font,
            ctx: ctx.finish(),
            clip: child_clip,
        };
        if !(clips && child_clip.is_empty()) {
            for c in &node.children {
                if let Some(child) = self.tree.get(*c) {
                    self.node(child, rect, &child_inh, false);
                }
            }
        }
        if clips {
            self.marker(Item::PopClip);
        }
        if opacity < 1.0 {
            self.marker(Item::PopOpacity);
        }
    }

    fn shadow(
        &mut self,
        sh: &Shadow,
        frame: kurbo::Rect,
        r: &RoundedRectRadii,
        box_path: &BezPath,
        sig: &mut DefaultHasher,
        ink: &mut Rect,
    ) {
        let s = self.scale.as_f64();
        if sh.color.a <= 0.0 {
            return;
        }
        let spread = sh.spread as f64 * s;
        let rect = frame
            .with_origin((frame.x0 + sh.x as f64 * s, frame.y0 + sh.y as f64 * s))
            .inflate(spread, spread);
        if rect.width() <= 0.0 || rect.height() <= 0.0 {
            return;
        }
        let radius = (r
            .top_left
            .max(r.top_right)
            .max(r.bottom_right)
            .max(r.bottom_left)
            + spread)
            .clamp(0.0, rect.width().min(rect.height()) / 2.0);
        // CSS blur radius is twice the Gaussian standard deviation.
        let std_dev = (sh.blur as f64 * s / 2.0).max(0.0);
        let reach = (3.0 * std_dev).ceil() + 1.0;
        let extent = rect.inflate(reach, reach);
        let mut clip = extent.to_path(TOLERANCE);
        clip.extend(box_path.iter());
        self.push(
            Item::Shadow {
                rect,
                radius: radius as f32,
                std_dev: std_dev as f32,
                color: sh.color,
                clip,
            },
            cover(extent),
            sig,
            ink,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strand_scene::{SceneDiff, SceneOp, Transition};

    fn id(i: u32) -> NodeId {
        NodeId::new(i, 0)
    }

    fn tree() -> SceneTree {
        let mut t = SceneTree::new();
        let mut d = SceneDiff::new();
        d.create(id(0), NodeKind::Bar, None, 0)
            .set(id(0), Prop::Bg, PropValue::Color(Color::WHITE))
            .create(id(1), NodeKind::Box, Some(id(0)), 0)
            .set(id(1), Prop::X, PropValue::Number(10.0))
            .set(id(1), Prop::Y, PropValue::Number(4.0))
            .set(id(1), Prop::Size, PropValue::Number(8.0))
            .set(id(1), Prop::Bg, PropValue::Color(Color::BLACK));
        assert!(t.apply(d).is_empty());
        t
    }

    #[test]
    fn absolute_layout_and_records() {
        let t = tree();
        let f = flatten(&t, id(0), Size::new(100, 20), Scale::ONE, &HashMap::new());
        assert_eq!(f.records[&id(0)].bounds, Rect::new(0, 0, 100, 20));
        assert_eq!(f.records[&id(1)].bounds, Rect::new(10, 4, 8, 8));
        assert_eq!(f.items.len(), 2);
    }

    #[test]
    fn fractional_scale_snaps_edges() {
        let t = tree();
        let s = Scale::new(150).unwrap();
        let f = flatten(&t, id(0), Size::new(125, 25), s, &HashMap::new());
        // 10 × 1.25 = 12.5 → 13, 18 × 1.25 = 22.5 → 23.
        assert_eq!(f.records[&id(1)].bounds, Rect::new(13, 5, 10, 10));
    }

    #[test]
    fn signatures_track_paint_changes_only() {
        let mut t = tree();
        let a = flatten(&t, id(0), Size::new(100, 20), Scale::ONE, &HashMap::new());
        // Same value again: same signature.
        t.apply_op(SceneOp::SetProp {
            id: id(1),
            prop: Prop::Bg,
            value: PropValue::Color(Color::BLACK),
            transition: Transition::Instant,
        })
        .unwrap();
        let b = flatten(&t, id(0), Size::new(100, 20), Scale::ONE, &HashMap::new());
        assert_eq!(a.records, b.records);
        t.apply_op(SceneOp::SetProp {
            id: id(0),
            prop: Prop::Opacity,
            value: PropValue::Number(0.5),
            transition: Transition::Instant,
        })
        .unwrap();
        let c = flatten(&t, id(0), Size::new(100, 20), Scale::ONE, &HashMap::new());
        // Parent opacity changes how the child paints.
        assert_ne!(a.records[&id(1)].sig, c.records[&id(1)].sig);
    }

    #[test]
    fn radii_shrink_like_css() {
        let r = radii(Corners::all(999.0), 40.0, 10.0, 1.0);
        assert_eq!(r.top_left, 5.0);
        let r = radii(
            Corners {
                top_left: 14.0,
                top_right: 14.0,
                bottom_right: 0.0,
                bottom_left: 0.0,
            },
            100.0,
            100.0,
            1.5,
        );
        assert_eq!((r.top_left, r.bottom_left), (21.0, 0.0));
    }
}
