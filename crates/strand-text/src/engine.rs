//! The synchronous text engine: parley shaping plus swash rasterisation
//! into the per-scale atlases. [`crate::TextWorker`] runs one on its own
//! thread; tests and benchmarks can drive one directly.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use parley::fontique::{Blob, Collection, CollectionOptions, GenericFamily, SourceCache};
use parley::{
    Alignment, AlignmentOptions, FontContext, FontFamily, FontStyle, FontWeight, LayoutContext,
    LineHeight, PositionedLayoutItem, StyleProperty,
};
use strand_scene::{LogicalSize, Rect, Scale};
use swash::scale::{Render, ScaleContext, Source, StrikeWith, image::Content, image::Image};
use swash::zeno::{Angle, Format, Transform, Vector};
use swash::{CacheKey, FontRef};

use crate::atlas::{AtlasConfig, AtlasUpload, CachedGlyph, GlyphAtlas, GlyphKey, PageId};
use crate::{Ellipsis, GlyphRun, PlacedGlyph, TextAlign, TextLayout, TextRequest, TextSpan};

/// Largest font size shaped, in physical pixels; larger requests are
/// shaped at this size so one value from a bad expression cannot stall the
/// worker or exhaust memory.
pub const MAX_FONT_PX: f32 = 512.0;

/// Longest text shaped, in bytes; longer text is cut at a character
/// boundary. Keeps one request from occupying the worker for long.
pub const MAX_TEXT_BYTES: usize = 64 * 1024;

/// Horizontal subpixel positions per pixel. Glyph origins are snapped to a
/// quarter pixel horizontally and a whole pixel vertically.
pub const SUBPIXEL_STEPS: u8 = 4;

/// Where fonts come from.
#[derive(Clone, Debug)]
pub struct FontConfig {
    /// Load system fonts through fontique (fontconfig directories on Linux).
    pub system_fonts: bool,
    /// Extra font files (TTF/OTF/TTC bytes) registered before any lookup.
    /// With `system_fonts` off, the generic families (`sans-serif`, …)
    /// resolve to these.
    pub fonts: Vec<Arc<Vec<u8>>>,
    pub atlas: AtlasConfig,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            system_fonts: true,
            fonts: Vec::new(),
            atlas: AtlasConfig::default(),
        }
    }
}

impl FontConfig {
    /// Only the given fonts, no system lookup: deterministic across
    /// machines.
    pub fn isolated(fonts: Vec<Arc<Vec<u8>>>) -> Self {
        Self {
            system_fonts: false,
            fonts,
            atlas: AtlasConfig::default(),
        }
    }
}

/// Shapes text and rasterises glyphs.
pub struct TextEngine {
    font_cx: FontContext,
    /// Brushes are span indices plus one (0: no span).
    layout_cx: LayoutContext<u32>,
    scale_cx: ScaleContext,
    atlas_config: AtlasConfig,
    atlases: HashMap<Scale, GlyphAtlas>,
    font_keys: HashMap<(u64, u32), CacheKey>,
    image: Image,
    clock: u64,
}

impl std::fmt::Debug for TextEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextEngine")
            .field("atlases", &self.atlases)
            .field("clock", &self.clock)
            .finish_non_exhaustive()
    }
}

impl TextEngine {
    pub fn new(config: FontConfig) -> Self {
        let mut collection = Collection::new(CollectionOptions {
            shared: false,
            system_fonts: config.system_fonts,
        });
        let mut families = Vec::new();
        for data in &config.fonts {
            let blob: Blob<u8> = Blob::new(data.clone());
            for (family, _) in collection.register_fonts(blob, None) {
                if !families.contains(&family) {
                    families.push(family);
                }
            }
        }
        if !config.system_fonts && !families.is_empty() {
            for generic in [
                GenericFamily::SansSerif,
                GenericFamily::Serif,
                GenericFamily::Monospace,
                GenericFamily::SystemUi,
                GenericFamily::UiSansSerif,
            ] {
                collection.set_generic_families(generic, families.iter().copied());
            }
        }
        Self {
            font_cx: FontContext {
                collection,
                source_cache: SourceCache::default(),
            },
            layout_cx: LayoutContext::new(),
            scale_cx: ScaleContext::new(),
            atlas_config: config.atlas,
            atlases: HashMap::new(),
            font_keys: HashMap::new(),
            image: Image::new(),
            clock: 0,
        }
    }

