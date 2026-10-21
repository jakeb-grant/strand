//! (M5) The inspector's view of a running instance (design.md, "Developer
//! experience": identity, layout, token provenance, kept-state badges;
//! architecture.md, "M5 interfaces"). Read only, never read by
//! evaluation, and nothing here runs unless asked, except
//! [`Instance::inspect_tick`], which notes the cells a tick kept over a
//! changed default (and forgets those no longer holding the kept value)
//! and the password values a dump redacts.
//!
//! - [`Instance::identity`]: what keeps a node's identity across reloads
//!   (design.md, "Identity and state, in plain words").
//! - [`Instance::cells_of`]: the state cells on a node's mount path, with
//!   the three kept badges (kept over a changed default, persisted, from
//!   an overlay layer).
//! - [`Instance::reset_subtree`]: `strand reload --hard` limited to a
//!   subtree.
//! - [`Instance::source_hash`]: what write-back compares a file against.
//! - [`Instance::inspect_node`] and [`Instance::dump`]: the JSON the
//!   `inspect.node` and `inspect.dump` answers carry, password inputs
//!   redacted.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::{Rc, Weak};
use std::sync::Arc;

use serde_json::{Map, Value as Json};
use strand_core::{Error, NodeId as CoreId, SettingsLayer};
use strand_scene::provenance::value_reads;
use strand_scene::tokens::OVERLAY_ORIGIN;
use strand_scene::{NodeId, NodeKind, Prop, PropValue, SceneOp, StepKind, TokenExpr, TokenTable};

use super::Instance;
use super::emit::Emitter;
use super::reload::CellRec;
use crate::hir::LocalKind;
use crate::reconcile::{Build, KeptCell, Report};
use crate::source::{FileId, SourceMap};
use crate::vm::Value;

/// What a node's place in the instance tree is (design.md: a node keeps
/// its identity while its source text maps to it; next `key` or `id`
/// decides, then its position among siblings of the same kind).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The instance qualifiers on the node's mount path, outermost first
    /// (`[Mock | DP-1 | Display][3]`): a per-monitor bar's monitor, each
    /// `for` row's key.
    pub instance_path: String,
    /// The innermost of them: the `for` row's key, or the bar's monitor
    /// for a node directly in a bar; `None` outside any.
    pub key: Option<String>,
    /// The element's `id:` name, else that of the component call whose
    /// root it is (the outermost of a chain of calls).
    pub id: Option<String>,
    /// (eval-finish) Every other `id:` that names it: those of the calls
    /// nested in that one whose root it is too (`Pill live { id: pill }`
    /// as `PlayButton`'s root, called with `id: play_btn`), or the
    /// outermost call's when the element has its own; outermost first.
    pub also: Vec<String>,
    /// Its position among the siblings of its kind under its scene parent
    /// (the surface roots for a surface), in render's order, from 0.
    pub sibling_index: u32,
    /// The component the element is written in, else its file's module.
    pub scope: String,
}

/// A state cell as the inspector shows it, with its badges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellInfo {
    /// The cell's path as `strand watch` and `reset` name it
    /// (`Counter.open`, `TopBar[<monitor>].expanded`, `theme.prefs.compact`),
    /// or a token's (`$accent`).
    pub path: String,
    /// Kept over a changed default (`launcher.query: kept "fir" (default
    /// changed) [reset]`), and still not at its default.
    pub kept: bool,
    /// A `persist` cell: stored across restarts.
    pub persisted: bool,
    /// Where a settings field's value comes from (`overlay`, `file`,
    /// `default`), or `overlay` for a token a runtime overlay sets; `None`
    /// for plain state.
    pub layer: Option<&'static str>,
}

/// What the inspector keeps across reloads: the sources the running
/// program was compiled from, the cells kept over a changed default since
/// the last boot or hard reload, and the password values a dump redacts.
#[derive(Default)]
pub(crate) struct Track {
    sources: RefCell<Option<Arc<SourceMap>>>,
    /// Each kept cell by its registry key (one instance's cell, not every
    /// cell sharing its path), with the value it kept: forgotten once the
    /// cell holds anything else (reset, written, gone).
    kept: RefCell<HashMap<Rc<str>, Value>>,
    secrets: RefCell<Secrets>,
}

