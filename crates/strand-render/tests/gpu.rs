//! (M4) The GPU backend driven through the renderer, on whatever Vulkan
//! device the environment has (lavapipe in CI, with
//! `STRAND_GPU_SOFTWARE=1`; skipped without one unless
//! `STRAND_REQUIRE_GPU=1`): a `shader` node's pass read back into a CPU
//! frame, a promoted surface's frames drawn by the GPU and compared with
//! the CPU's, the device dropped after idle, and the CPU fallback.
//!
//! The GPU-vs-CPU comparison has its own tolerance: the GPU draws
//! gradients without the CPU's dither and rasterises edges with its own
//! coverage, so a channel may differ by [`GPU_TOLERANCE`] and a few edge
//! pixels by more ([`EDGE_SHARE`] of the buffer at most).

#![cfg(feature = "gpu")]

mod canvas_scene;
mod common;
mod gpu_host;

use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::*;
use gpu_host::*;
use strand_gpu::{Gpu, GpuErrorKind, GpuOptions, GpuReply, GpuRequest};
use strand_render::Renderer;
use strand_scene::shader::{ShaderCode, UniformType};
use strand_scene::*;

const S: SurfaceId = SurfaceId(1);

/// Per-channel difference allowed between the GPU's and the CPU's frame.
const GPU_TOLERANCE: u8 = 6;

/// Share of pixels (edges) allowed past [`GPU_TOLERANCE`].
const EDGE_SHARE: f64 = 0.02;

/// A file that paints `u_tint` over its box, fading to transparent at
/// the right edge by `u_fade` (0: none).
fn tint_code() -> Arc<ShaderCode> {
    Arc::new(ShaderCode {
        path: "tint.wgsl".into(),
        wgsl: "@group(1) @binding(0) var<uniform> u_tint: vec4<f32>;\n\
               @group(1) @binding(1) var<uniform> u_fade: f32;\n\
               @fragment\n\
               fn main(v: StrandVertex) -> @location(0) vec4<f32> {\n\
                   return u_tint * (1.0 - u_fade * v.uv.x);\n\
               }\n"
        .into(),
        uniforms: ShaderCode::packed(vec![
            ("u_tint".into(), UniformType::Vec4, 0),
            ("u_fade".into(), UniformType::F32, 1),
        ]),
    })
}

/// A 240×60 bar with a 40×20 `shader` node at (10, 10).
fn shader_scene(tint: &str) -> (SceneDiff, NodeId) {
    let mut b = Builder::default();
    let node = shader_bar(&mut b, tint).1;
    (b.diff, node)
}

/// [`shader_scene`]'s bar and node, in `b`.
fn shader_bar(b: &mut Builder, tint: &str) -> (NodeId, NodeId) {
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let node = b.node(
        NodeKind::Shader,
        Some(root),
        vec![
            (Prop::X, num(10.0)),
            (Prop::Y, num(10.0)),
            (Prop::Width, num(40.0)),
            (Prop::Height, num(20.0)),
            (Prop::Shader, PropValue::Shader(tint_code())),
            (
                Prop::Uniforms,
                PropValue::Uniforms(vec![("u_tint".into(), color(tint))]),
            ),
        ],
    );
    (root, node)
}

#[test]
fn a_shader_node_is_drawn_offscreen_and_read_back_into_a_cpu_frame() {
    let Some(opts) = device() else { return };
    let mut r = renderer();
    let (diff, node) = shader_scene("#ff0000");
    assert!(r.apply(diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(240, 60, Scale::ONE);
    let mut host = Host::new(opts);
    // The first frame: the device is not up, so the node keeps its box
    // and draws nothing (the CPU fallback), and the device is asked for.
    buf.paint(&mut r, S, 0);
    assert_eq!(buf.px(20, 20), buf.px(100, 20), "nothing drawn yet");
    assert_eq!(r.gpu_status(), GpuStatus::Starting);
    assert!(r.gpu_in_demand());
    host.send(&mut r);
    host.until(&mut r, |m| matches!(m, GpuReply::PassPixels { .. }));
    assert!(matches!(r.gpu_status(), GpuStatus::Up(_)));
    assert!(r.wants_frame(S), "the pass's pixels repaint the node");
    buf.paint(&mut r, S, 1);
    assert_eq!(buf.px(20, 20), [0, 0, 255, 255], "red, from the pass");
    assert_eq!(buf.px(5, 5), buf.px(100, 20), "only inside its box");
    // A uniform change asks for a new pass; the frame holds for it
    // (`frame_deadline`) until the pixels come.
    let mut d = SceneDiff::new();
    d.set(
        node,
        Prop::Uniforms,
        PropValue::Uniforms(vec![("u_tint".into(), color("#0000ff"))]),
    );
    assert!(r.apply(d).is_empty());
    r.update();
    assert!(r.frame_deadline(S).is_some(), "held for the pass");
    host.send(&mut r);
    host.until(&mut r, |m| matches!(m, GpuReply::PassPixels { .. }));
    assert_eq!(r.frame_deadline(S), None);
    buf.paint(&mut r, S, 1);
    assert_eq!(buf.px(20, 20), [255, 0, 0, 255], "blue");
    // Nothing changes: no pass is asked for again.
    buf.paint(&mut r, S, 1);
    assert!(r.take_gpu_requests().is_empty());
}

/// (gpu-first) The warm start: `warm_gpu` asks the host to start the
/// device before anything needs it (`BackendChange::Start`, once), so a
/// later pass or promotion finds it up and starts nothing.
#[test]
fn the_warm_start_brings_the_device_up_before_anything_needs_it() {
    let Some(opts) = device() else { return };
    let mut r = renderer();
    let (diff, _) = shader_scene("#00ff00");
    let mut host = Host::new(opts);
    r.warm_gpu();
    assert_eq!(r.take_backend_changes(), [BackendChange::Start]);
    assert_eq!(r.gpu_status(), GpuStatus::Starting);
    host.start();
    host.until(&mut r, |m| matches!(m, GpuReply::Ready(_)));
    assert!(matches!(r.gpu_status(), GpuStatus::Up(_)));
    r.warm_gpu();
    assert!(r.take_backend_changes().is_empty(), "up: nothing to start");
    // A shader shows: its pass goes to the running device.
    assert!(r.apply(diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(240, 60, Scale::ONE);
    buf.paint(&mut r, S, 0);
    assert!(r.take_backend_changes().is_empty());
    host.send(&mut r);
    host.until(&mut r, |m| matches!(m, GpuReply::PassPixels { .. }));
    buf.paint(&mut r, S, 1);
    assert_eq!(buf.px(20, 20), [0, 255, 0, 255], "green, from the pass");
}

/// (gpu-first) The device is kept up once nothing needs it: no wake is
/// asked for, nothing is reported, and the thread keeps running (3a638d14
/// dropped it 30 s after its last use, at a wake of render's).
#[test]
fn the_device_stays_up_when_idle() {
    let Some(opts) = device() else { return };
    let mut r = renderer();
    let (diff, node) = shader_scene("#00ff00");
    assert!(r.apply(diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(240, 60, Scale::ONE);
    let mut host = Host::new(opts);
    buf.paint(&mut r, S, 0);
    host.send(&mut r);
    host.until(&mut r, |m| matches!(m, GpuReply::PassPixels { .. }));
    buf.paint(&mut r, S, 1);
    // The node goes: nothing wants the GPU any more.
    let mut d = SceneDiff::new();
    d.remove(node);
    assert!(r.apply(d).is_empty());
    buf.paint(&mut r, S, 1);
    assert!(!r.gpu_in_demand());
    assert_eq!(r.next_wake(), None, "no wake: an idle shell takes none");
    std::thread::sleep(Duration::from_millis(100));
    r.update();
    assert!(r.take_backend_changes().is_empty(), "the device is kept");
    assert!(matches!(r.gpu_status(), GpuStatus::Up(_)));
    assert!(
        host.gpu.as_ref().is_some_and(|g| !g.exited()),
        "the thread runs"
    );
}

/// (gpu-first review 2) A kept-warm device that failed for a reason that
/// may pass is started again at its retry deadline, once: that wake is
/// the only one, so the next animation does not wait for a device, and a
/// device that fails again leaves the shell idle (asked for on demand
/// only). Once up again, a later loss gets its one retry too. 47a3cd04
/// asked again only on demand. Needs no device: replies by hand.
#[test]
fn a_failed_warm_device_is_started_again_once_at_its_retry() {
    let retry = Duration::from_millis(40);
    let mut r = renderer();
    r.set_gpu_retry(retry);
    let starts = |r: &mut Renderer| {
        r.take_backend_changes()
            .iter()
            .filter(|c| matches!(c, BackendChange::Start))
            .count()
    };
    let failed = |r: &mut Renderer, kind| {
        r.deliver_gpu(GpuReply::Unavailable(strand_gpu::GpuError {
            kind,
            message: "the GPU failed to start (test)".into(),
        }));
        r.deliver_gpu(GpuReply::Exited);
    };
    // Nothing warm asked for: a failure is retried on demand only.
    let mut cold = renderer();
    cold.set_gpu_retry(retry);
    cold.promote_now(S);
    let _ = cold.take_backend_changes();
    failed(&mut cold, GpuErrorKind::Device);
    assert_eq!(cold.next_wake(), None, "no warm start: no retry wake");
    // The warm start fails.
    r.warm_gpu();
    assert_eq!(starts(&mut r), 1, "the warm start");
    let at = Instant::now();
    failed(&mut r, GpuErrorKind::Device);
    let wake = r.next_wake().expect("a wake at the retry");
    assert!(
        wake >= at && wake <= at + retry + Duration::from_millis(20),
        "{wake:?}"
    );
    r.update();
    assert_eq!(starts(&mut r), 0, "not before the deadline");
    sleep_past(wake);
    r.update();
    assert_eq!(starts(&mut r), 1, "started again at the retry");
    assert_eq!(r.gpu_status(), GpuStatus::Starting);
    // It fails again: no wake, and none later.
    failed(&mut r, GpuErrorKind::Device);
    assert_eq!(r.next_wake(), None, "one retry, then idle");
    std::thread::sleep(retry * 2);
    r.update();
    assert_eq!(starts(&mut r), 0, "not asked for again unprompted");
    // Asked for on demand, it comes up, then is lost: one retry again.
    r.promote_now(S);
    let _ = r.take_backend_changes();
    up_without_a_device(&mut r);
    r.deliver_gpu(GpuReply::Lost(strand_gpu::GpuError {
        kind: GpuErrorKind::Lost,
        message: "the GPU device was lost (test)".into(),
    }));
    r.deliver_gpu(GpuReply::Exited);
    let wake = r.next_wake().expect("a wake at the lost device's retry");
    sleep_past(wake);
    r.update();
    assert_eq!(starts(&mut r), 1, "a lost device is warmed again");
    // A final answer (no adapter) asks for nothing again.
    failed(&mut r, GpuErrorKind::NoAdapter);
    assert_eq!(r.next_wake(), None);
}

/// A still `shader` node keeps its pixels when the device goes (its
/// thread ended): they are CPU pixmaps, not GPU memory. A later frame of
/// its surface (the bar's clock ticking) repaints it from them, asks for
/// no new pass, and so does not start the device again.
#[test]
fn a_still_shader_keeps_its_pixels_when_the_device_goes() {
    let Some(opts) = device() else { return };
    let mut r = renderer();
    let (diff, _) = shader_scene("#ff0000");
    assert!(r.apply(diff).is_empty());
    let root = r.tree().roots()[0];
    r.attach_surface(S, root);
    let mut buf = Buffer::new(240, 60, Scale::ONE);
    let mut host = Host::new(opts);
    buf.paint(&mut r, S, 0);
    host.send(&mut r);
    host.until(&mut r, |m| matches!(m, GpuReply::PassPixels { .. }));
    buf.paint(&mut r, S, 1);
    assert_eq!(buf.px(20, 20), [0, 0, 255, 255], "red, from the pass");
    // The thread ends.
    host.end(&mut r);
    assert_eq!(r.gpu_status(), GpuStatus::Unused);
    // Something else on the bar changes: the shader still shows red, in
    // a full repaint too, and nothing asks for the device.
    let mut d = SceneDiff::new();
    d.set(root, Prop::Bg, color("#313244"));
    assert!(r.apply(d).is_empty());
    buf.pixels.fill(0);
    buf.paint(&mut r, S, 0);
    assert_eq!(buf.px(20, 20), [0, 0, 255, 255], "the pass's pixels stay");
    assert_ne!(buf.px(100, 20), [0, 0, 0, 0], "the frame was painted");
    assert!(r.take_gpu_requests().is_empty(), "no new pass");
    assert!(r.take_backend_changes().is_empty());
    assert_eq!(r.gpu_status(), GpuStatus::Unused, "the device stays down");
    assert_eq!(r.next_wake(), None, "nothing to wake for");
}

/// A readback surface resized: the pixels that come back first were
/// drawn at the old size (frames run one behind), so that frame is the
/// CPU's, at the new size, and the GPU's next one is copied in.
#[test]
fn a_resized_readback_surface_does_not_copy_old_size_pixels() {
    let Some(opts) = device() else { return };
    let mut cpu = renderer();
    rich(&mut cpu);
    let mut want = Buffer::new(200, 60, Scale::ONE);
    want.paint(&mut cpu, S, 0);
    cpu.update();
    want.paint(&mut cpu, S, 0);

    let mut r = renderer();
    rich(&mut r);
    let mut buf = Buffer::new(240, 60, Scale::ONE);
    buf.paint(&mut r, S, 0);
    r.update();
    buf.paint(&mut r, S, 0);
    let mut host = Host::new(opts);
    r.promote_now(S);
    host.send(&mut r);
    host.until(&mut r, |m| matches!(m, GpuReply::Attached { .. }));
    assert_eq!(r.backend(S), Backend::GpuReadback);
    buf.paint(&mut r, S, 0);
    host.send(&mut r);
    host.until(&mut r, |m| matches!(m, GpuReply::Pixels { .. }));
    // The 240×60 pixels are in; the surface is now 200×60.
    let copied = r.gpu_frames_copied();
    let mut small = Buffer::new(200, 60, Scale::ONE);
    small.paint(&mut r, S, 0);
    assert_eq!(r.gpu_frames_copied(), copied, "old-size pixels copied in");
    assert_eq!(small.pixels, want.pixels, "the CPU's frame at the new size");
    // The frame sent at the new size comes back and is copied in.
    host.send(&mut r);
    host.until(&mut r, |m| matches!(m, GpuReply::Pixels { .. }));
    small.pixels.fill(0);
    small.paint(&mut r, S, 0);
    assert_eq!(r.gpu_frames_copied(), copied + 1, "drawn by the GPU");
    let (bad, worst) = compare(&small, &want);
    assert!(
        bad as f64 / (200.0 * 60.0) <= EDGE_SHARE,
        "{bad} pixels differ by more than {GPU_TOLERANCE} (worst {worst})"
    );
}

/// A scene with what lowering covers: solid and gradient fills, a
/// border, a shadow, an opacity group, a masked layer (drawn on the CPU)
/// and text.
fn rich_scene() -> SceneDiff {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(8.0)),
            (Prop::Y, num(10.0)),
            (Prop::Width, num(60.0)),
            (Prop::Height, num(40.0)),
            (Prop::Radius, num(10.0)),
            (
                Prop::Bg,
                PropValue::Paint(Paint::Linear {
                    angle: 90.0,
                    stops: vec![
                        GradientStop {
                            offset: 0.0,
                            color: hex("#f38ba8"),
                        },
                        GradientStop {
                            offset: 1.0,
                            color: hex("#89b4fa"),
                        },
                    ],
                }),
            ),
            (
                Prop::Border,
                PropValue::Border(Border {
                    width: 2.0,
                    paint: Paint::Solid(hex("#f9e2af")),
                }),
            ),
            (
                Prop::Shadow,
                PropValue::Shadow(vec![Shadow {
                    x: 0.0,
                    y: 2.0,
                    blur: 6.0,
                    spread: 0.0,
                    color: hex("#00000080"),
                }]),
            ),
        ],
    );
    b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(80.0)),
            (Prop::Y, num(10.0)),
            (Prop::Size, num(40.0)),
            (Prop::Bg, color("#a6e3a1")),
            (Prop::Opacity, num(0.5)),
        ],
    );
    b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(130.0)),
            (Prop::Y, num(10.0)),
            (Prop::Size, num(40.0)),
            (Prop::Bg, color("#cba6f7")),
            (
                Prop::Mask,
                PropValue::Call {
                    name: "radial".into(),
                    args: vec![PropValue::Keyword("center".into()), num(16.0)],
                },
            ),
        ],
    );
    b.node(
        NodeKind::Text,
        Some(root),
        vec![
            (Prop::X, num(180.0)),
            (Prop::Y, num(20.0)),
            (Prop::Text, text("Strand")),
            (Prop::Font, PropValue::Font(font(14.0))),
            (Prop::Color, color("#cdd6f4")),
        ],
    );
    b.diff
}

