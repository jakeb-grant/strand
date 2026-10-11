//! `strand reload [--hard]`: the compiler worker rescans; the answer
//! comes with that load's event (`Shell::after_step`).

use serde_json::json;

use super::{Caller, Shell};
use crate::live::Job;

impl Shell {
    pub(super) fn reload_request(&mut self, from: Caller, hard: bool) {
        let id = match from {
            Caller::Socket(id) => id,
            // A load's answer goes to the socket clients that asked
            // (`Job::Reload`'s client); another thread's reload is not
            // served yet.
            from @ Caller::Bus(_) => return self.not_served(from, "reload"),
        };
        match &self.jobs {
            Some(j)
                if j.send(Job::Reload {
                    hard,
                    client: Some(id),
                })
                .is_ok() => {}
            _ => self.answer(
                Caller::Socket(id),
                json!({"ok": false, "error": "this shell does not reload"}),
            ),
        }
    }
}
