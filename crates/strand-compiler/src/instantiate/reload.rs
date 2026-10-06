//! Live reload of a mounted instance (design.md, "What each edit does"
//! and "Identity and state, in plain words").
//!
//! A reload mounts the new program on the same runtime next to the old
//! one, with the old instance's registry as a *carry*: while mounting,
//! every scene node, state cell and handler of the new program looks up
//! the old one with the same reload key (the scope's place in the
//! instance tree plus the node's [`Sid`](crate::reconcile::Sid)) and takes
//! it over:
//!
//! - **Scene nodes** keep their ids, so render patches them in place
//!   ([`super::emit::Emitter::reduce`] turns the new boot ops into the
//!   diff from what render shows).
//! - **State cells** (on components, surfaces, list items and files) are
//!   moved to their new owner with `rt.reparent`: the same cell, its
//!   value kept. A changed default is adopted only if the cell still
//!   holds the old one, else kept with a notice; a changed name or type
//!   resets that cell with a warning; `@reset` resets it.
//! - **Handlers** whose Merkle hash is unchanged hand their in-flight
//!   tasks to the new handler; changed ones are disposed with the old
//!   instance, which cancels their `await` and reports it.
//! - **Timers** take over the old countdown, rescaled to the new
//!   duration (`Timer::rescale_from`).
//!
//! Then the old instance is disposed: whatever it still owns goes.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::Arc;

use strand_core::{NodeId as CoreId, Runtime, Scope};
use strand_scene::{NodeId, NodeKind};

use super::Ctx;
use crate::lower::Program;
use crate::reconcile::{EditClass, Report};
use crate::source::FileId;
use crate::syntax::Span;
use crate::vm::value::{NodeState, Value};
use crate::vm::{Env, SettingsSlot};

/// A state cell as a reload finds it.
pub(crate) enum CellRec {
    /// A `state` (persisted or not): its holder scope owns the cell.
    Plain {
        holder: Scope,
        sig: strand_core::Signal<Value>,
        persisted: Option<Rc<strand_core::Persisted<Value>>>,
        default: Value,
        ty: String,
        path: String,
    },
    /// A keyed list `state`: the items carry over (into a new
    /// collection, keyed again by the new program).
    Keyed {
        holder: Scope,
        keyed: strand_core::KeyedSignal<crate::vm::value::ValueKey, Value>,
        list: strand_core::Memo<Value>,
        default: Value,
        ty: String,
        path: String,
    },
    /// A settings file: the handle is kept and redeclared.
    Settings {
        holder: Scope,
        slot: Rc<SettingsSlot>,
        file: Option<std::path::PathBuf>,
        path: String,
    },
}

impl CellRec {
    pub(crate) fn holder(&self) -> Scope {
        match self {
            CellRec::Plain { holder, .. }
            | CellRec::Keyed { holder, .. }
            | CellRec::Settings { holder, .. } => *holder,
        }
    }

    /// Drop the cell (its holder and what it owns).
    pub(crate) fn dispose(&self, rt: &Runtime) {
        self.holder().dispose(rt);
    }

    /// It still holds its default (a settings file counts as kept: its
    /// values are on disk).
    pub(crate) fn at_default(&self, rt: &Runtime) -> bool {
        match self {
            CellRec::Plain { sig, default, .. } => {
                sig.get_untracked(rt).ok().is_none_or(|v| v == *default)
            }
            CellRec::Keyed { list, default, .. } => {
                list.get_untracked(rt).ok().is_none_or(|v| v == *default)
            }
            CellRec::Settings { .. } => true,
        }
    }

    pub(crate) fn path(&self) -> &str {
        match self {
            CellRec::Plain { path, .. }
            | CellRec::Keyed { path, .. }
            | CellRec::Settings { path, .. } => path,
        }
    }
}

/// An `on change … after` handler's current debounce and its delay.
pub(crate) type CurrentDebounce = Rc<Cell<Option<(std::time::Duration, strand_core::Debounced)>>>;

/// A handler or timer as a reload finds it.
pub(crate) struct HandlerRec {
    /// The node owning its in-flight tasks.
    pub site: Option<CoreId>,
    /// Its Merkle hash (0: unknown).
    pub hash: u64,
    /// A timer to rescale from, or a debounce (its current one).
    pub timer: Option<strand_core::Timer>,
    pub debounce: Option<CurrentDebounce>,
}

/// What an instance has mounted, by reload key.
#[derive(Default)]
pub(crate) struct Registry {
    pub nodes: HashMap<Rc<str>, (NodeId, NodeKind, Rc<NodeState>)>,
    pub cells: HashMap<Rc<str>, CellRec>,
    pub handlers: HashMap<Rc<str>, HandlerRec>,
    /// Scopes by key, for redirecting kept handlers' old scopes.
    pub envs: HashMap<Rc<str>, Weak<Env>>,
    /// `envs` is pruned of unmounted scopes when it reaches this length
    /// (doubling with what survives: amortised O(1) per mount).
    pub envs_prune_at: usize,
}

