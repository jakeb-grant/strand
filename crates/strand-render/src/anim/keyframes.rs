//! (M4) Keyframes playback (design.md, "Motion and time": `keyframes
//! shake { … }` + `play shake`; architecture.md: "Keyframes are offsets
//! composed with the node's springs; a new `seq` restarts them").
//!
//! `Prop::Play` holds the compiled block. A block render has not seen
//! (another name or `seq`) starts on the first painted frame that draws
//! it and plays `repeat` times (`None`: forever) of `duration` each,
//! after `delay`, every other run backwards when `alternate`. Between two
//! stops that set a prop it moves along `easing`. A prop the first stop
//! (or the last) leaves out starts (or ends) at the node's own value, so
//! `25% { x: -4 }` alone is a nudge out and back.
//!
//! The values compose with what the springs drew this frame: `x`, `y`
//! and `rotate` add to the node's own, `scale` and `opacity` multiply
//! it, and any other prop is drawn at the stop's value in place of the
//! node's (colours and other channels interpolated from the node's own).
//! During the delay and once it has played, the node draws as it is.
//!
//! `reduced_motion` (and frames with no clock): a block that plays a
//! fixed number of times is not played, and a loop holds still at its
//! start.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use strand_scene::motion::ease;
use strand_scene::{Color, Keyframes, NodeId, Prop, PropValue};

use super::motion::{Enc, Extents, decode, encode, number};
use crate::shapes::morph::Frame;

#[derive(Debug)]
struct Play {
    name: String,
    seq: u32,
    /// When it started.
    start: Duration,
    /// When it ends (`None`: a loop), and whether a painted frame has
    /// seen it end.
    end: Option<Duration>,
    done: bool,
}

/// Every node's last `play`.
#[derive(Debug, Default)]
pub(crate) struct Plays {
    nodes: HashMap<NodeId, Play>,
    /// Nodes a preview saw a new `play` on: the next painted frame starts
    /// it.
    pending: HashSet<NodeId>,
}

/// How long `k` plays after it starts, delay included (`None`: forever).
fn length(k: &Keyframes) -> Option<Duration> {
    let runs = k.repeat?;
    Some(k.delay.saturating_add(k.duration.saturating_mul(runs)))
}

impl Plays {
    /// Where node `id`'s block `k` is in `frame`: its progress through
    /// the run in `0..=1` (`None`: it draws as it is), and whether it is
    /// still playing.
    pub(crate) fn progress(
        &mut self,
        id: NodeId,
        k: &Keyframes,
        frame: Frame,
    ) -> (Option<f32>, bool) {
        let seen = self
            .nodes
            .get(&id)
            .is_some_and(|p| p.seq == k.seq && p.name == k.name);
        if !seen {
            if frame.snap {
                // Held still (a loop) or skipped, and never started late
                // when motion comes back.
                if frame.commit {
                    self.pending.remove(&id);
                    self.nodes.insert(
                        id,
                        Play {
                            name: k.name.clone(),
                            seq: k.seq,
                            start: frame.at,
                            end: None,
                            done: k.repeat.is_some(),
                        },
                    );
                }
                return (k.repeat.is_none().then_some(0.0), false);
            }
            if !frame.commit {
                self.pending.insert(id);
                return (None, false);
            }
            self.pending.remove(&id);
            self.nodes.insert(
                id,
                Play {
                    name: k.name.clone(),
                    seq: k.seq,
                    start: frame.at,
                    end: length(k).map(|l| frame.at.saturating_add(l)),
                    done: false,
                },
            );
        }
        let Some(play) = self.nodes.get_mut(&id) else {
            return (None, false);
        };
        if frame.snap {
            if play.end.is_some() && frame.commit {
                play.done = true;
            }
            return (k.repeat.is_none().then_some(0.0), false);
        }
        if play.done {
            return (None, false);
        }
        let ran = frame.at.saturating_sub(play.start);
        if ran < k.delay {
            return (None, true);
        }
        if play.end.is_some_and(|end| frame.at >= end) || k.duration.is_zero() {
            if frame.commit {
                play.done = true;
            }
            return (None, false);
        }
        let runs = (ran - k.delay).as_secs_f64() / k.duration.as_secs_f64();
        let run = runs.floor();
        let mut p = (runs - run) as f32;
        if k.alternate && run as u64 % 2 == 1 {
            p = 1.0 - p;
        }
        (Some(p.clamp(0.0, 1.0)), true)
    }

    /// Anything `under` a surface playing or about to.
    pub(crate) fn busy(&self, mut under: impl FnMut(NodeId) -> bool) -> bool {
        self.pending.iter().any(|id| under(*id))
            || self.nodes.iter().any(|(id, p)| !p.done && under(*id))
    }

    /// Drops nodes `keep` rejects.
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(NodeId) -> bool) {
        self.nodes.retain(|id, _| keep(*id));
        self.pending.retain(|id| keep(*id));
    }

    pub(crate) fn forget(&mut self, id: NodeId) {
        self.nodes.remove(&id);
        self.pending.remove(&id);
    }
}

/// How a prop's keyframe value meets the node's own.
#[derive(Copy, Clone, PartialEq)]
enum Compose {
    Add,
    Multiply,
    Replace,
}

