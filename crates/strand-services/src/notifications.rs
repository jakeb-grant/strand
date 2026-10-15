//! `notifications`: the shell's own notification server
//! (`org.freedesktop.Notifications`, Desktop Notifications spec 1.2) as
//! a zbus `#[interface]` on a session-bus connection of its own, so the
//! name goes with the connection when the service stops.
//!
//! - `Notify` (`replaces_id`, actions, hints: `urgency`, `image-data`,
//!   `image-path`, `desktop-entry`, `resident`; `expire_timeout`),
//!   `CloseNotification`, `GetCapabilities`, `GetServerInformation`; the
//!   `NotificationClosed`, `ActionInvoked` and (M5) `ActivationToken`
//!   signals: an `invoke` or `activate` from a handler a press started
//!   sends the xdg-activation token for that press first, so the app
//!   that sent the notification may raise its window (the `activation`
//!   module; the token waits at most 200 ms, then the action goes
//!   without one).
//! - The store: `popups` (shown now), `all` (kept, at most [`KEPT`]),
//!   `count`, `dnd`, the `received` event; `n.expire()` ends a popup (the
//!   notification stays open in `all`: the server keeps notifications,
//!   `persistence`), `n.dismiss()` closes it (dismissed), `n.activate()`
//!   invokes its `default` action, `a.invoke()` one of its buttons,
//!   `clear()` closes every one. A notification is closed (one
//!   `NotificationClosed`) exactly when it leaves `all`; beyond [`KEPT`]
//!   the oldest closes as expired. The server sets no timers: the shell
//!   expires popups (`after n.timeout ?? 6s { n.expire() }`), so an idle
//!   server wakes nothing.
//! - `image-data` is checked and written as a PNG off the runtime thread
//!   ([`crate::pixmap`]), kept while its notification is. At most
//!   [`IMAGES_QUEUED`] pictures wait to be written; beyond that a sender's
//!   picture is left out (its notification still arrives), so a flood of
//!   large pictures cannot queue unbounded memory.
//! - Notifications belong to the run that received them: when the server
//!   stops (nobody reads `notifications` for 5 s) each one still open is
//!   closed (`NotificationClosed`, reason 3) before the name goes, so ids
//!   are never reused for a notification still shown.
//! - **History across restarts** (gap 35, decisions.md m6-service-gaps):
//!   while `remember` is on (the default), `all` is kept in the persist
//!   store (`services:notifications.all`, JSON, the newest [`KEPT`], a
//!   sender's `transient` ones left out, and only a sender's own
//!   `image-path`, never our runtime-dir pictures), saved 1 s after the
//!   last change off the runtime (the latest save wins), and once more as
//!   the run ends. A new run restores it before it asks for the name: the
//!   restored notifications are renumbered 1..=n below every new id, come
//!   back without actions (their senders cannot hear an answer) and
//!   without pictures that are gone, never as popups and without
//!   `received`; closing one sends no D-Bus signal, and `n.activate()`
//!   only dismisses it. Writing `remember = false` erases the file at once
//!   and keeps the choice (`services:notifications.remember`). Without a
//!   state directory ([`Config::state`] `None`) nothing is kept and a new
//!   run starts with none.
//! - **Another server owns the name** (dunst, mako, a desktop's): the
//!   name is asked for without queueing, and the run raises a notice
//!   naming the owner's process (`GetConnectionUnixProcessID`,
//!   `/proc/<pid>/comm`, and its systemd user unit when it has one:
//!   `GetUnitByPID`) and how to stop it (`kill <pid>`; `pkill -x` only
//!   for a command that is not an interpreter; a desktop's own shell is
//!   told to turn its notifications off instead). There is never a silent
//!   second server, and no polling: the run follows `NameOwnerChanged`
//!   and asks for the name again the moment it is free, then resolves
//!   the notice.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use chrono::{Datelike, Timelike};
use tokio::sync::mpsc;
use zbus::fdo::RequestNameReply;
use zbus::zvariant::OwnedValue;

use crate::{Call, Cx, Event, Msg, ServiceError, Store, service};

/// The schema the `notifications` service serves.
pub const SCHEMA: &str = strand_services_schema::NOTIFICATIONS;

/// The bus name the server owns.
pub const NAME: &str = "org.freedesktop.Notifications";
/// Its object path.
pub const PATH: &str = "/org/freedesktop/Notifications";

/// How many notifications `all` keeps: beyond it, the oldest closes as
/// expired. A bound on a client sending notifications in a loop, which
/// would otherwise grow memory without end, not the memory budget
/// (design.md: budgets are test targets, never limits on a shell): far
/// past any history a person reads (mako keeps 5, dunst 20).
pub const KEPT: usize = 1000;

/// How many `image-data` pictures may wait to be written: beyond it a
/// new notification's picture is left out.
pub const IMAGES_QUEUED: usize = 8;

/// The persist store path of the remembered notifications.
pub const HISTORY_PATH: &str = "services:notifications.all";
/// The persist store path of `remember` (`0` or `1`).
pub const REMEMBER_PATH: &str = "services:notifications.remember";
/// How long after the last change to `all` it is saved.
pub const SAVE_AFTER: Duration = Duration::from_millis(1000);
/// The longest a change to `all` waits to be saved: a sender replacing a
/// notification more often than [`SAVE_AFTER`] (a progress bar) does not
/// keep postponing the save, which a crash would then lose.
pub const SAVE_WITHIN: Duration = Duration::from_secs(5);
/// The remembered notifications' format.
const HISTORY_VERSION: u32 = 1;

/// Where a `notifications` service keeps its history.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    /// The persist store's directory (`None`: nothing is remembered).
    pub state: Option<PathBuf>,
}

impl Config {
    /// The persist store's directory from the environment
    /// (`$XDG_STATE_HOME/strand/persist`).
    pub fn from_env() -> Config {
        Config {
            state: strand_core::PersistStore::from_env()
                .ok()
                .map(|s| s.dir().to_path_buf()),
        }
    }
}

static CONFIG: Mutex<Option<Config>> = Mutex::new(None);

/// Use `config` instead of [`Config::from_env`] for services started
/// from now on (`None`: the environment again).
pub fn set_config(config: Option<Config>) {
    *CONFIG.lock().unwrap_or_else(PoisonError::into_inner) = config;
}

fn config() -> Config {
    CONFIG
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .unwrap_or_else(Config::from_env)
}

/// What `GetServerInformation` answers as the spec version: 1.2, with
/// its `ActivationToken` signal (M5; decisions.md m5-platform).
pub const SPEC_VERSION: &str = "1.2";

/// What `GetCapabilities` answers.
pub const CAPABILITIES: &[&str] = &[
    "actions",
    "body",
    "body-markup",
    "icon-static",
    "persistence",
];

/// A notification's urgency.
#[derive(crate::Data, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[data(name = "Urgency")]
pub enum Urgency {
    Low,
    #[default]
    Normal,
    Critical,
}

/// A `Date` (the clock's record): year, month, day, weekday (Monday 1),
/// and the time of day to the minute (null for a date alone).
#[derive(crate::Data, Clone, Debug, Default, PartialEq)]
#[data(name = "Date")]
pub struct Date {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    pub weekday: i64,
    pub hour: Option<i64>,
    pub minute: Option<i64>,
}

impl Date {
    /// Now, to the minute, in the local zone: a notification's `time`.
    pub fn now() -> Date {
        Date::at(chrono::Local::now().naive_local())
    }

