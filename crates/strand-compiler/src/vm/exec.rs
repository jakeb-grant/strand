//! The bytecode interpreter.
//!
//! A [`Machine`] runs one chunk: an operand stack, a frame of locals and
//! a program counter. It returns at the end of the chunk or at an
//! `await`, keeping its state, so a handler coroutine resumes with the
//! awaited value pushed. Nested calls (`fn`s, lambdas) run their own
//! machine synchronously: they are pure and never await.

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use strand_core::{Error, Runtime};
use strand_scene::{Channel, Color, TokenExpr};

use super::builtins;
use super::value::{Closure, NodeState, PendingOp, Value};
use super::{Env, Vm};
use crate::hir::{AssignOp, LocalId};
use crate::lower::{ArgMap, ChunkId, Const, Op, Pattern, Place, PlaceRoot, PlaceSeg};

/// Locals bound while a chunk runs (handler `let`s, event parameters,
/// lambda parameters, `for` statement bindings).
pub type Frame = Vec<(LocalId, Value)>;

/// The event a handler runs for: the element it is on, the event name and
/// its arguments (`propagate()` passes it on, `play` targets the node).
#[derive(Debug)]
pub struct EventCtx {
    pub node: Option<Rc<NodeState>>,
    pub scene: Option<strand_scene::NodeId>,
    pub event: String,
    pub args: Vec<Value>,
}

pub(crate) enum Exit {
    Done(Value),
    Await(Value),
}

pub(crate) struct Machine {
    chunk: ChunkId,
    pc: usize,
    stack: Vec<Value>,
    frame: Frame,
    closure: Option<Rc<Closure>>,
    env: Rc<Env>,
    ctx: Option<Rc<EventCtx>>,
}

/// Arguments in parameter order.
pub(crate) struct Args {
    pub params: Vec<Option<Value>>,
    /// The variadic parameter's arguments.
    pub rest: Vec<Value>,
}

impl Args {
    pub fn get(&self, i: usize) -> Option<&Value> {
        self.params.get(i).and_then(Option::as_ref)
    }

    pub fn num(&self, i: usize) -> Option<f64> {
        self.get(i).and_then(Value::as_f64)
    }

    pub fn into_vec(self) -> Vec<Value> {
        self.params
            .into_iter()
            .map(|v| v.unwrap_or(Value::Null))
            .chain(self.rest)
            .collect()
    }
}

fn fail(msg: impl Into<String>) -> Error {
    Error::failed(msg.into())
}

impl Machine {
    pub(crate) fn new(
        chunk: ChunkId,
        env: Rc<Env>,
        frame: Frame,
        closure: Option<Rc<Closure>>,
        ctx: Option<Rc<EventCtx>>,
    ) -> Self {
        Self {
            chunk,
            pc: 0,
            stack: Vec::new(),
            frame,
            closure,
            env,
            ctx,
        }
    }

    pub(crate) fn push(&mut self, v: Value) {
        self.stack.push(v);
    }

    fn pop(&mut self) -> Value {
        self.stack.pop().unwrap_or(Value::Null)
    }

    fn pop_args(&mut self, map: &ArgMap) -> Args {
        let n = map.params.len();
        let at = self.stack.len().saturating_sub(n);
        let pushed: Vec<Value> = self.stack.drain(at..).collect();
        let mut params = vec![None; map.arity as usize];
        let mut rest = Vec::new();
        for (v, &p) in pushed.into_iter().zip(&map.params) {
            if Some(p) == map.variadic {
                rest.push(v);
            } else if let Some(slot) = params.get_mut(p as usize) {
                *slot = Some(v);
            }
        }
        Args { params, rest }
    }

