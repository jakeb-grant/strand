//! A generic labelled view of the syntax tree.
//!
//! Used for snapshot tests (the AST "shape" without spans), for checking
//! that every child span nests inside its parent, and for debugging the
//! parser from the LSP. Expressions render as one-line s-expressions.

use std::fmt::Write as _;

use super::Span;
use super::ast::*;

/// One node of the view.
#[derive(Clone, Debug, PartialEq)]
pub struct TreeNode {
    pub label: String,
    pub span: Span,
    pub children: Vec<TreeNode>,
    /// Rendered as an s-expression on its parent's line.
    pub inline: bool,
}

impl TreeNode {
    fn node(label: impl Into<String>, span: Span) -> Self {
        Self {
            label: label.into(),
            span,
            children: Vec::new(),
            inline: false,
        }
    }

    fn leaf(label: impl Into<String>, span: Span) -> Self {
        Self {
            inline: true,
            ..Self::node(label, span)
        }
    }

    fn sexpr(label: impl Into<String>, span: Span, children: Vec<TreeNode>) -> Self {
        Self {
            label: label.into(),
            span,
            children,
            inline: true,
        }
    }

    fn with(mut self, child: TreeNode) -> Self {
        self.children.push(child);
        self
    }

    fn with_all(mut self, children: impl IntoIterator<Item = TreeNode>) -> Self {
        self.children.extend(children);
        self
    }

    /// Renders the tree: one line per structural node, expressions inline.
    pub fn render(&self) -> String {
        let mut out = String::new();
        self.render_into(&mut out, 0);
        out
    }

    fn render_into(&self, out: &mut String, indent: usize) {
        if self.inline {
            out.push_str(&"  ".repeat(indent));
            self.render_sexpr(out);
            out.push('\n');
            return;
        }
        out.push_str(&"  ".repeat(indent));
        out.push_str(&self.label);
        let mut rest = Vec::new();
        for c in &self.children {
            if c.inline && rest.is_empty() {
                out.push(' ');
                c.render_sexpr(out);
            } else {
                rest.push(c);
            }
        }
        out.push('\n');
        for c in rest {
            c.render_into(out, indent + 1);
        }
    }

    fn render_sexpr(&self, out: &mut String) {
        if self.children.is_empty() {
            out.push_str(&self.label);
            return;
        }
        out.push('(');
        out.push_str(&self.label);
        for c in &self.children {
            out.push(' ');
            c.render_sexpr(out);
        }
        out.push(')');
    }

    /// Visits this node and every descendant, parent first.
    pub fn walk(&self, f: &mut impl FnMut(&TreeNode, Option<&TreeNode>)) {
        fn go<'a>(
            n: &'a TreeNode,
            parent: Option<&'a TreeNode>,
            f: &mut impl FnMut(&TreeNode, Option<&TreeNode>),
        ) {
            f(n, parent);
            for c in &n.children {
                go(c, Some(n), f);
            }
        }
        go(self, None, f);
    }
}

/// The view of a whole file.
pub fn tree(file: &File) -> TreeNode {
    TreeNode::node("file", file.span).with_all(file.items.iter().map(item))
}

fn ident_path(ids: &[Ident]) -> String {
    ids.iter()
        .map(|i| i.name.as_str())
        .collect::<Vec<_>>()
        .join(".")
}

fn items(block: &Block<Item>) -> impl Iterator<Item = TreeNode> + '_ {
    block.items.iter().map(item)
}

fn stmts(block: &Block<Stmt>) -> impl Iterator<Item = TreeNode> + '_ {
    block.items.iter().map(stmt)
}

fn entries(block: &Block<TokenEntry>) -> impl Iterator<Item = TreeNode> + '_ {
    block.items.iter().map(token_entry)
}

fn item(it: &Item) -> TreeNode {
    let mut n = item_kind(&it.kind, it.span);
    // The item's span covers its attributes (a field's own span does not).
    n.span = it.span;
    let attrs: Vec<TreeNode> = it
        .attrs
        .iter()
        .map(|a| {
            TreeNode::sexpr(
                format!("@{}", a.name.name),
                a.span,
                a.args.iter().map(arg).collect(),
            )
        })
        .collect();
    if !attrs.is_empty() {
        let mut children = attrs;
        children.append(&mut n.children);
        n.children = children;
    }
    n
}

