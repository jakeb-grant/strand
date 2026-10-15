//! `outputs`: the monitors' configuration, from the compositor's
//! `zwlr_output_manager_v1` (wlr-output-management, which sway, Hyprland,
//! niri and labwc implement; owner-testing gap 18, decisions.md
//! m6-owner-fixes).
//!
//! `all` lists every head the manager announces, on or off, published at
//! each manager `done` (the protocol's atomic update): its name,
//! description, make, model and serial, its modes (deduplicated, in the
//! compositor's order), the current and preferred mode, its place, scale,
//! transform and adaptive sync. `available` is the manager being offered.
//!
//! `apply(changes)` and `test(changes)` each make one
//! `zwlr_output_configuration_v1` against the last `done`'s serial. The
//! protocol needs every head in it, so each head not named, and each
//! property a change leaves null, is sent as the head has it: an enabled
//! head with its current mode, place, scale, transform (and adaptive sync,
//! version 4), a disabled one disabled. A change is checked before it is
//! sent, since what the protocol calls an error would end the connection:
//! an unknown or repeated name, a scale `wl_fixed` cannot carry (below
//! 1/256, which it sends as 0), a mode or
//! place out of range are refused here. A change's mode is the head's
//! listed mode of that size nearest its rate within 0.5 Hz (the fastest
//! listed one for a rate of 0), else a custom mode; a head turned on with
//! no mode named shows its current mode, else its preferred, else its
//! first. The compositor answers `succeeded` (`outputs.succeeded(action)`)
//! or `failed` (`outputs.failed(action, error)`); `cancelled` (the outputs
//! changed since the serial) makes the configuration again from the new
//! state, at most [`MAX_TRIES`] times. One configuration is in flight at a
//! time; later ones wait their turn, in order. Every refusal is
//! `failed(action, error)`, as the action reaching no run is
//! (`Service::refused`, spec.md 12).
//!
//! The service runs on a thread of its own (`strand-outputs`) with its own
//! Wayland connection, asleep in `poll(2)` on its socket and an eventfd
//! the service's messages poke, so an idle compositor costs no wakeup. It
//! connects to `$WAYLAND_DISPLAY` ([`configure`] points it elsewhere:
//! tests). A lost connection empties `all`, clears `available` and ends
//! the run with an error, which the contract retries with its backoff.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustix::event::{EventfdFlags, PollFd, PollFlags};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::{wl_callback, wl_output, wl_registry};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, event_created_child,
};
use wayland_protocols_wlr::output_management::v1::client::{
    zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};

use crate::wm::WaylandTarget;
use crate::{Call, Cx, Event, Msg, ServiceError, Store, service};

/// The schema the `outputs` service serves.
pub const SCHEMA: &str = strand_services_schema::OUTPUTS;

/// How many times a configuration the compositor cancels (the outputs
/// changed under it) is made again before it fails.
pub const MAX_TRIES: u32 = 3;

/// How long a stopping run waits for the answer to a configuration it
/// sent, so the compositor reads it before the connection closes.
const STOP_WAIT: Duration = Duration::from_millis(500);

/// The highest manager version this client speaks (4: adaptive sync).
const VERSION: u32 = 4;

/// The smallest scale the protocol carries (`wl_fixed` steps by 1/256).
const MIN_SCALE: f64 = 1.0 / 256.0;

/// The largest scale the protocol carries (`wl_fixed`'s largest).
const MAX_SCALE: f64 = 8_388_607.0;

/// How far a change's refresh rate may be from a listed mode's, in mHz.
const RATE_SLACK: i64 = 500;

static TARGET: Mutex<Option<WaylandTarget>> = Mutex::new(None);

/// Which display `outputs` connects to from its next start on (tests: a
/// headless sway's socket). `None` restores `$WAYLAND_DISPLAY`. A running
/// service keeps its connection until it stops.
pub fn configure(target: Option<WaylandTarget>) {
    if let Ok(mut t) = TARGET.lock() {
        *t = target;
    }
}

fn target() -> WaylandTarget {
    TARGET
        .lock()
        .ok()
        .and_then(|t| t.clone())
        .unwrap_or(WaylandTarget::Env)
}

/// A mode: its size in hardware pixels and its refresh rate in Hz (0:
/// none fixed).
#[derive(crate::Data, Clone, Copy, Debug, Default, PartialEq)]
#[data(name = "OutputMode")]
pub struct OutputMode {
    pub width: i64,
    pub height: i64,
    pub refresh: f64,
}

