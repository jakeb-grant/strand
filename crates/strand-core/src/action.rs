//! What an action knows about the handler that runs it (M5): the input
//! event that started the handler and the handler invocation's id.
//!
//! Defined here, not in strand-compiler's VM, because both ends of the
//! route name it: the VM passes it to `ServiceHost::action` (strand-
//! compiler re-exports it as `strand_compiler::vm::host::ActionCtx`),
//! and strand-services' `DynService::action` and its ordering queue
//! take it (S-platform), and strand-services depends only on strand-core
//! (docs/architecture.md, "Handler input context").

/// The input event that started a handler (M5, S-platform): the
/// surface it arrived on and the compositor's serial for it, what an
/// `xdg_activation_v1` token is asked with. Available to the handler's
/// actions until its first `await`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputCtx {
    /// The surface's number: `strand_scene::SurfaceId`'s `.0` (a plain
    /// number so strand-core and strand-services need no strand-scene).
    pub surface: u32,
    pub serial: u32,
}

/// What an action knows about the handler that runs it (M5): its input
/// event, if any, and the handler invocation's id, so the services side
/// keeps one invocation's actions in order behind a token wait
/// (docs/architecture.md, "Handler input context"). The default, empty
/// context (no input, invocation 0) is what every action gets until the
/// VM records them, and stays an action outside any handler's: 0 is
/// unordered. A handler invocation is numbered from 1 with or without
/// input and keeps its number across every `await`; only `input` is
/// cleared at the first one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ActionCtx {
    pub input: Option<InputCtx>,
    pub invocation: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default context is the empty one every action gets until the
    /// VM records input: no input event, invocation 0.
    #[test]
    fn the_default_action_context_is_empty() {
        let ctx = ActionCtx::default();
        assert_eq!(ctx.input, None);
        assert_eq!(ctx.invocation, 0);
        let with = ActionCtx {
            input: Some(InputCtx {
                surface: 3,
                serial: 42,
            }),
            invocation: 7,
        };
        assert_ne!(with, ctx);
    }
}
