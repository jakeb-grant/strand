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

/// design.md's clock (`"%a %d  %H:%M"`) ticking from 09:41 to 09:42:
/// only the glyph that changed is repainted (a text node's glyph cells,
/// `NodeRecord::glyphs`), and the partial repaint equals a full one.
/// The design bar's whole clock is about 89 × 11 px at 1×; one digit is
/// a tenth of that.
#[test]
fn a_tick_repaints_only_the_glyphs_that_changed() {
    for (s, budget) in [(Scale::ONE, 250), (Scale::new(150).unwrap(), 400)] {
        let size = s.physical_size(LogicalSize::new(2560.0, 36.0));
        let (diff, clock) = bar("Mon 05  09:41");
        let (mut r, mut buf) = fresh(diff, size.w, size.h, s);
        r.apply(set_text(clock, "Mon 05  09:42"));
        let d = buf.paint(&mut r, BAR, 1);
        assert!(
            d.area() > 0 && d.area() <= budget,
            "{s:?}: damage {d:?} area {}",
            d.area()
        );
        let (_, full) = fresh(bar("Mon 05  09:42").0, size.w, size.h, s);
        assert!(
            buf.pixels == full.pixels,
            "{s:?}: partial differs from full"
        );
        // A longer text (the day changed width): its own glyphs, and the
        // ones that moved; still exact.
        r.apply(set_text(clock, "Tue 06  10:00"));
        buf.paint(&mut r, BAR, 1);
        let (_, full) = fresh(bar("Tue 06  10:00").0, size.w, size.h, s);
        assert!(buf.pixels == full.pixels, "{s:?}: second tick differs");
    }
}

#[test]
fn clock_tick_at_fractional_scale_matches_full_repaint() {
    let s = Scale::new(150).unwrap();
    let size = s.physical_size(LogicalSize::new(2048.0, 36.0));
    let (diff, clock) = bar("12:59");
    let (mut r, mut buf) = fresh(diff, size.w, size.h, s);
    r.apply(set_text(clock, "13:00"));
    let d = buf.paint(&mut r, BAR, 1);
    // Scale::new(150) is 150/120 = 1.25×: 1.25² × the 1× budget.
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
    d.push(SceneOp::Remove {
        id: c,
        window: false,
    });
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
    // Every supported scale (1, 1.25, 1.5, 1.75, 2) with one seed, and a
    // second seed at the integer and the most common fractional scale.
    // (Debug-build vello is slow; this keeps the test around 20 s.)
    for scale in [120, 150, 180, 210, 240] {
        random_edits(0x2545_f491_4f6c_dd1d, Scale::new(scale).unwrap(), 160);
    }
    for scale in [120, 180] {
        random_edits(0x9e37_79b9_7f4a_7c15, Scale::new(scale).unwrap(), 160);
    }
}

