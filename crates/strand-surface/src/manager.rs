//! The Wayland side: connection, globals, layer surfaces per output, frame
//! scheduling and input, driven by a calloop event loop on the main
//! (render + surface) thread.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use calloop::channel::{self, Channel};
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, LoopHandle, RegistrationToken};
use smithay_client_toolkit::compositor::{
    CompositorHandler, CompositorState, FrameCallbackData, Region,
};
use smithay_client_toolkit::dispatch2::Dispatch2;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    self, KeyboardInteractivity, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_dispatch2, delegate_registry, registry_handlers};
use wayland_client::backend::ObjectId;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_buffer, wl_output, wl_pointer, wl_seat, wl_surface};
use wayland_client::{Connection, Proxy, QueueHandle};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::presentation_time::client::{
    wp_presentation::{self, WpPresentation},
    wp_presentation_feedback::{self, WpPresentationFeedback},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::{self, WpViewport},
    wp_viewporter::{self, WpViewporter},
};

use strand_scene::{
    Keyboard, Layer, LogicalPoint, LogicalSize, NodeId, PaintTarget, Painter, Rect, Scale, Screens,
    Size, SurfaceChange, SurfaceId, SurfaceSpec,
};

use crate::clock::{FrameClock, Presentation, PresentationClock};
use crate::input::{AxisDelta, AxisSource, ButtonState, InputEvent};
use crate::monitor::{Monitor, MonitorId, Monitors};
use crate::placement::{LayerConfig, PlacementError, layer_config};
use crate::shm::{BufferData, MAX_BUFFERS, ShmBuffers};

/// What the surface manager calls on the main thread: the [`Painter`]
/// (render) plus surface lifecycle notifications, which the binary forwards
/// to `Renderer::attach_surface`, `configure_surface` and `detach_surface`.
/// Every method but `paint` and `wants_frame` has a no-op default.
pub trait SurfaceHost: Painter {
    /// A Wayland surface now shows `node` on `monitor`.
    fn surface_attached(&mut self, surface: SurfaceId, node: NodeId, monitor: &Monitor) {
        let _ = (surface, node, monitor);
    }
    /// The surface's buffer size or scale changed; called before the first
    /// paint at that size.
    fn surface_configured(&mut self, surface: SurfaceId, size: Size, scale: Scale) {
        let _ = (surface, size, scale);
    }
    /// The surface is gone (node removed, output unplugged, or the
    /// compositor closed it).
    fn surface_detached(&mut self, surface: SurfaceId) {
        let _ = surface;
    }
    /// A monitor appeared. `reconnected` is true when the same monitor
    /// (make + model + description) was unplugged less than 30 s ago.
    fn monitor_added(&mut self, monitor: &Monitor, reconnected: bool) {
        let _ = (monitor, reconnected);
    }
    /// A monitor was unplugged; it is remembered for 30 s.
    fn monitor_removed(&mut self, monitor: &Monitor) {
        let _ = monitor;
    }
    /// An unplugged monitor did not come back within 30 s.
    fn monitor_forgotten(&mut self, monitor: &Monitor) {
        let _ = monitor;
    }
    /// When a surface whose last paint drew nothing while
    /// [`Painter::wants_frame`] stayed true should be asked again
    /// (`Renderer::frame_deadline`). `None` asks again on the next frame
    /// callback.
    fn frame_deadline(&self, surface: SurfaceId) -> Option<Instant> {
        let _ = surface;
        None
    }
    /// A paint returned damage but its buffer could not be committed; the
    /// painter must not count that frame (`Renderer::invalidate`).
    fn frame_dropped(&mut self, surface: SurfaceId) {
        let _ = surface;
    }
}

/// Settings for [`SurfaceManager`].
pub struct Config {
    /// Frame timing source; [`PresentationClock`] by default, a
    /// [`crate::FakeClock`] in tests.
    pub clock: Box<dyn FrameClock>,
    /// Use `wp_fractional_scale_v1` + `wp_viewporter` when the compositor
    /// has both. When false or unavailable, buffers use the integer
    /// `wl_surface` buffer scale.
    pub fractional_scale: bool,
    /// Buffers per surface, 2 or 3.
    pub max_buffers: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            clock: Box::new(PresentationClock::new()),
            fractional_scale: true,
            max_buffers: MAX_BUFFERS,
        }
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("fractional_scale", &self.fractional_scale)
            .field("max_buffers", &self.max_buffers)
            .finish_non_exhaustive()
    }
}

/// Why the surface manager could not start or run.
#[derive(Debug)]
pub enum SurfaceError {
    /// No Wayland compositor to connect to.
    Connect(wayland_client::ConnectError),
    /// The initial registry roundtrip failed.
    Registry(wayland_client::globals::GlobalError),
    /// A global the shell cannot work without is missing.
    MissingGlobal(&'static str),
    /// The event loop failed.
    EventLoop(calloop::Error),
}

impl std::fmt::Display for SurfaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect(e) => write!(f, "cannot connect to the Wayland compositor: {e}"),
            Self::Registry(e) => write!(f, "Wayland registry: {e}"),
            Self::MissingGlobal(g) => write!(f, "the compositor does not offer {g}"),
            Self::EventLoop(e) => write!(f, "event loop: {e}"),
        }
    }
}

impl std::error::Error for SurfaceError {}

impl From<calloop::Error> for SurfaceError {
    fn from(e: calloop::Error) -> Self {
        Self::EventLoop(e)
    }
}

/// A request sent from any thread through a [`RepaintHandle`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// Paint `surface` at the next opportunity.
    Repaint(SurfaceId),
    /// Paint every surface.
    RepaintAll,
    /// Ask every surface's [`Painter::wants_frame`] again without forcing
    /// a paint.
    Poll,
}

/// Wakes the surface manager from another thread ("repaint surface X").
/// Cheap to clone; sending never blocks.
#[derive(Clone, Debug)]
pub struct RepaintHandle {
    tx: channel::Sender<Request>,
}

