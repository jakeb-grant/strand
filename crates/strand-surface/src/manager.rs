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
use smithay_client_toolkit::error::GlobalError;
use smithay_client_toolkit::globals::{GlobalData, ProvidesBoundGlobal};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::keyboard::{
    KeyEvent, KeyboardHandler, Keysym, Modifiers as XkbModifiers, RawModifiers, RepeatInfo,
};
use smithay_client_toolkit::seat::pointer::{
    CursorIcon, PointerEvent, PointerEventKind, PointerHandler, ThemeSpec, ThemedPointer,
};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    self, KeyboardInteractivity, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::xdg::XdgPositioner;
use smithay_client_toolkit::shell::xdg::popup::{Popup, PopupConfigure, PopupHandler};
use smithay_client_toolkit::shm::raw::RawPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_dispatch2, delegate_registry, registry_handlers};
use wayland_client::backend::ObjectId;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{
    wl_buffer, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface,
};
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
use wayland_protocols::xdg::shell::client::{xdg_positioner, xdg_wm_base::XdgWmBase};

use strand_scene::{
    Keyboard, Layer, LogicalPoint, LogicalSize, NodeId, NodeKind, PaintTarget, Painter, Rect,
    Scale, Screens, Size, SurfaceChange, SurfaceId, SurfaceSpec,
};

use crate::clock::{FrameClock, Presentation, PresentationClock};
use crate::input::{AxisDelta, AxisSource, ButtonState, InputEvent};
use crate::monitor::{Geometry, Monitor, MonitorId, Monitors};
use crate::placement::{
    LayerConfig, PlacementError, PopupConfig, PopupSide, layer_config, popup_config,
};
use crate::shm::{BufferData, MAX_BUFFERS, ShmBuffers};
use strand_scene::{KeyInput, Modifiers};

/// How long an unmapped surface whose paint drew nothing, while its
/// painter still wants a frame, waits before it is painted again (no frame
/// callbacks come before the first buffer): about one 60 Hz frame.
const UNMAPPED_RETRY: Duration = Duration::from_millis(16);

/// What the surface manager calls on the main thread: the [`Painter`]
/// (render) plus surface lifecycle notifications, which the binary forwards
/// to `Renderer::attach_surface`, `configure_surface` and `detach_surface`.
/// Every method but `paint` and `wants_frame` has a no-op default.
pub trait SurfaceHost: Painter {
    /// A Wayland surface now shows `node` on `monitor`. `None` for a
    /// `screens: focused` surface the compositor places itself; its
    /// monitor follows in [`SurfaceHost::surface_entered`].
    fn surface_attached(&mut self, surface: SurfaceId, node: NodeId, monitor: Option<&Monitor>) {
        let _ = (surface, node, monitor);
    }
    /// The compositor showed a `screens: focused` surface on `monitor`
    /// (first `wl_surface.enter`, after its first frame).
    fn surface_entered(&mut self, surface: SurfaceId, monitor: &Monitor) {
        let _ = (surface, monitor);
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
    /// A plugged-in monitor's scale, logical size or position changed
    /// (its identity did not; a new make, model or description is an
    /// unplug and a plug).
    fn monitor_changed(&mut self, monitor: &Monitor) {
        let _ = monitor;
    }
    /// A monitor was unplugged; it is remembered for 30 s.
    fn monitor_removed(&mut self, monitor: &Monitor) {
        let _ = monitor;
    }
    /// An unplugged monitor did not come back within 30 s.
    fn monitor_forgotten(&mut self, monitor: &Monitor) {
        let _ = monitor;
    }
    /// The painter is holding `surface`'s frame until this instant
    /// (`Renderer::frame_deadline`: a first frame waiting for its text, or
    /// a later one for a new node's).
    /// While it is `Some` and [`Painter::wants_frame`] is false, nothing
    /// is painted, even on a first configure or resize; the manager asks
    /// again at the deadline or on the next `poll()`. Also used after a
    /// paint that drew nothing while `wants_frame` stayed true; `None`
    /// then asks again on the next frame callback.
    fn frame_deadline(&self, surface: SurfaceId) -> Option<Instant> {
        let _ = surface;
        None
    }
    /// A paint returned damage but its buffer could not be committed; the
    /// painter must not count that frame (`Renderer::invalidate`).
    fn frame_dropped(&mut self, surface: SurfaceId) {
        let _ = surface;
    }
    /// Pointer input on one of the surfaces, on the main thread, before
    /// it goes out on the [`SurfaceManager::take_input`] channel: render
    /// hit-tests here (`hover`, `pressed`) and forwards node events to
    /// logic.
    fn input(&mut self, event: &InputEvent) {
        let _ = event;
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
    /// Dirty marks that waited because the surface's last frame was still
    /// in flight (refresh-rate throttling).
    pub throttled: u64,
    /// Cursor images set on pointer enter (`wp_cursor_shape_v1` or the
    /// cursor theme).
    pub cursor_sets: u64,
    /// Popups created with an `xdg_popup.grab` (opened within
    /// [`GRAB_WINDOW`] of a press).
    pub grabs: u64,
}

/// A snapshot of one surface.
#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceInfo {
    pub id: SurfaceId,
    pub node: NodeId,
    pub kind: NodeKind,
    /// The monitor it is on: the one its spec chose, or for `screens:
    /// focused` the one the compositor showed it on (`None` until then).
    pub monitor: Option<MonitorId>,
    /// A `screens: focused` surface (one per node, placed by the
    /// compositor or by [`State::set_focused_monitor`]).
    pub focused: bool,
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
    /// `damage_buffer` rects of the last buffer commit.
    pub last_damage: Vec<Rect>,
    /// The opaque region last sent, in surface-local logical pixels.
    pub opaque_region: Vec<Rect>,
    /// Input passes through (an `osd`: empty input region).
    pub click_through: bool,
    /// The input region: `None` the whole surface, `Some(None)` empty,
    /// else the box `(x, y, w, h)` inside the shadow overhang.
    pub input_region: Option<Option<(i32, i32, i32, i32)>>,
    /// A click-away catcher is mapped under it.
    pub click_away: bool,
    pub stats: Stats,
}

/// Where a surface of a node goes; with the node, the key of its stable
/// [`SurfaceId`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Placement {
    Monitor(MonitorId),
    /// `screens: focused`: one surface, on the focused output.
    Focused,
}

struct Surface {
    id: SurfaceId,
    /// Unique per created surface (ids are reused across recreation):
    /// stale protocol events carry an older one.
    generation: u64,
    node: NodeId,
    kind: NodeKind,
    placement: Placement,
    /// The monitor and `wl_output` global it is on (for `Focused`, known
    /// after the first enter).
    monitor: Option<MonitorId>,
    output: Option<u32>,
    /// The output asked for at creation (`None`: the compositor picks).
    requested_output: Option<u32>,
    role: Role,
    /// A layer surface's state; a popup's input region and size as one.
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
    /// Buffer commits so far; tags presentation feedback.
    commit_seq: u64,
    /// The buffer commit whose presentation (or discard) we wait for
    /// before painting again: frames lock to the refresh rate.
    in_flight: Option<u64>,
    /// A configure was acked and no commit has followed yet.
    ack_pending: bool,
    /// A paint is owed: first configure, resize, rescale or a request.
    repaint: bool,
    /// Last opaque region sent, in logical pixels.
    opaque: Vec<Rect>,
    last_damage: Vec<Rect>,
    click_through: bool,
    /// The input region last sent: `None` the whole surface, `Some(None)`
    /// empty, else the box inside the overhang (logical pixels).
    input_region: Option<Option<(i32, i32, i32, i32)>>,
    stats: Stats,
}

/// The bound `xdg_wm_base` (sctk's `XdgShell` would also bind the
/// toplevel decoration manager and need a window handler).
#[derive(Debug)]
struct WmBase(XdgWmBase);

impl ProvidesBoundGlobal<XdgWmBase, 5> for WmBase {
    fn bound_global(&self) -> Result<XdgWmBase, GlobalError> {
        Ok(self.0.clone())
    }
}

impl ProvidesBoundGlobal<XdgWmBase, 6> for WmBase {
    fn bound_global(&self) -> Result<XdgWmBase, GlobalError> {
        Ok(self.0.clone())
    }
}

/// What a Wayland surface of ours is.
enum Role {
    Layer(LayerSurface),
    /// An `xdg_popup` nested in the surface `parent` (a layer surface or
    /// another popup).
    Popup {
        popup: Popup,
        parent: SurfaceId,
        config: PopupConfig,
        /// It was made with an `xdg_popup.grab` (asked for, and a press
        /// came within [`GRAB_WINDOW`]); only these take the keyboard
        /// and obey xdg-shell's topmost-grab rule.
        grabbed: bool,
    },
}

impl Role {
    fn wl(&self) -> &wl_surface::WlSurface {
        match self {
            Role::Layer(l) => l.wl_surface(),
            Role::Popup { popup, .. } => popup.wl_surface(),
        }
    }

    fn popup_parent(&self) -> Option<SurfaceId> {
        match self {
            Role::Popup { parent, .. } => Some(*parent),
            Role::Layer(_) => None,
        }
    }

    /// A popup made with an `xdg_popup.grab`.
    fn grabbed(&self) -> bool {
        matches!(self, Role::Popup { grabbed: true, .. })
    }
}

impl Surface {
    fn wl(&self) -> &wl_surface::WlSurface {
        self.role.wl()
    }

    fn is_fractional(&self) -> bool {
        self.viewport.is_some() && self.fractional.is_some()
    }

    /// Waiting for a frame callback or for the last frame's presentation.
    fn throttled(&self) -> bool {
        self.callback_pending || self.in_flight.is_some()
    }

    /// A buffer has been committed since the surface was created, so the
    /// compositor maps it (and only then sends frame callbacks).
    fn mapped(&self) -> bool {
        self.commit_seq > 0
    }

