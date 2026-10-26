//! Reload identity: which node of a new program is which node of the old
//! one (design.md, "Identity and state, in plain words").
//!
//! Every identifiable node of a lowered program (elements, surfaces,
//! component calls, `for`, `if`, `match`, handlers and timers) gets a
//! stable id, a [`Sid`]. The first program numbers them; each later
//! program inherits the old node's `Sid` where:
//!
//! 1. **Its source text maps to it.** The significant tokens of each file
//!    (comments and whitespace excluded) are diffed against the old file's
//!    (Myers). A new node whose first token is the image of an old node's
//!    first token, with the same kind, is that node, wherever it now sits:
//!    editing a node's own props, or wrapping it in a `row`, never resets
//!    it.
//! 2. **Its `id:` name** matches an unmatched old sibling of the same kind
//!    under the same (matched) parent.
//! 3. **Its position among unmatched siblings of the same kind** under the
//!    same parent, when as many such siblings are left on both sides.
//!
//! When unmatched siblings of one kind are left on both sides in
//! different numbers, Strand does not guess: the new ones start fresh and
//! a warning names them ([`Identity::warnings`]).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use crate::lower::{self, ElementKind, Event, Node};
use crate::source::{FileId, SourceMap};
use crate::syntax::Span;
use crate::syntax::lexer::{TokenKind, lex};

/// A node's identity across reloads.
pub type Sid = u64;

/// Where a skeleton node sits: at the top of a file, at the top of a
/// component's body, or under another node (in one of its arms: an `if`
/// has two, a `match` one per arm).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Group {
    File(String),
    Component(String),
    Child(usize, u16),
}

#[derive(Clone, Debug)]
struct SkelNode {
    /// What it is: `text`, a component's name, `bar`, `for`, `if`,
    /// `match`, `on click`, `after`.
    label: String,
    /// Index into [`Identity::files`].
    file: usize,
    span: Span,
    /// The token its span starts at.
    anchor: Option<usize>,
    group: Group,
    /// Its `id:` name.
    id_name: Option<String>,
    /// Hash of its own text: its significant tokens outside its
    /// children's spans.
    own: u64,
    sid: Sid,
}

/// One file's significant tokens.
#[derive(Clone, Debug)]
pub(crate) struct FileSkel {
    pub name: String,
    pub id: FileId,
    /// Hash of each significant token's text.
    toks: Vec<u64>,
    /// Byte offset each significant token starts at.
    starts: Vec<u32>,
    /// Byte offset each significant token ends at.
    ends: Vec<u32>,
    /// Each significant token's text.
    pub texts: Vec<std::sync::Arc<str>>,
    kinds: Vec<TokenKind>,
    /// Byte offset of each line's start.
    lines: Vec<u32>,
}

impl FileSkel {
    fn new(id: FileId, name: &str, text: &str) -> Self {
        let (tokens, _) = lex(text);
        let mut f = FileSkel {
            name: name.to_string(),
            id,
            toks: Vec::new(),
            starts: Vec::new(),
            ends: Vec::new(),
            texts: Vec::new(),
            kinds: Vec::new(),
            lines: std::iter::once(0)
                .chain(
                    text.bytes()
                        .enumerate()
                        .filter(|(_, b)| *b == b'\n')
                        .map(|(i, _)| i as u32 + 1),
                )
                .collect(),
        };
        for t in tokens {
            if t.kind.is_trivia() || t.kind == TokenKind::Eof {
                continue;
            }
            let s = text
                .get(t.span.start as usize..t.span.end as usize)
                .unwrap_or("");
            let mut h = std::collections::hash_map::DefaultHasher::new();
            s.hash(&mut h);
            f.toks.push(h.finish());
            f.starts.push(t.span.start);
            f.ends.push(t.span.end);
            f.texts.push(s.into());
            f.kinds.push(t.kind);
        }
        f
    }

