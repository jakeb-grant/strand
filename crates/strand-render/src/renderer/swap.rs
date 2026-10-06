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
//! Before it starts, a swap is played through (at [`CHECK_STEP`] while
//! the roots move fast, at up to [`CHECK_STEP_MAX`] while they move
//! slowly, and [`CHECK_FINE`] times as finely, for the pairs concerned,
//! around moments that only just make it),
//! in the global scope first and then under each `set { }` scope of the
//! nodes shown whose overrides reach a declared background (a scope that
//! appears while the roots spring, on a node given `tokens`, a subtree
//! moved or a surface attached, is played through then, from the roots'
//! motions as they are): if at some moment the
//! backgrounds of a declared pair leave no text lightness at 3:1 (one
//! background too dark for dark text while another is too light for
//! light text, [`Color::contrast_reachable`]), that scope cannot spring.
//! In the global scope, the table snaps and every shown surface
//! crossfades from a snapshot of its old frame to the new frames. Under
//! a `set { }` scope, only the surfaces drawing it crossfade: they show
//! the new table at once (held for them while the roots spring
//! elsewhere), from their snapshots (a surface attached mid-swap showed
//! nothing old: it just shows the new table). The play-through's work is
//! bounded ([`CHECK_WORK`]): the global scope runs out of it only with
//! every surface crossfading, the `set { }` scopes left unchecked when it
//! runs out only with their own surfaces. A snapshot is taken once, at the
//! fade's first frame on its surface, and each surface fades along the
//! colour curve from its own first frame, at its own presentation times.
//!
//! What cannot interpolate snaps: every other plain token (lengths,
//! fonts, springs) takes its new value at once. A swap on no surface
//! shown with a clock, a table sent `Instant` (the boot table) and
//! `reduced_motion` snap everything.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use strand_scene::motion::{channels_color, color_channels};
use strand_scene::{
    Color, Curve, Damage, MIN_CONTRAST, Motion, NodeId, PaintTarget, Prop, PropValue, Scale,
    SceneDiff, SceneOp, Size, SurfaceId, TokenExpr, TokenScope, TokenTable, Transition,
    luminance_reachable,
};

use super::{Flattened, Renderer};

/// Settling tolerance of a palette root, in OKLab channels.
const ROOT_EPS: f32 = 0.0005;

/// Settling tolerance of a crossfade's progress.
const FADE_EPS: f32 = 0.002;

/// How finely a planned swap is played through for its contrast check
/// while its roots move fast (240 Hz: a quarter of a 60 Hz frame).
pub const CHECK_STEP: Duration = Duration::from_micros(4_167);

/// The longest step of the play-through, while the roots move slowly
/// (a slow `$motion.effects`, a spring's tail): the step grows (at most
/// doubling) while no root moves more than [`CHECK_MOVE`] over it.
pub const CHECK_STEP_MAX: Duration = Duration::from_millis(100);

/// Most a root may move (in OKLab channels) over one step of the
/// play-through, as its speed at a sample and over the last step say.
/// A background moving 0.02 in OKLab lightness changes the contrast it
/// allows by at most about 9%, within the 10% between [`MIN_CONTRAST`]
/// and [`CHECK_NEAR`]: a moment between two samples that both clear
/// [`CHECK_NEAR`] still clears 3:1, and steps with an end below it are
/// looked at [`CHECK_FINE`] times as finely. (The design's
/// `$motion.effects` moves up to about 0.04 per [`CHECK_STEP`] at its
/// fastest, so it is sampled at [`CHECK_STEP`] throughout but its
/// tail.)
pub const CHECK_MOVE: f32 = 0.02;

/// Next to a moment that only just reaches 3:1, the check looks again
/// at the pairs concerned this many times as finely (1 kHz while the
/// roots move fast), so a frame landing between two samples finds no
/// dip they missed.
pub const CHECK_FINE: u32 = 4;

/// How much of a swap the contrast check plays through at most: a swap
/// whose roots have not settled by then (a `~ 20s` palette) is not
/// sprung unchecked, it crossfades.
pub const CHECK_SPAN: Duration = Duration::from_secs(10);

/// Most distinct `set { }` scopes a swap's contrast check plays through
/// besides the global one (counting only scopes whose overrides reach a
/// declared background, merged when those overrides are the same);
/// surfaces drawing more crossfade.
pub const CHECK_SCOPES: usize = 32;

/// The play-through's work at most, in units of about 0.44 µs
/// optimised (`theme_swap_bench`: some 3.4 ms over about 7,800 units):
/// per sample, one per root sampled and per pair judged, and two per
/// background evaluated from the roots (a derived or overridden one).
/// The global scope is played through first, with all of it; the `set {
/// }` scopes get what it leaves. Where it runs out, the scopes not yet
/// cleared crossfade (all surfaces if the global scope is one of them),
/// so planning stays within a fixed share of the 5 ms budget however
/// slow or bouncy the spring and however many scopes are shown.
pub const CHECK_WORK: u32 = 9_000;

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

/// Most distinct `set { }` chains looked at under one surface root;
/// a root showing more crossfades.
const RAW_SCOPES: usize = 256;

#[derive(Clone, Debug)]
struct Root {
    motion: Motion<4>,
    /// Exactly what logic sent: written once the spring settles.
    target: Color,
}

/// A surface's old frame: tightly packed premultiplied ARGB8888 at the
/// buffer size it was painted at, and its own fade. Taken at the fade's
/// first frame on the surface: copied from the buffer it paints into,
/// with what the buffer missed since (its age) drawn again from the old
/// frame's flattened scene, kept from when the swap was planned; drawn
/// in full when the buffer is new.
#[derive(Debug)]
pub(super) struct Snapshot {
    size: Size,
    scale: Scale,
    /// Empty until taken; while `under` is set, the snapshot of the fade
    /// this one replaces.
    pixels: Vec<u8>,
    old: Option<Flattened>,
    /// A crossfade landing mid-crossfade: the surface shows `old`'s frame
    /// weighted this much over `pixels`, and that blend is the snapshot.
    under: Option<f32>,
    /// 0 (the old frame) to 1 (the new one); started by the surface's
    /// first fade frame.
    progress: Motion<1>,
    /// The new frame's weight in the last frame blended.
    last_w: f32,
    /// When the surface last painted a frame of the fade (or the fade was
    /// planned): a surface that paints nothing for the exit stall
    /// (asleep, occluded) loses its snapshot.
    since: Instant,
}

