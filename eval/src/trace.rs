//! The run trace (`trace.json`, schema 1; docs/eval.md "Trace"): what a
//! scenario's run drew, frame by frame and node by node, keyed so runs
//! and builds compare. Geometry is in logical pixels, quantised to 1/64;
//! `op` is effective opacity (the node's times its ancestors'); `auth` is
//! effective authored axes (its own or an ancestor's). The digest hashes
//! every deterministic field and none of the wall times.

use std::collections::BTreeMap;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The trace format's version.
pub const SCHEMA: u32 = 1;

/// The columns of a row before `fx`, `fy` (still read).
const OLD_COLS: usize = 34;
/// The columns of a row before `fit`, `ink` (still read).
const FLIGHT_COLS: usize = 36;
/// The columns of a row before `paint` (still read).
const INK_COLS: usize = 38;
/// The columns of a row before `ov` (still read).
const PAINT_COLS: usize = 39;
/// The columns of a row before `clip` (still read).
const OV_COLS: usize = 40;

/// The node row's columns, in order (`Row`'s array form).
pub const COLS: [&str; 41] = [
    "key", "s", "x", "y", "w", "h", "vx", "vy", "vw", "vh", "lx", "ly", "lw", "lh", "op", "oo",
    "sx", "sy", "rot", "mot", "emot", "auth", "hid", "txt", "ix", "iy", "wx", "wy", "spx", "spy",
    "gx", "gy", "mx", "my", "fx", "fy", "fit", "ink", "paint", "ov", "clip",
];

/// A rect, `[x, y, w, h]`.
pub type Rect = [f32; 4];

/// The whole run.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RunTrace {
    pub schema: u32,
    pub scenario: ScenarioInfo,
    pub build: BuildInfo,
    pub surfaces: Vec<SurfaceInfo>,
    /// Every node seen, by key.
    pub nodes: BTreeMap<String, NodeMeta>,
    /// The steps as they ran.
    pub events: Vec<EventRec>,
    pub segments: Vec<Segment>,
    /// Each `rest` and whether it reached rest.
    pub rests: Vec<RestRec>,
    pub expects: Vec<ExpectRec>,
    /// Actions and commands the shell ran (recorded, never executed).
    pub commands: Vec<CommandRec>,
    pub cols: Vec<String>,
    /// The ticks that painted something.
    pub frames: Vec<Frame>,
    pub digest: String,
    /// How many painted frames (of a surface each) were compared with a
    /// full repaint of the same frame (the `redraw` check;
    /// `Frame::redraw` holds the differences); 0: none. Never in the
    /// digest: the comparison changes nothing the run draws.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub redraw: u32,
    /// Rows and input regions moved by their surface's origin since it
    /// was first placed ([`RunTrace::on_screen`]); never written.
    #[serde(skip)]
    pub on_screen: bool,
    /// (eval-surface-leg) What the surface leg recorded (docs/eval.md,
    /// "Surface leg"), when it ran. Never in the digest or a recording:
    /// it is how the run's frames reached a compositor, not what they
    /// drew, and the checks that read it compare it with the frames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire: Option<crate::wire::WireLog>,
    /// (integration-r7) The ticks at which a `list`'s window mounted each
    /// row, by key: logic's diff said so (`SceneOp::Create { window:
    /// true }`), and render plays no `enter` for it (decisions.md
    /// m4-lists-w1). Read by `stagger` to tell a window's row from a
    /// child that mounts anew, from what logic declared rather than the
    /// renderer's motion bits. Never in the digest.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub windowed: BTreeMap<String, Vec<u32>>,
    /// (integration-r7) The ticks at which logic's diff created each
    /// surface's root (a `bar`, `panel`, `osd`, `popup` or `lock` node:
    /// `SceneOp::Create`), by key: when logic opened it, read by
    /// `vanish`'s pose back to tell a surface opened again while it
    /// closed from the renderer's motion bits alone. Never in the digest.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub opened: BTreeMap<String, Vec<u32>>,
    /// (integration-r7 part 2) The ticks at which logic's diff closed
    /// each surface, by its root's key: removed its root or a node above
    /// it (`SceneOp::Remove`), or set its `open` false. Read by `vanish`'s
    /// pose back to tell when a surface starts closing from what the
    /// scenario did, never from the renderer's motion bits, so a renderer
    /// that drops a closing surface's exit is still judged. Never in the
    /// digest.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub closed: BTreeMap<String, Vec<u32>>,
    /// (eval-rules-8) Each `keyframes` block a node played, by name, as
    /// logic declared it (`strand_scene::Keyframes`, compiled from the
    /// config): what `continuity`'s `track` works the track out from.
    /// Never in the digest.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub keyframes: BTreeMap<String, KeyframesRec>,
}

/// (eval-rules-8) A `keyframes` block as declared: its stops (fractions
/// of its run, in order) with the values they give the props the checks
/// read as numbers (`x`, `y`, `rotate` in px or degrees, `opacity`,
/// `scale`), its run's length and delay (ns), how many runs (`None`:
/// forever), whether every other run goes backwards, and its easing.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct KeyframesRec {
    pub stops: Vec<(f32, BTreeMap<String, f32>)>,
    /// Props its stops set that are not read as numbers here (a colour, a
    /// percentage): they are not judged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub other: Vec<String>,
    pub d: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub delay: u64,
    pub repeat: Option<u32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub alternate: bool,
    pub ease: EaseRec,
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

/// (eval-rules-8) What drives a node by time at one frame, as logic
/// declared it: its `play` (the block's name and logic's `seq`, bumped
/// each time it plays again) and whether a prop of it reads time (`t`,
/// `wave(…)`, `noise(…)`, or a token that does).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TimedRec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub play: Option<(String, u32)>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub t: bool,
}

