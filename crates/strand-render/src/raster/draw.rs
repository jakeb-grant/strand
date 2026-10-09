//! Drawing a display list into one render context, and splitting the
//! damage into disjoint rectangles.

use std::collections::HashMap;
use std::sync::Arc;

use strand_scene::{Damage, Paint, Rect, Scale};
use vello_cpu::kurbo::{self, Affine};
use vello_cpu::peniko::{Extend, Fill, ImageQuality, ImageSampler};
use vello_cpu::{Image, ImageSource, PaintType, RenderContext, Tint, TintMode};

use super::atlas::AtlasMirror;
use super::paint::{bgra, paint_type};
use crate::cache::{
    PaintCache, ShadowShape, draw_shadow, gradient_key, image_paint, render_gradient_part,
    shadow_key,
};
use crate::flatten::{DisplayItem, FillShape, Item};
use crate::offscreen::{Drawn, layer_key};

/// A gradient frame's size in whole pixels.
pub(super) fn frame_px(f: kurbo::Rect) -> Option<(u32, u32)> {
    let (w, h) = (f.width().round(), f.height().round());
    (w.is_finite() && h.is_finite() && w >= 1.0 && h >= 1.0 && w <= 65535.0 && h <= 65535.0)
        .then_some((w as u32, h as u32))
}

/// The paint of `p` over `frame`: a dithered pixmap for a gradient
/// (cached, or for one too large to cache the part of it in `region`,
/// the cell being drawn in item coordinates), else vello's own paint.
/// `smooth` samples the pixmap smoothly (under a scale or rotation).
pub(super) fn paint_for(
    p: &Paint,
    frame: kurbo::Rect,
    cache: &PaintCache,
    region: kurbo::Rect,
    smooth: bool,
) -> (PaintType, Affine) {
    if matches!(p, Paint::Solid(_)) {
        return paint_type(p, frame);
    }
    let Some((w, h)) = frame_px(frame) else {
        return paint_type(p, frame);
    };
    let (fx, fy) = (frame.x0.round(), frame.y0.round());
    if let Some(pm) = cache.peek(gradient_key(p, w, h)) {
        return image_paint(pm, fx, fy, smooth);
    }
    // Too large to cache, or animated (see `PaintCache::gradient`): the
    // part this cell shows, dithered the same.
    {
        // One pixel of margin for smooth sampling at the part's edge.
        let x0 = ((region.x0 - fx).floor() - 1.0).clamp(0.0, w as f64) as u32;
        let y0 = ((region.y0 - fy).floor() - 1.0).clamp(0.0, h as f64) as u32;
        let x1 = ((region.x1 - fx).ceil() + 1.0).clamp(0.0, w as f64) as u32;
        let y1 = ((region.y1 - fy).ceil() + 1.0).clamp(0.0, h as f64) as u32;
        if x1 > x0
            && y1 > y0
            && let Some(pm) = render_gradient_part(p, w, h, (x0, y0, x1 - x0, y1 - y0))
        {
            return image_paint(&Arc::new(pm), fx + x0 as f64, fy + y0 as f64, smooth);
        }
    }
    paint_type(p, frame)
}

/// Index just past the pop matching the push at `i`.
pub(crate) fn skip_group(items: &[DisplayItem], i: usize) -> usize {
    let mut depth = 0usize;
    for (j, d) in items.iter().enumerate().skip(i) {
        match d.item {
            Item::PushClip(_)
            | Item::PushOpacity(_)
            | Item::PushTransform(_)
            | Item::PushLayer(_) => depth += 1,
            Item::PopClip | Item::PopOpacity | Item::PopTransform | Item::PopLayer => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return j + 1;
                }
            }
            _ => {}
        }
    }
    items.len()
}

