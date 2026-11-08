//! `outputs`: the monitors' configuration, from the compositor's
//! `zwlr_output_manager_v1` (wlr-output-management, which sway, Hyprland,
//! niri and labwc implement; owner-testing gap 18, decisions.md
//! m6-owner-fixes).
//!
//! `all` lists every head the manager announces, on or off, in the order
//! of their names, published at each manager `done` (the protocol's
//! atomic update): its name, description, make, model and serial, its
//! modes (deduplicated, in the compositor's order), the current and
//! preferred mode, its place, scale, transform and adaptive sync.
//! (output-id) `id` is the identity a `Screen` has for the same monitor
//! ([`output_id`]). `available` is the manager being offered.
//! (outputs-mode) A head on that names no current mode (Hyprland 0.57,
//! for a mode not in its list) has the one its `wl_output` shows, which
//! the service binds for that. (outputs-resync) A change the manager
//! sends with no `done` for 250 ms (Hyprland 0.57, whose change events
//! carry the values from before the change) has the manager bound again,
//! which gets the outputs as they are ([`Client::resync`]).
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

/// (outputs-resync) How long a manager's change may go without a `done`
/// before the manager is bound again ([`Client::resync`]).
const RESYNC_AFTER: Duration = Duration::from_millis(250);

/// The highest manager version this client speaks (4: adaptive sync).
const VERSION: u32 = 4;

/// The smallest scale the protocol carries (`wl_fixed` steps by 1/256).
const MIN_SCALE: f64 = 1.0 / 256.0;

/// The largest scale the protocol carries (`wl_fixed`'s largest).
const MAX_SCALE: f64 = 8_388_607.0;

/// How far a change's refresh rate may be from a listed mode's, in mHz.
const RATE_SLACK: i64 = 500;

static TARGET: Mutex<Option<WaylandTarget>> = Mutex::new(None);

/// (output-id, review rounds 3 and 4) The monitors' ids between the
/// `outputs` service and strand-surface ([`ids`], [`shown`]).
struct Ids {
    /// Connector and `Output.id` of every monitor connected, on or off,
    /// as the last snapshot numbered them.
    given: Vec<(String, String)>,
    /// A run is up and `given` is its last snapshot. A stopped run (no
    /// reader, a reload, a failure) sees no plug, unplug or move, so its
    /// list is not given out ([`ids`]).
    live: bool,
    /// Connector and `Screen.id` of every monitor strand-surface shows
    /// ([`shown`]), which a run's new heads take first.
    shown: Vec<(String, String)>,
}

static IDS: Mutex<Ids> = Mutex::new(Ids {
    given: Vec::new(),
    live: false,
    shown: Vec::new(),
});

/// (output-id, review rounds 3 and 4) Each connected monitor's connector
/// and `Output.id`, on or off, while an `outputs` run is up. strand-surface
/// numbers a monitor's `Screen.id` from this (`SurfaceHost::output_ids`):
/// a monitor off has no `wl_output`, so only this list shows that it is
/// still connected and holds its number. Empty while no run is up (none
/// yet, or stopped, backing off, its manager gone): what the last run
/// said may no longer hold (a monitor moved to another port since), and
/// `Screen.id` is numbered by plug order alone.
pub fn ids() -> Vec<(String, String)> {
    IDS.lock()
        .map(|i| match i.live {
            true => i.given.clone(),
            false => Vec::new(),
        })
        .unwrap_or_default()
}

/// (output-id, review round 4) A monitor strand-surface shows on
/// `connector` has `Screen.id` `id` (`None`: it is no longer shown). A
/// run's heads new to it take these ids first, then the ids the last run
/// gave their connectors, so `Output.id` is `Screen.id` for every monitor
/// shown whatever happened while no run was up.
pub fn shown(connector: &str, id: Option<&str>) {
    if let Ok(mut i) = IDS.lock() {
        i.shown.retain(|(c, _)| c != connector);
        if let Some(id) = id {
            i.shown.push((connector.to_string(), id.to_string()));
        }
    }
}

/// The ids a run's new heads take, by connector: [`shown`]'s, then the
/// last run's for the connectors not shown (a monitor off).
fn remembered_ids() -> Vec<(String, String)> {
    IDS.lock()
        .map(|i| {
            let mut all = i.shown.clone();
            all.extend(
                i.given
                    .iter()
                    .filter(|(c, _)| !i.shown.iter().any(|(s, _)| s == c))
                    .cloned(),
            );
            all
        })
        .unwrap_or_default()
}

