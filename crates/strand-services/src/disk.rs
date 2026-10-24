//! `disk`: how full the mounted filesystems are (gap 33, decisions.md
//! m6-service-gaps), shaped like `memory`: the root filesystem's
//! `usage`, `used`, `total` and `free`, and `all`, every mounted
//! filesystem on a local disk keyed by `path`, `/` first.
//!
//! The mounts come from `/proc/self/mountinfo` ([`parse_mountinfo`]),
//! read again at every sample so mounts come and go without a watch, and
//! [`select`]ed: `/` always, then each mount of a block device (`/dev/…`)
//! or a ZFS dataset, without read-only images (`squashfs` snaps,
//! `iso9660`, `udf`), network or FUSE filesystems (whose `statvfs` can
//! hang on a server that went away), mount points that are not
//! directories (a container's bound `/etc/hosts`), mounts at or under an
//! automount point (`autofs`: every look at one counts as using it, so
//! sampling would keep a mount meant to expire when idle mounted), and a
//! second mount of the same device and root (a bind). Each is measured
//! with `statvfs` and counted as `df` does ([`usage`]).
//!
//! It samples every [`PERIOD`] (10 s), only while a reader is visible,
//! at start and at once when one becomes visible again; while none is
//! and a `graph` keeps history of it, at the kept, coarser period
//! ([`Ticker`]; history). A sample runs on
//! tokio's blocking pool, one at a time: a `statvfs` that hangs holds no
//! thread of the shared runtime, keeps the last values shown and queues
//! no second sample behind it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use crate::{Cx, Msg, ServiceError, Store, Ticker, service};

/// The schema the `disk` service serves.
pub const SCHEMA: &str = strand_services_schema::DISK;

/// How often it samples while visible.
pub const PERIOD: Duration = Duration::from_secs(10);

static SAMPLES: AtomicU64 = AtomicU64::new(0);

/// How many samples every `disk` service of this process took (tests: a
/// hidden reader stops the sampling).
pub fn samples() -> u64 {
    SAMPLES.load(Ordering::Relaxed)
}

/// Where a `disk` service reads its mounts and how often (tests point
/// it at a fixture and a short period; [`configure`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// The mount table, `/proc/self/mountinfo`'s format.
    pub mountinfo: PathBuf,
    /// How often it samples while visible.
    pub period: Duration,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            mountinfo: PathBuf::from("/proc/self/mountinfo"),
            period: PERIOD,
        }
    }
}

static CONFIG: Mutex<Option<Config>> = Mutex::new(None);

/// Use `config` for `disk` services started from now on (`None`: the
/// defaults again).
pub fn configure(config: Option<Config>) {
    *CONFIG.lock().unwrap_or_else(PoisonError::into_inner) = config;
}

fn config() -> Config {
    CONFIG
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .unwrap_or_default()
}

/// A mounted filesystem.
#[derive(crate::Data, Clone, Debug, Default, PartialEq)]
#[data(name = "Filesystem", key = path)]
pub struct Filesystem {
    pub path: String,
    pub device: String,
    pub usage: f64,
    pub used: f64,
    pub total: f64,
    pub free: f64,
}

/// See the module docs.
#[service(name = "disk")]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Disk {
    /// The root filesystem's fraction in use, 0 to 1: `pct(disk.usage)`.
    pub usage: f64,
    /// The root filesystem's bytes in use.
    pub used: f64,
    /// The root filesystem's size in bytes.
    pub total: f64,
    /// The root filesystem's bytes free to a user.
    pub free: f64,
    /// Every mounted filesystem on a local disk, `/` first, keyed by
    /// `path`.
    #[store(keyed)]
    pub all: Vec<Filesystem>,
}

/// One line of `/proc/self/mountinfo`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Mount {
    /// The mount's id.
    pub id: u64,
    /// The device, `major:minor`.
    pub dev: String,
    /// The directory of the filesystem mounted here (a bind's or a btrfs
    /// subvolume's, `/` for the whole of it).
    pub root: String,
    /// Where it is mounted.
    pub point: String,
    /// Its filesystem type.
    pub fstype: String,
    /// What was mounted: a device (`/dev/nvme0n1p2`), a dataset, `tmpfs`.
    pub source: String,
}

