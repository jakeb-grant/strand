//! (M4) Animated GIF, APNG and WebP (design.md, "Generative, data-driven
//! and media": `image "spin.gif"`, frames streamed, not cached whole;
//! "Per-node clocks with frame caps": GIFs at their own rate): offline
//! PNGs at fixed times, the frames composed as each format says, a clock
//! at the frames' tick that stops under `reduced_motion` and after a
//! finite loop count, and at most two frames held per image and size.
//! Regenerate with `STRAND_BLESS=1 cargo test -p strand-render --test animated`.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::*;
use strand_render::Renderer;
use strand_render::image::{Fit, IconTheme, ImageBackend, ImageKey, ImageStore, load};
use strand_scene::*;

const S: SurfaceId = SurfaceId(1);
const TOLERANCE: u8 = 2;

/// The three frames every fixture holds, 16 × 16:
/// 0. all red, 100 ms;
/// 1. a green 8 × 8 square at (4, 4) over it, 100 ms, its area cleared
///    after it;
/// 2. a blue 8 × 8 square at (0, 0), 200 ms.
///
/// So frame 2 shows blue top left, the rest of the green square's area
/// clear, and red elsewhere.
const RED: [u8; 4] = [0xe0, 0x30, 0x30, 0xff];
const GREEN: [u8; 4] = [0x30, 0xc0, 0x50, 0xff];
const BLUE: [u8; 4] = [0x30, 0x60, 0xe0, 0xff];

fn solid(w: u32, h: u32, c: [u8; 4]) -> Vec<u8> {
    c.repeat((w * h) as usize)
}

fn dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("strand-animated-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The GIF: a palette of the three colours, the second frame disposed
/// to the background (cleared), looping forever, or (`forever` false,
/// with no loop extension) played once.
fn write_gif(path: &Path, forever: bool) {
    let palette: Vec<u8> = [RED, GREEN, BLUE]
        .iter()
        .flat_map(|c| [c[0], c[1], c[2]])
        .collect();
    let mut out = Vec::new();
    {
        let mut enc = gif::Encoder::new(&mut out, 16, 16, &palette).unwrap();
        if forever {
            enc.set_repeat(gif::Repeat::Infinite).unwrap();
        }
        let mut f0 = gif::Frame::from_palette_pixels(16, 16, vec![0; 256], palette.clone(), None);
        f0.delay = 10;
        enc.write_frame(&f0).unwrap();
        let mut f1 = gif::Frame::from_palette_pixels(8, 8, vec![1; 64], palette.clone(), None);
        (f1.left, f1.top, f1.delay) = (4, 4, 10);
        f1.dispose = gif::DisposalMethod::Background;
        enc.write_frame(&f1).unwrap();
        let mut f2 = gif::Frame::from_palette_pixels(8, 8, vec![2; 64], palette.clone(), None);
        f2.delay = 20;
        enc.write_frame(&f2).unwrap();
    }
    std::fs::write(path, out).unwrap();
}

/// The APNG: the default image is frame 0; frame 1 blends over it and is
/// disposed to the background.
fn write_apng(path: &Path) {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, 16, 16);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_animated(3, 0).unwrap();
        enc.set_frame_delay(1, 10).unwrap();
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&solid(16, 16, RED)).unwrap();
        w.set_frame_dimension(8, 8).unwrap();
        w.set_frame_position(4, 4).unwrap();
        w.set_blend_op(png::BlendOp::Over).unwrap();
        w.set_dispose_op(png::DisposeOp::Background).unwrap();
        w.write_image_data(&solid(8, 8, GREEN)).unwrap();
        w.set_frame_position(0, 0).unwrap();
        w.set_dispose_op(png::DisposeOp::None).unwrap();
        w.set_frame_delay(2, 10).unwrap();
        w.write_image_data(&solid(8, 8, BLUE)).unwrap();
        w.finish().unwrap();
    }
    std::fs::write(path, out).unwrap();
}

