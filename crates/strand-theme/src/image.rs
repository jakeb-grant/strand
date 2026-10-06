//! `material(image:)`: a wallpaper's seed colour, quantised off-thread
//! from a 128 px downscale and cached by content hash.
//!
//! [`Quantiser`] lives on the logic thread and never blocks it: a lookup
//! `stat`s the path (following symlinks) and answers from its index when
//! the file the path resolves to is unchanged; otherwise it hands the
//! path to its worker thread and answers [`Lookup::Pending`] with the
//! last seed it produced, so the old palette holds until the new one is
//! ready. The worker reads and hashes the file (BLAKE3) and decodes it at
//! reduced size (see [`seed_from_reader`]); a hash it has
//! seen (a wallpaper copied, touched or swapped back) costs no decode.
//! Seeds by hash and the path index are kept on disk, so a boot with an
//! unchanged wallpaper is answered at once and never flashes default
//! colours.

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, Seek};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use material_colors::color::Rgb;
use strand_scene::Color;

use crate::material::from_rgb;
use crate::writer::write_atomic;

/// The longest side images are downscaled to before quantising.
pub const DOWNSCALE: u32 = 128;
/// Images with more pixels than this are refused from their header,
/// before anything is decoded (a decoding bomb, or hours of work).
pub const MAX_PIXELS: u64 = 64 * 1024 * 1024;
/// Wallpaper files larger than this are not read.
pub const MAX_FILE_BYTES: u64 = 128 * 1024 * 1024;
/// What a decode that needs the whole frame may allocate: WebP (no Rust
/// decoder decodes it at reduced size) and progressive or lossless JPEG
/// (their coefficients are kept for the whole image). Baseline JPEG is
/// decoded at 1/8 scale and PNG row by row, in well under 1 MB.
pub const FULL_FRAME_BYTES: u64 = 64 * 1024 * 1024;
/// What a reduced-size decode may allocate (a row, the scaled frame).
const STREAM_BYTES: usize = 16 * 1024 * 1024;
/// Colours the quantiser starts from (material-color-utilities' 128).
const QUANTISE_COLORS: usize = 128;
/// Wallpapers remembered (path index, seeds by hash, `.seed` files),
/// most recently used first; older ones are forgotten (a slideshow
/// cannot grow the cache without bound).
pub const MAX_REMEMBERED: usize = 64;
/// How long a wallpaper that gave a seed may be missing, or unreadable
/// (a torn read of a file being copied over in place), before its
/// lookup fails: a file replaced by delete-then-create, or a link being
/// swapped, keeps its palette through the gap.
pub const MISSING_GRACE: Duration = Duration::from_millis(500);

/// Why an image gave no seed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageError {
    Io(String),
    Decode(String),
    Empty,
}

impl std::fmt::Display for ImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImageError::Io(e) => write!(f, "cannot read the image: {e}"),
            ImageError::Decode(e) => write!(f, "cannot decode the image: {e}"),
            ImageError::Empty => f.write_str("the image has no pixels"),
        }
    }
}

impl std::error::Error for ImageError {}

fn decode_err(e: impl std::fmt::Display) -> ImageError {
    ImageError::Decode(e.to_string())
}

fn too_large(w: u64, h: u64) -> ImageError {
    ImageError::Decode(format!("{w}×{h} is too large"))
}

/// The box-filtered downscale an image is decoded into: at most
/// [`DOWNSCALE`] px on its longest side, each cell the average of the
/// source pixels that fall in it. Rows are added as they are decoded, so
/// the full frame is never held.
struct Boxes {
    w: u32,
    h: u32,
    tw: u32,
    th: u32,
    /// Sums of r, g, b, a and the pixel count per cell.
    cells: Vec<[u64; 5]>,
}

impl Boxes {
    fn new(w: u32, h: u32) -> Result<Boxes, ImageError> {
        if w == 0 || h == 0 {
            return Err(ImageError::Empty);
        }
        if w as u64 * h as u64 > MAX_PIXELS {
            return Err(too_large(w as u64, h as u64));
        }
        let long = w.max(h);
        let (tw, th) = if long <= DOWNSCALE {
            (w, h)
        } else {
            let fit =
                |n: u32| ((n as u64 * DOWNSCALE as u64 + long as u64 / 2) / long as u64).max(1);
            (fit(w) as u32, fit(h) as u32)
        };
        Ok(Boxes {
            w,
            h,
            tw,
            th,
            cells: vec![[0; 5]; (tw * th) as usize],
        })
    }

