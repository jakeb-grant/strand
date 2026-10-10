//! (M4) The GPU backend as render drives it (architecture.md,
//! "`strand-gpu`"): promotion and the device's lifecycle
//! ([`crate::promote`]), lowering a surface's display list to
//! [`strand_gpu::Frame`] ops, `shader` nodes' passes, and the replies.
//!
//! Render never owns the [`strand_gpu::Gpu`]: it queues requests
//! ([`Renderer::take_gpu_requests`]) and backend changes
//! ([`Renderer::take_backend_changes`]) for the host, which starts the
//! `Gpu` when requests come and none runs, drops it on
//! [`BackendChange::Drop`], and hands every reply to
//! [`Renderer::deliver_gpu`].
//!
//! - **Lowering.** Fills, borders, clips, opacity and transforms lower to
//!   ops, gradients to peniko gradients in straight colours. Pixmaps
//!   (images, CPU raster nodes, glyph atlas pages, shadows) are uploads,
//!   keyed by the pixmap they are and retired once nothing draws them.
//!   What the GPU cannot do is drawn on the CPU and uploaded as one
//!   pixmap: masked layers (masks are always rasterised on the CPU), and
//!   groups the CPU draws offscreen (blur, colour matrix), whose
//!   filtered pixels are used as they are, so both backends show the
//!   same group.
//! - **Readback surfaces.** A promoted surface in `GpuReadback` mode is
//!   lowered and sent each frame its scene changes; the next paint copies
//!   the pixels in with full damage (one frame behind, so the main thread
//!   never waits for the GPU). Until pixels come (the first frame, a frame
//!   still in flight past [`GPU_WAIT`]) the CPU draws it.
//! - **`shader` nodes.** On a surface the GPU does not draw, each pass is
//!   drawn offscreen at its box and read back into the CPU frame as a
//!   raster item. A pass whose inputs changed is asked for once at a
//!   time per node; the frame holds for it up to [`GPU_WAIT`] (through
//!   `frame_deadline`) unless it is clocked (`strand.time`), whose passes
//!   are pipelined a frame behind instead. With no device a `shader` node
//!   keeps its box and draws nothing.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use strand_gpu::{
    Brush, Frame, GpuMode, GpuReply, GpuRequest, Op, PassFrame, PassGlobals, Readback, Upload,
};
use strand_scene::shader::ShaderCode;
use strand_scene::{
    Backend, BackendChange, Color, Damage, GpuStatus, GradientStop, Length, NodeId, Paint,
    PaintTarget, PropValue, Rect, Scale, ShaderInput, ShaderPass, ShaderRef, Size, SurfaceId,
};
use vello_cpu::color::{AlphaColor, ColorSpaceTag, Srgb};
use vello_cpu::kurbo::{self, Affine, Shape};
use vello_cpu::peniko::{Extend, Gradient};
use vello_cpu::{
    Pixmap, RasterizerSettings, RenderContext, RenderMode, RenderSettings, Resources, TargetInit,
};

use super::Renderer;
use crate::cache::{PaintCache, ShadowShape};
use crate::flatten::{DisplayItem, FillShape, Item};
use crate::offscreen::{Drawn, layer_key};
use crate::promote::{Device, Promotion, Switch};
use crate::raster::AtlasMirror;

/// How long a frame holds for a pass or a readback before drawing what
/// it has.
pub const GPU_WAIT: Duration = Duration::from_millis(8);

/// Lowerings after which an upload nothing drew is retired.
const RETIRE_AFTER: u64 = 600;

/// Largest side of a pass, an island or a readback, in pixels.
const MAX_SIDE: u32 = 8192;

// ---------------------------------------------------------------------------
// Shader nodes

/// A node as a pass's key, and back.
fn node_key(n: NodeId) -> u64 {
    (u64::from(n.index) << 32) | u64::from(n.generation)
}

fn key_node(k: u64) -> NodeId {
    NodeId::new((k >> 32) as u32, k as u32)
}

/// A `shader` node's pass as one frame wants it.
#[derive(Clone, Debug)]
pub(crate) struct PassWant {
    pub node: NodeId,
    /// What the pass's pixels depend on: code, uniforms, size, time.
    pub key: u64,
    pub size: Size,
    pub pass: ShaderPass,
    pub globals: PassGlobals,
    /// It reads `strand.time`: its passes are pipelined, never held for.
    pub timed: bool,
}

/// The last pixels each `shader` node's pass gave, with the key of the
/// want they answer.
#[derive(Debug, Default)]
pub struct ShaderResults {
    map: HashMap<NodeId, (u64, Arc<Pixmap>)>,
}

impl ShaderResults {
    pub(crate) fn get(&self, node: NodeId) -> Option<&(u64, Arc<Pixmap>)> {
        self.map.get(&node)
    }
}

/// A transparent pixel: what a `shader` node with no pixels yet draws.
pub(crate) fn empty_pixmap() -> Arc<Pixmap> {
    static EMPTY: std::sync::OnceLock<Arc<Pixmap>> = std::sync::OnceLock::new();
    EMPTY.get_or_init(|| Arc::new(Pixmap::new(1, 1))).clone()
}

/// True if `code` reads Strand's clock (its passes change every frame).
pub(crate) fn reads_time(code: &ShaderCode) -> bool {
    code.wgsl.contains("strand.time")
}

/// The pass a `shader` node draws at `w × h` buffer pixels, its `u_*`
/// values packed in its slots' order.
pub(crate) fn pass_want(
    node: NodeId,
    code: &Arc<ShaderCode>,
    uniforms: Option<&PropValue>,
    w: u32,
    h: u32,
    scale: f32,
    time: f32,
) -> Option<PassWant> {
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
        return None;
    }
    let entries: &[(String, PropValue)] = match uniforms {
        Some(PropValue::Uniforms(e)) => e,
        _ => &[],
    };
    let packed = pack(code, entries, scale);
    let timed = reads_time(code);
    let time = if timed && time.is_finite() { time } else { 0.0 };
    let mut k = std::collections::hash_map::DefaultHasher::new();
    {
        use std::hash::{Hash, Hasher};
        code.hash(&mut k);
        for v in &packed {
            v.to_bits().hash(&mut k);
        }
        (w, h, scale.to_bits(), time.to_bits()).hash(&mut k);
        let _ = k.finish();
    }
    let key = std::hash::Hasher::finish(&k);
    Some(PassWant {
        node,
        key,
        size: Size::new(w, h),
        pass: ShaderPass {
            code: ShaderRef::File(code.clone()),
            uniforms: packed.into(),
            input: ShaderInput::None,
        },
        globals: PassGlobals {
            time,
            scale,
            pointer: [-1.0, -1.0],
        },
        timed,
    })
}

