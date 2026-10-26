//! (oi-lock) `check::secret_flow`: a password typed into a `type:
//! password` input warns where it reaches something that shows, keeps,
//! runs or sends it, along `let`s, states, component parameters, `fn`s
//! and lambdas, and nowhere else (decisions.md oi-lock, 2026-10-26).

use strand_compiler::diagnostic::{Style, render};
use strand_compiler::{SourceMap, compile};

/// The text of each `check::secret_flow` warning's primary span in
/// `src` (one file, `shell.strand`), in order; panics on an error.
fn flows(src: &str) -> Vec<String> {
    let mut map = SourceMap::new();
    map.add("shell.strand", src);
    let out = compile(&map);
    let shown = render(&out.diagnostics, &map, Style::Plain);
    assert!(
        !out.diagnostics.iter().any(|d| d.is_error()),
        "{src}\n{shown}"
    );
    out.diagnostics
        .iter()
        .filter(|d| d.code == "check::secret_flow")
        .map(|d| {
            let s = d.primary_span().expect("a span");
            src[s.start as usize..s.end as usize].to_string()
        })
        .collect()
}

/// A top-level `pw` a bar's password field writes.
const TOP: &str = "state pw = \"\"\nbar Top { input { type: password; text: <-> pw } }\n";

/// A bar with a password field writing `pw`, then `rest`.
fn field(rest: &str) -> String {
    format!(
        "bar Top {{\n  state pw = \"\"\n  input {{ type: password; text: <-> pw }}\n{rest}\n}}\n"
    )
}

#[test]
fn shown_on_screen() {
    assert_eq!(flows(&field("  text pw")), ["pw"]);
    assert_eq!(flows(&field("  text \"x\" { tooltip: pw }")), ["pw"]);
    assert_eq!(
        flows(&field("  input { placeholder: pw.trim() }")),
        ["pw.trim()"]
    );
    // A plain input bound to it shows it in clear.
    assert_eq!(flows(&field("  input { text: <-> pw }")), ["pw"]);
    // In a `when`, on another element.
    assert_eq!(
        flows(&field(
            "  text \"x\" { when pw != \"\" { tooltip: \"[\" + pw } }"
        )),
        ["\"[\" + pw"]
    );
    // A token set from it, at the top or in an element; an enter pose
    // and keyframes reading it.
    assert_eq!(
        flows(&format!(
            "{TOP}tokens base {{ font {{ ui: pw 13px }} }}\nuse tokens base\n"
        )),
        ["ui"]
    );
    assert_eq!(
        flows(&field("  box { set { $font.ui: pw 13px } }")),
        ["$font.ui"]
    );
    assert_eq!(
        flows(&field("  box { enter { opacity: number(pw) ?? 0 } }")),
        ["number(pw) ?? 0"]
    );
    assert_eq!(
        flows(&format!(
            "{TOP}keyframes hop {{ 0% {{ y: number(pw) ?? 0 }}; duration: 450ms }}\n"
        )),
        ["number(pw) ?? 0"]
    );
    // A canvas drawing it.
    assert_eq!(
        flows(&field(
            "  canvas { width: 80; height: 20; draw: (c) => { c.text(pw, 0, 0) } }"
        )),
        ["(c) => { c.text(pw, 0, 0) }"]
    );
}

