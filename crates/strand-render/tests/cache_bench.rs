//! Not gates: what the render caches save on large surfaces, printed for
//! A/B builds (decisions.md, m5-speed-caches). Run with
//! `cargo test --profile timing -p strand-render --test cache_bench --
//! --ignored --nocapture --test-threads 1`.
//!
//! - `launcher_open_cost`: the path users feel. A launcher opened more
//!   than `IDLE_FREE` after it last closed (nothing cached), as a
//!   compositor with `wp_alpha_modifier_v1` and viewporter shows it: one
//!   whole first frame, then typing into the filter, each keystroke
//!   damaging only the result rows (design.md: surface-level fades and
//!   scales cost no repaint). Prints the first frame and the mean typing
//!   frame; then the same launcher reopened within `IDLE_FREE`, several
//!   times, with the same frames.
//! - `large_gradient_frame_cost`: a launcher at 2× (600 × 460 logical
//!   with its shadow's overhang, 1332 × 1116 px) with a gradient card,
//!   rounded, with a large shadow, repainted whole every frame: a fade or
//!   scale of the whole surface on a compositor without those protocols
//!   (the fallback), a resize, an `age 0` buffer. The paint cache's
//!   per-entry limit decides whether the gradient is dithered from a
//!   pixmap or cell by cell, and the shadow drawn from its pixmap or
//!   directly.
//! - `large_blur_group_frame_cost`: a blurred group of 700 × 500 at 2×
//!   (1400 × 1000 px) repainted whole every frame: the offscreen budget
//!   decides whether it is filtered once or every frame.
//! - `resizing_shadow_frame_cost` (shadow-slice): the owner's launcher
//!   card at 1.5× (600 × 420 logical, corners 18, 18, 32, 32, a 0 16px
//!   48px shadow) resized every frame as a size spring does, each frame
//!   repainting what changed; then with its bottom radius springing too,
//!   without its shadow, and still but repainted whole, with and without
//!   its shadow (what drawing the shadow from a cached pixmap costs).
//!   Prints the mean, median and 95th percentile frame.
//! - `reopen_first_frame_cost`: the launcher closed while a bar paints
//!   on, then opened again: its first three frames' cost, each repainted
//!   whole (the fallback), with what the caches kept from its last
//!   showing.

mod common;

use std::time::{Duration, Instant};

use common::*;
use strand_render::Renderer;
use strand_scene::*;

const LAUNCHER: SurfaceId = SurfaceId(1);
const BAR: SurfaceId = SurfaceId(2);

fn frames() -> u32 {
    std::env::var("STRAND_CACHE_BENCH_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200)
}

fn kw(k: &str) -> PropValue {
    PropValue::Keyword(k.into())
}

fn shadow() -> PropValue {
    PropValue::Shadow(vec![Shadow {
        x: 0.0,
        y: 16.0,
        blur: 48.0,
        spread: 0.0,
        color: Color::BLACK.with_alpha(0.4),
    }])
}

/// A launcher-like panel: a gradient card with a large shadow, a search
/// row and a few rows of text.
fn launcher(b: &mut Builder, w: f32, h: f32) -> NodeId {
    launcher_rows(b, w, h).0
}

/// [`launcher`], and its result rows.
fn launcher_rows(b: &mut Builder, w: f32, h: f32) -> (NodeId, Vec<NodeId>) {
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(w)),
            (Prop::Height, num(h)),
            (Prop::Pad, num(40.0)),
        ],
    );
    let card = b.node(
        NodeKind::Col,
        Some(root),
        vec![
            (Prop::Width, num(w - 80.0)),
            (Prop::Height, num(h - 80.0)),
            (Prop::Radius, num(18.0)),
            (Prop::Shadow, shadow()),
            (
                Prop::Bg,
                PropValue::Paint(Paint::Linear {
                    angle: 160.0,
                    stops: vec![
                        GradientStop {
                            offset: 0.0,
                            color: hex("#1e1e2e"),
                        },
                        GradientStop {
                            offset: 1.0,
                            color: hex("#313244"),
                        },
                    ],
                }),
            ),
            (Prop::Pad, num(16.0)),
            (Prop::Gap, num(8.0)),
        ],
    );
    let rows = (0..8)
        .map(|i| {
            b.node(
                NodeKind::Text,
                Some(card),
                vec![
                    (Prop::Text, text(&format!("Application number {i}"))),
                    (Prop::Color, color("#cdd6f4")),
                    (Prop::Font, PropValue::Font(font(15.0))),
                ],
            )
        })
        .collect();
    (root, rows)
}

