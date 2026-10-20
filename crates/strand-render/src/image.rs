//! `image` and `icon`: decoded at drawn size into a 6 MB LRU (design.md,
//! "Rendering, performance and memory budget"), off the render thread
//! when a worker runs.
//!
//! An `icon` names an icon of the freedesktop icon theme
//! (`window-close-symbolic`), looked up first in the config's `icons/`
//! folder ([`strand_icons::set_config_icons`]); a symbolic icon draws as
//! a mask in the node's `color` (a config file only when its own name
//! ends in `-symbolic`). An `image` takes a file path (PNG, JPEG, SVG, GIF or
//! WebP; `~/` and `file://` work) or, when its source has no path, an
//! icon name as well (`image h.app.icon`, `image item.icon`). A relative
//! path in a config is under the config directory: the compiler makes it
//! absolute before it reaches render (`check::assets::local_source`), so
//! one that arrives relative is read against the process's directory as
//! given. Every
//! decode is at the box's physical size, so the cache holds what is
//! drawn, never a full-resolution original.
//!
//! (M4) An animated GIF, APNG or WebP plays (design.md: "frames
//! streamed, not cached whole"): each decode carries the file's
//! [`Timeline`], render asks for the frame the node's time shows
//! ([`ImageKey::frame`]) on a clock at the timeline's tick
//! ([`ImageStore::frame_rate`]), and a [`Players`] entry per image
//! decodes frames one after another from the compressed file. At most
//! two frames per image and size are kept (the one drawn and the next).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Instant;

use strand_scene::SurfaceId;
use strand_text::HookGate;

pub use crate::media::animated::{Players, Timeline};
use vello_cpu::Pixmap;
use vello_cpu::color::PremulRgba8;

/// Bytes of decoded pixels kept (design.md: "Images decode at drawn size
/// into a 32 MB LRU"): about 900 icons at 96 px, so a launcher of a
/// desktop's apps at 2× scrolled through and opened again decodes none
/// twice (decisions.md, m5-speed-caches).
pub const IMAGE_CACHE_BYTES: usize = 32 << 20;

/// Failed loads remembered (so a missing icon is not looked up every
/// frame); the oldest are forgotten past this.
const MAX_FAILED: usize = 512;

/// Largest file read, bytes.
const MAX_FILE_BYTES: u64 = 64 << 20;

/// Peak bytes of source pixels one decode may hold: what a decode at
/// full resolution may take (16 Mpx of RGBA). Most decodes hold far
/// less: a JPEG is decoded with its IDCT scaled to the nearest 1/2, 1/4
/// or 1/8 at or above the drawn size, and a non-interlaced PNG is reduced
/// row by row as it is decoded, so a 24 Mpx photo drawn as album art
/// never holds more than about four times the drawn size.
pub const MAX_DECODE_BYTES: usize = 64 << 20;

/// Largest source decoded at full resolution (an interlaced PNG, a JPEG
/// drawn near its own size), in pixels.
const MAX_SOURCE_PIXELS: usize = MAX_DECODE_BYTES / 4;

/// Largest source reduced as it is decoded (a non-interlaced PNG), in
/// pixels: only one row is held at full resolution.
const MAX_STREAMED_PIXELS: usize = 1 << 30;

/// Largest drawn size of one image, pixels per side.
const MAX_SIDE: u32 = 4096;

/// How an image fills its box (`fit: cover | contain | fill | none`).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum Fit {
    /// Whole, as large as fits, centred (the default).
    #[default]
    Contain,
    /// Covers the box, cropped to it, centred.
    Cover,
    /// Stretched to the box.
    Fill,
    /// At its own size (an SVG's or icon's at the box's scale), centred
    /// and cropped.
    None,
}

impl Fit {
    /// The keyword `fit:` writes it as.
    pub fn name(self) -> &'static str {
        match self {
            Fit::Contain => "contain",
            Fit::Cover => "cover",
            Fit::Fill => "fill",
            Fit::None => "none",
        }
    }

    pub fn from_name(n: &str) -> Option<Self> {
        Some(match n {
            "contain" => Fit::Contain,
            "cover" => Fit::Cover,
            "fill" => Fit::Fill,
            "none" => Fit::None,
            _ => return None,
        })
    }
}

/// One decode: a source at a drawn size.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ImageKey {
    /// An icon name, a path or a `file://` URI.
    pub source: String,
    /// From an `icon` node: always an icon name.
    pub icon: bool,
    /// The drawn size, physical pixels.
    pub w: u32,
    pub h: u32,
    pub fit: Fit,
    /// The integer scale icon lookup asks the theme for (`@2x` dirs).
    pub scale: u16,
    /// (M4) Which frame of an animated image (0 for a still one, and an
    /// animation's first).
    pub frame: u32,
}

impl ImageKey {
    /// The key of the same image's first frame: what its frames share.
    pub fn first(&self) -> ImageKey {
        ImageKey {
            frame: 0,
            ..self.clone()
        }
    }
}

/// A decoded image at its drawn size: premultiplied, red and blue swapped
/// as the raster draws (see `raster`).
#[derive(Clone, Debug)]
pub struct Decoded {
    pub pixmap: Arc<Pixmap>,
    /// A symbolic icon: drawn as a mask in the node's colour.
    pub symbolic: bool,
    /// The source's size (as decoded) and the fit it was placed by, so
    /// a decode standing in for another box size is placed again by the
    /// fit rather than stretched (see [`Decoded::placed_in`]).
    pub source: (f64, f64),
    pub fit: Fit,
    /// (M4) An animated image's frames in time (`None`: a still image).
    pub anim: Option<Arc<Timeline>>,
}

impl Decoded {
    /// Where the whole pixmap goes so that its content lands where the
    /// fit places the source in a `w × h` box at `(x, y)`: the same rect
    /// when the box is the decode's own size.
    pub fn placed_in(&self, x: f64, y: f64, w: f64, h: f64) -> (f64, f64, f64, f64) {
        self.placed_for(x, y, w, h, self.fit)
    }

    /// (stand-in-fit) Where the whole pixmap goes so that the source it
    /// holds (placed in it by its own fit) lands where `fit`, the
    /// drawing node's, places the source in a `w × h` box at `(x, y)`.
    pub fn placed_for(&self, x: f64, y: f64, w: f64, h: f64, fit: Fit) -> (f64, f64, f64, f64) {
        let (pw, ph) = (self.pixmap.width() as f64, self.pixmap.height() as f64);
        let (sw, sh) = self.source;
        if !(sw > 0.0 && sh > 0.0 && pw > 0.0 && ph > 0.0) {
            return (x, y, w, h);
        }
        let (oxo, oyo, dwo, dho) = placement(sw, sh, pw, ph, self.fit);
        let (oxn, oyn, dwn, dhn) = placement(sw, sh, w, h, fit);
        if !(dwo > 0.0 && dho > 0.0) {
            return (x, y, w, h);
        }
        let (kx, ky) = (dwn / dwo, dhn / dho);
        (x + oxn - oxo * kx, y + oyn - oyo * ky, pw * kx, ph * ky)
    }

    /// (stand-in-fit) Drawn by [`Decoded::placed_for`] into a `w × h` box
    /// at the origin: the part of the box it has source pixels for (the
    /// pixmap where it holds the source; edges `[x0, y0, x1, y1]`), and
    /// how far, at most, what `fit` shows of the source in the box lies
    /// past it (box pixels). A decode for a box of another shape holds
    /// another crop of the source (`cover`), and past what it holds the
    /// raster would pad its edge rows out.
    pub fn holds(&self, w: f64, h: f64, fit: Fit) -> ([f64; 4], f64) {
        let (pw, ph) = (self.pixmap.width() as f64, self.pixmap.height() as f64);
        let (sw, sh) = self.source;
        let all = [0.0, 0.0, w, h];
        if !(sw > 0.0 && sh > 0.0 && pw > 0.0 && ph > 0.0 && w > 0.0 && h > 0.0) {
            return (all, 0.0);
        }
        let (dx, dy, dw, dh) = self.placed_for(0.0, 0.0, w, h, fit);
        let (ox, oy, sdw, sdh) = placement(sw, sh, pw, ph, self.fit);
        let (kx, ky) = (dw / pw, dh / ph);
        let edges = |x: f64, y: f64, w: f64, h: f64| [x, y, x + w, y + h];
        let meet = |a: [f64; 4], b: [f64; 4]| {
            [
                a[0].max(b[0]),
                a[1].max(b[1]),
                a[2].min(b[2]),
                a[3].min(b[3]),
            ]
        };
        let held = meet(
            edges(dx, dy, dw, dh),
            edges(dx + ox * kx, dy + oy * ky, sdw * kx, sdh * ky),
        );
        let (nx, ny, ndw, ndh) = placement(sw, sh, w, h, fit);
        let need = meet(edges(nx, ny, ndw, ndh), all);
        if need[2] <= need[0] || need[3] <= need[1] {
            return (held, 0.0);
        }
        let bare = [
            held[0] - need[0],
            held[1] - need[1],
            need[2] - held[2],
            need[3] - held[3],
        ]
        .into_iter()
        .fold(0.0f64, f64::max);
        (held, bare)
    }
}

/// (stand-in-fit) How far a stand-in may leave what its box shows bare
/// and still stand in, buffer pixels: the pixel a box's antialiased edge
/// already blends. With none holding that much the box's own size is
/// decoded at once, a size spring or not ([`ImageStore::want`]); the
/// best is drawn until it arrives (inline: the same frame).
pub const STAND_IN_BARE: f64 = 1.0;

/// Why an image could not be shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageError {
    /// No icon of that name in the theme, or no such file.
    NotFound(String),
    /// The file could not be read.
    Io(String),
    /// The file is not an image this decodes, or is broken.
    Decode(String),
    /// Larger than this decodes.
    TooLarge,
}

impl std::fmt::Display for ImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(s) => write!(f, "image not found: {s}"),
            Self::Io(e) => write!(f, "image could not be read: {e}"),
            Self::Decode(e) => write!(f, "image could not be decoded: {e}"),
            Self::TooLarge => f.write_str("image too large"),
        }
    }
}

impl std::error::Error for ImageError {}

/// Where icons come from: the theme to look in.
#[derive(Clone, Debug, Default)]
pub struct IconTheme {
    /// A theme by name; `None` is the desktop's
    /// ([`strand_icons::system_theme`], read again after the icon caches
    /// are invalidated).
    named: Option<String>,
}

impl IconTheme {
    /// This theme (falling back to hicolor, as every lookup does).
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            named: Some(name.into()),
        }
    }

    /// The desktop's theme: `$STRAND_ICON_THEME`, else
    /// `gtk-icon-theme-name` in `$XDG_CONFIG_HOME/gtk-3.0/settings.ini`
    /// (or `gtk-4.0`), else Adwaita.
    pub fn system() -> Self {
        Self::default()
    }

    /// The theme's name.
    pub fn name(&self) -> String {
        match &self.named {
            Some(n) => n.clone(),
            None => strand_icons::system_theme(),
        }
    }
}

/// Whether `key` is looked up in the icon theme (an `icon`, or an
/// `image` whose source is no path): what an icon theme change
/// invalidates.
/// The rule is [`strand_scene::source_is_file`], the one the compiler
/// resolves a relative path against the config directory by.
pub fn is_icon(key: &ImageKey) -> bool {
    key.icon || !strand_scene::source_is_file(&key.source)
}

/// Resolves `key`'s source to a file.
fn resolve(key: &ImageKey, theme: &IconTheme) -> Result<PathBuf, ImageError> {
    let src = key.source.trim();
    if !is_icon(key) {
        let p = local_path(src);
        return if p.is_file() {
            Ok(p)
        } else {
            Err(ImageError::NotFound(src.into()))
        };
    }
    if src.is_empty() || src.contains('/') {
        return Err(ImageError::NotFound(src.into()));
    }
    let size = (key.w.max(key.h) as f32 / key.scale.max(1) as f32).round() as u16;
    let theme = theme.name();
    strand_icons::resolve(src, size.max(1), key.scale.max(1), Some(&theme))
        .ok_or_else(|| ImageError::NotFound(src.into()))
}

