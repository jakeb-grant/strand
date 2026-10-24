//! (M4) `graph` and `spectrum` (design.md, "Generative, data-driven and
//! media"): a graph keeps its history and repaints only its new column on
//! each tick; a spectrum draws the bands it is fed in four styles, has no
//! clock, and asks for feeds only while visible. Offline PNGs at fixed
//! times. Regenerate with `STRAND_BLESS=1 cargo test -p strand-render --test media`.

mod common;

use std::time::Duration;

use common::*;
use strand_render::{FeedDemand, FeedKind, Renderer};
use strand_scene::*;

const S: SurfaceId = SurfaceId(1);
const TOLERANCE: u8 = 2;
const T0: Duration = Duration::from_secs(1);

/// (history) The boot clock graphs read in these tests, seconds: its
/// time at `T0`.
const BOOT: f64 = 500.0;

/// (graph-follow) The boot clock at a presentation time in these tests:
/// `BOOT` at `T0`, so 100 ms columns start at `ms(0)`, `ms(100)`, ….
fn boot(p: Duration) -> Duration {
    (p + Duration::from_secs_f64(BOOT)).saturating_sub(T0)
}

fn ms(n: u64) -> Duration {
    T0 + Duration::from_millis(n)
}

/// Paints until nothing more is wanted, from `ms(from)` a frame each 16 ms.
fn settle(r: &mut Renderer, buf: &mut Buffer, from: u64) -> u64 {
    let mut at = from;
    while r.wants_frame(S) {
        buf.paint_at(r, S, 1, ms(at));
        at += 16;
        assert!(at < from + 1000, "settles");
    }
    at
}

fn set(r: &mut Renderer, id: NodeId, prop: Prop, v: PropValue) {
    let mut d = SceneDiff::default();
    d.set(id, prop, v);
    assert!(r.apply(d).is_empty());
}

/// A 60 × 30 graph over 6 s: 60 columns of 100 ms.
fn graph_scene() -> (Renderer, NodeId, Buffer) {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Bar,
        None,
        vec![
            (Prop::Width, num(80.0)),
            (Prop::Height, num(40.0)),
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Pad, num(5.0)),
        ],
    );
    let g = b.node(
        NodeKind::Graph,
        Some(root),
        vec![
            (Prop::Width, num(60.0)),
            (Prop::Height, num(30.0)),
            (Prop::Value, num(0.2)),
            (Prop::History, PropValue::Duration(Duration::from_secs(6))),
            (Prop::Color, color("#89b4fa")),
            (Prop::Fill, color("#89b4fa55")),
        ],
    );
    let mut r = renderer();
    r.set_history_clock(boot);
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    (r, g, Buffer::new(80, 40, Scale::ONE))
}

#[test]
fn a_graph_keeps_its_history_and_draws_only_new_columns() {
    let (mut r, g, mut buf) = graph_scene();
    buf.paint_at(&mut r, S, 0, T0);
    // The first frame learns the width; the clock then ticks per column.
    buf.paint_at(&mut r, S, 1, ms(16));
    assert!(!r.wants_frame(S), "between ticks no frame");
    assert!(r.next_wake().is_some(), "woken at the next column");
    // A rising value over a second, one step a column.
    for k in 1..=10u64 {
        set(&mut r, g, Prop::Value, num(0.2 + 0.06 * k as f32));
        buf.paint_at(&mut r, S, 1, ms(100 * k));
    }
    let before = r.graph_columns_drawn(g).unwrap();
    let damage = buf.paint_at(&mut r, S, 1, ms(1100));
    let drawn = r.graph_columns_drawn(g).unwrap() - before;
    assert_eq!(
        drawn, 2,
        "one tick draws the new column and the last live one"
    );
    let rect = r.boxes(S).unwrap().rects[&g];
    let gbox = (rect.x as i32, rect.y as i32, rect.w as i32, rect.h as i32);
    for d in damage.rects() {
        assert!(
            d.x >= gbox.0 && d.y >= gbox.1 && d.x + d.w as i32 <= gbox.0 + gbox.2,
            "damage {d:?} outside the graph {gbox:?}"
        );
    }
    // A value change between ticks redraws only the newest column.
    let before = r.graph_columns_drawn(g).unwrap();
    set(&mut r, g, Prop::Value, num(0.5));
    buf.paint_at(&mut r, S, 1, ms(1130));
    assert_eq!(r.graph_columns_drawn(g).unwrap() - before, 1);
    assert_matches_ref("graph_history", &buf, TOLERANCE);
    // The left part has no history yet: background shows there.
    let [b, gr, rd, _] = buf.px(rect.x as u32 + 2, rect.y as u32 + 28);
    assert_eq!((rd, gr, b), (0x1e, 0x1e, 0x2e));
}