/// `entries` in `code`'s slot order, in buffer units (architecture.md,
/// the ABI): lengths × `scale`, angles in radians, durations in
/// seconds, colours premultiplied linear. A slot no entry sets is zero.
pub(crate) fn pack(code: &ShaderCode, entries: &[(String, PropValue)], scale: f32) -> Vec<f32> {
    let mut out = vec![0.0; code.uniform_floats() as usize];
    for slot in &code.uniforms {
        let Some((_, v)) = entries.iter().find(|(n, _)| *n == slot.name) else {
            continue;
        };
        let mut floats = Vec::with_capacity(4);
        value_floats(v, scale, &mut floats);
        let n = slot.ty.floats() as usize;
        let at = slot.offset as usize;
        for (i, f) in floats.into_iter().take(n).enumerate() {
            if let Some(o) = out.get_mut(at + i) {
                *o = if f.is_finite() { f } else { 0.0 };
            }
        }
    }
    out
}

fn value_floats(v: &PropValue, scale: f32, out: &mut Vec<f32>) {
    match v {
        PropValue::Number(n) => out.push(*n),
        PropValue::Length(Length::Px(n)) => out.push(n * scale),
        PropValue::Length(Length::Percent(n)) => out.push(n / 100.0),
        PropValue::Length(Length::Ch(n)) => out.push(*n),
        PropValue::Length(Length::Auto) => out.push(0.0),
        PropValue::Angle(deg) => out.push(deg.to_radians()),
        PropValue::Duration(d) => out.push(d.as_secs_f32()),
        PropValue::Color(c) => {
            let l = c.clamped().to_linear();
            let a = l.alpha as f32;
            out.extend([l.r as f32 * a, l.g as f32 * a, l.b as f32 * a, a]);
        }
        PropValue::List(items) => {
            for i in items {
                value_floats(i, scale, out);
            }
        }
        PropValue::Bool(b) => out.push(if *b { 1.0 } else { 0.0 }),
        _ => out.push(0.0),
    }
}

// ---------------------------------------------------------------------------
// State

/// One surface as the GPU backend sees it.
#[derive(Debug, Default)]
struct Surf {
    promo: Promotion,
    /// What draws it.
    backend: Backend,
    /// Its readback frame in flight, and since when.
    inflight: Option<(u64, Instant)>,
    /// Pixels of its last readback frame, not copied in yet.
    pixels: Option<Readback>,
    /// Until when its next frame holds (a pass or readback in flight).
    hold: Option<Instant>,
}

/// One `shader` node's pass in flight.
#[derive(Debug)]
struct Pending {
    frame: u64,
    key: u64,
    surface: SurfaceId,
}

/// The GPU backend's state in the renderer.
#[derive(Debug, Default)]
pub(super) struct GpuState {
    device: Device,
    status: GpuStatus,
    surfaces: HashMap<SurfaceId, Surf>,
    changes: Vec<BackendChange>,
    out: Vec<GpuRequest>,
    uploads: Uploads,
    pending: HashMap<NodeId, Pending>,
    next_frame: u64,
    /// A `GpuPresent` frame `paint` lowered, for [`Renderer::paint_gpu`].
    present: Option<Frame>,
    /// Readback frames copied in (tests).
    copied: u64,
    /// The pixels `paint_gpu`'s target points at: never written (a
    /// present frame is lowered, not rasterised), so never resident.
    scratch: Vec<u8>,
    /// A `shader` node was drawn by the last frame of some surface.
    demand: HashSet<SurfaceId>,
    /// Presented surfaces crossfading: their snapshot as an upload,
    /// with the snapshot's address it was copied from.
    fades: HashMap<SurfaceId, (usize, Arc<Pixmap>)>,
}

impl GpuState {
    fn frame_id(&mut self) -> u64 {
        self.next_frame += 1;
        self.next_frame
    }

    /// Asks for the device at `now`: true if it may be used.
    fn want(&mut self, now: Instant) -> bool {
        let start = self.device.idle();
        let ok = self.device.want(now);
        if ok && start {
            self.status = GpuStatus::Starting;
        }
        ok
    }

    fn promoted(&self) -> bool {
        self.surfaces.values().any(|s| s.promo.on_gpu())
    }

    /// The device is gone (failed, lost, dropped): every promoted surface
    /// goes back to the CPU at once, and what was in flight is forgotten.
    fn fall_back(&mut self) -> Vec<SurfaceId> {
        let mut back = Vec::new();
        for (id, s) in self.surfaces.iter_mut() {
            if s.promo.on_gpu() || s.backend.is_gpu() {
                s.promo.force_cpu();
                s.backend = Backend::Cpu;
                back.push(*id);
            }
            s.inflight = None;
            s.pixels = None;
            s.hold = None;
        }
        for id in &back {
            self.changes.push(BackendChange::Demote(*id));
        }
        self.pending.clear();
        self.uploads.clear();
        self.fades.clear();
        back
    }
}

impl Renderer {
    /// (M4) Requests for the GPU thread since the last call. When some
    /// come and no `Gpu` runs, the host starts one and sends them (its
    /// channel keeps them until the device is up).
    pub fn take_gpu_requests(&mut self) -> Vec<GpuRequest> {
        std::mem::take(&mut self.gpu.out)
    }

    /// (M4) What the host must do about the GPU: attach a surface
    /// (`Promote`), release one (`Demote`), drop the `Gpu` (`Drop`).
    ///
    /// The host calls it after every dispatch, so it also re-arms the
    /// renderer's timer: a reply or a surface detached since the last
    /// paint can bring the device's drop forward.
    pub fn take_backend_changes(&mut self) -> Vec<BackendChange> {
        self.arm_timer();
        std::mem::take(&mut self.gpu.changes)
    }

    /// (M4) Why the GPU is or is not drawing.
    pub fn gpu_status(&self) -> GpuStatus {
        self.gpu.status.clone()
    }

    /// (M4) True while a frame shows a `shader` node (logic hears the
    /// status only then).
    pub fn gpu_in_demand(&self) -> bool {
        !self.gpu.demand.is_empty()
    }

    /// (M4) Readback frames copied into `wl_shm` buffers so far (tests).
    #[doc(hidden)]
    pub fn gpu_frames_copied(&self) -> u64 {
        self.gpu.copied
    }

    /// (M4) Promotes `surface` now, as 500 ms of heavy frames would
    /// (tests: promotion's timing is `promote.rs`'s).
    #[doc(hidden)]
    pub fn promote_now(&mut self, surface: SurfaceId) {
        let now = Instant::now();
        if self.gpu.want(now) {
            self.gpu
                .surfaces
                .entry(surface)
                .or_default()
                .promo
                .force_gpu();
            self.gpu.changes.push(BackendChange::Promote(surface));
        }
    }

