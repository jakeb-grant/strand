//! "Extract component" and live reload (design.md, "Identity and state"): the extracted subtree lives under a new component, so
//! its state resets on the reload that commits the edit (the action's
//! title says so), and nothing outside it does.
//!
//! The edit is the language server's own (`strand_dev::refactor`), and the
//! reload is the runtime's (`Instance::reload`, as `strand run` commits a
//! build), on the schema host's mock services.

use std::rc::Rc;
use std::sync::Arc;

use strand_compiler::instantiate::{Instance, SceneMirror, Storage};
use strand_compiler::reconcile::Build;
use strand_compiler::syntax::Span;
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_compiler::{SourceMap, schema::Schema};
use strand_core::Runtime;
use strand_dev::refactor::{Extract, extract};
use strand_dev::workspace::{Analysis, ConfigKey, Input};

/// Two counters with state, one inside the element extracted (`col`),
/// one beside it, and a top-level total both write.
const BAR: &str = "\
state total = 0
bar Top {
  row {
    Counter { label: \"out\" }
    col {
      text \"inner\"
      Counter { label: \"in\" }
    }
  }
}
component Counter(label: text) {
  state n = 0
  text join(\" \", label, n) { on click { n += 1; total += 1 } }
}
";

fn build(prev: Option<&Build>, text: &str) -> Build {
    let mut map = SourceMap::new();
    map.add("bar.strand", text.to_string());
    Build::compile(prev, map).unwrap_or_else(|d| panic!("{d:#?}"))
}

/// The language server's "extract component" for the element named at
/// `needle`, applied.
fn extracted(text: &str, needle: &str) -> String {
    let uri = "file:///config/bar.strand".to_string();
    let an = Analysis::new(
        ConfigKey::Single(uri.clone()),
        vec![Input::new(uri.clone(), "bar.strand".into(), text.into())],
        Arc::new(Schema::builtin().clone()),
    );
    let file = an.file(&uri).unwrap();
    let at = text.find(needle).unwrap() as u32;
    let Some(Extract::Edit { title, mut edits }) = extract(&an, file, Span::new(at, at)) else {
        panic!("no extract at {needle:?}");
    };
    assert!(
        title.ends_with("(state inside it resets on reload)"),
        "{title}"
    );
    edits.sort_by_key(|(s, _)| std::cmp::Reverse(s.start));
    let mut out = text.to_string();
    for (s, t) in edits {
        out.replace_range(s.start as usize..s.end as usize, &t);
    }
    out
}

#[test]
fn an_extracted_component_resets_only_its_subtree() {
    let rt = Runtime::new();
    let first = build(None, BAR);
    let host = Rc::new(SchemaHost::mock(&rt, &first.program.types));
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
    let mut inst = Instance::from_build(&rt, &first, host.clone(), Storage::none());
    let mut scene = SceneMirror::new();
    let flush = |inst: &Instance, scene: &mut SceneMirror| {
        let u = inst.flush();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
        scene.apply(&u.diff).unwrap();
    };
    flush(&inst, &mut scene);

    // Both counters away from their defaults: `out` at 2, `in` at 1.
    for label in ["out 0", "out 1", "in 0"] {
        let node = scene.find_text(label).unwrap_or_else(|| panic!("{label}"));
        inst.event(node, "click", Vec::new());
        flush(&inst, &mut scene);
    }
    assert!(scene.find_text("out 2").is_some());
    assert!(scene.find_text("in 1").is_some());
    assert_eq!(inst.value_of("bar", "total").unwrap(), Value::int(3));
    let out = scene.find_text("out 2").unwrap();

    let text = extracted(BAR, "col");
    assert!(text.contains("    NewComponent\n"), "{text}");
    assert!(text.contains("component NewComponent {\n  col {"), "{text}");
    let second = build(Some(&first), &text);
    let report = inst.reload(&second);
    flush(&inst, &mut scene);

    // Inside the extracted subtree: back to the default.
    assert!(scene.find_text("in 0").is_some(), "{}", scene.render());
    assert!(scene.find_text("in 1").is_none(), "{}", scene.render());
    // Outside it: the same node, its state and the top-level total kept.
    assert_eq!(scene.find_text("out 2"), Some(out), "{}", scene.render());
    assert_eq!(inst.value_of("bar", "total").unwrap(), Value::int(3));
    assert!(report.kept.contains(&"bar.total".to_string()), "{report:?}");
}
