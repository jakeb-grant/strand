//! Damage tests: exact per-node damage, buffer-age widening, and that
//! repainting only the damage gives the same pixels as a full repaint.

mod common;

use common::*;
use strand_render::Renderer;
use strand_scene::*;

const BAR: SurfaceId = SurfaceId(1);

/// A 2560×36 bar: workspace dots, a title, a clock in the middle and a
/// status text on the right.
fn bar(clock: &str) -> (SceneDiff, NodeId) {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Bar,
        None,
        vec![
            (Prop::Bg, PropValue::Color(hex("#1e1e2e").with_alpha(0.9))),
            (Prop::Radius, num(10.0)),
            (Prop::Color, color("#cdd6f4")),
            (Prop::Font, PropValue::Font(font(13.0))),
        ],
    );
    for i in 0..5 {
        b.node(
            NodeKind::Box,
            Some(root),
            vec![
                (Prop::X, num(12.0 + 14.0 * i as f32)),
                (Prop::Y, num(14.0)),
                (Prop::Size, num(8.0)),
                (Prop::Radius, num(999.0)),
                (Prop::Bg, color("#89b4fa")),
            ],
        );
    }
    b.node(
        NodeKind::Text,
        Some(root),
        vec![
            (Prop::X, num(100.0)),
            (Prop::Y, num(10.0)),
            (Prop::Text, text("~/src/strand — nvim")),
        ],
    );
    let clock_id = b.node(
        NodeKind::Text,
        Some(root),
        vec![
            (Prop::X, num(1262.0)),
            (Prop::Y, num(10.0)),
            (Prop::Text, text(clock)),
        ],
    );
    b.node(
        NodeKind::Text,
        Some(root),
        vec![
            (Prop::X, num(2480.0)),
            (Prop::Y, num(10.0)),
            (Prop::Text, text("87%")),
        ],
    );
    (b.diff, clock_id)
}

fn set_text(id: NodeId, s: &str) -> SceneDiff {
    let mut d = SceneDiff::new();
    d.set(id, Prop::Text, text(s));
    d
}

fn fresh(diff: SceneDiff, w: u32, h: u32, scale: Scale) -> (Renderer, Buffer) {
    let mut r = renderer();
    assert!(r.apply(diff).is_empty());
    let root = r.tree().roots()[0];
    r.attach_surface(BAR, root);
    let mut buf = Buffer::new(w, h, scale);
    buf.paint(&mut r, BAR, 0);
    (r, buf)
}

#[test]
fn clock_tick_damage_is_small_and_exact() {
    let (diff, clock) = bar("12:59");
    let (mut r, mut buf) = fresh(diff, 2560, 36, Scale::ONE);
    assert!(!r.wants_frame(BAR), "nothing to do after the first frame");

    assert!(r.apply(set_text(clock, "13:00")).is_empty());
    assert!(r.wants_frame(BAR));
    let d = buf.paint(&mut r, BAR, 1);
    assert!(!r.wants_frame(BAR));

    // The M0 exit criterion: ≤2,000 px² per clock tick.
    assert!(
        d.area() > 0 && d.area() <= 2000,
        "damage {d:?} area {}",
        d.area()
    );
    let b = d.bounds().unwrap();
    assert!(
        Rect::new(1255, 0, 60, 36).contains_rect(b),
        "damage stays on the clock: {b:?}"
    );

    // Repainting only the damage equals a full repaint of the new scene.
    let (_, full) = fresh(bar("13:00").0, 2560, 36, Scale::ONE);
    assert!(
        buf.pixels == full.pixels,
        "partial repaint differs from full repaint"
    );

    // No change, no damage.
    let none = buf.paint(&mut r, BAR, 1);
    assert!(none.is_empty(), "{none:?}");
}

