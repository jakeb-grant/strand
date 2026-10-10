//! (M4) `lottie "loader.json" { speed: 1 }` (design.md: "Lottie via
//! velato").
//!
//! A Lottie animation is a CPU raster node: velato's model of the file,
//! drawn by velato's renderer into our vello_cpu context (velato without
//! its vello GPU feature: the same kurbo and peniko as vello_cpu, so no
//! second copy). Its clock runs at the file's frame rate (capped like any
//! clock, stopped while hidden and under `reduced_motion`), the time of
//! its node times `speed` choosing the frame, looping over the file's
//! frames. It is drawn whole, `contain`ed in its box.
//!
//! The file is read and parsed once per source on the render thread
//! (at most [`MAX_LOTTIE_BYTES`]). Image layers draw nothing (embedded
//! raster assets are not decoded); the rest of velato's support holds.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use strand_scene::{Prop, PropValue, TimeContext};
use vello_cpu::RenderContext;
use vello_cpu::color::PremulRgba8;
use vello_cpu::kurbo::{Affine, Shape};
use vello_cpu::peniko::{BlendMode, Brush};

use super::graph::number;
use crate::clock::Rate;
use crate::image::Fit;
use crate::offscreen::{RasterProps, RasterSource};

/// The largest Lottie file read.
pub const MAX_LOTTIE_BYTES: u64 = 8 << 20;

/// The fastest a Lottie clock runs.
const MAX_FPS: f64 = 120.0;

/// Path flattening tolerance, in the animation's units.
const TOLERANCE: f64 = 0.1;

/// velato's draw calls into a vello_cpu context.
struct Sink<'c> {
    ctx: &'c mut RenderContext,
}

impl velato::RenderSink for Sink<'_> {
    fn push_layer(
        &mut self,
        blend: impl Into<BlendMode>,
        alpha: f32,
        transform: Affine,
        shape: &impl Shape,
    ) {
        self.ctx.set_transform(transform);
        let path = shape.to_path(TOLERANCE);
        self.ctx
            .push_layer(Some(&path), Some(blend.into()), Some(alpha), None, None);
    }

    fn push_clip_layer(&mut self, transform: Affine, shape: &impl Shape) {
        self.ctx.set_transform(transform);
        self.ctx.push_clip_layer(&shape.to_path(TOLERANCE));
    }

    fn pop_layer(&mut self) {
        self.ctx.pop_layer();
    }

    fn draw(
        &mut self,
        stroke: Option<&velato::model::fixed::Stroke>,
        transform: Affine,
        brush: &velato::model::fixed::Brush,
        shape: &impl Shape,
    ) {
        match brush {
            Brush::Solid(c) => self.ctx.set_paint(*c),
            Brush::Gradient(g) => self.ctx.set_paint(g.clone()),
            // Image brushes: not decoded (see the module docs).
            Brush::Image(_) => return,
        }
        self.ctx.set_transform(transform);
        let path = shape.to_path(TOLERANCE);
        match stroke {
            Some(s) => {
                self.ctx.set_stroke(s.clone());
                self.ctx.stroke_path(&path);
            }
            None => self.ctx.fill_path(&path),
        }
    }

    fn draw_image(&mut self, _image: &velato::model::ImageAsset, _transform: Affine, _alpha: f64) {}
}

#[derive(Default)]
struct Lottie {
    source: String,
    /// The parsed file, read on the first draw of a source.
    comp: Option<Result<Arc<velato::Composition>, String>>,
    speed: f32,
    renderer: velato::Renderer,
}

impl std::fmt::Debug for Lottie {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lottie")
            .field("source", &self.source)
            .field("loaded", &matches!(self.comp, Some(Ok(_))))
            .field("speed", &self.speed)
            .finish_non_exhaustive()
    }
}