/// Random edits painted into three buffers handed back out of order, like
/// a compositor releasing them; each paint must equal a full repaint.
/// Buffer age counts commits, and only non-empty paints are committed
/// (the `Painter` contract), so under-widening by age would show.
fn random_edits(seed: u64, scale: Scale, frames: usize) {
    let mut seed = seed;
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
    let size = scale.physical_size(LogicalSize::new(200.0, 24.0));
    let (w, h) = (size.w, size.h);
    let mut reference = renderer();
    assert!(reference.apply(b.diff.clone()).is_empty());
    reference.attach_surface(BAR, root);
    let (mut r, first) = fresh(b.diff, w, h, scale);
    let mut bufs = [first, Buffer::new(w, h, scale), Buffer::new(w, h, scale)];
    // Commit number each buffer was last committed at.
    let mut commits = 1usize;
    let mut painted_at: [Option<usize>; 3] = [Some(1), None, None];
    let mut front = 0;
    let mut log: Vec<String> = Vec::new();
    for frame in 1..frames {
        let mut d = SceneDiff::new();
        for _ in 0..1 + rnd(3) {
            let id = boxes[rnd(boxes.len() as u32) as usize];
            match rnd(12) {
                0 => d.set(id, Prop::X, num(rnd(180) as f32)),
                1 => d.set(id, Prop::Y, num(rnd(20) as f32 - 4.0)),
                2 => d.set(id, Prop::Bg, color(palette[rnd(5) as usize])),
                3 => d.set(id, Prop::Radius, num(rnd(9) as f32)),
                4 => d.set(id, Prop::Opacity, num(rnd(4) as f32 / 3.0)),
                5 => d.set(label, Prop::Color, color(palette[rnd(4) as usize])),
                6 if id != boxes[0] => d.push(SceneOp::Move {
                    id,
                    parent: Some(if rnd(2) == 0 { root } else { clip }),
                    index: rnd(8),
                }),
                7 => d.set(
                    id,
                    Prop::Shadow,
                    PropValue::Shadow(if rnd(3) == 0 {
                        vec![]
                    } else {
                        vec![Shadow {
                            x: rnd(5) as f32 - 2.0,
                            y: rnd(4) as f32,
                            blur: rnd(8) as f32,
                            spread: rnd(3) as f32,
                            color: hex(palette[rnd(4) as usize]).with_alpha(0.6),
                        }]
                    }),
                ),
                8 => d.set(
                    id,
                    Prop::Border,
                    PropValue::Border(Border {
                        width: rnd(3) as f32,
                        paint: Paint::Solid(hex(palette[rnd(4) as usize])),
                    }),
                ),
                9 => d.set(
                    id,
                    Prop::Bg,
                    PropValue::Paint(Paint::Linear {
                        angle: rnd(360) as f32,
                        stops: vec![
                            GradientStop {
                                offset: 0.0,
                                color: hex(palette[rnd(4) as usize]),
                            },
                            GradientStop {
                                offset: 1.0,
                                color: hex(palette[rnd(5) as usize]),
                            },
                        ],
                    }),
                ),
                _ => d.set(label, Prop::Text, text(&rnd(1000).to_string())),
            };
        }
        log.push(format!("{d:?}"));
        reference.apply(d.clone());
        r.apply(d);
        let i = rnd(3) as usize;
        if r.wants_frame(BAR) {
            let age = painted_at[i].map_or(0, |c| (commits + 1 - c).min(255) as u8);
            let dmg = bufs[i].paint(&mut r, BAR, age);
            log.push(format!("frame {frame} buf {i} age {age} damage {dmg:?}"));
            if !dmg.is_empty() {
                commits += 1;
                painted_at[i] = Some(commits);
                front = i;
            }
        }
        let mut full = Buffer::new(w, h, scale);
        full.paint(&mut reference, BAR, 0);
        if let Some(p) = bufs[front]
            .pixels
            .iter()
            .zip(&full.pixels)
            .position(|(a, b)| a != b)
        {
            let px = p as u32 / 4;
            for l in &log[log.len().saturating_sub(10)..] {
                eprintln!("{l}");
            }
            panic!(
                "scale {scale:?} frame {frame} differs at ({}, {}): {:?} vs {:?}",
                px % w,
                px / w,
                bufs[front].px(px % w, px / w),
                full.px(px % w, px / w),
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

/// A worker renderer whose text worker answers nothing the renderer asks
/// until the returned sender is sent to or dropped, so a test can look
/// at the state "requests in flight" without racing the worker.
///
/// The worker is handed a warm-up request of its own before the
/// renderer gets it; the waker it runs after answering that request
/// blocks the worker thread on the gate. The warm-up is at a scale no
/// surface uses and under a key the renderer never issues, so its
/// delivery is ignored (no atlas page is mirrored for an unused scale).
fn held_worker_renderer() -> (Renderer, std::sync::mpsc::Sender<()>) {
    use std::sync::{Arc, Mutex, mpsc};
    use strand_render::TextBackend;
    use strand_text::{FontConfig, TextKey, TextRequest, TextWorker, test_font_path};
    let data = std::fs::read(test_font_path()).unwrap();
    let (open, gate) = mpsc::channel::<()>();
    let gate = Mutex::new(Some(gate));
    let waker: strand_text::Waker = Box::new(move || {
        // Only the first wake (the warm-up's) holds; later ones pass.
        let held = gate.lock().ok().and_then(|mut g| g.take());
        if let Some(gate) = held {
            let _ = gate.recv();
        }
    });
    let worker =
        TextWorker::spawn_with_waker(FontConfig::isolated(vec![Arc::new(data)]), Some(waker))
            .unwrap();
    worker
        .request(TextRequest {
            key: TextKey(u64::MAX),
            text: "warm-up".into(),
            style: Default::default(),
            max_width: None,
            scale: Scale::from_integer(7).unwrap(),
        })
        .unwrap();
    (Renderer::new(TextBackend::Worker(worker)), open)
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
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![(Prop::Bg, color("#ffffff")), (Prop::Size, num(100.0))],
    );
    b.node(
        NodeKind::Box,
        Some(root),
        vec![
            // Placed by coordinates: its shadow's reach counts from there.
            (Prop::Place, PropValue::Keyword("absolute".into())),
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
    b.diff.set_tokens(table.clone(), Transition::Instant);
    let (mut r, mut buf) = fresh(b.diff, 100, 20, Scale::ONE);
    let blue = buf.px(15, 5);
    assert!(blue[0] > blue[2], "accent is blue: {blue:?}");

    table.insert("accent", color("#f38ba8"));
    let mut d = SceneDiff::new();
    d.set_tokens(table, Transition::Default);
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
    // Clear, so the boxes are read as drawn (a bar naming no `bg` gets
    // the table's `$surface`).
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#00000000"))]);
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
    b.diff.set_tokens(table, Transition::Instant);
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

    // Removing the bar takes the popup with it: its surface clears its
    // stale frame (until the surface manager destroys it on `Removed`).
    let mut d = SceneDiff::new();
    d.push(SceneOp::Remove {
        id: bar,
        window: false,
    });
    r.apply(d);
    assert!(
        r.wants_frame(popup_s),
        "the orphaned popup surface repaints"
    );
    assert!(!p.paint(&mut r, popup_s, 1).is_empty());
    assert_eq!(p.px(8, 8), [0, 0, 0, 0]);
    let removed: Vec<NodeId> = r
        .take_surface_changes()
        .into_iter()
        .filter(|(_, c)| *c == SurfaceChange::Removed)
        .map(|(id, _)| id)
        .collect();
    assert_eq!(removed, vec![bar, popup]);
}

/// The surface manager learns what to create from resolved specs: token
/// changes reach a token-bound `margin`, and a `layer` change asks for the
/// surface to be recreated.
#[test]
fn surface_specs_resolve_tokens_and_report_changes() {
    let mut table = TokenTable::default();
    table.insert("space.2", num(8.0));
    let mut b = Builder::default();
    let space = || PropValue::Token(TokenExpr::path("space.2"));
    let bar = b.node(
        NodeKind::Bar,
        None,
        vec![
            (Prop::Name, text("Top")),
            (Prop::Edge, PropValue::Keyword("top".into())),
            (Prop::Height, num(36.0)),
            // margin: $space.2, $space.2, 0
            (
                Prop::Margin,
                PropValue::List(vec![space(), space(), num(0.0)]),
            ),
        ],
    );
    let popup = b.node(NodeKind::Popup, Some(bar), vec![]);
    b.diff.set_tokens(table.clone(), Transition::Instant);
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    let changes = r.take_surface_changes();
    assert_eq!(changes.len(), 2, "{changes:?}");
    let SurfaceChange::Created(spec) = &changes[0].1 else {
        panic!("{changes:?}")
    };
    assert_eq!(changes[0].0, bar);
    assert_eq!(spec.namespace(), "strand-Top");
    assert_eq!(spec.exclusive_zone(), Some(36.0));
    assert_eq!(
        (spec.margin.top, spec.margin.left, spec.margin.bottom),
        (8.0, 8.0, 0.0)
    );
    assert_eq!(changes[1].0, popup);
    assert!(r.take_surface_changes().is_empty());

    // A compact theme changes $space.2: reconfigure in place.
    table.insert("space.2", num(4.0));
    let mut d = SceneDiff::new();
    d.set_tokens(table, Transition::Default);
    r.apply(d);
    let changes = r.take_surface_changes();
    assert_eq!(changes.len(), 1, "only the bar's spec moved: {changes:?}");
    let SurfaceChange::Updated { spec, recreate } = &changes[0].1 else {
        panic!("{changes:?}")
    };
    assert!(!recreate);
    assert_eq!(spec.margin.left, 4.0);
    assert_eq!(r.surface_spec(bar), Some(spec));

    // layer: overlay needs a new layer surface.
    let mut d = SceneDiff::new();
    d.set(bar, Prop::Layer, PropValue::Keyword("overlay".into()));
    r.apply(d);
    let changes = r.take_surface_changes();
    assert!(matches!(
        changes.as_slice(),
        [(id, SurfaceChange::Updated { recreate: true, .. })] if *id == bar
    ));
}

/// A new surface's first frame already has its text: while the worker
/// shapes it the surface asks for no frame (up to a deadline).
#[test]
fn first_frame_of_a_new_surface_has_its_text() {
    use std::time::Duration;
    // The worker is held until the asserts below have looked: with a free
    // one, `configure_surface`'s poll could collect the bar's layouts
    // (requested at `apply`, for its size) before the assert, which then
    // saw a surface with nothing to wait for (a flake under load).
    let (mut r, open) = held_worker_renderer();
    r.set_first_frame_wait(Duration::from_secs(30));
    let (diff, _) = bar("12:59");
    r.apply(diff);
    r.attach_surface(BAR, r.tree().roots()[0]);
    r.configure_surface(BAR, Size::new(2560, 36), Scale::ONE);
    assert!(r.text_pending());
    assert!(!r.wants_frame(BAR), "no frame without its text");
    assert!(r.frame_deadline(BAR).is_some());
    drop(open);
    assert!(r.wait_for_text(Duration::from_secs(10)));
    assert!(r.wants_frame(BAR));
    assert!(r.frame_deadline(BAR).is_none());
    let mut buf = Buffer::new(2560, 36, Scale::ONE);
    buf.paint(&mut r, BAR, 0);
    let (_, want) = fresh(bar("12:59").0, 2560, 36, Scale::ONE);
    assert!(buf.pixels == want.pixels, "the first frame has the text");

    // Past the deadline a surface paints anyway.
    let mut r = worker_renderer();
    r.set_first_frame_wait(Duration::ZERO);
    r.apply(bar("12:59").0);
    r.attach_surface(BAR, r.tree().roots()[0]);
    r.configure_surface(BAR, Size::new(2560, 36), Scale::ONE);
    assert!(r.wants_frame(BAR));
}

/// A text node added to a painted surface holds the frame for its glyphs
/// (up to `NEW_TEXT_WAIT`), so the frame that shows the node shows its
/// text, not an empty node a refresh before it; changed text holds
/// nothing (its old layout shows until the new one lands).
#[test]
fn a_new_text_node_holds_the_frame_for_its_glyphs() {
    use std::time::Duration;
    let added = |root: NodeId| {
        let mut d = SceneDiff::new();
        let id = NodeId::new(100, 0);
        d.create(id, NodeKind::Text, Some(root), u32::MAX);
        d.set(id, Prop::X, num(2000.0));
        d.set(id, Prop::Y, num(10.0));
        d.set(id, Prop::Text, text("added"));
        d
    };
    let mut r = worker_renderer();
    r.set_new_text_wait(Duration::from_secs(30));
    let (diff, clock) = bar("12:59");
    r.apply(diff);
    let root = r.tree().roots()[0];
    r.attach_surface(BAR, root);
    r.configure_surface(BAR, Size::new(2560, 36), Scale::ONE);
    assert!(r.wait_for_text(Duration::from_secs(10)));
    let mut buf = Buffer::new(2560, 36, Scale::ONE);
    buf.paint(&mut r, BAR, 0);

    // An idle surface (nothing painted within the busy window, here none,
    // so the test does not depend on the clock).
    r.set_busy_window(Duration::ZERO);
    r.apply(added(root));
    assert!(
        !r.wants_frame(BAR),
        "no frame with the node but not its text"
    );
    assert!(r.frame_deadline(BAR).is_some());
    assert!(r.wait_for_text(Duration::from_secs(10)));
    assert!(r.wants_frame(BAR), "its text is here: paint");
    assert!(r.frame_deadline(BAR).is_none());
    buf.paint(&mut r, BAR, 1);
    let mut want = bar("12:59").0;
    want.ops.extend(added(root).ops);
    let (_, want) = fresh(want, 2560, 36, Scale::ONE);
    assert!(buf.pixels == want.pixels, "the frame has the new text");

    // Changed text shows its old layout meanwhile: nothing is held.
    let mut d = SceneDiff::new();
    d.set(clock, Prop::Text, text("13:00"));
    r.apply(d);
    assert!(
        r.frame_deadline(BAR).is_none(),
        "changed text holds nothing"
    );

    // With no wait, the node is painted at once.
    let mut r = worker_renderer();
    r.set_new_text_wait(Duration::ZERO);
    r.apply(bar("12:59").0);
    r.attach_surface(BAR, root);
    r.configure_surface(BAR, Size::new(2560, 36), Scale::ONE);
    assert!(r.wait_for_text(Duration::from_secs(10)));
    buf.paint(&mut r, BAR, 0);
    r.apply(added(root));
    assert!(r.wants_frame(BAR));
}

/// A surface in motion (it painted within `BUSY_WINDOW`, as every frame
/// of an animation does) never holds a frame for new text: the frame is
/// painted at once, without the new glyphs, and they follow a frame later.
#[test]
fn a_busy_surface_does_not_hold_for_new_text() {
    use std::time::Duration;
    let mut r = worker_renderer();
    r.set_new_text_wait(Duration::from_secs(30));
    // Busy however slow the machine paints (the default is 34 ms).
    r.set_busy_window(Duration::from_secs(30));
    r.apply(bar("12:59").0);
    let root = r.tree().roots()[0];
    r.attach_surface(BAR, root);
    r.configure_surface(BAR, Size::new(2560, 36), Scale::ONE);
    assert!(r.wait_for_text(Duration::from_secs(10)));
    let mut buf = Buffer::new(2560, 36, Scale::ONE);
    buf.paint(&mut r, BAR, 0);
    // A frame of motion (a colour on its way), then a node added right
    // after.
    let mut d = SceneDiff::new();
    d.set(root, Prop::Bg, color("#303050"));
    r.apply(d);
    assert!(r.wants_frame(BAR));
    buf.paint(&mut r, BAR, 1);
    let mut d = SceneDiff::new();
    let id = NodeId::new(100, 0);
    d.create(id, NodeKind::Text, Some(root), u32::MAX);
    d.set(id, Prop::X, num(2000.0));
    d.set(id, Prop::Text, text("added"));
    r.apply(d);
    assert!(
        r.frame_deadline(BAR).is_none(),
        "a busy surface holds nothing"
    );
    assert!(
        r.wants_frame(BAR),
        "painted at once, without the new glyphs"
    );
}

/// `ellipsis` and `max_lines` reach the text engine: a long title with
/// `max_width` stays on one line; `marks` paint in `mark_color`.
#[test]
fn ellipsis_and_marks_reach_the_text_engine() {
    let title = "Firefox — The Rust Programming Language — Fearless Concurrency";
    let scene = |ellipsis: bool| {
        let mut b = Builder::default();
        let root = b.node(
            NodeKind::Bar,
            None,
            vec![
                (Prop::Color, color("#ffffff")),
                (Prop::Font, PropValue::Font(font(13.0))),
            ],
        );
        let mut props = vec![
            (Prop::X, num(2.0)),
            (Prop::MaxWidth, num(120.0)),
            (Prop::Text, text(title)),
            (Prop::MarkColor, color("#ff0000")),
            (
                Prop::Marks,
                PropValue::List(vec![PropValue::List(vec![num(0.0), num(7.0)])]),
            ),
        ];
        if ellipsis {
            props.push((Prop::Ellipsis, PropValue::Keyword("end".into())));
        }
        b.node(NodeKind::Text, Some(root), props);
        b.diff
    };
    let (_, wrapped) = fresh(scene(false), 200, 60, Scale::ONE);
    let (_, cut) = fresh(scene(true), 200, 60, Scale::ONE);
    assert!(lit(&wrapped, Rect::new(0, 20, 200, 40)) > 0, "wraps");
    assert_eq!(lit(&cut, Rect::new(0, 20, 200, 40)), 0, "one line");
    assert_eq!(lit(&cut, Rect::new(123, 0, 77, 20)), 0, "within max_width");
    // "Firefox" is red, the rest white (BGRA).
    let reds = (0..20)
        .flat_map(|y| (0..40).map(move |x| (x, y)))
        .filter(|&(x, y)| {
            let p = cut.px(x, y);
            p[2] > 100 && p[0] < 30
        })
        .count();
    assert!(reds > 20, "{reds}");
    assert_eq!(
        cut.px(100, 8)[2],
        cut.px(100, 8)[0],
        "unmarked text is white"
    );
}

/// The render thread's atlas mirror follows the worker's pages: replaced
/// text does not leave pages behind.
#[test]
fn atlas_mirror_stays_bounded() {
    let (diff, clock) = bar("12:59");
    let (mut r, mut buf) = fresh(diff, 2560, 36, Scale::ONE);
    let max = strand_text::AtlasConfig::default();
    for i in 0..60u32 {
        // Many distinct glyphs at many sizes.
        let mut d = SceneDiff::new();
        d.set(clock, Prop::Font, PropValue::Font(font(10.0 + i as f32)));
        d.set(
            clock,
            Prop::Text,
            text(&format!(
                "{i} ABCDEFGHIJKLMNOPQRSTUVWXYZ abcdefghijklmnopqrstuvwxyz"
            )),
        );
        r.apply(d);
        buf.paint(&mut r, BAR, 1);
        assert!(
            r.atlas_mirror_bytes(Scale::ONE) <= 4 * max.max_bytes,
            "{}",
            r.atlas_mirror_bytes(Scale::ONE)
        );
    }
    // Back to a small clock: once the big layouts are gone the worker trims
    // its extra pages, and the mirror drops them too.
    for t in ["13:00", "13:01"] {
        r.apply(set_text(clock, t));
        let mut d = SceneDiff::new();
        d.set(clock, Prop::Font, PropValue::Font(font(13.0)));
        r.apply(d);
        buf.paint(&mut r, BAR, 1);
    }
    let regular = 4 * max.page_size as usize * max.page_size as usize * max.max_pages;
    assert!(
        r.atlas_mirror_bytes(Scale::ONE) <= regular,
        "{} > {regular}",
        r.atlas_mirror_bytes(Scale::ONE)
    );
}

/// A bar with its clock centred in a full-width text (the demo's `split`).
fn centred_bar(clock: &str) -> SceneDiff {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Bar,
        None,
        vec![
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Color, color("#cdd6f4")),
            (Prop::Font, PropValue::Font(font(13.0))),
        ],
    );
    b.node(
        NodeKind::Text,
        Some(root),
        vec![
            (Prop::Y, num(10.0)),
            (Prop::Width, PropValue::Length(Length::Percent(100.0))),
            (Prop::Align, PropValue::Keyword("center".into())),
            (Prop::Text, text(clock)),
        ],
    );
    b.diff
}

/// With the text worker: a surface added at the same scale as a painted
/// one but another width (a 1920 monitor next to a 2560 one) shares its
/// text layout (text is shaped without a width bound and aligned in its
/// box), so its first frame needs no wait and shows exactly a fresh
/// render; the first surface does not repaint.
#[test]
fn new_surface_of_another_width_shares_the_layout() {
    use std::time::Duration;
    const WIDE: SurfaceId = SurfaceId(1);
    const NARROW: SurfaceId = SurfaceId(2);
    let mut r = worker_renderer();
    r.set_first_frame_wait(Duration::from_secs(30));
    assert!(r.apply(centred_bar("12:59")).is_empty());
    let root = r.tree().roots()[0];
    r.attach_surface(WIDE, root);
    r.configure_surface(WIDE, Size::new(2560, 32), Scale::ONE);
    assert!(r.wait_for_text(Duration::from_secs(10)));
    let mut wide = Buffer::new(2560, 32, Scale::ONE);
    wide.paint(&mut r, WIDE, 0);

    r.attach_surface(NARROW, root);
    r.configure_surface(NARROW, Size::new(1920, 32), Scale::ONE);
    assert!(!r.text_pending(), "nothing to shape for another width");
    assert!(r.frame_deadline(NARROW).is_none());
    assert!(r.wants_frame(NARROW));
    assert!(!r.wants_frame(WIDE), "the wide bar keeps its layout");
    let mut narrow = Buffer::new(1920, 32, Scale::ONE);
    narrow.paint(&mut r, NARROW, 0);
    let (_, want) = fresh(centred_bar("12:59"), 1920, 32, Scale::ONE);
    assert!(narrow.pixels == want.pixels, "the narrow bar is centred");

    // A tick: each bar repaints only its own clock, centred on it.
    let clock = r.tree().get(root).unwrap().children[0];
    let mut d = SceneDiff::new();
    d.set(clock, Prop::Text, text("13:00"));
    assert!(r.apply(d).is_empty());
    assert!(r.wait_for_text(Duration::from_secs(10)));
    for (id, buf, w) in [(WIDE, &mut wide, 2560), (NARROW, &mut narrow, 1920)] {
        let d = buf.paint(&mut r, id, 1);
        let b = d.bounds().unwrap();
        let c = b.x + b.w as i32 / 2;
        assert!((c - w / 2).abs() < 8, "{w}: damage {b:?} off centre");
        assert!(d.area() <= 2000, "{w}: {}", d.area());
        let (_, want) = fresh(centred_bar("13:00"), w as u32, 32, Scale::ONE);
        assert!(buf.pixels == want.pixels, "{w}: tick differs from fresh");
    }
}

/// Hit testing: the node painted under a point, innermost first, up to
/// the surface root; the root alone where nothing is drawn; positions are
/// logical, so a fractional scale hits the same nodes.
#[test]
fn hit_finds_the_painted_node_and_its_ancestors() {
    for scale in [Scale::ONE, Scale::from_f64(1.25).unwrap()] {
        let (diff, clock) = bar("12:34");
        let w = (2560.0 * scale.as_f64()) as u32;
        let h = (36.0 * scale.as_f64()) as u32;
        let (r, _buf) = fresh(diff, w, h, scale);
        let root = r.tree().roots()[0];
        // The first workspace dot spans x 12..20, y 14..22.
        let dot = r.tree().get(root).unwrap().children[0];
        assert_eq!(r.hit(BAR, LogicalPoint::new(15.0, 17.0)), [dot, root]);
        // Over the clock text.
        let hit = r.hit(BAR, LogicalPoint::new(1270.0, 18.0));
        assert_eq!(hit, [clock, root], "{scale:?}");
        // The texts stretch over the bar (a surface root stacks its
        // children): bare background is under the topmost of them.
        let hit = r.hit(BAR, LogicalPoint::new(700.0, 30.0));
        assert_eq!(hit.len(), 2, "{hit:?}");
        assert_eq!(hit[1], root);
        assert!(r.hit(SurfaceId(99), LogicalPoint::new(1.0, 1.0)).is_empty());
    }
}

/// Overlapping nodes: the hit is the one painted on top (later siblings
/// over earlier ones and their children), not the deepest one.
#[test]
fn hit_follows_paint_order() {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let under = b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(10.0)),
            (Prop::Y, num(4.0)),
            (Prop::Size, num(20.0)),
            (Prop::Bg, color("#89b4fa")),
        ],
    );
    let inner = b.node(
        NodeKind::Box,
        Some(under),
        vec![
            (Prop::X, num(10.0)),
            (Prop::Y, num(0.0)),
            (Prop::Size, num(10.0)),
            (Prop::Bg, color("#f38ba8")),
        ],
    );
    let over = b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(20.0)),
            (Prop::Y, num(4.0)),
            (Prop::Size, num(20.0)),
            (Prop::Bg, color("#a6e3a1")),
        ],
    );
    let (r, _buf) = fresh(b.diff, 200, 36, Scale::ONE);
    // Under `over` and `inner` both: `over` is painted last.
    assert_eq!(r.hit(BAR, LogicalPoint::new(25.0, 8.0)), [over, root]);
    // Only `under` (left of `over`).
    assert_eq!(r.hit(BAR, LogicalPoint::new(12.0, 8.0)), [under, root]);
    // `inner` alone is never on top here; its own chain is reachable
    // where nothing covers it.
    assert!(!r.hit(BAR, LogicalPoint::new(25.0, 8.0)).contains(&inner));
}

