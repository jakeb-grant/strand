//! (live examples, docs/live-examples.md) Strand's render path in a
//! browser: a config compiled with the mock services, run on a virtual
//! clock, its surfaces painted by the CPU renderer and composited into
//! RGBA for a canvas. [`shell::Shell`] is the host, [`steps`] plays a
//! scenario's steps on it, [`live`] is a docs example as its figure
//! carries it (all tested natively); `web` is the wasm-bindgen boundary,
//! built only for wasm.

pub mod live;
pub mod shell;
pub mod steps;
pub mod surfaces;
#[cfg(target_family = "wasm")]
mod web;

pub use shell::{LoadError, Options, Shell};
pub use surfaces::Screen;
