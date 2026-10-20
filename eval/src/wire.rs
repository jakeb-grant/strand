//! The surface leg (docs/eval.md, "Surface leg"; docs/decisions.md
//! eval-surface-leg): a run's frames handed through the real surface
//! manager (`strand_surface::SurfaceManager`) to an in-process fake
//! compositor (`strand_fake_wayland`) on the run's virtual clock, and
//! what the compositor received recorded per commit ([`WireLog`]).
//!
//! The harness stays the scheduler: it paints each tick as it always
//! does, and the leg mirrors each painted frame to the manager. The
//! manager's painter ([`WireHost`]) answers with that frame, as the
//! renderer answered right after painting it: its damage, whether it
//! wants another, and its `Painter` regions and pose. A CPU frame is the
//! manager's to paint and commit (`draw`, through its shm buffers); a GPU
//! frame goes as the shell's host sends one (crates/strand/src/run/gpu.rs
//! `GpuHost::present`): the surface lent (`State::raw_handles`) and
//! handed off (`State::hand_off`) the first time, then
//! `State::sync_presented` after the frame, then a present on the lent
//! `wl_surface` by a stand-in for the GPU thread's WSI (a buffer of the
//! surface's size, its whole damage and a frame callback). A surface's
//! first frame is the manager's whatever draws it: one it never
//! committed is not mapped for it (no popup opens on it), and in the
//! shell a surface is promoted to the GPU only after frames of its own on
//! the CPU. Back on the CPU, the surface is taken back
//! (`State::take_back`) as `Released` does, and so is one the manager has
//! to destroy while handed off (`SurfaceHost::gpu_release`).
//!
//! The fake runs in vblank mode: nothing is shown, no frame callback is
//! done and no feedback presented until the leg's refresh at the end of
//! each tick, at the tick's time on the fake's presentation clock (the
//! manager's `FakeClock` reads the same virtual time). A commit made
//! during tick `k` is shown at refresh `k`; one the manager could only
//! make after it is late.
//!
//! What the leg does not model: the buffer holds no pixels (the fake
//! reads none), a CPU frame's damage is the harness buffer's (age 1),
//! the stand-in WSI presents at once (the GPU thread's own latency is
//! strand-gpu's), the scale is 1 (the fake's output has no other), and
//! no layer surface is sized by another's exclusive zone (the fake
//! stretches each over the whole output; a run that needs a zone is
//! refused, eval-false-alarms).

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use strand_fake_wayland::{BufferKind, CommitRecord, Fake, Region, SurfaceGlobals};
use strand_scene::{
    BlurRegion, Damage, LogicalRect, NodeId, PaintTarget, Painter, Scale, Screens, Size,
    SurfaceChange, SurfaceId, SurfacePose,
};
use strand_surface::{Config, FakeClock, SurfaceHost, SurfaceManager};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer::WlBuffer, wl_callback::WlCallback, wl_registry::WlRegistry, wl_shm, wl_shm::WlShm,
    wl_shm_pool::WlShmPool, wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};

use crate::Error;
use crate::scenario::Screen;
use crate::surfaces::{OUTPUT, SCREEN_ID};

/// How long the fake may take to answer one barrier.
const WAIT: Duration = Duration::from_secs(20);
/// Barrier rounds before the manager is called unsettled.
const ROUNDS: usize = 64;

/// How a frame reached the compositor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Path {
    /// Painted and committed by the surface manager, through its shm
    /// buffers.
    Shm,
    /// Presented on the handed-off surface by the GPU thread's WSI (the
    /// leg's stand-in).
    Gpu,
}

/// A rectangle `[x, y, w, h]`.
pub type R = [i32; 4];

/// What the surface leg recorded: every frame the renderer drew, with
/// the regions its painter computed (in the compositor's coordinates),
/// every commit the compositor took, and each refresh.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WireLog {
    pub frames: Vec<WireFrame>,
    pub commits: Vec<WireCommit>,
    pub vblanks: Vec<VblankRec>,
    /// What the leg could not mirror, as it ran.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// (eval-r3) The ticks in which the manager gave up waiting for a
    /// frame callback or presentation and painted anyway
    /// (`Stats::throttle_given_up`). Its give-up runs on real time
    /// (`THROTTLE_GIVE_UP`, 1 s, `Instant::now`), not the leg's clock, so
    /// it fires only when the run itself is that slow (a loaded
    /// machine): `late-present` does not judge the frames of such a tick
    /// or the next.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gave_up: Vec<u32>,
    /// (integration-r7) The margins each layer surface's state asks for
    /// at the end of each tick, after the tick's paints changed it (a new
    /// shadow's overhang): its state at rest then, by the driver, moved
    /// by the offset of the pose its latest frame commits
    /// ([`posed_margins`]). A bare commit sends the state of its moment,
    /// not of the frame before it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub held: Vec<HeldMargins>,
}

/// (integration-r7) [`WireLog::held`]: surface `surface`'s margins (top,
/// right, bottom, left) as its state asks for them at the end of tick
/// `k`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HeldMargins {
    pub k: u32,
    pub surface: u32,
    pub margin: [f32; 4],
    /// (integration-r7 part 2) The edges layer shell reads a margin on
    /// ([`read_edges`]); `None`: all four.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edges: Option<[bool; 4]>,
}

