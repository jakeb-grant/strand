//! How a [`SurfaceSpec`] maps onto `zwlr_layer_surface_v1` state: anchor,
//! size, exclusive zone, margins, layer, keyboard focus and namespace.
//! Pure, so it is tested without a compositor.

use strand_scene::{Anchor, Edge, Keyboard, Layer, NodeKind, SurfaceSpec};

/// `zwlr_layer_surface_v1.anchor` bits.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Anchors {
    pub top: bool,
    pub bottom: bool,
    pub left: bool,
    pub right: bool,
}

impl Anchors {
    const fn new(top: bool, bottom: bool, left: bool, right: bool) -> Self {
        Self {
            top,
            bottom,
            left,
            right,
        }
    }
}

/// Everything sent to a layer surface before its first commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerConfig {
    pub namespace: String,
    pub layer: Layer,
    pub anchors: Anchors,
    /// Requested size in logical pixels; 0 lets the compositor stretch
    /// that dimension between two opposite anchors.
    pub width: u32,
    pub height: u32,
    pub exclusive_zone: i32,
    /// Top, right, bottom, left, in logical pixels.
    pub margin: [i32; 4],
    pub keyboard: Keyboard,
    /// How far the buffer reaches past the surface's box (shadows), top,
    /// right, bottom, left, in logical pixels: the size above includes
    /// it and the margins are shifted by it, so the box stays where its
    /// margin puts it; the exclusive zone reserves only the box.
    pub overhang: [i32; 4],
    /// The input region: the box inside the overhang, or nothing at all
    /// for a click-through `osd`.
    pub click_through: bool,
}

impl LayerConfig {
    /// True if the surface reserves an exclusive zone and its buffer
    /// lies wholly inside it: no overhang on the side facing the usable
    /// area (a bar with no shadow towards the windows). The zone counts
    /// from the edge the surface is anchored to without its opposite.
    pub fn inside_exclusive_zone(&self) -> bool {
        if self.exclusive_zone <= 0 {
            return false;
        }
        let [ot, or, ob, ol] = self.overhang;
        let a = self.anchors;
        let away = match (a.top, a.bottom, a.left, a.right) {
            (true, false, _, _) => ob,
            (false, true, _, _) => ot,
            (_, _, true, false) => or,
            (_, _, false, true) => ol,
            // Anchored to all four edges or none: the zone applies to
            // no edge, so nothing lies outside the usable area.
            _ => return false,
        };
        away == 0
    }

    /// Where a surface of this config, `(w, h)` logical pixels (its
    /// buffer: box plus overhang), lands in an area of `(aw, ah)` (the
    /// output's usable area, where the compositor arranges layer
    /// surfaces): its top-left corner, as wlroots compositors arrange it
    /// (centred between two opposite anchors or none, margins only on an
    /// anchored side).
    pub fn position_in(&self, (w, h): (u32, u32), (aw, ah): (u32, u32)) -> (i32, i32) {
        let axis = |size: u32, area: u32, lo: bool, hi: bool, mlo: i32, mhi: i32| -> i32 {
            let (size, area) = (i64::from(size), i64::from(area));
            let v = match (lo, hi) {
                (true, false) => i64::from(mlo),
                (false, true) => area - size - i64::from(mhi),
                _ => area / 2 - size / 2,
            };
            v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
        };
        let [mt, mr, mb, ml] = self.margin;
        let a = self.anchors;
        (
            axis(w, aw, a.left, a.right, ml, mr),
            axis(h, ah, a.top, a.bottom, mt, mb),
        )
    }

    /// The box (input region) of a surface of this config, `(w, h)`
    /// logical pixels, arranged in an area of `(aw, ah)`, in that area's
    /// coordinates: the hole a click-away catcher configured to the same
    /// area leaves for it.
    pub fn box_in(&self, (w, h): (u32, u32), area: (u32, u32)) -> (i32, i32, i32, i32) {
        let (x, y) = self.position_in((w, h), area);
        let (bx, by, bw, bh) = match self.input_region((w, h)) {
            Some(Some(r)) => r,
            _ => (0, 0, clamp(w), clamp(h)),
        };
        (x.saturating_add(bx), y.saturating_add(by), bw, bh)
    }

    /// Fits the requested size to an output of logical size `(w, h)`: the
    /// box is never larger than the output less its margins (a runaway
    /// content-sized panel gets no buffer taller than its screen; render
    /// lays it out at the configured size, so the rest scrolls or clips).
    /// A dimension the compositor stretches (0) is left alone.
    pub fn fit(&mut self, (w, h): (i32, i32)) {
        let [mt, mr, mb, ml] = self.margin;
        let limit = |v: u32, out: i32, a: i32, b: i32| -> u32 {
            if v == 0 || out <= 0 {
                return v;
            }
            let room = (out as i64 - a as i64 - b as i64).clamp(1, u32::MAX as i64) as u32;
            v.min(room)
        };
        self.width = limit(self.width, w, ml, mr);
        self.height = limit(self.height, h, mt, mb);
    }

    /// The input region for a surface of logical size `(w, h)`: `None` is
    /// the whole surface; `Some(None)` is empty (click-through); otherwise
    /// the box `(x, y, w, h)` inside the overhang.
    pub fn input_region(&self, (w, h): (u32, u32)) -> Option<Option<(i32, i32, i32, i32)>> {
        if self.click_through {
            return Some(None);
        }
        let [t, r, b, l] = self.overhang;
        if [t, r, b, l] == [0; 4] {
            return None;
        }
        Some(Some((
            l,
            t,
            (w as i32 - l - r).max(0),
            (h as i32 - t - b).max(0),
        )))
    }
}

/// Why a surface spec has no layer surface (yet).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlacementError {
    /// `popup` and `lock` are not layer surfaces (xdg_popup lands in M4,
    /// `ext-session-lock` with the lock screen).
    NotLayerSurface(NodeKind),
    /// A bar without a thickness, or a panel or OSD without a width and
    /// height (render fills them in from layout; a spec from elsewhere
    /// may lack them).
    AutoSize(NodeKind),
}

