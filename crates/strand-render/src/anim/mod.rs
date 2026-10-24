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

use strand_scene::motion::START_LEAD;
use strand_scene::{
    Color, Curve, LogicalRect, Motion, NodeId, Paint, Prop, PropValue, TokenScope, Transition,
};

use crate::tree::{Node, SceneTree};

pub(crate) mod keyframes;
mod morph;
mod motion;
mod pages;
mod pose;
mod sizes;
mod stagger;

pub(crate) use morph::MorphBox;
use morph::Start;
pub(crate) use motion::Extents;
use motion::{PropMotion, decode, encode, glow_color};
pub use pages::PageSwap;
pub(crate) use pages::slide as page_slide;
use pose::has_enter;
pub(crate) use pose::{exit_pose, is_pose, pose_props};

/// Props that spring between values; the others snap.
pub(crate) const ANIMATED: [Prop; 18] = [
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
    // amplitude (a wavy meter flattens when paused), a glow and a text's
    // solid `fill` (gradients still snap; `blur:` is the compositor's,
    // decisions.md m4-effects-paint-w2).
    Prop::Stroke,
    Prop::Trim,
    Prop::Wave,
    Prop::Glow,
    Prop::Fill,
    // (M4) A `shader` node's uniforms, channel by channel (a change of
    // names or kinds snaps; decisions.md m4-gpu-effects).
    Prop::Uniforms,
];

/// (graph-follow) A change of `prop` on a `kind` node springs: one of
/// [`ANIMATED`], but for a `graph`'s value, which `smooth` eases and
/// whose columns are its values at their ticks, not a spring's place at
/// whatever frame drew them (a replay draws other frames).
pub(crate) fn springs(kind: strand_scene::NodeKind, prop: Prop) -> bool {
    ANIMATED.contains(&prop) && !(kind == strand_scene::NodeKind::Graph && prop == Prop::Value)
}

/// (review) What a `value`'s settling tolerance is scaled by: a
/// `slider`'s, `meter`'s or `arc`'s range's span (`props` resolved, with
/// `drawn` the range an auto one last drew), 1 for any other node or a
/// span not known.
fn value_scale(
    node: &Node,
    props: &[(Prop, std::borrow::Cow<'_, PropValue>)],
    drawn: Option<&crate::range::Auto>,
) -> f32 {
    use strand_scene::NodeKind;
    if !matches!(
        node.kind,
        NodeKind::Slider | NodeKind::Meter | NodeKind::Arc
    ) {
        return 1.0;
    }
    let written = props
        .iter()
        .find(|(q, _)| *q == Prop::Range)
        .map(|(_, v)| v.as_ref());
    let range = match crate::range::Spec::of(written) {
        Some(s) if !s.is_auto() => Some(s.fixed()),
        _ => drawn.and_then(crate::range::Auto::drawn),
    };
    match range.map(|r| (r.hi - r.lo).abs()) {
        Some(span) if span.is_finite() && span > 0.0 => span,
        _ => 1.0,
    }
}

/// Props whose change springs the laid-out size.
pub(crate) fn is_size(p: Prop) -> bool {
    matches!(p, Prop::Width | Prop::Height | Prop::Size)
}

/// Settling tolerance per prop, in its own units: of a border's, a
/// stroke's or a glow's width (their colour channels settle by
/// [`COLOUR_EPS`], [`motion::PropMotion::rest`]).
fn eps(p: Prop) -> f32 {
    match p {
        Prop::Opacity => 0.002,
        Prop::Scale => 0.0005,
        Prop::Rotate => 0.05,
        Prop::Bg | Prop::Color | Prop::Track | Prop::Fill | Prop::Uniforms => COLOUR_EPS,
        Prop::Value => 0.0005,
        _ => 0.05,
    }
}

/// Settling tolerance of a colour channel (premultiplied OKLab plus
/// alpha): half a step of 1/255, so the snap that ends a fade moves
/// nothing the eye sees, wherever the colour is (a fill, or a border,
/// glow or shadow beside its geometry; decisions.md border-eps).
const COLOUR_EPS: f32 = 0.002;

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
    /// (bar-blank-on-exit) What a box an `enter` or `exit` pose squeezes
    /// on its width keeps of its layout at rest (where its enter lands,
    /// where its exit left from), so it never wraps into a column of
    /// letters taller than the row it sits in. On the squeezed box (its
    /// `size[0]` forced): a text's line box, the border-box width its
    /// lines are laid out and drawn for, clipped to the box. On each of
    /// its children (`size[0]` free): the width it has at rest, its
    /// least, so what it holds keeps its layout and the squeezed box
    /// clips it. `None`: laid out for its box.
    pub hold: Option<f32>,
    /// (growth) `[width, height]` a node keeps, its least and its most,
    /// while a content-sized surface's box springs over a step of its
    /// content: what the boxes filling that box hold keeps its layout at
    /// rest, cut to them (`Renderer::lay_out_stepped`).
    pub keep: [Option<f32>; 2],
}

/// (bar-blank-on-exit) What a node a pose squeezes on its width keeps
/// of its layout at rest: its own border-box width, and each child's.
#[derive(Clone, Debug, Default, PartialEq)]
struct LineHold {
    own: f32,
    children: Vec<(NodeId, f32)>,
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
    /// (growth) Per axis, on a content-sized surface's root: how far its
    /// box is from its content's size, springing to zero
    /// ([`Animator::step_size`]).
    step: [Option<Motion<1>>; 2],
}

/// Drops the paint state (the props' springs, logic's writes not yet
/// drawn, a glide) of the nodes `gone` picks: they show at rest when they
/// are next drawn. Size springs belong to layout and stay.
fn drop_paint_state(nodes: &mut HashMap<NodeId, NodeAnim>, mut gone: impl FnMut(NodeId) -> bool) {
    for (id, na) in nodes.iter_mut() {
        if gone(*id) {
            na.props.clear();
            na.touched.clear();
            na.respring = false;
            na.glide = None;
        }
    }
    nodes.retain(|_, n| !n.is_empty());
}

impl NodeAnim {
    fn is_empty(&self) -> bool {
        self.props.is_empty()
            && self.touched.is_empty()
            && self.glide.is_none()
            && self.size.iter().all(Option::is_none)
            && !self.size_touched
            && self.step.iter().all(Option::is_none)
    }
}

/// (growth) The boxes that follow a content-sized surface's box while it
/// springs over a step (`Renderer::lay_out_stepped`).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct StepFills {
    /// Per axis: the root and the boxes that follow it.
    pub chains: [HashSet<NodeId>; 2],
    /// Per axis: what they hold keeps to their far edge, the one the
    /// compositor keeps (a bottom OSD's rows to its bottom edge), and
    /// not to their near edge as it flows.
    pub far: [bool; 2],
}

