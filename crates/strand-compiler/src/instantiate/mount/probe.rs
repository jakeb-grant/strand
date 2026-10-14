//! (M6) The `drag:` values a windowed `list`'s row would hold if it
//! were mounted, for a drop's `at` (`Instance::drop_at`, spec.md 6.7):
//! the row's tree is walked for its item as mounting walks it, every
//! `drag:` source of it evaluated in a scope of its own that is
//! disposed afterwards. Nothing reaches the scene, the registry or the
//! services: a `let` is a plain memo, a `state` reads the cell the row
//! keeps while it is scrolled out (`keep_row_cells`) or else its initial
//! value, and a `when` on a laid-out size (`self.width`) reads it as a
//! row not laid out yet does (0).

use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;

use strand_core::Runtime;
use strand_scene::{NodeKind, Prop as SceneProp};

use super::{Ctx, collect_decls};
use crate::hir::{LocalId, NodeIdx};
use crate::lower::{Element, ElementKind, ForKey, Node, StateInit};
use crate::vm::Env;
use crate::vm::value::{Slot, Value, ValueKey};

/// What a windowed list's rows are made from, kept with its window so a
/// row that is not mounted can be probed.
pub(crate) struct RowProbe {
    /// The scope the `for` sits in (each row's scope is a child of it).
    pub(super) env: Rc<Env>,
    /// The `for`'s item binding.
    pub(super) binding: Option<LocalId>,
    /// The elements each row owns.
    pub(super) owned: Arc<BTreeSet<NodeIdx>>,
    /// The `for`'s body: one row each.
    pub(super) nodes: Arc<Vec<Node>>,
    /// Where the rows sit in the instance tree (`…/f12`), so a row's
    /// `state` is found under the key its mounted self kept it by.
    pub(super) prefix: Rc<str>,
}

impl Ctx {
    /// The `drag:` values of the row of `item` (keyed `key`) as render
    /// would read them off it mounted (`row_drags` in render's
    /// `drop_index`): every `drag:` source in the row, except in the
    /// rows of a `list` or `for` inside it. A source whose value fails
    /// holds none, as mounted.
    pub(super) fn probe_row(
        self: &Rc<Self>,
        rt: &Runtime,
        p: &RowProbe,
        key: &ValueKey,
        item: &Value,
    ) -> Vec<Value> {
        let (scope, out) = rt.scope(|rt| {
            rt.untrack(|rt| {
                let env = Env::child(&p.env, p.owned.clone(), None, None);
                let shown = key.0.show(&self.vm.prog.types);
                env.set_ident(format!("{}[{shown}]", p.prefix));
                env.set_instance(shown);
                env.set_owner(rt.current_owner());
                if let Some(b) = p.binding {
                    env.bind_local(b, Slot::Signal(rt.signal(item.clone())));
                }
                let mut out = Vec::new();
                self.probe_body(rt, &p.nodes, &env, false, 0, &mut out);
                out
            })
        });
        scope.dispose(rt);
        out
    }

    /// Declares `nodes`' `let`s and `state`s in `env` and probes them.
    fn probe_body(
        self: &Rc<Self>,
        rt: &Runtime,
        nodes: &[Node],
        env: &Rc<Env>,
        in_rows: bool,
        depth: u32,
        out: &mut Vec<Value>,
    ) {
        // Trees nest deep: grow the stack as mounting does.
        stacker::maybe_grow(64 * 1024, 1024 * 1024, || {
            self.probe_declare(rt, nodes, env);
            self.probe_nodes(rt, nodes, env, in_rows, depth, out);
        });
    }

