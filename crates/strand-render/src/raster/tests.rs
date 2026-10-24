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

/// (integration-r8) A gradient drawn smoothly (held-out set I's album
/// art, a rounded box morphing at a scale of 1.02 over its panel's
/// fill) repaints, from a raster whose earlier frames cached its pixmap,
/// in full and within any damage, as a fresh raster with nothing cached
/// draws it. Before, the cached frame sampled the whole pixmap and the
/// fresh one each cell's part of it, the same pixels placed from another
/// origin, and 2 pixels rounded a level apart.
#[test]
fn a_scaled_gradient_repaints_from_its_cache_as_fresh() {
    use strand_scene::{Color, GradientStop, Paint, PaintTarget, Size};
    use vello_cpu::kurbo::{self, Shape};
    let rr = |r: kurbo::Rect, rad: f64| kurbo::RoundedRect::from_rect(r, rad).into_path(0.1);
    let rgb = |r: f32, g: f32, b: f32| Color { r, g, b, a: 1.0 };
    let panel = kurbo::Rect::new(73.0, 8.0, 293.0, 277.0);
    let art = kurbo::Rect::new(89.0, 23.0, 285.0, 219.0);
    let s = 1.020_362_854_003_906_3;
    let morph = kurbo::Affine::new([
        s,
        0.0,
        0.0,
        s,
        -3.807_853_698_730_468_8,
        -2.463_905_334_472_656_3,
    ]);
    let ramp = Paint::Linear {
        angle: 45.0,
        stops: vec![
            GradientStop {
                offset: 0.0,
                color: rgb(0.576_470_6, 0.121_568_63, 0.776_470_6),
            },
            GradientStop {
                offset: 1.0,
                color: rgb(0.156_862_75, 0.325_490_2, 0.819_607_85),
            },
        ],
    };
    let items = vec![
        DisplayItem {
            item: Item::Fill {
                shape: crate::flatten::FillShape::Path(rr(panel, 16.0)),
                paint: Paint::Solid(rgb(0.980_392_16, 0.976_470_6, 1.0)),
                frame: panel,
            },
            bounds: Rect::new(73, 8, 220, 269),
        },
        DisplayItem {
            item: Item::PushTransform(morph),
            bounds: Rect::new(86, 20, 202, 202),
        },
        DisplayItem {
            item: Item::Fill {
                shape: crate::flatten::FillShape::Path(rr(art, 12.0)),
                paint: ramp,
                frame: art,
            },
            bounds: Rect::new(86, 20, 202, 202),
        },
        DisplayItem {
            item: Item::PopTransform,
            bounds: Rect::new(0, 0, 366, 366),
        },
    ];
    let (w, h) = (366u32, 366u32);
    let atlas = AtlasMirror::default();
    let paint = |r: &mut Raster, damage: &Damage| {
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let mut t = PaintTarget::new(&mut buf, Size::new(w, h), w * 4, Scale::ONE, 0).unwrap();
        r.paint(&items, damage, &atlas, Scale::ONE, &mut t);
        buf
    };
    let full = Damage::full(Size::new(w, h));
    let fresh = paint(&mut Raster::default(), &full);
    let mut damages = vec![full];
    let mut d = Damage::new();
    d.add(Rect::new(86, 20, 204, 204));
    damages.push(d);
    // Damage starting at every few pixels across the box, so a part of
    // the gradient would start elsewhere in each if it followed the
    // damage instead of the cell.
    for i in 0..48 {
        let mut d = Damage::new();
        d.add(Rect::new(
            86 + i * 5,
            20 + i * 4,
            120 + i as u32,
            90 + 2 * i as u32,
        ));
        damages.push(d);
    }
    for damage in &damages {
        // Two whole frames first: the second admits the gradient's
        // pixmap to the cache (`PaintCache::gradient_in`).
        let mut warm = Raster::default();
        paint(&mut warm, &full);
        paint(&mut warm, &full);
        let part = paint(&mut warm, damage);
        let bad: Vec<(usize, usize)> = (0..(w * h) as usize)
            .map(|i| (i % w as usize, i / w as usize))
            .filter(|&(x, y)| {
                damage
                    .rects()
                    .iter()
                    .any(|r| r.contains(strand_scene::Point::new(x as i32, y as i32)))
            })
            .filter(|&(x, y)| {
                let i = (y * w as usize + x) * 4;
                part[i..i + 4] != fresh[i..i + 4]
            })
            .collect();
        assert!(
            bad.is_empty(),
            "{damage:?}: {} pixels differ: {:?}",
            bad.len(),
            &bad[..bad.len().min(6)]
        );
    }
}

