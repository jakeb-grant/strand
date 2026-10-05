//! The VM: bytecode evaluated against strand-core signals.

use std::rc::Rc;
use std::sync::Arc;

use strand_compiler::instantiate::Instance;
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_compiler::{SourceMap, lower};
use strand_core::Runtime;

fn run(src: &str) -> (Runtime, Rc<SchemaHost>, Instance) {
    let mut map = SourceMap::new();
    map.add("t.strand", src.to_string());
    let c = strand_compiler::compile(&map);
    assert_eq!(c.errors(), 0, "{:#?}", c.diagnostics);
    let p = Arc::new(lower::lower(
        &c.program,
        strand_compiler::schema::Schema::builtin(),
    ));
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &p.types));
    let inst = Instance::new(
        &rt,
        p,
        host.clone(),
        strand_compiler::instantiate::Storage::none(),
    );
    let u = inst.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    (rt, host, inst)
}

fn get(inst: &Instance, name: &str) -> Value {
    inst.value_of("t", name).unwrap()
}

/// A binding depends on exactly what its last run read: the branch a
/// ternary did not take is not a dependency.
#[test]
fn dependencies_are_exact() {
    let (rt, _, inst) = run("state c = true\nstate a = 1\nstate b = 2\nlet x = c ? a : b\n");
    assert_eq!(get(&inst, "x"), Value::int(1));
    let before = rt.stats().computations;
    inst.set_value("t", "b", Value::int(5)).unwrap();
    inst.flush();
    assert_eq!(get(&inst, "x"), Value::int(1));
    assert_eq!(rt.stats().computations, before, "`b` is not read while `c`");
    inst.set_value("t", "c", Value::Bool(false)).unwrap();
    inst.flush();
    assert_eq!(get(&inst, "x"), Value::int(5));
    let before = rt.stats().computations;
    inst.set_value("t", "a", Value::int(9)).unwrap();
    inst.flush();
    let _ = get(&inst, "x");
    assert_eq!(rt.stats().computations, before, "now `a` is not read");
}

/// `??` replaces null and a pending or failed `Async`; `?.` stops at
/// null.
#[test]
fn coalesce_and_optional_chaining() {
    let src = "let title = windows.focused?.title ?? \"none\"\nlet wall = material(image: \"x.jpg\") ?? material(seed: #7aa2f7)\nlet hits = apps.search(\"f\")\nlet n = hits.len\nlet first = hits ?? []\nlet pending = hits.pending\n";
    let (rt, host, inst) = run(src);
    assert_eq!(get(&inst, "title"), Value::text("none"));
    let w = host.record("Window", &[("title", Value::text("Files"))]);
    host.set(&rt, "windows.focused", w).unwrap();
    inst.flush();
    assert_eq!(get(&inst, "title"), Value::text("Files"));
    // A failed wallpaper palette falls back to the seed.
    assert!(matches!(get(&inst, "wall"), Value::Palette(_)));
    assert_eq!(get(&inst, "n"), Value::int(0));
    assert_eq!(get(&inst, "pending"), Value::Bool(false));
}

/// fns are pure and may recurse; runaway recursion is an error value.
#[test]
fn fns_lambdas_and_recursion() {
    let src = "fn fact(n: int) -> int { n <= 1 ? 1 : n * fact(n - 1) }\nfn forever(n: int) -> int { forever(n + 1) }\nstate xs = [3, 1, 2]\nlet f = fact(5)\nlet big = xs.filter(x => x > 1).map(x => x * 10)\nlet sorted = xs.sort_by(x => x)\nlet total = xs.count(x => x >= 2)\nlet name = \"Strand\".upper()\nlet m = match xs.len { 3 => \"three\", _ => \"other\" }\n";
    let (_, _, inst) = run(src);
    assert_eq!(get(&inst, "f"), Value::int(120));
    let shown = |n: &str| get(&inst, n).show(inst.vm().types());
    assert_eq!(shown("big"), "30, 20");
    assert_eq!(shown("sorted"), "1, 2, 3");
    assert_eq!(get(&inst, "total"), Value::int(2));
    assert_eq!(get(&inst, "name"), Value::text("STRAND"));
    assert_eq!(get(&inst, "m"), Value::text("three"));
}