/// `\NNN` octal escapes (mountinfo writes a space as `\040`, a tab as
/// `\011`, a newline as `\012` and a backslash as `\134`) undone.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let n = (b[i + 1] - b'0') as u32 * 64
                + (b[i + 2] - b'0') as u32 * 8
                + (b[i + 3] - b'0') as u32;
            if let Ok(c) = u8::try_from(n) {
                out.push(c);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The mounts of a `/proc/self/mountinfo` text, in its order; a line it
/// cannot read is skipped.
pub fn parse_mountinfo(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            let (head, tail) = line.split_once(" - ")?;
            let mut h = head.split(' ');
            let id = h.next()?.parse().ok()?;
            let _parent = h.next()?;
            let dev = h.next()?.to_string();
            let root = unescape(h.next()?);
            let point = unescape(h.next()?);
            let mut t = tail.split(' ');
            let fstype = unescape(t.next()?);
            let source = unescape(t.next()?);
            Some(Mount {
                id,
                dev,
                root,
                point,
                fstype,
                source,
            })
        })
        .collect()
}

/// Read-only images: a snap, a disc.
const IMAGES: [&str; 3] = ["squashfs", "iso9660", "udf"];

/// Network filesystems, whose `statvfs` waits on a server.
const NETWORK: [&str; 13] = [
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "smbfs",
    "9p",
    "afs",
    "ceph",
    "glusterfs",
    "davfs",
    "lustre",
    "ncpfs",
    "virtiofs",
];

fn network_or_fuse(fstype: &str) -> bool {
    NETWORK.contains(&fstype) || fstype == "fuse" || fstype.starts_with("fuse.")
}

/// The mounts `disk.all` lists, `/` first: see the module docs.
/// `is_dir` says whether a mount point is a directory.
pub fn select(mounts: Vec<Mount>, is_dir: impl Fn(&str) -> bool) -> Vec<Mount> {
    // Automount points (a systemd `x-systemd.automount`, the automounter's
    // maps), hidden or not by what they mounted: the kernel takes every
    // path walk through one as use, which resets its idle timeout.
    let automounts: Vec<String> = mounts
        .iter()
        .filter(|m| m.fstype == "autofs")
        .map(|m| m.point.clone())
        .collect();
    let automounted = |point: &str| {
        automounts.iter().any(|a| {
            point == a
                || point
                    .strip_prefix(a.as_str())
                    .is_some_and(|rest| rest.starts_with('/') || a.ends_with('/'))
        })
    };
    // A later mount at the same point hides the earlier one.
    let mut top: Vec<Mount> = Vec::new();
    for m in mounts {
        top.retain(|t| t.point != m.point);
        top.push(m);
    }
    let root = top.iter().position(|m| m.point == "/");
    let mut ordered: Vec<Mount> = Vec::new();
    if let Some(i) = root {
        ordered.push(top.remove(i));
    }
    let listed = |m: &Mount| {
        (m.source.starts_with("/dev/") || m.fstype == "zfs")
            && !IMAGES.contains(&m.fstype.as_str())
            && !network_or_fuse(&m.fstype)
            && !automounted(&m.point)
            && is_dir(&m.point)
    };
    ordered.extend(top.into_iter().filter(listed));
    let mut seen: Vec<(String, String)> = Vec::new();
    ordered.retain(|m| {
        let k = (m.dev.clone(), m.root.clone());
        if seen.contains(&k) {
            return false;
        }
        seen.push(k);
        true
    });
    ordered
}

/// A filesystem's blocks, as `statvfs` gives them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Blocks {
    /// The fragment size, the unit of the counts.
    pub frsize: u64,
    /// The size, in fragments.
    pub blocks: u64,
    /// Free fragments, the space reserved for root included.
    pub bfree: u64,
    /// Free fragments a user may take.
    pub bavail: u64,
}

/// `(usage, used, total, free)` of `b` as `df` counts them: used is the
/// size less every free block, free what a user may take, and usage
/// used over used plus free (so the space reserved for root counts as
/// neither), 0 when both are 0.
pub fn usage(b: Blocks) -> (f64, f64, f64, f64) {
    let f = b.frsize as f64;
    let total = b.blocks as f64 * f;
    let used = b.blocks.saturating_sub(b.bfree) as f64 * f;
    let free = b.bavail as f64 * f;
    let usage = if used + free > 0.0 {
        used / (used + free)
    } else {
        0.0
    };
    (usage, used, total, free)
}

fn statvfs(path: &str) -> Option<Blocks> {
    let s = rustix::fs::statvfs(path).ok()?;
    Some(Blocks {
        frsize: if s.f_frsize > 0 {
            s.f_frsize
        } else {
            s.f_bsize
        },
        blocks: s.f_blocks,
        bfree: s.f_bfree,
        bavail: s.f_bavail,
    })
}

