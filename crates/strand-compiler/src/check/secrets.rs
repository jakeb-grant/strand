//! (oi-lock) `check::secret_flow`: a warning where a password typed into
//! a `type: password` input (the lock's, a polkit dialog's, a greeter's, a
//! Wi-Fi network's) flows somewhere that shows, keeps or sends it
//! (decisions.md oi-lock, 2026-10-26; spec.md 5.1).
//!
//! **Where a password starts.** The place an `input` whose `type` may be
//! `password` binds its `text` to (`text: <-> pw`): a `state` (or a field
//! or item of one). One bound to a service's field or a settings field
//! sends or saves every key typed, a sink at once. Its `type` may be
//! `password` when one is written on it or in one of its `when` blocks,
//! or when it is read from elsewhere (a component's parameter, a state).
//! The key of an `on key(k)` on a password input, or around one (keys go
//! to the innermost handler), is one too: `k.text` is a typed character.
//!
//! **How it flows.** A value carries the password when it reads a place
//! that holds it, through anything that keeps its characters: `+`, `?:`,
//! `??`, `match`, lists and records built from it, a text method
//! (`pw.trim()`, `pw.split("")`), a builtin (`join`, `number`), a user
//! `fn` whose body passes on a parameter given it (or reads one), a
//! lambda a place holds and whose body does the same, a lambda's
//! parameters given its items (`pw.split("").map(c => …)` carries it only
//! when the lambda's result does). Places take it from `let`s, `state` starts, assignments
//! (`copy = pw`, `xs.push(pw)`), `for` bindings, component parameters
//! (each call's argument) and the parameters of a lambda handed to a
//! component and called there. Flows are followed over the whole config,
//! not per call: a component given the password by one call is checked as
//! if every call gave it.
//!
//! **What does not carry it.** A `bool` (`pw != ""`, `pw.contains(x)`,
//! `pw == other`) and its length (`pw.len`): password dots and "Caps Lock
//! is on" read these. Which branch of an `if` or `?:` runs is not
//! followed either.
//!
//! **Sinks.** Every prop or positional of a built-in element (`text pw`,
//! a `placeholder`, a canvas's `draw`, a `file`'s text), except the `text`
//! of a password input; an argument to a declared `action` (the process
//! list shows it) or to an action of a service or record (`clipboard.copy`,
//! `n.reply`, a `from dbus` service's method), except the four meant for a
//! password: `auth.submit`, `polkit.respond`, `greetd.submit` and an access
//! point's `connect_with`; a write to a service's field or a settings
//! field; and a `state` that holds it and is persisted (written to disk)
//! or exported (`strand get` reads it, as does every other file).
//!
//! The language has no `log` call and no notification of its own to send,
//! so those reach it only through the actions above.

use std::collections::{HashMap, HashSet};

use crate::diagnostic::Diagnostic;
use crate::hir::{
    self, CallArg, Callee, Def, DefId, DefKind, ElementKind, Expr, ExprKind, FileHir, Item,
    LambdaBody, Local, LocalId, Node, Stmt, StmtKind,
};
use crate::source::FileId;
use crate::syntax::Span;
use crate::ty::{Prim, Ty, TypeTable};

/// The code of every diagnostic here.
const CODE: &str = "check::secret_flow";

/// What the warning suggests.
const HELP: &str = "pass a password only to `auth.submit`, `polkit.respond`, `greetd.submit` or an \
                    access point's `connect_with`; to show that something is typed, read its \
                    length (`pw.len`) or whether it is empty (`pw != \"\"`)";

/// The actions meant for a password: (the record or service, the action).
const INTENDED: &[(&str, &str)] = &[
    ("auth", "submit"),
    ("polkit", "respond"),
    ("greetd", "submit"),
    ("AccessPoint", "connect_with"),
];

/// Something that may hold the password.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Place {
    Def(DefId),
    Local(LocalId),
    /// What a user `fn` returns.
    FnResult(DefId),
}

/// Where a password is typed: the password input's `text:` prop, or an
/// `on key` handler that hears its keys; and the label that says so.
type Origin = (FileId, Span, &'static str);

/// The label of an origin at a password input's `text:`.
const TYPED: &str = "the password is typed here";

/// Where a value assigned or bound lands.
enum Root<'a> {
    Place(Place),
    /// A schema service (`clipboard`).
    Service(&'a str),
}

/// A value that must not carry the password, and what it would do.
struct Sink<'a> {
    file: FileId,
    span: Span,
    value: &'a Expr,
    /// The message's end: "a password is …".
    what: String,
    label: String,
}

