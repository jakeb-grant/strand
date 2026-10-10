//! Values to scene props: [`Value`] → [`PropValue`], [`TokenExpr`] and
//! [`Transition`], and widget writes back.
//!
//! The prop's type (from the schema) decides what a comma or
//! space-separated value means: `1, $border` is a border, `0 2px 8px
//! $shadow` a shadow, `"Inter" 13px 500` a font. Values holding tokens
//! stay symbolic: a colour slot filled by a token becomes a
//! [`TokenExpr::Template`], a comma shorthand keeps `PropValue::Token`
//! items, so render evaluates them every frame. Time-bound values (M4:
//! `t`, `wave(…)`, `noise(t)`) travel the same way: a whole prop as a
//! `PropValue::Token`, a number inside a composite value (a border's
//! width, a gradient's angle or `from`, a shadow's offsets and blur) as a
//! template's numeric slot, so `border: 1.5, conic(from: t * 40deg, …)`
//! is one value render evaluates per node per frame.

use std::time::Duration;

use strand_scene::{
    Border, Color, Easing, Font, GradientStop, Length, Paint, Prop, PropValue, Shadow, TokenExpr,
    TokenTable, Transition,
};

use crate::ty::{EnumId, Prim, Ty, TypeTable};
use crate::vm::value::{CallValue, Num, Value, duration_ms, f32_to_f64, fits};

/// Colour (or number) slots of a composite value: a placeholder in the
/// value, the token or time-bound expression that fills it here.
type Slots = Vec<Option<TokenExpr>>;

fn number(v: &Value) -> Option<f32> {
    match v {
        Value::Num(n, _) => Some(*n as f32),
        _ => None,
    }
}

/// A number or a symbolic one (a token, a time-bound value such as `t *
/// 40deg`): the placeholder and its slot.
fn number_slot(v: &Value) -> Option<(f32, Option<TokenExpr>)> {
    match v {
        Value::Num(n, _) => Some((*n as f32, None)),
        Value::Token(t) | Value::Time(t) => Some((0.0, Some((**t).clone()))),
        _ => None,
    }
}

/// Whether a symbolic value is a number by its shape: a time leaf,
/// arithmetic (always numeric in render), or a literal number. A colour
/// method result (`$accent.alpha(wave(1s))`), `oklch(from …)` or a token
/// reference may be a colour.
fn numeric(e: &TokenExpr) -> bool {
    match e {
        TokenExpr::Time
        | TokenExpr::Wave { .. }
        | TokenExpr::Noise(_)
        | TokenExpr::Index
        | TokenExpr::Count
        | TokenExpr::Binary { .. } => true,
        TokenExpr::Value(v) => matches!(**v, PropValue::Number(_) | PropValue::Length(_)),
        _ => false,
    }
}

/// A colour or token colour: the placeholder and its slot. A symbolic
/// number (`8 * wave(2s)`, `$gap * 2`) is not one.
fn color_slot(v: &Value) -> Option<(Color, Option<TokenExpr>)> {
    match v {
        Value::Color(c) => Some((*c, None)),
        Value::Token(t) | Value::Time(t) if !numeric(t) => {
            Some((Color::BLACK, Some((**t).clone())))
        }
        _ => None,
    }
}

/// A paint's slots: its colours, and its numbers in
/// [`PropValue::numbers_mut`] order (a gradient's angle or `from`, then
/// its stops' offsets, which are never symbolic).
struct PaintSlots {
    colors: Slots,
    numbers: Slots,
}

