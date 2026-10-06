//! `material(image:)`: a wallpaper's seed colour, quantised off-thread
//! from a 128 px downscale and cached by content hash.
//!
//! [`Quantiser`] lives on the logic thread and never blocks it: a lookup
//! `stat`s the path (following symlinks) and answers from its index when
//! the file the path resolves to is unchanged; otherwise it hands the
//! path to its worker thread and answers [`Lookup::Pending`] with the
//! last seed it produced, so the old palette holds until the new one is
//! ready. The worker reads and hashes the file (BLAKE3); a hash it has
//! seen (a wallpaper copied, touched or swapped back) costs no decode.
//! Seeds by hash and the path index are kept on disk, so a boot with an
//! unchanged wallpaper is answered at once and never flashes default
//! colours.

use std::collections::{HashMap, VecDeque};
use std::io;
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

/// The longest side images are downscaled to before quantising.
pub const DOWNSCALE: u32 = 128;
/// Images larger than this many pixels are refused (a decoding bomb).
pub const MAX_PIXELS: u64 = 256 * 1024 * 1024 / 4;
/// Colours the quantiser starts from (material-color-utilities' 128).
const QUANTISE_COLORS: usize = 128;
/// Wallpapers remembered (path index, seeds by hash, `.seed` files),
/// most recently used first; older ones are forgotten (a slideshow
/// cannot grow the cache without bound).
pub const MAX_REMEMBERED: usize = 64;
/// How long a wallpaper that gave a seed may be missing before its
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

/// The Material seed colour of an encoded image (PNG, JPEG or WebP):
/// decoded, downscaled to fit [`DOWNSCALE`] px, quantised (Celebi, 128
/// colours) and scored as material-color-utilities does. Deterministic.
pub fn seed_from_bytes(bytes: &[u8]) -> Result<Color, ImageError> {
    let reader = image::ImageReader::new(io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| ImageError::Io(e.to_string()))?;
    let (w, h) = reader
        .into_dimensions()
        .map_err(|e| ImageError::Decode(e.to_string()))?;
    if w as u64 * h as u64 > MAX_PIXELS {
        return Err(ImageError::Decode(format!("{w}×{h} is too large")));
    }
    let img = image::ImageReader::new(io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| ImageError::Io(e.to_string()))?
        .decode()
        .map_err(|e| ImageError::Decode(e.to_string()))?;
    let small = img.thumbnail(DOWNSCALE, DOWNSCALE).into_rgba8();
    seed_from_pixels(
        small
            .pixels()
            .filter(|p| p.0[3] == 255)
            .map(|p| Rgb::new(p.0[0], p.0[1], p.0[2])),
    )
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
        Ok(Stamp {
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
            mtime_ns: m.mtime() as i128 * 1_000_000_000 + m.mtime_nsec() as i128,
            ctime_ns: m.ctime() as i128 * 1_000_000_000 + m.ctime_nsec() as i128,
        })
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

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
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

fn worker(
    dir: Option<PathBuf>,
    jobs: Receiver<Job>,
    done: Sender<Done>,
    waker: Waker,
    quantised: Arc<AtomicUsize>,
    mut known: VecDeque<Known>,
) {
    while let Ok(job) = jobs.recv() {
        let seed = match std::fs::read(&job.path) {
            Err(e) => Err(ImageError::Io(e.to_string()).to_string()),
            Ok(bytes) => {
                let hash = blake3::hash(&bytes);
                // A content seen before (copied, touched, swapped back)
                // costs no decode.
                let cached = known
                    .iter()
                    .find(|k| k.hash == hash)
                    .map(|k| k.seed)
                    .or_else(|| {
                        let d = dir.as_ref()?;
                        let text =
                            std::fs::read_to_string(d.join(format!("{}.seed", hash.to_hex())))
                                .ok()?;
                        Color::from_hex(text.trim())
                    });
                let seed = match cached {
                    Some(c) => Ok(c),
                    None => {
                        quantised.fetch_add(1, Ordering::SeqCst);
                        let r = seed_from_bytes(&bytes);
                        if let (Ok(c), Some(d)) = (&r, &dir) {
                            let path = d.join(format!("{}.seed", hash.to_hex()));
                            if let Err(e) = write_atomic(&path, color_hex(*c).as_bytes()) {
                                log::warn!("caching a wallpaper seed: {e}");
                            }
                        }
                        r.map_err(|e| e.to_string())
                    }
                };
                if let Ok(c) = seed {
                    known.retain(|k| k.path != job.path);
                    known.push_back(Known {
                        path: job.path.clone(),
                        stamp: job.stamp,
                        hash,
                        seed: c,
                    });
                    while known.len() > MAX_REMEMBERED {
                        known.pop_front();
                    }
                    if let Some(d) = &dir
                        && let Err(e) = write_index(d, &known, Some(c))
                    {
                        log::warn!("saving the wallpaper index: {e}");
                    }
                }
                seed
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
            .map(|(i, k)| {
                let e = Entry {
                    stamp: k.stamp,
                    seed: Ok(k.seed),
                    used: i as u64,
                    stale: false,
                };
                (k.path.clone(), e)
            })
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
                    let since = *self.missing.entry(path.to_path_buf()).or_insert_with(|| {
                        let (flag, waker) = (self.grace_over.clone(), self.waker.clone());
                        // Wakes the owner when the grace runs out, so a
                        // file that stays missing is reported.
                        let _ = std::thread::Builder::new()
                            .name("strand-quantise-grace".into())
                            .spawn(move || {
                                std::thread::sleep(MISSING_GRACE);
                                flag.store(true, Ordering::SeqCst);
                                wake(&waker);
                            });
                        Instant::now()
                    });
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

    fn take(&mut self, d: Done) {
        if self.in_flight.get(&d.path) == Some(&d.stamp) {
            self.in_flight.remove(&d.path);
        }
        if let Ok(c) = d.seed {
            self.last = Some(c);
        }
        self.clock += 1;
        self.index.insert(
            d.path,
            Entry {
                stamp: d.stamp,
                seed: d.seed,
                used: self.clock,
                stale: false,
            },
        );
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