    /// `t` to the minute.
    pub fn at(t: chrono::NaiveDateTime) -> Date {
        let d = t.date();
        Date {
            year: i64::from(d.year()),
            month: i64::from(d.month()),
            day: i64::from(d.day()),
            weekday: i64::from(d.weekday().number_from_monday()),
            hour: Some(i64::from(t.hour())),
            minute: Some(i64::from(t.minute())),
        }
    }
}

/// The app that sent a notification.
#[derive(crate::Data, Clone, Debug, Default, PartialEq)]
#[data(name = "NotificationApp")]
pub struct NotificationApp {
    pub name: String,
    pub icon: String,
}

/// A button a notification offers.
#[derive(crate::Data, Clone, Debug, Default, PartialEq)]
#[data(name = "NotificationAction", key = id)]
pub struct NotificationAction {
    pub id: String,
    pub label: String,
    pub notification: i64,
}

/// A desktop notification.
#[derive(crate::Data, Clone, Debug, Default, PartialEq)]
#[data(name = "Notification", key = id)]
pub struct Notification {
    pub id: i64,
    pub app: NotificationApp,
    pub summary: String,
    pub body: String,
    pub image: Option<String>,
    pub urgency: Urgency,
    pub timeout: Option<Duration>,
    pub persistent: bool,
    pub time: Date,
    pub actions: Vec<NotificationAction>,
}

/// `notifications`' actions.
#[derive(Call, Debug)]
pub enum NotificationsAction {
    /// `notifications.clear()`.
    Clear,
    /// `n.expire()`.
    Expire { item: Notification },
    /// `n.dismiss()`.
    Dismiss { item: Notification },
    /// `n.activate()`.
    Activate { item: Notification },
    /// `a.invoke()`.
    Invoke { item: NotificationAction },
}

/// See the module docs.
#[service(
    name = "notifications",
    action = NotificationsAction,
    activation = &["invoke", "activate"]
)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Notifications {
    /// The notifications to show now; each leaves when it expires or is
    /// dismissed.
    #[store(keyed)]
    pub popups: Vec<Notification>,
    /// Every notification kept, popups included: the newest 1,000 (older
    /// ones close as expired), remembered across restarts while `remember`
    /// is on.
    #[store(keyed)]
    pub all: Vec<Notification>,
    /// How many notifications are kept.
    pub count: i64,
    /// Do not disturb: new notifications that are not critical skip the
    /// popups (they are kept in `all`, and `received` still fires).
    /// Writable.
    #[store(rw)]
    pub dnd: bool,
    /// Notifications kept in `all` are remembered across restarts (under
    /// `$XDG_STATE_HOME/strand`, readable only by you); on by default.
    /// Those from an earlier run come back in `all` (never as popups, and
    /// without `received`) with no buttons, and `activate()` only dismisses
    /// them, since their senders cannot hear it. A sender's `transient`
    /// notifications are never remembered. Writable: false forgets every
    /// remembered notification at once, and stays false across restarts.
    #[store(rw)]
    pub remember: bool,
    /// A notification arrived. Events are lossless queues:
    /// `on notifications.received(n) { … }`.
    pub received: Event<Notification>,
}

/// Why a notification closed (the spec's `NotificationClosed` reasons).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Closed {
    Expired = 1,
    Dismissed = 2,
    Closed = 3,
}

/// The picture a `Notify` carried: `image-data` pixels (written as a PNG
/// by the body, off the runtime thread) or a path.
#[derive(Clone, Debug, Default)]
enum Image {
    #[default]
    None,
    Path(String),
    Data(ImageData),
}

/// `image-data`: width, height, rowstride, has alpha, bits, channels,
/// pixels.
type ImageData = (i32, i32, i32, bool, i32, i32, Vec<u8>);

/// What a sender's `Notify` asked for, as the store keeps it.
#[derive(Clone, Debug)]
struct Arrival {
    n: Notification,
    image: Image,
    /// It replaces the notification of the same id.
    replaces: bool,
    /// `resident`: kept after an action is invoked.
    resident: bool,
    /// It offers a `default` action (`n.activate()`).
    default: bool,
    /// `transient`: never remembered across restarts.
    transient: bool,
}

/// From the bus to the service body.
#[derive(Debug)]
enum FromBus {
    Notify(Box<Arrival>),
    Close(u32),
}

/// The ids alive, shared by the interface (which answers `Notify` with
/// an id at once) and the body.
#[derive(Debug, Default)]
struct Ids {
    next: u32,
    live: HashSet<u32>,
    /// The run ended: `Notify` and `CloseNotification` are refused (the
    /// name may still be ours while the closing signals go out).
    closed: bool,
}

/// The `org.freedesktop.Notifications` object.
struct Server {
    ids: Arc<Mutex<Ids>>,
    tx: mpsc::UnboundedSender<FromBus>,
    /// Pictures queued for the body, not written yet.
    images: Arc<AtomicUsize>,
}

