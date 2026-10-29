//! What the surface manager needs to create a Wayland surface for a
//! surface-kind node, resolved from that node's props and tokens.
//!
//! The renderer owns the tree, so it resolves these (through the node's
//! [`crate::TokenScope`]) and reports changes; `strand-surface` creates,
//! reconfigures or recreates layer surfaces from them. See
//! `docs/architecture.md`, "Render loop".

use crate::color::Color;
use crate::geometry::LogicalRect;
use crate::id::NodeId;
use crate::protocol::{Insets, Length, NodeKind, Paint, Prop, PropValue, named_enum};

named_enum! {
    /// A screen edge: `edge: top` on a bar, `attach: top` on a panel.
    pub enum Edge {
        Top = "top",
        Bottom = "bottom",
        Left = "left",
        Right = "right",
    }
}

named_enum! {
    /// Where a non-bar surface sits on its output (`anchor: top_right`).
    pub enum Anchor {
        Center = "center",
        Top = "top",
        Bottom = "bottom",
        Left = "left",
        Right = "right",
        TopLeft = "top_left",
        TopRight = "top_right",
        BottomLeft = "bottom_left",
        BottomRight = "bottom_right",
    }
}

named_enum! {
    /// (flex) The pointer's shape over a node (`cursor: grab`), as
    /// `wp_cursor_shape_v1` and the CSS cursor names have it.
    pub enum CursorShape {
        Default = "default",
        Pointer = "pointer",
        Text = "text",
        Crosshair = "crosshair",
        Grab = "grab",
        Grabbing = "grabbing",
        Move = "move",
        NotAllowed = "not_allowed",
        Wait = "wait",
        Progress = "progress",
        Help = "help",
        ContextMenu = "context_menu",
        Cell = "cell",
        Copy = "copy",
        Alias = "alias",
        NoDrop = "no_drop",
        AllScroll = "all_scroll",
        ZoomIn = "zoom_in",
        ZoomOut = "zoom_out",
        ColResize = "col_resize",
        RowResize = "row_resize",
        EwResize = "ew_resize",
        NsResize = "ns_resize",
        NeswResize = "nesw_resize",
        NwseResize = "nwse_resize",
        NResize = "n_resize",
        EResize = "e_resize",
        SResize = "s_resize",
        WResize = "w_resize",
        NeResize = "ne_resize",
        NwResize = "nw_resize",
        SeResize = "se_resize",
        SwResize = "sw_resize",
    }
}

named_enum! {
    /// The layer-shell layer (`layer: overlay`).
    pub enum Layer {
        Background = "background",
        Bottom = "bottom",
        Top = "top",
        Overlay = "overlay",
    }
}

named_enum! {
    /// Keyboard focus (`keyboard: none | on_demand | exclusive`).
    pub enum Keyboard {
        None = "none",
        OnDemand = "on_demand",
        Exclusive = "exclusive",
    }
}

named_enum! {
    /// (window) Who draws a `window`'s title bar and borders
    /// (`decorations: server | none`).
    pub enum Decorations {
        /// The compositor, where it offers to (`xdg-decoration`); the
        /// config draws its own where it does not (`self.decorated`
        /// false).
        Server = "server",
        /// The config: the compositor is asked to draw none.
        None = "none",
    }
}

/// (window) What a config asks of its own `window` (`w.maximize()`):
/// `xdg_toplevel` requests, which the compositor may refuse.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WindowAsk {
    /// `w.move()`: an interactive move, with the press still held.
    Move,
    /// `w.resize(edge)`: an interactive resize from that edge or corner,
    /// with the press still held. `Center` asks nothing.
    Resize(Anchor),
    /// `w.minimize()`.
    Minimize,
    /// `w.maximize()`: maximised, or restored when it is.
    Maximize,
    /// `w.fullscreen()`: fullscreen, or restored when it is.
    Fullscreen,
    /// `w.close()`: closed as the compositor's close button closes it
    /// (`open: <-> x` written false).
    Close,
}