/// The password values a dump redacts, as the lock's `lock::Secrets`
/// keeps them: each password input's sent or written `text` and `value`,
/// and the newest [`REMEMBERED`] replaced by a value they do not start
/// (submitted and emptied, edited, their input removed). A widget's write
/// is noted as it happens ([`Secrets::see_write`]), so a value written and
/// cleared before the next tick (a paste and Return in one wake) is still
/// remembered.
#[derive(Default)]
struct Secrets {
    inputs: HashMap<NodeId, [Option<String>; 2]>,
    remembered: VecDeque<String>,
}

/// How many replaced password values [`Secrets`] keeps (the lock's
/// rule).
const REMEMBERED: usize = 4;

impl Secrets {
    /// A widget wrote `value` to password input `node`'s `prop` (the
    /// lock's `see_write`): the written value replaces the slot, and the
    /// old one is remembered unless the new one starts with it.
    fn see_write(&mut self, em: &Emitter, node: NodeId, prop: Prop, value: &PropValue) {
        let i = match prop {
            Prop::Text => 0,
            Prop::Value => 1,
            _ => return,
        };
        if !is_password(em, node) {
            return;
        }
        let PropValue::Text(t) = value else {
            return;
        };
        let slot = &mut self.inputs.entry(node).or_default()[i];
        let new = (!t.is_empty()).then(|| t.to_string());
        let old = std::mem::replace(slot, new);
        if let Some(old) = old
            && !t.starts_with(&old)
        {
            remember(&mut self.remembered, old);
        }
    }

    /// A tick's `ops` went out, and `em` holds what was sent.
    fn see(&mut self, em: &Emitter, ops: &[SceneOp]) {
        for op in ops {
            if let SceneOp::SetProp {
                id,
                prop: Prop::InputType,
                ..
            } = op
            {
                self.inputs.entry(*id).or_default();
            }
        }
        if self.inputs.is_empty() {
            return;
        }
        let Secrets { inputs, remembered } = self;
        inputs.retain(|id, slots| {
            let live = is_password(em, *id);
            for (slot, p) in slots.iter_mut().zip([Prop::Text, Prop::Value]) {
                let new = match em.sent.get(id).and_then(|s| s.get(&p)) {
                    Some(PropValue::Text(t)) if live && !t.is_empty() => Some(t.to_string()),
                    _ => None,
                };
                if *slot == new {
                    continue;
                }
                let old = std::mem::replace(slot, new);
                // Typed on (the old value starts the new one): nothing to
                // keep.
                if let Some(old) = old
                    && !slot.as_deref().is_some_and(|n| n.starts_with(&old))
                {
                    remember(remembered, old);
                }
            }
            live
        });
    }
}

/// Keeps a replaced value: one inside a kept value is dropped, and kept
/// ones inside it go.
fn remember(remembered: &mut VecDeque<String>, value: String) {
    if value.len() < MIN_SECRET || remembered.iter().any(|r| r.contains(&value)) {
        return;
    }
    remembered.retain(|r| !value.contains(r.as_str()));
    if remembered.len() == REMEMBERED {
        remembered.pop_front();
    }
    remembered.push_back(value);
}

impl Track {
    /// `build` is mounted (a boot, or `fresh`: a hard reload, which
    /// forgets which cells were kept).
    pub(crate) fn compiled(&self, build: &Build, fresh: bool) {
        *self.sources.borrow_mut() = Some(build.sources.clone());
        if fresh {
            self.kept.borrow_mut().clear();
        }
    }
}

/// A plain or keyed cell's path, value and default (`None`: a settings
/// slot, or a value that cannot be read).
fn cell_value<'a>(
    rec: &'a CellRec,
    rt: &strand_core::Runtime,
) -> Option<(&'a str, Value, &'a Value)> {
    match rec {
        CellRec::Plain {
            sig, default, path, ..
        } => Some((path, sig.get_untracked(rt).ok()?, default)),
        CellRec::Keyed {
            list,
            default,
            path,
            ..
        } => Some((path, list.get_untracked(rt).ok()?, default)),
        CellRec::Settings { .. } => None,
    }
}

impl Instance {
    /// A widget writes `value` to `node`'s `prop`: a password input's
    /// value is noted before it reaches the bound place.
    pub(crate) fn inspect_write(&self, node: NodeId, prop: Prop, value: &PropValue) {
        self.inspect
            .secrets
            .borrow_mut()
            .see_write(&self.ctx.em.borrow(), node, prop, value);
    }

    /// `build` was committed by a reload that did `report`.
    pub(crate) fn inspect_reloaded(&self, build: &Build, report: &Report) {
        self.inspect.compiled(build, false);
        self.note_kept(&report.kept_over_default);
    }

