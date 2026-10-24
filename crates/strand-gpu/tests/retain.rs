//! (perf-gpu-retain) What a frame draws on the GPU that the CPU used to
//! draw and upload: a backdrop's input and pass (`Op::Backdrop`), a box
//! shadow (`Op::BlurredRect`) and a crossfade's snapshot
//! (`Op::Snapshot`, `Frame::snapshot`), pixel by pixel on whatever
//! Vulkan device the environment has (see `lifecycle.rs`).

mod common;

use std::sync::Arc;

use common::*;
use strand_gpu::kurbo::{Affine, Rect, Shape};
use strand_gpu::{
    AlphaColor, Brush, Frame, FrameSnapshot, GpuReply, GpuRequest, Op, PassFrame, PassGlobals,
    Pixmap, Readback,
};
use strand_scene::{Bundled, Scale, ShaderInput, ShaderPass, ShaderRef, Size};

const RED: [f32; 4] = [1.0, 0.0, 0.0, 1.0];
const BLUE: [f32; 4] = [0.0, 0.0, 1.0, 1.0];

/// Vertical stripes 4 px wide, red and blue, over `w × h`.
fn stripes(w: u32, h: u32) -> Vec<Op> {
    (0..w.div_ceil(4))
        .map(|i| Op::Fill {
            path: Rect::new(f64::from(i * 4), 0.0, f64::from(i * 4 + 4), f64::from(h)).to_path(0.1),
            brush: Brush::Solid(AlphaColor::new(if i % 2 == 0 { RED } else { BLUE })),
            brush_transform: Affine::IDENTITY,
            even_odd: false,
        })
        .collect()
}

/// The same stripes over `region` as the CPU keeps pixmaps (red and
/// blue swapped), the input a pass of its own reads.
fn stripes_pixmap(region: (u32, u32, u32, u32)) -> Pixmap {
    let (x0, y0, w, h) = region;
    let mut pm = Pixmap::new(w as u16, h as u16);
    let data = pm.data_mut();
    for y in 0..h {
        for x in 0..w {
            let p = &mut data[(y * w + x) as usize];
            let red = ((x0 + x) / 4).is_multiple_of(2);
            let _ = y0;
            (p.r, p.g, p.b, p.a) = if red {
                (0, 0, 255, 255)
            } else {
                (255, 0, 0, 255)
            };
        }
    }
    pm
}

fn blur(sigma: f32) -> ShaderPass {
    ShaderPass {
        code: ShaderRef::Bundled(Bundled::BackdropBlur),
        uniforms: Arc::from([sigma].as_slice()),
        input: ShaderInput::Content,
    }
}

fn frame_of(id: u64, size: Size, ops: Vec<Op>, snapshot: FrameSnapshot) -> Frame {
    Frame {
        surface: S,
        id,
        size,
        scale: Scale::ONE,
        ops,
        uploads: vec![],
        retire: Vec::new(),
        clear: AlphaColor::TRANSPARENT,
        snapshot,
    }
}

fn pass_pixels(h: &mut Harness, p: PassFrame) -> Readback {
    let key = p.key;
    h.gpu.send(GpuRequest::Pass(p));
    match h.next() {
        GpuReply::PassPixels { key: k, pixels, .. } => {
            assert_eq!(k, key);
            pixels
        }
        other => panic!("expected PassPixels, got {other:?}"),
    }
}

