//! (M4) `thumbnail w` (design.md: "Live window thumbnails … via
//! ext-image-copy-capture").
//!
//! A thumbnail is a CPU raster node with no clock: its pixels are the
//! last frame of its window, captured off the main thread and fed to it
//! through the binary ([`crate::Renderer::feed_frame`]) only while it is
//! visible (render asks for them with a [`crate::FeedKind::Thumbnail`]
//! demand). Each frame repaints it once; a still window costs nothing.
//! The frame is drawn by its `fit` (`contain` by default, `cover` cropped
//! to the box), sampled bilinearly. Before its first frame, and after its
//! window is gone (fed `None`), it draws nothing. Its frame is of the
//! window its source named when it came: once the source names another
//! window, the frame is dropped, so it never shows one window's pixels
//! for another while the new window's first frame is on its way (or
//! never comes).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use strand_scene::{Prop, PropValue, TimeContext};
use vello_cpu::color::PremulRgba8;

use crate::clock::Rate;
use crate::image::Fit;
use crate::offscreen::{RasterContent, RasterProps, RasterSource};

/// A captured frame as a thumbnail takes it: premultiplied BGRA rows,
/// `width × height`, packed (what `wm::capture` delivers).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub pixels: Arc<[u8]>,
}

#[derive(Debug, Default)]
struct Thumb {
    frame: Option<Frame>,
    /// The window its source named at the last paint: the window the
    /// frame is of.
    window: String,
    /// Frames fed so far: the state hash, so each frame draws once.
    fed: u64,
    fit: Fit,
}

/// A `thumbnail` node's source.
#[derive(Debug, Default)]
pub struct ThumbnailSource {
    inner: Mutex<Thumb>,
}

impl ThumbnailSource {
    /// A new frame (`None`: the window is gone, draw nothing). A frame
    /// whose buffer is short for its size is dropped.
    pub fn feed(&self, frame: Option<Frame>) {
        let frame = frame.filter(|f| f.pixels.len() as u64 >= f.width as u64 * f.height as u64 * 4);
        if let Ok(mut s) = self.inner.lock() {
            s.frame = frame;
            s.fed += 1;
        }
    }

    /// Whether it holds a frame (tests).
    #[cfg(test)]
    fn has_frame(&self) -> bool {
        self.inner.lock().is_ok_and(|s| s.frame.is_some())
    }
}

/// The frame's pixel at byte offset `i` as premultiplied RGBA floats.
fn texel(p: &[u8], i: usize) -> [f32; 4] {
    [
        p[i + 2] as f32,
        p[i + 1] as f32,
        p[i] as f32,
        p[i + 3] as f32,
    ]
}

/// One column's or row's sample: the byte offsets of its two texels
/// (clamped to the frame) along that axis, the weight of the second, and
/// how much of the pixel the placement covers.
struct Tap {
    near: usize,
    far: usize,
    t: f32,
    cover: f64,
}

/// The taps of pixels `from..to` along an axis placed at `o`, `d` long,
/// `k` frame texels to a pixel, over `n` texels `step` bytes apart.
fn taps(from: u32, to: u32, o: f64, d: f64, k: f64, n: u32, step: usize) -> Vec<Tap> {
    let at = |i: i64| i.clamp(0, n as i64 - 1) as usize * step;
    (from..to)
        .map(|p| {
            let s = (p as f64 + 0.5 - o) * k - 0.5;
            let i = s.floor() as i64;
            Tap {
                near: at(i),
                far: at(i + 1),
                t: (s - s.floor()) as f32,
                // Coverage of the edge pixels of a fractional placement.
                cover: ((p as f64 + 1.0).min(o + d) - (p as f64).max(o)).clamp(0.0, 1.0),
            }
        })
        .collect()
}

/// A premultiplied pixel scaled by its coverage `k`; its alpha is never
/// below a channel.
fn covered(px: [u8; 4], k: f32) -> PremulRgba8 {
    let px = px.map(|v| (v as f32 * k).round() as u8);
    PremulRgba8 {
        r: px[0],
        g: px[1],
        b: px[2],
        a: px[3].max(px[0]).max(px[1]).max(px[2]),
    }
}

