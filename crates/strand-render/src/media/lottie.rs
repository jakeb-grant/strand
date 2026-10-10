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
//! to the file's directory), each read at most [`MAX_ASSET_BYTES`]. Each
//! is decoded at its authored size, but no side over [`MAX_ASSET_SIDE`]
//! and all of a file's assets together within [`MAX_ASSET_PIXELS`] (all
//! shrunk alike to fit, then in id order to what is left), and drawn
//! scaled to its
//! authored box: a downloaded file cannot hold more than 64 MiB of
//! pixels (an image's own decode bound, [`crate::image::MAX_DECODE_BYTES`])
//! however large or many its assets say they are. These bound hostile
//! files, not the memory budget (design.md: budgets are test targets):
//! an asset a shell would show is decoded at its authored size. An asset
//! that cannot be read or decoded, or that no pixels are left for, draws
//! nothing. The rest of
//! velato's support holds.
//!
//! velato draws a file's structure as it stands (m4-audit): a precomp
//! that instances itself, or a matte that is its own, recursed until the
//! stack overflowed (an abort no `catch_unwind` stops), precomps that
//! fan out cost 2^N layers, and a repeater's copy count is unbounded
//! (`1e18` cloned geometry until memory ran out). A file is refused at
//! load unless its precomps and mattes form no reference cycle (a
//! precomp or matte that refers back to itself, directly or through
//! others), nest at most [`MAX_NESTING`] deep, and one frame draws at
//! most [`MAX_WORK`] layer and shape instances, each precomp instance and
//! repeater copy counted at the most its keyframes (and their easing) can
//! reach. Playback looping and time remapping (`tm`) are not reference
//! cycles and are unaffected: the check reads only what refers to what.

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

/// The longest side an image asset is decoded at: an image's largest
/// drawn side.
pub const MAX_ASSET_SIDE: u32 = 4096;

/// The most pixels a file's image assets hold together (64 MiB of RGBA,
/// an image decode's peak): a bound on a hostile file, not a budget.
pub const MAX_ASSET_PIXELS: u64 = (crate::image::MAX_DECODE_BYTES / 4) as u64;

/// The most layer and shape instances one frame of a file may draw.
pub const MAX_WORK: u64 = 100_000;

/// The deepest chain of precomp instances and mattes.
pub const MAX_NESTING: usize = 32;

/// The fastest a Lottie clock runs.
const MAX_FPS: f64 = 120.0;

/// Path flattening tolerance, in the animation's units.
const TOLERANCE: f64 = 0.1;

/// velato's draw calls into a vello_cpu context.
struct Sink<'c> {
    ctx: &'c mut RenderContext,
    images: &'c HashMap<String, Asset>,
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

    /// An image layer: its asset, scaled from its decoded size to its
    /// authored box (velato clips it there).
    fn draw_image(&mut self, image: &velato::model::ImageAsset, transform: Affine, alpha: f64) {
        let Some(Asset { pixmap: pm, w, h }) = self.images.get(&image.id) else {
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
        self.ctx.set_paint_transform(Affine::scale_non_uniform(
            w / pm.width() as f64,
            h / pm.height() as f64,
        ));
        self.ctx
            .fill_rect(&vello_cpu::kurbo::Rect::new(0.0, 0.0, *w, *h));
        self.ctx.reset_paint_transform();
    }
}

/// A read and parsed file: velato's model and its decoded image assets.
#[derive(Debug)]
struct File {
    comp: velato::Composition,
    images: HashMap<String, Asset>,
}

/// A decoded image asset and the box it is drawn over (its authored
/// size, in the animation's units).
#[derive(Debug)]
struct Asset {
    pixmap: Arc<Pixmap>,
    w: f64,
    h: f64,
}

/// Reads, parses and decodes `source` (on the image worker).
fn load(source: &str) -> Result<File, String> {
    let bytes = crate::image::read_local(source, MAX_LOTTIE_BYTES).map_err(|e| e.to_string())?;
    let comp = velato::Composition::from_slice(bytes).map_err(|e| e.to_string())?;
    check_work(&comp)?;
    let dir = local_dir(source);
    let mut ids: Vec<&String> = comp.images.keys().collect();
    ids.sort();
    // Their authored sizes within the side cap, shrunk alike to fit.
    let claimed: f64 = comp
        .images
        .values()
        .filter_map(capped)
        .map(|(w, h)| w as f64 * h as f64)
        .sum();
    let k = (MAX_ASSET_PIXELS as f64 / claimed.max(1.0)).sqrt().min(1.0);
    let mut left = MAX_ASSET_PIXELS;
    let mut images = HashMap::new();
    for id in ids {
        let Some(asset) = comp.images.get(id) else {
            continue;
        };
        if left == 0 {
            break;
        }
        if let Some(a) = asset_pixels(asset, &dir, k, left) {
            left = left.saturating_sub(u64::from(a.pixmap.width()) * u64::from(a.pixmap.height()));
            images.insert(id.clone(), a);
        }
    }
    Ok(File { comp, images })
}

