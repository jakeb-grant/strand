//! `strand watch`: answered by the socket's server itself; the running
//! config's warnings, the main thread's notices, the `file` outputs'
//! standing notices and (build-ready) the open service diagnostics
//! follow here (the boot's were made before anyone watched), and a
//! handler's repeated fault is reported again at its next run.

use serde_json::json;

use super::{Caller, Shell};

impl Shell {
    pub(super) fn watch_request(&mut self, from: Caller) {
        let id = match from {
            Caller::Socket(id) => id,
            // On D-Bus, watching is the `Changed` signal (S-ipc).
            from @ Caller::Bus(_) => return self.not_served(from, "watch"),
        };
        // (build-ready review) A handler failing the same way since before
        // this watch (an `every` failing each tick) is reported once more
        // at its next failing run: the dedupe would keep it from a
        // watcher who never heard it (decisions.md
        // handler-fault-repeat-watch).
        self.inst.report_faults_again();
        let notices: Vec<&String> = self
            .host_notices
            .iter()
            .chain(&self.blur_notices)
            .chain(self.files.notices())
            .chain(self.service_open.iter().map(|(_, t)| t))
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