    /// Family names the engine can resolve.
    pub fn family_names(&mut self) -> Vec<String> {
        self.font_cx
            .collection
            .family_names()
            .map(str::to_owned)
            .collect()
    }

    /// Pages currently allocated in the atlas for `scale`.
    pub fn atlas_pages(&self, scale: Scale) -> usize {
        self.atlases.get(&scale).map_or(0, GlyphAtlas::page_count)
    }

    /// Alpha bytes of the atlas pages for `scale`.
    pub fn atlas_bytes(&self, scale: Scale) -> usize {
        self.atlases.get(&scale).map_or(0, GlyphAtlas::bytes)
    }

    /// Glyphs cached in the atlas for `scale`.
    pub fn atlas_glyphs(&self, scale: Scale) -> usize {
        self.atlases.get(&scale).map_or(0, GlyphAtlas::glyph_count)
    }

    /// Drops the atlas for `scale` (no output uses it any more). Glyphs at
    /// that scale are rasterised and uploaded again if it comes back.
    pub fn drop_scale(&mut self, scale: Scale) {
        self.atlases.remove(&scale);
    }

    /// Shapes `req.text` and rasterises any glyphs the atlas lacks.
    /// Non-finite or out-of-range sizes, widths and weights are replaced by
    /// safe values first (they can come from user expressions).
    pub fn layout(&mut self, req: &TextRequest) -> TextLayout {
        self.clock += 1;
        let now = self.clock;
        let scale = req.scale;
        let s = scale.as_f32();
        let default = strand_scene::Font::default();
        let size = req.style.font.size;
        let size = if size.is_finite() && size > 0.0 {
            size.min(MAX_FONT_PX / s)
        } else {
            default.size
        };
        let weight = req.style.font.weight.clamp(1, 1000) as f32;
        let line_height = req
            .style
            .line_height
            .filter(|l| l.is_finite() && *l > 0.0)
            .map(|l| l.min(100.0));
        let max_width = req
            .max_width
            .filter(|w| w.is_finite() && *w >= 0.0)
            .map(|w| (w * s).min(1e7));

        let mut end = req.text.len().min(MAX_TEXT_BYTES);
        while !req.text.is_char_boundary(end) {
            end -= 1;
        }
        let full = &req.text[..end];
        let base = Shape {
            family: req.style.font.family.as_str(),
            size,
            weight,
            line_height,
            scale: s,
            max_width,
            align: req.style.align,
        };
        let (text, spans) = cut(full, &req.style.spans, &[Piece::Text(0..full.len())]);
        let mut layout = base.layout(&mut self.layout_cx, &mut self.font_cx, &text, &spans);
        let mut spans = spans;
        // Truncation: `max_lines`, and `ellipsis` (one line unless
        // `max_lines` says otherwise).
        let limit = req
            .style
            .max_lines
            .map(|n| n.max(1) as usize)
            .or(req.style.ellipsis.map(|_| 1));
        if let Some(limit) = limit {
            let ellipsis = req.style.ellipsis;
            let fits = |l: &parley::Layout<u32>| {
                l.len() <= limit
                    && (ellipsis.is_none() || max_width.is_none_or(|w| l.width() <= w + 0.5))
            };
            if !fits(&layout) {
                // Text past the last allowed line can never be kept.
                let keep_end = layout
                    .lines()
                    .nth(limit - 1)
                    .map_or(text.len(), |l| l.text_range().end);
                let pieces = match ellipsis {
                    None => vec![Piece::Text(0..trim_end(&text, keep_end))],
                    Some(e) => {
                        let bounds: Vec<usize> = text
                            .char_indices()
                            .map(|(i, _)| i)
                            .chain([text.len()])
                            .collect();
                        let n = bounds.len() - 1;
                        let pieces_for = |k: usize| -> Vec<Piece> {
                            match e {
                                Ellipsis::End => vec![
                                    Piece::Text(0..trim_end(&text, bounds[k])),
                                    Piece::Ellipsis,
                                ],
                                Ellipsis::Start => vec![
                                    Piece::Ellipsis,
                                    Piece::Text(trim_start(&text, bounds[n - k])..text.len()),
                                ],
                                Ellipsis::Middle => vec![
                                    Piece::Text(0..trim_end(&text, bounds[k.div_ceil(2)])),
                                    Piece::Ellipsis,
                                    Piece::Text(trim_start(&text, bounds[n - k / 2])..text.len()),
                                ],
                            }
                        };
                        // Largest kept character count that fits.
                        let mut lo = 0;
                        let mut hi = match e {
                            Ellipsis::End => bounds.partition_point(|b| *b < keep_end),
                            _ => n,
                        }
                        .min(n.saturating_sub(1));
                        while lo < hi {
                            let mid = (lo + hi).div_ceil(2);
                            let (t, sp) = cut(&text, &spans, &pieces_for(mid));
                            let l = base.layout(&mut self.layout_cx, &mut self.font_cx, &t, &sp);
                            if fits(&l) {
                                lo = mid;
                            } else {
                                hi = mid - 1;
                            }
                        }
                        pieces_for(lo)
                    }
                };
                let (t, sp) = cut(&text, &spans, &pieces);
                layout = base.layout(&mut self.layout_cx, &mut self.font_cx, &t, &sp);
                spans = sp;
            }
        }

        let atlas = self
            .atlases
            .entry(scale)
            .or_insert_with(|| GlyphAtlas::new(scale, self.atlas_config));
        atlas.trim(now);
        let mut runs = Vec::new();
        let mut uploads = Vec::new();
        let mut leases = Vec::new();
        let mut leased: HashSet<PageId> = HashSet::new();
        let mut incomplete = false;
        let mut ink = Rect::default();
        let mut baseline = None;

        for line in layout.lines() {
            baseline.get_or_insert(line.metrics().baseline / s);
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                    continue;
                };
                let run = glyph_run.run();
                let font = run.font();
                let font_size = run.font_size();
                let synthesis = run.synthesis();
                let coords = run.normalized_coords();
                let coords_hash = {
                    let mut h = DefaultHasher::new();
                    coords.hash(&mut h);
                    h.finish()
                };
                let skew = synthesis.skew().unwrap_or(0.0);
                let brush = glyph_run.style().brush as usize;
                let color = brush
                    .checked_sub(1)
                    .and_then(|i| spans.get(i))
                    .and_then(|sp| sp.color);
                let font_id = font.data.id();
                let cache_key = *self.font_keys.entry((font_id, font.index)).or_default();
                let Some(mut font_ref) = FontRef::from_index(font.data.data(), font.index as usize)
                else {
                    continue;
                };
                font_ref.key = cache_key;
                let mut scaler = self
                    .scale_cx
                    .builder(font_ref)
                    .size(font_size)
                    .hint(false)
                    .normalized_coords(coords.iter())
                    .build();

                let mut glyphs = Vec::new();
                for g in glyph_run.positioned_glyphs() {
                    let mut px = g.x.floor();
                    let mut bucket = ((g.x - px) * SUBPIXEL_STEPS as f32).round() as u8;
                    if bucket >= SUBPIXEL_STEPS {
                        px += 1.0;
                        bucket = 0;
                    }
                    let py = g.y.round();
                    let key = GlyphKey {
                        font_id,
                        font_index: font.index,
                        glyph: g.id,
                        size_bits: font_size.to_bits(),
                        subpixel: bucket,
                        embolden: synthesis.embolden(),
                        skew: skew as i8,
                        coords: coords_hash,
                    };
                    let cached = match atlas.get(&key) {
                        Some(c) => c,
                        None => {
                            let c = rasterise(
                                &mut scaler,
                                &mut self.image,
                                atlas,
                                &mut uploads,
                                g.id,
                                bucket,
                                synthesis.embolden(),
                                skew,
                                now,
                            );
                            // An atlas allocation failure is not cached:
                            // a later request retries once pages free up.
                            let Some(c) = c else {
                                incomplete = true;
                                continue;
                            };
                            atlas.insert(key, c);
                            c
                        }
                    };
                    let Some(slot) = cached.slot else { continue };
                    let lease = atlas.touch(slot.page, now);
                    if leased.insert(slot.page) {
                        leases.push(lease);
                    }
                    let placed = PlacedGlyph {
                        x: px as i32 + cached.left,
                        y: py as i32 - cached.top,
                        slot,
                    };
                    ink = ink.union(Rect::new(placed.x, placed.y, slot.w as u32, slot.h as u32));
                    glyphs.push(placed);
                }
                if !glyphs.is_empty() {
                    runs.push(GlyphRun {
                        font_size,
                        color,
                        glyphs,
                    });
                }
            }
        }

        TextLayout {
            key: req.key,
            scale,
            size: LogicalSize::new(layout.width() / s, layout.height() / s),
            baseline: baseline.unwrap_or(0.0),
            ink,
            runs,
            uploads,
            leases,
            reset: false,
            incomplete,
            atlas_pages: Some(atlas.page_ids()),
        }
    }
}

