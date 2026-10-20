//! `ink`: what a text draws fits the box it is laid out in. It reads the
//! glyphs' ink each frame traces (the layout drawn, where it was drawn,
//! clipped where the frame clips it), so it sees what no box does: a
//! stand-in layout spilling past its box, or one cut far shorter than
//! its box holds (owner-testing.md bug 13, flatten/text.rs `place_text`:
//! "a layout of another width stands in: the widest shaped for no more
//! than the box (last frame's, while it grows), else the nearest wider
//! one clipped to the box; the unbounded one only where nothing else
//! was ever shaped, also clipped").
//!
//! - **spill**: a text's ink stays across its own box, give or take
//!   [`OVERHANG`] (a glyph's overhang, and the pixel a text may be wider
//!   than its box before it is cut). A text drawn by the layout shaped
//!   for its own box (`txt=S`) is also given, on each side, how far that
//!   layout's own ink reaches past the layout's box (`Row::ov`: the
//!   font's overhang, an icon glyph wider than its advance, an italic):
//!   ink the shaper put there is the font's, while a layout drawn in the
//!   wrong place (a wrong origin, an offset applied twice) still spills.
//!   (launcher-rules) A text without an ellipsis whose line box is
//!   narrower than one of its words draws that word whole past the box's
//!   end: a line breaks only between words, and CSS starts an overlong
//!   line at its start edge and lets it overflow at its end (`overflow:
//!   visible`). Such a box comes from a full row giving way around the
//!   text's holder (design.md "Shrinking"; a box's stack cell is
//!   `minmax(0, 1fr)`, decisions.md wave3-pixels). The end side of a text
//!   drawn by the layout shaped for its box is given how far its widest
//!   word, measured on its unbounded layout (`Frame::lines`, as `fills`
//!   reads it), is wider than that box. Its start side, and a text drawn
//!   by another layout, are held as before.
//! - **over-cut**: a text cut with an ellipsis draws no less of itself
//!   as its box widens: from one frame to the next its line box grows
//!   and its ink does not narrow (an ellipsis cuts at a word or a glyph,
//!   so more room never shows less; a stand-in from a narrower box is
//!   the one the frame before drew, kept while it stands in). The frame
//!   before counts only where its ink stopped short of its box's edge:
//!   ink that reaches the edge may be a wider layout clipped there,
//!   which a wider box cannot draw again. A frame logic's diff or input
//!   reached is left out: its text may have changed.
//! - (eval-rules-4) **fills**: a text cut to its line box keeps on each
//!   line all of itself that fits there: a wrapped line breaks where its
//!   next word no longer fits, and an ellipsis at the end keeps at least
//!   every whole word that fits with it (it cuts at a word or a glyph,
//!   so it never keeps less than the words that fit). Read from the
//!   layout drawn when it is the one shaped for that box at the frame's
//!   scale (`Frame::lines`): the line before each break, with its next
//!   word, or the kept start through the end of its next word with the
//!   ellipsis, measured on the text's unbounded layout, would not fit,
//!   give or take a pixel and `tol`. Lines broken at a newline, an
//!   ellipsis at the start or middle, and a text whose unbounded layout
//!   is not one left-to-right line are not judged.

use std::collections::HashMap;

use super::{Cx, Finding, first_per_node};
use crate::scenario::CheckId;

/// How far ink may reach past its box, logical px.
pub const OVERHANG: f64 = 2.0;

