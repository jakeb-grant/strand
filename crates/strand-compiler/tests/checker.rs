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
/// each lies in its diagnostic's file on whole characters and that a
/// "did you mean `x`?" help comes with `x` as its one fix (the LSP reads
/// the fixes, never the help).
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
            assert_eq!(f.file, d.file(), "{d:?}");
            let file = map.get(f.file).unwrap();
            let text = file.text.get(f.span.start as usize..f.span.end as usize);
            let text = text.unwrap_or_else(|| panic!("fix span off the text: {d:?}"));
            assert!(!text.is_empty() && !text.contains('\n'), "{d:?}");
            out.push_str(&format!(
                "fix [{}] {}: `{text}` -> `{}`\n",
                d.code, file.name, f.replacement
            ));
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

/// `from dbus` fields against the object's introspection: a missing
/// property (with a did-you-mean), a type that does not convert, `rw` on
/// a read-only property, and a bus that cannot be reached (a warning, the
/// config still loads).
#[test]
fn dbus_services_are_checked_against_introspection() {
    use strand_compiler::check::dbus::{BusProperty, Introspect, check};
    struct Fake(Result<Vec<BusProperty>, String>);
    impl Introspect for Fake {
        fn properties(
            &self,
            system: bool,
            name: &str,
            path: &str,
        ) -> Result<Vec<BusProperty>, String> {
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
    let ppd = Fake(Ok(vec![
        prop("ActiveProfile", "s", true),
        prop("PerformanceDegraded", "s", false),
        prop("Profiles", "aa{sv}", false),
    ]));
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
    let down = Fake(Err("cannot reach the bus: no such file".into()));
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
