//! The reload fuzzer (design.md, "How reload is tested"; the M1 exit
//! gate "10k random edits with no panic or blank frame").
//!
//! Random edits of a three-file config (token values, props and
//! bindings, nodes added, removed and moved, keyed list entries added,
//! removed and moved, a component moved to another file, `state` defaults, cells renamed and retyped across two
//! files, syntax errors and partial multi-file saves) are replayed
//! through the five editor save styles: one live `strand run` pipeline
//! per style (the real watcher, compiler worker, logic thread and IPC
//! socket, without Wayland), each on its own copy of the config on
//! tmpfs, every edit saved into all five. Between edits, state is
//! changed by clicks, as a user would.
//!
//! Every diff must keep the bar up and add no surface but the error
//! overlay (no blank frame, no leaked surface). A partial save or a
//! broken save is held back and changes nothing. After each committed
//! edit every pipeline shows exactly what a cold boot of the same files
//! shows once the state the edit table keeps is written into it: kept
//! where the table keeps it, the new default where the cell still held
//! the old one, reset when renamed or retyped, fresh for a node added.
//! The table's resets are also what `strand watch` reports.
//!
//! `STRAND_FUZZ_EDITS` sets the number of edits (60), `STRAND_FUZZ_SEED`
//! the seed; the directory is `/dev/shm` when it is there (tmpfs),
//! `STRAND_FUZZ_DIR` overrides it. The 10,000-edit run is the nightly
//! CI job (`docs/m1-report.md`).

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value as Json;
use strand_compiler::SourceMap;
use strand_compiler::instantiate::{Instance, SceneMirror, Storage};
use strand_compiler::reconcile::Build;
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_core::Runtime;
use strand_scene::{NodeKind, Prop, PropValue, SceneDiff};

use crate::ipc;
use crate::live::Worker;
use crate::run::tests::{inbox, screen};
use crate::run::{Live, NodeEvent, ToLogic, logic, set_screens};

/// How long one pipeline may take to answer a save or show a scene.
const PATIENCE: Duration = Duration::from_secs(20);

const FILES: [&str; 3] = ["theme.strand", "cells.strand", "bar.strand"];
const CELL_NAMES: [&str; 8] = ["a", "b", "c", "d", "e", "f", "g", "h"];
const CHIP_LABELS: [&str; 4] = ["x", "y", "z", "w"];
const LIST_LABELS: [&str; 4] = ["p", "q", "r", "s"];

/// A small deterministic generator (xorshift).
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n.max(1)
    }

    fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.below(v.len() as u64) as usize]
    }
}

/// One `export state` of `cells.strand`.
#[derive(Clone, Debug, PartialEq)]
struct Cell {
    name: &'static str,
    text: bool,
    default: i64,
}

impl Cell {
    fn default_value(&self) -> V {
        match self.text {
            true => V::Text(format!("s{}", self.default)),
            false => V::Int(self.default),
        }
    }
}

/// A child of the bar's row after the fixed texts.
#[derive(Clone, Debug, PartialEq)]
enum Item {
    /// `Chip "x"`, or `row { Chip "x" }` when wrapped.
    Chip {
        label: &'static str,
        wrapped: bool,
    },
    Static(u32),
}

/// The config, as data.
#[derive(Clone, Debug, PartialEq)]
struct Model {
    bg: u32,
    fg: u32,
    tag: u32,
    height: u32,
    n_default: i64,
    cells: Vec<Cell>,
    chip_default: bool,
    /// `component Chip` lives in `bar.strand` (else `theme.strand`).
    chip_in_bar: bool,
    items: Vec<Item>,
    next_static: u32,
    /// `export let list` in `theme.strand`: a `Chip` per entry, keyed
    /// by it (list-item state).
    list: Vec<&'static str>,
}

