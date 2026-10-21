//! `strand toggle <path>` (S-ipc): flip an exported boolean in one
//! logic-thread step (`Instance::toggle`); any other type is refused,
//! naming it.

use serde_json::json;
use strand_core::{flight, ipc as proto};

use super::{Caller, Shell};

impl Shell {
    #[cold]
    #[inline(never)]
    pub(super) fn toggle_request(&mut self, from: Caller, path: &str) {
        let ans = match self.inst.toggle(path) {
            Ok(on) => {
                self.record_request(flight::request::TOGGLE, path, None);
                self.forget_unheard(path);
                json!({"ok": true, "value": on})
            }
            Err(e) => proto::refused(e.to_string()),
        };
        self.answer(from, ans);
    }
}