/// Draws `f` into the `w × h` buffer by `fit`, bilinearly.
pub(crate) fn draw_frame(pixels: &mut [PremulRgba8], w: u32, h: u32, f: &Frame, fit: Fit) {
    draw_frame_in(pixels, w, h, f, fit, false);
}

/// [`draw_frame`]; with `bgra`, red and blue swapped (the raster's
/// order, each pixel's `r` holding its blue), as if drawn and then
/// swapped.
fn draw_frame_in(pixels: &mut [PremulRgba8], w: u32, h: u32, f: &Frame, fit: Fit, bgra: bool) {
    let stride = f.width as usize * 4;
    if f.width == 0
        || f.height == 0
        || w == 0
        || h == 0
        || f.pixels.len() < stride * f.height as usize
    {
        return;
    }
    let (ox, oy, dw, dh) =
        crate::image::placement(f.width as f64, f.height as f64, w as f64, h as f64, fit);
    if !(dw > 0.0 && dh > 0.0) {
        return;
    }
    let (kx, ky) = (f.width as f64 / dw, f.height as f64 / dh);
    let x0 = ox.max(0.0).floor() as u32;
    let y0 = oy.max(0.0).floor() as u32;
    let x1 = ((ox + dw).ceil().max(0.0) as u32).min(w);
    let y1 = ((oy + dh).ceil().max(0.0) as u32).min(h);
    // (lock-4k) A frame placed at its own size on whole pixels (a screen's
    // thumbnail on that screen, as a lock's stills are) is copied: sampled
    // there, each pixel is the frame's own, wholly covered, so the copy
    // draws the same. Sampling a 3840 × 2160 still took about 200 ms of
    // the main thread on the owner's laptop.
    if kx == 1.0 && ky == 1.0 && ox.fract() == 0.0 && oy.fract() == 0.0 {
        let (sx0, sy0) = ((x0 as f64 - ox) as usize, (y0 as f64 - oy) as usize);
        let n = (x1 - x0) as usize;
        for (row, y) in (y0..y1).enumerate() {
            let from = (sy0 + row) * stride + sx0 * 4;
            let src = f.pixels.get(from..from + n * 4).unwrap_or_default();
            let at = (y * w + x0) as usize;
            let dst = pixels.get_mut(at..at + n).unwrap_or_default();
            // (blur-ready) In the raster's order a captured pixel is its
            // own bytes, alpha raised to its largest channel.
            let (r, b) = if bgra { (0, 2) } else { (2, 0) };
            for (out, p) in dst.iter_mut().zip(src.chunks_exact(4)) {
                *out = PremulRgba8 {
                    r: p[r],
                    g: p[1],
                    b: p[b],
                    a: p[3].max(p[0]).max(p[1]).max(p[2]),
                };
            }
        }
        return;
    }
    // Each column's taps once, each row's as it comes.
    let cols = taps(x0, x1, ox, dw, kx, f.width, 4);
    for (y, row) in (y0..y1).zip(taps(y0, y1, oy, dh, ky, f.height, stride)) {
        let line = (y * w) as usize;
        for (x, col) in (x0..x1).zip(&cols) {
            let a = texel(&f.pixels, row.near + col.near);
            let b = texel(&f.pixels, row.near + col.far);
            let c = texel(&f.pixels, row.far + col.near);
            let d = texel(&f.pixels, row.far + col.far);
            let mut px = [0u8; 4];
            for k in 0..4 {
                let top = a[k] + (b[k] - a[k]) * col.t;
                let bottom = c[k] + (d[k] - c[k]) * col.t;
                px[k] = (top + (bottom - top) * row.t).round().clamp(0.0, 255.0) as u8;
            }
            if let Some(out) = pixels.get_mut(line + x as usize) {
                let mut p = covered(px, (col.cover * row.cover) as f32);
                if bgra {
                    std::mem::swap(&mut p.r, &mut p.b);
                }
                *out = p;
            }
        }
    }
}

