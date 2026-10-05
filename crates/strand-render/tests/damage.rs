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
    // Nothing visible changes until the layout arrives, so no frame is
    // wanted yet: delivery marks the surface dirty.
    // (Nothing has polled the worker since `apply`, so it is pending.)
    assert!(!r.wants_frame(BAR), "no frames while waiting for text");
    // Painting anyway while shaping is in flight draws nothing new.
    let early = buf.paint(&mut r, BAR, 1);
    let d = if r.text_pending() {
        assert!(early.is_empty(), "{early:?}");
        assert!(!r.wants_frame(BAR), "no frames while waiting for text");
        assert!(r.wait_for_text(Duration::from_secs(10)));
        assert!(r.wants_frame(BAR), "delivery asks for a frame");
        buf.paint(&mut r, BAR, 1)
    } else {
        // The layout arrived before the paint polled for it.
        early
    };
    assert!(d.area() > 0 && d.area() <= 2000, "{d:?}");
    let (_, full) = fresh(bar("13:00").0, 2560, 36, Scale::ONE);
    assert!(buf.pixels == full.pixels);
    assert!(!r.wants_frame(BAR));
}

/// Edits only wake the surfaces whose subtree they touch.
#[test]
fn edits_only_dirty_their_own_surface() {
    let (left, clock) = bar("12:59");
    let mut r = renderer();
    r.apply(left);
    let other = NodeId::new(1000, 0);
    let mut d = SceneDiff::new();
    d.create(other, NodeKind::Bar, None, 1)
        .set(other, Prop::Bg, color("#000000"));
    r.apply(d);
    r.attach_surface(SurfaceId(1), r.tree().roots()[0]);
    r.attach_surface(SurfaceId(2), other);
    let mut a = Buffer::new(2560, 36, Scale::ONE);
    let mut c = Buffer::new(100, 36, Scale::ONE);
    a.paint(&mut r, SurfaceId(1), 0);
    c.paint(&mut r, SurfaceId(2), 0);
    assert!(!r.wants_frame(SurfaceId(1)) && !r.wants_frame(SurfaceId(2)));
    r.apply(set_text(clock, "13:00"));
    assert!(r.wants_frame(SurfaceId(1)));
    assert!(!r.wants_frame(SurfaceId(2)), "the other output stays idle");
}

/// One tree shown on outputs of different scales keeps a layout per scale
/// (no re-shaping ping-pong) and each output matches a single-output render.
#[test]
fn shared_tree_on_mixed_dpi_outputs() {
    let (diff, clock) = bar("12:59");
    let mut r = renderer();
    r.apply(diff);
    let root = r.tree().roots()[0];
    let s15 = Scale::new(180).unwrap();
    r.attach_surface(SurfaceId(1), root);
    r.attach_surface(SurfaceId(2), root);
    let mut a = Buffer::new(2560, 36, Scale::ONE);
    let mut b = Buffer::new(3840, 54, s15);
    for _ in 0..2 {
        a.paint(&mut r, SurfaceId(1), 1);
        b.paint(&mut r, SurfaceId(2), 1);
    }
    assert!(!r.wants_frame(SurfaceId(1)) && !r.wants_frame(SurfaceId(2)));
    r.apply(set_text(clock, "13:00"));
    let da = a.paint(&mut r, SurfaceId(1), 1);
    let db = b.paint(&mut r, SurfaceId(2), 1);
    assert!(
        da.area() <= 2000 && db.area() <= 2000 * 9 / 4,
        "{da:?} {db:?}"
    );
    assert!(!r.wants_frame(SurfaceId(1)) && !r.wants_frame(SurfaceId(2)));
    let (_, full_a) = fresh(bar("13:00").0, 2560, 36, Scale::ONE);
    let (_, full_b) = fresh(bar("13:00").0, 3840, 54, s15);
    assert!(a.pixels == full_a.pixels);
    assert!(b.pixels == full_b.pixels);
}

fn worker_renderer() -> Renderer {
    use std::sync::Arc;
    use strand_render::TextBackend;
    use strand_text::{FontConfig, TextWorker, test_font_path};
    let data = std::fs::read(test_font_path()).unwrap();
    let worker = TextWorker::spawn(FontConfig::isolated(vec![Arc::new(data)])).unwrap();
    Renderer::new(TextBackend::Worker(worker))
}

