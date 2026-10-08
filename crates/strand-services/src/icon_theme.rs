//! The desktop's icon theme as the settings portal names it
//! (`org.gnome.desktop.interface` `icon-theme`): read once (`ReadOne`),
//! then followed (`SettingChanged`, matched on namespace and key so no
//! other setting wakes it), and read again whenever the portal restarts.
//!
//! GNOME, and any desktop whose portal backend exposes GSettings, sets the
//! icon theme there rather than in GTK's `settings.ini`; GTK itself
//! follows it on Wayland. [`spawn`] (a task on the shared services
//! runtime, on its shared session connection) hands each answer to
//! [`strand_icons::set_desktop_theme`], which [`strand_icons::system_theme`]
//! prefers to the settings files (decisions.md, wave4-a3), and on a switch
//! tells the `apps` service and the caller (the renderer's icons are
//! looked up again).

use std::time::Duration;

use futures_lite::StreamExt;
use zbus::zvariant::{OwnedValue, Value};

use crate::Services;
use crate::bus::CONNECT_TIMEOUT;

/// The GSettings schema the portal exposes the theme under.
pub const NAMESPACE: &str = "org.gnome.desktop.interface";
/// Its key.
pub const KEY: &str = "icon-theme";

/// How long one read of the setting may take: a portal frontend stuck on
/// a hung backend answers only after its own 25 s timeout. A read that
/// times out leaves the theme as it was.
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);

#[zbus::proxy(
    interface = "org.freedesktop.portal.Settings",
    default_service = "org.freedesktop.portal.Desktop",
    default_path = "/org/freedesktop/portal/desktop"
)]
trait Settings {
    fn read_one(&self, namespace: &str, key: &str) -> zbus::Result<OwnedValue>;

    /// Version 1; the value comes wrapped in one more variant.
    fn read(&self, namespace: &str, key: &str) -> zbus::Result<OwnedValue>;

    #[zbus(signal)]
    fn setting_changed(&self, namespace: &str, key: &str, value: Value<'_>) -> zbus::Result<()>;
}

/// A theme name out of a setting's value (unwrapping `Read`'s variant).
fn theme_name(v: &Value<'_>) -> Option<String> {
    match v {
        Value::Str(s) => Some(s.to_string()).filter(|s| !s.trim().is_empty()),
        Value::Value(inner) => theme_name(inner),
        _ => None,
    }
}

/// The portal's answer: `Some(name)`, `Some(None)` when it has no such
/// setting (a backend without GSettings), `None` when it did not answer
/// in time (the theme stays as it was).
async fn read(proxy: &SettingsProxy<'_>) -> Option<Option<String>> {
    let ask = async {
        match proxy.read_one(NAMESPACE, KEY).await {
            Ok(v) => Ok(v),
            Err(zbus::Error::MethodError(name, _, _))
                if name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod" =>
            {
                proxy.read(NAMESPACE, KEY).await
            }
            Err(e) => Err(e),
        }
    };
    match tokio::time::timeout(READ_TIMEOUT, ask).await {
        Ok(Ok(v)) => Some(theme_name(&v)),
        Ok(Err(e)) => {
            log::debug!("the portal names no icon theme: {e}");
            Some(None)
        }
        Err(_) => None,
    }
}

/// Follow the portal's icon theme on `conn` until the connection ends:
/// `on` is called with the boot read's answer (`None`: no portal, or it
/// names no theme), then with each change, and again when the portal
/// restarts. A portal that goes away leaves the last name in place.
pub async fn follow(
    conn: &zbus::Connection,
    mut on: impl FnMut(Option<String>),
) -> zbus::Result<()> {
    let setup = async {
        let proxy = SettingsProxy::new(conn).await?;
        // Subscribe before reading, so a change between the two is not
        // lost; the match rule names namespace and key (arg0, arg1).
        let changes = proxy
            .receive_setting_changed_with_args(&[(0, NAMESPACE), (1, KEY)])
            .await?;
        let owners = proxy.inner().receive_owner_changed().await?;
        zbus::Result::Ok((proxy, changes, owners))
    };
    let (proxy, mut changes, mut owners) = match tokio::time::timeout(CONNECT_TIMEOUT, setup).await
    {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            on(None);
            return Err(e);
        }
        Err(_) => {
            on(None);
            return Err(zbus::Error::Failure("timed out".into()));
        }
    };
    on(read(&proxy).await.flatten());
    loop {
        tokio::select! {
            s = changes.next() => {
                let Some(signal) = s else { return Ok(()) };
                let Ok(args) = signal.args() else { continue };
                // A backstop: the match rule already filters on them.
                if *args.namespace() != NAMESPACE || *args.key() != KEY {
                    continue;
                }
                on(theme_name(args.value()));
            }
            o = owners.next() => match o {
                None => return Ok(()),
                // The portal (re)started: its value may differ.
                Some(Some(_)) => {
                    if let Some(name) = read(&proxy).await {
                        on(name);
                    }
                }
                // It went away: keep the last name.
                Some(None) => {}
            },
        }
    }
}

/// Follows the portal's icon theme on the shared services runtime until
/// dropped.
#[derive(Debug)]
pub struct Follower {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for Follower {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// Follow the portal's icon theme on the session bus of `services`: a
/// task on the shared services runtime (design.md: "Services share one
/// tokio current-thread runtime"), on the session connection the other
/// services share (the `system` service's), idle between changes. Each
/// answer goes to [`strand_icons::set_desktop_theme`]; when it switches
/// the theme, the `apps` service is told ([`crate::apps::changed`]) and
/// `switched` is called (on the services thread): the renderer's icons
/// must be looked up again. Without a bus or portal, nothing happens.
pub fn spawn(services: &Services, switched: impl Fn() + Send + 'static) -> Follower {
    let stop = services.spawn_task(move |buses| async move {
        let conn = match crate::bus::session(&buses).await {
            Ok(c) => c,
            Err(e) => {
                log::debug!("not following the portal's icon theme: {e}");
                return;
            }
        };
        let r = follow(&conn, |name| {
            if strand_icons::set_desktop_theme(name) {
                crate::apps::changed();
                switched();
            }
        })
        .await;
        if let Err(e) = r {
            log::debug!("following the portal's icon theme ended: {e}");
        }
    });
    Follower { stop }
}
