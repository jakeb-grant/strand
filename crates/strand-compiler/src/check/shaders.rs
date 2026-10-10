//! `shader "x.wgsl" { u_speed: 0.4 }` checked against its file (design.md:
//! "Shaders … checked with naga, hot-reloaded"; architecture.md,
//! "strand-compiler", M4 additions, and "`strand-gpu`", the ABI).
//!
//! The file is read by the caller ([`check`]'s `read`: the loader, `strand
//! check`), so this pass touches no disk itself, and checked with naga as
//! [`PRELUDE`] followed by the file: it parses and validates, has exactly
//! one `@fragment` entry and no other, declares nothing in `@group(0)`
//! (Strand's), and each `var<uniform>` in `@group(1)` is named `u_*` and
//! is `f32` or `vec2`–`vec4<f32>`. Every `u_*` prop of the node must name
//! one of those uniforms (an error with a did-you-mean) with a value of
//! its type (a number, length, angle or duration for `f32`, a colour for
//! `vec4`, comma values of the vector's width); a uniform no prop sets is
//! a warning and zero-filled.
//!
//! WGSL problems are reported on the node's path, with the file's own
//! line and column (the prelude's lines subtracted) in the message: the
//! `.wgsl` file is not one of the config's sources, so a diagnostic
//! cannot point into it, and the module that names it is the one held
//! back.
//!
//! Without the `shaders` feature (the CPU-only build) nothing is parsed:
//! the file is still read, its [`ShaderCode`] carries no slots and the
//! node draws nothing, and one warning per node says why.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(feature = "shaders")]
use strand_scene::shader::{PRELUDE, PRELUDE_LINES};
use strand_scene::shader::{ShaderCode, UniformType};

use crate::diagnostic::{Diagnostic, closest};
use crate::hir::{self, ExprKind, Node};
use crate::source::FileId;
use crate::syntax::Span;
use crate::ty::{Prim, Ty};

/// The checked shaders of a program, by path as written.
pub type Shaders = BTreeMap<String, Arc<ShaderCode>>;

/// Reads a shader file by its path as written (`"aurora.wgsl"`).
pub type Read<'a> = dyn Fn(&str) -> Result<String, String> + 'a;

/// The name of the vertex entry the GPU thread appends.
pub const VERTEX_ENTRY: &str = "strand_vertex_main";

/// Where a shader path is read from: `~/` is the home directory, a
/// relative path is under the config directory.
pub fn resolve(path: &str, config_dir: &Path) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    let p = PathBuf::from(path);
    if p.is_relative() {
        config_dir.join(p)
    } else {
        p
    }
}

/// Shader files larger than this are refused: a fragment shader is a few
/// kilobytes, and the cap keeps a path such as `/dev/zero` from
/// exhausting memory.
pub const MAX_SHADER_BYTES: u64 = 1024 * 1024;