    /// The scale buffers are painted at: the fractional one, or the
    /// integer buffer scale on the fallback path.
    fn effective_scale(&self) -> Scale {
        if self.is_fractional() {
            self.scale
        } else {
            Scale::from_integer(self.integer_scale.max(1) as u32).unwrap_or(Scale::ONE)
        }
    }

    /// The latest configure or preferred scale asks for a buffer size or
    /// scale not painted yet.
    fn geometry_changed(&self) -> bool {
        self.configured
            && (self.buffer_size() != self.buffers.size()
                || self.reported_scale != Some(self.effective_scale()))
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
            kind: self.kind,
            monitor: self.monitor.clone(),
            focused: self.placement == Placement::Focused,
            namespace: self.config.namespace.clone(),
            configured: self.configured,
            logical_size: self.logical,
            buffer_size: self.buffers.size(),
            scale: self.scale,
            fractional: self.is_fractional(),
            buffers: self.buffers.slots.len(),
            last_damage: self.last_damage.clone(),
            opaque_region: self.opaque.clone(),
            click_through: self.click_through,
            input_region: self.input_region,
            click_away: false,
            stats: self.stats,
        }
    }
}

/// A transparent layer surface over an output's usable area, mapped with
/// an open `keyboard: exclusive` surface whose `open` is two-way (the
/// design's launcher): a press on it is a click outside that surface,
/// reported as [`InputEvent::ClickAway`] on it. The order of surfaces in
/// one layer is undefined (wlr-layer-shell; sway 1.9 puts the older one
/// on top for input), so it does not rely on being below: its input
/// region has a hole where that surface's box is, computed as the
/// compositor arranges both in the same area (exclusive zone 0, all four
/// edges). It takes no keyboard; clicks on bars (outside the usable
/// area) do not reach it (the router closes the surface on a press on
/// any other Strand surface, its own bar included). Every other output
/// that shows no surface of the same node gets a catcher of its own,
/// over the whole output (exclusive zone -1, bars included) and with no
/// hole.
struct Catcher {
    layer: LayerSurface,
    /// On the output of the surface it serves, with a hole for it.
    primary: bool,
    /// The output it is on (the global's name), if one was asked for.
    output: Option<u32>,
    viewport: Option<WpViewport>,
    /// Its one transparent buffer: 1×1 scaled up by the viewport, or the
    /// output's size without viewporter.
    buffer: Option<(RawPool, wl_buffer::WlBuffer)>,
    /// Its configured size: the output's usable area, where the surface
    /// it serves is arranged too.
    size: Option<(u32, u32)>,
    /// The hole its input region leaves (that surface's box), as last
    /// sent.
    hole: Option<(i32, i32, i32, i32)>,
}

impl Catcher {
    fn destroy(&mut self) {
        if let Some((_, b)) = self.buffer.take() {
            b.destroy();
        }
        if let Some(v) = self.viewport.take() {
            v.destroy();
        }
    }
}

/// User data of a catcher's buffer (nothing to track: it is never
/// written after it is created).
#[derive(Debug)]
pub struct CatcherBuffer;

/// True if a surface of `spec` gets a click-away catcher under it.
fn wants_catcher(spec: &SurfaceSpec) -> bool {
    spec.layer.is_some()
        && spec.kind != NodeKind::Bar
        && spec.keyboard == Keyboard::Exclusive
        && spec.open_two_way
        && spec.open
}

/// User data for globals this crate binds itself.
#[derive(Debug)]
pub struct StrandGlobal;

/// User data for per-surface protocol objects.
#[derive(Debug)]
pub struct SurfaceTag(SurfaceId);

/// User data for presentation feedback: the surface, its generation and
/// the buffer commit the feedback is for.
#[derive(Debug)]
pub struct FeedbackTag {
    surface: SurfaceId,
    generation: u64,
    seq: u64,
}

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
    /// `xdg_wm_base`, for popups (none: popups are not shown).
    xdg_shell: Option<WmBase>,
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
    /// Click-away catchers, by the surface they serve, and the surface
    /// each catcher's `wl_surface` serves.
    catchers: HashMap<SurfaceId, Vec<Catcher>>,
    catcher_of: HashMap<ObjectId, SurfaceId>,
    /// Stable ids per (node, placement), kept while the monitor is
    /// remembered so a replugged monitor gets its surface id back.
    ids: HashMap<(NodeId, Placement), SurfaceId>,
    next_id: u32,
    next_generation: u64,
    dirty: BTreeSet<SurfaceId>,
    flush_scheduled: bool,
    pointers: Vec<SeatPointer>,
    /// Each seat's keyboard (xkbcommon keymaps). Key repeat is ours
    /// ([`State::start_repeat`]), not SCTK's: SCTK's repeat timer is
    /// removed from a `Drop` that runs while calloop's sources are
    /// borrowed when a keyboard goes away, and panics.
    keyboards: Vec<(wl_seat::WlSeat, wl_keyboard::WlKeyboard)>,
    /// Each keyboard's repeat rate and delay (`wl_keyboard.repeat_info`).
    repeat_info: HashMap<ObjectId, RepeatInfo>,
    /// The key repeating now: its keyboard, its key and its timer.
    key_repeat: Option<(ObjectId, u32, RegistrationToken)>,
    /// Each keyboard's last `wl_keyboard.modifiers` state (depressed,
    /// latched, locked, layout): a change on the repeating key's keyboard
    /// stops it, since its text was computed with the old one.
    raw_modifiers: HashMap<ObjectId, (u32, u32, u32, u32)>,
    /// The surface with keyboard focus, and the modifiers held.
    keyboard_focus: Option<SurfaceId>,
    modifiers: Modifiers,
    /// The surface the last pointer button press landed on (a popup
    /// opens from it when its parent shows on several).
    last_pressed: Option<SurfaceId>,
    /// Layer surfaces whose keyboard interactivity is `exclusive` for now
    /// because a grabbing popup of theirs is open (see
    /// [`State::sync_popup_keyboard`]).
    grab_keyboard: BTreeSet<SurfaceId>,
    /// The grabbing popup keys go to while its layer surface has keyboard
    /// focus (told a `KeyboardEnter` of its own).
    grab_focus: Option<SurfaceId>,
    /// The last user action (a button or key press) on one of our
    /// surfaces: a popup opened within [`GRAB_WINDOW`] of it grabs with
    /// its serial.
    last_action: Option<UserAction>,
    /// Popups the compositor dismissed (Escape, a click away) whose spec
    /// still says open: not shown again until it says closed.
    dismissed: BTreeSet<NodeId>,
    /// Set by [`State::set_focused_monitor`]; `None` lets the compositor
    /// place `screens: focused` surfaces.
    focused: Option<MonitorId>,
    /// Created by [`SurfaceManager::take_input`]; events are dropped
    /// until then.
    input: Option<mpsc::Sender<InputEvent>>,
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

/// How long after a press a popup that opens still counts as opened by
/// it, and grabs with its serial. xdg-shell wants the serial of the user
/// action that opened the popup; compositors that check it (KWin, Mutter)
/// end the popup at once when the serial is stale. A popup opened by a
/// timer, `on change` or IPC later than this has no grab.
pub const GRAB_WINDOW: Duration = Duration::from_millis(500);

/// A button or key press: its seat, serial and when it arrived.
struct UserAction {
    seat: wl_seat::WlSeat,
    serial: u32,
    at: Instant,
}

