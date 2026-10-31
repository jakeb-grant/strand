//! (wave-cost) A small element that never stops moving costs what it
//! changes: the size a reading sets in a box of fixed size lays out that
//! box and nothing else, a frame no box of which moved flattens nothing and
//! draws nothing, and a host that keeps real time is not even asked for it.

mod common;

use std::time::Duration;

use common::*;
use strand_render::Renderer;
use strand_scene::*;

const S: SurfaceId = SurfaceId(1);

fn frame(k: u32) -> Duration {
    Duration::from_secs(10) + Duration::from_micros(16_667) * k
}

struct Wave {
    r: Renderer,
    buf: Buffer,
    bars: [NodeId; 3],
    /// Every node on the surface.
    nodes: usize,
}

/// A panel with text and boxes around three bars in a `row { height: 14 }`
/// of no width of its own, as an audio visualiser is written. `hold` is what
/// the row holds besides the bars.
fn wave(extra: impl FnOnce(&mut Builder, NodeId)) -> Wave {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(400.0)),
            (Prop::Height, num(40.0)),
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Color, color("#cdd6f4")),
            (Prop::Font, PropValue::Font(font(13.0))),
        ],
    );
    let bar = b.node(NodeKind::Row, Some(root), vec![]);
    let _ = b.node(NodeKind::Text, Some(bar), vec![(Prop::Text, text("Clock"))]);
    for _ in 0..8 {
        b.node(
            NodeKind::Box,
            Some(bar),
            vec![(Prop::Size, num(6.0)), (Prop::Bg, color("#585b70"))],
        );
    }
    let row = b.node(NodeKind::Row, Some(bar), vec![(Prop::Height, num(14.0))]);
    let mut bars = [NodeId::new(0, 0); 3];
    for b_ in &mut bars {
        *b_ = b.node(
            NodeKind::Box,
            Some(row),
            vec![
                (Prop::Width, num(3.0)),
                (Prop::Height, num(4.0)),
                (Prop::Bg, color("#cba6f7")),
            ],
        );
    }
    extra(&mut b, row);
    let nodes = b.diff.ops.len();
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, root);
    let mut buf = Buffer::new(400, 40, Scale::ONE);
    buf.paint_at(&mut r, S, 0, frame(0));
    assert!(!r.wants_frame(S), "at rest");
    Wave {
        r,
        buf,
        bars,
        nodes,
    }
}

fn set_height(w: &mut Wave, bar: usize, h: f32) {
    let mut d = SceneDiff::new();
    d.set(w.bars[bar], Prop::Height, num(h));
    assert!(w.r.apply(d).is_empty());
}

fn height(w: &Wave, bar: usize) -> f32 {
    w.r.boxes(S).unwrap().rects[&w.bars[bar]].h
}

/// A height in a box of fixed height: no preview of the surface when it is
/// set, the row laid out alone (once to learn its target, once in flight),
/// no scene flattened while no box moves, nothing drawn then, and at rest
/// it is the pixels of a surface drawn that way from the start.
#[test]
fn a_size_in_a_fixed_box_costs_that_box_and_only_the_frames_that_move() {
    let mut w = wave(|_, _| {});
    let (flat0, passes0) = (w.r.flattens(), w.r.layout_passes());
    set_height(&mut w, 0, 9.0);
    assert_eq!(w.r.flattens(), flat0, "a preview of the whole surface");
    assert_eq!(w.r.layout_passes(), passes0, "laid out before a frame");

    let mut at = 1;
    let (mut drawn, mut kept) = (0, 0);
    let mut last = height(&w, 0);
    while w.r.wants_frame(S) {
        assert!(at < 200, "never settled");
        let (flat, passes) = (w.r.flattens(), w.r.layout_passes());
        let d = w.buf.paint_at(&mut w.r, S, 1, frame(at));
        let spent = w.r.layout_passes() - passes;
        assert!(spent <= 3, "frame {at}: {spent} passes");
        // Its last frame lands on a whole pass.
        assert!(
            !w.r.wants_frame(S) || w.r.last_layout_nodes() <= 8,
            "frame {at}: laid out {} nodes of {}: the row and its bars, twice, at most",
            w.r.last_layout_nodes(),
            w.nodes,
        );
        let moved = height(&w, 0) != last;
        last = height(&w, 0);
        if !moved && w.r.wants_frame(S) {
            kept += 1;
            assert_eq!(w.r.flattens(), flat, "frame {at}: no box moved");
            assert!(d.is_empty(), "frame {at}: drew {d:?}");
        }
        if moved {
            drawn += 1;
            assert!(!d.is_empty(), "frame {at}: a bar moved and nothing drew");
            assert!(d.area() <= 3 * 14, "frame {at}: drew {d:?}");
        }
        at += 1;
    }
    assert!(drawn >= 3, "{drawn} frames moved the bar");
    assert!(kept >= 1, "{kept} frames kept the scene");
    assert_eq!(height(&w, 0), 9.0);

    // At rest: as a surface that always had it.
    let mut fresh = wave(|_, _| {});
    set_height(&mut fresh, 0, 9.0);
    let mut k = 1;
    while fresh.r.wants_frame(S) {
        fresh.buf.paint_at(&mut fresh.r, S, 1, frame(k * 40));
        k += 1;
    }
    assert_eq!(w.buf.to_rgba(), fresh.buf.to_rgba());
}

