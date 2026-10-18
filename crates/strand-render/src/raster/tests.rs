use super::*;

#[test]
fn disjoint_covers_exactly_without_overlap() {
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut rnd = move |n: i64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n as u64) as i64
    };
    for _ in 0..500 {
        let mut d = Damage::new();
        for _ in 0..rnd(12) {
            d.add(Rect::new(
                rnd(60) as i32,
                rnd(60) as i32,
                rnd(30) as u32,
                rnd(30) as u32,
            ));
        }
        let parts = disjoint(&d);
        let sum: u64 = parts.iter().map(|r| r.area()).sum();
        assert_eq!(sum, d.area(), "{d:?} -> {parts:?}");
        for (i, a) in parts.iter().enumerate() {
            assert!(d.covers(*a));
            for b in &parts[i + 1..] {
                assert!(!a.intersects(*b), "{a:?} overlaps {b:?}");
            }
        }
    }
}

/// (gradient-cpu-gpu) The stops vello is given follow the OKLab ramp to
/// within half a level everywhere between them, for colours far apart in
/// hue and near, opaque and translucent; colours near in hue need few.
#[test]
fn ramp_samples_follow_the_oklab_ramp_within_half_a_level() {
    use strand_scene::{Color, GradientStop};
    let hex = |s: &str| Color::from_hex(s).unwrap();
    for (a, b) in [
        ("#ff0000", "#0000ff"),
        ("#00ff00", "#ff00ff"),
        ("#f38ba8", "#89b4fa"),
        ("#ffffff00", "#000000"),
        ("#ff000080", "#00ffff"),
    ] {
        let (a, b) = (hex(a), hex(b));
        let stops = [
            GradientStop {
                offset: 0.0,
                color: a,
            },
            GradientStop {
                offset: 1.0,
                color: b,
            },
        ];
        let ramp = super::paint::ramp(&stops);
        for i in 0..=1000 {
            let t = i as f32 / 1000.0;
            let j = ramp
                .windows(2)
                .position(|w| t <= w[1].0)
                .unwrap_or(ramp.len() - 2);
            let ((o0, c0), (o1, c1)) = (ramp[j], ramp[j + 1]);
            let q = (t - o0) / (o1 - o0);
            let exact = a.lerp_oklab(b, t).clamped();
            for (x, y, e) in [
                (c0.r, c1.r, exact.r),
                (c0.g, c1.g, exact.g),
                (c0.b, c1.b, exact.b),
                (c0.a, c1.a, exact.a),
            ] {
                let d = ((x + (y - x) * q) - e).abs() * 255.0;
                assert!(d <= 0.5 + 1e-3, "{a:?} to {b:?} at {t}: {d} levels off");
            }
        }
    }
    let near = super::paint::ramp(&[
        GradientStop {
            offset: 0.0,
            color: hex("#313244"),
        },
        GradientStop {
            offset: 1.0,
            color: hex("#45475a"),
        },
    ]);
    assert!(near.len() <= 5, "{} samples for near colours", near.len());
}
