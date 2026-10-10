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
use crate::offscreen::{RasterProps, RasterSource};

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

/// `f`'s pixel at `(x, y)` (clamped) as premultiplied RGBA floats.
fn texel(f: &Frame, x: i64, y: i64) -> [f32; 4] {
    let x = x.clamp(0, f.width as i64 - 1) as usize;
    let y = y.clamp(0, f.height as i64 - 1) as usize;
    let i = (y * f.width as usize + x) * 4;
    match f.pixels.get(i..i + 4) {
        Some(p) => [p[2] as f32, p[1] as f32, p[0] as f32, p[3] as f32],
        None => [0.0; 4],
    }
}

/// Draws `f` into the `w × h` buffer by `fit`, bilinearly.
pub(crate) fn draw_frame(pixels: &mut [PremulRgba8], w: u32, h: u32, f: &Frame, fit: Fit) {
    if f.width == 0 || f.height == 0 || w == 0 || h == 0 {
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
    for y in y0..y1 {
        let sy = (y as f64 + 0.5 - oy) * ky - 0.5;
        let (fy, ty) = (sy.floor(), (sy - sy.floor()) as f32);
        for x in x0..x1 {
            let sx = (x as f64 + 0.5 - ox) * kx - 0.5;
            let (fx, tx) = (sx.floor(), (sx - sx.floor()) as f32);
            let (ix, iy) = (fx as i64, fy as i64);
            let a = texel(f, ix, iy);
            let b = texel(f, ix + 1, iy);
            let c = texel(f, ix, iy + 1);
            let d = texel(f, ix + 1, iy + 1);
            let mut px = [0u8; 4];
            for k in 0..4 {
                let top = a[k] + (b[k] - a[k]) * tx;
                let bottom = c[k] + (d[k] - c[k]) * tx;
                px[k] = (top + (bottom - top) * ty).round().clamp(0.0, 255.0) as u8;
            }
            // Coverage of the edge pixels of a fractional placement.
            let cover = ((x as f64 + 1.0).min(ox + dw) - (x as f64).max(ox)).clamp(0.0, 1.0)
                * ((y as f64 + 1.0).min(oy + dh) - (y as f64).max(oy)).clamp(0.0, 1.0);
            if let Some(out) = pixels.get_mut((y * w + x) as usize) {
                let k = cover as f32;
                let px = px.map(|v| (v as f32 * k).round() as u8);
                *out = PremulRgba8 {
                    r: px[0],
                    g: px[1],
                    b: px[2],
                    a: px[3].max(px[0]).max(px[1]).max(px[2]),
                };
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
        };
        let a = s.state(&props);
        s.feed(Some(frame(2, 2, [0, 0, 0, 255])));
        assert!(s.has_frame());
        assert_ne!(s.state(&props), a);
        assert_eq!(s.clock(), None);
    }
}