fn rich(r: &mut Renderer) {
    assert!(r.apply(rich_scene()).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
}

/// Pixels past the tolerance, and the worst channel difference.
fn compare(a: &Buffer, b: &Buffer) -> (usize, u8) {
    let mut bad = 0;
    let mut worst = 0;
    for (x, y) in a.pixels.chunks_exact(4).zip(b.pixels.chunks_exact(4)) {
        let d = x
            .iter()
            .zip(y)
            .map(|(p, q)| p.abs_diff(*q))
            .max()
            .unwrap_or(0);
        worst = worst.max(d);
        if d > GPU_TOLERANCE {
            bad += 1;
        }
    }
    (bad, worst)
}

/// A promoted surface in readback mode: the GPU draws its frames from the
/// lowered display list, and they match the CPU's within the GPU
/// tolerance.
#[test]
fn a_promoted_surface_is_drawn_by_the_gpu_like_the_cpu() {
    let Some(opts) = device() else { return };
    let mut cpu = renderer();
    rich(&mut cpu);
    let mut want = Buffer::new(240, 60, Scale::ONE);
    want.paint(&mut cpu, S, 0);
    // Text arrives inline; paint again so both frames have it.
    cpu.update();
    want.paint(&mut cpu, S, 0);

    let mut r = renderer();
    rich(&mut r);
    let mut buf = Buffer::new(240, 60, Scale::ONE);
    buf.paint(&mut r, S, 0);
    r.update();
    buf.paint(&mut r, S, 0);
    let mut host = Host::new(opts);
    r.promote_now(S);
    host.send(&mut r);
    host.until(&mut r, |m| matches!(m, GpuReply::Attached { .. }));
    assert_eq!(r.backend(S), Backend::GpuReadback);
    // The first promoted frame is the CPU's (no pixels yet) and sends
    // the frame; the next copies the GPU's pixels in.
    buf.pixels.fill(0);
    buf.paint(&mut r, S, 0);
    host.send(&mut r);
    host.until(&mut r, |m| matches!(m, GpuReply::Pixels { .. }));
    buf.pixels.fill(0);
    let copied = r.gpu_frames_copied();
    buf.paint(&mut r, S, 0);
    assert_eq!(r.gpu_frames_copied(), copied + 1, "drawn by the GPU");
    let (bad, worst) = compare(&buf, &want);
    let share = bad as f64 / (240.0 * 60.0);
    eprintln!("GPU vs CPU: {bad} pixels past {GPU_TOLERANCE}, worst {worst}");
    if share > EDGE_SHARE {
        write_png(
            &refs_dir().join("gpu_rich.actual.png"),
            240,
            60,
            &buf.to_rgba(),
        );
    }
    assert!(
        share <= EDGE_SHARE,
        "{bad} pixels differ by more than {GPU_TOLERANCE} (worst {worst})"
    );
    // Demoted: the CPU draws again, in full.
    r.set_backend(S, Backend::Cpu);
    buf.pixels.fill(0);
    buf.paint(&mut r, S, 1);
    assert_eq!(buf.pixels, want.pixels, "the CPU's own frame");
}

/// (m4-integration-w2) S-effects' paints on a promoted surface: a
/// shape-library mask and gradient-`fill:` text are drawn on the CPU and
/// uploaded, so the GPU's frame matches the CPU's (neither is drawn
/// unmasked or as plain glyphs).
#[test]
fn shape_masks_and_text_fills_are_drawn_by_the_gpu_like_the_cpu() {
    let Some(opts) = device() else { return };
    let scene = || {
        let mut b = Builder::default();
        let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
        b.node(
            NodeKind::Box,
            Some(root),
            vec![
                (Prop::X, num(10.0)),
                (Prop::Y, num(10.0)),
                (Prop::Size, num(40.0)),
                (Prop::Bg, color("#a6e3a1")),
                (
                    Prop::Mask,
                    PropValue::Call {
                        name: "shape".into(),
                        args: vec![PropValue::Keyword("circle".into())],
                    },
                ),
            ],
        );
        b.node(
            NodeKind::Text,
            Some(root),
            vec![
                (Prop::X, num(70.0)),
                (Prop::Y, num(12.0)),
                (Prop::Text, text("Strand")),
                (Prop::Font, PropValue::Font(font(28.0))),
                (
                    Prop::Fill,
                    PropValue::Paint(Paint::Linear {
                        angle: 90.0,
                        stops: vec![
                            GradientStop {
                                offset: 0.0,
                                color: hex("#f38ba8"),
                            },
                            GradientStop {
                                offset: 1.0,
                                color: hex("#89b4fa"),
                            },
                        ],
                    }),
                ),
            ],
        );
        b.diff
    };
    let setup = |r: &mut Renderer| {
        assert!(r.apply(scene()).is_empty());
        r.attach_surface(S, r.tree().roots()[0]);
    };
    let mut cpu = renderer();
    setup(&mut cpu);
    let mut want = Buffer::new(240, 60, Scale::ONE);
    want.paint(&mut cpu, S, 0);
    cpu.update();
    want.paint(&mut cpu, S, 0);
    // The circle leaves its box's corner bare.
    assert_eq!(want.px(11, 11), want.px(5, 5), "the mask cuts the corner");

    let mut r = renderer();
    setup(&mut r);
    let mut buf = Buffer::new(240, 60, Scale::ONE);
    buf.paint(&mut r, S, 0);
    r.update();
    buf.paint(&mut r, S, 0);
    let mut host = Host::new(opts);
    r.promote_now(S);
    host.send(&mut r);
    host.until(&mut r, |m| matches!(m, GpuReply::Attached { .. }));
    assert_eq!(r.backend(S), Backend::GpuReadback);
    buf.pixels.fill(0);
    buf.paint(&mut r, S, 0);
    host.send(&mut r);
    host.until(&mut r, |m| matches!(m, GpuReply::Pixels { .. }));
    buf.pixels.fill(0);
    let copied = r.gpu_frames_copied();
    buf.paint(&mut r, S, 0);
    assert_eq!(r.gpu_frames_copied(), copied + 1, "drawn by the GPU");
    let (bad, worst) = compare(&buf, &want);
    eprintln!("GPU vs CPU: {bad} pixels past {GPU_TOLERANCE}, worst {worst}");
    assert!(
        bad as f64 / (240.0 * 60.0) <= EDGE_SHARE,
        "{bad} pixels differ by more than {GPU_TOLERANCE} (worst {worst})"
    );
}

/// The launcher's card at 1.5×: a `w × h` box with corners 18, 18, 32,
/// 32 and `elevation.xl`'s shadow (0 16px 48px), on a 760 × 560 panel.
/// Its node and the buffer to paint.
fn launcher_card(r: &mut Renderer, w: f32, h: f32) -> (NodeId, Buffer) {
    let scale = Scale::new(180).unwrap();
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(760.0)),
            (Prop::Height, num(560.0)),
            (Prop::Bg, color("#e6e9ef")),
        ],
    );
    let card = b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::Place, PropValue::Keyword("absolute".into())),
            (Prop::X, num(40.0)),
            (Prop::Y, num(30.0)),
            (Prop::Width, num(w)),
            (Prop::Height, num(h)),
            (Prop::Bg, color("#ffffff")),
            (
                Prop::Radius,
                PropValue::List(vec![num(18.0), num(18.0), num(32.0), num(32.0)]),
            ),
            (
                Prop::Shadow,
                PropValue::Shadow(vec![Shadow {
                    x: 0.0,
                    y: 16.0,
                    blur: 48.0,
                    spread: 0.0,
                    color: hex("#00000066"),
                }]),
            ),
        ],
    );
    assert!(r.apply(b.diff).is_empty());
    let spec = r.surface_spec(root).unwrap().clone();
    let size = scale.physical_size(LogicalSize::new(
        spec.width.unwrap() + spec.overhang.left + spec.overhang.right,
        spec.height.unwrap() + spec.overhang.top + spec.overhang.bottom,
    ));
    r.attach_surface(S, root);
    r.configure_surface(S, size, scale);
    (card, Buffer::new(size.w, size.h, scale))
}

fn resize(r: &mut Renderer, card: NodeId, w: f32, h: f32) {
    let mut d = SceneDiff::new();
    for (prop, v) in [(Prop::Width, w), (Prop::Height, h)] {
        d.push(SceneOp::SetProp {
            id: card,
            prop,
            value: num(v),
            transition: Transition::Instant,
        });
    }
    assert!(r.apply(d).is_empty());
}

/// (shadow-slice, perf-gpu-retain) A promoted surface's shadow is drawn
/// by the GPU (`Op::BlurredRect`, the CPU's primitive): as the
/// launcher's card resizes, no frame builds or uploads a pixmap for it
/// (each frame uploaded a new 3.8 MB shadow before slicing, and slicing
/// still built a corner tile a frame under a springing radius). Needs
/// no device: the frames are only lowered.
#[test]
fn a_resizing_shadow_is_drawn_by_the_gpu_with_nothing_built() {
    let mut r = renderer();
    let (card, mut buf) = launcher_card(&mut r, 560.0, 380.0);
    let t0 = Duration::from_secs(1);
    buf.paint_at(&mut r, S, 0, t0);
    r.promote_now(S);
    r.set_backend(S, Backend::GpuPresent);
    let _ = r.take_backend_changes();
    resize(&mut r, card, 566.0, 383.0);
    // What the CPU frame before built.
    let before = r.paint_cache().1;
    let first = r
        .paint_gpu(S, t0 + Duration::from_millis(16))
        .expect("resized");
    let images = |f: &strand_gpu::Frame| {
        f.ops
            .iter()
            .filter(|op| matches!(op, strand_gpu::Op::Image { .. }))
            .count()
    };
    let shadows = |f: &strand_gpu::Frame| {
        f.ops
            .iter()
            .filter(|op| matches!(op, strand_gpu::Op::BlurredRect { .. }))
            .count()
    };
    let builds = r.paint_cache().1;
    for i in 2..=10 {
        resize(&mut r, card, 560.0 + 6.0 * i as f32, 380.0 + 3.0 * i as f32);
        let f = r
            .paint_gpu(S, t0 + Duration::from_millis(16 * i))
            .expect("resized");
        assert!(
            f.uploads.is_empty(),
            "frame {i} uploaded {}",
            f.uploads.len()
        );
        assert_eq!(images(&f), images(&first));
        // One blurred rect, or one per quadrant when its corners differ.
        assert!(
            shadows(&f) == 1 || shadows(&f) == 4,
            "frame {i} draws its shadow"
        );
        assert_eq!(r.paint_cache().1, builds, "frame {i} built a pixmap");
    }
    assert_eq!(images(&first), 0, "no shadow pixmap drawn");
    assert_eq!(r.paint_cache().1, before, "nothing built for the GPU");
}

/// (shadow-slice) The sliced shadow the GPU draws matches the CPU's, at
/// the card's size after a resize.
#[test]
fn a_sliced_shadow_on_the_gpu_matches_the_cpu() {
    let Some(opts) = device() else { return };
    let mut cpu = renderer();
    let (_, mut want) = launcher_card(&mut cpu, 600.0, 400.0);
    want.paint(&mut cpu, S, 0);

    let mut r = renderer();
    let (card, mut buf) = launcher_card(&mut r, 560.0, 380.0);
    buf.paint(&mut r, S, 0);
    let mut host = Host::new(opts);
    r.promote_now(S);
    host.send(&mut r);
    host.until(&mut r, |m| matches!(m, GpuReply::Attached { .. }));
    assert_eq!(r.backend(S), Backend::GpuReadback);
    for (w, h) in [(580.0, 390.0), (600.0, 400.0)] {
        resize(&mut r, card, w, h);
        buf.paint(&mut r, S, 0);
        host.send(&mut r);
        host.until(&mut r, |m| matches!(m, GpuReply::Pixels { .. }));
    }
    buf.pixels.fill(0);
    let copied = r.gpu_frames_copied();
    buf.paint(&mut r, S, 0);
    assert_eq!(r.gpu_frames_copied(), copied + 1, "drawn by the GPU");
    let (bad, worst) = compare(&buf, &want);
    let n = (buf.size.w * buf.size.h) as f64;
    eprintln!("GPU vs CPU: {bad} pixels past {GPU_TOLERANCE}, worst {worst}");
    if bad as f64 / n > EDGE_SHARE {
        write_png(
            &refs_dir().join("gpu_sliced_shadow.actual.png"),
            buf.size.w,
            buf.size.h,
            &buf.to_rgba(),
        );
    }
    assert!(
        bad as f64 / n <= EDGE_SHARE,
        "{bad} pixels differ by more than {GPU_TOLERANCE} (worst {worst})"
    );
}

