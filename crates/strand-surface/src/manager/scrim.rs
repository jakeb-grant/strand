//! Solid buffers for click-away catchers and scrims
//! (`manager/catcher.rs`): the shm fallback when the compositor offers
//! no single-pixel buffers ([`crate::solid`]), and the user data of
//! their buffers.

use super::*;

/// User data of a catcher's or scrim's buffer (nothing to track: it is
/// never written after it is made).
#[derive(Debug)]
pub struct ScrimObject;

/// A `width × height` ARGB8888 shm buffer of `pixel`, with its pool.
#[allow(clippy::type_complexity)]
pub(super) fn shm_solid<H: SurfaceHost + 'static>(
    shm: &Shm,
    qh: &QueueHandle<State<H>>,
    width: u32,
    height: u32,
    pixel: [u8; 4],
) -> Option<((Option<RawPool>, wl_buffer::WlBuffer), (i32, i32))> {
    let (Ok(bw), Ok(bh)) = (i32::try_from(width), i32::try_from(height)) else {
        return None;
    };
    let len = (bw as usize)
        .checked_mul(bh as usize)
        .and_then(|n| n.checked_mul(4))?;
    let mut pool = match RawPool::new(len, shm) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("no buffer for a scrim or catcher: {e}");
            return None;
        }
    };
    // A fresh pool is zeroed: fully transparent.
    if pixel != [0; 4] {
        for px in pool.mmap()[..len].chunks_exact_mut(4) {
            px.copy_from_slice(&pixel);
        }
    }
    let buffer = pool.create_buffer(0, bw, bh, bw * 4, wl_shm::Format::Argb8888, ScrimObject, qh);
    Some(((Some(pool), buffer), (bw, bh)))
}

impl<H: SurfaceHost + 'static> Dispatch2<wl_buffer::WlBuffer, State<H>> for ScrimObject {
    fn event(
        &self,
        _: &mut State<H>,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
    }
}
