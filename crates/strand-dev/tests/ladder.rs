//! The guide's ladder test (m5-plan, S-docs; spec.md, section 14: one of
//! the conformance tests that hold the freeze). design.md's learning
//! ladder says nothing at level N+1 appears in a level-N example, and
//! decisions.md m5-docs places every construct the ladder's "You learn"
//! column does not name. This file holds the guide to both:
//!
//! - `no_guide_block_uses_a_construct_above_its_level`: every step is
//!   applied in page order (as `guide.rs` does), the config is compiled,
//!   and the HIR of the file the step wrote is walked; each construct it
//!   uses must be placed at the block's level or below.
//! - `each_construct_is_taught_at_its_level`: the levels build on each
//!   other in order. A construct's first step is at its own level (never
//!   later: then its placement would be wrong), or, for the few taught in
//!   prose, its level's page names it in inline code.
//! - `the_ladder_is_the_guides_table`: [`LADDER`] is the guide README's
//!   table: each construct is named on its level's row.
//!
//! The walk classifies every HIR node it meets; one it cannot place fails
//! the test, so a new construct gets a placement before the guide uses it.

mod support;

use std::collections::BTreeMap;
use std::path::Path;

use strand_compiler::hir::{
    self, AssignOp, BinaryOp, Callee, ElementKind, Event, Expr, ExprKind, FileHir, Item,
    LambdaBody, Node, Pattern, PoseKind, Prop, StateDecl, StateInit, Stmt, StmtKind, TimerKind,
    UnaryOp,
};
use strand_compiler::ty::{Origin, Ty};
use strand_dev::workspace::{Analysis, ConfigKey, Input};
use support::guide;

/// Every construct: (its name here, its level, how the guide README's
/// table names it on that level's row). Levels 1-5 are design.md's
/// ladder; the placements of what its "You learn" column does not name
/// are decisions.md m5-docs', one paragraph each.
const LADDER: &[(&str, u32, &str)] = &[
    ("a surface (bar, panel, osd)", 1, "Surfaces"),
    ("a node", 1, "nodes"),
    ("a prop", 1, "props"),
    ("a service read", 1, "services as live values"),
    ("split", 1, "`split`"),
    ("a comma shorthand", 1, "comma shorthands"),
    ("?.", 1, "`?.`"),
    ("??", 1, "`??`"),
    ("an operator", 1, "operators"),
    ("a call", 1, "calls"),
    ("a $token", 2, "`$tokens`"),
    ("component", 2, "`component`"),
    ("for", 2, "`for`"),
    ("when", 2, "`when`"),
    ("on click", 2, "`on click`"),
    ("on secondary", 2, "`on secondary`"),
    ("hit:", 2, "`hit:`"),
    ("a node boolean (hover, pressed)", 2, "`hover`"),
    ("an event handler", 2, "`on click`"),
    ("state", 3, "`state`"),
    ("<->", 3, "`<->`"),
    ("if", 3, "`if`"),
    ("popup", 3, "`popup`"),
    ("enter", 3, "`enter`"),
    ("assignment", 3, "`=`"),
    ("compound assignment", 3, "`+=`"),
    ("else", 3, "`else`"),
    ("exit", 3, "`exit`"),
    ("on scroll(dy)", 3, "`on scroll(dy)`"),
    ("an event's parameters", 3, "`on scroll(dy)`"),
    ("on change", 4, "`on change … after`"),
    ("after … while", 4, "`after … while`"),
    ("persist", 4, "`persist`"),
    ("export", 4, "`export`"),
    ("list", 4, "`list`"),
    ("Async", 4, "`Async`"),
    ("let", 4, "`let`"),
    ("enum", 4, "`enum`"),
    ("match", 4, "`match`"),
    ("every … while", 4, "`every … while`"),
    ("on show", 4, "`on show`"),
    ("id:", 4, "`id:`"),
    ("@reset", 4, "`@reset`"),
    ("a lambda", 4, "lambdas"),
    ("await", 4, "`await`"),
    ("the lock surface", 4, "`lock`"),
    ("tokens", 5, "`tokens`"),
    ("~", 5, "spring overrides `~`"),
    ("service from dbus", 5, "`service from dbus`"),
    ("pages", 5, "`pages`"),
    ("drag and drop", 5, "drag and drop"),
    ("a shader", 5, "shaders"),
    ("file", 5, "`file`"),
    ("set { }", 5, "`set { }`"),
    ("keyframes", 5, "`keyframes`"),
    ("play", 5, "`play`"),
    ("canvas", 5, "`canvas`"),
    ("type", 5, "`type`"),
    ("key", 5, "`key`"),
    ("slot", 5, "`slot`"),
    ("fn", 5, "`fn`"),
    ("a settings file", 5, "settings files"),
    ("an SVG layer selector", 5, "SVG layer selectors"),
    ("service from file", 5, "`service … from file`"),
    ("service from dir", 5, "`from dir`"),
    ("service from listen", 5, "`from listen`"),
    ("service from poll", 5, "`from poll`"),
    ("permit exec", 5, "`permit exec`"),
    ("action", 5, "`action`"),
];