/// Many readings in flight keep to the row: each sets its target there.
#[test]
fn readings_in_flight_keep_to_the_box() {
    let mut w = wave(|_, _| {});
    let mut at = 1;
    for i in 0..40u32 {
        set_height(&mut w, (i % 3) as usize, 3.0 + (i * 7 % 11) as f32);
        for _ in 0..3 {
            let passes = w.r.layout_passes();
            w.buf.paint_at(&mut w.r, S, 1, frame(at));
            assert!(w.r.layout_passes() - passes <= 3);
            assert!(
                !w.r.wants_frame(S) || w.r.last_layout_nodes() <= 8,
                "{}",
                w.r.last_layout_nodes()
            );
            at += 1;
        }
    }
    set_height(&mut w, 0, 12.0);
    while w.r.wants_frame(S) {
        assert!(at < 2_000, "never settled");
        w.buf.paint_at(&mut w.r, S, 1, frame(at));
        at += 1;
    }
    assert_eq!(height(&w, 0), 12.0);
}

/// Text in the box turns its width into a height: the whole surface is laid
/// out, as ever.
#[test]
fn text_in_the_box_lays_out_the_surface() {
    let mut w = wave(|b, row| {
        b.node(NodeKind::Text, Some(row), vec![(Prop::Text, text("Hi"))]);
    });
    set_height(&mut w, 0, 9.0);
    w.buf.paint_at(&mut w.r, S, 1, frame(1));
    assert!(w.r.last_layout_nodes() > 8, "{}", w.r.last_layout_nodes());
}

/// What moves as it is flattened is flattened every frame: only sizes hold.
#[test]
fn a_paint_spring_keeps_its_frames() {
    let mut w = wave(|_, _| {});
    set_height(&mut w, 0, 9.0);
    let mut d = SceneDiff::new();
    d.set(w.bars[1], Prop::Bg, color("#f38ba8"));
    assert!(w.r.apply(d).is_empty());
    let mut at = 1;
    while w.r.wants_frame(S) && at < 8 {
        let flat = w.r.flattens();
        w.buf.paint_at(&mut w.r, S, 1, frame(at));
        assert!(w.r.flattens() > flat, "frame {at}");
        at += 1;
    }
}

/// A content-sized panel measures nothing and steps nothing for a size in a
/// fixed box, and shows the same.
#[test]
fn a_content_sized_panel_keeps_to_the_box_too() {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Font, PropValue::Font(font(13.0))),
        ],
    );
    let row = b.node(NodeKind::Row, Some(root), vec![(Prop::Height, num(14.0))]);
    let bar = b.node(
        NodeKind::Box,
        Some(row),
        vec![
            (Prop::Width, num(3.0)),
            (Prop::Height, num(4.0)),
            (Prop::Bg, color("#cba6f7")),
        ],
    );
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, root);
    let size = |r: &Renderer| {
        let spec = r.surface_spec(root).unwrap();
        (spec.width, spec.height)
    };
    let before = size(&r);
    let mut buf = Buffer::new(40, 40, Scale::ONE);
    r.configure_surface(S, Size::new(40, 40), Scale::ONE);
    buf.paint_at(&mut r, S, 0, frame(0));
    let mut d = SceneDiff::new();
    d.set(bar, Prop::Height, num(10.0));
    let passes = r.layout_passes();
    assert!(r.apply(d).is_empty());
    assert_eq!(r.layout_passes(), passes, "laid out before a frame");
    assert_eq!(size(&r), before);
    let mut at = 1;
    while r.wants_frame(S) {
        assert!(at < 200, "never settled");
        buf.paint_at(&mut r, S, 1, frame(at));
        assert!(
            !r.wants_frame(S) || r.last_layout_nodes() <= 4,
            "{}",
            r.last_layout_nodes()
        );
        at += 1;
    }
    assert_eq!(r.boxes(S).unwrap().rects[&bar].h, 10.0);
    assert_eq!(size(&r), before);
}