impl StepFills {
    /// Whether `id` follows the box on either axis.
    pub fn contains(&self, id: &NodeId) -> bool {
        self.chains.iter().any(|c| c.contains(id))
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
    /// (unshaped-enter) Entering nodes whose pose's sizes wait for their
    /// text ([`Animator::start_sizes`]): laid out at those sizes until
    /// it is shaped, as their paint is held at the pose.
    size_held: HashMap<NodeId, [Option<f32>; 2]>,
    /// (cross-surface-morph) Surface roots opening: their next painted
    /// frame is the first that shows what they hold, and a node with a
    /// `morph` name in it comes on screen there as an entering node
    /// does, so it starts its shared morph ([`Animator::shared_morph`]).
    opened: HashSet<NodeId>,
    /// Nodes a shared morph took out of `enter` in a preview: a painted
    /// frame that misses the morph gives them their enter pose back.
    morph_took_enter: HashSet<NodeId>,
    exits: HashMap<NodeId, ExitKind>,
    /// When each exit started (wall clock): an exit no frame samples
    /// (its output asleep) is ended by `Renderer` after a while.
    exit_started: HashMap<NodeId, Instant>,
    /// (eval-r3) Exits a preview found at their pose with nothing left
    /// to move: they end with the next painted frame, which unmounts the
    /// ghost or closes the surface (a pose at the node's own values
    /// moves nothing, so its preview's scene matches the last frame's).
    exits_due: HashSet<NodeId>,
    /// (bar-blank-on-exit) What each node an `enter` or `exit` pose
    /// squeezes on its width keeps of its layout at rest ([`Forced::hold`]):
    /// set as the pose's size spring starts, kept while it moves or the
    /// node exits.
    line_holds: HashMap<NodeId, LineHold>,
    /// (growth) Per content-sized surface root whose box springs over a
    /// step: the boxes that follow its size in flight, per axis
    /// (`Renderer::lay_out_stepped`). Each clips what it holds while it
    /// does, as a box whose size springs does ([`Animator::sizing`]).
    step_fills: HashMap<NodeId, StepFills>,
    /// Exiting nodes drawn since [`Animator::begin`].
    drawn: HashSet<NodeId>,
    finished: Vec<(NodeId, ExitKind)>,
    /// (M4) When each node that reads time appeared ([`crate::time`]).
    times: crate::time::NodeTimes,
    /// (M4) Half a frame of the surface being drawn: how early a capped
    /// clock's tick counts as reached ([`crate::clock`]).
    slack: Duration,
    /// The frame period of the surface being drawn (zero: not known):
    /// how far before its first frame a change starts ([`Animator::lead`]).
    period: Duration,
    /// (graph-follow) The boot clock at a presentation time (`None`: the
    /// real one, [`crate::clock::boot_at`]).
    boot_clock: Option<crate::clock::BootClock>,
    /// (graph-follow) The boot clock less the presentation clock at the
    /// frame being drawn, in nanoseconds: aligned clocks tick on it.
    boot_offset: i128,
    /// (graph-follow) Each `meter`'s and `arc`'s `range: auto`, as the
    /// values it showed set it ([`crate::range::Auto`]).
    pub(crate) ranges: HashMap<NodeId, crate::range::Auto>,
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
    /// (late-text-dip) Entering nodes held at their pose while text
    /// under them is on its way: the frame each was first held.
    text_holds: HashMap<NodeId, Duration>,
    /// The ones held on the frame being drawn ([`Animator::hold_enter`]).
    text_held: HashSet<NodeId>,
    /// (M4) Pointer parallax and tilt (`crate::effects::lean`).
    leans: crate::effects::lean::Leans,
    /// (M4) Transition masks in flight (`crate::effects::transition`).
    reveals: crate::effects::transition::Reveals,
    /// (M4) Shared-element morphs ([`morph`]).
    shared: morph::SharedMorphs,
    /// (M4) Image swaps under a transition mask.
    image_swaps: crate::effects::transition::ImageSwaps,
    /// (M4) Drag and drop: nodes lifted off their box (a `drag:` source
    /// following the pointer, `Some` with its offset) or springing back
    /// to it (`None`); both paint above their siblings.
    lifted: HashMap<NodeId, Option<[f32; 2]>>,
    /// (M4) Jelly: dragged nodes' squash and stretch
    /// (`crate::effects::jelly`).
    jellies: crate::effects::jelly::Jellies,
    /// (eval-sandbox) Nodes whose enter pose started and still springs
    /// (`Animator::motions`): kept apart from `enter`, which the frame
    /// that starts the pose empties.
    #[cfg(feature = "eval-hooks")]
    entered: HashSet<NodeId>,
    /// (eval-rules-2) The samples the painted frame being drawn took of
    /// props' springs, sizes and glides (`crate::probe::SpringTrace`).
    #[cfg(feature = "eval-hooks")]
    pub(crate) springs: Vec<crate::probe::SpringTrace>,
}

/// (M4) A node's lift and glide, held aside by [`Animator::hold_still`].
pub(crate) struct Still {
    lift: Option<Option<[f32; 2]>>,
    glide: Option<Motion<2>>,
}

impl Animator {
    pub fn set_reduced(&mut self, reduced: bool) {
        self.reduced = reduced;
    }

