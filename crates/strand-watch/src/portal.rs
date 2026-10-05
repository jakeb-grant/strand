//! `org.freedesktop.portal.Settings` client: `ReadOne` at boot, then
//! `SettingChanged`, for `color-scheme`, `accent-color` and `contrast`.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures_lite::StreamExt;
use zbus::zvariant::{OwnedValue, Value};

use crate::event::{ChangeEvent, ColorScheme, Contrast, EventSink, SystemBatch, SystemSetting};

/// The namespace the three settings live in.
pub const APPEARANCE: &str = "org.freedesktop.appearance";
/// The keys read and followed.
pub const KEYS: [&str; 3] = ["color-scheme", "accent-color", "contrast"];

#[zbus::proxy(
    interface = "org.freedesktop.portal.Settings",
    default_service = "org.freedesktop.portal.Desktop",
    default_path = "/org/freedesktop/portal/desktop"
)]
trait Settings {
    fn read_one(&self, namespace: &str, key: &str) -> zbus::Result<OwnedValue>;

    /// Version 1; returns the value wrapped in one more variant.
    fn read(&self, namespace: &str, key: &str) -> zbus::Result<OwnedValue>;

    #[zbus(signal)]
    fn setting_changed(&self, namespace: &str, key: &str, value: Value<'_>) -> zbus::Result<()>;
}

/// Which bus to find the portal on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bus {
    /// `DBUS_SESSION_BUS_ADDRESS`.
    Session,
    /// An explicit address (tests use a private `dbus-daemon`).
    Address(String),
}

/// Type one appearance setting; `None` for other keys or malformed values.
pub fn parse_setting(key: &str, value: &Value<'_>) -> Option<SystemSetting> {
    let value = match value {
        Value::Value(inner) => inner.as_ref(),
        v => v,
    };
    match (key, value) {
        ("color-scheme", Value::U32(n)) => {
            let scheme = match n {
                1 => ColorScheme::PreferDark,
                2 => ColorScheme::PreferLight,
                _ => ColorScheme::NoPreference,
            };
            Some(SystemSetting::Dark {
                dark: scheme == ColorScheme::PreferDark,
                scheme,
            })
        }
        ("contrast", Value::U32(n)) => Some(SystemSetting::Contrast(if *n == 1 {
            Contrast::High
        } else {
            Contrast::Normal
        })),
        ("accent-color", Value::Structure(s)) => {
            let rgb: Vec<f64> = s
                .fields()
                .iter()
                .filter_map(|f| match f {
                    Value::F64(x) => Some(*x),
                    _ => None,
                })
                .collect();
            let [r, g, b] = rgb[..] else {
                return None;
            };
            let unit = |x: f64| (0.0..=1.0).contains(&x);
            Some(SystemSetting::Accent(
                (unit(r) && unit(g) && unit(b)).then_some([r, g, b]),
            ))
        }
        _ => None,
    }
}

async fn read_setting(proxy: &SettingsProxy<'_>, key: &str) -> Option<SystemSetting> {
    let value = match proxy.read_one(APPEARANCE, key).await {
        Ok(v) => v,
        // Portal version 1 has only `Read`.
        Err(zbus::Error::MethodError(name, _, _))
            if name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod" =>
        {
            proxy.read(APPEARANCE, key).await.ok()?
        }
        Err(_) => return None,
    };
    parse_setting(key, &value)
}

/// How long the boot read waits for the portal before sending what it
/// has. A portal frontend stuck on a hung backend at login answers only
/// after its own 25 s timeout; the rest follows as a later batch.
pub const BOOT_READ_TIMEOUT: Duration = Duration::from_millis(500);
/// How long connecting to the bus and subscribing may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// One key's read: a value (or `None` when the portal has none), or late.
enum Read {
    Done(Option<SystemSetting>),
    Late,
}

async fn read_key(proxy: &SettingsProxy<'_>, key: &str, limit: Option<Duration>) -> Read {
    match limit {
        Some(t) => match tokio::time::timeout(t, read_setting(proxy, key)).await {
            Ok(v) => Read::Done(v),
            Err(_) => Read::Late,
        },
        None => Read::Done(read_setting(proxy, key).await),
    }
}

