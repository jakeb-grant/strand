//! The VM's codecs for strand-core's storage: `persist` cells and
//! settings files.
//!
//! strand-core stores persisted cells (`rt.persisted`, one file per cell
//! path, off-thread atomic writes, the default's hash, `redeclare` and
//! `reset`) and settings files (`rt.settings_file`, `toml_edit`
//! write-back, per-field validation, overlays). Values are the VM's
//! [`Value`]s, so the codecs live here: persisted values are JSON shaped by
//! the declared type ([`encode_bytes`], [`decode_bytes`]: enums by variant
//! name, records by field name, (dictionary) a dictionary an array of
//! `[key, value]` pairs in key order), settings fields TOML items
//! ([`decode_item`], [`encode_item`]).

use serde_json::{Map, Value as Json};
use strand_core::settings::toml_edit;
use strand_scene::Color;

use super::value::{Dict, Dropped, Num, Value};
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
    decode_bytes_dropping(types, ty, bytes).map(|(v, _)| v)
}

/// (dictionary) [`decode_bytes`], with the dictionary entries it dropped
/// because their key no longer names anything (an enum key whose variant
/// the program no longer declares), which the mount notes.
pub fn decode_bytes_dropping(types: &TypeTable, ty: &Ty, bytes: &[u8]) -> Option<(Value, Dropped)> {
    let j: Json = serde_json::from_slice(bytes).ok()?;
    let mut dropped = Dropped::default();
    let v = decode_in(types, ty, &j, &mut dropped)?;
    Some((v, dropped))
}

/// A settings field's TOML item as a `ty`, or why it does not fit.
/// (dictionary) A dictionary is a table: a `[name]` section or an inline
/// `{ … }`, each key read as `K` (one bad entry fails the field, the
/// message naming its key).
/// (settings-tables) A record is a table too, a `[name]` section or an
/// inline `{ … }`, and a list of records an array of inline tables or an
/// array of tables, `[[name]]`, each wherever the type puts one (a
/// section's own keys read the same way). A key the record does not
/// have fails the field, as a bad value does, the message naming where
/// it is (`mons[0]: unknown field "scael"`).
pub fn decode_item(types: &TypeTable, ty: &Ty, item: &toml_edit::Item) -> Result<Value, String> {
    match (ty.non_null(), item) {
        (Ty::Dict(k, v), toml_edit::Item::Table(t)) => decode_entries(
            types,
            k,
            t.iter().map(|(key, i)| (key, decode_item(types, v, i))),
        ),
        (Ty::Record(r), toml_edit::Item::Table(t)) => decode_record(types, *r, t),
        (Ty::List(e, _), toml_edit::Item::ArrayOfTables(a)) => Ok(Value::list(
            a.iter()
                .enumerate()
                .map(|(i, t)| {
                    match e.non_null() {
                        Ty::Record(r) => decode_record(types, *r, t),
                        Ty::Dict(k, v) => decode_entries(
                            types,
                            k,
                            t.iter().map(|(key, i)| (key, decode_item(types, v, i))),
                        ),
                        _ => Err(format!("expected {}", types.show(e))),
                    }
                    .map_err(|m| at_index(i, m))
                })
                .collect::<Result<_, _>>()?,
        )),
        _ => match item.as_value() {
            Some(v) => decode_toml(types, ty, v),
            None if matches!(ty, Ty::Optional(_)) => Ok(Value::Null),
            None => Err(format!("expected {}", types.show(ty))),
        },
    }
}

/// (settings-tables) A record's fields from a table or an inline table:
/// a field it leaves out is the field's default, a key no field has
/// fails, and so does a field that does not fit, prefixed with its name.
fn decode_record(
    types: &TypeTable,
    r: crate::ty::RecordId,
    t: &dyn toml_edit::TableLike,
) -> Result<Value, String> {
    let def = types.record(r);
    if let Some((key, _)) = t.iter().find(|(key, _)| def.field(key).is_none()) {
        return Err(format!("unknown field {key:?}"));
    }
    let mut fields = Vec::with_capacity(def.fields.len());
    for f in &def.fields {
        fields.push(match t.get(&f.name) {
            Some(i) => decode_item(types, &f.ty, i).map_err(|m| match m.starts_with('[') {
                true => format!("{}{m}", f.name),
                false => format!("{}: {m}", f.name),
            })?,
            None => super::builtins::default_of(types, &f.ty),
        });
    }
    Ok(Value::record(r, fields))
}

