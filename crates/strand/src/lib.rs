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

/// Declares the process setup the `strand` binary runs with, at the
/// crate root of the binary that invokes it: mimalloc as the global
/// allocator (`docs/design.md`, "Stack") and no transparent huge pages
/// from before the first allocation, but for the large pixel buffers
/// render advises on kernels that allow it (decisions.md m5-speed-thp). A plugin binary
/// (`strand::Shell::builder()`) invokes it once, beside copying the
/// workspace's `[profile.release]`, or the PSS figures do not hold
/// (docs/architecture.md, "Plugins"; decisions.md m5-w0-harden).
///
/// THP: on a system with THP `always` (GitHub's runners) the kernel
/// backs a 2 MiB-aligned anonymous range with a huge page on its first
/// touch: mimalloc's arenas filled 2 MiB pages for a few KiB of heap
/// (55 MB PSS for design.md's bar instead of about 25). mimalloc's
/// `no_thp` only stops it asking for them, and turning THP off in `main`
/// came after the runtime's first allocations (the thread handle, the
/// arguments) had already faulted two huge pages in (4 MB of
/// `AnonHugePages`, the M0 gate missed by 265 kB in CI run 37644817292).
/// An ELF constructor at priority 100 runs before `main` and every other
/// constructor in the executable but std's argv capture (priority 99,
/// which does not allocate): mimalloc's own included (checked in the
/// release binary's `.init_array`: argv, this, then the unprioritised
/// rest). The call makes at most three `prctl`s and does not allocate. A failure
/// (an old kernel) leaves the default. The kernel keeps the flag across
/// `fork` and `exec`: every program strand starts gets back what strand
/// inherited (`strand_services::child::restore_in_child`, in each
/// `pre_exec`). It is a macro, not statics here: a library's `#[used]`
/// constructor is not guaranteed to reach the binary that links it.
#[macro_export]
macro_rules! process_setup {
    () => {
        #[global_allocator]
        static STRAND_GLOBAL: $crate::__process::MiMalloc = $crate::__process::MiMalloc;

        #[cfg(target_os = "linux")]
        #[used]
        #[unsafe(link_section = ".init_array.00100")]
        static STRAND_NO_THP: extern "C" fn() = {
            extern "C" fn no_thp() {
                $crate::__process::thp_off();
            }
            no_thp
        };
    };
}

/// What [`process_setup!`] names (not an API).
#[doc(hidden)]
pub mod __process {
    pub use mimalloc::MiMalloc;
    pub use strand_services::child::thp_off;
}

// The unit tests' harness runs with the binary's allocator and THP
// setting.
#[cfg(test)]
process_setup!();

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

    /// `process_setup!`'s constructor ran before the test harness's `main`.
    #[test]
    #[cfg(target_os = "linux")]
    fn transparent_huge_pages_are_off_in_the_test_harness() {
        if let Ok(off) = rustix::thread::transparent_huge_pages_are_disabled() {
            assert!(off, "THP is on for the process");
        }
    }
}
