//! (eval-coverage) A node's paint as the trace and its recording write it
//! (docs/eval.md, "What a recording holds"): the colours of what it drew
//! itself this frame, where its shadows reach, and which text its glyphs
//! came from, read from the flattened scene (`probe::NodePaint`), never
//! from pixels.
//!
//! A colour is straight-alpha sRGB, each channel clamped to `0..=1` as
//! the raster draws it and quantised to 1/255: `rrggbbaa`. A gradient is
//! its kind, then its stops (`lin45:ff0000ff@0:0000ffff@1`; the angle in
//! degrees to 1/100, offsets to 1/1000). A shadow is its colour and the
//! area it may cover (`000000cc@x,y,w,h`, 1/64 px). A text's glyphs are
//! the text they were shaped from, with the glyphs drawn when not all of
//! them were (`41[1]`: the second glyph of "41", rolling out).

use serde::{Deserialize, Serialize};
use strand_render::probe::{GlyphsDrawn, NodePaint};
use strand_scene::{Color, GradientStop, Paint};

use crate::trace::q;

/// What a node painted itself in one frame, as text.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaintRec {
    /// Its fills (its background), in paint order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bg: Vec<String>,
    /// Its borders.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bd: Vec<String>,
    /// Its drop shadows: colour and extent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sh: Vec<String>,
    /// Its glyphs' paints (a colour, span colours after `+`), one per
    /// text drawn ...
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fg: Vec<String>,
    /// ... and, in the same order, the text each was shaped from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gl: Vec<String>,
}

impl PaintRec {
    pub fn is_empty(&self) -> bool {
        self.bg.is_empty()
            && self.bd.is_empty()
            && self.sh.is_empty()
            && self.fg.is_empty()
            && self.gl.is_empty()
    }

    /// The paint a frame's node drew, as text; `None` when it drew none.
    pub fn of(p: &NodePaint) -> Option<PaintRec> {
        if p.is_empty() {
            return None;
        }
        Some(PaintRec {
            bg: p.fills.iter().map(paint).collect(),
            bd: p.borders.iter().map(paint).collect(),
            sh: p
                .shadows
                .iter()
                .map(|s| {
                    let e = s.extent;
                    format!(
                        "{}@{}",
                        hex(s.color),
                        [e.x, e.y, e.w, e.h].map(|v| num(q(v))).join(",")
                    )
                })
                .collect(),
            fg: p.glyphs.iter().map(glyph_paint).collect(),
            gl: p.glyphs.iter().map(glyph_text).collect(),
        })
    }
}

/// One channel, clamped and quantised to 1/255.
fn channel(v: f32) -> u8 {
    if v.is_nan() {
        return 0;
    }
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// `rrggbbaa`: straight-alpha sRGB, each channel to 1/255.
pub fn hex(c: Color) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}",
        channel(c.r),
        channel(c.g),
        channel(c.b),
        channel(c.a)
    )
}

/// `v` in its shortest decimal form.
fn num(v: f32) -> String {
    let s = format!("{v:.6}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.into() }
}

fn stops(s: &[GradientStop]) -> String {
    s.iter()
        .map(|g| {
            format!(
                ":{}@{}",
                hex(g.color),
                num((g.offset * 1000.0).round() / 1000.0)
            )
        })
        .collect()
}

/// A paint as text (see the module's docs).
pub fn paint(p: &Paint) -> String {
    let deg = |a: f32| num((a * 100.0).round() / 100.0);
    match p {
        Paint::Solid(c) => hex(*c),
        Paint::Linear { angle, stops: s } => format!("lin{}{}", deg(*angle), stops(s)),
        Paint::Radial { stops: s } => format!("rad{}", stops(s)),
        Paint::Conic { from, stops: s } => format!("con{}{}", deg(*from), stops(s)),
    }
}

fn glyph_paint(g: &GlyphsDrawn) -> String {
    let mut s = paint(&g.paint);
    for c in &g.spans {
        s.push('+');
        s += &hex(*c);
    }
    s
}

fn glyph_text(g: &GlyphsDrawn) -> String {
    let mut s = g.text.as_deref().unwrap_or("?").to_string();
    if let Some(at) = &g.subset {
        s.push('[');
        s += &at
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        s.push(']');
    }
    s
}

/// The channels of `rrggbbaa`.
fn rgba(s: &str) -> Option<[i64; 4]> {
    if s.len() != 8 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let ch = |i: usize| i64::from_str_radix(&s[i..i + 2], 16).ok();
    Some([ch(0)?, ch(2)?, ch(4)?, ch(6)?])
}

/// Two paints as text are the same within `tol` steps per colour channel
/// (anything else in them exactly).
pub fn paint_within(a: &str, b: &str, tol: i64) -> bool {
    if a == b {
        return true;
    }
    let split = |s: &str| -> Vec<String> {
        s.split([':', '@', '+'])
            .map(String::from)
            .collect::<Vec<_>>()
    };
    let (pa, pb) = (split(a), split(b));
    pa.len() == pb.len()
        && pa.iter().zip(&pb).all(|(x, y)| {
            x == y
                || matches!((rgba(x), rgba(y)), (Some(c), Some(d))
                    if c.iter().zip(&d).all(|(u, v)| (u - v).abs() <= tol))
        })
}

/// Two shadows as text are the same within `tol` colour steps and `px`
/// quanta (1/64 px) of extent.
pub fn shadow_within(a: &str, b: &str, tol: i64, px: i64) -> bool {
    if a == b {
        return true;
    }
    let part = |s: &str| -> Option<([i64; 4], Vec<i64>)> {
        let (c, r) = s.split_once('@')?;
        let r = r
            .split(',')
            .map(|v| v.parse::<f64>().ok().map(|v| (v * 64.0).round() as i64))
            .collect::<Option<Vec<_>>>()?;
        Some((rgba(c)?, r))
    };
    match (part(a), part(b)) {
        (Some((ca, ra)), Some((cb, rb))) => {
            ca.iter().zip(&cb).all(|(u, v)| (u - v).abs() <= tol)
                && ra.len() == rb.len()
                && ra.iter().zip(&rb).all(|(u, v)| (u - v).abs() <= px)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_are_straight_srgb_to_a_255th() {
        let c = Color {
            r: 1.0,
            g: 0.5,
            b: -0.2,
            a: 0.4,
        };
        assert_eq!(hex(c), "ff800066");
        let g = Paint::Linear {
            angle: 45.0,
            stops: vec![
                GradientStop {
                    offset: 0.0,
                    color: Color::TRANSPARENT,
                },
                GradientStop {
                    offset: 1.0,
                    color: c,
                },
            ],
        };
        assert_eq!(paint(&g), "lin45:00000000@0:ff800066@1");
    }

    #[test]
    fn a_step_per_channel_is_the_same_paint_and_two_are_not() {
        assert!(paint_within("ff800066", "fe810066", 1));
        assert!(!paint_within("ff800066", "fd800066", 1));
        assert!(paint_within(
            "lin45:00000000@0:ff800066@1",
            "lin45:00000001@0:ff800066@1",
            1
        ));
        assert!(!paint_within(
            "lin45:00000000@0:ff800066@1",
            "lin46:00000000@0:ff800066@1",
            1
        ));
        assert!(shadow_within(
            "000000cc@1,2,3,4",
            "000001cc@1.015625,2,3,4",
            1,
            1
        ));
        assert!(!shadow_within(
            "000000cc@1,2,3,4",
            "000000cc@1.5,2,3,4",
            1,
            1
        ));
    }
}
