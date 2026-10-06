//! Flex layout on taffy: every container laid out and painted offline,
//! compared with the PNGs in `tests/refs` (bless with `STRAND_BLESS=1`),
//! plus the numbers behind them: a truly centred `split`, a virtualised
//! 2,000-row list, scrolling, content-sized surfaces, hit testing on the
//! rounded shape and paint-only changes that never relayout.

mod common;

use common::*;
use strand_scene::*;

const TOLERANCE: u8 = 3;
const S: SurfaceId = SurfaceId(1);

fn len_pct(p: f32) -> PropValue {
    PropValue::Length(Length::Percent(p))
}

fn kw(k: &str) -> PropValue {
    PropValue::Keyword(k.into())
}

fn list(v: &[f32]) -> PropValue {
    PropValue::List(v.iter().map(|n| num(*n)).collect())
}

/// A panel of `w × h` holding what `build` adds under the given root.
fn panel(w: u32, h: u32, build: impl FnOnce(&mut Builder, NodeId)) -> (SceneDiff, NodeId) {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(w as f32)),
            (Prop::Height, num(h as f32)),
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Color, color("#cdd6f4")),
            (Prop::Font, PropValue::Font(font(13.0))),
        ],
    );
    build(&mut b, root);
    (b.diff, root)
}

fn swatch(b: &mut Builder, parent: NodeId, c: &str, extra: Vec<(Prop, PropValue)>) -> NodeId {
    let mut props = vec![(Prop::Bg, color(c))];
    props.extend(extra);
    b.node(NodeKind::Box, Some(parent), props)
}

/// Applies `diff`, shows `root` on a `w × h` surface at `scale` and paints.
fn show(
    diff: SceneDiff,
    root: NodeId,
    w: u32,
    h: u32,
    scale: Scale,
) -> (strand_render::Renderer, Buffer) {
    let mut r = renderer();
    assert!(r.apply(diff).is_empty());
    r.attach_surface(S, root);
    let mut buf = Buffer::new(w, h, scale);
    buf.paint(&mut r, S, 0);
    (r, buf)
}

fn rect(r: &strand_render::Renderer, id: NodeId) -> LogicalRect {
    *r.boxes(S)
        .unwrap()
        .rects
        .get(&id)
        .unwrap_or_else(|| panic!("{id:?} not laid out"))
}

fn approx(a: LogicalRect, b: (f32, f32, f32, f32)) {
    let ok = (a.x - b.0).abs() < 0.51
        && (a.y - b.1).abs() < 0.51
        && (a.w - b.2).abs() < 0.51
        && (a.h - b.3).abs() < 0.51;
    assert!(ok, "{a:?} vs {b:?}");
}

/// `row`: children side by side after `pad`, `gap` between them, centred
/// on the cross axis by default; `grow` shares the space left; `margin`
/// takes the comma shorthand; `align: end` moves them down.
#[test]
fn row_lays_out_side_by_side() {
    let mut ids = Vec::new();
    let (d, root) = panel(200, 60, |b, root| {
        let row = b.node(
            NodeKind::Row,
            Some(root),
            vec![(Prop::Pad, list(&[6.0, 8.0])), (Prop::Gap, num(4.0))],
        );
        ids.push(row);
        ids.push(swatch(b, row, "#f38ba8", vec![(Prop::Size, num(20.0))]));
        ids.push(swatch(
            b,
            row,
            "#a6e3a1",
            vec![
                (Prop::Width, num(10.0)),
                (Prop::Height, num(30.0)),
                (Prop::Margin, list(&[0.0, 6.0])),
            ],
        ));
        ids.push(swatch(
            b,
            row,
            "#89b4fa",
            vec![(Prop::Grow, num(1.0)), (Prop::Height, num(12.0))],
        ));
    });
    let (r, buf) = show(d, root, 200, 60, Scale::ONE);
    approx(rect(&r, ids[0]), (0.0, 0.0, 200.0, 60.0));
    // Centred on the cross axis: (60 - 20) / 2.
    approx(rect(&r, ids[1]), (8.0, 20.0, 20.0, 20.0));
    approx(rect(&r, ids[2]), (8.0 + 20.0 + 4.0 + 6.0, 15.0, 10.0, 30.0));
    // Grows to the end of the row, before its right pad.
    let g = rect(&r, ids[3]);
    assert!((g.x + g.w - 192.0).abs() < 0.51, "{g:?}");
    assert_matches_ref("layout_row", &buf, TOLERANCE);
}

/// `col`: children stacked, stretched across; inherited `color` and `font`
/// reach the text; `align: center` centres a text in its line.
#[test]
fn col_stretches_and_inherits() {
    let mut ids = Vec::new();
    let (d, root) = panel(160, 120, |b, root| {
        let col = b.node(
            NodeKind::Col,
            Some(root),
            vec![
                (Prop::Pad, num(8.0)),
                (Prop::Gap, num(6.0)),
                (Prop::Color, color("#f9e2af")),
                (Prop::Font, PropValue::Font(font(16.0))),
            ],
        );
        ids.push(col);
        ids.push(swatch(b, col, "#45475a", vec![(Prop::Height, num(16.0))]));
        ids.push(b.node(
            NodeKind::Text,
            Some(col),
            vec![(Prop::Text, text("Inherited"))],
        ));
        ids.push(b.node(
            NodeKind::Text,
            Some(col),
            vec![(Prop::Text, text("centre")), (Prop::Align, kw("center"))],
        ));
    });
    let (r, buf) = show(d, root, 160, 120, Scale::ONE);
    approx(rect(&r, ids[1]), (8.0, 8.0, 144.0, 16.0));
    let t = rect(&r, ids[2]);
    assert_eq!((t.x, t.y, t.w), (8.0, 30.0, 144.0), "stretched across");
    assert!(t.h > 16.0 && t.h < 24.0, "a 16px line: {t:?}");
    // The text is the inherited yellow, not the panel's colour.
    let yellow = (0..160)
        .flat_map(|x| (30..50).map(move |y| (x, y)))
        .any(|(x, y)| {
            let p = buf.px(x, y);
            p[2] > 200 && p[1] > 180 && p[0] < 190
        });
    assert!(yellow, "the inherited colour paints the text");
    assert_matches_ref("layout_col", &buf, TOLERANCE);
}

/// `stack` and `box` put every child in one cell, stretched where its size
/// is auto; `place: absolute` takes a child out of the flow at `x`, `y`.
#[test]
fn stack_overlays_and_absolute_places() {
    let mut ids = Vec::new();
    let (d, root) = panel(120, 80, |b, root| {
        let stack = b.node(NodeKind::Stack, Some(root), vec![(Prop::Pad, num(10.0))]);
        ids.push(swatch(b, stack, "#313244", vec![]));
        ids.push(swatch(
            b,
            stack,
            "#fab387",
            vec![(Prop::Size, num(20.0)), (Prop::Radius, kw("full"))],
        ));
        ids.push(swatch(
            b,
            stack,
            "#94e2d5",
            vec![
                (Prop::Place, kw("absolute")),
                (Prop::X, num(70.0)),
                (Prop::Y, num(40.0)),
                (Prop::Size, num(16.0)),
            ],
        ));
    });
    let (r, buf) = show(d, root, 120, 80, Scale::ONE);
    approx(rect(&r, ids[0]), (10.0, 10.0, 100.0, 60.0));
    approx(rect(&r, ids[1]), (10.0, 10.0, 20.0, 20.0));
    // Absolute: laid out at the stack's content corner; `x`/`y` are paint
    // offsets from there.
    approx(rect(&r, ids[2]), (10.0, 10.0, 16.0, 16.0));
    assert_eq!(buf.px(85, 55), [0xd5, 0xe2, 0x94, 0xff]);
    assert_matches_ref("layout_stack", &buf, TOLERANCE);
}

/// `grid { columns: 3; gap }`: cells flow left to right, top to bottom.
#[test]
fn grid_flows_into_columns() {
    let mut cells = Vec::new();
    let (d, root) = panel(120, 90, |b, root| {
        let grid = b.node(
            NodeKind::Grid,
            Some(root),
            vec![
                (Prop::Columns, num(3.0)),
                (Prop::Gap, num(4.0)),
                (Prop::Pad, num(4.0)),
            ],
        );
        for i in 0..7 {
            let c = ["#f38ba8", "#a6e3a1", "#89b4fa"][i % 3];
            cells.push(swatch(
                b,
                grid,
                c,
                vec![(Prop::Size, num(24.0)), (Prop::Radius, num(4.0))],
            ));
        }
    });
    let (r, buf) = show(d, root, 120, 90, Scale::ONE);
    approx(rect(&r, cells[0]), (4.0, 4.0, 24.0, 24.0));
    approx(rect(&r, cells[2]), (4.0 + 2.0 * 28.0, 4.0, 24.0, 24.0));
    approx(rect(&r, cells[3]), (4.0, 32.0, 24.0, 24.0));
    approx(rect(&r, cells[6]), (4.0, 60.0, 24.0, 24.0));
    assert_matches_ref("layout_grid", &buf, TOLERANCE);
}

