//! `strand call <service.action> [args]` (S-ipc): a service-level action
//! (decisions.md m5-owner Q13), resolved and its arguments read by
//! `ipc/call.rs`, run outside any handler (`ActionCtx::default()`:
//! invocation 0, unordered). A service nobody reads is started for it
//! and its first read waited for (up to `services::SET_WAIT`), as `strand
//! set` does for a service field; it stops 5 s later.

use serde_json::json;
use strand_compiler::vm::{ActionCtx, ActionTarget, ServiceHost};
use strand_core::ipc as proto;

use super::{Caller, Shell};

impl Shell {
    #[cold]
    #[inline(never)]
    pub(super) fn call_request(&mut self, from: Caller, target: &str, args: &[String]) {
        let ans = match crate::ipc::resolve_call(crate::services::schema(), target, args) {
            Ok(call) => match self.run_action(&call.service, &call.action, &call.args) {
                Ok(()) => json!({"ok": true}),
                Err(e) => proto::refused(format!("{target}: {e}")),
            },
            Err(e) => proto::refused(e),
        };
        self.answer(from, ans);
    }

    fn run_action(
        &self,
        service: &str,
        action: &str,
        args: &[strand_compiler::vm::Value],
    ) -> Result<(), strand_core::Error> {
        let rt = self.inst.runtime();
        let target = ActionTarget::Service(service);
        let ctx = ActionCtx::default();
        let Some(real) = &self.real else {
            return self.host.action(rt, target, action, args, ctx);
        };
        let svc = real.served(service);
        if let Some(svc) = &svc {
            svc.acquire(rt);
            real.services
                .wait_ready_of(rt, Some(service), crate::services::SET_WAIT);
        }
        let r = real.host.action(rt, target, action, args, ctx);
        if let Some(svc) = &svc {
            svc.release(rt);
        }
        r
    }
}
