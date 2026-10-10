//! GPU frames and shader passes, pixel by pixel, on whatever Vulkan
//! device the environment has (see `lifecycle.rs`).

mod common;

use std::sync::Arc;
use std::sync::mpsc;

use common::*;
use strand_gpu::kurbo::{Affine, BezPath, Rect, Shape};
use strand_gpu::{
    AlphaColor, Brush, Frame, Gpu, GpuErrorKind, GpuOptions, GpuReply, GpuRequest, Op, PassFrame,
    PassGlobals, Pixmap, Upload,
};
use strand_scene::{Scale, ShaderInput, ShaderPass, ShaderRef, Size, UniformType};

#[test]
fn frames_fill_clip_layer_and_draw_uploads() {
    let Some(mut h) = start() else { return };
    let size = Size::new(64, 32);
    attach(&mut h, size);
    let red = AlphaColor::new([1.0, 0.0, 0.0, 1.0]);
    let green = AlphaColor::new([0.0, 1.0, 0.0, 1.0]);
    // A 4×4 red upload, stored as the CPU raster keeps pixmaps (red and
    // blue swapped): swapped back on upload.
    let mut pm = Pixmap::new(4, 4);
    for p in pm.data_mut() {
        p.r = 0;
        p.g = 0;
        p.b = 255;
        p.a = 255;
    }
    let ops = vec![
        // Left half red.
        Op::Fill {
            path: Rect::new(0.0, 0.0, 32.0, 32.0).to_path(0.1),
            brush: Brush::Solid(red),
            brush_transform: Affine::IDENTITY,
            even_odd: false,
        },
        // A clip to the right half, a half-opacity layer: green at 50%.
        Op::PushClip(Rect::new(32.0, 0.0, 64.0, 32.0).to_path(0.1)),
        Op::PushLayer(strand_gpu::Layer {
            opacity: Some(0.5),
            ..Default::default()
        }),
        Op::Fill {
            path: Rect::new(0.0, 0.0, 64.0, 16.0).to_path(0.1),
            brush: Brush::Solid(green),
            brush_transform: Affine::IDENTITY,
            even_odd: false,
        },
        Op::PopLayer,
        Op::PopClip,
        // The upload at (40, 20), translated.
        Op::Transform(Affine::translate((40.0, 20.0))),
        Op::Image {
            rect: Rect::new(0.0, 0.0, 4.0, 4.0),
            image: 9,
            image_transform: Affine::IDENTITY,
            tint: None,
            smooth: false,
        },
    ];
    let px = frame(
        &mut h,
        Frame {
            surface: S,
            id: 2,
            size,
            scale: Scale::ONE,
            ops,
            uploads: vec![Upload {
                id: 9,
                generation: 1,
                pixmap: Arc::new(pm),
            }],
            retire: Vec::new(),
            clear: AlphaColor::TRANSPARENT,
        },
    );
    assert_eq!((px.width, px.height), (64, 32));
    assert!(px.stride >= 256 && px.stride.is_multiple_of(256));
    // BGRA bytes, premultiplied.
    assert_eq!(pixel(&px, 10, 10), [0, 0, 255, 255], "red");
    assert!(
        close(pixel(&px, 50, 8), [0, 128, 0, 128], 2),
        "green at half opacity: {:?}",
        pixel(&px, 50, 8)
    );
    assert_eq!(pixel(&px, 10, 30), [0, 0, 255, 255]);
    assert_eq!(
        pixel(&px, 34, 28),
        [0, 0, 0, 0],
        "cleared, outside the fills"
    );
    assert_eq!(
        pixel(&px, 41, 21),
        [0, 0, 255, 255],
        "the upload, back to red"
    );
    // A path and an even-odd ring.
    let mut ring = BezPath::new();
    ring.extend(Rect::new(0.0, 0.0, 20.0, 20.0).path_elements(0.1));
    ring.extend(Rect::new(5.0, 5.0, 15.0, 15.0).path_elements(0.1));
    let px = frame(
        &mut h,
        Frame {
            surface: S,
            id: 3,
            size,
            scale: Scale::ONE,
            ops: vec![Op::Fill {
                path: ring,
                brush: Brush::Solid(green),
                brush_transform: Affine::IDENTITY,
                even_odd: true,
            }],
            // Already uploaded: none sent again.
            uploads: vec![],
            retire: Vec::new(),
            clear: AlphaColor::TRANSPARENT,
        },
    );
    assert_eq!(pixel(&px, 2, 2), [0, 255, 0, 255]);
    assert_eq!(pixel(&px, 10, 10), [0, 0, 0, 0], "the ring's hole");
}