/// CSS generic family names.
const GENERICS: [&str; 9] = [
    "serif",
    "sans-serif",
    "monospace",
    "cursive",
    "fantasy",
    "system-ui",
    "emoji",
    "math",
    "fangsong",
];

/// Words in a family name that mark a monospace face.
const MONO_WORDS: [&str; 5] = ["mono", "code", "courier", "consol", "terminal"];

/// `family` with a generic family appended when it names none, so a
/// theme's `"Inter"` on a machine without Inter falls back to the system
/// sans (whole words, not a per-glyph mix of fallback fonts): `monospace`
/// for a family whose name says it is one (`"JetBrains Mono"`, `"Fira
/// Code"`), so columns stay aligned, else `sans-serif` (decisions.md,
/// wave3-theme).
fn with_generic(family: &str) -> std::borrow::Cow<'_, str> {
    let has_generic = family.split(',').any(|f| {
        let f = f.trim().trim_matches(|c| c == '"' || c == '\'');
        GENERICS.iter().any(|g| g.eq_ignore_ascii_case(f))
    });
    if has_generic {
        return family.into();
    }
    let lower = family.to_ascii_lowercase();
    let generic = if MONO_WORDS.iter().any(|w| lower.contains(w)) {
        "monospace"
    } else {
        "sans-serif"
    };
    format!("{family}, {generic}").into()
}