/// A run's snapshot numbered `all`: [`ids`] gives it out.
fn share(all: &[Output]) {
    if let Ok(mut i) = IDS.lock() {
        i.given = all.iter().map(|o| (o.name.clone(), o.id.clone())).collect();
        i.live = true;
    }
}

/// The run ended, or its manager went: [`ids`] gives nothing until a
/// snapshot again.
fn unshare() {
    if let Ok(mut i) = IDS.lock() {
        i.live = false;
    }
}

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
    /// (output-id) The monitor's identity, as `Screen.id` gives it
    /// ([`output_id`]): kept across ports, so a saved profile names it.
    pub id: String,
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

/// (output-id) A monitor's identity from its head, as strand-surface's
/// `MonitorId` makes it from its `wl_output` (`Screen.id`): make, model
/// and description joined by `" | "`, the description without the
/// connector the compositor puts in it ([`without_connector`]; a copy of
/// strand-surface's `identity_description`, which this crate does not
/// depend on; a test in crates/strand holds the two equal). A make or
/// model the head does not give is `Unknown`, as wlroots' `wl_output`
/// says it (seen on headless sway). [`Outputs`] numbers identical heads
/// ` #2`, ` #3`, the ones on first, in the order of their names.
pub fn output_id(make: &str, model: &str, description: &str, connector: &str) -> String {
    let known = |s: &'_ str| -> String {
        match s {
            "" => "Unknown".into(),
            s => s.into(),
        }
    };
    let (make, model) = (known(make), known(model));
    format!(
        "{make} | {model} | {}",
        without_connector(description, connector)
    )
}

/// (output-id, review round 4) `description` without the output's own
/// connector `c`, which changes when the monitor is plugged into another
/// port: wlroots ends it with `" (<c>)"`, or `" (<c> via <sub>)"` when a
/// DP connector drives an HDMI or DVI sink, and smithay (niri) with
/// `" - <c>"` (`make - model - connector`). Anything else is kept.
fn without_connector<'a>(description: &'a str, c: &str) -> &'a str {
    if c.is_empty() {
        return description;
    }
    if let Some(inner) = description.strip_suffix(')')
        && let Some(open) = inner.rfind(" (")
    {
        let within = &inner[open + 2..];
        let via = within
            .strip_prefix(c)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(" via "));
        if via {
            return &description[..open];
        }
    }
    description
        .strip_suffix(c)
        .and_then(|d| d.strip_suffix(" - "))
        .unwrap_or(description)
}

/// (outputs-order) Connector names in order, a run of digits by its
/// number (`DP-2` before `DP-10`), so `outputs.all` lists the outputs the
/// same way at every start, whatever order the compositor announces them
/// in (sway announces them in reverse order after a restart).
fn by_name(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        match (a.first(), b.first()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let run = |s: &[u8]| s.iter().take_while(|c| c.is_ascii_digit()).count();
                let (na, nb) = (run(a), run(b));
                let trim = |s: &[u8]| -> usize { s.iter().take_while(|c| **c == b'0').count() };
                let (da, db) = (
                    &a[trim(&a[..na]).min(na)..na],
                    &b[trim(&b[..nb]).min(nb)..nb],
                );
                let o = da.len().cmp(&db.len()).then_with(|| da.cmp(db));
                if o != std::cmp::Ordering::Equal {
                    return o;
                }
                a = &a[na..];
                b = &b[nb..];
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(y);
                }
                a = &a[1..];
                b = &b[1..];
            }
        }
    }
}

