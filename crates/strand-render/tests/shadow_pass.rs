//! (shadow-pass, decisions.md) The clip a box sets because its size
//! springs (a content-sized panel's growth, a `height` that springs)
//! cuts what spills past the box, not the shadow of a box that lies
//! inside it: that shadow is drawn whole at rest, where no such clip is,
//! so it is drawn whole while the size springs too. Before, the docs
//! launcher's card lost its shadow beside, above and below it on every
//! frame its panel shrank (all but the corners its rounding left), and
//! it came back at rest. A box that spills past the springing box is
//! still cut, its shadow with it. Regenerate the reference with
//! `STRAND_BLESS=1 cargo test -p strand-render --test shadow_pass`.

mod common;

use std::time::Duration;

use common::*;
use strand_render::Renderer;
use strand_scene::*;

const T0: Duration = Duration::from_secs(1);
const S: SurfaceId = SurfaceId(1);

fn frame(k: u32) -> Duration {
    T0 + Duration::from_micros(16_667 * u64::from(k))
}

fn kw(k: &str) -> PropValue {
    PropValue::Keyword(k.into())
}

/// A shadow all round its box: no offset, 16 px of blur.
fn shadow() -> PropValue {
    PropValue::Shadow(vec![Shadow {
        x: 0.0,
        y: 0.0,
        blur: 16.0,
        spread: 0.0,
        color: hex("#000000cc"),
    }])
}

/// The alpha of `buf` at logical `(x, y)`, out of the buffer 0.
fn alpha(buf: &Buffer, (x, y): (f32, f32)) -> u8 {
    let s = buf.scale.as_f32();
    let (x, y) = ((x * s).floor(), (y * s).floor());
    if x < 0.0 || y < 0.0 || x >= buf.size.w as f32 || y >= buf.size.h as f32 {
        return 0;
    }
    buf.px(x as u32, y as u32)[3]
}

/// Where to read a shadow `d` px outside box `r`: above, below, left of
/// and right of its middle.
fn around(r: LogicalRect, d: f32) -> [(f32, f32); 4] {
    let (cx, cy) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
    [
        (cx, r.y - d),
        (cx, r.y + r.h + d),
        (r.x - d, cy),
        (r.x + r.w + d, cy),
    ]
}

/// A centred, content-sized panel holding the launcher's card (a column
/// with a background, rounded corners and a shadow, no `clip`), which
/// holds `rows` rows 184 by 30. Returns the diff, the panel, the card
/// and the rows.
fn launcher(rows: u32) -> (SceneDiff, NodeId, NodeId, Vec<NodeId>) {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Panel, None, vec![(Prop::Anchor, kw("center"))]);
    let card = b.node(
        NodeKind::Col,
        Some(root),
        vec![
            (Prop::Width, num(200.0)),
            (Prop::Pad, num(8.0)),
            (Prop::Gap, num(4.0)),
            (Prop::Radius, num(12.0)),
            (Prop::Bg, color("#ffffff")),
            (Prop::Shadow, shadow()),
        ],
    );
    let rows = (0..rows)
        .map(|_| {
            b.node(
                NodeKind::Box,
                Some(card),
                vec![(Prop::Height, num(30.0)), (Prop::Bg, color("#ff0000"))],
            )
        })
        .collect();
    (b.diff, root, card, rows)
}

/// Applies `d` and configures the panel's surface at the size its spec
/// asks for, its overhang included, as the surface manager does
/// (scale 1).
fn apply(r: &mut Renderer, buf: &mut Option<Buffer>, root: NodeId, d: SceneDiff) {
    assert!(r.apply(d).is_empty());
    r.update();
    while r.has_surface_changes() {
        for (node, change) in r.take_surface_changes() {
            let (SurfaceChange::Created(spec) | SurfaceChange::Updated { spec, .. }) = change
            else {
                continue;
            };
            if node != root {
                continue;
            }
            let o = spec.overhang;
            let size = Size::new(
                (spec.width.unwrap_or(1.0) + o.left + o.right).floor() as u32,
                (spec.height.unwrap_or(1.0) + o.top + o.bottom).floor() as u32,
            );
            match buf {
                None => {
                    r.attach_surface(S, root);
                    r.configure_surface(S, size, Scale::ONE);
                    *buf = Some(Buffer::new(size.w, size.h, Scale::ONE));
                }
                Some(b) if b.size != size => {
                    r.configure_surface(S, size, Scale::ONE);
                    *b = Buffer::new(size.w, size.h, Scale::ONE);
                }
                Some(_) => {}
            }
        }
    }
}

