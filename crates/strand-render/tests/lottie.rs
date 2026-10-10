//! (M4) `lottie "loader.json" { speed: 1 }` (design.md: Lottie via
//! velato): drawn by velato into vello_cpu on a clock at the file's frame
//! rate, looping. Offline PNGs at fixed times. Regenerate with
//! `STRAND_BLESS=1 cargo test -p strand-render --test lottie`.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::*;
use strand_render::Renderer;
use strand_scene::*;

const S: SurfaceId = SurfaceId(1);
const TOLERANCE: u8 = 2;
const T0: Duration = Duration::from_secs(1);

/// 100 × 100 at 30 fps, 60 frames: a red 20 × 20 square moving from
/// x = 20 to x = 80 along y = 50, linearly.
const SLIDE: &str = r#"{"v":"5.7.0","fr":30,"ip":0,"op":60,"w":100,"h":100,"layers":[
 {"ty":4,"ind":1,"ip":0,"op":60,"st":0,
  "ks":{"o":{"a":0,"k":100},"r":{"a":0,"k":0},
        "p":{"a":1,"k":[{"t":0,"s":[20,50,0],"i":{"x":[1],"y":[1]},"o":{"x":[0],"y":[0]}},{"t":60,"s":[80,50,0]}]},
        "a":{"a":0,"k":[0,0,0]},"s":{"a":0,"k":[100,100,100]}},
  "shapes":[{"ty":"gr","it":[
    {"ty":"rc","p":{"a":0,"k":[0,0]},"s":{"a":0,"k":[20,20]},"r":{"a":0,"k":0}},
    {"ty":"fl","c":{"a":0,"k":[1,0,0,1]},"o":{"a":0,"k":100}},
    {"ty":"tr","p":{"a":0,"k":[0,0]},"a":{"a":0,"k":[0,0]},"s":{"a":0,"k":[100,100]},"r":{"a":0,"k":0},"o":{"a":0,"k":100}}]}]}]}"#;

fn file(name: &str, text: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("strand-lottie-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("slide.json");
    std::fs::write(&p, text).unwrap();
    p
}

/// A 100 × 100 bar holding the animation at its own size.
fn scene(source: &str, speed: f32) -> (Renderer, NodeId, Buffer) {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Bar,
        None,
        vec![
            (Prop::Width, num(100.0)),
            (Prop::Height, num(100.0)),
            (Prop::Bg, color("#1e1e2e")),
        ],
    );
    let l = b.node(
        NodeKind::Lottie,
        Some(root),
        vec![
            (Prop::Source, text(source)),
            (Prop::Size, num(100.0)),
            (Prop::Speed, num(speed)),
        ],
    );
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    (r, l, Buffer::new(100, 100, Scale::ONE))
}

fn red(buf: &Buffer, x: u32, y: u32) -> bool {
    let [b, g, r, _] = buf.px(x, y);
    r > 200 && g < 60 && b < 60
}

/// Paints at 60 Hz from `from` to `to`, as the frame clock asks.
fn run(r: &mut Renderer, buf: &mut Buffer, from: Duration, to: Duration) -> u32 {
    let mut painted = 0;
    let mut at = from;
    while at < to {
        at += Duration::from_micros(16_667);
        if (r.wants_frame(S) || r.next_wake().is_some()) && !buf.paint_at(r, S, 1, at).is_empty() {
            painted += 1;
        }
    }
    painted
}

#[test]
fn a_lottie_plays_at_its_frame_rate_and_loops() {
    let f = file("play", SLIDE);
    let (mut r, _l, mut buf) = scene(f.to_str().unwrap(), 1.0);
    buf.paint_at(&mut r, S, 0, T0);
    // Frame 0: the square centred at (20, 50).
    assert!(red(&buf, 20, 50) && !red(&buf, 50, 50));
    assert_matches_ref("lottie_frame0", &buf, TOLERANCE);
    // The file has been read: its clock runs at 30 fps, under refresh.
    let painted = run(&mut r, &mut buf, T0, T0 + Duration::from_secs(1));
    assert!(
        (25..=31).contains(&painted),
        "{painted} frames drawn in 1 s at 30 fps"
    );
    // One second in: frame 30, the square at (50, 50).
    assert!(red(&buf, 50, 50) && !red(&buf, 20, 50));
    assert_matches_ref("lottie_frame30", &buf, TOLERANCE);
    // Two seconds in it has looped back to the start.
    run(
        &mut r,
        &mut buf,
        T0 + Duration::from_secs(1),
        T0 + Duration::from_secs(2),
    );
    assert!(red(&buf, 20, 50) || red(&buf, 22, 50), "looped");
}

