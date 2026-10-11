//! `strand get <path>` (S-ipc): what `set` writes, read back: an
//! exported value, a settings field (with its layer) or a token with
//! the token spelling (`$accent`, its origin as the layer). The answer's
//! shape is `ipc/query.rs`'s.

use serde_json::json;
use strand_core::ipc as proto;

use super::{Caller, Shell};
use crate::ipc::{layer_name, token_json, token_path, token_text, value_json};

impl Shell {
    #[cold]
    #[inline(never)]
    pub(super) fn get_request(&mut self, from: Caller, path: &str) {
        let ans = match token_path(path) {
            Some(token) => match self.inst.token(token) {
                Some((v, origin)) => json!({
                    "ok": true,
                    "value": token_json(&v),
                    "text": token_text(&v),
                    "layer": origin,
                }),
                None => proto::refused(format!("no token `{path}`")),
            },
            None => match self.inst.get(path) {
                Ok(v) => {
                    let types = &self.inst.vm().prog.types;
                    json!({
                        "ok": true,
                        "value": value_json(&v, types),
                        "text": v.show(types),
                        "layer": self.inst.layer(path).map(layer_name),
                    })
                }
                Err(e) => proto::refused(e.to_string()),
            },
        };
        self.answer(from, ans);
    }
}