/// A lossless WebP frame's `VP8L` chunk (header, body and padding).
fn vp8l(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    let mut file = Vec::new();
    image_webp::WebPEncoder::new(&mut file)
        .encode(rgba, w, h, image_webp::ColorType::Rgba8)
        .unwrap();
    assert_eq!(&file[12..16], b"VP8L");
    file[12..].to_vec()
}

fn u24(v: u32) -> [u8; 3] {
    let b = v.to_le_bytes();
    [b[0], b[1], b[2]]
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(kind);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    if body.len() % 2 == 1 {
        out.push(0);
    }
}

/// The animated WebP: `VP8X`, `ANIM` and one `ANMF` per frame (frame 1
/// disposed to the background; frames blend over the canvas).
fn write_webp(path: &Path) {
    let mut body = Vec::new();
    let mut vp8x = vec![0x10 | 0x02, 0, 0, 0];
    vp8x.extend_from_slice(&u24(15));
    vp8x.extend_from_slice(&u24(15));
    chunk(&mut body, b"VP8X", &vp8x);
    chunk(&mut body, b"ANIM", &[0, 0, 0, 0, 0, 0]);
    // (x, y, side, colour, delay ms, flags)
    type Frame<'a> = (u32, u32, u32, &'a [u8; 4], u32, u8);
    let frames: [Frame; 3] = [
        (0, 0, 16, &RED, 100, 0),
        (4, 4, 8, &GREEN, 100, 1),
        (0, 0, 8, &BLUE, 200, 0),
    ];
    for (x, y, side, c, ms, dispose) in frames {
        let mut anmf = Vec::new();
        anmf.extend_from_slice(&u24(x / 2));
        anmf.extend_from_slice(&u24(y / 2));
        anmf.extend_from_slice(&u24(side - 1));
        anmf.extend_from_slice(&u24(side - 1));
        anmf.extend_from_slice(&u24(ms));
        anmf.push(dispose);
        anmf.extend_from_slice(&vp8l(side, side, &solid(side, side, *c)));
        chunk(&mut body, b"ANMF", &anmf);
    }
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(body.len() as u32 + 4).to_le_bytes());
    out.extend_from_slice(b"WEBP");
    out.extend_from_slice(&body);
    std::fs::write(path, out).unwrap();
}

#[derive(Clone)]
struct Files {
    gif: String,
    apng: String,
    webp: String,
}

/// The three animations, written once per process: the tests run on
/// parallel threads, and a test rewriting a file (truncate, then write)
/// while another decodes it would hand that one half a file.
fn files() -> Files {
    static FILES: std::sync::OnceLock<Files> = std::sync::OnceLock::new();
    FILES.get_or_init(write_files).clone()
}

fn write_files() -> Files {
    let d = dir();
    let (gif, apng, webp) = (d.join("spin.gif"), d.join("spin.png"), d.join("spin.webp"));
    write_gif(&gif, true);
    write_apng(&apng);
    write_webp(&webp);
    let s = |p: PathBuf| p.to_string_lossy().into_owned();
    Files {
        gif: s(gif),
        apng: s(apng),
        webp: s(webp),
    }
}

/// Three 48 × 48 images in a row (each 16 × 16 source drawn at 3×).
fn scene(sources: &[&str]) -> (SceneDiff, Vec<NodeId>) {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(176.0)),
            (Prop::Height, num(64.0)),
            (Prop::Bg, color("#282c34")),
        ],
    );
    let row = b.node(
        NodeKind::Row,
        Some(root),
        vec![(Prop::Pad, num(8.0)), (Prop::Gap, num(8.0))],
    );
    let imgs = sources
        .iter()
        .map(|s| {
            b.node(
                NodeKind::Image,
                Some(row),
                vec![(Prop::Source, text(s)), (Prop::Size, num(48.0))],
            )
        })
        .collect();
    (b.diff, imgs)
}