#[test]
fn shader_passes_read_their_uniforms_and_strand_globals() {
    let Some(mut h) = start() else { return };
    // Two uniforms at separate bindings: the second must land at its own
    // aligned range.
    let wgsl = "
@group(1) @binding(0) var<uniform> u_level: f32;
@group(1) @binding(3) var<uniform> u_tint: vec4<f32>;
@fragment
fn main(v: StrandVertex) -> @location(0) vec4<f32> {
    if v.uv.x < 0.5 {
        return vec4<f32>(u_level, 0.0, 0.0, 1.0);
    }
    // `size` is the box in buffer pixels.
    if strand.size.x == 32.0 && strand.time == 2.0 {
        return u_tint;
    }
    return vec4<f32>(1.0, 1.0, 1.0, 1.0);
}
";
    let pass = ShaderPass {
        code: ShaderRef::File(code(
            wgsl,
            vec![
                ("u_tint", UniformType::Vec4, 3),
                ("u_level", UniformType::F32, 0),
            ],
        )),
        // Slots in binding order: u_level, then u_tint.
        uniforms: Arc::from([0.5f32, 0.0, 0.0, 1.0, 1.0].as_slice()),
        input: ShaderInput::None,
    };
    h.gpu.send(GpuRequest::Pass(PassFrame {
        key: 42,
        id: 1,
        size: Size::new(32, 8),
        pass,
        globals: PassGlobals {
            time: 2.0,
            scale: 1.0,
            pointer: [-1.0, -1.0],
        },
    }));
    let px = match h.next() {
        GpuReply::PassPixels { key, frame, pixels } => {
            assert_eq!((key, frame), (42, 1));
            pixels
        }
        other => panic!("expected PassPixels, got {other:?}"),
    };
    assert!(
        close(pixel(&px, 4, 4), [0, 0, 128, 255], 1),
        "u_level red: {:?}",
        pixel(&px, 4, 4)
    );
    assert_eq!(pixel(&px, 28, 4), [255, 0, 0, 255], "u_tint blue (BGRA)");
}

#[test]
fn a_pass_inside_a_frame_is_drawn_in_place() {
    let Some(mut h) = start() else { return };
    let size = Size::new(32, 16);
    attach(&mut h, size);
    let wgsl = "
@fragment
fn main(v: StrandVertex) -> @location(0) vec4<f32> {
    return vec4<f32>(0.0, 1.0, 0.0, 1.0);
}
";
    let pass = ShaderPass {
        code: ShaderRef::File(code(wgsl, vec![])),
        uniforms: Arc::from([].as_slice()),
        input: ShaderInput::None,
    };
    let px = frame(
        &mut h,
        Frame {
            surface: S,
            id: 5,
            size,
            scale: Scale::ONE,
            ops: vec![Op::Pass {
                pass,
                bounds: Rect::new(8.0, 4.0, 24.0, 12.0),
                globals: PassGlobals::default(),
            }],
            uploads: vec![],
            retire: Vec::new(),
            clear: AlphaColor::TRANSPARENT,
        },
    );
    assert_eq!(pixel(&px, 16, 8), [0, 255, 0, 255], "inside the box");
    assert_eq!(pixel(&px, 2, 2), [0, 0, 0, 0], "outside it");
}