/// Shaping parameters shared by every attempt at one request.
#[derive(Copy, Clone)]
struct Shape<'a> {
    family: &'a str,
    size: f32,
    weight: f32,
    line_height: Option<f32>,
    scale: f32,
    /// Physical pixels.
    max_width: Option<f32>,
    align: TextAlign,
}

impl Shape<'_> {
    fn layout(
        &self,
        layout_cx: &mut LayoutContext<u32>,
        font_cx: &mut FontContext,
        text: &str,
        spans: &[TextSpan],
    ) -> parley::Layout<u32> {
        let mut builder = layout_cx.ranged_builder(font_cx, text, self.scale, true);
        builder.push_default(StyleProperty::FontFamily(FontFamily::Source(with_generic(
            self.family,
        ))));
        builder.push_default(StyleProperty::FontSize(self.size));
        builder.push_default(StyleProperty::FontWeight(FontWeight::new(self.weight)));
        if let Some(lh) = self.line_height {
            builder.push_default(StyleProperty::LineHeight(LineHeight::FontSizeRelative(lh)));
        }
        for (i, sp) in spans.iter().enumerate() {
            let r = sp.range.clone();
            builder.push(StyleProperty::Brush(i as u32 + 1), r.clone());
            if let Some(w) = sp.weight {
                builder.push(
                    StyleProperty::FontWeight(FontWeight::new(w.clamp(1, 1000) as f32)),
                    r.clone(),
                );
            }
            if sp.italic {
                builder.push(StyleProperty::FontStyle(FontStyle::Italic), r);
            }
        }
        let mut layout: parley::Layout<u32> = builder.build(text);
        layout.break_all_lines(self.max_width);
        let alignment = match self.align {
            TextAlign::Start => Alignment::Start,
            TextAlign::Center => Alignment::Center,
            TextAlign::End => Alignment::End,
        };
        layout.align(alignment, AlignmentOptions::default());
        layout
    }
}

