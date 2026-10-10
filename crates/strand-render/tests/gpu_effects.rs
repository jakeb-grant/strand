//! (M4) design.md's bundled GPU effects driven through the renderer on
//! the environment's Vulkan device (lavapipe in CI; skipped without one
//! unless `STRAND_REQUIRE_GPU=1`): each drawn by the GPU once its pixels
//! are back (against a reference rendered on lavapipe, tolerance 2 per
//! channel), each starting the device only while it is visible and
//! letting it drop after idle, the CPU's fallback until then; and a
//! `shader` node's `strand.pointer` following the pointer.
//!
//! The CPU fallbacks themselves are `tests/effects.rs`'s.

#![cfg(feature = "gpu")]

mod common;
mod gpu_host;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use gpu_host::*;
use strand_gpu::{GpuReply, GpuRequest};
use strand_render::Renderer;
use strand_scene::shader::{ShaderCode, UniformType};
use strand_scene::*;

const S: SurfaceId = SurfaceId(1);

/// Per-channel difference allowed against a lavapipe reference.
const REF_TOLERANCE: u8 = 2;

type Fx = Vec<(Prop, PropValue)>;

fn call(name: &str, args: Vec<PropValue>) -> PropValue {
    PropValue::Call {
        name: name.into(),
        args,
    }
}

fn kw(k: &str) -> PropValue {
    PropValue::Keyword(k.into())
}

fn at_xy(x: f32, y: f32, w: f32, h: f32) -> Fx {
    vec![
        (Prop::X, num(x)),
        (Prop::Y, num(y)),
        (Prop::Width, num(w)),
        (Prop::Height, num(h)),
        (Prop::Place, kw("absolute")),
    ]
}

/// A renderer, a buffer and a host for a `w × h` bar (clipped) holding
/// `nodes` (each a kind, its props and its children's props).
struct Run {
    r: Renderer,
    buf: Buffer,
    host: Host,
    ids: Vec<NodeId>,
    /// Passes asked for whose pixels are not back yet.
    inflight: usize,
}

fn run(nodes: Vec<(NodeKind, Fx, Vec<Fx>)>, (w, h): (u32, u32)) -> Option<Run> {
    let opts = device()?;
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Bar,
        None,
        vec![
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Clip, PropValue::Bool(true)),
        ],
    );
    let mut ids = Vec::new();
    for (kind, props, children) in nodes {
        let id = b.node(kind, Some(root), props);
        for c in children {
            b.node(NodeKind::Box, Some(id), c);
        }
        ids.push(id);
    }
    let mut tokens = TokenTable::default();
    tokens.insert("accent", PropValue::Color(hex("#89b4fa")));
    b.diff.set_tokens(tokens, Transition::Instant);
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    Some(Run {
        r,
        buf: Buffer::new(w, h, Scale::ONE),
        host: Host::new(opts),
        ids,
        inflight: 0,
    })
}

impl Run {
    /// Paints the frame at `ms`; returns the passes it asked for (sent).
    fn frame(&mut self, ms: u64) -> Vec<strand_gpu::PassFrame> {
        self.buf
            .paint_at(&mut self.r, S, 0, Duration::from_millis(ms));
        let reqs = self.r.take_gpu_requests();
        let passes: Vec<_> = reqs
            .iter()
            .filter_map(|q| match q {
                GpuRequest::Pass(p) => Some(p.clone()),
                _ => None,
            })
            .collect();
        if !reqs.is_empty() {
            self.host.start();
        }
        self.inflight += passes.len();
        for q in reqs {
            self.host.request(q);
        }
        assert!(
            self.r.take_backend_changes().is_empty(),
            "nothing is promoted here"
        );
        passes
    }

    /// Waits for the pixels of every pass asked for so far.
    fn wait(&mut self) {
        while self.inflight > 0 {
            let reply = self.host.until(&mut self.r, |m| {
                matches!(
                    m,
                    GpuReply::PassPixels { .. }
                        | GpuReply::Failed { .. }
                        | GpuReply::Unavailable(_)
                )
            });
            assert!(
                matches!(reply, GpuReply::PassPixels { .. }),
                "the pass drew: {reply:?}"
            );
            self.inflight -= 1;
        }
    }

    /// Paints at `ms` until a frame asks for no pass, delivering each
    /// pass's pixels before the next frame: the frame the GPU's pixels
    /// are all in.
    fn settle(&mut self, ms: u64) {
        for _ in 0..8 {
            self.wait();
            if self.frame(ms).is_empty() {
                return;
            }
        }
        panic!("passes kept being asked for at {ms} ms");
    }

