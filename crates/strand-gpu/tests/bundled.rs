//! design.md's bundled GPU effects as passes on the GPU thread, each on
//! a small input with a property its look must have (the pixel
//! references are render's, `strand-render/tests/gpu.rs`).

mod common;

use std::sync::Arc;

use common::*;
use strand_gpu::{GpuReply, GpuRequest, PassFrame, PassGlobals, Pixmap, Readback};
use strand_scene::{Bundled, ShaderInput, ShaderPass, ShaderRef, Size};

/// A `w × h` input whose pixels `f` gives (premultiplied RGBA, stored in
/// the CPU raster's swapped order as render sends them).
fn input(w: u16, h: u16, f: impl Fn(u32, u32) -> [u8; 4]) -> Arc<Pixmap> {
    let mut pm = Pixmap::new(w, h);
    for (i, p) in pm.data_mut().iter_mut().enumerate() {
        let [r, g, b, a] = f(i as u32 % w as u32, i as u32 / w as u32);
        p.r = b;
        p.g = g;
        p.b = r;
        p.a = a;
    }
    Arc::new(pm)
}

/// Runs bundled pass `b` with `uniforms` over `size`, reading `input`.
fn run(
    h: &mut Harness,
    b: Bundled,
    uniforms: &[f32],
    size: Size,
    input: Option<Arc<Pixmap>>,
    globals: PassGlobals,
) -> Readback {
    h.gpu.send(GpuRequest::Pass(PassFrame {
        key: 9,
        id: 1,
        size,
        pass: ShaderPass {
            code: ShaderRef::Bundled(b),
            uniforms: uniforms.into(),
            input: ShaderInput::Content,
        },
        then: Vec::new(),
        globals,
        input,
    }));
    match h.next() {
        GpuReply::PassPixels { key, pixels, .. } => {
            assert_eq!(key, 9);
            assert_eq!((pixels.width, pixels.height), (size.w, size.h));
            pixels
        }
        other => panic!("{b:?}: expected PassPixels, got {other:?}"),
    }
}

/// A pixel as straight RGBA (readback bytes are BGRA).
fn rgba(rb: &Readback, x: u32, y: u32) -> [u8; 4] {
    let [b, g, r, a] = pixel(rb, x, y);
    [r, g, b, a]
}

fn globals() -> PassGlobals {
    PassGlobals {
        time: 0.0,
        scale: 1.0,
        pointer: [-1.0, -1.0],
    }
}

#[test]
fn bloom_bleeds_bright_parts_and_keeps_dark_ones() {
    let Some(mut h) = start() else { return };
    // A white square and a dark grey one, 8 px each, on transparent.
    let px = input(64, 32, |x, y| {
        if (8..16).contains(&x) && (12..20).contains(&y) {
            [255, 255, 255, 255]
        } else if (40..48).contains(&x) && (12..20).contains(&y) {
            [40, 40, 40, 255]
        } else {
            [0; 4]
        }
    });
    let out = run(
        &mut h,
        Bundled::Bloom,
        &[8.0, 1.0],
        Size::new(64, 32),
        Some(px),
        globals(),
    );
    assert_eq!(rgba(&out, 12, 16), [255, 255, 255, 255], "the square stays");
    let near = rgba(&out, 18, 16)[3];
    let far = rgba(&out, 22, 16)[3];
    assert!(near > far && far > 0, "a halo fading out: {near} {far}");
    assert_eq!(rgba(&out, 30, 16), [0; 4], "nothing past the radius");
    assert_eq!(rgba(&out, 50, 16)[3], 0, "a dark part does not bloom");
}

#[test]
fn chromatic_splits_red_and_blue_apart() {
    let Some(mut h) = start() else { return };
    let px = input(32, 8, |x, _| if x == 16 { [255; 4] } else { [0; 4] });
    let out = run(
        &mut h,
        Bundled::Chromatic,
        &[3.0],
        Size::new(32, 8),
        Some(px),
        globals(),
    );
    assert_eq!(rgba(&out, 16, 4), [0, 255, 0, 255], "green stays");
    assert_eq!(rgba(&out, 19, 4), [255, 0, 0, 255], "red moves right");
    assert_eq!(rgba(&out, 13, 4), [0, 0, 255, 255], "blue moves left");
    assert_eq!(rgba(&out, 8, 4), [0; 4]);
}