/// The constructs the guide teaches in prose at their level and first
/// writes in a step at a later one, or never (decisions.md m5-docs): their
/// level's page shows them in inline code instead, as the second text.
const PROSE: &[(&str, &str)] = &[
    // Level 3 says `if` can have an `else { … }`; level 4's lock screen is
    // the first build to write one.
    ("else", "`else { … }`"),
    // Level 3 names `exit`; level 4's toasts are the first to write one.
    ("exit", "`exit { … }`"),
    // Level 4 shows `@reset` as inline code only (one attribute, not a
    // step that prints a file twice more).
    ("@reset", "`@reset`"),
    // Level 5's escape hatches are prose: `canvas`'s paint order is not
    // pinned down, a shader needs a `.wgsl` file of the reader's, and an
    // SVG selector an SVG with named layers.
    ("canvas", "`canvas { draw:"),
    ("a shader", "`shader \"aurora.wgsl\""),
    ("an SVG layer selector", "#needle {"),
    // Level 5: a directory of the reader's own (their wallpapers) is
    // what `from dir` lists.
    ("service from dir", "`service walls from dir"),
    // Level 5: `from listen`'s common commands are compositor-specific.
    ("service from listen", "`service player from listen"),
    // Level 5: an action's command is the reader's own (a compositor's
    // dispatch, an app of theirs), not one the guide's build can run.
    ("action", "`action dispatch("),
];

fn prose(name: &str) -> Option<&'static str> {
    PROSE.iter().find(|(n, _)| *n == name).map(|(_, c)| *c)
}

fn level_of(name: &str) -> u32 {
    LADDER
        .iter()
        .find(|(n, ..)| *n == name)
        .map(|(_, l, _)| *l)
        .unwrap_or_else(|| panic!("{name:?} is not in LADDER"))
}

/// A construct a step uses, and where.
#[derive(Debug, Clone)]
struct Use {
    construct: &'static str,
    line: usize,
}

/// The HIR walk: every construct in one file, by line.
struct Walk<'a> {
    program: &'a hir::Program,
    text: &'a str,
    out: Vec<Use>,
}

