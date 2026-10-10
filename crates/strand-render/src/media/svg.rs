//! (M4) Bindable SVG: `svg "gauge.svg" { #needle { rotate: level * 270deg } }`
//! (design.md, "SVG with bindable layers").
//!
//! An `svg` is a CPU raster node drawn with resvg. Each `#id { … }` block
//! is a `svg_part` child (the compiler's lowering) whose props, resolved
//! each frame like any node's (springs and time included), apply to the
//! element with that id: `rotate` and `scale` about the drawing's centre,
//! `x` and `y` in the drawing's own units, `opacity`, and `fill` (every
//! pixel of the layer in that one colour, its coverage kept). The `svg`'s
//! own `fill` colours the whole drawing the same way, and `fit` places it
//! like an image's.
//!
//! The file is read once per source, on the first draw, and the parts'
//! elements wrapped in groups of their own (`<g id="__strand_part_N">`)
//! by their byte ranges, so every SVG feature inside a layer (gradients,
//! clips, masks, filters) is kept. A part's transform is written in its
//! parent's coordinates, which the first parse of the wrapped file finds
//! (the wrapper's absolute transform). Each change of a part's values
//! parses the wrapped text with those attributes and draws it; nothing
//! is drawn between changes. A part whose id the file lacks does nothing.

use std::collections::hash_map::DefaultHasher;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use resvg::{tiny_skia, usvg};
use strand_scene::{Color, Length, Prop, PropValue, TimeContext};
use vello_cpu::color::PremulRgba8;

use super::graph::{flat, number};
use crate::clock::Rate;
use crate::image::Fit;
use crate::offscreen::{RasterProps, RasterSource};

/// The largest SVG file read.
pub const MAX_SVG_BYTES: u64 = 4 << 20;

/// One part's values this frame.
#[derive(Clone, Debug, PartialEq)]
struct Part {
    name: String,
    rotate: f32,
    scale: f32,
    x: f32,
    y: f32,
    opacity: f32,
    fill: Option<[u8; 4]>,
}

impl Part {
    fn of(name: &str, props: &[(Prop, PropValue)]) -> Part {
        let get = |p: Prop| props.iter().find(|(q, _)| *q == p).map(|(_, v)| v);
        let len = |p: Prop| match get(p) {
            Some(PropValue::Length(Length::Px(v))) => Some(*v),
            v => number(v),
        };
        Part {
            name: name.to_string(),
            rotate: match get(Prop::Rotate) {
                Some(PropValue::Angle(a)) => *a,
                v => number(v).unwrap_or(0.0),
            },
            scale: number(get(Prop::Scale)).unwrap_or(1.0),
            x: len(Prop::X).unwrap_or(0.0),
            y: len(Prop::Y).unwrap_or(0.0),
            opacity: number(get(Prop::Opacity)).unwrap_or(1.0).clamp(0.0, 1.0),
            fill: get(Prop::Fill).and_then(flat).map(rgba8),
        }
        .sane()
    }

    /// Non-finite values as if unset.
    fn sane(mut self) -> Part {
        for (v, d) in [
            (&mut self.rotate, 0.0),
            (&mut self.scale, 1.0),
            (&mut self.x, 0.0),
            (&mut self.y, 0.0),
            (&mut self.opacity, 1.0),
        ] {
            if !v.is_finite() {
                *v = d;
            }
        }
        self
    }

    fn hash_into(&self, h: &mut DefaultHasher) {
        self.name.hash(h);
        for v in [self.rotate, self.scale, self.x, self.y, self.opacity] {
            v.to_bits().hash(h);
        }
        self.fill.hash(h);
    }
}

fn rgba8(c: Color) -> [u8; 4] {
    [c.r, c.g, c.b, c.a].map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8)
}

/// A piece of the wrapped text: the file's, or part `N`'s attributes.
#[derive(Debug)]
enum Piece {
    Text(String),
    Attrs(usize),
}

/// Where a part's transform is written: its parent's coordinates.
#[derive(Clone, Copy, Debug)]
struct Frame {
    /// The drawing's centre in the parent's coordinates.
    centre: (f64, f64),
    /// The parent's inverse linear map (`x`, `y` offsets into it).
    inv: [f64; 4],
}