impl Snapshot {
    fn bytes(&self) -> usize {
        self.size.w as usize * self.size.h as usize * 4
    }
}

/// The palette roots in flight and the crossfades, if any.
#[derive(Debug, Default)]
pub(super) struct Swap {
    roots: BTreeMap<String, Root>,
    /// Surfaces crossfading, and from what.
    fade: HashMap<SurfaceId, Snapshot>,
    /// While roots spring: the newest table, for the surfaces in
    /// `held_for` (a `set { }` scope there no spring keeps readable).
    held: Option<TokenTable>,
    held_for: HashSet<SurfaceId>,
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
    /// Swaps that crossfaded on some surface (tests).
    crossfades: u64,
    /// The colour curve of the swap in flight (crossfades that start
    /// while it springs fade along it).
    curve: Option<Curve>,
}

/// What a `SetTokens` will do, worked out before the diff applies (the
/// old table and the frames on screen are still there).
#[derive(Debug)]
pub(super) struct Plan {
    roots: BTreeMap<String, Root>,
    /// The table snaps (sent `Instant`, `reduced_motion`, nothing shown
    /// yet): crossfades in flight end with it. A plan with no roots
    /// because no colour changed (a font, a length) leaves them running.
    snap: bool,
    /// No spring keeps the global scope readable: the table snaps and
    /// every shown surface crossfades.
    all: bool,
    /// Otherwise, the surfaces shown the new table at once while the
    /// roots spring (empty: none).
    hold: HashSet<SurfaceId>,
    /// The snapshots of the surfaces that crossfade.
    snaps: HashMap<SurfaceId, Snapshot>,
    /// The colour curve.
    curve: Curve,
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

/// Whether `text` can reach 3:1 over `bgs` both in `from` (the old
/// table's scope) and in `to` (the new one's), each background's
/// luminance looked up once per scope (`memo`).
fn readable_in(
    [from, to]: [&TokenScope<'_>; 2],
    memo: &mut [HashMap<String, Option<f64>>; 2],
    text: &str,
    bgs: &[String],
) -> bool {
    let mut lums = Vec::with_capacity(bgs.len());
    for (scope, memo) in [from, to].into_iter().zip(memo.iter_mut()) {
        lums.clear();
        for b in bgs.iter().filter(|b| b.as_str() != text) {
            let l = *memo
                .entry(b.clone())
                .or_insert_with(|| match scope.lookup(b) {
                    Some(PropValue::Color(c)) if c.a >= 1.0 => Some(c.relative_luminance()),
                    _ => None,
                });
            lums.extend(l);
        }
        if !luminance_reachable(&lums, MIN_CONTRAST) {
            return false;
        }
    }
    true
}

/// The paths a prop value reads.
fn value_refs(v: &PropValue, out: &mut Vec<String>) {
    match v {
        PropValue::Token(e) => expr_refs(e, out),
        PropValue::List(items) | PropValue::Call { args: items, .. } => {
            for i in items {
                value_refs(i, out);
            }
        }
        PropValue::Pose(props) => {
            for (_, v) in props {
                value_refs(v, out);
            }
        }
        _ => {}
    }
}

/// The paths an expression reads.
fn expr_refs(e: &TokenExpr, out: &mut Vec<String>) {
    match e {
        TokenExpr::Ref(p) => out.push(p.clone()),
        TokenExpr::Value(v) => value_refs(v, out),
        TokenExpr::Method { receiver, args, .. } => {
            expr_refs(receiver, out);
            for a in args {
                expr_refs(a, out);
            }
        }
        TokenExpr::OklchFrom {
            base,
            l,
            c,
            h,
            alpha,
        } => {
            expr_refs(base, out);
            for e in [l, c, h, alpha].into_iter().flatten() {
                expr_refs(e, out);
            }
        }
        TokenExpr::Channel(_) => {}
        TokenExpr::Binary { lhs, rhs, .. } => {
            expr_refs(lhs, out);
            expr_refs(rhs, out);
        }
        TokenExpr::Template { value, colors } => {
            value_refs(value, out);
            for e in colors.iter().flatten() {
                expr_refs(e, out);
            }
        }
    }
}

/// Every path evaluating `path` in the scope `levels` may read, itself
/// included: definitions at every level are followed, and a declared
/// text token reads its backgrounds (the guard).
fn reads(levels: &[&TokenTable], path: &str) -> HashSet<String> {
    let mut seen = HashSet::new();
    let mut stack = vec![path.to_string()];
    let mut refs = Vec::new();
    while let Some(p) = stack.pop() {
        if seen.contains(&p) {
            continue;
        }
        for t in levels {
            if let Some(v) = t.tokens.get(&p) {
                value_refs(v, &mut refs);
            }
            if let Some(e) = t.derived.get(&p) {
                expr_refs(e, &mut refs);
            }
        }
        if let Some(bgs) = levels.first().and_then(|t| t.contrast.get(&p)) {
            refs.extend(bgs.iter().cloned());
        }
        seen.insert(p);
        stack.append(&mut refs);
    }
    seen
}

/// A definition of a path at one level (scope keys).
#[derive(Clone, Debug, PartialEq)]
enum Def {
    Plain(PropValue),
    Derived(TokenExpr),
}

/// A `set { }` scope of the nodes shown: its override chain (outermost
/// first) and the surfaces drawing it.
pub(super) struct ShownScope<'a> {
    chain: Vec<&'a TokenTable>,
    surfaces: Vec<SurfaceId>,
}

/// Which surfaces a swap cannot spring on.
#[derive(Debug, Default)]
struct Verdict {
    /// The global scope: all of them (the table snaps).
    all: bool,
    /// Surfaces drawing a `set { }` scope no spring keeps readable.
    surfaces: HashSet<SurfaceId>,
}

/// Where a pair's background comes from while a swap is played
/// through.
#[derive(Clone, Debug, PartialEq)]
enum Src {
    /// A root in flight (an index into the roots).
    Root(usize),
    /// The luminance of an opaque colour that does not move.
    Fixed(f64),
    /// A background that reads a root in flight through a derived token
    /// or a `set { }` override: evaluated once per sample.
    Slot(usize),
    /// Translucent or unset: not judged.
    Skip,
}

/// The play-through of one swap.
struct Play<'a> {
    paths: Vec<&'a str>,
    /// The roots the pairs read: (index into `paths`, motion).
    sims: Vec<(usize, Motion<4>)>,
    /// The new table with the roots of the moment written in.
    scratch: TokenTable,
    /// Override chains of the scopes checked (index 0: global, empty).
    chains: Vec<Vec<&'a TokenTable>>,
    /// The scopes' surfaces.
    owners: Vec<Vec<SurfaceId>>,
    /// Backgrounds evaluated once per sample: (scope, path).
    slots: Vec<(usize, String)>,
    /// Pairs to judge: (scope, backgrounds).
    pairs: Vec<(usize, Vec<Src>)>,
    /// Per moment: luminance of each root (NaN: translucent) and slot,
    /// and the moment each slot was last evaluated at.
    lum: Vec<f64>,
    slot_lum: Vec<f64>,
    slot_at: Vec<u32>,
    moments: u32,
    /// Pairs only just readable at the last sample, and at this one.
    near_last: Vec<bool>,
    near: Vec<bool>,
    /// Scopes found unreadable.
    failed: Vec<bool>,
    /// Scopes with a pair to judge.
    judged: Vec<bool>,
    /// The pass: the global scope's pairs (true) or the `set { }`
    /// scopes'.
    global: bool,
    /// Work left (see [`CHECK_WORK`]).
    work: u32,
    /// The channels of the last sample (how fast the roots move).
    last: Vec<[f32; 4]>,
}

