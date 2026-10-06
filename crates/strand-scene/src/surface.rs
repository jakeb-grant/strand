//! What the surface manager needs to create a Wayland surface for a
//! surface-kind node, resolved from that node's props and tokens.
//!
//! The renderer owns the tree, so it resolves these (through the node's
//! [`crate::TokenScope`]) and reports changes; `strand-surface` creates,
//! reconfigures or recreates layer surfaces from them. See
//! `docs/architecture.md`, "Render loop".

use crate::protocol::{Insets, Length, NodeKind, Prop, PropValue, named_enum};

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
    /// `margin`, expanded like CSS (`$space.2, $space.2, 0`).
    pub margin: Insets,
    /// Requested width and height in logical pixels; `None` is auto (a
    /// bar stretches along its edge; otherwise layout decides, from M2).
    pub width: Option<f32>,
    pub height: Option<f32>,
    pub screens: Screens,
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
        let screens = match get(Prop::Screens).as_ref().map(AsRef::as_ref) {
            Some(PropValue::Keyword(k)) if k == "all" => Screens::All,
            Some(PropValue::Keyword(k)) if k == "focused" => Screens::Focused,
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
            })
            .unwrap_or_default();
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
            width: px(Prop::Width).or(size),
            height: px(Prop::Height).or(size),
            screens,
            open: !matches!(
                get(Prop::Open).as_ref().map(AsRef::as_ref),
                Some(PropValue::Bool(false))
            ),
            attach: keyword(Prop::Attach).and_then(|k| Edge::from_name(&k)),
            overhang: Insets::default(),
            open_two_way: is_two_way(get(Prop::TwoWay).as_ref().map(AsRef::as_ref), Prop::Open),
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

    /// The exclusive zone in logical pixels: a bar reserves its thickness
    /// across its edge (compositors add the edge's margin); other surfaces
    /// reserve nothing. `None` while a bar's thickness is auto.
    pub fn exclusive_zone(&self) -> Option<f32> {
        if self.kind != NodeKind::Bar {
            return Some(0.0);
        }
        match self.edge {
            Some(Edge::Left | Edge::Right) => self.width,
            _ => self.height,
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
        assert_eq!(osd.namespace(), "strand-osd");

        // Popups are not layer surfaces.
        assert_eq!(spec(NodeKind::Popup, &[]).layer, None);
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
        assert_eq!(s.screens, Screens::Named(vec!["DP-1".into()]));
    }
}