/// A layer: in the top-level list (`None`) or a precomp's, by index.
type LayerAt<'a> = (Option<&'a str>, usize);

/// Refuses a file velato could not draw within bounds (see the module
/// docs): a reference cycle, nesting past [`MAX_NESTING`], or past
/// [`MAX_WORK`].
fn check_work(comp: &velato::Composition) -> Result<(), String> {
    let mut walk = Walk {
        comp,
        done: HashMap::new(),
        stack: Vec::new(),
    };
    let mut total = 0u64;
    for i in 0..comp.layers.len() {
        let c = walk.layer((None, i))?;
        total = add(total, c)?;
    }
    Ok(())
}

struct Walk<'a> {
    comp: &'a velato::Composition,
    /// Each layer's cost, once known.
    done: HashMap<LayerAt<'a>, u64>,
    /// The layers being costed, outermost first: one met again is a
    /// reference cycle.
    stack: Vec<LayerAt<'a>>,
}

/// `a + b`, refused past [`MAX_WORK`].
fn add(a: u64, b: u64) -> Result<u64, String> {
    let sum = a.saturating_add(b);
    if sum > MAX_WORK {
        return Err(format!(
            "a frame would draw over {MAX_WORK} layers and shapes"
        ));
    }
    Ok(sum)
}

impl<'a> Walk<'a> {
    /// What drawing layer `at` costs: itself, its matte and its content.
    fn layer(&mut self, at: LayerAt<'a>) -> Result<u64, String> {
        use velato::model::Content;
        if let Some(c) = self.done.get(&at) {
            return Ok(*c);
        }
        if self.stack.contains(&at) {
            return Err("a precomp or matte refers back to itself (a reference cycle)".into());
        }
        if self.stack.len() >= MAX_NESTING {
            return Err(format!("precomps or mattes nest deeper than {MAX_NESTING}"));
        }
        let comp = self.comp;
        let set = match at.0 {
            None => Some(&comp.layers),
            Some(name) => comp.assets.get(name),
        };
        let Some(layer) = set.and_then(|s| s.get(at.1)) else {
            return Ok(0);
        };
        self.stack.push(at);
        let mut cost = 1;
        if let Some((_, m)) = layer.mask_layer {
            let c = self.layer((at.0, m))?;
            cost = add(cost, c)?;
        }
        match &layer.content {
            Content::Instance { name, .. } => {
                if let Some((name, layers)) = comp.assets.get_key_value(name) {
                    for i in 0..layers.len() {
                        let c = self.layer((Some(name.as_str()), i))?;
                        cost = add(cost, c)?;
                    }
                }
            }
            Content::Shape(shapes) => cost = add(cost, Self::shapes(shapes)?)?,
            Content::Image { .. } | Content::None => {}
        }
        self.stack.pop();
        self.done.insert(at, cost);
        Ok(cost)
    }

    /// What a shape list costs: a repeater multiplies what comes before
    /// it in its list by its copies (velato repeats those geometries).
    fn shapes(shapes: &[velato::model::Shape]) -> Result<u64, String> {
        use velato::model::Shape;
        let mut cost = 0u64;
        for shape in shapes {
            cost = match shape {
                Shape::Group(children, _) => add(cost, Self::shapes(children)?)?,
                Shape::Repeater(r) => {
                    let copies = match r {
                        velato::model::Repeater::Fixed(f) => f.copies as u64,
                        velato::model::Repeater::Animated(a) => {
                            most(&a.copies).ok_or("a repeater's copies are not finite")?
                        }
                    };
                    let n = cost.saturating_mul(copies.max(1));
                    add(n, 0)?
                }
                Shape::Geometry(_) | Shape::Draw(_) | Shape::Trim(_) => add(cost, 1)?,
            };
        }
        Ok(cost)
    }
}