/// One painted frame of one surface, as the renderer's painter left it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WireFrame {
    /// The tick it was painted at (and is due to be shown at).
    pub k: u32,
    /// The surface (the trace's id).
    pub surface: u32,
    pub path: Option<Path>,
    /// Its buffer size, physical pixels.
    pub size: [u32; 2],
    /// It drew something: a present is owed for it.
    pub due: bool,
    /// Where it takes input, surface-local logical pixels rounded out
    /// (as the manager sends it), normalised; `None`: the painter asks
    /// for none (the manager's own region).
    pub input: Option<Vec<R>>,
    /// Where it is opaque, logical pixels rounded in, normalised.
    pub opaque: Vec<R>,
    /// Where it asks for a blur behind it
    /// (`strand_surface::blur::region_rects`), normalised.
    pub blur: Vec<R>,
    /// Its surface's compositor pose scales it: the regions are sent in
    /// the pose's coordinates and are not compared.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub posed: bool,
    /// (eval-rules-6) A layer surface's margins (top, right, bottom,
    /// left) for this frame, worked out here: the ones its state asks for
    /// at rest, the side it is anchored to on an axis anchored on one
    /// side moved by its pose's offset ([`posed_margins`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub margin: Option<[f32; 4]>,
    /// (integration-r7 part 2) The edges (top, right, bottom, left) layer
    /// shell reads its margins on ([`read_edges`]); `None`: all four.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edges: Option<[bool; 4]>,
    /// The commit that carried it (an index into `commits`).
    pub commit: Option<usize>,
}

/// One `wl_surface.commit` the compositor took.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WireCommit {
    /// The tick it was made in.
    pub k: u32,
    /// The surface (the trace's id), once known.
    pub surface: Option<u32>,
    /// Who attached its buffer (none: a bare commit).
    pub by: Option<Path>,
    /// The tick of the frame its buffer carries.
    pub frame: Option<u32>,
    /// The buffer it attached, physical pixels.
    pub buffer: Option<[u32; 2]>,
    /// Buffer scale and viewport destination after it.
    pub scale: i32,
    pub viewport: Option<[i32; 2]>,
    /// Its `damage_buffer` rectangles.
    pub damage: Vec<R>,
    /// The regions the compositor holds after it, normalised: opaque
    /// (none: empty), input (`None`: the whole surface), blur.
    pub opaque: Vec<R>,
    pub input: Option<Vec<R>>,
    pub blur: Vec<R>,
    pub alpha: Option<u32>,
    /// (eval-rules-6) A layer surface's margins after it (top, right,
    /// bottom, left).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub margin: Option<[i32; 4]>,
    /// Frame callbacks it asked for, and presentation feedback.
    pub callbacks: u32,
    pub feedback: bool,
    /// The refresh (its tick) that showed it; `None`: none came.
    pub shown: Option<u32>,
    /// Its buffer was replaced before that refresh: never shown.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub superseded: bool,
    /// When the manager predicted the frame it painted for this commit
    /// would be shown (its `PaintTarget::time`), ns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predicted_ns: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VblankRec {
    pub k: u32,
    pub t_ns: u64,
}

/// A frame as the harness's renderer left it, handed to the leg.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub k: u32,
    /// The trace's surface id.
    pub surface: u32,
    pub node: NodeId,
    pub gpu: bool,
    pub size: Size,
    pub scale: Scale,
    pub damage: Damage,
    /// `Painter::wants_frame` right after the paint.
    pub wants: bool,
    pub input: Option<Vec<LogicalRect>>,
    pub opaque: Damage,
    pub blur: Vec<BlurRegion>,
    pub pose: Option<SurfacePose>,
    /// (eval-rules-6) A layer surface's state at rest (the driver's).
    pub layer: Option<strand_surface::placement::LayerConfig>,
}

/// What made a buffer commit, in the order they were made.
#[derive(Clone, Copy, Debug)]
struct Event {
    surface: u32,
    k: u32,
    by: Path,
    size: Size,
    predicted: Option<Duration>,
}

#[derive(Debug)]
struct Held {
    snap: Snapshot,
    served: bool,
}

/// The manager's painter and host: the harness's frames, as the renderer
/// answered right after painting them.
#[derive(Debug, Default)]
pub struct WireHost {
    /// The latest frame of each node.
    frames: HashMap<NodeId, Held>,
    nodes: HashMap<SurfaceId, NodeId>,
    /// Every paint that drew (so a buffer commit follows), and every
    /// stand-in present, in order.
    events: Vec<Event>,
    /// The surface whose paint just returned: the manager's next
    /// `wants_frame` is its read right after the paint.
    just_painted: Cell<Option<SurfaceId>>,
    /// Surfaces the manager must destroy while handed off.
    gpu_released: Vec<SurfaceId>,
    /// Surfaces the manager has committed a buffer of. One it has not is
    /// not mapped for it (no popup opens on it), so its first frame is
    /// the manager's whatever draws it: in the shell a surface is
    /// promoted to the GPU only after frames of its own on the CPU
    /// (`promote.rs`).
    mapped: HashSet<SurfaceId>,
}

