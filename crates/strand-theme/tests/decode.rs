//! `material(image:)` decodes at reduced size: a wallpaper never costs a
//! full-resolution frame (design.md, "Rendering, performance and memory
//! budget": full-resolution decodes are what bloats other shells).
//! Allocations are counted by this test binary's global allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use strand_theme::image::{FULL_FRAME_BYTES, seed_from_bytes};

struct Counting;

static NOW: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let now = NOW.fetch_add(l.size(), Ordering::SeqCst) + l.size();
            PEAK.fetch_max(now, Ordering::SeqCst);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        NOW.fetch_sub(l.size(), Ordering::SeqCst);
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// The tests measure one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

/// The most `f` allocates on top of what is live when it starts.
fn peak_of<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = NOW.load(Ordering::SeqCst);
    PEAK.store(base, Ordering::SeqCst);
    let out = f();
    (out, PEAK.load(Ordering::SeqCst).saturating_sub(base))
}

/// A wallpaper: three quarters blue, the rest yellow, with a ramp
/// so it does not compress to nothing.
fn wallpaper(w: u32, h: u32) -> image::RgbImage {
    image::RgbImage::from_fn(w, h, |x, y| {
        let n = ((x ^ y) & 7) as u8;
        image::Rgb(if x < w * 3 / 4 {
            [30 + n, 90, 200]
        } else {
            [240, 200 + n, 40]
        })
    })
}

fn encode(img: &image::RgbImage, f: image::ImageFormat) -> Vec<u8> {
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), f)
        .unwrap();
    out
}

const MB: usize = 1024 * 1024;

#[test]
fn a_4k_jpeg_and_png_decode_in_a_few_megabytes() {
    let _one = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let img = wallpaper(3840, 2160);
    let full = 3840 * 2160 * 3;
    for f in [image::ImageFormat::Jpeg, image::ImageFormat::Png] {
        let bytes = encode(&img, f);
        let (seed, peak) = peak_of(|| seed_from_bytes(&bytes));
        let seed = seed.unwrap();
        let lch = seed.to_oklch();
        assert!((lch.h - 260.0).abs() < 25.0, "{f:?}: {lch:?}");
        // The full frame would be 25 MB. The scaled decode is ~0.4 MB;
        // most of the ~1.9 MB peak is the quantiser's histogram.
        eprintln!("{f:?}: peak {peak} bytes");
        assert!(peak < 4 * MB, "{f:?}: peak {peak} bytes (frame {full})");
    }
}

/// A PNG written by hand with Adam7 interlacing: decoded pass by pass
/// into the same downscale as the plain file, so the same seed.
#[test]
fn interlaced_pngs_give_the_seed_of_the_plain_file() {
    let _one = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (w, h) = (301u32, 203u32);
    let img = wallpaper(w, h);
    let plain = encode(&img, image::ImageFormat::Png);
    const ADAM7: [(u32, u32, u32, u32); 7] = [
        (8, 0, 8, 0),
        (8, 4, 8, 0),
        (4, 0, 8, 4),
        (4, 2, 4, 0),
        (2, 0, 4, 2),
        (2, 1, 2, 0),
        (1, 0, 2, 1),
    ];
    let mut raw = Vec::new();
    for (xs, xo, ys, yo) in ADAM7 {
        if xo >= w || yo >= h {
            continue;
        }
        for y in (yo..h).step_by(ys as usize) {
            raw.push(0u8);
            for x in (xo..w).step_by(xs as usize) {
                raw.extend_from_slice(&img.get_pixel(x, y).0);
            }
        }
    }
    let chunk = |out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut crc = crc32fast::Hasher::new();
        crc.update(kind);
        crc.update(data);
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&crc.finalize().to_be_bytes());
    };
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 1]);
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &fdeflate::compress_to_vec(&raw));
    chunk(&mut png, b"IEND", &[]);
    // The reference decoder agrees it is the same picture.
    let check = image::load_from_memory(&png).unwrap().into_rgb8();
    assert_eq!(check, img);
    assert_eq!(
        seed_from_bytes(&png).unwrap(),
        seed_from_bytes(&plain).unwrap()
    );
}

/// A WebP (decoded whole: no reduced-size decoder exists) whose header
/// says 16000×16000 is refused before its frame is allocated; so is a
/// PNG past the pixel limit.
#[test]
fn oversized_frames_are_refused_before_they_allocate() {
    let _one = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // A lossless WebP header: signature 0x2f, then 14-bit width-1 and
    // height-1, alpha and version bits.
    let (w, h) = (16000u32, 16000u32);
    let bits: u32 = (w - 1) | ((h - 1) << 14);
    let mut vp8l = vec![0x2f];
    vp8l.extend_from_slice(&bits.to_le_bytes());
    vp8l.extend_from_slice(&[0u8; 64]);
    let mut webp = b"RIFF".to_vec();
    webp.extend_from_slice(&((4 + 8 + vp8l.len()) as u32).to_le_bytes());
    webp.extend_from_slice(b"WEBPVP8L");
    webp.extend_from_slice(&(vp8l.len() as u32).to_le_bytes());
    webp.extend_from_slice(&vp8l);
    assert!(w as u64 * h as u64 * 3 > FULL_FRAME_BYTES);
    let (r, peak) = peak_of(|| seed_from_bytes(&webp));
    assert!(r.is_err(), "{r:?}");
    assert!(peak < MB, "peak {peak}");

    // A PNG header claiming 100000×100000.
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = 13u32.to_be_bytes().to_vec();
    ihdr.extend_from_slice(b"IHDR");
    ihdr.extend_from_slice(&100_000u32.to_be_bytes());
    ihdr.extend_from_slice(&100_000u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    let crc = crc32fast::hash(&ihdr[4..]);
    ihdr.extend_from_slice(&crc.to_be_bytes());
    png.extend_from_slice(&ihdr);
    let idat = fdeflate::compress_to_vec(&[0u8; 64]);
    png.extend_from_slice(&(idat.len() as u32).to_be_bytes());
    let mut body = b"IDAT".to_vec();
    body.extend_from_slice(&idat);
    png.extend_from_slice(&body);
    png.extend_from_slice(&crc32fast::hash(&body).to_be_bytes());
    let (r, peak) = peak_of(|| seed_from_bytes(&png));
    let e = r.unwrap_err().to_string();
    assert!(e.contains("too large"), "{e}");
    assert!(peak < MB, "peak {peak}");
}