    fn local(&self, vm: &Rc<Vm>, rt: &Runtime, id: LocalId) -> Result<Value, Error> {
        if let Some((_, v)) = self.frame.iter().rev().find(|(l, _)| *l == id) {
            return Ok(v.clone());
        }
        if let Some(c) = &self.closure
            && let Some((_, v)) = c.captured.iter().rev().find(|(l, _)| *l == id)
        {
            return Ok(v.clone());
        }
        match self.env.local(id) {
            Some(slot) => slot.get(rt),
            None => {
                let name = &vm.prog.local(id).name;
                Err(fail(format!("`{name}` has no value here")))
            }
        }
    }

    /// Run until the chunk ends or an `await`.
    pub(crate) fn run(&mut self, vm: &Rc<Vm>, rt: &Runtime) -> Result<Exit, Error> {
        let prog = vm.prog.clone();
        let chunk = prog.chunk(self.chunk);
        while let Some(op) = chunk.ops.get(self.pc) {
            self.pc += 1;
            match op {
                Op::Const(k) => {
                    let v = match &chunk.consts[*k as usize] {
                        Const::Num(n, u) => Value::Num(*n, *u),
                        Const::Text(t) => Value::text(t.as_str()),
                        Const::Color([r, g, b, a]) => {
                            Value::Color(Color::from_rgba8(*r, *g, *b, *a))
                        }
                        Const::Channel(c) => Value::token(TokenExpr::Channel(match c.as_str() {
                            "l" => Channel::L,
                            "c" => Channel::C,
                            "h" => Channel::H,
                            _ => Channel::Alpha,
                        })),
                    };
                    self.stack.push(v);
                }
                Op::Null => self.stack.push(Value::Null),
                Op::Unit => self.stack.push(Value::Unit),
                Op::Bool(b) => self.stack.push(Value::Bool(*b)),
                Op::Local(id) => {
                    let v = self.local(vm, rt, *id)?;
                    self.stack.push(v);
                }
                Op::Def(d) => {
                    let v = vm.read_def(rt, *d, &self.env)?;
                    self.stack.push(v);
                }
                Op::Service(n) => self
                    .stack
                    .push(Value::Service(chunk.names[*n as usize].as_str().into())),
                Op::Value(n) => {
                    let v = builtins::value(&chunk.names[*n as usize]);
                    self.stack.push(v);
                }
                Op::Node(idx) => {
                    let n = self.env.node_state(rt, *idx);
                    self.stack.push(Value::Node(n));
                }
                Op::Token(n) => self.stack.push(Value::token(TokenExpr::path(
                    chunk.names[*n as usize].as_str(),
                ))),
                Op::Variant(e, v) => self.stack.push(Value::Enum(*e, *v)),
                Op::EnumType(e) => self.stack.push(Value::EnumType(*e)),
                Op::Field(n) => {
                    let base = self.pop();
                    let v = builtins::field(vm, rt, &base, &chunk.names[*n as usize])?;
                    self.stack.push(v);
                }
                Op::Index => {
                    let index = self.pop();
                    let base = self.pop();
                    self.stack.push(builtins::index(&base, &index));
                }
                Op::Unary(op) => {
                    let v = self.pop();
                    self.stack.push(builtins::unary(*op, v)?);
                }
                Op::Binary(op) => {
                    let b = self.pop();
                    let a = self.pop();
                    self.stack.push(builtins::binary(*op, &a, &b)?);
                }
                Op::Jump(t) => self.pc = *t as usize,
                Op::JumpIfFalse(t) => {
                    if !self.pop().truthy() {
                        self.pc = *t as usize;
                    }
                }
                Op::AndJump(t) => {
                    if self.stack.last().is_some_and(|v| !v.truthy()) {
                        self.pc = *t as usize;
                    } else {
                        self.pop();
                    }
                }
                Op::OrJump(t) => {
                    if self.stack.last().is_some_and(Value::truthy) {
                        self.pc = *t as usize;
                    } else {
                        self.pop();
                    }
                }
                Op::CoalesceJump(t) => {
                    let v = self.pop();
                    let usable = match &v {
                        Value::Null => None,
                        Value::Async(a) => a.usable().cloned(),
                        v => Some(v.clone()),
                    };
                    if let Some(u) = usable.filter(|u| !u.is_null()) {
                        self.stack.push(u);
                        self.pc = *t as usize;
                    }
                }
                Op::NullJump(t) => {
                    if self.stack.last().is_some_and(Value::is_null) {
                        self.pc = *t as usize;
                    }
                }
                Op::Pop => {
                    self.pop();
                }
                Op::Dup => {
                    let v = self.stack.last().cloned().unwrap_or(Value::Null);
                    self.stack.push(v);
                }
                Op::CallBuiltin {
                    name,
                    overload,
                    args,
                } => {
                    let a = self.pop_args(&chunk.args[*args as usize]);
                    let v = builtins::call(
                        vm,
                        rt,
                        &chunk.names[*name as usize],
                        *overload as usize,
                        a,
                        self.ctx.as_ref(),
                    )?;
                    self.stack.push(v);
                }
                Op::CallMethod { name, args, action } => {
                    let a = self.pop_args(&chunk.args[*args as usize]);
                    let recv = self.pop();
                    let name = &chunk.names[*name as usize];
                    let v = if *action {
                        let target = match &recv {
                            Value::Service(s) => super::ActionTarget::Service(s),
                            v => super::ActionTarget::Item(v),
                        };
                        vm.host.action(rt, target, name, &a.into_vec())?;
                        Value::Unit
                    } else {
                        builtins::method(vm, rt, &recv, name, a)?
                    };
                    self.stack.push(v);
                }
                Op::CallFn { def, args } => {
                    let a = self.pop_args(&chunk.args[*args as usize]);
                    let v = vm.call_fn(rt, *def, a.into_vec())?;
                    self.stack.push(v);
                }
                Op::CallValue { args } => {
                    let a = self.pop_args(&chunk.args[*args as usize]);
                    let f = self.pop();
                    let v = vm.call(rt, &f, a.into_vec())?;
                    self.stack.push(v);
                }
                Op::MakeRecord { ty, args } => {
                    let a = self.pop_args(&chunk.args[*args as usize]);
                    let def = vm.types().record(*ty);
                    let fields = def
                        .fields
                        .iter()
                        .enumerate()
                        .map(|(i, f)| {
                            a.get(i)
                                .cloned()
                                .unwrap_or_else(|| builtins::default_of(vm.types(), &f.ty))
                        })
                        .collect();
                    self.stack.push(Value::record(*ty, fields));
                }
                Op::Mutate {
                    place,
                    method,
                    args,
                } => {
                    let a = self.pop_args(&chunk.args[*args as usize]);
                    let place = &chunk.places[*place as usize];
                    let indices = self.pop_indices(place);
                    mutate(
                        vm,
                        rt,
                        place,
                        indices,
                        &self.env,
                        &chunk.names[*method as usize],
                        a,
                    )?;
                    self.stack.push(Value::Unit);
                }
                Op::List(n) => {
                    let items = self.pop_n(*n);
                    self.stack.push(Value::list(items));
                }
                Op::Commas(n) => {
                    let items = self.pop_n(*n);
                    self.stack.push(Value::Commas(Rc::new(items)));
                }
                Op::Spaced(n) => {
                    let items = self.pop_n(*n);
                    self.stack.push(Value::Spaced(Rc::new(items)));
                }
                Op::Closure(i) => {
                    let l = &chunk.lambdas[*i as usize];
                    let mut captured = self
                        .closure
                        .as_ref()
                        .map(|c| c.captured.clone())
                        .unwrap_or_default();
                    captured.extend(self.frame.iter().cloned());
                    self.stack.push(Value::Fn(Rc::new(Closure {
                        params: l.params.clone(),
                        chunk: l.chunk,
                        captured,
                        env: self.env.clone(),
                    })));
                }
                Op::Match(p) => {
                    let v = self.pop();
                    let m = matches(&chunk.patterns[*p as usize], &chunk.consts, &v);
                    self.stack.push(Value::Bool(m));
                }
                Op::SetLocal(id) => {
                    let v = self.pop();
                    self.frame.push((*id, v));
                }
                Op::Store { place, op } => {
                    let value = self.pop();
                    let place = &chunk.places[*place as usize];
                    let indices = self.pop_indices(place);
                    store(vm, rt, place, indices, &self.env, *op, value)?;
                }
                Op::IterNext { binding, end } => {
                    let i = self.stack.last().and_then(Value::as_f64).unwrap_or(0.0) as usize;
                    let len = self.stack.len();
                    let item = len
                        .checked_sub(2)
                        .and_then(|l| self.stack[l].as_list().and_then(|xs| xs.get(i)).cloned());
                    match item {
                        Some(item) => {
                            self.frame.push((*binding, item));
                            if let Some(top) = self.stack.last_mut() {
                                *top = Value::int(i as i64 + 1);
                            }
                        }
                        None => {
                            self.pop();
                            self.pop();
                            self.pc = *end as usize;
                        }
                    }
                }
                Op::Await => {
                    let v = self.pop();
                    return Ok(Exit::Await(v));
                }
                Op::Play => {
                    let v = self.pop();
                    if let (Some(hooks), Some(ctx)) = (vm.hooks(), &self.ctx)
                        && let Some(node) = &ctx.node
                        && let Some(name) = v.as_text()
                    {
                        hooks.play(node, name);
                    }
                }
                Op::Fail(n) => return Err(fail(chunk.names[*n as usize].clone())),
            }
        }
        Ok(Exit::Done(self.stack.pop().unwrap_or(Value::Unit)))
    }

