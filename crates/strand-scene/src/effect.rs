//! (M4) Effect layers (design.md, "Runtime changes these need", items
//! 1–4): a tagged group effect render wraps a subtree in. Props keep
//! arriving as [`crate::PropValue::Call`]s (`filter: grayscale(1)`,
//! `mask: fade(bottom, 24)`, `blend: screen`); render builds the
//! [`Effect`]s from them, and each backend lowers them its own way
//! (vello_cpu `push_layer`; masks always on the CPU). An effect's
//! [`Effect::reach`] is how far it spreads damage past the group.

use std::sync::Arc;

use crate::protocol::{Insets, named_enum};
use crate::shader::ShaderCode;
use crate::surface::{Anchor, Edge};

named_enum! {
    /// `blend: screen | add | multiply | overlay | difference`. (`normal`,
    /// which builtin.schema's `enum Blend` also has, is no effect.)
    pub enum BlendMode {
        Screen = "screen",
        Add = "add",
        Multiply = "multiply",
        Overlay = "overlay",
        Difference = "difference",
    }
}

named_enum! {
    /// design.md's bundled GPU effects (eight, CRT and chromatic
    /// aberration being two spellings of one): `filter: bloom(r) | crt()
    /// | chromatic(px) | wobble(amp)`, `backdrop: glass()`, and the GPU
    /// versions of `particles` above 1,000, 3-D `tilt`, `effect aurora`
    /// and large backdrop blur. Their WGSL lives in `strand-gpu`; the
    /// stream that builds each one records its knobs (decisions.md).
    pub enum Bundled {
        Bloom = "bloom",
        Glass = "glass",
        Particles = "particles",
        Tilt = "tilt",
        Wobble = "wobble",
        Crt = "crt",
        Chromatic = "chromatic",
        Aurora = "aurora",
        BackdropBlur = "backdrop_blur",
    }
}

impl Bundled {
    /// How far the pass draws past its bounds, in logical pixels: its
    /// first uniform for `bloom` (the radius), `chromatic` (the offset)
    /// and `wobble` (the amplitude), which the effect's builder puts in
    /// slot 0; the others draw inside their bounds.
    pub fn reach(self, uniforms: &[f32]) -> f32 {
        match self {
            Bundled::Bloom | Bundled::Chromatic | Bundled::Wobble => uniforms
                .first()
                .copied()
                .filter(|v| v.is_finite())
                .map_or(0.0, |v| v.abs()),
            Bundled::Glass
            | Bundled::Particles
            | Bundled::Tilt
            | Bundled::Crt
            | Bundled::Aurora
            | Bundled::BackdropBlur => 0.0,
        }
    }
}

/// Which shader a pass runs.
#[derive(Clone, Debug, PartialEq)]
pub enum ShaderRef {
    /// One of design.md's bundled GPU effects.
    Bundled(Bundled),
    /// A `.wgsl` file's checked code (a `shader` node).
    File(Arc<ShaderCode>),
}

/// What a pass reads as `strand_input`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum ShaderInput {
    /// Nothing (1×1 transparent): a `shader` node draws in its box.
    #[default]
    None,
    /// The subtree's own pixels, from its cached offscreen group: a
    /// `filter:` pass.
    Content,
    /// What is under the node in the surface: `backdrop: glass()`.
    Backdrop,
}

/// A shader run as an effect: a bundled GPU effect or a `.wgsl` file's
/// pass. Render packs `uniforms` each frame from the springing
/// [`crate::Prop::Uniforms`] in the code's slot order.
#[derive(Clone, Debug, PartialEq)]
pub struct ShaderPass {
    pub code: ShaderRef,
    pub uniforms: Arc<[f32]>,
    pub input: ShaderInput,
}

impl ShaderPass {
    /// How far the pass draws past its bounds, in logical pixels (a
    /// file's pass draws inside its box).
    pub fn reach(&self) -> f32 {
        match &self.code {
            ShaderRef::Bundled(b) => b.reach(&self.uniforms),
            ShaderRef::File(_) => 0.0,
        }
    }
}

/// `mask: fade(edge, len) | radial(at, size) | shape(name)`.
#[derive(Clone, Debug, PartialEq)]
pub enum Mask {
    /// Fades the subtree out toward `edge` over its last `len` logical
    /// pixels.
    Fade { edge: Edge, len: f32 },
    /// A radial reveal: opaque within `size` logical pixels of the point
    /// `at` names in the box.
    Radial { at: Anchor, size: f32 },
    /// A shape of the shape library (`cookie`, `burst`, …), by name as
    /// builtin.schema's `enum Shape` spells it.
    Shape(String),
}

/// The identity colour matrix (four rows of five: r, g, b, a, offset).
pub const IDENTITY_MATRIX: [f32; 20] = [
    1.0, 0.0, 0.0, 0.0, 0.0, //
    0.0, 1.0, 0.0, 0.0, 0.0, //
    0.0, 0.0, 1.0, 0.0, 0.0, //
    0.0, 0.0, 0.0, 1.0, 0.0,
];

/// `outer` applied after `inner`: a `filter:` chain composes into one
/// matrix, so `grayscale(1) brightness(0.9)` is one pass.
pub fn compose_matrices(outer: &[f32; 20], inner: &[f32; 20]) -> [f32; 20] {
    let mut out = [0.0; 20];
    for row in 0..4 {
        for col in 0..5 {
            let mut v = if col == 4 { outer[row * 5 + 4] } else { 0.0 };
            for k in 0..4 {
                v += outer[row * 5 + k] * inner[k * 5 + col];
            }
            out[row * 5 + col] = v;
        }
    }
    out
}