/// An output's rotation and flip, as `wl_output` names them: rotations
/// are counter-clockwise, and a flipped one is flipped about the vertical
/// axis, then rotated.
#[derive(crate::Data, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[data(name = "OutputTransform")]
pub enum OutputTransform {
    #[default]
    Normal,
    #[data(rename = "rotate_90")]
    Rotate90,
    #[data(rename = "rotate_180")]
    Rotate180,
    #[data(rename = "rotate_270")]
    Rotate270,
    Flipped,
    #[data(rename = "flipped_90")]
    Flipped90,
    #[data(rename = "flipped_180")]
    Flipped180,
    #[data(rename = "flipped_270")]
    Flipped270,
}

impl OutputTransform {
    fn from_wl(t: wl_output::Transform) -> OutputTransform {
        use wl_output::Transform as T;
        match t {
            T::_90 => OutputTransform::Rotate90,
            T::_180 => OutputTransform::Rotate180,
            T::_270 => OutputTransform::Rotate270,
            T::Flipped => OutputTransform::Flipped,
            T::Flipped90 => OutputTransform::Flipped90,
            T::Flipped180 => OutputTransform::Flipped180,
            T::Flipped270 => OutputTransform::Flipped270,
            _ => OutputTransform::Normal,
        }
    }

    fn to_wl(self) -> wl_output::Transform {
        use wl_output::Transform as T;
        match self {
            OutputTransform::Normal => T::Normal,
            OutputTransform::Rotate90 => T::_90,
            OutputTransform::Rotate180 => T::_180,
            OutputTransform::Rotate270 => T::_270,
            OutputTransform::Flipped => T::Flipped,
            OutputTransform::Flipped90 => T::Flipped90,
            OutputTransform::Flipped180 => T::Flipped180,
            OutputTransform::Flipped270 => T::Flipped270,
        }
    }
}

/// A head, as `outputs.all` lists it (the schema's `Output`).
#[derive(crate::Data, Clone, Debug, Default, PartialEq)]
#[data(name = "Output", key = name)]
pub struct Output {
    pub name: String,
    pub description: String,
    pub make: String,
    pub model: String,
    pub serial: String,
    pub enabled: bool,
    pub mode: Option<OutputMode>,
    pub preferred: Option<OutputMode>,
    pub modes: Vec<OutputMode>,
    pub x: i64,
    pub y: i64,
    pub scale: f64,
    pub transform: OutputTransform,
    pub adaptive_sync: bool,
    pub physical_width: i64,
    pub physical_height: i64,
}

/// One output's part of a configuration (the schema's `OutputChange`): a
/// `None` keeps what the output has.
#[derive(crate::Data, Clone, Debug, Default, PartialEq)]
#[data(name = "OutputChange")]
pub struct OutputChange {
    pub name: String,
    pub enabled: Option<bool>,
    pub mode: Option<OutputMode>,
    pub x: Option<i64>,
    pub y: Option<i64>,
    pub scale: Option<f64>,
    pub transform: Option<OutputTransform>,
    pub adaptive_sync: Option<bool>,
}

impl OutputChange {
    /// A change of `name` that keeps everything (fill in what changes).
    pub fn of(name: impl Into<String>) -> OutputChange {
        OutputChange {
            name: name.into(),
            ..OutputChange::default()
        }
    }
}

/// `outputs`' actions.
#[derive(Call, Debug, Clone, PartialEq)]
pub enum OutputsAction {
    /// `outputs.apply(changes)`.
    Apply { changes: Vec<OutputChange> },
    /// `outputs.test(changes)`.
    Test { changes: Vec<OutputChange> },
}

impl OutputsAction {
    /// Its name as the schema spells it.
    pub fn name(&self) -> &'static str {
        match self {
            OutputsAction::Apply { .. } => "apply",
            OutputsAction::Test { .. } => "test",
        }
    }

    fn changes(&self) -> &[OutputChange] {
        match self {
            OutputsAction::Apply { changes } | OutputsAction::Test { changes } => changes,
        }
    }
}

/// See the module docs.
#[service(name = "outputs", action = OutputsAction, refused = refused, thread)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Outputs {
    /// Every output, on or off, keyed by `name`.
    #[store(keyed)]
    pub all: Vec<Output>,
    /// The compositor offers output configuration: false without it, when
    /// `apply` and `test` fail with `outputs.failed`.
    pub available: bool,
    /// The compositor took a configuration: `on outputs.succeeded(action)
    /// { … }`, `action` as written (`"apply"` or `"test"`).
    pub succeeded: Event<String>,
    /// A configuration failed: the compositor refused it (a mode the
    /// monitor cannot show), it named no output or an impossible value, or
    /// the compositor offers no output configuration: `on
    /// outputs.failed(action, error) { … }`.
    pub failed: Event<(String, String)>,
}

/// An action the service could not take (its run backing off): `failed`,
/// as the compositor refusing it is.
fn refused(action: &OutputsAction, error: &str) -> Option<OutputsEvent> {
    Some(OutputsEvent::Failed((
        action.name().to_string(),
        error.to_string(),
    )))
}