impl std::fmt::Display for PlacementError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotLayerSurface(kind) => {
                write!(f, "a `{}` is not a layer surface", kind.name())
            }
            Self::AutoSize(kind) => write!(
                f,
                "a `{}` has no size yet (its content is not laid out)",
                kind.name()
            ),
        }
    }
}

impl std::error::Error for PlacementError {}

/// The largest magnitude, logical pixels, a size, margin or shadow
/// reach from a `.strand` file is taken at: far past any output, and
/// small enough that sums of a few of them never overflow `i32`.
pub const MAX_LOGICAL: i32 = 1 << 20;

/// Logical pixels as the integer the protocol takes: rounded, clamped to
/// ±[`MAX_LOGICAL`].
fn px(v: f32) -> i32 {
    if v.is_finite() {
        v.round().clamp(-(MAX_LOGICAL as f32), MAX_LOGICAL as f32) as i32
    } else {
        0
    }
}

fn clamp(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

fn size(v: f32) -> u32 {
    px(v).max(1) as u32
}

/// (M4) The margins that move a layer surface of `config` by `offset`
/// logical pixels (a compositor-animated pose): a margin moves a surface
/// only from the edge it is anchored to, so an axis anchored on one side
/// takes the offset there (subtracted on a right or bottom anchor); an
/// axis centred or stretched keeps its margins (render delegates no
/// offset on one). Rounded to whole pixels, as margins are.
pub fn posed_margin(config: &LayerConfig, offset: strand_scene::LogicalPoint) -> [i32; 4] {
    let [mut t, mut r, mut b, mut l] = config.margin;
    let a = config.anchors;
    let px = |v: f32| -> i32 {
        if v.is_finite() {
            v.round().clamp(-1e6, 1e6) as i32
        } else {
            0
        }
    };
    let (dx, dy) = (px(offset.x), px(offset.y));
    match (a.left, a.right) {
        (true, false) => l = l.saturating_add(dx),
        (false, true) => r = r.saturating_sub(dx),
        _ => {}
    }
    match (a.top, a.bottom) {
        (true, false) => t = t.saturating_add(dy),
        (false, true) => b = b.saturating_sub(dy),
        _ => {}
    }
    [t, r, b, l]
}

/// The layer under `layer` (the background has none: itself).
pub fn layer_below(layer: Layer) -> Layer {
    match layer {
        Layer::Overlay => Layer::Top,
        Layer::Top => Layer::Bottom,
        Layer::Bottom | Layer::Background => Layer::Background,
    }
}

/// The layer a popup's scrim goes on, for the layer surface the popup is
/// nested in (`root`). Popups stack above every layer surface, but the
/// scrim is a layer surface of its own, and two on one layer stack in an
/// order the protocol leaves open (sway 1.9 draws the older on top,
/// wlroots' scene graph, niri and labwc the newer). A bar whose buffer
/// lies wholly inside its exclusive zone is outside the usable area the
/// scrim covers, so the scrim shares its layer; any other surface (a
/// panel, or a bar whose shadow reaches past its zone) has it on the
/// layer below (a `top` one with such a popup rose to `overlay`, see
/// [`layer_config_with`]).
pub fn popup_scrim_layer(root: &LayerConfig) -> Layer {
    if root.inside_exclusive_zone() {
        root.layer
    } else {
        layer_below(root.layer)
    }
}

/// Resolves the layer-surface state for `spec`.
pub fn layer_config(spec: &SurfaceSpec) -> Result<LayerConfig, PlacementError> {
    layer_config_with(spec, false)
}

/// Resolves the layer-surface state for `spec`; `nested_scrim`: a popup
/// nested in it has a scrim (see [`popup_scrim_layer`]).
pub fn layer_config_with(
    spec: &SurfaceSpec,
    nested_scrim: bool,
) -> Result<LayerConfig, PlacementError> {
    let layer = spec
        .layer
        .ok_or(PlacementError::NotLayerSurface(spec.kind))?;
    let m = spec.margin;
    let o = spec.overhang;
    let overhang = [o.top, o.right, o.bottom, o.left].map(|v| px(v).max(0));
    let [ot, or, ob, ol] = overhang;
    // `attach: <edge>` (a panel): flush against that edge, gap 0.
    let attach = spec.attach.filter(|_| spec.kind == NodeKind::Panel);
    let flush = |e: Edge, v: f32| if attach == Some(e) { 0 } else { px(v) };
    // The box keeps its place: each margin moves out by the overhang.
    let margin = [
        flush(Edge::Top, m.top) - ot,
        flush(Edge::Right, m.right) - or,
        flush(Edge::Bottom, m.bottom) - ob,
        flush(Edge::Left, m.left) - ol,
    ];
    let (anchors, width, height, exclusive_zone) = if spec.kind == NodeKind::Bar {
        let edge = spec.edge.unwrap_or(Edge::Top);
        let thickness = spec
            .exclusive_zone()
            .ok_or(PlacementError::AutoSize(spec.kind))?;
        let t = size(thickness);
        let (tv, th) = (t + (ot + ob) as u32, t + (ol + or) as u32);
        // The zone counts from the (moved) margin: it reserves the box
        // plus the overhang towards the edge, so the reserved space stays
        // margin + thickness.
        match edge {
            Edge::Top => (
                Anchors::new(true, false, true, true),
                0,
                tv,
                px(thickness) + ot,
            ),
            Edge::Bottom => (
                Anchors::new(false, true, true, true),
                0,
                tv,
                px(thickness) + ob,
            ),
            Edge::Left => (
                Anchors::new(true, true, true, false),
                th,
                0,
                px(thickness) + ol,
            ),
            Edge::Right => (
                Anchors::new(true, true, false, true),
                th,
                0,
                px(thickness) + or,
            ),
        }
    } else {
        let (Some(w), Some(h)) = (spec.width, spec.height) else {
            return Err(PlacementError::AutoSize(spec.kind));
        };
        let mut anchors = match spec.anchor {
            Anchor::Center => Anchors::default(),
            Anchor::Top => Anchors::new(true, false, false, false),
            Anchor::Bottom => Anchors::new(false, true, false, false),
            Anchor::Left => Anchors::new(false, false, true, false),
            Anchor::Right => Anchors::new(false, false, false, true),
            Anchor::TopLeft => Anchors::new(true, false, true, false),
            Anchor::TopRight => Anchors::new(true, false, false, true),
            Anchor::BottomLeft => Anchors::new(false, true, true, false),
            Anchor::BottomRight => Anchors::new(false, true, false, true),
        };
        // Attached: anchored to that edge (not the opposite one); the
        // anchor's other axis stays.
        match attach {
            Some(Edge::Top) => (anchors.top, anchors.bottom) = (true, false),
            Some(Edge::Bottom) => (anchors.top, anchors.bottom) = (false, true),
            Some(Edge::Left) => (anchors.left, anchors.right) = (true, false),
            Some(Edge::Right) => (anchors.left, anchors.right) = (false, true),
            None => {}
        }
        (
            anchors,
            size(w) + (ol + or) as u32,
            size(h) + (ot + ob) as u32,
            0,
        )
    };
    let mut config = LayerConfig {
        namespace: spec.namespace(),
        layer,
        anchors,
        width,
        height,
        exclusive_zone,
        margin,
        keyboard: spec.keyboard,
        overhang,
        // An OSD is click-through (design example d).
        click_through: spec.kind == NodeKind::Osd,
    };
    // A panel's scrim, or a popup's nested in a panel or in a bar that
    // reaches past its exclusive zone, goes on the layer below it, which
    // for a `top` surface would be under the windows it should dim: such
    // a surface rises to `overlay`, the scrim on `top`
    // (`manager/catcher.rs`, [`popup_scrim_layer`]).
    let below = match spec.kind {
        NodeKind::Panel => spec.scrim.is_some() || nested_scrim,
        NodeKind::Bar => nested_scrim && !config.inside_exclusive_zone(),
        _ => false,
    };
    if below && layer == Layer::Top {
        config.layer = Layer::Overlay;
    }
    Ok(config)
}

/// Which side of its anchor a popup opens on.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PopupSide {
    Below,
    Above,
    Right,
    Left,
}

