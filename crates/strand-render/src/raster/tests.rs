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

/// A pixel a partial repaint drew otherwise: the damage, where, and the
/// partial and full repaints' pixels.
type Miss = (Rect, i32, i32, [u8; 4], [u8; 4]);

/// The pixels of `items` painted over a `w × h` surface within damage
/// rects whose edges fall at every offset from vello's 4 × 4 tiles
/// (`at` and `size` the first rect; each next one a pixel further right
/// or down and larger) that differ from a full repaint's.
fn partial_against_full(
    items: &[DisplayItem],
    (w, h): (u32, u32),
    at: (i32, i32),
    size: (u32, u32),
) -> Vec<Miss> {
    use strand_scene::{PaintTarget, Size};
    let atlas = AtlasMirror::default();
    let paint = |damage: &Damage| {
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let mut t = PaintTarget::new(&mut buf, Size::new(w, h), w * 4, Scale::ONE, 0).unwrap();
        Raster::default().paint(items, damage, &atlas, Scale::ONE, &mut t);
        buf
    };
    let full = paint(&Damage::full(Size::new(w, h)));
    let mut bad = Vec::new();
    for dx in 0..4 {
        for dy in 0..8 {
            let r = Rect::new(at.0 + dx, at.1 + dy, size.0 + dy as u32, size.1 + dx as u32);
            let mut d = Damage::new();
            d.add(r);
            let part = paint(&d);
            for y in r.top()..r.bottom() {
                for x in r.left()..r.right() {
                    let i = (y as usize * w as usize + x as usize) * 4;
                    if part[i..i + 4] != full[i..i + 4] {
                        let px = |b: &[u8]| [b[i], b[i + 1], b[i + 2], b[i + 3]];
                        bad.push((r, x as i32, y as i32, px(&part), px(&full)));
                    }
                }
            }
        }
    }
    bad
}

/// (image-redraw) An opaque image decoded at another size and drawn
/// resampled over its box's opaque fill (a stand-in while the box
/// springs) repaints within any damage exactly as a full repaint draws
/// it. Before, the decode claimed to be opaque, though its bilinear
/// samples are not quite (four f32 weights summing to a hair under 1):
/// vello overwrote the fill with them in a tile wholly inside the clip
/// and blended them over it in a tile a damage rect's edge cut, so the
/// fill showed through by a hair there, a channel a level off here and
/// there (203 of the damaged pixels here).
#[test]
fn a_resampled_image_repaints_in_part_as_in_full() {
    use crate::image::{Fit, IconTheme, ImageKey};
    use strand_scene::{Color, Paint};
    use vello_cpu::kurbo;
    // A smooth, opaque 27 × 13 PNG, decoded as an image node's is.
    let dir = std::env::temp_dir().join(format!("strand-image-redraw-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("ramp.png");
    {
        let f = std::fs::File::create(&path).unwrap();
        let mut e = png::Encoder::new(std::io::BufWriter::new(f), 27, 13);
        e.set_color(png::ColorType::Rgb);
        e.set_depth(png::BitDepth::Eight);
        let mut data = Vec::new();
        for y in 0..13u8 {
            for x in 0..27u8 {
                data.extend([220 - x * 2 - y, 60 + y * 9 + x, 40 + x * 5]);
            }
        }
        let mut wr = e.write_header().unwrap();
        wr.write_image_data(&data).unwrap();
    }
    let key = ImageKey {
        source: path.to_string_lossy().into_owned(),
        icon: false,
        w: 27,
        h: 13,
        fit: Fit::Fill,
        scale: 1,
        frame: 0,
    };
    let decoded = crate::image::load(&key, &IconTheme::named("hicolor")).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let boxed = kurbo::Rect::new(7.0, 13.0, 75.0, 67.0);
    let items = vec![
        DisplayItem {
            item: Item::Fill {
                shape: crate::flatten::FillShape::Rect(boxed),
                paint: Paint::Solid(Color::from_hex("#ffffff").unwrap()),
                frame: boxed,
            },
            bounds: Rect::new(7, 13, 68, 54),
        },
        DisplayItem {
            item: Item::Image {
                pixmap: decoded.pixmap,
                rect: boxed,
                // Scaled by 3 and half a pixel off the grid: its samples
                // fall on sixths, where a level is a near tie.
                dest: kurbo::Rect::new(0.5, 20.5, 81.5, 59.5),
                tint: None,
            },
            bounds: Rect::new(7, 13, 68, 54),
        },
    ];
    let bad = partial_against_full(&items, (128, 96), (20, 17), (23, 5));
    assert!(
        bad.is_empty(),
        "{} pixels differ: {:?}",
        bad.len(),
        &bad[..bad.len().min(6)]
    );
}
