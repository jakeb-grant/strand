//! Builtin functions, methods, operators and field access.

use std::cmp::Ordering;
use std::rc::Rc;
use std::time::Duration;

use strand_core::{Error, Runtime};
use strand_scene::{BinOp, Color, Oklch, TokenExpr, TokenMethod};

use super::Vm;
use super::exec::{Args, EventCtx};
use super::value::{AsyncValue, CallValue, NodeState, Num, PendingOp, Value};
use crate::hir::{BinaryOp, UnaryOp};
use crate::ty::{Prim, Ty, TypeTable};

fn fail(msg: impl Into<String>) -> Error {
    Error::failed(msg.into())
}

/// A builtin value: `t` (seconds since the node appeared). Time signals
/// are the render thread's (M4); until then `t` reads 0.
pub(crate) fn value(name: &str) -> Value {
    match name {
        "t" => Value::float(0.0),
        _ => Value::Null,
    }
}

/// The default value of a type: what an unset service field or a missing
/// record argument holds.
pub fn default_of(types: &TypeTable, ty: &Ty) -> Value {
    default_depth(types, ty, 0)
}

fn default_depth(types: &TypeTable, ty: &Ty, depth: u32) -> Value {
    match ty {
        Ty::Optional(_) | Ty::Null | Ty::Error | Ty::Any | Ty::Unit => Value::Null,
        Ty::Prim(p) => match p {
            Prim::Bool => Value::Bool(false),
            Prim::Int => Value::int(0),
            Prim::Float => Value::float(0.0),
            Prim::Length => Value::Num(0.0, Num::Px),
            Prim::Percent => Value::Num(0.0, Num::Percent),
            Prim::Angle => Value::Num(0.0, Num::Deg),
            Prim::Duration => Value::Num(0.0, Num::Ms),
            Prim::Color | Prim::Paint => Value::Color(Color::TRANSPARENT),
            Prim::Text | Prim::Path => Value::text(""),
            _ => Value::Null,
        },
        Ty::Enum(e) => Value::Enum(*e, 0),
        Ty::List(..) => Value::list(Vec::new()),
        Ty::Async(inner) => Value::Async(Rc::new(AsyncValue {
            value: Some(default_depth(types, inner, depth + 1)),
            pending: false,
            error: None,
            op: None,
        })),
        Ty::Record(r) if depth < 8 => {
            let def = types.record(*r);
            let fields = def
                .fields
                .iter()
                .map(|f| default_depth(types, &f.ty, depth + 1))
                .collect();
            Value::record(*r, fields)
        }
        _ => Value::Null,
    }
}

// ---------------------------------------------------------------------------
// Layout queries

/// What a `when` condition read while [`layout_query`] ran it.
#[derive(Copy, Clone, Debug, Default)]
struct QueryState {
    /// The offset added to every laid-out size it reads.
    bias: f64,
    /// It read a laid-out size.
    read: bool,
    /// One of them was a boot value: its node not laid out yet.
    boot_value: bool,
    /// A condition is being evaluated at all.
    active: bool,
}

thread_local! {
    static QUERY: std::cell::Cell<QueryState> = const {
        std::cell::Cell::new(QueryState {
            bias: 0.0,
            read: false,
            boot_value: false,
            active: false,
        })
    };
}

/// What a `when` condition read of laid-out sizes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct QueryRead {
    /// It read one: it is a container query.
    pub read: bool,
    /// One it read is still its boot value (no layout fact yet).
    pub boot_value: bool,
}

/// Runs `f` with every `width`/`height` of a node it reads moved by
/// `bias` logical pixels; returns its result and what it read. This is
/// how container queries get their hysteresis.
pub(crate) fn layout_query<R>(bias: f64, f: impl FnOnce() -> R) -> (R, QueryRead) {
    let outer = QUERY.with(|q| {
        q.replace(QueryState {
            bias,
            active: true,
            ..QueryState::default()
        })
    });
    let r = f();
    let st = QUERY.with(|q| q.replace(outer));
    (
        r,
        QueryRead {
            read: st.read,
            boot_value: st.boot_value,
        },
    )
}

