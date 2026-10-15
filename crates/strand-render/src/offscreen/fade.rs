//! (m6-owner-fixes, owner-testing.md bug 17) Fade layers: an opacity
//! group whose content is unchanged but for where it is drawn (an `enter`
//! or `exit` pose of `opacity` and `x`/`y`, a `when` fade, a translucent
//! row scrolling) is rastered once into an offscreen layer, a cached
//! offscreen group (design.md, "Runtime changes these need", item 2), and
//! each frame draws that layer as an image at the group's opacity (its
//! alpha: no group is composited) and position instead of rastering its
//! content again.
//!
//! - **Which groups.** An opacity group the flattener tagged with its
//!   node ([`crate::flatten::Fade`]), outside every transform and
//!   offscreen group (its pixels are then whole surface pixels), whose
//!   content holds no offscreen group (a blur, a colour matrix, a
//!   backdrop: those read pixels around them) and something that costs
//!   the raster more per pixel than a layer's image does (a shadow, a
//!   gradient, text filled with a paint: [`costly`]). Flat fills,
//!   borders, plain text and images cost vello_cpu's fine stage about
//!   what the image would, so a group of only those is drawn as before
//!   (measured: `tests/morph.rs`,
//!   `a_fading_dashboard_draws_its_panels_from_layers`). The outermost
//!   such group is layered; one nested in it is drawn into its layer.
//! - **The key.** The group's items hashed from the top-left pixel of
//!   what they draw ([`crate::flatten::Shift`]), with the layer's size
//!   and the scale: the same content moved by whole pixels has the same
//!   key, and anything else (a glyph shaped anew, a colour, a box moved
//!   within the group, a half-pixel phase at a fractional scale) has
//!   another. Its pixels are those of its items drawn at their place, so
//!   a key that matches is exact.
//! - **When it is built.** On a frame that repaints the group whole (it
//!   always does while its opacity or offset changes), when its node is
//!   in flight ([`crate::flatten::Fade::moving`]) or the same key was
//!   drawn in the node's recent frames. A node whose last two layers
//!   were never drawn again (content changing every frame) builds no more
//!   until it rests. A layer that is cached is drawn whatever the damage.
//! - **What it costs.** The group's pixels once per key, in the offscreen
//!   groups' budget ([`super::OFFSCREEN_BYTES`], least recently used
//!   first, freed when idle): at a fractional scale a fade moving by
//!   whole logical pixels has two keys, one per half-pixel phase.
//! - **Damage** is unchanged: the group's pixels repaint as before; what
//!   is saved is rastering its content.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use strand_scene::{Damage, NodeId, Rect, Scale};
use vello_cpu::kurbo::Affine;

use super::{Drawn, Offscreen, pins, rasterise};
use crate::cache::PaintCache;
use crate::flatten::{DisplayItem, Item, Shift, hash_item_from};
use crate::raster::{AtlasMirror, skip_group};

/// Frames (paints of any surface) a node's recent keys count for.
const RECENT: u64 = 6;

/// Layers built and never drawn again before a node builds no more.
const WASTED: u8 = 2;

/// Largest side of a layer, pixels.
const MAX_SIDE: u32 = 4096;

/// The fade layers' state in [`Offscreen`].
#[derive(Debug)]
pub(super) struct Fades {
    pub(super) enabled: bool,
    /// This frame's layers, by their group's push item (its address).
    pub(super) current: HashMap<usize, Drawn>,
    nodes: HashMap<NodeId, History>,
    pub(super) builds: u64,
    pub(super) draws: u64,
}

impl Default for Fades {
    fn default() -> Self {
        Self {
            enabled: true,
            current: HashMap::new(),
            nodes: HashMap::new(),
            builds: 0,
            draws: 0,
        }
    }
}

/// A node's recent fade groups.
#[derive(Debug, Default)]
struct History {
    /// The frame it was last seen.
    seen: u64,
    /// Its last keys, newest first.
    keys: [u64; 2],
    /// Its last layer built, and whether a frame drew it since.
    built: Option<(u64, bool)>,
    /// Layers built and never drawn again, in a row.
    wasted: u8,
}

/// Whether drawing `item` costs the raster more per pixel than drawing a
/// layer's pixels does (see the module docs).
fn costly(item: &Item) -> bool {
    match item {
        Item::Shadow { .. } => true,
        Item::Fill { paint, .. } | Item::Border { paint, .. } => {
            !matches!(paint, strand_scene::Paint::Solid(_))
        }
        Item::Glyphs { fill, .. } => fill.is_some(),
        _ => false,
    }
}

/// The key of the push item `d` in [`Fades::current`].
pub(crate) fn item_key(d: &DisplayItem) -> usize {
    d as *const DisplayItem as usize
}

