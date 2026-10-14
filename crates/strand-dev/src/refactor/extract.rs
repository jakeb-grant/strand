//! "Extract component" (`refactor.extract`): the selected element and
//! everything inside it move into a new component after the enclosing
//! one, and a call replaces them.
//!
//! - Names the subtree reads from around it (the enclosing component's
//!   parameters, `for` bindings, `let`s, `screen`) become parameters, and
//!   so does the enclosing component's or surface's `state` the subtree
//!   only reads: the call passes each by name.
//! - State it writes (`=`, `<->` or a list mutation) cannot move: a
//!   component's parameters are read-only and the checker refuses `<->`
//!   to one (`check::two_way_prop`, "component parameters are not bound
//!   two-way"), so the action is offered disabled, with the reason.
//! - An element outside the selection named with `id:`, or the
//!   component's `slot`, stays put the same way.
//! - The new component is `NewComponent` (`NewComponent2`, … when the
//!   name is written anywhere in the config, defined or not), formatted
//!   with `fmt::format`. The result is checked before it is applied: an
//!   edit whose result has an error the config did not have is refused
//!   with that error (a swapped error counts, not just a higher count).
//!   [`offer`] skips that check (one more compile of the whole config),
//!   for a client that asks for the edit later (`codeAction/resolve`),
//!   so a cursor moving over element names never compiles the config.
//!
//! The subtree's state resets on the next reload: its nodes now live
//! under another component, so their identity is new (design.md,
//! "Identity and state"); the title says so.

use std::collections::{HashMap, HashSet};

use strand_compiler::fmt::format;
use strand_compiler::hir::{
    Callee, DefKind, Element, Expr, ExprKind, Item, LambdaBody, LocalKind, Node, Prop, Stmt,
    StmtKind, Target,
};
use strand_compiler::schema::list_members;
use strand_compiler::syntax::{Span, ast};
use strand_compiler::ty::Ty;
use strand_compiler::{Compiled, Diagnostic, FileId};

use crate::ctx::word_end;
use crate::workspace::Analysis;

/// The action's title when it is refused.
pub const TITLE: &str = "Extract component";

/// What "extract component" does with a selection.
#[derive(Debug)]
pub enum Extract {
    /// Replace and insert, in the selection's file.
    Edit {
        title: String,
        edits: Vec<(Span, String)>,
    },
    /// Not possible here, and why.
    Refused { reason: String },
}

/// "Extract component" for `range` in `file`: offered for the element
/// whose name the selection starts on (or that it covers whole), inside a
/// component or surface; `None` anywhere else. An edit is one that checks
/// (the config is compiled once more with it, memoised per edit).
pub fn extract(an: &Analysis, file: FileId, range: Span) -> Option<Extract> {
    run(an, file, range, true)
}

/// [`extract`] without compiling the result: what a code-action request
/// answers when the client resolves the edit later. An `Edit` here may
/// still be refused by [`extract`] (a constraint only the compiler knows).
pub fn offer(an: &Analysis, file: FileId, range: Span) -> Option<Extract> {
    run(an, file, range, false)
}

fn run(an: &Analysis, file: FileId, range: Span, check: bool) -> Option<Extract> {
    let src = an.text(file);
    let range = trim(src, range);
    let item = an.parse(file)?.file.items.iter().find(|i| {
        contains(i.span, range)
            && matches!(
                i.kind,
                ast::ItemKind::Component(_) | ast::ItemKind::Surface(_)
            )
    })?;
    let p = &an.compiled.program;
    let hir = p.files.iter().find(|f| f.file == file)?;
    let (owner, nodes) = hir.items.iter().find_map(|i| match i {
        Item::Component(c) if contains(item.span, p.def(c.def).span) => {
            Some((format!("`{}`", p.def(c.def).name), &c.body[..]))
        }
        Item::Surface(s) if contains(item.span, s.element.span) => Some((
            s.def
                .map_or_else(|| "the surface".into(), |d| format!("`{}`", p.def(d).name)),
            &s.element.children[..],
        )),
        _ => None,
    })?;
    let mut best = None;
    innermost(nodes, range, &mut best);
    let el = best?;
    if range.start as usize > word_end(src, el.span.start as usize) {
        return None;
    }
    Some(match plan(an, file, item.span, &owner, el, check) {
        Ok((title, edits)) => Extract::Edit { title, edits },
        Err(reason) => Extract::Refused { reason },
    })
}

type Plan = (String, Vec<(Span, String)>);

