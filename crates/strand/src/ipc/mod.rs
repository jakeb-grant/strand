//! The IPC socket: `strand reload [--hard]` and `strand watch [--json]`
//! talk to a running `strand run` over a Unix socket in
//! `$XDG_RUNTIME_DIR` (design.md, "CLI"; the M5 `get | set | toggle |
//! call` commands extend the same protocol).
//!
//! **Protocol (version 1).** Newline-delimited JSON. A client sends one
//! request object per line, `{"v": 1, "cmd": "<name>", …}`; the server
//! answers each with one line, `{"ok": true, …}` or `{"ok": false,
//! "error": "…"}`. An unknown `cmd` (or a newer `v`) is answered `{"ok":
//! false, "error": "unknown command …"}` and the connection stays open,
//! so a newer client can probe an older shell.
//!
//! - `{"cmd": "reload", "hard": false}`: rescan the config now; the
//!   answer comes once that reload is committed (or held back) and
//!   carries its event: `{"ok": true, "event": {…}}`.
//!   While a lock is shown the reload waits for the unlock: the answer
//!   comes at once with `"deferred": true` in its event.
//! - `{"cmd": "reset", "path": "launcher.query"}`: the overlay's
//!   `[reset]`: the state cell back to its default (a persisted one
//!   forgets its stored value). `{"ok": true}` or why not.
//! - `{"cmd": "watch"}` (`strand watch --json` prints `{"event":
//!   "watching"}` once subscribed): `{"ok": true}`, then one event object per line
//!   for as long as the connection stays open: `{"event": "reload", …}`
//!   (files, edit classes, kept and reset cells, notices, timing,
//!   diagnostics), `{"event": "fault", …}` (a runtime error that froze a
//!   component), `{"event": "notices", …}` (service and settings
//!   notices; right after subscribing, the running config's warnings in
//!   its `diagnostics`, which a later reload event without them
//!   resolves).
//!
//! The server lives on the logic thread's loop (`docs/architecture.md`,
//! "Threads"): every socket is non-blocking, a client that does not read
//! its events is dropped once 1 MiB is queued for it.
//!
//! The protocol's types and codec are `strand_core::ipc`'s (shared with
//! strand-services' D-Bus front end and `strand-dev inspect`); here are
//! the socket's server ([`server`]), the CLI's client ([`cli`]) and the
//! events as text ([`describe`]).

use std::path::PathBuf;

mod cli;
mod describe;
mod server;

#[cfg(test)]
pub use cli::request;
pub use cli::{reload_cli, set_cli, watch_cli};
pub use describe::describe;
pub use server::{ClientId, HasReady, Ready, Server};
pub use strand_core::ipc::{IpcRequests, MAX_LINE, Request, VERSION, channel, encode, parse};

/// The socket of the shell on this Wayland display: `$STRAND_SOCKET`, or
/// `$XDG_RUNTIME_DIR/strand-<WAYLAND_DISPLAY>.sock` (`strand-0.sock`
/// without a display name). `None` without a runtime directory.
pub fn socket_path() -> Option<PathBuf> {
    socket_path_from(
        std::env::var_os("STRAND_SOCKET"),
        std::env::var_os("XDG_RUNTIME_DIR"),
        std::env::var_os("WAYLAND_DISPLAY"),
    )
}

fn socket_path_from(
    explicit: Option<std::ffi::OsString>,
    runtime: Option<std::ffi::OsString>,
    display: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    if let Some(p) = explicit.filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let dir = PathBuf::from(runtime.filter(|d| !d.is_empty())?);
    if !dir.is_absolute() {
        return None;
    }
    let display = display
        .map(|d| {
            // A display given as a path (`/run/user/1000/wayland-1`) is
            // named by its file name.
            let p = PathBuf::from(d);
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "0".into());
    Some(dir.join(format!("strand-{display}.sock")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn the_socket_is_per_display() {
        let os = |s: &str| Some(OsString::from(s));
        assert_eq!(
            socket_path_from(None, os("/run/user/1"), os("wayland-1")),
            Some(PathBuf::from("/run/user/1/strand-wayland-1.sock"))
        );
        assert_eq!(
            socket_path_from(None, os("/run/user/1"), os("/run/user/1/wayland-2")),
            Some(PathBuf::from("/run/user/1/strand-wayland-2.sock"))
        );
        assert_eq!(
            socket_path_from(None, os("/run/user/1"), None),
            Some(PathBuf::from("/run/user/1/strand-0.sock"))
        );
        assert_eq!(socket_path_from(None, None, os("wayland-1")), None);
        assert_eq!(socket_path_from(None, os("relative"), None), None);
        assert_eq!(
            socket_path_from(os("/tmp/s.sock"), None, None),
            Some(PathBuf::from("/tmp/s.sock"))
        );
    }
}
