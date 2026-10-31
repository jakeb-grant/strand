//! (M4) The GPU backend as render drives it (architecture.md,
//! "`strand-gpu`"): promotion and the device's lifecycle
//! ([`crate::promote`]), lowering a surface's display list to
//! [`strand_gpu::Frame`] ops, `shader` nodes' passes, and the replies.
//!
//! Render never owns the [`strand_gpu::Gpu`]: it queues requests
//! ([`Renderer::take_gpu_requests`]) and backend changes
//! ([`Renderer::take_backend_changes`]) for the host, which starts the
//! `Gpu` when requests come and none runs, or on [`BackendChange::Start`]
//! (the warm start, [`Renderer::warm_gpu`]), and hands every reply to
//! [`Renderer::deliver_gpu`]. The device is kept up once started
//! (gpu-first): only a failure or the thread's end takes it down.
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

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Weak};
use std::time::Duration;
use strand_scene::time::Instant;

use strand_gpu::{
    Brush, Frame, GpuErrorKind, GpuMode, GpuReply, GpuRequest, Op, PassFrame, PassGlobals,
    Readback, Upload,
};
use strand_scene::shader::ShaderCode;
use strand_scene::{
    Backend, BackendChange, Color, Damage, GpuStatus, GradientStop, Length, LogicalSize, NodeId,
    Paint, PaintTarget, PropValue, Rect, Scale, ShaderInput, ShaderPass, ShaderRef, Size,
    SurfaceId,
};
use vello_cpu::color::{AlphaColor, ColorSpaceTag, Srgb};
use vello_cpu::kurbo::{self, Affine, Shape};
use vello_cpu::peniko::{Extend, Gradient};
use vello_cpu::{
    Pixmap, RasterizerSettings, RenderContext, RenderMode, RenderSettings, Resources, TargetInit,
};

use super::Renderer;
use crate::cache::PaintCache;
use crate::flatten::{DisplayItem, FillShape, Item};
use crate::offscreen::{Drawn, layer_key};
use crate::promote::{Device, GPU_OFF, Promotion};
use crate::raster::AtlasMirror;

/// How long a frame holds for a pass or a readback before drawing what
/// it has.
pub const GPU_WAIT: Duration = Duration::from_millis(8);

/// Lowerings after which an upload nothing drew is retired.
const RETIRE_AFTER: u64 = 600;

/// Largest side of a pass, an island or a readback, in pixels.
const MAX_SIDE: u32 = 8192;

// ---------------------------------------------------------------------------
// Passes

/// Which of a node's passes: its own (a `shader` node, a GPU aurora or
/// particle field), its `filter:` layer's bundled passes, or its
/// `backdrop:`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Slot {
    Node = 0,
    Filter = 1,
    Backdrop = 2,
}

/// A pass, by its node and slot.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PassId {
    pub node: NodeId,
    pub slot: Slot,
}

impl PassId {
    pub(crate) fn new(node: NodeId, slot: Slot) -> Self {
        PassId { node, slot }
    }

    /// Its key on the wire: the node in the low 62 bits (indexes stay
    /// far below 2^30), the slot in the top two.
    fn wire(self) -> u64 {
        let n = (u64::from(self.node.index & 0x3FFF_FFFF) << 32) | u64::from(self.node.generation);
        n | ((self.slot as u64) << 62)
    }

    fn unwire(k: u64) -> Self {
        let slot = match k >> 62 {
            1 => Slot::Filter,
            2 => Slot::Backdrop,
            _ => Slot::Node,
        };
        PassId {
            node: NodeId::new(((k >> 32) & 0x3FFF_FFFF) as u32, k as u32),
            slot,
        }
    }
}

/// What a pass reads, by the display item that has it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum PassInput {
    /// Nothing.
    None,
    /// The group of the `PushLayer` at this index, CPU-filtered but not
    /// by its bundled passes.
    Group(usize),
    /// What the display list draws before the `PushLayer` at this
    /// index (a backdrop).
    Behind(usize),
}

/// A pass as one frame wants it.
#[derive(Clone, Debug)]
pub(crate) struct PassWant {
    pub id: PassId,
    /// What the pass's pixels depend on: code, uniforms, size, time,
    /// input.
    pub key: u64,
    /// What a failure of it depends on: its key without the time, the
    /// pointer and (for a bundled pass that moves) the uniforms that
    /// carry them. A failed pass is asked again only when this changes,
    /// so a clocked one is not asked at refresh rate.
    pub base: u64,
    /// Where its pixels go on the surface (buffer pixels), and their size.
    pub at: (i32, i32),
    pub size: Size,
    pub pass: ShaderPass,
    /// Passes run after it, each on the one before's pixels.
    pub then: Vec<ShaderPass>,
    pub globals: PassGlobals,
    pub input: PassInput,
    /// It reads `strand.time`: its passes are pipelined, never held for.
    pub timed: bool,
    /// It reads `strand.pointer`: a pointer motion repaints it.
    pub pointer: bool,
    /// (perf-gpu-retain review) Drawn at this fraction of its size, its
    /// uniforms in those pixels (a small backdrop blur at the CPU's
    /// reduction, `backdrop::Pass::reduction`); 1 for full size.
    pub reduce: u32,
    /// (perf-gpu-retain review) Drawn only in a GPU frame
    /// (`Op::Backdrop`), never asked for on its own: where no GPU frame
    /// draws it the CPU does (a small backdrop blur).
    pub inframe_only: bool,
}

/// The last pixels each pass gave, with the key of the want they
/// answer.
#[derive(Debug, Default)]
pub struct ShaderResults {
    map: HashMap<PassId, PassResult>,
    /// Nodes a filter pass has answered for (a tilt then turns in 3-D).
    answered: HashSet<NodeId>,
    /// (m4-audit) Shader files whose pass or frame lost the device (ran
    /// past `HUNG_AFTER`, or panicked): a hung device is leaked with its
    /// spinning queue, so such a file is never run again in this process,
    /// whatever its uniforms, size or node; a reload that changes its
    /// text is new code.
    lost: HashSet<Arc<ShaderCode>>,
}

#[derive(Debug)]
struct PassResult {
    got: (u64, Arc<Pixmap>),
    /// The surface whose frame asked for it.
    surface: SurfaceId,
    /// The pass failed: no pixels, not asked again until its want's
    /// `base` changes.
    failed: bool,
    /// The `base` of the want it answers.
    base: u64,
}

impl ShaderResults {
    /// The last answer for `id`: its want's key and its pixels (one
    /// transparent pixel when it failed).
    pub(crate) fn get(&self, id: PassId) -> Option<&(u64, Arc<Pixmap>)> {
        self.map.get(&id).map(|r| &r.got)
    }

    /// True if the last pass asked for `id` failed.
    pub(crate) fn failed(&self, id: PassId) -> bool {
        self.map.get(&id).is_some_and(|r| r.failed)
    }

    /// True if `code` lost a device: its `shader` nodes want no pass.
    pub(crate) fn lost(&self, code: &ShaderCode) -> bool {
        self.lost.contains(code)
    }

    /// `code` cost a device: never asked again (see `lost`).
    fn lose(&mut self, code: Option<Arc<ShaderCode>>) {
        if let Some(code) = code
            && !self.lost.contains(&*code)
        {
            log::warn!(
                "GPU: `{}` lost the device; it draws nothing until its file changes",
                code.path
            );
            self.lost.insert(code);
        }
    }

    /// True if a filter pass of `node` has given pixels.
    pub(crate) fn answered(&self, node: NodeId) -> bool {
        self.answered.contains(&node)
    }

    /// The pixels `w` can draw: its own, or (a frame behind) the last of
    /// the same size, as a clocked pass's are while the next is drawn,
    /// and a changed one's while it is redrawn. `None`: the CPU draws
    /// its fallback (no pixels yet, a failed pass, a new size).
    pub(crate) fn usable(&self, w: &PassWant) -> Option<&(u64, Arc<Pixmap>)> {
        self.map
            .get(&w.id)
            .filter(|r| !r.failed)
            .map(|r| &r.got)
            .filter(|(k, p)| {
                *k == w.key
                    || (u32::from(p.width()) == w.size.w && u32::from(p.height()) == w.size.h)
            })
    }