    /// (M4) How long the device outlives its last use (30 s; tests
    /// shorten it).
    #[doc(hidden)]
    pub fn set_gpu_idle(&mut self, idle: Duration) {
        self.gpu.device.set_idle(idle);
    }

    /// (M4) What draws `surface`.
    pub fn backend(&self, surface: SurfaceId) -> Backend {
        self.gpu
            .surfaces
            .get(&surface)
            .map_or(Backend::Cpu, |s| s.backend)
    }

    /// (M4) `surface` is drawn by `backend` from its next frame (the host
    /// tells it once the GPU thread attached it, or took it back). A
    /// switch repaints in full.
    pub fn set_backend(&mut self, surface: SurfaceId, backend: Backend) {
        let s = self.gpu.surfaces.entry(surface).or_default();
        if s.backend == backend {
            return;
        }
        s.backend = backend;
        s.inflight = None;
        s.pixels = None;
        s.hold = None;
        self.gpu.fades.remove(&surface);
        if backend.is_gpu() {
            if !s.promo.on_gpu() {
                // Attached for a promotion that has since been undone.
                s.backend = Backend::Cpu;
                self.gpu.changes.push(BackendChange::Demote(surface));
                return;
            }
        } else {
            s.promo.force_cpu();
        }
        if let Some(st) = self.surfaces.get_mut(&surface) {
            st.valid = false;
            st.mark_dirty();
        }
    }

    /// (M4) A reply from the GPU thread.
    pub fn deliver_gpu(&mut self, reply: GpuReply) {
        let now = Instant::now();
        match reply {
            GpuReply::Ready(info) => {
                self.gpu.device.up();
                self.gpu.status = GpuStatus::Up(info);
                // Passes wanted while it started.
                for s in self.surfaces.values_mut() {
                    s.mark_dirty();
                }
            }
            GpuReply::Unavailable(e) | GpuReply::Lost(e) => {
                log::warn!("GPU: {e}");
                self.gpu.device.failed(now);
                self.gpu.status = GpuStatus::Unavailable { reason: e.message };
                self.repaint_fallen();
            }
            GpuReply::Exited => {
                if self.gpu.device.is_up() || self.gpu.device.starting() {
                    self.gpu.device.dropped();
                    self.gpu.status = GpuStatus::Unused;
                }
                self.repaint_fallen();
            }
            GpuReply::Attached { surface, mode } => {
                let backend = match mode {
                    GpuMode::Present => Backend::GpuPresent,
                    GpuMode::Readback => Backend::GpuReadback,
                };
                self.set_backend(surface, backend);
            }
            GpuReply::Released(surface) => self.set_backend(surface, Backend::Cpu),
            GpuReply::Presented { .. } => self.gpu.device.used(now),
            GpuReply::Pixels {
                surface,
                frame,
                pixels,
            } => {
                self.gpu.device.used(now);
                if let Some(s) = self.gpu.surfaces.get_mut(&surface)
                    && s.inflight.is_some_and(|(f, _)| f == frame)
                {
                    s.inflight = None;
                    s.hold = None;
                    if s.backend == Backend::GpuReadback {
                        s.pixels = Some(pixels);
                        // Copied in by the next paint, in full.
                        if let Some(st) = self.surfaces.get_mut(&surface) {
                            st.valid = false;
                            st.mark_dirty();
                        }
                    }
                }
            }
            GpuReply::PassPixels { key, frame, pixels } => {
                self.gpu.device.used(now);
                let node = key_node(key);
                let Some(p) = self.gpu.pending.remove(&node).filter(|p| p.frame == frame) else {
                    return;
                };
                if let Some(pm) = readback_pixmap(&pixels) {
                    self.extras.shaders.map.insert(node, (p.key, Arc::new(pm)));
                }
                if let Some(s) = self.gpu.surfaces.get_mut(&p.surface) {
                    s.hold = None;
                }
                self.mark_node_dirty(node);
            }
            GpuReply::Failed {
                surface,
                key,
                error,
            } => {
                log::warn!("GPU: {error}");
                if let Some(node) = key.map(key_node) {
                    // Not asked again until its inputs change: the result
                    // records the failed want with no pixels.
                    if let Some(p) = self.gpu.pending.remove(&node) {
                        self.extras
                            .shaders
                            .map
                            .insert(node, (p.key, empty_pixmap()));
                        if let Some(s) = self.gpu.surfaces.get_mut(&p.surface) {
                            s.hold = None;
                        }
                    }
                    self.mark_node_dirty(node);
                }
                if let Some(surface) = surface
                    && let Some(s) = self.gpu.surfaces.get_mut(&surface)
                {
                    // A frame that cannot be drawn: back to the CPU.
                    s.inflight = None;
                    s.hold = None;
                    if s.promo.on_gpu() {
                        s.promo.force_cpu();
                        s.backend = Backend::Cpu;
                        self.gpu.changes.push(BackendChange::Demote(surface));
                    }
                    if let Some(st) = self.surfaces.get_mut(&surface) {
                        st.valid = false;
                        st.mark_dirty();
                    }
                }
            }
        }
    }

    /// After the device went: promoted surfaces back on the CPU, repainted
    /// in full.
    fn repaint_fallen(&mut self) {
        for id in self.gpu.fall_back() {
            if let Some(st) = self.surfaces.get_mut(&id) {
                st.valid = false;
                st.mark_dirty();
            }
        }
        for s in self.surfaces.values_mut() {
            s.mark_dirty();
        }
    }

    /// Until when `surface`'s next frame holds for the GPU.
    pub(super) fn gpu_hold(&self, surface: SurfaceId) -> Option<Instant> {
        self.gpu.surfaces.get(&surface).and_then(|s| s.hold)
    }

    /// When the loop must wake for the GPU: an idle promoted surface's
    /// demotion, or the device's drop.
    pub(super) fn gpu_wake(&self) -> Option<Instant> {
        let promos = self
            .gpu
            .surfaces
            .values()
            .filter_map(|s| s.promo.wake())
            .min();
        // A detached surface is forgotten at the next tick: it keeps
        // nothing up.
        let live = &self.surfaces;
        let promoted = self
            .gpu
            .surfaces
            .iter()
            .any(|(id, s)| s.promo.on_gpu() && live.contains_key(id));
        let pending = self
            .gpu
            .pending
            .values()
            .any(|p| live.contains_key(&p.surface));
        let drop = if promoted || pending {
            None
        } else {
            self.gpu.device.drop_at()
        };
        [promos, drop].into_iter().flatten().min()
    }