/// (eval-rules-8) One drop shadow drawn (a box's glow included), from
/// strand-render's `eval-hooks` (`probe::ShadowDrawn`): the node, its
/// surface, the box it is cast by and the area it may cover (surface
/// logical px, before transforms), what its props asked (`x`, `y`,
/// `blur`, `spread`, logical px) and its colour's alpha.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ShadowRec {
    pub key: String,
    pub s: u32,
    pub frame: Rect,
    pub extent: Rect,
    pub asked: [f32; 4],
    pub a: f32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ScenarioInfo {
    pub id: String,
    pub file: String,
    pub hz: u32,
    pub scale: f64,
    pub backend: String,
    pub text: String,
    /// The whole run had motion reduced (the twin, or the scenario's own
    /// `reduce_motion`).
    pub reduced_motion: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BuildInfo {
    pub rev: String,
    pub dirty: bool,
    pub profile: String,
    pub features: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SurfaceInfo {
    pub id: u32,
    /// `bar Top`, `popup #card`.
    pub label: String,
    pub kind: String,
    /// The node key of its root.
    pub root: String,
    pub attached: u32,
    pub detached: Option<u32>,
    /// (eval-rules) The axes it is sized by its content on (a popup or
    /// panel with no width, or no height): `[w, h]`.
    #[serde(default, skip_serializing_if = "is_fixed")]
    pub content: [bool; 2],
    /// (eval-rules-4) Its placement as declared (`SurfaceSpec`'s props,
    /// never where the surface manager put it), from tick `k` on: one
    /// entry per change, for `checks::placement`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub places: Vec<PlaceRec>,
}

/// (eval-rules-4) A surface's declared placement from tick `k`: its
/// layer, a bar's edge, `anchor:`, `margin` (top, right, bottom, left)
/// and whether it is set, `attach:`, whether a popup is a tooltip, a
/// popup's parent surface and the key of the element it opens from, and
/// its declared width and height.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PlaceRec {
    pub k: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge: Option<String>,
    pub anchor: String,
    pub margin: [f32; 4],
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub margin_set: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tooltip: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default)]
    pub size: [Option<f32>; 2],
}

fn is_fixed(c: &[bool; 2]) -> bool {
    !c[0] && !c[1]
}

/// What a node is (it does not change while it lives).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeMeta {
    /// `box#bars`, `text("iiii")`, `row@shell.strand:9`.
    pub label: String,
    pub kind: String,
    /// `file:line:col`.
    pub origin: Option<String>,
    /// The parent's key.
    pub parent: Option<String>,
    pub id: Option<String>,
    pub text: Option<String>,
    /// Its `enter` pose when it was first seen.
    pub enter: Option<Pose>,
    /// Its shared-element `morph:` name, when written as text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub morph: Option<String>,
    /// A text that cuts itself with an ellipsis (`ellipsis:`) rather
    /// than wrapping when its box is narrower than it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ellipsis: bool,
    /// (eval-rules) Its `exit` pose when it was first seen (its `enter`
    /// mirrored when it has none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<Pose>,
    /// (eval-rules) A container's `stagger:` step, in ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stagger: Option<f32>,
    /// (eval-rules) It clips what it holds to its box (`clip: true`, a
    /// `scroll`, a `list`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clips: bool,
    /// (eval-rules) Its width does not give way in a full row
    /// (`labels::rigid_of`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rigid: bool,
    /// (eval-rules-4) For a `split`, what of its box is not its content
    /// (its padding; top, right, bottom, left); for a split's part, its
    /// margin. `None` when not known (a token, a border) or another kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inset: Option<[f32; 4]>,
    /// (eval-rules-6) How it sits in its parent's flow, as written when
    /// it was first seen ([`Flow`] bits).
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub flow: u8,
    /// (integration-r7) How much of a change in its own width moves its
    /// left edge the other way, as its parent's declared props place it
    /// on x (spec.md 6.7): 0 at the start, 0.5 centred, 1 at the end.
    /// `None` where they do not fix it (a row that spreads its children,
    /// a stretched or filling child, a split's part, another kind).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x_share: Option<f32>,
}

/// (eval-rules-6) [`NodeMeta::flow`]'s bits.
#[derive(Debug)]
pub struct Flow;

impl Flow {
    /// `place: absolute`: out of its parent's flow.
    pub const ABSOLUTE: u8 = 1;
    /// A `margin:` of its own (in any form).
    pub const MARGIN: u8 = 1 << 1;
    /// A `width`, `size` or `min_width` of its own (in any form), or an
    /// image or icon: its width is not its content's.
    pub const SIZED: u8 = 1 << 2;
    /// A `shrink:` that is not a number over 0: it may not give way.
    pub const NO_SHRINK: u8 = 1 << 3;
    /// A container whose `justify:` spreads its children (`space_*`).
    pub const SPREAD: u8 = 1 << 4;
}

fn is_zero_u8(v: &u8) -> bool {
    *v == 0
}

/// The parts of an `enter` pose the checks read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Pose {
    pub opacity: Option<f32>,
    pub x: Option<f32>,
    pub y: Option<f32>,
    pub scale: Option<f32>,
    /// (eval-rules) A width or height it lays out at (`exit { height: 0 }`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub w: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub h: Option<f32>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EventRec {
    /// The tick it acted at.
    pub k: u32,
    pub line: u32,
    pub text: String,
    pub segment: Option<usize>,
}

