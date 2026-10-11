//! The inspector's commands (S-inspect): `inspect.pick`, `inspect.node`,
//! `inspect.dump`, `inspect.highlight`, `inspect.flash`. Not served yet.

use strand_core::ipc::Request;

use super::{Caller, Shell};

impl Shell {
    pub(super) fn inspect_request(&mut self, from: Caller, req: Request) {
        self.not_served(from, req.cmd());
    }
}