impl Model {
    fn first() -> Self {
        Model {
            bg: 0x204080,
            fg: 0xffeedd,
            tag: 1,
            height: 32,
            n_default: 0,
            cells: vec![
                Cell {
                    name: "a",
                    text: false,
                    default: 1,
                },
                Cell {
                    name: "b",
                    text: true,
                    default: 2,
                },
                Cell {
                    name: "c",
                    text: false,
                    default: 3,
                },
            ],
            chip_default: false,
            chip_in_bar: false,
            items: vec![
                Item::Chip {
                    label: "x",
                    wrapped: false,
                },
                Item::Static(0),
                Item::Chip {
                    label: "y",
                    wrapped: true,
                },
            ],
            next_static: 1,
            list: vec!["p", "q"],
        }
    }

    fn chip_decl(&self) -> String {
        format!(
            "component Chip(label: text) {{\n  state on = {}\n  text join(\" \", label, on) {{ color: $chip.fg; on click {{ on = !on }} }}\n}}\n",
            self.chip_default
        )
    }

    /// Every chip's label: the bar's own, then the list's.
    fn chips(&self) -> Vec<&'static str> {
        self.items
            .iter()
            .filter_map(|i| match i {
                Item::Chip { label, .. } => Some(*label),
                Item::Static(_) => None,
            })
            .chain(self.list.iter().copied())
            .collect()
    }

    /// The three files' texts, in [`FILES`] order.
    fn texts(&self) -> [String; 3] {
        let mut theme = format!(
            "tokens base {{ bar.bg: #{:06x}; chip.fg: #{:06x} }}\nexport let tag = \"t{}\"\nexport let list: [text] = [{}]\n",
            self.bg,
            self.fg,
            self.tag,
            self.list
                .iter()
                .map(|l| format!("\"{l}\""))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let cells: String = self
            .cells
            .iter()
            .map(|c| match c.text {
                true => format!("export state {} = \"s{}\"\n", c.name, c.default),
                false => format!("export state {} = {}\n", c.name, c.default),
            })
            .collect();
        let mut bar = format!(
            "state n = {}\nbar Top {{\n  edge: top; height: {}\n  bg: $bar.bg\n  row {{\n    text join(\" \", \"n\", n) {{ on click {{ n += 1 }} }}\n",
            self.n_default, self.height
        );
        for c in &self.cells {
            let handler = match c.text {
                true => format!("cells.{0} = join(\"\", cells.{0}, \"x\")", c.name),
                false => format!("cells.{} += 1", c.name),
            };
            bar.push_str(&format!(
                "    text join(\" \", \"{0}\", cells.{0}) {{ on click {{ {1} }} }}\n",
                c.name, handler
            ));
        }
        bar.push_str("    text theme.tag\n");
        for i in &self.items {
            match i {
                Item::Chip {
                    label,
                    wrapped: false,
                } => bar.push_str(&format!("    Chip \"{label}\"\n")),
                Item::Chip {
                    label,
                    wrapped: true,
                } => bar.push_str(&format!("    row {{ Chip \"{label}\" }}\n")),
                Item::Static(k) => bar.push_str(&format!("    text \"static{k}\"\n")),
            }
        }
        bar.push_str("    for l in theme.list key l { Chip l }\n  }\n}\n");
        if self.chip_in_bar {
            bar.push_str(&self.chip_decl());
        } else {
            theme.push_str(&self.chip_decl());
        }
        [theme, cells, bar]
    }
}

/// A state value as the bar shows it.
#[derive(Clone, Debug, PartialEq)]
enum V {
    Int(i64),
    Text(String),
}

impl V {
    fn show(&self) -> String {
        match self {
            V::Int(n) => n.to_string(),
            V::Text(t) => t.clone(),
        }
    }

    fn value(&self) -> Value {
        match self {
            V::Int(n) => Value::int(*n),
            V::Text(t) => Value::text(t.as_str()),
        }
    }
}

/// The state the running shells must hold.
#[derive(Clone, Debug, PartialEq)]
struct State {
    n: i64,
    cells: BTreeMap<&'static str, V>,
    chips: BTreeMap<&'static str, bool>,
}

impl State {
    fn defaults(m: &Model) -> Self {
        State {
            n: m.n_default,
            cells: m
                .cells
                .iter()
                .map(|c| (c.name, c.default_value()))
                .collect(),
            chips: m.chips().into_iter().map(|l| (l, m.chip_default)).collect(),
        }
    }

