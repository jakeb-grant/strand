//! Per-surface `wl_shm` buffers: one pool holding 2–3 ARGB8888 buffers,
//! with buffer age and release tracking.
//!
//! [`Slots`] is the bookkeeping (which buffer is busy, which frame each one
//! holds) and is tested on its own; [`ShmBuffers`] backs it with a
//! `wl_shm` pool.
//!
//! Age follows `EGL_EXT_buffer_age`, counted in committed frames of the
//! surface: 0 = unknown contents, 1 = the buffer holds the frame committed
//! last, 2 = the one before that, and so on.
//!
//! Resizing is safe without waiting for releases: a new size gets a new
//! pool and the old buffers are destroyed at once. `wl_surface.attach`
//! allows destroying a buffer before its release as long as its storage is
//! never written again, and the old pool's memory never is.

use smithay_client_toolkit::shm::Shm;
use smithay_client_toolkit::shm::raw::RawPool;
use wayland_client::QueueHandle;
use wayland_client::protocol::{wl_buffer, wl_shm};

use strand_scene::{BYTES_PER_PIXEL, Size, SurfaceId};

/// Buffers per surface: two in steady state, a third only while the
/// compositor holds both.
pub const MAX_BUFFERS: usize = 3;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct Slot {
    busy: bool,
    /// The commit number (1-based) of the frame this buffer holds.
    frame: Option<u64>,
}

/// Buffer bookkeeping for one surface at one size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slots {
    slots: Vec<Slot>,
    max: usize,
    /// Frames committed since the last [`Slots::reset`].
    commits: u64,
}

/// A buffer handed out for painting.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Acquired {
    pub index: usize,
    /// True if the slot did not exist before: the caller creates its
    /// `wl_buffer`.
    pub fresh: bool,
    pub age: u8,
}

impl Slots {
    /// `max` is clamped to 2..=[`MAX_BUFFERS`].
    pub fn new(max: usize) -> Self {
        Self {
            slots: Vec::new(),
            max: max.clamp(2, MAX_BUFFERS),
            commits: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Frames committed at this size.
    pub fn commits(&self) -> u64 {
        self.commits
    }

    fn age_of(&self, slot: Slot) -> u8 {
        match slot.frame {
            Some(f) => u8::try_from(self.commits + 1 - f).unwrap_or(u8::MAX),
            None => 0,
        }
    }

    /// The buffer to paint the next frame into: the free buffer holding the
    /// most recent frame (least to repaint), else a new one while fewer
    /// than `max` exist. `None` when every buffer is with the compositor.
    pub fn acquire(&mut self) -> Option<Acquired> {
        let best = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.busy)
            .max_by_key(|(_, s)| s.frame.map_or(0, |f| f + 1));
        if let Some((index, slot)) = best {
            return Some(Acquired {
                index,
                fresh: false,
                age: self.age_of(*slot),
            });
        }
        if self.slots.len() < self.max {
            self.slots.push(Slot::default());
            return Some(Acquired {
                index: self.slots.len() - 1,
                fresh: true,
                age: 0,
            });
        }
        None
    }

    /// The painted buffer `index` was committed: it now holds the newest
    /// frame and belongs to the compositor until released.
    pub fn commit(&mut self, index: usize) {
        self.commits += 1;
        if let Some(slot) = self.slots.get_mut(index) {
            slot.busy = true;
            slot.frame = Some(self.commits);
        }
    }

    /// `wl_buffer.release` for `index`.
    pub fn release(&mut self, index: usize) {
        if let Some(slot) = self.slots.get_mut(index) {
            slot.busy = false;
        }
    }

    /// The buffer's contents are unknown (painted into, then not
    /// committed): its age becomes 0.
    pub fn invalidate(&mut self, index: usize) {
        if let Some(slot) = self.slots.get_mut(index) {
            slot.frame = None;
        }
    }

    pub fn is_busy(&self, index: usize) -> bool {
        self.slots.get(index).is_some_and(|s| s.busy)
    }

    /// Forgets every buffer (new size).
    pub fn reset(&mut self) {
        self.slots.clear();
        self.commits = 0;
    }
}

/// User data of our `wl_buffer`s: which surface, pool generation and slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BufferData {
    pub surface: SurfaceId,
    pub generation: u64,
    pub index: usize,
}