#[test]
fn through_lets_states_components_fns_and_lambdas() {
    // A `let`, and a `let` of a `let`.
    assert_eq!(
        flows(&field(
            "  let a = \"pw: \" + pw\n  let b = [a]\n  text b[0]"
        )),
        ["b[0]"]
    );
    // A copy written by a handler.
    assert_eq!(
        flows(&field(
            "  state tried = \"\"\n  box { on click { tried = pw; pw = \"\" } }\n  text tried"
        )),
        ["tried"]
    );
    // Pushed onto a list, then shown.
    assert_eq!(
        flows(&field(
            "  state all: [text] = []\n  box { on click { all.push(pw) } }\n  text all.join(\",\")"
        )),
        ["all.join(\",\")"]
    );
    // A handler's `let`, and a `for` over its characters.
    assert_eq!(
        flows(&field(
            "  state out = \"\"\n  box { on click { let p = pw.upper(); for ch in p.split(\"\") { out = ch } } }\n  text out"
        )),
        ["out"]
    );
    // A component's parameter, by position and by name.
    let comp = "component Show(v: text, w: text = \"\") { text v + w }\n";
    assert_eq!(flows(&format!("{}{comp}", field("  Show pw"))), ["v + w"]);
    assert_eq!(
        flows(&format!("{}{comp}", field("  Show \"a\" { w: pw }"))),
        ["v + w"]
    );
    // A `fn` given it, and a `fn` that reads it.
    assert_eq!(
        flows(&format!(
            "{}fn wrap(x: text) -> text {{ \"[\" + x + \"]\" }}\n",
            field("  text wrap(pw)")
        )),
        ["wrap(pw)"]
    );
    // A lambda's items: `map` carries what the lambda makes of them.
    assert_eq!(
        flows(&field(
            "  text pw.split(\"\").map(c => c.upper()).join(\"\")"
        )),
        ["pw.split(\"\").map(c => c.upper()).join(\"\")"]
    );
    // A lambda a handler's `let` holds, called with it.
    assert_eq!(
        flows(&field(
            "  state out = \"\"\n  box { on click { let f = (x: text) => \"[\" + x + \"]\"; out = f(pw) } }\n  text out"
        )),
        ["out"]
    );
}

#[test]
fn kept_or_sent() {
    // Exported: `strand get` and other files read it.
    assert_eq!(
        flows("export state pw = \"\"\nbar Top { input { type: password; text: <-> pw } }\n"),
        ["pw"]
    );
    // Persisted: written to disk.
    assert_eq!(
        flows(&field(
            "  state last = \"\" persist\n  box { on click { last = pw } }"
        )),
        ["last"]
    );
    // A settings file, written or bound.
    let prefs = "state prefs from \"prefs.toml\" { last: text = \"\" }\n";
    assert_eq!(
        flows(&format!(
            "{prefs}{}",
            field("  box { on click { prefs.last = pw } }")
        )),
        ["prefs"]
    );
    assert_eq!(
        flows(&format!(
            "{prefs}bar Top {{ input {{ type: password; text: <-> prefs.last }} }}\n"
        )),
        ["prefs.last"]
    );
    // A file written with it, and a file's command given it.
    assert_eq!(
        flows(&format!(
            "permit exec \"say\"\n{TOP}file \"out.txt\" {{ text: \"x\"; run: [\"say\", pw] }}\n"
        )),
        ["[\"say\", pw]"]
    );
    // A declared command's argument.
    assert_eq!(
        flows(&format!(
            "permit exec \"say\"\naction say(t: text) [\"say\", t]\n{}",
            field("  box { on click { say(pw) } }")
        )),
        ["pw"]
    );
    // A service's action, a record's action, a `from dbus` method.
    assert_eq!(
        flows(&field("  box { on click { clipboard.copy(pw) } }")),
        ["pw"]
    );
    assert_eq!(
        flows(&field(
            "  on notifications.received(n) { n.reply(\"re: \" + pw) }"
        )),
        ["\"re: \" + pw"]
    );
    assert_eq!(
        flows(&format!(
            "service ppd from dbus system \"net.hadess.PowerProfiles\" {{\n  action hold(p: text) = HoldProfile\n}}\n{}",
            field("  box { on click { ppd.hold(pw) } }")
        )),
        ["pw"]
    );
    // A service's field written.
    assert_eq!(
        flows(&field(
            "  box { on click { brightness.level = number(pw) ?? 0 } }"
        )),
        ["number(pw) ?? 0"]
    );
}