impl Walk<'_> {
    fn at(&mut self, construct: &'static str, span: strand_compiler::syntax::Span) {
        let _ = level_of(construct);
        let line = self.text[..(span.start as usize).min(self.text.len())]
            .matches('\n')
            .count()
            + 1;
        self.out.push(Use { construct, line });
    }

    fn file(&mut self, f: &FileHir) {
        for item in &f.items {
            self.item(item);
        }
        for e in &f.outputs {
            self.element(e);
        }
    }

    fn def_span(&self, d: hir::DefId) -> strand_compiler::syntax::Span {
        self.program.def(d).span
    }

    fn item(&mut self, item: &Item) {
        match item {
            Item::Component(c) => {
                self.at("component", self.def_span(c.def));
                if !c.tokens.is_empty() {
                    self.at("tokens", self.def_span(c.def));
                    for t in &c.tokens {
                        self.expr(&t.value);
                    }
                }
                for p in &c.params {
                    if let Some(d) = &p.default {
                        self.expr(d);
                    }
                }
                self.nodes(&c.body);
            }
            Item::Surface(s) => {
                let span = s.element.span;
                match &s.element.kind {
                    ElementKind::Builtin(k) if k == "lock" => self.at("the lock surface", span),
                    _ => self.at("a surface (bar, panel, osd)", span),
                }
                self.element_body(&s.element);
            }
            Item::State(s) => self.state(s),
            Item::Let(l) => {
                self.at("let", self.def_span(l.def));
                if l.reset {
                    self.at("@reset", self.def_span(l.def));
                }
                self.export(l.def);
                self.expr(&l.value);
            }
            Item::Fn(f) => {
                self.at("fn", self.def_span(f.def));
                self.stmts(&f.body);
            }
            Item::Enum(d) => self.at("enum", self.def_span(*d)),
            Item::Type(d) => self.at("type", self.def_span(*d)),
            Item::Tokens(t) => {
                self.at("tokens", self.def_span(t.def));
                for e in &t.entries {
                    self.expr(&e.value);
                }
            }
            Item::Use(u) => {
                for e in u.tokens.iter().chain(&u.palette) {
                    self.at("tokens", e.span);
                    self.expr(e);
                }
            }
            Item::Service(s) => {
                let c = match s.source.as_str() {
                    "dbus" => "service from dbus",
                    "file" => "service from file",
                    "dir" => "service from dir",
                    "listen" => "service from listen",
                    "poll" => "service from poll",
                    other => panic!("a `service … from {other}`"),
                };
                self.at(c, s.source_span);
                self.export(s.def);
                for e in s.args.iter().chain(&s.every) {
                    self.expr(e);
                }
            }
            Item::Keyframes(k) => {
                self.at("keyframes", self.def_span(k.def));
                for p in k.props.iter().chain(k.stops.iter().flat_map(|s| &s.props)) {
                    self.prop(p);
                }
            }
            Item::Handler(h) => self.handler(h),
            Item::Timer(t) => self.timer(t),
            Item::Action(a) => self.at("action", a.span),
            Item::Permit(_) => {
                // A permit has no span of its own; the service it allows
                // is the one beside it.
                let line = self.text.find("permit exec").unwrap_or(0);
                self.at(
                    "permit exec",
                    strand_compiler::syntax::Span::at(line as u32),
                );
            }
        }
    }

    fn export(&mut self, d: hir::DefId) {
        if self.program.def(d).exported {
            self.at("export", self.def_span(d));
        }
    }

    fn state(&mut self, s: &StateDecl) {
        let span = self.def_span(s.def);
        self.export(s.def);
        if s.persist {
            self.at("persist", span);
        }
        if s.reset {
            self.at("@reset", span);
        }
        match &s.init {
            StateInit::Value { key, value } => {
                self.at("state", span);
                if key.is_some() {
                    self.at("key", span);
                }
                self.expr(value);
            }
            StateInit::File { fields, .. } => {
                self.at("a settings file", span);
                for f in fields {
                    if let Some(d) = &f.default {
                        self.expr(d);
                    }
                }
            }
        }
    }

    fn nodes(&mut self, nodes: &[Node]) {
        for n in nodes {
            self.node(n);
        }
    }

    fn node(&mut self, n: &Node) {
        match n {
            Node::Element(e) => self.element(e),
            Node::When(w) => {
                self.at("when", w.span);
                self.expr(&w.cond);
                for p in &w.props {
                    self.prop(p);
                }
            }
            Node::If(i) => {
                self.at("if", i.span);
                if !i.else_.is_empty() {
                    self.at("else", i.span);
                }
                self.expr(&i.cond);
                self.nodes(&i.then);
                self.nodes(&i.else_);
            }
            Node::For(f) => {
                self.at("for", f.span);
                if let Some(k) = &f.key {
                    self.at("key", k.span);
                    self.expr(k);
                }
                self.expr(&f.iter);
                self.nodes(&f.body);
            }
            Node::Match(m) => {
                self.at("match", m.span);
                self.expr(&m.scrutinee);
                for (p, body) in &m.arms {
                    self.pattern(p);
                    self.nodes(body);
                }
            }
            Node::Handler(h) => self.handler(h),
            Node::Timer(t) => self.timer(t),
            Node::Pose(p) => {
                self.at(
                    match p.kind {
                        PoseKind::Enter => "enter",
                        PoseKind::Exit => "exit",
                    },
                    p.span,
                );
                for q in &p.props {
                    self.prop(q);
                }
            }
            Node::Slot(span) => self.at("slot", *span),
            Node::Set(defs, span) => {
                self.at("set { }", *span);
                for d in defs {
                    self.expr(&d.value);
                }
            }
            Node::Selector(s) => {
                self.at("an SVG layer selector", s.span);
                for p in &s.props {
                    self.prop(p);
                }
            }
            Node::Play(e) => {
                self.at("play", e.span);
                self.expr(e);
            }
            Node::State(s) => self.state(s),
            Node::Let(l) => {
                self.at("let", self.def_span(l.def));
                if l.reset {
                    self.at("@reset", self.def_span(l.def));
                }
                self.expr(&l.value);
            }
        }
    }

    fn element(&mut self, e: &hir::Element) {
        match &e.kind {
            ElementKind::Builtin(k) => {
                let c = match k.as_str() {
                    "split" => "split",
                    "popup" => "popup",
                    "list" => "list",
                    "pages" | "page" => "pages",
                    "canvas" => "canvas",
                    "shader" => "a shader",
                    "file" => "file",
                    _ => "a node",
                };
                self.at(c, e.span);
            }
            ElementKind::Component(_) => self.at("component", e.span),
            ElementKind::Unknown(k) => panic!("an unknown element `{k}` checked"),
        }
        self.element_body(e);
    }

    fn element_body(&mut self, e: &hir::Element) {
        if e.id.is_some() {
            self.at("id:", e.span);
        }
        if let Some(a) = &e.arg {
            self.expr(a);
        }
        for p in &e.props {
            self.prop(p);
        }
        self.nodes(&e.children);
    }

    fn prop(&mut self, p: &Prop) {
        self.at("a prop", p.span);
        match p.name.as_str() {
            "hit" => self.at("hit:", p.span),
            "drag" => self.at("drag and drop", p.span),
            "shader" => self.at("a shader", p.span),
            _ => {}
        }
        if p.two_way {
            self.at("<->", p.span);
        }
        if let Some(t) = &p.transition {
            self.at("~", t.span);
            self.expr(t);
        }
        self.expr(&p.value);
        for q in &p.sub {
            self.prop(q);
        }
    }

    fn handler(&mut self, h: &hir::Handler) {
        match &h.event {
            Event::Element(name) => {
                self.at("an event handler", h.span);
                match name.as_str() {
                    "click" => self.at("on click", h.span),
                    "secondary" => self.at("on secondary", h.span),
                    "scroll" => self.at("on scroll(dy)", h.span),
                    "show" | "hide" => self.at("on show", h.span),
                    "drop" | "drag_enter" | "drag_leave" => self.at("drag and drop", h.span),
                    _ => {}
                }
                if !h.params.is_empty() {
                    self.at("an event's parameters", h.span);
                }
            }
            Event::Service { .. } => {
                self.at("an event handler", h.span);
                if !h.params.is_empty() {
                    self.at("an event's parameters", h.span);
                }
            }
            Event::Change { targets, debounce } => {
                self.at("on change", h.span);
                for e in targets.iter().chain(debounce) {
                    self.expr(e);
                }
            }
        }
        self.stmts(&h.body);
    }

    fn timer(&mut self, t: &hir::Timer) {
        self.at(
            match t.kind {
                TimerKind::After => "after … while",
                TimerKind::Every => "every … while",
            },
            t.span,
        );
        self.expr(&t.duration);
        if let Some(w) = &t.while_ {
            self.expr(w);
        }
        self.stmts(&t.body);
    }

    fn pattern(&mut self, p: &Pattern) {
        if let Pattern::Literal(e) = p {
            self.expr(e);
        }
    }

    fn stmts(&mut self, stmts: &[Stmt]) {
        for s in stmts {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::Let { value, .. } => {
                self.at("let", s.span);
                self.expr(value);
            }
            StmtKind::Assign { target, op, value } => {
                self.at(
                    match op {
                        AssignOp::Set => "assignment",
                        _ => "compound assignment",
                    },
                    s.span,
                );
                self.expr(target);
                self.expr(value);
            }
            StmtKind::Expr(e) => self.expr(e),
            StmtKind::If { cond, then, else_ } => {
                self.at("if", s.span);
                if !else_.is_empty() {
                    self.at("else", s.span);
                }
                self.expr(cond);
                self.stmts(then);
                self.stmts(else_);
            }
            StmtKind::For { iter, body, .. } => {
                self.at("for", s.span);
                self.expr(iter);
                self.stmts(body);
            }
            StmtKind::Match { scrutinee, arms } => {
                self.at("match", s.span);
                self.expr(scrutinee);
                for (p, body) in arms {
                    self.pattern(p);
                    self.stmts(body);
                }
            }
            StmtKind::Play(e) => {
                self.at("play", s.span);
                self.expr(e);
            }
            StmtKind::Error => panic!("a statement that did not check"),
        }
    }

    fn expr(&mut self, e: &Expr) {
        if matches!(e.ty, Ty::Async(_)) {
            self.at("Async", e.span);
        }
        match &e.kind {
            ExprKind::Number { .. }
            | ExprKind::Text(_)
            | ExprKind::Color(_)
            | ExprKind::Bool(_)
            | ExprKind::Null
            | ExprKind::Local(_)
            | ExprKind::Value(_) => {}
            ExprKind::Def(d) => {
                if let hir::DefKind::Fn = self.program.def(*d).kind {
                    self.at("fn", e.span);
                }
            }
            ExprKind::Service(_) => self.at("a service read", e.span),
            ExprKind::Node(_) => {}
            ExprKind::Variant(id, _) | ExprKind::EnumType(id) => {
                if let Origin::User(..) = self.program.types.enum_(*id).origin {
                    self.at("enum", e.span);
                }
            }
            ExprKind::Token(_) => self.at("a $token", e.span),
            ExprKind::Field {
                base,
                name,
                optional,
            } => {
                if *optional {
                    self.at("?.", e.span);
                }
                if let ExprKind::Node(_) = base.kind {
                    match name.as_str() {
                        "hover" | "pressed" | "focused" | "selected" => {
                            self.at("a node boolean (hover, pressed)", e.span)
                        }
                        other => panic!("a node's `{other}` read: place it in LADDER"),
                    }
                }
                self.expr(base);
            }
            ExprKind::Call { callee, args } => {
                self.at("a call", e.span);
                match callee {
                    Callee::Fn(_) => self.at("fn", e.span),
                    Callee::Record(_) => self.at("type", e.span),
                    Callee::Value(f) => {
                        self.at("a lambda", e.span);
                        self.expr(f);
                    }
                    Callee::Method { receiver, .. } => self.expr(receiver),
                    Callee::Builtin { .. } => {}
                    Callee::Action(_) => self.at("action", e.span),
                    Callee::Error => panic!("a call that did not check"),
                }
                for a in args {
                    self.expr(&a.value);
                }
            }
            ExprKind::Index { base, index } => {
                self.at("an operator", e.span);
                self.expr(base);
                self.expr(index);
            }
            ExprKind::Unary { op, expr } => {
                self.at(
                    match op {
                        UnaryOp::Await => "await",
                        _ => "an operator",
                    },
                    e.span,
                );
                self.expr(expr);
            }
            ExprKind::Binary { op, lhs, rhs } => {
                // `??` is an operator too: level 1's first one.
                self.at("an operator", e.span);
                if let BinaryOp::Coalesce = op {
                    self.at("??", e.span);
                }
                self.expr(lhs);
                self.expr(rhs);
            }
            ExprKind::Ternary { cond, then, else_ } => {
                self.at("an operator", e.span);
                self.expr(cond);
                self.expr(then);
                self.expr(else_);
            }
            ExprKind::Lambda { body, .. } => {
                self.at("a lambda", e.span);
                match body {
                    LambdaBody::Expr(b) => self.expr(b),
                    LambdaBody::Block(b) => self.stmts(b),
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.at("match", e.span);
                self.expr(scrutinee);
                for (p, v) in arms {
                    self.pattern(p);
                    self.expr(v);
                }
            }
            ExprKind::List(xs) | ExprKind::Spaced(xs) => {
                for x in xs {
                    self.expr(x);
                }
            }
            ExprKind::Commas(xs) => {
                self.at("a comma shorthand", e.span);
                for x in xs {
                    self.expr(x);
                }
            }
            ExprKind::Error => panic!("an expression that did not check"),
        }
    }
}

