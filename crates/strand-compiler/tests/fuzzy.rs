//! `xs.fuzzy(query, x => x.name)` (decisions.md keyboard, piece 3): any
//! list, each item with its score and ranges, best first; reactive and
//! memoised like `filter`, on a program mounted on the mock services.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use strand_compiler::instantiate::{Instance, SceneMirror, Storage};
use strand_compiler::reconcile::Build;
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_compiler::{SourceMap, lower};
use strand_core::Runtime;
use strand_scene::{NodeKind, Prop, PropValue};

struct Shell {
    rt: Runtime,
    host: Rc<SchemaHost>,
    inst: Instance,
    scene: SceneMirror,
}

impl Shell {
    fn flush(&mut self) {
        let u = self.inst.flush();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
        self.scene.apply(&u.diff).unwrap();
    }

    fn set(&mut self, path: &str, v: Value) {
        self.inst.set(&format!("f.{path}"), v).unwrap();
        self.flush();
    }

    /// The texts drawn, in tree order.
    fn texts(&self) -> Vec<String> {
        self.scene
            .of_kind(NodeKind::Text)
            .into_iter()
            .filter_map(|n| match self.scene.prop(n, Prop::Text) {
                Some(PropValue::Text(t)) => Some(t.to_string()),
                _ => None,
            })
            .collect()
    }

    /// The exported list `name`'s items as text.
    fn list(&self, name: &str) -> Vec<String> {
        let v = self.inst.get(&format!("f.{name}")).unwrap();
        v.as_list()
            .unwrap()
            .iter()
            .map(|t| t.as_text().unwrap().to_string())
            .collect()
    }

    fn number(&self, name: &str) -> f64 {
        self.inst
            .get(&format!("f.{name}"))
            .unwrap()
            .as_f64()
            .unwrap()
    }

    /// The selector and other lambda calls so far.
    fn calls(&self) -> u64 {
        self.inst.vm().calls()
    }
}

fn screens(rt: &Runtime, host: &SchemaHost) {
    let screen = host.record("Screen", &[("name", Value::text("DP-1"))]);
    host.set(rt, "screens.all", Value::list(vec![screen]))
        .unwrap();
}

fn boot(src: &str) -> Shell {
    boot_with(src, |_, _| {})
}

fn boot_with(src: &str, setup: impl FnOnce(&Runtime, &SchemaHost)) -> Shell {
    let mut map = SourceMap::new();
    map.add("f.strand", src.to_string());
    let compiled = strand_compiler::compile(&map);
    assert_eq!(compiled.errors(), 0, "{:#?}", compiled.diagnostics);
    let program = Arc::new(lower::lower(
        &compiled.program,
        strand_compiler::schema::Schema::builtin(),
    ));
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &program.types));
    screens(&rt, &host);
    setup(&rt, &host);
    let inst = Instance::new(&rt, program, host.clone(), Storage::none());
    let mut shell = Shell {
        rt,
        host,
        inst,
        scene: SceneMirror::new(),
    };
    shell.flush();
    shell
}

const ITEMS: &str = "type Item { id: int; name: text }\n";

fn ranges_of(m: &Value, shell: &Shell) -> Vec<(f64, f64)> {
    let types = &shell.inst.vm().prog.types;
    m.field(types, "ranges")
        .unwrap()
        .as_list()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r.field(types, "start").unwrap().as_f64().unwrap(),
                r.field(types, "end").unwrap().as_f64().unwrap(),
            )
        })
        .collect()
}

