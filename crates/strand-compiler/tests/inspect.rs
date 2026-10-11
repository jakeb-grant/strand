//! (M5) The inspector's compiler half (docs/m5-plan.md, S-inspect wave
//! 1): a node's identity, its state cells and their kept badges, reset
//! subtree, the source hash write-back compares against, and the dump.

use std::rc::Rc;
use std::time::Duration;

use strand_compiler::instantiate::inspect::{CellInfo, REDACTED, node_ref, parse_node_ref};
use strand_compiler::instantiate::{Instance, SceneMirror, Storage, Update};
use strand_compiler::reconcile::Build;
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_compiler::{FileId, SourceMap};
use strand_core::Runtime;
use strand_scene::{NodeId, NodeKind, Prop, PropValue};

struct Shell {
    inst: Instance,
    scene: SceneMirror,
    build: Build,
    storage: Storage,
}

fn compile(prev: Option<&Build>, files: &[(&str, &str)]) -> Build {
    let mut map = SourceMap::new();
    for (name, text) in files {
        map.add(*name, text.to_string());
    }
    Build::compile(prev, map).unwrap_or_else(|d| panic!("{d:#?}"))
}

fn screens(rt: &Runtime, host: &SchemaHost, names: &[&str]) {
    let list = names
        .iter()
        .map(|n| {
            host.record(
                "Screen",
                &[
                    ("id", Value::text(format!("Mock | {n} | Display"))),
                    ("name", Value::text(*n)),
                    ("make", Value::text("Mock")),
                    ("model", Value::text(*n)),
                    ("description", Value::text("Display")),
                ],
            )
        })
        .collect();
    host.set(rt, "screens.all", Value::list(list)).unwrap();
}

fn boot(files: &[(&str, &str)], storage: Storage, monitors: &[&str]) -> Shell {
    let build = compile(None, files);
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &build.program.types));
    screens(&rt, &host, monitors);
    let inst = Instance::from_build(&rt, &build, host, storage.clone());
    let mut shell = Shell {
        inst,
        scene: SceneMirror::new(),
        build,
        storage,
    };
    let u = shell.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    shell
}

impl Shell {
    fn flush(&mut self) -> Update {
        let u = self.inst.flush();
        self.scene.apply(&u.diff).unwrap();
        u
    }

    fn reload(&mut self, files: &[(&str, &str)]) {
        let build = compile(Some(&self.build), files);
        self.inst.reload(&build);
        self.build = build;
        let u = self.flush();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
    }

    /// Every text node showing `text`, in tree order.
    fn texts(&self, text: &str) -> Vec<NodeId> {
        self.scene
            .walk()
            .into_iter()
            .filter(|n| {
                matches!(self.scene.prop(*n, Prop::Text), Some(PropValue::Text(t)) if t == text)
            })
            .collect()
    }

    fn text(&self, text: &str) -> NodeId {
        self.scene
            .find_text(text)
            .unwrap_or_else(|| panic!("no text {text:?} in\n{}", self.scene.render()))
    }

    fn cell(&self, node: NodeId, path: &str) -> CellInfo {
        let cells = self.inst.cells_of(node);
        cells
            .iter()
            .find(|c| c.path == path)
            .cloned()
            .unwrap_or_else(|| panic!("no cell {path} in {cells:?}"))
    }