fn show(r: &mut Renderer, s: SurfaceId, root: NodeId, w: u32, h: u32, scale: Scale) -> Buffer {
    let spec = r.surface_spec(root).unwrap().clone();
    let size = scale.physical_size(LogicalSize::new(
        spec.width.unwrap_or(w as f32) + spec.overhang.left + spec.overhang.right,
        spec.height.unwrap_or(h as f32) + spec.overhang.top + spec.overhang.bottom,
    ));
    r.attach_surface(s, root);
    r.configure_surface(s, size, scale);
    Buffer::new(size.w, size.h, scale)
}

fn per_frame(r: &mut Renderer, buf: &mut Buffer, s: SurfaceId) -> f64 {
    for _ in 0..10 {
        buf.paint(r, s, 0);
    }
    let n = frames();
    let start = Instant::now();
    for _ in 0..n {
        buf.paint(r, s, 0);
    }
    start.elapsed().as_secs_f64() * 1e6 / f64::from(n)
}

#[test]
#[ignore = "measurement, not a gate"]
fn large_gradient_frame_cost() {
    let mut r = renderer();
    let mut b = Builder::default();
    let root = launcher(&mut b, 600.0, 460.0);
    assert!(r.apply(b.diff).is_empty());
    let mut buf = show(
        &mut r,
        LAUNCHER,
        root,
        600,
        460,
        Scale::from_integer(2).unwrap(),
    );
    let us = per_frame(&mut r, &mut buf, LAUNCHER);
    println!(
        "large_gradient_frame_cost: {us:.1} us/frame (1332x1116 px, paint cache {} kB held)",
        r.paint_cache().0 / 1024
    );
}

#[test]
#[ignore = "measurement, not a gate"]
fn large_blur_group_frame_cost() {
    let mut r = renderer();
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(760.0)),
            (Prop::Height, num(560.0)),
            (Prop::Bg, color("#1e1e2e")),
        ],
    );
    let group = b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::Place, kw("absolute")),
            (Prop::X, num(30.0)),
            (Prop::Y, num(30.0)),
            (Prop::Width, num(700.0)),
            (Prop::Height, num(500.0)),
            (Prop::Bg, color("#313244")),
            (Prop::Radius, num(24.0)),
            (
                Prop::Filter,
                PropValue::Call {
                    name: "blur".into(),
                    args: vec![num(6.0)],
                },
            ),
        ],
    );
    for (i, c) in ["#f38ba8", "#a6e3a1", "#89b4fa", "#f9e2af"]
        .iter()
        .enumerate()
    {
        b.node(
            NodeKind::Box,
            Some(group),
            vec![
                (Prop::Place, kw("absolute")),
                (Prop::X, num(40.0 + 150.0 * i as f32)),
                (Prop::Y, num(60.0 + 80.0 * i as f32)),
                (Prop::Width, num(140.0)),
                (Prop::Height, num(140.0)),
                (Prop::Radius, num(20.0)),
                (Prop::Bg, color(c)),
            ],
        );
    }
    assert!(r.apply(b.diff).is_empty());
    let mut buf = show(
        &mut r,
        LAUNCHER,
        root,
        760,
        560,
        Scale::from_integer(2).unwrap(),
    );
    let us = per_frame(&mut r, &mut buf, LAUNCHER);
    println!(
        "large_blur_group_frame_cost: {us:.1} us/frame (1400x1000 px group, offscreen {} kB held, {} groups drawn)",
        r.offscreen_cache().0 / 1024,
        r.offscreen_cache().1
    );
}

