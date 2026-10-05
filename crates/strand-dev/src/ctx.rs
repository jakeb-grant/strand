//! Where in the syntax tree an offset is: which block, and the element
//! whose props that block holds. Used by completion, which works on text
//! that is being typed and may not parse cleanly.

use strand_compiler::syntax::Span;
use strand_compiler::syntax::ast::*;

/// The kind of block the cursor is in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    /// Between top-level declarations.
    TopLevel,
    /// A tree block: children, and props of `element` (an element kind,
    /// a component's name or a surface kind; `None` in a component body).
    Tree {
        element: Option<String>,
        /// `when`, poses and selectors hold props only.
        props_only: bool,
        /// A prop's sub-block (`stroke: … { cap: round }`): the prop.
        sub_of: Option<String>,
    },
    /// A handler, `fn` or lambda body.
    Statements,
    /// Token, field, enum, service and keyframe blocks.
    Other,
}

/// Where `offset` is, and the top-level item holding it.
#[derive(Clone, Debug)]
pub struct Context<'a> {
    pub block: Block,
    /// The top-level component or surface holding the offset.
    pub top: Option<&'a Item>,
}

/// The innermost block holding `offset` in `file` (`src` is its text).
pub fn context<'a>(file: &'a File, src: &str, offset: u32) -> Context<'a> {
    let mut f = Finder {
        src,
        offset,
        block: Block::TopLevel,
        element: None,
    };
    let top = file.items.iter().find(|it| f.inside_item(it));
    if let Some(it) = top {
        f.item(it);
    }
    Context {
        block: f.block,
        top: top.filter(|it| matches!(it.kind, ItemKind::Component(_) | ItemKind::Surface(_))),
    }
}

struct Finder<'s> {
    src: &'s str,
    offset: u32,
    block: Block,
    /// The element whose block we are in.
    element: Option<String>,
}

