//! Trees: components, surfaces, elements and their props, `when`, `if`,
//! `for`, `match`, poses, `state`/`let` placement, keyframes and services.

use std::sync::Arc;

use super::collect::first_program;
use super::expr::path_text;
use super::{Checker, NodeCtx, TOP_KEYWORDS, TREE_KEYWORDS};
use crate::hir::{self, DefId, DefKind, ElementKind, LocalKind, Node, Target};
use crate::schema::PropSchema;
use crate::syntax::Span;
use crate::syntax::ast::{self, ItemKind};
use crate::ty::{FieldDef, FnSig, Origin, ParamSig, Prim, RecordDef, Ty};

/// What a tree block may hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Place {
    /// A component's body: elements, state, handlers; no props.
    Component,
    /// An element's block. `root` for a surface or a list item's element,
    /// which may hold `state`.
    Element { root: bool },
    /// An `if`/`for`/`match` body. `list_item` for a `for` body.
    Children { list_item: bool },
    /// `when`, poses, selectors, keyframe stops: props only.
    Props,
    /// A component call's block: parameters and children for its `slot`.
    Call,
}

/// A prop as written: in a block, or the named head of an element
/// (`pages current: page`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct PropRef<'a> {
    pub name: &'a ast::Ident,
    pub two_way: Option<Span>,
    pub value: &'a ast::Expr,
    pub transition: Option<&'a ast::Expr>,
    pub block: Option<&'a ast::Block<ast::Item>>,
}

impl<'a> PropRef<'a> {
    pub fn of(p: &'a ast::Prop) -> Self {
        Self {
            name: &p.name,
            two_way: p.two_way,
            value: &p.value,
            transition: p.transition.as_ref(),
            block: p.block.as_ref(),
        }
    }

