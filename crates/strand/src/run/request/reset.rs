//! `[reset]` from the command line. (S-inspect adds `reset{node}`,
//! `Request::ResetNode`, here.)

use serde_json::json;
use strand_core::ipc as proto;

use super::{Caller, Shell};

impl Shell {
    /// The overlay's `[reset]`: the state cell back to its default (a
    /// persisted one forgets its stored value).
    pub(super) fn reset_request(&mut self, from: Caller, path: &str) {
        let ans = match self.inst.reset(path) {
            Ok(()) => {
                self.overlay.forget_cell(path, &self.inst);
                json!({"ok": true})
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        };
        self.answer(from, ans);
    }

    /// `reset{node}` until S-inspect serves it: what the socket's
    /// `parse` answers a `reset` without a `path`, M4's answer, so a
    /// request from another thread hears the same refusal.
    pub(super) fn reset_node_request(&mut self, from: Caller) {
        self.answer(from, proto::refused("`reset` needs a `path`"));
    }
}
