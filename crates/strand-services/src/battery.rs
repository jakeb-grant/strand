//! `battery`: UPower's combined display device (`/org/freedesktop/UPower/
//! devices/DisplayDevice`) and every other power source it reports, on
//! the system bus.
//!
//! The service follows `org.freedesktop.UPower` ([`Daemon`]): the
//! display device's and the devices' `PropertiesChanged` and UPower's
//! `DeviceAdded`/`DeviceRemoved` are applied as they come; UPower
//! restarting is read afresh without a shell reload, and UPower missing
//! is "no battery" (`present` false) until it appears. Nothing polls.
//!
//! (services-round) `battery.history(span, points)` asks UPower's
//! `GetHistory("charge", …)` of the first battery that keeps one, in a
//! task of the body's own (so a slow answer holds up nothing), and
//! resamples its records ([`resample`]) to `points` evenly spaced
//! charges, oldest first, the last at the moment it was asked. Only
//! when asked: nothing is read for it otherwise.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::dbus::{self, Daemon, DaemonEvent, Props};
use crate::{Call, Cx, Msg, Reply, ServiceError, Store, service};

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

/// (services-round) `battery`'s async methods.
#[derive(Call, Debug)]
pub enum BatteryCall {
    /// `battery.history(span, points)`: `points` null asks
    /// [`HISTORY_POINTS`].
    History { span: Duration, points: Option<i64> },
}

/// How many charges `battery.history(span)` gives when `points` is left
/// out.
pub const HISTORY_POINTS: usize = 60;

/// The most charges `battery.history` gives (a larger `points` asks
/// this many).
pub const MAX_HISTORY_POINTS: usize = 2000;

/// See the module docs.
#[service(name = "battery", call = BatteryCall)]
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
        // (services-round) `history` calls being answered.
        let mut asks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
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
                    Some(_) = asks.join_next(), if !asks.is_empty() => {}
                    m = cx.recv() => match m {
                        None => return Ok(()),
                        Some(Msg::Call(BatteryCall::History { span, points }, reply)) => {
                            upower.history(&daemon, &mut asks, span, points, reply);
                        }
                        Some(_) => {}
                    },
                }
            }
        }
    }
}

/// UPower's `GetHistory` type for the charge.
const CHARGE: &str = "charge";

/// `points` charges (0 to 1) evenly spaced over the `span` before `now`
/// (unix seconds), oldest first, the last at `now`: each the newest of
/// `records` (UPower's `(time, percent)`, in any order) at or before
/// its time. Points before the first record are left out, so the list
/// is shorter than `points` when the records start within the span (it
/// still ends at `now`; a chart aligns it right); no records, none.
pub fn resample(records: &[(u64, f64)], now: u64, span: Duration, points: usize) -> Vec<f64> {
    let mut records: Vec<(u64, f64)> = records
        .iter()
        .copied()
        .filter(|(_, p)| p.is_finite())
        .collect();
    records.sort_by_key(|(t, _)| *t);
    if records.is_empty() || points == 0 {
        return Vec::new();
    }
    let now = now as f64;
    let span = span.as_secs_f64();
    let step = if points > 1 {
        span / (points - 1) as f64
    } else {
        0.0
    };
    (0..points)
        .filter_map(|i| {
            let at = now - span + step * i as f64;
            let at = if i + 1 == points { now } else { at };
            records
                .iter()
                .take_while(|(t, _)| *t as f64 <= at)
                .last()
                .map(|(_, p)| (p / 100.0).clamp(0.0, 1.0))
        })
        .collect()
}