impl RasterSource for ThumbnailSource {
    fn draw(&self, pixels: &mut [PremulRgba8], w: u32, h: u32, _scale: f32, _time: TimeContext) {
        let Ok(s) = self.inner.lock() else {
            return;
        };
        if let Some(f) = &s.frame {
            draw_frame(pixels, w, h, f, s.fit);
        }
    }

    /// (blur-ready) Drawn in the raster's order directly: no pass to swap
    /// red and blue (and none over a blank pixmap, before a frame).
    fn draw_bgra(
        &self,
        pixels: &mut [PremulRgba8],
        w: u32,
        h: u32,
        _scale: f32,
        _time: TimeContext,
    ) {
        let Ok(s) = self.inner.lock() else {
            return;
        };
        if let Some(f) = &s.frame {
            draw_frame_in(pixels, w, h, f, s.fit, true);
        }
    }

    /// (blur-ready) Its frame (by the address of its pixels, pinned) and
    /// fit: thumbnails fed one frame (a lock's sharp still and its
    /// blurred copy) share its pixmap; with none, the blank one.
    fn content(&self) -> Option<RasterContent> {
        let s = self.inner.lock().ok()?;
        let mut h = DefaultHasher::new();
        let pin = s.frame.as_ref().map(|f| {
            (
                "frame",
                Arc::as_ptr(&f.pixels) as *const u8 as usize,
                f.pixels.len(),
                f.width,
                f.height,
                s.fit,
            )
                .hash(&mut h);
            Arc::downgrade(&f.pixels)
        });
        if pin.is_none() {
            "blank".hash(&mut h);
        }
        Some(RasterContent {
            key: h.finish(),
            pin,
        })
    }

    fn rate(&self) -> Rate {
        Rate::Refresh
    }

    /// None: it repaints when fed.
    fn clock(&self) -> Option<Rate> {
        None
    }

    fn state(&self, props: &RasterProps<'_>) -> u64 {
        let Ok(mut s) = self.inner.lock() else {
            return 0;
        };
        s.fit = match (props.get)(Prop::Fit) {
            Some(PropValue::Keyword(k)) => Fit::from_name(k).unwrap_or_default(),
            _ => Fit::default(),
        };
        let window = match (props.get)(Prop::Source) {
            Some(PropValue::Text(t) | PropValue::Keyword(t)) => t.as_str(),
            _ => "",
        };
        if s.window != window {
            s.window = window.to_string();
            if s.frame.take().is_some() {
                s.fed += 1;
            }
        }
        let mut h = DefaultHasher::new();
        s.fed.hash(&mut h);
        s.fit.hash(&mut h);
        h.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: u32, h: u32, bgra: [u8; 4]) -> Frame {
        Frame {
            width: w,
            height: h,
            pixels: bgra.repeat((w * h) as usize).into(),
        }
    }

    #[test]
    fn frames_draw_by_fit() {
        let f = frame(4, 2, [255, 0, 0, 255]); // blue
        let mut px = vec![
            PremulRgba8 {
                r: 0,
                g: 0,
                b: 0,
                a: 0
            };
            16
        ];
        draw_frame(&mut px, 4, 4, &f, Fit::Contain);
        // Contained: rows 1 and 2, blue; rows 0 and 3 empty.
        assert_eq!(px[0].a, 0);
        assert_eq!(
            (px[4].r, px[4].g, px[4].b, px[4].a),
            (0, 0, 255, 255),
            "{:?}",
            px[4]
        );
        assert_eq!(px[12].a, 0);
        let mut px = vec![
            PremulRgba8 {
                r: 0,
                g: 0,
                b: 0,
                a: 0
            };
            16
        ];
        draw_frame(&mut px, 4, 4, &f, Fit::Cover);
        assert!(px.iter().all(|p| (p.b, p.a) == (255, 255)), "covered");
    }