/// Best first; equal scores keep the list's order, whichever comes first,
/// and what doesn't match is left out.
#[test]
fn the_best_match_comes_first_and_ties_keep_the_list_order() {
    let src = format!(
        "{ITEMS}export state q = \"fi\"\n\
         state items: [Item] = [\n  \
           Item(id: 1, name: \"xfi\"),\n  \
           Item(id: 2, name: \"zebra\"),\n  \
           Item(id: 3, name: \"fi\"),\n  \
           Item(id: 4, name: \"f-----i\"),\n  \
           Item(id: 5, name: \"fi\"),\n  \
           Item(id: 6, name: \"xfi\"),\n\
         ]\n\
         export let found = items.fuzzy(q, x => x.name)\n\
         export let ids = found.map(m => join(\"\", m.item.id))\n\
         export let scores = found.map(m => m.score)\n\
         bar B {{ text \"x\" }}\n"
    );
    let shell = boot(&src);
    let ids = shell.list("ids");
    // Whole words first, then the scattered one (`-` is a word's edge), then
    // those inside a word; equals by the list's order, and no `zebra`.
    assert_eq!(ids, ["3", "5", "4", "1", "6"]);
    let scores = shell.inst.get("f.scores").unwrap();
    let scores: Vec<f64> = scores
        .as_list()
        .unwrap()
        .iter()
        .map(|s| s.as_f64().unwrap())
        .collect();
    assert!(scores.is_sorted_by(|a, b| a >= b), "{scores:?}");
    assert_eq!(scores[0], scores[1]);
    assert_eq!(scores[3], scores[4]);
    assert!(scores[0] > scores[2] && scores[2] > scores[3], "{scores:?}");
}

/// An empty query, or a blank one as `apps.search` trims it: every item in
/// list order, with score 0 and no ranges.
#[test]
fn an_empty_query_gives_every_item_in_order() {
    let src = "export state q = \"\"\n\
               state items = [\"pear\", \"apple\", \"fig\"]\n\
               export let found = items.fuzzy(q, x => x)\n\
               export let names = found.map(m => m.item)\n\
               export let total = found.sum(m => m.score)\n\
               export let marked = found.count(m => m.ranges.len > 0)\n\
               bar B { text \"x\" }\n";
    let mut shell = boot(src);
    for q in ["", "   "] {
        shell.set("q", Value::text(q));
        assert_eq!(shell.list("names"), ["pear", "apple", "fig"], "{q:?}");
        assert_eq!(shell.number("total"), 0.0);
        assert_eq!(shell.number("marked"), 0.0);
    }
}

/// `apps.search`'s words: each must match.
#[test]
fn every_word_of_the_query_must_match() {
    let src = "state items = [\"Firefox\", \"Files\", \"Foot\"]\n\
               export let a = items.fuzzy(\"fi ox\", x => x).map(m => m.item)\n\
               export let b = items.fuzzy(\"f oo\", x => x).map(m => m.item)\n\
               bar B { text \"x\" }\n";
    let shell = boot(src);
    assert_eq!(shell.list("a"), ["Firefox"]);
    assert_eq!(shell.list("b"), ["Foot"]);
}

/// Ranges count characters, what `marks:` paints by, not bytes or grapheme
/// clusters; the pixels are `fuzzy_ranges_paint_the_matched_letters` in
/// tests/render.rs.
#[test]
fn ranges_count_characters_on_non_ascii_text() {
    let src = "state items = [\"Écrire\", \"Cafe\u{301} Files\", \"日本語テキスト\"]\n\
               export let cr = items.fuzzy(\"cr\", x => x)\n\
               export let fil = items.fuzzy(\"fil\", x => x)\n\
               export let cafe = items.fuzzy(\"cafe\", x => x)\n\
               export let jp = items.fuzzy(\"テキ\", x => x)\n\
               bar B { text \"x\" }\n";
    let shell = boot(src);
    let one = |name: &str| -> Vec<(f64, f64)> {
        let v = shell.inst.get(&format!("f.{name}")).unwrap();
        let l = v.as_list().unwrap();
        assert_eq!(l.len(), 1, "{name}");
        ranges_of(&l[0], &shell)
    };
    assert_eq!(one("cr"), [(1.0, 3.0)], "É is two bytes, one character");
    assert_eq!(one("fil"), [(6.0, 9.0)], "the mark is a character");
    assert_eq!(one("cafe"), [(0.0, 5.0)], "and goes with its letter");
    assert_eq!(one("jp"), [(3.0, 5.0)], "each three bytes, one character");
}

