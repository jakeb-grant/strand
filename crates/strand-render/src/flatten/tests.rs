use super::paint::{corners_of, radii};
use super::text::{marks, place_text};
use super::*;
use strand_scene::{Corners, NodeKind};
use strand_scene::{SceneDiff, SceneOp, Transition};
use strand_text::{TextAlign, TextStyle};

fn id(i: u32) -> NodeId {
    NodeId::new(i, 0)
}

fn tree() -> SceneTree {
    let mut t = SceneTree::new();
    let mut d = SceneDiff::new();
    d.create(id(0), NodeKind::Bar, None, 0)
        .set(id(0), Prop::Bg, PropValue::Color(Color::WHITE))
        .create(id(1), NodeKind::Box, Some(id(0)), 0)
        .set(id(1), Prop::X, PropValue::Number(10.0))
        .set(id(1), Prop::Y, PropValue::Number(4.0))
        .set(id(1), Prop::Size, PropValue::Number(8.0))
        .set(id(1), Prop::Bg, PropValue::Color(Color::BLACK));
    assert!(t.apply(d).is_empty());
    t
}

struct NoText;
impl crate::layout::TextSizes for NoText {
    fn natural(&self, _: NodeId) -> Option<strand_scene::LogicalSize> {
        None
    }
    fn fitted(&self, _: NodeId, _: f32) -> Option<strand_scene::LogicalSize> {
        None
    }
}

/// Lays out and flattens the surface `id(0)`.
fn flat(t: &SceneTree, size: Size, scale: Scale) -> Flattened {
    let l = scale.logical_size(size);
    let boxes = crate::layout::layout(
        t,
        id(0),
        crate::layout::RootSize::Fixed(LogicalRect::new(0.0, 0.0, l.w, l.h)),
        &NoText,
        &mut HashMap::new(),
        &HashMap::new(),
    );
    flatten(
        t,
        id(0),
        size,
        scale,
        &HashMap::new(),
        &boxes,
        &mut Animator::default(),
        &Extras::default(),
    )
}

/// Text that fits its box is drawn from its unbounded layout, placed
/// by `align` and centred vertically; a narrower box asks for a layout
/// of its width and draws the unbounded one until it arrives.
#[test]
fn text_is_aligned_in_its_box() {
    use strand_text::{FontConfig, TextEngine, TextKey, TextRequest, test_font_path};
    let data = std::fs::read(test_font_path()).unwrap();
    let mut engine = TextEngine::new(FontConfig::isolated(vec![Arc::new(data)]));
    let l = Arc::new(engine.layout(&TextRequest {
        key: TextKey(1),
        text: "12:59".into(),
        style: TextStyle::default(),
        max_width: None,
        scale: Scale::ONE,
    }));
    let shaped = vec![Shaped {
        layout: l.clone(),
        max_width: None,
        part: 0,
    }];
    let w = l.size.w;
    let rect = LogicalRect::new(0.0, 0.0, 100.0, l.size.h + 10.0);
    for (align, want) in [
        (TextAlign::Start, 0.0),
        (TextAlign::Center, (100.0 - w) / 2.0),
        (TextAlign::End, 100.0 - w),
    ] {
        let (fit, placed) = place_text(&shaped, Scale::ONE, rect, align);
        let (_, dx, dy) = placed.unwrap();
        assert_eq!(fit, None);
        assert!((dx - want).abs() < 1e-3, "{align:?}: {dx} vs {want}");
        let want_dy = if align == TextAlign::Center { 5.0 } else { 0.0 };
        assert!((dy - want_dy).abs() < 1e-3);
    }
    let narrow = LogicalRect::new(0.0, 0.0, (w / 2.0).round(), l.size.h);
    let (fit, placed) = place_text(&shaped, Scale::ONE, narrow, TextAlign::End);
    assert_eq!(fit, Some((w / 2.0).round()));
    assert_eq!(placed.unwrap().1, 0.0, "the stand-in is not aligned");
}

#[test]
fn absolute_layout_and_records() {
    let t = tree();
    let f = flat(&t, Size::new(100, 20), Scale::ONE);
    assert_eq!(f.records[&id(0)].bounds, Rect::new(0, 0, 100, 20));
    assert_eq!(f.records[&id(1)].bounds, Rect::new(10, 4, 8, 8));
    assert_eq!(f.items.len(), 2);
}

