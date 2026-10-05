//! Path helpers: editor scratch names, symlink chains, filesystem kinds.

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

/// Editor scratch names that are never a save: Vim's `4913` write test and
/// swap files (`*.swp`, plus the `*.swo`/`*.swx` it moves on to), backups
/// (`*~`) and JetBrains safe-write files (`*___jb_*___`).
pub fn is_scratch(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    name == "4913"
        || name.ends_with(".swp")
        || name.ends_with(".swo")
        || name.ends_with(".swx")
        || name.ends_with('~')
        || (name.ends_with("___") && name.contains("___jb_"))
}

/// Names starting with `.` are skipped in directory scans, as
/// `source::find_files` skips them.
pub fn is_hidden(name: &OsStr) -> bool {
    name.as_encoded_bytes().first() == Some(&b'.')
}

/// Make `path` absolute against the current directory, without touching
/// symlinks.
pub fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// `path` with every symlink followed, plus each symlink met on the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The canonical path when `exists`; otherwise the canonical prefix that
    /// exists joined with the rest.
    pub path: PathBuf,
    /// Every symlink followed, as `<canonical dir>/<link name>`: watching
    /// each one's directory sees the link being swapped.
    pub hops: Vec<PathBuf>,
    /// Whether the whole chain resolved to something.
    pub exists: bool,
}

impl Resolved {
    /// Directories to watch for this path: the target's directory and each
    /// link's directory.
    pub fn watch_dirs(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.path.as_path())
            .chain(self.hops.iter().map(PathBuf::as_path))
            .filter_map(Path::parent)
    }
}

const MAX_HOPS: usize = 40;

enum Part {
    Parent,
    Name(OsString),
}

fn parts(path: &Path) -> Vec<Part> {
    path.components()
        .filter_map(|c| match c {
            Component::ParentDir => Some(Part::Parent),
            Component::Normal(n) => Some(Part::Name(n.to_os_string())),
            _ => None,
        })
        .collect()
}

/// Resolve `path` one component at a time, recording each symlink.
pub fn resolve(path: &Path) -> Resolved {
    let path = absolute(path);
    let mut pending: std::collections::VecDeque<Part> = parts(&path).into();
    let mut cur = PathBuf::from("/");
    let mut hops = Vec::new();
    let mut missing = false;
    while let Some(part) = pending.pop_front() {
        let name = match part {
            Part::Parent => {
                cur.pop();
                continue;
            }
            Part::Name(n) => n,
        };
        let candidate = cur.join(&name);
        if missing {
            cur = candidate;
            continue;
        }
        match std::fs::symlink_metadata(&candidate) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if hops.len() >= MAX_HOPS {
                    missing = true;
                    cur = candidate;
                    continue;
                }
                hops.push(candidate.clone());
                match std::fs::read_link(&candidate) {
                    Ok(target) => {
                        if target.is_absolute() {
                            cur = PathBuf::from("/");
                        }
                        for p in parts(&target).into_iter().rev() {
                            pending.push_front(p);
                        }
                    }
                    Err(_) => {
                        missing = true;
                        cur = candidate;
                    }
                }
            }
            Ok(_) => cur = candidate,
            Err(_) => {
                missing = true;
                cur = candidate;
            }
        }
    }
    Resolved {
        path: cur,
        hops,
        exists: !missing,
    }
}

/// Where a directory lives, as far as watching goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsKind {
    /// inotify sees every write.
    Local,
    /// Writes from other machines produce no events: poll.
    NoEvents,
    /// Mounted read-only (`/nix/store`): nothing changes in place, so no
    /// watch is needed; a link swap is seen in the link's directory.
    ReadOnly,
}