fn plan(
    an: &Analysis,
    file: FileId,
    item: Span,
    owner: &str,
    el: &Element,
    check: bool,
) -> Result<Plan, String> {
    let p = &an.compiled.program;
    let src = an.text(file);
    let inside = |f: FileId, s: Span| f == file && contains(el.span, s);

    // What the subtree reads from around it, in order of first use.
    let mut free: Vec<(String, Ty, Target, String)> = Vec::new();
    for r in p.refs.iter().filter(|r| inside(r.file, r.span)) {
        let (name, ty, what) = match &r.target {
            Target::Local(l) => {
                let local = p.local(*l);
                if inside(local.file, local.span) {
                    continue;
                }
                if let LocalKind::NodeId(_) | LocalKind::NodeAlias(_) = local.kind {
                    return Err(format!(
                        "it refers to `{}`, an element outside the selection",
                        local.name
                    ));
                }
                (
                    &local.name,
                    &local.ty,
                    "bound outside the selection".to_string(),
                )
            }
            Target::Def(d) => {
                let def = p.def(*d);
                let nested =
                    def.owner.is_some() && matches!(def.kind, DefKind::State | DefKind::Let);
                if !nested || inside(def.file, def.span) {
                    continue;
                }
                let what = match def.kind {
                    DefKind::State => format!("state of {owner}"),
                    _ => format!("a `let` of {owner}"),
                };
                (&def.name, &def.ty, what)
            }
            _ => continue,
        };
        if free.iter().any(|f| f.2 == r.target) {
            continue;
        }
        if free.iter().any(|f| &f.0 == name) {
            return Err(format!("it uses two different `{name}`s"));
        }
        if ty.is_error() || ty.is_lenient() {
            return Err(format!("the type of `{name}` is not known"));
        }
        free.push((name.clone(), ty.clone(), r.target.clone(), what));
    }

    // What it writes, and whether it holds the slot.
    let mut found = Found::default();
    found.element(el);
    if found.slot {
        return Err("it holds the component's `slot`".into());
    }
    for (target, how) in &found.writes {
        if let Some((name, .., what)) = free.iter().find(|f| &f.2 == target) {
            return Err(format!(
                "it writes `{name}` ({how}), {what}: a component's parameters are read-only and never bound two-way"
            ));
        }
    }

    // Every name written anywhere in the config is taken, not only the
    // defined ones: a call to an undefined `NewComponent` would otherwise
    // bind to the new component.
    let mut taken: HashSet<&str> = p.defs.iter().map(|d| d.name.as_str()).collect();
    for (_, s) in an.map.iter() {
        taken.extend(words(&s.text));
    }
    let name = (1..)
        .map(|i| match i {
            1 => "NewComponent".to_string(),
            i => format!("NewComponent{i}"),
        })
        .find(|n| !taken.contains(n.as_str()))
        .unwrap_or_default();
    let params: Vec<String> = free
        .iter()
        .map(|(n, ty, ..)| format!("{n}: {}", p.types.show(ty)))
        .collect();
    let head = match params.is_empty() {
        true => format!("component {name}"),
        false => format!("component {name}({})", params.join(", ")),
    };
    let body = src
        .get(el.span.start as usize..el.span.end as usize)
        .unwrap_or("");
    let component = format!("{head} {{\n{body}\n}}\n");
    let component = format(&component).unwrap_or(component);
    let call = match free.is_empty() {
        true => name.clone(),
        false => {
            let args: Vec<String> = free.iter().map(|(n, ..)| format!("{n}: {n}")).collect();
            format!("{name} {{ {} }}", args.join("; "))
        }
    };
    let end = Span::new(item.end, item.end);
    let insert = format!("\n\n{}", component.trim_end());

    // Refuse what would not check, rather than hand the editor an error.
    let mut text = src.to_string();
    text.replace_range(item.end as usize..item.end as usize, &insert);
    text.replace_range(el.span.start as usize..el.span.end as usize, &call);
    let after = match check {
        true => Some(an.with_edited(file, &text)),
        false => None,
    };
    if let Some(after) = after
        && let Some(new) = added_error(&an.compiled, &after.compiled)
    {
        return Err(format!("the result would not check: {}", new.message));
    }
    Ok((
        format!("Extract component `{name}` (state inside it resets on reload)"),
        vec![(el.span, call), (end, insert)],
    ))
}

/// The identifiers in `src` (and any other run of identifier
/// characters: a comment's words only make a name look taken).
fn words(src: &str) -> impl Iterator<Item = &str> {
    src.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
}

