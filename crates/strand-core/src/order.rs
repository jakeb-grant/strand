//! Scheduling ranks: the order in which a flush runs sinks.
//!
//! Effects run once per flush, after everything that writes what they
//! read. Read edges alone don't say that: an effect (or an `on change`
//! handler, a listener, a task) that *writes* a cell creates an edge the
//! graph can't see, so creation order would run a reader before the writer
//! created after it and then run it again. Ranks are an Incremental-style
//! height over both kinds of edge:
//!
//! * a node read by another ranks no higher than its reader
//!   (`rank(reader) >= rank(source)`);
//! * a node owned by another ranks no lower than its owner (owners run
//!   first, so an owner re-running disposes what it owns before that runs);
//! * a cell or event queue written by a handler ranks above it
//!   (`rank(cell) >= rank(writer) + 1`), and an event queue's listeners
//!   rank with it or above (a listener also ranks with what it reads);
//!   each listener is delivered at its own rank, a woken task polled at
//!   its own or its writer's rank;
//! * `on change` handlers start at [`LATE_RANK`]: they run after every
//!   ordinary sink has settled, so one outside write fires them once with
//!   the final values.
//!
//! Most nodes have rank 0 and are not stored. A flush runs queued sinks by
//! `(rank, creation order)`.
//!
//! Read edges only exist once a node has run, so a sink's rank would be
//! incomplete before its first run. The compiler therefore *declares* the
//! edges it can see in the source: [`Runtime::reads_from`] (the syntactic
//! read set of a binding or handler, every branch: a conservative
//! superset) and [`Runtime::writes_to`] (every assignment and emit). With
//! both declared, ranks are complete before anything runs and a sink runs
//! exactly once per flush, the first flush included, even when it switches
//! between branches at run time. Undeclared edges are *learned*: a write
//! edge the first time a handler writes a target during a flush, a read
//! edge when a node first reads a source. Ranks only rise. A sink that has
//! never run and declares nothing (no reads, no writes) runs in a
//! *provisional* phase, after all ranked work and before `on change`
//! handlers ([`PROVISIONAL_RANK`]), so on a boot or reload it sees what
//! the declared writers wrote. Without declarations a learned edge can
//! still re-run a sink that already ran in that flush, once; its last run
//! sees the final values. A write edge that would close a loop (a handler
//! writing what it reads, directly or through other handlers) is a
//! feedback edge: it is not ranked, and the runtime's cycle guard bounds
//! it as before. A feedback edge is not a static-cycle error: a
//! self-normalising `on change x { if x > 10 { x = 10 } }` is a valid
//! program. Whether an edge ends up ranked or feedback, and the ranks,
//! do not depend on the order the declarations are made in: a read that
//! closes a loop demotes the write edge and lowers its target back to what
//! its remaining edges need.

use crate::error::Error;
use crate::runtime::{Diagnostic, NodeId, NodeKind, Runtime};

/// What a declared or learned write edge became ([`Runtime::writes_to`],
/// [`Runtime::write_edge`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteEdge {
    /// The target ranks above the writer: its readers run after the writer.
    Ranked,
    /// The edge closes a loop (the writer reads the target, directly or
    /// through other writers): not ranked; the runtime cycle guard bounds
    /// it. Not an error.
    Feedback,
}

/// The rank `on change` handlers start at: above any rank reachable from
/// ordinary writes (each write edge adds one).
pub(crate) const LATE_RANK: u32 = 1 << 20;

/// Where a sink that has never run and declares no edges runs its first
/// time: after every ranked sink, before `on change` handlers.
pub(crate) const PROVISIONAL_RANK: u32 = LATE_RANK - 1;

/// Write edges of one writer. Sets: a handler filling one cell per row
/// (2,000 rows) checks an edge on every write.
#[derive(Default)]
pub(crate) struct Writes {
    /// Targets ranked above the writer.
    ranked: foldhash::HashSet<NodeId>,
    /// Targets whose edge would close a loop (not ranked).
    feedback: foldhash::HashSet<NodeId>,
}

