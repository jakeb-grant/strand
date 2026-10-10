//! A frame's ops as a vello_gpu scene, built on the GPU thread so the
//! encoding stays off the main thread.

use std::collections::HashMap;

use vello_common::TextureId;
use vello_common::filter_effects::{EdgeMode, Filter, FilterPrimitive};
use vello_common::geometry::RectU16;
use vello_common::kurbo::{Affine, Rect, Shape};
use vello_common::paint::{Image, ImageId, ImageSource, PaintType, Tint, TintMode};
use vello_common::peniko::{Extend, Fill, ImageQuality, ImageSampler};
use vello_gpu::Scene;

use crate::{Brush, Op};

/// An upload the GPU holds.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Held {
    pub generation: u64,
    pub id: ImageId,
}

/// Uploads by render's id.
pub(crate) type Images = HashMap<u64, Held>;

/// A pass already drawn into its own texture: the scene samples it.
#[derive(Copy, Clone, Debug)]
pub(crate) struct DrawnPass {
    pub texture: TextureId,
    pub width: u16,
    pub height: u16,
}

/// What a scene could not draw (an upload the GPU does not have).
#[derive(Debug, Default)]
pub(crate) struct Missing {
    pub images: usize,
}

fn sampler(smooth: bool) -> ImageSampler {
    ImageSampler {
        x_extend: Extend::Pad,
        y_extend: Extend::Pad,
        quality: if smooth {
            ImageQuality::Medium
        } else {
            ImageQuality::Low
        },
        alpha: 1.0,
    }
}

/// Encodes `ops` into `scene`. Push and pop ops are balanced here: an
/// extra pop is ignored and what is still pushed at the end is popped.
pub(crate) fn build(
    scene: &mut Scene,
    ops: &[Op],
    images: &Images,
    passes: &[Option<DrawnPass>],
) -> Missing {
    let mut missing = Missing::default();
    // What is pushed: true for a layer, false for a clip.
    let mut stack: Vec<bool> = Vec::new();
    let mut pass_index = 0;
    scene.reset_transform();
    for op in ops {
        match op {
            Op::Transform(a) => scene.set_transform(*a),
            Op::PushClip(p) => {
                scene.push_clip_path(p);
                stack.push(false);
            }
            Op::PushClipEvenOdd(p) => {
                scene.set_fill_rule(Fill::EvenOdd);
                scene.push_clip_path(p);
                scene.set_fill_rule(Fill::NonZero);
                stack.push(false);
            }
            Op::PopClip => {
                if stack.last() == Some(&false) {
                    stack.pop();
                    scene.pop_clip();
                }
            }
            Op::PushLayer(l) => {
                let filter = l.blur.filter(|b| b.is_finite() && *b > 0.0).map(|b| {
                    Filter::from_primitive(FilterPrimitive::GaussianBlur {
                        std_deviation: b,
                        edge_mode: EdgeMode::None,
                    })
                });
                let opacity = l.opacity.map(|o| {
                    if o.is_finite() {
                        o.clamp(0.0, 1.0)
                    } else {
                        1.0
                    }
                });
                scene.push_layer(l.clip.as_ref(), l.blend, opacity, None, filter);
                stack.push(true);
            }
            Op::PopLayer => {
                if stack.last() == Some(&true) {
                    stack.pop();
                    scene.pop_layer();
                }
            }
            Op::Fill {
                path,
                brush,
                brush_transform,
                even_odd,
            } => {
                match brush {
                    Brush::Solid(c) => scene.set_paint(*c),
                    Brush::Gradient(g) => scene.set_paint(g.clone()),
                }
                scene.set_paint_transform(*brush_transform);
                scene.set_fill_rule(if *even_odd {
                    Fill::EvenOdd
                } else {
                    Fill::NonZero
                });
                scene.fill_path(path);
                scene.set_fill_rule(Fill::NonZero);
                scene.reset_paint_transform();
            }
            Op::Image {
                rect,
                image,
                image_transform,
                tint,
                smooth,
            } => {
                let Some(held) = images.get(image) else {
                    missing.images += 1;
                    continue;
                };
                if let Some(c) = tint {
                    scene.set_tint(Some(Tint {
                        color: *c,
                        mode: TintMode::AlphaMask,
                    }));
                }
                scene.set_paint(PaintType::Image(Image {
                    image: ImageSource::opaque_id(held.id),
                    sampler: sampler(*smooth),
                }));
                scene.set_paint_transform(*image_transform);
                scene.fill_rect(rect);
                scene.reset_paint_transform();
                scene.reset_tint();
            }
            Op::Pass { bounds, .. } => {
                let drawn = passes.get(pass_index).copied().flatten();
                pass_index += 1;
                let Some(d) = drawn else { continue };
                let (w, h) = (bounds.width(), bounds.height());
                if !(w > 0.0 && h > 0.0) {
                    continue;
                }
                scene.set_paint(PaintType::Image(Image {
                    image: ImageSource::external_texture(
                        d.texture,
                        RectU16 {
                            x0: 0,
                            y0: 0,
                            x1: d.width,
                            y1: d.height,
                        },
                        true,
                    ),
                    sampler: sampler(true),
                }));
                scene.set_paint_transform(
                    Affine::translate((bounds.x0, bounds.y0))
                        * Affine::scale_non_uniform(w / d.width as f64, h / d.height as f64),
                );
                scene
                    .fill_path(&Rect::new(bounds.x0, bounds.y0, bounds.x1, bounds.y1).to_path(0.1));
                scene.reset_paint_transform();
            }
        }
    }
    while let Some(layer) = stack.pop() {
        if layer {
            scene.pop_layer();
        } else {
            scene.pop_clip();
        }
    }
    missing
}

/// Swaps red and blue: the CPU's pixmaps are in the raster's BGRA order,
/// the GPU draws straight RGBA.
pub(crate) fn unswap(src: &vello_common::pixmap::Pixmap) -> vello_common::pixmap::Pixmap {
    let mut out = src.clone();
    for p in out.data_mut() {
        std::mem::swap(&mut p.r, &mut p.b);
    }
    out
}