/// One sample, blocking: the mount table read and every listed mount
/// measured (`None`: no mount table).
fn sample(mountinfo: &Path) -> Option<Vec<Filesystem>> {
    SAMPLES.fetch_add(1, Ordering::Relaxed);
    let text = std::fs::read_to_string(mountinfo).ok()?;
    let mounts = select(parse_mountinfo(&text), |p| Path::new(p).is_dir());
    Some(
        mounts
            .into_iter()
            .filter_map(|m| {
                let b = statvfs(&m.point)?;
                let (usage, used, total, free) = usage(b);
                Some(Filesystem {
                    path: m.point,
                    device: m.source,
                    usage,
                    used,
                    total,
                    free,
                })
            })
            .collect(),
    )
}

/// The state a sample shows: the root's fields from `/`'s entry.
fn apply(d: &mut Disk, all: Vec<Filesystem>) {
    match all.iter().find(|f| f.path == "/") {
        Some(r) => {
            d.usage = r.usage;
            d.used = r.used;
            d.total = r.total;
            d.free = r.free;
        }
        None => {
            d.usage = 0.0;
            d.used = 0.0;
            d.total = 0.0;
            d.free = 0.0;
        }
    }
    d.all = all;
}

type Sampling = tokio::task::JoinHandle<Option<Vec<Filesystem>>>;

/// Start a sample on the blocking pool, unless one is still running.
fn start(running: &mut Option<Sampling>, mountinfo: &Path) {
    if running.is_some() {
        return;
    }
    let path = mountinfo.to_path_buf();
    *running = Some(tokio::task::spawn_blocking(move || sample(&path)));
}