#[test]
fn crt_draws_scanlines_and_a_curved_screen() {
    let Some(mut h) = start() else { return };
    let px = input(60, 60, |_, _| [200, 200, 200, 255]);
    let out = run(
        &mut h,
        Bundled::Crt,
        &[],
        Size::new(60, 60),
        Some(px),
        globals(),
    );
    // Scanlines: one row in three darker, down the middle column.
    let col: Vec<u8> = (24..36).map(|y| rgba(&out, 30, y)[1]).collect();
    let (lo, hi) = (col.iter().min().copied(), col.iter().max().copied());
    assert!(hi.unwrap_or(0) - lo.unwrap_or(0) > 30, "scanlines: {col:?}");
    assert_eq!(rgba(&out, 30, 30)[3], 255, "the middle is covered");
    assert_eq!(rgba(&out, 0, 0)[3], 0, "the corners are cut round");
}

#[test]
fn wobble_moves_rows_with_time() {
    let Some(mut h) = start() else { return };
    let px = input(40, 40, |x, _| if x == 20 { [255; 4] } else { [0; 4] });
    let at = |h: &mut Harness, t: f32| {
        run(
            h,
            Bundled::Wobble,
            &[4.0, 40.0, 2.0],
            Size::new(40, 40),
            Some(px.clone()),
            PassGlobals {
                time: t,
                ..globals()
            },
        )
    };
    let a = at(&mut h, 0.0);
    let b = at(&mut h, 0.5);
    // The line's column on row 10, by its brightest pixel.
    let x_of = |rb: &Readback, y: u32| (0..40).max_by_key(|x| rgba(rb, *x, y)[1]).unwrap_or(0);
    assert_ne!(x_of(&a, 10), 20, "a row is moved");
    assert_ne!(x_of(&a, 10), x_of(&b, 10), "and moves with time");
    assert!(x_of(&a, 10).abs_diff(20) <= 4, "by at most the amplitude");
}

#[test]
fn tilt_turns_the_input_in_perspective() {
    let Some(mut h) = start() else { return };
    let px = input(80, 60, |_, _| [255, 255, 255, 255]);
    let flat = run(
        &mut h,
        Bundled::Tilt,
        &[0.0, 0.0],
        Size::new(80, 60),
        Some(px.clone()),
        globals(),
    );
    assert_eq!(rgba(&flat, 1, 1)[3], 255, "no turn: the whole box");
    assert_eq!(rgba(&flat, 78, 58)[3], 255);
    let turned = run(
        &mut h,
        Bundled::Tilt,
        &[0.0, 0.3],
        Size::new(80, 60),
        Some(px),
        globals(),
    );
    // Turned about its vertical axis by 0.3 rad (right side away): the
    // near side spans the box's height, the far side is shorter, and the
    // card fits the box (it is scaled to).
    assert_eq!(rgba(&turned, 40, 30)[3], 255, "the middle is covered");
    assert!(rgba(&turned, 2, 1)[3] > 200, "the near side's corner");
    assert_eq!(rgba(&turned, 76, 2)[3], 0, "the far side's corner is clear");
    assert_eq!(rgba(&turned, 78, 30)[3], 0, "the far side is narrower");
}