/// From a step that changes something to the next rest
/// (docs/eval.md, "Checks").
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub name: String,
    /// The line that opened it.
    pub line: u32,
    /// The tick before its first change (none for the boot segment).
    pub s: Option<u32>,
    /// Its last tick: the rest's, or the run's last.
    pub e: u32,
    /// It ended at a rest that settled.
    pub settled: bool,
    /// Motion was reduced at some tick of it.
    pub reduced_motion: bool,
    /// The rate at its end.
    pub hz: u32,
    pub intends: Vec<IntendRec>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct IntendRec {
    pub line: u32,
    pub what: String,
    pub keys: Vec<String>,
    /// `probe::Axes` bits.
    pub axes: u8,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RestRec {
    pub line: u32,
    /// The first tick it ran and how many it ran.
    pub k: u32,
    pub ticks: u32,
    pub max: u32,
    pub settled: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ExpectRec {
    pub k: u32,
    pub line: u32,
    pub text: String,
    /// still | visible | hidden | moves.
    pub what: String,
    pub keys: Vec<String>,
    pub axis: Option<String>,
    pub from: u32,
    pub segment: Option<usize>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CommandRec {
    pub k: u32,
    pub text: String,
}

/// A held text at a frame.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HeldRec {
    pub key: u64,
    pub text: String,
    pub paints: u32,
}

/// A surface's costs over one tick (every paint of it in the tick).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Cost {
    pub paints: u32,
    pub damage_px: u64,
    pub raster_px: u64,
    /// Of its last paint.
    pub items: u32,
    pub records: u32,
    pub layout_passes: u32,
    pub layout_nodes: u32,
    pub text_pending: u32,
    pub text_held: u32,
    /// Wall-clock, advisory: never in the digest, never gating.
    pub flatten_us: u64,
    pub text_wait_us: u64,
    pub raster_us: u64,
}

/// One tick that painted.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub k: u32,
    pub t_ns: u64,
    /// Surfaces painted this tick.
    pub painted: Vec<u32>,
    /// Surfaces still wanting a frame after it.
    pub wants: Vec<u32>,
    /// Surfaces shown, and their logical sizes.
    pub sizes: BTreeMap<u32, [f32; 2]>,
    pub fresh: BTreeMap<u32, bool>,
    pub cost: BTreeMap<u32, Cost>,
    pub held_text: Vec<HeldRec>,
    /// The image each painted surface showed (`frames/<hash>.png`).
    pub png: BTreeMap<u32, String>,
    /// What each painted surface asked of the compositor besides pixels.
    #[serde(default)]
    pub regions: BTreeMap<u32, Regions>,
    /// The nodes of each painted surface, at its last paint of the tick.
    pub n: Vec<Row>,
    /// The screen's scale at this tick (a `scale` step changes it); 0 in
    /// a trace from before it was recorded: the scenario's.
    #[serde(default)]
    pub scale: f64,
    /// Logic's diff with changes in it, or input, reached the renderer
    /// since the frame before: what is laid out anew may be theirs.
    #[serde(default)]
    pub driven: bool,
    /// Where each shown surface's buffer lies on the output (logical
    /// px): a popup the compositor re-centres as it grows moves.
    #[serde(default)]
    pub origins: BTreeMap<u32, [f32; 2]>,
    /// With `compositor_poses`, the pose each painted surface commits
    /// with this frame (`Renderer::delegated_pose`), which the
    /// compositor applies to it: absent is none (at rest).
    #[serde(default)]
    pub poses: BTreeMap<u32, PoseRec>,
    /// (eval-rules-6) How far from where it lies at rest (`origins`) the
    /// compositor put each surface for this frame, logical px: by the
    /// margins the surface manager sent for its pose's offset (layer
    /// shell's `set_margin`, `placement::posed_margin`). Absent: at rest.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub shifts: BTreeMap<u32, [f32; 2]>,
    /// With `RunTrace::redraw`, each painted surface whose buffer differs
    /// at all from a full repaint of the same frame. Never in the digest.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub redraw: BTreeMap<u32, RedrawRec>,
    /// (eval-coverage) Each painted surface blended over its crossfade
    /// snapshot (a theme swap's), and the weight it was blended at.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fades: BTreeMap<u32, f32>,
    /// (integration-r4) Each `scroll` and `list` on a painted surface, by
    /// key: its offset (logical px from the top, `ScrollState::offset`),
    /// so a check reads how far a scroll carried what it holds.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub offsets: BTreeMap<String, f32>,
    /// (eval-rules-2) Per painted surface that sampled a spring, the
    /// frame period (ns) a change it started began one of before it
    /// (zero: not known yet, and `START_LEAD` stands in). In the digest
    /// with everything below when any of it is there.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub period: BTreeMap<u32, u64>,
    /// (eval-rules-2) Every sample the painted surfaces took of a prop's,
    /// a laid-out size's or a glide's spring, by surface, node and what,
    /// each one's in the order taken.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sp: Vec<SpringRec>,
    /// (eval-rules-2) The `roll: true` texts rolling in this frame.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rolls: Vec<RollRec>,
    /// (eval-rules-2) Per shown surface, the sides of its buffer that lie
    /// on or past its output's edge, or on its `attach:` side, where a
    /// shadow may be cut (`checks::contain`, `Surfaces::cuts`): bits 1
    /// top, 2 right, 4 bottom, 8 left.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub cuts: BTreeMap<u32, u8>,
    /// (eval-rules-2) Where the pointer is after this tick's input: its
    /// surface and its place there (logical px, as the rows').
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ptr: Option<PtrRec>,
    /// (eval-rules-2) The sliders held in a drag, by key: the value the
    /// drag gives each.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub drags: BTreeMap<String, f32>,
    /// (eval-rules-4) Each `image` and `icon` drawn this frame: where
    /// each of its decodes went (`strand_render::probe::ImageDrawn`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imgs: Vec<ImageRec>,
    /// (eval-rules-4) Each text drawn cut to its line box from the
    /// layout shaped for it: its lines and its unbounded layout's stops
    /// (`strand_render::probe::CutDrawn`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<LinesRec>,
    /// (eval-rules-4) Per painted surface that blended a crossfade on
    /// the CPU: its new frame and its snapshot blended again here at the
    /// frame's weight, against the blend it drew (`checks::redraw`'s
    /// `blend` arm). Never in the digest, as `redraw`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub blend: BTreeMap<u32, BlendRec>,
    /// (eval-rules-6) Each `border:` ring drawn this frame: its box, its
    /// props and where the outline it filled meets [`crate::ring::rays`]
    /// (`coherence`'s `ring`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rings: Vec<crate::ring::RingRec>,
    /// (eval-rules-6) Each flex container (`row`, `col`, `scroll`)
    /// drawn this frame with a `gap:` over 0, by key: the
    /// gap its props give it (`continuity`'s `fold`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub gaps: BTreeMap<String, f32>,
    /// (eval-rules-6) On each painted surface that wants no more frames,
    /// the texts drawn whole on one line that could break narrower: shaped,
    /// not cut (no `fit`), with no `ellipsis:` or `max_lines:`, and two
    /// words or more apart by spaces, so their least width (their longest
    /// word, decisions.md wave3-pixels) is less than the width drawn
    /// (`contain`'s `wrap`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loose: Vec<String>,
    /// (eval-rules-8) Motion was reduced at this frame, as render had it
    /// (`system.reduced_motion` carried in logic's diff, or the run's
    /// own). Never in the digest.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reduced: bool,
    /// (eval-rules-8) The nodes drawn this frame that time drives as
    /// declared (a `play`, a prop that reads time), by key. Never in the
    /// digest.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub timed: BTreeMap<String, TimedRec>,
    /// (eval-rules-8) Each drop shadow drawn this frame. Never in the
    /// digest.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shadows: Vec<ShadowRec>,
}

