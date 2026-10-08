//! `battery`: UPower's combined display device (`/org/freedesktop/UPower/
//! devices/DisplayDevice`) and every other power source it reports, on
//! the system bus.
//!
//! The service follows `org.freedesktop.UPower` ([`Daemon`]): the
//! display device's and the devices' `PropertiesChanged` and UPower's
//! `DeviceAdded`/`DeviceRemoved` are applied as they come; UPower
//! restarting is read afresh without a shell reload, and UPower missing
//! is "no battery" (`present` false) until it appears. Nothing polls.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::dbus::{self, Daemon, DaemonEvent, Props};
use crate::{Cx, ServiceError, Store, service};

/// The schema the `battery` service serves.
pub const SCHEMA: &str = strand_services_schema::BATTERY;

/// UPower's bus name.
pub const UPOWER: &str = "org.freedesktop.UPower";
const ROOT: &str = "/org/freedesktop/UPower";
const DISPLAY: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
const DEVICE: &str = "org.freedesktop.UPower.Device";

/// A power source besides the combined battery.
#[derive(crate::Data, Clone, Debug, Default, PartialEq)]
#[data(name = "PowerDevice", key = id)]
pub struct PowerDevice {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub percent: f64,
    pub charging: bool,
    pub icon: String,
}

/// See the module docs.
#[service(name = "battery")]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Battery {
    /// A battery is present: `if battery.present { Battery }`.
    pub present: bool,
    /// Charge, 0 to 1: `pct(battery.percent)`.
    pub percent: f64,
    /// It is charging.
    pub charging: bool,
    /// Time until empty, or until full while charging; null when unknown.
    pub time_left: Option<Duration>,
    /// An icon name for its charge and state.
    pub icon: String,
    /// Power draw in watts.
    pub power: f64,
    /// Every power source but line power, keyed by `id`: laptop batteries
    /// and the batteries of peripherals.
    #[store(keyed)]
    pub devices: Vec<PowerDevice>,
}

/// UPower's device kinds (`Type`), as `PowerDevice.kind` spells them.
fn kind(t: u32) -> &'static str {
    match t {
        2 => "battery",
        3 => "ups",
        5 => "mouse",
        6 => "keyboard",
        8 => "phone",
        10 => "tablet",
        // Headset, headphones, other audio (speakers are `other`).
        17 | 19 | 21 => "headset",
        12 => "gaming_input",
        _ => "other",
    }
}

/// UPower's `State`: 1 charging (2 discharging, 4 fully charged, 5
/// pending charge, 6 pending discharge).
fn charging(props: &Props) -> bool {
    dbus::number(props, "State") == Some(1.0)
}

/// UPower's percentage (0 to 100) as a fraction.
fn fraction(props: &Props) -> f64 {
    (dbus::number(props, "Percentage").unwrap_or(0.0) / 100.0).clamp(0.0, 1.0)
}

/// The icon UPower names, or one for the charge and state.
fn icon(props: &Props) -> String {
    match dbus::text(props, "IconName") {
        Some(name) if !name.is_empty() => name,
        _ => level_icon(fraction(props), charging(props)),
    }
}

/// `battery-level-50-charging-symbolic`: the level rounded to tens.
pub fn level_icon(fraction: f64, charging: bool) -> String {
    let level = ((fraction * 10.0).round() as u32 * 10).min(100);
    if level == 100 && !charging {
        return "battery-level-100-charged-symbolic".to_string();
    }
    format!(
        "battery-level-{level}{}-symbolic",
        if charging { "-charging" } else { "" }
    )
}

impl Battery {
    /// The state from UPower's display device and its other devices (by
    /// object path).
    pub fn from_upower(display: &Props, devices: &BTreeMap<String, Props>) -> Battery {
        let is_charging = charging(display);
        let seconds = dbus::number(
            display,
            if is_charging {
                "TimeToFull"
            } else {
                "TimeToEmpty"
            },
        )
        .unwrap_or(0.0);
        let present = dbus::boolean(display, "IsPresent").unwrap_or(false);
        Battery {
            present,
            percent: fraction(display),
            charging: is_charging,
            time_left: (present && seconds > 0.0).then(|| Duration::from_secs(seconds as u64)),
            icon: if present {
                icon(display)
            } else {
                String::new()
            },
            power: dbus::number(display, "EnergyRate").unwrap_or(0.0).abs(),
            devices: devices
                .iter()
                .filter(|(_, p)| dbus::number(p, "Type").unwrap_or(0.0) != 1.0)
                .map(|(path, p)| PowerDevice {
                    id: path.clone(),
                    name: dbus::text(p, "Model")
                        .filter(|m| !m.is_empty())
                        .or_else(|| dbus::text(p, "NativePath"))
                        .unwrap_or_default(),
                    kind: kind(dbus::number(p, "Type").unwrap_or(0.0) as u32).to_string(),
                    percent: fraction(p),
                    charging: charging(p),
                    icon: icon(p),
                })
                .collect(),
        }
    }

    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let conn = match cx.system().await {
            Ok(c) => c,
            Err(e) => return dbus::idle_without_bus(&mut cx, "system", e).await,
        };
        let mut daemon = Daemon::follow(&conn, UPOWER, ROOT).await?;
        let mut upower = UPower::default();
        loop {
            // (Re)read everything from the current owner, if any.
            upower
                .read(&daemon)
                .await
                .map_err(|e| ServiceError(format!("UPower did not answer: {e}")))?;
            if !cx.update(|s| *s = upower.state()) {
                return Ok(());
            }
            cx.ready();
            loop {
                tokio::select! {
                    ev = daemon.next() => match ev {
                        None => return Err(ServiceError("the system bus connection ended".into())),
                        Some(DaemonEvent::Owner) => break,
                        Some(DaemonEvent::Signal(m)) => {
                            if upower.signal(&daemon, &m).await
                                && !cx.update(|s| *s = upower.state())
                            {
                                return Ok(());
                            }
                        }
                    },
                    m = cx.recv() => if m.is_none() {
                        return Ok(());
                    },
                }
            }
        }
    }
}