    fn head(name: &'a ast::Ident, value: &'a ast::Expr) -> Self {
        Self {
            name,
            two_way: None,
            value,
            transition: None,
            block: None,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CallCtx {
    pub def: DefId,
    pub name: String,
    pub filled: Vec<bool>,
    pub has_slot: bool,
    pub reported_children: bool,
}

impl<'a> Checker<'a> {
    // -----------------------------------------------------------------------
    // Declarations with trees

    pub(super) fn component(&mut self, id: DefId, c: &'a ast::Component) -> hir::Component {
        let sig = self.comp_sigs.get(&id).cloned();
        let saved = self.ctx;
        self.ctx.component = Some(id);
        self.ctx.owner = Some(id);
        self.ctx.handler = false;
        self.ctx.prop = false;
        self.push_scope();
        let mut params = Vec::new();
        for (i, p) in c.params.iter().flatten().enumerate() {
            let ty = sig
                .as_ref()
                .and_then(|s| s.params.get(i))
                .map_or(Ty::Error, |p| p.ty.clone());
            let local = self.bind_local(&p.name.name, ty, p.name.span, LocalKind::Param);
            self.add_ref(p.name.span, Target::Local(local));
            if p.ty.is_none() && p.default.is_none() {
                self.untyped.insert(local);
            }
            params.push(hir::Param {
                local,
                default: self.param_defaults.remove(&(id, i)),
            });
        }
        let tokens = match self.comp_tokens.get(&id).cloned() {
            Some(entries) => self.token_defs(&entries),
            None => Vec::new(),
        };
        self.collect_ids(&c.body.items);
        let (_, body) = self.tree_items(&c.body.items, Place::Component);
        self.pop_scope(); // ids
        self.pop_scope();
        self.ctx = saved;
        hir::Component {
            def: id,
            params,
            tokens,
            body,
            has_slot: sig.is_some_and(|s| s.has_slot),
        }
    }

    pub(super) fn surface(
        &mut self,
        def: Option<DefId>,
        s: &'a ast::Surface,
        span: Span,
    ) -> hir::Surface {
        let kind = s.kind.name.clone();
        let schema = self.element_schema(&kind);
        let saved = self.ctx;
        self.ctx.owner = def;
        self.ctx.component = None;
        self.ctx.handler = false;
        self.ctx.prop = false;
        let node = self.new_node();
        self.add_ref(s.kind.span, Target::Element(kind.clone()));
        self.nodes.push(NodeCtx {
            idx: node,
            kind: kind.clone(),
            schema,
        });
        self.push_scope();
        if let Some(sc) = schema {
            for (n, t) in &sc.scope {
                self.bind_local(n, t.clone(), s.kind.span, LocalKind::ElementScope);
            }
        }
        self.collect_ids(&s.body.items);
        let (props, children) = self.tree_items(&s.body.items, Place::Element { root: true });
        self.pop_scope();
        self.pop_scope();
        self.nodes.pop();
        self.ctx = saved;
        hir::Surface {
            def,
            element: hir::Element {
                node,
                kind: ElementKind::Builtin(kind),
                span,
                arg: None,
                id: None,
                props,
                children,
            },
        }
    }

    /// Finds the `id:` names of a component or surface body, so they can be
    /// read anywhere in it, and binds them in a new scope.
    fn collect_ids(&mut self, items: &'a [ast::Item]) {
        self.push_scope();
        let mut found: Vec<(&'a ast::Ident, Span)> = Vec::new();
        fn walk<'b>(items: &'b [ast::Item], out: &mut Vec<(&'b ast::Ident, Span)>) {
            for i in items {
                match &i.kind {
                    ItemKind::Element(e) => {
                        if let Some(b) = &e.block {
                            for p in &b.items {
                                if let ItemKind::Prop(p) = &p.kind
                                    && p.name.name == "id"
                                    && let ast::ExprKind::Name(n) = &p.value.kind
                                {
                                    out.push((n, i.span));
                                }
                            }
                            walk(&b.items, out);
                        }
                    }
                    ItemKind::If(f) => walk_if(f, out),
                    ItemKind::For(f) => walk(&f.body.items, out),
                    ItemKind::Match(m) => {
                        for a in &m.arms {
                            match &a.body {
                                ast::ArmBody::Block(b) => walk(&b.items, out),
                                ast::ArmBody::Single(s) => walk(std::slice::from_ref(&**s), out),
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        fn walk_if<'b>(f: &'b ast::If<ast::Item>, out: &mut Vec<(&'b ast::Ident, Span)>) {
            walk(&f.then.items, out);
            match &f.else_ {
                Some(ast::Else::If(i, _)) => walk_if(i, out),
                Some(ast::Else::Block(b)) => walk(&b.items, out),
                None => {}
            }
        }
        walk(items, &mut found);
        let node_ty = self.types.find_record("Node").map_or(Ty::Error, Ty::Record);
        let mut seen: Vec<(String, Span)> = Vec::new();
        for (name, el_span) in found {
            if let Some((_, prev)) = seen.iter().find(|(n, _)| *n == name.name) {
                let (file, prev) = (self.file(), *prev);
                self.error(
                    "check::redeclared",
                    format!("`id: {}` is used twice", name.name),
                    name.span,
                    "named again here",
                )
                .add_secondary(file, prev, "first named here");
                continue;
            }
            seen.push((name.name.clone(), name.span));
            let idx = self.new_node();
            let local = self.bind_local(
                &name.name,
                node_ty.clone(),
                name.span,
                LocalKind::NodeId(idx),
            );
            self.ids.insert((self.module, el_span), (local, idx));
        }
    }

    // -----------------------------------------------------------------------
    // Blocks

    pub(crate) fn tree_items(
        &mut self,
        items: &'a [ast::Item],
        place: Place,
    ) -> (Vec<hir::Prop>, Vec<Node>) {
        let mut out = (Vec::new(), Vec::new());
        if place == Place::Props {
            self.push_scope();
        } else {
            self.declare_block(items);
        }
        for item in items {
            self.tree_item(item, place, &mut out);
        }
        self.pop_scope();
        out
    }

    /// One item of a tree block. Trees nest deeply, so this dispatcher and
    /// the per-kind functions below keep their frames small.
    #[inline(never)]
    fn tree_item(
        &mut self,
        item: &'a ast::Item,
        place: Place,
        out: &mut (Vec<hir::Prop>, Vec<Node>),
    ) {
        let span = item.span;
        if !matches!(item.kind, ItemKind::State(_) | ItemKind::Let(_)) {
            self.reset_attr(item);
        }
        let structural = matches!(
            item.kind,
            ItemKind::Element(_) | ItemKind::If(_) | ItemKind::For(_) | ItemKind::Match(_)
        );
        if structural {
            if place == Place::Props {
                let what = match &item.kind {
                    ItemKind::Element(e) => e.kind.name.as_str(),
                    ItemKind::If(_) => "if",
                    ItemKind::For(_) => "for",
                    _ => "match",
                };
                self.props_only(span, what);
                return;
            }
            self.child_allowed(place, span);
        }
        let node = match &item.kind {
            ItemKind::Prop(p) => {
                self.item_prop(p, span, place, &mut out.0);
                None
            }
            ItemKind::Element(e) => {
                self.element(e, span, place == Place::Children { list_item: true })
            }
            ItemKind::When(w) => self.item_when(w, span, place),
            ItemKind::If(i) => Some(Node::If(self.tree_if(i, span))),
            ItemKind::For(f) => Some(Node::For(self.tree_for(f, span))),
            ItemKind::Match(m) => Some(Node::Match(self.tree_match(m, span))),
            ItemKind::On(o) => self.item_on(o, span, place),
            ItemKind::Timer(t) => self.item_timer(t, span, place),
            ItemKind::Pose(p) => self.item_pose(p, span, place),
            ItemKind::Slot => self.item_slot(span, place),
            ItemKind::Set(b) => self.item_set(b, span, place),
            ItemKind::Selector(s) => self.item_selector(s, span, place),
            ItemKind::Play(e) => self.item_play(e, span, place),
            ItemKind::State(s) => self.item_state(s, span, place),
            ItemKind::Let(l) => self.item_let(l, span, place),
            // Declarations here were reported by the parser.
            _ => None,
        };
        out.1.extend(node);
    }

    #[inline(never)]
    fn item_prop(
        &mut self,
        p: &'a ast::Prop,
        span: Span,
        place: Place,
        props: &mut Vec<hir::Prop>,
    ) {
        match place {
            Place::Element { .. } | Place::Props => {
                props.push(self.node_prop(PropRef::of(p), span))
            }
            Place::Call => props.extend(self.call_prop(PropRef::of(p), span)),
            Place::Component | Place::Children { .. } => {
                let where_ = if place == Place::Component {
                    "a component's body"
                } else {
                    "an `if`, `for` or `match` body"
                };
                self.error(
                    "check::misplaced",
                    format!("props cannot go directly in {where_}"),
                    p.name.span,
                    "not on an element",
                )
                .help = Some(if place == Place::Component {
                    "put it on an element: `row { … }`".to_string()
                } else {
                    "to style conditionally use `when cond { … }`; to choose children use `if`"
                        .to_string()
                });
                let saved = self.ctx;
                self.ctx.prop = true;
                self.expr(&p.value, None);
                self.ctx = saved;
            }
        }
    }

    #[inline(never)]
    fn item_when(&mut self, w: &'a ast::When, span: Span, place: Place) -> Option<Node> {
        if !self.node_needed(place, "`when`", span) {
            return None;
        }
        let cond = self.expect(&w.cond, &Ty::BOOL, "`when`");
        let (props, _) = self.tree_items(&w.body.items, Place::Props);
        Some(Node::When(hir::When { cond, props, span }))
    }

    #[inline(never)]
    fn item_on(&mut self, o: &'a ast::On, span: Span, place: Place) -> Option<Node> {
        if matches!(place, Place::Props | Place::Call) {
            self.not_here(place, span, "handlers");
            return None;
        }
        self.handler(o, span).map(Node::Handler)
    }

    #[inline(never)]
    fn item_timer(&mut self, t: &'a ast::Timer, span: Span, place: Place) -> Option<Node> {
        if matches!(place, Place::Props | Place::Call) {
            self.not_here(place, span, "timers");
            return None;
        }
        Some(Node::Timer(self.timer(t, span)))
    }

    #[inline(never)]
    fn item_pose(&mut self, p: &'a ast::Pose, span: Span, place: Place) -> Option<Node> {
        if place == Place::Call {
            self.not_here(place, span, "poses");
            return None;
        }
        if !self.node_needed(place, "a pose", span) {
            return None;
        }
        let (props, _) = self.tree_items(&p.body.items, Place::Props);
        Some(Node::Pose(hir::Pose {
            kind: p.kind,
            props,
            span,
        }))
    }

    #[inline(never)]
    fn item_slot(&mut self, span: Span, place: Place) -> Option<Node> {
        if self.ctx.component.is_none() || place == Place::Props {
            self.error(
                "check::misplaced",
                "`slot` belongs in a component's tree",
                span,
                "no component here",
            )
            .help = Some("a component renders the children its caller passes at `slot`".into());
            return None;
        }
        Some(Node::Slot(span))
    }

    #[inline(never)]
    fn item_set(
        &mut self,
        b: &'a ast::Block<ast::TokenEntry>,
        span: Span,
        place: Place,
    ) -> Option<Node> {
        if place == Place::Props {
            self.props_only(span, "set");
            return None;
        }
        Some(Node::Set(self.set_block(b), span))
    }

    #[inline(never)]
    fn item_selector(&mut self, s: &'a ast::Selector, span: Span, place: Place) -> Option<Node> {
        let ok = self
            .nodes
            .last()
            .and_then(|n| n.schema)
            .is_some_and(|s| s.flags.selectors)
            && place == Place::Element { root: false };
        if !ok {
            self.error(
                "check::misplaced",
                format!("`#{}` selects a part of an `svg`", s.name.name),
                s.name.span,
                "not inside an `svg`",
            );
            return None;
        }
        let (props, _) = self.tree_items(&s.body.items, Place::Props);
        Some(Node::Selector(hir::Selector {
            name: s.name.name.clone(),
            props,
            span,
        }))
    }

    #[inline(never)]
    fn item_play(&mut self, e: &'a ast::Expr, span: Span, place: Place) -> Option<Node> {
        if place == Place::Props {
            self.props_only(span, "play");
            return None;
        }
        Some(Node::Play(self.play_target(e)))
    }

    #[inline(never)]
    fn item_state(&mut self, s: &'a ast::State, span: Span, place: Place) -> Option<Node> {
        if place == Place::Props {
            self.props_only(span, "state");
            return None;
        }
        let allowed = matches!(
            place,
            Place::Component | Place::Element { root: true } | Place::Children { list_item: true }
        );
        if !allowed {
            let on = self
                .nodes
                .last()
                .map_or("here".to_string(), |n| format!("on a `{}`", n.kind));
            self.error(
                "check::state_placement",
                "`state` lives on components, surfaces and list items",
                s.name.span,
                format!("not {on}"),
            )
            .help = Some(
                "move it up to the component (or surface), or into the `for` body it belongs to"
                    .into(),
            );
        }
        let id = *self.item_defs.get(&(self.module, s.name.span))?;
        match self.take_done(id) {
            Some(super::DoneHir::State(st)) => Some(Node::State(st)),
            _ => None,
        }
    }

    #[inline(never)]
    fn item_let(&mut self, l: &'a ast::Let, span: Span, place: Place) -> Option<Node> {
        if place == Place::Props {
            self.props_only(span, "let");
            return None;
        }
        let id = *self.item_defs.get(&(self.module, l.name.span))?;
        match self.take_done(id) {
            Some(super::DoneHir::Let(lt)) => Some(Node::Let(lt)),
            _ => None,
        }
    }

    fn props_only(&mut self, span: Span, what: &str) {
        self.error(
            "check::misplaced",
            "this block holds props only",
            span,
            format!("`{what}` is not a prop"),
        )
        .help = Some(
            "`when`, poses and selectors set props of their element; use `if` to add children"
                .into(),
        );
    }

    fn not_here(&mut self, place: Place, span: Span, what: &str) {
        let (msg, help) = if place == Place::Call {
            (
                format!("{what} cannot go in a component call"),
                "put them inside the component's own elements".to_string(),
            )
        } else {
            (
                format!("{what} cannot go in this block"),
                "this block holds props only".to_string(),
            )
        };
        self.error("check::misplaced", msg, span, "not here").help = Some(help);
    }

    /// `when` and poses need the element they style.
    fn node_needed(&mut self, place: Place, what: &str, span: Span) -> bool {
        if place == Place::Props {
            self.props_only(span, what.trim_matches('`'));
            return false;
        }
        if place == Place::Call || self.nodes.is_empty() || place == Place::Component {
            self.error(
                "check::no_node",
                format!("{what} styles an element, and there is none here"),
                span,
                "not inside an element",
            )
            .help = Some("put it inside the element it styles".into());
            return false;
        }
        true
    }

    /// Children of a component call go to its `slot`.
    fn child_allowed(&mut self, place: Place, span: Span) {
        if place != Place::Call {
            return;
        }
        let Some(call) = self.calls.last_mut() else {
            return;
        };
        if !call.has_slot && !call.reported_children {
            call.reported_children = true;
            let name = call.name.clone();
            self.error(
                "check::children_not_allowed",
                format!("`{name}` has no `slot`, so it takes no children"),
                span,
                "a child",
            )
            .help = Some(format!(
                "add `slot` where `{name}` should render its children"
            ));
        }
    }

    // -----------------------------------------------------------------------
    // Structure

    fn tree_if(&mut self, i: &'a ast::If<ast::Item>, span: Span) -> hir::IfNode {
        let cond = self.expect(&i.cond, &Ty::BOOL, "the condition");
        let (_, then) = self.tree_items(&i.then.items, Place::Children { list_item: false });
        let else_ = match &i.else_ {
            None => Vec::new(),
            Some(ast::Else::Block(b)) => {
                self.tree_items(&b.items, Place::Children { list_item: false })
                    .1
            }
            Some(ast::Else::If(inner, s)) => vec![Node::If(self.tree_if(inner, *s))],
        };
        hir::IfNode {
            cond,
            then,
            else_,
            span,
        }
    }

    fn tree_for(&mut self, f: &'a ast::For<ast::Item>, span: Span) -> hir::ForNode {
        let iter = self.expr(&f.iter, None);
        let elem = self.iter_elem(&iter, &f.iter);
        let keyed = iter.ty.list_elem().is_some_and(|(_, k)| k);
        self.push_scope();
        let binding = self.bind_local(
            &f.binding.name,
            elem.clone(),
            f.binding.span,
            LocalKind::ForBinding,
        );
        self.add_ref(f.binding.span, Target::Local(binding));
        let key = f.key.as_ref().map(|k| {
            let h = self.expr(k, None);
            if !h.ty.is_data() {
                let shown = self.show(&h.ty);
                self.error(
                    "check::type_mismatch",
                    format!("a key must be plain data, not `{shown}`"),
                    k.span,
                    "not comparable",
                );
            }
            h
        });
        if key.is_none() && !keyed && !iter.ty.is_lenient() && !elem.is_lenient() {
            let b = &f.binding.name;
            let suggestion = match &elem {
                Ty::Record(r) => {
                    let rec = self.types.record(*r);
                    ["id", "name", "key"]
                        .into_iter()
                        .find(|n| rec.field(n).is_some())
                        .map_or_else(|| b.clone(), |n| format!("{b}.{n}"))
                }
                _ => b.clone(),
            };
            let what =
                path_text(&f.iter).map_or_else(|| "this list".to_string(), |p| format!("`{p}`"));
            self.error(
                "check::missing_key",
                format!("{what} is plain data, so `for` needs a `key`"),
                f.iter.span,
                "items have no identity",
            )
            .help = Some(format!(
                "say what identifies an item: `for {b} in … key {suggestion}`"
            ));
        }
        let (_, body) = self.tree_items(&f.body.items, Place::Children { list_item: true });
        self.pop_scope();
        hir::ForNode {
            binding,
            iter,
            key,
            body,
            span,
        }
    }

    fn tree_match(
        &mut self,
        m: &'a ast::Match<ast::ArmBody<ast::Item>>,
        span: Span,
    ) -> hir::MatchNode {
        let scrutinee = self.expr(&m.scrutinee, None);
        let mut arms = Vec::new();
        let mut pats = Vec::new();
        for arm in &m.arms {
            let p = self.pattern(&arm.pattern, &scrutinee.ty);
            pats.push((p.clone(), arm.pattern.span));
            let body = match &arm.body {
                ast::ArmBody::Block(b) => {
                    self.tree_items(&b.items, Place::Children { list_item: false })
                        .1
                }
                ast::ArmBody::Single(i) => {
                    self.tree_items(
                        std::slice::from_ref(&**i),
                        Place::Children { list_item: false },
                    )
                    .1
                }
            };
            arms.push((p, body));
        }
        self.exhaustive(&scrutinee.ty, &pats, m.arms_span);
        hir::MatchNode {
            scrutinee,
            arms,
            span,
        }
    }

    // -----------------------------------------------------------------------
    // Elements

    #[inline(never)]
    fn element(&mut self, el: &'a ast::Element, span: Span, list_item: bool) -> Option<Node> {
        let kind = el.kind.name.as_str();
        if let Some(schema) = self.element_schema(kind) {
            return Some(self.builtin_element(el, schema, span, list_item));
        }
        self.other_element(el, span)
    }

    #[inline(never)]
    fn other_element(&mut self, el: &'a ast::Element, span: Span) -> Option<Node> {
        let kind = el.kind.name.as_str();
        if let Some(&d) = self.globals.get(kind) {
            match self.defs[d.0 as usize].kind {
                DefKind::Component => return Some(self.component_call(d, el, span)),
                DefKind::Surface(_) => {
                    self.error(
                        "check::misplaced",
                        format!("`{kind}` is a surface, not an element"),
                        el.kind.span,
                        "cannot be placed",
                    )
                    .help =
                        Some("surfaces stand alone; move shared parts into a `component`".into());
                    return None;
                }
                _ => {}
            }
        }
        self.unknown_element(el, span)
    }

    /// Checks before the block: placement, the node and the positional.
    #[inline(never)]
    fn element_head(
        &mut self,
        el: &'a ast::Element,
        schema: &'a crate::schema::ElementSchema,
        span: Span,
    ) -> (Option<hir::LocalId>, hir::NodeIdx, Option<hir::Expr>) {
        let kind = el.kind.name.as_str();
        if schema.flags.surface {
            self.error(
                "check::misplaced",
                format!("`{kind}` is a surface, declared at the top level"),
                el.kind.span,
                "not inside a tree",
            )
            .help = Some(format!(
                "declare it on its own: `{kind} Name {{ … }}`; inside a tree, `popup` anchors a surface"
            ));
        }
        if let Some(parent) = &schema.flags.only_in
            && self.nodes.last().is_none_or(|n| n.kind != *parent)
        {
            self.error(
                "check::misplaced",
                format!("`{kind}` goes directly inside `{parent}`"),
                el.kind.span,
                format!("not in a `{parent}`"),
            );
        }
        self.add_ref(el.kind.span, Target::Element(kind.to_string()));
        let (id, node) = match self.ids.get(&(self.module, span)) {
            Some(&(l, n)) => (Some(l), n),
            None => (None, self.new_node()),
        };
        let saved = self.ctx;
        self.ctx.prop = false;
        let arg = match &el.arg {
            None | Some(ast::HeadArg::Named(..)) => None,
            Some(ast::HeadArg::Positional(e)) => {
                let want = if kind == "page" {
                    self.pages_current.last().cloned().or(Some(Ty::Any))
                } else {
                    schema.arg.clone()
                };
                match want {
                    Some(t) => Some(self.expect(e, &t, &format!("`{kind}`"))),
                    None => {
                        self.error(
                            "check::too_many_args",
                            format!("`{kind}` takes no positional value"),
                            e.span,
                            "nothing to fill",
                        )
                        .help = Some("set its props in the block: `{ name: value }`".into());
                        self.expr(e, None);
                        None
                    }
                }
            }
        };
        self.ctx = saved;
        (id, node, arg)
    }

    #[inline(never)]
    fn builtin_element(
        &mut self,
        el: &'a ast::Element,
        schema: &'a crate::schema::ElementSchema,
        span: Span,
        list_item: bool,
    ) -> Node {
        let (id, node, arg) = self.element_head(el, schema, span);
        let kind = el.kind.name.as_str();
        self.nodes.push(NodeCtx {
            idx: node,
            kind: kind.to_string(),
            schema: Some(schema),
        });
        if kind == "pages" {
            self.pages_current.push(Ty::Any);
        }
        let mut props = Vec::new();
        if let Some(ast::HeadArg::Named(n, e)) = &el.arg {
            props.push(self.named_head(n, e, span));
        }
        self.push_scope();
        for (n, t) in &schema.scope {
            self.bind_local(n, t.clone(), el.kind.span, LocalKind::ElementScope);
        }
        let (block_props, children) = match &el.block {
            Some(b) => self.tree_items(&b.items, Place::Element { root: list_item }),
            None => (Vec::new(), Vec::new()),
        };
        self.pop_scope();
        if kind == "pages" {
            self.pages_current.pop();
        }
        self.nodes.pop();
        props.extend(block_props);
        self.element_tail(kind, schema, &children);
        Node::Element(hir::Element {
            node,
            kind: ElementKind::Builtin(kind.to_string()),
            span,
            arg,
            id,
            props,
            children,
        })
    }

    #[inline(never)]
    fn element_tail(
        &mut self,
        kind: &str,
        schema: &crate::schema::ElementSchema,
        children: &[Node],
    ) {
        if schema.flags.leaf
            && let Some(child) = children.iter().find_map(rendered_child)
        {
            self.error(
                "check::children_not_allowed",
                format!("`{kind}` takes no children"),
                child,
                "a child",
            )
            .help = Some(format!("wrap them together: `row {{ {kind} …; … }}`"));
        }
    }

    fn unknown_element(&mut self, el: &'a ast::Element, span: Span) -> Option<Node> {
        let kind = el.kind.name.as_str();
        let mut candidates: Vec<String> = self.schema.elements.keys().cloned().collect();
        candidates.extend(
            self.globals
                .iter()
                .filter(|(_, d)| self.defs[d.0 as usize].kind == DefKind::Component)
                .map(|(n, _)| n.clone()),
        );
        candidates.extend(TREE_KEYWORDS.iter().map(|s| s.to_string()));
        candidates.extend(TOP_KEYWORDS.iter().map(|s| s.to_string()));
        let help = Self::did_you_mean(kind, &candidates);
        let label = if TOP_KEYWORDS.contains(&kind) {
            "a declaration keyword, here inside a tree"
        } else {
            "not an element or component"
        };
        self.error(
            "check::unknown_element",
            format!("unknown element `{kind}`"),
            el.kind.span,
            label,
        )
        .help = help;
        // Check what it holds anyway, so the names inside resolve.
        let node = self.new_node();
        if let Some(ast::HeadArg::Positional(e) | ast::HeadArg::Named(_, e)) = &el.arg {
            self.expr(e, None);
        }
        self.nodes.push(NodeCtx {
            idx: node,
            kind: kind.to_string(),
            schema: None,
        });
        let (props, children) = match &el.block {
            Some(b) => self.tree_items(&b.items, Place::Element { root: false }),
            None => (Vec::new(), Vec::new()),
        };
        self.nodes.pop();
        Some(Node::Element(hir::Element {
            node,
            kind: ElementKind::Unknown(kind.to_string()),
            span,
            arg: None,
            id: None,
            props,
            children,
        }))
    }

    /// `pages current: page { … }`: the named head is the first prop.
    fn named_head(&mut self, n: &'a ast::Ident, e: &'a ast::Expr, span: Span) -> hir::Prop {
        self.node_prop(PropRef::head(n, e), span)
    }

    fn component_call(&mut self, d: DefId, el: &'a ast::Element, span: Span) -> Node {
        let kind = el.kind.name.clone();
        self.add_ref(el.kind.span, Target::Def(d));
        let sig = self.comp_sigs.get(&d).cloned().unwrap_or(super::CompSig {
            params: Vec::new(),
            has_slot: false,
        });
        let node = self.new_node();
        let mut filled = vec![false; sig.params.len()];
        let saved = self.ctx;
        self.ctx.prop = true;
        let arg = match &el.arg {
            None => None,
            Some(ast::HeadArg::Positional(e)) => match sig.params.first() {
                Some(p) => {
                    filled[0] = true;
                    let ty = p.ty.clone();
                    Some(self.prop_value(e, &ty, &format!("`{}` of `{kind}`", p.name)))
                }
                None => {
                    self.error(
                        "check::too_many_args",
                        format!("`{kind}` takes no arguments"),
                        e.span,
                        "nothing to fill",
                    );
                    self.expr(e, None);
                    None
                }
            },
            Some(ast::HeadArg::Named(..)) => None,
        };
        self.ctx = saved;
        self.calls.push(CallCtx {
            def: d,
            name: kind.clone(),
            filled,
            has_slot: sig.has_slot,
            reported_children: false,
        });
        let mut props = Vec::new();
        if let Some(ast::HeadArg::Named(n, e)) = &el.arg {
            props.extend(self.call_prop(PropRef::head(n, e), span));
        }
        let (mut block_props, children) = match &el.block {
            Some(b) => self.tree_items(&b.items, Place::Call),
            None => (Vec::new(), Vec::new()),
        };
        props.append(&mut block_props);
        let call = self.calls.pop();
        if let Some(call) = call {
            let missing: Vec<String> = sig
                .params
                .iter()
                .zip(&call.filled)
                .filter(|(p, f)| !**f && !p.has_default)
                .map(|(p, _)| format!("`{}`", p.name))
                .collect();
            if !missing.is_empty() {
                self.error(
                    "check::missing_arg",
                    format!("`{kind}` needs {}", missing.join(", ")),
                    el.kind.span,
                    "missing argument",
                )
                .help = Some(format!(
                    "pass the first one after the name (`{kind} value`), the others in the block (`{{ name: value }}`)"
                ));
            }
        }
        Node::Element(hir::Element {
            node,
            kind: ElementKind::Component(d),
            span,
            arg,
            id: None,
            props,
            children,
        })
    }

    /// A prop in a component call: one of its parameters.
    fn call_prop(&mut self, p: PropRef<'a>, _span: Span) -> Option<hir::Prop> {
        let call = self.calls.last()?.clone();
        let sig = self.comp_sigs.get(&call.def).cloned()?;
        let name = p.name.name.as_str();
        let saved = self.ctx;
        self.ctx.prop = true;
        let out = match sig.params.iter().position(|q| q.name == name) {
            Some(i) => {
                if let Some(c) = self.calls.last_mut() {
                    if c.filled[i] {
                        self.error(
                            "check::duplicate_arg",
                            format!("`{name}` is given twice"),
                            p.name.span,
                            "given again here",
                        );
                    }
                    if let Some(c) = self.calls.last_mut() {
                        c.filled[i] = true;
                    }
                }
                if let Some(tw) = p.two_way {
                    self.error(
                        "check::two_way_prop",
                        "component parameters are not bound two-way",
                        tw,
                        "`<->` here",
                    )
                    .help = Some(
                        "pass the value; let the component's widgets bind their own state".into(),
                    );
                }
                let ty = sig.params[i].ty.clone();
                let value = self.prop_value(p.value, &ty, &format!("`{name}` of `{}`", call.name));
                Some(hir::Prop {
                    name: name.to_string(),
                    span: p.name.span,
                    value,
                    two_way: false,
                    transition: p.transition.map(|t| self.transition(t)),
                    sub: Vec::new(),
                    inherited: false,
                })
            }
            None => {
                let names: Vec<String> = sig.params.iter().map(|q| q.name.clone()).collect();
                let help = Self::did_you_mean(name, &names).or_else(|| {
                    Some(if names.is_empty() {
                        format!("`{}` takes no parameters", call.name)
                    } else {
                        format!(
                            "it takes {}",
                            names
                                .iter()
                                .map(|n| format!("`{n}`"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })
                });
                self.error(
                    "check::unknown_param",
                    format!("`{}` has no parameter `{name}`", call.name),
                    p.name.span,
                    "unknown parameter",
                )
                .help = help;
                self.expr(p.value, None);
                None
            }
        };
        self.ctx = saved;
        out
    }

    // -----------------------------------------------------------------------
    // Props

    /// A prop of the enclosing element.
    fn node_prop(&mut self, p: PropRef<'a>, span: Span) -> hir::Prop {
        let node = self.nodes.last().cloned();
        let name = p.name.name.as_str();
        let ps = match node.as_ref().and_then(|n| n.schema) {
            Some(schema) => match schema.prop(name) {
                Some(ps) => Some(ps.clone()),
                None if schema.flags.uniforms && name.starts_with("u_") => Some(PropSchema {
                    name: name.to_string(),
                    ty: Ty::Any,
                    two_way: false,
                    inherited: false,
                    sub: Vec::new(),
                }),
                None => {
                    let kind = node.as_ref().map_or("", |n| n.kind.as_str()).to_string();
                    let candidates: Vec<String> =
                        schema.props.iter().map(|q| q.name.clone()).collect();
                    let help = Self::did_you_mean(name, &candidates);
                    self.error(
                        "check::unknown_prop",
                        format!("unknown prop `{name}` on `{kind}`"),
                        p.name.span,
                        "not a prop of this element",
                    )
                    .help = help;
                    None
                }
            },
            None => None,
        };
        let Some(ps) = ps else {
            let saved = self.ctx;
            self.ctx.prop = true;
            // A bare variant (`elipsis: end`) cannot be read without the
            // prop's type; the unknown prop is the one error.
            let bare_variant = match &p.value.kind {
                ast::ExprKind::Name(id) => {
                    self.lookup_scope(&id.name).is_none()
                        && self
                            .types
                            .enums
                            .iter()
                            .any(|e| e.variant(&id.name).is_some())
                }
                _ => false,
            };
            let value = if bare_variant {
                hir::Expr::error(p.value.span)
            } else {
                self.expr(p.value, None)
            };
            self.ctx = saved;
            return hir::Prop {
                name: name.to_string(),
                span: p.name.span,
                value,
                two_way: p.two_way.is_some(),
                transition: None,
                sub: Vec::new(),
                inherited: false,
            };
        };
        let prop = self.prop_with(p, &ps, span);
        if let Some(n) = &node
            && n.kind == "pages"
            && name == "current"
            && let Some(last) = self.pages_current.last_mut()
        {
            *last = prop.value.ty.clone();
        }
        prop
    }

    fn prop_with(&mut self, p: PropRef<'a>, ps: &PropSchema, _span: Span) -> hir::Prop {
        let name = p.name.name.as_str();
        let saved = self.ctx;
        self.ctx.prop = true;
        let value = if name == "id" {
            match &p.value.kind {
                ast::ExprKind::Name(n) => match self.lookup_scope(&n.name) {
                    Some(b @ super::Binding::Local(_)) => self.binding_expr(b, n.span),
                    _ => hir::Expr::error(p.value.span),
                },
                _ => {
                    self.error(
                        "check::type_mismatch",
                        "`id` takes a name",
                        p.value.span,
                        "not a name",
                    )
                    .help = Some("write `id: results`, then read `results.hover`".into());
                    hir::Expr::error(p.value.span)
                }
            }
        } else if let Some(tw) = p.two_way {
            if !ps.two_way {
                self.error(
                    "check::two_way_prop",
                    format!("`{name}` cannot be bound two-way"),
                    tw,
                    "the element never writes it",
                )
                .help = Some(
                    "`<->` is for props a widget writes back: `value`, `text`, `open`, `current`; bind this one with `name: value`"
                        .into(),
                );
            }
            let h = self.expr(p.value, Some(&ps.ty));
            if let Err(why) = self.writable(&h) {
                self.not_writable_bind(why, p.value.span);
            } else if !ps.ty.is_lenient()
                && !h.ty.is_lenient()
                && !(self.types.assignable(&h.ty, &ps.ty) && self.types.assignable(&ps.ty, &h.ty))
            {
                let (a, b) = (self.show(&h.ty), self.show(&ps.ty));
                self.error(
                    "check::type_mismatch",
                    format!("`{name}` reads and writes `{b}`, but this is `{a}`"),
                    h.span,
                    format!("this is `{a}`"),
                );
            }
            h
        } else {
            self.prop_value(p.value, &ps.ty, &format!("`{name}`"))
        };
        let transition = p.transition.map(|t| self.transition(t));
        let mut sub = Vec::new();
        if let Some(b) = p.block {
            if ps.sub.is_empty() {
                self.error(
                    "check::misplaced",
                    format!("`{name}` takes no block"),
                    b.span,
                    "no sub-props",
                );
            } else {
                for item in &b.items {
                    match &item.kind {
                        ItemKind::Prop(q) => match ps.sub.iter().find(|s| s.name == q.name.name) {
                            Some(qs) => {
                                sub.push(self.prop_with(PropRef::of(q), &qs.clone(), item.span))
                            }
                            None => {
                                let candidates: Vec<String> =
                                    ps.sub.iter().map(|s| s.name.clone()).collect();
                                let help = Self::did_you_mean(&q.name.name, &candidates);
                                self.error(
                                    "check::unknown_prop",
                                    format!("`{name}` has no `{}`", q.name.name),
                                    q.name.span,
                                    "unknown",
                                )
                                .help = help;
                                self.expr(&q.value, None);
                            }
                        },
                        _ => self.props_only(item.span, name),
                    }
                }
            }
        }
        self.ctx = saved;
        hir::Prop {
            name: name.to_string(),
            span: p.name.span,
            value,
            two_way: p.two_way.is_some(),
            transition,
            sub,
            inherited: ps.inherited,
        }
    }

    fn not_writable_bind(&mut self, why: super::expr::NotWritable, span: Span) {
        let mark = self.diags.len();
        self.not_writable(why, span, "bind");
        if let Some(d) = self.diags.get_mut(mark) {
            d.code = "check::two_way_target";
            let reason = std::mem::replace(
                &mut d.message,
                "`<->` binds to a `state`, a settings field or an `rw` service field".into(),
            );
            if let Some(l) = d.labels.first_mut() {
                l.message = reason.trim_start_matches("cannot bind ").to_string();
            }
        }
    }

    /// `~ $motion.bouncy`, `~ 200ms`, `~ instant`, `~ ease(…)`.
    fn transition(&mut self, t: &'a ast::Expr) -> hir::Expr {
        let instant = self
            .schema
            .types
            .find_enum("Instant")
            .map_or(Ty::Error, Ty::Enum);
        let want = Ty::Union(vec![
            Ty::opaque("Spring"),
            Ty::DURATION,
            Ty::opaque("Easing"),
            instant,
        ]);
        let saved = self.ctx;
        self.ctx.prop = false;
        let h = self.expr(t, Some(&want));
        if !self.types.assignable(&h.ty, &want) {
            let shown = self.show(&h.ty);
            self.error(
                "check::type_mismatch",
                format!("`~` takes a spring, a duration, an easing or `instant`, found `{shown}`"),
                h.span,
                format!("this is `{shown}`"),
            )
            .help = Some(
                "`~ $motion.bouncy`, `~ 200ms`, `~ ease(out_back, 300ms)` or `~ instant`".into(),
            );
        }
        self.ctx = saved;
        h
    }

    /// A prop or token value against its type: comma shorthands and
    /// space-separated shadows and fonts are taken apart here.
    pub(crate) fn prop_value(&mut self, e: &'a ast::Expr, ty: &Ty, what: &str) -> hir::Expr {
        match &e.kind {
            ast::ExprKind::Commas(items) => self.commas(e, items, ty, what),
            ast::ExprKind::Spaced(items) => self.spaced(e, items, ty, what),
            _ => self.expect(e, ty, what),
        }
    }

    fn commas(
        &mut self,
        e: &'a ast::Expr,
        items: &'a [ast::Expr],
        ty: &Ty,
        what: &str,
    ) -> hir::Expr {
        let alts: Vec<&Ty> = match ty {
            Ty::Union(ts) => ts.iter().collect(),
            t => vec![t],
        };
        let shape = alts.iter().find_map(|t| match t {
            Ty::Prim(Prim::Insets | Prim::Corners) => Some(Shape::Sides((*t).clone())),
            Ty::Prim(Prim::Shadow) => Some(Shape::Shadows),
            Ty::Tuple(ts) => Some(Shape::Tuple(ts.clone())),
            Ty::List(elem, _) => Some(Shape::List((**elem).clone())),
            _ => None,
        });
        let mut parts = Vec::new();
        let out_ty = match shape {
            Some(Shape::Sides(t)) => {
                if items.len() > 4 {
                    self.error(
                        "check::too_many_args",
                        format!("{what} takes one to four values"),
                        items[4].span,
                        "a fifth value",
                    )
                    .help = Some("top, right, bottom, left — as in CSS".into());
                }
                for i in items {
                    parts.push(self.expect(i, &Ty::LENGTH, what));
                }
                t
            }
            Some(Shape::Tuple(ts)) => {
                for (k, i) in items.iter().enumerate() {
                    match ts.get(k) {
                        Some(t) => parts.push(self.prop_value(i, t, what)),
                        None => {
                            self.error(
                                "check::too_many_args",
                                format!("{what} takes {} values", ts.len()),
                                i.span,
                                "one too many",
                            );
                            parts.push(self.expr(i, None));
                        }
                    }
                }
                ty.clone()
            }
            Some(Shape::Shadows) => {
                for i in items {
                    parts.push(self.prop_value(i, &Ty::SHADOW, what));
                }
                Ty::SHADOW
            }
            Some(Shape::List(elem)) => {
                for i in items {
                    parts.push(self.prop_value(i, &elem, what));
                }
                Ty::list(elem)
            }
            None => {
                let shown = self.show(ty);
                self.error(
                    "check::type_mismatch",
                    format!("{what} takes one value, not a comma list"),
                    e.span,
                    format!("expects `{shown}`"),
                );
                for i in items {
                    parts.push(self.expr(i, None));
                }
                Ty::Error
            }
        };
        hir::Expr {
            kind: hir::ExprKind::Commas(parts),
            ty: out_ty,
            span: e.span,
        }
    }

    fn spaced(
        &mut self,
        e: &'a ast::Expr,
        items: &'a [ast::Expr],
        ty: &Ty,
        what: &str,
    ) -> hir::Expr {
        let accepts = |p: Prim| match ty {
            Ty::Prim(q) => *q == p,
            Ty::Union(ts) => ts.contains(&Ty::Prim(p)),
            Ty::Any => true,
            _ => false,
        };
        let mut parts = Vec::new();
        let out = if accepts(Prim::Shadow)
            && !matches!(
                items.first().map(|i| &i.kind),
                Some(ast::ExprKind::String(_))
            ) {
            let mut lengths = 0;
            for i in items {
                let h = self.expr(i, None);
                if self.types.assignable(&h.ty, &Ty::LENGTH) {
                    lengths += 1;
                } else if !self.types.assignable(&h.ty, &Ty::COLOR) {
                    let shown = self.show(&h.ty);
                    self.error(
                        "check::type_mismatch",
                        format!("a shadow is lengths and a colour, found `{shown}`"),
                        h.span,
                        format!("this is `{shown}`"),
                    );
                }
                parts.push(h);
            }
            if !(2..=4).contains(&lengths) {
                self.error(
                    "check::type_mismatch",
                    "a shadow takes x and y offsets, then blur and spread",
                    e.span,
                    format!("{lengths} length{}", if lengths == 1 { "" } else { "s" }),
                )
                .help = Some("`0 2px 8px $shadow.alpha(0.25)`".into());
            }
            Ty::SHADOW
        } else if accepts(Prim::Font) {
            for (k, i) in items.iter().enumerate() {
                let want = match k {
                    0 => Ty::TEXT,
                    1 => Ty::LENGTH,
                    _ => Ty::INT,
                };
                let label = match k {
                    0 => "the font family",
                    1 => "the font size",
                    _ => "the font weight",
                };
                parts.push(self.expect(i, &want, label));
                if k > 2 {
                    self.error(
                        "check::too_many_args",
                        "a font is family, size and weight",
                        i.span,
                        "one too many",
                    );
                }
            }
            Ty::FONT
        } else {
            let written: Option<Vec<String>> = items.iter().map(simple_text).collect();
            let help = match written {
                Some(w) => format!("separate the values with commas: `{}`", w.join(", ")),
                None => "separate the values with commas: `8, 8, 0`".to_string(),
            };
            self.error(
                "check::spaced_value",
                format!("{what} takes comma-separated values"),
                e.span,
                "space-separated values are for shadows and fonts",
            )
            .help = Some(help);
            for i in items {
                parts.push(self.expr(i, None));
            }
            Ty::Error
        };
        hir::Expr {
            kind: hir::ExprKind::Spaced(parts),
            ty: out,
            span: e.span,
        }
    }

    // -----------------------------------------------------------------------
    // State, let, fn

    pub(super) fn state_decl(
        &mut self,
        id: DefId,
        s: &'a ast::State,
        reset: bool,
    ) -> hir::StateDecl {
        match &s.init {
            ast::StateInit::Value {
                ty,
                key,
                value,
                persist,
            } => {
                let declared = ty.as_ref().map(|t| self.resolve_type(t));
                let h = match &declared {
                    Some(t) => self.expect(value, t, "the default"),
                    None => self.expr(value, None),
                };
                if declared.is_none() {
                    self.inferable(&h.ty, &s.name, value.span, "state");
                }
                let mut final_ty = declared.unwrap_or_else(|| h.ty.clone());
                let key_path = key.as_ref().and_then(|k| self.state_key(&mut final_ty, k));
                if let Some(path) = &key_path {
                    self.state_keys.insert(id, path.clone());
                }
                if let Some(p) = persist
                    && !final_ty.is_data()
                {
                    let shown = self.show(&final_ty);
                    self.error(
                        "check::persist",
                        format!("`{}` cannot persist a `{shown}`", s.name.name),
                        *p,
                        "not plain data",
                    )
                    .help = Some(
                        "only plain data (numbers, text, colours, enums, records, lists) is stored"
                            .into(),
                    );
                }
                self.defs[id.0 as usize].ty = final_ty;
                hir::StateDecl {
                    def: id,
                    init: hir::StateInit::Value {
                        key: key_path,
                        value: h,
                    },
                    persist: persist.is_some(),
                    reset,
                }
            }
            ast::StateInit::File { path, fields } => {
                let file = self.file();
                let rid = self.types.add_record(RecordDef::new(
                    s.name.name.clone(),
                    Origin::User(file, s.name.span),
                ));
                let mut defs: Vec<FieldDef> = Vec::new();
                let mut out = Vec::new();
                let saved = self.ctx;
                self.ctx.prop = false;
                for f in &fields.items {
                    if defs.iter().any(|d| d.name == f.name.name) {
                        self.error(
                            "check::redeclared",
                            format!("setting `{}` is declared twice", f.name.name),
                            f.name.span,
                            "declared again here",
                        );
                        continue;
                    }
                    let ty = self.resolve_type(&f.ty);
                    let default = match &f.default {
                        Some(d) => Some(self.expect(d, &ty, &format!("`{}`", f.name.name))),
                        None => {
                            self.error(
                                "check::needs_default",
                                format!("setting `{}` needs a default", f.name.name),
                                f.name.span,
                                "no default",
                            )
                            .help = Some(format!(
                                "a deleted or bad key springs back to its default: `{}: … = value`",
                                f.name.name
                            ));
                            None
                        }
                    };
                    if let Some(rw) = f.rw {
                        self.error(
                            "check::misplaced",
                            "settings fields are always writable",
                            rw,
                            "`rw` is for service fields",
                        );
                    }
                    defs.push(FieldDef {
                        name: f.name.name.clone(),
                        ty: ty.clone(),
                        rw: true,
                    });
                    out.push(hir::SettingsField {
                        name: f.name.name.clone(),
                        span: f.name.span,
                        ty,
                        default,
                    });
                }
                self.ctx = saved;
                self.types.record_mut(rid).fields = defs;
                self.defs[id.0 as usize].ty = Ty::Record(rid);
                hir::StateDecl {
                    def: id,
                    init: hir::StateInit::File {
                        path: path.value.clone(),
                        fields: out,
                    },
                    persist: false,
                    reset,
                }
            }
        }
    }

    /// `key app` of a keyed state collection: a field path of its items.
    fn state_key(&mut self, ty: &mut Ty, k: &ast::Expr) -> Option<Vec<String>> {
        fn path(e: &ast::Expr) -> Option<Vec<String>> {
            match &e.kind {
                ast::ExprKind::Name(n) => Some(vec![n.name.clone()]),
                ast::ExprKind::Field {
                    base,
                    name,
                    optional: false,
                } => {
                    let mut p = path(base)?;
                    p.push(name.name.clone());
                    Some(p)
                }
                _ => None,
            }
        }
        let Some(p) = path(k) else {
            self.error(
                "check::missing_key",
                "a collection's `key` names a field of its items",
                k.span,
                "not a field name",
            )
            .help = Some("`state pins: [Pin] key app = []`".into());
            return None;
        };
        let Ty::List(elem, _) = ty.clone() else {
            if !ty.is_lenient() {
                let shown = self.show(ty);
                self.error(
                    "check::missing_key",
                    format!("`key` is for lists, and this state is `{shown}`"),
                    k.span,
                    "not a list",
                );
            }
            return None;
        };
        match &*elem {
            Ty::Record(r) => match self.types.field_path(*r, &p) {
                Some(_) => {
                    *ty = Ty::List(elem.clone(), true);
                    Some(p)
                }
                None => {
                    let rec = self.types.record(*r);
                    let candidates: Vec<String> =
                        rec.fields.iter().map(|f| f.name.clone()).collect();
                    let help = Self::did_you_mean(&p.join("."), &candidates);
                    let rn = rec.name.clone();
                    self.error(
                        "check::unknown_field",
                        format!("`{rn}` has no field `{}`", p.join(".")),
                        k.span,
                        "not a field of the items",
                    )
                    .help = help;
                    None
                }
            },
            t if t.is_lenient() => None,
            t => {
                let shown = self.show(t);
                self.error(
                    "check::missing_key",
                    format!("`key` names a field, and `{shown}` items have none"),
                    k.span,
                    "not a record",
                );
                None
            }
        }
    }

    /// `state xs = []` and `let x = null` need a type.
    fn inferable(&mut self, ty: &Ty, name: &ast::Ident, span: Span, what: &str) {
        let problem = match ty {
            Ty::Null => Some(format!("`{what} {}: T? = null`", name.name)),
            Ty::List(e, _) if e.is_error() => Some(format!("`{what} {}: [T] = []`", name.name)),
            _ => None,
        };
        if let Some(help) = problem {
            self.error(
                "check::needs_type",
                format!("cannot tell the type of `{}`", name.name),
                span,
                "nothing to infer from",
            )
            .help = Some(format!("write the type: {help}"));
        }
    }

    pub(super) fn let_decl(&mut self, id: DefId, l: &'a ast::Let, reset: bool) -> hir::LetDecl {
        let declared = l.ty.as_ref().map(|t| self.resolve_type(t));
        let value = match &declared {
            Some(t) => self.expect(&l.value, t, &format!("`{}`", l.name.name)),
            None => self.expr(&l.value, None),
        };
        if declared.is_none() {
            self.inferable(&value.ty, &l.name, l.value.span, "let");
        }
        let ty = declared.unwrap_or_else(|| value.ty.clone());
        self.defs[id.0 as usize].ty = ty;
        hir::LetDecl {
            def: id,
            value,
            reset,
        }
    }

    pub(super) fn fn_decl(&mut self, id: DefId, f: &'a ast::FnDecl) -> hir::FnDecl {
        let (params, ret): (Vec<ParamSig>, Option<Ty>) = match &self.defs[id.0 as usize].ty {
            Ty::Fn(sig) => (sig.params.clone(), Some(sig.ret.clone())),
            _ => (self.fn_params.get(&id).cloned().unwrap_or_default(), None),
        };
        let saved = self.ctx;
        self.ctx = super::Ctx {
            pure_fn: true,
            ..super::Ctx::default()
        };
        self.push_scope();
        let locals: Vec<_> = f
            .params
            .iter()
            .zip(&params)
            .map(|(p, s)| {
                let l = self.bind_local(&p.name.name, s.ty.clone(), p.name.span, LocalKind::Param);
                self.add_ref(p.name.span, Target::Local(l));
                l
            })
            .collect();
        let body = self.stmts(&f.body.items);
        let value_ty = match body.last().map(|s| &s.kind) {
            Some(hir::StmtKind::Expr(e)) => Some((e.ty.clone(), e.clone())),
            _ => None,
        };
        match (&ret, &value_ty) {
            (_, None) => {
                self.error(
                    "check::fn_value",
                    format!("`{}` has no value", f.name.name),
                    f.body.span,
                    "the last statement is not an expression",
                )
                .help = Some(
                    "a fn's value is its last expression: `fn f(x: float) -> float { x * 2 }`"
                        .into(),
                );
            }
            (Some(r), Some((_, e))) => {
                self.require(e, r, "the function's value");
            }
            (None, Some(_)) => {}
        }
        self.pop_scope();
        self.ctx = saved;
        if ret.is_none() {
            let r = value_ty.map_or(Ty::Error, |(t, _)| t);
            self.defs[id.0 as usize].ty = Ty::Fn(Arc::new(FnSig::new(params, r)));
        }
        hir::FnDecl {
            def: id,
            params: locals,
            body,
        }
    }

    // -----------------------------------------------------------------------
    // Keyframes and services

    pub(super) fn keyframes(&mut self, id: DefId, k: &'a ast::Keyframes) -> hir::Keyframes {
        let node_group = self.schema.groups.get("node");
        let node = self.new_node();
        self.nodes.push(NodeCtx {
            idx: node,
            kind: "keyframes".into(),
            schema: node_group,
        });
        let mut stops = Vec::new();
        let mut props = Vec::new();
        let settings = keyframe_settings(self.schema);
        for item in &k.body.items {
            match &item.kind {
                ItemKind::KeyframeStop(s) => {
                    let at = s
                        .stops
                        .iter()
                        .filter_map(|e| match &e.kind {
                            ast::ExprKind::Number(n) => {
                                if n.unit != Some(ast::Unit::Percent)
                                    || !(0.0..=100.0).contains(&n.value)
                                {
                                    self.error(
                                        "check::type_mismatch",
                                        "a keyframe stop is a percentage from 0% to 100%",
                                        e.span,
                                        "not a stop",
                                    );
                                }
                                Some(n.value)
                            }
                            _ => None,
                        })
                        .collect();
                    let (ps, _) = self.tree_items(&s.body.items, Place::Props);
                    stops.push(hir::KeyframeStop { at, props: ps });
                }
                ItemKind::Prop(p) => match settings.iter().find(|s| s.name == p.name.name) {
                    Some(s) => props.push(self.prop_with(PropRef::of(p), &s.clone(), item.span)),
                    None => {
                        let candidates: Vec<String> =
                            settings.iter().map(|s| s.name.clone()).collect();
                        let help = Self::did_you_mean(&p.name.name, &candidates);
                        self.error(
                            "check::unknown_prop",
                            format!("keyframes have no setting `{}`", p.name.name),
                            p.name.span,
                            "unknown",
                        )
                        .help = help.or_else(|| Some("keyframes take `duration`, `delay`, `repeat` and `easing`; props go in stops: `50% { x: 4 }`".into()));
                    }
                },
                _ => {}
            }
        }
        self.nodes.pop();
        hir::Keyframes {
            def: id,
            stops,
            props,
        }
    }

    pub(super) fn service_decl(&mut self, id: DefId, s: &'a ast::Service) -> hir::ServiceDecl {
        let kind = s.source.kind.name.as_str();
        let mut args = Vec::new();
        match kind {
            "dbus" => {
                let bus = s.source.args.first();
                match bus.map(|b| &b.kind) {
                    Some(ast::ExprKind::Name(n)) if n.name == "system" || n.name == "session" => {}
                    _ => {
                        let span = bus.map_or(s.source.span, |b| b.span);
                        self.error(
                            "check::type_mismatch",
                            "a D-Bus service names its bus first: `system` or `session`",
                            span,
                            "expected `system` or `session`",
                        );
                    }
                }
                for (i, a) in s.source.args.iter().enumerate().skip(1) {
                    if i > 2 {
                        self.error(
                            "check::too_many_args",
                            "a D-Bus service takes a bus, a name and an optional object path",
                            a.span,
                            "one too many",
                        );
                    }
                    args.push(self.expect(a, &Ty::TEXT, "the bus name"));
                }
                if s.source.args.len() < 2 {
                    self.error(
                        "check::missing_arg",
                        "a D-Bus service needs its bus name",
                        s.source.span,
                        "no bus name",
                    )
                    .help = Some("`from dbus system \"net.hadess.PowerProfiles\"`".into());
                }
            }
            "file" | "listen" | "poll" => {
                for (i, a) in s.source.args.iter().enumerate() {
                    if i > 0 {
                        self.error(
                            "check::too_many_args",
                            format!("`{kind}` takes one value"),
                            a.span,
                            "one too many",
                        );
                    }
                    let want = if kind == "file" {
                        Ty::PATH
                    } else {
                        Ty::Union(vec![Ty::TEXT, Ty::list(Ty::TEXT)])
                    };
                    args.push(self.expect(a, &want, &format!("`{kind}`")));
                }
                if kind != "file" {
                    let program = s.source.args.first().and_then(first_program);
                    let in_block = s.body.items.iter().any(
                        |i| matches!(&i.kind, ItemKind::Permit(p) if p.capability.name == "exec"),
                    );
                    let allowed = in_block
                        || self.permits.iter().any(|p| match (p, program) {
                            (None, _) => true,
                            (Some(list), Some(prog)) => list.iter().any(|x| x == prog),
                            (Some(_), None) => false,
                        });
                    if !allowed {
                        let prog = program.unwrap_or("the command");
                        self.error(
                            "check::no_permit",
                            format!("running `{prog}` needs `permit exec`"),
                            s.source.span,
                            "not permitted",
                        )
                        .help = Some(format!("add `permit exec \"{prog}\"` at the top level"));
                    }
                }
            }
            _ => {}
        }
        let every = match (&s.source.every, kind) {
            (Some(e), "poll") => Some(self.expect(e, &Ty::DURATION, "`every`")),
            (Some(e), _) => {
                self.error(
                    "check::misplaced",
                    "`every` is for `poll` services",
                    e.span,
                    "not polled",
                );
                None
            }
            (None, "poll") => {
                self.error(
                    "check::missing_arg",
                    "a `poll` service needs `every`",
                    s.source.span,
                    "how often?",
                )
                .help = Some("`from poll [\"sensors\", \"-j\"] every 5s`".into());
                None
            }
            (None, _) => None,
        };
        let mut fields = Vec::new();
        let rid = match self.defs[id.0 as usize].kind {
            DefKind::Service(r) => Some(r),
            _ => None,
        };
        for item in &s.body.items {
            let ItemKind::Field(f) = &item.kind else {
                continue;
            };
            let ty = rid
                .and_then(|r| {
                    self.types
                        .record(r)
                        .field(&f.name.name)
                        .map(|d| d.ty.clone())
                })
                .unwrap_or(Ty::Error);
            let source_key = match f.default.as_ref().map(|d| &d.kind) {
                None => None,
                Some(ast::ExprKind::String(s)) => Some(s.value.clone()),
                Some(_) => f.default.as_ref().and_then(path_text),
            };
            if let Some(d) = &f.default
                && source_key.is_none()
            {
                self.error(
                    "check::type_mismatch",
                    "a service field's `= …` names where it reads from",
                    d.span,
                    "not a property or key name",
                )
                .help = Some("`profile: text rw = ActiveProfile`".into());
            }
            fields.push(hir::ServiceField {
                name: f.name.name.clone(),
                span: f.name.span,
                ty,
                rw: f.rw.is_some(),
                source_key,
            });
        }
        hir::ServiceDecl {
            def: id,
            source: kind.to_string(),
            args,
            every,
            fields,
        }
    }
}

enum Shape {
    Sides(Ty),
    Tuple(Vec<Ty>),
    Shadows,
    List(Ty),
}

/// A child that renders inside its parent, for "takes no children"
/// (a `popup` or `tooltip` is its own surface, so a leaf may hold one).
fn rendered_child(n: &Node) -> Option<Span> {
    match n {
        Node::Element(e) => match &e.kind {
            ElementKind::Builtin(k) if k == "popup" || k == "tooltip" => None,
            _ => Some(e.span),
        },
        Node::If(i) => Some(i.span),
        Node::For(f) => Some(f.span),
        Node::Match(m) => Some(m.span),
        Node::Slot(s) => Some(*s),
        _ => None,
    }
}

/// `8`, `$space.2`, `x` as written, for a "use commas" fix.
fn simple_text(e: &ast::Expr) -> Option<String> {
    match &e.kind {
        ast::ExprKind::Number(n) => Some(format!(
            "{}{}",
            n.value,
            n.unit.map_or("", ast::Unit::as_str)
        )),
        ast::ExprKind::Unary {
            op: ast::UnaryOp::Neg,
            expr,
        } => simple_text(expr).map(|s| format!("-{s}")),
        _ => path_text(e),
    }
}

/// What a `keyframes` block may set besides its stops.
fn keyframe_settings(schema: &crate::schema::Schema) -> Vec<PropSchema> {
    let curve = schema.types.find_enum("Curve").map_or(Ty::Error, Ty::Enum);
    [
        ("duration", Ty::DURATION),
        ("delay", Ty::DURATION),
        ("repeat", Ty::INT),
        ("alternate", Ty::BOOL),
        ("easing", Ty::Union(vec![Ty::opaque("Easing"), curve])),
    ]
    .into_iter()
    .map(|(n, t)| PropSchema {
        name: n.to_string(),
        ty: t,
        two_way: false,
        inherited: false,
        sub: Vec::new(),
    })
    .collect()
}
