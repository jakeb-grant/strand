//! `strand set <path> <value>` (S-ipc extends it to token overlays).

use serde_json::json;

use super::{Caller, Shell};

impl Shell {
    pub(super) fn set_request(&mut self, from: Caller, path: &str, value: &str) {
        // A service's `rw` field (`brightness.level +5%`) goes through the
        // service host; everything else is state or settings.
        let rt = self.inst.runtime();
        let exported = self.inst.get(path).is_ok();
        let r = if !exported && crate::services::is_service_path(path) {
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