/// A paint (colour or gradient) with its slots.
fn paint(v: &Value) -> Option<(Paint, PaintSlots)> {
    if let Some((c, slot)) = color_slot(v) {
        let slots = PaintSlots {
            colors: vec![slot],
            numbers: Vec::new(),
        };
        return Some((Paint::Solid(c), slots));
    }
    let Value::Call(c) = v else {
        return None;
    };
    // A gradient has colour stops; `radial(center, 40%)` is a mask.
    let stop_args = match c.name.as_str() {
        "linear" | "conic" => c.args.get(1..).unwrap_or(&[]),
        "radial" => &c.args[..],
        _ => return None,
    };
    if !stop_args.iter().any(|a| color_slot(a).is_some()) {
        return None;
    }
    let stops = |vals: &[Value]| -> (Vec<GradientStop>, Slots) {
        let colors: Vec<(Color, Option<TokenExpr>)> = vals.iter().filter_map(color_slot).collect();
        let n = colors.len().max(2) - 1;
        let stops = colors
            .iter()
            .enumerate()
            .map(|(i, (c, _))| GradientStop {
                offset: i as f32 / n as f32,
                color: *c,
            })
            .collect();
        (stops, colors.into_iter().map(|(_, s)| s).collect())
    };
    let (lead, lead_slot) = c
        .args
        .first()
        .and_then(number_slot)
        .map_or((None, None), |(n, s)| (Some(n), s));
    let (stops, colors) = stops(stop_args);
    let (paint, numbers) = match c.name.as_str() {
        "linear" => (
            Paint::Linear {
                angle: lead.unwrap_or(180.0),
                stops,
            },
            vec![lead_slot],
        ),
        "conic" => (
            Paint::Conic {
                from: lead.unwrap_or(0.0),
                stops,
            },
            vec![lead_slot],
        ),
        _ => (Paint::Radial { stops }, Vec::new()),
    };
    Some((paint, PaintSlots { colors, numbers }))
}

/// A value whose colours or numbers may be tokens or time-bound: plain,
/// or a template (numbers' trailing empty slots dropped).
fn templated(value: PropValue, colors: Slots, mut numbers: Slots) -> PropValue {
    while numbers.last().is_some_and(Option::is_none) {
        numbers.pop();
    }
    if colors.iter().any(Option::is_some) || !numbers.is_empty() {
        PropValue::Token(TokenExpr::Template {
            value: Box::new(value),
            colors,
            numbers,
        })
    } else {
        value
    }
}

/// One shadow from `x y blur [spread] color`, with its colour slot and
/// its four number slots. The colour is the last part when that can be
/// a colour: a colour, a token or a colour-valued time-bound value
/// (`$accent.alpha(wave(1s))`), never a symbolic number (`8 *
/// wave(2s)`, which with no colour given is the blur); every other part
/// is a number, possibly symbolic.
fn shadow(items: &[Value]) -> Option<(Shadow, Option<TokenExpr>, Slots)> {
    // The colour: the last part if it can be one, else the first plain
    // colour or token (`$shadow 0 2px 8px` reads as CSS does).
    let at = match items.last() {
        Some(last) if color_slot(last).is_some() => Some(items.len() - 1),
        _ => items.iter().position(|v| {
            matches!(v, Value::Color(_) | Value::Token(_)) && color_slot(v).is_some()
        }),
    };
    let (color, slot) = at
        .and_then(|i| color_slot(&items[i]))
        .unwrap_or((Color::BLACK.with_alpha(0.3), None));
    let nums: Vec<(f32, Option<TokenExpr>)> = items
        .iter()
        .enumerate()
        .filter(|(i, _)| Some(*i) != at)
        .filter_map(|(_, v)| number_slot(v))
        .collect();
    let n = |i: usize| nums.get(i).cloned().unwrap_or((0.0, None));
    let (x, y, blur, spread) = (n(0), n(1), n(2), n(3));
    Some((
        Shadow {
            x: x.0,
            y: y.0,
            blur: blur.0,
            spread: spread.0,
            color,
        },
        slot,
        vec![x.1, y.1, blur.1, spread.1],
    ))
}

fn shadows(v: &Value) -> Option<PropValue> {
    let list: Vec<&Value> = match v {
        Value::Commas(items) | Value::List(items) => items.iter().collect(),
        v => vec![v],
    };
    let mut out = Vec::new();
    let mut slots = Vec::new();
    let mut numbers = Vec::new();
    for item in list {
        let (s, slot, nums) = match item {
            Value::Spaced(parts) => shadow(parts)?,
            Value::Token(t) | Value::Time(t) if out.is_empty() => {
                // `shadow: $elevation.md`: the token is the whole list.
                return Some(PropValue::Token((**t).clone()));
            }
            _ => return None,
        };
        out.push(s);
        slots.push(slot);
        numbers.extend(nums);
    }
    Some(templated(PropValue::Shadow(out), slots, numbers))
}

fn font(parts: &[Value]) -> Option<PropValue> {
    let family = parts.iter().find_map(|p| p.as_text().map(str::to_string))?;
    let nums: Vec<f32> = parts.iter().filter_map(number).collect();
    Some(PropValue::Font(Font {
        family,
        size: nums.first().copied().unwrap_or(13.0),
        weight: nums.get(1).map_or(400, |w| w.clamp(1.0, 1000.0) as u16),
    }))
}

