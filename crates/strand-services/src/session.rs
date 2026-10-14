//! `session`: the login session and the machine's power, from logind
//! (`org.freedesktop.login1`) on the system bus (owner-testing gaps 1 and
//! 17, decisions.md m6-w2-services).
//!
//! `can_*` are logind's `CanSuspend`, `CanHibernate`, `CanReboot` and
//! `CanPowerOff`, asked when the service starts and again when logind
//! restarts (they are methods: logind signals no change). `yes` and
//! `challenge` (polkit asks first) are true; `no` and `na` false.
//!
//! The actions are logind's: `Suspend`, `Hibernate`, `Reboot` and
//! `PowerOff` with `interactive` true (so polkit may ask for a password),
//! and `LockSession` and `TerminateSession` on this process's own
//! session (`GetSession("auto")`: the caller's session, else the user's
//! graphical one). Each is a task of its own, so a polkit dialog holds
//! up nothing; a failure is `failed(action, error)`. logind's `Lock`
//! signal on that session is `lock_requested`. Nothing polls.

use std::time::Duration;

use tokio::task::JoinSet;

use crate::dbus::{self, Daemon, DaemonEvent};
use crate::{Call, Cx, Event, Msg, ServiceError, Store, service};

/// The schema the `session` service serves.
pub const SCHEMA: &str = strand_services_schema::SESSION;

/// logind's bus name.
pub const LOGIND: &str = "org.freedesktop.login1";
/// logind's manager object.
pub const MANAGER: &str = "/org/freedesktop/login1";
const MANAGER_IFACE: &str = "org.freedesktop.login1.Manager";
const SESSION_IFACE: &str = "org.freedesktop.login1.Session";

/// How long an action may take: polkit's password dialog included.
const ACTION_TIMEOUT: Duration = Duration::from_secs(120);

/// `session`'s actions.
#[derive(Call, Debug, Clone, Copy, PartialEq)]
pub enum SessionAction {
    /// `session.suspend()`.
    Suspend,
    /// `session.hibernate()`.
    Hibernate,
    /// `session.reboot()`.
    Reboot,
    /// `session.power_off()`.
    PowerOff,
    /// `session.lock()`.
    Lock,
    /// `session.log_out()`.
    LogOut,
}

impl SessionAction {
    /// Its name as the schema spells it.
    pub fn name(self) -> &'static str {
        match self {
            SessionAction::Suspend => "suspend",
            SessionAction::Hibernate => "hibernate",
            SessionAction::Reboot => "reboot",
            SessionAction::PowerOff => "power_off",
            SessionAction::Lock => "lock",
            SessionAction::LogOut => "log_out",
        }
    }

    /// The manager's method for a power action.
    fn power_method(self) -> Option<&'static str> {
        Some(match self {
            SessionAction::Suspend => "Suspend",
            SessionAction::Hibernate => "Hibernate",
            SessionAction::Reboot => "Reboot",
            SessionAction::PowerOff => "PowerOff",
            SessionAction::Lock | SessionAction::LogOut => return None,
        })
    }
}

/// See the module docs.
#[service(name = "session", action = SessionAction)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Session {
    /// Suspending is allowed (logind's `CanSuspend` is `yes`, or
    /// `challenge`: polkit asks for a password first).
    pub can_suspend: bool,
    /// Hibernating is allowed (`CanHibernate`, as `can_suspend`).
    pub can_hibernate: bool,
    /// Rebooting is allowed (`CanReboot`, as `can_suspend`).
    pub can_reboot: bool,
    /// Powering off is allowed (`CanPowerOff`, as `can_suspend`).
    pub can_power_off: bool,
    /// logind asks this session's locker to lock it (`session.lock()`,
    /// `loginctl lock-session`, an idle daemon): `on
    /// session.lock_requested { locked = true }` puts a shell's own `lock`
    /// up.
    pub lock_requested: Event<()>,
    /// An action failed (polkit refused it, no login session to lock or
    /// end): `on session.failed(action, error) { … }`, `action` as written
    /// (`"reboot"`).
    pub failed: Event<(String, String)>,
}

/// Whether a `Can*` answer allows it.
pub fn allowed(answer: &str) -> bool {
    matches!(answer, "yes" | "challenge")
}

/// This process's login session: its object and its id.
#[derive(Debug, Clone)]
struct Own {
    path: String,
    id: String,
}