/// A seat's pointer, with the cursor it shows over our surfaces.
struct SeatPointer {
    seat: wl_seat::WlSeat,
    pointer: ThemedPointer,
    /// Serial of the last button press (for popup grabs), and of the
    /// last enter.
    button_serial: Option<u32>,
    enter_serial: Option<u32>,
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
        let xdg_shell = globals
            .bind::<XdgWmBase, _, _>(&qh, 1..=6, GlobalData)
            .ok()
            .map(WmBase);
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
            xdg_shell,
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
            catchers: HashMap::new(),
            catcher_of: HashMap::new(),
            ids: HashMap::new(),
            next_id: 1,
            next_generation: 1,
            dirty: BTreeSet::new(),
            flush_scheduled: false,
            pointers: Vec::new(),
            keyboards: Vec::new(),
            repeat_info: HashMap::new(),
            key_repeat: None,
            raw_modifiers: HashMap::new(),
            keyboard_focus: None,
            last_pressed: None,
            last_action: None,
            grab_keyboard: BTreeSet::new(),
            grab_focus: None,
            dismissed: BTreeSet::new(),
            modifiers: Modifiers::default(),
            focused: None,
            input: None,
            stats: Stats::default(),
            expiry_timer: None,
            deadline_timers: HashMap::new(),
        };
        Ok(Self {
            event_loop,
            state,
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

    /// The receiving end of the input channel (once). Input arriving
    /// before this is called is not queued (the host still sees it through
    /// [`SurfaceHost::input`]).
    pub fn take_input(&mut self) -> Option<mpsc::Receiver<InputEvent>> {
        if self.state.input.is_some() {
            return None;
        }
        let (tx, rx) = mpsc::channel();
        self.state.input = Some(tx);
        Some(rx)
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
        self.surfaces
            .keys()
            .filter_map(|id| self.surface(*id))
            .collect()
    }

    pub fn surface(&self, id: SurfaceId) -> Option<SurfaceInfo> {
        let mut info = self.surfaces.get(&id).map(Surface::info)?;
        info.click_away = self
            .catchers
            .get(&id)
            .is_some_and(|v| v.iter().any(|c| c.primary && c.buffer.is_some()));
        Some(info)
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
                self.dismissed.remove(&node);
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

    /// Places `screens: focused` surfaces on `monitor` (from a compositor
    /// IPC service), moving open ones there. `None` (the default) lets the
    /// compositor choose: wlr-layer-shell puts a surface created without
    /// an output on the output the user last interacted with.
    pub fn set_focused_monitor(&mut self, monitor: Option<MonitorId>) {
        if self.focused == monitor {
            return;
        }
        self.focused = monitor;
        self.reconcile_all();
    }

    /// The focused monitor set by [`State::set_focused_monitor`].
    pub fn focused_monitor(&self) -> Option<&MonitorId> {
        self.focused.as_ref()
    }

    /// Serial of the last pointer button press on one of our surfaces, for
    /// popup grabs (`xdg_popup.grab`).
    pub fn last_button_serial(&self) -> Option<u32> {
        self.pointers.iter().filter_map(|p| p.button_serial).max()
    }

    /// The `wl_output` global of the focused monitor, when one is set and
    /// plugged in.
    fn focused_output(&self) -> Option<u32> {
        let id = self.focused.as_ref()?;
        self.outputs
            .keys()
            .copied()
            .find(|g| self.monitors.id_of(*g) == Some(id))
    }

    fn wants(spec: &SurfaceSpec, monitor: &Monitor) -> bool {
        match &spec.screens {
            Screens::All => true,
            Screens::Focused => false,
            Screens::Named(names) => names.iter().any(|n| {
                n == monitor.id.as_str() || monitor.connector.as_deref() == Some(n.as_str())
            }),
        }
    }

    fn reconcile_all(&mut self) {
        let nodes: Vec<NodeId> = self.specs.keys().copied().collect();
        for node in nodes {
            self.reconcile(node);
        }
    }

    /// Creates and destroys `node`'s surfaces so there is exactly one on
    /// each monitor its spec asks for (one in all for `screens: focused`).
    fn reconcile(&mut self, node: NodeId) {
        let Some(spec) = self.specs.get(&node).cloned() else {
            return;
        };
        if spec.kind == NodeKind::Popup {
            self.reconcile_popup(node, &spec);
            return;
        }
        let mapped = match layer_config(&spec) {
            Ok(_) => spec.open,
            Err(e @ PlacementError::NotLayerSurface(_)) => {
                log::debug!("{}: {e}", spec.namespace());
                false
            }
            Err(e) => {
                log::warn!("{}: {e}", spec.namespace());
                false
            }
        };
        let focused = spec.screens == Screens::Focused;
        let wanted: Vec<(u32, Monitor)> = if mapped && !focused {
            self.outputs
                .keys()
                .filter_map(|g| {
                    let m = self.monitors.get(self.monitors.id_of(*g)?)?;
                    Self::wants(&spec, m).then(|| (*g, m.clone()))
                })
                .collect()
        } else {
            Vec::new()
        };
        let want_focused = mapped && focused && !self.outputs.is_empty();
        let target = self.focused_output();
        let stale: Vec<SurfaceId> = self
            .surfaces
            .values()
            .filter(|s| s.node == node)
            .filter(|s| match &s.placement {
                Placement::Monitor(m) => !wanted.iter().any(|(_, w)| &w.id == m),
                Placement::Focused => {
                    !want_focused
                        || target.is_some_and(|t| match s.output {
                            // Shown elsewhere, or asked for elsewhere and
                            // not shown yet: move it.
                            Some(on) => on != t,
                            None => s.requested_output != Some(t),
                        })
                }
            })
            .map(|s| s.id)
            .collect();
        for id in stale {
            self.destroy_surface(id);
        }
        let has = |state: &Self, p: &Placement| {
            state
                .surfaces
                .values()
                .any(|s| s.node == node && &s.placement == p)
        };
        for (global, monitor) in wanted {
            let placement = Placement::Monitor(monitor.id.clone());
            if !has(self, &placement) {
                self.create_surface(node, &spec, placement, Some(global));
            }
        }
        if want_focused && !has(self, &Placement::Focused) {
            self.create_surface(node, &spec, Placement::Focused, target);
        }
    }

    // ---- popups -------------------------------------------------------------

    /// The surface a popup of `spec` nests in: one showing its parent
    /// node, mapped (the last one a button was pressed on, when the parent
    /// shows on several).
    fn popup_parent(&self, spec: &SurfaceSpec) -> Option<SurfaceId> {
        let parent = spec.parent?;
        let mut candidates: Vec<&Surface> = self
            .surfaces
            .values()
            .filter(|s| s.node == parent && s.mapped() && s.configured)
            .collect();
        candidates.sort_by_key(|s| std::cmp::Reverse(Some(s.id) == self.last_pressed));
        candidates.first().map(|s| s.id)
    }

    /// Creates or destroys `node`'s popup so it shows exactly while open
    /// and its parent is mapped.
    fn reconcile_popup(&mut self, node: NodeId, spec: &SurfaceSpec) {
        let existing = self.surfaces_of(node);
        if !spec.open {
            self.dismissed.remove(&node);
        }
        let parent = (spec.open && !self.dismissed.contains(&node))
            .then(|| self.popup_parent(spec))
            .flatten();
        let parent_spec = spec.parent.and_then(|p| self.specs.get(&p)).cloned();
        let config = match (&parent, &parent_spec) {
            (Some(_), Some(ps)) => popup_config(spec, ps).ok(),
            _ => None,
        };
        log::trace!(
            "{}: popup open {} parent {:?} config {:?} (spec parent {:?}, anchor {:?}, size {:?}×{:?})",
            spec.namespace(),
            spec.open,
            parent,
            config,
            spec.parent,
            spec.anchor_rect,
            spec.width,
            spec.height
        );
        let Some((parent, config)) = parent.zip(config) else {
            for id in existing {
                self.destroy_surface(id);
            }
            return;
        };
        if existing.is_empty() {
            self.create_popup(node, spec, parent, config);
        }
    }

    /// The popups nested in surfaces of `node` (now mapped or gone) are
    /// reconciled.
    fn reconcile_children(&mut self, node: NodeId) {
        let kids: Vec<NodeId> = self
            .specs
            .iter()
            .filter(|(_, s)| s.kind == NodeKind::Popup && s.parent == Some(node))
            .map(|(n, _)| *n)
            .collect();
        for k in kids {
            self.reconcile(k);
        }
    }

    /// A popup spec changed: a new size or anchor repositions it (or, on
    /// an `xdg_wm_base` older than version 3, makes it again).
    fn reconfigure_popups(&mut self, node: NodeId) {
        let Some(spec) = self.specs.get(&node).cloned() else {
            return;
        };
        let parent_spec = spec.parent.and_then(|p| self.specs.get(&p)).cloned();
        let new = parent_spec.and_then(|ps| popup_config(&spec, &ps).ok());
        for id in self.surfaces_of(node) {
            let Some(new) = new.clone() else {
                self.destroy_surface(id);
                continue;
            };
            let Some(positioner) = self.positioner(&new) else {
                continue;
            };
            let Some(s) = self.surfaces.get_mut(&id) else {
                continue;
            };
            let Role::Popup { popup, config, .. } = &mut s.role else {
                continue;
            };
            if *config == new {
                continue;
            }
            if new.grab != config.grab || new.namespace != config.namespace {
                self.destroy_surface(id);
                continue;
            }
            if popup.xdg_popup().version() >= 3 {
                popup.reposition(&positioner, 0);
                *config = new.clone();
                s.config = new.as_layer();
                s.geometry_dirty = true;
                popup.wl_surface().commit();
                s.stats.bare_commits += 1;
                self.stats.bare_commits += 1;
            } else {
                self.destroy_surface(id);
            }
        }
        self.reconcile(node);
    }

    fn positioner(&self, c: &PopupConfig) -> Option<XdgPositioner> {
        let shell = self.xdg_shell.as_ref()?;
        let p = XdgPositioner::new(shell).ok()?;
        p.set_size(c.width.max(1) as i32, c.height.max(1) as i32);
        let (x, y, w, h) = c.anchor_rect;
        p.set_anchor_rect(x, y, w.max(1), h.max(1));
        use xdg_positioner::{Anchor, ConstraintAdjustment as Adj, Gravity};
        let (anchor, gravity, offset, flip) = match c.side {
            PopupSide::Below => (Anchor::Bottom, Gravity::Bottom, (0, c.gap), Adj::FlipY),
            PopupSide::Above => (Anchor::Top, Gravity::Top, (0, -c.gap), Adj::FlipY),
            PopupSide::Right => (Anchor::Right, Gravity::Right, (c.gap, 0), Adj::FlipX),
            PopupSide::Left => (Anchor::Left, Gravity::Left, (-c.gap, 0), Adj::FlipX),
        };
        p.set_anchor(anchor);
        p.set_gravity(gravity);
        p.set_offset(offset.0, offset.1);
        p.set_constraint_adjustment(Adj::SlideX | Adj::SlideY | flip);
        Some(p)
    }

    /// Creates `node`'s popup in surface `parent`, grabbing with the last
    /// button or key press when it came within [`GRAB_WINDOW`] (never for
    /// a tooltip).
    fn create_popup(
        &mut self,
        node: NodeId,
        spec: &SurfaceSpec,
        parent: SurfaceId,
        config: PopupConfig,
    ) {
        if self.xdg_shell.is_none() {
            log::debug!("{}: no xdg_wm_base, popups are not shown", spec.namespace());
            return;
        }
        let Some(positioner) = self.positioner(&config) else {
            return;
        };
        // It grabs only with the serial of a press just made: a popup
        // opened later (a timer, `on change`, IPC) has no grab, so it
        // neither takes the keyboard nor closes other grabbing popups.
        let grab = match &self.last_action {
            Some(a) if config.grab && a.at.elapsed() <= GRAB_WINDOW => {
                Some((a.seat.clone(), a.serial))
            }
            _ => None,
        };
        if grab.is_some() {
            self.dismiss_other_grabs(parent);
            // The layer surface takes the keyboard before the grab starts:
            // once it has, the compositor moves keyboard focus no more.
            if let Some(layer) = self.root_layer(parent) {
                self.set_grab_keyboard(layer, true);
            }
        }
        let Some(shell) = self.xdg_shell.as_ref() else {
            return;
        };
        let Some(ps) = self.surfaces.get(&parent) else {
            self.sync_popup_keyboard();
            return;
        };
        let wl = self.compositor.create_surface(&self.qh);
        let parent_xdg = match &ps.role {
            Role::Popup { popup, .. } => Some(popup.xdg_surface().clone()),
            Role::Layer(_) => None,
        };
        let popup = match Popup::from_surface(
            parent_xdg.as_ref(),
            &positioner,
            &self.qh,
            wl.clone(),
            shell,
        ) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("{}: no popup: {e}", spec.namespace());
                self.sync_popup_keyboard();
                return;
            }
        };
        if let Role::Layer(layer) = &ps.role {
            layer.get_popup(popup.xdg_popup());
        }
        let (monitor, output, scale_src) = (ps.monitor.clone(), ps.output, ps.output);
        if let Some((seat, serial)) = &grab {
            popup.xdg_popup().grab(seat, *serial);
            self.stats.grabs += 1;
        }
        let key = (node, Placement::Focused);
        let id = match self.ids.get(&key) {
            Some(id) => *id,
            None => {
                let id = SurfaceId(self.next_id);
                self.next_id = self.next_id.wrapping_add(1).max(1);
                self.ids.insert(key, id);
                id
            }
        };
        let generation = self.next_generation;
        self.next_generation += 1;
        let (viewport, fractional) = match (&self.viewporter, &self.fractional_manager) {
            (Some(vp), Some(fm)) => (
                Some(vp.get_viewport(&wl, &self.qh, SurfaceTag(id))),
                Some(fm.get_fractional_scale(&wl, &self.qh, SurfaceTag(id))),
            ),
            _ => (None, None),
        };
        let (scale, integer_scale) = self.initial_scale(scale_src, fractional.is_some());
        let layer_like = config.as_layer();
        let click_through = layer_like.click_through;
        if click_through {
            match Region::new(&self.compositor) {
                Ok(region) => wl.set_input_region(Some(region.wl_region())),
                Err(e) => log::warn!("{}: no input region: {e}", config.namespace),
            }
        }
        wl.commit();
        self.by_wl.insert(wl.id(), id);
        let surface = Surface {
            id,
            generation,
            node,
            kind: spec.kind,
            placement: Placement::Focused,
            monitor: monitor.clone(),
            output,
            requested_output: output,
            role: Role::Popup {
                popup,
                parent,
                config,
                grabbed: grab.is_some(),
            },
            config: layer_like,
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
            commit_seq: 0,
            in_flight: None,
            ack_pending: false,
            repaint: true,
            opaque: Vec::new(),
            last_damage: Vec::new(),
            click_through,
            input_region: click_through.then_some(None),
            stats: Stats {
                bare_commits: 1,
                ..Stats::default()
            },
        };
        self.stats.bare_commits += 1;
        self.surfaces.insert(id, surface);
        let monitor = monitor.and_then(|m| self.monitors.get(&m)).cloned();
        self.host.surface_attached(id, node, monitor.as_ref());
        self.sync_popup_keyboard();
    }

    /// The layer surface popup `id` is nested in (itself for a layer
    /// surface).
    fn root_layer(&self, mut id: SurfaceId) -> Option<SurfaceId> {
        for _ in 0..64 {
            match &self.surfaces.get(&id)?.role {
                Role::Layer(_) => return Some(id),
                Role::Popup { parent, .. } => id = *parent,
            }
        }
        None
    }

    /// True if popup `id` is nested, at any depth, in surface `ancestor`.
    fn is_nested_in(&self, mut id: SurfaceId, ancestor: SurfaceId) -> bool {
        for _ in 0..64 {
            match self.surfaces.get(&id).and_then(|s| s.role.popup_parent()) {
                Some(p) if p == ancestor => return true,
                Some(p) => id = p,
                None => return false,
            }
        }
        false
    }

    /// Makes layer surface `id` `exclusive` for a popup grab (`on`), or
    /// gives it back its own keyboard interactivity.
    fn set_grab_keyboard(&mut self, id: SurfaceId, on: bool) {
        if on == self.grab_keyboard.contains(&id) {
            return;
        }
        let Some(s) = self.surfaces.get_mut(&id) else {
            return;
        };
        let Role::Layer(layer) = &s.role else {
            return;
        };
        let k = if on {
            self.grab_keyboard.insert(id);
            Keyboard::Exclusive
        } else {
            self.grab_keyboard.remove(&id);
            s.config.keyboard
        };
        layer.set_keyboard_interactivity(match k {
            Keyboard::None => KeyboardInteractivity::None,
            Keyboard::OnDemand => KeyboardInteractivity::OnDemand,
            Keyboard::Exclusive => KeyboardInteractivity::Exclusive,
        });
        layer.commit();
        s.stats.bare_commits += 1;
        self.stats.bare_commits += 1;
    }

    /// The topmost grabbing popup nested in layer surface `layer`.
    fn topmost_grab(&self, layer: SurfaceId) -> Option<SurfaceId> {
        let grabbing = |s: &Surface| s.role.grabbed();
        self.surfaces
            .values()
            .filter(|s| grabbing(s) && self.root_layer(s.id) == Some(layer))
            .find(|s| {
                !self
                    .surfaces
                    .values()
                    .any(|c| grabbing(c) && c.role.popup_parent() == Some(s.id))
            })
            .map(|s| s.id)
    }

    /// Keeps the keyboard with the grabbing popups. An `xdg_popup` grab
    /// asks for the keyboard, but compositors give it to the popup only
    /// through its parent's focus, and a layer surface with `keyboard:
    /// none` (a bar) never has focus: so while a grabbing popup is open
    /// its layer surface is `exclusive` (as an app's menu holds the
    /// keyboard), and keys arriving on it go to the topmost grabbing
    /// popup, which is told a `KeyboardEnter` of its own. Escape then
    /// closes the calendar of a `keyboard: none` bar.
    fn sync_popup_keyboard(&mut self) {
        let layers: Vec<SurfaceId> = self
            .surfaces
            .values()
            .filter(|s| matches!(s.role, Role::Layer(_)))
            .map(|s| s.id)
            .collect();
        for id in layers {
            let want = self.topmost_grab(id).is_some();
            self.set_grab_keyboard(id, want);
        }
        self.grab_keyboard
            .retain(|id| self.surfaces.contains_key(id));
        let want = self
            .keyboard_focus
            .and_then(|f| self.root_layer(f))
            .and_then(|l| self.topmost_grab(l));
        if want == self.grab_focus {
            return;
        }
        // Keys move from the old target to the new one: each is told,
        // as the compositor would tell surfaces of their own focus (the
        // surface that has it already was told by the compositor). A
        // popup whose nested popup takes the keys is not told it lost
        // them: a leave closes an `open: <->` popup, and the nested one
        // with it.
        let old = std::mem::replace(&mut self.grab_focus, want);
        if let Some(o) = old
            && self.surfaces.contains_key(&o)
            && Some(o) != self.keyboard_focus
            && !want.is_some_and(|w| self.is_nested_in(w, o))
        {
            self.send_input(InputEvent::KeyboardLeave { surface: o });
        }
        match want {
            Some(p) => {
                if Some(p) != self.keyboard_focus {
                    self.send_input(InputEvent::KeyboardEnter { surface: p });
                }
            }
            // The grab ended: keys go back to the surface with focus.
            None => {
                if let Some(f) = self.keyboard_focus
                    && old != Some(f)
                    && self.surfaces.contains_key(&f)
                {
                    self.send_input(InputEvent::KeyboardEnter { surface: f });
                }
            }
        }
    }

    /// Pushes a changed spec to `node`'s live surfaces in place.
    fn reconfigure(&mut self, node: NodeId) {
        let Some(spec) = self.specs.get(&node) else {
            return;
        };
        if spec.kind == NodeKind::Popup {
            self.reconfigure_popups(node);
            return;
        }
        let new = layer_config(spec);
        let catcher = wants_catcher(spec);
        let ids = self.surfaces_of(node);
        for id in ids {
            match (catcher, self.catchers.contains_key(&id)) {
                // A catcher goes under the surface: made again, catcher
                // first (`reconcile` follows).
                (true, false) => {
                    self.destroy_surface(id);
                    continue;
                }
                (false, true) => self.destroy_catcher(id),
                _ => {}
            }
            let Ok(mut config) = new.clone() else {
                self.destroy_surface(id);
                continue;
            };
            let Some(s) = self.surfaces.get_mut(&id) else {
                continue;
            };
            if let Some(size) = s
                .monitor
                .as_ref()
                .and_then(|m| self.monitors.get(m))
                .and_then(|m| m.logical_size)
            {
                config.fit(size);
            }
            if s.config == config {
                continue;
            }
            if s.config.layer != config.layer || s.config.namespace != config.namespace {
                // Layer and namespace are fixed at creation.
                self.destroy_surface(id);
                continue;
            }
            let Role::Layer(layer) = &s.role else {
                continue;
            };
            apply_layer_config(layer, &config);
            if self.grab_keyboard.contains(&id) {
                layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
            }
            s.config = config;
            layer.commit();
            s.stats.bare_commits += 1;
            self.stats.bare_commits += 1;
            self.update_catcher(id);
        }
    }

    /// The initial buffer scale for a surface on `output` (`None`: wherever
    /// the compositor puts it) until the compositor states its preference.
    fn initial_scale(&self, output: Option<u32>, fractional: bool) -> (Scale, i32) {
        let infos: Vec<_> = match output {
            Some(g) => self
                .outputs
                .get(&g)
                .and_then(|o| self.output_state.info(o))
                .into_iter()
                .collect(),
            None => self
                .outputs
                .values()
                .filter_map(|o| self.output_state.info(o))
                .collect(),
        };
        // Unknown output: a value every output shares, else the default.
        fn shared<T: PartialEq + Copy>(mut v: impl Iterator<Item = T>) -> Option<T> {
            let first = v.next()?;
            v.all(|x| x == first).then_some(first)
        }
        let integer_scale = shared(infos.iter().map(|i| i.scale_factor.max(1))).unwrap_or(1);
        let fallback = Scale::from_integer(integer_scale as u32).unwrap_or(Scale::ONE);
        let scale = if fractional {
            // From the output's mode and logical size, so the first frame
            // is already sharp.
            shared(infos.iter().map(estimate_scale))
                .flatten()
                .unwrap_or(fallback)
        } else {
            fallback
        };
        (scale, integer_scale)
    }

    fn create_surface(
        &mut self,
        node: NodeId,
        spec: &SurfaceSpec,
        placement: Placement,
        global: Option<u32>,
    ) {
        let mut config = match layer_config(spec) {
            Ok(c) => c,
            Err(PlacementError::AutoSize(_) | PlacementError::NotLayerSurface(_)) => return,
        };
        let output = match global {
            Some(g) => match self.outputs.get(&g) {
                Some(o) => Some(o.clone()),
                None => return,
            },
            None => None,
        };
        let monitor = global
            .and_then(|g| self.monitors.id_of(g))
            .and_then(|id| self.monitors.get(id))
            .cloned();
        if let Some(size) = monitor.as_ref().and_then(|m| m.logical_size) {
            config.fit(size);
        }
        let key = (node, placement.clone());
        let id = match self.ids.get(&key) {
            Some(id) => *id,
            None => {
                let id = SurfaceId(self.next_id);
                self.next_id = self.next_id.wrapping_add(1).max(1);
                self.ids.insert(key, id);
                id
            }
        };
        let generation = self.next_generation;
        self.next_generation += 1;
        if wants_catcher(spec) {
            self.create_catcher(id, node, &config, global);
        }
        let wl = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            wl.clone(),
            to_sctk_layer(config.layer),
            Some(config.namespace.clone()),
            output.as_ref(),
        );
        apply_layer_config(&layer, &config);
        let (viewport, fractional) = match (&self.viewporter, &self.fractional_manager) {
            (Some(vp), Some(fm)) => (
                Some(vp.get_viewport(&wl, &self.qh, SurfaceTag(id))),
                Some(fm.get_fractional_scale(&wl, &self.qh, SurfaceTag(id))),
            ),
            _ => (None, None),
        };
        let (scale, integer_scale) = self.initial_scale(global, fractional.is_some());
        // An OSD is click-through (design example d): an empty input region
        // lets clicks reach the windows beneath it. A shadowed surface
        // takes input on its box only (set once its size is known).
        let click_through = config.click_through;
        if click_through {
            match Region::new(&self.compositor) {
                Ok(region) => wl.set_input_region(Some(region.wl_region())),
                Err(e) => log::warn!("{}: no input region: {e}", config.namespace),
            }
        }
        layer.commit();
        self.by_wl.insert(wl.id(), id);
        let surface = Surface {
            id,
            generation,
            node,
            kind: spec.kind,
            placement,
            monitor: monitor.as_ref().map(|m| m.id.clone()),
            output: global,
            requested_output: global,
            role: Role::Layer(layer),
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
            commit_seq: 0,
            in_flight: None,
            ack_pending: false,
            repaint: true,
            opaque: Vec::new(),
            last_damage: Vec::new(),
            click_through,
            input_region: click_through.then_some(None),
            stats: Stats {
                bare_commits: 1,
                ..Stats::default()
            },
        };
        self.stats.bare_commits += 1;
        self.surfaces.insert(id, surface);
        self.host.surface_attached(id, node, monitor.as_ref());
    }

    /// Maps click-away catchers for surface `id` on output `global`, on
    /// its layer: one on that output, and one over each other output
    /// (see [`Catcher`]).
    fn create_catcher(
        &mut self,
        id: SurfaceId,
        node: NodeId,
        config: &LayerConfig,
        global: Option<u32>,
    ) {
        self.destroy_catcher(id);
        let output = global.and_then(|g| self.outputs.get(&g)).cloned();
        let primary = self.new_catcher(id, config, (global, output.as_ref()), true);
        self.catchers.insert(id, vec![primary]);
        if let Some(g) = global {
            self.add_secondary_catchers(id, node, config, g);
        }
    }

    /// Adds a catcher over every output but `global` (where surface `id`
    /// is) that shows no surface of `node`, and takes the node's other
    /// surfaces' catchers off `global`.
    fn add_secondary_catchers(
        &mut self,
        id: SurfaceId,
        node: NodeId,
        config: &LayerConfig,
        global: u32,
    ) {
        let siblings: Vec<SurfaceId> = self
            .surfaces
            .values()
            .filter(|s| s.node == node && s.id != id)
            .map(|s| s.id)
            .collect();
        let shown: BTreeSet<u32> = siblings
            .iter()
            .filter_map(|s| self.surfaces.get(s).and_then(|s| s.output))
            .collect();
        for sid in &siblings {
            let gone: Vec<Catcher> = match self.catchers.get_mut(sid) {
                Some(list) => {
                    let (gone, keep) = std::mem::take(list)
                        .into_iter()
                        .partition(|c| !c.primary && c.output == Some(global));
                    *list = keep;
                    gone
                }
                None => Vec::new(),
            };
            for mut c in gone {
                self.catcher_of.remove(&c.layer.wl_surface().id());
                c.destroy();
            }
        }
        let others: Vec<(u32, wl_output::WlOutput)> = self
            .outputs
            .iter()
            .filter(|(g, _)| **g != global && !shown.contains(*g))
            .map(|(g, o)| (*g, o.clone()))
            .collect();
        for (g, o) in others {
            let c = self.new_catcher(id, config, (Some(g), Some(&o)), false);
            self.catchers.entry(id).or_default().push(c);
        }
    }

    fn new_catcher(
        &mut self,
        id: SurfaceId,
        config: &LayerConfig,
        (global, output): (Option<u32>, Option<&wl_output::WlOutput>),
        primary: bool,
    ) -> Catcher {
        let wl = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            wl.clone(),
            to_sctk_layer(config.layer),
            Some(format!("{}-click-away", config.namespace)),
            output,
        );
        layer.set_anchor(
            wlr_layer::Anchor::TOP
                | wlr_layer::Anchor::BOTTOM
                | wlr_layer::Anchor::LEFT
                | wlr_layer::Anchor::RIGHT,
        );
        layer.set_size(0, 0);
        // The primary one shares the usable area its surface is arranged
        // in (its hole is computed there); the others span their whole
        // output, bars included.
        layer.set_exclusive_zone(if primary { 0 } else { -1 });
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();
        let viewport = self
            .viewporter
            .as_ref()
            .map(|vp| vp.get_viewport(&wl, &self.qh, SurfaceTag(id)));
        self.catcher_of.insert(wl.id(), id);
        Catcher {
            layer,
            primary,
            output: global,
            viewport,
            buffer: None,
            size: None,
            hole: None,
        }
    }

    fn destroy_catcher(&mut self, id: SurfaceId) {
        for mut c in self.catchers.remove(&id).unwrap_or_default() {
            self.catcher_of.remove(&c.layer.wl_surface().id());
            c.destroy();
        }
    }

    /// The compositor closed catcher `wl` of surface `id` (its output is
    /// going): only it goes, unless it was the one on that surface's own
    /// output.
    fn catcher_closed(&mut self, id: SurfaceId, wl: &wl_surface::WlSurface) {
        let Some(list) = self.catchers.get_mut(&id) else {
            return;
        };
        let Some(i) = list.iter().position(|c| c.layer.wl_surface() == wl) else {
            return;
        };
        if list[i].primary {
            self.destroy_catcher(id);
            return;
        }
        let mut c = list.remove(i);
        self.catcher_of.remove(&wl.id());
        c.destroy();
    }

    /// Catcher `wl` of surface `id` was configured at `(w, h)`: it maps
    /// with one transparent buffer covering that.
    fn configure_catcher(&mut self, id: SurfaceId, wl: &wl_surface::WlSurface, (w, h): (u32, u32)) {
        let Some(c) = self
            .catchers
            .get_mut(&id)
            .and_then(|v| v.iter_mut().find(|c| c.layer.wl_surface() == wl))
        else {
            return;
        };
        let (bw, bh) = if c.viewport.is_some() { (1, 1) } else { (w, h) };
        let (Ok(bw), Ok(bh)) = (i32::try_from(bw.max(1)), i32::try_from(bh.max(1))) else {
            return;
        };
        let Some(len) = (bw as usize)
            .checked_mul(bh as usize)
            .and_then(|n| n.checked_mul(4))
        else {
            return;
        };
        let pool = match RawPool::new(len, &self.shm) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("no buffer for a click-away catcher: {e}");
                return;
            }
        };
        let mut pool = pool;
        // A fresh pool is zeroed: fully transparent.
        let buffer = pool.create_buffer(
            0,
            bw,
            bh,
            bw * 4,
            wl_shm::Format::Argb8888,
            CatcherBuffer,
            &self.qh,
        );
        let wl = c.layer.wl_surface();
        if let Some(v) = &c.viewport {
            v.set_destination(clamp_i32(w.max(1)), clamp_i32(h.max(1)));
        }
        wl.attach(Some(&buffer), 0, 0);
        wl.damage_buffer(0, 0, bw, bh);
        if let Some((_, old)) = c.buffer.replace((pool, buffer)) {
            old.destroy();
        }
        c.size = Some((w, h));
        c.hole = None;
        if !c.primary {
            // No hole: its input region is all of it (the default).
            wl.commit();
            return;
        }
        // Committed with its input region.
        self.update_catcher(id);
    }

    /// Sets catcher `id`'s input region: all of it but the box of the
    /// surface it serves (one pixel wider each way, so the box's edge
    /// never closes it), then commits.
    fn update_catcher(&mut self, id: SurfaceId) {
        let Some(s) = self.surfaces.get(&id) else {
            return;
        };
        let size = if s.configured {
            s.logical
        } else {
            (s.config.width, s.config.height)
        };
        let Some(c) = self
            .catchers
            .get_mut(&id)
            .and_then(|v| v.iter_mut().find(|c| c.primary))
        else {
            return;
        };
        let Some(area) = c.size else {
            return;
        };
        let hole = s.config.box_in(size, area);
        if c.hole == Some(hole) {
            return;
        }
        let wl = c.layer.wl_surface();
        match Region::new(&self.compositor) {
            Ok(r) => {
                r.add(0, 0, clamp_i32(area.0), clamp_i32(area.1));
                let (x, y, w, h) = hole;
                r.subtract(
                    x.saturating_sub(1),
                    y.saturating_sub(1),
                    w.saturating_add(2),
                    h.saturating_add(2),
                );
                wl.set_input_region(Some(r.wl_region()));
                c.hole = Some(hole);
            }
            Err(e) => log::warn!("no input region for a click-away catcher: {e}"),
        }
        wl.commit();
    }

    /// The surface the click-away catcher `wl` serves.
    fn catcher_for(&self, wl: &wl_surface::WlSurface) -> Option<SurfaceId> {
        self.catcher_of.get(&wl.id()).copied()
    }

    fn destroy_surface(&mut self, id: SurfaceId) {
        // Popups nested in it go first, innermost first (a popup must be
        // the topmost when it is destroyed).
        let children: Vec<SurfaceId> = self
            .surfaces
            .values()
            .filter(|c| c.role.popup_parent() == Some(id))
            .map(|c| c.id)
            .collect();
        for c in children {
            self.destroy_surface(c);
        }
        self.destroy_catcher(id);
        let Some(mut s) = self.surfaces.remove(&id) else {
            return;
        };
        self.by_wl.remove(&s.wl().id());
        self.dirty.remove(&id);
        if self.keyboard_focus == Some(id) {
            self.keyboard_focus = None;
            // A compositor may skip the leave for a destroyed surface:
            // the release would then never reach us.
            self.stop_repeat();
        }
        self.cancel_deadline(id);
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
        // `grab_focus` may name it still: the sync moves it on and tells
        // the new target (not the gone one).
        self.sync_popup_keyboard();
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
        let monitor = self
            .monitors
            .set_geometry(global, output_geometry(&info))
            .unwrap_or(plugged.monitor);
        self.output_globals.insert(output.id(), global);
        self.outputs.insert(global, output);
        self.host.monitor_added(&monitor, plugged.reconnected);
        self.reconcile_all();
    }

    fn output_removed(&mut self, output: &wl_output::WlOutput) {
        let Some(global) = self.output_globals.remove(&output.id()) else {
            return;
        };
        self.outputs.remove(&global);
        let ids: Vec<SurfaceId> = self
            .surfaces
            .values()
            .filter(|s| s.output == Some(global) || s.requested_output == Some(global))
            .map(|s| s.id)
            .collect();
        for id in ids {
            self.destroy_surface(id);
        }
        if let Some(monitor) = self.monitors.unplug(global, Instant::now()) {
            self.host.monitor_removed(&monitor);
            self.arm_expiry();
        }
        // `screens: focused` surfaces it showed come back on another one.
        self.reconcile_all();
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
            self.ids
                .retain(|(_, p), _| *p != Placement::Monitor(monitor.id.clone()));
            self.host.monitor_forgotten(&monitor);
        }
    }

    // ---- geometry ---------------------------------------------------------

    /// Re-derives the buffer size from the latest configure and scale, right
    /// before a paint, so all the events of one wakeup (a configure plus a
    /// `preferred_scale`) make one `surface_configured` and one resize.
    /// Returns false if the surface is gone or not configured.
    fn update_geometry(&mut self, id: SurfaceId) -> bool {
        let Some(s) = self.surfaces.get_mut(&id) else {
            return false;
        };
        if !s.configured {
            return false;
        }
        let size = s.buffer_size();
        let scale = s.effective_scale();
        s.scale = scale;
        if size == s.buffers.size() && s.reported_scale == Some(scale) {
            return true;
        }
        s.reported_scale = Some(scale);
        s.buffers.resize(size);
        s.geometry_dirty = true;
        s.repaint = true;
        self.host.surface_configured(id, size, scale);
        true
    }

    // ---- painting ----------------------------------------------------------

    fn draw(&mut self, id: SurfaceId) {
        let Some(s) = self.surfaces.get_mut(&id) else {
            return;
        };
        if !s.configured {
            return;
        }
        if s.throttled() {
            if s.geometry_changed() {
                // A new size or scale supersedes the frame in flight: paint
                // now, so a surface the compositor does not present (an
                // occluded bar, an output in DPMS off) still applies its
                // configure. The old frame's feedback comes back discarded
                // and no longer matches `in_flight`; a late frame callback
                // only marks the surface again.
                s.in_flight = None;
                s.callback_pending = false;
            } else {
                // The frame in flight's callback or presentation marks it
                // again; whatever changed meanwhile is painted then, once.
                // A same-size configure's ack is committed then too (a
                // bare commit now would only discard the frame's
                // presentation feedback).
                s.stats.throttled += 1;
                self.stats.throttled += 1;
                return;
            }
        }
        if !self.update_geometry(id) {
            return;
        }
        let wants = self.host.wants_frame(id);
        if !wants && let Some(at) = self.host.frame_deadline(id) {
            // The painter holds this frame (text it would show is still
            // being shaped): ask again at its deadline, or sooner
            // when new content marks the surface. The paint stays owed.
            if let Some(s) = self.surfaces.get_mut(&id) {
                s.repaint = true;
            }
            self.commit_ack(id);
            self.arm_deadline(id, at);
            return;
        }
        let Some(s) = self.surfaces.get_mut(&id) else {
            return;
        };
        if !(s.repaint || wants) {
            // A configure that needs no new frame (margins, exclusive zone)
            // still takes effect only with a commit.
            self.commit_ack(id);
            return;
        }
        // Painting now: a deadline armed for an earlier, held or empty
        // paint would only wake the loop for nothing.
        self.cancel_deadline(id);
        let Some(s) = self.surfaces.get_mut(&id) else {
            return;
        };
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
            let mapped = s.mapped();
            if !mapped {
                // No buffer yet: the first frame is still owed.
                s.repaint = true;
            }
            if let Some(at) = self.host.frame_deadline(id) {
                // The painter held the frame after all (what it collected
                // while painting asks for a configure first): ask again
                // at its deadline.
                s.repaint = true;
                self.commit_ack(id);
                self.arm_deadline(id, at);
                return;
            }
            if !wants_more {
                self.commit_ack(id);
                return;
            }
            if !mapped {
                // The compositor sends no frame callbacks to an unmapped
                // surface: one would never come and would block every
                // later paint. Ask again after about a frame instead.
                self.commit_ack(id);
                self.arm_deadline(id, Instant::now() + UNMAPPED_RETRY);
            } else {
                wl.frame(&self.qh, FrameCallbackData(wl.clone()));
                s.callback_pending = true;
                s.ack_pending = false;
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
            // A popup's window geometry is its box: the compositor
            // positions that, and its shadow reaches past it.
            if let Role::Popup { popup, config, .. } = &s.role {
                let [t, r, b, l] = config.overhang;
                let (w, h) = s.logical;
                popup.xdg_surface().set_window_geometry(
                    l,
                    t,
                    (w as i32 - l - r).max(1),
                    (h as i32 - t - b).max(1),
                );
            }
        }
        wl.attach(Some(&buffer), 0, 0);
        if wl.version() >= 4 {
            for r in damage.rects() {
                wl.damage_buffer(r.x, r.y, clamp_i32(r.w), clamp_i32(r.h));
            }
            s.last_damage = damage.rects().to_vec();
        } else {
            wl.damage(0, 0, i32::MAX, i32::MAX);
            s.last_damage = vec![Rect::new(0, 0, size.w, size.h)];
        }
        let opaque = scale.inner_logical_region(&self.host.opaque_region(id));
        if opaque != s.opaque {
            let sent = if opaque.is_empty() {
                wl.set_opaque_region(None);
                true
            } else {
                match Region::new(&self.compositor) {
                    Ok(region) => {
                        for r in &opaque {
                            region.add(r.x, r.y, clamp_i32(r.w), clamp_i32(r.h));
                        }
                        wl.set_opaque_region(Some(region.wl_region()));
                        true
                    }
                    Err(e) => {
                        log::warn!("{}: no opaque region: {e}", s.config.namespace);
                        false
                    }
                }
            };
            // Unsent regions are retried with the next frame.
            if sent {
                s.opaque = opaque;
                s.stats.opaque_updates += 1;
                self.stats.opaque_updates += 1;
            }
        }
        s.commit_seq += 1;
        if let Some(p) = &self.presentation {
            let tag = FeedbackTag {
                surface: id,
                generation: s.generation,
                seq: s.commit_seq,
            };
            p.feedback(&wl, &self.qh, tag);
        }
        // Throttle to the refresh rate: paint again only after this frame's
        // callback, or (when nothing is animating) its presentation, which
        // is requested anyway and costs no extra wakeup when idle.
        if wants_more || self.presentation.is_none() {
            wl.frame(&self.qh, FrameCallbackData(wl.clone()));
            s.callback_pending = true;
            s.stats.frame_requests += 1;
            self.stats.frame_requests += 1;
        } else {
            s.in_flight = Some(s.commit_seq);
        }
        s.ack_pending = false;
        wl.commit();
        s.buffers.slots.commit(acquired.index);
        s.stats.commits += 1;
        self.stats.commits += 1;
        if s.commit_seq == 1 {
            // Mapped: popups waiting for it as their parent come now.
            let node = s.node;
            self.reconcile_children(node);
        }
    }

    /// Sends a bare commit if a configure was acked and nothing has
    /// committed since, so the ack takes effect.
    fn commit_ack(&mut self, id: SurfaceId) {
        let Some(s) = self.surfaces.get_mut(&id) else {
            return;
        };
        if s.ack_pending {
            s.ack_pending = false;
            s.wl().commit();
            s.stats.bare_commits += 1;
            self.stats.bare_commits += 1;
        }
    }

    fn cancel_deadline(&mut self, id: SurfaceId) {
        if let Some(t) = self.deadline_timers.remove(&id) {
            self.handle.remove(t);
        }
    }

    /// Marks `id` again at `at` (one timer per surface; re-arming replaces
    /// it).
    fn arm_deadline(&mut self, id: SurfaceId, at: Instant) {
        self.cancel_deadline(id);
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

    /// Presentation feedback for commit `seq` of `surface` arrived (or was
    /// discarded): the frame is no longer in flight.
    fn frame_settled(&mut self, tag: &FeedbackTag) -> bool {
        let Some(s) = self.surfaces.get_mut(&tag.surface) else {
            return false;
        };
        if s.generation != tag.generation {
            return false;
        }
        if s.in_flight == Some(tag.seq) {
            s.in_flight = None;
            self.mark(tag.surface);
        }
        true
    }

    // ---- events ----------------------------------------------------------

    fn surface_for(&self, wl: &wl_surface::WlSurface) -> Option<SurfaceId> {
        self.by_wl.get(&wl.id()).copied()
    }

    /// A key event on the surface with keyboard focus.
    fn key(&mut self, event: KeyEvent, state: ButtonState, repeat: bool) {
        // During a popup grab, the topmost grabbing popup takes the keys.
        let Some(surface) = self.grab_focus.or(self.keyboard_focus) else {
            return;
        };
        let name = key_name(event.keysym);
        let text = event
            .utf8
            .filter(|t| t.chars().all(|c| !c.is_control()))
            .unwrap_or_default();
        self.send_input(InputEvent::Key {
            surface,
            key: KeyInput {
                name,
                text,
                state,
                repeat,
                modifiers: self.modifiers,
                time: event.time,
            },
        });
    }

    /// Repeats `event` (just pressed on `keyboard`) at the keyboard's
    /// rate after its delay, from a calloop timer of ours; modifiers do
    /// not repeat. Replaces any key repeating before.
    fn start_repeat(&mut self, keyboard: &wl_keyboard::WlKeyboard, event: KeyEvent) {
        self.stop_repeat();
        let Some(RepeatInfo::Repeat { rate, delay }) = self.repeat_info.get(&keyboard.id()) else {
            return;
        };
        if event.keysym.is_modifier_key() {
            return;
        }
        let interval = repeat_interval(rate.get());
        let raw = event.raw_code;
        let token = self.handle.insert_source(
            Timer::from_duration(Duration::from_millis(u64::from(*delay))),
            move |_, _, state: &mut State<H>| {
                // Nothing focused (its surface went without a leave): the
                // release will go elsewhere, so stop rather than wake at
                // the repeat rate forever.
                if state.grab_focus.or(state.keyboard_focus).is_none() {
                    state.key_repeat = None;
                    return TimeoutAction::Drop;
                }
                state.key(event.clone(), ButtonState::Pressed, true);
                TimeoutAction::ToDuration(interval)
            },
        );
        match token {
            Ok(t) => self.key_repeat = Some((keyboard.id(), raw, t)),
            Err(e) => log::warn!("cannot repeat a key: {}", e.error),
        }
    }

    /// Stops the key repeating, if any (from event handlers only, never a
    /// `Drop`).
    fn stop_repeat(&mut self) {
        if let Some((_, _, token)) = self.key_repeat.take() {
            self.handle.remove(token);
        }
    }

    /// True while a key repeats (tests).
    pub fn key_repeating(&self) -> bool {
        self.key_repeat.is_some()
    }

    /// The surface with keyboard focus, if it is one of ours.
    pub fn keyboard_focus(&self) -> Option<SurfaceId> {
        self.keyboard_focus
    }

    /// True while layer surface `id` is `exclusive` because a grabbing
    /// popup of its is open (see `sync_popup_keyboard`).
    pub fn holds_keyboard_for_popup(&self, id: SurfaceId) -> bool {
        self.grab_keyboard.contains(&id)
    }

    fn send_input(&mut self, event: InputEvent) {
        self.host.input(&event);
        if let Some(tx) = &self.input
            && tx.send(event).is_err()
        {
            // Nobody listens any more: stop queueing.
            self.input = None;
        }
    }
}

