//! Transparent huge pages for the one large pixel buffer that gains from
//! them: a crossfade's snapshot (33 MB at 4K).
//!
//! `strand` turns THP off for its process (the memory budget: decisions.md
//! wave4-exitMemory) except, on kernels that have it (Linux 6.18 on),
//! for regions advised `MADV_HUGEPAGE` (`PR_THP_DISABLE_EXCEPT_ADVISED`,
//! `strand_services::child::thp_off`). A [`HugeBuf`] of at least
//! [`HUGE_MIN`] bytes is its own anonymous mapping, advised before it is
//! first written, so the kernel faults it in 2 MiB at a time: the first
//! 4K crossfade of a run takes about 10 ms less (decisions.md,
//! m5-speed-thp). The advice lives on that mapping alone: no heap memory
//! the allocator hands out can inherit it.
//!
//! A dropped mapping is kept as the one spare, which the next buffer of
//! the same mapped size reuses with its pages already in (a crossfade
//! after a crossfade faults nothing in, as the heap's reuse did); the
//! spare is unmapped, advice and all, by [`trim_idle`] once unused for
//! the paint cache's idle time (`cache::IDLE_FREE`), or when a buffer of
//! another size replaces it. Elsewhere (THP fully off on an older kernel,
//! or not Linux) the advice changes nothing and the spare still saves
//! the faults.

use std::time::{Duration, Instant};

/// Buffers smaller than this are plain heap memory, never advised: no
/// bar, toast or icon buffer is this large.
pub const HUGE_MIN: usize = 8 << 20;

/// The huge page size (x86-64 and aarch64 with 4 KiB pages).
#[cfg(target_os = "linux")]
const HUGE: usize = 2 << 20;

/// A zeroed byte buffer of fixed length: its own `MADV_HUGEPAGE` mapping
/// when at least [`HUGE_MIN`] bytes on Linux (2 MiB aligned, unmapped on
/// drop), else a `Vec`.
pub struct HugeBuf(Repr);

enum Repr {
    Heap(Vec<u8>),
    #[cfg(target_os = "linux")]
    Mapped {
        /// The mapping's start and length (a whole number of 2 MiB pages
        /// past the aligned start).
        map: *mut u8,
        map_len: usize,
        /// The buffer: `len` bytes from the first 2 MiB boundary.
        ptr: *mut u8,
        len: usize,
        /// Taken from the spare (pages in, contents stale).
        reused: bool,
    },
}

// SAFETY: the mapping is owned by this value alone, like a `Vec`'s heap
// block; nothing else holds the pointers.
unsafe impl Send for HugeBuf {}
// SAFETY: shared access only reads through `&[u8]`.
unsafe impl Sync for HugeBuf {}

impl Default for HugeBuf {
    fn default() -> Self {
        Self(Repr::Heap(Vec::new()))
    }
}

impl std::fmt::Debug for HugeBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HugeBuf")
            .field("len", &self.len())
            .field("mapped", &self.is_mapped())
            .finish()
    }
}

/// The spare mapping: start, length and when it was dropped.
#[cfg(target_os = "linux")]
static SPARE: std::sync::Mutex<Option<(usize, usize, Instant)>> = std::sync::Mutex::new(None);

/// Unmaps `[map, map + len)`, its advice with it.
#[cfg(target_os = "linux")]
fn unmap(map: usize, len: usize) {
    // SAFETY: a mapping `HugeBuf::map` made, owned by the caller and
    // unmapped once.
    unsafe {
        libc::munmap(map as *mut libc::c_void, len);
    }
}