/// Fills each output's `id`, numbering identical monitors (the second
/// ` #2`). The rule (decisions.md output-id, review round 3): an output
/// comes with the id it had at the last snapshot (empty when new) and
/// keeps it for as long as it is connected, on or off, so applying a
/// configuration, which connects and disconnects nothing, renames no
/// monitor, and a profile chosen by id is still the one chosen after it
/// is applied. A new one takes the lowest number no connected monitor of
/// its identity holds, those on first, in the order of their names, then
/// those off. strand-surface takes `Screen.id` from this numbering
/// ([`ids`]), so the two agree for every monitor shown.
fn number_ids(all: &mut [Output]) {
    let bases: Vec<String> = all
        .iter()
        .map(|o| output_id(&o.make, &o.model, &o.description, &o.name))
        .collect();
    let nth = |base: &str, n: usize| match n {
        1 => base.to_string(),
        n => format!("{base} #{n}"),
    };
    // The number of the id an output had, when it is still of its base.
    let prior: Vec<Option<usize>> = all
        .iter()
        .zip(&bases)
        .map(|(o, base)| match o.id.strip_prefix(base.as_str())? {
            "" => Some(1),
            rest => rest.strip_prefix(" #")?.parse().ok().filter(|&n| n >= 2),
        })
        .collect();
    let mut held: Vec<Option<usize>> = vec![None; all.len()];
    let taken = |held: &[Option<usize>], i: usize, n: usize| {
        held.iter()
            .enumerate()
            .any(|(j, h)| *h == Some(n) && bases[j] == bases[i])
    };
    let order: Vec<usize> = (0..all.len())
        .filter(|&i| all[i].enabled)
        .chain((0..all.len()).filter(|&i| !all[i].enabled))
        .collect();
    // Every one connected keeps its number (two with the same, after a
    // change of description: the first, those on first), then the new.
    for &i in &order {
        if let Some(n) = prior[i]
            && !taken(&held, i, n)
        {
            held[i] = Some(n);
        }
    }
    for &i in &order {
        if held[i].is_none() {
            let n = (1..).find(|&n| !taken(&held, i, n)).unwrap_or(1);
            held[i] = Some(n);
        }
    }
    for (i, o) in all.iter_mut().enumerate() {
        o.id = nth(&bases[i], held[i].unwrap_or(1));
    }
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
    /// Every output, on or off, keyed by `name`, in the order of their
    /// names (`DP-2` before `DP-10`), whatever order the compositor
    /// announces them in.
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
    /// (output-id) Its id at the last snapshot, kept while the head is
    /// there ([`number_ids`]).
    id: String,
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
            id: String::new(),
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
    /// The manager went (finished, or its global removed) and none is
    /// bound since: what was shown no longer holds, and the service is
    /// not ready until another manager's first `done`.
    lost: bool,
    /// The registry's first sync is answered: every global then present
    /// was announced.
    synced: bool,
    /// Answers to configurations, by the configuration's own number.
    answers: Vec<(u64, Outcome)>,
    /// (output-id, review round 3) Whether the snapshot shares its ids
    /// with the process ([`IDS`]): a head new to this run takes the id
    /// the last run gave its connector, and strand-surface reads them.
    /// Off in unit tests, which run side by side.
    shared: bool,
    /// (outputs-resync) The registry, to bind the manager again.
    registry: Option<wl_registry::WlRegistry>,
    /// The version the manager was bound at.
    manager_version: u32,
    /// (outputs-resync) When the current manager sent a head or mode
    /// event after its last `done` with no `done` since: Hyprland 0.57
    /// sends a change that way, its values one change behind, so the
    /// manager is bound again ([`Client::resync`]) once [`RESYNC_AFTER`]
    /// passes without a `done`.
    undone: Option<Instant>,
    /// (outputs-resync) Name and id of each head before a resync, which
    /// the new manager's heads take.
    carried: Vec<(String, String)>,
    /// (outputs-mode) The `wl_output`s, for the current mode of a head
    /// that names none.
    wl_outputs: Vec<WlOut>,
}

/// (outputs-mode) A `wl_output` as the outputs service binds it: its
/// connector name and current mode (`wl_output.mode` with `current`),
/// which every `wl_output` sends while its output is on.
#[derive(Debug)]
struct WlOut {
    global: u32,
    proxy: wl_output::WlOutput,
    name: String,
    mode: Option<OutputMode>,
}

impl Client {
    fn head_mut(&mut self, id: &ObjectId) -> Option<&mut HeadEntry> {
        self.heads.iter_mut().find(|h| h.proxy.id() == *id)
    }