fn laid_out(vm: &Vm, n: &Rc<NodeState>, v: Value) -> Value {
    let (bias, in_query) = QUERY.with(|q| {
        let mut st = q.get();
        st.read = true;
        st.boot_value |= !n.laid_out.get();
        q.set(st);
        (st.bias, st.active)
    });
    vm.watch(
        n,
        if in_query {
            NodeState::WATCH_QUERY
        } else {
            NodeState::WATCH_SIZE
        },
    );
    match v.as_f64() {
        Some(x) if bias != 0.0 => Value::float(x + bias),
        _ => v,
    }
}

// ---------------------------------------------------------------------------
// Fields

pub(crate) fn field(vm: &Rc<Vm>, rt: &Runtime, base: &Value, name: &str) -> Result<Value, Error> {
    Ok(match base {
        Value::Null => Value::Null,
        Value::Record(r) => {
            let def = vm.types().record(r.ty);
            match def.fields.iter().position(|f| f.name == name) {
                Some(i) => r.fields.get(i).cloned().unwrap_or(Value::Null),
                None => return Err(fail(format!("`{}` has no field `{name}`", def.name))),
            }
        }
        Value::Service(s) => vm.host.read(rt, s, name)?,
        Value::Node(n) => match name {
            "hover" => Value::Bool(n.hover.get(rt)?),
            "pressed" => Value::Bool(n.pressed.get(rt)?),
            "focused" => Value::Bool(n.focused.get(rt)?),
            "selected" => Value::Bool(n.selected.get(rt)?),
            "width" => laid_out(vm, n, n.width.get(rt)?),
            "height" => laid_out(vm, n, n.height.get(rt)?),
            _ => Value::Null,
        },
        Value::List(items) => list_field(items, name),
        Value::Text(t) if name == "len" => Value::int(t.chars().count() as i64),
        Value::Async(a) => match name {
            "pending" => Value::Bool(a.pending),
            "error" => a
                .error
                .as_ref()
                .map_or(Value::Null, |e| Value::text(e.clone())),
            "value" => a.value.clone().unwrap_or(Value::Null),
            _ => match &a.value {
                Some(Value::List(items)) => list_field(items, name),
                _ => list_field(&[], name),
            },
        },
        _ => Value::Null,
    })
}

