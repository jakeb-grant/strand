//! A binary that links the `strand` library, as a plugin does, gets the
//! release binary's process setup from `strand::process_setup!`
//! (docs/architecture.md, "Plugins"; decisions.md m5-w0-harden): this
//! test binary invokes it, so its allocator and THP setting are the
//! plugin's.

strand::process_setup!();

/// mimalloc is the global allocator of a binary that invokes the macro.
#[test]
fn a_linking_binary_allocates_with_mimalloc() {
    let b = Box::new([0u8; 64]);
    let p = Box::into_raw(b);
    // SAFETY: `p` is a live allocation; the query only reads metadata.
    let ours = unsafe { libmimalloc_sys::mi_is_in_heap_region(p.cast()) };
    // SAFETY: `p` came from `Box::into_raw` above.
    drop(unsafe { Box::from_raw(p) });
    assert!(ours, "the Box was not allocated by mimalloc");
}

/// The macro's constructor turned THP off before the harness's `main`.
#[test]
#[cfg(target_os = "linux")]
fn a_linking_binary_runs_with_thp_off() {
    if let Ok(off) = rustix::thread::transparent_huge_pages_are_disabled() {
        assert!(off, "THP is on for the process");
    }
}
