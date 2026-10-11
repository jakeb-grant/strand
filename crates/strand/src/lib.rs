//! The Strand shell runtime as a library: the module tree behind the
//! `strand` binary (`main.rs` keeps `main` and the COMMANDS table).
//!
//! **Unstable.** The library exists for plugins (`strand::Shell::builder()`,
//! docs/architecture.md, "Plugins"), git-only: nothing here is a stable
//! API, and [`cli`] is the binary's own entry points.
//!
//! See `docs/design.md` for the design and roadmap.

mod check;
mod demo;
mod fmt;
mod ipc;
mod live;
mod logging;
mod mock;
mod overlay;
mod rules;
mod run;
mod services;
mod system;

#[cfg(test)]
mod bench;
#[cfg(test)]
mod fuzz;

/// What the `strand` binary calls (not an API: it changes with the CLI).
#[doc(hidden)]
pub mod cli {
    pub use crate::check::{default_dir, run as check};
    pub use crate::demo::run as demo;
    pub use crate::fmt::run as fmt;
    pub use crate::ipc::{reload_cli, set_cli, watch_cli};
    pub use crate::logging::LogConfig;
    pub use crate::rules::run as compositor_rules;
    pub use crate::run::{run, trim};
}

/// The unit tests' harness runs with the binary's allocator and THP
/// setting (`main.rs`'s `GLOBAL` and `NO_THP`, which the binary links;
/// a library's `#[used]` constructor is not guaranteed to reach it).
#[cfg(test)]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(all(test, target_os = "linux"))]
#[used]
#[unsafe(link_section = ".init_array.00100")]
static NO_THP: extern "C" fn() = {
    extern "C" fn no_thp() {
        strand_services::child::thp_off();
    }
    no_thp
};

#[cfg(test)]
mod tests {
    /// The library's unit tests allocate as the binary does.
    #[test]
    fn mimalloc_is_the_test_harness_allocator() {
        let b = Box::new([0u8; 64]);
        let p = Box::into_raw(b);
        // SAFETY: `p` is a live allocation; the query only reads metadata.
        let ours = unsafe { libmimalloc_sys::mi_is_in_heap_region(p.cast()) };
        // SAFETY: `p` came from `Box::into_raw` above.
        drop(unsafe { Box::from_raw(p) });
        assert!(ours, "the Box was not allocated by mimalloc");
    }

    /// `NO_THP` ran before the test harness's `main`.
    #[test]
    #[cfg(target_os = "linux")]
    fn transparent_huge_pages_are_off_in_the_test_harness() {
        if let Ok(off) = rustix::thread::transparent_huge_pages_are_disabled() {
            assert!(off, "THP is on for the process");
        }
    }
}
