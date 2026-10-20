//! (eval-sandbox) The evaluation sandbox's view into a frame: what each
//! painted frame drew, node by node, and a hold on inline text so a test
//! can make text arrive late on purpose (docs/eval.md,
//! docs/decisions.md eval-sandbox).
//!
//! Built only with the `eval-hooks` feature, which no crate of the
//! workspace enables: the `eval/` workspace does
//! (`scripts/eval/check-isolation.sh` checks it). Plain data, read by the
//! harness; nothing here changes what a frame draws, but for the one
//! mutant the `redraw` check's own tests turn on
//! ([`Renderer::set_damage_inset`]).
//!
//! - [`Renderer::set_frame_trace`] turns the trace on: every
//!   `Painter::paint` (CPU, GPU readback or GPU present, empty damage
//!   included; never `update`'s preview) pushes one [`FrameTrace`],
//!   taken with [`Renderer::take_frame_traces`].
//! - [`Renderer::set_text_hold`] keeps the layouts inline shaping makes
//!   from being delivered (or [`Renderer::set_text_hold_with`], those of
//!   the texts it picks): the request stays pending, so layout and the
//!   frame treat the text as a late worker's (placed by its estimate)
//!   until [`Renderer::release_text`] delivers it.
//!
//! - [`Renderer::repaint_fresh`] (and, for a GPU-presented surface,
//!   `Renderer::fresh_gpu_frame`) draws a surface's last frame again in
//!   full, as into a fresh buffer, without touching what later frames
//!   read: the `redraw` check compares it with the buffer the partial
//!   redraw left.
//!
//! [`Renderer::set_frame_trace`]: crate::Renderer::set_frame_trace
//! [`Renderer::repaint_fresh`]: crate::Renderer::repaint_fresh
//! [`Renderer::set_damage_inset`]: crate::Renderer::set_damage_inset
//! [`Renderer::take_frame_traces`]: crate::Renderer::take_frame_traces
//! [`Renderer::set_text_hold`]: crate::Renderer::set_text_hold
//! [`Renderer::set_text_hold_with`]: crate::Renderer::set_text_hold_with
//! [`Renderer::release_text`]: crate::Renderer::release_text

use std::ops::{BitOr, BitOrAssign};
use std::sync::Arc;
use std::time::Duration;

use strand_scene::{Backend, Color, LogicalRect, NodeId, NodeKind, Paint, SurfaceId};
use strand_text::TextLayout;
use vello_cpu::kurbo;

use crate::PaintTimes;

/// One painted frame of one surface.
#[derive(Clone, Debug)]
pub struct FrameTrace {
    pub surface: SurfaceId,
    /// The frame's presentation time (`PaintTarget::time`).
    pub time: Duration,
    pub backend: Backend,
    /// Flattened for this paint; false when the scene flattened at rest
    /// was reused (its nodes are that flatten's, which nothing moved).
    pub fresh: bool,
    /// This frame's own damage, physical pixels, before the buffer's age
    /// widened it.
    pub damage_px: u64,
    /// Pixels the CPU rasterised (0 when the GPU drew it, or nothing).
    pub raster_px: u64,
    /// Display items and node records of the drawn scene.
    pub items: u32,
    pub records: u32,
    /// Layout passes run during this paint, and nodes the surface's last
    /// layout placed.
    pub layout_passes: u32,
    pub layout_nodes: u32,
    /// Text requests in flight, and layouts the hold keeps back.
    pub text_pending: u32,
    pub text_held: u32,
    /// Wall-clock times: advisory only, never deterministic.
    pub times: PaintTimes,
    /// (eval-coverage) The weight the frame was blended over its
    /// crossfade snapshot at (a theme swap's crossfade), if it was.
    pub fade: Option<f32>,
    /// Every node drawn, or found hidden, in paint order (a parent
    /// before its children). Unplaced nodes and the subtrees of hidden
    /// nodes are absent.
    pub nodes: Arc<[NodeTrace]>,
    /// (eval-rules-2) The frame period the frame's changes started one
    /// of before it (`Animator::set_period`: its surface's estimated
    /// refresh; zero when not known yet, and `START_LEAD` stands in).
    pub period: Duration,
    /// (eval-rules-2) Every sample the frame took of a prop's spring, a
    /// laid-out size's or a glide's, in the order taken.
    pub springs: Vec<SpringTrace>,
}

