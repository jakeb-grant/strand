//! A visitor over the typed program: every expression, element, prop list
//! and token definition, with the file it is in.

use strand_compiler::FileId;
use strand_compiler::hir::*;

/// What a walk reports. Every method has a no-op default.
pub trait Visit<'p> {
    /// Every expression, parents before children.
    fn expr(&mut self, _file: FileId, _e: &'p Expr) {}
    /// Every element, before its props and children.
    fn element(&mut self, _file: FileId, _e: &'p Element) {}
    /// Props of an element, or of a `when`, pose or selector inside one
    /// (`owner` is that element; `None` for keyframes).
    fn props(&mut self, _file: FileId, _owner: Option<&'p Element>, _props: &'p [Prop]) {}
    /// Token definitions: token sets, component `tokens { }`, `set { }`.
    fn token_def(&mut self, _file: FileId, _d: &'p TokenDef) {}
    fn component(&mut self, _file: FileId, _c: &'p Component) {}
}

pub fn walk<'p>(program: &'p Program, v: &mut impl Visit<'p>) {
    for f in &program.files {
        let mut w = Walker {
            file: f.file,
            v,
            stack: Vec::new(),
        };
        for item in &f.items {
            w.item(item);
        }
    }
}

struct Walker<'a, 'p, V> {
    file: FileId,
    v: &'a mut V,
    /// Enclosing elements.
    stack: Vec<&'p Element>,
}

impl<'p, V: Visit<'p>> Walker<'_, 'p, V> {
    fn item(&mut self, item: &'p Item) {
        match item {
            Item::Component(c) => {
                self.v.component(self.file, c);
                for p in &c.params {
                    if let Some(d) = &p.default {
                        self.expr(d);
                    }
                }
                for t in &c.tokens {
                    self.token_def(t);
                }
                self.nodes(&c.body);
            }
            Item::Surface(s) => self.element(&s.element),
            Item::State(s) => self.state(s),
            Item::Let(l) => self.expr(&l.value),
            Item::Fn(f) => self.stmts(&f.body),
            Item::Enum(_) | Item::Type(_) | Item::Permit(_) => {}
            Item::Tokens(t) => {
                for d in &t.entries {
                    self.token_def(d);
                }
            }
            Item::Use(u) => {
                for e in u.tokens.iter().chain(&u.palette) {
                    self.expr(e);
                }
            }
            Item::Service(s) => {
                for e in s.args.iter().chain(&s.every) {
                    self.expr(e);
                }
            }
            Item::Keyframes(k) => {
                for stop in &k.stops {
                    self.props(None, &stop.props);
                }
                self.props(None, &k.props);
            }
            Item::Handler(h) => self.handler(h),
            Item::Timer(t) => self.timer(t),
        }
    }

    fn token_def(&mut self, d: &'p TokenDef) {
        self.v.token_def(self.file, d);
        self.expr(&d.value);
    }

    fn state(&mut self, s: &'p StateDecl) {
        match &s.init {
            StateInit::Value { value, .. } => self.expr(value),
            StateInit::File { fields, .. } => {
                for f in fields {
                    if let Some(d) = &f.default {
                        self.expr(d);
                    }
                }
            }
        }
    }

    fn handler(&mut self, h: &'p Handler) {
        if let Event::Change { targets, debounce } = &h.event {
            for e in targets.iter().chain(debounce) {
                self.expr(e);
            }
        }
        self.stmts(&h.body);
    }

    fn timer(&mut self, t: &'p Timer) {
        self.expr(&t.duration);
        if let Some(w) = &t.while_ {
            self.expr(w);
        }
        self.stmts(&t.body);
    }

    fn element(&mut self, e: &'p Element) {
        self.v.element(self.file, e);
        if let Some(a) = &e.arg {
            self.expr(a);
        }
        self.stack.push(e);
        self.props(Some(e), &e.props);
        self.nodes(&e.children);
        self.stack.pop();
    }

    fn props(&mut self, owner: Option<&'p Element>, props: &'p [Prop]) {
        self.v.props(self.file, owner, props);
        for p in props {
            self.expr(&p.value);
            if let Some(t) = &p.transition {
                self.expr(t);
            }
            self.props(owner, &p.sub);
        }
    }

    fn nodes(&mut self, nodes: &'p [Node]) {
        for n in nodes {
            self.node(n);
        }
    }

    fn node(&mut self, n: &'p Node) {
        let owner = self.stack.last().copied();
        match n {
            Node::Element(e) => self.element(e),
            Node::When(w) => {
                self.expr(&w.cond);
                self.props(owner, &w.props);
            }
            Node::If(i) => {
                self.expr(&i.cond);
                self.nodes(&i.then);
                self.nodes(&i.else_);
            }
            Node::For(f) => {
                self.expr(&f.iter);
                if let Some(k) = &f.key {
                    self.expr(k);
                }
                self.nodes(&f.body);
            }
            Node::Match(m) => {
                self.expr(&m.scrutinee);
                for (p, body) in &m.arms {
                    self.pattern(p);
                    self.nodes(body);
                }
            }
            Node::Handler(h) => self.handler(h),
            Node::Timer(t) => self.timer(t),
            Node::Pose(p) => self.props(owner, &p.props),
            Node::Slot(_) => {}
            Node::Set(defs, _) => {
                for d in defs {
                    self.token_def(d);
                }
            }
            Node::Selector(s) => self.props(owner, &s.props),
            Node::Play(e) => self.expr(e),
            Node::State(s) => self.state(s),
            Node::Let(l) => self.expr(&l.value),
        }
    }

    fn pattern(&mut self, p: &'p Pattern) {
        if let Pattern::Literal(e) = p {
            self.expr(e);
        }
    }

    fn stmts(&mut self, stmts: &'p [Stmt]) {
        for s in stmts {
            match &s.kind {
                StmtKind::Let { value, .. } => self.expr(value),
                StmtKind::Assign { target, value, .. } => {
                    self.expr(target);
                    self.expr(value);
                }
                StmtKind::Expr(e) | StmtKind::Play(e) => self.expr(e),
                StmtKind::If { cond, then, else_ } => {
                    self.expr(cond);
                    self.stmts(then);
                    self.stmts(else_);
                }
                StmtKind::For { iter, body, .. } => {
                    self.expr(iter);
                    self.stmts(body);
                }
                StmtKind::Match { scrutinee, arms } => {
                    self.expr(scrutinee);
                    for (p, body) in arms {
                        self.pattern(p);
                        self.stmts(body);
                    }
                }
                StmtKind::Error => {}
            }
        }
    }

    fn expr(&mut self, e: &'p Expr) {
        self.v.expr(self.file, e);
        match &e.kind {
            ExprKind::Field { base, .. } => self.expr(base),
            ExprKind::Call { callee, args } => {
                match callee {
                    Callee::Method { receiver, .. } => self.expr(receiver),
                    Callee::Value(f) => self.expr(f),
                    _ => {}
                }
                for a in args {
                    self.expr(&a.value);
                }
            }
            ExprKind::Index { base, index } => {
                self.expr(base);
                self.expr(index);
            }
            ExprKind::Unary { expr, .. } => self.expr(expr),
            ExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            ExprKind::Ternary { cond, then, else_ } => {
                self.expr(cond);
                self.expr(then);
                self.expr(else_);
            }
            ExprKind::Lambda { body, .. } => match body {
                LambdaBody::Expr(x) => self.expr(x),
                LambdaBody::Block(b) => self.stmts(b),
            },
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for (p, x) in arms {
                    self.pattern(p);
                    self.expr(x);
                }
            }
            ExprKind::List(xs) | ExprKind::Commas(xs) | ExprKind::Spaced(xs) => {
                for x in xs {
                    self.expr(x);
                }
            }
            _ => {}
        }
    }
}

/// Every expression of the program, with its file.
pub fn exprs(program: &Program) -> Vec<(FileId, &Expr)> {
    struct V<'p>(Vec<(FileId, &'p Expr)>);
    impl<'p> Visit<'p> for V<'p> {
        fn expr(&mut self, file: FileId, e: &'p Expr) {
            self.0.push((file, e));
        }
    }
    let mut v = V(Vec::new());
    walk(program, &mut v);
    v.0
}