/// (perf-cpu-raster) A scene of most of what a cell draws: a backdrop
/// gradient, rounded cards with shadows, gradients drawn pixel for
/// pixel and turned or scaled (sampled smoothly), an opacity group and
/// a group masked by `shape(cookie)` and turned, over `w × h`.
fn busy_scene(w: u32, h: u32, turn: f64) -> Vec<DisplayItem> {
    use std::sync::Arc;
    use strand_scene::{Color, Effect, GradientStop, Mask, Paint};
    use vello_cpu::kurbo::{self, Shape};
    let hex = |s: &str| Color::from_hex(s).unwrap();
    let ramp = |a: &str, b: &str, angle: f32| Paint::Linear {
        angle,
        stops: vec![
            GradientStop {
                offset: 0.0,
                color: hex(a),
            },
            GradientStop {
                offset: 1.0,
                color: hex(b),
            },
        ],
    };
    let all = Rect::new(0, 0, w, h);
    let screen = kurbo::Rect::new(0.0, 0.0, w as f64, h as f64);
    let bounds = |r: kurbo::Rect| {
        let r = r.inflate(40.0, 40.0).intersect(screen);
        Rect::from_edges(
            r.x0.floor() as i64,
            r.y0.floor() as i64,
            r.x1.ceil() as i64,
            r.y1.ceil() as i64,
        )
    };
    let mut items = vec![DisplayItem {
        item: Item::Fill {
            shape: crate::flatten::FillShape::Rect(screen),
            paint: ramp("#1e1e2e", "#45475a", 120.0),
            frame: screen,
        },
        bounds: all,
    }];
    let rr = |r: kurbo::Rect, rad: f64| kurbo::RoundedRect::from_rect(r, rad).into_path(0.1);
    for i in 0..12 {
        let (col, row) = ((i % 4) as f64, (i / 4) as f64);
        let card = kurbo::Rect::new(
            30.0 + col * (w as f64 - 60.0) / 4.0,
            25.0 + row * (h as f64 - 50.0) / 3.0,
            30.0 + col * (w as f64 - 60.0) / 4.0 + (w as f64 - 60.0) / 4.0 - 24.0,
            25.0 + row * (h as f64 - 50.0) / 3.0 + (h as f64 - 50.0) / 3.0 - 30.0,
        );
        let b = bounds(card);
        let shadow_extent = card.inflate(30.0, 30.0);
        let mut clip = shadow_extent.to_path(0.1);
        clip.extend(rr(card, 14.0));
        items.push(DisplayItem {
            item: Item::Shadow {
                rect: card + kurbo::Vec2::new(0.0, 6.0),
                radii: [14.0; 4],
                std_dev: 8.0,
                color: hex("#00000080"),
                clip,
                extent: shadow_extent,
            },
            bounds: b,
        });
        let xform = match i % 3 {
            0 => None,
            1 => Some(kurbo::Affine::rotate_about(
                turn * (i as f64),
                card.center(),
            )),
            _ => Some(kurbo::Affine::scale_about(0.85 + 0.1 * turn, card.center())),
        };
        if let Some(t) = xform {
            items.push(DisplayItem {
                item: Item::PushTransform(t),
                bounds: b,
            });
        }
        if i == 5 {
            items.push(DisplayItem {
                item: Item::PushOpacity(0.6, None),
                bounds: b,
            });
        }
        items.push(DisplayItem {
            item: Item::Fill {
                shape: crate::flatten::FillShape::Path(rr(card, 14.0)),
                paint: ramp("#89b4fa", "#cba6f7", 135.0 + 20.0 * i as f32),
                frame: card,
            },
            bounds: b,
        });
        if i == 7 {
            let cover = kurbo::Rect::from_center_size(card.center(), (48.0, 48.0));
            let layer = crate::layers::Layer {
                effects: Arc::from(vec![Effect::Mask(Mask::Shape("cookie".into()))]),
                frame: cover,
                scale: 1.0,
                xform: kurbo::Affine::IDENTITY,
                mosaic: None,
                gpu: None,
            };
            items.push(DisplayItem {
                item: Item::PushTransform(kurbo::Affine::rotate_about(turn * 3.0, cover.center())),
                bounds: bounds(cover),
            });
            items.push(DisplayItem {
                item: Item::PushLayer(Arc::new(layer)),
                bounds: bounds(cover),
            });
            items.push(DisplayItem {
                item: Item::Fill {
                    shape: crate::flatten::FillShape::Rect(cover),
                    paint: ramp("#f38ba8", "#fab387", 45.0),
                    frame: cover,
                },
                bounds: bounds(cover),
            });
            items.push(DisplayItem {
                item: Item::PopLayer,
                bounds: bounds(cover),
            });
            items.push(DisplayItem {
                item: Item::PopTransform,
                bounds: bounds(cover),
            });
        }
        if i == 5 {
            items.push(DisplayItem {
                item: Item::PopOpacity,
                bounds: b,
            });
        }
        if xform.is_some() {
            items.push(DisplayItem {
                item: Item::PopTransform,
                bounds: b,
            });
        }
    }
    items
}

