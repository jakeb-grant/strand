//! `state x = … persist`: values kept across restarts.
//!
//! Each persisted cell is one small file under `$XDG_STATE_HOME/strand/
//! persist/` (or `~/.local/state/strand/persist/`), named by the cell's path
//! (`launcher.query`, `bar.dnd`: the same `file.name` path `export` uses).
//! The file stores the value's bytes together with a hash of the default
//! it was declared with, so a changed default can be noticed ("Persistence
//! and settings" in the design):
//!
//! * nothing stored: the default;
//! * stored under the same default: the stored value;
//! * the default changed and the stored value *was* the old default (never
//!   changed by the user): the new default is adopted;
//! * the default changed and the user had changed the value: it is kept and
//!   reported once ([`Diagnostic::PersistDefaultChanged`], the overlay's
//!   `launcher.query: kept "fir" (default changed) [reset]`).
//!
//! Values are opaque bytes: the VM encodes and decodes its own values
//! ([`Runtime::persisted`] takes the codec). A file that cannot be read back
//! (bad header, checksum mismatch, a value that no longer decodes because
//! the type changed) is moved aside to `<name>.corrupt` and the default is
//! used, with [`Diagnostic::PersistFailed`]. Writes are atomic (temp file,
//! `fsync`, rename, directory `fsync`), so a crash never leaves a torn
//! file, and debounced by [`PERSIST_DEBOUNCE`] of logic time; a pending
//! write is flushed when the cell's owner is disposed (unmount, shutdown).

use std::cell::RefCell;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::error::Error;
use crate::runtime::{Diagnostic, NodeId, Runtime, WeakRuntime};
use crate::signal::Signal;

/// Quiet time after the last change before a persisted value is written
/// (a slider drag writes once, when it stops).
pub const PERSIST_DEBOUNCE: Duration = Duration::from_millis(250);

/// First line of every persisted file.
const MAGIC: &str = "strand-persist 1";

/// Longest file name used as is; longer paths are shortened with a hash.
const MAX_NAME: usize = 200;

/// Why persisted storage failed. Comparable and cheap to clone, so it can
/// sit in a [`Diagnostic`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PersistError {
    /// Neither `$XDG_STATE_HOME` nor `$HOME` is set.
    NoStateDir,
    /// The cell path is empty.
    EmptyPath,
    /// Reading, writing or renaming failed.
    Io {
        /// The file or directory.
        path: PathBuf,
        /// The OS error.
        message: Arc<str>,
    },
    /// The file exists but is not a valid persisted value (moved aside to
    /// `<name>.corrupt`).
    Corrupt {
        /// The file.
        path: PathBuf,
        /// What was wrong.
        reason: Arc<str>,
    },
}

impl fmt::Display for PersistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoStateDir => f.write_str("neither XDG_STATE_HOME nor HOME is set"),
            Self::EmptyPath => f.write_str("empty persist path"),
            Self::Io { path, message } => write!(f, "{}: {message}", path.display()),
            Self::Corrupt { path, reason } => {
                write!(f, "{}: corrupt persisted value ({reason})", path.display())
            }
        }
    }
}

impl std::error::Error for PersistError {}

fn io_error(path: &Path, e: &std::io::Error) -> PersistError {
    PersistError::Io {
        path: path.to_path_buf(),
        message: Arc::from(e.to_string()),
    }
}

/// A stable 64-bit hash (FNV-1a) of a value's bytes: the same in every
/// build, so a hash written by one version is understood by the next.
pub fn value_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// A value read back from the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stored {
    /// The value's bytes.
    pub value: Vec<u8>,
    /// [`value_hash`] of the default it was saved under.
    pub default_hash: u64,
}

