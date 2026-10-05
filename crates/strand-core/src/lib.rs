//! Reactive core: the logic thread.
//!
//! A fine-grained push-pull signal graph (clean/check/dirty colouring with
//! equality cut-off). `state` cells are [`Signal`]s, `let`s and props are
//! lazy [`Memo`]s, and the edge of the graph is [`Effect`]s and
//! [`Runtime::watch`]es. Handlers are cancellable coroutines ([`Task`]) and
//! timers ([`Timer`]) count only while their condition holds. When nothing
//! writes, nothing is dirty, nothing is scheduled
//! ([`Runtime::next_deadline`] is `None`) and no work happens: true idle.
//!
//! Writes batch: [`Runtime::flush`] ends a tick, runs dirty effects in
//! creation order and returns a [`Tick`] listing what changed, from which
//! the scene emitter builds one diff per tick for the render thread, which
//! never waits on this one.
//!
//! Errors are values: a stale handle, a runtime cycle or a failed handler
//! is an [`Error`], never a panic.
//!
//! ```
//! use strand_core::Runtime;
//!
//! let rt = Runtime::new();
//! let volume = rt.signal(40);
//! let muted = rt.signal(false);
//! let level = rt.memo(move |rt| Ok(if muted.get(rt)? { 0 } else { volume.get(rt)? }));
//! let _watch = rt.watch(level.id()).unwrap();
//!
//! volume.set(&rt, 55).unwrap();
//! volume.set(&rt, 60).unwrap(); // coalesces within the tick
//! let tick = rt.flush();
//! assert_eq!(tick.changed, vec![level.id()]);
//! assert_eq!(level.get(&rt), Ok(60));
//! ```
//!
//! See `docs/design.md`, "Reactivity and state model".

mod async_value;
mod echo;
mod error;
mod events;
pub mod keyed;
pub mod rate;
mod runtime;
mod signal;
mod task;
mod timer;

pub use async_value::{Async, AsyncMemo, RequestId};
pub use echo::{Generation, MAX_PENDING_ECHOES, Received};
pub use error::{CyclePath, Error};
pub use events::EventQueue;
pub use keyed::reactive::{KeyedMemo, KeyedOps, KeyedSignal, KeyedSource, Snapshot};
pub use keyed::{KeyedError, KeyedVec, VecDiff, keyed_diff};
pub use runtime::{
    Diagnostic, HARD_RUNS_PER_FLUSH, MAX_FROZEN_EVENTS, MAX_RUNS_PER_FLUSH, NodeId, NodeKind,
    Runtime, Scope, Stats, Tick, WeakRuntime,
};
pub use signal::{Effect, Memo, Signal};
pub use task::{Sleep, Task};
pub use timer::{Debounced, MIN_EVERY_PERIOD, Timer};
