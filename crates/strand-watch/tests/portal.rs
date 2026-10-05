//! The portal Settings client against a mock portal on a private
//! `dbus-daemon` (design.md, "Testing": small zbus mocks for the portal).

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use strand_watch::{
    Bus, ChangeEvent, ColorScheme, Contrast, PortalSettings, SystemBatch, SystemSetting, channel,
};
use zbus::zvariant::{OwnedValue, Value};

const PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.portal.Settings";
const NAME: &str = "org.freedesktop.portal.Desktop";

struct Daemon {
    child: Child,
    address: String,
    _dir: tempfile::TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A private session bus. Skips (returns `None`) when `dbus-daemon` is not
/// installed, unless `STRAND_REQUIRE_DBUS` is set.
fn daemon() -> Option<Daemon> {
    let dir = tempfile::tempdir().unwrap();
    let spawned = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .arg(format!("--address=unix:path={}/bus", dir.path().display()))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) if std::env::var_os("STRAND_REQUIRE_DBUS").is_none() => {
            eprintln!("skipping: dbus-daemon unavailable ({e})");
            return None;
        }
        Err(e) => panic!("dbus-daemon: {e}"),
    };
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    Some(Daemon {
        child,
        address: line.trim().to_string(),
        _dir: dir,
    })
}

struct Mock {
    values: HashMap<String, OwnedValue>,
    /// Portal version 1: no `ReadOne`.
    v1: bool,
    /// A key whose read hangs this long (a portal stuck on its backend).
    slow: Option<(&'static str, Duration)>,
}

fn mock(values: HashMap<String, OwnedValue>) -> Mock {
    Mock {
        values,
        v1: false,
        slow: None,
    }
}

#[zbus::interface(name = "org.freedesktop.portal.Settings")]
impl Mock {
    async fn read_one(&self, namespace: &str, key: &str) -> zbus::fdo::Result<OwnedValue> {
        if self.v1 {
            return Err(zbus::fdo::Error::UnknownMethod("ReadOne".into()));
        }
        if let Some((slow, delay)) = self.slow
            && slow == key
        {
            tokio::time::sleep(delay).await;
        }
        self.lookup(namespace, key)
    }

    async fn read(&self, namespace: &str, key: &str) -> zbus::fdo::Result<OwnedValue> {
        let inner = self.lookup(namespace, key)?;
        Ok(Value::Value(Box::new(inner.into())).try_into().unwrap())
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        if self.v1 { 1 } else { 2 }
    }
}

impl Mock {
    fn lookup(&self, namespace: &str, key: &str) -> zbus::fdo::Result<OwnedValue> {
        if namespace != "org.freedesktop.appearance" {
            return Err(zbus::fdo::Error::Failed("not found".into()));
        }
        self.values
            .get(key)
            .map(|v| v.try_clone().unwrap())
            .ok_or_else(|| zbus::fdo::Error::Failed("not found".into()))
    }
}

fn owned(v: Value<'_>) -> OwnedValue {
    v.try_into().unwrap()
}

fn appearance() -> HashMap<String, OwnedValue> {
    HashMap::from([
        ("color-scheme".into(), owned(Value::from(1u32))),
        (
            "accent-color".into(),
            owned(Value::from((0.2f64, 0.4f64, 1.0f64))),
        ),
        ("contrast".into(), owned(Value::from(0u32))),
    ])
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
}

fn serve(rt: &tokio::runtime::Runtime, address: &str, mock: Mock) -> zbus::Connection {
    rt.block_on(async {
        zbus::connection::Builder::address(address)
            .unwrap()
            .name(NAME)
            .unwrap()
            .serve_at(PATH, mock)
            .unwrap()
            .build()
            .await
            .unwrap()
    })
}

fn emit(rt: &tokio::runtime::Runtime, conn: &zbus::Connection, ns: &str, key: &str, v: Value<'_>) {
    rt.block_on(conn.emit_signal(None::<&str>, PATH, IFACE, "SettingChanged", &(ns, key, v)))
        .unwrap();
}

fn next_system(rx: &Receiver<ChangeEvent>) -> SystemBatch {
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(ChangeEvent::System(b)) => {
            assert!(b.received <= Instant::now());
            b
        }
        other => panic!("expected a system batch, got {other:?}"),
    }
}

/// The settings and boot flag of the next batch.
fn next(rx: &Receiver<ChangeEvent>) -> (Vec<SystemSetting>, bool) {
    let b = next_system(rx);
    (b.settings, b.at_boot)
}

fn initial() -> Vec<SystemSetting> {
    vec![
        DARK,
        SystemSetting::Accent(Some([0.2, 0.4, 1.0])),
        SystemSetting::Contrast(Contrast::Normal),
    ]
}

const DARK: SystemSetting = SystemSetting::Dark {
    dark: true,
    scheme: ColorScheme::PreferDark,
};