    /// The texts that show it.
    fn texts(&self) -> Vec<String> {
        let mut t = vec![format!("n {}", self.n)];
        t.extend(self.cells.iter().map(|(k, v)| format!("{k} {}", v.show())));
        t.extend(self.chips.iter().map(|(k, v)| format!("{k} {v}")));
        t
    }
}

/// design.md's "State kept?" column: a cell takes a new default only if
/// it still holds the old one; renamed or retyped, it resets; a node
/// added starts fresh. Returns the state and the cells reset.
fn table(old: &Model, new: &Model, s: &State) -> (State, usize) {
    fn adopt<T: PartialEq + Clone>(old_default: &T, new_default: &T, value: &T) -> T {
        if value == old_default {
            new_default.clone()
        } else {
            value.clone()
        }
    }
    let mut next = State {
        n: adopt(&old.n_default, &new.n_default, &s.n),
        cells: BTreeMap::new(),
        chips: BTreeMap::new(),
    };
    for c in &new.cells {
        let v = match old
            .cells
            .iter()
            .find(|o| o.name == c.name && o.text == c.text)
        {
            Some(o) => adopt(&o.default_value(), &c.default_value(), &s.cells[o.name]),
            None => c.default_value(),
        };
        next.cells.insert(c.name, v);
    }
    let resets = old
        .cells
        .iter()
        .filter(|o| {
            !new.cells
                .iter()
                .any(|c| c.name == o.name && c.text == o.text)
        })
        .count();
    for l in new.chips() {
        let v = match s.chips.get(l) {
            Some(v) => adopt(&old.chip_default, &new.chip_default, v),
            None => new.chip_default,
        };
        next.chips.insert(l, v);
    }
    (next, resets)
}

