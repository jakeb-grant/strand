//! The clipboard's history and its files (decisions.md m6-service-gaps):
//! newest first, each content once (a repeat moves its entry to the
//! front, keeping its `id`), capped by entries and by bytes, images
//! included. It lives in `<state>/strand/clipboard/` (directory 0700):
//! `history.json` (0600: texts inline, ids, times) and `images/` (each
//! image a 0600 file named by its id and a time, so a path is never
//! reused, not even after `clear`). Every write is atomic (a temporary
//! file, synced, renamed, the directory synced), on the service's own
//! thread. `images/` holds only the files `history.json` names: a file
//! no entry names (a crash between an image's write and the save, or
//! the images of a corrupt file) goes at the next load or save, and a
//! temporary file a save cut short goes at the next load, save or
//! clear, so no copied data outlives the history that held it.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::ClipboardEntry;
use crate::notifications::Date;

/// How much the history keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// Entries at most.
    pub entries: usize,
    /// Bytes at most, every text and image together.
    pub total_bytes: u64,
    /// The largest text kept.
    pub text_bytes: usize,
    /// The largest image kept.
    pub image_bytes: usize,
}

impl Caps {
    /// The owner's caps (decisions.md m6-service-gaps; the owner may
    /// overrule them): 100 entries, 64 MiB, 1 MiB a text, 16 MiB an
    /// image.
    pub const DEFAULT: Caps = Caps {
        entries: 100,
        total_bytes: 64 << 20,
        text_bytes: 1 << 20,
        image_bytes: 16 << 20,
    };
}

/// One entry as kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stored {
    pub id: i64,
    pub what: What,
    /// Its size in bytes.
    pub size: u64,
    /// `strand_core::persist::value_hash` of its bytes.
    pub hash: u64,
    /// When it was last copied, seconds since the epoch.
    pub time: i64,
}

/// A text, or an image's file (its name in `images/`) and type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum What {
    Text(String),
    Image { file: String, mime: String },
}

/// The history, in memory and on disk.
#[derive(Debug)]
pub struct History {
    dir: PathBuf,
    caps: Caps,
    /// Newest first.
    entries: Vec<Stored>,
    next_id: i64,
}

/// `history.json`.
#[derive(Serialize, Deserialize)]
struct FileV1 {
    version: u32,
    next_id: i64,
    entries: Vec<EntryV1>,
}

#[derive(Serialize, Deserialize)]
struct EntryV1 {
    id: i64,
    /// `text` or `image`.
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(default)]
    mime: String,
    size: u64,
    hash: u64,
    time_unix: i64,
}

const FILE: &str = "history.json";
const IMAGES: &str = "images";

/// `clipboard` in strand's state directory (`$STRAND_STATE_DIR`, else
/// `$XDG_STATE_HOME/strand`, else `$HOME/.local/state/strand`, as
/// `strand_core::persist::state_dir_from` finds it from these values);
/// `None` without any of them.
pub fn default_dir(
    strand_state_dir: Option<std::ffi::OsString>,
    xdg_state_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    strand_core::persist::state_dir_from(strand_state_dir, xdg_state_home, home)
        .map(|s| s.join("clipboard"))
}