#[test]
fn clock_tick_at_fractional_scale_matches_full_repaint() {
    let s = Scale::new(150).unwrap();
    let size = s.physical_size(LogicalSize::new(2048.0, 36.0));
    let (diff, clock) = bar("12:59");
    let (mut r, mut buf) = fresh(diff, size.w, size.h, s);
    r.apply(set_text(clock, "13:00"));
    let d = buf.paint(&mut r, BAR, 1);
    // 1.25² × the 1× budget.
    assert!(d.area() <= 3125, "{}", d.area());
    let (_, full) = fresh(bar("13:00").0, size.w, size.h, s);
    assert!(buf.pixels == full.pixels);
}

#[test]
fn buffer_age_widens_damage() {
    let (diff, clock) = bar("12:59");
    let mut r = renderer();
    r.apply(diff);
    r.attach_surface(BAR, r.tree().roots()[0]);
    // Triple buffering: buffers are reused in rotation, so each is 3 frames
    // old when it comes back.
    let mut bufs: Vec<Buffer> = (0..3).map(|_| Buffer::new(2560, 36, Scale::ONE)).collect();
    let mut ages = [0u8; 3];
    let times = [
        "12:59", "13:00", "13:01", "13:02", "13:03", "13:04", "13:05",
    ];
    for (frame, t) in times.iter().enumerate() {
        r.apply(set_text(clock, t));
        let i = frame % 3;
        let d = bufs[i].paint(&mut r, BAR, ages[i]);
        for (j, a) in ages.iter_mut().enumerate() {
            if j == i {
                *a = 1;
            } else if *a > 0 {
                *a += 1;
            }
        }
        if frame >= 3 {
            assert!(d.area() <= 3 * 2000, "{d:?}");
            assert!(d.area() < 2560 * 36);
        }
        let (_, full) = fresh(bar(t).0, 2560, 36, Scale::ONE);
        assert!(
            bufs[i].pixels == full.pixels,
            "frame {frame} (buffer {i}) differs"
        );
    }
}

#[test]
fn age_zero_or_too_old_repaints_everything() {
    let (diff, clock) = bar("12:59");
    let (mut r, mut buf) = fresh(diff, 2560, 36, Scale::ONE);
    r.apply(set_text(clock, "13:00"));
    let d = buf.paint(&mut r, BAR, 0);
    assert_eq!(d.rects(), &[Rect::new(0, 0, 2560, 36)]);
    r.apply(set_text(clock, "13:01"));
    let d = buf.paint(&mut r, BAR, 9);
    assert_eq!(d.area(), 2560 * 36);
}

#[test]
fn resize_and_rescale_repaint_everything() {
    let (diff, _) = bar("12:59");
    let (mut r, _) = fresh(diff, 2560, 36, Scale::ONE);
    let mut bigger = Buffer::new(2560, 40, Scale::ONE);
    assert_eq!(bigger.paint(&mut r, BAR, 1).area(), 2560 * 40);
    let mut scaled = Buffer::new(2560, 54, Scale::new(180).unwrap());
    assert_eq!(scaled.paint(&mut r, BAR, 1).area(), 2560 * 54);
}

#[test]
fn structural_changes_damage_old_and_new_places() {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#000000"))]);
    let a = b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(10.0)),
            (Prop::Y, num(2.0)),
            (Prop::Size, num(10.0)),
            (Prop::Bg, color("#ff0000")),
        ],
    );
    let c = b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(15.0)),
            (Prop::Y, num(2.0)),
            (Prop::Size, num(10.0)),
            (Prop::Bg, color("#00ff00")),
        ],
    );
    let (mut r, mut buf) = fresh(b.diff, 200, 20, Scale::ONE);

    // Moving a box damages where it was and where it is.
    let mut d = SceneDiff::new();
    d.set(a, Prop::X, num(100.0));
    r.apply(d);
    let dmg = buf.paint(&mut r, BAR, 1);
    assert_eq!(dmg.area(), 200);
    assert!(dmg.covers(Rect::new(10, 2, 10, 10)) && dmg.covers(Rect::new(100, 2, 10, 10)));

    // Reordering repaints the moved node even though no prop changed.
    let mut d = SceneDiff::new();
    d.push(SceneOp::Move {
        id: c,
        parent: Some(root),
        index: 0,
    });
    r.apply(d);
    let dmg = buf.paint(&mut r, BAR, 1);
    assert_eq!(dmg.area(), 100);

    // Removing damages the old place only.
    let mut d = SceneDiff::new();
    d.push(SceneOp::Remove { id: c });
    r.apply(d);
    let dmg = buf.paint(&mut r, BAR, 1);
    assert_eq!(dmg.rects(), &[Rect::new(15, 2, 10, 10)]);
    assert_eq!(buf.px(20, 5), [0, 0, 0, 255]);
}

