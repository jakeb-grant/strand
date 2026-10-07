//! `image` and `icon`: decoded at drawn size into a 6 MB LRU (design.md,
//! "Rendering, performance and memory budget"), off the render thread
//! when a worker runs.
//!
//! An `icon` names an icon of the freedesktop icon theme
//! (`window-close-symbolic`); a symbolic icon draws as a mask in the
//! node's `color`. An `image` takes a file path (PNG, JPEG or SVG; `~/`
//! and `file://` work) or, when its source has no path, an icon name as
//! well (`image h.app.icon`, `image item.icon`). Every decode is at the
//! box's physical size, so the cache holds what is drawn, never a
//! full-resolution original.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use strand_scene::SurfaceId;
use vello_cpu::Pixmap;
use vello_cpu::color::PremulRgba8;

/// Bytes of decoded pixels kept (design.md: "Images decode at drawn size
/// into a 6 MB LRU").
pub const IMAGE_CACHE_BYTES: usize = 6 << 20;

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
}

impl Decoded {
    /// Where the whole pixmap goes so that its content lands where the
    /// fit places the source in a `w × h` box at `(x, y)`: the same rect
    /// when the box is the decode's own size.
    pub fn placed_in(&self, x: f64, y: f64, w: f64, h: f64) -> (f64, f64, f64, f64) {
        let (pw, ph) = (self.pixmap.width() as f64, self.pixmap.height() as f64);
        let (sw, sh) = self.source;
        if !(sw > 0.0 && sh > 0.0 && pw > 0.0 && ph > 0.0) {
            return (x, y, w, h);
        }
        let (oxo, oyo, dwo, dho) = placement(sw, sh, pw, ph, self.fit);
        let (oxn, oyn, dwn, dhn) = placement(sw, sh, w, h, self.fit);
        if !(dwo > 0.0 && dho > 0.0) {
            return (x, y, w, h);
        }
        let (kx, ky) = (dwn / dwo, dhn / dho);
        (x + oxn - oxo * kx, y + oyn - oyo * ky, pw * kx, ph * ky)
    }
}

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
pub fn is_icon(key: &ImageKey) -> bool {
    let src = key.source.trim();
    key.icon
        || !(src.starts_with('/')
            || src.starts_with("~/")
            || src.starts_with("file://")
            || src.starts_with("./")
            || Path::new(src).extension().is_some_and(|e| {
                matches!(
                    e.to_ascii_lowercase().to_str(),
                    Some("png" | "jpg" | "jpeg" | "svg")
                )
            }))
}

