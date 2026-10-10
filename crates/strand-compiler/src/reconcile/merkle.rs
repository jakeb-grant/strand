//! Merkle hashes over what a handler reaches: "did this handler change"
//! on reload (design.md, "What each edit does": changed handler code is
//! restarted, an in-flight `await` cancelled and reported; an unchanged
//! handler keeps running).
//!
//! A region's hash covers the texts of its significant tokens (comments
//! and whitespace do not count, so reformatting changes nothing) and the
//! hashes of the declarations it names that hold code: `fn`s, `let`s,
//! `type`s and `enum`s, keyframes, custom services and, for a `lock`
//! surface, the components it mounts, each in turn over what it names,
//! and the checked code of each `shader` file it draws: a saved `.wgsl`
//! a lock uses is a lock edit, deferred until the unlock like one.

use std::collections::{HashMap, HashSet};

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

    /// Custom services `old` declared that are gone now.
    pub fn removed_services(&self, old: &Hashes) -> Vec<String> {
        let mut out: Vec<String> = old
            .services
            .keys()
            .filter(|n| !self.services.contains_key(*n))
            .cloned()
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
        let mut shaders = Vec::new();
        crate::check::shaders::each_shader(hir, &mut |file, e| {
            if let Some(hir::Expr {
                kind: hir::ExprKind::Text(path),
                ..
            }) = &e.arg
            {
                shaders.push((file, e.span.start, path.clone()));
            }
        });
        let mut m = Merkle {
            id,
            map,
            hir,
            refs,
            shaders,
            codes: &prog.shaders,
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
                let h = m.def(DefId(i as u32), false);
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

/// A declaration as a node of the def graph: with or without the
/// components it mounts (only a `lock` follows those).
type Key = (DefId, bool);

/// What one region holds: its significant tokens (hashed) and the
/// declarations it names that hold code, in order.
struct Scan {
    tokens: [u8; 32],
    named: Vec<DefId>,
}

/// A declaration being hashed (Tarjan's strongly connected components,
/// iteratively: a long chain of `fn`s does not grow the call stack).
struct Frame {
    key: Key,
    next: usize,
}

/// Tarjan's bookkeeping for one [`Merkle::def`] run.
#[derive(Default)]
struct Tarjan {
    /// Visit order and lowest reachable visit order, by declaration.
    index: HashMap<Key, (usize, usize)>,
    scans: HashMap<Key, Scan>,
    stack: Vec<Key>,
    on_stack: HashSet<Key>,
    frames: Vec<Frame>,
}

impl Tarjan {
    fn enter(&mut self, m: &Merkle<'_>, k: Key) {
        let i = self.index.len();
        self.index.insert(k, (i, i));
        self.scans.insert(k, m.scan_def(k));
        self.stack.push(k);
        self.on_stack.insert(k);
        self.frames.push(Frame { key: k, next: 0 });
    }
}

struct Merkle<'a> {
    id: &'a Identity,
    map: &'a SourceMap,
    hir: &'a hir::Program,
    refs: Vec<&'a hir::Reference>,
    /// Every `shader` node with a literal path: its file and start.
    shaders: Vec<(FileId, u32, String)>,
    /// The checked shader files, by path as written.
    codes: &'a crate::check::shaders::Shaders,
    /// Memoised declaration hashes (with and without components).
    defs: HashMap<Key, u64>,
    out: Hashes,
}

impl Merkle<'_> {
    fn nodes(&mut self, prog: &lower::Program, nodes: &[Node]) {
        for n in nodes {
            match n {
                Node::Handler(h) => {
                    let file = prog.chunk(h.body).file;
                    let v = self.region(file, h.span, false);
                    self.out.map.insert((file, h.span.start, h.span.end), v);
                }
                Node::Timer(t) => {
                    let v = self.region(t.file, t.span, false);
                    self.out.map.insert((t.file, t.span.start, t.span.end), v);
                }
                Node::Surface(s) => {
                    if s.kind == strand_scene::NodeKind::Lock {
                        let e = &s.element;
                        let v = self.region(e.file, e.span, true);
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

    /// The tokens of `span` in `file` and the code-holding declarations
    /// it names.
    fn scan(&self, file: FileId, span: Span, components: bool) -> Scan {
        let mut h = blake3::Hasher::new();
        if let Some(f) = self.id.file(file) {
            for t in f.tokens_in(span) {
                h.update(f.texts[t].as_bytes());
                h.update(b"\0");
            }
        }
        // A lock's region (and the components it mounts) covers the code
        // of the shader files it draws, not only their paths.
        if components {
            // In source order (`each_shader` walks each file in order);
            // few enough to scan whole, and no sort is linked for them.
            for (f, s, path) in &self.shaders {
                if *f != file || *s < span.start || *s >= span.end {
                    continue;
                }
                h.update(b"\x01shader\0");
                h.update(path.as_bytes());
                h.update(b"\0");
                match self.codes.get(path) {
                    Some(code) => {
                        h.update(code.wgsl.as_bytes());
                        h.update(b"\x01");
                    }
                    None => {
                        h.update(b"\x02");
                    }
                }
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
                    named.push(d);
                }
            }
        }
        Scan {
            tokens: *h.finalize().as_bytes(),
            named,
        }
    }

    /// The scan of a declaration's own text.
    fn scan_def(&self, (d, components): Key) -> Scan {
        let def = self.hir.def(d);
        let braced = !matches!(def.kind, DefKind::Let | DefKind::State);
        match (self.id.file(def.file), self.map.get(def.file)) {
            (Some(f), Some(src)) => {
                let extent = f.decl_extent(&src.text, def.span.start, braced);
                self.scan(def.file, extent, components)
            }
            _ => Scan {
                tokens: [0; 32],
                named: Vec::new(),
            },
        }
    }

    /// The hash of `span` in `file` and of what it names.
    fn region(&mut self, file: FileId, span: Span, components: bool) -> u64 {
        let scan = self.scan(file, span, components);
        let mut h = blake3::Hasher::new();
        h.update(&scan.tokens);
        for d in scan.named {
            let v = self.def(d, components);
            h.update(self.hir.def(d).name.as_bytes());
            h.update(&v.to_le_bytes());
        }
        first_u64(h.finalize().as_bytes())
    }

    /// A declaration's hash: its text and the hashes of what it names.
    /// Declarations that name each other (recursive `fn`s, `let`s naming
    /// each other) are one strongly connected component, hashed once
    /// over every member's text: each member's hash is the component's
    /// with its own name, so a change to any member changes them all.
    /// Linear in the def graph; every hash is memoised.
    fn def(&mut self, d: DefId, components: bool) -> u64 {
        let root = (d, components);
        if let Some(v) = self.defs.get(&root) {
            return *v;
        }
        // Tarjan's algorithm from `root` over declarations not hashed yet.
        let mut t = Tarjan::default();
        t.enter(self, root);
        while let Some(top) = t.frames.last_mut() {
            let v = top.key;
            let next = t.scans.get(&v).and_then(|s| s.named.get(top.next)).copied();
            if let Some(w) = next {
                top.next += 1;
                let w = (w, v.1);
                if self.defs.contains_key(&w) {
                    continue;
                }
                match t.index.get(&w).copied() {
                    None => t.enter(self, w),
                    Some((wi, _)) if t.on_stack.contains(&w) => {
                        if let Some(e) = t.index.get_mut(&v) {
                            e.1 = e.1.min(wi);
                        }
                    }
                    // Finished in this run: its component is hashed.
                    Some(_) => {}
                }
                continue;
            }
            t.frames.pop();
            let (vi, vl) = t.index.get(&v).copied().unwrap_or((0, 0));
            if let Some(parent) = t.frames.last()
                && let Some(e) = t.index.get_mut(&parent.key)
            {
                e.1 = e.1.min(vl);
            }
            if vl == vi {
                let mut members = Vec::new();
                while let Some(k) = t.stack.pop() {
                    t.on_stack.remove(&k);
                    members.push(k);
                    if k == v {
                        break;
                    }
                }
                self.hash_component(members, &t.scans);
            }
        }
        self.defs.get(&root).copied().unwrap_or(0)
    }

    /// Hash one strongly connected component (everything it names
    /// outside itself is hashed already).
    fn hash_component(&mut self, mut members: Vec<Key>, scans: &HashMap<Key, Scan>) {
        let named_of = |k: &Key| scans.get(k).map_or(&[][..], |s| &s.named[..]);
        let single = members.len() == 1 && !named_of(&members[0]).contains(&members[0].0);
        if single {
            let k = members[0];
            let mut h = blake3::Hasher::new();
            if let Some(s) = scans.get(&k) {
                h.update(&s.tokens);
            }
            for d in named_of(&k) {
                h.update(self.hir.def(*d).name.as_bytes());
                let v = self.defs.get(&(*d, k.1)).copied().unwrap_or(0);
                h.update(&v.to_le_bytes());
            }
            self.defs.insert(k, first_u64(h.finalize().as_bytes()));
            return;
        }
        // A stable order: by place in the sources.
        members.sort_by_key(|(d, _)| {
            let def = self.hir.def(*d);
            (def.file, def.span.start, d.0)
        });
        let mut h = blake3::Hasher::new();
        h.update(b"cycle\0");
        for k in &members {
            h.update(self.hir.def(k.0).name.as_bytes());
            h.update(b"\0");
            if let Some(s) = scans.get(k) {
                h.update(&s.tokens);
            }
            for d in named_of(k) {
                h.update(self.hir.def(*d).name.as_bytes());
                match members.iter().position(|m| m.0 == *d) {
                    // A member: by its place in the component.
                    Some(i) => {
                        h.update(b"\x01");
                        h.update(&(i as u64).to_le_bytes());
                    }
                    None => {
                        let v = self.defs.get(&(*d, k.1)).copied().unwrap_or(0);
                        h.update(b"\x02");
                        h.update(&v.to_le_bytes());
                    }
                }
            }
        }
        let component = *h.finalize().as_bytes();
        for (i, k) in members.iter().enumerate() {
            let mut h = blake3::Hasher::new();
            h.update(&component);
            h.update(&(i as u64).to_le_bytes());
            h.update(self.hir.def(k.0).name.as_bytes());
            self.defs.insert(*k, first_u64(h.finalize().as_bytes()));
        }
    }
}
