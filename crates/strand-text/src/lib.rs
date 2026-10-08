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
pub use engine::{FontConfig, MAX_FONT_PX, MAX_TEXT_BYTES, SUBPIXEL_STEPS, TextEngine};
pub use worker::{TextError, TextWorker, Waker, set_idle_hook};

use std::ops::Range;

use strand_scene::{Color, Font, LogicalSize, Rect, Scale};

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

/// Where `ellipsis:` cuts text that does not fit.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Ellipsis {
    /// `ellipsis: start`: keeps the end (`…/src/strand`).
    Start,
    /// `ellipsis: middle`: keeps both ends.
    Middle,
    /// `ellipsis: end`: keeps the start (`Firefox — Strand d…`).
    End,
}

impl Ellipsis {
    /// The value of the `ellipsis` prop: `start`, `middle` or `end`.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "start" => Some(Self::Start),
            "middle" => Some(Self::Middle),
            "end" => Some(Self::End),
            _ => None,
        }
    }
}

/// A styled byte range of the text: a fuzzy-match mark (`marks:` with
/// `mark_color:`) or a markup span. Ranges are clipped to the text and
/// snapped to character boundaries; later spans win where they overlap.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextSpan {
    pub range: Range<usize>,
    /// Overrides the weight (1–1000).
    pub weight: Option<u16>,
    pub italic: bool,
    /// Underlines these glyphs (markup `<u>` and links): the run carries
    /// the line in [`GlyphRun::underline`].
    pub underline: bool,
    /// Overrides the paint colour of these glyphs ([`GlyphRun::color`]).
    pub color: Option<Color>,
}

/// Everything that affects shaping. The node's colour is not here: it is
/// applied when painting, so a colour spring never reshapes. (Span colours
/// are, since they split glyph runs.)
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextStyle {
    pub font: Font,
    /// Line height as a multiple of the font size; `None` uses the font's
    /// metrics.
    pub line_height: Option<f32>,
    pub align: TextAlign,
    /// Cut text that does not fit `max_width` (and `max_lines`) with
    /// "…". Without `max_lines`, ellipsised text is one line.
    pub ellipsis: Option<Ellipsis>,
    /// Most lines shown; lines past it are dropped (with `ellipsis`, the
    /// last kept line ends in "…").
    pub max_lines: Option<u32>,
    pub spans: Vec<TextSpan>,
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
    /// Colour of a [`TextSpan`] covering these glyphs; `None` paints them
    /// with the node's colour.
    pub color: Option<Color>,
    pub glyphs: Vec<PlacedGlyph>,
    /// The underline of an underlined span, physical pixels relative to
    /// the layout origin; painted in the run's colour.
    pub underline: Option<Rect>,
}

/// A caret stop: a cluster boundary of the shaped text, where a text
/// cursor can sit.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct CaretStop {
    /// Byte offset into the shaped text (the request's text, unless an
    /// ellipsis cut it).
    pub byte: u32,
    /// Logical pixels from the layout's left edge.
    pub x: f32,
    /// The line it is on, from 0.
    pub line: u32,
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
    /// Every cluster boundary, in visual order per line (an `input`'s
    /// caret and selection, and a click placing the caret).
    pub carets: Vec<CaretStop>,
    /// Atlas pixels this layout introduced; apply before drawing.
    pub uploads: Vec<AtlasUpload>,
    /// Keeps the pages this layout draws from alive.
    leases: Vec<PageLease>,
    /// The worker restarted its engine (see [`TextLayout::is_reset`]).
    reset: bool,
    /// Some glyph had no atlas room (see [`TextLayout::is_incomplete`]).
    incomplete: bool,
    /// Every live page of this scale's atlas after this layout.
    atlas_pages: Option<Vec<PageId>>,
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
            carets: Vec::new(),
            uploads: Vec::new(),
            leases: Vec::new(),
            reset: false,
            incomplete: false,
            atlas_pages: None,
        }
    }

    /// The empty reply the worker sends after recovering from a panicking
    /// request by starting a fresh engine (public so receivers can test
    /// their handling of it).
    pub fn reset(key: TextKey, scale: Scale) -> Self {
        Self {
            reset: true,
            ..Self::empty(key, scale)
        }
    }

    /// True when the worker had to restart its engine before answering:
    /// every atlas page uploaded before is gone. The receiver must drop
    /// its atlas mirror and every layout it holds (their glyphs point at
    /// those pages) and request its text again. Uploads of layouts that
    /// arrive after this one belong to the new engine.
    pub fn is_reset(&self) -> bool {
        self.reset
    }

    /// True when glyphs were left out because the atlas had no room (every
    /// page leased, or the byte budget spent). Asking again once other
    /// layouts are dropped can complete it.
    pub fn is_incomplete(&self) -> bool {
        self.incomplete
    }

    /// Every page of this layout's scale that the worker's atlas still
    /// holds after producing it (`None` when unknown, as for empty and
    /// reset replies). A mirror applies the uploads, then drops its other
    /// pages of that scale: they were trimmed or reset.
    pub fn atlas_pages(&self) -> Option<&[PageId]> {
        self.atlas_pages.as_deref()
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
