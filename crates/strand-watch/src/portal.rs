//! `org.freedesktop.portal.Settings` client: `ReadOne` at boot, then
//! `SettingChanged`, for `color-scheme`, `accent-color` and `contrast`.

use std::io;
use std::thread::JoinHandle;

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

async fn run(
    bus: Bus,
    sink: EventSink,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) -> zbus::Result<()> {
    let conn = match bus {
        Bus::Session => zbus::Connection::session().await?,
        Bus::Address(a) => {
            zbus::connection::Builder::address(a.as_str())?
                .build()
                .await?
        }
    };
    let proxy = SettingsProxy::new(&conn).await?;
    // Subscribe before reading, so a change between the two is not lost.
    let mut changes = proxy.receive_setting_changed().await?;
    let mut boot = Vec::new();
    for key in KEYS {
        if let Some(s) = read_setting(&proxy, key).await {
            boot.push(s);
        }
    }
    if !sink.send(ChangeEvent::System(SystemBatch {
        settings: boot,
        at_boot: true,
    })) {
        return Ok(());
    }
    loop {
        let signal = tokio::select! {
            _ = &mut stop => return Ok(()),
            s = changes.next() => s,
        };
        let Some(signal) = signal else {
            return Ok(());
        };
        let Ok(args) = signal.args() else {
            continue;
        };
        if *args.namespace() != APPEARANCE {
            continue;
        }
        if let Some(s) = parse_setting(args.key(), args.value())
            && !sink.send(ChangeEvent::System(SystemBatch {
                settings: vec![s],
                at_boot: false,
            }))
        {
            return Ok(());
        }
    }
}

/// Follows the portal's appearance settings on its own thread. The boot
/// read is always sent (empty when there is no portal), then one batch per
/// `SettingChanged`. Dropping it stops the thread.
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
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("strand-portal".into())
            .spawn(move || {
                let fallback = sink.clone();
                if rt.block_on(run(bus, sink, stop_rx)).is_err() {
                    fallback.send(ChangeEvent::System(SystemBatch {
                        settings: Vec::new(),
                        at_boot: true,
                    }));
                }
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