/// (history) A graph mounting onto the history logic kept of its value
/// (`Prop::Backlog`) shows it at once: over 6 s, a shown second of steps
/// (0.3, then 0.5, then 0.4), then a hidden span sampled every 2 s drawn
/// as lines between its samples (up to 0.9, down to 0.2), and the value
/// now. Nothing is left empty, and the next tick still draws only the
/// new column and the last live one.
#[test]
fn a_graph_mounting_onto_its_backlog_shows_it_with_hidden_spans_as_lines() {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Bar,
        None,
        vec![
            (Prop::Width, num(80.0)),
            (Prop::Height, num(40.0)),
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Pad, num(5.0)),
        ],
    );
    // Timed on the boot clock: 500 s at the first draw.
    let point = |age: f64, value, ramp| BacklogPoint {
        at: BOOT - age,
        value,
        ramp,
    };
    let backlog = Backlog {
        points: vec![
            point(6.5, 0.3, 0.0),
            point(5.0, 0.5, 0.0),
            point(4.0, 0.4, 0.0),
            // Hidden: one sample every 2 s, each a line from the last.
            point(2.0, 0.9, 2.0),
            point(0.0, 0.2, 2.0),
        ],
    };
    let g = b.node(
        NodeKind::Graph,
        Some(root),
        vec![
            (Prop::Width, num(60.0)),
            (Prop::Height, num(30.0)),
            (Prop::Value, num(0.25)),
            (Prop::History, PropValue::Duration(Duration::from_secs(6))),
            (Prop::Color, color("#89b4fa")),
            (Prop::Fill, color("#89b4fa55")),
            (
                Prop::Backlog,
                PropValue::Backlog(std::sync::Arc::new(backlog)),
            ),
        ],
    );
    let mut r = renderer();
    r.set_history_clock(boot);
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(80, 40, Scale::ONE);
    buf.paint_at(&mut r, S, 0, T0);
    buf.paint_at(&mut r, S, 1, ms(16));
    assert_matches_ref("graph_backlog", &buf, TOLERANCE);
    // The oldest column is drawn from the backlog, not left empty.
    let rect = r.boxes(S).unwrap().rects[&g];
    let [bl, gr, rd, _] = buf.px(rect.x as u32 + 1, rect.y as u32 + 28);
    assert_ne!((rd, gr, bl), (0x1e, 0x1e, 0x2e), "history at the left edge");
    // Columns it samples itself are its own: a tick draws two.
    set(&mut r, g, Prop::Value, num(0.3));
    buf.paint_at(&mut r, S, 1, ms(100));
    set(&mut r, g, Prop::Value, num(0.35));
    let before = r.graph_columns_drawn(g).unwrap();
    buf.paint_at(&mut r, S, 1, ms(200));
    assert_eq!(r.graph_columns_drawn(g).unwrap() - before, 2);
}

#[test]
fn a_hidden_graph_has_no_clock() {
    let (mut r, g, mut buf) = graph_scene();
    buf.paint_at(&mut r, S, 0, T0);
    buf.paint_at(&mut r, S, 1, ms(16));
    set(&mut r, g, Prop::Opacity, num(0.0));
    settle(&mut r, &mut buf, 30);
    assert_eq!(r.next_wake(), None);
}