/// The file a local source names, as [`resolve`] and [`read_local`] read
/// it: a `file://` URL decoded, `~/` under the home directory, anything
/// else as written (the compiler made a config's relative path absolute).
/// What an edited file the watcher reports is matched against
/// ([`ImageStore::invalidate_files`]).
pub(crate) fn local_path(src: &str) -> PathBuf {
    let src = src.trim();
    if let Some(rest) = src.strip_prefix("file://") {
        PathBuf::from(percent_decode(rest))
    } else if let Some(rest) = src.strip_prefix("~/") {
        match std::env::var_os("HOME") {
            Some(h) => PathBuf::from(h).join(rest),
            None => PathBuf::from(src),
        }
    } else {
        PathBuf::from(src)
    }
}

/// (M4) A local file's bytes, for an `svg`'s or `lottie`'s source: a
/// path, a `file://` URL or a `~/` path, at most `max` bytes.
pub(crate) fn read_local(src: &str, max: u64) -> Result<Vec<u8>, ImageError> {
    let src = src.trim();
    let path = local_path(src);
    if !path.is_file() {
        return Err(ImageError::NotFound(src.into()));
    }
    read_capped(&path, max)
}

/// (m4-audit) A regular file's bytes, at most `max`: the length is checked
/// on the open file and the read itself stops past `max`, so a procfs or
/// sysfs file (a regular file whose size reads 0, `/proc/self/pagemap`
/// hundreds of GiB long) or one growing after the stat is refused with
/// [`ImageError::TooLarge`] instead of read to its end.
fn read_capped(path: &Path, max: u64) -> Result<Vec<u8>, ImageError> {
    use std::io::Read as _;
    let io = |e: std::io::Error| ImageError::Io(e.to_string());
    // Refused before opening: a FIFO would block the open.
    if !std::fs::metadata(path).map_err(io)?.is_file() {
        return Err(ImageError::NotFound(path.display().to_string()));
    }
    let f = std::fs::File::open(path).map_err(io)?;
    let meta = f.metadata().map_err(io)?;
    if !meta.is_file() {
        return Err(ImageError::NotFound(path.display().to_string()));
    }
    if meta.len() > max {
        return Err(ImageError::TooLarge);
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    f.take(max + 1).read_to_end(&mut bytes).map_err(io)?;
    if bytes.len() as u64 > max {
        return Err(ImageError::TooLarge);
    }
    Ok(bytes)
}

/// The names an icon lookup tries ([`strand_icons::candidates`]: the
/// name, its other `-symbolic` variant, then its generic fallbacks).
pub use strand_icons::candidates as icon_candidates;

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(hex) = s.get(i + 1..i + 3)
            && let Ok(v) = u8::from_str_radix(hex, 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A decoded source: straight or premultiplied RGBA rows.
struct Raster {
    w: u32,
    h: u32,
    /// Premultiplied RGBA8.
    rgba: Vec<u8>,
    /// (png-aspect) The source's size in this raster's pixels: `(w, h)`,
    /// but less for a source reduced by a factor that does not divide a
    /// side, whose last column or row then holds a part block (a 60 × 40
    /// PNG reduced by 3 is 20 × 14 pixels holding 20 × 13⅓ of source).
    /// Placement and resampling read this, so the content keeps the
    /// source's aspect.
    extent: (f64, f64),
}

impl Raster {
    fn new(w: u32, h: u32, rgba: Vec<u8>) -> Raster {
        Raster {
            w,
            h,
            rgba,
            extent: (w as f64, h as f64),
        }
    }
}

/// Loads `key`: finds the file, decodes it and fits it into its drawn
/// size. An animated image's frame is decoded from its start (see
/// [`load_with`], which keeps the decoder between frames).
pub fn load(key: &ImageKey, theme: &IconTheme) -> Result<Decoded, ImageError> {
    load_with(key, theme, &mut Players::default())
}

/// [`load`], an animated image's frames streamed by `players`: a frame
/// after the last one asked for decodes only the frames in between.
pub fn load_with(
    key: &ImageKey,
    theme: &IconTheme,
    players: &mut Players,
) -> Result<Decoded, ImageError> {
    let (w, h) = (key.w.clamp(1, MAX_SIDE), key.h.clamp(1, MAX_SIDE));
    let path = resolve(key, theme)?;
    // A file of the config's `icons/` folder is recoloured by its own
    // name alone (the owner's ruling of 2026-10-14: `wifi-symbolic.svg`
    // is, `wifi.svg` is drawn as authored whatever name found it).
    let own = is_icon(key)
        && strand_icons::config_icons().is_some_and(|d| path.parent() == Some(d.as_path()));
    let symbolic = (!own && key.source.ends_with("-symbolic"))
        || path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.ends_with("-symbolic"));
    let read = || read_capped(&path, MAX_FILE_BYTES);
    // A file already playing is not read again.
    let mut data = None;
    let player = match players.get(key, || {
        let d = read()?;
        let f = crate::media::animated::format(&d);
        let out = f.map(|f| (Arc::<[u8]>::from(&d[..]), f));
        data = Some(d);
        out.ok_or_else(|| ImageError::Decode("not animated".into()))
    }) {
        Ok(p) => Some(p),
        Err(e) if data.is_none() => return Err(e),
        Err(_) => None,
    };
    if let Some(p) = player {
        let anim = p.timeline.clone();
        let (sw, sh) = p.size();
        let frame = p.frame(key.frame)?;
        let mut rgba = Vec::with_capacity(frame.len());
        for px in frame.chunks_exact(4) {
            let a = px[3] as u32;
            let pm = |c: u8| ((c as u32 * a + 127) / 255) as u8;
            rgba.extend_from_slice(&[pm(px[0]), pm(px[1]), pm(px[2]), px[3]]);
        }
        let src = Raster::new(sw, sh, rgba);
        return Ok(Decoded {
            pixmap: Arc::new(to_pixmap(&fit_raster(&src, w, h, key.fit))),
            symbolic,
            source: (sw as f64, sh as f64),
            fit: key.fit,
            anim,
        });
    }
    let data = match data {
        Some(d) => d,
        None => read()?,
    };
    let svg = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("svg"))
        || (data.starts_with(b"<") && !data.starts_with(b"\x89PNG"));
    let (out, source) = if svg {
        render_svg(&data, w, h, key.fit)?
    } else {
        let src = if data.starts_with(b"\x89PNG") {
            decode_png(&data, (w, h), key.fit)?
        } else if data.starts_with(&[0xff, 0xd8]) {
            decode_jpeg(&data, (w, h), key.fit)?
        } else {
            return Err(ImageError::Decode(
                "not a PNG, JPEG, SVG, GIF or WebP".into(),
            ));
        };
        (fit_raster(&src, w, h, key.fit), src.extent)
    };
    Ok(Decoded {
        pixmap: Arc::new(to_pixmap(&out)),
        symbolic,
        source,
        fit: key.fit,
        anim: None,
    })
}

/// How much of a `sw × sh` source a `w × h` box by `fit` needs: the
/// factor (at most 1) the source is drawn at.
fn needed_scale(sw: u32, sh: u32, (w, h): (u32, u32), fit: Fit) -> f64 {
    let (sw, sh) = (sw.max(1) as f64, sh.max(1) as f64);
    let (_, _, dw, dh) = placement(sw, sh, w as f64, h as f64, fit);
    (dw / sw).max(dh / sh).clamp(1e-6, 1.0)
}

/// Appends one pixel of `color` (8-bit samples) as premultiplied RGBA.
fn push_rgba(out: &mut Vec<u8>, color: png::ColorType, p: &[u8]) {
    let px = match color {
        png::ColorType::Rgba => [p[0], p[1], p[2], p[3]],
        png::ColorType::Rgb => [p[0], p[1], p[2], 255],
        png::ColorType::GrayscaleAlpha => [p[0], p[0], p[0], p[1]],
        _ => [p[0], p[0], p[0], 255],
    };
    let a = px[3] as u32;
    let pm = |c: u8| ((c as u32 * a + 127) / 255) as u8;
    out.extend_from_slice(&[pm(px[0]), pm(px[1]), pm(px[2]), px[3]]);
}

/// Decodes a PNG for a `want` box by `fit`. A non-interlaced one drawn
/// at half its size or less is reduced as its rows arrive (each block of
/// `f × f` source pixels averaged, `f` the whole reduction it allows),
/// so only one source row is ever held at full resolution.
fn decode_png(data: &[u8], want: (u32, u32), fit: Fit) -> Result<Raster, ImageError> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(data));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec
        .read_info()
        .map_err(|e| ImageError::Decode(e.to_string()))?;
    let info = reader.info();
    let (sw, sh, interlaced) = (info.width, info.height, info.interlaced);
    let pixels = (sw as usize).saturating_mul(sh as usize);
    let (color, _) = reader.output_color_type();
    if color == png::ColorType::Indexed {
        return Err(ImageError::Decode("indexed PNG not expanded".into()));
    }
    let channels = color.samples();
    let f = (1.0 / needed_scale(sw, sh, want, fit)).floor().max(1.0) as u32;
    if interlaced || f == 1 {
        if pixels > MAX_SOURCE_PIXELS {
            return Err(ImageError::TooLarge);
        }
        let size = reader
            .output_buffer_size()
            .ok_or_else(|| ImageError::Decode("PNG too large".into()))?;
        let mut buf = vec![0; size];
        let frame = reader
            .next_frame(&mut buf)
            .map_err(|e| ImageError::Decode(e.to_string()))?;
        let (w, h) = (frame.width, frame.height);
        let n = (w * h) as usize;
        let mut rgba = Vec::with_capacity(n * 4);
        for p in buf[..frame.buffer_size()].chunks_exact(channels).take(n) {
            push_rgba(&mut rgba, frame.color_type, p);
        }
        if rgba.len() != n * 4 {
            return Err(ImageError::Decode("short PNG".into()));
        }
        return Ok(Raster::new(w, h, rgba));
    }
    if pixels > MAX_STREAMED_PIXELS {
        return Err(ImageError::TooLarge);
    }
    let (rw, rh) = (sw.div_ceil(f), sh.div_ceil(f));
    let mut out = Vec::with_capacity(rw as usize * rh as usize * 4);
    // A block holds at most every source pixel (`MAX_STREAMED_PIXELS`),
    // so its sums fit `u64` where `u32` overflows past `f` ≈ 4100.
    let mut sums = vec![0u64; rw as usize * 4];
    let mut row_px = Vec::with_capacity(sw as usize * 4);
    let mut rows_in_band = 0u32;
    let mut y = 0u32;
    while y < sh {
        let row = reader
            .next_row()
            .map_err(|e| ImageError::Decode(e.to_string()))?
            .ok_or_else(|| ImageError::Decode("short PNG".into()))?;
        row_px.clear();
        for p in row.data().chunks_exact(channels).take(sw as usize) {
            push_rgba(&mut row_px, color, p);
        }
        if row_px.len() != sw as usize * 4 {
            return Err(ImageError::Decode("short PNG row".into()));
        }
        for (x, p) in row_px.chunks_exact(4).enumerate() {
            let b = (x / f as usize) * 4;
            for k in 0..4 {
                sums[b + k] += p[k] as u64;
            }
        }
        rows_in_band += 1;
        y += 1;
        if rows_in_band == f || y == sh {
            for bx in 0..rw {
                let cols = f.min(sw - bx * f);
                let n = cols as u64 * rows_in_band as u64;
                let b = bx as usize * 4;
                for k in 0..4 {
                    out.push(((sums[b + k] + n / 2) / n) as u8);
                }
            }
            sums.iter_mut().for_each(|s| *s = 0);
            rows_in_band = 0;
        }
    }
    // The last block of a side `f` does not divide holds less than `f`
    // source pixels: its pixel covers that fraction of one.
    Ok(Raster {
        w: rw,
        h: rh,
        rgba: out,
        extent: (sw as f64 / f as f64, sh as f64 / f as f64),
    })
}