    pub fn reduced(&self) -> bool {
        self.reduced
    }

    /// (graph-follow) Time aligned clocks and graphs on `clock` from now
    /// on (`crate::Renderer::set_history_clock`).
    pub fn set_boot_clock(&mut self, clock: crate::clock::BootClock) {
        self.boot_clock = Some(clock);
    }

    /// (graph-follow) The frame being drawn on the boot clock: what a
    /// `graph` lays its columns by. Zero in a frame with no clock (its
    /// time zero): it stands nowhere on the boot clock.
    pub fn frame_boot(&self) -> Duration {
        if self.time.is_zero() {
            return Duration::ZERO;
        }
        let ns = self.time.as_nanos() as i128 + self.boot_offset;
        Duration::from_nanos(u64::try_from(ns.max(0)).unwrap_or(u64::MAX))
    }

    /// (review) The frame being drawn will be painted (not a preview at
    /// the last painted frame's time).
    pub fn commits(&self) -> bool {
        self.commit
    }

    /// (review) How early a tick counts as reached: half a frame of the
    /// surface being drawn ([`Animator::set_period`]).
    pub fn slack(&self) -> Duration {
        self.slack
    }

    /// (graph-follow) The time of the frame being drawn.
    pub(crate) fn now(&self) -> Duration {
        self.time
    }

    /// (graph-follow) Time stands still in the frame being drawn
    /// (`reduced_motion`, or a frame with no clock).
    pub fn still(&self) -> bool {
        self.snapping()
    }

    /// Starts drawing a frame at `time` (`commit`: it will be painted).
    pub fn begin(&mut self, time: Duration, prev: Option<Duration>, commit: bool) {
        self.time = time;
        let boot = self.boot_clock.unwrap_or(crate::clock::boot_at)(time);
        self.boot_offset = boot.as_nanos() as i128 - time.as_nanos() as i128;
        self.prev = prev.filter(|p| !p.is_zero());
        self.commit = commit;
        self.active = false;
        self.drawn.clear();
        if commit {
            self.plays.begin(time);
            #[cfg(feature = "eval-hooks")]
            self.springs.clear();
        }
    }

    /// (eval-rules-2) The frame period changes start one of before the
    /// frame that first shows them ([`Animator::lead`] before its cap).
    #[cfg(feature = "eval-hooks")]
    pub(crate) fn period(&self) -> Duration {
        self.period
    }

    /// Something drawn since [`Animator::begin`] is still moving.
    pub fn active(&self) -> bool {
        self.active
    }

    /// The frame period of the surface about to be drawn (see
    /// [`crate::clock`]): half of it is how early a capped clock's tick
    /// counts as reached, and all of it how far before the first frame
    /// that shows a change the change starts ([`Animator::lead`]).
    pub fn set_period(&mut self, period: Duration) {
        self.period = period;
        self.slack = period / 2;
    }

    /// How far before the frame being drawn a change it first shows
    /// starts: one frame period of its surface (decisions.md,
    /// first-frame-lead), so that frame moves it as far as each frame
    /// after does, however long the surface sat idle and whichever
    /// backend paints it. At most [`START_LEAD`], which stands in for a
    /// period not known yet.
    pub(crate) fn lead(&self) -> Duration {
        if self.period.is_zero() {
            START_LEAD
        } else {
            self.period.min(START_LEAD)
        }
    }

    /// The frame being drawn, as the effects' motions see it.
    fn frame(&self) -> crate::shapes::morph::Frame {
        crate::shapes::morph::Frame {
            at: self.time,
            commit: self.commit,
            prev: self.prev,
            lead: self.lead(),
            snap: self.snapping(),
        }
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
            crate::clock::Rate::Aligned(p) => {
                return self.times.aligned_context(
                    id,
                    self.time,
                    false,
                    frozen,
                    p,
                    self.slack,
                    self.boot_offset,
                );
            }
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
        let frame = self.frame();
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
        let frame = self.frame();
        let (roll, moving) = self.rolls.roll(id, layout, curve, frame);
        if moving {
            self.active = true;
        }
        roll
    }