fn params(ps: &[Param], span: Span) -> TreeNode {
    TreeNode::sexpr("params", span, ps.iter().map(param).collect())
}

/// The span of a parameter list (which has no node of its own); an empty
/// list sits at the start of the node that holds it, `owner`.
fn params_span(ps: &[Param], owner: Span) -> Span {
    match (ps.first(), ps.last()) {
        (Some(a), Some(b)) => a.span.to(b.span),
        _ => Span::at(owner.start),
    }
}

fn param(p: &Param) -> TreeNode {
    let mut children = Vec::new();
    if let Some(t) = &p.ty {
        children.push(ty(t));
    }
    if let Some(d) = &p.default {
        children.push(TreeNode::sexpr("=", d.span, vec![expr(d)]));
    }
    if children.is_empty() {
        TreeNode::leaf(p.name.name.clone(), p.span)
    } else {
        TreeNode::sexpr(p.name.name.clone(), p.span, children)
    }
}

fn item_kind(kind: &ItemKind, span: Span) -> TreeNode {
    match kind {
        ItemKind::Component(c) => {
            let mut n = TreeNode::node(format!("component {}", c.name.name), span);
            if let Some(ps) = &c.params {
                n = n.with(params(ps, params_span(ps, span)));
            }
            if let Some(t) = &c.tokens {
                n = n.with(TreeNode::node("tokens", t.span).with_all(entries(t)));
            }
            n.with_all(items(&c.body))
        }
        ItemKind::Surface(s) => {
            let name = s
                .name
                .as_ref()
                .map_or(String::new(), |n| format!(" {}", n.name));
            TreeNode::node(format!("{}{name}", s.kind.name), span).with_all(items(&s.body))
        }
        ItemKind::State(s) => {
            let export = if s.export.is_some() { "export " } else { "" };
            let mut n = TreeNode::node(format!("{export}state {}", s.name.name), span);
            match &s.init {
                StateInit::Value {
                    ty: t,
                    key,
                    value,
                    persist,
                } => {
                    if let Some(t) = t {
                        n = n.with(ty(t));
                    }
                    if let Some(k) = key {
                        n = n.with(TreeNode::sexpr("key", k.span, vec![expr(k)]));
                    }
                    n = n.with(TreeNode::sexpr("=", value.span, vec![expr(value)]));
                    if let Some(p) = persist {
                        n = n.with(TreeNode::leaf("persist", *p));
                    }
                }
                StateInit::File { path, fields } => {
                    n = n.with(TreeNode::leaf(format!("from {:?}", path.value), path.span));
                    n = n.with_all(fields.items.iter().map(field));
                }
            }
            n
        }
        ItemKind::Let(l) => let_node(l, span),
        ItemKind::Enum(e) => TreeNode::node(format!("enum {}", e.name.name), span).with_all(
            e.variants
                .iter()
                .map(|v| TreeNode::leaf(v.name.clone(), v.span)),
        ),
        ItemKind::Type(t) => TreeNode::node(format!("type {}", t.name.name), span)
            .with_all(t.fields.items.iter().map(field)),
        ItemKind::Field(f) => field(f),
        ItemKind::Fn(f) => {
            let mut n = TreeNode::node(format!("fn {}", f.name.name), span)
                .with(params(&f.params, params_span(&f.params, span)));
            if let Some(r) = &f.ret {
                n = n.with(TreeNode::sexpr("->", r.span, vec![ty(r)]));
            }
            n.with_all(stmts(&f.body))
        }
        ItemKind::Tokens(t) => {
            let mut n = TreeNode::node(format!("tokens {}", t.name.name), span);
            if let Some(e) = &t.extends {
                n = n.with(TreeNode::leaf(format!("extends {}", e.name), e.span));
            }
            n.with_all(entries(&t.entries))
        }
        ItemKind::Use(u) => TreeNode::node("use", span).with_all(
            u.clauses
                .iter()
                .map(|c| TreeNode::sexpr(c.kind.name.clone(), c.span, vec![expr(&c.value)])),
        ),
        ItemKind::Service(s) => {
            let mut src = vec![TreeNode::leaf(
                s.source.kind.name.clone(),
                s.source.kind.span,
            )];
            src.extend(s.source.args.iter().map(expr));
            if let Some(e) = &s.source.every {
                src.push(TreeNode::sexpr("every", e.span, vec![expr(e)]));
            }
            TreeNode::node(format!("service {}", s.name.name), span)
                .with(TreeNode::sexpr("from", s.source.span, src))
                .with_all(items(&s.body))
        }
        ItemKind::Permit(p) => TreeNode::node(format!("permit {}", p.capability.name), span)
            .with_all(p.args.iter().map(expr)),
        ItemKind::Keyframes(k) => {
            TreeNode::node(format!("keyframes {}", k.name.name), span).with_all(items(&k.body))
        }
        ItemKind::KeyframeStop(k) => TreeNode::node("stop", span)
            .with_all(k.stops.iter().map(expr))
            .with_all(items(&k.body)),
        ItemKind::Prop(p) => {
            let mut n = TreeNode::node(format!("prop {}:", p.name.name), span);
            if let Some(t) = p.two_way {
                n = n.with(TreeNode::leaf("<->", t));
            }
            n = n.with(expr(&p.value));
            if let Some(t) = &p.transition {
                n = n.with(TreeNode::sexpr("~", t.span, vec![expr(t)]));
            }
            if let Some(b) = &p.block {
                n = n.with(TreeNode::node("block", b.span).with_all(items(b)));
            }
            n
        }
        ItemKind::Element(e) => {
            let mut n = TreeNode::node(format!("element {}", e.kind.name), span);
            match &e.arg {
                Some(HeadArg::Positional(x)) => n = n.with(expr(x)),
                Some(HeadArg::Named(name, x)) => {
                    n = n.with(TreeNode::sexpr(
                        format!("{}:", name.name),
                        name.span.to(x.span),
                        vec![expr(x)],
                    ));
                }
                None => {}
            }
            if let Some(b) = &e.block {
                n = n.with_all(items(b));
            }
            n
        }
        ItemKind::When(w) => TreeNode::node("when", span)
            .with(expr(&w.cond))
            .with_all(items(&w.body)),
        ItemKind::If(i) => if_node(i, span, &|b| items(b).collect()),
        ItemKind::For(f) => for_node(f, span, &|b| items(b).collect()),
        ItemKind::Match(m) => match_node(m, span, &|b| match b {
            ArmBody::Block(b) => items(b).collect(),
            ArmBody::Single(i) => vec![item(i)],
        }),
        ItemKind::On(o) => {
            let mut n = match &o.event {
                Event::Named { path, params: ps } => {
                    let mut n = TreeNode::node(format!("on {}", ident_path(path)), span);
                    if let Some(ps) = ps {
                        n = n.with(params(ps, params_span(ps, span)));
                    }
                    n
                }
                Event::Change { targets, debounce } => {
                    let mut n =
                        TreeNode::node("on change", span).with_all(targets.iter().map(expr));
                    if let Some(d) = debounce {
                        n = n.with(TreeNode::sexpr("after", d.span, vec![expr(d)]));
                    }
                    n
                }
            };
            n = n.with_all(stmts(&o.body));
            n
        }
        ItemKind::Timer(t) => {
            let kw = match t.kind {
                TimerKind::After => "after",
                TimerKind::Every => "every",
            };
            let mut n = TreeNode::node(kw, span).with(expr(&t.duration));
            if let Some(w) = &t.while_ {
                n = n.with(TreeNode::sexpr("while", w.span, vec![expr(w)]));
            }
            n.with_all(stmts(&t.body))
        }
        ItemKind::Pose(p) => {
            let kw = match p.kind {
                PoseKind::Enter => "enter",
                PoseKind::Exit => "exit",
            };
            TreeNode::node(kw, span).with_all(items(&p.body))
        }
        ItemKind::Slot => TreeNode::node("slot", span),
        ItemKind::Set(b) => TreeNode::node("set", span).with_all(entries(b)),
        ItemKind::Selector(s) => {
            TreeNode::node(format!("#{}", s.name.name), span).with_all(items(&s.body))
        }
        ItemKind::Play(e) => TreeNode::node("play", span).with(expr(e)),
        ItemKind::Error => TreeNode::node("<error>", span),
    }
}