impl WireHost {
    fn held(&self, surface: SurfaceId) -> Option<&Held> {
        self.frames.get(self.nodes.get(&surface)?)
    }

    /// The manager paints `h`'s frame of `surface`: a CPU frame, or the
    /// first frame of a surface it never committed.
    fn paints(&self, surface: SurfaceId, h: &Held) -> bool {
        !h.snap.gpu || !self.mapped.contains(&surface)
    }
}

impl Painter for WireHost {
    fn paint(&mut self, surface: SurfaceId, target: &mut PaintTarget<'_>) -> Damage {
        let Some(node) = self.nodes.get(&surface).copied() else {
            return Damage::new();
        };
        let Some(h) = self.frames.get_mut(&node) else {
            return Damage::new();
        };
        if h.served || (h.snap.gpu && self.mapped.contains(&surface)) {
            return Damage::new();
        }
        h.served = true;
        self.just_painted.set(Some(surface));
        // A buffer of unknown contents is drawn whole, as the renderer
        // draws one; else the frame's own damage.
        let damage = if target.age == 0 {
            Damage::full(target.size)
        } else {
            h.snap.damage
        };
        if !damage.is_empty() {
            self.mapped.insert(surface);
            self.events.push(Event {
                surface: h.snap.surface,
                k: h.snap.k,
                by: Path::Shm,
                size: target.size,
                predicted: Some(target.time),
            });
        }
        damage
    }

    fn wants_frame(&self, surface: SurfaceId) -> bool {
        if self.just_painted.get() == Some(surface) {
            self.just_painted.set(None);
            return self.held(surface).is_some_and(|h| h.snap.wants);
        }
        // A frame waiting to be painted; none else until the harness
        // paints the next.
        self.held(surface)
            .is_some_and(|h| !h.served && self.paints(surface, h))
    }

    fn opaque_region(&self, surface: SurfaceId) -> Damage {
        self.held(surface).map_or(Damage::new(), |h| h.snap.opaque)
    }

    fn blur_region(&self, surface: SurfaceId) -> Vec<BlurRegion> {
        self.held(surface)
            .map_or(Vec::new(), |h| h.snap.blur.clone())
    }

    fn surface_pose(&self, surface: SurfaceId) -> Option<SurfacePose> {
        self.held(surface).and_then(|h| h.snap.pose)
    }

    fn input_region(&self, surface: SurfaceId) -> Option<Vec<LogicalRect>> {
        self.held(surface).and_then(|h| h.snap.input.clone())
    }
}

impl SurfaceHost for WireHost {
    fn surface_attached(
        &mut self,
        surface: SurfaceId,
        node: NodeId,
        _: Option<&strand_surface::Monitor>,
    ) {
        self.nodes.insert(surface, node);
    }

    fn surface_detached(&mut self, surface: SurfaceId) {
        self.nodes.remove(&surface);
        self.mapped.remove(&surface);
    }

    fn frame_dropped(&mut self, surface: SurfaceId) {
        // Painted, but its buffer could not be committed.
        let Some(id) = self.held(surface).map(|h| h.snap.surface) else {
            return;
        };
        if let Some(i) = self
            .events
            .iter()
            .rposition(|e| e.by == Path::Shm && e.surface == id)
        {
            self.events.remove(i);
        }
    }

    fn gpu_release(&mut self, surface: SurfaceId) {
        self.gpu_released.push(surface);
    }
}

/// The leg's own Wayland objects: a barrier queue and the stand-in WSI's
/// buffers.
#[derive(Debug, Default)]
struct Probe {
    synced: u64,
}

struct SyncTag;
struct FrameTag;

impl Dispatch<WlRegistry, GlobalListContents> for Probe {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wayland_client::protocol::wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlCallback, SyncTag> for Probe {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        _: wayland_client::protocol::wl_callback::Event,
        _: &SyncTag,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.synced += 1;
    }
}