/// A hint's text.
fn hint_text(hints: &HashMap<String, OwnedValue>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| {
        hints
            .get(*k)
            .and_then(|v| v.downcast_ref::<&str>().ok())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

/// The `image-data` hint (or its older names), else `image-path`.
fn image(hints: &HashMap<String, OwnedValue>) -> Image {
    let data = ["image-data", "image_data", "icon_data"]
        .iter()
        .find_map(|k| hints.get(*k))
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| ImageData::try_from(v).ok());
    match data {
        Some(d) => Image::Data(d),
        None => hint_text(hints, &["image-path", "image_path"])
            .map(Image::Path)
            .unwrap_or_default(),
    }
}

/// `image-data` as a PNG file, kept while the handle lives (run off the
/// runtime thread: the pixels may be large).
fn write_image(d: ImageData) -> Option<crate::pixmap::Pinned> {
    let (w, h, stride, alpha, bits, ch, data) = d;
    let written = crate::pixmap::from_image_data(w, h, stride, alpha, bits, ch, &data)
        .and_then(|(w, h, rgba)| crate::pixmap::write_rgba(w, h, &rgba));
    match written {
        Ok(p) => Some(p),
        Err(e) => {
            log::debug!("notifications: image-data not shown: {e}");
            None
        }
    }
}

impl Server {
    #[allow(clippy::too_many_arguments)]
    fn arrival(
        &self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> Option<Arrival> {
        let (id, replaces) = {
            let mut ids = self.ids.lock().unwrap_or_else(|e| e.into_inner());
            if ids.closed {
                return None;
            }
            if replaces_id != 0 && ids.live.contains(&replaces_id) {
                (replaces_id, true)
            } else {
                ids.next = ids.next.checked_add(1).unwrap_or(1).max(1);
                let id = ids.next;
                ids.live.insert(id);
                (id, false)
            }
        };
        // A byte by the spec; some senders use another integer type.
        let urgency = match crate::dbus::number(&hints, "urgency").map(|n| n as i64) {
            Some(0) => Urgency::Low,
            Some(2) => Urgency::Critical,
            _ => Urgency::Normal,
        };
        let icon = if app_icon.is_empty() {
            hint_text(&hints, &["desktop-entry"]).unwrap_or_default()
        } else {
            app_icon
        };
        let mut image = image(&hints);
        if let Image::Data(_) = image {
            // Bounded: a flood of pictures leaves the later ones out.
            if self.images.fetch_add(1, Ordering::AcqRel) >= IMAGES_QUEUED {
                self.images.fetch_sub(1, Ordering::AcqRel);
                log::debug!("notifications: too many pictures queued; one left out");
                image = hint_text(&hints, &["image-path", "image_path"])
                    .map(Image::Path)
                    .unwrap_or_default();
            }
        }
        let mut buttons = Vec::new();
        let mut default = false;
        for pair in actions.chunks(2) {
            let [key, label] = pair else {
                continue;
            };
            if key == "default" {
                default = true;
                continue;
            }
            buttons.push(NotificationAction {
                id: key.clone(),
                label: label.clone(),
                notification: i64::from(id),
            });
        }
        let resident = hints
            .get("resident")
            .and_then(|v| v.downcast_ref::<bool>().ok())
            .unwrap_or(false);
        // A boolean by the spec; some senders send an integer.
        let transient = hints
            .get("transient")
            .and_then(|v| v.downcast_ref::<bool>().ok())
            .or_else(|| crate::dbus::number(&hints, "transient").map(|n| n != 0.0))
            .unwrap_or(false);
        Some(Arrival {
            n: Notification {
                id: i64::from(id),
                app: NotificationApp {
                    name: app_name,
                    icon,
                },
                summary,
                body,
                image: match &image {
                    Image::Path(p) => Some(p.clone()),
                    _ => None,
                },
                urgency,
                // -1: the server's choice (null: the shell's); 0: never,
                // `persistent` (decisions.md, wave4-a2).
                timeout: (expire_timeout > 0).then(|| Duration::from_millis(expire_timeout as u64)),
                persistent: expire_timeout == 0,
                time: Date::now(),
                actions: buttons,
            },
            image,
            replaces,
            resident,
            default,
            transient,
        })
    }
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Server {
    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> zbus::fdo::Result<u32> {
        let stopped = || zbus::fdo::Error::Failed("the notification server is stopping".into());
        let a = self
            .arrival(
                app_name,
                replaces_id,
                app_icon,
                summary,
                body,
                actions,
                hints,
                expire_timeout,
            )
            .ok_or_else(stopped)?;
        let id = a.n.id as u32;
        let (fresh, data) = (!a.replaces, matches!(a.image, Image::Data(_)));
        if self.tx.send(FromBus::Notify(Box::new(a))).is_err() {
            // The body is gone: no id is handed out for a notification
            // nobody shows.
            if fresh && let Ok(mut ids) = self.ids.lock() {
                ids.live.remove(&id);
            }
            if data {
                self.images.fetch_sub(1, Ordering::AcqRel);
            }
            return Err(stopped());
        }
        Ok(id)
    }

    /// The spec: an id that does not exist (any more) is an error.
    async fn close_notification(&self, id: u32) -> zbus::fdo::Result<()> {
        let live = self
            .ids
            .lock()
            .map(|ids| !ids.closed && ids.live.contains(&id))
            .unwrap_or(false);
        if !live {
            return Err(zbus::fdo::Error::Failed(format!("no notification {id}")));
        }
        let _ = self.tx.send(FromBus::Close(id));
        Ok(())
    }

    async fn get_capabilities(&self) -> Vec<String> {
        CAPABILITIES.iter().map(|c| c.to_string()).collect()
    }

    #[zbus(out_args("name", "vendor", "version", "spec_version"))]
    async fn get_server_information(&self) -> (String, String, String, String) {
        (
            "strand".to_string(),
            "strand".to_string(),
            env!("CARGO_PKG_VERSION").to_string(),
            SPEC_VERSION.to_string(),
        )
    }
}

/// The process owning a name, for the diagnostic.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Owner {
    /// Its pid.
    pub pid: u32,
    /// Its command name (`/proc/<pid>/comm`).
    pub comm: Option<String>,
    /// The systemd user service running it, if one does (`mako.service`):
    /// a scope (a terminal's, the compositor's autostart) is not one.
    pub unit: Option<String>,
}

/// The systemd user service running `pid`, asked of the session bus's
/// systemd (`GetUnitByPID`, then the unit's `Id`), bounded by
/// [`crate::dbus::CALL_TIMEOUT`]. Only a `.service` that is not a D-Bus
/// activation's transient one counts: stopping a scope would stop the
/// terminal or the session it belongs to.
pub async fn systemd_unit(conn: &zbus::Connection, pid: u32) -> Option<String> {
    const SYSTEMD: &str = "org.freedesktop.systemd1";
    let reply = crate::dbus::timed(conn.call_method(
        Some(SYSTEMD),
        "/org/freedesktop/systemd1",
        Some("org.freedesktop.systemd1.Manager"),
        "GetUnitByPID",
        &(pid,),
    ))
    .await
    .ok()?;
    let path: zbus::zvariant::OwnedObjectPath = reply.body().deserialize().ok()?;
    let id = crate::dbus::timed(crate::dbus::get(
        conn,
        SYSTEMD,
        path.as_str(),
        "org.freedesktop.systemd1.Unit",
        "Id",
    ))
    .await
    .ok()?;
    let id = id.downcast_ref::<&str>().ok()?.to_string();
    (id.ends_with(".service") && !id.starts_with("dbus-")).then_some(id)
}

/// A desktop's own shell, whose notification server is part of it:
/// stopping the process ends the session.
fn desktop_shell(comm: &str) -> bool {
    matches!(
        comm,
        "gnome-shell" | "plasmashell" | "cinnamon" | "budgie-daemon" | "budgie-wm" | "gala"
    )
}

/// An interpreter: `pkill -x` would stop every script it runs.
fn interpreter(comm: &str) -> bool {
    let base = comm.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.' || c == '-');
    matches!(
        base,
        "python"
            | "perl"
            | "node"
            | "nodejs"
            | "ruby"
            | "sh"
            | "bash"
            | "dash"
            | "zsh"
            | "fish"
            | "gjs"
            | "lua"
            | "java"
            | "dotnet"
            | "mono"
    )
}

/// The diagnostic for a name another process owns.
pub fn conflict_message(owner: Option<Owner>) -> String {
    let who = match &owner {
        Some(Owner {
            pid,
            comm: Some(comm),
            ..
        }) => format!("`{comm}` (pid {pid})"),
        Some(Owner { pid, .. }) => format!("process {pid}"),
        None => "another process".to_string(),
    };
    let how = match &owner {
        Some(Owner {
            comm: Some(comm), ..
        }) if desktop_shell(comm) => format!(
            "turn its notifications off in its own settings, or run strand in a session \
             without `{comm}`"
        ),
        Some(Owner {
            unit: Some(unit), ..
        }) => format!(
            "stop it and keep it from starting again (`systemctl --user stop {unit}` and \
             `systemctl --user mask {unit}`)"
        ),
        Some(Owner {
            pid,
            comm: Some(comm),
            ..
        }) if !interpreter(comm) => format!(
            "stop it (`kill {pid}`, or `pkill -x {comm}` for every one) and remove it from \
             your compositor's autostart"
        ),
        Some(Owner { pid, .. }) => {
            format!("stop it (`kill {pid}`) and remove it from your compositor's autostart")
        }
        None => "stop the other notification daemon".to_string(),
    };
    format!(
        "another notification server, {who}, owns {NAME}: strand cannot show notifications \
         while it runs; {how}. D-Bus may start one again on the next notification unless \
         its activation is overridden (an empty \
         ~/.local/share/dbus-1/services/{NAME}.service). strand takes the name over once it \
         is free."
    )
}

/// The body's own bookkeeping besides the store.
#[derive(Debug, Default)]
struct Kept {
    resident: HashSet<i64>,
    default: HashSet<i64>,
    /// The `image-data` files of the notifications kept.
    images: HashMap<i64, crate::pixmap::Pinned>,
    /// The `transient` ones: never remembered.
    transient: HashSet<i64>,
    /// Those restored from an earlier run: their senders cannot hear of
    /// them (no D-Bus signal when they close).
    restored: HashSet<i64>,
}

impl Notifications {
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let conn = match crate::bus::own_session(cx.buses()).await {
            Ok(c) => c,
            Err(e) => return crate::dbus::idle_without_bus(&mut cx, "session", e).await,
        };
        let ids = Arc::new(Mutex::new(Ids::default()));
        let images = Arc::new(AtomicUsize::new(0));
        let (tx, mut rx) = mpsc::unbounded_channel();
        conn.object_server()
            .at(
                PATH,
                Server {
                    ids: ids.clone(),
                    tx,
                    images: images.clone(),
                },
            )
            .await?;
        // The notifications of an earlier run were closed with it: this
        // server starts with those it remembers (restored, numbered
        // 1..=n), else none, its new ids above them. Restored before the
        // name is asked for, so no id is handed out before they are
        // numbered.
        let mut history = History::new(config().state);
        let mut kept = Kept::default();
        let (remember, restored) = history.load().await;
        if let Ok(mut i) = ids.lock() {
            i.next = u32::try_from(restored.len()).unwrap_or(u32::MAX);
        }
        kept.restored = restored.iter().map(|n| n.id).collect();
        let started = cx.update(|s| {
            s.count = restored.len() as i64;
            s.all = restored;
            s.popups.clear();
            s.remember = remember;
        });
        if !started {
            return Ok(());
        }
        // What is not saved yet is saved as the run ends, however it ends.
        let _saving = SaveOnStop(history.shared.clone());
        // Whoever owns the name, followed before it is asked for: the
        // other server leaving is heard, not polled for.
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender("org.freedesktop.DBus")?
            .interface("org.freedesktop.DBus")?
            .member("NameOwnerChanged")?
            .arg(0, NAME)?
            .build();
        let mut owners = zbus::MessageStream::for_match_rule(rule, &conn, Some(8)).await?;
        let mut noticed = false;
        let flags = zbus::fdo::RequestNameFlags::DoNotQueue;
        loop {
            match conn.request_name_with_flags(NAME, flags.into()).await {
                Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => break,
                // zbus says `NameTaken` for an `Exists` reply.
                Ok(RequestNameReply::Exists | RequestNameReply::InQueue)
                | Err(zbus::Error::NameTaken) => {
                    let owner = match crate::dbus::owner_process(&conn, NAME).await {
                        Some((pid, comm)) => Some(Owner {
                            pid,
                            comm,
                            unit: systemd_unit(&conn, pid).await,
                        }),
                        None => None,
                    };
                    // The notice before readiness: a ready run without
                    // one resolves the last (as a clean stop does).
                    cx.notice(conflict_message(owner));
                    cx.ready();
                    noticed = true;
                    // No second server, and no polling: the name is
                    // asked for again once its owner lets it go.
                    if !name_freed(&mut cx, &mut owners, &mut history, &kept).await? {
                        return Ok(());
                    }
                }
                Err(e) => {
                    cx.ready();
                    return Err(ServiceError(format!("{NAME} not owned: {e}")));
                }
            }
        }
        drop(owners);
        if noticed && !cx.resolve() {
            return Ok(());
        }
        cx.ready();
        // However the run ends (stopped, which drops this body, or
        // failed), the notifications still open close with it, before
        // the name goes.
        let _closing = CloseOnStop {
            conn: conn.clone(),
            ids: ids.clone(),
        };
        loop {
            // `all` changed: saved a moment after the last change.
            let mut touched = false;
            tokio::select! {
                () = &mut history.due, if history.armed => {
                    history.armed = false;
                    history.flush();
                }
                m = rx.recv() => {
                    touched = true;
                    let Some(m) = m else {
                        return Err(ServiceError("the notification server stopped".into()));
                    };
                    match m {
                        FromBus::Notify(a) => {
                            let Arrival { mut n, image, mut replaces, resident, default, transient } = *a;
                            // It replaced one the body closed meanwhile: a
                            // new notification under that id, open again.
                            if replaces && !cx.state().all.iter().any(|m| m.id == n.id) {
                                replaces = false;
                                if let Ok(mut ids) = ids.lock() {
                                    ids.live.insert(n.id as u32);
                                }
                            }
                            if let Image::Data(d) = image {
                                // Off the runtime thread: other services go on.
                                let pinned = tokio::task::spawn_blocking(move || write_image(d))
                                    .await
                                    .ok()
                                    .flatten();
                                images.fetch_sub(1, Ordering::AcqRel);
                                n.image = pinned.as_ref().map(crate::pixmap::Pinned::text);
                                match pinned {
                                    Some(p) => kept.images.insert(n.id, p),
                                    None => kept.images.remove(&n.id),
                                };
                            } else {
                                kept.images.remove(&n.id);
                            }
                            if resident { kept.resident.insert(n.id); } else { kept.resident.remove(&n.id); }
                            if default { kept.default.insert(n.id); } else { kept.default.remove(&n.id); }
                            if transient { kept.transient.insert(n.id); } else { kept.transient.remove(&n.id); }
                            // A replacement updates a popup shown; a new one
                            // shows unless do-not-disturb holds it back.
                            let shown = (replaces && cx.state().popups.iter().any(|m| m.id == n.id))
                                || !cx.state().dnd
                                || n.urgency == Urgency::Critical;
                            let sent = cx.update(|s| {
                                arrive(&mut s.all, n.clone());
                                if shown {
                                    arrive(&mut s.popups, n.clone());
                                }
                                s.count = s.all.len() as i64;
                            });
                            if !sent || !cx.emit(NotificationsEvent::Received(n)) {
                                return Ok(());
                            }
                            // History is bounded: the oldest beyond it close.
                            let over: Vec<i64> = {
                                let all = &cx.state().all;
                                all.iter().take(all.len().saturating_sub(KEPT)).map(|n| n.id).collect()
                            };
                            for id in over {
                                if !close(&mut cx, &conn, &ids, &mut kept, id, Closed::Expired).await {
                                    return Ok(());
                                }
                            }
                        }
                        FromBus::Close(id) => {
                            if !close(&mut cx, &conn, &ids, &mut kept, i64::from(id), Closed::Closed).await {
                                return Ok(());
                            }
                        }
                    }
                }
                m = cx.recv() => match m {
                    // Stopped: `_closing` closes what is still open.
                    None => return Ok(()),
                    Some(Msg::Write(w)) if w.field == "dnd" => {
                        let on: bool = w.value().unwrap_or(cx.state().dnd);
                        if !cx.report(&w, |s| s.dnd = on) {
                            return Ok(());
                        }
                    }
                    Some(Msg::Write(w)) if w.field == "remember" => {
                        if !write_remember(&mut cx, &w, &mut history, &kept) {
                            return Ok(());
                        }
                    }
                    Some(Msg::Action(a)) => {
                        touched = !matches!(a, NotificationsAction::Expire { .. });
                        // (M5) Sent before `ActionInvoked` (spec 1.2).
                        let token = cx.activation_token().map(str::to_owned);
                        let alive = match a {
                            NotificationsAction::Clear => {
                                let all: Vec<i64> = cx.state().all.iter().map(|n| n.id).collect();
                                let mut alive = true;
                                for id in all {
                                    alive &= close(&mut cx, &conn, &ids, &mut kept, id, Closed::Dismissed).await;
                                }
                                alive
                            }
                            // The popup ends; the notification stays open in
                            // `all` (closed once, when it leaves it).
                            NotificationsAction::Expire { item } => {
                                cx.update(|s| s.popups.retain(|n| n.id != item.id))
                            }
                            NotificationsAction::Dismiss { item } => {
                                close(&mut cx, &conn, &ids, &mut kept, item.id, Closed::Dismissed).await
                            }
                            NotificationsAction::Activate { item } => {
                                if kept.default.contains(&item.id) {
                                    invoked(&conn, item.id, "default", token.as_deref()).await;
                                }
                                kept.resident.contains(&item.id)
                                    || close(&mut cx, &conn, &ids, &mut kept, item.id, Closed::Dismissed).await
                            }
                            NotificationsAction::Invoke { item } => {
                                // A restored one's sender cannot hear it.
                                if !kept.restored.contains(&item.notification)
                                    && cx.state().all.iter().any(|n| n.id == item.notification)
                                {
                                    invoked(&conn, item.notification, &item.id, token.as_deref()).await;
                                }
                                kept.resident.contains(&item.notification)
                                    || close(&mut cx, &conn, &ids, &mut kept, item.notification, Closed::Dismissed).await
                            }
                        };
                        if !alive {
                            return Ok(());
                        }
                    }
                    Some(_) => {}
                }
            }
            if touched {
                history.changed(cx.state(), &kept);
            }
        }
    }
}

/// Wait for the other server to let [`NAME`] go (`NameOwnerChanged` to
/// no owner), answering `dnd` and `remember` writes meanwhile. `false`
/// once stopped.
async fn name_freed(
    cx: &mut Cx<Notifications>,
    owners: &mut zbus::MessageStream,
    history: &mut History,
    kept: &Kept,
) -> Result<bool, ServiceError> {
    use futures_lite::StreamExt;
    loop {
        tokio::select! {
            m = owners.next() => {
                let Some(m) = m else {
                    return Err(ServiceError("the session bus went away".into()));
                };
                let freed = m
                    .ok()
                    .and_then(|m| m.body().deserialize::<(String, String, String)>().ok())
                    .is_some_and(|(_, _, new)| new.is_empty());
                if freed {
                    return Ok(true);
                }
            }
            m = cx.recv() => match m {
                None => return Ok(false),
                Some(Msg::Write(w)) if w.field == "dnd" => {
                    let on: bool = w.value().unwrap_or(cx.state().dnd);
                    if !cx.report(&w, |s| s.dnd = on) {
                        return Ok(false);
                    }
                }
                Some(Msg::Write(w)) if w.field == "remember" => {
                    if !write_remember(cx, &w, history, kept) {
                        return Ok(false);
                    }
                }
                Some(_) => {}
            }
        }
    }
}

/// Closes every notification still open when the run ends: a stopped
/// body is dropped, not polled to its end, so this is a drop guard. The
/// signals go out in a task holding the connection, so the name is
/// released only after them.
struct CloseOnStop {
    conn: zbus::Connection,
    ids: Arc<Mutex<Ids>>,
}

impl Drop for CloseOnStop {
    fn drop(&mut self) {
        // First no new notification is taken (an id handed out now
        // would name one nobody shows), then those open are closed.
        let mut open: Vec<u32> = match self.ids.lock() {
            Ok(mut ids) => {
                ids.closed = true;
                ids.live.drain().collect()
            }
            Err(_) => return,
        };
        if open.is_empty() {
            return;
        }
        open.sort_unstable();
        let conn = self.conn.clone();
        // Sent after the body is gone, and waited for (bounded) when the
        // services' thread ends: `Services::shutdown` closes them too.
        crate::client::finalize(async move {
            let _ = conn.object_server().remove::<Server, _>(PATH).await;
            for id in open {
                signal_closed(&conn, i64::from(id), Closed::Closed).await;
            }
        });
    }
}

/// Writing `remember`: false erases the remembered notifications now
/// and keeps the choice; true keeps the choice and saves `all`. Without
/// a state directory it stays false. `false` once stopped.
fn write_remember(
    cx: &mut Cx<Notifications>,
    w: &crate::Write,
    history: &mut History,
    kept: &Kept,
) -> bool {
    let on: bool = w.value().unwrap_or(cx.state().remember);
    let on = on && history.shared.dir.is_some();
    if !cx.report(w, |s| s.remember = on) {
        return false;
    }
    history.remember(on, cx.state(), kept);
    true
}

/// One remembered notification, as the history file holds it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Saved {
    app_name: String,
    app_icon: String,
    summary: String,
    body: String,
    #[serde(default)]
    image: Option<String>,
    urgency: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    persistent: bool,
    time: SavedTime,
}