/// No device: a `shader` node keeps its box and draws nothing, the status
/// says why, and the device is not asked for again within 30 s.
#[test]
fn without_a_device_the_cpu_draws_and_the_shader_draws_nothing() {
    // A software adapter refused: what a user without a GPU gets. On
    // hardware the device comes up and there is nothing to check here.
    let opts = GpuOptions { software: false };
    let mut r = renderer();
    let (diff, _) = shader_scene("#ff0000");
    assert!(r.apply(diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(240, 60, Scale::ONE);
    let mut host = Host::new(opts);
    buf.paint(&mut r, S, 0);
    host.send(&mut r);
    let reply = host.until(&mut r, |m| {
        matches!(m, GpuReply::Unavailable(_) | GpuReply::PassPixels { .. })
    });
    match reply {
        GpuReply::Unavailable(e) => {
            assert!(matches!(
                e.kind,
                GpuErrorKind::Software | GpuErrorKind::NoAdapter
            ));
        }
        _ => {
            eprintln!("a hardware device came up: nothing to fall back from");
            return;
        }
    }
    assert!(matches!(r.gpu_status(), GpuStatus::Unavailable { .. }));
    buf.paint(&mut r, S, 1);
    assert_eq!(buf.px(20, 20), buf.px(100, 20), "the node draws nothing");
    // A change that wants a pass again does not ask within 30 s.
    let mut d = SceneDiff::new();
    d.push(SceneOp::SetProp {
        id: r.tree().roots()[0],
        prop: Prop::Opacity,
        value: num(0.9),
        transition: Transition::Instant,
    });
    assert!(r.apply(d).is_empty());
    buf.paint(&mut r, S, 1);
    assert!(r.take_gpu_requests().is_empty());
}

/// A pass's pixels in the CPU frame, against a reference (`gpu_shader.png`):
/// a tint fading to transparent across the box, over the bar, at 2×.
#[test]
fn a_shader_pass_matches_its_reference() {
    let Some(opts) = device() else { return };
    let mut r = renderer();
    let (diff, node) = shader_scene("#f9e2af");
    assert!(r.apply(diff).is_empty());
    let mut d = SceneDiff::new();
    d.set(
        node,
        Prop::Uniforms,
        PropValue::Uniforms(vec![
            ("u_fade".into(), num(1.0)),
            ("u_tint".into(), color("#f9e2af")),
        ]),
    );
    assert!(r.apply(d).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let scale = Scale::new(240).unwrap();
    let mut buf = Buffer::new(480, 120, scale);
    let mut host = Host::new(opts);
    buf.paint(&mut r, S, 0);
    host.send(&mut r);
    host.until(&mut r, |m| matches!(m, GpuReply::PassPixels { .. }));
    buf.paint(&mut r, S, 1);
    // The pass is drawn at the node's buffer size (80×40 at 2×).
    assert_ne!(buf.px(22, 22), buf.px(200, 60));
    assert_matches_ref("gpu_shader", &buf, 2);
}

/// A promoted canvas: its draw list lowered to the GPU matches the CPU's
/// raster within the GPU tolerance.
#[test]
fn a_canvas_is_drawn_by_the_gpu_like_the_cpu() {
    canvas_on_the_gpu_like_the_cpu(canvas_scene::chart);
}

/// (flex) The same for paths, arcs and an even-odd fill.
#[test]
fn canvas_paths_are_drawn_by_the_gpu_like_the_cpu() {
    canvas_on_the_gpu_like_the_cpu(canvas_scene::paths);
}

fn canvas_on_the_gpu_like_the_cpu(ops: fn() -> Vec<strand_scene::canvas::DrawOp>) {
    let Some(opts) = device() else { return };
    let paint = |r: &mut Renderer, buf: &mut Buffer| {
        let (diff, _) = canvas_scene::canvas_scene(ops());
        assert!(r.apply(diff).is_empty());
        r.attach_surface(S, r.tree().roots()[0]);
        buf.paint(r, S, 0);
    };
    let mut cpu = renderer();
    let mut want = Buffer::new(160, 60, Scale::ONE);
    paint(&mut cpu, &mut want);

    let mut r = renderer();
    let mut buf = Buffer::new(160, 60, Scale::ONE);
    paint(&mut r, &mut buf);
    let mut host = Host::new(opts);
    r.promote_now(S);
    host.send(&mut r);
    host.until(&mut r, |m| matches!(m, GpuReply::Attached { .. }));
    buf.pixels.fill(0);
    buf.paint(&mut r, S, 0);
    host.send(&mut r);
    host.until(&mut r, |m| matches!(m, GpuReply::Pixels { .. }));
    buf.pixels.fill(0);
    let copied = r.gpu_frames_copied();
    buf.paint(&mut r, S, 0);
    assert_eq!(r.gpu_frames_copied(), copied + 1, "drawn by the GPU");
    let (bad, worst) = compare(&buf, &want);
    let share = bad as f64 / (160.0 * 60.0);
    eprintln!("canvas GPU vs CPU: {bad} pixels past {GPU_TOLERANCE}, worst {worst}");
    if share > EDGE_SHARE {
        write_png(
            &refs_dir().join("gpu_canvas.actual.png"),
            160,
            60,
            &buf.to_rgba(),
        );
    }
    assert!(
        share <= EDGE_SHARE,
        "{bad} pixels differ by more than {GPU_TOLERANCE} (worst {worst})"
    );
}

/// The split palette of `theme_swap.rs`: `$fg` over `$surface` and
/// `$surface.split`, grey and grey, then black and white, which no
/// spring keeps readable, so the swap crossfades.
fn split_tables() -> (TokenTable, TokenTable) {
    use strand_theme::{Options, from_seed};
    let grey = Color::from_oklch(Oklch {
        l: 0.6,
        c: 0.0,
        h: 0.0,
        alpha: 1.0,
    });
    let palette = from_seed(
        hex(strand_theme::defaults::DEFAULT_SEED),
        Options {
            dark: false,
            ..Options::default()
        },
    );
    let mut a = strand_theme::defaults::base_tokens();
    palette.insert_into(&mut a);
    a.insert("font.ui", PropValue::Font(font(14.0)));
    let mut b = a.clone();
    for t in [&mut a, &mut b] {
        t.insert("surface.split", PropValue::Color(grey));
        t.insert_contrast("fg", vec!["surface".into(), "surface.split".into()]);
    }
    a.insert("surface", PropValue::Color(grey));
    b.insert("surface", PropValue::Color(Color::BLACK));
    b.insert("surface.split", PropValue::Color(Color::WHITE));
    (a, b)
}

/// A 640×330 panel (large enough to stay promoted while it crossfades)
/// in `$surface`, its right half in `$surface.split`, text in `$fg`
/// over both.
fn fade_scene(r: &mut Renderer, t: TokenTable) {
    let tok = |p: &str| PropValue::Token(TokenExpr::path(p));
    let mut b = Builder::default();
    b.diff.set_tokens(t, Transition::Instant);
    let root = b.node(NodeKind::Panel, None, vec![(Prop::Bg, tok("surface"))]);
    b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(320.0)),
            (Prop::Y, num(0.0)),
            (Prop::Width, num(320.0)),
            (Prop::Height, num(330.0)),
            (Prop::Bg, tok("surface.split")),
        ],
    );
    for x in [40.0, 360.0] {
        b.node(
            NodeKind::Text,
            Some(root),
            vec![
                (Prop::X, num(x)),
                (Prop::Y, num(150.0)),
                (Prop::Text, text("Strand")),
                (Prop::Color, tok("fg")),
            ],
        );
    }
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
}

/// A presented surface's frame drawn on the GPU (attached for readback,
/// so the test sees its pixels), as BGRA bytes like a CPU frame.
fn draw(gpu: &mut Gpu, pings: &mpsc::Receiver<()>, frame: strand_gpu::Frame) -> Vec<u8> {
    let id = frame.id;
    gpu.send(GpuRequest::Frame(frame));
    let deadline = Instant::now() + WAIT;
    loop {
        while let Some(reply) = gpu.try_recv() {
            match reply {
                GpuReply::Pixels { frame, pixels, .. } if frame == id => {
                    let mut out = Vec::new();
                    for y in 0..pixels.height {
                        out.extend_from_slice(pixels.row(y));
                    }
                    return out;
                }
                GpuReply::Failed { error, .. } => panic!("frame {id} failed: {error}"),
                _ => {}
            }
        }
        assert!(Instant::now() < deadline, "no pixels for frame {id}");
        let _ = pings.recv_timeout(Duration::from_millis(50));
    }
}

/// Pixels past the tolerance, and the worst channel difference.
fn compare_bytes(a: &[u8], b: &[u8]) -> (usize, u8) {
    assert_eq!(a.len(), b.len());
    let mut bad = 0;
    let mut worst = 0;
    for (x, y) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
        let d = x
            .iter()
            .zip(y)
            .map(|(p, q)| p.abs_diff(*q))
            .max()
            .unwrap_or(0);
        worst = worst.max(d);
        if d > GPU_TOLERANCE {
            bad += 1;
        }
    }
    (bad, worst)
}

/// A theme crossfade on a `GpuPresent` surface: each frame `paint_gpu`
/// lowers carries the snapshot of the old frame under the new one, and
/// the GPU draws it as the CPU blends it (within the GPU tolerance),
/// from the old frame to exactly the new one.
#[test]
fn a_presented_surface_crossfades_on_the_gpu_like_the_cpu() {
    let Some(opts) = device() else { return };
    const W: u32 = 640;
    const H: u32 = 330;
    let t0 = Duration::from_secs(1);
    let at = |k: u32| t0 + Duration::from_nanos(1_000_000_000 * k as u64 / 60);
    let swap = |r: &mut Renderer, t: &TokenTable| {
        let mut d = SceneDiff::new();
        d.set_tokens(t.clone(), Transition::Default);
        assert!(r.apply(d).is_empty());
        assert_eq!(r.swap_crossfades(), 1, "the swap crossfades");
    };
    let (a, b) = split_tables();

    // The CPU's crossfade, frame by frame.
    let mut cpu = renderer();
    fade_scene(&mut cpu, a.clone());
    let mut buf = Buffer::new(W, H, Scale::ONE);
    buf.paint_at(&mut cpu, S, 0, t0);
    cpu.update();
    buf.paint_at(&mut cpu, S, 0, t0);
    let old = buf.pixels.clone();
    swap(&mut cpu, &b);
    let mut want = Vec::new();
    let mut k = 1;
    while cpu.wants_frame(S) {
        buf.paint_at(&mut cpu, S, 0, at(k));
        want.push(buf.pixels.clone());
        k += 1;
        assert!(k < 120, "the CPU's crossfade never settled");
    }
    let new = want.last().cloned().expect("crossfade frames");
    assert!(want.len() > 4, "{} crossfade frames", want.len());

    // The same surface presented: its frames are lowered, not drawn.
    let mut r = renderer();
    fade_scene(&mut r, a);
    let mut first = Buffer::new(W, H, Scale::ONE);
    first.paint_at(&mut r, S, 0, t0);
    r.update();
    first.paint_at(&mut r, S, 0, t0);
    r.promote_now(S);
    let _ = r.take_backend_changes();
    r.set_backend(S, Backend::GpuPresent);
    assert_eq!(r.backend(S), Backend::GpuPresent);
    let (ping, pings) = mpsc::channel();
    let mut gpu = Gpu::spawn(
        Box::new(move || {
            let _ = ping.send(());
        }),
        opts,
    );
    gpu.send(GpuRequest::Attach {
        surface: S,
        handles: None,
        size: Size::new(W, H),
        scale: Scale::ONE,
        opaque: false,
    });
    let frame = r.paint_gpu(S, t0).expect("the switch repaints");
    let px = draw(&mut gpu, &pings, frame);
    let (bad, worst) = compare_bytes(&px, &old);
    assert!(
        bad as f64 / (W * H) as f64 <= EDGE_SHARE,
        "the presented frame before the swap: {bad} pixels past {GPU_TOLERANCE} (worst {worst})"
    );

    swap(&mut r, &b);
    let mut blended = 0;
    for (i, cpu_px) in want.iter().enumerate() {
        let frame = r
            .paint_gpu(S, at(i as u32 + 1))
            .unwrap_or_else(|| panic!("crossfade frame {}", i + 1));
        let fading = frame.ops.iter().any(|o| {
            matches!(
                o,
                strand_gpu::Op::PushLayer(strand_gpu::Layer { blend: Some(_), .. })
            )
        });
        // (perf-gpu-retain) The snapshot is the frame the GPU thread
        // shows, taken there at the first crossfade frame and kept: the
        // CPU draws and uploads none.
        if fading {
            let how = if i == 0 {
                strand_gpu::FrameSnapshot::Take
            } else {
                strand_gpu::FrameSnapshot::Keep
            };
            assert_eq!(frame.snapshot, how, "crossfade frame {}", i + 1);
            assert!(
                frame
                    .ops
                    .iter()
                    .any(|o| matches!(o, strand_gpu::Op::Snapshot { .. })),
                "crossfade frame {} draws the snapshot",
                i + 1
            );
            assert!(
                frame
                    .uploads
                    .iter()
                    .all(|u| u32::from(u.pixmap.width()) < W),
                "crossfade frame {} uploads a snapshot",
                i + 1
            );
        } else {
            assert_eq!(frame.snapshot, strand_gpu::FrameSnapshot::None);
        }
        let px = draw(&mut gpu, &pings, frame);
        let (bad, worst) = compare_bytes(&px, cpu_px);
        if bad as f64 / (W * H) as f64 > EDGE_SHARE {
            let rgba: Vec<u8> = px
                .chunks_exact(4)
                .flat_map(|p| [p[2], p[1], p[0], p[3]])
                .collect();
            write_png(&refs_dir().join("gpu_crossfade.actual.png"), W, H, &rgba);
            panic!(
                "crossfade frame {}: {bad} pixels past {GPU_TOLERANCE} of the CPU's (worst {worst})",
                i + 1
            );
        }
        let (from_old, _) = compare_bytes(&px, &old);
        let (from_new, _) = compare_bytes(&px, &new);
        if fading && from_old > 0 && from_new > 0 {
            blended += 1;
        }
    }
    assert!(
        blended > 2,
        "{blended} GPU frames between the old and the new frame"
    );
    assert!(!r.swapping(), "the crossfade ended");
    assert!(
        r.paint_gpu(S, at(want.len() as u32 + 1)).is_none(),
        "idle after"
    );
    drop(gpu);
}

/// (gpu-first review 2) A presented surface whose content size changes
/// is held for the configure at the new size, as a CPU one is, and the
/// GPU thread is asked to commit the size the manager left pending (a
/// `Commit`, once per size, when the surface changes are handed over):
/// the new size is pending surface state only a present commits.
/// 47a3cd04 drew the new layout at once into the old size (no hold, no
/// commit): the launcher's growing mode switches showed a frame or two
/// shifted by half the growth and clipped on the far side (burst
/// captures, decisions.md gpu-first). Before that, d636d051 held with
/// nothing to commit, and the hold only ran out. Needs no device (a
/// frame is lowered, not drawn).
#[test]
fn a_presented_surface_is_held_and_committed_for_its_new_size() {
    let mut b = Builder::default();
    let p = b.node(
        NodeKind::Panel,
        None,
        vec![(Prop::Font, PropValue::Font(font(13.0)))],
    );
    let t = b.node(NodeKind::Text, Some(p), vec![(Prop::Text, text("short"))]);
    let mut r = renderer();
    r.set_resize_wait(Duration::from_secs(30));
    assert!(r.apply(b.diff).is_empty());
    let size = |r: &Renderer| {
        let s = r.surface_spec(p).unwrap();
        Size::new(s.width.unwrap() as u32, s.height.unwrap() as u32)
    };
    let first = size(&r);
    r.attach_surface(S, p);
    r.configure_surface(S, first, Scale::ONE);
    let mut buf = Buffer::new(first.w, first.h, Scale::ONE);
    buf.paint(&mut r, S, 0);
    r.promote_now(S);
    r.set_backend(S, Backend::GpuPresent);
    let f = r
        .paint_gpu(S, Duration::from_millis(16))
        .expect("the first presented frame");
    assert_eq!(f.size, first);
    let commits = |r: &mut Renderer| {
        r.take_gpu_requests()
            .iter()
            .filter(|q| matches!(q, GpuRequest::Commit(s) if *s == S))
            .count()
    };
    let _ = r.take_surface_changes();
    assert_eq!(commits(&mut r), 0, "nothing pending, nothing committed");
    let mut d = SceneDiff::new();
    d.set(t, Prop::Text, text("a much longer line of text"));
    r.apply(d);
    let grown = size(&r);
    assert!(grown.w > first.w);
    assert!(r.frame_deadline(S).is_some(), "held for the configure");
    assert!(!r.wants_frame(S));
    assert!(
        r.paint_gpu(S, Duration::from_millis(32)).is_none(),
        "not drawn into the old size"
    );
    assert_eq!(commits(&mut r), 0, "not before the manager has the size");
    assert!(!r.take_surface_changes().is_empty(), "the new size");
    assert_eq!(commits(&mut r), 1, "the size is committed");
    let _ = r.take_surface_changes();
    assert_eq!(commits(&mut r), 0, "once");
    // The configure at the new size: drawn there.
    r.configure_surface(S, grown, Scale::ONE);
    assert!(r.frame_deadline(S).is_none());
    assert!(r.wants_frame(S));
    let f = r
        .paint_gpu(S, Duration::from_millis(48))
        .expect("drawn at its new size");
    assert_eq!(f.size, grown);
}

