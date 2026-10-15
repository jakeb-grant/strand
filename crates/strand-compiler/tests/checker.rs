//! The checker: every design fixture type-checks cleanly, and the negative
//! corpus (`tests/negative`, one file per error class) gives exactly the
//! diagnostics its header names, rendered as `strand check` and the
//! overlay show them (snapshotted).

use std::path::{Path, PathBuf};

use strand_compiler::diagnostic::{Style, render};
use strand_compiler::hir::{self, DefKind, ExprKind, Target};
use strand_compiler::ty::Ty;
use strand_compiler::{Compiled, SourceMap, compile};

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("{name}.strand"));
    std::fs::read_to_string(path).unwrap()
}

fn compile_files(files: &[(&str, String)]) -> (Compiled, SourceMap) {
    let mut map = SourceMap::new();
    for (name, text) in files {
        map.add(*name, text.as_str());
    }
    (compile(&map), map)
}

fn compile_fixtures(names: &[&str]) -> (Compiled, SourceMap) {
    let files: Vec<(String, String)> = names
        .iter()
        .map(|n| (format!("{n}.strand"), fixture(n)))
        .collect();
    let borrowed: Vec<(&str, String)> =
        files.iter().map(|(n, t)| (n.as_str(), t.clone())).collect();
    compile_files(&borrowed)
}

fn assert_clean(names: &[&str]) {
    let (out, map) = compile_fixtures(names);
    assert!(
        out.diagnostics.is_empty(),
        "{names:?}:\n{}",
        render(&out.diagnostics, &map, Style::Plain)
    );
}

/// design.md's four shells, theme and rice are one config: zero errors and
/// zero warnings, together and each with the theme.
#[test]
fn design_shells_type_check() {
    assert_clean(&["bar", "launcher", "toasts", "osd", "theme", "rice_now"]);
    for shell in ["bar", "launcher", "toasts", "osd", "rice_now"] {
        assert_clean(&[shell]);
    }
    assert_clean(&["theme"]);
    assert_clean(&["hello_bar"]);
}

/// The snippet file reads `theme.look` and `Look`, so it is checked with
/// the theme; the grammar examples hold one intended lint.
#[test]
fn snippets_and_grammar_examples_type_check() {
    assert_clean(&["snippets", "theme"]);
    let (out, map) = compile_fixtures(&["grammar_examples"]);
    let codes: Vec<&str> = out.diagnostics.iter().map(|d| d.code).collect();
    assert_eq!(
        codes,
        ["check::raw_color"],
        "{}",
        render(&out.diagnostics, &map, Style::Plain)
    );
    assert_eq!(out.errors(), 0);
}

// ---------------------------------------------------------------------------
// Negative corpus

/// A corpus file: `// expect: code, code` on its first line, then one or
/// more files, each after `// file: name.strand` (the first may omit it;
/// it is then named after the corpus file).
struct Case {
    name: String,
    expect: Vec<String>,
    files: Vec<(String, String)>,
}

fn load(path: &Path) -> Case {
    let name = path.file_stem().unwrap().to_string_lossy().into_owned();
    let text = std::fs::read_to_string(path).unwrap();
    let first = text.lines().next().unwrap_or_default();
    let expect: Vec<String> = first
        .strip_prefix("// expect:")
        .unwrap_or_else(|| panic!("{name}: first line must be `// expect: codes`"))
        .split(',')
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .collect();
    let mut files: Vec<(String, String)> = vec![(format!("{name}.strand"), String::new())];
    for line in text.lines() {
        if let Some(f) = line.strip_prefix("// file: ") {
            if files.len() == 1 && files[0].1.trim().lines().all(|l| l.starts_with("//")) {
                files[0].0 = f.trim().to_string();
                files[0].1.clear();
            } else {
                files.push((f.trim().to_string(), String::new()));
            }
            continue;
        }
        let cur = &mut files.last_mut().unwrap().1;
        cur.push_str(line);
        cur.push('\n');
    }
    Case {
        name,
        expect,
        files,
    }
}

fn corpus() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/negative");
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "strand"))
        .collect();
    v.sort();
    v
}

