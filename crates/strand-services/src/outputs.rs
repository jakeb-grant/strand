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
//! the service binds for that, published again when that mode comes
//! after the manager's `done` (a monitor plugged in). (outputs-resync)
//! Hyprland 0.57's manager is right only when bound: a change comes as
//! the values from before it with no `done`, or as nothing at all, so
//! the service checks the heads against what each `wl_output` and its
//! xdg-output say once they settle after a change, and when they disagree
//! or a change came with no `done` for 250 ms it reads the outputs again
//! as a client bound now gets them ([`Client::resync`]). (outputs-apply)
//! It reads them on a short connection of its own ([`read_again`]),
//! never by binding a second manager on its own connection: Hyprland
//! 0.57 never ends a stopped manager while its client is connected, and
//! follows, for each monitor, the configuration of the oldest manager
//! that made one, so every configuration is made on the one manager the
//! service binds.
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
//! first. A configuration asked for while the heads may be behind (a
//! change not read yet) is made after reading the outputs again. The
//! compositor answers `succeeded` or `failed` (`outputs.failed(action,
//! error)`). (outputs-apply) `succeeded` is its word only, so an applied
//! configuration is `outputs.succeeded("apply")` once the `wl_output`s
//! and xdg-outputs show what it asked ([`Client::unlike`]: its scale as
//! sent, within [`SCALE_SLACK`]; a mirror, on with no `wl_output`, only
//! once it has one), with the heads read again and published first when
//! they may be behind, and `failed`
//! naming how they differ once they are quiet for [`CHECK_QUIET`] (at
//! most [`CHECK_FOR`]) without it; a test is `succeeded` at its answer.
//! `cancelled` (the outputs
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
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::{self, ZxdgOutputManagerV1},
    zxdg_output_v1::{self, ZxdgOutputV1},
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
/// before the outputs are read again ([`Client::resync`]), and how long
/// the `wl_output`s are let settle after a change before the heads are
/// checked against them ([`Client::disagrees`]).
const RESYNC_AFTER: Duration = Duration::from_millis(250);

/// (outputs-apply) How long a read of the outputs on a connection of its
/// own ([`read_again`]) may take before it is given up.
const READ_WAIT: Duration = Duration::from_secs(2);

/// (outputs-apply) How long the `wl_output`s must be quiet after a
/// configuration's `succeeded`, while they differ from what it asked,
/// before it is `failed` ([`Check`]).
const CHECK_QUIET: Duration = Duration::from_secs(1);

/// (outputs-apply) The longest an applied configuration is checked for
/// when the outputs keep changing ([`Check`]).
const CHECK_FOR: Duration = Duration::from_secs(5);

/// (outputs-resync) How far, in logical pixels, xdg-output's size may be
/// from a head's mode over its scale before they disagree (compositors
/// round it each their own way).
const SIZE_SLACK: f64 = 2.0;

/// (outputs-apply) How far the scale xdg-output's size shows (a mode
/// over its size in logical pixels) may be from the one asked or sent
/// before they disagree: two `wl_fixed` steps, for a compositor that
/// takes the scale it is sent to the nearest one giving whole logical
/// pixels (Hyprland 0.57's `CMonitor::applyMonitorRule` searches a 1/120
/// grid: 4/3, sent as 341/256, is 1.3333, 2880 wide on 3840, not 2883).
const SCALE_SLACK: f64 = 1.0 / 128.0;

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
    /// { … }`, `action` as written (`"apply"` or `"test"`). An apply is
    /// `succeeded` once the monitors show what it asked, and `outputs.all`
    /// shows it then.
    pub succeeded: Event<String>,
    /// A configuration failed: the compositor refused it (a mode the
    /// monitor cannot show), it said it applied it but the monitors did
    /// not change to it (the error says how they differ), it named no
    /// output or an impossible value, or the compositor offers no output
    /// configuration: `on outputs.failed(action, error) { … }`.
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
    /// (outputs-apply) The mode the last read on a connection of its own
    /// said it shows, when it is none of `modes` ([`Client::take_fresh`]).
    shown_mode: Option<OutputMode>,
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
            shown_mode: None,
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
    /// (outputs-apply) A client that reads the outputs once on a
    /// connection of its own ([`read_again`]): it binds the manager
    /// alone.
    probe: bool,
    /// (outputs-resync) When the current manager sent a head or mode
    /// event after its last `done` with no `done` since: Hyprland 0.57
    /// sends a change that way, its values one change behind, so the
    /// outputs are read again ([`Client::resync`]) once [`RESYNC_AFTER`]
    /// passes without a `done`.
    undone: Option<Instant>,
    /// (outputs-mode) The `wl_output`s, for the current mode of a head
    /// that names none, and (outputs-resync) to check the heads against.
    wl_outputs: Vec<WlOut>,
    /// (outputs-resync) xdg-output, for each `wl_output`'s logical place
    /// and size.
    xdg_manager: Option<ZxdgOutputManagerV1>,
    /// (outputs-resync, review round 4) When a `wl_output` last ended a
    /// batch of changes (its `done`) after the manager's first `done`:
    /// once [`RESYNC_AFTER`] passes the heads are checked against the
    /// `wl_output`s ([`Client::disagrees`]), since Hyprland 0.57 sends
    /// the manager nothing for some changes.
    shown_at: Option<Instant>,
    /// (outputs-apply) When a `wl_output` last came, went or ended a
    /// batch of changes: an applied configuration is checked against them
    /// once they are quiet ([`Check`]).
    outputs_at: Option<Instant>,
}

/// (outputs-mode) A `wl_output` as the outputs service binds it: its
/// connector name and current mode (`wl_output.mode` with `current`),
/// which every `wl_output` sends while its output is on; (outputs-resync)
/// its transform, and its xdg-output's logical place and size.
#[derive(Debug)]
struct WlOut {
    global: u32,
    proxy: wl_output::WlOutput,
    name: String,
    mode: Option<OutputMode>,
    transform: Option<wl_output::Transform>,
    xdg: Option<ZxdgOutputV1>,
    place: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
    /// Its name or mode changed since its last `done`.
    dirty: bool,
}

