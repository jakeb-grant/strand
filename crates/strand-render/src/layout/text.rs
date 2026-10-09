//! Measuring leaves for taffy: text from its delivered layouts (or an
//! estimate), fixed-size widgets and lists.

use std::collections::HashMap;

use strand_scene::{LogicalSize, NodeId};
use taffy::prelude::{AvailableSpace, LengthPercentageAuto, Style};
use taffy::{LayoutInput, LayoutOutput, compute_leaf_layout};

use super::Ctx;
use super::list::list_cap;

/// Delivered text layouts, as layout sees them: sizes in logical pixels.
pub(crate) trait TextSizes {
    /// The size of `node`'s text shaped without a width bound.
    fn natural(&self, node: NodeId) -> Option<LogicalSize>;
    /// The size of `node`'s text shaped for a line box `width` wide
    /// (wrapped or ellipsised).
    fn fitted(&self, node: NodeId, width: f32) -> Option<LogicalSize>;
    /// The size of one of `node`'s other texts shaped without a width
    /// bound (a `segmented` label: `TextSpec::part`).
    fn part(&self, node: NodeId, part: u8) -> Option<LogicalSize> {
        let _ = (node, part);
        None
    }
}

/// A text leaf as measured: its node, font size, length and how it
/// fits a narrower box.
pub(super) struct TextLeaf {
    pub(super) node: NodeId,
    pub(super) font: f32,
    pub(super) chars: usize,
    pub(super) word: usize,
    pub(super) shrinks: bool,
    pub(super) wraps: bool,
    pub(super) max_lines: Option<u32>,
    /// `min_width`/`max_width` resolved against the parent: the width the
    /// height is computed for is clamped first, so a text capped by
    /// `max_width` is as tall as its wrapped lines.
    pub(super) min_w: Option<f32>,
    pub(super) max_w: Option<f32>,
}

/// The text size taffy is told for a text leaf.
pub(super) fn measure_text(
    texts: &dyn TextSizes,
    t: &TextLeaf,
    known: taffy::Size<Option<f32>>,
    avail: taffy::Size<AvailableSpace>,
) -> taffy::Size<f32> {
    let TextLeaf {
        node,
        font,
        chars,
        word,
        shrinks,
        wraps,
        max_lines,
        ..
    } = *t;
    let natural = texts.natural(node).unwrap_or_else(|| {
        // Not shaped yet: a guess from its length, replaced when the
        // layout arrives.
        LogicalSize::new(chars as f32 * font * 0.55, (font * 1.2).ceil())
    });
    // Whole pixels up, so rounding the layout never cuts a text that fits.
    let natural = LogicalSize::new(natural.w.ceil(), natural.h.ceil());
    // Wrapping text's smallest width is its longest word (as in CSS),
    // taken as its share of the natural width: a text in a growing column
    // wraps instead of widening it.
    let least = if chars > 0 {
        (natural.w * word as f32 / chars as f32)
            .ceil()
            .min(natural.w)
    } else {
        natural.w
    };
    let w = known.width.unwrap_or_else(|| {
        let w = match avail.width {
            AvailableSpace::MinContent if shrinks => 0.0,
            AvailableSpace::MinContent if wraps => least,
            AvailableSpace::Definite(a) if shrinks => natural.w.min(a.max(0.0)),
            AvailableSpace::Definite(a) if wraps => natural.w.min(a.max(least)),
            _ => natural.w,
        };
        let w = t.max_w.map_or(w, |m| w.min(m));
        t.min_w.map_or(w, |m| w.max(m))
    });
    let h = known.height.unwrap_or_else(|| {
        if w + 1.0 < natural.w && wraps {
            texts.fitted(node, w.round()).map_or_else(
                || {
                    let lines = (natural.w / w.max(1.0)).ceil().max(1.0);
                    let lines = max_lines.map_or(lines, |m| lines.min(m as f32));
                    natural.h * lines
                },
                |s| s.h,
            )
        } else {
            natural.h
        }
    });
    taffy::Size {
        width: w,
        height: h,
    }
}

/// What taffy is told for a leaf: text from its layouts, fixed-size
/// widgets, and a list's rows (capped by its `height`/`max_height`).
pub(super) fn measure_leaf(
    texts: &dyn TextSizes,
    lists: &HashMap<NodeId, f32>,
    inputs: LayoutInput,
    ctx: Option<&mut Ctx>,
    style: &Style,
) -> LayoutOutput {
    let cap = matches!(ctx, Some(Ctx::List(_)))
        .then(|| list_cap(&inputs, style))
        .flatten();
    let width_of = |d: LengthPercentageAuto| -> Option<f32> {
        use taffy::util::MaybeResolve;
        let v: Option<f32> = d.maybe_resolve(inputs.parent_size.width, |_: *const (), _| 0.0);
        v.filter(|v| v.is_finite())
    };
    let max_w = || width_of(style.max_size.width);
    let min_w = || width_of(style.min_size.width);
    compute_leaf_layout(
        inputs,
        style,
        |_, _| 0.0,
        |known, avail| match ctx {
            Some(Ctx::Text {
                node,
                font,
                chars,
                word,
                shrinks,
                wraps,
                max_lines,
                empty,
            }) => {
                if *empty {
                    return taffy::Size {
                        width: known.width.unwrap_or(0.0),
                        height: known.height.unwrap_or(0.0),
                    };
                }
                let leaf = TextLeaf {
                    node: *node,
                    font: *font,
                    chars: *chars,
                    word: *word,
                    shrinks: *shrinks,
                    wraps: *wraps,
                    max_lines: *max_lines,
                    min_w: min_w(),
                    max_w: max_w(),
                };
                measure_text(texts, &leaf, known, avail)
            }
            Some(Ctx::Fixed(w, h)) => taffy::Size {
                width: known.width.unwrap_or(*w),
                height: known.height.unwrap_or(*h),
            },
            Some(Ctx::Square(side)) => {
                let w = known.width.or(known.height).unwrap_or(*side);
                taffy::Size {
                    width: w,
                    height: known.height.unwrap_or(w),
                }
            }
            Some(Ctx::Segmented { node, n, font }) => {
                let widest = (0..*n)
                    .map(|i| {
                        texts
                            .part(*node, i as u8 + 1)
                            .map_or(*font * 0.55 * 6.0, |s| s.w)
                    })
                    .fold(0.0f32, f32::max);
                let pad = crate::widgets::SEGMENT_PAD;
                taffy::Size {
                    width: known
                        .width
                        .unwrap_or(((widest + 2.0 * pad) * *n as f32).ceil()),
                    height: known.height.unwrap_or((*font * 2.0).ceil()),
                }
            }
            Some(Ctx::List(id)) => {
                let h = lists.get(id).copied().unwrap_or_default();
                // A scroll container: its rows past `height` or
                // `max_height` scroll, so that is all its parent sees of
                // them, and its smallest size is none.
                let h = match avail.height {
                    AvailableSpace::MinContent => 0.0,
                    _ => cap.map_or(h, |c| h.min(c)),
                };
                taffy::Size {
                    width: known.width.unwrap_or(0.0),
                    height: known.height.unwrap_or(h),
                }
            }
            None => taffy::Size {
                width: known.width.unwrap_or(0.0),
                height: known.height.unwrap_or(0.0),
            },
        },
    )
}