fn compose(p: Prop) -> Compose {
    match p {
        Prop::X | Prop::Y | Prop::Rotate => Compose::Add,
        Prop::Scale | Prop::Opacity => Compose::Multiply,
        _ => Compose::Replace,
    }
}

fn lerp_enc(a: &Enc, b: &Enc, u: f32) -> Option<Enc> {
    fn mix<const N: usize>(a: &[f32; N], b: &[f32; N], u: f32) -> [f32; N] {
        std::array::from_fn(|i| a[i] + (b[i] - a[i]) * u)
    }
    Some(match (a, b) {
        (Enc::One(a), Enc::One(b)) => Enc::One(mix(a, b, u)),
        (Enc::Four(a), Enc::Four(b)) => Enc::Four(mix(a, b, u)),
        (Enc::Five(a), Enc::Five(b)) => Enc::Five(mix(a, b, u)),
        (Enc::Shadows(a), Enc::Shadows(b)) if a.len() == b.len() => {
            Enc::Shadows(a.iter().zip(b).map(|(a, b)| mix(a, b, u)).collect())
        }
        _ => return None,
    })
}

/// Draws `props` (this frame's values, springs applied) at progress `p`
/// of `k`. `inh` is the inherited colour and `b` the boxes lengths
/// resolve against.
pub(super) fn apply(
    k: &Keyframes,
    p: f32,
    props: &mut Vec<(Prop, std::borrow::Cow<'_, PropValue>)>,
    inh: Color,
    b: Extents,
) {
    let mut seen: Vec<Prop> = Vec::new();
    for (_, set) in &k.stops {
        for (prop, _) in set {
            if !seen.contains(prop) && *prop != Prop::Play {
                seen.push(*prop);
            }
        }
    }
    for prop in seen {
        let own_at = props.iter().position(|(q, _)| *q == prop);
        let own = own_at.map(|i| props[i].1.as_ref().clone());
        // The stops on either side of `p` that set it; the node's own
        // value (`None`) past the first or last.
        let stops = k
            .stops
            .iter()
            .filter_map(|(at, set)| Some((*at, set.iter().find(|(q, _)| *q == prop)?.1.clone())));
        let mut before: (f32, Option<PropValue>) = (0.0, None);
        let mut after: (f32, Option<PropValue>) = (1.0, None);
        for (at, v) in stops {
            if at <= p {
                before = (at, Some(v));
            } else {
                after = (at, Some(v));
                break;
            }
        }
        let span = after.0 - before.0;
        let u = if span > 0.0 {
            ease(k.easing, ((p - before.0) / span).clamp(0.0, 1.0))
        } else {
            1.0
        };
        let mode = compose(prop);
        let value = if mode == Compose::Replace {
            let base = encode(prop, own.as_ref(), inh, b);
            let end = |v: &Option<PropValue>| match v {
                Some(v) => encode(prop, Some(v), inh, b),
                None => base.clone(),
            };
            match (end(&before.1), end(&after.1)) {
                (Some(x), Some(y)) => match lerp_enc(&x, &y, u) {
                    Some(e) => decode(prop, &e),
                    None => continue,
                },
                // Cannot interpolate (a gradient): steps.
                _ => match if u < 1.0 { before.1 } else { after.1 } {
                    Some(v) => v,
                    None => continue,
                },
            }
        } else {
            let identity = if mode == Compose::Add { 0.0 } else { 1.0 };
            // Lengths resolved as the springs resolve them (a
            // percentage `x` against the parent).
            let one = |v: &PropValue| match encode(prop, Some(v), inh, b) {
                Some(Enc::One([n])) if n.is_finite() => Some(n),
                _ => number(v),
            };
            let at = |v: &Option<PropValue>| v.as_ref().and_then(one).unwrap_or(identity);
            let k = at(&before.1) + (at(&after.1) - at(&before.1)) * u;
            let own_n = own.as_ref().and_then(one).unwrap_or(identity);
            let n = if mode == Compose::Add {
                own_n + k
            } else {
                own_n * k
            };
            decode(prop, &Enc::One([n]))
        };
        match own_at {
            Some(i) => props[i].1 = std::borrow::Cow::Owned(value),
            None => props.push((prop, std::borrow::Cow::Owned(value))),
        }
    }
}

/// Adds `delta` to `props`' `prop` (`x`, `y` or `rotate`; unset is 0),
/// resolved as the springs resolve it.
pub(super) fn offset(
    props: &mut Vec<(Prop, std::borrow::Cow<'_, PropValue>)>,
    prop: Prop,
    delta: f32,
    inh: Color,
    b: Extents,
) {
    if delta == 0.0 || !delta.is_finite() {
        return;
    }
    let at = props.iter().position(|(q, _)| *q == prop);
    let own = at
        .and_then(|i| match encode(prop, Some(props[i].1.as_ref()), inh, b) {
            Some(Enc::One([n])) if n.is_finite() => Some(n),
            _ => number(props[i].1.as_ref()),
        })
        .unwrap_or(0.0);
    let value = std::borrow::Cow::Owned(decode(prop, &Enc::One([own + delta])));
    match at {
        Some(i) => props[i].1 = value,
        None => props.push((prop, value)),
    }
}