/// The history's directory under a state directory.
pub fn dir_in(state: &Path) -> PathBuf {
    state.join("strand").join("clipboard")
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// A plain file name (no directory part): what an entry's `file` must be.
fn plain_name(name: &str) -> bool {
    !name.is_empty() && Path::new(name).file_name() == Some(std::ffi::OsStr::new(name))
}

impl History {
    /// An empty history kept in `dir` (nothing is read or written yet).
    pub fn new(dir: PathBuf, caps: Caps) -> History {
        History {
            dir,
            caps,
            entries: Vec::new(),
            next_id: 1,
        }
    }

    /// The history kept in `dir`: empty when there is none; an unreadable
    /// or corrupt file is an empty history and the warning why (the file
    /// is replaced at the next change). Entries whose image file is gone
    /// are left out.
    ///
    /// Temporary files a save cut short are deleted; so are image files no
    /// entry names when the file was read (or there is none). When it
    /// cannot be read, its images stay until the next save replaces it.
    pub fn load(dir: PathBuf, caps: Caps) -> (History, Option<String>) {
        let mut h = History::new(dir, caps);
        h.sweep(false);
        let path = h.dir.join(FILE);
        let text = match fs::read(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                h.sweep(true);
                return (h, None);
            }
            Err(e) => {
                return (
                    h,
                    Some(format!("the clipboard history {}: {e}", path.display())),
                );
            }
        };
        let file: FileV1 = match serde_json::from_slice(&text) {
            Ok(f) => f,
            Err(e) => {
                return (
                    h,
                    Some(format!(
                        "the clipboard history {} is unreadable ({e}): starting empty",
                        path.display()
                    )),
                );
            }
        };
        if file.version != 1 {
            return (
                h,
                Some(format!(
                    "the clipboard history {} is version {}: starting empty",
                    path.display(),
                    file.version
                )),
            );
        }
        let mut max_id = 0;
        for e in file.entries {
            max_id = max_id.max(e.id);
            let what = match (e.kind.as_str(), e.text, e.file) {
                ("text", Some(t), _) => What::Text(t),
                ("image", _, Some(f))
                    if plain_name(&f) && h.dir.join(IMAGES).join(&f).is_file() =>
                {
                    What::Image {
                        file: f,
                        mime: e.mime,
                    }
                }
                _ => continue,
            };
            if h.entries.iter().any(|s| s.id == e.id) {
                continue;
            }
            h.entries.push(Stored {
                id: e.id,
                what,
                size: e.size,
                hash: e.hash,
                time: e.time_unix,
            });
        }
        h.next_id = file.next_id.max(max_id + 1).max(1);
        // Caps smaller than when it was written still hold.
        let _ = h.evict();
        h.sweep(true);
        (h, None)
    }

    /// Deletes the temporary files a save cut short (in the directory and
    /// in `images/`) and, with `orphans`, every image file no entry
    /// names. A file that cannot be deleted is a warning in the log.
    fn sweep(&self, orphans: bool) {
        let named: std::collections::HashSet<&str> = self
            .entries
            .iter()
            .filter_map(|s| match &s.what {
                What::Image { file, .. } => Some(file.as_str()),
                What::Text(_) => None,
            })
            .collect();
        let delete = |path: &Path| {
            if let Err(e) = fs::remove_file(path)
                && e.kind() != io::ErrorKind::NotFound
            {
                log::warn!("clipboard: cannot delete {}: {e}", path.display());
            }
        };
        for (dir, images) in [(self.dir.clone(), false), (self.dir.join(IMAGES), true)] {
            let Ok(files) = fs::read_dir(&dir) else {
                continue;
            };
            for f in files.flatten() {
                let name = f.file_name();
                let stale = is_temporary(&name)
                    || (images && orphans && !name.to_str().is_some_and(|n| named.contains(n)));
                if stale && f.file_type().is_ok_and(|t| t.is_file()) {
                    delete(&f.path());
                }
            }
        }
    }

    /// Its directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The entries, newest first.
    pub fn stored(&self) -> &[Stored] {
        &self.entries
    }

    /// Entry `id`.
    pub fn get(&self, id: i64) -> Option<&Stored> {
        self.entries.iter().find(|s| s.id == id)
    }

    /// An image entry's file.
    pub fn image_path(&self, file: &str) -> PathBuf {
        self.dir.join(IMAGES).join(file)
    }

    /// The entries as the store shows them.
    pub fn entries(&self) -> Vec<ClipboardEntry> {
        self.entries
            .iter()
            .map(|s| {
                let (text, image) = match &s.what {
                    What::Text(t) => (t.clone(), None),
                    What::Image { file, .. } => (
                        String::new(),
                        Some(self.image_path(file).to_string_lossy().into_owned()),
                    ),
                };
                let time = chrono::DateTime::from_timestamp(s.time, 0)
                    .map(|t| Date::at(t.with_timezone(&chrono::Local).naive_local()))
                    .unwrap_or_default();
                ClipboardEntry {
                    id: s.id,
                    text,
                    image,
                    size: i64::try_from(s.size).unwrap_or(i64::MAX),
                    time,
                }
            })
            .collect()
    }

    /// The bytes of entry `s` (an image read back from its file).
    fn bytes_of(&self, s: &Stored) -> Option<Vec<u8>> {
        match &s.what {
            What::Text(t) => Some(t.as_bytes().to_vec()),
            What::Image { file, .. } => fs::read(self.image_path(file)).ok(),
        }
    }

    /// The entry holding `bytes` (a text when `text`), if one does.
    fn find(&self, text: bool, bytes: &[u8], hash: u64) -> Option<usize> {
        self.entries.iter().position(|s| {
            matches!(s.what, What::Text(_)) == text
                && s.size == bytes.len() as u64
                && s.hash == hash
                && self.bytes_of(s).is_some_and(|b| b == bytes)
        })
    }

    /// Moves entry `i` to the front, copied now.
    fn move_to_front(&mut self, i: usize) -> i64 {
        let mut s = self.entries.remove(i);
        s.time = now_unix();
        let id = s.id;
        self.entries.insert(0, s);
        id
    }

    /// Moves entry `id` to the front, copied now (`e.copy()`).
    pub fn touch(&mut self, id: i64) -> bool {
        match self.entries.iter().position(|s| s.id == id) {
            Some(i) => {
                self.move_to_front(i);
                true
            }
            None => false,
        }
    }

    /// `text` copied: kept at the front (a repeat moves its entry there).
    /// `None`: not kept (empty, or over the cap for a text).
    pub fn push_text(&mut self, text: &str) -> Option<i64> {
        if text.is_empty() || text.len() > self.caps.text_bytes {
            return None;
        }
        let hash = strand_core::persist::value_hash(text.as_bytes());
        if let Some(i) = self.find(true, text.as_bytes(), hash) {
            return Some(self.move_to_front(i));
        }
        let id = self.next_id;
        self.next_id += 1;
        self.entries.insert(
            0,
            Stored {
                id,
                what: What::Text(text.to_string()),
                size: text.len() as u64,
                hash,
                time: now_unix(),
            },
        );
        self.evict();
        self.get(id).map(|s| s.id)
    }

    /// An image copied as `mime`: its file written, kept at the front (a
    /// repeat moves its entry there). `None`: not kept (empty, or over
    /// the cap for an image).
    pub fn push_image(&mut self, bytes: &[u8], mime: &str) -> io::Result<Option<i64>> {
        if bytes.is_empty() || bytes.len() > self.caps.image_bytes {
            return Ok(None);
        }
        let hash = strand_core::persist::value_hash(bytes);
        if let Some(i) = self.find(false, bytes, hash) {
            return Ok(Some(self.move_to_front(i)));
        }
        let id = self.next_id;
        self.next_id += 1;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let file = format!("{id}-{nanos:x}.{}", super::offer::extension(mime));
        let images = self.dir.join(IMAGES);
        make_dir(&self.dir)?;
        make_dir(&images)?;
        write_atomic(&images, &file, bytes)?;
        self.entries.insert(
            0,
            Stored {
                id,
                what: What::Image {
                    file,
                    mime: mime.to_string(),
                },
                size: bytes.len() as u64,
                hash,
                time: now_unix(),
            },
        );
        self.evict();
        Ok(self.get(id).map(|s| s.id))
    }

    /// Drops the oldest entries until both caps hold, deleting their
    /// image files; whether any went.
    fn evict(&mut self) -> bool {
        let mut gone = false;
        loop {
            let total: u64 = self.entries.iter().map(|s| s.size).sum();
            if self.entries.len() <= self.caps.entries && total <= self.caps.total_bytes {
                return gone;
            }
            let Some(s) = self.entries.pop() else {
                return gone;
            };
            self.delete_file(&s);
            gone = true;
        }
    }

    fn delete_file(&self, s: &Stored) {
        if let What::Image { file, .. } = &s.what
            && let Err(e) = fs::remove_file(self.image_path(file))
            && e.kind() != io::ErrorKind::NotFound
        {
            log::warn!("clipboard: cannot delete {file}: {e}");
        }
    }

    /// Removes entry `id`, its image file with it.
    pub fn remove(&mut self, id: i64) -> bool {
        let Some(i) = self.entries.iter().position(|s| s.id == id) else {
            return false;
        };
        let s = self.entries.remove(i);
        self.delete_file(&s);
        true
    }

    /// Empties the history: `history.json`, any temporary copy of it a
    /// save cut short, and every image file go.
    pub fn clear(&mut self) -> io::Result<()> {
        self.entries.clear();
        match fs::read_dir(&self.dir) {
            Ok(files) => {
                for f in files.flatten() {
                    let name = f.file_name();
                    if (name == FILE || is_temporary(&name)) && f.file_type()?.is_file() {
                        fs::remove_file(f.path())?;
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        match fs::read_dir(self.dir.join(IMAGES)) {
            Ok(files) => {
                for f in files.flatten() {
                    fs::remove_file(f.path())?;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(())
    }

    /// Writes `history.json` (atomically), then deletes the image files
    /// it does not name and any temporary file a save cut short.
    pub fn save(&self) -> io::Result<()> {
        let file = FileV1 {
            version: 1,
            next_id: self.next_id,
            entries: self
                .entries
                .iter()
                .map(|s| {
                    let (kind, text, file, mime) = match &s.what {
                        What::Text(t) => ("text", Some(t.clone()), None, String::new()),
                        What::Image { file, mime } => {
                            ("image", None, Some(file.clone()), mime.clone())
                        }
                    };
                    EntryV1 {
                        id: s.id,
                        kind: kind.into(),
                        text,
                        file,
                        mime,
                        size: s.size,
                        hash: s.hash,
                        time_unix: s.time,
                    }
                })
                .collect(),
        };
        let bytes = serde_json::to_vec(&file).map_err(io::Error::other)?;
        make_dir(&self.dir)?;
        write_atomic(&self.dir, FILE, &bytes)?;
        self.sweep(true);
        Ok(())
    }
}

/// Whether `name` is a temporary file of `write_atomic`'s
/// (`.<name>.tmp-<pid>`); no file the history keeps starts with a dot.
fn is_temporary(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_some_and(|n| n.starts_with('.') && n.contains(".tmp-"))
}

/// Creates `dir` (and its parents) private to the user, and makes it so
/// if it was there.
fn make_dir(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

/// Writes `bytes` to `dir/name` atomically: a private temporary file in
/// `dir`, synced, renamed over it, the directory synced.
fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let r = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, dir.join(name))?;
        fs::File::open(dir)?.sync_all()
    })();
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMALL: Caps = Caps {
        entries: 3,
        total_bytes: 100,
        text_bytes: 40,
        image_bytes: 60,
    };

    fn texts(h: &History) -> Vec<String> {
        h.stored()
            .iter()
            .map(|s| match &s.what {
                What::Text(t) => t.clone(),
                What::Image { file, .. } => format!("image {file}"),
            })
            .collect()
    }

    #[test]
    fn a_repeat_moves_to_the_front_and_keeps_its_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = History::new(dir.path().join("c"), Caps::DEFAULT);
        let one = h.push_text("one").unwrap();
        let two = h.push_text("two").unwrap();
        assert_ne!(one, two);
        assert_eq!(h.push_text("one"), Some(one));
        assert_eq!(texts(&h), ["one", "two"]);
        assert_eq!(h.stored()[0].id, one);
        // An image repeated is the same entry too.
        let a = h.push_image(b"\x89PNG one", "image/png").unwrap().unwrap();
        h.push_text("three");
        assert_eq!(h.push_image(b"\x89PNG one", "image/png").unwrap(), Some(a));
        assert_eq!(h.stored()[0].id, a);
        assert_eq!(h.stored().len(), 4);
        // A text equal to an image's bytes is another entry.
        let t = h.push_text("\u{89}PNG one");
        assert_ne!(t, Some(a));
    }

    #[test]
    fn caps_evict_the_oldest_by_count_and_by_bytes_images_included() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = History::new(dir.path().join("c"), SMALL);
        h.push_text("a");
        h.push_text("b");
        h.push_text("c");
        h.push_text("d");
        assert_eq!(texts(&h), ["d", "c", "b"], "three entries at most");
        // An image of 60 bytes and a text of 30: 93 bytes, under 100.
        let img = h.push_image(&[7u8; 60], "image/png").unwrap().unwrap();
        let file = match &h.get(img).unwrap().what {
            What::Image { file, .. } => h.image_path(file),
            What::Text(_) => unreachable!(),
        };
        assert!(file.is_file());
        h.push_text(&"x".repeat(30));
        assert_eq!(h.stored().len(), 3);
        // 40 more bytes: past 100, the image (oldest of the bytes kept)
        // goes, its file with it.
        h.push_text(&"y".repeat(40));
        assert!(h.get(img).is_none());
        assert!(!file.exists(), "an evicted image's file is deleted");
        let total: u64 = h.stored().iter().map(|s| s.size).sum();
        assert!(total <= 100, "{total}");
    }

    #[test]
    fn an_entry_over_its_cap_is_not_kept() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = History::new(dir.path().join("c"), SMALL);
        assert_eq!(h.push_text(&"x".repeat(41)), None);
        assert_eq!(h.push_image(&[1u8; 61], "image/png").unwrap(), None);
        assert!(h.stored().is_empty());
        assert!(!dir.path().join("c").exists(), "nothing written");
    }

    #[test]
    fn an_empty_text_is_not_kept() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = History::new(dir.path().join("c"), SMALL);
        assert_eq!(h.push_text(""), None);
        assert_eq!(h.push_image(b"", "image/png").unwrap(), None);
        assert!(h.stored().is_empty());
    }

    #[test]
    fn the_history_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let at = dir.path().join("c");
        let mut h = History::new(at.clone(), Caps::DEFAULT);
        let a = h.push_text("first").unwrap();
        let img = h.push_image(b"jpeg bytes", "image/jpeg").unwrap().unwrap();
        let b = h.push_text("second\nline").unwrap();
        h.save().unwrap();
        let (back, warning) = History::load(at.clone(), Caps::DEFAULT);
        assert_eq!(warning, None);
        assert_eq!(back.stored(), h.stored());
        assert_eq!(
            back.stored().iter().map(|s| s.id).collect::<Vec<_>>(),
            [b, img, a]
        );
        assert_eq!(back.entries(), h.entries());
        let e = &back.entries()[1];
        assert!(e.image.as_deref().unwrap().ends_with(".jpg"), "{e:?}");
        assert_eq!(e.size, 10);
        assert_eq!(e.text, "");
        // Ids go on from where they were.
        let mut back = back;
        assert!(back.push_text("third").unwrap() > b);
        // An image whose file went is left out.
        let path = std::path::PathBuf::from(e.image.clone().unwrap());
        std::fs::remove_file(&path).unwrap();
        let (again, _) = History::load(at, Caps::DEFAULT);
        assert!(again.get(img).is_none());
        assert_eq!(again.stored().len(), 2);
    }

    #[test]
    fn a_corrupt_history_file_is_an_empty_history() {
        let dir = tempfile::tempdir().unwrap();
        let at = dir.path().join("c");
        std::fs::create_dir_all(&at).unwrap();
        std::fs::write(at.join(FILE), b"{ not json").unwrap();
        let (h, warning) = History::load(at.clone(), Caps::DEFAULT);
        assert!(h.stored().is_empty());
        assert!(warning.unwrap().contains("unreadable"));
        // A file naming a path outside `images/` is not followed.
        std::fs::write(
            at.join(FILE),
            br#"{"version":1,"next_id":3,"entries":[{"id":1,"kind":"image","file":"../../etc/passwd","mime":"image/png","size":1,"hash":1,"time_unix":0},{"id":2,"kind":"text","text":"ok","size":2,"hash":2,"time_unix":0}]}"#,
        )
        .unwrap();
        let (h, warning) = History::load(at.clone(), Caps::DEFAULT);
        assert_eq!(warning, None);
        assert_eq!(texts(&h), ["ok"]);
        std::fs::write(at.join(FILE), br#"{"version":9,"next_id":1,"entries":[]}"#).unwrap();
        assert!(
            History::load(at, Caps::DEFAULT)
                .1
                .unwrap()
                .contains("version 9")
        );
    }

    #[test]
    fn files_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let at = dir.path().join("state/strand/clipboard");
        let mut h = History::new(at.clone(), Caps::DEFAULT);
        h.push_text("t");
        h.push_image(b"png", "image/png").unwrap();
        h.save().unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&at), 0o700);
        assert_eq!(mode(&at.join(IMAGES)), 0o700);
        assert_eq!(mode(&at.join(FILE)), 0o600);
        for f in std::fs::read_dir(at.join(IMAGES)).unwrap() {
            assert_eq!(mode(&f.unwrap().path()), 0o600);
        }
        // No temporary file is left behind.
        let names: Vec<String> = std::fs::read_dir(&at)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().all(|n| !n.contains(".tmp-")), "{names:?}");
    }

    #[test]
    fn clear_deletes_the_file_and_the_images() {
        let dir = tempfile::tempdir().unwrap();
        let at = dir.path().join("c");
        let mut h = History::new(at.clone(), Caps::DEFAULT);
        h.push_text("t");
        let first = h.push_image(b"png", "image/png").unwrap().unwrap();
        let first_file = h.entries()[0].image.clone().unwrap();
        h.save().unwrap();
        h.clear().unwrap();
        assert!(h.stored().is_empty());
        assert!(!at.join(FILE).exists());
        assert_eq!(std::fs::read_dir(at.join(IMAGES)).unwrap().count(), 0);
        // A new image after a clear never takes an old path.
        let next = h.push_image(b"png", "image/png").unwrap().unwrap();
        assert!(next > first);
        assert_ne!(h.entries()[0].image.clone().unwrap(), first_file);
        // A clear with nothing on disk is fine.
        let mut empty = History::new(dir.path().join("none"), Caps::DEFAULT);
        empty.clear().unwrap();
    }

    #[test]
    fn a_temporary_file_a_cut_short_save_left_goes_at_clear_and_at_load() {
        let dir = tempfile::tempdir().unwrap();
        let at = dir.path().join("c");
        let mut h = History::new(at.clone(), Caps::DEFAULT);
        h.push_text("a token");
        h.save().unwrap();
        // A save killed after its temporary file was synced, before the
        // rename: a full copy of the history under another pid's name.
        let tmp = at.join(format!(".{FILE}.tmp-4194301"));
        std::fs::write(&tmp, std::fs::read(at.join(FILE)).unwrap()).unwrap();
        let image_tmp = at.join(IMAGES).join(".1-0.png.tmp-4194301");
        std::fs::create_dir_all(at.join(IMAGES)).unwrap();
        std::fs::write(&image_tmp, b"png").unwrap();
        h.clear().unwrap();
        assert!(!tmp.exists(), "clear leaves no copy of the history");
        assert!(!image_tmp.exists());
        assert_eq!(std::fs::read_dir(&at).unwrap().count(), 1, "images/ only");
        // A load deletes one too, the history it holds not read back.
        std::fs::write(&tmp, br#"{"version":1,"next_id":2,"entries":[]}"#).unwrap();
        let (h, warning) = History::load(at.clone(), Caps::DEFAULT);
        assert_eq!(warning, None);
        assert!(h.stored().is_empty());
        assert!(!tmp.exists(), "a load deletes a cut-short save's file");
    }

    #[test]
    fn image_files_no_entry_names_are_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let at = dir.path().join("c");
        let mut h = History::new(at.clone(), Caps::DEFAULT);
        let kept = h.push_image(b"kept png", "image/png").unwrap().unwrap();
        h.save().unwrap();
        let kept_file = PathBuf::from(h.entries()[0].image.clone().unwrap());
        // A crash between an image's write and the save: its file is
        // named by no entry.
        h.push_image(b"orphan png", "image/png").unwrap().unwrap();
        let orphan = PathBuf::from(h.entries()[0].image.clone().unwrap());
        drop(h);
        assert!(orphan.is_file());
        let (h, _) = History::load(at.clone(), Caps::DEFAULT);
        assert!(!orphan.exists(), "a load deletes an image no entry names");
        assert!(kept_file.is_file());
        assert_eq!(h.stored().iter().map(|s| s.id).collect::<Vec<_>>(), [kept]);
        // A corrupt file: its images stay while it is not replaced (it may
        // be read again), and go at the save that replaces it.
        std::fs::write(at.join(FILE), b"{ not json").unwrap();
        let (mut h, warning) = History::load(at.clone(), Caps::DEFAULT);
        assert!(warning.is_some());
        assert!(kept_file.is_file());
        h.push_image(b"new png", "image/png").unwrap().unwrap();
        let new_file = PathBuf::from(h.entries()[0].image.clone().unwrap());
        h.save().unwrap();
        assert!(
            !kept_file.exists(),
            "the save deletes the old file's images"
        );
        assert!(new_file.is_file());
        assert_eq!(std::fs::read_dir(at.join(IMAGES)).unwrap().count(), 1);
        // With no history file at all, every image is named by none.
        std::fs::remove_file(at.join(FILE)).unwrap();
        let (h, warning) = History::load(at.clone(), Caps::DEFAULT);
        assert_eq!(warning, None);
        assert!(h.stored().is_empty());
        assert!(!new_file.exists());
    }

    #[test]
    fn the_default_directory_follows_the_state_directory() {
        assert_eq!(
            default_dir(None, Some("/s".into()), Some("/h".into())),
            Some(PathBuf::from("/s/strand/clipboard"))
        );
        assert_eq!(
            default_dir(None, Some("rel".into()), Some("/h".into())),
            Some(PathBuf::from("/h/.local/state/strand/clipboard"))
        );
        // `STRAND_STATE_DIR` names strand's own directory, ahead of both.
        assert_eq!(
            default_dir(Some("/t".into()), Some("/s".into()), Some("/h".into())),
            Some(PathBuf::from("/t/clipboard"))
        );
        assert_eq!(default_dir(None, None, None), None);
    }
}
