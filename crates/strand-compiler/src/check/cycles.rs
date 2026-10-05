//! Static cycles of components: `component C { box { C } }`, or `C` →
//! `D` → `C`, each mounting the next unconditionally, would mount forever.
//! A call under `if`/`else`, a `match` arm, a `for` or a `popup` (shown on
//! demand) breaks the cycle; a component call's children count only when
//! the callee has a `slot` to place them in.

use std::collections::HashMap;

use super::Checker;
use crate::hir::{self, DefId, ElementKind, Node};
use crate::syntax::Span;

impl Checker<'_> {
    /// Reports each static cycle of component calls once, as
    /// `check::cycle` naming the path.
    pub(super) fn component_cycles(&mut self, files: &[hir::FileHir]) {
        let mut slot: HashMap<DefId, bool> = HashMap::new();
        let mut bodies: Vec<(DefId, &[Node])> = Vec::new();
        for f in files {
            for item in &f.items {
                if let hir::Item::Component(c) = item {
                    slot.insert(c.def, c.has_slot);
                    bodies.push((c.def, &c.body));
                }
            }
        }
        // Unconditional calls, in source order: (callee, call span).
        let mut edges: HashMap<DefId, Vec<(DefId, Span)>> = HashMap::new();
        for (def, body) in &bodies {
            let mut out = Vec::new();
            let mut work: Vec<&Node> = body.iter().rev().collect();
            while let Some(n) = work.pop() {
                let Node::Element(e) = n else {
                    // `if`, `match` and `for` mount on a condition; the
                    // rest hold no elements.
                    continue;
                };
                let walk_children = match &e.kind {
                    ElementKind::Component(d) => {
                        out.push((*d, e.span));
                        slot.get(d).copied().unwrap_or(false)
                    }
                    ElementKind::Builtin(k) => k != "popup",
                    ElementKind::Unknown(_) => false,
                };
                if walk_children {
                    work.extend(e.children.iter().rev());
                }
            }
            edges.insert(*def, out);
        }
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
                        cycles.push((path, span));
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
                "a component cannot always contain itself; render the inner one under an `if` or `for`"
                    .into(),
            );
            self.module = saved;
        }
    }
}
