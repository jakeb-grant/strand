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
use super::value::{Closure, NodeState, PendingOp, Value, ValueKey};
use super::{Env, Vm};
use crate::hir::{AssignOp, LocalId};
use crate::lower::{
    ArgMap, ChunkId, Const, KeyedQuery, KeyedRoot, Op, Pattern, Place, PlaceRoot, PlaceSeg,
};

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
    /// `propagate()` already passed this event on (a second call, or a
    /// second handler of the same event on the element, does nothing).
    pub propagated: std::cell::Cell<bool>,
}

impl EventCtx {
    pub fn new(
        node: Option<Rc<NodeState>>,
        scene: Option<strand_scene::NodeId>,
        event: impl Into<String>,
        args: Vec<Value>,
    ) -> Self {
        Self {
            node,
            scene,
            event: event.into(),
            args,
            propagated: std::cell::Cell::new(false),
        }
    }
}

pub(crate) enum Exit {
    Done(Value),
    Await(Value),
}

pub(crate) struct Machine {
    chunk: ChunkId,
    pc: usize,
    /// Stop before this op (the call of an async service `let`).
    stop: Option<usize>,
    stack: Vec<Value>,
    frame: Frame,
    /// The frame's length at each open block scope ([`Op::ScopeEnter`]).
    marks: Vec<usize>,
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
            stop: None,
            stack: Vec::new(),
            frame,
            marks: Vec::new(),
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

    /// Run until the chunk ends or an `await`. A failure is noted with
    /// the failing op's source span ([`Vm::fault_of`]), unless an inner
    /// chunk (a `let` it read, a `fn` it called) already noted it.
    pub(crate) fn run(&mut self, vm: &Rc<Vm>, rt: &Runtime) -> Result<Exit, Error> {
        let r = self.run_ops(vm, rt);
        if let Err(e) = &r {
            let chunk = vm.prog.chunk(self.chunk);
            let at = self.pc.saturating_sub(1);
            if let Some(span) = chunk.spans.get(at).or(chunk.spans.last()) {
                vm.note_fault(e, chunk.file, *span);
            }
        }
        r
    }

    fn run_ops(&mut self, vm: &Rc<Vm>, rt: &Runtime) -> Result<Exit, Error> {
        let prog = vm.prog.clone();
        let chunk = prog.chunk(self.chunk);
        while let Some(op) = chunk.ops.get(self.pc) {
            if self.stop == Some(self.pc) {
                break;
            }
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
                    // `prefs.accent` reads that settings field's signal
                    // only, not the whole record.
                    if let Some(Op::Field(n)) = chunk.ops.get(self.pc)
                        && let Some(fields) = self.env.settings(*d)
                        && let Some(sig) = fields.field(&chunk.names[*n as usize])
                    {
                        self.pc += 1;
                        let v = sig.get(rt)?;
                        self.stack.push(v);
                        continue;
                    }
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
                    // Only the locals the body reads: a lambda made in a
                    // loop costs what it uses, not the whole frame.
                    let wanted = |id: &LocalId| l.free.binary_search(id).is_ok();
                    let mut captured: Frame = Vec::new();
                    if let Some(c) = &self.closure {
                        captured.extend(c.captured.iter().filter(|(id, _)| wanted(id)).cloned());
                    }
                    captured.extend(self.frame.iter().filter(|(id, _)| wanted(id)).cloned());
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
                Op::ScopeEnter => self.marks.push(self.frame.len()),
                Op::ScopeExit => {
                    if let Some(m) = self.marks.pop() {
                        self.frame.truncate(m);
                    }
                }
                Op::Store { place, op } => {
                    let value = self.pop();
                    let place = &chunk.places[*place as usize];
                    let indices = self.pop_indices(place);
                    store(vm, rt, place, indices, &self.env, *op, value)?;
                }
                Op::IterNext { binding, end } => {
                    if let Some(&m) = self.marks.last() {
                        self.frame.truncate(m);
                    }
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
                Op::Keyed { root, query } => {
                    let arg = matches!(query, KeyedQuery::Index | KeyedQuery::Contains)
                        .then(|| self.pop());
                    let v = self.keyed(vm, rt, chunk, root, *query, arg)?;
                    self.stack.push(v);
                }
            }
        }
        if self.stop.is_some_and(|s| s == self.pc) {
            // Stopped before the call: the stack holds its operands.
            return Ok(Exit::Done(Value::Unit));
        }
        Ok(Exit::Done(self.stack.pop().unwrap_or(Value::Unit)))
    }

