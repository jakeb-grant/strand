//! Theme swaps on the render thread (design.md, "How a swap animates").
//!
//! Logic re-resolves the token graph and sends the whole table in one
//! `SetTokens`; here only the palette roots move. Every plain colour of
//! the new table that differs from what is on screen springs from there,
//! in OKLab (premultiplied, as [`color_channels`]), along the table's
//! colour transition (`$motion.effects` for a `Default` swap). Each frame
//! writes the roots' values at its presentation time into the tree's
//! table, so flattening re-evaluates every derived token from them
//! exactly, and the contrast guard solves every declared text token
//! against its backgrounds as they are in that frame.
//!
//! Before it starts, a swap is played through at [`CHECK_STEP`] (and
//! [`CHECK_FINE`] around moments that only just make it): if at
//! some moment the backgrounds of a declared pair leave no text
//! lightness at 3:1 (one background too dark for dark text while
//! another is too light for light text, [`Color::contrast_reachable`]),
//! the swap does not spring. The table snaps, and each shown surface
//! crossfades from a snapshot of its old frame, rasterised once, to the
//! new frames along the same curve.
//!
//! What cannot interpolate snaps: every other plain token (lengths,
//! fonts, springs) takes its new value at once. A swap on no surface
//! shown with a clock, a table sent `Instant` (the boot table) and
//! `reduced_motion` snap everything.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use strand_scene::motion::{channels_color, color_channels};
use strand_scene::{
    Color, Curve, Damage, MIN_CONTRAST, Motion, PaintTarget, Prop, PropValue, SceneDiff, SceneOp,
    Size, SurfaceId, TokenScope, TokenTable, Transition, luminance_reachable,
};

use super::Renderer;

/// Settling tolerance of a palette root, in OKLab channels.
const ROOT_EPS: f32 = 0.0005;

/// Settling tolerance of a crossfade's progress.
const FADE_EPS: f32 = 0.002;

/// How finely a planned swap is played through for its contrast check
/// (240 Hz: a quarter of a 60 Hz frame).
pub const CHECK_STEP: Duration = Duration::from_micros(4_167);

/// Next to a moment that only just reaches 3:1, the check looks again
/// this finely (1 kHz), so a frame landing between two samples finds no
/// dip they missed.
pub const CHECK_FINE: Duration = Duration::from_micros(1_000);

/// How much of a swap the contrast check plays through at most; a
/// slower swap's tail (a `~ 10s` palette) is not checked.
pub const CHECK_SPAN: Duration = Duration::from_secs(3);

/// A moment that reaches [`MIN_CONTRAST`] but not this is "only just":
/// the moments around it are checked finely.
pub const CHECK_NEAR: f64 = 3.3;

#[derive(Clone, Debug)]
struct Root {
    motion: Motion<4>,
    /// Exactly what logic sent: written once the spring settles.
    target: Color,
}

/// A surface's old frame, rasterised once: tightly packed premultiplied
/// ARGB8888 at the buffer size it was painted at.
#[derive(Debug)]
pub(super) struct Snapshot {
    size: Size,
    pixels: Vec<u8>,
}

#[derive(Debug)]
struct Fade {
    /// 0 (the old frame) to 1 (the new one).
    progress: Motion<1>,
    snaps: HashMap<SurfaceId, Snapshot>,
}

/// The palette roots in flight and the crossfade, if any.
#[derive(Debug, Default)]
pub(super) struct Swap {
    roots: BTreeMap<String, Root>,
    fade: Option<Fade>,
    /// Surfaces whose last frame was blended with a snapshot: their next
    /// frame is painted in full.
    blended: HashSet<SurfaceId>,
    /// Render-thread work spent on swaps (planning, snapshots, sampling
    /// roots) since [`Renderer::take_swap_work`].
    work: Duration,
    /// Swaps that crossfaded (tests).
    crossfades: u64,
}

/// What a `SetTokens` will do, worked out before the diff applies (the
/// old table and the frames on screen are still there).
#[derive(Debug)]
pub(super) struct Plan {
    roots: BTreeMap<String, Root>,
    fade: Option<(Motion<1>, HashMap<SurfaceId, Snapshot>)>,
}