    /// Adds the pixel at (`x`, `y`) of the source image.
    fn add(&mut self, x: u32, y: u32, [r, g, b, a]: [u8; 4]) {
        if x >= self.w || y >= self.h {
            return;
        }
        let cx = (x as u64 * self.tw as u64 / self.w as u64) as u32;
        let cy = (y as u64 * self.th as u64 / self.h as u64) as u32;
        let cell = &mut self.cells[(cy * self.tw + cx) as usize];
        for (s, v) in cell.iter_mut().zip([r, g, b, a, 1]) {
            *s += v as u64;
        }
    }

    /// The opaque cells' colours (a cell with any transparency is left
    /// out, as a translucent pixel's colour is not what shows).
    fn seed(&self) -> Result<Color, ImageError> {
        seed_from_pixels(
            self.cells
                .iter()
                .filter(|[_, _, _, a, n]| *n > 0 && *a == 255 * n)
                .map(|[r, g, b, _, n]| {
                    let avg = |s: u64| ((s + n / 2) / n) as u8;
                    Rgb::new(avg(*r), avg(*g), avg(*b))
                }),
        )
    }
}

/// An encoded image's format, from its first bytes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Format {
    Png,
    Jpeg,
    WebP,
}

fn sniff(r: &mut (impl BufRead + Seek)) -> Result<Format, ImageError> {
    let mut head = [0u8; 12];
    let mut n = 0;
    while n < head.len() {
        match r.read(&mut head[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(ImageError::Io(e.to_string())),
        }
    }
    r.seek(io::SeekFrom::Start(0))
        .map_err(|e| ImageError::Io(e.to_string()))?;
    let head = &head[..n];
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        Ok(Format::Png)
    } else if head.starts_with(&[0xff, 0xd8, 0xff]) {
        Ok(Format::Jpeg)
    } else if head.len() == 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
        Ok(Format::WebP)
    } else {
        Err(ImageError::Decode("not a PNG, JPEG or WebP image".into()))
    }
}

/// The Adam7 passes: (x step, x offset, y step, y offset).
const ADAM7: [(u32, u32, u32, u32); 7] = [
    (8, 0, 8, 0),
    (8, 4, 8, 0),
    (4, 0, 8, 4),
    (4, 2, 4, 0),
    (2, 0, 4, 2),
    (2, 1, 2, 0),
    (1, 0, 2, 1),
];

/// A PNG decoded row by row into `Boxes` (an interlaced one pass by
/// pass): one row is held at a time.
fn png_boxes(r: impl BufRead + Seek) -> Result<Boxes, ImageError> {
    let mut d = png::Decoder::new(r);
    d.set_transformations(png::Transformations::normalize_to_color8());
    d.set_limits(png::Limits {
        bytes: STREAM_BYTES,
    });
    let mut reader = d.read_info().map_err(decode_err)?;
    let (w, h, interlaced) = {
        let i = reader.info();
        (i.width, i.height, i.interlaced)
    };
    let mut boxes = Boxes::new(w, h)?;
    let bpp = match reader.output_color_type() {
        (png::ColorType::Grayscale, _) => 1,
        (png::ColorType::GrayscaleAlpha, _) => 2,
        (png::ColorType::Rgb, _) => 3,
        (png::ColorType::Rgba, _) => 4,
        (png::ColorType::Indexed, _) => {
            return Err(ImageError::Decode("an unexpanded palette".into()));
        }
    };
    let px = |p: &[u8]| -> [u8; 4] {
        match *p {
            [l] => [l, l, l, 255],
            [l, a] => [l, l, l, a],
            [r, g, b] => [r, g, b, 255],
            [r, g, b, a] => [r, g, b, a],
            _ => [0, 0, 0, 0],
        }
    };
    // Where each row lands: plain rows top to bottom; Adam7 rows in the
    // decoder's order (passes with no pixels skipped), checked against
    // what it reports.
    let mut rows = (0..7u8).flat_map(|p| {
        let (xs, xo, ys, yo) = ADAM7[p as usize];
        let samples = w.saturating_sub(xo).div_ceil(xs);
        let lines = if samples == 0 {
            0
        } else {
            h.saturating_sub(yo).div_ceil(ys)
        };
        (0..lines).map(move |line| (p + 1, line, xs, xo, line * ys + yo))
    });
    let mut y = 0;
    while let Some(row) = reader.next_interlaced_row().map_err(decode_err)? {
        let (y_at, xs, xo) = match row.interlace() {
            png::InterlaceInfo::Null(_) => {
                y += 1;
                (y - 1, 1, 0)
            }
            png::InterlaceInfo::Adam7(info) if interlaced => {
                let Some((pass, line, xs, xo, y_at)) = rows.next() else {
                    return Err(ImageError::Decode(
                        "more interlaced rows than expected".into(),
                    ));
                };
                if *info != png::Adam7Info::new(pass, line, w) {
                    return Err(ImageError::Decode("unexpected interlaced row".into()));
                }
                (y_at, xs, xo)
            }
            png::InterlaceInfo::Adam7(_) => {
                return Err(ImageError::Decode("unexpected interlaced row".into()));
            }
        };
        for (i, p) in row.data().chunks_exact(bpp).enumerate() {
            boxes.add(i as u32 * xs + xo, y_at, px(p));
        }
    }
    Ok(boxes)
}