/// An error in `after` that `before` does not have (by code and message,
/// counted): an edit that clears one error and adds another still adds
/// one.
fn added_error<'a>(before: &Compiled, after: &'a Compiled) -> Option<&'a Diagnostic> {
    let mut seen: HashMap<(&str, &str), usize> = HashMap::new();
    for d in before.diagnostics.iter().filter(|d| d.is_error()) {
        *seen.entry((d.code, d.message.as_str())).or_default() += 1;
    }
    after.diagnostics.iter().filter(|d| d.is_error()).find(|d| {
        match seen.get_mut(&(d.code, d.message.as_str())) {
            Some(n) if *n > 0 => {
                *n -= 1;
                false
            }
            _ => true,
        }
    })
}

fn contains(outer: Span, inner: Span) -> bool {
    outer.start <= inner.start && inner.end <= outer.end
}

/// `range` without the whitespace around it, so whole selected lines
/// select the element on them.
fn trim(src: &str, range: Span) -> Span {
    let b = src.as_bytes();
    let (mut s, mut e) = (range.start as usize, (range.end as usize).min(b.len()));
    while s < e && b[s].is_ascii_whitespace() {
        s += 1;
    }
    while e > s && b[e - 1].is_ascii_whitespace() {
        e -= 1;
    }
    Span::new(s as u32, e.max(s) as u32)
}

/// The innermost element of `nodes` containing `range`.
fn innermost<'p>(nodes: &'p [Node], range: Span, best: &mut Option<&'p Element>) {
    for n in nodes {
        match n {
            Node::Element(e) if contains(e.span, range) => {
                *best = Some(e);
                innermost(&e.children, range, best);
            }
            Node::If(i) if contains(i.span, range) => {
                innermost(&i.then, range, best);
                innermost(&i.else_, range, best);
            }
            Node::For(f) if contains(f.span, range) => innermost(&f.body, range, best),
            Node::Match(m) if contains(m.span, range) => {
                for (_, body) in &m.arms {
                    innermost(body, range, best);
                }
            }
            _ => {}
        }
    }
}

/// The places a subtree writes (by the name they start from) and whether
/// it holds a `slot`.
#[derive(Default)]
struct Found {
    writes: Vec<(Target, String)>,
    slot: bool,
}

/// The name a place starts from: `prefs` in `prefs.pins[0].label`.
fn root(e: &Expr) -> Option<Target> {
    match &e.kind {
        ExprKind::Local(l) => Some(Target::Local(*l)),
        ExprKind::Def(d) => Some(Target::Def(*d)),
        ExprKind::Field { base, .. } | ExprKind::Index { base, .. } => root(base),
        _ => None,
    }
}

impl Found {
    fn write(&mut self, place: &Expr, how: String) {
        if let Some(t) = root(place) {
            self.writes.push((t, how));
        }
    }

    fn element(&mut self, e: &Element) {
        if let Some(a) = &e.arg {
            self.expr(a);
        }
        self.props(&e.props);
        self.nodes(&e.children);
    }

    fn props(&mut self, props: &[Prop]) {
        for prop in props {
            if prop.two_way {
                self.write(&prop.value, "`<->`".into());
            }
            self.expr(&prop.value);
            if let Some(t) = &prop.transition {
                self.expr(t);
            }
            self.props(&prop.sub);
        }
    }

    fn nodes(&mut self, nodes: &[Node]) {
        for n in nodes {
            match n {
                Node::Element(e) => self.element(e),
                Node::When(w) => {
                    self.expr(&w.cond);
                    self.props(&w.props);
                }
                Node::If(i) => {
                    self.expr(&i.cond);
                    self.nodes(&i.then);
                    self.nodes(&i.else_);
                }
                Node::For(f) => {
                    self.expr(&f.iter);
                    self.nodes(&f.body);
                }
                Node::Match(m) => {
                    self.expr(&m.scrutinee);
                    for (_, body) in &m.arms {
                        self.nodes(body);
                    }
                }
                Node::Handler(h) => self.stmts(&h.body),
                Node::Timer(t) => {
                    self.expr(&t.duration);
                    self.stmts(&t.body);
                }
                Node::Pose(x) => self.props(&x.props),
                Node::Selector(x) => self.props(&x.props),
                Node::Slot(_) => self.slot = true,
                Node::Play(e) => self.expr(e),
                Node::State(_) | Node::Let(_) | Node::Set(..) => {}
            }
        }
    }