/// One random edit.
enum Edit {
    /// A valid config.
    To(Model, &'static str),
    /// One file saved with a syntax or name error.
    Broken(usize, String),
}

fn edit(m: &Model, r: &mut Rng) -> Edit {
    let mut n = m.clone();
    let kind = match r.below(15) {
        0 => {
            n.bg = r.below(0x100_0000) as u32;
            "token"
        }
        1 => {
            n.fg = r.below(0x100_0000) as u32;
            "token"
        }
        2 => {
            n.tag = r.below(100) as u32;
            "binding"
        }
        3 => {
            n.height = 24 + 8 * r.below(4) as u32;
            "prop"
        }
        4 => {
            let statics: Vec<usize> = (0..n.items.len())
                .filter(|&i| matches!(n.items[i], Item::Static(_)))
                .collect();
            if statics.len() < 3 && (statics.is_empty() || r.below(2) == 0) {
                let at = r.below(n.items.len() as u64 + 1) as usize;
                n.items.insert(at, Item::Static(n.next_static));
                n.next_static += 1;
                "node-added"
            } else {
                n.items.remove(*r.pick(&statics));
                "node-removed"
            }
        }
        5 => {
            let free: Vec<&'static str> = CHIP_LABELS
                .iter()
                .copied()
                .filter(|l| !m.chips().contains(l))
                .collect();
            let own = m.items.iter().any(|i| matches!(i, Item::Chip { .. }));
            if !free.is_empty() && (!own || r.below(2) == 0) {
                let at = r.below(n.items.len() as u64 + 1) as usize;
                let wrapped = r.below(3) == 0;
                n.items.insert(
                    at,
                    Item::Chip {
                        label: r.pick(&free),
                        wrapped,
                    },
                );
                "node-added"
            } else {
                let chips: Vec<usize> = (0..n.items.len())
                    .filter(|&i| matches!(n.items[i], Item::Chip { .. }))
                    .collect();
                n.items.remove(*r.pick(&chips));
                "node-removed"
            }
        }
        6 => {
            if n.items.len() >= 2 && r.below(2) == 0 {
                let i = r.below(n.items.len() as u64) as usize;
                let j = r.below(n.items.len() as u64) as usize;
                n.items.swap(i, j);
            } else if let Some(Item::Chip { wrapped, .. }) = n
                .items
                .iter_mut()
                .filter(|i| matches!(i, Item::Chip { .. }))
                .nth(r.below(4) as usize)
            {
                *wrapped = !*wrapped;
            }
            "move"
        }
        7 => {
            n.chip_in_bar = !n.chip_in_bar;
            "component-moved"
        }
        8 => {
            n.n_default = r.below(6) as i64;
            "state-default"
        }
        9 => {
            let i = r.below(n.cells.len() as u64) as usize;
            n.cells[i].default = r.below(6) as i64;
            "state-default"
        }
        10 => {
            let free: Vec<&'static str> = CELL_NAMES
                .iter()
                .copied()
                .filter(|c| !m.cells.iter().any(|x| x.name == *c))
                .collect();
            let i = r.below(n.cells.len() as u64) as usize;
            n.cells[i].name = r.pick(&free);
            "rename"
        }
        11 => {
            let i = r.below(n.cells.len() as u64) as usize;
            n.cells[i].text = !n.cells[i].text;
            n.cells[i].default = r.below(6) as i64;
            "retype"
        }
        12 => {
            n.chip_default = !n.chip_default;
            "state-default"
        }
        13 => {
            // A list entry added, removed or moved: its item's state
            // goes with it.
            let free: Vec<&'static str> = LIST_LABELS
                .iter()
                .copied()
                .filter(|l| !m.list.contains(l))
                .collect();
            match r.below(3) {
                0 if !free.is_empty() => {
                    let at = r.below(n.list.len() as u64 + 1) as usize;
                    n.list.insert(at, r.pick(&free));
                }
                1 if !n.list.is_empty() => {
                    n.list.remove(r.below(n.list.len() as u64) as usize);
                }
                _ if n.list.len() >= 2 => {
                    let i = r.below(n.list.len() as u64) as usize;
                    let j = r.below(n.list.len() as u64) as usize;
                    n.list.swap(i, j);
                }
                _ => {}
            }
            "list"
        }
        _ => {
            let f = r.below(3) as usize;
            let mut text = m.texts()[f].clone();
            match r.below(4) {
                0 => text.push_str("export state = \n"),
                1 if f != 1 => {
                    // An unclosed block.
                    if let Some(k) = text.rfind('}') {
                        text.replace_range(k..k + 1, "");
                    }
                }
                2 if f == 2 => {
                    text = text.replacen("  row {\n", "  row {\n    txet \"oops\"\n", 1);
                }
                _ => text.push_str("let broken = cells.nope + 1\n"),
            }
            return Edit::Broken(f, text);
        }
    };
    Edit::To(n, kind)
}

fn compile(texts: &[String; 3]) -> Result<Build, Vec<strand_compiler::Diagnostic>> {
    let mut map = SourceMap::new();
    for (name, text) in FILES.iter().zip(texts) {
        map.add(*name, text.clone());
    }
    Build::compile(None, map)
}

/// The scene without the error overlay, surfaces in a fixed order.
fn shown(scene: &SceneMirror) -> String {
    let mut blocks: Vec<String> = Vec::new();
    for line in scene.render().lines() {
        if !line.starts_with(' ') || blocks.is_empty() {
            blocks.push(String::new());
        }
        if let Some(b) = blocks.last_mut() {
            b.push_str(line);
            b.push('\n');
        }
    }
    blocks.retain(|b| !b.lines().next().unwrap_or("").contains("StrandErrors"));
    blocks.sort();
    blocks.concat()
}

/// What a cold boot of `texts` shows once `state` is written into it.
fn cold_boot(texts: &[String; 3], state: &State) -> (String, String) {
    let build = compile(texts).unwrap_or_else(|d| panic!("{d:#?}"));
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::real(&rt, &build.program.types));
    set_screens(&rt, &host, &[screen("A", "DP-1")]);
    let inst = Instance::from_build(&rt, &build, host, Storage::none());
    let mut m = SceneMirror::new();
    m.apply(&inst.flush().diff).unwrap();
    inst.set_value("bar", "n", Value::int(state.n)).unwrap();
    for (name, v) in &state.cells {
        inst.set_value("cells", name, v.value()).unwrap();
    }
    for (label, on) in &state.chips {
        // A fresh chip holds the default: a click flips it.
        let fresh = format!("{label} {}", !on);
        if let Some(node) = m.find_text(&fresh) {
            inst.event(node, "click", Vec::new());
        }
    }
    m.apply(&inst.flush().diff).unwrap();
    (shown(&m), m.render_tokens())
}