/// A keysym's xkb name without its `XK_` prefix (`Escape`, `Return`,
/// `Down`, `a`), or its code in hex for one without a name.
fn key_name(sym: Keysym) -> String {
    match sym.name() {
        Some(n) => n.strip_prefix("XK_").unwrap_or(n).to_string(),
        None => format!("0x{:x}", sym.raw()),
    }
}

/// The time between repeats at `rate` keys a second, at least 1 ms: the
/// rate is the compositor's (SCTK casts a negative one to a huge `u32`),
/// and a zero interval would fire on every loop iteration.
fn repeat_interval(rate: u32) -> Duration {
    Duration::from_micros((1_000_000 / u64::from(rate.max(1))).max(1_000))
}

fn clamp_i32(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

/// A monitor's scale, logical size and position from its output info.
fn output_geometry(info: &smithay_client_toolkit::output::OutputInfo) -> Geometry {
    let integer = Scale::from_integer(info.scale_factor.max(1) as u32).unwrap_or(Scale::ONE);
    Geometry {
        scale: estimate_scale(info).unwrap_or(integer),
        logical_size: info.logical_size,
        position: info.logical_position,
    }
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
        // Resolved with the next paint (see `update_geometry`).
        self.mark(id);
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
        surface: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
    ) {
        // A `screens: focused` surface learns where the compositor put it.
        let Some(id) = self.surface_for(surface) else {
            return;
        };
        let Some(global) = self.output_globals.get(&output.id()).copied() else {
            return;
        };
        let Some(monitor) = self
            .monitors
            .id_of(global)
            .and_then(|m| self.monitors.get(m))
            .cloned()
        else {
            return;
        };
        let Some(s) = self.surfaces.get_mut(&id) else {
            return;
        };
        if s.placement != Placement::Focused || s.monitor.as_ref() == Some(&monitor.id) {
            return;
        }
        s.monitor = Some(monitor.id.clone());
        s.output = Some(global);
        let (node, config) = (s.node, s.config.clone());
        // Its catcher went where the compositor put it too: the other
        // outputs get theirs now.
        if let Some(list) = self.catchers.get_mut(&id)
            && list.len() == 1
            && list[0].output.is_none()
        {
            list[0].output = Some(global);
            self.add_secondary_catchers(id, node, &config, global);
        }
        self.host.surface_entered(id, &monitor);
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
        } else if let Some(monitor) = self.monitors.set_geometry(global, output_geometry(&info)) {
            self.host.monitor_changed(&monitor);
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
        if let Some(id) = self.catcher_for(layer.wl_surface()) {
            self.catcher_closed(id, layer.wl_surface());
            return;
        }
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
        if let Some(id) = self.catcher_for(layer.wl_surface()) {
            self.configure_catcher(id, layer.wl_surface(), configure.new_size);
            return;
        }
        let Some(id) = self.surface_for(layer.wl_surface()) else {
            return;
        };
        let Some(s) = self.surfaces.get(&id) else {
            return;
        };
        let (w, h) = configure.new_size;
        // 0 means "your choice": what we asked for.
        let w = if w == 0 { s.config.width.max(1) } else { w };
        let h = if h == 0 { s.config.height.max(1) } else { h };
        self.configured(id, (w, h));
    }
}