/// `check::secret_flow` over every file of a checked program.
pub(super) fn flow(
    files: &[FileHir],
    defs: &[Def],
    locals: &[Local],
    types: &TypeTable,
) -> Vec<Diagnostic> {
    let mut w = Walk {
        defs,
        locals,
        types,
        file: files.first().map_or(FileId(0), |f| f.file),
        fn_result: None,
        elem: "element",
        password: false,
        keys: false,
        bodies: HashMap::new(),
        params: HashMap::new(),
        fn_params: HashMap::new(),
        persisted: HashSet::new(),
        sources: Vec::new(),
        flows: Vec::new(),
        sinks: Vec::new(),
        now: Vec::new(),
        value_calls: Vec::new(),
    };
    for f in files {
        for item in &f.items {
            match item {
                Item::Component(c) => {
                    let ps = c
                        .params
                        .iter()
                        .map(|p| (locals[p.local.0 as usize].name.clone(), p.local))
                        .collect();
                    w.params.insert(c.def, ps);
                    w.bodies.insert(c.def, &c.body);
                }
                Item::Fn(fd) => {
                    w.fn_params.insert(fd.def, fd.params.clone());
                }
                _ => {}
            }
        }
    }
    for f in files {
        w.file = f.file;
        for item in &f.items {
            w.item(item);
        }
        for el in &f.outputs {
            w.element(el);
        }
    }
    let Walk {
        flows,
        sources,
        sinks,
        now,
        value_calls,
        persisted,
        fn_params,
        ..
    } = w;
    let mut flows = flows;
    // A function value called through a place: the lambdas that place
    // was given take the call's arguments.
    let mut lambdas: HashMap<Place, Vec<&[LocalId]>> = HashMap::new();
    for f in &flows {
        if let ExprKind::Lambda { params, .. } = &f.1.kind {
            lambdas.entry(f.0).or_default().push(params);
        }
    }
    for (callee, args) in value_calls {
        for params in lambdas.get(&callee).into_iter().flatten() {
            for (i, a) in args.iter().enumerate() {
                if let Some(&p) = params.get(a.param.unwrap_or(i)) {
                    flows.push((Place::Local(p), &a.value));
                }
            }
        }
    }
    // A place every flow into which is a lambda: calling it gives what
    // those lambdas give, whatever its arguments.
    let mut other: HashSet<Place> = HashSet::new();
    for f in &flows {
        if !matches!(f.1.kind, ExprKind::Lambda { .. }) {
            other.insert(f.0);
        }
    }
    let lambdas: HashSet<Place> = lambdas.into_keys().filter(|p| !other.contains(p)).collect();
    let fns: HashSet<DefId> = fn_params.into_keys().collect();
    let known = Known {
        types,
        fns: &fns,
        lambdas: &lambdas,
    };
    let held = spread(&flows, &sources, &known);
    let carry = Carry {
        held: &held,
        known: &known,
    };
    let mut out = now;
    for s in sinks {
        if let Some(origin) = carry.of(s.value) {
            out.push(warn(s.file, s.span, &s.what, s.label, origin));
        }
    }
    // A state that keeps it on disk, or that other files and `strand
    // get` read.
    let mut kept: Vec<(DefId, Origin)> = held
        .iter()
        .filter_map(|(p, o)| match p {
            Place::Def(d) => Some((*d, *o)),
            _ => None,
        })
        .collect();
    kept.sort_by_key(|(d, _)| *d);
    for (d, origin) in kept {
        let def = &defs[d.0 as usize];
        let (what, label) = if persisted.contains(&d) {
            (
                "kept on disk",
                format!("`{}` holds it, and is persisted across restarts", def.name),
            )
        } else if def.exported {
            (
                "exported",
                format!(
                    "`{}` holds it, and other files and `strand get` read it",
                    def.name
                ),
            )
        } else if def.kind == DefKind::Settings {
            (
                "saved in a settings file",
                format!("`{}` holds it, and is saved to its file", def.name),
            )
        } else {
            continue;
        };
        out.push(warn(def.file, def.span, what, label, origin));
    }
    out.sort_by_key(|d| (d.file(), d.primary_span().map(|s| (s.start, s.end))));
    out.dedup();
    out
}

/// The warning for a sink at `span` in `file`, reached from `origin`.
fn warn(file: FileId, span: Span, what: &str, label: String, origin: Origin) -> Diagnostic {
    let d =
        Diagnostic::warning(CODE, format!("a password is {what}")).with_label_in(file, span, label);
    let d = if (origin.0, origin.1) == (file, span) {
        d
    } else {
        d.with_secondary_in(origin.0, origin.1, origin.2)
    };
    d.with_help(HELP)
}

