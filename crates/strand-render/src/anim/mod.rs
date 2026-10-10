//! Springs on the render thread (design.md, "Layout, animation and
//! input"): every visual prop moves to a new value along its transition,
//! `enter`/`exit` poses, FLIP glides and size springs.
//!
//! State is kept only while something moves: a node at rest has no entry.
//! What starts a motion:
//!
//! - logic set an animatable prop ([`Animator::touch`], recorded by
//!   `Renderer::apply` with the value it had, so the motion starts there);
//! - a node created on a shown surface plays its `enter` pose
//!   ([`Animator::enter`]); a removed one, or a closing surface, plays its
//!   `exit` pose ([`Animator::exit`]);
//! - a structural change moves boxes: they glide from where they were
//!   ([`Animator::glide`]);
//! - `width`, `height` or `size` changed: the laid-out size springs
//!   ([`Animator::touch_size`]; see `Renderer`'s layout step).
//!
//! A target that changes for any other reason (a token swap, an inherited
//! colour) snaps at rest and steers a motion in flight. Paint at time zero
//! (a host with no clock), and `reduced_motion`, snap everything.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use strand_scene::{
    Color, Curve, LogicalRect, Motion, NodeId, Prop, PropValue, TokenScope, Transition,
};

use crate::tree::{Node, SceneTree};

mod keyframes;
mod morph;
mod motion;
mod pages;
mod pose;
mod sizes;
mod stagger;

pub(crate) use motion::Extents;
use motion::{PropMotion, decode, encode};
pub use pages::PageSwap;
pub(crate) use pages::slide as page_slide;
pub(crate) use pose::{exit_pose, is_pose, pose_props};

/// Props that spring between values; the others snap.
pub(crate) const ANIMATED: [Prop; 16] = [
    Prop::X,
    Prop::Y,
    Prop::Opacity,
    Prop::Scale,
    Prop::Rotate,
    Prop::Bg,
    Prop::Color,
    Prop::Border,
    Prop::Shadow,
    Prop::Radius,
    // Widgets: a meter's or slider's fill and its track's colour.
    Prop::Value,
    Prop::Track,
    // (M4) Effects: a stroke (width and solid paint), its trim, a wave's
    // amplitude (a wavy meter flattens when paused), a glow.
    Prop::Stroke,
    Prop::Trim,
    Prop::Wave,
    Prop::Glow,
];

/// Props whose change springs the laid-out size.
pub(crate) fn is_size(p: Prop) -> bool {
    matches!(p, Prop::Width | Prop::Height | Prop::Size)
}

/// Settling tolerance per prop, in its own units.
fn eps(p: Prop) -> f32 {
    match p {
        Prop::Opacity => 0.002,
        Prop::Scale => 0.0005,
        Prop::Rotate => 0.05,
        Prop::Bg | Prop::Color | Prop::Track => 0.002,
        Prop::Value => 0.0005,
        _ => 0.05,
    }
}

/// Settling tolerance of laid-out sizes and glides, logical pixels.
const PX_EPS: f32 = 0.05;

/// A size forced on a node for one layout pass.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub(crate) struct Forced {
    /// `[width, height]` in logical pixels.
    pub size: [Option<f32>; 2],
    /// Per axis: the size moves to or from zero (a toast's `exit {
    /// height: 0 }`). Its padding and the parent's gap beside it fold
    /// with it, so its slot reaches zero as the spring settles and
    /// unmounting it moves nothing.
    pub collapse: [bool; 2],
}

/// Laid-out sizes forced on nodes for one layout pass.
pub(crate) type SizeMap = HashMap<NodeId, Forced>;

/// A size this small is zero: its slot collapses.
const COLLAPSED: f32 = 0.5;

#[derive(Debug, Default)]
struct NodeAnim {
    props: Vec<(Prop, PropMotion)>,
    /// Props logic changed since the node was last drawn, with the value
    /// each had before (`None`: unset).
    touched: Vec<(Prop, Option<PropValue>)>,
    /// Every prop whose target changes springs to it (a pose started or
    /// was called off).
    respring: bool,
    glide: Option<Motion<2>>,
    size: [Option<Motion<1>>; 2],
    /// Per axis: the size spring moves to or from zero ([`Forced`]).
    collapse: [bool; 2],
    /// `width`, `height` or `size` changed: the laid-out size springs to
    /// its new value.
    size_touched: bool,
}

impl NodeAnim {
    fn is_empty(&self) -> bool {
        self.props.is_empty()
            && self.touched.is_empty()
            && self.glide.is_none()
            && self.size.iter().all(Option::is_none)
            && !self.size_touched
    }
}

