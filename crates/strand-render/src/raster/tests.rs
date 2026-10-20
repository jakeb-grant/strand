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

/// (opaque-fast-path) Pixmaps claimed opaque repaint within any damage
/// exactly as a full repaint draws them: a decode drawn pixel for pixel
/// (half a pixel off the grid, which nearest sampling still takes pixel
/// for pixel) over its box's fill, where vello overwrites the fill in a
/// tile wholly inside the damage and blends over it in one the damage's
/// edge cuts; and a gradient of opaque stops turned 20 degrees, sampled
/// smoothly from its pixmap and drawn in a layer of its own.
#[test]
fn opaque_pixmaps_repaint_in_part_as_in_full() {
    use strand_scene::{Color, GradientStop, Paint};
    use vello_cpu::kurbo;
    let mut pm = vello_cpu::Pixmap::new(41, 29);
    for (i, p) in pm.data_mut().iter_mut().enumerate() {
        let (x, y) = (i as u32 % 41, i as u32 / 41);
        *p = vello_cpu::color::PremulRgba8 {
            r: (x * 6) as u8,
            g: (200 - y * 5) as u8,
            b: ((x * y) % 251) as u8,
            a: 255,
        };
    }
    pm.set_may_have_transparency(false);
    let pm = std::sync::Arc::new(pm);
    let white = |r: kurbo::Rect| DisplayItem {
        item: Item::Fill {
            shape: crate::flatten::FillShape::Rect(r),
            paint: Paint::Solid(Color::from_hex("#ffffff").unwrap()),
            frame: r,
        },
        bounds: Rect::new(0, 0, 128, 96),
    };
    let boxed = kurbo::Rect::new(7.5, 13.5, 48.5, 42.5);
    let items = vec![
        white(kurbo::Rect::new(0.0, 0.0, 128.0, 96.0)),
        DisplayItem {
            item: Item::Image {
                pixmap: pm,
                rect: boxed,
                dest: boxed,
                tint: None,
            },
            bounds: Rect::new(7, 13, 42, 30),
        },
    ];
    let bad = partial_against_full(&items, (128, 96), (20, 17), (23, 5));
    assert!(
        bad.is_empty(),
        "the decode: {} pixels differ: {:?}",
        bad.len(),
        &bad[..bad.len().min(6)]
    );
    let card = kurbo::Rect::new(30.0, 20.0, 100.0, 76.0);
    let turn = kurbo::Affine::rotate_about(20f64.to_radians(), card.center());
    let ramp = Paint::Linear {
        angle: 60.0,
        stops: vec![
            GradientStop {
                offset: 0.0,
                color: Color::from_hex("#1e66f5").unwrap(),
            },
            GradientStop {
                offset: 1.0,
                color: Color::from_hex("#ea76cb").unwrap(),
            },
        ],
    };
    let items = vec![
        white(kurbo::Rect::new(0.0, 0.0, 128.0, 96.0)),
        DisplayItem {
            item: Item::PushTransform(turn),
            bounds: Rect::new(14, 4, 100, 88),
        },
        DisplayItem {
            item: Item::Fill {
                shape: crate::flatten::FillShape::Rect(card),
                paint: ramp,
                frame: card,
            },
            bounds: Rect::new(14, 4, 100, 88),
        },
        DisplayItem {
            item: Item::PopTransform,
            bounds: Rect::new(14, 4, 100, 88),
        },
    ];
    let bad = partial_against_full(&items, (128, 96), (40, 30), (23, 5));
    assert!(
        bad.is_empty(),
        "the gradient: {} pixels differ: {:?}",
        bad.len(),
        &bad[..bad.len().min(6)]
    );
}