    /// The significant token starting at byte `offset` (or the first one
    /// after it).
    fn token_at(&self, offset: u32) -> Option<usize> {
        let i = self.starts.partition_point(|&s| s < offset);
        (i < self.starts.len()).then_some(i)
    }

    /// The significant tokens inside `span`, as indices.
    pub(crate) fn tokens_in(&self, span: Span) -> std::ops::Range<usize> {
        let a = self.starts.partition_point(|&s| s < span.start);
        let b = self.ends.partition_point(|&e| e <= span.end);
        a..b.max(a)
    }

    /// The end of the declaration whose name token starts at `offset`: a
    /// braced body (`fn f(x) { … }`, `type T { … }`) when `braced`, else
    /// the end of the statement (a line break or `;` outside brackets).
    pub(crate) fn decl_extent(&self, text: &str, offset: u32, braced: bool) -> Span {
        let Some(first) = self.token_at(offset) else {
            return Span::new(offset, offset);
        };
        let mut depth = 0i32;
        let mut seen_brace = false;
        let mut end = self.ends[first];
        for i in first..self.starts.len() {
            let k = self.kinds[i];
            // A line break between this token and the next ends a
            // statement outside brackets.
            if !braced && depth == 0 && i > first {
                let gap = text
                    .get(self.ends[i - 1] as usize..self.starts[i] as usize)
                    .unwrap_or("");
                if gap.contains('\n') || gap.contains('\r') {
                    break;
                }
            }
            match k {
                TokenKind::LBrace | TokenKind::LParen | TokenKind::LBracket => {
                    if k == TokenKind::LBrace {
                        seen_brace = true;
                    }
                    depth += 1;
                }
                TokenKind::RBrace | TokenKind::RParen | TokenKind::RBracket => depth -= 1,
                TokenKind::Semi if depth == 0 && !braced => {
                    break;
                }
                _ => {}
            }
            end = self.ends[i];
            if braced && seen_brace && depth <= 0 {
                break;
            }
            if depth < 0 {
                break;
            }
        }
        Span::new(offset, end)
    }
}

/// The group at the top of node `i`'s chain of parents.
fn root_group(nodes: &[SkelNode], mut i: usize) -> &Group {
    loop {
        match &nodes[i].group {
            Group::Child(p, _) => i = *p,
            g => return g,
        }
    }
}

/// The identity of every node of one program, derived from the previous
/// program's (see the module docs).
#[derive(Clone, Debug, Default)]
pub struct Identity {
    pub(crate) files: Vec<FileSkel>,
    nodes: Vec<SkelNode>,
    by_span: HashMap<(FileId, u32, u32), Sid>,
    next: Sid,
    warnings: Vec<String>,
    /// (M6) The files (modules) declaring each component name: a
    /// component whose old file no longer declares it keeps its body's
    /// identity in the one file that does now (moved, or pasted then
    /// cut).
    components: HashMap<String, Vec<String>>,
    /// (M6) Calls of each component, by its file and name (`a.Row`): a
    /// body is taken over only from a component that had calls by one
    /// that had none (pasted, then cut).
    calls: HashMap<String, usize>,
}

impl Identity {
    /// The identity of `prog` (compiled from `map`), inheriting `prev`'s
    /// where nodes match. `None`: a first boot, every node new.
    pub fn derive(prev: Option<&Identity>, map: &SourceMap, prog: &lower::Program) -> Identity {
        let mut id = Identity {
            next: prev.map_or(1, |p| p.next),
            ..Identity::default()
        };
        for (fid, f) in map.iter() {
            id.files.push(FileSkel::new(fid, &f.name, &f.text));
        }
        id.build(prog);
        id.own_hashes();
        match prev {
            Some(p) => id.inherit(p),
            None => {
                for n in &mut id.nodes {
                    n.sid = id.next;
                    id.next += 1;
                }
            }
        }
        for n in &id.nodes {
            let fid = id.files[n.file].id;
            id.by_span.insert((fid, n.span.start, n.span.end), n.sid);
        }
        id
    }