/// (eval-rules-4) One decode of an image node as drawn, surface logical
/// px before transforms: the node's box, where the whole pixmap went,
/// the pixmap's size, the source's size, the fit asked for and the fit
/// the pixmap was decoded by, whether it stands in for a decode of
/// another box size, and the file's own size.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ImageRec {
    pub key: String,
    pub s: u32,
    pub rect: Rect,
    pub dest: Rect,
    pub px: [u32; 2],
    pub src: [f64; 2],
    pub fit: String,
    pub pfit: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stand_in: bool,
    /// (png-aspect) The size the image file's header gives, when its
    /// source is a PNG, JPEG or GIF file ([`crate::filesize`]): read by
    /// the sandbox, not by the renderer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<[u32; 2]>,
}

/// (eval-rules-4) A text drawn cut to its line box `fit` (the row's).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LinesRec {
    pub key: String,
    pub s: u32,
    /// The text (the drawn layout's stops count bytes of it, or of it
    /// cut and ended with an ellipsis).
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ellipsis: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_lines: Option<u32>,
    /// The drawn layout's cluster stops: byte in `shaped`, x, line.
    pub stops: Vec<(u32, f32, u32)>,
    /// The unbounded layout's stops (byte in `text`, x), when one line.
    pub natural: Vec<(u32, f32)>,
}

/// (eval-rules-4) A CPU crossfade frame blended again by the check's
/// own arithmetic: the largest channel difference from the blend drawn,
/// how many bytes differ by more than one step, and the weight.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BlendRec {
    pub w: f32,
    pub worst: u8,
    pub off: u64,
}

/// (eval-rules-2) The pointer at a frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PtrRec {
    pub s: u32,
    pub at: [f32; 2],
}

/// (eval-rules-2) One sample of a spring (`strand_render::probe::
/// SpringTrace`): the value it drew and what it was asked to do, never
/// how fast it went or where its way began, which `continuity` works out.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SpringRec {
    pub key: String,
    /// Its surface.
    pub s: u32,
    /// The prop (`x`, `opacity`, `bg`…), `w` or `h` (a laid-out size)
    /// or `glide`.
    pub of: String,
    /// Sampled at (ns), when not the frame's time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<u64>,
    /// The value drawn, per channel.
    pub v: Vec<f32>,
    /// What it moves to, and along what.
    pub to: Vec<f32>,
    pub c: CurveRec,
    /// Its settling tolerance per channel (decisions.md border-eps).
    #[serde(default)]
    pub eps: Vec<f32>,
    /// At rest before this sample, here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rest: Option<Vec<f32>>,
    /// A change started at this sample.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<StartRec>,
    /// On the frame its node enters, whether its enter pose sets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enter: Option<bool>,
    /// (eval-rules-3) A palette root's (`of` "palette", `key` "$" and
    /// its token path): the curve a colour takes in the table its swap
    /// sent (`probe::SpringTrace::asked`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask: Option<CurveRec>,
}

/// (eval-rules-2) A change that started at a sample: the motion's last
/// sample before (ns) and a FLIP's jump.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StartRec {
    pub last: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shift: Vec<f32>,
}

/// (eval-rules-2) A curve (`strand_scene::Curve`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CurveRec {
    #[default]
    Instant,
    /// Stiffness and damping ratio.
    Spring { k: f32, z: f32 },
    /// A duration (ns) and its easing: `linear`, `elastic`, `bounce` or
    /// a cubic bézier's four control values.
    Timed { d: u64, ease: EaseRec },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EaseRec {
    #[default]
    Linear,
    Elastic,
    Bounce,
    Bezier([f32; 4]),
}

impl CurveRec {
    pub fn of(c: &strand_scene::Curve) -> CurveRec {
        use strand_scene::protocol::Easing;
        match *c {
            strand_scene::Curve::Instant => CurveRec::Instant,
            strand_scene::Curve::Spring(s) => CurveRec::Spring {
                k: s.stiffness,
                z: s.damping,
            },
            strand_scene::Curve::Timed { duration, easing } => CurveRec::Timed {
                d: duration.as_nanos() as u64,
                ease: match easing {
                    Easing::Linear => EaseRec::Linear,
                    Easing::OutElastic => EaseRec::Elastic,
                    Easing::OutBounce => EaseRec::Bounce,
                    Easing::Bezier { x1, y1, x2, y2 } => EaseRec::Bezier([x1, y1, x2, y2]),
                },
            },
        }
    }
}