/// A reload in progress: what the old instance had, taken over as the
/// new one mounts, and what the takeover did.
pub(crate) struct Carry {
    pub old: Registry,
    pub old_prog: Arc<Program>,
    pub report: Report,
    /// Ids of old scene nodes the new instance kept.
    pub kept_nodes: std::collections::HashSet<NodeId>,
    /// Scope keys the new instance mounted.
    pub scopes: std::collections::HashSet<Rc<str>>,
    /// Cells the new instance created fresh, by scope key (renames).
    pub fresh: Vec<(Rc<str>, String)>,
    /// Persisted paths handed from an old cell to a new one: their
    /// "path in use" diagnostic is the handover, not a mistake.
    pub handover: Vec<String>,
    /// Surfaces the new instance mounted: their scope ident (a bar's:
    /// the prefix of its per-monitor idents) and whether it is a bar.
    pub surfaces: Vec<(Rc<str>, bool)>,
    /// Bars (by ident prefix) that mount exactly one instance in this
    /// reload: one that was a single surface hands that instance its
    /// cells.
    pub single_bars: std::collections::HashSet<Rc<str>>,
}

/// `<ident>[<instance>]<rest>` as `(ident, rest)`, for a bar instance
/// under `ident` (the instance's end: the first `]` followed by the
/// scope's own `#` or `/`, or the end).
fn strip_instance<'a>(key: &'a str, ident: &str) -> Option<&'a str> {
    let rest = key.strip_prefix(ident)?.strip_prefix('[')?;
    let mut from = 0;
    while let Some(i) = rest[from..].find(']') {
        let after = &rest[from + i + 1..];
        if after.is_empty() || after.starts_with('#') || after.starts_with('/') {
            return Some(after);
        }
        from += i + 1;
    }
    None
}

/// The instance part of a bar instance's key under `ident`.
fn instance_of<'a>(key: &'a str, ident: &str) -> Option<&'a str> {
    let rest = strip_instance(key, ident)?;
    Some(&key[ident.len()..key.len() - rest.len()])
}

impl Carry {
    /// Why the old cell under `key` cannot be kept when its surface
    /// changed between `bar` (one per monitor) and a single surface:
    /// one monitor's cell cannot become the single surface's, nor the
    /// other way round, without guessing.
    ///
    /// With one instance on either side there is nothing to guess: the
    /// cells move (see [`Ctx::bar_instances`] and [`Ctx::note_surface`]),
    /// and only what is left here (a bar on several monitors) is reset.
    pub(crate) fn surface_kind_change(&self, key: &str) -> Option<&'static str> {
        self.surfaces.iter().find_map(|(ident, per_monitor)| {
            let rest = key.strip_prefix(&**ident)?;
            match (per_monitor, rest.chars().next()) {
                (true, Some('#' | '/')) => Some("the surface is now a bar, one per monitor"),
                (false, Some('[')) => Some("the bar is now a single surface"),
                _ => None,
            }
        })
    }
}

impl Ctx {
    /// The reload identity of the node written at `span` in `file`.
    pub(crate) fn sid(&self, file: FileId, span: Span) -> String {
        match self.identity.as_ref().and_then(|i| i.sid(file, span)) {
            Some(s) => s.to_string(),
            // No identity (an instance made from a bare program): the
            // span is stable within one program.
            None => format!("@{}:{}", file.0, span.start),
        }
    }

    /// An element, service or `on change` handler at `span` with task
    /// site `site`: a reload hands it the old handler's in-flight tasks
    /// if its code is unchanged (else restarts it).
    pub(crate) fn keep_handler(
        self: &Rc<Self>,
        rt: &Runtime,
        env: &Rc<Env>,
        file: FileId,
        span: Span,
        site: CoreId,
    ) {
        let key: Rc<str> = format!("{}/h{}", env.ident(), self.sid(file, span)).into();
        let hash = self.hash(file, span);
        self.take_handler(rt, &key, hash, Some(site), EditClass::Handler);
        self.note_handler(
            rt,
            key,
            HandlerRec {
                site: Some(site),
                hash,
                timer: None,
                debounce: None,
            },
        );
    }

    /// A Merkle hash of the handler or timer at `span`.
    pub(crate) fn hash(&self, file: FileId, span: Span) -> u64 {
        self.hashes
            .as_ref()
            .and_then(|h| h.get(file, span))
            .unwrap_or(0)
    }