/// A long wrapped text (6,000 glyphs: a notification body, a log, a
/// clipboard entry) whose last character changes repaints only that
/// glyph and matches a full repaint. Its glyphs are diffed in linear
/// time (`renderer/tests.rs::glyph_damage_is_linear_in_the_glyphs`): the
/// quadratic diff made this frame 16 ms on an optimised build, the
/// linear one about 5 ms, nearly all of it shaping and flattening the
/// new text.
#[test]
fn a_long_text_repaints_only_its_changed_glyph() {
    let body = |last: char| -> String {
        let mut s: String = (0..5999)
            .map(|i| {
                if i % 9 == 8 {
                    ' '
                } else {
                    (b'a' + (i % 26) as u8) as char
                }
            })
            .collect();
        s.push(last);
        s
    };
    let scene = |t: &str| {
        let mut b = Builder::default();
        let root = b.node(
            NodeKind::Panel,
            None,
            vec![
                (Prop::Bg, color("#1e1e2e")),
                (Prop::Color, color("#cdd6f4")),
                (Prop::Font, PropValue::Font(font(13.0))),
            ],
        );
        let id = b.node(
            NodeKind::Text,
            Some(root),
            vec![
                (Prop::X, num(10.0)),
                (Prop::Y, num(10.0)),
                (Prop::Width, num(1900.0)),
                (Prop::Text, text(t)),
            ],
        );
        (b.diff, id)
    };
    let (diff, id) = scene(&body('x'));
    let (mut r, mut buf) = fresh(diff, 1920, 1080, Scale::ONE);
    let mut best = std::time::Duration::MAX;
    let mut last = 'x';
    for (i, c) in ['y', 'z', 'x', 'y', 'z', 'x'].into_iter().enumerate() {
        let t = body(c);
        let start = std::time::Instant::now();
        r.apply(set_text(id, &t));
        let d = buf.paint(&mut r, BAR, 1);
        best = best.min(start.elapsed());
        assert!(
            d.area() > 0 && d.area() <= 400,
            "frame {i}: damage {d:?} area {}",
            d.area()
        );
        last = c;
    }
    let (_, full) = fresh(scene(&body(last)).0, 1920, 1080, Scale::ONE);
    assert!(buf.pixels == full.pixels, "partial differs from full");
    eprintln!("6,000 glyphs, last one changed: {best:?} (best of 6)");
}