fn paint_with(
    r: &mut Raster,
    items: &[DisplayItem],
    damage: &Damage,
    (w, h): (u32, u32),
    buf: &mut [u8],
) {
    use strand_scene::{PaintTarget, Size};
    let atlas = AtlasMirror::default();
    let mut t = PaintTarget::new(buf, Size::new(w, h), w * 4, Scale::ONE, 0).unwrap();
    r.paint(items, damage, &atlas, Scale::ONE, &mut t);
}

/// (perf-cpu-raster) A paint's cells drawn on several threads are the
/// bytes the render thread alone draws, over frames of a turning,
/// scaling scene, in full and within scattered damage (rows of cells
/// shared by several threads, cells cut by the damage), from a fresh
/// raster and from one whose earlier frames cached its paint; and only a
/// paint of many cells starts threads.
#[test]
fn cells_drawn_on_threads_are_the_render_threads_bytes() {
    // Counts its paints' threads: other tests' paints take none of them.
    unbudgeted();
    use strand_scene::Size;
    let (w, h) = (1100u32, 700u32);
    let full = Damage::full(Size::new(w, h));
    let mut serial = Raster::default();
    serial.set_max_threads(1);
    let mut threaded = Raster::default();
    threaded.set_max_threads(4);
    let mut a = vec![0u8; (w * h * 4) as usize];
    let mut b = a.clone();
    let mut seed = 0x51f1_5eedu64;
    let mut rnd = move |n: u32| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n as u64) as u32
    };
    for frame in 0..8 {
        let items = busy_scene(w, h, frame as f64 * 0.07);
        let damage = if frame % 2 == 0 {
            full
        } else {
            let mut d = Damage::new();
            for _ in 0..6 {
                d.add(Rect::new(
                    rnd(w - 200) as i32,
                    rnd(h - 150) as i32,
                    60 + rnd(500),
                    20 + rnd(300),
                ));
            }
            d
        };
        paint_with(&mut serial, &items, &damage, (w, h), &mut a);
        paint_with(&mut threaded, &items, &damage, (w, h), &mut b);
        assert_eq!(serial.times().threads, 1);
        let cells = threaded.times().cells as usize;
        if cells >= PARALLEL_CELLS {
            assert_eq!(
                threaded.times().threads,
                4.min(cells) as u32,
                "frame {frame}"
            );
        }
        let differ = a.chunks(4).zip(b.chunks(4)).filter(|(p, q)| p != q).count();
        assert_eq!(differ, 0, "frame {frame}: {differ} pixels differ");
        // And as a fresh raster on its own threads draws the frame.
        let mut fresh = Raster::default();
        fresh.set_max_threads(3);
        let mut c = vec![0u8; (w * h * 4) as usize];
        paint_with(&mut fresh, &items, &full, (w, h), &mut c);
        let mut s = vec![0u8; (w * h * 4) as usize];
        let mut alone = Raster::default();
        alone.set_max_threads(1);
        paint_with(&mut alone, &items, &full, (w, h), &mut s);
        assert!(c == s, "frame {frame}: a fresh threaded frame differs");
    }
    assert_eq!(threaded.lanes(), 4);
    // A small damage stays on the render thread.
    let mut d = Damage::new();
    d.add(Rect::new(10, 10, 300, 60));
    paint_with(&mut threaded, &busy_scene(w, h, 0.0), &d, (w, h), &mut b);
    assert!(threaded.times().cells < PARALLEL_CELLS as u32);
    assert_eq!(threaded.times().threads, 1);
}

