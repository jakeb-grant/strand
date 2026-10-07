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
/// on: the `fn`s it calls, and its lambdas.
fn direct(prog: &Program, id: ChunkId) -> (Reads, Vec<ChunkId>, Vec<ChunkId>) {
    let chunk = prog.chunk(id);
    let mut r = Reads::default();
    let mut deps = Vec::new();
    let mut lambdas = Vec::new();
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
            Op::Closure(l) => lambdas.push(chunk.lambdas[*l as usize].chunk),
            // The load reads what its call chunk reads.
            Op::AsyncSite(site) => deps.push(*site),
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
    (r, deps, lambdas)
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
    let mut lambdas = Vec::with_capacity(n);
    let mut writes = Vec::with_capacity(n);
    for id in 0..n as ChunkId {
        let (r, d, l) = direct(prog, id);
        direct_reads.push(r);
        deps.push(d);
        lambdas.push(l);
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
    // Transitive closure over lambdas and `fn` calls (recursion
    // included): one union per strongly connected component, each taking
    // in the components it reaches, which Tarjan's algorithm finishes
    // first. Linear in the call graph (a fresh walk per chunk was
    // quadratic in call depth). A `fn`'s own locals (its parameters, its
    // `let`s) mean nothing where it is called: they reach callers only
    // through the lambdas that share their scope.
    let all: Vec<Vec<ChunkId>> = (0..n)
        .map(|i| deps[i].iter().chain(&lambdas[i]).copied().collect())
        .collect();
    let mut outer = closure(&all, |i| {
        let mut r = direct_reads[i].clone();
        r.locals.clear();
        r
    });
    let locals = closure(&lambdas, |i| Reads {
        locals: direct_reads[i].locals.clone(),
        ..Reads::default()
    });
    for (r, l) in outer.iter_mut().zip(locals) {
        r.locals = l.locals;
    }
    let reads = outer;
    prog.reads = reads;
    prog.writes = writes;
}

/// Each node's `own` reads together with those of every node it
/// reaches in `deps`.
fn closure(deps: &[Vec<ChunkId>], own: impl Fn(usize) -> Reads) -> Vec<Reads> {
    let (comp, order) = components(deps);
    let mut per: Vec<Reads> = Vec::with_capacity(order.len());
    for (c, members) in order.iter().enumerate() {
        let mut all = Reads::default();
        for &m in members {
            all.extend(&own(m));
            for &d in &deps[m] {
                let dc = comp[d as usize];
                if dc != c {
                    all.extend(&per[dc]);
                }
            }
        }
        per.push(all);
    }
    (0..deps.len()).map(|i| per[comp[i]].clone()).collect()
}

/// The strongly connected components of the graph `deps` (iterative
/// Tarjan): each node's component, and the components' members in the
/// order they finish (a component after every component it reaches).
fn components(deps: &[Vec<ChunkId>]) -> (Vec<usize>, Vec<Vec<usize>>) {
    const NONE: usize = usize::MAX;
    let n = deps.len();
    let mut index = vec![NONE; n];
    let mut low = vec![0; n];
    let mut on_stack = vec![false; n];
    let mut comp = vec![NONE; n];
    let mut stack = Vec::new();
    let mut order: Vec<Vec<usize>> = Vec::new();
    let mut next = 0;
    // (node, next edge to look at)
    let mut work: Vec<(usize, usize)> = Vec::new();
    for root in 0..n {
        if index[root] != NONE {
            continue;
        }
        work.push((root, 0));
        while let Some(&mut (v, ref mut edge)) = work.last_mut() {
            if *edge == 0 && index[v] == NONE {
                index[v] = next;
                low[v] = next;
                next += 1;
                stack.push(v);
                on_stack[v] = true;
            }
            if let Some(&w) = deps[v].get(*edge) {
                *edge += 1;
                let w = w as usize;
                if index[w] == NONE {
                    work.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
                continue;
            }
            work.pop();
            if let Some(&(parent, _)) = work.last() {
                low[parent] = low[parent].min(low[v]);
            }
            if low[v] == index[v] {
                let c = order.len();
                let mut members = Vec::new();
                while let Some(w) = stack.pop() {
                    on_stack[w] = false;
                    comp[w] = c;
                    members.push(w);
                    if w == v {
                        break;
                    }
                }
                order.push(members);
            }
        }
    }
    (comp, order)
}
