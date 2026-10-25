//! (report) What a glitch report asks of an instance (docs/architecture.md,
//! "Glitch reports"): the cell tap ([`Instance::set_cell_tap`]), every live
//! cell's value ([`Instance::cells_snapshot`]), and for the replay, a cell
//! seeded as a baseline ([`Instance::seed_cell`]) by its declared type
//! ([`Instance::cell_type`]).
//!
//! **The tap** is told of every write to a `state` cell or a settings
//! field, whoever made it (a handler, a two-way binding, IPC, a reset, a
//! reload), once per tick: a cell written several times in one tick is
//! told once, with the value it held before the tick and the one it holds
//! after (the tick is the unit logic hands render, so nothing between is
//! ever drawn). A write that leaves the value as it was is not told. While
//! a tap is installed the instance keeps the last value told of each
//! cell, to know the old one; the cells come from the runtime's own list
//! of the cells a tick wrote (`Tick::written`), so no cell pays for the
//! tap on its write path. A token overlay (`strand set '$accent' …`) is
//! told as its path with `$` and the overlay's text.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use strand_core::{Error, NodeId as CoreId};
use strand_scene::{NodeId, NodeKind, Prop as SceneProp};

use super::inspect::is_password;
use super::reload::CellRec;
use super::{Ctx, Instance};
use crate::hir::DefKind;
use crate::lower::{PlaceRoot, PlaceSeg, TwoWay};
use crate::ty::Ty;
use crate::vm::Env;
use crate::vm::Value;
use crate::vm::value::Slot;

/// (report review) The cell a two-way binding writes: a `state`'s
/// signal or keyed list, or a settings field. `None` for a service's
/// field.
fn place_cell(tw: &TwoWay, env: &Rc<Env>) -> Option<CoreId> {
    let PlaceRoot::Def(d) = tw.place.root else {
        return None;
    };
    if let Some(slot) = env.settings(d) {
        let Some(PlaceSeg::Field(f)) = tw.place.segs.first() else {
            return None;
        };
        return slot
            .fields
            .iter()
            .find(|(n, _)| n == f)
            .map(|(_, s)| s.id());
    }
    match env.def(d)? {
        Slot::Signal(s) => Some(s.id()),
        Slot::Keyed(k, _) => Some(k.id()),
        _ => None,
    }
}

impl Ctx {
    /// (report review) A surface of `kind` mounted with scope `ident`:
    /// a lock's is noted, so the cells mounted under it are in no
    /// report.
    pub(super) fn note_lock_scope(&self, kind: NodeKind, ident: Rc<str>) {
        if kind == NodeKind::Lock {
            self.lock_scopes.borrow_mut().insert(ident);
        }
    }

    /// (report review) Whether `scope` is a lock surface's, or under one
    /// (a per-screen lock's instances are `ident[<screen>]`).
    pub(super) fn in_lock_scope(&self, scope: &str) -> bool {
        self.lock_scopes.borrow().iter().any(|l| {
            scope
                .strip_prefix(&**l)
                .is_some_and(|r| r.is_empty() || r.starts_with('/') || r.starts_with('['))
        })
    }
}

/// (report) One write the cell tap is told of.
#[derive(Debug)]
pub struct CellWrite<'a> {
    /// The cell's path as [`Instance::cells_of`] spells it
    /// (`toasts.dnd`, `Clock.open`, `TopBar[DP-1].expanded`, a settings
    /// field's `prefs.compact`, a token overlay's `$accent`).
    pub path: &'a str,
    pub old: &'a Value,
    pub new: &'a Value,
}

