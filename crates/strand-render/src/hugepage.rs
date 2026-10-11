//! Transparent huge pages for the few large pixel buffers: a crossfade's
//! snapshot (33 MB at 4K) and the paint cache's and offscreen groups'
//! largest pixmaps.
//!
//! `strand` turns THP off for its process (the memory budget: decisions.md
//! wave4-exitMemory) except, on kernels that have it (Linux 6.18 on),
//! for regions advised `MADV_HUGEPAGE` (`PR_THP_DISABLE_EXCEPT_ADVISED`,
//! `strand_services::child::thp_off`). [`advise`] advises a buffer's whole
//! 2 MiB pages before it is first written, so the kernel faults it in
//! 2 MiB at a time and maps it with fewer TLB entries: the first 4K
//! crossfade of a run takes about 10 ms less and each of its blend frames
//! a few percent less (decisions.md, m5-speed-thp). Elsewhere (THP fully
//! off on an older kernel, or not Linux) the advice changes nothing.

/// Buffers smaller than this are never advised: no bar, toast or icon
/// buffer is, so the shell's steady memory has no huge page.
pub const HUGE_MIN: usize = 8 << 20;

/// The huge page size advised in (x86-64 and aarch64 with 4 KiB pages).
const HUGE: usize = 2 << 20;

/// Advises the whole 2 MiB pages inside `[ptr, ptr + len)` for transparent
/// huge pages, when `len` is at least [`HUGE_MIN`]. Call it before the
/// buffer is first written: pages already faulted in stay small.
pub fn advise(ptr: *const u8, len: usize) {
    if len < HUGE_MIN {
        return;
    }
    let start = (ptr as usize).next_multiple_of(HUGE);
    let end = (ptr as usize).saturating_add(len) & !(HUGE - 1);
    if end <= start {
        return;
    }
    #[cfg(target_os = "linux")]
    // SAFETY: `[start, end)` lies inside the caller's live allocation
    // (rounded inward to whole pages), and `MADV_HUGEPAGE` changes only
    // how its pages are backed, never their contents. A failure (no THP
    // in the kernel) is ignored.
    unsafe {
        libc::madvise(start as *mut libc::c_void, end - start, libc::MADV_HUGEPAGE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small buffer is left alone, a large one is advised inside its
    /// bounds (the call returns; the kernel's answer is not observable
    /// here when THP is off for the process).
    #[test]
    fn advises_only_large_buffers_inside_their_bounds() {
        let small = vec![0u8; 1 << 20];
        advise(small.as_ptr(), small.len());
        let mut big: Vec<u8> = Vec::with_capacity(HUGE_MIN + HUGE);
        advise(big.as_ptr(), big.capacity());
        big.resize(HUGE_MIN + HUGE, 7);
        assert!(big.iter().all(|b| *b == 7));
    }
}