#[test]
fn glass_refracts_the_backdrop_and_lights_its_rim_and_the_pointer() {
    let Some(mut h) = start() else { return };
    // Vertical stripes, 4 px apart, on mid grey.
    let px = input(80, 60, |x, _| {
        if x % 8 < 4 {
            [60, 60, 60, 255]
        } else {
            [120, 120, 120, 255]
        }
    });
    let u = [6.0, 12.0, 0.3, 1.0];
    let out = run(
        &mut h,
        Bundled::Glass,
        &u,
        Size::new(80, 60),
        Some(px.clone()),
        globals(),
    );
    let mid = rgba(&out, 40, 30);
    assert_eq!(mid[3], 255, "opaque inside");
    // Deep inside, the backdrop shows through, lightly frosted and
    // tinted: still between its two greys, and stripes still visible.
    let row: Vec<u8> = (36..44).map(|x| rgba(&out, x, 30)[1]).collect();
    assert!(row.iter().all(|v| (55..=135).contains(v)), "{row:?}");
    assert!(row.iter().max() > row.iter().min(), "stripes show: {row:?}");
    // The rim is lit: brighter than the middle.
    assert!(rgba(&out, 40, 1)[1] > mid[1] + 10, "fresnel rim");
    // Dispersion splits colour near the rim.
    let rim = rgba(&out, 3, 30);
    assert_ne!(rim[0], rim[2], "red and blue bent apart: {rim:?}");
    assert_eq!(rgba(&out, 0, 0)[3], 0, "outside the rounded corner");
    // The pointer lights the glass under it.
    let lit = run(
        &mut h,
        Bundled::Glass,
        &u,
        Size::new(80, 60),
        Some(px),
        PassGlobals {
            pointer: [40.0, 30.0],
            ..globals()
        },
    );
    assert!(rgba(&lit, 40, 30)[1] > mid[1] + 20, "pointer specular");
}

#[test]
fn backdrop_blur_smooths_at_full_resolution() {
    let Some(mut h) = start() else { return };
    let px = input(64, 64, |x, y| {
        if (x / 2 + y / 2) % 2 == 0 {
            [255, 255, 255, 255]
        } else {
            [0, 0, 0, 255]
        }
    });
    let out = run(
        &mut h,
        Bundled::BackdropBlur,
        &[6.0],
        Size::new(64, 64),
        Some(px),
        globals(),
    );
    for (x, y) in [(10, 10), (31, 33), (50, 20)] {
        let v = rgba(&out, x, y);
        assert!(
            (118..=138).contains(&v[1]) && v[3] == 255,
            "{v:?} at {x},{y}"
        );
    }
}

#[test]
fn aurora_draws_curtains_that_move() {
    let Some(mut h) = start() else { return };
    let green = [0.2, 0.9, 0.5, 1.0];
    let mut u = vec![];
    u.extend(green);
    u.extend([0.1, 0.6, 0.9, 1.0]);
    u.extend([0.6, 0.3, 0.9, 1.0]);
    u.extend([1.0, 0.0, 0.0, 0.0]);
    let a = run(
        &mut h,
        Bundled::Aurora,
        &u,
        Size::new(64, 48),
        None,
        globals(),
    );
    let b = run(
        &mut h,
        Bundled::Aurora,
        &u,
        Size::new(64, 48),
        None,
        PassGlobals {
            time: 3.0,
            ..globals()
        },
    );
    assert!(
        (0..48).any(|y| rgba(&a, 10, y)[3] > 60),
        "curtains are drawn"
    );
    assert_ne!(a.bytes, b.bytes, "and move");
}

#[test]
fn particles_are_drawn_as_sprites_where_they_are() {
    let Some(mut h) = start() else { return };
    // Radius 2, no glow, half size 3, white; three particles.
    let mut u = vec![2.0, 0.0, 3.0, 0.0, 1.0, 1.0, 1.0, 1.0];
    u.extend([10.0, 10.0, 1.0, 30.2, 20.0, 0.5, 50.0, 5.0, 0.0]);
    let out = run(
        &mut h,
        Bundled::Particles,
        &u,
        Size::new(64, 32),
        None,
        globals(),
    );
    assert_eq!(rgba(&out, 10, 10), [255, 255, 255, 255]);
    let half = rgba(&out, 30, 20)[3];
    assert!(
        (126..=129).contains(&half),
        "at its alpha, rounded centre: {half}"
    );
    assert_eq!(rgba(&out, 50, 5)[3], 0, "an invisible one");
    assert_eq!(rgba(&out, 20, 10)[3], 0);
    // Thousands of particles in one pass.
    let mut many = vec![1.0, 0.0, 2.0, 0.0, 1.0, 1.0, 1.0, 1.0];
    for i in 0..5000 {
        many.extend([(i % 64) as f32, (i / 64 % 32) as f32, 1.0]);
    }
    let out = run(
        &mut h,
        Bundled::Particles,
        &many,
        Size::new(64, 32),
        None,
        globals(),
    );
    assert!((0..32).all(|y| (0..64).all(|x| rgba(&out, x, y)[3] == 255)));
}