#[test]
fn runaway_recursion_is_an_error_value() {
    let src = "fn forever(n: int) -> int { forever(n + 1) }\nstate go = false\nlet r = go ? forever(0) : 0\n";
    let (_, _, inst) = run(src);
    inst.set_value("t", "go", Value::Bool(true)).unwrap();
    inst.flush();
    let err = inst.value_of("t", "r").unwrap_err();
    assert!(err.to_string().contains("nested more than"), "{err}");
}

/// Keyed collections in handlers: push, insert, move, update, remove by
/// key; a duplicate key is an error value.
#[test]
fn keyed_state_mutations() {
    let src = "type Pin { app: text; label: text }\nstate pins: [Pin] key app = []\nstate log = \"\"\nlet labels = pins.map(p => p.label).join(\",\")\nbar B {\n  box { on click { pins.push(Pin(app: \"a\", label: \"A\")); pins.push(Pin(app: \"b\", label: \"B\")); pins.insert(0, Pin(app: \"c\", label: \"C\")) } }\n  box { on click { pins.move(\"c\", 2); pins.update(\"a\", p => Pin(app: p.app, label: \"A2\")) } }\n  box { on click { pins.remove_key(\"b\") } }\n  box { on click { pins.push(Pin(app: \"a\", label: \"dup\")) } }\n}\n";
    let (rt, host, inst) = run(src);
    let list = host.record("Screen", &[("name", Value::text("DP-1"))]);
    host.set(&rt, "screens.all", Value::list(vec![list]))
        .unwrap();
    inst.flush();
    let boxes = inst.nodes_handling("click");
    assert_eq!(boxes.len(), 4);
    inst.event(boxes[0], "click", Vec::new());
    inst.flush();
    assert_eq!(get(&inst, "labels"), Value::text("C,A,B"));
    inst.event(boxes[1], "click", Vec::new());
    inst.flush();
    assert_eq!(get(&inst, "labels"), Value::text("A2,B,C"));
    inst.event(boxes[2], "click", Vec::new());
    inst.flush();
    assert_eq!(get(&inst, "labels"), Value::text("A2,C"));
    inst.event(boxes[3], "click", Vec::new());
    let u = inst.flush();
    assert_eq!(u.errors.len(), 1, "{:?}", u.errors);
    assert_eq!(get(&inst, "labels"), Value::text("A2,C"));
}

/// Compound assignment to `rw` service fields and settings fields.
#[test]
fn writes_to_services_and_settings() {
    let src = "state prefs from \"p.toml\" { scale: float = 1 }\nbar B { row { on scroll(dy) { audio.sink.volume -= dy * 0.05; prefs.scale *= 2 } } }\n";
    let (rt, host, inst) = run(src);
    host.set(
        &rt,
        "screens.all",
        Value::list(vec![
            host.record("Screen", &[("name", Value::text("DP-1"))]),
        ]),
    )
    .unwrap();
    host.set(&rt, "audio.sink.volume", Value::float(0.5))
        .unwrap();
    inst.flush();
    let row = inst.nodes_handling("scroll")[0];
    inst.event(row, "scroll", vec![Value::float(2.0), Value::float(0.0)]);
    inst.flush();
    let v = host
        .get(&rt, "audio.sink.volume")
        .unwrap()
        .as_f64()
        .unwrap();
    assert!((v - 0.4).abs() < 1e-9, "{v}");
    let prefs = get(&inst, "prefs");
    assert_eq!(
        prefs.field(inst.vm().types(), "scale"),
        Some(&Value::float(2.0))
    );
}