    fn pop_n(&mut self, n: u32) -> Vec<Value> {
        let at = self.stack.len().saturating_sub(n as usize);
        self.stack.drain(at..).collect()
    }

    fn pop_indices(&mut self, place: &Place) -> Vec<Value> {
        let n = place
            .segs
            .iter()
            .filter(|s| matches!(s, PlaceSeg::Index))
            .count();
        self.pop_n(n as u32)
    }
}

fn matches(p: &Pattern, consts: &[Const], v: &Value) -> bool {
    match p {
        Pattern::Wildcard => true,
        Pattern::Variant(e, i) => matches!(v, Value::Enum(e2, i2) if e2 == e && i2 == i),
        Pattern::Literal(k) => {
            let lit = match &consts[*k as usize] {
                Const::Num(n, u) => Value::Num(*n, *u),
                Const::Text(t) => Value::text(t.as_str()),
                Const::Color([r, g, b, a]) => Value::Color(Color::from_rgba8(*r, *g, *b, *a)),
                Const::Channel(_) => return false,
            };
            builtins::equal(&lit, v)
        }
        Pattern::Null => v.is_null(),
        Pattern::Bool(b) => *v == Value::Bool(*b),
        Pattern::Error => false,
    }
}

/// The key path items of the collection at `place` are identified by: the
/// state's `key`, else the item record's own key.
fn key_path(vm: &Vm, place: &Place, list: &[Value]) -> Option<Vec<String>> {
    if let PlaceRoot::Def(d) = &place.root
        && place.segs.is_empty()
        && let Some(k) = vm.prog.state_keys.get(d)
    {
        return Some(k.clone());
    }
    list.iter().find_map(|v| match v {
        Value::Record(r) => vm.types().record(r.ty).key.clone(),
        _ => None,
    })
}