fn split_scene(start_w: f32, end_w: f32) -> (SceneDiff, NodeId, [NodeId; 4]) {
    let mut ids = [NodeId::new(0, 0); 4];
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
        vec![(Prop::Pad, list(&[0.0, 12.0]))],
    );
    let start = b.node(NodeKind::Start, Some(split), vec![(Prop::Gap, num(8.0))]);
    swatch(
        &mut b,
        start,
        "#f38ba8",
        vec![(Prop::Width, num(start_w)), (Prop::Height, num(12.0))],
    );
    let center = b.node(NodeKind::Center, Some(split), vec![]);
    ids[1] = center;
    ids[2] = b.node(
        NodeKind::Text,
        Some(center),
        vec![(Prop::Text, text("12:59"))],
    );
    let end = b.node(NodeKind::End, Some(split), vec![]);
    ids[3] = swatch(
        &mut b,
        end,
        "#a6e3a1",
        vec![(Prop::Width, num(end_w)), (Prop::Height, num(12.0))],
    );
    ids[0] = start;
    (b.diff, root, ids)
}

/// `split`: the centre is truly centred on the bar whatever the start and
/// end sections hold; `start` packs to the left, `end` to the right.
#[test]
fn split_centre_is_truly_centred() {
    for (sw, ew) in [(300.0, 20.0), (20.0, 300.0), (10.0, 10.0)] {
        let (d, root, [start, center, clock, end]) = split_scene(sw, ew);
        let (r, buf) = show(d, root, 800, 32, Scale::ONE);
        let c = rect(&r, center);
        assert!(
            (c.x + c.w / 2.0 - 400.0).abs() <= 0.5,
            "{sw}/{ew}: centre {c:?}"
        );
        let t = rect(&r, clock);
        assert!(
            (t.x + t.w / 2.0 - 400.0).abs() <= 1.0,
            "{sw}/{ew}: clock {t:?}"
        );
        assert!(
            (t.y + t.h / 2.0 - 16.0).abs() <= 1.0,
            "vertically centred: {t:?}"
        );
        assert_eq!(rect(&r, start).x, 12.0);
        let e = rect(&r, end);
        assert!((e.x + e.w - 788.0).abs() <= 1.0, "end packs right: {e:?}");
        if sw == 300.0 {
            assert_matches_ref("layout_split", &buf, TOLERANCE);
        }
    }
}

/// `split`'s sides never depend on the centre: as the centre's width
/// changes through both parities (a clock going from "Mon 05  23:59" to
/// "Tue 06  00:00"), every box in `start` and `end` keeps its rect to the
/// pixel. The side tracks fall on half pixels when the bar's width less the
/// centre's is odd; boxes are snapped from their absolute positions, so
/// `end`'s content stays put against the right edge.
#[test]
fn split_sides_never_move_with_the_centre() {
    let mut seen: Option<Vec<LogicalRect>> = None;
    for cw in (81..=92).map(|w| w as f32).chain([82.5, 87.25, 90.75]) {
        let mut b = Builder::default();
        let root = b.node(NodeKind::Bar, None, vec![(Prop::Height, num(32.0))]);
        let split = b.node(
            NodeKind::Split,
            Some(root),
            vec![(Prop::Pad, list(&[0.0, 12.0]))],
        );
        let start = b.node(NodeKind::Start, Some(split), vec![(Prop::Gap, num(8.0))]);
        let s1 = swatch(&mut b, start, "#f38ba8", vec![(Prop::Width, num(37.0))]);
        let s2 = swatch(&mut b, start, "#f38ba8", vec![(Prop::Width, num(16.0))]);
        let center = b.node(NodeKind::Center, Some(split), vec![]);
        swatch(&mut b, center, "#cdd6f4", vec![(Prop::Width, num(cw))]);
        let end = b.node(NodeKind::End, Some(split), vec![(Prop::Gap, num(6.0))]);
        let row = b.node(NodeKind::Row, Some(end), vec![(Prop::Gap, num(4.0))]);
        let e1 = swatch(&mut b, row, "#a6e3a1", vec![(Prop::Width, num(16.0))]);
        let e2 = swatch(&mut b, row, "#a6e3a1", vec![(Prop::Width, num(111.0))]);
        let e3 = swatch(&mut b, end, "#a6e3a1", vec![(Prop::Width, num(16.0))]);
        let (r, _) = show(b.diff, root, 2560, 32, Scale::ONE);
        let rects: Vec<LogicalRect> = [s1, s2, row, e1, e2, e3]
            .iter()
            .map(|id| rect(&r, *id))
            .collect();
        for x in &rects {
            assert_eq!(x.x, x.x.round(), "whole pixels at {cw}: {x:?}");
        }
        assert_eq!(rects[5].x + rects[5].w, 2560.0 - 12.0, "{cw}: {rects:?}");
        match &seen {
            None => seen = Some(rects),
            Some(first) => assert_eq!(first, &rects, "centre {cw} moved a side"),
        }
    }
}

/// The same split at a fractional scale: still centred, and painted.
#[test]
fn split_at_fractional_scale() {
    let s = Scale::from_f64(1.25).unwrap();
    let (d, root, [_, center, _, _]) = split_scene(300.0, 20.0);
    let size = s.physical_size(LogicalSize::new(800.0, 32.0));
    let (r, buf) = show(d, root, size.w, size.h, s);
    let c = rect(&r, center);
    assert!((c.x + c.w / 2.0 - 400.0).abs() <= 0.5, "{c:?}");
    assert_matches_ref("layout_split_1_25x", &buf, TOLERANCE);
}

/// `spacer` takes the space left; `max_width: 40%` caps an ellipsised
/// text, which is then cut to its box; `width` takes `%` and `ch`.
#[test]
fn spacer_percentages_and_ellipsis() {
    let mut ids = Vec::new();
    let (d, root) = panel(300, 40, |b, root| {
        let row = b.node(NodeKind::Row, Some(root), vec![(Prop::Pad, num(4.0))]);
        ids.push(b.node(
            NodeKind::Text,
            Some(row),
            vec![
                (
                    Prop::Text,
                    text("Firefox — a window title far too long for its space"),
                ),
                (Prop::MaxWidth, len_pct(40.0)),
                (Prop::Ellipsis, kw("end")),
            ],
        ));
        ids.push(b.node(NodeKind::Spacer, Some(row), vec![]));
        ids.push(swatch(
            b,
            row,
            "#89b4fa",
            vec![(Prop::Width, len_pct(10.0)), (Prop::Height, num(10.0))],
        ));
        ids.push(b.node(
            NodeKind::Text,
            Some(row),
            vec![
                (Prop::Text, text("87%")),
                (Prop::Width, PropValue::Length(Length::Ch(4.0))),
                (Prop::Align, kw("end")),
            ],
        ));
    });
    let (r, buf) = show(d, root, 300, 40, Scale::ONE);
    let title = rect(&r, ids[0]);
    assert!(
        (title.w - 0.4 * 292.0).abs() < 1.0,
        "max_width 40%: {title:?}"
    );
    let pct = rect(&r, ids[3]);
    assert!((pct.w - 4.0 * 0.6 * 13.0).abs() < 0.51, "4ch: {pct:?}");
    assert!(
        (pct.x + pct.w - 296.0).abs() < 0.51,
        "pushed right by the spacer"
    );
    let sw = rect(&r, ids[2]);
    assert!(
        (sw.w - 29.2).abs() < 0.6,
        "10% of the row's content: {sw:?}"
    );
    // The cut title stays inside its box.
    let past = (title.x + title.w + 1.0) as u32;
    for x in past..(sw.x as u32 - 1) {
        for y in 0..40 {
            assert_eq!(
                buf.px(x, y),
                [0x2e, 0x1e, 0x1e, 0xff],
                "({x}, {y}) past the title"
            );
        }
    }
    assert_matches_ref("layout_spacer", &buf, TOLERANCE);
}