/// What [`PersistStore::restore`] decided for a cell at startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Restore {
    /// Nothing stored: the default.
    Default,
    /// The stored value, saved under the same default.
    Stored(Vec<u8>),
    /// The default changed and the stored value was the old default: the
    /// new default is adopted (the stale file is removed).
    Adopted,
    /// The default changed but the stored value had been changed: it is
    /// kept, and the file is re-stamped with the new default so this is
    /// reported once.
    KeptOverNewDefault(Vec<u8>),
    /// The file could not be read back: the default (a corrupt file was
    /// moved aside).
    Failed(PersistError),
}

/// Where persisted values live. Cheap to clone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistStore {
    dir: PathBuf,
}

/// Distinct temp names within one process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl PersistStore {
    /// A store keeping its files in `dir` (created on the first write).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// `$XDG_STATE_HOME/strand/persist`, or `$HOME/.local/state/strand/
    /// persist` when `XDG_STATE_HOME` is unset or not absolute (as the XDG
    /// spec says to ignore relative paths).
    pub fn from_env() -> Result<Self, PersistError> {
        Self::from_vars(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
    }

    /// [`PersistStore::from_env`] with the variables given.
    pub fn from_vars(
        xdg_state_home: Option<std::ffi::OsString>,
        home: Option<std::ffi::OsString>,
    ) -> Result<Self, PersistError> {
        let state = xdg_state_home
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                home.map(PathBuf::from)
                    .filter(|p| p.is_absolute())
                    .map(|h| h.join(".local/state"))
            })
            .ok_or(PersistError::NoStateDir)?;
        Ok(Self::new(state.join("strand").join("persist")))
    }

    /// The directory holding the files.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The file for a cell path. Bytes outside `[A-Za-z0-9_.-]` (and a
    /// leading `.`) are percent-escaped, so a path can never name a file
    /// outside the directory, a hidden file or a temp file; very long
    /// paths are shortened with their hash.
    pub fn file_of(&self, path: &str) -> Result<PathBuf, PersistError> {
        if path.is_empty() {
            return Err(PersistError::EmptyPath);
        }
        let mut name = String::with_capacity(path.len());
        for (i, b) in path.bytes().enumerate() {
            let plain = b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.' && i > 0;
            if plain {
                name.push(char::from(b));
            } else {
                name.push_str(&format!("%{b:02X}"));
            }
        }
        if name.len() > MAX_NAME {
            let cut = (0..=MAX_NAME - 20)
                .rev()
                .find(|&i| name.is_char_boundary(i))
                .unwrap_or(0);
            name = format!("{}~{:016x}", &name[..cut], value_hash(path.as_bytes()));
        }
        Ok(self.dir.join(name))
    }

    /// Read a stored value. `Ok(None)` when nothing is stored; a file that
    /// is not a valid persisted value is moved aside to `<name>.corrupt`
    /// and reported as [`PersistError::Corrupt`].
    pub fn load(&self, path: &str) -> Result<Option<Stored>, PersistError> {
        let file = self.file_of(path)?;
        let bytes = match fs::read(&file) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_error(&file, &e)),
        };
        match parse(&bytes) {
            Ok(stored) => Ok(Some(stored)),
            Err(reason) => {
                let mut aside = file.clone().into_os_string();
                aside.push(".corrupt");
                // Best effort: the default is used either way, and the next
                // write replaces the file.
                let _ = fs::rename(&file, &aside);
                Err(PersistError::Corrupt {
                    path: file,
                    reason: Arc::from(reason),
                })
            }
        }
    }

    /// Store `value` for `path`, stamped with the hash of `default`.
    /// Atomic: a reader sees the old file or the new one, never a mix.
    pub fn save(&self, path: &str, default: &[u8], value: &[u8]) -> Result<(), PersistError> {
        self.save_hashed(path, value_hash(default), value)
    }

    fn save_hashed(&self, path: &str, default_hash: u64, value: &[u8]) -> Result<(), PersistError> {
        let file = self.file_of(path)?;
        create_private_dir(&self.dir)?;
        let mut body = format!(
            "{MAGIC}\ndefault {default_hash:016x}\ncheck {:016x}\n\n",
            value_hash(value)
        )
        .into_bytes();
        body.extend_from_slice(value);
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let temp = self.dir.join(format!(
            ".{name}.tmp.{}.{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let written = (|| {
            let mut f = fs::File::create(&temp)?;
            f.write_all(&body)?;
            f.sync_all()?;
            fs::rename(&temp, &file)
        })();
        if let Err(e) = written {
            let _ = fs::remove_file(&temp);
            return Err(io_error(&file, &e));
        }
        // Make the rename itself durable; failing here loses nothing that
        // is not already on its way to disk.
        if let Ok(d) = fs::File::open(&self.dir) {
            let _ = d.sync_all();
        }
        Ok(())
    }

    /// Forget the stored value (`@reset`, the inspector's "reset").
    pub fn remove(&self, path: &str) -> Result<(), PersistError> {
        let file = self.file_of(path)?;
        match fs::remove_file(&file) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_error(&file, &e)),
        }
    }

    /// Decide a cell's starting value from what is stored and its declared
    /// `default` (encoded). See [`Restore`].
    pub fn restore(&self, path: &str, default: &[u8]) -> Restore {
        let stored = match self.load(path) {
            Ok(None) => return Restore::Default,
            Ok(Some(s)) => s,
            Err(e) => return Restore::Failed(e),
        };
        let new_default = value_hash(default);
        if stored.default_hash == new_default {
            return Restore::Stored(stored.value);
        }
        if value_hash(&stored.value) == stored.default_hash {
            // Never changed from the old default: take the new one. The
            // stale file would only say the same again next time.
            let _ = self.remove(path);
            return Restore::Adopted;
        }
        // Kept; re-stamp so the change of default is reported once.
        let _ = self.save_hashed(path, new_default, &stored.value);
        Restore::KeptOverNewDefault(stored.value)
    }
}