/// Reads a resolved shader file for [`check`]: a regular file (following
/// symlinks) of at most [`MAX_SHADER_BYTES`], as UTF-8. A FIFO or device
/// is refused before it is opened, so it cannot block the caller (the
/// compiler worker, the LSP, `strand check`). The loader, the LSP and
/// `strand check` all read through this (m4-audit).
pub fn read_file(path: &Path) -> Result<String, String> {
    use std::io::Read as _;
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    if meta.len() > MAX_SHADER_BYTES {
        return Err(format!(
            "{} bytes is larger than {MAX_SHADER_BYTES}",
            meta.len()
        ));
    }
    let f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    // Bounded again: the file may grow between the stat and the read.
    f.take(MAX_SHADER_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_SHADER_BYTES {
        return Err(format!("larger than {MAX_SHADER_BYTES} bytes"));
    }
    String::from_utf8(bytes).map_err(|_| "not UTF-8".to_string())
}

/// Does `program` have a `shader` node at all (so a caller can skip the
/// reads)?
pub fn any(program: &hir::Program) -> bool {
    let mut found = false;
    each_shader(program, &mut |_, _| found = true);
    found
}

/// Checks every `shader` node of `program` against its file, read with
/// `read`. Returns the diagnostics and the checked code of every file
/// that passed, by path as written.
pub fn check(program: &hir::Program, read: &Read<'_>) -> (Vec<Diagnostic>, Shaders) {
    let mut diags = Vec::new();
    // Each file once: its code, or the problem every node naming it gets.
    let mut files: BTreeMap<String, Result<Arc<ShaderCode>, Problem>> = BTreeMap::new();
    each_shader(program, &mut |file, e| {
        let Some(arg) = &e.arg else {
            return;
        };
        let path = match &arg.kind {
            ExprKind::Text(p) => p.clone(),
            ExprKind::Error => return,
            _ => {
                diags.push(
                    Diagnostic::error(
                        "check::shader_path",
                        "a shader's file is named by a plain string",
                    )
                    .with_label_in(file, arg.span, "not a literal path")
                    .with_help("write the path as text: `shader \"aurora.wgsl\" { … }`"),
                );
                return;
            }
        };
        let code = files
            .entry(path.clone())
            .or_insert_with(|| load(&path, read))
            .clone();
        match code {
            Ok(code) => node(file, e, arg.span, &code, &mut diags),
            Err(p) => {
                let mut d =
                    Diagnostic::error(p.code, p.message).with_label_in(file, arg.span, p.label);
                d.help = p.help;
                diags.push(d);
            }
        }
    });
    let shaders = files
        .into_iter()
        .filter_map(|(p, c)| Some((p, c.ok()?)))
        .collect();
    (diags, shaders)
}

/// What is wrong with a file, for every node that names it.
#[derive(Clone, Debug)]
struct Problem {
    code: &'static str,
    message: String,
    label: String,
    help: Option<String>,
}

fn load(path: &str, read: &Read<'_>) -> Result<Arc<ShaderCode>, Problem> {
    let wgsl = read(path).map_err(|e| Problem {
        code: "check::shader_file",
        message: format!("cannot read `{path}`: {e}"),
        label: "this shader file".into(),
        help: Some("a relative path is under the config directory".into()),
    })?;
    let uniforms = reflect(path, &wgsl)?;
    Ok(Arc::new(ShaderCode {
        path: path.to_string(),
        wgsl,
        uniforms: ShaderCode::packed(uniforms),
    }))
}

/// Every `shader` element of the program, with its file.
fn each_shader(program: &hir::Program, f: &mut dyn FnMut(FileId, &hir::Element)) {
    for file in &program.files {
        for item in &file.items {
            match item {
                hir::Item::Component(c) => nodes(file.file, &c.body, f),
                hir::Item::Surface(s) => element(file.file, &s.element, f),
                _ => {}
            }
        }
    }
}

fn nodes(file: FileId, ns: &[Node], f: &mut dyn FnMut(FileId, &hir::Element)) {
    for n in ns {
        match n {
            Node::Element(e) => element(file, e, f),
            Node::If(i) => {
                nodes(file, &i.then, f);
                nodes(file, &i.else_, f);
            }
            Node::For(l) => nodes(file, &l.body, f),
            Node::Match(m) => {
                for (_, arm) in &m.arms {
                    nodes(file, arm, f);
                }
            }
            _ => {}
        }
    }
}

fn element(file: FileId, e: &hir::Element, f: &mut dyn FnMut(FileId, &hir::Element)) {
    if matches!(&e.kind, hir::ElementKind::Builtin(k) if k == "shader") {
        f(file, e);
    }
    nodes(file, &e.children, f);
}

/// The uniform type a prop's value fills, if it is one a uniform takes.
pub fn value_type(e: &hir::Expr) -> Option<UniformType> {
    if let ExprKind::Commas(items) = &e.kind {
        return vector(items.len());
    }
    let ty = match &e.ty {
        Ty::Optional(t) => t,
        t => t,
    };
    match ty {
        Ty::Prim(Prim::Color) => Some(UniformType::Vec4),
        Ty::Prim(
            Prim::Float | Prim::Int | Prim::Length | Prim::Percent | Prim::Angle | Prim::Duration,
        ) => Some(UniformType::F32),
        Ty::Tuple(parts) => vector(parts.len()),
        _ => None,
    }
}

fn vector(n: usize) -> Option<UniformType> {
    match n {
        2 => Some(UniformType::Vec2),
        3 => Some(UniformType::Vec3),
        4 => Some(UniformType::Vec4),
        _ => None,
    }
}

/// What a value of `ty` is written as.
fn spelled(ty: UniformType) -> &'static str {
    match ty {
        UniformType::F32 => "a number, length, angle or duration",
        UniformType::Vec2 => "two comma values (`1, 0`)",
        UniformType::Vec3 => "three comma values (`1, 0, 0`)",
        UniformType::Vec4 => "a colour or four comma values",
    }
}