/// `scroll`: content taller than its box is clipped to it; a wheel moves
/// it, within the content.
#[test]
fn scroll_clips_and_scrolls() {
    let mut rows = Vec::new();
    let mut scroller = None;
    let (d, root) = panel(100, 100, |b, root| {
        let sc = b.node(
            NodeKind::Scroll,
            Some(root),
            vec![(Prop::MaxHeight, num(60.0)), (Prop::Gap, num(4.0))],
        );
        scroller = Some(sc);
        for i in 0..5 {
            rows.push(swatch(
                b,
                sc,
                ["#f38ba8", "#a6e3a1", "#89b4fa", "#f9e2af", "#cba6f7"][i],
                vec![(Prop::Height, num(20.0))],
            ));
        }
    });
    let sc = scroller.unwrap();
    let (mut r, mut buf) = show(d, root, 100, 100, Scale::ONE);
    approx(rect(&r, sc), (0.0, 0.0, 100.0, 60.0));
    // Below the scroll box nothing is drawn.
    assert_eq!(buf.px(50, 70), [0x2e, 0x1e, 0x1e, 0xff]);
    assert_matches_ref("layout_scroll", &buf, TOLERANCE);
    assert_eq!(r.scroll(S, LogicalPoint::new(50.0, 30.0), 1000.0), Some(sc));
    assert!(r.wants_frame(S));
    buf.paint(&mut r, S, 1);
    // 5 rows of 20 and 4 gaps of 4 = 116: scrolled to its end, 56.
    approx(rect(&r, rows[4]), (0.0, 96.0 - 56.0, 100.0, 20.0));
    assert_matches_ref("layout_scroll_end", &buf, TOLERANCE);
    // At the end already: nothing moves.
    assert_eq!(r.scroll(S, LogicalPoint::new(50.0, 30.0), 10.0), None);
}

fn long_list(n: usize) -> (SceneDiff, NodeId, NodeId) {
    let mut lst = None;
    let (d, root) = panel(200, 420, |b, root| {
        let l = b.node(
            NodeKind::List,
            Some(root),
            vec![(Prop::MaxHeight, num(400.0))],
        );
        lst = Some(l);
        for i in 0..n {
            let row = b.node(
                NodeKind::Row,
                Some(l),
                vec![(Prop::Pad, num(4.0)), (Prop::Gap, num(6.0))],
            );
            swatch(
                b,
                row,
                "#89b4fa",
                vec![(Prop::Size, num(24.0)), (Prop::Radius, num(6.0))],
            );
            b.node(
                NodeKind::Text,
                Some(row),
                vec![(Prop::Text, text(&format!("Row {i}")))],
            );
        }
    });
    (d, root, lst.unwrap())
}

/// A 2,000-row `list` lays out only the rows its viewport shows, measures
/// them, and scrolls by row heights; the rest are never laid out.
#[test]
fn a_2000_row_list_lays_out_only_visible_rows() {
    let (d, root, lst) = long_list(2000);
    let (mut r, mut buf) = show(d, root, 200, 420, Scale::ONE);
    let b = r.boxes(S).unwrap();
    assert_eq!(b.rows_total, 2000);
    // 32 px rows in a 400 px viewport: 13 rows at most.
    assert!(b.rows_laid_out <= 14, "{}", b.rows_laid_out);
    let laid = b.rects.len();
    assert!(laid < 14 * 3 + 10, "{laid} boxes");
    approx(rect(&r, lst), (0.0, 0.0, 200.0, 400.0));
    assert_matches_ref("layout_list", &buf, TOLERANCE);
    // Scroll to the end: the last rows are laid out, the first are gone.
    let t = std::time::Instant::now();
    assert!(r.scroll(S, LogicalPoint::new(10.0, 10.0), 1e6).is_some());
    buf.paint(&mut r, S, 1);
    let spent = t.elapsed();
    let b = r.boxes(S).unwrap();
    let last = r.tree().get(lst).unwrap().children[1999];
    let first = r.tree().get(lst).unwrap().children[0];
    assert!(b.rects.contains_key(&last) && !b.rects.contains_key(&first));
    let lr = b.rects[&last];
    assert!(
        (lr.y + lr.h - 400.0).abs() < 0.6,
        "last row at the bottom: {lr:?}"
    );
    assert!(b.rows_laid_out <= 14);
    eprintln!("scrolled 2000 rows and painted in {spent:?}");
}

/// Hit testing uses the rounded shape (a pill's corner is not the pill),
/// `hit: grow(n)` enlarges it, and a shadow never does.
#[test]
fn hits_follow_the_rounded_shape() {
    let mut ids = Vec::new();
    let (d, root) = panel(120, 60, |b, root| {
        let row = b.node(
            NodeKind::Row,
            Some(root),
            vec![(Prop::Pad, num(10.0)), (Prop::Gap, num(30.0))],
        );
        ids.push(swatch(
            b,
            row,
            "#f38ba8",
            vec![
                (Prop::Size, num(30.0)),
                (Prop::Radius, kw("full")),
                (
                    Prop::Shadow,
                    PropValue::Shadow(vec![Shadow {
                        x: 0.0,
                        y: 0.0,
                        blur: 8.0,
                        spread: 4.0,
                        color: hex("#000000"),
                    }]),
                ),
            ],
        ));
        ids.push(swatch(
            b,
            row,
            "#a6e3a1",
            vec![
                (Prop::Size, num(8.0)),
                (Prop::Radius, kw("full")),
                (
                    Prop::Hit,
                    PropValue::Call {
                        name: "grow".into(),
                        args: vec![num(6.0)],
                    },
                ),
            ],
        ));
        ids.push(row);
    });
    let (r, _buf) = show(d, root, 120, 60, Scale::ONE);
    let (pill, dot, row) = (ids[0], ids[1], ids[2]);
    let p = rect(&r, pill);
    assert_eq!(
        r.hit(S, LogicalPoint::new(p.x + 15.0, p.y + 15.0)),
        [pill, row, root]
    );
    // Inside the box, outside the circle: the row.
    assert_eq!(
        r.hit(S, LogicalPoint::new(p.x + 2.0, p.y + 2.0)),
        [row, root]
    );
    // In the shadow, outside the box: not the pill.
    assert_eq!(
        r.hit(S, LogicalPoint::new(p.x + 15.0, p.y + 32.0)),
        [row, root]
    );
    let d = rect(&r, dot);
    // 4 px right of the 8 px dot: inside `grow(6)`.
    assert_eq!(
        r.hit(S, LogicalPoint::new(d.x + 12.0, d.y + 4.0)),
        [dot, row, root]
    );
    assert_eq!(
        r.hit(S, LogicalPoint::new(d.x + 16.0, d.y + 4.0)),
        [row, root]
    );
}

/// Paint-only props (`bg`, `x`, `opacity`) never relayout; a size change
/// does, and only for its own surface.
#[test]
fn paint_only_changes_never_relayout() {
    let mut ids = Vec::new();
    let (d, root) = panel(100, 40, |b, root| {
        let row = b.node(NodeKind::Row, Some(root), vec![]);
        ids.push(swatch(b, row, "#f38ba8", vec![(Prop::Size, num(20.0))]));
    });
    let (mut r, mut buf) = show(d, root, 100, 40, Scale::ONE);
    let passes = r.layout_passes();
    let mut d = SceneDiff::new();
    d.set(ids[0], Prop::Bg, color("#a6e3a1"))
        .set(ids[0], Prop::X, num(30.0))
        .set(ids[0], Prop::Opacity, num(0.5));
    assert!(r.apply(d).is_empty());
    buf.paint(&mut r, S, 1);
    assert_eq!(r.layout_passes(), passes, "paint-only changes relaid out");
    let mut d = SceneDiff::new();
    d.set(ids[0], Prop::Width, num(40.0));
    assert!(r.apply(d).is_empty());
    buf.paint(&mut r, S, 1);
    assert!(r.layout_passes() > passes);
    assert_eq!(rect(&r, ids[0]).w, 40.0);
}

