//! Glows drawn on the CPU: a blurred copy of a group's pixels under the
//! group itself. `filter: bloom(r)` without a GPU is this glow of the
//! subtree's own colours (design.md, "Bundled GPU effects": "bloom
//! becomes glow"); `glow: r, color` on text and icons is the same with
//! the copy tinted to one colour.

use strand_scene::Color;

/// Draws `px` (premultiplied BGRA, `w × h`) over a copy of itself blurred
/// with standard deviation `sigma` pixels, tinted to `tint` (its alpha
/// scaling the copy's) when given.
pub(crate) fn glow_under(px: &mut [u8], w: usize, h: usize, sigma: f32, tint: Option<Color>) {
    if !(sigma.is_finite() && sigma > 0.0) || px.len() < w * h * 4 {
        return;
    }
    let mut copy = px[..w * h * 4].to_vec();
    if let Some(c) = tint {
        tint_alpha(&mut copy, c);
    }
    crate::offscreen::blur(&mut copy, w, h, sigma);
    for (d, g) in px.chunks_exact_mut(4).zip(copy.chunks_exact(4)) {
        let k = 255 - d[3] as u32;
        for c in 0..4 {
            let v = d[c] as u32 + (g[c] as u32 * k + 127) / 255;
            d[c] = v.min(255) as u8;
        }
    }
}

/// Repaints every pixel in `c` (premultiplied BGRA), keeping its alpha
/// times `c`'s.
pub(crate) fn tint_alpha(px: &mut [u8], c: Color) {
    let c = c.clamped();
    for p in px.chunks_exact_mut(4) {
        let a = p[3] as f32 / 255.0 * c.a;
        let q = |v: f32| (v * a * 255.0).round().clamp(0.0, 255.0) as u8;
        p[0] = q(c.b);
        p[1] = q(c.g);
        p[2] = q(c.r);
        p[3] = (a * 255.0).round().clamp(0.0, 255.0) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 9×9 transparent square with one opaque white pixel in the middle
    /// glows around it, keeps the pixel as it was, and a tint colours the
    /// halo only.
    #[test]
    fn a_glow_spreads_under_the_pixels() {
        let mut px = vec![0u8; 9 * 9 * 4];
        let mid = (4 * 9 + 4) * 4;
        px[mid..mid + 4].copy_from_slice(&[255, 255, 255, 255]);
        let mut plain = px.clone();
        glow_under(&mut plain, 9, 9, 1.0, None);
        assert_eq!(&plain[mid..mid + 4], &[255, 255, 255, 255]);
        let side = (4 * 9 + 5) * 4;
        assert!(
            plain[side + 3] > 20,
            "halo beside: {:?}",
            &plain[side..side + 4]
        );
        assert_eq!(plain[3], 0, "nothing reaches the corner at 1σ ≈ 4 px");
        let red = Color::from_hex("#ff0000").unwrap();
        glow_under(&mut px, 9, 9, 1.0, Some(red));
        let halo = &px[side..side + 4];
        assert!(halo[2] > 20 && halo[0] == 0 && halo[1] == 0, "{halo:?}");
        assert_eq!(&px[mid..mid + 4], &[255, 255, 255, 255]);
    }
}