impl CurveRec {
    /// (eval-rules-3) The most a value moved along this curve from rest
    /// goes past its target, as a share of the way: a spring's first
    /// peak, `exp(−ζπ/√(1−ζ²))` below critical damping and nothing at
    /// or above it; a bézier's no more than its control values lie
    /// outside 0..1 (the curve stays in their hull); linear and bounce
    /// easings none; an elastic one's first peak (`OutElastic`, under
    /// 0.4: `None` holds it to the old half-way bound).
    pub fn overshoot(self) -> Option<f64> {
        match self {
            CurveRec::Instant => Some(0.0),
            CurveRec::Spring { z, .. } => {
                let z = f64::from(z);
                Some(if z >= 1.0 {
                    0.0
                } else {
                    (-z * std::f64::consts::PI / (1.0 - z * z).sqrt()).exp()
                })
            }
            CurveRec::Timed { ease, .. } => match ease {
                EaseRec::Linear | EaseRec::Bounce => Some(0.0),
                EaseRec::Elastic => None,
                EaseRec::Bezier([_, y1, _, y2]) => Some(
                    [
                        f64::from(y1) - 1.0,
                        f64::from(y2) - 1.0,
                        -f64::from(y1),
                        -f64::from(y2),
                    ]
                    .into_iter()
                    .fold(0.0, f64::max),
                ),
            },
        }
    }
}

impl EaseRec {
    pub fn easing(self) -> strand_scene::protocol::Easing {
        use strand_scene::protocol::Easing;
        match self {
            EaseRec::Linear => Easing::Linear,
            EaseRec::Elastic => Easing::OutElastic,
            EaseRec::Bounce => Easing::OutBounce,
            EaseRec::Bezier([x1, y1, x2, y2]) => Easing::Bezier { x1, y1, x2, y2 },
        }
    }
}

/// (eval-rules-2) A rolling text in one frame: the texts it rolls from
/// and to and how far (0 to 1), and the roll a change interrupted at
/// this frame, as it stood then (its texts and its progress).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RollRec {
    pub key: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub p: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub was: Option<(Option<String>, Option<String>, f32)>,
}

/// How a painted surface's buffer, left by a partial redraw at its
/// buffer's age, differs from the same frame repainted in full into a
/// fresh buffer (the `redraw` check). Pixels are buffer pixels, compared
/// premultiplied; rects are `[x, y, w, h]`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RedrawRec {
    /// Pixels differing past the leg's tolerance on a channel
    /// (`checks::redraw::tolerance`).
    pub px: u64,
    /// Of those, the pixels outside what the frame redrew (its damage
    /// widened by the buffer's age): stale ones it should have redrawn.
    pub stale: u64,
    /// Pixels differing at all.
    pub any: u64,
    /// The largest channel difference.
    pub worst: u8,
    /// The bounding box of the pixels past the tolerance (of every
    /// differing pixel when none is).
    pub at: [u32; 4],
    /// The bounding box of what the frame redrew; `None`: nothing.
    pub redrawn: Option<[u32; 4]>,
    /// Drawn by the GPU leg (the presented frame and its repaint both).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gpu: bool,
    /// The full repaint's image (`frames/<hash>.png`), when the run keeps
    /// images; `Frame::png` is the buffer's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub png: Option<String>,
}

/// A surface pose the compositor applies (`strand_scene::SurfacePose`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PoseRec {
    /// Multiplied into the surface's alpha.
    pub opacity: f32,
    /// The surface drawn this much of its size, its top-left kept.
    pub scale: f32,
    /// Logical px its top-left moves.
    pub x: f32,
    pub y: f32,
}

/// (integration-r7) Where the compositor shows a posed surface at one
/// frame: the one model of a pose on screen every check reads
/// ([`Frame::posed`]). A layer surface's pose offset reaches the
/// compositor only as the margins the surface manager sends
/// (layer-shell `set_margin`, whole logical px; `Frame::shifts`), so it
/// is moved by those, not by the offset render asked for; its scale and
/// opacity are the pose's own, the scale about the surface's top-left
/// (`PoseRec`). A buffer point `v` shows at `shift + scale * v`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Posed {
    pub shift: [f32; 2],
    pub scale: f32,
    pub opacity: f32,
}

impl Posed {
    /// Where buffer coordinate `v` on axis `n` (0 x, 1 y) shows.
    pub fn show(&self, n: usize, v: f32) -> f32 {
        self.shift[n] + self.scale * v
    }

    /// The buffer coordinate on axis `n` that shows at `v`; `v` itself
    /// when the pose scales to nothing.
    pub fn unshow(&self, n: usize, v: f32) -> f32 {
        if self.scale.abs() < 1e-6 {
            return v;
        }
        (v - self.shift[n]) / self.scale
    }

    /// The pose moves or scales what it shows.
    pub fn moves(&self) -> bool {
        self.shift != [0.0, 0.0] || self.scale != 1.0
    }
}

impl Frame {
    /// (integration-r7) How the compositor shows surface `s` at this
    /// frame, when it applies a pose to it ([`Posed`]); `None` at rest.
    pub fn posed(&self, s: u32) -> Option<Posed> {
        let p = self.poses.get(&s)?;
        Some(Posed {
            shift: self.shifts.get(&s).copied().unwrap_or([0.0, 0.0]),
            scale: p.scale,
            opacity: p.opacity,
        })
    }
}