/// design.md's bar laid out by a `split`: dots in `start`, the clock
/// (`"%a %d  %H:%M"`) in `center`, and in `end` the volume icon, the
/// battery text and a tray icon. Returns the clock's id and `end`'s
/// children.
fn split_bar(clock: &str) -> (SceneDiff, NodeId, Vec<NodeId>) {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Bar,
        None,
        vec![
            (Prop::Height, num(32.0)),
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Color, color("#cdd6f4")),
            (Prop::Font, PropValue::Font(font(13.0))),
        ],
    );
    let split = b.node(
        NodeKind::Split,
        Some(root),
        vec![(Prop::Pad, PropValue::List(vec![num(0.0), num(12.0)]))],
    );
    let start = b.node(NodeKind::Start, Some(split), vec![(Prop::Gap, num(6.0))]);
    for _ in 0..5 {
        b.node(
            NodeKind::Box,
            Some(start),
            vec![
                (Prop::Size, num(8.0)),
                (Prop::Radius, num(999.0)),
                (Prop::Bg, color("#89b4fa")),
            ],
        );
    }
    let center = b.node(NodeKind::Center, Some(split), vec![]);
    let clock_id = b.node(
        NodeKind::Text,
        Some(center),
        vec![(Prop::Text, text(clock))],
    );
    let end = b.node(NodeKind::End, Some(split), vec![(Prop::Gap, num(8.0))]);
    let mut ends = Vec::new();
    for (c, w) in [("#a6e3a1", 16.0), ("", 0.0), ("#f9e2af", 16.0)] {
        ends.push(if c.is_empty() {
            b.node(
                NodeKind::Text,
                Some(end),
                vec![(Prop::Text, text("87%  ▂▄▆"))],
            )
        } else {
            b.node(
                NodeKind::Box,
                Some(end),
                vec![
                    (Prop::Width, num(w)),
                    (Prop::Height, num(16.0)),
                    (Prop::Bg, color(c)),
                ],
            )
        });
    }
    (b.diff, clock_id, ends)
}

