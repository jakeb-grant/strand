//! (M4) `letters { y: 2 * wave(1s, phase: index * 0.1) }` (design.md,
//! "Paint and light": "per-letter animation"; decisions.md m4-owner:
//! `letters` keeps no positional and animates the text of its enclosing
//! `text` node).
//!
//! A `text` with a `letters` child draws each letter on its own: the
//! letters are its laid-out glyphs in visual order (a ligature is one
//! letter; spaces draw nothing and are none), `index` counts them from 0
//! and `count` is how many there are. The `letters` node's props are
//! resolved once per letter with that letter's `index` and `count` (and
//! the text's `t`), and move, fade, scale, turn or recolour just that
//! letter: `x` and `y` (logical pixels), `opacity`, `scale` and `rotate`
//! (about the letter's centre) and `color`.

use std::sync::Arc;

use strand_scene::{Color, NodeKind, Prop, PropValue, TimeContext, TokenScope};
use strand_text::{GlyphRun, TextLayout};

use super::{degrees, number};
use crate::tree::{Node, SceneTree};

/// One letter's props.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct Letter {
    pub dx: f32,
    pub dy: f32,
    pub opacity: f32,
    pub scale: f32,
    pub rotate: f32,
    pub color: Option<Color>,
}

impl Default for Letter {
    fn default() -> Self {
        Letter {
            dx: 0.0,
            dy: 0.0,
            opacity: 1.0,
            scale: 1.0,
            rotate: 0.0,
            color: None,
        }
    }
}

/// The `letters` child of `node`, if it has one.
pub(crate) fn child<'a>(tree: &'a SceneTree, node: &Node) -> Option<&'a Node> {
    if node.kind != NodeKind::Text {
        return None;
    }
    node.children
        .iter()
        .filter_map(|c| tree.get(*c))
        .find(|n| n.kind == NodeKind::Letters)
}

/// The props of letter `index` of `count`, from `letters`'s props in
/// `scope` (whose time, if any, is the text's).
pub(crate) fn letter(letters: &Node, scope: &TokenScope, index: u32, count: u32) -> Letter {
    let base = scope.time().unwrap_or(TimeContext::at(0.0));
    let cx = TimeContext {
        index,
        count,
        ..base
    };
    let scope = scope.with_time(Some(cx));
    let get = |p: Prop| {
        letters
            .props
            .iter()
            .find(|e| e.prop == p)
            .and_then(|e| scope.resolve(&e.value))
    };
    let f = |p: Prop, d: f32| get(p).as_deref().and_then(number).unwrap_or(d);
    Letter {
        dx: f(Prop::X, 0.0).clamp(-1e4, 1e4),
        dy: f(Prop::Y, 0.0).clamp(-1e4, 1e4),
        opacity: f(Prop::Opacity, 1.0).clamp(0.0, 1.0),
        scale: f(Prop::Scale, 1.0).clamp(0.0, 1000.0),
        rotate: get(Prop::Rotate)
            .as_deref()
            .and_then(degrees)
            .unwrap_or(0.0)
            % 360.0,
        color: match get(Prop::Color).as_deref() {
            Some(PropValue::Color(c)) => Some(*c),
            _ => None,
        },
    }
}

/// `layout` split into one layout per letter (glyph), in visual order,
/// each with its run's colour. The copies carry no atlas uploads (the
/// whole layout's were applied when it arrived) and no caret stops.
pub(crate) fn split(layout: &TextLayout) -> Vec<Arc<TextLayout>> {
    let mut out = Vec::new();
    for run in &layout.runs {
        for g in &run.glyphs {
            let mut one = layout.clone();
            one.uploads.clear();
            one.carets.clear();
            one.runs = vec![GlyphRun {
                font_size: run.font_size,
                color: run.color,
                glyphs: vec![*g],
                underline: None,
            }];
            out.push(Arc::new(one));
        }
    }
    out
}

/// The display items that draw `layout` (placed at `origin`, physical
/// pixels, on a surface `s` device pixels a logical one, under the
/// transform `xform`) one letter at a time, each moved by its `letters`
/// props: each letter's glyphs item (its colour `color`, span colours
/// `spans` and `fill` faded by its opacity), wrapped in a transform when
/// it scales or turns. Bounds are in the node's untransformed space.
#[allow(clippy::too_many_arguments)]
pub(crate) fn items(
    letters: &Node,
    scope: &TokenScope,
    layout: &TextLayout,
    origin: (i32, i32),
    color: Color,
    spans: &[Color],
    fill: Option<Arc<crate::flatten::GlyphFill>>,
    s: f64,
    xform: vello_cpu::kurbo::Affine,
) -> Vec<(crate::flatten::Item, strand_scene::Rect)> {
    use crate::flatten::Item;
    use vello_cpu::kurbo::{self, Affine};
    let parts = split(layout);
    let count = parts.len() as u32;
    let mut out = Vec::with_capacity(parts.len());
    for (i, one) in parts.into_iter().enumerate() {
        let lt = letter(letters, scope, i as u32, count);
        if lt.opacity <= 0.0 || lt.scale <= 0.0 {
            continue;
        }
        let Some(g) = one.runs.first().and_then(|r| r.glyphs.first()).copied() else {
            continue;
        };
        let x = origin.0 + (lt.dx as f64 * s).round() as i32;
        let y = origin.1 + (lt.dy as f64 * s).round() as i32;
        let glyph = kurbo::Rect::new(
            (x + g.x) as f64 - 1.0,
            (y + g.y) as f64 - 1.0,
            (x + g.x) as f64 + g.slot.w as f64 + 2.0,
            (y + g.y) as f64 + g.slot.h as f64 + 2.0,
        );
        let fade = |c: Color| c.with_alpha(c.a * lt.opacity);
        let item = Item::Glyphs {
            x,
            y,
            layout: one,
            color: fade(lt.color.unwrap_or(color)),
            spans: spans.iter().map(|c| fade(*c)).collect(),
            fill: fill.clone(),
        };
        let turned = lt.scale != 1.0 || lt.rotate != 0.0;
        let local = if turned {
            let c = glyph.center().to_vec2();
            Affine::translate(c)
                * Affine::rotate((lt.rotate as f64).to_radians())
                * Affine::scale(lt.scale as f64)
                * Affine::translate(-c)
        } else {
            Affine::IDENTITY
        };
        let b = local.transform_rect_bbox(glyph);
        let bounds = strand_scene::Rect::from_edges(
            b.x0.floor() as i64,
            b.y0.floor() as i64,
            b.x1.ceil() as i64,
            b.y1.ceil() as i64,
        );
        if turned {
            out.push((Item::PushTransform(xform * local), bounds));
        }
        out.push((item, bounds));
        if turned {
            out.push((Item::PopTransform, bounds));
        }
    }
    out
}
