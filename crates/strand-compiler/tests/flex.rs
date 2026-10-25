//! (flex) The language additions of the 2026-10-24 gap review
//! (decisions.md `## flex`): math builtins and `pi`, `number`, text
//! `slice`/`index_of`, `num` and `bytes`, lists joined with `+` and
//! `reduce`, the `min` and `h` duration units, a whole-number literal on
//! the left of arithmetic, a time signal in a canvas `draw:` refused at
//! check time, and an action whose command's output is its value.

use std::rc::Rc;
use std::sync::Arc;

use strand_compiler::diagnostic::{Style, render};
use strand_compiler::instantiate::Instance;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_compiler::vm::{Num, Value};
use strand_compiler::{SourceMap, lower};
use strand_core::Runtime;

fn compile(src: &str) -> (strand_compiler::Compiled, SourceMap) {
    let mut map = SourceMap::new();
    map.add("flex.strand", src.to_string());
    let c = strand_compiler::compile(&map);
    (c, map)
}

fn run(src: &str) -> (Runtime, Rc<SchemaHost>, Instance) {
    let (c, map) = compile(src);
    assert_eq!(
        c.errors(),
        0,
        "{}",
        render(&c.diagnostics, &map, Style::Plain)
    );
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
    inst.value_of("flex", name).unwrap()
}

fn float(inst: &Instance, name: &str) -> f64 {
    get(inst, name)
        .as_f64()
        .unwrap_or_else(|| panic!("{name}: {:?}", get(inst, name)))
}

fn text(inst: &Instance, name: &str) -> String {
    match get(inst, name) {
        Value::Text(t) => t.to_string(),
        v => panic!("{name}: {v:?}"),
    }
}

/// The type the checker gave declaration `name`.
fn ty(src: &str, name: &str) -> String {
    let (c, map) = compile(src);
    assert_eq!(
        c.errors(),
        0,
        "{}",
        render(&c.diagnostics, &map, Style::Plain)
    );
    let p = &c.program;
    let d = p
        .defs
        .iter()
        .find(|d| d.name == name)
        .unwrap_or_else(|| panic!("no def {name}"));
    p.types.show(&d.ty).to_string()
}

/// The diagnostics of `src`, rendered.
fn diags(src: &str) -> (Vec<&'static str>, String) {
    let (c, map) = compile(src);
    (
        c.diagnostics.iter().map(|d| d.code).collect(),
        render(&c.diagnostics, &map, Style::Plain),
    )
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

/// `sin`, `cos`, `tan` take angles (a plain number is degrees), `atan2`
/// gives one, 0deg pointing right; `sqrt`, `pow`, `exp`, `ln` and `pi`
/// are the usual ones, and a result with no value is an error value.
#[test]
fn math_builtins_compute() {
    let (_, _, inst) = run("let s = sin(30deg)\n\
         let c = cos(60)\n\
         let t = tan(45deg)\n\
         let a = atan2(1, -1)\n\
         let down = atan2(1, 0)\n\
         let r = sqrt(2)\n\
         let p = pow(2, 10)\n\
         let e = exp(1)\n\
         let l = ln(exp(2))\n\
         let tau = 2 * pi\n\
         let x = 10 * cos(atan2(3, 4))\n");
    assert!(close(float(&inst, "s"), 0.5));
    assert!(close(float(&inst, "c"), 0.5));
    assert!(close(float(&inst, "t"), 1.0));
    assert_eq!(get(&inst, "a"), Value::Num(135.0, Num::Deg));
    assert_eq!(
        get(&inst, "down"),
        Value::Num(90.0, Num::Deg),
        "clockwise, y down"
    );
    assert!(close(float(&inst, "r"), std::f64::consts::SQRT_2));
    assert_eq!(float(&inst, "p"), 1024.0);
    assert!(close(float(&inst, "e"), std::f64::consts::E));
    assert!(close(float(&inst, "l"), 2.0));
    assert!(close(float(&inst, "tau"), std::f64::consts::TAU));
    assert!(close(float(&inst, "x"), 8.0));
    assert_eq!(ty("let a = atan2(1, 2)\n", "a"), "angle");
    assert_eq!(ty("let k = pi * 2\n", "k"), "float");
    // A square root of a negative number has no value: an error value
    // naming the call, as `1 / 0` is.
    let (c, map) = compile("state x = -4.0\nlet r = sqrt(x)\n");
    assert_eq!(
        c.errors(),
        0,
        "{}",
        render(&c.diagnostics, &map, Style::Plain)
    );
    let p = Arc::new(lower::lower(
        &c.program,
        strand_compiler::schema::Schema::builtin(),
    ));
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &p.types));
    let inst = Instance::new(&rt, p, host, strand_compiler::instantiate::Storage::none());
    inst.flush();
    let err = inst.value_of("flex", "r").unwrap_err();
    assert!(err.to_string().contains("`sqrt(-4)` has no value"), "{err}");
}