/// The clock's box in physical pixels, rounded out and widened by `m`.
fn physical_box(r: &Renderer, id: NodeId, s: Scale, m: i32) -> Rect {
    let b = r.boxes(BAR).unwrap().rects[&id];
    let x0 = s.to_physical(b.x).floor() as i32 - m;
    let y0 = s.to_physical(b.y).floor() as i32 - m;
    let x1 = s.to_physical(b.x + b.w).ceil() as i32 + m;
    let y1 = s.to_physical(b.y + b.h).ceil() as i32 + m;
    Rect::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32)
}

/// The midnight tick on design.md's centred clock: the day name changes
/// width ("Mon 05  23:59" to "Tue 06  00:00", and every other day of the
/// week, so both parities of the width change), so the centred text moves
/// and every glyph is repainted where it was and where it is. Nothing
/// else moves (`split`'s sides do not depend on the centre), so no damage
/// falls outside the clock's old and new boxes; the partial repaint
/// equals a full one. Per output the tick stays within design.md's
/// "about 60×20 px" (scaled by scale²); summed over a 1× and a 1.25×
/// output it is over M0's 2,000 px², the documented midnight exception
/// (decisions.md, wave3-pixels exit fixer r3; m2-report.md).
#[test]
fn the_midnight_tick_damages_only_the_centred_clock() {
    let days = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun", "Mon"];
    let mut worst = 0;
    for w in days.windows(2) {
        let (from, to) = (format!("{} 05  23:59", w[0]), format!("{} 06  00:00", w[1]));
        let mut total = 0;
        for s in [Scale::ONE, Scale::from_f64(1.25).unwrap()] {
            let size = s.physical_size(LogicalSize::new(2560.0, 32.0));
            let (diff, clock, ends) = split_bar(&from);
            let (mut r, mut buf) = fresh(diff, size.w, size.h, s);
            let before: Vec<LogicalRect> = ends
                .iter()
                .map(|id| r.boxes(BAR).unwrap().rects[id])
                .collect();
            let old = physical_box(&r, clock, s, 2);
            r.apply(set_text(clock, &to));
            let d = buf.paint(&mut r, BAR, 1);
            let new = physical_box(&r, clock, s, 2);
            let after: Vec<LogicalRect> = ends
                .iter()
                .map(|id| r.boxes(BAR).unwrap().rects[id])
                .collect();
            assert_eq!(
                before, after,
                "{from} -> {to} at {s:?}: the end section moved"
            );
            for rect in d.rects() {
                assert!(
                    old.contains_rect(*rect) || new.contains_rect(*rect),
                    "{from} -> {to} at {s:?}: damage {rect:?} outside the clock \
                     ({old:?}, {new:?})"
                );
            }
            let budget = (60.0 * 20.0 * s.as_f64() * s.as_f64()) as u64;
            assert!(
                d.area() > 0 && d.area() <= budget,
                "{from} -> {to} at {s:?}: {}",
                d.area()
            );
            total += d.area();
            let (_, full) = fresh(split_bar(&to).0, size.w, size.h, s);
            assert!(buf.pixels == full.pixels, "{to} at {s:?}: partial differs");
            // The next minute is an ordinary tick again.
            r.apply(set_text(clock, &format!("{} 06  00:01", w[1])));
            let d = buf.paint(&mut r, BAR, 1);
            assert!(d.area() <= budget / 4, "{to} at {s:?}: 00:01 {}", d.area());
        }
        eprintln!("{from} -> {to} over 1× + 1.25×: {total} px²");
        worst = worst.max(total);
    }
    eprintln!("worst midnight tick: {worst} px²");
    assert!(worst <= 2 * 2000, "{worst}");
}

// ---- (M4) Time signals and per-node clocks ---------------------------

const T0: std::time::Duration = std::time::Duration::from_secs(1);

/// `rotate: t * 90deg`: a time-bound value as logic sends it.
fn spin(deg_per_s: f32) -> PropValue {
    PropValue::Token(TokenExpr::Binary {
        op: BinOp::Mul,
        lhs: Box::new(TokenExpr::Time),
        rhs: Box::new(TokenExpr::value(num(deg_per_s))),
    })
}

/// A 200×40 bar: a static square on the left, and a square whose
/// `rotate` follows `t` on the right (the props of the second given).
fn timed_bar(props: Vec<(Prop, PropValue)>) -> (SceneDiff, NodeId, NodeId) {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let still = b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(10.0)),
            (Prop::Y, num(10.0)),
            (Prop::Size, num(20.0)),
            (Prop::Bg, color("#a6e3a1")),
        ],
    );
    let mut own = vec![
        (Prop::X, num(150.0)),
        (Prop::Y, num(10.0)),
        (Prop::Size, num(20.0)),
        (Prop::Bg, color("#f38ba8")),
    ];
    own.extend(props);
    let timed = b.node(NodeKind::Box, Some(root), own);
    (b.diff, still, timed)
}

/// The frame `k` refreshes after `T0` at `hz`.
fn at_hz(hz: u32, k: u32) -> std::time::Duration {
    T0 + std::time::Duration::from_nanos(1_000_000_000 * k as u64 / hz as u64)
}

