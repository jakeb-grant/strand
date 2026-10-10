//! `shader "x.wgsl" { u_*: … }` from the loader to the scene (M4,
//! architecture.md, "strand-compiler" M4 additions): the loader reads and
//! checks the file, the build carries its code, the instance sets
//! `Prop::Shader` and `Prop::Uniforms`, a `when` block's uniforms land
//! over the base ones, and a saved file that fails the check keeps the
//! last good build until it is fixed.

#![cfg_attr(not(feature = "shaders"), allow(unused_imports, dead_code))]

use std::path::{Path, PathBuf};
use std::rc::Rc;

use strand_compiler::instantiate::{Instance, SceneMirror, Storage};
use strand_compiler::reconcile::loader::Loader;
use strand_compiler::schema::Schema;
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_core::Runtime;
use strand_scene::shader::UniformType;
use strand_scene::{NodeKind, Prop, PropValue};

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("strand-shaders-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write(d: &Path, name: &str, text: &str) -> PathBuf {
    let p = d.join(name);
    std::fs::write(&p, text).unwrap();
    p
}

const BAR: &str = r#"export state hot = false
bar Top { edge: top; height: 30
  shader "aurora.wgsl" { width: 100; u_speed: 0.5; u_tint: 1, 0, 0, 1
    when hot { u_speed: 2 }
  }
}
"#;

const AURORA: &str = "\
@group(1) @binding(0) var<uniform> u_speed: f32;
@group(1) @binding(1) var<uniform> u_tint: vec4<f32>;
@fragment
fn main(v: StrandVertex) -> @location(0) vec4<f32> {
    return u_tint * fract(strand.time * u_speed + v.uv.x);
}
";

fn uniforms(scene: &SceneMirror, node: strand_scene::NodeId) -> Vec<(String, f32)> {
    match scene.prop(node, Prop::Uniforms) {
        Some(PropValue::Uniforms(entries)) => entries
            .iter()
            .map(|(n, v)| {
                let x = match v {
                    PropValue::Number(x) => *x,
                    PropValue::List(parts) => parts.len() as f32,
                    other => panic!("{n}: {other:?}"),
                };
                (n.clone(), x)
            })
            .collect(),
        other => panic!("no uniforms: {other:?}"),
    }
}

#[cfg(feature = "shaders")]
#[test]
fn the_loader_checks_shader_files_and_the_scene_gets_their_code() {
    let d = dir("load");
    write(&d, "bar.strand", BAR);
    let wgsl = write(&d, "aurora.wgsl", AURORA);
    let mut l = Loader::new(&d, Schema::builtin().clone(), None);
    let out = l.boot();
    let build = out.build.expect("the config compiles");
    assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    assert_eq!(l.shader_files(), [(wgsl.clone(), Some(AURORA.into()))]);
    let code = build.program.shaders["aurora.wgsl"].clone();
    assert_eq!(
        code.uniform("u_tint").map(|u| u.ty),
        Some(UniformType::Vec4)
    );

    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &build.program.types));
    let screen = host.record(
        "Screen",
        &[
            ("id", Value::text("Mock | DP-1 | Display")),
            ("name", Value::text("DP-1")),
            ("make", Value::text("Mock")),
            ("model", Value::text("DP-1")),
            ("description", Value::text("Display")),
        ],
    );
    host.set(&rt, "screens.all", Value::list(vec![screen]))
        .unwrap();
    let mut inst = Instance::from_build(&rt, &build, host, Storage::none());
    let mut scene = SceneMirror::new();
    let u = inst.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    scene.apply(&u.diff).unwrap();
    let node = scene.of_kind(NodeKind::Shader)[0];
    assert_eq!(
        scene.prop(node, Prop::Shader),
        Some(&PropValue::Shader(code.clone()))
    );
    // Sorted by name; `u_tint`'s four comma values arrive as a list.
    assert_eq!(
        uniforms(&scene, node),
        [("u_speed".to_string(), 0.5), ("u_tint".to_string(), 4.0)]
    );
    // A `when` block's uniform lands over the base ones, the others kept.
    inst.set("bar.hot", Value::Bool(true)).unwrap();
    scene.apply(&inst.flush().diff).unwrap();
    assert_eq!(
        uniforms(&scene, node),
        [("u_speed".to_string(), 2.0), ("u_tint".to_string(), 4.0)]
    );

    // A saved file that fails the check: no build, the module that names
    // it is reported, the last good build stays.
    std::fs::write(&wgsl, AURORA.replace("u_tint *", "u_nope *")).unwrap();
    let out = l.changed([(wgsl.clone(), true)]);
    assert!(out.build.is_none());
    assert!(out.errors() > 0, "{:?}", out.diagnostics);
    assert!(
        out.diagnostics
            .iter()
            .any(|d| d.code == "check::shader" && d.message.starts_with("aurora.wgsl:5:")),
        "{:?}",
        out.diagnostics
    );
    assert_eq!(
        l.last().unwrap().program.shaders["aurora.wgsl"].wgsl,
        AURORA
    );
    // Fixed: a new build with the new code, though no module changed.
    let fixed = AURORA.replace("fract", "sin");
    std::fs::write(&wgsl, &fixed).unwrap();
    let out = l.changed([(wgsl.clone(), true)]);
    let build = out.build.expect("the fixed shader commits");
    assert_eq!(build.program.shaders["aurora.wgsl"].wgsl, fixed);
    inst.reload(&build);
    scene.apply(&inst.flush().diff).unwrap();
    match scene.prop(node, Prop::Shader) {
        Some(PropValue::Shader(c)) => assert_eq!(c.wgsl, fixed),
        other => panic!("{other:?}"),
    }
    // A rescan with the file unchanged compiles nothing new.
    assert!(l.rescan().build.is_none());
    let _ = std::fs::remove_dir_all(d);
}

