//! The VM's codecs for strand-core's storage: `persist` cells and
//! settings files.
//!
//! strand-core stores persisted cells (`rt.persisted`, one file per cell
//! path, off-thread atomic writes, the default's hash, `redeclare` and
//! `reset`) and settings files (`rt.settings_file`, `toml_edit`
//! write-back, per-field validation, overlays). Values are the VM's
//! [`Value`]s, so the codecs live here: persisted values are JSON shaped by
//! the declared type ([`encode_bytes`], [`decode_bytes`]: enums by variant
//! name, records by field name), settings fields TOML items
//! ([`decode_item`], [`encode_item`]).

use serde_json::{Map, Value as Json};
use strand_core::settings::toml_edit;
use strand_scene::Color;

use super::value::{Num, Value};
use crate::ty::{Prim, Ty, TypeTable};

/// A persisted value's bytes: its JSON by declared type. A value the
/// type cannot hold encodes as `null` (decoded as the default).
pub fn encode_bytes(types: &TypeTable, ty: &Ty, v: &Value) -> Vec<u8> {
    encode(types, ty, v)
        .unwrap_or(Json::Null)
        .to_string()
        .into_bytes()
}

/// The value stored bytes hold as a `ty`; `None` if they no longer fit
/// (core then starts the cell from its default and reports it).
pub fn decode_bytes(types: &TypeTable, ty: &Ty, bytes: &[u8]) -> Option<Value> {
    let j: Json = serde_json::from_slice(bytes).ok()?;
    decode(types, ty, &j)
}

/// A settings field's TOML item as a `ty`, or why it does not fit.
pub fn decode_item(types: &TypeTable, ty: &Ty, item: &toml_edit::Item) -> Result<Value, String> {
    match item.as_value() {
        Some(v) => decode_toml(types, ty, v),
        None if matches!(ty, Ty::Optional(_)) => Ok(Value::Null),
        None => Err(format!("expected {}", types.show(ty))),
    }
}

fn decode_toml(types: &TypeTable, ty: &Ty, v: &toml_edit::Value) -> Result<Value, String> {
    use toml_edit::Value as T;
    let want = || format!("expected {}", types.show(ty));
    Ok(match (ty, v) {
        (Ty::Optional(t), v) => decode_toml(types, t, v)?,
        (Ty::Prim(p), v) => match (p, v) {
            (Prim::Bool, T::Boolean(b)) => Value::Bool(*b.value()),
            (Prim::Text | Prim::Path | Prim::Font, T::String(s)) => Value::text(s.value().as_str()),
            (Prim::Color | Prim::Paint, T::String(s)) => Value::Color(
                Color::from_hex(s.value())
                    .ok_or_else(|| "expected a colour like \"#7aa2f7\"".to_string())?,
            ),
            (Prim::Int, T::Integer(i)) => Value::int(*i.value()),
            (Prim::Duration, T::String(s)) => duration_text(s.value())
                .ok_or_else(|| "expected a duration like \"200ms\" or \"6s\"".to_string())?,
            (p, T::Integer(i)) => number(*p, *i.value() as f64).ok_or_else(want)?,
            (p, T::Float(f)) => number(*p, *f.value()).ok_or_else(want)?,
            _ => return Err(want()),
        },
        (Ty::Enum(e), T::String(s)) => {
            let def = types.enum_(*e);
            match def.variant(s.value()) {
                Some(i) => Value::Enum(*e, i),
                None => {
                    return Err(format!("expected one of {}", def.variants.join(", ")));
                }
            }
        }
        (Ty::List(t, _), T::Array(a)) => Value::list(
            a.iter()
                .map(|i| decode_toml(types, t, i))
                .collect::<Result<_, _>>()?,
        ),
        (Ty::Record(r), T::InlineTable(t)) => {
            let def = types.record(*r);
            let mut fields = Vec::with_capacity(def.fields.len());
            for f in &def.fields {
                fields.push(match t.get(&f.name) {
                    Some(v) => decode_toml(types, &f.ty, v)?,
                    None => super::builtins::default_of(types, &f.ty),
                });
            }
            Value::record(*r, fields)
        }
        _ => return Err(want()),
    })
}