impl<H: SurfaceHost + 'static> PopupHandler for State<H> {
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        popup: &Popup,
        config: PopupConfigure,
    ) {
        let Some(id) = self.surface_for(popup.wl_surface()) else {
            return;
        };
        let Some(Role::Popup { config: c, .. }) = self.surfaces.get(&id).map(|s| &s.role) else {
            return;
        };
        // The compositor sizes the box (the window geometry); the buffer
        // adds the shadow overhang.
        let [t, r, b, l] = c.overhang;
        let w = if config.width > 0 {
            config.width as u32
        } else {
            c.width
        };
        let h = if config.height > 0 {
            config.height as u32
        } else {
            c.height
        };
        let w = w.saturating_add((l + r).max(0) as u32);
        let h = h.saturating_add((t + b).max(0) as u32);
        self.configured(id, (w, h));
        // sctk acked it already; the next commit makes it take effect.
        if let Some(s) = self.surfaces.get_mut(&id) {
            s.geometry_dirty = true;
        }
    }

    fn done(&mut self, _: &Connection, _: &QueueHandle<Self>, popup: &Popup) {
        // Escape or a click away (the grab ended): its `open` goes false
        // through the router, and the surface goes now (the compositor
        // unmapped it already).
        let Some(id) = self.surface_for(popup.wl_surface()) else {
            return;
        };
        self.dismiss_popup(id);
    }
}