fn let_node(l: &Let, span: Span) -> TreeNode {
    let export = if l.export.is_some() { "export " } else { "" };
    let mut n = TreeNode::node(format!("{export}let {}", l.name.name), span);
    if let Some(t) = &l.ty {
        n = n.with(ty(t));
    }
    n.with(TreeNode::sexpr("=", l.value.span, vec![expr(&l.value)]))
}

fn field(f: &Field) -> TreeNode {
    let mut n = TreeNode::node(format!("field {}", f.name.name), f.span).with(ty(&f.ty));
    if let Some(rw) = f.rw {
        n = n.with(TreeNode::leaf("rw", rw));
    }
    if let Some(d) = &f.default {
        n = n.with(TreeNode::sexpr("=", d.span, vec![expr(d)]));
    }
    n
}

fn token_entry(e: &TokenEntry) -> TreeNode {
    let o = if e.override_.is_some() {
        "override "
    } else {
        ""
    };
    let d = if e.key.dollar { "$" } else { "" };
    let n = TreeNode::node(
        format!("{o}token {d}{}", ident_path(&e.key.segments)),
        e.span,
    );
    match &e.body {
        TokenBody::Value(v) => n.with(expr(v)),
        TokenBody::Group(b) => n.with_all(entries(b)),
    }
}