    /// (eval-rules-2) The roll of `id` a change interrupted in the frame
    /// being drawn, as it stood then (`crate::probe::RollDrawn::was`).
    #[cfg(feature = "eval-hooks")]
    pub(crate) fn roll_interrupted(&mut self, id: NodeId) -> Option<crate::effects::roll::Rolling> {
        self.rolls.take_interrupted(id)
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
        let frame = self.frame();
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

    /// (M4) An `image` with `transition:` showing `source` (`decode`:
    /// how far its decode is): the source it swaps from and the mask the new one comes
    /// in through, while it swaps (`crate::effects::transition`).
    pub fn image_swap(
        &mut self,
        node: &Node,
        source: &str,
        decode: crate::effects::transition::Decode,
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
        let frame = self.frame();
        if self.commit {
            // Drawn: a swap in flight survives `drop_undrawn_enters`.
            self.drawn.insert(node.id);
        }
        let (swap, moving) = self.image_swaps.swap(node.id, source, decode, curve, frame);
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
    /// (paint offsets included) on the surface of `root` (`origins`:
    /// where each surface lies on its output), how far it is drawn from there
    /// (`[dx, dy, sx, sy]`), if it morphs. A node that starts a morph
    /// plays it in place of its enter pose. A node with a `morph` name
    /// and a box of some area joins `drawn`, the scene's boxes its
    /// painted frames show ([`Animator::shared_painted`]). Called before
    /// [`Animator::paint`].
    pub(crate) fn shared_morph(
        &mut self,
        node: &Node,
        scope: &TokenScope<'_>,
        (root, rect, view, morphs): (
            NodeId,
            LogicalRect,
            vello_cpu::kurbo::Affine,
            vello_cpu::kurbo::Affine,
        ),
        origins: &morph::Origins,
        drawn: &mut Vec<MorphBox>,
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
        // (late-text-morph) A box of no area shows nothing (a node whose
        // text is not shaped yet takes no room, late-text-dip): its
        // frame does not put the name there, and the box it was last
        // seen at still starts morphs as one off screen does.
        if rect.w > 0.0 && rect.h > 0.0 {
            // (integration-r8) As the ancestors' transforms draw it.
            drawn.push(MorphBox {
                key: key.clone(),
                id: node.id,
                rect: morph::shown(view, rect).unwrap_or(rect),
            });
        }
        let frame = self.frame();
        // (cross-surface-morph) A node comes on screen when it enters, or
        // when the surface holding it opens: design.md's media pill grows
        // into the media panel as the panel opens.
        let entering = self.enter.contains(&node.id) || self.opened.contains(&root);
        let (v, moving, started) = self.shared.morph(
            node.id,
            &key,
            (root, origins),
            (rect, view, morphs),
            entering,
            curve,
            frame,
        );
        if started == Start::Missed && self.morph_took_enter.remove(&node.id) {
            // (shared-morph-on) A preview took its enter pose for a morph
            // the painted frame cannot start: it plays the pose after all.
            self.enter.insert(node.id);
        }
        if started == Start::Now {
            // In place of its enter pose. It is drawn for the first time
            // all the same: logic's writes that created it show at their
            // values, as [`Animator::paint`] shows an entering node's,
            // never springing from unset under the flight (its `x: 101`
            // from 0, moving where the morph starts: morph-cross-surface).
            if self.enter.remove(&node.id) && !frame.commit {
                self.morph_took_enter.insert(node.id);
            } else if frame.commit {
                self.morph_took_enter.remove(&node.id);
            }
            self.staggers.forget(node.id);
            if let Some(na) = self.nodes.get_mut(&node.id) {
                na.touched.clear();
            }
        }
        if moving {
            self.active = true;
        }
        v
    }

    /// (shared-morph-on) A frame of the surface of `root` painted at `at`
    /// shows the morph boxes `drawn`, from a scene flattened for it or
    /// reused ([`morph`]).
    pub(crate) fn shared_painted(&mut self, root: NodeId, at: Duration, drawn: &[MorphBox]) {
        self.shared.painted(root, at, drawn);
    }

    /// (shared-morph-on) No surface shows `root` any more: its morph
    /// boxes are off screen, (integration-r7) where it lay as it went
    /// (`place`).
    pub(crate) fn shared_hidden(
        &mut self,
        root: NodeId,
        place: Option<crate::flatten::SurfaceOrigin>,
        now: Instant,
    ) {
        self.shared.hidden(root, place, now);
    }

    /// (morph-memory) The frame begun is painted at moment `now`: morph
    /// boxes whose surface went before it are dated on its clock (with
    /// one, [`morph`]).
    pub(crate) fn date_hidden(&mut self, now: Instant) {
        if self.commit && !self.time.is_zero() {
            self.shared.date_left(self.time, now);
        }
    }

    /// (M4) Leans `node` with the pointer (`pointer`, `None` off its
    /// surface) by its `parallax` and `tilt` in `props`, laid out at
    /// `rect` on a surface `surface` (`crate::effects::lean`): adds the
    /// springing offset to its `x` and `y`, and unless `three_d` (the
    /// GPU's 3-D pass draws it) the turn to its `rotate` (the CPU's 2-D
    /// tilt). Returns the turn while it is turned: degrees for the
    /// pointer across and down its box.
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
        three_d: bool,
    ) -> Option<[f32; 2]> {
        let get = |p: Prop| props.iter().find(|(q, _)| *q == p).map(|(_, v)| v.as_ref());
        let Some(lean) = crate::effects::lean::Lean::of(get) else {
            self.leans.forget(node.id);
            return None;
        };
        let target = lean.target(pointer, rect, surface);
        let transition = node
            .props
            .iter()
            .find(|e| matches!(e.prop, Prop::Parallax | Prop::Tilt))
            .map_or(Transition::Default, |e| e.transition.clone());
        let curve = Curve::of(&scope.transition(&transition, Prop::Parallax));
        let frame = self.frame();
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
        if !three_d {
            keyframes::offset(props, Prop::Rotate, v[2], inh, boxes);
        }
        (v[2] != 0.0 || v[3] != 0.0).then_some([v[2], v[3]])
    }

    /// (M4) True if a node `under` a surface leans with the pointer: a
    /// pointer motion there repaints it.
    pub fn leans(&self, under: impl FnMut(NodeId) -> bool) -> bool {
        self.leans.used(under)
    }

    /// (M4) If `node` is about to enter under a parent with `stagger:`,
    /// numbers it and the siblings entering with it ([`stagger`]): only
    /// those with an `enter` (a pose that sets something, a preset, or a
    /// page's slide) take a turn (stagger); a child with none plays
    /// nothing, is drawn at rest at once and shifts no one's turn.
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
        let (enter, poses) = (&self.enter, &self.poses);
        let entering = parent
            .children
            .iter()
            .copied()
            .filter(|c| enter.contains(c) && (poses.contains_key(c) || has_enter(tree, *c)));
        let entering: Vec<NodeId> = entering.collect();
        self.staggers.plan(entering.into_iter(), step, self.time);
    }