/// design.md: a time signal repaints only its node, only while visible.
/// The node is evaluated at its own `t` every frame (from its first
/// painted frame), the surface wants every frame while it is drawn, and
/// each frame's damage stays on the node; the partial repaint equals a
/// full one. A quarter second in, the square is turned 22.5° (ref
/// `time_rotate.png`).
#[test]
fn a_time_signal_node_damages_only_itself() {
    let (diff, _, timed) = timed_bar(vec![(Prop::Rotate, spin(90.0))]);
    let mut r = renderer();
    assert!(r.apply(diff).is_empty());
    let root = r.tree().roots()[0];
    r.attach_surface(BAR, root);
    let mut buf = Buffer::new(200, 40, Scale::ONE);
    buf.paint_at(&mut r, BAR, 0, T0);
    let node = drawn_box(&r, timed, (150.0, 10.0), 6);
    for k in 1..=15 {
        assert!(r.wants_frame(BAR), "frame {k}: the clock runs");
        let d = buf.paint_at(&mut r, BAR, 1, at_hz(60, k));
        assert!(!d.is_empty(), "frame {k} repaints the node");
        for rect in d.rects() {
            assert!(
                node.contains_rect(*rect),
                "frame {k}: damage {rect:?} outside the node {node:?}"
            );
        }
    }
    // 15 frames at 60 Hz: t = 0.25 s, 22.5°.
    assert_matches_ref("time_rotate", &buf, 0);
    // The same frame painted afresh: partial repaints add up exactly.
    let (diff, _, _) = timed_bar(vec![(Prop::Rotate, PropValue::Angle(22.5))]);
    let mut full_r = renderer();
    full_r.apply(diff);
    full_r.attach_surface(BAR, full_r.tree().roots()[0]);
    let mut full = Buffer::new(200, 40, Scale::ONE);
    full.paint(&mut full_r, BAR, 0);
    assert!(buf.pixels == full.pixels, "partial differs from full");
}

/// design.md "Paint and light": `border: 2, conic(from: t * 60deg, …)`
/// turns with time and "only the ring repaints": each frame's damage lies
/// in the border's strips, never over the box's middle (where a child
/// sits), and the partial repaints add up to a full paint.
#[test]
fn a_turning_gradient_border_repaints_only_its_ring() {
    let stop = |offset, c: &str| GradientStop {
        offset,
        color: hex(c),
    };
    let border = PropValue::Token(TokenExpr::Template {
        value: Box::new(PropValue::Border(Border {
            width: 2.0,
            paint: Paint::Conic {
                from: 0.0,
                stops: vec![
                    stop(0.0, "#89b4fa"),
                    stop(0.5, "#f5c2e7"),
                    stop(1.0, "#89b4fa"),
                ],
            },
        })),
        colors: vec![],
        numbers: vec![
            None,
            Some(TokenExpr::Binary {
                op: BinOp::Mul,
                lhs: Box::new(TokenExpr::Time),
                rhs: Box::new(TokenExpr::value(num(60.0))),
            }),
        ],
    });
    let scene = |border: PropValue| {
        let mut b = Builder::default();
        let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
        let ring = b.node(
            NodeKind::Box,
            Some(root),
            vec![
                (Prop::Place, PropValue::Keyword("absolute".into())),
                (Prop::X, num(60.0)),
                (Prop::Y, num(10.0)),
                (Prop::Width, num(80.0)),
                (Prop::Height, num(40.0)),
                (Prop::Radius, num(8.0)),
                (Prop::Bg, color("#313244")),
                (Prop::Border, border),
            ],
        );
        b.node(
            NodeKind::Box,
            Some(ring),
            vec![
                (Prop::Place, PropValue::Keyword("absolute".into())),
                (Prop::X, num(30.0)),
                (Prop::Y, num(10.0)),
                (Prop::Size, num(20.0)),
                (Prop::Bg, color("#a6e3a1")),
            ],
        );
        let mut r = renderer();
        assert!(r.apply(b.diff).is_empty());
        r.attach_surface(BAR, r.tree().roots()[0]);
        r
    };
    let mut r = scene(border);
    let mut buf = Buffer::new(200, 60, Scale::ONE);
    buf.paint_at(&mut r, BAR, 0, T0);
    // The middle: the box less its corner radius, border and a margin.
    let middle = Rect::new(60 + 12, 10 + 12, 80 - 24, 40 - 24);
    for k in 1..=15 {
        assert!(r.wants_frame(BAR), "frame {k}: the clock runs");
        let d = buf.paint_at(&mut r, BAR, 1, at_hz(60, k));
        assert!(!d.is_empty(), "frame {k} repaints the ring");
        for rect in d.rects() {
            assert!(
                !rect.intersects(middle),
                "frame {k}: damage {rect:?} over the middle {middle:?}"
            );
        }
    }
    // A quarter second in: the gradient turned 15°.
    let still = PropValue::Border(Border {
        width: 2.0,
        paint: Paint::Conic {
            from: 15.0,
            stops: vec![
                stop(0.0, "#89b4fa"),
                stop(0.5, "#f5c2e7"),
                stop(1.0, "#89b4fa"),
            ],
        },
    });
    let mut full_r = scene(still);
    let mut full = Buffer::new(200, 60, Scale::ONE);
    full.paint(&mut full_r, BAR, 0);
    assert!(buf.pixels == full.pixels, "partial differs from full");
}

/// The box `id` is drawn in at 1× (its laid-out box moved by its `x`,
/// `y`), grown by `m` pixels.
fn drawn_box(r: &Renderer, id: NodeId, (x, y): (f32, f32), m: i32) -> Rect {
    let b = r.boxes(BAR).unwrap().rects[&id];
    let (x0, y0) = ((b.x + x).floor() as i32 - m, (b.y + y).floor() as i32 - m);
    let (x1, y1) = (
        (b.x + x + b.w).ceil() as i32 + m,
        (b.y + y + b.h).ceil() as i32 + m,
    );
    Rect::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32)
}

/// `0.5 + 0.5 * wave(1s)`: an opacity that follows time.
fn pulse() -> PropValue {
    PropValue::Token(TokenExpr::Binary {
        op: BinOp::Add,
        lhs: Box::new(TokenExpr::value(num(0.5))),
        rhs: Box::new(TokenExpr::Binary {
            op: BinOp::Mul,
            lhs: Box::new(TokenExpr::value(num(0.5))),
            rhs: Box::new(TokenExpr::Wave {
                period: std::time::Duration::from_secs(1),
                phase: Box::new(TokenExpr::value(num(0.0))),
            }),
        }),
    })
}

/// A renderer drawing `diff` on `BAR`, its first frame painted at `T0`.
fn clocked(diff: SceneDiff) -> (Renderer, Buffer) {
    let mut r = renderer();
    assert!(r.apply(diff).is_empty());
    let root = r.tree().roots()[0];
    r.attach_surface(BAR, root);
    let mut buf = Buffer::new(200, 40, Scale::ONE);
    buf.paint_at(&mut r, BAR, 0, T0);
    (r, buf)
}

