//! (M4) Compositor-animated poses (design.md, "Compositor-animated
//! poses"): a surface root's `opacity`, `scale` and `x`/`y`, its `enter`
//! and `exit` poses included, applied by the compositor to the whole
//! surface instead of repainted. Opacity goes through
//! `wp_alpha_modifier_v1`, scale through the viewporter's destination
//! size and the offset through layer-shell margins; render paints the
//! content at rest meanwhile, so a fade or a slide costs a bare commit a
//! frame rather than a raster.
//!
//! Only what the compositor can apply exactly is delegated
//! ([`PoseMask::of`]):
//!
//! - opacity on every surface but a `lock`;
//! - an offset on an axis a `panel` or `osd` is anchored to on one side
//!   only: a margin moves the surface only from an anchored edge, a
//!   centred or stretched axis ignores it, and a `bar`'s margin is part of
//!   its exclusive zone (moving it would reflow every window);
//! - scale on a `panel` or `osd` anchored on one side of both axes. The
//!   compositor arranges a layer surface by its requested size and draws
//!   the surface from that box's top-left corner (wlroots, smithay and
//!   Hyprland alike), so a smaller destination shrinks towards the
//!   top-left; render folds the move that keeps the box's centre in place
//!   into the offset, which needs both margins.
//!
//! A `popup` delegates only opacity: its place comes from the positioner,
//! so its x, y and scale always repaint. What a surface cannot delegate is
//! painted as before, so a pose may be half delegated (a popup's `popin`
//! fades by the compositor and scales in its buffer); the two commute.
//!
//! While a surface is presented by the GPU, poses are painted into its
//! frames (architecture.md, "Surface hand-off"): the host turns
//! delegation off for it.

use std::borrow::Cow;

use strand_scene::{
    Anchor, Edge, Length, LogicalPoint, LogicalRect, NodeKind, Prop, PropValue, SurfacePose,
};

/// The smallest scale handed to the compositor: a destination size must
/// be at least one pixel, and a root scaled to nothing is drawn as
/// nothing (opacity 0) at this size.
pub(crate) const MIN_DELEGATED_SCALE: f32 = 0.01;

/// What parts of a surface root's pose the compositor applies for a
/// surface of a kind and placement (see the module docs).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PoseMask {
    pub(crate) opacity: bool,
    pub(crate) scale: bool,
    pub(crate) x: bool,
    pub(crate) y: bool,
}