/// A fresh directory for one test.
fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("strand-ladder-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// The constructs `file` (whose text is `text`) uses in the config in
/// `dir`, which must have no error; `at` names it in a failure.
fn uses_in(dir: &Path, file: &str, text: &str, at: &str) -> Vec<Use> {
    let an = analyse(dir);
    let errors: Vec<_> = an
        .compiled
        .diagnostics
        .iter()
        .filter(|d| d.is_error())
        .collect();
    assert!(errors.is_empty(), "{at}: {errors:?}");
    let stem = file.trim_end_matches(".strand");
    let hir = an
        .compiled
        .program
        .files
        .iter()
        .find(|f| f.name == stem)
        .unwrap_or_else(|| panic!("{at}: no HIR for {file}"));
    let mut walk = Walk {
        program: &an.compiled.program,
        text,
        out: Vec::new(),
    };
    walk.file(hir);
    walk.out
}

/// The constructs each step uses, in page order: (the block, what it
/// uses). The config after each step is compiled as `strand check` does
/// (`guide.rs` checks it has no diagnostic); the walk reads the HIR of
/// the file the step wrote.
fn steps(tag: &str) -> Vec<(guide::Block, Vec<Use>)> {
    let dir = scratch(tag);
    let mut out = Vec::new();
    for b in guide::blocks() {
        std::fs::write(dir.join(&b.file), &b.text).unwrap();
        let uses = if b.is_strand() {
            uses_in(&dir, &b.file, &b.text, &b.at)
        } else {
            Vec::new()
        };
        out.push((b, uses));
    }
    let _ = std::fs::remove_dir_all(&dir);
    out
}

fn analyse(dir: &Path) -> Analysis {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "strand"))
        .collect();
    files.sort();
    let inputs = files
        .iter()
        .map(|p| {
            Input::new(
                format!("file://{}", p.display()),
                p.display().to_string(),
                std::fs::read_to_string(p).unwrap().into(),
            )
        })
        .collect();
    Analysis::offline(
        ConfigKey::Dir(dir.to_path_buf()),
        inputs,
        strand_dev::server::schema(),
    )
}