    /// The outputs as published: every head with a name, modes
    /// deduplicated, in the order of their names ([`by_name`]); each head
    /// keeps its id ([`number_ids`]). (outputs-mode) A head on that names
    /// no current mode (Hyprland, for a mode not in its list) has the one
    /// its `wl_output` shows.
    fn snapshot(&mut self) -> Vec<Output> {
        let mut remembered = std::mem::take(&mut self.carried);
        if self.shared {
            remembered.extend(remembered_ids());
        }
        let mut all: Vec<Output> = self
            .heads
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
                let mode = match h.enabled {
                    true => h
                        .current_mode
                        .as_ref()
                        .and_then(|id| self.modes.get(id))
                        .map(ModeEntry::public)
                        .or_else(|| {
                            self.wl_outputs
                                .iter()
                                .find(|o| o.name == h.name)
                                .and_then(|o| o.mode.clone())
                        }),
                    false => None,
                };
                // New to this run: what the last run called it, if any.
                let id = match h.id.is_empty() {
                    true => remembered
                        .iter()
                        .find(|(n, _)| *n == h.name)
                        .map(|(_, id)| id.clone())
                        .unwrap_or_default(),
                    false => h.id.clone(),
                };
                Output {
                    name: h.name.clone(),
                    id,
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
            .collect();
        all.sort_by(|a, b| by_name(&a.name, &b.name));
        number_ids(&mut all);
        for h in self.heads.iter_mut().filter(|h| !h.name.is_empty()) {
            if let Some(o) = all.iter().find(|o| o.name == h.name) {
                h.id.clone_from(&o.id);
            }
        }
        if self.shared {
            share(&all);
        }
        all
    }

    /// (outputs-resync) Binds the manager again, as a fresh client would,
    /// and stops the one bound: Hyprland 0.57 sends a change with no
    /// `done` and the values it had before the change, while a manager
    /// bound after it gets the outputs as they are, and a `done`. What
    /// was published stands until the new manager's first `done`; each
    /// head keeps its id ([`Client::carried`]), and the run stays ready.
    fn resync(&mut self, qh: &QueueHandle<Client>) {
        self.undone = None;
        let (Some(registry), Some(global), Some(old)) =
            (&self.registry, self.manager_global, self.manager.take())
        else {
            return;
        };
        // Hyprland 0.57 never answers `finished`, and goes on sending
        // the stopped manager's changes: they are not this state's
        // (`Dispatch<ZwlrOutputManagerV1>`), and nothing is kept of it.
        old.stop();
        self.carried = self
            .heads
            .iter()
            .filter(|h| !h.id.is_empty())
            .map(|h| (h.name.clone(), h.id.clone()))
            .collect();
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
        self.serial = None;
        self.changed = false;
        self.manager = Some(registry.bind(global, self.manager_version, qh, ()));
    }

    /// (outputs-resync) A resync is due: the manager sent a change and no
    /// `done` for [`RESYNC_AFTER`].
    fn resync_due(&self, now: Instant) -> bool {
        self.manager.is_some() && self.resync_at().is_some_and(|d| now >= d)
    }

    /// When the next resync falls due, for the poll's timeout.
    fn resync_at(&self) -> Option<Instant> {
        self.undone.map(|t| t + RESYNC_AFTER)
    }

    /// An event of the current manager, or one of its heads or modes,
    /// changed what a `done` will publish.
    fn touched(&mut self) {
        if self.serial.is_some() && self.undone.is_none() {
            self.undone = Some(Instant::now());
        }
    }

    /// What to publish after a dispatch (`ready`: the service is): the
    /// outputs at each `done`, or once the registry shows no manager.
    /// (service-ready) A manager that went while the connection stays (a
    /// compositor shutting down removes its globals first) clears them,
    /// not ready first, as a failed connection does, so a `file` written
    /// from `outputs.all` waits instead of writing an empty list; ready
    /// again at another manager's first `done`.
    fn publish(&mut self, ready: bool) -> Publish {
        let changed = std::mem::take(&mut self.changed);
        if self.lost {
            return match changed {
                true => Publish::Cleared,
                false => Publish::Nothing,
            };
        }
        let answered = self.manager.is_none() || self.serial.is_some();
        if !(changed || self.synced && !ready) || !answered {
            return Publish::Nothing;
        }
        let available = self.manager.is_some();
        Publish::Shown(self.snapshot(), available)
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
        self.lost = true;
    }
}

/// What [`Client::publish`] asks of the run.
#[derive(Debug, PartialEq)]
enum Publish {
    Nothing,
    /// Not ready, then the outputs cleared.
    Cleared,
    /// The outputs and whether a manager is there, then ready.
    Shown(Vec<Output>, bool),
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
/// shows, else (turned on) its preferred, else its first. (outputs-mode)
/// What it shows is the one its `wl_output` says when the head names
/// none (a mode not in its list on Hyprland), so a change to another
/// head leaves it as it is.
fn default_mode(client: &Client, h: &HeadEntry) -> ModeChoice {
    let listed = |id: &ObjectId| client.modes.contains_key(id);
    if let Some(id) = h.current_mode.as_ref().filter(|id| listed(id)) {
        return ModeChoice::Listed(id.clone());
    }
    let shown = client
        .wl_outputs
        .iter()
        .find(|o| o.name == h.name)
        .and_then(|o| o.mode.clone())
        .filter(|_| h.enabled);
    if let Some(m) = shown
        && let Ok(choice) = choose_mode(client, h, m)
    {
        return choice;
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
        // (output-id, review round 4) No run, no plug or move seen: the
        // ids are no longer given out.
        unshare();
        if let Err(e) = &r {
            // What it showed no longer holds, and what it was asked to do
            // will not be done: each is `failed`, as an action reaching no
            // run is. The service is not ready first (service-ready), so
            // no reader takes the cleared list for a change while `ready`
            // is true: a `file` written from `outputs.all` waits instead
            // of writing an empty list.
            cx.unready();
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
        let registry = display.get_registry(&qh, ());
        display.sync(&qh, ());
        let mut client = Client {
            shared: true,
            registry: Some(registry),
            ..Client::default()
        };
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
            match client.publish(cx.is_ready()) {
                Publish::Cleared => {
                    unshare();
                    cx.unready();
                    cx.update(|s| {
                        s.all.clear();
                        s.available = false;
                    });
                }
                Publish::Shown(all, available) if stopping.is_none() => {
                    cx.update(|s| {
                        s.all = all;
                        s.available = available;
                    });
                    cx.ready();
                }
                _ => {}
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
            // (outputs-resync) A change with no `done`: bound again, once
            // no configuration is in flight (its answer is the old
            // manager's).
            if client.resync_due(Instant::now()) && in_flight.is_none() && stopping.is_none() {
                log::debug!("outputs: a change came with no done; binding the manager again");
                client.resync(&qh);
                continue;
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
                let deadline = match (stopping, client.resync_at().filter(|_| in_flight.is_none()))
                {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                let timeout =
                    deadline.map(|d| timespec(d.saturating_duration_since(Instant::now())));
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
                state.manager_version = version.min(VERSION);
                state.manager = Some(registry.bind(name, state.manager_version, qh, ()));
                state.manager_global = Some(name);
                state.lost = false;
            }
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == wl_output::WlOutput::interface().name && version >= 4 => {
                state.wl_outputs.push(WlOut {
                    global: name,
                    proxy: registry.bind(name, 4, qh, name),
                    name: String::new(),
                    mode: None,
                });
            }
            wl_registry::Event::GlobalRemove { name } if state.manager_global == Some(name) => {
                if let Some(m) = &state.manager {
                    m.stop();
                }
                state.drop_manager();
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(i) = state.wl_outputs.iter().position(|o| o.global == name) {
                    let o = state.wl_outputs.remove(i);
                    if o.proxy.version() >= 3 {
                        o.proxy.release();
                    }
                }
            }
            _ => {}
        }
    }
}

/// (outputs-mode) A `wl_output`'s name and current mode.
impl Dispatch<wl_output::WlOutput, u32> for Client {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.wl_outputs.iter_mut().find(|o| o.global == *global) else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => o.name = name,
            wl_output::Event::Mode {
                flags: WEnum::Value(flags),
                width,
                height,
                refresh,
            } if flags.contains(wl_output::Mode::Current) => {
                o.mode = Some(OutputMode {
                    width: i64::from(width),
                    height: i64::from(height),
                    refresh: f64::from(refresh) / 1000.0,
                });
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
        manager: &ZwlrOutputManagerV1,
        event: zwlr_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // (outputs-resync) A manager stopped for a resync: its heads are
        // not this state's, and its end is not the manager going.
        if state.manager.as_ref() != Some(manager) {
            if let zwlr_output_manager_v1::Event::Head { head } = event
                && head.version() >= 3
            {
                head.release();
            }
            return;
        }
        match event {
            zwlr_output_manager_v1::Event::Head { head } => {
                state.touched();
                state.heads.push(HeadEntry::new(head));
            }
            zwlr_output_manager_v1::Event::Done { serial } => {
                state.serial = Some(serial);
                state.changed = true;
                state.undone = None;
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
        // A head of a manager stopped for a resync, or one released.
        if !state.heads.iter().any(|h| h.proxy.id() == id) {
            if let E::Mode { mode } = &event
                && mode.version() >= 3
            {
                mode.release();
            }
            return;
        }
        state.touched();
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
        if !state.modes.contains_key(&id) {
            return;
        }
        state.touched();
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

    /// (output-id) An output's id is its monitor's identity: the
    /// connector wlroots appends to the description dropped, identical
    /// monitors numbered, the ones on first.
    #[test]
    fn identical_heads_are_numbered_the_ones_on_first() {
        let head = |name: &str, desc: &str, enabled: bool| Output {
            name: name.into(),
            make: "Dell Inc.".into(),
            model: "DELL U2720Q".into(),
            description: desc.into(),
            enabled,
            ..Output::default()
        };
        let mut all = vec![
            head("DP-1", "Dell Inc. DELL U2720Q (DP-1)", false),
            head("DP-2", "Dell Inc. DELL U2720Q (DP-2)", true),
            head("DP-3", "Dell Inc. DELL U2720Q (DP-3)", true),
            Output {
                make: String::new(),
                ..head("eDP-1", "Built-in (not eDP-2)", true)
            },
        ];
        number_ids(&mut all);
        let ids: Vec<&str> = all.iter().map(|o| o.id.as_str()).collect();
        let base = "Dell Inc. | DELL U2720Q | Dell Inc. DELL U2720Q";
        assert_eq!(
            ids,
            [
                format!("{base} #3"),
                base.to_string(),
                format!("{base} #2"),
                "Unknown | DELL U2720Q | Built-in (not eDP-2)".to_string(),
            ]
        );
    }

    /// (output-id, review rounds 1 and 3) Two identical monitors keep
    /// their ids while they are connected, whichever is turned off or on;
    /// one unplugged and plugged in again takes the lowest number no
    /// connected monitor holds, and a new one does not take the number of
    /// one that is off.
    #[test]
    fn an_identical_monitor_keeps_its_id_while_it_is_there() {
        let head = |name: &str, enabled: bool| Output {
            name: name.into(),
            make: "Dell Inc.".into(),
            model: "DELL U2720Q".into(),
            description: format!("Dell Inc. DELL U2720Q ({name})"),
            enabled,
            ..Output::default()
        };
        let base = "Dell Inc. | DELL U2720Q | Dell Inc. DELL U2720Q";
        let second = format!("{base} #2");
        let ids = |all: &[Output]| -> Vec<String> { all.iter().map(|o| o.id.clone()).collect() };
        let mut all = vec![head("DP-1", true), head("DP-2", true)];
        number_ids(&mut all);
        assert_eq!(ids(&all), [base.to_string(), second.clone()]);
        // Every on and off of the two, from every other: neither is
        // renumbered.
        for (a, b) in [(false, true), (true, false), (false, false), (true, true)] {
            for (c, d) in [(false, true), (true, false), (false, false), (true, true)] {
                all[0].enabled = a;
                all[1].enabled = b;
                number_ids(&mut all);
                all[0].enabled = c;
                all[1].enabled = d;
                number_ids(&mut all);
                assert_eq!(ids(&all), [base.to_string(), second.clone()]);
            }
        }
        // DP-1 unplugged: DP-2 keeps its `#2`.
        let mut all = vec![all.remove(1)];
        number_ids(&mut all);
        assert_eq!(ids(&all), std::slice::from_ref(&second));
        // Plugged in again (or into DP-5), on or off: the number free.
        for enabled in [true, false] {
            let mut again = all.clone();
            again.push(head("DP-5", enabled));
            number_ids(&mut again);
            assert_eq!(ids(&again), [second.clone(), base.to_string()]);
        }
        // A new one beside one that is off with `X`: `X #2`, not `X`.
        let mut all = vec![head("DP-1", false), head("DP-2", true)];
        all[0].id = base.to_string();
        number_ids(&mut all);
        assert_eq!(ids(&all), [base.to_string(), second.clone()]);
    }

    /// (output-id, review round 3) The profile loop: two identical
    /// monitors with no serial, DP-1 off and DP-2 on at start, so DP-2 is
    /// `X` and DP-1 `X #2`. A profile `X` off, `X #2` on is applied: DP-1
    /// goes on and DP-2 off, at the heads, through the snapshots the
    /// service publishes. Neither id changes, so the same profile is
    /// chosen again and applying it changes nothing (it was applied every
    /// 1.5 s while a monitor turned on took the lowest number).
    #[test]
    fn a_profile_that_swaps_identical_monitors_is_applied_once() {
        let (a, _b) = UnixStream::pair().unwrap();
        let conn = Connection::from_socket(a).unwrap();
        let head = |name: &str, enabled: bool| HeadEntry {
            name: name.into(),
            description: format!("Acme Twin ({name})"),
            make: "Acme".into(),
            model: "Twin".into(),
            enabled,
            ..HeadEntry::new(ZwlrOutputHeadV1::inert(conn.backend().downgrade()))
        };
        let mut c = Client {
            heads: vec![head("DP-1", false), head("DP-2", true)],
            ..Client::default()
        };
        let state = |all: &[Output]| -> Vec<(String, String, bool)> {
            all.iter()
                .map(|o| (o.name.clone(), o.id.clone(), o.enabled))
                .collect()
        };
        let x = "Acme | Twin | Acme Twin".to_string();
        let x2 = format!("{x} #2");
        let first = c.snapshot();
        assert_eq!(
            state(&first),
            [
                ("DP-1".into(), x2.clone(), false),
                ("DP-2".into(), x.clone(), true)
            ]
        );
        // The profile, by id; applying it sets each head it names.
        let profile = [(x.clone(), false), (x2.clone(), true)];
        let apply = |c: &mut Client, all: &[Output]| -> bool {
            let mut changed = false;
            for (id, on) in &profile {
                let Some(o) = all.iter().find(|o| o.id == *id) else {
                    continue;
                };
                let h = c.heads.iter_mut().find(|h| h.name == o.name).unwrap();
                changed |= h.enabled != *on;
                h.enabled = *on;
            }
            changed
        };
        let mut all = first;
        assert!(apply(&mut c, &all), "the profile changes the monitors");
        for round in 0..3 {
            all = c.snapshot();
            assert_eq!(
                state(&all),
                [
                    ("DP-1".into(), x2.clone(), true),
                    ("DP-2".into(), x.clone(), false)
                ],
                "round {round}"
            );
            assert!(!apply(&mut c, &all), "applied again in round {round}");
        }
        // Both turned off and on again, the other one first: the same ids.
        c.heads.iter_mut().for_each(|h| h.enabled = false);
        let _ = c.snapshot();
        c.heads[1].enabled = true;
        let _ = c.snapshot();
        c.heads[0].enabled = true;
        let ids: Vec<String> = c.snapshot().into_iter().map(|o| o.id).collect();
        assert_eq!(ids, [x2, x]);
    }

    /// (output-id, review round 4) The ids are given to strand-surface
    /// only while a run is up: one stopped sees no monitor moved to
    /// another port, and its list would number that monitor ` #2` (a
    /// connector it left still holding its id). A run's new heads take
    /// the ids of the monitors shown first, then the last run's.
    #[test]
    fn ids_are_given_only_while_a_run_is_up() {
        let dell = "Dell Inc. | DELL U2720Q | Dell Inc. DELL U2720Q 6Y1ZW13";
        let out = |name: &str, id: &str| Output {
            name: name.into(),
            id: id.into(),
            ..Output::default()
        };
        share(&[out("eDP-1", "BOE"), out("DP-1", dell)]);
        assert_eq!(
            ids(),
            [("eDP-1".into(), "BOE".into()), ("DP-1".into(), dell.into())]
        );
        // The run stops (nobody reads `outputs`), or its manager goes.
        unshare();
        assert_eq!(ids(), Vec::<(String, String)>::new());
        // Meanwhile the Dell is moved to DP-2, where strand-surface shows
        // it (numbered by plug order, so plain): the next run's DP-2 takes
        // that, and the last run's DP-1 is only a fallback.
        shown("DP-2", Some(dell));
        shown("eDP-1", Some("BOE"));
        assert_eq!(
            remembered_ids(),
            [
                ("DP-2".into(), dell.into()),
                ("eDP-1".into(), "BOE".into()),
                ("DP-1".into(), dell.into())
            ]
        );
        shown("DP-2", None);
        shown("eDP-1", None);
        share(&[out("DP-2", dell)]);
        assert_eq!(ids(), [("DP-2".into(), dell.into())]);
        unshare();
    }

    /// (outputs-order) The outputs are listed by name, a number by its
    /// value, whatever order the compositor announced them in.
    #[test]
    fn outputs_are_listed_by_name() {
        let mut names = vec!["eDP-1", "DP-10", "HDMI-A-1", "DP-2", "DP-1", "DP-02"];
        names.sort_by(|a, b| by_name(a, b));
        assert_eq!(
            names,
            ["DP-1", "DP-2", "DP-02", "DP-10", "HDMI-A-1", "eDP-1"]
        );
        let (a, _b) = UnixStream::pair().unwrap();
        let conn = Connection::from_socket(a).unwrap();
        let head = |name: &str| HeadEntry {
            name: name.into(),
            description: format!("Headless {name}"),
            enabled: true,
            ..HeadEntry::new(ZwlrOutputHeadV1::inert(conn.backend().downgrade()))
        };
        // sway after a restart: the last made first.
        let mut c = Client {
            heads: vec![head("HEADLESS-3"), head("HEADLESS-2"), head("HEADLESS-10")],
            ..Client::default()
        };
        let all: Vec<String> = c.snapshot().into_iter().map(|o| o.name).collect();
        assert_eq!(all, ["HEADLESS-2", "HEADLESS-3", "HEADLESS-10"]);
        // Each head keeps its own id, not its place's.
        let ids: Vec<(String, String)> = c
            .heads
            .iter()
            .map(|h| (h.name.clone(), h.id.clone()))
            .collect();
        assert!(
            ids.iter().all(|(n, id)| id.ends_with(n.as_str())),
            "{ids:?}"
        );
    }

    /// (outputs-mode) A head that is on and names no current mode
    /// (Hyprland 0.57, for a mode not in its list) has the one its
    /// `wl_output` shows, and keeps it when a configuration changes
    /// another head; one off has none.
    #[test]
    fn a_head_on_with_no_current_mode_has_its_wl_outputs() {
        let (a, _b) = UnixStream::pair().unwrap();
        let conn = Connection::from_socket(a).unwrap();
        let head = |name: &str, enabled: bool| HeadEntry {
            name: name.into(),
            enabled,
            ..HeadEntry::new(ZwlrOutputHeadV1::inert(conn.backend().downgrade()))
        };
        let shows = OutputMode {
            width: 1280,
            height: 720,
            refresh: 60.0,
        };
        let wl = |global: u32, name: &str| WlOut {
            global,
            proxy: wl_output::WlOutput::inert(conn.backend().downgrade()),
            name: name.into(),
            mode: Some(shows.clone()),
        };
        let mut c = Client {
            heads: vec![head("NESTED-1", true), head("NESTED-2", false)],
            wl_outputs: vec![wl(7, "NESTED-1"), wl(8, "NESTED-2")],
            ..Client::default()
        };
        let modes: Vec<Option<OutputMode>> = c.snapshot().into_iter().map(|o| o.mode).collect();
        assert_eq!(modes, [Some(shows), None]);
        // A change to another head sends this one with the mode it shows,
        // not its preferred.
        assert_eq!(
            default_mode(&c, &c.heads[0]),
            ModeChoice::Custom(1280, 720, 60_000)
        );
        assert_eq!(default_mode(&c, &c.heads[1]), ModeChoice::Keep);
    }

    /// (outputs-resync) A head or mode event after a `done` with no
    /// `done` since makes a resync due after [`RESYNC_AFTER`]; a `done`
    /// takes it back, and the first state (no `done` yet) never does.
    #[test]
    fn a_change_with_no_done_is_resynced() {
        let (a, _b) = UnixStream::pair().unwrap();
        let conn = Connection::from_socket(a).unwrap();
        let mut c = Client {
            manager: Some(ZwlrOutputManagerV1::inert(conn.backend().downgrade())),
            ..Client::default()
        };
        let now = Instant::now();
        c.touched();
        assert_eq!(c.resync_at(), None, "the first state is not a change");
        c.serial = Some(1);
        c.touched();
        let at = c.resync_at().expect("a change with no done");
        assert!(!c.resync_due(now));
        assert!(c.resync_due(at + Duration::from_millis(1)));
        // A `done` in time: nothing to do.
        c.undone = None;
        assert!(!c.resync_due(at + RESYNC_AFTER * 4));
    }

    /// (service-ready, review round 1) The output manager going while
    /// the connection stays clears the outputs as not ready, and nothing
    /// shows them ready again until another manager answers.
    #[test]
    fn a_lost_manager_clears_its_outputs_unready() {
        let mut c = Client {
            synced: true,
            ..Client::default()
        };
        // No manager on this compositor: ready, with none.
        assert_eq!(c.publish(false), Publish::Shown(Vec::new(), false));
        assert_eq!(c.publish(true), Publish::Nothing);
        // The manager goes (finished, or its global removed).
        c.drop_manager();
        assert_eq!(c.publish(true), Publish::Cleared);
        assert_eq!(c.publish(false), Publish::Nothing, "not ready again");
        assert_eq!(c.publish(false), Publish::Nothing);
    }
}