/// Unmaps the spare mapping if it has been unused for `idle` (called with
/// the paint cache's trims: at every paint and wake of the render loop,
/// never on a wake of its own).
pub fn trim_idle(now: Instant, idle: Duration) {
    #[cfg(target_os = "linux")]
    {
        let mut spare = SPARE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((map, len, at)) = *spare
            && now.saturating_duration_since(at) >= idle
        {
            *spare = None;
            drop(spare);
            unmap(map, len);
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (now, idle);
}

/// Whether a spare mapping is kept (tests).
pub fn has_spare() -> bool {
    #[cfg(target_os = "linux")]
    {
        SPARE.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }
    #[cfg(not(target_os = "linux"))]
    false
}

impl HugeBuf {
    /// `len` zero bytes. A fresh mapping's pages fault in at their first
    /// write (as huge pages where the kernel allows); a reused spare is
    /// cleared.
    pub fn zeroed(len: usize) -> Self {
        let mut b = Self::for_overwrite(len);
        if b.reused() {
            b.fill(0);
        }
        b
    }

    /// `len` bytes the caller overwrites in full: zero, or what the
    /// reused spare held (no write spent clearing it).
    pub fn for_overwrite(len: usize) -> Self {
        #[cfg(target_os = "linux")]
        if len >= HUGE_MIN
            && let Some(b) = Self::map(len)
        {
            return b;
        }
        Self(Repr::Heap(vec![0u8; len]))
    }

    #[cfg(target_os = "linux")]
    fn map(len: usize) -> Option<Self> {
        // One spare huge page so the buffer can start on a 2 MiB boundary.
        let map_len = len.checked_next_multiple_of(HUGE)?.checked_add(HUGE)?;
        let taken = SPARE.lock().unwrap_or_else(|e| e.into_inner()).take();
        let (map, reused) = match taken {
            Some((map, l, _)) if l == map_len => (map as *mut u8, true),
            other => {
                if let Some((map, l, _)) = other {
                    unmap(map, l);
                }
                // SAFETY: a fresh private anonymous mapping; no existing
                // memory is touched.
                let map = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        map_len,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                        -1,
                        0,
                    )
                };
                if map == libc::MAP_FAILED {
                    return None;
                }
                (map.cast::<u8>(), false)
            }
        };
        let pad = (map as usize).next_multiple_of(HUGE) - map as usize;
        // SAFETY: `pad < HUGE`, inside the mapping.
        let ptr = unsafe { map.add(pad) };
        if !reused {
            // SAFETY: `[ptr, ptr + round_up(len))` lies inside the
            // mapping; `MADV_HUGEPAGE` changes only how its pages are
            // backed. A failure (no THP in the kernel) is ignored: small
            // pages, same contents.
            unsafe {
                libc::madvise(ptr.cast(), len.next_multiple_of(HUGE), libc::MADV_HUGEPAGE);
            }
        }
        Some(Self(Repr::Mapped {
            map,
            map_len,
            ptr,
            len,
            reused,
        }))
    }

    /// Whether the buffer is its own advised mapping.
    pub fn is_mapped(&self) -> bool {
        !matches!(self.0, Repr::Heap(_))
    }

    /// Whether the buffer reused the spare mapping.
    fn reused(&self) -> bool {
        match self.0 {
            Repr::Heap(_) => false,
            #[cfg(target_os = "linux")]
            Repr::Mapped { reused, .. } => reused,
        }
    }
}

impl std::ops::Deref for HugeBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match &self.0 {
            Repr::Heap(v) => v,
            // SAFETY: `len` initialised (zero-filled) bytes, owned by self.
            #[cfg(target_os = "linux")]
            Repr::Mapped { ptr, len, .. } => unsafe { std::slice::from_raw_parts(*ptr, *len) },
        }
    }
}

impl std::ops::DerefMut for HugeBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        match &mut self.0 {
            Repr::Heap(v) => v,
            // SAFETY: as `deref`, borrowed mutably through `self`.
            #[cfg(target_os = "linux")]
            Repr::Mapped { ptr, len, .. } => unsafe { std::slice::from_raw_parts_mut(*ptr, *len) },
        }
    }
}

impl Drop for HugeBuf {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if let Repr::Mapped { map, map_len, .. } = self.0 {
            // Kept as the spare; the one it replaces is unmapped.
            let old = SPARE.lock().unwrap_or_else(|e| e.into_inner()).replace((
                map as usize,
                map_len,
                Instant::now(),
            ));
            if let Some((map, len, _)) = old {
                unmap(map, len);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small buffer is heap memory, a large one its own 2 MiB-aligned
    /// mapping; both read zero and keep what is written.
    #[test]
    fn large_buffers_are_their_own_aligned_mapping() {
        let mut small = HugeBuf::zeroed(1 << 20);
        assert!(!small.is_mapped());
        assert!(small.iter().all(|b| *b == 0));
        small[5] = 9;
        assert_eq!(small[5], 9);
        let mut big = HugeBuf::zeroed(HUGE_MIN + 12_345);
        assert_eq!(big.len(), HUGE_MIN + 12_345);
        #[cfg(target_os = "linux")]
        {
            assert!(big.is_mapped());
            assert_eq!(big.as_ptr() as usize % HUGE, 0);
        }
        assert!(big.iter().all(|b| *b == 0));
        big.fill(7);
        assert!(big.iter().all(|b| *b == 7));
        assert!(HugeBuf::default().is_empty());
        #[cfg(target_os = "linux")]
        {
            // Dropped, it is the spare; the next buffer of its size reuses
            // it, cleared when asked for zeroes.
            let at = big.as_ptr() as usize;
            drop(big);
            assert!(has_spare());
            let again = HugeBuf::zeroed(HUGE_MIN + 12_345);
            assert!(again.reused() && again.as_ptr() as usize == at);
            assert!(again.iter().all(|b| *b == 0));
            drop(again);
            // Unused past the idle time, the spare is unmapped.
            trim_idle(Instant::now(), Duration::from_secs(3600));
            assert!(has_spare());
            trim_idle(Instant::now() + Duration::from_secs(1), Duration::ZERO);
            assert!(!has_spare());
        }
    }
}