/// Asks `path`'s charge history over `span` and resamples it.
async fn history(
    conn: zbus::Connection,
    path: String,
    span: Duration,
    points: usize,
) -> Result<Vec<f64>, String> {
    // UPower's resolution is about how many records it gives: asked
    // for twice the points, so each point has its own record.
    let resolution = u32::try_from(points.saturating_mul(2)).unwrap_or(u32::MAX);
    let seconds = u32::try_from(span.as_secs().max(1)).unwrap_or(u32::MAX);
    let answer = dbus::timed(conn.call_method(
        Some(UPOWER),
        path.as_str(),
        Some(DEVICE),
        "GetHistory",
        &(CHARGE, seconds, resolution),
    ))
    .await
    .map_err(|e| format!("UPower kept no history: {e}"))?;
    let records: Vec<(u32, f64, u32)> = answer
        .body()
        .deserialize()
        .map_err(|e| format!("UPower's history did not read: {e}"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let records: Vec<(u64, f64)> = records
        .into_iter()
        // A record of unknown state holds no charge.
        .filter(|(_, _, state)| *state != 0)
        .map(|(t, p, _)| (u64::from(t), p))
        .collect();
    Ok(resample(&records, now, span, points))
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

    /// The battery whose history `history` asks: the first (by path)
    /// that is a laptop battery (`Type` 2) powering the machine
    /// (`PowerSupply`) and keeps a history (`HasHistory`; one that does
    /// not say counts as keeping one).
    fn history_device(&self) -> Option<String> {
        self.devices
            .iter()
            .find(|(_, p)| {
                dbus::number(p, "Type") == Some(2.0)
                    && dbus::boolean(p, "PowerSupply") != Some(false)
                    && dbus::boolean(p, "HasHistory") != Some(false)
            })
            .map(|(path, _)| path.clone())
    }

    /// Answers `battery.history(span, points)` in a task of `asks`.
    fn history(
        &self,
        daemon: &Daemon,
        asks: &mut tokio::task::JoinSet<()>,
        span: Duration,
        points: Option<i64>,
        reply: Reply,
    ) {
        let points = points.map_or(HISTORY_POINTS, |n| {
            usize::try_from(n.max(1))
                .unwrap_or(1)
                .min(MAX_HISTORY_POINTS)
        });
        let Some(path) = self.history_device() else {
            reply.send::<Vec<f64>, _>(Err(if daemon.owner().is_none() {
                "UPower is not running"
            } else {
                "no battery keeps a history"
            }));
            return;
        };
        let conn = daemon.conn().clone();
        asks.spawn(async move {
            let r = history(conn, path, span, points).await;
            reply.send(r);
        });
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
        // (services-round) The history's battery: a laptop battery
        // powering the machine, not a mouse's.
        let mut upower = UPower {
            devices,
            ..UPower::default()
        };
        assert_eq!(upower.history_device(), None);
        upower.devices.insert(
            "/x/bat0".to_string(),
            props(&[
                ("Type", OwnedValue::from(2u32)),
                ("PowerSupply", OwnedValue::from(true)),
            ]),
        );
        upower.devices.insert(
            "/x/bat1".to_string(),
            props(&[("Type", OwnedValue::from(2u32))]),
        );
        assert_eq!(upower.history_device().as_deref(), Some("/x/bat0"));
        assert_eq!(
            level_icon(0.96, true),
            "battery-level-100-charging-symbolic"
        );
    }

    /// (services-round) Records become evenly spaced charges, oldest
    /// first, the last at now; times before the first record are left
    /// out; records come in any order.
    #[test]
    fn a_history_is_resampled_to_even_points() {
        let now = 10_000;
        let hour = Duration::from_secs(3600);
        // Newest first, as UPower answers.
        let records = [(now - 100, 40.0), (now - 1800, 60.0), (now - 3600, 80.0)];
        assert_eq!(
            resample(&records, now, hour, 5),
            [0.8, 0.8, 0.6, 0.6, 0.4],
            "every 15 minutes: the last record at or before each"
        );
        // Records starting within the span: the earlier points go.
        let late = [(now - 1000, 50.0), (now - 10, 49.0)];
        assert_eq!(resample(&late, now, hour, 5), [0.5, 0.49]);
        // One point is now's.
        assert_eq!(resample(&records, now, hour, 1), [0.4]);
        assert!(resample(&[], now, hour, 5).is_empty());
        assert!(resample(&records, now, hour, 0).is_empty());
        // Percentages past 100 or below 0 clamp; not numbers are dropped.
        let odd = [(now - 3600, 120.0), (now - 1800, f64::NAN), (now - 5, -3.0)];
        assert_eq!(resample(&odd, now, hour, 3), [1.0, 1.0, 0.0]);
    }
}
