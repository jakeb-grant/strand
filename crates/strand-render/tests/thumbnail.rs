//! (M4) `thumbnail w` (design.md: "Live window thumbnails"): a visible
//! thumbnail asks for its window's frames at its drawn size, draws each
//! frame it is fed by its `fit` with no clock of its own, and stops asking
//! when hidden or gone. Offline PNGs. Regenerate with
//! `STRAND_BLESS=1 cargo test -p strand-render --test thumbnail`.

mod common;

use std::time::Duration;

use common::*;
use strand_render::{FeedDemand, FeedKind, Renderer, ThumbnailFrame};
use strand_scene::*;

const S: SurfaceId = SurfaceId(1);
const TOLERANCE: u8 = 2;
const T0: Duration = Duration::from_secs(1);

fn ms(n: u64) -> Duration {
    T0 + Duration::from_millis(n)
}

fn settle(r: &mut Renderer, buf: &mut Buffer, from: u64) -> u64 {
    let mut at = from;
    while r.wants_frame(S) {
        buf.paint_at(r, S, 1, ms(at));
        at += 16;
        assert!(at < from + 1000, "settles");
    }
    at
}

/// A 64 × 32 window: its left half red, its right half blue, a white
/// band across the middle (premultiplied BGRA, as captures come).
fn window_frame() -> ThumbnailFrame {
    let (w, h) = (64u32, 32u32);
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let p = if (14..18).contains(&y) {
                [255, 255, 255, 255]
            } else if x < w / 2 {
                [0x41, 0x3b, 0xd2, 255]
            } else {
                [0xd2, 0x8a, 0x3b, 255]
            };
            px.extend_from_slice(&p);
        }
    }
    ThumbnailFrame {
        width: w,
        height: h,
        pixels: px.into(),
    }
}

/// Two thumbnails of one window, `contain` and `cover`, in 80 × 80 boxes.
fn scene() -> (Renderer, [NodeId; 2], Buffer) {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(180.0)),
            (Prop::Height, num(100.0)),
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Gap, num(10.0)),
            (Prop::Pad, num(10.0)),
        ],
    );
    let row = b.node(NodeKind::Row, Some(root), vec![(Prop::Gap, num(10.0))]);
    let mut ids = Vec::new();
    for fit in ["contain", "cover"] {
        ids.push(b.node(
            NodeKind::Thumbnail,
            Some(row),
            vec![
                (Prop::Source, text("0x55aa")),
                (Prop::Fit, PropValue::Keyword(fit.into())),
                (Prop::Width, num(80.0)),
                (Prop::Height, num(80.0)),
            ],
        ));
    }
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    (r, [ids[0], ids[1]], Buffer::new(180, 100, Scale::ONE))
}

#[test]
fn a_thumbnail_draws_the_frames_it_is_fed_by_its_fit() {
    let (mut r, ids, mut buf) = scene();
    buf.paint_at(&mut r, S, 0, T0);
    // Visible: both ask for the window, at their drawn size rounded up.
    let d = r.take_feed_demand().expect("demand");
    assert_eq!(
        d,
        ids.iter()
            .map(|id| FeedDemand {
                node: *id,
                kind: FeedKind::Thumbnail {
                    window: "0x55aa".into(),
                    max: (128, 128),
                },
            })
            .collect::<Vec<_>>()
    );
    // Nothing yet: no clock, nothing drawn.
    assert!(!r.wants_frame(S), "no clock");
    assert_eq!(buf.px(50, 50), buf.px(5, 5), "nothing before a frame");

    for id in ids {
        r.feed_frame(id, Some(window_frame()));
    }
    assert!(r.wants_frame(S), "a frame repaints");
    let at = settle(&mut r, &mut buf, 16);
    assert_matches_ref("thumbnail_fits", &buf, TOLERANCE);
    assert!(!r.wants_frame(S), "still between frames");
    assert_eq!(r.take_feed_demand(), None, "unchanged");

    // The window is gone: it draws nothing.
    r.feed_frame(ids[0], None);
    settle(&mut r, &mut buf, at);
    assert_eq!(buf.px(50, 50), buf.px(5, 5));
}

#[test]
fn only_visible_thumbnails_ask_for_frames() {
    let (mut r, ids, mut buf) = scene();
    buf.paint_at(&mut r, S, 0, T0);
    assert_eq!(r.take_feed_demand().map(|d| d.len()), Some(2));
    let mut d = SceneDiff::default();
    d.set(ids[1], Prop::Opacity, num(0.0));
    assert!(r.apply(d).is_empty());
    let at = settle(&mut r, &mut buf, 16);
    let d = r.take_feed_demand().expect("changed");
    assert_eq!(d.iter().map(|d| d.node).collect::<Vec<_>>(), [ids[0]]);
    // Shrunk past the step: asked for at the new size.
    let mut d = SceneDiff::default();
    d.set(ids[0], Prop::Width, num(40.0));
    assert!(r.apply(d).is_empty());
    let at = settle(&mut r, &mut buf, at);
    let d = r.take_feed_demand().expect("smaller");
    assert!(
        matches!(&d[0].kind, FeedKind::Thumbnail { max: (64, 128), .. }),
        "{d:?}"
    );
    // No window: not asked for.
    let mut d = SceneDiff::default();
    d.set(ids[0], Prop::Source, text(""));
    assert!(r.apply(d).is_empty());
    settle(&mut r, &mut buf, at);
    assert_eq!(r.take_feed_demand(), Some(Vec::new()));
    // A frame for a node that is no thumbnail does nothing.
    r.feed_frame(NodeId::new(999, 0), Some(window_frame()));
}