    /// Straight RGB of pixel `(x, y)`.
    fn rgb(&self, x: u32, y: u32) -> [u8; 3] {
        let [b, g, r, _] = self.buf.px(x, y);
        [r, g, b]
    }
}

/// A 40 px light group with a dark square in it, carrying `fx`.
fn tile(x: f32, fx: Fx) -> (NodeKind, Fx, Vec<Fx>) {
    let mut p = at_xy(x, 10.0, 40.0, 40.0);
    p.push((Prop::Bg, color("#f5e0dc")));
    p.extend(fx);
    let mut inner = at_xy(10.0, 10.0, 20.0, 20.0);
    inner.push((Prop::Bg, color("#1e66f5")));
    (NodeKind::Box, p, vec![inner])
}

/// design.md "Bundled GPU effects": `bloom(r)`, `crt()`, `chromatic(px)`
/// and `wobble(amp)` drawn by the GPU over each node's subtree, in place
/// of the CPU's fallback (a glow, the subtree unfiltered) once their
/// pixels are back (ref `gpu_filters.png`).
#[test]
fn bundled_filters_are_drawn_by_the_gpu() {
    let f = |name: &str, args| vec![(Prop::Filter, call(name, args))];
    let Some(mut run) = run(
        vec![
            tile(10.0, f("bloom", vec![num(6.0)])),
            tile(70.0, f("crt", vec![])),
            tile(130.0, f("chromatic", vec![num(3.0)])),
            tile(190.0, f("wobble", vec![num(4.0)])),
        ],
        (240, 60),
    ) else {
        return;
    };
    // The first frame is the CPU's, and asks for the four passes.
    assert_eq!(run.frame(1000).len(), 4);
    let cpu = run.buf.pixels.clone();
    run.settle(1000);
    assert_ne!(run.buf.pixels, cpu, "the GPU's frame");
    let bar = [0x1e, 0x1e, 0x2e];
    // Bloom: the light group bleeds past its box, the dark square stays.
    assert!(run.rgb(7, 30)[0] > bar[0] + 15, "{:?}", run.rgb(7, 30));
    // CRT: the screen's corners are cut round, scanlines down its middle.
    assert_eq!(run.rgb(70, 10), bar, "a cut corner");
    let col: Vec<u8> = (14..20).map(|y| run.rgb(80, y)[1]).collect();
    assert!(col.iter().max() > col.iter().min(), "scanlines: {col:?}");
    // Chromatic: blue moved left of the box (red stays inside).
    let left = run.rgb(128, 30);
    assert!(left[2] > bar[2] + 60 && left[0] < bar[0] + 30, "{left:?}");
    // Wobble: the dark square's edge is moved on some row.
    let unmoved = (12..48).all(|y| {
        let i = ((y * 240 + 200) * 4) as usize;
        run.buf.pixels[i..i + 4] == cpu[i..i + 4]
    });
    assert!(!unmoved, "wobble moves rows");
    assert_matches_ref("gpu_filters", &run.buf, REF_TOLERANCE);
}

/// The GPU's 3-D `tilt:` (design.md "true 3D perspective tilt"): a first
/// turn is the CPU's 2-D one while the device comes up; then the node
/// turns about its vertical axis, the side the pointer is on pressed
/// away (shorter), and fits its box (ref `gpu_tilt.png`).
#[test]
fn tilt_turns_in_3d_on_the_gpu() {
    let mut p = at_xy(90.0, 10.0, 60.0, 40.0);
    p.extend([
        (Prop::Bg, color("#89b4fa")),
        (Prop::Tilt, PropValue::Angle(20.0)),
    ]);
    let Some(mut run) = run(vec![(NodeKind::Box, p, vec![])], (240, 60)) else {
        return;
    };
    run.frame(1000);
    // The pointer at the box's right edge, half way down.
    run.r
        .set_pointer(S, Some(LogicalPoint { x: 150.0, y: 30.0 }));
    let mut t = 1000;
    while run.r.wants_frame(S) {
        t += 16;
        run.settle(t);
        assert!(t < 5000, "settles");
    }
    // Covered rows in a column near each side.
    let tall = |x: u32| (0..60).filter(|&y| run.rgb(x, y)[2] > 0xc0).count();
    let (near, far) = (tall(93), tall(146));
    assert!(near >= 36, "the near side spans the box: {near}");
    assert!(far + 4 <= near, "the far side is shorter: {near} {far}");
    assert_eq!(run.rgb(89, 30), [0x1e, 0x1e, 0x2e], "inside its box");
    assert_matches_ref("gpu_tilt", &run.buf, REF_TOLERANCE);
}