impl<'a> Play<'a> {
    /// Sets up the play-through of `roots` into `table` from `old`, in
    /// the global scope and the `scopes` shown. `None` when nothing
    /// declared moves. Scopes past [`CHECK_SCOPES`] fail at once.
    fn new(
        old: &TokenTable,
        table: &'a TokenTable,
        scopes: &[ShownScope<'a>],
        roots: &'a BTreeMap<String, Root>,
    ) -> Option<Play<'a>> {
        let paths: Vec<&str> = roots.keys().map(String::as_str).collect();
        let moving = |reads: &HashSet<String>| paths.iter().any(|p| reads.contains(*p));
        let mut play = Play {
            paths: paths.clone(),
            sims: Vec::new(),
            scratch: table.clone(),
            chains: vec![Vec::new()],
            owners: vec![Vec::new()],
            slots: Vec::new(),
            pairs: Vec::new(),
            lum: vec![f64::NAN; paths.len()],
            slot_lum: Vec::new(),
            slot_at: Vec::new(),
            moments: 0,
            near_last: Vec::new(),
            near: Vec::new(),
            failed: vec![false],
            judged: Vec::new(),
            global: true,
            work: CHECK_WORK,
            last: Vec::new(),
        };
        // Each background's reads in the global scope.
        let mut global_reads: HashMap<&str, HashSet<String>> = HashMap::new();
        for (text, bgs) in &table.contrast {
            for b in bgs.iter().filter(|b| *b != text) {
                global_reads
                    .entry(b.as_str())
                    .or_insert_with(|| reads(&[table], b));
            }
        }
        // Global sources.
        let global_scope = [table];
        let scope = TokenScope::new(&global_scope);
        let mut global_src: HashMap<&str, Src> = HashMap::new();
        for (b, r) in &global_reads {
            let src = if let (Some(PropValue::Color(_)), Ok(i)) =
                (table.get(b), paths.binary_search(b))
            {
                Src::Root(i)
            } else if moving(r) {
                play.slot(0, b)
            } else {
                match scope.lookup(b) {
                    Some(PropValue::Color(c)) if c.a >= 1.0 => Src::Fixed(c.relative_luminance()),
                    _ => Src::Skip,
                }
            };
            global_src.insert(b, src);
        }
        let olds = [old];
        let old_scope = TokenScope::new(&olds);
        let mut memo = [HashMap::new(), HashMap::new()];
        for (text, bgs) in &table.contrast {
            if !readable_in([&old_scope, &scope], &mut memo, text, bgs) {
                continue;
            }
            let srcs: Vec<Src> = bgs
                .iter()
                .filter(|b| *b != text)
                .map(|b| global_src.get(b.as_str()).cloned().unwrap_or(Src::Skip))
                .collect();
            play.add_pair(0, srcs);
        }
        // `set { }` scopes: only those whose overrides reach a background,
        // merged by the overrides that do.
        let mut keys: Vec<Vec<(String, Vec<Def>)>> = Vec::new();
        for s in scopes {
            let overridden: HashSet<&str> = s
                .chain
                .iter()
                .flat_map(|t| t.tokens.keys().chain(t.derived.keys()))
                .map(String::as_str)
                .collect();
            let affected = |b: &str| {
                global_reads
                    .get(b)
                    .is_some_and(|r| r.iter().any(|p| overridden.contains(p.as_str())))
            };
            if !table
                .contrast
                .iter()
                .any(|(text, bgs)| bgs.iter().any(|b| b != text && affected(b)))
            {
                continue;
            }
            let mut levels: Vec<&TokenTable> = vec![table];
            levels.extend(s.chain.iter().copied());
            // What the scope's backgrounds read, through its overrides
            // too, and the overrides of those paths: the scope's key.
            let mut scope_reads: BTreeMap<String, HashSet<String>> = BTreeMap::new();
            for (text, bgs) in &table.contrast {
                for b in bgs.iter().filter(|b| *b != text && affected(b)) {
                    scope_reads
                        .entry(b.clone())
                        .or_insert_with(|| reads(&levels, b));
                }
            }
            let all: HashSet<&String> = scope_reads.values().flatten().collect();
            let mut key: Vec<(String, Vec<Def>)> = Vec::new();
            for p in &all {
                let defs: Vec<Def> = s
                    .chain
                    .iter()
                    .filter_map(|t| {
                        t.tokens
                            .get(p.as_str())
                            .map(|v| Def::Plain(v.clone()))
                            .or_else(|| t.derived.get(p.as_str()).map(|e| Def::Derived(e.clone())))
                    })
                    .collect();
                if !defs.is_empty() {
                    key.push(((*p).clone(), defs));
                }
            }
            key.sort_by(|a, b| a.0.cmp(&b.0));
            if let Some(i) = keys.iter().position(|k| *k == key) {
                // The same overrides as a scope already checked.
                play.owners[i + 1].extend(s.surfaces.iter().copied());
                continue;
            }
            keys.push(key);
            let si = play.chains.len();
            play.chains.push(s.chain.clone());
            play.owners.push(s.surfaces.clone());
            play.failed.push(si > CHECK_SCOPES);
            if si > CHECK_SCOPES {
                continue;
            }
            let lscope = TokenScope::new(&levels);
            let mut from: Vec<&TokenTable> = vec![old];
            from.extend_from_slice(&levels[1..]);
            let from_scope = TokenScope::new(&from);
            let mut memo = [HashMap::new(), HashMap::new()];
            let mut src_of: HashMap<&str, Src> = HashMap::new();
            for (b, r) in &scope_reads {
                let src = if moving(r) {
                    play.slot(si, b)
                } else {
                    match lscope.lookup(b) {
                        Some(PropValue::Color(c)) if c.a >= 1.0 => {
                            Src::Fixed(c.relative_luminance())
                        }
                        _ => Src::Skip,
                    }
                };
                src_of.insert(b, src);
            }
            for (text, bgs) in &table.contrast {
                if !bgs.iter().any(|b| b != text && affected(b))
                    || !readable_in([&from_scope, &lscope], &mut memo, text, bgs)
                {
                    continue;
                }
                let srcs: Vec<Src> = bgs
                    .iter()
                    .filter(|b| *b != text)
                    .map(|b| {
                        src_of
                            .get(b.as_str())
                            .or_else(|| global_src.get(b.as_str()))
                            .cloned()
                            .unwrap_or(Src::Skip)
                    })
                    .collect();
                play.add_pair(si, srcs);
            }
        }
        // The roots the pairs read, directly or through a slot.
        let mut read = vec![false; paths.len()];
        for (_, srcs) in &play.pairs {
            for s in srcs {
                if let Src::Root(i) = s {
                    read[*i] = true;
                }
            }
        }
        for (si, b) in &play.slots {
            let mut levels: Vec<&TokenTable> = vec![table];
            levels.extend(play.chains[*si].iter().copied());
            let r = reads(&levels, b);
            for (i, p) in paths.iter().enumerate() {
                read[i] |= r.contains(*p);
            }
        }
        play.sims = roots
            .values()
            .enumerate()
            .filter(|(i, _)| read[*i])
            .map(|(i, r)| (i, r.motion.clone()))
            .collect();
        play.slot_lum = vec![f64::NAN; play.slots.len()];
        play.slot_at = vec![0; play.slots.len()];
        play.near_last = vec![false; play.pairs.len()];
        play.near = vec![false; play.pairs.len()];
        play.judged = vec![false; play.chains.len()];
        for (si, _) in &play.pairs {
            play.judged[*si] = true;
        }
        let failed = play.failed.iter().any(|f| *f);
        (!play.pairs.is_empty() || failed).then_some(play)
    }