/// An element's error, `[1]: …`.
fn at_index(i: usize, m: String) -> String {
    match m.starts_with('[') {
        true => format!("[{i}]{m}"),
        false => format!("[{i}]: {m}"),
    }
}

/// (dictionary) A TOML table's entries as a dictionary keyed by `k`:
/// each key's text read as a `K` (a decimal `int`, `true` or `false`, an
/// enum's variant by name, a `text` or `path` as it is). A key that does
/// not read, a value that does not fit, or two keys that read as one
/// (`3` and `03`) fail the whole value, naming the key.
fn decode_entries<'a>(
    types: &TypeTable,
    k: &Ty,
    entries: impl Iterator<Item = (&'a str, Result<Value, String>)>,
) -> Result<Value, String> {
    let mut out: Vec<(Value, Value)> = Vec::new();
    let mut texts: Vec<&str> = Vec::new();
    for (text, value) in entries {
        let key = match key_text(types, k, text, &mut Dropped::default()) {
            Some(Some(key)) => key,
            _ => {
                let want = match k {
                    Ty::Enum(e) => format!("one of {}", types.enum_(*e).variants.join(", ")),
                    k => types.show(k).to_string(),
                };
                return Err(format!("key {text:?}: expected {want}"));
            }
        };
        let value = value.map_err(|m| format!("{text:?}: {m}"))?;
        if let Some(i) = out
            .iter()
            .position(|(o, _)| super::value::key_cmp(o, &key).is_eq())
        {
            return Err(format!("key {text:?}: the same key as {:?}", texts[i]));
        }
        out.push((key, value));
        texts.push(text);
    }
    Ok(Value::Dict(std::rc::Rc::new(Dict::from_entries(out))))
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
                .enumerate()
                .map(|(i, x)| decode_toml(types, t, x).map_err(|m| at_index(i, m)))
                .collect::<Result<_, _>>()?,
        ),
        (Ty::Record(r), T::InlineTable(t)) => decode_record(types, *r, t)?,
        // (dictionary) An inline table, `{ "DP-1" = "…" }`.
        (Ty::Dict(k, t), T::InlineTable(table)) => decode_entries(
            types,
            k,
            table.iter().map(|(key, x)| (key, decode_toml(types, t, x))),
        )?,
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
/// (dictionary) A dictionary is a `[name]` section of its entries in key
/// order (strand-core merges it into the table the file has, key by
/// key); one inside a list or a record is an inline table.
pub fn encode_item(types: &TypeTable, ty: &Ty, v: &Value) -> toml_edit::Item {
    if let (Ty::Dict(k, t), Value::Dict(d)) = (ty.non_null(), v) {
        let mut table = toml_edit::Table::new();
        for (key, x) in d.entries() {
            let (Some(key), Some(x)) = (key_string(types, k, key), encode_toml(types, t, x)) else {
                return toml_edit::Item::None;
            };
            table.insert(&key, toml_edit::Item::Value(x));
        }
        return toml_edit::Item::Table(table);
    }
    match encode_toml(types, ty, v) {
        Some(v) => toml_edit::Item::Value(v),
        None => toml_edit::Item::None,
    }
}