/// The places that come to hold the password, each with where it was
/// typed: from `sources`, along `flows` (place, value) until nothing
/// more is reached. A flow is looked at again only when a place its
/// value reads is reached, so a long chain costs its length.
fn spread(
    flows: &[(Place, &Expr)],
    sources: &[(Place, Origin)],
    known: &Known,
) -> HashMap<Place, Origin> {
    let mut readers: HashMap<Place, Vec<usize>> = HashMap::new();
    for (i, (_, value)) in flows.iter().enumerate() {
        let mut read = HashSet::new();
        mentions(value, &mut read);
        for p in read {
            readers.entry(p).or_default().push(i);
        }
    }
    let mut held: HashMap<Place, Origin> = HashMap::new();
    let mut queue: Vec<Place> = Vec::new();
    for &(p, o) in sources {
        if let std::collections::hash_map::Entry::Vacant(e) = held.entry(p) {
            e.insert(o);
            queue.push(p);
        }
    }
    while let Some(p) = queue.pop() {
        for &i in readers.get(&p).into_iter().flatten() {
            let (target, value) = flows[i];
            if held.contains_key(&target) {
                continue;
            }
            let found = Carry { held: &held, known }.of(value);
            if let Some(o) = found {
                held.insert(target, o);
                queue.push(target);
            }
        }
    }
    held
}

/// Every place `e` reads, lambdas' bodies included.
fn mentions(e: &Expr, out: &mut HashSet<Place>) {
    super::grow(|| match &e.kind {
        ExprKind::Local(l) => {
            out.insert(Place::Local(*l));
        }
        ExprKind::Def(d) => {
            out.insert(Place::Def(*d));
        }
        ExprKind::Call { callee, args } => {
            match callee {
                Callee::Fn(d) => {
                    out.insert(Place::FnResult(*d));
                }
                Callee::Method { receiver, .. } => mentions(receiver, out),
                Callee::Value(v) => mentions(v, out),
                _ => {}
            }
            args.iter().for_each(|a| mentions(&a.value, out));
        }
        ExprKind::Field { base, .. } => mentions(base, out),
        ExprKind::Index { base, index } => {
            mentions(base, out);
            mentions(index, out);
        }
        ExprKind::Unary { expr, .. } => mentions(expr, out),
        ExprKind::Binary { lhs, rhs, .. } => {
            mentions(lhs, out);
            mentions(rhs, out);
        }
        ExprKind::Ternary { cond, then, else_ } => {
            mentions(cond, out);
            mentions(then, out);
            mentions(else_, out);
        }
        ExprKind::Lambda { body, .. } => match body {
            LambdaBody::Expr(b) => mentions(b, out),
            LambdaBody::Block(b) => stmt_mentions(b, out),
        },
        ExprKind::Match { scrutinee, arms } => {
            mentions(scrutinee, out);
            arms.iter().for_each(|(_, a)| mentions(a, out));
        }
        ExprKind::List(v) | ExprKind::Commas(v) | ExprKind::Spaced(v) => {
            v.iter().for_each(|x| mentions(x, out));
        }
        _ => {}
    });
}

fn stmt_mentions(stmts: &[Stmt], out: &mut HashSet<Place>) {
    for s in stmts {
        match &s.kind {
            StmtKind::Let { value, .. } | StmtKind::Expr(value) | StmtKind::Play(value) => {
                mentions(value, out)
            }
            StmtKind::Assign { target, value, .. } => {
                mentions(target, out);
                mentions(value, out);
            }
            StmtKind::If { cond, then, else_ } => {
                mentions(cond, out);
                stmt_mentions(then, out);
                stmt_mentions(else_, out);
            }
            StmtKind::For { iter, body, .. } => {
                mentions(iter, out);
                stmt_mentions(body, out);
            }
            StmtKind::Match { scrutinee, arms } => {
                mentions(scrutinee, out);
                arms.iter().for_each(|(_, b)| stmt_mentions(b, out));
            }
            StmtKind::Error => {}
        }
    }
}

/// What the walk learnt of the calls a value makes.
struct Known<'k> {
    types: &'k TypeTable,
    /// The user `fn`s whose parameters the walk fed each call's
    /// arguments: what one returns is its result's place, nothing else.
    fns: &'k HashSet<DefId>,
    /// The places given only lambdas, whose parameters the walk fed each
    /// call's arguments: calling one gives what its lambdas give.
    lambdas: &'k HashSet<Place>,
}

/// Whether a value carries the password, given the places that hold it.
struct Carry<'h> {
    held: &'h HashMap<Place, Origin>,
    known: &'h Known<'h>,
}