/// The five ways editors save a file (design.md, "Watch directories,
/// not files").
#[derive(Clone, Copy, Debug, PartialEq)]
enum Style {
    /// Truncate and write (VS Code).
    InPlace,
    /// Write a temp file and rename it over (Helix, atomic saves).
    Rename,
    /// Rename the original to a backup, write anew, drop the backup
    /// (Vim's `backupcopy=no`).
    BackupThenRename,
    /// Delete, then create.
    DeleteAndCreate,
    /// The file is a link into a store; a new target is written and the
    /// link swapped (home-manager).
    SymlinkSwap,
}

const STYLES: [Style; 5] = [
    Style::InPlace,
    Style::Rename,
    Style::BackupThenRename,
    Style::DeleteAndCreate,
    Style::SymlinkSwap,
];

/// One live pipeline, saving in one style.
struct Shell {
    style: Style,
    config: PathBuf,
    store: PathBuf,
    version: u64,
    worker: Option<Worker>,
    to_logic: calloop::channel::Sender<ToLogic>,
    thread: Option<JoinHandle<Result<(), String>>>,
    inbox: Receiver<SceneDiff>,
    scene: SceneMirror,
    events: BufReader<UnixStream>,
    /// Saves split into two loads by the watcher (held, then landed).
    splits: u64,
}