/// How a frame of a surface shows a crossfade.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(super) enum FadeFrame {
    /// No crossfade here.
    None,
    /// Blend the snapshot under the new frame, which has this weight.
    Blend(f32),
    /// The last frame was blended: this one repaints in full.
    Clean,
}

impl FadeFrame {
    /// The frame is painted in full.
    pub(super) fn full(self) -> bool {
        self != FadeFrame::None
    }
}

/// The opaque backgrounds of pair `text` over `bgs`, as evaluated in
/// `scope` (what the guard judges: translucent ones show what is under
/// them).
fn backgrounds(scope: &TokenScope<'_>, text: &str, bgs: &[String]) -> Vec<Color> {
    bgs.iter()
        .filter(|b| b.as_str() != text)
        .filter_map(|b| match scope.lookup(b) {
            Some(PropValue::Color(c)) if c.a >= 1.0 => Some(c),
            _ => None,
        })
        .collect()
}

fn reachable(table: &TokenTable, text: &str, bgs: &[String], min: f64) -> bool {
    let tables = [table];
    let scope = TokenScope::new(&tables);
    Color::contrast_reachable(&backgrounds(&scope, text, bgs), min)
}

/// Where a pair's background comes from while a swap is played
/// through.
enum Bg {
    /// A root in flight (an index into the simulated motions).
    Root(usize),
    /// The luminance of an opaque colour that does not move.
    Fixed(f64),
    /// A derived token: evaluated from the roots of the moment.
    Derived(String),
}

/// Plays `roots` through from `base` (the last frame shown) and says
/// whether some moment leaves a declared pair of `table` with no text
/// lightness at 3:1, a pair that has one in `old` and in `table` (a
/// palette that is unreadable at rest is not the swap's doing).
fn needs_crossfade(
    old: &TokenTable,
    table: &TokenTable,
    roots: &BTreeMap<String, Root>,
    base: Duration,
) -> bool {
    if roots.is_empty() {
        return false;
    }
    let paths: Vec<&str> = roots.keys().map(String::as_str).collect();
    let tables = [table];
    let scope = TokenScope::new(&tables);
    let pairs: Vec<Vec<Bg>> = table
        .contrast
        .iter()
        .filter(|(text, bgs)| {
            reachable(old, text, bgs, MIN_CONTRAST) && reachable(table, text, bgs, MIN_CONTRAST)
        })
        .map(|(text, bgs)| {
            bgs.iter()
                .filter(|b| *b != text)
                .filter_map(|b| {
                    if let Ok(i) = paths.binary_search(&b.as_str()) {
                        Some(Bg::Root(i))
                    } else if table.derived.contains_key(b) {
                        Some(Bg::Derived(b.clone()))
                    } else {
                        match scope.lookup(b) {
                            Some(PropValue::Color(c)) if c.a >= 1.0 => {
                                Some(Bg::Fixed(c.relative_luminance()))
                            }
                            _ => None,
                        }
                    }
                })
                .collect()
        })
        .collect();
    if pairs.is_empty() {
        return false;
    }
    // The roots the pairs read: directly, or all of them when a
    // background is derived (it may read any).
    let derived = pairs.iter().flatten().any(|b| matches!(b, Bg::Derived(_)));
    let mut read = vec![derived; paths.len()];
    for b in pairs.iter().flatten() {
        if let Bg::Root(i) = b {
            read[*i] = true;
        }
    }
    let mut sims: Vec<(usize, &str, Motion<4>)> = roots
        .iter()
        .enumerate()
        .filter(|(i, _)| read[*i])
        .map(|(i, (p, r))| (i, p.as_str(), r.motion.clone()))
        .collect();
    let mut scratch = derived.then(|| table.clone());
    // The first sample starts every retarget (as the first frame will);
    // after it the motions are pure functions of time.
    for (_, _, m) in &mut sims {
        m.sample(base + CHECK_STEP);
    }
    #[derive(Copy, Clone, PartialEq)]
    enum Moment {
        Readable,
        Near,
        Unreadable,
    }
    // Opaque roots' luminances this moment (NaN: translucent).
    let mut lum = vec![f64::NAN; paths.len()];
    let mut lums: Vec<f64> = Vec::new();
    let mut check = |at: Duration, settle: bool| -> (Moment, bool) {
        let mut settled = settle;
        for (i, path, m) in &sims {
            let c = channels_color(m.peek(at)).gamut_mapped();
            if settle {
                settled &= m.is_settled(at);
            }
            lum[*i] = if c.a >= 1.0 {
                c.relative_luminance()
            } else {
                f64::NAN
            };
            if let Some(slot) = scratch.as_mut().and_then(|t| t.tokens.get_mut(*path)) {
                *slot = PropValue::Color(c);
            }
        }
        let tables = scratch.as_ref().map(|t| [t]);
        let scope = tables.as_ref().map(|t| TokenScope::new(t));
        let mut moment = Moment::Readable;
        for pair in &pairs {
            lums.clear();
            lums.extend(pair.iter().filter_map(|b| match b {
                Bg::Root(i) => Some(lum[*i]).filter(|l| !l.is_nan()),
                Bg::Fixed(l) => Some(*l),
                Bg::Derived(path) => match scope.as_ref()?.lookup(path) {
                    Some(PropValue::Color(c)) if c.a >= 1.0 => Some(c.relative_luminance()),
                    _ => None,
                },
            }));
            if !luminance_reachable(&lums, MIN_CONTRAST) {
                return (Moment::Unreadable, settled);
            }
            if !luminance_reachable(&lums, CHECK_NEAR) {
                moment = Moment::Near;
            }
        }
        (moment, settled)
    };
    let steps = (CHECK_SPAN.as_nanos() / CHECK_STEP.as_nanos()) as u32;
    let fine = (CHECK_STEP.as_nanos() / CHECK_FINE.as_nanos()) as u32;
    let mut last = Moment::Readable;
    for k in 1..=steps {
        let at = base + CHECK_STEP * k;
        let (moment, settled) = check(at, true);
        if moment == Moment::Unreadable {
            return true;
        }
        // Either end of the step only just made it: the moments between
        // are looked at too.
        if k > 1 && (moment == Moment::Near || last == Moment::Near) {
            for j in 1..fine {
                if check(at - CHECK_FINE * j, false).0 == Moment::Unreadable {
                    return true;
                }
            }
        }
        last = moment;
        if settled {
            break;
        }
    }
    false
}