/// (eval-rules-2) What a spring is: a prop's (`x`, `opacity`, `bg`…), a
/// laid-out size's (0 the width, 1 the height) or a glide's (FLIP, a
/// drag's way back).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SpringOf {
    Prop(strand_scene::Prop),
    Size(u8),
    Glide,
    /// (eval-rules-3) A palette root's spring in a theme swap
    /// ([`SpringTrace::token`] names it): premultiplied OKLab plus alpha.
    Palette,
}

/// (eval-rules-2) One sample a painted frame took of a spring: the
/// value it drew, and what the motion was asked to do (its target, its
/// curve, a change that started at this sample and from what), never
/// the velocity or the place it started from, which the `continuity`
/// check works out itself from where the value was (docs/eval.md,
/// "Checks").
#[derive(Clone, Debug, PartialEq)]
pub struct SpringTrace {
    pub node: NodeId,
    pub what: SpringOf,
    /// The time it was sampled at, and the value it gave there (its
    /// channels: one for a length, four for a colour in premultiplied
    /// OKLab, five for a border).
    pub at: Duration,
    pub value: Vec<f32>,
    /// What it moves to, and how (the curve of the change this sample
    /// started, or of the one it was already on).
    pub target: Vec<f32>,
    pub curve: strand_scene::Curve,
    /// Its settling tolerance, per channel (decisions.md border-eps).
    pub eps: Vec<f32>,
    /// It was at rest before this sample, here (made at rest, or
    /// settled).
    pub rest: Option<Vec<f32>>,
    /// A change it was asked for started at this sample.
    pub start: Option<SpringStart>,
    /// A prop's spring on the frame its node enters: whether its enter
    /// pose sets that prop. `None` otherwise.
    pub entering: Option<bool>,
    /// (eval-rules-3) A palette root's token path ([`SpringOf::Palette`];
    /// `node` is then the root of the surface whose frame sampled it).
    pub token: Option<std::sync::Arc<str>>,
    /// (eval-rules-3) A palette root's: the curve a colour takes in the
    /// table the swap sent, `$motion.effects` unless the swap names
    /// another (`TokenScope::transition` of the swap's transition for
    /// `Prop::Color`), which the `colour` check holds the root's own
    /// curve to (design.md "Springs everywhere").
    pub asked: Option<strand_scene::Curve>,
}

/// (eval-rules-2) A change that started at a sample: the motion's last
/// sample before it (or the frame it was made after) and the jump a
/// FLIP adds where it starts.
#[derive(Clone, Debug, PartialEq)]
pub struct SpringStart {
    pub last: Option<Duration>,
    pub shift: Vec<f32>,
}

impl SpringTrace {
    /// A sample of a motion that was `pre` before it, which gave `value`.
    pub(crate) fn of<const N: usize>(
        node: NodeId,
        what: SpringOf,
        at: Duration,
        pre: &strand_scene::MotionView<N>,
        value: &[f32],
    ) -> SpringTrace {
        let (target, curve) = pre
            .pending
            .map_or((pre.target, pre.curve), |(t, c, _)| (t, c));
        SpringTrace {
            node,
            what,
            at,
            value: value.to_vec(),
            target: target.to_vec(),
            curve,
            eps: pre.eps.to_vec(),
            rest: (pre.curve == strand_scene::Curve::Instant).then(|| pre.target.to_vec()),
            start: pre.pending.map(|(_, _, shift)| SpringStart {
                last: pre.last,
                shift: shift.to_vec(),
            }),
            entering: None,
            token: None,
            asked: None,
        }
    }
}