/// (gpu-first review) No adapter, or only a software one refused, is
/// final: the renderer never asks for a device again, whatever needs one
/// later (a promotion, a warm start, a pass), however long after; and
/// where the binary found no render node (`gpu_absent`) nothing is ever
/// asked for. d636d051 asked again at every animation 30 s apart, each
/// ask a new instance and its drivers mapped. Needs no device.
#[test]
fn no_adapter_is_never_asked_for_again() {
    for kind in [GpuErrorKind::NoAdapter, GpuErrorKind::Software] {
        let mut r = renderer();
        let (diff, _) = shader_scene("#00ff00");
        assert!(r.apply(diff).is_empty());
        r.attach_surface(S, r.tree().roots()[0]);
        r.set_gpu_retry(Duration::from_millis(1));
        r.warm_gpu();
        assert_eq!(r.take_backend_changes(), vec![BackendChange::Start]);
        r.deliver_gpu(GpuReply::Unavailable(strand_gpu::GpuError {
            kind,
            message: "no Vulkan adapter".into(),
        }));
        r.deliver_gpu(GpuReply::Exited);
        std::thread::sleep(Duration::from_millis(5));
        let mut buf = Buffer::new(240, 60, Scale::ONE);
        for t in 0..3 {
            buf.paint(&mut r, S, t);
        }
        r.warm_gpu();
        r.promote_now(S);
        assert!(
            r.take_gpu_requests().is_empty(),
            "{kind:?}: no pass asked for"
        );
        assert!(r.take_backend_changes().is_empty(), "{kind:?}: no start");
        assert_eq!(r.backend(S), Backend::Cpu);
        assert_eq!(r.next_wake(), None, "{kind:?}: no wake");
    }
    // No render node: nothing asked for from the start.
    let mut r = renderer();
    let (diff, _) = shader_scene("#00ff00");
    assert!(r.apply(diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    r.gpu_absent("no GPU render node (/dev/dri/renderD*)");
    r.warm_gpu();
    let mut buf = Buffer::new(240, 60, Scale::ONE);
    buf.paint(&mut r, S, 0);
    r.promote_now(S);
    assert!(r.take_gpu_requests().is_empty());
    assert!(r.take_backend_changes().is_empty());
    assert_eq!(
        r.gpu_status(),
        GpuStatus::Unavailable {
            reason: "no GPU render node (/dev/dri/renderD*)".into()
        }
    );
    // Drawn as with no GPU at all.
    let mut cpu = renderer();
    let (diff, _) = shader_scene("#00ff00");
    assert!(cpu.apply(diff).is_empty());
    cpu.attach_surface(S, cpu.tree().roots()[0]);
    cpu.deliver_gpu(GpuReply::Unavailable(strand_gpu::GpuError {
        kind: GpuErrorKind::NoAdapter,
        message: "no Vulkan adapter".into(),
    }));
    let mut want = Buffer::new(240, 60, Scale::ONE);
    want.paint(&mut cpu, S, 0);
    assert!(buf.pixels == want.pixels, "drawn as the CPU draws it");
}

/// A start that fails the same way each time it is asked (no adapter,
/// only a software one: since the gpu-first review not asked again, but a
/// host may still deliver the answer of a start already sent) is a
/// warning once; a new reason, or the same after the device was up, is
/// said again. Needs no device.
#[test]
fn an_unavailable_reason_is_warned_once() {
    let mut r = renderer();
    let unavailable = |kind, message: &str| {
        GpuReply::Unavailable(strand_gpu::GpuError {
            kind,
            message: message.into(),
        })
    };
    let none = "no Vulkan adapter";
    r.deliver_gpu(unavailable(GpuErrorKind::NoAdapter, none));
    r.deliver_gpu(GpuReply::Exited);
    for _ in 0..3 {
        r.deliver_gpu(unavailable(GpuErrorKind::NoAdapter, none));
        r.deliver_gpu(GpuReply::Exited);
    }
    assert_eq!(r.gpu_warnings(), 1, "the same reason, once");
    assert_eq!(
        r.gpu_status(),
        GpuStatus::Unavailable {
            reason: none.into()
        }
    );
    r.deliver_gpu(unavailable(
        GpuErrorKind::Software,
        "only a software adapter",
    ));
    assert_eq!(r.gpu_warnings(), 2, "a new reason");
    r.deliver_gpu(GpuReply::Ready(AdapterInfo::default()));
    r.deliver_gpu(unavailable(
        GpuErrorKind::Software,
        "only a software adapter",
    ));
    assert_eq!(r.gpu_warnings(), 3, "again after the device was up");
}

/// A colour uniform reaches the shader in the space its output is read
/// in: a pass that returns `u_tint` paints the same pixels as a box
/// with that `bg`, mid-tones and translucency included.
#[test]
fn a_colour_uniform_paints_like_the_same_bg() {
    let Some(opts) = device() else { return };
    for tint in ["#7f3f1f", "#4080c080"] {
        let mut r = renderer();
        let (diff, node) = shader_scene(tint);
        assert!(r.apply(diff).is_empty());
        r.attach_surface(S, r.tree().roots()[0]);
        let mut buf = Buffer::new(240, 60, Scale::ONE);
        let mut host = Host::new(opts);
        buf.paint(&mut r, S, 0);
        host.send(&mut r);
        host.until(&mut r, |m| matches!(m, GpuReply::PassPixels { .. }));
        buf.paint(&mut r, S, 1);
        let shaded = buf.px(20, 20);
        // The same box painted with `bg` by the CPU.
        let mut cpu = renderer();
        let (mut diff, _) = shader_scene(tint);
        diff.set(node, Prop::Bg, color(tint));
        diff.set(node, Prop::Uniforms, PropValue::Uniforms(vec![]));
        assert!(cpu.apply(diff).is_empty());
        cpu.attach_surface(S, cpu.tree().roots()[0]);
        let mut want = Buffer::new(240, 60, Scale::ONE);
        want.paint(&mut cpu, S, 0);
        let bg = want.px(20, 20);
        assert_ne!(bg, want.px(100, 20), "{tint}: the box shows");
        let close = shaded.iter().zip(bg).all(|(a, b)| a.abs_diff(b) <= 1);
        assert!(close, "{tint}: the pass {shaded:?}, the bg {bg:?}");
    }
}

/// A clocked shader (`strand.time`) on a surface the GPU draws runs in
/// its GPU frames, so no pass pixels come back to change its record:
/// its box is still each frame's damage. Without that a promoted
/// surface showing only an animated shader painted nothing (a readback
/// surface sent no frames) and, its frames empty, went back to the CPU
/// as idle 500 ms after every promotion. Needs no device.
#[test]
fn a_clocked_shader_on_a_gpu_surface_damages_every_frame() {
    let clocked = Arc::new(ShaderCode {
        path: "clock.wgsl".into(),
        wgsl: "@fragment\n\
               fn main(v: StrandVertex) -> @location(0) vec4<f32> {\n\
                   return vec4<f32>(fract(strand.time), 0.0, 0.0, 1.0);\n\
               }\n"
        .into(),
        uniforms: ShaderCode::packed(vec![]),
    });
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    b.node(
        NodeKind::Shader,
        Some(root),
        vec![
            (Prop::X, num(10.0)),
            (Prop::Y, num(10.0)),
            (Prop::Width, num(40.0)),
            (Prop::Height, num(20.0)),
            (Prop::Shader, PropValue::Shader(clocked)),
        ],
    );
    let t0 = Duration::from_secs(1);
    let ms = |n: u64| t0 + Duration::from_millis(n);
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(240, 60, Scale::ONE);
    assert!(!buf.paint_at(&mut r, S, 0, t0).is_empty());
    r.promote_now(S);
    r.set_backend(S, Backend::GpuReadback);
    assert_eq!(r.backend(S), Backend::GpuReadback);
    assert!(!buf.paint_at(&mut r, S, 1, ms(16)).is_empty(), "the switch");
    assert!(
        r.take_gpu_requests()
            .iter()
            .any(|q| matches!(q, GpuRequest::Frame(_))),
        "the first GPU frame"
    );
    // Only the clock moves (and the frame sent is still in flight): the
    // shader's box is damaged, so the frame is painted (by the CPU,
    // until the GPU's pixels come) rather than skipped.
    let d = buf.paint_at(&mut r, S, 1, ms(33));
    assert!(!d.is_empty(), "a frame with only the clock moving");
}

/// (m4-integration-w2) A presented frame in flight is not idleness: a
/// promoted surface whose frame the GPU thread has not presented yet
/// (lavapipe's first frame compiles its pipelines, hundreds of ms on a
/// loaded machine) stays on the GPU past its settle window (here 300 ms),
/// and goes back once that frame is shown. Without it
/// `strand/tests/gpu_idle.rs` saw an animating panel released at its
/// first frame. Needs no device; waits in real time (the renderer's
/// clock is `Instant::now`).
#[test]
fn a_frame_in_flight_keeps_its_surface_promoted() {
    let mut b = Builder::default();
    b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(80.0)),
            (Prop::Height, num(40.0)),
            (Prop::Bg, color("#ff0000")),
        ],
    );
    let mut r = renderer();
    let settle = Duration::from_millis(300);
    r.set_gpu_settle(settle);
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(80, 40, Scale::ONE);
    assert!(
        !buf.paint_at(&mut r, S, 0, Duration::from_secs(1))
            .is_empty()
    );
    r.promote_now(S);
    r.set_backend(S, Backend::GpuPresent);
    let _ = r.take_backend_changes();
    assert!(
        r.paint_gpu(S, Duration::from_secs(1)).is_some(),
        "the switch's frame"
    );
    let idle = settle + Duration::from_millis(150);
    std::thread::sleep(idle);
    r.update();
    assert_eq!(
        r.backend(S),
        Backend::GpuPresent,
        "demoted with a frame in flight"
    );
    assert!(
        !r.take_backend_changes()
            .iter()
            .any(|c| matches!(c, BackendChange::Demote(_))),
        "a demotion asked for with a frame in flight"
    );
    // Presented: a still frame (nothing animates on it), shown past the
    // settle window, so the surface goes back now. An animated frame's
    // `Presented` would start the window over (`promote.rs`,
    // `a_shown_frame_restarts_the_settle_window`).
    r.deliver_gpu(GpuReply::Presented {
        surface: S,
        frame: 0,
        at: Instant::now(),
        waited: Duration::ZERO,
        ran: vec![],
    });
    r.update();
    assert!(
        r.take_backend_changes()
            .iter()
            .any(|c| matches!(c, BackendChange::Demote(id) if *id == S)),
        "settled once its frame was shown"
    );
}

/// (M5, m5-tools review) `strand report`'s frame time of a promoted
/// surface includes the GPU's work: a `GpuPresent` frame runs from its
/// `paint` to its `Presented`'s `at` (its submission; round 3,
/// `strand-gpu`'s `a_presented_frame_is_answered_before_the_gpu_ran_it`)
/// plus the GPU's run of it (`ran`), a readback frame from its `paint`
/// to its `Pixels`, and neither counts before its reply. Needs no
/// device; the GPU thread's 40 ms is a sleep, its 30 ms run a number.
#[test]
fn a_gpu_frame_is_timed_to_the_gpu_threads_reply() {
    let gpu_work = Duration::from_millis(40);
    let mut b = Builder::default();
    b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(80.0)),
            (Prop::Height, num(40.0)),
            (Prop::Bg, color("#ff0000")),
        ],
    );
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(80, 40, Scale::ONE);
    assert!(
        !buf.paint_at(&mut r, S, 0, Duration::from_secs(1))
            .is_empty()
    );
    let cpu = r.frame_summary(S).unwrap();
    assert_eq!(cpu.drawn, 1);
    assert!(cpu.p99 < gpu_work, "the CPU frame: {:?}", cpu.p99);

    // Presented: timed at the GPU thread's `Presented`.
    r.promote_now(S);
    r.set_backend(S, Backend::GpuPresent);
    let _ = r.take_backend_changes();
    let id = r.paint_gpu(S, Duration::from_secs(1)).unwrap().id;
    assert_eq!(
        r.frame_summary(S).unwrap().drawn,
        1,
        "counted before its reply"
    );
    std::thread::sleep(gpu_work);
    let ran = Duration::from_millis(30);
    r.deliver_gpu(GpuReply::Presented {
        surface: S,
        frame: id,
        at: Instant::now(),
        waited: Duration::ZERO,
        ran: vec![(id, ran)],
    });
    let f = r.frame_summary(S).unwrap();
    assert_eq!(f.drawn, 2);
    assert!(
        f.p99 >= gpu_work + ran,
        "the GPU's work left out: {:?}",
        f.p99
    );

    // Readback: timed at its `Pixels`.
    r.set_backend(S, Backend::Cpu);
    r.promote_now(S);
    r.set_backend(S, Backend::GpuReadback);
    let _ = r.take_backend_changes();
    buf.paint_at(&mut r, S, 0, Duration::from_secs(2));
    let id = r
        .take_gpu_requests()
        .into_iter()
        .find_map(|q| match q {
            GpuRequest::Frame(f) => Some(f.id),
            _ => None,
        })
        .expect("a readback frame sent");
    assert_eq!(
        r.frame_summary(S).unwrap().drawn,
        2,
        "counted before its reply"
    );
    std::thread::sleep(gpu_work);
    r.deliver_gpu(GpuReply::Pixels {
        surface: S,
        frame: id,
        pixels: strand_gpu::Readback {
            width: 80,
            height: 40,
            stride: 320,
            bytes: vec![0; 320 * 40],
        },
        waited: Duration::ZERO,
    });
    let f = r.frame_summary(S).unwrap();
    assert_eq!(f.drawn, 3);
    assert!(
        f.p50 >= gpu_work,
        "the readback's GPU work left out: {:?}",
        f.p50
    );
}

/// A promoted surface with one frame painted on the CPU, ready for
/// `paint_gpu`.
fn presenting() -> (Renderer, Buffer) {
    let mut b = Builder::default();
    b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(80.0)),
            (Prop::Height, num(40.0)),
            (Prop::Bg, color("#ff0000")),
        ],
    );
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(80, 40, Scale::ONE);
    assert!(
        !buf.paint_at(&mut r, S, 0, Duration::from_secs(1))
            .is_empty()
    );
    r.promote_now(S);
    r.set_backend(S, Backend::GpuPresent);
    let _ = r.take_backend_changes();
    (r, buf)
}

/// (M5, m5-tools review rounds 2 and 3) A presented frame's time leaves
/// out the GPU thread's `waited` (its queue behind other surfaces' work
/// and its block on the swapchain, the compositor's FIFO pacing), and
/// only that: of a 60 ms span with 30 ms waited, about 30 ms is left,
/// neither zero nor the span. Needs no device: the span is a sleep.
#[test]
fn a_presented_frames_time_leaves_out_the_swapchain_wait() {
    let span = Duration::from_millis(60);
    let waited = Duration::from_millis(30);
    let (mut r, _buf) = presenting();
    let id = r.paint_gpu(S, Duration::from_secs(1)).unwrap().id;
    std::thread::sleep(span);
    r.deliver_gpu(GpuReply::Presented {
        surface: S,
        frame: id,
        at: Instant::now(),
        waited,
        ran: vec![(id, Duration::ZERO)],
    });
    let f = r.frame_summary(S).unwrap();
    assert_eq!(f.drawn, 2);
    assert!(
        f.p99 >= span - waited && f.p99 < span - waited / 2,
        "the span less `waited`: {:?}",
        f.p99
    );
}