impl RepaintHandle {
    /// Sends `request`. Returns false if the surface manager is gone.
    pub fn send(&self, request: Request) -> bool {
        self.tx.send(request).is_ok()
    }

    pub fn repaint(&self, surface: SurfaceId) -> bool {
        self.send(Request::Repaint(surface))
    }

    pub fn repaint_all(&self) -> bool {
        self.send(Request::RepaintAll)
    }
}

/// Counters, for tests and `strand report`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// `Painter::paint` calls.
    pub paints: u64,
    /// Paints that returned no damage (nothing committed).
    pub empty_paints: u64,
    /// `wl_surface.commit`s carrying a new buffer.
    pub commits: u64,
    /// Commits without a buffer (initial, reconfigure, frame-only).
    pub bare_commits: u64,
    /// Frame callbacks requested.
    pub frame_requests: u64,
    /// Frame callbacks completed.
    pub frames_done: u64,
    /// Layer-surface configures received.
    pub configures: u64,
    /// `wl_buffer.release`s received.
    pub releases: u64,
    /// Layer surfaces the compositor closed.
    pub closed: u64,
    /// `set_opaque_region` requests (sent only when the region changes).
    pub opaque_updates: u64,
    pub presented: u64,
    pub discarded: u64,
}

/// A snapshot of one surface.
#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceInfo {
    pub id: SurfaceId,
    pub node: NodeId,
    pub monitor: MonitorId,
    pub namespace: String,
    /// The compositor configured it.
    pub configured: bool,
    /// Size in logical pixels from the last configure.
    pub logical_size: (u32, u32),
    /// Buffer size in physical pixels.
    pub buffer_size: Size,
    pub scale: Scale,
    /// Using `wp_fractional_scale_v1` + viewporter (else integer buffer
    /// scale).
    pub fractional: bool,
    /// shm buffers currently allocated.
    pub buffers: usize,
    pub stats: Stats,
}

struct Surface {
    id: SurfaceId,
    node: NodeId,
    monitor: MonitorId,
    output: u32,
    layer: LayerSurface,
    config: LayerConfig,
    viewport: Option<WpViewport>,
    fractional: Option<WpFractionalScaleV1>,
    configured: bool,
    logical: (u32, u32),
    /// Scale buffers are painted at.
    scale: Scale,
    /// Scale last reported to the host with the buffer size.
    reported_scale: Option<Scale>,
    /// Integer scale the compositor prefers (fallback path).
    integer_scale: i32,
    buffers: ShmBuffers,
    /// Viewport destination / buffer scale must be (re)sent with the next
    /// buffer.
    geometry_dirty: bool,
    callback_pending: bool,
    /// A paint is owed: first configure, resize, rescale or a request.
    repaint: bool,
    /// Last opaque region sent, in logical pixels.
    opaque: Vec<Rect>,
    stats: Stats,
}

impl Surface {
    fn wl(&self) -> &wl_surface::WlSurface {
        self.layer.wl_surface()
    }

    fn is_fractional(&self) -> bool {
        self.viewport.is_some() && self.fractional.is_some()
    }

    /// The buffer size for the current logical size and scale.
    fn buffer_size(&self) -> Size {
        let (w, h) = self.logical;
        if self.is_fractional() {
            self.scale
                .physical_size(LogicalSize::new(w as f32, h as f32))
        } else {
            let n = self.integer_scale.max(1) as u32;
            Size::new(w.saturating_mul(n), h.saturating_mul(n))
        }
    }

    fn info(&self) -> SurfaceInfo {
        SurfaceInfo {
            id: self.id,
            node: self.node,
            monitor: self.monitor.clone(),
            namespace: self.config.namespace.clone(),
            configured: self.configured,
            logical_size: self.logical,
            buffer_size: self.buffers.size(),
            scale: self.scale,
            fractional: self.is_fractional(),
            buffers: self.buffers.slots.len(),
            stats: self.stats,
        }
    }
}

/// User data for globals this crate binds itself.
#[derive(Debug)]
pub struct StrandGlobal;

/// User data for per-surface protocol objects.
#[derive(Debug)]
pub struct SurfaceTag(SurfaceId);

/// User data for presentation feedback.
#[derive(Debug)]
pub struct FeedbackTag(SurfaceId);

/// The event loop's shared data: everything the surface manager owns plus
/// the host. Reached through [`SurfaceManager::state`] or in calloop
/// callbacks registered on [`SurfaceManager::loop_handle`].
pub struct State<H: SurfaceHost + 'static> {
    host: H,
    conn: Connection,
    qh: QueueHandle<Self>,
    handle: LoopHandle<'static, Self>,
    registry: RegistryState,
    compositor: CompositorState,
    output_state: OutputState,
    seat_state: SeatState,
    shm: Shm,
    layer_shell: LayerShell,
    viewporter: Option<WpViewporter>,
    fractional_manager: Option<WpFractionalScaleManagerV1>,
    presentation: Option<WpPresentation>,
    clock: Box<dyn FrameClock>,
    max_buffers: usize,
    monitors: Monitors,
    /// `wl_output` global name → proxy, for plugged-in outputs.
    outputs: BTreeMap<u32, wl_output::WlOutput>,
    output_globals: HashMap<ObjectId, u32>,
    specs: BTreeMap<NodeId, SurfaceSpec>,
    surfaces: BTreeMap<SurfaceId, Surface>,
    by_wl: HashMap<ObjectId, SurfaceId>,
    /// Stable ids per (node, monitor), kept while the monitor is
    /// remembered so a replugged monitor gets its surface id back.
    ids: HashMap<(NodeId, MonitorId), SurfaceId>,
    next_id: u32,
    dirty: BTreeSet<SurfaceId>,
    flush_scheduled: bool,
    pointers: Vec<(wl_seat::WlSeat, wl_pointer::WlPointer)>,
    focused: Option<MonitorId>,
    input: mpsc::Sender<InputEvent>,
    stats: Stats,
    expiry_timer: Option<RegistrationToken>,
    deadline_timers: HashMap<SurfaceId, RegistrationToken>,
}

