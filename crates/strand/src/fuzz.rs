//! The reload fuzzer (design.md, "How reload is tested"; the M1 exit
//! gate "10k random edits with no panic or blank frame").
//!
//! Random edits of a four-file config are replayed through the five
//! editor save styles: one live `strand run` pipeline per style (the real
//! watcher, compiler worker, logic thread and IPC socket), each on its own
//! copy of the config on tmpfs, with two screens plugged in, every edit
//! saved into all five. A sixth pipeline saves in place into the whole
//! of `strand run` on a headless sway with two outputs: the surface
//! manager, the renderer and the text worker, so its layer surfaces and
//! committed buffers are checked (skipped when sway is not installed,
//! unless `STRAND_REQUIRE_SWAY` is set, as in CI).
//!
//! The edits cover each row of design.md's "What each edit does" that M1
//! runs: token values; props and bindings; nodes added, removed and
//! moved; keyed list entries; `state` defaults; `state` names and types;
//! handler code; a timer's duration; the surface's layer, namespace and
//! kind (`bar` on every screen ↔ one `panel`), with a second surface (an
//! `osd` in a file of its own) that must be kept. Renames go across
//! files: a cell, a token path, the component's parameter and the module
//! (its file renamed). A component moves between files. Syntax errors
//! come from templates and from random token deletions and duplications;
//! a random mutation that still compiles and only touches an expression
//! is run as an edit and saved back. Multi-file edits are sometimes
//! saved one file first (a partial save). Between edits, state is
//! changed by clicks, as a user would, also while a broken save is held
//! back (the shell keeps running its last good config).
//!
//! What every pipeline must do:
//!
//! - After every diff the scene is the one before the step or the one
//!   after it (a commit is atomic: no intermediate frame; a mutation's
//!   own frame is not modelled), every surface of the shell is up with
//!   its fixed texts, the surfaces' scene ids are the ones before (an
//!   edit of the main surface's layer, namespace or kind replaces that
//!   surface, in one diff, and keeps the note), and at most one error
//!   overlay is shown.
//! - The overlay never opens unless a reload left notices (a reset, a
//!   kept value) or a load was held back for 250 ms with no save ending
//!   the hold (once the next load came, judged on the logic thread's
//!   timeline from the events' timing: a stall on that load lengthens
//!   the hold there); once a commit lands clean it lists no errors.
//! - A broken or partial save is held back (`held`, never `unreadable`)
//!   and changes nothing; a single file's save is one load (never split,
//!   delete-then-create included, with a 0–25 ms gap: 0–20 in CI's
//!   per-push run; a split after the test thread itself was descheduled
//!   past the watcher's 50 ms grace fails saying so).
//! - After each step the scene and token table equal a cold boot of the
//!   same files with the state the edit table keeps written into it, and
//!   the resets `strand watch` reports are the table's.
//! - One pipeline's diffs also go through an offline `Renderer` (vello_cpu,
//!   damage tracked over two buffers per surface with their buffer age,
//!   text shaped inline): after every diff each surface paints something
//!   besides its background, one surface per scene surface, and after
//!   each step its pixels equal a fresh renderer's painting of the cold
//!   boot.
//! - The sway pipeline has one layer surface per scene surface (the
//!   overlay's included) whenever it has applied what the logic thread
//!   sent, never commits a buffer that shows only its background, and
//!   after each step every surface is configured and its last committed
//!   buffer (the screen) equals a fresh offline renderer's painting of
//!   the cold boot at that surface's configured size and scale (the
//!   overlay aside): a surface that stops repainting fails here.
//! - Every thread (logic, compiler worker, watcher) ends without a panic,
//!   and the sway pipeline's text worker is still running at the end.
//!
//! `STRAND_FUZZ_EDITS` sets the number of edits (60), `STRAND_FUZZ_SEED`
//! the seed (decimal or `0x` hex, as printed), `STRAND_FUZZ_MAX_GAP_MS`
//! the longest delete-to-create gap (25); the directory is `/dev/shm`
//! when it is there (tmpfs), `STRAND_FUZZ_DIR` overrides it. The
//! 10,000-edit run is the nightly CI job (`docs/m1-report.md`).

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
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
use strand_render::{Renderer, TextBackend};
use strand_scene::{
    NodeId, NodeKind, PaintTarget, Painter, Prop, PropValue, Scale, SceneDiff, Size, SurfaceChange,
    SurfaceId, SurfaceSpec,
};
use strand_surface::{Config, SurfaceManager};
use strand_text::{FontConfig, TextEngine, TextWorker};

use crate::bench::Sway;
use crate::demo::host::{Host, Probe, ProbeHandle};
use crate::ipc;
use crate::live::Worker;
use crate::run::tests::{inbox, screen};
use crate::run::{Live, NodeEvent, ScreenInfo, ToLogic, logic, set_screens};

/// How long one pipeline may take to answer a save or show a scene.
const PATIENCE: Duration = Duration::from_secs(20);
/// How long a held-back save is watched for a diff it must not cause:
/// the diff and the reload event leave the logic thread in the same
/// step, but the diff takes one more hop to the test.
const QUIET: Duration = Duration::from_millis(50);
/// The longest pause between a delete and its create: under the
/// watcher's 50 ms grace after a removal, over its 15 ms coalescing
/// (`STRAND_FUZZ_MAX_GAP_MS` overrides it: CI's per-push run takes 20).
/// The pause is spun, not slept, so a busy machine's wake-up latency is
/// not added to it.
const MAX_GAP_MS: u64 = 25;
/// A delete and its create further apart than this may be two saves to
/// the watcher (its grace after a removal). When the watcher did take
/// them as two, the step cannot be checked as one save: the run fails
/// naming a descheduled test thread, not a reload fault.
const GRACE: Duration = Duration::from_millis(50);

/// The screens plugged in (ids; a bar's `screens` prop is the id), named
/// as the sway pipeline's outputs so its bars land on them.
const SCREENS: [&str; 2] = ["HEADLESS-1", "HEADLESS-2"];
/// A second surface in a file of its own that no edit touches: an edit
/// of the main surface recreates only that one.
const NOTE_FILE: &str = "note.strand";
const NOTE_TEXT: &str = "note";
/// A hold the overlay may open on: its 250 ms of quiet, less the
/// difference between the test's clock and the logic thread's.
const HELD_LONG: Duration = Duration::from_millis(200);
const CELL_NAMES: [&str; 8] = ["a", "b", "c", "d", "e", "f", "g", "h"];
const CHIP_LABELS: [&str; 4] = ["x", "y", "z", "w"];
const LIST_LABELS: [&str; 4] = ["p", "q", "r", "s"];
/// The exported cells' module (its file's stem), renamed.
const MODULES: [&str; 2] = ["cells", "store"];
/// The bar colour's token path, renamed.
const BG_TOKENS: [&str; 2] = ["bar.bg", "bar.fill"];
/// `Chip`'s parameter, renamed.
const PARAMS: [&str; 2] = ["label", "name"];
/// The surface's name (its namespace `strand-<Name>`).
const SURFACE_NAMES: [&str; 2] = ["Top", "Main"];
/// The overlay's surface name.
const OVERLAY: &str = "StrandErrors";

fn screens() -> Vec<ScreenInfo> {
    SCREENS.iter().map(|s| screen(s, s)).collect()
}

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

fn other<T: PartialEq + Copy>(pair: &[T; 2], now: T) -> T {
    if pair[0] == now { pair[1] } else { pair[0] }
}

/// One `export state` of the cells' module.
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

/// A child of the surface's row after the fixed texts.
#[derive(Clone, Debug, PartialEq)]
enum Item {
    /// `Chip { label: "x" }`, or `row { Chip { label: "x" } }` when
    /// wrapped.
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
    /// Handler code: the `n` text's click runs `n += n_step`.
    n_step: i64,
    /// A timer's duration: `every <timer>000s { n += 1 }` in the surface
    /// (it never fires during a run).
    timer: u32,
    cells: Vec<Cell>,
    chip_default: bool,
    /// `component Chip` lives in `bar.strand` (else `theme.strand`).
    chip_in_bar: bool,
    items: Vec<Item>,
    next_static: u32,
    /// `export let list` in `theme.strand`: a `Chip` per entry, keyed
    /// by it (list-item state).
    list: Vec<&'static str>,
    /// One `panel` instead of a `bar` on every screen.
    panel: bool,
    /// The surface's name (namespace).
    name: &'static str,
    /// `layer: bottom` (else `top`).
    bottom: bool,
    bg_token: &'static str,
    param: &'static str,
    module: &'static str,
}

impl Model {
    fn first() -> Self {
        Model {
            bg: 0x204080,
            fg: 0xffeedd,
            tag: 1,
            height: 32,
            n_default: 0,
            n_step: 1,
            timer: 1,
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
            panel: false,
            name: SURFACE_NAMES[0],
            bottom: false,
            bg_token: BG_TOKENS[0],
            param: PARAMS[0],
            module: MODULES[0],
        }
    }

    fn chip_decl(&self) -> String {
        format!(
            "component Chip({0}: text) {{\n  state on = {1}\n  text join(\" \", {0}, on) {{ color: $chip.fg; on click {{ on = !on }} }}\n}}\n",
            self.param, self.chip_default
        )
    }

    /// Every chip's label: the surface's own, then the list's.
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

    /// The surface instances: one per screen for a bar.
    fn instances(&self) -> Vec<Option<usize>> {
        if self.panel {
            vec![None]
        } else {
            (0..SCREENS.len()).map(Some).collect()
        }
    }