/// Every token definition of the program, with its file.
pub fn token_defs(program: &Program) -> Vec<(FileId, &TokenDef)> {
    struct V<'p>(Vec<(FileId, &'p TokenDef)>);
    impl<'p> Visit<'p> for V<'p> {
        fn token_def(&mut self, file: FileId, d: &'p TokenDef) {
            self.0.push((file, d));
        }
    }
    let mut v = V(Vec::new());
    walk(program, &mut v);
    v.0
}

/// Every prop list with the element it belongs to.
pub fn prop_lists(program: &Program) -> Vec<(FileId, Option<&Element>, &[Prop])> {
    struct V<'p>(Vec<(FileId, Option<&'p Element>, &'p [Prop])>);
    impl<'p> Visit<'p> for V<'p> {
        fn props(&mut self, file: FileId, owner: Option<&'p Element>, props: &'p [Prop]) {
            self.0.push((file, owner, props));
        }
    }
    let mut v = V(Vec::new());
    walk(program, &mut v);
    v.0
}

/// The component declaring `def`.
pub fn component(program: &Program, def: DefId) -> Option<&Component> {
    program
        .files
        .iter()
        .flat_map(|f| &f.items)
        .find_map(|i| match i {
            Item::Component(c) if c.def == def => Some(c),
            _ => None,
        })
}
