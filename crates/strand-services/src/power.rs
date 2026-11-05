//! `power`: the power profile, from power-profiles-daemon on the system
//! bus.
//!
//! power-profiles-daemon 0.20 and later own
//! `org.freedesktop.UPower.PowerProfiles` (object
//! `/org/freedesktop/UPower/PowerProfiles`) and the older
//! `net.hadess.PowerProfiles` (`/net/hadess/PowerProfiles`) both; older
//! ones own only the older name. The service follows both names
//! ([`Daemon::watch`]) and reads the newer whenever it has an owner, else
//! the older ([`choose`]); any owner change of either reads afresh,
//! without the service restarting. The chosen object's
//! `PropertiesChanged` is applied as it comes; nothing polls.
//!
//! The daemon is never D-Bus-activated (decisions.md power-media-kbd):
//! TLP, auto-cpufreq and tuned conflict with it, and starting it for a
//! shell would fight them. No daemon is `available` false.
//!
//! Writing `profile` sets the daemon's `ActiveProfile`. A profile it does
//! not offer (`Profiles`), a value that is no profile, or a write while
//! no daemon answers is not sent, and the profile stays as it was; a
//! write the daemon refuses is logged and reported as last read, as
//! `brightness` and `bluetooth.powered` refuse writes. Holds
//! (`HoldProfile`) are not this service's: a config declares them as a
//! `from dbus` action.
//!
//! A daemon can settle on another profile right after a write it took
//! (decisions.md power-reask): on firmware with no low-power profile,
//! power-profiles-daemon writes `balanced` to the firmware for
//! `power-saver` and then reads its own write back as a switch to
//! `balanced`. So when the daemon reports another profile within
//! [`SETTLE`] of a write, the written one is asked once more before the
//! change is shown; if the daemon settles elsewhere again, that is
//! followed.

use std::time::Duration;

use tokio::time::Instant;
use zbus::zvariant::{OwnedValue, Str, Value};

use crate::dbus::{self, Daemon, DaemonEvent, Props};
use crate::{Cx, Msg, ServiceError, Store, service};

/// The schema the `power` service serves.
pub const SCHEMA: &str = strand_services_schema::POWER;

/// power-profiles-daemon's bus name (0.20 and later), also its interface.
pub const PPD: &str = "org.freedesktop.UPower.PowerProfiles";
/// Its object under [`PPD`].
pub const PPD_PATH: &str = "/org/freedesktop/UPower/PowerProfiles";
/// The older bus name (before 0.20, and still owned by later ones), also
/// its interface.
pub const LEGACY: &str = "net.hadess.PowerProfiles";
/// Its object under [`LEGACY`].
pub const LEGACY_PATH: &str = "/net/hadess/PowerProfiles";

/// How long after a write the daemon settling on another profile counts
/// as its quirk, so the written profile is asked once more (the module
/// docs). A switch made later, by a key or another tool, is followed.
pub const SETTLE: Duration = Duration::from_secs(2);

/// A power-profiles-daemon profile (`power.profile`).
#[derive(crate::Data, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[data(name = "PowerProfile")]
pub enum PowerProfile {
    /// `power-saver`.
    PowerSaver,
    /// `balanced`: what every machine offers.
    #[default]
    Balanced,
    /// `performance`, offered only where a driver supports it.
    Performance,
}

impl PowerProfile {
    /// The profile the daemon names `name`, if it is one.
    pub fn of_ppd(name: &str) -> Option<PowerProfile> {
        match name {
            "power-saver" => Some(PowerProfile::PowerSaver),
            "balanced" => Some(PowerProfile::Balanced),
            "performance" => Some(PowerProfile::Performance),
            _ => None,
        }
    }

    /// The daemon's name for it.
    pub fn ppd_name(self) -> &'static str {
        match self {
            PowerProfile::PowerSaver => "power-saver",
            PowerProfile::Balanced => "balanced",
            PowerProfile::Performance => "performance",
        }
    }
}