    /// The draw as it was before lock-4k, per pixel: what [`draw_frame`]
    /// must still draw, byte for byte.
    fn sampled(pixels: &mut [PremulRgba8], w: u32, h: u32, f: &Frame, fit: Fit) {
        let texel = |x: i64, y: i64| {
            let x = x.clamp(0, f.width as i64 - 1) as usize;
            let y = y.clamp(0, f.height as i64 - 1) as usize;
            let p = &f.pixels[(y * f.width as usize + x) * 4..];
            [p[2] as f32, p[1] as f32, p[0] as f32, p[3] as f32]
        };
        let (ox, oy, dw, dh) =
            crate::image::placement(f.width as f64, f.height as f64, w as f64, h as f64, fit);
        let (kx, ky) = (f.width as f64 / dw, f.height as f64 / dh);
        let x1 = ((ox + dw).ceil().max(0.0) as u32).min(w);
        let y1 = ((oy + dh).ceil().max(0.0) as u32).min(h);
        for y in oy.max(0.0).floor() as u32..y1 {
            let sy = (y as f64 + 0.5 - oy) * ky - 0.5;
            let (fy, ty) = (sy.floor(), (sy - sy.floor()) as f32);
            for x in ox.max(0.0).floor() as u32..x1 {
                let sx = (x as f64 + 0.5 - ox) * kx - 0.5;
                let (fx, tx) = (sx.floor(), (sx - sx.floor()) as f32);
                let (ix, iy) = (fx as i64, fy as i64);
                let (a, b) = (texel(ix, iy), texel(ix + 1, iy));
                let (c, d) = (texel(ix, iy + 1), texel(ix + 1, iy + 1));
                let mut px = [0u8; 4];
                for k in 0..4 {
                    let top = a[k] + (b[k] - a[k]) * tx;
                    let bottom = c[k] + (d[k] - c[k]) * tx;
                    px[k] = (top + (bottom - top) * ty).round().clamp(0.0, 255.0) as u8;
                }
                let cover = ((x as f64 + 1.0).min(ox + dw) - (x as f64).max(ox)).clamp(0.0, 1.0)
                    * ((y as f64 + 1.0).min(oy + dh) - (y as f64).max(oy)).clamp(0.0, 1.0);
                pixels[(y * w + x) as usize] = covered(px, cover as f32);
            }
        }
    }

