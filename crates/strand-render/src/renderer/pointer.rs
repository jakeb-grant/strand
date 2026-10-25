//! (flex) `Node.pointer` for logic: where the pointer is in the laid-out
//! box of each node logic reads it of (`Prop::Watch` with `pointer`),
//! handed over once a frame while it moves (decisions.md flex).
//!
//! The host gives the pointer as routing left it after each input event
//! ([`Renderer::pointer_input`]): its place on the surface, the hovered
//! chain (latched to the pressed one while the left button is held) and
//! whether the left button is held. A surface with a watched node then
//! wants a frame, and that frame's paint reports what changed
//! ([`Renderer::take_pointer_facts`]), so a fast mouse costs logic one
//! value a frame, not one an event. A press is reported at once, and the
//! host hands it over before the press's flags.
//!
//! A node's pointer is in its laid-out box: its paint offsets and
//! transforms, and its ancestors', are undone as the last frame drew
//! them (as a slider reads the pointer, [`Renderer::to_laid`]). While a
//! press that began on it is held, the frame is the one it had when the
//! press began, its surface's place included: a drag that moves the node
//! (its `x`, its layout) or its surface (margins from state) does not
//! move what the pointer is measured in, so it cannot feed back.

use std::collections::{HashMap, HashSet};

use strand_scene::{
    CursorShape, LogicalPoint, LogicalRect, NodeId, NodeKind, Prop, PropValue, SurfaceId,
};
use vello_cpu::kurbo;

use super::Renderer;

/// The pointer as each surface last saw it, and what was reported.
#[derive(Debug, Default)]
pub(super) struct Pointing {
    /// Nodes logic reads `pointer` of.
    watched: HashSet<NodeId>,
    /// Per surface the pointer was given for.
    over: HashMap<SurfaceId, Over>,
    /// Facts not yet taken.
    facts: Vec<(NodeId, Option<LogicalPoint>)>,
    /// The last value reported per node.
    sent: HashMap<NodeId, Option<LogicalPoint>>,
    /// (flex) The hovered chain per surface (latched to the pressed one
    /// while the left button is held), innermost first: what picks the
    /// cursor.
    chains: HashMap<SurfaceId, Vec<NodeId>>,
    /// (flex) Where the last press on each surface (by its root) went
    /// down, any button: where an `anchor: pointer` popup it opens opens.
    presses: HashMap<NodeId, PressAt>,
    /// (flex) Button presses and keys so far, on any surface: a press is
    /// the latest step while no other press or key came after it.
    steps: u64,
    /// (flex) The anchor each open `anchor: pointer` popup took when it
    /// opened, kept while it stays open, and the step of the press it
    /// took it from.
    anchors: HashMap<NodeId, (LogicalRect, u64)>,
}

/// (flex) A press as an `anchor: pointer` popup reads it.
#[derive(Debug, Clone)]
struct PressAt {
    at: LogicalPoint,
    /// The hit chain it went down on, innermost first.
    chain: Vec<NodeId>,
    /// Its step ([`Pointing::steps`]).
    step: u64,
    /// Its button came up.
    released: bool,
    /// A diff from logic was applied after its release: its click has
    /// been answered.
    answered: bool,
}

#[derive(Debug, Default)]
struct Over {
    /// The pointer on the surface (logical, as the compositor gave it),
    /// or `None` once it left.
    at: Option<LogicalPoint>,
    /// Where the surface lay on its output when `at` was given.
    placed: Option<LogicalPoint>,
    /// The hovered chain, innermost first.
    chain: Vec<NodeId>,
    /// The left button held since a press on the surface.
    press: Option<Press>,
    /// A frame is to report what changed.
    due: bool,
}

/// The frames of the watched nodes a press went down on, as they were.
#[derive(Debug)]
struct Press {
    placed: Option<LogicalPoint>,
    frames: HashMap<NodeId, Frame>,
}