/// A backdrop drawn in the frame is its pass over what its ops draw: the
/// same pixels as the pass run on its own over the CPU's input (which
/// the frame then drew as an upload), at its region, under the frame's
/// clip; and drawn again the next frame from the texture it reuses.
#[test]
fn a_backdrop_in_a_frame_is_its_pass_over_what_is_behind_it() {
    let Some(mut h) = start() else { return };
    let size = Size::new(64, 32);
    attach(&mut h, size);
    let (x0, y0, w, hh) = (8u32, 4u32, 48u32, 24u32);
    let region = Rect::new(
        f64::from(x0),
        f64::from(y0),
        f64::from(x0 + w),
        f64::from(y0 + hh),
    );
    let globals = PassGlobals {
        time: 0.0,
        scale: 1.0,
        pointer: [-1.0, -1.0],
    };
    let alone = pass_pixels(
        &mut h,
        PassFrame {
            key: 1,
            id: 1,
            size: Size::new(w, hh),
            pass: blur(3.0),
            then: Vec::new(),
            globals,
            input: Some(Arc::new(stripes_pixmap((x0, y0, w, hh)))),
        },
    );
    let mut ops = stripes(64, 32);
    // A clip that leaves the backdrop's left column out.
    ops.push(Op::PushClip(Rect::new(10.0, 0.0, 64.0, 32.0).to_path(0.1)));
    ops.push(Op::Transform(Affine::translate((3.0, 0.0))));
    ops.push(Op::Transform(Affine::IDENTITY));
    ops.push(Op::Backdrop {
        region,
        ops: stripes(64, 32),
        pass: blur(3.0),
        then: Vec::new(),
        globals,
    });
    ops.push(Op::PopClip);
    for id in [2, 3] {
        let px = frame(&mut h, frame_of(id, size, ops.clone(), FrameSnapshot::None));
        let mut worst = 0u8;
        for y in y0..y0 + hh {
            for x in 10..x0 + w {
                let got = pixel(&px, x, y);
                let want = pixel(&alone, x - x0, y - y0);
                let d = got
                    .iter()
                    .zip(want)
                    .map(|(a, b)| a.abs_diff(b))
                    .max()
                    .unwrap_or(0);
                worst = worst.max(d);
            }
        }
        assert!(worst <= 1, "frame {id}: the backdrop lies {worst} off");
        // Left of the clip: the stripes as they are (red at x 8, 9).
        assert_eq!(pixel(&px, 9, 10), [0, 0, 255, 255], "frame {id}");
        // Outside the region: the stripes.
        assert_eq!(pixel(&px, 60, 2), [255, 0, 0, 255], "frame {id}");
        // Blurred inside: no pure stripe left in its middle.
        let mid = pixel(&px, 30, 16);
        assert!(mid[0] > 40 && mid[2] > 40, "frame {id}: {mid:?}");
    }
}

/// Passes inside a backdrop's ops draw into its input, and a pass
/// before it keeps its own number: each lands where its op is.
#[test]
fn passes_inside_a_backdrop_draw_into_what_it_reads() {
    let Some(mut h) = start() else { return };
    let size = Size::new(48, 16);
    attach(&mut h, size);
    let solid = |rgb: &str| ShaderPass {
        code: ShaderRef::File(code(
            &format!(
                "@fragment\nfn main(v: StrandVertex) -> @location(0) vec4<f32> {{\n    return vec4<f32>({rgb}, 1.0);\n}}\n"
            ),
            vec![],
        )),
        uniforms: Arc::from([].as_slice()),
        input: ShaderInput::None,
    };
    // The backdrop's own pass reads its input as it is.
    let copy = ShaderPass {
        code: ShaderRef::File(code(
            "@fragment\nfn main(v: StrandVertex) -> @location(0) vec4<f32> {\n    return textureSampleLevel(strand_input, strand_sampler, v.uv, 0.0);\n}\n",
            vec![],
        )),
        uniforms: Arc::from([].as_slice()),
        input: ShaderInput::Content,
    };
    let ops = vec![
        Op::Pass {
            pass: solid("1.0, 0.0, 0.0"),
            bounds: Rect::new(0.0, 0.0, 16.0, 16.0),
            globals: PassGlobals::default(),
        },
        Op::Backdrop {
            region: Rect::new(16.0, 0.0, 48.0, 16.0),
            ops: vec![
                Op::Pass {
                    pass: solid("0.0, 1.0, 0.0"),
                    bounds: Rect::new(16.0, 0.0, 32.0, 16.0),
                    globals: PassGlobals::default(),
                },
                Op::Fill {
                    path: Rect::new(32.0, 0.0, 48.0, 16.0).to_path(0.1),
                    brush: Brush::Solid(AlphaColor::new(BLUE)),
                    brush_transform: Affine::IDENTITY,
                    even_odd: false,
                },
            ],
            pass: copy,
            then: Vec::new(),
            globals: PassGlobals::default(),
        },
    ];
    assert_eq!(Op::passes_in(&ops), 3);
    let px = frame(&mut h, frame_of(2, size, ops, FrameSnapshot::None));
    assert_eq!(pixel(&px, 8, 8), [0, 0, 255, 255], "red, the first pass");
    assert_eq!(pixel(&px, 24, 8), [0, 255, 0, 255], "green, inside");
    assert_eq!(pixel(&px, 40, 8), [255, 0, 0, 255], "blue, its fill");
}

