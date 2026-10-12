//! `strand set <path> <value>`: an exported state or settings field, a
//! service's `rw` field, or (S-ipc, m5-owner Q10) with the token
//! spelling, `strand set '$accent' '#ff8800'`, the token overlay layer.

use serde_json::json;

use super::{Caller, Shell};

impl Shell {
    #[cold]
    #[inline(never)]
    pub(super) fn set_request(&mut self, from: Caller, path: &str, value: &str) {
        // A service's `rw` field (`brightness.level +5%`, the config's
        // own `ppd.profile`) goes through the service host; a token (`$accent`) to its overlay; everything
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
        } else if let Some(record) = (!exported)
            .then(|| crate::services::config_service(&self.inst.vm().prog, path))
            .flatten()
        {
            // The config's own service (`ppd.profile`): its record is in
            // the program's types, and the VM's host writes it by name, as
            // a handler's `ppd.profile = …` does.
            let vm = self.inst.vm();
            let types = &vm.prog.types;
            crate::services::set_field_text(types, record, &*vm.host, rt, types, path, value)
        } else {
            self.inst.set_text(path, value).map_err(|e| e.to_string())
        };
        let ans = match r {
            Ok(()) => {
                if token.is_none() {
                    self.forget_unheard(path);
                }
                json!({"ok": true})
            }
            Err(e) => json!({"ok": false, "error": e}),
        };
        self.answer(from, ans);
    }
}