impl Dispatch<WlCallback, FrameTag> for Probe {
    fn event(
        _: &mut Self,
        _: &WlCallback,
        _: wayland_client::protocol::wl_callback::Event,
        _: &FrameTag,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

wayland_client::delegate_noop!(Probe: ignore WlShm);
wayland_client::delegate_noop!(Probe: ignore WlShmPool);
wayland_client::delegate_noop!(Probe: ignore WlBuffer);

/// A handed-off surface, as the stand-in WSI holds it.
struct Wsi {
    surface: WlSurface,
    /// Its buffers by size, kept until the leg ends (a WSI's swapchain
    /// images; the fake reads none of them).
    buffers: HashMap<(u32, u32), WlBuffer>,
}

/// The surface leg of one run.
pub struct Leg {
    // Dropped in this order: the manager and the leg's proxies before
    // the connection, the compositor last.
    mgr: SurfaceManager<WireHost>,
    clock: FakeClock,
    probe: EventQueue<Probe>,
    probe_state: Probe,
    qh: QueueHandle<Probe>,
    shm: WlShm,
    /// The shm files behind the stand-in's buffers.
    files: Vec<std::fs::File>,
    wsi: BTreeMap<SurfaceId, Wsi>,
    /// The surface changes of the tick reported before its first paint,
    /// and after it, and its frames, applied at its end in that order.
    changes: Vec<(NodeId, SurfaceChange)>,
    post: Vec<(NodeId, SurfaceChange)>,
    snaps: Vec<Snapshot>,
    /// The frames, with the regions expected on the wire.
    frames: Vec<WireFrame>,
    /// The commits made by the end of each tick (`fake.commit_count()`).
    ticks: Vec<(u32, usize)>,
    /// The manager's give-ups so far, and the ticks they came in.
    given_up: u64,
    gave_up: Vec<u32>,
    /// (integration-r7) Each surface's latest frame's pose offset, and
    /// the margins its state asks for at each tick's end.
    offsets: HashMap<u32, [f32; 2]>,
    held: Vec<HeldMargins>,
    vblanks: Vec<VblankRec>,
    notes: Vec<String>,
    /// `late-present`'s own tests: refreshes that never come.
    lost: BTreeSet<u32>,
    conn: Connection,
    fake: Fake,
}

impl std::fmt::Debug for Leg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Leg")
            .field("frames", &self.frames.len())
            .finish_non_exhaustive()
    }
}

fn io(what: &str) -> impl Fn(std::io::Error) -> Error + '_ {
    move |e| Error::Io(format!("the surface leg's {what}"), e)
}

fn wl<E: std::fmt::Display>(what: &str) -> impl Fn(E) -> Error + '_ {
    move |e| Error::Run(format!("the surface leg: {what}: {e}"))
}

impl Leg {
    /// Starts the fake compositor (one output of `screen`'s size, with
    /// `wp_presentation`, in vblank mode) and a surface manager on it.
    /// Refreshes at the ticks in `lost` never come.
    pub fn start(screen: Screen, lost: BTreeSet<u32>) -> Result<Leg, Error> {
        if screen.scale != 1.0 {
            return Err(Error::Unsupported(format!(
                "the surface leg at scale {} (the fake compositor's output has scale 1)",
                screen.scale
            )));
        }
        let fake = Fake::builder()
            .toplevel_list(false)
            .outputs(&[OUTPUT])
            .surfaces(SurfaceGlobals {
                output_size: (screen.width, screen.height),
                presentation: true,
                vblank: true,
                ..SurfaceGlobals::default()
            })
            .try_start()
            .map_err(io("fake compositor"))?;
        let stream = fake.try_connect().map_err(io("connection"))?;
        let conn = Connection::from_socket(stream).map_err(wl("connect"))?;
        // The leg's own queue, bound before the manager reads the socket.
        let (globals, probe) = registry_queue_init::<Probe>(&conn).map_err(wl("registry"))?;
        let qh = probe.handle();
        let shm: WlShm = globals.bind(&qh, 1..=1, ()).map_err(wl("wl_shm"))?;
        let clock = FakeClock::new(crate::clock::T0);
        let mgr = SurfaceManager::with_connection(
            conn.clone(),
            WireHost::default(),
            Config {
                clock: Box::new(clock.clone()),
                ..Config::default()
            },
        )
        .map_err(wl("surface manager"))?;
        let mut leg = Leg {
            fake,
            conn,
            mgr,
            clock,
            probe,
            probe_state: Probe::default(),
            qh,
            shm,
            files: Vec::new(),
            wsi: BTreeMap::new(),
            changes: Vec::new(),
            post: Vec::new(),
            snaps: Vec::new(),
            frames: Vec::new(),
            ticks: Vec::new(),
            given_up: 0,
            gave_up: Vec::new(),
            offsets: HashMap::new(),
            held: Vec::new(),
            vblanks: Vec::new(),
            notes: Vec::new(),
            lost,
        };
        leg.settle()?;
        Ok(leg)
    }

    /// Surface changes the renderer reported, for the manager at the end
    /// of the tick.
    pub fn surface_changes(&mut self, changes: impl IntoIterator<Item = (NodeId, SurfaceChange)>) {
        // The sandbox's one screen is the fake's one output: a surface
        // asked for on it (by its `screens.all` id) is asked for on the
        // output by its connector name, which the manager matches too.
        let changes = changes.into_iter().map(|(node, mut change)| {
            if let SurfaceChange::Created(spec) | SurfaceChange::Updated { spec, .. } = &mut change
                && let Screens::Named(names) = &mut spec.screens
            {
                for n in names.iter_mut().filter(|n| n.as_str() == SCREEN_ID) {
                    *n = OUTPUT.to_string();
                }
            }
            (node, change)
        });
        if self.snaps.is_empty() {
            self.changes.extend(changes);
        } else {
            self.post.extend(changes);
        }
    }