fn number(p: Prim, n: f64) -> Option<Value> {
    let unit = match p {
        Prim::Float => Num::Float,
        Prim::Length => Num::Px,
        Prim::Percent => Num::Percent,
        Prim::Angle => Num::Deg,
        // A plain number of seconds.
        Prim::Duration => return Some(Value::Num(n * 1000.0, Num::Ms)),
        _ => return None,
    };
    n.is_finite().then_some(Value::Num(n, unit))
}

/// `"200ms"`, `"6s"`, `"1.5s"` as a duration value.
fn duration_text(s: &str) -> Option<Value> {
    let s = s.trim();
    let (n, scale) = match s.strip_suffix("ms") {
        Some(n) => (n, 1.0),
        None => (s.strip_suffix('s')?, 1000.0),
    };
    let n: f64 = n.trim().parse().ok()?;
    (n.is_finite() && n >= 0.0).then_some(Value::Num(n * scale, Num::Ms))
}

/// A settings field's value as the TOML item written back.
pub fn encode_item(types: &TypeTable, ty: &Ty, v: &Value) -> toml_edit::Item {
    match encode_toml(types, ty, v) {
        Some(v) => toml_edit::Item::Value(v),
        None => toml_edit::Item::None,
    }
}

fn encode_toml(types: &TypeTable, ty: &Ty, v: &Value) -> Option<toml_edit::Value> {
    let ty = match ty {
        Ty::Optional(t) => t,
        t => t,
    };
    Some(match v {
        Value::Null => return None,
        Value::Bool(b) => (*b).into(),
        Value::Num(n, Num::Int) => (*n as i64).into(),
        Value::Num(n, Num::Ms) => format!("{}ms", super::value::number(*n)).into(),
        Value::Num(n, _) if matches!(ty, Ty::Prim(Prim::Int)) => (*n as i64).into(),
        Value::Num(n, _) => (*n).into(),
        Value::Text(t) => t.to_string().into(),
        Value::Color(c) => super::value::hex(*c).into(),
        Value::Enum(e, i) => types.enum_(*e).variants.get(*i as usize)?.clone().into(),
        Value::List(items) => {
            let elem = match ty {
                Ty::List(t, _) => (**t).clone(),
                _ => Ty::Any,
            };
            let mut a = toml_edit::Array::new();
            for i in items.iter() {
                a.push(encode_toml(types, &elem, i)?);
            }
            a.into()
        }
        Value::Record(r) => {
            let def = types.record(r.ty);
            let mut t = toml_edit::InlineTable::new();
            for (f, fv) in def.fields.iter().zip(&r.fields) {
                if let Some(v) = encode_toml(types, &f.ty, fv) {
                    t.insert(&f.name, v);
                }
            }
            t.into()
        }
        _ => return None,
    })
}

/// The JSON form of `v` as a `ty`.
pub fn encode(types: &TypeTable, ty: &Ty, v: &Value) -> Option<Json> {
    Some(match (ty, v) {
        (_, Value::Null) => Json::Null,
        (Ty::Optional(t), v) => encode(types, t, v)?,
        (_, Value::Bool(b)) => Json::Bool(*b),
        (_, Value::Num(n, _)) => serde_json::Number::from_f64(*n).map(Json::Number)?,
        (_, Value::Text(t)) => Json::String(t.to_string()),
        (_, Value::Color(c)) => Json::String(super::value::hex(*c)),
        (_, Value::Enum(e, i)) => Json::String(types.enum_(*e).variants.get(*i as usize)?.clone()),
        (Ty::Record(r), Value::Record(rec)) => {
            let def = types.record(*r);
            let mut m = Map::new();
            for (f, fv) in def.fields.iter().zip(&rec.fields) {
                m.insert(f.name.clone(), encode(types, &f.ty, fv)?);
            }
            Json::Object(m)
        }
        (Ty::List(t, _), Value::List(items)) => Json::Array(
            items
                .iter()
                .map(|i| encode(types, t, i))
                .collect::<Option<_>>()?,
        ),
        _ => return None,
    })
}