/// Why a node plays its exit pose.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExitKind {
    /// Removed by logic: a ghost, unmounted once the pose settles.
    Ghost,
    /// A surface whose `open` went false: it closes once the pose settles.
    Close,
}

/// Every spring the render thread owns.
#[derive(Debug, Default)]
pub(crate) struct Animator {
    reduced: bool,
    /// The frame being drawn: its time, whether it is painted (samples
    /// start motions and settled ones are dropped) or only previewed.
    time: Duration,
    commit: bool,
    /// The previous frame of the surface being drawn: a change starts at
    /// most one frame before the frame that first shows it.
    prev: Option<Duration>,
    /// Something drawn since [`Animator::begin`] is still moving.
    active: bool,
    nodes: HashMap<NodeId, NodeAnim>,
    /// Nodes that play their `enter` pose when first drawn (paint props)
    /// and laid out (sizes).
    enter: HashSet<NodeId>,
    enter_size: HashSet<NodeId>,
    exits: HashMap<NodeId, ExitKind>,
    /// When each exit started (wall clock): an exit no frame samples
    /// (its output asleep) is ended by `Renderer` after a while.
    exit_started: HashMap<NodeId, Instant>,
    /// Exiting nodes drawn since [`Animator::begin`].
    drawn: HashSet<NodeId>,
    finished: Vec<(NodeId, ExitKind)>,
    /// (M4) When each node that reads time appeared ([`crate::time`]).
    times: crate::time::NodeTimes,
    /// (M4) Half a frame of the surface being drawn: how early a capped
    /// clock's tick counts as reached ([`crate::clock`]).
    slack: Duration,
    /// Poses render chose for nodes in place of their own `enter`/`exit`
    /// (a page's directional slide): read when the enter starts and on
    /// every frame of the exit.
    poses: HashMap<NodeId, PropValue>,
    /// `pages` swaps (directional page transitions).
    pub pages: pages::PageSwaps,
    /// (M4) Shaped nodes' shapes and morphs (`crate::shapes`).
    shapes: crate::shapes::morph::Morphs,
    /// (M4) Rolling texts' layouts and rolls (`crate::effects::roll`).
    rolls: crate::effects::roll::Rolls,
    /// (M4) Nodes' `play`s ([`keyframes`]).
    plays: keyframes::Plays,
    /// (M4) Children waiting for their turn to enter ([`stagger`]).
    staggers: stagger::Staggers,
    /// (M4) Pointer parallax and tilt (`crate::effects::lean`).
    leans: crate::effects::lean::Leans,
    /// (M4) Transition masks in flight (`crate::effects::transition`).
    reveals: crate::effects::transition::Reveals,
    /// (M4) Shared-element morphs ([`morph`]).
    shared: morph::SharedMorphs,
    /// (M4) Image swaps under a transition mask.
    image_swaps: crate::effects::transition::ImageSwaps,
}

impl Animator {
    pub fn set_reduced(&mut self, reduced: bool) {
        self.reduced = reduced;
    }

    pub fn reduced(&self) -> bool {
        self.reduced
    }

    /// Starts drawing a frame at `time` (`commit`: it will be painted).
    pub fn begin(&mut self, time: Duration, prev: Option<Duration>, commit: bool) {
        self.time = time;
        self.prev = prev.filter(|p| !p.is_zero());
        self.commit = commit;
        self.active = false;
        self.drawn.clear();
    }

    /// Something drawn since [`Animator::begin`] is still moving.
    pub fn active(&self) -> bool {
        self.active
    }

    /// (M4) Half a frame of the surface about to be drawn (see
    /// [`crate::clock`]).
    pub fn set_slack(&mut self, slack: Duration) {
        self.slack = slack;
    }

    /// (M4) The time context of `id` in the frame being drawn: its own
    /// `t` since it was first drawn, in whole ticks of `rate` when capped,
    /// frozen at `t = 0` under `reduced_motion` or in a frame with no
    /// clock. Also when the clock next ticks (`None`: every frame).
    pub fn time_of(
        &mut self,
        id: NodeId,
        rate: crate::clock::Rate,
    ) -> (strand_scene::TimeContext, Option<Duration>) {
        let frozen = self.snapping();
        let period = match rate {
            crate::clock::Rate::Refresh => None,
            crate::clock::Rate::Every(p) => Some(p),
        };
        // Read only: the clock starts once the node is drawn
        // ([`Animator::start_clock`]), so a node hidden at first counts
        // `t` from when it shows. Unstarted, it reads as starting now.
        self.times
            .context(id, self.time, false, frozen, period, self.slack)
    }

    /// (M4) `id` is drawn (or hidden only by something that follows
    /// time) in the frame being drawn: its clock starts here if this
    /// frame will be painted and the clocks are not frozen.
    pub fn start_clock(&mut self, id: NodeId) {
        if self.commit && !self.snapping() {
            self.times.begin(id, self.time);
        }
    }