/// A surface's `Painter` regions after a frame.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Regions {
    /// Where it takes input, logical `[x, y, w, h]`; `None` keeps the
    /// surface manager's (its box inside the shadow overhang).
    pub input: Option<Vec<Rect>>,
    /// Where it is opaque, buffer pixels.
    pub opaque: Vec<[i64; 4]>,
    /// Where it asks for a blur behind it, buffer pixels.
    pub blur: Vec<BlurRec>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BlurRec {
    pub rect: [i64; 4],
    pub radii: [f32; 4],
    pub radius: f32,
}

/// One node in one frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row {
    pub key: String,
    pub surface: u32,
    pub drawn: Rect,
    pub visual: Rect,
    pub laid: Rect,
    /// Effective opacity (own times its ancestors').
    pub op: f32,
    /// Its own opacity.
    pub own_op: f32,
    pub scale: [f32; 2],
    pub rotate: f32,
    /// Its own `probe::Motions`, and with its ancestors'.
    pub mot: u32,
    pub emot: u32,
    /// Effective authored `probe::Axes`.
    pub auth: u8,
    /// `opacity0`, `scale0`, `inert`.
    pub hidden: Option<String>,
    /// `S` shaped, `E` estimate, `I` a stand-in.
    pub text: Option<String>,
    pub inherited: [f32; 2],
    pub written: [f32; 2],
    pub spring: [f32; 2],
    pub glide: [f32; 2],
    pub morph: [f32; 2],
    /// Of `inherited`, the part in flight: the ancestors' springs and
    /// glides, a drag's lift included (what they have written at rest
    /// left out). The sandbox's sum of their `spring` and `glide`, and
    /// (integration-r8) `morph`.
    pub flight: [f32; 2],
    /// A text cut to its box: the line box width it asked to be shaped
    /// for (its unbounded layout is wider than its box).
    pub fit: Option<f32>,
    /// A text's glyphs as drawn: the ink of the layout it drew, clipped
    /// where the frame clips it (`[x, y, w, h]`, before transforms).
    pub ink: Option<Rect>,
    /// (eval-coverage) What it painted itself (`crate::paint`); `None`:
    /// nothing.
    pub paint: Option<crate::paint::PaintRec>,
    /// (eval-r3) How far the glyph ink of the layout a text drew reaches
    /// past that layout's own box, left and right (the font's overhang:
    /// an italic, an icon glyph wider than its advance), logical px.
    pub ov: Option<[f32; 2]>,
    /// (eval-rules-2) The clip its ancestors set, `[x, y, w, h]`, when
    /// narrower than its surface.
    pub clip: Option<Rect>,
}

/// `v` to 1/64.
pub fn q(v: f32) -> f32 {
    (v * 64.0).round() / 64.0
}

/// (integration-r7) The trace's quantum, logical px: [`q`] rounds each
/// value it records to it, so two values recorded apart differ from
/// what was drawn by up to one quantum between them.
pub const QUANTUM: f64 = 1.0 / 64.0;

impl Serialize for Row {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = s.serialize_seq(Some(COLS.len()))?;
        seq.serialize_element(&self.key)?;
        seq.serialize_element(&self.surface)?;
        for r in [self.drawn, self.visual, self.laid] {
            for v in r {
                seq.serialize_element(&q(v))?;
            }
        }
        seq.serialize_element(&self.op)?;
        seq.serialize_element(&self.own_op)?;
        seq.serialize_element(&self.scale[0])?;
        seq.serialize_element(&self.scale[1])?;
        seq.serialize_element(&self.rotate)?;
        seq.serialize_element(&self.mot)?;
        seq.serialize_element(&self.emot)?;
        seq.serialize_element(&self.auth)?;
        seq.serialize_element(&self.hidden)?;
        seq.serialize_element(&self.text)?;
        for p in [
            self.inherited,
            self.written,
            self.spring,
            self.glide,
            self.morph,
            self.flight,
        ] {
            seq.serialize_element(&q(p[0]))?;
            seq.serialize_element(&q(p[1]))?;
        }
        seq.serialize_element(&self.fit.map(q))?;
        seq.serialize_element(&self.ink.map(|r| r.map(q)))?;
        seq.serialize_element(&self.paint)?;
        seq.serialize_element(&self.ov.map(|p| p.map(q)))?;
        seq.serialize_element(&self.clip.map(|r| r.map(q)))?;
        seq.end()
    }
}