fn if_node<T>(i: &If<T>, span: Span, body: &dyn Fn(&Block<T>) -> Vec<TreeNode>) -> TreeNode {
    let mut n = TreeNode::node("if", span)
        .with(expr(&i.cond))
        .with(TreeNode::node("then", i.then.span).with_all(body(&i.then)));
    match &i.else_ {
        Some(Else::If(nested, s)) => {
            n = n.with(TreeNode::node("else", *s).with(if_node(nested, *s, body)))
        }
        Some(Else::Block(b)) => n = n.with(TreeNode::node("else", b.span).with_all(body(b))),
        None => {}
    }
    n
}

fn for_node<T>(f: &For<T>, span: Span, body: &dyn Fn(&Block<T>) -> Vec<TreeNode>) -> TreeNode {
    let mut n = TreeNode::node(format!("for {}", f.binding.name), span).with(TreeNode::sexpr(
        "in",
        f.iter.span,
        vec![expr(&f.iter)],
    ));
    if let Some(k) = &f.key {
        n = n.with(TreeNode::sexpr("key", k.span, vec![expr(k)]));
    }
    n.with_all(body(&f.body))
}

fn match_node<B>(m: &Match<B>, span: Span, body: &dyn Fn(&B) -> Vec<TreeNode>) -> TreeNode {
    TreeNode::node("match", span)
        .with(expr(&m.scrutinee))
        .with_all(m.arms.iter().map(|a| {
            TreeNode::node("arm", a.span)
                .with(pattern(&a.pattern))
                .with_all(body(&a.body))
        }))
}

fn stmt(s: &Stmt) -> TreeNode {
    let span = s.span;
    match &s.kind {
        StmtKind::Let(l) => let_node(l, span),
        StmtKind::Assign { target, op, value } => {
            TreeNode::node(format!("assign {}", op.as_str()), span)
                .with(expr(target))
                .with(expr(value))
        }
        StmtKind::Expr(e) => TreeNode::node("expr", span).with(expr(e)),
        StmtKind::If(i) => if_node(i, span, &|b| stmts(b).collect()),
        StmtKind::For(f) => for_node(f, span, &|b| stmts(b).collect()),
        StmtKind::Match(m) => match_node(m, span, &|b| match b {
            ArmBody::Block(b) => stmts(b).collect(),
            ArmBody::Single(s) => vec![stmt(s)],
        }),
        StmtKind::Play(e) => TreeNode::node("play", span).with(expr(e)),
        StmtKind::Error => TreeNode::node("<error>", span),
    }
}

fn ty_label(t: &Type) -> String {
    match &t.kind {
        TypeKind::Named { path, args } => {
            let mut s = ident_path(path);
            if !args.is_empty() {
                let inner: Vec<String> = args.iter().map(ty_label).collect();
                let _ = write!(s, "<{}>", inner.join(", "));
            }
            s
        }
        TypeKind::List(inner) => format!("[{}]", ty_label(inner)),
        TypeKind::Optional(inner) => format!("{}?", ty_label(inner)),
        TypeKind::Error => "<error>".into(),
    }
}

fn ty(t: &Type) -> TreeNode {
    TreeNode::leaf(format!(":{}", ty_label(t)), t.span)
}