    /// A tick sends `ops` and reports `cells` (persisted cells kept over a
    /// changed default): kept cells that changed are forgotten, the new
    /// ones noted, the password values seen; `cells` passed on.
    pub(crate) fn inspect_tick(&self, ops: &[SceneOp], cells: Vec<KeptCell>) -> Vec<KeptCell> {
        self.prune_kept();
        self.note_kept(&cells);
        self.inspect
            .secrets
            .borrow_mut()
            .see(&self.ctx.em.borrow(), ops);
        // (report review) An input made a password one: its bound cell
        // is in no report from now on.
        let typed: Vec<NodeId> = ops
            .iter()
            .filter_map(|op| match op {
                SceneOp::SetProp {
                    id,
                    prop: Prop::InputType,
                    ..
                } => Some(*id),
                _ => None,
            })
            .collect();
        if !typed.is_empty() {
            self.mark_password_cells(Some(&typed));
        }
        cells
    }

    /// Notes the cells `cells` name that hold a value other than their
    /// default: just after a reload or a restore, exactly those kept
    /// (one at its old default took the new one).
    fn note_kept(&self, cells: &[KeptCell]) {
        if cells.is_empty() {
            return;
        }
        let reg = self.ctx.registry.borrow();
        let mut kept = self.inspect.kept.borrow_mut();
        for (k, rec) in &reg.cells {
            if let Some((path, v, d)) = cell_value(rec, &self.rt)
                && v != *d
                && cells.iter().any(|c| c.path == path)
            {
                kept.insert(k.clone(), v);
            }
        }
    }

    /// Forgets the kept cells that no longer hold the value they kept.
    fn prune_kept(&self) {
        let mut kept = self.inspect.kept.borrow_mut();
        if kept.is_empty() {
            return;
        }
        let reg = self.ctx.registry.borrow();
        kept.retain(|k, was| {
            reg.cells
                .get(k)
                .and_then(|rec| cell_value(rec, &self.rt))
                .is_some_and(|(_, v, _)| v == *was)
        });
    }

    /// Forgets the kept cells at `path` (`reset(path)`).
    pub(crate) fn forget_kept(&self, path: &str) {
        let mut kept = self.inspect.kept.borrow_mut();
        if kept.is_empty() {
            return;
        }
        let reg = self.ctx.registry.borrow();
        kept.retain(|k, _| {
            reg.cells
                .get(k)
                .and_then(|rec| cell_value(rec, &self.rt))
                .is_some_and(|(p, ..)| p != path)
        });
    }
}

/// A node id as the inspector's answers spell it (`12.0`: index and
/// generation), and as `inspect.node{id}`, `inspect.highlight{id}` and
/// `reset{node}` take it back.
pub fn node_ref(id: NodeId) -> String {
    format!("{}.{}", id.index, id.generation)
}

/// [`node_ref`] read back; `None` unless it is two numbers.
pub fn parse_node_ref(s: &str) -> Option<NodeId> {
    let (i, g) = s.split_once('.')?;
    Some(NodeId::new(i.parse().ok()?, g.parse().ok()?))
}

/// What replaces a password input's value, and anything showing it.
pub const REDACTED: &str = "<redacted>";

/// The shortest password matched outside its own input (the lock's
/// `Secrets` rule: a shorter one would match any text).
const MIN_SECRET: usize = 4;

/// The most core nodes [`Instance::cells_of`] walks back from a node's
/// bindings.
const MAX_READS: usize = 4096;

/// `scope` is `ident` or one of its ancestors in the instance tree.
fn under(ident: &str, scope: &str) -> bool {
    ident
        .strip_prefix(scope)
        .is_some_and(|r| r.is_empty() || r.starts_with('/'))
}

/// The instance-tree scope a node's element is mounted in: its reload
/// key without the element's own segment.
fn scope_ident(em: &Emitter, node: NodeId) -> Option<Rc<str>> {
    let key = em.nodes.get(&node)?.key.clone()?;
    Some(match key.rsplit_once('/') {
        Some((s, _)) => s.into(),
        None => "".into(),
    })
}

fn settings_layer(l: SettingsLayer) -> &'static str {
    match l {
        SettingsLayer::Overlay => OVERLAY_ORIGIN,
        SettingsLayer::File => "file",
        SettingsLayer::Default => "default",
    }
}