/// A `for` over a keyed `state`'s chain: core's view. A new query runs the
/// selector once per item; one changed item runs it once, and the rows keep
/// their nodes.
#[test]
fn a_keyed_chain_reruns_on_the_query_and_item_by_item() {
    let src = format!(
        "{ITEMS}export state q = \"\"\n\
         export state items: [Item] key id = [\n  \
           Item(id: 1, name: \"apple\"),\n  \
           Item(id: 2, name: \"grape\"),\n  \
           Item(id: 3, name: \"pear\"),\n\
         ]\n\
         export state renamed = 0\n\
         bar B {{\n  \
           box {{ on click {{ items.update(2, x => Item(id: x.id, name: \"zzz\")) }} }}\n  \
           box {{ on click {{ items.push(Item(id: 4, name: \"pineapple\")) }} }}\n  \
           box {{ on click {{ items.remove_key(1) }} }}\n  \
           for m in items.fuzzy(q, x => x.name) {{\n    \
             text m.item.name\n  \
           }}\n\
         }}\n"
    );
    let mut shell = boot(&src);
    assert_eq!(shell.texts(), ["apple", "grape", "pear"]);
    let apple = shell.scene.find_text("apple").unwrap();
    let grape = shell.scene.find_text("grape").unwrap();

    let before = shell.calls();
    shell.set("q", Value::text("ap"));
    assert_eq!(shell.calls() - before, 3, "a new query: one call per item");
    assert_eq!(shell.texts(), ["apple", "grape"]);
    assert_eq!(shell.scene.find_text("apple"), Some(apple), "kept by key");
    assert_eq!(shell.scene.find_text("grape"), Some(grape), "kept by key");

    let boxes = shell.scene.of_kind(NodeKind::Box);
    // One item changed: one call, and it goes.
    let before = shell.calls();
    shell.inst.event(boxes[0], "click", Vec::new());
    shell.flush();
    let update = 1; // `update`'s own lambda
    assert_eq!(shell.calls() - before, update + 1, "one selector call");
    assert_eq!(shell.texts(), ["apple"]);

    // One pushed: one call, and it joins by score.
    let before = shell.calls();
    shell.inst.event(boxes[1], "click", Vec::new());
    shell.flush();
    assert_eq!(shell.calls() - before, 1, "one selector call");
    assert_eq!(shell.texts(), ["apple", "pineapple"]);

    // One removed: none.
    let before = shell.calls();
    shell.inst.event(boxes[2], "click", Vec::new());
    shell.flush();
    assert_eq!(shell.calls() - before, 0);
    assert_eq!(shell.texts(), ["pineapple"]);

    // No query: all of them, in the list's order.
    shell.set("q", Value::text(""));
    assert_eq!(shell.texts(), ["zzz", "pear", "pineapple"]);
}

/// An item changing so that its score changes moves its row, and an item
/// tying another sits by the list's order, whichever arrived first.
#[test]
fn a_keyed_chain_keeps_ties_in_list_order_as_items_change() {
    let src = format!(
        "{ITEMS}export state q = \"fi\"\n\
         export state items: [Item] key id = [\n  \
           Item(id: 1, name: \"xfi\"),\n  \
           Item(id: 2, name: \"fi\"),\n  \
           Item(id: 3, name: \"xfi\"),\n\
         ]\n\
         bar B {{\n  \
           box {{ on click {{ items.insert(0, Item(id: 4, name: \"xfi\")) }} }}\n  \
           box {{ on click {{ items.update(2, x => Item(id: 2, name: \"nothing\")) }} }}\n  \
           for m in items.fuzzy(q, x => x.name) {{\n    \
             text join(\"\", m.item.id)\n  \
           }}\n\
         }}\n"
    );
    let mut shell = boot(&src);
    assert_eq!(shell.texts(), ["2", "1", "3"]);
    let boxes = shell.scene.of_kind(NodeKind::Box);
    shell.inst.event(boxes[0], "click", Vec::new());
    shell.flush();
    assert_eq!(
        shell.texts(),
        ["2", "4", "1", "3"],
        "4 is first in the list"
    );
    shell.inst.event(boxes[1], "click", Vec::new());
    shell.flush();
    assert_eq!(shell.texts(), ["4", "1", "3"]);
}

