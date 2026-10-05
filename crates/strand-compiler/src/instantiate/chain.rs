//! `for x in xs.filter(…).take(5)` on a keyed collection: core's
//! incremental views.
//!
//! design.md: `.filter`, `.map`, `.take` and `.sort_by` "update
//! incrementally and keep keys". When a `for` iterates such a chain
//! rooted at a keyed `state` or a service's keyed field, each step is
//! core's operator ([`strand_core::KeyedOps`]) fed by the source's diffs:
//! one pushed or updated row is one diff through the chain, and the loop
//! follows the result's diffs like a bare keyed collection. A step's
//! lambda runs per item through the VM; what the lambda reads besides
//! the item (a `state`, a service field, a component parameter) is the
//! step's tracked parameters, and a change there rebuilds the step.
//! Anything else (a plain list, a lambda calling a service method) is
//! compared as a whole list ([`Runtime::keyed_memo`]).

use std::cell::RefCell;
use std::rc::Rc;

use strand_core::{Error, KeyedMemo, KeyedOps, KeyedSource, Runtime};

use super::Ctx;
use crate::lower::{Chain, ChainOp, ChainRoot, ChunkId, For, ForKey, Program};
use crate::ty::Ty;
use crate::vm::Env;
use crate::vm::value::{Slot, Value, ValueKey};

/// A step's tracked parameters: its lambda (compared by code and
/// captured values, not by the closure's identity) and the values of
/// what the lambda reads besides its item.
#[derive(Clone)]
struct StepParams {
    f: Value,
    code: Option<(u32, Vec<(crate::hir::LocalId, Value)>)>,
    touched: Vec<Value>,
}

impl PartialEq for StepParams {
    fn eq(&self, other: &Self) -> bool {
        let same_f = match (&self.code, &other.code) {
            (Some(a), Some(b)) => a == b,
            (None, None) => self.f == other.f,
            _ => false,
        };
        same_f && self.touched == other.touched
    }
}

type Derived = KeyedMemo<ValueKey, Value>;

impl Ctx {
    /// The incremental view a `for` over a keyed chain follows, if it is
    /// one this can build (see the module docs).
    pub(crate) fn keyed_chain(
        self: &Rc<Self>,
        rt: &Runtime,
        f: &For,
        env: &Rc<Env>,
    ) -> Option<Derived> {
        let chain = f.chain.as_ref()?;
        let prog = self.vm.prog.clone();
        // The loop's items are the collection's, by its keys: the `for`
        // must not ask for other keys (`map` keeps the source keys, so a
        // mapped loop takes them as they are). Through view `let`s, the
        // collection is the one at the bottom of the chain.
        let (base, maps) = base_of(&prog, chain);
        let own = match &base {
            ChainRoot::Def(d) => prog.state_keys.get(d).cloned().or_else(|| {
                match prog
                    .def(*d)
                    .ty
                    .list_elem()
                    .map(|(t, _)| t.non_null().clone())
                {
                    Some(Ty::Record(r)) => prog.types.record(r).key.clone(),
                    _ => None,
                }
            }),
            ChainRoot::Service(..) => None,
        };
        let same = match (&f.key, &base) {
            (ForKey::Expr(_), _) => false,
            _ if maps => true,
            (ForKey::Path(p), ChainRoot::Def(_)) => own.as_ref() == Some(p),
            (ForKey::Value, ChainRoot::Def(_)) => own.is_none(),
            (ForKey::Path(_), ChainRoot::Service(..)) => true,
            (ForKey::Value, ChainRoot::Service(..)) => false,
        };
        if !same {
            return None;
        }
        let what: Rc<str> = format!(
            "for {} in … in {}",
            prog.local(f.binding).name,
            self.module_of(f.file)
        )
        .into();
        self.build_chain(rt, chain, env, &what)
    }

    /// The view a view `let` is ([`crate::lower::Program::let_chains`]),
    /// if this can build it; its root is bound in `env` already.
    pub(crate) fn let_view(
        self: &Rc<Self>,
        rt: &Runtime,
        def: crate::hir::DefId,
        env: &Rc<Env>,
    ) -> Option<Derived> {
        let prog = self.vm.prog.clone();
        let chain = prog.let_chains.get(&def)?;
        let what: Rc<str> = format!("let {}", prog.def(def).name).into();
        self.build_chain(rt, chain, env, &what)
    }

    fn build_chain(
        self: &Rc<Self>,
        rt: &Runtime,
        chain: &Chain,
        env: &Rc<Env>,
        what: &Rc<str>,
    ) -> Option<Derived> {
        let prog = self.vm.prog.clone();
        // A lambda calling a service method reads more than its
        // parameters can say: compare whole lists instead.
        if chain.steps.iter().any(|(_, c)| {
            prog.reads(*c)
                .services
                .iter()
                .any(|(_, field)| field.is_none())
        }) {
            return None;
        }
        let mut steps = chain.steps.iter();
        let mut cur = match &chain.root {
            ChainRoot::Def(d) => match env.def(*d)? {
                Slot::Keyed(k, _) => {
                    let (op, arg) = steps.next()?;
                    self.chain_step(rt, k, *op, *arg, env, what)
                }
                Slot::View(v, _) => v,
                _ => return None,
            },
            ChainRoot::Service(s, field) => {
                let k = self.vm.host.read_keyed(rt, s, field)?;
                let (op, arg) = steps.next()?;
                self.chain_step(rt, k, *op, *arg, env, what)
            }
        };
        for (op, arg) in steps {
            cur = self.chain_step(rt, cur, *op, *arg, env, what);
        }
        Some(cur)
    }