/// Stripes behind, 10 px each, across a `w`-wide bar.
fn stripes(w: u32, h: f32) -> Vec<(NodeKind, Fx, Vec<Fx>)> {
    let colors = ["#f38ba8", "#a6e3a1", "#89b4fa", "#f9e2af"];
    (0..w / 10)
        .map(|i| {
            let mut p = at_xy(i as f32 * 10.0, 0.0, 10.0, h);
            p.push((Prop::Bg, color(colors[i as usize % 4])));
            (NodeKind::Box, p, vec![])
        })
        .collect()
}

/// `backdrop: glass()` (design.md: liquid glass with refraction,
/// dispersion, a fresnel rim and a pointer highlight) drawn by the GPU
/// at full resolution in place of the CPU's blur and tint: the stripes
/// behind stay sharp enough to see, its rim is lit, and the pointer over
/// it repaints it with a highlight (ref `gpu_glass.png`).
#[test]
fn glass_is_drawn_by_the_gpu_and_follows_the_pointer() {
    let mut nodes = stripes(240, 60.0);
    let mut p = at_xy(40.0, 10.0, 120.0, 40.0);
    p.extend([
        (Prop::Radius, num(12.0)),
        (Prop::Backdrop, call("glass", vec![])),
    ]);
    nodes.push((NodeKind::Box, p, vec![]));
    let Some(mut run) = run(nodes, (240, 60)) else {
        return;
    };
    assert_eq!(run.frame(1000).len(), 1, "one backdrop pass");
    let spread = |run: &Run| {
        let row: Vec<u8> = (80..120).map(|x| run.rgb(x, 30)[1]).collect();
        row.iter().max().copied().unwrap_or(0) - row.iter().min().copied().unwrap_or(0)
    };
    let cpu = spread(&run);
    run.settle(1000);
    let gpu = spread(&run);
    assert!(
        gpu > cpu + 40,
        "lightly frosted, not blurred: {gpu} vs {cpu}"
    );
    assert_matches_ref("gpu_glass", &run.buf, REF_TOLERANCE);
    // The pointer over it: a new frame, and a highlight under it.
    let before = run.rgb(100, 30);
    run.r
        .set_pointer(S, Some(LogicalPoint { x: 100.0, y: 30.0 }));
    assert!(run.r.wants_frame(S), "the pointer repaints glass");
    run.settle(1100);
    let lit = run.rgb(100, 30);
    assert!(
        (0..3).all(|c| lit[c] >= before[c]) && lit != before,
        "{before:?} → {lit:?}"
    );
}

/// A large `backdrop: blur(r)` (over about 0.2 Mpx) is the GPU's, at
/// full resolution; a smaller one stays the CPU's and starts nothing.
#[test]
fn a_large_backdrop_blur_is_the_gpus() {
    for (w, h, gpu) in [(560.0, 360.0, true), (300.0, 300.0, false)] {
        let mut nodes = stripes(640, 400.0);
        let mut p = at_xy(20.0, 20.0, w, h);
        p.push((Prop::Backdrop, call("blur", vec![num(8.0)])));
        nodes.push((NodeKind::Box, p, vec![]));
        let Some(mut run) = run(nodes, (640, 400)) else {
            return;
        };
        let passes = run.frame(1000);
        if !gpu {
            assert!(passes.is_empty(), "{w}×{h}: the CPU's");
            assert_eq!(run.r.gpu_status(), GpuStatus::Unused);
            continue;
        }
        assert_eq!(passes.len(), 1);
        // Read past the box by 3σ (24 px), within the surface.
        assert_eq!(passes[0].size, Size::new(604, 400));
        let cpu = run.buf.pixels.clone();
        run.settle(1000);
        assert_ne!(run.buf.pixels, cpu, "the GPU's blur");
        // Smooth across the stripes: neighbours differ little.
        let row: Vec<i32> = (100..200).map(|x| run.rgb(x, 200)[1] as i32).collect();
        let step = row.windows(2).map(|p| (p[1] - p[0]).abs()).max();
        assert!(step.unwrap_or(0) <= 6, "{row:?}");
    }
}