/// The largest whole count `v` reaches (velato rounds it), `None` when
/// unbounded. An animated value between two keyframes stays within its
/// easing curve's hull: `a + (b - a) · y` for `y` between 0, 1 and the
/// handles' heights.
fn most(v: &velato::model::Value<f64>) -> Option<u64> {
    use velato::model::Value;
    let count = |x: f64| x.is_finite().then(|| x.round().max(0.0) as u64);
    match v {
        Value::Fixed(x) => count(*x),
        Value::Animated(a) => {
            let mut top = 0u64;
            for (i, &from) in a.values.iter().enumerate() {
                let to = a.values.get(i + 1).copied().unwrap_or(from);
                let mut lo = 0.0f64;
                let mut hi = 1.0f64;
                for t in [a.times.get(i), a.times.get(i + 1)].into_iter().flatten() {
                    for h in [t.in_tangent, t.out_tangent].into_iter().flatten() {
                        lo = lo.min(h.y);
                        hi = hi.max(h.y);
                    }
                }
                for y in [lo, hi] {
                    top = top.max(count(from + (to - from) * y)?);
                }
            }
            Some(top)
        }
    }
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

/// An asset's authored size, finite and at least 1 on each side.
fn authored(asset: &velato::model::ImageAsset) -> Option<(f64, f64)> {
    let size = |v: Option<f64>| v.filter(|v| v.is_finite() && *v >= 1.0 && *v <= 1e6);
    size(asset.width).zip(size(asset.height))
}

/// An asset's authored size within [`MAX_ASSET_SIDE`], aspect kept.
fn capped(asset: &velato::model::ImageAsset) -> Option<(u32, u32)> {
    let (w, h) = authored(asset)?;
    let k = (MAX_ASSET_SIDE as f64 / w.max(h)).min(1.0);
    let side = |v: f64| ((v * k).round() as u32).clamp(1, MAX_ASSET_SIDE);
    Some((side(w), side(h)))
}

/// An image asset's pixels, embedded or beside the file, at its
/// [`capped`] size times `k`, within `left` pixels. `None` if it cannot
/// be read or decoded.
fn asset_pixels(
    asset: &velato::model::ImageAsset,
    dir: &std::path::Path,
    k: f64,
    left: u64,
) -> Option<Asset> {
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
    let want = capped(asset).map(|(w, h)| {
        let side = |v: u32| ((v as f64 * k).floor() as u32).max(1);
        (side(w), side(h))
    });
    let pixmap = crate::image::decode_rgba(&bytes, want, left).ok()?;
    let (w, h) = authored(asset).unwrap_or((pixmap.width() as f64, pixmap.height() as f64));
    Some(Asset {
        pixmap: Arc::new(pixmap),
        w,
        h,
    })
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

    const KS: &str = r#""ks":{"o":{"a":0,"k":100},"r":{"a":0,"k":0},"p":{"a":0,"k":[0,0,0]},"a":{"a":0,"k":[0,0,0]},"s":{"a":0,"k":[100,100,100]}}"#;

    fn precomp(ind: u32, id: &str) -> String {
        format!(
            r#"{{"ty":0,"ind":{ind},"refId":"{id}","w":100,"h":100,"ip":0,"op":60,"st":0,{KS}}}"#
        )
    }

    /// A shape layer: a square, then a repeater of `copies` (a JSON value).
    fn repeated(ind: u32, copies: &str) -> String {
        format!(
            r#"{{"ty":4,"ind":{ind},"ip":0,"op":60,"st":0,{KS},"shapes":[
              {{"ty":"rc","p":{{"a":0,"k":[0,0]}},"s":{{"a":0,"k":[2,2]}},"r":{{"a":0,"k":0}}}},
              {{"ty":"fl","c":{{"a":0,"k":[1,0,0,1]}},"o":{{"a":0,"k":100}}}},
              {{"ty":"rp","c":{copies},"o":{{"a":0,"k":0}},"tr":{{"p":{{"a":0,"k":[1,0]}},"a":{{"a":0,"k":[0,0]}},"s":{{"a":0,"k":[100,100]}},"r":{{"a":0,"k":0}},"so":{{"a":0,"k":100}},"eo":{{"a":0,"k":100}}}}}}]}}"#
        )
    }

    /// Loads a file of `assets` (precomps) and `layers`.
    fn load_of(tag: &str, assets: &[String], layers: &[String]) -> Result<File, String> {
        let json = format!(
            r#"{{"v":"5.7.0","fr":30,"ip":0,"op":60,"w":100,"h":100,"assets":[{}],"layers":[{}]}}"#,
            assets.join(","),
            layers.join(",")
        );
        let dir = std::env::temp_dir().join(format!("strand-lottie-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.json");
        std::fs::write(&path, json).unwrap();
        let out = load(path.to_str().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    fn asset(id: &str, layers: &[String]) -> String {
        format!(r#"{{"id":"{id}","layers":[{}]}}"#, layers.join(","))
    }

    /// `load_of`, which must fail with an error containing `why`.
    fn refused(tag: &str, assets: &[String], layers: &[String], why: &str) {
        match load_of(tag, assets, layers) {
            Err(e) => assert!(e.contains(why), "{tag}: {e}"),
            Ok(_) => panic!("{tag}: loaded"),
        }
    }

    /// (m4-audit) A reference cycle, a precomp or matte that refers back
    /// to itself, made velato recurse until the stack overflowed (an
    /// abort): a precomp instancing itself, a cycle of two, and a layer
    /// that is its own matte are refused at load, named as reference
    /// cycles.
    #[test]
    fn reference_cycles_are_refused() {
        refused(
            "self",
            &[asset("a", &[precomp(1, "a")])],
            &[precomp(1, "a")],
            "reference cycle",
        );
        refused(
            "two",
            &[
                asset("a", &[precomp(1, "b")]),
                asset("b", &[precomp(1, "a")]),
            ],
            &[precomp(1, "a")],
            "reference cycle",
        );
        let own_matte =
            format!(r#"{{"ty":4,"ind":1,"tt":1,"tp":1,"ip":0,"op":60,"st":0,{KS},"shapes":[]}}"#);
        refused("matte", &[], &[own_matte], "reference cycle");
    }

    /// (owner, 2026-10-10) Playback looping and time remapping are not
    /// reference cycles: a precomp whose `tm` keyframes run its time
    /// backwards and around again, instanced twice from the top level and
    /// once from another precomp, loads, and its frames still loop.
    #[test]
    fn looping_and_time_remapping_are_not_reference_cycles() {
        let remapped = |ind: u32, id: &str| {
            format!(
                r#"{{"ty":0,"ind":{ind},"refId":"{id}","w":100,"h":100,"ip":0,"op":60,"st":0,{KS},"tm":{{"a":1,"k":[{{"t":0,"s":[1.5],"i":{{"x":[0.5],"y":[0.5]}},"o":{{"x":[0.5],"y":[0.5]}}}},{{"t":30,"s":[0]}},{{"t":60,"s":[1.5]}}]}}}}"#
            )
        };
        let file = load_of(
            "remap",
            &[
                asset("inner", &[repeated(1, r#"{"a":0,"k":3}"#)]),
                asset("outer", &[remapped(1, "inner")]),
            ],
            &[
                remapped(1, "inner"),
                remapped(2, "inner"),
                precomp(3, "outer"),
            ],
        )
        .unwrap();
        assert_eq!(file.comp.layers.len(), 3);
        assert_eq!(frame_at(&file.comp, 2.5, 1.0), 15.0, "loops");
    }

    /// (m4-audit) velato draws a file's structure as it stands, so a
    /// file it could not draw within bounds is refused at load (reference
    /// cycles: [`reference_cycles_are_refused`]): precomps fanning out
    /// 2^20 layers, a chain nested past `MAX_NESTING`, and a repeater of
    /// 1e18 copies, fixed or reached through its easing. Files within
    /// bounds load, a repeater's copies counted.
    #[test]
    fn files_velato_cannot_draw_within_bounds_are_refused() {
        // Each level instances the next twice: 2^20 leaves.
        let fan: Vec<String> = (0..20)
            .map(|i| {
                asset(
                    &format!("f{i}"),
                    &[
                        precomp(1, &format!("f{}", i + 1)),
                        precomp(2, &format!("f{}", i + 1)),
                    ],
                )
            })
            .chain(std::iter::once(asset(
                "f20",
                &[repeated(1, r#"{"a":0,"k":1}"#)],
            )))
            .collect();
        refused("fan", &fan, &[precomp(1, "f0")], "would draw over");
        let chain: Vec<String> = (0..40)
            .map(|i| asset(&format!("c{i}"), &[precomp(1, &format!("c{}", i + 1))]))
            .collect();
        refused("deep", &chain, &[precomp(1, "c0")], "nest deeper");
        refused(
            "copies",
            &[],
            &[repeated(1, r#"{"a":0,"k":1e18}"#)],
            "would draw over",
        );
        // 2 → 3 copies, but an easing handle 1e17 high overshoots.
        let eased = r#"{"a":1,"k":[{"t":0,"s":[2],"i":{"x":[0.5],"y":[1e17]},"o":{"x":[0.5],"y":[0]}},{"t":60,"s":[3]}]}"#;
        refused("eased", &[], &[repeated(1, eased)], "would draw over");

        // Within bounds: a shared precomp instanced from several places,
        // 1,000 copies, and a precomp nested 8 deep.
        let nested: Vec<String> = (0..8)
            .map(|i| asset(&format!("n{i}"), &[precomp(1, &format!("n{}", i + 1))]))
            .chain(std::iter::once(asset(
                "n8",
                &[repeated(1, r#"{"a":0,"k":1000}"#)],
            )))
            .collect();
        let file = load_of("fine", &nested, &[precomp(1, "n0"), precomp(2, "n3")]).unwrap();
        assert_eq!(file.comp.layers.len(), 2);
    }

    /// A file whose assets claim 4096 × 4096 each (from tiny PNGs) holds
    /// no side over [`MAX_ASSET_SIDE`] and no more than
    /// [`MAX_ASSET_PIXELS`] together, and each keeps its authored box.
    #[test]
    fn assets_are_decoded_within_their_bound() {
        let mut png = Vec::new();
        {
            let mut e = ::png::Encoder::new(&mut png, 2, 2);
            e.set_color(::png::ColorType::Rgba);
            e.set_depth(::png::BitDepth::Eight);
            let mut w = e.write_header().unwrap();
            w.write_image_data(&[255u8; 16]).unwrap();
        }
        let b64 = {
            const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut out = String::new();
            for c in png.chunks(3) {
                let n = c
                    .iter()
                    .enumerate()
                    .fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
                for i in 0..4 {
                    if i <= c.len() {
                        out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
                    } else {
                        out.push('=');
                    }
                }
            }
            out
        };
        assert_eq!(base64(&b64).unwrap(), png);
        let assets: Vec<String> = (0..4)
            .map(|i| {
                format!(
                    r#"{{"id":"a{i}","w":4096,"h":4096,"u":"","p":"data:image/png;base64,{b64}","e":1}}"#
                )
            })
            .collect();
        let json = format!(
            r#"{{"v":"5.7.0","fr":30,"ip":0,"op":60,"w":100,"h":100,"assets":[{}],"layers":[]}}"#,
            assets.join(",")
        );
        let dir = std::env::temp_dir().join(format!("strand-lottie-budget-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.json");
        std::fs::write(&path, json).unwrap();
        let file = load(path.to_str().unwrap()).unwrap();
        let mut total = 0u64;
        for a in file.images.values() {
            let (w, h) = (a.pixmap.width() as u32, a.pixmap.height() as u32);
            assert!(w <= MAX_ASSET_SIDE && h <= MAX_ASSET_SIDE, "{w}x{h}");
            assert_eq!((a.w, a.h), (4096.0, 4096.0), "its authored box");
            total += u64::from(w) * u64::from(h);
        }
        assert!(total <= MAX_ASSET_PIXELS, "{total}");
        // Each within 4096², then all four shrunk alike to 2048².
        assert_eq!(file.images.len(), 4);
        for a in file.images.values() {
            assert_eq!((a.pixmap.width(), a.pixmap.height()), (2048, 2048));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (owner, 2026-10-10: memory budgets are test targets, never limits
    /// on a shell) An asset a shell would show, authored at 1920 × 1080,
    /// is decoded at that size, not shrunk to stay under a budget; the
    /// bound only stops a file whose assets claim more than an image
    /// decode may hold.
    #[test]
    fn an_asset_a_shell_shows_is_decoded_at_its_authored_size() {
        let mut png = Vec::new();
        {
            let mut e = ::png::Encoder::new(&mut png, 2, 2);
            e.set_color(::png::ColorType::Rgba);
            e.set_depth(::png::BitDepth::Eight);
            let mut w = e.write_header().unwrap();
            w.write_image_data(&[255u8; 16]).unwrap();
        }
        let dir = std::env::temp_dir().join(format!("strand-lottie-hd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("bg.png"), &png).unwrap();
        let json = r#"{"v":"5.7.0","fr":30,"ip":0,"op":60,"w":100,"h":100,"assets":[{"id":"bg","w":1920,"h":1080,"u":"","p":"bg.png","e":0}],"layers":[]}"#;
        let path = dir.join("hd.json");
        std::fs::write(&path, json).unwrap();
        let file = load(path.to_str().unwrap()).unwrap();
        let a = &file.images["bg"];
        assert_eq!((a.pixmap.width(), a.pixmap.height()), (1920, 1080));
        const { assert!(MAX_ASSET_PIXELS >= 1920 * 1080 * 8, "room for several") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
