//! The paint contract between the surface manager (caller) and the render
//! thread (implementor).

use crate::damage::Damage;
use crate::geometry::{Rect, Scale, Size};
use crate::id::SurfaceId;

/// Bytes per ARGB8888 pixel.
pub const BYTES_PER_PIXEL: u32 = 4;

/// A buffer to paint into: `wl_shm` ARGB8888, premultiplied alpha,
/// little-endian (bytes in memory are blue, green, red, alpha).
#[derive(Debug)]
pub struct PaintTarget<'a> {
    /// ARGB8888 premultiplied, little-endian (wl_shm).
    pub pixels: &'a mut [u8],
    /// Buffer size in physical pixels.
    pub size: Size,
    /// Bytes per row; at least `size.w * 4`.
    pub stride: u32,
    pub scale: Scale,
    /// Buffer age: 0 = unknown contents, 1 = last frame, ...
    pub age: u8,
}

/// Why a [`PaintTarget`] cannot be painted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetError {
    /// `stride` is smaller than one row of pixels.
    StrideTooSmall { stride: u32, min: u32 },
    /// `pixels` is shorter than `stride × height`.
    BufferTooSmall { len: usize, needed: usize },
}

impl std::fmt::Display for TargetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StrideTooSmall { stride, min } => {
                write!(f, "stride {stride} is smaller than a row ({min} bytes)")
            }
            Self::BufferTooSmall { len, needed } => {
                write!(f, "buffer of {len} bytes is smaller than {needed}")
            }
        }
    }
}

impl std::error::Error for TargetError {}

impl<'a> PaintTarget<'a> {
    /// Wraps a buffer, checking that stride and length fit `size`.
    pub fn new(
        pixels: &'a mut [u8],
        size: Size,
        stride: u32,
        scale: Scale,
        age: u8,
    ) -> Result<Self, TargetError> {
        let target = Self {
            pixels,
            size,
            stride,
            scale,
            age,
        };
        target.validate()?;
        Ok(target)
    }

    /// Checks that `stride` and the buffer length are large enough.
    pub fn validate(&self) -> Result<(), TargetError> {
        let min = self.size.w.saturating_mul(BYTES_PER_PIXEL);
        if self.stride < min {
            return Err(TargetError::StrideTooSmall {
                stride: self.stride,
                min,
            });
        }
        let needed = self.stride as usize * self.size.h as usize;
        if self.pixels.len() < needed {
            return Err(TargetError::BufferTooSmall {
                len: self.pixels.len(),
                needed,
            });
        }
        Ok(())
    }

    /// The whole buffer as a rectangle.
    pub fn bounds(&self) -> Rect {
        Rect::from_size(self.size)
    }
}

/// Implemented by the render thread, called by the surface manager.
pub trait Painter {
    /// Paint everything that changed for `surface` and return the damage,
    /// already widened to cover the buffer's age.
    fn paint(&mut self, surface: SurfaceId, target: &mut PaintTarget<'_>) -> Damage;
    /// True while a spring or time signal on this surface is unsettled;
    /// the surface manager requests frame callbacks only while true.
    fn wants_frame(&self, surface: SurfaceId) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_validation() {
        let mut buf = vec![0u8; 4 * 10 * 2];
        assert!(PaintTarget::new(&mut buf, Size::new(10, 2), 40, Scale::ONE, 0).is_ok());
        assert_eq!(
            PaintTarget::new(&mut buf, Size::new(10, 2), 39, Scale::ONE, 0).unwrap_err(),
            TargetError::StrideTooSmall {
                stride: 39,
                min: 40
            }
        );
        assert!(matches!(
            PaintTarget::new(&mut buf, Size::new(10, 3), 40, Scale::ONE, 0),
            Err(TargetError::BufferTooSmall { .. })
        ));
    }
}