/// `new` (the new frame, in `target`) over the snapshot `old`, the new
/// frame weighted `w` (premultiplied channels, so translucent surfaces
/// fade too).
fn blend(target: &mut PaintTarget<'_>, old: &Snapshot, w: f32) {
    let a = (w.clamp(0.0, 1.0) * 256.0).round() as u32;
    if a >= 256 {
        return;
    }
    let row = old.size.w as usize * 4;
    let stride = target.stride as usize;
    for y in 0..old.size.h as usize {
        let dst = &mut target.pixels[y * stride..y * stride + row];
        let src = &old.pixels[y * row..(y + 1) * row];
        for (d, s) in dst.iter_mut().zip(src) {
            *d = ((*d as u32 * a + *s as u32 * (256 - a) + 128) >> 8) as u8;
        }
    }
}

impl Renderer {
    /// The latest presentation time a surface was shown at (with a
    /// clock), if any is.
    fn shown_at(&self) -> Option<Duration> {
        self.surfaces
            .values()
            .filter(|s| s.painted)
            .filter_map(|s| s.painted_time)
            .filter(|t| !t.is_zero())
            .max()
    }

    /// Works out what the last `SetTokens` of `diff` does, before the
    /// diff applies: which roots spring from where, or, when no spring
    /// keeps the declared pairs readable, the snapshots to crossfade
    /// from. `None` if the diff sends no table.
    pub(super) fn plan_swap(&mut self, diff: &SceneDiff) -> Option<Plan> {
        let (table, transition) = diff.ops.iter().rev().find_map(|op| match op {
            SceneOp::SetTokens { table, transition } => Some((table, transition)),
            _ => None,
        })?;
        let started = Instant::now();
        let reduced = self.anim.reduced()
            || matches!(table.get("motion.reduced"), Some(PropValue::Bool(true)));
        let shown_at = self.shown_at();
        let curve = match shown_at {
            Some(_) if !reduced && *transition != Transition::Instant => {
                let tables = [table];
                Curve::of(&TokenScope::new(&tables).transition(transition, Prop::Color))
            }
            _ => Curve::Instant,
        };
        let mut plan = Plan {
            roots: BTreeMap::new(),
            fade: None,
        };
        let Some(base) = shown_at.filter(|_| curve != Curve::Instant) else {
            self.swap.work += started.elapsed();
            return Some(plan);
        };
        let old = &self.tree.tokens;
        for (path, v) in &table.tokens {
            let PropValue::Color(target) = v else {
                continue;
            };
            let to = color_channels(*target);
            let motion = if let Some(r) = self.swap.roots.get(path) {
                // In flight: it turns towards the new target, keeping its
                // velocity.
                let mut m = r.motion.clone();
                m.retarget(to, curve);
                m
            } else {
                let from = match old.get(path) {
                    Some(PropValue::Color(c)) => *c,
                    Some(_) => continue,
                    None => match old.lookup(path) {
                        Some(PropValue::Color(c)) => c,
                        _ => continue,
                    },
                };
                if from == *target {
                    continue;
                }
                let mut m = Motion::rest(color_channels(from), ROOT_EPS).sampled_at(Some(base));
                m.retarget(to, curve);
                m
            };
            plan.roots.insert(
                path.clone(),
                Root {
                    motion,
                    target: *target,
                },
            );
        }
        if needs_crossfade(old, table, &plan.roots, base) {
            let mut progress = Motion::rest([0.0], FADE_EPS).sampled_at(Some(base));
            progress.retarget([1.0], curve);
            let snaps = self.snapshots();
            plan.roots.clear();
            plan.fade = Some((progress, snaps));
        }
        self.swap.work += started.elapsed();
        Some(plan)
    }

