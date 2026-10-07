//! `notifications`: the shell's own notification server
//! (`org.freedesktop.Notifications`, Desktop Notifications spec 1.2) as a
//! zbus `#[interface]` on a session-bus connection of its own, so the
//! name goes with the connection when the service stops.
//!
//! - `Notify` (`replaces_id`, actions, hints: `urgency`, `image-data`,
//!   `image-path`, `desktop-entry`, `resident`; `expire_timeout`),
//!   `CloseNotification`, `GetCapabilities`, `GetServerInformation`; the
//!   `NotificationClosed` and `ActionInvoked` signals.
//! - The store: `popups` (shown now), `all` (kept), `count`, `dnd`, the
//!   `received` event; `n.expire()` ends a popup (closed as expired),
//!   `n.dismiss()` closes it (dismissed), `n.activate()` invokes its
//!   `default` action, `a.invoke()` one of its buttons, `clear()` closes
//!   every one. The server sets no timers: the shell expires popups
//!   (`after n.timeout ?? 6s { n.expire() }`), so an idle server wakes
//!   nothing.
//! - **Another server owns the name** (dunst, mako, a desktop's): the
//!   name is asked for without queueing, and the run fails with a
//!   diagnostic naming the owner's process (`GetConnectionUnixProcessID`,
//!   `/proc/<pid>/comm`) and how to stop it. There is never a silent
//!   second server; the client retries with its backoff, so stopping the
//!   other daemon hands the name over within 30 s.

use std::collections::{HashMap, HashSet};
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
    /// Every notification kept, popups included.
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

/// What a sender's `Notify` asked for, as the store keeps it.
#[derive(Clone, Debug)]
struct Arrival {
    n: Notification,
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

/// The `image-data` hint (or its older names) written as a PNG file.
fn image_data(hints: &HashMap<String, OwnedValue>) -> Option<String> {
    let v = ["image-data", "image_data", "icon_data"]
        .iter()
        .find_map(|k| hints.get(*k))?;
    type Raw = (i32, i32, i32, bool, i32, i32, Vec<u8>);
    let (w, h, stride, alpha, bits, ch, data): Raw = v.try_clone().ok()?.try_into().ok()?;
    let written = crate::pixmap::from_image_data(w, h, stride, alpha, bits, ch, &data)
        .and_then(|(w, h, rgba)| crate::pixmap::write_rgba(w, h, &rgba));
    match written {
        Ok(p) => Some(p.to_string_lossy().into_owned()),
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
        let urgency = match hints
            .get("urgency")
            .and_then(|v| v.downcast_ref::<u8>().ok())
        {
            Some(0) => Urgency::Low,
            Some(2) => Urgency::Critical,
            _ => Urgency::Normal,
        };
        let icon = if app_icon.is_empty() {
            hint_text(&hints, &["desktop-entry"]).unwrap_or_default()
        } else {
            app_icon
        };
        let image = image_data(&hints).or_else(|| hint_text(&hints, &["image-path", "image_path"]));
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
                image,
                urgency,
                // -1: the server's choice; 0: never (left to the shell
                // too, which keeps critical ones; decisions.md, wave4-a2).
                timeout: (expire_timeout > 0).then(|| Duration::from_millis(expire_timeout as u64)),
                time: Date::today(),
                actions: buttons,
            },
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

    async fn close_notification(&self, id: u32) {
        let _ = self.tx.send(FromBus::Close(id));
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
            "1.2".to_string(),
        )
    }
}

/// The diagnostic for a name another process owns.
pub fn conflict_message(owner: Option<(u32, Option<String>)>) -> String {
    let who = match &owner {
        Some((pid, Some(comm))) => format!("`{comm}` (pid {pid})"),
        Some((pid, None)) => format!("process {pid}"),
        None => "another process".to_string(),
    };
    let how = match &owner {
        Some((_, Some(comm))) => format!(
            "stop it (`systemctl --user stop {comm}`, or `pkill -x {comm}`) and remove it from \
             your compositor's autostart"
        ),
        Some((pid, None)) => format!("stop it (`kill {pid}`) and remove it from your autostart"),
        None => "stop the other notification daemon".to_string(),
    };
    format!(
        "another notification server, {who}, owns {NAME}: strand cannot show notifications \
         while it runs; {how}. strand takes the name over once it is free."
    )
}

/// The body's own bookkeeping besides the store.
#[derive(Debug, Default)]
struct Kept {
    resident: HashSet<i64>,
    default: HashSet<i64>,
}

impl Notifications {
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let conn = match crate::bus::own_session(cx.buses()).await {
            Ok(c) => c,
            Err(e) => return crate::battery::idle_without_bus(&mut cx, "session", e).await,
        };
        let ids = Arc::new(Mutex::new(Ids::default()));
        let (tx, mut rx) = mpsc::unbounded_channel();
        conn.object_server()
            .at(
                PATH,
                Server {
                    ids: ids.clone(),
                    tx,
                },
            )
            .await?;
        let flags = zbus::fdo::RequestNameFlags::DoNotQueue;
        match conn.request_name_with_flags(NAME, flags.into()).await {
            Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {}
            // zbus says `NameTaken` for an `Exists` reply.
            Ok(RequestNameReply::Exists | RequestNameReply::InQueue)
            | Err(zbus::Error::NameTaken) => {
                let owner = crate::dbus::owner_process(&conn, NAME).await;
                let message = conflict_message(owner);
                cx.ready();
                cx.notice(message.clone());
                return Err(ServiceError(message));
            }
            Err(e) => {
                cx.ready();
                return Err(ServiceError(format!("{NAME} not owned: {e}")));
            }
        }
        cx.ready();
        let mut kept = Kept::default();
        loop {
            tokio::select! {
                m = rx.recv() => {
                    let Some(m) = m else {
                        return Err(ServiceError("the notification server stopped".into()));
                    };
                    match m {
                        FromBus::Notify(a) => {
                            let Arrival { n, replaces, resident, default } = *a;
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
                        }
                        FromBus::Close(id) => {
                            if !close(&mut cx, &conn, &ids, &mut kept, i64::from(id), Closed::Closed).await {
                                return Ok(());
                            }
                        }
                    }
                }
                m = cx.recv() => match m {
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
                            NotificationsAction::Expire { item } => {
                                let shown = cx.state().popups.iter().any(|n| n.id == item.id);
                                if shown {
                                    signal_closed(&conn, item.id, Closed::Expired).await;
                                }
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
                                invoked(&conn, item.notification, &item.id).await;
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
    if !known {
        return !cx.stopped();
    }
    signal_closed(conn, id, why).await;
    cx.update(|s| {
        s.popups.retain(|n| n.id != id);
        s.all.retain(|n| n.id != id);
        s.count = s.all.len() as i64;
    })
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

    #[test]
    fn the_conflict_names_the_owner_and_how_to_stop_it() {
        let m = conflict_message(Some((42, Some("mako".into()))));
        assert!(m.contains("`mako` (pid 42)"), "{m}");
        assert!(m.contains("systemctl --user stop mako"), "{m}");
        assert!(m.contains(NAME), "{m}");
        assert!(conflict_message(None).contains("another process"));
    }
}