/// The scene value of `v` for a value of type `ty` (a token entry, a
/// call argument).
pub fn prop_value(types: &TypeTable, ty: &Ty, v: &Value) -> PropValue {
    convert(types, ty, v, false).unwrap_or(PropValue::Unset)
}

/// The scene value of `v` for `prop` of type `ty`: `border`, `stroke` and
/// `text_stroke` take a width and a paint (`1, $border`) as a
/// [`Border`].
pub fn prop_value_for(types: &TypeTable, prop: Prop, ty: &Ty, v: &Value) -> PropValue {
    let border = matches!(prop, Prop::Border | Prop::Stroke | Prop::TextStroke);
    convert(types, ty, v, border).unwrap_or(PropValue::Unset)
}

fn convert(types: &TypeTable, ty: &Ty, v: &Value, border_pair: bool) -> Option<PropValue> {
    let ty = match ty {
        Ty::Optional(t) => t,
        t => t,
    };
    Some(match v {
        Value::Null | Value::Unit => PropValue::Unset,
        Value::Async(a) => return a.usable().and_then(|v| convert(types, ty, v, border_pair)),
        Value::Token(t) | Value::Time(t) => PropValue::Token((**t).clone()),
        Value::Bool(b) => PropValue::Bool(*b),
        Value::Num(x, u) => {
            let n = *x as f32;
            match u {
                Num::Percent => PropValue::Length(Length::Percent(n)),
                Num::Ch => PropValue::Length(Length::Ch(n)),
                Num::Deg => PropValue::Angle(n),
                Num::Ms => PropValue::Duration(duration_ms(x.max(0.0))?),
                Num::Int | Num::Float | Num::Px => PropValue::Number(n),
            }
        }
        Value::Text(t) => PropValue::Text(t.to_string()),
        Value::Color(c) => PropValue::Color(*c),
        Value::Enum(e, i) => PropValue::Keyword(variant(types, *e, *i)?),
        Value::EnumType(e) => PropValue::List(
            types
                .enum_(*e)
                .variants
                .iter()
                .map(|v| PropValue::Keyword(v.clone()))
                .collect(),
        ),
        // `marks: h.ranges`: a `Range` is the `[start, end]` pair the
        // renderer paints.
        Value::Record(r) if types.record(r.ty).name == "Range" => PropValue::List(
            r.fields
                .iter()
                .map(|f| prop_value(types, &Ty::Any, f))
                .collect(),
        ),
        Value::Record(_) => PropValue::Text(v.identity(types).show(types)),
        Value::List(items) => {
            let elem = match ty {
                Ty::List(t, _) => (**t).clone(),
                _ => Ty::Any,
            };
            PropValue::List(items.iter().map(|i| prop_value(types, &elem, i)).collect())
        }
        Value::Commas(items) => match ty {
            Ty::Prim(Prim::Shadow) => shadows(v)?,
            _ if border_pair && items.len() == 2 => border(items)?,
            _ => {
                let part = |i: usize| match ty {
                    Ty::Tuple(ts) => ts.get(i).cloned().unwrap_or(Ty::Any),
                    _ => Ty::Any,
                };
                PropValue::List(
                    items
                        .iter()
                        .enumerate()
                        .map(|(i, x)| prop_value(types, &part(i), x))
                        .collect(),
                )
            }
        },
        Value::Spaced(items) => match ty {
            Ty::Prim(Prim::Font) => font(items)?,
            _ => shadows(v)?,
        },
        Value::Call(c) => call(types, c)?,
        Value::Pose(props) => PropValue::Pose(
            props
                .iter()
                .filter_map(|(name, v)| {
                    let p = Prop::from_name(name)?;
                    Some((p, prop_value(types, &Ty::Any, v)))
                })
                .collect(),
        ),
        Value::Node(n) => PropValue::Node(n.scene.get()?),
        Value::Fn(_)
        | Value::Palette(_)
        | Value::TokenSet(_)
        | Value::Keyframes(_)
        | Value::Service(_) => return None,
    })
}

fn border(items: &[Value]) -> Option<PropValue> {
    let (width, width_slot) = items.first().and_then(number_slot).unwrap_or((1.0, None));
    let (p, slots) = items.get(1).and_then(paint)?;
    let mut numbers = vec![width_slot];
    numbers.extend(slots.numbers);
    Some(templated(
        PropValue::Border(Border { width, paint: p }),
        slots.colors,
        numbers,
    ))
}