/// `effect aurora` animated on the GPU (its CPU fallback is a still
/// frame with a notice): it runs a clock, moves, and says nothing
/// (ref `gpu_aurora.png` at 1 s).
#[test]
fn aurora_is_animated_by_the_gpu() {
    let mut p = at_xy(0.0, 0.0, 240.0, 60.0);
    p.push((Prop::Style, kw("aurora")));
    let Some(mut run) = run(vec![(NodeKind::Effect, p, vec![])], (240, 60)) else {
        return;
    };
    run.settle(1000);
    assert!(run.r.wants_frame(S), "its clock runs");
    assert_matches_ref("gpu_aurora", &run.buf, REF_TOLERANCE);
    let first = run.buf.pixels.clone();
    run.settle(2000);
    assert_ne!(run.buf.pixels, first, "it moves");
    assert!(
        run.r.take_effect_notices().is_empty(),
        "no still-frame notice"
    );
}

/// `particles` above 1,000 alive (design.md: "above: GPU"): every one of
/// `rate × life` drawn, as sprites, with no capped notice (ref
/// `gpu_particles.png`).
#[test]
fn particles_above_a_thousand_are_drawn_by_the_gpu() {
    let mut p = at_xy(0.0, 0.0, 240.0, 60.0);
    p.extend([
        (Prop::Rate, num(4000.0)),
        (Prop::Life, PropValue::Duration(Duration::from_secs(1))),
        (Prop::Sprite, call("dot", vec![num(2.0)])),
        (Prop::Color, color("#f5c2e7")),
    ]);
    let Some(mut run) = run(vec![(NodeKind::Particles, p, vec![])], (240, 60)) else {
        return;
    };
    run.frame(2000);
    let lit = |px: &[u8]| px.chunks_exact(4).filter(|p| p[2] > 0x80).count();
    let cpu = lit(&run.buf.pixels);
    run.settle(2000);
    let gpu = lit(&run.buf.pixels);
    assert!(
        gpu > cpu * 3 / 2,
        "more alive than the CPU's 1,000: {gpu} vs {cpu}"
    );
    assert!(run.r.take_effect_notices().is_empty(), "no capped notice");
    assert_matches_ref("gpu_particles", &run.buf, REF_TOLERANCE);
}

/// Each bundled effect starts the device only while it is visible: off
/// the bar (clipped out) it asks for nothing; in view it asks; moved out
/// again nothing wants the device, and it drops after the idle time (30
/// s; 50 ms here).
#[test]
fn bundled_effects_start_the_gpu_only_while_visible() {
    let f = |name: &str, args| vec![(Prop::Filter, call(name, args))];
    let kinds: Vec<(&str, NodeKind, Fx)> = vec![
        ("bloom", NodeKind::Box, f("bloom", vec![num(6.0)])),
        ("crt", NodeKind::Box, f("crt", vec![])),
        ("chromatic", NodeKind::Box, f("chromatic", vec![num(2.0)])),
        ("wobble", NodeKind::Box, f("wobble", vec![num(3.0)])),
        (
            "glass",
            NodeKind::Box,
            vec![(Prop::Backdrop, call("glass", vec![]))],
        ),
        (
            "aurora",
            NodeKind::Effect,
            vec![(Prop::Style, kw("aurora"))],
        ),
        (
            "particles",
            NodeKind::Particles,
            vec![
                (Prop::Rate, num(3000.0)),
                (Prop::Life, PropValue::Duration(Duration::from_secs(1))),
            ],
        ),
    ];
    for (name, kind, fx) in kinds {
        let mut p = at_xy(1000.0, 10.0, 40.0, 40.0);
        p.push((Prop::Bg, color("#f5e0dc")));
        p.extend(fx);
        let Some(mut run) = run(vec![(kind, p, vec![])], (240, 60)) else {
            return;
        };
        run.r.set_gpu_idle(Duration::from_millis(50));
        let node = run.ids[0];
        assert!(run.frame(1000).is_empty(), "{name}: hidden, nothing asked");
        assert!(!run.r.gpu_in_demand(), "{name}");
        assert_eq!(run.r.gpu_status(), GpuStatus::Unused, "{name}");
        assert!(run.r.apply(moved(node, 10.0)).is_empty());
        run.settle(1100);
        assert!(run.r.gpu_in_demand(), "{name}: visible, wanted");
        assert!(matches!(run.r.gpu_status(), GpuStatus::Up(_)), "{name}");
        assert!(run.r.apply(moved(node, 1000.0)).is_empty());
        assert!(run.frame(1200).is_empty(), "{name}");
        assert!(!run.r.gpu_in_demand(), "{name}: hidden again");
        let wake = run.r.next_wake().expect("render wakes to drop the device");
        std::thread::sleep(
            wake.saturating_duration_since(Instant::now()) + Duration::from_millis(5),
        );
        run.r.update();
        assert_eq!(
            run.r.take_backend_changes(),
            [BackendChange::Drop],
            "{name}: released after idle"
        );
        assert_eq!(run.r.gpu_status(), GpuStatus::Unused, "{name}");
        drop(run.host.gpu.take());
    }
}