impl<H: SurfaceHost + 'static> std::fmt::Debug for State<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State")
            .field("surfaces", &self.surfaces.keys().collect::<Vec<_>>())
            .field("specs", &self.specs.len())
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

/// Owns the Wayland connection and the event loop of the main thread.
///
/// Typical wiring (see `docs/architecture.md`, "Render loop"): build it
/// with the renderer as host, insert the logic → render diff channel and
/// the text worker's ping on [`SurfaceManager::loop_handle`], and in
/// those callbacks apply the diff, hand the renderer's surface changes to
/// [`State::apply_surface_change`] and call [`State::poll`]. Then call
/// [`SurfaceManager::dispatch`] in a loop; it blocks while idle.
pub struct SurfaceManager<H: SurfaceHost + 'static> {
    event_loop: EventLoop<'static, State<H>>,
    state: State<H>,
    input: Option<mpsc::Receiver<InputEvent>>,
    repaint: channel::Sender<Request>,
}

impl<H: SurfaceHost + 'static> std::fmt::Debug for SurfaceManager<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SurfaceManager")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl<H: SurfaceHost + 'static> SurfaceManager<H> {
    /// Connects to the compositor named by `WAYLAND_DISPLAY`.
    pub fn connect(host: H, config: Config) -> Result<Self, SurfaceError> {
        let conn = Connection::connect_to_env().map_err(SurfaceError::Connect)?;
        Self::with_connection(conn, host, config)
    }

    /// Uses an existing connection (tests connect to their own sway).
    pub fn with_connection(
        conn: Connection,
        host: H,
        config: Config,
    ) -> Result<Self, SurfaceError> {
        let (globals, queue) =
            registry_queue_init::<State<H>>(&conn).map_err(SurfaceError::Registry)?;
        let qh = queue.handle();
        let event_loop: EventLoop<'static, State<H>> = EventLoop::try_new()?;
        let handle = event_loop.handle();

        let compositor = CompositorState::bind(&globals, &qh)
            .map_err(|_| SurfaceError::MissingGlobal("wl_compositor"))?;
        let layer_shell = LayerShell::bind(&globals, &qh)
            .map_err(|_| SurfaceError::MissingGlobal("zwlr_layer_shell_v1"))?;
        let shm = Shm::bind(&globals, &qh).map_err(|_| SurfaceError::MissingGlobal("wl_shm"))?;
        let viewporter = globals
            .bind::<WpViewporter, _, _>(&qh, 1..=1, StrandGlobal)
            .ok();
        let fractional_manager = config
            .fractional_scale
            .then(|| {
                globals
                    .bind::<WpFractionalScaleManagerV1, _, _>(&qh, 1..=1, StrandGlobal)
                    .ok()
            })
            .flatten();
        let presentation = globals
            .bind::<WpPresentation, _, _>(&qh, 1..=1, StrandGlobal)
            .ok();

        WaylandSource::new(conn.clone(), queue)
            .insert(handle.clone())
            .map_err(|e| SurfaceError::EventLoop(e.error))?;
        let (repaint, requests): (channel::Sender<Request>, Channel<Request>) = channel::channel();
        handle
            .insert_source(requests, |event, _, state: &mut State<H>| {
                if let channel::Event::Msg(request) = event {
                    state.request(request);
                }
            })
            .map_err(|e| SurfaceError::EventLoop(e.error))?;

        let (input_tx, input_rx) = mpsc::channel();
        let state = State {
            host,
            conn,
            registry: RegistryState::new(&globals),
            output_state: OutputState::new(&globals, &qh),
            seat_state: SeatState::new(&globals, &qh),
            qh,
            handle,
            compositor,
            shm,
            layer_shell,
            viewporter,
            fractional_manager,
            presentation,
            clock: config.clock,
            max_buffers: config.max_buffers,
            monitors: Monitors::default(),
            outputs: BTreeMap::new(),
            output_globals: HashMap::new(),
            specs: BTreeMap::new(),
            surfaces: BTreeMap::new(),
            by_wl: HashMap::new(),
            ids: HashMap::new(),
            next_id: 1,
            dirty: BTreeSet::new(),
            flush_scheduled: false,
            pointers: Vec::new(),
            focused: None,
            input: input_tx,
            stats: Stats::default(),
            expiry_timer: None,
            deadline_timers: HashMap::new(),
        };
        Ok(Self {
            event_loop,
            state,
            input: Some(input_rx),
            repaint,
        })
    }

    /// Waits for events (at most `timeout`; `None` blocks until one
    /// arrives) and handles them, painting surfaces that need it. With
    /// nothing dirty and no timers armed this sleeps in `poll` until the
    /// compositor or another thread wakes it.
    pub fn dispatch(&mut self, timeout: Option<Duration>) -> Result<(), SurfaceError> {
        // Work scheduled from outside the loop (a `State` method called
        // between dispatches) runs as an idle callback; calloop only runs
        // idles after polling, so do not sleep first.
        let timeout = if self.state.flush_scheduled {
            Some(Duration::ZERO)
        } else {
            timeout
        };
        self.event_loop.dispatch(timeout, &mut self.state)?;
        Ok(())
    }

    /// Dispatches until `done` returns true or `timeout` passes. Returns
    /// whether `done` became true.
    pub fn dispatch_until(
        &mut self,
        timeout: Duration,
        mut done: impl FnMut(&State<H>) -> bool,
    ) -> Result<bool, SurfaceError> {
        let deadline = Instant::now() + timeout;
        loop {
            if done(&self.state) {
                return Ok(true);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            self.dispatch(Some(deadline - now))?;
        }
    }

    pub fn state(&self) -> &State<H> {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut State<H> {
        &mut self.state
    }

    /// For inserting more event sources (logic diffs, the text worker's
    /// ping) into the main loop.
    pub fn loop_handle(&self) -> LoopHandle<'static, State<H>> {
        self.event_loop.handle()
    }

    /// A handle other threads use to request repaints.
    pub fn repaint_handle(&self) -> RepaintHandle {
        RepaintHandle {
            tx: self.repaint.clone(),
        }
    }

    /// The receiving end of the input channel (once).
    pub fn take_input(&mut self) -> Option<mpsc::Receiver<InputEvent>> {
        self.input.take()
    }
}

impl<H: SurfaceHost + 'static> State<H> {
    pub fn host(&self) -> &H {
        &self.host
    }