#[test]
fn no_guide_block_uses_a_construct_above_its_level() {
    let steps = steps("above");
    assert!(steps.len() > 20, "the guide has {} steps", steps.len());
    let mut above = Vec::new();
    for (b, uses) in &steps {
        for u in uses {
            let level = level_of(u.construct);
            if level > b.level {
                above.push(format!(
                    "{} (level {}, {} line {}): {} is level {level}",
                    b.at, b.level, b.file, u.line, u.construct
                ));
            }
        }
    }
    above.dedup();
    assert!(
        above.is_empty(),
        "constructs above their block's level:\n{}",
        above.join("\n")
    );
}

#[test]
fn each_construct_is_taught_at_its_level() {
    // The first level each construct is written at.
    let mut first: BTreeMap<&str, (u32, String)> = BTreeMap::new();
    for (b, uses) in steps("first") {
        for u in uses {
            first
                .entry(u.construct)
                .or_insert_with(|| (b.level, b.at.clone()));
        }
    }
    let mut wrong = Vec::new();
    for &(name, level, _) in LADDER {
        let shown = prose(name);
        let prose = shown.is_some();
        match first.get(name) {
            Some((at, _)) if *at == level && !prose => {}
            Some((at, block)) if prose => {
                if *at < level {
                    wrong.push(format!("{name}: in PROSE but written at {block}"));
                }
            }
            Some((at, block)) => wrong.push(format!(
                "{name}: placed at level {level}, first written at level {at} ({block})"
            )),
            None if !prose => wrong.push(format!(
                "{name}: placed at level {level} and written in no step (if it is taught \
                 in prose, add it to PROSE)"
            )),
            None => {}
        }
        if let Some(shown) = shown {
            let page = guide::root().join(format!("docs/guide/level-{level}.md"));
            let text = std::fs::read_to_string(page).unwrap();
            if !text.contains(shown) {
                wrong.push(format!(
                    "{name}: taught in prose, but level-{level}.md never shows {shown}"
                ));
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn the_ladder_is_the_guides_table() {
    let readme = std::fs::read_to_string(guide::root().join("docs/guide/README.md")).unwrap();
    // Each level's rows, from both tables ("You learn" and "Also taught
    // here").
    let row = |level: u32| -> String {
        readme
            .lines()
            .filter(|l| l.starts_with(&format!("| {level} |")))
            .collect::<Vec<_>>()
            .join("\n")
    };
    for &(name, level, needle) in LADDER {
        assert!(
            row(level).contains(needle),
            "{name}: the README's level-{level} rows do not name it ({needle})"
        );
        for other in (1..=5).filter(|l| *l != level) {
            if needle.starts_with('`') && needle.ends_with('`') {
                assert!(
                    !row(other).contains(&format!("{needle};"))
                        && !row(other).contains(&format!("{needle} |"))
                        && !row(other).contains(&format!("{needle},")),
                    "{name}: the README names {needle} at level {other} too"
                );
            }
        }
    }
}

/// The walk sees what a level must not use: a level-2 shape with a
/// level-3 `state` and assignment, a level-4 `let` and lambda and a
/// level-5 `fn`, each on its line.
#[test]
fn the_walk_finds_constructs_above_a_level() {
    let dir = scratch("probe");
    let text = "fn twice(x: float) -> float { x * 2 }

component Dot(ws: Workspace) {
  state open = false
  let n = workspaces.all.filter(w => w.occupied).len
  box { size: twice(4)
    on click { open = !open }
  }
}

bar Top {
  for ws in workspaces.on(screen) { Dot ws }
}
";
    std::fs::write(dir.join("bar.strand"), text).unwrap();
    let uses = uses_in(&dir, "bar.strand", text, "the probe");
    let line = |c: &str| {
        uses.iter()
            .find(|u| u.construct == c)
            .map(|u| u.line)
            .unwrap_or_else(|| panic!("{c} not found in {uses:?}"))
    };
    assert_eq!(line("fn"), 1);
    assert_eq!(line("state"), 4);
    assert_eq!(line("let"), 5);
    assert_eq!(line("a lambda"), 5);
    assert_eq!(line("assignment"), 7);
    assert_eq!(line("for"), 12);
    let _ = std::fs::remove_dir_all(&dir);
}
