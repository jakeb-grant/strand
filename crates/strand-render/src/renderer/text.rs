//! Text layouts: requests to the text worker (or inline shaping), their
//! delivery, retries, and pruning of slots and scales no surface wants.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use strand_scene::{NodeId, Scale, SurfaceId};
use strand_text::{TextEngine, TextError, TextKey, TextLayout, TextRequest, TextWorker};

use super::{MAX_TEXT_RETRIES, Renderer};
use crate::flatten::{Shaped, TextSpec};
use crate::raster::AtlasMirror;

/// Where text layouts come from.
#[derive(Debug)]
pub enum TextBackend {
    /// The text worker thread (the runtime configuration).
    Worker(TextWorker),
    /// Shape synchronously on the calling thread: deterministic, for
    /// offline rendering, tests and benchmarks.
    Inline(Box<TextEngine>),
}

impl TextBackend {
    /// Withdraws a request that is no longer wanted.
    pub(super) fn cancel(&self, key: TextKey) {
        if let TextBackend::Worker(w) = self {
            // A gone worker has nothing queued.
            let _ = w.cancel(key);
        }
    }

    /// Frees the glyph atlas for a scale no surface uses any more.
    pub(super) fn drop_scale(&mut self, scale: Scale) {
        match self {
            TextBackend::Worker(w) => {
                let _ = w.drop_scale(scale);
            }
            TextBackend::Inline(e) => e.drop_scale(scale),
        }
    }
}

/// What one text layout is shaped for: a node at one scale, in a line box
/// of one width. Alignment happens inside the line box, so a node shown on
/// two surfaces of the same scale but different widths needs two layouts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct TextSlot {
    pub(super) node: NodeId,
    pub(super) scale: Scale,
    /// `max_width` as bits (`None` when unbounded).
    pub(super) width: Option<u32>,
    /// See [`TextSpec::part`].
    pub(super) part: u8,
}