#[test]
fn speed_hidden_and_reduced_motion() {
    let f = file("speed", SLIDE);
    let (mut r, l, mut buf) = scene(f.to_str().unwrap(), 2.0);
    buf.paint_at(&mut r, S, 0, T0);
    run(&mut r, &mut buf, T0, T0 + Duration::from_millis(500));
    // Half a second at speed 2: frame 30.
    assert!(red(&buf, 50, 50), "speed 2");
    // Hidden: no clock.
    let mut d = SceneDiff::default();
    d.set(l, Prop::Opacity, num(0.0));
    assert!(r.apply(d).is_empty());
    let mut at = T0 + Duration::from_millis(500);
    let mut k = 0;
    while r.wants_frame(S) {
        at += Duration::from_millis(16);
        buf.paint_at(&mut r, S, 1, at);
        k += 1;
        assert!(k < 100, "hiding settles");
    }
    assert_eq!(r.next_wake(), None, "a hidden lottie has no clock");
    // Shown under reduced motion: frozen.
    r.set_reduced_motion(true);
    let mut d = SceneDiff::default();
    d.set(l, Prop::Opacity, num(1.0));
    assert!(r.apply(d).is_empty());
    while r.wants_frame(S) {
        at += Duration::from_millis(16);
        buf.paint_at(&mut r, S, 1, at);
        k += 1;
        assert!(k < 200, "showing settles");
    }
    assert_eq!(r.next_wake(), None, "frozen under reduced motion");
}

/// A missing or broken file, or no source at all (a value logic has
/// not set yet), draws nothing and keeps no clock running.
#[test]
fn a_broken_or_missing_file_draws_nothing_and_has_no_clock() {
    for (name, source) in [
        ("empty", String::new()),
        ("blank", "  ".to_string()),
        ("missing", "/nonexistent/slide.json".to_string()),
        (
            "broken",
            file("broken", "{not lottie").to_str().unwrap().to_string(),
        ),
    ] {
        let (mut r, _l, mut buf) = scene(&source, 1.0);
        buf.paint_at(&mut r, S, 0, T0);
        buf.paint_at(&mut r, S, 1, T0 + Duration::from_millis(16));
        assert!(!r.wants_frame(S), "{name}");
        assert_eq!(r.next_wake(), None, "{name}");
        let [b, g, rr, _] = buf.px(50, 50);
        assert_eq!((rr, g, b), (0x1e, 0x1e, 0x2e), "{name}");
    }
}

/// A `w × h` PNG of one opaque colour.
fn png_of(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().unwrap();
        let data: Vec<u8> = (0..w * h).flat_map(|_| rgb).collect();
        wr.write_image_data(&data).unwrap();
    }
    out
}

fn base64(data: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                s.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

/// 100 × 100: two image layers, a 16 px asset authored at 20 × 20
/// embedded as a `data:` URL at (10, 40), and a file beside the
/// animation at (60, 40).
fn with_images(dir_file: &str) -> String {
    let red = base64(&png_of(16, 16, [255, 0, 0]));
    let layer = |ind: u32, id: &str, x: u32| {
        format!(
            r#"{{"ty":2,"ind":{ind},"refId":"{id}","ip":0,"op":60,"st":0,
              "ks":{{"o":{{"a":0,"k":100}},"r":{{"a":0,"k":0}},"p":{{"a":0,"k":[{x},40,0]}},
                    "a":{{"a":0,"k":[0,0,0]}},"s":{{"a":0,"k":[100,100,100]}}}}}}"#
        )
    };
    format!(
        r#"{{"v":"5.7.0","fr":30,"ip":0,"op":60,"w":100,"h":100,
          "assets":[{{"id":"red","w":20,"h":20,"u":"","p":"data:image/png;base64,{red}","e":1}},
                    {{"id":"green","w":20,"h":20,"u":"","p":"{dir_file}","e":0}}],
          "layers":[{},{}]}}"#,
        layer(1, "red", 10),
        layer(2, "green", 60)
    )
}