#[test]
fn the_keys_of_a_password_and_the_inputs_that_may_be_one() {
    let say = "permit exec \"say\"\naction say(t: text) [\"say\", t]\n";
    // An `on key` on a password input hears each character typed.
    assert_eq!(
        flows(&format!(
            "{say}bar Top {{\n  state pw = \"\"\n  input {{ type: password; text: <-> pw; on key(k) {{ say(k.text) }} }}\n}}\n"
        )),
        ["k.text"]
    );
    // So does one around it, the keys going to the innermost handler.
    assert_eq!(
        flows(&format!(
            "{say}bar Top {{\n  state pw = \"\"\n  box {{ on key(k) {{ say(k.name) }}\n    row {{ input {{ type: password; text: <-> pw }} }} }}\n}}\n"
        )),
        ["k.name"]
    );
    // Which key it was, as a `bool`, tells nothing; nor does a key
    // around a plain input.
    assert_eq!(
        flows(&format!(
            "{say}bar Top {{\n  state pw = \"\"\n  state go = false\n  \
             input {{ type: password; text: <-> pw; on key(k) {{ go = k.name == \"Return\" }} }}\n  \
             state q = \"\"\n  box {{ on key(k) {{ say(k.text) }}; input {{ text: <-> q }} }}\n}}\n"
        )),
        Vec::<String>::new()
    );
    // A `type: password` in a `when` block.
    assert_eq!(
        flows(
            "bar Top {\n  state pw = \"\"\n  state hide = true\n  \
             input { text: <-> pw; when hide { type: password } }\n  text pw\n}\n"
        ),
        ["pw"]
    );
    // A `type` a component is given.
    assert_eq!(
        flows(
            "component Field(kind: InputKind) {\n  state v = \"\"\n  input { type: kind; text: <-> v }\n  text v\n}\n\
             bar Top { Field password }\n"
        ),
        ["v"]
    );
}

#[test]
fn harmless_reads_and_the_meant_calls_do_not_warn() {
    let none = Vec::<String>::new();
    // The guide's lock: emptiness, `auth.submit`, a colour while typing.
    assert_eq!(
        flows(
            "lock {\n  state secret = \"\"\n  when secret != \"\" { bg: #20a050 }\n  \
             input { type: password; text: <-> secret; focus: true\n    \
             on activate { if secret != \"\" { auth.submit(secret) }; secret = \"\" }\n  }\n}\n"
        ),
        none
    );
    // Dots, a count, a comparison, a reveal toggle, the meant calls.
    let src = field(
        "  state reveal = false\n  \
         text pw.split(\"\").map(c => \"•\").join(\"\")\n  \
         text num(pw.len)\n  \
         text pw == \"\" ? \"empty\" : \"typed\"\n  \
         text pw.contains(\" \") ? \"a space\" : \"\"\n  \
         input { type: password; text: <-> pw; when reveal { type: text } }\n  \
         box { on click { polkit.respond(pw); greetd.submit(pw); pw = \"\" } }\n  \
         for ap in network.access_points key ap.ssid { box { on click { ap.connect_with(pw) } } }",
    );
    assert_eq!(flows(&src), none);
    // Dots made by a `fn` or by a lambda a `let` holds, given the
    // password: neither passes it on (`wrap(pw)` above does).
    assert_eq!(
        flows(&format!(
            "{}fn dots(p: text) -> text {{ p.split(\"\").map(c => \"•\").join(\"\") }}\n",
            field("  text dots(pw)")
        )),
        none
    );
    assert_eq!(
        flows(&field(
            "  state out = \"\"\n  box { on click { let f = (x: text) => \"•\"; out = f(pw) } }\n  text out"
        )),
        none
    );
    // No password input: a query shown is not a secret.
    assert_eq!(
        flows("bar Top {\n  state q = \"\"\n  input { text: <-> q }\n  text q\n}\n"),
        none
    );
}