impl WindowAsk {
    /// The ask a `Node` action names (`move`, `resize`, …), with the
    /// resize edge's name for `resize`; `None` for any other name.
    pub fn from_action(name: &str, edge: Option<&str>) -> Option<Self> {
        Some(match name {
            "move" => Self::Move,
            "resize" => Self::Resize(edge.and_then(Anchor::from_name)?),
            "minimize" => Self::Minimize,
            "maximize" => Self::Maximize,
            "fullscreen" => Self::Fullscreen,
            "close" => Self::Close,
            _ => return None,
        })
    }
}

/// (screens-focus) The [`Prop::Screens`] keyword the compiler gives an
/// instance whose `screen` is the focused monitor (a `screens:` that
/// evaluates to `focused`, a lock's single instance): [`Screens::Focused`]
/// with [`SurfaceSpec::follows_focus`]. Config text cannot write it.
pub const FOLLOW_FOCUS: &str = "follow_focus";

/// Which outputs a surface appears on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Screens {
    /// Every output (a bar's default).
    All,
    /// The focused output (`screens: focused`; the panel and OSD default).
    Focused,
    /// These outputs, by the identity `strand-surface` reports for them
    /// (make + model + description). Logic gives each per-monitor instance
    /// of a `bar` its own monitor this way.
    Named(Vec<String>),
}

/// Everything about a surface node that decides its Wayland surface.
/// Lengths are logical pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceSpec {
    pub kind: NodeKind,
    /// The declared name (`bar Top` → `Top`), from [`Prop::Name`].
    pub name: Option<String>,
    /// The edge a bar stretches along (default `top`); `None` for other
    /// kinds unless set.
    pub edge: Option<Edge>,
    /// Placement of a surface that does not stretch along an edge. A bar
    /// ignores it.
    pub anchor: Anchor,
    /// `None` for kinds that are not layer surfaces (`popup`, `lock`).
    pub layer: Option<Layer>,
    pub keyboard: Keyboard,
    /// `margin`, expanded like CSS (`$space.2, $space.2, 0`); zero
    /// while unset.
    pub margin: Insets,
    /// `margin` is set (`0` included): a popup's gap, its margin on the
    /// side facing its anchor, is 6 px only while it is not (spec.md
    /// 5.1).
    pub margin_set: bool,
    /// Requested width and height in logical pixels; `None` is auto (a
    /// bar stretches along its edge; otherwise layout decides, from M2).
    pub width: Option<f32>,
    pub height: Option<f32>,
    pub screens: Screens,
    /// (screens-focus) A [`Screens::Focused`] surface whose `screen` is
    /// the focused monitor ([`FOLLOW_FOCUS`]): the surface manager places
    /// it on the monitor it is told is focused and moves it with the
    /// focus. Without it (a `panel` or `osd` with no `screens:`, or
    /// `screens: focused` written), the compositor places it, once.
    pub follows_focus: bool,
    /// `open: …`; unset is open.
    pub open: bool,
    /// `attach: top`: concave fillets towards that edge (M4).
    pub attach: Option<Edge>,
    /// How far painting reaches past the surface's own box on each side
    /// (shadows), in logical pixels. Render fills it in from layout; the
    /// surface manager grows the buffer by it and shifts the margins, so
    /// the box stays where `margin` puts it, while the exclusive zone and
    /// the input region stay the box's (design.md: shadows enlarge the
    /// buffer but not the input region).
    pub overhang: Insets,
    /// `open` is bound two-way (`open: <-> x`, from [`Prop::TwoWay`]):
    /// Escape, a click away and focus loss write it `false`. A
    /// `keyboard: exclusive` layer surface with it gets a transparent
    /// catcher under it while open, so a click outside closes it.
    pub open_two_way: bool,
    /// A `popup`: the surface node it nests in (the root of the element
    /// it is declared in), whose Wayland surface is its parent. Filled
    /// in by render.
    pub parent: Option<NodeId>,
    /// A `popup`: the box of the element it is anchored to, in the parent
    /// surface's logical pixels (from its buffer's corner, shadow
    /// overhang included). `None` until that element is laid out; the
    /// popup waits for it. Filled in by render.
    pub anchor_rect: Option<LogicalRect>,
    /// A `popup` that is a tooltip (`tooltip: expr`): shown without a
    /// grab, takes no input.
    pub tooltip: bool,
    /// (flex) A `popup` with `anchor: pointer`: it opens where the press
    /// that opened it went down (render gives a 1 × 1 `anchor_rect`
    /// there), down and to the right of it, kept inside its output as
    /// any popup is.
    pub at_pointer: bool,
    /// (M6) A `trigger: hover` popup: the router opens and closes it by
    /// the pointer, and it is shown without a grab (it takes input).
    pub hover_trigger: bool,
    /// (M4) `scrim: $shadow.alpha(0.3)`: a full-output single-pixel
    /// surface in this colour under the surface while it is open (the
    /// click-away catcher too, when it has one). Only a `popup` or a
    /// `panel` has one (the compiler rejects it elsewhere).
    pub scrim: Option<Color>,
    /// (services-round) `inhibit_idle: true`: while it is shown the
    /// compositor does not go idle (no screensaver, dimming or lock by
    /// timeout): a `zwp_idle_inhibitor_v1` on each of its Wayland
    /// surfaces.
    pub inhibit_idle: bool,
    /// (flex) `exclusive:` on a bar or panel: the space it keeps from
    /// windows.
    pub exclusive: Exclusive,
    /// (window) A `window`'s `title:` (`None`: its name).
    pub title: Option<String>,
    /// (window) A `window`'s `app_id:` (`None`: its namespace,
    /// `strand-<Name>`).
    pub app_id: Option<String>,
    /// (window) A `window`'s `min_width`/`min_height` and
    /// `max_width`/`max_height` in logical pixels: the sizes the
    /// compositor may give it (`xdg_toplevel.set_min_size` and
    /// `set_max_size`). `None` for other kinds.
    pub min_size: (Option<f32>, Option<f32>),
    pub max_size: (Option<f32>, Option<f32>),
    /// (window) `decorations:`.
    pub decorations: Decorations,
}