/// Declared read edges ([`Runtime::reads_from`]), both directions, and
/// the reads of handlers learned while they ran (a listener or a handler's
/// task reads without subscribing, so its read edges are kept here).
#[derive(Default)]
pub(crate) struct Declared {
    /// Reader -> the sources it declared (or was seen reading).
    sources: foldhash::HashMap<NodeId, Vec<NodeId>>,
    /// Source -> the readers that declared it.
    readers: foldhash::HashMap<NodeId, foldhash::HashSet<NodeId>>,
    /// Readers in `sources` only through learned reads (they never called
    /// [`Runtime::reads_from`]).
    learned_only: foldhash::HashSet<NodeId>,
}

impl Declared {
    pub(crate) fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }
    pub(crate) fn declares(&self, reader: NodeId) -> bool {
        self.sources.contains_key(&reader)
            && (self.learned_only.is_empty() || !self.learned_only.contains(&reader))
    }
    fn add(&mut self, reader: NodeId, sources: &[NodeId]) {
        let list = self.sources.entry(reader).or_default();
        for &s in sources {
            if s != reader && !list.contains(&s) {
                list.push(s);
            }
        }
        for &s in sources {
            if s != reader {
                self.readers.entry(s).or_default().insert(reader);
            }
        }
    }
    /// Forget a disposed node.
    pub(crate) fn forget(&mut self, n: NodeId) {
        self.learned_only.remove(&n);
        if let Some(sources) = self.sources.remove(&n) {
            for s in sources {
                if let Some(r) = self.readers.get_mut(&s) {
                    r.remove(&n);
                    if r.is_empty() {
                        self.readers.remove(&s);
                    }
                }
            }
        }
        self.readers.remove(&n);
    }
}

impl Writes {
    fn knows(&self, target: NodeId) -> bool {
        self.ranked.contains(&target) || self.feedback.contains(&target)
    }
    /// Drop edges to disposed targets once the lists have grown (a writer
    /// filling per-row cells that come and go).
    fn prune(&mut self, rt: &Runtime) {
        let len = self.ranked.len() + self.feedback.len();
        if len >= 32 && len.is_power_of_two() {
            self.ranked.retain(|&t| rt.exists(t));
            self.feedback.retain(|&t| rt.exists(t));
        }
    }
}

impl Runtime {
    /// The scheduling rank of `id` (0 unless something it reads is written
    /// by a handler, or it is an `on change` handler). Graph introspection
    /// for the inspector and tests.
    pub fn rank(&self, id: NodeId) -> u32 {
        self.rank_of(id)
    }

    /// The rank a queued sink runs at: its rank, or [`PROVISIONAL_RANK`]
    /// for a sink that has never run and declares no edges (its read edges
    /// don't exist yet, so it waits for the ranked work).
    pub(crate) fn sched_rank(&self, id: NodeId) -> u32 {
        let r = self.rank_of(id);
        if r >= PROVISIONAL_RANK {
            return r;
        }
        let fresh = self.inner.fresh.borrow();
        if fresh.is_empty()
            || !fresh.contains(&id)
            || self.inner.declared.borrow().declares(id)
            || self.inner.writes.borrow().contains_key(&id)
        {
            return r;
        }
        PROVISIONAL_RANK
    }

    pub(crate) fn rank_of(&self, id: NodeId) -> u32 {
        let ranks = self.inner.ranks.borrow();
        if ranks.is_empty() {
            return 0;
        }
        ranks.get(&id).copied().unwrap_or(0)
    }

