//! (M4) Bindable SVG (design.md: `svg "icon.svg" { #needle { rotate:
//! level * 270deg } }`): an `svg` draws its file, and each `svg_part`
//! child applies its props to the layer with its id. Offline PNGs.
//! Regenerate with `STRAND_BLESS=1 cargo test -p strand-render --test svg`.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::*;
use strand_render::Renderer;
use strand_scene::*;

const S: SurfaceId = SurfaceId(1);
const TOLERANCE: u8 = 2;
const T0: Duration = Duration::from_secs(1);

/// A 40 × 40 gauge: a grey face, a white needle pointing up from the
/// centre (inside a translated group), and a red dot.
const GAUGE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="40" viewBox="0 0 40 40">
  <circle id="face" cx="20" cy="20" r="19" fill="#585b70"/>
  <g transform="translate(20 0)">
    <rect id="needle" x="-2" y="4" width="4" height="16" rx="2" fill="#ffffff"/>
  </g>
  <circle id="dot" cx="20" cy="20" r="3" fill="#f38ba8"/>
</svg>"##;

fn gauge_file(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("strand-svg-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("gauge.svg");
    std::fs::write(&p, GAUGE).unwrap();
    p
}

fn set(r: &mut Renderer, id: NodeId, prop: Prop, v: PropValue) {
    let mut d = SceneDiff::default();
    d.set(id, prop, v);
    assert!(r.apply(d).is_empty());
}

/// A 56 × 56 bar holding a 40 × 40 `svg` with parts `#needle` and `#face`.
fn scene(file: &str, needle: Vec<(Prop, PropValue)>) -> (Renderer, NodeId, NodeId, Buffer) {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Bar,
        None,
        vec![
            (Prop::Width, num(56.0)),
            (Prop::Height, num(56.0)),
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Pad, num(8.0)),
        ],
    );
    let svg = b.node(
        NodeKind::Svg,
        Some(root),
        vec![(Prop::Source, text(file)), (Prop::Size, num(40.0))],
    );
    let mut props = vec![(Prop::Name, text("needle"))];
    props.extend(needle);
    let part = b.node(NodeKind::SvgPart, Some(svg), props);
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    (r, svg, part, Buffer::new(56, 56, Scale::ONE))
}

/// Straight RGB of (x, y).
fn rgb(buf: &Buffer, x: u32, y: u32) -> (u8, u8, u8) {
    let [b, g, r, _] = buf.px(x, y);
    (r, g, b)
}

fn white(p: (u8, u8, u8)) -> bool {
    p.0 > 230 && p.1 > 230 && p.2 > 230
}

#[test]
fn an_svg_draws_and_its_needle_turns_with_its_part() {
    let file = gauge_file("turn");
    let (mut r, _svg, part, mut buf) = scene(file.to_str().unwrap(), Vec::new());
    buf.paint_at(&mut r, S, 0, T0);
    assert_matches_ref("svg_gauge", &buf, TOLERANCE);
    // The needle points up: white above the centre (28, 28), not right.
    assert!(white(rgb(&buf, 28, 16)));
    assert!(!white(rgb(&buf, 40, 28)));
    assert!(!r.wants_frame(S), "no clock");
    assert_eq!(r.next_wake(), None);

    // `rotate: 90deg` springs the needle right about the drawing's
    // centre, though its parent is translated; each frame repaints only
    // the svg.
    set(&mut r, part, Prop::Rotate, PropValue::Angle(90.0));
    assert!(r.wants_frame(S));
    let mut at = T0;
    let mut frames = 0;
    while r.wants_frame(S) {
        at += Duration::from_millis(16);
        let damage = buf.paint_at(&mut r, S, 1, at);
        if let Some(d) = damage.bounds() {
            assert!(
                d.x >= 8 && d.y >= 8 && d.x + d.w as i32 <= 48 && d.y + d.h as i32 <= 48,
                "{d:?}"
            );
        }
        frames += 1;
        assert!(frames < 200, "the spring settles");
    }
    assert!(frames > 3, "it springs: {frames} frames");
    assert!(white(rgb(&buf, 40, 28)), "{:?}", rgb(&buf, 40, 28));
    assert!(!white(rgb(&buf, 28, 16)));
    assert_matches_ref("svg_gauge_90", &buf, TOLERANCE);
    assert!(!r.wants_frame(S));
}