    /// The loop woke at `now`: idle promoted surfaces go back, and the
    /// device goes 30 s after its last use.
    pub(super) fn gpu_tick(&mut self, now: Instant) {
        let mut back = Vec::new();
        for (id, s) in self.gpu.surfaces.iter_mut() {
            if s.promo.idle(now) == Some(Switch::ToCpu) {
                back.push(*id);
            }
        }
        for id in back {
            self.demote(id);
        }
        // Surfaces that went away.
        let live = &self.surfaces;
        self.gpu.surfaces.retain(|id, _| live.contains_key(id));
        self.gpu.demand.retain(|id| live.contains_key(id));
        self.gpu.fades.retain(|id, _| live.contains_key(id));
        // Passes of surfaces that went away: their pixels have nowhere
        // to go.
        self.gpu
            .pending
            .retain(|_, p| live.contains_key(&p.surface));
        if !self.gpu.promoted() && self.gpu.pending.is_empty() && self.gpu.device.due(now) {
            self.gpu.device.dropped();
            self.gpu.status = GpuStatus::Unused;
            self.gpu.changes.push(BackendChange::Drop);
            self.gpu.uploads.clear();
            self.extras.shaders.map.clear();
            for s in self.gpu.surfaces.values_mut() {
                s.inflight = None;
                s.pixels = None;
                s.hold = None;
            }
        }
    }

    fn demote(&mut self, id: SurfaceId) {
        let Some(s) = self.gpu.surfaces.get_mut(&id) else {
            return;
        };
        s.promo.force_cpu();
        self.gpu.changes.push(BackendChange::Demote(id));
        if s.backend == Backend::GpuReadback {
            // Nothing to hand back: the CPU draws its next frame.
            s.backend = Backend::Cpu;
            s.inflight = None;
            s.pixels = None;
            s.hold = None;
            if let Some(st) = self.surfaces.get_mut(&id) {
                st.valid = false;
                st.mark_dirty();
            }
        }
    }

    /// A frame of `surface` damaged `damage` pixels with `springs` in
    /// flight: promotion's input.
    pub(super) fn gpu_frame_stats(&mut self, surface: SurfaceId, damage: u64, springs: bool) {
        let now = Instant::now();
        let s = self.gpu.surfaces.entry(surface).or_default();
        match s.promo.frame(now, damage, springs) {
            Some(Switch::ToGpu) => {
                if self.gpu.want(now) {
                    self.gpu.changes.push(BackendChange::Promote(surface));
                } else if let Some(s) = self.gpu.surfaces.get_mut(&surface) {
                    s.promo.force_cpu();
                }
            }
            Some(Switch::ToCpu) => self.demote(surface),
            None => {}
        }
    }

    /// The passes a frame of `surface` wants: those whose pixels are not
    /// its node's last are asked for (one in flight per node). `hold`: the
    /// frame is not painted yet, so it may wait for them.
    pub(super) fn gpu_passes(&mut self, surface: SurfaceId, wants: &[PassWant], hold: bool) {
        if wants.is_empty() {
            self.gpu.demand.remove(&surface);
            return;
        }
        self.gpu.demand.insert(surface);
        let now = Instant::now();
        // A promoted surface's passes run in its GPU frames.
        if self.backend(surface).is_gpu() {
            self.gpu.device.used(now);
            return;
        }
        let mut wait = false;
        for w in wants {
            let have = self.extras.shaders.get(w.node).map(|(k, _)| *k);
            if have == Some(w.key) || self.gpu.pending.contains_key(&w.node) {
                continue;
            }
            if !self.gpu.want(now) {
                // No device (a failure under 30 s old): nothing to draw.
                continue;
            }
            let frame = self.gpu.frame_id();
            self.gpu.out.push(GpuRequest::Pass(PassFrame {
                key: node_key(w.node),
                id: frame,
                size: w.size,
                pass: w.pass.clone(),
                globals: w.globals,
            }));
            self.gpu.pending.insert(
                w.node,
                Pending {
                    frame,
                    key: w.key,
                    surface,
                },
            );
            // Only an up device answers within the wait; a clocked pass
            // is pipelined, and a node showing pixels keeps them meanwhile
            // only if it is clocked.
            if self.gpu.device.is_up() && (have.is_none() || !w.timed) {
                wait = true;
            }
        }
        if wait && hold {
            let s = self.gpu.surfaces.entry(surface).or_default();
            s.hold = Some(now + GPU_WAIT);
        }
        // Results of nodes no longer drawn go.
        let pending = &self.gpu.pending;
        let tree = &self.tree;
        self.extras
            .shaders
            .map
            .retain(|id, _| pending.contains_key(id) || tree.get(*id).is_some());
    }

    /// Paints a frame of a `GpuReadback` surface: copies the last pixels
    /// in (`Some`: full damage) and sends this frame when its scene
    /// changed (`changed`); `None` when the CPU must draw it (no pixels
    /// yet, or a frame still in flight past [`GPU_WAIT`]).
    pub(super) fn gpu_readback_paint(
        &mut self,
        surface: SurfaceId,
        items: &[DisplayItem],
        passes: &[PassWant],
        changed: bool,
        target: &mut PaintTarget<'_>,
    ) -> Option<Damage> {
        let now = Instant::now();
        let ready = self.gpu.surfaces.get_mut(&surface)?.pixels.take();
        let inflight = self.gpu.surfaces.get(&surface)?.inflight;
        let fresh = ready.is_some() || inflight.is_none();
        if fresh && (changed || ready.is_none()) {
            let id = self.gpu.frame_id();
            let frame = self.lower_frame(surface, id, items, passes, target.size, target.scale);
            self.gpu.out.push(GpuRequest::Frame(frame));
            self.gpu.device.used(now);
            if let Some(s) = self.gpu.surfaces.get_mut(&surface) {
                s.inflight = Some((id, now));
                s.hold = Some(now + GPU_WAIT);
            }
        }
        let px = ready?;
        self.gpu.copied += 1;
        let stride = target.stride as usize;
        px.copy_into(target.pixels, stride, target.size, 0, 0);
        Some(Damage::full(target.size))
    }

