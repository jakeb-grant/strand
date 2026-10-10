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
    set(&mut r, g, Prop::Value, num(0.5));
    let before = r.graph_columns_drawn(g).unwrap();
    buf.paint_at(&mut r, S, 1, ms(1130));
    assert_eq!(r.graph_columns_drawn(g).unwrap() - before, 1);
    assert_matches_ref("graph_history", &buf, TOLERANCE);
    // The left part has no history yet: background shows there.
    let [b, gr, rd, _] = buf.px(rect.x as u32 + 2, rect.y as u32 + 28);
    assert_eq!((rd, gr, b), (0x1e, 0x1e, 0x2e));
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
