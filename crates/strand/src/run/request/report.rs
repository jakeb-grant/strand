//! `strand report` (S-tools): compositor, protocols, scales and frame
//! stats, from main's snapshot (`ToMain::Snapshot`). Not served yet.

use super::{Caller, Shell};

impl Shell {
    pub(super) fn report_request(&mut self, from: Caller) {
        self.not_served(from, "report");
    }
}