/// One node of a frame. Rects are surface logical pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeTrace {
    pub node: NodeId,
    pub parent: Option<NodeId>,
    pub kind: NodeKind,
    /// The layout box, before paint offsets (the surface root: its frame).
    pub laid: LogicalRect,
    /// The box as drawn: moved by paint offsets, springs, glides and a
    /// morph's translate, edges snapped as drawn; before `scale`,
    /// `rotate`, a morph's scale or jelly.
    pub drawn: LogicalRect,
    /// `drawn` through every transform above and on the node, axis
    /// aligned, at the physical pixels it covers: what the eye sees.
    pub visual: LogicalRect,
    /// The node's own opacity this frame (not multiplied by its
    /// ancestors').
    pub opacity: f32,
    /// Its own scale per axis (a morph's included) and rotation, degrees.
    pub scale: [f32; 2],
    pub rotate: f32,
    /// What moved it from `laid` to `drawn`.
    pub parts: OffsetParts,
    pub motions: Motions,
    /// Axes authored motion (springs of props logic set, poses, `play`,
    /// morphs, lean, jelly, drags, time) moves this frame on the node
    /// itself. A glide (FLIP, derived from layout) is not authored.
    pub authored: Axes,
    /// Why nothing of it shows (its subtree is then absent, but under
    /// [`Hidden::Clipped`]).
    pub hidden: Option<Hidden>,
    /// What a text, button or input drew its text from.
    pub text: Option<TextDrawn>,
    /// A text's line box width when its unbounded layout is wider than
    /// its box (the width it asked to be shaped for: cut, ellipsised or
    /// wrapped); `None` when it fits, or for anything but a text.
    pub fit: Option<f32>,
    /// A text's glyphs as drawn this frame: the ink of the layout it
    /// drew, where it drew it, clipped to its box when the frame clips
    /// it there; surface logical pixels, before `scale` and `rotate`.
    /// `None` when it drew no glyphs (an estimate, an empty text).
    pub ink: Option<LogicalRect>,
    /// (eval-r3) How far the ink of the layout it drew reaches past that
    /// layout's own box (`TextLayout::ink` against `size`), on the left
    /// and on the right, logical pixels: the font's own overhang, which
    /// lets a check tell glyphs the shaper put past their box from a
    /// layout drawn in the wrong place. `None` with `ink`.
    pub overhang: Option<[f32; 2]>,
    /// (eval-coverage) What it painted itself this frame (its subtree's
    /// paint is its children's): empty for a hidden node.
    pub paint: NodePaint,
    /// (eval-rules-2) The clip its ancestors set (`clip: true`, a
    /// `scroll`, `list` or `pages`, a box whose size springs), surface
    /// logical pixels, axis aligned: `None` when nothing narrower than
    /// its surface clips it.
    pub clip: Option<LogicalRect>,
    /// (eval-rules-2) A `roll: true` text rolling: the text it rolls
    /// from, the one it rolls to and how far; and on the frame a change
    /// interrupted a roll, that roll as it stood then.
    pub roll: Option<RollDrawn>,
    /// (eval-rules-2) A slider held in a drag: the value the drag gives
    /// it (`Widgets::drags`), which it draws until the release.
    pub drag: Option<f32>,
    /// (eval-rules-4) An `image` or `icon`'s decodes as drawn this frame
    /// (an image swap's old one first): where each pixmap went in its
    /// box, so a check places the source by `fit:` itself.
    pub images: Vec<ImageDrawn>,
    /// (eval-rules-4) A text cut to its box (`fit`), drawn from the
    /// layout shaped for that line box at this scale: its lines and its
    /// unbounded layout's cluster stops, so a check sees whether each
    /// line keeps all that fits.
    pub cut: Option<CutDrawn>,
    /// (eval-rules-6) Its `border:` ring as drawn: the outline the
    /// border fills, and the props it was drawn from, so a check sets
    /// the drawn ring beside the shape the props ask for.
    pub ring: Option<RingDrawn>,
    /// (eval-rules-6) A container's `gap:` as its props give it this
    /// frame, logical px; `None` when unset (or not a number).
    pub gap: Option<f32>,
}

/// (eval-rules-6) A `border:` ring as the flattener drew it (the facts,
/// never the shape it should have): the even-odd outline it filled, in
/// device pixels before the node's transforms, and the props it was
/// drawn from as this frame's props resolve them.
#[derive(Clone, Debug, PartialEq)]
pub struct RingDrawn {
    /// The box it is drawn inside: the node's drawn box, its edges on
    /// the physical pixels they fall on, device px (`x0, y0, x1, y1`).
    pub frame: [f64; 4],
    /// The outline it filled (even-odd), device px.
    pub path: kurbo::BezPath,
    /// `border:`'s width as the props give it, logical px.
    pub width: f32,
    /// `radius:` as the props give it (its kind's default when unset:
    /// a button's or a meter's); `None`: square.
    pub radius: Option<strand_scene::PropValue>,
    /// The drawn box's logical size, which a percentage radius is of.
    pub size: [f32; 2],
    /// Drawn along a `shape:` outline or with `corners: squircle`.
    pub shaped: bool,
    /// Device pixels per logical pixel.
    pub scale: f64,
}