/// Why a buffer could not be provided.
#[derive(Debug)]
pub enum BufferError {
    /// The size does not fit an `i32` stride or pool length.
    TooLarge(Size),
    /// Creating or growing the shared-memory pool failed.
    Pool(String),
}

impl std::fmt::Display for BufferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge(s) => write!(f, "a {}×{} buffer is too large for wl_shm", s.w, s.h),
            Self::Pool(e) => write!(f, "shm pool: {e}"),
        }
    }
}

impl std::error::Error for BufferError {}

/// One surface's `wl_shm` pool and buffers at the current size.
#[derive(Debug)]
pub(crate) struct ShmBuffers {
    surface: SurfaceId,
    size: Size,
    stride: u32,
    generation: u64,
    pool: Option<RawPool>,
    buffers: Vec<wl_buffer::WlBuffer>,
    pub slots: Slots,
}

impl ShmBuffers {
    pub fn new(surface: SurfaceId, max: usize) -> Self {
        Self {
            surface,
            size: Size::default(),
            stride: 0,
            generation: 0,
            pool: None,
            buffers: Vec::new(),
            slots: Slots::new(max),
        }
    }

    pub fn size(&self) -> Size {
        self.size
    }

    pub fn stride(&self) -> u32 {
        self.stride
    }

    /// Bytes of one buffer.
    fn buffer_len(&self) -> usize {
        self.stride as usize * self.size.h as usize
    }

    /// Switches to `size`: destroys the buffers and the pool (their memory
    /// is never written again, so busy ones stay valid for the compositor)
    /// and starts a new generation.
    pub fn resize(&mut self, size: Size) {
        if size == self.size && self.pool.is_some() {
            return;
        }
        self.destroy();
        self.size = size;
        self.stride = size.w.saturating_mul(BYTES_PER_PIXEL);
    }

    /// Destroys every buffer and the pool.
    pub fn destroy(&mut self) {
        for b in self.buffers.drain(..) {
            b.destroy();
        }
        self.pool = None;
        self.slots.reset();
        self.generation += 1;
    }

    /// A buffer to paint into, creating the pool or a new buffer as needed.
    /// `Ok(None)` when every buffer is busy.
    pub fn acquire<D>(
        &mut self,
        shm: &Shm,
        qh: &QueueHandle<D>,
    ) -> Result<Option<Acquired>, BufferError>
    where
        D: wayland_client::Dispatch<wl_buffer::WlBuffer, BufferData> + 'static,
    {
        let size = self.size;
        let len = self.buffer_len();
        let too_large = || BufferError::TooLarge(size);
        let (Ok(w), Ok(h), Ok(stride)) = (
            i32::try_from(size.w),
            i32::try_from(size.h),
            i32::try_from(self.stride),
        ) else {
            return Err(too_large());
        };
        if size.is_empty() {
            return Err(too_large());
        }
        let Some(acquired) = self.slots.acquire() else {
            return Ok(None);
        };
        if acquired.fresh {
            let index = acquired.index;
            let needed = len
                .checked_mul(index + 1)
                .filter(|n| i32::try_from(*n).is_ok())
                .ok_or_else(too_large)?;
            let offset = i32::try_from(len * index).map_err(|_| too_large())?;
            let pool = match &mut self.pool {
                Some(pool) => {
                    pool.resize(needed)
                        .map_err(|e| BufferError::Pool(e.to_string()))?;
                    pool
                }
                None => self.pool.insert(
                    RawPool::new(needed, shm).map_err(|e| BufferError::Pool(e.to_string()))?,
                ),
            };
            let data = BufferData {
                surface: self.surface,
                generation: self.generation,
                index,
            };
            let buffer =
                pool.create_buffer(offset, w, h, stride, wl_shm::Format::Argb8888, data, qh);
            debug_assert_eq!(self.buffers.len(), index);
            self.buffers.push(buffer);
        }
        Ok(Some(acquired))
    }