/// `number(t)` reads the number a text spells, or null; `num` formats
/// with fixed decimals and zero padding; `bytes` in binary units.
#[test]
fn text_and_number_helpers() {
    let (_, _, inst) = run("let a = number(\" 42 \")\n\
         let b = number(\"-0.5\")\n\
         let c = number(\"1e3\")\n\
         let d = number(\"4 GB\")\n\
         let e = number(\"inf\")\n\
         let f = number(\"\") ?? -1\n\
         let n1 = num(3.14159, digits: 2)\n\
         let n2 = num(7, pad: 2)\n\
         let n3 = num(-2.5)\n\
         let n4 = num(-0.004, digits: 2)\n\
         let n5 = num(9.999, digits: 1, pad: 3)\n\
         let b1 = bytes(512)\n\
         let b2 = bytes(1536)\n\
         let b3 = bytes(20830368563)\n\
         let b4 = bytes(512 * 1024 * 1024)\n\
         let b5 = bytes(1024)\n\
         let n6 = num(1.005, digits: 2)\n\
         let n7 = num(0.995, digits: 2)\n\
         let b6 = bytes(1023.6)\n\
         let b7 = bytes(1048575)\n\
         let b8 = bytes(1048524)\n\
         let b9 = bytes(1048000)\n");
    assert_eq!(get(&inst, "a"), Value::float(42.0));
    assert_eq!(get(&inst, "b"), Value::float(-0.5));
    assert_eq!(get(&inst, "c"), Value::float(1000.0));
    assert_eq!(get(&inst, "d"), Value::Null);
    assert_eq!(get(&inst, "e"), Value::Null, "not a number here");
    assert_eq!(float(&inst, "f"), -1.0);
    assert_eq!(text(&inst, "n1"), "3.14");
    assert_eq!(text(&inst, "n2"), "07");
    assert_eq!(text(&inst, "n3"), "-3", "half away from zero");
    assert_eq!(text(&inst, "n4"), "0.00", "never -0");
    assert_eq!(text(&inst, "n5"), "010.0");
    assert_eq!(text(&inst, "b1"), "512 B");
    assert_eq!(text(&inst, "b2"), "1.5 KiB");
    assert_eq!(text(&inst, "b3"), "19.4 GiB");
    assert_eq!(text(&inst, "n6"), "1.01", "on the decimals written");
    assert_eq!(text(&inst, "n7"), "1.00", "carried into the whole part");
    assert_eq!(text(&inst, "b6"), "1 KiB", "the unit after rounding");
    assert_eq!(text(&inst, "b7"), "1 MiB");
    assert_eq!(text(&inst, "b8"), "1 MiB");
    assert_eq!(text(&inst, "b9"), "1023 KiB");
    assert_eq!(text(&inst, "b4"), "512 MiB");
    assert_eq!(text(&inst, "b5"), "1 KiB");
    assert_eq!(ty("let a = number(\"1\")\n", "a"), "float?");
    // Null in, null out.
    let (_, _, inst) = run("state s: text? = null\nlet a = number(s)\nlet b = num(number(s))\n");
    assert_eq!(get(&inst, "a"), Value::Null);
    assert_eq!(get(&inst, "b"), Value::Null);
}