    fn module_file(&self) -> String {
        format!("{}.strand", self.module)
    }

    /// The files and their texts.
    fn files(&self) -> BTreeMap<String, String> {
        let mut theme = format!(
            "tokens base {{ {}: #{:06x}; chip.fg: #{:06x} }}\nexport let tag = \"t{}\"\nexport let list: [text] = [{}]\n",
            self.bg_token,
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
            "state n = {}\n{} {} {{\n  {}: top; {}height: {}\n  layer: {}\n  bg: ${}\n  every {}000s {{ n += 1 }}\n  row {{\n    text join(\" \", \"n\", n) {{ on click {{ n += {} }} }}\n",
            self.n_default,
            if self.panel { "panel" } else { "bar" },
            self.name,
            // A bar takes an edge, a panel an anchor and a width.
            if self.panel { "anchor" } else { "edge" },
            if self.panel { "width: 600; " } else { "" },
            self.height,
            if self.bottom { "bottom" } else { "top" },
            self.bg_token,
            self.timer,
            self.n_step,
        );
        let m = self.module;
        for c in &self.cells {
            let handler = match c.text {
                true => format!("{m}.{0} = join(\"\", {m}.{0}, \"x\")", c.name),
                false => format!("{m}.{} += 1", c.name),
            };
            bar.push_str(&format!(
                "    text join(\" \", \"{0}\", {m}.{0}) {{ on click {{ {1} }} }}\n",
                c.name, handler
            ));
        }
        bar.push_str("    text theme.tag\n");
        for i in &self.items {
            match i {
                Item::Chip {
                    label,
                    wrapped: false,
                } => bar.push_str(&format!("    Chip {{ {}: \"{label}\" }}\n", self.param)),
                Item::Chip {
                    label,
                    wrapped: true,
                } => bar.push_str(&format!(
                    "    row {{ Chip {{ {}: \"{label}\" }} }}\n",
                    self.param
                )),
                Item::Static(k) => bar.push_str(&format!("    text \"static{k}\"\n")),
            }
        }
        bar.push_str("    for l in theme.list key l { Chip l }\n  }\n}\n");
        if self.chip_in_bar {
            bar.push_str(&self.chip_decl());
        } else {
            theme.push_str(&self.chip_decl());
        }
        BTreeMap::from([
            ("theme.strand".to_string(), theme),
            (self.module_file(), cells),
            ("bar.strand".to_string(), bar),
            (
                NOTE_FILE.to_string(),
                format!(
                    "osd Note {{\n  anchor: bottom; width: 200; height: 24\n  text \"{NOTE_TEXT}\"\n}}\n"
                ),
            ),
        ])
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

/// A chip's cell: its surface instance (a screen of the bar, or the one
/// panel) and its label.
type ChipKey = (Option<usize>, &'static str);

/// The state the running shells must hold.
#[derive(Clone, Debug, PartialEq)]
struct State {
    n: i64,
    cells: BTreeMap<&'static str, V>,
    chips: BTreeMap<ChipKey, bool>,
}

impl State {
    fn defaults(m: &Model) -> Self {
        let mut chips = BTreeMap::new();
        for i in m.instances() {
            for l in m.chips() {
                chips.insert((i, l), m.chip_default);
            }
        }
        State {
            n: m.n_default,
            cells: m
                .cells
                .iter()
                .map(|c| (c.name, c.default_value()))
                .collect(),
            chips,
        }
    }
}

/// design.md's "State kept?" column: a cell takes a new default only if
/// it still holds the old one; renamed or retyped (or its module
/// renamed), it resets; a node added starts fresh. A surface turned from a
/// bar on two screens into one panel, or back, resets its cells
/// (decisions.md, wave2-runtime: whose would it keep?). Returns the
/// state and the cells reset.
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
    let mut resets = 0;
    let moved = old.module != new.module;
    for c in &new.cells {
        let v = match old
            .cells
            .iter()
            .find(|o| !moved && o.name == c.name && o.text == c.text)
        {
            Some(o) => adopt(&o.default_value(), &c.default_value(), &s.cells[o.name]),
            None => c.default_value(),
        };
        next.cells.insert(c.name, v);
    }
    // A module renamed (its file) in one load: each cell renamed, reset
    // with a warning. (Renamed in two loads, see the partial save in the
    // test.)
    resets += old
        .cells
        .iter()
        .filter(|o| {
            moved
                || !new
                    .cells
                    .iter()
                    .any(|c| c.name == o.name && c.text == o.text)
        })
        .count();
    let kind_changed = old.panel != new.panel;
    if kind_changed {
        resets += s.chips.len();
    }
    for i in new.instances() {
        for l in new.chips() {
            let v = match s.chips.get(&(i, l)).filter(|_| !kind_changed) {
                Some(v) => adopt(&old.chip_default, &new.chip_default, v),
                None => new.chip_default,
            };
            next.chips.insert((i, l), v);
        }
    }
    (next, resets)
}

/// One random edit.
enum Edit {
    /// A valid config.
    To(Model, &'static str),
    /// One file saved with a syntax or name error, or (`Some`: the word
    /// deleted or duplicated) a random mutation, which may still compile.
    Broken(String, String, Option<String>),
    /// A random mutation that compiles, searched for.
    Mutate,
}

/// The edits whose surfaces are replaced (design.md: "Only that surface
/// is recreated").
fn recreates(kind: &str) -> bool {
    kind.starts_with("surface-")
}

fn edit(m: &Model, r: &mut Rng) -> Edit {
    let mut n = m.clone();
    let kind = match r.below(25) {
        24 => return Edit::Mutate,
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
        14 => {
            n.n_step = 1 + r.below(4) as i64;
            "handler"
        }
        15 => {
            n.timer = 1 + r.below(9) as u32;
            "timer"
        }
        16 => {
            n.bottom = !n.bottom;
            "surface-layer"
        }
        17 => {
            n.name = other(&SURFACE_NAMES, n.name);
            "surface-name"
        }
        18 => {
            n.panel = !n.panel;
            "surface-kind"
        }
        19 => {
            n.bg_token = other(&BG_TOKENS, n.bg_token);
            "rename-token"
        }
        20 => {
            n.param = other(&PARAMS, n.param);
            "rename-param"
        }
        21 => {
            n.module = other(&MODULES, n.module);
            "rename-module"
        }
        _ => {
            let files = m.files();
            let names: Vec<&String> = files.keys().collect();
            let f = (*r.pick(&names)).clone();
            let mut text = files[&f].clone();
            match r.below(6) {
                0 => text.push_str("export state = \n"),
                1 => {
                    // An unclosed block.
                    if let Some(k) = text.rfind('}') {
                        text.replace_range(k..k + 1, "");
                    }
                }
                2 if f == "bar.strand" => {
                    text = text.replacen("  row {\n", "  row {\n    txet \"oops\"\n", 1);
                }
                3 | 4 => {
                    let (text, w) = mutate(&text, r);
                    return Edit::Broken(f, text, Some(w));
                }
                _ => text.push_str(&format!("let broken = {}.nope + 1\n", m.module)),
            }
            return Edit::Broken(f, text, None);
        }
    };
    Edit::To(n, kind)
}

/// A random word of `text` deleted, or duplicated: the text and the word.
fn mutate(text: &str, r: &mut Rng) -> (String, String) {
    let mut text = text.to_string();
    let words: Vec<(usize, usize)> = words(&text);
    let &(a, b) = r.pick(&words);
    let w = text[a..b].to_string();
    if r.below(2) == 0 {
        text.replace_range(a..b, "");
    } else {
        text.insert_str(b, &format!(" {w}"));
    }
    (text, w)
}

/// A random mutation of `files` that compiles and that the fuzzer runs
/// ([`runnable_mutation`]): the file, its text and the word (most
/// mutations do not compile; up to 100 are drawn).
fn compiling_mutation(
    files: &BTreeMap<String, String>,
    r: &mut Rng,
) -> Option<(String, String, String)> {
    let names: Vec<&String> = files.keys().collect();
    (0..100).find_map(|_| {
        let f = (*r.pick(&names)).clone();
        let (text, w) = mutate(&files[&f], r);
        let mut t = files.clone();
        t.insert(f.clone(), text.clone());
        (runnable_mutation(&f, &w, &files[&f], &text) && compile(&t).is_ok())
            .then_some((f, text, w))
    })
}

/// Byte ranges of the whitespace-separated words of `text`.
fn words(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices() {
        match (c.is_whitespace(), start) {
            (true, Some(s)) => {
                out.push((s, i));
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push((s, text.len()));
    }
    out
}

fn compile(files: &BTreeMap<String, String>) -> Result<Build, Vec<strand_compiler::Diagnostic>> {
    let mut map = SourceMap::new();
    for (name, text) in files {
        map.add(name.as_str(), text.clone());
    }
    Build::compile(None, map)
}

/// What a shell looks like: the scene without the error overlay
/// (surfaces in a fixed order) and the token table.
#[derive(Clone, Debug, PartialEq)]
struct Look {
    scene: String,
    tokens: String,
}

fn is_overlay(scene: &SceneMirror, root: NodeId) -> bool {
    matches!(
        scene.prop(root, Prop::Name),
        Some(PropValue::Text(t)) if t.as_str() == OVERLAY
    )
}

fn look(scene: &SceneMirror) -> Look {
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
    blocks.retain(|b| !b.lines().next().unwrap_or("").contains(OVERLAY));
    blocks.sort();
    Look {
        scene: blocks.concat(),
        tokens: scene.render_tokens(),
    }
}

/// The main surface (the bar on each screen, or the panel), not the
/// note.
fn is_main(scene: &SceneMirror, root: NodeId) -> bool {
    matches!(scene.kind(root), Some(NodeKind::Bar | NodeKind::Panel))
}

/// The shell's surfaces (every root but the overlay).
fn surfaces(scene: &SceneMirror) -> BTreeSet<NodeId> {
    scene
        .roots()
        .iter()
        .copied()
        .filter(|&r| !is_overlay(scene, r))
        .collect()
}

fn overlays(scene: &SceneMirror) -> Vec<NodeId> {
    scene
        .roots()
        .iter()
        .copied()
        .filter(|&r| is_overlay(scene, r))
        .collect()
}

/// The surface of instance `i`: the bar on screen `i`, or the panel.
fn surface_of(scene: &SceneMirror, i: Option<usize>) -> Option<NodeId> {
    surfaces(scene).into_iter().find(|&r| match i {
        Some(i) => matches!(
            scene.prop(r, Prop::Screens),
            Some(PropValue::Text(t)) if t.as_str() == SCREENS[i]
        ),
        None => scene.kind(r) == Some(NodeKind::Panel),
    })
}

fn root_of(scene: &SceneMirror, mut id: NodeId) -> NodeId {
    while let Some(p) = scene.parent(id) {
        id = p;
    }
    id
}

/// The text nodes under `root`.
fn texts_under(scene: &SceneMirror, root: NodeId) -> Vec<(NodeId, String)> {
    scene
        .walk()
        .into_iter()
        .filter_map(|n| match scene.prop(n, Prop::Text) {
            Some(PropValue::Text(t)) if root_of(scene, n) == root => Some((n, t.clone())),
            _ => None,
        })
        .collect()
}

fn find_under(scene: &SceneMirror, root: NodeId, text: &str) -> Option<NodeId> {
    texts_under(scene, root)
        .into_iter()
        .find(|(_, t)| t == text)
        .map(|(n, _)| n)
}

/// A cold boot of `files` with `state` written into it: how it looks,
/// and the diffs that made it.
fn cold_boot(m: &Model, files: &BTreeMap<String, String>, state: &State) -> (Look, Vec<SceneDiff>) {
    let build = compile(files).unwrap_or_else(|d| panic!("{d:#?}"));
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::real(&rt, &build.program.types));
    set_screens(&rt, &host, &screens());
    let inst = Instance::from_build(&rt, &build, host, Storage::none());
    let mut scene = SceneMirror::new();
    let mut diffs = vec![inst.flush().diff];
    scene.apply(&diffs[0]).unwrap();
    inst.set_value("bar", "n", Value::int(state.n)).unwrap();
    for (name, v) in &state.cells {
        inst.set_value(m.module, name, v.value()).unwrap();
    }
    for (&(i, label), &on) in &state.chips {
        if on == m.chip_default {
            continue;
        }
        // A fresh chip holds the default: a click flips it.
        let fresh = format!("{label} {}", m.chip_default);
        let node = surface_of(&scene, i)
            .and_then(|root| find_under(&scene, root, &fresh))
            .unwrap_or_else(|| {
                panic!(
                    "cold boot: no `{fresh}` on surface {i:?}\n{}",
                    scene.render()
                )
            });
        inst.event(node, "click", Vec::new());
    }
    diffs.push(inst.flush().diff);
    scene.apply(&diffs[1]).unwrap();
    (look(&scene), diffs)
}

/// Buffers per offline surface: a double-buffered compositor surface.
const BUFFERS: usize = 2;

/// One surface painted offline, as a compositor would show it.
struct Frame {
    surface: SurfaceId,
    size: Size,
    /// Its buffers and the frame each was last painted in.
    buffers: Vec<(Vec<u8>, Option<u64>)>,
    /// Frames painted; the shown buffer is the one painted last.
    frames: u64,
    shown: usize,
}

/// An offline renderer fed one pipeline's diffs, a surface per
/// surface-kind node (the scene-level stand-in for `strand-surface`'s;
/// the overlay aside), painted into two buffers in turn with their
/// buffer age (2 once both are painted), so damage tracking and its
/// history decide what is repainted. Text is shaped inline (the text
/// worker's hold is `damage.rs`'s and the sway pipeline's).
struct Pixels {
    r: Renderer,
    next: u32,
    frames: BTreeMap<NodeId, Frame>,
}

impl Pixels {
    fn new() -> Self {
        let font = std::fs::read(strand_text::test_font_path()).unwrap();
        let engine = TextEngine::new(FontConfig::isolated(vec![Arc::new(font)]));
        Pixels {
            r: Renderer::new(TextBackend::Inline(Box::new(engine))),
            next: 1,
            frames: BTreeMap::new(),
        }
    }

    fn size(spec: &SurfaceSpec) -> Size {
        Size::new(
            spec.width.unwrap_or(480.0).round() as u32,
            spec.height.unwrap_or(32.0).round() as u32,
        )
    }

    fn attach(&mut self, node: NodeId, spec: &SurfaceSpec) {
        if spec.name.as_deref() == Some(OVERLAY) {
            return;
        }
        let surface = SurfaceId(self.next);
        self.next += 1;
        let size = Self::size(spec);
        self.r.attach_surface(surface, node);
        self.r.configure_surface(surface, size, Scale::ONE);
        self.frames.insert(
            node,
            Frame {
                surface,
                size,
                buffers: vec![(vec![0; (size.w * size.h * 4) as usize], None); BUFFERS],
                frames: 0,
                shown: 0,
            },
        );
    }

    fn detach(&mut self, node: NodeId) {
        if let Some(f) = self.frames.remove(&node) {
            self.r.detach_surface(f.surface);
        }
    }

    fn apply(&mut self, diff: SceneDiff) {
        let errors = self.r.apply(diff);
        assert!(errors.is_empty(), "the renderer refused a diff: {errors:?}");
        for (node, change) in self.r.take_surface_changes() {
            match change {
                SurfaceChange::Created(spec) => self.attach(node, &spec),
                SurfaceChange::Updated { spec, recreate } => {
                    let resized = self
                        .frames
                        .get(&node)
                        .is_some_and(|f| f.size != Self::size(&spec));
                    if recreate || resized {
                        self.detach(node);
                        self.attach(node, &spec);
                    }
                }
                SurfaceChange::Removed => self.detach(node),
            }
        }
    }

    /// Paints every surface that wants a frame; returns its frames keyed
    /// by surface spec (kind and screens), so two renderers' match.
    fn paint(&mut self) -> BTreeMap<String, (NodeId, Size, Vec<u8>)> {
        let mut out = BTreeMap::new();
        for (node, f) in &mut self.frames {
            if self.r.wants_frame(f.surface) || f.frames == 0 {
                let next = (f.shown + 1) % BUFFERS;
                let (px, at) = &mut f.buffers[next];
                // Frames since this buffer was painted (0: never).
                let age = at.map_or(0, |k| (f.frames - k).min(u8::MAX as u64) as u8);
                let mut target =
                    PaintTarget::new(px, f.size, f.size.w * 4, Scale::ONE, age).unwrap();
                self.r.paint(f.surface, &mut target);
                *at = Some(f.frames);
                f.frames += 1;
                f.shown = next;
            }
            let key = spec_key(self.r.surface_spec(*node), *node);
            out.insert(key, (*node, f.size, f.buffers[f.shown].0.clone()));
        }
        out
    }
}

/// Where two frames differ by more than `tolerance` in a channel.
fn mismatch(a: &[u8], b: &[u8], tolerance: u8) -> usize {
    a.chunks_exact(4)
        .zip(b.chunks_exact(4))
        .filter(|(x, y)| {
            x.iter()
                .zip(y.iter())
                .any(|(p, q)| p.abs_diff(*q) > tolerance)
        })
        .count()
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

/// One change of the config directory.
#[derive(Clone, Debug)]
enum Op {
    /// A file saved (or created).
    Write(String, String),
    Remove(String),
    /// A file renamed (`mv`), its text unchanged.
    Rename(String, String),
}

/// The changes that take `disk` to `next`.
fn plan(disk: &BTreeMap<String, String>, next: &BTreeMap<String, String>) -> Vec<Op> {
    let mut ops = Vec::new();
    let mut added: Vec<&String> = next.keys().filter(|k| !disk.contains_key(*k)).collect();
    for (name, text) in disk {
        if next.contains_key(name) {
            continue;
        }
        match added.iter().position(|a| next[*a] == *text) {
            Some(k) => ops.push(Op::Rename(name.clone(), added.remove(k).clone())),
            None => ops.push(Op::Remove(name.clone())),
        }
    }
    for (name, text) in next {
        if disk.get(name) != Some(text)
            && !ops
                .iter()
                .any(|o| matches!(o, Op::Rename(_, to) if to == name))
        {
            ops.push(Op::Write(name.clone(), text.clone()));
        }
    }
    ops
}

fn after(disk: &BTreeMap<String, String>, op: &Op) -> BTreeMap<String, String> {
    let mut d = disk.clone();
    match op {
        Op::Write(n, t) => {
            d.insert(n.clone(), t.clone());
        }
        Op::Remove(n) => {
            d.remove(n);
        }
        Op::Rename(a, b) => {
            if let Some(t) = d.remove(a) {
                d.insert(b.clone(), t);
            }
        }
    }
    d
}

/// A surface's last committed buffer on the sway pipeline.
struct Committed {
    size: Size,
    scale: Scale,
    /// Rows of `size.w * 4` bytes (the stride dropped).
    pixels: Vec<u8>,
    /// Frames it committed.
    frames: u64,
}

/// What the sway pipeline's surfaces committed, seen from the painter.
#[derive(Default)]
struct Seen {
    /// Surfaces with a committed frame.
    painted: RefCell<BTreeSet<SurfaceId>>,
    /// Committed frames that showed only their background.
    blank: RefCell<Vec<SurfaceId>>,
    /// Each surface's last committed buffer.
    last: RefCell<BTreeMap<SurfaceId, Committed>>,
}

impl Probe for Seen {
    fn painted(&self, _: SurfaceId, _: bool, _: Scale, _: &Renderer) {}

    fn configured(&self, _: SurfaceId) {}

    fn monitor(&self) {}

    fn frame(&self, surface: SurfaceId, target: &PaintTarget<'_>) {
        self.painted.borrow_mut().insert(surface);
        let row = (target.size.w * 4) as usize;
        let first = target.pixels.get(..4).unwrap_or(&[]).to_vec();
        let drawn = target
            .pixels
            .chunks(target.stride as usize)
            .take(target.size.h as usize)
            .any(|r| r[..row.min(r.len())].chunks_exact(4).any(|c| c != first));
        if !drawn {
            self.blank.borrow_mut().push(surface);
        }
        let mut pixels = Vec::with_capacity(row * target.size.h as usize);
        for r in target
            .pixels
            .chunks(target.stride as usize)
            .take(target.size.h as usize)
        {
            pixels.extend_from_slice(&r[..row.min(r.len())]);
        }
        let mut last = self.last.borrow_mut();
        let frames = last.get(&surface).map_or(0, |c| c.frames) + 1;
        last.insert(
            surface,
            Committed {
                size: target.size,
                scale: target.scale,
                pixels,
                frames,
            },
        );
    }
}

/// The key two renderers' surfaces match on: kind, screens and name.
fn spec_key(spec: Option<&SurfaceSpec>, node: NodeId) -> String {
    match spec {
        Some(s) => format!("{:?} {:?} {:?}", s.kind, s.screens, s.name),
        None => format!("{node:?}"),
    }
}

/// Surfaces by key: buffer size and scale.
type Placement = BTreeMap<String, (Size, Scale)>;

/// A cold boot (`boot`'s diffs) painted by a fresh offline renderer, one
/// surface per entry of `at` (its key, buffer size and scale); the
/// overlay is not part of a cold boot.
fn paint_cold(
    boot: &[SceneDiff],
    at: &BTreeMap<String, (Size, Scale)>,
) -> BTreeMap<String, Vec<u8>> {
    let font = std::fs::read(strand_text::test_font_path()).unwrap();
    let engine = TextEngine::new(FontConfig::isolated(vec![Arc::new(font)]));
    let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
    for d in boot {
        let errors = r.apply(d.clone());
        assert!(errors.is_empty(), "the renderer refused a diff: {errors:?}");
    }
    let mut out = BTreeMap::new();
    let mut next = 1;
    for (node, change) in r.take_surface_changes() {
        let SurfaceChange::Created(spec) = change else {
            continue;
        };
        let key = spec_key(Some(&spec), node);
        let Some(&(size, scale)) = at.get(&key) else {
            continue;
        };
        let surface = SurfaceId(next);
        next += 1;
        r.attach_surface(surface, node);
        r.configure_surface(surface, size, scale);
        let mut px = vec![0; (size.w * size.h * 4) as usize];
        let mut target = PaintTarget::new(&mut px, size, size.w * 4, scale, 0).unwrap();
        r.paint(surface, &mut target);
        out.insert(key, px);
    }
    out
}

/// The pipeline on a compositor: `strand run`'s main thread (surface
/// manager, renderer, text worker) on a headless sway with two outputs
/// named as the fuzzer's screens.
struct Wl {
    mgr: SurfaceManager<Host>,
    seen: Rc<Seen>,
    /// Dropped after the manager (field order).
    _sway: Sway,
}

impl Wl {
    /// `None` when sway is not installed (and not required).
    fn start(diffs: calloop::channel::Channel<SceneDiff>) -> Option<(Wl, Receiver<SceneDiff>)> {
        let sway = Sway::start("reload fuzzer")?;
        sway.msg(&["create_output"]).expect("swaymsg create_output");
        for o in SCREENS {
            sway.msg(&["output", o, "resolution", "800x600"])
                .unwrap_or_else(|| panic!("swaymsg output {o}"));
        }
        let (ping, ping_source) = calloop::ping::make_ping().unwrap();
        let font = std::fs::read(strand_text::test_font_path()).unwrap();
        let worker = TextWorker::spawn_with_waker(
            FontConfig::isolated(vec![Arc::new(font)]),
            Some(Box::new(move || ping.ping())),
        )
        .unwrap();
        let mut renderer = Renderer::new(TextBackend::Worker(worker));
        renderer.set_first_frame_wait(crate::demo::FIRST_FRAME_TEXT_WAIT);
        // Not forwarding the monitors: the logic thread hears the same
        // screens as the other pipelines (named as sway's outputs).
        let mut host = Host::new(renderer, false);
        let seen = Rc::new(Seen::default());
        host.probe = Some(ProbeHandle(seen.clone()));
        let conn =
            wayland_client::Connection::from_socket(UnixStream::connect(&sway.socket).unwrap())
                .unwrap();
        let mut mgr = SurfaceManager::with_connection(conn, host, Config::default()).unwrap();
        let handle = mgr.loop_handle();
        handle
            .insert_source(ping_source, |_, _, state| crate::demo::text_ready(state))
            .unwrap();
        let (fwd, inbox) = std::sync::mpsc::channel();
        handle
            .insert_source(diffs, move |event, _, state| {
                if let calloop::channel::Event::Msg(diff) = event {
                    let _ = fwd.send(diff.clone());
                    crate::demo::apply(state, diff);
                }
            })
            .unwrap();
        let deadline = Instant::now() + PATIENCE;
        loop {
            let names: BTreeSet<String> = mgr
                .state()
                .monitors()
                .into_iter()
                .filter_map(|m| m.connector)
                .collect();
            if SCREENS.iter().all(|s| names.contains(*s)) {
                break;
            }
            assert!(Instant::now() < deadline, "sway's outputs: {names:?}");
            mgr.dispatch(Some(Duration::from_millis(10))).unwrap();
        }
        Some((
            Wl {
                mgr,
                seen,
                _sway: sway,
            },
            inbox,
        ))
    }
}

/// One live pipeline, saving in one style.
struct Shell {
    style: Style,
    /// What it runs on (`Sway` for the compositor pipeline).
    label: String,
    config: PathBuf,
    store: PathBuf,
    version: u64,
    /// The store target each link points at (symlink swap).
    targets: BTreeMap<String, PathBuf>,
    /// Delete-to-create gaps.
    gaps: Rng,
    max_gap: u64,
    /// This step's delete and create came further apart than the
    /// watcher's grace (the test thread was descheduled).
    descheduled: Option<Duration>,
    /// Creates that came past the grace and still landed as one load.
    late_creates: u64,
    worker: Option<Worker>,
    to_logic: calloop::channel::Sender<ToLogic>,
    thread: Option<JoinHandle<Result<(), String>>>,
    inbox: Receiver<SceneDiff>,
    wl: Option<Wl>,
    scene: SceneMirror,
    events: BufReader<UnixStream>,
    /// Every diff must leave the shell looking like one of these (empty:
    /// not checked, during the boot and while a mutation is shown).
    allowed: Vec<Look>,
    /// The surfaces' scene ids every diff must keep, and (`closed`) the
    /// only ones it may show: an edit of the main surface's layer,
    /// namespace or kind replaces that surface, never the note.
    fixed: BTreeSet<NodeId>,
    closed: bool,
    /// A random mutation is shown: the texts it may have changed are not
    /// checked.
    loose: bool,
    /// The overlay may open: a reload left notices that were not
    /// dismissed since.
    overlay_ok: bool,
    /// Reload notices not dismissed.
    notes: bool,
    /// The overlay may list errors (no clean commit since a held load).
    errors_ok: bool,
    had_overlay: bool,
    /// When the step's saves began.
    save_at: Instant,
    /// Since when a load is held back, and when the save that ends the
    /// hold began: the overlay opens only on a hold of 250 ms.
    hold: Option<Instant>,
    fix_at: Option<Instant>,
    /// How long after its save the held load reached the logic thread
    /// and was applied there (its event's timing: watch, compile,
    /// commit), when its 250 ms timer starts.
    held_lag: Duration,
    /// The hold on the logic thread's timeline, once the load that ends
    /// it came: from the held load's arrival to the next one's (each
    /// save's start plus its event's timing).
    gap: Option<Duration>,
    pixels: Option<Pixels>,
}

/// How long after its save a load reached the logic thread (`commit`:
/// and was applied there), from its reload event's timing.
fn lag(ev: &Json, commit: bool) -> Duration {
    let t = &ev["timing"];
    let mut keys = vec!["watch_ms", "compile_ms"];
    if commit {
        keys.push("commit_ms");
    }
    let ms: f64 = keys.iter().filter_map(|k| t[*k].as_f64()).sum();
    Duration::from_secs_f64(ms.max(0.0) / 1000.0)
}

impl Shell {
    /// A pipeline saving in `style`; `wayland`: on a headless sway
    /// (`None` when sway is not installed and not required).
    fn start(
        base: &Path,
        style: Style,
        files: &BTreeMap<String, String>,
        seed: u64,
        pixels: bool,
        wayland: bool,
    ) -> Option<Shell> {
        let label = if wayland {
            "Sway".to_string()
        } else {
            format!("{style:?}")
        };
        let (tx, rx) = calloop::channel::channel::<SceneDiff>();
        let (wl, inbox) = if wayland {
            let (wl, inbox) = Wl::start(rx)?;
            (Some(wl), inbox)
        } else {
            (None, inbox(rx))
        };
        let root = base.join(&label);
        let (config, store) = (root.join("config"), root.join("store"));
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&store).unwrap();
        let mut targets = BTreeMap::new();
        for (name, text) in files {
            if style == Style::SymlinkSwap {
                let target = store.join(format!("v0-{name}"));
                std::fs::write(&target, text).unwrap();
                std::os::unix::fs::symlink(&target, config.join(name)).unwrap();
                targets.insert(name.clone(), target);
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
            portal: None,
        };
        let (to_logic, from_main) = calloop::channel::channel();
        to_logic.send(ToLogic::Screens(screens())).unwrap();
        let thread = std::thread::spawn(move || logic(boot, Storage::none(), from_main, tx, live));
        let deadline = Instant::now() + PATIENCE;
        let stream = loop {
            match UnixStream::connect(&socket) {
                Ok(s) => break s,
                Err(e) => {
                    assert!(Instant::now() < deadline, "{label}: no IPC socket: {e}");
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        };
        let mut events = BufReader::new(stream);
        let ok = ipc::request(&mut events, &ipc::Request::Watch, PATIENCE).unwrap();
        assert_eq!(ok["ok"], true, "{ok}");
        let max_gap = env_u64("STRAND_FUZZ_MAX_GAP_MS").unwrap_or(MAX_GAP_MS);
        assert!(
            Duration::from_millis(max_gap) < GRACE,
            "STRAND_FUZZ_MAX_GAP_MS={max_gap} is not under the watcher's grace"
        );
        Some(Shell {
            style,
            label,
            config,
            store,
            version: 0,
            targets,
            gaps: Rng(seed.max(1)),
            max_gap,
            descheduled: None,
            late_creates: 0,
            worker: Some(worker),
            to_logic,
            thread: Some(thread),
            inbox,
            wl,
            scene: SceneMirror::new(),
            events,
            allowed: Vec::new(),
            fixed: BTreeSet::new(),
            closed: false,
            loose: false,
            overlay_ok: false,
            notes: false,
            errors_ok: false,
            had_overlay: false,
            save_at: Instant::now(),
            hold: None,
            fix_at: None,
            held_lag: Duration::ZERO,
            gap: None,
            pixels: pixels.then(Pixels::new),
        })
    }

    /// Why a step may not have been one save: the test thread's own
    /// delay, not a reload fault.
    fn blame(&self) -> String {
        match self.descheduled {
            Some(t) => format!(
                " (the test thread's delete and create were {} ms apart, past the watcher's 50 ms grace: it was descheduled on a loaded machine; not a reload fault)",
                t.as_millis()
            ),
            None => String::new(),
        }
    }

    /// A step's saves begin (one call per step, or per part of a partial
    /// save): a held load's hold ends here.
    fn saving(&mut self) {
        self.save_at = Instant::now();
        self.descheduled = None;
        if self.hold.is_some() && self.fix_at.is_none() {
            self.fix_at = Some(self.save_at);
        }
    }

    /// Save `text` as `name` the way this shell's editor does.
    fn save(&mut self, name: &str, text: &str) {
        let path = self.config.join(name);
        let exists = path.symlink_metadata().is_ok();
        match self.style {
            Style::InPlace => std::fs::write(&path, text).unwrap(),
            Style::Rename => {
                let tmp = self.config.join(".fuzz-save.tmp");
                std::fs::write(&tmp, text).unwrap();
                std::fs::rename(&tmp, &path).unwrap();
            }
            Style::BackupThenRename => {
                let backup = self.config.join(format!("{name}~"));
                if exists {
                    std::fs::rename(&path, &backup).unwrap();
                }
                std::fs::write(&path, text).unwrap();
                if exists {
                    std::fs::remove_file(&backup).unwrap();
                }
            }
            Style::DeleteAndCreate => {
                let removed = Instant::now();
                if exists {
                    std::fs::remove_file(&path).unwrap();
                    let gap = Duration::from_millis(self.gaps.below(self.max_gap + 1));
                    while removed.elapsed() < gap {
                        std::hint::spin_loop();
                    }
                }
                std::fs::write(&path, text).unwrap();
                let took = removed.elapsed();
                if took >= GRACE {
                    self.descheduled = Some(took);
                    self.late_creates += 1;
                }
            }
            Style::SymlinkSwap => {
                self.version += 1;
                let target = self.store.join(format!("v{}-{name}", self.version));
                std::fs::write(&target, text).unwrap();
                let link = self.config.join(".fuzz-link");
                let _ = std::fs::remove_file(&link);
                std::os::unix::fs::symlink(&target, &link).unwrap();
                std::fs::rename(&link, &path).unwrap();
                // The store keeps only what a link points at.
                if let Some(old) = self.targets.insert(name.to_string(), target) {
                    std::fs::remove_file(old).unwrap();
                }
            }
        }
    }

    fn run(&mut self, op: &Op) {
        match op {
            Op::Write(name, text) => self.save(name, text),
            Op::Remove(name) => {
                std::fs::remove_file(self.config.join(name)).unwrap();
                if let Some(t) = self.targets.remove(name) {
                    std::fs::remove_file(t).unwrap();
                }
            }
            Op::Rename(from, to) => {
                std::fs::rename(self.config.join(from), self.config.join(to)).unwrap();
                if let Some(t) = self.targets.remove(from) {
                    self.targets.insert(to.clone(), t);
                }
            }
        }
    }

    /// Apply one diff and check it (see the module docs).
    fn apply(&mut self, what: &str, diff: &SceneDiff) {
        let label = self.label.as_str();
        let booted = !self.scene.roots().is_empty();
        self.scene
            .apply(diff)
            .unwrap_or_else(|e| panic!("{what} ({label}): {e}"));
        if let Some(p) = &mut self.pixels {
            p.apply(diff.clone());
        }
        if !booted {
            return;
        }
        let scene = &self.scene;
        let now = look(scene);
        assert!(
            self.allowed.is_empty() || self.allowed.contains(&now),
            "{what} ({label}): a frame neither before nor after the step (not atomic){}\n{}\nallowed:\n{}",
            self.blame(),
            scene.render(),
            self.allowed
                .iter()
                .map(|l| l.scene.as_str())
                .collect::<Vec<_>>()
                .join("--\n")
        );
        let shell = surfaces(scene);
        assert!(
            shell.iter().any(|&r| is_main(scene, r)),
            "{what} ({label}): a blank frame: no surface\n{}",
            scene.render()
        );
        assert!(
            self.fixed.is_subset(&shell) && (!self.closed || self.fixed == shell),
            "{what} ({label}): a surface was recreated (or leaked) by an edit that keeps it: {:?} for {:?}\n{}",
            shell,
            self.fixed,
            scene.render()
        );
        let notes = shell
            .iter()
            .filter(|&&r| scene.kind(r) == Some(NodeKind::Osd))
            .count();
        assert_eq!(
            notes,
            1,
            "{what} ({label}): {notes} note surfaces\n{}",
            scene.render()
        );
        for &r in &shell {
            let texts = texts_under(scene, r);
            match scene.kind(r) {
                Some(NodeKind::Bar | NodeKind::Panel) => assert!(
                    self.loose
                        || (texts.len() >= 3 && texts.iter().any(|(_, t)| t.starts_with("n "))),
                    "{what} ({label}): a blank frame: the surface lost its texts\n{}",
                    scene.render()
                ),
                Some(NodeKind::Osd) => assert!(
                    texts.iter().any(|(_, t)| t == NOTE_TEXT),
                    "{what} ({label}): a blank frame: the note lost its text\n{}",
                    scene.render()
                ),
                _ => panic!("{what} ({label}): a leaked surface\n{}", scene.render()),
            }
        }
        let over = overlays(scene);
        assert!(
            over.len() <= 1,
            "{what} ({label}): {} overlays (a leaked surface)\n{}",
            over.len(),
            scene.render()
        );
        if let Some(&o) = over.first() {
            // Errors open it after 250 ms of quiet: a hold that long, and
            // not ended by a save before then (or notices left by a
            // reload, which wait as long).
            // Judged on the logic thread's timeline once the hold ended
            // (a watcher or compiler stall on the next load lengthens
            // the hold there), else on the test's.
            let held = self.gap.or_else(|| {
                self.hold.map(|h| {
                    self.fix_at
                        .unwrap_or_else(Instant::now)
                        .saturating_duration_since(h)
                })
            });
            assert!(
                self.had_overlay || self.overlay_ok || held.is_some_and(|h| h >= HELD_LONG),
                "{what} ({label}): the overlay opened with no notice and nothing held back for 250 ms (held {held:?}{}, the held load {:?} after its save)\n{}",
                if self.gap.is_some() {
                    " on the logic thread"
                } else {
                    " on the test's clock"
                },
                self.held_lag,
                scene.render()
            );
            let header = texts_under(scene, o)
                .into_iter()
                .map(|(_, t)| t)
                .find(|t| t.starts_with("strand:"))
                .unwrap_or_default();
            assert!(
                self.errors_ok || !header.contains(" error"),
                "{what} ({label}): the overlay lists errors after a clean commit\n{}",
                scene.render()
            );
        }
        self.had_overlay = !over.is_empty();
        if let Some(p) = &mut self.pixels {
            let frames = p.paint();
            assert_eq!(
                frames.len(),
                shell.len(),
                "{what} ({label}): {} painted surfaces for {} scene surfaces",
                frames.len(),
                shell.len()
            );
            for (key, (_, _, px)) in &frames {
                let first = px.get(..4).unwrap_or(&[]);
                assert!(
                    px.chunks_exact(4).any(|c| c != first),
                    "{what} ({label}): a blank frame: surface {key} painted only its background"
                );
            }
        }
    }

    /// The sway pipeline, with every diff it has applied applied here
    /// too: one layer surface per scene surface (the overlay's included),
    /// and no committed frame that showed only its background.
    fn check_wl(&self, what: &str) {
        let Some(wl) = &self.wl else {
            return;
        };
        let mut live: Vec<NodeId> = wl.mgr.state().surfaces().iter().map(|s| s.node).collect();
        live.sort();
        let mut want: Vec<NodeId> = self.scene.roots().to_vec();
        want.sort();
        assert_eq!(
            live,
            want,
            "{what} (Sway): the layer surfaces are not the scene's surfaces (leaked or missing)\n{}",
            self.scene.render()
        );
        let blank = wl.seen.blank.borrow();
        assert!(
            blank.is_empty(),
            "{what} (Sway): committed a frame showing only its background: {blank:?}\n{}",
            self.scene.render()
        );
    }

    /// The sway pipeline settles: every layer surface is configured and
    /// has committed a frame.
    fn settle_wl(&mut self, what: &str) {
        let Some(wl) = &mut self.wl else {
            return;
        };
        let deadline = Instant::now() + PATIENCE;
        loop {
            let waiting: Vec<SurfaceId> = wl
                .mgr
                .state()
                .surfaces()
                .iter()
                .filter(|s| !s.configured || !wl.seen.painted.borrow().contains(&s.id))
                .map(|s| s.id)
                .collect();
            if waiting.is_empty() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{what} (Sway): surfaces {waiting:?} never showed a frame"
            );
            wl.mgr.dispatch(Some(Duration::from_millis(5))).unwrap();
        }
        self.pump(what);
    }

    /// The next diff within `timeout` (the sway pipeline's main loop is
    /// run meanwhile).
    fn recv(&mut self, timeout: Duration) -> Option<SceneDiff> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(d) = self.inbox.try_recv() {
                return Some(d);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match &mut self.wl {
                None => return self.inbox.recv_timeout(left).ok(),
                Some(wl) => {
                    wl.mgr
                        .dispatch(Some(left.min(Duration::from_millis(5))))
                        .unwrap();
                    if left.is_zero() {
                        return self.inbox.try_recv().ok();
                    }
                }
            }
        }
    }

    fn pump(&mut self, what: &str) {
        while let Some(d) = self.recv(Duration::ZERO) {
            self.apply(what, &d);
        }
        self.check_wl(what);
    }

    /// Apply diffs until `done` holds.
    fn until(&mut self, what: &str, done: impl Fn(&SceneMirror) -> bool) {
        self.until_or(what, done, "");
    }

    /// [`Shell::until`], saying what was waited for when it times out.
    fn until_or(&mut self, what: &str, done: impl Fn(&SceneMirror) -> bool, wanted: &str) {
        self.pump(what);
        let deadline = Instant::now() + PATIENCE;
        while !done(&self.scene) {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.recv(left) {
                Some(d) => self.apply(what, &d),
                None => panic!(
                    "{what} ({}): timed out on\n{}{wanted}",
                    self.label,
                    self.scene.render()
                ),
            }
        }
        self.check_wl(what);
    }

    /// Apply diffs until the shell looks like `want`, every diff on the
    /// way looking like `before` or `want` (`before` `None`: not
    /// checked).
    fn reach(&mut self, what: &str, before: Option<&Look>, want: &Look) {
        self.allowed = match before {
            Some(b) => vec![b.clone(), want.clone()],
            None => Vec::new(),
        };
        let w = want.clone();
        let wanted = format!("\nwaiting for:\n{}{}", want.scene, want.tokens);
        self.until_or(what, |s| look(s) == w, &wanted);
        self.allowed = vec![want.clone()];
        self.settle_wl(what);
    }

    /// Its pixels equal a fresh renderer's painting of `boot`: the offline
    /// pipeline's painted buffers, and the sway pipeline's committed ones
    /// (the screen), at each surface's configured size and scale.
    fn same_pixels(&mut self, what: &str, boot: &[SceneDiff]) {
        self.same_screen(what, boot);
        let Some(p) = &mut self.pixels else {
            return;
        };
        let live = p.paint();
        let mut fresh = Pixels::new();
        for d in boot {
            fresh.apply(d.clone());
        }
        let want = fresh.paint();
        assert_eq!(
            live.keys().collect::<Vec<_>>(),
            want.keys().collect::<Vec<_>>(),
            "{what}: the painted surfaces differ from a cold boot's"
        );
        for (key, (_, size, px)) in &live {
            let (_, wsize, wpx) = &want[key];
            assert_eq!(
                size, wsize,
                "{what}: surface {key} sized unlike a cold boot"
            );
            let bad = mismatch(px, wpx, 2);
            assert!(
                bad == 0,
                "{what}: surface {key}: {bad} pixels differ from a cold boot's painting"
            );
        }
    }

    /// The sway pipeline's last committed buffer of every surface but the
    /// overlay equals a cold boot's painting at its size and scale: a
    /// surface that stopped repainting (or painted the wrong thing) fails
    /// here. Its main loop runs until they match (text from the worker
    /// may land a frame later), or the patience runs out.
    fn same_screen(&mut self, what: &str, boot: &[SceneDiff]) {
        let Some(wl) = &mut self.wl else {
            return;
        };
        let deadline = Instant::now() + PATIENCE;
        let mut cold: Option<(Placement, BTreeMap<String, Vec<u8>>)> = None;
        loop {
            // The live surfaces, keyed as a cold boot's.
            let mut live: BTreeMap<String, SurfaceId> = BTreeMap::new();
            let mut at: Placement = BTreeMap::new();
            let renderer = &wl.mgr.state().host().renderer;
            for info in wl.mgr.state().surfaces() {
                let spec = renderer.surface_spec(info.node);
                if spec.is_some_and(|s| s.name.as_deref() == Some(OVERLAY)) {
                    continue;
                }
                let key = spec_key(spec, info.node);
                at.insert(key.clone(), (info.buffer_size, info.scale));
                live.insert(key, info.id);
            }
            if cold.as_ref().is_none_or(|(a, _)| *a != at) {
                let want = paint_cold(boot, &at);
                assert_eq!(
                    want.keys().collect::<Vec<_>>(),
                    at.keys().collect::<Vec<_>>(),
                    "{what} (Sway): the layer surfaces differ from a cold boot's"
                );
                cold = Some((at, want));
            }
            let (at, want) = cold.as_ref().unwrap();
            let last = wl.seen.last.borrow();
            let bad: Vec<String> = live
                .iter()
                .filter_map(|(key, id)| {
                    let (size, scale) = at[key];
                    let why = match last.get(id) {
                        None => "no frame committed".to_string(),
                        Some(c) if c.size != size || c.scale != scale => format!(
                            "committed {:?} at {:?}, configured {size:?} at {scale:?}",
                            c.size, c.scale
                        ),
                        Some(c) => match mismatch(&c.pixels, &want[key], 2) {
                            0 => return None,
                            n => format!("{n} pixels differ (after {} frames)", c.frames),
                        },
                    };
                    Some(format!("{key}: {why}"))
                })
                .collect();
            drop(last);
            if bad.is_empty() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{what} (Sway): the screen differs from a cold boot's painting: {bad:?}\n{}",
                self.scene.render()
            );
            wl.mgr.dispatch(Some(Duration::from_millis(5))).unwrap();
        }
    }

    /// The next reload event `strand watch` hears (notices between
    /// reloads are noted).
    fn event(&mut self, what: &str) -> Json {
        loop {
            let mut line = String::new();
            match self.events.read_line(&mut line) {
                Ok(0) => panic!("{what} ({}): the shell hung up", self.label),
                Ok(_) => {}
                Err(e) => panic!("{what} ({}): no reload event: {e}", self.label),
            }
            let ev: Json = serde_json::from_str(&line).unwrap();
            if ev["event"] == "notices" {
                self.notes = true;
                self.overlay_ok = true;
            }
            if ev["event"] == "reload" {
                return ev;
            }
        }
    }

    /// Events already sent (none expected: a second one is a load the
    /// test did not cause).
    fn drain(&mut self, what: &str) {
        let stream = self.events.get_ref();
        stream.set_nonblocking(true).unwrap();
        let mut line = String::new();
        let mut extra = Vec::new();
        loop {
            match self.events.read_line(&mut line) {
                Ok(n) if n > 0 && line.ends_with('\n') => {
                    extra.push(std::mem::take(&mut line));
                }
                _ => break,
            }
        }
        self.events.get_ref().set_nonblocking(false).unwrap();
        if !line.is_empty() {
            // Half a line: the rest is on its way.
            let mut rest = String::new();
            let _ = self.events.read_line(&mut rest);
            line.push_str(&rest);
            extra.push(line);
        }
        for l in extra {
            let ev: Json = serde_json::from_str(&l).unwrap();
            assert!(
                ev["event"] != "reload",
                "{what} ({}): a load nobody saved for{}: {ev}",
                self.label,
                self.blame()
            );
            if ev["event"] == "notices" {
                self.notes = true;
                self.overlay_ok = true;
            }
        }
    }

    /// The save was held back: the attempt holds a file (with a
    /// diagnostic: never unreadable) and commits nothing, or (`partial`)
    /// only what is consistent without it (a module added beside the
    /// one whose removal is held), which changes nothing shown.
    fn held_event(&mut self, what: &str, partial: bool) {
        let ev = self.event(what);
        let label = self.label.as_str();
        assert!(
            ev["held"].as_array().is_some_and(|h| !h.is_empty()),
            "{what} ({label}): not held back: {ev}"
        );
        assert!(
            ev["unreadable"].as_array().is_some_and(|u| u.is_empty()),
            "{what} ({label}): a save read as unreadable: {ev}"
        );
        assert!(
            partial || ev["committed"].as_array().is_some_and(|c| c.is_empty()),
            "{what} ({label}): {ev}"
        );
        // Its diagnostics may open the overlay once they stood 250 ms.
        self.start_hold(&ev);
        self.errors_ok = true;
    }

    /// A load is held back (`ev` its event): the hold starts, unless one
    /// already runs.
    fn start_hold(&mut self, ev: &Json) {
        if self.hold.is_none() {
            self.hold = Some(self.save_at);
            self.held_lag = lag(ev, true);
        }
    }

    /// The save landed in one load: the cells it reset, whether it was
    /// split, and whether it left overlay notices.
    fn landed(&mut self, what: &str, single: bool) -> (usize, bool, bool) {
        let mut resets = 0;
        let mut split = false;
        let mut notes = false;
        loop {
            let ev = self.event(what);
            let label = self.label.as_str();
            assert!(
                ev["unreadable"].as_array().is_some_and(|u| u.is_empty()),
                "{what} ({label}): a save read as unreadable: {ev}"
            );
            resets += ev["reset"].as_array().map_or(0, Vec::len);
            notes |= ["reset", "kept_over_default", "notices"]
                .iter()
                .any(|k| ev[*k].as_array().is_some_and(|a| !a.is_empty()))
                || ev["cancelled"].as_u64().is_some_and(|c| c > 0);
            let held = ev["held"].as_array().is_some_and(|h| !h.is_empty());
            if !held {
                if let (Some(h), Some(f)) = (self.hold, self.fix_at) {
                    self.gap =
                        Some((f + lag(&ev, false)).saturating_duration_since(h + self.held_lag));
                }
                return (resets, split, notes);
            }
            // Several files saved one after another may land in two
            // loads (the first held back); one file's save never does.
            assert!(
                !single,
                "{what} ({label}): one save split into two loads{}: {ev}",
                self.blame()
            );
            split = true;
            // The rest of the save is already made: a hold this short
            // never opens the overlay.
            self.start_hold(&ev);
            self.fix_at.get_or_insert_with(Instant::now);
            self.errors_ok = true;
        }
    }

    /// After a clean commit: notices may keep the overlay open, errors
    /// may not.
    fn committed(&mut self, notes: bool) {
        self.notes |= notes;
        self.overlay_ok = self.notes;
        self.errors_ok = false;
        self.hold = None;
        self.fix_at = None;
        self.gap = None;
    }

    /// Dismiss the overlay if it is shown.
    fn dismiss(&mut self, what: &str) {
        self.pump(what);
        let Some(&o) = overlays(&self.scene).first() else {
            return;
        };
        let close = find_under(&self.scene, o, "×")
            .unwrap_or_else(|| panic!("{what}: the overlay has no close button"));
        self.to_logic
            .send(ToLogic::Event {
                node: close,
                event: NodeEvent::Click,
            })
            .unwrap();
        self.until(what, |s| overlays(s).is_empty());
        self.notes = false;
        self.overlay_ok = false;
    }

    fn click(&mut self, node: NodeId) {
        self.to_logic
            .send(ToLogic::Event {
                node,
                event: NodeEvent::Click,
            })
            .unwrap();
    }

    /// Stop it: the logic thread, the compiler worker and the watcher
    /// must end without a panic, and the sway pipeline's text worker must
    /// still be running.
    fn stop(mut self) {
        self.to_logic.send(ToLogic::Shutdown).unwrap();
        let joined = self.thread.take().map(|t| t.join());
        assert!(
            matches!(joined, Some(Ok(Ok(())))),
            "{}: the logic thread: {joined:?}",
            self.label
        );
        let worker = self.worker.take().map(Worker::join);
        assert!(
            matches!(worker, Some(Ok(()))),
            "{}: the compiler worker or the watcher panicked",
            self.label
        );
        if let Some(wl) = &mut self.wl {
            // The main thread's last diffs (the logic thread is gone).
            wl.mgr.dispatch(Some(Duration::ZERO)).unwrap();
            // The text worker runs until its handle goes: ended now, it
            // panicked.
            let running = match wl.mgr.state().host().renderer.text() {
                TextBackend::Worker(w) => w.is_running(),
                TextBackend::Inline(_) => true,
            };
            assert!(running, "{}: the text worker panicked", self.label);
        }
    }
}

/// The run's directory, removed however the run ends.
struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
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

/// A number from the environment: decimal, or hex with `0x`. Set but
/// unreadable is an error, never a silent default.
fn env_u64(name: &str) -> Option<u64> {
    let v = std::env::var(name).ok()?;
    let v = v.trim();
    let parsed = match v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(&hex.replace('_', ""), 16),
        None => v.replace('_', "").parse(),
    };
    Some(parsed.unwrap_or_else(|e| panic!("{name}={v:?} is not a number: {e}")))
}

/// What a user clicks: the `n` text, a cell's text, or one chip.
#[derive(Clone, Debug)]
enum Target {
    N,
    Cell(&'static str),
    Chip(ChipKey),
}

/// What a click on `t` changes.
fn clicked(m: &Model, s: &State, t: &Target) -> State {
    let mut s = s.clone();
    match t {
        Target::N => s.n += m.n_step,
        Target::Cell(c) => match s.cells.get_mut(c) {
            Some(V::Int(n)) => *n += 1,
            Some(V::Text(x)) => x.push('x'),
            None => panic!("no cell {c}"),
        },
        Target::Chip(k) => {
            if let Some(on) = s.chips.get_mut(k) {
                *on = !*on;
            }
        }
    }
    s
}

/// The text `t` shows now.
fn shown_text(s: &State, t: &Target) -> String {
    match t {
        Target::N => format!("n {}", s.n),
        Target::Cell(c) => format!("{c} {}", s.cells[c].show()),
        Target::Chip(k) => format!("{} {}", k.1, s.chips[k]),
    }
}

/// A random mutation (`before` → `after`) that still compiles is run as
/// an edit when it only touches an expression (a string, an argument, a
/// path) outside the keyed list's literal: the table keeps every cell
/// for it. One of a declaration's keywords is drawn again (what it would
/// keep is not modelled), as is one of the list (an entry dropped and
/// put back starts fresh: its chips' state goes) and one of the note's
/// file (the note may then show nothing, which is not a fault).
fn runnable_mutation(file: &str, word: &str, before: &str, after: &str) -> bool {
    let list = |t: &str| {
        t.lines()
            .find(|l| l.starts_with("export let list"))
            .map(str::to_string)
    };
    file != NOTE_FILE
        && (word.contains('"') || word.contains(',') || word.contains('.'))
        && list(before) == list(after)
}

/// design.md, "How reload is tested", through the whole live pipeline.
#[test]
fn random_edits_through_five_save_styles() {
    let edits = env_u64("STRAND_FUZZ_EDITS").unwrap_or(60);
    let seed = env_u64("STRAND_FUZZ_SEED").unwrap_or(0x5eed_f00d_cafe_0001);
    eprintln!("reload fuzzer: {edits} edits, seed {seed} ({seed:#x})");
    let mut r = Rng(seed.max(1));
    let base = Dir(base_dir());
    let _ = std::fs::remove_dir_all(&base.0);
    let mut model = Model::first();
    let mut files = model.files();
    // What is on disk (a broken file included).
    let mut disk = files.clone();
    let mut state = State::defaults(&model);
    let mut shells: Vec<Shell> = STYLES
        .iter()
        .enumerate()
        .filter_map(|(k, &s)| {
            Shell::start(
                &base.0,
                s,
                &files,
                seed ^ ((k as u64 + 1) * 0x9e37_79b9),
                s == Style::InPlace,
                false,
            )
        })
        .collect();
    // The compiler-to-pixels pipeline on a compositor, saving in place.
    shells.extend(Shell::start(
        &base.0,
        Style::InPlace,
        &files,
        seed,
        false,
        true,
    ));
    let labels: Vec<String> = shells.iter().map(|s| s.label.clone()).collect();
    eprintln!("reload fuzzer: pipelines {labels:?}");
    let (mut expect, boot) = cold_boot(&model, &files, &state);
    for sh in &mut shells {
        let e = expect.clone();
        sh.until("the boot", |s| look(s) == e);
        sh.allowed = vec![expect.clone()];
        sh.fixed = surfaces(&sh.scene);
        sh.closed = true;
        sh.settle_wl("the boot");
        sh.same_pixels("the boot", &boot);
    }
    let started = Instant::now();
    let mut counts: BTreeMap<&str, u64> = BTreeMap::new();
    // A file saved broken, on disk until a later save fixes it.
    let mut broken: Option<String> = None;
    // Saves made: drawn edits that change nothing are drawn again.
    let mut step = 0;
    let mut drawn = 0u64;
    let mut split_saves = 0u64;
    while step < edits {
        drawn += 1;
        // The user changes some state, a broken save held back or not
        // (the shell keeps running its last good config).
        for _ in 0..r.below(3) {
            let mut targets = vec![Target::N];
            targets.extend(state.cells.keys().map(|k| Target::Cell(k)));
            targets.extend(state.chips.keys().map(|k| Target::Chip(*k)));
            let target = r.pick(&targets).clone();
            let text = shown_text(&state, &target);
            let next = clicked(&model, &state, &target);
            let (want, want_boot) = cold_boot(&model, &files, &next);
            let w = format!(
                "step {step}: click `{text}` ({target:?}{})",
                if broken.is_some() {
                    ", a save held back"
                } else {
                    ""
                }
            );
            if broken.is_some() {
                *counts.entry("click-while-held").or_default() += 1;
            }
            for sh in &mut shells {
                sh.pump(&w);
                let root = match target {
                    Target::Chip((i, _)) => surface_of(&sh.scene, i),
                    _ => surfaces(&sh.scene)
                        .into_iter()
                        .find(|&r| is_main(&sh.scene, r)),
                };
                let node = root
                    .and_then(|root| find_under(&sh.scene, root, &text))
                    .unwrap_or_else(|| {
                        panic!("{w} ({}): not shown\n{}", sh.label, sh.scene.render())
                    });
                sh.click(node);
            }
            for sh in &mut shells {
                sh.reach(&w, Some(&expect), &want);
                sh.same_pixels(&w, &want_boot);
            }
            state = next;
            expect = want;
        }
        // Now and then the user closes the overlay.
        if r.below(3) == 0 {
            for sh in &mut shells {
                sh.dismiss(&format!("step {step}: dismiss"));
            }
        }
        let at = step;
        let what = |kind: &str| format!("step {at} ({kind})");
        let mut drew = edit(&model, &mut r);
        if matches!(drew, Edit::Mutate) {
            if broken.is_some() {
                continue;
            }
            match compiling_mutation(&files, &mut r) {
                Some((f, text, w)) => drew = Edit::Broken(f, text, Some(w)),
                None => continue,
            }
        }
        match drew {
            Edit::Mutate => continue,
            Edit::Broken(f, text, word) if broken.is_none() => {
                let mut t = disk.clone();
                t.insert(f.clone(), text.clone());
                if compile(&t).is_ok() {
                    // A random mutation that still compiles: run it,
                    // then save the file back.
                    let word = match word {
                        Some(word) if runnable_mutation(&f, &word, &files[&f], &text) => word,
                        _ => continue,
                    };
                    *counts.entry("mutation").or_default() += 1;
                    step += 1;
                    let w = format!("{} {f}: `{word}` {text:?}", what("mutation"));
                    for sh in &mut shells {
                        sh.saving();
                        sh.save(&f, &text);
                    }
                    for sh in &mut shells {
                        // What the mutation shows is not modelled.
                        sh.allowed = Vec::new();
                        sh.loose = true;
                        let (resets, _, notes) = sh.landed(&w, true);
                        assert_eq!(resets, 0, "{w} ({}): reset {resets} cells", sh.label);
                        sh.committed(notes);
                    }
                    std::thread::sleep(QUIET);
                    for sh in &mut shells {
                        sh.pump(&w);
                        sh.drain(&w);
                    }
                    let w = format!("{} {f}", what("mutation saved back"));
                    let (_, want_boot) = cold_boot(&model, &files, &state);
                    for sh in &mut shells {
                        sh.saving();
                        sh.save(&f, &files[&f]);
                    }
                    for sh in &mut shells {
                        let (resets, _, notes) = sh.landed(&w, true);
                        assert_eq!(resets, 0, "{w} ({}): reset {resets} cells", sh.label);
                        // Every cell kept: the state from before.
                        sh.reach(&w, None, &expect);
                        sh.loose = false;
                        sh.committed(notes);
                        sh.same_pixels(&w, &want_boot);
                        sh.drain(&w);
                    }
                    continue;
                }
                *counts.entry("broken").or_default() += 1;
                step += 1;
                let w = what("broken");
                for sh in &mut shells {
                    sh.saving();
                    sh.save(&f, &text);
                }
                for sh in &mut shells {
                    sh.held_event(&w, false);
                }
                std::thread::sleep(QUIET);
                for sh in &mut shells {
                    sh.pump(&w);
                    sh.drain(&w);
                }
                disk = t;
                broken = Some(f);
            }
            Edit::Broken(..) => {
                // The fix: the broken file back to its last good text.
                let f = broken.take().unwrap_or_default();
                *counts.entry("fixed").or_default() += 1;
                step += 1;
                let w = what("fix");
                for sh in &mut shells {
                    sh.saving();
                    sh.save(&f, &files[&f]);
                }
                let (_, want_boot) = cold_boot(&model, &files, &state);
                for sh in &mut shells {
                    let (resets, _, notes) = sh.landed(&w, true);
                    // The last good text again: nothing reset, and the
                    // screen a cold boot's.
                    assert_eq!(resets, 0, "{w} ({}): reset {resets} cells", sh.label);
                    sh.reach(&w, Some(&expect), &expect);
                    sh.committed(notes);
                    sh.same_pixels(&w, &want_boot);
                    sh.drain(&w);
                }
                disk = files.clone();
            }
            Edit::To(next, kind) => {
                let next_files = next.files();
                compile(&next_files).unwrap_or_else(|d| panic!("{kind}: {d:#?}\n{next_files:#?}"));
                let mut ops = plan(&disk, &next_files);
                if ops.is_empty() {
                    continue;
                }
                broken = None;
                *counts.entry(kind).or_default() += 1;
                step += 1;
                let w = what(kind);
                let surface_edit = recreates(kind);
                if surface_edit {
                    // The main surface may be replaced; the note is kept.
                    for sh in &mut shells {
                        sh.fixed = surfaces(&sh.scene)
                            .into_iter()
                            .filter(|&r| !is_main(&sh.scene, r))
                            .collect();
                        sh.closed = false;
                    }
                }
                // A partial multi-file save: one change alone (one that
                // does not type-check with the rest's old text) first,
                // held back; then the rest lands with it.
                // A module renamed in two loads (the new file first, its
                // old one's removal held back) is a module added, then
                // one removed: its cells go with their declarations, and
                // those holding a value the user set are reported reset
                // ("removed with its module"), the others silently.
                let mut unreported = 0;
                if ops.len() > 1 && r.below(2) == 0 {
                    // Held back on its own: inconsistent with what is on
                    // disk and with the last good files (the loader holds
                    // a broken file and commits what is consistent with
                    // the last good text).
                    let first = ops.iter().position(|op| {
                        compile(&after(&disk, op)).is_err() && compile(&after(&files, op)).is_err()
                    });
                    if let Some(k) = first {
                        let op = ops.remove(k);
                        if matches!(op, Op::Rename(..)) {
                            unreported = model
                                .cells
                                .iter()
                                .filter(|c| state.cells[c.name] == c.default_value())
                                .count();
                        }
                        *counts.entry("partial").or_default() += 1;
                        let w = format!("{w}, partial {op:?}");
                        for sh in &mut shells {
                            sh.saving();
                            sh.run(&op);
                        }
                        for sh in &mut shells {
                            sh.held_event(&w, true);
                        }
                        std::thread::sleep(QUIET);
                        for sh in &mut shells {
                            sh.pump(&w);
                        }
                    }
                }
                let single = ops.len() == 1;
                for sh in &mut shells {
                    sh.saving();
                    for op in &ops {
                        sh.run(op);
                    }
                }
                let (next_state, resets) = table(&model, &next, &state);
                let resets = resets - unreported;
                let (want, want_boot) = cold_boot(&next, &next_files, &next_state);
                for sh in &mut shells {
                    let main = |sh: &Shell| -> BTreeSet<NodeId> {
                        surfaces(&sh.scene)
                            .into_iter()
                            .filter(|&r| is_main(&sh.scene, r))
                            .collect()
                    };
                    let before = main(sh);
                    let (reported, split, notes) = sh.landed(&w, single);
                    if split {
                        split_saves += 1;
                    } else {
                        assert_eq!(
                            reported, resets,
                            "{w} ({}): `strand watch` reset {reported} cells, the table {resets}",
                            sh.label
                        );
                    }
                    sh.reach(&w, Some(&expect), &want);
                    sh.committed(notes);
                    if surface_edit {
                        // The main surface replaced, in the one diff.
                        let now = main(sh);
                        assert!(
                            before.is_disjoint(&now),
                            "{w} ({}): a surface kept its scene node",
                            sh.label
                        );
                    }
                    sh.fixed = surfaces(&sh.scene);
                    sh.closed = true;
                    sh.same_pixels(&w, &want_boot);
                    sh.drain(&w);
                }
                model = next;
                files = next_files.clone();
                disk = next_files;
                state = next_state;
                expect = want;
            }
        }
    }
    let elapsed = started.elapsed();
    let late: u64 = shells.iter().map(|s| s.late_creates).sum();
    for sh in shells {
        sh.stop();
    }
    eprintln!(
        "reload fuzzer: {edits} edits ({drawn} drawn) through {} pipelines {labels:?} in {:.1} s: {counts:?}; multi-file saves the watcher took in two loads: {split_saves}; delete-and-create saves the test thread was too slow for that still landed as one: {late}",
        labels.len(),
        elapsed.as_secs_f64()
    );
}