/// See the module docs.
#[service(name = "power")]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Power {
    /// The active profile: `power.profile = power_saver` in a handler, or
    /// `strand set power.profile performance`. Writable: the daemon is asked
    /// to switch (its `ActiveProfile`). A profile it does not offer
    /// (`profiles`), or a write while it is not `available`, leaves the
    /// profile as it was. A daemon that settles on another profile within
    /// two seconds of a write it took is asked once more, then followed.
    /// `balanced` while no daemon answers.
    #[store(rw)]
    pub profile: PowerProfile,
    /// The profiles the machine offers, in the daemon's order: `for p in
    /// power.profiles key p { … }`. `performance` is listed only where a
    /// driver supports it. Empty while no daemon answers.
    pub profiles: Vec<PowerProfile>,
    /// power-profiles-daemon answers. False without it, or without a system
    /// bus, so a menu can hide what cannot work.
    pub available: bool,
    /// Why `performance` is held back, as the daemon names it
    /// (`lap-detected`, `high-operating-temperature`); null when it is not.
    pub degraded: Option<String>,
}

/// Which of the daemon's two names the service reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// [`PPD`].
    Ppd,
    /// [`LEGACY`].
    Legacy,
}

impl Backend {
    /// Its bus name, which is also its interface's name.
    pub fn name(self) -> &'static str {
        match self {
            Backend::Ppd => PPD,
            Backend::Legacy => LEGACY,
        }
    }

    /// Its object.
    pub fn path(self) -> &'static str {
        match self {
            Backend::Ppd => PPD_PATH,
            Backend::Legacy => LEGACY_PATH,
        }
    }
}

/// The name to read, given which have an owner: the newer one whenever it
/// has one, else the older, else none.
pub fn choose(new_owned: bool, old_owned: bool) -> Option<Backend> {
    if new_owned {
        Some(Backend::Ppd)
    } else if old_owned {
        Some(Backend::Legacy)
    } else {
        None
    }
}

/// The `Profiles` property (`aa{sv}`): each dict's `Profile`, in order,
/// the ones this service knows only.
fn profiles(props: &Props) -> Vec<PowerProfile> {
    let Some(v) = props.get("Profiles") else {
        return Vec::new();
    };
    let v: &Value<'_> = v;
    let v = match v {
        Value::Value(inner) => &**inner,
        v => v,
    };
    let Value::Array(list) = v else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|d| {
            let d = OwnedValue::try_from(d.try_clone().ok()?).ok()?;
            let d = Props::try_from(d).ok()?;
            PowerProfile::of_ppd(&dbus::text(&d, "Profile")?)
        })
        .collect()
}

