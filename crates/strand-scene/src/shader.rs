//! (M4) Shaders as the scene carries them: the checked WGSL of a `shader`
//! node or a `filter:` pass ([`ShaderCode`]), and the prelude Strand
//! prepends to every file ([`PRELUDE`]). The ABI is in
//! docs/architecture.md, "`strand-gpu`".
//!
//! The checker (`strand-compiler`'s `check::shaders`, feature `shaders`)
//! parses and validates `PRELUDE` + the file with naga and fills in the
//! uniform slots; the GPU thread compiles exactly the same text. This
//! crate does no WGSL parsing.

/// What Strand prepends to a shader file before checking and compiling
/// it. `@group(0)` is Strand's: `strand` (`time` in seconds since the
/// node appeared, `scale`, `size` in buffer pixels, `pointer` in buffer
/// pixels relative to the box, or -1 when outside), `strand_input` (a
/// filter's subtree, a glass backdrop, or 1×1 transparent) and
/// `strand_sampler`. The file declares each `u_*` uniform as its own
/// `var<uniform>` in `@group(1)`, at a binding of its choice (one value
/// per binding: [`UniformSlot`]), and one `@fragment` entry taking [`StrandVertex`]
/// (`uv` in 0..1 over the box) and returning a premultiplied
/// `vec4<f32>`. Diagnostics subtract [`PRELUDE_LINES`] from line numbers.
///
/// [`StrandVertex`]: PRELUDE
pub const PRELUDE: &str = "\
struct Strand {
    time: f32,
    scale: f32,
    size: vec2<f32>,
    pointer: vec2<f32>,
}
@group(0) @binding(0) var<uniform> strand: Strand;
@group(0) @binding(1) var strand_input: texture_2d<f32>;
@group(0) @binding(2) var strand_sampler: sampler;
struct StrandVertex {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
}
";

/// How many lines [`PRELUDE`] adds before the file's first line.
pub const PRELUDE_LINES: u32 = 13;

/// The WGSL type of a `u_*` uniform, as reflected by the checker. Values
/// arrive as `f32`s in buffer units: lengths in px × scale, angles in
/// radians, durations in seconds, colours premultiplied linear `vec4`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum UniformType {
    F32,
    Vec2,
    Vec3,
    Vec4,
}

impl UniformType {
    /// How many `f32`s the value has.
    pub const fn floats(self) -> u32 {
        match self {
            UniformType::F32 => 1,
            UniformType::Vec2 => 2,
            UniformType::Vec3 => 3,
            UniformType::Vec4 => 4,
        }
    }

    /// Its WGSL spelling.
    pub const fn wgsl(self) -> &'static str {
        match self {
            UniformType::F32 => "f32",
            UniformType::Vec2 => "vec2<f32>",
            UniformType::Vec3 => "vec3<f32>",
            UniformType::Vec4 => "vec4<f32>",
        }
    }

    /// Looks a WGSL type up by its spelling (`vec4f` and `vec4<f32>`
    /// alike).
    pub fn from_wgsl(ty: &str) -> Option<Self> {
        Some(match ty {
            "f32" => UniformType::F32,
            "vec2<f32>" | "vec2f" => UniformType::Vec2,
            "vec3<f32>" | "vec3f" => UniformType::Vec3,
            "vec4<f32>" | "vec4f" => UniformType::Vec4,
            _ => return None,
        })
    }
}

/// One `u_*` uniform of a file: its own `var<uniform>` at
/// `@group(1) @binding(binding)`.
///
/// There is no packed WGSL block: each binding is its own buffer
/// binding, which the GPU thread fills from `ShaderPass::uniforms` at
/// the device's uniform-offset alignment. `offset` only says where the
/// slot's `ty.floats()` values sit in that dense `f32` array: slots in
/// binding order, each starting where the previous one ends (no padding),
/// as [`ShaderCode::packed`] lays them out.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UniformSlot {
    pub name: String,
    pub ty: UniformType,
    /// The `@binding(n)` the file declared it at, in `@group(1)`.
    pub binding: u32,
    /// Its first `f32` in `ShaderPass::uniforms`.
    pub offset: u32,
}

/// A shader file after the checker accepted it: what render runs, so it
/// never re-reads a file that may have changed since (a save that fails
/// the check keeps the last good build).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ShaderCode {
    /// The file as written in source (`"aurora.wgsl"`), for diagnostics
    /// and the inspector.
    pub path: String,
    /// The file's text, without [`PRELUDE`].
    pub wgsl: String,
    /// Its `u_*` uniforms in binding order, `offset`s dense (see
    /// [`UniformSlot`]).
    pub uniforms: Vec<UniformSlot>,
}