/// Ask logind what is allowed, and which session is ours.
async fn read(conn: &zbus::Connection) -> (Session, Option<Own>) {
    let can = |method: &'static str| async move {
        let r =
            dbus::timed(conn.call_method(Some(LOGIND), MANAGER, Some(MANAGER_IFACE), method, &()))
                .await
                .and_then(|m| m.body().deserialize::<String>());
        match r {
            Ok(answer) => allowed(&answer),
            Err(e) => {
                log::debug!("session: {method}: {e}");
                false
            }
        }
    };
    let own = async {
        let path: zbus::zvariant::OwnedObjectPath = dbus::timed(conn.call_method(
            Some(LOGIND),
            MANAGER,
            Some(MANAGER_IFACE),
            "GetSession",
            &("auto",),
        ))
        .await
        .and_then(|m| m.body().deserialize())
        .inspect_err(|e| log::info!("session: no login session ({e}): lock and log out fail"))
        .ok()?;
        let path = path.to_string();
        let id = dbus::timed(dbus::get(conn, LOGIND, &path, SESSION_IFACE, "Id"))
            .await
            .ok()
            .and_then(|v| v.downcast_ref::<&str>().ok().map(str::to_string))?;
        Some(Own { path, id })
    };
    let (can_suspend, can_hibernate, can_reboot, can_power_off, own) = tokio::join!(
        can("CanSuspend"),
        can("CanHibernate"),
        can("CanReboot"),
        can("CanPowerOff"),
        own
    );
    let state = Session {
        can_suspend,
        can_hibernate,
        can_reboot,
        can_power_off,
        ..Session::default()
    };
    (state, own)
}

/// What a failed call says.
fn error_text(e: zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(name, text, _) => text.unwrap_or_else(|| name.to_string()),
        zbus::Error::Failure(text) => text,
        e => e.to_string(),
    }
}

/// Make `action`'s call; why it failed, if it did.
async fn act(
    conn: zbus::Connection,
    action: SessionAction,
    own: Option<Own>,
) -> Option<(String, String)> {
    let call = |method: &'static str, id: Option<String>| {
        let conn = conn.clone();
        async move {
            let r = match id {
                Some(id) => {
                    conn.call_method(Some(LOGIND), MANAGER, Some(MANAGER_IFACE), method, &(id,))
                        .await
                }
                None => {
                    conn.call_method(Some(LOGIND), MANAGER, Some(MANAGER_IFACE), method, &(true,))
                        .await
                }
            };
            r.map(|_| ())
        }
    };
    let r = match (action.power_method(), own) {
        (Some(method), _) => dbus::timed_for(ACTION_TIMEOUT, call(method, None)).await,
        (None, Some(own)) => {
            let method = if action == SessionAction::Lock {
                "LockSession"
            } else {
                "TerminateSession"
            };
            dbus::timed_for(ACTION_TIMEOUT, call(method, Some(own.id))).await
        }
        (None, None) => Err(zbus::Error::Failure(
            "this process is in no login session".into(),
        )),
    };
    let e = r.err()?;
    log::warn!("session: {} failed: {e}", action.name());
    Some((action.name().to_string(), error_text(e)))
}

impl Session {
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let conn = match cx.system().await {
            Ok(c) => c,
            Err(e) => return crate::dbus::idle_without_bus(&mut cx, "system", e).await,
        };
        let mut daemon = Daemon::new(&conn, LOGIND).await?;
        let mut acting: JoinSet<Option<(String, String)>> = JoinSet::new();
        loop {
            let (state, own) = if daemon.owner().is_some() {
                read(&conn).await
            } else {
                (Session::default(), None)
            };
            // Its `Lock` signal (sent to the session's locker).
            match own.as_ref().map(|o| dbus::path_rule(&o.path)) {
                Some(Ok(rule)) => {
                    if let Err(e) = daemon.subscribe("session", rule).await {
                        log::debug!("session: no Lock signals: {e}");
                    }
                }
                _ => daemon.unsubscribe("session"),
            }
            if !cx.update(|s| *s = state) {
                return Ok(());
            }
            cx.ready();
            loop {
                tokio::select! {
                    ev = daemon.next() => match ev {
                        None => return Err(ServiceError("the system bus connection ended".into())),
                        // logind restarted (or went): ask again.
                        Some(DaemonEvent::Owner) => break,
                        Some(DaemonEvent::Signal(m)) => {
                            if dbus::interface(&m).as_deref() == Some(SESSION_IFACE)
                                && dbus::member(&m).as_deref() == Some("Lock")
                                && !cx.emit(SessionEvent::LockRequested(()))
                            {
                                return Ok(());
                            }
                        }
                    },
                    Some(done) = acting.join_next(), if !acting.is_empty() => {
                        if let Ok(Some(failure)) = done
                            && !cx.emit(SessionEvent::Failed(failure))
                        {
                            return Ok(());
                        }
                    }
                    m = cx.recv() => match m {
                        None => return Ok(()),
                        Some(Msg::Action(a)) => {
                            acting.spawn(act(conn.clone(), a, own.clone()));
                        }
                        Some(_) => {}
                    },
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yes_and_challenge_are_allowed() {
        assert!(allowed("yes"));
        assert!(allowed("challenge"));
        assert!(!allowed("no"));
        assert!(!allowed("na"));
        assert!(!allowed(""));
    }
}
