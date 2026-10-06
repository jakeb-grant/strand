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
use std::time::Duration;

use strand_scene::motion::{channels_color, color_channels};
use strand_scene::{
    Border, Color, Corners, Curve, Length, LogicalRect, Motion, NodeId, Paint, Prop, PropValue,
    Shadow, TokenScope, Transition,
};

use crate::tree::{Node, SceneTree};

/// Props that spring between values; the others snap.
pub(crate) const ANIMATED: [Prop; 10] = [
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
        Prop::Bg | Prop::Color => 0.002,
        _ => 0.05,
    }
}

/// Settling tolerance of laid-out sizes and glides, logical pixels.
const PX_EPS: f32 = 0.05;

/// Laid-out sizes forced on nodes for one layout pass: `[width, height]`
/// in logical pixels.
pub(crate) type SizeMap = HashMap<NodeId, [Option<f32>; 2]>;

#[derive(Clone, Debug, PartialEq)]
enum Enc {
    One([f32; 1]),
    Four([f32; 4]),
    Five([f32; 5]),
    Shadows(Vec<[f32; 8]>),
}

#[derive(Clone, Debug)]
enum PropMotion {
    One(Motion<1>),
    Four(Motion<4>),
    Five(Motion<5>),
    /// Padded to the longer of the lists it moves between; `len` is the
    /// target's own length.
    Shadows {
        m: Vec<Motion<8>>,
        len: usize,
    },
}

fn number(v: &PropValue) -> Option<f32> {
    match v {
        PropValue::Number(n) | PropValue::Length(Length::Px(n)) | PropValue::Angle(n) => {
            n.is_finite().then_some(*n)
        }
        _ => None,
    }
}

fn shadow_channels(s: &Shadow) -> [f32; 8] {
    let c = color_channels(s.color);
    let f = |v: f32| if v.is_finite() { v } else { 0.0 };
    [
        f(s.x),
        f(s.y),
        f(s.blur),
        f(s.spread),
        c[0],
        c[1],
        c[2],
        c[3],
    ]
}

/// A prop value as channels (`None` value: the prop's default, `inh` for
/// `color`). `None` when it cannot interpolate (a gradient, a percentage
/// offset, `radius: full`): it snaps.
fn encode(p: Prop, v: Option<&PropValue>, inh: Color) -> Option<Enc> {
    let solid = |v: &PropValue| match v {
        PropValue::Color(c) | PropValue::Paint(Paint::Solid(c)) => Some(*c),
        _ => None,
    };
    Some(match (p, v) {
        (Prop::X | Prop::Y | Prop::Rotate, None) => Enc::One([0.0]),
        (Prop::Opacity | Prop::Scale, None) => Enc::One([1.0]),
        (Prop::X | Prop::Y | Prop::Opacity | Prop::Scale | Prop::Rotate, Some(v)) => {
            Enc::One([number(v)?])
        }
        (Prop::Bg, None) => Enc::Four(color_channels(Color::TRANSPARENT)),
        (Prop::Color, None) => Enc::Four(color_channels(inh)),
        (Prop::Bg | Prop::Color, Some(v)) => Enc::Four(color_channels(solid(v)?)),
        (Prop::Border, None) => {
            let c = color_channels(Color::TRANSPARENT);
            Enc::Five([0.0, c[0], c[1], c[2], c[3]])
        }
        (Prop::Border, Some(PropValue::Border(Border { width, paint }))) => {
            let c = color_channels(solid(&PropValue::Paint(paint.clone()))?);
            Enc::Five([width.is_finite().then_some(*width)?, c[0], c[1], c[2], c[3]])
        }
        (Prop::Shadow, None) => Enc::Shadows(Vec::new()),
        (Prop::Shadow, Some(PropValue::Shadow(list))) => {
            Enc::Shadows(list.iter().map(shadow_channels).collect())
        }
        (Prop::Radius, None) => Enc::Four([0.0; 4]),
        (Prop::Radius, Some(v)) => {
            let c = match v {
                PropValue::Corners(c) => *c,
                PropValue::List(items) => {
                    let n: Option<Vec<f32>> = items.iter().map(number).collect();
                    Corners::from_values(&n?)?
                }
                v => Corners::all(number(v)?),
            };
            let all = [c.top_left, c.top_right, c.bottom_right, c.bottom_left];
            if all.iter().any(|v| !v.is_finite() || *v > 1e5) {
                return None;
            }
            Enc::Four(all)
        }
        _ => return None,
    })
}

