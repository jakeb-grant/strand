//! The VM: bytecode evaluated against strand-core signals.

use std::rc::Rc;
use std::sync::Arc;

use strand_compiler::instantiate::Instance;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_compiler::vm::{Num, Value};
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

/// (services-round) A record built without its optional and list fields
/// holds null and the empty list there (spec.md 5.4).
#[test]
fn a_record_left_without_a_list_field_holds_an_empty_one() {
    let src = "type Pin { app: text; tags: [text]; note: text? }\nlet p = Pin(app: \"a\")\nlet n = p.tags.join(\",\")\nlet none = p.note == null\nbar B { height: 10 }\n";
    let (_rt, _host, inst) = run(src);
    inst.flush();
    assert_eq!(get(&inst, "n"), Value::text(""));
    assert_eq!(get(&inst, "none"), Value::Bool(true));
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

/// `??` gives the fallback while an `Async` is loading the first time,
/// then the value once it is ready, and keeps that value while a newer
/// one loads (strand-core's `Async::or`, decisions.md wave3-theme); the
/// `let` keeps one load per change of its arguments (`apps.search(q)` is
/// an async memo).
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
    assert_eq!(get(&inst, "n"), Value::int(3), "the last results stay");
    assert_eq!(get(&inst, "waiting"), Value::Bool(true));
    host.release_fetch("apps.search");
    inst.flush();
    assert_eq!(get(&inst, "n"), Value::int(2));
}