/// Text `slice` and `index_of` count characters, as `len` does.
#[test]
fn text_slices_by_characters() {
    let (_, _, inst) = run("let s = \"héllo 😀 world\"\n\
         let a = s.slice(1, 3)\n\
         let b = s.slice(6)\n\
         let c = s.slice(-5)\n\
         let d = s.slice(4, 2)\n\
         let e = s.slice(0, 100)\n\
         let i = s.index_of(\"😀\")\n\
         let j = s.index_of(\"xyz\") ?? -1\n\
         let w = s.slice(s.index_of(\" \") ?? 0).trim()\n\
         let n = s.len\n");
    assert_eq!(text(&inst, "a"), "él");
    assert_eq!(text(&inst, "b"), "😀 world");
    assert_eq!(text(&inst, "c"), "world");
    assert_eq!(text(&inst, "d"), "");
    assert_eq!(text(&inst, "e"), "héllo 😀 world");
    assert_eq!(get(&inst, "i"), Value::int(6));
    assert_eq!(float(&inst, "j"), -1.0);
    assert_eq!(text(&inst, "w"), "😀 world");
    assert_eq!(get(&inst, "n"), Value::int(13));
    assert_eq!(ty("let i = \"ab\".index_of(\"b\")\n", "i"), "int?");
}

/// `+` joins two lists of one item type, in a binding and with `+=` in
/// a handler, so a pure `fn` builds any list (here a range).
#[test]
fn lists_join_with_plus() {
    let (_, _, inst) = run(
        "fn range(n: int) -> [int] { n <= 0 ? [] : range(n - 1) + [n - 1] }\n\
         state xs = [1, 2]\n\
         let both = xs + [3]\n\
         let r = range(4)\n\
         let front = [] + xs\n\
         let mixed = [0.5] + xs\n\
         component C { box { on click { xs += [9] } } }\n",
    );
    assert_eq!(
        get(&inst, "both"),
        Value::list(vec![Value::int(1), Value::int(2), Value::int(3)])
    );
    assert_eq!(
        get(&inst, "r"),
        Value::list((0..4).map(Value::int).collect())
    );
    assert_eq!(get(&inst, "front"), get(&inst, "xs"));
    assert_eq!(ty("state xs = [1]\nlet b = xs + [2]\n", "b"), "[int]");
    assert_eq!(ty("state xs = [1]\nlet f = [] + xs\n", "f"), "[int]");
    assert_eq!(ty("state xs = [1]\nlet m = [0.5] + xs\n", "m"), "[float]");
    let (codes, text) = diags("state xs = [1]\nlet bad = xs + [\"a\"]\n");
    assert_eq!(codes, ["check::type_mismatch"], "{text}");
    assert!(text.contains("a list item expects `int`"), "{text}");
    let (codes, text) = diags("state xs = [1]\nstate ys = [\"a\"]\nlet bad = xs + ys\n");
    assert_eq!(codes, ["check::type_mismatch"], "{text}");
    assert!(text.contains("`+` joins lists of one item type"), "{text}");
    let (codes, text) = diags("state xs = [1]\nlet bad = xs + 1\n");
    assert_eq!(codes, ["check::type_mismatch"], "{text}");
}

