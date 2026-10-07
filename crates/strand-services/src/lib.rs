//! System services.
//!
//! Typed stores that start on first subscription, are reference-counted, and
//! stop 5 s after their last reader leaves. They share one tokio
//! current-thread runtime; PipeWire and the Wayland toplevel protocols get
//! their own threads.
//!
//! See `docs/design.md`, "System services and third-party crates". Lands in M3.
#[cfg(feature = "pipewire")]
pub mod audio;
pub mod wm;