/// (flex) What a surface keeps from windows (`exclusive:`).
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Exclusive {
    /// `true`: its thickness across its edge (a bar's default).
    Auto,
    /// `false` or `0`: nothing; windows go under it (every other
    /// surface's default).
    None,
    /// A length: exactly that much from its edge, whatever its size.
    Length(f32),
}

/// (M4) Which optional protocols the compositor offered, reported once
/// the surface manager has bound its globals
/// (`SurfaceHost::compositor_caps`); the host hands render what it uses
/// (`set_compositor_blur`, `set_compositor_poses`).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct CompositorCaps {
    /// `wp_alpha_modifier_v1`: delegated opacity poses.
    pub alpha_modifier: bool,
    /// `wp_viewporter`: delegated scale poses, single-pixel surfaces.
    pub viewporter: bool,
    /// `wp_single_pixel_buffer_v1`: scrims and lock backgrounds without
    /// shm.
    pub single_pixel_buffer: bool,
    /// `ext_background_effect_manager_v1`: the blur ladder's first rung.
    pub background_effect: bool,
    /// `ext_session_lock_manager_v1`: the lock screen.
    pub session_lock: bool,
    /// `wl_data_device_manager`: drag and drop with other programs.
    pub data_device: bool,
    /// (M4) Hyprland's own globals (`hyprland_*`) are in the registry.
    /// Hyprland blurs layer surfaces by its layer rules (`strand
    /// compositor-rules`), and draws a layer surface stretched to the box
    /// it arranged whatever its viewport, so a root's scale is painted.
    pub hyprland: bool,
}

impl CompositorCaps {
    /// True when a root's opacity, scale and x/y poses can be delegated:
    /// the alpha modifier and the viewporter are both there (x/y uses
    /// layer-shell margins, which every compositor has).
    pub fn delegates_poses(&self) -> bool {
        self.alpha_modifier && self.viewporter
    }
}

/// True if a [`Prop::TwoWay`] value names `prop`.
pub fn is_two_way(two_way: Option<&PropValue>, prop: Prop) -> bool {
    match two_way {
        Some(PropValue::List(items)) => items
            .iter()
            .any(|v| matches!(v, PropValue::Keyword(k) if k == prop.name())),
        _ => false,
    }
}