/// A prop the file does not declare holds the module back like any
/// checker error.
#[cfg(feature = "shaders")]
#[test]
fn a_uniform_the_file_lacks_holds_the_module_back() {
    let d = dir("held");
    let bar = write(&d, "bar.strand", BAR);
    write(&d, "aurora.wgsl", AURORA);
    let mut l = Loader::new(&d, Schema::builtin().clone(), None);
    assert!(l.boot().build.is_some());
    std::fs::write(&bar, BAR.replace("u_speed: 2", "u_sped: 2")).unwrap();
    let out = l.changed([(bar.clone(), true)]);
    assert!(out.build.is_none());
    assert_eq!(out.held, [bar]);
    let e = out
        .diagnostics
        .iter()
        .find(|d| d.code == "check::unknown_uniform")
        .expect("the unknown uniform is reported");
    assert_eq!(e.help.as_deref(), Some("did you mean `u_speed`?"));
    let _ = std::fs::remove_dir_all(d);
}

/// The CPU-only build reads the file but does not parse it: the node
/// gets code without slots, and a warning says why it draws nothing.
#[cfg(not(feature = "shaders"))]
#[test]
fn without_shaders_the_file_is_not_checked() {
    let d = dir("cpu");
    write(&d, "bar.strand", BAR);
    write(&d, "aurora.wgsl", "not wgsl at all");
    let mut l = Loader::new(&d, Schema::builtin().clone(), None);
    let out = l.boot();
    let build = out.build.expect("the config compiles");
    assert!(build.program.shaders["aurora.wgsl"].uniforms.is_empty());
    assert!(
        out.diagnostics
            .iter()
            .any(|d| d.message.contains("built without the GPU backend")),
        "{:?}",
        out.diagnostics
    );
    let _ = std::fs::remove_dir_all(d);
}

/// (m4-audit) A shader path is any string of the config, so the file
/// must be a regular file of at most a megabyte: `/dev/zero` (which would
/// read until memory ran out), a FIFO (which would block the compiler
/// worker, ending hot reload) and an oversized file are refused with
/// `check::shader_file`, without reading them. The FIFO has no writer:
/// opening it would block this test for good.
#[test]
fn shader_files_refuse_fifos_devices_and_huge_files() {
    use strand_compiler::check::shaders::{MAX_SHADER_BYTES, read_file};
    let d = dir("kinds");
    let fifo = d.join("pipe.wgsl");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .is_ok_and(|s| s.success());
    let huge = d.join("huge.wgsl");
    std::fs::write(&huge, vec![b' '; MAX_SHADER_BYTES as usize + 1]).unwrap();
    let mut refused = vec!["/dev/zero".to_string(), huge.display().to_string()];
    if made {
        refused.push(fifo.display().to_string());
    }
    for path in &refused {
        let r = read_file(Path::new(path));
        assert!(r.is_err(), "{path}: {r:?}");
        // Through the loader too: the module is held back, nothing read.
        write(
            &d,
            "bar.strand",
            &format!(
                "bar Top {{ edge: top; height: 30\n  shader \"{path}\" {{ width: 100 }}\n}}\n"
            ),
        );
        let mut l = Loader::new(&d, Schema::builtin().clone(), None);
        let out = l.boot();
        assert!(out.build.is_none(), "{path}");
        assert!(
            out.diagnostics
                .iter()
                .any(|e| e.code == "check::shader_file"),
            "{path}: {:?}",
            out.diagnostics
        );
    }
    // A regular file at the limit is read.
    let ok = write(&d, "ok.wgsl", AURORA);
    assert_eq!(read_file(&ok).as_deref(), Ok(AURORA));
    let _ = std::fs::remove_dir_all(d);
}