impl Finder<'_> {
    /// Strictly inside the braces of `span` (or after an unclosed `{`).
    fn inside(&self, span: Span) -> bool {
        let closed =
            span.end > span.start && self.src.as_bytes().get(span.end as usize - 1) == Some(&b'}');
        let opened = self.src.as_bytes().get(span.start as usize) == Some(&b'{');
        opened
            && span.start < self.offset
            && (self.offset < span.end || !closed && self.offset >= span.end)
    }

    fn inside_item(&self, it: &Item) -> bool {
        it.span.start <= self.offset && self.offset <= it.span.end
            || self.blocks_of(it).iter().any(|b| self.inside(*b))
    }

    /// The block spans an item opens.
    fn blocks_of(&self, it: &Item) -> Vec<Span> {
        match &it.kind {
            ItemKind::Component(c) => vec![c.body.span],
            ItemKind::Surface(s) => vec![s.body.span],
            ItemKind::Element(e) => e.block.iter().map(|b| b.span).collect(),
            _ => Vec::new(),
        }
    }

    fn tree(&mut self, items: &[Item], element: Option<String>, props_only: bool) {
        let saved = self.element.clone();
        self.element = element.clone();
        self.block = Block::Tree {
            element,
            props_only,
            sub_of: None,
        };
        for it in items {
            if it.span.start <= self.offset && self.offset <= it.span.end {
                self.item(it);
                break;
            }
        }
        self.element = saved;
    }

    fn stmts(&mut self, stmts: &[Stmt]) {
        self.block = Block::Statements;
        for s in stmts {
            if s.span.start <= self.offset && self.offset <= s.span.end {
                self.stmt(s);
                break;
            }
        }
    }

    fn item(&mut self, it: &Item) {
        let cur = self.element.clone();
        match &it.kind {
            ItemKind::Component(c) => {
                if c.tokens.as_ref().is_some_and(|t| self.inside(t.span)) {
                    self.block = Block::Other;
                } else if self.inside(c.body.span) {
                    self.tree(&c.body.items, None, false);
                } else if let Some(ps) = &c.params {
                    self.params(ps);
                }
            }
            ItemKind::Surface(s) => {
                if self.inside(s.body.span) {
                    self.tree(&s.body.items, Some(s.kind.name.clone()), false);
                }
            }
            ItemKind::Element(e) => {
                match &e.arg {
                    Some(HeadArg::Positional(x) | HeadArg::Named(_, x)) => self.expr(x),
                    None => {}
                }
                if let Some(b) = &e.block
                    && self.inside(b.span)
                {
                    self.tree(&b.items, Some(e.kind.name.clone()), false);
                }
            }
            ItemKind::Prop(p) => {
                self.expr(&p.value);
                if let Some(b) = &p.block
                    && self.inside(b.span)
                {
                    self.block = Block::Tree {
                        element: cur,
                        props_only: true,
                        sub_of: Some(p.name.name.clone()),
                    };
                }
            }
            ItemKind::When(w) => {
                self.expr(&w.cond);
                if self.inside(w.body.span) {
                    self.tree(&w.body.items, cur, true);
                }
            }
            ItemKind::Pose(p) if self.inside(p.body.span) => self.tree(&p.body.items, cur, true),
            ItemKind::Selector(s) if self.inside(s.body.span) => {
                self.tree(&s.body.items, cur, true)
            }
            ItemKind::If(i) => self.if_item(i, cur),
            ItemKind::For(f) => {
                self.expr(&f.iter);
                if self.inside(f.body.span) {
                    self.tree(&f.body.items, cur, false);
                }
            }
            ItemKind::Match(m) => {
                self.expr(&m.scrutinee);
                for arm in &m.arms {
                    match &arm.body {
                        ArmBody::Block(b) if self.inside(b.span) => {
                            self.tree(&b.items, cur.clone(), false)
                        }
                        ArmBody::Single(i)
                            if i.span.start <= self.offset && self.offset <= i.span.end =>
                        {
                            self.item(i)
                        }
                        _ => {}
                    }
                }
            }
            ItemKind::On(o) => {
                if self.inside(o.body.span) {
                    self.stmts(&o.body.items);
                }
            }
            ItemKind::Timer(t) => {
                self.expr(&t.duration);
                if self.inside(t.body.span) {
                    self.stmts(&t.body.items);
                }
            }
            ItemKind::Fn(f) => {
                if self.inside(f.body.span) {
                    self.stmts(&f.body.items);
                }
            }
            ItemKind::Let(l) => self.expr(&l.value),
            ItemKind::State(s) => {
                if let StateInit::Value { value, .. } = &s.init {
                    self.expr(value);
                } else {
                    self.block = Block::Other;
                }
            }
            ItemKind::Tokens(_)
            | ItemKind::Set(_)
            | ItemKind::Type(_)
            | ItemKind::Enum(_)
            | ItemKind::Service(_)
            | ItemKind::Keyframes(_)
                if self.blocks_any(it) =>
            {
                self.block = Block::Other;
            }
            _ => {}
        }
    }

    /// The offset is inside some block of `it` (not on its head).
    fn blocks_any(&self, it: &Item) -> bool {
        let src = self.src.as_bytes();
        let from = it.span.start as usize;
        let to = (self.offset as usize).min(src.len());
        src.get(from..to).is_some_and(|s| s.contains(&b'{'))
    }

    fn if_item(&mut self, i: &If<Item>, cur: Option<String>) {
        self.expr(&i.cond);
        if self.inside(i.then.span) {
            self.tree(&i.then.items, cur, false);
            return;
        }
        match &i.else_ {
            Some(Else::If(next, _)) => self.if_item(next, cur),
            Some(Else::Block(b)) if self.inside(b.span) => self.tree(&b.items, cur, false),
            _ => {}
        }
    }

    fn params(&mut self, ps: &[Param]) {
        for p in ps {
            if let Some(d) = &p.default {
                self.expr(d);
            }
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        match &s.kind {
            StmtKind::If(i) => self.if_stmt(i),
            StmtKind::For(f) => {
                if self.inside(f.body.span) {
                    self.stmts(&f.body.items);
                }
            }
            StmtKind::Match(m) => {
                for arm in &m.arms {
                    if let ArmBody::Block(b) = &arm.body
                        && self.inside(b.span)
                    {
                        self.stmts(&b.items);
                    }
                }
            }
            StmtKind::Let(l) => self.expr(&l.value),
            StmtKind::Assign { target, value, .. } => {
                self.expr(target);
                self.expr(value);
            }
            StmtKind::Expr(e) | StmtKind::Play(e) => self.expr(e),
            StmtKind::Error => {}
        }
    }

    fn if_stmt(&mut self, i: &If<Stmt>) {
        if self.inside(i.then.span) {
            self.stmts(&i.then.items);
            return;
        }
        match &i.else_ {
            Some(Else::If(next, _)) => self.if_stmt(next),
            Some(Else::Block(b)) if self.inside(b.span) => self.stmts(&b.items),
            _ => {}
        }
    }

    /// A lambda with a block body holding the offset.
    fn expr(&mut self, e: &Expr) {
        if !(e.span.start <= self.offset && self.offset <= e.span.end) {
            return;
        }
        match &e.kind {
            ExprKind::Lambda {
                body: LambdaBody::Block(b),
                ..
            } if self.inside(b.span) => self.stmts(&b.items),
            ExprKind::Lambda {
                body: LambdaBody::Expr(x),
                ..
            } => self.expr(x),
            ExprKind::Call { callee, args } => {
                self.expr(callee);
                for a in args {
                    self.expr(&a.value);
                }
            }
            ExprKind::Paren(x) | ExprKind::Unary { expr: x, .. } => self.expr(x),
            ExprKind::Field { base, .. } => self.expr(base),
            ExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            ExprKind::Ternary { cond, then, else_ } => {
                self.expr(cond);
                self.expr(then);
                self.expr(else_);
            }
            ExprKind::Array(xs) | ExprKind::Commas(xs) | ExprKind::Spaced(xs) => {
                for x in xs {
                    self.expr(x);
                }
            }
            _ => {}
        }
    }
}

/// `[A-Za-z0-9_]`.
pub fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The identifier characters before `offset`: (start, word).
pub fn word_before(src: &str, offset: usize) -> (usize, &str) {
    let b = src.as_bytes();
    let mut s = offset.min(b.len());
    while s > 0 && is_word(b[s - 1]) {
        s -= 1;
    }
    (s, &src[s..offset.min(b.len())])
}

/// The end of the identifier characters from `offset` on.
pub fn word_end(src: &str, offset: usize) -> usize {
    let b = src.as_bytes();
    let mut e = offset.min(b.len());
    while e < b.len() && is_word(b[e]) {
        e += 1;
    }
    e
}

/// The text of the line holding `offset`, up to `offset`.
pub fn line_before(src: &str, offset: usize) -> &str {
    let offset = offset.min(src.len());
    let start = src[..offset].rfind(['\n', '\r']).map_or(0, |i| i + 1);
    &src[start..offset]
}