/// Encodes the display items that touch `clip`. `base` maps surface
/// coordinates to the cell being rasterised, and `start` is the
/// transform in force before the first item (an offscreen group's).
/// `groups` holds this frame's offscreen groups.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw(
    ctx: &mut RenderContext,
    items: &[DisplayItem],
    clip: Rect,
    atlas: &AtlasMirror,
    cache: &PaintCache,
    scale: Scale,
    base: Affine,
    start: Affine,
    groups: &HashMap<usize, Drawn>,
) {
    let touches = |b: &Rect| clip.intersects(*b);
    // The cell in item coordinates under transform `cur`.
    let region = |cur: Affine| {
        let c = kurbo::Rect::new(
            clip.x as f64,
            clip.y as f64,
            clip.x as f64 + clip.w as f64,
            clip.y as f64 + clip.h as f64,
        );
        (cur.inverse() * base).transform_rect_bbox(c)
    };
    // The transform in force (`scale`, `rotate` groups), and the ones
    // their pops return to.
    let mut cur = base * start;
    let mut saved: Vec<Affine> = Vec::new();
    let mut i = 0;
    while i < items.len() {
        let d = &items[i];
        i += 1;
        match &d.item {
            Item::PushClip(_)
            | Item::PushOpacity(_)
            | Item::PushTransform(_)
            | Item::PushLayer(_)
                if !touches(&d.bounds) =>
            {
                i = skip_group(items, i - 1);
            }
            Item::PushTransform(a) => {
                saved.push(cur);
                cur = base * *a;
                ctx.set_transform(cur);
            }
            Item::PopTransform => {
                cur = saved.pop().unwrap_or(base * start);
                ctx.set_transform(cur);
            }
            Item::PushClip(p) => ctx.push_clip_path(p),
            Item::PopClip => ctx.pop_clip(),
            Item::PushOpacity(o) => ctx.push_opacity_layer(*o),
            Item::PopOpacity => ctx.pop_layer(),
            Item::PushLayer(l) => {
                crate::layers::push(ctx, l, cur);
                // An offscreen group: its filtered pixels, as an image in
                // surface pixels, in place of its items.
                if let Some(g) = groups.get(&layer_key(l)) {
                    ctx.set_transform(base);
                    let (p, t) = image_paint(&g.pixmap, g.x as f64, g.y as f64, false);
                    ctx.set_paint(p);
                    ctx.set_paint_transform(t);
                    ctx.fill_rect(&kurbo::Rect::new(
                        g.x as f64,
                        g.y as f64,
                        g.x as f64 + g.pixmap.width() as f64,
                        g.y as f64 + g.pixmap.height() as f64,
                    ));
                    ctx.reset_paint_transform();
                    ctx.set_transform(cur);
                    ctx.pop_layer();
                    i = skip_group(items, i - 1);
                }
            }
            Item::PopLayer => ctx.pop_layer(),
            _ if !touches(&d.bounds) => {}
            Item::Shadow {
                rect,
                radii,
                std_dev,
                color,
                clip,
                extent,
            } => {
                ctx.set_fill_rule(Fill::EvenOdd);
                ctx.push_clip_path(clip);
                ctx.set_fill_rule(Fill::NonZero);
                let shape = ShadowShape {
                    rect: *rect,
                    radii: *radii,
                    std_dev: *std_dev,
                    color: *color,
                    extent: *extent,
                };
                match cache.peek(shadow_key(&shape)) {
                    Some(pm) => {
                        let (ox, oy) = (extent.x0.floor(), extent.y0.floor());
                        let (p, t) = image_paint(pm, ox, oy, cur != base);
                        ctx.set_paint(p);
                        ctx.set_paint_transform(t);
                        ctx.fill_rect(&kurbo::Rect::new(
                            ox,
                            oy,
                            ox + pm.width() as f64,
                            oy + pm.height() as f64,
                        ));
                        ctx.reset_paint_transform();
                    }
                    None => draw_shadow(ctx, &shape),
                }
                ctx.pop_clip();
            }
            Item::Fill {
                shape,
                paint,
                frame,
            } => {
                let (p, t) = paint_for(paint, *frame, cache, region(cur), cur != base);
                ctx.set_paint(p);
                ctx.set_paint_transform(t);
                match shape {
                    FillShape::Rect(r) => ctx.fill_rect(r),
                    FillShape::Path(p) => ctx.fill_path(p),
                }
                ctx.reset_paint_transform();
            }
            Item::Border { path, paint, frame } => {
                let (p, t) = paint_for(paint, *frame, cache, region(cur), cur != base);
                ctx.set_paint(p);
                ctx.set_paint_transform(t);
                ctx.set_fill_rule(Fill::EvenOdd);
                ctx.fill_path(path);
                ctx.set_fill_rule(Fill::NonZero);
                ctx.reset_paint_transform();
            }
            Item::Raster { pixmap, rect, .. } => {
                let (p, t) = image_paint(pixmap, rect.x0, rect.y0, cur != base);
                ctx.set_paint(p);
                ctx.set_paint_transform(t);
                ctx.fill_rect(rect);
                ctx.reset_paint_transform();
            }
            Item::Image {
                pixmap,
                rect,
                dest,
                tint,
            } => {
                // Decoded at this size: drawn pixel for pixel unless a
                // transform scales or turns it. A decode at another size
                // standing in (a size spring) is scaled onto where the
                // fit places it, clipped to the box.
                let (kx, ky) = (
                    dest.width() / pixmap.width().max(1) as f64,
                    dest.height() / pixmap.height().max(1) as f64,
                );
                let resized = (kx - 1.0).abs() > 1e-9 || (ky - 1.0).abs() > 1e-9;
                let smooth = cur != base || resized;
                let (p, t) = image_paint(pixmap, dest.x0, dest.y0, smooth);
                let t = if resized {
                    t * kurbo::Affine::scale_non_uniform(kx, ky)
                } else {
                    t
                };
                if let Some(c) = tint {
                    ctx.set_tint(Some(Tint {
                        color: bgra(*c),
                        mode: TintMode::AlphaMask,
                    }));
                }
                ctx.set_paint(p);
                ctx.set_paint_transform(t);
                ctx.fill_rect(rect);
                ctx.reset_paint_transform();
                ctx.reset_tint();
            }
            Item::Glyphs {
                x,
                y,
                layout,
                color,
                spans,
            } => {
                // A layout shaped for another scale (the surface moved
                // to a different output) is drawn resampled until the
                // re-shaped one arrives.
                let k = scale.as_f64() / layout.scale.as_f64();
                let transformed = cur != base;
                let quality = if k == 1.0 && !transformed {
                    ImageQuality::Low
                } else {
                    ImageQuality::Medium
                };
                ctx.set_transform(
                    cur * Affine::translate((*x as f64, *y as f64)) * Affine::scale(k),
                );
                for (g, run_color) in layout
                    .runs
                    .iter()
                    .flat_map(|r| r.glyphs.iter().map(move |g| (g, r.color)))
                {
                    let Some(page) = atlas.page(g.slot.page) else {
                        continue;
                    };
                    let gb = Rect::new(
                        *x + (g.x as f64 * k).floor() as i32,
                        *y + (g.y as f64 * k).floor() as i32,
                        (g.slot.w as f64 * k).ceil() as u32 + 1,
                        (g.slot.h as f64 * k).ceil() as u32 + 1,
                    );
                    // Under a transform glyph boxes are not in surface
                    // pixels: the group's bounds already matched.
                    if !transformed && !touches(&gb) {
                        continue;
                    }
                    // Marks and markup spans paint in their own colour.
                    ctx.set_tint(Some(Tint {
                        color: bgra(crate::flatten::slot_color(run_color, spans, *color)),
                        mode: TintMode::AlphaMask,
                    }));
                    ctx.set_paint(PaintType::Image(Image {
                        image: ImageSource::Pixmap(page.clone()),
                        sampler: ImageSampler {
                            x_extend: Extend::Pad,
                            y_extend: Extend::Pad,
                            quality,
                            alpha: 1.0,
                        },
                    }));
                    ctx.set_paint_transform(Affine::translate((
                        (g.x - g.slot.x as i32) as f64,
                        (g.y - g.slot.y as i32) as f64,
                    )));
                    ctx.fill_rect(&kurbo::Rect::new(
                        g.x as f64,
                        g.y as f64,
                        (g.x + g.slot.w as i32) as f64,
                        (g.y + g.slot.h as i32) as f64,
                    ));
                }
                ctx.reset_tint();
                ctx.reset_paint_transform();
                ctx.set_transform(cur);
            }
        }
    }
}

