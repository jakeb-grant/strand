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
//! [`CHECK_FINE`] around moments that only just make it), in the global
//! scope and under each `set { }` chain of the nodes shown: if at some
//! moment the backgrounds of a declared pair leave no text lightness at
//! 3:1 (one background too dark for dark text while another is too
//! light for light text, [`Color::contrast_reachable`]), the swap does
//! not spring. The table snaps, and each shown surface crossfades from a
//! snapshot of its old frame (taken once, at the fade's first frame on
//! it) to the new frames along the same curve, read at each surface's
//! own presentation time.
//!
//! What cannot interpolate snaps: every other plain token (lengths,
//! fonts, springs) takes its new value at once. A swap on no surface
//! shown with a clock, a table sent `Instant` (the boot table) and
//! `reduced_motion` snap everything.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use strand_scene::motion::{channels_color, color_channels};
use strand_scene::{
    Color, Curve, Damage, MIN_CONTRAST, Motion, PaintTarget, Prop, PropValue, Scale, SceneDiff,
    SceneOp, Size, SurfaceId, TokenScope, TokenTable, Transition, luminance_reachable,
};

use super::{Flattened, Renderer};

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

/// How much of a swap the contrast check plays through at most: a swap
/// whose roots have not settled by then (a `~ 20s` palette) is not
/// sprung unchecked, it crossfades.
pub const CHECK_SPAN: Duration = Duration::from_secs(10);

/// Most distinct `set { }` scopes a swap's contrast check plays through
/// besides the global one; past it the swap crossfades (a check bounded
/// in work).
pub const CHECK_SCOPES: usize = 32;

/// Largest snapshot a crossfade keeps for one surface (a 1920×1080
/// buffer); a larger surface (a 4K scrim or overlay) snaps to the new
/// frame instead of fading.
pub const SNAPSHOT_MAX: usize = 1920 * 1080 * 4;

/// Most bytes all of a crossfade's snapshots keep at once (two 1440p
/// bars and a launcher at 2× fit); surfaces past it snap.
pub const SNAPSHOTS_MAX: usize = SNAPSHOT_MAX;

/// A moment that reaches [`MIN_CONTRAST`] but not this is "only just":
/// the moments around it are checked finely.
pub const CHECK_NEAR: f64 = 3.3;

#[derive(Clone, Debug)]
struct Root {
    motion: Motion<4>,
    /// Exactly what logic sent: written once the spring settles.
    target: Color,
}

/// A surface's old frame: tightly packed premultiplied ARGB8888 at the
/// buffer size it was painted at. Taken at the fade's first frame on the
/// surface: copied from the buffer it paints into when that still holds
/// the old frame (age 1), else rasterised once from the old frame's
/// flattened scene, kept from when the swap was planned.
#[derive(Debug)]
pub(super) struct Snapshot {
    size: Size,
    scale: Scale,
    /// Empty until taken.
    pixels: Vec<u8>,
    old: Option<Flattened>,
}

impl Snapshot {
    fn bytes(&self) -> usize {
        self.size.w as usize * self.size.h as usize * 4
    }
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
    /// roots and evaluating the frame's token graph) since
    /// [`Renderer::take_swap_work`].
    work: Duration,
    /// Time spent blending crossfade frames since
    /// [`Renderer::take_fade_blend_work`] (frame cost, kept apart).
    blend: Duration,
    /// Swaps that crossfaded (tests).
    crossfades: u64,
}