/// What the compositor said of a mode.
#[derive(Debug)]
struct ModeEntry {
    proxy: ZwlrOutputModeV1,
    width: i32,
    height: i32,
    /// mHz; 0 when not sent.
    refresh: i32,
    preferred: bool,
}

impl ModeEntry {
    fn public(&self) -> OutputMode {
        OutputMode {
            width: i64::from(self.width),
            height: i64::from(self.height),
            refresh: f64::from(self.refresh) / 1000.0,
        }
    }
}

/// What the compositor said of a head.
#[derive(Debug)]
struct HeadEntry {
    proxy: ZwlrOutputHeadV1,
    name: String,
    description: String,
    make: String,
    model: String,
    serial: String,
    physical: (i32, i32),
    enabled: bool,
    /// Its modes, in the order announced (finished ones removed).
    modes: Vec<ObjectId>,
    current_mode: Option<ObjectId>,
    position: (i32, i32),
    transform: wl_output::Transform,
    scale: f64,
    adaptive_sync: bool,
}

impl HeadEntry {
    fn new(proxy: ZwlrOutputHeadV1) -> HeadEntry {
        HeadEntry {
            proxy,
            name: String::new(),
            description: String::new(),
            make: String::new(),
            model: String::new(),
            serial: String::new(),
            physical: (0, 0),
            enabled: false,
            modes: Vec::new(),
            current_mode: None,
            position: (0, 0),
            transform: wl_output::Transform::Normal,
            scale: 1.0,
            adaptive_sync: false,
        }
    }
}

/// A configuration's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Succeeded,
    Failed,
    Cancelled,
}

/// The connection's state (the `Dispatch` target).
#[derive(Debug, Default)]
struct Client {
    manager: Option<ZwlrOutputManagerV1>,
    /// The manager's global name, to see it removed.
    manager_global: Option<u32>,
    /// The last `done`'s serial; `None` before the first.
    serial: Option<u32>,
    heads: Vec<HeadEntry>,
    modes: HashMap<ObjectId, ModeEntry>,
    /// A `done` (or the manager going) is waiting to be published.
    changed: bool,
    /// The registry's first sync is answered: every global then present
    /// was announced.
    synced: bool,
    /// Answers to configurations, by the configuration's own number.
    answers: Vec<(u64, Outcome)>,
}

impl Client {
    fn head_mut(&mut self, id: &ObjectId) -> Option<&mut HeadEntry> {
        self.heads.iter_mut().find(|h| h.proxy.id() == *id)
    }

    /// The outputs as published: every head with a name, modes
    /// deduplicated.
    fn snapshot(&self) -> Vec<Output> {
        self.heads
            .iter()
            .filter(|h| !h.name.is_empty())
            .map(|h| {
                let mut modes: Vec<OutputMode> = Vec::new();
                for m in h.modes.iter().filter_map(|id| self.modes.get(id)) {
                    let m = m.public();
                    if !modes.contains(&m) {
                        modes.push(m);
                    }
                }
                let preferred = h
                    .modes
                    .iter()
                    .filter_map(|id| self.modes.get(id))
                    .find(|m| m.preferred)
                    .map(ModeEntry::public);
                let mode = h
                    .current_mode
                    .as_ref()
                    .filter(|_| h.enabled)
                    .and_then(|id| self.modes.get(id))
                    .map(ModeEntry::public);
                Output {
                    name: h.name.clone(),
                    description: h.description.clone(),
                    make: h.make.clone(),
                    model: h.model.clone(),
                    serial: h.serial.clone(),
                    enabled: h.enabled,
                    mode,
                    preferred,
                    modes,
                    x: i64::from(h.position.0),
                    y: i64::from(h.position.1),
                    scale: h.scale,
                    transform: OutputTransform::from_wl(h.transform),
                    adaptive_sync: h.adaptive_sync,
                    physical_width: i64::from(h.physical.0),
                    physical_height: i64::from(h.physical.1),
                }
            })
            .collect()
    }

    /// The manager is gone (finished, or its global removed): its heads
    /// and modes with it.
    fn drop_manager(&mut self) {
        for h in self.heads.drain(..) {
            if h.proxy.version() >= 3 {
                h.proxy.release();
            }
        }
        for (_, m) in self.modes.drain() {
            if m.proxy.version() >= 3 {
                m.proxy.release();
            }
        }
        self.manager = None;
        self.manager_global = None;
        self.serial = None;
        self.changed = true;
    }
}

/// How a configured head gets its mode.
#[derive(Debug, PartialEq)]
enum ModeChoice {
    /// No mode request (the head lists none and shows none).
    Keep,
    /// One of its listed modes.
    Listed(ObjectId),
    /// A custom mode: width, height, mHz.
    Custom(i32, i32, i32),
}