/// (m4-audit) A saved `.wgsl` file the lock draws is a lock edit
/// (design.md, "What each edit does": anything inside `lock` waits for
/// the unlock): its code is part of the lock's hash, whether the lock
/// names the file itself or through a component it mounts, so while the
/// lock is shown the build waits and the lock's node keeps its code. A
/// save of a file only a bar draws still commits at once.
#[test]
fn a_saved_shader_file_the_lock_draws_waits_for_the_unlock() {
    use strand_compiler::reconcile::EditClass;
    const PLAIN: &str = "@fragment\nfn main(v: StrandVertex) -> @location(0) vec4<f32> {\n    return vec4<f32>(v.uv.x, 0.0, 0.0, 1.0);\n}\n";
    let d = dir("lock");
    write(
        &d,
        "shell.strand",
        "component Halo {\n  shader \"halo.wgsl\" { width: 10 }\n}\n\
         lock L {\n  on key(k) { auth.submit(k.name) }\n  shader \"glow.wgsl\" { width: 10 }\n  Halo\n}\n\
         bar Top { edge: top; height: 30\n  shader \"aurora.wgsl\" { width: 10 }\n}\n",
    );
    let glow = write(&d, "glow.wgsl", PLAIN);
    let halo = write(&d, "halo.wgsl", PLAIN);
    let aurora = write(&d, "aurora.wgsl", PLAIN);
    let mut l = Loader::new(&d, Schema::builtin().clone(), None);
    let out = l.boot();
    let build = out.build.expect("the config compiles");
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &build.program.types));
    let screen = host.record(
        "Screen",
        &[
            ("id", Value::text("Mock | DP-1 | Display")),
            ("name", Value::text("DP-1")),
            ("make", Value::text("Mock")),
            ("model", Value::text("DP-1")),
            ("description", Value::text("Display")),
        ],
    );
    host.set(&rt, "screens.all", Value::list(vec![screen]))
        .unwrap();
    let mut inst = Instance::from_build(&rt, &build, host, Storage::none());
    let mut scene = SceneMirror::new();
    let u = inst.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    scene.apply(&u.diff).unwrap();
    assert!(inst.lock_shown());
    let codes = |scene: &SceneMirror| -> Vec<String> {
        let mut v: Vec<String> = scene
            .of_kind(NodeKind::Shader)
            .into_iter()
            .map(|n| match scene.prop(n, Prop::Shader) {
                Some(PropValue::Shader(c)) => format!("{}: {}", c.path, c.wgsl.len()),
                other => panic!("{other:?}"),
            })
            .collect();
        v.sort();
        v
    };
    let before = codes(&scene);
    assert_eq!(before.len(), 3, "{before:?}");
    let locks = build.hashes.locks();

    // The bar's file: the lock's hash is unchanged and the build commits.
    let longer = PLAIN.replace("0.0, 0.0", "0.25, 0.0");
    std::fs::write(&aurora, &longer).unwrap();
    let build = l.changed([(aurora.clone(), true)]).build.expect("built");
    assert_eq!(build.hashes.locks(), locks);
    let report = inst.reload(&build);
    assert!(
        !report.classes.contains(&EditClass::LockDeferred),
        "{:?}",
        report.classes
    );
    scene.apply(&inst.flush().diff).unwrap();
    let committed = codes(&scene);
    assert_ne!(committed, before);

    // The lock's own file, then the file of a component it mounts: each
    // changes the lock's hash and waits while the lock is shown.
    for file in [&glow, &halo] {
        std::fs::write(file, &longer).unwrap();
        let build = l.changed([(file.clone(), true)]).build.expect("built");
        assert_ne!(build.hashes.locks(), locks, "{}", file.display());
        let report = inst.reload(&build);
        assert_eq!(
            report.classes,
            [EditClass::LockDeferred],
            "{}",
            file.display()
        );
        scene.apply(&inst.flush().diff).unwrap();
        assert_eq!(codes(&scene), committed, "{}", file.display());
        std::fs::write(file, PLAIN).unwrap();
        let back = l.changed([(file.clone(), true)]).build.expect("built");
        assert_eq!(back.hashes.locks(), locks, "{}", file.display());
    }
    let _ = std::fs::remove_dir_all(d);
}
