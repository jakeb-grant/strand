//! What the test files share, so that one fix reaches them all: waiting
//! with a deadline ([`wait`]), wall-clock gates held at a test's end
//! ([`gate`]), short private directories and directories out of the way of
//! other tests ([`dir`]), a headless sway ([`sway`]), a private D-Bus session
//! daemon ([`bus`]).
//!
//! A dev-dependency only, on std alone: see `Cargo.toml`.

pub mod bus;
pub mod dir;
pub mod gate;
pub mod sway;
pub mod wait;