    pub fn host_mut(&mut self) -> &mut H {
        &mut self.host
    }

    /// Totals over every surface, including destroyed ones.
    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn surfaces(&self) -> Vec<SurfaceInfo> {
        self.surfaces.values().map(Surface::info).collect()
    }

    pub fn surface(&self, id: SurfaceId) -> Option<SurfaceInfo> {
        self.surfaces.get(&id).map(Surface::info)
    }

    /// Plugged-in monitors.
    pub fn monitors(&self) -> Vec<Monitor> {
        let mut v: Vec<Monitor> = self.monitors.present().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// Unplugged monitors still remembered (less than 30 s ago).
    pub fn remembered_monitors(&self) -> Vec<Monitor> {
        let mut v: Vec<Monitor> = self.monitors.remembered().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// True when the compositor offers fractional scaling and viewporter
    /// and they are enabled.
    pub fn fractional_available(&self) -> bool {
        self.viewporter.is_some() && self.fractional_manager.is_some()
    }

    /// True when `wp_presentation` feedback drives the frame clock.
    pub fn presentation_available(&self) -> bool {
        self.presentation.is_some()
    }

    /// Handles a [`Request`] (also what the repaint channel delivers).
    pub fn request(&mut self, request: Request) {
        match request {
            Request::Repaint(id) => self.repaint(id),
            Request::RepaintAll => {
                let ids: Vec<SurfaceId> = self.surfaces.keys().copied().collect();
                for id in ids {
                    self.repaint(id);
                }
            }
            Request::Poll => self.poll(),
        }
    }

    /// Paints `surface` at the next opportunity (after the pending frame
    /// callback, if one is outstanding).
    pub fn repaint(&mut self, surface: SurfaceId) {
        if let Some(s) = self.surfaces.get_mut(&surface) {
            s.repaint = true;
            self.mark(surface);
        }
    }

    /// Asks every surface's [`Painter::wants_frame`] again: call after
    /// handing the painter new content (a scene diff, delivered text).
    pub fn poll(&mut self) {
        let ids: Vec<SurfaceId> = self.surfaces.keys().copied().collect();
        for id in ids {
            self.mark(id);
        }
    }

    /// Applies a change reported by `Renderer::take_surface_changes`.
    pub fn apply_surface_change(&mut self, node: NodeId, change: SurfaceChange) {
        match change {
            SurfaceChange::Created(spec) => {
                self.specs.insert(node, spec);
                self.reconcile(node);
            }
            SurfaceChange::Updated { spec, recreate } => {
                if recreate {
                    self.destroy_node_surfaces(node);
                }
                self.specs.insert(node, spec);
                self.reconfigure(node);
                self.reconcile(node);
            }
            SurfaceChange::Removed => {
                self.destroy_node_surfaces(node);
                self.specs.remove(&node);
                self.ids.retain(|(n, _), _| *n != node);
            }
        }
    }

    /// The surfaces showing `node`.
    pub fn surfaces_of(&self, node: NodeId) -> Vec<SurfaceId> {
        self.surfaces
            .values()
            .filter(|s| s.node == node)
            .map(|s| s.id)
            .collect()
    }

    fn mark(&mut self, id: SurfaceId) {
        self.dirty.insert(id);
        if !self.flush_scheduled {
            self.flush_scheduled = true;
            // Runs at the end of the current (or next) dispatch, so every
            // event of one wakeup coalesces into at most one paint per
            // surface.
            self.handle.insert_idle(|state| state.flush());
        }
    }

    fn flush(&mut self) {
        self.flush_scheduled = false;
        let dirty = std::mem::take(&mut self.dirty);
        for id in dirty {
            self.draw(id);
        }
        if let Err(e) = self.conn.flush() {
            log::warn!("flushing the Wayland connection failed: {e}");
        }
    }

    // ---- outputs and surfaces -------------------------------------------

    fn focused_monitor(&self) -> Option<MonitorId> {
        self.focused
            .clone()
            .filter(|id| self.monitors.present().any(|m| &m.id == id))
            .or_else(|| {
                self.outputs
                    .keys()
                    .find_map(|g| self.monitors.id_of(*g).cloned())
            })
    }

    fn wants(&self, spec: &SurfaceSpec, monitor: &Monitor, focused: Option<&MonitorId>) -> bool {
        if !spec.open || layer_config(spec).is_err() {
            return false;
        }
        match &spec.screens {
            Screens::All => true,
            Screens::Focused => focused == Some(&monitor.id),
            Screens::Named(names) => names.iter().any(|n| {
                n == monitor.id.as_str() || monitor.connector.as_deref() == Some(n.as_str())
            }),
        }
    }

    /// Creates and destroys `node`'s surfaces so there is exactly one on
    /// each monitor its spec asks for.
    fn reconcile(&mut self, node: NodeId) {
        let Some(spec) = self.specs.get(&node).cloned() else {
            return;
        };
        if let Err(e) = layer_config(&spec) {
            log::warn!("{}: {e}", spec.namespace());
        }
        let focused = self.focused_monitor();
        let wanted: Vec<(u32, Monitor)> = self
            .outputs
            .keys()
            .filter_map(|g| {
                let id = self.monitors.id_of(*g)?;
                let m = self.monitors.get(id)?;
                self.wants(&spec, m, focused.as_ref())
                    .then(|| (*g, m.clone()))
            })
            .collect();
        let stale: Vec<SurfaceId> = self
            .surfaces
            .values()
            .filter(|s| s.node == node && !wanted.iter().any(|(_, m)| m.id == s.monitor))
            .map(|s| s.id)
            .collect();
        for id in stale {
            self.destroy_surface(id);
        }
        for (global, monitor) in wanted {
            let exists = self
                .surfaces
                .values()
                .any(|s| s.node == node && s.monitor == monitor.id);
            if !exists {
                self.create_surface(node, &spec, global, &monitor);
            }
        }
    }

    /// Pushes a changed spec to `node`'s live surfaces in place.
    fn reconfigure(&mut self, node: NodeId) {
        let Some(spec) = self.specs.get(&node) else {
            return;
        };
        let new = layer_config(spec);
        let ids = self.surfaces_of(node);
        for id in ids {
            let Ok(config) = new.clone() else {
                self.destroy_surface(id);
                continue;
            };
            let Some(s) = self.surfaces.get_mut(&id) else {
                continue;
            };
            if s.config == config {
                continue;
            }
            if s.config.layer != config.layer || s.config.namespace != config.namespace {
                // Layer and namespace are fixed at creation.
                self.destroy_surface(id);
                continue;
            }
            apply_layer_config(&s.layer, &config);
            s.config = config;
            s.layer.commit();
            s.stats.bare_commits += 1;
            self.stats.bare_commits += 1;
        }
    }

    fn create_surface(&mut self, node: NodeId, spec: &SurfaceSpec, global: u32, monitor: &Monitor) {
        let config = match layer_config(spec) {
            Ok(c) => c,
            Err(PlacementError::AutoSize(_) | PlacementError::NotLayerSurface(_)) => return,
        };
        let Some(output) = self.outputs.get(&global).cloned() else {
            return;
        };
        let key = (node, monitor.id.clone());
        let id = match self.ids.get(&key) {
            Some(id) => *id,
            None => {
                let id = SurfaceId(self.next_id);
                self.next_id = self.next_id.wrapping_add(1).max(1);
                self.ids.insert(key, id);
                id
            }
        };
        let wl = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            wl.clone(),
            to_sctk_layer(config.layer),
            Some(config.namespace.clone()),
            Some(&output),
        );
        apply_layer_config(&layer, &config);
        let (viewport, fractional) = match (&self.viewporter, &self.fractional_manager) {
            (Some(vp), Some(fm)) => (
                Some(vp.get_viewport(&wl, &self.qh, SurfaceTag(id))),
                Some(fm.get_fractional_scale(&wl, &self.qh, SurfaceTag(id))),
            ),
            _ => (None, None),
        };
        let info = self.output_state.info(&output);
        let integer_scale = info.as_ref().map_or(1, |i| i.scale_factor.max(1));
        // Until the compositor says otherwise, guess the fractional scale
        // from the output's mode and logical size so the first frame is
        // already sharp.
        let scale = if fractional.is_some() {
            info.as_ref()
                .and_then(estimate_scale)
                .unwrap_or_else(|| Scale::from_integer(integer_scale as u32).unwrap_or(Scale::ONE))
        } else {
            Scale::from_integer(integer_scale as u32).unwrap_or(Scale::ONE)
        };
        layer.commit();
        self.by_wl.insert(wl.id(), id);
        let surface = Surface {
            id,
            node,
            monitor: monitor.id.clone(),
            output: global,
            layer,
            config,
            viewport,
            fractional,
            configured: false,
            logical: (0, 0),
            scale,
            reported_scale: None,
            integer_scale,
            buffers: ShmBuffers::new(id, self.max_buffers),
            geometry_dirty: true,
            callback_pending: false,
            repaint: true,
            opaque: Vec::new(),
            stats: Stats {
                bare_commits: 1,
                ..Stats::default()
            },
        };
        self.stats.bare_commits += 1;
        self.surfaces.insert(id, surface);
        self.host.surface_attached(id, node, monitor);
    }

    fn destroy_surface(&mut self, id: SurfaceId) {
        let Some(mut s) = self.surfaces.remove(&id) else {
            return;
        };
        self.by_wl.remove(&s.wl().id());
        self.dirty.remove(&id);
        if let Some(t) = self.deadline_timers.remove(&id) {
            self.handle.remove(t);
        }
        s.buffers.destroy();
        if let Some(f) = s.fractional.take() {
            f.destroy();
        }
        if let Some(v) = s.viewport.take() {
            v.destroy();
        }
        // Dropping the layer surface destroys it and its wl_surface.
        drop(s);
        self.clock.forget(id);
        self.host.surface_detached(id);
    }

    fn destroy_node_surfaces(&mut self, node: NodeId) {
        for id in self.surfaces_of(node) {
            self.destroy_surface(id);
        }
    }

    fn output_added(&mut self, output: wl_output::WlOutput) {
        let Some(info) = self.output_state.info(&output) else {
            return;
        };
        let global = info.id;
        let now = Instant::now();
        self.forget_expired(now);
        let plugged = self.monitors.plug(
            global,
            &info.make,
            &info.model,
            info.description.as_deref().unwrap_or(""),
            info.name.clone(),
            now,
        );
        self.output_globals.insert(output.id(), global);
        self.outputs.insert(global, output);
        self.host
            .monitor_added(&plugged.monitor, plugged.reconnected);
        let nodes: Vec<NodeId> = self.specs.keys().copied().collect();
        for node in nodes {
            self.reconcile(node);
        }
    }

    fn output_removed(&mut self, output: &wl_output::WlOutput) {
        let Some(global) = self.output_globals.remove(&output.id()) else {
            return;
        };
        self.outputs.remove(&global);
        let ids: Vec<SurfaceId> = self
            .surfaces
            .values()
            .filter(|s| s.output == global)
            .map(|s| s.id)
            .collect();
        for id in ids {
            self.destroy_surface(id);
        }
        if let Some(monitor) = self.monitors.unplug(global, Instant::now()) {
            if self.focused.as_ref() == Some(&monitor.id) {
                self.focused = None;
            }
            self.host.monitor_removed(&monitor);
            self.arm_expiry();
        }
        // `screens: focused` surfaces follow the new focus.
        let nodes: Vec<NodeId> = self.specs.keys().copied().collect();
        for node in nodes {
            self.reconcile(node);
        }
    }

    fn arm_expiry(&mut self) {
        if self.expiry_timer.is_some() {
            return;
        }
        let Some(at) = self.monitors.next_expiry() else {
            return;
        };
        let token =
            self.handle
                .insert_source(Timer::from_deadline(at), |_, _, state: &mut State<H>| {
                    state.forget_expired(Instant::now());
                    match state.monitors.next_expiry() {
                        Some(next) => TimeoutAction::ToInstant(next),
                        None => {
                            state.expiry_timer = None;
                            TimeoutAction::Drop
                        }
                    }
                });
        match token {
            Ok(t) => self.expiry_timer = Some(t),
            Err(e) => log::warn!("cannot arm the monitor expiry timer: {}", e.error),
        }
    }

    fn forget_expired(&mut self, now: Instant) {
        for monitor in self.monitors.expire(now) {
            self.ids.retain(|(_, m), _| *m != monitor.id);
            self.host.monitor_forgotten(&monitor);
        }
    }

    // ---- geometry ---------------------------------------------------------

    /// Re-derives the buffer size after a configure or a scale change and
    /// schedules a paint if it changed.
    fn update_geometry(&mut self, id: SurfaceId) {
        let Some(s) = self.surfaces.get_mut(&id) else {
            return;
        };
        if !s.configured {
            return;
        }
        let size = s.buffer_size();
        let scale = if s.is_fractional() {
            s.scale
        } else {
            Scale::from_integer(s.integer_scale.max(1) as u32).unwrap_or(Scale::ONE)
        };
        s.scale = scale;
        if size == s.buffers.size() && s.reported_scale == Some(scale) {
            return;
        }
        s.reported_scale = Some(scale);
        s.buffers.resize(size);
        s.geometry_dirty = true;
        s.repaint = true;
        self.host.surface_configured(id, size, scale);
        self.mark(id);
    }

    // ---- painting ----------------------------------------------------------

    fn draw(&mut self, id: SurfaceId) {
        let Some(s) = self.surfaces.get_mut(&id) else {
            return;
        };
        if !s.configured || s.callback_pending {
            return;
        }
        if !(s.repaint || self.host.wants_frame(id)) {
            return;
        }
        let acquired = match s.buffers.acquire(&self.shm, &self.qh) {
            Ok(Some(a)) => a,
            Ok(None) => {
                // Every buffer is with the compositor: the next release
                // brings us back.
                s.repaint = true;
                return;
            }
            Err(e) => {
                log::error!("{}: {e}", s.config.namespace);
                s.repaint = false;
                return;
            }
        };
        s.repaint = false;
        let size = s.buffers.size();
        let stride = s.buffers.stride();
        let scale = s.scale;
        let time = self.clock.predict(id);
        let Some(pixels) = s.buffers.pixels(acquired.index) else {
            log::error!(
                "{}: buffer {} has no memory",
                s.config.namespace,
                acquired.index
            );
            return;
        };
        let damage = match PaintTarget::new(pixels, size, stride, scale, acquired.age) {
            Ok(target) => {
                let mut target = target.at(time);
                self.host.paint(id, &mut target)
            }
            Err(e) => {
                log::error!("{}: {e}", s.config.namespace);
                return;
            }
        };
        s.stats.paints += 1;
        self.stats.paints += 1;
        let wants_more = self.host.wants_frame(id);
        let wl = s.wl().clone();
        if damage.is_empty() {
            // Nothing drawn, nothing recorded: the buffer keeps its age.
            s.stats.empty_paints += 1;
            self.stats.empty_paints += 1;
            if !wants_more {
                return;
            }
            if let Some(at) = self.host.frame_deadline(id) {
                self.arm_deadline(id, at);
            } else {
                wl.frame(&self.qh, FrameCallbackData(wl.clone()));
                s.callback_pending = true;
                wl.commit();
                s.stats.frame_requests += 1;
                s.stats.bare_commits += 1;
                self.stats.frame_requests += 1;
                self.stats.bare_commits += 1;
            }
            return;
        }
        let Some(buffer) = s.buffers.buffer(acquired.index).cloned() else {
            s.buffers.slots.invalidate(acquired.index);
            s.repaint = true;
            self.host.frame_dropped(id);
            return;
        };
        if s.geometry_dirty {
            s.geometry_dirty = false;
            match &s.viewport {
                Some(vp) if s.fractional.is_some() => {
                    wl.set_buffer_scale(1);
                    vp.set_destination(s.logical.0 as i32, s.logical.1 as i32);
                }
                _ => wl.set_buffer_scale(s.integer_scale.max(1)),
            }
        }
        wl.attach(Some(&buffer), 0, 0);
        if wl.version() >= 4 {
            for r in damage.rects() {
                wl.damage_buffer(r.x, r.y, clamp_i32(r.w), clamp_i32(r.h));
            }
        } else {
            wl.damage(0, 0, i32::MAX, i32::MAX);
        }
        let opaque = scale.inner_logical_region(&self.host.opaque_region(id));
        if opaque != s.opaque {
            if opaque.is_empty() {
                wl.set_opaque_region(None);
            } else if let Ok(region) = Region::new(&self.compositor) {
                for r in &opaque {
                    region.add(r.x, r.y, clamp_i32(r.w), clamp_i32(r.h));
                }
                wl.set_opaque_region(Some(region.wl_region()));
            }
            s.opaque = opaque;
            s.stats.opaque_updates += 1;
            self.stats.opaque_updates += 1;
        }
        if let Some(p) = &self.presentation {
            p.feedback(&wl, &self.qh, FeedbackTag(id));
        }
        if wants_more {
            wl.frame(&self.qh, FrameCallbackData(wl.clone()));
            s.callback_pending = true;
            s.stats.frame_requests += 1;
            self.stats.frame_requests += 1;
        }
        wl.commit();
        s.buffers.slots.commit(acquired.index);
        s.stats.commits += 1;
        self.stats.commits += 1;
    }

    fn arm_deadline(&mut self, id: SurfaceId, at: Instant) {
        if let Some(t) = self.deadline_timers.remove(&id) {
            self.handle.remove(t);
        }
        let token = self.handle.insert_source(
            Timer::from_deadline(at),
            move |_, _, state: &mut State<H>| {
                state.deadline_timers.remove(&id);
                state.mark(id);
                TimeoutAction::Drop
            },
        );
        match token {
            Ok(t) => {
                self.deadline_timers.insert(id, t);
            }
            Err(e) => log::warn!("cannot arm a frame deadline: {}", e.error),
        }
    }

    // ---- events ----------------------------------------------------------

    fn surface_for(&self, wl: &wl_surface::WlSurface) -> Option<SurfaceId> {
        self.by_wl.get(&wl.id()).copied()
    }

    fn send_input(&self, event: InputEvent) {
        // A dropped receiver means nobody listens; that is fine.
        let _ = self.input.send(event);
    }
}

fn clamp_i32(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

/// The output's fractional scale from its current mode and xdg-output
/// logical size (1920 px shown as 1280 logical → 1.5).
fn estimate_scale(info: &smithay_client_toolkit::output::OutputInfo) -> Option<Scale> {
    let mode = info.modes.iter().find(|m| m.current)?;
    let (lw, lh) = info.logical_size?;
    let physical = mode.dimensions.0.max(mode.dimensions.1);
    let logical = lw.max(lh);
    if physical <= 0 || logical <= 0 {
        return None;
    }
    Scale::new(((physical as f64 * 120.0) / logical as f64).round() as u32)
}

fn to_sctk_layer(layer: Layer) -> wlr_layer::Layer {
    match layer {
        Layer::Background => wlr_layer::Layer::Background,
        Layer::Bottom => wlr_layer::Layer::Bottom,
        Layer::Top => wlr_layer::Layer::Top,
        Layer::Overlay => wlr_layer::Layer::Overlay,
    }
}

fn apply_layer_config(layer: &LayerSurface, c: &LayerConfig) {
    let mut anchor = wlr_layer::Anchor::empty();
    anchor.set(wlr_layer::Anchor::TOP, c.anchors.top);
    anchor.set(wlr_layer::Anchor::BOTTOM, c.anchors.bottom);
    anchor.set(wlr_layer::Anchor::LEFT, c.anchors.left);
    anchor.set(wlr_layer::Anchor::RIGHT, c.anchors.right);
    layer.set_anchor(anchor);
    layer.set_size(c.width, c.height);
    layer.set_exclusive_zone(c.exclusive_zone);
    let [t, r, b, l] = c.margin;
    layer.set_margin(t, r, b, l);
    layer.set_keyboard_interactivity(match c.keyboard {
        Keyboard::None => KeyboardInteractivity::None,
        Keyboard::OnDemand => KeyboardInteractivity::OnDemand,
        Keyboard::Exclusive => KeyboardInteractivity::Exclusive,
    });
}

// ---- SCTK handlers -----------------------------------------------------------

impl<H: SurfaceHost + 'static> CompositorHandler for State<H> {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        let Some(id) = self.surface_for(surface) else {
            return;
        };
        if let Some(s) = self.surfaces.get_mut(&id) {
            s.integer_scale = new_factor.max(1);
        }
        self.update_geometry(id);
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
        // Buffers stay untransformed; the compositor rotates them.
    }

    fn frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        let Some(id) = self.surface_for(surface) else {
            return;
        };
        self.stats.frames_done += 1;
        if let Some(s) = self.surfaces.get_mut(&id) {
            s.callback_pending = false;
            s.stats.frames_done += 1;
        }
        self.mark(id);
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl<H: SurfaceHost + 'static> OutputHandler for State<H> {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.output_added(output);
    }

    fn update_output(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        // A monitor whose make, model or description changed is a
        // different monitor.
        let Some(info) = self.output_state.info(&output) else {
            return;
        };
        let Some(global) = self.output_globals.get(&output.id()).copied() else {
            return;
        };
        let same = self
            .monitors
            .id_of(global)
            .and_then(|id| self.monitors.get(id))
            .is_some_and(|m| {
                m.make == info.make
                    && m.model == info.model
                    && m.description == info.description.clone().unwrap_or_default()
            });
        if !same {
            self.output_removed(&output);
            self.output_added(output);
        }
    }

    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.output_removed(&output);
    }
}

