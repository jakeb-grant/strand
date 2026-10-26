//! (wasm-spike) The monotonic clock type strand-core names, as
//! strand-scene's `time` (which it cannot depend on)
//! (docs/live-examples.md). Natively it is `std::time::Instant`, so nothing
//! changes. On wasm32-unknown-unknown std has no clock (its
//! `Instant::now()` panics), so there it is web-time's, which reads the
//! page's `performance.now()`; every crate on the render path names
//! `Instant` through here so their interfaces (`Renderer::set_now`,
//! `Router::set_now`) take the same type on either target.

#[cfg(not(target_family = "wasm"))]
pub use std::time::Instant;
#[cfg(target_family = "wasm")]
pub use web_time::Instant;

/// The wall clock now, as a `std::time::SystemTime` on either target
/// (std's `SystemTime::now()` panics on wasm32-unknown-unknown; web-time
/// reads the page's `Date.now()`).
pub fn system_now() -> std::time::SystemTime {
    #[cfg(not(target_family = "wasm"))]
    {
        std::time::SystemTime::now()
    }
    #[cfg(target_family = "wasm")]
    {
        let since = web_time::SystemTime::now()
            .duration_since(web_time::UNIX_EPOCH)
            .unwrap_or_default();
        std::time::UNIX_EPOCH + since
    }
}