pub fn run(cx: &Cx) -> Vec<Finding> {
    let mut out = Vec::new();
    let seg = |k: u32| {
        cx.trace
            .segments
            .iter()
            .enumerate()
            .rev()
            .find(|(_, s)| s.s.is_none_or(|s| s < k) && k <= s.e)
    };
    // Per text and surface, the frame before: its line box and ink
    // width, and whether its ink stopped short of its box's edge.
    let mut before: HashMap<(&str, u32), (f64, f64, bool)> = HashMap::new();
    for f in &cx.trace.frames {
        let mut now: HashMap<(&str, u32), (f64, f64, bool)> = HashMap::new();
        // Per cut text, how far its widest word passes its line box.
        let words: HashMap<(&str, u32), f64> = f
            .lines
            .iter()
            .filter(|l| l.ellipsis.is_none())
            .filter_map(|l| {
                let fit =
                    f.n.iter()
                        .find(|r| r.key == l.key && r.surface == l.s)?
                        .fit?;
                let widest = widest_word(&l.text, &l.natural)?;
                Some(((l.key.as_str(), l.s), (widest - f64::from(fit)).max(0.0)))
            })
            .collect();
        for r in &f.n {
            let Some(ink) = r.ink else { continue };
            let key = r.key.as_str();
            let at = seg(f.k);
            if at.is_some_and(|(si, _)| {
                cx.exempt(
                    CheckId::Ink,
                    si,
                    cx.trace.nodes.get_key_value(key).map_or(key, |(k, _)| k),
                    strand_render::probe::Axes::W,
                )
            }) {
                continue;
            }
            let name = at.map(|(_, s)| s.name.as_str()).unwrap_or("");
            let (left, right) = (f64::from(ink[0]), f64::from(ink[0] + ink[2]));
            let (bl, br) = (f64::from(r.drawn[0]), f64::from(r.drawn[0] + r.drawn[2]));
            let give = OVERHANG + cx.tol;
            let [ol, or] = match (r.text.as_deref(), r.ov) {
                (Some("S"), Some([l, r])) => [f64::from(l), f64::from(r)],
                _ => [0.0, 0.0],
            };
            let or = or
                + match r.text.as_deref() {
                    Some("S") => words.get(&(key, r.surface)).copied().unwrap_or(0.0),
                    _ => 0.0,
                };
            if left < bl - give - ol || right > br + give + or {
                let past = (bl - ol - left).max(right - br - or);
                out.push(
                    Finding::fail(
                        CheckId::Ink,
                        format!("spill: its glyphs reach {past:.1} px past its box"),
                    )
                    .at(f.k)
                    .node(key)
                    .axis("w")
                    .got(past, [0.0, give])
                    .in_segment(name),
                );
            }
            let width = right - left;
            let ellipsis = cx.trace.nodes.get(key).is_some_and(|m| m.ellipsis);
            if let (Some(fit), true) = (r.fit, ellipsis) {
                let fit = f64::from(fit);
                if let Some((was_fit, was_ink, short)) = before.get(&(key, r.surface))
                    && *short
                    && fit > was_fit + cx.tol
                    && width < was_ink - cx.tol
                    && !f.driven
                {
                    out.push(
                        Finding::fail(
                            CheckId::Ink,
                            format!(
                                "over-cut: its line box widened {was_fit:.0} -> {fit:.0} px and \
                                 its ellipsised text drew less of itself ({was_ink:.1} -> \
                                 {width:.1} px)"
                            ),
                        )
                        .at(f.k)
                        .node(key)
                        .axis("w")
                        .got(width, [was_ink - cx.tol, fit + give])
                        .in_segment(name),
                    );
                }
                now.insert((key, r.surface), (fit, width, right < br - cx.tol));
            }
        }
        before.retain(|(_, s), _| !f.painted.contains(s));
        before.extend(now);
    }
    out.extend(fills(cx));
    first_per_node(out)
}

/// (launcher-rules) The width of the widest word of `text` (a run with
/// no whitespace) on its unbounded layout, from `natural`'s x at each
/// cluster boundary (one left-to-right line). `None` when that layout is
/// not one such line.
fn widest_word(text: &str, natural: &[(u32, f32)]) -> Option<f64> {
    let nat: Vec<(usize, f64)> = natural
        .iter()
        .map(|(b, x)| (*b as usize, f64::from(*x)))
        .collect();
    if nat.len() < 2
        || nat
            .windows(2)
            .any(|w| w[1].0 < w[0].0 || w[1].1 < w[0].1 - 0.01)
    {
        return None;
    }
    let x_at = |b: usize| nat.iter().find(|(nb, _)| *nb >= b).map(|(_, x)| *x);
    let mut widest: f64 = 0.0;
    let mut start = None;
    for (i, ch) in text
        .char_indices()
        .chain(std::iter::once((text.len(), ' ')))
    {
        match (ch.is_whitespace(), start) {
            (false, None) => start = Some(i),
            (true, Some(a)) => {
                if let (Some(xa), Some(xb)) = (x_at(a), x_at(i)) {
                    widest = widest.max(xb - xa);
                }
                start = None;
            }
            _ => {}
        }
    }
    Some(widest)
}

/// The ellipsis an `ellipsis:` text ends with when cut.
const ELLIPSIS: char = '\u{2026}';