    /// (M4) The next frame of a `GpuPresent` surface, at `at` (the last
    /// present plus the output's refresh period), if it wants one: the
    /// host sends it to the GPU thread, which presents it. It is painted
    /// as a CPU frame would be (springs, clocks, damage bookkeeping), and
    /// lowered where the CPU would rasterise.
    pub fn paint_gpu(&mut self, surface: SurfaceId, at: Duration) -> Option<Frame> {
        use strand_scene::Painter;
        if self.backend(surface) != Backend::GpuPresent || !self.wants(surface) {
            return None;
        }
        let (size, scale) = {
            let s = self.surfaces.get(&surface)?;
            (s.size, s.scale)
        };
        if size.is_empty() {
            return None;
        }
        let stride = size.w * 4;
        let len = stride as usize * size.h as usize;
        let mut scratch = std::mem::take(&mut self.gpu.scratch);
        if scratch.len() != len {
            // Zeroed by the allocator and never touched: no pages.
            scratch = vec![0u8; len];
        }
        let mut target = PaintTarget {
            pixels: &mut scratch,
            size,
            stride,
            scale,
            age: 0,
            time: at,
        };
        self.gpu.present = None;
        let _ = self.paint(surface, &mut target);
        self.gpu.scratch = scratch;
        let frame = self.gpu.present.take();
        if frame.is_some() {
            self.gpu.device.used(Instant::now());
        }
        frame
    }

    /// In `paint` of a `GpuPresent` surface: lowers the frame for
    /// [`Renderer::paint_gpu`] instead of rasterising it.
    ///
    /// `fade`: the new frame's weight in a theme crossfade. The frame
    /// then draws the snapshot weighted `1 - fade` and adds the new
    /// frame weighted `fade` over it (a `Plus` layer), which is the CPU's
    /// blend of the two (`swap::blend`), done on the GPU.
    pub(super) fn gpu_present_paint(
        &mut self,
        surface: SurfaceId,
        items: &[DisplayItem],
        passes: &[PassWant],
        size: Size,
        scale: Scale,
        fade: Option<f32>,
    ) -> Damage {
        let id = self.gpu.frame_id();
        let mut frame = self.lower_frame(surface, id, items, passes, size, scale);
        match fade.and_then(|w| Some((w, self.fade_pixmap(surface, size)?))) {
            Some((w, pm)) => {
                let image = self.gpu.uploads.id(&pm, &mut frame.uploads);
                frame.ops = crossfade_ops(std::mem::take(&mut frame.ops), image, size, w);
            }
            None => {
                self.gpu.fades.remove(&surface);
            }
        }
        self.gpu.present = Some(frame);
        Damage::full(size)
    }

    /// `surface`'s crossfade snapshot as a pixmap to upload, copied once
    /// per snapshot.
    fn fade_pixmap(&mut self, surface: SurfaceId, size: Size) -> Option<Arc<Pixmap>> {
        let px = self.fade_pixels(surface, size)?;
        let at = px.as_ptr() as usize;
        if let Some((a, pm)) = self.gpu.fades.get(&surface)
            && *a == at
        {
            return Some(pm.clone());
        }
        let (w, h) = (u16::try_from(size.w).ok()?, u16::try_from(size.h).ok()?);
        let mut pm = Pixmap::new(w, h);
        let bytes = pm.data_as_u8_slice_mut();
        if bytes.len() != px.len() {
            return None;
        }
        // The CPU raster's bytes are an upload's order already.
        bytes.copy_from_slice(px);
        let pm = Arc::new(pm);
        self.gpu.fades.insert(surface, (at, pm.clone()));
        Some(pm)
    }

    /// Lowers a display list to a GPU frame.
    fn lower_frame(
        &mut self,
        surface: SurfaceId,
        id: u64,
        items: &[DisplayItem],
        passes: &[PassWant],
        size: Size,
        scale: Scale,
    ) -> Frame {
        let bounds = Rect::from_size(size);
        let (cache, offscreen) = self.raster.gpu_parts();
        // Groups the CPU filters (blur, colour matrix), drawn whole.
        offscreen.prepare(
            items,
            &Damage::full(size),
            bounds,
            &self.atlas,
            &*cache,
            scale,
        );
        let passes: HashMap<NodeId, &PassWant> = passes.iter().map(|p| (p.node, p)).collect();
        let mut l = Lowering {
            atlas: &self.atlas,
            cache,
            groups: offscreen.current(),
            scale,
            surface: bounds,
            uploads: &mut self.gpu.uploads,
            passes: &passes,
            ops: Vec::new(),
            new: Vec::new(),
        };
        l.uploads.begin();
        l.lower(items);
        let (ops, uploads) = (l.ops, l.new);
        let retire = self.gpu.uploads.retire();
        Frame {
            surface,
            id,
            size,
            scale,
            ops,
            uploads,
            retire,
            clear: AlphaColor::TRANSPARENT,
        }
    }
}

/// A crossfade frame's ops: upload `image` (the snapshot, `size`)
/// weighted `1 - w`, and the new frame's `ops` added over it weighted
/// `w`. Both start at the identity transform.
fn crossfade_ops(ops: Vec<Op>, image: u64, size: Size, w: f32) -> Vec<Op> {
    use vello_cpu::peniko::{BlendMode, Compose, Mix};
    let w = if w.is_finite() {
        w.clamp(0.0, 1.0)
    } else {
        1.0
    };
    let rect = kurbo::Rect::new(0.0, 0.0, size.w as f64, size.h as f64);
    let mut out = Vec::with_capacity(ops.len() + 7);
    out.push(Op::Transform(Affine::IDENTITY));
    out.push(Op::PushLayer(strand_gpu::Layer {
        opacity: Some(1.0 - w),
        ..Default::default()
    }));
    out.push(Op::Image {
        rect,
        image,
        image_transform: Affine::IDENTITY,
        tint: None,
        smooth: false,
    });
    out.push(Op::PopLayer);
    out.push(Op::PushLayer(strand_gpu::Layer {
        blend: Some(BlendMode::new(Mix::Normal, Compose::Plus)),
        opacity: Some(w),
        ..Default::default()
    }));
    out.extend(ops);
    out.push(Op::PopLayer);
    out
}

/// Readback rows (BGRA bytes) as a pixmap in the CPU raster's order
/// (red and blue swapped is exactly BGRA in memory).
fn readback_pixmap(px: &Readback) -> Option<Pixmap> {
    let (w, h) = (
        u16::try_from(px.width).ok()?,
        u16::try_from(px.height).ok()?,
    );
    if w == 0 || h == 0 {
        return None;
    }
    let mut pm = Pixmap::new(w, h);
    let row = px.width as usize * 4;
    let bytes = pm.data_as_u8_slice_mut();
    for y in 0..px.height as usize {
        let src = px.row(y as u32);
        bytes
            .get_mut(y * row..(y + 1) * row)?
            .copy_from_slice(src.get(..row)?);
    }
    Some(pm)
}

// ---------------------------------------------------------------------------
// Uploads

/// The pixmaps the GPU holds, by the pixmap they are.
#[derive(Debug, Default)]
pub(super) struct Uploads {
    by_ptr: HashMap<usize, Held>,
    next: u64,
    lowering: u64,
}

