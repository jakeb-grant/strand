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

use strand_scene::{Backend, LogicalRect, NodeId, NodeKind, SurfaceId};
use strand_text::TextLayout;

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
    /// Every node drawn, or found hidden, in paint order (a parent
    /// before its children). Unplaced nodes and the subtrees of hidden
    /// nodes are absent.
    pub nodes: Arc<[NodeTrace]>,
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
    /// Why nothing of it shows (its subtree is then absent).
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
    /// flight. Every held layout counts the paint.
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
            nodes: f.trace.clone().unwrap_or_else(|| Arc::from([])),
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