/// Layout facts: sizes that changed go to logic (`self.width`), once,
/// and only for nodes logic reads them of (`watch`); a size change of an
/// unwatched node sends nothing.
#[test]
fn layout_facts_report_changed_sizes() {
    let mut ids = Vec::new();
    let (d, root) = panel(100, 40, |b, root| {
        let row = b.node(NodeKind::Row, Some(root), vec![]);
        ids.push(swatch(
            b,
            row,
            "#f38ba8",
            vec![(Prop::Size, num(20.0)), (Prop::Watch, kw("size"))],
        ));
        ids.push(swatch(b, row, "#a6e3a1", vec![(Prop::Size, num(20.0))]));
    });
    let (mut r, mut buf) = show(d, root, 100, 40, Scale::ONE);
    assert_eq!(r.take_layout_facts(), vec![(ids[0], 20.0, 20.0)]);
    assert_eq!(r.layout_seq(), 1);
    let mut d = SceneDiff::new();
    d.set(ids[0], Prop::Width, num(30.0));
    r.apply(d);
    buf.paint(&mut r, S, 1);
    assert_eq!(r.take_layout_facts(), vec![(ids[0], 30.0, 20.0)]);
    assert!(r.take_layout_facts().is_empty());
    assert_eq!(r.layout_seq(), 2, "an empty batch takes no number");
    // The unwatched one changes size: nothing goes to logic.
    let mut d = SceneDiff::new();
    d.set(ids[1], Prop::Width, num(40.0));
    r.apply(d);
    buf.paint(&mut r, S, 1);
    assert_eq!(rect(&r, ids[1]).w, 40.0);
    assert!(r.take_layout_facts().is_empty());
    // Watched from now on: its current size goes at once.
    let mut d = SceneDiff::new();
    d.set(ids[1], Prop::Watch, kw("size"));
    r.apply(d);
    assert_eq!(r.take_layout_facts(), vec![(ids[1], 40.0, 20.0)]);
}

/// A container query's size changing holds the frame for logic's answer:
/// no frame is wanted until a diff says logic saw the facts (or the wait
/// runs out), and at most one hold per frame.
#[test]
fn a_query_size_change_holds_the_frame_for_logic() {
    let mut ids = Vec::new();
    let (d, root) = panel(100, 40, |b, root| {
        ids.push(b.node(
            NodeKind::Row,
            Some(root),
            vec![(Prop::Watch, kw("query")), (Prop::Bg, color("#f38ba8"))],
        ));
    });
    let mut r = renderer();
    r.set_query_wait(std::time::Duration::from_secs(30));
    // Idle however recently it painted (the busy guard has its own test).
    r.set_busy_window(std::time::Duration::ZERO);
    assert!(r.apply(d).is_empty());
    r.attach_surface(S, root);
    r.configure_surface(S, Size::new(100, 40), Scale::ONE);
    assert!(!r.wants_frame(S), "held for the query's answer");
    assert!(r.frame_deadline(S).is_some());
    assert_eq!(r.take_layout_facts(), vec![(ids[0], 100.0, 40.0)]);
    // A diff from before logic saw them does not release it.
    let mut d = SceneDiff::new();
    d.layout_seen = Some(r.layout_seq() - 1);
    r.apply(d);
    assert!(!r.wants_frame(S));
    // Logic's answer: the variant it picked, and the batch it saw.
    let mut d = SceneDiff::new();
    d.set(ids[0], Prop::Bg, color("#a6e3a1"));
    d.layout_seen = Some(r.layout_seq());
    r.apply(d);
    assert!(r.wants_frame(S));
    let mut buf = Buffer::new(100, 40, Scale::ONE);
    buf.paint(&mut r, S, 0);
    // A second size change after a paint holds again.
    let mut d = SceneDiff::new();
    d.set(ids[0], Prop::Height, num(20.0));
    r.apply(d);
    assert!(!r.wants_frame(S));
    assert_eq!(r.take_layout_facts(), vec![(ids[0], 100.0, 20.0)]);
    // The answer changes its size again: no second hold in this frame.
    let mut d = SceneDiff::new();
    d.set(ids[0], Prop::Height, num(10.0));
    d.layout_seen = Some(r.layout_seq());
    r.apply(d);
    assert!(r.wants_frame(S), "one hold per frame");
}

/// Surfaces without a size are sized by their content: a panel by its
/// column, a bar's thickness by its content; shadows reach past the box
/// as the overhang the surface manager grows the buffer by.
#[test]
fn content_sized_surfaces_report_their_size_and_overhang() {
    let mut b = Builder::default();
    let p = b.node(
        NodeKind::Panel,
        None,
        vec![(Prop::Font, PropValue::Font(font(13.0)))],
    );
    let col = b.node(
        NodeKind::Col,
        Some(p),
        vec![
            (Prop::Width, num(200.0)),
            (Prop::Pad, num(8.0)),
            (Prop::Gap, num(4.0)),
            (
                Prop::Shadow,
                PropValue::Shadow(vec![Shadow {
                    x: 0.0,
                    y: 8.0,
                    blur: 24.0,
                    spread: 0.0,
                    color: hex("#0000004d"),
                }]),
            ),
        ],
    );
    b.node(NodeKind::Text, Some(col), vec![(Prop::Text, text("One"))]);
    b.node(NodeKind::Text, Some(col), vec![(Prop::Text, text("Two"))]);
    let bar = b.node(
        NodeKind::Bar,
        None,
        vec![(Prop::Font, PropValue::Font(font(13.0)))],
    );
    let row = b.node(NodeKind::Row, Some(bar), vec![(Prop::Pad, num(6.0))]);
    b.node(NodeKind::Text, Some(row), vec![(Prop::Text, text("bar"))]);
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    let spec = r.surface_spec(p).unwrap().clone();
    assert_eq!(spec.width, Some(200.0));
    let h = spec.height.unwrap();
    assert!(h > 8.0 + 15.0 + 4.0 + 15.0 + 8.0 - 3.0 && h < 60.0, "{h}");
    // blur 24: reach 1.5 × 24 + 1 = 37, offset 8 down (29 above, 45
    // below); a centred panel asks for the larger side on both, so the
    // compositor centres its box, not its buffer.
    let o = spec.overhang;
    assert_eq!((o.top, o.bottom, o.left, o.right), (45.0, 45.0, 37.0, 37.0));
    let bar_spec = r.surface_spec(bar).unwrap();
    let t = bar_spec.height.unwrap();
    assert!(t > 20.0 && t < 32.0, "thickness from content: {t}");
    assert_eq!(bar_spec.exclusive_zone(), Some(t));

    // Shown at its size plus the overhang, the column sits inside it.
    r.attach_surface(S, p);
    let (w, hh) = (200 + 74, h as u32 + 45 + 45);
    let mut buf = Buffer::new(w, hh, Scale::ONE);
    buf.paint(&mut r, S, 0);
    approx(rect(&r, col), (37.0, 45.0, 200.0, h));
    // Hits stop at the box: the shadow area is the root, not the column.
    assert_eq!(r.hit(S, LogicalPoint::new(20.0, 40.0)), [p]);
}

/// `markup: basic` turns tags into spans: bold, italic, underline, links
/// in `$accent`.
#[test]
fn markup_basic_paints_spans() {
    let (d, root) = panel(260, 40, |b, root| {
        let row = b.node(NodeKind::Row, Some(root), vec![(Prop::Pad, num(8.0))]);
        b.node(
            NodeKind::Text,
            Some(row),
            vec![
                (
                    Prop::Text,
                    text("<b>Bold</b> <i>it</i> <u>under</u> &amp; <a href=\"x\">link</a>"),
                ),
                (Prop::Markup, kw("basic")),
            ],
        );
    });
    let (_r, buf) = show(d, root, 260, 40, Scale::ONE);
    assert_matches_ref("layout_markup", &buf, TOLERANCE);
}

/// The design's launcher: a content-sized panel holding `col { width }`
/// over a `list { max_height: 420 }` of 2,000 rows. The list's cap is what
/// its column sees (it does not grow to every row), the surface asks for a
/// panel of its capped size, and only the rows in view are shaped.
#[test]
fn a_content_sized_launcher_caps_its_list_and_shapes_visible_rows() {
    for fixed in [false, true] {
        let mut b = Builder::default();
        let mut props = vec![(Prop::Font, PropValue::Font(font(13.0)))];
        if fixed {
            props.push((Prop::Width, num(600.0)));
            props.push((Prop::Height, num(1000.0)));
        }
        let p = b.node(NodeKind::Panel, None, props);
        let col = b.node(
            NodeKind::Col,
            Some(p),
            vec![(Prop::Width, num(600.0)), (Prop::Pad, num(8.0))],
        );
        let lst = b.node(
            NodeKind::List,
            Some(col),
            vec![(Prop::MaxHeight, num(420.0))],
        );
        for i in 0..2000 {
            let row = b.node(NodeKind::Row, Some(lst), vec![(Prop::Pad, num(8.0))]);
            b.node(
                NodeKind::Text,
                Some(row),
                vec![(Prop::Text, text(&format!("App {i}")))],
            );
        }
        let mut r = renderer();
        assert!(r.apply(b.diff).is_empty());
        let spec = r.surface_spec(p).unwrap().clone();
        let (w, h) = (spec.width.unwrap(), spec.height.unwrap());
        assert_eq!(w, 600.0);
        if fixed {
            assert_eq!(h, 1000.0);
        } else {
            assert!((h - 436.0).abs() < 1.0, "content height {h}");
        }
        assert!(r.text_slots() < 50, "{} text slots", r.text_slots());
        r.attach_surface(S, p);
        let mut buf = Buffer::new(w as u32, h as u32, Scale::ONE);
        buf.paint(&mut r, S, 0);
        // In a sized panel the column stretches over its cell (a stack)
        // and no further; by content it is the capped list plus its pad.
        let c = rect(&r, col);
        let want = if fixed { 1000.0 } else { 436.0 };
        assert!((c.h - want).abs() < 1.0, "col {c:?} (fixed: {fixed})");
        approx(rect(&r, lst), (8.0, 8.0, 584.0, 420.0));
        let bx = r.boxes(S).unwrap();
        assert!(bx.rows_laid_out <= 14, "{}", bx.rows_laid_out);
        assert!(r.text_slots() < 50, "{} text slots", r.text_slots());
    }
}