/// (eval-rules-4) One decode of an `image` or `icon` as drawn: the facts
/// of the pixels, never where a fit would put them.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageDrawn {
    /// The box it is drawn into: the node's box, its edges on the
    /// physical pixels it fills, surface logical pixels, before
    /// transforms.
    pub rect: LogicalRect,
    /// Where the whole decoded pixmap is drawn, surface logical pixels.
    pub dest: LogicalRect,
    /// The pixmap's size, pixels.
    pub pixmap: [u32; 2],
    /// The source's size as decoded (`Decoded::source`): what the decode
    /// placed in the pixmap, by `fit`, at the pixmap's size.
    pub source: [f64; 2],
    /// `fit:` as the node has it (`contain` unless set), and the fit the
    /// decode placed the source in its pixmap by.
    pub fit: &'static str,
    pub pixmap_fit: &'static str,
    /// A decode for another box size stands in while its own is made.
    pub stand_in: bool,
    /// (integration-r7) The `source` the decode was made from: the
    /// node's own, or, for the old image an image swap draws under the
    /// new one (`transition:`), the source it swaps from.
    pub file: String,
    /// (eval-coverage-3) A symbolic icon's colour: it is drawn as a mask
    /// in it.
    pub tint: Option<Color>,
}

/// (eval-rules-4) A text drawn cut to its line box.
#[derive(Clone, Debug, PartialEq)]
pub struct CutDrawn {
    /// The text it was asked to show (the drawn layout's stops count
    /// bytes of it, or of it cut and ended with an ellipsis).
    pub text: Arc<str>,
    /// `ellipsis:` (`end`, `start`, `middle`) and `max_lines`, as asked.
    pub ellipsis: Option<&'static str>,
    pub max_lines: Option<u32>,
    /// The drawn layout's cluster stops (byte in `shaped`, x from the
    /// layout's left edge, line), logical pixels.
    pub stops: Vec<(u32, f32, u32)>,
    /// The unbounded layout's cluster stops (byte in `text`, x), when it
    /// is one line: how wide each prefix of the text is.
    pub natural: Vec<(u32, f32)>,
}

/// (eval-rules-2) A rolling text in one frame (`crate::effects::roll`).
#[derive(Clone, Debug, PartialEq)]
pub struct RollDrawn {
    /// The texts its layouts were shaped from (`None`: unknown, a layout
    /// from before the trace was on), and its progress from 0 to 1.
    pub from: Option<Arc<str>>,
    pub to: Option<Arc<str>>,
    pub p: f32,
    /// The roll a change interrupted at this frame: its texts and its
    /// progress at this frame's time.
    pub was: Option<RollWas>,
}

/// (eval-rules-2) A roll as it stood when a change interrupted it: the
/// text it rolled from, the one it rolled to, and its progress.
pub type RollWas = (Option<Arc<str>>, Option<Arc<str>>, f32);

/// (eval-coverage) What a node painted itself in one frame, read from
/// the display items it added to the flattened scene (not from pixels):
/// the colours each item was drawn in, where its shadows reach, and which
/// text its glyphs came from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodePaint {
    /// Its fills, in paint order: its background (a meter's track and
    /// value, a button's themed one).
    pub fills: Vec<Paint>,
    /// (eval-coverage-2) Where each of its fills lies, in the same order:
    /// the bounds of the shape it fills, surface logical pixels, before
    /// transforms (a slider's track, value and knob, a meter's value).
    pub fill_boxes: Vec<LogicalRect>,
    /// Its borders' paints.
    pub borders: Vec<Paint>,
    /// Its drop shadows, a box's glow included.
    pub shadows: Vec<ShadowDrawn>,
    /// Its glyphs, by the text they were shaped from and the paint they
    /// were drawn in.
    pub glyphs: Vec<GlyphsDrawn>,
}

impl NodePaint {
    pub fn is_empty(&self) -> bool {
        self.fills.is_empty()
            && self.borders.is_empty()
            && self.shadows.is_empty()
            && self.glyphs.is_empty()
    }
}

