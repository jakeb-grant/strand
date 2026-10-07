//! `notifications`: the shell's own notification server
//! (`org.freedesktop.Notifications`, Desktop Notifications spec 1.1: 1.2's
//! `ActivationToken` needs an xdg-activation token from the clicked
//! surface, which M4's renderer will provide) as a
//! zbus `#[interface]` on a session-bus connection of its own, so the
//! name goes with the connection when the service stops.
//!
//! - `Notify` (`replaces_id`, actions, hints: `urgency`, `image-data`,
//!   `image-path`, `desktop-entry`, `resident`; `expire_timeout`),
//!   `CloseNotification`, `GetCapabilities`, `GetServerInformation`; the
//!   `NotificationClosed` and `ActionInvoked` signals.
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
//!   closed (`NotificationClosed`, reason 3) before the name goes, and a
//!   new run starts with none, so ids are never reused for a notification
//!   still shown.
//! - **Another server owns the name** (dunst, mako, a desktop's): the
//!   name is asked for without queueing, and the run fails with a
//!   diagnostic naming the owner's process (`GetConnectionUnixProcessID`,
//!   `/proc/<pid>/comm`, and its systemd user unit when it has one:
//!   `GetUnitByPID`) and how to stop it. There is never a silent
//!   second server; the client retries with its backoff, so stopping the
//!   other daemon hands the name over within 30 s, and the diagnostic is
//!   then resolved.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Datelike;
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
/// expired (design.md's memory budget holds notification history to a few
/// megabytes).
pub const KEPT: usize = 100;

/// How many `image-data` pictures may wait to be written: beyond it a
/// new notification's picture is left out.
pub const IMAGES_QUEUED: usize = 8;

/// What `GetServerInformation` answers as the spec version: 1.1, since
/// 1.2's `ActivationToken` signal is not sent (decisions.md, wave4-a2).
pub const SPEC_VERSION: &str = "1.1";

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

/// A `Date` (the clock's record): year, month, day, weekday (Monday 1).
#[derive(crate::Data, Clone, Debug, Default, PartialEq)]
#[data(name = "Date")]
pub struct Date {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    pub weekday: i64,
}

impl Date {
    /// Today, in the local zone.
    pub fn today() -> Date {
        let d = chrono::Local::now().date_naive();
        Date {
            year: i64::from(d.year()),
            month: i64::from(d.month()),
            day: i64::from(d.day()),
            weekday: i64::from(d.weekday().number_from_monday()),
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
#[service(name = "notifications", action = NotificationsAction)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Notifications {
    /// The notifications to show now; each leaves when it expires or is
    /// dismissed.
    #[store(keyed)]
    pub popups: Vec<Notification>,
    /// Every notification kept, popups included: the newest 100 (older
    /// ones close as expired).
    #[store(keyed)]
    pub all: Vec<Notification>,
    /// How many notifications are kept.
    pub count: i64,
    /// Do not disturb: new notifications that are not critical skip the
    /// popups (they are kept in `all`, and `received` still fires).
    /// Writable.
    #[store(rw)]
    pub dnd: bool,
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
    ) -> Arrival {
        let (id, replaces) = {
            let mut ids = self.ids.lock().unwrap_or_else(|e| e.into_inner());
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
        Arrival {
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
                time: Date::today(),
                actions: buttons,
            },
            image,
            replaces,
            resident,
            default,
        }
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
    ) -> u32 {
        let a = self.arrival(
            app_name,
            replaces_id,
            app_icon,
            summary,
            body,
            actions,
            hints,
            expire_timeout,
        );
        let id = a.n.id as u32;
        let _ = self.tx.send(FromBus::Notify(Box::new(a)));
        id
    }

    /// The spec: an id that does not exist (any more) is an error.
    async fn close_notification(&self, id: u32) -> zbus::fdo::Result<()> {
        let live = self
            .ids
            .lock()
            .map(|ids| ids.live.contains(&id))
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
            unit: Some(unit), ..
        }) => format!(
            "stop it and keep it from starting again (`systemctl --user stop {unit}` and \
             `systemctl --user mask {unit}`)"
        ),
        Some(Owner {
            pid,
            comm: Some(comm),
            ..
        }) => format!(
            "stop it (`pkill -x {comm}`, or `kill {pid}`) and remove it from your \
             compositor's autostart"
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
        // server starts with none (its ids start again at 1).
        if !cx.state().all.is_empty() || !cx.state().popups.is_empty() {
            let cleared = cx.update(|s| {
                s.all.clear();
                s.popups.clear();
                s.count = 0;
            });
            if !cleared {
                return Ok(());
            }
        }
        let flags = zbus::fdo::RequestNameFlags::DoNotQueue;
        match conn.request_name_with_flags(NAME, flags.into()).await {
            Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {}
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
                let message = conflict_message(owner);
                // The notice before readiness: a ready run without one
                // resolves it (the name taken over).
                cx.notice(message.clone());
                cx.ready();
                return Err(ServiceError(message));
            }
            Err(e) => {
                cx.ready();
                return Err(ServiceError(format!("{NAME} not owned: {e}")));
            }
        }
        cx.ready();
        // However the run ends (stopped, which drops this body, or
        // failed), the notifications still open close with it, before
        // the name goes.
        let _closing = CloseOnStop {
            conn: conn.clone(),
            ids: ids.clone(),
        };
        let mut kept = Kept::default();
        loop {
            tokio::select! {
                m = rx.recv() => {
                    let Some(m) = m else {
                        return Err(ServiceError("the notification server stopped".into()));
                    };
                    match m {
                        FromBus::Notify(a) => {
                            let Arrival { mut n, image, replaces, resident, default } = *a;
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
                    Some(Msg::Action(a)) => {
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
                                    invoked(&conn, item.id, "default").await;
                                }
                                kept.resident.contains(&item.id)
                                    || close(&mut cx, &conn, &ids, &mut kept, item.id, Closed::Dismissed).await
                            }
                            NotificationsAction::Invoke { item } => {
                                if cx.state().all.iter().any(|n| n.id == item.notification) {
                                    invoked(&conn, item.notification, &item.id).await;
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
        let mut open: Vec<u32> = match self.ids.lock() {
            Ok(mut ids) => ids.live.drain().collect(),
            Err(_) => return,
        };
        if open.is_empty() {
            return;
        }
        open.sort_unstable();
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let conn = self.conn.clone();
        rt.spawn(async move {
            for id in open {
                signal_closed(&conn, i64::from(id), Closed::Closed).await;
            }
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
    // The file goes once the state no longer names it.
    let image = kept.images.remove(&id);
    if !known {
        return !cx.stopped();
    }
    signal_closed(conn, id, why).await;
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

async fn invoked(conn: &zbus::Connection, id: i64, key: &str) {
    let r = conn
        .emit_signal(None::<&str>, PATH, NAME, "ActionInvoked", &(id as u32, key))
        .await;
    if let Err(e) = r {
        log::debug!("notifications: ActionInvoked not sent: {e}");
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
            server.arrival(
                "a".into(),
                0,
                String::new(),
                "s".into(),
                String::new(),
                vec![],
                hints,
                -1,
            )
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
        assert!(m.contains("pkill -x python3.12"), "{m}");
        assert!(m.contains("kill 7"), "{m}");
        assert!(m.contains("dbus-1/services"), "{m}");
        assert!(conflict_message(None).contains("another process"));
    }
}