    /// Declare that `writer` (an effect, listener, timer, `on change`
    /// handler or handler site) writes `target` (a cell or event queue), so
    /// sinks reading `target` are ordered after `writer` before the first
    /// write is seen: from the first flush on when they declare their reads
    /// ([`Runtime::reads_from`]) or declare nothing (they then wait for the
    /// ranked work on their first run), else once they have run. The
    /// compiler declares every assignment and `emit`. A declaration that
    /// closes a loop (`writer` reads `target`, directly or through other
    /// writers) is a feedback edge ([`WriteEdge::Feedback`]): nothing is
    /// ranked, and it is not an error (a self-normalising handler is
    /// valid). The answer is what the edge is *now*; a read declared later
    /// ([`Runtime::reads_from`]) can still turn a ranked edge into feedback,
    /// with the same ranks either order ([`Runtime::write_edge`] tells).
    /// `Err` only for a disposed id.
    pub fn writes_to(&self, writer: NodeId, target: NodeId) -> Result<WriteEdge, Error> {
        self.write_target_check(writer, target)?;
        Ok(if self.learn(writer, target) {
            WriteEdge::Ranked
        } else {
            WriteEdge::Feedback
        })
    }

    /// What the write edge `writer -> target` is, if it was declared or
    /// learned. Graph introspection for the inspector and tests.
    pub fn write_edge(&self, writer: NodeId, target: NodeId) -> Option<WriteEdge> {
        let writes = self.inner.writes.borrow();
        let w = writes.get(&writer)?;
        if w.ranked.contains(&target) {
            Some(WriteEdge::Ranked)
        } else if w.feedback.contains(&target) {
            Some(WriteEdge::Feedback)
        } else {
            None
        }
    }

    fn write_target_check(&self, writer: NodeId, target: NodeId) -> Result<(), Error> {
        if !self.exists(writer) {
            return Err(Error::Disposed(writer));
        }
        if !self.exists(target) {
            return Err(Error::Disposed(target));
        }
        Ok(())
    }

    /// Declare that `reader` (a memo, derived collection, effect, timer,
    /// `on change` handler, listener or handler site) reads `sources`, so
    /// it is ranked with them before its first run instead of after it. The
    /// compiler declares the syntactic read set of every binding and
    /// handler: every branch, a conservative superset of what one run
    /// reads. Calling it marks `reader` as declared even with no sources
    /// (a handler that reads nothing), so it is not held back to the
    /// provisional phase on its first run. Calls add up. A declared read
    /// that closes a loop through write edges turns those write edges into
    /// feedback edges, as a learned read does.
    pub fn reads_from(&self, reader: NodeId, sources: &[NodeId]) -> Result<(), Error> {
        if !self.exists(reader) {
            return Err(Error::Disposed(reader));
        }
        if let Some(&gone) = sources.iter().find(|&&s| !self.exists(s)) {
            return Err(Error::Disposed(gone));
        }
        {
            let mut declared = self.inner.declared.borrow_mut();
            declared.learned_only.remove(&reader);
            declared.add(reader, sources);
        }
        self.rank_after_sources(reader);
        Ok(())
    }

    /// A handler that is scheduled by rank rather than by observer edges
    /// (a listener; a task, through its handler) read `sources` while it
    /// ran: keep the reads it had not declared as read edges, so it ranks
    /// with them from now on (the first delivery may have seen a value
    /// its writer had not yet written), and count and report them like a
    /// sink's undeclared reads ([`Runtime::set_strict_edges`]) if it
    /// declared its reads.
    pub(crate) fn learn_reads(&self, reader: NodeId, sources: &[NodeId]) {
        if sources.is_empty() || !self.exists(reader) {
            return;
        }
        let (new, declared_reads): (Vec<NodeId>, bool) = {
            let declared = self.inner.declared.borrow();
            let list = declared.sources.get(&reader).map_or(&[][..], Vec::as_slice);
            let new = if list.len() > 16 && sources.len() > 16 {
                let set: foldhash::HashSet<NodeId> = list.iter().copied().collect();
                sources
                    .iter()
                    .filter(|&&s| s != reader && !set.contains(&s))
                    .copied()
                    .collect()
            } else {
                sources
                    .iter()
                    .filter(|&&s| s != reader && !list.contains(&s))
                    .copied()
                    .collect()
            };
            (new, declared.declares(reader))
        };
        if new.is_empty() {
            return;
        }
        if declared_reads {
            self.bump(|s| s.learned_edges += new.len() as u64);
            for &source in &new {
                self.report_learned(reader, source, false);
            }
        }
        {
            let mut declared = self.inner.declared.borrow_mut();
            if !declared.sources.contains_key(&reader) {
                declared.learned_only.insert(reader);
            }
            declared.add(reader, &new);
        }
        self.rank_after_sources(reader);
    }