/// `node` moved to `x` at once.
fn moved(node: NodeId, x: f32) -> SceneDiff {
    let mut d = SceneDiff::new();
    d.push(SceneOp::SetProp {
        id: node,
        prop: Prop::X,
        value: num(x),
        transition: Transition::Instant,
    });
    d
}

/// A file that paints red within 4 px of `strand.pointer`.
fn pointer_code() -> Arc<ShaderCode> {
    Arc::new(ShaderCode {
        path: "pointer.wgsl".into(),
        wgsl: "@group(1) @binding(0) var<uniform> u_unused: f32;\n\
               @fragment\n\
               fn main(v: StrandVertex) -> @location(0) vec4<f32> {\n\
                   if (distance(v.pos.xy, strand.pointer) < 4.0) {\n\
                       return vec4<f32>(1.0, 0.0, 0.0, 1.0);\n\
                   }\n\
                   return vec4<f32>(0.0);\n\
               }\n"
        .into(),
        uniforms: ShaderCode::packed(vec![("u_unused".into(), UniformType::F32, 0)]),
    })
}

/// A `shader` node's `strand.pointer` is the pointer over its box (in
/// its buffer pixels), and a pointer motion repaints it.
#[test]
fn a_shader_follows_the_pointer() {
    let mut p = at_xy(10.0, 10.0, 60.0, 30.0);
    p.push((Prop::Shader, PropValue::Shader(pointer_code())));
    let Some(mut run) = run(vec![(NodeKind::Shader, p, vec![])], (240, 60)) else {
        return;
    };
    run.r
        .set_pointer(S, Some(LogicalPoint { x: 20.0, y: 20.0 }));
    run.settle(1000);
    assert_eq!(run.rgb(20, 20), [255, 0, 0], "under the pointer");
    assert_ne!(run.rgb(50, 20), [255, 0, 0]);
    run.r
        .set_pointer(S, Some(LogicalPoint { x: 50.0, y: 20.0 }));
    assert!(run.r.wants_frame(S), "a pointer motion repaints it");
    run.settle(1100);
    assert_eq!(run.rgb(50, 20), [255, 0, 0], "it followed");
    assert_ne!(run.rgb(20, 20), [255, 0, 0]);
}

/// A file that paints `u_tint`.
fn tint_code() -> Arc<ShaderCode> {
    Arc::new(ShaderCode {
        path: "tint.wgsl".into(),
        wgsl: "@group(1) @binding(0) var<uniform> u_tint: vec4<f32>;\n\
               @fragment\n\
               fn main(v: StrandVertex) -> @location(0) vec4<f32> {\n\
                   return u_tint;\n\
               }\n"
        .into(),
        uniforms: ShaderCode::packed(vec![("u_tint".into(), UniformType::Vec4, 0)]),
    })
}

/// A shader's uniforms spring like any animated prop (design.md: "a
/// `u_*` uniform animates like any prop"): a new tint is reached through
/// frames whose passes draw the colours between, then the pass stops
/// being asked for.
#[test]
fn shader_uniforms_spring() {
    let tint = |c: &str| PropValue::Uniforms(vec![("u_tint".into(), color(c))]);
    let mut p = at_xy(10.0, 10.0, 40.0, 20.0);
    p.extend([
        (Prop::Shader, PropValue::Shader(tint_code())),
        (Prop::Uniforms, tint("#ff0000")),
    ]);
    let Some(mut run) = run(vec![(NodeKind::Shader, p, vec![])], (240, 60)) else {
        return;
    };
    run.settle(1000);
    assert_eq!(run.rgb(20, 20), [255, 0, 0]);
    let mut d = SceneDiff::new();
    d.set(run.ids[0], Prop::Uniforms, tint("#0000ff"));
    assert!(run.r.apply(d).is_empty());
    let mut t = 1000;
    let mut between = 0;
    run.r.update();
    while t == 1000 || run.r.wants_frame(S) {
        t += 16;
        assert!(t < 5000, "settles");
        run.settle(t);
        let [r, _, b] = run.rgb(20, 20);
        if r > 10 && b > 10 {
            between += 1;
        }
    }
    assert!(between >= 3, "frames between red and blue: {between}");
    assert_eq!(run.rgb(20, 20), [0, 0, 255], "the new tint");
    assert!(run.frame(t + 16).is_empty(), "settled: no more passes");
}