/// Read `keys` concurrently. Returns the settings read and the keys that
/// did not answer within `limit`.
async fn read_keys(
    proxy: &SettingsProxy<'_>,
    keys: &[&'static str],
    limit: Option<Duration>,
) -> (Vec<SystemSetting>, Vec<&'static str>) {
    let one = |key: &'static str| async move {
        if keys.contains(&key) {
            read_key(proxy, key, limit).await
        } else {
            Read::Done(None)
        }
    };
    let (a, b, c) = tokio::join!(one(KEYS[0]), one(KEYS[1]), one(KEYS[2]));
    let mut settings = Vec::new();
    let mut late = Vec::new();
    for (key, read) in KEYS.into_iter().zip([a, b, c]) {
        match read {
            Read::Done(Some(s)) => settings.push(s),
            Read::Done(None) => {}
            Read::Late => late.push(key),
        }
    }
    (settings, late)
}

/// Read `keys` with no short limit (a late boot read, or a portal that
/// just appeared).
async fn read_late(proxy: &SettingsProxy<'_>, keys: Vec<&'static str>) -> Vec<SystemSetting> {
    read_keys(proxy, &keys, None).await.0
}

fn send(sink: &EventSink, settings: Vec<SystemSetting>, at_boot: bool) -> bool {
    sink.send(ChangeEvent::System(SystemBatch {
        settings,
        at_boot,
        received: Instant::now(),
    }))
}

fn timed_out() -> zbus::Error {
    zbus::Error::Failure("timed out".into())
}