    /// The scene node a reload keeps for `key`, if the old instance had
    /// one of `kind`, with the old element state.
    pub(crate) fn claim_node(&self, key: &str, kind: NodeKind) -> Option<(NodeId, Rc<NodeState>)> {
        let mut carry = self.carry.borrow_mut();
        let c = carry.as_mut()?;
        match c.old.nodes.get(key) {
            Some((id, k, st)) if *k == kind => {
                let out = (*id, st.clone());
                c.old.nodes.remove(key);
                c.kept_nodes.insert(out.0);
                Some(out)
            }
            // A surface of another kind (`panel` → `osd`): a new node,
            // the surface recreated (its children and state are kept by
            // their own keys).
            Some((_, k, _)) if k.is_surface() && kind.is_surface() => {
                c.report.class(EditClass::Surface);
                None
            }
            _ => None,
        }
    }

    pub(crate) fn note_node(&self, key: Rc<str>, id: NodeId, kind: NodeKind, st: Rc<NodeState>) {
        self.registry.borrow_mut().nodes.insert(key, (id, kind, st));
    }

    pub(crate) fn note_env(&self, env: &Rc<Env>) {
        let key = env.ident();
        if let Some(c) = self.carry.borrow_mut().as_mut() {
            c.scopes.insert(key.clone());
        }
        let mut reg = self.registry.borrow_mut();
        if reg.envs.len() >= reg.envs_prune_at {
            reg.envs.retain(|_, w| w.strong_count() > 0);
            reg.envs_prune_at = (reg.envs.len() * 2).max(64);
        }
        reg.envs.insert(key, Rc::downgrade(env));
    }

    /// The old cell for `key`, taken out of the carry, with the program
    /// that made it.
    ///
    /// A bar's only instance takes the cell of the single surface it was
    /// (`ident#…` for `ident[<monitor>]#…`): design.md keeps state across
    /// a kind change, and with one instance nothing is guessed.
    pub(crate) fn take_cell(&self, key: &str) -> Option<(CellRec, Arc<Program>)> {
        let mut c = self.carry.borrow_mut();
        let c = c.as_mut()?;
        let rec = match c.old.cells.remove(key) {
            Some(r) => r,
            None => {
                let single = c.single_bars.iter().find_map(|ident| {
                    strip_instance(key, ident).map(|rest| format!("{ident}{rest}"))
                })?;
                c.old.cells.remove(single.as_str())?
            }
        };
        Some((rec, c.old_prog.clone()))
    }

    /// A bar under `ident` mounts `n` instances in this reload (its
    /// monitor list as first computed).
    pub(crate) fn bar_instances(&self, ident: &str, n: usize) {
        if let Some(c) = self.carry.borrow_mut().as_mut() {
            if n == 1 {
                c.single_bars.insert(ident.into());
            } else {
                c.single_bars.remove(ident);
            }
        }
    }

    /// Remember a cell under `key`, until its holder goes.
    pub(crate) fn note_cell(self: &Rc<Self>, rt: &Runtime, key: Rc<str>, rec: CellRec) {
        let holder = rec.holder().id();
        self.registry.borrow_mut().cells.insert(key.clone(), rec);
        let weak = Rc::downgrade(self);
        let _ = rt.with_owner(holder, |rt| {
            rt.on_cleanup(move || {
                if let Some(ctx) = weak.upgrade() {
                    let mut reg = ctx.registry.borrow_mut();
                    if reg
                        .cells
                        .get(&key)
                        .is_some_and(|c| c.holder().id() == holder)
                    {
                        reg.cells.remove(&key);
                        // Kept for a closed popup that went for good.
                        ctx.closed.borrow_mut().remove(&key);
                    }
                }
            });
        });
    }

    /// Note a cell made fresh in a reload (no old cell under its key).
    pub(crate) fn fresh_cell(&self, scope: Rc<str>, name: String) {
        if let Some(c) = self.carry.borrow_mut().as_mut() {
            c.fresh.push((scope, name));
        }
    }