    /// The frame each surface shows now, rasterised once (surfaces shown
    /// with a clock that a crossfade does not already cover).
    fn snapshots(&mut self) -> HashMap<SurfaceId, Snapshot> {
        let ids: Vec<SurfaceId> = self
            .surfaces
            .iter()
            .filter(|(_, s)| {
                s.painted
                    && s.valid
                    && !s.size.is_empty()
                    && s.painted_time.is_some_and(|t| !t.is_zero())
            })
            .map(|(id, _)| *id)
            .filter(|id| {
                self.swap
                    .fade
                    .as_ref()
                    .is_none_or(|f| !f.snaps.contains_key(id))
            })
            .collect();
        let mut out = HashMap::new();
        for id in ids {
            let Some(s) = self.surfaces.get_mut(&id) else {
                continue;
            };
            let (size, scale, time, prev) = (s.size, s.scale, s.time, s.painted_time);
            let f = match s.cache.take() {
                Some(f) => f,
                None => {
                    // Flattened at rest as of the last frame (a preview:
                    // nothing starts).
                    self.anim.begin(time, prev, false);
                    self.flatten_now(id)
                }
            };
            let mut pixels = vec![0u8; size.w as usize * size.h as usize * 4];
            if let Ok(mut t) = PaintTarget::new(&mut pixels, size, size.w * 4, scale, 0) {
                self.raster
                    .paint(&f.items, &Damage::full(size), &self.atlas, scale, &mut t);
                out.insert(id, Snapshot { size, pixels });
            }
            if let Some(s) = self.surfaces.get_mut(&id) {
                s.cache = Some(f);
            }
        }
        out
    }

    /// Puts a plan in place once its table is the tree's: the roots
    /// spring from the next frame on (every frame writes their values),
    /// or the table stays snapped and the surfaces crossfade.
    pub(super) fn install_swap(&mut self, plan: Plan) {
        let started = Instant::now();
        match plan.fade {
            Some((progress, snaps)) => {
                self.swap.roots.clear();
                self.swap.crossfades += 1;
                match &mut self.swap.fade {
                    // Already fading from an older frame: it carries on
                    // from there, to the newest table.
                    Some(f) => f.snaps.extend(snaps),
                    None => self.swap.fade = Some(Fade { progress, snaps }),
                }
            }
            None => {
                if plan.roots.is_empty() {
                    // Snapped: a crossfade in flight ends too.
                    self.swap.fade = None;
                }
                self.swap.roots = plan.roots;
            }
        }
        self.swap.work += started.elapsed();
    }