impl PoseMask {
    /// The mask of a `kind` surface placed by `anchor` (and, on a panel,
    /// `attach`, which forces the anchor to its edge as placement does).
    pub(crate) fn of(kind: NodeKind, anchor: Anchor, attach: Option<Edge>) -> Self {
        match kind {
            NodeKind::Popup | NodeKind::Bar => Self {
                opacity: true,
                ..Self::default()
            },
            NodeKind::Panel | NodeKind::Osd => {
                // (top, bottom, left, right), as strand-surface's
                // placement anchors them.
                let (mut t, mut b, mut l, mut r) = match anchor {
                    Anchor::Center => (false, false, false, false),
                    Anchor::Top => (true, false, false, false),
                    Anchor::Bottom => (false, true, false, false),
                    Anchor::Left => (false, false, true, false),
                    Anchor::Right => (false, false, false, true),
                    Anchor::TopLeft => (true, false, true, false),
                    Anchor::TopRight => (true, false, false, true),
                    Anchor::BottomLeft => (false, true, true, false),
                    Anchor::BottomRight => (false, true, false, true),
                };
                match attach.filter(|_| kind == NodeKind::Panel) {
                    Some(Edge::Top) => (t, b) = (true, false),
                    Some(Edge::Bottom) => (t, b) = (false, true),
                    Some(Edge::Left) => (l, r) = (true, false),
                    Some(Edge::Right) => (l, r) = (false, true),
                    None => {}
                }
                let (x, y) = (l != r, t != b);
                Self {
                    opacity: true,
                    scale: x && y,
                    x,
                    y,
                }
            }
            _ => Self::default(),
        }
    }

    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Takes the parts of a surface root's pose the compositor can apply out
/// of `props` (its resolved, sprung props this frame), so the root paints
/// at rest there, and returns the pose to delegate. `frame` is the root's
/// box in the surface, logical pixels (inside the shadow overhang).
/// `None` when the surface delegates nothing (a `lock`).
pub(crate) fn delegate(
    kind: NodeKind,
    props: &mut Vec<(Prop, Cow<'_, PropValue>)>,
    frame: LogicalRect,
) -> Option<SurfacePose> {
    let get = |p: Prop| props.iter().find(|(q, _)| *q == p).map(|(_, v)| v.as_ref());
    let keyword = |p: Prop| match get(p) {
        Some(PropValue::Keyword(k) | PropValue::Text(k)) => Some(k.as_str()),
        _ => None,
    };
    let anchor = keyword(Prop::Anchor)
        .and_then(Anchor::from_name)
        .unwrap_or(Anchor::Center);
    let attach = keyword(Prop::Attach).and_then(Edge::from_name);
    let mask = PoseMask::of(kind, anchor, attach);
    if mask.is_empty() {
        return None;
    }
    let number = |v: Option<&PropValue>| match v {
        Some(PropValue::Number(n)) if n.is_finite() => Some(*n),
        _ => None,
    };
    let length = |v: Option<&PropValue>, reference: f32| match v {
        Some(PropValue::Number(n) | PropValue::Length(Length::Px(n))) if n.is_finite() => Some(*n),
        Some(PropValue::Length(Length::Percent(p))) if p.is_finite() => Some(reference * p / 100.0),
        _ => None,
    };
    let mut pose = SurfacePose::IDENTITY;
    let mut taken = Vec::with_capacity(4);
    if mask.opacity
        && let Some(o) = number(get(Prop::Opacity))
    {
        pose.opacity = o.clamp(0.0, 1.0);
        taken.push(Prop::Opacity);
    }
    if mask.scale
        && let Some(s) = number(get(Prop::Scale))
    {
        if s < MIN_DELEGATED_SCALE {
            pose.opacity = 0.0;
        }
        pose.scale = s.clamp(MIN_DELEGATED_SCALE, 1000.0);
        taken.push(Prop::Scale);
    }
    let mut offset = (0.0, 0.0);
    if mask.x
        && let Some(x) = length(get(Prop::X), frame.w)
    {
        offset.0 = x;
        taken.push(Prop::X);
    }
    if mask.y
        && let Some(y) = length(get(Prop::Y), frame.h)
    {
        offset.1 = y;
        taken.push(Prop::Y);
    }
    // Render scales about the box's centre; the compositor keeps the
    // surface's top-left corner: the corner moves by what keeps the
    // centre in place.
    let k = 1.0 - pose.scale;
    let (cx, cy) = (frame.x + frame.w / 2.0, frame.y + frame.h / 2.0);
    pose.offset = LogicalPoint::new(offset.0 + k * cx, offset.1 + k * cy);
    props.retain(|(p, _)| !taken.contains(p));
    Some(pose)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(v: f32) -> Cow<'static, PropValue> {
        Cow::Owned(PropValue::Number(v))
    }

    fn kw(k: &str) -> Cow<'static, PropValue> {
        Cow::Owned(PropValue::Keyword(k.into()))
    }