/// Unplugging the only output at a scale and plugging one back in must
/// bring text back: the worker's atlas for that scale is dropped together
/// with the render thread's mirror, so glyphs are uploaded again.
#[test]
fn text_survives_output_hotplug() {
    use std::time::Duration;
    for worker in [false, true] {
        let mut r = if worker {
            worker_renderer()
        } else {
            renderer()
        };
        let (diff, _) = bar("12:59");
        r.apply(diff);
        let root = r.tree().roots()[0];
        r.attach_surface(SurfaceId(1), root);
        r.configure_surface(SurfaceId(1), Size::new(2560, 36), Scale::ONE);
        assert!(r.wait_for_text(Duration::from_secs(10)));
        let mut a = Buffer::new(2560, 36, Scale::ONE);
        a.paint(&mut r, SurfaceId(1), 0);
        r.detach_surface(SurfaceId(1));

        r.attach_surface(SurfaceId(2), root);
        r.configure_surface(SurfaceId(2), Size::new(2560, 36), Scale::ONE);
        assert!(r.wait_for_text(Duration::from_secs(10)));
        let mut b = Buffer::new(2560, 36, Scale::ONE);
        b.paint(&mut r, SurfaceId(2), 0);
        let (_, want) = fresh(bar("12:59").0, 2560, 36, Scale::ONE);
        assert!(a.pixels == want.pixels, "worker: {worker}");
        assert!(
            b.pixels == want.pixels,
            "text after replug (worker: {worker})"
        );
    }
}

/// Moving a surface to an output of another scale frees the old scale's
/// text state, and coming back works.
#[test]
fn rescaling_frees_and_restores_text() {
    let (diff, _) = bar("12:59");
    let (mut r, _) = fresh(diff, 2560, 36, Scale::ONE);
    let s2 = Scale::new(240).unwrap();
    let mut b = Buffer::new(5120, 72, s2);
    b.paint(&mut r, BAR, 0);
    let (_, want_b) = fresh(bar("12:59").0, 5120, 72, s2);
    assert!(b.pixels == want_b.pixels, "2x after the move");
    let mut a = Buffer::new(2560, 36, Scale::ONE);
    a.paint(&mut r, BAR, 0);
    let (_, want) = fresh(bar("12:59").0, 2560, 36, Scale::ONE);
    assert!(a.pixels == want.pixels);
}

/// Number of pixels with any coverage inside `r`.
fn lit(buf: &Buffer, r: Rect) -> usize {
    let mut n = 0;
    for y in r.top()..r.bottom() {
        for x in r.left()..r.right() {
            if buf.px(x as u32, y as u32)[3] > 0 {
                n += 1;
            }
        }
    }
    n
}

/// With the threaded worker, the first frame after a rescale draws the
/// old scale's layouts resampled while the new ones are shaped: text never
/// blanks (startup's late `preferred_scale`, outputs of mixed scale).
#[test]
fn worker_rescale_keeps_text_on_the_first_frame() {
    use std::time::Duration;
    let mut r = worker_renderer();
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Color, color("#ffffff"))]);
    b.node(
        NodeKind::Text,
        Some(root),
        vec![
            (Prop::X, num(4.0)),
            (Prop::Y, num(2.0)),
            (Prop::Font, PropValue::Font(font(13.0))),
            (Prop::Text, text("Hello 12:59")),
        ],
    );
    r.apply(b.diff);
    r.attach_surface(BAR, root);
    r.configure_surface(BAR, Size::new(200, 20), Scale::ONE);
    assert!(r.wait_for_text(Duration::from_secs(10)));
    let mut a = Buffer::new(200, 20, Scale::ONE);
    a.paint(&mut r, BAR, 0);
    let one = lit(&a, Rect::new(0, 0, 200, 20));
    assert!(one > 50, "{one}");

    let s2 = Scale::new(240).unwrap();
    let mut b = Buffer::new(400, 40, s2);
    b.paint(&mut r, BAR, 0);
    let first = lit(&b, Rect::new(0, 0, 400, 40));
    assert!(
        first > one,
        "text on the first 2x frame: {first} lit pixels"
    );
    // Then the sharp layout arrives and replaces it.
    assert!(r.wait_for_text(Duration::from_secs(10)));
    assert!(r.wants_frame(BAR));
    b.paint(&mut r, BAR, 1);
    let mut want_r = renderer();
    let mut w = Builder::default();
    let root = w.node(NodeKind::Bar, None, vec![(Prop::Color, color("#ffffff"))]);
    w.node(
        NodeKind::Text,
        Some(root),
        vec![
            (Prop::X, num(4.0)),
            (Prop::Y, num(2.0)),
            (Prop::Font, PropValue::Font(font(13.0))),
            (Prop::Text, text("Hello 12:59")),
        ],
    );
    want_r.apply(w.diff);
    want_r.attach_surface(BAR, root);
    let mut want = Buffer::new(400, 40, s2);
    want.paint(&mut want_r, BAR, 0);
    assert!(b.pixels == want.pixels, "sharp 2x text after delivery");
}