#[test]
fn negative_corpus() {
    let files = corpus();
    assert!(files.len() >= 40, "corpus shrank: {}", files.len());
    let mut failures = Vec::new();
    let mut snapshots = Vec::new();
    for path in files {
        let case = load(&path);
        let borrowed: Vec<(&str, String)> = case
            .files
            .iter()
            .map(|(n, t)| (n.as_str(), t.clone()))
            .collect();
        let (out, map) = compile_files(&borrowed);
        let mut rendered = render(&out.diagnostics, &map, Style::Plain);
        rendered.push_str(&fixes(&out.diagnostics, &map));
        let mut got: Vec<String> = out.diagnostics.iter().map(|d| d.code.to_string()).collect();
        got.sort();
        got.dedup();
        let mut want = case.expect.clone();
        want.sort();
        if got != want {
            failures.push(format!(
                "{}: expected {want:?}, got {got:?}\n{rendered}",
                case.name
            ));
        }
        for d in &out.diagnostics {
            assert!(!d.labels.is_empty(), "{}: {d:?}", case.name);
        }
        snapshots.push((case.name, rendered));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    for (name, rendered) in snapshots {
        insta::assert_snapshot!(format!("negative@{name}"), rendered);
    }
}

/// The suggestions (quick fixes) of `diags`, one line each, checking that
/// each lies on whole characters of one line (in its diagnostic's file, or
/// for a missing key at the declaration it adds the key to), with a
/// zero-width span only as an insertion (`+` lines), and that a "did you
/// mean `x`?" help comes with `x` as its one fix (the LSP reads the fixes,
/// never the help).
fn fixes(diags: &[strand_compiler::diagnostic::Diagnostic], map: &SourceMap) -> String {
    let mut out = String::new();
    for d in diags {
        let meant = d
            .help
            .as_deref()
            .and_then(|h| h.strip_prefix("did you mean `")?.strip_suffix("`?"));
        if let Some(m) = meant {
            assert_eq!(d.suggestions.len(), 1, "no single fix for {d:?}");
            assert_eq!(d.suggestions[0].replacement, m, "{d:?}");
        }
        for f in &d.suggestions {
            if f.file != d.file() {
                assert_eq!(d.code, "check::missing_key", "{d:?}");
            }
            let file = map.get(f.file).unwrap();
            let text = file.text.get(f.span.start as usize..f.span.end as usize);
            let text = text.unwrap_or_else(|| panic!("fix span off the text: {d:?}"));
            assert!(!text.contains('\n'), "{d:?}");
            let title = f
                .title
                .as_deref()
                .map_or(String::new(), |t| format!(" ({t})"));
            if text.is_empty() {
                out.push_str(&format!(
                    "fix [{}] {}@{}: + `{}`{title}\n",
                    d.code, file.name, f.span.start, f.replacement
                ));
            } else {
                out.push_str(&format!(
                    "fix [{}] {}: `{text}` -> `{}`{title}\n",
                    d.code, file.name, f.replacement
                ));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The typed program

fn one(src: &str) -> Compiled {
    let (out, map) = compile_files(&[("a.strand", src.to_string())]);
    assert!(
        out.diagnostics.is_empty(),
        "{}",
        render(&out.diagnostics, &map, Style::Plain)
    );
    out
}

fn find_let<'p>(p: &'p hir::Program, name: &str) -> &'p hir::LetDecl {
    p.files
        .iter()
        .flat_map(|f| &f.items)
        .find_map(|i| match i {
            hir::Item::Let(l) if p.def(l.def).name == name => Some(l),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no let {name}"))
}

fn show(p: &hir::Program, t: &Ty) -> String {
    p.types.show(t).to_string()
}

#[test]
fn expressions_carry_types_and_spans() {
    let src = "let hits = apps.search(\"fi\")\n\
               let n = hits.len\n\
               let title = windows.focused?.title ?? \"\"\n\
               let label = join(\" · \", pct(battery.percent), dur(battery.time_left))\n\
               let wait = 6s\n";
    let out = one(src);
    let p = &out.program;
    let ty = |n: &str| show(p, &find_let(p, n).value.ty);
    assert_eq!(ty("hits"), "Async<[Hit]>");
    assert_eq!(ty("n"), "int");
    assert_eq!(ty("title"), "text");
    assert_eq!(ty("label"), "text");
    assert_eq!(ty("wait"), "duration");
    let v = &find_let(p, "title").value;
    assert_eq!(&src[v.span.range()], "windows.focused?.title ?? \"\"");
    let ExprKind::Binary { lhs, .. } = &v.kind else {
        panic!("{:?}", v.kind)
    };
    assert_eq!(show(p, &lhs.ty), "text?");
    // A whole-number handler `let` is an `int`, like a top-level one; a
    // fractional one stays a `float`.
    let out = one(
        "state open = false\nbar B { box { on click { let y = 1\n let z = 1.5\n open = y > z } } }\n",
    );
    let p = &out.program;
    let local = |n: &str| {
        let l = p.locals.iter().find(|l| l.name == n).unwrap();
        show(p, &l.ty)
    };
    assert_eq!(local("y"), "int");
    assert_eq!(local("z"), "float");
}

#[test]
fn references_resolve_for_the_lsp() {
    let src = "state open = false\n\
               component Clock {\n  text clock.format(\"%H\") { on click { open = !open } }\n}\n\
               bar Top { edge: top; Clock }\n";
    let out = one(src);
    let p = &out.program;
    let at = |needle: &str, nth: usize| {
        let off = src.match_indices(needle).nth(nth).unwrap().0 as u32;
        p.reference_at(out.parses[0].file_id, off + 1)
            .unwrap_or_else(|| panic!("no reference at {needle}"))
            .target
            .clone()
    };
    let Target::Def(open) = at("open", 1) else {
        panic!()
    };
    assert_eq!(p.def(open).kind, DefKind::State);
    assert_eq!(p.def(open).name, "open");
    assert_eq!(at("clock", 0), Target::Service("clock".into()));
    let Target::Def(clock) = at("Clock", 1) else {
        panic!()
    };
    assert_eq!(p.def(clock).kind, DefKind::Component);
    assert_eq!(at("top", 0), Target::Variant(variant_of(p, "Edge"), 0));
}

fn variant_of(p: &hir::Program, enum_name: &str) -> strand_compiler::ty::EnumId {
    strand_compiler::ty::EnumId(
        p.types
            .enums
            .iter()
            .position(|e| e.name == enum_name)
            .unwrap() as u32,
    )
}

#[test]
fn exports_are_file_paths() {
    let (out, _) = compile_fixtures(&["theme", "launcher", "toasts"]);
    let mut exports: Vec<String> = out.program.exports().map(|(p, _)| p).collect();
    exports.sort();
    assert_eq!(exports, ["launcher.open", "theme.look", "toasts.dnd"]);
}

#[test]
fn token_paths_are_typed() {
    let (out, _) = compile_fixtures(&["theme"]);
    let p = &out.program;
    for (path, ty) in [
        ("space.2", "length"),
        ("font.ui", "font"),
        ("motion.bouncy", "Spring"),
        ("elevation.lg", "shadow"),
        ("surface.hi", "color"),
        ("border", "color"),
    ] {
        assert_eq!(show(p, &p.tokens[path]), ty, "{path}");
    }
}

#[test]
fn every_element_gets_a_node_and_ids_are_shared() {
    let src = "component V {\n  row {\n    box { id: vol }\n    box { when vol.hover { opacity: 0.5 } }\n  }\n}\n";
    let out = one(src);
    let p = &out.program;
    let hir::Item::Component(c) = &p.files[0].items[0] else {
        panic!()
    };
    let hir::Node::Element(row) = &c.body[0] else {
        panic!()
    };
    let hir::Node::Element(first) = &row.children[0] else {
        panic!()
    };
    let hir::Node::Element(second) = &row.children[1] else {
        panic!()
    };
    let vol = first.id.expect("id: vol");
    let hir::LocalKind::NodeId(idx) = p.local(vol).kind else {
        panic!()
    };
    assert_eq!(idx, first.node);
    assert_ne!(first.node, second.node);
    let hir::Node::When(w) = &second.children[0] else {
        panic!("{:?}", second.children)
    };
    let ExprKind::Field { base, name, .. } = &w.cond.kind else {
        panic!()
    };
    assert_eq!(name, "hover");
    // An `id:` name reads the node itself.
    assert!(matches!(base.kind, ExprKind::Node(n) if n == first.node));
}

/// A service crate adds its schema, and configs type-check against it.
#[test]
fn contributed_service_schemas_check() {
    let mut schema = strand_compiler::schema::Schema::builtin().clone();
    schema
        .extend("service weather { temp: float; summary: text; action refresh() }")
        .unwrap();
    let src = "bar Top { edge: top; text pct(weather.temp) { on click { weather.refresh() } } }\n\
               let bad = weather.tmp\n";
    let mut map = SourceMap::new();
    map.add("a.strand", src);
    let out = strand_compiler::compile_with(&map, &schema);
    let msgs: Vec<String> = out
        .diagnostics
        .iter()
        .map(|d| format!("{} {}", d.message, d.help.clone().unwrap_or_default()))
        .collect();
    assert_eq!(msgs.len(), 1, "{msgs:?}");
    assert!(msgs[0].contains("did you mean `temp`?"), "{msgs:?}");
}

#[test]
fn builtin_names_are_a_prelude() {
    // A declaration shadows a builtin function, value or type in its
    // scope: no reserved words (grammar.md).
    one("state min = 0\n\
         let hue = 3\n\
         enum Place { home, away }\n\
         type Key { k: int }\n\
         enum Variant { a, b }\n\
         fn ease_out(t: float) -> float { 1 - (1 - t) * (1 - t) }\n\
         fn wave(x: float) -> float { x }\n\
         component Avatar(shape: Shape = circle, blur: length = 0, contrast: float = 1) {\n\
           box { width: blur; opacity: contrast }\n\
         }\n\
         bar Top { edge: top; text pct(ease_out(min + hue)); Avatar { }; box { bg: conic($accent, $secondary) } }\n");
    // A service crate that adds a name the config already uses does not
    // break it.
    let mut schema = strand_compiler::schema::Schema::builtin().clone();
    schema
        .extend("service weather { temp: float }\nfn ring(x: float) -> float")
        .unwrap();
    let mut map = SourceMap::new();
    map.add(
        "a.strand",
        "let ring = 2\nfn f(weather: float) -> float { weather * ring }\n",
    );
    let out = strand_compiler::compile_with(&map, &schema);
    assert!(
        out.diagnostics.iter().all(|d| !d.is_error()),
        "{:?}",
        out.diagnostics
    );
}

/// A file's top-level `let` or `state` comes before the builtins at a
/// call too, so a name means one thing in a file, and a service crate
/// that later adds a function of that name changes nothing.
#[test]
fn top_level_bindings_shadow_builtins_at_calls() {
    // `pct` is the builtin `-> text`; the lambda returns a float.
    let out = one("let pct = (x: float) => x * 2\nlet z: float = pct(0.5)\n");
    assert_eq!(def_ty(&out.program, "z"), "float");
    // A non-function named like a builtin cannot be called.
    let (out, map) = compile_files(&[(
        "a.strand",
        "state blur = 4px\nlet y = blur(16)\nbar B { box { pad: blur } }\n".to_string(),
    )]);
    let text = render(&out.diagnostics, &map, Style::Plain);
    assert_eq!(out.diagnostics.len(), 1, "{text}");
    assert!(text.contains("check::type_mismatch"), "{text}");
    assert!(text.contains("hides the builtin `blur`"), "{text}");
    // So does any other global named like one (`enum wave`).
    let (out, map) = compile_files(&[(
        "a.strand",
        "enum wave { a, b }\nlet y = wave(2s)\n".to_string(),
    )]);
    let text = render(&out.diagnostics, &map, Style::Plain);
    assert!(text.contains("hides the builtin `wave`"), "{text}");
    // A crate adding `fn ring` after the fact: the call still reaches
    // the config's lambda (the builtin would return `text`).
    let src = "let ring = (x: float) => x * 2\nlet y: float = ring(1)\n";
    one(src);
    let mut schema = strand_compiler::schema::Schema::builtin().clone();
    schema.extend("fn ring(x: float) -> text").unwrap();
    let mut map = SourceMap::new();
    map.add("a.strand", src);
    let out = strand_compiler::compile_with(&map, &schema);
    assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    assert_eq!(def_ty(&out.program, "y"), "float");
}

#[test]
fn component_tokens_are_overridden_loudly() {
    let src = "component Toast(n: int) tokens { radius: $radius.lg } {\n\
                 col { radius: $Toast.radius }\n\
               }\n\
               tokens x { override Toast.radius: 3px }\n\
               use tokens x\n";
    one(src);
    let (out, map) = compile_files(&[(
        "a.strand",
        "component Toast(n: int) tokens { radius: $radius.lg } {\n\
           col { radius: $Toast.radius }\n\
         }\n\
         tokens x { override Toast.radius: $accent }\n"
            .to_string(),
    )]);
    let text = render(&out.diagnostics, &map, Style::Plain);
    assert_eq!(out.diagnostics.len(), 1, "{text}");
    assert!(text.contains("check::type_mismatch"), "{text}");
}

fn def_ty(p: &hir::Program, name: &str) -> String {
    let d = p
        .defs
        .iter()
        .find(|d| d.name == name)
        .unwrap_or_else(|| panic!("no def {name}"));
    show(p, &d.ty)
}

/// (M6, owner-testing gap 23) `xs.index_of(test)` is the position of
/// `find`'s item, an `int?` that indexes the list back once defaulted;
/// its test is a function of the item, as `find`'s is.
#[test]
fn index_of_is_an_optional_int_of_a_predicate() {
    let out = one("state names = [\"a\", \"b\", \"c\"]\n\
                   let at = names.index_of(n => n == \"b\")\n\
                   let next = names[(at ?? -1) + 1]\n\
                   bar B { text names[names.index_of(n => n != \"\") ?? 0] }\n");
    assert_eq!(def_ty(&out.program, "at"), "int?");
    assert_eq!(def_ty(&out.program, "next"), "text");
    // The island bar's step to the next workspace (owner-testing.md).
    one("let slots = workspaces.all\n\
         bar B { box { on click {\n\
           slots[(slots.index_of(w => w.focused) ?? -1) + 1]?.focus()\n\
         } } }\n");
    assert_eq!(
        codes("state names = [\"a\"]\nlet at = names.index_of(\"a\")\n"),
        ["check::type_mismatch"]
    );
    assert_eq!(
        codes("state names = [\"a\"]\nlet at: int = names.index_of(n => n == \"a\")\n"),
        ["check::nullable"]
    );
}

/// A list literal of whole numbers is an `[int]` (design.md: `state xs =
/// [1, 2]`), so its items index, count and fill `int` props; a fraction
/// written to the list or to one of its items makes it a `[float]`.
#[test]
fn whole_number_lists_are_int_lists_until_a_fraction_arrives() {
    let out = one("state xs = [1, 2]\n\
                   let ys = [3, -4]\n\
                   state grid_of = [[1], [2, 3]]\n\
                   state mixed = [1, 2.5]\n\
                   state whole_list = [0.5]\n\
                   state set_whole = [1, 2]\n\
                   state set_item = [1, 2]\n\
                   state set_int = [1, 2]\n\
                   state from_a = [0]\n\
                   state a = 0\n\
                   component C {\n\
                     col {\n\
                       grid { columns: xs[0] }\n\
                       text \"x\" { max_lines: ys[1] + grid_of[1][0] }\n\
                       box { on click {\n\
                         set_whole = [0.5]\n\
                         set_item[1] = 0.5\n\
                         set_int[0] = 3\n\
                         a = 0.5\n\
                         from_a = [a]\n\
                         let local = [1, 2]\n\
                         xs = local\n\
                       } }\n\
                     }\n\
                   }\n");
    let p = &out.program;
    for (name, ty) in [
        ("xs", "[int]"),
        ("ys", "[int]"),
        ("grid_of", "[[int]]"),
        ("mixed", "[float]"),
        ("whole_list", "[float]"),
        ("set_whole", "[float]"),
        ("set_item", "[float]"),
        ("set_int", "[int]"),
        ("from_a", "[float]"),
    ] {
        assert_eq!(def_ty(p, name), ty, "{name}");
    }
    // A fraction reaching a list read as an `int` is reported there.
    let (out, map) = compile_files(&[(
        "a.strand",
        "state xs = [1, 2]\n\
         component C { box { on click { xs[0] = 0.5 } }\n  grid { columns: xs[0] } }\n"
            .to_string(),
    )]);
    let codes: Vec<&str> = out.diagnostics.iter().map(|d| d.code).collect();
    assert_eq!(
        codes,
        ["check::type_mismatch"],
        "{}",
        render(&out.diagnostics, &map, Style::Plain)
    );
    assert_eq!(def_ty(&out.program, "xs"), "[float]");
}

/// Gaps 12 (decisions.md m6-w2-language): whole numbers stay `int`s
/// through `?:` and `match` (an empty list fits any list), so a `fn`
/// returning workspace ids by monitor says `[int]`; a fraction written
/// to such a declaration still makes it a `float`.
#[test]
fn whole_numbers_stay_ints_through_match_and_ternaries() {
    let out = one("fn rules(name: text) -> [int] {\n\
                     let r = match name { \"DP-8\" => [1, 2, 3, 4], \"DP-6\" => [9, 10], _ => [] }\n\
                     r\n\
                   }\n\
                   state c = true\n\
                   let ids = c ? [1, 2] : []\n\
                   let one = match c { true => 1, false => 2 }\n\
                   state widened = c ? [1] : [2]\n\
                   let mixed = c ? [1] : [0.5]\n\
                   component C {\n\
                     grid { columns: one + ids[0] + rules(\"DP-8\")[0]\n\
                       on click { widened = [0.5] } }\n\
                   }\n");
    assert_eq!(out.errors(), 0, "{:#?}", out.diagnostics);
    let p = &out.program;
    for (name, ty) in [
        ("ids", "[int]"),
        ("one", "int"),
        ("widened", "[float]"),
        ("mixed", "[float]"),
    ] {
        assert_eq!(def_ty(p, name), ty, "{name}");
    }
}

/// An untyped `state` or `let` holding a whole number is an `int`, so it
/// can index, count and fill `int` props; once a fraction is written to
/// it (directly, through a `float` it is set from, or a slider's `<->`)
/// it is a `float`.
#[test]
fn whole_number_declarations_are_ints_until_a_fraction_arrives() {
    let out = one("state i = 0\n\
                   state xs: [text] = []\n\
                   state cols = 7\n\
                   state n = 3\n\
                   state level = 0\n\
                   state a = 0\n\
                   state b = 0\n\
                   state count = 0\n\
                   component C {\n\
                     col {\n\
                       text xs[i]\n\
                       grid { columns: cols }\n\
                       text \"x\" { max_lines: cols }\n\
                       for p in notifications.popups.take(n) { text p.summary }\n\
                       slider { value: <-> level }\n\
                       box { on click { a = a + 0.5; b = a; count += 1; i = i + 1 } }\n\
                     }\n\
                   }\n");
    let p = &out.program;
    for (name, ty) in [
        ("i", "int"),
        ("cols", "int"),
        ("n", "int"),
        ("count", "int"),
        ("level", "float"),
        ("a", "float"),
        ("b", "float"),
    ] {
        assert_eq!(def_ty(p, name), ty, "{name}");
    }
    // A long chain of hand-offs widens every link, so the fraction
    // reaches the `int` prop and is reported there.
    let links = 12;
    let mut src = String::new();
    for k in 0..=links {
        src.push_str(&format!("state s{k} = 0\n"));
    }
    src.push_str("component C {\n  box { on click { s0 = 0.5");
    for k in 1..=links {
        src.push_str(&format!("; s{k} = s{} * 2", k - 1));
    }
    src.push_str(&format!(" }} }}\n  grid {{ columns: s{links} }}\n}}\n"));
    let (out, map) = compile_files(&[("a.strand", src)]);
    let codes: Vec<&str> = out.diagnostics.iter().map(|d| d.code).collect();
    assert_eq!(
        codes,
        ["check::type_mismatch"],
        "{}",
        render(&out.diagnostics, &map, Style::Plain)
    );
    assert_eq!(def_ty(&out.program, &format!("s{links}")), "float");
    // Hand-offs through handler locals and untyped fn values are
    // followed too: a long chain of either checks clean (no pass cap to
    // hit), and the fraction still reaches the `int` prop at the end.
    let chains = [
        (
            String::new(),
            (1..=links)
                .map(|k| format!("; let t{k} = s{}; s{k} = t{k}", k - 1))
                .collect::<String>(),
        ),
        (
            (1..=links)
                .map(|k| format!("fn f{k}() {{ s{} }}\n", k - 1))
                .collect::<String>(),
            (1..=links)
                .map(|k| format!("; s{k} = f{k}()"))
                .collect::<String>(),
        ),
    ];
    for (fns, writes) in &chains {
        for grid in [false, true] {
            let mut src = String::new();
            for k in 0..=links {
                src.push_str(&format!("state s{k} = 0\n"));
            }
            src.push_str(fns);
            src.push_str(&format!(
                "component C {{\n  box {{ on click {{ s0 = 0.5{writes} }} }}\n"
            ));
            if grid {
                src.push_str(&format!("  grid {{ columns: s{links} }}\n"));
            }
            src.push_str("}\n");
            let (out, map) = compile_files(&[("a.strand", src)]);
            let codes: Vec<&str> = out.diagnostics.iter().map(|d| d.code).collect();
            let want: &[&str] = if grid { &["check::type_mismatch"] } else { &[] };
            assert_eq!(
                codes,
                want,
                "{}",
                render(&out.diagnostics, &map, Style::Plain)
            );
            assert_eq!(def_ty(&out.program, &format!("s{links}")), "float");
        }
    }
    // An exported whole number reads as an `int` from another file.
    let (out, map) = compile_files(&[
        ("a.strand", "export let q = 1\n".into()),
        ("b.strand", "let r: int = a.q\n".into()),
    ]);
    assert!(
        out.diagnostics.is_empty(),
        "{}",
        render(&out.diagnostics, &map, Style::Plain)
    );
}

/// An untyped component parameter takes its type from its callers
/// (grammar.md's `component Toast(n) tokens { … }`).
#[test]
fn component_parameters_are_inferred_from_callers() {
    let out = one("tokens base { radius { lg: 14px } }\n\
                   component Toast(n) tokens { radius: $radius.lg } {\n\
                     col { radius: $Toast.radius; text n.summary }\n\
                   }\n\
                   panel Toasts { for n in notifications.popups { Toast n } }\n\
                   component Label(s) { text s }\n\
                   component Wrap(x) { Label s: x }\n\
                   component Grid(n) { grid { columns: n } }\n\
                   component Mixed(v) { meter v }\n\
                   bar Top { edge: top; Wrap \"hi\"; Grid 3; Grid n: -2; Mixed 1; Mixed 0.5 }\n");
    let p = &out.program;
    let param_ty = |comp: &str| {
        let c = p
            .files
            .iter()
            .flat_map(|f| &f.items)
            .find_map(|i| match i {
                hir::Item::Component(c) if p.def(c.def).name == comp => Some(c),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no component {comp}"));
        show(p, &p.locals[c.params[0].local.0 as usize].ty)
    };
    assert_eq!(param_ty("Toast"), "Notification");
    // Through another inferred component, named or positional.
    assert_eq!(param_ty("Wrap"), "text");
    assert_eq!(param_ty("Label"), "text");
    // A whole-number literal is an `int`, as in an untyped `state`; a
    // fraction from another caller widens it.
    assert_eq!(param_ty("Grid"), "int");
    assert_eq!(param_ty("Mixed"), "float");
    // The HIR keeps file order.
    let names: Vec<&str> = p.files[0]
        .items
        .iter()
        .filter_map(|i| match i {
            hir::Item::Component(c) => Some(p.def(c.def).name.as_str()),
            hir::Item::Surface(s) => s.def.map(|d| p.def(d).name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        names,
        ["Toast", "Toasts", "Label", "Wrap", "Grid", "Mixed", "Top"]
    );
    // An argument is checked as the value of an untyped `let` is, so
    // integer arithmetic stays an `int`; mutually recursive inferring
    // components see each other's arguments that way too.
    let out = one("state count = 2\n\
                   component Grid(n) { grid { columns: n } }\n\
                   component A(x) { if x > 1 { B x - 1 } }\n\
                   component B(y) { if y > 1 { A y - 1 } }\n\
                   bar Top { Grid count + 1; A 3 }\n");
    let p = &out.program;
    let param_ty = |comp: &str| {
        let c = p
            .files
            .iter()
            .flat_map(|f| &f.items)
            .find_map(|i| match i {
                hir::Item::Component(c) if p.def(c.def).name == comp => Some(c),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no component {comp}"));
        show(p, &p.locals[c.params[0].local.0 as usize].ty)
    };
    assert_eq!(param_ty("Grid"), "int");
    assert_eq!(param_ty("A"), "int");
    assert_eq!(param_ty("B"), "int");
    // A call inside such a cycle can come after the parameter's type was
    // joined: a fraction there widens it (`1` outside, `0.5` inside).
    // (B's call is conditional: an unconditional one is a static cycle.)
    let out = one("component A(x) { text \"${x}\"; B x }\n\
                   component B(y) { if y > 0 { A 0.5 } }\n\
                   bar Top { A 1 }\n");
    let p = &out.program;
    let a = p
        .files
        .iter()
        .flat_map(|f| &f.items)
        .find_map(|i| match i {
            hir::Item::Component(c) if p.def(c.def).name == "A" => Some(c),
            _ => None,
        })
        .expect("A");
    assert_eq!(show(p, &p.locals[a.params[0].local.0 as usize].ty), "float");
}

/// A whole-number literal is a `float` unless its position expects an
/// `int`; a fn's value is such a position, and a typed fn may recurse.
#[test]
fn literals_take_the_type_their_position_expects() {
    let out = one("fn fact(n: int) -> int { n <= 1 ? 1 : n * fact(n - 1) }\n\
                   state ticks = 0\n\
                   let cols: int = 7\n\
                   let half = ticks / 2\n");
    let p = &out.program;
    assert_eq!(show(p, &find_let(p, "cols").value.ty), "int");
    assert_eq!(show(p, &find_let(p, "half").value.ty), "float");
}

/// An override whose closest token lies in another group cannot be fixed
/// by editing its key: the help names that token and offers no fix, and
/// never reads as a did-you-mean with nothing to apply.
#[test]
fn an_override_near_another_group_names_it() {
    let src = "tokens base { fg.muted: $fg.alpha(0.65); ink { x: 1px } }\n\
               tokens compact extends base { override ink { muted: 2px } }\n";
    let (out, _) = compile_files(&[("t.strand", src.to_string())]);
    let d = out
        .diagnostics
        .iter()
        .find(|d| d.code == "check::unknown_token")
        .unwrap_or_else(|| panic!("{:?}", out.diagnostics));
    assert_eq!(
        d.help.as_deref(),
        Some("the closest token is `$fg.muted`, which is outside group `ink`"),
        "{d:?}"
    );
    assert!(d.suggestions.is_empty(), "{d:?}");
}

/// design.md "What you see" #2: a call still passing a parameter that was
/// renamed reads `bar.strand:12: unknown prop "expanded"; did you mean
/// "open"?`, and proposes `open` as the fix.
#[test]
fn renamed_parameter_reads_as_the_design_shows() {
    let src = "component Calendar(open: bool = false, day: int = 1) { col { } }\n\
               component Bar(d: int) {\n  Calendar {\n    day: d\n    expanded: true\n  }\n}\n";
    let (out, map) = compile_files(&[("bar.strand", src.to_string())]);
    let short = strand_compiler::diagnostic::render_short(&out.diagnostics, &map);
    assert_eq!(
        short,
        "bar.strand:5:5: error[check::unknown_param]: unknown prop \"expanded\"; did you mean \"open\"?\n"
    );
    let fix = &out.diagnostics[0].suggestions;
    assert_eq!(fix.len(), 1);
    assert_eq!(fix[0].replacement, "open");
    assert_eq!(fix[0].span.text(src), "expanded");
}

/// An unknown named argument of a function call: its fixes are the
/// parameters that call does not set yet, read from the call itself.
#[test]
fn unknown_named_arguments_offer_the_parameters_left() {
    let fixes = |src: &str| {
        let (out, _) = compile_files(&[("a.strand", src.to_string())]);
        let d = out
            .diagnostics
            .iter()
            .find(|d| d.code == "check::unknown_param")
            .unwrap_or_else(|| panic!("{:?}", out.diagnostics));
        let names: Vec<String> = d
            .suggestions
            .iter()
            .map(|s| s.replacement.clone())
            .collect();
        (d.help.clone().unwrap_or_default(), names)
    };
    let (help, names) = fixes("fn f(a: int, b: int) -> int { a + b }\nlet x = f(a: 1, zzz: 2)\n");
    assert_eq!(
        (help.as_str(), names),
        ("did you mean `b`?", vec!["b".to_string()])
    );
    let (help, names) =
        fixes("fn f(a: int, b: int, c: int) -> int { a + b + c }\nlet x = f(1, zzz: 2)\n");
    assert_eq!(help, "it takes `a`, `b` and `c`");
    assert_eq!(names, vec!["b".to_string(), "c".to_string()]);
}

/// An async service call a binding reaches through a lambda or a `fn` is
/// fetched once in place and never answers the binding: warned at the
/// call (a binding's own call, and calls in handlers, are fine).
#[test]
fn async_service_calls_a_binding_reaches_through_functions_are_warned() {
    let codes = |src: &str| {
        let (out, map) = compile_files(&[("a.strand", src.to_string())]);
        assert_eq!(
            out.errors(),
            0,
            "{}",
            render(&out.diagnostics, &map, Style::Plain)
        );
        out.diagnostics.iter().map(|d| d.code).collect::<Vec<_>>()
    };
    const W: &str = "check::async_in_binding_fn";
    assert_eq!(
        codes("let qs = [\"a\"]\nlet r = qs.map(q => apps.search(q))\n"),
        [W]
    );
    assert_eq!(
        codes(
            "fn f(q: text) -> Async<[Hit]> { apps.search(q) }\n\
             fn g(q: text) -> Async<[Hit]> { f(q) }\n\
             let r = g(\"x\")\n"
        ),
        [W]
    );
    // Fine: the binding's own call, and calls in handlers (awaited).
    assert!(codes("let r = apps.search(\"x\")\n").is_empty());
    assert!(
        codes(
            "fn f(q: text) -> Async<[Hit]> { apps.search(q) }\n\
             export state q = \"\"\n\
             export state n = 0\n\
             on change q { let h = await f(q)\n  n = h.count(x => true) }\n"
        )
        .is_empty()
    );
}

/// An `rw` field of an item of a service's keyed list is written through
/// the item (the service finds it by its key): assigned in a handler, or
/// bound two-way, through a `for` local or an index.
#[test]
fn rw_fields_of_service_items_are_writable() {
    one("component Mixer {\n\
           col {\n\
             for s in audio.sinks {\n\
               row {\n\
                 text s.description\n\
                 box { on click { s.volume = 0.5; s.muted = !s.muted } }\n\
                 slider { value: <-> s.volume }\n\
               }\n\
             }\n\
             box { on click { audio.sinks[0].volume += 0.1 } }\n\
           }\n\
         }\n");
}

// ---------------------------------------------------------------------------
// No-code services

fn codes(src: &str) -> Vec<&'static str> {
    let (out, _) = compile_files(&[("a.strand", src.to_string())]);
    out.diagnostics.iter().map(|d| d.code).collect()
}

/// design.md: `from dbus`, `from file`, `from listen` and `from poll`
/// sources are constants evaluated at load, lowered with each field's
/// key; running a command needs `permit exec` (a poll of a file runs
/// nothing), and only a D-Bus property can be `rw`.
#[test]
fn no_code_service_sources_are_constant_and_lowered() {
    let src = r#"
permit exec "sensors", "playerctl", "my tool"
service ppd from dbus system "net.hadess.PowerProfiles" { profile: text rw = ActiveProfile }
service ups from dbus session "org.example.Thing" "/obj" { level: float = Level }
service mood from file "~/.cache/mood.json" { level: int = mood.level; name: text? = "Display Name" }
service music from listen ["playerctl", "-F", "metadata"] { title: text? }
service temp from poll ["sensors", "-j"] every 5s { cpu: float = package }
service tool from poll "'my tool' --json a\\ b" every 500ms { x: int }
service heat from poll "/sys/class/thermal/thermal_zone0/temp" every 2s { milli: int }
bar B { text join(" ", ppd.profile, ups.level, mood.level, music.title ?? "", temp.cpu, tool.x, heat.milli) }
"#;
    let out = one(src);
    let program =
        strand_compiler::lower::lower(&out.program, strand_compiler::schema::Schema::builtin());
    let svc = |name: &str| {
        program
            .services
            .values()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("{name} lowered"))
            .clone()
    };
    use std::time::Duration;
    use strand_compiler::hir::{PollTarget, SourceSpec};
    assert_eq!(
        svc("ppd").source,
        SourceSpec::Dbus {
            system: true,
            name: "net.hadess.PowerProfiles".into(),
            path: None
        }
    );
    assert_eq!(svc("ppd").fields[0].key, ["ActiveProfile"]);
    assert!(svc("ppd").fields[0].rw);
    assert_eq!(
        svc("ups").source,
        SourceSpec::Dbus {
            system: false,
            name: "org.example.Thing".into(),
            path: Some("/obj".into())
        }
    );
    assert_eq!(
        svc("mood").source,
        SourceSpec::File {
            path: "~/.cache/mood.json".into()
        }
    );
    assert_eq!(svc("mood").fields[0].key, ["mood", "level"]);
    assert_eq!(svc("mood").fields[1].key, ["Display Name"]);
    assert_eq!(
        svc("music").source,
        SourceSpec::Listen {
            command: vec!["playerctl".into(), "-F".into(), "metadata".into()]
        }
    );
    assert_eq!(svc("music").fields[0].key, ["title"], "its own name");
    assert_eq!(
        svc("temp").source,
        SourceSpec::Poll {
            target: PollTarget::Command(vec!["sensors".into(), "-j".into()]),
            every: Duration::from_secs(5)
        }
    );
    assert_eq!(
        svc("tool").source,
        SourceSpec::Poll {
            target: PollTarget::Command(vec!["my tool".into(), "--json".into(), "a b".into()]),
            every: Duration::from_millis(500)
        }
    );
    assert_eq!(
        svc("heat").source,
        SourceSpec::Poll {
            target: PollTarget::File("/sys/class/thermal/thermal_zone0/temp".into()),
            every: Duration::from_secs(2)
        }
    );

    // A poll of a file needs no permit; a command does.
    assert_eq!(
        codes("service u from poll \"uptime -p\" every 60s { t: text }"),
        ["check::no_permit"]
    );
    assert!(codes("service h from poll \"/sys/x\" every 1s { a: int }").is_empty());
    // Sources are constants.
    assert_eq!(
        codes("let p = \"/tmp/x\"\nservice f from file p { a: int }"),
        ["check::not_constant"]
    );
    assert_eq!(
        codes("permit exec\nservice f from poll [\"a\"] every 0s { a: int }"),
        ["check::not_constant"]
    );
    // An interval no `Duration` holds is reported, never a panic (the
    // checker runs on every keystroke in the LSP).
    assert!(
        codes(
            "permit exec\nservice f from poll [\"a\"] every 99999999999999999999999999s { a: int }"
        )
        .contains(&"check::not_constant")
    );
    // A permit inside the service block lists programs as a top-level
    // one does: bare allows any, a list only those it names.
    assert_eq!(
        codes(
            "service t from poll [\"sensors\", \"-j\"] every 5s { cpu: float = package; permit exec \"foo\" }"
        ),
        ["check::no_permit"]
    );
    assert_eq!(
        codes("service t from listen [\"curl\", \"x\"] { a: int; permit exec \"sensors\" }"),
        ["check::no_permit"]
    );
    assert!(codes("service t from poll [\"sensors\", \"-j\"] every 5s { cpu: float = package; permit exec \"sensors\" }").is_empty());
    assert!(codes("service t from listen [\"curl\", \"x\"] { a: int; permit exec }").is_empty());
    // A command polled faster than the floor forks for little: warned
    // (it runs at the floor); a file poll forks nothing.
    assert_eq!(
        codes("permit exec\nservice f from poll [\"a\"] every 10ms { a: int }"),
        ["check::poll_too_fast"]
    );
    assert!(codes("permit exec\nservice f from poll [\"a\"] every 100ms { a: int }").is_empty());
    assert!(codes("service h from poll \"/sys/x\" every 10ms { a: int }").is_empty());
    // A list's first item is the program whole, as it runs (never split
    // into words): `["sh -c", …]` is not the program `sh`.
    assert_eq!(
        codes("permit exec \"sh\"\nservice t from listen [\"sh -c\", \"x\"] { a: int }"),
        ["check::no_permit"]
    );
    assert!(
        codes("permit exec \"my tool\"\nservice t from listen [\"my tool\", \"x\"] { a: int }")
            .is_empty()
    );
    // A D-Bus field reads one property whole; a key path on it is an
    // error, not `Prop` read silently (a file's key path walks the document).
    assert_eq!(
        codes(
            "service p from dbus system \"net.hadess.PowerProfiles\" { a: text = ActiveProfile.sub }"
        ),
        ["check::type_mismatch"]
    );
    assert!(codes("service f from file \"/tmp/x.json\" { a: int = outer.inner }").is_empty());
    // Only D-Bus properties are written.
    assert_eq!(
        codes("service f from file \"/tmp/x\" { a: int rw }"),
        ["check::not_writable"]
    );
    // A field's type is one a document holds: a schema entity (`Screen`)
    // or a paint is not; colours, durations, enums, lists, optionals and
    // declared records of these are.
    assert_eq!(
        codes("service bad from file \"x.json\" { m: Screen; c: color; s: [Screen]; p: paint? }"),
        [
            "check::type_mismatch",
            "check::type_mismatch",
            "check::type_mismatch"
        ]
    );
    assert!(
        codes(
            "enum Mood { calm, busy }\ntype Temp { label: text; c: float; at: duration }\n\
             service ok from file \"x.json\" { c: color; d: duration; m: Mood?; t: [Temp]; \
             l: length; p: percent; a: angle; f: path; b: bool }"
        )
        .is_empty()
    );
    assert_eq!(
        codes("type Holder { s: Screen }\nservice bad from file \"x.json\" { h: [Holder] }"),
        ["check::type_mismatch"]
    );
}

/// A config's own service's `rw` field is written as a builtin service's
/// is (spec.md 5.6, 8.1): assigned (`=`, a compound `+=`) and bound with
/// `<->`, lowered as a write of that service's field (the service host
/// writes it by name). Its read-only fields, and the service as a whole,
/// stay refused.
#[test]
fn a_config_services_rw_field_is_writable() {
    use strand_compiler::lower::{PlaceRoot, PlaceSeg};
    const SVC: &str = "service ppd from dbus system \"net.hadess.PowerProfiles\" \
                       { profile: text rw = ActiveProfile; degraded: text = PerformanceDegraded }\n\
                       service lvl from dbus session \"org.example.Level\" { n: int rw = Level }\n";
    let src = format!(
        "{SVC}export state saver = false\n\
         bar B {{\n  on change saver {{ ppd.profile = saver ? \"power-saver\" : \"balanced\"; lvl.n += 1 }}\n\
           text ppd.profile\n  input {{ text: <-> ppd.profile }}\n}}\n"
    );
    let out = one(&src);
    let lowered =
        strand_compiler::lower::lower(&out.program, strand_compiler::schema::Schema::builtin());
    // Every place the program writes: the two fields, by service name.
    let mut places: Vec<(String, Vec<String>)> = lowered
        .chunks
        .iter()
        .flat_map(|c| &c.places)
        .filter_map(|p| match &p.root {
            PlaceRoot::Service(s) => Some((
                s.clone(),
                p.segs
                    .iter()
                    .map(|g| match g {
                        PlaceSeg::Field(f) => f.clone(),
                        PlaceSeg::Index => "[]".into(),
                    })
                    .collect(),
            )),
            _ => None,
        })
        .collect();
    places.sort();
    places.dedup();
    assert_eq!(
        places,
        [
            ("lvl".to_string(), vec!["n".to_string()]),
            ("ppd".to_string(), vec!["profile".to_string()]),
        ]
    );
    // A field without `rw`, and the whole service, are not written.
    for (body, code) in [
        ("ppd.degraded = \"x\"", "check::read_only"),
        ("ppd = ppd", "check::read_only"),
    ] {
        let c = codes(&format!("{SVC}bar B {{ on click {{ {body} }} }}\n"));
        assert_eq!(c, [code], "{body}");
    }
}

/// Gaps 13 (m6-w2-services): `media.position` is writable, so a
/// scrubber binds it both ways and a click assigns it; the rest of
/// `media` stays read-only.
#[test]
fn media_position_is_a_scrubber() {
    assert_eq!(
        codes(
            "bar B {\n  slider { value: <-> media.position }\n  \
             text media.title ?? \"\"\n  on click { media.position = 0 }\n}\n"
        ),
        Vec::<String>::new()
    );
    assert_eq!(
        codes("bar B { on click { media.elapsed = media.elapsed } }\n"),
        ["check::read_only"]
    );
}

/// Gaps 2, 3 and 9 (m6-w2-services, review M2): the members a
/// workspace bar and a window list reach for check as written:
/// `workspaces.focus` with a number or a name, `workspaces.slots(screen)`,
/// `workspaces.configured`, and a window's `win.app?.categories`. A float
/// is no workspace number (`int | text`).
#[test]
fn workspace_and_window_members_check() {
    assert_eq!(
        codes(
            "bar B {\n  \
             for ws in workspaces.slots(screen) { text ws.name { on click { ws.focus() } } }\n  \
             text \"4\" { on click { workspaces.focus(4) } }\n  \
             text \"web\" { on click { workspaces.focus(\"web\") } }\n  \
             for ws in workspaces.configured { text ws.name }\n  \
             for win in windows.all { for c in win.app?.categories ?? [] key c { text c } }\n}\n"
        ),
        Vec::<String>::new()
    );
    assert_eq!(
        codes("bar B { text \"x\" { on click { workspaces.focus(true) } } }\n"),
        ["check::type_mismatch"]
    );
}

/// Gaps 17 (m6-w2-services): a power menu over the builtin `session`
/// service checks (its `can_*` fields, its actions, `lock_requested` and
/// `failed`). A config's own service named `session` shadows the builtin
/// one with a warning, the owner's ruling (spec.md 4.2,
/// `check::shadows_builtin`; decisions.md m6-w2-services, m6-w2-language
/// and m6-integration-w2), so a config that declared `service session`
/// before the builtin existed still loads (spec.md 14).
#[test]
fn session_is_a_power_menu() {
    assert_eq!(
        codes(
            "export state locked = false\n\
             bar B {\n  state why = \"\"\n  \
             on session.lock_requested { locked = true }\n  \
             on session.failed(action, error) { why = join(\" \", action, error) }\n  \
             text why\n  \
             if session.can_suspend { text \"Suspend\" { on click { session.suspend() } } }\n  \
             text \"Off\" { on click { if session.can_power_off { session.power_off() } } }\n  \
             text \"Lock\" { on click { session.lock() } }\n  \
             text \"Out\" { on click { session.log_out() } }\n}\n"
        ),
        Vec::<String>::new()
    );
    assert_eq!(
        codes(
            "service session from file \"s.json\" { user: text }\n\
             bar B { text session.user }\n"
        ),
        ["check::shadows_builtin"]
    );
}

/// (M6, owner-testing gap 18) `outputs` is a display settings page: every
/// output with its modes, test-only and applied configurations built
/// from `OutputChange` records (a null field keeps the output's value, a
/// bare `OutputTransform` variant where one is expected), and the
/// answers as events.
#[test]
fn outputs_is_a_display_settings_page() {
    assert_eq!(
        codes(
            "bar B {\n  state why = \"\"\n  \
             on outputs.succeeded(action) { why = action }\n  \
             on outputs.failed(action, error) { why = join(\" \", action, error) }\n  \
             text why\n  \
             if outputs.available {\n    \
             for o in outputs.all {\n      \
             text o.description\n      \
             text join(\" \", o.mode?.width, o.mode?.height, o.scale, o.x, o.y)\n      \
             for m in o.modes key join(\"x\", m.width, m.height, m.refresh) {\n        \
             text join(\"x\", m.width, m.height) {\n          \
             on click { outputs.test([OutputChange(name: o.name, mode: m)]) }\n        \
             }\n      \
             }\n      \
             text \"On\" { on click { outputs.apply([OutputChange(name: o.name, enabled: !o.enabled)]) } }\n      \
             text \"Turn\" { on click { outputs.apply([OutputChange(name: o.name, transform: rotate_90, scale: 1.5, x: o.x + 10, adaptive_sync: true)]) } }\n      \
             text \"4K\" { on click { outputs.apply([OutputChange(name: o.name, mode: OutputMode(width: 3840, height: 2160, refresh: 60))]) } }\n    \
             }\n  \
             }\n}\n"
        ),
        Vec::<String>::new()
    );
    // An action outside a handler, as for every service.
    assert_eq!(
        codes("bar B { text \"x\" { opacity: outputs.apply([]) } }\n"),
        ["check::action_in_binding"]
    );
}

/// (M6, owner-testing gap 32) A player's buttons dim by what the active
/// player can do: `media.can_next`, `can_previous` and `can_seek` are
/// plain booleans, read in bindings and `when`s.
#[test]
fn media_buttons_dim_by_what_the_player_can_do() {
    assert_eq!(
        codes(
            "bar B {\n  row {\n    \
             text \"prev\" { opacity: media.can_previous ? 1 : 0.4\n      on click { media.previous() } }\n    \
             text \"next\" { opacity: media.can_next ? 1 : 0.4\n      on click { media.next() } }\n    \
             slider { value: <-> media.position\n      when !media.can_seek { opacity: 0.4 } }\n  \
             }\n}\n"
        ),
        Vec::<String>::new()
    );
}

/// (M6, owner-testing gap 34) A dashboard shows the uptime and the
/// kernel: `system.uptime` is a `duration` for `dur`, `system.kernel` a
/// text.
#[test]
fn system_shows_uptime_and_kernel() {
    assert_eq!(
        codes(
            "bar B {\n  row {\n    text dur(system.uptime)\n    text system.kernel\n    \
             text join(\" \", system.hostname, system.kernel)\n  }\n}\n"
        ),
        Vec::<String>::new()
    );
}

/// `from dbus` fields against the object's introspection: a missing
/// property (with a did-you-mean), a type that does not convert, `rw` on
/// a read-only property, and a bus that cannot be reached (a warning, the
/// config still loads).
#[test]
fn dbus_services_are_checked_against_introspection() {
    use strand_compiler::check::dbus::{BusProperty, Introspect, check};
    struct Fake(Option<Result<Vec<BusProperty>, String>>);
    impl Introspect for Fake {
        fn properties(
            &self,
            system: bool,
            name: &str,
            path: &str,
        ) -> Option<Result<Vec<BusProperty>, String>> {
            assert!(system);
            assert_eq!(name, "net.hadess.PowerProfiles");
            assert_eq!(path, "/net/hadess/PowerProfiles");
            self.0.clone()
        }
    }
    let prop = |name: &str, sig: &str, writable: bool| BusProperty {
        interface: "net.hadess.PowerProfiles".into(),
        name: name.into(),
        signature: sig.into(),
        writable,
    };
    let ppd = Fake(Some(Ok(vec![
        prop("ActiveProfile", "s", true),
        prop("PerformanceDegraded", "s", false),
        prop("Profiles", "aa{sv}", false),
    ])));
    let src = |fields: &str| {
        format!(
            "service ppd from dbus system \"net.hadess.PowerProfiles\" {{ {fields} }}\nbar B {{ text \"x\" }}\n"
        )
    };
    let run = |fields: &str, intro: &Fake| {
        let (out, _) = compile_files(&[("a.strand", src(fields))]);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        check(&out.program, intro)
    };
    assert!(
        run(
            "profile: text rw = ActiveProfile; degraded: text = PerformanceDegraded",
            &ppd
        )
        .is_empty()
    );
    let d = run("profile: text rw = ActiveProfil", &ppd);
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(d[0].code, "check::dbus_property");
    assert_eq!(d[0].help.as_deref(), Some("did you mean `ActiveProfile`?"));
    assert_eq!(d[0].suggestions[0].replacement, "ActiveProfile");
    let d = run("profile: int = ActiveProfile", &ppd);
    assert_eq!(d[0].code, "check::dbus_type", "{d:?}");
    assert!(d[0].message.contains("`s`"), "{}", d[0].message);
    let d = run("degraded: text rw = PerformanceDegraded", &ppd);
    assert_eq!(d[0].code, "check::dbus_read_only", "{d:?}");
    // An answer still being asked for (the LSP's): not checked, no word.
    assert!(run("profile: int rw = Nope", &Fake(None)).is_empty());
    let down = Fake(Some(Err("cannot reach the bus: no such file".into())));
    let d = run("profile: text rw = ActiveProfile", &down);
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].code, "check::dbus_unchecked");
    assert!(!d[0].is_error(), "a warning: the config still loads");
    assert!(
        d[0].message.contains("cannot reach the bus"),
        "{}",
        d[0].message
    );
}

/// One mistake in a no-code service's source is one diagnostic: the
/// parser's, never also a checker error on the same source.
#[test]
fn a_bad_service_source_is_one_diagnostic() {
    for (src, code) in [
        (
            "service t from poll \"/tmp/x\" { a: text }",
            "syntax::expected",
        ),
        (
            "service t from dbus systm \"net.hadess.PowerProfiles\" { a: text = A }",
            "syntax::unknown_bus",
        ),
        (
            "service t from pol \"x\" every 1s { a: text }",
            "syntax::unknown_source",
        ),
        (
            "service t from dbus \"net.hadess.PowerProfiles\" { a: text = A }",
            "syntax::expected",
        ),
        ("service t from file { a: text }", "syntax::expected"),
        (
            "service t from file \"/a\" \"/b\" { a: text }",
            "syntax::expected",
        ),
    ] {
        let (out, _) = compile_files(&[("a.strand", src.to_string())]);
        let codes: Vec<String> = out.diagnostics.iter().map(|d| d.code.to_string()).collect();
        assert_eq!(codes, [code], "{src}");
    }
}

/// decisions.md m4-owner: `letters` animates the text of its enclosing
/// `text` node, so a leaf `text` holds a `letters` child; any other child
/// is still an error.
#[test]
fn a_text_holds_letters_and_nothing_else() {
    one("bar B {\n  text \"x\" { letters { y: 2 * wave(1s, phase: index * 0.1) } }\n}\n");
    // Any other child of a `text`, and `letters` in any other leaf, are
    // still refused.
    for src in [
        "text \"x\" { box {} }",
        "icon \"x\" { letters { y: 2 } }",
        "meter { letters { y: 2 } }",
        "image { letters { y: 2 } }",
        "input { letters { y: 2 } }",
    ] {
        let (out, map) = compile_files(&[("a.strand", format!("bar B {{\n  {src}\n}}\n"))]);
        let codes: Vec<&str> = out.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            codes.contains(&"check::children_not_allowed"),
            "{src}: {}",
            render(&out.diagnostics, &map, Style::Plain)
        );
    }
}

/// `check::lock_no_auth`: a lock is accepted when `auth.submit` is
/// reachable from it (a handler, a timer, a branch, a component it
/// shows) and refused when the only call is outside it.
#[test]
fn a_lock_needs_a_reachable_auth_submit() {
    let way_out = |src: &str| {
        let (out, map) = compile_files(&[("shell.strand", src.to_string())]);
        let codes: Vec<&str> = out.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            !out.diagnostics.iter().any(|d| d.is_error()) || codes == ["check::lock_no_auth"],
            "{}",
            render(&out.diagnostics, &map, Style::Plain)
        );
        !codes.contains(&"check::lock_no_auth")
    };
    let field = "input { type: password; text: <-> pw; on activate { auth.submit(pw) } }";
    assert!(way_out(&format!("lock {{ state pw = \"\"\n  {field} }}\n")));
    assert!(way_out(&format!(
        "state typing = false\nlock {{ state pw = \"\"\n  if typing {{ {field} }} else {{ text \"locked\" }} }}\n"
    )));
    assert!(way_out(&format!(
        "lock {{ Field }}\ncomponent Field {{ state pw = \"\"\n  {field} }}\n"
    )));
    assert!(way_out(
        "lock { every 1s { auth.submit(\"\") }; text \"x\" }\n"
    ));
    assert!(!way_out("lock { text clock.format(\"%H:%M\") }\n"));
    assert!(!way_out(
        "lock { Face }\ncomponent Face { text \"x\" }\nbar Top { on key(k) { auth.submit(k.name) }; text \"b\" }\n"
    ));
}

/// (m4-audit) A popup or tooltip never opens on a lock surface, so an
/// `auth.submit` inside one is no way out (`check::lock_no_auth`), and
/// each is the warning `check::lock_popup`, in the lock or in a component
/// it shows.
#[test]
fn a_popup_in_a_lock_is_no_way_out() {
    let field = "input { type: password; text: <-> s; on activate { auth.submit(s) } }";
    let codes = |src: String| {
        let (out, _) = compile_files(&[("shell.strand", src)]);
        let mut codes: Vec<&str> = out.diagnostics.iter().map(|d| d.code).collect();
        codes.retain(|c| c.starts_with("check::lock"));
        codes.sort();
        codes
    };
    assert_eq!(
        codes(format!(
            "lock {{ state ask = false\n  state s = \"\"\n  box {{ on click {{ ask = true }}; text \"x\" }}\n  popup {{ open: <-> ask; {field} }} }}\n"
        )),
        ["check::lock_no_auth", "check::lock_popup"]
    );
    assert_eq!(
        codes(format!(
            "lock {{ Face }}\ncomponent Face {{ state s = \"\"\n  box {{ text \"x\"; tooltip {{ {field} }} }} }}\n"
        )),
        ["check::lock_no_auth", "check::lock_popup"]
    );
    // A field beside the popup is a way out; the popup still warns.
    assert_eq!(
        codes(format!(
            "lock {{ state s = \"\"\n  {field}\n  popup {{ text \"hint\" }} }}\n"
        )),
        ["check::lock_popup"]
    );
}

/// (m4-audit) `check::lock_no_auth`'s suggested field compiles, gives the
/// lock its way out, and does not send an empty password: with
/// pam_faillock each empty Return pressed to wake the screen would count
/// as a failed login.
#[test]
fn the_lock_no_auth_help_compiles_and_skips_an_empty_password() {
    let (out, _) = compile_files(&[("shell.strand", "lock { text \"x\" }\n".to_string())]);
    let help = out
        .diagnostics
        .iter()
        .find(|d| d.code == "check::lock_no_auth")
        .and_then(|d| d.help.clone())
        .unwrap();
    let field = help.split('`').nth(1).unwrap();
    assert!(
        field.contains("if secret != \"\" { auth.submit(secret) }"),
        "{field}"
    );
    let src = format!("lock {{ state secret = \"\"\n  {field} }}\n");
    let (out, map) = compile_files(&[("shell.strand", src)]);
    assert!(
        !out.diagnostics.iter().any(|d| d.is_error()),
        "{}",
        render(&out.diagnostics, &map, Style::Plain)
    );
}

/// (m4-audit) `transition:` takes design.md's masks (`wipe(left)`, `disc`,
/// `dissolve`, `pixelate`) or `none`; `crossfade`, which the schema once
/// offered and render never drew, is an error.
#[test]
fn transition_takes_the_design_masks_only() {
    for ok in ["wipe(left)", "disc", "dissolve", "pixelate", "none"] {
        let (out, _) = compile_files(&[(
            "a.strand",
            format!("bar Top {{\n  box {{ transition: {ok}; width: 10 }}\n}}\n"),
        )]);
        let errors: Vec<_> = out.diagnostics.iter().filter(|d| d.is_error()).collect();
        assert!(errors.is_empty(), "{ok}: {errors:?}");
    }
    let (out, _) = compile_files(&[(
        "a.strand",
        "bar Top {\n  box { transition: crossfade; width: 10 }\n}\n".to_string(),
    )]);
    assert!(
        out.diagnostics.iter().any(|d| d.is_error()),
        "{:?}",
        out.diagnostics
    );
}

/// (M6) Declared actions (decisions.md m6-w2-language, the owner's
/// ruling): the program must be allowed by `permit exec`; a call is
/// checked like a function's (arity, types) and runs only in a handler;
/// the command holds texts and parameters only, the program written out;
/// a parameter's type is one an argument carries; a parameter the
/// command never passes is a warning.
#[test]
fn actions_are_checked_like_calls_and_against_permit_exec() {
    const OK: &str = "permit exec \"hyprctl\"\n\
        action dispatch(ws: int, name: text) [\"hyprctl\", \"dispatch\", \"workspace\", ws, name]\n";
    let with = |decls: &str, body: &str| codes(&format!("{decls}bar B {{ {body} }}\n"));
    assert!(with(OK, "on click { dispatch(3, \"a b\") }").is_empty());
    // Not permitted: the program by name, or no `permit exec` at all.
    assert_eq!(
        with(
            "permit exec \"swaymsg\"\naction go(ws: int) [\"hyprctl\", \"dispatch\", ws]\n",
            "on click { go(1) }"
        ),
        ["check::no_permit"]
    );
    assert_eq!(
        with(
            "action go(ws: int) [\"hyprctl\", ws]\n",
            "on click { go(1) }"
        ),
        ["check::no_permit"]
    );
    // A bare `permit exec` allows any program.
    assert!(
        with(
            "permit exec\naction go(ws: int) [\"x\", ws]\n",
            "on click { go(1) }"
        )
        .is_empty()
    );
    // Arity and types, as a function's.
    assert_eq!(with(OK, "on click { dispatch(3) }"), ["check::missing_arg"]);
    assert_eq!(
        with(OK, "on click { dispatch(3, \"a\", 4) }"),
        ["check::too_many_args"]
    );
    assert_eq!(
        with(OK, "on click { dispatch(\"3\", \"a\") }"),
        ["check::type_mismatch"]
    );
    // Only a handler runs it.
    assert_eq!(
        with(OK, "text dispatch(1, \"a\")"),
        ["check::action_in_binding"]
    );
    // The command: texts and parameters, the program written out.
    assert_eq!(
        with(
            "permit exec\naction go(p: text) [p, \"x\"]\n",
            "on click { go(\"a\") }"
        ),
        ["check::not_constant"]
    );
    assert_eq!(
        with(
            "permit exec\naction go(p: text) [\"x\", p + \"y\"]\n",
            "on click { go(\"a\") }"
        ),
        ["check::not_constant"]
    );
    assert_eq!(
        with(
            "permit exec\naction go(p: text) [\"x\", q]\n",
            "on click { go(\"a\") }"
        ),
        ["check::not_constant"]
    );
    assert_eq!(
        with(
            "permit exec\naction go(p: text) \"x\"\n",
            "on click { go(\"a\") }"
        ),
        ["syntax::expected"]
    );
    // A parameter an argument cannot carry, and one never passed.
    assert_eq!(
        with(
            "permit exec\naction go(c: color) [\"x\", c]\n",
            "on click { go(#fff) }"
        ),
        ["check::type_mismatch"]
    );
    assert_eq!(
        with(
            "permit exec\naction go(p: text?) [\"x\", p]\n",
            "on click { go(\"a\") }"
        ),
        ["check::type_mismatch"]
    );
    assert_eq!(
        with(
            "permit exec\naction go(p: text, q: int) [\"x\", p]\n",
            "on click { go(\"a\", 1) }"
        ),
        ["check::unknown_param"]
    );
    // A `from dbus` service's action calls a method; other sources have
    // none.
    let svc = "service ppd from dbus system \"net.hadess.PowerProfiles\" {\n  \
        profile: text = ActiveProfile\n  \
        action hold(profile: text, reason: text, app: text) = HoldProfile\n}\n";
    assert!(
        with(
            svc,
            "on click { ppd.hold(\"performance\", \"x\", \"strand\") }"
        )
        .is_empty()
    );
    assert_eq!(
        with(svc, "on click { ppd.hold(\"performance\") }"),
        ["check::missing_arg"]
    );
    assert_eq!(
        with(svc, "text ppd.hold(\"a\", \"b\", \"c\")"),
        ["check::action_in_binding"]
    );
    assert_eq!(
        with(
            "service h from file \"/etc/hostname\" {\n  name: text\n  action go() = Go\n}\n",
            "text h.name"
        ),
        ["check::misplaced"]
    );
}

/// (M6) A `from dbus` service's actions against its object's methods: a
/// missing method (with a did-you-mean), another number of arguments, and
/// a parameter that does not convert to its argument's D-Bus type.
#[test]
fn dbus_actions_are_checked_against_introspection() {
    use strand_compiler::check::dbus::{BusMethod, BusProperty, Introspect, check};
    struct Fake;
    impl Introspect for Fake {
        fn properties(
            &self,
            _: bool,
            _: &str,
            _: &str,
        ) -> Option<Result<Vec<BusProperty>, String>> {
            Some(Ok(vec![BusProperty {
                interface: "net.hadess.PowerProfiles".into(),
                name: "ActiveProfile".into(),
                signature: "s".into(),
                writable: true,
            }]))
        }
        fn methods(&self, _: bool, _: &str, _: &str) -> Option<Result<Vec<BusMethod>, String>> {
            let m = |name: &str, inputs: &[&str]| BusMethod {
                interface: "net.hadess.PowerProfiles".into(),
                name: name.into(),
                inputs: inputs.iter().map(|s| s.to_string()).collect(),
            };
            Some(Ok(vec![
                m("HoldProfile", &["s", "s", "s"]),
                m("ReleaseProfile", &["u"]),
            ]))
        }
    }
    let run = |actions: &str| {
        let src = format!(
            "service ppd from dbus system \"net.hadess.PowerProfiles\" {{\n  profile: text = ActiveProfile\n{actions}\n}}\nbar B {{ text \"x\" }}\n"
        );
        let (out, _) = compile_files(&[("a.strand", src)]);
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        check(&out.program, &Fake)
    };
    assert!(
        run(
            "  action hold(p: text, r: text, a: text) = HoldProfile\n  action release(c: int) = ReleaseProfile"
        )
        .is_empty()
    );
    let d = run("  action hold(p: text, r: text, a: text) = HoldProfil");
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(d[0].code, "check::dbus_method");
    assert_eq!(d[0].suggestions[0].replacement, "HoldProfile");
    let d = run("  action hold(p: text) = HoldProfile");
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(d[0].code, "check::dbus_method");
    assert!(
        d[0].message.contains("takes 3 arguments"),
        "{}",
        d[0].message
    );
    let d = run("  action release(c: text) = ReleaseProfile");
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(d[0].code, "check::dbus_type");
    // A `float` never goes to an integer argument.
    let d = run("  action release(c: float) = ReleaseProfile");
    assert_eq!(d[0].code, "check::dbus_type", "{d:?}");
}

/// (M6) `$palette.<name>` outside templates (owner testing Gaps 10): an
/// extra is a `color` (which ones a palette has is known only when it
/// is made), `name` and `source` are `text`, `dark` a `bool`; the bare
/// group is an error, and so is reading the palette where it is made (a
/// token's value, a `use` clause).
#[test]
fn palette_extras_and_identity_check_outside_templates() {
    let bar = |body: &str| codes(&format!("bar B {{ {body} }}\n"));
    assert!(bar("box { bg: $palette.mantle }").is_empty());
    assert!(bar("box { bg: $palette.mantle ?? $accent }").is_empty());
    assert!(bar("box { bg: $palette.crust.alpha(0.5) }").is_empty());
    assert!(bar("box { bg: $palette.color.brand_primary }").is_empty());
    assert!(bar("text $palette.name").is_empty());
    assert!(bar("text $palette.dark ? \"dark\" : \"light\"").is_empty());
    assert_eq!(bar("text $palette.mantle"), ["check::type_mismatch"]);
    assert_eq!(bar("box { bg: $palette.name }"), ["check::type_mismatch"]);
    assert_eq!(bar("box { bg: $palette }"), ["check::unknown_token"]);
    assert_eq!(
        codes("tokens t { panel: $palette.mantle }\nbar B { text \"x\" }\n"),
        ["check::palette_in_tokens"]
    );
    assert_eq!(
        codes("use palette material(seed: $palette.mauve)\nbar B { text \"x\" }\n"),
        ["check::palette_in_tokens"]
    );
}

/// (M6, the owner's ruling) A config `service` named like a builtin one
/// shadows it within the config, with a warning, as a `let` or `state`
/// hiding a name does: the config's own fields are the ones read, and
/// the builtin's are not there (spec.md 4.2, decisions.md
/// m6-w2-language).
#[test]
fn a_config_service_shadows_a_builtin_one() {
    let svc = "service battery from file \"/tmp/level\" { level: int }\n";
    let (out, map) = compile_files(&[(
        "a.strand",
        format!("{svc}bar B {{ text \"{{battery.level}}\" }}\n"),
    )]);
    let codes: Vec<_> = out.diagnostics.iter().map(|d| d.code).collect();
    assert_eq!(
        codes,
        ["check::shadows_builtin"],
        "{}",
        render(&out.diagnostics, &map, Style::Plain)
    );
    assert!(!out.diagnostics[0].is_error());
    assert!(
        out.diagnostics[0]
            .message
            .contains("hides the builtin service `battery`"),
        "{}",
        out.diagnostics[0].message
    );
    // The builtin's fields are hidden with it.
    let (out, map) = compile_files(&[(
        "a.strand",
        format!("{svc}bar B {{ text battery.percent }}\n"),
    )]);
    let c: Vec<_> = out.diagnostics.iter().map(|d| d.code).collect();
    assert_eq!(
        c,
        ["check::shadows_builtin", "check::unknown_field"],
        "{}",
        render(&out.diagnostics, &map, Style::Plain)
    );
}

/// (M6) A literal list of whole numbers is `[int]` where it is looped
/// over, so its items pass to an `int` parameter, as a bar's workspace
/// buttons pass them to `workspaces.focus` (owner testing Gaps 12).
#[test]
fn a_looped_whole_number_list_gives_ints() {
    let src = "fn pick(n: int) -> int { n }\n\
        let ids = [1, 2, 3]\n\
        bar B {\n  \
        for w in [1, 2, 3] key w { text join(\" \", pick(w)) }\n  \
        for w in ids key w { text join(\" \", pick(w)) }\n  \
        row { for w in [4, 5] key w { box { on click { let k = pick(w) } } } }\n}\n";
    let (out, map) = compile_files(&[("a.strand", src.to_string())]);
    assert!(
        out.diagnostics.iter().all(|d| !d.is_error()),
        "{}",
        render(&out.diagnostics, &map, Style::Plain)
    );
}

/// (M6 wave 2 integration) A looped literal list of whole numbers
/// switches workspaces: its items are `int`s (m6-w2-language), which
/// `workspaces.focus(target: int | text)` takes (m6-w2-services), so a
/// bar's persistent workspace buttons need no conversion; a looped list
/// of names is the text arm.
#[test]
fn a_looped_whole_number_list_focuses_workspaces() {
    let src = "bar B {\n  \
        for w in [1, 2, 3] key w { text \"{w}\" { on click { workspaces.focus(w) } } }\n  \
        for n in [\"web\", \"mail\"] key n { text n { on click { workspaces.focus(n) } } }\n}\n";
    let (out, map) = compile_files(&[("a.strand", src.to_string())]);
    assert!(
        out.diagnostics.is_empty(),
        "{}",
        render(&out.diagnostics, &map, Style::Plain)
    );
}

/// (M6, owner testing Gaps 8) A handler or `id:` on a component call is
/// about its root element: checked against that element's events, and
/// a load error naming the fix where the component's tree is not one
/// element.
#[test]
fn handlers_on_a_call_are_its_root_elements() {
    let comps = "component Chip { box { text \"a\" } }\n\
        component Two { box {}\n  box {} }\n";
    let ok = codes(&format!(
        "{comps}state n = 0\nbar B {{ Chip {{ id: c; on click {{ n += 1 }}; on scroll(dy) {{ n += 1 }} }}\n  text \"{{c.hover}}\" }}\n"
    ));
    assert!(ok.is_empty(), "{ok:?}");
    let bad = codes(&format!("{comps}bar B {{ Chip {{ on clack {{ }} }} }}\n"));
    assert_eq!(bad, ["check::unknown_event"]);
    let two = codes(&format!("{comps}bar B {{ Two {{ on click {{ }} }} }}\n"));
    assert_eq!(two, ["check::misplaced"]);
    let two_id = codes(&format!("{comps}bar B {{ Two {{ id: t }} }}\n"));
    assert_eq!(two_id, ["check::misplaced"]);
}

/// (M6, owner testing Gaps 7) A component reads `screen`, the screen of
/// the bar it is in; a call of one where no bar is (a `panel`) is a load
/// error at the call, through components that call it too. A
/// `screen` declared at the top level or as a parameter is the one read.
#[test]
fn a_components_screen_comes_from_its_bar() {
    let comps = "component Name { text screen.name }\ncomponent Line { row { Name } }\n";
    let ok = codes(&format!("{comps}bar B {{ Line }}\n"));
    assert!(ok.is_empty(), "{ok:?}");
    let panel = codes(&format!("{comps}panel P {{ Line }}\n"));
    assert_eq!(panel, ["check::no_screen"]);
    // Never called: nothing to pass, nothing to report.
    assert!(codes(comps).is_empty());
    // A parameter named `screen` is the one read, and callers pass it.
    let param = codes("component Name(screen: text) { text screen }\npanel P { Name \"x\" }\n");
    assert!(param.is_empty(), "{param:?}");
    // A top-level `screen` is never hidden by a component's.
    let top = codes("let screen = \"x\"\ncomponent Name { text screen }\npanel P { Name }\n");
    assert!(top.is_empty(), "{top:?}");
}

/// (M6, the review of Gaps 12) Programs that loaded before whole-number
/// lists kept their `int`s through `for`, `?:` and `match` still load: a
/// declaration whose value is a whole number widens to `float` when a
/// fraction is written to it, as a literal-initialised one does, and a
/// fraction pushed or inserted into a whole-number list widens the list.
#[test]
fn whole_number_values_widen_where_fractions_are_written() {
    let cases = [
        // A `state` from a `for` binding over a whole-number literal.
        "bar B {\n  row {\n    for w in [1, 2, 3] key w {\n      state size = w\n      \
         text join(\" \", size) { on click { size = size / 2 } }\n    }\n  }\n}\n",
        // An inferred parameter from such a binding.
        "component Dot(n) {\n  state r = n\n  text join(\" \", r) { on click { r += 0.5 } }\n}\n\
         bar B { row { for x in [4, 8] key x { Dot x } } }\n",
        // A whole-number list through `?:`, then a fraction pushed.
        "state big = true\nstate marks = big ? [10, 20] : []\n\
         bar B { text \"m\" { on click { marks.push(2.5) } } }\n",
        // An item of a whole-number `let`, then a fraction written.
        "state wide = true\nlet sizes = wide ? [20, 30] : [10, 20]\nstate cur = sizes[0]\n\
         bar B { text \"c\" { on click { cur = cur * 1.5 } } }\n",
        // `insert` too, into a literal-initialised list.
        "state xs = [1, 2]\nbar B { text \"x\" { on click { xs.insert(0, 0.5) } } }\n",
    ];
    for src in cases {
        let (out, map) = compile_files(&[("a.strand", src.to_string())]);
        assert!(
            out.diagnostics.iter().all(|d| !d.is_error()),
            "{src}\n{}",
            render(&out.diagnostics, &map, Style::Plain)
        );
    }
    // Widened, each is a `float` everywhere it is read.
    let c = codes(
        "fn f(n: int) -> int { n }\nstate xs = [1, 2]\n\
         bar B { text \"x\" { on click { xs.push(0.5); let k = f(xs[0]) } } }\n",
    );
    assert_eq!(c, ["check::type_mismatch"]);
}

/// (M6, the review of Gaps 7) A component's own `screen` is found only
/// where the name is otherwise unknown: an enum variant named `screen`
/// is still the one a component reads, and the component takes no
/// `screen` for it.
#[test]
fn a_screen_variant_wins_over_a_components_screen() {
    let src = "enum Shot { screen, window }\nstate s = Shot.window\n\
        component Pick { text \"x\" { on click { let k = screen; s = k } } }\n\
        panel P { Pick }\n";
    let c = codes(src);
    assert!(c.is_empty(), "{c:?}");
}

/// (M6, the review of the owner's ruling) `on notifications.received`
/// with a config `service notifications` is about the config's service:
/// an event it does not declare is a load error, not the builtin's event.
#[test]
fn a_config_services_events_shadow_the_builtins() {
    let c = codes(
        "service notifications from file \"/tmp/b.json\" { level: int }\n\
         bar B { text \"x\" { on notifications.received(n) { } } }\n",
    );
    assert!(c.contains(&"check::unknown_event"), "{c:?}");
}

/// (M6, the review of the owner's ruling) A service whose properties
/// were read but whose methods could not be is warned about, as an
/// unread service is: its actions were not checked.
#[test]
fn unread_methods_leave_actions_unchecked_with_a_warning() {
    use strand_compiler::check::dbus::{BusMethod, BusProperty, Introspect, check};
    struct NoMethods;
    impl Introspect for NoMethods {
        fn properties(
            &self,
            _: bool,
            _: &str,
            _: &str,
        ) -> Option<Result<Vec<BusProperty>, String>> {
            Some(Ok(vec![BusProperty {
                interface: "net.hadess.PowerProfiles".into(),
                name: "ActiveProfile".into(),
                signature: "s".into(),
                writable: true,
            }]))
        }
        fn methods(&self, _: bool, _: &str, _: &str) -> Option<Result<Vec<BusMethod>, String>> {
            Some(Err("timed out".into()))
        }
    }
    let src = "service ppd from dbus system \"net.hadess.PowerProfiles\" {\n  profile: text = ActiveProfile\n  action hold(p: text) = HoldProfile\n}\nbar B { text \"x\" }\n".to_string();
    let (out, _) = compile_files(&[("a.strand", src)]);
    assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    let d = check(&out.program, &NoMethods);
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(d[0].code, "check::dbus_unchecked");
    assert!(d[0].message.contains("timed out"), "{}", d[0].message);
}

/// (M6, the review of the owner's ruling) A command `permit exec` does
/// not allow keeps no argv (nothing could run it, whatever the loader
/// does with errors), and a parameter right after a shell's `-c` (alone
/// or among short options) is warned about: the shell runs it as a
/// script. A parameter after the script (`$1`) is not.
#[test]
fn refused_commands_keep_no_argv_and_shell_scripts_are_warned_about() {
    let argv = |src: &str| {
        let (out, _) = compile_files(&[("a.strand", src.to_string())]);
        let a = out
            .program
            .files
            .iter()
            .flat_map(|f| &f.items)
            .find_map(|i| match i {
                strand_compiler::hir::Item::Action(a) => Some(a.argv.len()),
                _ => None,
            });
        (
            a,
            out.diagnostics.iter().map(|d| d.code).collect::<Vec<_>>(),
        )
    };
    let (n, c) = argv(
        "permit exec \"swaymsg\"\naction go(ws: int) [\"hyprctl\", ws]\nbar B { text \"x\" }\n",
    );
    assert_eq!((n, c), (Some(0), vec!["check::no_permit"]));
    for cmd in [
        "[\"sh\", \"-c\", s]",
        "[\"/bin/bash\", \"-ec\", s]",
        "[\"dash\", \"-c\", s, \"x\"]",
    ] {
        let (n, c) = argv(&format!(
            "permit exec\naction go(s: text) {cmd}\nbar B {{ text \"x\" }}\n"
        ));
        assert_eq!(c, ["check::shell_script"], "{cmd}");
        assert!(n.unwrap_or(0) > 0, "still runs: {cmd}");
    }
    for cmd in [
        "[\"sh\", \"-c\", \"echo \\\"$1\\\"\", \"sh\", s]",
        "[\"hyprctl\", \"-c\", s]",
        "[\"sh\", \"--\", s]",
    ] {
        let (_, c) = argv(&format!(
            "permit exec\naction go(s: text) {cmd}\nbar B {{ text \"x\" }}\n"
        ));
        assert!(c.is_empty(), "{cmd}: {c:?}");
    }
}

/// (M6, the owner's ruling of 2026-10-14) Two files may each declare a
/// component, type, enum and `fn` of one name. The files `a` and `b`
/// below give each kind a different shape, so a use resolved to the
/// wrong declaration is a type error.
const SAME_A: &str = "component Row(n: int) { text \"a\" }\n\
    type Item { a: int }\n\
    enum Kind { x, y }\n\
    fn f() -> int { 1 }\n\
    let item = Item(a: 1)\n\
    state kind = Kind.x\n\
    let n: int = f()\n\
    bar A { Row 3 }\n";
const SAME_B: &str = "component Row(s: text) { text s }\n\
    type Item { b: text }\n\
    enum Kind { p, q }\n\
    fn f() -> text { \"b\" }\n\
    let item = Item(b: \"s\")\n\
    state kind = Kind.p\n\
    let label: text = f()\n\
    panel P { Row \"s\" }\n";

fn same_named(c: &str) -> (Compiled, SourceMap) {
    compile_files(&[
        ("a.strand", SAME_A.to_string()),
        ("b.strand", SAME_B.to_string()),
        ("c.strand", c.to_string()),
    ])
}

/// Rule 1: a file that declares a name and uses it gets its own, for
/// each kind; rule 2: a name only one file declares is the one every
/// other file gets, as before.
#[test]
fn a_file_uses_its_own_declaration_of_a_shared_name() {
    let (out, map) = same_named("let only = solo()\nfn solo() -> int { 2 }\n");
    let errs: Vec<_> = out.diagnostics.iter().filter(|d| d.is_error()).collect();
    assert!(
        errs.is_empty(),
        "{}",
        render(&out.diagnostics, &map, Style::Plain)
    );
    // Rule 2: `Unique`, declared in `a` alone, is read from `c`.
    let (out, map) = compile_files(&[
        (
            "a.strand",
            "component Unique { text \"u\" }\ntype Pair { x: int }\n".to_string(),
        ),
        (
            "c.strand",
            "let p = Pair(x: 1)\nbar C { Unique }\n".to_string(),
        ),
    ]);
    assert!(
        out.diagnostics.is_empty(),
        "{}",
        render(&out.diagnostics, &map, Style::Plain)
    );
    // Each use refers to its own file's declaration.
    let (out, _) = same_named("");
    for file in ["a.strand", "b.strand"] {
        let id = map_id(&out, file);
        for r in out.program.refs.iter().filter(|r| r.file == id) {
            if let Target::Def(d) = r.target {
                let def = out.program.def(d);
                if ["Row", "Item", "Kind", "f"].contains(&def.name.as_str()) {
                    assert_eq!(
                        def.file, id,
                        "{file}: `{}` read from another file",
                        def.name
                    );
                }
            }
        }
    }
}

fn map_id(out: &Compiled, file: &str) -> strand_compiler::source::FileId {
    out.program
        .files
        .iter()
        .find(|f| format!("{}.strand", f.name) == file)
        .unwrap()
        .file
}

/// Rule 3: a bare use in a file without its own, with two declarations
/// elsewhere, is `check::ambiguous_name` at the use, for each kind; the
/// error names both files and offers both qualified forms.
#[test]
fn a_bare_use_of_a_name_two_other_files_declare_is_ambiguous() {
    for (c, at) in [
        ("bar C { Row 3 }\n", "Row 3"),
        ("let i = Item(a: 1)\n", "Item(a"),
        ("let i: Item? = null\n", "Item? "),
        ("state k = Kind.x\n", "Kind.x"),
        ("let n = f()\n", "f()"),
        (
            "fn g(k: Kind) -> int { match k { Kind.x => 1, _ => 2 } }\n",
            "Kind)",
        ),
    ] {
        let (out, map) = same_named(c);
        let amb: Vec<_> = out
            .diagnostics
            .iter()
            .filter(|d| d.code == "check::ambiguous_name")
            .collect();
        assert!(
            !amb.is_empty(),
            "{c}: {}",
            render(&out.diagnostics, &map, Style::Plain)
        );
        let d = amb[0];
        let name = &at[..at
            .find(|ch: char| !ch.is_alphanumeric())
            .unwrap_or(at.len())];
        assert_eq!(
            d.message,
            format!(
                "`{name}` is declared in 2 files, `a.strand` and `b.strand`, and not in this one"
            ),
            "{c}"
        );
        let span = d.primary_span().unwrap();
        assert_eq!(&c[span.start as usize..span.end as usize], name, "{c}");
        let fixes: Vec<&str> = d
            .suggestions
            .iter()
            .map(|s| s.replacement.as_str())
            .collect();
        assert_eq!(fixes, [format!("a.{name}"), format!("b.{name}")], "{c}");
        assert_eq!(
            d.help.as_deref(),
            Some(
                format!(
                    "name the file it comes from: `a.{name}` or `b.{name}` (or declare `{name}` in this file, which then uses its own)"
                )
                .as_str()
            ),
            "{c}"
        );
        // Each declaring file is a secondary label.
        assert_eq!(d.labels.iter().filter(|l| !l.primary).count(), 2, "{c}");
    }
}

/// `file.Name` names the declaration of that file everywhere a bare
/// name stands: a component call (bare, with an argument, with a
/// block), a type in an annotation, a constructor, an enum's variant in
/// a value and a pattern, an enum as a type, and a `fn` call.
#[test]
fn qualified_names_reach_one_files_declaration() {
    let c = "let i = b.Item(b: \"x\")\n\
        let j: a.Item = a.Item(a: 2)\n\
        state k: a.Kind = a.Kind.y\n\
        let kb = b.Kind.q\n\
        let n: int = a.f()\n\
        let label: text = b.f()\n\
        let m = match k { a.Kind.x => 1, a.Kind.y => 2 }\n\
        fn g(k: a.Kind) -> a.Kind { k }\n\
        bar C {\n  row {\n    a.Row 3\n    b.Row \"s\" { on click { } }\n  }\n}\n";
    let (out, map) = same_named(c);
    assert!(
        out.diagnostics.iter().all(|d| !d.is_error()),
        "{}",
        render(&out.diagnostics, &map, Style::Plain)
    );
    // A file that declares no such name, and a name no file has.
    let (out, _) = same_named("bar C { a.Rwo 3 }\n");
    let d = out
        .diagnostics
        .iter()
        .find(|d| d.code == "check::unknown_name")
        .unwrap();
    assert_eq!(d.message, "`a.strand` declares no `Rwo`");
    assert_eq!(d.suggestions[0].replacement, "Row");
    let (out, _) = same_named("bar C { zz.Row 3 }\n");
    assert!(
        out.diagnostics
            .iter()
            .any(|d| d.message == "no file `zz.strand` in this config"),
        "{:?}",
        out.diagnostics
    );
    // Types and enums named in two files are shown qualified.
    let (out, _) = same_named("let i: a.Item = b.Item(b: \"x\")\n");
    let d = out
        .diagnostics
        .iter()
        .find(|d| d.code == "check::type_mismatch")
        .unwrap();
    assert!(
        d.message.contains("a.Item") && d.message.contains("b.Item"),
        "{}",
        d.message
    );
}

/// A file whose name is not an identifier cannot be named in
/// `file.Name`: the ambiguity's fix offers only the other file and says
/// to rename it.
#[test]
fn an_ambiguous_names_fix_needs_identifier_file_names() {
    let (out, _) = compile_files(&[
        (
            "my-row.strand",
            "component Row { text \"m\" }\n".to_string(),
        ),
        ("b.strand", "component Row { text \"b\" }\n".to_string()),
        ("c.strand", "bar C { Row }\n".to_string()),
    ]);
    let d = out
        .diagnostics
        .iter()
        .find(|d| d.code == "check::ambiguous_name")
        .unwrap();
    let fixes: Vec<&str> = d
        .suggestions
        .iter()
        .map(|s| s.replacement.as_str())
        .collect();
    assert_eq!(fixes, ["b.Row"]);
    assert!(
        d.help
            .as_deref()
            .unwrap()
            .contains("rename `my-row.strand` to an identifier"),
        "{:?}",
        d.help
    );
    // (Review minor 8) Nor does the note offer `my-row.Row`.
    let notes = strand_compiler::check::names::notes(&out.program);
    assert_eq!(notes.len(), 2);
    for n in &notes {
        let help = n.help.as_deref().unwrap();
        assert!(
            !help.contains("my-row.Row") && help.contains("rename `my-row.strand`"),
            "{help}"
        );
    }
}

/// `check::same_name`: a note (never an error or a warning) at every
/// declaration of a name declared in more than one file, listing the
/// other files and saying how a use resolves; none for a unique name,
/// and none in a load's diagnostics.
#[test]
fn a_shared_name_is_a_note_at_each_declaration() {
    let (out, _) = same_named("");
    assert!(
        out.diagnostics.iter().all(|d| d.code != "check::same_name"),
        "a load never reports it"
    );
    let notes = strand_compiler::check::names::notes(&out.program);
    assert_eq!(notes.len(), 8, "{notes:?}");
    assert!(
        notes
            .iter()
            .all(|n| n.severity == strand_compiler::Severity::Note)
    );
    let a = map_id(&out, "a.strand");
    let row: Vec<_> = notes
        .iter()
        .filter(|n| n.message == "`Row` is also declared in `b.strand`")
        .collect();
    assert_eq!(row.len(), 1, "{notes:?}");
    assert_eq!(row[0].primary().unwrap().file, a);
    assert_eq!(
        row[0].help.as_deref(),
        Some(
            "not a problem: a file that declares `Row` uses its own; a file without one uses \
             the only declaration elsewhere, and where two or more other files declare it, a \
             bare `Row` is an error there: name the file it comes from: `a.Row` or `b.Row`"
        )
    );
    assert!(
        notes
            .iter()
            .any(|n| n.message == "`Row` is also declared in `a.strand`")
    );
    // A name in one file only: no note.
    let (out, _) = compile_files(&[("a.strand", "component Row { text \"a\" }\n".to_string())]);
    assert!(strand_compiler::check::names::notes(&out.program).is_empty());
}

/// Surfaces and services keep one name across the config, and no kind
/// may share a name within one file or with a surface or service.
#[test]
fn surfaces_and_services_stay_unique() {
    for (a, b) in [
        ("bar Top { text \"a\" }\n", "bar Top { text \"b\" }\n"),
        (
            "service s from file \"/tmp/a.json\" { n: int }\n",
            "service s from file \"/tmp/b.json\" { n: int }\n",
        ),
        ("component Top { text \"a\" }\n", "bar Top { text \"b\" }\n"),
        ("bar Top { text \"a\" }\n", "component Top { text \"b\" }\n"),
        (
            "action go() [\"true\"]\npermit exec\n",
            "action go() [\"true\"]\n",
        ),
    ] {
        let (out, _) = compile_files(&[("a.strand", a.to_string()), ("b.strand", b.to_string())]);
        let c: Vec<_> = out.diagnostics.iter().map(|d| d.code).collect();
        assert!(c.contains(&"check::redeclared"), "{a} / {b}: {c:?}");
    }
    let c = codes("component Row { text \"a\" }\ncomponent Row { text \"b\" }\n");
    assert_eq!(c, ["check::redeclared"]);
}

fn codes_of(out: &Compiled) -> Vec<&'static str> {
    out.diagnostics.iter().map(|d| d.code).collect()
}

/// (Review blocker 1) An ambiguous use is one error and types as an
/// error, inside an overloaded builtin's arguments too (each overload
/// tried checks it again), with no declaration standing in: no cascade
/// of field errors against the first file's.
#[test]
fn an_ambiguous_use_is_one_error_even_in_overloaded_calls() {
    let two = |c: &str| {
        compile_files(&[
            (
                "a.strand",
                "fn f() -> color { $accent }\ntype Item { a: int }\n".to_string(),
            ),
            (
                "b.strand",
                "fn f() -> color { $fg }\ntype Item { b: text }\n".to_string(),
            ),
            ("c.strand", c.to_string()),
        ])
    };
    for c in [
        "let p = radial(f(), $fg)\n",
        "let m = material(f())\n",
        "let i = Item(b: \"x\")\n",
        "let i = Item(a: 1)\nlet n = i.zzz\n",
    ] {
        let (out, map) = two(c);
        assert_eq!(
            codes_of(&out),
            ["check::ambiguous_name"],
            "{c}{}",
            render(&out.diagnostics, &map, Style::Plain)
        );
    }
    // Qualified, each call checks against its file's declaration.
    let (out, map) = two("let p = radial(a.f(), $fg)\nlet i = b.Item(b: \"x\")\n");
    assert!(
        out.diagnostics.is_empty(),
        "{}",
        render(&out.diagnostics, &map, Style::Plain)
    );
}

/// (Review major 3) The rules choose among the declarations a position
/// accepts, and fall through to the builtins when none does.
#[test]
fn the_rules_count_only_what_a_position_accepts() {
    // Two `component Workspace`s never hide the builtin type.
    let ws = "component Workspace { text \"w\" }\n";
    let (out, map) = compile_files(&[
        ("a.strand", ws.to_string()),
        ("b.strand", ws.to_string()),
        (
            "c.strand",
            "fn label(w: Workspace) -> text { w.name }\nbar C { a.Workspace }\n".to_string(),
        ),
    ]);
    assert!(
        out.diagnostics.is_empty(),
        "{}",
        render(&out.diagnostics, &map, Style::Plain)
    );
    // A `type Item` in one file and a `component Item` in another: a
    // type position reads the type, an element the component.
    let (out, map) = compile_files(&[
        ("a.strand", "type Item { n: int }\n".to_string()),
        ("b.strand", "component Item { text \"i\" }\n".to_string()),
        (
            "c.strand",
            "let x: Item? = null\nlet y = Item(n: 2)\nbar C { Item }\n".to_string(),
        ),
    ]);
    assert!(
        out.diagnostics.is_empty(),
        "{}",
        render(&out.diagnostics, &map, Style::Plain)
    );
    // In `b`, whose own `Item` is a component, a type is still `a`'s.
    let (out, map) = compile_files(&[
        ("a.strand", "type Item { n: int }\n".to_string()),
        (
            "b.strand",
            "component Item { text \"i\" }\nlet x: Item? = null\nbar B { Item }\n".to_string(),
        ),
    ]);
    assert!(
        out.diagnostics.is_empty(),
        "{}",
        render(&out.diagnostics, &map, Style::Plain)
    );
    // Two enums named like a builtin enum hide it; two components do
    // not.
    let (out, map) = compile_files(&[
        ("a.strand", "component Edge { text \"e\" }\n".to_string()),
        ("b.strand", "component Edge { text \"f\" }\n".to_string()),
        ("c.strand", "bar P { edge: Edge.top }\n".to_string()),
    ]);
    assert!(
        out.diagnostics.is_empty(),
        "{}",
        render(&out.diagnostics, &map, Style::Plain)
    );
}

/// (Review minor 10) A file named like a builtin type is never read as
/// `file.Name`: `Edge.top` stays the builtin enum's variant even when
/// `Edge.strand` declares a `fn top`.
#[test]
fn a_file_named_like_a_builtin_type_is_not_a_qualifier() {
    let (out, map) = compile_files(&[
        ("Edge.strand", "fn top() -> int { 1 }\n".to_string()),
        ("c.strand", "bar P { edge: Edge.top }\n".to_string()),
    ]);
    assert!(
        out.diagnostics.is_empty(),
        "{}",
        render(&out.diagnostics, &map, Style::Plain)
    );
}

/// (Review minor 11) A qualifier that is a name in scope, a service or
/// a value keeps the parser's "statement in a tree" error, never "no
/// file".
#[test]
fn a_qualifier_in_scope_is_a_statement_in_a_tree() {
    for c in [
        "state n = 1\nbar C { n.expire 3 }\n",
        "bar C { battery.percent 3 }\n",
        "component K(w: int) { w.x 2 }\n",
    ] {
        let (out, map) = compile_files(&[("c.strand", c.to_string())]);
        let d = out
            .diagnostics
            .iter()
            .find(|d| d.code == "syntax::expected")
            .unwrap_or_else(|| panic!("{c}{}", render(&out.diagnostics, &map, Style::Plain)));
        assert_eq!(
            d.message,
            "expected `;` or a line break after this element, found `.`"
        );
        assert_eq!(
            d.help.as_deref(),
            Some("statements go inside a handler: `on click { … }`")
        );
        assert!(
            out.diagnostics
                .iter()
                .all(|d| d.code != "check::unknown_name"),
            "{c}"
        );
    }
}

/// (Review major 4) `$Row.gap` and a set's `override Row.gap` of a
/// component more than one file declares follow rules 1 and 2 over the
/// `Row`s with `tokens { }`: a component reads its own; a theme reads
/// the only one with tokens; with two, a read must write `$a.Row.gap`,
/// and the error says so.
#[test]
fn shared_components_tokens_follow_the_rules() {
    let row = |gap: &str| {
        format!("component Row tokens {{ gap: {gap} }} {{\n  row {{ gap: $Row.gap }}\n}}\n")
    };
    // Each `Row` reads its own; a qualified read names one.
    let (out, map) = compile_files(&[
        ("a.strand", format!("{}bar A {{ Row }}\n", row("4px"))),
        ("b.strand", format!("{}bar B {{ Row }}\n", row("8px"))),
        (
            "c.strand",
            "tokens t { override a.Row.gap: 2px }\nuse tokens t\nbar C { row { gap: $b.Row.gap } }\n"
                .to_string(),
        ),
    ]);
    assert!(
        out.diagnostics.is_empty(),
        "{}",
        render(&out.diagnostics, &map, Style::Plain)
    );
    // A bare read, and a bare override, in a file without a `Row`: an
    // error naming both, with the qualified paths as fixes.
    let (out, _) = compile_files(&[
        ("a.strand", row("4px")),
        ("b.strand", row("8px")),
        (
            "c.strand",
            "bar C { row { gap: $Row.gap } }\ntokens t { override Row.gap: 2px }\n".to_string(),
        ),
    ]);
    let amb: Vec<_> = out
        .diagnostics
        .iter()
        .filter(|d| d.code == "check::ambiguous_name")
        .collect();
    assert_eq!(amb.len(), 2, "{:?}", out.diagnostics);
    assert_eq!(
        amb[0].message,
        "`$Row.gap` is a token of the component `Row`, which 2 files declare with tokens, \
         `a.strand` and `b.strand`, and not this one"
    );
    let fixes: Vec<Vec<&str>> = amb
        .iter()
        .map(|d| {
            d.suggestions
                .iter()
                .map(|s| s.replacement.as_str())
                .collect()
        })
        .collect();
    assert!(
        fixes.contains(&vec!["$a.Row.gap", "$b.Row.gap"]),
        "{fixes:?}"
    );
    assert!(fixes.contains(&vec!["a.Row.gap", "b.Row.gap"]), "{fixes:?}");
    assert_eq!(
        out.diagnostics.len(),
        2,
        "no cascade: {:?}",
        out.diagnostics
    );
    // Only one `Row` has tokens: a bare read elsewhere is that one's.
    let (out, map) = compile_files(&[
        ("a.strand", row("4px")),
        ("b.strand", "component Row { text \"b\" }\n".to_string()),
        (
            "theme.strand",
            "tokens t { override Row.gap: 2px }\nuse tokens t\nbar C { row { gap: $Row.gap } }\n"
                .to_string(),
        ),
    ]);
    assert!(
        out.diagnostics.is_empty(),
        "{}",
        render(&out.diagnostics, &map, Style::Plain)
    );
}

/// (Review major 5) Two files of one stem in two directories that both
/// declare a shared name: a load error asking to rename one, and no
/// `row.Row` fix anywhere, since it could not say which.
#[test]
fn one_stem_in_two_directories_cannot_share_a_name() {
    let (out, _) = compile_files(&[
        (
            "bar/row.strand",
            "component Row { text \"a\" }\n".to_string(),
        ),
        (
            "menu/row.strand",
            "component Row { text \"b\" }\n".to_string(),
        ),
        ("c.strand", "bar C { Row }\n".to_string()),
    ]);
    let d = out
        .diagnostics
        .iter()
        .find(|d| d.code == "check::file_name")
        .unwrap_or_else(|| panic!("{:?}", out.diagnostics));
    assert_eq!(d.message, "two files named `row.strand` both declare `Row`");
    assert_eq!(
        d.help.as_deref(),
        Some("rename one of the files: `row.Row` cannot say which one a use means")
    );
    let amb = out
        .diagnostics
        .iter()
        .find(|d| d.code == "check::ambiguous_name")
        .unwrap();
    assert!(amb.suggestions.is_empty(), "{:?}", amb.suggestions);
    assert!(
        amb.help
            .as_deref()
            .unwrap()
            .starts_with("rename `row.strand` to an identifier no other file has"),
        "{:?}",
        amb.help
    );
    let notes = strand_compiler::check::names::notes(&out.program);
    assert!(
        notes
            .iter()
            .all(|n| !n.help.as_deref().unwrap().contains("row.Row")),
        "{notes:?}"
    );
}

/// (Re-review major 1) Every fix the checker offers for an ambiguous
/// use compiles: over files named like a service (`tray`, `battery`), a
/// builtin type (`color`) and plain ones, each suggestion applied alone
/// removes its error and adds none. An element or a type always takes
/// `tray.Row`; an expression whose `stem.name` already means something
/// (`battery.percent`, `tray.items`: the services' fields) gets no fix
/// there, and the help asks to rename the file instead.
#[test]
fn every_offered_fix_compiles_whatever_the_files_are_named() {
    let decls = "component Row { text \"r\" }\ntype Item { n: int }\nenum Kind { dark, light }\n\
fn percent() -> int { 1 }\nfn items() -> int { 2 }\nfn fmt() -> int { 3 }\n";
    let user = "let a = percent()\nlet b = items()\nlet f = fmt()\nlet i: Item? = null\n\
let j = Item(n: 1)\nlet k: Kind = Kind.dark\nlet m = match k { Kind.dark => 1, _ => 2 }\n\
bar C { Row; text join(\"\", a, b, f, m, j.n) }\n";
    let stems = ["tray", "battery", "theme", "color", "dock"];
    for (x, s1) in stems.iter().enumerate() {
        for s2 in &stems[x + 1..] {
            let files = |c: &str| {
                vec![
                    (format!("{s1}.strand"), decls.to_string()),
                    (format!("{s2}.strand"), decls.to_string()),
                    ("use.strand".to_string(), c.to_string()),
                ]
            };
            let compile = |c: &str| {
                let owned = files(c);
                let borrowed: Vec<(&str, String)> =
                    owned.iter().map(|(n, t)| (n.as_str(), t.clone())).collect();
                compile_files(&borrowed)
            };
            let (out, map) = compile(user);
            let errors: Vec<_> = out.diagnostics.iter().filter(|d| d.is_error()).collect();
            assert!(
                errors.iter().all(|d| d.code == "check::ambiguous_name"),
                "{s1}/{s2}: {}",
                render(&out.diagnostics, &map, Style::Plain)
            );
            // `percent`, `items`, `fmt`, `Item` twice, `Kind` three
            // times, `Row`.
            assert_eq!(
                errors.len(),
                9,
                "{s1}/{s2}: {}",
                render(&out.diagnostics, &map, Style::Plain)
            );
            for d in &errors {
                let used = &user[d.primary_span().unwrap().start as usize
                    ..d.primary_span().unwrap().end as usize];
                for (stem, hidden) in [(*s1, false), (*s2, false)].map(|(s, _)| {
                    (
                        s,
                        matches!((s, used), ("battery", "percent") | ("tray", "items")),
                    )
                }) {
                    let form = format!("{stem}.{used}");
                    let offered = d.suggestions.iter().any(|f| f.replacement == form);
                    assert_eq!(offered, !hidden, "{s1}/{s2}: {form} in {:?}", d.suggestions);
                    if hidden {
                        let help = d.help.as_deref().unwrap();
                        assert!(help.contains(&format!("rename `{stem}.strand`")), "{help}");
                    }
                }
                for f in &d.suggestions {
                    let span = f.span;
                    let fixed = format!(
                        "{}{}{}",
                        &user[..span.start as usize],
                        f.replacement,
                        &user[span.end as usize..]
                    );
                    let (after, map) = compile(&fixed);
                    let left: Vec<_> = after.diagnostics.iter().filter(|d| d.is_error()).collect();
                    assert!(
                        left.len() == errors.len() - 1
                            && left.iter().all(|d| d.code == "check::ambiguous_name"),
                        "{s1}/{s2}: `{}` does not compile:\n{fixed}\n{}",
                        f.replacement,
                        render(&after.diagnostics, &map, Style::Plain)
                    );
                }
            }
            // The notes say the same.
            for n in strand_compiler::check::names::notes(&out.program) {
                let help = n.help.as_deref().unwrap();
                for bad in ["battery.percent`", "tray.items`"] {
                    if help.contains(&format!("`{bad}")) {
                        assert!(help.contains("already means something else"), "{help}");
                    }
                }
            }
        }
    }
}

/// (Third review) A file named `self.strand` never changes `self.x`,
/// nor one named like a node name `hover.x`: those are read before any
/// file (spec.md 14).
#[test]
fn a_file_named_self_is_not_a_qualifier() {
    let (out, map) = compile_files(&[
        (
            "self.strand",
            "fn width() -> bool { true }\nfn height() -> bool { true }\n".to_string(),
        ),
        (
            "c.strand",
            "bar Top {\n  height: 30\n  row {\n    opacity: 1\n    when self.width < 300 { opacity: 0.5 }\n    text \"x\"\n  }\n}\n"
                .to_string(),
        ),
    ]);
    assert!(
        out.diagnostics.iter().all(|d| !d.is_error()),
        "{}",
        render(&out.diagnostics, &map, Style::Plain)
    );
    assert!(
        out.program
            .refs
            .iter()
            .all(|r| !matches!(r.target, strand_compiler::hir::Target::File(_))),
        "`self` named the file"
    );
}

/// (M6, spec.md 6.3) `.box` drives what layout does not place: paint
/// props anywhere, the size of a `place: absolute` element it is not
/// inside, and anything on another surface (a popup sized from the bar
/// item it opens from, an island in it following the hovered item);
/// a conditional id (`when c { id: name }`) names the `for` copy whose
/// condition holds, read outside the `for`. None of it is an error.
#[test]
fn box_drives_paint_absolute_and_other_surfaces() {
    let src = r#"
state current = 1
state hovered = 0
bar Top {
  height: 30
  row {
    id: ws
    for n in [1, 2, 3] key n {
      box { size: 20; when n == current { id: cur }; text "{n}" }
    }
    box {
      place: absolute; height: 2; bg: $fg
      x: cur.box.x - ws.box.x ~ spring(300, 30)
      width: cur.box.width ~ spring(300, 30)
      opacity: cur.box.width > 0 ? 1 : 0
    }
  }
  row {
    id: pill
    text "vol" { id: vol; when hovered == 1 { id: it } }
    text "net" { id: net; when hovered == 2 { id: it } }
    popup {
      id: pop
      trigger: hover
      width: pill.box.width; height: 80
      col {
        place: absolute; height: 60; bg: $surface
        x: it.box.x - pop.box.x ~ spring(300, 30)
        width: it.box.width + 16 ~ spring(300, 30)
        text hovered == 1 ? "volume" : "network"
      }
    }
  }
}
"#;
    one(src);
}

/// (M6, spec.md 14) Before conditional ids, `when c { id: x }` loaded and
/// did nothing. Where binding the name would change a program that loads
/// that way, it keeps that meaning, with a warning: beside a plain `id:
/// x` (one error before this ruling's first cut), and where `x` already
/// names a `state`, a parameter or an enum variant (each would now fail
/// to check). A name that means nothing else there is a conditional id.
#[test]
fn a_conditional_id_never_changes_a_name_that_loads_today() {
    let cases = [
        // Beside a plain id: the plain one keeps the name.
        "state on = true\nbar Top {\n  height: 30\n  text \"a\" { when on { id: x } }\n  text \"b\" { when !on { id: x } }\n  text \"c\" { id: x }\n  text x.hover ? \"h\" : \"-\"\n}\n",
        // A state: `x > 0` still reads the number.
        "state x = 1\nbar Top {\n  height: 30\n  text \"a\" { when x > 0 { id: x } }\n  text x > 0 ? \"pos\" : \"neg\"\n}\n",
        // A parameter.
        "component C(x: int) {\n  text \"a\" { when x > 0 { id: x } }\n  text x > 0 ? \"pos\" : \"neg\"\n}\nbar Top {\n  height: 30\n  C { x: 1 }\n}\n",
        // An enum variant read by an `edge:`.
        "state on = true\nbar Top {\n  edge: top\n  height: 30\n  text \"a\" { when on { id: top } }\n}\n",
    ];
    for src in cases {
        let (out, map) = compile_files(&[("a.strand", src.to_string())]);
        let codes: Vec<&str> = out.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            !codes.is_empty() && codes.iter().all(|c| *c == "check::conditional_id_ignored"),
            "{src}\n{}",
            render(&out.diagnostics, &map, Style::Plain)
        );
        assert!(out.diagnostics.iter().all(|d| !d.is_error()));
    }
    // A free name is one conditional id however many `when`s give it.
    one(
        "state on = true\nbar Top {\n  height: 30\n  text \"a\" { when on { id: x } }\n  text \"b\" { when !on { id: x } }\n  text x.hover ? \"h\" : \"-\"\n}\n",
    );
}

/// The `check::box_layout_loop` messages of `src`, each with its path.
fn box_loops(src: &str) -> Vec<String> {
    let (out, map) = compile_files(&[("a.strand", src.to_string())]);
    let others: Vec<_> = out
        .diagnostics
        .iter()
        .filter(|d| d.code != "check::box_layout_loop")
        .collect();
    assert!(
        others.is_empty(),
        "{}",
        render(&out.diagnostics, &map, Style::Plain)
    );
    out.diagnostics
        .iter()
        .filter(|d| d.code == "check::box_layout_loop")
        .map(|d| d.message.clone())
        .collect()
}

/// (M6, the review of 66c2f03) A box may not drive a cycle of bindings
/// that lays it out again, however long: two absolutes each sized from
/// the other's box (each was "absolute, not inside it" alone), a bar item
/// and a popup's content each sized from the other's (each was "another
/// surface" alone), and the oscillating variant that goes through a
/// state a handler sets from a box and a ternary that flips each frame.
/// Each binding on the loop is refused, the loop's path in its message.
#[test]
fn a_box_loop_through_several_bindings_is_refused() {
    let two_absolutes = "bar Top {\n  height: 30\n  row {\n    box { id: a; place: absolute; height: 4; width: b.box.width + 1 }\n    box { id: b; place: absolute; height: 4; width: a.box.width + 1 }\n  }\n}\n";
    assert_eq!(
        box_loops(two_absolutes),
        [
            "`width` depends on `b.box`, which it moves again: `b.box` → `width` of `a` → `a.box` → `width` of `b` → `b.box`",
            "`width` depends on `a.box`, which it moves again: `a.box` → `width` of `b` → `b.box` → `width` of `a` → `a.box`",
        ]
    );
    let across = "bar Top {\n  height: 30\n  row {\n    text \"i\" { id: item; width: island.box.width }\n    popup {\n      width: 200; height: 50\n      box { id: island; height: 20; width: item.box.width + 10 }\n    }\n  }\n}\n";
    assert_eq!(
        box_loops(across),
        [
            // `item`'s width lays out the bar, the popup's box on it and so
            // what the popup holds.
            "`width` depends on `island.box`, which it moves again: `island.box` → `width` of `item` → `island.box`",
            "`width` depends on `item.box`, which it moves again: `item.box` → `width` of `island` → `island.box` → `width` of `item` → `item.box`",
        ]
    );
    let oscillating = "state wide = false\nbar Top {\n  height: 30\n  row {\n    box { id: a; place: absolute; height: 4; width: b.box.width / 2 }\n    box { id: b; place: absolute; height: 4; width: wide ? 100 : 20 }\n    on change a.box.width { wide = a.box.width < 30 }\n  }\n}\n";
    let got = box_loops(oscillating);
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(
        got.iter()
            .any(|m| m.ends_with("`b.box` → `width` of `a` → `a.box` → `width` of `b` → `b.box`")),
        "{got:?}"
    );
    // Each alone is fine: one absolute following another's box.
    one(
        "bar Top {\n  height: 30\n  row {\n    box { id: a; place: absolute; height: 4; width: 20 }\n    box { id: b; place: absolute; height: 4; width: a.box.width + 1 }\n  }\n}\n",
    );
}

/// (M6, the review of 66c2f03) A conditional id's conditions decide which
/// box its name reads: they taint `name.box` and every read of the name.
/// A `when { id: }` whose condition reads a box of its own surface is
/// refused like an `if` (the flip-flop: the name picked by where the
/// named box is, which moves it to the other copy and back each frame),
/// and a name picked by a popup's box cannot size that popup's content,
/// even read only as a value.
#[test]
fn a_conditional_ids_condition_is_followed() {
    let flip_flop = "bar Top {\n  height: 30\n  row {\n    for n in [1, 2] key n {\n      box { width: 20; height: 20; when n == (cur.box.x > 10 ? 1 : 2) { id: cur } }\n    }\n    box { place: absolute; height: 2; x: cur.box.x }\n  }\n}\n";
    let got = box_loops(flip_flop);
    assert_eq!(
        got,
        [
            "this `when`'s condition depends on `cur.box`, which it moves again: `cur.box` → the `when` naming `cur` → `cur.box`"
        ]
    );
    let by_value = "bar Top {\n  height: 30\n  row {\n    text \"a\" { id: a; when big.box.width > 50 { id: cur } }\n    text \"b\" { id: b; when big.box.width <= 50 { id: cur } }\n    popup {\n      width: 200; height: 50\n      box { id: big; height: 20; width: cur == a ? 100 : 30 }\n    }\n  }\n}\n";
    let got = box_loops(by_value);
    // Each condition renames on the bar, which holds the popup: what the
    // popup holds moves with it.
    assert_eq!(
        got,
        [
            "this `when`'s condition depends on `big.box`, which it moves again: `big.box` → the `when` naming `cur` → `big.box`",
            "this `when`'s condition depends on `big.box`, which it moves again: `big.box` → the `when` naming `cur` → `big.box`",
            "`width` depends on `big.box`, which it moves again: `big.box` → `width` of `big` → `big.box`"
        ]
    );
}

/// (M6, the review of 66c2f03) An element is absolute only if no `when`
/// or pose changes its `place`: one that a `when` may put back in flow
/// lays out its siblings and its row, so it may not follow its row's box.
#[test]
fn absolute_only_while_a_when_says_so_moves_its_surface() {
    let always = "state free = false\nbar Top {\n  height: 30\n  row {\n    id: r\n    box { place: absolute; height: 4; width: r.box.width / 2 }\n  }\n}\n";
    one(always);
    let sometimes = always.replace(
        "width: r.box.width / 2 }",
        "width: r.box.width / 2; when free { place: flow } }",
    );
    assert_eq!(
        box_loops(&sometimes),
        [
            "`width` depends on `r.box`, which it moves again: `r.box` → `width` of a `box` → `r.box`"
        ]
    );
}

/// (M6, the review of 66c2f03) Every channel that lays a surface out is
/// followed: a `set { $token }` its subtree reads, a component's tokens
/// (any element may read them), `pages`' `current` (it mounts a page by
/// value, like `match`), a `segmented`'s `options` (their count sizes
/// it), and another node's `.width` and `.height` (the same frame-late
/// fact as its `.box`, a loop through them alone a warning:
/// `a_loop_through_sizes_alone_is_a_warning`). An element's own size in
/// its own props stays a container query, and a surface's paint props
/// may follow a box.
#[test]
fn every_layout_channel_is_followed() {
    let set = "bar Top {\n  height: 30\n  row { id: r; set { $radius.lg: r.box.width * 1px }; box { radius: $radius.lg } }\n}\n";
    assert_eq!(box_loops(set).len(), 1, "set");
    let component = "state w = 0\ncomponent Card tokens { pad: w * 1px } { box { pad: $Card.pad } }\nbar Top {\n  height: 30\n  row { id: r; Card {} }\n  on change r.box.width { w = r.box.width }\n}\n";
    assert_eq!(box_loops(component).len(), 1, "component tokens");
    let pages = "enum Pg { w, n }\nbar Top {\n  height: 30\n  row {\n    id: r\n    pages current: r.box.width > 10 ? w : n { page w { text \"w\" }; page n { text \"n\" } }\n  }\n}\n";
    assert_eq!(box_loops(pages).len(), 1, "pages");
    let options = "bar Top {\n  height: 30\n  row {\n    id: r\n    segmented { options: r.box.width > 100 ? [\"a\", \"b\"] : [\"a\"] }\n  }\n}\n";
    assert_eq!(box_loops(options).len(), 1, "options");
    let size = "bar Top {\n  height: 30\n  row {\n    id: r\n    box { width: r.width / 2 }\n    box { height: r.height }\n  }\n}\n";
    assert_eq!(size_loops(size).len(), 2, "width and height");
    // Its own size: a container query.
    one("bar Top {\n  height: 30\n  box { when self.width < 300 { pad: 4 } }\n}\n");
    // A surface's paint props may follow a box on it; its size may not.
    one(
        "bar Top {\n  height: 30\n  opacity: r.box.width > 100 ? 1 : 0.5\n  row { id: r; text \"a\" }\n}\n",
    );
    assert_eq!(
        box_loops("bar Top {\n  height: r.box.height\n  row { id: r; text \"a\" }\n}\n").len(),
        1
    );
}

/// The `check::size_loop` warnings of `src`, which must have no other
/// diagnostic.
fn size_loops(src: &str) -> Vec<String> {
    let (out, map) = compile_files(&[("a.strand", src.to_string())]);
    let others: Vec<_> = out
        .diagnostics
        .iter()
        .filter(|d| d.code != "check::size_loop")
        .collect();
    assert!(
        others.is_empty(),
        "{}",
        render(&out.diagnostics, &map, Style::Plain)
    );
    assert_eq!(out.errors(), 0);
    out.diagnostics.iter().map(|d| d.message.clone()).collect()
}

/// (M6, spec.md 14) Another node's `.width` and `.height` loaded on
/// `main` before `.box` and still load with the same meaning: a loop of
/// bindings through them alone is a `check::size_loop` warning naming
/// its path, not an error. `instantiate.rs::only_measured_nodes_are_watched`'s
/// bar loads with that warning; a node passed as a value is not followed
/// by its size. A loop with a `.box` read on it stays an error, wherever
/// the sizes are.
#[test]
fn a_loop_through_sizes_alone_is_a_warning() {
    let measured = "bar Top {\n  height: 30\n  row {\n    when self.width < 300 { opacity: 0.5 }\n    box { id: b }\n    text \"w\"\n  }\n  text b.width > 10 ? \"wide\" : \"narrow\"\n}\n";
    let got = size_loops(measured);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        got[0].starts_with("`text` depends on `b`'s size, which it changes again: `b`'s size → "),
        "{got:?}"
    );
    assert!(got[0].ends_with(" → `b`'s size"), "{got:?}");
    let by_value = "component Half(n: Node) { box { width: n.width / 2 } }\nbar Top {\n  height: 30\n  row { id: r; Half r }\n}\n";
    one(by_value);
    // `.box` on the loop, a size after it: both bindings refused.
    let mixed = "bar Top {\n  height: 30\n  row {\n    box { id: a; place: absolute; height: 4; width: b.box.width + 1 }\n    box { id: b; place: absolute; height: 4; width: a.width + 1 }\n  }\n}\n";
    assert_eq!(
        box_loops(mixed),
        [
            "`width` depends on `b.box`, which it moves again: `b.box` → `width` of `a` → `a`'s size → `width` of `b` → `b.box`",
            "`width` depends on `a`'s size, which it moves again: `a`'s size → `width` of `b` → `b.box` → `width` of `a` → `a`'s size",
        ]
    );
}

/// (M6, the third review of the box graph) Inherited values reach into
/// popups: a `set`, `font` or `weight` on an element, or a surface's own
/// props, lays out every surface nested in what it sets, so a binding
/// that moves a popup's box moves what the popup holds too, through
/// popups inside popups. Each program here sizes a popup's content from
/// a box inside it by one of those routes, and each is refused.
#[test]
fn inherited_values_reach_nested_popups() {
    let popup =
        |inner: &str| format!("box {{ size: 20; popup {{ width: 200; height: 50; {inner} }} }}");
    let chip = "row { pad: $space.2; text \"c\" { id: chip } }";
    let set = "set { $space.2: chip.box.width > 60 ? 4px : 48px }";
    let cases = [
        // The review's program: a token set on the row.
        (
            "set",
            format!(
                "bar Top {{\n  height: 30\n  row {{\n    {set}\n    {}\n  }}\n}}\n",
                popup(chip)
            ),
        ),
        // The popup's own size through an ancestor's token.
        (
            "popup width",
            format!(
                "bar Top {{\n  height: 30\n  row {{\n    {set}\n    box {{ size: 20; popup {{ width: $space.2 * 4; height: 50; text \"c\" {{ id: chip }} }} }}\n  }}\n}}\n"
            ),
        ),
        // An `if` mounting a `set`.
        (
            "if",
            format!(
                "bar Top {{\n  height: 30\n  row {{\n    if chip.box.width > 60 {{ set {{ $space.2: 4px }} }}\n    {}\n  }}\n}}\n",
                popup(chip)
            ),
        ),
        // An inherited prop on an element, and on the surface itself.
        (
            "weight",
            format!(
                "bar Top {{\n  height: 30\n  row {{\n    weight: chip.box.width > 60 ? 700 : 400\n    {}\n  }}\n}}\n",
                popup("text \"c\" { id: chip }")
            ),
        ),
        (
            "surface",
            format!(
                "bar Top {{\n  height: 30\n  weight: chip.box.width > 60 ? 700 : 400\n  row {{ {} }}\n}}\n",
                popup("text \"c\" { id: chip }")
            ),
        ),
        // A popup inside a popup, set from the outer one's content.
        (
            "nested",
            format!(
                "bar Top {{\n  height: 30\n  row {{\n    {}\n  }}\n}}\n",
                popup(&format!("{set}; {}", popup(chip)))
            ),
        ),
    ];
    for (what, src) in cases {
        let got = box_loops(&src);
        assert!(
            got.iter().any(|m| m.ends_with("`chip.box`")),
            "{what}: {got:?}\n{src}"
        );
    }
}

/// (M6, the third review) The dependences settle however long a chain
/// is: 100 `let`s, one a round, then a width on the last.
#[test]
fn a_long_let_chain_is_followed_to_its_end() {
    let mut src =
        String::from("bar Top {\n  height: 30\n  row {\n    id: r\n    let v0 = r.box.width\n");
    for i in 1..100 {
        src.push_str(&format!("    let v{i} = v{} + 1\n", i - 1));
    }
    src.push_str("    box { width: v99 }\n  }\n}\n");
    let got = box_loops(&src);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].starts_with("`width` depends on `r.box`"), "{got:?}");
}

