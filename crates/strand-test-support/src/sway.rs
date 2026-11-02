//! A headless sway of a test's own.

use std::ffi::{c_int, c_ulong};
use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const PR_SET_PDEATHSIG: c_int = 1;
const SIGKILL: c_ulong = 9;

unsafe extern "C" {
    fn prctl(option: c_int, ...) -> c_int;
}

/// `sway -c cfg`, headless with the pixman renderer and no input devices,
/// its runtime directory `dir` (which needs a short path, see
/// [`crate::dir::ShortDir`]), none of the session's displays, its output in
/// `log`. It dies with the thread that spawns it, so a test binary killed
/// before its `Drop` (a timeout, SIGKILL, an abort) never leaks it. A caller
/// adds to the command what it wants besides, and spawns it.
pub fn command(dir: impl AsRef<Path>, cfg: impl AsRef<Path>, log: File) -> Command {
    let mut cmd = Command::new("sway");
    // SAFETY: the hook runs between fork and exec and makes one
    // async-signal-safe syscall, allocating nothing unless it fails.
    unsafe {
        cmd.pre_exec(|| {
            if prctl(PR_SET_PDEATHSIG, SIGKILL) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    cmd.arg("-c")
        .arg(cfg.as_ref())
        .env("XDG_RUNTIME_DIR", dir.as_ref())
        .env("WLR_BACKENDS", "headless")
        .env("WLR_RENDERER", "pixman")
        .env("WLR_LIBINPUT_NO_DEVICES", "1")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("SWAYSOCK")
        .env_remove("DISPLAY")
        .stdin(Stdio::null());
    match log.try_clone() {
        Ok(out) => cmd.stdout(out),
        Err(_) => cmd.stdout(Stdio::null()),
    };
    cmd.stderr(log);
    cmd
}

/// The name of the Wayland socket in `dir`, when there is one.
pub fn display_in(dir: &Path) -> Option<String> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|e| e.starts_with("wayland-") && !e.ends_with(".lock"))
}

/// Waits (`limit` at most) for the Wayland socket in `dir` and returns its
/// name; `None` when it never came.
pub fn wait_display(dir: &Path, limit: Duration) -> Option<String> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(display) = display_in(dir) {
            return Some(display);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The Wayland socket's name and the IPC socket's in `dir`, once both are
/// there.
pub fn sockets_in(dir: impl AsRef<Path>) -> Option<(String, String)> {
    let names: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let display = names
        .iter()
        .find(|e| e.starts_with("wayland-") && !e.ends_with(".lock"))?;
    let ipc = names.iter().find(|e| e.starts_with("sway-ipc."))?;
    Some((display.clone(), ipc.clone()))
}
