//! `[reset]` from the command line. (S-inspect adds `reset{node}`,
//! `Request::ResetNode`, here.)

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
}
