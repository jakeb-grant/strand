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
                self.overlay.forget_cell(path, &self.inst);
                self.reset_done(path);
                json!({"ok": true})
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        };
        self.answer(from, ans);
    }

    /// What follows a cell's reset, from IPC or the overlay's own
    /// `[reset]` click alike: the reset is recorded (a glitch report's
    /// replay repeats it), and the next reload event no longer lists the
    /// cell as kept over its default.
    pub(in crate::run) fn reset_done(&mut self, path: &str) {
        self.record_request(flight::request::RESET, path, None);
        self.forget_unheard(path);
    }
}