#[test]
fn parent_opacity_change_repaints_overflowing_children() {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![]);
    let parent = b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(0.0)),
            (Prop::Size, num(10.0)),
            (Prop::Bg, color("#ffffff")),
        ],
    );
    b.node(
        NodeKind::Box,
        Some(parent),
        vec![
            (Prop::X, num(50.0)),
            (Prop::Size, num(10.0)),
            (Prop::Bg, color("#ffffff")),
        ],
    );
    let (mut r, mut buf) = fresh(b.diff, 100, 10, Scale::ONE);
    let mut d = SceneDiff::new();
    d.set(parent, Prop::Opacity, num(0.5));
    r.apply(d);
    let dmg = buf.paint(&mut r, BAR, 1);
    assert!(dmg.covers(Rect::new(50, 0, 10, 10)), "{dmg:?}");
    assert_eq!(buf.px(55, 5)[3], 128);
}

/// Random edits to a small scene, painted incrementally into rotating
/// buffers, always match a from-scratch render.
#[test]
fn random_edits_match_full_repaint() {
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut rnd = move |n: u32| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n as u64) as u32
    };
    let palette = ["#f38ba8", "#a6e3a1", "#89b4fa", "#f9e2af", "#00000000"];
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Bar,
        None,
        vec![
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Font, PropValue::Font(font(12.0))),
            (Prop::Color, color("#ffffff")),
        ],
    );
    // A clipping container with an overflowing child: clip paths must also
    // combine with damage clips exactly.
    let clip = b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(150.0)),
            (Prop::Y, num(2.0)),
            (Prop::Width, num(40.0)),
            (Prop::Height, num(20.0)),
            (Prop::Radius, num(6.0)),
            (Prop::Clip, PropValue::Bool(true)),
            (Prop::Bg, color("#45475a")),
        ],
    );
    let mut boxes = vec![b.node(
        NodeKind::Box,
        Some(clip),
        vec![
            (Prop::X, num(30.0)),
            (Prop::Y, num(8.0)),
            (Prop::Size, num(16.0)),
            (Prop::Radius, num(4.0)),
            (Prop::Bg, color("#fab387")),
        ],
    )];
    for i in 0..6 {
        boxes.push(b.node(
            NodeKind::Box,
            Some(root),
            vec![
                (Prop::X, num(20.0 * i as f32)),
                (Prop::Y, num(4.0)),
                (Prop::Size, num(16.0)),
                (Prop::Bg, color(palette[i % 4])),
            ],
        ));
    }
    let label = b.node(
        NodeKind::Text,
        Some(root),
        vec![
            (Prop::X, num(130.0)),
            (Prop::Y, num(4.0)),
            (Prop::Text, text("0")),
        ],
    );
    let mut scene = b.diff.clone();
    let (mut r, first) = fresh(b.diff, 200, 24, Scale::ONE);
    // Three buffers handed back in arbitrary order, like a compositor
    // releasing them out of order; age = frames since a buffer was painted.
    let mut bufs = [
        first,
        Buffer::new(200, 24, Scale::ONE),
        Buffer::new(200, 24, Scale::ONE),
    ];
    let mut painted_at: [Option<usize>; 3] = [Some(0), None, None];
    let mut log: Vec<String> = Vec::new();
    for frame in 1..200 {
        let mut d = SceneDiff::new();
        for _ in 0..1 + rnd(3) {
            let id = boxes[rnd(boxes.len() as u32) as usize];
            match rnd(7) {
                0 => d.set(id, Prop::X, num(rnd(180) as f32)),
                1 => d.set(id, Prop::Bg, color(palette[rnd(5) as usize])),
                2 => d.set(id, Prop::Radius, num(rnd(9) as f32)),
                3 => d.set(id, Prop::Opacity, num(rnd(4) as f32 / 3.0)),
                4 => d.set(label, Prop::Color, color(palette[rnd(4) as usize])),
                5 if id != boxes[0] => d.push(SceneOp::Move {
                    id,
                    parent: Some(root),
                    index: rnd(8),
                }),
                _ => d.set(label, Prop::Text, text(&rnd(1000).to_string())),
            };
        }
        scene.ops.extend(d.ops.iter().cloned());
        log.push(format!("{d:?}"));
        r.apply(d);
        let i = rnd(3) as usize;
        let age = painted_at[i].map_or(0, |f| (frame - f).min(255) as u8);
        let dmg = bufs[i].paint(&mut r, BAR, age);
        painted_at[i] = Some(frame);
        log.push(format!("frame {frame} buf {i} age {age} damage {dmg:?}"));
        let (_, full) = fresh(scene.clone(), 200, 24, Scale::ONE);
        if let Some(p) = bufs[i]
            .pixels
            .iter()
            .zip(&full.pixels)
            .position(|(a, b)| a != b)
        {
            let px = p / 4;
            for l in &log[log.len().saturating_sub(10)..] {
                eprintln!("{l}");
            }
            panic!(
                "frame {frame} differs at ({}, {}): {:?} vs {:?}",
                px % 200,
                px / 200,
                bufs[i].px(px as u32 % 200, px as u32 / 200),
                full.px(px as u32 % 200, px as u32 / 200),
            );
        }
    }
}