/// Content taller than any output is capped: a 2,000-row list with no
/// `max_height` in a content-sized panel asks for `MAX_CONTENT_SIZE`, not
/// 64,000 px.
#[test]
fn content_sized_surfaces_are_capped() {
    let mut b = Builder::default();
    let p = b.node(
        NodeKind::Panel,
        None,
        vec![(Prop::Font, PropValue::Font(font(13.0)))],
    );
    let lst = b.node(NodeKind::List, Some(p), vec![(Prop::Width, num(300.0))]);
    for i in 0..2000 {
        b.node(
            NodeKind::Text,
            Some(lst),
            vec![(Prop::Text, text(&format!("Line {i}")))],
        );
    }
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    let spec = r.surface_spec(p).unwrap();
    assert_eq!(spec.width, Some(300.0));
    assert_eq!(spec.height, Some(strand_render::MAX_CONTENT_SIZE));
    r.attach_surface(S, p);
    let mut buf = Buffer::new(300, 4096, Scale::ONE);
    buf.paint(&mut r, S, 0);
    let bx = r.boxes(S).unwrap();
    approx(bx.rects[&lst], (0.0, 0.0, 300.0, 4096.0));
    assert!(bx.rows_laid_out < 300, "{}", bx.rows_laid_out);
    assert!(r.text_slots() < 300, "{} text slots", r.text_slots());
}

/// A list inside a list's row lays out its own rows in view.
#[test]
fn a_list_in_a_list_row_lays_out_its_rows() {
    let mut inner = None;
    let (d, root) = panel(200, 200, |b, root| {
        let outer = b.node(NodeKind::List, Some(root), vec![]);
        let row = b.node(NodeKind::Col, Some(outer), vec![]);
        let l = b.node(NodeKind::List, Some(row), vec![(Prop::Height, num(40.0))]);
        inner = Some(l);
        for i in 0..10 {
            b.node(
                NodeKind::Text,
                Some(l),
                vec![(Prop::Text, text(&format!("Inner {i}")))],
            );
        }
        b.node(
            NodeKind::Text,
            Some(outer),
            vec![(Prop::Text, text("Next"))],
        );
    });
    let (r, _buf) = show(d, root, 200, 200, Scale::ONE);
    let inner = inner.unwrap();
    approx(rect(&r, inner), (0.0, 0.0, 200.0, 40.0));
    let kids = &r.tree().get(inner).unwrap().children;
    let first = rect(&r, kids[0]);
    assert!(
        first.y >= 0.0 && first.y < 1.0 && first.h > 10.0,
        "{first:?}"
    );
    // 40 px of ~16 px rows: the last ones are not laid out.
    assert!(!r.boxes(S).unwrap().rects.contains_key(&kids[9]));
}

/// A wheel over a list at its end scrolls the `scroll` around it.
#[test]
fn a_scroll_at_its_end_passes_the_wheel_outward() {
    let mut ids = Vec::new();
    let (d, root) = panel(100, 100, |b, root| {
        let outer = b.node(
            NodeKind::Scroll,
            Some(root),
            vec![(Prop::MaxHeight, num(60.0))],
        );
        let inner = b.node(NodeKind::List, Some(outer), vec![(Prop::Height, num(40.0))]);
        for i in 0..6 {
            b.node(
                NodeKind::Text,
                Some(inner),
                vec![(Prop::Text, text(&format!("Row {i}")))],
            );
        }
        swatch(b, outer, "#89b4fa", vec![(Prop::Height, num(100.0))]);
        ids.push(outer);
        ids.push(inner);
    });
    let (mut r, mut buf) = show(d, root, 100, 100, Scale::ONE);
    let (outer, inner) = (ids[0], ids[1]);
    let at = LogicalPoint::new(50.0, 10.0);
    assert_eq!(r.scroll(S, at, 1000.0), Some(inner));
    buf.paint(&mut r, S, 1);
    assert_eq!(r.scroll(S, at, 10.0), Some(outer), "the list is at its end");
    buf.paint(&mut r, S, 1);
    // Back up: the list under the pointer moves first again.
    let at = LogicalPoint::new(50.0, 1.0);
    assert_eq!(r.scroll(S, at, -5.0), Some(inner));
}

/// A percentage `x`/`y` is of the parent's box, as CSS insets are.
#[test]
fn percent_offsets_are_of_the_parent() {
    let mut ids = Vec::new();
    let (d, root) = panel(200, 100, |b, root| {
        let st = b.node(NodeKind::Stack, Some(root), vec![]);
        ids.push(swatch(
            b,
            st,
            "#f38ba8",
            vec![
                (Prop::Place, kw("absolute")),
                (Prop::Size, num(20.0)),
                (Prop::X, len_pct(50.0)),
                (Prop::Y, len_pct(25.0)),
            ],
        ));
    });
    let (r, buf) = show(d, root, 200, 100, Scale::ONE);
    // Drawn at (100, 25), not at 50% of its own 20 px.
    assert_eq!(buf.px(105, 30), [0xa8, 0x8b, 0xf3, 0xff]);
    assert_eq!(buf.px(15, 10), [0x2e, 0x1e, 0x1e, 0xff]);
    assert_eq!(r.hit(S, LogicalPoint::new(110.0, 35.0))[0], ids[0]);
}

const LONG: &str =
    "A notification body long enough that it has to wrap over several lines in any of these boxes";

/// Long plain text stays inside its surface and wraps: in a column
/// aligned `start` on a fixed panel, and in a growing column in a fixed
/// row (its smallest width is its longest word, as in CSS, so the column
/// shrinks to the row instead of widening it).
#[test]
fn long_plain_text_wraps_inside_its_surface() {
    let mut ids = Vec::new();
    let (d, root) = panel(300, 200, |b, root| {
        let outer = b.node(
            NodeKind::Col,
            Some(root),
            vec![(Prop::Align, kw("start")), (Prop::Gap, num(6.0))],
        );
        ids.push(outer);
        ids.push(b.node(NodeKind::Text, Some(outer), vec![(Prop::Text, text(LONG))]));
        let row = b.node(
            NodeKind::Row,
            Some(outer),
            vec![(Prop::Width, num(300.0)), (Prop::Bg, color("#313244"))],
        );
        ids.push(row);
        let col = b.node(NodeKind::Col, Some(row), vec![(Prop::Grow, num(1.0))]);
        ids.push(col);
        ids.push(b.node(NodeKind::Text, Some(col), vec![(Prop::Text, text(LONG))]));
        ids.push(swatch(
            b,
            row,
            "#f38ba8",
            vec![(Prop::Size, num(20.0)), (Prop::Shrink, num(0.0))],
        ));
    });
    let (r, buf) = show(d, root, 300, 200, Scale::ONE);
    for id in &ids {
        let b = rect(&r, *id);
        assert!(
            b.x >= -0.5 && b.x + b.w <= 300.5,
            "{id:?} past the surface: {b:?}"
        );
    }
    let (t1, col, t2, end) = (
        rect(&r, ids[1]),
        rect(&r, ids[3]),
        rect(&r, ids[4]),
        rect(&r, ids[5]),
    );
    assert!(t1.h > 1.5 * 15.0, "wrapped: {t1:?}");
    assert!(
        t2.h > 1.5 * 15.0 && t2.w <= 280.5,
        "wrapped in the growing column: {t2:?}"
    );
    assert!(
        col.w <= 280.5,
        "the column leaves room for its sibling: {col:?}"
    );
    approx(end, (280.0, end.y, 20.0, 20.0));
    assert_matches_ref("layout_wrap", &buf, TOLERANCE);
}