impl<'de> Deserialize<'de> for Row {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v: Vec<serde_json::Value> = Vec::deserialize(d)?;
        // A trace from before `fx`, `fy` were recorded (an A/B against
        // an earlier revision) reads with nothing in flight; one from
        // before `fit` and `ink`, or before `paint`, without them.
        if ![
            COLS.len(),
            OV_COLS,
            PAINT_COLS,
            INK_COLS,
            FLIGHT_COLS,
            OLD_COLS,
        ]
        .contains(&v.len())
        {
            return Err(D::Error::custom(format!(
                "a node row has {} columns, not {}",
                v.len(),
                COLS.len()
            )));
        }
        let f = |i: usize| -> Result<f32, D::Error> {
            v[i].as_f64()
                .map(|x| x as f32)
                .ok_or_else(|| D::Error::custom(format!("column {} is not a number", COLS[i])))
        };
        let u = |i: usize| -> Result<u64, D::Error> {
            v[i].as_u64()
                .ok_or_else(|| D::Error::custom(format!("column {} is not a count", COLS[i])))
        };
        let s = |i: usize| -> Result<Option<String>, D::Error> {
            match &v[i] {
                serde_json::Value::Null => Ok(None),
                serde_json::Value::String(t) => Ok(Some(t.clone())),
                _ => Err(D::Error::custom(format!("column {} is not text", COLS[i]))),
            }
        };
        let rect =
            |i: usize| -> Result<Rect, D::Error> { Ok([f(i)?, f(i + 1)?, f(i + 2)?, f(i + 3)?]) };
        let pair = |i: usize| -> Result<[f32; 2], D::Error> { Ok([f(i)?, f(i + 1)?]) };
        Ok(Row {
            key: s(0)?.ok_or_else(|| D::Error::custom("a node row without a key"))?,
            surface: u(1)? as u32,
            drawn: rect(2)?,
            visual: rect(6)?,
            laid: rect(10)?,
            op: f(14)?,
            own_op: f(15)?,
            scale: pair(16)?,
            rotate: f(18)?,
            mot: u(19)? as u32,
            emot: u(20)? as u32,
            auth: u(21)? as u8,
            hidden: s(22)?,
            text: s(23)?,
            inherited: pair(24)?,
            written: pair(26)?,
            spring: pair(28)?,
            glide: pair(30)?,
            morph: pair(32)?,
            flight: if v.len() > OLD_COLS {
                pair(34)?
            } else {
                [0.0, 0.0]
            },
            fit: match v.get(36) {
                None | Some(serde_json::Value::Null) => None,
                Some(_) => Some(f(36)?),
            },
            ink: match v.get(37) {
                None | Some(serde_json::Value::Null) => None,
                Some(r) => {
                    let r: Vec<f32> = serde_json::from_value(r.clone())
                        .map_err(|_| D::Error::custom("column ink is not a rect"))?;
                    match r.as_slice() {
                        [x, y, w, h] => Some([*x, *y, *w, *h]),
                        _ => return Err(D::Error::custom("column ink is not a rect")),
                    }
                }
            },
            paint: match v.get(38) {
                None | Some(serde_json::Value::Null) => None,
                Some(p) => Some(
                    serde_json::from_value(p.clone())
                        .map_err(|_| D::Error::custom("column paint is not a paint"))?,
                ),
            },
            ov: match v.get(39) {
                None | Some(serde_json::Value::Null) => None,
                Some(p) => Some(
                    serde_json::from_value(p.clone())
                        .map_err(|_| D::Error::custom("column ov is not a pair"))?,
                ),
            },
            clip: match v.get(40) {
                None | Some(serde_json::Value::Null) => None,
                Some(p) => Some(
                    serde_json::from_value(p.clone())
                        .map_err(|_| D::Error::custom("column clip is not a rect"))?,
                ),
            },
        })
    }
}

impl RunTrace {
    /// An empty trace of the current schema.
    pub fn new(scenario: ScenarioInfo, build: BuildInfo) -> RunTrace {
        RunTrace {
            schema: SCHEMA,
            scenario,
            build,
            cols: COLS.iter().map(|c| c.to_string()).collect(),
            ..RunTrace::default()
        }
    }

    /// The hash of everything deterministic the run drew: each frame's
    /// tick, surfaces and nodes, its deterministic costs and held texts,
    /// its springs' samples, rolls and drags, the segments and rests.
    /// Wall times and images are left out.
    pub fn compute_digest(&self) -> String {
        let mut h = blake3::Hasher::new();
        let mut put = |v: &dyn erased::Json| {
            h.update(&v.bytes());
            h.update(b"\n");
        };
        for f in &self.frames {
            let det: BTreeMap<u32, Cost> = f
                .cost
                .iter()
                .map(|(s, c)| {
                    (
                        *s,
                        Cost {
                            flatten_us: 0,
                            text_wait_us: 0,
                            raster_us: 0,
                            ..*c
                        },
                    )
                })
                .collect();
            put(&(f.k, f.t_ns, &f.painted, &f.wants, &f.sizes));
            put(&(&det, &f.held_text, &f.n));
            put(&f.regions);
            put(&(f.scale, f.driven));
            put(&(&f.origins, &f.poses));
            if !f.fades.is_empty() {
                put(&f.fades);
            }
            if !f.offsets.is_empty() {
                put(&f.offsets);
            }
            // (eval-rules-2) Its springs' samples, rolls, cut sides, the
            // pointer and the drags, when there are any: `determinism`
            // holds them too.
            if !(f.sp.is_empty()
                && f.rolls.is_empty()
                && f.cuts.is_empty()
                && f.ptr.is_none()
                && f.drags.is_empty())
            {
                put(&(&f.period, &f.sp, &f.rolls));
                put(&(&f.cuts, &f.ptr, &f.drags));
            }
            // (eval-rules-4) Images' decodes and cut texts' lines.
            if !(f.imgs.is_empty() && f.lines.is_empty()) {
                put(&(&f.imgs, &f.lines));
            }
            // (eval-rules-6) Rings, gaps and texts that could break.
            if !(f.rings.is_empty() && f.gaps.is_empty() && f.loose.is_empty()) {
                put(&(&f.rings, &f.gaps, &f.loose));
            }
            if !f.shifts.is_empty() {
                put(&f.shifts);
            }
        }
        put(&self.segments);
        put(&self.rests);
        put(&self.nodes);
        format!("blake3:{}", h.finalize().to_hex())
    }

    /// The screen's scale at `f`: its own, else the scenario's, else 1.
    pub fn scale_at(&self, f: &Frame) -> f64 {
        if f.scale > 0.0 {
            f.scale
        } else if self.scenario.scale > 0.0 {
            self.scenario.scale
        } else {
            1.0
        }
    }

    /// The least scale any frame was drawn at (the scenario's when none
    /// was): the coarsest physical pixel, in logical px, is its inverse.
    pub fn min_scale(&self) -> f64 {
        self.frames
            .iter()
            .map(|f| self.scale_at(f))
            .fold(None, |m: Option<f64>, s| Some(m.map_or(s, |m| m.min(s))))
            .unwrap_or(if self.scenario.scale > 0.0 {
                self.scenario.scale
            } else {
                1.0
            })
    }