/// (perf-cpu-raster) A window of the surface (the drag icon) drawn on
/// threads is that part of the full paint, its rows cut from the
/// window's own origin.
#[test]
fn a_window_drawn_on_threads_is_the_full_paints_part() {
    // Counts its paints' threads: other tests' paints take none of them.
    unbudgeted();
    use strand_scene::{PaintTarget, Size};
    let (w, h) = (1100u32, 700u32);
    let items = busy_scene(w, h, 0.3);
    let full = Damage::full(Size::new(w, h));
    let mut buf = vec![0u8; (w * h * 4) as usize];
    let mut alone = Raster::default();
    alone.set_max_threads(1);
    paint_with(&mut alone, &items, &full, (w, h), &mut buf);
    let atlas = AtlasMirror::default();
    // A window starting off the cells' grid, its stride wider than it.
    let (ox, oy, ww, wh, stride) = (97i64, 41i64, 700u32, 500u32, 700 * 4 + 64);
    let mut win = vec![0u8; stride as usize * wh as usize];
    let mut r = Raster::default();
    r.set_max_threads(4);
    let mut t = PaintTarget::new(&mut win, Size::new(ww, wh), stride, Scale::ONE, 0).unwrap();
    r.paint_window(
        &items,
        &full,
        &atlas,
        Scale::ONE,
        &mut t,
        Rect::new(0, 0, w, h),
        (ox, oy),
    );
    assert!(r.times().threads > 1);
    for y in 0..wh as usize {
        let src = ((y + oy as usize) * w as usize + ox as usize) * 4;
        let dst = y * stride as usize;
        assert!(
            win[dst..dst + ww as usize * 4] == buf[src..src + ww as usize * 4],
            "row {y} differs"
        );
    }
}

/// (perf-cpu-raster) However many renderers paint at once, at most
/// `MAX_THREADS` − 1 helper threads run in the process: eight threads
/// sharing work together never have more helpers working than that,
/// and every task still runs once.
#[test]
fn renderers_painting_together_share_the_helper_threads() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let working = AtomicUsize::new(0);
    let most = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                for _ in 0..6 {
                    let mut states = vec![(); MAX_THREADS];
                    let ran = share(&mut states, 24, |_, _| {
                        let helper = std::thread::current().name() == Some("strand-raster");
                        if helper {
                            let now = working.fetch_add(1, Ordering::SeqCst) + 1;
                            most.fetch_max(now, Ordering::SeqCst);
                        }
                        std::thread::sleep(std::time::Duration::from_micros(300));
                        if helper {
                            working.fetch_sub(1, Ordering::SeqCst);
                        }
                        done.fetch_add(1, Ordering::SeqCst);
                    });
                    assert!((1..=MAX_THREADS).contains(&ran), "{ran} threads ran");
                }
            });
        }
    });
    assert_eq!(
        done.load(Ordering::SeqCst),
        8 * 6 * 24,
        "every task ran once"
    );
    // (Whether any started depends on the other tests' paints, which
    // may hold every place; the tests that count threads start theirs.)
    let most = most.load(Ordering::SeqCst);
    assert!(most < MAX_THREADS, "{most} helpers at once");
}