/// `xs.reduce(init, f)` folds in order and is typed by `init`; a whole
/// `init` becomes a `float` when `f` gives fractions.
#[test]
fn reduce_folds_in_order() {
    let src = "type P { name: text; cpu: float }\n\
         state ps: [P] = [P(name: \"a\", cpu: 1.5), P(name: \"b\", cpu: 2.25)]\n\
         state xs = [3, 1, 4]\n\
         let total = xs.reduce(0, (s, x) => s + x)\n\
         let cpu = ps.reduce(0, (s, p) => s + p.cpu)\n\
         let names = ps.reduce(\"\", (acc, p) => acc + p.name)\n\
         let top = xs.reduce(0, (m, x) => max(m, x))\n\
         let none = [].reduce(7, (a, x) => a)\n";
    let (_, _, inst) = run(src);
    assert_eq!(get(&inst, "total"), Value::int(8));
    assert_eq!(float(&inst, "cpu"), 3.75);
    assert_eq!(text(&inst, "names"), "ab");
    assert_eq!(float(&inst, "top"), 4.0);
    assert_eq!(get(&inst, "none"), Value::int(7));
    assert_eq!(ty(src, "total"), "int");
    assert_eq!(ty(src, "cpu"), "float");
    assert_eq!(ty(src, "names"), "text");
    let (codes, text) = diags("state xs = [1]\nlet bad = xs.reduce(\"\", (a, x) => a + x)\n");
    assert!(codes.contains(&"check::type_mismatch"), "{text}");
}

/// Durations take `min` and `h` beside `s` and `ms`.
#[test]
fn minutes_and_hours_are_durations() {
    let (_, _, inst) = run("let a = 10min\nlet b = 3h\nlet c = 1.5h + 30min\nlet d = dur(90min)\n");
    assert_eq!(get(&inst, "a"), Value::Num(600_000.0, Num::Ms));
    assert_eq!(get(&inst, "b"), Value::Num(10_800_000.0, Num::Ms));
    assert_eq!(get(&inst, "c"), Value::Num(7_200_000.0, Num::Ms));
    assert_eq!(text(&inst, "d"), "1 h 30 min");
    assert_eq!(ty("let a = 2h\n", "a"), "duration");
    // In a graph's history and a timer, as any duration.
    let (codes, text) =
        diags("state n = 0\nevery 5min { n += 1 }\nbar B { graph cpu.usage { history: 3h } }\n");
    assert!(codes.is_empty(), "{text}");
}

/// (critic:int-literal-arith) A whole-number literal on the left of
/// arithmetic takes the right operand's type, as one on the right takes
/// the left's: `2500 + n` is an `int` like `n + 2500`. The help for a
/// `float` where an `int` goes names `round()` with its parentheses.
#[test]
fn a_literal_on_the_left_is_an_int_beside_an_int() {
    let src = "state n = 3\nlet a = n + 2500\nlet b = 2500 + n\nlet c = 2 * n - 1\nlet d = -1 + n\nlet f = 2500 + n * 0.5\n";
    assert_eq!(ty(src, "a"), "int");
    assert_eq!(ty(src, "b"), "int");
    assert_eq!(ty(src, "c"), "int");
    assert_eq!(ty(src, "d"), "int");
    assert_eq!(ty(src, "f"), "float");
    let (codes, text) = diags(
        "permit exec \"say\"\naction say(k: int) [\"say\", k]\nstate v = 0.5\n\
         let k = 2500 + v * 4000\n\
         component C { box { on click { say(k) } } }\n",
    );
    assert_eq!(codes, ["check::type_mismatch"], "{text}");
    assert!(text.contains("round it here: `k.round()`"), "{text}");
}

/// A time signal in a canvas's `draw:` is a load error (it faulted at
/// run time before), however it reaches the lambda: `t`, `wave(…)`,
/// `noise` of `t`, or a `let` holding one. Elsewhere it is fine.
#[test]
fn a_time_signal_in_a_draw_is_refused_at_check() {
    for (draw, what) in [
        ("c.circle(20, 20, 10 + 5 * wave(1s))", "`wave(…)`"),
        ("c.rect(t * 10, 0, 4, 4)", "`t`"),
        ("{ let r = noise(t); c.circle(0, 0, r) }", "`t`"),
        ("c.line(0, 0, spin, 10)", "this value"),
        ("{ if wave(2s) > 0 { c.fill($accent) } }", "`wave(…)`"),
    ] {
        let src =
            format!("let spin = t * 90\nbar B {{ canvas {{ size: 40; draw: (c) => {draw} }} }}\n");
        let (codes, text) = diags(&src);
        assert_eq!(codes, ["check::time_in_canvas"], "{draw}: {text}");
        assert!(text.contains(what), "{draw}: {text}");
        assert!(text.contains("a `shader`"), "{text}");
    }
    // Animating the canvas node itself, and a draw reading no time, check
    // clean.
    let (codes, text) = diags(
        "state v = 0.5\nbar B { canvas { size: 40; rotate: t * 90deg; opacity: wave(2s); \
         draw: (c) => { c.circle(20, 20, 10 * v); c.fill($accent) } } }\n",
    );
    assert!(codes.is_empty(), "{text}");
}