/// A password input on the scene (`input { type: password }`).
pub(super) fn is_password(em: &Emitter, node: NodeId) -> bool {
    em.nodes
        .get(&node)
        .is_some_and(|e| e.kind == NodeKind::Input)
        && matches!(
            em.sent.get(&node).and_then(|p| p.get(&Prop::InputType)),
            Some(PropValue::Keyword(k)) if k == "password"
        )
}

/// `text` holds a run of [`MIN_SECRET`] bytes of one of `secrets`
/// (lower-cased already), compared lower-cased: a slice or a case-changed
/// copy of a password counts (the lock's `Secrets` rule).
fn holds(text: &str, secrets: &[String]) -> bool {
    if secrets.is_empty() || text.len() < MIN_SECRET {
        return false;
    }
    let text = text.to_lowercase();
    let runs = || text.as_bytes().windows(MIN_SECRET);
    secrets.iter().any(|s| {
        s.as_bytes()
            .windows(MIN_SECRET)
            .any(|w| runs().any(|t| t == w))
    })
}

/// `v` shows one of `secrets`, raw or as shown.
fn leaks(v: &PropValue, shown: &str, secrets: &[String]) -> bool {
    let raw = |v: &PropValue| matches!(v, PropValue::Text(t) if holds(t, secrets));
    holds(shown, secrets) || raw(v) || matches!(v, PropValue::List(items) if items.iter().any(raw))
}

impl Instance {
    /// Where `node` sits in the instance tree; `None` for a node this
    /// instance did not make (the error overlay's) or that is gone.
    pub fn identity(&self, node: NodeId) -> Option<Identity> {
        let em = self.ctx.em.borrow();
        let e = em.nodes.get(&node)?;
        let ident = scope_ident(&em, node).unwrap_or_else(|| "".into());
        let reg = self.ctx.registry.borrow();
        // Outermost (shortest scope) first.
        let mut instances: Vec<(usize, Rc<str>)> = Vec::new();
        for (k, w) in &reg.envs {
            if let Some(i) = w.upgrade().and_then(|e| e.instance())
                && under(&ident, k)
            {
                let at = instances.partition_point(|(l, _)| *l < k.len());
                instances.insert(at, (k.len(), i));
            }
        }
        let prog = &self.ctx.vm.prog;
        let named = |idx| {
            prog.locals
                .iter()
                .find(|l| l.kind == LocalKind::NodeId(idx))
                .map(|l| l.name.clone())
        };
        // (M6) A component's root element shares its call's state, so an
        // `id:` on the call names it (decisions.md m6-w2-language): the
        // element's own `id:` first, then the call's (the outermost call
        // of a chain of calls, whose state the chain shares), then
        // (eval-finish) those of the calls nested in it the state was
        // handed down through. Each names it; the first is its `id`.
        let mut ids: Vec<String> = Vec::new();
        let calls = e.state.calls.borrow();
        let chain = std::iter::once(e.idx)
            .chain((e.state.idx != e.idx).then_some(e.state.idx))
            .chain(calls.iter().copied().filter(|c| *c != e.idx));
        for name in chain.filter_map(named) {
            if !ids.contains(&name) {
                ids.push(name);
            }
        }
        let mut ids = ids.into_iter();
        let id = ids.next();
        let also: Vec<String> = ids.collect();
        let sibling_index = em
            .children_of(e.parent)
            .iter()
            .filter(|n| em.nodes.get(n).is_some_and(|s| s.kind == e.kind))
            .position(|n| *n == node)
            .unwrap_or(0) as u32;
        let scope = reg
            .envs
            .get(&ident)
            .and_then(Weak::upgrade)
            .and_then(|env| env.component)
            // (M6) `tray.Row` when another file has a `Row`.
            .map(|c| prog.owner_name(c))
            .unwrap_or_else(|| self.ctx.module_of(e.file));
        Some(Identity {
            instance_path: instances.iter().map(|(_, i)| format!("[{i}]")).collect(),
            key: instances.last().map(|(_, i)| i.to_string()),
            id,
            also,
            sibling_index,
            scope,
        })
    }