/// A notification's `time`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct SavedTime {
    year: i64,
    month: i64,
    day: i64,
    weekday: i64,
    hour: Option<i64>,
    minute: Option<i64>,
}

/// The history file: its version, then the notifications oldest first.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct SavedHistory {
    version: u32,
    notifications: Vec<Saved>,
}

/// What to remember of `all`: the newest [`KEPT`] that are not
/// `transient`, oldest first; an image only when it is a sender's own
/// (`ours` names those whose picture we wrote from `image-data`: a file
/// of this run, gone after it).
fn history_text(all: &[Notification], transient: &HashSet<i64>, ours: &HashSet<i64>) -> String {
    let kept: Vec<&Notification> = all.iter().filter(|n| !transient.contains(&n.id)).collect();
    let from = kept.len().saturating_sub(KEPT);
    let notifications = kept[from..]
        .iter()
        .map(|n| Saved {
            app_name: n.app.name.clone(),
            app_icon: n.app.icon.clone(),
            summary: n.summary.clone(),
            body: n.body.clone(),
            image: n.image.clone().filter(|_| !ours.contains(&n.id)),
            urgency: match n.urgency {
                Urgency::Low => "low",
                Urgency::Normal => "normal",
                Urgency::Critical => "critical",
            }
            .to_string(),
            timeout_ms: n
                .timeout
                .map(|t| u64::try_from(t.as_millis()).unwrap_or(u64::MAX)),
            persistent: n.persistent,
            time: SavedTime {
                year: n.time.year,
                month: n.time.month,
                day: n.time.day,
                weekday: n.time.weekday,
                hour: n.time.hour,
                minute: n.time.minute,
            },
        })
        .collect();
    serde_json::to_string(&SavedHistory {
        version: HISTORY_VERSION,
        notifications,
    })
    .unwrap_or_default()
}