/// `??` gives the fallback while an `Async` is pending, then the value
/// once it is ready; the `let` keeps one load per change of its
/// arguments (`apps.search(q)` is an async memo).
#[test]
fn coalesce_covers_a_pending_async() {
    let src = "state q = \"f\"\nlet hits = apps.search(q)\nlet first = hits ?? []\nlet n = first.len\nlet waiting = hits.pending\nbar B { text join(\" \", n, waiting) }\n";
    let mut map = SourceMap::new();
    map.add("t.strand", src.to_string());
    let c = strand_compiler::compile(&map);
    assert_eq!(c.errors(), 0, "{:#?}", c.diagnostics);
    let p = Arc::new(lower::lower(
        &c.program,
        strand_compiler::schema::Schema::builtin(),
    ));
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &p.types));
    let apps = ["firefox", "files", "foot"]
        .iter()
        .map(|a| host.record("App", &[("id", Value::text(*a)), ("name", Value::text(*a))]))
        .collect();
    host.set(&rt, "apps.all", Value::list(apps)).unwrap();
    let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
    host.set(&rt, "screens.all", Value::list(vec![screen]))
        .unwrap();
    host.hold("apps.search");
    let inst = Instance::new(
        &rt,
        p,
        host.clone(),
        strand_compiler::instantiate::Storage::none(),
    );
    inst.flush();
    assert_eq!(get(&inst, "waiting"), Value::Bool(true));
    assert_eq!(get(&inst, "first"), Value::list(Vec::new()), "pending");
    assert_eq!(get(&inst, "n"), Value::int(0));
    host.release_fetch("apps.search");
    inst.flush();
    assert_eq!(get(&inst, "waiting"), Value::Bool(false));
    assert_eq!(
        get(&inst, "n"),
        Value::int(3),
        "ready: firefox, files, foot"
    );
    // A new query starts a new load: pending again, then its result.
    host.hold("apps.search");
    inst.set_value("t", "q", Value::text("fi")).unwrap();
    inst.flush();
    assert_eq!(get(&inst, "first"), Value::list(Vec::new()));
    assert_eq!(get(&inst, "waiting"), Value::Bool(true));
    host.release_fetch("apps.search");
    inst.flush();
    assert_eq!(get(&inst, "n"), Value::int(2));
}

/// `await` on an async `let` waits for its load (never a silent null or
/// a stale value), and every handler awaiting the same value gets the
/// same result.
#[test]
fn await_waits_for_a_pending_load() {
    let src = "state q = \"f\"\nstate a = -1\nstate b = -1\nlet hits = apps.search(q)\non notifications.received(x) { let r = await hits\n a = r.len }\non notifications.received(x) { let r = await hits\n b = r.len }\nbar B { text join(\" \", a, b, hits.pending) }\n";
    let mut map = SourceMap::new();
    map.add("t.strand", src.to_string());
    let c = strand_compiler::compile(&map);
    assert_eq!(c.errors(), 0, "{:#?}", c.diagnostics);
    let p = Arc::new(lower::lower(
        &c.program,
        strand_compiler::schema::Schema::builtin(),
    ));
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &p.types));
    let apps = ["firefox", "files", "foot"]
        .iter()
        .map(|a| host.record("App", &[("id", Value::text(*a)), ("name", Value::text(*a))]))
        .collect();
    host.set(&rt, "apps.all", Value::list(apps)).unwrap();
    let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
    host.set(&rt, "screens.all", Value::list(vec![screen]))
        .unwrap();
    host.hold("apps.search");
    let inst = Instance::new(
        &rt,
        p,
        host.clone(),
        strand_compiler::instantiate::Storage::none(),
    );
    inst.flush();
    host.emit(&rt, "notifications.received", vec![Value::Null])
        .unwrap();
    let u = inst.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    // Both handlers are suspended at `await`: nothing written yet.
    assert_eq!(get(&inst, "a"), Value::int(-1));
    assert_eq!(get(&inst, "b"), Value::int(-1));
    host.release_fetch("apps.search");
    inst.flush();
    let u = inst.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    assert_eq!(get(&inst, "a"), Value::int(3), "firefox, files, foot");
    assert_eq!(get(&inst, "b"), Value::int(3), "the second awaiter too");
    // Awaiting a settled value resumes at once.
    host.emit(&rt, "notifications.received", vec![Value::Null])
        .unwrap();
    inst.set_value("t", "a", Value::int(0)).unwrap();
    inst.flush();
    inst.flush();
    assert_eq!(get(&inst, "a"), Value::int(3));
}

/// The runtime host keeps no history of actions (the mock records them
/// for tests).
#[test]
fn the_real_host_records_no_actions() {
    let src = "bar B { text \"x\" { on click { notifications.clear() } } }\n";
    let mut map = SourceMap::new();
    map.add("t.strand", src.to_string());
    let c = strand_compiler::compile(&map);
    assert_eq!(c.errors(), 0, "{:#?}", c.diagnostics);
    let p = Arc::new(lower::lower(
        &c.program,
        strand_compiler::schema::Schema::builtin(),
    ));
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::real(&rt, &p.types));
    let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
    host.set(&rt, "screens.all", Value::list(vec![screen]))
        .unwrap();
    let inst = Instance::new(
        &rt,
        p,
        host.clone(),
        strand_compiler::instantiate::Storage::none(),
    );
    inst.flush();
    let text = inst.nodes_handling("click")[0];
    for _ in 0..100 {
        assert!(inst.event(text, "click", Vec::new()));
        inst.flush();
    }
    assert!(host.actions().is_empty());
}