/// `max_width` (px or %) on wrapping text gives a box as tall as its
/// wrapped lines, so the next sibling sits below them.
#[test]
fn max_width_text_is_as_tall_as_its_lines() {
    for max in [num(120.0), len_pct(40.0)] {
        let mut ids = Vec::new();
        let (d, root) = panel(300, 200, |b, root| {
            let col = b.node(NodeKind::Col, Some(root), vec![]);
            ids.push(b.node(
                NodeKind::Text,
                Some(col),
                vec![(Prop::Text, text(LONG)), (Prop::MaxWidth, max.clone())],
            ));
            ids.push(swatch(b, col, "#f38ba8", vec![(Prop::Height, num(10.0))]));
        });
        let (r, _) = show(d, root, 300, 200, Scale::ONE);
        let (t, next) = (rect(&r, ids[0]), rect(&r, ids[1]));
        assert!(t.w <= 120.5, "{max:?}: {t:?}");
        assert!(
            next.y >= 3.0 * 15.0,
            "{max:?}: next at {next:?}, text {t:?}"
        );
    }
}

/// A `text` showing nothing (the launcher's `h.app.comment ?? ""`) takes
/// no line, as an empty block in CSS: a row centring a one-line name
/// beside a 32 px icon centres it on the icon.
#[test]
fn an_empty_text_takes_no_line() {
    let mut ids = Vec::new();
    let (d, root) = panel(300, 60, |b, root| {
        let row = b.node(NodeKind::Row, Some(root), vec![(Prop::Align, kw("center"))]);
        ids.push(swatch(b, row, "#89b4fa", vec![(Prop::Size, num(32.0))]));
        let col = b.node(NodeKind::Col, Some(row), vec![]);
        ids.push(b.node(NodeKind::Text, Some(col), vec![(Prop::Text, text("Foot"))]));
        ids.push(b.node(NodeKind::Text, Some(col), vec![(Prop::Text, text(""))]));
        ids.push(col);
    });
    let (r, _) = show(d, root, 300, 60, Scale::ONE);
    let (icon, name, empty, col) = (
        rect(&r, ids[0]),
        rect(&r, ids[1]),
        rect(&r, ids[2]),
        rect(&r, ids[3]),
    );
    assert_eq!(empty.h, 0.0, "{empty:?}");
    assert_eq!(col.h, name.h, "the column is the name's line");
    let mid = |r: LogicalRect| r.y + r.h / 2.0;
    assert!(
        (mid(name) - mid(icon)).abs() <= 1.0,
        "name {name:?} centred on icon {icon:?}"
    );
}

/// A non-finite wheel delta moves nothing, and the list keeps its rows.
#[test]
fn a_nan_scroll_is_ignored() {
    let (d, root, lst) = long_list(50);
    let (mut r, mut buf) = show(d, root, 200, 420, Scale::ONE);
    assert_eq!(r.scroll(S, LogicalPoint::new(10.0, 10.0), f32::NAN), None);
    assert_eq!(
        r.scroll(S, LogicalPoint::new(10.0, 10.0), f32::INFINITY),
        None
    );
    buf.paint(&mut r, S, 1);
    assert!(r.boxes(S).unwrap().rows_laid_out > 5);
    let first = r.tree().get(lst).unwrap().children[0];
    approx(rect(&r, first), (0.0, 0.0, 200.0, rect(&r, first).h));
}

/// A surface in motion (painted within the busy window: a size
/// animating) never holds a frame for a container query: its size
/// changes still go to logic, and the answer lands a frame later, so a
/// busy logic thread never drops an animation frame.
#[test]
fn a_busy_surface_does_not_hold_for_queries() {
    let mut ids = Vec::new();
    let (d, root) = panel(100, 40, |b, root| {
        ids.push(b.node(
            NodeKind::Row,
            Some(root),
            vec![
                (Prop::Watch, kw("query")),
                (Prop::Width, num(80.0)),
                (Prop::Bg, color("#f38ba8")),
            ],
        ));
    });
    let mut r = renderer();
    r.set_query_wait(std::time::Duration::from_secs(30));
    r.set_busy_window(std::time::Duration::from_secs(30));
    assert!(r.apply(d).is_empty());
    r.attach_surface(S, root);
    r.configure_surface(S, Size::new(100, 40), Scale::ONE);
    // Never painted: idle, so the first frame waits for the answer.
    assert!(!r.wants_frame(S));
    r.take_layout_facts();
    let mut d = SceneDiff::new();
    d.layout_seen = Some(r.layout_seq());
    r.apply(d);
    let mut buf = Buffer::new(100, 40, Scale::ONE);
    buf.paint(&mut r, S, 0);
    // Animating: every frame changes its width, none is held.
    for w in [78.0, 76.0, 74.0, 72.0] {
        let mut d = SceneDiff::new();
        d.set(ids[0], Prop::Width, num(w));
        r.apply(d);
        assert!(r.wants_frame(S), "held at width {w}");
        assert_eq!(r.take_layout_facts(), vec![(ids[0], w, 40.0)]);
        buf.paint(&mut r, S, 1);
    }
}

/// A content-sized surface whose size changes holds its frame for the
/// compositor's configure at the new size, so no frame is painted at the
/// old size first; the configure releases it, and past the wait it paints
/// anyway. Offline (no wait set) nothing holds.
#[test]
fn a_resized_content_sized_surface_waits_for_its_configure() {
    let mut b = Builder::default();
    let p = b.node(
        NodeKind::Panel,
        None,
        vec![(Prop::Font, PropValue::Font(font(13.0)))],
    );
    let t = b.node(NodeKind::Text, Some(p), vec![(Prop::Text, text("short"))]);
    let mut r = renderer();
    r.set_resize_wait(std::time::Duration::from_secs(30));
    assert!(r.apply(b.diff).is_empty());
    let size = |r: &strand_render::Renderer| {
        let s = r.surface_spec(p).unwrap();
        Size::new(s.width.unwrap() as u32, s.height.unwrap() as u32)
    };
    let first = size(&r);
    r.attach_surface(S, p);
    r.configure_surface(S, first, Scale::ONE);
    assert!(r.wants_frame(S), "configured at its spec size: no hold");
    let mut buf = Buffer::new(first.w, first.h, Scale::ONE);
    buf.paint(&mut r, S, 0);
    let mut d = SceneDiff::new();
    d.set(t, Prop::Text, text("a much longer line of text"));
    r.apply(d);
    let second = size(&r);
    assert!(second.w > first.w);
    assert!(!r.wants_frame(S), "no frame at the old size");
    assert!(r.frame_deadline(S).is_some());
    r.configure_surface(S, second, Scale::ONE);
    assert!(r.wants_frame(S), "the configure releases it");
    assert!(r.frame_deadline(S).is_none());
    // Bounded by the output: a size the compositor cannot give is not
    // waited for.
    r.set_surface_bounds(S, Some(LogicalSize::new(second.w as f32 - 20.0, 1080.0)));
    let mut d = SceneDiff::new();
    d.set(
        t,
        Prop::Text,
        text("a much longer line of text, and longer still"),
    );
    r.apply(d);
    r.configure_surface(S, Size::new(second.w - 20, second.h), Scale::ONE);
    assert!(
        r.wants_frame(S),
        "clamped to its output: nothing to wait for"
    );
}

/// Shadows on a centred surface: the buffer is grown evenly on its
/// centred axes, so the box (and its input region) stays centred.
#[test]
fn a_centred_surface_gets_an_even_overhang() {
    for (anchor, even_v, even_h) in [
        ("center", true, true),
        ("top", false, true),
        ("left", true, false),
        ("top_left", false, false),
    ] {
        let mut b = Builder::default();
        let p = b.node(
            NodeKind::Panel,
            None,
            vec![
                (Prop::Anchor, kw(anchor)),
                (Prop::Width, num(600.0)),
                (Prop::Height, num(200.0)),
                (
                    Prop::Shadow,
                    PropValue::Shadow(vec![Shadow {
                        x: 4.0,
                        y: 16.0,
                        blur: 48.0,
                        spread: 0.0,
                        color: hex("#00000066"),
                    }]),
                ),
            ],
        );
        let mut r = renderer();
        assert!(r.apply(b.diff).is_empty());
        let o = r.surface_spec(p).unwrap().overhang;
        assert_eq!(o.top == o.bottom, even_v, "{anchor}: {o:?}");
        assert_eq!(o.left == o.right, even_h, "{anchor}: {o:?}");
        assert_eq!(o.bottom, 1.5 * 48.0 + 1.0 + 16.0, "{anchor}");
    }
}

