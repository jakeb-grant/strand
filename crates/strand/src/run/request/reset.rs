//! `[reset]` from the command line. Its `{node}` form, "reset subtree"
//! (`Request::ResetNode`), is the inspector's (`inspect.rs`).

use serde_json::json;

use strand_core::flight;

use super::{Caller, Shell};

impl Shell {
    /// The overlay's `[reset]`: the state cell back to its default (a
    /// persisted one forgets its stored value).
    #[cold]
    #[inline(never)]
    pub(super) fn reset_request(&mut self, from: Caller, path: &str) {
        let ans = match self.inst.reset(path) {
            Ok(()) => {
                self.record_request(flight::request::RESET, path, None);
                self.overlay.forget_cell(path, &self.inst);
                self.forget_unheard(path);
                json!({"ok": true})
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        };
        self.answer(from, ans);
    }
}
