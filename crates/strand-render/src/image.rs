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
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

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

/// Largest source image decoded, in pixels (about 8K × 8K).
const MAX_SOURCE_PIXELS: usize = 64 << 20;

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
    /// A theme by name; `None` is the system's, found on first use.
    named: Option<String>,
    found: std::sync::OnceLock<String>,
}

impl IconTheme {
    /// This theme (falling back to hicolor, as every lookup does).
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            named: Some(name.into()),
            found: std::sync::OnceLock::new(),
        }
    }

    /// The system's theme, read on first use: `$STRAND_ICON_THEME`, else
    /// `gtk-icon-theme-name` in `$XDG_CONFIG_HOME/gtk-3.0/settings.ini`
    /// (or `gtk-4.0`), else Adwaita.
    pub fn system() -> Self {
        Self::default()
    }

    /// The theme's name.
    pub fn name(&self) -> &str {
        if let Some(n) = &self.named {
            return n;
        }
        self.found.get_or_init(system_theme)
    }
}

fn system_theme() -> String {
    if let Ok(t) = std::env::var("STRAND_ICON_THEME")
        && !t.trim().is_empty()
    {
        return t.trim().to_string();
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    for v in ["gtk-3.0", "gtk-4.0"] {
        let Some(path) = config.as_ref().map(|c| c.join(v).join("settings.ini")) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=')
                && k.trim() == "gtk-icon-theme-name"
            {
                let v = v.trim().trim_matches('"');
                if !v.is_empty() {
                    return v.to_string();
                }
            }
        }
    }
    "Adwaita".into()
}

/// Resolves `key`'s source to a file.
fn resolve(key: &ImageKey, theme: &IconTheme) -> Result<PathBuf, ImageError> {
    let src = key.source.trim();
    let path_like = !key.icon
        && (src.starts_with('/')
            || src.starts_with("~/")
            || src.starts_with("file://")
            || src.starts_with("./")
            || Path::new(src).extension().is_some_and(|e| {
                matches!(
                    e.to_ascii_lowercase().to_str(),
                    Some("png" | "jpg" | "jpeg" | "svg")
                )
            }));
    if path_like {
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
    freedesktop_icons::lookup(src)
        .with_theme(theme.name())
        .with_size(size.max(1))
        .with_scale(key.scale.max(1))
        .with_cache()
        .find()
        .ok_or_else(|| ImageError::NotFound(src.into()))
}

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
    let out = if svg {
        render_svg(&data, w, h, key.fit)?
    } else {
        let src = if data.starts_with(b"\x89PNG") {
            decode_png(&data)?
        } else if data.starts_with(&[0xff, 0xd8]) {
            decode_jpeg(&data)?
        } else {
            return Err(ImageError::Decode("not a PNG, JPEG or SVG".into()));
        };
        fit_raster(&src, w, h, key.fit)
    };
    Ok(Decoded {
        pixmap: Arc::new(to_pixmap(&out)),
        symbolic,
    })
}