/// `fills`: each line of a cut text keeps what fits.
fn fills(cx: &Cx) -> Vec<Finding> {
    let tol = 1.0 + cx.tol;
    let mut out = Vec::new();
    let mut lines_judged = 0u64;
    for f in &cx.trace.frames {
        for l in &f.lines {
            if cx.allowed(CheckId::Ink, &l.key) {
                continue;
            }
            let Some(fit) =
                f.n.iter()
                    .find(|r| r.key == l.key && r.surface == l.s)
                    .and_then(|r| r.fit)
                    .map(f64::from)
            else {
                continue;
            };
            // The unbounded layout's x at each cluster boundary, one
            // left-to-right line.
            let nat: Vec<(usize, f64)> = l
                .natural
                .iter()
                .map(|(b, x)| (*b as usize, f64::from(*x)))
                .collect();
            if nat.len() < 2
                || nat
                    .windows(2)
                    .any(|w| w[1].0 < w[0].0 || w[1].1 < w[0].1 - 0.01)
            {
                continue;
            }
            // Where the text before byte `b` ends (the first boundary at
            // or past it).
            let x_at = |b: usize| nat.iter().find(|(nb, _)| *nb >= b).map(|(_, x)| *x);
            let text = l.text.as_str();
            // The drawn layout's lines: the first byte of each, its first
            // and last stop's x.
            let mut starts: std::collections::BTreeMap<u32, (usize, f64, f64)> = Default::default();
            for (b, x, line) in &l.stops {
                let e = starts
                    .entry(*line)
                    .or_insert((*b as usize, f64::from(*x), f64::from(*x)));
                e.0 = e.0.min(*b as usize);
                e.1 = e.1.min(f64::from(*x));
                e.2 = e.2.max(f64::from(*x));
            }
            let lines: Vec<(usize, f64, f64)> = starts.into_values().collect();
            let end = l
                .stops
                .iter()
                .map(|(b, _, _)| *b as usize)
                .max()
                .unwrap_or(0);
            let width = nat.last().map_or(0.0, |n| n.1);
            // An ellipsis at the end cut it: its stops end past the kept
            // start by the ellipsis, or short of the text, or its one
            // line is narrower than the text's.
            let one = lines.len() == 1;
            let cut_at = (l.ellipsis.as_deref() == Some("end")
                && (end != text.len() || (one && lines[0].2 < width - 0.5)))
                .then(|| end.checked_sub(ELLIPSIS.len_utf8()))
                .flatten()
                .filter(|k| text.is_char_boundary(*k));
            // The bytes the lines hold of the text itself.
            let own = cut_at.unwrap_or(end.min(text.len()));
            let mut fail = |what: String, got: f64| {
                out.push(
                    Finding::fail(CheckId::Ink, what)
                        .at(f.k)
                        .node(&l.key)
                        .axis("w")
                        .got(got, [fit - tol, fit + tol]),
                );
            };
            // Each break: would the line hold its next word?
            for w in lines.windows(2) {
                let (a, b) = (w[0].0, w[1].0);
                if b <= a || b > own || text.get(a..b).is_none_or(|t| t.contains('\n')) {
                    continue;
                }
                let rest = &text[b..];
                let word = rest.find(char::is_whitespace).unwrap_or(rest.len());
                let c = (b + word).min(own);
                if c <= b {
                    continue;
                }
                let (Some(xa), Some(xc)) = (x_at(a), x_at(c)) else {
                    continue;
                };
                lines_judged += 1;
                if xc - xa + tol <= fit {
                    fail(
                        format!(
                            "fills: a line breaks before {:?}, though with it the line is \
                             {:.1} px wide in its {fit:.0} px line box",
                            &text[b..c],
                            xc - xa
                        ),
                        xc - xa,
                    );
                }
            }
            // An ellipsis at the end: would the kept start take its next
            // word?
            if let (Some(k), Some(last)) = (cut_at, lines.last()) {
                let a = last.0;
                let rest = text.get(k..).unwrap_or("");
                let skip = rest.len() - rest.trim_start().len();
                let word = rest[skip..]
                    .find(char::is_whitespace)
                    .unwrap_or(rest.len() - skip);
                if word == 0 {
                    continue;
                }
                let c = k + skip + word;
                // The ellipsis as drawn: from the cut to the line's end.
                let ell = l
                    .stops
                    .iter()
                    .find(|(b, _, _)| *b as usize == k)
                    .map(|(_, x, _)| last.2 - f64::from(*x));
                let (Some(xa), Some(xc), Some(ell)) = (x_at(a), x_at(c), ell) else {
                    continue;
                };
                lines_judged += 1;
                let want = xc - xa + ell;
                if want + tol <= fit {
                    fail(
                        format!(
                            "fills: its ellipsis cuts before {:?}, though with it the line is \
                             {want:.1} px wide in its {fit:.0} px line box",
                            &text[k..c]
                        ),
                        want,
                    );
                }
            }
        }
    }
    super::stats("ink.fills", cx, &[("lines", lines_judged)]);
    out
}
