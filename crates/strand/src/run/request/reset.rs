//! `[reset]` from the command line, and (S-inspect) `reset{node}`.

use serde_json::json;

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

    /// (S-inspect) Reset subtree (`Instance::reset_subtree`). Not served
    /// yet.
    pub(super) fn reset_node_request(&mut self, from: Caller, node: &str) {
        let _ = node;
        self.answer(
            from,
            strand_core::ipc::refused("`reset` of a node is not served by this shell yet"),
        );
    }
}