    /// Everything snaps: `reduced_motion`, or a frame at time zero (no
    /// clock).
    fn snapping(&self) -> bool {
        self.reduced || self.time.is_zero()
    }

    /// (M4) The outline `id` draws for `shape` in a box of `aspect`: the
    /// shape, or while a change of it morphs (along `curve`), the points
    /// between (`crate::shapes::morph`).
    pub fn shape_outline(
        &mut self,
        id: NodeId,
        shape: crate::shapes::Shape,
        aspect: f64,
        curve: Curve,
    ) -> crate::shapes::Outline {
        let frame = crate::shapes::morph::Frame {
            at: self.time,
            commit: self.commit,
            prev: self.prev,
            snap: self.snapping(),
        };
        let (outline, moving) = self.shapes.outline(id, shape, aspect, curve, frame);
        if moving {
            self.active = true;
        }
        outline
    }

    /// (M4) What a `roll: true` text `id`, laid out as `layout`, draws:
    /// `None` for `layout` as it is, or the roll from its last text
    /// (along `curve`): the old layout, the new one and the progress
    /// (`crate::effects::roll`).
    pub fn roll(
        &mut self,
        id: NodeId,
        layout: &std::sync::Arc<strand_text::TextLayout>,
        curve: Curve,
    ) -> Option<(
        std::sync::Arc<strand_text::TextLayout>,
        std::sync::Arc<strand_text::TextLayout>,
        f32,
    )> {
        let frame = crate::shapes::morph::Frame {
            at: self.time,
            commit: self.commit,
            prev: self.prev,
            snap: self.snapping(),
        };
        let (roll, moving) = self.rolls.roll(id, layout, curve, frame);
        if moving {
            self.active = true;
        }
        roll
    }

    /// (M4) The transition mask `node` draws under this frame, if any
    /// (`crate::effects::transition`): its own `transition:` as it enters
    /// or leaves, or its `pages` parent's as the pages swap. Called
    /// before [`Animator::paint`], which keeps a ghost the mask still
    /// needs.
    pub fn reveal(
        &mut self,
        tree: &SceneTree,
        node: &Node,
        scope: &TokenScope<'_>,
    ) -> Option<crate::effects::transition::Masked> {
        use crate::effects::transition::{Kind, Masked};
        let id = node.id;
        let kind_of = |n: &Node| {
            n.get(Prop::Transition)
                .and_then(|v| scope.resolve(v))
                .and_then(|v| Kind::of(&v))
        };
        let pages = node
            .parent
            .and_then(|p| tree.get(p))
            .filter(|p| p.kind == strand_scene::NodeKind::Pages)
            .and_then(|p| Some((p, kind_of(p)?)));
        let (holder, kind) = match pages {
            Some((p, k)) => (p, k),
            None if node.kind != strand_scene::NodeKind::Pages => match kind_of(node) {
                Some(k) => (node, k),
                None => {
                    self.reveals.forget(id);
                    return None;
                }
            },
            None => return None,
        };
        let frame = crate::shapes::morph::Frame {
            at: self.time,
            commit: self.commit,
            prev: self.prev,
            snap: self.snapping(),
        };
        if frame.snap {
            self.reveals.forget(id);
            return None;
        }
        let transition = holder
            .props
            .iter()
            .find(|e| e.prop == Prop::Transition)
            .map_or(Transition::Default, |e| e.transition.clone());
        // The prop is a snap: its default curve is `x`'s.
        let curve = Curve::of(&scope.transition(&transition, Prop::X));
        let entering = self.enter.contains(&id) || self.staggers.planned(id);
        let exiting = self.exits.contains_key(&id);
        let masked = |p: f32, invert: bool| Masked { kind, p, invert };
        if let Some((pages, _)) = pages {
            let swap = self.pages.last(pages.id);
            let index = |n: NodeId| pages.children.iter().position(|c| *c == n);
            let over = |a: NodeId, b: NodeId| index(a) > index(b);
            if exiting {
                let incoming = swap.and_then(|s| s.entering).filter(|e| *e != id);
                if let Some(e) = incoming {
                    // The old page plays out until the new one is in.
                    if self.enter.contains(&e) || self.staggers.planned(e) {
                        self.reveals.aim(e, 0.0, 1.0, curve, frame);
                    }
                    let p = self.reveals.peek(e, self.time);
                    let waits = self.reveals.moving(e, self.time);
                    self.reveals.hold(id, waits);
                    if waits {
                        self.active = true;
                    }
                    return over(id, e).then(|| masked(p.unwrap_or(1.0), true));
                }
            } else {
                if entering {
                    self.reveals.aim(id, 0.0, 1.0, curve, frame);
                }
                let (p, moving) = self.reveals.progress(id, frame)?;
                if moving {
                    self.active = true;
                }
                // Under the old page, the old page carries the mask.
                let ghost = swap
                    .and_then(|s| s.leaving)
                    .filter(|g| *g != id && tree.is_ghost(*g));
                return match ghost {
                    Some(g) if over(g, id) => None,
                    _ => Some(masked(p, false)),
                };
            }
        }
        if entering && !exiting {
            self.reveals.aim(id, 0.0, 1.0, curve, frame);
        }
        if exiting {
            self.reveals.aim(id, 1.0, 0.0, curve, frame);
        }
        let Some((p, moving)) = self.reveals.progress(id, frame) else {
            self.reveals.hold(id, false);
            return None;
        };
        if moving {
            self.active = true;
        }
        self.reveals.hold(id, exiting && moving);
        Some(masked(p, false))
    }

