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

/// Logical pixels as the integer the protocol takes: rounded, clamped to
/// what `i32` holds.
fn px(v: f32) -> i32 {
    if v.is_finite() {
        v.round().clamp(i32::MIN as f32, i32::MAX as f32) as i32
    } else {
        0
    }
}

fn size(v: f32) -> u32 {
    px(v).max(1) as u32
}

/// Resolves the layer-surface state for `spec`.
pub fn layer_config(spec: &SurfaceSpec) -> Result<LayerConfig, PlacementError> {
    let layer = spec
        .layer
        .ok_or(PlacementError::NotLayerSurface(spec.kind))?;
    let m = spec.margin;
    let o = spec.overhang;
    let overhang = [o.top, o.right, o.bottom, o.left].map(|v| px(v).max(0));
    let [ot, or, ob, ol] = overhang;
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
        let anchors = match spec.anchor {
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
        (
            anchors,
            size(w) + (ol + or) as u32,
            size(h) + (ot + ob) as u32,
            0,
        )
    };
    Ok(LayerConfig {
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
}
