//! The outline and the repaint flash as display items, drawn after a
//! frame's own items by both backends (`Renderer::paint_frame`, through
//! [`Inspect::overlay`]).
//!
//! The flash tints the area a frame repainted for its own reasons (the
//! damage its node records gave, before the overlay's), fading from
//! [`FLASH_ALPHA`] to nothing over [`FLASH`]. Each later frame repaints
//! the tints it changes; that repaint is the overlay's own and is never
//! tinted, so the flash does not feed itself. The outline is a ring of
//! [`OUTLINE_WIDTH`] logical pixels inside the node's drawn box, under
//! its transform and its ancestors' clip.

use std::hash::{Hash, Hasher};
use std::time::Duration;

use strand_scene::{Color, Damage, Paint, Rect, Scale, SurfaceId};
use vello_cpu::kurbo::{self, BezPath, Shape};

use super::{FLASH, Inspect};
use crate::flatten::{DisplayItem, FillShape, HitBox, Item};

/// The outline's colour.
pub const OUTLINE: Color = Color::new(0.129, 0.588, 0.953, 1.0);

/// The outline's width, logical pixels.
pub const OUTLINE_WIDTH: f64 = 2.0;

/// The flash's colour; its alpha fades from [`FLASH_ALPHA`].
pub const FLASH_COLOR: Color = Color::new(0.0, 0.902, 0.463, 1.0);

/// The flash's alpha on the frame that repainted.
pub const FLASH_ALPHA: f32 = 0.45;

/// What the inspector adds to one frame.
#[derive(Debug, Default)]
pub(crate) struct Overlay {
    /// Drawn after the frame's own items.
    pub(crate) items: Vec<DisplayItem>,
    /// Where the overlay changed since the surface's last frame: added
    /// to the frame's damage.
    pub(crate) damage: Damage,
}

/// A hit shape's box without `hit: grow(n)`, physical pixels, in the
/// node's own (untransformed) space.
pub(crate) fn drawn_box(h: &HitBox) -> kurbo::Rect {
    let r = h.rect.inflate(-h.grow, -h.grow);
    if r.width() < 0.0 || r.height() < 0.0 {
        h.rect
    } else {
        r
    }
}

/// Smallest pixel rectangle covering `r`.
fn cover(r: kurbo::Rect) -> Rect {
    Rect::from_edges(
        r.x0.floor() as i64,
        r.y0.floor() as i64,
        r.x1.ceil() as i64,
        r.y1.ceil() as i64,
    )
}

fn kurbo_rect(r: Rect) -> kurbo::Rect {
    kurbo::Rect::new(
        r.left() as f64,
        r.top() as f64,
        r.right() as f64,
        r.bottom() as f64,
    )
}

/// The flash's alpha `age` after its frame (none once it has faded).
fn flash_alpha(age: Duration) -> Option<f32> {
    let left = 1.0 - age.as_secs_f32() / FLASH.as_secs_f32();
    // Quantised to the 8-bit steps it is drawn at, so a frame whose
    // tints round alike repaints nothing.
    let a = (FLASH_ALPHA * left * 255.0).round() / 255.0;
    (left > 0.0 && a > 0.0).then_some(a)
}

impl Inspect {
    /// The overlay of `surface`'s frame at `time`, whose own repaint is
    /// `content` and whose hit shapes are `hits` (`bounds`: the buffer).
    /// `None` when the inspector has nothing to draw or erase anywhere
    /// (no work and no allocation).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn overlay(
        &mut self,
        surface: SurfaceId,
        hits: &[HitBox],
        lock: bool,
        content: &Damage,
        time: Duration,
        scale: Scale,
        bounds: Rect,
    ) -> Option<Overlay> {
        if self.idle() {
            return None;
        }
        let mut out = Overlay::default();
        let mut now = Damage::new();
        let mut sig = std::collections::hash_map::DefaultHasher::new();
        if self.flash && !lock && !time.is_zero() {
            let list = self.flashes.entry(surface).or_default();
            list.retain(|(_, t)| flash_alpha(time.saturating_sub(*t)).is_some());
            if !content.is_empty() {
                list.push((*content, time));
            }
            for (d, t) in list.iter() {
                let Some(a) = flash_alpha(time.saturating_sub(*t)) else {
                    continue;
                };
                for r in d.rects() {
                    let Some(r) = r.intersect(bounds) else {
                        continue;
                    };
                    out.items.push(DisplayItem {
                        item: Item::Fill {
                            shape: FillShape::Rect(kurbo_rect(r)),
                            paint: Paint::Solid(FLASH_COLOR.with_alpha(a)),
                            frame: kurbo_rect(r),
                        },
                        bounds: r,
                    });
                    now.add(r);
                    (r.x, r.y, r.w, r.h, a.to_bits()).hash(&mut sig);
                }
            }
            if list.is_empty() {
                self.flashes.remove(&surface);
            }
        }
        if let Some(node) = self.hover.or(self.highlight).filter(|_| !lock)
            && let Some(h) = hits.iter().rev().find(|h| h.node == node)
            && let Some(at) = outline(h, scale, bounds, &mut out.items)
        {
            now.add(at);
            (node, at.x, at.y, at.w, at.h).hash(&mut sig);
        }
        let sig = sig.finish();
        let before = self.drawn.remove(&surface);
        let changed = before.map(|(_, s)| s) != (!now.is_empty()).then_some(sig);
        if changed {
            if let Some((d, _)) = before {
                out.damage.union(&d);
            }
            out.damage.union(&now);
        }
        if !now.is_empty() {
            self.drawn.insert(surface, (now, sig));
        }
        Some(out)
    }
}