impl ShaderCode {
    /// The full module the GPU compiles: [`PRELUDE`] then the file.
    pub fn module(&self) -> String {
        let mut s = String::with_capacity(PRELUDE.len() + self.wgsl.len());
        s.push_str(PRELUDE);
        s.push_str(&self.wgsl);
        s
    }

    /// Slots for `(name, ty, binding)` as reflected, sorted by binding
    /// with dense offsets: what the checker stores in
    /// [`ShaderCode::uniforms`].
    pub fn packed(mut uniforms: Vec<(String, UniformType, u32)>) -> Vec<UniformSlot> {
        uniforms.sort_by_key(|u| u.2);
        let mut offset = 0;
        uniforms
            .into_iter()
            .map(|(name, ty, binding)| {
                let slot = UniformSlot {
                    name,
                    ty,
                    binding,
                    offset,
                };
                offset += ty.floats();
                slot
            })
            .collect()
    }

    /// How many `f32`s `ShaderPass::uniforms` holds (the last slot's
    /// end).
    pub fn uniform_floats(&self) -> u32 {
        self.uniforms
            .iter()
            .map(|u| u.offset + u.ty.floats())
            .max()
            .unwrap_or(0)
    }

    /// The slot of a uniform by name.
    pub fn uniform(&self, name: &str) -> Option<&UniformSlot> {
        self.uniforms.iter().find(|u| u.name == name)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{Prop, PropValue};

    #[test]
    fn the_prelude_declares_the_abi() {
        assert_eq!(PRELUDE.lines().count() as u32, PRELUDE_LINES);
        assert!(PRELUDE.ends_with('\n'));
        for name in [
            "struct Strand",
            "time: f32",
            "scale: f32",
            "size: vec2<f32>",
            "pointer: vec2<f32>",
            "@group(0) @binding(0) var<uniform> strand: Strand",
            "var strand_input: texture_2d<f32>",
            "var strand_sampler: sampler",
            "struct StrandVertex",
            "uv: vec2<f32>",
        ] {
            assert!(PRELUDE.contains(name), "{name}");
        }
        assert!(!PRELUDE.contains("@group(1)"), "group 1 is the file's");
    }

    #[test]
    fn uniform_types_round_trip() {
        for ty in [
            UniformType::F32,
            UniformType::Vec2,
            UniformType::Vec3,
            UniformType::Vec4,
        ] {
            assert_eq!(UniformType::from_wgsl(ty.wgsl()), Some(ty));
        }
        assert_eq!(UniformType::from_wgsl("vec4f"), Some(UniformType::Vec4));
        assert_eq!(UniformType::from_wgsl("mat4x4<f32>"), None);
    }

    #[test]
    fn checked_code_travels_shared() {
        let code = Arc::new(ShaderCode {
            path: "aurora.wgsl".into(),
            wgsl: "@group(1) @binding(0) var<uniform> u_speed: f32;\n\
                   @group(1) @binding(3) var<uniform> u_tint: vec4<f32>;\n\
                   @group(1) @binding(1) var<uniform> u_dir: vec2<f32>;\n"
                .into(),
            // As reflected, in declaration order.
            uniforms: ShaderCode::packed(vec![
                ("u_speed".into(), UniformType::F32, 0),
                ("u_tint".into(), UniformType::Vec4, 3),
                ("u_dir".into(), UniformType::Vec2, 1),
            ]),
        });
        assert!(code.module().starts_with(PRELUDE));
        assert!(code.module().ends_with(&code.wgsl));
        // Binding order, offsets dense: no WGSL block padding, since each
        // binding is its own buffer binding.
        let layout: Vec<_> = code
            .uniforms
            .iter()
            .map(|u| (u.name.as_str(), u.binding, u.offset))
            .collect();
        assert_eq!(
            layout,
            [("u_speed", 0, 0), ("u_dir", 1, 1), ("u_tint", 3, 3)]
        );
        assert_eq!(code.uniform_floats(), 7);
        assert_eq!(code.uniform("u_tint").map(|u| u.offset), Some(3));
        assert_eq!(code.uniform("u_nope"), None);
        let v = PropValue::Shader(code.clone());
        assert_eq!(v, PropValue::Shader(Arc::new((*code).clone())));
        assert!(!v.has_tokens());
        assert_eq!(Prop::Shader.name(), "shader");
        assert_eq!(Prop::from_name("uniforms"), Some(Prop::Uniforms));
    }
}