/// (M5, m5-tools review round 3) The GPU thread answers a presented
/// frame before the GPU ran it, so its run comes with a later frame's
/// `Presented`: a frame counts only then, as its time before the GPU
/// plus the run; a frame whose run never comes (both of the surface's
/// timers were busy) is skipped once a later frame's run comes.
#[test]
fn a_presented_frame_counts_when_its_gpu_run_comes() {
    let (mut r, _buf) = presenting();
    r.log_gpu_frames(true);
    let present = |r: &mut Renderer, ran: Vec<(u64, Duration)>| {
        r.invalidate(S);
        let id = r.paint_gpu(S, Duration::from_secs(1)).unwrap().id;
        let at = Instant::now();
        r.deliver_gpu(GpuReply::Presented {
            surface: S,
            frame: id,
            at,
            waited: Duration::ZERO,
            ran,
        });
        id
    };
    let _first = present(&mut r, vec![]);
    assert_eq!(r.frame_summary(S).unwrap().drawn, 1, "no run yet");
    let second = present(&mut r, vec![]);
    let run = Duration::from_millis(500);
    let _third = present(&mut r, vec![(second, run)]);
    let f = r.frame_summary(S).unwrap();
    assert_eq!(f.drawn, 2, "the second, and not the first");
    assert!(f.p99 >= run && f.p99 < run * 2, "its run: {:?}", f.p99);
    // (gpu-first) The `damage` log's `gpu-frame` line: the second
    // frame, its GPU run apart from its CPU part.
    let timed = r.take_gpu_frame_times();
    assert_eq!(timed.len(), 1, "{timed:?}");
    assert_eq!((timed[0].0, timed[0].2), (S, run));
    assert!(r.take_gpu_frame_times().is_empty(), "taken once");
}

/// (m4-integration-w2) A surface the GPU thread presents paints its pose
/// into its frames (architecture.md, "Surface hand-off"): with
/// compositor poses on, a root's static `opacity: 0.5` is delegated while
/// the CPU draws, nothing is delegated once the surface is `GpuPresent`
/// (its lowered frame carries the opacity group instead), and delegation
/// comes back when the CPU draws it again.
#[test]
fn a_presented_surface_paints_its_pose() {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(80.0)),
            (Prop::Height, num(40.0)),
            (Prop::Bg, color("#ff0000")),
            (Prop::Anchor, PropValue::Keyword("top_right".into())),
            (Prop::Opacity, num(0.5)),
        ],
    );
    let mut r = renderer();
    r.set_compositor_poses(true);
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, root);
    r.configure_surface(S, Size::new(80, 40), Scale::ONE);
    let mut buf = Buffer::new(80, 40, Scale::ONE);
    let t0 = Duration::from_secs(1);
    buf.paint_at(&mut r, S, 0, t0);
    let pose = r.surface_pose(S).expect("delegated while the CPU draws");
    assert!((pose.opacity - 0.5).abs() < 1e-3, "{pose:?}");
    let opacity_group = |ops: &[strand_gpu::Op]| {
        ops.iter().any(|op| {
            matches!(op, strand_gpu::Op::PushLayer(l)
                if l.opacity.is_some_and(|o| (o - 0.5).abs() < 1e-3))
        })
    };

    r.promote_now(S);
    r.set_backend(S, Backend::GpuPresent);
    assert_eq!(r.backend(S), Backend::GpuPresent);
    let frame = r
        .paint_gpu(S, t0 + Duration::from_millis(16))
        .expect("the switch repaints");
    assert_eq!(
        r.surface_pose(S),
        None,
        "a presented surface delegates nothing"
    );
    assert!(
        opacity_group(&frame.ops),
        "the opacity is painted into the GPU's frame"
    );

    r.set_backend(S, Backend::Cpu);
    buf.paint_at(&mut r, S, 0, t0 + Duration::from_millis(33));
    let pose = r.surface_pose(S).expect("delegated again on the CPU");
    assert!((pose.opacity - 0.5).abs() < 1e-3, "{pose:?}");
}

/// A device that answered `Ready` (no GPU thread runs here: replies are
/// delivered by hand).
fn up_without_a_device(r: &mut Renderer) {
    r.deliver_gpu(GpuReply::Ready(AdapterInfo {
        name: "test".into(),
        driver: "none".into(),
        software: true,
    }));
}

/// Sleeps past `until`.
fn sleep_past(until: Instant) {
    std::thread::sleep(until.saturating_duration_since(Instant::now()) + Duration::from_millis(2));
}

/// (m4-audit) Why a GPU stall cannot freeze a lock (features.md, the
/// lock exit): a lock surface is never handed to the GPU thread, so a
/// promoted one is read back, and a readback frame the GPU never answers
/// holds the surface at most [`strand_render::GPU_WAIT`]; the CPU then
/// draws it, and while that frame stays unanswered no later frame waits
/// at all. Needs no device: the GPU is "up" and never replies.
#[test]
fn a_readback_frame_the_gpu_never_answers_holds_at_most_gpu_wait() {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#ff0000"))]);
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, root);
    let mut buf = Buffer::new(240, 60, Scale::ONE);
    r.promote_now(S);
    up_without_a_device(&mut r);
    r.deliver_gpu(GpuReply::Attached {
        surface: S,
        mode: strand_gpu::GpuMode::Readback,
    });
    assert_eq!(r.backend(S), Backend::GpuReadback);
    let _ = r.take_backend_changes();
    // The first frame: no pixels yet, so the CPU draws it, and the GPU is
    // sent the frame. It never answers.
    buf.paint(&mut r, S, 0);
    let sent = Instant::now();
    assert!(
        r.take_gpu_requests()
            .iter()
            .any(|q| matches!(q, GpuRequest::Frame(_))),
        "the frame goes to the GPU"
    );
    assert_eq!(buf.px(100, 20), [0, 0, 255, 255], "red, drawn by the CPU");
    // The scene changes: the next frame holds for the GPU's pixels, but
    // never past GPU_WAIT from the request.
    let mut d = SceneDiff::new();
    d.set(root, Prop::Bg, color("#0000ff"));
    assert!(r.apply(d).is_empty());
    r.update();
    let until = r.frame_deadline(S).expect("held for the GPU's frame");
    assert!(
        until <= sent + strand_render::GPU_WAIT,
        "held {:?} past the request, more than GPU_WAIT",
        until.saturating_duration_since(sent)
    );
    assert!(!r.wants_frame(S), "held while the GPU may still answer");
    sleep_past(until);
    r.update();
    assert_eq!(r.frame_deadline(S), None, "the hold ends at GPU_WAIT");
    assert!(r.wants_frame(S), "the frame is wanted once the hold ends");
    assert!(
        !buf.paint(&mut r, S, 1).is_empty(),
        "painted without the GPU"
    );
    assert_eq!(buf.px(100, 20), [255, 0, 0, 255], "blue, drawn by the CPU");
    assert!(
        r.take_gpu_requests().is_empty(),
        "nothing more is queued behind the unanswered frame"
    );
    // While that frame stays unanswered, later frames do not wait at all.
    let mut d = SceneDiff::new();
    d.set(root, Prop::Bg, color("#00ff00"));
    assert!(r.apply(d).is_empty());
    r.update();
    assert_eq!(r.frame_deadline(S), None, "no second hold");
    assert!(r.wants_frame(S));
    buf.paint(&mut r, S, 1);
    assert_eq!(buf.px(100, 20), [0, 255, 0, 255], "green, at once");
    assert_eq!(r.backend(S), Backend::GpuReadback, "still promoted");
}

/// (m4-audit) The same bound for a `shader` node's pass on a CPU
/// surface (a lock's shader is one): a pass the GPU never answers holds
/// the frame at most [`strand_render::GPU_WAIT`], then the frame paints
/// with the pass's last pixels, and the surface's later frames do not
/// wait for it. Needs no device.
#[test]
fn a_pass_the_gpu_never_answers_holds_at_most_gpu_wait() {
    let mut r = renderer();
    let (diff, node) = shader_scene("#ff0000");
    assert!(r.apply(diff).is_empty());
    let root = r.tree().roots()[0];
    r.attach_surface(S, root);
    let mut buf = Buffer::new(240, 60, Scale::ONE);
    buf.paint(&mut r, S, 0);
    up_without_a_device(&mut r);
    // The first pass answers: a 40×20 block of one colour.
    let first = r
        .take_gpu_requests()
        .into_iter()
        .find_map(|q| match q {
            GpuRequest::Pass(p) => Some(p),
            _ => None,
        })
        .expect("the node's pass is asked for");
    r.deliver_gpu(GpuReply::PassPixels {
        key: first.key,
        frame: first.id,
        pixels: strand_gpu::Readback {
            width: 40,
            height: 20,
            stride: 160,
            bytes: [0u8, 0, 255, 255].repeat(40 * 20),
        },
    });
    buf.paint(&mut r, S, 1);
    let shown = buf.px(20, 20);
    assert_ne!(shown, buf.px(100, 20), "the pass's pixels are shown");
    // A uniform change asks for a new pass, which never answers; the
    // background changes in the same frame. (A uniform change alone
    // changes nothing on screen until its pixels come, so it wants no
    // frame of its own.)
    let mut d = SceneDiff::new();
    d.set(
        node,
        Prop::Uniforms,
        PropValue::Uniforms(vec![("u_tint".into(), color("#0000ff"))]),
    );
    d.set(root, Prop::Bg, color("#45475a"));
    assert!(r.apply(d).is_empty());
    let old_bg = buf.px(100, 20);
    r.update();
    let sent = Instant::now();
    assert!(
        r.take_gpu_requests()
            .iter()
            .any(|q| matches!(q, GpuRequest::Pass(_))),
        "a new pass is asked for"
    );
    let until = r.frame_deadline(S).expect("held for the pass");
    assert!(
        until <= sent + strand_render::GPU_WAIT,
        "held {:?} past the request, more than GPU_WAIT",
        until.saturating_duration_since(sent)
    );
    assert!(!r.wants_frame(S));
    sleep_past(until);
    r.update();
    assert_eq!(r.frame_deadline(S), None, "the hold ends at GPU_WAIT");
    assert!(r.wants_frame(S), "the frame is wanted once the hold ends");
    assert!(
        !buf.paint(&mut r, S, 1).is_empty(),
        "painted without the pass"
    );
    assert_ne!(buf.px(100, 20), old_bg, "the new background is shown");
    assert_eq!(buf.px(20, 20), shown, "the pass's last pixels stay");
    // Something else on the surface changes: no hold while the pass is
    // still unanswered.
    let mut d = SceneDiff::new();
    d.set(root, Prop::Bg, color("#313244"));
    assert!(r.apply(d).is_empty());
    r.update();
    assert_eq!(r.frame_deadline(S), None, "no second hold");
    let before = buf.px(100, 20);
    buf.paint(&mut r, S, 1);
    assert_ne!(buf.px(100, 20), before, "the new background, at once");
    assert!(
        r.take_gpu_requests().is_empty(),
        "the pass is not asked again"
    );
}

/// (m4-audit) A frame of a promoted surface that hangs (or panics) is
/// answered `Failed` with no pass key, and the device goes with it. The
/// `shader` node whose pass ran inline in that frame is recorded as
/// failed too: demoted, the surface would otherwise ask the next device
/// for the same pass as one of its own, 30 s later, and hang (and leak)
/// that device as well. Not asked again while its base stays; asked
/// once that changes. Needs no device: replies are delivered by hand,
/// and the device is left up so that only the record stops the ask.
#[test]
fn a_hung_frame_records_its_inline_passes_as_failed() {
    let mut r = renderer();
    let (diff, node) = shader_scene("#ff0000");
    assert!(r.apply(diff).is_empty());
    let root = r.tree().roots()[0];
    r.attach_surface(S, root);
    let mut buf = Buffer::new(240, 60, Scale::ONE);
    r.promote_now(S);
    up_without_a_device(&mut r);
    r.deliver_gpu(GpuReply::Attached {
        surface: S,
        mode: strand_gpu::GpuMode::Readback,
    });
    let _ = r.take_backend_changes();
    buf.paint(&mut r, S, 0);
    let reqs = r.take_gpu_requests();
    let frame = reqs
        .iter()
        .find_map(|q| match q {
            GpuRequest::Frame(f) => Some(f),
            _ => None,
        })
        .expect("the frame goes to the GPU");
    assert!(
        frame
            .ops
            .iter()
            .any(|op| matches!(op, strand_gpu::Op::Pass { .. })),
        "the shader runs inline in the GPU's frame"
    );
    assert!(
        !reqs.iter().any(|q| matches!(q, GpuRequest::Pass(_))),
        "and is not asked for on its own"
    );
    // The frame ran past HUNG_AFTER.
    r.deliver_gpu(GpuReply::Failed {
        surface: Some(S),
        key: None,
        error: strand_gpu::GpuError {
            kind: GpuErrorKind::Lost,
            message: "the GPU ran past 10s on one submission".into(),
        },
    });
    assert!(
        r.take_backend_changes()
            .iter()
            .any(|c| matches!(c, BackendChange::Demote(id) if *id == S)),
        "demoted"
    );
    assert_eq!(r.backend(S), Backend::Cpu);
    buf.paint(&mut r, S, 1);
    assert!(
        !r.take_gpu_requests()
            .iter()
            .any(|q| matches!(q, GpuRequest::Pass(_))),
        "the pass that hung is not asked for again"
    );
    // (m4-audit round 5) Its uniforms change: still not asked, since the
    // file lost a device. A reload that changes its text is new code.
    let mut d = SceneDiff::new();
    d.set(
        node,
        Prop::Uniforms,
        PropValue::Uniforms(vec![("u_tint".into(), color("#0000ff"))]),
    );
    assert!(r.apply(d).is_empty());
    r.update();
    buf.paint(&mut r, S, 2);
    assert!(
        !r.take_gpu_requests()
            .iter()
            .any(|q| matches!(q, GpuRequest::Pass(_))),
        "the file that hung is not asked for with new uniforms"
    );
    let mut d = SceneDiff::new();
    d.set(node, Prop::Shader, PropValue::Shader(edited_tint_code()));
    assert!(r.apply(d).is_empty());
    r.update();
    buf.paint(&mut r, S, 0);
    assert!(
        r.take_gpu_requests()
            .iter()
            .any(|q| matches!(q, GpuRequest::Pass(_))),
        "an edited file is asked for"
    );
}

/// [`tint_code`] after an edit (a comment added): new code.
fn edited_tint_code() -> Arc<ShaderCode> {
    let mut c = (*tint_code()).clone();
    c.wgsl.push_str("// edited\n");
    Arc::new(c)
}

