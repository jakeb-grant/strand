//! (eval-sandbox) The flattener's side of the frame trace
//! (`crate::probe`): one [`NodeTrace`] per node drawn or found hidden,
//! built from the locals `Flattener::node` already has (nothing is
//! sampled twice: a spring's value is the one the frame draws).

use std::collections::HashMap;

use strand_scene::{LogicalRect, NodeId, NodeKind, Paint, Prop, Rect, Scale};
use strand_text::TextKey;
use vello_cpu::kurbo;

use super::text::{Shaped, pick};
use super::{DisplayItem, Flattener, Item};
use crate::probe::{
    Axes, GlyphsDrawn, Hidden, Motions, NodePaint, NodeTrace, OffsetParts, ProbeText, ShadowDrawn,
    TextDrawn,
};

/// What `Flattener::node` knows of a node's motion once its springs,
/// `play`, lean, jelly and glide are sampled.
pub(super) struct Seen {
    pub node: NodeId,
    pub parent: Option<NodeId>,
    pub kind: NodeKind,
    pub laid: LogicalRect,
    pub drawn: LogicalRect,
    /// `Animator::motions` once the springs and the glide are sampled.
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
    /// The line box a text asked to be shaped for, when cut.
    pub fit: Option<f32>,
}

impl Seen {
    /// The node's motions and the axes authored motion moves.
    fn classify(&self) -> (Motions, Axes) {
        let mut m = Motions::NONE;
        let mut a = Axes::NONE;
        let (after, springing) = self.after;
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
            fit: self.fit,
            ink: None,
            overhang: None,
            paint: NodePaint::default(),
            clip: None,
            roll: None,
            drag: None,
            images: Vec::new(),
            cut: None,
            ring: None,
            gap: None,
        }
    }
}