fn decode(p: Prop, e: &Enc) -> PropValue {
    match (p, e) {
        (Prop::Rotate, Enc::One([v])) => PropValue::Angle(*v),
        (_, Enc::One([v])) => PropValue::Number(*v),
        (Prop::Radius, Enc::Four(c)) => PropValue::Corners(Corners {
            top_left: c[0].max(0.0),
            top_right: c[1].max(0.0),
            bottom_right: c[2].max(0.0),
            bottom_left: c[3].max(0.0),
        }),
        (_, Enc::Four(c)) => PropValue::Color(channels_color(*c)),
        (_, Enc::Five(b)) => PropValue::Border(Border {
            width: b[0].max(0.0),
            paint: Paint::Solid(channels_color([b[1], b[2], b[3], b[4]])),
        }),
        (_, Enc::Shadows(list)) => PropValue::Shadow(
            list.iter()
                .map(|s| Shadow {
                    x: s[0],
                    y: s[1],
                    blur: s[2].max(0.0),
                    spread: s[3],
                    color: channels_color([s[4], s[5], s[6], s[7]]),
                })
                .collect(),
        ),
    }
}

/// A shadow's geometry with no colour: what a shorter list is padded with.
fn clear(s: [f32; 8]) -> [f32; 8] {
    [s[0], s[1], s[2], s[3], 0.0, 0.0, 0.0, 0.0]
}

impl PropMotion {
    fn rest(p: Prop, e: &Enc, last: Option<Duration>) -> PropMotion {
        let k = eps(p);
        match e {
            Enc::One(v) => PropMotion::One(Motion::rest(*v, k).sampled_at(last)),
            Enc::Four(v) => PropMotion::Four(Motion::rest(*v, k).sampled_at(last)),
            Enc::Five(v) => PropMotion::Five(Motion::rest(*v, k).sampled_at(last)),
            Enc::Shadows(list) => PropMotion::Shadows {
                m: list
                    .iter()
                    .map(|s| Motion::rest(*s, 0.01).sampled_at(last))
                    .collect(),
                len: list.len(),
            },
        }
    }

    fn target(&self) -> Enc {
        match self {
            PropMotion::One(m) => Enc::One(m.target()),
            PropMotion::Four(m) => Enc::Four(m.target()),
            PropMotion::Five(m) => Enc::Five(m.target()),
            PropMotion::Shadows { m, len } => {
                Enc::Shadows(m.iter().take(*len).map(Motion::target).collect())
            }
        }
    }

    /// Retargets; false if `e` is of another shape (the caller snaps).
    fn retarget(&mut self, e: &Enc, curve: Curve, last: Option<Duration>) -> bool {
        match (self, e) {
            (PropMotion::One(m), Enc::One(v)) => m.retarget(*v, curve),
            (PropMotion::Four(m), Enc::Four(v)) => m.retarget(*v, curve),
            (PropMotion::Five(m), Enc::Five(v)) => m.retarget(*v, curve),
            (PropMotion::Shadows { m, len }, Enc::Shadows(list)) => {
                // Padded to equal length: a missing shadow is the other's
                // geometry, fully transparent (design.md, snap rules).
                for (i, motion) in m.iter_mut().enumerate() {
                    let t = list
                        .get(i)
                        .copied()
                        .unwrap_or_else(|| clear(motion.target()));
                    motion.retarget(t, curve);
                }
                for s in list.iter().skip(m.len()) {
                    let mut motion = Motion::rest(clear(*s), 0.01).sampled_at(last);
                    motion.retarget(*s, curve);
                    m.push(motion);
                }
                *len = list.len();
            }
            _ => return false,
        }
        true
    }

