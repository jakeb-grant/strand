//! Loads of async service calls (`apps.search(q)`): core's
//! `rt.async_memo(args, fetch)` behind a value memo, shared by an async
//! `let` and by such a call anywhere else in a binding
//! ([`crate::lower::Op::AsyncSite`]).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use strand_core::{Error, Memo, NodeId, Runtime};

use super::value::Value;
use super::{Env, Vm};
use crate::lower::{ChunkId, Op};

/// Told of a load's effect node when it is made.
pub(crate) type DeclareLoad = Rc<dyn Fn(&Runtime, NodeId)>;

impl Vm {
    /// The value of the async service call chunk `value` (`[Service,
    /// args…, CallMethod]`) evaluated in `env`: core's
    /// `rt.async_memo(args, fetch)` created on the first read, owned by
    /// `owner`, so each change of the arguments starts one
    /// [`super::ServiceHost::fetch`] and drops the superseded one, and
    /// the value keeps its last result while pending. `declare` is told
    /// of the load's effect (the instantiator declares the chunk's reads
    /// on it).
    pub(crate) fn async_load(
        self: &Rc<Self>,
        rt: &Runtime,
        value: ChunkId,
        env: &Rc<Env>,
        owner: Option<NodeId>,
        declare: DeclareLoad,
    ) -> Memo<Value> {
        let (vm, e) = (self.clone(), env.clone());
        let weak = rt.downgrade();
        let cell: Rc<Cell<Option<strand_core::AsyncMemo<Value>>>> = Rc::default();
        // Handlers awaiting the `let` while it is pending: woken when it
        // settles.
        let waiters: Rc<RefCell<Vec<std::task::Waker>>> = Rc::default();
        let w_outer = waiters.clone();
        rt.memo(move |rt| {
            let waiters = w_outer.clone();
            let am = match cell.get() {
                Some(am) => am,
                None => {
                    let (vm2, e2, w) = (vm.clone(), e.clone(), weak.clone());
                    let make = move |rt: &Runtime| {
                        let vm3 = vm2.clone();
                        rt.async_memo(
                            move |rt| {
                                let (recv, args) = vm2.eval_call_args(rt, value, &e2)?;
                                let Value::Service(s) = recv else {
                                    return Err(Error::failed("not a service call"));
                                };
                                let method = match vm2.prog.chunk(value).ops.last() {
                                    Some(Op::CallMethod { name, .. }) => {
                                        vm2.prog.chunk(value).names[*name as usize].clone()
                                    }
                                    _ => String::new(),
                                };
                                Ok((s, method, args))
                            },
                            move |(s, method, args): (Rc<str>, String, Vec<Value>)| {
                                let host = vm3.host.clone();
                                let rt = w.upgrade();
                                async move {
                                    let Some(rt) = rt else {
                                        return Err(Error::failed("the runtime is gone"));
                                    };
                                    let fut = host.fetch(&rt, &s, &method, args);
                                    drop(rt);
                                    fut.await
                                }
                            },
                        )
                    };
                    let am = rt.untrack(|rt| match owner {
                        Some(o) => rt.with_owner(o, make),
                        None => Ok(make(rt)),
                    })?;
                    // The input's reads go on the load's effect; readers
                    // of the `let` read the async memo.
                    declare(rt, am.effect_id());
                    // Wakes the awaiters once the load settles.
                    let w = waiters.clone();
                    let settle = move |rt: &Runtime| {
                        if !am.get(rt)?.pending() {
                            for w in w.borrow_mut().drain(..) {
                                w.wake();
                            }
                        }
                        Ok(())
                    };
                    rt.untrack(|rt| {
                        let make = |rt: &Runtime| rt.effect(settle);
                        let fx = match owner {
                            Some(o) => rt.with_owner(o, make),
                            None => Ok(make(rt)),
                        };
                        if let Ok(fx) = fx {
                            let _ = rt.reads_from(fx.id(), &[am.id()]);
                        }
                    });
                    cell.set(Some(am));
                    am
                }
            };
            let a = am.get(rt)?;
            // `await hits` while a load runs waits for it to settle (and
            // every handler awaiting it gets the same result).
            let op = a.pending().then(|| {
                let (weak, waiters) = (weak.clone(), waiters.clone());
                let fut = std::future::poll_fn(move |cx| {
                    let Some(rt) = weak.upgrade() else {
                        return std::task::Poll::Ready(Err("the runtime is gone".to_string()));
                    };
                    match am.get_untracked(&rt) {
                        Ok(a) if a.pending() => {
                            let mut w = waiters.borrow_mut();
                            if !w.iter().any(|x| x.will_wake(cx.waker())) {
                                w.push(cx.waker().clone());
                            }
                            std::task::Poll::Pending
                        }
                        Ok(a) => std::task::Poll::Ready(match (a.error(), a.value()) {
                            (Some(e), _) => Err(e.to_string()),
                            (None, v) => Ok(v.cloned().unwrap_or(Value::Null)),
                        }),
                        Err(e) => std::task::Poll::Ready(Err(e.to_string())),
                    }
                });
                Rc::new(crate::vm::value::PendingOp::new(Box::pin(fut)))
            });
            Ok(Value::Async(Rc::new(crate::vm::value::AsyncValue {
                value: a.value().cloned(),
                pending: a.pending(),
                error: a.error().map(|e| e.to_string().into()),
                op,
            })))
        })
    }

    /// [`Op::AsyncSite`]: the load of async call chunk `site` in `env`,
    /// made on its first read and kept with the scope (the nearest
    /// owning core scope), as a `let` of the call would be.
    pub(crate) fn async_site(
        self: &Rc<Self>,
        rt: &Runtime,
        site: ChunkId,
        env: &Rc<Env>,
    ) -> Result<Value, Error> {
        let found = env
            .sites
            .borrow()
            .iter()
            .find(|(c, _)| *c == site)
            .map(|(_, m)| *m);
        let memo = match found {
            Some(m) => m,
            None => {
                let mut cur = Some(env.clone());
                let mut owner = None;
                while let Some(e) = cur {
                    if let Some(o) = e.owner.get() {
                        owner = Some(o);
                        break;
                    }
                    cur = e.parent.clone();
                }
                let make = |rt: &Runtime| {
                    rt.untrack(|rt| self.async_load(rt, site, env, owner, Rc::new(|_, _| {})))
                };
                let m = match owner {
                    Some(o) => rt.with_owner(o, make)?,
                    None => make(rt),
                };
                env.sites.borrow_mut().push((site, m));
                m
            }
        };
        memo.get(rt)
    }
}