/// One head's part of a configuration about to be sent.
#[derive(Debug, PartialEq)]
struct HeadPlan {
    head: ObjectId,
    enabled: bool,
    mode: ModeChoice,
    position: (i32, i32),
    transform: wl_output::Transform,
    scale: f64,
    adaptive_sync: bool,
}

/// Checks `changes` against the heads and works out every head's part:
/// why not, when a change names no head or a value the protocol would
/// count as an error.
fn plan(client: &Client, changes: &[OutputChange]) -> Result<Vec<HeadPlan>, String> {
    let mut seen: Vec<&str> = Vec::new();
    for c in changes {
        if seen.contains(&c.name.as_str()) {
            return Err(format!("`{}` is named twice", c.name));
        }
        seen.push(&c.name);
        if !client.heads.iter().any(|h| h.name == c.name) {
            let names: Vec<&str> = client.heads.iter().map(|h| h.name.as_str()).collect();
            return Err(format!(
                "no output named `{}` (there are: {})",
                c.name,
                names.join(", ")
            ));
        }
    }
    client
        .heads
        .iter()
        .filter(|h| !h.name.is_empty())
        .map(|h| {
            let c = changes.iter().find(|c| c.name == h.name);
            let enabled = c.and_then(|c| c.enabled).unwrap_or(h.enabled);
            let field = |what: &str, v: i64| {
                i32::try_from(v).map_err(|_| format!("{}: {what} {v} is out of range", h.name))
            };
            let position = (
                c.and_then(|c| c.x)
                    .map_or(Ok(h.position.0), |x| field("x", x))?,
                c.and_then(|c| c.y)
                    .map_or(Ok(h.position.1), |y| field("y", y))?,
            );
            let scale = match c.and_then(|c| c.scale) {
                // `wl_fixed` steps by 1/256: a smaller scale is sent as 0,
                // which the protocol counts as an error.
                Some(s) if !(MIN_SCALE..=MAX_SCALE).contains(&s) => {
                    return Err(format!(
                        "{}: scale {s} is outside 1/256 to {MAX_SCALE}, what the protocol carries",
                        h.name
                    ));
                }
                Some(s) => s,
                // A head never on may have sent no scale.
                None if h.scale > 0.0 => h.scale,
                None => 1.0,
            };
            let transform = c
                .and_then(|c| c.transform)
                .map_or(h.transform, OutputTransform::to_wl);
            let adaptive_sync = c.and_then(|c| c.adaptive_sync).unwrap_or(h.adaptive_sync);
            let mode = match c.and_then(|c| c.mode) {
                Some(m) => choose_mode(client, h, m)?,
                None => default_mode(client, h),
            };
            Ok(HeadPlan {
                head: h.proxy.id(),
                enabled,
                mode,
                position,
                transform,
                scale,
                adaptive_sync,
            })
        })
        .collect()
}

/// A change's mode on `h`: the listed one of its size nearest its rate
/// (within [`RATE_SLACK`]; the fastest for a rate of 0), else a custom
/// mode.
fn choose_mode(client: &Client, h: &HeadEntry, m: OutputMode) -> Result<ModeChoice, String> {
    let size = |what: &str, v: i64| match i32::try_from(v) {
        Ok(v) if v > 0 => Ok(v),
        _ => Err(format!("{}: mode {what} {v} is not a size", h.name)),
    };
    let width = size("width", m.width)?;
    let height = size("height", m.height)?;
    if !(m.refresh.is_finite() && m.refresh >= 0.0 && m.refresh < 1_000_000.0) {
        return Err(format!("{}: refresh {} is not a rate", h.name, m.refresh));
    }
    // mHz, as the protocol counts it (below 1e9, so it fits).
    let rate = (m.refresh * 1000.0).round() as i32;
    let listed = h.modes.iter().filter_map(|id| {
        client
            .modes
            .get(id)
            .map(|e| (id, e.width, e.height, e.refresh))
    });
    Ok(match pick(listed, width, height, rate) {
        Some(id) => ModeChoice::Listed(id.clone()),
        None => ModeChoice::Custom(width, height, rate),
    })
}

/// Of `listed` modes (each a key, its width, height and mHz), the one of
/// `width` by `height` nearest `rate` (mHz) within [`RATE_SLACK`], or the
/// fastest of that size for a rate of 0. The first of equals wins.
fn pick<K>(
    listed: impl Iterator<Item = (K, i32, i32, i32)>,
    width: i32,
    height: i32,
    rate: i32,
) -> Option<K> {
    let mut best: Option<(K, i64)> = None;
    for (k, _, _, r) in listed.filter(|&(_, w, h, _)| w == width && h == height) {
        // Smaller is better: the distance from the rate, or (for 0) the
        // rate itself, negated, so the fastest wins.
        let score = if rate == 0 {
            -i64::from(r)
        } else {
            let d = (i64::from(r) - i64::from(rate)).abs();
            if d > RATE_SLACK {
                continue;
            }
            d
        };
        if best.as_ref().is_none_or(|(_, b)| score < *b) {
            best = Some((k, score));
        }
    }
    best.map(|(k, _)| k)
}