impl SurfaceSpec {
    /// Resolves the spec of a `kind` node from its props, read through
    /// `get` (which must already have resolved token references). Unset,
    /// mistyped or unknown values take the kind's defaults.
    pub fn resolve<V: AsRef<PropValue>>(kind: NodeKind, get: impl Fn(Prop) -> Option<V>) -> Self {
        let keyword = |p: Prop| match get(p).as_ref().map(AsRef::as_ref) {
            Some(PropValue::Keyword(k)) | Some(PropValue::Text(k)) => Some(k.clone()),
            _ => None,
        };
        let px = |p: Prop| match get(p).as_ref().map(AsRef::as_ref) {
            Some(PropValue::Number(n)) | Some(PropValue::Length(Length::Px(n))) => {
                Some(*n).filter(|n| n.is_finite() && *n >= 0.0)
            }
            _ => None,
        };
        let text = |p: Prop| match get(p).as_ref().map(AsRef::as_ref) {
            Some(PropValue::Text(t)) => Some(t.clone()),
            _ => None,
        };
        let window_px = |w: Prop, h: Prop| {
            if kind == NodeKind::Window {
                (px(w), px(h))
            } else {
                (None, None)
            }
        };
        let layer_shell = matches!(kind, NodeKind::Bar | NodeKind::Panel | NodeKind::Osd);
        let edge = keyword(Prop::Edge)
            .and_then(|k| Edge::from_name(&k))
            .or((kind == NodeKind::Bar).then_some(Edge::Top));
        let default_layer = match kind {
            NodeKind::Osd => Layer::Overlay,
            _ => Layer::Top,
        };
        let layer = layer_shell.then(|| {
            keyword(Prop::Layer)
                .and_then(|k| Layer::from_name(&k))
                .unwrap_or(default_layer)
        });
        let screens_prop = get(Prop::Screens);
        let follows_focus = matches!(
            screens_prop.as_ref().map(AsRef::as_ref),
            Some(PropValue::Keyword(k)) if k == FOLLOW_FOCUS
        );
        let screens = match screens_prop.as_ref().map(AsRef::as_ref) {
            Some(PropValue::Keyword(k)) if k == "all" => Screens::All,
            Some(PropValue::Keyword(k)) if k == "focused" || k == FOLLOW_FOCUS => Screens::Focused,
            Some(PropValue::Text(t)) => Screens::Named(vec![t.clone()]),
            Some(PropValue::List(items)) => Screens::Named(
                items
                    .iter()
                    .filter_map(|v| match v {
                        PropValue::Text(t) => Some(t.clone()),
                        _ => None,
                    })
                    .collect(),
            ),
            _ if kind == NodeKind::Bar => Screens::All,
            _ => Screens::Focused,
        };
        let margin = get(Prop::Margin)
            .and_then(|v| v.as_ref().insets())
            .filter(|i| {
                [i.top, i.right, i.bottom, i.left]
                    .iter()
                    .all(|v| v.is_finite())
            });
        let margin_set = margin.is_some();
        let margin = margin.unwrap_or_default();
        let size = px(Prop::Size);
        Self {
            kind,
            name: match get(Prop::Name).as_ref().map(AsRef::as_ref) {
                Some(PropValue::Text(n)) if !n.is_empty() => Some(n.clone()),
                _ => None,
            },
            edge,
            anchor: keyword(Prop::Anchor)
                .and_then(|k| Anchor::from_name(&k))
                .unwrap_or(Anchor::Center),
            layer,
            keyboard: keyword(Prop::Keyboard)
                .and_then(|k| Keyboard::from_name(&k))
                .unwrap_or(Keyboard::None),
            margin,
            margin_set,
            width: px(Prop::Width).or(size),
            height: px(Prop::Height).or(size),
            screens,
            follows_focus,
            open: !matches!(
                get(Prop::Open).as_ref().map(AsRef::as_ref),
                Some(PropValue::Bool(false))
            ),
            attach: keyword(Prop::Attach).and_then(|k| Edge::from_name(&k)),
            overhang: Insets::default(),
            open_two_way: is_two_way(get(Prop::TwoWay).as_ref().map(AsRef::as_ref), Prop::Open),
            parent: None,
            anchor_rect: None,
            tooltip: false,
            at_pointer: kind == NodeKind::Popup
                && keyword(Prop::Anchor).is_some_and(|k| k == "pointer"),
            hover_trigger: kind == NodeKind::Popup
                && keyword(Prop::Trigger).is_some_and(|k| k == "hover"),
            scrim: match (kind, get(Prop::Scrim).as_ref().map(AsRef::as_ref)) {
                (
                    NodeKind::Popup | NodeKind::Panel,
                    Some(PropValue::Color(c) | PropValue::Paint(Paint::Solid(c))),
                ) => Some(*c),
                _ => None,
            },
            inhibit_idle: matches!(
                get(Prop::InhibitIdle).as_ref().map(AsRef::as_ref),
                Some(PropValue::Bool(true))
            ),
            title: (kind == NodeKind::Window)
                .then(|| text(Prop::Title))
                .flatten(),
            app_id: (kind == NodeKind::Window)
                .then(|| text(Prop::AppId))
                .flatten(),
            min_size: window_px(Prop::MinWidth, Prop::MinHeight),
            max_size: window_px(Prop::MaxWidth, Prop::MaxHeight),
            decorations: keyword(Prop::Decorations)
                .and_then(|k| Decorations::from_name(&k))
                .unwrap_or(Decorations::Server),
            exclusive: match (kind, get(Prop::Exclusive).as_ref().map(AsRef::as_ref)) {
                (NodeKind::Bar | NodeKind::Panel, Some(PropValue::Bool(true))) => Exclusive::Auto,
                (NodeKind::Bar | NodeKind::Panel, Some(PropValue::Bool(false))) => Exclusive::None,
                (
                    NodeKind::Bar | NodeKind::Panel,
                    Some(PropValue::Number(n) | PropValue::Length(Length::Px(n))),
                ) if n.is_finite() => {
                    if *n > 0.0 {
                        Exclusive::Length(*n)
                    } else {
                        Exclusive::None
                    }
                }
                (NodeKind::Bar, _) => Exclusive::Auto,
                _ => Exclusive::None,
            },
        }
    }