/// One drop shadow as drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShadowDrawn {
    pub color: Color,
    /// The area it may cover (its offset, spread box grown by its blur's
    /// reach), surface logical pixels, before transforms.
    pub extent: LogicalRect,
    /// (eval-rules-8) What its props asked this frame, springs applied:
    /// its offset, blur and spread (`x`, `y`, `blur`, `spread`), logical
    /// px, before the frame's scale.
    pub asked: [f32; 4],
    /// (eval-rules-8) The box it is cast by, as drawn: surface logical
    /// px, before transforms.
    pub frame: LogicalRect,
}

/// A node's glyphs from one text, drawn in one paint.
#[derive(Clone, Debug, PartialEq)]
pub struct GlyphsDrawn {
    /// Its colour (`Paint::Solid`), or the `fill:` it was painted with.
    pub paint: Paint,
    /// The colours of its span slots (marks, markup links), if any.
    pub spans: Vec<Color>,
    /// The text its layout was shaped from; `None` when the renderer
    /// does not know it (a layout from before the trace was on).
    pub text: Option<Arc<str>>,
    /// Which of that layout's glyphs were drawn (indices in visual
    /// order), when not all of them were: a rolling text's letters
    /// rolling out of an older text, or in from the new one.
    pub subset: Option<Vec<u32>>,
}

/// (eval-coverage) A shaped text as the trace names it: its text and its
/// glyphs' places in its layout (physical pixels, in visual order), so a
/// letter drawn alone (`roll: true`, `letters`) is found in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProbeText {
    pub(crate) text: Arc<str>,
    pub(crate) glyphs: Arc<[(i32, i32)]>,
}

/// The paint offsets between a node's laid box and its drawn box,
/// logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct OffsetParts {
    /// The ancestors' offsets, as written and in flight.
    pub inherited: [f32; 2],
    /// Its own `x`, `y` as written.
    pub written: [f32; 2],
    /// Its own `x`, `y` springs' way from what is written.
    pub spring: [f32; 2],
    /// Its glide (FLIP) or lift.
    pub glide: [f32; 2],
    /// A shared-element morph's translate.
    pub morph: [f32; 2],
}

/// What moves a node this frame (bits).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Motions(pub u32);

impl Motions {
    /// A prop logic set springs to its new value (`x`, `y`, `scale`,
    /// `rotate`, colours…).
    pub const PROP_SPRING: Self = Self(1);
    pub const OPACITY_SPRING: Self = Self(1 << 1);
    /// A FLIP glide from where layout last drew it.
    pub const GLIDE: Self = Self(1 << 2);
    /// Its laid-out size springs.
    pub const SIZE_SPRING: Self = Self(1 << 3);
    /// It plays (or waits to play) its `enter` pose.
    pub const ENTER: Self = Self(1 << 4);
    /// It plays its `exit` pose, or is inside a subtree that does.
    pub const EXIT: Self = Self(1 << 5);
    /// A `play` is playing.
    pub const KEYFRAMES: Self = Self(1 << 6);
    /// A shared-element `morph:`.
    pub const SHARED_MORPH: Self = Self(1 << 7);
    /// A transition mask (`transition:`).
    pub const REVEAL: Self = Self(1 << 8);
    /// It waits for its `stagger:` turn.
    pub const STAGGER: Self = Self(1 << 9);
    /// `parallax:` or `tilt:` follows the pointer.
    pub const LEAN: Self = Self(1 << 10);
    pub const JELLY: Self = Self(1 << 11);
    /// Lifted by a drag, or springing back from one.
    pub const LIFT: Self = Self(1 << 12);
    /// A prop that places it follows time (`t`, `wave(…)`).
    pub const TIME: Self = Self(1 << 13);
    /// (eval-rules) Its own background springs between colours (`bg`, a
    /// meter's `value` or `track`). It comes with `PROP_SPRING`, and
    /// tells a node's own colour transition from a colour the token
    /// graph derives anew each frame (a theme swap).
    pub const FILL_SPRING: Self = Self(1 << 14);
    /// (eval-rules) Its own text colour springs (`color`, a text's solid
    /// `fill`).
    pub const INK_SPRING: Self = Self(1 << 15);
    /// (eval-rules) Its own border springs.
    pub const LINE_SPRING: Self = Self(1 << 16);
    /// (eval-coverage-2) A `scroll` or `list` whose offset moves by
    /// itself this frame (a wheel step's spring, a fling): it lays its
    /// content out anew each frame with no change from logic.
    pub const SCROLL: Self = Self(1 << 17);
    /// (eval-coverage-2) A scroll of [`Motions::SCROLL`] flinging on
    /// after a touchpad lift (`reduced_motion` drops the fling).
    pub const FLING: Self = Self(1 << 18);