/// (dictionary) A dictionary key as a TOML key's text, which
/// [`key_text`] reads back: an `int` in decimal, a `bool` as `true` or
/// `false`, an enum by variant name, a `text` or `path` as it is.
fn key_string(types: &TypeTable, ty: &Ty, k: &Value) -> Option<String> {
    Some(match k {
        Value::Num(n, _) if matches!(ty, Ty::Prim(Prim::Int)) && n.fract() == 0.0 => {
            format!("{}", *n as i64)
        }
        Value::Bool(b) => b.to_string(),
        Value::Text(t) => t.to_string(),
        Value::Enum(e, i) => types.enum_(*e).variants.get(*i as usize)?.clone(),
        _ => return None,
    })
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
        // (dictionary) Inside a list or a record: an inline table.
        Value::Dict(d) => {
            let Ty::Dict(k, vt) = ty else {
                return None;
            };
            let mut t = toml_edit::InlineTable::new();
            for (key, x) in d.entries() {
                t.insert(&key_string(types, k, key)?, encode_toml(types, vt, x)?);
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
        // (dictionary) `[[key, value], …]` in key order: a JSON object's
        // keys are only texts, and a pair keeps an `int` key a number
        // and an enum key its variant's name.
        (Ty::Dict(k, t), Value::Dict(d)) => Json::Array(
            d.entries()
                .iter()
                .map(|(key, x)| {
                    Some(Json::Array(vec![
                        encode_key(types, k, key)?,
                        encode(types, t, x)?,
                    ]))
                })
                .collect::<Option<_>>()?,
        ),
        _ => return None,
    })
}

/// (dictionary) A dictionary key's JSON: an `int` key as a JSON integer
/// (`3`, not `3.0`), any other by its type.
fn encode_key(types: &TypeTable, ty: &Ty, k: &Value) -> Option<Json> {
    match (ty, k) {
        (Ty::Prim(Prim::Int), Value::Num(n, _)) if n.fract() == 0.0 && n.abs() < 9.0e15 => {
            Some(Json::from(*n as i64))
        }
        _ => encode(types, ty, k),
    }
}

/// The value JSON `j` holds as a `ty`; `None` if it does not fit. A
/// dictionary entry whose key no longer names anything is left out
/// ([`decode_bytes_dropping`]).
pub fn decode(types: &TypeTable, ty: &Ty, j: &Json) -> Option<Value> {
    decode_in(types, ty, j, &mut Dropped::default())
}

fn decode_in(types: &TypeTable, ty: &Ty, j: &Json, dropped: &mut Dropped) -> Option<Value> {
    let decode = |ty: &Ty, j: &Json, dropped: &mut Dropped| decode_in(types, ty, j, dropped);
    Some(match (ty, j) {
        (Ty::Optional(_), Json::Null) => Value::Null,
        (Ty::Optional(t), j) => decode(t, j, dropped)?,
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
            // (services-round) A field the object leaves out reads its
            // type's default, as a settings file's does: a value written
            // before its record gained a field (a persisted list of apps,
            // a report's answer from before `App.actions`) still reads.
            let def = types.record(*r);
            let fields = def
                .fields
                .iter()
                .map(|f| match m.get(&f.name) {
                    Some(v) => decode(&f.ty, v, dropped),
                    None => Some(super::builtins::default_of(types, &f.ty)),
                })
                .collect::<Option<Vec<_>>>()?;
            Value::record(*r, fields)
        }
        (Ty::List(t, _), Json::Array(items)) => Value::list(
            items
                .iter()
                .map(|i| decode(t, i, dropped))
                .collect::<Option<_>>()?,
        ),
        // (dictionary) Pairs, as `encode` writes them, in any order: put
        // in key order, and of two with one key the later wins, so a store
        // edited by hand cannot break the order rule.
        (Ty::Dict(k, t), Json::Array(pairs)) => {
            let mut entries = Vec::with_capacity(pairs.len());
            for p in pairs {
                let [key, value] = p.as_array().map(Vec::as_slice)? else {
                    return None;
                };
                let Some(key) = decode_key(types, k, key, dropped)? else {
                    continue;
                };
                entries.push((key, decode(t, value, dropped)?));
            }
            Value::Dict(std::rc::Rc::new(Dict::from_entries(entries)))
        }
        // (dictionary) An object, as a hand-written store or `strand get`
        // has it (`{"3": "0x55"}`): each key's text read as a `K`.
        (Ty::Dict(k, t), Json::Object(m)) => {
            let mut entries = Vec::with_capacity(m.len());
            for (key, value) in m {
                let Some(key) = key_text(types, k, key, dropped)? else {
                    continue;
                };
                entries.push((key, decode(t, value, dropped)?));
            }
            Value::Dict(std::rc::Rc::new(Dict::from_entries(entries)))
        }
        _ => return None,
    })
}