#[test]
#[ignore = "measurement, not a gate"]
fn resizing_shadow_frame_cost() {
    let scale = Scale::new(180).unwrap();
    let radii = |bottom: f32| PropValue::List(vec![num(18.0), num(18.0), num(bottom), num(bottom)]);
    // (size springs, radius springs, shadowed)
    for (resizes, radius_springs, shadowed) in [
        (true, false, true),
        (true, true, true),
        (true, false, false),
        (false, false, true),
        (false, false, false),
    ] {
        let mut r = renderer();
        let mut b = Builder::default();
        let root = b.node(
            NodeKind::Panel,
            None,
            vec![
                (Prop::Width, num(900.0)),
                (Prop::Height, num(640.0)),
                (Prop::Bg, color("#1e1e2e")),
            ],
        );
        let card = b.node(
            NodeKind::Box,
            Some(root),
            vec![
                (Prop::Place, kw("absolute")),
                (Prop::X, num(40.0)),
                (Prop::Y, num(30.0)),
                (Prop::Width, num(600.0)),
                (Prop::Height, num(420.0)),
                (Prop::Bg, color("#313244")),
                (Prop::Radius, radii(32.0)),
                (
                    Prop::Shadow,
                    if shadowed {
                        shadow()
                    } else {
                        PropValue::Shadow(Vec::new())
                    },
                ),
            ],
        );
        assert!(r.apply(b.diff).is_empty());
        let mut buf = show(&mut r, LAUNCHER, root, 900, 640, scale);
        buf.paint(&mut r, LAUNCHER, 0);
        let n = frames();
        let mut times = Vec::with_capacity(n as usize);
        for i in 0..n {
            // A triangle wave of sizes, as a spring swings past its rest.
            let k = (i % 60) as f32;
            let k = if k < 30.0 { k } else { 60.0 - k };
            let mut sets = Vec::new();
            if resizes {
                sets.push((Prop::Width, num(560.0 + 4.0 * k)));
                sets.push((Prop::Height, num(380.0 + 2.0 * k)));
            }
            if radius_springs {
                sets.push((Prop::Radius, radii(32.0 + 0.37 * k)));
            }
            let mut d = SceneDiff::new();
            for (prop, value) in sets {
                d.push(SceneOp::SetProp {
                    id: card,
                    prop,
                    value,
                    transition: Transition::Instant,
                });
            }
            assert!(r.apply(d).is_empty());
            let start = Instant::now();
            buf.paint(&mut r, LAUNCHER, if resizes { 1 } else { 0 });
            times.push(start.elapsed().as_secs_f64() * 1e3);
        }
        let mean = times.iter().sum::<f64>() / times.len() as f64;
        times.sort_by(f64::total_cmp);
        let at = |q: f64| times[((times.len() - 1) as f64 * q).round() as usize];
        println!(
            "resizing_shadow_frame_cost ({}): mean {mean:.2} ms, p50 {:.2} ms, p95 {:.2} ms over {n} frames (paint cache {} kB held, {} built)",
            match (resizes, radius_springs, shadowed) {
                (false, _, true) => "still, repainted whole",
                (false, _, false) => "still, repainted whole, no shadow",
                (_, _, false) => "size, no shadow",
                (_, false, _) => "size",
                (_, true, _) => "size and radius",
            },
            at(0.5),
            at(0.95),
            r.paint_cache().0 / 1024,
            r.paint_cache().1
        );
    }
}

#[test]
#[ignore = "measurement, not a gate"]
fn reopen_first_frame_cost() {
    let mut r = renderer();
    let mut b = Builder::default();
    let root = launcher(&mut b, 600.0, 460.0);
    let bar = b.node(
        NodeKind::Bar,
        None,
        vec![(Prop::Bg, color("#11111b")), (Prop::Height, num(30.0))],
    );
    let label = b.node(
        NodeKind::Text,
        Some(bar),
        vec![
            (Prop::Text, text("12:00")),
            (Prop::Color, color("#cdd6f4")),
            (Prop::Font, PropValue::Font(font(14.0))),
        ],
    );
    assert!(r.apply(b.diff).is_empty());
    let mut bar_buf = show(&mut r, BAR, bar, 800, 30, Scale::ONE);
    bar_buf.paint(&mut r, BAR, 0);
    let rounds = frames().min(100);
    let (mut first, mut second, mut warm) = (0.0, 0.0, 0.0);
    for i in 0..rounds {
        let mut buf = show(
            &mut r,
            LAUNCHER,
            root,
            600,
            460,
            Scale::from_integer(2).unwrap(),
        );
        let start = Instant::now();
        buf.paint(&mut r, LAUNCHER, 0);
        first += start.elapsed().as_secs_f64();
        let start = Instant::now();
        buf.paint(&mut r, LAUNCHER, 0);
        second += start.elapsed().as_secs_f64();
        let start = Instant::now();
        buf.paint(&mut r, LAUNCHER, 0);
        warm += start.elapsed().as_secs_f64();
        r.detach_surface(LAUNCHER);
        // The bar ticks on (its loop stops after each paint).
        let mut d = SceneDiff::new();
        d.set(label, Prop::Text, text(&format!("12:{:02}", i % 60)));
        r.apply(d);
        bar_buf.paint(&mut r, BAR, 1);
        std::thread::sleep(Duration::from_millis(2));
    }
    let n = f64::from(rounds);
    println!(
        "reopen_first_frame_cost: first frame {:.1} us, second {:.1} us, third {:.1} us (1332x1116 px)",
        first * 1e6 / n,
        second * 1e6 / n,
        warm * 1e6 / n
    );
}

/// The idle time while the launcher shows: `IDLE_FREE` (30 s on this
/// branch, 10 s before; either is far longer than a showing here).
fn cache_idle() -> Duration {
    Duration::from_secs(10)
}

