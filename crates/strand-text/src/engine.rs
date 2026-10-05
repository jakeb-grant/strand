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

    /// Glyphs cached in the atlas for `scale`.
    pub fn atlas_glyphs(&self, scale: Scale) -> usize {
        self.atlases.get(&scale).map_or(0, GlyphAtlas::glyph_count)
    }

    /// Shapes `req.text` and rasterises any glyphs the atlas lacks.
    pub fn layout(&mut self, req: &TextRequest) -> TextLayout {
        self.clock += 1;
        let now = self.clock;
        let scale = req.scale;
        let s = scale.as_f32();

        let mut builder = self
            .layout_cx
            .ranged_builder(&mut self.font_cx, &req.text, s, true);
        builder.push_default(StyleProperty::FontFamily(FontFamily::Source(
            req.style.font.family.as_str().into(),
        )));
        builder.push_default(StyleProperty::FontSize(req.style.font.size));
        builder.push_default(StyleProperty::FontWeight(FontWeight::new(
            req.style.font.weight as f32,
        )));
        if let Some(lh) = req.style.line_height {
            builder.push_default(StyleProperty::LineHeight(LineHeight::FontSizeRelative(lh)));
        }
        let mut layout: parley::Layout<()> = builder.build(&req.text);
        layout.break_all_lines(req.max_width.map(|w| w * s));
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
) -> CachedGlyph {
    let empty = CachedGlyph {
        slot: None,
        left: 0,
        top: 0,
    };
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
    let Some(slot) = atlas.allocate(w, h, now) else {
        return empty;
    };
    uploads.push(AtlasUpload {
        page: slot.page,
        page_size: atlas_page_size(atlas),
        x: slot.x,
        y: slot.y,
        w,
        h,
        alpha: image.data.clone(),
    });
    CachedGlyph {
        slot: Some(slot),
        left: p.left,
        top: p.top,
    }
}

fn atlas_page_size(atlas: &GlyphAtlas) -> u16 {
    atlas.page_size()
}
