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
//! The file is read and parsed once per source on the image worker (at
//! most [`MAX_LOTTIE_BYTES`]; inline where there is none), never on the
//! render thread: until it is in the node draws nothing and keeps no
//! clock, and its arrival repaints. Image layers draw their assets,
//! decoded on the worker with the file: a PNG or JPEG embedded as a
//! base64 `data:` URL or a file beside the animation (`u` + `p`, relative
//! to the file's directory), at the asset's authored size (each at most
//! [`MAX_ASSET_BYTES`]). An asset that cannot be read or decoded draws
//! nothing. The rest of velato's support holds.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use strand_scene::{Prop, PropValue, TimeContext};
use vello_cpu::color::PremulRgba8;
use vello_cpu::kurbo::{Affine, Shape};
use vello_cpu::peniko::{BlendMode, Brush};
use vello_cpu::peniko::{Extend, ImageQuality, ImageSampler};
use vello_cpu::{Image, ImageSource, PaintType, Pixmap, RenderContext};

use super::graph::number;
use crate::clock::Rate;
use crate::image::Fit;
use crate::offscreen::{RasterProps, RasterSource};

/// The largest Lottie file read.
pub const MAX_LOTTIE_BYTES: u64 = 8 << 20;

/// The largest image asset read or embedded.
pub const MAX_ASSET_BYTES: u64 = 16 << 20;

/// The fastest a Lottie clock runs.
const MAX_FPS: f64 = 120.0;

/// Path flattening tolerance, in the animation's units.
const TOLERANCE: f64 = 0.1;

/// velato's draw calls into a vello_cpu context.
struct Sink<'c> {
    ctx: &'c mut RenderContext,
    images: &'c HashMap<String, Arc<Pixmap>>,
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

    /// An image layer: its asset, decoded at its authored size, over the
    /// layer's box (velato clips it there).
    fn draw_image(&mut self, image: &velato::model::ImageAsset, transform: Affine, alpha: f64) {
        let Some(pm) = self.images.get(&image.id) else {
            return;
        };
        let alpha = if alpha.is_finite() {
            alpha.clamp(0.0, 1.0) as f32
        } else {
            0.0
        };
        self.ctx.set_transform(transform);
        self.ctx.set_paint(PaintType::Image(Image {
            image: ImageSource::Pixmap(pm.clone()),
            sampler: ImageSampler {
                x_extend: Extend::Pad,
                y_extend: Extend::Pad,
                quality: ImageQuality::Medium,
                alpha,
            },
        }));
        self.ctx.reset_paint_transform();
        self.ctx.fill_rect(&vello_cpu::kurbo::Rect::new(
            0.0,
            0.0,
            pm.width() as f64,
            pm.height() as f64,
        ));
    }
}

/// A read and parsed file: velato's model and its decoded image assets.
#[derive(Debug)]
struct File {
    comp: velato::Composition,
    images: HashMap<String, Arc<Pixmap>>,
}

/// Reads, parses and decodes `source` (on the image worker).
fn load(source: &str) -> Result<File, String> {
    let bytes = crate::image::read_local(source, MAX_LOTTIE_BYTES).map_err(|e| e.to_string())?;
    let comp = velato::Composition::from_slice(bytes).map_err(|e| e.to_string())?;
    let dir = local_dir(source);
    let images = comp
        .images
        .iter()
        .filter_map(|(id, asset)| Some((id.clone(), Arc::new(asset_pixels(asset, &dir)?))))
        .collect();
    Ok(File { comp, images })
}

/// The directory of a local `source` (as `read_local` reads it).
fn local_dir(source: &str) -> std::path::PathBuf {
    let path = source.strip_prefix("file://").unwrap_or(source);
    let path = match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_default()
            .join(rest),
        None => std::path::PathBuf::from(path),
    };
    path.parent().map(|p| p.to_path_buf()).unwrap_or_default()
}