impl<H: SurfaceHost + 'static> State<H> {
    /// Ends popup `id` as a click away does: told to the host while the
    /// surface is still known (it routes by surface), then destroyed with
    /// the popups nested in it; not made again until its spec has closed.
    fn dismiss_popup(&mut self, id: SurfaceId) {
        self.stats.closed += 1;
        self.send_input(InputEvent::ClickAway { surface: id });
        if let Some(node) = self.surfaces.get(&id).map(|s| s.node) {
            self.dismissed.insert(node);
        }
        self.destroy_surface(id);
    }

    /// Before a grabbing popup opens in `parent`: the grabbing popups
    /// that are not `parent` or one of its ancestors are dismissed first.
    /// xdg-shell wants a grabbing popup to be the topmost one, nested in
    /// the topmost grabbing popup or in a toplevel/layer surface; opening
    /// the volume menu while the calendar is shown closes the calendar,
    /// as a click away would.
    fn dismiss_other_grabs(&mut self, parent: SurfaceId) {
        let mut chain = BTreeSet::new();
        let mut at = Some(parent);
        while let Some(id) = at {
            if !chain.insert(id) {
                break;
            }
            at = self.surfaces.get(&id).and_then(|s| s.role.popup_parent());
        }
        // Outermost first: dismissing one takes the popups nested in it.
        let others: Vec<SurfaceId> = self
            .surfaces
            .values()
            .filter(|s| !chain.contains(&s.id))
            .filter(|s| s.role.grabbed())
            .filter(|s| {
                s.role
                    .popup_parent()
                    .and_then(|p| self.surfaces.get(&p))
                    .is_none_or(|p| !p.role.grabbed())
            })
            .map(|s| s.id)
            .collect();
        for id in others {
            if self.surfaces.contains_key(&id) {
                self.dismiss_popup(id);
            }
        }
    }
}