/// Resolves `key`'s source to a file.
fn resolve(key: &ImageKey, theme: &IconTheme) -> Result<PathBuf, ImageError> {
    let src = key.source.trim();
    if !is_icon(key) {
        let p = if let Some(rest) = src.strip_prefix("file://") {
            PathBuf::from(percent_decode(rest))
        } else if let Some(rest) = src.strip_prefix("~/") {
            match std::env::var_os("HOME") {
                Some(h) => PathBuf::from(h).join(rest),
                None => PathBuf::from(src),
            }
        } else {
            PathBuf::from(src)
        };
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
}

/// Loads `key`: finds the file, decodes it and fits it into its drawn
/// size.
pub fn load(key: &ImageKey, theme: &IconTheme) -> Result<Decoded, ImageError> {
    let (w, h) = (key.w.clamp(1, MAX_SIDE), key.h.clamp(1, MAX_SIDE));
    let path = resolve(key, theme)?;
    let symbolic = key.source.ends_with("-symbolic")
        || path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.ends_with("-symbolic"));
    let meta = std::fs::metadata(&path).map_err(|e| ImageError::Io(e.to_string()))?;
    if meta.len() > MAX_FILE_BYTES {
        return Err(ImageError::TooLarge);
    }
    let data = std::fs::read(&path).map_err(|e| ImageError::Io(e.to_string()))?;
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
            return Err(ImageError::Decode("not a PNG, JPEG or SVG".into()));
        };
        (
            fit_raster(&src, w, h, key.fit),
            (src.w as f64, src.h as f64),
        )
    };
    Ok(Decoded {
        pixmap: Arc::new(to_pixmap(&out)),
        symbolic,
        source,
        fit: key.fit,
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
        return Ok(Raster { w, h, rgba });
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
    Ok(Raster {
        w: rw,
        h: rh,
        rgba: out,
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
    Ok(Raster { w, h, rgba })
}

/// Where a `sw × sh` source goes in a `w × h` box: its scaled size and
/// offset (logical to the box, may be negative when cropped).
fn placement(sw: f64, sh: f64, w: f64, h: f64, fit: Fit) -> (f64, f64, f64, f64) {
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
    let (sw, sh) = (src.w as f64, src.h as f64);
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
                for k in 0..4 {
                    acc[k] = (a[k] * (1.0 - tx) + b[k] * tx) * (1.0 - ty)
                        + (c[k] * (1.0 - tx) + d[k] * tx) * ty;
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
    Raster { w, h, rgba: out }
}

/// Renders an SVG into a `w × h` box by `fit`.
/// Renders an SVG into a `w × h` box by `fit`, with its own size.
fn render_svg(data: &[u8], w: u32, h: u32, fit: Fit) -> Result<(Raster, (f64, f64)), ImageError> {
    use resvg::{tiny_skia, usvg};
    let tree = usvg::Tree::from_data(data, &usvg::Options::default())
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
    Ok((
        Raster {
            w,
            h,
            rgba: pm.take(),
        },
        (sw, sh),
    ))
}

/// A vello pixmap of premultiplied RGBA, red and blue swapped.
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
#[derive(Debug)]
pub struct ImageWorker {
    requests: Option<Sender<ImageKey>>,
    /// `None`: dropped undecoded, no longer wanted.
    results: Receiver<(ImageKey, Option<Result<Decoded, ImageError>>)>,
    /// The keys some live frame draws (or will once decoded).
    wanted: Arc<Mutex<HashSet<ImageKey>>>,
    thread: Option<JoinHandle<()>>,
}

impl ImageWorker {
    /// Starts the worker; `waker` runs after each result is sent (the
    /// render loop's ping).
    pub fn spawn(theme: IconTheme, waker: Option<Box<dyn Fn() + Send>>) -> std::io::Result<Self> {
        let (req_tx, req_rx) = mpsc::channel::<ImageKey>();
        let (out_tx, out_rx) = mpsc::channel();
        let wanted: Arc<Mutex<HashSet<ImageKey>>> = Arc::default();
        let still = wanted.clone();
        let thread = std::thread::Builder::new()
            .name("strand-image".into())
            .spawn(move || {
                while let Ok(key) = req_rx.recv() {
                    // A request no frame wants any more (a size passed
                    // through, a node gone) is dropped undecoded.
                    let want = still.lock().map(|w| w.contains(&key)).unwrap_or(true);
                    let r = if want {
                        Some(
                            std::panic::catch_unwind(|| load(&key, &theme)).unwrap_or_else(|_| {
                                Err(ImageError::Decode("decoder panicked".into()))
                            }),
                        )
                    } else {
                        None
                    };
                    if out_tx.send((key, r)).is_err() {
                        return;
                    }
                    if want && let Some(w) = &waker {
                        w();
                    }
                }
            })?;
        Ok(Self {
            requests: Some(req_tx),
            results: out_rx,
            wanted,
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
type SourceKey = (String, bool, Fit, u16);

fn source_key(k: &ImageKey) -> SourceKey {
    (k.source.clone(), k.icon, k.fit, k.scale)
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
    /// Per source, the latest decode at any size: drawn scaled while the
    /// one at the drawn size is not there yet (a size spring).
    latest: HashMap<SourceKey, ImageKey>,
    /// Icon decodes in flight when the icon theme changed: their results
    /// are dropped (and asked for again) when they arrive.
    stale: HashSet<ImageKey>,
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
            latest: HashMap::new(),
            stale: HashSet::new(),
        }
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

    /// The latest decode of `key`'s source at another size, with its key:
    /// drawn scaled into the box until `key` itself is decoded.
    pub fn stand_in(&self, key: &ImageKey) -> Option<(&ImageKey, &Decoded)> {
        let k = self.latest.get(&source_key(key))?;
        match self.entries.get(k).map(|e| &e.result) {
            Some(Ok(d)) => Some((k, d)),
            _ => None,
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
    /// with a stand-in is not asked for: the stand-in is drawn scaled
    /// until the size comes to rest, so a spring does not decode every
    /// size it passes through. Returns true if anything was decoded
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
        let mut decoded = false;
        for k in keys {
            self.tick += 1;
            if let Some(e) = self.entries.get_mut(k) {
                e.used = self.tick;
                continue;
            }
            if self.pending.contains(k) || (defer && self.stand_in(k).is_some()) {
                continue;
            }
            match &mut self.backend {
                ImageBackend::Inline(theme) => {
                    let r = load(k, theme);
                    self.insert(k.clone(), r);
                    decoded = true;
                }
                ImageBackend::Worker(w) => {
                    let sent = w
                        .requests
                        .as_ref()
                        .is_some_and(|tx| tx.send(k.clone()).is_ok());
                    if sent {
                        self.pending.insert(k.clone());
                    } else {
                        self.insert(k.clone(), Err(ImageError::Io("no image worker".into())));
                    }
                }
            }
        }
        decoded
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
        }
    }

    /// True if surface `surface`'s last frame draws `key`.
    pub fn drawn_by(&self, surface: SurfaceId, key: &ImageKey) -> bool {
        self.frames.get(&surface).is_some_and(|f| f.contains(key))
    }

    /// Takes the worker's results: the keys that arrived (decoded or
    /// failed; ones dropped undecoded are not among them).
    pub fn poll(&mut self) -> Vec<ImageKey> {
        let ImageBackend::Worker(w) = &self.backend else {
            return Vec::new();
        };
        let got: Vec<_> = w.results.try_iter().collect();
        let mut arrived = Vec::new();
        for (k, r) in got {
            self.pending.remove(&k);
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
        if result.is_ok() {
            self.latest.insert(source_key(&key), key.clone());
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
            let sk = source_key(&v);
            if self.latest.get(&sk) == Some(&v) {
                self.latest.remove(&sk);
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
        Raster { w, h, rgba }
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
        for i in 0..10 {
            let k = ImageKey {
                source: format!("k{i}"),
                icon: false,
                w: side,
                h: side,
                fit: Fit::Contain,
                scale: 1,
            };
            s.insert(
                k,
                Ok(Decoded {
                    pixmap: Arc::new(Pixmap::new(side as u16, side as u16)),
                    symbolic: false,
                    source: (side as f64, side as f64),
                    fit: Fit::Contain,
                }),
            );
        }
        assert!(s.bytes() <= IMAGE_CACHE_BYTES);
        assert_eq!(s.bytes(), 6 * 512 * 512 * 4);
        let has = |s: &ImageStore, i: u32| s.entries.keys().any(|k| k.source == format!("k{i}"));
        assert!(!has(&s, 0) && has(&s, 9), "the oldest go first");
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
        };
        assert!(matches!(
            load(&k("/nonexistent/a.png", false), &theme),
            Err(ImageError::NotFound(_))
        ));
        assert!(load(&k("", true), &theme).is_err());
        assert!(load(&k("../../etc/passwd", true), &theme).is_err());
        assert_eq!(percent_decode("/a%20b.png"), "/a b.png");
    }
}