/// A JPEG decoded at reduced size: the DCT is scaled to 1/8, 1/4 or 1/2
/// so that the result is just at least [`DOWNSCALE`] px, never the full
/// frame. Progressive and lossless files keep coefficients for the whole
/// image, so they are refused past [`FULL_FRAME_BYTES`].
fn jpeg_boxes(r: impl BufRead) -> Result<Boxes, ImageError> {
    let mut d = jpeg_decoder::Decoder::new(r);
    d.read_info().map_err(decode_err)?;
    let info = d.info().ok_or_else(|| decode_err("no frame"))?;
    let (w, h) = (info.width as u64, info.height as u64);
    if w * h > MAX_PIXELS {
        return Err(too_large(w, h));
    }
    if info.coding_process != jpeg_decoder::CodingProcess::DctSequential
        && w * h * 3 > FULL_FRAME_BYTES
    {
        return Err(ImageError::Decode(format!(
            "{w}×{h} is too large for a progressive or lossless JPEG; save it as a baseline JPEG or PNG"
        )));
    }
    let (sw, sh) = if info.coding_process == jpeg_decoder::CodingProcess::Lossless {
        (info.width, info.height)
    } else {
        let side = DOWNSCALE.min(u16::MAX as u32) as u16;
        d.scale(side, side).map_err(decode_err)?
    };
    let out = sw as usize * sh as usize * info.pixel_format.pixel_bytes();
    if out > STREAM_BYTES.max(FULL_FRAME_BYTES as usize) {
        return Err(too_large(w, h));
    }
    d.set_max_decoding_buffer_size(out.max(1));
    let data = d.decode().map_err(decode_err)?;
    let mut boxes = Boxes::new(sw as u32, sh as u32)?;
    let bpp = info.pixel_format.pixel_bytes();
    let px = |p: &[u8]| -> [u8; 4] {
        match (info.pixel_format, p) {
            (jpeg_decoder::PixelFormat::L8, [l]) => [*l, *l, *l, 255],
            (jpeg_decoder::PixelFormat::L16, [a, b]) => {
                let l = (u16::from_ne_bytes([*a, *b]) >> 8) as u8;
                [l, l, l, 255]
            }
            (jpeg_decoder::PixelFormat::RGB24, [r, g, b]) => [*r, *g, *b, 255],
            (jpeg_decoder::PixelFormat::CMYK32, [c, m, y, k]) => {
                let ink = |v: u8| ((255 - v as u16) * (255 - *k as u16) / 255) as u8;
                [ink(*c), ink(*m), ink(*y), 255]
            }
            _ => [0, 0, 0, 0],
        }
    };
    for (i, p) in data.chunks_exact(bpp).enumerate() {
        boxes.add(i as u32 % sw as u32, i as u32 / sw as u32, px(p));
    }
    Ok(boxes)
}