    /// A handler is writing `target` during a flush: queue the write edge
    /// to be learned once the handler's run is over (its read edges are
    /// linked then, so a handler writing what it reads is seen as
    /// feedback).
    pub(crate) fn note_write(&self, target: NodeId) {
        if !self.inner.flushing.get() {
            return;
        }
        let Some(writer) = self.inner.writer.get() else {
            return;
        };
        if writer == target
            || self
                .inner
                .writes
                .borrow()
                .get(&writer)
                .is_some_and(|w| w.knows(target))
        {
            return;
        }
        if self.inner.learn_seen.borrow_mut().insert((writer, target)) {
            self.inner.learn_queue.borrow_mut().push((writer, target));
        }
    }

    /// Learn the write edges seen since the last call (the flush calls it
    /// before choosing the next sink).
    pub(crate) fn learn_queued(&self) {
        if self.inner.learn_queue.borrow().is_empty() {
            return;
        }
        let queue = std::mem::take(&mut *self.inner.learn_queue.borrow_mut());
        self.inner.learn_seen.borrow_mut().clear();
        for (writer, target) in queue {
            if self.exists(writer) && self.exists(target) {
                if self.write_edge(writer, target).is_none() {
                    self.bump(|s| s.learned_edges += 1);
                    self.report_learned(writer, target, true);
                }
                self.learn(writer, target);
            }
        }
    }

    /// Record `writer -> target` and rank `target` above `writer`. Returns
    /// false for a feedback edge (kept unranked).
    fn learn(&self, writer: NodeId, target: NodeId) -> bool {
        if let Some(w) = self.inner.writes.borrow().get(&writer) {
            if w.ranked.contains(&target) {
                return true;
            }
            if w.feedback.contains(&target) {
                return false;
            }
        }
        let rank = self.rank_of(writer).saturating_add(1);
        let ok = writer != target && self.raise(target, rank, Some(writer));
        let mut writes = self.inner.writes.borrow_mut();
        let w = writes.entry(writer).or_default();
        if ok {
            w.ranked.insert(target);
        } else {
            w.feedback.insert(target);
        }
        w.prune(self);
        ok
    }

    /// `id`'s sources changed: count the ones it did not declare, if it
    /// declared its reads ([`Stats::learned_edges`](crate::Stats)).
    pub(crate) fn count_undeclared_reads(&self, id: NodeId) {
        let declared = self.inner.declared.borrow();
        if !declared.declares(id) {
            return;
        }
        let Some(list) = declared.sources.get(&id) else {
            return;
        };
        let nodes = self.inner.nodes.borrow();
        let Some(n) = nodes.get(id) else { return };
        let undeclared: Vec<NodeId> = if list.len() > 16 && n.sources.len() > 16 {
            let set: foldhash::HashSet<NodeId> = list.iter().copied().collect();
            n.sources
                .iter()
                .filter(|s| !set.contains(s))
                .copied()
                .collect()
        } else {
            n.sources
                .iter()
                .filter(|s| !list.contains(s))
                .copied()
                .collect()
        };
        drop(nodes);
        drop(declared);
        if !undeclared.is_empty() {
            self.bump(|s| s.learned_edges += undeclared.len() as u64);
            for source in undeclared {
                self.report_learned(id, source, false);
            }
        }
    }