fn variant(types: &TypeTable, e: EnumId, i: u32) -> Option<String> {
    types
        .enums
        .get(e.0 as usize)?
        .variants
        .get(i as usize)
        .cloned()
}

fn call(types: &TypeTable, c: &CallValue) -> Option<PropValue> {
    if let Some((p, slots)) = paint(&Value::Call(std::rc::Rc::new(c.clone()))) {
        return Some(templated(PropValue::Paint(p), slots.colors, slots.numbers));
    }
    if let Some(t) = transition_of_call(types, c) {
        return Some(PropValue::Transition(t));
    }
    Some(PropValue::Call {
        name: c.name.clone(),
        args: c
            .args
            .iter()
            .filter(|a| !a.is_null())
            .map(|a| prop_value(types, &Ty::Any, a))
            .collect(),
    })
}

/// The default duration of `~ bezier(…)`, which names a curve but no
/// duration.
pub const BEZIER_DURATION: Duration = Duration::from_millis(300);

fn transition_of_call(types: &TypeTable, c: &CallValue) -> Option<Transition> {
    let n = |i: usize| c.args.get(i).and_then(number);
    match c.name.as_str() {
        "spring" => Some(Transition::Spring {
            stiffness: n(0)?,
            damping: n(1)?,
        }),
        "ease" => {
            let curve = c.args.first().map(|v| v.show(types)).unwrap_or_default();
            let duration = c.args.get(1).and_then(Value::as_duration)?;
            Some(Transition::Duration {
                duration,
                easing: Easing::named(&curve).unwrap_or(Easing::STANDARD),
            })
        }
        "bezier" => Some(Transition::Duration {
            duration: BEZIER_DURATION,
            easing: Easing::Bezier {
                x1: n(0)?,
                y1: n(1)?,
                x2: n(2)?,
                y2: n(3)?,
            },
        }),
        _ => None,
    }
}

/// (M4) A keyframes block's `easing:`: a named curve (`out_back`) or
/// `bezier(…)`.
pub fn easing(types: &TypeTable, v: &Value) -> Option<Easing> {
    match v {
        Value::Enum(e, i) => Easing::named(&variant(types, *e, *i)?),
        Value::Call(c) => match transition_of_call(types, c)? {
            Transition::Duration { easing, .. } => Some(easing),
            _ => None,
        },
        _ => None,
    }
}

/// The `~` of a prop: `~ $motion.bouncy`, `~ 200ms`, `~ instant`,
/// `~ ease(out_back, 300ms)`, `~ bezier(…)`, `~ spring(…)`.
pub fn transition(types: &TypeTable, v: &Value) -> Transition {
    match v {
        Value::Token(t) => match &**t {
            TokenExpr::Ref(p) => Transition::Token(p.clone()),
            _ => Transition::Default,
        },
        Value::Num(ms, Num::Ms) => match duration_ms(ms.max(0.0)) {
            Some(duration) => Transition::Duration {
                duration,
                easing: Easing::STANDARD,
            },
            None => Transition::Default,
        },
        Value::Enum(e, i) if variant(types, *e, *i).as_deref() == Some("instant") => {
            Transition::Instant
        }
        Value::Call(c) => transition_of_call(types, c).unwrap_or_default(),
        _ => Transition::Default,
    }
}

/// A token table entry: plain (no token inside) or derived. Sets the token at `path`, replacing whatever an earlier entry (the
/// palette, a component default, a set further down the chain) put
/// there, plain or derived.
pub fn token_entry(types: &TypeTable, table: &mut TokenTable, path: &str, ty: &Ty, v: &Value) {
    let pv = prop_value(types, ty, v);
    if matches!(pv, PropValue::Unset) {
        return;
    }
    if v.has_tokens() {
        let e = match pv {
            PropValue::Token(e) => e,
            pv => TokenExpr::value(pv),
        };
        table.tokens.remove(path);
        table.insert_derived(path, e);
    } else {
        table.derived.remove(path);
        table.insert(path, pv);
    }
}

/// A value a widget wrote back (`value: <-> level`, `text: <-> query`,
/// `open: <-> open`) as the type of the place it goes to.
pub fn from_prop(types: &TypeTable, ty: &Ty, v: &PropValue) -> Option<Value> {
    let v = from_prop_unchecked(types, ty, v)?;
    fits(types, ty, &v).then_some(v)
}