fn pattern(p: &Pattern) -> TreeNode {
    match &p.kind {
        PatternKind::Wildcard => TreeNode::leaf("_", p.span),
        PatternKind::Path(path) => TreeNode::leaf(ident_path(path), p.span),
        PatternKind::Literal(e) => expr(e),
        PatternKind::Error => TreeNode::leaf("<error>", p.span),
    }
}

fn number_label(n: &Number) -> String {
    let unit = n.unit.map_or("", Unit::as_str);
    format!("{}{unit}", n.value)
}

fn arg(a: &Arg) -> TreeNode {
    match &a.kind {
        ArgKind::Positional => expr(&a.value),
        ArgKind::Named(name) => {
            TreeNode::sexpr(format!("{}:", name.name), a.span, vec![expr(&a.value)])
        }
        ArgKind::From(_) => TreeNode::sexpr("from", a.span, vec![expr(&a.value)]),
    }
}

/// An expression as an inline s-expression node.
pub fn expr(e: &Expr) -> TreeNode {
    let span = e.span;
    let many =
        |label: &str, xs: &[Expr]| TreeNode::sexpr(label, span, xs.iter().map(expr).collect());
    match &e.kind {
        ExprKind::Number(n) => TreeNode::leaf(number_label(n), span),
        ExprKind::String(s) => TreeNode::leaf(format!("{:?}", s.value), span),
        ExprKind::Color(c) => {
            let [r, g, b, a] = c.rgba;
            TreeNode::leaf(format!("#{r:02x}{g:02x}{b:02x}{a:02x}"), span)
        }
        ExprKind::Bool(b) => TreeNode::leaf(b.to_string(), span),
        ExprKind::Null => TreeNode::leaf("null", span),
        ExprKind::Name(id) => TreeNode::leaf(id.name.clone(), span),
        ExprKind::Token(k) => TreeNode::leaf(format!("${}", ident_path(&k.segments)), span),
        ExprKind::Array(xs) => many("array", xs),
        ExprKind::Paren(x) => TreeNode::sexpr("paren", span, vec![expr(x)]),
        ExprKind::Unary { op, expr: x } => {
            let label = if *op == UnaryOp::Neg {
                "neg"
            } else {
                op.as_str()
            };
            TreeNode::sexpr(label, span, vec![expr(x)])
        }
        ExprKind::Binary { op, lhs, rhs } => {
            TreeNode::sexpr(op.as_str(), span, vec![expr(lhs), expr(rhs)])
        }
        ExprKind::Ternary { cond, then, else_ } => {
            TreeNode::sexpr("?:", span, vec![expr(cond), expr(then), expr(else_)])
        }
        ExprKind::Field {
            base,
            name,
            optional,
        } => TreeNode::sexpr(
            if *optional { "?." } else { "." },
            span,
            vec![expr(base), TreeNode::leaf(name.name.clone(), name.span)],
        ),
        ExprKind::Call { callee, args } => {
            let mut children = vec![expr(callee)];
            children.extend(args.iter().map(arg));
            TreeNode::sexpr("call", span, children)
        }
        ExprKind::Index { base, index } => {
            TreeNode::sexpr("index", span, vec![expr(base), expr(index)])
        }
        ExprKind::Lambda { params: ps, body } => {
            let body = match body {
                LambdaBody::Expr(x) => expr(x),
                LambdaBody::Block(b) => {
                    TreeNode::sexpr("block", b.span, stmts(b).map(inline_all).collect())
                }
            };
            TreeNode::sexpr("=>", span, vec![params(ps, params_span(ps, span)), body])
        }
        ExprKind::Match(m) => {
            let mut children = vec![expr(&m.scrutinee)];
            children.extend(
                m.arms.iter().map(|a| {
                    TreeNode::sexpr("arm", a.span, vec![pattern(&a.pattern), expr(&a.body)])
                }),
            );
            TreeNode::sexpr("match", span, children)
        }
        ExprKind::Commas(xs) => many("commas", xs),
        ExprKind::Spaced(xs) => many("spaced", xs),
        ExprKind::Error => TreeNode::leaf("<error>", span),
    }
}

fn inline_all(mut n: TreeNode) -> TreeNode {
    n.inline = true;
    n.children = n.children.into_iter().map(inline_all).collect();
    n
}
