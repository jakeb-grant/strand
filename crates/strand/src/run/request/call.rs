//! `strand call <service.action> [args]` (S-ipc): a service-level action
//! (decisions.md m5-owner Q13). Not served yet.

use super::{Caller, Shell};

impl Shell {
    pub(super) fn call_request(&mut self, from: Caller, target: &str, args: &[String]) {
        let _ = (target, args);
        self.not_served(from, "call");
    }
}