fn key_of(vm: &Vm, item: &Value, path: Option<&[String]>) -> Value {
    match path {
        Some(p) => item.key_path(vm.types(), p).cloned().unwrap_or(Value::Null),
        None => item.clone(),
    }
}

/// `xs.push(x)`, `xs.remove_key(k)`, `xs.move(k, i)`, … on a writable
/// list.
fn mutate(
    vm: &Rc<Vm>,
    rt: &Runtime,
    place: &Place,
    indices: Vec<Value>,
    env: &Rc<Env>,
    method: &str,
    args: Args,
) -> Result<(), Error> {
    let current = rt.untrack(|rt| read_place(vm, rt, place, &indices, env))?;
    let mut list: Vec<Value> = current.as_list().map(<[Value]>::to_vec).unwrap_or_default();
    let path = key_path(vm, place, &list);
    let find = |list: &[Value], k: &Value| {
        list.iter()
            .position(|v| builtins::equal(&key_of(vm, v, path.as_deref()), k))
    };
    let arg = |i: usize| args.get(i).cloned().unwrap_or(Value::Null);
    let index = |v: &Value, len: usize| v.as_f64().map(|n| (n.max(0.0) as usize).min(len));
    match method {
        "push" => {
            let item = arg(0);
            if path.is_some() && find(&list, &key_of(vm, &item, path.as_deref())).is_some() {
                return Err(fail("`push`: an item with this key is already in the list"));
            }
            list.push(item);
        }
        "insert" => {
            let at = index(&arg(0), list.len()).unwrap_or(list.len());
            let item = arg(1);
            if path.is_some() && find(&list, &key_of(vm, &item, path.as_deref())).is_some() {
                return Err(fail(
                    "`insert`: an item with this key is already in the list",
                ));
            }
            list.insert(at, item);
        }
        "remove" => {
            let at = arg(0).as_f64().unwrap_or(-1.0);
            if at < 0.0 || at as usize >= list.len() {
                return Err(fail(format!("`remove`: no item at {at}")));
            }
            list.remove(at as usize);
        }
        "clear" => list.clear(),
        "remove_key" => match find(&list, &arg(0)) {
            Some(i) => {
                list.remove(i);
            }
            None => return Err(fail("`remove_key`: no item with this key")),
        },
        "move" => match find(&list, &arg(0)) {
            Some(i) => {
                let item = list.remove(i);
                let to = index(&arg(1), list.len()).unwrap_or(list.len());
                list.insert(to, item);
            }
            None => return Err(fail("`move`: no item with this key")),
        },
        "update" => match find(&list, &arg(0)) {
            Some(i) => {
                let new = vm.call(rt, &arg(1), vec![list[i].clone()])?;
                let old_key = key_of(vm, &list[i], path.as_deref());
                if path.is_some() && !builtins::equal(&key_of(vm, &new, path.as_deref()), &old_key)
                {
                    return Err(fail("`update` changed the item's key"));
                }
                list[i] = new;
            }
            None => return Err(fail("`update`: no item with this key")),
        },
        _ => return Err(fail(format!("lists have no method `{method}`"))),
    }
    store(
        vm,
        rt,
        place,
        indices,
        env,
        AssignOp::Set,
        Value::list(list),
    )
}