    /// The stable layer-shell namespace: `strand-<Name>`, or
    /// `strand-<kind>` for an unnamed surface. Compositor rules (blur,
    /// Hyprland layer rules) match on it.
    pub fn namespace(&self) -> String {
        format!(
            "strand-{}",
            self.name.as_deref().unwrap_or(self.kind.name())
        )
    }

    /// (window) The app id a `window` gives the compositor: its
    /// `app_id:`, else its namespace (`strand-<Name>`), so compositor
    /// rules match it as they match a layer surface.
    pub fn window_app_id(&self) -> String {
        self.app_id
            .clone()
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| self.namespace())
    }

    /// (window) The title a `window` gives the compositor: its `title:`,
    /// else its name.
    pub fn window_title(&self) -> String {
        self.title
            .clone()
            .or_else(|| self.name.clone())
            .unwrap_or_else(|| self.kind.name().to_string())
    }

    /// A bar's thickness across its edge, logical pixels (`None` while
    /// auto); 0 for other kinds.
    pub fn thickness(&self) -> Option<f32> {
        if self.kind != NodeKind::Bar {
            return Some(0.0);
        }
        match self.edge {
            Some(Edge::Left | Edge::Right) => self.width,
            _ => self.height,
        }
    }

    /// (flex) The edge a surface's exclusive zone reserves along: a
    /// bar's edge; a panel's `attach:` edge, else its anchor's when that
    /// is one edge (`top`, `left`, …). A panel at a corner or the centre
    /// has none.
    pub fn exclusive_edge(&self) -> Option<Edge> {
        match self.kind {
            NodeKind::Bar => Some(self.edge.unwrap_or(Edge::Top)),
            NodeKind::Panel => self.attach.or(match self.anchor {
                Anchor::Top => Some(Edge::Top),
                Anchor::Bottom => Some(Edge::Bottom),
                Anchor::Left => Some(Edge::Left),
                Anchor::Right => Some(Edge::Right),
                _ => None,
            }),
            _ => None,
        }
    }

    /// The exclusive zone in logical pixels: what `exclusive:` keeps
    /// from windows along [`Self::exclusive_edge`] (compositors add the
    /// edge's margin): by default a bar's thickness and nothing for other
    /// surfaces; `true` the box's thickness across that edge, a length
    /// exactly that, `false` or `0` nothing. `None` while a thickness it
    /// needs is auto.
    pub fn exclusive_zone(&self) -> Option<f32> {
        let Some(edge) = self.exclusive_edge() else {
            return Some(0.0);
        };
        match self.exclusive {
            Exclusive::None => Some(0.0),
            Exclusive::Length(n) => Some(n),
            Exclusive::Auto => match edge {
                Edge::Left | Edge::Right => self.width,
                Edge::Top | Edge::Bottom => self.height,
            },
        }
    }

    /// True if going from `self` to `new` needs the Wayland surface
    /// destroyed and created again: its kind, namespace or layer changed
    /// (design: "Surface layer, namespace or kind → only that surface is
    /// recreated"). Everything else is reconfigured in place.
    pub fn needs_recreate(&self, new: &SurfaceSpec) -> bool {
        self.kind != new.kind || self.namespace() != new.namespace() || self.layer != new.layer
    }
}

