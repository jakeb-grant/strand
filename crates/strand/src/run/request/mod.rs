//! IPC requests on the logic thread: [`Shell::request`] dispatches each
//! command to its own module, so the streams that serve M5's commands
//! own disjoint files (docs/m5-plan.md, "Streams": `get`, `set`,
//! `toggle`, `call`, `clear` S-ipc; `inspect.*` S-inspect; `report`
//! S-tools). A command no module serves yet does not parse from the
//! socket (`strand_core::ipc::parse`); from another thread it arrives
//! as a `Request` and is answered as an unknown one
//! ([`strand_core::ipc::unknown`]), so both hear M4's answer.
//!
//! A request comes from a socket client or, through
//! [`strand_core::ipc::IpcRequests`], from another thread (the D-Bus
//! front end) with a [`Reply`]; both are answered through
//! [`Shell::answer`].

use serde_json::Value as Json;
use strand_core::ipc::{self as proto, Reply, Request};

use super::shell::Shell;
use crate::ipc;

mod call;
mod clear;
mod get;
mod inspect;
mod mock;
mod reload;
mod report;
mod reset;
mod set;
mod toggle;
mod watch;

/// Who asked, and where the answer goes.
#[derive(Debug)]
pub(in crate::run) enum Caller {
    /// A client of the socket.
    Socket(ipc::ClientId),
    /// A request from another thread ([`ipc::IpcRequests`]).
    Bus(Reply),
}

impl Shell {
    /// A request from an IPC client.
    pub(in crate::run) fn request(&mut self, from: Caller, req: Request) {
        match req {
            Request::Reload { hard } => self.reload_request(from, hard),
            Request::Watch => self.watch_request(from),
            Request::Reset { path } => self.reset_request(from, &path),
            Request::Set { path, value } => self.set_request(from, &path, &value),
            Request::Mock(req) => self.mock_request(from, &req),
            req => self.not_served(from, req.cmd()),
        }
    }

    /// Answer `from`'s request.
    pub(in crate::run) fn answer(&mut self, from: Caller, answer: Json) {
        match from {
            Caller::Socket(id) => {
                if let Some(s) = &mut self.server {
                    s.answer(id, &answer);
                }
            }
            Caller::Bus(reply) => reply.send(answer),
        }
    }

    /// Answer a command this shell does not serve yet as an unknown one
    /// (the request channel's M5 commands; `reload` and `watch` from it).
    fn not_served(&mut self, from: Caller, cmd: &str) {
        self.answer(from, proto::unknown(cmd));
    }
}
