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

use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::*;
use strand_gpu::{Gpu, GpuErrorKind, GpuOptions, GpuReply, GpuRequest};
use strand_render::Renderer;
use strand_scene::shader::{ShaderCode, UniformType};
use strand_scene::*;

const S: SurfaceId = SurfaceId(1);

/// Per-channel difference allowed between the GPU's and the CPU's frame.
const GPU_TOLERANCE: u8 = 6;

/// Share of pixels (edges) allowed past [`GPU_TOLERANCE`].
const EDGE_SHARE: f64 = 0.02;

/// How long a reply may take (a cold lavapipe device: well under this).
const WAIT: Duration = Duration::from_secs(60);

/// The host side: starts the `Gpu` when the renderer has requests and
/// hands replies back, as `strand run` does.
struct Host {
    gpu: Option<Gpu>,
    pings: mpsc::Receiver<()>,
    ping: mpsc::Sender<()>,
    opts: GpuOptions,
    replies: Vec<GpuReply>,
}

impl Host {
    fn new(opts: GpuOptions) -> Self {
        let (ping, pings) = mpsc::channel();
        Host {
            gpu: None,
            pings,
            ping,
            opts,
            replies: Vec::new(),
        }
    }

    /// Sends what the renderer asks (starting the `Gpu`), and attaches
    /// promoted surfaces for readback.
    fn send(&mut self, r: &mut Renderer) {
        for c in r.take_backend_changes() {
            match c {
                BackendChange::Promote(s) => {
                    self.start();
                    self.request(GpuRequest::Attach {
                        surface: s,
                        handles: None,
                        size: Size::new(240, 60),
                        scale: Scale::ONE,
                        opaque: false,
                    });
                }
                BackendChange::Demote(s) => self.request(GpuRequest::Release(s)),
                BackendChange::Drop => {
                    if let Some(g) = self.gpu.take() {
                        drop(g);
                    }
                }
            }
        }
        let reqs = r.take_gpu_requests();
        if !reqs.is_empty() {
            self.start();
        }
        for q in reqs {
            self.request(q);
        }
    }

    fn start(&mut self) {
        if self.gpu.is_none() {
            let ping = self.ping.clone();
            self.gpu = Some(Gpu::spawn(
                Box::new(move || {
                    let _ = ping.send(());
                }),
                self.opts,
            ));
        }
    }

    fn request(&mut self, q: GpuRequest) {
        if let Some(g) = &self.gpu {
            g.send(q);
        }
    }

    /// Waits for a reply `want` accepts, delivering every reply to `r`.
    fn until(&mut self, r: &mut Renderer, want: impl Fn(&GpuReply) -> bool) -> GpuReply {
        let deadline = Instant::now() + WAIT;
        loop {
            while let Some(reply) = self.gpu.as_mut().and_then(Gpu::try_recv) {
                let hit = want(&reply);
                self.replies.push(reply.clone());
                r.deliver_gpu(reply.clone());
                self.send(r);
                if hit {
                    return reply;
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "no reply in {WAIT:?}: {:?}", self.replies);
            let _ = self.pings.recv_timeout(left.min(Duration::from_millis(50)));
        }
    }
}

/// A device to test on, or `None` (skipped) without one, unless
/// `STRAND_REQUIRE_GPU=1`.
fn device() -> Option<GpuOptions> {
    let opts = GpuOptions::from_env();
    let (tx, rx) = mpsc::channel();
    let mut g = Gpu::spawn(
        Box::new(move || {
            let _ = tx.send(());
        }),
        opts,
    );
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(reply) = g.try_recv() {
            match reply {
                GpuReply::Ready(_) => return Some(opts),
                GpuReply::Unavailable(e) => {
                    assert!(
                        std::env::var("STRAND_REQUIRE_GPU").as_deref() != Ok("1"),
                        "STRAND_REQUIRE_GPU=1 and no device: {e}"
                    );
                    eprintln!("skipped: no GPU device ({e})");
                    return None;
                }
                _ => {}
            }
        }
        assert!(Instant::now() < deadline, "the GPU thread did not answer");
        let _ = rx.recv_timeout(Duration::from_millis(50));
    }
}

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
    (b.diff, node)
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

/// The device goes 30 s (here 50 ms) after its last use, at the wake
/// render asked for; the thread ends.
#[test]
fn the_device_is_dropped_after_idle_at_one_wake() {
    let Some(opts) = device() else { return };
    let mut r = renderer();
    r.set_gpu_idle(Duration::from_millis(50));
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
    let wake = r.next_wake().expect("render wakes to drop the device");
    std::thread::sleep(wake.saturating_duration_since(Instant::now()) + Duration::from_millis(5));
    r.update();
    assert_eq!(r.take_backend_changes(), [BackendChange::Drop]);
    assert_eq!(r.gpu_status(), GpuStatus::Unused);
    assert_eq!(r.next_wake(), None, "nothing more to wake for");
    let gpu = host.gpu.take().expect("the Gpu ran");
    drop(gpu);
}

/// A still `shader` node keeps its pixels across the device's drop: they
/// are CPU pixmaps, not GPU memory. A later frame of its surface (the
/// bar's clock ticking) repaints it from them, asks for no new pass, and
/// so does not start the device again.
#[test]
fn a_still_shader_keeps_its_pixels_when_the_device_drops() {
    let Some(opts) = device() else { return };
    let mut r = renderer();
    r.set_gpu_idle(Duration::from_millis(50));
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
    // Idle: the device drops at its wake.
    let wake = r.next_wake().expect("render wakes to drop the device");
    std::thread::sleep(wake.saturating_duration_since(Instant::now()) + Duration::from_millis(5));
    r.update();
    assert_eq!(r.take_backend_changes(), [BackendChange::Drop]);
    assert_eq!(r.gpu_status(), GpuStatus::Unused);
    drop(host.gpu.take());
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
fn rich_scene() -> (SceneDiff, Vec<(NodeId, Vec<Effect>)>) {
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
    let group = b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(80.0)),
            (Prop::Y, num(10.0)),
            (Prop::Size, num(40.0)),
            (Prop::Bg, color("#a6e3a1")),
        ],
    );
    let masked = b.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::X, num(130.0)),
            (Prop::Y, num(10.0)),
            (Prop::Size, num(40.0)),
            (Prop::Bg, color("#cba6f7")),
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
    let effects = vec![
        (group, vec![Effect::Opacity(0.5)]),
        (
            masked,
            vec![Effect::Mask(Mask::Radial {
                at: Anchor::Center,
                size: 16.0,
            })],
        ),
    ];
    (b.diff, effects)
}

fn rich(r: &mut Renderer) {
    let (diff, effects) = rich_scene();
    assert!(r.apply(diff).is_empty());
    for (id, e) in effects {
        r.set_layer_effects(id, e);
    }
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
    r.set_layer_effects(r.tree().roots()[0], vec![Effect::Opacity(0.9)]);
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
    let Some(opts) = device() else { return };
    let paint = |r: &mut Renderer, buf: &mut Buffer| {
        let (diff, _) = canvas_scene::canvas_scene(canvas_scene::chart());
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

/// A start that fails the same way at every retry (once per 30 s while a
/// `shader` node shows: no adapter, only a software one) is a warning
/// once; a new reason, or the same after the device was up, is said
/// again. Needs no device.
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
