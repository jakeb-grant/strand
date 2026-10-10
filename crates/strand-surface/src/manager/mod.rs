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
use wayland_protocols::ext::background_effect::v1::client::{
    ext_background_effect_manager_v1::{self, ExtBackgroundEffectManagerV1},
    ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1,
};
use wayland_protocols::wp::alpha_modifier::v1::client::wp_alpha_modifier_v1::WpAlphaModifierV1;
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::presentation_time::client::{
    wp_presentation::{self, WpPresentation},
    wp_presentation_feedback::{self, WpPresentationFeedback},
};
use wayland_protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1;
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::{self, WpViewport},
    wp_viewporter::{self, WpViewporter},
};
use wayland_protocols::xdg::shell::client::{xdg_positioner, xdg_wm_base::XdgWmBase};

use strand_scene::{
    CompositorCaps, DropPayload, Keyboard, Layer, LogicalPoint, LogicalSize, NodeId, NodeKind,
    PaintTarget, Painter, Rect, Scale, Screens, Size, SurfaceChange, SurfaceId, SurfaceSpec,
};

use crate::caps::Offered;
use crate::clock::{FrameClock, Presentation, PresentationClock};
use crate::input::{AxisDelta, AxisSource, ButtonState, InputEvent};
use crate::monitor::{Geometry, Monitor, MonitorId, Monitors};
use crate::placement::{
    LayerConfig, PlacementError, PopupConfig, PopupSide, layer_config_with, popup_config,
    popup_scrim_layer,
};
use crate::shm::{BufferData, MAX_BUFFERS, ShmBuffers};
use strand_scene::{KeyInput, Modifiers};

mod catcher;
mod commit;
// (M4) Drag and drop is the lists stream's file, `src/dnd.rs`, but part
// of the manager: it reaches `State` as the other parts here do.
#[path = "../dnd.rs"]
mod dnd;
mod effect;
#[path = "../gpu_handoff.rs"]
mod gpu_handoff;
mod layer;
mod origin;
mod outputs;
mod popup;
mod pose;
mod protocols;
mod scrim;
mod seat;
mod session_lock;
#[cfg(feature = "gpu")]
pub use gpu_handoff::RawHandles;

use catcher::{Catcher, Under, wants_scrim, wants_under};
use layer::to_sctk_layer;
pub use session_lock::{LOCK_FALLBACK_NODE, LockError};

/// How long an unmapped surface whose paint drew nothing, while its
/// painter still wants a frame, waits before it is painted again (no frame
/// callbacks come before the first buffer): about one 60 Hz frame.
const UNMAPPED_RETRY: Duration = Duration::from_millis(16);