impl<H: SurfaceHost + 'static> LayerShellHandler for State<H> {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        // The compositor took it away (usually its output is going). It
        // comes back when its output does or its spec changes.
        if let Some(id) = self.surface_for(layer.wl_surface()) {
            self.stats.closed += 1;
            self.destroy_surface(id);
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let Some(id) = self.surface_for(layer.wl_surface()) else {
            return;
        };
        self.stats.configures += 1;
        let Some(s) = self.surfaces.get_mut(&id) else {
            return;
        };
        s.stats.configures += 1;
        let (w, h) = configure.new_size;
        // 0 means "your choice": what we asked for.
        let w = if w == 0 { s.config.width.max(1) } else { w };
        let h = if h == 0 { s.config.height.max(1) } else { h };
        if s.logical != (w, h) {
            s.geometry_dirty = true;
        }
        s.logical = (w, h);
        let first = !s.configured;
        s.configured = true;
        if first {
            s.repaint = true;
        }
        self.update_geometry(id);
        self.mark(id);
    }
}

impl<H: SurfaceHost + 'static> SeatHandler for State<H> {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            match self.seat_state.get_pointer(qh, &seat) {
                Ok(p) => self.pointers.push((seat, p)),
                Err(e) => log::warn!("cannot get the pointer: {e}"),
            }
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            self.pointers.retain(|(s, p)| {
                let mine = *s == seat;
                if mine {
                    p.release();
                }
                !mine
            });
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