    /// The state cells of `node`: those on its mount path (the
    /// component, surface and list-item instances it is mounted in,
    /// nearest first), then those its props read, directly or through
    /// `let`s (a file's top-level state and settings fields included),
    /// each settings field on its own, then the tokens it reads that a
    /// runtime overlay sets. Empty for an unknown node.
    pub fn cells_of(&self, node: NodeId) -> Vec<CellInfo> {
        let Some(ident) = scope_ident(&self.ctx.em.borrow(), node) else {
            return Vec::new();
        };
        let reads = self.reads_of(node);
        let reg = self.ctx.registry.borrow();
        let closed = self.ctx.closed.borrow();
        let kept = self.inspect.kept.borrow();
        // Kept: this cell still holds the value it kept.
        let is_kept = |k: &Rc<str>, v: Option<Value>, d: &Value| {
            v.is_some_and(|v| v != *d && kept.get(k) == Some(&v))
        };
        // (rank: the scope's depth on the mount path, 0 for a read; cell)
        let mut found: Vec<(usize, CellInfo)> = Vec::new();
        for (k, rec) in &reg.cells {
            if closed.contains(k) {
                continue;
            }
            let rank = match k.rsplit_once('#') {
                Some((scope, _)) if !scope.is_empty() && under(&ident, scope) => scope.len(),
                _ => 0,
            };
            // Nearest scope first, then by path.
            let mut push = |read: bool, info: CellInfo| {
                if rank > 0 || read {
                    let at = found
                        .partition_point(|(r, c)| *r > rank || (*r == rank && c.path < info.path));
                    found.insert(at, (rank, info));
                }
            };
            match rec {
                CellRec::Plain {
                    sig,
                    persisted,
                    default,
                    path,
                    ..
                } => push(
                    reads.contains(&sig.id()),
                    CellInfo {
                        path: path.clone(),
                        kept: is_kept(k, sig.get_untracked(&self.rt).ok(), default),
                        persisted: persisted.is_some(),
                        layer: None,
                    },
                ),
                CellRec::Keyed {
                    keyed: kv,
                    list,
                    default,
                    path,
                    ..
                } => push(
                    reads.contains(&kv.id()) || reads.contains(&list.id()),
                    CellInfo {
                        path: path.clone(),
                        kept: is_kept(k, list.get_untracked(&self.rt).ok(), default),
                        persisted: false,
                        layer: None,
                    },
                ),
                CellRec::Settings { slot, path, .. } => {
                    for (name, field) in &slot.fields {
                        push(
                            reads.contains(&field.id()),
                            CellInfo {
                                path: format!("{path}.{name}"),
                                kept: false,
                                persisted: false,
                                layer: slot
                                    .handle
                                    .as_ref()
                                    .and_then(|h| h.layer(name))
                                    .map(settings_layer),
                            },
                        );
                    }
                }
            }
        }
        drop((reg, closed, kept));
        let mut out: Vec<CellInfo> = found.into_iter().map(|(_, c)| c).collect();
        let table = self.token_table();
        let em = self.ctx.em.borrow();
        let levels = token_levels(&em, node);
        let mut overlaid: Vec<String> = Vec::new();
        for path in token_reads(&em, node) {
            for s in table.provenance(&path, &levels) {
                if s.kind == StepKind::Overlay && !overlaid.contains(&s.path) {
                    overlaid.push(s.path);
                }
            }
        }
        out.extend(overlaid.into_iter().map(|p| CellInfo {
            path: format!("${p}"),
            kept: false,
            persisted: false,
            layer: Some(OVERLAY_ORIGIN),
        }));
        out
    }

    /// Every core node `node`'s prop bindings read, through any number
    /// of memos (bounded: an inspector query never walks a whole graph).
    fn reads_of(&self, node: NodeId) -> HashSet<CoreId> {
        let mut stack = self
            .ctx
            .em
            .borrow()
            .nodes
            .get(&node)
            .map(|e| e.bindings.clone())
            .unwrap_or_default();
        let mut seen = HashSet::new();
        while let Some(id) = stack.pop() {
            if seen.len() >= MAX_READS {
                break;
            }
            if seen.insert(id) {
                stack.extend(self.rt.sources(id).unwrap_or_default());
            }
        }
        seen
    }