/// Create `dir` (and parents) readable only by the user.
fn create_private_dir(dir: &Path) -> Result<(), PersistError> {
    use std::os::unix::fs::DirBuilderExt;
    if dir.is_dir() {
        return Ok(());
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| io_error(dir, &e))
}

/// Parse a persisted file: magic, `default <hex>`, `check <hex>`, a blank
/// line, then the value bytes.
fn parse(bytes: &[u8]) -> Result<Stored, String> {
    let mut rest = bytes;
    let mut line = || -> Result<&str, String> {
        let end = rest
            .iter()
            .position(|&b| b == b'\n')
            .ok_or("truncated header")?;
        let l = std::str::from_utf8(&rest[..end]).map_err(|_| "header is not text")?;
        rest = &rest[end + 1..];
        Ok(l)
    };
    if line()? != MAGIC {
        return Err("not a strand persist file".into());
    }
    let hex = |l: &str, key: &str| -> Result<u64, String> {
        l.strip_prefix(key)
            .and_then(|h| h.strip_prefix(' '))
            .filter(|h| h.len() == 16)
            .and_then(|h| u64::from_str_radix(h, 16).ok())
            .ok_or_else(|| format!("bad `{key}` line"))
    };
    let default_hash = hex(line()?, "default")?;
    let check = hex(line()?, "check")?;
    if !line()?.is_empty() {
        return Err("missing blank line after the header".into());
    }
    if value_hash(rest) != check {
        return Err("checksum mismatch".into());
    }
    Ok(Stored {
        value: rest.to_vec(),
        default_hash,
    })
}

/// A persisted cell: [`Runtime::persisted`].
#[derive(Debug)]
pub struct Persisted<T> {
    /// The state cell (an ordinary [`Signal`]).
    pub signal: Signal<T>,
    /// How its starting value was chosen.
    pub restored: Restore,
}

type Encode<T> = Box<dyn Fn(&T) -> Vec<u8>>;