    /// (M4) An `image` with `transition:` showing `source` (`ready`:
    /// decoded): the source it swaps from and the mask the new one comes
    /// in through, while it swaps (`crate::effects::transition`).
    pub fn image_swap(
        &mut self,
        node: &Node,
        source: &str,
        ready: bool,
        scope: &TokenScope<'_>,
    ) -> Option<(String, crate::effects::transition::Masked)> {
        use crate::effects::transition::{Kind, Masked};
        let kind = node
            .get(Prop::Transition)
            .and_then(|v| scope.resolve(v))
            .and_then(|v| Kind::of(&v));
        let Some(kind) = kind else {
            self.image_swaps.forget(node.id);
            return None;
        };
        let transition = node
            .props
            .iter()
            .find(|e| e.prop == Prop::Transition)
            .map_or(Transition::Default, |e| e.transition.clone());
        let curve = Curve::of(&scope.transition(&transition, Prop::X));
        let frame = crate::shapes::morph::Frame {
            at: self.time,
            commit: self.commit,
            prev: self.prev,
            snap: self.snapping(),
        };
        let (swap, moving) = self.image_swaps.swap(node.id, source, ready, curve, frame);
        if moving {
            self.active = true;
        }
        swap.map(|(old, p)| {
            (
                old,
                Masked {
                    kind,
                    p,
                    invert: false,
                },
            )
        })
    }

    /// (M4) `node`'s shared-element morph ([`morph`]): laid out at `rect`
    /// (paint offsets included) on the surface of `root`, how far it is
    /// drawn from there (`[dx, dy, sx, sy]`), if it morphs. A node that
    /// starts a morph plays it in place of its enter pose. Called before
    /// [`Animator::paint`].
    pub fn shared_morph(
        &mut self,
        node: &Node,
        scope: &TokenScope<'_>,
        root: NodeId,
        rect: LogicalRect,
    ) -> Option<[f32; 4]> {
        let key = match node
            .get(Prop::Morph)
            .and_then(|v| scope.resolve(v))
            .as_deref()
        {
            Some(PropValue::Text(k) | PropValue::Keyword(k)) if !k.is_empty() => k.clone(),
            _ => {
                self.shared.forget(node.id);
                return None;
            }
        };
        let transition = node
            .props
            .iter()
            .find(|e| e.prop == Prop::Morph)
            .map_or(Transition::Default, |e| e.transition.clone());
        let curve = Curve::of(&scope.transition(&transition, Prop::X));
        let frame = crate::shapes::morph::Frame {
            at: self.time,
            commit: self.commit,
            prev: self.prev,
            snap: self.snapping(),
        };
        let entering = self.enter.contains(&node.id);
        let (v, moving, started) = self
            .shared
            .morph(node.id, &key, root, rect, entering, curve, frame);
        if started {
            // In place of its enter pose.
            self.enter.remove(&node.id);
            self.staggers.forget(node.id);
        }
        if moving {
            self.active = true;
        }
        v
    }