    /// "Reset subtree": `strand reload --hard` limited to `node`'s
    /// subtree. Every state cell of the component, surface, list-item
    /// and branch instances in it (those mounted under `node`, and those
    /// whose only top node is `node`: the component `node` is the root
    /// of) goes back to its default, as a reload write, so `on change`
    /// does not fire. Persisted cells and settings are untouched, as a
    /// hard reload keeps them. Returns how many cells changed.
    pub fn reset_subtree(&self, node: NodeId) -> Result<usize, Error> {
        let scopes: Vec<CoreId> = {
            let em = self.ctx.em.borrow();
            if !em.nodes.contains_key(&node) {
                return Err(Error::failed(format!("no node {}", node_ref(node))));
            }
            let within = |n: NodeId| {
                let mut cur = Some(n);
                while let Some(c) = cur {
                    if c == node {
                        return true;
                    }
                    cur = em.nodes.get(&c).and_then(|e| e.parent);
                }
                false
            };
            em.scoped()
                .into_iter()
                .filter(|(_, at, top)| at.is_some_and(&within) || top.as_slice() == [node])
                .map(|(s, ..)| s)
                .collect()
        };
        let rt = &self.rt;
        let inside = |holder: CoreId| {
            let mut cur = Some(holder);
            while let Some(c) = cur {
                if scopes.contains(&c) {
                    return true;
                }
                cur = rt.owner_of(c).ok().flatten();
            }
            false
        };
        let mut plain = Vec::new();
        let mut keyed = Vec::new();
        {
            let reg = self.ctx.registry.borrow();
            let closed = self.ctx.closed.borrow();
            for (k, rec) in &reg.cells {
                if closed.contains(k) || !inside(rec.holder().id()) {
                    continue;
                }
                match rec {
                    CellRec::Plain {
                        sig,
                        persisted: None,
                        default,
                        ..
                    } => plain.push((*sig, default.clone())),
                    CellRec::Keyed {
                        keyed: kv, default, ..
                    } => keyed.push((*kv, default.clone())),
                    _ => {}
                }
            }
        }
        let mut n = 0;
        for (sig, d) in plain {
            n += usize::from(matches!(sig.set_reloaded(rt, d), Ok(true)));
        }
        for (k, d) in keyed {
            let items = d.as_list().map(<[Value]>::to_vec).unwrap_or_default();
            n += usize::from(matches!(k.replace_all_reloaded(rt, items), Ok(true)));
        }
        // Back at their defaults: no longer kept.
        self.prune_kept();
        Ok(n)
    }

    /// The BLAKE3 hash (hex) of the text the running program compiled
    /// `file` from: write-back writes only if the file's bytes still hash
    /// to it. `None` for an instance made from a bare program or an
    /// unknown file.
    pub fn source_hash(&self, file: FileId) -> Option<String> {
        let sources = self.inspect.sources.borrow();
        let f = sources.as_ref()?.get(file)?;
        Some(blake3::hash(f.text.as_bytes()).to_hex().to_string())
    }

    /// `node` as `inspect.node{id}` answers it, `{"node": {...},
    /// "files": {...}}` (see [`Instance::dump`] for the shape); `None`
    /// for an unknown node.
    pub fn inspect_node(&self, node: NodeId) -> Option<Json> {
        let secrets = self.secrets();
        let table = self.token_table();
        let mut files = Map::new();
        let n = self.node_json(node, &table, &secrets, &mut files)?;
        self.every_file(&mut files);
        Some(obj(vec![("node", n), ("files", Json::Object(files))]))
    }

    /// (M5 wave 3) Every file the running program was compiled from, with
    /// its hash, beside the nodes' own: write-back checks a token's
    /// defining file too, which no node's origin names.
    fn every_file(&self, files: &mut Map<String, Json>) {
        let sources = self.inspect.sources.borrow();
        for (_, f) in sources.iter().flat_map(|m| m.iter()) {
            if !files.contains_key(&f.name) {
                let hash = blake3::hash(f.text.as_bytes()).to_hex().to_string();
                files.insert(f.name.clone(), Json::String(hash));
            }
        }
    }