/// Decodes a JPEG for a `want` box by `fit`, its IDCT scaled to the
/// smallest of 1, 1/2, 1/4 and 1/8 still at or above the drawn size.
fn decode_jpeg(data: &[u8], want: (u32, u32), fit: Fit) -> Result<Raster, ImageError> {
    std::panic::catch_unwind(|| decode_jpeg_inner(data, want, fit))
        .unwrap_or_else(|_| Err(ImageError::Decode("JPEG decoder panicked".into())))
}

fn decode_jpeg_inner(data: &[u8], want: (u32, u32), fit: Fit) -> Result<Raster, ImageError> {
    use jpeg_decoder::{Decoder, PixelFormat};
    let err = |e: jpeg_decoder::Error| ImageError::Decode(e.to_string());
    let mut dec = Decoder::new(std::io::Cursor::new(data));
    dec.set_max_decoding_buffer_size(MAX_DECODE_BYTES);
    dec.read_info().map_err(err)?;
    let info = dec
        .info()
        .ok_or_else(|| ImageError::Decode("no JPEG size".into()))?;
    let (sw, sh) = (info.width as u32, info.height as u32);
    let k = needed_scale(sw, sh, want, fit);
    let req = |v: u32| ((v as f64 * k).ceil() as u32).clamp(1, u16::MAX as u32) as u16;
    let (w, h) = dec.scale(req(sw), req(sh)).map_err(err)?;
    let (w, h) = (w as u32, h as u32);
    if (w as usize).saturating_mul(h as usize) > MAX_SOURCE_PIXELS {
        return Err(ImageError::TooLarge);
    }
    let px = dec.decode().map_err(err)?;
    let n = w as usize * h as usize;
    let mut rgba = Vec::with_capacity(n * 4);
    match info.pixel_format {
        PixelFormat::L8 => {
            for &g in px.iter().take(n) {
                rgba.extend_from_slice(&[g, g, g, 255]);
            }
        }
        PixelFormat::L16 => {
            for p in px.chunks_exact(2).take(n) {
                let g = p[0];
                rgba.extend_from_slice(&[g, g, g, 255]);
            }
        }
        PixelFormat::RGB24 => {
            for p in px.chunks_exact(3).take(n) {
                rgba.extend_from_slice(&[p[0], p[1], p[2], 255]);
            }
        }
        PixelFormat::CMYK32 => {
            for p in px.chunks_exact(4).take(n) {
                let ink = |c: u8| ((255 - c as u32) * (255 - p[3] as u32) / 255) as u8;
                rgba.extend_from_slice(&[ink(p[0]), ink(p[1]), ink(p[2]), 255]);
            }
        }
    }
    if rgba.len() != n * 4 {
        return Err(ImageError::Decode("short JPEG".into()));
    }
    // The IDCT scales by `d / 8` and rounds each side up apart
    // (jpeg-decoder's `update_idct_size`), so the last column or row
    // holds a part block: the source's extent is its size times `d / 8`.
    let extent = [1u32, 2, 4, 8]
        .into_iter()
        .find(|d| (sw * d).div_ceil(8) == w && (sh * d).div_ceil(8) == h)
        .map_or((w as f64, h as f64), |d| {
            (sw as f64 * d as f64 / 8.0, sh as f64 * d as f64 / 8.0)
        });
    Ok(Raster { w, h, rgba, extent })
}

/// Where a `sw × sh` source goes in a `w × h` box: its scaled size and
/// offset (logical to the box, may be negative when cropped).
pub(crate) fn placement(sw: f64, sh: f64, w: f64, h: f64, fit: Fit) -> (f64, f64, f64, f64) {
    let (dw, dh) = match fit {
        Fit::Fill => (w, h),
        Fit::None => (sw, sh),
        Fit::Contain | Fit::Cover => {
            let k = if fit == Fit::Contain {
                (w / sw).min(h / sh)
            } else {
                (w / sw).max(h / sh)
            };
            (sw * k, sh * k)
        }
    };
    ((w - dw) / 2.0, (h - dh) / 2.0, dw, dh)
}

/// Resamples a premultiplied source into a `w × h` box by `fit`: each
/// output pixel averages the source area it covers (downscaling) or
/// interpolates bilinearly (upscaling).
fn fit_raster(src: &Raster, w: u32, h: u32, fit: Fit) -> Raster {
    // The source's extent: a part block at a reduced side's end counts
    // for its part, so the content keeps the source's aspect.
    let (sw, sh) = src.extent;
    let (ox, oy, dw, dh) = placement(sw, sh, w as f64, h as f64, fit);
    let (kx, ky) = (sw / dw.max(1e-9), sh / dh.max(1e-9));
    let mut out = vec![0u8; (w * h * 4) as usize];
    let at = |x: i64, y: i64| -> [f64; 4] {
        let x = x.clamp(0, src.w as i64 - 1) as usize;
        let y = y.clamp(0, src.h as i64 - 1) as usize;
        let i = (y * src.w as usize + x) * 4;
        let p = &src.rgba[i..i + 4];
        [p[0] as f64, p[1] as f64, p[2] as f64, p[3] as f64]
    };
    for y in 0..h {
        for x in 0..w {
            // The output pixel's square in source coordinates.
            let sx0 = (x as f64 - ox) * kx;
            let sy0 = (y as f64 - oy) * ky;
            let (sx1, sy1) = (sx0 + kx, sy0 + ky);
            if sx1 <= 0.0 || sy1 <= 0.0 || sx0 >= sw || sy0 >= sh {
                continue;
            }
            let mut acc = [0.0f64; 4];
            if kx <= 1.0 && ky <= 1.0 {
                // Upscaling: bilinear at the pixel centre.
                let cx = (sx0 + sx1) / 2.0 - 0.5;
                let cy = (sy0 + sy1) / 2.0 - 0.5;
                let (fx, fy) = (cx.floor(), cy.floor());
                let (tx, ty) = (cx - fx, cy - fy);
                let (ix, iy) = (fx as i64, fy as i64);
                let (a, b, c, d) = (
                    at(ix, iy),
                    at(ix + 1, iy),
                    at(ix, iy + 1),
                    at(ix + 1, iy + 1),
                );
                // (png-aspect) A pixel the source's edge passes through
                // fades by the part it covers, as a downscaled one does,
                // so a contained source is drawn at its placed size, not
                // grown to whole pixels at each edge.
                let cover = ((sx1.min(sw) - sx0.max(0.0)) * (sy1.min(sh) - sy0.max(0.0))
                    / (kx * ky))
                    .clamp(0.0, 1.0);
                for k in 0..4 {
                    acc[k] = ((a[k] * (1.0 - tx) + b[k] * tx) * (1.0 - ty)
                        + (c[k] * (1.0 - tx) + d[k] * tx) * ty)
                        * cover;
                }
            } else {
                // Downscaling: the area average, partial pixels weighted.
                let (x0, x1) = (sx0.max(0.0), sx1.min(sw));
                let (y0, y1) = (sy0.max(0.0), sy1.min(sh));
                let mut wsum = 0.0;
                let mut yy = y0.floor();
                while yy < y1 {
                    let wy = (yy + 1.0).min(y1) - yy.max(y0);
                    let mut xx = x0.floor();
                    while xx < x1 {
                        let wx = (xx + 1.0).min(x1) - xx.max(x0);
                        let p = at(xx as i64, yy as i64);
                        let wgt = wx * wy;
                        for k in 0..4 {
                            acc[k] += p[k] * wgt;
                        }
                        wsum += wgt;
                        xx += 1.0;
                    }
                    yy += 1.0;
                }
                // Partly covered edge pixels fade (the image's own edge).
                let cover = wsum / (kx * ky);
                if wsum > 0.0 {
                    for v in &mut acc {
                        *v = *v / wsum * cover;
                    }
                }
            }
            let i = ((y * w + x) * 4) as usize;
            let a = acc[3].round().clamp(0.0, 255.0);
            out[i + 3] = a as u8;
            for k in 0..3 {
                out[i + k] = acc[k].round().clamp(0.0, a) as u8;
            }
        }
    }
    Raster::new(w, h, out)
}

/// Renders an SVG into a `w × h` box by `fit`.
/// Renders an SVG into a `w × h` box by `fit`, with its own size.
fn render_svg(data: &[u8], w: u32, h: u32, fit: Fit) -> Result<(Raster, (f64, f64)), ImageError> {
    use resvg::{tiny_skia, usvg};
    let tree = usvg::Tree::from_data(data, &crate::media::svg::options())
        .map_err(|e| ImageError::Decode(e.to_string()))?;
    let size = tree.size();
    let (sw, sh) = (size.width() as f64, size.height() as f64);
    if !(sw > 0.0 && sh > 0.0) {
        return Err(ImageError::Decode("empty SVG".into()));
    }
    let (ox, oy, dw, dh) = placement(sw, sh, w as f64, h as f64, fit);
    let mut pm =
        tiny_skia::Pixmap::new(w, h).ok_or_else(|| ImageError::Decode("bad size".into()))?;
    let t = tiny_skia::Transform::from_row(
        (dw / sw) as f32,
        0.0,
        0.0,
        (dh / sh) as f32,
        ox as f32,
        oy as f32,
    );
    resvg::render(&tree, t, &mut pm.as_mut());
    Ok((Raster::new(w, h, pm.take()), (sw, sh)))
}

/// (M4) Decodes a PNG or JPEG held in memory (a Lottie's image asset)
/// at `want` pixels (its own size, within [`MAX_SIDE`], when `None`),
/// shrunk, its aspect kept, to at most `max_pixels`, as premultiplied
/// RGBA in vello's own order (not swapped: drawn inside a raster node, as
/// the node's own colours are).
pub(crate) fn decode_rgba(
    data: &[u8],
    want: Option<(u32, u32)>,
    max_pixels: u64,
) -> Result<Pixmap, ImageError> {
    let want = want.map(|(w, h)| {
        within(
            w.clamp(1, MAX_SIDE) as f64,
            h.clamp(1, MAX_SIDE) as f64,
            max_pixels,
        )
    });
    let ask = want.unwrap_or((MAX_SIDE, MAX_SIDE));
    let src = if data.starts_with(b"\x89PNG") {
        decode_png(data, ask, Fit::Fill)?
    } else if data.starts_with(&[0xff, 0xd8]) {
        decode_jpeg(data, ask, Fit::Fill)?
    } else {
        return Err(ImageError::Decode("not a PNG or JPEG".into()));
    };
    let (w, h) = match want {
        Some(wh) => wh,
        None => {
            let side = |v: f64| v.clamp(1.0, MAX_SIDE as f64);
            within(side(src.extent.0), side(src.extent.1), max_pixels)
        }
    };
    if u64::from(w) * u64::from(h) > max_pixels {
        return Err(ImageError::TooLarge);
    }
    let r = if (src.w, src.h) == (w, h) && src.extent == (w as f64, h as f64) {
        src
    } else {
        fit_raster(&src, w, h, Fit::Fill)
    };
    let mut pm = Pixmap::new(r.w as u16, r.h as u16);
    for (d, s) in pm.data_mut().iter_mut().zip(r.rgba.chunks_exact(4)) {
        *d = PremulRgba8 {
            r: s[0],
            g: s[1],
            b: s[2],
            a: s[3],
        };
    }
    Ok(pm)
}

/// `w × h` (a source's extent, whole or not) shrunk, its aspect kept, to
/// at most `max_pixels` (each side at least 1, so a very long, thin size
/// can stay above it). Each side is rounded, so the aspect is kept within
/// half a pixel a side; where rounding both up would pass `max_pixels`
/// the side that rounded up is rounded down instead, within a pixel.
fn within(w: f64, h: f64, max_pixels: u64) -> (u32, u32) {
    let k = (max_pixels as f64 / (w * h).max(1e-9)).sqrt().min(1.0);
    let (x, y) = (w * k, h * k);
    let fits = |a: f64, b: f64| (a.max(1.0) as u64).saturating_mul(b.max(1.0) as u64) <= max_pixels;
    let (rx, ry) = (x.round(), y.round());
    let (a, b) = if fits(rx, ry) {
        (rx, ry)
    } else if fits(x.floor(), ry) && x.fract() >= 0.5 {
        (x.floor(), ry)
    } else if fits(rx, y.floor()) {
        (rx, y.floor())
    } else {
        (x.floor(), y.floor())
    };
    (a.max(1.0) as u32, b.max(1.0) as u32)
}