/// (graph-follow) A column of 60 × 24 graphs over 6 s (100 ms columns),
/// one per entry of `each` (its props; `#89b4fa` unless it sets a
/// colour), on a dark panel.
fn graphs(each: &[Vec<(Prop, PropValue)>]) -> (Renderer, Vec<NodeId>, Buffer) {
    let mut b = Builder::default();
    let h = 10 + 30 * each.len() as u32;
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(76.0)),
            (Prop::Height, num(h as f32)),
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Pad, num(8.0)),
        ],
    );
    let col = b.node(NodeKind::Col, Some(root), vec![(Prop::Gap, num(6.0))]);
    let ids = each
        .iter()
        .map(|props| {
            let mut p = vec![
                (Prop::Width, num(60.0)),
                (Prop::Height, num(24.0)),
                (Prop::History, PropValue::Duration(Duration::from_secs(6))),
            ];
            if !props.iter().any(|(q, _)| *q == Prop::Color) {
                p.push((Prop::Color, color("#89b4fa")));
            }
            p.extend(props.iter().cloned());
            b.node(NodeKind::Graph, Some(col), p)
        })
        .collect();
    let mut r = renderer();
    r.set_history_clock(boot);
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    (r, ids, Buffer::new(76, h, Scale::ONE))
}

/// A wave between 0 and 1 over ticks.
fn wave(k: u64) -> f32 {
    0.5 + 0.4 * (k as f32 * 0.45).sin() * (k as f32 * 0.11).cos()
}

/// Paints 45 ticks of the graphs: `value(i, k)` set on graph `i` in the
/// middle of tick `k` and painted, then the next tick painted with no
/// change. Returns the most slots each drew at one paint after the
/// first ticks.
fn feed(
    r: &mut Renderer,
    ids: &[NodeId],
    buf: &mut Buffer,
    value: impl Fn(usize, u64) -> PropValue,
) -> Vec<u64> {
    let mut most = vec![0; ids.len()];
    let drawn = |r: &Renderer| -> Vec<u64> {
        ids.iter()
            .map(|g| r.graph_columns_drawn(*g).unwrap_or(0))
            .collect()
    };
    for k in 0..45u64 {
        for (i, g) in ids.iter().enumerate() {
            set(r, *g, Prop::Value, value(i, k));
        }
        for at in [100 * k + 50, 100 * k + 110] {
            let before = drawn(r);
            buf.paint_at(r, S, if at == 50 { 0 } else { 1 }, ms(at));
            if k >= 2 {
                for ((m, now), was) in most.iter_mut().zip(drawn(r)).zip(before) {
                    *m = (*m).max(now - was);
                }
            }
        }
    }
    most
}

fn list(items: impl IntoIterator<Item = PropValue>) -> PropValue {
    PropValue::List(items.into_iter().collect())
}

fn kw(k: &str) -> PropValue {
    PropValue::Keyword(k.into())
}

fn fade(angle: f32, from: &str, to: Color) -> PropValue {
    PropValue::Paint(Paint::Linear {
        angle,
        stops: vec![
            GradientStop {
                offset: 0.0,
                color: hex(from),
            },
            GradientStop {
                offset: 1.0,
                color: to,
            },
        ],
    })
}

/// (graph-follow) `style: line`, `steps` and `bars` (2 px bars 1 px
/// apart, and 3 px bars with no gap), over the same values; each tick
/// draws only its new slot and the last live one.
#[test]
fn a_graph_draws_as_a_line_steps_or_bars() {
    let (mut r, ids, mut buf) = graphs(&[
        vec![(Prop::Fill, color("#89b4fa44"))],
        vec![
            (Prop::Style, kw("steps")),
            (Prop::Color, color("#a6e3a1")),
            (Prop::Fill, color("#a6e3a144")),
        ],
        vec![(Prop::Style, kw("bars")), (Prop::Color, color("#f9e2af"))],
        vec![
            (Prop::Style, kw("bars")),
            (Prop::Color, color("#f38ba8")),
            (Prop::LineWidth, num(3.0)),
            (Prop::Gap, num(0.0)),
        ],
    ]);
    let drawn = feed(&mut r, &ids, &mut buf, |_, k| num(wave(k)));
    assert_eq!(drawn, [2, 2, 2, 2], "only the new slots repaint");
    assert_matches_ref("graph_styles", &buf, TOLERANCE);
}