    fn slot(&mut self, scope: usize, path: &str) -> Src {
        let i = match self
            .slots
            .iter()
            .position(|(s, p)| *s == scope && p == path)
        {
            Some(i) => i,
            None => {
                self.slots.push((scope, path.to_string()));
                self.slots.len() - 1
            }
        };
        Src::Slot(i)
    }

    /// Adds a pair whose backgrounds move (one that cannot change needs
    /// no play-through), once.
    fn add_pair(&mut self, scope: usize, srcs: Vec<Src>) {
        if !srcs
            .iter()
            .any(|s| matches!(s, Src::Root(_) | Src::Slot(_)))
        {
            return;
        }
        let pair = (scope, srcs);
        if !self.pairs.contains(&pair) {
            self.pairs.push(pair);
        }
    }

    /// Judges the moment `at`, marking the scopes it leaves unreadable
    /// in `failed` (the global scope at 0): every pair, or (`fine`) only
    /// those only just readable at either end of the step. Returns
    /// whether some pair judged is only just readable; `None` once the
    /// work budget is spent.
    fn moment(&mut self, at: Duration, fine: bool) -> Option<bool> {
        self.moments += 1;
        let cost = self.sims.len() as u32;
        self.work = self.work.checked_sub(cost)?;
        let slots = !self.slots.is_empty();
        for (i, m) in &self.sims {
            let c = channels_color(m.peek(at)).gamut_mapped();
            self.lum[*i] = if c.a >= 1.0 {
                c.relative_luminance()
            } else {
                f64::NAN
            };
            if slots && let Some(slot) = self.scratch.tokens.get_mut(self.paths[*i]) {
                *slot = PropValue::Color(c);
            }
        }
        let mut any_near = false;
        let mut lums: Vec<f64> = Vec::new();
        for p in 0..self.pairs.len() {
            let si = self.pairs[p].0;
            if self.failed[si]
                || (si == 0) != self.global
                || (fine && !(self.near_last[p] || self.near[p]))
            {
                continue;
            }
            self.work = self.work.checked_sub(1)?;
            lums.clear();
            for k in 0..self.pairs[p].1.len() {
                let l = match self.pairs[p].1[k] {
                    Src::Root(i) => self.lum[i],
                    Src::Fixed(l) => l,
                    Src::Slot(k) => self.slot_lum(k)?,
                    Src::Skip => continue,
                };
                if !l.is_nan() {
                    lums.push(l);
                }
            }
            let near = if !luminance_reachable(&lums, MIN_CONTRAST) {
                self.failed[si] = true;
                false
            } else {
                !luminance_reachable(&lums, CHECK_NEAR)
            };
            any_near |= near;
            if !fine {
                self.near_last[p] = std::mem::replace(&mut self.near[p], near);
            }
        }
        Some(any_near)
    }

