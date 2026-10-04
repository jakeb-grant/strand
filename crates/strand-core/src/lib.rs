//! Reactive core: the logic thread.
//!
//! A fine-grained push-pull signal graph (clean/check/dirty colouring with
//! equality cut-off). `state` cells are writable, `let`s and props are lazy
//! derived values, and handlers are cancellable coroutines. When nothing
//! writes, nothing is dirty and no frame callbacks are requested.
//!
//! Writes batch into one prop diff per tick for the render thread, which
//! never waits on this one.
//!
//! See `docs/design.md`, "Reactivity and state model". Lands in M1; the
//! 10k-node graph benchmark is part of M0.