impl Pointing {
    /// (flex) The anchor of `anchor: pointer` popup `popup`, hanging from
    /// `element` in the surface of `parent`: 1 × 1 where the press that
    /// opened it went down, kept while it stays open (`open`). A press
    /// opened it when it is the latest step (no press or key since) and
    /// went down on `element`; else (a key, a timer, a press elsewhere
    /// long before) it opens at its element: `None`. A later press on
    /// `element` moves it there once its click is answered with the
    /// popup still open, so a press that closes it does not move it
    /// first; presses inside the popup are on its own surface and never
    /// move it.
    pub(super) fn anchor(
        &mut self,
        popup: NodeId,
        element: Option<NodeId>,
        parent: Option<NodeId>,
        open: bool,
    ) -> Option<LogicalRect> {
        if !open {
            self.anchors.remove(&popup);
            return None;
        }
        let steps = self.steps;
        let fresh = parent
            .and_then(|p| self.presses.get(&p))
            .filter(|p| p.step == steps && element.is_some_and(|e| p.chain.contains(&e)));
        let take = match (self.anchors.get(&popup), fresh) {
            (None, Some(p)) => Some(p),
            (Some((_, from)), Some(p)) if p.step > *from && p.answered => Some(p),
            _ => None,
        };
        if let Some(p) = take {
            let rect = LogicalRect::new(p.at.x, p.at.y, 1.0, 1.0);
            self.anchors.insert(popup, (rect, p.step));
        }
        self.anchors.get(&popup).map(|(r, _)| *r)
    }

    /// (flex) The anchor an open `anchor: pointer` popup took, if it is
    /// one.
    pub(super) fn held_anchor(&self, popup: NodeId) -> Option<LogicalRect> {
        self.anchors.get(&popup).map(|(r, _)| *r)
    }

    /// (flex) Logic's diff is being applied: the click of each press
    /// released before it has been answered. The open `anchor: pointer`
    /// popups to look at again when one was answered just now.
    pub(super) fn diff_applied(&mut self) -> Vec<NodeId> {
        let mut any = false;
        for p in self.presses.values_mut() {
            any |= p.released && !p.answered;
            p.answered |= p.released;
        }
        if any {
            self.anchors.keys().copied().collect()
        } else {
            Vec::new()
        }
    }
}

/// Surface pixels into a node's laid-out box: `((inverse · p) − origin)
/// / k` for a point `p` in physical pixels.
#[derive(Clone, Copy, Debug)]
struct Frame {
    inverse: Option<kurbo::Affine>,
    origin: (f64, f64),
    k: f64,
}

impl Frame {
    fn local(&self, at: LogicalPoint) -> Option<LogicalPoint> {
        let mut p = kurbo::Point::new(at.x as f64 * self.k, at.y as f64 * self.k);
        if let Some(inv) = self.inverse {
            p = inv * p;
        }
        let x = (p.x - self.origin.0) / self.k;
        let y = (p.y - self.origin.1) / self.k;
        (x.is_finite() && y.is_finite()).then(|| LogicalPoint::new(x as f32, y as f32))
    }
}

/// Whether a `Prop::Watch` keyword asks for the pointer.
pub(super) fn watches_pointer(v: Option<&strand_scene::PropValue>) -> bool {
    matches!(v, Some(strand_scene::PropValue::Keyword(k)) if k.split('_').any(|p| p == "pointer"))
}

impl Renderer {
    /// (flex) The pointer on `surface` after an input event was routed:
    /// `at` (`None`: it left), the hovered chain (innermost first;
    /// latched to the pressed chain while the left button is held) and
    /// whether the left button is held there. A surface showing a node
    /// whose `pointer` logic reads wants a frame when it changed, and
    /// that frame reports it ([`Renderer::take_pointer_facts`]).
    pub fn pointer_input(
        &mut self,
        surface: SurfaceId,
        at: Option<LogicalPoint>,
        chain: &[NodeId],
        held: bool,
    ) {
        let Some(root) = self.surfaces.get(&surface).map(|s| s.root) else {
            return;
        };
        match self.pointing.chains.get_mut(&surface) {
            Some(c) if c.as_slice() == chain => {}
            Some(c) => {
                c.clear();
                c.extend_from_slice(chain);
            }
            None => {
                self.pointing.chains.insert(surface, chain.to_vec());
            }
        }
        let tree = &self.tree;
        let watched_here = self
            .pointing
            .watched
            .iter()
            .any(|n| tree.root_of(*n) == Some(root));
        if !watched_here {
            self.pointing.over.remove(&surface);
            return;
        }
        let placed = self.extras.placed.get(&root).map(|(_, p)| *p);
        let frames = match (held, self.pointing.over.get(&surface)) {
            // The press went down now: the frames it holds to.
            (true, o) if o.is_none_or(|o| o.press.is_none()) => Some(
                chain
                    .iter()
                    .filter(|n| self.pointing.watched.contains(n))
                    .filter_map(|n| Some((*n, self.pointer_frame(surface, *n)?)))
                    .collect::<HashMap<_, _>>(),
            ),
            _ => None,
        };
        let o = self.pointing.over.entry(surface).or_default();
        let mut due = o.at != at || o.chain != chain || o.placed != placed;
        o.at = at;
        o.placed = placed;
        if o.chain != chain {
            o.chain = chain.to_vec();
        }
        let began = frames.is_some();
        if let Some(frames) = frames {
            o.press = Some(Press { placed, frames });
            due = true;
        } else if !held && o.press.take().is_some() {
            due = true;
        }
        if due {
            o.due = true;
        }
        // Where a press went down is not left to the next frame: logic
        // hears it before the press itself.
        if began {
            self.report_pointer(surface);
        }
    }