    /// The device went (dropped on idle, lost, unavailable): a tilt that
    /// holds no 3-D pixels warms up in 2-D again (`Flattener::warm_tilt`)
    /// rather than drawing flat until a new device answers. One that holds
    /// them keeps drawing them.
    fn device_gone(&mut self) {
        let Self { map, answered, .. } = self;
        answered.retain(|n| {
            map.get(&PassId::new(*n, Slot::Filter))
                .is_some_and(|r| !r.failed)
        });
    }

    /// Bytes of pixels held (tests).
    fn bytes(&self) -> usize {
        self.map
            .values()
            .filter(|r| !r.failed)
            .map(|r| r.got.1.data_as_u8_slice().len())
            .sum()
    }
}

/// The shader file `w` runs, if any (a `shader` node's pass; a bundled
/// effect runs Strand's own code).
fn file_code(w: &PassWant) -> Option<Arc<ShaderCode>> {
    std::iter::once(&w.pass)
        .chain(&w.then)
        .find_map(|p| match &p.code {
            ShaderRef::File(c) => Some(c.clone()),
            ShaderRef::Bundled(_) => None,
        })
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

/// True if `code` reads the pointer.
pub(crate) fn reads_pointer(code: &ShaderCode) -> bool {
    code.wgsl.contains("strand.pointer")
}

/// A pass's key: what its pixels depend on.
fn want_key(
    passes: &[&ShaderPass],
    size: Size,
    scale: f32,
    time: f32,
    pointer: [f32; 2],
    input: u64,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut k = std::collections::hash_map::DefaultHasher::new();
    for p in passes {
        match &p.code {
            ShaderRef::File(code) => code.hash(&mut k),
            ShaderRef::Bundled(b) => b.hash(&mut k),
        }
        for v in p.uniforms.iter() {
            v.to_bits().hash(&mut k);
        }
    }
    (size.w, size.h, scale.to_bits(), time.to_bits()).hash(&mut k);
    pointer.map(f32::to_bits).hash(&mut k);
    input.hash(&mut k);
    k.finish()
}

/// A bundled want's `base`: its passes' code, its size, scale and input,
/// and its uniforms if `uniforms` (a moving pass's carry its time, as
/// particles' positions do).
fn fail_key(passes: &[&ShaderPass], size: Size, scale: f32, input: u64, uniforms: bool) -> u64 {
    if uniforms {
        return want_key(passes, size, scale, 0.0, [-1.0, -1.0], input);
    }
    use std::hash::{Hash, Hasher};
    let mut k = std::collections::hash_map::DefaultHasher::new();
    for p in passes {
        match &p.code {
            ShaderRef::File(code) => code.hash(&mut k),
            ShaderRef::Bundled(b) => b.hash(&mut k),
        }
    }
    (size.w, size.h, scale.to_bits(), input).hash(&mut k);
    k.finish()
}

/// The pass a `shader` node draws at `w × h` buffer pixels at `at`, its
/// `u_*` values packed in its slots' order; `pointer` in buffer pixels
/// relative to its box (-1, -1 outside).
#[allow(clippy::too_many_arguments)]
pub(crate) fn pass_want(
    node: NodeId,
    code: &Arc<ShaderCode>,
    uniforms: Option<&PropValue>,
    at: (i32, i32),
    w: u32,
    h: u32,
    scale: f32,
    time: f32,
    pointer: [f32; 2],
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
    let reads = reads_pointer(code);
    let pointer = if reads { pointer } else { [-1.0, -1.0] };
    let pass = ShaderPass {
        code: ShaderRef::File(code.clone()),
        uniforms: packed.into(),
        input: ShaderInput::None,
    };
    let size = Size::new(w, h);
    Some(PassWant {
        id: PassId::new(node, Slot::Node),
        key: want_key(&[&pass], size, scale, time, pointer, 0),
        base: want_key(&[&pass], size, scale, 0.0, [-1.0, -1.0], 0),
        at,
        size,
        pass,
        then: Vec::new(),
        globals: PassGlobals {
            time,
            scale,
            pointer,
        },
        input: PassInput::None,
        timed,
        pointer: reads,
        reduce: 1,
        inframe_only: false,
    })
}

/// A bundled effect's pass want: `passes` (run in order) over `region`
/// of the surface, reading `input` (whose content hashes to `input_key`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn bundled_want(
    id: PassId,
    passes: Vec<ShaderPass>,
    region: Rect,
    input: PassInput,
    input_key: u64,
    scale: f32,
    time: f32,
    pointer: [f32; 2],
) -> Option<PassWant> {
    if region.w == 0 || region.h == 0 || region.w > MAX_SIDE || region.h > MAX_SIDE {
        return None;
    }
    let mut passes = passes.into_iter();
    let pass = passes.next()?;
    let then: Vec<ShaderPass> = passes.collect();
    let all: Vec<&ShaderPass> = std::iter::once(&pass).chain(then.iter()).collect();
    let timed = all.iter().any(|p| bundled_timed(p));
    let reads = all.iter().any(|p| bundled_pointer(p));
    let time = if timed && time.is_finite() { time } else { 0.0 };
    let pointer = if reads { pointer } else { [-1.0, -1.0] };
    let size = Size::new(region.w, region.h);
    Some(PassWant {
        id,
        key: want_key(&all, size, scale, time, pointer, input_key),
        base: fail_key(&all, size, scale, input_key, !timed),
        at: (region.x, region.y),
        size,
        pass,
        then,
        globals: PassGlobals {
            time,
            scale,
            pointer,
        },
        input,
        timed,
        pointer: reads,
        reduce: 1,
        inframe_only: false,
    })
}

/// True for a bundled pass whose look moves with time.
fn bundled_timed(p: &ShaderPass) -> bool {
    use strand_scene::Bundled;
    matches!(
        p.code,
        ShaderRef::Bundled(Bundled::Wobble | Bundled::Aurora | Bundled::Particles)
    )
}

/// True for a bundled pass that follows the pointer (glass's highlight).
fn bundled_pointer(p: &ShaderPass) -> bool {
    matches!(p.code, ShaderRef::Bundled(strand_scene::Bundled::Glass))
}

/// True for a pass the GPU draws in place of a layer's CPU version:
/// a bundled `filter:` pass (bloom, CRT, chromatic, wobble, 3D tilt).
pub(crate) fn gpu_filter(e: &strand_scene::Effect) -> Option<&ShaderPass> {
    match e {
        strand_scene::Effect::Shader(
            p @ ShaderPass {
                code: ShaderRef::Bundled(_),
                input: ShaderInput::Content,
                ..
            },
        ) => Some(p),
        _ => None,
    }
}

/// `entries` in `code`'s slot order, in buffer units (architecture.md,
/// the ABI): lengths × `scale`, angles in radians, durations in
/// seconds, colours premultiplied sRGB (the space Strand composites in,
/// which is also how a pass's output is read). A slot no entry sets is
/// zero.
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
            let c = c.clamped();
            out.extend([c.r * c.a, c.g * c.a, c.b * c.a, c.a]);
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

/// Presented frames kept waiting for their GPU run: the GPU thread
/// times at most two at once, so an older one's never comes.
const UNTIMED: usize = 4;

/// One surface as the GPU backend sees it.
#[derive(Debug, Default)]
struct Surf {
    promo: Promotion,
    /// What draws it.
    backend: Backend,
    /// Its readback frame in flight, and since when.
    inflight: Option<(u64, Instant)>,
    /// A presented frame was handed out ([`Renderer::paint_gpu`]) and
    /// its `Presented` (or `Failed`) has not come: the next `Presented`
    /// times that frame.
    presenting: bool,
    /// Pixels of its last readback frame, not copied in yet.
    pixels: Option<Readback>,
    /// When the `paint` of its last frame sent to the GPU began: that
    /// frame's time for `strand report` runs from there to its
    /// `Presented` or `Pixels` (`summary.rs`).
    started: Option<Instant>,
    /// Its presented frames whose GPU run has not come (frame number and
    /// the time before it), oldest first, at most [`UNTIMED`].
    untimed: VecDeque<(u64, Duration)>,
    /// Until when its next frame holds (a pass or readback in flight).
    hold: Option<Instant>,
    /// The `shader` nodes' passes its GPU frames draw inline (their id,
    /// key, base and file), from its last frame's wants: recorded as
    /// failed, and their files as lost, when a frame of it hangs or
    /// panics, so that no new device is asked for them (m4-audit).
    inline: Vec<(PassId, u64, u64, Option<Arc<ShaderCode>>)>,
    /// (gpu-first review 2) The size hold the GPU thread was asked to
    /// commit for ([`GpuRequest::Commit`]): asked once per size.
    committed: Option<LogicalSize>,
    /// (perf-gpu-retain) The size of the last frame lowered for it since
    /// it was attached to present: the GPU thread holds that frame when
    /// the next one is drawn (a frame that fails takes the surface back
    /// to the CPU), so a crossfade can take it as its snapshot.
    shown: Option<Size>,
    /// (perf-gpu-retain) What its last presented frame did with its
    /// snapshot (`Renderer::fresh_gpu_frame` does it again).
    snap: strand_gpu::FrameSnapshot,
}