/// The mode `h` is configured with when no change names one: what it
/// shows, else (turned on) its preferred, else its first.
fn default_mode(client: &Client, h: &HeadEntry) -> ModeChoice {
    let listed = |id: &ObjectId| client.modes.contains_key(id);
    if let Some(id) = h.current_mode.as_ref().filter(|id| listed(id)) {
        return ModeChoice::Listed(id.clone());
    }
    let preferred = h
        .modes
        .iter()
        .find(|id| client.modes.get(id).is_some_and(|m| m.preferred));
    match preferred.or_else(|| h.modes.first()) {
        Some(id) => ModeChoice::Listed(id.clone()),
        None => ModeChoice::Keep,
    }
}

/// A configuration waiting for its turn, or in flight.
#[derive(Debug)]
struct Request {
    action: OutputsAction,
    /// Times the compositor cancelled it.
    tries: u32,
}

/// The configurations asked for and not answered yet.
#[derive(Debug, Default)]
struct Pending {
    /// Waiting for their turn, in order.
    waiting: VecDeque<Request>,
    /// Sent, its answer not read yet.
    in_flight: Option<InFlight>,
}

/// The configuration in flight.
#[derive(Debug)]
struct InFlight {
    id: u64,
    config: ZwlrOutputConfigurationV1,
    request: Request,
}

/// Sends `request` as a configuration (number `id`): `Err` (why) when it
/// is refused before it is sent.
fn send(
    client: &Client,
    qh: &QueueHandle<Client>,
    id: u64,
    request: &Request,
) -> Result<ZwlrOutputConfigurationV1, String> {
    let (Some(manager), Some(serial)) = (client.manager.as_ref(), client.serial) else {
        return Err(
            "the compositor offers no output configuration (zwlr_output_manager_v1)".into(),
        );
    };
    let plans = plan(client, request.action.changes())?;
    let config = manager.create_configuration(serial, qh, id);
    for p in plans {
        let Some(head) = client.heads.iter().find(|h| h.proxy.id() == p.head) else {
            continue;
        };
        if !p.enabled {
            config.disable_head(&head.proxy);
            continue;
        }
        let ch = config.enable_head(&head.proxy, qh, ());
        match &p.mode {
            ModeChoice::Keep => {}
            ModeChoice::Listed(m) => {
                if let Some(m) = client.modes.get(m) {
                    ch.set_mode(&m.proxy);
                }
            }
            ModeChoice::Custom(w, h, r) => ch.set_custom_mode(*w, *h, *r),
        }
        ch.set_position(p.position.0, p.position.1);
        ch.set_transform(p.transform);
        ch.set_scale(p.scale);
        if ch.version() >= 4 {
            ch.set_adaptive_sync(if p.adaptive_sync {
                zwlr_output_head_v1::AdaptiveSyncState::Enabled
            } else {
                zwlr_output_head_v1::AdaptiveSyncState::Disabled
            });
        }
    }
    match request.action {
        OutputsAction::Apply { .. } => config.apply(),
        OutputsAction::Test { .. } => config.test(),
    }
    Ok(config)
}

/// The socket `$WAYLAND_DISPLAY` names (under `$XDG_RUNTIME_DIR` unless
/// absolute; `wayland-0` when unset).
fn socket(target: WaylandTarget) -> io::Result<std::path::PathBuf> {
    match target {
        WaylandTarget::Socket(p) => Ok(p),
        WaylandTarget::Env => crate::wm::protocol::display_socket(
            std::env::var_os("WAYLAND_DISPLAY"),
            std::env::var_os("XDG_RUNTIME_DIR"),
        ),
    }
}

fn timespec(d: Duration) -> rustix::event::Timespec {
    rustix::event::Timespec {
        tv_sec: d.as_secs().try_into().unwrap_or(i64::MAX),
        tv_nsec: d.subsec_nanos().into(),
    }
}

