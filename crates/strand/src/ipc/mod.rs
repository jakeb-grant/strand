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
//! strand-services' D-Bus front end and `strand-dev inspect`), and so is
//! the socket's path (`strand_core::ipc::socket_path`); here are
//! the socket's server ([`server`]), the CLI's client ([`cli`]) and the
//! events as text ([`describe`]).

mod cli;
mod describe;
mod server;

#[cfg(test)]
pub use cli::request;
pub use cli::{reload_cli, set_cli, watch_cli};
pub use describe::describe;
pub use server::{ClientId, HasReady, Ready, Server};
pub use strand_core::ipc::{
    EventSink, IpcRequests, MAX_LINE, Request, VERSION, channel, encode, parse, socket_path,
};