fn list_field(items: &[Value], name: &str) -> Value {
    match name {
        "len" => Value::int(items.len() as i64),
        "first" => items.first().cloned().unwrap_or(Value::Null),
        "last" => items.last().cloned().unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

pub(crate) fn index(base: &Value, index: &Value) -> Value {
    let Some(items) = base.as_list() else {
        return Value::Null;
    };
    match index.as_f64() {
        Some(i) if i >= 0.0 => items.get(i as usize).cloned().unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

// ---------------------------------------------------------------------------
// Operators

pub(crate) fn unary(op: UnaryOp, v: Value) -> Result<Value, Error> {
    Ok(match (op, v) {
        (UnaryOp::Not, v) => Value::Bool(!v.truthy()),
        (UnaryOp::Neg, Value::Num(n, u)) => Value::Num(-n, u),
        (UnaryOp::Neg, Value::Token(t)) => Value::token(TokenExpr::Binary {
            op: BinOp::Sub,
            lhs: Box::new(TokenExpr::value(strand_scene::PropValue::Number(0.0))),
            rhs: Box::new((*t).clone()),
        }),
        (UnaryOp::Neg, Value::Null) => Value::Null,
        (UnaryOp::Neg, _) => return Err(fail("`-` needs a number")),
        (UnaryOp::Await, v) => v,
    })
}

/// Structural equality; numbers compare by value whatever their unit
/// (`1 == 1.0`).
pub fn equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Num(x, _), Value::Num(y, _)) => x == y,
        (Value::List(x), Value::List(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(a, b)| equal(a, b))
        }
        (Value::Record(x), Value::Record(y)) => {
            x.ty == y.ty
                && x.fields.len() == y.fields.len()
                && x.fields.iter().zip(&y.fields).all(|(a, b)| equal(a, b))
        }
        (Value::Color(x), Value::Color(y)) => x.to_rgba8() == y.to_rgba8(),
        _ => a == b,
    }
}

fn token_of(v: &Value) -> Option<TokenExpr> {
    match v {
        Value::Token(t) => Some((**t).clone()),
        Value::Num(n, Num::Percent) => Some(TokenExpr::value(strand_scene::PropValue::Number(
            (*n / 100.0) as f32,
        ))),
        Value::Num(n, _) => Some(TokenExpr::value(strand_scene::PropValue::Number(*n as f32))),
        Value::Color(c) => Some(TokenExpr::value(strand_scene::PropValue::Color(*c))),
        _ => None,
    }
}

pub(crate) fn binary(op: BinaryOp, a: &Value, b: &Value) -> Result<Value, Error> {
    use BinaryOp::*;
    Ok(match op {
        Eq => Value::Bool(equal(a, b)),
        Ne => Value::Bool(!equal(a, b)),
        Lt | Le | Gt | Ge => {
            let ord = compare(a, b);
            Value::Bool(match (op, ord) {
                (_, None) => false,
                (Lt, Some(o)) => o == Ordering::Less,
                (Le, Some(o)) => o != Ordering::Greater,
                (Gt, Some(o)) => o == Ordering::Greater,
                (_, Some(o)) => o != Ordering::Less,
            })
        }
        And => Value::Bool(a.truthy() && b.truthy()),
        Or => Value::Bool(a.truthy() || b.truthy()),
        Coalesce => match a {
            Value::Null => b.clone(),
            Value::Async(x) => x.usable().cloned().unwrap_or_else(|| b.clone()),
            v => v.clone(),
        },
        Add | Sub | Mul | Div | Rem => {
            if matches!(a, Value::Token(_)) || matches!(b, Value::Token(_)) {
                let (Some(l), Some(r)) = (token_of(a), token_of(b)) else {
                    return Err(fail("arithmetic on a token needs numbers"));
                };
                let op = match op {
                    Add => BinOp::Add,
                    Sub => BinOp::Sub,
                    Mul => BinOp::Mul,
                    _ => BinOp::Div,
                };
                return Ok(Value::token(TokenExpr::Binary {
                    op,
                    lhs: Box::new(l),
                    rhs: Box::new(r),
                }));
            }
            let (Value::Num(x, u), Value::Num(y, v)) = (a, b) else {
                if a.is_null() || b.is_null() {
                    return Ok(Value::Null);
                }
                return Err(fail(format!("`{}` needs numbers", op.as_str())));
            };
            let scalar = |u: &Num| matches!(u, Num::Int | Num::Float);
            let unit = match op {
                Div => match (u, v) {
                    (a, b) if scalar(a) && scalar(b) => Num::Float,
                    (d, s) if scalar(s) => *d,
                    _ => Num::Float,
                },
                _ => match (u, v) {
                    (Num::Int, Num::Int) => Num::Int,
                    (a, b) if scalar(a) && scalar(b) => Num::Float,
                    (s, d) if scalar(s) => *d,
                    (d, _) => *d,
                },
            };
            let r = match op {
                Add => x + y,
                Sub => x - y,
                Mul => x * y,
                Div => x / y,
                _ => x % y,
            };
            if !r.is_finite() {
                return Err(fail(format!(
                    "`{} {} {}` has no value",
                    super::value::number(*x),
                    op.as_str(),
                    super::value::number(*y)
                )));
            }
            Value::Num(r, unit)
        }
    })
}

fn compare(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Num(x, _), Value::Num(y, _)) => x.partial_cmp(y),
        (Value::Text(x), Value::Text(y)) => Some(x.cmp(y)),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        (Value::Enum(_, x), Value::Enum(_, y)) => Some(x.cmp(y)),
        (Value::Null, Value::Null) => Some(Ordering::Equal),
        (Value::Null, _) => Some(Ordering::Less),
        (_, Value::Null) => Some(Ordering::Greater),
        (Value::Record(x), Value::Record(y)) => {
            for (a, b) in x.fields.iter().zip(&y.fields) {
                match compare(a, b) {
                    Some(Ordering::Equal) => {}
                    o => return o,
                }
            }
            Some(Ordering::Equal)
        }
        _ => None,
    }
}

