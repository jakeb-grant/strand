//! A private D-Bus session daemon of a test's own.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};

/// `dbus-daemon --session` listening in a directory, killed on drop.
#[derive(Debug)]
pub struct SessionDaemon {
    child: Child,
    /// What it printed as its address, its guid included.
    pub address: String,
}

impl SessionDaemon {
    /// Starts one on `<dir>/bus` (a short path: see
    /// [`crate::dir::ShortDir`]) and waits for its address. `None`, after
    /// saying why, when `dbus-daemon` can't be started, unless
    /// `STRAND_REQUIRE_DBUS` is set: there the test must run, so it panics.
    pub fn start(dir: &Path) -> Option<SessionDaemon> {
        let spawned = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .arg(format!("--address=unix:path={}/bus", dir.display()))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) if std::env::var_os("STRAND_REQUIRE_DBUS").is_none() => {
                eprintln!("skipping: dbus-daemon unavailable ({e})");
                return None;
            }
            Err(e) => panic!("dbus-daemon: {e}"),
        };
        let mut line = String::new();
        if let Some(out) = child.stdout.take()
            && let Err(e) = BufReader::new(out).read_line(&mut line)
        {
            panic!("dbus-daemon's address: {e}");
        }
        Some(SessionDaemon {
            child,
            address: line.trim().to_string(),
        })
    }
}

impl Drop for SessionDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