/// Paints frames from `k` on, each over the last, until the surface
/// settles, configuring it as it asks; returns each frame's buffer and
/// the panel's and card's boxes in it, and the next frame.
fn frames(
    r: &mut Renderer,
    buf: &mut Option<Buffer>,
    root: NodeId,
    card: NodeId,
    mut k: u32,
) -> (Vec<(Buffer, LogicalRect, LogicalRect)>, u32) {
    let mut out = Vec::new();
    while r.wants_frame(S) {
        let b = buf.as_mut().expect("attached");
        b.paint_at(r, S, 1, frame(k));
        let boxes = r.boxes(S).expect("laid out");
        out.push((b.clone(), boxes.rects[&root], boxes.rects[&card]));
        k += 1;
        assert!(k < 600, "never settled");
        apply(r, buf, root, SceneDiff::new());
    }
    (out, k)
}

/// The launcher's case: rows leave, the panel's box springs shorter and
/// the card filling it follows. On every frame the card's shadow above,
/// below and beside it is as strong as at rest (it was cut to nothing).
#[test]
fn a_card_filling_a_shrinking_panel_keeps_its_shadow() {
    let (diff, root, card, rows) = launcher(4);
    let mut r = renderer();
    let mut buf = None;
    apply(&mut r, &mut buf, root, diff);
    let (shots, k) = frames(&mut r, &mut buf, root, card, 1);
    let (rest, _, at) = shots.last().expect("painted");
    let want = around(*at, 4.0).map(|p| alpha(rest, p));
    assert!(
        want.iter().all(|a| *a > 40),
        "a shadow at rest all round: {want:?}"
    );
    let mut d = SceneDiff::new();
    for row in &rows[1..] {
        d.remove(*row);
    }
    apply(&mut r, &mut buf, root, d);
    let (shots, _) = frames(&mut r, &mut buf, root, card, k);
    let moving: Vec<_> = shots
        .iter()
        .filter(|(_, p, _)| (p.h - 46.0).abs() > 0.01)
        .collect();
    assert!(moving.len() >= 4, "the box springs over frames");
    for (b, panel, c) in &moving {
        assert!(
            (c.h - panel.h).abs() < 0.01,
            "the card fills the panel: {c:?} {panel:?}"
        );
        let got = around(*c, 4.0).map(|p| alpha(b, p));
        for (g, w) in got.iter().zip(want) {
            assert!(
                u32::from(*g) * 10 >= u32::from(w) * 9,
                "its shadow above, below, left and right {got:?}, at rest {want:?}, \
                 card {c:?}"
            );
        }
    }
    let (b, _, _) = moving[moving.len() / 3];
    assert_matches_ref("shadow_pass_shrink", b, 1);
}

/// A plain size spring: a box whose `height` springs holds a card with
/// a shadow. While the card lies inside it, its shadow passes the
/// box's clip; once the box is shorter than the card, what spills past
/// is cut, the card's shadow with it.
#[test]
fn a_spilling_box_is_cut_with_its_shadow() {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Pad, num(40.0))]);
    let outer = b.node(
        NodeKind::Col,
        Some(root),
        vec![
            (Prop::Width, num(104.0)),
            (Prop::Height, num(200.0)),
            (Prop::Pad, num(2.0)),
        ],
    );
    let inner = b.node(
        NodeKind::Box,
        Some(outer),
        vec![
            (Prop::Width, num(100.0)),
            (Prop::Height, num(100.0)),
            (Prop::Shrink, num(0.0)),
            (Prop::Bg, color("#ffffff")),
            (Prop::Shadow, shadow()),
        ],
    );
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, root);
    let mut buf = Buffer::new(260, 320, Scale::ONE);
    let mut k = 0;
    while k == 0 || r.wants_frame(S) {
        buf.paint_at(&mut r, S, 1, frame(k));
        k += 1;
    }
    let mut d = SceneDiff::new();
    d.set(outer, Prop::Height, num(20.0));
    assert!(r.apply(d).is_empty());
    let (mut inside, mut spilling) = (0, 0);
    while r.wants_frame(S) {
        buf.paint_at(&mut r, S, 1, frame(k));
        k += 1;
        assert!(k < 600, "never settled");
        let boxes = r.boxes(S).expect("laid out");
        let (o, i) = (boxes.rects[&outer], boxes.rects[&inner]);
        if o.h >= 199.0 || o.h <= 21.0 {
            continue;
        }
        // Left of the outer box, beside the card: the card's shadow
        // passes the outer box's clip only while the card fits in it.
        let left = alpha(&buf, (o.x - 3.0, i.y + 10.0));
        if i.y + i.h <= o.y + o.h {
            inside += 1;
            assert!(left > 20, "the shadow passes: {left} at {o:?}");
        } else {
            spilling += 1;
            assert_eq!(left, 0, "cut with the card: {o:?}");
            let below = alpha(&buf, (i.x + i.w / 2.0, o.y + o.h + 2.0));
            assert_eq!(below, 0, "the card is cut past the box: {o:?}");
        }
    }
    assert!(inside >= 1 && spilling >= 1, "{inside} {spilling}");
}