/// (M6, the third review) A service field a handler writes from a box
/// carries the box to every read of it: the backlight level set from a
/// row's width sizes a box in that row.
#[test]
fn a_service_field_written_from_a_box_is_followed() {
    let src = "bar Top {\n  height: 30\n  row {\n    id: r\n    on change r.box.width { brightness.level = r.box.width / 1000 }\n    box { width: brightness.level * 100 }\n  }\n}\n";
    let got = box_loops(src);
    assert_eq!(
        got,
        [
            "`width` depends on `r.box`, which it moves again: `r.box` → `width` of a `box` → `r.box`"
        ]
    );
    // Read for paint only, it loads.
    one(&src.replace(
        "box { width: brightness.level * 100 }",
        "box { opacity: brightness.level }",
    ));
}

/// (M6, the third review) A bar's thickness is its exclusive zone, which
/// places and sizes the other layer surfaces: a top bar sized from a box
/// on a side bar (through a state) moves that box again. A bar's paint
/// may follow it, and so may a panel's size, which reserves nothing.
#[test]
fn a_bars_exclusive_zone_moves_the_other_layer_surfaces() {
    let two_bars = "state h = 0\nbar Top {\n  height: h + 10\n  text \"t\"\n}\nbar Side {\n  edge: left; width: 40\n  col { box { id: s; height: 20 }; on change s.box.y { h = s.box.y } }\n}\n";
    let got = box_loops(two_bars);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        got[0].starts_with(
            "`height` depends on `s.box`, which it moves again: `s.box` → `height` of a `bar`"
        ),
        "{got:?}"
    );
    // Its content sizes a bar with no thickness written: the same.
    let auto = "state h = 0\nbar Top {\n  text \"t\" { height: h + 10 }\n}\nbar Side {\n  edge: left; width: 40\n  col { box { id: s; height: 20 }; on change s.box.y { h = s.box.y } }\n}\n";
    assert_eq!(box_loops(auto).len(), 1);
    one(&two_bars.replace("height: h + 10", "height: 30; opacity: h > 5 ? 1 : 0.5"));
    one(&two_bars.replace(
        "bar Top {\n  height: h + 10",
        "panel Top {\n  height: h + 10",
    ));
}

