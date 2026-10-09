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
    /// An immutable store (`/nix/store`, `/gnu/store`) on a read-only
    /// mount: nothing changes in place, so no watch is needed; a link swap
    /// is seen in the link's directory.
    Immutable,
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

/// Content-addressed stores whose paths never change once written.
const STORES: &[&str] = &["/nix/store", "/gnu/store"];

/// Whether `dir` is the root of a mount (`/`, a tmpfs `/tmp`, a separate
/// `/home`): `statx`'s `STATX_ATTR_MOUNT_ROOT` (Linux 5.8), else a device
/// that differs from its parent's, or a directory that is its own parent.
/// A path that cannot be stat'ed is not a mount root.
pub fn is_mount_root(dir: &Path) -> bool {
    use rustix::fs::{AtFlags, CWD, StatxAttributes, StatxFlags};
    use std::os::unix::fs::MetadataExt;
    if let Ok(x) = rustix::fs::statx(
        CWD,
        dir,
        AtFlags::NO_AUTOMOUNT | AtFlags::SYMLINK_NOFOLLOW,
        StatxFlags::empty(),
    ) && x.stx_attributes_mask.contains(StatxAttributes::MOUNT_ROOT)
    {
        return x.stx_attributes.contains(StatxAttributes::MOUNT_ROOT);
    }
    let Ok(me) = std::fs::symlink_metadata(dir) else {
        return false;
    };
    let Ok(up) = std::fs::metadata(dir.join("..")) else {
        return false;
    };
    me.dev() != up.dev() || me.ino() == up.ino()
}

/// The ancestors of the watched directory `dir` that hold a light
/// `WatchKind::Ancestor` watch, nearest first (design.md, "Watch
/// directories, not files"). Under `home` (`$HOME`, canonical), only
/// those strictly below it: `~/.config` yes, `~` and above no, since
/// atomic renames in `~` (a shell saving its history) would wake the
/// watcher thread, and nobody moves `$HOME` in a session. Elsewhere, up
/// to and including the root of the mount holding `dir` (whose children
/// moving is still seen); none when `dir` is that root. `mount_root`
/// answers [`is_mount_root`] (a fake in tests).
pub fn watched_ancestors(
    dir: &Path,
    home: Option<&Path>,
    mut mount_root: impl FnMut(&Path) -> bool,
) -> Vec<PathBuf> {
    if let Some(home) = home.filter(|h| dir.starts_with(h)) {
        return dir
            .ancestors()
            .skip(1)
            .take_while(|a| *a != home && a.starts_with(home))
            .map(Path::to_path_buf)
            .collect();
    }
    let mut out = Vec::new();
    if mount_root(dir) {
        return out;
    }
    for a in dir.ancestors().skip(1) {
        out.push(a.to_path_buf());
        if mount_root(a) {
            break;
        }
    }
    out
}

/// `$HOME` when it is set to an absolute path, canonical when it exists
/// (watched directories are canonical, and `/home` may be a link).
pub fn home_dir() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    if !home.is_absolute() {
        return None;
    }
    Some(std::fs::canonicalize(&home).unwrap_or(home))
}

/// Classify the filesystem holding `dir`.
pub fn fs_kind(dir: &Path) -> FsKind {
    let read_only = rustix::fs::statvfs(dir)
        .is_ok_and(|v| v.f_flag.contains(rustix::fs::StatVfsMountFlags::RDONLY));
    let magic = rustix::fs::statfs(dir).map_or(0, |fs| fs.f_type as u64);
    let store = STORES.iter().any(|s| dir.starts_with(s));
    classify(magic, read_only, store)
}

/// Network and FUSE filesystems are polled even when mounted read-only
/// (the server's copy still changes). Other read-only mounts are watched
/// (a read-only bind mount of a writable tree still gets events) unless
/// they hold an immutable store.
fn classify(magic: u64, read_only: bool, store: bool) -> FsKind {
    if NO_EVENT_MAGICS.contains(&(magic & 0xFFFF_FFFF)) {
        FsKind::NoEvents
    } else if read_only && store {
        FsKind::Immutable
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
        assert_eq!(classify(0x6969, false, false), FsKind::NoEvents); // NFS
        assert_eq!(classify(0xFF53_4D42, false, false), FsKind::NoEvents); // CIFS
        assert_eq!(classify(0x6573_5546, false, false), FsKind::NoEvents); // FUSE
        assert_eq!(classify(0xEF53, false, false), FsKind::Local); // ext4
        assert_eq!(classify(0x0102_1994, false, false), FsKind::Local); // tmpfs
        // A read-only NFS mount still changes on the server: poll it.
        assert_eq!(classify(0x6969, true, false), FsKind::NoEvents);
        assert_eq!(classify(0x6969, true, true), FsKind::NoEvents);
        // A read-only bind mount of a writable tree still gets events.
        assert_eq!(classify(0xEF53, true, false), FsKind::Local);
        // /nix/store is read-only and immutable: no watch.
        assert_eq!(classify(0xEF53, true, true), FsKind::Immutable);
        // A writable /nix/store (single-user Nix) is still watched.
        assert_eq!(classify(0xEF53, false, true), FsKind::Local);
    }

    #[test]
    fn tmp_is_local() {
        let tmp = tempfile::tempdir().unwrap();
        assert_ne!(fs_kind(tmp.path()), FsKind::NoEvents);
    }
}