/// (m4-audit) A `shader` node's pass that loses the device (it ran past
/// `HUNG_AFTER`: the GPU thread leaks the hung device with its spinning
/// queue) is answered `Failed` (kind `Lost`) with its key. Its file is
/// then never asked of a new device in this process: not with new
/// uniforms (a bound value changing), not at a new size, not on a new
/// node (a popup closed and opened again), each of which used to leak
/// one more device per retry. Edited, it is new code and asked for.
/// Needs no device: replies are delivered by hand, and the device is
/// left up so that only the record stops the ask.
#[test]
fn a_shader_file_that_hung_a_pass_is_never_run_again() {
    let mut r = renderer();
    let (diff, node) = shader_scene("#ff0000");
    assert!(r.apply(diff).is_empty());
    let root = r.tree().roots()[0];
    r.attach_surface(S, root);
    let mut buf = Buffer::new(240, 60, Scale::ONE);
    up_without_a_device(&mut r);
    buf.paint(&mut r, S, 0);
    let pass = r
        .take_gpu_requests()
        .into_iter()
        .find_map(|q| match q {
            GpuRequest::Pass(p) => Some(p),
            _ => None,
        })
        .expect("the node's pass is asked for");
    r.deliver_gpu(GpuReply::Failed {
        surface: None,
        key: Some(pass.key),
        error: strand_gpu::GpuError {
            kind: GpuErrorKind::Lost,
            message: "the GPU ran past 10s on one submission".into(),
        },
    });
    // The device comes back (30 s later, in a run).
    up_without_a_device(&mut r);
    // Painted whole each time (buffer age 0).
    let asked = |r: &mut Renderer, buf: &mut Buffer| {
        r.update();
        buf.paint(r, S, 0);
        r.take_gpu_requests()
            .iter()
            .any(|q| matches!(q, GpuRequest::Pass(_)))
    };
    let mut d = SceneDiff::new();
    d.set(
        node,
        Prop::Uniforms,
        PropValue::Uniforms(vec![("u_tint".into(), color("#0000ff"))]),
    );
    assert!(r.apply(d).is_empty());
    assert!(!asked(&mut r, &mut buf), "new uniforms");
    let mut d = SceneDiff::new();
    d.set(node, Prop::Width, num(60.0));
    assert!(r.apply(d).is_empty());
    assert!(!asked(&mut r, &mut buf), "a new size");
    // The node goes and the same file comes back on a new one.
    let mut d = SceneDiff::new();
    d.remove(node);
    assert!(r.apply(d).is_empty());
    let _ = asked(&mut r, &mut buf);
    let again = NodeId::new(100, 0);
    let mut d = SceneDiff::new();
    d.create(again, NodeKind::Shader, Some(root), u32::MAX);
    for (p, v) in [
        (Prop::X, num(10.0)),
        (Prop::Y, num(10.0)),
        (Prop::Width, num(40.0)),
        (Prop::Height, num(20.0)),
        (Prop::Shader, PropValue::Shader(tint_code())),
        (
            Prop::Uniforms,
            PropValue::Uniforms(vec![("u_tint".into(), color("#ff0000"))]),
        ),
    ] {
        d.set(again, p, v);
    }
    assert!(r.apply(d).is_empty());
    assert!(!asked(&mut r, &mut buf), "a new node");
    // Edited: new code, asked for.
    let mut d = SceneDiff::new();
    d.set(again, Prop::Shader, PropValue::Shader(edited_tint_code()));
    assert!(r.apply(d).is_empty());
    assert!(asked(&mut r, &mut buf), "an edited file is asked for");
}

/// A bar holding a `shader` node (returned), a bloomed box (a bundled
/// effect with a CPU version: a glow) and text.
fn off_scene() -> (SceneDiff, NodeId) {
    let mut b = Builder::default();
    let (root, shader) = shader_bar(&mut b, "#ff0000");
    b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(70.0)),
            (Prop::Y, num(15.0)),
            (Prop::Width, num(40.0)),
            (Prop::Height, num(30.0)),
            (Prop::Bg, color("#f38ba8")),
            (
                Prop::Filter,
                PropValue::Call {
                    name: "bloom".into(),
                    args: vec![num(6.0)],
                },
            ),
        ],
    );
    b.node(
        NodeKind::Text,
        Some(root),
        vec![
            (Prop::X, num(130.0)),
            (Prop::Y, num(20.0)),
            (Prop::Text, text("Strand")),
            (Prop::Font, PropValue::Font(font(14.0))),
            (Prop::Color, color("#cdd6f4")),
        ],
    );
    (b.diff, shader)
}

/// (m4-close-gpucap) The third lost device in a process turns the GPU
/// off until strand restarts, not the first or second: the status says
/// so (`GPU_OFF`, told by the host once, `run/gpu.rs`), no device is
/// asked for again (no pass, no promotion), a `shader` node draws
/// nothing and everything else is drawn as the CPU draws it with no GPU
/// at all, the bundled bloom's glow included (ref `gpu_off.png`). Needs
/// no device: replies are delivered by hand.
///
/// The retry wait is zero here (`set_gpu_retry`), so the whole
/// retry-and-lose cycle runs at renderer level: after the first and
/// second loss the device is asked for again at once (a promotion),
/// comes up and is lost again. With `RETRY_AFTER`'s 30 s the checks
/// after the third loss would pass during the wait without any cap;
/// with no wait only the cap keeps the device off.
#[test]
fn the_gpu_is_off_after_three_lost_devices() {
    use strand_render::promote::{GPU_OFF, LOST_CAP};
    let mut r = renderer();
    r.set_gpu_retry(Duration::ZERO);
    let (diff, shader) = off_scene();
    assert!(r.apply(diff.clone()).is_empty());
    let root = r.tree().roots()[0];
    r.attach_surface(S, root);
    let mut buf = Buffer::new(240, 60, Scale::ONE);
    buf.paint(&mut r, S, 0);
    assert!(
        r.take_gpu_requests()
            .iter()
            .any(|q| matches!(q, GpuRequest::Pass(_))),
        "the shader asks for a device"
    );
    let promoted = |r: &mut Renderer| {
        r.take_backend_changes()
            .iter()
            .any(|c| matches!(c, BackendChange::Promote(_)))
    };
    for n in 1..=LOST_CAP {
        if n > 1 {
            // Retried at once: the device is asked for again.
            r.promote_now(S);
            assert!(promoted(&mut r), "asked for again after loss {}", n - 1);
            assert_eq!(r.gpu_status(), GpuStatus::Starting);
        }
        up_without_a_device(&mut r);
        r.deliver_gpu(GpuReply::Lost(strand_gpu::GpuError {
            kind: GpuErrorKind::Lost,
            message: format!("the GPU device was lost (test {n})"),
        }));
        r.deliver_gpu(GpuReply::Exited);
        if n < LOST_CAP {
            assert!(!r.gpu_off(), "not off after loss {n}");
            assert_eq!(
                r.gpu_status(),
                GpuStatus::Unavailable {
                    reason: format!("the GPU device was lost (test {n})")
                }
            );
        }
    }
    assert!(r.gpu_off(), "off at loss {LOST_CAP}");
    assert_eq!(
        r.gpu_status(),
        GpuStatus::Unavailable {
            reason: GPU_OFF.into()
        }
    );
    // A change that wants the shader's pass again, painted whole: no
    // device is asked for.
    let mut d = SceneDiff::new();
    d.set(
        shader,
        Prop::Uniforms,
        PropValue::Uniforms(vec![("u_tint".into(), color("#0000ff"))]),
    );
    assert!(r.apply(d).is_empty());
    r.update();
    buf.paint(&mut r, S, 0);
    assert!(r.take_gpu_requests().is_empty(), "no device asked for");
    r.promote_now(S);
    assert!(!promoted(&mut r), "never promoted");
    assert_eq!(r.backend(S), Backend::Cpu);
    // Drawn as with no GPU at all.
    let mut cpu = renderer();
    assert!(cpu.apply(diff).is_empty());
    let mut d = SceneDiff::new();
    d.set(
        shader,
        Prop::Uniforms,
        PropValue::Uniforms(vec![("u_tint".into(), color("#0000ff"))]),
    );
    assert!(cpu.apply(d).is_empty());
    cpu.attach_surface(S, cpu.tree().roots()[0]);
    cpu.deliver_gpu(GpuReply::Unavailable(strand_gpu::GpuError {
        kind: GpuErrorKind::NoAdapter,
        message: "no Vulkan adapter".into(),
    }));
    let mut want = Buffer::new(240, 60, Scale::ONE);
    want.paint(&mut cpu, S, 0);
    assert!(buf.pixels == want.pixels, "drawn as the CPU draws it");
    assert_eq!(buf.px(20, 20), buf.px(5, 5), "the shader draws nothing");
    assert_ne!(buf.px(90, 30), buf.px(5, 5), "the bloomed box is drawn");
    assert_matches_ref("gpu_off", &buf, 0);
}

/// (gradient-cpu-gpu) Gradients between colours far apart in hue (red
/// to blue across, green to magenta down), on boxes whose edges lie on
/// whole pixels: the GPU's frame matches the CPU's within the tolerance
/// at every pixel. Both draw design.md's OKLab ramp; the GPU interpolates
/// per channel in sRGB between the stops it is given, and with 8 samples
/// a segment its ramp left the OKLab one by up to 29 levels between them
/// (round 3's probe: 750 of 16000 pixels past 6).
#[test]
fn a_gradient_far_apart_in_hue_is_drawn_by_the_gpu_like_the_cpu() {
    let Some(opts) = device() else { return };
    let lin = |angle: f32, a: &str, b: &str| {
        PropValue::Paint(Paint::Linear {
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
        })
    };
    let scene = || {
        let mut b = Builder::default();
        let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#000000"))]);
        for (x, w, h, paint) in [
            (8.0, 200.0, 20.0, lin(90.0, "#ff0000", "#0000ff")),
            (216.0, 12.0, 52.0, lin(0.0, "#00ff00", "#ff00ff")),
        ] {
            b.node(
                NodeKind::Box,
                Some(root),
                vec![
                    (Prop::X, num(x)),
                    (Prop::Y, num(4.0)),
                    (Prop::Width, num(w)),
                    (Prop::Height, num(h)),
                    (Prop::Bg, paint),
                ],
            );
        }
        b.diff
    };
    let mut cpu = renderer();
    assert!(cpu.apply(scene()).is_empty());
    cpu.attach_surface(S, cpu.tree().roots()[0]);
    let mut want = Buffer::new(240, 60, Scale::ONE);
    want.paint(&mut cpu, S, 0);

    let mut r = renderer();
    assert!(r.apply(scene()).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(240, 60, Scale::ONE);
    buf.paint(&mut r, S, 0);
    let mut host = Host::new(opts);
    r.promote_now(S);
    host.send(&mut r);
    host.until(&mut r, |m| matches!(m, GpuReply::Attached { .. }));
    buf.pixels.fill(0);
    buf.paint(&mut r, S, 0);
    host.send(&mut r);
    host.until(&mut r, |m| matches!(m, GpuReply::Pixels { .. }));
    buf.pixels.fill(0);
    let copied = r.gpu_frames_copied();
    buf.paint(&mut r, S, 0);
    assert_eq!(r.gpu_frames_copied(), copied + 1, "drawn by the GPU");
    let (bad, worst) = compare(&buf, &want);
    eprintln!("GPU vs CPU: {bad} pixels past {GPU_TOLERANCE}, worst {worst}");
    assert_eq!(
        bad, 0,
        "{bad} pixels differ by more than {GPU_TOLERANCE} (worst {worst})"
    );
}

// ---------------------------------------------------------------------------
// (perf-gpu-retain) Backdrops drawn in a GPU frame

/// Stripes behind, 10 px each, across a `w × h` bar, and a box over
/// them at `at` (x, y, w, h) with `backdrop` (and `radius`), its other
/// props `extra`; then boxes with the props `over`, placed absolutely.
fn backdrop_scene(
    (w, h): (u32, u32),
    at: (f32, f32, f32, f32),
    backdrop: PropValue,
    extra: Vec<(Prop, PropValue)>,
    over: Vec<Vec<(Prop, PropValue)>>,
) -> SceneDiff {
    let colors = ["#f38ba8", "#a6e3a1", "#89b4fa", "#f9e2af"];
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Bar,
        None,
        vec![
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Clip, PropValue::Bool(true)),
        ],
    );
    let abs = |x: f32, y: f32, w: f32, h: f32| {
        vec![
            (Prop::X, num(x)),
            (Prop::Y, num(y)),
            (Prop::Width, num(w)),
            (Prop::Height, num(h)),
            (Prop::Place, PropValue::Keyword("absolute".into())),
        ]
    };
    for i in 0..w / 10 {
        let mut p = abs(i as f32 * 10.0, 0.0, 10.0, h as f32);
        p.push((Prop::Bg, color(colors[i as usize % 4])));
        b.node(NodeKind::Box, Some(root), p);
    }
    let mut p = abs(at.0, at.1, at.2, at.3);
    p.extend([(Prop::Radius, num(12.0)), (Prop::Backdrop, backdrop)]);
    p.extend(extra);
    b.node(NodeKind::Box, Some(root), p);
    for mut o in over {
        o.push((Prop::Place, PropValue::Keyword("absolute".into())));
        b.node(NodeKind::Box, Some(root), o);
    }
    b.diff
}

fn backdrop_call(name: &str, args: Vec<PropValue>) -> PropValue {
    PropValue::Call {
        name: name.into(),
        args,
    }
}