impl Shell {
    fn start(base: &Path, style: Style, texts: &[String; 3]) -> Shell {
        let root = base.join(format!("{style:?}"));
        let (config, store) = (root.join("config"), root.join("store"));
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&store).unwrap();
        for (name, text) in FILES.iter().zip(texts) {
            if style == Style::SymlinkSwap {
                let target = store.join(format!("v0-{name}"));
                std::fs::write(&target, text).unwrap();
                std::os::unix::fs::symlink(&target, config.join(name)).unwrap();
            } else {
                std::fs::write(config.join(name), text).unwrap();
            }
        }
        let socket = root.join("ipc.sock");
        let (wtx, wrx) = calloop::channel::channel();
        let (worker, boot) = Worker::spawn(&config, None, wtx).unwrap();
        assert_eq!(boot.errors(), 0, "{:?}", boot.diagnostics);
        let live = Live {
            worker: Some(wrx),
            jobs: Some(worker.jobs()),
            socket: Some(socket.clone()),
        };
        let (to_logic, from_main) = calloop::channel::channel();
        let (tx, rx) = calloop::channel::channel::<SceneDiff>();
        to_logic
            .send(ToLogic::Screens(vec![screen("A", "DP-1")]))
            .unwrap();
        let thread = std::thread::spawn(move || logic(boot, Storage::none(), from_main, tx, live));
        let deadline = Instant::now() + PATIENCE;
        let stream = loop {
            match UnixStream::connect(&socket) {
                Ok(s) => break s,
                Err(e) => {
                    assert!(Instant::now() < deadline, "{style:?}: no IPC socket: {e}");
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        };
        let mut events = BufReader::new(stream);
        let ok = ipc::request(&mut events, &ipc::Request::Watch, PATIENCE).unwrap();
        assert_eq!(ok["ok"], true, "{ok}");
        Shell {
            style,
            config,
            store,
            version: 0,
            worker: Some(worker),
            to_logic,
            thread: Some(thread),
            inbox: inbox(rx),
            scene: SceneMirror::new(),
            events,
            splits: 0,
        }
    }

    /// Save `text` as `name` the way this shell's editor does.
    fn save(&mut self, name: &str, text: &str) {
        let path = self.config.join(name);
        match self.style {
            Style::InPlace => std::fs::write(&path, text).unwrap(),
            Style::Rename => {
                let tmp = self.config.join(".fuzz-save.tmp");
                std::fs::write(&tmp, text).unwrap();
                std::fs::rename(&tmp, &path).unwrap();
            }
            Style::BackupThenRename => {
                let backup = self.config.join(format!("{name}~"));
                std::fs::rename(&path, &backup).unwrap();
                std::fs::write(&path, text).unwrap();
                std::fs::remove_file(&backup).unwrap();
            }
            Style::DeleteAndCreate => {
                std::fs::remove_file(&path).unwrap();
                std::thread::sleep(Duration::from_millis(5));
                std::fs::write(&path, text).unwrap();
            }
            Style::SymlinkSwap => {
                self.version += 1;
                let target = self.store.join(format!("v{}-{name}", self.version));
                std::fs::write(&target, text).unwrap();
                let link = self.config.join(".fuzz-link");
                let _ = std::fs::remove_file(&link);
                std::os::unix::fs::symlink(&target, &link).unwrap();
                std::fs::rename(&link, &path).unwrap();
            }
        }
    }

    /// Apply one diff: the bar stays up, and no surface shows but the bar
    /// and the error overlay.
    fn apply(&mut self, what: &str, diff: &SceneDiff) {
        let booted = !self.scene.roots().is_empty();
        self.scene
            .apply(diff)
            .unwrap_or_else(|e| panic!("{what} ({:?}): {e}", self.style));
        if !booted {
            return;
        }
        let bars = self.scene.of_kind(NodeKind::Bar).len();
        assert_eq!(
            bars,
            1,
            "{what} ({:?}): a blank frame\n{}",
            self.style,
            self.scene.render()
        );
        for &r in self.scene.roots() {
            let overlay = matches!(
                self.scene.prop(r, Prop::Name),
                Some(PropValue::Text(t)) if t == "StrandErrors"
            );
            assert!(
                self.scene.kind(r) == Some(NodeKind::Bar) || overlay,
                "{what} ({:?}): a leaked surface\n{}",
                self.style,
                self.scene.render()
            );
        }
    }

    fn pump(&mut self, what: &str) {
        while let Ok(d) = self.inbox.try_recv() {
            self.apply(what, &d);
        }
    }

    /// Apply diffs until `done` holds.
    fn until(&mut self, what: &str, done: impl Fn(&SceneMirror) -> bool) {
        self.pump(what);
        let deadline = Instant::now() + PATIENCE;
        while !done(&self.scene) {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.inbox.recv_timeout(left) {
                Ok(d) => self.apply(what, &d),
                Err(_) => panic!(
                    "{what} ({:?}): timed out on\n{}",
                    self.style,
                    self.scene.render()
                ),
            }
        }
    }

    /// The next reload event `strand watch` hears.
    fn event(&mut self, what: &str) -> Json {
        loop {
            let mut line = String::new();
            match self.events.read_line(&mut line) {
                Ok(0) => panic!("{what} ({:?}): the shell hung up", self.style),
                Ok(_) => {}
                Err(e) => panic!("{what} ({:?}): no reload event: {e}", self.style),
            }
            let ev: Json = serde_json::from_str(&line).unwrap();
            if ev["event"] == "reload" {
                return ev;
            }
        }
    }

    /// Events already sent (a save the watcher split in two leaves the
    /// second one behind).
    fn drain(&mut self) {
        let stream = self.events.get_ref();
        stream.set_nonblocking(true).unwrap();
        let mut line = String::new();
        loop {
            match self.events.read_line(&mut line) {
                Ok(n) if n > 0 && line.ends_with('\n') => line.clear(),
                _ => break,
            }
        }
        self.events.get_ref().set_nonblocking(false).unwrap();
        if !line.is_empty() {
            // Half a line: the rest is on its way.
            let mut rest = String::new();
            let _ = self.events.read_line(&mut rest);
        }
    }

    /// The save was held back: the attempt holds a file, nothing changed.
    fn held(&mut self, what: &str, before: &str) {
        let ev = self.event(what);
        assert!(
            ev["held"].as_array().is_some_and(|h| !h.is_empty())
                || ev["unreadable"].as_array().is_some_and(|u| !u.is_empty()),
            "{what} ({:?}): not held back: {ev}",
            self.style
        );
        assert!(
            ev["committed"].as_array().is_some_and(|c| c.is_empty()),
            "{what} ({:?}): {ev}",
            self.style
        );
        self.pump(what);
        assert_eq!(
            shown(&self.scene),
            before,
            "{what} ({:?}): a held-back save changed the scene",
            self.style
        );
    }

    /// The save landed: events until one holds nothing back; the cells
    /// they reset.
    fn landed(&mut self, what: &str) -> (usize, bool) {
        let mut resets = 0;
        let mut split = false;
        loop {
            let ev = self.event(what);
            resets += ev["reset"].as_array().map_or(0, Vec::len);
            let held = ev["held"].as_array().is_some_and(|h| !h.is_empty())
                || ev["unreadable"].as_array().is_some_and(|u| !u.is_empty());
            if !held {
                return (resets, split);
            }
            split = true;
            self.splits += 1;
        }
    }

    fn click(&mut self, what: &str, text: &str) {
        self.pump(what);
        let node = self
            .scene
            .find_text(text)
            .unwrap_or_else(|| panic!("{what} ({:?}): no `{text}`", self.style));
        self.to_logic
            .send(ToLogic::Event {
                node,
                event: NodeEvent::Click,
            })
            .unwrap();
    }

    fn stop(mut self) {
        self.to_logic.send(ToLogic::Shutdown).unwrap();
        let joined = self.thread.take().map(|t| t.join());
        assert!(
            matches!(joined, Some(Ok(Ok(())))),
            "{:?}: the logic thread: {joined:?}",
            self.style
        );
        drop(self.worker.take());
    }
}

fn base_dir() -> PathBuf {
    let root = std::env::var_os("STRAND_FUZZ_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            let shm = PathBuf::from("/dev/shm");
            shm.is_dir().then_some(shm)
        })
        .unwrap_or_else(std::env::temp_dir);
    root.join(format!("strand-fuzz-{}", std::process::id()))
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
}