    /// The identity of the node written at `span` in `file`.
    pub fn sid(&self, file: FileId, span: Span) -> Option<Sid> {
        self.by_span.get(&(file, span.start, span.end)).copied()
    }

    /// Every identified node: its kind label (`text`, a component's
    /// name, `for`, `on click`), file, span and identity, in tree order.
    pub fn entries(&self) -> impl Iterator<Item = (&str, FileId, Span, Sid)> + '_ {
        self.nodes
            .iter()
            .map(|n| (n.label.as_str(), self.files[n.file].id, n.span, n.sid))
    }

    /// Nodes that matched more than one way and were reset.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// How many nodes it identifies.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub(crate) fn file(&self, id: FileId) -> Option<&FileSkel> {
        self.files.iter().find(|f| f.id == id)
    }

    fn file_index(&self, id: FileId) -> Option<usize> {
        self.files.iter().position(|f| f.id == id)
    }

    /// (M6) For each component name declared in one file now and, before,
    /// in that file (with no calls) and exactly one other that no longer
    /// declares it (with calls): pasted, then cut. Match the new body's nodes to the cut body's
    /// through the token diff between the cut file's old text and the
    /// new file's text.
    fn take_over_moved_bodies(
        &self,
        prev: &Identity,
        matched: &mut [Option<usize>],
        taken: &mut [bool],
    ) {
        for (name, now) in &self.components {
            let [m] = now.as_slice() else { continue };
            let Some(before) = prev.components.get(name) else {
                continue;
            };
            if !before.contains(m) {
                continue;
            }
            let gone: Vec<&String> = before.iter().filter(|b| *b != m).collect();
            let [g] = gone.as_slice() else { continue };
            // Only the paste-then-cut shape: the survivor was not called
            // before (a fresh copy) and the cut one was. Undoing a paste,
            // or deleting one of two components in use, keeps the
            // survivor's own body.
            let called = |module: &str| {
                prev.calls
                    .get(&format!("{module}.{name}"))
                    .is_some_and(|n| *n > 0)
            };
            if called(m) || !called(g) {
                continue;
            }
            let file_of = |files: &[FileSkel], module: &str| {
                files
                    .iter()
                    .position(|f| crate::module_name(&f.name) == module)
            };
            let (Some(fi), Some(ofi)) = (file_of(&self.files, m), file_of(&prev.files, g)) else {
                continue;
            };
            let new_group = Group::Component(format!("{m}.{name}"));
            let old_group = Group::Component(format!("{g}.{name}"));
            let image = token_image(&prev.files[ofi].toks, &self.files[fi].toks);
            let mut old_at: HashMap<(usize, &str), usize> = HashMap::new();
            for (j, o) in prev.nodes.iter().enumerate() {
                if o.file == ofi
                    && let Some(a) = o.anchor
                    && *root_group(&prev.nodes, j) == old_group
                {
                    old_at.insert((a, o.label.as_str()), j);
                }
            }
            for (i, node) in self.nodes.iter().enumerate() {
                if node.file != fi
                    || matched[i].is_some()
                    || *root_group(&self.nodes, i) != new_group
                {
                    continue;
                }
                let Some(a) = node.anchor else { continue };
                let Some(oa) = image[a] else { continue };
                if let Some(&j) = old_at.get(&(oa, node.label.as_str()))
                    && !taken[j]
                {
                    matched[i] = Some(j);
                    taken[j] = true;
                }
            }
        }
    }

    // ---------------------------------------------------------------
    // The skeleton

    fn build(&mut self, prog: &lower::Program) {
        for f in &prog.files {
            let g = Group::File(f.module.clone());
            self.walk(prog, &f.items, &g);
        }
        for c in prog.components.values() {
            // (M6) By file and name: a name becoming shared keeps it
            // (and a unique one moving to another file, in `inherit`).
            let g = Group::Component(prog.owner_key(c.def));
            let d = prog.def(c.def);
            self.components
                .entry(d.name.clone())
                .or_default()
                .push(d.module.clone());
            self.walk(prog, &c.body.nodes, &g);
        }
    }

    /// Each node's own-text hash.
    fn own_hashes(&mut self) {
        let mut kids: Vec<Vec<Span>> = vec![Vec::new(); self.nodes.len()];
        for n in &self.nodes {
            if let Group::Child(p, _) = n.group {
                kids[p].push(n.span);
            }
        }
        for (i, kid_spans) in kids.iter().enumerate() {
            let n = &self.nodes[i];
            let f = &self.files[n.file];
            let mut h = std::collections::hash_map::DefaultHasher::new();
            n.label.hash(&mut h);
            for t in f.tokens_in(n.span) {
                let s = f.starts[t];
                if kid_spans.iter().any(|k| k.start <= s && s < k.end) {
                    continue;
                }
                f.toks[t].hash(&mut h);
            }
            self.nodes[i].own = h.finish();
        }
    }

    fn push(
        &mut self,
        label: String,
        file: FileId,
        span: Span,
        group: &Group,
        id_name: Option<String>,
    ) -> Option<usize> {
        let fi = self.file_index(file)?;
        let anchor = self.files[fi]
            .token_at(span.start)
            .filter(|&t| self.files[fi].starts[t] == span.start);
        self.nodes.push(SkelNode {
            label,
            file: fi,
            span,
            anchor,
            group: group.clone(),
            id_name,
            own: 0,
            sid: 0,
        });
        Some(self.nodes.len() - 1)
    }

    fn walk(&mut self, prog: &lower::Program, nodes: &[Node], group: &Group) {
        for n in nodes {
            match n {
                Node::Element(e) => self.element(prog, e, group, None),
                // A surface is one kind of node whatever its kind, so
                // `bar Top` edited to `panel Top` keeps its identity (and
                // its state); render recreates only the surface itself.
                Node::Surface(s) => self.element(prog, &s.element, group, Some("surface")),
                Node::For(f) => {
                    if let Some(i) = self.push("for".into(), f.file, f.span, group, None) {
                        self.walk(prog, &f.body.nodes, &Group::Child(i, 0));
                    }
                }
                Node::If {
                    then,
                    else_,
                    file,
                    span,
                    ..
                } => {
                    if let Some(i) = self.push("if".into(), *file, *span, group, None) {
                        self.walk(prog, then, &Group::Child(i, 0));
                        self.walk(prog, else_, &Group::Child(i, 1));
                    }
                }
                Node::Match {
                    arms, file, span, ..
                } => {
                    if let Some(i) = self.push("match".into(), *file, *span, group, None) {
                        for (a, arm) in arms.iter().enumerate() {
                            self.walk(prog, arm, &Group::Child(i, a as u16));
                        }
                    }
                }
                Node::Handler(h) => {
                    let label = match &h.event {
                        Event::Element(name) => format!("on {name}"),
                        Event::Service { service, event } => format!("on {service}.{event}"),
                        Event::Change { .. } => "on change".to_string(),
                    };
                    let file = prog.chunk(h.body).file;
                    self.push(label, file, h.span, group, None);
                }
                Node::Timer(t) => {
                    let label = match t.kind {
                        crate::hir::TimerKind::After => "after",
                        crate::hir::TimerKind::Every => "every",
                    };
                    self.push(label.into(), t.file, t.span, group, None);
                }
                Node::Slot
                | Node::State(_)
                | Node::Let { .. }
                | Node::When { .. }
                | Node::Pose { .. }
                | Node::Set(_)
                | Node::Play(_) => {}
            }
        }
    }

    fn element(
        &mut self,
        prog: &lower::Program,
        e: &lower::Element,
        group: &Group,
        label: Option<&str>,
    ) {
        let label = match label {
            Some(l) => l.to_string(),
            None => match &e.kind {
                ElementKind::Builtin(k) => k.name().to_string(),
                // The name as written: which file's it is shows in the
                // component's own group, so a call keeps its identity
                // when its component moves or its name becomes shared.
                ElementKind::Component(d) => {
                    *self.calls.entry(prog.owner_key(*d)).or_default() += 1;
                    prog.def(*d).name.clone()
                }

                ElementKind::Unknown(n) => format!("?{n}"),
                ElementKind::File(_) => "file".to_string(),
            },
        };
        let id_name = e.id.map(|l| prog.local(l).name.clone());
        if let Some(i) = self.push(label, e.file, e.span, group, id_name) {
            self.walk(prog, &e.children, &Group::Child(i, 0));
        }
    }

    // ---------------------------------------------------------------
    // Matching

    fn inherit(&mut self, prev: &Identity) {
        let n = self.nodes.len();
        let mut matched: Vec<Option<usize>> = vec![None; n];
        let mut taken: Vec<bool> = vec![false; prev.nodes.len()];
        // (M6) A component pasted into another file and then cut from its
        // own: the one declaration left takes over the cut one's body (its
        // mounted nodes), not the pasted copy's, by the token diff between
        // the two files.
        self.take_over_moved_bodies(prev, &mut matched, &mut taken);
        // 0. Unchanged text: a node whose own text (label and tokens
        // outside its children) appears as often before as now, in the
        // same file, is the one at the same place in tree order.
        let mut old_own: HashMap<(&str, u64), Vec<usize>> = HashMap::new();
        for (j, o) in prev.nodes.iter().enumerate() {
            old_own
                .entry((prev.files[o.file].name.as_str(), o.own))
                .or_default()
                .push(j);
        }
        let mut new_own: HashMap<(&str, u64), Vec<usize>> = HashMap::new();
        for (i, node) in self.nodes.iter().enumerate() {
            new_own
                .entry((self.files[node.file].name.as_str(), node.own))
                .or_default()
                .push(i);
        }
        for (k, news) in &new_own {
            if let Some(olds) = old_own.get(k)
                && olds.len() == news.len()
            {
                for (&i, &j) in news.iter().zip(olds) {
                    if matched[i].is_none() && !taken[j] {
                        matched[i] = Some(j);
                        taken[j] = true;
                    }
                }
            }
        }
        // 1. Source text: anchors through the token diff, per file name.
        let mut old_at: HashMap<(usize, usize, &str), usize> = HashMap::new();
        for (j, o) in prev.nodes.iter().enumerate() {
            if let Some(a) = o.anchor {
                old_at.insert((o.file, a, o.label.as_str()), j);
            }
        }
        for (fi, f) in self.files.iter().enumerate() {
            let Some(ofi) = prev.files.iter().position(|o| o.name == f.name) else {
                continue;
            };
            let old = &prev.files[ofi];
            let image = token_image(&old.toks, &f.toks);
            for (i, node) in self.nodes.iter().enumerate() {
                if node.file != fi || matched[i].is_some() {
                    continue;
                }
                let Some(a) = node.anchor else { continue };
                let Some(oa) = image[a] else { continue };
                if let Some(&j) = old_at.get(&(ofi, oa, node.label.as_str()))
                    && !taken[j]
                {
                    matched[i] = Some(j);
                    taken[j] = true;
                }
            }
        }
        // 2 and 3. `id:` names, then position among unmatched siblings of
        // the same kind, top-down (a parent matched here lets its
        // children match).
        let mut old_groups: HashMap<Group, Vec<usize>> = HashMap::new();
        for (j, o) in prev.nodes.iter().enumerate() {
            old_groups.entry(o.group.clone()).or_default().push(j);
        }
        let mut new_groups: Vec<(Group, Vec<usize>)> = Vec::new();
        let mut where_: HashMap<Group, usize> = HashMap::new();
        for (i, node) in self.nodes.iter().enumerate() {
            let k = match where_.get(&node.group) {
                Some(&k) => k,
                None => {
                    new_groups.push((node.group.clone(), Vec::new()));
                    where_.insert(node.group.clone(), new_groups.len() - 1);
                    new_groups.len() - 1
                }
            };
            new_groups[k].1.push(i);
        }
        let mut done = vec![false; new_groups.len()];
        for i in 0..n {
            let g = &self.nodes[i].group;
            let k = where_[g];
            if done[k] {
                continue;
            }
            done[k] = true;
            let counterpart = match g {
                Group::File(m) => Some(Group::File(m.clone())),
                Group::Component(c) => Some(Group::Component(c.clone())),
                Group::Child(p, a) => matched[*p].map(|q| Group::Child(q, *a)),
            };
            let Some(cg) = counterpart else { continue };
            let olds = match old_groups.get(&cg) {
                Some(o) => o,
                // A component moved to another file: the body of the one
                // old file that declared one of its name and no longer
                // does.
                None => match &cg {
                    Group::Component(k) => {
                        let name = k.rsplit_once('.').map_or(k.as_str(), |(_, n)| n);
                        let now = self.components.get(name).cloned().unwrap_or_default();
                        let mut gone = old_groups.iter().filter_map(|(g, o)| match g {
                            Group::Component(ok) => match ok.rsplit_once('.') {
                                Some((m, n)) if n == name && !now.iter().any(|x| x == m) => Some(o),
                                _ => None,
                            },
                            _ => None,
                        });
                        match (gone.next(), gone.next()) {
                            (Some(o), None) => o,
                            _ => continue,
                        }
                    }
                    _ => continue,
                },
            };

            let news = new_groups[k].1.clone();
            // By `id:` name.
            for &i2 in &news {
                if matched[i2].is_some() {
                    continue;
                }
                let Some(name) = &self.nodes[i2].id_name else {
                    continue;
                };
                if let Some(&j) = olds.iter().find(|&&j| {
                    !taken[j]
                        && prev.nodes[j].label == self.nodes[i2].label
                        && prev.nodes[j].id_name.as_ref() == Some(name)
                }) {
                    matched[i2] = Some(j);
                    taken[j] = true;
                }
            }
            // By position among what is left, kind by kind.
            let mut labels: Vec<&str> = Vec::new();
            for &i2 in &news {
                let l = self.nodes[i2].label.as_str();
                if matched[i2].is_none() && !labels.contains(&l) {
                    labels.push(l);
                }
            }
            for label in labels {
                let new_left: Vec<usize> = news
                    .iter()
                    .copied()
                    .filter(|&x| matched[x].is_none() && self.nodes[x].label == label)
                    .collect();
                let old_left: Vec<usize> = olds
                    .iter()
                    .copied()
                    .filter(|&j| !taken[j] && prev.nodes[j].label == label)
                    .collect();
                if old_left.is_empty() {
                    continue;
                }
                if old_left.len() == new_left.len() {
                    for (a, b) in new_left.iter().zip(&old_left) {
                        matched[*a] = Some(*b);
                        taken[*b] = true;
                    }
                } else {
                    let first = &self.nodes[new_left[0]];
                    let f = &self.files[first.file];
                    self.warnings.push(format!(
                        "{}: `{label}` is ambiguous ({} before, {} now); {} reset",
                        f.name_at(first.span.start),
                        old_left.len(),
                        new_left.len(),
                        if new_left.len() == 1 {
                            "it is".to_string()
                        } else {
                            format!("{} are", new_left.len())
                        }
                    ));
                }
            }
        }
        for (i, m) in matched.iter().enumerate() {
            self.nodes[i].sid = match m {
                Some(j) => prev.nodes[*j].sid,
                None => {
                    self.next += 1;
                    self.next - 1
                }
            };
        }
    }
}

