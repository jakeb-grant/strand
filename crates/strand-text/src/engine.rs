//! The synchronous text engine: parley shaping plus swash rasterisation
//! into the per-scale atlases. [`crate::TextWorker`] runs one on its own
//! thread; tests and benchmarks can drive one directly.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use parley::fontique::{Blob, Collection, CollectionOptions, GenericFamily, SourceCache};
use parley::{
    Alignment, AlignmentOptions, FontContext, FontFamily, FontWeight, LayoutContext, LineHeight,
    PositionedLayoutItem, StyleProperty,
};
use strand_scene::{LogicalSize, Rect, Scale};
use swash::scale::{Render, ScaleContext, Source, StrikeWith, image::Content, image::Image};
use swash::zeno::{Angle, Format, Transform, Vector};
use swash::{CacheKey, FontRef};

use crate::atlas::{AtlasConfig, AtlasUpload, CachedGlyph, GlyphAtlas, GlyphKey, PageId};
use crate::{GlyphRun, PlacedGlyph, TextAlign, TextLayout, TextRequest};

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
    layout_cx: LayoutContext<()>,
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
        let text = &req.text[..end];
        let mut builder = self
            .layout_cx
            .ranged_builder(&mut self.font_cx, text, s, true);
        builder.push_default(StyleProperty::FontFamily(FontFamily::Source(
            req.style.font.family.as_str().into(),
        )));
        builder.push_default(StyleProperty::FontSize(size));
        builder.push_default(StyleProperty::FontWeight(FontWeight::new(weight)));
        if let Some(lh) = line_height {
            builder.push_default(StyleProperty::LineHeight(LineHeight::FontSizeRelative(lh)));
        }
        let mut layout: parley::Layout<()> = builder.build(text);
        layout.break_all_lines(max_width);
        let alignment = match req.style.align {
            TextAlign::Start => Alignment::Start,
            TextAlign::Center => Alignment::Center,
            TextAlign::End => Alignment::End,
        };
        layout.align(alignment, AlignmentOptions::default());

        let atlas = self
            .atlases
            .entry(scale)
            .or_insert_with(|| GlyphAtlas::new(scale, self.atlas_config));
        atlas.trim(now);
        let mut runs = Vec::new();
        let mut uploads = Vec::new();
        let mut leases = Vec::new();
        let mut leased: HashSet<PageId> = HashSet::new();
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
                            let Some(c) = c else { continue };
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
                    runs.push(GlyphRun { font_size, glyphs });
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
        }
    }
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
