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
//! - (M5, S-ipc) `{"cmd": "get", "path": …}`, `toggle{path}`,
//!   `call{target, args}` and `clear{path}`: `strand get`, `toggle`,
//!   `call` and `clear` ([`query`], [`call`], [`overlay`] say what each
//!   answers); a path with the token spelling (`$accent`) names the
//!   token overlay layer for `set`, `get` and `clear`.
//!
//! The protocol's types and codec are `strand_core::ipc`'s (shared with
//! strand-services' D-Bus front end and `strand-dev inspect`), and so is
//! the socket's path (`strand_core::ipc::socket_path`); here are
//! the socket's server ([`server`]), the CLI's client ([`cli`], and
//! [`query`], [`call`] and [`overlay`] for M5's commands) and the events
//! as text ([`describe`]).

mod call;
mod cli;
mod describe;
pub(crate) mod inspect;
mod overlay;
mod query;
mod server;

pub use call::call_cli;
pub(crate) use call::resolve_in as resolve_call;
// `strand export` asks the shell with it too (S-tools).
pub use cli::request;
pub use cli::{reload_cli, set_cli, watch_cli};
pub use describe::describe;
pub use overlay::clear_cli;
pub(crate) use overlay::token_path;
pub use query::{get_cli, toggle_cli};
// `strand report` asks with it too (S-tools).
pub(crate) use query::ask;
pub(crate) use query::{layer_name, token_json, token_text, value_json};
pub use server::{ClientId, HasReady, Ready, Server};
pub use strand_core::ipc::{
    EventSink, IpcRequests, MAX_LINE, Request, VERSION, channel, encode, parse, socket_path,
};