impl WlOut {
    fn new(global: u32, proxy: wl_output::WlOutput) -> WlOut {
        WlOut {
            global,
            proxy,
            name: String::new(),
            mode: None,
            transform: None,
            xdg: None,
            place: None,
            size: None,
            dirty: false,
        }
    }
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
        let remembered = match self.shared {
            true => remembered_ids(),
            false => Vec::new(),
        };
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
                                .and_then(|o| o.mode)
                        })
                        .or(h.shown_mode),
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

    /// (outputs-resync, outputs-apply) Reads the outputs again on a
    /// connection of its own ([`read_again`] at `path`), as a client
    /// bound now would, and takes what it says into the heads
    /// ([`Client::take_fresh`]): Hyprland 0.57's manager is right only
    /// when bound. The manager bound here stays the one every
    /// configuration is made on, never stopped, so no stopped manager
    /// piles up and Hyprland follows each configuration (it follows the
    /// oldest manager that configured a monitor). The heads keep their
    /// ids and the run stays ready; a read that fails keeps what was
    /// published, and the next change tries again.
    fn resync(&mut self, path: &std::path::Path) {
        self.undone = None;
        self.shown_at = None;
        if self.manager.is_none() {
            return;
        }
        match read_again(path, READ_WAIT) {
            Ok(fresh) => self.take_fresh(&fresh),
            Err(e) => log::warn!("outputs: reading the outputs again failed: {e}"),
        }
    }

    /// (outputs-apply) What a read on a connection of its own says of
    /// each head ([`read_again`]).
    fn fresh(&self) -> Vec<Fresh> {
        self.heads
            .iter()
            .filter(|h| !h.name.is_empty())
            .map(|h| Fresh {
                name: h.name.clone(),
                description: h.description.clone(),
                make: h.make.clone(),
                model: h.model.clone(),
                serial: h.serial.clone(),
                physical: h.physical,
                enabled: h.enabled,
                current: h
                    .current_mode
                    .as_ref()
                    .and_then(|id| self.modes.get(id))
                    .map(ModeEntry::public)
                    .filter(|m| m.width > 0 && m.height > 0),
                position: h.position,
                transform: h.transform,
                scale: h.scale,
                adaptive_sync: h.adaptive_sync,
            })
            .collect()
    }

    /// (outputs-apply) Takes a read's values into the heads of the same
    /// name, published at once: the head keeps its proxy, its modes and
    /// its id. Its current mode is its listed one of that size and rate,
    /// else [`HeadEntry::shown_mode`].
    fn take_fresh(&mut self, fresh: &[Fresh]) {
        for h in self.heads.iter_mut().filter(|h| !h.name.is_empty()) {
            let Some(f) = fresh.iter().find(|f| f.name == h.name) else {
                continue;
            };
            h.description.clone_from(&f.description);
            h.make.clone_from(&f.make);
            h.model.clone_from(&f.model);
            h.serial.clone_from(&f.serial);
            h.physical = f.physical;
            h.enabled = f.enabled;
            h.position = f.position;
            h.transform = f.transform;
            h.scale = f.scale;
            h.adaptive_sync = f.adaptive_sync;
            let listed = f.current.and_then(|m| {
                h.modes
                    .iter()
                    .find(|id| self.modes.get(id).is_some_and(|e| e.public() == m))
                    .cloned()
            });
            h.shown_mode = f.current.filter(|_| listed.is_none());
            h.current_mode = listed;
        }
        self.changed = true;
    }

    /// (outputs-resync) A resync is due: the manager sent a change and no
    /// `done` for [`RESYNC_AFTER`], or the `wl_output`s settled after a
    /// change and the heads disagree with them ([`Client::disagrees`]).
    /// A check that finds them agreeing is done with.
    fn resync_due(&mut self, now: Instant) -> bool {
        if self.manager.is_none() {
            return false;
        }
        let due = |t: Option<Instant>| t.is_some_and(|t| now >= t + RESYNC_AFTER);
        if due(self.undone) {
            return true;
        }
        if due(self.shown_at) {
            self.shown_at = None;
            if self.disagrees() {
                return true;
            }
        }
        false
    }

    /// (outputs-apply) The heads may be behind the outputs: a change came
    /// with no `done` yet, or the `wl_output`s changed since and disagree
    /// with them.
    fn stale(&self) -> bool {
        self.manager.is_some()
            && (self.undone.is_some() || self.shown_at.is_some() && self.disagrees())
    }

    /// When the next resync or check falls due, for the poll's timeout.
    fn resync_at(&self) -> Option<Instant> {
        match (self.undone, self.shown_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
        .map(|t| t + RESYNC_AFTER)
    }

    /// (outputs-resync, review round 4) A head on disagrees with what its
    /// `wl_output` and xdg-output say of it: its place, its mode's size
    /// or rate, its transform, or its size in logical pixels (its scale).
    /// What either side does not say is not compared ([`differs`]).
    fn disagrees(&self) -> bool {
        self.heads
            .iter()
            .filter(|h| h.enabled && !h.name.is_empty())
            .any(|h| {
                let Some(o) = self.wl_outputs.iter().find(|o| o.name == h.name) else {
                    return false;
                };
                let current = h
                    .current_mode
                    .as_ref()
                    .and_then(|id| self.modes.get(id))
                    .map(ModeEntry::public);
                differs(o, h.position, h.transform, current, h.scale).is_some()
            })
    }

    /// (outputs-apply) How the outputs differ from what a configuration
    /// asked (`None`: as asked, or nothing to tell by), from what each
    /// `wl_output` and its xdg-output say. A head asked off must have no
    /// `wl_output`; one asked on must have one, with its mode and (with
    /// xdg-output) its place and size in, that [`differs`] in nothing,
    /// unless it was on with none when asked (`unseen`, a mirror), which
    /// is compared only once it has one. A compositor whose `wl_output`s
    /// give no names (before version 4) tells nothing.
    fn unlike(&self, asked: &[Expect]) -> Option<String> {
        if self.wl_outputs.iter().all(|o| o.name.is_empty()) {
            return None;
        }
        asked.iter().find_map(|e| {
            let o = self.wl_outputs.iter().find(|o| o.name == e.name);
            match (e.enabled, o) {
                (false, None) => None,
                (false, Some(_)) => Some(format!("{} is still on", e.name)),
                (true, None) if e.unseen => None,
                (true, None) => Some(format!("{} is not on", e.name)),
                (true, Some(o)) if o.mode.is_none() => Some(format!("{} shows no mode", e.name)),
                (true, Some(o))
                    if self.xdg_manager.is_some() && (o.place.is_none() || o.size.is_none()) =>
                {
                    Some(format!("{} has no place in the layout", e.name))
                }
                (true, Some(o)) => differs(o, e.position, e.transform, e.mode, e.scale),
            }
        })
    }

    /// (outputs-resync, outputs-mode; review round 4) `wl_output`
    /// `global` ended a batch of changes. Once the manager has given its
    /// first `done`, the heads are checked against it when it settles,
    /// and a changed name or mode is published again (a monitor plugged
    /// in gets its `wl_output`'s mode after the manager's `done`), unless
    /// the manager's own change is still waiting for its `done`.
    fn output_done(&mut self, global: u32) {
        self.outputs_at = Some(Instant::now());
        let Some(o) = self.wl_outputs.iter_mut().find(|o| o.global == global) else {
            return;
        };
        let dirty = std::mem::take(&mut o.dirty);
        if self.manager.is_none() || self.serial.is_none() {
            return;
        }
        if dirty && self.undone.is_none() {
            self.changed = true;
        }
        self.shown_at = Some(Instant::now());
    }

    /// An xdg-output for `wl_output` `global`'s proxy, once both are bound.
    fn get_xdg_output(&mut self, global: u32, qh: &QueueHandle<Client>) {
        let Some(manager) = &self.xdg_manager else {
            return;
        };
        if let Some(o) = self
            .wl_outputs
            .iter_mut()
            .find(|o| o.global == global && o.xdg.is_none())
        {
            o.xdg = Some(manager.get_xdg_output(&o.proxy, qh, global));
        }
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

/// (outputs-apply) What a read on a connection of its own says of a head
/// ([`read_again`], [`Client::take_fresh`]).
#[derive(Clone, Debug, PartialEq)]
struct Fresh {
    name: String,
    description: String,
    make: String,
    model: String,
    serial: String,
    physical: (i32, i32),
    enabled: bool,
    current: Option<OutputMode>,
    position: (i32, i32),
    transform: wl_output::Transform,
    scale: f64,
    adaptive_sync: bool,
}

/// (outputs-apply) What a configuration asked of a head, to check the
/// outputs against once the compositor says it applied it
/// ([`Client::unlike`]). `mode` is `None` when no mode was sent; a
/// refresh of 0 is not compared. `scale` is as sent ([`sent_scale`]).
/// `unseen`: asked on, and on with no `wl_output` when it was made (a
/// mirror), so its having none tells nothing ([`expect`]).
#[derive(Clone, Debug, PartialEq)]
struct Expect {
    name: String,
    enabled: bool,
    mode: Option<OutputMode>,
    position: (i32, i32),
    transform: wl_output::Transform,
    scale: f64,
    unseen: bool,
}

/// (outputs-apply) An applied configuration the compositor answered
/// `succeeded`, until the outputs show it: `succeeded` once they agree
/// with it ([`Client::unlike`]), `failed` naming how they differ once
/// they are quiet for [`CHECK_QUIET`] after the answer, or after
/// [`CHECK_FOR`] in all.
#[derive(Debug)]
struct Check {
    action: String,
    asked: Vec<Expect>,
    since: Instant,
}

impl Check {
    /// When it is `failed` if the outputs still differ, given when they
    /// last changed.
    fn deadline(&self, outputs_at: Option<Instant>) -> Instant {
        let quiet = outputs_at.map_or(self.since, |t| t.max(self.since)) + CHECK_QUIET;
        quiet.min(self.since + CHECK_FOR)
    }
}

/// The schema's name of a transform.
fn transform_name(t: wl_output::Transform) -> &'static str {
    match OutputTransform::from_wl(t) {
        OutputTransform::Normal => "normal",
        OutputTransform::Rotate90 => "rotate_90",
        OutputTransform::Rotate180 => "rotate_180",
        OutputTransform::Rotate270 => "rotate_270",
        OutputTransform::Flipped => "flipped",
        OutputTransform::Flipped90 => "flipped_90",
        OutputTransform::Flipped180 => "flipped_180",
        OutputTransform::Flipped270 => "flipped_270",
    }
}

/// (outputs-resync, outputs-apply) How `wl_output` `o` and its
/// xdg-output differ from an output at `position` turned `transform`,
/// showing `mode` at `scale` (`None`: they agree): its place, its mode's
/// size or rate (within [`RATE_SLACK`]; a rate of 0 is not compared), its
/// transform, or its size in logical pixels, its mode (or, with none,
/// the one `o` shows) over the scale, within [`SIZE_SLACK`] or with the
/// scale it shows within [`SCALE_SLACK`]. What `o` does not say is not
/// compared.
fn differs(
    o: &WlOut,
    position: (i32, i32),
    transform: wl_output::Transform,
    mode: Option<OutputMode>,
    scale: f64,
) -> Option<String> {
    let name = &o.name;
    if let Some(p) = o.place.filter(|p| *p != position) {
        return Some(format!(
            "{name} is at {}, {}, not {}, {}",
            p.0, p.1, position.0, position.1
        ));
    }
    if let Some(t) = o.transform.filter(|t| *t != transform) {
        return Some(format!(
            "{name} is turned {}, not {}",
            transform_name(t),
            transform_name(transform)
        ));
    }
    if let (Some(c), Some(m)) = (mode, o.mode) {
        let mhz = |r: f64| (r * 1000.0).round() as i64;
        if (c.width, c.height) != (m.width, m.height)
            || c.refresh > 0.0
                && m.refresh > 0.0
                && (mhz(c.refresh) - mhz(m.refresh)).abs() > RATE_SLACK
        {
            return Some(format!(
                "{name} shows {}x{}@{}, not {}x{}@{}",
                m.width, m.height, m.refresh, c.width, c.height, c.refresh
            ));
        }
    }
    if let (Some(m), Some((w, h))) = (mode.or(o.mode), o.size)
        && scale > 0.0
    {
        let (mw, mh) = match transform {
            wl_output::Transform::_90
            | wl_output::Transform::_270
            | wl_output::Transform::Flipped90
            | wl_output::Transform::Flipped270 => (m.height, m.width),
            _ => (m.width, m.height),
        };
        let off = |px: i64, logical: i32| {
            (px as f64 / scale - f64::from(logical)).abs() > SIZE_SLACK
                && (logical <= 0 || (px as f64 / f64::from(logical) - scale).abs() > SCALE_SLACK)
        };
        if off(mw, w) || off(mh, h) {
            let shown = (scale * 100.0).round() / 100.0;
            return Some(format!(
                "{name} is {w}x{h} in the layout, not {mw}x{mh} at scale {shown}"
            ));
        }
    }
    None
}

/// (outputs-apply) The outputs as a client bound now gets them: a
/// connection of its own to the socket at `path` binding the output
/// manager alone, read up to its first `done` (within `wait`), then
/// closed, which ends the manager with it (Hyprland 0.57 keeps a stopped
/// manager until its client goes).
fn read_again(path: &std::path::Path, wait: Duration) -> Result<Vec<Fresh>, String> {
    let deadline = Instant::now() + wait;
    let stream = UnixStream::connect(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let conn = Connection::from_socket(stream).map_err(|e| e.to_string())?;
    let mut queue: EventQueue<Client> = conn.new_event_queue();
    let qh = queue.handle();
    let display = conn.display();
    let _registry = display.get_registry(&qh, ());
    display.sync(&qh, ());
    let mut c = Client {
        probe: true,
        ..Client::default()
    };
    loop {
        queue.dispatch_pending(&mut c).map_err(|e| e.to_string())?;
        if c.serial.is_some() {
            return Ok(c.fresh());
        }
        if c.synced && c.manager.is_none() {
            return Err("the compositor offers no output manager".into());
        }
        queue.flush().map_err(|e| e.to_string())?;
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(format!("no answer in {} ms", wait.as_millis()));
        }
        let readable = {
            let fd = guard.connection_fd();
            let mut fds = [PollFd::new(&fd, PollFlags::IN)];
            match rustix::event::poll(&mut fds, Some(&timespec(left))) {
                Ok(_) => !fds[0].revents().is_empty(),
                Err(rustix::io::Errno::INTR) => false,
                Err(e) => return Err(io::Error::from(e).to_string()),
            }
        };
        if readable {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.to_string()),
            }
        } else {
            drop(guard);
        }
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
        .and_then(|o| o.mode)
        .or(h.shown_mode)
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
    /// (outputs-apply) Answered `succeeded`, until the outputs show it.
    checking: Option<Check>,
}