/// A WebP decoded whole (no decoder can do less), refused past
/// [`FULL_FRAME_BYTES`] before it allocates.
fn webp_boxes(r: impl BufRead + Seek) -> Result<Boxes, ImageError> {
    let mut reader = image::ImageReader::with_format(r, image::ImageFormat::WebP);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(FULL_FRAME_BYTES);
    reader.limits(limits);
    let img = reader.decode().map_err(decode_err)?;
    let mut boxes = Boxes::new(img.width(), img.height())?;
    match img {
        image::DynamicImage::ImageRgb8(i) => {
            for (x, y, p) in i.enumerate_pixels() {
                boxes.add(x, y, [p.0[0], p.0[1], p.0[2], 255]);
            }
        }
        image::DynamicImage::ImageRgba8(i) => {
            for (x, y, p) in i.enumerate_pixels() {
                boxes.add(x, y, p.0);
            }
        }
        _ => return Err(decode_err("an unexpected WebP pixel format")),
    }
    Ok(boxes)
}

/// The Material seed colour of an encoded image (PNG, JPEG or WebP) read
/// from `r`: decoded at reduced size where the format allows (see
/// [`FULL_FRAME_BYTES`]), box-filtered to fit [`DOWNSCALE`] px,
/// quantised (Celebi, 128 colours) and scored as
/// material-color-utilities does. Deterministic.
pub fn seed_from_reader(mut r: impl BufRead + Seek) -> Result<Color, ImageError> {
    let boxes = match sniff(&mut r)? {
        Format::Png => png_boxes(r)?,
        Format::Jpeg => jpeg_boxes(r)?,
        Format::WebP => webp_boxes(r)?,
    };
    boxes.seed()
}

/// [`seed_from_reader`] over bytes in memory.
pub fn seed_from_bytes(bytes: &[u8]) -> Result<Color, ImageError> {
    seed_from_reader(io::Cursor::new(bytes))
}

/// The seed of already small pixels (opaque sRGB).
pub fn seed_from_pixels(pixels: impl Iterator<Item = Rgb>) -> Result<Color, ImageError> {
    use material_colors::quantize::{Quantizer, QuantizerCelebi};
    use material_colors::score::Score;
    let pixels: Vec<Rgb> = pixels.collect();
    if pixels.is_empty() {
        return Err(ImageError::Empty);
    }
    let result = QuantizerCelebi::quantize(&pixels, QUANTISE_COLORS);
    let ranked = Score::score(&result.color_to_count, None, None, None);
    let seed = ranked.first().copied().ok_or(ImageError::Empty)?;
    Ok(from_rgb(seed))
}

/// What identifies the file a path resolves to without reading it: its
/// device, inode, size, modification and change times (after every
/// symlink), so a replaced file or a swapped link is a new stamp, also
/// when a copy keeps the size and the modification time (`cp -p`,
/// `rsync -t`: writing or renaming always moves the change time).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Stamp {
    pub dev: u64,
    pub ino: u64,
    pub len: u64,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
}

impl Stamp {
    pub fn of(path: &Path) -> io::Result<Stamp> {
        let m = std::fs::metadata(path)?;
        if !m.is_file() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a file"));
        }
        Ok(Stamp::of_metadata(&m))
    }

    fn of_metadata(m: &std::fs::Metadata) -> Stamp {
        Stamp {
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
            mtime_ns: m.mtime() as i128 * 1_000_000_000 + m.mtime_nsec() as i128,
            ctime_ns: m.ctime() as i128 * 1_000_000_000 + m.ctime_nsec() as i128,
        }
    }
}

/// A [`Quantiser::lookup`] answer.
#[derive(Clone, Debug, PartialEq)]
pub enum Lookup {
    /// The seed of the file as it is now.
    Ready(Color),
    /// Being quantised; `last` is the last seed produced (this run or,
    /// persisted, an earlier one), to hold until the new one is ready.
    Pending { last: Option<Color> },
    /// The file is missing or not an image.
    Failed { error: String, last: Option<Color> },
}

#[derive(Clone, Debug)]
struct Entry {
    stamp: Stamp,
    seed: Result<Color, String>,
    /// When the entry was last used (a logical clock), for eviction.
    used: u64,
    /// The watcher saw the file change: read it again whatever its stamp.
    stale: bool,
    /// A read of a file that gave a seed failed (torn by a copy in
    /// place, say): its seed holds until then, and it is read again.
    hold_until: Option<Instant>,
    /// That hold was used: a second failure is reported.
    held: bool,
}