/// A total order over values for `sort_by`: numbers by `total_cmp`
/// (NaN after every other number), values of different kinds by kind,
/// records field by field.
pub(crate) fn total_compare(a: &Value, b: &Value) -> Ordering {
    fn kind(v: &Value) -> u8 {
        match v {
            Value::Null => 0,
            Value::Bool(_) => 1,
            Value::Num(..) => 2,
            Value::Text(_) => 3,
            Value::Enum(..) => 4,
            Value::Record(_) => 5,
            _ => 6,
        }
    }
    match (a, b) {
        (Value::Num(x, _), Value::Num(y, _)) => {
            // -0 and 0 are one key; every NaN is one key, after the rest.
            let n = |f: f64| if f.is_nan() { f64::NAN.abs() } else { f + 0.0 };
            n(*x).total_cmp(&n(*y))
        }
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Enum(_, x), Value::Enum(_, y)) => x.cmp(y),
        (Value::Record(x), Value::Record(y)) => {
            for (a, b) in x.fields.iter().zip(&y.fields) {
                match total_compare(a, b) {
                    Ordering::Equal => {}
                    o => return o,
                }
            }
            x.fields.len().cmp(&y.fields.len())
        }
        _ => kind(a).cmp(&kind(b)),
    }
}

// ---------------------------------------------------------------------------
// Builtin functions

fn call_value(name: &str, args: Vec<Value>) -> Value {
    Value::Call(Rc::new(CallValue {
        name: name.to_string(),
        args,
    }))
}

/// `noise(x)`: smooth 1D value noise in `0..=1`, the same for the same
/// `x`.
pub fn noise(x: f64) -> f64 {
    fn hash(i: i64) -> f64 {
        let mut h = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h ^= h >> 31;
        h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        h ^= h >> 29;
        (h >> 11) as f64 / (1u64 << 53) as f64
    }
    if !x.is_finite() {
        return 0.5;
    }
    let i = x.floor();
    let f = x - i;
    let s = f * f * (3.0 - 2.0 * f);
    // `as` saturates for huge `x`; the next cell wraps instead of
    // overflowing.
    let i = i as i64;
    let (a, b) = (hash(i), hash(i.wrapping_add(1)));
    a + (b - a) * s
}

/// `pct(0.42)` is `42%`.
pub fn pct(f: f64) -> String {
    format!("{}%", (f * 100.0).round() as i64)
}

/// `dur(…)`: `45 s`, `12 min`, `2 h 5 min`.
pub fn dur(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s} s")
    } else if s < 3600 {
        format!("{} min", s / 60)
    } else {
        let (h, m) = (s / 3600, (s % 3600) / 60);
        if m == 0 {
            format!("{h} h")
        } else {
            format!("{h} h {m} min")
        }
    }
}