/// (dictionary) A pair's key as a `ty`: `Some(None)` when it is an enum
/// variant the program no longer declares (its entry is dropped and
/// counted), `None` when it does not fit at all (the whole value fails).
fn decode_key(
    types: &TypeTable,
    ty: &Ty,
    j: &Json,
    dropped: &mut Dropped,
) -> Option<Option<Value>> {
    match (ty, j) {
        (Ty::Enum(e), Json::String(s)) => Some(enum_key(types, *e, s, dropped)),
        _ => decode_in(types, ty, j, dropped).map(Some),
    }
}

/// (dictionary) An object's key text as a `ty`: an `int` in decimal, a
/// `bool` as `true` or `false`, an enum by variant name (one no longer
/// declared: `Some(None)`, dropped), a `text` or `path` as it is.
fn key_text(types: &TypeTable, ty: &Ty, s: &str, dropped: &mut Dropped) -> Option<Option<Value>> {
    Some(Some(match ty {
        Ty::Prim(Prim::Int) => Value::int(s.trim().parse::<i64>().ok()?),
        Ty::Prim(Prim::Bool) => Value::Bool(s.parse().ok()?),
        Ty::Prim(Prim::Text | Prim::Path) => Value::text(s),
        Ty::Enum(e) => return Some(enum_key(types, *e, s, dropped)),
        _ => return None,
    }))
}

