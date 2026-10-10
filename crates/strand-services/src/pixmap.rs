//! Pixel data from D-Bus (a notification's `image-data`, a tray item's
//! `IconPixmap`) written as PNG files, so `image` shows them by path like
//! any other picture.
//!
//! The pixels come from any process on the bus, so nothing is trusted:
//! sizes are checked against the bytes actually sent before anything is
//! allocated, a side above [`MAX_SIDE`] is refused, and a picture larger
//! than [`SIDE`] on a side is sampled down to it (an icon or a toast's
//! picture is shown far smaller), which bounds both the memory and the
//! PNG encoding of one picture.
//!
//! Files are content-addressed (`<hash>.png`) under
//! `$XDG_RUNTIME_DIR/strand/pixmaps/<pid>` (without one, under
//! `strand-<uid>` in the temporary directory, used only when it is a real
//! directory of this user's that nobody else may enter: the temporary
//! directory is world-writable, so a directory another user made first
//! is refused): the same pixels are one file, written once. Temporary
//! files are created new (`O_EXCL`), never through a link. A file lives
//! as long as a [`Pinned`] handle to it does (a notification kept, a tray
//! item's current icon): the last handle dropped removes it, so nothing
//! still shown is ever removed and nothing unshown stays. Directories of
//! processes that are gone are removed on the first write.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The largest side accepted from a sender (larger is refused).
pub const MAX_SIDE: u32 = 16_384;
/// The largest side written: larger pictures are sampled down to it.
pub const SIDE: u32 = 512;

/// How many handles each written file has.
static PINS: Mutex<Option<HashMap<PathBuf, usize>>> = Mutex::new(None);

/// A written PNG file, kept while a handle to it lives.
#[derive(Debug, PartialEq, Eq)]
pub struct Pinned(PathBuf);

impl Pinned {
    fn pin(path: PathBuf) -> Pinned {
        let mut pins = PINS.lock().unwrap_or_else(|e| e.into_inner());
        *pins
            .get_or_insert_with(HashMap::new)
            .entry(path.clone())
            .or_insert(0) += 1;
        Pinned(path)
    }