pub(crate) fn call(
    vm: &Rc<Vm>,
    rt: &Runtime,
    name: &str,
    overload: usize,
    args: Args,
    ctx: Option<&Rc<EventCtx>>,
) -> Result<Value, Error> {
    let types = vm.types();
    let num = |i: usize| args.num(i);
    Ok(match name {
        "pct" => match args.get(0) {
            Some(Value::Num(f, _)) => Value::text(pct(*f)),
            _ => Value::Null,
        },
        "dur" => match args.get(0).and_then(Value::as_duration) {
            Some(d) => Value::text(dur(d)),
            None => Value::Null,
        },
        "join" => {
            let sep = args.get(0).map(|v| v.show(types)).unwrap_or_default();
            let parts: Vec<String> = args
                .rest
                .iter()
                .filter(|v| !v.is_null())
                .map(|v| v.show(types))
                .collect();
            Value::text(parts.join(&sep))
        }
        "material" => {
            let dark = args.get(2).is_some_and(Value::truthy);
            let contrast = num(3).unwrap_or(0.0);
            let variant = args.get(1).map(|v| v.show(types)).unwrap_or_default();
            let opts = strand_theme::Options {
                variant: super::theme::variant(&variant),
                dark,
                contrast,
            };
            if overload == 0 {
                let seed = match args.get(0) {
                    Some(Value::Color(c)) => *c,
                    _ => return Ok(Value::Null),
                };
                Value::Palette(Rc::new(strand_theme::from_seed(seed, opts)))
            } else {
                // `material(image:)`: quantised off-thread; while that
                // runs the last image palette holds (`??` takes a kept
                // value), and with none yet the fallback after `??`.
                use super::theme::ImagePalette;
                let Some(path) = args
                    .get(0)
                    .and_then(Value::as_text)
                    .filter(|p| !p.is_empty())
                else {
                    return Ok(Value::Async(Rc::new(AsyncValue::failed("no image path"))));
                };
                let Some(theme) = vm.theme() else {
                    return Ok(Value::Async(Rc::new(AsyncValue::failed(
                        "wallpaper palettes need a running instance",
                    ))));
                };
                let palette = |p: strand_theme::Palette| Value::Palette(Rc::new(p));
                let a = match theme.material_image(rt, path, opts)? {
                    ImagePalette::Ready(p) => AsyncValue::ready(palette(p)),
                    ImagePalette::Pending(last) => AsyncValue {
                        value: last.map(palette),
                        pending: true,
                        error: None,
                        op: None,
                    },
                    // A missing wallpaper is not held on to: `??` takes
                    // the fallback ("or if missing").
                    ImagePalette::Failed(e, _) => AsyncValue {
                        value: None,
                        pending: false,
                        error: Some(e.into()),
                        op: None,
                    },
                };
                Value::Async(Rc::new(a))
            }
        }
        "import" => {
            let src = args.get(0).and_then(Value::as_text).unwrap_or_default();
            let result = match vm.theme() {
                Some(theme) => theme.import(rt, src),
                None => strand_theme::import(src, None).map_err(|e| e.to_string()),
            };
            match result {
                Ok(p) => Value::Palette(Rc::new(p)),
                Err(e) => return Err(fail(e)),
            }
        }
        "oklch" => {
            if overload == 0 {
                let from = args.get(0).cloned().unwrap_or(Value::Null);
                let channel = |i: usize| args.get(i).and_then(token_of).map(Box::new);
                let symbolic =
                    from.has_tokens() || (1..5).any(|i| args.get(i).is_some_and(Value::has_tokens));
                if symbolic || matches!(from, Value::Color(_)) {
                    let Some(base) = token_of(&from) else {
                        return Ok(Value::Null);
                    };
                    let e = TokenExpr::OklchFrom {
                        base: Box::new(base),
                        l: channel(1),
                        c: channel(2),
                        h: channel(3),
                        alpha: channel(4),
                    };
                    // A literal colour with literal channels folds now.
                    if !symbolic {
                        let t = strand_scene::TokenTable::default();
                        if let Some(strand_scene::PropValue::Color(c)) = t.eval(&e) {
                            return Ok(Value::Color(c));
                        }
                    }
                    Value::token(e)
                } else {
                    Value::Null
                }
            } else {
                let c = Color::from_oklch(Oklch {
                    l: num(0).unwrap_or(0.0),
                    c: num(1).unwrap_or(0.0),
                    h: num(2).unwrap_or(0.0),
                    alpha: num(3).unwrap_or(1.0),
                });
                Value::Color(c.clamped())
            }
        }
        "rgb" => {
            let ch = |i: usize, d: f64| num(i).unwrap_or(d) as f32;
            let scale = |v: f32| if v > 1.0 { v / 255.0 } else { v };
            Value::Color(Color::new(
                scale(ch(0, 0.0)),
                scale(ch(1, 0.0)),
                scale(ch(2, 0.0)),
                ch(3, 1.0),
            ))
        }
        "min" => Value::float(num(0).unwrap_or(0.0).min(num(1).unwrap_or(0.0))),
        "max" => Value::float(num(0).unwrap_or(0.0).max(num(1).unwrap_or(0.0))),
        "clamp" => {
            let (x, lo, hi) = (
                num(0).unwrap_or(0.0),
                num(1).unwrap_or(0.0),
                num(2).unwrap_or(0.0),
            );
            Value::float(x.max(lo).min(hi.max(lo)))
        }
        // `wave` is a time signal, the render thread's (M4): 0 until then.
        "wave" => Value::float(0.0),
        "noise" => Value::float(noise(num(0).unwrap_or(0.0))),
        "sleep" => {
            let Some(d) = args.get(0).and_then(Value::as_duration) else {
                return Err(Error::failed(format!(
                    "`sleep` needs a duration from 0 to {} s",
                    Duration::MAX.as_secs()
                )));
            };
            let sleep = rt.sleep(d);
            Value::Async(Rc::new(AsyncValue {
                value: None,
                pending: true,
                error: None,
                op: Some(Rc::new(PendingOp::new(Box::pin(async move {
                    sleep.await;
                    Ok(Value::Unit)
                })))),
            }))
        }
        "propagate" => {
            if let (Some(hooks), Some(ctx)) = (vm.hooks(), ctx) {
                hooks.propagate(rt, ctx);
            }
            Value::Unit
        }
        // Call-shaped values the emitter lowers by name: gradients,
        // springs and curves, filters, masks, poses, hit areas, sprites.
        _ => {
            let mut all: Vec<Value> = args
                .params
                .into_iter()
                .map(|v| v.unwrap_or(Value::Null))
                .collect();
            all.extend(args.rest);
            call_value(name, all)
        }
    })
}

