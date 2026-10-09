//! Solid surfaces (design.md: "Scrims and lock backgrounds use
//! single-pixel buffers; a 4K triple buffer would cost about 99 MB"): one
//! colour over a whole surface is one `wp_single_pixel_buffer_v1` pixel
//! the viewporter scales up, or, without the protocol, a 1×1 shm pixel
//! the viewporter scales (or, without that too, one pixel per logical
//! pixel). Pure, so the pixel values are tested without a compositor; the
//! manager makes the buffers (`manager/catcher.rs`).

use strand_scene::Color;

/// What a solid surface's buffer is.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SolidBuffer {
    /// `wp_single_pixel_buffer_manager_v1.create_u32_rgba_buffer` with
    /// these premultiplied components, scaled by the viewport.
    SinglePixel([u32; 4]),
    /// A `wl_shm` ARGB8888 buffer of `width × height` pixels of `pixel`
    /// (premultiplied, in memory order).
    Shm {
        width: u32,
        height: u32,
        pixel: [u8; 4],
    },
}

/// The buffer for a surface of `size` logical pixels in `color`: a
/// single pixel when the compositor offers single-pixel buffers and the
/// viewporter, a 1×1 shm pixel with the viewporter alone, else a buffer
/// as large as the surface (transparent pixels cost the same memory, so
/// the click-away catcher without a scrim takes the same path).
pub fn solid_buffer(
    color: Color,
    size: (u32, u32),
    single_pixel: bool,
    viewporter: bool,
) -> SolidBuffer {
    if single_pixel && viewporter {
        return SolidBuffer::SinglePixel(single_pixel_rgba(color));
    }
    let (width, height) = if viewporter {
        (1, 1)
    } else {
        (size.0.max(1), size.1.max(1))
    };
    SolidBuffer::Shm {
        width,
        height,
        pixel: color.to_argb8888_premul(),
    }
}

/// `color` as `create_u32_rgba_buffer`'s components: premultiplied by
/// alpha, each scaled so `u32::MAX` is 1.0 (the protocol's definition).
pub fn single_pixel_rgba(color: Color) -> [u32; 4] {
    let c = color.clamped();
    let q = |v: f32| (f64::from(v.clamp(0.0, 1.0)) * f64::from(u32::MAX)).round() as u32;
    [q(c.r * c.a), q(c.g * c.a), q(c.b * c.a), q(c.a)]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Premultiplied, full range: opaque white is all ones, transparent is
    /// all zeros, and a 30 % black scrim is alpha only.
    #[test]
    fn single_pixel_components_are_premultiplied_and_full_range() {
        assert_eq!(
            single_pixel_rgba(Color::new(1.0, 1.0, 1.0, 1.0)),
            [u32::MAX; 4]
        );
        assert_eq!(single_pixel_rgba(Color::new(1.0, 0.5, 0.0, 0.0)), [0; 4]);
        let [r, g, b, a] = single_pixel_rgba(Color::new(0.0, 0.0, 0.0, 0.3));
        assert_eq!([r, g, b], [0; 3]);
        assert!((f64::from(a) / f64::from(u32::MAX) - 0.3).abs() < 1e-6);
        let [r, _, _, a] = single_pixel_rgba(Color::new(1.0, 0.0, 0.0, 0.5));
        assert!(r.abs_diff(a) <= 1, "red premultiplied by half: {r} vs {a}");
        // Out-of-range values are clamped.
        assert_eq!(
            single_pixel_rgba(Color::new(2.0, -1.0, 0.0, 1.0))[..2],
            [u32::MAX, 0]
        );
    }

    /// The ladder: single pixel with both protocols, a 1×1 shm pixel with
    /// the viewporter alone, a surface-sized buffer with neither.
    #[test]
    fn the_buffer_falls_back_to_shm() {
        let dim = Color::new(0.0, 0.0, 0.0, 0.3);
        assert!(matches!(
            solid_buffer(dim, (1920, 1080), true, true),
            SolidBuffer::SinglePixel(_)
        ));
        assert_eq!(
            solid_buffer(dim, (1920, 1080), true, false),
            SolidBuffer::Shm {
                width: 1920,
                height: 1080,
                pixel: [0, 0, 0, 77]
            }
        );
        assert_eq!(
            solid_buffer(dim, (1920, 1080), false, true),
            SolidBuffer::Shm {
                width: 1,
                height: 1,
                pixel: [0, 0, 0, 77]
            }
        );
        assert_eq!(
            solid_buffer(Color::default(), (0, 0), false, false),
            SolidBuffer::Shm {
                width: 1,
                height: 1,
                pixel: [0; 4]
            }
        );
    }
}