    pub const NONE: Self = Self(0);

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl BitOr for Motions {
    type Output = Self;
    fn bitor(self, o: Self) -> Self {
        Self(self.0 | o.0)
    }
}

impl BitOrAssign for Motions {
    fn bitor_assign(&mut self, o: Self) {
        self.0 |= o.0;
    }
}

/// Axes of a node's geometry and look (bits).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Axes(pub u8);

impl Axes {
    pub const X: Self = Self(1);
    pub const Y: Self = Self(1 << 1);
    pub const W: Self = Self(1 << 2);
    pub const H: Self = Self(1 << 3);
    pub const OPACITY: Self = Self(1 << 4);
    pub const SCALE: Self = Self(1 << 5);
    pub const ROTATE: Self = Self(1 << 6);
    /// Where and how large it is drawn: x, y, w, h, scale and rotate.
    pub const GEOMETRY: Self = Self(0b110_1111);
    pub const ALL: Self = Self(0b111_1111);
    pub const NONE: Self = Self(0);

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl BitOr for Axes {
    type Output = Self;
    fn bitor(self, o: Self) -> Self {
        Self(self.0 | o.0)
    }
}

impl BitOrAssign for Axes {
    fn bitor_assign(&mut self, o: Self) {
        self.0 |= o.0;
    }
}

/// Why a node draws nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Hidden {
    /// Its opacity is 0 (as an `enter { opacity: 0 }` starts).
    Opacity0,
    /// Its scale is 0.
    Scale0,
    /// Drawn, but inside an exit pose's subtree: never hit (its pixels
    /// show; the checks treat it as leaving).
    Inert,
    /// (eval-coverage-2) On its surface, but wholly outside the clip its
    /// ancestors set (a `scroll`'s or `list`'s rows scrolled out of its
    /// box, a `clip: true` box's overflow): none of its pixels show. Its
    /// children are traced, each with its own reason.
    Clipped,
}

/// What a text node's glyphs came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextDrawn {
    /// The layout shaped for its scale and box.
    Shaped,
    /// No layout yet: nothing drawn, its box laid out without it (a
    /// list's row's by an estimate; decisions.md late-text-dip).
    Estimate,
    /// Another width's or scale's layout stands in while its own is
    /// shaped.
    StandIn,
}

/// A layout the text hold keeps back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldText {
    pub node: NodeId,
    /// The request's key (a newer request for the node supersedes it).
    pub key: u64,
    pub text: String,
    /// Frames painted while it was held.
    pub paints: u32,
}

/// (eval-sandbox) A surface's last painted frame as it was drawn: the
/// display list (the scene flattened at the frame's time) and what the
/// frame did with it besides, which `Renderer::repaint_fresh` draws
/// again. Kept apart from the surface's flatten cache, which a frame's
/// end may drop.
#[derive(Debug)]
pub(crate) struct LastFrame {
    pub(crate) items: Vec<crate::flatten::DisplayItem>,
    #[cfg(feature = "gpu")]
    pub(crate) passes: Vec<crate::renderer::backend::PassWant>,
    pub(crate) size: strand_scene::Size,
    pub(crate) scale: strand_scene::Scale,
    /// The crossfade weight it was blended over its snapshot at, if it
    /// was.
    pub(crate) fade: Option<f32>,
}

/// Which texts the hold keeps back (`Renderer::set_text_hold_with`): a
/// layout's node and text, true to hold it.
pub type HoldPick = Box<dyn FnMut(NodeId, &str) -> bool>;

/// The renderer's probe state (`Renderer::probe`).
#[derive(Default)]
pub(crate) struct Probe {
    pub(crate) trace_on: bool,
    pub(crate) frames: Vec<FrameTrace>,
    /// The hold: `None` holds nothing.
    pub(crate) hold: Option<HoldPick>,
    pub(crate) held: Vec<(HeldText, TextLayout)>,
    /// The time the renderer's own timers read in place of the wall
    /// clock (`Renderer::set_now`).
    pub(crate) now: Option<std::time::Instant>,
    /// Layouts inline shaping delivered at once since the last
    /// `Renderer::take_inline_shaped`: each is one a text worker would
    /// have woken the loop for.
    pub(crate) shaped: u32,
    /// Keep each surface's last frame as drawn (`Renderer::set_keep_frames`).
    pub(crate) keep: bool,
    /// Each surface's last frame as drawn, while `keep` is on.
    pub(crate) last: std::collections::HashMap<SurfaceId, LastFrame>,
    /// A mutant for the `redraw` check's own tests
    /// (`Renderer::set_damage_inset`): each frame's own damage shrunk by
    /// this many pixels on every side. 0, the default, is none.
    pub(crate) damage_inset: u32,
}

