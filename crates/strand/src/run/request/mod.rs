//! IPC requests on the logic thread: [`Shell::request`] dispatches each
//! command to its own module, so the streams that serve M5's commands
//! own disjoint files (docs/m5-plan.md, "Streams": `get`, `set`,
//! `toggle`, `call`, `clear` S-ipc, and in wave 2 `inspect.node`,
//! `inspect.dump` and `reset{node}` over S-inspect's library; the other
//! `inspect.*` S-inspect; `report` and `export` S-tools). A command no module serves
//! yet does not parse from the socket (`strand_core::ipc::parse`); from
//! another thread it arrives as a `Request` and is answered as the
//! socket answers it, as an unknown command ([`strand_core::ipc::unknown`]).
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
mod export;
mod get;
mod inspect;
mod mock;
mod reload;
mod report;
mod reset;
mod set;
mod toggle;
mod watch;

pub(in crate::run) use export::Waiting;
pub(in crate::run) use inspect::Inspector;

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
            Request::ResetNode { node } => self.reset_node_request(from, &node),
            Request::InspectNode { id } => self.inspect_node_request(from, &id),
            Request::InspectDump { surface } => self.inspect_dump_request(from, surface.as_deref()),
            // (M5 wave 3, S-inspect) Socket requests only.
            Request::InspectPick { on } => self.inspect_pick_request(from, on),
            Request::InspectHighlight { id } => self.inspect_highlight_request(from, id.as_deref()),
            Request::InspectFlash { on } => self.inspect_flash_request(from, on),
            Request::Set { path, value } => self.set_request(from, &path, &value),
            Request::Get { path } => self.get_request(from, &path),
            Request::Toggle { path } => self.toggle_request(from, &path),
            Request::Call { target, args } => self.call_request(from, &target, &args),
            Request::Clear { path } => self.clear_request(from, &path),
            Request::Mock(req) => self.mock_request(from, &req),
            // A socket request only (decisions.md m5-tools, "the CLI").
            Request::Export {
                kind,
                template,
                name,
            } if matches!(from, Caller::Socket(_)) => {
                self.export_request(from, &kind, template, name)
            }
            Request::Report { save: None } if matches!(from, Caller::Socket(_)) => {
                self.report_request(from)
            }
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
    /// (the request channel's unserved M5 commands, and `watch` from it:
    /// on D-Bus, watching is the `Changed` signal).
    fn not_served(&mut self, from: Caller, cmd: &str) {
        self.answer(from, proto::unknown(cmd));
    }
}