// ---------------------------------------------------------------------------
// Methods

pub(crate) fn method(
    vm: &Rc<Vm>,
    rt: &Runtime,
    recv: &Value,
    name: &str,
    args: Args,
) -> Result<Value, Error> {
    match recv {
        Value::Null => Ok(Value::Null),
        Value::Service(s) => vm.host.call(rt, s, name, &args.into_vec()),
        Value::Token(_) | Value::Color(_) if TokenMethod::from_name(name).is_some() => {
            color_method(recv, name, &args)
        }
        Value::Text(t) => text_method(t, name, &args),
        Value::Num(n, u) => num_method(*n, *u, name, &args),
        Value::List(items) => list_method(vm, rt, items, name, args),
        Value::Async(a) => {
            let again = a.op.as_ref().map(|_| Args {
                params: args.params.clone(),
                rest: args.rest.clone(),
            });
            let out = match &a.value {
                Some(Value::List(items)) => list_method(vm, rt, items, name, args)?,
                _ => list_method(vm, rt, &[], name, args)?,
            };
            // A transform of a loading list is still loading (the
            // checker types it `Async<[U]>`): `.pending` and `.error`
            // carry over, so `?? fallback` still covers them, and `await`
            // on it waits for the source, then applies the transform.
            Ok(if matches!(out, Value::List(_)) {
                let op = again.map(|args| derived_op(vm, rt, a, name, args));
                Value::Async(Rc::new(AsyncValue {
                    value: a.value.as_ref().map(|_| out),
                    pending: a.pending,
                    error: a.error.clone(),
                    op,
                }))
            } else {
                out
            })
        }
        Value::Record(r) => {
            let def = vm.types().record(r.ty);
            if def.name == "Date" {
                return super::clock::date_method(vm.types(), recv, name, &args.into_vec());
            }
            // Canvas drawing and other render-side methods do nothing on
            // the logic thread.
            Ok(Value::Unit)
        }
        _ => Ok(Value::Null),
    }
}

/// What `await` waits on for `source.name(args)` (`hits.take(2)`): the
/// source's load, then the transform of its result. Weak handles, so a
/// value kept in state does not keep the VM or the runtime alive.
fn derived_op(
    vm: &Rc<Vm>,
    rt: &Runtime,
    source: &Rc<AsyncValue>,
    name: &str,
    args: Args,
) -> Rc<PendingOp> {
    let (vm, rt) = (Rc::downgrade(vm), rt.downgrade());
    let (source, name) = (Value::Async(source.clone()), name.to_string());
    Rc::new(PendingOp::new(Box::pin(async move {
        let settled = super::exec::await_value(source)
            .await
            .map_err(|e| e.to_string())?;
        let (Some(vm), Some(rt)) = (vm.upgrade(), rt.upgrade()) else {
            return Err("the program was unloaded".to_string());
        };
        let items = match &settled {
            Value::List(items) => items.as_slice(),
            _ => &[],
        };
        list_method(&vm, &rt, items, &name, args).map_err(|e| e.to_string())
    })))
}

fn color_method(recv: &Value, name: &str, args: &Args) -> Result<Value, Error> {
    let Some(method) = TokenMethod::from_name(name) else {
        return Ok(Value::Null);
    };
    let symbolic =
        matches!(recv, Value::Token(_)) || args.params.iter().flatten().any(Value::has_tokens);
    if symbolic {
        let receiver = token_of(recv).ok_or_else(|| fail("not a colour"))?;
        let targs = args
            .params
            .iter()
            .flatten()
            .map(|a| token_of(a).ok_or_else(|| fail(format!("`{name}` needs colours and numbers"))))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(Value::token(receiver.call(method, targs)));
    }
    let Value::Color(c) = recv else {
        return Ok(Value::Null);
    };
    let frac = |v: Option<&Value>| match v {
        Some(Value::Num(n, Num::Percent)) => *n / 100.0,
        Some(Value::Num(n, _)) => *n,
        _ => 0.0,
    };
    let out = match method {
        TokenMethod::Alpha => c.with_alpha(frac(args.get(0)) as f32),
        TokenMethod::Mix => match args.get(0) {
            Some(Value::Color(o)) => {
                c.lerp_oklab(*o, frac(args.get(1).or(Some(&Value::float(0.5)))) as f32)
            }
            _ => *c,
        },
        TokenMethod::Lighten | TokenMethod::Darken => {
            let mut lch = c.to_oklch();
            let d = frac(args.get(0));
            lch.l += if method == TokenMethod::Lighten {
                d
            } else {
                -d
            };
            Color::from_oklch(lch)
        }
    };
    Ok(Value::Color(out.clamped()))
}

