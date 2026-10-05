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
//!   rank with it;
//! * `on change` handlers start at [`LATE_RANK`]: they run after every
//!   ordinary sink has settled, so one outside write fires them once with
//!   the final values.
//!
//! Most nodes have rank 0 and are not stored. A flush runs queued sinks by
//! `(rank, creation order)`. Write edges are *learned* the first time a
//! handler writes a target during a flush (or *declared* up front with
//! [`Runtime::writes_to`], which the compiler can do for every assignment
//! it sees), and ranks only rise. So a sink runs at most once per flush,
//! except that a write edge seen for the first time can re-run a sink that
//! already ran in that flush, once; from then on it is ordered after the
//! writer. A write edge that would close a loop (a handler writing what it
//! reads, directly or through other handlers) is a feedback edge: it is not
//! ranked, and the runtime's cycle guard bounds it as before.

use crate::error::Error;
use crate::runtime::{NodeId, Runtime};

use std::sync::Arc;

/// The rank `on change` handlers start at: above any rank reachable from
/// ordinary writes (each write edge adds one).
pub(crate) const LATE_RANK: u32 = 1 << 20;

/// Write edges of one writer.
#[derive(Default)]
pub(crate) struct Writes {
    /// Targets ranked above the writer.
    ranked: Vec<NodeId>,
    /// Targets whose edge would close a loop (not ranked).
    feedback: Vec<NodeId>,
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

    pub(crate) fn rank_of(&self, id: NodeId) -> u32 {
        let ranks = self.inner.ranks.borrow();
        if ranks.is_empty() {
            return 0;
        }
        ranks.get(&id).copied().unwrap_or(0)
    }

    /// Declare that `writer` (an effect, listener, timer, `on change`
    /// handler or handler site) writes `target` (a cell or event queue), so
    /// sinks reading `target` are ordered after `writer` from the first
    /// flush on, instead of after the first write is seen. The compiler
    /// knows every assignment a handler makes. A declaration that would
    /// close a loop (`writer` reads `target`, directly or through other
    /// writers) is a feedback edge: [`Error::Cycle`] names it and nothing
    /// is ranked.
    pub fn writes_to(&self, writer: NodeId, target: NodeId) -> Result<(), Error> {
        if !self.exists(writer) {
            return Err(Error::Disposed(writer));
        }
        if !self.exists(target) {
            return Err(Error::Disposed(target));
        }
        if self.learn(writer, target) {
            Ok(())
        } else {
            Err(Error::Cycle(Arc::new(
                self.path(vec![writer, target, writer]),
            )))
        }
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
        let mut queue = self.inner.learn_queue.borrow_mut();
        if !queue.contains(&(writer, target)) {
            queue.push((writer, target));
        }
    }

    /// Learn the write edges seen since the last call (the flush calls it
    /// before choosing the next sink).
    pub(crate) fn learn_queued(&self) {
        if self.inner.learn_queue.borrow().is_empty() {
            return;
        }
        let queue = std::mem::take(&mut *self.inner.learn_queue.borrow_mut());
        for (writer, target) in queue {
            if self.exists(writer) && self.exists(target) {
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
            w.ranked.push(target);
        } else {
            w.feedback.push(target);
        }
        w.prune(self);
        ok
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
        let top = {
            let nodes = self.inner.nodes.borrow();
            let Some(n) = nodes.get(id) else { return };
            let ranks = self.inner.ranks.borrow();
            n.sources
                .iter()
                .filter_map(|s| ranks.get(s).copied())
                .max()
                .unwrap_or(0)
        };
        for _ in 0..8 {
            if top <= self.rank_of(id) || self.raise(id, top, None) {
                return;
            }
            if !self.break_loop(id) {
                return;
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
        if let Ok(data) = self.data(n) {
            out.extend(data.downstream().into_iter().map(|l| (l, false)));
        }
    }

    /// Find a path from `id` to one of its own sources and unrank the
    /// write edges on it. Returns whether any edge was unranked.
    fn break_loop(&self, id: NodeId) -> bool {
        let sources: foldhash::HashSet<NodeId> = match self.inner.nodes.borrow().get(id) {
            Some(n) => n.sources.iter().copied().collect(),
            None => return false,
        };
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
                let mut demoted = false;
                let mut cur = m;
                let mut writes = self.inner.writes.borrow_mut();
                while cur != id {
                    let (p, write) = parent[&cur];
                    if write && let Some(w) = writes.get_mut(&p) {
                        w.ranked.retain(|&t| t != cur);
                        w.feedback.push(cur);
                        demoted = true;
                    }
                    cur = p;
                }
                if demoted {
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