/// (graph-follow) `range`: fixed (`0, 100`, a value past it drawn at
/// the top), `auto` (fitted on the 1, 2, 5 ladder), `50, auto` and `auto`
/// over values below zero. A value that grows an auto range redraws
/// every column; one that fits it, only the new ones.
#[test]
fn a_graph_reads_its_values_against_its_range() {
    let (mut r, ids, mut buf) = graphs(&[
        vec![(Prop::Range, list([num(0.0), num(100.0)]))],
        vec![(Prop::Range, kw("auto"))],
        vec![(Prop::Range, list([num(50.0), kw("auto")]))],
        vec![(Prop::Range, kw("auto")), (Prop::Fill, color("#89b4fa44"))],
    ]);
    let drawn = feed(&mut r, &ids, &mut buf, |i, k| {
        let w = wave(k);
        num(match i {
            0 => 130.0 * w,
            1 => 300.0 + 500.0 * w,
            2 => 55.0 + 40.0 * w,
            _ => -3.0 + 7.0 * w,
        })
    });
    assert_eq!(drawn, [2, 2, 2, 2]);
    assert_matches_ref("graph_range", &buf, TOLERANCE);
    // Past the auto top: it grows at once, and every column is drawn
    // against it.
    let before = r.graph_columns_drawn(ids[1]).unwrap();
    set(&mut r, ids[1], Prop::Value, num(4000.0));
    buf.paint_at(&mut r, S, 1, ms(4520));
    assert_eq!(r.graph_columns_drawn(ids[1]).unwrap() - before, 60);
}

/// (graph-follow) A gradient `fill` is drawn as written: a fade from the
/// line down to nothing (straight down, so kept columns still move), and
/// one across the box (drawn whole at each tick); `line_width` 3 and
/// 0.5 (a pixel at least).
#[test]
fn a_graph_draws_a_gradient_fill_and_its_line_width() {
    let accent = hex("#89b4fa");
    let (mut r, ids, mut buf) = graphs(&[
        vec![(Prop::Fill, fade(180.0, "#89b4fa", accent.with_alpha(0.0)))],
        vec![
            (Prop::Color, color("#ffffff")),
            (Prop::Fill, fade(90.0, "#f38ba8", hex("#89b4fa"))),
        ],
        vec![(Prop::LineWidth, num(3.0))],
        vec![(Prop::LineWidth, num(0.5))],
    ]);
    let drawn = feed(&mut r, &ids, &mut buf, |_, k| num(wave(k)));
    assert_eq!(drawn, [2, 60, 2, 2], "a fill across the box draws it whole");
    assert_matches_ref("graph_fill", &buf, TOLERANCE);
    // The fade: stronger under the line than at the bottom, where the
    // panel all but shows through.
    let rect = r.boxes(S).unwrap().rects[&ids[0]];
    let at = |y: f32| buf.px(rect.x as u32 + 58, (rect.y + y) as u32)[0];
    let (mid, bottom) = (at(16.0), at(rect.h - 1.0));
    assert!(mid > bottom + 20, "{mid} over {bottom}");
    assert!(bottom.abs_diff(0x2e) <= 8, "{bottom}");
}

/// (graph-follow) A list value is several series, drawn over one another
/// against one range, the first at the back: two lines with a colour and
/// a fill each, three series over two colours (taken in turn), two bars
/// overlaid, and steps under one colour for both.
#[test]
fn a_graph_draws_several_series() {
    let (mut r, ids, mut buf) = graphs(&[
        vec![
            (Prop::Color, list([color("#89b4fa"), color("#f38ba8")])),
            (Prop::Fill, list([color("#89b4fa33"), color("#f38ba833")])),
        ],
        vec![(Prop::Color, list([color("#a6e3a1"), color("#f9e2af")]))],
        vec![
            (Prop::Style, kw("bars")),
            (Prop::Color, list([color("#89b4fa"), color("#f38ba8")])),
        ],
        vec![(Prop::Style, kw("steps")), (Prop::Color, color("#cba6f7"))],
    ]);
    let drawn = feed(&mut r, &ids, &mut buf, |i, k| {
        let (a, b) = (wave(k), wave(k + 9));
        match i {
            1 => list([num(a), num(b), num(0.5 * (a + b))]),
            2 => list([num(0.5 * (a + b)), num(0.5 * b)]),
            _ => list([num(a), num(b)]),
        }
    });
    assert_eq!(drawn, [2, 2, 2, 2]);
    assert_matches_ref("graph_series", &buf, TOLERANCE);
}