fn enum_key(
    types: &TypeTable,
    e: crate::ty::EnumId,
    s: &str,
    dropped: &mut Dropped,
) -> Option<Value> {
    let def = types.enum_(e);
    match def.variant(s) {
        Some(i) => Some(Value::Enum(e, i)),
        None => {
            dropped.add(&def.name);
            None
        }
    }
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

    /// (dictionary) A dictionary is stored as `[key, value]` pairs in key
    /// order, an `int` key a JSON integer and an enum key its variant's
    /// name; it reads back from pairs in any order (of two with one key
    /// the later wins) or from an object of key texts. A stored entry
    /// whose enum key the program no longer declares is dropped and
    /// counted; anything else that does not fit refuses the whole value.
    #[test]
    fn dictionaries_store_as_pairs_in_key_order() {
        let t = types();
        let urgency =
            crate::ty::EnumId(t.enums.iter().position(|e| e.name == "Urgency").unwrap() as u32);
        let dict = |entries: Vec<(Value, Value)>| {
            Value::Dict(std::rc::Rc::new(Dict::from_entries(entries)))
        };
        let ty = Ty::dict(Ty::INT, Ty::TEXT);
        let v = dict(vec![
            (Value::int(5), Value::text("0x7a")),
            (Value::float(3.0), Value::text("0x55")),
        ]);
        let bytes = encode_bytes(&t, &ty, &v);
        assert_eq!(
            String::from_utf8_lossy(&bytes),
            r#"[[3,"0x55"],[5,"0x7a"]]"#
        );
        assert_eq!(decode_bytes(&t, &ty, &bytes), Some(v.clone()));
        // By hand: unsorted with a key twice, or an object.
        let read = |s: &str| decode_bytes(&t, &ty, s.as_bytes());
        assert_eq!(
            read(r#"[[5,"old"],[3,"0x55"],[5,"0x7a"]]"#),
            Some(v.clone())
        );
        assert_eq!(read(r#"{"5":"0x7a","3":"0x55"}"#), Some(v.clone()));
        assert_eq!(read("[]"), Some(dict(Vec::new())));
        for bad in [
            r#"[[3]]"#,
            r#"[[3,"a",1]]"#,
            r#"[["3","a"]]"#,
            r#"[[3.5,"a"]]"#,
            r#"[[3,4]]"#,
            r#"{"three":"a"}"#,
            r#"{"3.5":"a"}"#,
            r#""3""#,
        ] {
            assert_eq!(read(bad), None, "{bad}");
        }
        // Enum keys by variant name, in declaration order.
        let by = Ty::dict(Ty::Enum(urgency), Ty::INT);
        let v = dict(vec![
            (Value::Enum(urgency, 2), Value::int(0)),
            (Value::Enum(urgency, 0), Value::int(3)),
        ]);
        let bytes = encode_bytes(&t, &by, &v);
        assert_eq!(
            String::from_utf8_lossy(&bytes),
            r#"[["low",3.0],["critical",0.0]]"#
        );
        assert_eq!(decode_bytes(&t, &by, &bytes), Some(v.clone()));
        let (back, dropped) =
            decode_bytes_dropping(&t, &by, br#"{"critical":0,"urgent":1,"low":3,"gone":2}"#)
                .unwrap();
        assert_eq!(back, v);
        assert_eq!(dropped.entries, 2);
        assert_eq!(
            dropped.notice("toasts.after").as_deref(),
            Some("toasts.after: dropped 2 entries (key no longer in Urgency)")
        );
        let (_, none) = decode_bytes_dropping(&t, &by, &bytes).unwrap();
        assert_eq!(none.notice("toasts.after"), None);
        // Bool keys, and an optional dictionary's null.
        let flags = Ty::dict(Ty::BOOL, Ty::TEXT);
        assert_eq!(
            decode_bytes(&t, &flags, br#"{"true":"on","false":"off"}"#),
            decode_bytes(&t, &flags, br#"[[false,"off"],[true,"on"]]"#)
        );
        assert_eq!(
            decode_bytes(&t, &Ty::Optional(Box::new(ty)), b"null"),
            Some(Value::Null)
        );
    }

    /// (services-round) A record's missing field is its default; a field
    /// that is there and does not fit still refuses the whole value.
    #[test]
    fn a_records_missing_field_reads_its_default() {
        let t = types();
        let app = Ty::Record(t.find_record("App").unwrap());
        let old = serde_json::json!({"id": "firefox", "name": "Firefox", "comment": null,
            "icon": "firefox", "categories": []});
        let v = decode(&t, &app, &old).expect("an App from before `actions`");
        let Value::Record(rec) = &v else {
            panic!("{v:?}")
        };
        let def = t.record(t.find_record("App").unwrap());
        let at = def.fields.iter().position(|f| f.name == "actions").unwrap();
        assert_eq!(rec.fields[at], Value::list(Vec::new()));
        let bad = serde_json::json!({"id": "firefox", "name": 3});
        assert_eq!(decode(&t, &app, &bad), None);
    }

    /// (dictionary) A settings dictionary is written as a section of its
    /// entries in key order (an `int` key in decimal, an enum key its
    /// variant's name, a dotted text key quoted), one inside a list as an
    /// inline table, and reads back from either form; a key or value that
    /// does not read fails the whole value, naming the key.
    #[test]
    fn settings_dictionaries_are_tables() {
        let t = types();
        let urgency =
            crate::ty::EnumId(t.enums.iter().position(|e| e.name == "Urgency").unwrap() as u32);
        let dict = |entries: Vec<(Value, Value)>| {
            Value::Dict(std::rc::Rc::new(Dict::from_entries(entries)))
        };
        let doc_of = |name: &str, item: toml_edit::Item| {
            let mut doc = toml_edit::DocumentMut::new();
            doc.insert(name, item);
            doc.to_string()
        };
        let item = |s: &str, name: &str| {
            let doc: toml_edit::DocumentMut = s.parse().unwrap();
            doc[name].clone()
        };
        let ty = Ty::dict(Ty::TEXT, Ty::FLOAT);
        let v = dict(vec![
            (Value::text("org.gnome.Nautilus"), Value::float(0.5)),
            (Value::text("kitty"), Value::float(0.9)),
        ]);
        let written = doc_of("scale", encode_item(&t, &ty, &v));
        assert_eq!(
            written,
            "[scale]\nkitty = 0.9\n\"org.gnome.Nautilus\" = 0.5\n"
        );
        assert_eq!(
            decode_item(&t, &ty, &item(&written, "scale")),
            Ok(v.clone())
        );
        assert_eq!(
            decode_item(
                &t,
                &ty,
                &item(
                    "scale = { kitty = 0.9, \"org.gnome.Nautilus\" = 0.5 }",
                    "scale"
                )
            ),
            Ok(v.clone())
        );
        let by = Ty::dict(Ty::Enum(urgency), Ty::INT);
        let v = dict(vec![
            (Value::Enum(urgency, 2), Value::int(0)),
            (Value::Enum(urgency, 0), Value::int(3)),
        ]);
        let written = doc_of("after", encode_item(&t, &by, &v));
        assert_eq!(written, "[after]\nlow = 3\ncritical = 0\n");
        assert_eq!(decode_item(&t, &by, &item(&written, "after")), Ok(v));
        let ints = Ty::dict(Ty::INT, Ty::TEXT);
        let v = dict(vec![(Value::float(3.0), Value::text("c"))]);
        let written = doc_of("names", encode_item(&t, &ints, &v));
        assert_eq!(written, "[names]\n3 = \"c\"\n");
        // Nested: a list of dictionaries is inline tables.
        let list = Ty::list(ints.clone());
        let v = Value::list(vec![v]);
        let written = doc_of("all", encode_item(&t, &list, &v));
        assert_eq!(written, "all = [{ 3 = \"c\" }]\n");
        assert_eq!(decode_item(&t, &list, &item(&written, "all")), Ok(v));
        // An empty one is an empty section, not the field left out.
        let empty = dict(Vec::new());
        let written = doc_of("names", encode_item(&t, &ints, &empty));
        assert_eq!(written, "[names]\n");
        assert_eq!(decode_item(&t, &ints, &item(&written, "names")), Ok(empty));
        for (bad, said) in [
            ("names = { three = \"c\" }", "key \"three\": expected int"),
            ("names = { 3 = 4 }", "\"3\": expected text"),
            (
                "[names]\n3 = \"c\"\n\"+3\" = \"d\"",
                "key \"+3\": the same key as \"3\"",
            ),
            ("names = \"3\"", "expected [int: text]"),
        ] {
            assert_eq!(
                decode_item(&t, &ints, &item(bad, "names")),
                Err(said.to_string()),
                "{bad}"
            );
        }
        assert_eq!(
            decode_item(&t, &by, &item("after = { urgent = 1 }", "after")),
            Err("key \"urgent\": expected one of low, normal, critical".to_string())
        );
    }

    /// (settings-tables) `type Mon { id: text; scale: float; mode: Mode? }`,
    /// `type Mode { width: int; height: int }` and `type Profile { mons:
    /// [Mon]; lid: text?; order: [text] }`, as a config declares them.
    fn profile_types() -> (TypeTable, Ty, Ty) {
        let mut t = types();
        let field = |name: &str, ty: Ty| crate::ty::FieldDef {
            name: name.into(),
            ty,
            rw: false,
        };
        let mut mode = crate::ty::RecordDef::new("Mode", crate::ty::Origin::Schema);
        mode.fields = vec![field("width", Ty::INT), field("height", Ty::INT)];
        let mode = t.add_record(mode);
        let mut mon = crate::ty::RecordDef::new("Mon", crate::ty::Origin::Schema);
        mon.fields = vec![
            field("id", Ty::TEXT),
            field("scale", Ty::FLOAT),
            field("mode", Ty::Record(mode).optional()),
        ];
        let mon = Ty::Record(t.add_record(mon));
        let mut profile = crate::ty::RecordDef::new("Profile", crate::ty::Origin::Schema);
        profile.fields = vec![
            field("mons", Ty::list(mon.clone())),
            field("lid", Ty::TEXT.optional()),
            field("order", Ty::list(Ty::TEXT)),
        ];
        let profile = Ty::Record(t.add_record(profile));
        (t, mon, profile)
    }

    /// (settings-tables) A record reads from a `[section]` and a list of
    /// records from `[[name]]`, wherever the type puts them (a dictionary
    /// of profiles, each with its monitors as an array of tables, a
    /// monitor's optional mode a sub-table), the same as from inline
    /// tables; what is left out is its default. A key the record does not
    /// have fails the value, naming where it is, as a bad value does.
    #[test]
    fn records_read_from_sections_and_arrays_of_tables() {
        let (t, mon, profile) = profile_types();
        let profiles = Ty::dict(Ty::TEXT, profile.clone());
        let item = |s: &str, name: &str| {
            let doc: toml_edit::DocumentMut = s.parse().unwrap();
            doc[name].clone()
        };
        let tables = "\
[profiles.desk]
lid = \"BOE\"
order = [\"DP-1\", \"eDP-1\"]

[[profiles.desk.mons]]
id = \"Dell\"
scale = 1.5
[profiles.desk.mons.mode]
width = 3840
height = 2160

[[profiles.desk.mons]]
id = \"BOE\"
scale = 1

[profiles.couch]
[[profiles.couch.mons]]
id = \"BOE\"
scale = 1.25
";
        let inline = "\
[profiles]
desk = { lid = \"BOE\", order = [\"DP-1\", \"eDP-1\"], mons = [{ id = \"Dell\", scale = 1.5, mode = { width = 3840, height = 2160 } }, { id = \"BOE\", scale = 1.0 }] }
couch = { mons = [{ id = \"BOE\", scale = 1.25 }] }
";
        let from_tables = decode_item(&t, &profiles, &item(tables, "profiles"));
        assert_eq!(
            from_tables,
            decode_item(&t, &profiles, &item(inline, "profiles"))
        );
        let Ok(Value::Dict(d)) = &from_tables else {
            panic!("{from_tables:?}")
        };
        let Some(Value::Record(couch)) = d.get(&Value::text("couch")) else {
            panic!("{d:?}")
        };
        // Left out: `lid` is null, `order` empty, a monitor's `mode` null.
        assert_eq!(couch.fields[1], Value::Null);
        assert_eq!(couch.fields[2], Value::list(Vec::new()));
        let Value::List(mons) = &couch.fields[0] else {
            panic!("{couch:?}")
        };
        let Value::Record(m) = &mons[0] else {
            panic!("{mons:?}")
        };
        assert_eq!(
            m.fields,
            [Value::text("BOE"), Value::float(1.25), Value::Null]
        );
        // A section where a record goes, an array of tables where a list
        // of records goes, at the top.
        assert!(
            decode_item(
                &t,
                &profile,
                &item("[p]\nlid = \"x\"\n[[p.mons]]\nid = \"a\"\n", "p")
            )
            .is_ok()
        );
        let list = Ty::list(mon.clone());
        assert_eq!(
            decode_item(
                &t,
                &list,
                &item("[[m]]\nid = \"a\"\nscale = 2\n[[m]]\nid = \"b\"\n", "m")
            )
            .map(|v| match v {
                Value::List(l) => l.len(),
                _ => 0,
            }),
            Ok(2)
        );
        // An unknown key, or a value that does not fit, names where it is.
        for (bad, said) in [
            (
                tables.replace("scale = 1.25", "scael = 1.25"),
                "\"couch\": mons[0]: unknown field \"scael\"",
            ),
            (
                tables.replace("height = 2160", "height = \"tall\""),
                "\"desk\": mons[0]: mode: height: expected int",
            ),
            (
                tables.replace("lid = \"BOE\"", "lid = \"BOE\"\nenabeld = false"),
                "\"desk\": unknown field \"enabeld\"",
            ),
            (
                inline.replace("scale = 1.0", "scale = 1.0, on = true"),
                "\"desk\": mons[1]: unknown field \"on\"",
            ),
        ] {
            assert_eq!(
                decode_item(&t, &profiles, &item(&bad, "profiles")),
                Err(said.to_string()),
                "{bad}"
            );
        }
        // TOML 1.1's inline table over several lines, with a trailing
        // comma, reads too (toml_edit 0.25 parses TOML 1.1).
        let spread = "m = {\n  id = \"a\",   # the left one\n  scale = 2,\n}\n";
        assert_eq!(
            decode_item(&t, &mon, &item(spread, "m")),
            decode_item(&t, &mon, &item("m = { id = \"a\", scale = 2.0 }", "m"))
        );
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
