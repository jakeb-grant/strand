//! Static cycles of components: `component C { box { C } }`, or `C` →
//! `D` → `C`, each mounting the next unconditionally, would mount forever.
//! A call under `if`/`else`, a `match` arm, a `for`, or an element that
//! mounts its children on demand (schema flag `on_demand`: a `popup`, a
//! `tooltip`, a `page`, since hidden pages unmount) breaks the cycle. A
//! component call's children count only when the callee mounts its `slot`
//! unconditionally by the same rules.

use std::collections::{HashMap, HashSet};

use super::Checker;
use crate::hir::{self, DefId, ElementKind, Node};
use crate::schema::Schema;
use crate::syntax::Span;

/// The unconditional component calls of `body`, in source order, and
/// whether it reaches its `slot` unconditionally. `slot` says which
/// components mount their own slot unconditionally.
fn walk(body: &[Node], schema: &Schema, slot: &HashMap<DefId, bool>) -> (Vec<(DefId, Span)>, bool) {
    let mut calls = Vec::new();
    let mut reaches_slot = false;
    let mut work: Vec<&Node> = body.iter().rev().collect();
    while let Some(n) = work.pop() {
        let e = match n {
            Node::Element(e) => e,
            Node::Slot(_) => {
                reaches_slot = true;
                continue;
            }
            // `if`, `match` and `for` mount on a condition; the rest hold
            // no elements.
            _ => continue,
        };
        let walk_children = match &e.kind {
            ElementKind::Component(d) => {
                calls.push((*d, e.span));
                slot.get(d).copied().unwrap_or(false)
            }
            ElementKind::Builtin(k) => !schema.element(k).is_some_and(|s| s.flags.on_demand),
            ElementKind::Unknown(_) => false,
        };
        if walk_children {
            work.extend(e.children.iter().rev());
        }
    }
    (calls, reaches_slot)
}

impl Checker<'_> {
    /// Reports each static cycle of component calls once, as
    /// `check::cycle` naming the path.
    pub(super) fn component_cycles(&mut self, files: &[hir::FileHir]) {
        let mut bodies: Vec<(DefId, &[Node])> = Vec::new();
        for f in files {
            for item in &f.items {
                if let hir::Item::Component(c) = item {
                    bodies.push((c.def, &c.body));
                }
            }
        }
        // Which components mount their slot unconditionally: a least
        // fixpoint, since a slot placed inside another component's call
        // mounts only if that one's slot does.
        let mut slot: HashMap<DefId, bool> = bodies.iter().map(|(d, _)| (*d, false)).collect();
        loop {
            let mut changed = false;
            for (def, body) in &bodies {
                if !slot[def] && walk(body, self.schema, &slot).1 {
                    slot.insert(*def, true);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        // Unconditional calls, in source order: (callee, call span).
        let edges: HashMap<DefId, Vec<(DefId, Span)>> = bodies
            .iter()
            .map(|(def, body)| (*def, walk(body, self.schema, &slot).0))
            .collect();
        // Iterative DFS: a back edge to a node on the stack closes a
        // cycle, read off the stack.
        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            New,
            Open,
            Done,
        }
        let mut mark: HashMap<DefId, Mark> = bodies.iter().map(|(d, _)| (*d, Mark::New)).collect();
        let mut cycles: Vec<(Vec<DefId>, Span)> = Vec::new();
        let mut seen: HashSet<Vec<DefId>> = HashSet::new();
        for (root, _) in &bodies {
            if mark[root] != Mark::New {
                continue;
            }
            // (component, next edge index); `path` mirrors the open nodes.
            let mut stack: Vec<(DefId, usize)> = vec![(*root, 0)];
            mark.insert(*root, Mark::Open);
            while let Some(&mut (d, ref mut i)) = stack.last_mut() {
                let Some(&(next, span)) = edges.get(&d).and_then(|es| es.get(*i)) else {
                    mark.insert(d, Mark::Done);
                    stack.pop();
                    continue;
                };
                *i += 1;
                match mark.get(&next).copied() {
                    Some(Mark::New) => {
                        mark.insert(next, Mark::Open);
                        stack.push((next, 0));
                    }
                    Some(Mark::Open) => {
                        let from = stack.iter().position(|(s, _)| *s == next).unwrap_or(0);
                        let path: Vec<DefId> = stack[from..].iter().map(|(s, _)| *s).collect();
                        // One report per cycle (`row { C; C }` closes it
                        // twice): keyed by its rotation from the least id.
                        let least = (0..path.len()).min_by_key(|&i| path[i].0).unwrap_or(0);
                        let mut canon = path[least..].to_vec();
                        canon.extend_from_slice(&path[..least]);
                        if seen.insert(canon) {
                            cycles.push((path, span));
                        }
                    }
                    _ => {}
                }
            }
        }
        for (path, span) in cycles {
            let names: Vec<String> = path
                .iter()
                .chain(path.first())
                .map(|d| format!("`{}`", self.defs[d.0 as usize].name))
                .collect();
            // The closing call is in the last component of the path.
            let Some(&last) = path.last() else { continue };
            let file = self.defs[last.0 as usize].file;
            let Some(module) = self.modules.iter().position(|m| m.file == file) else {
                continue;
            };
            let first = path[0];
            let (ff, fs, fname) = {
                let d = &self.defs[first.0 as usize];
                (d.file, d.span, d.name.clone())
            };
            let saved = std::mem::replace(&mut self.module, module);
            self.error(
                "check::cycle",
                format!("static cycle: {}", names.join(" → ")),
                span,
                format!("mounts `{fname}` again, unconditionally"),
            )
            .add_secondary(ff, fs, format!("the component `{fname}`"))
            .help = Some(
                "a component cannot always contain itself; render the inner one under an `if`, `for`, `match`, a `page`, a `tooltip` or a `popup`"
                    .into(),
            );
            self.module = saved;
        }
    }
}