/// `scene` on a CPU surface whose backdrop passes are drawn by the GPU
/// and read back (the path a surface the CPU draws keeps): its frame
/// once their pixels are in.
fn backdrop_by_readback(opts: GpuOptions, scene: SceneDiff, (w, h): (u32, u32)) -> Buffer {
    let mut r = renderer();
    r.set_promotion(false);
    assert!(r.apply(scene).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut host = Host::new(opts);
    let mut buf = Buffer::new(w, h, Scale::ONE);
    for _ in 0..4 {
        buf.paint_at(&mut r, S, 0, Duration::from_secs(1));
        let reqs = r.take_gpu_requests();
        let passes = reqs
            .iter()
            .filter(|q| matches!(q, GpuRequest::Pass(_)))
            .count();
        if passes == 0 {
            assert!(r.gpu_pass_bytes() > 0, "the GPU's pixels are in");
            return buf;
        }
        host.start();
        for q in reqs {
            host.request(q);
        }
        for _ in 0..passes {
            host.until(&mut r, |m| matches!(m, GpuReply::PassPixels { .. }));
        }
    }
    panic!("passes kept being asked for");
}

/// `scene` presented: painted once on the CPU, promoted, and a GPU
/// thread attached for readback (the test sees its pixels).
fn backdrop_presented(
    opts: GpuOptions,
    scene: SceneDiff,
    (w, h): (u32, u32),
) -> (Renderer, Gpu, mpsc::Receiver<()>) {
    let mut r = renderer();
    assert!(r.apply(scene).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(w, h, Scale::ONE);
    buf.paint_at(&mut r, S, 0, Duration::from_secs(1));
    let _ = r.take_gpu_requests();
    r.promote_now(S);
    let _ = r.take_backend_changes();
    r.set_backend(S, Backend::GpuPresent);
    let (ping, pings) = mpsc::channel();
    let gpu = Gpu::spawn(
        Box::new(move || {
            let _ = ping.send(());
        }),
        opts,
    );
    gpu.send(GpuRequest::Attach {
        surface: S,
        handles: None,
        size: Size::new(w, h),
        scale: Scale::ONE,
        opaque: false,
    });
    (r, gpu, pings)
}

/// The keys of `f`'s backdrops.
fn backdrop_keys(f: &strand_gpu::Frame) -> Vec<u64> {
    f.ops
        .iter()
        .filter_map(|o| match o {
            strand_gpu::Op::Backdrop { key, .. } => Some(*key),
            _ => None,
        })
        .collect()
}

fn backdrops_in(f: &strand_gpu::Frame) -> Vec<&strand_gpu::Op> {
    f.ops
        .iter()
        .filter(|o| matches!(o, strand_gpu::Op::Backdrop { .. }))
        .collect()
}

/// `backdrop: glass()` and a large `backdrop: blur()` on a presented
/// surface are drawn in its frames (`Op::Backdrop`): what is behind is
/// not rasterised by the CPU, uploaded, run as a pass of its own and
/// read back a frame late, and nothing of the readback path is held;
/// the frame matches the CPU surface's frame with the GPU's pass read
/// back, within the GPU tolerance.
#[test]
fn a_presented_surface_draws_its_backdrops_in_its_frames() {
    let Some(opts) = device() else { return };
    let cases = [
        (
            "glass",
            (240, 60),
            (40.0, 10.0, 120.0, 40.0),
            backdrop_call("glass", vec![]),
        ),
        (
            "blur",
            (640, 400),
            (20.0, 20.0, 560.0, 360.0),
            backdrop_call("blur", vec![num(8.0)]),
        ),
    ];
    for (name, size, at, backdrop) in cases {
        let want = backdrop_by_readback(
            opts,
            backdrop_scene(size, at, backdrop.clone(), vec![], vec![]),
            size,
        );
        let (mut r, mut gpu, pings) = backdrop_presented(
            opts,
            backdrop_scene(size, at, backdrop, vec![], vec![]),
            size,
        );
        let t = Duration::from_secs(1) + Duration::from_millis(16);
        let frame = r.paint_gpu(S, t).expect("the switch repaints");
        assert_eq!(backdrops_in(&frame).len(), 1, "{name}: in the frame");
        assert!(
            !r.take_gpu_requests()
                .iter()
                .any(|q| matches!(q, GpuRequest::Pass(_))),
            "{name}: no pass of its own"
        );
        let region_sized = frame
            .uploads
            .iter()
            .any(|u| u32::from(u.pixmap.width()) >= (at.2 as u32).min(size.0));
        assert!(!region_sized, "{name}: nothing behind it uploaded");
        assert_eq!(r.gpu_pass_bytes(), 0, "{name}: no readback held");
        let px = draw(&mut gpu, &pings, frame);
        let (bad, worst) = compare_bytes(&px, &want.pixels);
        let share = bad as f64 / f64::from(size.0 * size.1);
        if share > EDGE_SHARE {
            let rgba: Vec<u8> = px
                .chunks_exact(4)
                .flat_map(|p| [p[2], p[1], p[0], p[3]])
                .collect();
            write_png(
                &refs_dir().join(format!("gpu_backdrop_{name}.actual.png")),
                size.0,
                size.1,
                &rgba,
            );
        }
        assert!(
            share <= EDGE_SHARE,
            "{name}: {bad} pixels past {GPU_TOLERANCE} of the readback's (worst {worst})"
        );
        drop(gpu);
    }
}

/// Glass in a presented frame follows the pointer: a pointer over it
/// repaints the surface (its pass's pointer is in the node's damage),
/// and the new frame lights it.
#[test]
fn glass_in_a_presented_frame_follows_the_pointer() {
    let Some(opts) = device() else { return };
    let size = (240, 60);
    let scene = || {
        backdrop_scene(
            size,
            (40.0, 10.0, 120.0, 40.0),
            backdrop_call("glass", vec![]),
            vec![],
            vec![],
        )
    };
    let (mut r, mut gpu, pings) = backdrop_presented(opts, scene(), size);
    let t0 = Duration::from_secs(1);
    let frame = r.paint_gpu(S, t0).expect("the switch repaints");
    let still = backdrop_keys(&frame);
    let before = draw(&mut gpu, &pings, frame);
    assert!(r.paint_gpu(S, t0).is_none(), "idle without the pointer");
    r.set_pointer(S, Some(LogicalPoint { x: 100.0, y: 30.0 }));
    assert!(r.wants_frame(S), "the pointer repaints glass");
    let frame = r
        .paint_gpu(S, t0 + Duration::from_millis(16))
        .expect("a frame for the pointer");
    let pointer = backdrops_in(&frame)
        .first()
        .map(|o| match o {
            strand_gpu::Op::Backdrop { globals, .. } => globals.pointer,
            _ => [-1.0, -1.0],
        })
        .unwrap_or([-1.0, -1.0]);
    assert_eq!(pointer, [60.0, 20.0], "relative to the glass");
    let moved = backdrop_keys(&frame);
    assert!(
        still[0] != 0 && moved[0] != 0 && still != moved,
        "drawn anew"
    );
    let lit = draw(&mut gpu, &pings, frame);
    let i = ((30 * size.0 + 100) * 4) as usize;
    let (b, a) = (&before[i..i + 3], &lit[i..i + 3]);
    assert!((0..3).all(|c| a[c] >= b[c]) && a != b, "{b:?} → {a:?}");
    drop(gpu);
}

/// A backdrop the CPU draws with its group (inside a `filter: blur`),
/// or one such a backdrop reads (drawn before it, under it), keeps the
/// readback path on a presented surface: its pass is asked for on its
/// own and the frame draws its pixels.
#[test]
fn backdrops_the_cpu_draws_with_others_keep_their_own_pass() {
    let size = (240, 60);
    let at = (40.0, 10.0, 120.0, 40.0);
    let blurred = backdrop_scene(
        size,
        at,
        backdrop_call("glass", vec![]),
        vec![(Prop::Filter, backdrop_call("blur", vec![num(2.0)]))],
        vec![],
    );
    // Glass, and over it, later, a box whose backdrop the CPU draws
    // with its `filter: blur` group, reading the glass.
    let read = backdrop_scene(
        size,
        at,
        backdrop_call("glass", vec![]),
        vec![],
        vec![vec![
            (Prop::X, num(100.0)),
            (Prop::Y, num(20.0)),
            (Prop::Width, num(60.0)),
            (Prop::Height, num(30.0)),
            (Prop::Backdrop, backdrop_call("blur", vec![num(4.0)])),
            (Prop::Filter, backdrop_call("blur", vec![num(2.0)])),
        ]],
    );
    for (name, scene) in [("filtered", blurred), ("read", read)] {
        let mut r = renderer();
        assert!(r.apply(scene).is_empty());
        r.attach_surface(S, r.tree().roots()[0]);
        let mut buf = Buffer::new(size.0, size.1, Scale::ONE);
        up_without_a_device(&mut r);
        buf.paint_at(&mut r, S, 0, Duration::from_secs(1));
        // Asked for by the CPU frame, and still in flight.
        let mut reqs = r.take_gpu_requests();
        r.promote_now(S);
        let _ = r.take_backend_changes();
        r.set_backend(S, Backend::GpuPresent);
        let frame = r
            .paint_gpu(S, Duration::from_secs(1) + Duration::from_millis(16))
            .expect("the switch repaints");
        assert!(backdrops_in(&frame).is_empty(), "{name}: not in the frame");
        reqs.extend(r.take_gpu_requests());
        assert!(
            reqs.iter().any(|q| matches!(q, GpuRequest::Pass(_))),
            "{name}: its own pass"
        );
    }
}

/// A small `backdrop: blur()` (under about 0.2 Mpx, the CPU's at
/// quarter scale on a surface the CPU draws) is drawn in a presented
/// surface's frame too, at the same quarter scale (review): nothing
/// rasterised or uploaded for it, smooth across the stripes behind.
#[test]
fn a_small_backdrop_blur_on_a_presented_surface_is_drawn_in_its_frame() {
    let Some(opts) = device() else { return };
    let size = (640, 400);
    let scene = backdrop_scene(
        size,
        (20.0, 20.0, 300.0, 300.0),
        backdrop_call("blur", vec![num(8.0)]),
        vec![],
        vec![],
    );
    let (mut r, mut gpu, pings) = backdrop_presented(opts, scene, size);
    let frame = r
        .paint_gpu(S, Duration::from_secs(1) + Duration::from_millis(16))
        .expect("the switch repaints");
    assert_eq!(backdrops_in(&frame).len(), 1, "in the frame");
    assert!(frame.uploads.iter().all(|u| u.pixmap.width() < 100));
    assert!(
        !r.take_gpu_requests()
            .iter()
            .any(|q| matches!(q, GpuRequest::Pass(_)))
    );
    let px = draw(&mut gpu, &pings, frame);
    let g = |x: u32, y: u32| i32::from(px[((y * size.0 + x) * 4 + 1) as usize]);
    let row: Vec<i32> = (100..200).map(|x| g(x, 150)).collect();
    let step = row.windows(2).map(|p| (p[1] - p[0]).abs()).max();
    assert!(step.unwrap_or(0) <= 6, "smooth: {row:?}");
    // Outside it the stripes stay sharp.
    let outside: Vec<i32> = (400..440).map(|x| g(x, 150)).collect();
    let sharp = outside.windows(2).map(|p| (p[1] - p[0]).abs()).max();
    assert!(sharp.unwrap_or(0) > 40, "{outside:?}");
    drop(gpu);
}

/// (perf-gpu-retain) A backdrop's key in a GPU frame follows what is
/// behind it and nothing else: a box over it changing colour keeps it
/// (the GPU thread draws the last pixels again), one behind it moving
/// changes it. Needs no device: the frames are only lowered.
#[test]
fn a_backdrops_key_follows_what_is_behind_it() {
    let size = (640, 400);
    let scene = backdrop_scene(
        size,
        (20.0, 20.0, 300.0, 300.0),
        backdrop_call("blur", vec![num(8.0)]),
        vec![],
        vec![vec![
            (Prop::X, num(100.0)),
            (Prop::Y, num(100.0)),
            (Prop::Width, num(40.0)),
            (Prop::Height, num(40.0)),
            (Prop::Bg, color("#ffffff")),
        ]],
    );
    let mut r = renderer();
    assert!(r.apply(scene).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(size.0, size.1, Scale::ONE);
    buf.paint_at(&mut r, S, 0, Duration::from_secs(1));
    r.promote_now(S);
    let _ = r.take_backend_changes();
    r.set_backend(S, Backend::GpuPresent);
    up_without_a_device(&mut r);
    let t = |ms: u64| Duration::from_secs(1) + Duration::from_millis(ms);
    let first = backdrop_keys(&r.paint_gpu(S, t(16)).expect("the switch repaints"));
    assert_eq!(first.len(), 1);
    assert_ne!(first[0], 0);
    // The box in front: a new colour.
    // Built in order: the bar, 64 stripes, the card, the box in front.
    let (stripe, front) = (NodeId::new(4, 0), NodeId::new(66, 0));
    let mut d = SceneDiff::new();
    d.set(front, Prop::Bg, color("#000000"));
    assert!(r.apply(d).is_empty());
    let f = r.paint_gpu(S, t(32)).expect("a new colour");
    assert_eq!(backdrop_keys(&f), first, "the same backdrop");
    // A stripe behind it moved.
    let mut d = SceneDiff::new();
    d.set(stripe, Prop::Y, num(10.0));
    assert!(r.apply(d).is_empty());
    let f = r.paint_gpu(S, t(48)).expect("moved");
    assert_ne!(backdrop_keys(&f), first, "drawn anew");
}

// ---------------------------------------------------------------------------
// (perf-gpu-retain review) What a backdrop's key and pixels follow

/// The stripes of [`backdrop_scene`] (nodes 1 to `w / 10`; the bar is 0
/// and the card the next).
fn stripe_ids(w: u32) -> impl Iterator<Item = NodeId> {
    (1..=w / 10).map(|id| NodeId::new(id, 0))
}

/// The card of [`backdrop_scene`].
fn card_id(w: u32) -> NodeId {
    NodeId::new(w / 10 + 1, 0)
}

/// [`backdrop_scene`] with a `blur(sigma)` card at `at`, its stripes
/// faded to 0.1 if `faded`.
fn blur_scene(size: (u32, u32), at: (f32, f32, f32, f32), sigma: f32, faded: bool) -> SceneDiff {
    let mut s = backdrop_scene(
        size,
        at,
        backdrop_call("blur", vec![num(sigma)]),
        vec![],
        vec![],
    );
    if faded {
        for id in stripe_ids(size.0) {
            s.set(id, Prop::Opacity, num(0.1));
        }
    }
    s
}

/// A presented renderer of `scene` without a device: its frames are
/// lowered, not drawn.
fn lowered(scene: SceneDiff, size: (u32, u32)) -> Renderer {
    let mut r = renderer();
    assert!(r.apply(scene).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(size.0, size.1, Scale::ONE);
    buf.paint_at(&mut r, S, 0, Duration::from_secs(1));
    r.promote_now(S);
    let _ = r.take_backend_changes();
    r.set_backend(S, Backend::GpuPresent);
    up_without_a_device(&mut r);
    r
}

/// A backdrop's key and region (whole pixels).
type Keyed = (u64, [i64; 4]);

/// The keys and regions of `f`'s backdrops.
fn keyed(f: &strand_gpu::Frame) -> Vec<Keyed> {
    f.ops
        .iter()
        .filter_map(|o| match o {
            strand_gpu::Op::Backdrop { key, region, .. } => Some((
                *key,
                [region.x0, region.y0, region.x1, region.y1].map(|v| v.round() as i64),
            )),
            _ => None,
        })
        .collect()
}

/// Paints `r`'s presented frames from `from_ms` until it settles: each
/// frame's backdrop keys and regions, and the last frame drawn on `gpu`
/// if given.
fn settle(
    r: &mut Renderer,
    mut gpu: Option<(&mut Gpu, &mpsc::Receiver<()>)>,
    from_ms: u64,
) -> (Vec<Vec<Keyed>>, Option<Vec<u8>>) {
    let mut keys = Vec::new();
    let mut last = None;
    for k in 0..240u64 {
        let t = Duration::from_secs(1) + Duration::from_millis(from_ms + 16 * k);
        if let Some(f) = r.paint_gpu(S, t) {
            keys.push(keyed(&f));
            if let Some((g, p)) = gpu.as_mut() {
                last = Some(draw(g, p, f));
            }
        }
        if !r.wants_frame(S) && k > 2 {
            break;
        }
    }
    (keys, last)
}

/// A change made to a scene.
type Change = fn(&mut SceneDiff);

/// A backdrop's key follows what is behind it however it changes: the
/// stripes behind fading (their `PushOpacity` markers change, the items
/// inside do not) or taking a `filter:` (a `PushLayer`), and the card
/// moving a pixel over stripes it already covered (the items over its
/// region are the same; what it reads is not: every frame drawing it
/// elsewhere has another key). Before the review's fix (the want's key
/// skipped every marker, and a backdrop drawn from its last pixels kept
/// its pre-fade blur) the first two kept their key. Needs no device.
#[test]
fn a_backdrops_key_follows_a_fade_a_filter_or_a_move() {
    let size = (640, 400);
    let changes: [(&str, Change); 3] = [
        ("fade", |d| {
            for id in stripe_ids(640) {
                d.set(id, Prop::Opacity, num(0.1));
            }
        }),
        ("filter", |d| {
            for id in stripe_ids(640) {
                d.set(id, Prop::Filter, backdrop_call("grayscale", vec![num(1.0)]));
            }
        }),
        // Its read region (x 77 to 425) crosses no stripe's edge moving
        // to 102.
        ("move", |d| {
            d.set(card_id(640), Prop::X, num(102.0));
        }),
    ];
    for (name, change) in changes {
        let mut r = lowered(
            blur_scene(size, (101.0, 20.0, 300.0, 300.0), 8.0, false),
            size,
        );
        let (before, _) = settle(&mut r, None, 16);
        let before = before.last().cloned().unwrap_or_default();
        assert_eq!(before.len(), 1, "{name}");
        assert_ne!(before[0].0, 0, "{name}");
        let mut d = SceneDiff::new();
        change(&mut d);
        assert!(r.apply(d).is_empty());
        let (after, _) = settle(&mut r, None, 2000);
        assert!(!after.is_empty(), "{name}: repainted");
        let last = after.last().cloned().unwrap_or_default();
        assert_ne!(
            last[0].0, before[0].0,
            "{name}: drawn from the pixels before it"
        );
        if name == "move" {
            assert_ne!(last[0].1, before[0].1, "moved");
            for k in &after {
                assert!(
                    k[0].1 == before[0].1 || k[0].0 != before[0].0,
                    "a frame drawing it at {:?} with the key it had at {:?}",
                    k[0].1,
                    before[0].1
                );
            }
        } else {
            for k in &after {
                assert_ne!(
                    k[0].0, before[0].0,
                    "{name}: drawn from the pixels before it"
                );
            }
        }
    }
}

/// An ancestor fading leaves a backdrop's key alone: the group open
/// around the card holds the backdrop itself, not what is behind it, so
/// a box fading in with a blurred card in it blurs it once. Needs no
/// device.
#[test]
fn an_ancestor_fading_keeps_a_backdrops_key() {
    let size = (640, 400);
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Bar,
        None,
        vec![
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Clip, PropValue::Bool(true)),
        ],
    );
    let abs = |x: f32, y: f32, w: f32, h: f32| {
        vec![
            (Prop::X, num(x)),
            (Prop::Y, num(y)),
            (Prop::Width, num(w)),
            (Prop::Height, num(h)),
            (Prop::Place, PropValue::Keyword("absolute".into())),
        ]
    };
    for i in 0..64 {
        let mut p = abs(i as f32 * 10.0, 0.0, 10.0, 400.0);
        p.push((Prop::Bg, color(["#f38ba8", "#a6e3a1"][i % 2])));
        b.node(NodeKind::Box, Some(root), p);
    }
    let group = b.node(NodeKind::Box, Some(root), abs(0.0, 0.0, 640.0, 400.0));
    let mut p = abs(101.0, 20.0, 300.0, 300.0);
    p.extend([
        (Prop::Radius, num(12.0)),
        (Prop::Backdrop, backdrop_call("blur", vec![num(8.0)])),
    ]);
    b.node(NodeKind::Box, Some(group), p);
    let mut r = lowered(b.diff, size);
    let (before, _) = settle(&mut r, None, 16);
    let before = before.last().cloned().unwrap_or_default();
    assert_eq!(before.len(), 1);
    let mut d = SceneDiff::new();
    d.set(group, Prop::Opacity, num(0.5));
    assert!(r.apply(d).is_empty());
    let (after, _) = settle(&mut r, None, 2000);
    assert!(!after.is_empty(), "repainted");
    assert_eq!(after.last(), Some(&before), "the same pixels");
}

/// A backdrop over a `shader` node with a clock is drawn anew every
/// frame (key 0): the pass's pixels move with its clock, which the
/// display list's hash does not see. Needs no device.
#[test]
fn a_backdrop_over_a_clocked_shader_is_drawn_anew() {
    let clocked = Arc::new(ShaderCode {
        path: "clock.wgsl".into(),
        wgsl: "@fragment\n\
               fn main(v: StrandVertex) -> @location(0) vec4<f32> {\n\
                   return vec4<f32>(fract(strand.time), 0.0, 0.0, 1.0);\n\
               }\n"
        .into(),
        uniforms: ShaderCode::packed(vec![]),
    });
    let size = (640, 400);
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Bar,
        None,
        vec![
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Clip, PropValue::Bool(true)),
        ],
    );
    let abs = |x: f32, y: f32, w: f32, h: f32| {
        vec![
            (Prop::X, num(x)),
            (Prop::Y, num(y)),
            (Prop::Width, num(w)),
            (Prop::Height, num(h)),
            (Prop::Place, PropValue::Keyword("absolute".into())),
        ]
    };
    let mut p = abs(60.0, 60.0, 200.0, 200.0);
    p.push((Prop::Shader, PropValue::Shader(clocked)));
    b.node(NodeKind::Shader, Some(root), p);
    let mut p = abs(20.0, 20.0, 300.0, 300.0);
    p.extend([
        (Prop::Radius, num(12.0)),
        (Prop::Backdrop, backdrop_call("blur", vec![num(8.0)])),
    ]);
    b.node(NodeKind::Box, Some(root), p);
    let mut r = lowered(b.diff, size);
    let t = |ms: u64| Duration::from_secs(1) + Duration::from_millis(ms);
    for ms in [16, 32, 48] {
        let f = r.paint_gpu(S, t(ms)).expect("the clock repaints");
        assert_eq!(backdrop_keys(&f), vec![0], "at {ms} ms");
    }
}

