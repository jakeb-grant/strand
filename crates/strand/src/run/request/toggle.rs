//! `strand toggle <path>` (S-ipc): flip an exported boolean. Not served
//! yet.

use super::{Caller, Shell};

impl Shell {
    pub(super) fn toggle_request(&mut self, from: Caller, path: &str) {
        let _ = path;
        self.not_served(from, "toggle");
    }
}