enum Next {
    Changed(Option<SettingChanged>),
    Owner(Option<Option<zbus::names::UniqueName<'static>>>),
    Read(Vec<SystemSetting>),
}

type ReadFuture<'a> = Pin<Box<dyn Future<Output = Vec<SystemSetting>> + Send + 'a>>;

/// Follow the portal's appearance settings on `conn` until the sink's
/// receiver or the connection goes away. This is the async core of
/// [`PortalSettings`]; `strand-services` can run it on the shared services
/// runtime and session connection instead.
///
/// Sends the boot batch (`at_boot: true`, after at most
/// [`BOOT_READ_TIMEOUT`]; empty when there is no portal), then one batch
/// per `SettingChanged`. Keys the boot read did not get in time, and all
/// three keys whenever the portal (re)starts (a new owner of
/// `org.freedesktop.portal.Desktop`), are read again and sent with
/// `at_boot: false`. If subscribing fails the boot batch is still sent
/// (empty) and the error returned.
pub async fn follow(conn: &zbus::Connection, sink: EventSink) -> zbus::Result<()> {
    let setup = async {
        let proxy = SettingsProxy::new(conn).await?;
        // Subscribe before reading, so a change between the two is not
        // lost.
        let changes = proxy.receive_setting_changed().await?;
        let owners = proxy.inner().receive_owner_changed().await?;
        zbus::Result::Ok((proxy, changes, owners))
    };
    let (proxy, mut changes, mut owners) = match tokio::time::timeout(CONNECT_TIMEOUT, setup).await
    {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            send(&sink, Vec::new(), true);
            return Err(e);
        }
        Err(_) => {
            send(&sink, Vec::new(), true);
            return Err(timed_out());
        }
    };
    let (boot, late) = read_keys(&proxy, &KEYS, Some(BOOT_READ_TIMEOUT)).await;
    if !send(&sink, boot, true) {
        return Ok(());
    }
    let mut pending: Option<ReadFuture<'_>> =
        (!late.is_empty()).then(|| Box::pin(read_late(&proxy, late)) as ReadFuture<'_>);
    // Settings that changed (by signal) while `pending` was in flight: the
    // signal is at least as new as the read's answer, so the read's value
    // for them is dropped rather than sent after it as a stale revert.
    let mut newer: Vec<&'static str> = Vec::new();
    loop {
        let next = tokio::select! {
            s = changes.next() => Next::Changed(s),
            o = owners.next() => Next::Owner(o),
            r = async {
                match pending.as_mut() {
                    Some(read) => read.await,
                    None => std::future::pending().await,
                }
            }, if pending.is_some() => Next::Read(r),
        };
        let sent = match next {
            Next::Changed(None) | Next::Owner(None) => return Ok(()),
            Next::Changed(Some(signal)) => {
                let Ok(args) = signal.args() else {
                    continue;
                };
                if *args.namespace() != APPEARANCE {
                    continue;
                }
                match parse_setting(args.key(), args.value()) {
                    Some(s) => {
                        if pending.is_some() && !newer.contains(&s.path()) {
                            newer.push(s.path());
                        }
                        send(&sink, vec![s], false)
                    }
                    None => continue,
                }
            }
            // The portal (re)started: its values may differ from ours.
            Next::Owner(Some(Some(_))) => {
                pending = Some(Box::pin(read_late(&proxy, KEYS.to_vec())));
                newer.clear();
                continue;
            }
            // The portal went away; keep the last values.
            Next::Owner(Some(None)) => continue,
            Next::Read(mut settings) => {
                pending = None;
                settings.retain(|s| !newer.contains(&s.path()));
                newer.clear();
                if settings.is_empty() {
                    continue;
                }
                send(&sink, settings, false)
            }
        };
        if !sent {
            return Ok(());
        }
    }
}

async fn connect(bus: Bus) -> zbus::Result<zbus::Connection> {
    let connect = async {
        match bus {
            Bus::Session => zbus::Connection::session().await,
            Bus::Address(a) => {
                zbus::connection::Builder::address(a.as_str())?
                    .build()
                    .await
            }
        }
    };
    tokio::time::timeout(CONNECT_TIMEOUT, connect)
        .await
        .unwrap_or_else(|_| Err(timed_out()))
}

/// Follows the portal's appearance settings on its own thread, with its
/// own connection and current-thread runtime (see [`follow`] to share
/// both). The boot read is always sent (empty when there is no bus or
/// portal), then changes. Dropping it stops the thread at once, even
/// while a portal call is outstanding.
#[derive(Debug)]
pub struct PortalSettings {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl PortalSettings {
    /// Connect to `bus` and start following. Connection failures (no
    /// session bus) are reported by the thread as an empty boot batch.
    pub fn spawn(bus: Bus, sink: EventSink) -> io::Result<PortalSettings> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("strand-portal".into())
            .spawn(move || {
                rt.block_on(async move {
                    let run = async {
                        match connect(bus).await {
                            Ok(conn) => {
                                let _ = follow(&conn, sink).await;
                            }
                            Err(_) => {
                                send(&sink, Vec::new(), true);
                            }
                        }
                    };
                    tokio::select! {
                        _ = stop_rx => {}
                        () = run => {}
                    }
                });
            })?;
        Ok(PortalSettings {
            stop: Some(stop_tx),
            thread: Some(thread),
        })
    }
}

impl Drop for PortalSettings {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Structure;

    #[test]
    fn settings_are_typed() {
        assert_eq!(
            parse_setting("color-scheme", &Value::U32(1)),
            Some(SystemSetting::Dark {
                dark: true,
                scheme: ColorScheme::PreferDark
            })
        );
        assert_eq!(
            parse_setting("color-scheme", &Value::Value(Box::new(Value::U32(2)))),
            Some(SystemSetting::Dark {
                dark: false,
                scheme: ColorScheme::PreferLight
            })
        );
        assert_eq!(
            parse_setting("contrast", &Value::U32(1)),
            Some(SystemSetting::Contrast(Contrast::High))
        );
        let accent = Value::Structure(Structure::from((0.2f64, 0.4f64, 1.0f64)));
        assert_eq!(
            parse_setting("accent-color", &accent),
            Some(SystemSetting::Accent(Some([0.2, 0.4, 1.0])))
        );
        let unset = Value::Structure(Structure::from((-1.0f64, -1.0f64, -1.0f64)));
        assert_eq!(
            parse_setting("accent-color", &unset),
            Some(SystemSetting::Accent(None))
        );
        assert_eq!(parse_setting("contrast", &Value::Str("x".into())), None);
        assert_eq!(parse_setting("reduced-motion", &Value::U32(1)), None);
    }
}