fn axis_delta(a: &smithay_client_toolkit::seat::pointer::AxisScroll) -> AxisDelta {
    AxisDelta {
        pixels: a.absolute,
        value120: if a.value120 != 0 {
            a.value120
        } else {
            a.discrete.saturating_mul(120)
        },
        stop: a.stop,
    }
}

impl<H: SurfaceHost + 'static> PointerHandler for State<H> {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for e in events {
            let Some(surface) = self.surface_for(&e.surface) else {
                continue;
            };
            let position = LogicalPoint::new(e.position.0 as f32, e.position.1 as f32);
            let event = match &e.kind {
                PointerEventKind::Enter { .. } => {
                    if let Some(s) = self.surfaces.get(&surface) {
                        self.focused = Some(s.monitor.clone());
                    }
                    InputEvent::PointerEnter { surface, position }
                }
                PointerEventKind::Leave { .. } => InputEvent::PointerLeave { surface },
                PointerEventKind::Motion { time } => InputEvent::PointerMotion {
                    surface,
                    position,
                    time: *time,
                },
                PointerEventKind::Press { time, button, .. }
                | PointerEventKind::Release { time, button, .. } => InputEvent::PointerButton {
                    surface,
                    position,
                    button: *button,
                    state: if matches!(e.kind, PointerEventKind::Press { .. }) {
                        ButtonState::Pressed
                    } else {
                        ButtonState::Released
                    },
                    time: *time,
                },
                PointerEventKind::Axis {
                    time,
                    horizontal,
                    vertical,
                    source,
                } => InputEvent::PointerAxis {
                    surface,
                    position,
                    horizontal: axis_delta(horizontal),
                    vertical: axis_delta(vertical),
                    source: source.map(|s| match s {
                        wl_pointer::AxisSource::Finger => AxisSource::Finger,
                        wl_pointer::AxisSource::Continuous => AxisSource::Continuous,
                        wl_pointer::AxisSource::WheelTilt => AxisSource::WheelTilt,
                        _ => AxisSource::Wheel,
                    }),
                    time: *time,
                },
            };
            self.send_input(event);
        }
    }
}