/// One watched node shown on two surfaces of different sizes (a bar on
/// two monitors) reports a size when either surface's size of it
/// changes, and never flip-flops between them as each lays out again.
#[test]
fn a_node_on_two_surfaces_reports_each_change_once() {
    let mut ids = Vec::new();
    let mut b = Builder::default();
    let bar = b.node(
        NodeKind::Bar,
        None,
        vec![
            (Prop::Height, num(30.0)),
            (Prop::Font, PropValue::Font(font(13.0))),
        ],
    );
    let row = b.node(NodeKind::Row, Some(bar), vec![(Prop::Watch, kw("query"))]);
    ids.push(row);
    ids.push(b.node(NodeKind::Text, Some(row), vec![(Prop::Text, text("12:00"))]));
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    let (a, c) = (SurfaceId(1), SurfaceId(2));
    r.attach_surface(a, bar);
    r.attach_surface(c, bar);
    let mut wide = Buffer::new(400, 30, Scale::ONE);
    let mut narrow = Buffer::new(300, 30, Scale::ONE);
    wide.paint(&mut r, a, 0);
    narrow.paint(&mut r, c, 0);
    let facts = r.take_layout_facts();
    assert_eq!(facts.len(), 2, "{facts:?}");
    for i in 0..5 {
        let mut d = SceneDiff::new();
        d.set(ids[1], Prop::Text, text(&format!("12:0{i}")));
        r.apply(d);
        wide.paint(&mut r, a, 1);
        narrow.paint(&mut r, c, 1);
        assert!(
            r.take_layout_facts().is_empty(),
            "tick {i}: sizes unchanged"
        );
    }
}

/// A closed content-sized surface (a launcher at boot) is neither laid
/// out nor shaped; opening it sizes it.
#[test]
fn a_closed_surface_is_not_laid_out() {
    let mut b = Builder::default();
    let p = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Open, PropValue::Bool(false)),
            (Prop::Font, PropValue::Font(font(13.0))),
        ],
    );
    let t = b.node(NodeKind::Text, Some(p), vec![(Prop::Text, text("Firefox"))]);
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    assert_eq!(r.layout_passes(), 0);
    assert_eq!(r.text_slots(), 0);
    let mut d = SceneDiff::new();
    d.set(t, Prop::Text, text("Firefox Web Browser"));
    r.apply(d);
    assert_eq!(r.layout_passes(), 0, "an update to a closed surface");
    let mut d = SceneDiff::new();
    d.set(p, Prop::Open, PropValue::Bool(true));
    r.apply(d);
    let spec = r.surface_spec(p).unwrap();
    assert!(spec.open && spec.width.unwrap() > 100.0, "{spec:?}");
    assert!(r.text_slots() > 0);
}

/// A shown surface of a fixed size runs one layout pass for a text
/// change (its overhang comes from that pass, no content pass runs), and
/// none for a paint-only change after it.
#[test]
fn a_fixed_surface_lays_out_once_per_change() {
    let (d, root, lst) = long_list(2000);
    let (mut r, mut buf) = show(d, root, 200, 420, Scale::ONE);
    let first = r.tree().get(lst).unwrap().children[0];
    let label = r.tree().get(first).unwrap().children[1];
    let before = r.layout_passes();
    let mut d = SceneDiff::new();
    d.set(label, Prop::Text, text("Row zero"));
    r.apply(d);
    buf.paint(&mut r, S, 1);
    let spent = r.layout_passes() - before;
    // The change, then its delivered layout (inline shaping); no content
    // pass for either.
    assert!(spent <= 2, "{spent} passes");
    let before = r.layout_passes();
    let mut d = SceneDiff::new();
    d.set(label, Prop::Color, color("#f38ba8"));
    r.apply(d);
    buf.paint(&mut r, S, 1);
    assert_eq!(r.layout_passes(), before, "paint-only");
}

/// An `input` draws its `text`, or its `placeholder` in `$fg.muted`
/// while the text is empty.
#[test]
fn an_input_shows_its_text_or_placeholder() {
    let mut ids = Vec::new();
    let (mut d, root) = panel(200, 60, |b, root| {
        let col = b.node(
            NodeKind::Col,
            Some(root),
            vec![(Prop::Pad, num(6.0)), (Prop::Gap, num(6.0))],
        );
        for t in ["", "fire"] {
            ids.push(b.node(
                NodeKind::Input,
                Some(col),
                vec![
                    (Prop::Text, text(t)),
                    (Prop::Placeholder, text("Search apps")),
                ],
            ));
        }
    });
    let mut t = TokenTable::default();
    t.insert("fg.muted", color("#6c7086"));
    d.set_tokens(t, Transition::Instant);
    let (r, buf) = show(d, root, 200, 60, Scale::ONE);
    // Glyphs inside each input's box: the muted placeholder, the text.
    let inked = |id: NodeId| {
        let b = rect(&r, id);
        let mut found = Vec::new();
        for y in b.y as u32..(b.y + b.h) as u32 {
            for x in b.x as u32..(b.x + b.w) as u32 {
                let p = buf.px(x, y);
                if p != [0x2e, 0x1e, 0x1e, 0xff] {
                    found.push(p);
                }
            }
        }
        found
    };
    let (ph, tx) = (inked(ids[0]), inked(ids[1]));
    assert!(ph.len() > 20 && tx.len() > 10, "{} {}", ph.len(), tx.len());
    // The placeholder's brightest pixel is the muted colour, the text's
    // the inherited one (#cdd6f4).
    let max = |v: &[[u8; 4]]| {
        v.iter()
            .map(|p| p[0] as u32 + p[1] as u32 + p[2] as u32)
            .max()
    };
    assert!(max(&ph) < max(&tx), "{:?} {:?}", max(&ph), max(&tx));
    assert_matches_ref("layout_input", &buf, TOLERANCE);
}

/// A shadowed toast sliding in (`enter { x: 420 }`, the x offset
/// stepped over ten frames) never changes its surface's spec: the
/// overhang comes from the shadow at rest, so the buffer keeps its size
/// and the frames run no layout pass.
#[test]
fn an_animated_offset_never_resizes_its_surface() {
    let mut b = Builder::default();
    let p = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Anchor, kw("top_right")),
            (Prop::Font, PropValue::Font(font(13.0))),
        ],
    );
    let toast = b.node(
        NodeKind::Col,
        Some(p),
        vec![
            (Prop::Width, num(380.0)),
            (Prop::Pad, num(12.0)),
            (Prop::X, num(420.0)),
            (
                Prop::Shadow,
                PropValue::Shadow(vec![Shadow {
                    x: 0.0,
                    y: 8.0,
                    blur: 24.0,
                    spread: 0.0,
                    color: hex("#0000004d"),
                }]),
            ),
        ],
    );
    b.node(
        NodeKind::Text,
        Some(toast),
        vec![(Prop::Text, text("Saved"))],
    );
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    let spec = r.surface_spec(p).unwrap().clone();
    let o = spec.overhang;
    assert_eq!((o.left, o.right), (37.0, 37.0), "at rest: {o:?}");
    r.take_surface_changes();
    r.attach_surface(S, p);
    let (w, h) = (
        (spec.width.unwrap() + o.left + o.right) as u32,
        (spec.height.unwrap() + o.top + o.bottom) as u32,
    );
    let mut buf = Buffer::new(w, h, Scale::ONE);
    buf.paint(&mut r, S, 0);
    let passes = r.layout_passes();
    for i in 1..=10 {
        let mut d = SceneDiff::new();
        d.set(toast, Prop::X, num(420.0 - 42.0 * i as f32));
        r.apply(d);
        buf.paint(&mut r, S, 1);
    }
    assert!(r.take_surface_changes().is_empty(), "a spec changed");
    assert_eq!(r.layout_passes(), passes, "an offset relayouts nothing");
    assert_eq!(r.surface_spec(p).unwrap(), &spec);
}