    /// The pixels of buffer `index`.
    pub fn pixels(&mut self, index: usize) -> Option<&mut [u8]> {
        let len = self.buffer_len();
        let pool = self.pool.as_mut()?;
        let start = len.checked_mul(index)?;
        pool.mmap().get_mut(start..start.checked_add(len)?)
    }

    pub fn buffer(&self, index: usize) -> Option<&wl_buffer::WlBuffer> {
        self.buffers.get(index)
    }

    /// Handles `wl_buffer.release`. Returns true if it freed a buffer of
    /// the current generation.
    pub fn release(&mut self, data: &BufferData) -> bool {
        if data.generation != self.generation || !self.slots.is_busy(data.index) {
            return false;
        }
        self.slots.release(data.index);
        true
    }
}

impl Drop for ShmBuffers {
    fn drop(&mut self) {
        self.destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_frames_allocate_then_alternate() {
        let mut s = Slots::new(3);
        let a = s.acquire().unwrap();
        assert_eq!((a.index, a.fresh, a.age), (0, true, 0));
        s.commit(a.index);
        // Buffer 0 is on screen: frame 2 needs a second buffer.
        let b = s.acquire().unwrap();
        assert_eq!((b.index, b.fresh, b.age), (1, true, 0));
        s.commit(b.index);
        s.release(0);
        // Buffer 0 holds frame 1; frame 3 is painted over it: age 2.
        let c = s.acquire().unwrap();
        assert_eq!((c.index, c.fresh, c.age), (0, false, 2));
        s.commit(c.index);
        s.release(1);
        let d = s.acquire().unwrap();
        assert_eq!((d.index, d.age), (1, 2));
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn third_buffer_only_while_two_are_busy() {
        let mut s = Slots::new(3);
        for _ in 0..2 {
            let a = s.acquire().unwrap();
            s.commit(a.index);
        }
        let c = s.acquire().unwrap();
        assert_eq!((c.index, c.fresh), (2, true));
        s.commit(c.index);
        assert_eq!(s.acquire(), None, "all three are with the compositor");
        s.release(1);
        let d = s.acquire().unwrap();
        // Buffer 1 holds frame 2 of 3 committed: age 2.
        assert_eq!((d.index, d.age), (1, 2));
    }

    #[test]
    fn two_buffer_limit_waits() {
        let mut s = Slots::new(1);
        for _ in 0..2 {
            let a = s.acquire().unwrap();
            s.commit(a.index);
        }
        assert_eq!(s.acquire(), None);
    }

    #[test]
    fn prefers_the_most_recent_free_buffer() {
        let mut s = Slots::new(3);
        for _ in 0..3 {
            let a = s.acquire().unwrap();
            s.commit(a.index);
        }
        // Frames 1, 2, 3 in buffers 0, 1, 2; 0 and 1 come back.
        s.release(0);
        s.release(1);
        let a = s.acquire().unwrap();
        assert_eq!((a.index, a.age), (1, 2));
    }

    #[test]
    fn same_buffer_released_before_next_frame_has_age_one() {
        // A compositor that copies (releases at once) lets one buffer
        // serve every frame, always holding the last frame.
        let mut s = Slots::new(3);
        for i in 0..5 {
            let a = s.acquire().unwrap();
            assert_eq!(a.index, 0);
            assert_eq!(a.age, if i == 0 { 0 } else { 1 });
            s.commit(a.index);
            s.release(a.index);
        }
        assert_eq!(s.commits(), 5);
    }

    #[test]
    fn invalidate_and_reset_forget_contents() {
        let mut s = Slots::new(2);
        let a = s.acquire().unwrap();
        s.commit(a.index);
        s.release(a.index);
        s.invalidate(a.index);
        assert_eq!(s.acquire().unwrap().age, 0);
        s.reset();
        assert!(s.is_empty());
        assert_eq!(s.commits(), 0);
        assert!(s.acquire().unwrap().fresh);
    }
}