/// design.md: a hidden node's clock stops. Nodes reading time that
/// nothing shows (opacity 0, scale 0, under a hidden ancestor, all they
/// draw outside their parent's clip) ask for no frame and no wake; an
/// opacity that is 0 now but follows time keeps its clock (it comes
/// back), and so does a node outside the clip whose `x` follows time.
#[test]
fn hidden_time_nodes_request_no_frames() {
    let hidden = |extra: Option<Vec<(Prop, PropValue)>>| {
        let mut b = Builder::default();
        let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
        let sq = |x: f32, more: Vec<(Prop, PropValue)>| {
            let mut p = vec![
                (Prop::X, num(x)),
                (Prop::Y, num(10.0)),
                (Prop::Size, num(20.0)),
                (Prop::Bg, color("#f38ba8")),
            ];
            p.extend(more);
            p
        };
        b.node(
            NodeKind::Box,
            Some(root),
            sq(
                10.0,
                vec![(Prop::Rotate, spin(90.0)), (Prop::Opacity, num(0.0))],
            ),
        );
        b.node(
            NodeKind::Box,
            Some(root),
            sq(
                40.0,
                vec![(Prop::Rotate, spin(90.0)), (Prop::Scale, num(0.0))],
            ),
        );
        let gone = b.node(
            NodeKind::Box,
            Some(root),
            sq(70.0, vec![(Prop::Opacity, num(0.0))]),
        );
        b.node(
            NodeKind::Box,
            Some(gone),
            sq(0.0, vec![(Prop::Rotate, spin(90.0))]),
        );
        // A clipping box with a pulsing child drawn wholly outside it.
        let clip = b.node(
            NodeKind::Box,
            Some(root),
            sq(100.0, vec![(Prop::Clip, PropValue::Bool(true))]),
        );
        b.node(
            NodeKind::Box,
            Some(clip),
            sq(60.0, vec![(Prop::Opacity, pulse())]),
        );
        if let Some(extra) = extra {
            let mut p = vec![
                (Prop::Y, num(10.0)),
                (Prop::Size, num(20.0)),
                (Prop::Bg, color("#f38ba8")),
            ];
            p.extend(extra);
            b.node(NodeKind::Box, Some(clip), p);
        }
        b.diff
    };
    let (mut r, mut buf) = clocked(hidden(None));
    assert!(!r.wants_frame(BAR), "no hidden clock asks for a frame");
    assert_eq!(r.next_wake(), None, "nor for a wake");
    buf.paint_at(&mut r, BAR, 1, at_hz(60, 1));
    assert!(!r.wants_frame(BAR) && r.next_wake().is_none());

    // An opacity of `wave(1s)` is 0 at `t = 0`, and comes back.
    let wave = PropValue::Token(TokenExpr::Wave {
        period: std::time::Duration::from_secs(1),
        phase: Box::new(TokenExpr::value(num(0.0))),
    });
    let (mut r, mut buf) = clocked(hidden(Some(vec![(Prop::Opacity, wave)])));
    assert!(
        r.wants_frame(BAR),
        "a hiding opacity that follows time keeps its clock"
    );
    let d = buf.paint_at(&mut r, BAR, 1, at_hz(60, 15));
    assert!(!d.is_empty(), "it shows a quarter second in");

    // Outside the clip, but its `x` follows time: it may come in.
    let (r, _) = clocked(hidden(Some(vec![
        (Prop::Rotate, spin(90.0)),
        (
            Prop::X,
            PropValue::Token(TokenExpr::Binary {
                op: BinOp::Add,
                lhs: Box::new(TokenExpr::value(num(60.0))),
                rhs: Box::new(TokenExpr::Time),
            }),
        ),
    ])));
    assert!(r.wants_frame(BAR), "moving in: its clock runs");
}

/// design.md: an idle shell does zero work. A built-in `effect` draws
/// its raster every frame, so its clock runs while it is drawn; one
/// outside the bar's clip draws nothing, and its clock never starts (the
/// surface asks for no frame and no wake).
#[test]
fn effect_nodes_run_their_clocks_only_while_drawn() {
    for (x, drawn) in [(10.0, true), (1000.0, false)] {
        let mut b = Builder::default();
        let root = b.node(
            NodeKind::Bar,
            None,
            vec![
                (Prop::Bg, color("#1e1e2e")),
                (Prop::Clip, PropValue::Bool(true)),
            ],
        );
        for (i, style) in ["lightning", "sparks", "ripple", "aurora", "shimmer"]
            .into_iter()
            .enumerate()
        {
            b.node(
                NodeKind::Effect,
                Some(root),
                vec![
                    (Prop::X, num(x + 30.0 * i as f32)),
                    (Prop::Y, num(10.0)),
                    (Prop::Size, num(20.0)),
                    (Prop::Style, PropValue::Keyword(style.into())),
                ],
            );
        }
        let (mut r, mut buf) = clocked(b.diff);
        let mut k = 1;
        while r.wants_frame(BAR) && k < 5 {
            buf.paint_at(&mut r, BAR, 1, at_hz(60, k));
            k += 1;
        }
        assert_eq!(r.wants_frame(BAR), drawn, "x = {x}");
        if !drawn {
            assert_eq!(r.next_wake(), None, "no wake either");
        }
    }
}

/// `effect shimmer` (30 fps cap) turning with `t`, and a square turning
/// at refresh, on a fake 60 Hz and 144 Hz output.
fn shimmer_bar(capped: bool) -> (SceneDiff, NodeId) {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let mut props = vec![
        (Prop::X, num(150.0)),
        (Prop::Y, num(10.0)),
        (Prop::Size, num(20.0)),
        (Prop::Bg, color("#f38ba8")),
        (Prop::Rotate, spin(90.0)),
    ];
    if capped {
        // (`speed: 0` holds the shimmer's band still: the pixels compared
        // below are the square's turn.)
        props.push((Prop::Style, PropValue::Keyword("shimmer".into())));
        props.push((Prop::Speed, num(0.0)));
    }
    let kind = if capped {
        NodeKind::Effect
    } else {
        NodeKind::Box
    };
    let id = b.node(kind, Some(root), props);
    (b.diff, id)
}

/// Runs a host loop for half a second of `hz` refreshes from `T0`: a
/// frame each refresh the renderer wants one; otherwise the loop sleeps
/// until [`Renderer::next_wake`], runs `update`, and the frame it then
/// asks for lands on the first refresh after the wake. Returns the
/// presentation times of every frame painted. Each must draw something:
/// a frame the renderer asks for between a capped clock's ticks has
/// empty damage, and fails here rather than going uncounted.
fn host_loop(r: &mut Renderer, buf: &mut Buffer, hz: u32) -> Vec<std::time::Duration> {
    use std::time::{Duration, Instant};
    let end = T0 + Duration::from_millis(500);
    let mut drawn = Vec::new();
    let (mut k, mut last, mut at) = (1, T0, Instant::now());
    while at_hz(hz, k) < end {
        if r.wants_frame(BAR) {
            last = at_hz(hz, k);
            at = Instant::now();
            let damage = buf.paint_at(r, BAR, 1, last);
            assert!(!damage.is_empty(), "frame at {last:?} painted nothing");
            drawn.push(last);
            k += 1;
            continue;
        }
        let wake = r
            .next_wake()
            .expect("a capped clock between ticks waits on a wake");
        std::thread::sleep(wake.saturating_duration_since(Instant::now()));
        r.update();
        assert!(r.wants_frame(BAR), "woken for the tick");
        let due = last + wake.saturating_duration_since(at);
        while at_hz(hz, k) <= due {
            k += 1;
        }
    }
    drawn
}

/// design.md: per-node clocks with frame caps. A refresh clock draws
/// every frame of a 60 Hz and a 144 Hz output; `effect shimmer` draws at
/// 30 fps on both, asking for no frame between its ticks (the loop
/// sleeps until `next_wake`), each tick a whole period of `t` on.
#[test]
fn capped_clocks_paint_at_their_rate() {
    use std::time::Duration;
    for hz in [60, 144] {
        let (diff, _) = shimmer_bar(false);
        let (mut r, mut buf) = clocked(diff);
        let drawn = host_loop(&mut r, &mut buf, hz);
        let frames = (hz / 2 - 1) as usize;
        assert_eq!(
            drawn.len(),
            frames,
            "{hz} Hz: a refresh clock draws every frame"
        );

        let (diff, id) = shimmer_bar(true);
        let (mut r, mut buf) = clocked(diff);
        let drawn = host_loop(&mut r, &mut buf, hz);
        assert!(
            (14..=15).contains(&drawn.len()),
            "{hz} Hz: 30 fps for half a second, drew {}: {drawn:?}",
            drawn.len()
        );
        let tick = Duration::from_nanos(1_000_000_000 / 30);
        let frame = Duration::from_nanos(1_000_000_000 / hz as u64);
        for w in std::iter::once(T0)
            .chain(drawn.iter().copied())
            .collect::<Vec<_>>()
            .windows(2)
        {
            let gap = w[1] - w[0];
            assert!(
                gap + frame > tick && gap < tick + frame,
                "{hz} Hz: a tick {gap:?} after the last"
            );
        }
        // The last frame shows the `t` of the tick nearest it, not the
        // frame's: the same pixels as the square turned by whole ticks.
        let last = *drawn.last().unwrap();
        let ticks = ((last - T0).as_secs_f64() / tick.as_secs_f64()).round() as f32;
        let mut still = renderer();
        let (diff, _) = shimmer_bar(true);
        still.apply(diff);
        still.apply({
            let mut d = SceneDiff::new();
            d.set(id, Prop::Rotate, PropValue::Angle(90.0 * ticks / 30.0));
            d
        });
        still.attach_surface(BAR, still.tree().roots()[0]);
        let mut full = Buffer::new(200, 40, Scale::ONE);
        full.paint(&mut still, BAR, 0);
        assert!(buf.pixels == full.pixels, "{hz} Hz: drawn at tick {ticks}");
    }
}