impl TextSlot {
    pub(super) fn of(node: NodeId, spec: &TextSpec) -> Self {
        Self {
            node,
            scale: spec.scale,
            width: spec.max_width.map(f32::to_bits),
            part: spec.part,
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct TextState {
    /// The layout being drawn (the last one delivered).
    pub(super) layout: Option<Arc<TextLayout>>,
    /// What `layout` was shaped from.
    pub(super) shaped: Option<TextSpec>,
    /// The request in flight, if any.
    pub(super) requested: Option<(TextKey, TextSpec)>,
    /// Shaping `shaped` crashed the text worker's engine: it is not asked
    /// for again (nothing is drawn) until the spec changes.
    pub(super) poisoned: bool,
    /// Retries left for an incomplete layout are `MAX_TEXT_RETRIES -
    /// retries`.
    pub(super) retries: u8,
}

impl TextState {
    /// The layout lacks glyphs for want of atlas room.
    pub(super) fn incomplete(&self) -> bool {
        self.layout.as_ref().is_some_and(|l| l.is_incomplete())
    }
}

impl Renderer {
    /// The text backend (the runtime's text worker, or inline shaping).
    pub fn text(&self) -> &TextBackend {
        &self.text
    }

    /// Text layouts kept or asked for, over all nodes, scales and widths
    /// (tests: a virtualised list shapes only the rows in view).
    pub fn text_slots(&self) -> usize {
        self.texts.len()
    }

    /// Bytes of atlas pixels the render thread mirrors for `scale`.
    pub fn atlas_mirror_bytes(&self, scale: Scale) -> usize {
        self.atlas.bytes(scale)
    }

    /// Drops text no surface wants any more: slots for a width or scale
    /// a surface has left. A slot drawn as a stand-in (its node's wanted
    /// slot has no layout yet) is kept until the wanted one arrives; a
    /// poisoned slot never gets one, so it keeps no stand-ins.
    ///
    /// A surface whose wanted slot has no layout may have drawn a dropped
    /// slot as its stand-in: it is marked dirty (its cache cleared) so it
    /// stops drawing it. Returns those surfaces.
    pub(super) fn prune_texts(&mut self) -> Vec<SurfaceId> {
        let mut keep: HashSet<TextSlot> = self.spec_wanted.values().flatten().copied().collect();
        let mut standing_in: HashSet<NodeId> = HashSet::new();
        for s in self.surfaces.values() {
            for slot in &s.wanted {
                keep.insert(*slot);
                if self
                    .texts
                    .get(slot)
                    .is_none_or(|t| t.layout.is_none() && !t.poisoned)
                {
                    standing_in.insert(slot.node);
                }
            }
        }
        let mut dropped: HashSet<NodeId> = HashSet::new();
        let text = &self.text;
        self.texts.retain(|slot, t| {
            let k = keep.contains(slot) || standing_in.contains(&slot.node);
            if !k {
                if let Some((key, _)) = t.requested.take() {
                    text.cancel(key);
                }
                if t.layout.is_some() {
                    dropped.insert(slot.node);
                }
            }
            k
        });
        let mut marked = Vec::new();
        if dropped.is_empty() {
            return marked;
        }
        let texts = &self.texts;
        self.pending.retain(|_, slot| texts.contains_key(slot));
        for (id, s) in &mut self.surfaces {
            let drew_stand_in = s.wanted.iter().any(|w| {
                dropped.contains(&w.node) && texts.get(w).is_none_or(|t| t.layout.is_none())
            });
            if drew_stand_in {
                s.mark_dirty();
                marked.push(*id);
            }
        }
        self.refresh_retries();
        marked
    }

    /// Frees everything held for scales no surface uses any more: text
    /// layouts, requests in flight, the atlas mirror's pages and the text
    /// worker's atlas. The worker and the mirror are dropped together, so
    /// a scale that comes back (a monitor replugged) re-rasterises and
    /// re-uploads its glyphs.
    ///
    /// A surface's previous scale counts as used until its text has been
    /// re-shaped at the new scale, so a rescale never blanks text.
    pub(super) fn prune_scales(&mut self) {
        let used: BTreeSet<Scale> = self
            .surfaces
            .values()
            .flat_map(|s| [Some(s.scale), s.prev_scale])
            .flatten()
            .collect();
        let gone: Vec<Scale> = self.text_scales.difference(&used).copied().collect();
        if gone.is_empty() {
            return;
        }
        for s in self.surfaces.values_mut() {
            s.cache = None;
        }
        for scale in gone {
            self.text_scales.remove(&scale);
            self.atlas.retain_scales(|s| s != scale);
            let text = &self.text;
            self.texts.retain(|slot, t| {
                if slot.scale == scale
                    && let Some((k, _)) = t.requested.take()
                {
                    text.cancel(k);
                }
                slot.scale != scale
            });
            self.pending.retain(|_, slot| slot.scale != scale);
            self.text.drop_scale(scale);
        }
    }

    /// Gives every incomplete layout its retries back and wakes its
    /// surfaces, after something happened that can free atlas pages (a
    /// layout replaced for new text, text removed, a scale dropped).
    /// Retries themselves never refresh, so this cannot loop.
    pub(super) fn refresh_retries(&mut self) {
        let mut roots = BTreeSet::new();
        for (slot, t) in self.texts.iter_mut() {
            if t.incomplete() {
                t.retries = 0;
                roots.extend(self.tree.root_of(slot.node));
            }
        }
        for s in self.surfaces.values_mut() {
            if roots.contains(&s.root) {
                s.mark_dirty();
            }
        }
    }

    /// True while text requests are in flight.
    pub fn text_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Blocks until every text request in flight has been delivered or
    /// `timeout` passes (offline rendering and tests). Returns true if
    /// nothing is pending any more.
    pub fn wait_for_text(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while !self.pending.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            let TextBackend::Worker(w) = &self.text else {
                break;
            };
            match w.recv_timeout(left) {
                Ok(Some(l)) => self.deliver(l),
                Ok(None) | Err(_) => break,
            }
            // A delivered layout can change sizes and so re-request.
            self.update();
        }
        self.pending.is_empty()
    }

    pub(super) fn poll_text(&mut self) {
        // Decoded images: the surfaces drawing them repaint.
        let arrived = self.extras.images.poll();
        if !arrived.is_empty() {
            let images = &self.extras.images;
            for (id, s) in self.surfaces.iter_mut() {
                if arrived.iter().any(|k| images.drawn_by(*id, k)) {
                    s.mark_dirty();
                }
            }
        }
        loop {
            let TextBackend::Worker(w) = &self.text else {
                return;
            };
            match w.try_recv() {
                Ok(Some(l)) => self.deliver(l),
                Ok(None) => return,
                Err(TextError::WorkerGone | TextError::Spawn(_)) => {
                    // Keep drawing the last layouts; nothing more will come.
                    self.pending.clear();
                    return;
                }
            }
        }
    }

    pub(super) fn deliver(&mut self, layout: TextLayout) {
        if layout.is_reset() {
            // The request answered by the reset crashed the engine: keep
            // it from being asked for again, or the worker would restart
            // its engine and every text would reshape, forever.
            let culprit = self.pending.get(&layout.key).and_then(|slot| {
                let (_, spec) = self.texts.get(slot)?.requested.as_ref()?;
                Some((*slot, spec.clone()))
            });
            self.reset_text();
            if let Some((slot, spec)) = culprit {
                self.texts.insert(
                    slot,
                    TextState {
                        shaped: Some(spec),
                        poisoned: true,
                        ..TextState::default()
                    },
                );
            }
            return;
        }
        // Atlas uploads apply even when the layout itself is stale, except
        // for scales already pruned (their pages would never be freed).
        if self.text_scales.contains(&layout.scale) {
            for up in &layout.uploads {
                if up.page.scale == layout.scale {
                    self.atlas.apply(up);
                }
            }
            // Pages the worker trimmed or reset since are dropped.
            if let Some(live) = layout.atlas_pages() {
                self.atlas.retain_pages(layout.scale, live);
            }
        }
        let Some(slot) = self.pending.remove(&layout.key) else {
            return;
        };
        let Some(state) = self.texts.get_mut(&slot) else {
            return;
        };
        if state.requested.as_ref().map(|(k, _)| *k) != Some(layout.key) {
            return;
        }
        let mut replaced = false;
        if let Some((_, spec)) = state.requested.take() {
            // A retry (same spec) does not refresh anyone's retries.
            replaced = state.shaped.as_ref() != Some(&spec);
            if replaced {
                state.retries = 0;
            }
            state.shaped = Some(spec);
            state.poisoned = false;
            state.layout = Some(Arc::new(layout));
        }
        if replaced {
            // The layout it replaced released its atlas pages.
            self.refresh_retries();
        }
        // Surfaces at other scales may draw it resampled meanwhile; its
        // size feeds layout, and a content-sized surface's size.
        let root = self.tree.root_of(slot.node);
        for s in self.surfaces.values_mut() {
            if Some(s.root) == root {
                s.mark_layout();
            }
        }
        self.spec_dirty.extend(root);
    }

    /// The text worker restarted its engine: every mirrored page and every
    /// layout drawing from one is stale. Forget them all and re-request.
    pub(super) fn reset_text(&mut self) {
        self.atlas = AtlasMirror::default();
        let text = &self.text;
        self.texts.retain(|_, t| {
            if let Some((k, _)) = t.requested.take() {
                text.cancel(k);
            }
            // Earlier culprits stay poisoned.
            t.layout = None;
            t.poisoned
        });
        self.pending.clear();
        // Repaint in full, but keep showing the old frame (rather than one
        // without text) until the text is back, as for a first frame.
        let until = Instant::now() + self.first_frame_wait;
        for s in self.surfaces.values_mut() {
            s.valid = false;
            s.painted = false;
            s.wait_until = Some(until);
            s.mark_layout();
        }
    }

    /// Every delivered layout per node, with the width it was shaped for.
    /// `flatten` draws the one for the surface's scale and width or, while
    /// that is being shaped, a stand-in (resampled and re-aligned).
    pub(super) fn shaped(&self) -> HashMap<NodeId, Vec<Shaped>> {
        let mut out: HashMap<NodeId, Vec<Shaped>> = HashMap::new();
        for (slot, t) in &self.texts {
            let Some(l) = &t.layout else { continue };
            out.entry(slot.node).or_default().push(Shaped {
                layout: l.clone(),
                max_width: slot.width.map(f32::from_bits),
                part: slot.part,
            });
        }
        // Stand-in choice must not depend on hash order.
        for v in out.values_mut() {
            v.sort_by(|a, b| {
                (a.part, a.layout.scale, a.max_width.map(f32::to_bits)).cmp(&(
                    b.part,
                    b.layout.scale,
                    b.max_width.map(f32::to_bits),
                ))
            });
        }
        out
    }

    pub(super) fn request_text(&mut self, needs: &[(NodeId, TextSpec)]) -> bool {
        let mut delivered = false;
        for (node, spec) in needs {
            let slot = TextSlot::of(*node, spec);
            let state = self.texts.entry(slot).or_default();
            if state.requested.as_ref().is_some_and(|(_, s)| s == spec) {
                continue;
            }
            if state.shaped.as_ref() == Some(spec) {
                // Reverted to what is shown: a request in flight for
                // something else must not replace it on arrival.
                if let Some((old, _)) = state.requested.take() {
                    self.pending.remove(&old);
                    self.text.cancel(old);
                }
                // Glyphs left out for want of atlas room: ask again, a
                // bounded number of times.
                if !(state.incomplete() && state.retries < MAX_TEXT_RETRIES) {
                    continue;
                }
                state.retries += 1;
            }
            let key = TextKey(self.next_key);
            self.next_key += 1;
            if let Some((old, _)) = state.requested.take() {
                self.pending.remove(&old);
                self.text.cancel(old);
            }
            self.text_scales.insert(spec.scale);
            state.requested = Some((key, spec.clone()));
            let req = TextRequest {
                key,
                text: spec.text.clone(),
                style: spec.style.clone(),
                max_width: spec.max_width,
                scale: spec.scale,
            };
            match &mut self.text {
                TextBackend::Worker(w) => {
                    if w.request(req).is_ok() {
                        self.pending.insert(key, slot);
                    } else if let Some(state) = self.texts.get_mut(&slot) {
                        // No worker: keep the last layout, stop asking.
                        state.shaped = Some(spec.clone());
                        state.requested = None;
                        state.retries = MAX_TEXT_RETRIES;
                    }
                }
                TextBackend::Inline(engine) => {
                    let layout = engine.layout(&req);
                    self.pending.insert(key, slot);
                    self.deliver(layout);
                    delivered = true;
                }
            }
        }
        delivered
    }
}