/// A plain list: a list method like any other, so a reactive one.
#[test]
fn a_plain_list_reruns_on_the_query_and_the_items() {
    let src = "export state q = \"a\"\n\
               export state items = [\"apple\", \"berry\", \"grape\"]\n\
               export let names = items.fuzzy(q, x => x).map(m => m.item)\n\
               bar B {\n  \
                 for m in items.fuzzy(q, x => x) key m.item {\n    \
                   text m.item\n  \
                 }\n\
               }\n";
    let mut shell = boot(src);
    assert_eq!(shell.list("names"), ["apple", "grape"]);
    assert_eq!(shell.texts(), ["apple", "grape"]);
    shell.set("q", Value::text("rr"));
    assert_eq!(shell.list("names"), ["berry"]);
    assert_eq!(shell.texts(), ["berry"]);
    shell.set(
        "items",
        Value::list(vec![Value::text("cherry"), Value::text("kiwi")]),
    );
    assert_eq!(shell.list("names"), ["cherry"]);
    assert_eq!(shell.texts(), ["cherry"]);
}

/// A lambda's other reads are the selector's parameters: a change there is a
/// new search.
#[test]
fn a_selector_reading_state_reruns_when_it_changes() {
    let src = format!(
        "{ITEMS}export state prefix = \"\"\n\
         state items: [Item] key id = [\n  \
           Item(id: 1, name: \"apple\"),\n  \
           Item(id: 2, name: \"grape\"),\n\
         ]\n\
         bar B {{\n  \
           for m in items.fuzzy(\"zap\", x => prefix + x.name) {{\n    \
             text m.item.name\n  \
           }}\n\
         }}\n"
    );
    let mut shell = boot(&src);
    assert!(shell.texts().is_empty());
    shell.set("prefix", Value::text("z"));
    assert_eq!(shell.texts(), ["apple", "grape"]);
}

/// After it: `.take`, in a `for` and in a view `let`; before it: `.filter`.
/// The named arguments may come in either order.
#[test]
fn it_chains_with_take_and_filter() {
    let src = format!(
        "{ITEMS}export state q = \"a\"\n\
         export state n = 2\n\
         state items: [Item] key id = [\n  \
           Item(id: 1, name: \"aaa\"),\n  \
           Item(id: 2, name: \"xaa\"),\n  \
           Item(id: 3, name: \"xxa\"),\n  \
           Item(id: 4, name: \"bbb\"),\n\
         ]\n\
         state plain = [\"aaa\", \"xaa\", \"xxa\", \"bbb\"]\n\
         let best = items.fuzzy(by: x => x.name, query: q).take(n)\n\
         export let count = best.len\n\
         export let top = plain.fuzzy(q, x => x).take(n).map(m => m.item)\n\
         bar B {{\n  \
           for m in best {{ text m.item.name }}\n  \
           for m in items.filter(x => x.id != 1).fuzzy(q, x => x.name).take(1) {{\n    \
             text join(\"\", \"then \", m.item.name)\n  \
           }}\n\
         }}\n"
    );
    let mut shell = boot(&src);
    assert_eq!(shell.number("count"), 2.0);
    assert_eq!(shell.list("top").len(), 2);
    let texts = shell.texts();
    assert_eq!(texts.len(), 3, "{texts:?}");
    assert_eq!(shell.list("top"), texts[..2]);
    assert!(texts[2].starts_with("then "), "{texts:?}");
    shell.set("n", Value::int(1));
    assert_eq!(shell.number("count"), 1.0);
    assert_eq!(shell.list("top").len(), 1);
    assert_eq!(shell.texts().len(), 2);
    shell.set("q", Value::text("bb"));
    assert_eq!(shell.texts(), ["bbb", "then bbb"]);
}

fn window(host: &SchemaHost, id: &str, title: &str) -> Value {
    host.record(
        "Window",
        &[("id", Value::text(id)), ("title", Value::text(title))],
    )
}

