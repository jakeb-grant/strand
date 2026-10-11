//! A mock service change (`STRAND_MOCK` only; the acceptance tests'
//! driver).

use serde_json::{Value as Json, json};

use super::{Caller, Shell};

impl Shell {
    #[cold]
    #[inline(never)]
    pub(super) fn mock_request(&mut self, from: Caller, req: &Json) {
        let ans = if crate::mock::requested().is_none() {
            json!({"ok": false, "error": "`mock` needs a shell run with STRAND_MOCK"})
        } else {
            match crate::mock::command(self.inst.runtime(), &self.host, req) {
                Ok(()) => json!({"ok": true}),
                Err(e) => json!({"ok": false, "error": e}),
            }
        };
        self.answer(from, ans);
    }
}