/// (perf-cpu-raster) The raster's threads live only while a paint
/// draws: once it returns, none it started is left to run or wake (idle
/// is zero work).
#[test]
fn no_raster_thread_outlives_its_paint() {
    // Counts its paints' threads: other tests' paints take none of them.
    unbudgeted();
    use strand_scene::Size;
    let (w, h) = (1100u32, 700u32);
    let mut r = Raster::default();
    r.set_max_threads(4);
    let mut buf = vec![0u8; (w * h * 4) as usize];
    // SAFETY: gettid has no preconditions.
    let me = unsafe { libc::gettid() } as i64;
    for f in 0..3 {
        let items = busy_scene(w, h, f as f64 * 0.1);
        paint_with(
            &mut r,
            &items,
            &Damage::full(Size::new(w, h)),
            (w, h),
            &mut buf,
        );
        let tids = r.worker_tids();
        assert_eq!(tids.len(), 3, "frame {f}: three threads beside this one");
        for tid in tids {
            assert_ne!(tid, me);
            // Joined: the kernel clears a thread's id (what the join
            // waits for) a moment before it removes the task, so a
            // joined thread may still be listed for a moment, never for
            // long.
            let task = format!("/proc/self/task/{tid}");
            let gone = (0..200).any(|_| {
                let gone = !std::path::Path::new(&task).exists();
                if !gone {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                gone
            });
            assert!(gone, "frame {f}: thread {tid} outlived its paint");
        }
    }
}

/// (perf-cpu-raster review) The other threads' lanes are kept while
/// large paints come within `LANE_IDLE` of each other, freed at the
/// first trim after it (a paint's or a wake's), and built again by the
/// next large paint, which draws the same bytes.
#[test]
fn helper_lanes_are_freed_when_idle() {
    // Counts its paints' threads: other tests' paints take none of them.
    unbudgeted();
    use std::time::{Duration, Instant};
    use strand_scene::Size;
    let (w, h) = (1100u32, 700u32);
    let full = Damage::full(Size::new(w, h));
    let items = busy_scene(w, h, 0.2);
    let mut r = Raster::default();
    r.set_max_threads(4);
    let mut a = vec![0u8; (w * h * 4) as usize];
    paint_with(&mut r, &items, &full, (w, h), &mut a);
    assert_eq!(r.lanes(), 4);
    let painted = Instant::now();
    // Within LANE_IDLE: kept.
    r.trim_idle(painted + LANE_IDLE / 2);
    assert_eq!(r.lanes(), 4);
    // Past it: freed, the render thread's own kept.
    r.trim_idle(painted + LANE_IDLE + Duration::from_millis(1));
    assert_eq!(r.lanes(), 1);
    assert!(r.contexts() > 0, "the render thread's contexts were freed");
    // A small paint keeps none; a large one builds them again.
    let mut d = Damage::new();
    d.add(Rect::new(10, 10, 300, 60));
    let mut b = a.clone();
    paint_with(&mut r, &items, &d, (w, h), &mut b);
    assert_eq!(r.lanes(), 1);
    paint_with(&mut r, &items, &full, (w, h), &mut b);
    assert_eq!(r.lanes(), 4);
    assert!(a == b, "the frame after the lanes were freed differs");
    // A test's shorter idle time frees them sooner, at a paint's trim.
    r.set_idle_free(Duration::from_millis(1));
    std::thread::sleep(Duration::from_millis(5));
    paint_with(&mut r, &items, &d, (w, h), &mut b);
    assert_eq!(r.lanes(), 1);
}

/// (perf-cpu-raster) A gradient scaling in (a staggered tile's `enter
/// { scale: 0.85 }`) or turning, drawn smoothly from the part each cell
/// shows: once its whole pixmap is cached the parts are copied from it,
/// not dithered again, and every frame is the bytes a fresh raster, with
/// nothing cached, dithers.
#[test]
fn a_transformed_gradient_draws_its_parts_from_the_cache_as_fresh() {
    use crate::cache::PARTS_DITHERED;
    use strand_scene::Size;
    let (w, h) = (600u32, 300u32);
    let full = Damage::full(Size::new(w, h));
    let mut warm = Raster::default();
    warm.set_max_threads(1);
    let mut a = vec![0u8; (w * h * 4) as usize];
    let mut b = a.clone();
    let mut dithered_warm = Vec::new();
    for frame in 0..10 {
        let items = busy_scene(w, h, 0.02 + frame as f64 * 0.05);
        let before = PARTS_DITHERED.with(|n| n.get());
        paint_with(&mut warm, &items, &full, (w, h), &mut a);
        dithered_warm.push(PARTS_DITHERED.with(|n| n.get()) - before);
        let mut fresh = Raster::default();
        fresh.set_max_threads(1);
        paint_with(&mut fresh, &items, &full, (w, h), &mut b);
        let differ = a.chunks(4).zip(b.chunks(4)).filter(|(p, q)| p != q).count();
        assert_eq!(differ, 0, "frame {frame}: {differ} pixels differ");
    }
    // The first frame dithers every part; from the third (the second
    // admits the pixmaps) none.
    assert!(dithered_warm[0] > 0, "{dithered_warm:?}");
    assert!(
        dithered_warm[2..].iter().all(|&n| n == 0),
        "{dithered_warm:?}"
    );
}