/// The value JSON `j` holds as a `ty`; `None` if it does not fit.
pub fn decode(types: &TypeTable, ty: &Ty, j: &Json) -> Option<Value> {
    Some(match (ty, j) {
        (Ty::Optional(_), Json::Null) => Value::Null,
        (Ty::Optional(t), j) => decode(types, t, j)?,
        (Ty::Prim(p), j) => match (p, j) {
            (Prim::Bool, Json::Bool(b)) => Value::Bool(*b),
            (Prim::Text | Prim::Path, Json::String(s)) => Value::text(s.as_str()),
            (Prim::Color | Prim::Paint, Json::String(s)) => Value::Color(Color::from_hex(s)?),
            (p, Json::Number(n)) => {
                let n = n.as_f64()?;
                let unit = match p {
                    Prim::Int => Num::Int,
                    Prim::Length => Num::Px,
                    Prim::Percent => Num::Percent,
                    Prim::Angle => Num::Deg,
                    Prim::Duration => Num::Ms,
                    Prim::Float => Num::Float,
                    _ => return None,
                };
                if unit == Num::Int && n.fract() != 0.0 {
                    return None;
                }
                Value::Num(n, unit)
            }
            _ => return None,
        },
        (Ty::Enum(e), Json::String(s)) => Value::Enum(*e, types.enum_(*e).variant(s)?),
        (Ty::Record(r), Json::Object(m)) => {
            let def = types.record(*r);
            let fields = def
                .fields
                .iter()
                .map(|f| decode(types, &f.ty, m.get(&f.name).unwrap_or(&Json::Null)))
                .collect::<Option<Vec<_>>>()?;
            Value::record(*r, fields)
        }
        (Ty::List(t, _), Json::Array(items)) => Value::list(
            items
                .iter()
                .map(|i| decode(types, t, i))
                .collect::<Option<_>>()?,
        ),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types() -> TypeTable {
        crate::schema::Schema::builtin().types.clone()
    }

    #[test]
    fn round_trips_by_type() {
        let t = types();
        let urgency =
            crate::ty::EnumId(t.enums.iter().position(|e| e.name == "Urgency").unwrap() as u32);
        let ty = Ty::Enum(urgency);
        let v = Value::Enum(urgency, 2);
        let j = encode(&t, &ty, &v).unwrap();
        assert_eq!(j, Json::String("critical".into()));
        assert_eq!(decode_bytes(&t, &ty, &encode_bytes(&t, &ty, &v)), Some(v));
        let list = Ty::list(Ty::INT);
        let v = Value::list(vec![Value::int(1), Value::int(2)]);
        assert_eq!(
            decode_bytes(&t, &list, &encode_bytes(&t, &list, &v)),
            Some(v)
        );
        assert_eq!(decode(&t, &Ty::INT, &serde_json::json!(1.5)), None);
        // Bytes that no longer fit the type: nothing (core uses the
        // default).
        assert_eq!(decode_bytes(&t, &Ty::BOOL, b"\"x\""), None);
        assert_eq!(decode_bytes(&t, &Ty::BOOL, b"not json"), None);
    }

    #[test]
    fn settings_items_round_trip_and_explain() {
        let t = types();
        let item = |s: &str| {
            let doc: toml_edit::DocumentMut = format!("x = {s}").parse().unwrap();
            doc["x"].clone()
        };
        assert_eq!(
            decode_item(&t, &Ty::COLOR, &item("\"#7aa2f7\"")),
            Ok(Value::Color(Color::from_hex("#7aa2f7").unwrap()))
        );
        assert_eq!(
            decode_item(&t, &Ty::BOOL, &item("true")),
            Ok(Value::Bool(true))
        );
        assert_eq!(
            decode_item(&t, &Ty::DURATION, &item("\"200ms\"")),
            Ok(Value::Num(200.0, Num::Ms))
        );
        assert!(decode_item(&t, &Ty::BOOL, &item("3")).is_err());
        assert!(
            decode_item(&t, &Ty::COLOR, &item("\"blue\""))
                .unwrap_err()
                .contains("colour")
        );
        for (ty, v) in [
            (Ty::BOOL, Value::Bool(true)),
            (Ty::INT, Value::int(3)),
            (Ty::FLOAT, Value::float(0.5)),
            (Ty::COLOR, Value::Color(Color::from_hex("#112233").unwrap())),
            (Ty::DURATION, Value::Num(1500.0, Num::Ms)),
            (Ty::TEXT, Value::text("hi")),
        ] {
            let back = decode_item(&t, &ty, &encode_item(&t, &ty, &v));
            assert_eq!(back, Ok(v), "{}", t.show(&ty));
        }
    }
}