/// A service's keyed list: rows by the window's `id`, following the service
/// and the query.
#[test]
fn it_searches_a_services_list() {
    let src = "export state q = \"fire\"\n\
               bar B {\n  \
                 for m in windows.all.fuzzy(q, w => w.title).take(5) {\n    \
                   text m.item.title { marks: m.ranges }\n  \
                 }\n\
               }\n";
    let mut shell = boot_with(src, |rt, host| {
        let list = vec![
            window(host, "a", "Firefox"),
            window(host, "b", "foot"),
            window(host, "c", "Bonfire"),
        ];
        host.set(rt, "windows.all", Value::list(list)).unwrap();
    });
    let mut texts = shell.texts();
    assert_eq!(texts.len(), 2);
    texts.sort();
    assert_eq!(texts, ["Bonfire", "Firefox"]);
    let firefox = shell.scene.find_text("Firefox").unwrap();
    assert_eq!(
        shell.scene.prop(firefox, Prop::Marks),
        Some(&PropValue::List(vec![PropValue::List(vec![
            PropValue::Number(0.0),
            PropValue::Number(4.0),
        ])])),
    );
    shell.set("q", Value::text("fo"));
    assert_eq!(shell.scene.find_text("Firefox"), Some(firefox), "by `id`");
    let list = vec![
        window(&shell.host, "a", "Firefox"),
        window(&shell.host, "d", "fog"),
    ];
    shell
        .host
        .set(&shell.rt, "windows.all", Value::list(list))
        .unwrap();
    shell.flush();
    assert_eq!(shell.scene.find_text("Firefox"), Some(firefox));
    assert!(shell.scene.find_text("fog").is_some());
    assert!(shell.scene.find_text("foot").is_none());
}

/// A `key` on the `state`, not the record: the same, by `item.<key>`, when
/// the list isn't a view either (`skip` isn't one).
#[test]
fn a_state_key_reaches_the_rows_through_the_item() {
    let src = format!(
        "{ITEMS}export state q = \"a\"\n\
         export state items: [Item] key id = [\n  \
           Item(id: 1, name: \"apple\"),\n  \
           Item(id: 2, name: \"grape\"),\n\
         ]\n\
         bar B {{\n  \
           for m in items.skip(0).fuzzy(q, x => x.name) {{ text m.item.name }}\n\
         }}\n"
    );
    let mut shell = boot(&src);
    let grape = shell.scene.find_text("grape").unwrap();
    shell.set("q", Value::text("grape"));
    assert_eq!(shell.texts(), ["grape"]);
    assert_eq!(shell.scene.find_text("grape"), Some(grape));
}

/// A loading list stays loading, as `filter` does.
#[test]
fn a_loading_list_stays_loading() {
    let src = "export state q = \"\"\n\
               let hits = apps.search(\"\").fuzzy(q, h => h.app.name)\n\
               bar B {\n  \
                 if hits.pending { text \"loading\" }\n  \
                 for m in hits { text m.item.app.name }\n\
               }\n";
    let mut shell = boot_with(src, |rt, host| {
        let app = |id: &str, name: &str| {
            host.record(
                "App",
                &[("id", Value::text(id)), ("name", Value::text(name))],
            )
        };
        let list = vec![app("f", "Foot"), app("g", "Gimp")];
        host.set(rt, "apps.all", Value::list(list)).unwrap();
    });
    assert_eq!(shell.texts(), ["Foot", "Gimp"]);
    shell.set("q", Value::text("gi"));
    assert_eq!(shell.texts(), ["Gimp"]);
}

/// `Match<T>` can be written, so a component can take one, and its fields
/// are checked as `T`'s; a config's own `type Match` keeps its name.
#[test]
fn match_of_t_is_a_type_a_component_can_take() {
    let src = format!(
        "{ITEMS}component Row(m: Match<Item>) {{ text m.item.name {{ marks: m.ranges }} }}\n\
         state items: [Item] key id = [Item(id: 1, name: \"apple\")]\n\
         let score: float = items.fuzzy(\"a\", x => x.name).first?.score ?? 0\n\
         export let scored = score > 0\n\
         bar B {{ for m in items.fuzzy(\"a\", x => x.name) {{ Row(m) }} }}\n"
    );
    let shell = boot(&src);
    assert_eq!(shell.texts(), ["apple"]);
    assert_eq!(shell.inst.get("f.scored").unwrap(), Value::Bool(true));

    let own = "type Match { a: int }\nstate m = Match(a: 1)\nexport let a = m.a\n\
               bar B { text \"x\" }\n";
    assert_eq!(boot(own).number("a"), 1.0);
}