/// UPower's properties as last read.
#[derive(Default)]
struct UPower {
    display: Props,
    devices: BTreeMap<String, Props>,
}

impl UPower {
    fn state(&self) -> Battery {
        Battery::from_upower(&self.display, &self.devices)
    }

    /// Read everything; a hung UPower (no answer in
    /// [`dbus::READ_TIMEOUT`]) is an error: the run fails and is retried.
    async fn read(&mut self, daemon: &Daemon) -> zbus::Result<()> {
        self.display.clear();
        self.devices.clear();
        if daemon.owner().is_none() {
            return Ok(());
        }
        let conn = daemon.conn();
        match dbus::get_all(conn, UPOWER, DISPLAY, DEVICE).await {
            Ok(p) => self.display = p,
            Err(e) if dbus::is_timeout(&e) => return Err(e),
            Err(e) => log::debug!("battery: no display device: {e}"),
        }
        let listed = dbus::timed_for(
            dbus::READ_TIMEOUT,
            conn.call_method(Some(UPOWER), ROOT, Some(UPOWER), "EnumerateDevices", &()),
        )
        .await
        .and_then(|r| {
            r.body()
                .deserialize::<Vec<zbus::zvariant::OwnedObjectPath>>()
        });
        match listed {
            Ok(paths) => {
                for path in dbus::paths(paths) {
                    self.add(daemon, path).await;
                }
            }
            Err(e) if dbus::is_timeout(&e) => return Err(e),
            Err(e) => log::debug!("battery: no devices: {e}"),
        }
        Ok(())
    }

    async fn add(&mut self, daemon: &Daemon, path: String) {
        if path == DISPLAY {
            return;
        }
        if let Ok(p) = dbus::get_all(daemon.conn(), UPOWER, &path, DEVICE).await {
            self.devices.insert(path, p);
        }
    }

    /// Apply a signal; whether anything changed.
    async fn signal(&mut self, daemon: &Daemon, m: &zbus::Message) -> bool {
        if let Some(c) = dbus::properties_changed(m) {
            if c.iface != DEVICE {
                return false;
            }
            let props = if c.path == DISPLAY {
                &mut self.display
            } else if let Some(p) = self.devices.get_mut(&c.path) {
                p
            } else {
                return false;
            };
            dbus::apply_changed(daemon.conn(), UPOWER, props, c).await;
            return true;
        }
        if dbus::interface(m).as_deref() != Some(UPOWER) {
            return false;
        }
        let Ok(path) = m.body().deserialize::<zbus::zvariant::OwnedObjectPath>() else {
            return false;
        };
        match dbus::member(m).as_deref() {
            Some("DeviceAdded") => {
                self.add(daemon, path.to_string()).await;
                true
            }
            Some("DeviceRemoved") => self.devices.remove(path.as_str()).is_some(),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::OwnedValue;

    fn props(entries: &[(&str, OwnedValue)]) -> Props {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.try_clone().unwrap()))
            .collect()
    }

    #[test]
    fn upower_properties_become_the_battery() {
        let display = props(&[
            ("IsPresent", OwnedValue::from(true)),
            ("Percentage", OwnedValue::from(42.0f64)),
            ("State", OwnedValue::from(2u32)),
            ("TimeToEmpty", OwnedValue::from(3600i64)),
            ("TimeToFull", OwnedValue::from(0i64)),
            ("EnergyRate", OwnedValue::from(7.5f64)),
            ("IconName", OwnedValue::from(zbus::zvariant::Str::from(""))),
        ]);
        let mut devices = BTreeMap::new();
        devices.insert(
            "/x/ac".to_string(),
            props(&[("Type", OwnedValue::from(1u32))]),
        );
        devices.insert(
            "/x/mouse".to_string(),
            props(&[
                ("Type", OwnedValue::from(5u32)),
                ("Percentage", OwnedValue::from(80.0f64)),
                ("Model", OwnedValue::from(zbus::zvariant::Str::from("MX"))),
            ]),
        );
        let b = Battery::from_upower(&display, &devices);
        assert!(b.present && !b.charging);
        assert!((b.percent - 0.42).abs() < 1e-9);
        assert_eq!(b.time_left, Some(Duration::from_secs(3600)));
        assert_eq!(b.icon, "battery-level-40-symbolic");
        assert_eq!(b.power, 7.5);
        assert_eq!(b.devices.len(), 1, "line power is left out");
        assert_eq!(b.devices[0].kind, "mouse");
        assert_eq!(b.devices[0].name, "MX");
        // UPower's kinds: 17 headset, 18 speakers, 19 headphones, 23 printer.
        assert_eq!(kind(17), "headset");
        assert_eq!(kind(19), "headset");
        assert_eq!(kind(18), "other");
        assert_eq!(kind(23), "other");
        // No UPower: no battery.
        assert_eq!(
            Battery::from_upower(&Props::new(), &BTreeMap::new()),
            Battery::default()
        );
        assert_eq!(level_icon(1.0, false), "battery-level-100-charged-symbolic");
        assert_eq!(
            level_icon(0.96, true),
            "battery-level-100-charging-symbolic"
        );
    }
}