#[derive(Debug)]
struct Held {
    pixmap: Weak<Pixmap>,
    id: u64,
    used: u64,
}

impl Uploads {
    fn begin(&mut self) {
        self.lowering += 1;
    }

    /// The upload id of `pm`, queuing it in `new` if the GPU does not
    /// have it.
    fn id(&mut self, pm: &Arc<Pixmap>, new: &mut Vec<Upload>) -> u64 {
        let ptr = Arc::as_ptr(pm) as usize;
        let lowering = self.lowering;
        if let Some(h) = self.by_ptr.get_mut(&ptr)
            && h.pixmap.upgrade().is_some_and(|p| Arc::ptr_eq(&p, pm))
        {
            h.used = lowering;
            return h.id;
        }
        self.next += 1;
        let id = self.next;
        if let Some(old) = self.by_ptr.insert(
            ptr,
            Held {
                pixmap: Arc::downgrade(pm),
                id,
                used: lowering,
            },
        ) {
            // The same address holds new pixels (the old pixmap went):
            // retired below with the next sweep.
            self.by_ptr.insert(
                usize::MAX - old.id as usize,
                Held {
                    pixmap: Weak::new(),
                    id: old.id,
                    used: 0,
                },
            );
        }
        new.push(Upload {
            id,
            generation: 0,
            pixmap: pm.clone(),
        });
        id
    }

    /// Uploads whose pixmap is gone, or that nothing drew for
    /// [`RETIRE_AFTER`] lowerings.
    fn retire(&mut self) -> Vec<u64> {
        let now = self.lowering;
        let mut out = Vec::new();
        self.by_ptr.retain(|_, h| {
            let keep = h.pixmap.strong_count() > 0 && now.saturating_sub(h.used) < RETIRE_AFTER;
            if !keep {
                out.push(h.id);
            }
            keep
        });
        out
    }

    fn clear(&mut self) {
        self.by_ptr.clear();
    }
}

// ---------------------------------------------------------------------------
// Lowering

struct Lowering<'a> {
    atlas: &'a AtlasMirror,
    cache: &'a mut PaintCache,
    groups: &'a HashMap<usize, Drawn>,
    scale: Scale,
    surface: Rect,
    uploads: &'a mut Uploads,
    passes: &'a HashMap<NodeId, &'a PassWant>,
    ops: Vec<Op>,
    new: Vec<Upload>,
}