/// The current value of a place.
fn read_place(
    vm: &Rc<Vm>,
    rt: &Runtime,
    place: &Place,
    indices: &[Value],
    env: &Rc<Env>,
) -> Result<Value, Error> {
    let (mut v, segs) = match &place.root {
        PlaceRoot::Def(d) => (vm.read_def(rt, *d, env)?, &place.segs[..]),
        PlaceRoot::Service(s) => match place.segs.first() {
            Some(PlaceSeg::Field(f)) => (vm.host.read(rt, s, f)?, &place.segs[1..]),
            _ => (Value::Service(s.as_str().into()), &place.segs[..]),
        },
    };
    let mut idx = indices.iter();
    for seg in segs {
        v = match seg {
            PlaceSeg::Field(f) => builtins::field(vm, rt, &v, f)?,
            PlaceSeg::Index => builtins::index(&v, idx.next().unwrap_or(&Value::Null)),
        };
    }
    Ok(v)
}

/// Write `value` (combined with `op`) at `place`.
pub(crate) fn store(
    vm: &Rc<Vm>,
    rt: &Runtime,
    place: &Place,
    indices: Vec<Value>,
    env: &Rc<Env>,
    op: AssignOp,
    value: Value,
) -> Result<(), Error> {
    match &place.root {
        PlaceRoot::Def(d) => {
            let Some(slot) = env.def(*d) else {
                return Err(fail(format!(
                    "`{}` cannot be written here",
                    vm.prog.def(*d).name
                )));
            };
            let super::Slot::Signal(sig) = slot else {
                return Err(fail(format!("`{}` is not state", vm.prog.def(*d).name)));
            };
            let mut result = Ok(());
            sig.update(rt, |cur| {
                match set_in(vm, cur, &place.segs, &indices, op, value) {
                    Ok(new) => *cur = new,
                    Err(e) => result = Err(e),
                }
            })?;
            result
        }
        PlaceRoot::Service(s) => {
            let Some(PlaceSeg::Field(f)) = place.segs.first() else {
                return Err(fail(format!("`{s}` cannot be written")));
            };
            let cur = rt.untrack(|rt| vm.host.read(rt, s, f))?;
            let new = set_in(vm, &cur, &place.segs[1..], &indices, op, value)?;
            vm.host.write(rt, s, f, new)
        }
    }
}