/// One `shader` node's pass in flight.
#[derive(Debug)]
struct Pending {
    frame: u64,
    key: u64,
    base: u64,
    surface: SurfaceId,
    /// Its shader file: lost with the device if the pass hangs.
    code: Option<Arc<ShaderCode>>,
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
    pending: HashMap<PassId, Pending>,
    next_frame: u64,
    /// A `GpuPresent` frame `paint` lowered, for [`Renderer::paint_gpu`].
    present: Option<Frame>,
    /// The current `paint` sent its frame to the GPU (lowered or
    /// requested): its time is taken when the GPU's reply comes.
    sent: bool,
    /// Readback frames copied in (tests).
    copied: u64,
    /// The pixels `paint_gpu`'s target points at: never written (a
    /// present frame is lowered, not rasterised), so never resident.
    scratch: Vec<u8>,
    /// A pass was wanted by the last frame of some surface.
    demand: HashSet<SurfaceId>,
    /// Surfaces whose last frame had a pass that reads the pointer.
    pub(super) pointer_users: HashSet<SurfaceId>,
    /// Presented surfaces crossfading: their snapshot as an upload,
    /// with the snapshot's address it was copied from.
    fades: HashMap<SurfaceId, (usize, Arc<Pixmap>)>,
    /// The last reason the device was unavailable that was logged as a
    /// warning (a retry every 30 s with the same reason is not), and
    /// how many were (tests).
    warned: Option<String>,
    warnings: u64,
    /// Promotion is off (tests whose frames' wall-clock pacing must not
    /// matter).
    no_promotion: bool,
    /// Timed GPU frames for the binary's `damage` log (its `gpu-frame`
    /// lines): the surface, the frame's CPU part and its GPU run; kept
    /// only while asked for ([`Renderer::log_gpu_frames`]).
    timed: Option<Vec<(SurfaceId, Duration, Duration)>>,
    /// The warm start asked for the device and it has not answered: a
    /// device that cannot start then is said at `info`, since nothing
    /// showed needs it (a GPU-less machine is not a fault).
    warming: bool,
    /// (gpu-first review 2) The binary keeps a device warm
    /// ([`Renderer::warm_gpu`]): one that failed for a reason that may
    /// pass, or was lost, is started again at its retry deadline, not at
    /// the next animation's first frame.
    keep_warm: bool,
    /// That retry was made since the device was last up: a device that
    /// fails again is asked for on demand only, so a machine whose device
    /// never starts is not woken every 30 s while idle.
    rewarmed: bool,
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