/// Pixels: a presented surface's blurred card after the stripes behind
/// it fade, or after it moves a pixel, against the same scene drawn
/// fresh on a device of its own: the same within the GPU tolerance. A
/// small card (the CPU's reduction) and a large one (full size). The
/// review measured the stale card at 89,902 and 113,350 pixels off.
#[test]
fn a_backdrops_pixels_follow_a_fade_or_a_move_behind_it() {
    let Some(opts) = device() else { return };
    let size = (640, 400);
    for at in [(101.0, 20.0, 300.0, 300.0), (20.0, 20.0, 560.0, 360.0)] {
        for (name, faded, moved) in [("fade", true, at.0), ("move", false, at.0 + 1.0)] {
            let (mut r, mut gpu, pings) =
                backdrop_presented(opts, blur_scene(size, at, 8.0, false), size);
            let (_, before) = settle(&mut r, Some((&mut gpu, &pings)), 16);
            let mut d = SceneDiff::new();
            if faded {
                for id in stripe_ids(size.0) {
                    d.set(id, Prop::Opacity, num(0.1));
                }
            }
            d.set(card_id(size.0), Prop::X, num(moved));
            assert!(r.apply(d).is_empty());
            let (_, live) = settle(&mut r, Some((&mut gpu, &pings)), 2000);
            drop(gpu);
            let (mut f, mut gpu2, pings2) = backdrop_presented(
                opts,
                blur_scene(size, (moved, at.1, at.2, at.3), 8.0, faded),
                size,
            );
            let (_, fresh) = settle(&mut f, Some((&mut gpu2, &pings2)), 16);
            drop(gpu2);
            let (before, live, fresh) = (before.unwrap(), live.unwrap(), fresh.unwrap());
            let (bad, worst) = compare_bytes(&live, &fresh);
            assert!(
                bad as f64 / f64::from(size.0 * size.1) <= EDGE_SHARE,
                "{name} {at:?}: {bad} pixels past {GPU_TOLERANCE} of a fresh draw (worst {worst})"
            );
            let (changed, _) = compare_bytes(&before, &live);
            assert!(changed > 0, "{name} {at:?}: the frame changed");
        }
    }
}

/// A small `backdrop: blur()` (under about 0.2 Mpx) looks the same
/// whoever draws it: a presented surface draws it in its frame at the
/// CPU's reduction (a quarter for sigma 8 and up), so the frame matches
/// the CPU surface's, at scale 1 and 1.5, within the GPU tolerance. At
/// full resolution it was 99.8% of the card past the tolerance (worst
/// 11 at sigma 8, 19 at 16): the look changed at every promotion.
#[test]
fn a_small_backdrop_blur_looks_the_same_on_either_backend() {
    let Some(opts) = device() else { return };
    let logical = (640u32, 400u32);
    let at = (100.0, 60.0, 240.0, 240.0);
    for scale in [Scale::ONE, Scale::new(180).unwrap()] {
        let size = scale.physical_size(LogicalSize::new(logical.0 as f32, logical.1 as f32));
        for sigma in [2.0f32, 8.0, 16.0] {
            let scene = || blur_scene(logical, at, sigma, false);
            let mut c = renderer();
            c.set_promotion(false);
            assert!(c.apply(scene()).is_empty());
            c.attach_surface(S, c.tree().roots()[0]);
            c.configure_surface(S, size, scale);
            let mut buf = Buffer::new(size.w, size.h, scale);
            buf.paint_at(&mut c, S, 0, Duration::from_secs(1));
            assert!(
                !c.take_gpu_requests()
                    .iter()
                    .any(|q| matches!(q, GpuRequest::Pass(_))),
                "a small blur is the CPU's on a CPU surface"
            );
            let mut r = renderer();
            assert!(r.apply(scene()).is_empty());
            r.attach_surface(S, r.tree().roots()[0]);
            r.configure_surface(S, size, scale);
            let mut first = Buffer::new(size.w, size.h, scale);
            first.paint_at(&mut r, S, 0, Duration::from_secs(1));
            let _ = r.take_gpu_requests();
            r.promote_now(S);
            let _ = r.take_backend_changes();
            r.set_backend(S, Backend::GpuPresent);
            let (ping, pings) = mpsc::channel();
            let mut gpu = Gpu::spawn(
                Box::new(move || {
                    let _ = ping.send(());
                }),
                opts,
            );
            gpu.send(GpuRequest::Attach {
                surface: S,
                handles: None,
                size,
                scale,
                opaque: false,
            });
            let frame = r
                .paint_gpu(S, Duration::from_secs(1) + Duration::from_millis(16))
                .expect("the switch repaints");
            let reduce: Vec<u32> = backdrops_in(&frame)
                .iter()
                .filter_map(|o| match o {
                    strand_gpu::Op::Backdrop { reduce, .. } => Some(*reduce),
                    _ => None,
                })
                .collect();
            let sigma_px = sigma * scale.as_f32();
            let want = if sigma_px >= 8.0 {
                4
            } else if sigma_px >= 2.0 {
                2
            } else {
                1
            };
            assert_eq!(reduce, vec![want], "in the frame, at the CPU's reduction");
            assert!(
                !r.take_gpu_requests()
                    .iter()
                    .any(|q| matches!(q, GpuRequest::Pass(_))),
                "no pass of its own"
            );
            let px = draw(&mut gpu, &pings, frame);
            drop(gpu);
            let (bad, worst) = compare_bytes(&px, &buf.pixels);
            let share = bad as f64 / f64::from(size.w * size.h);
            eprintln!(
                "blur({sigma}) at {scale:?}: {bad} pixels past {GPU_TOLERANCE}, worst {worst}"
            );
            assert!(
                share <= EDGE_SHARE,
                "blur({sigma}) at {scale:?}: {bad} pixels past {GPU_TOLERANCE} of the CPU's (worst {worst})"
            );
        }
    }
}

/// A surface promoted, sent back to the CPU and promoted again holds no
/// GPU frame until it presents one (its GPU thread target is new): a
/// crossfade planned while it was presented whose first frame is its
/// first after the second promotion blends the CPU's snapshot, uploaded,
/// not a frame the GPU thread no longer has (`Surf::shown` is cleared by
/// each backend change). Needs no device.
#[test]
fn a_crossfade_right_after_a_second_promotion_blends_the_cpus_snapshot() {
    const W: u32 = 640;
    const H: u32 = 330;
    let t0 = Duration::from_secs(1);
    let at = |k: u32| t0 + Duration::from_nanos(1_000_000_000 * k as u64 / 60);
    let (a, b) = split_tables();
    let mut r = renderer();
    fade_scene(&mut r, a);
    let mut buf = Buffer::new(W, H, Scale::ONE);
    buf.paint_at(&mut r, S, 0, t0);
    r.update();
    buf.paint_at(&mut r, S, 0, t0);
    r.promote_now(S);
    let _ = r.take_backend_changes();
    r.set_backend(S, Backend::GpuPresent);
    assert!(r.paint_gpu(S, at(1)).is_some(), "presented");
    let mut d = SceneDiff::new();
    d.set_tokens(b, Transition::Default);
    assert!(r.apply(d).is_empty());
    assert_eq!(r.swap_crossfades(), 1, "the swap crossfades");
    // Back to the CPU and promoted again before its first frame.
    r.set_backend(S, Backend::Cpu);
    r.promote_now(S);
    let _ = r.take_backend_changes();
    r.set_backend(S, Backend::GpuPresent);
    assert_eq!(r.backend(S), Backend::GpuPresent);
    let frame = r.paint_gpu(S, at(2)).expect("the crossfade's first frame");
    assert!(
        frame.ops.iter().any(|o| matches!(
            o,
            strand_gpu::Op::PushLayer(strand_gpu::Layer { blend: Some(_), .. })
        )),
        "a crossfade frame"
    );
    assert_eq!(frame.snapshot, strand_gpu::FrameSnapshot::None);
    assert!(
        !frame
            .ops
            .iter()
            .any(|o| matches!(o, strand_gpu::Op::Snapshot { .. })),
        "no GPU snapshot drawn"
    );
    assert!(
        frame
            .uploads
            .iter()
            .any(|u| u32::from(u.pixmap.width()) == W),
        "the CPU's snapshot uploaded"
    );
}

/// A crossfade landing mid-crossfade on a presented surface whose first
/// fade the GPU thread holds, sent back to the CPU before the second's
/// first frame: the CPU takes the second's snapshot as the blend the
/// surface showed, drawing the first fade's snapshot from its old frame
/// (it has no pixels of it), so the fade goes on from what was on
/// screen, as a surface the CPU drew all along does. Without it the
/// first frame jumped to the first fade's new frame alone.
#[test]
fn a_crossfade_replaced_after_its_gpu_snapshot_goes_on_from_the_blend_shown() {
    const W: u32 = 640;
    const H: u32 = 330;
    let t0 = Duration::from_secs(1);
    let at = |k: u32| t0 + Duration::from_nanos(1_000_000_000 * k as u64 / 60);
    let swap = |r: &mut Renderer, t: &TokenTable| {
        let mut d = SceneDiff::new();
        d.set_tokens(t.clone(), Transition::Default);
        assert!(r.apply(d).is_empty());
    };
    let (a, b) = split_tables();
    // The CPU all along.
    let mut cpu = renderer();
    fade_scene(&mut cpu, a.clone());
    let mut want = Buffer::new(W, H, Scale::ONE);
    want.paint_at(&mut cpu, S, 0, t0);
    cpu.update();
    want.paint_at(&mut cpu, S, 0, t0);
    let old = want.pixels.clone();
    swap(&mut cpu, &b);
    for k in 1..=4 {
        want.paint_at(&mut cpu, S, 0, at(k));
    }
    let mid = want.pixels.clone();
    swap(&mut cpu, &a);
    want.paint_at(&mut cpu, S, 0, at(5));
    // Presented through the first fade, then back on the CPU.
    let mut r = renderer();
    fade_scene(&mut r, a.clone());
    let mut buf = Buffer::new(W, H, Scale::ONE);
    buf.paint_at(&mut r, S, 0, t0);
    r.update();
    buf.paint_at(&mut r, S, 0, t0);
    r.promote_now(S);
    let _ = r.take_backend_changes();
    r.set_backend(S, Backend::GpuPresent);
    assert!(r.paint_gpu(S, t0).is_some(), "the switch repaints");
    swap(&mut r, &b);
    let mut how = Vec::new();
    for k in 1..=4 {
        let f = r.paint_gpu(S, at(k)).expect("a crossfade frame");
        how.push(f.snapshot);
    }
    use strand_gpu::FrameSnapshot::{Keep, Take};
    assert_eq!(how, vec![Take, Keep, Keep, Keep], "held on the GPU");
    swap(&mut r, &a);
    r.set_backend(S, Backend::Cpu);
    buf.paint_at(&mut r, S, 0, at(5));
    let (bad, worst) = compare_bytes(&buf.pixels, &want.pixels);
    assert!(
        bad == 0 && worst <= 1,
        "{bad} pixels past {GPU_TOLERANCE} of the CPU's blend (worst {worst})"
    );
    // The first fade was under way: neither its old frame nor its new.
    let (moved, _) = compare_bytes(&mid, &old);
    assert!(moved > 0, "mid-crossfade");
}