#[test]
fn boot_read_then_changes() {
    let Some(d) = daemon() else { return };
    let rt = runtime();
    let conn = serve(&rt, &d.address, mock(appearance()));
    let (sink, rx) = channel();
    let portal = PortalSettings::spawn(Bus::Address(d.address.clone()), sink).unwrap();
    assert_eq!(next(&rx), (initial(), true));

    emit(
        &rt,
        &conn,
        "org.freedesktop.appearance",
        "color-scheme",
        Value::from(2u32),
    );
    let b = next_system(&rx);
    assert_eq!(
        (b.settings.clone(), b.at_boot),
        (
            vec![SystemSetting::Dark {
                dark: false,
                scheme: ColorScheme::PreferLight
            }],
            false
        )
    );
    assert_eq!(b.settings[0].path(), "system.dark");

    // Other namespaces and keys are not ours; the next event we see is the
    // contrast change sent after them.
    emit(
        &rt,
        &conn,
        "org.gnome.desktop.interface",
        "color-scheme",
        Value::from(1u32),
    );
    emit(
        &rt,
        &conn,
        "org.freedesktop.appearance",
        "reduced-motion",
        Value::from(1u32),
    );
    emit(
        &rt,
        &conn,
        "org.freedesktop.appearance",
        "contrast",
        Value::from(1u32),
    );
    let b = next_system(&rx);
    assert_eq!(b.settings, vec![SystemSetting::Contrast(Contrast::High)]);
    assert_eq!(b.settings[0].path(), "system.contrast");

    emit(
        &rt,
        &conn,
        "org.freedesktop.appearance",
        "accent-color",
        Value::from((2.0f64, -1.0f64, 0.5f64)),
    );
    assert_eq!(next_system(&rx).settings, vec![SystemSetting::Accent(None)]);

    // Dropping the client stops its thread promptly.
    drop(portal);
    emit(
        &rt,
        &conn,
        "org.freedesktop.appearance",
        "contrast",
        Value::from(0u32),
    );
    assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
}

/// Portal version 1 has no `ReadOne`; the boot read falls back to `Read`.
#[test]
fn version_one_portal_uses_read() {
    let Some(d) = daemon() else { return };
    let rt = runtime();
    let _conn = serve(
        &rt,
        &d.address,
        Mock {
            v1: true,
            ..mock(appearance())
        },
    );
    let (sink, rx) = channel();
    let _portal = PortalSettings::spawn(Bus::Address(d.address.clone()), sink).unwrap();
    let b = next_system(&rx);
    assert!(b.at_boot);
    assert_eq!(b.settings[0], DARK);
    assert_eq!(b.settings.len(), 3);
}

/// No portal at boot: an empty boot batch. When one starts, its current
/// values are read without any signal, then its changes follow; when it
/// restarts with other values, they are read again.
#[test]
fn a_late_portal_is_read_and_followed() {
    let Some(d) = daemon() else { return };
    let (sink, rx) = channel();
    let _portal = PortalSettings::spawn(Bus::Address(d.address.clone()), sink).unwrap();
    assert_eq!(next(&rx), (vec![], true));
    let rt = runtime();
    let conn = serve(&rt, &d.address, mock(appearance()));
    assert_eq!(next(&rx), (initial(), false));

    emit(
        &rt,
        &conn,
        "org.freedesktop.appearance",
        "contrast",
        Value::from(1u32),
    );
    assert_eq!(
        next(&rx),
        (vec![SystemSetting::Contrast(Contrast::High)], false)
    );

    // The portal restarts (crash, backend switch) with other values.
    rt.block_on(conn.close()).unwrap();
    let mut values = appearance();
    values.insert("color-scheme".into(), owned(Value::from(2u32)));
    let _conn = serve(&rt, &d.address, mock(values));
    let (settings, at_boot) = next(&rx);
    assert!(!at_boot);
    assert_eq!(
        settings[0],
        SystemSetting::Dark {
            dark: false,
            scheme: ColorScheme::PreferLight
        }
    );
}

/// A portal whose read hangs does not hold the boot batch past
/// `BOOT_READ_TIMEOUT`; the slow value follows when it arrives, and
/// dropping the client meanwhile returns at once.
#[test]
fn a_hanging_read_does_not_hold_the_boot_batch() {
    let Some(d) = daemon() else { return };
    let rt = runtime();
    let _conn = serve(
        &rt,
        &d.address,
        Mock {
            slow: Some(("contrast", Duration::from_millis(1500))),
            ..mock(appearance())
        },
    );
    let (sink, rx) = channel();
    let start = Instant::now();
    let portal = PortalSettings::spawn(Bus::Address(d.address.clone()), sink).unwrap();
    assert_eq!(next(&rx), (initial()[..2].to_vec(), true));
    assert!(
        start.elapsed() < Duration::from_millis(1400),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(
        next(&rx),
        (vec![SystemSetting::Contrast(Contrast::Normal)], false)
    );

    // Stopping while a read is outstanding does not wait for it.
    let (sink, _rx2) = channel();
    let slow = PortalSettings::spawn(Bus::Address(d.address.clone()), sink).unwrap();
    std::thread::sleep(Duration::from_millis(700));
    let stop = Instant::now();
    drop(slow);
    drop(portal);
    assert!(
        stop.elapsed() < Duration::from_millis(500),
        "{:?}",
        stop.elapsed()
    );
}

#[test]
fn an_unreachable_bus_sends_an_empty_boot_batch() {
    let (sink, rx) = channel();
    let _portal = PortalSettings::spawn(
        Bus::Address("unix:path=/nonexistent/strand-test-bus".into()),
        sink,
    )
    .unwrap();
    assert_eq!(next(&rx), (vec![], true));
}