/// A group effect.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// The `filter:` colour functions (`grayscale`, `saturate`, `hue`,
    /// `brightness`, `contrast`, `invert`, `tint`) composed into one 4×5
    /// matrix over straight-alpha colour (rows r, g, b, a; the fifth
    /// column an offset).
    ColorMatrix([f32; 20]),
    /// `filter: blur(radius)` (and `backdrop: blur(…)`'s CPU path): a
    /// Gaussian with standard deviation `radius` logical pixels, as CSS's
    /// `blur()`.
    Blur { radius: f32 },
    /// `blend:`.
    Blend(BlendMode),
    /// `mask:`.
    Mask(Mask),
    /// Group opacity, `0..=1`.
    Opacity(f32),
    /// A shader pass.
    Shader(ShaderPass),
}

impl Effect {
    /// How far the effect spreads damage past its group's bounds, in
    /// logical pixels: three standard deviations for a blur (where a
    /// Gaussian has fallen under 1/255), a bundled pass's own reach, and
    /// nothing for colour, blend, mask and opacity.
    pub fn reach(&self) -> Insets {
        let r = match self {
            Effect::Blur { radius } if radius.is_finite() => (3.0 * radius.abs()).ceil(),
            Effect::Shader(pass) => pass.reach(),
            Effect::Blur { .. }
            | Effect::ColorMatrix(_)
            | Effect::Blend(_)
            | Effect::Mask(_)
            | Effect::Opacity(_) => 0.0,
        };
        Insets::all(r)
    }

    /// The reach of a stack of effects, applied in order: each spreads
    /// what the ones inside it reached.
    pub fn reach_of(effects: &[Effect]) -> Insets {
        let total = effects.iter().map(|e| e.reach().top).sum();
        Insets::all(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for b in BlendMode::ALL {
            assert_eq!(BlendMode::from_name(b.name()), Some(*b));
        }
        assert_eq!(BlendMode::from_name("normal"), None);
        for b in Bundled::ALL {
            assert_eq!(Bundled::from_name(b.name()), Some(*b));
        }
        assert_eq!(Bundled::ALL.len(), 9);
    }

    #[test]
    fn reach_grows_damage_by_each_effect() {
        assert_eq!(Effect::Blur { radius: 10.0 }.reach(), Insets::all(30.0));
        assert_eq!(Effect::Blur { radius: 0.4 }.reach(), Insets::all(2.0));
        assert_eq!(Effect::Blur { radius: f32::NAN }.reach(), Insets::all(0.0));
        assert_eq!(Effect::Opacity(0.5).reach(), Insets::default());
        assert_eq!(Effect::Blend(BlendMode::Screen).reach(), Insets::default());
        assert_eq!(
            Effect::ColorMatrix(IDENTITY_MATRIX).reach(),
            Insets::default()
        );
        assert_eq!(
            Effect::Mask(Mask::Fade {
                edge: Edge::Bottom,
                len: 24.0
            })
            .reach(),
            Insets::default()
        );
        let pass = |b, u: &[f32]| {
            Effect::Shader(ShaderPass {
                code: ShaderRef::Bundled(b),
                uniforms: Arc::from(u),
                input: ShaderInput::Content,
            })
        };
        assert_eq!(
            pass(Bundled::Bloom, &[12.0, 0.5]).reach(),
            Insets::all(12.0)
        );
        assert_eq!(pass(Bundled::Chromatic, &[-3.0]).reach(), Insets::all(3.0));
        assert_eq!(pass(Bundled::Wobble, &[]).reach(), Insets::all(0.0));
        assert_eq!(pass(Bundled::Crt, &[9.0]).reach(), Insets::all(0.0));
        let file = Effect::Shader(ShaderPass {
            code: ShaderRef::File(Arc::new(ShaderCode {
                path: "a.wgsl".into(),
                wgsl: String::new(),
                uniforms: vec![],
            })),
            uniforms: Arc::from([5.0f32].as_slice()),
            input: ShaderInput::None,
        });
        assert_eq!(file.reach(), Insets::default());
        assert_eq!(
            Effect::reach_of(&[Effect::Blur { radius: 2.0 }, pass(Bundled::Bloom, &[4.0])]),
            Insets::all(10.0)
        );
        assert_eq!(ShaderInput::default(), ShaderInput::None);
    }

    #[test]
    fn matrices_compose() {
        assert_eq!(
            compose_matrices(&IDENTITY_MATRIX, &IDENTITY_MATRIX),
            IDENTITY_MATRIX
        );
        // brightness(0.5) after an offset of 0.2 on red: r' = 0.5 r + 0.1.
        let mut bright = IDENTITY_MATRIX;
        for i in [0, 6, 12] {
            bright[i] = 0.5;
        }
        let mut offset = IDENTITY_MATRIX;
        offset[4] = 0.2;
        let m = compose_matrices(&bright, &offset);
        assert_eq!(m[0], 0.5);
        assert!((m[4] - 0.1).abs() < 1e-6);
        assert_eq!(compose_matrices(&bright, &IDENTITY_MATRIX), bright);
        assert_eq!(compose_matrices(&IDENTITY_MATRIX, &offset), offset);
    }
}