    /// (eval-rules-5) Motion was reduced over the whole run: from the
    /// start (the header's `reduce_motion`, a reduced re-run) and in
    /// every segment (no `reduce-motion off` step turned it off).
    pub fn reduced_throughout(&self) -> bool {
        self.scenario.reduced_motion && self.segments.iter().all(|s| s.reduced_motion)
    }

    /// Where each surface was first placed (its first frame's origin).
    pub fn first_origins(&self) -> BTreeMap<u32, [f32; 2]> {
        let mut first = BTreeMap::new();
        for f in &self.frames {
            for (s, o) in &f.origins {
                first.entry(*s).or_insert(*o);
            }
        }
        first
    }

    /// How far surface `s` has moved at `f` from where it was first
    /// placed (`first`, [`RunTrace::first_origins`]).
    pub fn moved(first: &BTreeMap<u32, [f32; 2]>, f: &Frame, s: u32) -> [f32; 2] {
        match (f.origins.get(&s), first.get(&s)) {
            (Some(o), Some(a)) => [o[0] - a[0], o[1] - a[1]],
            _ => [0.0, 0.0],
        }
    }

    /// The trace as the eye sees it, where the compositor moves or poses
    /// a surface:
    ///
    /// - a surface that moves on the output (a content-sized popup the
    ///   compositor re-centres as it grows): every row's boxes and ink,
    ///   and every input region, moved by how far its surface has moved
    ///   since it was first placed, so content render keeps in place on
    ///   screen while its surface moves under it reads as still;
    /// - a surface pose the compositor applies (`compositor_poses`): its
    ///   opacity multiplied into every row's, and into the root's own;
    ///   its scale (about the surface's top-left) applied to every visual
    ///   box and to the root's scale, and every visual box moved where
    ///   the compositor put the surface for its offset (`Frame::shifts`,
    ///   the margins sent on the wire, eval-rules-6: not the offset render
    ///   asked for); the rows authored on every axis it moves, as a pose
    ///   authors its subtree.
    ///
    /// `None` when no surface moved and none was posed (the trace as it
    /// is).
    pub fn on_screen(&self) -> Option<RunTrace> {
        let first = self.first_origins();
        let moves = self.frames.iter().any(|f| {
            !f.poses.is_empty()
                || f.origins
                    .keys()
                    .any(|s| RunTrace::moved(&first, f, *s) != [0.0, 0.0])
        });
        if !moves {
            return None;
        }
        let roots: BTreeMap<u32, String> = self
            .surfaces
            .iter()
            .map(|s| (s.id, s.root.clone()))
            .collect();
        let mut t = self.clone();
        t.on_screen = true;
        for f in &mut t.frames {
            let shift: BTreeMap<u32, [f32; 2]> = f
                .origins
                .keys()
                .map(|s| (*s, RunTrace::moved(&first, f, *s)))
                .collect();
            let by = |s: u32| shift.get(&s).copied().unwrap_or([0.0, 0.0]);
            let mv = |r: &mut Rect, d: [f32; 2]| {
                r[0] += d[0];
                r[1] += d[1];
            };
            let posed: BTreeMap<u32, (Posed, bool)> = f
                .poses
                .iter()
                .filter_map(|(s, p)| Some((*s, (f.posed(*s)?, p.x != 0.0 || p.y != 0.0))))
                .collect();
            for r in &mut f.n {
                if let Some((p, offset)) = posed.get(&r.surface).copied() {
                    let k = p.scale;
                    let [px, py] = p.shift;
                    r.op *= p.opacity;
                    r.visual = [
                        px + k * r.visual[0],
                        py + k * r.visual[1],
                        k * r.visual[2],
                        k * r.visual[3],
                    ];
                    if let Some(c) = r.clip.as_mut() {
                        *c = [px + k * c[0], py + k * c[1], k * c[2], k * c[3]];
                    }
                    if roots.get(&r.surface) == Some(&r.key) {
                        r.own_op *= p.opacity;
                        r.scale = [r.scale[0] * k, r.scale[1] * k];
                    }
                    use strand_render::probe::Axes;
                    let mut auth = Axes::NONE;
                    if p.opacity != 1.0 {
                        auth |= Axes::OPACITY;
                    }
                    if k != 1.0 {
                        auth |= Axes::X | Axes::Y | Axes::W | Axes::H | Axes::SCALE;
                    }
                    if offset || px != 0.0 || py != 0.0 {
                        auth |= Axes::X | Axes::Y;
                    }
                    r.auth |= auth.0;
                }
                let d = by(r.surface);
                mv(&mut r.drawn, d);
                mv(&mut r.visual, d);
                mv(&mut r.laid, d);
                if let Some(ink) = r.ink.as_mut() {
                    mv(ink, d);
                }
                if let Some(c) = r.clip.as_mut() {
                    mv(c, d);
                }
            }
            for (s, reg) in &mut f.regions {
                let d = by(*s);
                for r in reg.input.iter_mut().flatten() {
                    mv(r, d);
                }
            }
            if let Some(q) = f.ptr.as_mut() {
                let d = by(q.s);
                q.at = [q.at[0] + d[0], q.at[1] + d[1]];
            }
        }
        Some(t)
    }

    /// Fills in the digest.
    pub fn seal(&mut self) {
        self.digest = self.compute_digest();
    }

    /// The trace as JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }

    pub fn from_json(text: &str) -> Result<RunTrace, String> {
        serde_json::from_str(text).map_err(|e| e.to_string())
    }
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

mod erased {
    /// Something hashed as its JSON.
    pub trait Json {
        fn bytes(&self) -> Vec<u8>;
    }

    impl<T: serde::Serialize> Json for T {
        fn bytes(&self) -> Vec<u8> {
            serde_json::to_vec(self).unwrap_or_default()
        }
    }
}