/// A vello pixmap of premultiplied RGBA, red and blue swapped, claimed
/// opaque when every pixel is.
///
/// (opaque-fast-path) The claim lets vello overwrite what is under the
/// image and cull what an opaque image hides, about a ninth of a 4K
/// frame's raster time. It is exact only where the samples are the
/// pixels themselves: drawn pixel for pixel (nearest sampling, so a
/// sample is a pixel, however the pixmap is placed). A decode standing
/// in for another size, or drawn under a scale or turn, is sampled
/// smoothly, and its bilinear samples are not quite opaque (four f32
/// weights summing to a hair under 1): vello writes a paint claimed
/// opaque over what is under it in a tile wholly inside the clip and
/// blends it in a tile the clip's edge cuts, so a partial repaint,
/// clipped to the damage, differed from a full one by a level here and
/// there (image-redraw). `raster::draw` draws such a sample into a layer
/// of its own, where overwriting and blending over nothing agree, and
/// blends the layer over what is under it alike in every tile.
fn to_pixmap(r: &Raster) -> Pixmap {
    let mut pm = Pixmap::new(r.w as u16, r.h as u16);
    let mut opaque = true;
    for (d, s) in pm.data_mut().iter_mut().zip(r.rgba.chunks_exact(4)) {
        opaque &= s[3] == 255;
        *d = PremulRgba8 {
            r: s[2],
            g: s[1],
            b: s[0],
            a: s[3],
        };
    }
    pm.set_may_have_transparency(!opaque);
    pm
}

/// Decodes on a thread of its own.
/// What every image worker runs when its queue drains after a decode,
/// on its own thread, before it blocks for the next request (see
/// [`set_idle_hook`]).
static IDLE_HOOK: OnceLock<fn()> = OnceLock::new();

/// Installs the hook image workers run when their queue drains after
/// work (the first call wins), like `strand_text::set_idle_hook`:
/// `strand run` returns the allocator's freed pages there, which only
/// the worker's own thread can free. It runs inside the burst that woke
/// the worker, so it costs no wakeup of its own, and only after a
/// decode (a drain that only dropped unwanted requests owes none), at
/// most once per 5 s plus that burst's first 250 ms
/// ([`strand_text::HookGate`]); a skipped one runs once the worker has
/// been quiet for 500 ms (at most one such wake per 5 s).
pub fn set_idle_hook(hook: fn()) {
    let _ = IDLE_HOOK.set(hook);
}

/// (M4) What a media source's file job gives back: its parsed content
/// (an `svg`'s text, a `lottie`'s model and decoded images), or why not.
pub(crate) type Loaded = Result<Arc<dyn std::any::Any + Send + Sync>, String>;

/// (m4-audit) Room for one more file result. Results no source came
/// back for are not kept for ever: once `MAX_FILES_DONE` are held, those
/// older than `FILE_RESULT_STALE` go, and past `MAX_FILES_HELD` the
/// oldest. Fresh results stay, so a burst of sources first drawn
/// together (a launcher of `svg`s) is not read and parsed again because
/// the worker outran the render loop.
fn make_room(done: &mut HashMap<String, (Loaded, Instant)>, now: Instant) {
    if done.len() < MAX_FILES_DONE {
        return;
    }
    done.retain(|_, (_, at)| now.saturating_duration_since(*at) < FILE_RESULT_STALE);
    while done.len() >= MAX_FILES_HELD {
        let Some(oldest) = done
            .iter()
            .min_by_key(|(_, (_, at))| *at)
            .map(|(k, _)| k.clone())
        else {
            break;
        };
        done.remove(&oldest);
    }
}

/// (M4) A media source's file job: read and parse on the worker.
pub(crate) struct FileJob {
    key: String,
    work: Box<dyn FnOnce() -> Loaded + Send>,
}

/// One job for the worker.
enum Job {
    Image(ImageKey),
    File(FileJob),
    /// (M6) These sources' files changed: their players (decoders
    /// holding the old bytes) go ([`ImageStore::invalidate_files`]).
    Forget(Vec<String>),
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Job::Image(k) => f.debug_tuple("Image").field(k).finish(),
            Job::File(j) => f.debug_tuple("File").field(&j.key).finish(),
            Job::Forget(s) => f.debug_tuple("Forget").field(s).finish(),
        }
    }
}

/// (M4) File results kept for their sources to take before stale ones
/// are dropped.
const MAX_FILES_DONE: usize = 64;

/// (m4-audit) How long a file result waits for its source before it
/// counts as one no source came back for (its source changed while it
/// was read): a waiting source's surface flattens within a frame or so
/// of the wake.
const FILE_RESULT_STALE: std::time::Duration = std::time::Duration::from_secs(1);

/// (m4-audit) Most file results kept however fresh: past it the oldest
/// go (a burst of more fresh results than this, which no scene asks
/// for at once, is read again).
const MAX_FILES_HELD: usize = 4 * MAX_FILES_DONE;

/// (M4) File jobs in flight and done, by key.
#[derive(Default)]
struct FileLoads {
    pending: HashSet<String>,
    /// Each result with when it arrived.
    done: HashMap<String, (Loaded, Instant)>,
    /// A job finished since the render loop last looked.
    arrived: bool,
    /// (M6) Jobs in flight whose file changed after they were sent: their
    /// result is dropped on arrival, so the source asks again
    /// ([`ImageStore::invalidate_files`]).
    stale: HashSet<String>,
}

impl std::fmt::Debug for FileLoads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileLoads")
            .field("pending", &self.pending)
            .field("done", &self.done.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[derive(Debug)]
pub struct ImageWorker {
    requests: Option<Sender<Job>>,
    /// `None`: dropped undecoded, no longer wanted.
    results: Receiver<(ImageKey, Option<Result<Decoded, ImageError>>)>,
    /// The keys some live frame draws (or will once decoded).
    wanted: Arc<Mutex<HashSet<ImageKey>>>,
    /// (M4) Media sources' file jobs.
    files: Arc<Mutex<FileLoads>>,
    thread: Option<JoinHandle<()>>,
}

impl ImageWorker {
    /// Starts the worker; `waker` runs after each result is sent (the
    /// render loop's ping).
    pub fn spawn(theme: IconTheme, waker: Option<Box<dyn Fn() + Send>>) -> std::io::Result<Self> {
        let (req_tx, req_rx) = mpsc::channel::<Job>();
        let (out_tx, out_rx) = mpsc::channel();
        let wanted: Arc<Mutex<HashSet<ImageKey>>> = Arc::default();
        let still = wanted.clone();
        let files: Arc<Mutex<FileLoads>> = Arc::default();
        let loads = files.clone();
        let thread = std::thread::Builder::new()
            .name("strand-image".into())
            .spawn(move || {
                let mut gate = HookGate::default();
                // A player some live frame draws is never dropped for the
                // budget (two animations drawn together would otherwise
                // rebuild each other at every frame change).
                let live = still.clone();
                let players_live = move || {
                    let live = live.clone();
                    Players::with_live(move |k| {
                        live.lock()
                            .map(|w| w.iter().any(|x| x.first() == *k))
                            .unwrap_or(true)
                    })
                };
                let mut players = players_live();
                loop {
                    let job = match req_rx.try_recv() {
                        Ok(job) => job,
                        Err(TryRecvError::Disconnected) => return,
                        Err(TryRecvError::Empty) => {
                            // Images no frame draws any more stop playing.
                            if !players.is_empty()
                                && let Ok(w) = still.lock()
                            {
                                players.retain(|k| w.iter().any(|x| x.first() == *k));
                            }
                            // Drained: the decodes' garbage goes back
                            // before the worker blocks (rate-limited as
                            // the text worker's, `HookGate`).
                            let wait = IDLE_HOOK
                                .get()
                                .and_then(|hook| gate.drained(Instant::now(), hook));
                            let next = match wait {
                                None => req_rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                                Some(d) => req_rx.recv_timeout(d),
                            };
                            match next {
                                Ok(job) => job,
                                // Quiet: the owed hook, then block.
                                Err(RecvTimeoutError::Timeout) => {
                                    if let Some(hook) = IDLE_HOOK.get() {
                                        gate.quiet(Instant::now(), hook);
                                    }
                                    continue;
                                }
                                Err(RecvTimeoutError::Disconnected) => return,
                            }
                        }
                    };
                    let key = match job {
                        Job::Image(key) => key,
                        // (M4) A media source's file: read and parsed
                        // here, the render loop woken to take it.
                        Job::File(FileJob { key, work }) => {
                            gate.worked();
                            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                                .unwrap_or_else(|_| Err("parser panicked".into()));
                            if let Ok(mut f) = loads.lock() {
                                make_room(&mut f.done, Instant::now());
                                f.pending.remove(&key);
                                // Read before its file changed: dropped,
                                // and the wake makes its source ask again.
                                if !f.stale.remove(&key) {
                                    f.done.insert(key, (r, Instant::now()));
                                }
                                f.arrived = true;
                            }
                            if let Some(w) = &waker {
                                w();
                            }
                            continue;
                        }
                        Job::Forget(sources) => {
                            players.retain(|k| !sources.contains(&k.source));
                            continue;
                        }
                    };
                    // A request no frame wants any more (a size passed
                    // through, a node gone) is dropped undecoded, and
                    // owes no hook.
                    let want = still.lock().map(|w| w.contains(&key)).unwrap_or(true);
                    let r = if want {
                        gate.worked();
                        Some(
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                load_with(&key, &theme, &mut players)
                            }))
                            .unwrap_or_else(|_| {
                                // A decoder that panicked may be left
                                // half-way: start every image again.
                                players = players_live();
                                Err(ImageError::Decode("decoder panicked".into()))
                            }),
                        )
                    } else {
                        None
                    };
                    if out_tx.send((key, r)).is_err() {
                        return;
                    }
                    // A dropped request wakes the render loop too: a frame
                    // may have wanted it again while it was in flight (it
                    // is not asked for twice), and only the render loop
                    // can send it again (`ImageStore::poll`).
                    if let Some(w) = &waker {
                        w();
                    }
                }
            })?;
        Ok(Self {
            requests: Some(req_tx),
            results: out_rx,
            wanted,
            files,
            thread: Some(thread),
        })
    }
}