    fn stmts(&mut self, stmts: &[Stmt]) {
        for s in stmts {
            match &s.kind {
                StmtKind::Assign { target, value, .. } => {
                    self.write(target, "`=`".into());
                    self.expr(value);
                }
                StmtKind::Let { value: e, .. } | StmtKind::Expr(e) | StmtKind::Play(e) => {
                    self.expr(e)
                }
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
                    for (_, body) in arms {
                        self.stmts(body);
                    }
                }
                StmtKind::Error => {}
            }
        }
    }

    fn expr(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Call { callee, args } => {
                match callee {
                    Callee::Method { receiver, name, .. } => {
                        // A list method that changes the list (`push`,
                        // `remove_key`, …) writes the place it is called on.
                        if let Some((elem, keyed)) = receiver.ty.list_elem()
                            && list_members(elem, keyed, &Ty::Error)
                                .iter()
                                .any(|m| &m.name == name && m.writes)
                        {
                            self.write(receiver, format!("`.{name}()`"));
                        }
                        self.expr(receiver);
                    }
                    Callee::Value(f) => self.expr(f),
                    _ => {}
                }
                for a in args {
                    self.expr(&a.value);
                }
            }
            ExprKind::Field { base, .. } | ExprKind::Unary { expr: base, .. } => self.expr(base),
            ExprKind::Index { base, index } => {
                self.expr(base);
                self.expr(index);
            }
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
                for (_, x) in arms {
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use strand_compiler::schema::Schema;

    use super::*;
    use crate::workspace::{ConfigKey, Input};

    /// [`offer`] never compiles the result; [`extract`] compiles it once
    /// per edit, however often it is asked from the same element name.
    #[test]
    fn the_result_is_compiled_once_per_edit_and_only_when_checked() {
        let uri = "file:///t.strand".to_string();
        let src = "component Side(label: text) {\n  col {\n    row { text label }\n  }\n}\n";
        let an = Analysis::new(
            ConfigKey::Single(uri.clone()),
            vec![Input::new(uri.clone(), "t.strand".into(), src.into())],
            Arc::new(Schema::builtin().clone()),
        );
        let file = an.file(&uri).unwrap();
        let at = src.find("row").unwrap() as u32;
        let offered = offer(&an, file, Span::new(at, at));
        assert!(matches!(offered, Some(Extract::Edit { .. })), "{offered:?}");
        assert!(an.edited().is_none());
        let first = extract(&an, file, Span::new(at, at));
        assert!(matches!(first, Some(Extract::Edit { .. })), "{first:?}");
        let memo = an.edited().unwrap();
        for d in 1..3 {
            let again = extract(&an, file, Span::new(at + d, at + d));
            assert!(matches!(again, Some(Extract::Edit { .. })), "{again:?}");
            assert!(Arc::ptr_eq(&memo, &an.edited().unwrap()));
        }
    }

    fn analysis(src: &str) -> (Analysis, FileId) {
        let uri = "file:///t.strand".to_string();
        let an = Analysis::new(
            ConfigKey::Single(uri.clone()),
            vec![Input::new(uri.clone(), "t.strand".into(), src.into())],
            Arc::new(Schema::builtin().clone()),
        );
        let file = an.file(&uri).unwrap();
        (an, file)
    }

    const PLACEHOLDER: &str = "component Side(label: text) {\n  state open = false\n  col {\n    row { text label { color: open ? $accent : $fg } }\n  }\n}\nbar Top {\n  Side { label: \"x\" }\n}\nbar Other {\n  NewComponent { label: \"x\" }\n}\n";

    /// A name called but never defined is taken: the new component must
    /// not become what an unresolved `NewComponent` call means.
    #[test]
    fn an_undefined_name_in_use_is_not_the_new_components() {
        let (an, file) = analysis(PLACEHOLDER);
        assert_eq!(an.compiled.errors(), 1, "{:?}", an.compiled.diagnostics);
        let at = PLACEHOLDER.find("row").unwrap() as u32;
        let Some(Extract::Edit { title, edits }) = extract(&an, file, Span::new(at, at)) else {
            panic!("no edit");
        };
        assert!(title.contains("`NewComponent2`"), "{title}");
        assert!(
            edits.iter().all(|(_, t)| !t.contains("NewComponent ")),
            "{edits:?}"
        );
    }

    /// An edit that clears one error and adds another adds an error,
    /// though the count stays the same.
    #[test]
    fn a_swapped_error_is_an_added_error() {
        let (before, _) = analysis(PLACEHOLDER);
        let defined = format!(
            "{PLACEHOLDER}component NewComponent(label: text, open: bool) {{\n  text label\n}}\n"
        );
        let (after, _) = analysis(&defined);
        assert_eq!(before.compiled.errors(), after.compiled.errors());
        let added = added_error(&before.compiled, &after.compiled);
        assert_eq!(
            added.map(|d| d.code),
            Some("check::missing_arg"),
            "{added:?}"
        );
        assert!(added_error(&before.compiled, &before.compiled).is_none());
    }
}
