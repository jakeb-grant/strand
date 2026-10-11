//! `strand clear <path>` (S-ipc): drop a runtime overlay, a token's
//! (`$accent`) or a settings field's; the CLI twin of the overlay
//! notice's `[clear]`.

use serde_json::json;
use strand_core::ipc as proto;

use super::{Caller, Shell};
use crate::ipc::token_path;

impl Shell {
    pub(super) fn clear_request(&mut self, from: Caller, path: &str) {
        let cleared = match token_path(path) {
            Some(token) => Ok(self.inst.clear_token_overlay(token)),
            None => self.inst.clear_overlay(path).map_err(|e| e.to_string()),
        };
        let ans = match cleared {
            Ok(cleared) => json!({"ok": true, "cleared": cleared}),
            Err(e) => proto::refused(e),
        };
        self.answer(from, ans);
    }
}