impl Default for Frame {
    fn default() -> Self {
        Frame {
            centre: (0.0, 0.0),
            inv: [1.0, 0.0, 0.0, 1.0],
        }
    }
}

/// The file, its parts wrapped.
#[derive(Debug)]
struct Doc {
    names: Vec<String>,
    pieces: Vec<Piece>,
    frames: Vec<Frame>,
}

/// The wrapped text with part `N`'s attributes from `attrs`.
fn assemble(pieces: &[Piece], mut attrs: impl FnMut(usize, &mut String)) -> String {
    let mut out = String::new();
    for p in pieces {
        match p {
            Piece::Text(t) => out.push_str(t),
            Piece::Attrs(n) => attrs(*n, &mut out),
        }
    }
    out
}

/// Wraps each named element of `text` in a group of its own; `None` if
/// the text is no XML.
fn wrap(text: &str, names: &[String]) -> Option<Vec<Piece>> {
    let doc = usvg::roxmltree::Document::parse(text).ok()?;
    // (byte offset, closing, part): closings before openings at one
    // offset (adjacent siblings), so nested parts nest.
    let mut at: Vec<(usize, bool, usize)> = Vec::new();
    for (n, name) in names.iter().enumerate() {
        if let Some(el) = doc
            .descendants()
            .find(|e| e.is_element() && e.attribute("id") == Some(name.as_str()))
        {
            let r = el.range();
            at.push((r.start, false, n));
            at.push((r.end, true, n));
        }
    }
    at.sort_by_key(|(off, closing, n)| (*off, !*closing, *n));
    let mut pieces = Vec::new();
    let mut from = 0;
    for (off, closing, n) in at {
        pieces.push(Piece::Text(text[from..off].to_string()));
        if closing {
            pieces.push(Piece::Text("</g>".into()));
        } else {
            pieces.push(Piece::Text(format!("<g id=\"__strand_part_{n}\"")));
            pieces.push(Piece::Attrs(n));
        }
        from = off;
    }
    pieces.push(Piece::Text(text[from..].to_string()));
    Some(pieces)
}

fn options() -> usvg::Options<'static> {
    usvg::Options::default()
}

impl Doc {
    fn new(text: &str, names: &[String]) -> Option<Doc> {
        let pieces = wrap(text, names)?;
        let plain = assemble(&pieces, |_, out| out.push('>'));
        let tree = usvg::Tree::from_str(&plain, &options()).ok()?;
        let size = tree.size();
        let c = tiny_skia::Point::from_xy(size.width() / 2.0, size.height() / 2.0);
        let frames = (0..names.len())
            .map(|n| {
                let Some(node) = tree.node_by_id(&format!("__strand_part_{n}")) else {
                    return Frame::default();
                };
                let Some(inv) = node.abs_transform().invert() else {
                    return Frame::default();
                };
                let mut p = [c];
                inv.map_points(&mut p);
                Frame {
                    centre: (p[0].x as f64, p[0].y as f64),
                    inv: [inv.sx, inv.ky, inv.kx, inv.sy].map(f64::from),
                }
            })
            .collect();
        Some(Doc {
            names: names.to_vec(),
            pieces,
            frames,
        })
    }

    /// Part `n`'s attributes (and its fill filter), closing its tag.
    fn attrs(&self, n: usize, p: &Part, out: &mut String) {
        let f = self.frames.get(n).copied().unwrap_or_default();
        let moved = p.rotate != 0.0 || p.scale != 1.0 || p.x != 0.0 || p.y != 0.0;
        if moved {
            let (cx, cy) = f.centre;
            let [a, b, c, d] = f.inv;
            let (dx, dy) = (
                a * p.x as f64 + c * p.y as f64,
                b * p.x as f64 + d * p.y as f64,
            );
            let _ = write!(
                out,
                " transform=\"translate({} {}) rotate({}) scale({}) translate({} {})\"",
                cx + dx,
                cy + dy,
                p.rotate,
                p.scale,
                -cx,
                -cy
            );
        }
        if p.opacity < 1.0 {
            let _ = write!(out, " opacity=\"{}\"", p.opacity);
        }
        match p.fill {
            Some([r, g, b, a]) => {
                let _ = write!(
                    out,
                    " filter=\"url(#__strand_fill_{n})\"><filter id=\"__strand_fill_{n}\" \
                     filterUnits=\"userSpaceOnUse\" x=\"-100000\" y=\"-100000\" \
                     width=\"200000\" height=\"200000\" color-interpolation-filters=\"sRGB\">\
                     <feFlood flood-color=\"rgb({r},{g},{b})\" flood-opacity=\"{}\"/>\
                     <feComposite in2=\"SourceGraphic\" operator=\"in\"/></filter>",
                    a as f32 / 255.0
                );
            }
            None => out.push('>'),
        }
    }
}