/// What a `SetTokens` will do, worked out before the diff applies (the
/// old table and the frames on screen are still there).
#[derive(Debug)]
pub(super) struct Plan {
    roots: BTreeMap<String, Root>,
    fade: Option<(Motion<1>, HashMap<SurfaceId, Snapshot>)>,
    /// The table snaps (sent `Instant`, `reduced_motion`, nothing shown
    /// yet): a crossfade in flight ends with it. A plan with no roots
    /// because no colour changed (a font, a length) leaves it running.
    snap: bool,
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

/// Whether `text` can reach `min` over `bgs` with `table` as the global
/// table under the `set { }` overrides `over`.
fn reachable(
    table: &TokenTable,
    over: &[TokenTable],
    text: &str,
    bgs: &[String],
    min: f64,
) -> bool {
    let tables: Vec<&TokenTable> = std::iter::once(table).chain(over).collect();
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
    /// A derived token, or one a `set { }` override defines: evaluated
    /// in its scope from the roots of the moment.
    Derived(String),
}

/// Plays `roots` through from `base` (the last frame shown) and says
/// whether some moment leaves a declared pair of `table` with no text
/// lightness at 3:1, in the global scope or under one of the `set { }`
/// override chains `scopes` of the nodes shown: a pair that has one in
/// `old` and in `table` there (a palette that is unreadable at rest is
/// not the swap's doing).
fn needs_crossfade(
    old: &TokenTable,
    table: &TokenTable,
    scopes: &[Vec<TokenTable>],
    roots: &BTreeMap<String, Root>,
    base: Duration,
) -> bool {
    if roots.is_empty() {
        return false;
    }
    if scopes.len() > CHECK_SCOPES {
        return true;
    }
    let paths: Vec<&str> = roots.keys().map(String::as_str).collect();
    let global: [Vec<TokenTable>; 1] = [Vec::new()];
    // (the scope's overrides, its pairs' backgrounds)
    let mut pairs: Vec<(&[TokenTable], Vec<Bg>)> = Vec::new();
    for over in global.iter().chain(scopes) {
        let tables: Vec<&TokenTable> = std::iter::once(table).chain(over).collect();
        let scope = TokenScope::new(&tables);
        let scoped = |b: &str| {
            over.iter()
                .any(|t| t.get(b).is_some() || t.derived.contains_key(b))
        };
        for (text, bgs) in &table.contrast {
            if !(reachable(old, over, text, bgs, MIN_CONTRAST)
                && reachable(table, over, text, bgs, MIN_CONTRAST))
            {
                continue;
            }
            let bgs = bgs
                .iter()
                .filter(|b| *b != text)
                .filter_map(|b| {
                    if scoped(b) || table.derived.contains_key(b) {
                        Some(Bg::Derived(b.clone()))
                    } else if let Ok(i) = paths.binary_search(&b.as_str()) {
                        Some(Bg::Root(i))
                    } else {
                        match scope.lookup(b) {
                            Some(PropValue::Color(c)) if c.a >= 1.0 => {
                                Some(Bg::Fixed(c.relative_luminance()))
                            }
                            _ => None,
                        }
                    }
                })
                .collect();
            pairs.push((over.as_slice(), bgs));
        }
    }
    if pairs.is_empty() {
        return false;
    }
    // The roots the pairs read: directly, or all of them when a
    // background is derived (it may read any).
    let derived = pairs
        .iter()
        .flat_map(|(_, p)| p)
        .any(|b| matches!(b, Bg::Derived(_)));
    let mut read = vec![derived; paths.len()];
    for b in pairs.iter().flat_map(|(_, p)| p) {
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
        let mut moment = Moment::Readable;
        for (over, pair) in &pairs {
            let tables: Option<Vec<&TokenTable>> = scratch
                .as_ref()
                .map(|t| std::iter::once(t).chain(over.iter()).collect());
            let scope = tables.as_deref().map(TokenScope::new);
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
            return false;
        }
    }
    // Still moving after the longest span checked: not sprung unchecked.
    true
}

/// `new` (the new frame, in `target`) over the snapshot `old`, the new
/// frame weighted `w` (premultiplied channels, so translucent surfaces
/// fade too).
fn blend(target: &mut PaintTarget<'_>, old: &Snapshot, w: f32) {
    let a = (w.clamp(0.0, 1.0) * 256.0).round() as u32;
    if a >= 256 || old.pixels.len() != old.bytes() {
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
            snap: false,
        };
        let Some(base) = shown_at.filter(|_| curve != Curve::Instant) else {
            plan.snap = true;
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
        let scopes = if plan.roots.is_empty() {
            Vec::new()
        } else {
            self.shown_scopes()
        };
        let old = &self.tree.tokens;
        if needs_crossfade(old, table, &scopes, &plan.roots, base) {
            let mut progress = Motion::rest([0.0], FADE_EPS).sampled_at(Some(base));
            progress.retarget([1.0], curve);
            let snaps = self.snapshots();
            plan.roots.clear();
            plan.fade = Some((progress, snaps));
        }
        self.swap.work += started.elapsed();
        Some(plan)
    }

    /// The surfaces shown with a clock.
    fn shown_with_clock(&self) -> impl Iterator<Item = (SurfaceId, &super::SurfaceState)> {
        self.surfaces
            .iter()
            .filter(|(_, s)| {
                s.painted
                    && s.valid
                    && !s.size.is_empty()
                    && s.painted_time.is_some_and(|t| !t.is_zero())
            })
            .map(|(id, s)| (*id, s))
    }

    /// The distinct `set { }` override chains (outermost first) of the
    /// nodes the shown surfaces draw, at most one past [`CHECK_SCOPES`]
    /// (enough to know there are too many).
    fn shown_scopes(&self) -> Vec<Vec<TokenTable>> {
        let mut out: Vec<Vec<TokenTable>> = Vec::new();
        let mut seen: HashSet<strand_scene::NodeId> = HashSet::new();
        let mut stack: Vec<strand_scene::NodeId> =
            self.shown_with_clock().map(|(_, s)| s.root).collect();
        while let Some(id) = stack.pop() {
            if out.len() > CHECK_SCOPES || !seen.insert(id) {
                continue;
            }
            let Some(n) = self.tree.get(id) else {
                continue;
            };
            if n.get(Prop::Tokens).is_some() {
                let chain: Vec<TokenTable> = crate::flatten::scope_tables(&self.tree, id)
                    .into_iter()
                    .skip(1)
                    .cloned()
                    .collect();
                if !out.contains(&chain) {
                    out.push(chain);
                }
            }
            stack.extend(n.children.iter().copied());
        }
        out
    }

    /// The frame each surface shows now, rasterised once (surfaces shown
    /// with a clock that a crossfade does not already cover, smallest
    /// first, within [`SNAPSHOT_MAX`] and [`SNAPSHOTS_MAX`]: the others
    /// snap to the new frame).
    fn snapshots(&mut self) -> HashMap<SurfaceId, Snapshot> {
        let mut ids: Vec<(usize, SurfaceId)> = self
            .shown_with_clock()
            .map(|(id, s)| (s.size.w as usize * s.size.h as usize * 4, id))
            .filter(|(_, id)| {
                self.swap
                    .fade
                    .as_ref()
                    .is_none_or(|f| !f.snaps.contains_key(id))
            })
            .collect();
        ids.sort();
        let mut kept = self
            .swap
            .fade
            .as_ref()
            .map_or(0, |f| f.snaps.values().map(Snapshot::bytes).sum::<usize>());
        let mut out = HashMap::new();
        for (bytes, id) in ids {
            if bytes > SNAPSHOT_MAX || kept + bytes > SNAPSHOTS_MAX {
                continue;
            }
            kept += bytes;
            let Some(s) = self.surfaces.get_mut(&id) else {
                continue;
            };
            let (size, scale, time, prev) = (s.size, s.scale, s.time, s.painted_time);
            // The old frame's scene, for when its buffer no longer holds
            // it (a moving surface paints a fresh scene anyway).
            let f = match s.cache.take() {
                Some(f) => f,
                None => {
                    // Flattened at rest as of the last frame (a preview:
                    // nothing starts).
                    self.anim.begin(time, prev, false);
                    self.flatten_now(id)
                }
            };
            out.insert(
                id,
                Snapshot {
                    size,
                    scale,
                    pixels: Vec::new(),
                    old: Some(f),
                },
            );
        }
        out
    }

    /// Takes `surface`'s snapshot if its fade has not yet, before the
    /// frame in `target` is painted (see [`Snapshot`]).
    pub(super) fn take_snapshot(&mut self, surface: SurfaceId, target: &PaintTarget<'_>) {
        let Some(snap) = self
            .swap
            .fade
            .as_mut()
            .and_then(|f| f.snaps.get_mut(&surface))
            .filter(|s| s.old.is_some())
        else {
            return;
        };
        let started = Instant::now();
        let valid = self.surfaces.get(&surface).is_some_and(|s| s.valid);
        let row = snap.size.w as usize * 4;
        let mut pixels = vec![0u8; snap.bytes()];
        if valid && target.age == 1 && target.size == snap.size && target.scale == snap.scale {
            // The buffer still shows the old frame.
            let stride = target.stride as usize;
            for (y, dst) in pixels.chunks_exact_mut(row).enumerate() {
                dst.copy_from_slice(&target.pixels[y * stride..y * stride + row]);
            }
        } else if let Some(old) = &snap.old
            && let Ok(mut t) =
                PaintTarget::new(&mut pixels, snap.size, snap.size.w * 4, snap.scale, 0)
        {
            self.raster.paint(
                &old.items,
                &Damage::full(snap.size),
                &self.atlas,
                snap.scale,
                &mut t,
            );
        }
        snap.pixels = pixels;
        snap.old = None;
        self.swap.work += started.elapsed();
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
                if plan.snap {
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
        // The frame's token graph, evaluated once from these roots: the
        // nodes read it (derived tokens and guarded text exact).
        self.tree.tokens.freeze();
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
            if let Some(snap) = f.snaps.get(&surface) {
                // The first frame starts it; after that the progress is
                // a function of each surface's own clock (one on another
                // output fades on at its own times).
                if f.progress.is_pending() {
                    f.progress.sample(at);
                }
                let p = f.progress.peek(at)[0];
                if !f.progress.is_settled(at) && snap.size == size {
                    self.swap.blended.insert(surface);
                    return FadeFrame::Blend(p.clamp(0.0, 1.0));
                }
                // Done here, or resized since: it shows the new frames
                // as they are.
                f.snaps.remove(&surface);
            }
            if f.snaps.is_empty() {
                self.swap.fade = None;
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
        self.swap.blend += started.elapsed();
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
    /// root springs and the token graph evaluated from them (the bench in
    /// `tests/theme_swap_bench.rs` adds logic's re-resolve and the rest
    /// of applying the table).
    #[doc(hidden)]
    pub fn take_swap_work(&mut self) -> Duration {
        std::mem::take(&mut self.swap.work)
    }

    /// Time spent blending crossfade frames with their snapshots since
    /// the last call (a cost of each crossfade frame, like painting it).
    #[doc(hidden)]
    pub fn take_fade_blend_work(&mut self) -> Duration {
        std::mem::take(&mut self.swap.blend)
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