    /// (flex) A button went down at `at` on `surface` (the host tells
    /// it before routing the press): a popup with `anchor: pointer` that
    /// the press opens opens there.
    pub fn pressed_at(&mut self, surface: SurfaceId, at: LogicalPoint) {
        self.pointing.steps += 1;
        if let Some(root) = self.surfaces.get(&surface).map(|s| s.root)
            && at.x.is_finite()
            && at.y.is_finite()
        {
            let chain = self.hit(surface, at);
            let step = self.pointing.steps;
            self.pointing.presses.insert(
                root,
                PressAt {
                    at,
                    chain,
                    step,
                    released: false,
                    answered: false,
                },
            );
        }
    }

    /// (flex) A button came up on `surface`: the press there is a click
    /// logic answers.
    pub fn released_at(&mut self, surface: SurfaceId) {
        if let Some(root) = self.surfaces.get(&surface).map(|s| s.root)
            && let Some(p) = self.pointing.presses.get_mut(&root)
        {
            p.released = true;
        }
    }

    /// (flex) A key went down: a press before it no longer opens an
    /// `anchor: pointer` popup at its point.
    pub fn key_step(&mut self) {
        self.pointing.steps += 1;
    }

    /// (flex) Whether `surface` owes logic the pointer (a frame reports
    /// it).
    pub(super) fn pointer_due(&self, surface: SurfaceId) -> bool {
        self.pointing.over.get(&surface).is_some_and(|o| o.due)
    }

    /// (flex) Pointer facts not yet handed to logic: `(node, where)` in
    /// the node's laid-out box, `None` once the pointer is not over it.
    pub fn take_pointer_facts(&mut self) -> Vec<(NodeId, Option<LogicalPoint>)> {
        std::mem::take(&mut self.pointing.facts)
    }

    /// (flex) The watched nodes changed (a diff set `Prop::Watch`): those
    /// reading `pointer` are reported from the next pointer event on.
    pub(super) fn watch_pointer(&mut self, ids: &[NodeId]) {
        for id in ids {
            let reads = self
                .tree
                .get(*id)
                .is_some_and(|n| watches_pointer(n.get(Prop::Watch)));
            if reads {
                self.pointing.watched.insert(*id);
            } else {
                self.pointing.watched.remove(id);
            }
        }
        let tree = &self.tree;
        self.pointing.watched.retain(|n| tree.contains_live(*n));
        self.pointing.sent.retain(|n, _| tree.contains_live(*n));
        self.pointing.anchors.retain(|n, _| tree.contains_live(*n));
        self.pointing.presses.retain(|n, _| tree.contains_live(*n));
    }

    /// (flex) `surface` went: the pointer over its nodes is gone too.
    pub(super) fn pointer_detached(&mut self, surface: SurfaceId, root: Option<NodeId>) {
        self.pointing.over.remove(&surface);
        self.pointing.chains.remove(&surface);
        let Some(root) = root else {
            return;
        };
        let tree = &self.tree;
        let gone: Vec<NodeId> = self
            .pointing
            .sent
            .iter()
            .filter(|(n, v)| v.is_some() && tree.root_of(**n) == Some(root))
            .map(|(n, _)| *n)
            .collect();
        for n in gone {
            self.pointing.sent.insert(n, None);
            self.pointing.facts.push((n, None));
        }
    }

