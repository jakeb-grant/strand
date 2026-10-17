//! (eval-sandbox) The flattener's side of the frame trace
//! (`crate::probe`): one [`NodeTrace`] per node drawn or found hidden,
//! built from the locals `Flattener::node` already has (nothing is
//! sampled twice: a spring's value is the one the frame draws).

use strand_scene::{LogicalRect, NodeId, NodeKind, Prop, Scale};
use vello_cpu::kurbo;

use super::Flattener;
use super::text::{Shaped, pick};
use crate::probe::{Axes, Hidden, Motions, NodeTrace, OffsetParts, TextDrawn};

/// What `Flattener::node` knows of a node's motion once its springs,
/// `play`, lean, jelly and glide are sampled.
pub(super) struct Seen {
    pub node: NodeId,
    pub parent: Option<NodeId>,
    pub kind: NodeKind,
    pub laid: LogicalRect,
    pub drawn: LogicalRect,
    /// `Animator::motions` before `Animator::paint` (it consumes the
    /// enter) and after the glide's sample.
    pub before: (Motions, Axes),
    pub after: (Motions, Axes),
    pub written: (f32, f32),
    pub sprung: (f32, f32),
    pub written_opacity: f32,
    pub sprung_opacity: f32,
    /// Axes a playing `play` sets.
    pub plays: Axes,
    pub playing: bool,
    pub shared: Option<[f32; 4]>,
    pub tilt: bool,
    pub jelly: bool,
    pub raw_glide: (f32, f32),
    pub lifted: bool,
    pub reveal: bool,
    pub inert: bool,
    /// Axes whose props follow time.
    pub follows: Axes,
    pub inherited: (f32, f32),
    pub text: Option<TextDrawn>,
}

impl Seen {
    /// The node's motions and the axes authored motion moves.
    fn classify(&self) -> (Motions, Axes) {
        let mut m = Motions::NONE;
        let mut a = Axes::NONE;
        let (before, _) = self.before;
        let (after, springing) = self.after;
        if before.contains(Motions::ENTER) {
            m |= Motions::ENTER;
        }
        m |= after;
        // Its own prop springs (an enter or exit pose's included).
        if self.sprung.0 != self.written.0 || springing.contains(Axes::X) {
            m |= Motions::PROP_SPRING;
            a |= Axes::X;
        }
        if self.sprung.1 != self.written.1 || springing.contains(Axes::Y) {
            m |= Motions::PROP_SPRING;
            a |= Axes::Y;
        }
        if self.sprung_opacity != self.written_opacity || springing.contains(Axes::OPACITY) {
            m |= Motions::OPACITY_SPRING;
            a |= Axes::OPACITY;
        }
        a |= Axes(springing.0 & (Axes::SCALE.0 | Axes::ROTATE.0));
        // Its own size spring moves its drawn box.
        if after.contains(Motions::SIZE_SPRING) {
            a |= Axes(springing.0 & (Axes::W.0 | Axes::H.0));
        }
        if self.playing {
            m |= Motions::KEYFRAMES;
            a |= self.plays;
        }
        if self.shared.is_some() {
            m |= Motions::SHARED_MORPH;
            a |= Axes::GEOMETRY;
        }
        if self.tilt {
            m |= Motions::LEAN;
            a |= Axes::GEOMETRY;
        }
        if self.jelly {
            m |= Motions::JELLY;
            a |= Axes::GEOMETRY;
        }
        // A drag follows the pointer: authored. A glide is derived.
        if self.lifted {
            m |= Motions::LIFT;
            a |= Axes::X | Axes::Y;
        } else if self.raw_glide != (0.0, 0.0) {
            m |= Motions::GLIDE;
        }
        if self.reveal {
            m |= Motions::REVEAL;
        }
        if self.inert {
            m |= Motions::EXIT;
        }
        if !self.follows.is_empty() {
            m |= Motions::TIME;
            a |= self.follows;
        }
        (m, a)
    }

    /// The trace of the node, its look still unset.
    pub(super) fn trace(&self) -> NodeTrace {
        let (motions, authored) = self.classify();
        let spring = (
            self.sprung.0 - self.written.0,
            self.sprung.1 - self.written.1,
        );
        let morph = self.shared.map_or([0.0, 0.0], |m| [m[0], m[1]]);
        NodeTrace {
            node: self.node,
            parent: self.parent,
            kind: self.kind,
            laid: self.laid,
            drawn: self.drawn,
            visual: self.drawn,
            opacity: 1.0,
            scale: [1.0, 1.0],
            rotate: 0.0,
            parts: OffsetParts {
                inherited: [self.inherited.0, self.inherited.1],
                written: [self.written.0, self.written.1],
                spring: [spring.0, spring.1],
                glide: [self.raw_glide.0, self.raw_glide.1],
                morph,
            },
            motions,
            authored,
            hidden: None,
            text: self.text,
        }
    }
}

/// The axes of the props that place a node and that `on` holds for.
pub(super) fn axes(on: &dyn Fn(Prop) -> bool) -> Axes {
    [
        (Prop::X, Axes::X),
        (Prop::Y, Axes::Y),
        (Prop::Width, Axes::W),
        (Prop::Height, Axes::H),
        (Prop::Size, Axes::W | Axes::H),
        (Prop::Opacity, Axes::OPACITY),
        (Prop::Scale, Axes::SCALE),
        (Prop::Rotate, Axes::ROTATE),
    ]
    .into_iter()
    .filter(|(p, _)| on(*p))
    .fold(Axes::NONE, |a, (_, b)| a | b)
}

/// What a text drew from: `shaped` is its delivered layouts, `fit` the
/// line box it asked for (`None`: unbounded).
pub(super) fn text_drawn(shaped: &[Shaped], scale: Scale, fit: Option<f32>) -> TextDrawn {
    if shaped.is_empty() {
        TextDrawn::Estimate
    } else if pick(shaped, scale, fit).is_some() {
        TextDrawn::Shaped
    } else {
        TextDrawn::StandIn
    }
}

impl Flattener<'_> {
    /// Adds `t` to the frame's trace with its look this frame: `frame`
    /// is its drawn box in physical pixels and `xform` the transform it
    /// is drawn under.
    pub(super) fn probe_push(
        &mut self,
        mut t: NodeTrace,
        look: (f32, [f32; 2], f32),
        frame: kurbo::Rect,
        xform: kurbo::Affine,
        hidden: Option<Hidden>,
    ) {
        let Some(trace) = self.trace.as_mut() else {
            return;
        };
        let s = self.scale.as_f64();
        let v = xform.transform_rect_bbox(frame);
        t.visual = LogicalRect::new(
            (v.x0 / s) as f32,
            (v.y0 / s) as f32,
            (v.width() / s) as f32,
            (v.height() / s) as f32,
        );
        (t.opacity, t.scale, t.rotate) = look;
        t.hidden = hidden;
        trace.push(t);
    }
}
