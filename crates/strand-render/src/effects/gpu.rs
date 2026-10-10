//! (M4, `gpu` builds) The bundled GPU effects as the flattener asks for
//! them (design.md, "Bundled GPU effects"; decisions.md, m4-gpu-effects).
//!
//! Each effect is a pass want (`renderer::backend::PassWant`) its node
//! adds to the frame while it is drawn, which is what starts the device
//! (only while visible) and keeps it up; when its node stops being drawn
//! the wants stop, and the device drops 30 s later
//! (`renderer::promote`). Until the GPU's pixels for a want are back, and
//! with no device, the CPU draws its fallback:
//!
//! - `filter: bloom(…) | crt() | chromatic(…) | wobble(…)` and the 3-D
//!   `tilt:` ([`super::with_tilt`]): passes over the node's layer group
//!   (its subtree, CPU-filtered first), whose pixels become the layer's
//!   offscreen group ([`crate::layers::Layer::gpu`]). The fallback: a
//!   glow for bloom, the subtree unfiltered for the rest, the 2-D tilt.
//! - `backdrop: glass()` and a `backdrop: blur(…)` over 0.2 Mpx: passes
//!   over what is drawn behind the box, at full resolution, as the
//!   backdrop layer's group. The fallback: `crate::backdrop`'s quarter-
//!   scale blur (and tint).
//! - `effect aurora` and `particles` above 1,000 alive: the node's own
//!   pass, drawn as its raster. The fallback: the still aurora and the
//!   capped field, with their notices (said only when no GPU can draw
//!   them).

use strand_scene::{NodeKind, Prop, PropValue};

use super::builtin::{Builtin, rotate_hue};
use crate::backdrop::GLASS_REFRACTION;
use crate::clock::Rate;
use crate::tree::Node;

/// How far glass splits red and blue, as a share of its refraction.
const GLASS_DISPERSION: f32 = 0.3;

/// Glass's frost, logical pixels.
const GLASS_FROST: f32 = 1.5;

/// A `backdrop: blur(…)` over a box of at least this many buffer pixels
/// is drawn by the GPU at full resolution (design.md: "large
/// full-resolution backdrop blur, above about 0.2 Mpx").
pub(crate) const LARGE_BLUR: u64 = 200_000;

/// True if `node`'s raw `filter:` has a `wobble(…)`, which moves with
/// time once the GPU draws it.
pub(crate) fn moves(node: &Node) -> bool {
    fn has(v: &PropValue) -> bool {
        match v {
            PropValue::Call { name, .. } => name == "wobble",
            PropValue::List(items) => items.iter().any(has),
            _ => false,
        }
    }
    node.get(Prop::Filter).is_some_and(has)
}

/// The clock of a node the GPU animates and the CPU draws still: an
/// `effect aurora`.
pub(crate) fn rate(node: &Node) -> Option<Rate> {
    let aurora = node.kind == NodeKind::Effect
        && matches!(node.get(Prop::Style),
            Some(PropValue::Keyword(k) | PropValue::Text(k)) if k == "aurora");
    aurora.then_some(Rate::Refresh)
}

/// Glass's pass uniforms on a surface at `scale`: its refraction (buffer
/// pixels, from the effect), its corners' `radius` (buffer pixels), its
/// dispersion and its frost.
pub(crate) fn glass_uniforms(refraction: f32, radius: f32, scale: f32) -> Vec<f32> {
    let refraction = if refraction.is_finite() {
        refraction.max(0.0)
    } else {
        GLASS_REFRACTION * scale
    };
    vec![
        refraction,
        radius.max(0.0),
        GLASS_DISPERSION,
        GLASS_FROST * scale,
    ]
}

/// The aurora pass's uniforms: its three curtains' colours (straight),
/// then its speed.
pub(crate) fn aurora_uniforms(b: &Builtin) -> Vec<f32> {
    let c = b.color;
    let mut out = Vec::with_capacity(16);
    for c in [c, rotate_hue(c, 50.0), rotate_hue(c, -50.0)] {
        out.extend([c.r, c.g, c.b, c.a]);
    }
    out.extend([b.speed.clamp(0.0, 100.0), 0.0, 0.0, 0.0]);
    out
}