    /// Strict edges: report a learned edge once
    /// ([`Diagnostic::UndeclaredWrite`] / [`Diagnostic::UndeclaredRead`]).
    fn report_learned(&self, node: NodeId, other: NodeId, write: bool) {
        if !self.inner.strict_edges.get()
            || !self.inner.strict_seen.borrow_mut().insert((node, other))
        {
            return;
        }
        self.diagnose(if write {
            Diagnostic::UndeclaredWrite {
                writer: node,
                target: other,
            }
        } else {
            Diagnostic::UndeclaredRead {
                reader: node,
                source: other,
            }
        });
    }

    /// Report every edge the runtime has to learn as a diagnostic
    /// ([`Diagnostic::UndeclaredWrite`], [`Diagnostic::UndeclaredRead`]),
    /// once per edge, in the tick it is learned: what
    /// [`Stats::learned_edges`](crate::Stats) counts, made loud. Off by
    /// default; the VM's and compiler's test suites turn it on, so every
    /// fixture fails on a missing `reads_from`/`writes_to` declaration
    /// without asserting the counters. A debug build of the binary may turn
    /// it on too (the overlay then shows a lowering bug).
    pub fn set_strict_edges(&self, on: bool) {
        self.inner.strict_edges.set(on);
    }

    /// `id` was just created by (or moved to) `owner`: it ranks no lower.
    pub(crate) fn inherit_rank(&self, id: NodeId, owner: NodeId) {
        let r = self.rank_of(owner);
        if r > self.rank_of(id) {
            self.raise(id, r, None);
        }
    }

    /// `id`'s sources changed: rank it with its highest source. If that
    /// would close a loop (it now reads, through other handlers, what a
    /// handler downstream of it writes), the write edges on the loop are
    /// feedback: they are unranked and the rank retried.
    pub(crate) fn rank_after_sources(&self, id: NodeId) {
        for _ in 0..8 {
            // Again after a loop was broken: its target may have been
            // lowered.
            let top = {
                let nodes = self.inner.nodes.borrow();
                let Some(n) = nodes.get(id) else { return };
                let ranks = self.inner.ranks.borrow();
                if ranks.is_empty() {
                    return;
                }
                let declared = self.inner.declared.borrow();
                let extra = declared.sources.get(&id).map_or(&[][..], Vec::as_slice);
                n.sources
                    .iter()
                    .chain(extra)
                    .filter_map(|s| ranks.get(s).copied())
                    .max()
                    .unwrap_or(0)
            };
            if top <= self.rank_of(id) || self.raise(id, top, None) {
                return;
            }
            if !self.break_loop(id) {
                return;
            }
        }
    }

    /// A write edge into `target` was demoted to feedback: lower `target`
    /// (a cell or event queue) to what its remaining edges need (its owner,
    /// its ranked writers + 1), so the ranks match those of declaring the
    /// read first. Lowering one node keeps every constraint: what ranks at
    /// or above it still does.
    fn lower(&self, target: NodeId) {
        let cur = self.rank_of(target);
        let floor = {
            let nodes = self.inner.nodes.borrow();
            let Some(n) = nodes.get(target) else { return };
            if !matches!(
                n.kind,
                NodeKind::Signal | NodeKind::Events | NodeKind::Collection
            ) {
                return;
            }
            let ranks = self.inner.ranks.borrow();
            let rank = |id: NodeId| ranks.get(&id).copied().unwrap_or(0);
            let mut floor = n.owner.map_or(0, rank);
            let declared = self.inner.declared.borrow();
            for s in n
                .sources
                .iter()
                .chain(declared.sources.get(&target).into_iter().flatten())
            {
                floor = floor.max(rank(*s));
            }
            for (&w, edges) in self.inner.writes.borrow().iter() {
                if edges.ranked.contains(&target) {
                    floor = floor.max(rank(w).saturating_add(1));
                }
            }
            floor
        };
        if floor < cur {
            let mut ranks = self.inner.ranks.borrow_mut();
            if floor == 0 {
                ranks.remove(&target);
            } else {
                ranks.insert(target, floor);
            }
        }
    }

