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

use std::collections::HashMap;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use material_colors::color::Rgb;
use strand_scene::Color;

use crate::material::from_rgb;

/// The longest side images are downscaled to before quantising.
pub const DOWNSCALE: u32 = 128;
/// Images larger than this many pixels are refused (a decoding bomb).
pub const MAX_PIXELS: u64 = 256 * 1024 * 1024 / 4;
/// Colours the quantiser starts from (material-color-utilities' 128).
const QUANTISE_COLORS: usize = 128;

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
/// device, inode, size and modification time (after every symlink), so a
/// replaced file or a swapped link is a new stamp.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Stamp {
    pub dev: u64,
    pub ino: u64,
    pub len: u64,
    pub mtime_ns: i128,
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
    in_flight: HashMap<PathBuf, Stamp>,
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
/// mtime_ns #rrggbb path` lines; anything else is skipped.
fn read_index(dir: &Path) -> (HashMap<PathBuf, Entry>, Option<Color>) {
    let mut index = HashMap::new();
    let mut last = None;
    let Ok(text) = std::fs::read_to_string(dir.join(INDEX)) else {
        return (index, last);
    };
    for line in text.lines() {
        let mut parts = line.splitn(7, ' ');
        match parts.next() {
            Some("last") => last = parts.next().and_then(Color::from_hex),
            Some("entry") => {
                let mut num = || parts.next().and_then(|p| p.parse::<i128>().ok());
                let (Some(dev), Some(ino), Some(len), Some(mtime_ns)) =
                    (num(), num(), num(), num())
                else {
                    continue;
                };
                let (Some(seed), Some(path)) =
                    (parts.next().and_then(Color::from_hex), parts.next())
                else {
                    continue;
                };
                let stamp = Stamp {
                    dev: dev as u64,
                    ino: ino as u64,
                    len: len as u64,
                    mtime_ns,
                };
                index.insert(
                    PathBuf::from(path),
                    Entry {
                        stamp,
                        seed: Ok(seed),
                    },
                );
            }
            _ => {}
        }
    }
    (index, last)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

fn write_index(dir: &Path, index: &HashMap<PathBuf, Entry>, last: Option<Color>) -> io::Result<()> {
    let mut out = String::from("# strand wallpaper seeds (material-colors 0.5, spec 2021)\n");
    if let Some(l) = last {
        out.push_str(&format!("last {}\n", color_hex(l)));
    }
    let mut paths: Vec<_> = index.iter().collect();
    paths.sort_by(|a, b| a.0.cmp(b.0));
    for (path, e) in paths {
        let (Ok(seed), Some(p)) = (&e.seed, path.to_str()) else {
            continue;
        };
        if p.contains('\n') {
            continue;
        }
        let s = e.stamp;
        out.push_str(&format!(
            "entry {} {} {} {} {} {p}\n",
            s.dev,
            s.ino,
            s.len,
            s.mtime_ns,
            color_hex(*seed)
        ));
    }
    write_atomic(&dir.join(INDEX), out.as_bytes())
}

fn worker(
    dir: Option<PathBuf>,
    jobs: Receiver<Job>,
    done: Sender<Done>,
    waker: Waker,
    quantised: Arc<AtomicUsize>,
    mut index: HashMap<PathBuf, Entry>,
) {
    let mut by_hash: HashMap<blake3::Hash, Color> = HashMap::new();
    while let Ok(job) = jobs.recv() {
        let seed = match std::fs::read(&job.path) {
            Err(e) => Err(ImageError::Io(e.to_string()).to_string()),
            Ok(bytes) => {
                let hash = blake3::hash(&bytes);
                let cached = by_hash.get(&hash).copied().or_else(|| {
                    let d = dir.as_ref()?;
                    let text =
                        std::fs::read_to_string(d.join(format!("{}.seed", hash.to_hex()))).ok()?;
                    Color::from_hex(text.trim())
                });
                match cached {
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
                }
                .inspect(|c| {
                    by_hash.insert(hash, *c);
                })
            }
        };
        if let Ok(c) = seed {
            index.insert(
                job.path.clone(),
                Entry {
                    stamp: job.stamp,
                    seed: Ok(c),
                },
            );
            if let Some(d) = &dir
                && let Err(e) = write_index(d, &index, Some(c))
            {
                log::warn!("saving the wallpaper index: {e}");
            }
        }
        let sent = done.send(Done {
            path: job.path,
            stamp: job.stamp,
            seed,
        });
        if sent.is_err() {
            return;
        }
        let hook = waker.lock().unwrap_or_else(PoisonError::into_inner).clone();
        if let Some(w) = hook {
            w();
        }
    }
}

impl Quantiser {
    /// A quantiser whose caches live in `dir` (`None`: memory only). The
    /// index is read now; the worker thread starts now and idles.
    pub fn new(dir: Option<PathBuf>) -> io::Result<Quantiser> {
        let (index, last) = dir.as_deref().map(read_index).unwrap_or_default();
        let (job_tx, job_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let waker: Waker = Arc::default();
        let quantised = Arc::new(AtomicUsize::new(0));
        let thread = {
            let (dir, waker, quantised) = (dir.clone(), waker.clone(), quantised.clone());
            let index = index.clone();
            std::thread::Builder::new()
                .name("strand-quantise".into())
                .spawn(move || worker(dir, job_rx, done_tx, waker, quantised, index))?
        };
        Ok(Quantiser {
            index,
            in_flight: HashMap::new(),
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
        let stamp = match Stamp::of(path) {
            Ok(s) => s,
            Err(e) => {
                return Lookup::Failed {
                    error: ImageError::Io(e.to_string()).to_string(),
                    last: self.last,
                };
            }
        };
        if let Some(e) = self.index.get(path)
            && e.stamp == stamp
        {
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

    /// Takes finished jobs. Returns whether any arrived.
    pub fn poll(&mut self) -> bool {
        let mut any = false;
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
        self.index.insert(
            d.path,
            Entry {
                stamp: d.stamp,
                seed: d.seed,
            },
        );
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
        self.index.remove(path);
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
