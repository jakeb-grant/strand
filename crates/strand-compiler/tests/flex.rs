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

/// (json-fn) `json(value)` writes plain data as compact JSON: a record
/// by its fields in order, nested lists and records, a dictionary as an
/// object of its keys' texts, null, an enum's name, a colour `#rrggbb`
/// (`#rrggbbaa` translucent), a duration's milliseconds, an `int` whole
/// and a float in its shortest form; a text escaped as JSON requires, other characters kept. What it
/// writes reads back as the same data.
#[test]
fn json_writes_plain_data() {
    let (_, _, inst) = run("enum Look { light, dark_mode }\n\
         type Mon { id: text; scale: float; on: bool; look: Look; note: text? }\n\
         let m = Mon(id: \"DP-1 \\\"é\\\"\\n\\ttab\\\\\", scale: 1.25, on: true, look: dark_mode, note: null)\n\
         let j1 = json(m)\n\
         let j2 = json([m, Mon(id: \"x\", scale: 2, on: false, look: light, note: \"n\")])\n\
         let j3 = json([\"b\": [1, 2], \"a\": []])\n\
         let j4 = json([3: \"x\"])\n\
         let j5 = json(null)\n\
         let j6 = json(1500ms)\n\
         let j7 = json(0.1 + 0.2)\n\
         let j8 = json(7)\n\
         let j9 = json(#ff8800)\n\
         let j10 = json(#ff880080)\n");
    let j1 = text(&inst, "j1");
    assert_eq!(
        j1,
        r#"{"id":"DP-1 \"é\"\n\ttab\\","scale":1.25,"on":true,"look":"dark_mode","note":null}"#
    );
    let back: serde_json::Value = serde_json::from_str(&j1).unwrap();
    assert_eq!(back["id"], "DP-1 \"é\"\n\ttab\\");
    assert_eq!(
        text(&inst, "j2"),
        format!(r#"[{j1},{{"id":"x","scale":2,"on":false,"look":"light","note":"n"}}]"#)
    );
    assert_eq!(text(&inst, "j3"), r#"{"a":[],"b":[1,2]}"#, "in key order");
    assert_eq!(text(&inst, "j4"), r#"{"3":"x"}"#);
    assert_eq!(text(&inst, "j5"), "null");
    assert_eq!(text(&inst, "j6"), "1500");
    assert_eq!(text(&inst, "j7"), "0.30000000000000004");
    assert_eq!(text(&inst, "j8"), "7");
    assert_eq!(text(&inst, "j9"), r##""#ff8800""##);
    assert_eq!(text(&inst, "j10"), r##""#ff880080""##, "translucent");
    assert_eq!(ty("let a = json([1])\n", "a"), "text");
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

/// (doc-formats) `plain(markup)` is the text `markup:` shows, without its
/// styles, and `links(markup)` its links in order, as written; `none` is
/// the text as it is.
#[test]
fn text_plain_and_links_read_markup() {
    let (_, _, inst) = run("let h = \"<p>a &amp; b</p><ul><li>x</li></ul>\"\n\
         let a = h.plain(html)\n\
         let b = \"a <b>b</b> Vec<T>\".plain(basic)\n\
         let c = h.plain(none)\n\
         let d = \"<a href='/rel'>r</a> <a href=\\\"https://x.org/?a=1&amp;b=2\\\">x</a>\".links(html)\n\
         let e = \"<a href=x>a</a> <b>b</b>\".links(basic)\n\
         let f = h.links(none)\n\
         state n: text? = null\n\
         let g = n?.plain(html)\n\
         let i = h.plain(html).contains(\"a & b\")\n\
         let j = \"caf&eacute;&nbsp;&#8212; ok\".plain(html)\n");
    assert_eq!(text(&inst, "a"), "a & b\n\n• x");
    assert_eq!(text(&inst, "b"), "a b Vec<T>");
    assert_eq!(text(&inst, "c"), "<p>a &amp; b</p><ul><li>x</li></ul>");
    assert_eq!(
        get(&inst, "d"),
        Value::list(vec![
            Value::text("/rel"),
            Value::text("https://x.org/?a=1&b=2")
        ])
    );
    assert_eq!(get(&inst, "e"), Value::list(vec![Value::text("x")]));
    assert_eq!(get(&inst, "f"), Value::list(vec![]));
    assert_eq!(get(&inst, "g"), Value::Null);
    assert_eq!(get(&inst, "i"), Value::Bool(true));
    assert_eq!(text(&inst, "j"), "café — ok");
    assert_eq!(ty("let a = \"x\".plain(html)\n", "a"), "text");
    assert_eq!(ty("let a = \"x\".links(html)\n", "a"), "[text]");
    assert_eq!(
        ty("state n: text? = null\nlet a = n?.plain(basic)\n", "a"),
        "text?"
    );
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

/// (panel-change-closed) The `on change` handlers written in the body of
/// a surface that starts closed exist from the start and read what they
/// watch while it is closed (spec.md 5.1): one watching a value from an
/// action with a result runs that action before the panel first opens,
/// and again when its arguments change. Nothing else in the closed body
/// reads it.
#[test]
fn a_closed_panels_body_on_change_reads_what_it_watches() {
    let src = "permit exec \"qalc\"\n\
        action calc(q: text) -> text [\"qalc\", \"-t\", q]\n\
        export state query = \"1+1\"\n\
        export state open = false\n\
        state sel = 0\n\
        let r = calc(query)\n\
        let answer = r ?? \"\"\n\
        panel P {\n  open: <-> open\n  on change answer { sel = 0 }\n  text answer\n}\n";
    let (_rt, _host, inst) = run(src);
    inst.flush();
    let argv = |inst: &Instance| -> Vec<Vec<String>> {
        inst.take_execs().into_iter().map(|r| r.argv).collect()
    };
    assert_eq!(argv(&inst), [["qalc", "-t", "1+1"]], "read at boot");
    inst.set("flex.query", Value::text("2*3")).unwrap();
    inst.flush();
    assert_eq!(argv(&inst), [["qalc", "-t", "2*3"]], "and while closed");
}

/// (lockmon) An action's name reads as its run: `rec.running` (`bool`),
/// `rec.started` (`Date?`) in a binding, `rec.stop()` an action of a
/// handler; the bare name is still no value, the run is not written, and
/// a local of the name hides it.
#[test]
fn an_actions_run_is_read_through_its_name() {
    let head = "permit exec \"wf-recorder\"\npermit exec \"qalc\"\n\
        action rec(file: path) [\"wf-recorder\", \"-f\", file]\n\
        action calc(q: text) -> text [\"qalc\", \"-t\", q]\n";
    let src = format!(
        "{head}let on = rec.running\nlet since = rec.started\nlet busy = calc.running\n\
         let shown = rec.started?.format(\"%H:%M\") ?? \"\"\n"
    );
    assert_eq!(ty(&src, "on"), "bool");
    assert_eq!(ty(&src, "since"), "Date?");
    assert_eq!(ty(&src, "busy"), "bool");
    assert_eq!(ty(&src, "shown"), "text");
    let (codes, text) = diags(&format!(
        "{head}state n = 0\non change n {{ rec.stop(); rec(\"/tmp/a.mp4\") }}\n\
         on change rec.running {{ n = 1 }}\n"
    ));
    assert!(codes.is_empty(), "{text}");
    // `stop()` changes something: a handler's alone.
    let (codes, text) = diags(&format!("{head}let x = rec.stop()\n"));
    assert!(codes.contains(&"check::action_in_binding"), "{text}");
    assert!(text.contains("`rec.stop` is an action"), "{text}");
    // A typo is offered the field.
    let (codes, text) = diags(&format!("{head}let x = rec.runing\n"));
    assert_eq!(codes, ["check::unknown_field"], "{text}");
    assert!(text.contains("`rec` has no field `runing`"), "{text}");
    assert!(text.contains("running"), "{text}");
    let (codes, text) = diags(&format!("{head}let x = rec.stop\n"));
    assert_eq!(codes, ["check::not_a_value"], "{text}");
    // The run is the host's to say: never written.
    let (codes, text) = diags(&format!(
        "{head}state n = 0\non change n {{ rec.running = true }}\n"
    ));
    assert!(!codes.is_empty(), "{text}");
    // A parameter called like the action is the parameter.
    let (codes, text) = diags(&format!(
        "{head}type Job {{ running: int }}\nfn f(rec: Job) -> int {{ rec.running }}\n"
    ));
    assert!(codes.is_empty(), "{text}");
}

/// (dbus-action-run) A `from dbus` service's action read without a call is
/// its run, an `ActionRun` as a declared command action's is:
/// `ppd.hold.running`, `.started` and `.error` read in a binding and are
/// heard by `on change`; they are never written, the run has no `stop()`
/// (a method call cannot be taken back), a field of the service keeps
/// its name, and a name that is neither is still an unknown field.
#[test]
fn a_dbus_actions_run_is_read_through_its_name() {
    let head = "service ppd from dbus system \"net.hadess.PowerProfiles\" {\n  \
         profile: text = ActiveProfile\n  action hold(p: text) = HoldProfile\n}\n";
    let src = format!(
        "{head}let busy = ppd.hold.running\nlet since = ppd.hold.started\n\
         let why = ppd.hold.error ?? \"\"\nlet p = ppd.profile\n"
    );
    assert_eq!(ty(&src, "busy"), "bool");
    assert_eq!(ty(&src, "since"), "Date?");
    assert_eq!(ty(&src, "why"), "text");
    assert_eq!(ty(&src, "p"), "text");
    let (codes, text) = diags(&format!(
        "{head}state shown = \"\"\non change ppd.hold.error {{ shown = ppd.hold.error ?? \"\" }}\n\
         on change shown {{ ppd.hold(\"performance\") }}\n"
    ));
    assert!(codes.is_empty(), "{text}");
    let (codes, text) = diags(&format!(
        "{head}state n = 0\non change n {{ ppd.hold.error = \"no\" }}\n"
    ));
    assert_eq!(codes, ["check::read_only"], "{text}");
    let (codes, text) = diags(&format!(
        "{head}state n = 0\non change n {{ ppd.hold.stop() }}\n"
    ));
    assert_eq!(codes, ["check::unknown_field"], "{text}");
    assert!(text.contains("`ppd.hold` has no method `stop`"), "{text}");
    let (codes, text) = diags(&format!("{head}let x = ppd.hold.runing\n"));
    assert_eq!(codes, ["check::unknown_field"], "{text}");
    assert!(text.contains("running"), "{text}");
    let (codes, text) = diags(&format!("{head}let x = ppd.held\n"));
    assert_eq!(codes, ["check::unknown_field"], "{text}");
}

/// (action-error) `rec.error` is what the host last said of the action's
/// failure: `text?`, tracked, null to start with (so `on change` hears only
/// real changes), and read in a binding and a handler like the run's other
/// fields; it cannot be written; another action's stays its own.
#[test]
fn an_actions_error_follows_the_host() {
    let src = "permit exec \"wf-recorder\"\n\
        action rec() [\"wf-recorder\"]\n\
        action other() [\"wf-recorder\"]\n\
        let why = rec.error ?? \"-\"\n\
        let elsewhere = other.error ?? \"-\"\n\
        export state seen = 0\n\
        on change rec.error { seen += 1 }\n";
    let (_rt, _host, inst) = run(src);
    assert_eq!(text(&inst, "why"), "-");
    assert_eq!(get(&inst, "seen").as_f64(), Some(0.0));
    inst.set_action_error("rec", Some("could not start `wf-recorder`"));
    inst.flush();
    assert_eq!(text(&inst, "why"), "could not start `wf-recorder`");
    assert_eq!(text(&inst, "elsewhere"), "-");
    assert_eq!(get(&inst, "seen").as_f64(), Some(1.0));
    inst.set_action_error("nope", Some("ignored"));
    inst.set_action_error("rec", None);
    inst.flush();
    assert_eq!(text(&inst, "why"), "-");
    assert_eq!(get(&inst, "seen").as_f64(), Some(2.0));
    let (codes, msg) = diags(
        "permit exec \"x\"\naction rec() [\"x\"]\non change rec.running { rec.error = \"no\" }\n",
    );
    assert!(!codes.is_empty(), "{msg}");
}

/// (lockmon) What the host says of an action's commands is what
/// `rec.running` and `rec.started` read (to the minute, null once none
/// runs); `rec.stop()` asks the host to stop them, in order with the
/// calls around it.
#[test]
fn an_actions_run_follows_the_host_and_stop_is_asked_in_order() {
    let src = "permit exec \"wf-recorder\"\n\
        action rec(file: path) [\"wf-recorder\", \"-f\", file]\n\
        let on = rec.running\n\
        let at = rec.started?.format(\"%H:%M\") ?? \"-\"\n\
        export state go = 0\n\
        export state seen = 0\n\
        on change rec.running { seen += 1 }\n\
        on change go { rec.stop(); rec(\"/tmp/b.mp4\") }\n";
    let (_rt, _host, inst) = run(src);
    assert_eq!(get(&inst, "on"), Value::Bool(false));
    assert_eq!(text(&inst, "at"), "-");
    assert_eq!(inst.action_running("rec"), Some(false));
    assert_eq!(inst.action_running("nope"), None);
    let t = chrono::NaiveDate::from_ymd_opt(2026, 10, 25)
        .unwrap()
        .and_hms_opt(14, 7, 33)
        .unwrap();
    inst.set_action_run("rec", true, Some(t));
    inst.flush();
    assert_eq!(get(&inst, "on"), Value::Bool(true));
    assert_eq!(text(&inst, "at"), "14:07");
    assert_eq!(get(&inst, "seen").as_f64(), Some(1.0));
    // A later second of the same minute changes nothing.
    inst.set_action_run("rec", true, Some(t + chrono::Duration::seconds(20)));
    inst.flush();
    assert_eq!(get(&inst, "seen").as_f64(), Some(1.0));
    // An action the program does not declare is ignored.
    inst.set_action_run("other", true, Some(t));
    inst.set_action_run("rec", false, Some(t));
    inst.flush();
    assert_eq!(get(&inst, "on"), Value::Bool(false));
    assert_eq!(text(&inst, "at"), "-", "no start once none runs");
    assert_eq!(get(&inst, "seen").as_f64(), Some(2.0));
    // `stop()`, then a call: asked in that order.
    inst.set("flex.go", Value::int(1)).unwrap();
    inst.flush();
    let reqs = inst.take_execs();
    assert_eq!(reqs.len(), 2, "{reqs:?}");
    assert!(reqs[0].stop && reqs[0].action == "rec" && reqs[0].argv.is_empty());
    assert!(!reqs[1].stop);
    assert_eq!(reqs[1].argv, ["wf-recorder", "-f", "/tmp/b.mp4"]);
}

/// (proc-run) What the host says of a run's output is what `rec.lines`
/// and `rec.code` read: lines replaced by number (the open line redrawn,
/// then ended), dropped from the front, emptied with `code` and `error`
/// when a run starts; one update's parts land in one flush, so `on change
/// rec.running` sees the last line, the status and why. An action with a
/// result reads `[]` and null.
#[test]
fn a_runs_lines_and_code_follow_the_hosts_updates() {
    use strand_compiler::vm::runs::{RunLine, RunUpdate};
    let line = |n: u64, text: &str, stderr: bool, open: bool| RunLine {
        n,
        text: text.into(),
        stderr,
        open,
    };
    let src = "permit exec \"pacman\", \"qalc\"\n\
        action tx() [\"pacman\", \"-Syu\"]\n\
        action calc(q: text) -> text [\"qalc\", q]\n\
        let shown = tx.lines.map(l => (l.stderr ? \"!\" : \"\") + l.text + (l.open ? \"…\" : \"\"))\n\
        let count = tx.lines.len\n\
        let first = tx.lines.first?.n ?? 0\n\
        let code = tx.code ?? -1\n\
        let other = calc.lines.len + (calc.code ?? -1)\n\
        export state ended = \"\"\n\
        on change tx.running { if !tx.running { ended = join(\"|\", tx.lines.last?.text, num(tx.code ?? -1), tx.error) } }\n";
    let (_rt, _host, inst) = run(src);
    let list = |inst: &Instance| -> Vec<String> {
        match get(inst, "shown") {
            Value::List(items) => items
                .iter()
                .map(|v| match v {
                    Value::Text(t) => t.to_string(),
                    v => format!("{v:?}"),
                })
                .collect(),
            v => panic!("{v:?}"),
        }
    };
    assert_eq!(get(&inst, "count").as_f64(), Some(0.0));
    assert_eq!(get(&inst, "code").as_f64(), Some(-1.0));
    let t = chrono::NaiveDate::from_ymd_opt(2026, 11, 8)
        .unwrap()
        .and_hms_opt(16, 4, 0)
        .unwrap();
    inst.update_action_run(
        "tx",
        RunUpdate {
            reset: true,
            lines: vec![
                line(1, ":: Starting", false, false),
                line(2, " 10%", true, true),
            ],
            running: Some((true, Some(t))),
            ..RunUpdate::default()
        },
    );
    inst.flush();
    assert_eq!(list(&inst), [":: Starting", "! 10%…"]);
    inst.update_action_run(
        "tx",
        RunUpdate {
            keep_from: 2,
            lines: vec![
                line(2, "100%", true, false),
                line(3, "error: failed", true, false),
            ],
            ..RunUpdate::default()
        },
    );
    inst.flush();
    assert_eq!(list(&inst), ["!100%", "!error: failed"]);
    assert_eq!(get(&inst, "first").as_f64(), Some(2.0));
    assert_eq!(text(&inst, "ended"), "", "still running");
    // The end: the last line, the status, why, then `running`, one flush.
    inst.update_action_run(
        "tx",
        RunUpdate {
            lines: vec![line(4, "done", false, false)],
            code: Some(1),
            error: Some(Some(
                "`pacman` failed (exit status: 1): error: failed".into(),
            )),
            running: Some((false, None)),
            ..RunUpdate::default()
        },
    );
    inst.flush();
    assert_eq!(get(&inst, "code").as_f64(), Some(1.0));
    assert_eq!(
        text(&inst, "ended"),
        "done|1|`pacman` failed (exit status: 1): error: failed"
    );
    // A run starting empties `lines`, `code` and `error`.
    inst.update_action_run(
        "tx",
        RunUpdate {
            reset: true,
            running: Some((true, Some(t))),
            ..RunUpdate::default()
        },
    );
    inst.flush();
    assert_eq!(get(&inst, "count").as_f64(), Some(0.0));
    assert_eq!(get(&inst, "code").as_f64(), Some(-1.0));
    // An undeclared action is ignored; one with a result has neither.
    inst.update_action_run("nope", RunUpdate::default());
    inst.flush();
    assert_eq!(get(&inst, "other").as_f64(), Some(-1.0));
}
