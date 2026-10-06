//! Small state files written off the logic thread: the last palette and
//! the portal's last values. A slow or hung home directory (NFS) must
//! not stall a frame, so [`FileWriter::write`] only queues; a worker
//! thread writes each file atomically (temp file plus rename), the
//! latest content per path winning when writes pile up.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How long dropping a [`FileWriter`] waits for its queued writes.
pub const DROP_WAIT: Duration = Duration::from_secs(1);

#[derive(Default)]
struct Pending {
    count: Mutex<usize>,
    idle: Condvar,
}

/// See the module docs.
pub struct FileWriter {
    tx: Option<Sender<(PathBuf, Vec<u8>)>>,
    pending: Arc<Pending>,
}

impl std::fmt::Debug for FileWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileWriter").finish_non_exhaustive()
    }
}

/// `bytes` into `path` through a temp file and a rename. The temp name
/// carries the process id, so two runs sharing a directory never write
/// the same temp file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

fn run(rx: Receiver<(PathBuf, Vec<u8>)>, pending: Arc<Pending>) {
    while let Ok(first) = rx.recv() {
        let mut batch: HashMap<PathBuf, Vec<u8>> = HashMap::new();
        let mut n = 1;
        batch.insert(first.0, first.1);
        while let Ok((p, b)) = rx.try_recv() {
            batch.insert(p, b);
            n += 1;
        }
        for (path, bytes) in batch {
            if let Err(e) = write_atomic(&path, &bytes) {
                log::warn!("saving {}: {e}", path.display());
            }
        }
        let mut c = pending.count.lock().unwrap_or_else(PoisonError::into_inner);
        *c = c.saturating_sub(n);
        if *c == 0 {
            pending.idle.notify_all();
        }
    }
}

impl FileWriter {
    /// Starts the worker thread (idle until the first write).
    pub fn new() -> io::Result<FileWriter> {
        let (tx, rx) = mpsc::channel();
        let pending = Arc::new(Pending::default());
        let p = pending.clone();
        std::thread::Builder::new()
            .name("strand-state-write".into())
            .spawn(move || run(rx, p))?;
        Ok(FileWriter {
            tx: Some(tx),
            pending,
        })
    }

    /// Queues `bytes` for `path`; returns at once.
    pub fn write(&self, path: impl Into<PathBuf>, bytes: impl Into<Vec<u8>>) {
        let Some(tx) = &self.tx else { return };
        *self
            .pending
            .count
            .lock()
            .unwrap_or_else(PoisonError::into_inner) += 1;
        if tx.send((path.into(), bytes.into())).is_err() {
            let mut c = self
                .pending
                .count
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            *c = c.saturating_sub(1);
        }
    }

    /// Waits up to `timeout` for every queued write to land. Returns
    /// whether none is left.
    pub fn flush(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut c = self
            .pending
            .count
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while *c > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            c = self
                .pending
                .idle
                .wait_timeout(c, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }
}

impl Drop for FileWriter {
    fn drop(&mut self) {
        // Closing the channel ends the worker once the queue is written;
        // a hung file system costs shutdown at most DROP_WAIT.
        self.tx = None;
        self.flush(DROP_WAIT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_land_off_thread_latest_wins() {
        let dir = std::env::temp_dir().join(format!("strand-writer-{}", std::process::id()));
        let w = FileWriter::new().unwrap();
        for i in 0..50 {
            w.write(dir.join("a"), format!("{i}"));
        }
        w.write(dir.join("b"), "b");
        assert!(w.flush(Duration::from_secs(5)));
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "49");
        assert_eq!(std::fs::read_to_string(dir.join("b")).unwrap(), "b");
        w.write(dir.join("a"), "last");
        drop(w);
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "last");
        let _ = std::fs::remove_dir_all(dir);
    }
}
