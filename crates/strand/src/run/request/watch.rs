//! `strand watch`: answered by the socket's server itself; the running
//! config's warnings and the main thread's notices follow here (the
//! boot's were made before anyone watched).

use serde_json::json;

use super::{Caller, Shell};

impl Shell {
    pub(super) fn watch_request(&mut self, from: Caller) {
        let id = match from {
            Caller::Socket(id) => id,
            // On D-Bus, watching is the `Changed` signal (S-ipc).
            from @ Caller::Bus(_) => return self.not_served(from, "watch"),
        };
        if !(self.warnings.is_empty()
            && self.host_notices.is_empty()
            && self.blur_notices.is_empty())
            && let Some(s) = &mut self.server
        {
            let notices: Vec<&String> =
                self.host_notices.iter().chain(&self.blur_notices).collect();
            s.send(
                id,
                &json!({
                    "event": "notices",
                    "kept_over_default": [],
                    "notices": notices,
                    "diagnostics": self.warnings,
                }),
            );
        }
    }
}