/// With the threaded text worker the renderer keeps drawing the last layout
/// until the new one arrives, asks for frames meanwhile, and then repaints
/// only the text.
#[test]
fn worker_backend_keeps_last_layout_until_delivery() {
    use std::sync::Arc;
    use std::time::Duration;
    use strand_render::TextBackend;
    use strand_text::{FontConfig, TextWorker, test_font_path};

    let data = std::fs::read(test_font_path()).unwrap();
    let worker = TextWorker::spawn(FontConfig::isolated(vec![Arc::new(data)])).unwrap();
    let mut r = Renderer::new(TextBackend::Worker(worker));
    let (diff, clock) = bar("12:59");
    r.apply(diff);
    r.attach_surface(BAR, r.tree().roots()[0]);
    r.configure_surface(BAR, Size::new(2560, 36), Scale::ONE);
    assert!(r.wait_for_text(Duration::from_secs(10)));
    let mut buf = Buffer::new(2560, 36, Scale::ONE);
    buf.paint(&mut r, BAR, 0);
    let (_, inline) = fresh(bar("12:59").0, 2560, 36, Scale::ONE);
    assert!(
        buf.pixels == inline.pixels,
        "worker and inline shaping agree"
    );

    r.apply(set_text(clock, "13:00"));
    assert!(r.text_pending());
    assert!(r.wants_frame(BAR));
    assert!(r.wait_for_text(Duration::from_secs(10)));
    let d = buf.paint(&mut r, BAR, 1);
    assert!(d.area() > 0 && d.area() <= 2000, "{d:?}");
    let (_, full) = fresh(bar("13:00").0, 2560, 36, Scale::ONE);
    assert!(buf.pixels == full.pixels);
    assert!(!r.wants_frame(BAR));
}