pub(super) fn to_kurbo(r: Rect) -> kurbo::Rect {
    kurbo::Rect::new(
        r.left() as f64,
        r.top() as f64,
        r.right() as f64,
        r.bottom() as f64,
    )
}

/// Splits damage into disjoint rectangles covering exactly the same
/// pixels: horizontal bands between rectangle edges, each band's covered
/// x-intervals merged, and vertically adjacent identical spans joined.
pub(super) fn disjoint(damage: &Damage) -> Vec<Rect> {
    let rects = damage.rects();
    if rects.len() <= 1 {
        return rects.to_vec();
    }
    let mut ys: Vec<i64> = rects.iter().flat_map(|r| [r.top(), r.bottom()]).collect();
    ys.sort_unstable();
    ys.dedup();
    let mut out: Vec<Rect> = Vec::new();
    // Spans of the previous band, as indices into `out`, for joining.
    let mut open: Vec<usize> = Vec::new();
    for band in ys.windows(2) {
        let (y0, y1) = (band[0], band[1]);
        let mut spans: Vec<(i64, i64)> = rects
            .iter()
            .filter(|r| r.top() <= y0 && r.bottom() >= y1)
            .map(|r| (r.left(), r.right()))
            .collect();
        spans.sort_unstable();
        let mut merged: Vec<(i64, i64)> = Vec::new();
        for (a, b) in spans {
            match merged.last_mut() {
                Some(last) if a <= last.1 => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        let mut next_open = Vec::new();
        for (a, b) in merged {
            let joined = open.iter().copied().find(|&i| {
                let r = out[i];
                r.left() == a && r.right() == b && r.bottom() == y0
            });
            match joined {
                Some(i) => {
                    out[i] = Rect::from_edges(a, out[i].top(), b, y1);
                    next_open.push(i);
                }
                None => {
                    out.push(Rect::from_edges(a, y0, b, y1));
                    next_open.push(out.len() - 1);
                }
            }
        }
        open = next_open;
    }
    out
}