    /// The inspector's dump (`inspect.dump`) of the tree under the
    /// surface root `root`, or of every surface: `{"nodes": [...],
    /// "files": {name: hash}}`, nodes in tree order. Each node has its
    /// `id` (as [`node_ref`] spells it), `kind`, `parent`, `origin`
    /// (`file`, 1-based `line` and `col`, byte `span`), `identity`,
    /// `props` (each sent prop as text), `cells` (with their badges) and
    /// `tokens` (each token a prop reads, with its provenance `chain`).
    /// A password input's value, and any prop or key showing it, is
    /// [`REDACTED`]. `None` if `root` is not a node.
    pub fn dump(&self, root: Option<NodeId>) -> Option<Json> {
        let em = self.ctx.em.borrow();
        let mut order = Vec::new();
        let mut stack: Vec<NodeId> = match root {
            Some(r) if em.nodes.contains_key(&r) => vec![r],
            Some(_) => return None,
            None => em.children_of(None).iter().rev().copied().collect(),
        };
        while let Some(n) = stack.pop() {
            if em.nodes.contains_key(&n) {
                order.push(n);
                stack.extend(em.children_of(Some(n)).iter().rev());
            }
        }
        drop(em);
        let secrets = self.secrets();
        let table = self.token_table();
        let mut files = Map::new();
        let nodes: Vec<Json> = order
            .into_iter()
            .filter_map(|n| self.node_json(n, &table, &secrets, &mut files))
            .collect();
        self.every_file(&mut files);
        Some(obj(vec![
            ("nodes", Json::Array(nodes)),
            ("files", Json::Object(files)),
        ]))
    }

    /// The password values a dump redacts, lower-cased: the password
    /// inputs' `Text` and `Value` on the scene, and the newest replaced
    /// ones ([`Secrets`]), each of [`MIN_SECRET`] bytes or more.
    fn secrets(&self) -> Vec<String> {
        let em = self.ctx.em.borrow();
        let mut out: Vec<String> = Vec::new();
        let mut add = |t: &str| {
            let t = t.to_lowercase();
            if t.len() >= MIN_SECRET && !out.contains(&t) {
                out.push(t);
            }
        };
        for n in em.nodes.keys().filter(|n| is_password(&em, **n)) {
            for p in [Prop::Text, Prop::Value] {
                if let Some(PropValue::Text(t)) = em.sent.get(n).and_then(|s| s.get(&p)) {
                    add(t);
                }
            }
        }
        let secrets = self.inspect.secrets.borrow();
        for t in secrets.inputs.values().flatten().flatten() {
            add(t);
        }
        for t in &secrets.remembered {
            add(t);
        }
        out
    }

    /// The global token table as last computed.
    fn token_table(&self) -> TokenTable {
        self.tokens
            .and_then(|t| t.get_untracked(&self.rt).ok())
            .unwrap_or_default()
    }

    fn node_json(
        &self,
        node: NodeId,
        table: &TokenTable,
        secrets: &[String],
        files: &mut Map<String, Json>,
    ) -> Option<Json> {
        let identity = self.identity(node)?;
        let cells = self.cells_of(node);
        let em = self.ctx.em.borrow();
        let e = em.nodes.get(&node)?;
        let levels = token_levels(&em, node);
        let password = is_password(&em, node);
        let empty = Default::default();
        let sent = em.sent.get(&node).unwrap_or(&empty);
        let mut props = Map::new();
        let mut tokens = Vec::new();
        // In the protocol's order.
        for p in Prop::ALL {
            let Some(v) = sent.get(p) else { continue };
            let text = shown(v);
            let hidden =
                (password && matches!(p, Prop::Text | Prop::Value)) || leaks(v, &text, secrets);
            props.insert(
                p.name().into(),
                Json::String(if hidden { REDACTED.into() } else { text }),
            );
            if *p == Prop::Tokens {
                continue;
            }
            for path in value_reads(v) {
                let chain: Vec<Json> = table
                    .provenance(&path, &levels)
                    .into_iter()
                    .map(|s| {
                        let level = match s.kind {
                            StepKind::Set(l) => Json::from(l),
                            _ => Json::Null,
                        };
                        obj(vec![
                            ("path", Json::String(s.path)),
                            ("kind", Json::from(s.kind.name())),
                            ("level", level),
                            ("origin", opt(s.origin)),
                            ("value", opt(s.value.as_ref().map(shown))),
                            ("reads", strings(s.reads)),
                        ])
                    })
                    .collect();
                tokens.push(obj(vec![
                    ("prop", Json::from(p.name())),
                    ("path", Json::String(path)),
                    ("chain", Json::Array(chain)),
                ]));
            }
        }
        let redact = |s: &str| match holds(s, secrets) {
            true => REDACTED.to_string(),
            false => s.to_string(),
        };
        let sources = self.inspect.sources.borrow();
        let mut origin = vec![
            ("span", numbers(&[e.span.start, e.span.end])),
            ("file", Json::Null),
        ];
        if let Some(f) = sources.as_ref().and_then(|m| m.get(e.file)) {
            let before = f.text.get(..e.span.start as usize).unwrap_or("");
            let line = before.matches('\n').count() + 1;
            let col = before[before.rfind('\n').map_or(0, |i| i + 1)..]
                .chars()
                .count()
                + 1;
            if !files.contains_key(&f.name) {
                let hash = blake3::hash(f.text.as_bytes()).to_hex().to_string();
                files.insert(f.name.clone(), Json::String(hash));
            }
            origin[1].1 = Json::String(f.name.clone());
            origin.push(("line", Json::from(line)));
            origin.push(("col", Json::from(col)));
        }
        let cells = cells
            .into_iter()
            .map(|c| {
                obj(vec![
                    ("path", Json::String(redact(&c.path))),
                    ("kept", Json::Bool(c.kept)),
                    ("persisted", Json::Bool(c.persisted)),
                    ("layer", opt(c.layer.map(str::to_string))),
                ])
            })
            .collect();
        Some(obj(vec![
            ("id", Json::String(node_ref(node))),
            ("kind", Json::from(e.kind.name())),
            ("parent", opt(e.parent.map(node_ref))),
            ("origin", obj(origin)),
            (
                "identity",
                obj(vec![
                    (
                        "instance_path",
                        Json::String(redact(&identity.instance_path)),
                    ),
                    ("key", opt(identity.key.as_deref().map(redact))),
                    ("id", opt(identity.id)),
                    ("sibling_index", Json::from(identity.sibling_index)),
                    ("scope", Json::String(identity.scope)),
                ]),
            ),
            ("props", Json::Object(props)),
            ("cells", Json::Array(cells)),
            ("tokens", Json::Array(tokens)),
        ]))
    }
}

