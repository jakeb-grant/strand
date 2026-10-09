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
        let key = crate::image::ImageKey {
            source,
            icon,
            w: phys.w.min(4096),
            h: phys.h.min(4096),
            fit,
            scale: self.scale.as_f32().ceil().clamp(1.0, 8.0) as u16,
        };
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
        let Some(d) = decoded else {
            return;
        };
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
        let (dx, dy, dw, dh) = d.placed_in(rect.x0, rect.y0, rect.width(), rect.height());
        self.push(
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
        if clip.is_some() {
            self.marker(Item::PopClip);
        }
    }
}
