//! The syntactic read and write sets of chunks, for core's declared edges.
//!
//! core orders sinks by ranks over read edges and the write edges
//! handlers make. Read edges exist only once a node has run, so the
//! compiler declares what it can see in the source before the first
//! flush (architecture.md, "Lowering into strand-core"):
//! `rt.reads_from(node, &sources)` with every name a binding or handler
//! condition can read (all branches: a conservative superset), and
//! `rt.writes_to(handler, target)` for every assignment. This module
//! computes those sets per chunk, as names; the instantiator resolves the
//! names to core nodes in the scope it mounts the chunk in.
//!
//! A chunk's reads include the reads of the lambdas it makes and of the
//! `fn`s it calls (or takes as values), transitively: they run inside
//! the same computation.

use std::collections::BTreeSet;

use super::code::{Op, Place, PlaceRoot, PlaceSeg};
use super::{ChunkId, Program};
use crate::hir::{DefId, DefKind, LocalId, NodeIdx};

/// What a chunk can read, by name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reads {
    /// `state`s, `let`s and settings files read whole.
    pub defs: BTreeSet<DefId>,
    /// One field of a settings file (`prefs.accent`).
    pub fields: BTreeSet<(DefId, String)>,
    /// Locals bound in a scope (`for` items, component parameters,
    /// `screen`).
    pub locals: BTreeSet<LocalId>,
    /// Service fields; `None`: the service as a whole (a method call such
    /// as `clock.format(p)` reads whatever the method reads).
    pub services: BTreeSet<(String, Option<String>)>,
    /// Element instances (`hover`, `self.width`, `vol.hover`).
    pub nodes: BTreeSet<NodeIdx>,
}

impl Reads {
    fn extend(&mut self, o: &Reads) {
        self.defs.extend(o.defs.iter().copied());
        self.fields.extend(o.fields.iter().cloned());
        self.locals.extend(o.locals.iter().copied());
        self.services.extend(o.services.iter().cloned());
        self.nodes.extend(o.nodes.iter().copied());
    }
}

/// A place an assignment or list mutation writes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum WriteTarget {
    /// A `state` (or a settings file written whole).
    Def(DefId),
    /// One field of a settings file.
    Field(DefId, String),
    /// A service's `rw` field.
    Service(String, String),
    /// What an action of the service can change (`n.dismiss()`,
    /// `notifications.clear()`): [`crate::vm::ServiceHost::action_writes`].
    Action(String),
}

/// The direct reads of one chunk, and the chunks whose reads it takes
/// on (its lambdas, the `fn`s it calls).
fn direct(prog: &Program, id: ChunkId) -> (Reads, Vec<ChunkId>) {
    let chunk = prog.chunk(id);
    let mut r = Reads::default();
    let mut deps = Vec::new();
    let next_field = |i: usize| match chunk.ops.get(i + 1) {
        Some(Op::Field(n)) => Some(chunk.names[*n as usize].clone()),
        _ => None,
    };
    for (i, op) in chunk.ops.iter().enumerate() {
        match op {
            Op::Def(d) => match &prog.def(*d).kind {
                DefKind::Settings => match next_field(i) {
                    Some(f) => {
                        r.fields.insert((*d, f));
                    }
                    None => {
                        r.defs.insert(*d);
                    }
                },
                DefKind::State | DefKind::Let => {
                    r.defs.insert(*d);
                }
                DefKind::Fn => {
                    if let Some(f) = prog.fns.get(d) {
                        deps.push(f.body);
                    }
                }
                DefKind::Service(_) => {
                    r.services
                        .insert((prog.def(*d).name.clone(), next_field(i)));
                }
                _ => {}
            },
            Op::CallFn { def, .. } => {
                if let Some(f) = prog.fns.get(def) {
                    deps.push(f.body);
                }
            }
            Op::Local(l) => {
                r.locals.insert(*l);
            }
            Op::Service(n) => {
                r.services
                    .insert((chunk.names[*n as usize].clone(), next_field(i)));
            }
            Op::Node(n) => {
                r.nodes.insert(*n);
            }
            Op::Closure(l) => deps.push(chunk.lambdas[*l as usize].chunk),
            Op::Keyed { root, .. } => match root {
                super::code::KeyedRoot::Def(d) => {
                    r.defs.insert(*d);
                }
                super::code::KeyedRoot::Service { service, field } => {
                    r.services.insert((
                        chunk.names[*service as usize].clone(),
                        Some(chunk.names[*field as usize].clone()),
                    ));
                }
            },
            _ => {}
        }
    }
    (r, deps)
}

/// What one place write targets.
fn target(prog: &Program, place: &Place) -> Option<WriteTarget> {
    let first = match place.segs.first() {
        Some(PlaceSeg::Field(f)) => Some(f.clone()),
        _ => None,
    };
    match &place.root {
        PlaceRoot::Def(d) => Some(match (&prog.def(*d).kind, first) {
            (DefKind::Settings, Some(f)) => WriteTarget::Field(*d, f),
            _ => WriteTarget::Def(*d),
        }),
        PlaceRoot::Service(s) => first.map(|f| WriteTarget::Service(s.clone(), f)),
    }
}

/// Every chunk's reads (lambdas and called `fn`s included) and writes.
pub(crate) fn compute(prog: &mut Program) {
    let n = prog.chunks.len();
    let mut direct_reads = Vec::with_capacity(n);
    let mut deps = Vec::with_capacity(n);
    let mut writes = Vec::with_capacity(n);
    for id in 0..n as ChunkId {
        let (r, d) = direct(prog, id);
        direct_reads.push(r);
        deps.push(d);
        let chunk = prog.chunk(id);
        let mut w: Vec<WriteTarget> = chunk
            .ops
            .iter()
            .filter_map(|op| match op {
                Op::Store { place, .. } | Op::Mutate { place, .. } => {
                    target(prog, &chunk.places[*place as usize])
                }
                _ => None,
            })
            .chain(chunk.actions.iter().cloned().map(WriteTarget::Action))
            .collect();
        w.sort();
        w.dedup();
        writes.push(w);
    }
    // Transitive closure over lambdas and `fn` calls (recursion included).
    let mut reads = Vec::with_capacity(n);
    for id in 0..n {
        if deps[id].is_empty() {
            reads.push(direct_reads[id].clone());
            continue;
        }
        let mut all = direct_reads[id].clone();
        let mut seen: BTreeSet<ChunkId> = BTreeSet::from([id as ChunkId]);
        let mut stack: Vec<ChunkId> = deps[id].clone();
        while let Some(c) = stack.pop() {
            if !seen.insert(c) {
                continue;
            }
            all.extend(&direct_reads[c as usize]);
            stack.extend(deps[c as usize].iter().copied());
        }
        reads.push(all);
    }
    prog.reads = reads;
    prog.writes = writes;
}