/// A change to the set of surfaces the tree asks for, reported by the
/// renderer after applying a diff (`Renderer::take_surface_changes`).
#[derive(Clone, Debug, PartialEq)]
pub enum SurfaceChange {
    /// A surface-kind node appeared: create its surface(s).
    Created(SurfaceSpec),
    /// Its spec changed. With `recreate` (kind, namespace or layer
    /// changed) destroy and create the Wayland surface; otherwise
    /// reconfigure it in place (anchor, margin, size, exclusive zone,
    /// keyboard, outputs, open).
    Updated { spec: SurfaceSpec, recreate: bool },
    /// The node is gone: destroy its surface(s) and detach them.
    Removed,
}

impl AsRef<PropValue> for PropValue {
    fn as_ref(&self) -> &PropValue {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn spec(kind: NodeKind, props: &[(Prop, PropValue)]) -> SurfaceSpec {
        let map: HashMap<Prop, PropValue> = props.iter().cloned().collect();
        SurfaceSpec::resolve(kind, |p| map.get(&p))
    }

    fn kw(k: &str) -> PropValue {
        PropValue::Keyword(k.into())
    }

    /// (services-round) `inhibit_idle` is off unless it is `true`, on any
    /// surface, and changes no Wayland surface's identity.
    #[test]
    fn inhibit_idle_resolves() {
        assert!(!spec(NodeKind::Bar, &[]).inhibit_idle);
        let on = spec(NodeKind::Bar, &[(Prop::InhibitIdle, PropValue::Bool(true))]);
        assert!(on.inhibit_idle);
        assert!(!spec(NodeKind::Bar, &[]).needs_recreate(&on));
        assert!(
            spec(
                NodeKind::Popup,
                &[(Prop::InhibitIdle, PropValue::Bool(true))]
            )
            .inhibit_idle
        );
        assert!(
            !spec(
                NodeKind::Osd,
                &[(Prop::InhibitIdle, PropValue::Bool(false))]
            )
            .inhibit_idle
        );
    }

    /// (window) A `window` is no layer surface: its title is its name
    /// and its app id its namespace until set, its size limits are its
    /// `min_*`/`max_*` in px, and it asks for server-side decorations
    /// unless `decorations: none`. A new title, app id or limit changes
    /// it in place; another name makes it again.
    #[test]
    fn a_window_resolves() {
        let w = spec(
            NodeKind::Window,
            &[(Prop::Name, PropValue::Text("Settings".into()))],
        );
        assert_eq!(w.layer, None);
        assert_eq!(w.window_title(), "Settings");
        assert_eq!(w.window_app_id(), "strand-Settings");
        assert_eq!((w.min_size, w.max_size), ((None, None), (None, None)));
        assert_eq!(w.decorations, Decorations::Server);
        assert_eq!(w.exclusive_zone(), Some(0.0));
        assert!(w.open);
        let set = spec(
            NodeKind::Window,
            &[
                (Prop::Name, PropValue::Text("Settings".into())),
                (Prop::Title, PropValue::Text("Strand settings".into())),
                (Prop::AppId, PropValue::Text("org.strand.Settings".into())),
                (Prop::MinWidth, PropValue::Number(320.0)),
                (Prop::MaxHeight, PropValue::Length(Length::Px(900.0))),
                (Prop::Decorations, kw("none")),
            ],
        );
        assert_eq!(set.window_title(), "Strand settings");
        assert_eq!(set.window_app_id(), "org.strand.Settings");
        assert_eq!(set.min_size, (Some(320.0), None));
        assert_eq!(set.max_size, (None, Some(900.0)));
        assert_eq!(set.decorations, Decorations::None);
        assert!(!w.needs_recreate(&set));
        let mut renamed = w.clone();
        renamed.name = Some("Other".into());
        assert!(w.needs_recreate(&renamed));
        // Size limits are a window's only: a panel's `min_width` lays
        // out its content.
        let panel = spec(
            NodeKind::Panel,
            &[(Prop::MinWidth, PropValue::Number(320.0))],
        );
        assert_eq!(panel.min_size, (None, None));
        assert_eq!(panel.title, None);
    }

    #[test]
    fn window_asks_name_the_node_actions() {
        assert_eq!(WindowAsk::from_action("move", None), Some(WindowAsk::Move));
        assert_eq!(
            WindowAsk::from_action("resize", Some("bottom_right")),
            Some(WindowAsk::Resize(Anchor::BottomRight))
        );
        assert_eq!(WindowAsk::from_action("resize", None), None);
        assert_eq!(
            WindowAsk::from_action("fullscreen", None),
            Some(WindowAsk::Fullscreen)
        );
        assert_eq!(WindowAsk::from_action("focus", None), None);
    }

    #[test]
    fn design_examples_resolve() {
        // bar Top { edge: top; height: 36; margin: 8, 8, 0 }
        let bar = spec(
            NodeKind::Bar,
            &[
                (Prop::Name, PropValue::Text("Top".into())),
                (Prop::Edge, kw("top")),
                (Prop::Height, PropValue::Number(36.0)),
                (
                    Prop::Margin,
                    PropValue::List(vec![
                        PropValue::Number(8.0),
                        PropValue::Number(8.0),
                        PropValue::Number(0.0),
                    ]),
                ),
            ],
        );
        assert_eq!(bar.namespace(), "strand-Top");
        assert_eq!(bar.layer, Some(Layer::Top));
        assert_eq!(bar.screens, Screens::All);
        assert_eq!(bar.exclusive_zone(), Some(36.0));
        assert_eq!(
            bar.margin,
            Insets {
                top: 8.0,
                right: 8.0,
                bottom: 0.0,
                left: 8.0
            }
        );
        assert!(bar.open);

        // panel Launcher { screens: focused; layer: overlay; anchor: center;
        //                  keyboard: exclusive; open: false }
        let launcher = spec(
            NodeKind::Panel,
            &[
                (Prop::Name, PropValue::Text("Launcher".into())),
                (Prop::Screens, kw("focused")),
                (Prop::Layer, kw("overlay")),
                (Prop::Anchor, kw("center")),
                (Prop::Keyboard, kw("exclusive")),
                (Prop::Open, PropValue::Bool(false)),
            ],
        );
        assert_eq!(launcher.keyboard, Keyboard::Exclusive);
        assert_eq!(launcher.layer, Some(Layer::Overlay));
        assert_eq!(launcher.edge, None);
        assert_eq!(launcher.exclusive_zone(), Some(0.0));
        assert!(!launcher.open);

        // osd Level { anchor: bottom; margin: 0, 0, 96 }: focused, overlay.
        let osd = spec(NodeKind::Osd, &[(Prop::Anchor, kw("bottom"))]);
        assert_eq!(osd.layer, Some(Layer::Overlay));
        assert_eq!(osd.screens, Screens::Focused);
        assert!(!osd.follows_focus, "the compositor places it");
        assert_eq!(osd.namespace(), "strand-osd");
        // (screens-focus) Written `focused` is the default's placement;
        // the compiler's FOLLOW_FOCUS follows the focus.
        let written = spec(NodeKind::Panel, &[(Prop::Screens, kw("focused"))]);
        assert_eq!(
            (written.screens, written.follows_focus),
            (Screens::Focused, false)
        );
        let follows = spec(NodeKind::Panel, &[(Prop::Screens, kw(FOLLOW_FOCUS))]);
        assert_eq!(
            (follows.screens, follows.follows_focus),
            (Screens::Focused, true)
        );

        // Popups are not layer surfaces.
        assert_eq!(spec(NodeKind::Popup, &[]).layer, None);
    }

    /// (m6-owner-fixes) An unset `margin` and `margin: 0` resolve to the
    /// same insets, but only the second is set: a popup's gap is 6 px
    /// only while its margin is unset (spec.md 5.1).
    #[test]
    fn a_zero_margin_is_set_and_an_absent_one_is_not() {
        let unset = spec(NodeKind::Popup, &[]);
        assert_eq!((unset.margin, unset.margin_set), (Insets::default(), false));
        let zero = spec(NodeKind::Popup, &[(Prop::Margin, PropValue::Number(0.0))]);
        assert_eq!((zero.margin, zero.margin_set), (Insets::default(), true));
        let negative = spec(
            NodeKind::Popup,
            &[(
                Prop::Margin,
                PropValue::List(vec![
                    PropValue::Number(-30.0),
                    PropValue::Number(0.0),
                    PropValue::Number(0.0),
                ]),
            )],
        );
        assert_eq!(negative.margin.top, -30.0);
        assert!(negative.margin_set);
    }

    #[test]
    fn only_kind_namespace_and_layer_recreate() {
        let a = spec(
            NodeKind::Bar,
            &[(Prop::Name, PropValue::Text("Top".into()))],
        );
        let mut b = a.clone();
        b.margin = Insets::all(4.0);
        b.edge = Some(Edge::Bottom);
        b.keyboard = Keyboard::OnDemand;
        assert!(!a.needs_recreate(&b));
        b.layer = Some(Layer::Overlay);
        assert!(a.needs_recreate(&b));
        let mut c = a.clone();
        c.name = Some("Bottom".into());
        assert!(a.needs_recreate(&c));
    }

    #[test]
    fn a_scrim_is_a_colour_on_popups_and_panels() {
        let dim = Color::BLACK.with_alpha(0.3);
        for kind in [NodeKind::Popup, NodeKind::Panel] {
            assert_eq!(
                spec(kind, &[(Prop::Scrim, PropValue::Color(dim))]).scrim,
                Some(dim)
            );
            assert_eq!(
                spec(kind, &[(Prop::Scrim, PropValue::Paint(Paint::Solid(dim)))]).scrim,
                Some(dim)
            );
            assert_eq!(spec(kind, &[]).scrim, None);
        }
        // A gradient has no single-pixel form; other kinds take none.
        let gradient = PropValue::Paint(Paint::Radial { stops: vec![] });
        assert_eq!(
            spec(NodeKind::Panel, &[(Prop::Scrim, gradient)]).scrim,
            None
        );
        for kind in [NodeKind::Bar, NodeKind::Osd, NodeKind::Lock] {
            assert_eq!(
                spec(kind, &[(Prop::Scrim, PropValue::Color(dim))]).scrim,
                None
            );
        }
        // A scrim change reconfigures in place.
        let a = spec(NodeKind::Panel, &[]);
        let b = spec(NodeKind::Panel, &[(Prop::Scrim, PropValue::Color(dim))]);
        assert!(!a.needs_recreate(&b));
    }

    #[test]
    fn compositor_caps() {
        let none = CompositorCaps::default();
        assert!(!none.delegates_poses());
        let poses = CompositorCaps {
            alpha_modifier: true,
            viewporter: true,
            ..CompositorCaps::default()
        };
        assert!(poses.delegates_poses());
        assert!(
            !CompositorCaps {
                alpha_modifier: true,
                ..none
            }
            .delegates_poses()
        );
    }

    #[test]
    fn bad_values_fall_back_to_defaults() {
        let s = spec(
            NodeKind::Bar,
            &[
                (Prop::Edge, kw("diagonal")),
                (Prop::Height, PropValue::Number(f32::NAN)),
                (
                    Prop::Margin,
                    PropValue::List(vec![PropValue::Number(1.0); 5]),
                ),
                (
                    Prop::Screens,
                    PropValue::List(vec![PropValue::Text("DP-1".into())]),
                ),
            ],
        );
        assert_eq!(s.edge, Some(Edge::Top));
        assert_eq!(s.height, None);
        assert_eq!(s.margin, Insets::default());
        assert!(!s.margin_set, "a margin it cannot use is unset");
        assert_eq!(s.screens, Screens::Named(vec!["DP-1".into()]));
    }
}
