//! Not gates: what the render caches save on large surfaces, printed for
//! A/B builds (decisions.md, m5-speed-caches). Run with
//! `cargo test --profile timing -p strand-render --test cache_bench --
//! --ignored --nocapture --test-threads 1`.
//!
//! - `large_gradient_frame_cost`: a launcher at 2× (600 × 460 logical
//!   with its shadow's overhang, 1332 × 1116 px) with a gradient card,
//!   rounded, with a large shadow, repainted whole every frame (a fade or scale of the whole
//!   surface repaints it all): the paint cache's per-entry limit decides
//!   whether the gradient is dithered from a pixmap or cell by cell, and
//!   the shadow drawn from its pixmap or directly.
//! - `large_blur_group_frame_cost`: a blurred group of 700 × 500 at 2×
//!   (1400 × 1000 px) repainted whole every frame: the offscreen budget
//!   decides whether it is filtered once or every frame.
//! - `reopen_first_frame_cost`: the launcher closed while a bar paints
//!   on, then opened again: its first frame's cost, with what the caches
//!   kept from its last showing.

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
    for i in 0..8 {
        b.node(
            NodeKind::Text,
            Some(card),
            vec![
                (Prop::Text, text(&format!("Application number {i}"))),
                (Prop::Color, color("#cdd6f4")),
                (Prop::Font, PropValue::Font(font(15.0))),
            ],
        );
    }
    root
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