/// The notifications a history file holds, numbered 1..=n oldest first,
/// without actions, and without an image whose file `exists` says is
/// gone (an icon name, not a path, is kept). An error for a file that is
/// not this version's JSON.
fn restore(text: &str, exists: impl Fn(&Path) -> bool) -> Result<Vec<Notification>, String> {
    let saved: SavedHistory = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if saved.version != HISTORY_VERSION {
        return Err(format!(
            "version {} is not {HISTORY_VERSION}",
            saved.version
        ));
    }
    let picture = |image: String| {
        let path = match image.strip_prefix("file://") {
            Some(rest) => file_url_path(rest),
            None => PathBuf::from(&image),
        };
        if path.is_absolute() && !exists(&path) {
            return None;
        }
        Some(image)
    };
    Ok(saved
        .notifications
        .into_iter()
        .zip(1i64..)
        .map(|(s, id)| Notification {
            id,
            app: NotificationApp {
                name: s.app_name,
                icon: s.app_icon,
            },
            summary: s.summary,
            body: s.body,
            image: s.image.and_then(picture),
            urgency: match s.urgency.as_str() {
                "low" => Urgency::Low,
                "critical" => Urgency::Critical,
                _ => Urgency::Normal,
            },
            timeout: s.timeout_ms.map(Duration::from_millis),
            persistent: s.persistent,
            time: Date {
                year: s.time.year,
                month: s.time.month,
                day: s.time.day,
                weekday: s.time.weekday,
                hour: s.time.hour,
                minute: s.time.minute,
            },
            actions: Vec::new(),
        })
        .collect())
}