#[test]
fn a_broken_shader_fails_its_pass_and_the_device_stays_up() {
    let Some(mut h) = start() else { return };
    let pass = ShaderPass {
        code: ShaderRef::File(code(
            "@fragment fn main() -> @location(0) vec4<f32> { return nope; }",
            vec![],
        )),
        uniforms: Arc::from([].as_slice()),
        input: ShaderInput::None,
    };
    h.gpu.send(GpuRequest::Pass(PassFrame {
        key: 1,
        id: 1,
        size: Size::new(4, 4),
        pass,
        globals: PassGlobals::default(),
    }));
    match h.next() {
        GpuReply::Failed { key, error, .. } => {
            assert_eq!(key, Some(1));
            assert_eq!(error.kind, GpuErrorKind::Shader);
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    // Still up: a frame renders.
    attach(&mut h, Size::new(4, 4));
    let px = frame(
        &mut h,
        Frame {
            surface: S,
            id: 1,
            size: Size::new(4, 4),
            scale: Scale::ONE,
            ops: vec![],
            uploads: vec![],
            retire: Vec::new(),
            clear: AlphaColor::new([1.0, 1.0, 1.0, 1.0]),
        },
    );
    assert_eq!(pixel(&px, 0, 0), [255, 255, 255, 255]);
}

#[test]
fn a_software_adapter_counts_as_no_device_unless_accepted() {
    let (tx, pings) = mpsc::channel();
    let mut h = Harness {
        gpu: Gpu::spawn(
            Box::new(move || {
                let _ = tx.send(());
            }),
            GpuOptions { software: false },
        ),
        pings,
    };
    match h.next() {
        GpuReply::Ready(info) => assert!(!info.software, "a hardware adapter"),
        GpuReply::Unavailable(e) => {
            assert!(
                matches!(e.kind, GpuErrorKind::Software | GpuErrorKind::NoAdapter),
                "{e:?}"
            );
            assert!(matches!(h.next(), GpuReply::Exited));
        }
        other => panic!("{other:?}"),
    }
}

/// An even-odd clip cuts a hole (a shadow's ring), and a retired upload
/// is gone from the next frame (it draws nothing, as an unknown id).
#[test]
fn even_odd_clips_cut_holes_and_retired_uploads_go() {
    let Some(mut h) = start() else { return };
    let size = Size::new(32, 32);
    attach(&mut h, size);
    let blue = AlphaColor::new([0.0, 0.0, 1.0, 1.0]);
    let mut ring = Rect::new(0.0, 0.0, 32.0, 32.0).to_path(0.1);
    ring.extend(Rect::new(8.0, 8.0, 24.0, 24.0).to_path(0.1));
    let mut white = Pixmap::new(2, 2);
    for p in white.data_mut() {
        *p = strand_gpu::peniko::color::PremulRgba8 {
            r: 255,
            g: 255,
            b: 255,
            a: 255,
        };
    }
    let fill = Op::Fill {
        path: Rect::new(0.0, 0.0, 32.0, 32.0).to_path(0.1),
        brush: Brush::Solid(blue),
        brush_transform: Affine::IDENTITY,
        even_odd: false,
    };
    let image = Op::Image {
        rect: Rect::new(0.0, 0.0, 2.0, 2.0),
        image: 5,
        image_transform: Affine::IDENTITY,
        tint: None,
        smooth: false,
    };
    let f = |id, ops, uploads, retire| Frame {
        surface: S,
        id,
        size,
        scale: Scale::ONE,
        ops,
        uploads,
        retire,
        clear: AlphaColor::TRANSPARENT,
    };
    let px = frame(
        &mut h,
        f(
            1,
            vec![Op::PushClipEvenOdd(ring), fill, Op::PopClip, image.clone()],
            vec![Upload {
                id: 5,
                generation: 0,
                pixmap: Arc::new(white),
            }],
            vec![],
        ),
    );
    assert_eq!(pixel(&px, 4, 16), [255, 0, 0, 255], "the ring is blue");
    assert_eq!(pixel(&px, 16, 16), [0, 0, 0, 0], "the hole is not");
    assert_eq!(pixel(&px, 1, 1), [255, 255, 255, 255], "the upload");
    let px = frame(&mut h, f(2, vec![image], vec![], vec![5]));
    assert_eq!(pixel(&px, 1, 1), [0, 0, 0, 0], "retired");
}