fn decode_png(data: &[u8]) -> Result<Raster, ImageError> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(data));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec
        .read_info()
        .map_err(|e| ImageError::Decode(e.to_string()))?;
    let info = reader.info();
    if (info.width as usize).saturating_mul(info.height as usize) > MAX_SOURCE_PIXELS {
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
    let px = &buf[..frame.buffer_size()];
    match frame.color_type {
        png::ColorType::Rgba => rgba.extend_from_slice(&px[..n * 4]),
        png::ColorType::Rgb => {
            for p in px.chunks_exact(3).take(n) {
                rgba.extend_from_slice(&[p[0], p[1], p[2], 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for p in px.chunks_exact(2).take(n) {
                rgba.extend_from_slice(&[p[0], p[0], p[0], p[1]]);
            }
        }
        png::ColorType::Grayscale => {
            for &g in px.iter().take(n) {
                rgba.extend_from_slice(&[g, g, g, 255]);
            }
        }
        png::ColorType::Indexed => {
            return Err(ImageError::Decode("indexed PNG not expanded".into()));
        }
    }
    if rgba.len() != n * 4 {
        return Err(ImageError::Decode("short PNG".into()));
    }
    premultiply(&mut rgba);
    Ok(Raster { w, h, rgba })
}

fn decode_jpeg(data: &[u8]) -> Result<Raster, ImageError> {
    use zune_jpeg::JpegDecoder;
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;
    let opts = DecoderOptions::default()
        .jpeg_set_out_colorspace(ColorSpace::RGBA)
        .set_max_width(16384)
        .set_max_height(16384);
    let mut dec = JpegDecoder::new_with_options(std::io::Cursor::new(data), opts);
    dec.decode_headers()
        .map_err(|e| ImageError::Decode(format!("{e:?}")))?;
    let (w, h) = dec
        .dimensions()
        .ok_or_else(|| ImageError::Decode("no JPEG size".into()))?;
    if w.saturating_mul(h) > MAX_SOURCE_PIXELS {
        return Err(ImageError::TooLarge);
    }
    let rgba = dec
        .decode()
        .map_err(|e| ImageError::Decode(format!("{e:?}")))?;
    if rgba.len() != w * h * 4 {
        return Err(ImageError::Decode("short JPEG".into()));
    }
    Ok(Raster {
        w: w as u32,
        h: h as u32,
        rgba,
    })
}

fn premultiply(rgba: &mut [u8]) {
    for p in rgba.chunks_exact_mut(4) {
        let a = p[3] as u32;
        if a < 255 {
            for c in &mut p[..3] {
                *c = ((*c as u32 * a + 127) / 255) as u8;
            }
        }
    }
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
fn render_svg(data: &[u8], w: u32, h: u32, fit: Fit) -> Result<Raster, ImageError> {
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
    Ok(Raster {
        w,
        h,
        rgba: pm.take(),
    })
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
    results: Receiver<(ImageKey, Result<Decoded, ImageError>)>,
    thread: Option<JoinHandle<()>>,
}

impl ImageWorker {
    /// Starts the worker; `waker` runs after each result is sent (the
    /// render loop's ping).
    pub fn spawn(theme: IconTheme, waker: Option<Box<dyn Fn() + Send>>) -> std::io::Result<Self> {
        let (req_tx, req_rx) = mpsc::channel::<ImageKey>();
        let (out_tx, out_rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("strand-image".into())
            .spawn(move || {
                while let Ok(key) = req_rx.recv() {
                    let r = std::panic::catch_unwind(|| load(&key, &theme))
                        .unwrap_or_else(|_| Err(ImageError::Decode("decoder panicked".into())));
                    if out_tx.send((key, r)).is_err() {
                        return;
                    }
                    if let Some(w) = &waker {
                        w();
                    }
                }
            })?;
        Ok(Self {
            requests: Some(req_tx),
            results: out_rx,
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

/// Decoded images by key, within [`IMAGE_CACHE_BYTES`].
#[derive(Debug)]
pub struct ImageStore {
    backend: ImageBackend,
    entries: HashMap<ImageKey, Entry>,
    pending: HashSet<ImageKey>,
    bytes: usize,
    tick: u64,
    /// Keys the frame being flattened uses: never evicted for another.
    frame: HashSet<ImageKey>,
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
            frame: HashSet::new(),
        }
    }

    pub fn set_backend(&mut self, backend: ImageBackend) {
        self.backend = backend;
        self.pending.clear();
    }

    /// The decoded image of `key`, if it is ready (`Some(Err)` when it
    /// failed: nothing is drawn).
    pub fn get(&self, key: &ImageKey) -> Option<&Result<Decoded, ImageError>> {
        self.entries.get(key).map(|e| &e.result)
    }

    /// Bytes of decoded pixels held.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Decodes in flight.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// The frame flattened wants `keys`: they move to the front of the
    /// LRU, and the missing ones are decoded (inline at once, or on the
    /// worker). Returns true if anything was decoded inline.
    pub fn want(&mut self, keys: &[ImageKey]) -> bool {
        self.frame = keys.iter().cloned().collect();
        let mut decoded = false;
        for k in keys {
            self.tick += 1;
            if let Some(e) = self.entries.get_mut(k) {
                e.used = self.tick;
                continue;
            }
            if self.pending.contains(k) {
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

    /// Takes the worker's results; true if any arrived.
    pub fn poll(&mut self) -> bool {
        let ImageBackend::Worker(w) = &self.backend else {
            return false;
        };
        let got: Vec<_> = w.results.try_iter().collect();
        let any = !got.is_empty();
        for (k, r) in got {
            self.pending.remove(&k);
            self.insert(k, r);
        }
        any
    }

    fn insert(&mut self, key: ImageKey, result: Result<Decoded, ImageError>) {
        let bytes = match &result {
            Ok(d) => d.pixmap.width() as usize * d.pixmap.height() as usize * 4,
            Err(_) => 0,
        };
        self.tick += 1;
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
        // Least recently used first, never what this frame draws.
        while self.bytes > IMAGE_CACHE_BYTES {
            let victim = self
                .entries
                .iter()
                .filter(|(k, e)| e.bytes > 0 && !self.frame.contains(*k))
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| k.clone());
            let Some(v) = victim else { break };
            if let Some(e) = self.entries.remove(&v) {
                self.bytes -= e.bytes;
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
                }),
            );
        }
        assert!(s.bytes() <= IMAGE_CACHE_BYTES);
        assert_eq!(s.bytes(), 6 * 512 * 512 * 4);
        let has = |s: &ImageStore, i: u32| s.entries.keys().any(|k| k.source == format!("k{i}"));
        assert!(!has(&s, 0) && has(&s, 9), "the oldest go first");
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
