//! `strand watch`: answered by the socket's server itself; the running
//! config's warnings, the main thread's notices and the `file` outputs'
//! standing notices follow here (the boot's were made before anyone
//! watched).

use serde_json::json;

use super::{Caller, Shell};

impl Shell {
    pub(super) fn watch_request(&mut self, from: Caller) {
        let id = match from {
            Caller::Socket(id) => id,
            // On D-Bus, watching is the `Changed` signal (S-ipc).
            from @ Caller::Bus(_) => return self.not_served(from, "watch"),
        };
        let notices: Vec<&String> = self
            .host_notices
            .iter()
            .chain(&self.blur_notices)
            .chain(self.files.notices())
            .collect();
        if !(self.warnings.is_empty() && notices.is_empty())
            && let Some(s) = &mut self.server
        {
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