/// One node's `u_*` props (its own and its `when` blocks') against the
/// file's uniforms.
fn node(file: FileId, e: &hir::Element, at: Span, code: &ShaderCode, diags: &mut Vec<Diagnostic>) {
    let whens = e.children.iter().filter_map(|n| match n {
        Node::When(w) => Some(&w.props),
        _ => None,
    });
    let props: Vec<&hir::Prop> = e
        .props
        .iter()
        .chain(whens.flatten())
        .filter(|p| p.name.starts_with("u_"))
        .collect();
    if !cfg!(feature = "shaders") {
        diags.push(
            Diagnostic::warning(
                "check::shader_unchecked",
                format!(
                    "`{}` is not checked and draws nothing: built without the GPU backend",
                    code.path
                ),
            )
            .with_label_in(file, at, "this shader"),
        );
        return;
    }
    let names: Vec<&str> = code.uniforms.iter().map(|u| u.name.as_str()).collect();
    for p in &props {
        let Some(slot) = code.uniform(&p.name) else {
            let mut d = Diagnostic::error(
                "check::unknown_uniform",
                format!("`{}` declares no uniform `{}`", code.path, p.name),
            )
            .with_label_in(file, p.span, "not in the shader file");
            let near = closest(&p.name, names.iter().copied());
            d.help = Some(match (&near, names.is_empty()) {
                (Some(n), _) => format!("did you mean `{n}`?"),
                (None, true) => format!(
                    "declare it in the file: `@group(1) @binding(0) var<uniform> {}: f32;`",
                    p.name
                ),
                (None, false) => format!("its uniforms are {}", list(&names)),
            });
            diags.push(d);
            continue;
        };
        let Some(given) = value_type(&p.value) else {
            // Not a uniform value at all: the type check reported it.
            continue;
        };
        // A colour is a premultiplied `vec4`; any other `vec4` value is
        // four comma values.
        if given != slot.ty {
            diags.push(
                Diagnostic::error(
                    "check::type_mismatch",
                    format!(
                        "uniform `{}` is a `{}`: it takes {}",
                        p.name,
                        slot.ty.wgsl(),
                        spelled(slot.ty)
                    ),
                )
                .with_label_in(
                    file,
                    p.value.span,
                    format!("{} here", spelled(given)),
                ),
            );
        }
    }
    for u in &code.uniforms {
        if !props.iter().any(|p| p.name == u.name) {
            diags.push(
                Diagnostic::warning(
                    "check::unset_uniform",
                    format!(
                        "uniform `{}` of `{}` is not set: it reads zero",
                        u.name, code.path
                    ),
                )
                .with_label_in(file, at, "this shader")
                .with_help(format!("set it with `{}: …`", u.name)),
            );
        }
    }
}