/// The file a `file://` URL's rest names, percent-decoded as the
/// renderer reads it (`file:///home/me/My%20Pictures/cat.png` is
/// `/home/me/My Pictures/cat.png`); a `%` not followed by two hex digits
/// is kept as written.
fn file_url_path(rest: &str) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    let b = rest.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(hex) = rest.get(i + 1..i + 3)
            && let Ok(v) = u8::from_str_radix(hex, 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    PathBuf::from(std::ffi::OsString::from_vec(out))
}

/// The history's next save: `remember`, and what `all` held (`None`:
/// the file is removed).
#[derive(Debug)]
struct Pending {
    generation: u64,
    remember: bool,
    all: Option<Snapshot>,
}

/// `all` as it was, with what saving it needs.
#[derive(Debug)]
struct Snapshot {
    all: Vec<Notification>,
    transient: HashSet<i64>,
    ours: HashSet<i64>,
}

/// Every save of this process is numbered: the latest wins.
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// The latest generation written (held while writing, so saves run one
/// at a time and an older one finishing last never lands).
static WRITTEN: Mutex<u64> = Mutex::new(0);

/// What the run and its drop guard share.
#[derive(Debug)]
struct HistoryShared {
    dir: Option<PathBuf>,
    /// The save not yet made.
    pending: Mutex<Option<Pending>>,
}

impl HistoryShared {
    /// Make the pending save, if any (blocking).
    fn save_pending(&self) {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let (Some(dir), Some(p)) = (&self.dir, pending) {
            save(dir, p);
        }
    }
}

/// Write `p` under `dir` unless a later save was written already.
fn save(dir: &Path, p: Pending) {
    let mut written = WRITTEN.lock().unwrap_or_else(PoisonError::into_inner);
    if *written >= p.generation {
        return;
    }
    let store = strand_core::PersistStore::new(dir);
    let flag: &[u8] = if p.remember { b"1" } else { b"0" };
    if let Err(e) = store.save(REMEMBER_PATH, b"1", flag) {
        log::warn!("notifications: `remember` not kept: {e}");
    }
    let r = match p.all {
        Some(s) => store.save(
            HISTORY_PATH,
            b"",
            history_text(&s.all, &s.transient, &s.ours).as_bytes(),
        ),
        None => store.remove(HISTORY_PATH),
    };
    if let Err(e) = r {
        log::warn!("notifications: the history not kept: {e}");
    }
    *written = p.generation;
}

/// The run's side of the history: the debounced save.
struct History {
    shared: Arc<HistoryShared>,
    due: std::pin::Pin<Box<tokio::time::Sleep>>,
    armed: bool,
    /// While `armed`: when the save must be made at the latest
    /// ([`SAVE_WITHIN`] after the first change it holds).
    latest: tokio::time::Instant,
}

impl History {
    fn new(dir: Option<PathBuf>) -> History {
        History {
            shared: Arc::new(HistoryShared {
                dir,
                pending: Mutex::new(None),
            }),
            due: Box::pin(tokio::time::sleep(Duration::ZERO)),
            armed: false,
            latest: tokio::time::Instant::now(),
        }
    }

    /// `remember` and the remembered notifications (none while it is
    /// off), read off the runtime. A history that does not read is
    /// logged and starts empty; the next save replaces it.
    async fn load(&self) -> (bool, Vec<Notification>) {
        let Some(dir) = self.shared.dir.clone() else {
            return (false, Vec::new());
        };
        tokio::task::spawn_blocking(move || {
            // After any save still being written (a run that just ended).
            let _written = WRITTEN.lock().unwrap_or_else(PoisonError::into_inner);
            // Read-only: a file that does not read is left where it is
            // rather than set aside as a `.corrupt` copy the store keeps
            // for its retention, since the history holds notification
            // bodies; the next save replaces it, `remember = false`
            // removes it.
            let store = strand_core::PersistStore::read_only(&dir);
            let remember = match store.load(REMEMBER_PATH) {
                Ok(Some(v)) => v.value != b"0",
                Ok(None) => true,
                Err(e) => {
                    log::warn!("notifications: `remember` not read: {e}");
                    true
                }
            };
            if !remember {
                return (false, Vec::new());
            }
            let restored = match store.load(HISTORY_PATH) {
                Ok(Some(v)) => restore(&String::from_utf8_lossy(&v.value), Path::exists)
                    .unwrap_or_else(|e| {
                        log::warn!("notifications: the history not read: {e}");
                        Vec::new()
                    }),
                Ok(None) => Vec::new(),
                Err(e) => {
                    log::warn!("notifications: the history not read: {e}");
                    Vec::new()
                }
            };
            (true, restored)
        })
        .await
        .unwrap_or_default()
    }

    fn snapshot(state: &Notifications, kept: &Kept) -> Snapshot {
        Snapshot {
            all: state.all.clone(),
            transient: kept.transient.clone(),
            ours: kept.images.keys().copied().collect(),
        }
    }

    fn set(&self, remember: bool, all: Option<Snapshot>) {
        let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
        *self
            .shared
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Pending {
            generation,
            remember,
            all,
        });
    }

    /// `all` changed: saved [`SAVE_AFTER`] after the last change, but
    /// no later than [`SAVE_WITHIN`] after the first unsaved one, while
    /// `remember` is on.
    fn changed(&mut self, state: &Notifications, kept: &Kept) {
        if !state.remember || self.shared.dir.is_none() {
            return;
        }
        self.set(true, Some(History::snapshot(state, kept)));
        let now = tokio::time::Instant::now();
        if !self.armed {
            self.latest = now + SAVE_WITHIN;
        }
        self.due.as_mut().reset((now + SAVE_AFTER).min(self.latest));
        self.armed = true;
    }

    /// `remember` written: kept, with `all` saved (on) or erased (off),
    /// now.
    fn remember(&mut self, on: bool, state: &Notifications, kept: &Kept) {
        if self.shared.dir.is_none() {
            return;
        }
        self.set(on, on.then(|| History::snapshot(state, kept)));
        self.armed = false;
        self.flush();
    }

    /// Make the pending save off the runtime.
    fn flush(&self) {
        let shared = self.shared.clone();
        tokio::task::spawn_blocking(move || shared.save_pending());
    }
}

/// Makes the history's last save as the run ends (a stopped body is
/// dropped): queued as a finalizer, so a stop or shutdown loses nothing.
struct SaveOnStop(Arc<HistoryShared>);

impl Drop for SaveOnStop {
    fn drop(&mut self) {
        let pending = self.0.pending.lock().map(|p| p.is_some()).unwrap_or(false);
        if !pending {
            return;
        }
        let shared = self.0.clone();
        crate::client::finalize(async move {
            let _ = tokio::task::spawn_blocking(move || shared.save_pending()).await;
        });
    }
}

/// `n` into `list`: in place when it replaces one, else at the end.
fn arrive(list: &mut Vec<Notification>, n: Notification) {
    match list.iter_mut().find(|m| m.id == n.id) {
        Some(m) => *m = n,
        None => list.push(n),
    }
}

