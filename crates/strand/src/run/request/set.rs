//! `strand set <path> <value>`: an exported state or settings field, a
//! service's `rw` field, or (S-ipc, m5-owner Q10) with the token
//! spelling, `strand set '$accent' '#ff8800'`, the token overlay layer.

use serde_json::json;

use super::{Caller, Shell};

impl Shell {
    pub(super) fn set_request(&mut self, from: Caller, path: &str, value: &str) {
        // A service's `rw` field (`brightness.level +5%`) goes through the
        // service host; a token (`$accent`) to its overlay; everything
        // else is state or settings.
        let rt = self.inst.runtime();
        let token = crate::ipc::token_path(path);
        let exported = token.is_none() && self.inst.get(path).is_ok();
        let r = if let Some(token) = token {
            self.inst
                .set_token_overlay(token, value)
                .map_err(|e| e.to_string())
        } else if !exported && crate::services::is_service_path(path) {
            match &self.real {
                Some(real) => real.set_text(rt, path, value),
                None => crate::services::set_text(
                    &*self.host,
                    rt,
                    &crate::services::schema().types,
                    path,
                    value,
                ),
            }
        } else {
            self.inst.set_text(path, value).map_err(|e| e.to_string())
        };
        let ans = match r {
            Ok(()) => json!({"ok": true}),
            Err(e) => json!({"ok": false, "error": e}),
        };
        self.answer(from, ans);
    }
}