/// (graph-follow) A graph's columns are whole periods of the boot clock:
/// two shells drawing the same values, one at 60 Hz from its mount and
/// one at 24 Hz from 23 ms later, draw the same columns, the steps in
/// the same places. (Columns timed from each one's first frame put them
/// a pixel apart, as a replay drew them before.)
#[test]
fn a_graphs_columns_fall_on_the_boot_clock_whatever_its_frames() {
    let changes = [
        (0u64, 0.2f32),
        (250, 0.7),
        (730, 0.4),
        (1180, 0.9),
        (1640, 0.1),
        (2050, 0.5),
    ];
    let value = |at: u64| {
        changes
            .iter()
            .rev()
            .find(|(t, _)| *t <= at)
            .map_or(0.0, |c| c.1)
    };
    let draw = |period: u64, first: u64| {
        let (mut r, ids, mut buf) = graphs(&[vec![(Prop::Fill, color("#89b4fa55"))]]);
        let mut at = first;
        while at < 2500 {
            set(&mut r, ids[0], Prop::Value, num(value(at)));
            buf.paint_at(&mut r, S, if at == first { 0 } else { 1 }, ms(at));
            at += period;
        }
        set(&mut r, ids[0], Prop::Value, num(value(2500)));
        buf.paint_at(&mut r, S, 1, ms(2500));
        buf
    };
    let (a, b) = (draw(16, 0), draw(41, 23));
    let diff = a
        .to_rgba()
        .iter()
        .zip(b.to_rgba())
        .filter(|(x, y)| **x != *y)
        .count();
    assert_eq!(diff, 0, "the same columns");
}

/// (graph-follow) A frame with no clock (time zero, as a shell's first
/// can be) stands nowhere on the boot clock, so it places no columns: a
/// graph whose first frame had none draws, from its first frame with a
/// clock, what one whose first frame had a clock draws. (Placed at the
/// boot clock's time zero, the ticks from it to the next frame were each
/// filled with the value: the e2e report's live shell drew a flat line
/// across a graph 3 s old that its replay did not.)
#[test]
fn a_frame_with_no_clock_places_no_columns() {
    let draw = |clockless: bool| {
        let (mut r, ids, mut buf) = graphs(&[vec![(Prop::Fill, color("#89b4fa55"))]]);
        set(&mut r, ids[0], Prop::Value, num(0.3));
        if clockless {
            buf.paint_at(&mut r, S, 0, Duration::ZERO);
        }
        let mut at = 0;
        while at <= 400 {
            if at == 256 {
                set(&mut r, ids[0], Prop::Value, num(0.8));
            }
            let age = u8::from(at > 0 || clockless);
            buf.paint_at(&mut r, S, age, ms(at));
            at += 16;
        }
        buf
    };
    let (a, b) = (draw(true), draw(false));
    // The columns the line lights: the graph is 60 px from x 8, rows 8
    // to 32, its line #89b4fa over #1e1e2e.
    let lit = |buf: &Buffer| {
        (8..68)
            .filter(|&x| (8..32).any(|y| buf.px(x, y)[0] > 0x90))
            .count()
    };
    assert!(
        (1..=8).contains(&lit(&b)),
        "a graph 400 ms old shows a few columns, not {}",
        lit(&b)
    );
    let diff = a
        .to_rgba()
        .iter()
        .zip(b.to_rgba())
        .filter(|(x, y)| **x != *y)
        .count();
    assert_eq!(diff, 0, "the same columns");
}

const STYLES: [&str; 4] = ["bars", "mirror", "wave", "line"];

/// Four 96 × 24 spectra of 12 bars, one per style, all of device 40.
fn spectrum_scene() -> (Renderer, Vec<NodeId>, Buffer) {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(112.0)),
            (Prop::Height, num(128.0)),
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Pad, num(8.0)),
            (Prop::Gap, num(8.0)),
        ],
    );
    let col = b.node(NodeKind::Col, Some(root), vec![(Prop::Gap, num(6.0))]);
    let ids = STYLES
        .iter()
        .map(|style| {
            b.node(
                NodeKind::Spectrum,
                Some(col),
                vec![
                    (Prop::Source, text("40")),
                    (Prop::Width, num(96.0)),
                    (Prop::Height, num(24.0)),
                    (Prop::Bars, num(12.0)),
                    (Prop::Smooth, num(0.0)),
                    (Prop::Style, PropValue::Keyword((*style).into())),
                    (Prop::Color, color("#a6e3a1")),
                ],
            )
        })
        .collect();
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    (r, ids, Buffer::new(112, 128, Scale::ONE))
}