fn set_in(
    vm: &Vm,
    cur: &Value,
    segs: &[PlaceSeg],
    indices: &[Value],
    op: AssignOp,
    value: Value,
) -> Result<Value, Error> {
    let Some((seg, rest)) = segs.split_first() else {
        return match op.binary() {
            None => Ok(value),
            Some(b) => builtins::binary(b, cur, &value),
        };
    };
    match seg {
        PlaceSeg::Field(name) => {
            let Value::Record(r) = cur else {
                return Err(fail(format!("cannot set `{name}` of a missing value")));
            };
            let def = vm.types().record(r.ty);
            let Some(i) = def.fields.iter().position(|f| f.name == *name) else {
                return Err(fail(format!("`{}` has no field `{name}`", def.name)));
            };
            let mut fields = r.fields.clone();
            fields[i] = set_in(vm, &fields[i], rest, indices, op, value)?;
            Ok(Value::record(r.ty, fields))
        }
        PlaceSeg::Index => {
            let (i, more) = indices.split_first().ok_or_else(|| fail("missing index"))?;
            let Some(list) = cur.as_list() else {
                return Err(fail("cannot index a missing list"));
            };
            let at = i.as_f64().unwrap_or(-1.0);
            if at < 0.0 || at as usize >= list.len() {
                return Err(fail(format!("index {at} out of range for {}", list.len())));
            }
            let mut items = list.to_vec();
            let at = at as usize;
            items[at] = set_in(vm, &items[at], rest, more, op, value)?;
            Ok(Value::list(items))
        }
    }
}

/// Wait for an awaited value: the future of a pending `Async` (a `sleep`,
/// a load), or the value itself.
pub(crate) fn await_value(v: Value) -> impl Future<Output = Result<Value, Error>> {
    AwaitValue { v: Some(v) }
}

struct AwaitValue {
    v: Option<Value>,
}

impl Future for AwaitValue {
    type Output = Result<Value, Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(v) = self.v.clone() else {
            return Poll::Ready(Ok(Value::Null));
        };
        let Value::Async(a) = &v else {
            self.v = None;
            return Poll::Ready(Ok(v));
        };
        if let Some(op) = &a.op {
            match poll_op(op, cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(r) => {
                    self.v = None;
                    return Poll::Ready(r.map_err(Error::failed));
                }
            }
        }
        self.v = None;
        Poll::Ready(match (&a.error, &a.value) {
            (Some(e), _) => Err(Error::failed(e.to_string())),
            (None, Some(v)) => Ok(v.clone()),
            (None, None) => Ok(Value::Null),
        })
    }
}

fn poll_op(op: &PendingOp, cx: &mut Context<'_>) -> Poll<Result<Value, String>> {
    let mut slot = op.fut.borrow_mut();
    match slot.as_mut() {
        None => Poll::Ready(Err("this value was already awaited".into())),
        Some(f) => match f.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(r) => {
                *slot = None;
                Poll::Ready(r)
            }
        },
    }
}