/// The `set { }` overrides in scope at `node`, outermost first (as
/// render's scope chain has them).
fn token_levels(em: &Emitter, node: NodeId) -> Vec<&TokenTable> {
    let mut out = Vec::new();
    let mut cur = Some(node);
    while let Some(n) = cur {
        if let Some(PropValue::Tokens(t)) = em.sent.get(&n).and_then(|p| p.get(&Prop::Tokens)) {
            out.push(&**t);
        }
        cur = em.nodes.get(&n).and_then(|e| e.parent);
    }
    out.reverse();
    out
}

/// Every token `node`'s props read.
fn token_reads(em: &Emitter, node: NodeId) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (p, v) in em.sent.get(&node).into_iter().flatten() {
        if *p != Prop::Tokens {
            for r in value_reads(v) {
                if !out.contains(&r) {
                    out.push(r);
                }
            }
        }
    }
    out
}

/// A prop value as the dump shows it: text as it is, colours in hex,
/// numbers as the language writes them, a token reference as `$path`;
/// anything else as its debug form.
fn shown(v: &PropValue) -> String {
    match v {
        PropValue::Text(t) | PropValue::Keyword(t) => t.clone(),
        PropValue::Color(c) => crate::vm::value::hex(*c),
        PropValue::Number(n) => crate::vm::value::number(f64::from(*n)),
        PropValue::Bool(b) => b.to_string(),
        PropValue::Token(TokenExpr::Ref(p)) => format!("${p}"),
        v => format!("{v:?}"),
    }
}

/// A JSON object from its fields, in order.
fn obj(fields: Vec<(&str, Json)>) -> Json {
    let mut m = Map::new();
    for (k, v) in fields {
        m.insert(k.to_string(), v);
    }
    Json::Object(m)
}

fn opt(s: Option<String>) -> Json {
    s.map_or(Json::Null, Json::String)
}

fn strings(v: Vec<String>) -> Json {
    Json::Array(v.into_iter().map(Json::String).collect())
}

fn numbers(v: &[u32]) -> Json {
    Json::Array(v.iter().map(|n| Json::from(*n)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_refs_round_trip() {
        let id = NodeId::new(12, 3);
        assert_eq!(node_ref(id), "12.3");
        assert_eq!(parse_node_ref("12.3"), Some(id));
        for bad in ["", "12", "12.", ".3", "a.b", "1.2.3", "-1.0"] {
            assert_eq!(parse_node_ref(bad), None, "{bad}");
        }
    }

    #[test]
    fn scopes_nest_at_segment_boundaries() {
        assert!(under("/s3[DP-1]/c7", "/s3[DP-1]"));
        assert!(under("/s3[DP-1]/c7", "/s3[DP-1]/c7"));
        assert!(!under("/s3[DP-1]/c70", "/s3[DP-1]/c7"));
        assert!(!under("/s3", "/s3[DP-1]"));
    }
}