/// (flex review) The maths builtins take a time signal in a prop: the
/// call stays symbolic and render evaluates it per frame (`x: 20 *
/// sin(t * 90deg)` bobs), as `wave` does. A builtin that computes with
/// its arguments now (`pct`, `num`, `dur`, …) refuses one at check
/// time, where it faulted at run time.
#[test]
fn maths_follow_time_and_other_builtins_refuse_it() {
    let (codes, text) = diags(
        "let s = sin(t * 90deg)\n\
         bar B { box { x: 20 * s; y: 10 * cos(t * 45deg) + max(0, sin(t * 30deg)); \
         rotate: atan2(sin(t * 90deg), 1) } }\n",
    );
    assert!(codes.is_empty(), "{text}");
    let (_, _, inst) = run("let s = 20 * sin(t * 90deg)\nlet m = clamp(wave(2s), 0.2, 0.8)\n");
    let Value::Time(e) = get(&inst, "s") else {
        panic!("{:?}", get(&inst, "s"));
    };
    // At t = 1 s: 20 · sin(90°) = 20.
    let tables = strand_scene::TokenTable::default();
    let at = |e: &strand_scene::TokenExpr, t: f32| {
        let levels = [&tables];
        let scope = strand_scene::TokenScope::new(&levels)
            .with_time(Some(strand_scene::TimeContext::at(t)));
        match scope
            .resolve(&strand_scene::PropValue::Token(e.clone()))
            .map(|v| v.into_owned())
        {
            Some(strand_scene::PropValue::Number(n)) => n,
            v => panic!("{v:?}"),
        }
    };
    assert!((at(&e, 1.0) - 20.0).abs() < 1e-3);
    assert!(at(&e, 0.0).abs() < 1e-3);
    let Value::Time(m) = get(&inst, "m") else {
        panic!("{:?}", get(&inst, "m"));
    };
    assert_eq!(at(&m, 0.0), 0.2, "clamped up from 0");
    for (call, what) in [
        ("pct(wave(2s))", "`wave(…)`"),
        ("num(t, digits: 1)", "`t`"),
        ("bytes(s * 1000)", "this value"),
    ] {
        let (codes, text) = diags(&format!(
            "let s = sin(t * 90deg)\nbar B {{ text {call} }}\n"
        ));
        assert_eq!(codes, ["check::time_in_logic"], "{call}: {text}");
        assert!(text.contains(what), "{call}: {text}");
    }
}

/// (flex review) The maths builtins take degrees: a bare `pi` in their
/// angle is warned, with `180deg` as the fix.
#[test]
fn pi_in_an_angle_is_warned() {
    let src = "let a = sin(pi / 2)\n";
    let (codes, text) = diags(src);
    assert_eq!(codes, ["check::pi_degrees"], "{text}");
    assert!(text.contains("`sin` takes degrees"), "{text}");
    let (c, _) = compile(src);
    let fix = &c.diagnostics[0].suggestions[0];
    assert_eq!(&src[fix.span.start as usize..fix.span.end as usize], "pi");
    assert_eq!(fix.replacement, "180deg");
    for ok in [
        "let a = sin(180deg / 2)\n",
        "let a = sqrt(pi)\n",
        "let a = 2 * pi * 3\n",
        "let a = cos(90)\n",
    ] {
        let (codes, text) = diags(ok);
        assert!(codes.is_empty(), "{ok}: {text}");
    }
}