impl std::fmt::Debug for Probe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Probe")
            .field("trace_on", &self.trace_on)
            .field("frames", &self.frames.len())
            .field("hold", &self.hold.is_some())
            .field("held", &self.held.len())
            .field("now", &self.now)
            .field("shaped", &self.shaped)
            .field("keep", &self.keep)
            .field("last", &self.last.len())
            .field("damage_inset", &self.damage_inset)
            .finish()
    }
}

impl Probe {
    /// Keeps `f`, the frame of `surface` just painted at `size` and
    /// `scale` (blended over its crossfade snapshot at `fade`), as drawn,
    /// when `keep` is on.
    pub(crate) fn keep_frame(
        &mut self,
        surface: SurfaceId,
        f: &crate::flatten::Flattened,
        size: strand_scene::Size,
        scale: strand_scene::Scale,
        fade: Option<f32>,
    ) {
        if !self.keep {
            return;
        }
        self.last.insert(
            surface,
            LastFrame {
                items: f.items.clone(),
                #[cfg(feature = "gpu")]
                passes: f.passes.clone(),
                size,
                scale,
                fade,
            },
        );
    }

    /// A frame of `surface` at `time` painted `f` (`fresh`: flattened
    /// for this paint): its trace, when on. `damage_px` is the frame's
    /// own damage, `layout_passes` those this paint ran, `layout_nodes`
    /// the boxes of the surface's layout, `pending` the text requests in
    /// flight, `scrolling` the scrolls whose offset moves by itself
    /// ([`Motions::SCROLL`]), each with whether it flings
    /// ([`Motions::FLING`]), `period` the frame period its changes
    /// started one of before it and `springs` the samples it took of
    /// springs. Every held layout counts the paint.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn frame(
        &mut self,
        surface: SurfaceId,
        time: Duration,
        backend: Backend,
        f: &crate::flatten::Flattened,
        fresh: bool,
        damage_px: u64,
        layout_passes: u64,
        layout_nodes: usize,
        pending: usize,
        times: PaintTimes,
        fade: Option<f32>,
        scrolling: &[(NodeId, bool)],
        period: Duration,
        springs: Vec<SpringTrace>,
    ) {
        for (h, _) in &mut self.held {
            h.paints += 1;
        }
        if !self.trace_on {
            return;
        }
        let count = |n: u64| u32::try_from(n).unwrap_or(u32::MAX);
        self.frames.push(FrameTrace {
            surface,
            time,
            backend,
            fresh,
            damage_px,
            raster_px: 0,
            items: count(f.items.len() as u64),
            records: count(f.records.len() as u64),
            layout_passes: count(layout_passes),
            layout_nodes: count(layout_nodes as u64),
            text_pending: count(pending as u64),
            text_held: count(self.held.len() as u64),
            times,
            fade,
            nodes: match f.trace.clone() {
                Some(n) if n.iter().any(|t| scrolling.iter().any(|s| s.0 == t.node)) => n
                    .iter()
                    .map(|t| {
                        let mut t = t.clone();
                        if let Some((_, fling)) = scrolling.iter().find(|s| s.0 == t.node) {
                            t.motions |= Motions::SCROLL;
                            if *fling {
                                t.motions |= Motions::FLING;
                            }
                        }
                        t
                    })
                    .collect(),
                Some(n) => n,
                None => Arc::from([]),
            },
            period,
            springs,
        });
    }

    /// The frame of `surface` just traced was rasterised by the CPU:
    /// `px` pixels, in `times`.
    pub(crate) fn rastered(&mut self, surface: SurfaceId, px: u64, times: PaintTimes) {
        if let Some(t) = self.frames.last_mut().filter(|t| t.surface == surface) {
            t.raster_px = px;
            t.times = times;
        }
    }
}