#[test]
fn parts_take_opacity_fill_and_offsets() {
    let file = gauge_file("paint");
    let (mut r, svg, _part, mut buf) = scene(
        file.to_str().unwrap(),
        vec![
            (Prop::Fill, color("#a6e3a1")),
            (Prop::X, num(-6.0)),
            (Prop::Scale, num(0.75)),
        ],
    );
    // The face at half opacity.
    let mut d = SceneDiff::default();
    let face = NodeId::new(100, 0);
    d.create(face, NodeKind::SvgPart, Some(svg), u32::MAX);
    d.set(face, Prop::Name, text("face"));
    d.set(face, Prop::Opacity, num(0.5));
    // An id the file lacks does nothing.
    let none = NodeId::new(101, 0);
    d.create(none, NodeKind::SvgPart, Some(svg), u32::MAX);
    d.set(none, Prop::Name, text("nope"));
    d.set(none, Prop::Rotate, PropValue::Angle(45.0));
    assert!(r.apply(d).is_empty());
    buf.paint_at(&mut r, S, 0, T0);
    assert_matches_ref("svg_parts", &buf, TOLERANCE);
    // The needle is green and 6 units left of the centre line.
    let (r_, g, b) = rgb(&buf, 22, 18);
    assert!(g > 200 && r_ < 200 && b < 200, "{:?}", (r_, g, b));
    // The face is half blended into the background.
    let (fr, fg, fb) = rgb(&buf, 14, 40);
    assert!(
        (fr as i32 - 0x3b).abs() < 6 && (fb as i32 - 0x4f).abs() < 6,
        "{:?}",
        (fr, fg, fb)
    );
}

#[test]
fn a_part_reading_time_gives_the_svg_a_clock() {
    let file = gauge_file("time");
    let spin = PropValue::Token(TokenExpr::Binary {
        op: BinOp::Mul,
        lhs: Box::new(TokenExpr::Time),
        rhs: Box::new(TokenExpr::value(PropValue::Number(90.0))),
    });
    let (mut r, _svg, _part, mut buf) = scene(file.to_str().unwrap(), vec![(Prop::Rotate, spin)]);
    buf.paint_at(&mut r, S, 0, T0);
    assert!(r.wants_frame(S), "time runs");
    // One second in: a quarter turn.
    let mut at = T0;
    for _ in 0..62 {
        at += Duration::from_millis(16);
        buf.paint_at(&mut r, S, 1, at);
    }
    // The node's time started at its first frame: about 0.99 s later
    // the needle points right.
    assert!(white(rgb(&buf, 40, 28)), "{:?}", rgb(&buf, 40, 28));
    r.set_reduced_motion(true);
    let mut k = 0;
    while r.wants_frame(S) && k < 10 {
        at += Duration::from_millis(16);
        buf.paint_at(&mut r, S, 1, at);
        k += 1;
    }
    assert!(!r.wants_frame(S), "reduced motion freezes it");
}

#[test]
fn an_svg_fill_colours_the_drawing_and_a_missing_file_draws_nothing() {
    let file = gauge_file("fill");
    let (mut r, svg, _part, mut buf) = scene(file.to_str().unwrap(), Vec::new());
    set(&mut r, svg, Prop::Fill, color("#89b4fa"));
    buf.paint_at(&mut r, S, 0, T0);
    let (r_, g, b) = rgb(&buf, 28, 16);
    assert_eq!((r_, g, b), (0x89, 0xb4, 0xfa), "the needle in the fill");
    let (r_, g, b) = rgb(&buf, 14, 40);
    assert_eq!((r_, g, b), (0x89, 0xb4, 0xfa), "the face too");

    let (mut r, _svg, _part, mut buf) = scene("/nonexistent/gauge.svg", Vec::new());
    buf.paint_at(&mut r, S, 0, T0);
    assert_eq!(rgb(&buf, 28, 28), (0x1e, 0x1e, 0x2e));
}

/// With a text worker, the file is read on the image worker: the first
/// frame shows only the bar, and the read's arrival repaints the gauge.
#[test]
fn the_file_is_read_on_the_image_worker() {
    use strand_text::{FontConfig, TextWorker, test_font_path};
    let f = gauge_file("worker");
    let data = std::fs::read(test_font_path()).unwrap();
    let worker =
        TextWorker::spawn_with_waker(FontConfig::isolated(vec![std::sync::Arc::new(data)]), None)
            .unwrap();
    let mut r = Renderer::new(strand_render::TextBackend::Worker(worker));
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Bar,
        None,
        vec![
            (Prop::Width, num(56.0)),
            (Prop::Height, num(56.0)),
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Pad, num(8.0)),
        ],
    );
    b.node(
        NodeKind::Svg,
        Some(root),
        vec![
            (Prop::Source, text(f.to_str().unwrap())),
            (Prop::Size, num(40.0)),
        ],
    );
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(56, 56, Scale::ONE);
    buf.paint_at(&mut r, S, 0, T0);
    assert_eq!(
        rgb(&buf, 28, 16),
        (0x1e, 0x1e, 0x2e),
        "not read in the frame"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !r.wants_frame(S) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
        r.update();
    }
    assert!(r.wants_frame(S), "its arrival repaints");
    buf.paint_at(&mut r, S, 1, T0 + Duration::from_millis(16));
    assert!(
        white(rgb(&buf, 28, 16)),
        "the needle: {:?}",
        rgb(&buf, 28, 16)
    );
}
