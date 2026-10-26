//! (wasm spike, docs/wasm-spike.md) Strand's render path in a browser: a
//! config compiled with the mock services, run on a virtual clock, its
//! surfaces painted by the CPU renderer and composited into one RGBA
//! image of the screen. [`shell::Shell`] is the logic (tested natively);
//! `web` is the wasm-bindgen boundary, built only for wasm.

#[path = "../../strand-surface/src/placement.rs"]
#[allow(dead_code)]
mod placement;
pub mod shell;
pub mod surfaces;
#[cfg(target_family = "wasm")]
mod web;

pub use shell::{LoadError, Options, Shell};
pub use surfaces::Screen;