/// A box shadow is vello's blurred rounded rect in the frame: a soft
/// edge, darkest inside, nothing far out.
#[test]
fn a_blurred_rect_draws_a_soft_shadow() {
    let Some(mut h) = start() else { return };
    let size = Size::new(64, 64);
    attach(&mut h, size);
    let px = frame(
        &mut h,
        frame_of(
            2,
            size,
            vec![Op::BlurredRect {
                rect: Rect::new(16.0, 16.0, 48.0, 48.0),
                radius: 6.0,
                std_dev: 4.0,
                color: AlphaColor::new([0.0, 0.0, 0.0, 0.5]),
            }],
            FrameSnapshot::None,
        ),
    );
    let a = |x, y| pixel(&px, x, y)[3];
    assert!(a(32, 32).abs_diff(128) <= 1, "inside: {}", a(32, 32));
    assert!(a(16, 32) > 30 && a(16, 32) < 100, "edge: {}", a(16, 32));
    assert!(a(10, 32) < a(16, 32), "falls off");
    assert_eq!(a(1, 1), 0, "nothing far out");
    // Not finite: nothing drawn, the frame still answered.
    let px = frame(
        &mut h,
        frame_of(
            3,
            size,
            vec![Op::BlurredRect {
                rect: Rect::new(16.0, 16.0, f64::NAN, 48.0),
                radius: 6.0,
                std_dev: 4.0,
                color: AlphaColor::new([0.0, 0.0, 0.0, 0.5]),
            }],
            FrameSnapshot::None,
        ),
    );
    assert_eq!(pixel(&px, 32, 32), [0, 0, 0, 0]);
}

/// A crossfade's snapshot is the surface's last frame, taken on the GPU
/// thread before the frame draws, kept while frames keep it, and gone
/// with the first frame that holds none (or taken from no frame of the
/// size: nothing drawn).
#[test]
fn a_snapshot_is_the_last_frame_taken_kept_and_freed() {
    let Some(mut h) = start() else { return };
    let size = Size::new(32, 16);
    attach(&mut h, size);
    let fill = |c: [f32; 4]| Op::Fill {
        path: Rect::new(0.0, 0.0, 32.0, 16.0).to_path(0.1),
        brush: Brush::Solid(AlphaColor::new(c)),
        brush_transform: Affine::IDENTITY,
        even_odd: false,
    };
    let whole = Rect::new(0.0, 0.0, 32.0, 16.0);
    // Right half: the snapshot alone; left: the new frame over it.
    let fade = |c: [f32; 4]| {
        vec![
            Op::Snapshot { rect: whole },
            Op::PushClip(Rect::new(0.0, 0.0, 16.0, 16.0).to_path(0.1)),
            fill(c),
            Op::PopClip,
        ]
    };
    frame(
        &mut h,
        frame_of(1, size, vec![fill(RED)], FrameSnapshot::None),
    );
    let px = frame(&mut h, frame_of(2, size, fade(BLUE), FrameSnapshot::Take));
    assert_eq!(pixel(&px, 24, 8), [0, 0, 255, 255], "the red frame taken");
    assert_eq!(pixel(&px, 8, 8), [255, 0, 0, 255], "the new frame");
    // Kept: still the red frame, not the frame before this one.
    let green = [0.0, 1.0, 0.0, 1.0];
    let px = frame(&mut h, frame_of(3, size, fade(green), FrameSnapshot::Keep));
    assert_eq!(pixel(&px, 24, 8), [0, 0, 255, 255], "kept");
    assert_eq!(pixel(&px, 8, 8), [0, 255, 0, 255]);
    // Freed: a later keep has nothing to draw.
    frame(
        &mut h,
        frame_of(4, size, vec![fill(BLUE)], FrameSnapshot::None),
    );
    let px = frame(&mut h, frame_of(5, size, fade(green), FrameSnapshot::Keep));
    assert_eq!(pixel(&px, 24, 8), [0, 0, 0, 0], "none held");
    // Taken at another size than the frame held: none.
    let big = Size::new(48, 16);
    h.gpu.send(GpuRequest::Resize {
        surface: S,
        size: big,
        scale: Scale::ONE,
    });
    let px = frame(&mut h, frame_of(6, big, fade(green), FrameSnapshot::Take));
    assert_eq!(pixel(&px, 24, 8), [0, 0, 0, 0], "no frame of its size");
}