/// design.md "Lottie via velato": image layers draw their assets, an
/// embedded `data:` PNG (scaled to its authored size) and a PNG beside
/// the file (ref `lottie_images.png`).
#[test]
fn image_layers_draw_embedded_and_neighbouring_assets() {
    let f = file("images", "{}");
    let dir = f.parent().unwrap();
    std::fs::write(dir.join("green.png"), png_of(20, 20, [0, 255, 0])).unwrap();
    std::fs::write(&f, with_images("green.png")).unwrap();
    let (mut r, _l, mut buf) = scene(f.to_str().unwrap(), 1.0);
    buf.paint_at(&mut r, S, 0, T0);
    assert_matches_ref("lottie_images", &buf, TOLERANCE);
    // The embedded red, over its authored 20 × 20.
    assert!(
        red(&buf, 12, 42) && red(&buf, 28, 58),
        "{:?}",
        buf.px(28, 58)
    );
    assert!(!red(&buf, 32, 50));
    // The neighbouring green.
    let [b, g, rr, _] = buf.px(70, 50);
    assert!(g > 200 && rr < 60 && b < 60, "{:?}", buf.px(70, 50));
    let [_, g, _, _] = buf.px(85, 50);
    assert!(g < 100, "outside the asset");
}

/// An asset authored at 4000 × 2000 (from a 4 × 2 PNG, left half red,
/// right half green) is decoded within the asset budget but drawn over
/// its whole authored box: a layer scaled to 1 % shows it at 40 × 20, red
/// on the left and green on the right, and nothing past it.
#[test]
fn a_large_asset_is_decoded_small_and_drawn_over_its_box() {
    let mut png_bytes = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut png_bytes, 4, 2);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().unwrap();
        let row = [255, 0, 0, 255, 0, 0, 0, 255, 0, 0, 255, 0];
        wr.write_image_data(&[row, row].concat()).unwrap();
    }
    let json = format!(
        r#"{{"v":"5.7.0","fr":30,"ip":0,"op":60,"w":100,"h":100,
          "assets":[{{"id":"big","w":4000,"h":2000,"u":"","p":"data:image/png;base64,{}","e":1}}],
          "layers":[{{"ty":2,"ind":1,"refId":"big","ip":0,"op":60,"st":0,
              "ks":{{"o":{{"a":0,"k":100}},"r":{{"a":0,"k":0}},"p":{{"a":0,"k":[30,40,0]}},
                    "a":{{"a":0,"k":[0,0,0]}},"s":{{"a":0,"k":[1,1,100]}}}}}}]}}"#,
        base64(&png_bytes)
    );
    let f = file("big-asset", &json);
    let (mut r, _l, mut buf) = scene(f.to_str().unwrap(), 1.0);
    buf.paint_at(&mut r, S, 0, T0);
    // The box is (30, 40)–(70, 60).
    assert!(red(&buf, 34, 50), "{:?}", buf.px(34, 50));
    let [b, g, rr, _] = buf.px(66, 50);
    assert!(g > 200 && rr < 60 && b < 60, "{:?}", buf.px(66, 50));
    for (x, y) in [(26, 50), (74, 50), (50, 36), (50, 64)] {
        let [b, g, rr, _] = buf.px(x, y);
        assert_eq!((rr, g, b), (0x1e, 0x1e, 0x2e), "({x}, {y}) is outside");
    }
}

/// With a text worker, the file is read and parsed on the image worker,
/// not during the frame: the first frame draws nothing and keeps no
/// clock, and the read's arrival wakes the loop and repaints with it.
#[test]
fn the_file_is_read_on_the_image_worker() {
    use strand_text::{FontConfig, TextWorker, test_font_path};
    let f = file("worker", SLIDE);
    let data = std::fs::read(test_font_path()).unwrap();
    let woken = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let w2 = woken.clone();
    let worker = TextWorker::spawn_with_waker(
        FontConfig::isolated(vec![std::sync::Arc::new(data)]),
        Some(Box::new(move || {
            w2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })),
    )
    .unwrap();
    let mut r = Renderer::new(strand_render::TextBackend::Worker(worker));
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Bar,
        None,
        vec![
            (Prop::Width, num(100.0)),
            (Prop::Height, num(100.0)),
            (Prop::Bg, color("#1e1e2e")),
        ],
    );
    b.node(
        NodeKind::Lottie,
        Some(root),
        vec![
            (Prop::Source, text(f.to_str().unwrap())),
            (Prop::Size, num(100.0)),
        ],
    );
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(100, 100, Scale::ONE);
    buf.paint_at(&mut r, S, 0, T0);
    assert!(!red(&buf, 20, 50), "not read during the frame");
    assert_eq!(r.next_wake(), None, "no clock while it is read");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !r.wants_frame(S) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
        r.update();
    }
    assert!(r.wants_frame(S), "its arrival repaints");
    assert!(woken.load(std::sync::atomic::Ordering::SeqCst) > 0);
    buf.paint_at(&mut r, S, 1, T0 + Duration::from_millis(16));
    assert!(red(&buf, 20, 50), "drawn once read");
}