/// The buffer-age contract: an empty paint is not a frame. A caller that
/// skips the commit on empty damage stays in step with the renderer.
#[test]
fn empty_paints_are_not_frames() {
    let (diff, clock) = bar("12:59");
    let mut r = renderer();
    r.apply(diff);
    r.attach_surface(BAR, r.tree().roots()[0]);
    let mut a = Buffer::new(2560, 36, Scale::ONE);
    let mut b = Buffer::new(2560, 36, Scale::ONE);
    let status = NodeId::new(clock.index + 1, 0);
    // Double buffering, committing only non-empty paints.
    assert!(!a.paint(&mut r, BAR, 0).is_empty()); // commit 1 (A)
    assert!(!b.paint(&mut r, BAR, 0).is_empty()); // commit 2 (B, age 0)
    assert!(a.paint(&mut r, BAR, 2).is_empty(), "A is current: no frame");
    // Commit 3: the status text changes, painted into A.
    r.apply(set_text(status, "12%"));
    assert!(!a.paint(&mut r, BAR, 2).is_empty());
    assert!(a.paint(&mut r, BAR, 1).is_empty(), "idle: no frame");
    // Commit 4: the clock ticks, painted into B, which last saw commit 2
    // and so must also repaint the status text from commit 3.
    r.apply(set_text(clock, "13:00"));
    let d = b.paint(&mut r, BAR, 2);
    assert!(d.area() <= 2 * 2000, "{d:?}");
    let mut want = bar("13:00").0;
    want.set(status, Prop::Text, text("12%"));
    let (_, want) = fresh(want, 2560, 36, Scale::ONE);
    assert!(b.pixels == want.pixels);
    assert!(r.opaque_region(BAR).is_empty(), "translucent bar");
}

/// Reverting text before the layout for the intermediate value arrives
/// must not show the intermediate value.
#[test]
fn reverted_text_does_not_flash_the_stale_layout() {
    use std::time::Duration;
    let mut r = worker_renderer();
    let (diff, clock) = bar("AAAA");
    r.apply(diff);
    r.attach_surface(BAR, r.tree().roots()[0]);
    r.configure_surface(BAR, Size::new(2560, 36), Scale::ONE);
    assert!(r.wait_for_text(Duration::from_secs(10)));
    let mut buf = Buffer::new(2560, 36, Scale::ONE);
    buf.paint(&mut r, BAR, 0);
    r.apply(set_text(clock, "WWWWWW"));
    r.apply(set_text(clock, "AAAA"));
    assert!(r.wait_for_text(Duration::from_secs(10)));
    // Let any reply for the cancelled request arrive and be discarded.
    std::thread::sleep(Duration::from_millis(50));
    r.update();
    buf.paint(&mut r, BAR, 1);
    let (_, want) = fresh(bar("AAAA").0, 2560, 36, Scale::ONE);
    assert!(buf.pixels == want.pixels);
}