/// One showing of the launcher: its whole first frame, then `keys`
/// keystrokes, each changing the result rows' text (only they repaint).
/// Returns the first frame's time and the typing frames' total, in s.
fn showing(
    r: &mut Renderer,
    root: NodeId,
    rows: &[NodeId],
    keys: usize,
    round: usize,
) -> (f64, f64) {
    let mut buf = show(r, LAUNCHER, root, 600, 460, Scale::from_integer(2).unwrap());
    let start = Instant::now();
    buf.paint(r, LAUNCHER, 0);
    let first = start.elapsed().as_secs_f64();
    let mut typing = 0.0;
    if std::env::var_os("STRAND_CACHE_BENCH_TRACE").is_some() {
        eprintln!(
            "  first {:.1} us, builds {}",
            first * 1e6,
            r.paint_cache().1
        );
    }
    for k in 0..keys {
        let mut d = SceneDiff::new();
        for (i, row) in rows.iter().enumerate() {
            let t = format!("Result {i} for query {}{}", "q".repeat(k + 1), round % 7);
            d.set(*row, Prop::Text, text(&t));
        }
        r.apply(d);
        let start = Instant::now();
        buf.paint(r, LAUNCHER, 1);
        let t = start.elapsed().as_secs_f64();
        typing += t;
        if std::env::var_os("STRAND_CACHE_BENCH_TRACE").is_some() {
            eprintln!("  key {k}: {:.1} us, builds {}", t * 1e6, r.paint_cache().1);
        }
    }
    r.detach_surface(LAUNCHER);
    (first, typing)
}

#[test]
#[ignore = "measurement, not a gate"]
fn launcher_open_cost() {
    const KEYS: usize = 8;
    let mut r = renderer();
    let mut b = Builder::default();
    let (root, rows) = launcher_rows(&mut b, 600.0, 460.0);
    let bar = b.node(
        NodeKind::Bar,
        None,
        vec![(Prop::Bg, color("#11111b")), (Prop::Height, num(30.0))],
    );
    let label = b.node(
        NodeKind::Text,
        Some(bar),
        vec![
            (Prop::Text, text("12:00")),
            (Prop::Color, color("#cdd6f4")),
            (Prop::Font, PropValue::Font(font(14.0))),
        ],
    );
    assert!(r.apply(b.diff).is_empty());
    let mut bar_buf = show(&mut r, BAR, bar, 800, 30, Scale::ONE);
    bar_buf.paint(&mut r, BAR, 0);
    let rounds = frames().clamp(1, 40) as usize;
    // Cold: every open comes after the idle time (shortened to 5 ms
    // between showings, with a bar tick past it), so nothing of the last
    // showing is kept; while open, the idle time is the real one.
    let (mut first, mut typing) = (0.0, 0.0);
    for i in 0..rounds {
        r.set_paint_cache_idle(cache_idle());
        let (f, t) = showing(&mut r, root, &rows, KEYS, i);
        first += f;
        typing += t;
        r.set_paint_cache_idle(Duration::from_millis(5));
        std::thread::sleep(Duration::from_millis(10));
        let mut d = SceneDiff::new();
        d.set(label, Prop::Text, text(&format!("12:{:02}", i % 60)));
        r.apply(d);
        bar_buf.paint(&mut r, BAR, 1);
    }
    let n = rounds as f64;
    println!(
        "launcher_open_cost cold: first frame {:.1} us, typing frame {:.1} us, open + {KEYS} keys {:.1} us (paint cache {} kB held while open)",
        first * 1e6 / n,
        typing * 1e6 / (n * KEYS as f64),
        (first + typing) * 1e6 / n,
        r.paint_cache().0 / 1024
    );
    // Warm: reopened within the idle time, again and again.
    r.set_paint_cache_idle(cache_idle());
    let (mut first, mut typing) = (0.0, 0.0);
    // The first two reopens: the second whole frame of a large entry
    // builds it (once per `IDLE_FREE` of disuse).
    let warmup = [
        showing(&mut r, root, &rows, KEYS, 0),
        showing(&mut r, root, &rows, KEYS, 1),
    ];
    println!(
        "launcher_open_cost reopen 1: first frame {:.1} us; reopen 2: first frame {:.1} us",
        warmup[0].0 * 1e6,
        warmup[1].0 * 1e6
    );
    for i in 0..rounds {
        let (f, t) = showing(&mut r, root, &rows, KEYS, i);
        first += f;
        typing += t;
    }
    println!(
        "launcher_open_cost warm: first frame {:.1} us, typing frame {:.1} us, open + {KEYS} keys {:.1} us (paint cache {} kB held)",
        first * 1e6 / n,
        typing * 1e6 / (n * KEYS as f64),
        (first + typing) * 1e6 / n,
        r.paint_cache().0 / 1024
    );
}