impl Carry<'_> {
    /// Where the password `e` carries was typed, if it carries one.
    fn of(&self, e: &Expr) -> Option<Origin> {
        // A `bool` tells no characters: `pw != ""`, `pw.contains(x)`.
        if matches!(e.ty.non_null(), Ty::Prim(Prim::Bool)) {
            return None;
        }
        super::grow(|| self.kind(e))
    }

    fn held(&self, p: Place) -> Option<Origin> {
        self.held.get(&p).copied()
    }

    /// Whether `v` names a place given only lambdas.
    fn lambda(&self, v: &Expr) -> bool {
        let p = match &v.kind {
            ExprKind::Local(l) => Place::Local(*l),
            ExprKind::Def(d) => Place::Def(*d),
            _ => return false,
        };
        self.known.lambdas.contains(&p)
    }

    fn any<'e>(&self, es: impl IntoIterator<Item = &'e Expr>) -> Option<Origin> {
        es.into_iter().find_map(|x| self.of(x))
    }

    fn kind(&self, e: &Expr) -> Option<Origin> {
        match &e.kind {
            ExprKind::Local(l) => self.held(Place::Local(*l)),
            ExprKind::Def(d) => self.held(Place::Def(*d)),
            // Its length tells no characters (password dots).
            ExprKind::Field { name, .. } if name == "len" => None,
            ExprKind::Field { base, .. } => self.of(base),
            ExprKind::Index { base, index } => self.of(base).or_else(|| self.of(index)),
            ExprKind::Call { callee, args } => {
                let args = || args.iter().map(|a| &a.value);
                match callee {
                    // Its arguments went to its parameters, and its result
                    // holds them when its body passes them on: a
                    // password-dots helper carries nothing.
                    Callee::Fn(d) if self.known.fns.contains(d) => self.held(Place::FnResult(*d)),
                    Callee::Fn(d) => self.held(Place::FnResult(*d)).or_else(|| self.any(args())),
                    Callee::Builtin { .. } | Callee::Record(_) => self.any(args()),
                    // So with a lambda a place holds (`let f = (x) => …`).
                    Callee::Value(v) if self.lambda(v) => self.of(v),
                    Callee::Value(v) => self.of(v).or_else(|| self.any(args())),
                    // What a command or an action answers is its own.
                    Callee::Action(_) | Callee::Error => None,
                    Callee::Method { receiver, name, .. } => {
                        if action(self.known.types, &receiver.ty, name) {
                            None
                        } else if name == "map" {
                            // Each item is what the lambda makes of it.
                            self.any(args())
                        } else {
                            self.of(receiver).or_else(|| self.any(args()))
                        }
                    }
                }
            }
            ExprKind::Unary { expr, .. } => self.of(expr),
            ExprKind::Binary { lhs, rhs, .. } => self.of(lhs).or_else(|| self.of(rhs)),
            ExprKind::Ternary { then, else_, .. } => self.of(then).or_else(|| self.of(else_)),
            ExprKind::Lambda { body, .. } => match body {
                LambdaBody::Expr(b) => self.of(b),
                LambdaBody::Block(b) => self.block(b),
            },
            ExprKind::Match { arms, .. } => arms.iter().find_map(|(_, a)| self.of(a)),
            ExprKind::List(v) | ExprKind::Commas(v) | ExprKind::Spaced(v) => self.any(v),
            _ => None,
        }
    }

    /// A block lambda's value: any expression statement in it.
    fn block(&self, stmts: &[Stmt]) -> Option<Origin> {
        stmts.iter().find_map(|s| match &s.kind {
            StmtKind::Expr(v) => self.of(v),
            StmtKind::If { then, else_, .. } => self.block(then).or_else(|| self.block(else_)),
            StmtKind::For { body, .. } => self.block(body),
            StmtKind::Match { arms, .. } => arms.iter().find_map(|(_, b)| self.block(b)),
            _ => None,
        })
    }
}

/// Whether `name` on a value of type `ty` is an action (it changes the
/// world: sends, writes, runs).
fn action(types: &TypeTable, ty: &Ty, name: &str) -> bool {
    match ty.non_null() {
        Ty::Record(r) => types
            .record(*r)
            .method(name)
            .is_some_and(|m| m.sigs.iter().any(|s| s.action)),
        _ => false,
    }
}

/// Whether a list method changes its receiver (`push`, `insert`, `set`).
fn list_write(ty: &Ty, name: &str) -> bool {
    match ty.non_null() {
        Ty::List(elem, keyed) => crate::schema::list_members(elem, *keyed, &Ty::Error)
            .iter()
            .any(|m| m.name == name && m.writes),
        _ => false,
    }
}

/// Where a place expression (`pw`, `form.pw`, `xs[0]`, `clipboard.text`)
/// lands.
fn root(e: &Expr) -> Option<Root<'_>> {
    match &e.kind {
        ExprKind::Def(d) => Some(Root::Place(Place::Def(*d))),
        ExprKind::Local(l) => Some(Root::Place(Place::Local(*l))),
        ExprKind::Service(s) => Some(Root::Service(s)),
        ExprKind::Field { base, .. } | ExprKind::Index { base, .. } => root(base),
        _ => None,
    }
}