/// 64 bands with a bump in the low middle.
fn bump() -> Vec<f32> {
    (0..64)
        .map(|i| {
            let x = (i as f32 - 20.0) / 12.0;
            (0.95 * (-x * x).exp()).max(0.05)
        })
        .collect()
}

#[test]
fn a_spectrum_draws_its_bands_in_four_styles_when_fed() {
    let (mut r, ids, mut buf) = spectrum_scene();
    buf.paint_at(&mut r, S, 0, T0);
    assert!(!r.wants_frame(S), "no clock: nothing until fed");
    assert_eq!(r.next_wake(), None);
    // Visible: each asks for its device's bands.
    let demand = r.take_feed_demand().expect("a first demand");
    assert_eq!(demand.len(), 4);
    assert!(demand.iter().all(|d| d.kind
        == FeedKind::Spectrum {
            device: "40".into()
        }));
    assert_eq!(r.take_feed_demand(), None, "unchanged");
    for id in &ids {
        r.feed(*id, &bump());
    }
    assert!(r.wants_frame(S), "a feed repaints");
    let d = buf.paint_at(&mut r, S, 1, ms(16));
    assert!(!d.is_empty());
    assert!(!r.wants_frame(S));
    assert_matches_ref("spectrum_styles", &buf, TOLERANCE);
    // Silence: the bars rest at once.
    for id in &ids {
        r.feed(*id, &[]);
    }
    buf.paint_at(&mut r, S, 1, ms(32));
    assert_matches_ref("spectrum_rest", &buf, TOLERANCE);
}

#[test]
fn only_visible_spectra_are_fed() {
    let (mut r, ids, mut buf) = spectrum_scene();
    buf.paint_at(&mut r, S, 0, T0);
    assert_eq!(r.take_feed_demand().map(|d| d.len()), Some(4));
    set(&mut r, ids[1], Prop::Opacity, num(0.0));
    let at = settle(&mut r, &mut buf, 16);
    let d = r.take_feed_demand().expect("changed");
    assert_eq!(
        d.iter().map(|d| d.node).collect::<Vec<_>>(),
        [ids[0], ids[2], ids[3]]
    );
    // A spectrum logic removes stops being fed.
    let mut diff = SceneDiff::default();
    diff.remove(ids[0]);
    assert!(r.apply(diff).is_empty());
    settle(&mut r, &mut buf, at);
    assert_eq!(r.take_feed_demand().map(|d| d.len()), Some(2));
    // A feed for a node that is gone does nothing.
    r.feed(ids[0], &bump());
    let _: Option<Vec<FeedDemand>> = r.take_feed_demand();
}

/// The demand follows a node removed and a surface detached before any
/// paint: the host asks then (`run/feeds.rs`, `sync`), so a producer
/// stops even when no surface paints again.
#[test]
fn demand_drops_without_a_paint() {
    let (mut r, ids, mut buf) = spectrum_scene();
    buf.paint_at(&mut r, S, 0, T0);
    assert_eq!(r.take_feed_demand().map(|d| d.len()), Some(4));
    let mut diff = SceneDiff::default();
    diff.remove(ids[0]);
    assert!(r.apply(diff).is_empty());
    assert_eq!(r.take_feed_demand().map(|d| d.len()), Some(3), "removed");
    r.detach_surface(S);
    assert_eq!(r.take_feed_demand(), Some(Vec::new()), "detached");
    assert_eq!(r.take_feed_demand(), None);
}

#[test]
fn reduced_motion_rests_the_spectrum_and_stops_its_feeds() {
    let (mut r, ids, mut buf) = spectrum_scene();
    buf.paint_at(&mut r, S, 0, T0);
    assert!(r.take_feed_demand().is_some());
    for id in &ids {
        r.feed(*id, &bump());
    }
    buf.paint_at(&mut r, S, 1, ms(16));
    r.set_reduced_motion(true);
    assert_eq!(r.take_feed_demand(), Some(Vec::new()), "no feeds");
    buf.paint_at(&mut r, S, 1, ms(32));
    assert_matches_ref("spectrum_rest", &buf, TOLERANCE);
}