    /// A frame the harness painted this tick.
    pub fn frame(&mut self, snap: Snapshot) {
        let posed = !snap.gpu && snap.pose.is_some_and(|p| p.scale != 1.0);
        let input = snap.input.as_ref().map(|v| {
            normal(
                v.iter()
                    .filter(|r| r.w > 0.0 && r.h > 0.0 && r.x.is_finite() && r.y.is_finite())
                    .map(|r| {
                        let (x0, y0) = (r.x.floor(), r.y.floor());
                        let (x1, y1) = ((r.x + r.w).ceil(), (r.y + r.h).ceil());
                        let c = |v: f32| v.clamp(-1e6, 1e6) as i32;
                        (true, [c(x0), c(y0), c(x1) - c(x0), c(y1) - c(y0)])
                    }),
            )
        });
        let opaque = normal(
            snap.scale
                .inner_logical_region(&snap.opaque)
                .into_iter()
                .map(|r| (true, [r.x, r.y, r.w as i32, r.h as i32])),
        );
        let blur = normal(
            strand_surface::blur::region_rects(&snap.blur, snap.scale)
                .into_iter()
                .map(|(x, y, w, h)| (true, [x, y, w, h])),
        );
        self.frames.push(WireFrame {
            k: snap.k,
            surface: snap.surface,
            path: Some(if snap.gpu { Path::Gpu } else { Path::Shm }),
            size: [snap.size.w, snap.size.h],
            due: !snap.damage.is_empty(),
            input,
            opaque,
            blur,
            posed,
            margin: snap.layer.as_ref().map(|c| {
                let o = snap.pose.map_or([0.0, 0.0], |p| [p.offset.x, p.offset.y]);
                posed_margins(c, o)
            }),
            edges: snap.layer.as_ref().map(read_edges),
            commit: None,
        });
        self.offsets.insert(
            snap.surface,
            snap.pose.map_or([0.0, 0.0], |p| [p.offset.x, p.offset.y]),
        );
        self.snaps.push(snap);
    }

    /// (integration-r7) The state at rest each layer surface asks for at
    /// the end of tick `k` (the driver's, after the tick's paints changed
    /// it), noted with the offset of the pose its latest frame commits
    /// ([`WireLog::held`]).
    pub fn layers(
        &mut self,
        k: u32,
        layers: impl IntoIterator<Item = (u32, strand_surface::placement::LayerConfig)>,
    ) {
        for (surface, c) in layers {
            let o = self.offsets.get(&surface).copied().unwrap_or([0.0, 0.0]);
            self.held.push(HeldMargins {
                k,
                surface,
                margin: posed_margins(&c, o),
                edges: Some(read_edges(&c)),
            });
        }
    }

    /// The end of tick `k` (shown at `t`, refreshing every `period`): the
    /// tick's surface changes and frames to the manager, the GPU's frames
    /// presented, then the refresh.
    pub fn end_tick(&mut self, k: u32, t: Duration, period: Duration) -> Result<(), Error> {
        // Painting now aims at the refresh at `t`.
        self.clock.set(t.saturating_sub(Duration::from_nanos(1)));
        let snaps = std::mem::take(&mut self.snaps);
        {
            let host = self.mgr.state_mut().host_mut();
            for s in &snaps {
                host.frames.insert(
                    s.node,
                    Held {
                        snap: s.clone(),
                        served: false,
                    },
                );
            }
        }
        for (node, change) in std::mem::take(&mut self.changes) {
            self.mgr.state_mut().apply_surface_change(node, change);
        }
        self.settle()?;
        for s in &snaps {
            let Some(id) = self.mgr.state().surfaces_of(s.node).first().copied() else {
                self.notes.push(format!(
                    "k{k}: surface {} has no Wayland surface yet: its frame is not mirrored",
                    s.surface
                ));
                continue;
            };
            let host = self.mgr.state().host();
            let mapped = host.mapped.contains(&id);
            let served = host.frames.get(&s.node).is_some_and(|h| h.served);
            if served {
                // The manager painted it already (a new surface's first).
            } else if s.gpu && (mapped || self.wsi.contains_key(&id)) {
                if !s.damage.is_empty() {
                    self.present(id, s)?;
                }
            } else if self.wsi.remove(&id).is_some() {
                // Back on the CPU: the GPU thread's `Released`.
                self.mgr.state_mut().take_back(id);
            } else {
                self.mgr.state_mut().repaint(id);
            }
        }
        self.settle()?;
        // What the paints left (a closing surface's removal, a new size).
        for (node, change) in std::mem::take(&mut self.post) {
            self.mgr.state_mut().apply_surface_change(node, change);
        }
        self.settle()?;
        let released = std::mem::take(&mut self.mgr.state_mut().host_mut().gpu_released);
        for id in released {
            self.wsi.remove(&id);
            self.mgr.state_mut().take_back(id);
        }
        self.settle()?;
        if !self.lost.contains(&k) {
            self.fake.cmd(strand_fake_wayland::Cmd::Vblank {
                time: t,
                refresh: period,
                seq: u64::from(k),
            });
            self.fake.sync();
            self.vblanks.push(VblankRec {
                k,
                t_ns: t.as_nanos() as u64,
            });
            self.clock.set(t);
            self.settle()?;
        }
        self.ticks.push((k, self.fake.commit_count()));
        let given_up = self.mgr.state().stats().throttle_given_up;
        if given_up > self.given_up {
            self.given_up = given_up;
            self.gave_up.push(k);
            self.notes.push(format!(
                "k{k}: the surface manager gave up waiting for a refresh after a second of real \
                 time (the run was that slow): late-present does not judge this tick's frames \
                 or the next's"
            ));
        }
        Ok(())
    }