/// An image asset's pixels at its authored size: embedded or beside the
/// file. `None` if it cannot be read or decoded.
fn asset_pixels(asset: &velato::model::ImageAsset, dir: &std::path::Path) -> Option<Pixmap> {
    let bytes = if asset.is_data_url() {
        let (_, data) = asset.file_name.split_once(";base64,")?;
        if data.len() as u64 > MAX_ASSET_BYTES * 4 / 3 + 4 {
            return None;
        }
        base64(data)?
    } else {
        let loc = asset.location();
        if loc.contains("://") && !loc.starts_with("file://") {
            return None;
        }
        let path = std::path::Path::new(loc.strip_prefix("file://").unwrap_or(&loc));
        let path = if path.is_relative() {
            dir.join(path)
        } else {
            path.to_path_buf()
        };
        crate::image::read_local(path.to_str()?, MAX_ASSET_BYTES).ok()?
    };
    let size = |v: Option<f64>| {
        v.filter(|v| v.is_finite() && *v >= 1.0)
            .map(|v| v.round().min(4096.0) as u32)
    };
    let want = size(asset.width).zip(size(asset.height));
    crate::image::decode_rgba(&bytes, want).ok()
}

/// Standard base64 (padding and whitespace allowed). `None` if it is not.
fn base64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in text.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b' ' | b'\n' | b'\r' | b'\t' => continue,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[derive(Default)]
struct Lottie {
    source: String,
    /// The parsed file, read on the image worker for a source.
    comp: Option<Result<Arc<File>, String>>,
    /// Its read is on the worker.
    waiting: bool,
    speed: f32,
    renderer: velato::Renderer,
}

impl std::fmt::Debug for Lottie {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lottie")
            .field("source", &self.source)
            .field("loaded", &matches!(self.comp, Some(Ok(_))))
            .field("waiting", &self.waiting)
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
        let Some(Ok(file)) = s.comp.clone() else {
            return;
        };
        let comp = &file.comp;
        let (cw, ch) = (comp.width as f64, comp.height as f64);
        if !(cw > 0.0 && ch > 0.0) {
            return;
        }
        let (ox, oy, dw, dh) = crate::image::placement(cw, ch, w as f64, h as f64, Fit::Contain);
        let transform = Affine::translate((ox, oy)) * Affine::scale_non_uniform(dw / cw, dh / ch);
        let frame = frame_at(comp, time.t, s.speed);
        let renderer = &mut s.renderer;
        super::vector(pixels, w, h, |ctx| {
            let mut sink = Sink {
                ctx,
                images: &file.images,
            };
            renderer.append(comp, frame, transform, 1.0, &mut sink);
        });
    }

    fn rate(&self) -> Rate {
        Rate::Refresh
    }

    /// The file's frame rate once it is read (one refresh frame asks for
    /// it); none while the worker reads it (its arrival repaints), for a
    /// file that could not be read, or with no source at all.
    fn clock(&self) -> Option<Rate> {
        let s = self.inner.lock().ok()?;
        match &s.comp {
            None if s.waiting => None,
            None => Some(Rate::Refresh),
            Some(Err(_)) => None,
            Some(Ok(f)) => {
                let c = &f.comp;
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
        if source != s.source {
            s.comp = None;
            s.waiting = false;
            s.source = source;
        }
        if s.comp.is_none() {
            // No source (a value not set yet) is no animation: no clock,
            // as for a file that could not be read.
            let got = if s.source.is_empty() {
                Some(Err("no source".to_string()))
            } else {
                let src = s.source.clone();
                props
                    .load_file(&format!("lottie:{}", s.source), move || {
                        load(&src).map(|f| Arc::new(f) as Arc<dyn std::any::Any + Send + Sync>)
                    })
                    .map(|r| {
                        r.and_then(|a| {
                            a.downcast::<File>()
                                .map_err(|_| "not a Lottie file".to_string())
                        })
                    })
            };
            s.waiting = got.is_none();
            s.comp = got;
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