/// Close notification `id` (gone from `popups` and `all`) and tell the
/// sender why. `false` once the service was stopped.
async fn close(
    cx: &mut Cx<Notifications>,
    conn: &zbus::Connection,
    ids: &Arc<Mutex<Ids>>,
    kept: &mut Kept,
    id: i64,
    why: Closed,
) -> bool {
    let known = cx.state().all.iter().any(|n| n.id == id);
    if let Ok(mut ids) = ids.lock() {
        ids.live.remove(&(id as u32));
    }
    kept.resident.remove(&id);
    kept.default.remove(&id);
    kept.transient.remove(&id);
    let restored = kept.restored.remove(&id);
    // The file goes once the state no longer names it.
    let image = kept.images.remove(&id);
    if !known {
        return !cx.stopped();
    }
    // A restored one's sender is not told: it never knew this id.
    if !restored {
        signal_closed(conn, id, why).await;
    }
    let alive = cx.update(|s| {
        s.popups.retain(|n| n.id != id);
        s.all.retain(|n| n.id != id);
        s.count = s.all.len() as i64;
    });
    drop(image);
    alive
}

async fn signal_closed(conn: &zbus::Connection, id: i64, why: Closed) {
    let r = conn
        .emit_signal(
            None::<&str>,
            PATH,
            NAME,
            "NotificationClosed",
            &(id as u32, why as u32),
        )
        .await;
    if let Err(e) = r {
        log::debug!("notifications: NotificationClosed not sent: {e}");
    }
}