impl Drop for ImageWorker {
    fn drop(&mut self) {
        self.requests.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Decodes inline (tests, offline renders) or on a worker.
#[derive(Debug)]
pub enum ImageBackend {
    Inline(IconTheme),
    Worker(ImageWorker),
}

#[derive(Debug)]
struct Entry {
    result: Result<Decoded, ImageError>,
    bytes: usize,
    used: u64,
}

/// One source drawn by one node kind with one fit at one scale: what a
/// decode at another size of it can stand in for.
/// A source as decodes of it stand in for one another: its path or
/// icon name at a scale, at any size and fit (stand-in-fit).
type SourceKey = (String, bool, u16);

fn source_key(k: &ImageKey) -> SourceKey {
    (k.source.clone(), k.icon, k.scale)
}

/// Decoded images by key, within [`IMAGE_CACHE_BYTES`].
#[derive(Debug)]
pub struct ImageStore {
    backend: ImageBackend,
    entries: HashMap<ImageKey, Entry>,
    pending: HashSet<ImageKey>,
    bytes: usize,
    tick: u64,
    /// The keys each surface's last frame uses: never evicted for
    /// another, and what a decode that arrives repaints.
    frames: HashMap<SurfaceId, HashSet<ImageKey>>,
    /// Per source, its decodes held at any size and fit, newest last:
    /// one is drawn scaled while the one at the drawn size is not there
    /// yet (a size spring), the one that best holds what the box shows
    /// ([`ImageStore::stand_in`], stand-in-fit).
    held: HashMap<SourceKey, Vec<ImageKey>>,
    /// (stand-in-fit) Whole-source decodes in flight on the worker, asked
    /// for to stand in while a `cover` box springs ([`ImageStore::want`]):
    /// wanted though no frame draws them yet.
    wholes: HashSet<ImageKey>,
    /// Icon decodes in flight when the icon theme changed: their results
    /// are dropped (and asked for again) when they arrive.
    stale: HashSet<ImageKey>,
    /// (M4) Animated images decoded inline: their decoders between
    /// frames.
    players: Players,
    /// (M4) The timelines of the animated sources decoded, by source.
    timelines: HashMap<String, Arc<Timeline>>,
    /// (M6) Decodes of a file that changed on disk which a live frame
    /// still draws: kept and drawn until their new decode replaces them
    /// (no blank frame), and asked for again as if missing
    /// ([`ImageStore::invalidate_files`]).
    outdated: HashSet<ImageKey>,
}

impl Default for ImageStore {
    fn default() -> Self {
        Self::new(ImageBackend::Inline(IconTheme::system()))
    }
}

impl ImageStore {
    pub fn new(backend: ImageBackend) -> Self {
        Self {
            backend,
            entries: HashMap::new(),
            pending: HashSet::new(),
            bytes: 0,
            tick: 0,
            frames: HashMap::new(),
            held: HashMap::new(),
            wholes: HashSet::new(),
            stale: HashSet::new(),
            // Inline, the players are those of the frames (`want` drops
            // the others at each change): all live.
            players: Players::with_live(|_| true),
            timelines: HashMap::new(),
            outdated: HashSet::new(),
        }
    }

    /// (M4) The clock of an animated `image` node: its timeline's tick,
    /// once its first decode showed it animated, until a finite loop
    /// count has played out at the node's time (`t()`, read only then).
    /// `None` for anything else.
    pub(crate) fn frame_rate(
        &self,
        node: &crate::tree::Node,
        t: impl FnOnce() -> f32,
    ) -> Option<crate::clock::Rate> {
        if node.kind != strand_scene::NodeKind::Image || self.timelines.is_empty() {
            return None;
        }
        let source = match node.get(strand_scene::Prop::Source) {
            Some(strand_scene::PropValue::Text(s) | strand_scene::PropValue::Keyword(s)) => s,
            _ => return None,
        };
        let tl = self.timelines.get(source)?;
        if tl.loops.is_some() && tl.done(t()) {
            return None;
        }
        Some(crate::clock::Rate::Every(tl.tick))
    }

    /// (M4) When an animated `image` node's clock next ticks: at its next
    /// frame change, not at every [`Timeline::tick`] between. `t` is the
    /// node's time and `next` the tick after it on its `tick` clock (what
    /// [`crate::time::NodeTimes::context`] gave), so `next - tick` is the
    /// instant of `t`. `next` unchanged for anything else.
    pub(crate) fn frame_wake(
        &self,
        node: &crate::tree::Node,
        t: f32,
        tick: std::time::Duration,
        next: std::time::Duration,
    ) -> std::time::Duration {
        let source = match node.get(strand_scene::Prop::Source) {
            Some(strand_scene::PropValue::Text(s) | strand_scene::PropValue::Keyword(s)) => s,
            _ => return next,
        };
        match self.timelines.get(source) {
            Some(tl) if tl.tick == tick => match tl.until_change(t) {
                Some(d) => next.saturating_sub(tick) + d.max(tick),
                None => next,
            },
            _ => next,
        }
    }

    /// (M4) The timeline of `source`, if a decode showed it animated.
    pub fn timeline(&self, source: &str) -> Option<&Arc<Timeline>> {
        self.timelines.get(source)
    }

    /// (M4) Animated images playing inline (offline renders and tests).
    pub fn players(&self) -> usize {
        self.players.len()
    }

    /// (M4) Frames of `key`'s image at its size held decoded.
    pub fn frames_held(&self, key: &ImageKey) -> usize {
        let first = key.first();
        self.entries
            .iter()
            .filter(|(k, e)| e.bytes > 0 && k.first() == first)
            .count()
    }

    /// The icon theme changed (a theme installed or switched, an icon
    /// added): every decode looked up in it is forgotten, so the next
    /// frame asks for it again, and decodes in flight are dropped on
    /// arrival. Returns the surfaces whose last frame drew one (to
    /// repaint). Images drawn from paths stay.
    pub fn invalidate_icons(&mut self) -> Vec<SurfaceId> {
        let gone: Vec<ImageKey> = self
            .entries
            .keys()
            .filter(|k| is_icon(k))
            .cloned()
            .collect();
        for k in &gone {
            if let Some(e) = self.entries.remove(k) {
                self.bytes -= e.bytes;
            }
        }
        // A stand-in of the old icon would be drawn until the new one
        // arrives: keep it (no blank frame), it is replaced then.
        self.stale
            .extend(self.pending.iter().filter(|k| is_icon(k)).cloned());
        self.frames
            .iter()
            .filter(|(_, f)| f.iter().any(is_icon))
            .map(|(s, _)| *s)
            .collect()
    }

    /// (M6) Local files changed on disk (an `image`'s file saved while
    /// the shell runs: the watcher's `Role::Asset`): every decode of a
    /// path among `paths` is read again. One a live frame draws is kept
    /// and drawn until its new decode replaces it (no blank frame); the
    /// others are forgotten; decodes and file reads (an `svg`'s, a
    /// `lottie`'s) in flight are dropped on arrival and asked for again;
    /// an animated image's timeline and players go with its old bytes.
    /// Returns the surfaces whose last frame drew one (to repaint).
    /// Icon names are never paths: an edited `icons/` file is
    /// [`ImageStore::invalidate_icons`]'.
    pub fn invalidate_files(&mut self, paths: &[PathBuf]) -> Vec<SurfaceId> {
        if paths.is_empty() {
            return Vec::new();
        }
        let hit = |k: &ImageKey| !is_icon(k) && paths.contains(&local_path(&k.source));
        let mut sources: HashSet<String> = HashSet::new();
        let found: Vec<ImageKey> = self.entries.keys().filter(|k| hit(k)).cloned().collect();
        for k in found {
            sources.insert(k.source.clone());
            if self.frames.values().any(|f| f.contains(&k)) {
                self.outdated.insert(k);
                continue;
            }
            if let Some(e) = self.entries.remove(&k) {
                self.bytes -= e.bytes;
            }
            self.unhold(&k);
        }
        let flying: Vec<ImageKey> = self.pending.iter().filter(|k| hit(k)).cloned().collect();
        sources.extend(flying.iter().map(|k| k.source.clone()));
        self.stale.extend(flying);
        // The outdated set names only decodes still held.
        let entries = &self.entries;
        self.outdated.retain(|k| entries.contains_key(k));
        for s in &sources {
            self.timelines.remove(s);
        }
        self.players.retain(|k| !sources.contains(&k.source));
        if let ImageBackend::Worker(w) = &self.backend {
            // A file job's key is `svg:<source>` or `lottie:<source>`.
            let file_hit = |key: &String| {
                key.split_once(':')
                    .is_some_and(|(_, src)| paths.contains(&local_path(src)))
            };
            if let Ok(mut f) = w.files.lock() {
                f.done.retain(|key, _| !file_hit(key));
                let flying: Vec<String> =
                    f.pending.iter().filter(|k| file_hit(k)).cloned().collect();
                f.stale.extend(flying);
            }
            if !sources.is_empty()
                && let Some(tx) = &w.requests
            {
                let _ = tx.send(Job::Forget(sources.into_iter().collect()));
            }
        }
        self.frames
            .iter()
            .filter(|(_, f)| f.iter().any(hit))
            .map(|(s, _)| *s)
            .collect()
    }

    pub fn set_backend(&mut self, backend: ImageBackend) {
        self.backend = backend;
        self.pending.clear();
        self.publish_wanted();
    }

    /// The decoded image of `key`, if it is ready (`Some(Err)` when it
    /// failed: nothing is drawn).
    pub fn get(&self, key: &ImageKey) -> Option<&Result<Decoded, ImageError>> {
        self.entries.get(key).map(|e| &e.result)
    }

    /// A decode of `key`'s source at another size or fit, with its key:
    /// drawn scaled into the box, by the box's own fit, until `key`
    /// itself is decoded (stand-in-fit). Of those held: one that holds
    /// what the box shows ([`Decoded::holds`], within [`STAND_IN_BARE`])
    /// before one that does not, then the sharpest (the most pixmap
    /// pixels to a source pixel), then the newest; of those that do not
    /// hold it, the one leaving least bare. The latest alone was a
    /// thumbnail's crop standing in for a box of another shape, its edge
    /// rows padded out over what it did not hold.
    pub fn stand_in(&self, key: &ImageKey) -> Option<(&ImageKey, &Decoded)> {
        let (w, h) = (f64::from(key.w), f64::from(key.h));
        // Ordered: smaller is better.
        let rank = |d: &Decoded| {
            let bare = d.holds(w, h, key.fit).1;
            let (sw, sh) = d.source;
            let (pw, ph) = (f64::from(d.pixmap.width()), f64::from(d.pixmap.height()));
            let sharp = if sw > 0.0 && sh > 0.0 {
                placement(sw, sh, pw, ph, d.fit).2 / sw
            } else {
                0.0
            };
            if bare <= STAND_IN_BARE {
                (0, -sharp)
            } else {
                (1, bare)
            }
        };
        let mut best: Option<((u8, f64), &ImageKey, &Decoded)> = None;
        for k in self.held.get(&source_key(key))?.iter().rev() {
            if k == key {
                continue;
            }
            let Some(Ok(d)) = self.entries.get(k).map(|e| &e.result) else {
                continue;
            };
            let r = rank(d);
            if best.is_none_or(|(b, ..)| r < b) {
                best = Some((r, k, d));
            }
        }
        best.map(|(_, k, d)| (k, d))
    }

    /// A stand-in for `key` that holds what its box shows, within
    /// [`STAND_IN_BARE`].
    fn stands_in(&self, key: &ImageKey) -> bool {
        self.stand_in(key).is_some_and(|(_, d)| {
            d.holds(f64::from(key.w), f64::from(key.h), key.fit).1 <= STAND_IN_BARE
        })
    }

    /// (stand-in-fit) The decode that holds all of `key`'s source at about
    /// the scale `key`'s fit draws it (`fill` into the source's size at
    /// that scale, rounded: every pixel the source's, so an opaque source
    /// stays claimed opaque, and the rounding's stretch, under half a
    /// pixel a side, is undone where [`Decoded::placed_for`] places it),
    /// to stand in for a `cover` box no decode held covers; `None` for the other fits, which show the whole source
    /// (any decode of it holds what they show), or when no decode tells
    /// the source's size. Its longer side is at most four times the
    /// box's (a box far from the source's shape crops most of it).
    fn whole_for(&self, key: &ImageKey) -> Option<ImageKey> {
        if key.fit != Fit::Cover {
            return None;
        }
        let (sw, sh) = self.held.get(&source_key(key))?.iter().find_map(|k| {
            match self.entries.get(k).map(|e| &e.result) {
                Some(Ok(d)) => Some(d.source),
                _ => None,
            }
        })?;
        let (w, h) = (f64::from(key.w), f64::from(key.h));
        if !(sw > 0.0 && sh > 0.0 && w > 0.0 && h > 0.0) {
            return None;
        }
        let side = 4.0 * w.max(h);
        let k = (w / sw).max(h / sh).min(side / sw.max(sh));
        Some(ImageKey {
            w: (sw * k).round().max(1.0) as u32,
            h: (sh * k).round().max(1.0) as u32,
            fit: Fit::Fill,
            ..key.clone()
        })
    }

    /// `key`'s decode is gone: it no longer stands in.
    fn unhold(&mut self, key: &ImageKey) {
        let sk = source_key(key);
        if let Some(list) = self.held.get_mut(&sk) {
            list.retain(|k| k != key);
            if list.is_empty() {
                self.held.remove(&sk);
            }
        }
    }

    /// Bytes of decoded pixels held.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Decodes in flight.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Surface `surface`'s frame wants `keys`: they move to the front of
    /// the LRU, and the missing ones are decoded (inline at once, or on
    /// the worker). With `defer` (a size springs on that surface) a key
    /// with a stand-in that holds what its box shows
    /// ([`STAND_IN_BARE`]) is not asked for: the stand-in is drawn
    /// scaled until the size comes to rest, so a spring does not decode
    /// every size it passes through. Returns true if anything was decoded
    /// inline.
    pub fn want(&mut self, surface: SurfaceId, keys: &[ImageKey], defer: bool) -> bool {
        // Published to the worker only when the surface's set changed (an
        // animation frame drawing the same images allocates nothing).
        // Both ways: keys may repeat (two rows showing one app's icon), so
        // equal lengths do not make one inclusion enough.
        let same = self.frames.get(&surface).is_some_and(|f| {
            f.len() <= keys.len()
                && keys.iter().all(|k| f.contains(k))
                && f.iter().all(|k| keys.contains(k))
        });
        if !same {
            self.frames.insert(surface, keys.iter().cloned().collect());
            self.publish_wanted();
        }
        // Images no frame draws any more stop playing.
        if !same && !self.players.is_empty() {
            let frames = &self.frames;
            self.players
                .retain(|p| frames.values().flatten().any(|k| k.first() == *p));
        }
        let mut decoded = false;
        for k in keys {
            self.tick += 1;
            if let Some(e) = self.entries.get_mut(k) {
                e.used = self.tick;
                if !self.outdated.contains(k) {
                    continue;
                }
            }
            if self.pending.contains(k) || (defer && self.stands_in(k)) {
                continue;
            }
            // (stand-in-fit) No decode held covers what this springing box
            // shows: one of the whole source stands in for every size the
            // spring passes through, so those sizes are still not decoded.
            let whole = if defer { self.whole_for(k) } else { None };
            let k = match &whole {
                Some(w) if self.wholes.iter().any(|p| source_key(p) == source_key(w)) => continue,
                Some(w) if !self.entries.contains_key(w) => w,
                _ => k,
            };
            match &mut self.backend {
                ImageBackend::Inline(theme) => {
                    let r = load_with(k, theme, &mut self.players);
                    self.insert(k.clone(), r);
                    decoded = true;
                }
                ImageBackend::Worker(w) => {
                    let sent = w
                        .requests
                        .as_ref()
                        .is_some_and(|tx| tx.send(Job::Image(k.clone())).is_ok());
                    if sent {
                        self.pending.insert(k.clone());
                        if whole.as_ref() == Some(k) {
                            self.wholes.insert(k.clone());
                            self.publish_wanted();
                        }
                    } else {
                        self.insert(k.clone(), Err(ImageError::Io("no image worker".into())));
                    }
                }
            }
        }
        decoded
    }

    /// (M4) A media source's file job `key` (an `svg`'s or `lottie`'s
    /// source): its result once done, taken (the source keeps it), or
    /// `None` while the worker runs it. Inline, it runs at once; with a
    /// worker it is sent once and the render loop is woken when it is
    /// done ([`ImageStore::files_arrived`]).
    pub(crate) fn load_file(
        &self,
        key: &str,
        work: impl FnOnce() -> Loaded + Send + 'static,
    ) -> Option<Loaded> {
        let ImageBackend::Worker(w) = &self.backend else {
            return Some(work());
        };
        let Ok(mut f) = w.files.lock() else {
            return Some(Err("image worker poisoned".into()));
        };
        if let Some((r, _)) = f.done.remove(key) {
            return Some(r);
        }
        if f.pending.contains(key) {
            return None;
        }
        let job = Job::File(FileJob {
            key: key.to_string(),
            work: Box::new(work),
        });
        match w.requests.as_ref().map(|tx| tx.send(job)) {
            Some(Ok(())) => {
                f.pending.insert(key.to_string());
                None
            }
            _ => Some(Err("no image worker".into())),
        }
    }

    /// (M4) True once if a file job finished since the last call.
    pub(crate) fn files_arrived(&self) -> bool {
        match &self.backend {
            ImageBackend::Worker(w) => w
                .files
                .lock()
                .map(|mut f| std::mem::take(&mut f.arrived))
                .unwrap_or(false),
            ImageBackend::Inline(_) => false,
        }
    }

    /// Surface `surface` is gone: its frame no longer holds images.
    pub fn forget(&mut self, surface: SurfaceId) {
        if self.frames.remove(&surface).is_some() {
            self.publish_wanted();
        }
    }

    /// Tells the worker which keys some frame still wants.
    fn publish_wanted(&self) {
        if let ImageBackend::Worker(w) = &self.backend
            && let Ok(mut set) = w.wanted.lock()
        {
            set.clear();
            set.extend(self.frames.values().flatten().cloned());
            set.extend(self.wholes.iter().cloned());
        }
    }

    /// True if surface `surface`'s last frame draws `key`.
    pub fn drawn_by(&self, surface: SurfaceId, key: &ImageKey) -> bool {
        self.frames.get(&surface).is_some_and(|f| f.contains(key))
    }

    /// Takes the worker's results: the keys that arrived (decoded or
    /// failed; ones dropped undecoded are not among them). A key dropped
    /// undecoded that a frame wants again is sent again: it was dropped
    /// because no frame wanted it when the worker came to it, and a frame
    /// that wanted it again before this poll did not ask for it (it was
    /// in flight), so nothing else would (decisions.md m6-flakes). A
    /// stale one is sent again too, and is no longer stale: it has no
    /// old-theme result to throw away, and the worker looks it up in the
    /// theme current when it comes to it.
    pub fn poll(&mut self) -> Vec<ImageKey> {
        let ImageBackend::Worker(w) = &self.backend else {
            return Vec::new();
        };
        let got: Vec<_> = w.results.try_iter().collect();
        let mut arrived = Vec::new();
        for (k, r) in got {
            if r.is_none()
                && self.frames.values().any(|f| f.contains(&k))
                && let ImageBackend::Worker(w) = &self.backend
                && w.requests
                    .as_ref()
                    .is_some_and(|tx| tx.send(Job::Image(k.clone())).is_ok())
            {
                self.stale.remove(&k);
                continue;
            }
            self.pending.remove(&k);
            self.wholes.remove(&k);
            if self.stale.remove(&k) {
                // Looked up in the old theme: asked for again by the
                // frame it repaints.
                if r.is_some() {
                    arrived.push(k);
                }
                continue;
            }
            if let Some(r) = r {
                arrived.push(k.clone());
                self.insert(k, r);
            }
        }
        arrived
    }

    fn insert(&mut self, key: ImageKey, result: Result<Decoded, ImageError>) {
        let bytes = match &result {
            Ok(d) => d.pixmap.width() as usize * d.pixmap.height() as usize * 4,
            Err(_) => 0,
        };
        self.tick += 1;
        self.outdated.remove(&key);
        if let Ok(d) = &result {
            let entries = &self.entries;
            let list = self.held.entry(source_key(&key)).or_default();
            list.retain(|k| k != &key && entries.contains_key(k));
            list.push(key.clone());
            if let Some(tl) = &d.anim
                && !self.timelines.contains_key(&key.source)
            {
                self.timelines.insert(key.source.clone(), tl.clone());
            }
        }
        // (M4) An animated image keeps two frames per size: the newest
        // two, or the ones live frames draw.
        if result.as_ref().is_ok_and(|d| d.anim.is_some()) {
            let first = key.first();
            let mut held: Vec<(u64, ImageKey)> = self
                .entries
                .iter()
                .filter(|(k, e)| e.bytes > 0 && k.first() == first && **k != key)
                .map(|(k, e)| (e.used, k.clone()))
                .collect();
            held.sort_by_key(|(u, _)| std::cmp::Reverse(*u));
            let drawn = |k: &ImageKey| self.frames.values().any(|f| f.contains(k));
            let gone: Vec<ImageKey> = held
                .into_iter()
                .skip(1)
                .map(|(_, k)| k)
                .filter(|k| !drawn(k))
                .collect();
            for k in gone {
                if let Some(e) = self.entries.remove(&k) {
                    self.bytes -= e.bytes;
                }
            }
        }
        if let Some(old) = self.entries.insert(
            key,
            Entry {
                result,
                bytes,
                used: self.tick,
            },
        ) {
            self.bytes -= old.bytes;
        }
        self.bytes += bytes;
        // Least recently used first, never what a live frame draws.
        while self.bytes > IMAGE_CACHE_BYTES {
            let drawn = |k: &ImageKey| self.frames.values().any(|f| f.contains(k));
            let victim = self
                .entries
                .iter()
                .filter(|(k, e)| e.bytes > 0 && !drawn(k))
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| k.clone());
            let Some(v) = victim else { break };
            if let Some(e) = self.entries.remove(&v) {
                self.bytes -= e.bytes;
            }
            self.unhold(&v);
            if !self.entries.keys().any(|k| k.source == v.source) {
                self.timelines.remove(&v.source);
            }
        }
        let failed = self.entries.values().filter(|e| e.result.is_err()).count();
        if failed > MAX_FAILED {
            let mut errs: Vec<(u64, ImageKey)> = self
                .entries
                .iter()
                .filter(|(_, e)| e.result.is_err())
                .map(|(k, e)| (e.used, k.clone()))
                .collect();
            errs.sort_by_key(|(u, _)| *u);
            for (_, k) in errs.into_iter().take(failed - MAX_FAILED) {
                self.entries.remove(&k);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raster(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> Raster {
        let mut rgba = Vec::new();
        for y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(&f(x, y));
            }
        }
        Raster::new(w, h, rgba)
    }

    fn base64(bytes: &[u8]) -> String {
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for c in bytes.chunks(3) {
            let n = c
                .iter()
                .enumerate()
                .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
            for i in 0..4 {
                if i <= c.len() {
                    out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    /// (m4-audit) An SVG's `<image href>` naming a file is not read:
    /// usvg's default resolver read any path whole (`/dev/zero` until
    /// strand was killed, a FIFO forever), past `read_local`'s limits.
    /// A nested SVG file is not drawn, a FIFO is not opened (the test
    /// would block), and a `data:` URL still is.
    #[test]
    fn an_svg_reads_no_file_its_images_name() {
        let dir = std::env::temp_dir().join(format!("strand-svg-href-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let red = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10" fill="red"/></svg>"#;
        let inner = dir.join("inner.svg");
        std::fs::write(&inner, red).unwrap();
        let fifo = dir.join("pipe.svg");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .is_ok_and(|s| s.success());
        let outer = |href: &str| {
            format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="10" height="10"><image width="10" height="10" href="{href}"/><rect width="1" height="1" fill="blue"/></svg>"#
            )
        };
        let alpha = |svg: &str| {
            let (r, _) = render_svg(svg.as_bytes(), 10, 10, Fit::Fill).unwrap();
            r.rgba[(5 * 10 + 5) * 4 + 3]
        };
        assert_eq!(alpha(&outer(&inner.display().to_string())), 0, "a file");
        if made {
            assert_eq!(alpha(&outer(&fifo.display().to_string())), 0, "a FIFO");
        }
        assert_eq!(alpha(&outer("/dev/zero")), 0, "a device");
        let data = format!("data:image/svg+xml;base64,{}", base64(red.as_bytes()));
        assert_eq!(alpha(&outer(&data)), 255, "a data URL");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// (m4-audit) An image, SVG or Lottie path (a notification's
    /// `image-path` hint, an asset a Lottie names) to a procfs file is
    /// refused, not read to its end: `/proc/self/pagemap` is a regular
    /// file whose size reads 0 and whose content runs to hundreds of GiB.
    /// A file at the limit is read; one past it, or growing past it after
    /// the stat (a procfs file), is `TooLarge`.
    #[test]
    fn image_reads_stop_at_their_limit() {
        let pagemap = Path::new("/proc/self/pagemap");
        if pagemap.is_file() {
            let r = read_local("/proc/self/pagemap", 1 << 20);
            assert!(r.is_err(), "pagemap read {:?} bytes", r.map(|b| b.len()));
            let key = ImageKey {
                source: "/proc/self/pagemap".into(),
                icon: false,
                w: 16,
                h: 16,
                fit: Fit::Contain,
                scale: 1,
                frame: 0,
            };
            assert!(load(&key, &IconTheme::default()).is_err());
        }
        // A procfs file longer than the cap, though its size reads 0.
        let maps = std::fs::read("/proc/self/maps").unwrap();
        assert!(maps.len() > 16);
        assert_eq!(
            read_capped(Path::new("/proc/self/maps"), 16),
            Err(ImageError::TooLarge)
        );
        let dir = std::env::temp_dir().join(format!("strand-img-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("f.bin");
        std::fs::write(&f, [7u8; 16]).unwrap();
        assert_eq!(read_capped(&f, 16).unwrap().len(), 16);
        assert_eq!(
            read_local(&f.display().to_string(), 15),
            Err(ImageError::TooLarge)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn downscaling_averages_areas() {
        // A 4 × 4 checker of black and white averages to mid grey at 1 × 1.
        let src = raster(4, 4, |x, y| {
            if (x + y) % 2 == 0 {
                [255; 4]
            } else {
                [0, 0, 0, 255]
            }
        });
        let out = fit_raster(&src, 1, 1, Fit::Fill);
        assert!((out.rgba[0] as i32 - 128).abs() <= 1, "{:?}", out.rgba);
        assert_eq!(out.rgba[3], 255);
    }

    #[test]
    fn contain_letterboxes_and_cover_crops() {
        // A 2:1 red image in a square: contain leaves bands, cover fills.
        let src = raster(20, 10, |_, _| [255, 0, 0, 255]);
        let c = fit_raster(&src, 10, 10, Fit::Contain);
        let px = |r: &Raster, x: u32, y: u32| r.rgba[((y * 10 + x) * 4) as usize + 3];
        assert_eq!(px(&c, 5, 0), 0, "band above");
        assert_eq!(px(&c, 5, 5), 255);
        let v = fit_raster(&src, 10, 10, Fit::Cover);
        assert_eq!(px(&v, 5, 0), 255);
        assert_eq!(px(&v, 0, 9), 255);
    }

    #[test]
    fn the_store_is_an_lru_within_its_budget() {
        let mut s = ImageStore::default();
        let side = 512u32; // 1 MB each
        let held = IMAGE_CACHE_BYTES / (512 * 512 * 4);
        for i in 0..held + 4 {
            let k = ImageKey {
                source: format!("k{i}"),
                icon: false,
                w: side,
                h: side,
                fit: Fit::Contain,
                scale: 1,
                frame: 0,
            };
            s.insert(
                k,
                Ok(Decoded {
                    pixmap: Arc::new(Pixmap::new(side as u16, side as u16)),
                    symbolic: false,
                    source: (side as f64, side as f64),
                    fit: Fit::Contain,
                    anim: None,
                }),
            );
        }
        assert!(s.bytes() <= IMAGE_CACHE_BYTES);
        assert_eq!(s.bytes(), held * 512 * 512 * 4);
        let has = |s: &ImageStore, i: usize| s.entries.keys().any(|k| k.source == format!("k{i}"));
        assert!(!has(&s, 0) && has(&s, held + 3), "the oldest go first");
    }

    /// A surface drawing one image twice (two rows with one app's icon)
    /// no longer draws an image it dropped.
    #[test]
    fn repeated_keys_drop_what_a_frame_no_longer_draws() {
        let mut s = ImageStore::default();
        let k = |source: &str| ImageKey {
            source: source.into(),
            icon: false,
            w: 8,
            h: 8,
            fit: Fit::Contain,
            scale: 1,
            frame: 0,
        };
        let (a, b) = (k("/nonexistent/a.png"), k("/nonexistent/b.png"));
        s.want(SurfaceId(1), &[a.clone(), b.clone()], false);
        assert!(s.drawn_by(SurfaceId(1), &b));
        s.want(SurfaceId(1), &[a.clone(), a.clone()], false);
        assert!(s.drawn_by(SurfaceId(1), &a));
        assert!(!s.drawn_by(SurfaceId(1), &b), "b is no longer drawn");
        s.want(SurfaceId(1), &[a.clone(), b.clone(), b.clone()], false);
        assert!(s.drawn_by(SurfaceId(1), &b));
    }

    #[test]
    fn icon_names_fall_back_to_the_other_variant_and_generic_names() {
        assert_eq!(
            icon_candidates("network-wireless"),
            [
                "network-wireless",
                "network-wireless-symbolic",
                "network",
                "network-symbolic"
            ]
        );
        assert_eq!(
            icon_candidates("audio-volume-high-symbolic"),
            [
                "audio-volume-high-symbolic",
                "audio-volume-high",
                "audio-volume-symbolic",
                "audio-volume",
                "audio-symbolic",
                "audio"
            ]
        );
        assert_eq!(icon_candidates("firefox"), ["firefox", "firefox-symbolic"]);
    }

    /// A large source drawn small never holds more than a few times the
    /// drawn size: the JPEG's IDCT is scaled, the PNG reduced row by row.
    #[test]
    fn large_sources_decode_reduced() {
        let jpeg = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/halves-large.jpg"
        ))
        .unwrap();
        // 4000 × 2000 drawn as 200 × 100: decoded at 1/8, 500 × 250.
        let r = decode_jpeg(&jpeg, (200, 100), Fit::Contain).unwrap();
        assert_eq!((r.w, r.h), (500, 250));
        // Drawn at 600 × 300: 1/4 (1000 × 500) is the smallest above.
        let r = decode_jpeg(&jpeg, (600, 300), Fit::Contain).unwrap();
        assert_eq!((r.w, r.h), (1000, 500));
        let px = |r: &Raster, x: u32, y: u32| {
            let i = ((y * r.w + x) * 4) as usize;
            [r.rgba[i], r.rgba[i + 1], r.rgba[i + 2]]
        };
        assert!(px(&r, 100, 100)[0] > 0xc0 && px(&r, 900, 100)[2] > 0xc0);

        // A 3000 × 2000 PNG (red left, blue right, half transparent at
        // the bottom) contained in a 100 × 100 box (drawn 100 × 67):
        // reduced by 30 as it is read.
        let (sw, sh) = (3000u32, 2000u32);
        let mut png_bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png_bytes, sw, sh);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            let mut data = Vec::with_capacity((sw * sh * 4) as usize);
            for y in 0..sh {
                for x in 0..sw {
                    let a = if y >= sh / 2 { 128 } else { 255 };
                    data.extend_from_slice(&if x < sw / 2 {
                        [255, 0, 0, a]
                    } else {
                        [0, 0, 255, a]
                    });
                }
            }
            w.write_image_data(&data).unwrap();
        }
        let r = decode_png(&png_bytes, (100, 100), Fit::Contain).unwrap();
        assert_eq!((r.w, r.h), (100, 67));
        assert_eq!(px(&r, 10, 10), [255, 0, 0]);
        assert_eq!(px(&r, 90, 10), [0, 0, 255]);
        // Premultiplied before averaging: half-transparent red.
        assert_eq!(px(&r, 10, 60), [128, 0, 0]);
        assert_eq!(r.rgba[((60 * r.w + 10) * 4 + 3) as usize], 128);
        // Drawn at its own size: decoded whole.
        let r = decode_png(&png_bytes, (3000, 2000), Fit::Contain).unwrap();
        assert_eq!((r.w, r.h), (3000, 2000));
    }

    /// (png-aspect) `within` keeps a size's aspect within half a pixel a
    /// side where rounding fits `max_pixels`, and never passes it; a
    /// reduced PNG reports its source's extent.
    #[test]
    fn within_rounds_each_side_and_keeps_its_budget() {
        for (w, h, max) in [
            (20.0, 13.0 + 1.0 / 3.0, 1_000),
            (250.0, 9.0, 1_000),
            (3000.0, 2000.0, 10_000),
            (61.0, 37.0, 500),
            (1e6, 1.0, 100),
        ] {
            let (a, b) = within(w, h, max);
            assert!(u64::from(a) * u64::from(b) <= max.max(u64::from(a.max(b))));
            let k = (max as f64 / (w * h)).sqrt().min(1.0);
            let (x, y) = (w * k, h * k);
            if (x.round().max(1.0) * y.round().max(1.0)) as u64 <= max {
                assert!(
                    (a as f64 - x.max(1.0)).abs() <= 0.5,
                    "{w}x{h} in {max}: {a}x{b}"
                );
                assert!(
                    (b as f64 - y.max(1.0)).abs() <= 0.5,
                    "{w}x{h} in {max}: {a}x{b}"
                );
            } else {
                assert!((a as f64 - x).abs() < 1.0 && (b as f64 - y).abs() < 1.0);
            }
        }
        // A 60 × 40 PNG read for a box a third its size keeps 20 × 13⅓ of
        // source in its 20 × 14 pixels; a Lottie asset with no size
        // within 300 pixels is 21 × 14 (21.2 × 14.1).
        let mut png_bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png_bytes, 60, 40);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[90u8; 60 * 40 * 3]).unwrap();
        }
        let r = decode_png(&png_bytes, (20, 14), Fit::Contain).unwrap();
        assert_eq!((r.w, r.h), (20, 14));
        assert!((r.extent.0 - 20.0).abs() < 1e-9 && (r.extent.1 - 40.0 / 3.0).abs() < 1e-9);
        let pm = decode_rgba(&png_bytes, None, 300).unwrap();
        assert_eq!((pm.width(), pm.height()), (21, 14));
    }

    #[test]
    fn huge_reductions_average_without_overflow() {
        // A 4400 × 4400 PNG drawn at 1 × 1 averages blocks of 4400²
        // pixels: 4400² × 255 is past `u32`.
        let side = 4400u32;
        let mut png_bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png_bytes, side, side);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            let mut s = w.stream_writer().unwrap();
            let row: Vec<u8> = [200u8, 120, 40].repeat(side as usize);
            for _ in 0..side {
                std::io::Write::write_all(&mut s, &row).unwrap();
            }
            s.finish().unwrap();
        }
        let r = decode_png(&png_bytes, (1, 1), Fit::Contain).unwrap();
        assert_eq!((r.w, r.h), (1, 1));
        assert_eq!(r.rgba, [200, 120, 40, 255]);
    }

    /// (opaque-fast-path) A decode claims to be opaque exactly when every
    /// pixel it holds is (vello then overwrites and culls under it): with
    /// one translucent pixel it does not.
    #[test]
    fn a_decode_claims_opacity_only_when_every_pixel_is() {
        let png_of = |w: u32, h: u32, alpha_at: Option<(u32, u32)>| {
            let mut bytes = Vec::new();
            {
                let mut e = png::Encoder::new(&mut bytes, w, h);
                e.set_color(png::ColorType::Rgba);
                e.set_depth(png::BitDepth::Eight);
                let mut data = Vec::new();
                for y in 0..h {
                    for x in 0..w {
                        let a = if alpha_at == Some((x, y)) { 254 } else { 255 };
                        data.extend([40 + x as u8, 90 + y as u8, 160, a]);
                    }
                }
                e.write_header().unwrap().write_image_data(&data).unwrap();
            }
            bytes
        };
        let claims = |bytes: &[u8], want, fit| {
            !to_pixmap(&decode_png(bytes, want, fit).unwrap()).may_have_transparency()
        };
        assert!(
            claims(&png_of(8, 6, None), (8, 6), Fit::Fill),
            "opaque, own size"
        );
        assert!(
            claims(&png_of(8, 6, None), (16, 12), Fit::Cover),
            "opaque, scaled"
        );
        assert!(
            !claims(&png_of(8, 6, Some((3, 2))), (8, 6), Fit::Fill),
            "one translucent pixel"
        );
    }

    /// A decode standing in for another box size is placed by its fit,
    /// not stretched: a 2:1 source contained in a square (letterboxed)
    /// stands in for a 2:1 box by filling it, its bands outside.
    #[test]
    fn stand_ins_are_placed_by_their_fit() {
        let d = |fit| Decoded {
            pixmap: Arc::new(Pixmap::new(100, 100)),
            symbolic: false,
            source: (400.0, 200.0),
            fit,
            anim: None,
        };
        // Contain: content rows 25..75 of the pixmap fill 0..100.
        let (x, y, w, h) = d(Fit::Contain).placed_in(10.0, 20.0, 200.0, 100.0);
        assert_eq!((x, y, w, h), (10.0, -30.0, 200.0, 200.0));
        // Its own size: unchanged.
        assert_eq!(
            d(Fit::Contain).placed_in(10.0, 20.0, 100.0, 100.0),
            (10.0, 20.0, 100.0, 100.0)
        );
        // Cover: the 100 × 100 crop of the middle (source x 100..300 at
        // half scale) lands centred in a 200 × 100 box, uniformly.
        let (x, y, w, h) = d(Fit::Cover).placed_in(0.0, 0.0, 200.0, 100.0);
        assert_eq!((x, y, w, h), (50.0, 0.0, 100.0, 100.0));
        // Fill stretches, as it always does.
        assert_eq!(
            d(Fit::Fill).placed_in(0.0, 0.0, 200.0, 50.0),
            (0.0, 0.0, 200.0, 50.0)
        );
    }

    /// (stand-in-fit) A decode stands in placed by the drawing node's
    /// fit, whatever fit it was decoded by, and says how much of what
    /// that fit shows it leaves bare: a 2:1 source contained in a square
    /// holds all of it, so it stands in for a 2:1 `cover` box whole; the
    /// square's `cover` crop holds only the middle half.
    #[test]
    fn stand_ins_are_placed_by_the_drawing_fit_and_say_what_they_hold() {
        let d = |fit| Decoded {
            pixmap: Arc::new(Pixmap::new(100, 100)),
            symbolic: false,
            source: (400.0, 200.0),
            fit,
            anim: None,
        };
        let contained = d(Fit::Contain);
        assert_eq!(
            contained.placed_for(0.0, 0.0, 200.0, 100.0, Fit::Cover),
            (0.0, -50.0, 200.0, 200.0)
        );
        assert_eq!(
            contained.holds(200.0, 100.0, Fit::Cover),
            ([0.0, 0.0, 200.0, 100.0], 0.0)
        );
        let cropped = d(Fit::Cover);
        assert_eq!(
            cropped.holds(200.0, 100.0, Fit::Cover),
            ([50.0, 0.0, 150.0, 100.0], 50.0)
        );
        // Drawn by its own fit in its own box, each holds what it shows;
        // the crop standing in for a contained box leaves its sides bare.
        assert_eq!(contained.holds(100.0, 100.0, Fit::Contain).1, 0.0);
        assert_eq!(cropped.holds(100.0, 100.0, Fit::Cover).1, 0.0);
        assert_eq!(cropped.holds(100.0, 100.0, Fit::Contain).1, 25.0);
    }

    #[test]
    fn missing_sources_fail_without_panicking() {
        let theme = IconTheme::named("hicolor");
        let k = |source: &str, icon| ImageKey {
            source: source.into(),
            icon,
            w: 16,
            h: 16,
            fit: Fit::Contain,
            scale: 1,
            frame: 0,
        };
        assert!(matches!(
            load(&k("/nonexistent/a.png", false), &theme),
            Err(ImageError::NotFound(_))
        ));
        assert!(load(&k("", true), &theme).is_err());
        assert!(load(&k("../../etc/passwd", true), &theme).is_err());
        assert_eq!(percent_decode("/a%20b.png"), "/a b.png");
    }

    /// (m4-audit) More file results than `MAX_FILES_DONE` finishing
    /// before the render loop takes them are all kept: each is taken
    /// without running its job again.
    #[test]
    fn a_burst_of_file_results_is_kept_for_its_sources() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let worker = ImageWorker::spawn(IconTheme::named("StrandTest"), None).unwrap();
        let store = ImageStore::new(ImageBackend::Worker(worker));
        let runs = Arc::new(AtomicUsize::new(0));
        let n = MAX_FILES_DONE * 2;
        let job = |runs: &Arc<AtomicUsize>| {
            let runs = runs.clone();
            move || -> Loaded {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok(Arc::new(()))
            }
        };
        for i in 0..n {
            assert!(store.load_file(&format!("k{i}"), job(&runs)).is_none());
        }
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while runs.load(Ordering::SeqCst) < n
            || matches!(&store.backend, ImageBackend::Worker(w)
                if !w.files.lock().unwrap().pending.is_empty())
        {
            assert!(Instant::now() < deadline, "the worker answers");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        for i in 0..n {
            let got = store.load_file(&format!("k{i}"), job(&runs));
            assert!(matches!(got, Some(Ok(_))), "k{i}: {got:?}");
        }
        assert_eq!(runs.load(Ordering::SeqCst), n, "no job ran twice");
    }

    /// (m4-audit) Past `MAX_FILES_DONE` only stale results go; past
    /// `MAX_FILES_HELD` the oldest go too.
    #[test]
    fn only_stale_or_surplus_file_results_are_dropped() {
        let t0 = Instant::now();
        let ok = || -> Loaded { Ok(Arc::new(())) };
        let mut done: HashMap<String, (Loaded, Instant)> = HashMap::new();
        for i in 0..MAX_FILES_DONE {
            done.insert(format!("old{i}"), (ok(), t0));
        }
        let later = t0 + FILE_RESULT_STALE / 2;
        done.insert("fresh".into(), (ok(), later));
        make_room(&mut done, later);
        assert_eq!(done.len(), MAX_FILES_DONE + 1, "nothing stale yet");
        make_room(&mut done, t0 + FILE_RESULT_STALE);
        assert_eq!(done.keys().collect::<Vec<_>>(), ["fresh"]);
        let mut done: HashMap<String, (Loaded, Instant)> = HashMap::new();
        for i in 0..MAX_FILES_HELD {
            let at = t0 + std::time::Duration::from_millis(i as u64);
            done.insert(format!("k{i}"), (ok(), at));
        }
        make_room(
            &mut done,
            t0 + std::time::Duration::from_millis(MAX_FILES_HELD as u64),
        );
        assert_eq!(done.len(), MAX_FILES_HELD - 1);
        assert!(!done.contains_key("k0"), "the oldest went");
    }

    /// A request dropped undecoded (no frame wanted it when the worker
    /// came to it) that a frame wanted again while it was in flight is
    /// decoded: the worker wakes the render loop for the drop too, and
    /// the poll sends it again. Before, the frame's second want skipped
    /// it as in flight, the poll forgot it, and the surface drew without
    /// it until something else repainted it (the budgets test's launcher
    /// without its icons, decisions.md m6-flakes).
    #[test]
    fn a_dropped_request_wanted_again_is_decoded() {
        let (woke, wakes) = std::sync::mpsc::channel::<()>();
        let waker = Box::new(move || {
            let _ = woke.send(());
        });
        let worker = ImageWorker::spawn(IconTheme::named("StrandTest"), Some(waker)).unwrap();
        let mut store = ImageStore::new(ImageBackend::Worker(worker));
        let k = ImageKey {
            source: "/nonexistent/strand-dropped.png".into(),
            icon: false,
            w: 8,
            h: 8,
            fit: Fit::Contain,
            scale: 1,
            frame: 0,
        };
        // The worker held while the frame wants `k`, then stops wanting
        // it: the worker drops it when it comes to it.
        let (release, held) = std::sync::mpsc::channel::<()>();
        let hold = move || -> Loaded {
            let _ = held.recv();
            Ok(Arc::new(()))
        };
        assert!(store.load_file("hold", hold).is_none());
        store.want(SurfaceId(1), std::slice::from_ref(&k), false);
        assert_eq!(store.pending(), 1);
        store.want(SurfaceId(1), &[], false);
        release.send(()).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let wait = |what: &str| {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(wakes.recv_timeout(left).is_ok(), "no wake for {what}");
        };
        wait("the held job");
        // The drop wakes the render loop too.
        wait("`k` dropped");
        // Wanted again before the render loop polled: in flight to the
        // store, so not asked for again here.
        store.want(SurfaceId(1), std::slice::from_ref(&k), false);
        // The render loop: it polls at each wake, until `k` is in.
        loop {
            store.poll();
            if store.get(&k).is_some() {
                break;
            }
            wait("`k` decoded: it was dropped and never asked for again");
        }
        assert!(
            store.get(&k).is_some_and(|r| r.is_err()),
            "decoded (a missing file)"
        );
        assert_eq!(store.pending(), 0);
    }

    /// (m6-flakes) The same race when the icon theme changed while the
    /// key was in flight: a stale key dropped undecoded that a frame
    /// wants again is sent again (and kept when it comes), not forgotten.
    #[test]
    fn a_dropped_stale_icon_wanted_again_is_decoded() {
        let (woke, wakes) = std::sync::mpsc::channel::<()>();
        let waker = Box::new(move || {
            let _ = woke.send(());
        });
        let worker = ImageWorker::spawn(IconTheme::named("StrandTest"), Some(waker)).unwrap();
        let mut store = ImageStore::new(ImageBackend::Worker(worker));
        let k = ImageKey {
            source: "strand-dropped-icon".into(),
            icon: true,
            w: 8,
            h: 8,
            fit: Fit::Contain,
            scale: 1,
            frame: 0,
        };
        let (release, held) = std::sync::mpsc::channel::<()>();
        let hold = move || -> Loaded {
            let _ = held.recv();
            Ok(Arc::new(()))
        };
        assert!(store.load_file("hold", hold).is_none());
        store.want(SurfaceId(1), std::slice::from_ref(&k), false);
        assert_eq!(store.pending(), 1);
        // The theme changes while `k` is in flight: it is stale.
        assert_eq!(store.invalidate_icons(), vec![SurfaceId(1)]);
        store.want(SurfaceId(1), &[], false);
        release.send(()).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let wait = |what: &str| {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(wakes.recv_timeout(left).is_ok(), "no wake for {what}");
        };
        wait("the held job");
        wait("`k` dropped");
        // The repaint the theme change asked for wants `k` again while
        // it is in flight.
        store.want(SurfaceId(1), std::slice::from_ref(&k), false);
        loop {
            store.poll();
            if store.get(&k).is_some() {
                break;
            }
            wait("`k` decoded: it was dropped stale and never asked for again");
        }
        assert_eq!(store.pending(), 0);
    }

    /// A media source's file job runs on the worker's thread, once while
    /// in flight, and its result is taken once; inline it runs at once.
    #[test]
    fn file_jobs_run_on_the_worker_and_are_taken_once() {
        let worker = ImageWorker::spawn(IconTheme::named("StrandTest"), None).unwrap();
        let store = ImageStore::new(ImageBackend::Worker(worker));
        let job = || -> Loaded {
            let name = std::thread::current().name().map(str::to_string);
            Ok(Arc::new(name))
        };
        // The first job holds the worker until the in-flight case is
        // checked: a job the worker had already finished would be taken.
        let (release, held) = std::sync::mpsc::channel::<()>();
        let first = move || -> Loaded {
            let _ = held.recv();
            job()
        };
        assert!(store.load_file("a", first).is_none(), "sent");
        assert!(
            store
                .load_file("a", || -> Loaded { panic!("sent twice") })
                .is_none(),
            "in flight, not sent again"
        );
        release.send(()).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while !store.files_arrived() {
            assert!(Instant::now() < deadline, "the worker answers");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let got = store.load_file("a", job).unwrap().unwrap();
        let name = got.downcast::<Option<String>>().unwrap();
        assert_eq!(name.as_deref(), Some("strand-image"));
        assert!(!store.files_arrived(), "said once");
        // Taken: asked again, it is read again.
        assert!(store.load_file("a", job).is_none());
        let inline = ImageStore::new(ImageBackend::Inline(IconTheme::named("StrandTest")));
        let got = inline.load_file("a", job).unwrap().unwrap();
        assert_eq!(
            got.downcast::<Option<String>>().unwrap().as_deref(),
            std::thread::current().name()
        );
    }
}