/// statfs `f_type` values of filesystems whose remote writes produce no
/// inotify events.
const NO_EVENT_MAGICS: &[u64] = &[
    0x6969,      // NFS
    0x517B,      // SMB
    0xFF53_4D42, // CIFS
    0xFE53_4D42, // SMB2
    0x0102_1997, // 9p (v9fs)
    0x00C3_6400, // Ceph
    0x5346_414F, // AFS
    0x6B41_4653, // kAFS
    0x7375_7245, // Coda
    0x6573_5546, // FUSE (sshfs, rclone, …)
];

/// Classify the filesystem holding `dir`.
pub fn fs_kind(dir: &Path) -> FsKind {
    let read_only = rustix::fs::statvfs(dir)
        .is_ok_and(|v| v.f_flag.contains(rustix::fs::StatVfsMountFlags::RDONLY));
    let magic = rustix::fs::statfs(dir).map_or(0, |fs| fs.f_type as u64);
    classify(magic, read_only)
}

fn classify(magic: u64, read_only: bool) -> FsKind {
    if read_only {
        FsKind::ReadOnly
    } else if NO_EVENT_MAGICS.contains(&(magic & 0xFFFF_FFFF)) {
        FsKind::NoEvents
    } else {
        FsKind::Local
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn scratch_names() {
        for n in [
            "4913",
            "theme.strand.swp",
            ".theme.strand.swp",
            ".bar.strand.swx",
            "bar.strand~",
            "bar.strand___jb_tmp___",
            "bar.strand___jb_old___",
        ] {
            assert!(is_scratch(OsStr::new(n)), "{n}");
        }
        for n in ["bar.strand", "49130", "theme.wgsl", "prefs.toml", "jb___"] {
            assert!(!is_scratch(OsStr::new(n)), "{n}");
        }
    }

    #[test]
    fn resolve_records_every_hop() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir_all(root.join("dotfiles/strand")).unwrap();
        std::fs::write(root.join("dotfiles/strand/bar.strand"), "x").unwrap();
        std::fs::create_dir(root.join("config")).unwrap();
        // A directory link (GNU stow) and a relative file link through it.
        symlink(root.join("dotfiles/strand"), root.join("config/strand")).unwrap();
        symlink("strand/bar.strand", root.join("config/bar-link.strand")).unwrap();

        let r = resolve(&root.join("config/bar-link.strand"));
        assert!(r.exists);
        assert_eq!(r.path, root.join("dotfiles/strand/bar.strand"));
        assert_eq!(
            r.hops,
            vec![
                root.join("config/bar-link.strand"),
                root.join("config/strand")
            ]
        );
        let dirs: Vec<_> = r.watch_dirs().collect();
        assert_eq!(
            dirs,
            vec![
                root.join("dotfiles/strand").as_path(),
                root.join("config").as_path(),
                root.join("config").as_path()
            ]
        );

        let missing = resolve(&root.join("config/strand/nope/x.toml"));
        assert!(!missing.exists);
        assert_eq!(missing.path, root.join("dotfiles/strand/nope/x.toml"));

        symlink("loop-b", root.join("loop-a")).unwrap();
        symlink("loop-a", root.join("loop-b")).unwrap();
        assert!(!resolve(&root.join("loop-a")).exists);
    }

    #[test]
    fn network_and_read_only_filesystems() {
        assert_eq!(classify(0x6969, false), FsKind::NoEvents); // NFS
        assert_eq!(classify(0xFF53_4D42, false), FsKind::NoEvents); // CIFS
        assert_eq!(classify(0x6573_5546, false), FsKind::NoEvents); // FUSE
        assert_eq!(classify(0xEF53, false), FsKind::Local); // ext4
        assert_eq!(classify(0x0102_1994, false), FsKind::Local); // tmpfs
        assert_eq!(classify(0x6969, true), FsKind::ReadOnly);
        assert_eq!(classify(0xEF53, true), FsKind::ReadOnly);
    }

    #[test]
    fn tmp_is_local() {
        let tmp = tempfile::tempdir().unwrap();
        assert_ne!(fs_kind(tmp.path()), FsKind::NoEvents);
    }
}
