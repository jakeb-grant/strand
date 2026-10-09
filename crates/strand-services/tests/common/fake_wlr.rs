//! The fake compositor, shared with `strand-surface`'s tests: it lives
//! in the `strand-fake-wayland` crate. Used by tests/protocol.rs and by
//! the protocol client's own unit tests (src/wm/protocol.rs, by
//! `#[path]`).
#![allow(unused_imports)]

pub use strand_fake_wayland::{Cmd, Fake};