/// A list transform of an `Async` list is `Async` too: it keeps the
/// loading state, so `.pending` and `??` work on `hits.take(2)`.
#[test]
fn async_list_transforms_keep_the_loading_state() {
    let src = "state q = \"f\"\nlet hits = apps.search(q)\nlet top = hits.take(2)\nlet top_pending = top.pending\nlet top_n = (top ?? []).len\nlet shown = hits.len\nbar B { text join(\" \", top_n, top_pending, shown) }\n";
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
    assert_eq!(get(&inst, "top_pending"), Value::Bool(true));
    assert_eq!(get(&inst, "top_n"), Value::int(0), "pending: the fallback");
    host.release_fetch("apps.search");
    inst.flush();
    assert_eq!(get(&inst, "top_pending"), Value::Bool(false));
    assert_eq!(get(&inst, "top_n"), Value::int(2));
    assert_eq!(get(&inst, "shown"), Value::int(3));
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

/// `await` on a transform of a loading list waits for the load, then
/// applies the transform.
#[test]
fn await_waits_for_a_transformed_load() {
    let src = "state q = \"f\"\nstate a = -1\nlet hits = apps.search(q)\non notifications.received(x) { let r = await hits.take(2)\n a = r.len }\nbar B { text join(\" \", a, hits.pending) }\n";
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
    assert_eq!(get(&inst, "a"), Value::int(-1), "suspended at `await`");
    host.release_fetch("apps.search");
    inst.flush();
    let u = inst.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    assert_eq!(get(&inst, "a"), Value::int(2), "the first two hits");
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

/// A lambda's parameters have names, so a call may name them, in any
/// order (decisions.md, wave2-check round 8).
#[test]
fn lambdas_take_named_arguments() {
    let (_, _, inst) = run(
        "let f = (a: int, b: text) => join(\":\", b, a)\nlet y = f(b: \"x\", a: 1)\nlet z = f(2, b: \"w\")\n",
    );
    assert_eq!(get(&inst, "y"), Value::text("x:1"));
    assert_eq!(get(&inst, "z"), Value::text("w:2"));
}

/// Time-bound values (M4) stay symbolic on the logic thread: `t`,
/// `wave(…)` and `noise(t)` and arithmetic on them are
/// [`Value::Time`] expressions render evaluates per node per frame, with
/// no warning; `noise` of a constant is a number now. Reaching logic (a
/// handler storing one, a comparison, a condition, `match`, text) is an
/// error value, as a token in arithmetic without numbers is. (flex) A
/// builtin that computes at once (`join(" ", t)`) is refused at check
/// instead (`check::time_in_logic`, tested in flex.rs).
#[test]
fn time_values_in_handlers_are_error_values() {
    use strand_scene::{BinOp, PropValue, TokenExpr};
    let src = "state x = 0.0
state log = \"\"
let spin = t * 20deg
let glow = 10 * wave(2s, phase: 0.25)
let jitter = noise(t * 3)
let still = noise(3)
let late = t > 2
let picked = match wave(1s) { 0 => \"zero\", _ => \"other\" }
bar B {\n  box { rotate: spin; on click { x = t } }\n  box { on click { log = \"ok\" } }\n}\n";
    let mut map = SourceMap::new();
    map.add("t.strand", src.to_string());
    let c = strand_compiler::compile(&map);
    assert_eq!(c.errors(), 0, "{:#?}", c.diagnostics);
    let p = Arc::new(lower::lower(
        &c.program,
        strand_compiler::schema::Schema::builtin(),
    ));
    assert!(p.warnings.is_empty(), "no warning: {:#?}", p.warnings);
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &p.types));
    let inst = Instance::new(
        &rt,
        p,
        host.clone(),
        strand_compiler::instantiate::Storage::none(),
    );
    let u = inst.flush();
    assert!(u.notices.is_empty(), "{:?}", u.notices);
    let num = |n: f32| TokenExpr::value(PropValue::Number(n));
    assert_eq!(
        get(&inst, "spin"),
        Value::Time(Rc::new(TokenExpr::Binary {
            op: BinOp::Mul,
            lhs: Box::new(TokenExpr::Time),
            rhs: Box::new(num(20.0)),
        }))
    );
    assert_eq!(
        get(&inst, "glow"),
        Value::Time(Rc::new(TokenExpr::Binary {
            op: BinOp::Mul,
            lhs: Box::new(num(10.0)),
            rhs: Box::new(TokenExpr::Wave {
                period: std::time::Duration::from_secs(2),
                phase: Box::new(num(0.25)),
            }),
        }))
    );
    assert!(
        matches!(get(&inst, "jitter"), Value::Time(e) if matches!(&*e, TokenExpr::Noise(_))),
        "noise of `t` is a time value"
    );
    assert_eq!(
        get(&inst, "still"),
        Value::float(strand_scene::tokens::noise(3.0) as f64),
        "noise of a constant is computed now, as render would"
    );
    for (name, what) in [("late", "with `>`"), ("picked", "in `match`")] {
        let err = inst.value_of("t", name).unwrap_err().to_string();
        assert!(
            err.contains("time-bound value") && err.contains(what),
            "{name}: {err}"
        );
    }
    // A handler storing one fails (its task returns the error); the
    // other handler still runs.
    let list = host.record("Screen", &[("name", Value::text("DP-1"))]);
    host.set(&rt, "screens.all", Value::list(vec![list]))
        .unwrap();
    inst.flush();
    let boxes = inst.nodes_handling("click");
    assert_eq!(boxes.len(), 2);
    inst.event(boxes[0], "click", Vec::new());
    let u = inst.flush();
    assert_eq!(u.errors.len(), 1, "{:?}", u.errors);
    assert!(
        u.errors[0].to_string().contains("cannot be stored"),
        "{:?}",
        u.errors
    );
    assert_eq!(get(&inst, "x"), Value::float(0.0), "nothing stored");
    inst.event(boxes[1], "click", Vec::new());
    assert!(inst.flush().errors.is_empty());
    assert_eq!(get(&inst, "log"), Value::text("ok"));
}

/// Gaps 16 (decisions.md m6-w2-language): dates order, earlier first.
#[test]
fn dates_order() {
    let src = "let m = clock.today.month_start()\nlet earlier = m.add(days: -1) < m\nlet later = m.add(months: 1) > m\nlet same = m <= m && m >= m\nlet not_less = m < m\nlet before = calendar.days(m).count(d => d.date < m)\n";
    let (_, _, inst) = run(src);
    assert_eq!(get(&inst, "earlier"), Value::Bool(true));
    assert_eq!(get(&inst, "later"), Value::Bool(true));
    assert_eq!(get(&inst, "same"), Value::Bool(true));
    assert_eq!(get(&inst, "not_less"), Value::Bool(false));
    // The grid's leading days are the previous month's.
    let lead = get(&inst, "before").as_f64().unwrap();
    assert!((0.0..7.0).contains(&lead), "{lead}");
}

/// Gaps 11 (decisions.md m6-w2-language): `+` joins texts, `distinct`
/// drops repeats keeping the first, `sum` adds numbers of one unit.
#[test]
fn text_plus_distinct_and_sum() {
    let src = "state name = \"Str\"\nlet joined = name + \"and\"\nstate xs = [3, 1, 3, 2, 1]\nlet uniq = xs.distinct()\nlet total = xs.sum()\nlet widths = xs.sum(x => x * 2px)\nlet none = xs.filter(x => x > 9).sum()\nlet glyphs = [\"web\", \"code\", \"web\"].distinct().join(\",\")\nlet idle = xs.filter(x => x > 9).sum(x => x * 1ms)\nlet waits = xs.filter(x => x > 9).map(x => x * 1s).sum()\nlet shares = xs.filter(x => x > 9).sum(x => x * 1%)\nbar B { box { on click { name += \"!\" } } }\n";
    let (rt, host, inst) = run(src);
    let shown = |n: &str| get(&inst, n).show(inst.vm().types());
    assert_eq!(get(&inst, "joined"), Value::text("Strand"));
    assert_eq!(shown("uniq"), "3, 1, 2");
    assert_eq!(get(&inst, "total"), Value::int(10));
    assert_eq!(shown("widths"), "20px");
    assert_eq!(get(&inst, "none"), Value::int(0));
    assert_eq!(get(&inst, "glyphs"), Value::text("web,code"));
    // (M6, the review) An empty list's sum is its unit's zero.
    assert_eq!(get(&inst, "idle"), Value::Num(0.0, Num::Ms));
    assert_eq!(get(&inst, "waits"), Value::Num(0.0, Num::Ms));
    assert_eq!(get(&inst, "shares"), Value::Num(0.0, Num::Percent));
    assert_eq!(shown("idle"), shown("waits"));
    let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
    host.set(&rt, "screens.all", Value::list(vec![screen]))
        .unwrap();
    inst.flush();
    let boxes = inst.nodes_handling("click");
    inst.event(boxes[0], "click", Vec::new());
    inst.flush();
    assert_eq!(get(&inst, "joined"), Value::text("Str!and"));
}

/// (M6, owner-testing gap 23) `index_of(test)` is the index of the first
/// item `test` is true for, or null, and follows the list it reads.
#[test]
fn index_of_finds_the_first_position() {
    let src = "state xs = [3, 1, 4, 1]\nlet one = xs.index_of(x => x == 1)\nlet first = xs.index_of(x => x > 2)\nlet none = xs.index_of(x => x > 9)\nlet after = xs[(xs.index_of(x => x == 4) ?? -1) + 1]\nbar B { box { on click { xs = [1, 4] } } }\n";
    let (rt, host, inst) = run(src);
    assert_eq!(get(&inst, "one"), Value::int(1));
    assert_eq!(get(&inst, "first"), Value::int(0));
    assert_eq!(get(&inst, "none"), Value::Null);
    assert_eq!(get(&inst, "after"), Value::int(1));
    let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
    host.set(&rt, "screens.all", Value::list(vec![screen]))
        .unwrap();
    inst.flush();
    let boxes = inst.nodes_handling("click");
    inst.event(boxes[0], "click", Vec::new());
    inst.flush();
    assert_eq!(get(&inst, "one"), Value::int(0));
    assert_eq!(get(&inst, "first"), Value::int(1));
    assert_eq!(get(&inst, "after"), Value::Null, "past the end");
}

/// Gaps 14 (decisions.md m6-w2-language): `dur(d, clock: true)` reads as
/// a player's clock.
#[test]
fn dur_has_a_clock_form() {
    let src = "let a = dur(5s, clock: true)\nlet b = dur(225s, clock: true)\nlet c = dur(3723s, clock: true)\nlet d = dur(225s)\nlet e = dur(media.length, clock: true)\nlet f = dur((media.length ?? 0s) * media.position, clock: true)\n";
    let (_, _, inst) = run(src);
    assert_eq!(get(&inst, "a"), Value::text("0:05"));
    assert_eq!(get(&inst, "b"), Value::text("3:45"));
    assert_eq!(get(&inst, "c"), Value::text("1:02:03"));
    assert_eq!(get(&inst, "d"), Value::text("3 min"));
    assert_eq!(get(&inst, "e"), Value::Null, "null in, null out");
    assert_eq!(get(&inst, "f"), Value::text("0:00"));
}

/// (report) A value as a glitch report's JSON and back by its type:
/// records by field name, enums by variant, durations in ms, colours as
/// `#rrggbbaa`; a text `keep` refuses is its length, read back as that
/// many placeholder `x`s; a value the type does not hold is refused.
#[test]
fn values_round_trip_through_report_json() {
    use strand_compiler::vm::json::{from_json, to_json, to_json_string};
    let src = "type Pin { app: text; label: text; at: duration; tint: color }\nstate pins: [Pin] = [Pin(app: \"a\", label: \"secret\", at: 1.5s, tint: #ff000080)]\nbar B { height: 10 }\n";
    let (_rt, _host, inst) = run(src);
    let prog = inst.vm().prog.clone();
    let v = get(&inst, "pins");
    assert_eq!(
        to_json_string(&prog.types, &v),
        r##"[{"app":"a","label":"secret","at":1500,"tint":"#ff000080"}]"##
    );
    let mut held = String::new();
    to_json(&prog.types, &v, &mut held, &mut |t| t != "secret").unwrap();
    assert_eq!(
        held,
        r##"[{"app":"a","label":{"~text":6},"at":1500,"tint":"#ff000080"}]"##
    );
    let ty = inst.cell_type("t.pins").unwrap();
    let j: serde_json::Value = serde_json::from_str(&to_json_string(&prog.types, &v)).unwrap();
    assert_eq!(from_json(&prog.types, &ty, &j).unwrap(), v);
    let j: serde_json::Value = serde_json::from_str(&held).unwrap();
    let back = from_json(&prog.types, &ty, &j).unwrap();
    let label = back.as_list().unwrap()[0]
        .field(&prog.types, "label")
        .cloned();
    assert_eq!(label, Some(Value::text("xxxxxx")));
    assert!(from_json(&prog.types, &ty, &serde_json::json!(3)).is_err());
}

/// (dictionary) A dictionary in a glitch report is `[key, value]` pairs
/// in key order, read back by its type; a key `keep` refuses is held
/// back as a value is, `{"~text": N}`. Two held keys of one length read
/// back as the same placeholders, so the later entry is kept and
/// `keys_read_as_one` counts the one lost (the replay's fidelity note).
#[test]
fn dictionaries_round_trip_through_report_json_with_held_keys() {
    use strand_compiler::vm::json::{from_json, keys_read_as_one, to_json, to_json_string};
    let src = "enum Level { low, high }\nstate used: [text: int] = [\"kitty\": 1, \"ab\": 2, \"cd\": 3]\nstate by: [Level: [int: text]] = [high: [2: \"b\"], low: [1: \"a\"]]\nbar B { height: 10 }\n";
    let (_rt, _host, inst) = run(src);
    let prog = inst.vm().prog.clone();
    let used = get(&inst, "used");
    assert_eq!(
        to_json_string(&prog.types, &used),
        r#"[["ab",2],["cd",3],["kitty",1]]"#
    );
    let ty = inst.cell_type("t.used").unwrap();
    let j: serde_json::Value = serde_json::from_str(&to_json_string(&prog.types, &used)).unwrap();
    assert_eq!(from_json(&prog.types, &ty, &j).unwrap(), used);
    assert_eq!(keys_read_as_one(&prog.types, &ty, &j), 0);
    // An object reads too, as a hand-written seed has it.
    let o = serde_json::json!({"kitty": 1, "cd": 3, "ab": 2});
    assert_eq!(from_json(&prog.types, &ty, &o).unwrap(), used);
    let mut held = String::new();
    to_json(&prog.types, &used, &mut held, &mut |t| t == "kitty").unwrap();
    assert_eq!(held, r#"[[{"~text":2},2],[{"~text":2},3],["kitty",1]]"#);
    let j: serde_json::Value = serde_json::from_str(&held).unwrap();
    let back = from_json(&prog.types, &ty, &j).unwrap();
    assert_eq!(back.show(&prog.types), "kitty: 1, xx: 3");
    assert_eq!(keys_read_as_one(&prog.types, &ty, &j), 1);
    // Nested: enum keys by name, a dictionary as a value.
    let by = get(&inst, "by");
    assert_eq!(
        to_json_string(&prog.types, &by),
        r#"[["low",[[1,"a"]]],["high",[[2,"b"]]]]"#
    );
    let ty = inst.cell_type("t.by").unwrap();
    let j: serde_json::Value = serde_json::from_str(&to_json_string(&prog.types, &by)).unwrap();
    assert_eq!(from_json(&prog.types, &ty, &j).unwrap(), by);
    let nested = serde_json::json!([["low", [[{"~text": 1}, "a"], [{"~text": 1}, "b"]]]]);
    let ty = strand_compiler::ty::Ty::dict(
        strand_compiler::ty::Ty::TEXT,
        strand_compiler::ty::Ty::dict(strand_compiler::ty::Ty::TEXT, strand_compiler::ty::Ty::TEXT),
    );
    assert_eq!(keys_read_as_one(&prog.types, &ty, &nested), 1);
}

/// (dictionary) `[K: V]` at run time: `m[k]` reads the value for a key or
/// null; `len`, `keys`, `values` and `entries` follow key order (ints
/// numerically, texts by Unicode scalar, enums in declaration order,
/// `false` before `true`) however the literal was written; of two
/// computed keys the later wins.
#[test]
fn dictionaries_read_by_key_in_key_order() {
    let src = "enum Urgency { low, normal, critical }\n\
               state two = 1\n\
               state names: [int: text] = [3: \"chat\", 1: \"web\", 10: \"mail\", 2: \"code\"]\n\
               let hit = names[1] ?? \"none\"\n\
               let miss = names[4] ?? \"none\"\n\
               let computed = names[two] ?? \"none\"\n\
               let n = names.len\n\
               let keys = names.keys\n\
               let values = names.values.join(\",\")\n\
               let pairs = names.entries.map(e => join(\"=\", e.key, e.value)).join(\",\")\n\
               let texts: [text: int] = [\"b\": 2, \"é\": 3, \"B\": 1, \"a\": 0]\n\
               let text_keys = texts.keys.join(\",\")\n\
               let waits: [Urgency: duration] = [critical: 0s, low: 3s, normal: 6s]\n\
               let wait_keys = waits.keys.map(u => join(\"\", u)).join(\",\")\n\
               let low_wait = waits[low] ?? 1s\n\
               let flags: [bool: text] = [true: \"on\", false: \"off\"]\n\
               let flag_values = flags.values.join(\",\")\n\
               state k = 1\n\
               let later = [k: \"first\", 1: \"second\"]\n\
               let later_n = later.len\n\
               let later_v = later[1] ?? \"\"\n";
    let (rt, _, inst) = run(src);
    let types = inst.vm().types();
    let shown = |n: &str| get(&inst, n).show(types);
    assert_eq!(get(&inst, "hit"), Value::text("web"));
    assert_eq!(get(&inst, "miss"), Value::text("none"));
    // A key read through a state follows it.
    assert_eq!(get(&inst, "computed"), Value::text("web"));
    inst.set_value("t", "two", Value::float(2.0)).unwrap();
    inst.flush();
    assert_eq!(get(&inst, "computed"), Value::text("code"));
    assert_eq!(get(&inst, "n"), Value::int(4));
    assert_eq!(
        get(&inst, "keys"),
        Value::list(vec![
            Value::int(1),
            Value::int(2),
            Value::int(3),
            Value::int(10)
        ])
    );
    assert_eq!(get(&inst, "values"), Value::text("web,code,chat,mail"));
    assert_eq!(
        get(&inst, "pairs"),
        Value::text("1=web,2=code,3=chat,10=mail")
    );
    assert_eq!(get(&inst, "text_keys"), Value::text("B,a,b,é"));
    assert_eq!(get(&inst, "wait_keys"), Value::text("low,normal,critical"));
    assert_eq!(get(&inst, "low_wait"), Value::Num(3000.0, Num::Ms));
    assert_eq!(get(&inst, "flag_values"), Value::text("off,on"));
    assert_eq!(shown("names"), "1: web, 2: code, 3: chat, 10: mail");
    // `[k: …, 1: …]` with `k` holding 1: one entry, the later.
    assert_eq!(get(&inst, "later_n"), Value::int(1));
    assert_eq!(get(&inst, "later_v"), Value::text("second"));
    inst.set_value("t", "k", Value::int(5)).unwrap();
    inst.flush();
    assert_eq!(get(&inst, "later_n"), Value::int(2));
    drop(rt);
}

/// (dictionary) Writes, in handlers: `m[k] = v` inserts at the key's place
/// or replaces its value; `m[k].f = x` changes a field of a present
/// entry; `m.remove(k)` forgets an entry and does nothing for a key the
/// dictionary does not hold; `m.clear()` empties it; `m = m.filter(e =>
/// …)` keeps the entries the test is true for, as a dictionary. A field
/// write to an absent entry is a handler error value, and changes
/// nothing.
#[test]
fn dictionary_writes_insert_replace_remove_and_filter() {
    let src = "type Pin { app: text; at: int }\n\
               state m: [int: text] = [2: \"b\"]\n\
               state pins: [text: Pin] = [\"a\": Pin(app: \"a\", at: 1)]\n\
               let shown = m.entries.map(e => join(\"=\", e.key, e.value)).join(\",\")\n\
               let pin_at = pins.values.map(p => join(\"@\", p.app, p.at)).join(\",\")\n\
               bar B {\n\
                 box { on click { m[3] = \"c\"; m[1] = \"a\"; m[2] = \"B\" } }\n\
                 box { on click { m.remove(2); m.remove(7) } }\n\
                 box { on click { m = m.filter(e => e.key > 1) } }\n\
                 box { on click { m.clear() } }\n\
                 box { on click { pins[\"a\"].at = 5 } }\n\
                 box { on click { pins[\"z\"].at = 9 } }\n\
                 box { on click { m[4] = \"d\" } }\n\
                 box { on click { m.remove(9) } }\n\
               }\n\
               state writes = 0\n\
               on change m { writes += 1 }\n";
    let (rt, host, inst) = run(src);
    host.set(
        &rt,
        "screens.all",
        Value::list(vec![
            host.record("Screen", &[("name", Value::text("DP-1"))]),
        ]),
    )
    .unwrap();
    inst.flush();
    let boxes = inst.nodes_handling("click");
    assert_eq!(boxes.len(), 8);
    let click = |i: usize| {
        inst.event(boxes[i], "click", Vec::new());
        inst.flush()
    };
    assert_eq!(get(&inst, "shown"), Value::text("2=b"));
    click(0);
    assert_eq!(get(&inst, "shown"), Value::text("1=a,2=B,3=c"));
    let u = click(1);
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    assert_eq!(get(&inst, "shown"), Value::text("1=a,3=c"));
    click(2);
    assert_eq!(get(&inst, "shown"), Value::text("3=c"));
    click(3);
    assert_eq!(get(&inst, "shown"), Value::text(""));
    assert_eq!(get(&inst, "pin_at"), Value::text("a@1"));
    click(4);
    assert_eq!(get(&inst, "pin_at"), Value::text("a@5"));
    let u = click(5);
    assert_eq!(u.errors.len(), 1, "{:?}", u.errors);
    assert!(
        u.errors[0]
            .error
            .to_string()
            .contains("no entry for this key"),
        "{:?}",
        u.errors
    );
    assert_eq!(get(&inst, "pin_at"), Value::text("a@5"));
    // Removing a key it does not hold writes nothing: `on change` does
    // not fire.
    click(6);
    let writes = get(&inst, "writes");
    let u = click(7);
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    assert_eq!(get(&inst, "shown"), Value::text("4=d"));
    assert_eq!(get(&inst, "writes"), writes);
}

/// (dictionary) `==` on dictionaries is structural: the same keys with
/// equal values, however the literals were written (there is no
/// insertion order to tell them apart); the derived equality agrees, so
/// the graph's cut-off does too.
#[test]
fn dictionaries_compare_by_their_entries() {
    let src = "let a: [int: text] = [1: \"x\", 2: \"y\"]\n\
               let b: [int: text] = [2: \"y\", 1: \"x\"]\n\
               let c: [int: text] = [1: \"x\", 2: \"z\"]\n\
               let d: [int: text] = [1: \"x\"]\n\
               let e: [int: text] = []\n\
               let same = a == b\n\
               let other_value = a == c\n\
               let fewer = a != d\n\
               let empty = e == []\n\
               let ints = [1: 2]\n\
               let floats: [int: float] = [1: 2.0]\n\
               let widened = ints == floats\n";
    let (_, _, inst) = run(src);
    assert_eq!(get(&inst, "same"), Value::Bool(true));
    assert_eq!(get(&inst, "other_value"), Value::Bool(false));
    assert_eq!(get(&inst, "fewer"), Value::Bool(true));
    assert_eq!(get(&inst, "empty"), Value::Bool(true));
    // Values compare as `==` compares them: `2 == 2.0`.
    assert_eq!(get(&inst, "widened"), Value::Bool(true));
    assert_eq!(get(&inst, "a"), get(&inst, "b"));
}