    fn value(&mut self, at: Duration, commit: bool) -> Enc {
        fn one<const N: usize>(m: &mut Motion<N>, at: Duration, commit: bool) -> [f32; N] {
            if commit { m.sample(at) } else { m.peek(at) }
        }
        match self {
            PropMotion::One(m) => Enc::One(one(m, at, commit)),
            PropMotion::Four(m) => Enc::Four(one(m, at, commit)),
            PropMotion::Five(m) => Enc::Five(one(m, at, commit)),
            PropMotion::Shadows { m, .. } => {
                Enc::Shadows(m.iter_mut().map(|m| one(m, at, commit)).collect())
            }
        }
    }

    fn settled(&self, at: Duration) -> bool {
        match self {
            PropMotion::One(m) => m.is_settled(at),
            PropMotion::Four(m) => m.is_settled(at),
            PropMotion::Five(m) => m.is_settled(at),
            PropMotion::Shadows { m, .. } => m.iter().all(|m| m.is_settled(at)),
        }
    }
}

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

/// The props of a pose: `enter { x: 420; opacity: 0 }`, or a preset
/// (`fade`, `slidefade`, `popin(0.8)`, `slide(top)`), for a node laid out
/// in `rect` (a `slide` moves by the node's own size).
pub(crate) fn pose_props(v: &PropValue, rect: Option<LogicalRect>) -> Vec<(Prop, PropValue)> {
    let n = PropValue::Number;
    match v {
        PropValue::Pose(props) => props.clone(),
        PropValue::Keyword(k) => match k.as_str() {
            "fade" => vec![(Prop::Opacity, n(0.0))],
            // Fades in while rising a little: the design gives no
            // distance (decisions.md, wave3-pixels).
            "slidefade" => vec![(Prop::Opacity, n(0.0)), (Prop::Y, n(8.0))],
            _ => Vec::new(),
        },
        PropValue::Call { name, args } => match name.as_str() {
            "popin" => {
                let s = args.first().and_then(number).unwrap_or(0.8);
                vec![(Prop::Scale, n(s)), (Prop::Opacity, n(0.0))]
            }
            "slide" | "slidefade" => {
                let edge = match args.first() {
                    Some(PropValue::Keyword(k)) => k.as_str(),
                    _ => "bottom",
                };
                let (w, h) = rect.map_or((0.0, 0.0), |r| (r.w, r.h));
                let mut out = match edge {
                    "top" => vec![(Prop::Y, n(-h))],
                    "left" => vec![(Prop::X, n(-w))],
                    "right" => vec![(Prop::X, n(w))],
                    _ => vec![(Prop::Y, n(h))],
                };
                if name == "slidefade" {
                    out.push((Prop::Opacity, n(0.0)));
                }
                out
            }
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// The `exit` pose of a node, else its `enter` (exit mirrors enter).
pub(crate) fn exit_pose(node: &Node) -> Option<&PropValue> {
    node.get(Prop::Exit).or_else(|| node.get(Prop::Enter))
}

/// True if a pose value moves anything.
pub(crate) fn is_pose(v: Option<&PropValue>) -> bool {
    !pose_props(v.unwrap_or(&PropValue::Unset), None).is_empty()
        || matches!(v, Some(PropValue::Call { name, .. }) if name == "slide" || name == "slidefade")
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
    /// Exiting nodes drawn since [`Animator::begin`].
    drawn: HashSet<NodeId>,
    finished: Vec<(NodeId, ExitKind)>,
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

    /// Everything snaps: `reduced_motion`, or a frame at time zero (no
    /// clock).
    fn snapping(&self) -> bool {
        self.reduced || self.time.is_zero()
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
    }

    /// `id` plays its exit pose.
    pub fn exit(&mut self, id: NodeId, kind: ExitKind) {
        self.enter.remove(&id);
        self.enter_size.remove(&id);
        self.exits.insert(id, kind);
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

    pub fn exiting(&self, id: NodeId) -> Option<ExitKind> {
        self.exits.get(&id).copied()
    }

    /// Exits that finished in the frames painted since the last call.
    pub fn take_finished(&mut self) -> Vec<(NodeId, ExitKind)> {
        std::mem::take(&mut self.finished)
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
        self.nodes.retain(|id, _| keep(*id));
        self.enter.retain(|id| keep(*id));
        self.enter_size.retain(|id| keep(*id));
        self.exits.retain(|id, _| keep(*id));
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

    /// True while `id`'s laid-out size springs: it clips its content.
    pub fn sizing(&self, id: NodeId) -> bool {
        self.nodes
            .get(&id)
            .is_some_and(|n| n.size.iter().any(Option::is_some))
    }

    /// Springs the animatable props of `node` (resolved in `props`, which
    /// get the values of this frame), playing its enter or exit pose.
    /// `inh` is the colour it inherits; `rect` its laid-out box.
    pub fn paint(
        &mut self,
        node: &Node,
        props: &mut Vec<(Prop, Cow<'_, PropValue>)>,
        scope: &TokenScope<'_>,
        inh: Color,
        rect: Option<LogicalRect>,
    ) {
        let id = node.id;
        let exiting = self.exits.get(&id).copied();
        if exiting.is_some() || self.nodes.contains_key(&id) {
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
                if let Some(k) = self.exits.remove(&id) {
                    self.finished.push((id, k));
                }
            }
            return;
        }
        let entering = self.enter.remove(&id);
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
        let enter_pose = entering
            .then(|| node.get(Prop::Enter).map(resolve))
            .flatten()
            .unwrap_or_default();
        let exit_pose = exiting
            .and_then(|_| exit_pose(node).map(resolve))
            .unwrap_or_default();
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
            let Some(target) = encode(p, target_v.as_ref(), inh) else {
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
                        Some(encode(p, Some(v), inh))
                    } else if let Some((_, old)) = touch {
                        Some(encode(p, old.as_ref(), inh))
                    } else if in_exit.is_some() || respring {
                        Some(encode(p, own_v.as_ref(), inh))
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
                if commit {
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

    /// Nodes under `root` whose laid-out size may spring: the layout step
    /// lays them out at rest first to learn their targets.
    pub fn size_work(&self, tree: &SceneTree, root: NodeId) -> bool {
        let under = |id: &NodeId| tree.root_of(*id) == Some(root);
        self.nodes
            .iter()
            .any(|(id, n)| (n.size_touched || n.size.iter().any(Option::is_some)) && under(id))
            || self.enter_size.iter().any(under)
    }

    /// The sizes exiting nodes under `root` take at rest: their exit
    /// pose's `width`/`height`/`size` (toasts collapse to `height: 0`).
    pub fn rest_sizes(&self, tree: &SceneTree, root: NodeId) -> SizeMap {
        let mut out = SizeMap::new();
        if self.snapping() && self.exits.is_empty() {
            return out;
        }
        for id in self.exits.keys() {
            if tree.root_of(*id) != Some(root) {
                continue;
            }
            let Some(node) = tree.get(*id) else { continue };
            let Some(pose) = exit_pose(node) else {
                continue;
            };
            let s = pose_sizes(tree, node, pose);
            if s.iter().any(Option::is_some) {
                out.insert(*id, s);
            }
        }
        out
    }

    /// Starts or retargets size springs of nodes under `root`, given
    /// their sizes laid out at rest (`targets`) and in the frame before
    /// (`old`).
    pub fn start_sizes(
        &mut self,
        tree: &SceneTree,
        root: NodeId,
        targets: &HashMap<NodeId, LogicalRect>,
        old: Option<&HashMap<NodeId, LogicalRect>>,
        partial: bool,
    ) {
        let snapping = self.snapping();
        let ids: Vec<NodeId> = self
            .nodes
            .iter()
            .filter(|(_, n)| n.size_touched || n.size.iter().any(Option::is_some))
            .map(|(id, _)| *id)
            .chain(self.enter_size.iter().copied())
            .filter(|id| tree.root_of(*id) == Some(root))
            // A partial pass knows only the subtrees it laid out.
            .filter(|id| !partial || targets.contains_key(id))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let last = self.prev;
        for id in ids {
            let entering = self.enter_size.remove(&id);
            let Some(node) = tree.get(id) else {
                self.nodes.remove(&id);
                continue;
            };
            let na = self.nodes.entry(id).or_default();
            let touched = std::mem::take(&mut na.size_touched);
            let Some(t) = targets.get(&id).filter(|_| !snapping) else {
                na.size = [None, None];
                continue;
            };
            let target = [t.w, t.h];
            let from_box = old.and_then(|o| o.get(&id)).map(|r| [r.w, r.h]);
            let pose = if entering {
                node.get(Prop::Enter)
                    .map(|p| pose_sizes(tree, node, p))
                    .unwrap_or_default()
            } else {
                [None, None]
            };
            let scopes = crate::flatten::scope_tables(tree, id);
            let scope = TokenScope::new(&scopes);
            for axis in 0..2 {
                let prop = if axis == 0 { Prop::Width } else { Prop::Height };
                let transition = node
                    .props
                    .iter()
                    .find(|e| e.prop == prop)
                    .or_else(|| node.props.iter().find(|e| e.prop == Prop::Size))
                    .map_or(Transition::Default, |e| e.transition.clone());
                let curve = Curve::of(&scope.transition(&transition, prop));
                match &mut na.size[axis] {
                    Some(m) => {
                        if (m.target()[0] - target[axis]).abs() > 0.01 {
                            m.retarget([target[axis]], curve);
                        }
                    }
                    slot @ None => {
                        let from = pose[axis].or(if touched {
                            from_box.map(|b| b[axis])
                        } else {
                            None
                        });
                        if let Some(f) = from
                            && (f - target[axis]).abs() > 0.01
                            && curve != Curve::Instant
                        {
                            let mut m = Motion::rest([f], PX_EPS).sampled_at(last);
                            m.retarget([target[axis]], curve);
                            *slot = Some(m);
                        }
                    }
                }
            }
            if na.is_empty() {
                self.nodes.remove(&id);
            }
        }
    }

    /// The sizes springs give nodes under `root` in this frame, over
    /// `rest`. Settled springs end (their node lays out at rest).
    pub fn size_overrides(&mut self, tree: &SceneTree, root: NodeId, rest: &SizeMap) -> SizeMap {
        let mut out = rest.clone();
        let (at, commit) = (self.time, self.commit);
        let mut moving = false;
        for (id, na) in self.nodes.iter_mut() {
            if na.size.iter().all(Option::is_none) || tree.root_of(*id) != Some(root) {
                continue;
            }
            let mut o = rest.get(id).copied().unwrap_or([None, None]);
            for (slot, forced) in na.size.iter_mut().zip(o.iter_mut()) {
                let Some(m) = slot.as_mut() else {
                    continue;
                };
                let v = if commit { m.sample(at) } else { m.peek(at) };
                if m.is_settled(at) {
                    if commit {
                        *slot = None;
                    }
                } else {
                    moving = true;
                    *forced = Some(v[0].max(0.0));
                }
            }
            if o.iter().any(Option::is_some) {
                out.insert(*id, o);
            }
        }
        self.nodes.retain(|_, n| !n.is_empty());
        if moving {
            self.active = true;
        }
        out
    }

    /// True if any size spring under `root` is moving.
    pub fn sizes_moving(&self, tree: &SceneTree, root: NodeId) -> bool {
        self.nodes
            .iter()
            .any(|(id, n)| n.size.iter().any(Option::is_some) && tree.root_of(*id) == Some(root))
    }

    /// Nodes with a size spring under `root`.
    pub fn sized_nodes(&self, tree: &SceneTree, root: NodeId) -> Vec<NodeId> {
        self.nodes
            .iter()
            .filter(|(id, n)| {
                n.size.iter().any(Option::is_some) && tree.root_of(**id) == Some(root)
            })
            .map(|(id, _)| *id)
            .collect()
    }

    /// Anything under `root` moving or about to: frames are wanted.
    pub fn busy(&self, tree: &SceneTree, root: NodeId) -> bool {
        let under = |id: &NodeId| tree.root_of(*id) == Some(root);
        self.nodes.keys().any(under) || self.enter.iter().any(under) || self.exits.keys().any(under)
    }
}

/// The `[width, height]` a pose gives `node` (plain lengths only).
fn pose_sizes(tree: &SceneTree, node: &Node, pose: &PropValue) -> [Option<f32>; 2] {
    let scopes = crate::flatten::scope_tables(tree, node.id);
    let scope = TokenScope::new(&scopes);
    let Some(pose) = scope.resolve(pose) else {
        return [None, None];
    };
    let mut out = [None, None];
    for (p, v) in pose_props(&pose, None) {
        let n = scope
            .resolve(&v)
            .and_then(|v| number(&v))
            .map(|v| v.max(0.0));
        match p {
            Prop::Width => out[0] = n.or(out[0]),
            Prop::Height => out[1] = n.or(out[1]),
            Prop::Size => {
                out[0] = out[0].or(n);
                out[1] = out[1].or(n);
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_round_trip_through_channels() {
        let white = Color::WHITE;
        for (p, v) in [
            (Prop::X, PropValue::Number(4.0)),
            (Prop::Rotate, PropValue::Angle(30.0)),
            (
                Prop::Bg,
                PropValue::Color(Color::from_rgba8(10, 200, 30, 255)),
            ),
            (
                Prop::Border,
                PropValue::Border(Border {
                    width: 2.0,
                    paint: Paint::Solid(Color::BLACK),
                }),
            ),
            (Prop::Radius, PropValue::Corners(Corners::all(6.0))),
        ] {
            let e = encode(p, Some(&v), white).unwrap();
            let back = decode(p, &e);
            let e2 = encode(p, Some(&back), white).unwrap();
            assert_eq!(e, e2, "{p:?}");
        }
        assert!(
            encode(
                Prop::X,
                Some(&PropValue::Length(Length::Percent(5.0))),
                white
            )
            .is_none()
        );
        assert!(
            encode(
                Prop::Radius,
                Some(&PropValue::Corners(Corners::FULL)),
                white
            )
            .is_none()
        );
        assert_eq!(encode(Prop::Opacity, None, white), Some(Enc::One([1.0])));
    }

    #[test]
    fn presets_expand() {
        assert_eq!(
            pose_props(&PropValue::Keyword("fade".into()), None),
            vec![(Prop::Opacity, PropValue::Number(0.0))]
        );
        let popin = PropValue::Call {
            name: "popin".into(),
            args: vec![PropValue::Number(0.8)],
        };
        assert_eq!(
            pose_props(&popin, None)[0],
            (Prop::Scale, PropValue::Number(0.8))
        );
        let slide = PropValue::Call {
            name: "slide".into(),
            args: vec![PropValue::Keyword("top".into())],
        };
        let r = LogicalRect::new(0.0, 0.0, 100.0, 30.0);
        assert_eq!(
            pose_props(&slide, Some(r)),
            vec![(Prop::Y, PropValue::Number(-30.0))]
        );
        assert!(pose_props(&PropValue::Keyword("none".into()), None).is_empty());
    }
}