fn from_prop_unchecked(types: &TypeTable, ty: &Ty, v: &PropValue) -> Option<Value> {
    let ty = match ty {
        Ty::Optional(t) => t,
        t => t,
    };
    let num = |n: f32| f32_to_f64(n);
    Some(match (ty, v) {
        (_, PropValue::Unset) => Value::Null,
        (_, PropValue::Bool(b)) => Value::Bool(*b),
        (Ty::Prim(Prim::Int), PropValue::Number(n)) => Value::int(n.round() as i64),
        (Ty::Prim(Prim::Length), PropValue::Number(n)) => Value::Num(num(*n), Num::Px),
        (Ty::Prim(Prim::Percent), PropValue::Number(n)) => Value::Num(num(*n), Num::Percent),
        (Ty::Prim(Prim::Angle), PropValue::Number(n) | PropValue::Angle(n)) => {
            Value::Num(num(*n), Num::Deg)
        }
        (_, PropValue::Number(n)) => Value::float(num(*n)),
        (_, PropValue::Text(t)) => Value::text(t.as_str()),
        (_, PropValue::Color(c)) => Value::Color(*c),
        (_, PropValue::Duration(d)) => Value::from(*d),
        (Ty::Enum(e), PropValue::Keyword(k)) => Value::Enum(*e, types.enum_(*e).variant(k)?),
        (_, PropValue::Keyword(k)) => {
            // An `any`-typed place (`segmented`'s value, `pages`' current):
            // a variant unique among the enums.
            let mut found = None;
            for (i, e) in types.enums.iter().enumerate() {
                if let Some(v) = e.variant(k) {
                    if found.is_some() {
                        return Some(Value::text(k.as_str()));
                    }
                    found = Some(Value::Enum(EnumId(i as u32), v));
                }
            }
            found.unwrap_or_else(|| Value::text(k.as_str()))
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    fn types() -> TypeTable {
        crate::schema::Schema::builtin().types.clone()
    }

    #[test]
    fn border_with_a_token_is_a_template() {
        let t = types();
        let ty = Ty::Tuple(vec![Ty::LENGTH, Ty::PAINT]);
        let v = Value::Commas(Rc::new(vec![
            Value::float(1.0),
            Value::token(TokenExpr::path("border")),
        ]));
        let pv = prop_value_for(&t, Prop::Border, &ty, &v);
        let PropValue::Token(TokenExpr::Template { value, colors, .. }) = &pv else {
            panic!("{pv:?}")
        };
        assert!(matches!(
            **value,
            PropValue::Border(Border { width: 1.0, .. })
        ));
        assert_eq!(colors, &vec![Some(TokenExpr::path("border"))]);
        let mut table = TokenTable::default();
        table.insert("border", PropValue::Color(Color::WHITE));
        assert_eq!(
            table.resolve(&pv).map(|c| c.into_owned()),
            Some(PropValue::Border(Border {
                width: 1.0,
                paint: Paint::Solid(Color::WHITE)
            }))
        );
    }

    #[test]
    fn fonts_and_shadows_from_spaced_values() {
        let t = types();
        let f = Value::Spaced(Rc::new(vec![
            Value::text("Inter"),
            Value::Num(13.0, Num::Px),
            Value::float(500.0),
        ]));
        assert_eq!(
            prop_value(&t, &Ty::FONT, &f),
            PropValue::Font(Font {
                family: "Inter".into(),
                size: 13.0,
                weight: 500
            })
        );
        let s = Value::Spaced(Rc::new(vec![
            Value::float(0.0),
            Value::Num(2.0, Num::Px),
            Value::Num(8.0, Num::Px),
            Value::token(TokenExpr::path("shadow")),
        ]));
        let pv = prop_value(&t, &Ty::SHADOW, &s);
        assert!(
            matches!(pv, PropValue::Token(TokenExpr::Template { .. })),
            "{pv:?}"
        );
    }

    /// `8 * wave(2s)`: a symbolic number, as a vm value.
    fn wave_times(k: f32) -> Value {
        Value::symbolic(TokenExpr::Binary {
            op: strand_scene::BinOp::Mul,
            lhs: Box::new(TokenExpr::value(PropValue::Number(k))),
            rhs: Box::new(TokenExpr::Wave {
                period: Duration::from_secs(2),
                phase: Box::new(TokenExpr::value(PropValue::Number(0.0))),
            }),
        })
    }

    /// A shadow whose blur reads time and that names no colour keeps the
    /// expression in the blur's number slot and the default colour; a
    /// time-bound colour (`$accent.alpha(wave(1s))`) last is its colour.
    #[test]
    fn a_time_bound_shadow_number_is_not_its_colour() {
        let t = types();
        let s = Value::Spaced(Rc::new(vec![
            Value::float(0.0),
            Value::float(0.0),
            wave_times(8.0),
        ]));
        let pv = prop_value(&t, &Ty::SHADOW, &s);
        let PropValue::Token(TokenExpr::Template {
            value,
            colors,
            numbers,
        }) = &pv
        else {
            panic!("{pv:?}")
        };
        let PropValue::Shadow(sh) = &**value else {
            panic!("{value:?}")
        };
        assert_eq!(sh[0].color, Color::BLACK.with_alpha(0.3));
        assert_eq!(colors, &vec![None]);
        assert_eq!(numbers.len(), 3, "x, y, blur: {numbers:?}");
        assert!(numbers[2].as_ref().is_some_and(TokenExpr::reads_time));
        // At t = 0.5 s (a quarter period) the blur is 8 · 0.5.
        let table = TokenTable::default();
        let levels = [&table];
        let at = strand_scene::TokenScope::new(&levels)
            .with_time(Some(strand_scene::TimeContext::at(0.5)))
            .resolve(&pv)
            .map(|c| c.into_owned());
        let Some(PropValue::Shadow(sh)) = at else {
            panic!("{at:?}")
        };
        assert!((sh[0].blur - 4.0).abs() < 1e-5, "{:?}", sh[0]);

        // A colour method on a token with a time argument is the colour.
        let pulse = Value::symbolic(TokenExpr::path("accent").call(
            strand_scene::TokenMethod::Alpha,
            vec![TokenExpr::Wave {
                period: Duration::from_secs(1),
                phase: Box::new(TokenExpr::value(PropValue::Number(0.0))),
            }],
        ));
        let s = Value::Spaced(Rc::new(vec![
            Value::float(0.0),
            Value::float(2.0),
            wave_times(8.0),
            pulse,
        ]));
        let PropValue::Token(TokenExpr::Template {
            colors, numbers, ..
        }) = prop_value(&t, &Ty::SHADOW, &s)
        else {
            panic!("a template")
        };
        assert!(colors[0].as_ref().is_some_and(TokenExpr::reads_time));
        assert!(numbers[2].is_some(), "the blur: {numbers:?}");
    }

    /// `radial(center, 40 * wave(2s))` is a mask with an animated size,
    /// not a gradient: only a colour-valued argument makes a stop.
    #[test]
    fn a_radial_mask_with_a_time_bound_size_is_not_a_gradient() {
        let t = types();
        let anchor = t.find_enum("Anchor").expect("Anchor");
        let center = t
            .enum_(anchor)
            .variants
            .iter()
            .position(|v| v == "center")
            .expect("center");
        let c = CallValue {
            name: "radial".into(),
            args: vec![Value::Enum(anchor, center as u32), wave_times(40.0)],
        };
        let pv = prop_value(&t, &Ty::Any, &Value::Call(Rc::new(c)));
        let PropValue::Call { name, args } = &pv else {
            panic!("{pv:?}")
        };
        assert_eq!(name, "radial");
        assert!(args[1].reads_time(), "{args:?}");
        // With colours it is still a gradient.
        let g = CallValue {
            name: "radial".into(),
            args: vec![
                Value::Color(Color::WHITE),
                Value::token(TokenExpr::path("accent")),
            ],
        };
        assert!(matches!(
            prop_value(&t, &Ty::Any, &Value::Call(Rc::new(g))),
            PropValue::Token(TokenExpr::Template { .. })
        ));
    }

    #[test]
    fn transitions() {
        let t = types();
        assert_eq!(
            transition(&t, &Value::token(TokenExpr::path("motion.bouncy"))),
            Transition::Token("motion.bouncy".into())
        );
        assert_eq!(
            transition(&t, &Value::Num(200.0, Num::Ms)),
            Transition::Duration {
                duration: Duration::from_millis(200),
                easing: Easing::STANDARD
            }
        );
    }
}