impl Disk {
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let config = config();
        let mut running: Option<Sampling> = None;
        start(&mut running, &config.mountinfo);
        let mut tick = Ticker::new(&cx, config.period);
        loop {
            tokio::select! {
                r = async {
                    match running.as_mut() {
                        Some(h) => h.await,
                        None => std::future::pending().await,
                    }
                } => {
                    running = None;
                    match r {
                        Ok(Some(all)) => {
                            if !cx.update(|d| apply(d, all)) {
                                return Ok(());
                            }
                        }
                        Ok(None) => log::debug!("disk: no mount table at {}", config.mountinfo.display()),
                        Err(e) => log::warn!("disk: a sample failed: {e}"),
                    }
                    if !cx.is_ready() {
                        cx.ready();
                    }
                }
                m = cx.recv() => {
                    let shown = match m {
                        None => return Ok(()),
                        Some(m) => matches!(m, Msg::Visible(true)),
                    };
                    tick.follow(&cx);
                    if shown {
                        // Back in view: the current values at once.
                        start(&mut running, &config.mountinfo);
                        tick.reset();
                    }
                }
                _ = tick.tick() => {
                    start(&mut running, &config.mountinfo);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A desktop's mount table: an overlay root as in a container, the
    /// container's bound `/etc/hosts`, btrfs `/` and `/home` subvolumes
    /// of one device, a snap, an NFS share, tmpfs, proc, a mount point
    /// with a space, a bind of `/home`'s subvolume again, an ext4 disk
    /// and a FUSE mount.
    const MOUNTINFO: &str = "\
22 1 0:21 / /proc rw,nosuid shared:5 - proc proc rw
24 1 0:23 / /run rw,nosuid shared:7 - tmpfs tmpfs rw,mode=755
26 1 0:25 /@ / rw,relatime shared:1 - btrfs /dev/nvme0n1p2 rw,subvol=/@
27 26 0:25 /@home /home rw,relatime shared:2 - btrfs /dev/nvme0n1p2 rw,subvol=/@home
28 26 7:1 / /snap/core/1 ro,nodev shared:10 - squashfs /dev/loop1 ro
29 26 0:50 / /mnt/nas rw,relatime shared:11 - nfs4 nas:/export rw
30 26 8:17 / /mnt/My\\040Disk rw,relatime shared:12 - ext4 /dev/sdb1 rw
31 26 0:25 /@home /srv/home-again rw,relatime shared:13 - btrfs /dev/nvme0n1p2 rw
32 26 8:33 / /media/usb rw,relatime shared:14 - vfat /dev/sdc1 rw
33 26 0:60 / /mnt/cloud rw shared:15 - fuse.rclone remote: rw
34 26 8:2 /var/lib/docker/hosts /etc/hosts rw - ext4 /dev/sda2 rw
35 26 0:61 / /tank rw - zfs tank rw
";

    #[test]
    fn mountinfo_is_read_with_its_escapes() {
        let m = parse_mountinfo(MOUNTINFO);
        assert_eq!(m.len(), 12);
        let disk = m.iter().find(|m| m.id == 30).unwrap();
        assert_eq!(disk.point, "/mnt/My Disk");
        assert_eq!(disk.dev, "8:17");
        assert_eq!(disk.fstype, "ext4");
        assert_eq!(disk.source, "/dev/sdb1");
        let home = m.iter().find(|m| m.id == 27).unwrap();
        assert_eq!(
            (home.root.as_str(), home.point.as_str()),
            ("/@home", "/home")
        );
        assert_eq!(unescape(r"a\011b\012c\134d\040"), "a\tb\nc\\d ");
        assert_eq!(unescape(r"\9\04"), r"\9\04", "not an escape");
        assert!(parse_mountinfo("garbage\n\n1 2 3\n").is_empty());
    }

    #[test]
    fn local_disks_are_listed_root_first() {
        let not_dirs = ["/etc/hosts"];
        let points = |text: &str| -> Vec<String> {
            select(parse_mountinfo(text), |p| !not_dirs.contains(&p))
                .into_iter()
                .map(|m| m.point)
                .collect()
        };
        assert_eq!(
            points(MOUNTINFO),
            ["/", "/home", "/mnt/My Disk", "/media/usb", "/tank"]
        );
        // A container: an overlay `/` (no device), the host's disk bound
        // at a file. `/` is kept whatever it is.
        let container = "\
500 400 0:90 / / rw - overlay overlay rw,lowerdir=/x
501 500 0:91 / /proc rw - proc proc rw
502 500 259:2 /var/lib/docker/containers/abc/hosts /etc/hosts rw - ext4 /dev/nvme0n1p2 rw
503 500 259:2 /home/me/src /src rw - ext4 /dev/nvme0n1p2 rw
504 500 259:2 /home/me/src /src2 rw - ext4 /dev/nvme0n1p2 rw
";
        assert_eq!(points(container), ["/", "/src"]);
        // A later mount at `/` hides the first (rootfs under the real one).
        let stacked = "\
1 0 0:1 / / rw - rootfs rootfs rw
20 1 8:2 / / rw - ext4 /dev/sda2 rw
";
        let s = select(parse_mountinfo(stacked), |_| true);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].source, "/dev/sda2");
        assert!(select(Vec::new(), |_| true).is_empty());
    }

    /// (gap 33, review round 1) A mount an automount made (an ESP behind
    /// `x-systemd.automount`, an automounter map's `/misc/usb`) is not
    /// listed and its point is never looked at: every path walk through
    /// an autofs point counts as use, so sampling it every 10 s would keep
    /// a mount meant to expire when idle mounted. A mount beside one
    /// (`/bootstrap` beside `/boot`) is listed.
    #[test]
    fn automounted_filesystems_are_left_untouched() {
        let text = "20 1 259:2 / / rw - ext4 /dev/nvme0n1p2 rw
40 20 0:45 / /boot rw - autofs systemd-1 rw,fd=50,pgrp=1,direct
41 40 259:1 / /boot rw - vfat /dev/nvme0n1p1 rw
42 41 259:3 / /boot/efi rw - vfat /dev/nvme0n1p3 rw
43 20 0:46 / /misc rw - autofs /etc/auto.misc rw,fd=7,pgrp=900,indirect
44 43 8:33 / /misc/usb rw - ext4 /dev/sdc1 rw
45 20 8:17 / /bootstrap rw - ext4 /dev/sdb1 rw
46 20 0:47 / /mnt/later rw - autofs systemd-1 rw,fd=51,pgrp=1,direct
";
        let looked = std::cell::RefCell::new(Vec::new());
        let points: Vec<String> = select(parse_mountinfo(text), |p| {
            looked.borrow_mut().push(p.to_string());
            true
        })
        .into_iter()
        .map(|m| m.point)
        .collect();
        assert_eq!(points, ["/", "/bootstrap"]);
        assert_eq!(*looked.borrow(), ["/bootstrap"], "only the listed one");
    }

    #[test]
    fn usage_is_counted_as_df_does() {
        // 1000 blocks of 4 KiB, 300 free of which 50 are reserved for
        // root: 700 used, 250 free to a user, 700/950 in use (not 0.7).
        let b = Blocks {
            frsize: 4096,
            blocks: 1000,
            bfree: 300,
            bavail: 250,
        };
        let (u, used, total, free) = usage(b);
        assert_eq!(total, 4096.0 * 1000.0);
        assert_eq!(used, 4096.0 * 700.0);
        assert_eq!(free, 4096.0 * 250.0);
        assert!((u - 700.0 / 950.0).abs() < 1e-12, "{u}");
        assert!((u - used / total).abs() > 0.01);
        assert_eq!(usage(Blocks::default()), (0.0, 0.0, 0.0, 0.0));
    }
}