#[derive(Debug, Default)]
struct Svg {
    source: String,
    /// The file's text, read on the first draw of a source.
    text: Option<Result<Arc<str>, String>>,
    doc: Option<Doc>,
    parts: Vec<Part>,
    fit: Fit,
    fill: Option<[u8; 4]>,
}

impl Svg {
    /// Reads the source and wraps its parts, when either changed.
    fn prepare(&mut self) {
        if self.text.is_none() && !self.source.is_empty() {
            self.text = Some(
                crate::image::read_local(&self.source, MAX_SVG_BYTES)
                    .map_err(|e| e.to_string())
                    .and_then(|b| String::from_utf8(b).map_err(|e| e.to_string()))
                    .map(Arc::from),
            );
        }
        let names: Vec<String> = self.parts.iter().map(|p| p.name.clone()).collect();
        let stale = self.doc.as_ref().is_none_or(|d| d.names != names);
        if stale && let Some(Ok(text)) = &self.text {
            // Not XML: drawn as resvg draws it (nothing, when broken).
            self.doc = Doc::new(text, &names);
        }
    }
}

/// An `svg` node's source.
#[derive(Debug, Default)]
pub struct SvgSource {
    inner: Mutex<Svg>,
}

/// Draws `text` into a `w × h` RGBA buffer by `fit`.
fn render(text: &str, w: u32, h: u32, scale: f32, fit: Fit) -> Option<tiny_skia::Pixmap> {
    let tree = usvg::Tree::from_str(text, &options()).ok()?;
    let size = tree.size();
    let (sw, sh) = (size.width() as f64, size.height() as f64);
    if !(sw > 0.0 && sh > 0.0) {
        return None;
    }
    // `fit: none` draws at the SVG's own size in logical pixels.
    let s = scale.max(0.01) as f64;
    let (ox, oy, dw, dh) = match fit {
        Fit::None => {
            let (dw, dh) = (sw * s, sh * s);
            ((w as f64 - dw) / 2.0, (h as f64 - dh) / 2.0, dw, dh)
        }
        _ => crate::image::placement(sw, sh, w as f64, h as f64, fit),
    };
    let mut pm = tiny_skia::Pixmap::new(w, h)?;
    let t = tiny_skia::Transform::from_row(
        (dw / sw) as f32,
        0.0,
        0.0,
        (dh / sh) as f32,
        ox as f32,
        oy as f32,
    );
    resvg::render(&tree, t, &mut pm.as_mut());
    Some(pm)
}

impl RasterSource for SvgSource {
    fn draw(&self, pixels: &mut [PremulRgba8], w: u32, h: u32, scale: f32, _time: TimeContext) {
        let Ok(s) = self.inner.lock() else {
            return;
        };
        let Some(text) = (match (&s.doc, &s.text) {
            (Some(doc), _) => Some(assemble(&doc.pieces, |n, out| match s.parts.get(n) {
                Some(p) => doc.attrs(n, p, out),
                None => out.push('>'),
            })),
            (None, Some(Ok(t))) => Some(t.to_string()),
            _ => None,
        }) else {
            return;
        };
        let (fit, fill) = (s.fit, s.fill);
        drop(s);
        let Some(pm) = render(&text, w, h, scale, fit) else {
            return;
        };
        for (d, p) in pixels.iter_mut().zip(pm.pixels()) {
            let a = p.alpha();
            *d = match fill {
                Some([r, g, b, fa]) => {
                    let k = a as u32 * fa as u32;
                    let m = |c: u8| ((c as u32 * k + 255 * 255 / 2) / (255 * 255)) as u8;
                    PremulRgba8 {
                        r: m(r),
                        g: m(g),
                        b: m(b),
                        a: ((k + 127) / 255) as u8,
                    }
                }
                None => PremulRgba8 {
                    r: p.red(),
                    g: p.green(),
                    b: p.blue(),
                    a,
                },
            };
        }
    }

