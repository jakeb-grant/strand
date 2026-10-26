//! How a [`SurfaceSpec`] maps onto `zwlr_layer_surface_v1` state: anchor,
//! size, exclusive zone, margins, layer, keyboard focus and namespace.
//! Pure, so it is tested without a compositor, and shared by every host
//! that places surfaces: strand-surface's manager (which re-exports it as
//! `strand_surface::placement`), eval's driver and the web host
//! (strand-web), which cannot link Wayland (docs/decisions.md,
//! live-examples).

use crate::{Anchor, Edge, Keyboard, Layer, NodeKind, SurfaceSpec};

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

    /// Where a surface of this config lands in an output's usable area
    /// (in the output's logical coordinates), as
    /// [`LayerConfig::position_in`] places it in an area of that size.
    pub fn position_in_rect(&self, size: (u32, u32), area: UsableArea) -> (i32, i32) {
        let (x, y) = self.position_in(size, (area.w, area.h));
        (x.saturating_add(area.x), y.saturating_add(area.y))
    }

    /// The edge this surface's exclusive zone reserves, as wlroots
    /// applies one (`apply_exclusive`): anchored to that edge alone, or
    /// to it and both of its neighbours. `None` for no zone (0 or -1) or
    /// any other anchoring.
    pub fn reserved_edge(&self) -> Option<Edge> {
        if self.exclusive_zone <= 0 {
            return None;
        }
        let a = self.anchors;
        match (a.top, a.bottom, a.left, a.right) {
            (true, false, false, false) | (true, false, true, true) => Some(Edge::Top),
            (false, true, false, false) | (false, true, true, true) => Some(Edge::Bottom),
            (false, false, true, false) | (true, true, true, false) => Some(Edge::Left),
            (false, false, false, true) | (true, true, false, true) => Some(Edge::Right),
            _ => None,
        }
    }

    /// Takes this surface's exclusive zone, and its margin on that edge,
    /// out of `area`, as the compositor does once it has arranged it.
    pub fn reserve(&self, area: &mut UsableArea) {
        let Some(edge) = self.reserved_edge() else {
            return;
        };
        let [mt, mr, mb, ml] = self.margin;
        let margin = match edge {
            Edge::Top => mt,
            Edge::Bottom => mb,
            Edge::Left => ml,
            Edge::Right => mr,
        };
        let by = i64::from(self.exclusive_zone) + i64::from(margin);
        if by <= 0 {
            return;
        }
        let shrink = |v: u32| (i64::from(v) - by).max(0) as u32;
        let by = by.min(i64::from(i32::MAX)) as i32;
        match edge {
            Edge::Top => {
                area.y = area.y.saturating_add(by);
                area.h = shrink(area.h);
            }
            Edge::Bottom => area.h = shrink(area.h),
            Edge::Left => {
                area.x = area.x.saturating_add(by);
                area.w = shrink(area.w);
            }
            Edge::Right => area.w = shrink(area.w),
        }
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
pub fn posed_margin(config: &LayerConfig, offset: crate::LogicalPoint) -> [i32; 4] {
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

/// (M4) Where an output arranges layer surfaces: logical pixels, in the
/// output's own coordinates (its top-left corner is 0, 0).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct UsableArea {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl UsableArea {
    /// The whole output of logical size `(w, h)`.
    pub fn full((w, h): (u32, u32)) -> Self {
        Self { x: 0, y: 0, w, h }
    }
}

/// (M4) The area the layer surface `target` is arranged in on an output
/// of logical size `full` that holds `surfaces` (ours, in the order they
/// were made, `target` among them), as wlroots compositors arrange them
/// (sway's `arrange_layers`, `wlr_scene_layer_surface_v1_configure`):
/// the surfaces with an exclusive zone first, layer by layer from the
/// overlay down, each in what the ones before it left and reserving its
/// zone from it; then every other surface in what is left; a surface
/// whose zone is -1 in the whole output. Other programs' zones are not
/// known to a client, so they are not taken out.
pub fn arranged_area<K: PartialEq>(
    full: (u32, u32),
    surfaces: &[(K, &LayerConfig)],
    target: &K,
) -> UsableArea {
    let mut area = UsableArea::full(full);
    let Some((_, own)) = surfaces.iter().find(|(k, _)| k == target) else {
        return area;
    };
    if own.exclusive_zone < 0 {
        return area;
    }
    for layer in [Layer::Overlay, Layer::Top, Layer::Bottom, Layer::Background] {
        let exclusive = surfaces
            .iter()
            .filter(|(_, c)| c.layer == layer && c.exclusive_zone > 0);
        for (k, c) in exclusive {
            if k == target {
                return area;
            }
            c.reserve(&mut area);
        }
    }
    area
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

/// (flex) The edge an exclusive zone applies to with these anchors, as
/// layer-shell has it: anchored to that edge alone, or to it and both
/// edges beside it.
fn one_edge(a: Anchors) -> Option<Edge> {
    match (a.top, a.bottom, a.left, a.right) {
        (true, false, false, false) | (true, false, true, true) => Some(Edge::Top),
        (false, true, false, false) | (false, true, true, true) => Some(Edge::Bottom),
        (false, false, true, false) | (true, true, true, false) => Some(Edge::Left),
        (false, false, false, true) | (true, true, false, true) => Some(Edge::Right),
        _ => None,
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
    // `attach: <edge>` (a panel) anchors that edge; its margin there is
    // kept like any margin (0 unless set: flush with the edge or past a
    // bar's exclusive zone; negative over it, owner-testing.md gap 26).
    let attach = spec.attach.filter(|_| spec.kind == NodeKind::Panel);
    // The box keeps its place: each margin moves out by the overhang.
    let margin = [
        px(m.top) - ot,
        px(m.right) - or,
        px(m.bottom) - ob,
        px(m.left) - ol,
    ];
    let (anchors, width, height, exclusive_zone) = if spec.kind == NodeKind::Bar {
        let edge = spec.edge.unwrap_or(Edge::Top);
        let thickness = spec
            .thickness()
            .ok_or(PlacementError::AutoSize(spec.kind))?;
        let t = size(thickness);
        let (tv, th) = (t + (ot + ob) as u32, t + (ol + or) as u32);
        // (flex) What it keeps: its thickness unless `exclusive:` says.
        let zone = spec.exclusive_zone().unwrap_or(thickness);
        // The zone counts from the (moved) margin: it reserves the box
        // plus the overhang towards the edge, so the reserved space stays
        // margin + thickness.
        let reach = |o: i32| if zone > 0.0 { px(zone) + o } else { 0 };
        match edge {
            Edge::Top => (Anchors::new(true, false, true, true), 0, tv, reach(ot)),
            Edge::Bottom => (Anchors::new(false, true, true, true), 0, tv, reach(ob)),
            Edge::Left => (Anchors::new(true, true, true, false), th, 0, reach(ol)),
            Edge::Right => (Anchors::new(true, true, false, true), th, 0, reach(or)),
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
        // (flex) A panel's `exclusive:` along the one edge it is
        // anchored to; a corner or centre keeps nothing (layer-shell
        // reserves only along one edge).
        let zone = match (spec.exclusive_edge(), spec.exclusive_zone()) {
            (Some(edge), Some(z)) if z > 0.0 && one_edge(anchors) == Some(edge) => {
                px(z)
                    + match edge {
                        Edge::Top => ot,
                        Edge::Bottom => ob,
                        Edge::Left => ol,
                        Edge::Right => or,
                    }
            }
            _ => 0,
        };
        (
            anchors,
            size(w) + (ol + or) as u32,
            size(h) + (ot + ob) as u32,
            zone,
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

/// (window) Everything a `window`'s `xdg_toplevel` is told: what it is
/// called, the size it asks for and the sizes it takes, and how far its
/// buffer reaches past its window geometry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowConfig {
    /// Its namespace (`strand-<Name>`), as a layer surface's: logs and
    /// the reload identity of its surface.
    pub namespace: String,
    pub title: String,
    pub app_id: String,
    /// The box it asks for (its window geometry), logical pixels: the
    /// compositor's configure may give another.
    pub width: u32,
    pub height: u32,
    /// `xdg_toplevel.set_min_size` and `set_max_size`, window geometry,
    /// 0 for none on that axis.
    pub min: (u32, u32),
    pub max: (u32, u32),
    /// Shadow reach past the box, top, right, bottom, left: the buffer is
    /// the box plus it, and the window geometry leaves it out.
    pub overhang: [i32; 4],
    /// Who draws its title bar and borders.
    pub decorations: crate::Decorations,
}

impl WindowConfig {
    /// The buffer's logical size for a box of `(w, h)`: plus the
    /// overhang.
    pub fn buffer(&self, (w, h): (u32, u32)) -> (u32, u32) {
        let [t, r, b, l] = self.overhang;
        (
            w.saturating_add((l + r).max(0) as u32),
            h.saturating_add((t + b).max(0) as u32),
        )
    }

    /// Its input region and namespace, as a layer surface's: the box
    /// inside the overhang.
    pub fn as_layer(&self) -> LayerConfig {
        let (w, h) = self.buffer((self.width, self.height));
        LayerConfig {
            namespace: self.namespace.clone(),
            layer: Layer::Top,
            anchors: Anchors::default(),
            width: w,
            height: h,
            exclusive_zone: 0,
            margin: [0; 4],
            keyboard: Keyboard::OnDemand,
            overhang: self.overhang,
            click_through: false,
        }
    }
}

/// (window) The size a window keeps on one axis from a configure that
/// gives it `given` (`None` or 0: its choice) when it asked for `asked`,
/// and whether that is the compositor's: a size the compositor gives is
/// where a state constrains it (`constrained`: maximized, fullscreen,
/// tiled, being resized) or where it is not the one asked for (the user
/// resized a floating window); an echo of the asked size, or none,
/// leaves the size to the window. The surface manager's configure and
/// the evaluation sandbox's compositor both size a window by it.
pub fn window_kept_size(given: Option<u32>, asked: u32, constrained: bool) -> (bool, u32) {
    match given.filter(|g| *g > 0) {
        Some(g) if constrained || g != asked => (true, g),
        _ => (false, asked.max(1)),
    }
}

/// (window) Resolves the `xdg_toplevel` state of a `window` spec. Waits
/// (`AutoSize`) until render gave it a size; `NotLayerSurface` names any
/// other kind.
pub fn window_config(spec: &SurfaceSpec) -> Result<WindowConfig, PlacementError> {
    if spec.kind != NodeKind::Window {
        return Err(PlacementError::NotLayerSurface(spec.kind));
    }
    let (Some(w), Some(h)) = (spec.width, spec.height) else {
        return Err(PlacementError::AutoSize(spec.kind));
    };
    let o = spec.overhang;
    let overhang = [o.top, o.right, o.bottom, o.left].map(|v| px(v).max(0));
    let opt = |v: Option<f32>| v.map_or(0, |v| px(v).max(0) as u32);
    let (min_w, min_h) = (opt(spec.min_size.0), opt(spec.min_size.1));
    let (max_w, max_h) = (opt(spec.max_size.0), opt(spec.max_size.1));
    // A minimum over the maximum is `xdg_toplevel.invalid_size`, which
    // ends the whole connection (every bar and panel with it): the
    // maximum is raised to the minimum (`check::window_min_max` refuses
    // it when both are written as numbers; a binding can still make it).
    let fit = |min: u32, max: u32, axis: &str| {
        if max > 0 && min > max {
            log::warn!(
                "{}: min_{axis} {min} is over max_{axis} {max}: max_{axis} taken as {min}",
                spec.namespace()
            );
            min
        } else {
            max
        }
    };
    let (max_w, max_h) = (fit(min_w, max_w, "width"), fit(min_h, max_h, "height"));
    Ok(WindowConfig {
        namespace: spec.namespace(),
        title: spec.window_title(),
        app_id: spec.window_app_id(),
        width: size(w),
        height: size(h),
        min: (min_w, min_h),
        max: (max_w, max_h),
        overhang,
        decorations: spec.decorations,
    })
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
/// otherwise; `margin` on the side facing its anchor is the gap: 6 px
/// only while `margin` is unset (0 with `attach:`), so `0` touches the
/// anchor and a negative margin overlaps it (spec.md 5.1). Waits
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
    // overhang), and (window) a window's; a layer surface's is its whole
    // buffer.
    let (gx, gy) = if parent_popup || parent.kind == NodeKind::Window {
        (pl, pt)
    } else {
        (0, 0)
    };
    let (mut x, mut y, mut aw, mut ah) = (px(a.x) - gx, px(a.y) - gy, px(a.w), px(a.h));
    let bar_edge = (parent.kind == NodeKind::Bar).then(|| parent.edge.unwrap_or(Edge::Top));
    // `attach: <edge>` names the popup's side that touches its anchor,
    // so it opens away from it; its margin there is the gap as always,
    // 0 while unset.
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
    // (flex) `anchor: pointer`: below and to the right of the press,
    // touching it unless a margin says otherwise.
    let side = if spec.at_pointer && spec.attach.is_none() {
        PopupSide::Below
    } else {
        side
    };
    // A submenu beside its row starts level with it; a popup at the
    // pointer starts at it.
    let aligned = (spec.at_pointer && spec.attach.is_none())
        || (parent_popup
            && spec.attach.is_none()
            && matches!(side, PopupSide::Right | PopupSide::Left));
    if let Some(edge) = bar_edge.filter(|_| !spec.at_pointer) {
        // Across the whole bar: from its box's edge to its other edge.
        let thick = parent.thickness().map_or(0, |t| px(t).max(0));
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
    // (owner-testing.md gap 26) `attach:` changes only the unset gap:
    // a set margin is kept, so a negative one takes an attached popup
    // over its bar to the screen's edge, where its fillets meet.
    let gap = match (spec.margin_set, spec.attach) {
        (true, _) => px(gap),
        (false, Some(_)) => 0,
        (false, None) if spec.at_pointer => 0,
        (false, None) => 6,
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

/// The box (`x, y, w, h`, relative to the parent's window geometry) an
/// `xdg_positioner` for `c` asks for before the compositor constrains
/// it, with the anchor rect `rect` (`c.anchor_rect`, posed) and the
/// positioner's offset moved by `shift`: xdg-shell's anchor point and
/// gravity, with integer halves as wlroots takes them.
pub fn popup_request(
    c: &PopupConfig,
    rect: (i32, i32, i32, i32),
    shift: (i32, i32),
) -> (i32, i32, i32, i32) {
    let (ax, ay, aw, ah) = rect;
    let (w, h) = (c.width.max(1) as i32, c.height.max(1) as i32);
    let (x, y) = match (c.side, c.aligned) {
        // (flex) At the pointer: from the press, down and right.
        (PopupSide::Below, true) => (ax + aw, ay + ah + c.gap),
        (PopupSide::Below, _) => (ax + aw / 2 - w / 2, ay + ah + c.gap),
        (PopupSide::Above, _) => (ax + aw / 2 - w / 2, ay - h - c.gap),
        (PopupSide::Right, false) => (ax + aw + c.gap, ay + ah / 2 - h / 2),
        (PopupSide::Left, false) => (ax - w - c.gap, ay + ah / 2 - h / 2),
        (PopupSide::Right, true) => (ax + aw + c.gap, ay),
        (PopupSide::Left, true) => (ax - w - c.gap, ay),
    };
    (x + shift.0, y + shift.1, w, h)
}

/// (owner-testing.md bug 6) How far to move the positioner's offset so
/// the box `c` asks for (anchor rect `rect`) lies inside `within` (its
/// output, relative to the parent's window geometry) along its anchor's
/// edge: across for a popup below or above its anchor, up or down for
/// one beside it, as `slide_x`/`slide_y` would (a box wider than the
/// output starts at its start). The side the popup opens on is left to
/// the compositor's flip. The compositor need not slide it, which
/// Hyprland 0.56 gets wrong for a popup with a window geometry offset
/// (a shadow's overhang).
pub fn popup_shift(
    c: &PopupConfig,
    rect: (i32, i32, i32, i32),
    within: (i32, i32, i32, i32),
) -> (i32, i32) {
    let (x, y, w, h) = popup_request(c, rect, (0, 0));
    let (wx, wy, ww, wh) = within;
    let slide = |v: i32, len: i32, lo: i32, room: i32| -> i32 {
        if room <= 0 {
            return 0;
        }
        let mut d = 0;
        if v + len > lo + room {
            d = lo + room - (v + len);
        }
        if v + d < lo {
            d = lo - v;
        }
        d
    };
    match c.side {
        PopupSide::Below | PopupSide::Above => (slide(x, w, wx, ww), 0),
        PopupSide::Right | PopupSide::Left => (0, slide(y, h, wy, wh)),
    }
}

/// Whether the positioner for `c` (anchor rect `rect`, offset moved by
/// `shift`) asks the compositor to slide it across and down: on an axis
/// only where its box does not lie wholly inside `within` (its output,
/// relative to the parent's window geometry, as for [`popup_shift`]),
/// and on both while the output is not known. A box that fits is where
/// it asked to be: Hyprland 0.56 slides into its output less 4 px on
/// every side (`CXDGPositionerRules::getPosition`), so a slide asked
/// for anyway moved a card lifted over its top bar to the screen's edge
/// 4 px down, a sliver of wallpaper above it.
pub fn popup_slides(
    c: &PopupConfig,
    rect: (i32, i32, i32, i32),
    shift: (i32, i32),
    within: Option<(i32, i32, i32, i32)>,
) -> (bool, bool) {
    let Some((wx, wy, ww, wh)) = within else {
        return (true, true);
    };
    let (x, y, w, h) = popup_request(c, rect, shift);
    let inside = |v: i32, len: i32, lo: i32, room: i32| {
        let (v, len, lo, room) = (i64::from(v), i64::from(len), i64::from(lo), i64::from(room));
        v >= lo && v + len <= lo + room
    };
    (!inside(x, w, wx, ww), !inside(y, h, wy, wh))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Insets, Prop, PropValue};
    use std::collections::HashMap;

    fn spec(kind: NodeKind, props: &[(Prop, PropValue)]) -> SurfaceSpec {
        let map: HashMap<Prop, PropValue> = props.iter().cloned().collect();
        SurfaceSpec::resolve(kind, |p| map.get(&p))
    }

    fn kw(k: &str) -> PropValue {
        PropValue::Keyword(k.into())
    }

    /// (M4) Layer surfaces are arranged in what the exclusive zones
    /// before them leave, as wlroots does: a top bar's zone moves a
    /// corner panel below it and a centred one by half of it, a left
    /// bar on the overlay layer is taken out before a top bar on the
    /// top layer (which then starts past it), a zone of -1 ignores them
    /// all, and a bar's own margin on its edge counts into its zone.
    #[test]
    fn layer_surfaces_are_arranged_past_exclusive_zones() {
        let bar = |edge: &str, h: f32, margin: f32| {
            let mut c = layer_config(&spec(
                NodeKind::Bar,
                &[
                    (Prop::Edge, kw(edge)),
                    (Prop::Height, PropValue::Number(h)),
                    (Prop::Width, PropValue::Number(h)),
                    (
                        Prop::Margin,
                        PropValue::Insets(Insets::from_values(&[margin]).unwrap()),
                    ),
                ],
            ))
            .unwrap();
            assert!(c.exclusive_zone > 0, "{c:?}");
            c.layer = Layer::Top;
            c
        };
        let panel = |anchor: &str| {
            layer_config(&spec(
                NodeKind::Panel,
                &[
                    (Prop::Anchor, kw(anchor)),
                    (Prop::Width, PropValue::Number(400.0)),
                    (Prop::Height, PropValue::Number(300.0)),
                ],
            ))
            .unwrap()
        };
        let full = (1920, 1080);
        let top = bar("top", 36.0, 0.0);
        assert_eq!(top.reserved_edge(), Some(Edge::Top));
        let corner = panel("top_right");
        let centred = panel("center");
        let list = [(1, &top), (2, &corner), (3, &centred)];
        let area = arranged_area(full, &list, &2);
        assert_eq!(
            area,
            UsableArea {
                x: 0,
                y: 36,
                w: 1920,
                h: 1044
            }
        );
        assert_eq!(corner.position_in_rect((400, 300), area), (1520, 36));
        assert_eq!(
            centred.position_in_rect((400, 300), arranged_area(full, &list, &3)),
            (760, 36 + 522 - 150)
        );
        // The bar itself is arranged in the whole output.
        assert_eq!(arranged_area(full, &list, &1), UsableArea::full(full));
        assert_eq!(
            top.position_in_rect((1920, 36), UsableArea::full(full)),
            (0, 0)
        );
        // A left bar on the overlay is arranged first: the top bar starts
        // past it, and the corner panel is below the top bar.
        let mut left = bar("left", 48.0, 0.0);
        left.layer = Layer::Overlay;
        assert_eq!(left.reserved_edge(), Some(Edge::Left));
        let list = [(1, &top), (2, &corner), (4, &left)];
        assert_eq!(
            arranged_area(full, &list, &1),
            UsableArea {
                x: 48,
                y: 0,
                w: 1872,
                h: 1080
            }
        );
        assert_eq!(
            arranged_area(full, &list, &2),
            UsableArea {
                x: 48,
                y: 36,
                w: 1872,
                h: 1044
            }
        );
        // A catcher (zone -1) ignores every zone.
        let mut catcher = panel("top_left");
        catcher.exclusive_zone = -1;
        let list = [(1, &top), (5, &catcher)];
        assert_eq!(arranged_area(full, &list, &5), UsableArea::full(full));
        // A bar's margin on its edge counts into what it takes.
        let floating = bar("bottom", 30.0, 8.0);
        assert_eq!(floating.reserved_edge(), Some(Edge::Bottom));
        let list = [(1, &floating), (2, &corner)];
        let area = arranged_area(full, &list, &2);
        assert_eq!((area.y, area.h), (0, 1080 - 30 - 8));
        // An unknown target and an empty output.
        assert_eq!(arranged_area(full, &list, &9), UsableArea::full(full));
        let mut a = UsableArea::full((10, 10));
        floating.reserve(&mut a);
        assert_eq!(a.h, 0);
    }

    #[test]
    fn poses_move_a_surface_from_its_anchored_edges() {
        use crate::LogicalPoint;
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

    /// (owner-testing.md bug 6) The box a positioner asks for, and the
    /// shift that keeps it inside its output along its anchor's edge: a
    /// popup under a top bar's last item moves left by what passes the
    /// output's right edge, one under its first item right; one that fits
    /// stays; one wider than the output starts at its start; one beside
    /// its anchor moves up or down only.
    #[test]
    fn a_popup_is_shifted_inside_its_output() {
        let c = PopupConfig {
            namespace: "p".into(),
            width: 400,
            height: 120,
            overhang: [31, 37, 43, 37],
            anchor_rect: (0, 0, 1, 1),
            side: PopupSide::Below,
            aligned: false,
            gap: 8,
            grab: false,
        };
        let out = (0, 0, 1920, 1080);
        let right = (1880, 4, 20, 36);
        assert_eq!(popup_request(&c, right, (0, 0)), (1690, 48, 400, 120));
        assert_eq!(popup_shift(&c, right, out), (-170, 0));
        assert_eq!(popup_request(&c, right, (-170, 0)), (1520, 48, 400, 120));
        assert_eq!(popup_shift(&c, (0, 4, 20, 36), out), (190, 0));
        assert_eq!(popup_shift(&c, (900, 4, 20, 36), out), (0, 0));
        // An output to the left of the parent's origin (its layer surface
        // on the next output), and one narrower than the popup.
        assert_eq!(popup_shift(&c, right, (-640, 0, 2880, 1440)), (0, 0));
        assert_eq!(popup_shift(&c, right, (1700, 0, 300, 1080)), (10, 0));
        let beside = PopupConfig {
            side: PopupSide::Right,
            ..c
        };
        assert_eq!(
            popup_request(&beside, (0, 1060, 36, 20), (0, 0)),
            (44, 1010, 400, 120)
        );
        assert_eq!(popup_shift(&beside, (0, 1060, 36, 20), out), (0, -50));
    }

    /// A popup whose box lies inside its output asks for no slide, so a
    /// compositor that slides into a padded output (Hyprland, 4 px)
    /// leaves it where it asked: a card lifted over its top bar to the
    /// screen's edge (a gap of minus the bar's thickness) stays at y 0.
    /// It asks for one on each axis it passes, and on both while its
    /// output is not known.
    #[test]
    fn a_popup_that_fits_its_output_asks_for_no_slide() {
        let c = PopupConfig {
            namespace: "p".into(),
            width: 852,
            height: 300,
            overhang: [0; 4],
            anchor_rect: (0, 0, 1, 1),
            side: PopupSide::Below,
            aligned: false,
            gap: -32,
            grab: false,
        };
        // The island's bar: 440 px wide, centred on a 1920 px output.
        let bar = (0, 0, 440, 32);
        let out = (-740, 0, 1920, 1200);
        assert_eq!(popup_request(&c, bar, (0, 0)), (-206, 0, 852, 300));
        assert_eq!(popup_slides(&c, bar, (0, 0), Some(out)), (false, false));
        assert_eq!(popup_slides(&c, bar, (0, 0), None), (true, true));
        // Past the top, the bottom, and either side.
        let low = PopupConfig {
            gap: -33,
            ..c.clone()
        };
        assert_eq!(popup_slides(&low, bar, (0, 0), Some(out)), (false, true));
        let tall = PopupConfig {
            height: 1201,
            gap: 0,
            ..c.clone()
        };
        assert_eq!(
            popup_slides(&tall, (0, -32, 440, 32), (0, 0), Some(out)),
            (false, true)
        );
        assert_eq!(popup_slides(&c, bar, (-535, 0), Some(out)), (true, false));
        assert_eq!(popup_slides(&c, bar, (-534, 0), Some(out)), (false, false));
        assert_eq!(popup_slides(&c, bar, (1603, 0), Some(out)), (true, false));
        // The box popup_shift moved inside the output asks for none.
        let right = (1100, 0, 20, 32);
        let shift = popup_shift(&c, right, out);
        assert_eq!(popup_slides(&c, right, shift, Some(out)), (false, false));
    }

    /// (flex) `anchor: pointer`: the popup opens from the 1 × 1 anchor
    /// render gives at the press, not across its bar, down and to the
    /// right of it, touching it unless a margin says otherwise.
    #[test]
    fn a_popup_at_the_pointer_opens_from_the_press() {
        use crate::{LogicalRect, NodeId};
        let bar = spec(NodeKind::Bar, &[(Prop::Height, PropValue::Number(36.0))]);
        let mut p = spec(
            NodeKind::Popup,
            &[(Prop::Anchor, PropValue::Keyword("pointer".into()))],
        );
        assert!(p.at_pointer);
        p.parent = Some(NodeId::new(1, 0));
        p.width = Some(120.0);
        p.height = Some(80.0);
        p.anchor_rect = Some(LogicalRect::new(210.0, 12.0, 1.0, 1.0));
        let c = popup_config(&p, &bar).unwrap();
        assert_eq!((c.side, c.aligned), (PopupSide::Below, true));
        assert_eq!(
            c.anchor_rect,
            (210, 12, 1, 1),
            "at the press, not across the bar"
        );
        assert_eq!(c.gap, 0);
        assert_eq!(popup_request(&c, c.anchor_rect, (0, 0)), (211, 13, 120, 80));
        // A set margin is its gap, as any popup's.
        p.margin = Insets::all(4.0);
        p.margin_set = true;
        assert_eq!(popup_config(&p, &bar).unwrap().gap, 4);
        // Not on a panel: `pointer` is a popup's.
        let panel = spec(
            NodeKind::Panel,
            &[(Prop::Anchor, PropValue::Keyword("pointer".into()))],
        );
        assert!(!panel.at_pointer);
    }

    /// A popup in a top bar opens below the bar (its anchor spans the
    /// bar's thickness under the clock), 6 px away; in a bottom bar,
    /// above; nested in another popup, beside its anchor in that popup's
    /// box (its overhang taken off); a tooltip below what it describes,
    /// with no grab and no input.
    #[test]
    fn popups_open_away_from_their_bar() {
        use crate::{LogicalRect, NodeId};
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
        child.margin_set = true;
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
        use crate::{LogicalRect, NodeId};
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
        s.margin_set = true;
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
    /// `anchor:` said about that axis (the other axis stays); its margins
    /// are kept like any surface's (owner-testing.md gap 26): unset, the
    /// box is flush with the edge, and a negative top margin takes it
    /// over a bar's exclusive zone to the screen's edge.
    #[test]
    fn an_attached_panel_is_anchored_to_its_edge_and_keeps_its_margin() {
        let s = spec(
            NodeKind::Panel,
            &[
                (Prop::Anchor, kw("bottom_right")),
                (Prop::Attach, kw("top")),
                (Prop::Width, PropValue::Number(400.0)),
                (Prop::Height, PropValue::Number(300.0)),
            ],
        );
        let c = layer_config(&s).unwrap();
        assert_eq!(c.anchors, Anchors::new(true, false, false, true));
        assert_eq!(c.margin, [0, 0, 0, 0], "unset: flush");
        let mut set = s.clone();
        set.margin = Insets::all(8.0);
        set.margin_set = true;
        assert_eq!(layer_config(&set).unwrap().margin, [8, 8, 8, 8], "kept");
        let mut over = s.clone();
        over.margin = Insets {
            top: -36.0,
            ..Insets::default()
        };
        over.margin_set = true;
        assert_eq!(layer_config(&over).unwrap().margin, [-36, 0, 0, 0]);
        // The fillets' overhang moves the side margins out; the box stays.
        let mut s2 = set.clone();
        s2.overhang = Insets {
            top: 0.0,
            right: 16.0,
            bottom: 0.0,
            left: 16.0,
        };
        let c = layer_config(&s2).unwrap();
        assert_eq!(c.margin, [8, -8, 8, -8]);
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
    /// opens away from it, even away from a bar's usual side, at its
    /// margin on that side like any popup but 0 while that is unset
    /// (owner-testing.md gap 26): `margin: -36, 0, 0` on a popup of a
    /// 36 px top bar covers the bar, its fillets at the screen's edge.
    #[test]
    fn an_attached_popup_touches_its_anchor_at_its_margin() {
        use crate::{LogicalRect, NodeId};
        let bar = spec(NodeKind::Bar, &[(Prop::Height, PropValue::Number(36.0))]);
        let mut p = spec(NodeKind::Popup, &[(Prop::Attach, kw("top"))]);
        p.parent = Some(NodeId::new(1, 0));
        p.width = Some(200.0);
        p.height = Some(120.0);
        p.anchor_rect = Some(LogicalRect::new(100.0, 8.0, 60.0, 20.0));
        let c = popup_config(&p, &bar).unwrap();
        assert_eq!((c.side, c.gap), (PopupSide::Below, 0), "unset: touching");
        for (edge, side) in [
            ("bottom", PopupSide::Above),
            ("left", PopupSide::Right),
            ("right", PopupSide::Left),
        ] {
            let mut q = p.clone();
            q.attach = Edge::from_name(edge);
            let c = popup_config(&q, &bar).unwrap();
            assert_eq!((c.side, c.gap), (side, 0), "{edge}");
        }
        // A set margin is the gap, on the side facing the anchor.
        let mut set = p.clone();
        set.margin = Insets::all(10.0);
        set.margin_set = true;
        assert_eq!(popup_config(&set, &bar).unwrap().gap, 10);
        // Over the bar: from the bar's far edge back across it, so the
        // popup's top is the bar's top, the screen's edge.
        let mut over = p.clone();
        over.margin = Insets {
            top: -36.0,
            ..Insets::default()
        };
        over.margin_set = true;
        let c = popup_config(&over, &bar).unwrap();
        assert_eq!(
            (c.side, c.gap, c.anchor_rect),
            (PopupSide::Below, -36, (100, 0, 60, 36))
        );
        assert_eq!(popup_request(&c, c.anchor_rect, (0, 0)), (30, 0, 200, 120));
        // `attach: bottom` on a bottom bar's popup, the same way up.
        let mut bottom_bar = bar.clone();
        bottom_bar.edge = Some(Edge::Bottom);
        let mut up = p.clone();
        up.attach = Some(Edge::Bottom);
        up.margin = Insets {
            bottom: -36.0,
            ..Insets::default()
        };
        up.margin_set = true;
        let c = popup_config(&up, &bottom_bar).unwrap();
        assert_eq!((c.side, c.gap), (PopupSide::Above, -36));
        assert_eq!(popup_request(&c, c.anchor_rect, (0, 0)).1 + 120, 36);
    }

    /// A `top` panel with a scrim rises to `overlay`, so its scrim fits on
    /// `top` beneath it and above the windows; other layers keep theirs,
    /// and the scrim's layer is the one below.
    #[test]
    fn a_top_panel_with_a_scrim_rises_to_overlay() {
        let dim = PropValue::Color(crate::Color::new(0.0, 0.0, 0.0, 0.3));
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

    /// (window) A window's toplevel state: its title, app id, the box it
    /// asks for, its size limits and overhang; it waits for a size, and
    /// a popup in it is placed in its window geometry (its box).
    #[test]
    fn a_window_config_names_and_sizes_it() {
        let mut spec = SurfaceSpec::resolve(NodeKind::Window, |_| None::<&crate::PropValue>);
        spec.name = Some("Settings".into());
        assert_eq!(
            window_config(&spec),
            Err(PlacementError::AutoSize(NodeKind::Window))
        );
        spec.width = Some(400.4);
        spec.height = Some(300.0);
        spec.min_size = (Some(200.0), None);
        spec.max_size = (None, Some(900.0));
        spec.overhang = crate::Insets::from_values(&[4.0, 8.0, 12.0, 8.0]).unwrap();
        let c = window_config(&spec).unwrap();
        assert_eq!(c.title, "Settings");
        assert_eq!(c.app_id, "strand-Settings");
        assert_eq!(c.namespace, "strand-Settings");
        assert_eq!((c.width, c.height), (400, 300));
        assert_eq!((c.min, c.max), ((200, 0), (0, 900)));
        assert_eq!(c.overhang, [4, 8, 12, 8]);
        assert_eq!(c.buffer((640, 480)), (656, 496));
        // A minimum over the maximum raises the maximum to it (the
        // compositor would end the connection on it).
        let mut bad = spec.clone();
        bad.min_size = (Some(500.0), Some(100.0));
        bad.max_size = (Some(400.0), Some(900.0));
        assert_eq!(window_config(&bad).unwrap().max, (500, 900));
        assert_eq!(
            c.as_layer().input_region((416, 316)),
            Some(Some((8, 4, 400, 300)))
        );
        assert_eq!(
            window_config(&SurfaceSpec::resolve(NodeKind::Panel, |_| None::<
                &crate::PropValue,
            >)),
            Err(PlacementError::NotLayerSurface(NodeKind::Panel))
        );
        // A popup in it: anchored in its box, as in a popup's.
        let mut popup = SurfaceSpec::resolve(NodeKind::Popup, |_| None::<&crate::PropValue>);
        popup.parent = None;
        popup.width = Some(100.0);
        popup.height = Some(50.0);
        popup.anchor_rect = Some(crate::LogicalRect::new(28.0, 24.0, 40.0, 20.0));
        let p = popup_config(&popup, &spec).unwrap();
        assert_eq!((p.anchor_rect.0, p.anchor_rect.1), (20, 20));
    }
}
