use super::motion::Enc;
use super::*;
use strand_scene::{Border, Corners, GradientStop, Length, Paint};

#[test]
fn values_round_trip_through_channels() {
    let white = Color::WHITE;
    let b = Extents {
        own: (80.0, 30.0),
        parent: (200.0, 100.0),
    };
    for (p, v) in [
        (Prop::X, PropValue::Number(4.0)),
        (Prop::Rotate, PropValue::Angle(30.0)),
        (
            Prop::Bg,
            PropValue::Color(Color::from_rgba8(10, 200, 30, 255)),
        ),
        (
            Prop::Border,
            PropValue::Border(Border {
                width: 2.0,
                paint: Paint::Solid(Color::BLACK),
            }),
        ),
        (Prop::Radius, PropValue::Corners(Corners::all(6.0))),
        (
            Prop::Fill,
            PropValue::Paint(Paint::Solid(Color::from_rgba8(10, 200, 30, 255))),
        ),
    ] {
        let e = encode(p, Some(&v), white, b).unwrap();
        let back = decode(p, &e);
        let e2 = encode(p, Some(&back), white, b).unwrap();
        assert_eq!(e, e2, "{p:?}");
    }
    // Lengths resolve against the boxes: a percentage offset of the
    // parent's, `radius: full` half the node's shorter side.
    let pct = |v: f32| PropValue::Length(Length::Percent(v));
    assert_eq!(
        encode(Prop::X, Some(&pct(5.0)), white, b),
        Some(Enc::One([10.0]))
    );
    assert_eq!(
        encode(Prop::Y, Some(&pct(50.0)), white, b),
        Some(Enc::One([50.0]))
    );
    let full = Some(Enc::Four([15.0; 4]));
    assert_eq!(
        encode(
            Prop::Radius,
            Some(&PropValue::Corners(Corners::FULL)),
            white,
            b
        ),
        full
    );
    assert_eq!(
        encode(
            Prop::Radius,
            Some(&PropValue::Keyword("full".into())),
            white,
            b
        ),
        full
    );
    // A gradient cannot interpolate: it snaps.
    let grad = PropValue::Paint(Paint::Linear {
        angle: 0.0,
        stops: vec![
            GradientStop {
                offset: 0.0,
                color: Color::BLACK,
            },
            GradientStop {
                offset: 1.0,
                color: Color::WHITE,
            },
        ],
    });
    assert_eq!(encode(Prop::Bg, Some(&grad), white, b), None);
    assert_eq!(encode(Prop::Fill, Some(&grad), white, b), None);
    // No `fill` draws in `color`: nothing to spring from.
    assert_eq!(encode(Prop::Fill, None, white, b), None);
    // `blur:` is the compositor's (its region is re-sent only when the
    // shape changes): it snaps.
    assert_eq!(
        encode(Prop::Blur, Some(&PropValue::Number(6.0)), white, b),
        None
    );
    assert_eq!(encode(Prop::Opacity, None, white, b), Some(Enc::One([1.0])));
}

#[test]
fn presets_expand() {
    assert_eq!(
        pose_props(&PropValue::Keyword("fade".into()), None),
        vec![(Prop::Opacity, PropValue::Number(0.0))]
    );
    let popin = PropValue::Call {
        name: "popin".into(),
        args: vec![PropValue::Number(0.8)],
    };
    assert_eq!(
        pose_props(&popin, None)[0],
        (Prop::Scale, PropValue::Number(0.8))
    );
    let slide = PropValue::Call {
        name: "slide".into(),
        args: vec![PropValue::Keyword("top".into())],
    };
    let r = LogicalRect::new(0.0, 0.0, 100.0, 30.0);
    assert_eq!(
        pose_props(&slide, Some(r)),
        vec![(Prop::Y, PropValue::Number(-30.0))]
    );
    assert!(pose_props(&PropValue::Keyword("none".into()), None).is_empty());
}