fn setup(sources: &[&str]) -> (Renderer, Buffer, Vec<NodeId>) {
    let (diff, imgs) = scene(sources);
    let mut r = renderer();
    assert!(r.apply(diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    (r, Buffer::new(176, 64, Scale::ONE), imgs)
}

/// The colour (straight RGB) at source pixel `(sx, sy)` of image `i`.
fn at(buf: &Buffer, i: u32, sx: u32, sy: u32) -> [u8; 3] {
    let x = 8 + i * 56 + sx * 3 + 1;
    let y = 8 + sy * 3 + 1;
    let [b, g, r, _] = buf.px(x, y);
    [r, g, b]
}

fn rgb(c: [u8; 4]) -> [u8; 3] {
    [c[0], c[1], c[2]]
}

/// Asserts `got` is `want` within 2 per channel (WebP composes its
/// frames with its own rounding).
#[track_caller]
fn near(got: [u8; 3], want: [u8; 3], what: &str) {
    let close = got
        .iter()
        .zip(want)
        .all(|(a, b)| (*a as i32 - b as i32).abs() <= 2);
    assert!(close, "{what}: {got:?}, want {want:?}");
}

const BG: [u8; 3] = [0x28, 0x2c, 0x34];
const T0: Duration = Duration::from_secs(1);

fn ms(n: u64) -> Duration {
    T0 + Duration::from_millis(n)
}

#[test]
fn gif_apng_and_webp_play_their_frames_composed() {
    let f = files();
    let (mut r, mut buf, _) = setup(&[&f.gif, &f.apng, &f.webp]);
    buf.paint_at(&mut r, S, 0, T0);
    for i in 0..3 {
        near(at(&buf, i, 8, 8), rgb(RED), &format!("image {i} at 0 ms"));
    }
    assert!(!r.wants_frame(S), "no frame before the first frame change");
    assert!(r.next_wake().is_some(), "woken at the tick");

    buf.paint_at(&mut r, S, 1, ms(150));
    for i in 0..3 {
        near(
            at(&buf, i, 8, 8),
            rgb(GREEN),
            &format!("image {i} at 150 ms"),
        );
        near(at(&buf, i, 1, 1), rgb(RED), &format!("image {i} at 150 ms"));
    }
    assert_matches_ref("animated_frame1", &buf, TOLERANCE);

    buf.paint_at(&mut r, S, 1, ms(250));
    for i in 0..3 {
        near(
            at(&buf, i, 2, 2),
            rgb(BLUE),
            &format!("image {i}: blue on top"),
        );
        near(
            at(&buf, i, 10, 10),
            BG,
            &format!("image {i}: green disposed"),
        );
        near(
            at(&buf, i, 14, 14),
            rgb(RED),
            &format!("image {i}: red kept"),
        );
    }
    assert_matches_ref("animated_frame2", &buf, TOLERANCE);

    // Frame 2 lasts 200 ms; then the loop starts again.
    buf.paint_at(&mut r, S, 1, ms(350));
    near(at(&buf, 0, 2, 2), rgb(BLUE), "");
    buf.paint_at(&mut r, S, 1, ms(450));
    for i in 0..3 {
        near(at(&buf, i, 2, 2), rgb(RED), &format!("image {i} looped"));
        near(at(&buf, i, 10, 10), rgb(RED), &format!("image {i} looped"));
    }
}

/// Only frame changes repaint: the clock ticks at the delays' greatest
/// common divisor (100 ms), and a tick that shows the same frame paints
/// nothing.
#[test]
fn only_frame_changes_repaint() {
    let f = files();
    let (mut r, mut buf, _) = setup(&[&f.gif]);
    buf.paint_at(&mut r, S, 0, T0);
    let mut damaged = Vec::new();
    for k in 1..=48 {
        let t = T0 + Duration::from_nanos(1_000_000_000 * k / 60);
        if !buf.paint_at(&mut r, S, 1, t).is_empty() {
            damaged.push(k);
        }
    }
    // 0.8 s: frames change at 100, 200, 400 (the loop), 500, 600 and
    // 800 ms.
    assert_eq!(damaged.len(), 6, "repainted at {damaged:?}");
}

/// A GIF of three solid frames with these delays (centiseconds),
/// looping forever.
fn write_gif_delays(path: &Path, delays: [u16; 3]) {
    let palette: Vec<u8> = [RED, GREEN, BLUE]
        .iter()
        .flat_map(|c| [c[0], c[1], c[2]])
        .collect();
    let mut out = Vec::new();
    {
        let mut enc = gif::Encoder::new(&mut out, 16, 16, &palette).unwrap();
        enc.set_repeat(gif::Repeat::Infinite).unwrap();
        for (i, d) in delays.into_iter().enumerate() {
            let mut f =
                gif::Frame::from_palette_pixels(16, 16, vec![i as u8; 256], palette.clone(), None);
            f.delay = d;
            enc.write_frame(&f).unwrap();
        }
    }
    std::fs::write(path, out).unwrap();
}

/// Delays of 70, 80 and 90 ms share only a 10 ms step, but the clock
/// wakes the loop only at frame changes: over one second at 60 Hz, as
/// the frame clock asks (a frame when the surface wants one or its wake
/// has come), about one frame per frame change is drawn, not one per
/// refresh, and each shows a new frame.
#[test]
fn the_clock_wakes_only_at_frame_changes() {
    let gif = dir().join("uneven.gif");
    write_gif_delays(&gif, [7, 8, 9]);
    let (mut r, mut buf, _) = setup(&[&gif.to_string_lossy()]);
    // The wake is a real `Instant`, set from the real clock while the
    // paint ran: measured from just before the paint, so a test thread
    // held up after it never moves the wake earlier than asked (which
    // drew extra frames under load).
    let mut before = std::time::Instant::now();
    buf.paint_at(&mut r, S, 0, T0);
    let mut last = T0;
    let wake = |r: &Renderer, painted: Duration, before: std::time::Instant| {
        r.next_wake()
            .map(|w| painted + w.saturating_duration_since(before))
    };
    let mut due = wake(&r, T0, before);
    let (mut painted, mut changed) = (0, 0);
    for k in 1..=60u64 {
        let at = T0 + Duration::from_nanos(1_000_000_000 * k / 60);
        if !(r.wants_frame(S) || due.is_some_and(|d| at >= d)) {
            continue;
        }
        painted += 1;
        before = std::time::Instant::now();
        if !buf.paint_at(&mut r, S, 1, at).is_empty() {
            changed += 1;
        }
        last = at;
        due = wake(&r, at, before);
    }
    // Changes at 70, 150, 240, 310, 390, 480, 550, 630, 720, 790, 870
    // and 960 ms.
    assert!(
        (11..=13).contains(&changed),
        "{changed} frame changes drawn in 1 s"
    );
    assert!(
        painted <= changed + 2,
        "{painted} frames drawn for {changed} frame changes (last at {last:?})"
    );
}

#[test]
fn reduced_motion_shows_the_first_frame_and_stops_the_clock() {
    let f = files();
    let (mut r, mut buf, _) = setup(&[&f.webp]);
    r.set_reduced_motion(true);
    buf.paint_at(&mut r, S, 0, T0);
    buf.paint_at(&mut r, S, 0, ms(250));
    near(at(&buf, 0, 2, 2), rgb(RED), "");
    assert!(!r.wants_frame(S));
    assert_eq!(r.next_wake(), None, "no clock");
}

/// A GIF that plays once stops on its last frame, and its clock stops.
#[test]
fn a_finite_loop_count_stops_on_the_last_frame() {
    let gif = dir().join("once.gif");
    write_gif(&gif, false);
    let (mut r, mut buf, _) = setup(&[&gif.to_string_lossy()]);
    buf.paint_at(&mut r, S, 0, T0);
    buf.paint_at(&mut r, S, 1, ms(250));
    near(at(&buf, 0, 2, 2), rgb(BLUE), "");
    buf.paint_at(&mut r, S, 1, ms(450));
    near(at(&buf, 0, 2, 2), rgb(BLUE), "the last frame stays");
    near(at(&buf, 0, 10, 10), BG, "");
    assert!(!r.wants_frame(S));
    assert_eq!(r.next_wake(), None, "played out: no clock");
}

/// Frames are decoded one after another from the file, never all at
/// once, and at most two per image and size stay decoded.
#[test]
fn two_frames_are_held_per_image_and_size() {
    let f = files();
    let mut store = ImageStore::new(ImageBackend::Inline(IconTheme::named("hicolor")));
    let key = |frame| ImageKey {
        source: f.gif.clone(),
        icon: false,
        w: 32,
        h: 32,
        fit: Fit::Contain,
        scale: 1,
        frame,
    };
    for n in 0..9u32 {
        store.want(S, &[key(n % 3), key((n + 1) % 3)], false);
        assert!(store.frames_held(&key(0)) <= 2, "after frame {n}");
        assert_eq!(store.players(), 1);
    }
    let d = match store.get(&key(2)) {
        Some(Ok(d)) => d.clone(),
        other => panic!("frame 2: {other:?}"),
    };
    let tl = d.anim.expect("a timeline");
    assert_eq!(tl.delays, [100, 100, 200]);
    // Another size of the same image is its own pair.
    let mut other = key(1);
    other.w = 16;
    store.want(S, &[key(1), other.clone()], false);
    assert_eq!(store.frames_held(&other), 1);
    // Drawn no more: the player goes.
    store.want(S, &[], false);
    assert_eq!(store.players(), 0);
}

/// A still GIF and a still WebP draw as images; a broken file fails
/// without a panic.
#[test]
fn still_gif_and_webp_draw_and_broken_files_fail() {
    let d = dir();
    let key = |p: &Path| ImageKey {
        source: p.to_string_lossy().into_owned(),
        icon: false,
        w: 8,
        h: 8,
        fit: Fit::Fill,
        scale: 1,
        frame: 0,
    };
    let theme = IconTheme::named("hicolor");
    let webp = d.join("still.webp");
    let mut file = Vec::new();
    image_webp::WebPEncoder::new(&mut file)
        .encode(&solid(4, 4, GREEN), 4, 4, image_webp::ColorType::Rgba8)
        .unwrap();
    std::fs::write(&webp, &file).unwrap();
    let w = load(&key(&webp), &theme).unwrap();
    assert!(w.anim.is_none());
    let p = w.pixmap.data()[0];
    assert_eq!((p.b, p.g, p.r, p.a), (0x30, 0xc0, 0x50, 0xff));

    let gif = d.join("still.gif");
    let mut out = Vec::new();
    {
        let mut enc = gif::Encoder::new(&mut out, 4, 4, &[0xe0, 0x30, 0x30]).unwrap();
        enc.write_frame(&gif::Frame::from_palette_pixels(
            4,
            4,
            vec![0; 16],
            vec![0xe0, 0x30, 0x30],
            None,
        ))
        .unwrap();
    }
    std::fs::write(&gif, &out).unwrap();
    let g = load(&key(&gif), &theme).unwrap();
    assert!(g.anim.is_none());
    assert_eq!(g.pixmap.data()[0].b, 0xe0, "red, stored blue-first");

    let broken = d.join("broken.gif");
    std::fs::write(&broken, b"GIF89a\x10\x00").unwrap();
    assert!(load(&key(&broken), &theme).is_err());
    let broken = d.join("broken.webp");
    std::fs::write(&broken, b"RIFF\x04\x00\x00\x00WEBP").unwrap();
    assert!(load(&key(&broken), &theme).is_err());
}
