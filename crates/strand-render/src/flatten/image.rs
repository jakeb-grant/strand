//! `icon` and `image` nodes in the display list.

use std::collections::hash_map::DefaultHasher;

use strand_scene::{Color, NodeKind, Prop, PropValue, Rect};
use vello_cpu::kurbo::{self, BezPath, RoundedRectRadii};

use super::paint::radii_zero;
use super::{Flattener, Item};
use crate::tree::Node;

impl Flattener<'_> {
    /// Draws an `icon` or `image` node's source at its box's size, clipped
    /// to its rounded box; asks for it to be decoded when it is not yet.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn image<'v>(
        &mut self,
        node: &Node,
        get: &impl Fn(Prop) -> Option<&'v PropValue>,
        frame: kurbo::Rect,
        phys: Rect,
        box_path: &BezPath,
        r: &RoundedRectRadii,
        color: Color,
        scope: &strand_scene::TokenScope,
        time: Option<strand_scene::TimeContext>,
        sig: &mut DefaultHasher,
        ink: &mut Rect,
    ) {
        let source = match get(Prop::Source) {
            Some(PropValue::Text(t)) if !t.trim().is_empty() => t.clone(),
            Some(PropValue::Keyword(k)) => k.clone(),
            _ => return,
        };
        let icon = node.kind == NodeKind::Icon;
        let fit = match get(Prop::Fit) {
            Some(PropValue::Keyword(k)) => crate::image::Fit::from_name(k).unwrap_or_default(),
            _ => crate::image::Fit::default(),
        };
        let key_of = |source: String| crate::image::ImageKey {
            source,
            icon,
            w: phys.w.min(4096),
            h: phys.h.min(4096),
            fit,
            scale: self.scale.as_f32().ceil().clamp(1.0, 8.0) as u16,
            frame: 0,
        };
        let mut key = key_of(source.clone());
        // (M4) An animated image draws the frame its time shows, and asks
        // for the next one ahead of its tick.
        if !icon && let Some(tl) = self.extras.images.timeline(&key.source) {
            // A finite loop count played out leaves no clock: the node's
            // own time still says it is over.
            let t = match time {
                Some(cx) => cx.t,
                None => self.anim.time_of(node.id, crate::clock::Rate::Refresh).0.t,
            };
            key.frame = tl.frame_at(t);
            let n = tl.delays.len() as u32;
            if n > 1 && !tl.done(t) {
                self.out.images.push(crate::image::ImageKey {
                    frame: (key.frame + 1) % n,
                    ..key.clone()
                });
            }
        }
        let failed = matches!(self.extras.images.get(&key), Some(Err(_)));
        let decoded = match self.extras.images.get(&key) {
            Some(Ok(d)) => Some(d.clone()),
            Some(Err(_)) => None,
            // Not decoded at this size yet (a size springs, or the decode
            // is on its way): the latest decode at another size is drawn
            // scaled into the box meanwhile.
            None => self.extras.images.stand_in(&key).map(|(k, d)| {
                let d = d.clone();
                self.out.images.push(k.clone());
                d
            }),
        };
        self.out.images.push(key);
        // (M4) An image swap under `transition:`: the old source under
        // the new one, which comes in through the mask once decoded
        // (`crate::effects::transition`).
        let decode = {
            use crate::effects::transition::Decode;
            match (decoded.is_some(), failed) {
                (true, _) => Decode::Ready,
                (false, true) => Decode::Failed,
                (false, false) => Decode::Waiting,
            }
        };
        let swap = (node.kind == NodeKind::Image)
            .then(|| self.anim.image_swap(node, &source, decode, scope))
            .flatten();
        let old = swap.as_ref().and_then(|(from, _)| {
            let k = key_of(from.clone());
            let d = match self.extras.images.get(&k) {
                Some(Ok(d)) => Some(d.clone()),
                _ => None,
            };
            self.out.images.push(k);
            d
        });
        if decoded.is_none() && old.is_none() {
            return;
        }
        let clip = (!radii_zero(r)).then(|| self.marker(Item::PushClip(box_path.clone())));
        if let Some(i) = clip {
            self.out.items[i].bounds = phys;
        }
        let rect = kurbo::Rect::new(
            frame.x0,
            frame.y0,
            frame.x0 + phys.w as f64,
            frame.y0 + phys.h as f64,
        );
        let mut draw = |this: &mut Self, d: crate::image::Decoded, sig: &mut DefaultHasher| {
            let (dx, dy, dw, dh) = d.placed_in(rect.x0, rect.y0, rect.width(), rect.height());
            this.push(
                Item::Image {
                    pixmap: d.pixmap,
                    rect,
                    dest: kurbo::Rect::new(dx, dy, dx + dw, dy + dh),
                    tint: d.symbolic.then_some(color),
                },
                phys,
                sig,
                ink,
            );
        };
        if let Some(o) = old {
            draw(self, o, sig);
        }
        if let Some(d) = decoded {
            let mask = swap.map(|(_, m)| {
                use std::hash::Hash;
                format!("{m:?}").hash(sig);
                let (push, pop) = m.group(frame, self.scale.as_f32(), self.xform);
                (self.marker(push), pop)
            });
            draw(self, d, sig);
            if let Some((i, pop)) = mask {
                self.out.items[i].bounds = phys;
                self.marker(pop);
            }
        }
        if clip.is_some() {
            self.marker(Item::PopClip);
        }
    }
}