impl Lowering<'_> {
    fn lower(&mut self, items: &[DisplayItem]) {
        let mut cur = Affine::IDENTITY;
        let mut saved: Vec<Affine> = Vec::new();
        let mut i = 0;
        while i < items.len() {
            let d = &items[i];
            i += 1;
            match &d.item {
                Item::PushClip(_)
                | Item::PushOpacity(_)
                | Item::PushTransform(_)
                | Item::PushLayer(_)
                    if d.bounds.intersect(self.surface).is_none() =>
                {
                    i = crate::raster::skip_group(items, i - 1);
                }
                Item::PushTransform(a) => {
                    saved.push(cur);
                    cur = *a;
                    self.ops.push(Op::Transform(cur));
                }
                Item::PopTransform => {
                    cur = saved.pop().unwrap_or(Affine::IDENTITY);
                    self.ops.push(Op::Transform(cur));
                }
                Item::PushClip(p) => self.ops.push(Op::PushClip(p.clone())),
                Item::PopClip => self.ops.push(Op::PopClip),
                Item::PushOpacity(o) => self.ops.push(Op::PushLayer(strand_gpu::Layer {
                    opacity: Some(*o),
                    ..Default::default()
                })),
                Item::PopOpacity => self.ops.push(Op::PopLayer),
                Item::PushLayer(l) => {
                    let end = crate::raster::skip_group(items, i - 1);
                    let masked = l.effects.iter().any(|e| {
                        matches!(
                            e,
                            strand_scene::Effect::Mask(
                                strand_scene::Mask::Fade { .. } | strand_scene::Mask::Radial { .. }
                            )
                        )
                    });
                    if masked {
                        // Masks are always rasterised on the CPU: the
                        // whole group, as one upload.
                        self.island(&items[i - 1..end], d.bounds, cur);
                        i = end;
                        continue;
                    }
                    let (blend, opacity) = layer_parts(l);
                    self.ops.push(Op::PushLayer(strand_gpu::Layer {
                        blend,
                        opacity,
                        ..Default::default()
                    }));
                    if let Some(g) = self.groups.get(&layer_key(l)) {
                        // Filtered on the CPU: its pixels in place of its
                        // items.
                        let id = self.uploads.id(&g.pixmap, &mut self.new);
                        let (x, y) = (g.x as f64, g.y as f64);
                        self.ops.push(Op::Transform(Affine::IDENTITY));
                        self.ops.push(Op::Image {
                            rect: kurbo::Rect::new(
                                x,
                                y,
                                x + g.pixmap.width() as f64,
                                y + g.pixmap.height() as f64,
                            ),
                            image: id,
                            image_transform: Affine::translate((x, y)),
                            tint: None,
                            smooth: false,
                        });
                        self.ops.push(Op::Transform(cur));
                        self.ops.push(Op::PopLayer);
                        i = end;
                    }
                }
                Item::PopLayer => self.ops.push(Op::PopLayer),
                _ if d.bounds.intersect(self.surface).is_none() => {}
                Item::Shadow {
                    rect,
                    radii,
                    std_dev,
                    color,
                    clip,
                    extent,
                } => {
                    let shape = ShadowShape {
                        rect: *rect,
                        radii: *radii,
                        std_dev: *std_dev,
                        color: *color,
                        extent: *extent,
                    };
                    match self.cache.shadow(&shape) {
                        Some(pm) => {
                            let id = self.uploads.id(&pm, &mut self.new);
                            let (ox, oy) = (extent.x0.floor(), extent.y0.floor());
                            self.ops.push(Op::PushClipEvenOdd(clip.clone()));
                            self.ops.push(Op::Image {
                                rect: kurbo::Rect::new(
                                    ox,
                                    oy,
                                    ox + pm.width() as f64,
                                    oy + pm.height() as f64,
                                ),
                                image: id,
                                image_transform: Affine::translate((ox, oy)),
                                tint: None,
                                smooth: cur != Affine::IDENTITY,
                            });
                            self.ops.push(Op::PopClip);
                        }
                        // Too large to cache: drawn on the CPU.
                        None => self.island(&items[i - 1..i], d.bounds, cur),
                    }
                }
                Item::Fill {
                    shape,
                    paint,
                    frame,
                } => {
                    let (brush, brush_transform) = brush(paint, *frame);
                    let path = match shape {
                        FillShape::Rect(r) => r.to_path(0.1),
                        FillShape::Path(p) => p.clone(),
                    };
                    self.ops.push(Op::Fill {
                        path,
                        brush,
                        brush_transform,
                        even_odd: false,
                    });
                }
                Item::Border { path, paint, frame } => {
                    let (brush, brush_transform) = brush(paint, *frame);
                    self.ops.push(Op::Fill {
                        path: path.clone(),
                        brush,
                        brush_transform,
                        even_odd: true,
                    });
                }
                Item::Raster {
                    node, pixmap, rect, ..
                } => {
                    if let Some(w) = self.passes.get(node) {
                        // A `shader` node: its pass, in the GPU frame.
                        let r = kurbo::Rect::new(
                            rect.x0,
                            rect.y0,
                            rect.x0 + w.size.w as f64,
                            rect.y0 + w.size.h as f64,
                        );
                        self.ops.push(Op::Pass {
                            pass: w.pass.clone(),
                            bounds: r,
                            globals: w.globals,
                        });
                        continue;
                    }
                    let id = self.uploads.id(pixmap, &mut self.new);
                    self.ops.push(Op::Image {
                        rect: *rect,
                        image: id,
                        image_transform: Affine::translate((rect.x0, rect.y0)),
                        tint: None,
                        smooth: cur != Affine::IDENTITY,
                    });
                }
                Item::Image {
                    pixmap,
                    rect,
                    dest,
                    tint,
                } => {
                    let (kx, ky) = (
                        dest.width() / pixmap.width().max(1) as f64,
                        dest.height() / pixmap.height().max(1) as f64,
                    );
                    let resized = (kx - 1.0).abs() > 1e-9 || (ky - 1.0).abs() > 1e-9;
                    let id = self.uploads.id(pixmap, &mut self.new);
                    self.ops.push(Op::Image {
                        rect: *rect,
                        image: id,
                        image_transform: Affine::translate((dest.x0, dest.y0))
                            * Affine::scale_non_uniform(kx, ky),
                        tint: tint.map(straight),
                        smooth: cur != Affine::IDENTITY || resized,
                    });
                }
                Item::Glyphs {
                    x,
                    y,
                    layout,
                    color,
                    spans,
                } => {
                    let k = self.scale.as_f64() / layout.scale.as_f64();
                    let smooth = k != 1.0 || cur != Affine::IDENTITY;
                    self.ops.push(Op::Transform(
                        cur * Affine::translate((*x as f64, *y as f64)) * Affine::scale(k),
                    ));
                    for (g, run_color) in layout
                        .runs
                        .iter()
                        .flat_map(|r| r.glyphs.iter().map(move |g| (g, r.color)))
                    {
                        let Some(page) = self.atlas.page(g.slot.page) else {
                            continue;
                        };
                        let id = self.uploads.id(page, &mut self.new);
                        self.ops.push(Op::Image {
                            rect: kurbo::Rect::new(
                                g.x as f64,
                                g.y as f64,
                                (g.x + g.slot.w as i32) as f64,
                                (g.y + g.slot.h as i32) as f64,
                            ),
                            image: id,
                            image_transform: Affine::translate((
                                (g.x - g.slot.x as i32) as f64,
                                (g.y - g.slot.y as i32) as f64,
                            )),
                            tint: Some(straight(crate::flatten::slot_color(
                                run_color, spans, *color,
                            ))),
                            smooth,
                        });
                    }
                    self.ops.push(Op::Transform(cur));
                }
            }
        }
    }

    /// Draws `items` (a whole group, or one item) on the CPU into a pixmap
    /// of `bounds` and draws that: what the GPU cannot.
    fn island(&mut self, items: &[DisplayItem], bounds: Rect, cur: Affine) {
        let Some(region) = bounds.intersect(self.surface).filter(|r| !r.is_empty()) else {
            return;
        };
        let (Ok(w), Ok(h)) = (u16::try_from(region.w), u16::try_from(region.h)) else {
            return;
        };
        if region.w > MAX_SIDE || region.h > MAX_SIDE {
            return;
        }
        let mut ctx = RenderContext::new_with(
            w,
            h,
            RenderSettings {
                num_threads: 0,
                ..RenderSettings::default()
            },
        );
        let base = Affine::translate((-(region.x as f64), -(region.y as f64)));
        ctx.set_transform(base * cur);
        crate::raster::draw_group(
            &mut ctx,
            items,
            region,
            self.atlas,
            &*self.cache,
            self.scale,
            base,
            cur,
            self.groups,
        );
        ctx.flush();
        let mut pm = Pixmap::new(w, h);
        ctx.render_with(
            pm.as_mut(),
            &mut Resources::new(),
            RasterizerSettings {
                target_init: TargetInit::SrcOver,
                render_mode: RenderMode::OptimizeQuality,
                ..RasterizerSettings::default()
            },
        );
        drop(ctx);
        let pm = Arc::new(pm);
        let id = self.uploads.id(&pm, &mut self.new);
        let (x, y) = (region.x as f64, region.y as f64);
        self.ops.push(Op::Transform(Affine::IDENTITY));
        self.ops.push(Op::Image {
            rect: kurbo::Rect::new(x, y, x + w as f64, y + h as f64),
            image: id,
            image_transform: Affine::translate((x, y)),
            tint: None,
            smooth: false,
        });
        self.ops.push(Op::Transform(cur));
    }
}

/// A layer's cell-local blend and opacity, as the CPU lowers them.
fn layer_parts(l: &crate::layers::Layer) -> (Option<vello_cpu::peniko::BlendMode>, Option<f32>) {
    let mut opacity: Option<f32> = None;
    let mut blend = None;
    for e in l.effects.iter() {
        match e {
            strand_scene::Effect::Opacity(o) => {
                let o = if o.is_finite() {
                    o.clamp(0.0, 1.0)
                } else {
                    1.0
                };
                opacity = Some(opacity.unwrap_or(1.0) * o);
            }
            strand_scene::Effect::Blend(b) => blend = Some(crate::layers::blend_mode(*b)),
            _ => {}
        }
    }
    (blend, opacity)
}

/// A colour as the GPU takes it (straight, not swapped).
fn straight(c: Color) -> AlphaColor<Srgb> {
    let c = c.clamped();
    AlphaColor::new([c.r, c.g, c.b, c.a])
}

/// Samples between adjacent stops (as the CPU's gradients).
const GRADIENT_SAMPLES: usize = 8;