    /// `surface`'s state.
    fn surf(&mut self, surface: SurfaceId) -> &mut Surf {
        self.surfaces.entry(surface).or_default()
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
            s.presenting = false;
            s.pixels = None;
            s.hold = None;
            s.shown = None;
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

    /// (M4) What the host must do about the GPU: start the `Gpu`
    /// (`Start`), attach a surface (`Promote`), release one the GPU failed
    /// (`Demote`).
    ///
    /// The host calls it after every dispatch, so it also re-arms the
    /// renderer's timer: a reply since the last paint can set the kept-warm
    /// device's retry.
    pub fn take_backend_changes(&mut self) -> Vec<BackendChange> {
        self.arm_timer();
        std::mem::take(&mut self.gpu.changes)
    }

    /// (gpu-first) Keeps each timed GPU frame (its CPU part and its GPU
    /// run) for [`Renderer::take_gpu_frame_times`]: the binary's `damage`
    /// log turns them into `gpu-frame` lines.
    pub fn log_gpu_frames(&mut self, on: bool) {
        self.gpu.timed = on.then(Vec::new);
    }

    /// (gpu-first) The GPU frames timed since the last call: surface, CPU
    /// part (its paint and the GPU thread's work to its submission) and
    /// GPU run (zero for a readback frame, whose time is one figure).
    pub fn take_gpu_frame_times(&mut self) -> Vec<(SurfaceId, Duration, Duration)> {
        self.gpu
            .timed
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
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

    /// (M4) True once [`crate::promote::LOST_CAP`] devices were lost: the GPU is off until
    /// strand restarts, and the host tells the status whether or not a
    /// frame shows a `shader` node.
    pub fn gpu_off(&self) -> bool {
        self.gpu.device.is_off()
    }

    /// (M5) Devices lost so far in this process (`strand report`).
    pub fn gpu_lost_devices(&self) -> u32 {
        self.gpu.device.losses()
    }

    /// (M4) Unavailable reasons logged as warnings so far (tests).
    #[doc(hidden)]
    pub fn gpu_warnings(&self) -> u64 {
        self.gpu.warnings
    }

    /// (M4) Bytes of GPU pass pixels render holds (tests): only those
    /// of passes some surface's last frame wanted.
    #[doc(hidden)]
    pub fn gpu_pass_bytes(&self) -> usize {
        self.extras.shaders.bytes()
    }

    /// (M4) Readback frames copied into `wl_shm` buffers so far (tests).
    #[doc(hidden)]
    pub fn gpu_frames_copied(&self) -> u64 {
        self.gpu.copied
    }

    /// (M4) Promotes `surface` now, as an animation's first large frame
    /// would (tests: promotion's timing is `promote.rs`'s).
    #[doc(hidden)]
    pub fn promote_now(&mut self, surface: SurfaceId) {
        let now = Instant::now();
        if self.gpu.want(now) {
            self.gpu.surf(surface).promo.force_gpu();
            self.gpu.changes.push(BackendChange::Promote(surface));
        }
    }

    /// (gpu-first) Starts the device now, before anything needs it, and
    /// keeps it for the process (decisions.md gpu-first): the binary calls
    /// it as the shell starts, so the first animation is drawn by a device
    /// whose pipelines are built. Reports [`BackendChange::Start`] when it
    /// asks for one; nothing while the device is up or starting, after a
    /// failure under [`crate::promote::RETRY_AFTER`] old, or once the GPU
    /// is off.
    pub fn warm_gpu(&mut self) {
        self.gpu.keep_warm = true;
        if !self.gpu.device.idle() {
            return;
        }
        if self.gpu.want(Instant::now()) {
            self.gpu.warming = true;
            self.gpu.changes.push(BackendChange::Start);
        }
    }

    /// (gpu-first review) No device can exist in this process (the binary
    /// found no render node and no software adapter is accepted): none is
    /// ever asked for, nothing is promoted, and the status says `reason`.
    /// Nothing Vulkan is loaded, so the shell costs what it did before
    /// gpu-first.
    pub fn gpu_absent(&mut self, reason: &str) {
        self.gpu.device.absent();
        if self.gpu.device.is_absent() {
            log::info!("GPU: {reason}; everything draws on the CPU");
            self.gpu.status = GpuStatus::Unavailable {
                reason: reason.to_string(),
            };
        }
    }

    /// (M4) Turns promotion off or on (tests: a test of passes on CPU
    /// surfaces whose frames' wall-clock spacing must not matter;
    /// promotion's own timing is `promote.rs`'s).
    #[doc(hidden)]
    pub fn set_promotion(&mut self, on: bool) {
        self.gpu.no_promotion = !on;
    }

    /// (M4) How long a failed or lost device waits before it is asked
    /// for again (30 s; tests shorten it, so a retry-and-lose cycle runs
    /// without the wait hiding what the GPU-off cap does).
    #[doc(hidden)]
    pub fn set_gpu_retry(&mut self, retry: Duration) {
        self.gpu.device.set_retry(retry);
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
        s.presenting = false;
        s.pixels = None;
        s.hold = None;
        s.shown = None;
        self.gpu.fades.remove(&surface);
        if backend.is_gpu() {
            if !s.promo.on_gpu() {
                // Attached for a promotion a failure has since undone.
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
            // A hold taken on the CPU was committed by the manager.
            if backend == Backend::GpuPresent
                && let Some(s) = self.gpu.surfaces.get_mut(&surface)
            {
                s.committed = st.size_hold.map(|(t, _)| t);
            }
        }
    }

    /// (gpu-first review 2) After the surface changes were handed to the
    /// manager ([`Renderer::take_surface_changes`]): a presented surface
    /// held for its new size has that size pending on its layer surface,
    /// which only a present commits (the manager commits a surface the
    /// GPU thread presents no more), so the GPU thread presents its last
    /// frame again ([`GpuRequest::Commit`]), once per size. The configure
    /// then comes as a CPU surface's does, and the new layout is drawn
    /// at its new size: drawn at once instead, into the old size, the
    /// launcher's growing mode switches showed a frame or two shifted by
    /// half the growth and clipped on the far side.
    pub(super) fn gpu_commit_holds(&mut self) {
        for (id, st) in &self.surfaces {
            let Some(s) = self.gpu.surfaces.get_mut(id) else {
                continue;
            };
            if s.backend != Backend::GpuPresent {
                continue;
            }
            let Some((target, _)) = st.size_hold else {
                s.committed = None;
                continue;
            };
            if s.committed != Some(target) {
                s.committed = Some(target);
                self.gpu.out.push(GpuRequest::Commit(*id));
            }
        }
    }

    /// (M4) A reply from the GPU thread.
    pub fn deliver_gpu(&mut self, reply: GpuReply) {
        let now = Instant::now();
        match reply {
            GpuReply::Ready(info) => {
                self.gpu.warned = None;
                self.gpu.warming = false;
                self.gpu.rewarmed = false;
                self.gpu.device.up();
                self.gpu.status = GpuStatus::Up(info);
                // Passes wanted while it started.
                for s in self.surfaces.values_mut() {
                    s.mark_dirty();
                }
            }
            GpuReply::Unavailable(ref e) | GpuReply::Lost(ref e) => {
                // Asked again every 30 s while demand lasts: a reason that
                // cannot change (no adapter, only a software one) is
                // said once. The warm start's, with nothing shown that
                // needs the GPU, is no fault: everything draws on the CPU,
                // and demand later says it as a warning.
                let warm = std::mem::take(&mut self.gpu.warming)
                    && self.gpu.demand.is_empty()
                    && !self.gpu.surfaces.values().any(|s| s.promo.on_gpu());
                if warm && matches!(reply, GpuReply::Unavailable(_)) {
                    log::info!("GPU: {e}; everything draws on the CPU");
                } else if self.gpu.warned.as_deref() == Some(e.message.as_str()) {
                    log::debug!("GPU: {e}");
                } else {
                    log::warn!("GPU: {e}");
                    self.gpu.warned = Some(e.message.clone());
                    self.gpu.warnings += 1;
                }
                // The third lost device turns the GPU off for the process
                // (m4-close-gpucap): the status says so from then on, and
                // the host tells it once (`gpu_off`).
                // No adapter, or only a software one refused: no device
                // can exist in this process, and none is asked for again
                // (gpu-first review).
                if matches!(reply, GpuReply::Lost(_)) {
                    self.gpu.device.lost(now);
                } else if matches!(e.kind, GpuErrorKind::NoAdapter | GpuErrorKind::Software) {
                    self.gpu.device.absent();
                } else {
                    self.gpu.device.failed(now);
                }
                let reason = if self.gpu.device.is_off() {
                    GPU_OFF.to_string()
                } else {
                    e.message.clone()
                };
                self.gpu.status = GpuStatus::Unavailable { reason };
                self.repaint_fallen();
            }
            GpuReply::Exited => {
                self.gpu.warming = false;
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
            GpuReply::Presented {
                surface,
                frame,
                at,
                waited,
                ran,
            } => {
                if let Some(s) = self.gpu.surfaces.get_mut(&surface) {
                    // The frame's time: its paint on main and the GPU
                    // thread's work to its submission, less the thread's
                    // queue and swapchain waits (other surfaces' work, the
                    // compositor's pacing), then the GPU's run of it,
                    // which comes with a later `Presented`.
                    if s.presenting
                        && let Some(t) = s.started.take()
                    {
                        if s.untimed.len() == UNTIMED {
                            s.untimed.pop_front();
                        }
                        let cpu = at.saturating_duration_since(t).saturating_sub(waited);
                        s.untimed.push_back((frame, cpu));
                    }
                    for (done, gpu) in ran {
                        while let Some(&(f, cpu)) = s.untimed.front()
                            && f <= done
                        {
                            s.untimed.pop_front();
                            if f == done && self.surfaces.contains_key(&surface) {
                                self.frame_stats.record(surface, cpu + gpu, true);
                                if let Some(t) = &mut self.gpu.timed {
                                    t.push((surface, cpu, gpu));
                                }
                            }
                        }
                    }
                    s.presenting = false;
                }
            }
            GpuReply::Pixels {
                surface,
                frame,
                pixels,
                waited,
            } => {
                if let Some(s) = self.gpu.surfaces.get_mut(&surface)
                    && s.inflight.is_some_and(|(f, _)| f == frame)
                {
                    s.inflight = None;
                    s.hold = None;
                    // The frame's time: its paint, the GPU's render and
                    // the readback (it is shown at the next paint), less
                    // its wait in the GPU thread's queue.
                    if let Some(t) = s.started.take()
                        && self.surfaces.contains_key(&surface)
                    {
                        let took = now.saturating_duration_since(t).saturating_sub(waited);
                        self.frame_stats.record(surface, took, true);
                        if let Some(t) = &mut self.gpu.timed {
                            t.push((surface, took, Duration::ZERO));
                        }
                    }
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
                let id = PassId::unwire(key);
                let Some(p) = self.gpu.pending.remove(&id).filter(|p| p.frame == frame) else {
                    return;
                };
                if let Some(pm) = readback_pixmap(&pixels) {
                    let r = PassResult {
                        got: (p.key, Arc::new(pm)),
                        surface: p.surface,
                        failed: false,
                        base: p.base,
                    };
                    self.extras.shaders.map.insert(id, r);
                    if id.slot == Slot::Filter {
                        self.extras.shaders.answered.insert(id.node);
                    }
                }
                if let Some(s) = self.gpu.surfaces.get_mut(&p.surface) {
                    s.hold = None;
                }
                self.mark_node_dirty(id.node);
            }
            GpuReply::Failed {
                surface,
                key,
                error,
            } => {
                log::warn!("GPU: {error}");
                if let Some(id) = key.map(PassId::unwire) {
                    // Not asked again until its inputs change: the result
                    // records the failed want with no pixels. A `shader`
                    // node then draws nothing; a bundled pass's node, its
                    // CPU version (`ShaderResults::usable`).
                    if let Some(p) = self.gpu.pending.remove(&id) {
                        if error.kind == GpuErrorKind::Lost {
                            self.extras.shaders.lose(p.code.clone());
                        }
                        let r = PassResult {
                            got: (p.key, empty_pixmap()),
                            surface: p.surface,
                            failed: true,
                            base: p.base,
                        };
                        self.extras.shaders.map.insert(id, r);
                        if let Some(s) = self.gpu.surfaces.get_mut(&p.surface) {
                            s.hold = None;
                        }
                    }
                    self.mark_node_dirty(id.node);
                }
                if let Some(surface) = surface
                    && let Some(s) = self.gpu.surfaces.get_mut(&surface)
                {
                    // A frame that hung or panicked (the device is lost
                    // with it) took its inline passes along: one of them
                    // may be the culprit, and the demoted surface would
                    // ask a new device for it as a pass of its own, to
                    // hang (and leak) that device too. Like a pass that
                    // lost the device, their files are not run again.
                    if key.is_none() && error.kind == GpuErrorKind::Lost {
                        for (id, key, base, code) in std::mem::take(&mut s.inline) {
                            self.extras.shaders.lose(code);
                            self.extras.shaders.map.insert(
                                id,
                                PassResult {
                                    got: (key, empty_pixmap()),
                                    surface,
                                    failed: true,
                                    base,
                                },
                            );
                        }
                    }
                    // A frame that cannot be drawn: back to the CPU.
                    s.inflight = None;
                    s.presenting = false;
                    s.hold = None;
                    s.shown = None;
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
        self.extras.shaders.device_gone();
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

    /// When the loop must wake for the GPU: a kept-warm device's retry
    /// after a failure that may pass (one wake: the start it makes ends in
    /// a device or in the next failure's deadline). The device is kept up
    /// (gpu-first) and a promoted surface stays promoted (gpu-stay), so
    /// nothing else: a promoted surface that paints nothing is as quiet as
    /// a CPU one.
    pub(super) fn gpu_wake(&self) -> Option<Instant> {
        (self.gpu.keep_warm && !self.gpu.rewarmed)
            .then(|| self.gpu.device.retry_at())
            .flatten()
    }

    /// The loop woke at `now`: a kept-warm device whose retry is due
    /// starts again, and what surfaces that went away left is forgotten.
    pub(super) fn gpu_tick(&mut self, now: Instant) {
        if self.gpu.keep_warm
            && !self.gpu.rewarmed
            && self.gpu.device.retry_at().is_some_and(|t| now >= t)
        {
            self.gpu.rewarmed = true;
            self.warm_gpu();
        }
        // Surfaces that went away.
        let live = &self.surfaces;
        self.gpu.surfaces.retain(|id, _| live.contains_key(id));
        self.gpu.demand.retain(|id| live.contains_key(id));
        self.gpu.pointer_users.retain(|id| live.contains_key(id));
        self.gpu.fades.retain(|id, _| live.contains_key(id));
        // Passes of surfaces that went away: their pixels have nowhere
        // to go.
        self.gpu
            .pending
            .retain(|_, p| live.contains_key(&p.surface));
        self.extras
            .shaders
            .map
            .retain(|_, r| live.contains_key(&r.surface));
    }

    /// After a `paint` of `surface` that began at `started`: whether it
    /// sent its frame to the GPU, whose reply then times it.
    pub(super) fn gpu_frame_sent(&mut self, surface: SurfaceId, started: Instant) -> bool {
        if !std::mem::take(&mut self.gpu.sent) {
            return false;
        }
        if let Some(s) = self.gpu.surfaces.get_mut(&surface) {
            s.started = Some(started);
        }
        true
    }

    /// A frame of `surface` damaged `damage` pixels with `springs` in
    /// flight: promotion's input.
    pub(super) fn gpu_frame_stats(&mut self, surface: SurfaceId, damage: u64, springs: bool) {
        if self.gpu.no_promotion {
            return;
        }
        let now = Instant::now();
        if self.gpu.device.is_off() || self.gpu.device.is_absent() {
            return;
        }
        // (centred-surface) A surface that spans its output draws only its
        // box: a GPU frame's whole buffer, or a first frame's, says nothing of
        // how much of it moves.
        let damage = self
            .surfaces
            .get(&surface)
            .filter(|st| self.spanned.contains(&st.root))
            .and_then(|st| {
                let r = st.boxes.as_ref()?.rects.get(&st.root).copied()?;
                let k = st.scale.as_f32();
                Some(damage.min((r.w * k * r.h * k) as u64))
            })
            .unwrap_or(damage);
        let s = self.gpu.surf(surface);
        if s.promo.frame(now, damage, springs) {
            if self.gpu.want(now) {
                self.gpu.changes.push(BackendChange::Promote(surface));
            } else if let Some(s) = self.gpu.surfaces.get_mut(&surface) {
                s.promo.force_cpu();
            }
        }
    }

    /// The boxes of the clocked passes (`strand.time`) of a surface the
    /// GPU draws. Its passes run in its GPU frames, so no new pixels come
    /// back to change their records as on a CPU surface, yet each frame
    /// draws them anew: their boxes are this frame's damage, so a
    /// presented aurora draws its next frame and a readback surface sends
    /// it.
    pub(super) fn gpu_clocked_damage(
        &self,
        surface: SurfaceId,
        items: &[DisplayItem],
        passes: &[PassWant],
    ) -> Damage {
        let mut out = Damage::new();
        if !self.backend(surface).is_gpu() || !passes.iter().any(|p| p.timed) {
            return out;
        }
        let timed: HashSet<NodeId> = passes
            .iter()
            .filter(|p| p.timed && p.id.slot == Slot::Node)
            .map(|p| p.id.node)
            .collect();
        for d in items {
            if let Item::Raster { node, .. } = &d.item
                && timed.contains(node)
            {
                out.add(d.bounds);
            }
        }
        out
    }

    /// The passes a frame of `surface` (its display list `items`) wants:
    /// those whose pixels are not their last are asked for (one in
    /// flight per pass), with their input drawn now. `hold`: the frame is
    /// not painted yet, so it may wait for them.
    pub(super) fn gpu_passes(
        &mut self,
        surface: SurfaceId,
        items: &[DisplayItem],
        wants: &[PassWant],
        hold: bool,
    ) {
        if wants.iter().any(|w| w.pointer) {
            self.gpu.pointer_users.insert(surface);
        } else {
            self.gpu.pointer_users.remove(&surface);
        }
        // A promoted surface's own passes run in its GPU frames, and so
        // do its backdrops but those the CPU draws with their groups
        // (`inframe_backdrops`); its layers' passes are read back like any
        // surface's.
        let gpu_drawn = self.backend(surface).is_gpu();
        let scale = self.surfaces.get(&surface).map(|s| s.scale);
        let (inframe, inframe_at): (HashSet<PassId>, HashSet<usize>) = match scale {
            Some(scale) if gpu_drawn => inframe_backdrops(items, wants, scale)
                .into_iter()
                .map(|(i, w)| (w.id, i))
                .unzip(),
            _ => Default::default(),
        };
        // Pixels this surface's frame no longer wants go (an effect
        // hidden, a tilt at rest, a node gone, a backdrop its GPU frames
        // draw), with or without other wants; a failure stays while its
        // node does, so that it is not asked again.
        {
            let wanted: HashSet<PassId> = wants
                .iter()
                .filter(|w| !w.inframe_only)
                .map(|w| w.id)
                .filter(|id| !inframe.contains(id))
                .collect();
            let pending = &self.gpu.pending;
            let tree = &self.tree;
            let results = &mut self.extras.shaders;
            results.map.retain(|id, r| {
                tree.get(id.node).is_some()
                    && (r.surface != surface
                        || r.failed
                        || wanted.contains(id)
                        || pending.contains_key(id))
            });
            results.answered.retain(|n| tree.get(*n).is_some());
        }
        if let Some(s) = self.gpu.surfaces.get_mut(&surface) {
            s.inline.clear();
            if gpu_drawn {
                s.inline.extend(
                    wants
                        .iter()
                        .filter(|w| w.id.slot == Slot::Node)
                        .map(|w| (w.id, w.key, w.base, file_code(w))),
                );
            }
        }
        if wants.is_empty() {
            self.gpu.demand.remove(&surface);
            return;
        }
        self.gpu.demand.insert(surface);
        let now = Instant::now();
        let mut wait = false;
        let mut prepared = false;
        for w in wants {
            if gpu_drawn && (w.id.slot == Slot::Node || inframe.contains(&w.id)) {
                continue;
            }
            // (review) A small backdrop blur no GPU frame draws is the
            // CPU's, at its reduction, as on a surface the CPU draws.
            if w.inframe_only {
                continue;
            }
            let last = self.extras.shaders.map.get(&w.id);
            let have = last.map(|r| r.got.0);
            // A failure is not asked again while its `base` stays (a
            // clocked pass's key changes every frame).
            let stuck = last.is_some_and(|r| r.failed && r.base == w.base);
            if have == Some(w.key) || stuck || self.gpu.pending.contains_key(&w.id) {
                continue;
            }
            if !self.gpu.want(now) {
                // No device (a failure under 30 s old): nothing to draw.
                continue;
            }
            let input = match w.input {
                PassInput::None => None,
                PassInput::Group(_) | PassInput::Behind(_) => {
                    if !prepared {
                        self.prepare_groups(surface, items, &inframe_at);
                        prepared = true;
                    }
                    match self.pass_input(surface, items, w) {
                        Some(px) => Some(px),
                        None => continue,
                    }
                }
            };
            let frame = self.gpu.frame_id();
            self.gpu.out.push(GpuRequest::Pass(PassFrame {
                key: w.id.wire(),
                id: frame,
                size: w.size,
                pass: w.pass.clone(),
                then: w.then.clone(),
                globals: w.globals,
                input,
            }));
            self.gpu.pending.insert(
                w.id,
                Pending {
                    frame,
                    key: w.key,
                    base: w.base,
                    surface,
                    code: file_code(w),
                },
            );
            // Only an up device answers within the wait; a clocked pass
            // is pipelined, and a pass with pixels keeps them meanwhile
            // only if it is clocked.
            if self.gpu.device.is_up() && (have.is_none() || !w.timed) {
                wait = true;
            }
        }
        if wait && hold {
            let s = self.gpu.surf(surface);
            s.hold = Some(now + GPU_WAIT);
        }
    }

    /// Draws this frame's offscreen groups of `surface` (a pass's input
    /// may hold or show them).
    /// The backdrops its GPU frames draw (pushed at `inframe`) are left
    /// out.
    fn prepare_groups(
        &mut self,
        surface: SurfaceId,
        items: &[DisplayItem],
        inframe: &HashSet<usize>,
    ) {
        let Some((size, scale)) = self.surfaces.get(&surface).map(|s| (s.size, s.scale)) else {
            return;
        };
        let (cache, offscreen) = self.raster.gpu_parts();
        offscreen.prepare_except(
            items,
            &Damage::full(size),
            Rect::from_size(size),
            &self.atlas,
            &*cache,
            scale,
            &|i| inframe.contains(&i),
        );
    }

    /// The pixels pass `w` reads: its layer's group, CPU-filtered but not
    /// by its bundled passes, or what is drawn behind its backdrop.
    fn pass_input(
        &mut self,
        surface: SurfaceId,
        items: &[DisplayItem],
        w: &PassWant,
    ) -> Option<Arc<Pixmap>> {
        let scale = self.surfaces.get(&surface)?.scale;
        let region = Rect {
            x: w.at.0,
            y: w.at.1,
            w: w.size.w,
            h: w.size.h,
        };
        let (cache, offscreen) = self.raster.gpu_parts();
        let groups = offscreen.current();
        match w.input {
            PassInput::None => None,
            PassInput::Group(i) => {
                let Item::PushLayer(layer) = &items.get(i)?.item else {
                    return None;
                };
                let end = crate::raster::skip_group(items, i);
                let inner = &items[i + 1..end.saturating_sub(1).max(i + 1)];
                let d = crate::offscreen::render_group(
                    inner,
                    layer,
                    region,
                    &self.atlas,
                    &*cache,
                    scale,
                    groups,
                    false,
                )?;
                Some(d.pixmap)
            }
            PassInput::Behind(i) => {
                let below = crate::backdrop::behind(items, i, region);
                let pm = crate::offscreen::rasterise(
                    &below,
                    Affine::IDENTITY,
                    region,
                    &self.atlas,
                    &*cache,
                    scale,
                    groups,
                )?;
                Some(Arc::new(pm))
            }
        }
    }

    /// Paints a frame of a `GpuReadback` surface: copies the last pixels
    /// in (`Some`: full damage) and sends this frame when its scene
    /// changed (`changed`); `None` when the CPU must draw it (no pixels
    /// yet, pixels of another size after a resize, or a frame still in
    /// flight past [`GPU_WAIT`]).
    pub(super) fn gpu_readback_paint(
        &mut self,
        surface: SurfaceId,
        items: &[DisplayItem],
        passes: &[PassWant],
        changed: bool,
        target: &mut PaintTarget<'_>,
    ) -> Option<Damage> {
        let now = Instant::now();
        // Frames run one behind: pixels drawn before a resize are not
        // this frame's (copied in, they would fill only a corner).
        let ready = self
            .gpu
            .surfaces
            .get_mut(&surface)?
            .pixels
            .take()
            .filter(|px| px.width == target.size.w && px.height == target.size.h);
        let inflight = self.gpu.surfaces.get(&surface)?.inflight;
        let fresh = ready.is_some() || inflight.is_none();
        if fresh && (changed || ready.is_none()) {
            let id = self.gpu.frame_id();
            let frame = self.lower_frame(surface, id, items, passes, target.size, target.scale);
            self.gpu.out.push(GpuRequest::Frame(frame));
            self.gpu.sent = true;
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

    /// (presented-quiet) Whether [`Renderer::paint_gpu`] would paint
    /// `surface` now, and if so, its refresh period. Its paint may still
    /// return no frame, when it draws nothing.
    pub fn presented_due(&self, surface: SurfaceId) -> Option<Duration> {
        (self.backend(surface) == Backend::GpuPresent
            && self.wants(surface)
            && self
                .surfaces
                .get(&surface)
                .is_some_and(|s| !s.size.is_empty()))
        .then(|| self.clocks.period(surface))
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
        if frame.is_some()
            && let Some(s) = self.gpu.surfaces.get_mut(&surface)
        {
            s.presenting = true;
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
        let rect = kurbo::Rect::new(0.0, 0.0, size.w as f64, size.h as f64);
        // (perf-gpu-retain) The snapshot the GPU thread holds (its last
        // frame), else the CPU's, uploaded.
        match fade.map(|w| (w, self.gpu_snapshot(surface, size))) {
            Some((w, Some(how))) => {
                self.gpu.fades.remove(&surface);
                frame.snapshot = how;
                frame.ops = crossfade_ops(std::mem::take(&mut frame.ops), Op::Snapshot { rect }, w);
            }
            Some((w, None)) => match self.fade_pixmap(surface, size) {
                Some(pm) => {
                    let image = self.gpu.uploads.id(&pm, &mut frame.uploads);
                    frame.ops =
                        crossfade_ops(std::mem::take(&mut frame.ops), image_op(rect, image), w);
                }
                None => {
                    self.gpu.fades.remove(&surface);
                }
            },
            None => {
                self.gpu.fades.remove(&surface);
            }
        }
        if let Some(s) = self.gpu.surfaces.get_mut(&surface) {
            s.shown = Some(size);
            s.snap = frame.snapshot;
        }
        self.gpu.present = Some(frame);
        self.gpu.sent = true;
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

    /// (perf-gpu-retain) True if `surface` is presented and the GPU
    /// thread holds its last frame at `size` (a crossfade takes it as its
    /// snapshot).
    pub(super) fn gpu_shows(&self, surface: SurfaceId, size: Size) -> bool {
        self.gpu
            .surfaces
            .get(&surface)
            .is_some_and(|s| s.backend == Backend::GpuPresent && s.shown == Some(size))
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
        let lowering = Instant::now();
        let mut frame = lower(
            &mut self.raster,
            &mut self.gpu.uploads,
            &self.atlas,
            (surface, id),
            items,
            passes,
            size,
            scale,
        );
        frame.retire = self.gpu.uploads.retire();
        self.paint_times.lower += lowering.elapsed();
        frame
    }

    /// (eval-sandbox) `surface`'s last painted frame lowered again, as
    /// for a GPU that holds nothing yet: by a raster with nothing cached
    /// and an upload table of its own, so every pixmap it draws is in its
    /// uploads (numbered from `first_upload + 1`, retiring nothing), with
    /// the crossfade it was blended with. The `redraw` check draws it on a
    /// device of its own and compares it with what the presented frame
    /// drew. `id` is its frame number. It reads the renderer and changes
    /// nothing a later frame reads (`&self`). `None` when no frame is kept
    /// for the surface (`Renderer::set_keep_frames`).
    #[cfg(feature = "eval-hooks")]
    pub fn fresh_gpu_frame(&self, surface: SurfaceId, id: u64, first_upload: u64) -> Option<Frame> {
        let last = self.probe.last.get(&surface)?;
        let mut raster = self.raster.fresh();
        let mut uploads = Uploads {
            next: first_upload,
            ..Uploads::default()
        };
        let mut frame = lower(
            &mut raster,
            &mut uploads,
            &self.atlas,
            (surface, id),
            &last.items,
            &last.passes,
            last.size,
            last.scale,
        );
        // Drawn anew: no backdrop's pixels from the device's last frame.
        uncached(&mut frame.ops);
        // As `gpu_present_paint` blends: over the snapshot the GPU holds
        // (the fresh device's own last frame, the same frame drawn fresh),
        // or over the CPU's, when there is one to upload.
        let rect = kurbo::Rect::new(0.0, 0.0, last.size.w as f64, last.size.h as f64);
        let snap = self
            .gpu
            .surfaces
            .get(&surface)
            .map_or(strand_gpu::FrameSnapshot::None, |s| s.snap);
        if let Some(w) = last.fade
            && snap != strand_gpu::FrameSnapshot::None
        {
            frame.snapshot = snap;
            frame.ops = crossfade_ops(std::mem::take(&mut frame.ops), Op::Snapshot { rect }, w);
        } else if let Some(w) = last.fade
            && let Some(px) = self.fade_pixels(surface, last.size)
            && let (Ok(pw), Ok(ph)) = (u16::try_from(last.size.w), u16::try_from(last.size.h))
        {
            let mut pm = Pixmap::new(pw, ph);
            let bytes = pm.data_as_u8_slice_mut();
            if bytes.len() == px.len() {
                bytes.copy_from_slice(px);
                let image = uploads.id(&Arc::new(pm), &mut frame.uploads);
                frame.ops = crossfade_ops(std::mem::take(&mut frame.ops), image_op(rect, image), w);
            }
        }
        Some(frame)
    }
}

/// Lowers `items` to a GPU frame of `surface` numbered `id` with `raster`'s
/// caches and the pixmaps `uploads` holds (`retire` left empty).
#[allow(clippy::too_many_arguments)]
fn lower(
    raster: &mut crate::raster::Raster,
    uploads: &mut Uploads,
    atlas: &AtlasMirror,
    (surface, id): (SurfaceId, u64),
    items: &[DisplayItem],
    passes: &[PassWant],
    size: Size,
    scale: Scale,
) -> Frame {
    let bounds = Rect::from_size(size);
    let (cache, offscreen) = raster.gpu_parts();
    // A frame of its own: what the last one built uncached (a sliced
    // shadow's corner tiles) is cached if this one asks again.
    cache.begin_frame();
    // The backdrops the frame draws itself (no CPU input, no readback).
    let inframe = inframe_backdrops(items, passes, scale);
    // Groups the CPU filters (blur, colour matrix), drawn whole.
    offscreen.prepare_except(
        items,
        &Damage::full(size),
        bounds,
        atlas,
        &*cache,
        scale,
        &|i| inframe.contains_key(&i),
    );
    // The surface's own passes, run in the frame.
    let passes: HashMap<NodeId, &PassWant> = passes
        .iter()
        .filter(|p| p.id.slot == Slot::Node)
        .map(|p| (p.id.node, p))
        .collect();
    let mut l = Lowering {
        atlas,
        cache,
        groups: offscreen.current(),
        scale,
        surface: bounds,
        uploads,
        passes: &passes,
        inframe: &inframe,
        ops: Vec::new(),
        new: Vec::new(),
    };
    l.uploads.begin();
    l.lower(items, items.len(), None);
    Frame {
        surface,
        id,
        size,
        scale,
        ops: l.ops,
        uploads: l.new,
        retire: Vec::new(),
        clear: AlphaColor::TRANSPARENT,
        snapshot: strand_gpu::FrameSnapshot::None,
    }
}

/// (perf-gpu-retain) The backdrops a GPU frame of `items` draws itself
/// ([`Op::Backdrop`]), by the index of their layer, with the wants they
/// answer: those whose pass reads what is behind them, unless an
/// offscreen group the CPU draws holds them (a `filter: blur`, a mask:
/// it draws them with its own items) or a backdrop the CPU draws later
/// reads them (it reads their pixels from the CPU's groups). The rest
/// keep the CPU's input, readback and upload.
pub(crate) fn inframe_backdrops<'w>(
    items: &[DisplayItem],
    wants: &'w [PassWant],
    scale: Scale,
) -> HashMap<usize, &'w PassWant> {
    let wanted: HashMap<usize, &PassWant> = wants
        .iter()
        .filter(|w| w.id.slot == Slot::Backdrop)
        .filter_map(|w| match w.input {
            PassInput::Behind(i) => Some((i, w)),
            _ => None,
        })
        .collect();
    if wanted.is_empty() {
        return HashMap::new();
    }
    // Whether an offscreen group the CPU draws holds each item.
    let cpu_group = |d: &DisplayItem| match &d.item {
        Item::PushLayer(l) => {
            !l.cell_local()
                || l.effects
                    .iter()
                    .any(|e| matches!(e, strand_scene::Effect::Mask(_)))
        }
        _ => false,
    };
    let mut held = vec![false; items.len()];
    let mut open: Vec<bool> = Vec::new();
    for (j, d) in items.iter().enumerate() {
        let inside = open.last().copied().unwrap_or(false);
        held[j] = inside;
        match &d.item {
            Item::PushClip(_) | Item::PushOpacity(..) | Item::PushTransform(_) => open.push(inside),
            Item::PushLayer(_) => open.push(inside || cpu_group(d)),
            Item::PopClip | Item::PopOpacity | Item::PopTransform | Item::PopLayer => {
                open.pop();
            }
            _ => {}
        }
    }
    // Last first: whether a later backdrop the CPU draws reads one is
    // known when it is reached.
    let mut out = HashMap::new();
    let mut cpu_reads: Vec<Rect> = Vec::new();
    for (j, d) in items.iter().enumerate().rev() {
        let Item::PushLayer(l) = &d.item else {
            continue;
        };
        let cpu = crate::backdrop::pass(l, scale);
        let want = wanted.get(&j).copied();
        if cpu.is_none() && want.is_none() {
            continue;
        }
        let read_by_cpu = cpu_reads.iter().any(|r| r.intersects(d.bounds));
        match want {
            Some(w) if !held[j] && !read_by_cpu => {
                out.insert(j, w);
            }
            _ => cpu_reads.push(match cpu {
                Some(p) => d.bounds.inflate(p.reach()),
                None => d.bounds,
            }),
        }
    }
    out
}

/// A crossfade frame's ops: the snapshot drawn by `under` (an upload,
/// or the snapshot the GPU thread holds) weighted `1 - w`, and the new
/// frame's `ops` added over it weighted `w`. Both start at the identity
/// transform.
fn crossfade_ops(ops: Vec<Op>, under: Op, w: f32) -> Vec<Op> {
    use vello_cpu::peniko::{BlendMode, Compose, Mix};
    let w = if w.is_finite() {
        w.clamp(0.0, 1.0)
    } else {
        1.0
    };
    let mut out = Vec::with_capacity(ops.len() + 7);
    out.push(Op::Transform(Affine::IDENTITY));
    out.push(Op::PushLayer(strand_gpu::Layer {
        opacity: Some(1.0 - w),
        ..Default::default()
    }));
    out.push(under);
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

/// (eval-sandbox) Every backdrop among `ops` drawn anew
/// (`Op::Backdrop`'s key 0).
#[cfg(feature = "eval-hooks")]
fn uncached(ops: &mut [Op]) {
    for op in ops {
        if let Op::Backdrop { key, ops, .. } = op {
            *key = 0;
            uncached(ops);
        }
    }
}

/// Upload `image` drawn 1:1 over `rect` from the origin.
fn image_op(rect: kurbo::Rect, image: u64) -> Op {
    Op::Image {
        rect,
        image,
        image_transform: Affine::IDENTITY,
        tint: None,
        smooth: false,
    }
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
    /// (perf-gpu-retain) The backdrops drawn in the frame, by their
    /// layer's index ([`inframe_backdrops`]).
    inframe: &'a HashMap<usize, &'a PassWant>,
    ops: Vec<Op>,
    new: Vec<Upload>,
}

impl Lowering<'_> {
    /// Lowers the items before `end`, those `keep` keeps (all if
    /// `None`), from the identity transform.
    fn lower(&mut self, items: &[DisplayItem], end: usize, keep: Option<&[bool]>) {
        let mut cur = Affine::IDENTITY;
        let mut saved: Vec<Affine> = Vec::new();
        let end = end.min(items.len());
        let mut i = 0;
        while i < end {
            let d = &items[i];
            i += 1;
            if keep.is_some_and(|k| !k.get(i - 1).copied().unwrap_or(false)) {
                continue;
            }
            match &d.item {
                Item::PushClip(_)
                | Item::PushOpacity(..)
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
                Item::PushOpacity(o, _) => self.ops.push(Op::PushLayer(strand_gpu::Layer {
                    opacity: Some(*o),
                    ..Default::default()
                })),
                Item::PopOpacity => self.ops.push(Op::PopLayer),
                Item::PushLayer(l) if self.inframe.contains_key(&(i - 1)) => {
                    let group_end = crate::raster::skip_group(items, i - 1);
                    if let Some(w) = self.inframe.get(&(i - 1)).copied() {
                        self.backdrop(items, i - 1, w, l, cur);
                    }
                    i = group_end;
                }
                Item::PushLayer(l) => {
                    let end = crate::raster::skip_group(items, i - 1);
                    // (Every mask, the shape library's outlines too.)
                    let masked = l
                        .effects
                        .iter()
                        .any(|e| matches!(e, strand_scene::Effect::Mask(_)));
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
                    // (perf-gpu-retain) Drawn by the GPU with the CPU's
                    // primitive, as `cache::draw_shadow` draws it: no
                    // tiles built, nothing uploaded, whatever its size.
                    self.ops.push(Op::PushClipEvenOdd(clip.clone()));
                    shadow_ops(&mut self.ops, rect, radii, *std_dev, *color, extent);
                    self.ops.push(Op::PopClip);
                }
                Item::Fill {
                    shape,
                    paint,
                    frame,
                } => {
                    let (brush, brush_transform) = brush(paint, *frame);
                    let (path, even_odd) = match shape {
                        FillShape::Rect(r) => (r.to_path(0.1), false),
                        FillShape::Path(p) => (p.clone(), false),
                        FillShape::EvenOdd(p) => (p.clone(), true),
                    };
                    self.ops.push(Op::Fill {
                        path,
                        brush,
                        brush_transform,
                        even_odd,
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
                // (M4) Text painted with a `fill:` paint is drawn on the
                // CPU, like a masked group: its coverage times the paint.
                Item::Glyphs { fill: Some(_), .. } => {
                    self.island(&items[i - 1..i], d.bounds, cur);
                }
                Item::Glyphs {
                    x,
                    y,
                    layout,
                    color,
                    spans,
                    fill: None,
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

    /// (perf-gpu-retain) The backdrop layer `l` pushed at item `at`, for
    /// want `w`: what is drawn behind it over the want's region, lowered
    /// into the backdrop's own ops (as `pass_input` rasterises it for a
    /// pass of its own), and its pass over them, in the frame.
    fn backdrop(
        &mut self,
        items: &[DisplayItem],
        at: usize,
        w: &PassWant,
        l: &crate::layers::Layer,
        cur: Affine,
    ) {
        let region = Rect {
            x: w.at.0,
            y: w.at.1,
            w: w.size.w,
            h: w.size.h,
        };
        let keep = crate::backdrop::behind_mask(items, at, region);
        let outer = std::mem::take(&mut self.ops);
        let surface = std::mem::replace(&mut self.surface, region);
        self.lower(items, at, Some(&keep));
        let behind = std::mem::replace(&mut self.ops, outer);
        self.surface = surface;
        // What its pixels depend on: its want's key (its passes, size,
        // globals and what is behind it) and where it is; none when what
        // is behind it runs passes of its own (their pixels move with
        // their clocks).
        // (review) With the pixels of each group among them the CPU
        // filtered (drawn as uploads, whose items it reads past the
        // region), as `backdrop::key` pins them: the want's key hashes
        // the items over the region and their groups.
        let key = if behind
            .iter()
            .any(|o| matches!(o, Op::Pass { .. } | Op::Backdrop { .. }))
        {
            0
        } else {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (w.key, w.at, w.size.w, w.size.h, w.reduce).hash(&mut h);
            for (d, _) in items.iter().zip(&keep).filter(|(_, k)| **k) {
                if let Item::PushLayer(g) = &d.item
                    && let Some(n) = self.groups.get(&layer_key(g))
                {
                    (Arc::as_ptr(&n.pixmap) as usize, n.x, n.y).hash(&mut h);
                }
            }
            h.finish().max(1)
        };
        let (blend, opacity) = layer_parts(l);
        let (x, y) = (f64::from(region.x), f64::from(region.y));
        self.ops.push(Op::PushLayer(strand_gpu::Layer {
            blend,
            opacity,
            ..Default::default()
        }));
        self.ops.push(Op::Transform(Affine::IDENTITY));
        self.ops.push(Op::Backdrop {
            region: kurbo::Rect::new(x, y, x + f64::from(region.w), y + f64::from(region.h)),
            ops: behind,
            pass: w.pass.clone(),
            then: w.then.clone(),
            globals: w.globals,
            key,
            reduce: w.reduce,
        });
        self.ops.push(Op::Transform(cur));
        self.ops.push(Op::PopLayer);
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
            &std::collections::HashMap::new(),
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

/// A box shadow's ops, as [`crate::cache::draw_shadow`] draws it: one
/// blurred rounded rect, or, when its corners' radii differ, one per
/// quadrant of `extent` split at the box's rounded centre.
fn shadow_ops(
    ops: &mut Vec<Op>,
    rect: &kurbo::Rect,
    radii: &[f32; 4],
    std_dev: f32,
    color: Color,
    extent: &kurbo::Rect,
) {
    let color = straight(color);
    if radii.iter().all(|r| *r == radii[0]) {
        ops.push(Op::BlurredRect {
            rect: *rect,
            radius: radii[0],
            std_dev,
            color,
        });
        return;
    }
    let (mx, my) = (rect.center().x.round(), rect.center().y.round());
    let quads = [
        kurbo::Rect::new(extent.x0, extent.y0, mx, my),
        kurbo::Rect::new(mx, extent.y0, extent.x1, my),
        kurbo::Rect::new(mx, my, extent.x1, extent.y1),
        kurbo::Rect::new(extent.x0, my, mx, extent.y1),
    ];
    for (q, r) in quads.iter().zip(radii) {
        if q.width() <= 0.0 || q.height() <= 0.0 {
            continue;
        }
        ops.push(Op::PushClip(q.to_path(0.1)));
        ops.push(Op::BlurredRect {
            rect: *rect,
            radius: *r,
            std_dev,
            color,
        });
        ops.push(Op::PopClip);
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

/// The gradient's stops as the CPU's vello paints take them
/// ([`crate::raster::ramp`]), in straight colours.
fn stops(stops: &[GradientStop]) -> Vec<(f32, AlphaColor<Srgb>)> {
    crate::raster::ramp(stops)
        .into_iter()
        .map(|(o, c)| (o, straight(c)))
        .collect()
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
                PropValue::Color(Color::new(0.5, 1.0, 0.0, 0.5)),
            ),
        ];
        let p = pack(&code(), &entries, 2.0);
        assert_eq!(p.len(), 8);
        assert_eq!(p[0], 12.0, "px × scale");
        assert_eq!(&p[1..5], &[0.25, 0.5, 0.0, 0.5], "premultiplied sRGB");
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
    fn pass_ids_round_trip_on_the_wire() {
        for slot in [Slot::Node, Slot::Filter, Slot::Backdrop] {
            let id = PassId::new(NodeId::new(7, 3), slot);
            assert_eq!(PassId::unwire(id.wire()), id);
        }
        let big = PassId::new(NodeId::new(1 << 29, u32::MAX), Slot::Backdrop);
        assert_eq!(PassId::unwire(big.wire()), big);
    }

    const P: [f32; 2] = [-1.0, -1.0];

    #[test]
    fn a_clocked_pass_changes_with_time_and_a_static_one_does_not() {
        let c = code();
        let a = pass_want(NodeId::new(3, 0), &c, None, (0, 0), 10, 10, 1.0, 0.5, P).unwrap();
        let b = pass_want(NodeId::new(3, 0), &c, None, (0, 0), 10, 10, 1.0, 0.6, P).unwrap();
        assert!(a.timed);
        assert_ne!(a.key, b.key);
        let still = Arc::new(ShaderCode {
            wgsl: "@fragment fn main() -> @location(0) vec4<f32> { return vec4<f32>(1.0); }".into(),
            ..(*c).clone()
        });
        let a = pass_want(NodeId::new(3, 0), &still, None, (0, 0), 10, 10, 1.0, 0.5, P).unwrap();
        let b = pass_want(NodeId::new(3, 0), &still, None, (0, 0), 10, 10, 1.0, 0.6, P).unwrap();
        assert!(!a.timed);
        assert_eq!(a.key, b.key, "time is 0 for a pass that does not read it");
        assert_eq!(a.globals.time, 0.0);
        assert!(pass_want(NodeId::new(3, 0), &still, None, (0, 0), 0, 10, 1.0, 0.0, P).is_none());
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