impl<H: SurfaceHost + 'static> ShmHandler for State<H> {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl<H: SurfaceHost + 'static> ProvidesRegistryState for State<H> {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

delegate_registry!(@<H: SurfaceHost + 'static> State<H>);
delegate_dispatch2!(@<H: SurfaceHost + 'static> State<H>);

// ---- our own protocol objects ------------------------------------------------

impl<H: SurfaceHost + 'static> Dispatch2<wl_buffer::WlBuffer, State<H>> for BufferData {
    fn event(
        &self,
        state: &mut State<H>,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
        if let wl_buffer::Event::Release = event {
            state.stats.releases += 1;
            let Some(s) = state.surfaces.get_mut(&self.surface) else {
                return;
            };
            s.stats.releases += 1;
            if s.buffers.release(self) && s.repaint {
                state.mark(self.surface);
            }
        }
    }
}

impl<H: SurfaceHost + 'static> Dispatch2<WpViewporter, State<H>> for StrandGlobal {
    fn event(
        &self,
        _: &mut State<H>,
        _: &WpViewporter,
        _: wp_viewporter::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
    }
}

impl<H: SurfaceHost + 'static> Dispatch2<WpViewport, State<H>> for SurfaceTag {
    fn event(
        &self,
        _: &mut State<H>,
        _: &WpViewport,
        _: wp_viewport::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
    }
}