    /// Frame `s` of handed-off surface `id`, as the shell's host sends a
    /// GPU frame: handed off the first time, its regions synced, then
    /// presented by the stand-in WSI.
    fn present(&mut self, id: SurfaceId, s: &Snapshot) -> Result<(), Error> {
        if !self.wsi.contains_key(&id) {
            let Some(handles) = self.mgr.state_mut().raw_handles(id) else {
                self.notes.push(format!(
                    "k{}: surface {} is not configured: its GPU frame is not mirrored",
                    s.k, s.surface
                ));
                return Ok(());
            };
            let raw_window_handle::RawWindowHandle::Wayland(w) = handles.window else {
                return Err(Error::Run(
                    "the surface leg: a surface's handle is not a Wayland one".into(),
                ));
            };
            // SAFETY: the handle names the manager's live `wl_surface` on
            // `self.conn`, lent (kept alive) until `take_back`, before
            // which the leg drops its proxy.
            let obj = unsafe {
                wayland_client::backend::ObjectId::from_ptr(
                    WlSurface::interface(),
                    w.surface.as_ptr().cast(),
                )
            }
            .map_err(wl("the lent wl_surface"))?;
            let surface = WlSurface::from_id(&self.conn, obj).map_err(wl("the lent wl_surface"))?;
            if !self.mgr.state_mut().hand_off(id) {
                return Err(Error::Run(format!(
                    "the surface leg: surface {} could not be handed off",
                    s.surface
                )));
            }
            self.wsi.insert(
                id,
                Wsi {
                    surface,
                    buffers: HashMap::new(),
                },
            );
        }
        // What follows a GPU frame in the shell's host: its regions as
        // pending state the present commits (decisions.md gpu-regions).
        self.mgr.state_mut().sync_presented(id);
        let size = self
            .mgr
            .state()
            .surface(id)
            .map_or(s.size, |i| i.buffer_size);
        let key = (size.w.max(1), size.h.max(1));
        if !self
            .wsi
            .get(&id)
            .is_some_and(|w| w.buffers.contains_key(&key))
        {
            let buffer = self.buffer(key)?;
            if let Some(w) = self.wsi.get_mut(&id) {
                w.buffers.insert(key, buffer);
            }
        }
        let Some(w) = self.wsi.get(&id) else {
            return Ok(());
        };
        let Some(buffer) = w.buffers.get(&key) else {
            return Ok(());
        };
        self.mgr.state_mut().host_mut().events.push(Event {
            surface: s.surface,
            k: s.k,
            by: Path::Gpu,
            size: Size::new(key.0, key.1),
            predicted: None,
        });
        if let Some(h) = self.mgr.state_mut().host_mut().frames.get_mut(&s.node) {
            h.served = true;
        }
        w.surface.attach(Some(buffer), 0, 0);
        w.surface.damage_buffer(0, 0, key.0 as i32, key.1 as i32);
        w.surface.frame(&self.qh, FrameTag);
        w.surface.commit();
        self.conn.flush().map_err(wl("flush"))?;
        Ok(())
    }

    /// A stand-in swapchain image of `(w, h)` pixels.
    fn buffer(&mut self, (w, h): (u32, u32)) -> Result<WlBuffer, Error> {
        use std::os::fd::AsFd;
        let len = u64::from(w) * u64::from(h) * 4;
        let file = tempfile::tempfile().map_err(io("shm file"))?;
        file.set_len(len).map_err(io("shm file"))?;
        let size = i32::try_from(len)
            .map_err(|_| Error::Run(format!("the surface leg: a {w}x{h} buffer is too large")))?;
        let pool = self.shm.create_pool(file.as_fd(), size, &self.qh, ());
        let buffer = pool.create_buffer(
            0,
            w as i32,
            h as i32,
            (w * 4) as i32,
            wl_shm::Format::Argb8888,
            &self.qh,
            (),
        );
        pool.destroy();
        self.files.push(file);
        Ok(buffer)
    }

    /// Lets the manager and the fake answer each other until nothing more
    /// happens: barriers (a `wl_display.sync` the manager's loop reads)
    /// until a round changes nothing.
    fn settle(&mut self) -> Result<(), Error> {
        let mut last = None;
        for _ in 0..ROUNDS {
            self.barrier()?;
            let now = (
                self.fake.commit_count(),
                self.mgr.state().host().events.len(),
                self.mgr.state().stats(),
            );
            if last.as_ref() == Some(&now) {
                return Ok(());
            }
            last = Some(now);
        }
        Err(Error::Run(format!(
            "the surface leg: the manager and the compositor did not settle in {ROUNDS} rounds"
        )))
    }

    fn barrier(&mut self) -> Result<(), Error> {
        let target = self.probe_state.synced + 1;
        self.conn.display().sync(&self.qh, SyncTag);
        self.conn.flush().map_err(wl("flush"))?;
        let deadline = Instant::now() + WAIT;
        loop {
            self.mgr
                .dispatch(Some(Duration::from_millis(1)))
                .map_err(wl("dispatch"))?;
            self.probe
                .dispatch_pending(&mut self.probe_state)
                .map_err(wl("dispatch"))?;
            self.conn.flush().map_err(wl("flush"))?;
            if self.probe_state.synced >= target {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(Error::Run(format!(
                    "the surface leg: the compositor did not answer within {WAIT:?}"
                )));
            }
        }
    }

