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

use strand_core::{Error, NodeId as CoreId};

use super::Instance;
use super::reload::CellRec;
use crate::hir::DefKind;
use crate::ty::Ty;
use crate::vm::Value;

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

    /// Each live cell: its core id, path and value now.
    fn cell_values(&self) -> Vec<(CoreId, String, Value)> {
        let reg = self.ctx.registry.borrow();
        let closed = self.ctx.closed.borrow();
        let rt = &self.rt;
        let mut out = Vec::new();
        for (k, rec) in &reg.cells {
            if closed.contains(k) {
                continue;
            }
            match rec {
                CellRec::Plain { sig, path, .. } => {
                    if let Ok(v) = sig.get_untracked(rt) {
                        out.push((sig.id(), path.clone(), v));
                    }
                }
                CellRec::Keyed {
                    keyed, list, path, ..
                } => {
                    if let Ok(v) = list.get_untracked(rt) {
                        out.push((keyed.id(), path.clone(), v));
                    }
                }
                CellRec::Settings { slot, path, .. } => {
                    for (name, field) in &slot.fields {
                        if let Ok(v) = field.get_untracked(rt) {
                            out.push((field.id(), format!("{path}.{name}"), v));
                        }
                    }
                }
            }
        }
        out
    }

    /// Tells the tap of the cells `written` this tick (`Tick::written`).
    pub(super) fn tap_cells(&self, written: &[CoreId]) {
        if written.is_empty() || self.cell_tap.borrow().tap.is_none() {
            return;
        }
        let written: HashSet<CoreId> = written.iter().copied().collect();
        let changed: Vec<(CoreId, String, Value)> = self
            .cell_values()
            .into_iter()
            .filter(|(id, ..)| written.contains(id))
            .collect();
        let mut st = self.cell_tap.borrow_mut();
        let TapState { tap, last } = &mut *st;
        let Some(tap) = tap.as_mut() else {
            return;
        };
        for (id, path, new) in changed {
            let old = match last.get(&id) {
                Some((_, v)) => v.clone(),
                None => self.default_of(&path),
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
            let live: HashSet<CoreId> = self.cell_values().into_iter().map(|(id, ..)| id).collect();
            last.retain(|id, _| live.contains(id));
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

    /// The default of the cell at `path`, the value a cell mounted
    /// during the tick held before its first write.
    fn default_of(&self, path: &str) -> Value {
        let reg = self.ctx.registry.borrow();
        for rec in reg.cells.values() {
            match rec {
                CellRec::Plain {
                    default, path: p, ..
                }
                | CellRec::Keyed {
                    default, path: p, ..
                } if p == path => {
                    return default.clone();
                }
                _ => {}
            }
        }
        Value::Null
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
        let target = {
            let reg = self.ctx.registry.borrow();
            let mut found = None;
            for rec in reg.cells.values() {
                match rec {
                    CellRec::Plain { sig, path: p, .. } if p == path => {
                        found = Some(Seed::Plain(*sig));
                    }
                    CellRec::Keyed { keyed, path: p, .. } if p == path => {
                        found = Some(Seed::Keyed(*keyed));
                    }
                    CellRec::Settings { slot, path: p, .. } => {
                        if let Some(field) = path
                            .strip_prefix(p.as_str())
                            .and_then(|r| r.strip_prefix('.'))
                            && let Some((_, sig)) = slot.fields.iter().find(|(n, _)| n == field)
                        {
                            found = Some(Seed::Plain(*sig));
                        }
                    }
                    _ => {}
                }
                if found.is_some() {
                    break;
                }
            }
            found
        };
        match target {
            Some(Seed::Plain(sig)) => sig.set_reloaded(&self.rt, value).map(|_| ()),
            Some(Seed::Keyed(k)) => {
                let items = value.as_list().map(<[Value]>::to_vec).unwrap_or_default();
                k.replace_all_reloaded(&self.rt, items).map(|_| ())
            }
            None => Err(Error::failed(format!("no cell `{path}`"))),
        }
    }
}

enum Seed {
    Plain(strand_core::Signal<Value>),
    Keyed(strand_core::KeyedSignal<crate::vm::value::ValueKey, Value>),
}