impl<H: SurfaceHost + 'static> Dispatch2<WpFractionalScaleManagerV1, State<H>> for StrandGlobal {
    fn event(
        &self,
        _: &mut State<H>,
        _: &WpFractionalScaleManagerV1,
        _: wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
    }
}

impl<H: SurfaceHost + 'static> Dispatch2<WpFractionalScaleV1, State<H>> for SurfaceTag {
    fn event(
        &self,
        state: &mut State<H>,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            let Some(scale) = Scale::new(scale) else {
                return;
            };
            if let Some(s) = state.surfaces.get_mut(&self.0) {
                s.scale = scale;
            }
            state.update_geometry(self.0);
        }
    }
}

impl<H: SurfaceHost + 'static> Dispatch2<WpPresentation, State<H>> for StrandGlobal {
    fn event(
        &self,
        state: &mut State<H>,
        _: &WpPresentation,
        event: wp_presentation::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
        if let wp_presentation::Event::ClockId { clk_id } = event {
            state.clock.set_clock_id(clk_id);
        }
    }
}

impl<H: SurfaceHost + 'static> Dispatch2<WpPresentationFeedback, State<H>> for FeedbackTag {
    fn event(
        &self,
        state: &mut State<H>,
        _: &WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
        match event {
            wp_presentation_feedback::Event::Presented {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
                refresh,
                seq_hi,
                seq_lo,
                flags: _,
            } => {
                let secs = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
                let presentation = Presentation {
                    time: Duration::new(secs, tv_nsec.min(999_999_999)),
                    refresh: (refresh > 0).then(|| Duration::from_nanos(u64::from(refresh))),
                    seq: (u64::from(seq_hi) << 32) | u64::from(seq_lo),
                };
                state.stats.presented += 1;
                if let Some(s) = state.surfaces.get_mut(&self.0) {
                    s.stats.presented += 1;
                    state.clock.presented(self.0, presentation);
                }
            }
            wp_presentation_feedback::Event::Discarded => {
                state.stats.discarded += 1;
                if let Some(s) = state.surfaces.get_mut(&self.0) {
                    s.stats.discarded += 1;
                    state.clock.discarded(self.0);
                }
            }
            _ => {}
        }
    }
}