/// A content-sized panel holding a short list of rows taller than the
/// row estimate (the design's launcher) asks for its measured size at
/// once: the content pass settles the list as the painted pass does, so
/// the surface is never configured at an estimated size first.
#[test]
fn a_content_sized_list_asks_for_its_measured_size() {
    let mut b = Builder::default();
    let p = b.node(
        NodeKind::Panel,
        None,
        vec![(Prop::Font, PropValue::Font(font(13.0)))],
    );
    let l = b.node(NodeKind::List, Some(p), vec![(Prop::Width, num(300.0))]);
    // Icon rows (no text to shape, so no second content pass on its
    // delivery settles the list by chance).
    for _ in 0..3 {
        let row = b.node(NodeKind::Row, Some(l), vec![(Prop::Pad, num(12.0))]);
        swatch(&mut b, row, "#89b4fa", vec![(Prop::Size, num(24.0))]);
    }
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    let spec = r.surface_spec(p).unwrap().clone();
    let (w, h) = (spec.width.unwrap(), spec.height.unwrap());
    let est = strand_render::LIST_ROW_ESTIMATE;
    assert_eq!(h, 3.0 * 48.0, "three 48 px rows, not 3 × {est}");
    r.take_surface_changes();
    r.attach_surface(S, p);
    let mut buf = Buffer::new(w as u32, h as u32, Scale::ONE);
    buf.paint(&mut r, S, 0);
    assert!(r.take_surface_changes().is_empty(), "painted at its size");
    assert_eq!(r.boxes(S).unwrap().rows_laid_out, 3);
}

/// A content-sized surface whose spec changed after it was created but
/// before its first configure (its text arrived meanwhile) holds its first
/// frame when configured at the size it was created with, until the
/// configure at the new size.
#[test]
fn a_first_configure_at_a_stale_size_is_held() {
    let mut b = Builder::default();
    let p = b.node(
        NodeKind::Panel,
        None,
        vec![(Prop::Font, PropValue::Font(font(13.0)))],
    );
    let t = b.node(NodeKind::Text, Some(p), vec![(Prop::Text, text("short"))]);
    let mut r = renderer();
    r.set_resize_wait(std::time::Duration::from_secs(30));
    assert!(r.apply(b.diff).is_empty());
    let size = |r: &strand_render::Renderer| {
        let s = r.surface_spec(p).unwrap();
        Size::new(s.width.unwrap() as u32, s.height.unwrap() as u32)
    };
    let created = size(&r);
    r.attach_surface(S, p);
    let mut d = SceneDiff::new();
    d.set(t, Prop::Text, text("a much longer line of text"));
    r.apply(d);
    let wanted = size(&r);
    assert!(wanted.w > created.w);
    r.configure_surface(S, created, Scale::ONE);
    assert!(!r.wants_frame(S), "no first frame at the stale size");
    r.configure_surface(S, wanted, Scale::ONE);
    assert!(
        r.wants_frame(S),
        "the configure at the new size releases it"
    );
}

/// An `input` whose text is wider than its box keeps it on one line:
/// clipped to the box and shifted so its end, where typing happens, is
/// in view; nothing draws below or beside the box.
#[test]
fn an_overlong_input_stays_on_one_line() {
    let mut id = None;
    let (d, root) = panel(300, 100, |b, root| {
        let col = b.node(NodeKind::Col, Some(root), vec![(Prop::Pad, num(10.0))]);
        id = Some(b.node(
            NodeKind::Input,
            Some(col),
            vec![
                (Prop::Width, num(100.0)),
                (
                    Prop::Text,
                    text("a quite long search query that does not fit"),
                ),
            ],
        ));
    });
    let (r, buf) = show(d, root, 300, 100, Scale::ONE);
    let b = rect(&r, id.unwrap());
    let bg = [0x2e, 0x1e, 0x1e, 0xff];
    let lit = |x: u32, y: u32| buf.px(x, y) != bg;
    let (x0, y0, x1, y1) = (
        b.x as u32,
        b.y as u32,
        (b.x + b.w).ceil() as u32,
        (b.y + b.h).ceil() as u32,
    );
    let mut outside = 0;
    for y in 0..100 {
        for x in 0..300 {
            if lit(x, y) && !(x0..x1).contains(&x) | !(y0..y1).contains(&y) {
                outside += 1;
            }
        }
    }
    assert_eq!(outside, 0, "text drawn outside its {b:?}");
    // The end of the text is in view: ink near the box's right edge.
    assert!(
        (y0..y1).any(|y| (x1 - 12..x1).any(|x| lit(x, y))),
        "the end of the text is not in view"
    );
    assert_matches_ref("layout_input_overlong", &buf, TOLERANCE);
}

/// A `place: absolute` node is placed by its `x`/`y`, so its shadow's
/// reach counts from there: moving it past the box grows the overhang.
#[test]
fn an_absolute_shadow_counts_from_its_coordinates() {
    let mut b = Builder::default();
    let p = b.node(
        NodeKind::Panel,
        None,
        vec![(Prop::Width, num(200.0)), (Prop::Height, num(100.0))],
    );
    let card = b.node(
        NodeKind::Box,
        Some(p),
        vec![
            (Prop::Place, kw("absolute")),
            (Prop::X, num(150.0)),
            (Prop::Size, num(40.0)),
            (
                Prop::Shadow,
                PropValue::Shadow(vec![Shadow {
                    x: 0.0,
                    y: 0.0,
                    blur: 4.0,
                    spread: 0.0,
                    color: hex("#000000"),
                }]),
            ),
        ],
    );
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    // 150 + 40 + reach 7 = 197: inside the box.
    assert_eq!(r.surface_spec(p).unwrap().overhang.right, 0.0);
    let mut d = SceneDiff::new();
    d.set(card, Prop::X, num(170.0));
    r.apply(d);
    // 170 + 40 + 7 = 217: 17 past it.
    assert_eq!(r.surface_spec(p).unwrap().overhang.right, 17.0);
}

/// An `image` with no size is 16 × 16 (like an icon) before and after its
/// source decodes, and square to a single side given: it never lays out
/// as 0 × 0 and silently draws nothing.
#[test]
fn unsized_images_are_square() {
    let mut ids = Vec::new();
    let (d, root) = panel(200, 60, |b, root| {
        let row = b.node(NodeKind::Row, Some(root), vec![(Prop::Align, kw("start"))]);
        ids.push(b.node(
            NodeKind::Image,
            Some(row),
            vec![(Prop::Source, text("network-wireless"))],
        ));
        ids.push(b.node(
            NodeKind::Image,
            Some(row),
            vec![
                (Prop::Source, text("/nonexistent.png")),
                (Prop::Width, num(40.0)),
            ],
        ));
        ids.push(b.node(
            NodeKind::Image,
            Some(row),
            vec![
                (Prop::Source, text("/nonexistent.png")),
                (Prop::Width, num(30.0)),
                (Prop::Height, num(10.0)),
            ],
        ));
    });
    let (r, _buf) = show(d, root, 200, 60, Scale::ONE);
    approx(rect(&r, ids[0]), (0.0, 0.0, 16.0, 16.0));
    approx(rect(&r, ids[1]), (16.0, 0.0, 40.0, 40.0));
    approx(rect(&r, ids[2]), (56.0, 0.0, 30.0, 10.0));
}

/// A `tooltip { … }` element's content is not drawn yet (the checker's
/// `check::not_drawn_yet`): logic mounts it while its element is hovered,
/// and render neither lays it out nor paints it, so hovering moves
/// nothing.
#[test]
fn a_tooltip_element_takes_no_room_and_draws_nothing() {
    let build = |tip: bool| {
        panel(200, 40, |b, root| {
            let row = b.node(NodeKind::Row, Some(root), vec![(Prop::Gap, num(4.0))]);
            let host = b.node(NodeKind::Box, Some(row), vec![]);
            b.node(NodeKind::Text, Some(host), vec![(Prop::Text, text("x"))]);
            if tip {
                let t = b.node(NodeKind::Tooltip, Some(host), vec![]);
                b.node(NodeKind::Text, Some(t), vec![(Prop::Text, text("TIP"))]);
            }
            swatch(
                b,
                row,
                "#f38ba8",
                vec![(Prop::Width, num(20.0)), (Prop::Height, num(20.0))],
            );
        })
    };
    let (diff, root) = build(false);
    let (_, plain) = show(diff, root, 200, 40, Scale::ONE);
    let (diff, root) = build(true);
    let (r, tipped) = show(diff, root, 200, 40, Scale::ONE);
    assert!(
        plain.pixels == tipped.pixels,
        "the tooltip element was drawn"
    );
    let laid = &r.boxes(S).unwrap().rects;
    assert!(
        laid.keys().all(|id| r
            .tree()
            .get(*id)
            .is_none_or(|n| n.kind != NodeKind::Tooltip)),
        "the tooltip element was laid out"
    );
}