    /// Run up to the chunk's last op, a method call, and return its
    /// receiver and arguments instead of calling it.
    pub(crate) fn call_args(
        &mut self,
        vm: &Rc<Vm>,
        rt: &Runtime,
    ) -> Result<(Value, Vec<Value>), Error> {
        let prog = vm.prog.clone();
        let chunk = prog.chunk(self.chunk);
        let Some(Op::CallMethod { args, .. }) = chunk.ops.last() else {
            return Err(fail("not a method call"));
        };
        self.stop = Some(chunk.ops.len() - 1);
        if let Exit::Await(_) = self.run(vm, rt)? {
            return Err(fail("`await` outside a handler"));
        }
        let a = self.pop_args(&chunk.args[*args as usize]);
        let recv = self.pop();
        Ok((recv, a.into_vec()))
    }

    /// [`Op::Keyed`]: a keyed collection answered through core's
    /// accessors (tracked on the collection, no copy of the list); any
    /// other list value is read and queried as usual.
    fn keyed(
        &self,
        vm: &Rc<Vm>,
        rt: &Runtime,
        chunk: &crate::lower::Chunk,
        root: &KeyedRoot,
        query: KeyedQuery,
        arg: Option<Value>,
    ) -> Result<Value, Error> {
        let cell = match root {
            KeyedRoot::Def(d) => match self.env.def(*d) {
                Some(super::Slot::Keyed(k, _)) => Some(k),
                Some(super::Slot::View(v, _)) => return view_query(rt, v, query, arg),
                _ => None,
            },
            KeyedRoot::Service { service, field } => vm.host.read_keyed(
                rt,
                &chunk.names[*service as usize],
                &chunk.names[*field as usize],
            ),
        };
        let arg = arg.unwrap_or(Value::Null);
        let Some(k) = cell else {
            let base = match root {
                KeyedRoot::Def(d) => vm.read_def(rt, *d, &self.env)?,
                KeyedRoot::Service { service, field } => vm.host.read(
                    rt,
                    &chunk.names[*service as usize],
                    &chunk.names[*field as usize],
                )?,
            };
            return match query {
                KeyedQuery::Len => builtins::field(vm, rt, &base, "len"),
                KeyedQuery::First => builtins::field(vm, rt, &base, "first"),
                KeyedQuery::Last => builtins::field(vm, rt, &base, "last"),
                KeyedQuery::Index => Ok(builtins::index(&base, &arg)),
                KeyedQuery::Contains => builtins::method(
                    vm,
                    rt,
                    &base,
                    "contains",
                    Args {
                        params: vec![Some(arg)],
                        rest: Vec::new(),
                    },
                ),
            };
        };
        let item = |v: Option<&(ValueKey, Value)>| v.map_or(Value::Null, |(_, x)| x.clone());
        match query {
            KeyedQuery::Len => k.with(rt, |v| Value::int(v.len() as i64)),
            KeyedQuery::First => k.with(rt, |v| item(v.items().first())),
            KeyedQuery::Last => k.with(rt, |v| item(v.items().last())),
            KeyedQuery::Index => {
                let i = arg.as_f64().filter(|i| *i >= 0.0);
                k.with(rt, |v| item(i.and_then(|i| v.items().get(i as usize))))
            }
            KeyedQuery::Contains => {
                let key = k.with_untracked(rt, |v| (v.key_fn())(&arg))?;
                let found = k.get_key(rt, &key)?;
                Ok(Value::Bool(
                    found.is_some_and(|x| builtins::equal(&x, &arg)),
                ))
            }
        }
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

/// `shown.len`, `.first`, `.last`, `[i]`, `.contains(x)` on a view `let`,
/// through the view's items (no list copy).
fn view_query(
    rt: &Runtime,
    v: strand_core::KeyedMemo<ValueKey, Value>,
    query: KeyedQuery,
    arg: Option<Value>,
) -> Result<Value, Error> {
    let item = |v: Option<&(ValueKey, Value)>| v.map_or(Value::Null, |(_, x)| x.clone());
    let arg = arg.unwrap_or(Value::Null);
    match query {
        KeyedQuery::Len => v.with(rt, |xs| Value::int(xs.len() as i64)),
        KeyedQuery::First => v.with(rt, |xs| item(xs.first())),
        KeyedQuery::Last => v.with(rt, |xs| item(xs.last())),
        KeyedQuery::Index => {
            let i = arg.as_f64().filter(|i| *i >= 0.0);
            v.with(rt, |xs| item(i.and_then(|i| xs.get(i as usize))))
        }
        KeyedQuery::Contains => v.with(rt, |xs| {
            Value::Bool(xs.iter().any(|(_, x)| builtins::equal(x, &arg)))
        }),
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
/// list. A keyed `state` is a core keyed collection: each call is one
/// keyed operation (one `VecDiff`), never a copy of the list.
fn mutate(
    vm: &Rc<Vm>,
    rt: &Runtime,
    place: &Place,
    indices: Vec<Value>,
    env: &Rc<Env>,
    method: &str,
    args: Args,
) -> Result<(), Error> {
    if let PlaceRoot::Def(d) = &place.root
        && place.segs.is_empty()
        && let Some(super::Slot::Keyed(k, _)) = env.def(*d)
    {
        return mutate_keyed(vm, rt, k, method, args);
    }
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

/// A list mutation on a keyed `state`, as core keyed operations.
fn mutate_keyed(
    vm: &Rc<Vm>,
    rt: &Runtime,
    k: strand_core::KeyedSignal<ValueKey, Value>,
    method: &str,
    args: Args,
) -> Result<(), Error> {
    let arg = |i: usize| args.get(i).cloned().unwrap_or(Value::Null);
    let len = k.with_untracked(rt, |v| v.len())?;
    let index = |v: &Value| v.as_f64().map(|n| (n.max(0.0) as usize).min(len));
    let key_of_item = |item: &Value| k.with_untracked(rt, |v| (v.key_fn())(item));
    let has = |key: &ValueKey| k.with_untracked(rt, |v| v.contains_key(key));
    match method {
        "push" | "insert" => {
            let (at, item) = if method == "push" {
                (len, arg(0))
            } else {
                (index(&arg(0)).unwrap_or(len), arg(1))
            };
            if has(&key_of_item(&item)?)? {
                return Err(fail(format!(
                    "`{method}`: an item with this key is already in the list"
                )));
            }
            k.insert(rt, at, item)
        }
        "remove" => {
            let at = arg(0).as_f64().unwrap_or(-1.0);
            let key = k.with_untracked(rt, |v| {
                (at >= 0.0)
                    .then(|| v.items().get(at as usize).map(|(key, _)| key.clone()))
                    .flatten()
            })?;
            match key {
                Some(key) => k.remove_key(rt, &key),
                None => Err(fail(format!("`remove`: no item at {at}"))),
            }
        }
        "clear" => k.replace_all(rt, Vec::new()),
        "remove_key" | "move" | "update" => {
            let key = ValueKey(arg(0));
            if !has(&key)? {
                return Err(fail(format!("`{method}`: no item with this key")));
            }
            match method {
                "remove_key" => k.remove_key(rt, &key),
                "move" => {
                    let to = index(&arg(1)).unwrap_or(len).min(len.saturating_sub(1));
                    k.move_key(rt, &key, to)
                }
                _ => {
                    let Some(old) = k.get_key(rt, &key)? else {
                        return Err(fail("`update`: no item with this key"));
                    };
                    // The new item is computed before the collection is
                    // touched (no read inside its own update).
                    let new = vm.call(rt, &arg(1), vec![old])?;
                    if key_of_item(&new)? != key {
                        return Err(fail("`update` changed the item's key"));
                    }
                    k.update(rt, &key, move |v| *v = new)
                }
            }
        }
        _ => Err(fail(format!("lists have no method `{method}`"))),
    }
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
        PlaceRoot::Def(d) if env.settings(*d).is_some() => {
            let Some(fields) = env.settings(*d) else {
                return Err(fail("no settings file"));
            };
            match place.segs.split_first() {
                // `prefs.compact = true`: that field's signal only.
                Some((PlaceSeg::Field(name), rest)) => {
                    let Some(sig) = fields.field(name) else {
                        return Err(fail(format!("settings have no field `{name}`")));
                    };
                    let cur = sig.get_untracked(rt)?;
                    let new = set_in(vm, &cur, rest, &indices, op, value)?;
                    sig.set(rt, new)
                }
                // A whole record: each field.
                _ => {
                    let Value::Record(r) = &value else {
                        return Err(fail("settings take a record"));
                    };
                    let def = vm.types().record(r.ty);
                    for (f, v) in def.fields.iter().zip(&r.fields) {
                        if let Some(sig) = fields.field(&f.name) {
                            sig.set(rt, v.clone())?;
                        }
                    }
                    Ok(())
                }
            }
        }
        PlaceRoot::Def(d) => {
            let Some(slot) = env.def(*d) else {
                return Err(fail(format!(
                    "`{}` cannot be written here",
                    vm.prog.def(*d).name
                )));
            };
            if let super::Slot::Keyed(k, _) = slot {
                // `xs = [...]` replaces by key; `xs[i].done = true` updates
                // the item at `i` by its key.
                let cur = rt.untrack(|rt| slot.get(rt))?;
                let new = set_in(vm, &cur, &place.segs, &indices, op, value)?;
                let items = new.as_list().map(<[Value]>::to_vec).unwrap_or_default();
                return k.replace_all(rt, items);
            }
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
            // The leaf's path, indices resolved: the host writes only what
            // changed.
            let mut path = vec![super::host::PathSeg::Field(f.clone())];
            let mut idx = indices.iter();
            for seg in &place.segs[1..] {
                path.push(match seg {
                    PlaceSeg::Field(n) => super::host::PathSeg::Field(n.clone()),
                    PlaceSeg::Index => {
                        let i = idx.next().and_then(Value::as_f64).unwrap_or(-1.0);
                        if i < 0.0 {
                            return Err(fail(format!("index {i} out of range")));
                        }
                        super::host::PathSeg::Index(i as usize)
                    }
                });
            }
            let value = match op.binary() {
                None => value,
                Some(b) => {
                    let cur = rt.untrack(|rt| read_place(vm, rt, place, &indices, env))?;
                    builtins::binary(b, &cur, &value)?
                }
            };
            vm.host.write(rt, s, &path, value)
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
            // Pending with nothing to wait on: never a silent null or a
            // stale value.
            _ if a.pending => Err(Error::failed(
                "nothing to await: the value is still pending",
            )),
            (None, Some(v)) => Ok(v.clone()),
            (None, None) => Ok(Value::Null),
        })
    }
}

/// Poll a shared pending operation: the first awaiter to see it finish
/// keeps the result for the others and wakes them.
fn poll_op(op: &PendingOp, cx: &mut Context<'_>) -> Poll<Result<Value, String>> {
    if let Some(r) = op.result.borrow().as_ref() {
        return Poll::Ready(r.clone());
    }
    let polled = {
        let Ok(mut slot) = op.fut.try_borrow_mut() else {
            // Another awaiter is polling it right now (re-entrant): wait
            // for its wake.
            op.waiters.borrow_mut().push(cx.waker().clone());
            return Poll::Pending;
        };
        match slot.as_mut() {
            None => return Poll::Ready(Err("this value was already awaited".into())),
            Some(f) => match f.as_mut().poll(cx) {
                Poll::Pending => None,
                Poll::Ready(r) => {
                    *slot = None;
                    Some(r)
                }
            },
        }
    };
    match polled {
        None => {
            // The future keeps only the latest waker: every other awaiter
            // waits on the list.
            let mut w = op.waiters.borrow_mut();
            if !w.iter().any(|x| x.will_wake(cx.waker())) {
                w.push(cx.waker().clone());
            }
            Poll::Pending
        }
        Some(r) => {
            *op.result.borrow_mut() = Some(r.clone());
            for w in op.waiters.borrow_mut().drain(..) {
                w.wake();
            }
            Poll::Ready(r)
        }
    }
}