#[test]
fn fractional_scale_snaps_edges() {
    let t = tree();
    let s = Scale::new(150).unwrap();
    let f = flat(&t, Size::new(125, 25), s);
    // 10 × 1.25 = 12.5 → 13, 18 × 1.25 = 22.5 → 23.
    assert_eq!(f.records[&id(1)].bounds, Rect::new(13, 5, 10, 10));
}

#[test]
fn signatures_track_paint_changes_only() {
    let mut t = tree();
    let a = flat(&t, Size::new(100, 20), Scale::ONE);
    // Same value again: same signature.
    t.apply_op(SceneOp::SetProp {
        id: id(1),
        prop: Prop::Bg,
        value: PropValue::Color(Color::BLACK),
        transition: Transition::Instant,
    })
    .unwrap();
    let b = flat(&t, Size::new(100, 20), Scale::ONE);
    assert_eq!(a.records, b.records);
    t.apply_op(SceneOp::SetProp {
        id: id(0),
        prop: Prop::Opacity,
        value: PropValue::Number(0.5),
        transition: Transition::Instant,
    })
    .unwrap();
    let c = flat(&t, Size::new(100, 20), Scale::ONE);
    // Parent opacity changes how the child paints.
    assert_ne!(a.records[&id(1)].sig, c.records[&id(1)].sig);
}

#[test]
fn radius_full_is_a_pill_in_every_encoding() {
    let full = [
        PropValue::Keyword("full".into()),
        PropValue::Corners(Corners::FULL),
        PropValue::Number(f32::INFINITY),
        PropValue::Length(Length::Percent(50.0)),
    ];
    for v in full {
        let c = corners_of(Some(&v), 40.0, 10.0);
        let r = radii(c, 40.0, 10.0, 1.0);
        assert_eq!(r.top_left, 5.0, "{v:?}");
        assert_eq!(r.bottom_right, 5.0, "{v:?}");
    }
    for v in [PropValue::Number(f32::NAN), PropValue::Number(-3.0)] {
        assert!(corners_of(Some(&v), 40.0, 10.0).is_zero(), "{v:?}");
    }
}

#[test]
fn radius_lists_expand_like_css() {
    let top = PropValue::List(vec![
        PropValue::Number(14.0),
        PropValue::Number(14.0),
        PropValue::Number(0.0),
        PropValue::Number(0.0),
    ]);
    let c = corners_of(Some(&top), 100.0, 40.0);
    assert_eq!((c.top_left, c.top_right, c.bottom_right), (14.0, 14.0, 0.0));
    let pair = PropValue::List(vec![
        PropValue::Keyword("full".into()),
        PropValue::Length(Length::Percent(10.0)),
    ]);
    let c = corners_of(Some(&pair), 100.0, 40.0);
    assert_eq!(
        (c.top_left, c.top_right, c.bottom_right),
        (MAX_LOGICAL, 4.0, MAX_LOGICAL)
    );
    let bad = PropValue::List(vec![PropValue::Number(1.0); 5]);
    assert!(corners_of(Some(&bad), 100.0, 40.0).is_zero());
}

#[test]
fn marks_become_coloured_spans_on_char_ranges() {
    let pair = |a: f32, b: f32| PropValue::List(vec![PropValue::Number(a), PropValue::Number(b)]);
    let v = PropValue::List(vec![pair(0.0, 1.0), pair(2.0, 9.0), pair(3.0, 3.0)]);
    let s = marks("héllo", Some(&v), || Some(Color::WHITE));
    let r: Vec<_> = s.iter().map(|s| s.range.clone()).collect();
    assert_eq!(r, vec![0..1, 3..6]);
    assert!(s.iter().all(|s| s.color == Some(Color::WHITE)));
    let s = marks("héllo", Some(&v), || None);
    assert_eq!(s[0].weight, Some(700));
}

#[test]
fn radii_shrink_like_css() {
    let r = radii(Corners::all(999.0), 40.0, 10.0, 1.0);
    assert_eq!(r.top_left, 5.0);
    let r = radii(
        Corners {
            top_left: 14.0,
            top_right: 14.0,
            bottom_right: 0.0,
            bottom_left: 0.0,
        },
        100.0,
        100.0,
        1.5,
    );
    assert_eq!((r.top_left, r.bottom_left), (21.0, 0.0));
}