    /// Writes the palette roots' values at `at` into the tree's table: a
    /// committed frame samples (starting retargets, letting settled roots
    /// go with their exact targets), a preview only peeks.
    pub(super) fn sample_tokens(&mut self, at: Duration, commit: bool) {
        if self.swap.roots.is_empty() {
            return;
        }
        let started = Instant::now();
        let snap = self.anim.reduced();
        let tokens = &mut self.tree.tokens.tokens;
        for (path, r) in self.swap.roots.iter_mut() {
            let ch = if snap {
                None
            } else if commit {
                Some(r.motion.sample(at))
            } else {
                Some(r.motion.peek(at))
            };
            let c = match ch {
                Some(ch) if !r.motion.is_settled(at) => channels_color(ch).gamut_mapped(),
                _ => r.target,
            };
            if let Some(slot) = tokens.get_mut(path) {
                *slot = PropValue::Color(c);
            }
        }
        if snap {
            self.swap.roots.clear();
        } else if commit {
            self.swap.roots.retain(|_, r| !r.motion.is_settled(at));
        }
        self.swap.work += started.elapsed();
    }

    /// The swap keeps `surface` moving: roots in flight, a crossfade on
    /// it, or a blended frame to clean up.
    pub(super) fn swap_moving(&self, surface: SurfaceId) -> bool {
        !self.swap.roots.is_empty()
            || self.swap.blended.contains(&surface)
            || self
                .swap
                .fade
                .as_ref()
                .is_some_and(|f| f.snaps.contains_key(&surface))
    }

    /// How the frame of `surface` at `at`, `size` shows the crossfade.
    pub(super) fn fade_frame(&mut self, surface: SurfaceId, at: Duration, size: Size) -> FadeFrame {
        if self.anim.reduced() {
            self.swap.fade = None;
        }
        if let Some(f) = &mut self.swap.fade {
            let p = f.progress.sample(at)[0];
            if f.progress.is_settled(at) {
                self.swap.fade = None;
            } else if f.snaps.get(&surface).is_some_and(|s| s.size == size) {
                self.swap.blended.insert(surface);
                return FadeFrame::Blend(p.clamp(0.0, 1.0));
            } else {
                // Resized since: it shows the new frames as they are.
                f.snaps.remove(&surface);
            }
        }
        if self.swap.blended.remove(&surface) {
            FadeFrame::Clean
        } else {
            FadeFrame::None
        }
    }

    /// Blends `surface`'s snapshot under the frame just rasterised.
    pub(super) fn blend_fade(&mut self, surface: SurfaceId, w: f32, target: &mut PaintTarget<'_>) {
        let started = Instant::now();
        if let Some(snap) = self
            .swap
            .fade
            .as_ref()
            .and_then(|f| f.snaps.get(&surface))
            .filter(|s| s.size == target.size)
        {
            blend(target, snap, w);
        }
        self.swap.work += started.elapsed();
    }

    /// Forgets a detached surface's crossfade.
    pub(super) fn forget_fade(&mut self, surface: SurfaceId) {
        self.swap.blended.remove(&surface);
        if let Some(f) = &mut self.swap.fade {
            f.snaps.remove(&surface);
        }
    }

    /// Render-thread work spent on theme swaps since the last call:
    /// planning (the contrast play-through, snapshots), and every frame's
    /// root springs and crossfade blends (the bench in
    /// `tests/theme_swap.rs` adds logic's re-resolve and the per-frame
    /// token evaluation).
    #[doc(hidden)]
    pub fn take_swap_work(&mut self) -> Duration {
        std::mem::take(&mut self.swap.work)
    }

    /// Theme swaps that crossfaded instead of springing.
    pub fn swap_crossfades(&self) -> u64 {
        self.swap.crossfades
    }

    /// True while palette roots spring or a crossfade runs.
    pub fn swapping(&self) -> bool {
        !self.swap.roots.is_empty() || self.swap.fade.is_some()
    }
}