/// A short name for a call's receiver, for the label: `clipboard`,
/// `n`, `net.points`.
fn shown(e: &Expr, defs: &[Def], locals: &[Local]) -> Option<String> {
    match &e.kind {
        ExprKind::Service(s) => Some(s.clone()),
        ExprKind::Def(d) => Some(defs[d.0 as usize].name.clone()),
        ExprKind::Local(l) => Some(locals[l.0 as usize].name.clone()),
        ExprKind::Field { base, name, .. } => {
            shown(base, defs, locals).map(|b| format!("{b}.{name}"))
        }
        _ => None,
    }
}

/// The record or service name of a receiver's type.
fn record_name<'t>(types: &'t TypeTable, ty: &Ty) -> Option<&'t str> {
    match ty.non_null() {
        Ty::Record(r) => Some(types.record(*r).name.as_str()),
        _ => None,
    }
}

/// Whether an `input`'s `type` may be `password`: a literal `password`
/// in any branch, or a value read from elsewhere (a component's
/// parameter, a state), which may be.
fn password_type(types: &TypeTable, e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Variant(en, i) => {
            let en = types.enum_(*en);
            en.name == "InputKind"
                && en
                    .variants
                    .get(*i as usize)
                    .is_some_and(|v| v == "password")
        }
        ExprKind::Text(t) | ExprKind::Value(t) => t == "password",
        ExprKind::Number { .. }
        | ExprKind::Color(_)
        | ExprKind::Bool(_)
        | ExprKind::Null
        | ExprKind::EnumType(_)
        | ExprKind::Error => false,
        ExprKind::Ternary { then, else_, .. } => {
            password_type(types, then) || password_type(types, else_)
        }
        ExprKind::Match { arms, .. } => arms.iter().any(|(_, a)| password_type(types, a)),
        _ => true,
    }
}

/// Whether `el` is an `input` whose `type` may be `password`, set on it
/// or in one of its `when` blocks.
fn password_input(types: &TypeTable, el: &hir::Element) -> bool {
    let typed = |ps: &[hir::Prop]| {
        ps.iter()
            .any(|p| p.name == "type" && password_type(types, &p.value))
    };
    matches!(&el.kind, ElementKind::Builtin(k) if k == "input")
        && (typed(&el.props)
            || el
                .children
                .iter()
                .any(|n| matches!(n, Node::When(w) if typed(&w.props))))
}

/// Whether an `on key` is among `nodes` (an element's own, through
/// `if`, `for` and `match`).
fn hears_keys(nodes: &[Node]) -> bool {
    nodes.iter().any(|n| match n {
        Node::Handler(h) => matches!(&h.event, hir::Event::Element(e) if e == "key"),
        Node::If(i) => hears_keys(&i.then) || hears_keys(&i.else_),
        Node::For(f) => hears_keys(&f.body),
        Node::Match(m) => m.arms.iter().any(|(_, b)| hears_keys(b)),
        _ => false,
    })
}