impl Entry {
    fn new(stamp: Stamp, seed: Result<Color, String>, used: u64) -> Entry {
        Entry {
            stamp,
            seed,
            used,
            stale: false,
            hold_until: None,
            held: false,
        }
    }
}

/// A persisted wallpaper: the file as stamped, its content hash and seed.
#[derive(Clone, Debug)]
struct Known {
    path: PathBuf,
    stamp: Stamp,
    hash: blake3::Hash,
    seed: Color,
}

struct Job {
    path: PathBuf,
    stamp: Stamp,
}

struct Done {
    path: PathBuf,
    stamp: Stamp,
    seed: Result<Color, String>,
}

type Waker = Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>;

/// Wallpaper seeds, quantised off-thread (see the module docs).
pub struct Quantiser {
    index: HashMap<PathBuf, Entry>,
    clock: u64,
    in_flight: HashMap<PathBuf, Stamp>,
    /// Wallpapers that gave a seed and are now missing, since when.
    missing: HashMap<PathBuf, Instant>,
    /// Set when a missing wallpaper's grace ran out (see `poll`).
    grace_over: Arc<AtomicBool>,
    last: Option<Color>,
    jobs: Option<Sender<Job>>,
    done: Receiver<Done>,
    waker: Waker,
    quantised: Arc<AtomicUsize>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Quantiser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Quantiser")
            .field("index", &self.index.len())
            .field("in_flight", &self.in_flight.len())
            .finish_non_exhaustive()
    }
}

const INDEX: &str = "index";

fn color_hex(c: Color) -> String {
    let [r, g, b, _] = c.to_rgba8();
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// Parses the persisted index: `last #rrggbb` and `entry dev ino len
/// mtime_ns ctime_ns #rrggbb hash path` lines, least recently used
/// first; anything else (an older format) is skipped.
fn read_index(dir: &Path) -> (VecDeque<Known>, Option<Color>) {
    let mut known = VecDeque::new();
    let mut last = None;
    let Ok(text) = std::fs::read_to_string(dir.join(INDEX)) else {
        return (known, last);
    };
    for line in text.lines() {
        let mut parts = line.splitn(9, ' ');
        match parts.next() {
            Some("last") => last = parts.next().and_then(Color::from_hex),
            Some("entry") => {
                let mut num = || parts.next().and_then(|p| p.parse::<i128>().ok());
                let (Some(dev), Some(ino), Some(len), Some(mtime_ns), Some(ctime_ns)) =
                    (num(), num(), num(), num(), num())
                else {
                    continue;
                };
                let (Some(seed), Some(hash), Some(path)) = (
                    parts.next().and_then(Color::from_hex),
                    parts.next().and_then(|h| blake3::Hash::from_hex(h).ok()),
                    parts.next(),
                ) else {
                    continue;
                };
                let path = PathBuf::from(path);
                known.retain(|k: &Known| k.path != path);
                known.push_back(Known {
                    path,
                    stamp: Stamp {
                        dev: dev as u64,
                        ino: ino as u64,
                        len: len as u64,
                        mtime_ns,
                        ctime_ns,
                    },
                    hash,
                    seed,
                });
            }
            _ => {}
        }
    }
    while known.len() > MAX_REMEMBERED {
        known.pop_front();
    }
    (known, last)
}

fn write_index(dir: &Path, known: &VecDeque<Known>, last: Option<Color>) -> io::Result<()> {
    let mut out = String::from("# strand wallpaper seeds (material-colors 0.5, spec 2021)\n");
    if let Some(l) = last {
        out.push_str(&format!("last {}\n", color_hex(l)));
    }
    for k in known {
        let Some(p) = k.path.to_str() else { continue };
        if p.contains('\n') {
            continue;
        }
        let s = k.stamp;
        out.push_str(&format!(
            "entry {} {} {} {} {} {} {} {p}\n",
            s.dev,
            s.ino,
            s.len,
            s.mtime_ns,
            s.ctime_ns,
            color_hex(k.seed),
            k.hash.to_hex()
        ));
    }
    write_atomic(&dir.join(INDEX), out.as_bytes())?;
    // Seeds no remembered wallpaper has are forgotten with it.
    let keep: std::collections::HashSet<String> = known
        .iter()
        .map(|k| format!("{}.seed", k.hash.to_hex()))
        .collect();
    for e in std::fs::read_dir(dir)?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.ends_with(".seed") && !keep.contains(&name) {
            let _ = std::fs::remove_file(e.path());
        }
    }
    Ok(())
}