/// (m4-audit) How long a surface waits for its last frame's callback or
/// presentation before it paints anyway: a compositor can lose them (a
/// bar's first frame on an output being re-enabled), and with nothing
/// else to end the wait the surface would never paint again. Far longer
/// than any refresh; a surface the compositor does not show (occluded,
/// DPMS off) paints new content at most this often.
const THROTTLE_GIVE_UP: Duration = Duration::from_secs(1);

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
    /// (M4) Where `surface`'s buffer (its top-left corner, shadow
    /// overhang included) now lies in the compositor's logical layout
    /// (global logical pixels: its output's position added): a layer
    /// surface as the compositor arranges one of its size and anchors in
    /// its output's usable area (our own surfaces' exclusive zones taken
    /// out), a popup where the compositor's configure put it relative to
    /// its parent. Called when it changes. The host turns a press into a
    /// screen position with it (the tray's click point).
    fn surface_placed(&mut self, surface: SurfaceId, origin: (i32, i32)) {
        let _ = (surface, origin);
    }

    /// (M4) The optional protocols the compositor offered, once the
    /// manager has bound its globals; the host hands render what it uses
    /// (`set_compositor_blur`, `set_compositor_poses`). The manager calls
    /// it once S-surface binds the M4 globals.
    fn compositor_caps(&mut self, caps: &CompositorCaps) {
        let _ = caps;
    }
    /// (M4) The manager must destroy or recreate `surface` while the GPU
    /// thread presents to it: the host sends `GpuRequest::Release` and,
    /// on the reply, calls `State::take_back`, which destroys it then
    /// (docs/architecture.md, "`strand-gpu`", "Surface hand-off").
    fn gpu_release(&mut self, surface: SurfaceId) {
        let _ = surface;
    }
    /// (M4) The session lock changed (`ext_session_lock_v1`).
    fn lock_changed(&mut self, state: LockState) {
        let _ = state;
    }
    /// (M4) Whether a drop on `surface` now, at the drag's last position,
    /// would land on something whose `on drop` takes it (the Router's
    /// `drop_target`). The manager asks after every `Drag*` event it sent
    /// and accepts the `wl_data_device` offer only while it holds.
    fn drop_accepted(&self, surface: SurfaceId) -> bool {
        let _ = surface;
        false
    }
    /// (M4) The `drag:` node being dragged on `surface`, if a drag is in
    /// flight there (the Router's `drag`): when the held pointer leaves
    /// the surface, the manager hands that drag to the compositor
    /// (`wl_data_device.start_drag`), so it can drop on another surface.
    fn drag_source(&self, surface: SurfaceId) -> Option<NodeId> {
        let _ = surface;
        None
    }
    /// (M4 interaction-finish) What the `drag:` node `node` gives other
    /// programs when the compositor carries it out
    /// (`strand_scene::drag_export` of its `Prop::Drag`): the drag then
    /// offers it as `text/uri-list` and text besides its private type.
    /// `None`: nothing, the drag is Strand's alone.
    fn drag_data(&self, node: NodeId) -> Option<DropPayload> {
        let _ = node;
        None
    }
    /// (M4 interaction-finish) The icon of the `drag:` node `node` on
    /// `surface` (render's `Renderer::drag_image`): the manager shows it
    /// under the pointer, where it was held, while the compositor
    /// carries the drag. `None`: no icon.
    fn drag_image(&mut self, surface: SurfaceId, node: NodeId) -> Option<strand_scene::DragImage> {
        let _ = (surface, node);
        None
    }
}