impl Outputs {
    fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let wake = Arc::new(
            rustix::event::eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)
                .map_err(io::Error::from)?,
        );
        let poke = wake.clone();
        cx.set_notify(move || {
            let _ = rustix::io::write(&*poke, &1u64.to_ne_bytes());
        });
        let mut pending = Pending::default();
        let r = Self::serve(&mut cx, &wake, &mut pending);
        if let Err(e) = &r {
            // What it showed no longer holds, and what it was asked to do
            // will not be done: each is `failed`, as an action reaching no
            // run is.
            cx.update(|s| {
                s.all.clear();
                s.available = false;
            });
            let error = format!("the compositor's connection failed: {e}");
            let mut lost: Vec<OutputsAction> = pending
                .in_flight
                .take()
                .map(|f| f.request.action)
                .into_iter()
                .chain(pending.waiting.drain(..).map(|r| r.action))
                .collect();
            while let Ok(m) = cx.try_recv() {
                if let Msg::Action(a) = m {
                    lost.push(a);
                }
            }
            for a in lost {
                cx.emit(OutputsEvent::Failed((a.name().to_string(), error.clone())));
            }
        }
        r
    }

    fn serve(cx: &mut Cx<Self>, wake: &OwnedFd, pending: &mut Pending) -> Result<(), ServiceError> {
        let path = socket(target())?;
        let stream = UnixStream::connect(&path)
            .map_err(|e| ServiceError(format!("{}: {e}", path.display())))?;
        let conn = Connection::from_socket(stream).map_err(|e| ServiceError(e.to_string()))?;
        let mut queue: EventQueue<Client> = conn.new_event_queue();
        let qh = queue.handle();
        let display = conn.display();
        let _registry = display.get_registry(&qh, ());
        display.sync(&qh, ());
        let mut client = Client::default();
        let Pending { waiting, in_flight } = pending;
        let mut next_id = 0u64;
        // Set once stopped: the deadline for the answer in flight.
        let mut stopping: Option<Instant> = None;
        loop {
            queue
                .dispatch_pending(&mut client)
                .map_err(|e| ServiceError(e.to_string()))?;
            // Messages.
            if stopping.is_none() {
                loop {
                    match cx.try_recv() {
                        Ok(Msg::Action(a)) => waiting.push_back(Request {
                            action: a,
                            tries: 0,
                        }),
                        Ok(_) => {}
                        Err(false) => break,
                        Err(true) => {
                            stopping = Some(Instant::now() + STOP_WAIT);
                            break;
                        }
                    }
                }
            }
            // State, at each `done` (or once the registry shows no manager).
            if std::mem::take(&mut client.changed) || (client.synced && !cx.is_ready()) {
                let ready = client.manager.is_none() || client.serial.is_some();
                if ready && stopping.is_none() {
                    let all = client.snapshot();
                    let available = client.manager.is_some();
                    cx.update(|s| {
                        s.all = all;
                        s.available = available;
                    });
                    cx.ready();
                }
            }
            // Answers.
            for (id, outcome) in std::mem::take(&mut client.answers) {
                let Some(f) = in_flight.take_if(|f| f.id == id) else {
                    continue;
                };
                f.config.destroy();
                let action = f.request.action.name().to_string();
                match outcome {
                    Outcome::Succeeded => {
                        cx.emit(OutputsEvent::Succeeded(action));
                    }
                    Outcome::Failed => {
                        log::warn!("outputs: the compositor refused a configuration ({action})");
                        cx.emit(OutputsEvent::Failed((
                            action,
                            "the compositor refused the configuration".into(),
                        )));
                    }
                    Outcome::Cancelled if f.request.tries + 1 < MAX_TRIES => {
                        // The outputs changed under it: again, from the
                        // new state, before anything waiting.
                        waiting.push_front(Request {
                            tries: f.request.tries + 1,
                            ..f.request
                        });
                    }
                    Outcome::Cancelled => {
                        cx.emit(OutputsEvent::Failed((
                            action,
                            "the outputs kept changing while it was applied".into(),
                        )));
                    }
                }
            }
            if let Some(deadline) = stopping
                && (in_flight.is_none() || Instant::now() >= deadline)
            {
                let _ = conn.flush();
                return Ok(());
            }
            // The next configuration, once the first state is in (a
            // manager announced with no `done` yet is waited for).
            let settled = client.synced && (client.manager.is_none() || client.serial.is_some());
            while in_flight.is_none() && settled && stopping.is_none() {
                let Some(request) = waiting.pop_front() else {
                    break;
                };
                next_id += 1;
                match send(&client, &qh, next_id, &request) {
                    Ok(config) => {
                        *in_flight = Some(InFlight {
                            id: next_id,
                            config,
                            request,
                        })
                    }
                    Err(e) => {
                        let action = request.action.name();
                        log::warn!("outputs: {action} refused: {e}");
                        cx.emit(OutputsEvent::Failed((action.to_string(), e)));
                    }
                }
            }
            // A full socket buffer is not an error: wait until it drains.
            let unflushed = match queue.flush() {
                Ok(()) => false,
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == io::ErrorKind::WouldBlock =>
                {
                    true
                }
                Err(e) => return Err(ServiceError(e.to_string())),
            };
            let Some(guard) = queue.prepare_read() else {
                continue;
            };
            let (readable, woken) = {
                let fd = guard.connection_fd();
                let flags = if unflushed {
                    PollFlags::IN | PollFlags::OUT
                } else {
                    PollFlags::IN
                };
                let mut fds = [PollFd::new(&fd, flags), PollFd::new(wake, PollFlags::IN)];
                let timeout =
                    stopping.map(|d| timespec(d.saturating_duration_since(Instant::now())));
                match rustix::event::poll(&mut fds, timeout.as_ref()) {
                    Ok(_) => (
                        fds[0]
                            .revents()
                            .intersects(PollFlags::IN | PollFlags::ERR | PollFlags::HUP),
                        !fds[1].revents().is_empty(),
                    ),
                    Err(rustix::io::Errno::INTR) => (false, false),
                    Err(e) => return Err(io::Error::from(e).into()),
                }
            };
            if readable {
                match guard.read() {
                    Ok(_) => {}
                    Err(wayland_client::backend::WaylandError::Io(e))
                        if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        return Err(ServiceError(format!("the Wayland connection ended: {e}")));
                    }
                }
            } else {
                drop(guard);
            }
            if woken {
                let mut buf = [0u8; 8];
                let _ = rustix::io::read(wake, &mut buf);
            }
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for Client {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == ZwlrOutputManagerV1::interface().name && state.manager.is_none() => {
                state.manager = Some(registry.bind(name, version.min(VERSION), qh, ()));
                state.manager_global = Some(name);
            }
            wl_registry::Event::GlobalRemove { name } if state.manager_global == Some(name) => {
                if let Some(m) = &state.manager {
                    m.stop();
                }
                state.drop_manager();
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for Client {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.synced = true;
        }
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for Client {
    fn event(
        state: &mut Self,
        _: &ZwlrOutputManagerV1,
        event: zwlr_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_manager_v1::Event::Head { head } => {
                state.heads.push(HeadEntry::new(head));
            }
            zwlr_output_manager_v1::Event::Done { serial } => {
                state.serial = Some(serial);
                state.changed = true;
            }
            zwlr_output_manager_v1::Event::Finished => state.drop_manager(),
            _ => {}
        }
    }

    event_created_child!(Client, ZwlrOutputManagerV1, [
        zwlr_output_manager_v1::EVT_HEAD_OPCODE => (ZwlrOutputHeadV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputHeadV1, ()> for Client {
    fn event(
        state: &mut Self,
        head: &ZwlrOutputHeadV1,
        event: zwlr_output_head_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_output_head_v1::Event as E;
        let id = head.id();
        if let E::Finished = event {
            if let Some(i) = state.heads.iter().position(|h| h.proxy.id() == id) {
                let h = state.heads.remove(i);
                for m in &h.modes {
                    if let Some(m) = state.modes.remove(m)
                        && m.proxy.version() >= 3
                    {
                        m.proxy.release();
                    }
                }
                if h.proxy.version() >= 3 {
                    h.proxy.release();
                }
            }
            return;
        }
        if let E::Mode { mode } = &event {
            state.modes.insert(
                mode.id(),
                ModeEntry {
                    proxy: mode.clone(),
                    width: 0,
                    height: 0,
                    refresh: 0,
                    preferred: false,
                },
            );
        }
        let Some(h) = state.head_mut(&id) else {
            return;
        };
        match event {
            E::Name { name } => h.name = name,
            E::Description { description } => h.description = description,
            E::PhysicalSize { width, height } => h.physical = (width, height),
            E::Mode { mode } => h.modes.push(mode.id()),
            E::Enabled { enabled } => h.enabled = enabled != 0,
            E::CurrentMode { mode } => h.current_mode = Some(mode.id()),
            E::Position { x, y } => h.position = (x, y),
            E::Transform {
                transform: WEnum::Value(t),
            } => h.transform = t,
            E::Scale { scale } => h.scale = scale,
            E::Make { make } => h.make = make,
            E::Model { model } => h.model = model,
            E::SerialNumber { serial_number } => h.serial = serial_number,
            E::AdaptiveSync { state } => {
                h.adaptive_sync = matches!(
                    state,
                    WEnum::Value(zwlr_output_head_v1::AdaptiveSyncState::Enabled)
                );
            }
            _ => {}
        }
    }

    event_created_child!(Client, ZwlrOutputHeadV1, [
        zwlr_output_head_v1::EVT_MODE_OPCODE => (ZwlrOutputModeV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputModeV1, ()> for Client {
    fn event(
        state: &mut Self,
        mode: &ZwlrOutputModeV1,
        event: zwlr_output_mode_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_output_mode_v1::Event as E;
        let id = mode.id();
        if let E::Finished = event {
            if let Some(m) = state.modes.remove(&id)
                && m.proxy.version() >= 3
            {
                m.proxy.release();
            }
            for h in &mut state.heads {
                h.modes.retain(|m| *m != id);
                if h.current_mode.as_ref() == Some(&id) {
                    h.current_mode = None;
                }
            }
            return;
        }
        let Some(m) = state.modes.get_mut(&id) else {
            return;
        };
        match event {
            E::Size { width, height } => (m.width, m.height) = (width, height),
            E::Refresh { refresh } => m.refresh = refresh,
            E::Preferred => m.preferred = true,
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationV1, u64> for Client {
    fn event(
        state: &mut Self,
        _: &ZwlrOutputConfigurationV1,
        event: zwlr_output_configuration_v1::Event,
        id: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_output_configuration_v1::Event as E;
        let outcome = match event {
            E::Succeeded => Outcome::Succeeded,
            E::Failed => Outcome::Failed,
            E::Cancelled => Outcome::Cancelled,
            _ => return,
        };
        state.answers.push((*id, outcome));
    }
}

impl Dispatch<ZwlrOutputConfigurationHeadV1, ()> for Client {
    fn event(
        _: &mut Self,
        _: &ZwlrOutputConfigurationHeadV1,
        _: zwlr_output_configuration_head_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Data, FromData, Keyed, Service, ToData};

    #[test]
    fn the_store_is_the_schema() {
        assert_eq!(<Outputs as Service>::NAME, "outputs");
        let names: Vec<&str> = Outputs::FIELDS.iter().map(|f| f.name).collect();
        assert_eq!(names, ["all", "available"]);
        assert_eq!((Outputs::FIELDS[0].ty)(), "[Output]");
        assert_eq!(Output::KEY_FIELD, "name");
    }

    #[test]
    fn a_change_round_trips_with_its_nulls() {
        let c = OutputChange {
            scale: Some(1.5),
            transform: Some(OutputTransform::Flipped90),
            mode: Some(OutputMode {
                width: 1280,
                height: 720,
                refresh: 59.94,
            }),
            ..OutputChange::of("DP-1")
        };
        let d = c.to_data();
        assert_eq!(OutputChange::from_data(&d), Ok(c));
        // A record the language built with only `name` set.
        let bare = Data::Record {
            ty: "OutputChange".into(),
            fields: vec![("name".into(), Data::Text("HDMI-A-1".into()))],
        };
        assert_eq!(
            OutputChange::from_data(&bare),
            Ok(OutputChange::of("HDMI-A-1"))
        );
        assert_eq!(
            OutputTransform::Rotate270.to_data(),
            Data::Enum {
                ty: "OutputTransform".into(),
                variant: "rotate_270".into()
            }
        );
    }

    #[test]
    fn every_transform_maps_to_wl_output_and_back() {
        for t in [
            OutputTransform::Normal,
            OutputTransform::Rotate90,
            OutputTransform::Rotate180,
            OutputTransform::Rotate270,
            OutputTransform::Flipped,
            OutputTransform::Flipped90,
            OutputTransform::Flipped180,
            OutputTransform::Flipped270,
        ] {
            assert_eq!(OutputTransform::from_wl(t.to_wl()), t);
        }
    }

    #[test]
    fn a_mode_is_the_listed_one_nearest_its_rate() {
        let listed = [
            ("a", 1920, 1080, 60_000),
            ("b", 1920, 1080, 59_940),
            ("c", 1920, 1080, 144_001),
            ("d", 2560, 1440, 59_951),
            ("e", 1920, 1080, 60_000),
        ];
        let pick_of = |w, h, hz: f64| pick(listed.into_iter(), w, h, (hz * 1000.0).round() as i32);
        assert_eq!(
            pick_of(1920, 1080, 60.0),
            Some("a"),
            "exact, the first of equals"
        );
        assert_eq!(pick_of(1920, 1080, 59.94), Some("b"));
        assert_eq!(pick_of(1920, 1080, 59.95), Some("b"), "the nearest");
        assert_eq!(pick_of(1920, 1080, 144.0), Some("c"), "within 0.5 Hz");
        assert_eq!(pick_of(2560, 1440, 60.0), Some("d"));
        assert_eq!(pick_of(1920, 1080, 75.0), None, "a custom mode");
        assert_eq!(pick_of(1280, 720, 60.0), None, "another size");
        assert_eq!(pick_of(1920, 1080, 0.0), Some("c"), "the fastest");
    }

    #[test]
    fn a_refused_action_is_the_failed_event() {
        let a = OutputsAction::Test {
            changes: Vec::new(),
        };
        assert_eq!(
            refused(&a, "no run"),
            Some(OutputsEvent::Failed(("test".into(), "no run".into())))
        );
    }
}