/// Holds an advisory lock on the cache directory's index while it is
/// read, merged and rewritten, so two `strand run`s sharing
/// `$XDG_STATE_HOME/strand/palettes` keep each other's entries (each
/// adds its own result to what is on disk) and never prune a seed the
/// other just wrote. Best effort: without a lock, the merge still runs.
fn lock_index(dir: &Path) -> Option<std::fs::File> {
    std::fs::create_dir_all(dir).ok()?;
    let f = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("index.lock"))
        .ok()?;
    f.lock().ok()?;
    Some(f)
}

/// Reads the wallpaper through `f` (opened once: what is hashed is what
/// is decoded) and finds its seed: by content hash when known, else by
/// decoding it. A file written while it was read is reported as torn.
fn read_seed(
    f: &mut std::fs::File,
    known: &VecDeque<Known>,
    dir: Option<&Path>,
    quantised: &AtomicUsize,
) -> Result<(blake3::Hash, Color, bool), ImageError> {
    let io_err = |e: io::Error| ImageError::Io(e.to_string());
    let before = f.metadata().map_err(io_err)?;
    if !before.is_file() {
        return Err(ImageError::Io("not a file".into()));
    }
    if before.len() > MAX_FILE_BYTES {
        return Err(ImageError::Io(format!(
            "{} bytes is larger than {MAX_FILE_BYTES}",
            before.len()
        )));
    }
    let hash = blake3::Hasher::new()
        .update_reader(&mut *f)
        .map_err(io_err)?
        .finalize();
    // A content seen before (copied, touched, swapped back) costs no
    // decode.
    let cached = known
        .iter()
        .find(|k| k.hash == hash)
        .map(|k| k.seed)
        .or_else(|| {
            let text =
                std::fs::read_to_string(dir?.join(format!("{}.seed", hash.to_hex()))).ok()?;
            Color::from_hex(text.trim())
        });
    let (seed, fresh) = match cached {
        Some(c) => (c, false),
        None => {
            quantised.fetch_add(1, Ordering::SeqCst);
            f.seek(io::SeekFrom::Start(0)).map_err(io_err)?;
            (seed_from_reader(io::BufReader::new(&mut *f))?, true)
        }
    };
    let after = f.metadata().map_err(io_err)?;
    if Stamp::of_metadata(&before) != Stamp::of_metadata(&after) {
        return Err(ImageError::Io("the file changed while it was read".into()));
    }
    Ok((hash, seed, fresh))
}

fn worker(
    dir: Option<PathBuf>,
    jobs: Receiver<Job>,
    done: Sender<Done>,
    waker: Waker,
    quantised: Arc<AtomicUsize>,
    mut known: VecDeque<Known>,
) {
    while let Ok(job) = jobs.recv() {
        if let Some(d) = &dir {
            // What other runs sharing the cache learned meanwhile.
            known = merge(read_index(d).0, &known);
        }
        let read = std::fs::File::open(&job.path)
            .map_err(|e| ImageError::Io(e.to_string()))
            .and_then(|mut f| read_seed(&mut f, &known, dir.as_deref(), &quantised));
        let seed = match read {
            Err(e) => Err(e.to_string()),
            Ok((hash, c, fresh)) => {
                let entry = Known {
                    path: job.path.clone(),
                    stamp: job.stamp,
                    hash,
                    seed: c,
                };
                match &dir {
                    None => {
                        known.retain(|k| k.path != job.path);
                        known.push_back(entry);
                        while known.len() > MAX_REMEMBERED {
                            known.pop_front();
                        }
                    }
                    Some(d) => {
                        let _lock = lock_index(d);
                        if fresh {
                            let path = d.join(format!("{}.seed", hash.to_hex()));
                            if let Err(e) = write_atomic(&path, color_hex(c).as_bytes()) {
                                log::warn!("caching a wallpaper seed: {e}");
                            }
                        }
                        let mut merged = read_index(d).0;
                        merged.retain(|k| k.path != job.path);
                        merged.push_back(entry);
                        while merged.len() > MAX_REMEMBERED {
                            merged.pop_front();
                        }
                        if let Err(e) = write_index(d, &merged, Some(c)) {
                            log::warn!("saving the wallpaper index: {e}");
                        }
                        known = merged;
                    }
                }
                Ok(c)
            }
        };
        let sent = done.send(Done {
            path: job.path,
            stamp: job.stamp,
            seed,
        });
        if sent.is_err() {
            return;
        }
        wake(&waker);
    }
}

