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
        // (M6) A service the config declares is the one called (spec.md 4.2).
        let prog = self.inst.vm().prog.clone();
        let ans =
            match crate::ipc::resolve_call(crate::services::schema(), Some(&prog), target, args) {
                // An action the service could not take is the program's
                // failure event and only that inside a handler, but this
                // caller hears it too (m6-owner-fixes, review of the
                // integration).
                Ok(call) => match strand_services::catch_refusals(|| {
                    self.run_action(&call.service, &call.action, &call.args)
                }) {
                    (Ok(()), refused) if refused.is_empty() => json!({"ok": true}),
                    (Ok(()), refused) => proto::refused(format!("{target}: {}", refused.join("; "))),
                    (Err(e), _) => proto::refused(format!("{target}: {e}")),
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
        // A config's `service` of this name is the one called; the
        // builtin it shadows is not started for it.
        let svc = if self.inst.vm().prog.declares_service(service) {
            None
        } else {
            real.served(service)
        };
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