/// (M4) The session lock as the compositor reports it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum LockState {
    /// `locked`: every output shows a lock surface.
    Locked,
    /// `finished`: the compositor refused or ended the lock, or offers no
    /// `ext-session-lock` at all. Without a `locked` before it, the lock
    /// was never shown.
    Finished,
    /// The lock was released with an `UnlockToken`.
    Unlocked,
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
    /// (m4-audit) Paints made although the last frame's callback or
    /// presentation never came ([`THROTTLE_GIVE_UP`]).
    pub throttle_given_up: u64,
    /// Cursor images set on pointer enter (`wp_cursor_shape_v1` or the
    /// cursor theme).
    pub cursor_sets: u64,
    /// Popups created with an `xdg_popup.grab` (opened within
    /// [`GRAB_WINDOW`] of a press).
    pub grabs: u64,
    /// `ext_background_effect_surface_v1.set_blur_region` requests (sent
    /// only when the region changes).
    pub blur_updates: u64,
    /// (M4) Compositor poses set (`Painter::surface_pose` changed): each
    /// rides that frame's commit, or a bare commit when nothing was
    /// drawn.
    pub poses: u64,
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
    /// The blur region last sent (`ext-background-effect-v1`), in
    /// surface-local logical pixels: `None` until one was sent.
    pub blur_region: Option<Vec<crate::blur::BlurRect>>,
    /// Input passes through (an `osd`: empty input region).
    pub click_through: bool,
    /// The input region: `None` the whole surface, `Some(None)` empty,
    /// else the box `(x, y, w, h)` inside the shadow overhang.
    pub input_region: Option<Option<(i32, i32, i32, i32)>>,
    /// A click-away catcher is mapped under it.
    pub click_away: bool,
    /// A scrim in this colour is mapped under it (on its root layer
    /// surface's output, for a popup).
    pub scrim: Option<strand_scene::Color>,
    /// The layer it is on (`None` for a popup).
    pub layer: Option<Layer>,
    /// The layer its click-away catcher or scrim is on.
    pub under_layer: Option<Layer>,
    /// (M4) The compositor pose last set on it (identity at rest).
    pub pose: strand_scene::SurfacePose,
    /// (M4) Where its buffer's top-left corner is in the compositor's
    /// logical layout, as last told to [`SurfaceHost::surface_placed`].
    pub origin: Option<(i32, i32)>,
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
    /// (m4-audit) When the commit that set `callback_pending` or
    /// `in_flight` was made ([`THROTTLE_GIVE_UP`]).
    throttled_at: Option<Instant>,
    /// A configure was acked and no commit has followed yet.
    ack_pending: bool,
    /// A paint is owed: first configure, resize, rescale or a request.
    repaint: bool,
    /// Last opaque region sent, in logical pixels.
    opaque: Vec<Rect>,
    /// Its `ext_background_effect_surface_v1`, made with the first blur
    /// region it sends, and that region's rectangles as last sent
    /// (`None`: unknown, sent again with the next frame).
    blur: Option<ExtBackgroundEffectSurfaceV1>,
    blur_sent: Option<Vec<crate::blur::BlurRect>>,
    /// (M4) The compositor pose set on it (`pose.rs`), and its
    /// `wp_alpha_modifier_surface_v1`, made with the first opacity.
    pose: strand_scene::SurfacePose,
    alpha: Option<wayland_protocols::wp::alpha_modifier::v1::client::wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1>,
    /// (M4) Its buffer's top-left corner on its output (`origin.rs`).
    origin: Option<(i32, i32)>,
    /// (M4) The monitor whose usable area its exclusive zone was last
    /// taken from when it was placed (`origin.rs`): a zone that drops to
    /// none, or a surface that moves, places the others there again.
    zone_on: Option<MonitorId>,
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
    /// (M4) A session lock surface showing the lock's content
    /// (`session_lock.rs`).
    Lock(session_lock::LockSurface),
}

impl Role {
    fn wl(&self) -> &wl_surface::WlSurface {
        match self {
            Role::Layer(l) => l.wl_surface(),
            Role::Popup { popup, .. } => popup.wl_surface(),
            Role::Lock(l) => l.wl(),
        }
    }