    /// The file.
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// The file's path as text (what `image` takes).
    pub fn text(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl Clone for Pinned {
    fn clone(&self) -> Pinned {
        Pinned::pin(self.0.clone())
    }
}

impl Drop for Pinned {
    fn drop(&mut self) {
        let mut pins = PINS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(map) = pins.as_mut() else {
            return;
        };
        let last = match map.get_mut(&self.0) {
            Some(n) if *n > 1 => {
                *n -= 1;
                false
            }
            Some(_) => {
                map.remove(&self.0);
                true
            }
            None => false,
        };
        if last {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

/// How many handles `path` has (tests).
pub fn pins(path: &Path) -> usize {
    PINS.lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|m| m.get(path).copied())
        .unwrap_or(0)
}

/// The directory every strand process writes pixmaps under, or why
/// there is none.
pub fn root() -> Result<PathBuf, String> {
    match std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        Some(run) => Ok(run.join("strand").join("pixmaps")),
        None => private_dir(&std::env::temp_dir().join(format!("strand-{}", euid())))
            .map(|d| d.join("pixmaps")),
    }
}

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// `path` as a directory only this user may enter: made 0700 when
/// missing, refused when it is a link, not a directory, another user's,
/// or open to others (someone else made it first in a shared directory).
fn private_dir(path: &Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("{}: {e}", path.display())),
    }
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.file_type().is_dir() || meta.uid() != euid() || meta.mode() & 0o077 != 0 {
        return Err(format!(
            "{}: not a private directory of this user's; set XDG_RUNTIME_DIR",
            path.display()
        ));
    }
    Ok(path.to_path_buf())
}

/// Where this process's pixmaps go, or why nowhere.
pub fn dir() -> Result<PathBuf, String> {
    root().map(|r| r.join(std::process::id().to_string()))
}

/// `path` created new for writing: never an existing file or a link.
fn create_new(path: &Path) -> Result<std::fs::File, String> {
    // A temporary file a crashed process of the same pid left is ours.
    let _ = std::fs::remove_file(path);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Remove the directories of processes that are gone (once a process).
fn sweep() {
    static SWEPT: std::sync::Once = std::sync::Once::new();
    SWEPT.call_once(|| {
        let Ok(dirs) = root().and_then(|r| std::fs::read_dir(r).map_err(|e| e.to_string())) else {
            return;
        };
        for d in dirs.filter_map(Result::ok) {
            let name = d.file_name();
            let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            if pid != std::process::id() && !Path::new(&format!("/proc/{pid}")).exists() {
                let _ = std::fs::remove_dir_all(d.path());
            }
        }
    });
}

/// RGBA pixels (row-major, `width * height * 4` bytes) as a PNG file,
/// kept while the handle lives; or why not.
pub fn write_rgba(width: u32, height: u32, rgba: &[u8]) -> Result<Pinned, String> {
    let need = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or("pixmap too large")?;
    if width == 0 || height == 0 || rgba.len() < need {
        return Err(format!(
            "bad pixmap {width}x{height} ({} bytes)",
            rgba.len()
        ));
    }
    let rgba = &rgba[..need];
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (width, height).hash(&mut h);
    rgba.hash(&mut h);
    let dir = dir()?;
    let path = dir.join(format!("{:016x}.png", h.finish()));
    // Pinned before the check: a handle dropping meanwhile cannot remove
    // a file about to be handed out.
    let pinned = Pinned::pin(path.clone());
    if path.exists() {
        return Ok(pinned);
    }
    sweep();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    publish(&dir, &path, |tmp| encode(tmp, width, height, rgba))?;
    Ok(pinned)
}

/// Write a file at `path` through `write` on a temporary file of this
/// writer's own, then rename it into place. Writers of the same pixels at
/// once each have their own temporary file; one rename wins, and a writer
/// whose rename fails while the file exists has it all the same (the name
/// is the content's).
fn publish(
    dir: &Path,
    path: &Path,
    write: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("pixmap");
    let tmp = dir.join(format!(".{stem}.{n}.tmp"));
    let done = write(&tmp).and_then(|()| {
        std::fs::rename(&tmp, path).or_else(|e| {
            if path.exists() {
                Ok(())
            } else {
                Err(format!("{}: {e}", path.display()))
            }
        })
    });
    if done.is_err() || tmp.exists() {
        let _ = std::fs::remove_file(&tmp);
    }
    done
}

/// The largest PNG a sender may hand over as bytes.
pub const MAX_PNG: usize = 1 << 20;

/// PNG bytes a sender gave (a menu entry's `icon-data`) as a file, kept
/// while the handle lives; or why not.
pub fn write_png(bytes: &[u8]) -> Result<Pinned, String> {
    if bytes.len() > MAX_PNG || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(format!("not a PNG of at most {MAX_PNG} bytes"));
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    let dir = dir()?;
    let path = dir.join(format!("{:016x}.png", h.finish()));
    let pinned = Pinned::pin(path.clone());
    if path.exists() {
        return Ok(pinned);
    }
    sweep();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    publish(&dir, &path, |tmp| {
        use std::io::Write;
        create_new(tmp)?
            .write_all(bytes)
            .map_err(|e| format!("{}: {e}", tmp.display()))
    })?;
    Ok(pinned)
}

fn encode(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    let file = create_new(path)?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().map_err(|e| e.to_string())?;
    w.write_image_data(rgba).map_err(|e| e.to_string())?;
    w.finish().map_err(|e| e.to_string())
}

/// The size written for a `w`×`h` picture: within [`SIDE`], aspect kept.
fn fitted(w: u32, h: u32) -> (u32, u32) {
    let long = w.max(h);
    if long <= SIDE {
        return (w, h);
    }
    let scale = |v: u32| ((u64::from(v) * u64::from(SIDE)) / u64::from(long)).max(1) as u32;
    (scale(w), scale(h))
}

/// The checked size of a `width`×`height` picture in `data`, its rows
/// `stride` bytes apart and its pixels `ch` bytes wide: every row it
/// claims is within `data`.
fn checked(
    width: i32,
    height: i32,
    stride: usize,
    ch: usize,
    data: &[u8],
) -> Result<(u32, u32), String> {
    let w = u32::try_from(width).map_err(|_| "negative width")?;
    let h = u32::try_from(height).map_err(|_| "negative height")?;
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
        return Err(format!("pixmap {w}x{h} refused"));
    }
    let row = (w as usize).checked_mul(ch).ok_or("pixmap too large")?;
    if stride < row {
        return Err(format!("rowstride {stride} under a row of {row} bytes"));
    }
    let need = stride
        .checked_mul(h as usize - 1)
        .and_then(|n| n.checked_add(row))
        .ok_or("pixmap too large")?;
    if data.len() < need {
        return Err(format!(
            "pixmap {w}x{h} needs {need} bytes, {} sent",
            data.len()
        ));
    }
    Ok((w, h))
}

/// Sample `w`×`h` pixels (`px(x, y)` gives one as RGBA) at most
/// [`SIDE`] on a side.
fn sample(w: u32, h: u32, px: impl Fn(usize, usize) -> [u8; 4]) -> (u32, u32, Vec<u8>) {
    let (ow, oh) = fitted(w, h);
    let mut out = Vec::with_capacity(ow as usize * oh as usize * 4);
    for oy in 0..oh as usize {
        let y = oy * h as usize / oh as usize;
        for ox in 0..ow as usize {
            let x = ox * w as usize / ow as usize;
            out.extend_from_slice(&px(x, y));
        }
    }
    (ow, oh, out)
}

/// The notification spec's `image-data` (`(iiibiiay)`: width, height,
/// rowstride, has alpha, bits per sample, channels, data) as RGBA, at most
/// [`SIDE`] on a side.
pub fn from_image_data(
    width: i32,
    height: i32,
    rowstride: i32,
    has_alpha: bool,
    bits: i32,
    channels: i32,
    data: &[u8],
) -> Result<(u32, u32, Vec<u8>), String> {
    if bits != 8 || !(3..=4).contains(&channels) || has_alpha != (channels == 4) {
        return Err(format!(
            "unsupported image-data: {bits} bits, {channels} channels"
        ));
    }
    let stride = usize::try_from(rowstride).map_err(|_| "negative rowstride")?;
    let ch = channels as usize;
    let (w, h) = checked(width, height, stride, ch, data)?;
    let at = |i: usize| data.get(i).copied().unwrap_or(0);
    Ok(sample(w, h, |x, y| {
        let i = y * stride + x * ch;
        [
            at(i),
            at(i + 1),
            at(i + 2),
            if ch == 4 { at(i + 3) } else { 255 },
        ]
    }))
}

/// StatusNotifierItem's `IconPixmap` entry (ARGB32 in network byte
/// order) as RGBA, at most [`SIDE`] on a side.
pub fn from_argb32(width: i32, height: i32, data: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let stride = usize::try_from(width)
        .map_err(|_| "negative width")?
        .checked_mul(4)
        .ok_or("pixmap too large")?;
    let (w, h) = checked(width, height, stride, 4, data)?;
    let at = |i: usize| data.get(i).copied().unwrap_or(0);
    Ok(sample(w, h, |x, y| {
        let i = y * stride + x * 4;
        [at(i + 1), at(i + 2), at(i + 3), at(i)]
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixels_convert_and_write_once() {
        // 2x1 RGB with a padded rowstride.
        let (w, h, rgba) =
            from_image_data(2, 1, 8, false, 8, 3, &[1, 2, 3, 4, 5, 6, 0, 0]).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(rgba, [1, 2, 3, 255, 4, 5, 6, 255]);
        assert!(from_image_data(2, 1, 8, true, 8, 3, &[0; 8]).is_err());
        assert!(from_image_data(2, 2, 8, false, 8, 3, &[0; 8]).is_err());
        // The last row needs no padding.
        assert!(from_image_data(2, 2, 8, false, 8, 3, &[0; 14]).is_ok());
        let (_, _, argb) = from_argb32(1, 1, &[255, 10, 20, 30]).unwrap();
        assert_eq!(argb, [10, 20, 30, 255]);
        let a = write_rgba(2, 1, &rgba).unwrap();
        let b = write_rgba(2, 1, &rgba).unwrap();
        assert_eq!(a, b, "content-addressed");
        assert_eq!(pins(a.path()), 2);
        let bytes = std::fs::read(a.path()).unwrap();
        assert_eq!(&bytes[1..4], b"PNG");
        let path = a.path().to_path_buf();
        drop(a);
        assert!(path.exists(), "kept while a handle lives");
        drop(b);
        assert!(!path.exists(), "removed with the last handle");
        assert!(write_rgba(0, 1, &[]).is_err());
        assert!(write_png(b"GIF89a").is_err());
        let png = write_png(&bytes).unwrap();
        assert!(png.path().exists());
    }

    #[test]
    fn writers_of_the_same_pixels_at_once_all_succeed() {
        let rgba: Vec<u8> = (0..64 * 64 * 4).map(|i| (i % 251) as u8).collect();
        let png = {
            let p = write_rgba(64, 64, &rgba).unwrap();
            std::fs::read(p.path()).unwrap()
        };
        for _ in 0..20 {
            let threads: Vec<_> = (0..4)
                .map(|_| {
                    let (rgba, png) = (rgba.clone(), png.clone());
                    std::thread::spawn(move || {
                        let a = write_rgba(64, 64, &rgba).unwrap();
                        assert!(a.path().exists());
                        let b = write_png(&png).unwrap();
                        assert!(b.path().exists());
                        (a, b)
                    })
                })
                .collect();
            let held: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
            let paths = [
                held[0].0.path().to_path_buf(),
                held[0].1.path().to_path_buf(),
            ];
            assert!(paths.iter().all(|p| p.exists()));
            drop(held);
            assert!(
                paths.iter().all(|p| !p.exists()),
                "removed with the last handle"
            );
            let ours: Vec<String> = paths
                .iter()
                .filter_map(|p| p.file_stem()?.to_str().map(|s| format!(".{s}.")))
                .collect();
            let tmps = std::fs::read_dir(dir().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    ours.iter().any(|o| n.starts_with(o.as_str()))
                })
                .count();
            assert_eq!(tmps, 0, "no temporary file left behind");
        }
    }

    /// (m4-audit) Without `XDG_RUNTIME_DIR` pixmaps go under the shared
    /// temporary directory, so the directory is used only when it is this
    /// user's and closed to others, and temporary files are never opened
    /// through a link someone planted.
    #[test]
    fn the_temporary_directory_fallback_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::tempdir().unwrap();
        let fresh = base.path().join("fresh");
        assert_eq!(private_dir(&fresh).unwrap(), fresh);
        let mode = std::fs::metadata(&fresh).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "made closed to others");
        assert_eq!(private_dir(&fresh).unwrap(), fresh, "kept when private");
        let open = base.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(private_dir(&open).is_err(), "a directory others may write");
        let link = base.path().join("link");
        std::os::unix::fs::symlink(&fresh, &link).unwrap();
        assert!(private_dir(&link).is_err(), "a link, even to a private one");
        let file = base.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(private_dir(&file).is_err(), "not a directory");
        // A planted link at a temporary name is replaced, never followed.
        let victim = base.path().join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        let tmp = fresh.join(".x.0.tmp");
        std::os::unix::fs::symlink(&victim, &tmp).unwrap();
        encode(&tmp, 1, 1, &[1, 2, 3, 4]).unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        assert!(
            !std::fs::symlink_metadata(&tmp)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn sizes_a_sender_lies_about_are_refused_before_allocating() {
        // A huge picture in four bytes: refused, nothing allocated.
        assert!(from_image_data(100_000, 100_000, 0, true, 8, 4, &[0; 4]).is_err());
        assert!(from_image_data(4000, 4000, 0, true, 8, 4, &[0; 4]).is_err());
        assert!(from_image_data(i32::MAX, i32::MAX, i32::MAX, true, 8, 4, &[0; 4]).is_err());
        assert!(from_image_data(4, 4, 16, true, 8, 4, &[0; 63]).is_err());
        assert!(from_argb32(i32::MAX, i32::MAX, &[0; 4]).is_err());
        assert!(from_argb32(4000, 4000, &[0; 16]).is_err());
        assert!(from_argb32(-1, 1, &[0; 4]).is_err());
    }

    #[test]
    fn large_pictures_are_sampled_down() {
        let (w, h) = (2048usize, 1024usize);
        let data = vec![7u8; w * h * 3];
        let (ow, oh, rgba) =
            from_image_data(w as i32, h as i32, (w * 3) as i32, false, 8, 3, &data).unwrap();
        assert_eq!((ow, oh), (SIDE, SIDE / 2));
        assert_eq!(rgba.len(), (ow * oh * 4) as usize);
        assert_eq!(&rgba[..4], &[7, 7, 7, 255]);
    }
}
