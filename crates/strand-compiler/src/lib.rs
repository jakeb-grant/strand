//! The one compiler crate behind the runtime, `strand check` and the LSP.
//!
//! Parses and type-checks `.strand` files, lowers bindings to bytecode, and
//! reconciles a new tree against the live one by identity (source span, then
//! `key`/`id`, then position). A broken save keeps the last good tree.
//!
//! See `docs/design.md`, "The proposed API" and "Live reload and real-time
//! config changes". Lands in M1.
