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
        let rendered = render(&out.diagnostics, &map, Style::Plain);
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

fn def_ty(p: &hir::Program, name: &str) -> String {
    let d = p
        .defs
        .iter()
        .find(|d| d.name == name)
        .unwrap_or_else(|| panic!("no def {name}"));
    show(p, &d.ty)
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
    let out = one("component A(x) { text \"${x}\"; B x }\n\
                   component B(y) { A 0.5 }\n\
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