/// A host that keeps real time is not asked for frames a slow spring cannot
/// show, a change asks at once, a fast spring keeps its refresh, and it
/// lands exactly.
#[test]
fn a_slow_spring_asks_for_the_frames_it_moves_in() {
    let mut w = wave(|_, _| {});
    w.r.set_quiet_frames(true);
    // Fast at first: every frame moves further than a visible step.
    let mut d = SceneDiff::new();
    d.push(SceneOp::SetProp {
        id: w.bars[0],
        prop: Prop::Height,
        value: num(200.0),
        transition: Transition::Spring {
            stiffness: 100.0,
            damping: 1.0,
        },
    });
    assert!(w.r.apply(d).is_empty());
    let mut at = 1;
    w.buf.paint_at(&mut w.r, S, 1, frame(at));
    for _ in 0..4 {
        at += 1;
        assert!(w.r.wants_frame(S), "fast: frame {at} not asked for");
        w.buf.paint_at(&mut w.r, S, 1, frame(at));
    }
    // Its tail: frames are skipped, and painted at any time it lands.
    let mut held = 0;
    let mut painted = 0;
    while w.r.boxes(S).unwrap().rects[&w.bars[0]].h != 200.0 || w.r.frame_deadline(S).is_some() {
        assert!(at < 2_000, "never landed");
        at += 1;
        if w.r.frame_deadline(S).is_some() {
            assert!(!w.r.wants_frame(S), "held and asking");
            held += 1;
            // A real host's timer: it skips the frames it was told to.
            if held % 4 != 0 {
                continue;
            }
            // A change asks at once.
            if held == 4 {
                set_height(&mut w, 1, 6.0);
                assert!(w.r.wants_frame(S), "a change held back");
            }
        }
        w.buf.paint_at(&mut w.r, S, 1, frame(at));
        painted += 1;
    }
    assert!(held >= 2, "{held} frames skipped");
    assert!(painted > 0);
    // Nothing moves: no frames, no deadline.
    at += 40;
    w.buf.paint_at(&mut w.r, S, 1, frame(at));
    assert_eq!(w.r.frame_deadline(S), None);
    assert!(!w.r.wants_frame(S));
    assert_eq!(height(&w, 0), 200.0);
}

/// Without [`Renderer::set_quiet_frames`] every frame is asked for: a host
/// painting while `wants_frame` holds, on its own clock, sees them all.
#[test]
fn a_host_on_its_own_clock_gets_every_frame() {
    let mut w = wave(|_, _| {});
    set_height(&mut w, 0, 5.0);
    let mut at = 1;
    while w.r.wants_frame(S) {
        assert_eq!(w.r.frame_deadline(S), None);
        w.buf.paint_at(&mut w.r, S, 1, frame(at));
        at += 1;
    }
    assert!(at > 5);
    assert_eq!(height(&w, 0), 5.0);
}

/// Its box at half a pixel, a bar laid out alone rounds from where the box
/// was rounded to: where it lands it is laid out whole, so its pixels at rest
/// are those of a surface that always had that height.
#[test]
fn a_box_at_half_a_pixel_lands_where_a_whole_layout_puts_it() {
    let build = |h: f32| {
        let mut b = Builder::default();
        let root = b.node(
            NodeKind::Panel,
            None,
            vec![
                (Prop::Width, num(100.0)),
                (Prop::Height, num(41.0)),
                (Prop::Bg, color("#1e1e2e")),
            ],
        );
        let line = b.node(
            NodeKind::Row,
            Some(root),
            vec![
                (Prop::Grow, num(1.0)),
                (Prop::Align, PropValue::Keyword("center".into())),
            ],
        );
        let row = b.node(
            NodeKind::Row,
            Some(line),
            vec![
                (Prop::Height, num(14.0)),
                (Prop::Align, PropValue::Keyword("center".into())),
            ],
        );
        let bar = b.node(
            NodeKind::Box,
            Some(row),
            vec![
                (Prop::Width, num(3.0)),
                (Prop::Height, num(h)),
                (Prop::Bg, color("#cba6f7")),
            ],
        );
        let mut r = renderer();
        assert!(r.apply(b.diff).is_empty());
        r.attach_surface(S, root);
        let mut buf = Buffer::new(100, 41, Scale::ONE);
        buf.paint_at(&mut r, S, 0, frame(0));
        (r, buf, bar)
    };
    let (mut r, mut buf, bar) = build(4.0);
    let mut d = SceneDiff::new();
    d.set(bar, Prop::Height, num(9.0));
    assert!(r.apply(d).is_empty());
    let mut at = 1;
    while r.wants_frame(S) {
        assert!(at < 200, "never settled");
        buf.paint_at(&mut r, S, 1, frame(at));
        at += 1;
    }
    let (fresh, fresh_buf, fresh_bar) = build(9.0);
    assert_eq!(
        r.boxes(S).unwrap().rects[&bar],
        fresh.boxes(S).unwrap().rects[&fresh_bar]
    );
    assert_eq!(buf.to_rgba(), fresh_buf.to_rgba());
}