    /// The log: each commit the compositor took, its buffer matched with
    /// the paint or present that made it (both are made in order on one
    /// connection, so the n-th buffer commit is the n-th of them), and
    /// each frame with the commit that carried it.
    pub fn finish(mut self) -> Result<WireLog, Error> {
        self.settle()?;
        // The stand-in's proxies go before the manager's surfaces.
        self.wsi.clear();
        let raw = self.fake.commits();
        let events = std::mem::take(&mut self.mgr.state_mut().host_mut().events);
        let mut log = WireLog {
            frames: std::mem::take(&mut self.frames),
            commits: Vec::with_capacity(raw.len()),
            vblanks: std::mem::take(&mut self.vblanks),
            notes: std::mem::take(&mut self.notes),
            gave_up: std::mem::take(&mut self.gave_up),
            held: std::mem::take(&mut self.held),
        };
        let mut next = events.iter();
        let mut owner: HashMap<usize, u32> = HashMap::new();
        let mut tick = self.ticks.iter().peekable();
        for (i, c) in raw.iter().enumerate() {
            while tick.next_if(|(_, n)| *n <= i).is_some() {}
            let k = tick.peek().map_or(0, |(k, _)| *k);
            let mut w = commit(c, k);
            if let Some(Some(BufferKind::Shm { width, height })) = c.buffer {
                let Some(e) = next.next() else {
                    return Err(Error::Run(format!(
                        "the surface leg: commit {i} attached a buffer no paint or present made"
                    )));
                };
                let size = [width.max(0) as u32, height.max(0) as u32];
                if size != [e.size.w, e.size.h] {
                    return Err(Error::Run(format!(
                        "the surface leg: commit {i}'s {}x{} buffer is not the {}x{} its paint drew",
                        size[0], size[1], e.size.w, e.size.h
                    )));
                }
                if let Some(o) = owner.insert(c.surface, e.surface)
                    && o != e.surface
                {
                    return Err(Error::Run(format!(
                        "the surface leg: Wayland surface {} carried frames of surfaces {o} and {}",
                        c.surface, e.surface
                    )));
                }
                w.by = Some(e.by);
                w.frame = Some(e.k);
                w.predicted_ns = e.predicted.map(|t| t.as_nanos() as u64);
            }
            log.commits.push(w);
        }
        if next.next().is_some() {
            return Err(Error::Run(
                "the surface leg: a paint or present made no buffer commit".into(),
            ));
        }
        for (c, raw) in log.commits.iter_mut().zip(&raw) {
            c.surface = owner.get(&raw.surface).copied();
        }
        for f in &mut log.frames {
            f.commit = log
                .commits
                .iter()
                .position(|c| c.surface == Some(f.surface) && c.frame == Some(f.k));
            // A GPU frame the manager painted (its surface's first).
            if let Some(c) = f.commit.and_then(|i| log.commits.get(i)) {
                f.path = c.by;
            }
        }
        Ok(log)
    }
}

fn commit(c: &CommitRecord, k: u32) -> WireCommit {
    let ops = |r: &Region| {
        r.0.iter()
            .map(|o| (o.add, [o.x, o.y, o.w, o.h]))
            .collect::<Vec<_>>()
    };
    WireCommit {
        k,
        surface: None,
        by: None,
        frame: None,
        buffer: match c.buffer {
            Some(Some(BufferKind::Shm { width, height })) => {
                Some([width.max(0) as u32, height.max(0) as u32])
            }
            _ => None,
        },
        scale: c.buffer_scale,
        viewport: c.viewport.map(|(w, h)| [w, h]),
        damage: c.damage.iter().map(|&(x, y, w, h)| [x, y, w, h]).collect(),
        opaque: c.opaque.as_ref().map_or(Vec::new(), |r| normal(ops(r))),
        input: c.input.as_ref().map(|r| normal(ops(r))),
        blur: c.blur.as_ref().map_or(Vec::new(), |r| normal(ops(r))),
        alpha: c.alpha,
        margin: c.margin,
        callbacks: c.frames as u32,
        feedback: c.feedback,
        shown: c.vblank.map(|v| v as u32),
        superseded: c.superseded,
        predicted_ns: None,
    }
}