impl FileSkel {
    /// `file:line` of a byte offset.
    fn name_at(&self, offset: u32) -> String {
        let line = self.lines.partition_point(|&s| s <= offset);
        format!("{}:{line}", self.name)
    }
}

/// For each token of `new`, the token of `old` it is (unchanged text kept
/// by the diff), if any.
fn token_image(old: &[u64], new: &[u64]) -> Vec<Option<usize>> {
    let mut image = vec![None; new.len()];
    // Most saves change one region: match the common prefix and suffix
    // first and diff only the middle.
    let mut pre = 0;
    while pre < old.len() && pre < new.len() && old[pre] == new[pre] {
        image[pre] = Some(pre);
        pre += 1;
    }
    let mut suf = 0;
    while suf < old.len() - pre
        && suf < new.len() - pre
        && old[old.len() - 1 - suf] == new[new.len() - 1 - suf]
    {
        image[new.len() - 1 - suf] = Some(old.len() - 1 - suf);
        suf += 1;
    }
    let (om, nm) = (&old[pre..old.len() - suf], &new[pre..new.len() - suf]);
    if om.is_empty() || nm.is_empty() {
        return image;
    }
    let deadline = strand_scene::time::Instant::now() + std::time::Duration::from_millis(50);
    let ops =
        similar::capture_diff_slices_deadline(similar::Algorithm::Myers, om, nm, Some(deadline));
    for op in ops {
        if let similar::DiffOp::Equal {
            old_index,
            new_index,
            len,
        } = op
        {
            for k in 0..len {
                image[pre + new_index + k] = Some(pre + old_index + k);
            }
        }
    }
    image
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(label: &str, group: Group, own: u64) -> SkelNode {
        SkelNode {
            label: label.into(),
            file: 0,
            span: Span::new(0, 0),
            anchor: None,
            group,
            id_name: None,
            own,
            sid: 0,
        }
    }

    fn skel(nodes: Vec<SkelNode>) -> Identity {
        Identity {
            files: vec![FileSkel::new(FileId(0), "bar.strand", "")],
            nodes,
            ..Identity::default()
        }
    }

    /// Unmatched siblings of one kind, three before and two now, with
    /// nothing in their text to tell them apart: the new ones start fresh
    /// and a warning says so. As many on both sides match by position.
    #[test]
    fn ambiguity_resets_with_a_warning() {
        let top = || Group::File("bar".into());
        let kid = || Group::Child(0, 0);
        let mut old = skel(vec![
            node("row", top(), 1),
            node("text", kid(), 2),
            node("text", kid(), 3),
            node("text", kid(), 4),
        ]);
        for (i, n) in old.nodes.iter_mut().enumerate() {
            n.sid = 10 + i as u64;
        }
        old.next = 20;
        let mut new = skel(vec![
            node("row", top(), 1),
            node("text", kid(), 5),
            node("text", kid(), 6),
        ]);
        new.next = old.next;
        new.inherit(&old);
        assert_eq!(new.nodes[0].sid, 10, "the row keeps its text");
        assert!(new.nodes[1].sid >= 20 && new.nodes[2].sid >= 20);
        assert_eq!(new.warnings.len(), 1, "{:?}", new.warnings);
        assert!(new.warnings[0].contains("`text` is ambiguous (3 before, 2 now)"));
        // Two and two: by position, no warning.
        let mut two = skel(vec![
            node("row", top(), 1),
            node("text", kid(), 7),
            node("text", kid(), 8),
        ]);
        two.next = new.next;
        two.inherit(&new);
        assert_eq!(
            two.nodes.iter().map(|n| n.sid).collect::<Vec<_>>(),
            new.nodes.iter().map(|n| n.sid).collect::<Vec<_>>()
        );
        assert!(two.warnings.is_empty());
    }

    #[test]
    fn token_image_keeps_unchanged_text() {
        let old = [1, 2, 3, 4, 5];
        let new = [1, 9, 2, 3, 4, 5];
        assert_eq!(
            token_image(&old, &new),
            [Some(0), None, Some(1), Some(2), Some(3), Some(4)]
        );
        let new = [3, 4, 5, 1, 2];
        let img = token_image(&old, &new);
        assert_eq!(img.iter().filter(|x| x.is_some()).count(), 3);
        assert_eq!(token_image(&[], &[1]), [None]);
        assert_eq!(token_image(&[1], &[]), Vec::<Option<usize>>::new());
    }
}