/// Everything an `xdg_positioner` and the popup's `xdg_surface` need.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PopupConfig {
    pub namespace: String,
    /// The box (the window geometry), logical pixels.
    pub width: u32,
    pub height: u32,
    /// Shadow reach past the box, top, right, bottom, left: the buffer is
    /// the box plus it, and the window geometry leaves it out, so the
    /// compositor positions the box.
    pub overhang: [i32; 4],
    /// The anchor rectangle in the parent's window geometry, logical
    /// pixels: `x, y, w, h` (at least 1 × 1).
    pub anchor_rect: (i32, i32, i32, i32),
    pub side: PopupSide,
    /// (M4) Opens along its anchor's start instead of centred on it: a
    /// submenu beside its row, its box's top level with the row's top
    /// (flipped as the side is).
    pub aligned: bool,
    /// The gap between the anchor and the popup's box.
    pub gap: i32,
    /// Takes an `xdg_popup.grab` (a menu, the calendar); a tooltip does
    /// not, and takes no input.
    pub grab: bool,
}

impl PopupConfig {
    /// The buffer's logical size: the box plus the overhang.
    pub fn buffer(&self) -> (u32, u32) {
        let [t, r, b, l] = self.overhang;
        (
            self.width.saturating_add((l + r).max(0) as u32),
            self.height.saturating_add((t + b).max(0) as u32),
        )
    }

    /// The input region, as a layer surface's: the box, or nothing for a
    /// tooltip.
    pub fn as_layer(&self) -> LayerConfig {
        let (w, h) = self.buffer();
        LayerConfig {
            namespace: self.namespace.clone(),
            layer: Layer::Overlay,
            anchors: Anchors::default(),
            width: w,
            height: h,
            exclusive_zone: 0,
            margin: [0; 4],
            keyboard: if self.grab {
                Keyboard::Exclusive
            } else {
                Keyboard::None
            },
            overhang: self.overhang,
            click_through: !self.grab,
        }
    }
}