/// `disk` with the entries only `mine` has added at the old end (most
/// recently used last), at most [`MAX_REMEMBERED`].
fn merge(mut disk: VecDeque<Known>, mine: &VecDeque<Known>) -> VecDeque<Known> {
    for (i, k) in mine.iter().enumerate() {
        if !disk.iter().any(|d| d.path == k.path) {
            disk.insert(i.min(disk.len()), k.clone());
        }
    }
    while disk.len() > MAX_REMEMBERED {
        disk.pop_front();
    }
    disk
}

fn wake(waker: &Waker) {
    let hook = waker.lock().unwrap_or_else(PoisonError::into_inner).clone();
    if let Some(w) = hook {
        w();
    }
}

impl Quantiser {
    /// A quantiser whose caches live in `dir` (`None`: memory only). The
    /// index is read now; the worker thread starts now and idles.
    pub fn new(dir: Option<PathBuf>) -> io::Result<Quantiser> {
        let (known, last) = dir.as_deref().map(read_index).unwrap_or_default();
        let index: HashMap<PathBuf, Entry> = known
            .iter()
            .enumerate()
            .map(|(i, k)| (k.path.clone(), Entry::new(k.stamp, Ok(k.seed), i as u64)))
            .collect();
        let (job_tx, job_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let waker: Waker = Arc::default();
        let quantised = Arc::new(AtomicUsize::new(0));
        let thread = {
            let (dir, waker, quantised) = (dir.clone(), waker.clone(), quantised.clone());
            std::thread::Builder::new()
                .name("strand-quantise".into())
                .spawn(move || worker(dir, job_rx, done_tx, waker, quantised, known))?
        };
        Ok(Quantiser {
            clock: index.len() as u64,
            index,
            in_flight: HashMap::new(),
            missing: HashMap::new(),
            grace_over: Arc::default(),
            last,
            jobs: Some(job_tx),
            done: done_rx,
            waker,
            quantised,
            thread: Some(thread),
        })
    }

    /// Called on the worker thread after each finished job, so the logic
    /// loop can wake and [`Quantiser::poll`].
    pub fn set_waker(&self, f: impl Fn() + Send + Sync + 'static) {
        *self.waker.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(f));
    }

    /// How many images were decoded and quantised (cache misses).
    pub fn quantised(&self) -> usize {
        self.quantised.load(Ordering::SeqCst)
    }

    /// The last seed produced, kept across restarts.
    pub fn last(&self) -> Option<Color> {
        self.last
    }