    fn popup_parent(&self) -> Option<SurfaceId> {
        match self {
            Role::Popup { parent, .. } => Some(*parent),
            Role::Layer(_) | Role::Lock(_) => None,
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
            blur_region: self.blur.as_ref().and(self.blur_sent.clone()),
            click_through: self.click_through,
            input_region: self.input_region,
            click_away: false,
            scrim: None,
            layer: matches!(self.role, Role::Layer(_)).then_some(self.config.layer),
            under_layer: None,
            pose: self.pose,
            origin: self.origin,
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
    /// (M4) Optional protocols: the alpha modifier (poses), single-pixel
    /// buffers (scrims) and the background effect (the blur ladder).
    alpha_modifier: Option<WpAlphaModifierV1>,
    single_pixel: Option<WpSinglePixelBufferManagerV1>,
    background_effect: Option<ExtBackgroundEffectManagerV1>,
    /// What the compositor offers, and the capabilities last reported
    /// through [`SurfaceHost::compositor_caps`] (`None`: not yet).
    offered: Offered,
    reported_caps: Option<CompositorCaps>,
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
    /// Layer surfaces that gave a grab's `exclusive` back for `none`
    /// while they had keyboard focus: the compositor's leave for that may
    /// come only with its next keyboard change (sway sends it with the
    /// enter of the next grab), and must not close a popup that grabbed
    /// since.
    releasing: BTreeSet<SurfaceId>,
    /// A leave that came for a `releasing` layer surface while a new
    /// grab of its held the keyboard: stale if an enter for it follows in
    /// the same dispatch (sway), else a real focus loss, told to the
    /// grabbing popup once the dispatch ends ([`State::resolve_held_leave`]).
    held_leave: Option<SurfaceId>,
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
    /// (m4-audit) Surfaces whose frame callback or presentation is
    /// awaited with new content waiting: marked again at
    /// [`THROTTLE_GIVE_UP`] unless the frame settles first.
    give_up_timers: HashMap<SurfaceId, RegistrationToken>,
    /// (M4) The session lock (`session_lock.rs`).
    session_lock: session_lock::SessionLock,
    /// (M4) Surfaces the GPU thread commits (`gpu_handoff.rs`).
    gpu: gpu_handoff::HandOffs,
    /// (M4) Drag and drop over `wl_data_device` (`src/dnd.rs`).
    dnd: dnd::Dnd,
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
        let session_lock = globals
            .bind::<wayland_protocols::ext::session_lock::v1::client::ext_session_lock_manager_v1::ExtSessionLockManagerV1, _, _>(
                &qh,
                1..=1,
                StrandGlobal,
            )
            .ok();
        let alpha_modifier = globals
            .bind::<WpAlphaModifierV1, _, _>(&qh, 1..=1, StrandGlobal)
            .ok();
        let single_pixel = globals
            .bind::<WpSinglePixelBufferManagerV1, _, _>(&qh, 1..=1, StrandGlobal)
            .ok();
        let background_effect = globals
            .bind::<ExtBackgroundEffectManagerV1, _, _>(&qh, 1..=1, StrandGlobal)
            .ok();
        let mut offered = globals
            .contents()
            .with_list(|list| Offered::from_registry(list.iter().map(|g| g.interface.as_str())));
        offered.alpha_modifier = alpha_modifier.is_some();
        offered.viewporter = viewporter.is_some();
        offered.single_pixel_buffer = single_pixel.is_some();
        offered.background_effect = background_effect.is_some();

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

        // The capabilities go to the host once the first wakeup has read
        // the replies to these binds (the background effect's
        // `capabilities` among them), before any surface is configured.
        handle.insert_idle(|state: &mut State<H>| state.report_caps());
        let dnd = dnd::Dnd::bind(&globals, &qh);
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
            alpha_modifier,
            single_pixel,
            background_effect,
            offered,
            reported_caps: None,
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
            releasing: BTreeSet::new(),
            held_leave: None,
            grab_focus: None,
            dismissed: BTreeSet::new(),
            modifiers: Modifiers::default(),
            focused: None,
            input: None,
            stats: Stats::default(),
            expiry_timer: None,
            deadline_timers: HashMap::new(),
            give_up_timers: HashMap::new(),
            session_lock: session_lock::SessionLock::new(session_lock),
            gpu: gpu_handoff::HandOffs::default(),
            dnd,
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

    /// (m4-audit) Whether a give-up timer is armed for `id`: a paint
    /// was refused while its frame was in flight (tests).
    #[doc(hidden)]
    pub fn give_up_armed(&self, id: SurfaceId) -> bool {
        self.give_up_timers.contains_key(&id)
    }

    pub fn surface(&self, id: SurfaceId) -> Option<SurfaceInfo> {
        let mut info = self.surfaces.get(&id).map(Surface::info)?;
        let primary = self
            .catchers
            .get(&id)
            .and_then(|v| v.iter().find(|c| c.primary && c.buffer.is_some()));
        info.click_away = primary.is_some_and(|c| c.clicks);
        info.scrim = primary.and_then(|c| c.scrim);
        info.under_layer = primary.map(|c| c.on_layer);
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

    /// (M4) The optional protocols the compositor offers, as reported to
    /// the host ([`SurfaceHost::compositor_caps`]).
    pub fn compositor_caps(&self) -> CompositorCaps {
        self.offered.caps()
    }

    /// Tells the host the compositor's capabilities when they are new or
    /// changed (the background effect's flags can change at any time).
    fn report_caps(&mut self) {
        let caps = self.offered.caps();
        if self.reported_caps == Some(caps) {
            return;
        }
        let blur_changed = self
            .reported_caps
            .is_some_and(|old| old.background_effect != caps.background_effect);
        self.reported_caps = Some(caps);
        log::debug!("compositor capabilities: {caps:?}");
        self.host.compositor_caps(&caps);
        if blur_changed {
            self.blur_capability_changed();
        }
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
        // A `lock` is never a layer surface and never unlocks by a spec
        // change (`session_lock.rs`).
        let Some(change) = self.lock_spec_change(node, change) else {
            return;
        };
        // A popup's scrim coming or going can move the layer surface it
        // is nested in (`placement::layer_config_with`): that one is
        // made again afterwards, and the popup nests in it once it maps.
        let old = self.specs.get(&node);
        let had = old.is_some_and(nested_scrim_of);
        let (has, parent) = match &change {
            SurfaceChange::Created(spec) | SurfaceChange::Updated { spec, .. } => {
                (nested_scrim_of(spec), spec.parent)
            }
            SurfaceChange::Removed => (false, old.and_then(|s| s.parent)),
        };
        let moved = (had != has)
            .then(|| parent.and_then(|p| self.root_node(p)))
            .flatten();
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
        if let Some(root) = moved {
            self.reconfigure(root);
            self.reconcile(root);
        }
    }

    /// The layer-surface node `node` is nested in (itself for one):
    /// `None` when its chain of parents is broken.
    fn root_node(&self, mut node: NodeId) -> Option<NodeId> {
        for _ in 0..64 {
            let spec = self.specs.get(&node)?;
            if spec.kind != NodeKind::Popup {
                return Some(node);
            }
            node = spec.parent?;
        }
        None
    }

    /// True if a popup nested in layer-surface node `node` declares a
    /// scrim (open or not, so opening it does not move `node`).
    fn has_nested_scrim(&self, node: NodeId) -> bool {
        self.specs
            .values()
            .any(|s| nested_scrim_of(s) && s.parent.and_then(|p| self.root_node(p)) == Some(node))
    }

    /// The layer-surface state for `node` with `spec`, raised for a
    /// popup's scrim nested in it.
    fn layer_config_of(
        &self,
        node: NodeId,
        spec: &SurfaceSpec,
    ) -> Result<LayerConfig, PlacementError> {
        layer_config_with(spec, self.has_nested_scrim(node))
    }

    /// The layer that layer surface `id` is on (`None` for a popup).
    fn layer_of(&self, id: SurfaceId) -> Option<Layer> {
        let s = self.surfaces.get(&id)?;
        matches!(s.role, Role::Layer(_)).then_some(s.config.layer)
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
        // The lock covers every output, with or without its spec.
        self.sync_lock_surfaces();
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
        if spec.kind == NodeKind::Lock {
            self.reconcile_lock();
            return;
        }
        let mapped = match self.layer_config_of(node, &spec) {
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
}

fn clamp_i32(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

/// A popup that declares a scrim.
fn nested_scrim_of(spec: &SurfaceSpec) -> bool {
    spec.kind == NodeKind::Popup && spec.scrim.is_some()
}

#[cfg(test)]
mod hook_tests {
    use super::*;
    use strand_scene::Damage;

    /// A host that implements only the painter.
    struct Bare;

    impl Painter for Bare {
        fn paint(&mut self, _: SurfaceId, _: &mut PaintTarget<'_>) -> Damage {
            Damage::new()
        }
        fn wants_frame(&self, _: SurfaceId) -> bool {
            false
        }
    }

    impl SurfaceHost for Bare {}

    #[test]
    fn the_m4_hooks_default_to_nothing() {
        let mut host = Bare;
        host.compositor_caps(&CompositorCaps {
            alpha_modifier: true,
            ..CompositorCaps::default()
        });
        host.gpu_release(SurfaceId(1));
        for state in [LockState::Locked, LockState::Finished, LockState::Unlocked] {
            host.lock_changed(state);
        }
        assert_eq!(host.surface_pose(SurfaceId(1)), None);
    }
}