    /// A surface mounted under `ident` (a bar: per monitor, `ident` the
    /// prefix of its instances' idents).
    ///
    /// A single surface that was a bar with one instance (on a monitor
    /// now or parked) takes that instance's cells: they move to the
    /// surface's keys (`ident[<monitor>]#…` to `ident#…`).
    pub(crate) fn note_surface(&self, ident: Rc<str>, per_monitor: bool) {
        let mut carry = self.carry.borrow_mut();
        let Some(c) = carry.as_mut() else { return };
        if !per_monitor {
            let mut pending = self.pending.borrow_mut();
            let mut instances: Vec<&str> = c
                .old
                .cells
                .keys()
                .chain(pending.keys())
                .filter_map(|k| instance_of(k, &ident))
                .collect();
            instances.sort_unstable();
            instances.dedup();
            if instances.len() == 1 {
                let moved: Vec<Rc<str>> = c
                    .old
                    .cells
                    .keys()
                    .filter(|k| strip_instance(k, &ident).is_some())
                    .cloned()
                    .collect();
                for k in moved {
                    if let (Some(rest), Some(rec)) =
                        (strip_instance(&k, &ident), c.old.cells.remove(&k))
                    {
                        c.old.cells.insert(format!("{ident}{rest}").into(), rec);
                    }
                }
                let moved: Vec<Rc<str>> = pending
                    .keys()
                    .filter(|k| strip_instance(k, &ident).is_some())
                    .cloned()
                    .collect();
                for k in moved {
                    if let (Some(rest), Some(rec)) =
                        (strip_instance(&k, &ident), pending.remove(&k))
                    {
                        pending.insert(format!("{ident}{rest}").into(), rec);
                    }
                }
            }
        }
        c.surfaces.push((ident, per_monitor));
    }

    /// A persisted path handed from an old cell to a new one.
    pub(crate) fn handover_path(&self, path: String) {
        if let Some(c) = self.carry.borrow_mut().as_mut() {
            c.handover.push(path);
        }
    }

    pub(crate) fn report(&self, f: impl FnOnce(&mut Report)) {
        if let Some(c) = self.carry.borrow_mut().as_mut() {
            f(&mut c.report);
        }
    }

    /// A handler at `key` with Merkle hash `hash` and task site `site`:
    /// an old one with the same hash hands over its in-flight tasks; one
    /// with another hash is restarted (its tasks are cancelled when the
    /// old instance goes). Returns the old timer or debounce, for a
    /// rescale.
    pub(crate) fn take_handler(
        &self,
        rt: &Runtime,
        key: &str,
        hash: u64,
        site: Option<CoreId>,
        class: EditClass,
    ) -> Option<HandlerRec> {
        let old = self.carry.borrow_mut().as_mut()?.old.handlers.remove(key)?;
        let tasks: Vec<CoreId> = old
            .site
            .and_then(|s| rt.owned(s).ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|t| rt.kind(*t) == Ok(strand_core::NodeKind::Task))
            .collect();
        if hash != 0 && hash == old.hash {
            if let Some(s) = site {
                for t in tasks {
                    let _ = rt.reparent(t, Some(s));
                }
            }
        } else {
            let n = tasks.len();
            self.report(|r| {
                r.class(class);
                if class == EditClass::Handler {
                    r.restarted += 1;
                }
                r.cancelled += n;
            });
        }
        Some(old)
    }

    pub(crate) fn note_handler(self: &Rc<Self>, rt: &Runtime, key: Rc<str>, rec: HandlerRec) {
        let (site, timer, plain) = (rec.site, rec.timer, rec.debounce.is_none());
        self.registry.borrow_mut().handlers.insert(key.clone(), rec);
        let weak = Rc::downgrade(self);
        rt.on_cleanup(move || {
            if let Some(ctx) = weak.upgrade() {
                let mut reg = ctx.registry.borrow_mut();
                if reg.handlers.get(&key).is_some_and(|h| {
                    h.site == site && h.timer == timer && h.debounce.is_none() == plain
                }) {
                    reg.handlers.remove(&key);
                }
            }
        });
    }

    /// The value a kept cell starts the new program with: the old value
    /// (translated to the new program's types), or the new default when
    /// the cell still held the old default. `None`: the value does not
    /// fit the new program (reset).
    pub(crate) fn adopt_value(
        &self,
        current: &Value,
        old_default: &Value,
        new_default: &Value,
        path: &str,
        from: &Program,
    ) -> Option<Value> {
        let to = self.vm.prog.clone();
        let (cur, _) = crate::vm::value::translate(current, &from.types, &to.types)?;
        let od = crate::vm::value::translate(old_default, &from.types, &to.types).map(|v| v.0);
        if od.as_ref() == Some(new_default) {
            return Some(cur);
        }
        self.report(|r| r.class(EditClass::StateDefault));
        if od.as_ref() == Some(&cur) {
            return Some(new_default.clone());
        }
        let shown = shown_kept(&cur, &to.types);
        self.report(|r| {
            r.kept_over(crate::reconcile::KeptCell {
                path: path.to_string(),
                shown,
            })
        });
        Some(cur)
    }
}

/// A kept value as the reload notice shows it: text quoted
/// (`launcher.query: kept "fir" (default changed) [reset]`).
pub(crate) fn shown_kept(v: &Value, types: &crate::ty::TypeTable) -> String {
    match v {
        Value::Text(t) => format!("{:?}", &**t),
        v => v.show(types),
    }
}