/// (M6) spec.md 6.3's travelling island, as written there, checks with
/// no diagnostic.
#[test]
fn the_specs_travelling_island_checks() {
    let spec =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/spec.md"))
            .unwrap();
    let at = spec.find("- **A travelling island**").expect("the island");
    let rest = &spec[at..];
    let start = rest.find("```strand\n").unwrap() + "```strand\n".len();
    let end = start + rest[start..].find("  ```").unwrap();
    let src: String = rest[start..end]
        .lines()
        .map(|l| l.strip_prefix("  ").unwrap_or(l))
        .collect::<Vec<_>>()
        .join("\n");
    one(&src);
}

/// (M6, decisions.md m6-integration-w2, the final pass) Where the
/// surface stream's ids meet the language stream's file names: a plain
/// `id: a` in a file beside `a.strand` is read by `a.box` as the id, as
/// a name in scope always was; a `when`'s `id: a` names nothing, since
/// `a` is already a file whose `a.Row` still reads, and nor does one
/// named like a component two files share; and a box's `float` written
/// into a state started from a whole number loads.
#[test]
fn file_stems_and_shared_names_meet_ids_and_boxes() {
    let row = |t: &str| format!("component Row {{ text \"{t}\" }}\n");
    let plain = "bar One {\n  height: 30\n  row { id: a; text \"x\" }\n  \
        box { place: absolute; height: 2; width: a.box.width }\n}\n";
    let (out, map) = compile_files(&[
        ("a.strand", row("a")),
        ("b.strand", row("b")),
        ("m.strand", plain.to_string()),
    ]);
    assert!(
        out.diagnostics.is_empty(),
        "{}",
        render(&out.diagnostics, &map, Style::Plain)
    );
    let conditional = "state on = true\nbar Two {\n  height: 30\n  \
        text \"p\" { when on { id: a } }\n  text \"q\" { when !on { id: Row } }\n  a.Row\n}\n";
    let (out, map) = compile_files(&[
        ("a.strand", row("a")),
        ("b.strand", row("b")),
        ("m.strand", conditional.to_string()),
    ]);
    let shown = render(&out.diagnostics, &map, Style::Plain);
    assert_eq!(
        codes_of(&out),
        [
            "check::conditional_id_ignored",
            "check::conditional_id_ignored"
        ],
        "{shown}"
    );
    assert!(shown.contains("a file of this config"), "{shown}");
    assert!(shown.contains("a declaration of this config"), "{shown}");
    one("state w = 0\nbar Three {\n  height: 30\n  \
         text \"t\" { id: t; opacity: w > 10 ? 1 : 0.5\n    \
         on change t.box.width { w = t.box.width } }\n}\n");
}