    #[test]
    fn masks_follow_kind_and_anchoring() {
        let only_opacity = PoseMask {
            opacity: true,
            ..PoseMask::default()
        };
        assert_eq!(
            PoseMask::of(NodeKind::Popup, Anchor::TopRight, None),
            only_opacity
        );
        assert_eq!(
            PoseMask::of(NodeKind::Bar, Anchor::TopLeft, None),
            only_opacity
        );
        assert_eq!(
            PoseMask::of(NodeKind::Lock, Anchor::Center, None),
            PoseMask::default()
        );
        // A centred panel: margins and scale cannot be applied exactly.
        assert_eq!(
            PoseMask::of(NodeKind::Panel, Anchor::Center, None),
            only_opacity
        );
        // Anchored top: y moves by its top margin, x is centred.
        let top = PoseMask::of(NodeKind::Osd, Anchor::Top, None);
        assert!(top.y && !top.x && !top.scale);
        let corner = PoseMask::of(NodeKind::Panel, Anchor::TopRight, None);
        assert!(corner.x && corner.y && corner.scale && corner.opacity);
        // `attach: left` anchors a centred panel to the left edge.
        let attached = PoseMask::of(NodeKind::Panel, Anchor::Center, Some(Edge::Left));
        assert!(attached.x && !attached.y && !attached.scale);
        // Only a panel takes `attach`.
        assert_eq!(
            PoseMask::of(NodeKind::Osd, Anchor::Center, Some(Edge::Left)),
            only_opacity
        );
    }

    #[test]
    fn a_corner_panel_hands_over_its_whole_pose() {
        let mut props = vec![
            (Prop::Anchor, kw("top_right")),
            (Prop::Opacity, n(0.5)),
            (Prop::Scale, n(0.8)),
            (Prop::X, n(20.0)),
            (
                Prop::Y,
                Cow::Owned(PropValue::Length(Length::Percent(10.0))),
            ),
            (Prop::Bg, kw("red")),
        ];
        // A 200 × 100 box 10 px inside its shadow.
        let frame = LogicalRect::new(10.0, 10.0, 200.0, 100.0);
        let pose = delegate(NodeKind::Panel, &mut props, frame).unwrap();
        assert_eq!(pose.opacity, 0.5);
        assert_eq!(pose.scale, 0.8);
        // The offset plus the corner's move that keeps the centre
        // (110, 60) in place at 0.8: 0.2 × centre.
        assert!((pose.offset.x - (20.0 + 22.0)).abs() < 1e-4, "{pose:?}");
        assert!((pose.offset.y - (10.0 + 12.0)).abs() < 1e-4, "{pose:?}");
        let left: Vec<Prop> = props.iter().map(|(p, _)| *p).collect();
        assert_eq!(left, vec![Prop::Anchor, Prop::Bg]);
    }

    #[test]
    fn a_popup_hands_over_only_its_opacity() {
        let mut props = vec![
            (Prop::Opacity, n(0.25)),
            (Prop::Scale, n(0.8)),
            (Prop::Y, n(8.0)),
        ];
        let pose = delegate(
            NodeKind::Popup,
            &mut props,
            LogicalRect::new(0.0, 0.0, 100.0, 50.0),
        )
        .unwrap();
        assert_eq!(
            pose,
            SurfacePose {
                opacity: 0.25,
                ..SurfacePose::IDENTITY
            }
        );
        let left: Vec<Prop> = props.iter().map(|(p, _)| *p).collect();
        assert_eq!(left, vec![Prop::Scale, Prop::Y]);
        // A lock delegates nothing and keeps everything.
        assert_eq!(
            delegate(
                NodeKind::Lock,
                &mut props,
                LogicalRect::new(0.0, 0.0, 1.0, 1.0)
            ),
            None
        );
        assert_eq!(props.len(), 2);
    }

    #[test]
    fn a_root_scaled_to_nothing_is_transparent() {
        let mut props = vec![(Prop::Anchor, kw("bottom_left")), (Prop::Scale, n(0.0))];
        let pose = delegate(
            NodeKind::Panel,
            &mut props,
            LogicalRect::new(0.0, 0.0, 100.0, 100.0),
        )
        .unwrap();
        assert_eq!(pose.opacity, 0.0);
        assert_eq!(pose.scale, MIN_DELEGATED_SCALE);
    }
}