/// (eval-coverage) What a node painted itself: `items` are the display
/// items it added (its own, before its children's), on a surface `s`
/// device pixels a logical one; `texts` names the layouts' texts. Glyph
/// items from one text in one paint are one entry, the letters a roll or
/// `letters` draws one by one gathered back.
pub(super) fn paint(
    items: &[DisplayItem],
    s: f64,
    texts: &HashMap<TextKey, ProbeText>,
) -> NodePaint {
    let mut out = NodePaint::default();
    // Per entry of `out.glyphs`: the glyphs of its layout drawn so far,
    // and how many it has (unknown without its text).
    let mut drawn: Vec<(Vec<u32>, Option<usize>)> = Vec::new();
    let lg = |v: f64| (v / s) as f32;
    for i in items {
        match &i.item {
            Item::Fill { paint, shape, .. } => {
                out.fills.push(paint.clone());
                let b = match shape {
                    super::FillShape::Rect(r) => *r,
                    super::FillShape::Path(p) => kurbo::Shape::bounding_box(p),
                };
                out.fill_boxes.push(LogicalRect::new(
                    lg(b.x0),
                    lg(b.y0),
                    lg(b.width()),
                    lg(b.height()),
                ));
            }
            Item::Border { paint, .. } => out.borders.push(paint.clone()),
            Item::Shadow { color, extent, .. } => out.shadows.push(ShadowDrawn {
                color: *color,
                extent: LogicalRect::new(
                    lg(extent.x0),
                    lg(extent.y0),
                    lg(extent.width()),
                    lg(extent.height()),
                ),
            }),
            Item::Glyphs {
                layout,
                color,
                spans,
                fill,
                ..
            } => {
                let source = texts.get(&layout.key);
                let paint = fill
                    .as_ref()
                    .map_or(Paint::Solid(*color), |f| f.paint.clone());
                // Its glyphs' places in the layout it was shaped as.
                let at: Vec<u32> = layout
                    .runs
                    .iter()
                    .flat_map(|r| r.glyphs.iter())
                    .enumerate()
                    .map(|(n, g)| {
                        source
                            .and_then(|t| t.glyphs.iter().position(|p| *p == (g.x, g.y)))
                            .map_or(n as u32, |p| p as u32)
                    })
                    .collect();
                let text = source.map(|t| t.text.clone());
                let found = out
                    .glyphs
                    .iter()
                    .position(|g| g.text == text && g.paint == paint && g.spans == *spans);
                let n = match found {
                    Some(n) => n,
                    None => {
                        out.glyphs.push(GlyphsDrawn {
                            paint,
                            spans: spans.clone(),
                            text,
                            subset: None,
                        });
                        drawn.push((Vec::new(), source.map(|t| t.glyphs.len())));
                        out.glyphs.len() - 1
                    }
                };
                drawn[n].0.extend(at);
            }
            _ => {}
        }
    }
    for (g, (mut at, all)) in out.glyphs.iter_mut().zip(drawn) {
        at.sort_unstable();
        at.dedup();
        // Indices are in sort order and distinct: as many as it has is
        // every one of them.
        let whole = all.is_none_or(|n| at.len() == n);
        g.subset = (!whole).then_some(at);
    }
    out
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
    /// `clip`: the clip its ancestors set (physical pixels); a node drawn
    /// on its surface but wholly outside it is [`Hidden::Clipped`].
    pub(super) fn probe_push(
        &mut self,
        mut t: NodeTrace,
        look: (f32, [f32; 2], f32),
        frame: kurbo::Rect,
        xform: kurbo::Affine,
        hidden: Option<Hidden>,
        clip: Rect,
    ) {
        let surface = self.surface;
        let Some(trace) = self.trace.as_mut() else {
            return;
        };
        let s = self.scale.as_f64();
        let v = xform.transform_rect_bbox(frame);
        let meets = |r: Rect| {
            v.x1 > f64::from(r.x)
                && v.y1 > f64::from(r.y)
                && v.x0 < f64::from(r.x) + f64::from(r.w)
                && v.y0 < f64::from(r.y) + f64::from(r.h)
        };
        let hidden = hidden.or_else(|| (meets(surface) && !meets(clip)).then_some(Hidden::Clipped));
        if clip != surface {
            let c = LogicalRect::new(
                (f64::from(clip.x) / s) as f32,
                (f64::from(clip.y) / s) as f32,
                (f64::from(clip.w) / s) as f32,
                (f64::from(clip.h) / s) as f32,
            );
            t.clip = Some(c);
        }
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

/// (eval-rules-4) What a text cut to line box `w` drew, when it drew the
/// layout shaped for `w` at this scale (a stand-in for another width or
/// scale is not one to judge): the facts a check needs to see whether
/// each line keeps all that fits (`crate::probe::CutDrawn`).
#[allow(clippy::too_many_arguments)]
pub(super) fn cut(
    shaped: &[Shaped],
    scale: Scale,
    w: f32,
    drawn: &std::sync::Arc<strand_text::TextLayout>,
    text: &str,
    style: &strand_text::TextStyle,
) -> Option<crate::probe::CutDrawn> {
    let own = pick(shaped, scale, Some(w))?;
    if !std::sync::Arc::ptr_eq(&own, drawn) || own.scale != scale {
        return None;
    }
    let natural = pick(shaped, scale, None).filter(|n| n.scale == scale)?;
    let one_line = natural.carets.iter().all(|c| c.line == 0);
    Some(crate::probe::CutDrawn {
        text: std::sync::Arc::from(text),
        ellipsis: style.ellipsis.map(|e| match e {
            strand_text::Ellipsis::Start => "start",
            strand_text::Ellipsis::Middle => "middle",
            strand_text::Ellipsis::End => "end",
        }),
        max_lines: style.max_lines,
        stops: own.carets.iter().map(|c| (c.byte, c.x, c.line)).collect(),
        natural: if one_line {
            natural.carets.iter().map(|c| (c.byte, c.x)).collect()
        } else {
            Vec::new()
        },
    })
}