/// design.md: "The frame loop stops when every clock is idle." A
/// surface with a refresh clock and a capped one wants frames, then
/// waits on wakes once the refresh one is removed, then neither once the
/// capped one is hidden; `reduced_motion` stops every clock too.
#[test]
fn all_idle_clocks_stop_the_frame_loop() {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let fast = b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(10.0)),
            (Prop::Y, num(10.0)),
            (Prop::Size, num(20.0)),
            (Prop::Bg, color("#a6e3a1")),
            (Prop::Rotate, spin(90.0)),
        ],
    );
    let slow = b.node(
        NodeKind::Effect,
        Some(root),
        vec![
            (Prop::X, num(150.0)),
            (Prop::Y, num(10.0)),
            (Prop::Size, num(20.0)),
            (Prop::Bg, color("#f38ba8")),
            (Prop::Rotate, spin(90.0)),
            (Prop::Style, PropValue::Keyword("shimmer".into())),
        ],
    );
    let (mut r, mut buf) = clocked(b.diff);
    assert!(r.wants_frame(BAR), "a refresh clock runs");
    buf.paint_at(&mut r, BAR, 1, at_hz(60, 1));
    assert!(r.wants_frame(BAR));

    // The refresh clock goes: the capped one waits on a wake.
    let mut d = SceneDiff::new();
    d.remove(fast);
    assert!(r.apply(d).is_empty());
    let mut k = 2;
    while r.wants_frame(BAR) {
        buf.paint_at(&mut r, BAR, 1, at_hz(60, k));
        k += 1;
        assert!(k < 20, "the removal settles");
    }
    assert!(r.next_wake().is_some(), "the capped clock still runs");

    // Hidden: nothing runs, no frame and no wake.
    let mut d = SceneDiff::new();
    d.set(slow, Prop::Opacity, num(0.0));
    assert!(r.apply(d).is_empty());
    while r.wants_frame(BAR) {
        buf.paint_at(&mut r, BAR, 1, at_hz(60, k));
        k += 1;
        assert!(k < 40, "hiding settles");
    }
    assert_eq!(r.next_wake(), None, "every clock idle: the loop stops");

    // Shown again under `reduced_motion`: frozen, so still idle.
    r.set_reduced_motion(true);
    let mut d = SceneDiff::new();
    d.set(slow, Prop::Opacity, num(1.0));
    assert!(r.apply(d).is_empty());
    while r.wants_frame(BAR) {
        buf.paint_at(&mut r, BAR, 1, at_hz(60, k));
        k += 1;
        assert!(k < 60, "a frozen clock settles");
    }
    assert_eq!(r.next_wake(), None, "frozen clocks are idle");
    r.set_reduced_motion(false);
    assert!(r.wants_frame(BAR), "motion back: the clock runs again");
}

/// The bar of [`timed_bar`] painted afresh with its timed square turned
/// `deg` degrees, for comparing a clocked frame against.
fn turned(deg: f32) -> Buffer {
    let (diff, _, _) = timed_bar(vec![(Prop::Rotate, PropValue::Angle(deg))]);
    let mut r = renderer();
    r.apply(diff);
    r.attach_surface(BAR, r.tree().roots()[0]);
    let mut full = Buffer::new(200, 40, Scale::ONE);
    full.paint(&mut r, BAR, 0);
    full
}

/// A global token that reads time (`$spin: t * 90deg` in a token set),
/// read through another token (`$turn: $spin`) or through an override
/// (`set { $local: $spin }`), makes its reader frame-driven at the
/// reader's own `t`, as a time value written on the node does; the table
/// stays frozen for every token that does not read time.
#[test]
fn global_tokens_that_read_time_drive_their_readers() {
    let mut table = TokenTable::default();
    table.insert("ok", color("#f38ba8"));
    table.insert("spin", spin(90.0));
    table.insert_derived("turn", TokenExpr::path("spin"));
    table.insert_derived("ok.dim", TokenExpr::path("ok"));
    let paths = table.time_paths();
    assert!(
        paths.contains("spin") && paths.contains("turn"),
        "{paths:?}"
    );
    assert!(!paths.contains("ok") && !paths.contains("ok.dim"));

    let (mut diff, _, timed) = timed_bar(vec![(
        Prop::Rotate,
        PropValue::Token(TokenExpr::path("turn")),
    )]);
    diff.set_tokens(table.clone(), Transition::Instant);
    let (mut r, mut buf) = clocked(diff);
    let node = drawn_box(&r, timed, (150.0, 10.0), 6);
    for k in 1..=15 {
        assert!(
            r.wants_frame(BAR),
            "frame {k}: the token's reader has a clock"
        );
        let d = buf.paint_at(&mut r, BAR, 1, at_hz(60, k));
        assert!(!d.is_empty(), "frame {k} repaints the reader");
        for rect in d.rects() {
            assert!(node.contains_rect(*rect), "frame {k}: {rect:?}");
        }
    }
    // 15 frames at 60 Hz: t = 0.25 s, 22.5°.
    assert!(buf.pixels == turned(22.5).pixels, "drawn at its own t");

    // Through an override of the same table: the subtree reads time.
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let mut local = TokenTable::default();
    local.insert_derived("local", TokenExpr::path("spin"));
    let group = b.node(
        NodeKind::Stack,
        Some(root),
        vec![(Prop::Tokens, PropValue::Tokens(Box::new(local)))],
    );
    b.node(
        NodeKind::Box,
        Some(group),
        vec![
            (Prop::X, num(150.0)),
            (Prop::Y, num(10.0)),
            (Prop::Size, num(20.0)),
            (Prop::Bg, PropValue::Token(TokenExpr::path("ok.dim"))),
            (Prop::Rotate, PropValue::Token(TokenExpr::path("local"))),
        ],
    );
    b.diff.set_tokens(table, Transition::Instant);
    let (mut r, mut buf) = clocked(b.diff);
    let still = buf.pixels.clone();
    for k in 1..=15 {
        assert!(r.wants_frame(BAR), "frame {k}: the override reads time");
        buf.paint_at(&mut r, BAR, 1, at_hz(60, k));
    }
    assert!(buf.pixels != still, "the square turned");
}

/// decisions.md (m4-runtime-w1, F1): `t` counts from the first committed
/// frame that drew the node. A time-reading node hidden by `opacity: 0`
/// draws nothing and starts no clock; shown a second later, it reads
/// `t = 0` there and a quarter second on (`t * 36deg`: 9°), not the time
/// since its first hidden frame.
#[test]
fn a_hidden_node_starts_its_clock_when_it_shows() {
    let (diff, _, timed) = timed_bar(vec![(Prop::Rotate, spin(36.0)), (Prop::Opacity, num(0.0))]);
    let (mut r, mut buf) = clocked(diff);
    assert!(!r.wants_frame(BAR), "hidden: no clock");
    let shown = T0 + std::time::Duration::from_secs(1);
    let mut d = SceneDiff::new();
    d.push(SceneOp::SetProp {
        id: timed,
        prop: Prop::Opacity,
        value: num(1.0),
        transition: Transition::Instant,
    });
    assert!(r.apply(d).is_empty());
    buf.paint_at(&mut r, BAR, 1, shown);
    assert!(buf.pixels == turned(0.0).pixels, "t = 0 when it shows");
    buf.paint_at(
        &mut r,
        BAR,
        1,
        shown + std::time::Duration::from_millis(250),
    );
    assert!(buf.pixels == turned(9.0).pixels, "t = 0.25 s: 9°");
}