/// design.md, "How reload is tested", through the whole live pipeline.
#[test]
fn random_edits_through_five_save_styles() {
    let edits = env_u64("STRAND_FUZZ_EDITS").unwrap_or(60);
    let seed = env_u64("STRAND_FUZZ_SEED").unwrap_or(0x5eed_f00d_cafe_0001);
    eprintln!("reload fuzzer: {edits} edits, seed {seed:#x}");
    let mut r = Rng(seed.max(1));
    let base = base_dir();
    let _ = std::fs::remove_dir_all(&base);
    let mut model = Model::first();
    let mut texts = model.texts();
    let mut state = State::defaults(&model);
    let mut shells: Vec<Shell> = STYLES
        .iter()
        .map(|&s| Shell::start(&base, s, &texts))
        .collect();
    let (mut expect, mut tokens) = cold_boot(&texts, &state);
    for sh in &mut shells {
        let (e, t) = (expect.clone(), tokens.clone());
        sh.until("the boot", |s| shown(s) == e && s.render_tokens() == t);
    }
    let started = Instant::now();
    let mut counts: BTreeMap<&str, u64> = BTreeMap::new();
    // A file saved broken, on disk until a later save fixes it.
    let mut broken: Option<usize> = None;
    // Saves made: drawn edits that change nothing are drawn again.
    let mut step = 0;
    let mut drawn = 0u64;
    while step < edits {
        drawn += 1;
        // The user changes some state.
        if broken.is_none() {
            let clicks = r.below(3);
            for _ in 0..clicks {
                let mut targets = vec!["n".to_string()];
                targets.extend(state.cells.keys().map(|k| k.to_string()));
                targets.extend(state.chips.keys().map(|k| k.to_string()));
                let t = r.pick(&targets).clone();
                let shown_now = state
                    .texts()
                    .into_iter()
                    .find(|x| x.split(' ').next() == Some(t.as_str()))
                    .unwrap();
                if t == "n" {
                    state.n += 1;
                } else if let Some(v) = state.cells.get_mut(t.as_str()) {
                    match v {
                        V::Int(n) => *n += 1,
                        V::Text(s) => s.push('x'),
                    }
                } else if let Some(on) = state.chips.get_mut(t.as_str()) {
                    *on = !*on;
                }
                let want = state.texts();
                for sh in &mut shells {
                    sh.click(&format!("step {step}: click"), &shown_now);
                    sh.until(&format!("step {step}: click {t}"), |s| {
                        let have = s.texts();
                        want.iter().all(|w| have.contains(w))
                    });
                }
            }
            if clicks > 0 {
                (expect, tokens) = cold_boot(&texts, &state);
                for sh in &mut shells {
                    sh.until(&format!("step {step}: clicked"), |s| {
                        shown(s) == expect && s.render_tokens() == tokens
                    });
                }
            }
        }
        let at = step;
        let what = |kind: &str| format!("step {at} ({kind})");
        match edit(&model, &mut r) {
            Edit::Broken(f, text) if broken.is_none() => {
                if compile(&{
                    let mut t = texts.clone();
                    t[f] = text.clone();
                    t
                })
                .is_ok()
                {
                    continue;
                }
                *counts.entry("broken").or_default() += 1;
                step += 1;
                let w = what("broken");
                for sh in &mut shells {
                    sh.save(FILES[f], &text);
                }
                for sh in &mut shells {
                    sh.held(&w, &expect);
                }
                broken = Some(f);
            }
            Edit::Broken(..) => {
                // The fix: the broken file back to its last good text.
                let f = broken.take().unwrap_or(0);
                *counts.entry("fixed").or_default() += 1;
                step += 1;
                let w = what("fix");
                for sh in &mut shells {
                    sh.save(FILES[f], &texts[f]);
                }
                for sh in &mut shells {
                    sh.landed(&w);
                    sh.until(&w, |s| shown(s) == expect);
                    sh.drain();
                }
            }
            Edit::To(next, kind) => {
                let next_texts = next.texts();
                compile(&next_texts).unwrap_or_else(|d| panic!("{kind}: {d:#?}"));
                let mut changed: Vec<usize> =
                    (0..3).filter(|&i| next_texts[i] != texts[i]).collect();
                if let Some(f) = broken.take()
                    && !changed.contains(&f)
                {
                    changed.push(f);
                }
                if changed.is_empty() {
                    continue;
                }
                *counts.entry(kind).or_default() += 1;
                step += 1;
                let w = what(kind);
                // A partial multi-file save: one file alone (one that
                // does not type-check with the others' old text) first,
                // held back; then the rest lands with it.
                if changed.len() > 1 && r.below(2) == 0 {
                    let first = changed.iter().position(|&f| {
                        let mut t = texts.clone();
                        t[f] = next_texts[f].clone();
                        compile(&t).is_err()
                    });
                    if let Some(k) = first {
                        let f = changed.remove(k);
                        *counts.entry("partial").or_default() += 1;
                        let w = format!("{w}, partial {}", FILES[f]);
                        for sh in &mut shells {
                            sh.save(FILES[f], &next_texts[f]);
                        }
                        for sh in &mut shells {
                            sh.held(&w, &expect);
                        }
                    }
                }
                for sh in &mut shells {
                    for &f in &changed {
                        sh.save(FILES[f], &next_texts[f]);
                    }
                }
                let (next_state, resets) = table(&model, &next, &state);
                (expect, tokens) = cold_boot(&next_texts, &next_state);
                for sh in &mut shells {
                    let (reported, split) = sh.landed(&w);
                    if !split {
                        assert_eq!(
                            reported, resets,
                            "{w} ({:?}): `strand watch` reset {reported} cells, the table {resets}",
                            sh.style
                        );
                    }
                    let (e, t) = (&expect, &tokens);
                    sh.until(&w, |s| shown(s) == *e && s.render_tokens() == *t);
                    sh.drain();
                }
                model = next;
                texts = next_texts;
                state = next_state;
            }
        }
    }
    let elapsed = started.elapsed();
    let splits: Vec<u64> = shells.iter().map(|s| s.splits).collect();
    for sh in shells {
        sh.stop();
    }
    eprintln!(
        "reload fuzzer: {edits} edits ({drawn} drawn) through 5 save styles in {:.1} s: {counts:?}; split saves per style {splits:?}",
        elapsed.as_secs_f64()
    );
    let _ = std::fs::remove_dir_all(&base);
}
