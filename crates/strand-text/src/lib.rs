//! Text worker thread.
//!
//! parley shapes text off the render thread; swash rasterises into LRU glyph
//! atlases, one per output scale, so mixed-DPI setups stay sharp.
//!
//! The render thread sends [`TextRequest`]s and receives [`TextLayout`]s
//! over channels ([`TextWorker`]); it keeps drawing the last layout it has
//! until a new one arrives. Every layout carries the [`AtlasUpload`]s for
//! glyphs rasterised while producing it; the receiver must apply them to
//! its atlas mirror in arrival order, even for layouts it then discards.
//!
//! See `docs/design.md`, "Rendering, performance and memory budget".

mod atlas;
mod engine;
mod worker;

pub use atlas::{AtlasConfig, AtlasSlot, AtlasUpload, MAX_PAGE_SIZE, PageId, PageLease};
pub use engine::{FontConfig, MAX_FONT_PX, SUBPIXEL_STEPS, TextEngine};
pub use worker::{TextError, TextWorker, Waker};

use strand_scene::{Font, LogicalSize, Rect, Scale};

/// Identifies a request; echoed in the layout so the requester can match
/// responses and drop stale ones.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TextKey(pub u64);

/// Horizontal alignment of lines within the layout width.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum TextAlign {
    #[default]
    Start,
    Center,
    End,
}

/// Everything that affects shaping. Colour is not here: it is applied when
/// painting, so a colour spring never reshapes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextStyle {
    pub font: Font,
    /// Line height as a multiple of the font size; `None` uses the font's
    /// metrics.
    pub line_height: Option<f32>,
    pub align: TextAlign,
}

/// A request to shape `text`.
#[derive(Clone, Debug, PartialEq)]
pub struct TextRequest {
    pub key: TextKey,
    pub text: String,
    pub style: TextStyle,
    /// Wrap width in logical pixels; `None` never wraps.
    pub max_width: Option<f32>,
    pub scale: Scale,
}

/// A glyph placed in physical pixels: `(x, y)` is the top-left of its mask
/// relative to the layout origin, which the painter must put on a whole
/// physical pixel.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PlacedGlyph {
    pub x: i32,
    pub y: i32,
    pub slot: AtlasSlot,
}

/// Glyphs of one font at one size.
#[derive(Clone, Debug, PartialEq)]
pub struct GlyphRun {
    /// Pixel size the glyphs were rasterised at (physical).
    pub font_size: f32,
    pub glyphs: Vec<PlacedGlyph>,
}

/// A shaped, rasterised paragraph.
#[derive(Clone, Debug)]
pub struct TextLayout {
    pub key: TextKey,
    /// The scale the glyphs were rasterised for.
    pub scale: Scale,
    /// Layout box in logical pixels (widest line × total line height).
    pub size: LogicalSize,
    /// First baseline, logical pixels from the top.
    pub baseline: f32,
    /// Union of glyph masks in physical pixels relative to the origin; may
    /// extend past `size` (overhangs).
    pub ink: Rect,
    pub runs: Vec<GlyphRun>,
    /// Atlas pixels this layout introduced; apply before drawing.
    pub uploads: Vec<AtlasUpload>,
    /// Keeps the pages this layout draws from alive.
    leases: Vec<PageLease>,
}

impl TextLayout {
    /// A layout with no glyphs (the reply when shaping failed).
    pub fn empty(key: TextKey, scale: Scale) -> Self {
        Self {
            key,
            scale,
            size: LogicalSize::default(),
            baseline: 0.0,
            ink: Rect::default(),
            runs: Vec::new(),
            uploads: Vec::new(),
            leases: Vec::new(),
        }
    }

    /// Number of atlas pages this layout keeps alive.
    pub fn pages_leased(&self) -> usize {
        self.leases.len()
    }

    /// Iterates every placed glyph.
    pub fn glyphs(&self) -> impl Iterator<Item = &PlacedGlyph> {
        self.runs.iter().flat_map(|r| r.glyphs.iter())
    }
}

/// Path of the vendored test font (Liberation Sans, SIL OFL 1.1) in the
/// source tree. Tests and benches load it to stay independent of the
/// fonts installed on the machine.
pub fn test_font_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/LiberationSans-Regular.ttf")
}

/// Family name of [`test_font_path`].
pub const TEST_FONT_FAMILY: &str = "Liberation Sans";
