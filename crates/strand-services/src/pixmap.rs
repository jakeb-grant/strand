//! Pixel data from D-Bus (a notification's `image-data`, a tray item's
//! `IconPixmap`) written as PNG files, so `image` shows them by path like
//! any other picture.
//!
//! Files are content-addressed (`<hash>.png`) under
//! `$XDG_RUNTIME_DIR/strand/pixmaps` (the temporary directory without
//! one): the same pixels are one file, written once. This process keeps
//! at most [`KEEP`] of the files it wrote and removes the oldest beyond
//! that (an animated tray icon writes a new one per frame).

use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// How many of the files it wrote this process keeps.
pub const KEEP: usize = 256;

static WRITTEN: Mutex<VecDeque<PathBuf>> = Mutex::new(VecDeque::new());

/// Where pixmaps go.
pub fn dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(std::env::temp_dir)
        .join("strand")
        .join("pixmaps")
}

/// RGBA pixels (row-major, `width * height * 4` bytes) as a PNG file;
/// its path, or why not.
pub fn write_rgba(width: u32, height: u32, rgba: &[u8]) -> Result<PathBuf, String> {
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
    let dir = dir();
    let path = dir.join(format!("{:016x}.png", h.finish()));
    if path.exists() {
        return Ok(path);
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = dir.join(format!(".{:016x}.{}.tmp", h.finish(), std::process::id()));
    encode(&tmp, width, height, rgba)?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    if let Ok(mut w) = WRITTEN.lock() {
        w.push_back(path.clone());
        while w.len() > KEEP {
            if let Some(old) = w.pop_front() {
                let _ = std::fs::remove_file(old);
            }
        }
    }
    Ok(path)
}

fn encode(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().map_err(|e| e.to_string())?;
    w.write_image_data(rgba).map_err(|e| e.to_string())?;
    w.finish().map_err(|e| e.to_string())
}

/// The notification spec's `image-data` (`(iiibiiay)`: width, height,
/// rowstride, has alpha, bits per sample, channels, data) as RGBA.
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
    let (w, h, stride, ch) = (
        u32::try_from(width).map_err(|_| "negative width")?,
        u32::try_from(height).map_err(|_| "negative height")?,
        usize::try_from(rowstride).map_err(|_| "negative rowstride")?,
        channels as usize,
    );
    let mut out = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h as usize {
        let row = data.get(y * stride..).ok_or("image-data too short")?;
        for x in 0..w as usize {
            let px = row.get(x * ch..x * ch + ch).ok_or("image-data too short")?;
            out.extend_from_slice(&[px[0], px[1], px[2], if ch == 4 { px[3] } else { 255 }]);
        }
    }
    Ok((w, h, out))
}

/// StatusNotifierItem's `IconPixmap` entry (ARGB32 in network byte
/// order) as RGBA.
#[allow(dead_code)] // The tray uses it (next commit).
pub fn from_argb32(width: i32, height: i32, data: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let w = u32::try_from(width).map_err(|_| "negative width")?;
    let h = u32::try_from(height).map_err(|_| "negative height")?;
    let n = (w as usize) * (h as usize) * 4;
    let src = data.get(..n).ok_or("pixmap too short")?;
    let mut out = Vec::with_capacity(n);
    for px in src.chunks_exact(4) {
        out.extend_from_slice(&[px[1], px[2], px[3], px[0]]);
    }
    Ok((w, h, out))
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
        let (_, _, argb) = from_argb32(1, 1, &[255, 10, 20, 30]).unwrap();
        assert_eq!(argb, [10, 20, 30, 255]);
        let a = write_rgba(2, 1, &rgba).unwrap();
        let b = write_rgba(2, 1, &rgba).unwrap();
        assert_eq!(a, b, "content-addressed");
        let bytes = std::fs::read(&a).unwrap();
        assert_eq!(&bytes[1..4], b"PNG");
        assert!(write_rgba(0, 1, &[]).is_err());
    }
}