/// The frame shown `t` seconds in at `speed`, looping over `frames`.
pub fn frame_at(comp: &velato::Composition, t: f32, speed: f32) -> f64 {
    let (a, b) = (comp.frames.start, comp.frames.end);
    let len = b - a;
    if len.is_nan() || len <= 0.0 || !comp.frame_rate.is_finite() {
        return a;
    }
    let f = (t as f64 * speed as f64 * comp.frame_rate).rem_euclid(len);
    // Whole frames: a capped clock shows the file's frames, not between.
    a + f.floor().min(len - 1.0).max(0.0)
}

/// A `lottie` node's source.
#[derive(Debug, Default)]
pub struct LottieSource {
    inner: Mutex<Lottie>,
}

impl RasterSource for LottieSource {
    fn draw(&self, pixels: &mut [PremulRgba8], w: u32, h: u32, _scale: f32, time: TimeContext) {
        let Ok(mut s) = self.inner.lock() else {
            return;
        };
        let Some(Ok(comp)) = s.comp.clone() else {
            return;
        };
        let (cw, ch) = (comp.width as f64, comp.height as f64);
        if !(cw > 0.0 && ch > 0.0) {
            return;
        }
        let (ox, oy, dw, dh) = crate::image::placement(cw, ch, w as f64, h as f64, Fit::Contain);
        let transform = Affine::translate((ox, oy)) * Affine::scale_non_uniform(dw / cw, dh / ch);
        let frame = frame_at(&comp, time.t, s.speed);
        let renderer = &mut s.renderer;
        super::vector(pixels, w, h, |ctx| {
            renderer.append(&comp, frame, transform, 1.0, &mut Sink { ctx });
        });
    }

    fn rate(&self) -> Rate {
        Rate::Refresh
    }

    /// The file's frame rate once it is read (one refresh frame reads
    /// it); none for a file that could not be.
    fn clock(&self) -> Option<Rate> {
        let s = self.inner.lock().ok()?;
        match &s.comp {
            None => Some(Rate::Refresh),
            Some(Err(_)) => None,
            Some(Ok(c)) => {
                let fps = (c.frame_rate * s.speed.abs() as f64).clamp(1.0, MAX_FPS);
                if s.speed == 0.0 || c.frames.end <= c.frames.start {
                    return None;
                }
                Some(Rate::Every(Duration::from_secs_f64(1.0 / fps)))
            }
        }
    }

    fn state(&self, props: &RasterProps<'_>) -> u64 {
        let Ok(mut s) = self.inner.lock() else {
            return 0;
        };
        let source = match (props.get)(Prop::Source) {
            Some(PropValue::Text(t) | PropValue::Keyword(t)) => t.trim().to_string(),
            _ => String::new(),
        };
        if source != s.source || s.comp.is_none() {
            s.comp = (!source.is_empty()).then(|| {
                crate::image::read_local(&source, MAX_LOTTIE_BYTES)
                    .map_err(|e| e.to_string())
                    .and_then(|b| velato::Composition::from_slice(b).map_err(|e| e.to_string()))
                    .map(Arc::new)
            });
            s.source = source;
        }
        s.speed = number((props.get)(Prop::Speed))
            .filter(|v| v.is_finite())
            .unwrap_or(1.0)
            .clamp(-16.0, 16.0);
        let mut h = DefaultHasher::new();
        s.source.hash(&mut h);
        s.speed.to_bits().hash(&mut h);
        matches!(s.comp, Some(Ok(_))).hash(&mut h);
        h.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_loop_at_the_files_rate_and_speed() {
        let comp = velato::Composition {
            frames: 0.0..30.0,
            frame_rate: 30.0,
            width: 10,
            height: 10,
            ..Default::default()
        };
        assert_eq!(frame_at(&comp, 0.0, 1.0), 0.0);
        assert_eq!(frame_at(&comp, 0.5, 1.0), 15.0);
        assert_eq!(frame_at(&comp, 1.25, 1.0), 7.0, "loops");
        assert_eq!(frame_at(&comp, 0.25, 2.0), 15.0);
        assert_eq!(frame_at(&comp, 0.25, -1.0), 22.0, "backwards");
        let empty = velato::Composition::default();
        assert_eq!(frame_at(&empty, 3.0, 1.0), 0.0);
    }
}