    /// [`Ctx::declare`] without its side effects: each `let` a memo, each
    /// plain `state` the cell kept under its key or its initial value.
    fn probe_declare(self: &Rc<Self>, rt: &Runtime, nodes: &[Node], env: &Rc<Env>) {
        use super::super::reload::CellRec;
        let mut lets = Vec::new();
        let mut states = Vec::new();
        collect_decls(nodes, &mut lets, &mut states);
        for (def, value) in lets {
            let (vm, e) = (self.vm.clone(), env.clone());
            let m = rt.memo(move |rt| vm.eval(rt, value, &e));
            env.bind_def(def, Slot::Memo(m));
        }
        let prog = self.vm.prog.clone();
        for s in states {
            let info = prog.def(s.def);
            let owner = match info.owner {
                Some(o) => prog.owner_key(o),
                None => info.module.clone(),
            };
            let surface_owned = info
                .owner
                .is_some_and(|o| matches!(prog.def(o).kind, crate::hir::DefKind::Surface(_)));
            let key = match surface_owned {
                true => format!("{}#.{}", env.ident(), info.name),
                false => format!("{}#{owner}.{}", env.ident(), info.name),
            };
            let kept = match self.registry.borrow().cells.get(key.as_str()) {
                Some(CellRec::Plain { sig, .. }) => Some(Slot::Signal(*sig)),
                Some(CellRec::Keyed { keyed, list, .. }) => Some(Slot::Keyed(*keyed, *list)),
                _ => None,
            };
            let slot = match (kept, &s.init) {
                (Some(slot), _) => slot,
                (None, StateInit::Value(c)) => {
                    let v = self.eval(rt, *c, env).unwrap_or(Value::Null);
                    Slot::Signal(rt.signal(v))
                }
                // A settings file in a row: not read for a probe.
                (None, _) => continue,
            };
            env.bind_def(s.def, slot);
        }
    }

    /// Probes each of `nodes`. `in_rows`: they are the content of an
    /// element holding a `for`'s rows, whose own `for`s are not this
    /// row's (render's `Prop::DropRows` boundary).
    fn probe_nodes(
        self: &Rc<Self>,
        rt: &Runtime,
        nodes: &[Node],
        env: &Rc<Env>,
        in_rows: bool,
        depth: u32,
        out: &mut Vec<Value>,
    ) {
        if depth >= super::super::MAX_MOUNT_DEPTH {
            return;
        }
        for n in nodes {
            match n {
                Node::Element(e) => self.probe_element(rt, e, env, depth, out),
                Node::If {
                    cond, then, else_, ..
                } => {
                    let Ok(v) = self.eval(rt, *cond, env) else {
                        continue;
                    };
                    let (which, nodes) = if v.truthy() { (0, then) } else { (1, else_) };
                    let benv = self.probe_branch(env, n, which);
                    self.probe_body(rt, nodes, &benv, in_rows, depth + 1, out);
                }
                Node::Match { selector, arms, .. } => {
                    let Ok(v) = self.eval(rt, *selector, env) else {
                        continue;
                    };
                    let Some(i) = v.as_f64().map(|i| i as usize) else {
                        continue;
                    };
                    if let Some(nodes) = arms.get(i) {
                        let benv = self.probe_branch(env, n, i);
                        self.probe_body(rt, nodes, &benv, in_rows, depth + 1, out);
                    }
                }
                // Another list's rows inside this row: not its own.
                Node::For(_) if in_rows => {}
                Node::For(f) => {
                    let Ok(list) = self.eval(rt, f.iter, env) else {
                        continue;
                    };
                    let types = &self.vm.prog.types;
                    let prefix = format!("{}/f{}", env.ident(), self.sid(f.file, f.span));
                    for item in list.as_list().unwrap_or(&[]) {
                        let key = match &f.key {
                            ForKey::Path(p) => item.key_path(types, p).cloned(),
                            ForKey::Value => Some(item.clone()),
                            ForKey::Expr(c) => self
                                .vm
                                .eval_with(rt, *c, env, vec![(f.binding, item.clone())])
                                .ok(),
                        };
                        let ienv = Env::child(env, f.body.owned.clone(), None, None);
                        let shown = key.unwrap_or(Value::Null).show(types);
                        ienv.set_ident(format!("{prefix}[{shown}]"));
                        ienv.set_instance(shown);
                        ienv.bind_local(f.binding, Slot::Signal(rt.signal(item.clone())));
                        self.probe_body(rt, &f.body.nodes, &ienv, false, depth + 1, out);
                    }
                }
                Node::Slot => {
                    let mut cur = Some(env.clone());
                    while let Some(e) = cur {
                        if let Some((children, caller)) = &e.slot {
                            let caller = Env::child(caller, Arc::default(), None, None);
                            caller.set_ident(format!("{}/slot", e.ident()));
                            self.probe_body(rt, children, &caller, in_rows, depth + 1, out);
                            break;
                        }
                        cur = e.parent.clone();
                    }
                }
                Node::Surface(_)
                | Node::State(_)
                | Node::Let { .. }
                | Node::Handler(_)
                | Node::Timer(_)
                | Node::When { .. }
                | Node::Pose { .. }
                | Node::Set(_)
                | Node::Play(_) => {}
            }
        }
    }

