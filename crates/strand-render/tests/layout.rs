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
    // blur 24: reach 1.5 × 24 + 1 = 37, offset 8 down.
    let o = spec.overhang;
    assert_eq!((o.top, o.bottom, o.left, o.right), (29.0, 45.0, 37.0, 37.0));
    let bar_spec = r.surface_spec(bar).unwrap();
    let t = bar_spec.height.unwrap();
    assert!(t > 20.0 && t < 32.0, "thickness from content: {t}");
    assert_eq!(bar_spec.exclusive_zone(), Some(t));

    // Shown at its size plus the overhang, the column sits inside it.
    r.attach_surface(S, p);
    let (w, hh) = (200 + 74, h as u32 + 29 + 45);
    let mut buf = Buffer::new(w, hh, Scale::ONE);
    buf.paint(&mut r, S, 0);
    approx(rect(&r, col), (37.0, 29.0, 200.0, h));
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