/// Cost follows damage, not buffer size: a clock tick on a 4K surface
/// rasterises a few cells, and stays bit-identical to a full repaint.
#[test]
fn clock_tick_on_4k_rasterises_only_the_damage() {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Lock,
        None,
        vec![
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Color, color("#cdd6f4")),
            (Prop::Font, PropValue::Font(font(96.0))),
        ],
    );
    let clock = b.node(
        NodeKind::Text,
        Some(root),
        vec![
            (Prop::X, num(1700.0)),
            (Prop::Y, num(900.0)),
            (Prop::Text, text("12:59")),
        ],
    );
    let scene = b.diff;
    let (mut r, mut buf) = fresh(scene.clone(), 3840, 2160, Scale::ONE);
    assert_eq!(r.opaque_region(BAR).rects(), &[Rect::new(0, 0, 3840, 2160)]);
    r.apply(set_text(clock, "13:00"));
    let d = buf.paint(&mut r, BAR, 1);
    let px = r.last_raster_pixels();
    assert!(d.area() > 0 && d.area() < 40_000, "{d:?}");
    assert!(
        px <= 8 * 256 * 64,
        "rasterised {px} px for {} px of damage",
        d.area()
    );
    let mut full_r = renderer();
    full_r.apply(scene);
    full_r.apply(set_text(clock, "13:00"));
    full_r.attach_surface(BAR, full_r.tree().roots()[0]);
    let mut full = Buffer::new(3840, 2160, Scale::ONE);
    full.paint(&mut full_r, BAR, 0);
    assert!(buf.pixels == full.pixels);
}

/// Values from user expressions (`13/0`) or services must never panic the
/// render thread or produce bogus damage.
#[test]
fn non_finite_and_huge_values_are_safe() {
    let bad = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1e30, -1e30];
    let numeric = [
        Prop::X,
        Prop::Y,
        Prop::Width,
        Prop::Height,
        Prop::Size,
        Prop::Opacity,
        Prop::Radius,
        Prop::MaxWidth,
        Prop::Weight,
    ];
    for v in bad {
        let mut b = Builder::default();
        let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
        let mut props: Vec<(Prop, PropValue)> = numeric.iter().map(|p| (*p, num(v))).collect();
        props.push((Prop::Bg, color("#89b4fa")));
        props.push((
            Prop::Shadow,
            PropValue::Shadow(vec![Shadow {
                x: v,
                y: v,
                blur: v,
                spread: v,
                color: hex("#000000").with_alpha(0.5),
            }]),
        ));
        props.push((
            Prop::Border,
            PropValue::Border(Border {
                width: v,
                paint: Paint::Linear {
                    angle: v,
                    stops: vec![
                        GradientStop {
                            offset: v,
                            color: Color::new(v, 0.5, 0.5, 1.0),
                        },
                        GradientStop {
                            offset: 1.0,
                            color: Color::WHITE,
                        },
                    ],
                },
            }),
        ));
        props.push((Prop::Clip, PropValue::Bool(true)));
        let boxed = b.node(NodeKind::Box, Some(root), props);
        b.node(
            NodeKind::Text,
            Some(boxed),
            vec![
                (Prop::Text, text("12:59")),
                (Prop::Width, num(v)),
                (
                    Prop::Font,
                    PropValue::Font(Font {
                        size: v,
                        ..font(13.0)
                    }),
                ),
            ],
        );
        // A shadow on an ordinary box, one field at a time.
        for field in 0..4 {
            let mut sh = Shadow {
                x: 0.0,
                y: 2.0,
                blur: 8.0,
                spread: 0.0,
                color: hex("#000000").with_alpha(0.5),
            };
            match field {
                0 => sh.x = v,
                1 => sh.y = v,
                2 => sh.blur = v,
                _ => sh.spread = v,
            }
            b.node(
                NodeKind::Box,
                Some(root),
                vec![
                    (Prop::X, num(10.0)),
                    (Prop::Size, num(10.0)),
                    (
                        Prop::Radius,
                        PropValue::Corners(Corners {
                            top_left: v,
                            ..Corners::all(4.0)
                        }),
                    ),
                    (Prop::Shadow, PropValue::Shadow(vec![sh])),
                ],
            );
        }
        let (mut r, mut buf) = fresh(b.diff, 400, 36, Scale::ONE);
        let d = buf.paint(&mut r, BAR, 1);
        assert!(d.area() <= 400 * 36, "{v}: {d:?}");
        assert!(!r.wants_frame(BAR), "{v}: settles");
    }
}