    /// (flex) After `surface` painted: the pointer over its watched nodes
    /// that changed since the last report, if it owes one, or while the
    /// pointer rests on it: a frame that moved a node under a still
    /// pointer (a scroll, a layout change, an animation) moved where the
    /// pointer is in it. A surface paints only when something changed,
    /// so a still scene costs nothing.
    pub(super) fn report_pointer(&mut self, surface: SurfaceId) {
        let Some(root) = self.surfaces.get(&surface).map(|s| s.root) else {
            return;
        };
        let Some(o) = self
            .pointing
            .over
            .get(&surface)
            .filter(|o| o.due || o.at.is_some())
        else {
            return;
        };
        let placed_now = self.extras.placed.get(&root).map(|(_, p)| *p);
        let shift = |from: Option<LogicalPoint>, to: Option<LogicalPoint>| match (from, to) {
            (Some(a), Some(b)) => (a.x - b.x, a.y - b.y),
            _ => (0.0, 0.0),
        };
        let mut out = Vec::new();
        for &node in &self.pointing.watched {
            if self.tree.root_of(node) != Some(root) {
                continue;
            }
            let value = match o.at {
                Some(at) if o.chain.contains(&node) => {
                    match o
                        .press
                        .as_ref()
                        .and_then(|p| Some((p, p.frames.get(&node)?)))
                    {
                        // Held: in the frame the press began in, the
                        // surface's moves since undone.
                        Some((p, f)) => {
                            let (dx, dy) = shift(o.placed, p.placed);
                            f.local(LogicalPoint::new(at.x + dx, at.y + dy))
                        }
                        // Over it: where the surface is now.
                        None => {
                            let (dx, dy) = shift(o.placed, placed_now);
                            self.pointer_frame(surface, node)
                                .and_then(|f| f.local(LogicalPoint::new(at.x + dx, at.y + dy)))
                        }
                    }
                }
                _ => None,
            };
            if self.pointing.sent.get(&node) != Some(&value) {
                out.push((node, value));
            }
        }
        for (node, value) in out {
            self.pointing.sent.insert(node, value);
            self.pointing.facts.push((node, value));
        }
        if let Some(o) = self.pointing.over.get_mut(&surface) {
            o.due = false;
        }
    }

    /// (flex) The cursor the pointer should show on `surface`: the
    /// `cursor:` of the innermost node under it that has one (of the
    /// node a held press went down on, while it is held), `text` over an
    /// `input` that names none, else the default arrow. It follows the
    /// props as they are now, so `when pressed { cursor: grabbing }`
    /// shows once logic answers the press.
    pub fn cursor(&self, surface: SurfaceId) -> CursorShape {
        let Some(chain) = self.pointing.chains.get(&surface) else {
            return CursorShape::Default;
        };
        for n in chain {
            let Some(node) = self.tree.get(*n) else {
                continue;
            };
            if let Some(PropValue::Keyword(k) | PropValue::Text(k)) = node.get(Prop::Cursor)
                && let Some(shape) = CursorShape::from_name(k)
            {
                return shape;
            }
            if node.kind == NodeKind::Input {
                return CursorShape::Text;
            }
        }
        CursorShape::Default
    }

    /// `node`'s laid-out box on `surface` as its last frame drew it: the
    /// transform its hit shape was drawn through, else its box as laid
    /// out (a node with `hit: none` has no hit shape of its own).
    fn pointer_frame(&self, surface: SurfaceId, node: NodeId) -> Option<Frame> {
        let s = self.surfaces.get(&surface)?;
        let k = s.scale.as_f64();
        if let Some(h) = s.hits.iter().rev().find(|h| h.node == node) {
            return Some(Frame {
                inverse: h.inverse,
                origin: (h.rect.x0 + h.grow, h.rect.y0 + h.grow),
                k,
            });
        }
        let laid = self.node_rect(surface, node)?;
        Some(Frame {
            inverse: None,
            origin: (laid.x as f64 * k, laid.y as f64 * k),
            k,
        })
    }
}