/// (flex review) An `arc` that set its line with `width` (before its
/// line became its `stroke:`) is warned, with `stroke` as the fix; one
/// sized by `width` and `height`, or with a `stroke:`, is not.
#[test]
fn an_arc_with_its_old_width_is_warned() {
    let old = "bar B { arc { value: 0.5; sweep: 270deg; size: 56; width: 5 } }\n";
    let (codes, text) = diags(old);
    assert_eq!(codes, ["check::arc_width"], "{text}");
    assert!(text.contains("line is its `stroke:`"), "{text}");
    let (c, _) = compile(old);
    let fix = &c.diagnostics[0].suggestions[0];
    assert_eq!(
        &old[fix.span.start as usize..fix.span.end as usize],
        "width"
    );
    assert_eq!(fix.replacement, "stroke");
    for ok in [
        "bar B { arc { value: 0.5; size: 56; stroke: 5 } }\n",
        "bar B { arc { value: 0.5; width: 40; height: 40; stroke: 4 } }\n",
        "bar B { arc { value: 0.5; width: 40; height: 40 } }\n",
    ] {
        let (codes, text) = diags(ok);
        assert!(codes.is_empty(), "{ok}: {text}");
    }
}

/// (flex) An action with a result is an `Async<T>`: loaded in a binding
/// as `apps.search(q)` is (and refused where it could not answer), awaited
/// in a handler; one returning nothing stays an action.
#[test]
fn an_action_with_a_result_is_typed_async() {
    let head = "permit exec \"qalc\"\naction calc(q: text) -> text [\"qalc\", \"-t\", q]\n";
    let src = format!("{head}state query = \"1+1\"\nlet r = calc(query)\nlet shown = r ?? \"…\"\n");
    assert_eq!(ty(&src, "r"), "Async<text>");
    assert_eq!(ty(&src, "shown"), "text");
    let rec = "permit exec \"probe\"\ntype Probe { load: float; name: text }\n\
         action probe(id: int) -> Probe? [\"probe\", id]\nlet p = probe(3)\n"
        .to_string();
    assert_eq!(ty(&rec, "p"), "Async<Probe?>");
    // Awaited in a handler.
    let (codes, text) = diags(&format!(
        "{head}state out = \"\"\nstate q = \"\"\non change q {{ out = await calc(q) }}\n"
    ));
    assert!(codes.is_empty(), "{text}");
    // A pure `fn` runs no command.
    let (codes, text) = diags(&format!(
        "{head}fn f(x: text) -> text {{ calc(x) ?? \"\" }}\n"
    ));
    assert!(codes.contains(&"check::impure"), "{text}");
    assert!(text.contains("`calc` runs a command"), "{text}");
    // A binding's lambda would never get its answer.
    let (codes, text) = diags(&format!(
        "{head}state qs = [\"1\", \"2\"]\nlet rs = qs.map(q => calc(q) ?? \"\")\n"
    ));
    assert!(codes.contains(&"check::action_in_binding"), "{text}");
    // What a command prints cannot be a function, nor a live handle
    // (what a `from poll` field cannot hold; `File` and its `Date` are
    // data a document may hold, services-round).
    for ret in ["font", "Window", "Canvas", "Screen", "[Window]"] {
        let (codes, text) = diags(&format!("permit exec \"x\"\naction f() -> {ret} [\"x\"]\n"));
        assert!(codes.contains(&"check::type_mismatch"), "{ret}: {text}");
        assert!(text.contains("cannot be read as"), "{ret}: {text}");
    }
    // A result action called for its effect alone drops its call (and
    // ends its command): refused, as a dropped `sleep` is.
    let (codes, text) = diags(&format!(
        "{head}state q = \"\"\non change q {{ calc(q) }}\n"
    ));
    assert_eq!(codes, ["check::async"], "{text}");
    assert!(
        text.contains("is dropped") && text.contains("`await` it"),
        "{text}"
    );
    // A D-Bus method's action has none.
    let (codes, text) = diags(
        "service ppd from dbus system \"net.hadess.PowerProfiles\" {\n  \
         profile: text = ActiveProfile\n  action hold(p: text) -> text = HoldProfile\n}\n",
    );
    assert!(codes.contains(&"check::misplaced"), "{text}");
    // An action with no result is still refused in a binding.
    let (codes, _) =
        diags("permit exec \"say\"\naction say(t: text) [\"say\", t]\nlet x = say(\"hi\")\n");
    assert!(codes.contains(&"check::action_in_binding"));
}

