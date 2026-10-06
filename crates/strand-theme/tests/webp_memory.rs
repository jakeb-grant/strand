//! A WebP wallpaper is decoded whole (no Rust decoder can do less), but
//! that frame is a transient: once quantised, the process's memory goes
//! back to where it was, also on the second decode, when glibc's adaptive
//! mmap threshold would otherwise keep a freed frame in the arena
//! (design.md, "Rendering, performance and memory budget": Images 1 MB).
//! One test per binary, so nothing else allocates meanwhile.

use std::path::Path;
use std::time::Duration;

use strand_theme::image::{Lookup, Quantiser};

/// This process's proportional set size, in bytes.
fn pss() -> usize {
    let text = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap();
    let kb: usize = text
        .lines()
        .find_map(|l| l.strip_prefix("Pss:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap();
    kb * 1024
}

/// A 4K lossless WebP: mostly `major`, with a ramp so it is not trivial.
fn webp(path: &Path, major: [u8; 3]) {
    let (w, h) = (3840u32, 2160u32);
    let img = image::RgbImage::from_fn(w, h, |x, y| {
        let n = ((x ^ y) & 7) as u8;
        image::Rgb(if x < w * 3 / 4 {
            [major[0], major[1] + n, major[2]]
        } else {
            [240, 200 + n, 40]
        })
    });
    let mut out = Vec::new();
    img.write_to(
        &mut std::io::Cursor::new(&mut out),
        image::ImageFormat::WebP,
    )
    .unwrap();
    std::fs::write(path, out).unwrap();
}

fn quantise(q: &mut Quantiser, path: &Path) {
    q.lookup(path);
    assert!(q.wait(Duration::from_secs(120)));
    assert!(matches!(q.lookup(path), Lookup::Ready(_)), "{path:?}");
}

#[test]
fn a_4k_webp_leaves_no_resident_frame() {
    if !Path::new("/proc/self/smaps_rollup").exists() {
        eprintln!("no smaps_rollup: skipped");
        return;
    }
    let dir = std::env::temp_dir().join(format!("strand-theme-webp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let walls: Vec<_> = (0..3u8)
        .map(|i| {
            let p = dir.join(format!("w{i}.webp"));
            webp(&p, [30 + i * 20, 90, 200]);
            p
        })
        .collect();
    let mut q = Quantiser::new(None).unwrap();
    // The worker exists and idles; the frames made above go back to the
    // system first, so the baseline is what the quantiser holds idle.
    q.lookup(&dir.join("none.webp"));
    // SAFETY: only releases free memory.
    unsafe {
        libc::malloc_trim(0);
    }
    let base = pss();
    for w in &walls {
        quantise(&mut q, w);
    }
    let after = pss();
    assert_eq!(q.quantised(), 3);
    let grown = after.saturating_sub(base);
    eprintln!("PSS {base} -> {after} (+{grown})");
    // A frame is 25 MB (33 MB with image-webp's RGBA working copy);
    // what may stay is the decoder's code pages and small change.
    assert!(grown < 4 * 1024 * 1024, "PSS grew by {grown} bytes");
    drop(q);
    let _ = std::fs::remove_dir_all(dir);
}