/// A reload moving the program's record ids leaves `Match<T>` the new
/// program's: the view is rebuilt, and an item arriving after it is read
/// by its fields.
#[test]
fn a_reload_moving_record_ids_keeps_the_matches_readable() {
    let files = |extra: &str| {
        format!(
            "{extra}{ITEMS}export state q = \"a\"\n\
             export state items: [Item] key id = [Item(id: 1, name: \"apple\")]\n\
             bar B {{\n  \
               box {{ on click {{ items.push(Item(id: 2, name: \"grape\")) }} }}\n  \
               for m in items.fuzzy(q, x => x.name) {{ text m.item.name {{ opacity: m.score > 0 ? 1 : 0.5 }} }}\n\
             }}\n"
        )
    };
    let one = |text: String, prev: Option<&Build>| {
        let mut map = SourceMap::new();
        map.add("f.strand", text);
        Build::compile(prev, map).unwrap_or_else(|d| panic!("{d:#?}"))
    };
    let first = one(files(""), None);
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &first.program.types));
    screens(&rt, &host);
    let inst = Instance::from_build(&rt, &first, host.clone(), Storage::none());
    let mut shell = Shell {
        rt,
        host,
        inst,
        scene: SceneMirror::new(),
    };
    shell.flush();
    assert_eq!(shell.texts(), ["apple"]);
    let next = one(
        files(
            "type Other { x: int }\nstate others: [Other] = []\n\
             let z = others.fuzzy(\"\", o => join(\"\", o.x))\nexport let n = z.len\n",
        ),
        Some(&first),
    );
    let _ = shell.inst.reload(&next);
    shell.flush();
    assert_eq!(shell.texts(), ["apple"]);
    assert_eq!(shell.number("n"), 0.0);
    let b = shell.scene.of_kind(NodeKind::Box)[0];
    shell.inst.event(b, "click", Vec::new());
    shell.flush();
    assert_eq!(shell.texts(), ["apple", "grape"]);
    let grape = shell.scene.find_text("grape").unwrap();
    assert_eq!(
        shell.scene.prop(grape, Prop::Opacity),
        Some(&PropValue::Number(1.0)),
        "its score read through the new program's record",
    );
}

fn errors(src: &str) -> Vec<(String, String)> {
    let mut map = SourceMap::new();
    map.add("f.strand", src.to_string());
    let compiled = strand_compiler::compile(&map);
    compiled
        .diagnostics
        .iter()
        .filter(|d| d.severity == strand_compiler::diagnostic::Severity::Error)
        .map(|d| (d.code.to_string(), d.message.clone()))
        .collect()
}

/// The checker holds the call to its shape, and types what comes out by
/// the item.
#[test]
fn the_checker_types_the_query_the_selector_and_the_result() {
    let wrap = |line: &str| {
        format!("{ITEMS}state items: [Item] = []\nstate n = 0\n{line}\nbar B {{ text \"x\" }}\n")
    };
    let code = |line: &str| -> String {
        let e = errors(&wrap(line));
        assert_eq!(e.len(), 1, "{line}: {e:?}");
        e[0].0.clone()
    };
    // Right.
    for ok in [
        "let a = items.fuzzy(\"a\", x => x.name)",
        "let a = items.fuzzy(\"a\", x => x.name).take(2).first?.item?.id",
        "let a: [Match<Item>] = items.fuzzy(\"a\", x => x.name)",
        "fn pick(x: Item) -> text { x.name }\nlet a = items.fuzzy(\"a\", pick)",
    ] {
        let e = errors(&wrap(ok));
        assert!(e.is_empty(), "{ok}: {e:?}");
    }
    // The selector returns text.
    assert_eq!(
        code("let a = items.fuzzy(\"a\", x => x.id)"),
        "check::type_mismatch",
    );
    // The selector is a function of one item.
    for bad in [
        "let a = items.fuzzy(\"a\", \"name\")",
        "let a = items.fuzzy(\"a\", (x, y) => x.name)",
    ] {
        assert!(!errors(&wrap(bad)).is_empty(), "{bad}");
    }
    // Its item is the list's.
    assert_eq!(
        code("let a = items.fuzzy(\"a\", x => x.nope)"),
        "check::unknown_field",
    );
    // The query is text.
    assert_eq!(
        code("let a = items.fuzzy(3, x => x.name)"),
        "check::type_mismatch",
    );
    // Both are needed.
    assert!(!errors(&wrap("let a = items.fuzzy(\"a\")")).is_empty());
    // What comes out is `Match<Item>`.
    let e = errors(&wrap(
        "let a = items.fuzzy(\"a\", x => x.name).first?.item?.nope",
    ));
    assert_eq!(e.len(), 1, "{e:?}");
    assert_eq!(e[0].0, "check::unknown_field");
    let e = errors(&wrap(
        "let a: [Match<text>] = items.fuzzy(\"a\", x => x.name)",
    ));
    assert_eq!(e.len(), 1, "{e:?}");
    assert!(e[0].1.contains("Match<Item>"), "{e:?}");
    // Written, it takes one type.
    assert_eq!(code("let a: Match? = null"), "check::unknown_type");
}

