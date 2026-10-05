//! Values to scene props: [`Value`] → [`PropValue`], [`TokenExpr`] and
//! [`Transition`], and widget writes back.
//!
//! The prop's type (from the schema) decides what a comma or
//! space-separated value means: `1, $border` is a border, `0 2px 8px
//! $shadow` a shadow, `"Inter" 13px 500` a font. Values holding tokens
//! stay symbolic: a colour slot filled by a token becomes a
//! [`TokenExpr::Template`], a comma shorthand keeps `PropValue::Token`
//! items, so render evaluates them every frame.

use std::time::Duration;

use strand_scene::{
    Border, Color, Easing, Font, GradientStop, Length, Paint, Prop, PropValue, Shadow, TokenExpr,
    TokenTable, Transition,
};

use crate::ty::{EnumId, Prim, Ty, TypeTable};
use crate::vm::value::{CallValue, Num, Value};

/// Colour slots of a composite value: a placeholder colour in the value,
/// the token that fills it here.
type Slots = Vec<Option<TokenExpr>>;

fn number(v: &Value) -> Option<f32> {
    match v {
        Value::Num(n, _) => Some(*n as f32),
        _ => None,
    }
}

/// A colour or token colour: the placeholder and its slot.
fn color_slot(v: &Value) -> Option<(Color, Option<TokenExpr>)> {
    match v {
        Value::Color(c) => Some((*c, None)),
        Value::Token(t) => Some((Color::BLACK, Some((**t).clone()))),
        _ => None,
    }
}

/// A paint (colour or gradient) with its colour slots.
fn paint(v: &Value) -> Option<(Paint, Slots)> {
    if let Some((c, slot)) = color_slot(v) {
        return Some((Paint::Solid(c), vec![slot]));
    }
    let Value::Call(c) = v else {
        return None;
    };
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
    match c.name.as_str() {
        "linear" => {
            let angle = c.args.first().and_then(number).unwrap_or(180.0);
            let (stops, slots) = stops(c.args.get(1..).unwrap_or(&[]));
            Some((Paint::Linear { angle, stops }, slots))
        }
        "radial" => {
            let (stops, slots) = stops(&c.args);
            Some((Paint::Radial { stops }, slots))
        }
        "conic" => {
            let from = c.args.first().and_then(number).unwrap_or(0.0);
            let (stops, slots) = stops(c.args.get(1..).unwrap_or(&[]));
            Some((Paint::Conic { from, stops }, slots))
        }
        _ => None,
    }
}

/// A value whose colours may be tokens: plain, or a template.
fn templated(value: PropValue, slots: Slots) -> PropValue {
    if slots.iter().any(Option::is_some) {
        PropValue::Token(TokenExpr::Template {
            value: Box::new(value),
            colors: slots,
        })
    } else {
        value
    }
}

/// One shadow from `x y blur [spread] color`.
fn shadow(items: &[Value]) -> Option<(Shadow, Option<TokenExpr>)> {
    let nums: Vec<f32> = items.iter().filter_map(number).collect();
    let (color, slot) = items
        .iter()
        .find_map(color_slot)
        .unwrap_or((Color::BLACK.with_alpha(0.3), None));
    Some((
        Shadow {
            x: nums.first().copied().unwrap_or(0.0),
            y: nums.get(1).copied().unwrap_or(0.0),
            blur: nums.get(2).copied().unwrap_or(0.0),
            spread: nums.get(3).copied().unwrap_or(0.0),
            color,
        },
        slot,
    ))
}