fn list(names: &[&str]) -> String {
    names
        .iter()
        .map(|n| format!("`{n}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The most bytes of private (and workgroup) globals plus every
/// function's local variables a shader may declare (decisions.md,
/// m4-audit): far above a real effect's few dozen floats, far below what
/// a driver thread's stack holds.
#[cfg(feature = "shaders")]
const MAX_PIXEL_MEMORY: u64 = 16 * 1024;

/// Without naga: no slots (the node draws nothing).
#[cfg(not(feature = "shaders"))]
fn reflect(_path: &str, _wgsl: &str) -> Result<Vec<(String, UniformType, u32)>, Problem> {
    Ok(Vec::new())
}

/// Parses and validates `PRELUDE` + the file and reflects its uniforms.
#[cfg(feature = "shaders")]
fn reflect(path: &str, wgsl: &str) -> Result<Vec<(String, UniformType, u32)>, Problem> {
    use naga::{AddressSpace, ScalarKind, ShaderStage, TypeInner, VectorSize};

    let bad = |message: String, help: Option<String>| Problem {
        code: "check::shader",
        message,
        label: "this shader file".into(),
        help,
    };
    let source = format!("{PRELUDE}{wgsl}");
    // `file:line:col` in the file, the prelude's lines subtracted.
    let at = |loc: Option<naga::SourceLocation>| match loc {
        Some(l) if l.line_number > PRELUDE_LINES => format!(
            "{path}:{}:{}",
            l.line_number - PRELUDE_LINES,
            l.line_position
        ),
        Some(_) => format!("{path} (in Strand's prelude)"),
        None => path.to_string(),
    };
    let module = naga::front::wgsl::parse_str(&source).map_err(|e| {
        bad(
            format!("{}: {}", at(e.location(&source)), e.message()),
            e.labels()
                .filter(|(_, l)| !l.is_empty())
                .map(|(_, l)| l.to_string())
                .next(),
        )
    })?;
    let info = match naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::default(),
    )
    .validate(&module)
    {
        Ok(info) => info,
        Err(e) => {
            let mut message = e.as_inner().to_string();
            let mut src = std::error::Error::source(e.as_inner());
            while let Some(s) = src {
                message.push_str(": ");
                message.push_str(&s.to_string());
                src = s.source();
            }
            return Err(bad(format!("{}: {message}", at(e.location(&source))), None));
        }
    };
    // Every pixel gets its own copy of the private and function
    // variables, inside the strand process: a driver that cannot give
    // them (lavapipe puts them on its threads' stacks) kills the process,
    // past any error scope, and under a lock that leaves the session
    // locked with no client. naga bounds a type only at 1 GiB.
    let gctx = module.to_ctx();
    let bytes = |ty: naga::Handle<naga::Type>| {
        module.types[ty]
            .inner
            .try_size(gctx)
            .map_or(u64::MAX, u64::from)
    };
    let private = module
        .global_variables
        .iter()
        .filter(|(_, g)| matches!(g.space, AddressSpace::Private | AddressSpace::WorkGroup))
        .fold(0u64, |n, (_, g)| n.saturating_add(bytes(g.ty)));
    let functions = module.functions.iter().map(|(h, f)| (f, &info[h])).chain(
        module
            .entry_points
            .iter()
            .enumerate()
            .map(|(i, e)| (&e.function, info.get_entry_point(i))),
    );
    let mut local = 0u64;
    for (f, fi) in functions {
        local = f
            .local_variables
            .iter()
            .fold(local, |n, (_, v)| n.saturating_add(bytes(v.ty)));
        // (m4-audit) A by-value array or matrix indexed at run time (a
        // `let`, a `const`, a parameter or a call's result) is copied
        // into a function variable of its whole size by the SPIR-V
        // backend, though no `var` names it; so is a composite passed to
        // or returned from a function.
        let composite = |inner: &naga::TypeInner| {
            matches!(
                inner,
                naga::TypeInner::Array { .. } | naga::TypeInner::Matrix { .. }
            )
        };
        let mut spilled = std::collections::HashSet::new();
        for (_, e) in f.expressions.iter() {
            if let naga::Expression::Access { base, .. } = *e {
                let inner = fi[base].ty.inner_with(&module.types);
                if composite(inner) && spilled.insert(base) {
                    local = local.saturating_add(inner.try_size(gctx).map_or(u64::MAX, u64::from));
                }
            }
        }
        for ty in f
            .arguments
            .iter()
            .map(|a| a.ty)
            .chain(f.result.as_ref().map(|r| r.ty))
        {
            if matches!(module.types[ty].inner, naga::TypeInner::Array { .. }) {
                local = local.saturating_add(bytes(ty));
            }
        }
    }
    let memory = private.saturating_add(local);
    if memory > MAX_PIXEL_MEMORY {
        return Err(bad(
            format!(
                "{path}: the shader's private and function variables take {} bytes per pixel, \
                 over the {MAX_PIXEL_MEMORY}-byte limit",
                if memory == u64::MAX {
                    "too many".to_string()
                } else {
                    memory.to_string()
                }
            ),
            Some(
                "every pixel gets its own copy: pass large data as uniforms or compute it \
                 instead of storing it"
                    .into(),
            ),
        ));
    }
    let fragments = module
        .entry_points
        .iter()
        .filter(|e| e.stage == ShaderStage::Fragment)
        .count();
    if let Some(other) = module
        .entry_points
        .iter()
        .find(|e| e.stage != ShaderStage::Fragment)
    {
        return Err(bad(
            format!(
                "{path}: `{}` is a {:?} entry: a shader file has only its `@fragment` entry",
                other.name, other.stage
            ),
            Some("Strand supplies the vertex stage over the node's box".into()),
        ));
    }
    if fragments != 1 {
        return Err(bad(
            format!(
                "{path}: a shader file has exactly one `@fragment` entry, this one has {fragments}"
            ),
            Some(
                "write `@fragment fn main(v: StrandVertex) -> @location(0) vec4<f32> { … }`".into(),
            ),
        ));
    }
    if module
        .functions
        .iter()
        .any(|(_, f)| f.name.as_deref() == Some(VERTEX_ENTRY))
        || module.entry_points.iter().any(|e| e.name == VERTEX_ENTRY)
    {
        return Err(bad(
            format!("{path}: `{VERTEX_ENTRY}` is Strand's vertex entry"),
            Some("rename the function".into()),
        ));
    }
    let mut uniforms = Vec::new();
    for (_, g) in module.global_variables.iter() {
        let name = g.name.clone().unwrap_or_default();
        let Some(b) = &g.binding else {
            continue;
        };
        match b.group {
            0 => {
                if !matches!(name.as_str(), "strand" | "strand_input" | "strand_sampler") {
                    return Err(bad(
                        format!("{path}: `{name}` is in `@group(0)`, which is Strand's"),
                        Some("declare the file's uniforms in `@group(1)`".into()),
                    ));
                }
            }
            1 => {
                if g.space != AddressSpace::Uniform {
                    return Err(bad(
                        format!("{path}: `{name}` in `@group(1)` is not a `var<uniform>`"),
                        Some("`@group(1)` holds only `u_*` uniforms; Strand gives the input texture as `strand_input`".into()),
                    ));
                }
                if !name.starts_with("u_") {
                    return Err(bad(
                        format!("{path}: uniform `{name}` is not named `u_…`"),
                        Some(format!(
                            "rename it `u_{name}`, and set it with `u_{name}: …`"
                        )),
                    ));
                }
                let ty = match &module.types[g.ty].inner {
                    TypeInner::Scalar(s) if s.kind == ScalarKind::Float && s.width == 4 => {
                        Some(UniformType::F32)
                    }
                    TypeInner::Vector { size, scalar }
                        if scalar.kind == ScalarKind::Float && scalar.width == 4 =>
                    {
                        Some(match size {
                            VectorSize::Bi => UniformType::Vec2,
                            VectorSize::Tri => UniformType::Vec3,
                            VectorSize::Quad => UniformType::Vec4,
                        })
                    }
                    _ => None,
                };
                let Some(ty) = ty else {
                    return Err(bad(
                        format!("{path}: uniform `{name}` is not `f32` or `vec2`–`vec4<f32>`"),
                        Some("pass more data as several uniforms".into()),
                    ));
                };
                uniforms.push((name, ty, b.binding));
            }
            g => {
                return Err(bad(
                    format!(
                        "{path}: `{name}` is in `@group({g})`: a file's uniforms are in `@group(1)`"
                    ),
                    None,
                ));
            }
        }
    }
    Ok(uniforms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SourceMap;

    #[cfg_attr(not(feature = "shaders"), allow(dead_code))]
    const AURORA: &str = "\
@group(1) @binding(0) var<uniform> u_speed: f32;
@group(1) @binding(2) var<uniform> u_tint: vec4<f32>;
@group(1) @binding(1) var<uniform> u_dir: vec2<f32>;
@fragment
fn main(v: StrandVertex) -> @location(0) vec4<f32> {
    let t = strand.time * u_speed;
    return u_tint * (0.5 + 0.5 * sin(t + v.uv.x * u_dir.x));
}
";

    fn run(src: &str, files: &[(&str, &str)]) -> (Vec<Diagnostic>, Shaders) {
        let mut map = SourceMap::new();
        map.add("a.strand", src.to_string());
        let c = crate::compile(&map);
        let errors: Vec<_> = c.diagnostics.iter().filter(|d| d.is_error()).collect();
        assert!(errors.is_empty(), "{errors:?}");
        let files: BTreeMap<String, String> = files
            .iter()
            .map(|(p, t)| (p.to_string(), t.to_string()))
            .collect();
        check(&c.program, &|p| {
            files.get(p).cloned().ok_or_else(|| "no such file".into())
        })
    }

    fn codes(d: &[Diagnostic]) -> Vec<(&str, &str)> {
        d.iter().map(|d| (d.code, d.message.as_str())).collect()
    }

    #[cfg_attr(not(feature = "shaders"), allow(dead_code))]
    const BAR: &str =
        "bar Top { edge: top; height: 30\n  shader \"aurora.wgsl\" { %%PROPS%% }\n}\n";

    #[cfg_attr(not(feature = "shaders"), allow(dead_code))]
    fn bar(props: &str) -> String {
        BAR.replace("%%PROPS%%", props)
    }

    /// Each `u_*` prop matches a uniform naga reflects, by name and type;
    /// the code keeps the file and its slots in binding order.
    #[cfg(feature = "shaders")]
    #[test]
    fn uniforms_match_the_file_by_name_and_type() {
        let (d, shaders) = run(
            &bar("u_speed: 0.4; u_tint: #ff8800; u_dir: 1, 0"),
            &[("aurora.wgsl", AURORA)],
        );
        assert!(d.is_empty(), "{:?}", codes(&d));
        let code = &shaders["aurora.wgsl"];
        assert_eq!(code.wgsl, AURORA);
        let slots: Vec<_> = code
            .uniforms
            .iter()
            .map(|u| (u.name.as_str(), u.ty, u.binding, u.offset))
            .collect();
        assert_eq!(
            slots,
            [
                ("u_speed", UniformType::F32, 0, 0),
                ("u_dir", UniformType::Vec2, 1, 1),
                ("u_tint", UniformType::Vec4, 2, 3),
            ]
        );
        // Lengths, angles and durations are `f32`s too.
        for v in ["12px", "90deg", "300ms", "2"] {
            let (d, _) = run(
                &bar(&format!("u_speed: {v}; u_tint: #fff; u_dir: 0, 1")),
                &[("aurora.wgsl", AURORA)],
            );
            assert!(d.is_empty(), "{v}: {:?}", codes(&d));
        }
    }

    #[cfg(feature = "shaders")]
    #[test]
    fn a_prop_the_file_lacks_is_an_error_with_a_did_you_mean() {
        let (d, _) = run(
            &bar("u_sped: 0.4; u_tint: #fff; u_dir: 1, 0"),
            &[("aurora.wgsl", AURORA)],
        );
        let c = codes(&d);
        assert!(
            c.contains(&(
                "check::unknown_uniform",
                "`aurora.wgsl` declares no uniform `u_sped`"
            )),
            "{c:?}"
        );
        let e = d
            .iter()
            .find(|d| d.code == "check::unknown_uniform")
            .unwrap();
        assert!(e.is_error());
        assert_eq!(e.help.as_deref(), Some("did you mean `u_speed`?"));
        // The uniform nothing sets is a warning: it reads zero.
        let w = d.iter().find(|d| d.code == "check::unset_uniform").unwrap();
        assert!(!w.is_error());
        assert!(w.message.contains("`u_speed`"), "{}", w.message);
    }

    #[cfg(feature = "shaders")]
    #[test]
    fn a_value_of_the_wrong_type_is_an_error() {
        for (props, msg) in [
            (
                "u_speed: #fff; u_tint: #fff; u_dir: 1, 0",
                "uniform `u_speed` is a `f32`: it takes a number, length, angle or duration",
            ),
            (
                "u_speed: 1; u_tint: #fff; u_dir: 1, 0, 0",
                "uniform `u_dir` is a `vec2<f32>`: it takes two comma values (`1, 0`)",
            ),
            (
                "u_speed: 1; u_tint: 0.5; u_dir: 1, 0",
                "uniform `u_tint` is a `vec4<f32>`: it takes a colour or four comma values",
            ),
        ] {
            let (d, _) = run(&bar(props), &[("aurora.wgsl", AURORA)]);
            assert_eq!(codes(&d), [("check::type_mismatch", msg)], "{props}");
        }
        // Four comma values fill a `vec4` as a colour does; a `when`
        // block's props are checked too.
        let src = "state on = false\nbar Top { edge: top; height: 30\n  shader \"aurora.wgsl\" { u_speed: 1; u_tint: 1, 0, 0, 1; u_dir: 1, 0\n    when on { u_dir: 1 }\n  }\n}\n";
        let (d, _) = run(src, &[("aurora.wgsl", AURORA)]);
        assert_eq!(codes(&d).len(), 1, "{:?}", codes(&d));
        assert_eq!(d[0].code, "check::type_mismatch");
    }

    /// WGSL errors point into the file, the prelude's lines subtracted.
    #[cfg(feature = "shaders")]
    #[test]
    fn broken_files_are_errors_with_their_own_lines() {
        let cases = [
            (
                "@fragment\nfn main(v: StrandVertex) -> @location(0) vec4<f32> {\n    return vec4<f32>(nope);\n}\n",
                "aurora.wgsl:3:",
            ),
            (
                "fn helper() {}\n",
                "exactly one `@fragment` entry, this one has 0",
            ),
            (
                "@vertex fn v() -> @builtin(position) vec4<f32> { return vec4<f32>(); }\n@fragment fn main() -> @location(0) vec4<f32> { return vec4<f32>(); }\n",
                "is a Vertex entry",
            ),
            (
                "@group(1) @binding(0) var<uniform> speed: f32;\n@fragment fn main() -> @location(0) vec4<f32> { return vec4<f32>(speed); }\n",
                "uniform `speed` is not named `u_…`",
            ),
            (
                "@group(1) @binding(0) var<uniform> u_m: mat4x4<f32>;\n@fragment fn main() -> @location(0) vec4<f32> { return u_m[0]; }\n",
                "is not `f32` or `vec2`–`vec4<f32>`",
            ),
            (
                "@group(0) @binding(3) var<uniform> u_x: f32;\n@fragment fn main() -> @location(0) vec4<f32> { return vec4<f32>(u_x); }\n",
                "which is Strand's",
            ),
            (
                "fn strand_vertex_main() {}\n@fragment fn main() -> @location(0) vec4<f32> { return vec4<f32>(); }\n",
                "is Strand's vertex entry",
            ),
        ];
        for (wgsl, msg) in cases {
            let (d, shaders) = run(&bar(""), &[("aurora.wgsl", wgsl)]);
            assert!(shaders.is_empty());
            assert_eq!(d.len(), 1, "{wgsl}: {:?}", codes(&d));
            assert_eq!(d[0].code, "check::shader");
            assert!(d[0].is_error());
            assert!(d[0].message.contains(msg), "{wgsl}: {}", d[0].message);
        }
        // An unreadable file.
        let (d, _) = run(&bar(""), &[]);
        assert_eq!(
            codes(&d),
            [(
                "check::shader_file",
                "cannot read `aurora.wgsl`: no such file"
            )]
        );
    }

    /// A shader whose private or function variables would take more
    /// than `MAX_PIXEL_MEMORY` per pixel is refused before it reaches the
    /// GPU thread (a driver that cannot give every pixel its copy kills
    /// the process), including by-value arrays indexed at run time, which
    /// the backend copies into a variable; small arrays pass.
    #[cfg(feature = "shaders")]
    #[test]
    fn huge_private_and_local_arrays_are_refused() {
        let main = "@fragment fn main() -> @location(0) vec4<f32>";
        let pos = "@fragment fn main(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32>";
        let refused = [
            format!("var<private> a: array<vec4<f32>, 50000000>;\n{main} {{ return a[0]; }}\n"),
            format!("{main} {{ var a: array<vec4<f32>, 4096>; return a[1]; }}\n"),
            // Split across a helper and the entry, under the limit each.
            format!(
                "fn h() -> f32 {{ var a: array<f32, 3000>; return a[2]; }}\n\
                 {main} {{ var b: array<f32, 3000>; return vec4<f32>(h() + b[1]); }}\n"
            ),
            format!(
                "struct S {{ x: array<vec4<f32>, 2000> }}\nvar<private> s: S;\n\
                 {main} {{ return s.x[0]; }}\n"
            ),
            // (m4-audit) No `var`: a by-value array indexed at run time is
            // copied whole into a function variable by the SPIR-V backend.
            format!("{pos} {{ let a = array<vec4<f32>, 50000000>(); return a[u32(p.x)]; }}\n"),
            format!("{pos} {{ let a = array<vec4<f32>, 1048576>(); return a[u32(p.x)]; }}\n"),
            format!("const K = array<vec4<f32>, 2000>();\n{pos} {{ return K[u32(p.x)]; }}\n"),
            format!(
                "fn h(a: array<vec4<f32>, 2000>, i: u32) -> vec4<f32> {{ return a[i]; }}\n\
                 {pos} {{ return h(array<vec4<f32>, 2000>(), u32(p.x)); }}\n"
            ),
        ];
        for wgsl in &refused {
            let (d, shaders) = run(&bar(""), &[("aurora.wgsl", wgsl)]);
            assert!(shaders.is_empty(), "{wgsl}");
            assert_eq!(codes(&d).len(), 1, "{wgsl}: {:?}", codes(&d));
            assert!(
                d[0].message.contains("bytes per pixel"),
                "{wgsl}: {}",
                d[0].message
            );
        }
        let fine = format!(
            "var<private> p: array<vec4<f32>, 64>;\n\
             {main} {{ var a: array<f32, 256>; return p[0] + vec4<f32>(a[0]); }}\n"
        );
        let (d, _) = run(&bar(""), &[("aurora.wgsl", &fine)]);
        assert!(d.is_empty(), "{:?}", codes(&d));
        // A small by-value array indexed at run time passes.
        let fine = format!(
            "{pos} {{ let a = array<vec4<f32>, 64>(); let m = mat4x4<f32>(); \
             return a[u32(p.x)] + m[u32(p.y) % 4u]; }}\n"
        );
        let (d, _) = run(&bar(""), &[("aurora.wgsl", &fine)]);
        assert!(d.is_empty(), "{:?}", codes(&d));
    }

    /// A file without uniforms needs no props; two nodes share one check.
    #[test]
    fn plain_files_and_shared_files() {
        let plain = "@fragment\nfn main(v: StrandVertex) -> @location(0) vec4<f32> {\n    return vec4<f32>(v.uv, 0.0, 1.0);\n}\n";
        let src = "bar Top { edge: top; height: 30\n  shader \"p.wgsl\" { width: 10 }\n  shader \"p.wgsl\" { width: 20 }\n}\n";
        let (d, shaders) = run(src, &[("p.wgsl", plain)]);
        assert_eq!(shaders.len(), 1);
        if cfg!(feature = "shaders") {
            assert!(d.is_empty(), "{:?}", codes(&d));
        } else {
            // The CPU-only build says once per node why it draws nothing.
            assert_eq!(d.len(), 2);
            assert!(
                d.iter()
                    .all(|d| d.code == "check::shader_unchecked" && !d.is_error())
            );
            assert!(d[0].message.contains("built without the GPU backend"));
            assert!(shaders["p.wgsl"].uniforms.is_empty());
        }
    }

    #[test]
    fn paths_resolve_under_the_config() {
        let dir = Path::new("/cfg");
        assert_eq!(resolve("a.wgsl", dir), Path::new("/cfg/a.wgsl"));
        assert_eq!(resolve("/x/a.wgsl", dir), Path::new("/x/a.wgsl"));
    }
}