    /// One step of a chain over `src`.
    fn chain_step<S>(
        self: &Rc<Self>,
        rt: &Runtime,
        src: S,
        op: ChainOp,
        arg: ChunkId,
        env: &Rc<Env>,
        what: &Rc<str>,
    ) -> Derived
    where
        S: KeyedSource<ValueKey, Value>,
    {
        let (ctx, e) = (self.clone(), env.clone());
        let m = if op == ChainOp::Take {
            src.take_with(rt, move |rt| {
                let n = ctx.eval(rt, arg, &e)?;
                Ok(n.as_f64().unwrap_or(0.0).max(0.0) as usize)
            })
        } else {
            let params = move |rt: &Runtime| -> Result<StepParams, Error> {
                let f = ctx.eval(rt, arg, &e)?;
                let code = match &f {
                    Value::Fn(c) => Some((c.chunk, c.captured.clone())),
                    _ => None,
                };
                Ok(StepParams {
                    f,
                    code,
                    touched: ctx.touch(rt, arg, &e)?,
                })
            };
            // The lambda, per item, untracked (its reads beyond the item
            // are the parameters). An error is reported once and the item
            // counts as filtered out / null / equal.
            let (vm, weak, me) = (self.vm.clone(), rt.downgrade(), Rc::downgrade(self));
            let last: Rc<RefCell<Option<String>>> = Rc::default();
            let what = what.clone();
            let call = move |p: &StepParams, v: &Value| -> Option<Value> {
                let rt = weak.upgrade()?;
                match rt.untrack(|rt| vm.call(rt, &p.f, vec![v.clone()])) {
                    Ok(r) => Some(r),
                    Err(err) => {
                        let msg = err.to_string();
                        if last.borrow().as_deref() != Some(msg.as_str()) {
                            *last.borrow_mut() = Some(msg);
                            if let Some(ctx) = me.upgrade() {
                                ctx.error(what.to_string(), err);
                            }
                        }
                        None
                    }
                }
            };
            match op {
                ChainOp::Filter => src.filter_with(rt, params, move |p, v| {
                    call(p, v).is_some_and(|r| r.truthy())
                }),
                ChainOp::Map => {
                    src.map_with(rt, params, move |p, v| call(p, v).unwrap_or(Value::Null))
                }
                _ => src.sort_by_with(rt, params, move |p, a, b| match (call(p, a), call(p, b)) {
                    (Some(x), Some(y)) => crate::vm::builtins::total_compare(&x, &y),
                    _ => std::cmp::Ordering::Equal,
                }),
            }
        };
        rt.set_name(m.id(), &**what);
        // Declared edges: the source, and what the argument reads.
        let mut extra = vec![src.node()];
        extra.extend(self.read_ids(rt, &[arg], env));
        let _ = rt.reads_from(m.id(), &extra);
        m
    }

    /// Read (tracked) everything `chunk` can read in `env`, as values to
    /// compare: a keyed collection by its version, never as a copy.
    pub(crate) fn touch(
        self: &Rc<Self>,
        rt: &Runtime,
        chunk: ChunkId,
        env: &Rc<Env>,
    ) -> Result<Vec<Value>, Error> {
        let prog = self.vm.prog.clone();
        let r = prog.reads(chunk);
        let mut out = Vec::new();
        let slot = |s: Slot, out: &mut Vec<Value>| -> Result<(), Error> {
            match s {
                Slot::Keyed(k, _) => out.push(Value::int(k.snapshot(rt)?.version() as i64)),
                s => out.push(s.get(rt)?),
            }
            Ok(())
        };
        for d in &r.defs {
            match env.settings(*d) {
                Some(s) => {
                    for (_, f) in &s.fields {
                        out.push(f.get(rt)?);
                    }
                }
                None => {
                    if let Some(s) = env.def(*d) {
                        slot(s, &mut out)?;
                    }
                }
            }
        }
        for (d, f) in &r.fields {
            match env.settings(*d).and_then(|s| s.field(f)) {
                Some(sig) => out.push(sig.get(rt)?),
                None => {
                    if let Some(s) = env.def(*d) {
                        slot(s, &mut out)?;
                    }
                }
            }
        }
        for l in &r.locals {
            if let Some(s) = env.local(*l) {
                slot(s, &mut out)?;
            }
        }
        for (s, f) in &r.services {
            let Some(f) = f else { continue };
            match self.vm.host.read_keyed(rt, s, f) {
                Some(k) => out.push(Value::int(k.snapshot(rt)?.version() as i64)),
                None => out.push(self.vm.host.read(rt, s, f)?),
            }
        }
        for n in &r.nodes {
            let st = env.node_state(rt, *n);
            out.extend([
                Value::Bool(st.hover.get(rt)?),
                Value::Bool(st.pressed.get(rt)?),
                Value::Bool(st.focused.get(rt)?),
                Value::Bool(st.selected.get(rt)?),
                st.width.get(rt)?,
                st.height.get(rt)?,
            ]);
        }
        Ok(out)
    }
}

/// The collection at the bottom of `chain` (through view `let`s) and
/// whether a `map` is on the way.
fn base_of(prog: &Program, chain: &Chain) -> (ChainRoot, bool) {
    let mut maps = chain.steps.iter().any(|(op, _)| *op == ChainOp::Map);
    let mut root = chain.root.clone();
    for _ in 0..64 {
        let ChainRoot::Def(d) = &root else { break };
        let Some(next) = prog.let_chains.get(d) else {
            break;
        };
        maps |= next.steps.iter().any(|(op, _)| *op == ChainOp::Map);
        root = next.root.clone();
    }
    (root, maps)
}