    /// (M4) Leans `node` with the pointer (`pointer`, `None` off its
    /// surface) by its `parallax` and `tilt` in `props`, laid out at
    /// `rect` on a surface `surface` (`crate::effects::lean`): adds the
    /// springing offset and turn to its `x`, `y` and `rotate`.
    #[allow(clippy::too_many_arguments)]
    pub fn lean(
        &mut self,
        node: &Node,
        props: &mut Vec<(Prop, Cow<'_, PropValue>)>,
        scope: &TokenScope<'_>,
        inh: Color,
        pointer: Option<strand_scene::LogicalPoint>,
        rect: LogicalRect,
        surface: LogicalRect,
    ) {
        let get = |p: Prop| props.iter().find(|(q, _)| *q == p).map(|(_, v)| v.as_ref());
        let Some(lean) = crate::effects::lean::Lean::of(get) else {
            self.leans.forget(node.id);
            return;
        };
        let target = lean.target(pointer, rect, surface);
        let transition = node
            .props
            .iter()
            .find(|e| matches!(e.prop, Prop::Parallax | Prop::Tilt))
            .map_or(Transition::Default, |e| e.transition.clone());
        let curve = Curve::of(&scope.transition(&transition, Prop::Parallax));
        let frame = crate::shapes::morph::Frame {
            at: self.time,
            commit: self.commit,
            prev: self.prev,
            snap: self.snapping(),
        };
        let (v, moving) = self.leans.sample(node.id, target, curve, frame);
        if moving {
            self.active = true;
        }
        let boxes = Extents {
            own: (rect.w, rect.h),
            parent: (surface.w, surface.h),
        };
        keyframes::offset(props, Prop::X, v[0], inh, boxes);
        keyframes::offset(props, Prop::Y, v[1], inh, boxes);
        keyframes::offset(props, Prop::Rotate, v[2], inh, boxes);
    }

    /// (M4) True if a node `under` a surface leans with the pointer: a
    /// pointer motion there repaints it.
    pub fn leans(&self, under: impl FnMut(NodeId) -> bool) -> bool {
        self.leans.used(under)
    }

    /// (M4) If `node` is about to enter under a parent with `stagger:`,
    /// numbers it and the siblings entering with it ([`stagger`]).
    /// Called before [`Animator::paint`].
    pub fn stagger(&mut self, tree: &SceneTree, node: &Node, scope: &TokenScope<'_>) {
        if self.snapping() || !self.enter.contains(&node.id) || self.staggers.planned(node.id) {
            return;
        }
        let Some(parent) = node.parent.and_then(|p| tree.get(p)) else {
            return;
        };
        let step = match parent
            .get(Prop::Stagger)
            .and_then(|v| scope.resolve(v))
            .as_deref()
        {
            Some(PropValue::Duration(d)) if !d.is_zero() => *d,
            _ => return,
        };
        let enter = &self.enter;
        let entering = parent
            .children
            .iter()
            .copied()
            .filter(|c| enter.contains(c));
        let entering: Vec<NodeId> = entering.collect();
        self.staggers.plan(entering.into_iter(), step, self.time);
    }

    /// (M4) Draws node `node`'s `play` (`Prop::Play` in `props`, which
    /// already hold this frame's springs) over `props`
    /// ([`keyframes`]). `inh`, `rect` and `parent` as for
    /// [`Animator::paint`].
    pub fn keyframes(
        &mut self,
        node: &Node,
        props: &mut Vec<(Prop, Cow<'_, PropValue>)>,
        inh: Color,
        rect: Option<LogicalRect>,
        parent: LogicalRect,
    ) {
        let Some(PropValue::Keyframes(k)) = props
            .iter()
            .find(|(q, _)| *q == Prop::Play)
            .map(|(_, v)| v.as_ref())
        else {
            self.plays.forget(node.id);
            return;
        };
        let k = k.clone();
        let frame = crate::shapes::morph::Frame {
            at: self.time,
            commit: self.commit,
            prev: self.prev,
            snap: self.snapping(),
        };
        let (p, moving) = self.plays.progress(node.id, &k, frame);
        if moving {
            self.active = true;
        }
        if let Some(p) = p {
            let boxes = Extents {
                own: rect.map_or((0.0, 0.0), |r| (r.w, r.h)),
                parent: (parent.w, parent.h),
            };
            keyframes::apply(&k, p, props, inh, boxes);
        }
    }

    /// (M4) `id` does not roll (any more).
    pub fn forget_roll(&mut self, id: NodeId) {
        self.rolls.forget(id);
    }

    /// (M4) `id` draws no shape (any more).
    pub fn forget_shape(&mut self, id: NodeId) {
        self.shapes.forget(id);
    }

    /// Logic set `prop` of `id`, which had the value `old`.
    pub fn touch(&mut self, id: NodeId, prop: Prop, old: Option<PropValue>) {
        let na = self.nodes.entry(id).or_default();
        if !na.touched.iter().any(|(p, _)| *p == prop) {
            na.touched.push((prop, old));
        }
    }

    /// Logic set `width`, `height` or `size` of `id`.
    pub fn touch_size(&mut self, id: NodeId) {
        self.nodes.entry(id).or_default().size_touched = true;
    }

    pub fn enter(&mut self, id: NodeId) {
        self.enter.insert(id);
        self.enter_size.insert(id);
        // A surface opened again: props held at its exit pose spring back.
        if let Some(na) = self.nodes.get_mut(&id) {
            na.respring = true;
        }
    }

    /// `id` plays its exit pose.
    pub fn exit(&mut self, id: NodeId, kind: ExitKind) {
        self.enter.remove(&id);
        self.enter_size.remove(&id);
        self.exits.insert(id, kind);
        self.exit_started.insert(id, Instant::now());
        let na = self.nodes.entry(id).or_default();
        na.respring = true;
        na.size_touched = true;
    }

    /// A closing surface opened again: it springs back to its props.
    pub fn cancel_exit(&mut self, id: NodeId) {
        if self.exits.remove(&id).is_some() {
            let na = self.nodes.entry(id).or_default();
            na.respring = true;
            na.size_touched = true;
        }
    }

    /// Nodes with motion state drawn since [`Animator::begin`].
    pub fn drawn(&self) -> &HashSet<NodeId> {
        &self.drawn
    }

    /// Every exit in flight.
    pub fn exits(&self) -> impl Iterator<Item = (NodeId, ExitKind)> + '_ {
        self.exits.iter().map(|(id, k)| (*id, *k))
    }

    pub fn exiting(&self, id: NodeId) -> Option<ExitKind> {
        self.exits.get(&id).copied()
    }

    /// Every exit in flight, with when it started.
    pub fn exit_times(&mut self) -> Vec<(NodeId, Instant)> {
        let exits = &self.exits;
        self.exit_started.retain(|id, _| exits.contains_key(id));
        self.exit_started.iter().map(|(id, t)| (*id, *t)).collect()
    }

    /// When the exit of `id` started, if it is exiting.
    pub fn exit_started(&self, id: NodeId) -> Option<Instant> {
        self.exits
            .contains_key(&id)
            .then(|| self.exit_started.get(&id).copied())
            .flatten()
    }

    /// Ends the exit of `id` now, as if its pose had settled.
    pub fn finish_now(&mut self, id: NodeId) {
        if let Some(k) = self.exits.remove(&id) {
            self.exit_started.remove(&id);
            self.finished.push((id, k));
        }
    }

    /// Drops every motion of `id` (its id now names another node).
    pub fn forget(&mut self, id: NodeId) {
        self.times.forget(id);
        self.shapes.forget(id);
        self.rolls.forget(id);
        self.plays.forget(id);
        self.staggers.forget(id);
        self.leans.forget(id);
        self.reveals.forget(id);
        self.shared.forget(id);
        self.image_swaps.forget(id);
        self.poses.remove(&id);
        self.nodes.remove(&id);
        self.enter.remove(&id);
        self.enter_size.remove(&id);
        self.exits.remove(&id);
        self.exit_started.remove(&id);
    }

    /// Drops the enter poses of nodes `under` the surface just painted
    /// that it did not draw (a row out of view, a subtree under a
    /// transparent parent): they show at rest when they come into view,
    /// and a pose never drawn keeps no frames coming.
    pub fn drop_undrawn_enters(&mut self, mut under: impl FnMut(NodeId) -> bool) {
        self.enter.retain(|id| !under(*id));
    }

    /// Exits that finished in the frames painted since the last call.
    pub fn take_finished(&mut self) -> Vec<(NodeId, ExitKind)> {
        for (id, _) in &self.finished {
            self.poses.remove(id);
        }
        std::mem::take(&mut self.finished)
    }

    /// `id` plays `pose` instead of its own `enter` (when its enter
    /// starts) or `exit` (while it exits). Its props show at their
    /// values meanwhile: a page sliding in is drawn whole (its colours
    /// do not also spring from the values they had before it existed).
    pub fn set_pose(&mut self, id: NodeId, pose: PropValue) {
        self.poses.insert(id, pose);
        if let Some(na) = self.nodes.get_mut(&id) {
            na.touched.clear();
        }
    }

    /// Ends the exits of nodes under `root` (by `root_of`) that the
    /// frame just painted did not draw: nobody sees them.
    pub fn finish_undrawn(&mut self, mut under: impl FnMut(NodeId) -> bool) {
        let gone: Vec<(NodeId, ExitKind)> = self
            .exits
            .iter()
            .filter(|(id, _)| !self.drawn.contains(id) && under(**id))
            .map(|(id, k)| (*id, *k))
            .collect();
        for (id, k) in gone {
            self.exits.remove(&id);
            self.finished.push((id, k));
        }
        // Paint state of nodes the frame did not draw (out of view, under
        // a transparent parent) is dropped: they show at rest when they
        // come back. Size springs belong to layout and stay.
        let drawn = &self.drawn;
        for (id, na) in self.nodes.iter_mut() {
            if !drawn.contains(id) && under(*id) {
                na.props.clear();
                na.touched.clear();
                na.respring = false;
                na.glide = None;
            }
        }
        self.nodes.retain(|_, n| !n.is_empty());
    }

    /// Drops the state of nodes `keep` rejects (gone from the tree).
    pub fn retain(&mut self, mut keep: impl FnMut(NodeId) -> bool) {
        self.times.retain(&mut keep);
        self.shapes.retain(&mut keep);
        self.rolls.retain(&mut keep);
        self.plays.retain(&mut keep);
        self.staggers.retain(&mut keep);
        self.leans.retain(&mut keep);
        self.reveals.retain(&mut keep);
        self.shared.retain(&mut keep);
        self.image_swaps.retain(&mut keep);
        self.nodes.retain(|id, _| keep(*id));
        self.enter.retain(|id| keep(*id));
        self.enter_size.retain(|id| keep(*id));
        self.exits.retain(|id, _| keep(*id));
        let exits = &self.exits;
        self.exit_started.retain(|id, _| exits.contains_key(id));
    }

    /// A FLIP: `id` is drawn `delta` away from its new box, then glides
    /// there.
    pub fn glide(&mut self, id: NodeId, delta: [f32; 2], curve: Curve) {
        if self.reduced {
            return;
        }
        let last = self.prev;
        self.nodes
            .entry(id)
            .or_default()
            .glide
            .get_or_insert_with(|| Motion::rest([0.0, 0.0], PX_EPS).sampled_at(last))
            .shift(delta, curve);
    }

    /// The glide offset of `id` in this frame.
    pub fn offset(&mut self, id: NodeId) -> (f32, f32) {
        let (at, commit, snap) = (self.time, self.commit, self.snapping());
        let Some(na) = self.nodes.get_mut(&id) else {
            return (0.0, 0.0);
        };
        let Some(g) = na.glide.as_mut() else {
            return (0.0, 0.0);
        };
        if snap {
            if commit {
                na.glide = None;
            }
            return (0.0, 0.0);
        }
        let v = if commit { g.sample(at) } else { g.peek(at) };
        if g.is_settled(at) {
            if commit {
                na.glide = None;
            }
        } else {
            self.active = true;
        }
        (v[0], v[1])
    }

    /// Springs the animatable props of `node` (resolved in `props`, which
    /// get the values of this frame), playing its enter or exit pose.
    /// `inh` is the colour it inherits; `rect` its laid-out box, `parent`
    /// its parent's (lengths resolve against them).
    pub fn paint(
        &mut self,
        node: &Node,
        props: &mut Vec<(Prop, Cow<'_, PropValue>)>,
        scope: &TokenScope<'_>,
        inh: Color,
        rect: Option<LogicalRect>,
        parent: LogicalRect,
    ) {
        let boxes = Extents {
            own: rect.map_or((0.0, 0.0), |r| (r.w, r.h)),
            parent: (parent.w, parent.h),
        };
        let id = node.id;
        let exiting = self.exits.get(&id).copied();
        // Drawn this frame: its motions (an enter pose that starts now
        // included) survive `finish_undrawn`.
        let waiting = self.staggers.planned(id);
        if exiting.is_some() || self.nodes.contains_key(&id) || self.enter.contains(&id) || waiting
        {
            self.drawn.insert(id);
        }
        if self.snapping() {
            if self.commit {
                if let Some(na) = self.nodes.get_mut(&id) {
                    na.props.clear();
                    na.touched.clear();
                    na.respring = false;
                }
                self.enter.remove(&id);
                self.staggers.forget(id);
                if let Some(k) = self.exits.remove(&id) {
                    self.finished.push((id, k));
                }
            }
            return;
        }
        // A staggered child stays entering (in `staggers`, not `enter`,
        // which frames clear) until its turn.
        let entering = self.enter.remove(&id) || waiting;
        if !entering && exiting.is_none() && !self.nodes.contains_key(&id) {
            return;
        }
        let resolve = |v: &PropValue| -> Vec<(Prop, PropValue)> {
            let v = scope
                .resolve(v)
                .map(Cow::into_owned)
                .unwrap_or(PropValue::Unset);
            pose_props(&v, rect)
                .into_iter()
                .filter_map(|(p, v)| Some((p, scope.resolve(&v)?.into_owned())))
                .collect()
        };
        let chosen = self.poses.get(&id);
        // A staggered child waiting for its turn holds at its pose.
        if entering && exiting.is_none() && self.staggers.held(id, self.time) {
            for (p, v) in chosen
                .or(node.get(Prop::Enter))
                .map(resolve)
                .unwrap_or_default()
            {
                match props.iter().position(|(q, _)| *q == p) {
                    Some(i) => props[i].1 = Cow::Owned(v),
                    None => props.push((p, Cow::Owned(v))),
                }
            }
            self.active = true;
            return;
        }
        let enter_pose = entering
            .then(|| chosen.or(node.get(Prop::Enter)).map(resolve))
            .flatten()
            .unwrap_or_default();
        let exit_pose = exiting
            .and_then(|_| chosen.or(exit_pose(node)).map(resolve))
            .unwrap_or_default();
        if entering && exiting.is_none() {
            self.poses.remove(&id);
        }
        let (at, commit, last) = (self.time, self.commit, self.prev);
        let na = self.nodes.entry(id).or_default();
        let touched = std::mem::take(&mut na.touched);
        let respring = std::mem::take(&mut na.respring);
        let mut moving = false;
        let mut exit_done = true;
        for p in ANIMATED {
            let own = props.iter().position(|(q, _)| *q == p);
            let own_v = own.map(|i| props[i].1.as_ref().clone());
            let in_exit = exit_pose.iter().find(|(q, _)| *q == p).map(|(_, v)| v);
            let target_v = in_exit.cloned().or_else(|| own_v.clone());
            let Some(target) = encode(p, target_v.as_ref(), inh, boxes) else {
                // Cannot interpolate: snaps.
                na.props.retain(|(q, _)| *q != p);
                continue;
            };
            let slot = na.props.iter().position(|(q, _)| *q == p);
            let touch = touched.iter().find(|(q, _)| *q == p);
            let transition = node
                .props
                .iter()
                .find(|e| e.prop == p)
                .map_or(Transition::Default, |e| e.transition.clone());
            let curve = Curve::of(&scope.transition(&transition, p));
            match slot {
                Some(i) => {
                    let m = &mut na.props[i].1;
                    if m.target() != target {
                        let springs = respring || touch.is_some() || !m.settled(at);
                        if !(springs && m.retarget(&target, curve, last)) {
                            *m = PropMotion::rest(p, &target, last);
                        }
                    }
                }
                None => {
                    // Where it starts: the enter pose, the value logic
                    // replaced, or (an exit) the node's own value.
                    let from = if let Some((_, v)) = enter_pose.iter().find(|(q, _)| *q == p) {
                        Some(encode(p, Some(v), inh, boxes))
                    } else if let Some((_, old)) = touch {
                        Some(encode(p, old.as_ref(), inh, boxes))
                    } else if in_exit.is_some() || respring {
                        Some(encode(p, own_v.as_ref(), inh, boxes))
                    } else {
                        None
                    };
                    if let Some(Some(from)) = from
                        && from != target
                    {
                        let mut m = PropMotion::rest(p, &from, last);
                        if !m.retarget(&target, curve, last) {
                            continue;
                        }
                        na.props.push((p, m));
                    } else {
                        continue;
                    }
                }
            }
            let Some(i) = na.props.iter().position(|(q, _)| *q == p) else {
                continue;
            };
            let enc = na.props[i].1.value(at, commit);
            let settled = na.props[i].1.settled(at);
            let value = decode(p, &enc);
            match own {
                Some(j) => props[j].1 = Cow::Owned(value),
                None => props.push((p, Cow::Owned(value))),
            }
            if settled {
                // A settled exit prop stays at its pose until the exit
                // ends (dropped, it would start over from the node's own
                // value).
                if commit && exiting.is_none() {
                    if let PropMotion::Shadows { m, len } = &mut na.props[i].1 {
                        m.truncate(*len);
                    }
                    na.props.swap_remove(i);
                }
            } else {
                moving = true;
                if in_exit.is_some() {
                    exit_done = false;
                }
            }
        }
        if exiting.is_some() && na.size.iter().any(Option::is_some) {
            exit_done = false;
        }
        // (M4) A ghost playing out under a transition mask.
        if self.reveals.holds(id) {
            exit_done = false;
        }
        if moving {
            self.active = true;
        }
        if commit
            && let Some(k) = exiting
            && exit_done
        {
            self.exits.remove(&id);
            self.finished.push((id, k));
        }
        if self.nodes.get(&id).is_some_and(NodeAnim::is_empty) {
            self.nodes.remove(&id);
        }
    }

    // ---- Laid-out sizes ----------------------------------------------

    /// Anything under `root` moving or about to: frames are wanted.
    pub fn busy(&self, tree: &SceneTree, root: NodeId) -> bool {
        let under = |id: &NodeId| tree.root_of(*id) == Some(root);
        self.nodes.keys().any(under)
            || self.enter.iter().any(under)
            || self.exits.keys().any(under)
            || self.shapes.pending(|id| under(&id))
            || self.rolls.pending(|id| under(&id))
            || self.plays.busy(|id| under(&id))
            || self.staggers.busy(|id| under(&id))
            || self.reveals.busy(self.time, |id| under(&id))
            || self.shared.busy(|id| under(&id))
            || self.image_swaps.busy(|id| under(&id))
    }
}

#[cfg(test)]
mod tests;