/// A selector is pure as `filter`'s lambda is: the same diagnostics for the
/// same body.
#[test]
fn a_selector_is_as_pure_as_a_filter() {
    let wrap = |call: &str| {
        format!(
            "{ITEMS}state items: [Item] = []\nstate n = 0\nlet a = {call}\nbar B {{ text \"x\" }}\n"
        )
    };
    for (selector, predicate, refused) in [
        (
            "x => { notifications.clear()\n x.name }",
            "x => { notifications.clear()\n true }",
            true,
        ),
        // A write in a binding's lambda: what `filter` says of it.
        ("x => { n = 1\n x.name }", "x => { n = 1\n true }", false),
        (
            "x => join(\"\", apps.search(x.name).len)",
            "x => apps.search(x.name).len > 0",
            false,
        ),
    ] {
        let as_filter = errors(&wrap(&format!("items.filter({predicate})")));
        assert_eq!(!as_filter.is_empty(), refused, "{predicate}: {as_filter:?}");
        let as_fuzzy = errors(&wrap(&format!("items.fuzzy(\"a\", {selector})")));
        let codes = |e: &[(String, String)]| e.iter().map(|x| x.0.clone()).collect::<Vec<_>>();
        assert_eq!(codes(&as_fuzzy), codes(&as_filter), "{selector}");
    }
}

/// What one keystroke costs over 5,000 items: selector calls, always; time,
/// printed (`cargo test --profile timing -p strand-compiler --test fuzzy
/// -- --nocapture five`): benchmarks.md "Fuzzy search".
#[test]
fn five_thousand_items_per_keystroke() {
    let names = ["Firefox", "Files", "Foot", "Écrire", "Terminal", "Settings"];
    let items: Vec<String> = (0..5000)
        .map(|i| format!("Item(id: {i}, name: \"{} {i}\")", names[i % names.len()]))
        .collect();
    let keyed = format!(
        "{ITEMS}export state q = \"\"\n\
         state items: [Item] key id = [{}]\n\
         let best = items.fuzzy(q, x => x.name).take(50)\n\
         bar B {{ for m in best {{ text m.item.name {{ marks: m.ranges }} }} }}\n",
        items.join(",")
    );
    let plain = format!(
        "{ITEMS}export state q = \"\"\n\
         state items: [Item] = [{}]\n\
         let best = items.fuzzy(q, x => x.name).take(50)\n\
         bar B {{ for m in best key m.item.id {{ text m.item.name {{ marks: m.ranges }} }} }}\n",
        items.join(",")
    );
    for (what, src) in [("keyed", keyed), ("plain", plain)] {
        let mut shell = boot(&src);
        assert_eq!(shell.texts().len(), 50);
        let queries = [
            "f", "fi", "fil", "file", "files", "fire", "ter", "é", "se 49",
        ];
        let mut times = Vec::new();
        for q in queries {
            let before = shell.calls();
            let t = Instant::now();
            shell.set("q", Value::text(q));
            times.push(t.elapsed().as_secs_f64() * 1000.0);
            assert!(
                shell.calls() - before >= 5000,
                "{what} {q}: one selector call per item"
            );
            assert!(shell.texts().len() <= 50);
        }
        times.sort_by(f64::total_cmp);
        println!(
            "fuzzy {what}, 5000 items, take(50): {:.1} ms median, {:.1} ms worst per keystroke",
            times[times.len() / 2],
            times[times.len() - 1]
        );
    }
}
