//! Merkle hashes over what a handler reaches: "did this handler change"
//! on reload (design.md, "What each edit does": changed handler code is
//! restarted, an in-flight `await` cancelled and reported; an unchanged
//! handler keeps running).
//!
//! A region's hash covers the texts of its significant tokens (comments
//! and whitespace do not count, so reformatting changes nothing) and the
//! hashes of the declarations it names that hold code: `fn`s, `let`s,
//! `type`s and `enum`s, keyframes, custom services and, for a `lock`
//! surface, the components it mounts, each in turn over what it names.

use std::collections::HashMap;

use crate::hir::{self, DefId, DefKind, Target};
use crate::lower::{self, Node};
use crate::source::{FileId, SourceMap};
use crate::syntax::Span;

use super::identity::Identity;

/// Region hashes of one program, by `(file, span)`.
#[derive(Clone, Debug, Default)]
pub struct Hashes {
    map: HashMap<(FileId, u32, u32), u64>,
    /// `lock` surfaces' hashes (their whole subtree and what it mounts).
    locks: Vec<u64>,
    /// Custom service declarations, by name.
    services: HashMap<String, u64>,
}

impl Hashes {
    /// The hash of the handler or timer written at `span` in `file`.
    pub fn get(&self, file: FileId, span: Span) -> Option<u64> {
        self.map.get(&(file, span.start, span.end)).copied()
    }

    /// One hash over every `lock` surface: a reload that changes it while
    /// a lock is shown waits for the unlock.
    pub fn locks(&self) -> u64 {
        let mut h = blake3::Hasher::new();
        for l in &self.locks {
            h.update(&l.to_le_bytes());
        }
        first_u64(h.finalize().as_bytes())
    }

    /// Custom services whose declaration differs from `old`'s (or is
    /// new).
    pub fn changed_services(&self, old: &Hashes) -> Vec<String> {
        let mut out: Vec<String> = self
            .services
            .iter()
            .filter(|(n, h)| old.services.get(*n) != Some(h))
            .map(|(n, _)| n.clone())
            .collect();
        out.sort();
        out
    }

    pub(crate) fn compute(
        id: &Identity,
        map: &SourceMap,
        hir: &hir::Program,
        prog: &lower::Program,
    ) -> Hashes {
        let mut refs: Vec<&hir::Reference> = hir.refs.iter().collect();
        refs.sort_by_key(|r| (r.file, r.span.start));
        let mut m = Merkle {
            id,
            map,
            hir,
            refs,
            defs: HashMap::new(),
            out: Hashes::default(),
        };
        for f in &prog.files {
            m.nodes(prog, &f.items);
        }
        for c in prog.components.values() {
            m.nodes(prog, &c.body.nodes);
        }
        for (i, d) in hir.defs.iter().enumerate() {
            if matches!(d.kind, DefKind::Service(_)) {
                let h = m.def(DefId(i as u32), false, &mut Vec::new());
                m.out.services.insert(d.name.clone(), h);
            }
        }
        m.out
    }
}

fn first_u64(b: &[u8; 32]) -> u64 {
    let mut x = [0u8; 8];
    x.copy_from_slice(&b[..8]);
    u64::from_le_bytes(x)
}

struct Merkle<'a> {
    id: &'a Identity,
    map: &'a SourceMap,
    hir: &'a hir::Program,
    refs: Vec<&'a hir::Reference>,
    /// Memoised declaration hashes (with and without components).
    defs: HashMap<(DefId, bool), u64>,
    out: Hashes,
}

impl Merkle<'_> {
    fn nodes(&mut self, prog: &lower::Program, nodes: &[Node]) {
        for n in nodes {
            match n {
                Node::Handler(h) => {
                    let file = prog.chunk(h.body).file;
                    let v = self.region(file, h.span, false, &mut Vec::new());
                    self.out.map.insert((file, h.span.start, h.span.end), v);
                }
                Node::Timer(t) => {
                    let v = self.region(t.file, t.span, false, &mut Vec::new());
                    self.out.map.insert((t.file, t.span.start, t.span.end), v);
                }
                Node::Surface(s) => {
                    if s.kind == strand_scene::NodeKind::Lock {
                        let e = &s.element;
                        let v = self.region(e.file, e.span, true, &mut Vec::new());
                        self.out.locks.push(v);
                    }
                    self.nodes(prog, &s.element.children);
                }
                Node::Element(e) => self.nodes(prog, &e.children),
                Node::For(f) => self.nodes(prog, &f.body.nodes),
                Node::If { then, else_, .. } => {
                    self.nodes(prog, then);
                    self.nodes(prog, else_);
                }
                Node::Match { arms, .. } => {
                    for a in arms {
                        self.nodes(prog, a);
                    }
                }
                _ => {}
            }
        }
    }

    /// The hash of `span` in `file` and of what it names.
    fn region(
        &mut self,
        file: FileId,
        span: Span,
        components: bool,
        stack: &mut Vec<DefId>,
    ) -> u64 {
        let mut h = blake3::Hasher::new();
        if let Some(f) = self.id.file(file) {
            for t in f.tokens_in(span) {
                h.update(f.texts[t].as_bytes());
                h.update(b"\0");
            }
        }
        let lo = self
            .refs
            .partition_point(|r| (r.file, r.span.start) < (file, span.start));
        let mut named: Vec<DefId> = Vec::new();
        for r in &self.refs[lo..] {
            if r.file != file || r.span.start >= span.end {
                break;
            }
            if let Target::Def(d) = r.target
                && !named.contains(&d)
            {
                named.push(d);
            }
        }
        for d in named {
            let kind = &self.hir.def(d).kind;
            let reach = matches!(
                kind,
                DefKind::Fn
                    | DefKind::Let
                    | DefKind::Enum(_)
                    | DefKind::Type(_)
                    | DefKind::Keyframes
                    | DefKind::Service(_)
            ) || (components && *kind == DefKind::Component);
            if reach {
                let v = self.def(d, components, stack);
                h.update(self.hir.def(d).name.as_bytes());
                h.update(&v.to_le_bytes());
            }
        }
        first_u64(h.finalize().as_bytes())
    }

    fn def(&mut self, d: DefId, components: bool, stack: &mut Vec<DefId>) -> u64 {
        if let Some(v) = self.defs.get(&(d, components)) {
            return *v;
        }
        // A recursive `fn` (or `let`s naming each other) counts by name
        // inside its own cycle.
        if stack.contains(&d) {
            return 0;
        }
        stack.push(d);
        let def = self.hir.def(d);
        let braced = !matches!(def.kind, DefKind::Let | DefKind::State);
        let v = match (self.id.file(def.file), self.map.get(def.file)) {
            (Some(f), Some(src)) => {
                let extent = f.decl_extent(&src.text, def.span.start, braced);
                self.region(def.file, extent, components, stack)
            }
            _ => 0,
        };
        stack.pop();
        self.defs.insert((d, components), v);
        v
    }
}