fn stops(stops: &[GradientStop]) -> Vec<(f32, AlphaColor<Srgb>)> {
    let clean: Vec<GradientStop> = stops
        .iter()
        .map(|s| GradientStop {
            offset: if s.offset.is_finite() {
                s.offset.clamp(0.0, 1.0)
            } else {
                0.0
            },
            color: s.color.clamped(),
        })
        .collect();
    let mut out = Vec::with_capacity(clean.len() * GRADIENT_SAMPLES);
    for (i, s) in clean.iter().enumerate() {
        out.push((s.offset, straight(s.color)));
        let Some(next) = clean.get(i + 1) else { break };
        if next.offset <= s.offset {
            continue;
        }
        for k in 1..GRADIENT_SAMPLES {
            let t = k as f32 / GRADIENT_SAMPLES as f32;
            let offset = s.offset + (next.offset - s.offset) * t;
            out.push((offset, straight(s.color.lerp_oklab(next.color, t))));
        }
    }
    out
}

fn gradient(mut g: Gradient, s: &[GradientStop]) -> Brush {
    g.interpolation_cs = ColorSpaceTag::Srgb;
    g.extend = Extend::Pad;
    Brush::Gradient(g.with_stops(stops(s).as_slice()))
}

fn finite_angle(deg: f32) -> f64 {
    if deg.is_finite() {
        (deg % 360.0) as f64
    } else {
        0.0
    }
}

/// The brush for `p` filling `frame`, and its transform (the CPU's
/// `paint_type` in straight colours).
fn brush(p: &Paint, frame: kurbo::Rect) -> (Brush, Affine) {
    let center = frame.center();
    let b = match p {
        Paint::Solid(c) => Brush::Solid(straight(*c)),
        Paint::Linear { stops: s, .. }
        | Paint::Radial { stops: s }
        | Paint::Conic { stops: s, .. }
            if s.len() < 2 =>
        {
            Brush::Solid(
                s.first()
                    .map_or(AlphaColor::TRANSPARENT, |st| straight(st.color)),
            )
        }
        Paint::Linear { angle, stops: s } => {
            let a = finite_angle(*angle).to_radians();
            let (sin, cos) = a.sin_cos();
            let len = (frame.width() * sin).abs() + (frame.height() * cos).abs();
            let d = kurbo::Vec2::new(sin, -cos) * (len / 2.0);
            gradient(Gradient::new_linear(center - d, center + d), s)
        }
        Paint::Radial { stops: s } => {
            let r = (frame.width() / 2.0).hypot(frame.height() / 2.0);
            gradient(Gradient::new_radial(center, r as f32), s)
        }
        Paint::Conic { from, stops: s } => {
            let g = gradient(Gradient::new_sweep(center, 0.0, std::f32::consts::TAU), s);
            let turn = (finite_angle(*from) - 90.0).to_radians();
            return (g, Affine::rotate_about(turn, center));
        }
    };
    (b, Affine::IDENTITY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use strand_scene::shader::{ShaderCode, UniformType};

    fn code() -> Arc<ShaderCode> {
        Arc::new(ShaderCode {
            path: "a.wgsl".into(),
            wgsl:
                "@fragment fn main() -> @location(0) vec4<f32> { return vec4<f32>(strand.time); }"
                    .into(),
            uniforms: ShaderCode::packed(vec![
                ("u_r".into(), UniformType::F32, 0),
                ("u_tint".into(), UniformType::Vec4, 1),
                ("u_dir".into(), UniformType::Vec2, 2),
                ("u_turn".into(), UniformType::F32, 3),
            ]),
        })
    }

    /// Uniforms reach the GPU in buffer units, in slot order; a slot no
    /// prop sets is zero.
    #[test]
    fn uniforms_pack_in_buffer_units() {
        let entries = vec![
            (
                "u_dir".to_string(),
                PropValue::List(vec![PropValue::Number(1.0), PropValue::Number(0.5)]),
            ),
            ("u_r".to_string(), PropValue::Length(Length::Px(6.0))),
            (
                "u_tint".to_string(),
                PropValue::Color(Color::new(1.0, 1.0, 1.0, 0.5)),
            ),
        ];
        let p = pack(&code(), &entries, 2.0);
        assert_eq!(p.len(), 8);
        assert_eq!(p[0], 12.0, "px × scale");
        assert_eq!(&p[1..5], &[0.5, 0.5, 0.5, 0.5], "premultiplied linear");
        assert_eq!(&p[5..7], &[1.0, 0.5]);
        assert_eq!(p[7], 0.0, "unset");
        let p = pack(
            &code(),
            &[("u_turn".to_string(), PropValue::Angle(180.0))],
            1.0,
        );
        assert!((p[7] - std::f32::consts::PI).abs() < 1e-6, "radians");
    }

    #[test]
    fn node_keys_round_trip() {
        let n = NodeId::new(7, 3);
        assert_eq!(key_node(node_key(n)), n);
    }

    #[test]
    fn a_clocked_pass_changes_with_time_and_a_static_one_does_not() {
        let c = code();
        let a = pass_want(NodeId::new(3, 0), &c, None, 10, 10, 1.0, 0.5).unwrap();
        let b = pass_want(NodeId::new(3, 0), &c, None, 10, 10, 1.0, 0.6).unwrap();
        assert!(a.timed);
        assert_ne!(a.key, b.key);
        let still = Arc::new(ShaderCode {
            wgsl: "@fragment fn main() -> @location(0) vec4<f32> { return vec4<f32>(1.0); }".into(),
            ..(*c).clone()
        });
        let a = pass_want(NodeId::new(3, 0), &still, None, 10, 10, 1.0, 0.5).unwrap();
        let b = pass_want(NodeId::new(3, 0), &still, None, 10, 10, 1.0, 0.6).unwrap();
        assert!(!a.timed);
        assert_eq!(a.key, b.key, "time is 0 for a pass that does not read it");
        assert_eq!(a.globals.time, 0.0);
        assert!(pass_want(NodeId::new(3, 0), &still, None, 0, 10, 1.0, 0.0).is_none());
    }

    #[test]
    fn uploads_are_kept_by_pixmap_and_retired_when_it_goes() {
        let mut u = Uploads::default();
        let mut new = Vec::new();
        let a = Arc::new(Pixmap::new(2, 2));
        u.begin();
        let id = u.id(&a, &mut new);
        assert_eq!(new.len(), 1);
        assert_eq!(u.id(&a, &mut new), id);
        assert_eq!(new.len(), 1, "held: not sent again");
        assert!(u.retire().is_empty());
        // Sent: the GPU thread holds the upload's pixmap only until it
        // is uploaded.
        new.clear();
        drop(a);
        assert_eq!(u.retire(), [id], "its pixmap went");
        let b = Arc::new(Pixmap::new(2, 2));
        let id = u.id(&b, &mut new);
        for _ in 0..RETIRE_AFTER {
            u.begin();
        }
        assert_eq!(u.retire(), [id], "nothing drew it");
    }
}