/// The configuration in flight.
#[derive(Debug)]
struct InFlight {
    id: u64,
    config: ZwlrOutputConfigurationV1,
    request: Request,
    /// What it asked of each head.
    asked: Vec<Expect>,
}

/// Sends `request` as a configuration (number `id`), with what it asks of
/// each head: `Err` (why) when it is refused before it is sent.
fn send(
    client: &Client,
    qh: &QueueHandle<Client>,
    id: u64,
    request: &Request,
) -> Result<(ZwlrOutputConfigurationV1, Vec<Expect>), String> {
    let (Some(manager), Some(serial)) = (client.manager.as_ref(), client.serial) else {
        return Err(
            "the compositor offers no output configuration (zwlr_output_manager_v1)".into(),
        );
    };
    let plans = plan(client, request.action.changes())?;
    let config = manager.create_configuration(serial, qh, id);
    let mut asked = Vec::new();
    for p in plans {
        let Some(head) = client.heads.iter().find(|h| h.proxy.id() == p.head) else {
            continue;
        };
        asked.push(expect(client, head, &p));
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
    Ok((config, asked))
}

/// (outputs-apply) What plan `p` asks of `head`, to check once it is
/// applied: its mode as sent, and its scale as the protocol carries it
/// ([`sent_scale`]). A head on that has no `wl_output` when the
/// configuration is made is `unseen` (Hyprland 0.57 removes a mirrored
/// monitor's `wl_output`, `CProtocolManager::onMonitorModeChange`, and
/// sends its head as on).
fn expect(client: &Client, head: &HeadEntry, p: &HeadPlan) -> Expect {
    Expect {
        name: head.name.clone(),
        enabled: p.enabled,
        mode: match &p.mode {
            ModeChoice::Keep => None,
            ModeChoice::Listed(m) => client
                .modes
                .get(m)
                .map(ModeEntry::public)
                .filter(|m| m.width > 0 && m.height > 0),
            ModeChoice::Custom(w, h, r) => Some(OutputMode {
                width: i64::from(*w),
                height: i64::from(*h),
                refresh: f64::from(*r) / 1000.0,
            }),
        },
        position: p.position,
        transform: p.transform,
        scale: sent_scale(p.scale),
        unseen: p.enabled && head.enabled && !client.wl_outputs.iter().any(|o| o.name == head.name),
    }
}

/// (outputs-apply) Scale `s` as `set_scale` sends it: a `wl_fixed`,
/// which wayland-client truncates to a 1/256 step (`(s * 256.) as i32`).
fn sent_scale(s: f64) -> f64 {
    f64::from((s * 256.0) as i32) / 256.0
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
            let mut lost: Vec<String> = pending
                .checking
                .take()
                .map(|c| c.action)
                .into_iter()
                .chain(
                    pending
                        .in_flight
                        .take()
                        .map(|f| f.request.action.name().to_string()),
                )
                .chain(
                    pending
                        .waiting
                        .drain(..)
                        .map(|r| r.action.name().to_string()),
                )
                .collect();
            while let Ok(m) = cx.try_recv() {
                if let Msg::Action(a) = m {
                    lost.push(a.name().to_string());
                }
            }
            for a in lost {
                cx.emit(OutputsEvent::Failed((a, error.clone())));
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
        let mut client = Client {
            shared: true,
            ..Client::default()
        };
        let Pending {
            waiting,
            in_flight,
            checking,
        } = pending;
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
                    // (outputs-apply) Applied: `succeeded` once the
                    // outputs show it.
                    Outcome::Succeeded
                        if matches!(f.request.action, OutputsAction::Apply { .. }) =>
                    {
                        *checking = Some(Check {
                            action,
                            asked: f.asked,
                            since: Instant::now(),
                        });
                    }
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
            // (outputs-apply) An applied configuration, checked against the
            // outputs.
            if let Some(c) = checking.as_ref() {
                let now = Instant::now();
                match client.unlike(&c.asked) {
                    None => {
                        // The outputs show it while the heads may be
                        // behind (Hyprland's reload sends their old
                        // values): read them again and publish first, so
                        // a handler on `succeeded` reads `outputs.all` as
                        // applied.
                        if client.stale() {
                            log::debug!("outputs: an apply shown; reading the heads again");
                            client.resync(&path);
                            if let Publish::Shown(all, available) = client.publish(cx.is_ready())
                                && stopping.is_none()
                            {
                                cx.update(|s| {
                                    s.all = all;
                                    s.available = available;
                                });
                                cx.ready();
                            }
                        }
                        if let Some(c) = checking.take() {
                            cx.emit(OutputsEvent::Succeeded(c.action));
                        }
                    }
                    Some(why) if now >= c.deadline(client.outputs_at) => {
                        log::warn!(
                            "outputs: the compositor took no configuration it said it applied: {why}"
                        );
                        if let Some(c) = checking.take() {
                            cx.emit(OutputsEvent::Failed((
                                c.action,
                                format!(
                                    "the compositor said it applied the configuration, but {why}"
                                ),
                            )));
                        }
                    }
                    Some(_) => {}
                }
            }
            if let Some(deadline) = stopping
                && (in_flight.is_none() && checking.is_none() || Instant::now() >= deadline)
            {
                let _ = conn.flush();
                return Ok(());
            }
            let idle = in_flight.is_none() && checking.is_none() && stopping.is_none();
            // (outputs-resync) A change with no `done`, or heads behind
            // their outputs: read again, once no configuration is in
            // flight or being checked.
            if idle && client.resync_due(Instant::now()) {
                log::debug!("outputs: the heads are behind the outputs; reading them again");
                client.resync(&path);
                continue;
            }
            // The next configuration, once the first state is in (a
            // manager announced with no `done` yet is waited for).
            let settled = client.synced && (client.manager.is_none() || client.serial.is_some());
            // (outputs-apply) Made from heads that may be behind (a
            // change not read yet): read again first.
            if idle && settled && !waiting.is_empty() && client.stale() {
                log::debug!("outputs: reading the outputs again before a configuration");
                client.resync(&path);
            }
            while in_flight.is_none() && checking.is_none() && settled && stopping.is_none() {
                let Some(request) = waiting.pop_front() else {
                    break;
                };
                next_id += 1;
                match send(&client, &qh, next_id, &request) {
                    Ok((config, asked)) => {
                        *in_flight = Some(InFlight {
                            id: next_id,
                            config,
                            request,
                            asked,
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
                let resync = client
                    .resync_at()
                    .filter(|_| in_flight.is_none() && checking.is_none());
                let check = checking.as_ref().map(|c| c.deadline(client.outputs_at));
                let deadline = [stopping, resync, check].into_iter().flatten().min();
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
                state.manager = Some(registry.bind(name, version.min(VERSION), qh, ()));
                state.manager_global = Some(name);
                state.lost = false;
            }
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == wl_output::WlOutput::interface().name
                && version >= 4
                && !state.probe =>
            {
                state.outputs_at = Some(Instant::now());
                state
                    .wl_outputs
                    .push(WlOut::new(name, registry.bind(name, 4, qh, name)));
                state.get_xdg_output(name, qh);
            }
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == ZxdgOutputManagerV1::interface().name
                && state.xdg_manager.is_none()
                && !state.probe =>
            {
                state.xdg_manager = Some(registry.bind(name, version.min(3), qh, ()));
                let globals: Vec<u32> = state.wl_outputs.iter().map(|o| o.global).collect();
                for g in globals {
                    state.get_xdg_output(g, qh);
                }
            }
            wl_registry::Event::GlobalRemove { name } if state.manager_global == Some(name) => {
                if let Some(m) = &state.manager {
                    m.stop();
                }
                state.drop_manager();
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(i) = state.wl_outputs.iter().position(|o| o.global == name) {
                    state.outputs_at = Some(Instant::now());
                    let o = state.wl_outputs.remove(i);
                    if let Some(x) = o.xdg {
                        x.destroy();
                    }
                    if o.proxy.version() >= 3 {
                        o.proxy.release();
                    }
                }
            }
            _ => {}
        }
    }
}

/// (outputs-mode) A `wl_output`'s name and current mode; (outputs-resync)
/// its transform, and its `done`, which ends a batch of changes to it and
/// to its xdg-output (version 3).
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
            wl_output::Event::Name { name } => {
                o.dirty |= o.name != name;
                o.name = name;
            }
            wl_output::Event::Mode {
                flags: WEnum::Value(flags),
                width,
                height,
                refresh,
            } if flags.contains(wl_output::Mode::Current) => {
                let mode = Some(OutputMode {
                    width: i64::from(width),
                    height: i64::from(height),
                    refresh: f64::from(refresh) / 1000.0,
                });
                o.dirty |= o.mode != mode;
                o.mode = mode;
            }
            wl_output::Event::Geometry {
                transform: WEnum::Value(t),
                ..
            } => o.transform = Some(t),
            wl_output::Event::Done => state.output_done(*global),
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputManagerV1, ()> for Client {
    fn event(
        _: &mut Self,
        _: &ZxdgOutputManagerV1,
        _: zxdg_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// (outputs-resync) A `wl_output`'s logical place and size; its own
/// `done` before version 3.
impl Dispatch<ZxdgOutputV1, u32> for Client {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.wl_outputs.iter_mut().find(|o| o.global == *global) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => o.place = Some((x, y)),
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                o.size = Some((width, height));
            }
            zxdg_output_v1::Event::Done => state.output_done(*global),
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
        // A manager that is not the current one (stopped when its global
        // went): its heads are not this state's, and its end is not the
        // manager going.
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
        // A head of a manager no longer current, or one released.
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
            E::CurrentMode { mode } => {
                h.current_mode = Some(mode.id());
                h.shown_mode = None;
            }
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
            name: name.into(),
            mode: Some(shows),
            ..WlOut::new(
                global,
                wl_output::WlOutput::inert(conn.backend().downgrade()),
            )
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

    /// (outputs-resync, review round 4) Once the `wl_output`s settle
    /// after a change, the heads are checked against them: a place, mode
    /// size, transform or scale they disagree on makes a resync due, and
    /// heads that agree make none (sway, which sends its own change). A
    /// check is made once per change: the next waits for another `done`.
    #[test]
    fn heads_behind_their_outputs_are_resynced() {
        let (a, _b) = UnixStream::pair().unwrap();
        let conn = Connection::from_socket(a).unwrap();
        // DP-1 at 1920, 0, 3840x2160 at scale 2, as both sides say.
        let agreeing = || {
            let mode = ZwlrOutputModeV1::inert(conn.backend().downgrade());
            let mut c = Client {
                manager: Some(ZwlrOutputManagerV1::inert(conn.backend().downgrade())),
                serial: Some(1),
                heads: vec![HeadEntry {
                    name: "DP-1".into(),
                    enabled: true,
                    position: (1920, 0),
                    scale: 2.0,
                    current_mode: Some(mode.id()),
                    ..HeadEntry::new(ZwlrOutputHeadV1::inert(conn.backend().downgrade()))
                }],
                wl_outputs: vec![WlOut {
                    name: "DP-1".into(),
                    mode: Some(OutputMode {
                        width: 3840,
                        height: 2160,
                        refresh: 60.0,
                    }),
                    transform: Some(wl_output::Transform::Normal),
                    place: Some((1920, 0)),
                    size: Some((1920, 1080)),
                    ..WlOut::new(9, wl_output::WlOutput::inert(conn.backend().downgrade()))
                }],
                ..Client::default()
            };
            c.modes.insert(
                mode.id(),
                ModeEntry {
                    proxy: mode,
                    width: 3840,
                    height: 2160,
                    refresh: 60_000,
                    preferred: true,
                },
            );
            c
        };
        let due = |c: &Client| c.resync_at().expect("a check is due");
        let mut c = agreeing();
        assert!(!c.disagrees(), "they agree");
        c.output_done(9);
        let t = due(&c);
        assert!(!c.resync_due(t), "agreeing heads are not resynced");
        assert_eq!(c.resync_at(), None, "the check is done with");
        // Before the manager's first `done`, nothing is checked.
        c.serial = None;
        c.output_done(9);
        assert_eq!(c.resync_at(), None);
        // Each field the heads may lag on.
        type Lag = fn(&mut WlOut);
        let lag: [(&str, Lag); 4] = [
            ("place", |o| o.place = Some((0, 0))),
            ("mode", |o| {
                o.mode = Some(OutputMode {
                    width: 2560,
                    height: 1440,
                    refresh: 60.0,
                });
            }),
            ("transform", |o| {
                o.transform = Some(wl_output::Transform::_90)
            }),
            ("scale", |o| o.size = Some((3840, 2160))),
        ];
        for (what, f) in lag {
            let mut c = agreeing();
            f(&mut c.wl_outputs[0]);
            assert!(c.disagrees(), "{what}");
            c.output_done(9);
            let t = due(&c);
            assert!(
                !c.resync_due(t - Duration::from_millis(1)),
                "{what}: settling"
            );
            assert!(c.resync_due(t), "{what}: resynced");
        }
    }

    /// (outputs-mode, review round 4) A `wl_output` whose name or mode
    /// changes after the manager's `done` (a monitor plugged in: its head
    /// and the `done` come before its `wl_output` is bound) has the list
    /// published again at its own `done`; one that says nothing new does
    /// not, and neither does one while the manager's change waits for its
    /// `done`.
    #[test]
    fn a_wl_output_mode_after_the_done_is_published() {
        let (a, _b) = UnixStream::pair().unwrap();
        let conn = Connection::from_socket(a).unwrap();
        let mut c = Client {
            manager: Some(ZwlrOutputManagerV1::inert(conn.backend().downgrade())),
            serial: Some(1),
            synced: true,
            heads: vec![HeadEntry {
                name: "HOT-1".into(),
                enabled: true,
                ..HeadEntry::new(ZwlrOutputHeadV1::inert(conn.backend().downgrade()))
            }],
            wl_outputs: vec![WlOut::new(
                5,
                wl_output::WlOutput::inert(conn.backend().downgrade()),
            )],
            ..Client::default()
        };
        assert!(matches!(c.publish(false), Publish::Shown(..)));
        assert_eq!(c.publish(true), Publish::Nothing);
        c.wl_outputs[0].name = "HOT-1".into();
        c.wl_outputs[0].mode = Some(OutputMode {
            width: 1280,
            height: 720,
            refresh: 60.0,
        });
        c.wl_outputs[0].dirty = true;
        c.output_done(5);
        match c.publish(true) {
            Publish::Shown(all, _) => {
                assert_eq!(all[0].mode.map(|m| (m.width, m.height)), Some((1280, 720)))
            }
            p => panic!("published again: {p:?}"),
        }
        c.output_done(5);
        assert_eq!(c.publish(true), Publish::Nothing, "nothing new");
        c.wl_outputs[0].dirty = true;
        c.undone = Some(Instant::now());
        c.output_done(5);
        assert_eq!(
            c.publish(true),
            Publish::Nothing,
            "the manager's change waits"
        );
    }

    /// (outputs-apply) An applied configuration is checked against the
    /// `wl_output`s and their xdg-outputs: a head asked off has none, one
    /// asked on has one in its place, mode, transform and scale, and
    /// what is not in yet counts as not done. A compositor whose
    /// `wl_output`s give no names tells nothing.
    #[test]
    fn an_applied_configuration_is_checked_against_the_outputs() {
        let (a, _b) = UnixStream::pair().unwrap();
        let conn = Connection::from_socket(a).unwrap();
        let mode = OutputMode {
            width: 3840,
            height: 2160,
            refresh: 60.0,
        };
        let shown = || WlOut {
            name: "DP-1".into(),
            mode: Some(mode),
            transform: Some(wl_output::Transform::Normal),
            place: Some((1920, 0)),
            size: Some((1920, 1080)),
            ..WlOut::new(9, wl_output::WlOutput::inert(conn.backend().downgrade()))
        };
        let c = Client {
            xdg_manager: Some(ZxdgOutputManagerV1::inert(conn.backend().downgrade())),
            wl_outputs: vec![shown()],
            ..Client::default()
        };
        let asked = Expect {
            name: "DP-1".into(),
            enabled: true,
            mode: Some(mode),
            position: (1920, 0),
            transform: wl_output::Transform::Normal,
            scale: 2.0,
            unseen: false,
        };
        assert_eq!(c.unlike(std::slice::from_ref(&asked)), None, "as asked");
        let unlike = |e: Expect| c.unlike(&[e]).unwrap_or_default();
        assert_eq!(
            unlike(Expect {
                position: (0, 0),
                ..asked.clone()
            }),
            "DP-1 is at 1920, 0, not 0, 0"
        );
        assert_eq!(
            unlike(Expect {
                scale: 1.0,
                ..asked.clone()
            }),
            "DP-1 is 1920x1080 in the layout, not 3840x2160 at scale 1"
        );
        assert!(
            unlike(Expect {
                mode: Some(OutputMode {
                    width: 2560,
                    height: 1440,
                    refresh: 60.0
                }),
                ..asked.clone()
            })
            .starts_with("DP-1 shows 3840x2160@60, not 2560x1440@60")
        );
        assert_eq!(
            unlike(Expect {
                transform: wl_output::Transform::_90,
                ..asked.clone()
            }),
            "DP-1 is turned normal, not rotate_90"
        );
        // A rate within 0.5 Hz, or none asked, is the same mode.
        for refresh in [59.6, 0.0] {
            let e = Expect {
                mode: Some(OutputMode { refresh, ..mode }),
                ..asked.clone()
            };
            assert_eq!(c.unlike(&[e]), None, "{refresh}");
        }
        assert_eq!(
            unlike(Expect {
                enabled: false,
                ..asked.clone()
            }),
            "DP-1 is still on"
        );
        assert_eq!(
            unlike(Expect {
                name: "DP-2".into(),
                ..asked.clone()
            }),
            "DP-2 is not on"
        );
        let off = Expect {
            name: "DP-2".into(),
            enabled: false,
            ..asked.clone()
        };
        assert_eq!(c.unlike(&[off]), None, "off, and no wl_output");
        // (review round 5) On with no wl_output when asked (a mirror):
        // having none tells nothing.
        let mirror = Expect {
            name: "DP-2".into(),
            unseen: true,
            ..asked.clone()
        };
        assert_eq!(c.unlike(&[mirror]), None, "a mirror");
        // Not in yet: its mode, its place.
        let mut c = c;
        c.wl_outputs[0].place = None;
        assert_eq!(
            c.unlike(std::slice::from_ref(&asked)).as_deref(),
            Some("DP-1 has no place in the layout")
        );
        c.wl_outputs[0].mode = None;
        assert_eq!(
            c.unlike(std::slice::from_ref(&asked)).as_deref(),
            Some("DP-1 shows no mode")
        );
        c.wl_outputs[0].name.clear();
        assert_eq!(c.unlike(&[asked]), None, "no names: nothing to tell by");
    }

    /// (outputs-apply, review round 5) A fractional scale is checked as
    /// the protocol carried it and as the compositor may round it. sway
    /// takes 1.33 as sent, 340/256: 2891 wide on 3840, not 2887.
    /// Hyprland 0.57 takes 4/3, which it sends as 341/256 (and a profile
    /// saved from it asks), as 1.3333 for whole logical pixels: 2880, not
    /// 2883. A scale it moves further (1.5 to 4/3) is not the one asked.
    #[test]
    fn a_fractional_scale_is_checked_as_it_was_sent_and_rounded() {
        let (a, _b) = UnixStream::pair().unwrap();
        let conn = Connection::from_socket(a).unwrap();
        let mode = OutputMode {
            width: 3840,
            height: 2160,
            refresh: 60.0,
        };
        let shown = |size: (i32, i32)| WlOut {
            name: "DP-1".into(),
            mode: Some(mode),
            transform: Some(wl_output::Transform::Normal),
            place: Some((0, 0)),
            size: Some(size),
            ..WlOut::new(9, wl_output::WlOutput::inert(conn.backend().downgrade()))
        };
        let client = |size| Client {
            xdg_manager: Some(ZxdgOutputManagerV1::inert(conn.backend().downgrade())),
            wl_outputs: vec![shown(size)],
            ..Client::default()
        };
        let asked = |scale: f64| Expect {
            name: "DP-1".into(),
            enabled: true,
            mode: Some(mode),
            position: (0, 0),
            transform: wl_output::Transform::Normal,
            scale: sent_scale(scale),
            unseen: false,
        };
        assert_eq!(sent_scale(1.33), 340.0 / 256.0);
        assert_eq!(sent_scale(341.0 / 256.0), 341.0 / 256.0);
        let sway = client((2891, 1626));
        assert_eq!(sway.unlike(&[asked(1.33)]), None, "sway at 1.33");
        let hyprland = client((2880, 1620));
        assert_eq!(hyprland.unlike(&[asked(341.0 / 256.0)]), None, "4/3");
        assert_eq!(hyprland.unlike(&[asked(1.33)]), None, "1.33 as 4/3");
        assert_eq!(
            hyprland.unlike(&[asked(1.5)]).as_deref(),
            Some("DP-1 is 2880x1620 in the layout, not 3840x2160 at scale 1.5")
        );
        assert_eq!(
            sway.unlike(&[asked(1.25)]).as_deref(),
            Some("DP-1 is 2891x1626 in the layout, not 3840x2160 at scale 1.25")
        );
        // A head Hyprland sends at 341/256 agrees with its 2880.
        let mut c = client((2880, 1620));
        c.manager = Some(ZwlrOutputManagerV1::inert(conn.backend().downgrade()));
        let proxy = ZwlrOutputModeV1::inert(conn.backend().downgrade());
        let id = proxy.id();
        c.modes.insert(
            id.clone(),
            ModeEntry {
                proxy,
                width: 3840,
                height: 2160,
                refresh: 60_000,
                preferred: true,
            },
        );
        c.heads.push(HeadEntry {
            name: "DP-1".into(),
            enabled: true,
            current_mode: Some(id),
            transform: wl_output::Transform::Normal,
            scale: 341.0 / 256.0,
            ..HeadEntry::new(ZwlrOutputHeadV1::inert(conn.backend().downgrade()))
        });
        assert!(!c.disagrees(), "the head agrees with its xdg-output");
    }

    /// (outputs-apply, review round 5) What a configuration asks of a
    /// head: its scale as sent, and whether it is a head on with no
    /// `wl_output` (a Hyprland mirror), whose having none tells nothing.
    #[test]
    fn a_plan_is_checked_as_it_was_sent() {
        let (a, _b) = UnixStream::pair().unwrap();
        let conn = Connection::from_socket(a).unwrap();
        let head = |name: &str, enabled: bool| HeadEntry {
            name: name.into(),
            enabled,
            ..HeadEntry::new(ZwlrOutputHeadV1::inert(conn.backend().downgrade()))
        };
        let c = Client {
            wl_outputs: vec![WlOut {
                name: "DP-1".into(),
                ..WlOut::new(9, wl_output::WlOutput::inert(conn.backend().downgrade()))
            }],
            ..Client::default()
        };
        let plan = |enabled: bool| HeadPlan {
            head: ZwlrOutputHeadV1::inert(conn.backend().downgrade()).id(),
            enabled,
            mode: ModeChoice::Keep,
            position: (0, 0),
            transform: wl_output::Transform::Normal,
            scale: 1.33,
            adaptive_sync: false,
        };
        let e = expect(&c, &head("DP-1", true), &plan(true));
        assert_eq!(e.scale, 340.0 / 256.0, "as sent");
        assert!(!e.unseen, "it has its wl_output");
        assert!(
            expect(&c, &head("HDMI-A-1", true), &plan(true)).unseen,
            "a mirror"
        );
        assert!(
            !expect(&c, &head("HDMI-A-1", false), &plan(true)).unseen,
            "turned on"
        );
        assert!(
            !expect(&c, &head("HDMI-A-1", true), &plan(false)).unseen,
            "turned off"
        );
    }

    /// (outputs-apply) A check is `failed` only once the outputs are
    /// quiet for [`CHECK_QUIET`] after the answer, and at most
    /// [`CHECK_FOR`] after it.
    #[test]
    fn a_check_waits_for_the_outputs_to_be_quiet() {
        let since = Instant::now();
        let c = Check {
            action: "apply".into(),
            asked: Vec::new(),
            since,
        };
        assert_eq!(c.deadline(None), since + CHECK_QUIET);
        let earlier = since.checked_sub(Duration::from_secs(9)).unwrap_or(since);
        assert_eq!(c.deadline(Some(earlier)), since + CHECK_QUIET);
        let later = since + Duration::from_millis(700);
        assert_eq!(c.deadline(Some(later)), later + CHECK_QUIET);
        assert_eq!(c.deadline(Some(since + CHECK_FOR)), since + CHECK_FOR);
    }

    /// (outputs-apply) A read on a connection of its own is taken into
    /// the heads of the same name: each keeps its proxy, modes and id,
    /// its current mode is its listed one of that size and rate, else the
    /// one shown, and it is published.
    #[test]
    fn a_read_again_is_taken_into_the_heads() {
        let (a, _b) = UnixStream::pair().unwrap();
        let conn = Connection::from_socket(a).unwrap();
        let listed = ZwlrOutputModeV1::inert(conn.backend().downgrade());
        let head = HeadEntry {
            name: "DP-1".into(),
            id: "Dell | U2720Q | Dell U2720Q".into(),
            enabled: true,
            position: (0, 0),
            modes: vec![listed.id()],
            current_mode: Some(listed.id()),
            ..HeadEntry::new(ZwlrOutputHeadV1::inert(conn.backend().downgrade()))
        };
        let proxy = head.proxy.id();
        let mut c = Client {
            serial: Some(4),
            synced: true,
            heads: vec![head],
            ..Client::default()
        };
        c.modes.insert(
            listed.id(),
            ModeEntry {
                proxy: listed.clone(),
                width: 1920,
                height: 1080,
                refresh: 60_000,
                preferred: true,
            },
        );
        let fresh = |current: OutputMode| Fresh {
            name: "DP-1".into(),
            description: "Dell U2720Q (DP-1)".into(),
            make: "Dell".into(),
            model: "U2720Q".into(),
            serial: String::new(),
            physical: (600, 340),
            enabled: true,
            current: Some(current),
            position: (2560, 40),
            transform: wl_output::Transform::_90,
            scale: 1.5,
            adaptive_sync: false,
        };
        let custom = OutputMode {
            width: 1280,
            height: 720,
            refresh: 60.0,
        };
        c.take_fresh(&[
            fresh(custom),
            Fresh {
                name: "DP-9".into(),
                ..fresh(custom)
            },
        ]);
        let h = &c.heads[0];
        assert_eq!(h.proxy.id(), proxy);
        assert_eq!((h.position, h.scale), ((2560, 40), 1.5));
        assert_eq!(h.transform, wl_output::Transform::_90);
        assert_eq!((h.current_mode.clone(), h.shown_mode), (None, Some(custom)));
        assert_eq!(c.heads.len(), 1, "a head this manager has not sent waits");
        match c.publish(true) {
            Publish::Shown(all, _) => {
                assert_eq!((all[0].x, all[0].y), (2560, 40));
                assert_eq!(all[0].mode, Some(custom));
                assert_eq!(all[0].id, "Dell | U2720Q | Dell U2720Q");
            }
            p => panic!("published: {p:?}"),
        }
        assert_eq!(
            default_mode(&c, &c.heads[0]),
            ModeChoice::Custom(1280, 720, 60_000)
        );
        c.take_fresh(&[fresh(OutputMode {
            width: 1920,
            height: 1080,
            refresh: 60.0,
        })]);
        let h = &c.heads[0];
        assert_eq!(
            (h.current_mode.clone(), h.shown_mode),
            (Some(listed.id()), None)
        );
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