impl<H: SurfaceHost + 'static> State<H> {
    /// A layer surface or popup was configured at `(w, h)` logical pixels
    /// (its whole buffer).
    fn configured(&mut self, id: SurfaceId, (w, h): (u32, u32)) {
        self.stats.configures += 1;
        let Some(s) = self.surfaces.get_mut(&id) else {
            return;
        };
        s.stats.configures += 1;
        if s.logical != (w, h) {
            s.geometry_dirty = true;
        }
        s.logical = (w, h);
        let region = s.config.input_region((w, h));
        if region != s.input_region {
            s.input_region = region;
            let wl = s.wl().clone();
            let ns = s.config.namespace.clone();
            match region {
                None => wl.set_input_region(None),
                Some(rect) => match Region::new(&self.compositor) {
                    Ok(r) => {
                        if let Some((x, y, w, h)) = rect {
                            r.add(x, y, w, h);
                        }
                        wl.set_input_region(Some(r.wl_region()));
                    }
                    Err(e) => log::warn!("{ns}: no input region: {e}"),
                },
            }
        }
        let first = !s.configured;
        s.configured = true;
        s.ack_pending = true;
        if first {
            s.repaint = true;
        }
        // The size it got may not be the one it asked for.
        self.update_catcher(id);
        // Size and scale are resolved once, right before the next paint.
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
        if capability == Capability::Pointer && !self.pointers.iter().any(|p| p.seat == seat) {
            // A themed pointer sets the cursor through wp_cursor_shape_v1
            // when the compositor has it, else from the cursor theme.
            let cursor_surface = self.compositor.create_surface(qh);
            match self.seat_state.get_pointer_with_theme::<_, ()>(
                qh,
                &seat,
                self.shm.wl_shm(),
                cursor_surface,
                ThemeSpec::default(),
            ) {
                Ok(pointer) => self.pointers.push(SeatPointer {
                    seat: seat.clone(),
                    pointer,
                    button_serial: None,
                    enter_serial: None,
                }),
                Err(e) => log::warn!("cannot get the pointer: {e}"),
            }
        }
        if capability == Capability::Keyboard && !self.keyboards.iter().any(|(s, _)| *s == seat) {
            match self.seat_state.get_keyboard(qh, &seat, None) {
                Ok(k) => self.keyboards.push((seat, k)),
                Err(e) => log::warn!("cannot get the keyboard: {e}"),
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
            // Dropping a themed pointer releases it.
            self.pointers.retain(|p| p.seat != seat);
        }
        if capability == Capability::Keyboard {
            let gone: Vec<ObjectId> = self
                .keyboards
                .iter()
                .filter(|(s, _)| *s == seat)
                .map(|(_, k)| k.id())
                .collect();
            if self
                .key_repeat
                .as_ref()
                .is_some_and(|(k, _, _)| gone.contains(k))
            {
                self.stop_repeat();
            }
            for k in &gone {
                self.repeat_info.remove(k);
                self.raw_modifiers.remove(k);
            }
            self.keyboards.retain(|(s, k)| {
                let keep = *s != seat;
                if !keep && k.version() >= 3 {
                    k.release();
                }
                keep
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
        conn: &Connection,
        _: &QueueHandle<Self>,
        pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        let seat = self
            .pointers
            .iter()
            .position(|p| p.pointer.pointer() == pointer);
        for e in events {
            if let Some(surface) = self.catcher_for(&e.surface) {
                // Only a press means anything on a catcher: a click
                // outside the surface it serves.
                match &e.kind {
                    PointerEventKind::Enter { .. } => {
                        if let Some(p) = seat.and_then(|i| self.pointers.get_mut(i)) {
                            let _ = p.pointer.set_cursor(conn, CursorIcon::Default);
                        }
                    }
                    PointerEventKind::Press { serial, .. } => {
                        if let Some(p) = seat.and_then(|i| self.pointers.get_mut(i)) {
                            p.button_serial = Some(*serial);
                            self.last_action = Some(UserAction {
                                seat: p.seat.clone(),
                                serial: *serial,
                                at: Instant::now(),
                            });
                        }
                        self.send_input(InputEvent::ClickAway { surface });
                    }
                    _ => {}
                }
                continue;
            }
            let Some(surface) = self.surface_for(&e.surface) else {
                continue;
            };
            let position = LogicalPoint::new(e.position.0 as f32, e.position.1 as f32);
            let event = match &e.kind {
                PointerEventKind::Enter { serial } => {
                    if let Some(p) = seat.and_then(|i| self.pointers.get_mut(i)) {
                        p.enter_serial = Some(*serial);
                        match p.pointer.set_cursor(conn, CursorIcon::Default) {
                            Ok(()) => {
                                self.stats.cursor_sets += 1;
                                if let Some(s) = self.surfaces.get_mut(&surface) {
                                    s.stats.cursor_sets += 1;
                                }
                            }
                            Err(e) => log::debug!("cannot set the cursor: {e}"),
                        }
                    }
                    InputEvent::PointerEnter { surface, position }
                }
                PointerEventKind::Leave { .. } => InputEvent::PointerLeave { surface },
                PointerEventKind::Motion { time } => InputEvent::PointerMotion {
                    surface,
                    position,
                    time: *time,
                },
                PointerEventKind::Press {
                    time,
                    button,
                    serial,
                } => {
                    if let Some(p) = seat.and_then(|i| self.pointers.get_mut(i)) {
                        p.button_serial = Some(*serial);
                        self.last_action = Some(UserAction {
                            seat: p.seat.clone(),
                            serial: *serial,
                            at: Instant::now(),
                        });
                    }
                    self.last_pressed = Some(surface);
                    InputEvent::PointerButton {
                        surface,
                        position,
                        button: *button,
                        state: ButtonState::Pressed,
                        time: *time,
                    }
                }
                PointerEventKind::Release { time, button, .. } => InputEvent::PointerButton {
                    surface,
                    position,
                    button: *button,
                    state: ButtonState::Released,
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

impl<H: SurfaceHost + 'static> KeyboardHandler for State<H> {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        if let Some(id) = self.surface_for(surface) {
            self.keyboard_focus = Some(id);
            self.send_input(InputEvent::KeyboardEnter { surface: id });
            self.sync_popup_keyboard();
        }
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        self.stop_repeat();
        let id = self.surface_for(surface).or(self.keyboard_focus);
        if self.keyboard_focus == id {
            self.keyboard_focus = None;
        }
        // The keyboard left the layer surface: its grabbing popup loses it
        // too.
        if let Some(p) = self.grab_focus.take()
            && self.surfaces.contains_key(&p)
        {
            self.send_input(InputEvent::KeyboardLeave { surface: p });
        }
        if let Some(id) = id {
            self.send_input(InputEvent::KeyboardLeave { surface: id });
        }
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        keyboard: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        if let Some((seat, _)) = self.keyboards.iter().find(|(_, k)| k == keyboard) {
            self.last_action = Some(UserAction {
                seat: seat.clone(),
                serial,
                at: Instant::now(),
            });
        }
        self.key(event.clone(), ButtonState::Pressed, false);
        self.start_repeat(keyboard, event);
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.key(event, ButtonState::Pressed, true);
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        keyboard: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if self
            .key_repeat
            .as_ref()
            .is_some_and(|(k, raw, _)| *k == keyboard.id() && *raw == event.raw_code)
        {
            self.stop_repeat();
        }
        self.key(event, ButtonState::Released, false);
    }

    fn update_repeat_info(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        keyboard: &wl_keyboard::WlKeyboard,
        info: RepeatInfo,
    ) {
        self.repeat_info.insert(keyboard.id(), info);
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        keyboard: &wl_keyboard::WlKeyboard,
        _: u32,
        m: XkbModifiers,
        raw: RawModifiers,
        layout: u32,
    ) {
        let raw = (raw.depressed, raw.latched, raw.locked, layout);
        let id = keyboard.id();
        let changed = self.raw_modifiers.insert(id.clone(), raw) != Some(raw);
        if changed && self.key_repeat.as_ref().is_some_and(|(k, _, _)| *k == id) {
            // The repeating key's text was computed under its keyboard's
            // old modifiers; repeating it under the new ones would send
            // e.g. "a" with Shift held. Another seat's keyboard leaves it.
            self.stop_repeat();
        }
        self.modifiers = Modifiers {
            ctrl: m.ctrl,
            alt: m.alt,
            shift: m.shift,
            logo: m.logo,
        };
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

impl<H: SurfaceHost + 'static> Dispatch2<wl_buffer::WlBuffer, State<H>> for CatcherBuffer {
    fn event(
        &self,
        _: &mut State<H>,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
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
            if let Some(s) = state.surfaces.get_mut(&self.0)
                && s.scale != scale
            {
                s.scale = scale;
                state.mark(self.0);
            }
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
                if state.frame_settled(self) {
                    if let Some(s) = state.surfaces.get_mut(&self.surface) {
                        s.stats.presented += 1;
                    }
                    state.clock.presented(self.surface, presentation);
                }
            }
            wp_presentation_feedback::Event::Discarded => {
                state.stats.discarded += 1;
                if state.frame_settled(self) {
                    if let Some(s) = state.surfaces.get_mut(&self.surface) {
                        s.stats.discarded += 1;
                    }
                    state.clock.discarded(self.surface);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_repeat_interval_never_busy_loops() {
        assert_eq!(repeat_interval(25), Duration::from_millis(40));
        assert_eq!(repeat_interval(1_000), Duration::from_millis(1));
        assert_eq!(repeat_interval(2_000_000), Duration::from_millis(1));
        assert_eq!(repeat_interval(u32::MAX), Duration::from_millis(1));
        assert_eq!(repeat_interval(0), Duration::from_secs(1));
    }
}