impl Offscreen {
    /// Finds or builds this frame's fade layers among `items` (see the
    /// module docs); `whole` tells if the damage repaints a rectangle
    /// whole.
    pub(crate) fn prepare_fades(
        &mut self,
        items: &[DisplayItem],
        damage: &Damage,
        surface: Rect,
        atlas: &AtlasMirror,
        cache: &PaintCache,
        scale: Scale,
    ) {
        self.fades.current.clear();
        if !self.fades.enabled {
            return;
        }
        let frame = self.frame;
        // Groups inside a transform or an offscreen group are not drawn in
        // whole surface pixels: each open transform and layer, and
        // whether it is one of those.
        let mut open: Vec<bool> = Vec::new();
        let mut i = 0;
        while i < items.len() {
            let d = &items[i];
            match &d.item {
                Item::PushTransform(_) => open.push(true),
                Item::PushLayer(l) => open.push(!l.cell_local()),
                Item::PopTransform | Item::PopLayer => {
                    open.pop();
                }
                Item::PushOpacity(alpha, Some(fade))
                    if !open.contains(&true) && *alpha > 0.0 && d.bounds.intersects(surface) =>
                {
                    let end = skip_group(items, i);
                    if let Some(g) = self.fade_layer(
                        &items[i + 1..end.saturating_sub(1).max(i + 1)],
                        d.bounds,
                        fade.node,
                        fade.moving,
                        (damage, surface),
                        atlas,
                        cache,
                        scale,
                        frame,
                    ) {
                        self.fades.current.insert(item_key(d), g);
                        i = end;
                        continue;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        self.fades.nodes.retain(|_, h| h.seen + RECENT * 4 >= frame);
    }

    /// The layer of one fade group (`inner` its items, `bounds` its push
    /// item's), if one is cached or worth building now.
    #[allow(clippy::too_many_arguments)]
    fn fade_layer(
        &mut self,
        inner: &[DisplayItem],
        bounds: Rect,
        node: NodeId,
        moving: bool,
        (damage, surface): (&Damage, Rect),
        atlas: &AtlasMirror,
        cache: &PaintCache,
        scale: Scale,
        frame: u64,
    ) -> Option<Drawn> {
        let leaf = |d: &&DisplayItem| {
            !matches!(
                d.item,
                Item::PushClip(_)
                    | Item::PopClip
                    | Item::PushOpacity(..)
                    | Item::PopOpacity
                    | Item::PushTransform(_)
                    | Item::PopTransform
                    | Item::PushLayer(_)
                    | Item::PopLayer
            )
        };
        // What reads pixels around it cannot be drawn alone, and content
        // that costs the raster no more than a layer does is drawn as
        // before.
        if inner
            .iter()
            .any(|d| matches!(&d.item, Item::PushLayer(l) if !l.cell_local()))
            || !inner.iter().any(|d| costly(&d.item))
        {
            return None;
        }
        // Everything it draws, unclipped: its leaves' bounds (a push
        // item's bounds are its group's, clipped by what is around it).
        let extent = inner
            .iter()
            .filter(leaf)
            .fold(Rect::default(), |a, d| a.union(d.bounds));
        if extent.is_empty()
            || extent.w > MAX_SIDE
            || extent.h > MAX_SIDE
            || extent.area() * 4 > (super::OFFSCREEN_BYTES / 4) as u64
        {
            return None;
        }
        let o = Shift {
            x: extent.left(),
            y: extent.top(),
        };
        let mut h = DefaultHasher::new();
        ("fade", extent.w, extent.h, scale.as_f32().to_bits()).hash(&mut h);
        // A push item's bounds are its group's as clipped around the
        // fade group, which is not drawn into the layer: only what is
        // drawn and the leaves' bounds count.
        for d in inner {
            hash_item_from(&mut h, &d.item, o);
            if leaf(&d) {
                (
                    d.bounds.left() - o.x,
                    d.bounds.top() - o.y,
                    d.bounds.w,
                    d.bounds.h,
                )
                    .hash(&mut h);
            }
        }
        let key = h.finish();
        // A cached layer is drawn where its content is this frame.
        let at = |g: Drawn| Drawn {
            pixmap: g.pixmap,
            x: extent.x,
            y: extent.y,
        };
        let recent = {
            let hist = self.fades.nodes.entry(node).or_default();
            if hist.seen + RECENT < frame {
                *hist = History::default();
            }
            let recent = hist.seen > 0 && hist.keys.contains(&key);
            hist.seen = frame;
            if hist.keys[0] != key {
                hist.keys = [key, hist.keys[0]];
            }
            recent
        };
        if let Some(g) = self.get(key) {
            let hist = self.fades.nodes.entry(node).or_default();
            if let Some((_, used)) = &mut hist.built {
                *used = true;
            }
            hist.wasted = 0;
            self.fades.draws += 1;
            return Some(at(g));
        }
        // Built only on a frame that repaints all of it that shows.
        let shows = bounds.intersect(surface)?;
        if !damage.covers(shows) {
            return None;
        }
        let hist = self.fades.nodes.entry(node).or_default();
        if let Some((_, false)) = hist.built {
            hist.wasted = hist.wasted.saturating_add(1);
            hist.built = None;
        }
        if hist.wasted >= WASTED || !(moving || recent) {
            return None;
        }
        hist.built = Some((key, false));
        // Drawn whole: no group in it is passed over for being clipped
        // away around the fade group (it may show on a later frame).
        let whole: Vec<DisplayItem> = inner
            .iter()
            .map(|d| DisplayItem {
                item: d.item.clone(),
                bounds: if leaf(&d) { d.bounds } else { extent },
            })
            .collect();
        let pm = rasterise(
            &whole,
            Affine::IDENTITY,
            extent,
            atlas,
            cache,
            scale,
            &HashMap::new(),
        )?;
        let drawn = Drawn {
            pixmap: std::sync::Arc::new(pm),
            x: extent.x,
            y: extent.y,
        };
        self.fades.builds += 1;
        self.fades.draws += 1;
        let pins = pins(inner, &HashMap::new());
        Some(self.insert(key, drawn, pins))
    }
}