impl Power {
    /// The state from the daemon's properties (`GetAll` of its
    /// interface): an `ActiveProfile` this service does not know reads
    /// `balanced`, a profile it does not know is left out of `profiles`,
    /// and an empty `PerformanceDegraded` is null.
    pub fn from_props(props: &Props) -> Power {
        Power {
            profile: dbus::text(props, "ActiveProfile")
                .and_then(|p| PowerProfile::of_ppd(&p))
                .unwrap_or_default(),
            profiles: profiles(props),
            available: true,
            degraded: dbus::text(props, "PerformanceDegraded").filter(|d| !d.is_empty()),
        }
    }

    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let conn = match cx.system().await {
            Ok(c) => c,
            Err(e) => return dbus::idle_without_bus(&mut cx, "system", e).await,
        };
        // Watched, never started (the module docs); each subscribed to
        // its object's signals before the first read.
        let mut new = Daemon::watch(&conn, PPD).await?;
        new.subscribe("main", dbus::path_rule(PPD_PATH)?).await?;
        let mut old = Daemon::watch(&conn, LEGACY).await?;
        old.subscribe("main", dbus::path_rule(LEGACY_PATH)?).await?;
        loop {
            let mut backend = choose(new.owner().is_some(), old.owner().is_some());
            let mut props = Props::new();
            if let Some(b) = backend {
                match dbus::get_all(&conn, b.name(), b.path(), b.name()).await {
                    Ok(p) => props = p,
                    Err(e) if dbus::is_timeout(&e) => {
                        return Err(ServiceError(format!(
                            "power-profiles-daemon did not answer: {e}"
                        )));
                    }
                    Err(e) => {
                        log::debug!("power: {} not read: {e}", b.name());
                        backend = None;
                    }
                }
            }
            let state = match backend {
                Some(_) => Power::from_props(&props),
                None => Power::default(),
            };
            if !cx.update(|s| *s = state) {
                return Ok(());
            }
            cx.ready();
            // The profile a write just set, and until when the daemon
            // settling on another is asked once more (the module docs).
            let mut asked: Option<(PowerProfile, Instant)> = None;
            loop {
                // A signal from either name's owner, and which name.
                let signal = tokio::select! {
                    ev = new.next() => match ev {
                        None => return Err(ServiceError("the system bus connection ended".into())),
                        Some(DaemonEvent::Owner) => break,
                        Some(DaemonEvent::Signal(m)) => Some((Backend::Ppd, m)),
                    },
                    ev = old.next() => match ev {
                        None => return Err(ServiceError("the system bus connection ended".into())),
                        Some(DaemonEvent::Owner) => break,
                        Some(DaemonEvent::Signal(m)) => Some((Backend::Legacy, m)),
                    },
                    m = cx.recv() => match m {
                        None => return Ok(()),
                        Some(Msg::Write(w)) if w.field == "profile" => {
                            let (now, sent) =
                                write(&conn, backend, &mut props, cx.state(), w.value()).await;
                            asked = sent.then(|| (now, Instant::now() + SETTLE));
                            if !cx.report(&w, |s| s.profile = now) {
                                return Ok(());
                            }
                            None
                        }
                        Some(_) => None,
                    },
                };
                let Some((from, m)) = signal else {
                    continue;
                };
                // Only the chosen name's object counts.
                if backend != Some(from) {
                    continue;
                }
                let Some(c) = dbus::properties_changed(&m) else {
                    continue;
                };
                if c.iface != from.name() {
                    continue;
                }
                dbus::apply_changed(&conn, from.name(), &mut props, c).await;
                let mut state = Power::from_props(&props);
                if let Some((wanted, by)) = asked
                    && state.profile != wanted
                {
                    // Once only: what the daemon says next is followed.
                    asked = None;
                    if Instant::now() < by && state.profiles.contains(&wanted) {
                        log::info!(
                            "power: the daemon settled on {} after {} was set; asking once more",
                            state.profile.ppd_name(),
                            wanted.ppd_name()
                        );
                        if ask(&conn, from, &mut props, wanted).await.is_ok() {
                            state = Power::from_props(&props);
                        }
                    }
                }
                if !cx.update(|s| *s = state) {
                    return Ok(());
                }
            }
        }
    }
}

/// A `profile` write: the profile to report, and whether the daemon took
/// it. Sent only to a daemon that answers and offers it; refused, the
/// profile as last read.
async fn write(
    conn: &zbus::Connection,
    backend: Option<Backend>,
    props: &mut Props,
    state: &Power,
    wanted: Result<PowerProfile, crate::DataError>,
) -> (PowerProfile, bool) {
    let current = state.profile;
    let wanted = match wanted {
        Ok(p) => p,
        Err(e) => {
            log::info!("power: not a profile: {e}");
            return (current, false);
        }
    };
    let Some(b) = backend else {
        log::info!(
            "power: no power-profiles-daemon to switch to {}",
            wanted.ppd_name()
        );
        return (current, false);
    };
    if !state.profiles.contains(&wanted) {
        log::info!(
            "power: {} is not offered ({:?})",
            wanted.ppd_name(),
            state.profiles
        );
        return (current, false);
    }
    match ask(conn, b, props, wanted).await {
        Ok(()) => (wanted, true),
        Err(()) => (Power::from_props(props).profile, false),
    }
}