/// (Bug 8, owner testing 2026-10-14) An `image`, `svg` or `lottie` whose
/// literal file is not there is a `check::missing_file` warning with a
/// help line, its relative path under the config directory (as render
/// reads it, not the process's directory); a file that is there, a
/// subdirectory's included, an icon name and a computed source are not.
#[test]
fn a_missing_literal_file_is_a_warning() {
    let dir = std::env::temp_dir().join(format!("strand-assets-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("icons")).unwrap();
    std::fs::write(dir.join("dot.svg"), "<svg/>").unwrap();
    std::fs::write(dir.join("icons/wifi.svg"), "<svg/>").unwrap();
    let warnings = |src: &str| {
        let out = one(src);
        strand_compiler::check::assets::check(&out.program, Some(&dir))
    };
    let quiet = "state name = \"wifi\"\nbar B {\n  height: 30\n  \
        svg \"dot.svg\" { size: 24 }\n  image \"dot.svg\"\n  image \"./dot.svg\"\n  \
        svg \"icons/wifi.svg\"\n  image \"firefox\"\n  image \"network-wireless-symbolic\"\n  \
        image \"org.gnome.Nautilus\"\n  icon \"missing.svg\"\n  \
        svg \"icons/\" + name + \".svg\"\n  image name\n}\n";
    let w = warnings(quiet);
    assert!(w.is_empty(), "{w:?}");

    let loud = "bar B {\n  height: 30\n  svg \"gone.svg\"\n  image \"icons/gone.png\"\n  \
        lottie \"gone.json\"\n  image \"/nonexistent/strand/x.png\"\n}\n";
    let w = warnings(loud);
    assert_eq!(
        w.iter().map(|d| d.code).collect::<Vec<_>>(),
        ["check::missing_file"; 4],
        "{w:?}"
    );
    assert!(w.iter().all(|d| !d.is_error()));
    let help = w[0].help.as_deref().unwrap();
    assert!(help.contains("under the config directory"), "{help}");
    assert!(
        help.contains(&dir.join("gone.svg").display().to_string()),
        "{help}"
    );
    assert!(w[1].help.as_deref().unwrap().contains("icon name"));
    // Without a config directory a relative path is not checked.
    assert_eq!(
        strand_compiler::check::assets::check(&one(loud).program, None).len(),
        1
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// (M6, the owner's ruling of 2026-10-14) An icon name is looked up in
/// the config's `icons/` folder at run time, then the theme: the check
/// never looks one up, so a name only the folder has (or nothing has)
/// is not warned about, through `icon` or `image`, while a path into the
/// folder is a file like any other.
#[test]
fn icon_names_of_the_config_icons_folder_are_not_warned() {
    let dir = std::env::temp_dir().join(format!("strand-cfgicons-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("icons")).unwrap();
    std::fs::write(dir.join("icons/strand-own.svg"), "<svg/>").unwrap();
    let src = "bar B {\n  height: 30\n  icon \"strand-own\"\n  icon \"strand-own-symbolic\"\n  \
        image \"strand-own\"\n  icon \"strand-nowhere\"\n  image \"icons/strand-own.svg\"\n}\n";
    let out = one(src);
    let w = strand_compiler::check::assets::check(&out.program, Some(&dir));
    assert!(w.is_empty(), "{w:?}");
    let out = one("bar B {\n  height: 30\n  image \"icons/strand-gone.svg\"\n}\n");
    let w = strand_compiler::check::assets::check(&out.program, Some(&dir));
    assert_eq!(w.len(), 1, "{w:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