/// (eval-rules-6) The margins (top, right, bottom, left) that put a
/// layer surface of state `c` `offset` logical px (output coordinates,
/// down and right positive) from where it lies at rest, as layer shell
/// reads them: a margin is the distance from the edge a surface is
/// anchored to, so on an axis anchored on one side that side's margin
/// grows by the offset toward the far edge (top, left) or shrinks by it
/// (bottom, right); an axis anchored on both sides or neither is placed
/// by the compositor whatever its margins, and keeps them. Worked out
/// here from the protocol, never by strand-surface's `posed_margin`,
/// which it judges; unrounded (the wire's margins are whole pixels).
pub fn posed_margins(c: &strand_surface::placement::LayerConfig, offset: [f32; 2]) -> [f32; 4] {
    let a = c.anchors;
    let [t, r, b, l] = c.margin.map(|m| m as f32);
    let [dx, dy] = offset.map(|v| if v.is_finite() { v } else { 0.0 });
    let (l, r) = match (a.left, a.right) {
        (true, false) => (l + dx, r),
        (false, true) => (l, r - dx),
        _ => (l, r),
    };
    let (t, b) = match (a.top, a.bottom) {
        (true, false) => (t + dy, b),
        (false, true) => (t, b - dy),
        _ => (t, b),
    };
    [t, r, b, l]
}

/// (integration-r7 part 2) The edges (top, right, bottom, left) of a
/// layer surface of state `c` whose margins layer shell reads: the edges
/// it is anchored to (wlr-layer-shell `set_margin`: "Setting this value
/// for edges you are not anchored to has no effect"), both edges of an
/// axis anchored at both ends. A margin on any other edge moves nothing
/// and enters no exclusive zone, so it is not compared.
pub fn read_edges(c: &strand_surface::placement::LayerConfig) -> [bool; 4] {
    let a = c.anchors;
    [a.top, a.right, a.bottom, a.left]
}

/// A region (rectangles added, `true`, or subtracted, in order) as the
/// set of pixels it covers, in one normal form: bands top to bottom, each
/// a run of rows with the same spans, its spans left to right, adjacent
/// bands with equal spans merged. Two regions cover the same pixels when
/// their normal forms are equal.
pub fn normal(ops: impl IntoIterator<Item = (bool, R)>) -> Vec<R> {
    let ops: Vec<(bool, R)> = ops
        .into_iter()
        .filter(|(_, [_, _, w, h])| *w > 0 && *h > 0)
        .collect();
    let mut ys: Vec<i64> = ops
        .iter()
        .flat_map(|(_, [_, y, _, h])| [i64::from(*y), i64::from(*y) + i64::from(*h)])
        .collect();
    ys.sort_unstable();
    ys.dedup();
    // Runs of rows `[y0, y1)` with the same spans.
    type Band = (i64, i64, Vec<(i64, i64)>);
    let mut bands: Vec<Band> = Vec::new();
    for w in ys.windows(2) {
        let (y0, y1) = (w[0], w[1]);
        let mut spans: Vec<(i64, i64)> = Vec::new();
        for (add, [x, y, rw, rh]) in &ops {
            let (top, bot) = (i64::from(*y), i64::from(*y) + i64::from(*rh));
            if top > y0 || bot < y1 {
                continue;
            }
            let (a, b) = (i64::from(*x), i64::from(*x) + i64::from(*rw));
            spans = if *add {
                union(&spans, a, b)
            } else {
                minus(&spans, a, b)
            };
        }
        if spans.is_empty() {
            continue;
        }
        match bands.last_mut() {
            Some((_, end, s)) if *end == y0 && *s == spans => *end = y1,
            _ => bands.push((y0, y1, spans)),
        }
    }
    let mut out = Vec::new();
    for (y0, y1, spans) in bands {
        for (a, b) in spans {
            out.push([a as i32, y0 as i32, (b - a) as i32, (y1 - y0) as i32]);
        }
    }
    out
}

fn union(spans: &[(i64, i64)], a: i64, b: i64) -> Vec<(i64, i64)> {
    let mut all: Vec<(i64, i64)> = spans.to_vec();
    all.push((a, b));
    all.sort_unstable();
    let mut out: Vec<(i64, i64)> = Vec::new();
    for (s, e) in all {
        match out.last_mut() {
            Some((_, end)) if s <= *end => *end = (*end).max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

fn minus(spans: &[(i64, i64)], a: i64, b: i64) -> Vec<(i64, i64)> {
    let mut out = Vec::new();
    for &(s, e) in spans {
        if e <= a || s >= b {
            out.push((s, e));
            continue;
        }
        if s < a {
            out.push((s, a));
        }
        if e > b {
            out.push((b, e));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_that_cover_the_same_pixels_have_one_normal_form() {
        // Two halves side by side are the whole.
        let halves = normal([(true, [0, 0, 5, 10]), (true, [5, 0, 5, 10])]);
        assert_eq!(halves, normal([(true, [0, 0, 10, 10])]));
        assert_eq!(halves, [[0, 0, 10, 10]]);
        // Stacked, overlapping, in another order.
        let a = normal([(true, [0, 4, 10, 6]), (true, [0, 0, 10, 5])]);
        assert_eq!(a, [[0, 0, 10, 10]]);
        // A hole cut out of a box.
        let hole = normal([(true, [0, 0, 10, 10]), (false, [2, 2, 2, 2])]);
        assert_eq!(
            hole,
            [[0, 0, 10, 2], [0, 2, 2, 2], [4, 2, 6, 2], [0, 4, 10, 6]]
        );
        assert_ne!(hole, normal([(true, [0, 0, 10, 10])]));
        // Empty rectangles and an empty region.
        assert!(normal([(true, [3, 3, 0, 4])]).is_empty());
        assert!(normal([]).is_empty());
    }
}
