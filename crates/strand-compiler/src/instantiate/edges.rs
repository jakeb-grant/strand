//! Declared edges: the read and write sets the compiler sees in the
//! source, given to core as nodes are created (architecture.md,
//! "Lowering into strand-core").
//!
//! Every binding memo, `let`, derived list, effect, timer, `on change`
//! and listener declares its syntactic read set with `rt.reads_from`
//! (also when empty), and every handler its assignment targets with
//! `rt.writes_to`, so core ranks sinks before anything runs and each sink
//! runs once per flush with final values from the first flush on. The
//! names come from [`crate::lower::reads`]; this module resolves them to
//! core nodes in the scope a chunk is mounted in.

use std::rc::Rc;

use strand_core::{NodeId as CoreId, Runtime};

use super::Ctx;
use crate::lower::{ChunkId, WriteTarget};
use crate::vm::Env;
use crate::vm::value::Slot;

fn slot_ids(slot: Slot, out: &mut Vec<CoreId>) {
    match slot {
        Slot::Signal(s) => out.push(s.id()),
        Slot::Memo(m) => out.push(m.id()),
        Slot::Keyed(k, l) => {
            out.push(k.id());
            out.push(l.id());
        }
    }
}

impl Ctx {
    /// The core nodes `chunks` can read when evaluated in `env`.
    pub(crate) fn read_ids(
        self: &Rc<Self>,
        rt: &Runtime,
        chunks: &[ChunkId],
        env: &Rc<Env>,
    ) -> Vec<CoreId> {
        let prog = self.vm.prog.clone();
        let mut out = Vec::new();
        for &c in chunks {
            let r = prog.reads(c);
            for d in &r.defs {
                match env.settings(*d) {
                    Some(s) => out.extend(s.fields.iter().map(|(_, f)| f.id())),
                    None => {
                        if let Some(slot) = env.def(*d) {
                            slot_ids(slot, &mut out);
                        }
                    }
                }
            }
            for (d, f) in &r.fields {
                match env.settings(*d).and_then(|s| s.field(f)) {
                    Some(sig) => out.push(sig.id()),
                    None => {
                        if let Some(slot) = env.def(*d) {
                            slot_ids(slot, &mut out);
                        }
                    }
                }
            }
            for l in &r.locals {
                if let Some(slot) = env.local(*l) {
                    slot_ids(slot, &mut out);
                }
            }
            for (s, f) in &r.services {
                out.extend(self.vm.host.sources(rt, s, f.as_deref()));
            }
            for n in &r.nodes {
                let st = env.node_state(rt, *n);
                out.extend([
                    st.hover.id(),
                    st.pressed.id(),
                    st.focused.id(),
                    st.selected.id(),
                    st.width.id(),
                    st.height.id(),
                ]);
            }
        }
        out.sort();
        out.dedup();
        out.retain(|&id| rt.exists(id));
        out
    }

    /// Declare that `node` reads what `chunks` read in `env` (and
    /// `extra`): called for every binding and handler node, even with
    /// nothing to read, so core ranks it before its first run.
    pub(crate) fn declare_reads(
        self: &Rc<Self>,
        rt: &Runtime,
        node: CoreId,
        chunks: &[ChunkId],
        env: &Rc<Env>,
        extra: &[CoreId],
    ) {
        let mut ids = self.read_ids(rt, chunks, env);
        ids.extend(extra.iter().copied().filter(|&id| rt.exists(id)));
        let _ = rt.reads_from(node, &ids);
    }

    /// Declare that `writer` (a handler site, an `on change` effect, a
    /// timer) writes every assignment target of `body` in `env`. A
    /// feedback edge (the handler reads what it writes) is fine.
    pub(crate) fn declare_writes(
        self: &Rc<Self>,
        rt: &Runtime,
        writer: CoreId,
        body: ChunkId,
        env: &Rc<Env>,
    ) {
        let prog = self.vm.prog.clone();
        let mut targets = Vec::new();
        for w in prog.writes(body) {
            match w {
                WriteTarget::Def(d) => match env.settings(*d) {
                    Some(s) => targets.extend(s.fields.iter().map(|(_, f)| f.id())),
                    None => match env.def(*d) {
                        Some(Slot::Signal(s)) => targets.push(s.id()),
                        Some(Slot::Keyed(k, _)) => targets.push(k.id()),
                        _ => {}
                    },
                },
                WriteTarget::Field(d, f) => {
                    if let Some(sig) = env.settings(*d).and_then(|s| s.field(f)) {
                        targets.push(sig.id());
                    }
                }
                WriteTarget::Service(s, f) => {
                    targets.extend(self.vm.host.sources(rt, s, Some(f)));
                }
            }
        }
        for t in targets {
            let _ = rt.writes_to(writer, t);
        }
    }
}