/// A piece of truncated text: a byte range of the source, or the "…".
#[derive(Clone, Debug)]
enum Piece {
    Text(std::ops::Range<usize>),
    Ellipsis,
}

const ELLIPSIS: &str = "\u{2026}";

/// Builds the text made of `pieces` and moves `spans` (byte ranges of
/// `text`) onto it. Span ranges are clipped and snapped to character
/// boundaries; empty ones are dropped.
fn cut(text: &str, spans: &[TextSpan], pieces: &[Piece]) -> (String, Vec<TextSpan>) {
    let mut out = String::new();
    // (source start, source end, output start) of each text piece.
    let mut map = Vec::new();
    for p in pieces {
        match p {
            Piece::Text(r) => {
                map.push((r.start, r.end, out.len()));
                out.push_str(&text[r.clone()]);
            }
            Piece::Ellipsis => out.push_str(ELLIPSIS),
        }
    }
    let snap = |mut i: usize| {
        i = i.min(text.len());
        while !text.is_char_boundary(i) {
            i -= 1;
        }
        i
    };
    let mut moved = Vec::new();
    for sp in spans {
        let (a, b) = (snap(sp.range.start), snap(sp.range.end));
        for &(s0, s1, o) in &map {
            let (x, y) = (a.max(s0), b.min(s1));
            if x < y {
                moved.push(TextSpan {
                    range: o + x - s0..o + y - s0,
                    ..sp.clone()
                });
            }
        }
    }
    (out, moved)
}

/// `end` moved back over trailing whitespace.
fn trim_end(text: &str, end: usize) -> usize {
    text[..end].trim_end().len()
}

/// `start` moved forward over leading whitespace.
fn trim_start(text: &str, start: usize) -> usize {
    text.len() - text[start..].trim_start().len()
}

#[allow(clippy::too_many_arguments)]
fn rasterise(
    scaler: &mut swash::scale::Scaler<'_>,
    image: &mut Image,
    atlas: &mut GlyphAtlas,
    uploads: &mut Vec<AtlasUpload>,
    glyph: u32,
    subpixel: u8,
    embolden: bool,
    skew: f32,
    now: u64,
) -> Option<CachedGlyph> {
    let empty = Some(CachedGlyph {
        slot: None,
        left: 0,
        top: 0,
    });
    let Ok(glyph) = u16::try_from(glyph) else {
        return empty;
    };
    let mut render = Render::new(&[Source::Outline, Source::Bitmap(StrikeWith::BestFit)]);
    render
        .format(Format::Alpha)
        .offset(Vector::new(subpixel as f32 / SUBPIXEL_STEPS as f32, 0.0));
    if embolden {
        render.embolden(0.5);
    }
    if skew != 0.0 {
        render.transform(Some(Transform::skew(
            Angle::from_degrees(skew),
            Angle::from_degrees(0.0),
        )));
    }
    image.clear();
    if !render.render_into(scaler, glyph, image) || image.content != Content::Mask {
        return empty;
    }
    let p = image.placement;
    let (Ok(w), Ok(h)) = (u16::try_from(p.width), u16::try_from(p.height)) else {
        return empty;
    };
    if w == 0 || h == 0 {
        return empty;
    }
    let slot = atlas.allocate(w, h, now)?;
    uploads.push(AtlasUpload {
        page: slot.page,
        page_size: atlas.page_size_of(slot.page),
        x: slot.x,
        y: slot.y,
        w,
        h,
        alpha: image.data.clone(),
    });
    Some(CachedGlyph {
        slot: Some(slot),
        left: p.left,
        top: p.top,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_families_fall_back_to_their_generic() {
        assert_eq!(with_generic("Inter"), "Inter, sans-serif");
        assert_eq!(
            with_generic("\"JetBrains Mono\""),
            "\"JetBrains Mono\", monospace"
        );
        assert_eq!(with_generic("Fira Code"), "Fira Code, monospace");
        assert_eq!(with_generic("Inter, serif"), "Inter, serif");
        assert_eq!(with_generic("monospace"), "monospace");
    }
}