    /// The seed of the image at `path` (see [`Lookup`]). Never blocks on
    /// the image: only a `stat`.
    pub fn lookup(&mut self, path: &Path) -> Lookup {
        self.poll();
        self.clock += 1;
        let stamp = match Stamp::of(path) {
            Ok(s) => {
                self.missing.remove(path);
                s
            }
            Err(e) => {
                // A wallpaper that gave a seed and just went missing is
                // likely being replaced: hold its palette for a moment.
                let had = self.index.get(path).is_some_and(|e| e.seed.is_ok());
                if e.kind() == io::ErrorKind::NotFound && had {
                    let since = match self.missing.get(path) {
                        Some(t) => *t,
                        None => {
                            // Wakes the owner when the grace runs out, so a
                            // file that stays missing is reported.
                            self.wake_after_grace();
                            let now = Instant::now();
                            self.missing.insert(path.to_path_buf(), now);
                            now
                        }
                    };
                    if since.elapsed() < MISSING_GRACE {
                        return Lookup::Pending { last: self.last };
                    }
                }
                self.index.remove(path);
                return Lookup::Failed {
                    error: ImageError::Io(e.to_string()).to_string(),
                    last: self.last,
                };
            }
        };
        let clock = self.clock;
        if let Some(e) = self.index.get_mut(path)
            && let Some(until) = e.hold_until
        {
            if Instant::now() < until {
                // A torn read: the last seed holds, and it is read again
                // when the grace is over (or the watcher saw it finish).
                return Lookup::Pending { last: self.last };
            }
            e.hold_until = None;
        }
        if let Some(e) = self.index.get_mut(path)
            && e.stamp == stamp
            && !e.stale
        {
            e.used = clock;
            return match &e.seed {
                Ok(c) => Lookup::Ready(*c),
                Err(error) => Lookup::Failed {
                    error: error.clone(),
                    last: self.last,
                },
            };
        }
        if self.in_flight.get(path) != Some(&stamp) {
            let sent = self.jobs.as_ref().is_some_and(|j| {
                j.send(Job {
                    path: path.to_path_buf(),
                    stamp,
                })
                .is_ok()
            });
            if !sent {
                return Lookup::Failed {
                    error: "the quantiser thread has stopped".into(),
                    last: self.last,
                };
            }
            self.in_flight.insert(path.to_path_buf(), stamp);
        }
        Lookup::Pending { last: self.last }
    }

    /// Takes finished jobs. Returns whether any arrived (or a missing
    /// wallpaper's grace ran out: look it up again).
    pub fn poll(&mut self) -> bool {
        let mut any = self.grace_over.swap(false, Ordering::SeqCst);
        while let Ok(d) = self.done.try_recv() {
            self.take(d);
            any = true;
        }
        any
    }

    /// Wakes the owner (with `poll` answering true) once
    /// [`MISSING_GRACE`] is over.
    fn wake_after_grace(&self) {
        let (flag, waker) = (self.grace_over.clone(), self.waker.clone());
        let _ = std::thread::Builder::new()
            .name("strand-quantise-grace".into())
            .spawn(move || {
                std::thread::sleep(MISSING_GRACE);
                flag.store(true, Ordering::SeqCst);
                wake(&waker);
            });
    }

    fn take(&mut self, d: Done) {
        if self.in_flight.get(&d.path) == Some(&d.stamp) {
            self.in_flight.remove(&d.path);
        }
        if d.seed.is_err()
            && let Some(e) = self.index.get_mut(&d.path)
            && e.seed.is_ok()
            && !e.held
        {
            // A wallpaper that gave a seed and now fails to read is most
            // likely being written (`cp new.jpg wall.jpg`): its palette
            // holds for the grace, then it is read again once.
            e.hold_until = Some(Instant::now() + MISSING_GRACE);
            e.held = true;
            e.stale = true;
            self.wake_after_grace();
            return;
        }
        if let Ok(c) = d.seed {
            self.last = Some(c);
        }
        self.clock += 1;
        self.index
            .insert(d.path, Entry::new(d.stamp, d.seed, self.clock));
        while self.index.len() > MAX_REMEMBERED {
            let Some(oldest) = self
                .index
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(p, _)| p.clone())
            else {
                break;
            };
            self.index.remove(&oldest);
        }
    }

    /// How many wallpapers the lookup index holds (at most
    /// [`MAX_REMEMBERED`]).
    pub fn remembered(&self) -> usize {
        self.index.len()
    }

    /// Whether a job is still running.
    pub fn busy(&self) -> bool {
        !self.in_flight.is_empty()
    }

    /// Waits up to `timeout` for every job to finish (tests, `strand
    /// check`). Returns whether none is left.
    pub fn wait(&mut self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while self.busy() {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.done.recv_timeout(left) {
                Ok(d) => self.take(d),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return false,
            }
        }
        true
    }

    /// Forget what is known about `path` (the watcher saw it change), so
    /// the next lookup re-reads it even if its stamp looks the same.
    pub fn invalidate(&mut self, path: &Path) {
        if let Some(e) = self.index.get_mut(path) {
            e.stale = true;
            // The write it was torn by is finished: read it now.
            e.hold_until = None;
        }
    }
}

impl Drop for Quantiser {
    fn drop(&mut self) {
        // Closing the job channel ends the worker after its current job;
        // it is not waited for (its writes are atomic renames).
        self.jobs = None;
        drop(self.thread.take());
    }
}