/// (flex) In a binding, the call asks the host for its command with an
/// answer slot; the answer settles the `let`; newer arguments ask again
/// and abandon the older call (the host ends its command). In a
/// handler, `await` waits for the answer.
#[test]
fn an_action_with_a_result_is_loaded_and_awaited() {
    let src = "permit exec \"qalc\"\n\
        action calc(q: text) -> text [\"qalc\", \"-t\", q]\n\
        export state query = \"1+1\"\n\
        let r = calc(query)\n\
        let shown = r ?? \"…\"\n\
        let pending = r.pending\n\
        export state asked = \"\"\n\
        state out = \"\"\n\
        on change asked { out = await calc(asked) }\n";
    let (_rt, _host, inst) = run(src);
    // Nothing reads `r` yet: a `let` is lazy. Reading it makes its load,
    // which asks on the next flush.
    assert_eq!(text(&inst, "shown"), "…");
    inst.flush();
    assert_eq!(
        get(&inst, "pending"),
        Value::Bool(true),
        "{:?}",
        get(&inst, "r")
    );
    let reqs = inst.take_execs();
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    assert_eq!(reqs[0].action, "calc");
    assert_eq!(reqs[0].argv, ["qalc", "-t", "1+1"]);
    let first = reqs[0].reply.clone().expect("a reply slot");
    assert_eq!(
        first.ty(),
        &strand_compiler::ty::Ty::Prim(strand_compiler::ty::Prim::Text)
    );
    first.answer(Ok(Value::text("2")));
    inst.flush();
    assert_eq!(text(&inst, "shown"), "2");
    assert_eq!(get(&inst, "pending"), Value::Bool(false));
    // New arguments: asked again; the older call is abandoned once its
    // load is superseded (here it answered already, so nothing to end).
    inst.set("flex.query", Value::text("2*3")).unwrap();
    inst.flush();
    assert_eq!(
        text(&inst, "shown"),
        "2",
        "the last value is kept while pending"
    );
    let reqs = inst.take_execs();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].argv, ["qalc", "-t", "2*3"]);
    let second = reqs[0].reply.clone().unwrap();
    inst.set("flex.query", Value::text("7")).unwrap();
    inst.flush();
    let reqs = inst.take_execs();
    assert_eq!(reqs[0].argv, ["qalc", "-t", "7"]);
    assert!(second.abandoned(), "superseded: its command can end");
    // A failure is the `Async`'s error, its last value kept.
    let third = reqs[0].reply.clone().unwrap();
    third.answer(Err("`qalc` failed (exit status: 1)".into()));
    inst.flush();
    assert_eq!(text(&inst, "shown"), "2");
    let (_, _, inst2) = run(&format!("{src}let err = r.error ?? \"\"\n"));
    let _ = text(&inst2, "err");
    inst2.flush();
    let r = inst2.take_execs().remove(0).reply.unwrap();
    r.answer(Err("`qalc` failed (exit status: 1)".into()));
    inst2.flush();
    assert_eq!(text(&inst2, "err"), "`qalc` failed (exit status: 1)");
    // In a handler: `await` waits for the answer.
    inst.set("flex.asked", Value::text("9/3")).unwrap();
    inst.flush();
    let reqs = inst.take_execs();
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    assert_eq!(reqs[0].argv, ["qalc", "-t", "9/3"]);
    assert_eq!(text(&inst, "out"), "");
    reqs[0].reply.as_ref().unwrap().answer(Ok(Value::text("3")));
    inst.flush();
    assert_eq!(text(&inst, "out"), "3");
}