/// Per-corner radii shape the shadow too: a card rounded only on top
/// casts a square-cornered shadow at the bottom.
#[test]
fn shadows_follow_per_corner_radii() {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Panel, None, vec![(Prop::Bg, color("#ffffff"))]);
    b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(20.0)),
            (Prop::Y, num(20.0)),
            (Prop::Size, num(60.0)),
            (
                Prop::Radius,
                PropValue::Corners(Corners {
                    top_left: 20.0,
                    top_right: 20.0,
                    bottom_right: 0.0,
                    bottom_left: 0.0,
                }),
            ),
            (Prop::Bg, color("#ffffff")),
            (
                Prop::Shadow,
                PropValue::Shadow(vec![Shadow {
                    x: 0.0,
                    y: 0.0,
                    blur: 2.0,
                    spread: 3.0,
                    color: hex("#000000"),
                }]),
            ),
        ],
    );
    let (_, buf) = fresh(b.diff, 100, 100, Scale::ONE);
    // Just outside the bottom-right corner, inside the spread: dark for a
    // square corner. Just outside the top-right corner: light (rounded).
    let br = buf.px(81, 81);
    let tr = buf.px(81, 18);
    assert!(br[0] < 0x40, "square bottom corner: {br:?}");
    assert!(tr[0] > 0xc0, "rounded top corner: {tr:?}");
}

/// Props bound to tokens are evaluated by render: a palette change
/// repaints exactly the nodes using the derived token, with no prop
/// re-sent by logic.
#[test]
fn token_bound_props_follow_the_table() {
    let mut table = TokenTable::default();
    table.insert("accent", color("#89b4fa"));
    table.insert_derived(
        "accent.container",
        TokenExpr::path("accent").call(TokenMethod::Alpha, vec![TokenExpr::value(num(0.5))]),
    );
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(10.0)),
            (Prop::Size, num(10.0)),
            (
                Prop::Bg,
                PropValue::Token(TokenExpr::path("accent.container")),
            ),
            (
                Prop::Enter,
                PropValue::Pose(vec![(Prop::Opacity, num(0.0)), (Prop::Width, num(0.0))]),
            ),
        ],
    );
    b.diff.push(SceneOp::SetTokens {
        table: table.clone(),
    });
    let (mut r, mut buf) = fresh(b.diff, 100, 20, Scale::ONE);
    let blue = buf.px(15, 5);
    assert!(blue[0] > blue[2], "accent is blue: {blue:?}");

    table.insert("accent", color("#f38ba8"));
    let mut d = SceneDiff::new();
    d.push(SceneOp::SetTokens { table });
    r.apply(d);
    assert!(r.wants_frame(BAR));
    let dmg = buf.paint(&mut r, BAR, 1);
    assert_eq!(dmg.rects(), &[Rect::new(10, 0, 10, 10)]);
    let pink = buf.px(15, 5);
    assert!(pink[2] > pink[0], "accent is pink: {pink:?}");
}