/// Resolves the popup state of `spec` nested in a surface of `parent`
/// (design.md: popups are anchored xdg_popups that nest). The popup
/// opens away from a bar's edge, across the bar (below a top bar, so a
/// calendar under the clock clears the bar), and below its anchor
/// otherwise; `margin` on that side is the gap (default 6). Waits
/// (`AutoSize`) until render gave it a size and an anchor.
pub fn popup_config(
    spec: &SurfaceSpec,
    parent: &SurfaceSpec,
) -> Result<PopupConfig, PlacementError> {
    let (Some(w), Some(h), Some(a)) = (spec.width, spec.height, spec.anchor_rect) else {
        return Err(PlacementError::AutoSize(spec.kind));
    };
    let o = spec.overhang;
    let overhang = [o.top, o.right, o.bottom, o.left].map(|v| px(v).max(0));
    let po = parent.overhang;
    let (pt, pl) = (px(po.top).max(0), px(po.left).max(0));
    let parent_popup = parent.kind == NodeKind::Popup;
    // The parent's window geometry: a popup's is its box (inside its
    // overhang); a layer surface's is its whole buffer.
    let (gx, gy) = if parent_popup { (pl, pt) } else { (0, 0) };
    let (mut x, mut y, mut aw, mut ah) = (px(a.x) - gx, px(a.y) - gy, px(a.w), px(a.h));
    let bar_edge = (parent.kind == NodeKind::Bar).then(|| parent.edge.unwrap_or(Edge::Top));
    // `attach: <edge>` names the popup's side that touches its anchor,
    // so it opens away from it, at gap 0.
    let side = match spec.attach.or(bar_edge) {
        Some(Edge::Top) => PopupSide::Below,
        Some(Edge::Bottom) => PopupSide::Above,
        Some(Edge::Left) => PopupSide::Right,
        Some(Edge::Right) => PopupSide::Left,
        // A popup in a popup is a submenu: beside its anchor, on the side
        // its `anchor:` names (right unless it says left, top or
        // bottom; the compositor flips it when it does not fit).
        None if parent_popup => match spec.anchor {
            Anchor::Left | Anchor::TopLeft | Anchor::BottomLeft => PopupSide::Left,
            Anchor::Top => PopupSide::Above,
            Anchor::Bottom => PopupSide::Below,
            Anchor::Center | Anchor::Right | Anchor::TopRight | Anchor::BottomRight => {
                PopupSide::Right
            }
        },
        None => PopupSide::Below,
    };
    // A submenu beside its row starts level with it.
    let aligned =
        parent_popup && spec.attach.is_none() && matches!(side, PopupSide::Right | PopupSide::Left);
    if let Some(edge) = bar_edge {
        // Across the whole bar: from its box's edge to its other edge.
        let thick = parent.exclusive_zone().map_or(0, |t| px(t).max(0));
        match edge {
            Edge::Top | Edge::Bottom => {
                y = pt;
                ah = thick;
            }
            Edge::Left | Edge::Right => {
                x = pl;
                aw = thick;
            }
        }
    }
    let m = spec.margin;
    let gap = match side {
        PopupSide::Below => m.top,
        PopupSide::Above => m.bottom,
        PopupSide::Right => m.left,
        PopupSide::Left => m.right,
    };
    let gap = if spec.attach.is_some() {
        0
    } else if gap == 0.0 {
        6
    } else {
        px(gap)
    };
    if spec.tooltip {
        // A tooltip sits just under what it describes, bars included.
        if bar_edge.is_some() {
            (x, y, aw, ah) = (px(a.x) - gx, px(a.y) - gy, px(a.w), px(a.h));
        }
    }
    Ok(PopupConfig {
        namespace: spec.namespace(),
        width: size(w),
        height: size(h),
        overhang,
        anchor_rect: (x, y, aw.max(1), ah.max(1)),
        side: if spec.tooltip { PopupSide::Below } else { side },
        aligned: aligned && !spec.tooltip,
        gap: if spec.tooltip { 4 } else { gap },
        grab: !spec.tooltip,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use strand_scene::{Insets, Prop, PropValue};

    fn spec(kind: NodeKind, props: &[(Prop, PropValue)]) -> SurfaceSpec {
        let map: HashMap<Prop, PropValue> = props.iter().cloned().collect();
        SurfaceSpec::resolve(kind, |p| map.get(&p))
    }

    fn kw(k: &str) -> PropValue {
        PropValue::Keyword(k.into())
    }

    #[test]
    fn poses_move_a_surface_from_its_anchored_edges() {
        use strand_scene::LogicalPoint;
        // panel { anchor: top_right; margin: 8 } and one centred.
        let corner = spec(
            NodeKind::Panel,
            &[
                (Prop::Anchor, kw("top_right")),
                (Prop::Width, PropValue::Number(100.0)),
                (Prop::Height, PropValue::Number(50.0)),
                (
                    Prop::Margin,
                    PropValue::Insets(Insets::from_values(&[8.0]).unwrap()),
                ),
            ],
        );
        let c = layer_config(&corner).unwrap();
        assert_eq!(c.margin, [8, 8, 8, 8]);
        // 40 px right and 10 down: the right margin shrinks, the top grows.
        assert_eq!(
            posed_margin(&c, LogicalPoint::new(40.4, 10.0)),
            [18, -32, 8, 8]
        );
        assert_eq!(posed_margin(&c, LogicalPoint::new(0.0, 0.0)), c.margin);
        let mut centred = corner.clone();
        centred.anchor = Anchor::Center;
        let c = layer_config(&centred).unwrap();
        assert_eq!(posed_margin(&c, LogicalPoint::new(40.0, 10.0)), c.margin);
        let mut bottom_left = corner;
        bottom_left.anchor = Anchor::BottomLeft;
        let c = layer_config(&bottom_left).unwrap();
        assert_eq!(
            posed_margin(&c, LogicalPoint::new(-5.0, 20.0)),
            [8, 8, -12, 3]
        );
    }

    #[test]
    fn top_bar_stretches_along_its_edge() {
        // bar Top { edge: top; height: 36; margin: 8, 8, 0 }
        let s = spec(
            NodeKind::Bar,
            &[
                (Prop::Name, PropValue::Text("Top".into())),
                (Prop::Edge, kw("top")),
                (Prop::Height, PropValue::Number(36.0)),
                (
                    Prop::Margin,
                    PropValue::Insets(Insets::from_values(&[8.0, 8.0, 0.0]).unwrap()),
                ),
            ],
        );
        let c = layer_config(&s).unwrap();
        assert_eq!(c.namespace, "strand-Top");
        assert_eq!(c.layer, Layer::Top);
        assert_eq!(c.anchors, Anchors::new(true, false, true, true));
        assert_eq!((c.width, c.height), (0, 36));
        assert_eq!(c.exclusive_zone, 36);
        assert_eq!(c.margin, [8, 8, 0, 8]);
        assert_eq!(c.keyboard, Keyboard::None);
    }

    #[test]
    fn side_bars_use_width() {
        let s = spec(
            NodeKind::Bar,
            &[
                (Prop::Edge, kw("left")),
                (Prop::Width, PropValue::Number(48.0)),
            ],
        );
        let c = layer_config(&s).unwrap();
        assert_eq!(c.anchors, Anchors::new(true, true, true, false));
        assert_eq!((c.width, c.height, c.exclusive_zone), (48, 0, 48));
        assert_eq!(c.namespace, "strand-bar");
        let s = spec(
            NodeKind::Bar,
            &[
                (Prop::Edge, kw("bottom")),
                (Prop::Height, PropValue::Number(30.4)),
            ],
        );
        let c = layer_config(&s).unwrap();
        assert_eq!(c.anchors, Anchors::new(false, true, true, true));
        assert_eq!((c.width, c.height, c.exclusive_zone), (0, 30, 30));
    }

    #[test]
    fn panels_anchor_and_reserve_nothing() {
        let s = spec(
            NodeKind::Panel,
            &[
                (Prop::Name, PropValue::Text("Launcher".into())),
                (Prop::Layer, kw("overlay")),
                (Prop::Anchor, kw("top_right")),
                (Prop::Keyboard, kw("exclusive")),
                (Prop::Width, PropValue::Number(400.0)),
                (Prop::Height, PropValue::Number(300.0)),
            ],
        );
        let c = layer_config(&s).unwrap();
        assert_eq!(c.anchors, Anchors::new(true, false, false, true));
        assert_eq!((c.width, c.height, c.exclusive_zone), (400, 300, 0));
        assert_eq!(c.layer, Layer::Overlay);
        assert_eq!(c.keyboard, Keyboard::Exclusive);
    }

    /// Shadows grow the buffer and move the margins out, so the box stays
    /// put; the bar's reserved space (margin + zone) is unchanged and the
    /// input region is the box alone; an OSD's is empty.
    #[test]
    fn overhang_grows_the_buffer_not_the_box() {
        let mut s = spec(
            NodeKind::Bar,
            &[
                (Prop::Height, PropValue::Number(36.0)),
                (
                    Prop::Margin,
                    PropValue::Insets(Insets::from_values(&[8.0, 8.0, 0.0]).unwrap()),
                ),
            ],
        );
        s.overhang = Insets {
            top: 11.0,
            right: 13.0,
            bottom: 15.0,
            left: 13.0,
        };
        let c = layer_config(&s).unwrap();
        assert_eq!((c.width, c.height), (0, 36 + 11 + 15));
        assert_eq!(c.margin, [8 - 11, 8 - 13, -15, 8 - 13]);
        assert_eq!(c.margin[0] + c.exclusive_zone, 8 + 36, "reserved space");
        assert_eq!(
            c.input_region((2560 - 16 + 26, 62)),
            Some(Some((13, 11, 2544, 36)))
        );
        let mut p = spec(
            NodeKind::Panel,
            &[
                (Prop::Anchor, kw("top_right")),
                (Prop::Width, PropValue::Number(380.0)),
                (Prop::Height, PropValue::Number(200.0)),
            ],
        );
        p.overhang = Insets::all(10.0);
        let c = layer_config(&p).unwrap();
        assert_eq!((c.width, c.height), (400, 220));
        assert_eq!(c.input_region((400, 220)), Some(Some((10, 10, 380, 200))));
        p.overhang = Insets::default();
        assert_eq!(layer_config(&p).unwrap().input_region((380, 200)), None);
        let o = spec(NodeKind::Osd, &[(Prop::Size, PropValue::Number(100.0))]);
        let c = layer_config(&o).unwrap();
        assert_eq!(c.layer, Layer::Overlay);
        assert_eq!(c.input_region((100, 100)), Some(None), "click-through");
    }

    /// A content-sized panel taller than its output gets a buffer no
    /// taller than the output less its margins; its overhang stays
    /// outside the box, and a stretched bar dimension is untouched.
    #[test]
    fn an_oversized_surface_fits_its_output() {
        let mut p = spec(
            NodeKind::Panel,
            &[
                (Prop::Anchor, kw("top_right")),
                (Prop::Width, PropValue::Number(600.0)),
                (Prop::Height, PropValue::Number(10_000.0)),
                (
                    Prop::Margin,
                    PropValue::Insets(Insets::from_values(&[8.0]).unwrap()),
                ),
            ],
        );
        p.overhang = Insets::all(10.0);
        let mut c = layer_config(&p).unwrap();
        c.fit((1920, 1080));
        assert_eq!((c.width, c.height), (620, 1080 - 16 + 20));
        let (_, _, bw, bh) = c.input_region((c.width, c.height)).unwrap().unwrap();
        assert_eq!((bw, bh), (600, 1080 - 16), "the box fits the output");
        let bar = spec(NodeKind::Bar, &[(Prop::Height, PropValue::Number(36.0))]);
        let mut c = layer_config(&bar).unwrap();
        c.fit((1920, 1080));
        assert_eq!((c.width, c.height), (0, 36));
    }

    /// A centred panel with an even overhang (render makes it even on
    /// centred axes) has its box, and so its input region, in the middle
    /// of its buffer: the compositor centres the buffer, so the box is
    /// centred too.
    #[test]
    fn a_centred_panel_with_an_even_overhang_stays_centred() {
        let mut p = spec(
            NodeKind::Panel,
            &[
                (Prop::Width, PropValue::Number(600.0)),
                (Prop::Height, PropValue::Number(200.0)),
            ],
        );
        p.overhang = Insets {
            top: 89.0,
            right: 73.0,
            bottom: 89.0,
            left: 73.0,
        };
        let c = layer_config(&p).unwrap();
        assert_eq!(c.anchors, Anchors::default());
        let (x, y, w, h) = c.input_region((c.width, c.height)).unwrap().unwrap();
        assert_eq!(x * 2 + w, c.width as i32);
        assert_eq!(y * 2 + h, c.height as i32);
        // On a 1080 px output the compositor puts the buffer at
        // (1080 - height) / 2: the box's centre is the output's.
        let top = (1080 - c.height as i32) / 2;
        assert_eq!(top + y + h / 2, 540);
    }

    /// A layer surface lands where wlroots arranges it: centred with no
    /// anchor (margins ignored), at its margin from an anchored edge; its
    /// box is inside its overhang.
    #[test]
    fn surfaces_land_where_the_compositor_arranges_them() {
        let mut p = spec(
            NodeKind::Panel,
            &[
                (Prop::Width, PropValue::Number(600.0)),
                (Prop::Height, PropValue::Number(200.0)),
            ],
        );
        p.overhang = Insets::all(10.0);
        let c = layer_config(&p).unwrap();
        assert_eq!(c.position_in((620, 220), (1920, 1044)), (650, 412));
        assert_eq!(c.box_in((620, 220), (1920, 1044)), (660, 422, 600, 200));
        let mut p = spec(
            NodeKind::Panel,
            &[
                (Prop::Anchor, kw("top_right")),
                (Prop::Width, PropValue::Number(380.0)),
                (Prop::Height, PropValue::Number(200.0)),
                (
                    Prop::Margin,
                    PropValue::Insets(Insets::from_values(&[8.0]).unwrap()),
                ),
            ],
        );
        p.overhang = Insets::all(10.0);
        let c = layer_config(&p).unwrap();
        // Margins move out by the overhang: the box sits 8 px in.
        assert_eq!(
            c.box_in((400, 220), (1920, 1044)),
            (1920 - 8 - 380, 8, 380, 200)
        );
    }

    /// Values a `.strand` file can hold, however large, never overflow:
    /// margins, a thickness and the shadow overhang are clamped, so the
    /// sums the config takes stay in range (debug builds panic on
    /// overflow; release builds would wrap into a huge margin).
    #[test]
    fn huge_values_do_not_overflow() {
        let mut bar = spec(
            NodeKind::Bar,
            &[
                (Prop::Height, PropValue::Number(1e12)),
                (
                    Prop::Margin,
                    PropValue::Insets(Insets::from_values(&[-1e12, 1e12]).unwrap()),
                ),
            ],
        );
        bar.overhang = Insets::all(1e12);
        let c = layer_config(&bar).unwrap();
        assert_eq!(c.margin[0], -MAX_LOGICAL - MAX_LOGICAL);
        assert_eq!(c.exclusive_zone, 2 * MAX_LOGICAL);
        assert!(c.height > 0);
        let mut p = spec(
            NodeKind::Panel,
            &[
                (Prop::Anchor, kw("bottom_right")),
                (Prop::Width, PropValue::Number(1e12)),
                (Prop::Height, PropValue::Number(1e12)),
                (
                    Prop::Margin,
                    PropValue::Insets(Insets::from_values(&[-1e12]).unwrap()),
                ),
            ],
        );
        p.overhang = Insets::all(1e12);
        let c = layer_config(&p).unwrap();
        let size = (c.width, c.height);
        for area in [(1920, 1080), (u32::MAX, u32::MAX), (0, 0)] {
            let _ = c.position_in(size, area);
            let _ = c.box_in(size, area);
            let _ = c.box_in((u32::MAX, u32::MAX), area);
        }
        let mut c = c;
        c.fit((1920, 1080));
        assert!(c.width >= 1);
    }

    #[test]
    fn content_sized_and_non_layer_surfaces_are_errors() {
        assert_eq!(
            layer_config(&spec(NodeKind::Bar, &[])),
            Err(PlacementError::AutoSize(NodeKind::Bar))
        );
        assert_eq!(
            layer_config(&spec(
                NodeKind::Osd,
                &[(Prop::Width, PropValue::Number(9.0))]
            )),
            Err(PlacementError::AutoSize(NodeKind::Osd))
        );
        let e = layer_config(&spec(NodeKind::Popup, &[])).unwrap_err();
        assert_eq!(e, PlacementError::NotLayerSurface(NodeKind::Popup));
        assert!(e.to_string().contains("popup"));
    }

    /// A popup in a top bar opens below the bar (its anchor spans the
    /// bar's thickness under the clock), 6 px away; in a bottom bar,
    /// above; nested in another popup, beside its anchor in that popup's
    /// box (its overhang taken off); a tooltip below what it describes,
    /// with no grab and no input.
    #[test]
    fn popups_open_away_from_their_bar() {
        use strand_scene::{LogicalRect, NodeId};
        let mut bar = spec(NodeKind::Bar, &[(Prop::Height, PropValue::Number(36.0))]);
        bar.overhang = Insets::all(4.0);
        let mut p = spec(NodeKind::Popup, &[]);
        p.parent = Some(NodeId::new(1, 0));
        assert_eq!(
            popup_config(&p, &bar),
            Err(PlacementError::AutoSize(NodeKind::Popup)),
            "no size or anchor yet"
        );
        p.width = Some(200.0);
        p.height = Some(120.0);
        p.anchor_rect = Some(LogicalRect::new(100.0, 12.0, 60.0, 20.0));
        p.overhang = Insets::all(10.0);
        let c = popup_config(&p, &bar).unwrap();
        assert_eq!(c.side, PopupSide::Below);
        assert_eq!(c.anchor_rect, (100, 4, 60, 36), "across the bar's box");
        assert_eq!((c.gap, c.grab), (6, true));
        assert_eq!(c.buffer(), (220, 140));
        assert_eq!(
            c.as_layer().input_region((220, 140)),
            Some(Some((10, 10, 200, 120)))
        );
        bar.edge = Some(Edge::Bottom);
        assert_eq!(popup_config(&p, &bar).unwrap().side, PopupSide::Above);
        // Nested: in its parent popup's box.
        let mut child = p.clone();
        child.anchor_rect = Some(LogicalRect::new(30.0, 40.0, 50.0, 20.0));
        child.margin = Insets::all(2.0);
        let c = popup_config(&child, &p).unwrap();
        assert_eq!(c.anchor_rect, (20, 30, 50, 20));
        assert_eq!((c.side, c.gap, c.aligned), (PopupSide::Right, 2, true));
        // A tooltip: under its node, no grab, click-through.
        let mut tip = p.clone();
        tip.tooltip = true;
        bar.edge = Some(Edge::Top);
        let c = popup_config(&tip, &bar).unwrap();
        assert_eq!(c.anchor_rect, (100, 12, 60, 20));
        assert!(!c.grab && c.as_layer().click_through);
    }

    /// (M4) A popup in a popup is a submenu: it opens to the side of its
    /// row, right unless `anchor:` names left (a left corner too), top or
    /// bottom, level with the row's top when beside it (`aligned`), at
    /// the usual 6 px gap. `attach:` still names its touching side,
    /// centred; a popup in a bar or panel keeps opening below, centred,
    /// whatever its `anchor:`.
    #[test]
    fn submenus_open_to_the_side_their_anchor_names() {
        use strand_scene::{LogicalRect, NodeId};
        let mut menu = spec(NodeKind::Popup, &[]);
        menu.parent = Some(NodeId::new(1, 0));
        menu.width = Some(160.0);
        menu.height = Some(200.0);
        menu.anchor_rect = Some(LogicalRect::new(40.0, 8.0, 20.0, 20.0));
        menu.overhang = Insets::all(8.0);
        let mut sub = spec(NodeKind::Popup, &[]);
        sub.parent = Some(NodeId::new(2, 0));
        sub.width = Some(120.0);
        sub.height = Some(80.0);
        // The third row, 8 px inside the menu's overhang.
        sub.anchor_rect = Some(LogicalRect::new(12.0, 72.0, 144.0, 28.0));
        let c = popup_config(&sub, &menu).unwrap();
        assert_eq!(
            (c.side, c.aligned, c.gap, c.anchor_rect),
            (PopupSide::Right, true, 6, (4, 64, 144, 28))
        );
        for (anchor, side, aligned) in [
            ("right", PopupSide::Right, true),
            ("top_right", PopupSide::Right, true),
            ("bottom_right", PopupSide::Right, true),
            ("left", PopupSide::Left, true),
            ("top_left", PopupSide::Left, true),
            ("bottom_left", PopupSide::Left, true),
            ("top", PopupSide::Above, false),
            ("bottom", PopupSide::Below, false),
            ("center", PopupSide::Right, true),
        ] {
            let mut s = sub.clone();
            s.anchor = Anchor::from_name(anchor).unwrap();
            let c = popup_config(&s, &menu).unwrap();
            assert_eq!((c.side, c.aligned), (side, aligned), "{anchor}");
        }
        let mut s = sub.clone();
        s.margin = Insets {
            left: 0.0,
            right: 3.0,
            top: 0.0,
            bottom: 0.0,
        };
        s.anchor = Anchor::Left;
        assert_eq!(popup_config(&s, &menu).unwrap().gap, 3, "its margin");
        s.attach = Some(Edge::Top);
        let c = popup_config(&s, &menu).unwrap();
        assert_eq!((c.side, c.aligned, c.gap), (PopupSide::Below, false, 0));
        // In a bar (or a panel) `anchor:` does not turn a popup.
        let bar = spec(NodeKind::Bar, &[(Prop::Height, PropValue::Number(36.0))]);
        let mut m = menu.clone();
        m.anchor = Anchor::Left;
        let c = popup_config(&m, &bar).unwrap();
        assert_eq!((c.side, c.aligned), (PopupSide::Below, false));
        let panel = spec(
            NodeKind::Panel,
            &[
                (Prop::Width, PropValue::Number(300.0)),
                (Prop::Height, PropValue::Number(300.0)),
            ],
        );
        let c = popup_config(&m, &panel).unwrap();
        assert_eq!((c.side, c.aligned), (PopupSide::Below, false));
        // A tooltip in a menu still sits under its row.
        let mut tip = sub.clone();
        tip.tooltip = true;
        let c = popup_config(&tip, &menu).unwrap();
        assert_eq!((c.side, c.aligned), (PopupSide::Below, false));
    }

    /// `attach: top` on a panel: anchored to the top edge whatever its
    /// `anchor:` said about that axis (the other axis stays), its top
    /// margin gone (the box flush with the edge), the others kept.
    #[test]
    fn an_attached_panel_is_flush_with_its_edge() {
        let s = spec(
            NodeKind::Panel,
            &[
                (Prop::Anchor, kw("bottom_right")),
                (Prop::Attach, kw("top")),
                (Prop::Margin, PropValue::Insets(Insets::all(8.0))),
                (Prop::Width, PropValue::Number(400.0)),
                (Prop::Height, PropValue::Number(300.0)),
            ],
        );
        let mut s2 = s.clone();
        s2.overhang = Insets {
            top: 0.0,
            right: 16.0,
            bottom: 0.0,
            left: 16.0,
        };
        let c = layer_config(&s).unwrap();
        assert_eq!(c.anchors, Anchors::new(true, false, false, true));
        assert_eq!(c.margin, [0, 8, 8, 8]);
        // The fillets' overhang moves the side margins out; the box stays.
        let c = layer_config(&s2).unwrap();
        assert_eq!(c.margin, [0, -8, 8, -8]);
        assert_eq!(c.width, 432);
        for (edge, anchors) in [
            ("bottom", Anchors::new(false, true, false, true)),
            ("left", Anchors::new(false, true, true, false)),
            ("right", Anchors::new(false, true, false, true)),
        ] {
            let mut t = s.clone();
            t.attach = Edge::from_name(edge);
            assert_eq!(layer_config(&t).unwrap().anchors, anchors, "{edge}");
        }
        let mut centred = spec(
            NodeKind::Panel,
            &[
                (Prop::Attach, kw("left")),
                (Prop::Width, PropValue::Number(100.0)),
                (Prop::Height, PropValue::Number(100.0)),
            ],
        );
        assert_eq!(
            layer_config(&centred).unwrap().anchors,
            Anchors::new(false, false, true, false)
        );
        centred.attach = None;
        assert_eq!(layer_config(&centred).unwrap().anchors, Anchors::default());
    }

    /// `attach:` on a popup names its side that touches the anchor: it
    /// opens away from it, at gap 0 (whatever its margin), even away from
    /// a bar's usual side.
    #[test]
    fn an_attached_popup_touches_its_anchor() {
        use strand_scene::{LogicalRect, NodeId};
        let bar = spec(NodeKind::Bar, &[(Prop::Height, PropValue::Number(36.0))]);
        let mut p = spec(
            NodeKind::Popup,
            &[
                (Prop::Attach, kw("top")),
                (Prop::Margin, PropValue::Insets(Insets::all(10.0))),
            ],
        );
        p.parent = Some(NodeId::new(1, 0));
        p.width = Some(200.0);
        p.height = Some(120.0);
        p.anchor_rect = Some(LogicalRect::new(100.0, 8.0, 60.0, 20.0));
        let c = popup_config(&p, &bar).unwrap();
        assert_eq!((c.side, c.gap), (PopupSide::Below, 0));
        for (edge, side) in [
            ("bottom", PopupSide::Above),
            ("left", PopupSide::Right),
            ("right", PopupSide::Left),
        ] {
            p.attach = Edge::from_name(edge);
            let c = popup_config(&p, &bar).unwrap();
            assert_eq!((c.side, c.gap), (side, 0), "{edge}");
        }
    }

    /// A `top` panel with a scrim rises to `overlay`, so its scrim fits on
    /// `top` beneath it and above the windows; other layers keep theirs,
    /// and the scrim's layer is the one below.
    #[test]
    fn a_top_panel_with_a_scrim_rises_to_overlay() {
        let dim = PropValue::Color(strand_scene::Color::new(0.0, 0.0, 0.0, 0.3));
        let size = [
            (Prop::Width, PropValue::Number(100.0)),
            (Prop::Height, PropValue::Number(100.0)),
        ];
        let mut props = size.to_vec();
        props.push((Prop::Scrim, dim.clone()));
        assert_eq!(
            layer_config(&spec(NodeKind::Panel, &props)).unwrap().layer,
            Layer::Overlay
        );
        assert_eq!(
            layer_config(&spec(NodeKind::Panel, &size)).unwrap().layer,
            Layer::Top
        );
        props.push((Prop::Layer, kw("bottom")));
        assert_eq!(
            layer_config(&spec(NodeKind::Panel, &props)).unwrap().layer,
            Layer::Bottom
        );
        assert_eq!(layer_below(Layer::Overlay), Layer::Top);
        assert_eq!(layer_below(Layer::Bottom), Layer::Background);
        assert_eq!(layer_below(Layer::Background), Layer::Background);
    }

    /// A popup's scrim goes below a panel it is nested in (which rises
    /// from `top` to `overlay` for it), and beside a bar, whose
    /// exclusive zone the scrim leaves out.
    #[test]
    fn a_popup_scrim_goes_below_its_panel_and_beside_its_bar() {
        let size = [
            (Prop::Width, PropValue::Number(100.0)),
            (Prop::Height, PropValue::Number(100.0)),
        ];
        let panel = spec(NodeKind::Panel, &size);
        let raised = layer_config_with(&panel, true).unwrap();
        assert_eq!(raised.layer, Layer::Overlay);
        assert_eq!(popup_scrim_layer(&raised), Layer::Top);
        assert_eq!(layer_config_with(&panel, false).unwrap().layer, Layer::Top);
        let mut bottom = size.to_vec();
        bottom.push((Prop::Layer, kw("bottom")));
        let c = layer_config_with(&spec(NodeKind::Panel, &bottom), true).unwrap();
        assert_eq!(
            (c.layer, popup_scrim_layer(&c)),
            (Layer::Bottom, Layer::Background)
        );

        let bar = spec(NodeKind::Bar, &[(Prop::Height, PropValue::Number(30.0))]);
        let c = layer_config_with(&bar, true).unwrap();
        assert_eq!(c.layer, Layer::Top, "a bar does not rise");
        assert_eq!(popup_scrim_layer(&c), Layer::Top);
    }

    /// A bar whose shadow reaches past its exclusive zone into the
    /// usable area is a panel to its popup's scrim: the scrim goes on
    /// the layer below, and a `top` bar rises to `overlay` for it. An
    /// overhang towards the edge or along it stays inside the zone.
    #[test]
    fn a_bar_shadowing_the_usable_area_rises_over_its_popup_scrim() {
        for (edge, away) in [
            (
                "top",
                Insets {
                    bottom: 6.0,
                    ..Insets::default()
                },
            ),
            (
                "bottom",
                Insets {
                    top: 6.0,
                    ..Insets::default()
                },
            ),
            (
                "left",
                Insets {
                    right: 6.0,
                    ..Insets::default()
                },
            ),
            (
                "right",
                Insets {
                    left: 6.0,
                    ..Insets::default()
                },
            ),
        ] {
            let mut bar = spec(
                NodeKind::Bar,
                &[
                    (Prop::Edge, kw(edge)),
                    (Prop::Height, PropValue::Number(30.0)),
                    (Prop::Width, PropValue::Number(30.0)),
                ],
            );
            bar.overhang = away;
            let c = layer_config_with(&bar, true).unwrap();
            assert!(!c.inside_exclusive_zone(), "{edge}");
            assert_eq!(
                (c.layer, popup_scrim_layer(&c)),
                (Layer::Overlay, Layer::Top),
                "{edge}"
            );
            let c = layer_config_with(&bar, false).unwrap();
            assert_eq!(c.layer, Layer::Top, "{edge}: no scrim, no rise");
        }

        // A top bar's shadow above it and to its sides is in the zone.
        let mut bar = spec(NodeKind::Bar, &[(Prop::Height, PropValue::Number(30.0))]);
        bar.overhang = Insets {
            top: 6.0,
            left: 6.0,
            right: 6.0,
            bottom: 0.0,
        };
        let c = layer_config_with(&bar, true).unwrap();
        assert!(c.inside_exclusive_zone());
        assert_eq!((c.layer, popup_scrim_layer(&c)), (Layer::Top, Layer::Top));

        // A shadowed bar the user put on `bottom` keeps it, its popup's
        // scrim one below.
        let mut bar = spec(
            NodeKind::Bar,
            &[
                (Prop::Height, PropValue::Number(30.0)),
                (Prop::Layer, kw("bottom")),
            ],
        );
        bar.overhang = Insets::all(6.0);
        let c = layer_config_with(&bar, true).unwrap();
        assert_eq!(
            (c.layer, popup_scrim_layer(&c)),
            (Layer::Bottom, Layer::Background)
        );
    }
}