    /// Slot `k`'s luminance at this moment, evaluated once per moment.
    fn slot_lum(&mut self, k: usize) -> Option<f64> {
        if self.slot_at[k] == self.moments {
            return Some(self.slot_lum[k]);
        }
        self.work = self.work.checked_sub(2)?;
        let (si, path) = &self.slots[k];
        let mut levels: Vec<&TokenTable> = vec![&self.scratch];
        levels.extend(self.chains[*si].iter().copied());
        self.slot_lum[k] = match TokenScope::new(&levels).lookup(path) {
            Some(PropValue::Color(c)) if c.a >= 1.0 => c.relative_luminance(),
            _ => f64::NAN,
        };
        self.slot_at[k] = self.moments;
        Some(self.slot_lum[k])
    }

    /// Whether this pass has nothing left to find: the global scope
    /// failed, or every `set { }` scope judged has.
    fn decided(&self) -> bool {
        if self.global {
            self.failed[0]
        } else {
            (1..self.failed.len()).all(|s| self.failed[s] || !self.judged[s])
        }
    }

    /// Plays the global scope's pairs (`global`) or the `set { }`
    /// scopes' through from `base` until the roots settle or the pass is
    /// decided; false if the work budget or [`CHECK_SPAN`] ran out first.
    fn pass(&mut self, base: Duration, global: bool) -> bool {
        self.global = global;
        self.near_last.fill(false);
        self.near.fill(false);
        self.last.clear();
        let end = base + CHECK_SPAN;
        let mut at = base;
        let mut step = CHECK_STEP;
        let mut first = true;
        while at < end {
            at += step;
            let Some(near) = self.moment(at, false) else {
                return false;
            };
            if self.decided() {
                return true;
            }
            // Either end of the step only just made it: the moments
            // between are looked at too, for the pairs concerned.
            let was_near = self.near_last.iter().any(|n| *n);
            if !first && (near || was_near) {
                for j in 1..CHECK_FINE {
                    if self.moment(at - step * j / CHECK_FINE, true).is_none() {
                        return false;
                    }
                    if self.decided() {
                        return true;
                    }
                }
            }
            first = false;
            if self.settled(at) {
                return true;
            }
            // The next step: no root moves more than `CHECK_MOVE` over it
            // (and it is at most twice as long as this one).
            let speed = self.speed(at, step);
            let bound = if speed > 0.0 {
                Duration::from_secs_f32((CHECK_MOVE / speed).min(1.0))
            } else {
                CHECK_STEP_MAX
            };
            step = bound.clamp(CHECK_STEP, (step * 2).min(CHECK_STEP_MAX));
        }
        // Still moving after the longest span checked.
        false
    }

    /// Whether every root read has settled at `at`.
    fn settled(&self, at: Duration) -> bool {
        self.sims.iter().all(|(_, m)| m.is_settled(at))
    }

    /// How fast the roots move at `at`, in OKLab channels per second:
    /// the faster of their velocity there and their mean speed over the
    /// last `step`.
    fn speed(&mut self, at: Duration, step: Duration) -> f32 {
        let mut fastest = 0.0f32;
        let first = self.last.is_empty();
        if first {
            self.last = vec![[0.0; 4]; self.sims.len()];
        }
        for ((_, m), last) in self.sims.iter().zip(self.last.iter_mut()) {
            let now = m.peek(at);
            let v = m.velocity(at);
            for c in 0..4 {
                let moved = if first {
                    0.0
                } else {
                    (now[c] - last[c]).abs() / step.as_secs_f32()
                };
                fastest = fastest.max(moved).max(v[c].abs());
            }
            *last = now;
        }
        fastest
    }

    /// The surfaces that cannot spring: every one (`all`, or the global
    /// scope failed), or those drawing a failed scope.
    fn verdict(&self, all: bool) -> Verdict {
        if all || self.failed[0] {
            return Verdict {
                all: true,
                surfaces: HashSet::new(),
            };
        }
        Verdict {
            all: false,
            surfaces: self
                .failed
                .iter()
                .zip(&self.owners)
                .filter(|(f, _)| **f)
                .flat_map(|(_, o)| o.iter().copied())
                .collect(),
        }
    }
}

/// Plays `roots` through from `base` (the last frame shown) and says
/// where some moment leaves a declared pair of `table` with no text
/// lightness at 3:1: in the global scope (every surface; judged only
/// with `global`), or under one of the `set { }` scopes shown (the
/// surfaces drawing it). Only pairs with one in `old` and in `table`
/// there count (a palette that is unreadable at rest is not the swap's
/// doing). The global scope is played through first: past
/// [`CHECK_SPAN`] or [`CHECK_WORK`] there, every surface; past them
/// under the `set { }` scopes, the surfaces drawing every scope judged.
fn check_swap<'a>(
    old: &TokenTable,
    table: &'a TokenTable,
    scopes: &[ShownScope<'a>],
    roots: &'a BTreeMap<String, Root>,
    base: Duration,
    global: bool,
) -> Verdict {
    if roots.is_empty() {
        return Verdict::default();
    }
    let Some(mut play) = Play::new(old, table, scopes, roots) else {
        return Verdict::default();
    };
    // The first sample starts every retarget (as the first frame will);
    // after it the motions are pure functions of time.
    for (_, m) in &mut play.sims {
        m.sample(base + CHECK_STEP);
    }
    if global && play.judged[0] && !play.pass(base, true) {
        // Not sprung unchecked.
        return play.verdict(true);
    }
    if play.failed[0] {
        return play.verdict(false);
    }
    let open = (1..play.failed.len()).any(|s| play.judged[s] && !play.failed[s]);
    if open && !play.pass(base, false) {
        // Out of work or still moving: the scopes not cleared do not
        // spring unchecked, on their own surfaces.
        for s in 1..play.failed.len() {
            play.failed[s] |= play.judged[s];
        }
    }
    play.verdict(false)
}