/// (opaque-fast-path) Not a gate: what claiming a pixmap opaque saves
/// when it covers a 4K surface, drawn pixel for pixel over the surface's
/// opaque fill (a lock backdrop, a wallpaper; a gradient's cached
/// pixmap is drawn by the same paint), and resampled (a stand-in, a
/// scale). Run with `cargo test --profile timing -p strand-render --lib
/// opaque_pixmap_frame_cost -- --ignored --nocapture --test-threads 1`.
#[test]
#[ignore = "measurement, not a gate"]
fn opaque_pixmap_frame_cost() {
    use strand_scene::{Color, Paint, PaintTarget, Size};
    use vello_cpu::kurbo;
    let (w, h) = (3840u32, 2160u32);
    let pixmap = |opaque: bool| {
        let mut pm = vello_cpu::Pixmap::new(w as u16, h as u16);
        for (i, p) in pm.data_mut().iter_mut().enumerate() {
            let (x, y) = (i as u32 % w, i as u32 / w);
            *p = vello_cpu::color::PremulRgba8 {
                r: (x / 15) as u8,
                g: (y / 9) as u8,
                b: ((x + y) / 24) as u8,
                a: 255,
            };
        }
        pm.set_may_have_transparency(!opaque);
        std::sync::Arc::new(pm)
    };
    let screen = kurbo::Rect::new(0.0, 0.0, w as f64, h as f64);
    let items = |pm: std::sync::Arc<vello_cpu::Pixmap>, dest: kurbo::Rect| {
        vec![
            DisplayItem {
                item: Item::Fill {
                    shape: crate::flatten::FillShape::Rect(screen),
                    paint: Paint::Solid(Color::from_hex("#1e1e2e").unwrap()),
                    frame: screen,
                },
                bounds: Rect::new(0, 0, w, h),
            },
            DisplayItem {
                item: Item::Image {
                    pixmap: pm,
                    rect: screen,
                    dest,
                    tint: None,
                },
                bounds: Rect::new(0, 0, w, h),
            },
        ]
    };
    // Rounds of a few frames each, the two claims taken in turn, so load
    // that comes and goes falls on both; each frame is timed, and the
    // medians are compared.
    let rounds: u32 = std::env::var("STRAND_OPAQUE_BENCH_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(12);
    let atlas = AtlasMirror::default();
    let mut buf = vec![0u8; (w * h * 4) as usize];
    let full = Damage::full(Size::new(w, h));
    for (what, dest) in [
        ("pixel for pixel", screen),
        (
            "resampled",
            kurbo::Rect::new(-0.5, -0.5, w as f64 + 1.5, h as f64 + 1.5),
        ),
    ] {
        let lists = [items(pixmap(true), dest), items(pixmap(false), dest)];
        let mut raster = Raster::default();
        let mut times = [Vec::new(), Vec::new()];
        for round in 0..=rounds {
            for (i, list) in lists.iter().enumerate() {
                for _ in 0..4 {
                    let start = std::time::Instant::now();
                    let mut t =
                        PaintTarget::new(&mut buf, Size::new(w, h), w * 4, Scale::ONE, 0).unwrap();
                    raster.paint(list, &full, &atlas, Scale::ONE, &mut t);
                    // The first round warms the contexts.
                    if round > 0 {
                        times[i].push(start.elapsed().as_secs_f64() * 1e3);
                    }
                }
            }
        }
        let median = |v: &mut Vec<f64>| {
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        let (a, b) = (median(&mut times[0]), median(&mut times[1]));
        println!(
            "opaque_pixmap_frame_cost: 3840x2160 {what}: claimed opaque {a:.2} ms, \
             maybe transparent {b:.2} ms (medians of {} frames each)",
            times[0].len()
        );
    }
    // A screen-filling gradient of opaque stops over the fill, drawn from
    // its dithered pixmap (`render_gradient_part`, the part each cell
    // shows): as this build claims it.
    let ramp = Paint::Linear {
        angle: 135.0,
        stops: vec![
            strand_scene::GradientStop {
                offset: 0.0,
                color: Color::from_hex("#1e66f5").unwrap(),
            },
            strand_scene::GradientStop {
                offset: 1.0,
                color: Color::from_hex("#ea76cb").unwrap(),
            },
        ],
    };
    let mut list = items(pixmap(true), screen);
    list[1] = DisplayItem {
        item: Item::Fill {
            shape: crate::flatten::FillShape::Rect(screen),
            paint: ramp,
            frame: screen,
        },
        bounds: Rect::new(0, 0, w, h),
    };
    let mut raster = Raster::default();
    let mut times = Vec::new();
    for round in 0..=rounds {
        for _ in 0..4 {
            let start = std::time::Instant::now();
            let mut t = PaintTarget::new(&mut buf, Size::new(w, h), w * 4, Scale::ONE, 0).unwrap();
            raster.paint(&list, &full, &atlas, Scale::ONE, &mut t);
            if round > 0 {
                times.push(start.elapsed().as_secs_f64() * 1e3);
            }
        }
    }
    times.sort_by(f64::total_cmp);
    println!(
        "opaque_pixmap_frame_cost: 3840x2160 gradient of opaque stops: {:.2} ms (median of {} frames)",
        times[times.len() / 2],
        times.len()
    );
}