/// Write-behind for one persisted cell.
struct Writer<T> {
    store: PersistStore,
    path: Arc<str>,
    cell: NodeId,
    default_hash: u64,
    encode: Encode<T>,
    /// What the file holds now (the default when nothing is stored).
    baseline: RefCell<Vec<u8>>,
    /// Encoded value not yet written.
    pending: RefCell<Option<Vec<u8>>>,
    rt: WeakRuntime,
}

impl<T> Writer<T> {
    /// The value changed: remember it unless the file already says it.
    fn note(&self, value: &T) {
        let bytes = (self.encode)(value);
        let fresh = *self.baseline.borrow() != bytes;
        *self.pending.borrow_mut() = fresh.then_some(bytes);
    }

    /// Write the pending value, if any.
    fn flush(&self) {
        let Some(bytes) = self.pending.borrow_mut().take() else {
            return;
        };
        match self
            .store
            .save_hashed(&self.path, self.default_hash, &bytes)
        {
            Ok(()) => *self.baseline.borrow_mut() = bytes,
            Err(error) => {
                if let Some(rt) = self.rt.upgrade() {
                    rt.diagnose(Diagnostic::PersistFailed {
                        cell: self.cell,
                        path: self.path.clone(),
                        error,
                    });
                }
            }
        }
    }
}

impl Runtime {
    /// `state x = default persist`: a [`Signal`] whose value survives
    /// restarts, stored under `path` (the cell's `file.name` path) in
    /// `store`. `encode`/`decode` are the VM's codec for the value (any
    /// stable byte encoding).
    ///
    /// The starting value follows [`PersistStore::restore`]; a kept value
    /// over a changed default reports [`Diagnostic::PersistDefaultChanged`],
    /// and a file that cannot be read (or a value that no longer decodes,
    /// such as after a type change) starts from the default and reports
    /// [`Diagnostic::PersistFailed`]. Changes are written
    /// [`PERSIST_DEBOUNCE`] after the last one (logic time, so the host's
    /// `tick` drives it), and a pending write is flushed when the current
    /// owner is disposed or at [`Runtime::shutdown`].
    pub fn persisted<T, E, D>(
        &self,
        store: &PersistStore,
        path: &str,
        default: T,
        encode: E,
        decode: D,
    ) -> Persisted<T>
    where
        T: Clone + PartialEq + 'static,
        E: Fn(&T) -> Vec<u8> + 'static,
        D: Fn(&[u8]) -> Option<T>,
    {
        let path: Arc<str> = Arc::from(path);
        let default_bytes = encode(&default);
        let mut restored = store.restore(&path, &default_bytes);
        let mut baseline = default_bytes.clone();
        let initial = match &restored {
            Restore::Stored(bytes) | Restore::KeptOverNewDefault(bytes) => match decode(bytes) {
                Some(v) => {
                    baseline.clone_from(bytes);
                    v
                }
                None => {
                    let file = store.file_of(&path).unwrap_or_default();
                    restored = Restore::Failed(PersistError::Corrupt {
                        path: file,
                        reason: Arc::from("the stored value does not decode as this cell's type"),
                    });
                    default.clone()
                }
            },
            Restore::Default | Restore::Adopted | Restore::Failed(_) => default.clone(),
        };
        let signal = self.signal(initial);
        self.set_name(signal.id(), path.clone());
        match &restored {
            Restore::KeptOverNewDefault(_) => self.diagnose(Diagnostic::PersistDefaultChanged {
                cell: signal.id(),
                path: path.clone(),
            }),
            Restore::Failed(error) => self.diagnose(Diagnostic::PersistFailed {
                cell: signal.id(),
                path: path.clone(),
                error: error.clone(),
            }),
            _ => {}
        }
        let writer = Rc::new(Writer {
            store: store.clone(),
            path,
            cell: signal.id(),
            default_hash: crate::persist::value_hash(&default_bytes),
            encode: Box::new(encode),
            baseline: RefCell::new(baseline),
            pending: RefCell::new(None),
            rt: self.downgrade(),
        });
        let w = writer.clone();
        let saver = writer.clone();
        self.on_change_after(
            move |rt| {
                let v = signal.get(rt)?;
                w.note(&v);
                Ok(v)
            },
            PERSIST_DEBOUNCE,
            move |_| {
                saver.flush();
                Ok(())
            },
        );
        self.on_cleanup(move || writer.flush());
        Persisted { signal, restored }
    }