/// `new` (the new frame, in `target`) over the snapshot pixels `old` of
/// `size`, the new frame weighted `w` (premultiplied channels, so
/// translucent surfaces fade too).
fn blend(target: &mut PaintTarget<'_>, old: &[u8], size: Size, w: f32) {
    let a = (w.clamp(0.0, 1.0) * 256.0).round() as u32;
    if a >= 256 || old.len() != size.w as usize * size.h as usize * 4 {
        return;
    }
    let row = size.w as usize * 4;
    let stride = target.stride as usize;
    for y in 0..size.h as usize {
        let dst = &mut target.pixels[y * stride..y * stride + row];
        let src = &old[y * row..(y + 1) * row];
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
    /// diff applies: which roots spring from where, or, where no spring
    /// keeps the declared pairs readable, the snapshots to crossfade
    /// from. `None` if the diff sends no table.
    pub(super) fn plan_swap(&mut self, diff: &SceneDiff) -> Option<Plan> {
        let (table, transition) = diff.ops.iter().rev().find_map(|op| match op {
            SceneOp::SetTokens { table, transition } => Some((table, transition)),
            _ => None,
        })?;
        let started = Instant::now();
        self.prune_fades(None);
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
            snap: false,
            all: false,
            hold: HashSet::new(),
            snaps: HashMap::new(),
            curve,
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
        if plan.roots.is_empty() {
            self.swap.work += started.elapsed();
            return Some(plan);
        }
        let (scopes, crowded) = self.shown_scopes();
        let verdict = check_swap(&self.tree.tokens, table, &scopes, &plan.roots, base, true);
        drop(scopes);
        let fading: HashSet<SurfaceId> = if verdict.all {
            plan.all = true;
            plan.roots.clear();
            self.shown_with_clock().map(|(id, _)| id).collect()
        } else {
            // Surfaces already shown a held table keep one (now the
            // newest), fading to it again.
            plan.hold = self
                .swap
                .held_for
                .iter()
                .copied()
                .chain(verdict.surfaces)
                .chain(crowded)
                .collect();
            plan.hold.clone()
        };
        if !fading.is_empty() {
            plan.snaps = self.snapshots(&fading, curve);
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
    /// nodes the shown surfaces draw, each with the surfaces drawing it,
    /// and the surfaces under a root with more than [`RAW_SCOPES`].
    fn shown_scopes(&self) -> (Vec<ShownScope<'_>>, Vec<SurfaceId>) {
        let mut by_root: BTreeMap<NodeId, Vec<SurfaceId>> = BTreeMap::new();
        for (id, s) in self.shown_with_clock() {
            by_root.entry(s.root).or_default().push(id);
        }
        let mut out: Vec<ShownScope<'_>> = Vec::new();
        let mut crowded = Vec::new();
        for (root, surfaces) in by_root {
            let mut chains: Vec<Vec<&TokenTable>> = Vec::new();
            let mut stack = vec![root];
            let mut seen: HashSet<NodeId> = HashSet::new();
            while let Some(id) = stack.pop() {
                if !seen.insert(id) {
                    continue;
                }
                let Some(n) = self.tree.get(id) else {
                    continue;
                };
                if n.get(Prop::Tokens).is_some() {
                    let chain: Vec<&TokenTable> = crate::flatten::scope_tables(&self.tree, id)
                        .into_iter()
                        .skip(1)
                        .collect();
                    if !chains.contains(&chain) {
                        chains.push(chain);
                    }
                    if chains.len() > RAW_SCOPES {
                        break;
                    }
                }
                stack.extend(n.children.iter().copied());
            }
            if chains.len() > RAW_SCOPES {
                crowded.extend(surfaces);
                continue;
            }
            for chain in chains {
                match out.iter_mut().find(|s| s.chain == chain) {
                    Some(s) => s.surfaces.extend(surfaces.iter().copied()),
                    None => out.push(ShownScope {
                        chain,
                        surfaces: surfaces.clone(),
                    }),
                }
            }
        }
        (out, crowded)
    }

    /// Snapshots of the frames `ids` show now (those shown with a
    /// clock, smallest first, within [`SNAPSHOT_MAX`] and
    /// [`SNAPSHOTS_MAX`]: the others snap to the new frame), each fading
    /// along `curve` from its first frame. A surface already crossfading
    /// takes the blend it shows as its snapshot and fades on from there.
    fn snapshots(
        &mut self,
        ids: &HashSet<SurfaceId>,
        curve: Curve,
    ) -> HashMap<SurfaceId, Snapshot> {
        let mut order: Vec<(usize, SurfaceId)> = self
            .shown_with_clock()
            .filter(|(id, _)| ids.contains(id))
            .map(|(id, s)| (s.size.w as usize * s.size.h as usize * 4, id))
            .collect();
        order.sort();
        // What the fades left alone keep.
        let mut kept: usize = self
            .swap
            .fade
            .iter()
            .filter(|(id, _)| !ids.contains(id))
            .map(|(_, s)| s.bytes())
            .sum();
        let mut out = HashMap::new();
        let now = Instant::now();
        for (bytes, id) in order {
            let prev = self.swap.fade.remove(&id);
            if bytes > SNAPSHOT_MAX || kept + bytes > SNAPSHOTS_MAX {
                continue;
            }
            kept += bytes;
            let Some(s) = self.surfaces.get_mut(&id) else {
                continue;
            };
            let (size, scale, time, painted) = (s.size, s.scale, s.time, s.painted_time);
            let mut progress = Motion::rest([0.0], FADE_EPS).sampled_at(painted);
            progress.retarget([1.0], curve);
            let (pixels, under) = match prev {
                // Not taken yet: the surface still shows the frame it is
                // for.
                Some(p) if p.old.is_some() => {
                    out.insert(
                        id,
                        Snapshot {
                            progress,
                            since: now,
                            ..p
                        },
                    );
                    continue;
                }
                Some(p) if p.size == size && p.scale == scale => (p.pixels, Some(p.last_w)),
                _ => (Vec::new(), None),
            };
            // The old frame's scene, for what its buffer no longer holds.
            let f = match s.cache.take() {
                Some(f) => f,
                None => {
                    // Flattened at rest as of the last frame (a preview:
                    // nothing starts).
                    self.anim.begin(time, painted, false);
                    self.flatten_now(id)
                }
            };
            out.insert(
                id,
                Snapshot {
                    size,
                    scale,
                    pixels,
                    old: Some(f),
                    under,
                    progress,
                    last_w: 0.0,
                    since: now,
                },
            );
        }
        out
    }

    /// Takes `surface`'s snapshot if its fade has not yet, before the
    /// frame in `target` is painted (see [`Snapshot`]).
    pub(super) fn take_snapshot(&mut self, surface: SurfaceId, target: &PaintTarget<'_>) {
        let Some(snap) = self.swap.fade.get_mut(&surface).filter(|s| s.old.is_some()) else {
            return;
        };
        let Some(s) = self.surfaces.get(&surface) else {
            return;
        };
        let started = Instant::now();
        let age = target.age as usize;
        let row = snap.size.w as usize * 4;
        let mut pixels = vec![0u8; snap.bytes()];
        // The buffer holds the frame shown but for the last `age - 1`
        // frames' damage. (Under a replaced crossfade every frame was
        // painted in full: only age 1 holds it.)
        let same = s.valid
            && age >= 1
            && age <= s.history.len() + 1
            && (age == 1 || snap.under.is_none())
            && target.size == snap.size
            && target.scale == snap.scale;
        let missed = same.then(|| {
            let stride = target.stride as usize;
            for (y, dst) in pixels.chunks_exact_mut(row).enumerate() {
                dst.copy_from_slice(&target.pixels[y * stride..y * stride + row]);
            }
            let mut missed = Damage::new();
            for d in s.history.iter().take(age - 1) {
                missed.union(d);
            }
            missed.clip(target.bounds());
            missed
        });
        let under = snap.under.take();
        let prev = std::mem::take(&mut snap.pixels);
        let draw = match &missed {
            Some(m) => (!m.is_empty()).then_some(*m),
            None => Some(Damage::full(snap.size)),
        };
        if let (Some(region), Some(old)) = (draw, &snap.old)
            && let Ok(mut t) =
                PaintTarget::new(&mut pixels, snap.size, snap.size.w * 4, snap.scale, 0)
        {
            self.raster
                .paint(&old.items, &region, &self.atlas, snap.scale, &mut t);
            // A crossfade replaced mid-crossfade: what showed was the
            // old frame over the fade's snapshot.
            if missed.is_none()
                && let Some(w) = under
            {
                blend(&mut t, &prev, snap.size, w);
            }
        }
        snap.pixels = pixels;
        snap.old = None;
        self.swap.work += started.elapsed();
    }

    /// Puts a plan in place once its table is the tree's: the roots
    /// spring from the next frame on (every frame writes their values),
    /// and the surfaces that cannot spring crossfade (all of them, the
    /// table snapped, or those shown the held new table).
    pub(super) fn install_swap(&mut self, plan: Plan) {
        let started = Instant::now();
        self.swap.curve = Some(plan.curve);
        if plan.snap {
            // Snapped: crossfades in flight end too.
            self.swap.fade.clear();
            self.swap.roots.clear();
            self.swap.held = None;
            self.swap.held_for.clear();
        } else if plan.all {
            self.swap.roots.clear();
            self.swap.held = None;
            self.swap.held_for.clear();
            self.swap.crossfades += 1;
            self.swap.fade.extend(plan.snaps);
        } else {
            self.swap.roots = plan.roots;
            if !plan.hold.is_empty() && !self.swap.roots.is_empty() {
                let mut held = self.tree.tokens.clone();
                held.freeze();
                self.swap.held = Some(held);
                if !plan.snaps.is_empty() {
                    self.swap.crossfades += 1;
                }
                self.swap.held_for = plan.hold;
                for id in &self.swap.held_for {
                    if let Some(s) = self.surfaces.get_mut(id) {
                        s.cache = None;
                    }
                }
                self.swap.fade.extend(plan.snaps);
            } else {
                self.swap.held = None;
                self.swap.held_for.clear();
            }
        }
        self.swap.work += started.elapsed();
    }

    /// While roots spring, the `set { }` scopes under `nodes` (a node
    /// given `tokens`, a subtree moved, a surface's root attached: scopes
    /// the swap's plan never saw) are played through from the roots'
    /// motions as they are. The surfaces drawing one no spring keeps
    /// readable (`only`, if given, else any surface drawing `nodes`) are
    /// shown the new table at once, crossfading from a snapshot if they
    /// show a frame. Called before the diff's surfaces are marked dirty
    /// (their caches still hold the frames shown).
    pub(super) fn check_new_scopes(&mut self, nodes: &[NodeId], only: Option<SurfaceId>) {
        if self.swap.roots.is_empty() || nodes.is_empty() {
            return;
        }
        let started = Instant::now();
        // The scopes under `nodes`, with the surfaces drawing them.
        let mut found: Vec<(NodeId, Vec<SurfaceId>)> = Vec::new();
        let mut crowded: HashSet<SurfaceId> = HashSet::new();
        let mut seen: HashSet<NodeId> = HashSet::new();
        for &n in nodes {
            let drawing: Vec<SurfaceId> = self
                .surfaces
                .iter()
                .filter(|(id, s)| {
                    only.is_none_or(|o| o == **id)
                        && !self.swap.held_for.contains(id)
                        && self.tree.is_ancestor(s.root, n)
                })
                .map(|(id, _)| *id)
                .collect();
            if drawing.is_empty() {
                continue;
            }
            let mut stack = vec![n];
            let mut scoped = 0usize;
            while let Some(id) = stack.pop() {
                if !seen.insert(id) {
                    continue;
                }
                let Some(node) = self.tree.get(id) else {
                    continue;
                };
                if node.get(Prop::Tokens).is_some() {
                    scoped += 1;
                    if scoped > RAW_SCOPES {
                        crowded.extend(drawing.iter().copied());
                        break;
                    }
                    found.push((id, drawing.clone()));
                }
                stack.extend(node.children.iter().copied());
            }
        }
        if found.is_empty() && crowded.is_empty() {
            self.swap.work += started.elapsed();
            return;
        }
        // The new table: the tree's, with the roots' targets.
        let mut table = self.tree.tokens.clone();
        for (path, r) in &self.swap.roots {
            if let Some(slot) = table.tokens.get_mut(path) {
                *slot = PropValue::Color(r.target);
            }
        }
        table.freeze();
        let base = self.shown_at().unwrap_or_default();
        let mut hold = crowded;
        {
            let mut scopes: Vec<ShownScope<'_>> = Vec::new();
            for (id, drawing) in &found {
                let chain: Vec<&TokenTable> = crate::flatten::scope_tables(&self.tree, *id)
                    .into_iter()
                    .skip(1)
                    .collect();
                match scopes.iter_mut().find(|s| s.chain == chain) {
                    Some(s) => s.surfaces.extend(drawing.iter().copied()),
                    None => scopes.push(ShownScope {
                        chain,
                        surfaces: drawing.clone(),
                    }),
                }
            }
            let verdict = check_swap(&table, &table, &scopes, &self.swap.roots, base, false);
            hold.extend(verdict.surfaces);
        }
        hold.retain(|id| !self.swap.held_for.contains(id));
        if !hold.is_empty() {
            if self.swap.held.is_none() {
                self.swap.held = Some(table);
            }
            let curve = self.swap.curve.unwrap_or(Curve::Instant);
            let snaps = self.snapshots(&hold, curve);
            if !snaps.is_empty() {
                self.swap.crossfades += 1;
            }
            self.swap.fade.extend(snaps);
            for id in &hold {
                if let Some(s) = self.surfaces.get_mut(id) {
                    s.cache = None;
                }
            }
            self.swap.held_for.extend(hold);
        }
        self.swap.work += started.elapsed();
    }

    /// Swaps the held new table into the tree while `id` is flattened,
    /// if `id` is shown it (see [`Swap::held`]); true if it did.
    pub(super) fn hold_tokens(&mut self, id: SurfaceId) -> bool {
        if !self.swap.held_for.contains(&id) {
            return false;
        }
        match &mut self.swap.held {
            Some(h) => {
                std::mem::swap(&mut self.tree.tokens, h);
                true
            }
            None => false,
        }
    }

    /// Puts the springing table back after [`Renderer::hold_tokens`].
    pub(super) fn release_tokens(&mut self, held: bool) {
        if held && let Some(h) = &mut self.swap.held {
            std::mem::swap(&mut self.tree.tokens, h);
        }
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
        if self.swap.roots.is_empty() {
            // Landed: the tree's table is the held one now.
            self.swap.held = None;
            self.swap.held_for.clear();
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
            || self.swap.fade.contains_key(&surface)
    }

    /// Drops the snapshots of surfaces (but `keep`) that painted nothing
    /// for the exit stall: an output asleep or occluded never ends its
    /// fade, and its snapshot is not held for ever.
    fn prune_fades(&mut self, keep: Option<SurfaceId>) {
        let (now, stall) = (Instant::now(), self.exit_stall);
        self.swap
            .fade
            .retain(|id, s| Some(*id) == keep || now.saturating_duration_since(s.since) < stall);
    }

    /// How the frame of `surface` at `at`, `size` shows the crossfade.
    pub(super) fn fade_frame(&mut self, surface: SurfaceId, at: Duration, size: Size) -> FadeFrame {
        if self.anim.reduced() {
            self.swap.fade.clear();
        }
        self.prune_fades(Some(surface));
        if let Some(snap) = self.swap.fade.get_mut(&surface) {
            // The surface's first frame starts its fade; after that the
            // progress is a function of its own clock.
            if snap.progress.is_pending() {
                snap.progress.sample(at);
            }
            let p = snap.progress.peek(at)[0];
            if !snap.progress.is_settled(at) && snap.size == size {
                snap.last_w = p.clamp(0.0, 1.0);
                snap.since = Instant::now();
                self.swap.blended.insert(surface);
                return FadeFrame::Blend(snap.last_w);
            }
            // Done here, or resized since: it shows the new frames as
            // they are.
            self.swap.fade.remove(&surface);
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
            .get(&surface)
            .filter(|s| s.size == target.size)
        {
            blend(target, &snap.pixels, snap.size, w);
        }
        self.swap.blend += started.elapsed();
    }

    /// Forgets a detached surface's crossfade.
    pub(super) fn forget_fade(&mut self, surface: SurfaceId) {
        self.swap.blended.remove(&surface);
        self.swap.fade.remove(&surface);
        self.swap.held_for.remove(&surface);
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

    /// Theme swaps that crossfaded (on every surface, or on those whose
    /// `set { }` scopes no spring keeps readable) instead of springing.
    pub fn swap_crossfades(&self) -> u64 {
        self.swap.crossfades
    }

    /// True while palette roots spring or a crossfade runs on a surface
    /// still painting (one that painted nothing for the exit stall does
    /// not count).
    pub fn swapping(&self) -> bool {
        let (now, stall) = (Instant::now(), self.exit_stall);
        !self.swap.roots.is_empty()
            || self
                .swap
                .fade
                .values()
                .any(|s| now.saturating_duration_since(s.since) < stall)
    }

    /// Surfaces shown the held new table while the roots spring
    /// elsewhere (tests).
    #[doc(hidden)]
    pub fn swap_held(&self) -> Vec<SurfaceId> {
        let mut v: Vec<SurfaceId> = self.swap.held_for.iter().copied().collect();
        v.sort();
        v
    }
}
