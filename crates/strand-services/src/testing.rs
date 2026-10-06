//! Test support for service tests (design.md, "Testing": the services
//! tier runs on a private D-Bus with python-dbusmock and small zbus
//! mocks).
//!
//! - [`PrivateBus`]: a `dbus-daemon` of the test's own; hand its
//!   [`PrivateBus::buses`] to [`crate::Services::new`], or its
//!   [`PrivateBus::env`] to a child process. Tests never touch the
//!   machine's session or system bus.
//! - [`DbusMock`]: a python-dbusmock template (`upower`, `logind`,
//!   `networkmanager`, `bluez5`, `notification_daemon`, …) on a private
//!   bus.
//!
//! Both skip (return `None`, saying so on stderr) when the tool is
//! missing, unless `STRAND_REQUIRE_DBUS` is set (CI): there a missing
//! tool fails the test instead of passing it silently.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::bus::Buses;

/// `STRAND_REQUIRE_DBUS` is set: the D-Bus tiers must run.
pub fn require_dbus() -> bool {
    std::env::var_os("STRAND_REQUIRE_DBUS").is_some()
}

fn skip(what: &str) {
    assert!(
        !require_dbus(),
        "{what}, but STRAND_REQUIRE_DBUS is set: the D-Bus tier must run"
    );
    eprintln!("\n*** SKIPPED: {what} ***\n");
}

/// A private `dbus-daemon` (session configuration), killed on drop.
#[derive(Debug)]
pub struct PrivateBus {
    child: Child,
    /// Its address (`unix:path=…`).
    pub address: String,
    dir: PathBuf,
}

impl PrivateBus {
    /// Start one; `None` (skipped) without `dbus-daemon`.
    pub fn start() -> Option<PrivateBus> {
        let dir =
            std::env::temp_dir().join(format!("strand-bus-{}-{}", std::process::id(), unique()));
        let _ = std::fs::remove_dir_all(&dir);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            skip(&format!("no directory for a private bus ({e})"));
            return None;
        }
        let spawned = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .arg(format!("--address=unix:path={}/bus", dir.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                skip(&format!("dbus-daemon is not available ({e})"));
                return None;
            }
        };
        let mut line = String::new();
        let read = child
            .stdout
            .take()
            .map(|out| BufReader::new(out).read_line(&mut line));
        if !matches!(read, Some(Ok(n)) if n > 0) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(&dir);
            skip("dbus-daemon printed no address");
            return None;
        }
        Some(PrivateBus {
            child,
            address: line.trim().to_string(),
            dir,
        })
    }

    /// Session and system bus both this one.
    pub fn buses(&self) -> Buses {
        Buses::private(&self.address)
    }

    /// The environment that points a child process's session and system
    /// bus here.
    pub fn env(&self) -> [(&'static str, String); 2] {
        [
            ("DBUS_SESSION_BUS_ADDRESS", self.address.clone()),
            ("DBUS_SYSTEM_BUS_ADDRESS", self.address.clone()),
        ]
    }

    /// Wait until `name` has an owner on this bus, at most `limit`.
    pub fn wait_for_name(&self, name: &str, limit: Duration) -> bool {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return false;
        };
        rt.block_on(async {
            let Ok(builder) = zbus::connection::Builder::address(self.address.as_str()) else {
                return false;
            };
            let Ok(conn) = builder.build().await else {
                return false;
            };
            let Ok(dbus) = zbus::fdo::DBusProxy::new(&conn).await else {
                return false;
            };
            let Ok(name) = zbus::names::BusName::try_from(name) else {
                return false;
            };
            let deadline = Instant::now() + limit;
            while Instant::now() < deadline {
                if dbus.name_has_owner(name.clone()).await.unwrap_or(false) {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            false
        })
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

/// The Python that runs python-dbusmock: `$STRAND_DBUSMOCK_PYTHON`, else
/// the first of `python3` and `python3.12` that imports `dbusmock`.
pub fn dbusmock_python() -> Option<String> {
    let imports = |py: &str| {
        Command::new(py)
            .args(["-c", "import dbusmock"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    if let Ok(py) = std::env::var("STRAND_DBUSMOCK_PYTHON") {
        return imports(&py).then_some(py);
    }
    ["python3", "python3.12"]
        .into_iter()
        .find(|py| imports(py))
        .map(str::to_string)
}

/// A python-dbusmock template running on a [`PrivateBus`], killed on
/// drop.
#[derive(Debug)]
pub struct DbusMock {
    child: Child,
}

impl DbusMock {
    /// Run `template` (with `parameters`, a JSON object, if any) on
    /// `bus`, as a system service when `system`, and wait (up to 10 s)
    /// for `name` to appear. `None` (skipped) without a Python that has
    /// dbusmock.
    pub fn start(
        bus: &PrivateBus,
        template: &str,
        system: bool,
        parameters: Option<&str>,
        name: &str,
    ) -> Option<DbusMock> {
        let Some(py) = dbusmock_python() else {
            skip("no python with dbusmock (python3-dbusmock)");
            return None;
        };
        let mut cmd = Command::new(py);
        cmd.args(["-m", "dbusmock", "--template", template]);
        if system {
            cmd.arg("--system");
        }
        if let Some(p) = parameters {
            cmd.args(["--parameters", p]);
        }
        let child = cmd
            .envs(bus.env())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let child = match child {
            Ok(c) => c,
            Err(e) => {
                skip(&format!("python-dbusmock did not start ({e})"));
                return None;
            }
        };
        let mock = DbusMock { child };
        assert!(
            bus.wait_for_name(name, Duration::from_secs(10)),
            "python-dbusmock's {template} never owned {name}"
        );
        Some(mock)
    }
}

impl Drop for DbusMock {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