    /// A frame of varied premultiplied pixels (some with alpha below a
    /// channel, as a compositor can send).
    fn noisy(w: u32, h: u32) -> Frame {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let pixels = (0..w * h)
            .flat_map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let [b, g, r, a, ..] = seed.to_le_bytes();
                [b, g, r, if a < 16 { a } else { a.max(b).max(g).max(r) }]
            })
            .collect::<Vec<u8>>();
        Frame {
            width: w,
            height: h,
            pixels: pixels.into(),
        }
    }

    /// (lock-4k) The draw is the same, byte for byte, as the per-pixel one
    /// it replaced: copied where a frame is placed at its own size on
    /// whole pixels (a screen's thumbnail on that screen, cropped by
    /// `cover` or not), sampled from taps computed once per column and row
    /// everywhere else (scaled up and down, letterboxed, placed on
    /// fractions of a pixel).
    #[test]
    fn the_draw_is_the_per_pixel_draw_it_replaced() {
        let blank = PremulRgba8 {
            r: 0,
            g: 0,
            b: 0,
            a: 0,
        };
        let cases = [
            // (frame, box, fit): its own size, copied
            ((64, 36), (64, 36), Fit::Cover),
            ((64, 36), (64, 36), Fit::Contain),
            // a frame taller than its screen by whole pixels (Hyprland's
            // nested 3840 × 2176 for 3840 × 2160), cropped by cover
            ((64, 40), (64, 36), Fit::Cover),
            ((70, 36), (64, 36), Fit::Cover),
            // `fit: none` at its own size: padded on both axes, cropped on
            // one and padded on the other (copied), and centred on half a
            // pixel (sampled); `fill` at its own size (copied)
            ((60, 30), (64, 36), Fit::None),
            ((70, 30), (64, 36), Fit::None),
            ((61, 36), (64, 36), Fit::None),
            ((64, 36), (64, 36), Fit::Fill),
            // scaled, letterboxed, on fractions
            ((64, 36), (96, 54), Fit::Cover),
            ((64, 36), (40, 40), Fit::Contain),
            ((64, 36), (41, 23), Fit::Cover),
            ((33, 17), (100, 61), Fit::Contain),
            ((5, 3), (7, 7), Fit::Cover),
            ((1, 1), (9, 4), Fit::Contain),
        ];
        for ((fw, fh), (w, h), fit) in cases {
            let f = noisy(fw, fh);
            let mut want = vec![blank; (w * h) as usize];
            let mut got = want.clone();
            sampled(&mut want, w, h, &f, fit);
            draw_frame(&mut got, w, h, &f, fit);
            let first = want.iter().zip(&got).position(|(a, b)| a != b);
            assert_eq!(
                first.map(|i| (i % w as usize, i / w as usize, want[i], got[i])),
                None,
                "{fw}x{fh} into {w}x{h} by {fit:?}"
            );
        }
    }

    /// (blur-ready) Drawn in the raster's order, a frame is what the RGBA
    /// draw swapped gives, byte for byte: copied at its own size (cropped
    /// or padded), sampled scaled, letterboxed and on fractions of a
    /// pixel, with pixels whose alpha is below a channel.
    #[test]
    fn bgra_draw_is_the_rgba_draw_swapped() {
        let blank = PremulRgba8 {
            r: 0,
            g: 0,
            b: 0,
            a: 0,
        };
        let cases = [
            ((64, 36), (64, 36), Fit::Cover),
            ((64, 36), (64, 36), Fit::Contain),
            ((64, 40), (64, 36), Fit::Cover),
            ((60, 30), (64, 36), Fit::None),
            ((61, 36), (64, 36), Fit::None),
            ((64, 36), (64, 36), Fit::Fill),
            ((64, 36), (96, 54), Fit::Cover),
            ((64, 36), (40, 40), Fit::Contain),
            ((64, 36), (41, 23), Fit::Cover),
            ((33, 17), (100, 61), Fit::Fill),
            ((1, 1), (9, 4), Fit::Contain),
        ];
        for ((fw, fh), (w, h), fit) in cases {
            let f = noisy(fw, fh);
            let mut want = vec![blank; (w * h) as usize];
            draw_frame(&mut want, w, h, &f, fit);
            for p in &mut want {
                std::mem::swap(&mut p.r, &mut p.b);
            }
            let mut got = vec![blank; (w * h) as usize];
            draw_frame_in(&mut got, w, h, &f, fit, true);
            assert!(want == got, "{fw}x{fh} into {w}x{h} by {fit:?}");
        }
    }

    /// (lock-4k) A short buffer or a short frame draws what fits and never
    /// panics: the frame comes from the compositor.
    #[test]
    fn a_short_buffer_or_frame_never_panics() {
        let f = noisy(8, 8);
        let mut px = vec![
            PremulRgba8 {
                r: 0,
                g: 0,
                b: 0,
                a: 0
            };
            10
        ];
        draw_frame(&mut px, 8, 8, &f, Fit::Cover);
        draw_frame(&mut px, 8, 8, &f, Fit::Contain);
        let short = Frame {
            width: 8,
            height: 8,
            pixels: f.pixels[..100].into(),
        };
        let mut px = vec![px[0]; 64];
        draw_frame(&mut px, 8, 8, &short, Fit::Cover);
    }

    #[test]
    fn short_frames_are_dropped_and_each_feed_changes_the_state() {
        let s = ThumbnailSource::default();
        s.feed(Some(Frame {
            width: 10,
            height: 10,
            pixels: vec![0; 12].into(),
        }));
        assert!(!s.has_frame());
        let get = |_: Prop| None;
        let props = RasterProps {
            get: &get,
            color: strand_scene::Color::WHITE,
            parts: &[],
            files: None,
            boot: std::time::Duration::ZERO,
            slack: std::time::Duration::ZERO,
            still: false,
            commit: true,
        };
        let a = s.state(&props);
        s.feed(Some(frame(2, 2, [0, 0, 0, 255])));
        assert!(s.has_frame());
        assert_ne!(s.state(&props), a);
        assert_eq!(s.clock(), None);
    }
}
