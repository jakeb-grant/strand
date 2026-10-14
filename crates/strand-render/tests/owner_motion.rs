//! owner-testing.md bug 2: frame times and settle times of the owner's
//! animations at scale 1.5, where a 2560-logical bar is 3840 physical
//! pixels wide: the workspace pill moving (`x: ix ~ ease(out_back,
//! 200ms)`) across the floating, shadowed bar, again after the paint
//! cache went idle, and an island opening, with `enter { opacity: 0;
//! scale: 0.9 }` on its card as the owner wrote it (it plays nothing:
//! the card stays mounted) and on the popup (`$motion.spatial`).
//! decisions.md m6-w2-surface has the numbers.
//!
//! Prints each; `STRAND_BENCH_ROUNDS` repeats them. The assertions are
//! functional (damage, frame counts), never wall-clock. Run optimised:
//! `cargo test --profile timing -p strand-render --test owner_motion -- --nocapture`.

mod common;

use std::time::{Duration, Instant};

use common::*;
use strand_render::Renderer;
use strand_scene::*;

const BAR: SurfaceId = SurfaceId(1);
const POP: SurfaceId = SurfaceId(2);
const T0: Duration = Duration::from_secs(1);

fn frame(k: u32) -> Duration {
    T0 + Duration::from_micros(16_667 * k as u64)
}

fn shadow() -> PropValue {
    PropValue::Shadow(vec![Shadow {
        x: 0.0,
        y: 6.0,
        blur: 24.0,
        spread: 0.0,
        color: hex("#11111b59"),
    }])
}

struct Scene {
    bar: NodeId,
    pill: NodeId,
    popup: NodeId,
    card: NodeId,
}

/// The owner's bar (target/try/qs/strand/shell.strand), its shapes and
/// texts: a floating 36 px bar with a shadow, a workspace row with the
/// pill behind ten workspaces, a clock and a status row whose volume
/// item holds an island.
fn scene(b: &mut Builder) -> Scene {
    let bar = b.node(
        NodeKind::Bar,
        None,
        vec![
            (Prop::Height, num(36.0)),
            (
                Prop::Margin,
                PropValue::List(vec![num(4.0), num(4.0), num(0.0), num(4.0)]),
            ),
            (Prop::Radius, num(16.0)),
            (Prop::Bg, color("#181825e6")),
            (Prop::Shadow, shadow()),
            (Prop::Color, color("#cdd6f4")),
            (Prop::Font, PropValue::Font(font(14.0))),
        ],
    );
    let split = b.node(NodeKind::Split, Some(bar), vec![(Prop::Pad, num(4.0))]);
    let start = b.node(NodeKind::Start, Some(split), vec![(Prop::Gap, num(4.0))]);
    let ws = b.node(
        NodeKind::Row,
        Some(start),
        vec![
            (Prop::Height, num(28.0)),
            (Prop::Pad, PropValue::List(vec![num(0.0), num(8.0)])),
            (Prop::Gap, num(8.0)),
            (Prop::Radius, num(16.0)),
            (Prop::Bg, color("#313244")),
        ],
    );
    let pill = b.node(
        NodeKind::Box,
        Some(ws),
        vec![
            (Prop::Place, PropValue::Keyword("absolute".into())),
            (Prop::X, num(8.0)),
            (Prop::Width, num(55.0)),
            (Prop::Height, num(28.0)),
            (Prop::Radius, num(14.0)),
            (Prop::Bg, color("#89b4fa")),
        ],
    );
    for i in 1..=10 {
        b.node(
            NodeKind::Text,
            Some(ws),
            vec![
                (Prop::Text, text(if i % 3 == 0 { "•" } else { "◆" })),
                (Prop::Width, num(28.0)),
            ],
        );
    }
    let end = b.node(NodeKind::End, Some(split), vec![(Prop::Gap, num(4.0))]);
    b.node(
        NodeKind::Text,
        Some(end),
        vec![(Prop::Text, text("10:42 AM"))],
    );
    let status = b.node(
        NodeKind::Row,
        Some(end),
        vec![
            (Prop::Height, num(28.0)),
            (Prop::Pad, PropValue::List(vec![num(0.0), num(8.0)])),
            (Prop::Gap, num(4.0)),
            (Prop::Radius, num(16.0)),
            (Prop::Bg, color("#313244")),
        ],
    );
    let vol = b.node(NodeKind::Box, Some(status), vec![]);
    b.node(NodeKind::Text, Some(vol), vec![(Prop::Text, text("vol"))]);
    for t in ["net", "bt", "bat"] {
        b.node(NodeKind::Text, Some(status), vec![(Prop::Text, text(t))]);
    }
    let popup = b.node(
        NodeKind::Popup,
        Some(vol),
        vec![(Prop::Open, PropValue::Bool(false))],
    );
    let card = b.node(
        NodeKind::Col,
        Some(popup),
        vec![
            (Prop::Width, num(304.0)),
            (
                Prop::Pad,
                PropValue::List(vec![num(10.0), num(16.0), num(14.0), num(16.0)]),
            ),
            (Prop::Gap, num(8.0)),
            (
                Prop::Margin,
                PropValue::List(vec![num(8.0), num(0.0), num(0.0)]),
            ),
            (Prop::Radius, num(20.0)),
            (Prop::Bg, color("#181825e6")),
            (Prop::Shadow, shadow()),
            (
                Prop::Enter,
                PropValue::Pose(vec![(Prop::Opacity, num(0.0)), (Prop::Scale, num(0.9))]),
            ),
        ],
    );
    b.node(
        NodeKind::Text,
        Some(card),
        vec![(Prop::Text, text("Volume 42%"))],
    );
    b.node(NodeKind::Slider, Some(card), vec![(Prop::Value, num(0.42))]);
    let row = b.node(NodeKind::Row, Some(card), vec![(Prop::Gap, num(10.0))]);
    b.node(
        NodeKind::Box,
        Some(row),
        vec![
            (Prop::Size, num(48.0)),
            (Prop::Radius, num(10.0)),
            (Prop::Bg, color("#45475a")),
        ],
    );
    let col = b.node(NodeKind::Col, Some(row), vec![]);
    b.node(
        NodeKind::Text,
        Some(col),
        vec![(Prop::Text, text("A song title"))],
    );
    b.node(
        NodeKind::Text,
        Some(col),
        vec![(Prop::Text, text("An artist"))],
    );
    Scene {
        bar,
        pill,
        popup,
        card,
    }
}