    /// [`Runtime::persisted`] for values with a built-in text encoding
    /// ([`PersistValue`]).
    pub fn persisted_value<T>(&self, store: &PersistStore, path: &str, default: T) -> Persisted<T>
    where
        T: PersistValue + Clone + PartialEq + 'static,
    {
        self.persisted(store, path, default, T::encode, T::decode)
    }
}

/// A plain text encoding for common value types, for Rust-side state and
/// tests; the VM brings its own codec.
pub trait PersistValue: Sized {
    /// The value's bytes.
    fn encode(&self) -> Vec<u8>;
    /// The value back, or `None` if the bytes are not one.
    fn decode(bytes: &[u8]) -> Option<Self>;
}

macro_rules! persist_by_display {
    ($($t:ty),*) => {$(
        impl PersistValue for $t {
            fn encode(&self) -> Vec<u8> {
                self.to_string().into_bytes()
            }
            fn decode(bytes: &[u8]) -> Option<Self> {
                std::str::from_utf8(bytes).ok()?.parse().ok()
            }
        }
    )*};
}

persist_by_display!(bool, i32, i64, u32, u64, f64);

impl PersistValue for String {
    fn encode(&self) -> Vec<u8> {
        self.clone().into_bytes()
    }
    fn decode(bytes: &[u8]) -> Option<Self> {
        String::from_utf8(bytes.to_vec()).ok()
    }
}

/// Errors from the persist layer as graph errors (for handlers that call
/// the store directly).
impl From<PersistError> for Error {
    fn from(e: PersistError) -> Self {
        Error::failed(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_are_stable_fnv1a() {
        // Reference values of FNV-1a 64.
        assert_eq!(value_hash(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(value_hash(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(value_hash(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn paths_cannot_escape_the_directory() {
        let store = PersistStore::new("/state");
        let name = |p: &str| {
            store
                .file_of(p)
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };
        assert_eq!(name("launcher.query"), "launcher.query");
        assert_eq!(name("../etc/passwd"), "%2E.%2Fetc%2Fpasswd");
        assert_eq!(name(".hidden"), "%2Ehidden");
        assert_eq!(name("bar[DP-1].pins"), "bar%5BDP-1%5D.pins");
        assert_eq!(name("ü"), "%C3%BC");
        let long = "x".repeat(500);
        let n = name(&long);
        assert!(n.len() <= MAX_NAME, "{}", n.len());
        assert_ne!(
            n,
            name(&"x".repeat(501)),
            "distinct long paths stay distinct"
        );
        assert_eq!(store.file_of(""), Err(PersistError::EmptyPath));
        for p in ["../etc/passwd", ".hidden", &long] {
            assert_eq!(
                store.file_of(p).unwrap().parent(),
                Some(Path::new("/state"))
            );
        }
    }

    #[test]
    fn the_header_is_checked() {
        let good = b"strand-persist 1\ndefault 0000000000000001\ncheck af63dc4c8601ec8c\n\na";
        assert_eq!(
            parse(good),
            Ok(Stored {
                value: b"a".to_vec(),
                default_hash: 1
            })
        );
        for bad in [
            &b""[..],
            b"strand-persist 2\n",
            b"strand-persist 1\ndefault 1\ncheck af63dc4c8601ec8c\n\na",
            b"strand-persist 1\ndefault 0000000000000001\ncheck af63dc4c8601ec8c\n\nb",
            b"strand-persist 1\ndefault 0000000000000001\ncheck af63dc4c8601ec8c\nx\na",
            b"strand-persist 1\ndefault 0000000000000001\ncheck af63dc4c8601ec8c",
        ] {
            assert!(parse(bad).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
    }
}