    /// (M4) Draws node `node`'s `play` (`Prop::Play` in `props`, which
    /// already hold this frame's springs) over `props`
    /// ([`keyframes`]). `inh`, `rect` and `parent` as for
    /// [`Animator::paint`]. Returns the block and whether it is still
    /// playing; it wants frames only once the flattener finds the node
    /// drawn ([`Animator::play_drawn`]).
    pub fn keyframes(
        &mut self,
        node: &Node,
        props: &mut Vec<(Prop, Cow<'_, PropValue>)>,
        inh: Color,
        rect: Option<LogicalRect>,
        parent: LogicalRect,
    ) -> Option<(std::sync::Arc<strand_scene::Keyframes>, bool)> {
        let Some(PropValue::Keyframes(k)) = props
            .iter()
            .find(|(q, _)| *q == Prop::Play)
            .map(|(_, v)| v.as_ref())
        else {
            self.plays.forget(node.id);
            return None;
        };
        let k = k.clone();
        let frame = self.frame();
        let (p, moving) = self.plays.progress(node.id, &k, frame);
        if let Some(p) = p {
            let boxes = Extents {
                own: rect.map_or((0.0, 0.0), |r| (r.w, r.h)),
                parent: (parent.w, parent.h),
            };
            keyframes::apply(&k, p, props, inh, boxes);
        }
        Some((k, moving))
    }

    /// (M4) Node `id`, whose `play` is still playing, is drawn (or hidden
    /// only by something that follows time or the play itself): frames
    /// are wanted, and its surface is busy until the play ends.
    pub fn play_drawn(&mut self, id: NodeId) {
        self.active = true;
        if self.commit {
            self.plays.drawn(id, self.time);
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
        // Props it still holds spring from where they are drawn. A
        // surface that closed holds none (`surface_closed`): it enters
        // from its pose.
        if let Some(na) = self.nodes.get_mut(&id) {
            na.respring = true;
        }
    }

    /// (late-text-dip) True if `id`'s enter pose starts when it is next
    /// drawn (it is not exiting).
    pub(crate) fn enter_starts(&self, id: NodeId) -> bool {
        self.enter.contains(&id) && !self.exits.contains_key(&id)
    }

    /// (late-text-dip) Text under entering `id` has not arrived: its
    /// enter holds at its pose on the frame being drawn, so it plays
    /// from the first frame its text is drawn in rather than showing up
    /// partway through it. A hold lasts at most
    /// [`FIRST_FRAME_TEXT_WAIT`](crate::renderer::FIRST_FRAME_TEXT_WAIT) from its
    /// first frame, as a new surface's first frame waits for its text,
    /// so text that never comes (shaping that failed) holds nothing for
    /// long.
    pub(crate) fn hold_enter(&mut self, id: NodeId) {
        let since = *self.text_holds.entry(id).or_insert(self.time);
        if self.time < since.saturating_add(crate::renderer::FIRST_FRAME_TEXT_WAIT) {
            self.text_held.insert(id);
        }
    }

    /// `id` plays its exit pose.
    pub fn exit(&mut self, id: NodeId, kind: ExitKind) {
        self.enter.remove(&id);
        self.enter_size.remove(&id);
        self.size_held.remove(&id);
        self.exits.insert(id, kind);
        self.exit_started.insert(id, Instant::now());
        let na = self.nodes.entry(id).or_default();
        na.respring = true;
        na.size_touched = true;
    }

    /// A closing surface opened again: it springs back to its props.
    pub fn cancel_exit(&mut self, id: NodeId) {
        self.exits_due.remove(&id);
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
        self.end_exit(id);
    }

    /// The exit of `id` ends with the frame being painted. It stays
    /// exiting until [`Animator::take_finished`] hands it over to be
    /// unmounted (exit-height0): a frame may lay out and flatten again
    /// after the pass that ended it (text it waited for, a stand-in
    /// pruned), and every pass of that frame lays it out at its exit
    /// pose's size and draws it at its pose, as the pass that ended it
    /// did. Ended in an earlier pass, it laid out whole in the next and
    /// was drawn so for one frame.
    fn end_exit(&mut self, id: NodeId) {
        self.exits_due.remove(&id);
        if let Some(k) = self.exits.get(&id).copied()
            && !self.finished.iter().any(|(f, _)| *f == id)
        {
            self.finished.push((id, k));
        }
    }

    /// Drops every motion of `id` (its id now names another node).
    pub fn forget(&mut self, id: NodeId) {
        self.times.forget(id);
        self.ranges.remove(&id);
        self.shapes.forget(id);
        self.rolls.forget(id);
        self.plays.forget(id);
        self.staggers.forget(id);
        self.leans.forget(id);
        self.reveals.forget(id);
        self.shared.forget(id);
        self.image_swaps.forget(id);
        self.jellies.forget(id);
        self.poses.remove(&id);
        self.nodes.remove(&id);
        #[cfg(feature = "eval-hooks")]
        self.entered.remove(&id);
        self.enter.remove(&id);
        self.enter_size.remove(&id);
        self.size_held.remove(&id);
        self.opened.remove(&id);
        self.morph_took_enter.remove(&id);
        self.text_holds.remove(&id);
        self.line_holds.remove(&id);
        self.exits.remove(&id);
        self.exit_started.remove(&id);
        self.exits_due.remove(&id);
    }

    /// Drops the enter poses of nodes `under` the surface just painted
    /// that it did not draw (a row out of view, a subtree under a
    /// transparent parent): they show at rest when they come into view,
    /// and a pose never drawn keeps no frames coming. So do staggered
    /// children waiting for their turn (a list's rows past its viewport,
    /// numbered with the rows in view) and image swaps mid-wipe.
    pub fn drop_undrawn_enters(&mut self, mut under: impl FnMut(NodeId) -> bool) {
        // (late-text-dip) One drawn held at its pose for its text stays
        // entering.
        let (drawn, holds) = (&self.drawn, &self.text_holds);
        self.enter
            .retain(|id| drawn.contains(id) && holds.contains_key(id) || !under(*id));
        let enter = &self.enter;
        self.text_holds.retain(|id, _| enter.contains(id));
        let drawn = &self.drawn;
        self.staggers.retain(|id| drawn.contains(&id) || !under(id));
        self.image_swaps
            .end_undrawn(|id| !drawn.contains(&id) && under(id));
        // A morph a preview saw start whose painted frame did not draw
        // the node: it shows at rest, as an unseen enter pose does.
        self.shared.drop_pending(&mut under);
        self.morph_took_enter.retain(|id| !under(*id));
        // (roll-preview) Rolls and shapes a preview left to the painted
        // frame that it did not draw (a frame drawing them clears its
        // own): remembered when one draws them.
        self.rolls.drop_pending(&mut under);
        self.shapes.drop_pending(&mut under);
    }

    /// Exits that finished in the frames painted since the last call.
    pub fn take_finished(&mut self) -> Vec<(NodeId, ExitKind)> {
        for (id, _) in &self.finished {
            self.poses.remove(id);
            self.line_holds.remove(id);
            self.exits.remove(id);
            self.exit_started.remove(id);
            self.exits_due.remove(id);
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
        let gone: Vec<NodeId> = self
            .exits
            .keys()
            .filter(|id| !self.drawn.contains(id) && under(**id))
            .copied()
            .collect();
        for id in gone {
            self.end_exit(id);
        }
        // Paint state of nodes the frame did not draw (out of view, under
        // a transparent parent) is dropped: they show at rest when they
        // come back. Size springs belong to layout and stay.
        let drawn = &self.drawn;
        drop_paint_state(&mut self.nodes, |id| !drawn.contains(&id) && under(id));
        // So is a jelly the frame did not sample (jelly-drag review): a
        // held node a preview left pending, or one still moving, whose
        // ancestor a diff hid before the frame would leave its surface
        // busy until it was drawn again (no frame reuses its preview,
        // and a content-sized surface is held at its size). It comes
        // back at rest, measured from its next painted frame.
        self.jellies.retain(|id| drawn.contains(&id) || !under(id));
    }

    /// (popup-reopen) The surface of the nodes `under` it closed: what
    /// their props were drawn at goes with it, the poses its exit held
    /// included, and so does any motion still playing on it: a jelly
    /// and a shared morph in flight or about to start (the boxes their
    /// names were drawn at stay, so a node elsewhere can still morph
    /// from one). Opened again, it is a new surface whose enter starts
    /// from its pose, as the first time; its nodes show at rest
    /// otherwise, and nothing left over keeps its root busy. Size springs
    /// belong to layout and stay.
    /// (cross-surface-morph) The surface of `root` opens: its next
    /// painted frame shows what it holds for the first time.
    pub(crate) fn surface_opened(&mut self, root: NodeId) {
        self.opened.insert(root);
    }

    /// (cross-surface-morph) The surface of `root` opened and has not
    /// painted since: its first frame flattens what it holds at that
    /// frame's time (a morph starts there), never reusing a scene a
    /// time-zero preview flattened.
    pub(crate) fn opening(&self, root: NodeId) -> bool {
        self.opened.contains(&root)
    }

    /// (cross-surface-morph) A frame of the surface of `root` was
    /// painted: what it holds is on screen.
    pub(crate) fn surface_shown(&mut self, root: NodeId) {
        self.opened.remove(&root);
    }

    pub fn surface_closed(&mut self, mut under: impl FnMut(NodeId) -> bool) {
        // (growth) A root's size step belongs to the box it was drawn
        // at: opened again, it shows at its content's size.
        for (id, na) in self.nodes.iter_mut() {
            if under(*id) {
                na.step = [None, None];
            }
        }
        self.step_fills.retain(|id, _| !under(*id));
        drop_paint_state(&mut self.nodes, &mut under);
        self.jellies.retain(|id| !under(id));
        self.shared.stop(&mut under);
    }

    /// Drops the state of nodes `keep` rejects (gone from the tree).
    pub fn retain(&mut self, mut keep: impl FnMut(NodeId) -> bool) {
        self.lifted.retain(|id, _| keep(*id));
        self.step_fills.retain(|id, _| keep(*id));
        self.ranges.retain(|id, _| keep(*id));
        self.times.retain(&mut keep);
        self.shapes.retain(&mut keep);
        self.rolls.retain(&mut keep);
        self.plays.retain(&mut keep);
        self.staggers.retain(&mut keep);
        self.leans.retain(&mut keep);
        self.reveals.retain(&mut keep);
        self.shared.retain(&mut keep);
        self.image_swaps.retain(&mut keep);
        self.jellies.retain(&mut keep);
        self.nodes.retain(|id, _| keep(*id));
        self.enter.retain(|id| keep(*id));
        self.enter_size.retain(|id| keep(*id));
        self.size_held.retain(|id, _| keep(*id));
        self.text_holds.retain(|id, _| keep(*id));
        self.exits.retain(|id, _| keep(*id));
        let exits = &self.exits;
        self.exit_started.retain(|id, _| exits.contains_key(id));
        self.exits_due.retain(|id| exits.contains_key(id));
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

    /// (M4) Lifts `id` off its box to `to` (a drag in flight: drawn
    /// there, above its siblings), or lets it go (`None`): it springs
    /// from where it was held back to its box along `curve`, or to the
    /// box it has by then (a drop that moved it).
    pub fn lift(&mut self, id: NodeId, to: Option<[f32; 2]>, curve: Curve) {
        match to {
            Some(v) => {
                self.lifted.insert(id, Some(v));
            }
            None => {
                if let Some(Some(v)) = self.lifted.get(&id).copied() {
                    self.lifted.insert(id, None);
                    self.glide(id, v, curve);
                }
            }
        }
    }

    /// (M4) The squash and stretch `node` draws (its `jelly`, `amount`)
    /// with its drawn `offset` (a lift's or the glide back): the
    /// deformation `crate::effects::jelly::matrix` turns into a
    /// transform, if any.
    pub fn jelly(
        &mut self,
        node: &Node,
        amount: f32,
        offset: (f32, f32),
        scope: &TokenScope<'_>,
    ) -> Option<[f32; 2]> {
        let held = matches!(self.lifted.get(&node.id), Some(Some(_)));
        let transition = node
            .props
            .iter()
            .find(|e| e.prop == Prop::Jelly)
            .map_or(Transition::Default, |e| e.transition.clone());
        let curve = match transition {
            Transition::Default => crate::effects::jelly::wobble(),
            t => Curve::of(&scope.transition(&t, Prop::Jelly)),
        };
        let frame = self.frame();
        if self.commit {
            // Drawn: its jelly survives `finish_undrawn`.
            self.drawn.insert(node.id);
        }
        let (d, moving) =
            self.jellies
                .sample(node.id, amount, [offset.0, offset.1], held, curve, frame);
        if moving {
            self.active = true;
        }
        d
    }

    /// (M4) Takes `id`'s lift and glide away, so it is drawn in its box
    /// ([`crate::Renderer::drag_image`]); [`Animator::release`] gives
    /// them back.
    pub(crate) fn hold_still(&mut self, id: NodeId) -> Still {
        Still {
            lift: self.lifted.remove(&id),
            glide: self.nodes.get_mut(&id).and_then(|n| n.glide.take()),
        }
    }

    /// (M4) Gives back what [`Animator::hold_still`] took.
    pub(crate) fn release(&mut self, id: NodeId, still: Still) {
        if let Some(l) = still.lift {
            self.lifted.insert(id, l);
        }
        if let (Some(g), Some(n)) = (still.glide, self.nodes.get_mut(&id)) {
            n.glide = Some(g);
        }
    }

    /// (eval-sandbox) What moves `id` as its state stands, and the axes
    /// of its own springs in flight (`x`, `y`, `opacity`, `scale`,
    /// `rotate`, and its laid-out size), read without sampling anything
    /// (`crate::probe`). `ENTER` holds from the frame an enter is asked
    /// for (or waits for its stagger) until its pose's springs settle,
    /// across every flatten of a frame (the first takes `enter`), and
    /// never under `reduced_motion`, which shows the node at once.
    #[cfg(feature = "eval-hooks")]
    pub(crate) fn motions(&self, id: NodeId) -> (crate::probe::Motions, crate::probe::Axes) {
        use crate::probe::{Axes, Motions};
        let mut m = Motions::NONE;
        let mut a = Axes::NONE;
        let staggered = self.staggers.planned(id);
        let springing = self.nodes.get(&id).is_some_and(|n| !n.props.is_empty());
        if self.enter.contains(&id) || staggered || self.entered.contains(&id) && springing {
            m |= Motions::ENTER;
        }
        if staggered {
            m |= Motions::STAGGER;
        }
        if self.exits.contains_key(&id) {
            m |= Motions::EXIT;
        }
        if self.lifted.contains_key(&id) {
            m |= Motions::LIFT;
        }
        if let Some(na) = self.nodes.get(&id) {
            for (p, _) in &na.props {
                m |= match p {
                    Prop::Opacity => Motions::OPACITY_SPRING,
                    _ => Motions::PROP_SPRING,
                };
                // (eval-rules) Which of its own colours springs.
                m |= match p {
                    Prop::Bg | Prop::Value | Prop::Track => Motions::FILL_SPRING,
                    Prop::Color | Prop::Fill => Motions::INK_SPRING,
                    Prop::Border => Motions::LINE_SPRING,
                    _ => Motions::NONE,
                };
                a |= match p {
                    Prop::X => Axes::X,
                    Prop::Y => Axes::Y,
                    Prop::Opacity => Axes::OPACITY,
                    Prop::Scale => Axes::SCALE,
                    Prop::Rotate => Axes::ROTATE,
                    _ => Axes::NONE,
                };
            }
            if na.glide.is_some() {
                m |= Motions::GLIDE;
            }
            for (s, axis) in na.size.iter().zip([Axes::W, Axes::H]) {
                if s.is_some() {
                    m |= Motions::SIZE_SPRING;
                    a |= axis;
                }
            }
            // (growth) A content-sized surface's box springing over a
            // step of its content's size.
            for (s, axis) in na.step.iter().zip([Axes::W, Axes::H]) {
                if s.is_some() {
                    m |= Motions::SIZE_SPRING;
                    a |= axis;
                }
            }
        }
        (m, a)
    }

    /// (M4) `id` is lifted or springing back from a lift (it paints
    /// above its siblings).
    pub fn lifted(&self, id: NodeId) -> bool {
        self.lift_rank(id) > 0
    }

    /// (dock-drag) Where `id` paints among its siblings for a lift in
    /// this frame: 0 in its place, 1 above them while it springs back, 2
    /// above those while a drag holds it. A lift let go is back among its
    /// siblings on the frame its spring lands in its box (the frame
    /// [`Animator::offset`] samples at rest), not one frame later: no
    /// frame may follow it. Its entry goes when its node is next drawn,
    /// so one that settled is no longer counted (a pin lifted just after
    /// another came to rest is the one on top).
    pub fn lift_rank(&self, id: NodeId) -> u8 {
        let springing = |n: &NodeAnim| {
            n.glide
                .as_ref()
                .is_some_and(|g| !self.snapping() && !g.is_settled(self.time))
        };
        match self.lifted.get(&id) {
            Some(Some(_)) => 2,
            Some(None) if self.nodes.get(&id).is_some_and(springing) => 1,
            _ => 0,
        }
    }

    /// The glide offset of `id` in this frame (and a lift's).
    pub fn offset(&mut self, id: NodeId) -> (f32, f32) {
        if let Some(l) = self.lifted.get(&id).copied() {
            if let Some(v) = l {
                return (v[0], v[1]);
            }
            // Back in its box: no longer above its siblings.
            if self.nodes.get(&id).is_none_or(|n| n.glide.is_none()) {
                self.lifted.remove(&id);
            }
        }
        let (at, commit, snap, lead) = (self.time, self.commit, self.snapping(), self.lead());
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
        #[cfg(feature = "eval-hooks")]
        let pre = g.view();
        let v = if commit {
            g.sample_frame(at, lead)
        } else {
            g.peek(at)
        };
        #[cfg(feature = "eval-hooks")]
        if commit {
            self.springs.push(crate::probe::SpringTrace::of(
                id,
                crate::probe::SpringOf::Glide,
                at,
                &pre,
                &v,
            ));
        }
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
        let text_held = self.text_held.remove(&id);
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
                self.end_exit(id);
            }
            return;
        }
        // A staggered child stays entering (in `staggers`, not `enter`,
        // which frames clear) until its turn.
        let entering = self.enter.remove(&id) || waiting;
        #[cfg(feature = "eval-hooks")]
        if !entering && self.nodes.get(&id).is_none_or(|n| n.props.is_empty()) {
            self.entered.remove(&id);
        }
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
        // A staggered child waiting for its turn holds at its pose; so
        // does one whose text is on its way (late-text-dip), entering
        // still.
        if entering && exiting.is_none() && text_held {
            self.enter.insert(id);
        }
        if entering && exiting.is_none() && (text_held || self.staggers.held(id, self.time)) {
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
            self.text_holds.remove(&id);
            #[cfg(feature = "eval-hooks")]
            self.entered.insert(id);
        }
        let (at, commit, last, lead) = (self.time, self.commit, self.prev, self.lead());
        let na = self.nodes.entry(id).or_default();
        let touched = std::mem::take(&mut na.touched);
        // A node drawn for the first time has no value on screen to spring
        // from: logic's writes since it was created (the props of the diff
        // that created it, recorded as replacing "unset") start at their
        // values, and only its enter pose moves.
        let touched = if entering && exiting.is_none() {
            Vec::new()
        } else {
            touched
        };
        let respring = std::mem::take(&mut na.respring);
        let mut moving = false;
        let mut exit_done = true;
        // A lone-radius glow springs in the colour flatten draws it in at
        // rest: the node's own (target) colour, else the inherited one.
        let glow_inh = glow_color(props, inh);
        // (eval-rules-7) An unset `bg` is the fill flatten draws without
        // one (a button's `$surface.hi`, a bar's `$surface`), so a `bg`
        // a state sets springs from it and back to it, not from clear: a
        // button's default fill vanished at once as `$accent.container`
        // faded in from nothing, and snapped back at rest after the fill
        // faded out to the bar under it.
        let text_color = match props
            .iter()
            .find(|(q, _)| *q == Prop::Color)
            .map(|(_, v)| v.as_ref())
        {
            Some(PropValue::Color(c) | PropValue::Paint(Paint::Solid(c))) => *c,
            _ => crate::flatten::default_color(scope),
        };
        let unset_bg =
            crate::flatten::default_fill(node.kind, node.parent.is_none(), scope, text_color)
                .map(PropValue::Color);
        let encode = |p: Prop, v: Option<&PropValue>, inh: Color, boxes: Extents| {
            let v = match v {
                None | Some(PropValue::Unset) if p == Prop::Bg => unset_bg.as_ref(),
                v => v,
            };
            encode(p, v, inh, boxes)
        };
        // (review) A ranged widget's `value` is in its range's units, so
        // it settles within its tolerance of that range's span, as a
        // value of 0 to 1 did: a meter of bytes no longer springs until
        // within 0.0005 of a byte.
        let value_scale = value_scale(node, props, self.ranges.get(&id));
        for p in ANIMATED {
            let inh = if p == Prop::Glow { glow_inh } else { inh };
            let k = if p == Prop::Value { value_scale } else { 1.0 };
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
                            *m = PropMotion::rest(p, &target, last, k);
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
                        let mut m = PropMotion::rest(p, &from, last, k);
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
            #[cfg(feature = "eval-hooks")]
            let pre = commit.then(|| na.props[i].1.view()).flatten();
            let enc = na.props[i].1.value(at, commit, lead);
            #[cfg(feature = "eval-hooks")]
            if let Some(mut t) =
                pre.and_then(|v| v.trace(id, crate::probe::SpringOf::Prop(p), at, &enc))
            {
                // On the frame it enters, whether its pose sets the prop.
                t.entering = (entering && exiting.is_none())
                    .then(|| enter_pose.iter().any(|(q, _)| *q == p));
                self.springs.push(t);
            }
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
        if exiting.is_some() && exit_done {
            if commit {
                self.end_exit(id);
            } else {
                self.exits_due.insert(id);
            }
        } else if commit {
            self.exits_due.remove(&id);
        }
        if self.nodes.get(&id).is_some_and(NodeAnim::is_empty) {
            self.nodes.remove(&id);
        }
    }

    // ---- Laid-out sizes ----------------------------------------------

    /// Anything under `root` moving or about to: frames are wanted.
    /// (roll-preview) A preview of the surface of `root` left something
    /// to the painted frame: a shape, text or image it saw first or saw
    /// change, a `play` or shared morph it saw start, a held node it saw
    /// moved. It drew the node as the last frame did (the old shape, no
    /// play), so its scene matching the last frame's does not mean
    /// nothing changed: the surface still wants that frame. (eval-r3) So
    /// does an exit it found at its pose with nothing left to move: only
    /// a painted frame ends it.
    pub fn deferred(&self, tree: &SceneTree, root: NodeId) -> bool {
        let under = |id: NodeId| tree.root_of(id) == Some(root);
        self.exits_due.iter().any(|id| under(*id))
            || self.shapes.pending(under)
            || self.rolls.pending(under)
            || self.image_swaps.pending(under)
            || self.plays.pending(under)
            || self.shared.pending(under)
            || self.jellies.pending(under)
    }

    /// (shadow-buffer) A shadow under `root` is in flight, or logic set
    /// one not drawn yet (its spring starts at the next paint, from the
    /// value it had): what it draws is not its value at rest, the only
    /// one layout reads for the surface's overhang. Never while motion
    /// snaps.
    pub(crate) fn shadows_moving(&self, tree: &SceneTree, root: NodeId) -> bool {
        !self.snapping()
            && self.nodes.iter().any(|(id, n)| {
                // (integration-r8) A glow is held as a shadow is: the
                // overhang keeps room for it (`layout`).
                let reach = |p: Prop| matches!(p, Prop::Shadow | Prop::Glow);
                (n.props.iter().any(|(p, _)| reach(*p)) || n.touched.iter().any(|(p, _)| reach(*p)))
                    && tree.root_of(*id) == Some(root)
            })
    }

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
            || self.jellies.busy(|id| under(&id))
    }
}

#[cfg(test)]
mod tests;