/// Median and worst of `times`.
fn stats(times: &mut [Duration]) -> (Duration, Duration) {
    times.sort();
    (times[times.len() / 2], *times.last().unwrap())
}

fn spec_size(r: &Renderer, node: NodeId, scale: f64) -> Size {
    let s = r.surface_spec(node).unwrap();
    let o = s.overhang;
    let w = s.width.unwrap() as f64 + (o.left + o.right) as f64;
    let h = s.height.unwrap() as f64 + (o.top + o.bottom) as f64;
    Size::new((w * scale).ceil() as u32, (h * scale).ceil() as u32)
}

#[test]
fn the_owners_bar_and_island_at_one_and_a_half() {
    let rounds: usize = std::env::var("STRAND_BENCH_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let scale = Scale::from_f64(1.5).unwrap();
    for round in 0..rounds {
        let mut b = Builder::default();
        let n = scene(&mut b);
        let mut r = renderer();
        assert!(r.apply(b.diff).is_empty());
        r.take_surface_changes();
        r.set_surface_bounds(BAR, Some(LogicalSize::new(2560.0, 1440.0)));
        // The bar at 2560 logical px, 1.5: its box less its margins, and
        // its overhang.
        r.attach_surface(BAR, n.bar);
        let spec = r.surface_spec(n.bar).unwrap();
        let o = spec.overhang;
        let lw = 2560.0 - 8.0 + (o.left + o.right) as f64;
        let lh = 36.0 + (o.top + o.bottom) as f64;
        let size = Size::new((lw * 1.5).ceil() as u32, (lh * 1.5).ceil() as u32);
        r.configure_surface(BAR, size, scale);
        let mut bufs = [
            Buffer::new(size.w, size.h, scale),
            Buffer::new(size.w, size.h, scale),
        ];
        let t = Instant::now();
        bufs[0].paint_at(&mut r, BAR, 0, frame(0));
        let first = t.elapsed();
        let t = Instant::now();
        bufs[1].paint_at(&mut r, BAR, 0, frame(1));
        let full = t.elapsed();

        // The pill moves three workspaces over, 200 ms out_back, painted
        // into two buffers in turn (age 2).
        let mut d = SceneDiff::new();
        d.push(SceneOp::SetProp {
            id: n.pill,
            prop: Prop::X,
            value: num(8.0 + 3.0 * 36.0),
            transition: Transition::Duration {
                duration: Duration::from_millis(200),
                easing: Easing::named("out_back").unwrap(),
            },
        });
        assert!(r.apply(d).is_empty());
        let mut times = Vec::new();
        let mut area = 0u64;
        let mut k = 2;
        while r.wants_frame(BAR) {
            let t = Instant::now();
            let dmg = bufs[(k % 2) as usize].paint_at(&mut r, BAR, 2, frame(k));
            times.push(t.elapsed());
            area = area.max(dmg.area());
            k += 1;
            assert!(k < 200, "the pill never settled");
        }
        let frames = times.len();
        let (median, worst) = stats(&mut times);
        eprintln!(
            "round {round}: bar {}x{} at 1.5: first paint {first:?}, full repaint {full:?}; \
             pill move: {frames} frames ({:?}), median {median:?}, worst {worst:?}, \
             largest damage {area} px ({:.1}% of the buffer)",
            size.w,
            size.h,
            Duration::from_micros(16_667 * frames as u64),
            100.0 * area as f64 / (size.w as f64 * size.h as f64),
        );
        // A moving pill repaints only around itself, never the bar.
        assert!(
            area * 20 < size.w as u64 * size.h as u64,
            "the pill's frames damage {area} px of {}",
            size.w * size.h
        );
        assert_eq!(frames, 12, "200 ms at 60 Hz");

        // The same move back after the bar sat idle long enough for its
        // cached shadow to be freed (`IDLE_FREE`, 30 s; shortened here).
        r.set_paint_cache_idle(Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(5));
        r.update();
        r.set_paint_cache_idle(Duration::from_secs(30));
        let mut d = SceneDiff::new();
        d.push(SceneOp::SetProp {
            id: n.pill,
            prop: Prop::X,
            value: num(8.0),
            transition: Transition::Duration {
                duration: Duration::from_millis(200),
                easing: Easing::named("out_back").unwrap(),
            },
        });
        assert!(r.apply(d).is_empty());
        let mut times = Vec::new();
        while r.wants_frame(BAR) {
            let t = Instant::now();
            bufs[(k % 2) as usize].paint_at(&mut r, BAR, 2, frame(k));
            times.push(t.elapsed());
            k += 1;
        }
        let first_after_idle = times[0];
        let (median, worst) = stats(&mut times);
        eprintln!(
            "round {round}: pill move after idle: first frame {first_after_idle:?}, \
             median {median:?}, worst {worst:?}"
        );

        // The island as the owner wrote it: `enter` on the card inside an
        // always-mounted popup. The card mounted at boot, so opening the
        // popup plays nothing (spec.md 6.6: `enter` plays on mount).
        let (frames, ..) = open_island(&mut r, &n, &mut k, scale, round, "card's enter");
        assert_eq!(frames, 1, "an always-mounted card does not enter again");
        close_island(&mut r, &n, &mut k);
        // The pose on the popup (a surface's `enter` plays as it opens).
        let mut d = SceneDiff::new();
        d.set(n.card, Prop::Enter, PropValue::Pose(vec![])).set(
            n.popup,
            Prop::Enter,
            PropValue::Pose(vec![(Prop::Opacity, num(0.0)), (Prop::Scale, num(0.9))]),
        );
        assert!(r.apply(d).is_empty());
        let (frames, near) = open_island(&mut r, &n, &mut k, scale, round, "popup's enter");
        assert!(frames > 4, "the popup plays its enter: {frames} frames");
        assert!(near > 2, "and reaches rest over several: {near}");
        close_island(&mut r, &n, &mut k);
    }
}

/// Opens the island, paints it until it rests and prints its frames;
/// returns how many it painted and after how many its centre pixel was
/// within 3/255 of rest.
fn open_island(
    r: &mut Renderer,
    n: &Scene,
    k: &mut u32,
    scale: Scale,
    round: usize,
    what: &str,
) -> (usize, usize) {
    let mut d = SceneDiff::new();
    d.set(n.popup, Prop::Open, PropValue::Bool(true));
    assert!(r.apply(d).is_empty());
    r.take_surface_changes();
    r.attach_surface(POP, n.popup);
    let psize = spec_size(r, n.popup, 1.5);
    r.configure_surface(POP, psize, scale);
    let mut pbufs = [
        Buffer::new(psize.w, psize.h, scale),
        Buffer::new(psize.w, psize.h, scale),
    ];
    let start = *k;
    let mut times = Vec::new();
    let mut alpha = Vec::new();
    while r.wants_frame(POP) {
        let age = if *k == start { 0 } else { 2 };
        let buf = &mut pbufs[(*k % 2) as usize];
        let t = Instant::now();
        buf.paint_at(r, POP, age, frame(*k));
        times.push(t.elapsed());
        alpha.push(buf.px(psize.w / 2, psize.h / 2)[3]);
        *k += 1;
        assert!(*k - start < 200, "the island never settled");
    }
    let frames = times.len();
    let (median, worst) = stats(&mut times);
    let at_rest = *alpha.last().unwrap_or(&0);
    let near = alpha
        .iter()
        .position(|a| (*a as i32 - at_rest as i32).abs() <= 3)
        .unwrap_or(frames);
    eprintln!(
        "round {round}: island {}x{} at 1.5, {what}: {frames} frames ({:?}), \
         centre within 3/255 of rest after {near} ({:?}); median {median:?}, worst {worst:?}",
        psize.w,
        psize.h,
        Duration::from_micros(16_667 * frames as u64),
        Duration::from_micros(16_667 * near as u64),
    );
    (frames, near)
}

/// Closes the island and lets its surface go.
fn close_island(r: &mut Renderer, n: &Scene, k: &mut u32) {
    let mut d = SceneDiff::new();
    d.set(n.popup, Prop::Open, PropValue::Bool(false));
    assert!(r.apply(d).is_empty());
    let mut buf = Buffer::new(8, 8, Scale::ONE);
    while r.wants_frame(POP) {
        buf.paint_at(r, POP, 0, frame(*k));
        *k += 1;
    }
    r.take_surface_changes();
    r.detach_surface(POP);
}
