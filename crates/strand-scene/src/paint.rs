//! The paint contract between the surface manager (caller) and the render
//! thread (implementor).

use std::time::Duration;

use crate::damage::Damage;
use crate::geometry::{LogicalPoint, Rect, Scale, Size};
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
    /// Buffer age: 0 = unknown contents, 1 = last frame, ... counted in
    /// commits of this surface (see [`Painter::paint`]).
    pub age: u8,
    /// When this frame is expected on screen: the predicted presentation
    /// time on the `wp_presentation` clock (`CLOCK_MONOTONIC`). Springs and
    /// time signals are sampled at it; tests pass fixed values.
    /// [`PaintTarget::new`] sets zero; use [`PaintTarget::at`].
    pub time: Duration,
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
            time: Duration::ZERO,
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

    /// Sets the frame's presentation time.
    pub fn at(mut self, time: Duration) -> Self {
        self.time = time;
        self
    }

    /// The whole buffer as a rectangle.
    pub fn bounds(&self) -> Rect {
        Rect::from_size(self.size)
    }
}

/// A rounded box a node asks the compositor to blur behind (`blur: 24`),
/// in buffer pixels: the first rung of the blur ladder
/// (`ext-background-effect-v1`, M4), whose region follows the rounded
/// shape. Without it render draws the tint fallback itself.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct BlurRegion {
    pub rect: Rect,
    /// Corner radii in buffer pixels, clockwise from top-left.
    pub radii: [f32; 4],
    /// The blur radius in logical pixels.
    pub radius: f32,
}

/// (M4) The pose the compositor applies to a whole surface when render
/// delegates its root's pose (design.md, "Compositor-animated poses"):
/// opacity through `wp_alpha_modifier_v1`, scale through the
/// viewporter's destination size, and the offset through layer-shell
/// margins. Render paints the content at rest meanwhile.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct SurfacePose {
    /// `0..=1`, multiplied into the surface's alpha.
    pub opacity: f32,
    /// The surface's size over its laid-out size, with its top-left
    /// corner kept (as compositors draw a layer surface smaller than its
    /// arranged box); render folds the move that keeps the root's centre
    /// in place into `offset`.
    pub scale: f32,
    /// Logical pixels the surface's top-left corner moves from where it
    /// is placed (layer-shell margins).
    pub offset: LogicalPoint,
}

impl SurfacePose {
    /// The pose of a surface at rest.
    pub const IDENTITY: SurfacePose = SurfacePose {
        opacity: 1.0,
        scale: 1.0,
        offset: LogicalPoint::new(0.0, 0.0),
    };

    /// True when applying it changes nothing.
    pub fn is_identity(&self) -> bool {
        *self == Self::IDENTITY
    }
}

impl Default for SurfacePose {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// Implemented by the render thread, called by the surface manager.
pub trait Painter {
    /// Paint everything that changed for `surface` and return the damage,
    /// already widened to cover the buffer's age and clipped to the buffer.
    ///
    /// Buffer-age contract: a non-empty result is a new frame and the
    /// caller must commit that buffer (with exactly this damage); an empty
    /// result means nothing was drawn and nothing was recorded, so the
    /// caller must not count a commit for it. Ages are counted in commits
    /// of this surface.
    fn paint(&mut self, surface: SurfaceId, target: &mut PaintTarget<'_>) -> Damage;
    /// True while something on this surface is dirty or a spring or time
    /// signal is unsettled; the surface manager requests frame callbacks
    /// only while true.
    fn wants_frame(&self, surface: SurfaceId) -> bool;
    /// The part of the last painted frame that is fully opaque, in
    /// physical *buffer* pixels. Empty when nothing is known to be opaque.
    /// `wl_surface.set_opaque_region` takes surface-local logical
    /// coordinates: convert with [`crate::Scale::inner_logical_region`],
    /// which rounds inward so translucent pixels are never claimed.
    fn opaque_region(&self, surface: SurfaceId) -> Damage {
        let _ = surface;
        Damage::new()
    }
    /// Where the last painted frame asks the compositor to blur behind
    /// the surface (nodes with `blur`), in buffer pixels. Empty when
    /// nothing asks.
    fn blur_region(&self, surface: SurfaceId) -> Vec<BlurRegion> {
        let _ = surface;
        Vec::new()
    }
    /// (M4) The pose the compositor should apply to the whole surface
    /// this frame, when render delegates its root's pose (after
    /// `Renderer::set_compositor_poses(true)`); `None` means identity. A
    /// frame whose pose changed while `paint` returned no damage is a
    /// pose-only commit: no buffer is attached and `age` does not
    /// advance.
    fn surface_pose(&self, surface: SurfaceId) -> Option<SurfacePose> {
        let _ = surface;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Blank;

    impl Painter for Blank {
        fn paint(&mut self, _: SurfaceId, _: &mut PaintTarget<'_>) -> Damage {
            Damage::new()
        }
        fn wants_frame(&self, _: SurfaceId) -> bool {
            false
        }
    }

    #[test]
    fn a_painter_delegates_no_pose_by_default() {
        assert_eq!(Blank.surface_pose(SurfaceId(1)), None);
        assert!(Blank.blur_region(SurfaceId(1)).is_empty());
        assert_eq!(SurfacePose::default(), SurfacePose::IDENTITY);
        assert!(SurfacePose::IDENTITY.is_identity());
        let half = SurfacePose {
            opacity: 0.5,
            ..SurfacePose::IDENTITY
        };
        assert!(!half.is_identity());
    }

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