    fn rate(&self) -> Rate {
        Rate::Refresh
    }

    /// No clock: it redraws when its or its parts' values change (a part
    /// that reads time gives the `svg` node its clock).
    fn clock(&self) -> Option<Rate> {
        None
    }

    fn state(&self, props: &RasterProps<'_>) -> u64 {
        let Ok(mut s) = self.inner.lock() else {
            return 0;
        };
        let source = match (props.get)(Prop::Source) {
            Some(PropValue::Text(t) | PropValue::Keyword(t)) => t.trim().to_string(),
            _ => String::new(),
        };
        if source != s.source {
            s.source = source;
            s.text = None;
            s.doc = None;
        }
        s.fit = match (props.get)(Prop::Fit) {
            Some(PropValue::Keyword(k)) => Fit::from_name(k).unwrap_or_default(),
            _ => Fit::default(),
        };
        s.fill = (props.get)(Prop::Fill).and_then(flat).map(rgba8);
        s.parts = props
            .parts
            .iter()
            .map(|(name, p)| Part::of(name, p))
            .collect();
        s.prepare();
        let mut h = DefaultHasher::new();
        s.source.hash(&mut h);
        s.fit.hash(&mut h);
        s.fill.hash(&mut h);
        matches!(s.text, Some(Ok(_))).hash(&mut h);
        for p in &s.parts {
            p.hash_into(&mut h);
        }
        h.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GAUGE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20"><g id="face"><rect width="20" height="20" fill="#000"/></g><g transform="translate(10 0)"><rect id="needle" x="-1" y="2" width="2" height="8" fill="#fff"/></g></svg>"##;

    #[test]
    fn parts_are_wrapped_by_id_and_nest() {
        let names = ["needle".to_string(), "face".to_string(), "nope".to_string()];
        let pieces = wrap(GAUGE, &names).unwrap();
        let text = assemble(&pieces, |n, out| {
            let _ = write!(out, " data-n=\"{n}\">");
        });
        assert!(text.contains(r#"<g id="__strand_part_1" data-n="1"><g id="face">"#));
        assert!(text.contains(r#"<g id="__strand_part_0" data-n="0"><rect id="needle""#));
        assert!(!text.contains("__strand_part_2"), "an id the file lacks");
        assert!(usvg::roxmltree::Document::parse(&text).is_ok());
        // Nested: a part inside a part.
        let nested = r#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><g id="a"><g id="b"/></g></svg>"#;
        let pieces = wrap(nested, &["b".into(), "a".into()]).unwrap();
        let text = assemble(&pieces, |_, out| out.push('>'));
        assert!(text.contains(r#"<g id="__strand_part_1"><g id="a"><g id="__strand_part_0"><g id="b"/></g></g></g>"#), "{text}");
        assert!(wrap("not xml <", &names).is_none());
    }

    #[test]
    fn a_part_turns_about_the_drawings_centre_in_its_parents_frame() {
        let doc = Doc::new(GAUGE, &["needle".into()]).unwrap();
        // The needle's parent is translated by (10, 0): the centre (10,
        // 10) is (0, 10) there.
        let f = doc.frames[0];
        assert!(
            (f.centre.0 - 0.0).abs() < 1e-4 && (f.centre.1 - 10.0).abs() < 1e-4,
            "{f:?}"
        );
        let mut out = String::new();
        let part = Part::of("needle", &[(Prop::Rotate, PropValue::Angle(90.0))]);
        doc.attrs(0, &part, &mut out);
        assert_eq!(
            out,
            " transform=\"translate(0 10) rotate(90) scale(1) translate(-0 -10)\">"
        );
    }
}