fn shadows(v: &Value) -> Option<PropValue> {
    let list: Vec<&Value> = match v {
        Value::Commas(items) | Value::List(items) => items.iter().collect(),
        v => vec![v],
    };
    let mut out = Vec::new();
    let mut slots = Vec::new();
    for item in list {
        let (s, slot) = match item {
            Value::Spaced(parts) => shadow(parts)?,
            Value::Token(t) if out.is_empty() => {
                // `shadow: $elevation.md`: the token is the whole list.
                return Some(PropValue::Token((**t).clone()));
            }
            _ => return None,
        };
        out.push(s);
        slots.push(slot);
    }
    Some(templated(PropValue::Shadow(out), slots))
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

/// The scene value of `v` for a prop of type `ty`.
pub fn prop_value(types: &TypeTable, ty: &Ty, v: &Value) -> PropValue {
    convert(types, ty, v).unwrap_or(PropValue::Unset)
}

fn convert(types: &TypeTable, ty: &Ty, v: &Value) -> Option<PropValue> {
    let ty = match ty {
        Ty::Optional(t) => t,
        t => t,
    };
    Some(match v {
        Value::Null | Value::Unit => PropValue::Unset,
        Value::Async(a) => return a.usable().and_then(|v| convert(types, ty, v)),
        Value::Token(t) => PropValue::Token((**t).clone()),
        Value::Bool(b) => PropValue::Bool(*b),
        Value::Num(n, u) => {
            let n = *n as f32;
            match u {
                Num::Percent => PropValue::Length(Length::Percent(n)),
                Num::Ch => PropValue::Length(Length::Ch(n)),
                Num::Deg => PropValue::Angle(n),
                Num::Ms => {
                    PropValue::Duration(Duration::from_secs_f64((n.max(0.0) / 1000.0) as f64))
                }
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
            Ty::Tuple(parts) if is_border(parts) => border(items)?,
            Ty::Union(alts)
                if alts
                    .iter()
                    .any(|t| matches!(t, Ty::Tuple(p) if is_border(p))) =>
            {
                border(items)?
            }
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
        Value::Fn(_)
        | Value::Node(_)
        | Value::Palette(_)
        | Value::TokenSet(_)
        | Value::Service(_) => return None,
    })
}

fn is_border(parts: &[Ty]) -> bool {
    matches!(
        parts,
        [Ty::Prim(Prim::Length), Ty::Prim(Prim::Paint | Prim::Color)]
    )
}

fn border(items: &[Value]) -> Option<PropValue> {
    let width = items.first().and_then(number).unwrap_or(1.0);
    let (p, slots) = items.get(1).and_then(paint)?;
    Some(templated(
        PropValue::Border(Border { width, paint: p }),
        slots,
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
        return Some(templated(PropValue::Paint(p), slots));
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

/// The `~` of a prop: `~ $motion.bouncy`, `~ 200ms`, `~ instant`,
/// `~ ease(out_back, 300ms)`, `~ bezier(…)`, `~ spring(…)`.
pub fn transition(types: &TypeTable, v: &Value) -> Transition {
    match v {
        Value::Token(t) => match &**t {
            TokenExpr::Ref(p) => Transition::Token(p.clone()),
            _ => Transition::Default,
        },
        Value::Num(ms, Num::Ms) => Transition::Duration {
            duration: Duration::from_secs_f64(ms.max(0.0) / 1000.0),
            easing: Easing::STANDARD,
        },
        Value::Enum(e, i) if variant(types, *e, *i).as_deref() == Some("instant") => {
            Transition::Instant
        }
        Value::Call(c) => transition_of_call(types, c).unwrap_or_default(),
        _ => Transition::Default,
    }
}

/// A token table entry: plain (no token inside) or derived.
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
        table.insert_derived(path, e);
    } else {
        table.insert(path, pv);
    }
}

/// A value a widget wrote back (`value: <-> level`, `text: <-> query`,
/// `open: <-> open`) as the type of the place it goes to.
pub fn from_prop(types: &TypeTable, ty: &Ty, v: &PropValue) -> Option<Value> {
    let ty = match ty {
        Ty::Optional(t) => t,
        t => t,
    };
    Some(match (ty, v) {
        (_, PropValue::Unset) => Value::Null,
        (_, PropValue::Bool(b)) => Value::Bool(*b),
        (Ty::Prim(Prim::Int), PropValue::Number(n)) => Value::int(n.round() as i64),
        (Ty::Prim(Prim::Length), PropValue::Number(n)) => Value::Num(*n as f64, Num::Px),
        (Ty::Prim(Prim::Percent), PropValue::Number(n)) => Value::Num(*n as f64, Num::Percent),
        (Ty::Prim(Prim::Angle), PropValue::Number(n) | PropValue::Angle(n)) => {
            Value::Num(*n as f64, Num::Deg)
        }
        (_, PropValue::Number(n)) => Value::float(*n as f64),
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
        let pv = prop_value(&t, &ty, &v);
        let PropValue::Token(TokenExpr::Template { value, colors }) = &pv else {
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