/// `set { $surface: … }` on one subtree changes `$surface` (and tokens
/// derived from it) for that subtree only; editing the override repaints
/// only what reads it.
#[test]
fn scoped_token_overrides_apply_to_their_subtree() {
    let mut table = TokenTable::default();
    table.insert("surface", color("#0000ff"));
    table.insert_derived(
        "surface.dim",
        TokenExpr::path("surface").call(TokenMethod::Alpha, vec![TokenExpr::value(num(0.5))]),
    );
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![]);
    let boxed = |x: f32, path: &str| {
        vec![
            (Prop::X, num(x)),
            (Prop::Size, num(10.0)),
            (Prop::Bg, PropValue::Token(TokenExpr::path(path))),
        ]
    };
    let plain = b.node(NodeKind::Box, Some(root), boxed(0.0, "surface"));
    let mut red = TokenTable::default();
    red.insert("surface", color("#ff0000"));
    let group = b.node(
        NodeKind::Stack,
        Some(root),
        vec![(Prop::Tokens, PropValue::Tokens(Box::new(red)))],
    );
    let child = b.node(NodeKind::Box, Some(group), boxed(20.0, "surface"));
    let derived = b.node(NodeKind::Box, Some(group), boxed(40.0, "surface.dim"));
    b.diff.push(SceneOp::SetTokens { table });
    let (mut r, mut buf) = fresh(b.diff, 60, 10, Scale::ONE);
    let _ = (plain, child, derived);
    // BGRA bytes.
    assert_eq!(buf.px(5, 5), [255, 0, 0, 255], "outside: blue");
    assert_eq!(buf.px(25, 5), [0, 0, 255, 255], "inside: red");
    let dim = buf.px(45, 5);
    assert_eq!(
        (dim[0], dim[2], dim[3]),
        (0, 128, 128),
        "derived follows: {dim:?}"
    );

    // Editing the override repaints only the subtree.
    let mut green = TokenTable::default();
    green.insert("surface", color("#00ff00"));
    let mut d = SceneDiff::new();
    d.set(group, Prop::Tokens, PropValue::Tokens(Box::new(green)));
    r.apply(d);
    let dmg = buf.paint(&mut r, BAR, 1);
    assert!(!dmg.covers(Rect::new(0, 0, 10, 10)), "{dmg:?}");
    assert!(dmg.covers(Rect::new(20, 0, 10, 10)), "{dmg:?}");
    assert!(dmg.covers(Rect::new(40, 0, 10, 10)), "{dmg:?}");
    assert_eq!(buf.px(5, 5), [255, 0, 0, 255]);
    assert_eq!(buf.px(25, 5), [0, 255, 0, 255]);
}

/// `radius: full` (the workspace dot, the OSD pill) draws round in each of
/// its encodings.
#[test]
fn radius_full_keyword_draws_a_pill() {
    for radius in [
        PropValue::Keyword("full".into()),
        PropValue::Corners(Corners::FULL),
        num(999.0),
    ] {
        let mut b = Builder::default();
        let root = b.node(NodeKind::Bar, None, vec![]);
        b.node(
            NodeKind::Box,
            Some(root),
            vec![
                (Prop::Width, num(40.0)),
                (Prop::Height, num(20.0)),
                (Prop::Radius, radius.clone()),
                (Prop::Bg, color("#ffffff")),
            ],
        );
        let (_, buf) = fresh(b.diff, 40, 20, Scale::ONE);
        assert_eq!(buf.px(0, 0)[3], 0, "corner is cut: {radius:?}");
        assert_eq!(buf.px(1, 1)[3], 0, "corner is cut: {radius:?}");
        assert_eq!(buf.px(20, 10)[3], 255, "{radius:?}");
    }
}

/// A popup nested in a bar is its own surface: the bar does not paint it,
/// and editing it does not wake the bar.
#[test]
fn nested_popup_paints_only_on_its_own_surface() {
    let mut b = Builder::default();
    let bar = b.node(NodeKind::Bar, None, vec![(Prop::Color, color("#ff0000"))]);
    let popup = b.node(
        NodeKind::Popup,
        Some(bar),
        vec![(Prop::Bg, color("#00ff00"))],
    );
    let inner = b.node(
        NodeKind::Box,
        Some(popup),
        vec![(Prop::Size, num(4.0)), (Prop::Bg, color("#0000ff"))],
    );
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    let (bar_s, popup_s) = (SurfaceId(1), SurfaceId(2));
    r.attach_surface(bar_s, bar);
    r.attach_surface(popup_s, popup);
    let mut a = Buffer::new(20, 10, Scale::ONE);
    let mut p = Buffer::new(10, 10, Scale::ONE);
    a.paint(&mut r, bar_s, 0);
    p.paint(&mut r, popup_s, 0);
    assert_eq!(a.px(1, 1), [0, 0, 0, 0], "the bar does not paint the popup");
    assert_eq!(p.px(1, 1), [255, 0, 0, 255]);
    assert_eq!(p.px(8, 8), [0, 255, 0, 255]);

    let mut d = SceneDiff::new();
    d.set(inner, Prop::Bg, color("#ffffff"));
    r.apply(d);
    assert!(!r.wants_frame(bar_s), "the bar stays idle");
    assert!(r.wants_frame(popup_s));
    assert!(!p.paint(&mut r, popup_s, 1).is_empty());
    assert_eq!(p.px(1, 1), [255, 255, 255, 255]);
}