/// Set the daemon's `ActiveProfile` to `wanted`, and put it in `props` once
/// the daemon takes it; a refusal is logged.
async fn ask(
    conn: &zbus::Connection,
    b: Backend,
    props: &mut Props,
    wanted: PowerProfile,
) -> Result<(), ()> {
    let name = wanted.ppd_name();
    match dbus::set(
        conn,
        b.name(),
        b.path(),
        b.name(),
        "ActiveProfile",
        Value::from(name),
    )
    .await
    {
        Ok(()) => {
            // As the daemon's `PropertiesChanged` will say: a signal of
            // another property meanwhile does not undo it.
            if let Ok(v) = OwnedValue::try_from(Value::from(Str::from(name))) {
                props.insert("ActiveProfile".into(), v);
            }
            Ok(())
        }
        Err(e) => {
            log::warn!("power: the daemon refused {name}: {e}");
            Err(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn text(s: &str) -> OwnedValue {
        OwnedValue::try_from(Value::from(s.to_string())).unwrap()
    }

    fn offered(names: &[&str]) -> OwnedValue {
        let list: Vec<HashMap<String, Value<'static>>> = names
            .iter()
            .map(|n| {
                HashMap::from([
                    ("Profile".to_string(), Value::from(n.to_string())),
                    (
                        "Driver".to_string(),
                        Value::from("intel_pstate".to_string()),
                    ),
                    (
                        "CpuDriver".to_string(),
                        Value::from("amd_pstate".to_string()),
                    ),
                ])
            })
            .collect();
        OwnedValue::try_from(Value::from(list)).unwrap()
    }

    /// The daemon's names map to the profiles; what this service does not
    /// know is skipped (a profile) or read as `balanced` (the active one);
    /// an empty degraded reason is none; the dicts' other keys are
    /// ignored.
    #[test]
    fn the_daemons_properties_become_the_state() {
        for p in [
            PowerProfile::PowerSaver,
            PowerProfile::Balanced,
            PowerProfile::Performance,
        ] {
            assert_eq!(PowerProfile::of_ppd(p.ppd_name()), Some(p));
        }
        assert_eq!(PowerProfile::of_ppd("power_saver"), None);
        let props = Props::from([
            ("ActiveProfile".to_string(), text("power-saver")),
            (
                "Profiles".to_string(),
                offered(&["power-saver", "turbo", "balanced", "performance"]),
            ),
            ("PerformanceDegraded".to_string(), text("lap-detected")),
        ]);
        assert_eq!(
            Power::from_props(&props),
            Power {
                profile: PowerProfile::PowerSaver,
                profiles: vec![
                    PowerProfile::PowerSaver,
                    PowerProfile::Balanced,
                    PowerProfile::Performance
                ],
                available: true,
                degraded: Some("lap-detected".into()),
            }
        );
        let props = Props::from([
            ("ActiveProfile".to_string(), text("turbo")),
            (
                "Profiles".to_string(),
                offered(&["power-saver", "balanced"]),
            ),
            ("PerformanceDegraded".to_string(), text("")),
        ]);
        let p = Power::from_props(&props);
        assert_eq!(p.profile, PowerProfile::Balanced);
        assert_eq!(
            p.profiles,
            [PowerProfile::PowerSaver, PowerProfile::Balanced]
        );
        assert_eq!(p.degraded, None);
        assert!(p.available);
        // Nothing read: available, and nothing offered.
        assert_eq!(
            Power::from_props(&Props::new()),
            Power {
                available: true,
                ..Power::default()
            }
        );
        assert!(!Power::default().available);
        assert_eq!(Power::default().profile, PowerProfile::Balanced);
    }

    #[test]
    fn the_newer_name_is_chosen_when_both_run() {
        assert_eq!(choose(true, true), Some(Backend::Ppd));
        assert_eq!(choose(true, false), Some(Backend::Ppd));
        assert_eq!(choose(false, true), Some(Backend::Legacy));
        assert_eq!(choose(false, false), None);
        assert_eq!(Backend::Legacy.path(), "/net/hadess/PowerProfiles");
        assert_eq!(Backend::Ppd.name(), "org.freedesktop.UPower.PowerProfiles");
    }
}