    fn click(&mut self, node: NodeId) {
        assert!(self.inst.event(node, "click", Vec::new()));
        self.flush();
    }
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("strand-inspect-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

const ROWS: &str = "type Row { id: int; label: text }\n\
state rows: [Row] key id = [Row(id: 1, label: \"a\"), Row(id: 2, label: \"b\")]\n\
bar Top {\n  row {\n    text \"x\"\n    text \"y\" { id: second }\n    for r in rows { Item r }\n  }\n}\n\
component Item(r: Row) { text r.label }\n";

/// A per-monitor bar's rows: the monitor and the row's key make the
/// instance path, `id:` names the element, and siblings of one kind are
/// counted in render's order.
#[test]
fn identity_of_per_monitor_keyed_rows() {
    let shell = boot(
        &[("t.strand", ROWS)],
        Storage::none(),
        &["DP-1", "HDMI-A-1"],
    );
    let bs = shell.texts("b");
    assert_eq!(bs.len(), 2, "one per monitor");
    let ids: Vec<_> = bs
        .iter()
        .map(|n| shell.inst.identity(*n).unwrap())
        .collect();
    for (id, monitor) in ids.iter().zip(["DP-1", "HDMI-A-1"]) {
        assert!(
            id.instance_path.starts_with('[')
                && id.instance_path.contains(monitor)
                && id.instance_path.ends_with("][2]"),
            "{id:?}"
        );
        assert_eq!(id.key.as_deref(), Some("2"), "the row's key");
        assert_eq!(id.scope, "Item");
        assert_eq!(id.id, None);
        // x, y, a, b: four texts under the row.
        assert_eq!(id.sibling_index, 3);
    }
    assert_ne!(ids[0].instance_path, ids[1].instance_path);
    let a = shell.inst.identity(shell.texts("a")[0]).unwrap();
    assert_eq!((a.key.as_deref(), a.sibling_index), (Some("1"), 2));
    let y = shell.inst.identity(shell.texts("y")[0]).unwrap();
    assert_eq!(y.id.as_deref(), Some("second"));
    assert_eq!(y.scope, "t", "a top-level surface's: the file's module");
    assert_eq!(y.sibling_index, 1);
    assert!(y.instance_path.contains("DP-1") && !y.instance_path.contains("]["));
    assert!(y.key.as_deref().is_some_and(|k| k.contains("DP-1")));
    // The bars themselves: two of one kind among the surface roots.
    let bars: Vec<u32> = shell
        .scene
        .of_kind(NodeKind::Bar)
        .into_iter()
        .map(|b| shell.inst.identity(b).unwrap().sibling_index)
        .collect();
    let mut sorted = bars.clone();
    sorted.sort();
    assert_eq!(sorted, [0, 1]);
    // Not a node of this instance.
    assert_eq!(shell.inst.identity(NodeId::new(9999, 0)), None);
    assert!(shell.inst.cells_of(NodeId::new(9999, 0)).is_empty());
}

const COUNTER: &str = "state total = 0\n\
bar Top {\n  row {\n    Counter \"one\"\n    Counter \"two\"\n  }\n}\n\
component Counter(label: text) {\n  state q = \"x\"\n  state hits = 0 persist\n  \
text join(\"\", label, q) { on click { q = \"fir\"; total += 1 } }\n}\n";

/// Kept over a changed default: shown while the cell keeps its value,
/// gone once it is reset. Persisted: a `persist` cell.
#[test]
fn kept_and_persisted_badges() {
    let dir = temp_dir("badges");
    let storage = Storage::in_dirs(dir.join("state"), &dir);
    let mut shell = boot(&[("t.strand", COUNTER)], storage, &["DP-1"]);
    let one = shell.text("onex");
    shell.click(one);
    let q = shell.cell(one, "Counter.q");
    assert!(!q.kept && !q.persisted && q.layer.is_none(), "{q:?}");
    let hits = shell.cell(one, "Counter.hits");
    assert!(hits.persisted && !hits.kept, "{hits:?}");
    // The file's `total`, read by the click handler, not by a prop: not
    // the node's.
    assert!(shell.inst.cells_of(one).iter().all(|c| c.path != "t.total"));
    // A new default for `q`: the clicked counter keeps "fir".
    let edited = COUNTER.replace("state q = \"x\"", "state q = \"y\"");
    shell.reload(&[("t.strand", &edited)]);
    let one = shell.text("onefir");
    let two = shell.text("twoy");
    assert!(
        shell.cell(one, "Counter.q").kept,
        "kept over the new default"
    );
    assert!(!shell.cell(two, "Counter.q").kept, "took the new default");
    // A later reload keeps the badge; `[reset]` clears it.
    shell.reload(&[("t.strand", &edited.replace("join(\"\"", "join(\"\" "))]);
    let one = shell.text("onefir");
    assert!(shell.cell(one, "Counter.q").kept);
    // Both counters' cells share a path (`Counter.q`): reset sets both
    // to the default, so the badge goes.
    shell.inst.reset("Counter.q").unwrap();
    shell.flush();
    let one = shell.text("oney");
    assert!(!shell.cell(one, "Counter.q").kept);
    if let Some(s) = &shell.storage.persist {
        s.sync(Duration::from_secs(5));
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// From an overlay layer: a settings field whose file is read-only is
/// written to the runtime overlay, and the field says so; the field it
/// does not write still comes from the file.
#[test]
fn a_settings_overlay_shows_its_layer() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir("layer");
    std::fs::write(dir.join("prefs.toml"), "compact = true\n").unwrap();
    // A Nix-store-like file: read, but writes go to the overlay.
    std::fs::set_permissions(
        dir.join("prefs.toml"),
        std::fs::Permissions::from_mode(0o444),
    )
    .unwrap();
    let storage = Storage::in_dirs(dir.join("state"), &dir);
    let src = "state prefs from \"prefs.toml\" { compact: bool = false; gap: int = 4 }\n\
bar B { text prefs.compact ? \"compact\" : \"roomy\" }\n";
    let mut shell = boot(&[("theme.strand", src)], storage.clone(), &["DP-1"]);
    let t = shell.text("compact");
    let layer = |shell: &Shell, node, f: &str| shell.cell(node, &format!("theme.prefs.{f}")).layer;
    assert_eq!(layer(&shell, t, "compact"), Some("file"));
    // The node reads only `compact`.
    assert!(
        shell
            .inst
            .cells_of(t)
            .iter()
            .all(|c| c.path != "theme.prefs.gap")
    );
    shell.inst.set_text("prefs.compact", "false").unwrap();
    // The write goes out after the persist debounce.
    for at in [0.01, 2.0] {
        let u = shell.inst.tick(Duration::from_secs_f64(at));
        shell.scene.apply(&u.diff).unwrap();
    }
    if let Some(s) = &storage.settings {
        assert!(s.sync(Duration::from_secs(5)));
    }
    shell.flush();
    let t = shell.text("roomy");
    assert_eq!(layer(&shell, t, "compact"), Some("overlay"));
    assert!(!shell.cell(t, "theme.prefs.compact").persisted);
    let _ = std::fs::set_permissions(
        dir.join("prefs.toml"),
        std::fs::Permissions::from_mode(0o644),
    );
    let _ = std::fs::remove_dir_all(dir);
}

const PANELS: &str = "state top = \"t\"\n\
bar Top {\n  row {\n    Panel \"left\"\n    Panel \"right\"\n    \
text top { on click { top = \"T\" } }\n  }\n}\n\
component Panel(name: text) {\n  state open = false\n  state seen = 0 persist\n  \
col {\n    text join(\"\", name, open, seen) { on click { open = !open; seen += 1 } }\n    \
for i in [\"p\"] key i { Leaf i }\n  }\n}\n\
component Leaf(i: text) { state n = 0\n  text join(\"\", \"leaf\", i, n) { on click { n += 1 } } }\n";

/// Reset subtree resets the state of the instances under a node (and of
/// the component the node is the root of), as a hard reload would, and
/// nothing else; a reload afterwards keeps the rest.
#[test]
fn reset_subtree_resets_only_the_subtree() {
    let dir = temp_dir("reset");
    let storage = Storage::in_dirs(dir.join("state"), &dir);
    let mut shell = boot(&[("t.strand", PANELS)], storage, &["DP-1"]);
    let left = shell.text("leftfalse0");
    shell.click(left);
    let right = shell.text("rightfalse0");
    shell.click(right);
    for n in shell.texts("leafp0") {
        shell.click(n);
    }
    let top = shell.text("t");
    shell.click(top);
    assert_eq!(shell.texts("leafp1").len(), 2);
    // The left panel's column: its own root.
    let left = shell.text("lefttrue1");
    let col = shell.scene.parent(left).unwrap();
    assert_eq!(shell.scene.kind(col), Some(NodeKind::Col));
    // `open` and its leaf's `n` change; `seen` is persisted and kept.
    assert_eq!(shell.inst.reset_subtree(col).unwrap(), 2);
    shell.flush();
    assert!(
        shell.scene.find_text("leftfalse1").is_some(),
        "{}",
        shell.scene.render()
    );
    assert!(
        shell.scene.find_text("righttrue1").is_some(),
        "the sibling kept"
    );
    assert_eq!(shell.texts("leafp1").len(), 1, "the right leaf kept");
    assert_eq!(shell.texts("leafp0").len(), 1);
    assert!(shell.scene.find_text("T").is_some(), "the bar's state kept");
    // A leaf alone: only its own cell.
    let leaf = shell.texts("leafp1")[0];
    assert_eq!(shell.inst.reset_subtree(leaf).unwrap(), 1);
    shell.flush();
    assert_eq!(shell.texts("leafp0").len(), 2);
    // A reload keeps what was not reset.
    shell.click(shell.texts("leafp0")[0]);
    shell.reload(&[("t.strand", &PANELS.replace("key i", "key i "))]);
    assert!(shell.scene.find_text("leftfalse1").is_some());
    assert!(shell.scene.find_text("righttrue1").is_some());
    assert_eq!(shell.texts("leafp1").len(), 1);
    assert!(shell.scene.find_text("T").is_some());
    // An unknown node is refused.
    assert!(shell.inst.reset_subtree(NodeId::new(9999, 0)).is_err());
    let _ = std::fs::remove_dir_all(dir);
}

/// The source hash is BLAKE3 of the text the program was compiled from,
/// and follows a reload.
#[test]
fn source_hash_is_the_compiled_text() {
    let a = "bar B { text \"a\" }\n";
    let mut shell = boot(&[("t.strand", a)], Storage::none(), &["DP-1"]);
    let hash = |s: &str| blake3::hash(s.as_bytes()).to_hex().to_string();
    assert_eq!(shell.inst.source_hash(FileId(0)), Some(hash(a)));
    assert_eq!(shell.inst.source_hash(FileId(7)), None);
    let b = "bar B { text \"b\" }\n";
    shell.reload(&[("t.strand", b)]);
    assert_eq!(shell.inst.source_hash(FileId(0)), Some(hash(b)));
}

/// The dump: every node in tree order with its origin, identity, props,
/// cells and token chains; a password input's value never appears.
#[test]
fn a_dump_shows_the_tree_and_never_a_password() {
    let src = "bar B {\n  set { $surface: $surface.alpha(0.5) }\n  state pw = \"hunter22\"\n  \
input { type: password; text: <-> pw }\n  text pw\n  box { bg: $surface.hi }\n}\n";
    let shell = boot(&[("t.strand", src)], Storage::none(), &["DP-1"]);
    let dump = shell.inst.dump(None).unwrap();
    let text = dump.to_string();
    assert!(!text.contains("hunter22"), "{text}");
    assert!(text.contains(REDACTED));
    let nodes = dump["nodes"].as_array().unwrap();
    let walk: Vec<String> = shell.scene.walk().into_iter().map(node_ref).collect();
    let ids: Vec<&str> = nodes.iter().map(|n| n["id"].as_str().unwrap()).collect();
    assert_eq!(ids, walk, "tree order");
    for n in nodes {
        assert_eq!(
            parse_node_ref(n["id"].as_str().unwrap())
                .map(node_ref)
                .as_deref(),
            n["id"].as_str()
        );
    }
    let input = nodes.iter().find(|n| n["kind"] == "input").unwrap();
    assert_eq!(input["props"]["text"], REDACTED);
    assert_eq!(input["origin"]["file"], "t.strand");
    assert_eq!(input["origin"]["line"], 4);
    assert_eq!(input["origin"]["col"], 3);
    let shown = nodes.iter().find(|n| n["kind"] == "text").unwrap();
    assert_eq!(shown["props"]["text"], REDACTED, "the text showing it too");
    // The box's bg: surface.hi (base) ← surface (the set) ← surface
    // (the palette).
    let b = nodes.iter().find(|n| n["kind"] == "box").unwrap();
    let chain = b["tokens"][0]["chain"].as_array().unwrap();
    let steps: Vec<(&str, &str)> = chain
        .iter()
        .map(|s| (s["path"].as_str().unwrap(), s["kind"].as_str().unwrap()))
        .collect();
    assert_eq!(
        steps,
        [
            ("surface.hi", "global"),
            ("surface", "set"),
            ("surface", "global")
        ],
        "{chain:?}"
    );
    assert_eq!(chain[1]["level"], 1);
    assert_eq!(chain[0]["origin"], "base");
    assert!(chain[2]["origin"].as_str().unwrap().starts_with("palette:"));
    assert_eq!(
        dump["files"]["t.strand"],
        blake3::hash(src.as_bytes()).to_hex().to_string()
    );
    // One surface's tree, and one node.
    let root = shell.scene.roots()[0];
    assert_eq!(
        shell.inst.dump(Some(root)).unwrap()["nodes"]
            .as_array()
            .unwrap()
            .len(),
        nodes.len()
    );
    assert!(shell.inst.dump(Some(NodeId::new(9999, 0))).is_none());
    let one = shell
        .inst
        .inspect_node(shell.scene.of_kind(NodeKind::Input)[0])
        .unwrap();
    assert!(!one.to_string().contains("hunter22"));
    assert_eq!(one["node"]["props"]["type"], "password");
}