    /// The scope of an `if` or `match` branch, as `mount_branches` makes it.
    fn probe_branch(&self, env: &Rc<Env>, n: &Node, which: usize) -> Rc<Env> {
        let (two, file, span) = match n {
            Node::If { file, span, .. } => (true, *file, *span),
            Node::Match { file, span, .. } => (false, *file, *span),
            _ => return env.clone(),
        };
        let tag = format!("{}{}", if two { "i" } else { "m" }, self.sid(file, span));
        let benv = Env::child(env, Arc::default(), None, None);
        benv.set_ident(format!("{}/{tag}.{which}", env.ident()));
        benv
    }

    /// An element's `drag:` value (its last `when` that holds, else its
    /// own), then its content's.
    fn probe_element(
        self: &Rc<Self>,
        rt: &Runtime,
        e: &Element,
        env: &Rc<Env>,
        depth: u32,
        out: &mut Vec<Value>,
    ) {
        let kind = match &e.kind {
            // Mounted only while shown, and a surface of its own.
            ElementKind::Builtin(NodeKind::Page | NodeKind::Tooltip) => return,
            ElementKind::Builtin(k) if k.is_surface() => return,
            ElementKind::Builtin(k) => *k,
            ElementKind::Component(d) => return self.probe_component(rt, *d, e, env, depth, out),
            ElementKind::Unknown(_) | ElementKind::File(_) => return,
        };
        for &l in &e.scope {
            if env.local(l).is_none() {
                let name = self.vm.prog.local(l).name.as_str();
                let v = crate::vm::builtins::scope_value(kind, name).unwrap_or(Value::int(0));
                env.bind_local(l, Slot::Signal(rt.signal(v)));
            }
        }
        let is_drag = |p: &crate::lower::Prop| p.prop == Some(SceneProp::Drag);
        let base = e.arg.iter().chain(e.props.iter()).rfind(|p| is_drag(p));
        let mut chosen = base.map(|p| p.value);
        for c in e.children.iter() {
            if let Node::When { cond, props } = c
                && let Some(p) = props.iter().rfind(|p| is_drag(p))
            {
                let (v, _) = crate::vm::builtins::layout_query(0.0, || self.eval(rt, *cond, env));
                if v.is_ok_and(|v| v.truthy()) {
                    chosen = Some(p.value);
                }
            }
        }
        if let Some(c) = chosen
            && let Ok(v) = self.eval(rt, c, env)
        {
            out.push(v);
        }
        // Render's boundary: a `list` holds rows of its own (and only
        // its content beside a `for` is its own), as does an element
        // whose direct child is a `for`.
        let holds_rows = e.children.iter().any(|n| matches!(n, Node::For(_)));
        if kind == NodeKind::List && !holds_rows {
            return;
        }
        self.probe_nodes(rt, &e.children, env, holds_rows, depth + 1, out);
    }

    /// A component's body in a scope of its own, its parameters bound as
    /// `mount_component` binds them.
    fn probe_component(
        self: &Rc<Self>,
        rt: &Runtime,
        def: crate::hir::DefId,
        call: &Element,
        caller: &Rc<Env>,
        depth: u32,
        out: &mut Vec<Value>,
    ) {
        let Some(comp) = self.vm.prog.components.get(&def).cloned() else {
            return;
        };
        let env = Env::child(
            &self.vm.root,
            comp.body.owned.clone(),
            Some((call.children.clone(), caller.clone())),
            Some(def),
        );
        env.set_ident(format!(
            "{}/c{}",
            caller.ident(),
            self.sid(call.file, call.span)
        ));
        env.set_owner(rt.current_owner());
        for (local, default) in &comp.params {
            let name = &self.vm.prog.local(*local).name;
            let arg = call
                .arg
                .iter()
                .chain(call.props.iter())
                .find(|p| &p.name == name)
                .map(|p| p.value);
            let vm = self.vm.clone();
            let m = match (arg, default) {
                (Some(c), _) => {
                    let e = caller.clone();
                    rt.memo(move |rt| vm.eval(rt, c, &e))
                }
                (None, Some(d)) => {
                    let (e, d) = (env.clone(), *d);
                    rt.memo(move |rt| vm.eval(rt, d, &e))
                }
                (None, None) => rt.memo(|_| Ok(Value::Null)),
            };
            env.bind_local(*local, Slot::Memo(m));
        }
        self.probe_body(rt, &comp.body.nodes, &env, false, depth + 1, out);
    }
}