/// Pushes the outline of `h`'s node: a ring inside its drawn box, under
/// its transform, inside its ancestors' clip. Returns the pixels it can
/// touch (none: clipped away or empty).
fn outline(h: &HitBox, scale: Scale, bounds: Rect, items: &mut Vec<DisplayItem>) -> Option<Rect> {
    let r = drawn_box(h);
    if r.width() <= 0.0 || r.height() <= 0.0 {
        return None;
    }
    let w = (OUTLINE_WIDTH * scale.as_f64())
        .min(r.width() / 2.0)
        .min(r.height() / 2.0);
    // Its rounded shape, as drawn (the hit shape's radii without the
    // grow), and inside it the same shape `w` in.
    let less = |r: kurbo::RoundedRectRadii, by: f64| {
        kurbo::RoundedRectRadii::new(
            (r.top_left - by).max(0.0),
            (r.top_right - by).max(0.0),
            (r.bottom_right - by).max(0.0),
            (r.bottom_left - by).max(0.0),
        )
    };
    let outer = less(h.radii, h.grow);
    let mut ring = BezPath::new();
    ring.extend(kurbo::RoundedRect::from_rect(r, outer).path_elements(0.1));
    ring.extend(
        kurbo::RoundedRect::from_rect(r.inflate(-w, -w), less(outer, w)).path_elements(0.1),
    );
    let xform = h.inverse.map(|inv| inv.inverse());
    let reach = xform.map_or(r, |x| x.transform_rect_bbox(r));
    let at = cover(reach).intersect(h.clip)?.intersect(bounds)?;
    items.push(DisplayItem {
        item: Item::PushClip(kurbo_rect(h.clip).to_path(0.1)),
        bounds: at,
    });
    if let Some(x) = xform {
        items.push(DisplayItem {
            item: Item::PushTransform(x),
            bounds: at,
        });
    }
    items.push(DisplayItem {
        item: Item::Border {
            path: ring,
            paint: Paint::Solid(OUTLINE),
            frame: r,
        },
        bounds: at,
    });
    if xform.is_some() {
        items.push(DisplayItem {
            item: Item::PopTransform,
            bounds: at,
        });
    }
    items.push(DisplayItem {
        item: Item::PopClip,
        bounds: at,
    });
    Some(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flash_fades_over_its_300_ms() {
        let a0 = flash_alpha(Duration::ZERO).unwrap();
        assert!((a0 - FLASH_ALPHA).abs() < 1.0 / 255.0);
        let mid = flash_alpha(Duration::from_millis(150)).unwrap();
        assert!(mid < a0 && mid > 0.0);
        assert_eq!(flash_alpha(FLASH), None);
        assert_eq!(flash_alpha(Duration::from_secs(1)), None);
    }

    /// Off and with nothing outlined, the overlay does nothing and keeps
    /// nothing.
    #[test]
    fn an_idle_inspector_adds_nothing() {
        let mut i = Inspect::default();
        let d = Damage::from_rect(Rect::new(0, 0, 10, 10));
        let o = i.overlay(
            SurfaceId(1),
            &[],
            false,
            &d,
            Duration::from_secs(1),
            Scale::ONE,
            Rect::new(0, 0, 100, 100),
        );
        assert!(o.is_none());
        assert_eq!(i.flashes.capacity(), 0);
        assert_eq!(i.drawn.capacity(), 0);
    }
}