    /// Nodes that must rank at or above `n` (`true`: strictly above, a
    /// write edge).
    fn successors(&self, n: NodeId, out: &mut Vec<(NodeId, bool)>) {
        if let Some(node) = self.inner.nodes.borrow().get(n) {
            out.extend(node.observers.iter().map(|&o| (o, false)));
        }
        if let Some(children) = self.inner.owned.borrow().get(n) {
            out.extend(children.iter().map(|&c| (c, false)));
        }
        if let Some(w) = self.inner.writes.borrow().get(&n) {
            out.extend(w.ranked.iter().map(|&t| (t, true)));
        }
        if let Some(r) = self.inner.declared.borrow().readers.get(&n) {
            out.extend(r.iter().map(|&r| (r, false)));
        }
        if let Ok(data) = self.data(n) {
            out.extend(data.downstream().into_iter().map(|l| (l, false)));
        }
    }

    /// Find a path from `id` to one of its own sources and unrank the
    /// write edges on it. Returns whether any edge was unranked.
    fn break_loop(&self, id: NodeId) -> bool {
        let mut sources: foldhash::HashSet<NodeId> = match self.inner.nodes.borrow().get(id) {
            Some(n) => n.sources.iter().copied().collect(),
            None => return false,
        };
        if let Some(d) = self.inner.declared.borrow().sources.get(&id) {
            sources.extend(d.iter().copied());
        }
        let mut parent: foldhash::HashMap<NodeId, (NodeId, bool)> = foldhash::HashMap::default();
        let mut queue = std::collections::VecDeque::from([id]);
        let mut next = Vec::new();
        while let Some(n) = queue.pop_front() {
            next.clear();
            self.successors(n, &mut next);
            for &(m, write) in &next {
                if m == id || parent.contains_key(&m) {
                    continue;
                }
                parent.insert(m, (n, write));
                if !sources.contains(&m) {
                    queue.push_back(m);
                    continue;
                }
                let mut demoted = Vec::new();
                let mut cur = m;
                {
                    let mut writes = self.inner.writes.borrow_mut();
                    while cur != id {
                        let (p, write) = parent[&cur];
                        if write && let Some(w) = writes.get_mut(&p) {
                            w.ranked.remove(&cur);
                            w.feedback.insert(cur);
                            demoted.push(cur);
                        }
                        cur = p;
                    }
                }
                for &t in &demoted {
                    self.lower(t);
                }
                if !demoted.is_empty() {
                    return true;
                }
            }
        }
        false
    }

    /// Raise `root` to at least `rank` and everything that must rank above
    /// or with it (readers, owned nodes, listeners, write targets). All or
    /// nothing: returns false, changing nothing, when the walk reaches
    /// `forbid` (the writer whose edge is being learned) or comes back to
    /// `root` (a loop).
    pub(crate) fn raise(&self, root: NodeId, rank: u32, forbid: Option<NodeId>) -> bool {
        if self.rank_of(root) >= rank {
            return true;
        }
        let mut lifted: foldhash::HashMap<NodeId, u32> = foldhash::HashMap::default();
        let mut stack = vec![(root, rank)];
        let mut next = Vec::new();
        while let Some((n, r)) = stack.pop() {
            if Some(n) == forbid {
                return false;
            }
            let cur = lifted.get(&n).copied().unwrap_or_else(|| self.rank_of(n));
            if r <= cur {
                continue;
            }
            // Back at the root with a higher rank, or (a safety net) a rank
            // grown by more than one per node lifted: going round a loop.
            if n == root && lifted.contains_key(&root)
                || r > rank.saturating_add(lifted.len() as u32 + 1)
            {
                return false;
            }
            lifted.insert(n, r);
            next.clear();
            self.successors(n, &mut next);
            stack.extend(
                next.iter()
                    .map(|&(m, write)| (m, if write { r.saturating_add(1) } else { r })),
            );
        }
        let mut ranks = self.inner.ranks.borrow_mut();
        for (n, r) in lifted {
            if self.inner.nodes.borrow().contains_key(n) {
                ranks.insert(n, r);
            }
        }
        true
    }
}