fn text_method(t: &str, name: &str, args: &Args) -> Result<Value, Error> {
    let s = |i: usize| {
        args.get(i)
            .and_then(Value::as_text)
            .unwrap_or("")
            .to_string()
    };
    Ok(match name {
        "upper" => Value::text(t.to_uppercase()),
        "lower" => Value::text(t.to_lowercase()),
        "trim" => Value::text(t.trim()),
        "contains" => Value::Bool(t.contains(&s(0))),
        "starts_with" => Value::Bool(t.starts_with(&s(0))),
        "ends_with" => Value::Bool(t.ends_with(&s(0))),
        "split" => Value::list(t.split(&s(0)).map(Value::text).collect()),
        "replace" => Value::text(t.replace(&s(0), &s(1))),
        _ => return Err(fail(format!("text has no method `{name}`"))),
    })
}

fn num_method(n: f64, u: Num, name: &str, args: &Args) -> Result<Value, Error> {
    let a = |i: usize| args.num(i).unwrap_or(0.0);
    let keep = |v: f64| Value::Num(v, if u == Num::Int { Num::Float } else { u });
    Ok(match name {
        "abs" => Value::Num(n.abs(), u),
        "round" => Value::int(n.round() as i64),
        "floor" => Value::int(n.floor() as i64),
        "ceil" => Value::int(n.ceil() as i64),
        "min" => keep(n.min(a(0))),
        "max" => keep(n.max(a(0))),
        "clamp" => keep(n.max(a(0)).min(a(1).max(a(0)))),
        _ => return Err(fail(format!("numbers have no method `{name}`"))),
    })
}