/// `ActionInvoked`, after (M5) spec 1.2's `ActivationToken` when the
/// press that invoked it got a token.
async fn invoked(conn: &zbus::Connection, id: i64, key: &str, token: Option<&str>) {
    for (member, text) in [("ActivationToken", token), ("ActionInvoked", Some(key))] {
        let Some(text) = text else {
            continue;
        };
        let r = conn
            .emit_signal(None::<&str>, PATH, NAME, member, &(id as u32, text))
            .await;
        if let Err(e) = r {
            log::debug!("notifications: {member} not sent: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A flood of pictures: once [`IMAGES_QUEUED`] wait to be written,
    /// a new notification arrives without its picture (or with its
    /// `image-path`), and the count of those waiting stays bounded.
    #[test]
    fn pictures_waiting_to_be_written_are_bounded() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let server = Server {
            ids: Arc::default(),
            tx,
            images: Arc::new(AtomicUsize::new(0)),
        };
        let pixels = || {
            let d: ImageData = (1, 1, 4, true, 8, 4, vec![1, 2, 3, 4]);
            HashMap::from([(
                "image-data".to_string(),
                OwnedValue::try_from(zbus::zvariant::Value::from(d)).unwrap(),
            )])
        };
        let arrive = |hints| {
            server
                .arrival(
                    "a".into(),
                    0,
                    String::new(),
                    "s".into(),
                    String::new(),
                    vec![],
                    hints,
                    -1,
                )
                .unwrap()
        };
        for _ in 0..IMAGES_QUEUED {
            assert!(matches!(arrive(pixels()).image, Image::Data(_)));
        }
        assert_eq!(server.images.load(Ordering::Acquire), IMAGES_QUEUED);
        // Full: left out, and the count does not grow.
        assert!(matches!(arrive(pixels()).image, Image::None));
        assert_eq!(server.images.load(Ordering::Acquire), IMAGES_QUEUED);
        // An image-path is still shown.
        let mut hints = pixels();
        hints.insert(
            "image-path".into(),
            OwnedValue::try_from(zbus::zvariant::Value::from("/tmp/p.png")).unwrap(),
        );
        assert!(matches!(arrive(hints).image, Image::Path(p) if p == "/tmp/p.png"));
        // One written: room for one more.
        server.images.fetch_sub(1, Ordering::AcqRel);
        assert!(matches!(arrive(pixels()).image, Image::Data(_)));
        assert!(rx.try_recv().is_err(), "arrival() itself sends nothing");
        // The run ended: nothing more is taken (no id for a notification
        // nobody shows).
        server.ids.lock().unwrap().closed = true;
        let none = server.arrival(
            "a".into(),
            0,
            String::new(),
            "s".into(),
            String::new(),
            vec![],
            HashMap::new(),
            -1,
        );
        assert!(none.is_none());
    }

    #[test]
    fn the_conflict_names_the_owner_and_how_to_stop_it() {
        // Run by a systemd user service: that unit is named.
        let m = conflict_message(Some(Owner {
            pid: 42,
            comm: Some("mako".into()),
            unit: Some("mako.service".into()),
        }));
        assert!(m.contains("`mako` (pid 42)"), "{m}");
        assert!(m.contains("systemctl --user stop mako.service"), "{m}");
        assert!(m.contains("systemctl --user mask mako.service"), "{m}");
        assert!(m.contains("dbus-1/services"), "{m}");
        assert!(m.contains(NAME), "{m}");
        // Started some other way (a compositor's exec, a terminal): no
        // unit to name, the process is.
        let m = conflict_message(Some(Owner {
            pid: 7,
            comm: Some("python3.12".into()),
            unit: None,
        }));
        assert!(!m.contains("systemctl"), "{m}");
        assert!(m.contains("`kill 7`"), "{m}");
        assert!(!m.contains("pkill"), "an interpreter is not pkilled: {m}");
        assert!(m.contains("dbus-1/services"), "{m}");
        let m = conflict_message(Some(Owner {
            pid: 9,
            comm: Some("dunst".into()),
            unit: None,
        }));
        assert!(
            m.contains("`kill 9`") && m.contains("pkill -x dunst"),
            "{m}"
        );
        // A desktop's own shell: never stopped, not even by its unit.
        let m = conflict_message(Some(Owner {
            pid: 3,
            comm: Some("gnome-shell".into()),
            unit: Some("org.gnome.Shell@wayland.service".into()),
        }));
        assert!(m.contains("its own settings"), "{m}");
        assert!(!m.contains("kill") && !m.contains("systemctl"), "{m}");
        for i in ["python3", "python3.12", "perl", "node", "bash", "sh", "gjs"] {
            assert!(interpreter(i), "{i}");
        }
        assert!(!interpreter("mako") && !interpreter("swaync"));
        assert!(conflict_message(None).contains("another process"));
    }

    /// A notification as a sender's `Notify` leaves it in `all`.
    fn sent(id: i64, summary: &str, image: Option<&str>) -> Notification {
        Notification {
            id,
            app: NotificationApp {
                name: "Mail".into(),
                icon: "mail-unread".into(),
            },
            summary: summary.into(),
            body: "You have <b>mail</b>".into(),
            image: image.map(str::to_string),
            urgency: Urgency::Critical,
            timeout: Some(Duration::from_millis(4500)),
            persistent: false,
            time: Date {
                year: 2026,
                month: 10,
                day: 15,
                weekday: 4,
                hour: Some(9),
                minute: Some(41),
            },
            actions: vec![NotificationAction {
                id: "reply".into(),
                label: "Reply".into(),
                notification: id,
            }],
        }
    }

    /// (gap 35, m6-service-gaps) What is remembered comes back, oldest
    /// first, renumbered 1..=n, without actions: its senders cannot hear
    /// an answer.
    #[test]
    fn the_history_round_trips_without_actions() {
        let all = vec![sent(7, "First", None), sent(9, "Second", None)];
        let text = history_text(&all, &HashSet::new(), &HashSet::new());
        let back = restore(&text, |_| true).unwrap();
        assert_eq!(back.len(), 2);
        for (i, (b, a)) in back.iter().zip(&all).enumerate() {
            assert_eq!(b.id, i as i64 + 1, "renumbered");
            assert!(b.actions.is_empty(), "no buttons");
            assert_eq!(
                Notification {
                    id: a.id,
                    actions: a.actions.clone(),
                    ..b.clone()
                },
                *a
            );
        }
    }

    /// The newest [`KEPT`] are remembered, `transient` ones never.
    #[test]
    fn the_history_keeps_the_newest_and_never_a_transient_one() {
        let all: Vec<Notification> = (1..=KEPT as i64 + 5)
            .map(|i| sent(i, &format!("n{i}"), None))
            .collect();
        let transient = HashSet::from([KEPT as i64 + 5, 3]);
        let back = restore(&history_text(&all, &transient, &HashSet::new()), |_| true).unwrap();
        assert_eq!(back.len(), KEPT);
        assert_eq!(back.last().unwrap().summary, format!("n{}", KEPT + 4));
        assert_eq!(back[0].summary, "n5", "the oldest beyond KEPT left out");
        assert!(back.iter().all(|n| n.summary != "n3"));
    }

    /// Our own pictures (written from `image-data` into the runtime
    /// directory) are never remembered; a sender's `image-path` is, and
    /// is dropped at restore when its file is gone (an icon name stays).
    #[test]
    fn only_a_senders_existing_picture_is_remembered() {
        let all = vec![
            sent(1, "ours", Some("/run/user/1000/strand/pixmaps/a.png")),
            sent(2, "path", Some("/home/me/Pictures/cat.png")),
            sent(3, "url", Some("file:///home/me/Pictures/gone.png")),
            sent(4, "icon", Some("dialog-information")),
        ];
        let text = history_text(&all, &HashSet::new(), &HashSet::from([1]));
        let exists = |p: &Path| p == Path::new("/home/me/Pictures/cat.png");
        let back = restore(&text, exists).unwrap();
        let images: Vec<Option<&str>> = back.iter().map(|n| n.image.as_deref()).collect();
        assert_eq!(
            images,
            [
                None,
                Some("/home/me/Pictures/cat.png"),
                None,
                Some("dialog-information")
            ]
        );
        assert!(!text.contains("pixmaps"), "{text}");
    }

    /// (gap 35, review round 2) A `file://` picture is looked for at the
    /// path it names, percent-decoded as the renderer reads it, so a
    /// picture under a folder with a space or a non-ASCII name survives
    /// the restart; the URL itself is kept as the sender wrote it.
    #[test]
    fn a_percent_encoded_file_url_picture_is_found_at_restore() {
        let all = vec![
            sent(1, "space", Some("file:///home/me/My%20Pictures/cat.png")),
            sent(2, "utf8", Some("file:///home/me/Bilder/K%C3%A4tzchen.png")),
            sent(3, "gone", Some("file:///home/me/My%20Pictures/gone.png")),
        ];
        let text = history_text(&all, &HashSet::new(), &HashSet::new());
        let exists = |p: &Path| {
            p == Path::new("/home/me/My Pictures/cat.png")
                || p == Path::new("/home/me/Bilder/Kätzchen.png")
        };
        let back = restore(&text, exists).unwrap();
        let images: Vec<Option<&str>> = back.iter().map(|n| n.image.as_deref()).collect();
        assert_eq!(
            images,
            [
                Some("file:///home/me/My%20Pictures/cat.png"),
                Some("file:///home/me/Bilder/K%C3%A4tzchen.png"),
                None
            ]
        );
    }

    /// A history that is not this version's JSON does not restore.
    #[test]
    fn a_history_that_does_not_read_is_refused() {
        assert!(restore("{not json", |_| true).is_err());
        assert!(restore("", |_| true).is_err());
        assert!(restore(r#"{"version":2,"notifications":[]}"#, |_| true).is_err());
        assert!(restore(r#"{"version":1,"notifications":[{"summary":1}]}"#, |_| true).is_err());
        assert_eq!(
            restore(r#"{"version":1,"notifications":[]}"#, |_| true),
            Ok(Vec::new())
        );
    }

    /// (gap 35, review round 1) The save waits [`SAVE_AFTER`] after the
    /// last change, but a sender replacing a notification every half
    /// second (a progress bar) does not postpone it past [`SAVE_WITHIN`]
    /// after the first unsaved change; after a save the bound starts
    /// again.
    #[tokio::test(start_paused = true)]
    async fn a_history_changing_without_pause_is_saved_within_its_bound() {
        let dir = tempfile::tempdir().unwrap();
        let mut history = History::new(Some(dir.path().to_path_buf()));
        let state = Notifications {
            remember: true,
            ..Notifications::default()
        };
        let kept = Kept::default();
        // When each save fell due, from `start`, while `all` changes every
        // half second for 20 s.
        let start = tokio::time::Instant::now();
        let mut saves = Vec::new();
        history.changed(&state, &kept);
        while start.elapsed() < Duration::from_secs(20) {
            tokio::select! {
                () = &mut history.due, if history.armed => {
                    history.armed = false;
                    saves.push(start.elapsed());
                }
                () = tokio::time::sleep(SAVE_AFTER / 2) => history.changed(&state, &kept),
            }
        }
        assert!(saves.len() >= 3, "{saves:?}");
        assert!(saves[0] <= SAVE_WITHIN, "{saves:?}");
        for w in saves.windows(2) {
            assert!(w[1] - w[0] <= SAVE_WITHIN + SAVE_AFTER, "{saves:?}");
        }

        // One change alone is still saved SAVE_AFTER after it.
        if history.armed {
            (&mut history.due).await;
            history.armed = false;
        }
        let at = tokio::time::Instant::now();
        history.changed(&state, &kept);
        (&mut history.due).await;
        assert_eq!(at.elapsed(), SAVE_AFTER);
    }

    /// (gap 35, review round 2) A history file that does not read is not
    /// set aside as a `.corrupt` copy (which would keep every body for
    /// the store's retention): it stays where it is, the run starts
    /// empty, and `remember = false` erases it with the rest, leaving no
    /// file in the directory that holds a notification's text.
    #[tokio::test]
    async fn a_broken_history_is_erased_by_forgetting_not_kept_aside() {
        let dir = tempfile::tempdir().unwrap();
        let store = strand_core::PersistStore::new(dir.path());
        let file = store.file_of(HISTORY_PATH).unwrap();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"not a persisted value: 2FA code 123456").unwrap();

        let history = History::new(Some(dir.path().to_path_buf()));
        assert_eq!(history.load().await, (true, Vec::new()));

        save(
            dir.path(),
            Pending {
                generation: GENERATION.fetch_add(1, Ordering::Relaxed) + 1,
                remember: false,
                all: None,
            },
        );
        let mut left = Vec::new();
        for entry in walk(dir.path()) {
            let bytes = std::fs::read(&entry).unwrap();
            if String::from_utf8_lossy(&bytes).contains("123456") {
                left.push(entry);
            }
        }
        assert_eq!(left, Vec::<PathBuf>::new());
        assert!(!file.exists());
    }

    /// Every file under `dir`.
    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
        out
    }

    /// The `transient` hint, a boolean by the spec or an integer.
    #[test]
    fn the_transient_hint_is_read() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let server = Server {
            ids: Arc::default(),
            tx,
            images: Arc::new(AtomicUsize::new(0)),
        };
        let arrive = |hints: HashMap<String, OwnedValue>| {
            server
                .arrival(
                    "a".into(),
                    0,
                    String::new(),
                    "s".into(),
                    String::new(),
                    vec![],
                    hints,
                    -1,
                )
                .unwrap()
                .transient
        };
        let hint = |v: zbus::zvariant::Value<'static>| {
            HashMap::from([("transient".to_string(), OwnedValue::try_from(v).unwrap())])
        };
        assert!(arrive(hint(true.into())));
        assert!(arrive(hint(1i32.into())));
        assert!(!arrive(hint(false.into())));
        assert!(!arrive(HashMap::new()));
    }
}