/// The walk that collects sources, flows and sinks.
struct Walk<'a> {
    defs: &'a [Def],
    locals: &'a [Local],
    types: &'a TypeTable,
    /// The file being walked.
    file: FileId,
    /// Inside a `fn`'s body: the `fn` its expressions return from.
    fn_result: Option<DefId>,
    /// The element whose children are being walked (its `when` blocks'
    /// props are its own), and whether it is a password input.
    elem: &'a str,
    password: bool,
    /// Whether an `on key` on that element hears a password's keys (it is
    /// a password input or holds one: keys go to the innermost handler).
    keys: bool,
    /// Each component's body.
    bodies: HashMap<DefId, &'a [Node]>,
    /// Each component's parameters, by name.
    params: HashMap<DefId, Vec<(String, LocalId)>>,
    /// Each `fn`'s parameters.
    fn_params: HashMap<DefId, Vec<LocalId>>,
    /// The `persist` states.
    persisted: HashSet<DefId>,
    sources: Vec<(Place, Origin)>,
    flows: Vec<(Place, &'a Expr)>,
    sinks: Vec<Sink<'a>>,
    /// Warnings found as the walk goes (a password bound straight to a
    /// service's field).
    now: Vec<Diagnostic>,
    /// Calls of a function value held by a place, with their arguments.
    value_calls: Vec<(Place, &'a [CallArg])>,
}

impl<'a> Walk<'a> {
    fn item(&mut self, item: &'a Item) {
        match item {
            Item::Component(c) => {
                for p in &c.params {
                    if let Some(d) = &p.default {
                        self.flow(Place::Local(p.local), d);
                    }
                }
                self.nodes(&c.body);
            }
            Item::Surface(s) => self.element(&s.element),
            Item::State(s) => self.state(s),
            Item::Let(l) => self.flow(Place::Def(l.def), &l.value),
            Item::Fn(f) => {
                let saved = self.fn_result.replace(f.def);
                self.stmts(&f.body);
                self.fn_result = saved;
            }
            Item::Tokens(t) => {
                for e in &t.entries {
                    self.shown(&e.value, e.span, "the token `$".to_string() + &e.path + "`");
                }
            }
            Item::Keyframes(k) => {
                k.props.iter().for_each(|p| self.prop_sink(p, "keyframes"));
                for s in &k.stops {
                    s.props.iter().for_each(|p| self.prop_sink(p, "keyframes"));
                }
            }
            Item::Handler(h) => self.handler(h),
            Item::Timer(t) => self.timer(t),
            Item::Enum(_)
            | Item::Type(_)
            | Item::Use(_)
            | Item::Service(_)
            | Item::Permit(_)
            | Item::Action(_) => {}
        }
    }

    fn state(&mut self, s: &'a hir::StateDecl) {
        if s.persist {
            self.persisted.insert(s.def);
        }
        if let hir::StateInit::Value { value, .. } = &s.init {
            self.flow(Place::Def(s.def), value);
        }
    }

    fn handler(&mut self, h: &'a hir::Handler) {
        if let hir::Event::Change { targets, debounce } = &h.event {
            targets.iter().for_each(|t| self.expr(t));
            if let Some(d) = debounce {
                self.expr(d);
            }
        }
        self.stmts(&h.body);
    }

    fn timer(&mut self, t: &'a hir::Timer) {
        self.expr(&t.duration);
        if let Some(w) = &t.while_ {
            self.expr(w);
        }
        self.stmts(&t.body);
    }

    /// `target` takes `value`.
    fn flow(&mut self, target: Place, value: &'a Expr) {
        self.expr(value);
        self.flows.push((target, value));
    }

    fn nodes(&mut self, nodes: &'a [Node]) {
        for n in nodes {
            match n {
                Node::Element(e) => self.element(e),
                Node::When(w) => {
                    self.expr(&w.cond);
                    for p in &w.props {
                        if self.password && p.name == "text" {
                            self.password_text(p);
                        } else {
                            self.prop_sink(p, self.elem);
                        }
                    }
                }
                Node::If(i) => {
                    self.expr(&i.cond);
                    self.nodes(&i.then);
                    self.nodes(&i.else_);
                }
                Node::For(f) => {
                    self.flow(Place::Local(f.binding), &f.iter);
                    self.nodes(&f.body);
                }
                Node::Match(m) => {
                    self.expr(&m.scrutinee);
                    m.arms.iter().for_each(|(_, b)| self.nodes(b));
                }
                Node::Handler(h) => {
                    // Each key typed into the password, `k.text` and
                    // `k.name` (a `bool` of them, `k.name == "Return"`,
                    // carries none).
                    if self.keys
                        && matches!(&h.event, hir::Event::Element(e) if e == "key")
                        && let Some(&k) = h.params.first()
                    {
                        self.sources.push((
                            Place::Local(k),
                            (
                                self.file,
                                h.span,
                                "this `on key` hears each key of a password",
                            ),
                        ));
                    }
                    self.handler(h);
                }
                Node::Timer(t) => self.timer(t),
                Node::Pose(p) => p.props.iter().for_each(|p| self.prop_sink(p, "pose")),
                Node::Selector(s) => s.props.iter().for_each(|p| self.prop_sink(p, "svg part")),
                Node::Set(tokens, _) => {
                    for t in tokens {
                        self.shown(&t.value, t.span, "the token `$".to_string() + &t.path + "`");
                    }
                }
                Node::Play(e) => self.expr(e),
                Node::State(s) => self.state(s),
                Node::Let(l) => self.flow(Place::Def(l.def), &l.value),
                Node::Slot(_) => {}
            }
        }
    }

    fn element(&mut self, el: &'a hir::Element) {
        let saved = (self.elem, self.password, self.keys);
        self.elem = "element";
        self.password = false;
        self.keys = false;
        match &el.kind {
            ElementKind::Component(def) => {
                let params = self.params.get(def).cloned().unwrap_or_default();
                if let Some(a) = &el.arg
                    && let Some((_, p)) = params.first()
                {
                    self.flow(Place::Local(*p), a);
                }
                for p in &el.props {
                    match params.iter().find(|(n, _)| *n == p.name) {
                        Some((_, l)) => self.flow(Place::Local(*l), &p.value),
                        None => self.prop_sink(p, "element"),
                    }
                }
            }
            ElementKind::Builtin(kind) => {
                let password = password_input(self.types, el);
                if let Some(a) = &el.arg {
                    self.expr(a);
                    let (what, label) = sink_words(kind, None);
                    self.sinks.push(Sink {
                        file: self.file,
                        span: a.span,
                        value: a,
                        what,
                        label,
                    });
                }
                for p in &el.props {
                    if password && p.name == "text" {
                        self.password_text(p);
                    } else {
                        self.prop_sink(p, kind);
                    }
                }
                self.elem = kind;
                self.password = password;
                self.keys = password
                    || (hears_keys(&el.children)
                        && self.holds_password(&el.children, &mut HashSet::new()));
            }
            ElementKind::Unknown(_) => {}
        }
        self.nodes(&el.children);
        (self.elem, self.password, self.keys) = saved;
    }

    /// Whether a password input is among `nodes`, in components called
    /// there too.
    fn holds_password(&self, nodes: &[Node], seen: &mut HashSet<DefId>) -> bool {
        nodes.iter().any(|n| match n {
            Node::Element(e) => match &e.kind {
                ElementKind::Builtin(_) => {
                    password_input(self.types, e) || self.holds_password(&e.children, seen)
                }
                ElementKind::Component(d) => {
                    self.holds_password(&e.children, seen)
                        || (seen.insert(*d)
                            && self
                                .bodies
                                .get(d)
                                .is_some_and(|b| self.holds_password(b, seen)))
                }
                ElementKind::Unknown(_) => false,
            },
            Node::If(i) => {
                self.holds_password(&i.then, seen) || self.holds_password(&i.else_, seen)
            }
            Node::For(f) => self.holds_password(&f.body, seen),
            Node::Match(m) => m.arms.iter().any(|(_, b)| self.holds_password(b, seen)),
            _ => false,
        })
    }

    /// A password input's `text`: shown masked, and where its password
    /// starts when bound two-way.
    fn password_text(&mut self, p: &'a hir::Prop) {
        self.expr(&p.value);
        if p.two_way {
            self.password_bound(p);
        }
    }

    /// A password input's `text: <-> place`: where its password starts.
    fn password_bound(&mut self, p: &'a hir::Prop) {
        let origin = (self.file, p.span, TYPED);
        match root(&p.value) {
            Some(Root::Place(Place::Def(d)))
                if matches!(
                    self.defs[d.0 as usize].kind,
                    DefKind::Service(_) | DefKind::Settings
                ) =>
            {
                let name = &self.defs[d.0 as usize].name;
                self.now.push(warn(
                    self.file,
                    p.value.span,
                    if self.defs[d.0 as usize].kind == DefKind::Settings {
                        "saved in a settings file"
                    } else {
                        "written to a service"
                    },
                    format!("every key typed is written to `{name}`"),
                    origin,
                ));
            }
            Some(Root::Place(place)) => self.sources.push((place, origin)),
            Some(Root::Service(s)) => self.now.push(warn(
                self.file,
                p.value.span,
                "written to a service",
                format!("every key typed is written to `{s}`"),
                origin,
            )),
            None => {}
        }
    }

    /// Prop `p` of a `kind` element shows (or, for a `file`, writes) its
    /// value.
    fn prop_sink(&mut self, p: &'a hir::Prop, kind: &str) {
        self.expr(&p.value);
        let (what, label) = sink_words(kind, Some(&p.name));
        self.sinks.push(Sink {
            file: self.file,
            span: p.value.span,
            value: &p.value,
            what,
            label,
        });
        if let Some(t) = &p.transition {
            self.expr(t);
        }
        p.sub.iter().for_each(|s| self.prop_sink(s, kind));
    }

    /// `value` is shown as `what` (a token).
    fn shown(&mut self, value: &'a Expr, span: Span, what: String) {
        self.expr(value);
        self.sinks.push(Sink {
            file: self.file,
            span,
            value,
            what: "shown on screen".into(),
            label: format!("{what} shows it"),
        });
    }

    fn stmts(&mut self, stmts: &'a [Stmt]) {
        for s in stmts {
            match &s.kind {
                StmtKind::Let { local, value } => self.flow(Place::Local(*local), value),
                StmtKind::Assign { target, value, .. } => {
                    self.expr(target);
                    self.assign(target, value);
                }
                StmtKind::Expr(e) => {
                    if let Some(f) = self.fn_result {
                        self.flows.push((Place::FnResult(f), e));
                    }
                    self.expr(e);
                }
                StmtKind::If { cond, then, else_ } => {
                    self.expr(cond);
                    self.stmts(then);
                    self.stmts(else_);
                }
                StmtKind::For {
                    binding,
                    iter,
                    body,
                } => {
                    self.flow(Place::Local(*binding), iter);
                    self.stmts(body);
                }
                StmtKind::Match { scrutinee, arms } => {
                    self.expr(scrutinee);
                    arms.iter().for_each(|(_, b)| self.stmts(b));
                }
                StmtKind::Play(e) => self.expr(e),
                StmtKind::Error => {}
            }
        }
    }

    /// `target = value` (or `+=` …).
    fn assign(&mut self, target: &'a Expr, value: &'a Expr) {
        match root(target) {
            Some(Root::Place(Place::Def(d)))
                if matches!(self.defs[d.0 as usize].kind, DefKind::Service(_)) =>
            {
                let name = shown(target, self.defs, self.locals)
                    .unwrap_or_else(|| self.defs[d.0 as usize].name.clone());
                self.expr(value);
                self.sinks.push(Sink {
                    file: self.file,
                    span: value.span,
                    value,
                    what: "written to a service".into(),
                    label: format!("written to `{name}`"),
                });
            }
            Some(Root::Place(place)) => self.flow(place, value),
            Some(Root::Service(_)) => {
                let name = shown(target, self.defs, self.locals).unwrap_or_default();
                self.expr(value);
                self.sinks.push(Sink {
                    file: self.file,
                    span: value.span,
                    value,
                    what: "written to a service".into(),
                    label: format!("written to `{name}`"),
                });
            }
            None => self.expr(value),
        }
    }

    /// Walks `e` for the calls in it: sinks, and the flows a call makes.
    fn expr(&mut self, e: &'a Expr) {
        super::grow(|| self.expr_inner(e));
    }

    fn expr_inner(&mut self, e: &'a Expr) {
        match &e.kind {
            ExprKind::Call { callee, args } => {
                match callee {
                    Callee::Action(d) => {
                        let name = self.defs[d.0 as usize].name.clone();
                        for a in args {
                            self.sinks.push(Sink {
                                file: self.file,
                                span: a.value.span,
                                value: &a.value,
                                what: "passed to a command".into(),
                                label: format!(
                                    "`{name}` runs with it in its arguments, which every user \
                                     can read in the process list"
                                ),
                            });
                        }
                    }
                    Callee::Method { receiver, name, .. } => {
                        self.expr(receiver);
                        if action(self.types, &receiver.ty, name) {
                            let owner = record_name(self.types, &receiver.ty).unwrap_or("");
                            let meant = INTENDED.iter().any(|(r, a)| *r == owner && a == name);
                            if !meant {
                                let call = shown(receiver, self.defs, self.locals)
                                    .map_or_else(|| name.clone(), |r| format!("{r}.{name}"));
                                for a in args {
                                    self.sinks.push(Sink {
                                        file: self.file,
                                        span: a.value.span,
                                        value: &a.value,
                                        what: format!("sent by `{call}`"),
                                        label: format!("`{call}` sends it"),
                                    });
                                }
                            }
                        } else if list_write(&receiver.ty, name)
                            && let Some(Root::Place(place)) = root(receiver)
                        {
                            for a in args {
                                self.flows.push((place, &a.value));
                            }
                        }
                        // A lambda given a list's (or text's) items.
                        for a in args {
                            if let ExprKind::Lambda { params, .. } = &a.value.kind {
                                for p in params {
                                    self.flows.push((Place::Local(*p), receiver));
                                    for other in args {
                                        if !matches!(other.value.kind, ExprKind::Lambda { .. }) {
                                            self.flows.push((Place::Local(*p), &other.value));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Callee::Value(v) => {
                        self.expr(v);
                        if let Some(Root::Place(place)) = root(v)
                            && matches!(v.kind, ExprKind::Local(_) | ExprKind::Def(_))
                        {
                            self.value_calls.push((place, args));
                        }
                    }
                    Callee::Fn(d) => {
                        if let Some(ps) = self.fn_params.get(d).cloned() {
                            for (i, a) in args.iter().enumerate() {
                                if let Some(&p) = ps.get(a.param.unwrap_or(i)) {
                                    self.flows.push((Place::Local(p), &a.value));
                                }
                            }
                        }
                    }
                    Callee::Builtin { .. } | Callee::Record(_) | Callee::Error => {}
                }
                args.iter().for_each(|a| self.expr(&a.value));
            }
            ExprKind::Field { base, .. } => self.expr(base),
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
            ExprKind::Lambda { body, .. } => {
                // A lambda's body returns to the lambda, not the `fn`.
                let saved = self.fn_result.take();
                match body {
                    LambdaBody::Expr(b) => self.expr(b),
                    LambdaBody::Block(b) => self.stmts(b),
                }
                self.fn_result = saved;
            }
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                arms.iter().for_each(|(_, a)| self.expr(a));
            }
            ExprKind::List(v) | ExprKind::Commas(v) | ExprKind::Spaced(v) => {
                v.iter().for_each(|x| self.expr(x));
            }
            _ => {}
        }
    }
}

/// What a value given to prop `prop` (the positional when `None`) of a
/// `kind` element does: the message's end and the label.
fn sink_words(kind: &str, prop: Option<&str>) -> (String, String) {
    match (kind, prop) {
        ("file", Some("run")) => (
            "passed to a command".into(),
            "this `file`'s `run:` runs with it in its arguments".into(),
        ),
        ("file", _) => ("written to a file".into(), "this `file` writes it".into()),
        (k, None) => ("shown on screen".into(), format!("this `{k}` shows it")),
        (k, Some(p)) => (
            "shown on screen".into(),
            format!("`{p}:` of this `{k}` shows it"),
        ),
    }
}