fn list_method(
    vm: &Rc<Vm>,
    rt: &Runtime,
    items: &[Value],
    name: &str,
    args: Args,
) -> Result<Value, Error> {
    let types = vm.types();
    let f = args.get(0).cloned().unwrap_or(Value::Null);
    let test =
        |v: &Value| -> Result<bool, Error> { Ok(vm.call(rt, &f, vec![v.clone()])?.truthy()) };
    Ok(match name {
        "filter" => {
            let mut out = Vec::new();
            for v in items {
                if test(v)? {
                    out.push(v.clone());
                }
            }
            Value::list(out)
        }
        "map" => Value::list(
            items
                .iter()
                .map(|v| vm.call(rt, &f, vec![v.clone()]))
                .collect::<Result<_, _>>()?,
        ),
        "sort_by" => {
            let mut keyed: Vec<(Value, Value)> = items
                .iter()
                .map(|v| Ok((vm.call(rt, &f, vec![v.clone()])?, v.clone())))
                .collect::<Result<_, Error>>()?;
            // A total order (NaN and mixed kinds included): the
            // standard sort may panic on one that is not.
            keyed.sort_by(|a, b| total_compare(&a.0, &b.0));
            Value::list(keyed.into_iter().map(|(_, v)| v).collect())
        }
        "take" => {
            let n = args.num(0).unwrap_or(0.0).max(0.0) as usize;
            Value::list(items.iter().take(n).cloned().collect())
        }
        "skip" => {
            let n = args.num(0).unwrap_or(0.0).max(0.0) as usize;
            Value::list(items.iter().skip(n).cloned().collect())
        }
        "reverse" => Value::list(items.iter().rev().cloned().collect()),
        "join" => {
            let sep = f.as_text().unwrap_or("").to_string();
            Value::text(
                items
                    .iter()
                    .map(|v| v.show(types))
                    .collect::<Vec<_>>()
                    .join(&sep),
            )
        }
        "contains" => Value::Bool(items.iter().any(|v| equal(v, &f))),
        "any" => {
            for v in items {
                if test(v)? {
                    return Ok(Value::Bool(true));
                }
            }
            Value::Bool(false)
        }
        "all" => {
            for v in items {
                if !test(v)? {
                    return Ok(Value::Bool(false));
                }
            }
            Value::Bool(true)
        }
        "count" => {
            let mut n = 0;
            for v in items {
                if test(v)? {
                    n += 1;
                }
            }
            Value::int(n)
        }
        "find" => {
            for v in items {
                if test(v)? {
                    return Ok(v.clone());
                }
            }
            Value::Null
        }
        _ => return Err(fail(format!("lists have no method `{name}` here"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_follow_the_checker() {
        let r = binary(
            BinaryOp::Mul,
            &Value::float(2.0),
            &Value::Num(3.0, Num::Deg),
        )
        .unwrap();
        assert_eq!(r, Value::Num(6.0, Num::Deg));
        let r = binary(BinaryOp::Add, &Value::int(2), &Value::int(3)).unwrap();
        assert_eq!(r, Value::int(5));
        let r = binary(BinaryOp::Div, &Value::int(3), &Value::int(2)).unwrap();
        assert_eq!(r, Value::float(1.5));
        let r = binary(BinaryOp::Div, &Value::int(2), &Value::float(4.0)).unwrap();
        assert!(matches!(r, Value::Num(x, Num::Float) if x == 0.5));
        let r = binary(BinaryOp::Div, &Value::float(1.0), &Value::int(4)).unwrap();
        assert!(matches!(r, Value::Num(x, Num::Float) if x == 0.25));
        let r = binary(BinaryOp::Div, &Value::Num(90.0, Num::Deg), &Value::int(2)).unwrap();
        assert!(matches!(r, Value::Num(x, Num::Deg) if x == 45.0));
        assert!(binary(BinaryOp::Div, &Value::int(1), &Value::int(0)).is_err());
        assert_eq!(
            binary(BinaryOp::Eq, &Value::int(1), &Value::float(1.0)).unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn noise_is_smooth_and_bounded() {
        for i in 0..100 {
            let x = i as f64 * 0.37;
            let n = noise(x);
            assert!((0.0..=1.0).contains(&n));
            assert!((noise(x + 1e-6) - n).abs() < 1e-3);
        }
        assert_eq!(noise(2.0), noise(2.0));
        // Huge inputs saturate the cell index: no overflow panic.
        for x in [1e23, -1e23, f64::MAX, f64::MIN, 9.3e18] {
            assert!((0.0..=1.0).contains(&noise(x)), "{x}");
        }
    }

    #[test]
    fn sort_keys_are_totally_ordered() {
        // NaN keys mixed with numbers, text and null: the comparator must
        // be a total order or the standard sort may panic.
        let mut keys: Vec<Value> = (0..200)
            .map(|i| match i % 5 {
                0 => Value::float(f64::NAN),
                1 => Value::float(-(i as f64)),
                2 => Value::Null,
                3 => Value::text(format!("{i}")),
                _ => Value::float(i as f64 * 0.5),
            })
            .collect();
        keys.sort_by(total_compare);
        assert!(keys[0].is_null());
        let nums: Vec<f64> = keys.iter().filter_map(Value::as_f64).collect();
        let firstnan = nums.iter().position(|n| n.is_nan()).unwrap();
        assert!(nums[firstnan..].iter().all(|n| n.is_nan()));
        assert!(nums[..firstnan].windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(
            total_compare(&Value::float(-0.0), &Value::float(0.0)),
            Ordering::Equal
        );
    }

    #[test]
    fn text_helpers() {
        assert_eq!(pct(0.424), "42%");
        assert_eq!(dur(Duration::from_secs(45)), "45 s");
        assert_eq!(dur(Duration::from_secs(7500)), "2 h 5 min");
        assert_eq!(dur(Duration::from_secs(7200)), "2 h");
    }

    #[test]
    fn token_arithmetic_stays_symbolic() {
        let l = Value::token(TokenExpr::Channel(strand_scene::Channel::L));
        let r = binary(BinaryOp::Add, &l, &Value::float(0.12)).unwrap();
        assert!(matches!(r, Value::Token(_)));
    }
}
