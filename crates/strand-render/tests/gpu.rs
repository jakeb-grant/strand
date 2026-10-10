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