/// (report) The callback [`Instance::set_cell_tap`] installs.
pub type CellTap = Box<dyn FnMut(CellWrite<'_>)>;

/// The tap and what it was last told of each cell.
#[derive(Default)]
pub(crate) struct TapState {
    tap: Option<CellTap>,
    /// By core id: the cell's path and the value last told (or seen when
    /// the tap was installed).
    last: HashMap<CoreId, (String, Value)>,
}

impl Instance {
    /// (report) Install `tap` (`None` removes it): from the next tick on
    /// it is told of every write to a cell (see the module docs).
    pub fn set_cell_tap(&self, tap: Option<CellTap>) {
        if tap.is_some() {
            self.mark_password_cells(None);
        }
        let mut st = self.cell_tap.borrow_mut();
        st.last.clear();
        if tap.is_some() {
            for (id, path, v) in self.cell_values() {
                st.last.insert(id, (path, v));
            }
        }
        st.tap = tap;
    }

    /// (report) Every live cell's value by its path, sorted by path: each
    /// `state` cell, each settings field on its own, and each token
    /// overlay as `$path` with its text. What a glitch report's
    /// `cells.json` holds (the binary applies the redaction rules).
    pub fn cells_snapshot(&self) -> Vec<(String, Value)> {
        self.mark_password_cells(None);
        let mut out: Vec<(String, Value)> = self
            .cell_values()
            .into_iter()
            .map(|(_, path, v)| (path, v))
            .collect();
        for (path, text) in self.ctx.storage.tokens.values() {
            out.push((format!("${path}"), Value::text(text.as_str())));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Each live cell a report may hold (none of [`Ctx::secret_cells`]):
    /// its core id, path and value now.
    fn cell_values(&self) -> Vec<(CoreId, String, Value)> {
        let reg = self.ctx.registry.borrow();
        let closed = self.ctx.closed.borrow();
        let secret = self.ctx.secret_cells.borrow();
        let mut out = Vec::new();
        for (k, rec) in &reg.cells {
            if closed.contains(k) {
                continue;
            }
            for id in rec.ids() {
                if !secret.contains(&id)
                    && let Some((path, v, _)) = self.cell_value(rec, id)
                {
                    out.push((id, path, v));
                }
            }
        }
        out
    }

    /// The cell `id` of `rec`: its path, value now and default (`Null`
    /// for a settings field).
    fn cell_value(&self, rec: &CellRec, id: CoreId) -> Option<(String, Value, Value)> {
        let rt = &self.rt;
        match rec {
            CellRec::Plain {
                sig, path, default, ..
            } if sig.id() == id => {
                Some((path.clone(), sig.get_untracked(rt).ok()?, default.clone()))
            }
            CellRec::Keyed {
                keyed,
                list,
                path,
                default,
                ..
            } if keyed.id() == id => {
                Some((path.clone(), list.get_untracked(rt).ok()?, default.clone()))
            }
            CellRec::Settings { slot, path, .. } => {
                let (name, field) = slot.fields.iter().find(|(_, f)| f.id() == id)?;
                Some((
                    format!("{path}.{name}"),
                    field.get_untracked(rt).ok()?,
                    Value::Null,
                ))
            }
            _ => None,
        }
    }

    /// (report review) The cells of `written` a report may hold, found
    /// by [`Ctx::cell_ids`]: their id, path, value now and default. Only
    /// the cells written are read.
    fn written_values(&self, written: &[CoreId]) -> Vec<(CoreId, String, Value, Value)> {
        let ids = self.ctx.cell_ids.borrow();
        let reg = self.ctx.registry.borrow();
        let closed = self.ctx.closed.borrow();
        let secret = self.ctx.secret_cells.borrow();
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for id in written {
            if secret.contains(id) || !seen.insert(*id) {
                continue;
            }
            let Some(k) = ids.get(id) else {
                continue;
            };
            if closed.contains(k) {
                continue;
            }
            if let Some((path, v, d)) = reg.cells.get(k).and_then(|r| self.cell_value(r, *id)) {
                out.push((*id, path, v, d));
            }
        }
        out
    }

    /// (report review) Marks the cells password inputs are bound to
    /// two-way (`text: <-> pw`) as left out of reports: those of `nodes`,
    /// or of every node.
    pub(super) fn mark_password_cells(&self, nodes: Option<&[NodeId]>) {
        let em = self.ctx.em.borrow();
        let mut found = Vec::new();
        let mut look = |n: NodeId| {
            if !is_password(&em, n) {
                return;
            }
            if let Some(e) = em.nodes.get(&n) {
                for (p, tw, env) in &e.two_way {
                    if matches!(p, SceneProp::Text | SceneProp::Value)
                        && let Some(id) = place_cell(tw, env)
                    {
                        found.push(id);
                    }
                }
            }
        };
        match nodes {
            Some(ns) => ns.iter().copied().for_each(&mut look),
            None => em.nodes.keys().copied().for_each(&mut look),
        }
        drop(em);
        if !found.is_empty() {
            self.ctx.secret_cells.borrow_mut().extend(found);
        }
    }

    /// (report review) A widget's write to `node`'s `prop` through `tw`:
    /// a password input's bound cell is left out of reports before the
    /// write is told to the tap.
    pub(super) fn note_secret_write(
        &self,
        node: NodeId,
        prop: SceneProp,
        tw: &TwoWay,
        env: &Rc<Env>,
    ) {
        if matches!(prop, SceneProp::Text | SceneProp::Value)
            && is_password(&self.ctx.em.borrow(), node)
            && let Some(id) = place_cell(tw, env)
        {
            self.ctx.secret_cells.borrow_mut().insert(id);
        }
    }

    /// Tells the tap of the cells `written` this tick (`Tick::written`).
    pub(super) fn tap_cells(&self, written: &[CoreId]) {
        if written.is_empty() || self.cell_tap.borrow().tap.is_none() {
            return;
        }
        let changed = self.written_values(written);
        let mut st = self.cell_tap.borrow_mut();
        let TapState { tap, last } = &mut *st;
        let Some(tap) = tap.as_mut() else {
            return;
        };
        for (id, path, new, default) in changed {
            let old = match last.get(&id) {
                Some((_, v)) => v.clone(),
                None => default,
            };
            if old != new {
                tap(CellWrite {
                    path: &path,
                    old: &old,
                    new: &new,
                });
            }
            last.insert(id, (path, new));
        }
        // Cells gone with their scopes are forgotten now and then.
        if last.len() > 256 && last.len() > 2 * self.ctx.registry.borrow().cells.len() {
            let ids = self.ctx.cell_ids.borrow();
            let reg = self.ctx.registry.borrow();
            last.retain(|id, _| {
                ids.get(id)
                    .and_then(|k| reg.cells.get(k))
                    .is_some_and(|r| r.ids().contains(id))
            });
        }
    }

    /// Tells the tap of a token overlay set or cleared.
    pub(super) fn tap_overlay(&self, path: &str, old: Option<String>, new: Option<&str>) {
        let mut st = self.cell_tap.borrow_mut();
        let Some(tap) = st.tap.as_mut() else {
            return;
        };
        let text = |t: Option<&str>| t.map_or(Value::Null, Value::text);
        let (old, new) = (text(old.as_deref()), text(new));
        if old != new {
            tap(CellWrite {
                path: &format!("${path}"),
                old: &old,
                new: &new,
            });
        }
    }

    /// (report) The declared type of the cell at `path` (as
    /// [`Instance::cells_of`] spells it): a `state`'s, or a settings
    /// field's. `None` for no such cell.
    pub fn cell_type(&self, path: &str) -> Option<Ty> {
        let prog = &self.ctx.vm.prog;
        // `Owner[instance].name`: the instance qualifiers say nothing of
        // the type.
        let mut bare = String::with_capacity(path.len());
        let mut depth = 0usize;
        for c in path.chars() {
            match c {
                '[' => depth += 1,
                ']' => depth = depth.saturating_sub(1),
                _ if depth == 0 => bare.push(c),
                _ => {}
            }
        }
        let parts: Vec<&str> = bare.split('.').collect();
        let owner_of = |d: &crate::lower::DefInfo| match d.owner {
            Some(o) => prog.owner_name(o),
            None => d.module.clone(),
        };
        for d in &prog.defs {
            let owner = owner_of(d);
            let owner_parts = owner.split('.').count();
            let (Some(head), rest) = (parts.get(..owner_parts), parts.get(owner_parts..)) else {
                continue;
            };
            if head.join(".") != owner {
                continue;
            }
            match (&d.kind, rest) {
                (DefKind::State, Some([name])) if *name == d.name => return Some(d.ty.clone()),
                (DefKind::Settings, Some([name, field])) if *name == d.name => {
                    if let Ty::Record(r) = d.ty.non_null() {
                        return prog
                            .types
                            .record(*r)
                            .fields
                            .iter()
                            .find(|f| f.name == *field)
                            .map(|f| f.ty.clone());
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// (report) Set the cell at `path` to `value` as a baseline: readers
    /// update, but no `on change` fires (spec.md 8.4: never at boot), as a
    /// reload writes a kept cell. The replay's `[seed]` (docs/eval.md,
    /// "Replaying a glitch report"). A value its type does not hold is
    /// refused.
    pub fn seed_cell(&self, path: &str, value: Value) -> Result<(), Error> {
        let prog = self.ctx.vm.prog.clone();
        if let Some(ty) = self.cell_type(path)
            && !crate::vm::value::fits(&prog.types, &ty, &value)
        {
            return Err(Error::failed(format!(
                "`{path}` is a {}, not {}",
                prog.types.show(&ty),
                value.show(&prog.types)
            )));
        }
        match self.seed_target(path) {
            Some(Seed::Plain(sig)) => sig.set_reloaded(&self.rt, value).map(|_| ()),
            Some(Seed::Keyed(k)) => {
                let items = value.as_list().map(<[Value]>::to_vec).unwrap_or_default();
                k.replace_all_reloaded(&self.rt, items).map(|_| ())
            }
            None => Err(Error::failed(format!("no cell `{path}`"))),
        }
    }
}

impl Instance {
    /// (replayfix) Whether a cell is mounted at `path` now, one
    /// [`Instance::seed_cell`] can set: a cell whose type the program
    /// knows ([`Instance::cell_type`]) may not be mounted yet (a
    /// component inside a popup that has not opened).
    pub fn cell_mounted(&self, path: &str) -> bool {
        self.seed_target(path).is_some()
    }

    /// The mounted cell at `path`, as a seed sets it.
    fn seed_target(&self, path: &str) -> Option<Seed> {
        let reg = self.ctx.registry.borrow();
        for rec in reg.cells.values() {
            match rec {
                CellRec::Plain { sig, path: p, .. } if p == path => {
                    return Some(Seed::Plain(*sig));
                }
                CellRec::Keyed { keyed, path: p, .. } if p == path => {
                    return Some(Seed::Keyed(*keyed));
                }
                CellRec::Settings { slot, path: p, .. } => {
                    if let Some(field) = path
                        .strip_prefix(p.as_str())
                        .and_then(|r| r.strip_prefix('.'))
                        && let Some((_, sig)) = slot.fields.iter().find(|(n, _)| n == field)
                    {
                        return Some(Seed::Plain(*sig));
                    }
                }
                _ => {}
            }
        }
        None
    }
}

enum Seed {
    Plain(strand_core::Signal<Value>),
    Keyed(strand_core::KeyedSignal<crate::vm::value::ValueKey, Value>),
}
