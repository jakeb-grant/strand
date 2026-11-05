//! (report) The VM's values as a glitch report's JSON (docs/architecture.md,
//! "Glitch reports", Values): [`to_json`] writes a [`Value`] by the type
//! it carries (records as objects of their fields, lists as arrays, enums
//! by variant name, durations in ms, colours as `"#rrggbbaa"`, (dictionary)
//! a dictionary as an array of `[key, value]` pairs in key order) straight
//! into any writer, so the ring's bounded arena writer allocates nothing;
//! [`from_json`] reads it back by a declared type (the replay's `set` and
//! `[seed]`). A text held back by the redaction rules is `{"~text": N}`;
//! read back, it is N placeholder `x`s, as the replay types them.

use std::fmt;

use serde_json::Value as Json;
use strand_core::flight::{HELD_TEXT, json_color, json_held, json_number, json_text};

use super::value::Value;
use crate::ty::{Ty, TypeTable};

/// The character a held-back text is read back as, N times.
pub const PLACEHOLDER: char = 'x';

/// Writes `v` as compact JSON. `keep` decides for each text whether it is
/// written; one it refuses is written `{"~text": N}`, N its characters.
/// What has no JSON form (a function, a node, a token or time
/// expression) is written `null`.
pub fn to_json(
    types: &TypeTable,
    v: &Value,
    w: &mut dyn fmt::Write,
    keep: &mut dyn FnMut(&str) -> bool,
) -> fmt::Result {
    match v {
        Value::Bool(b) => w.write_str(if *b { "true" } else { "false" }),
        Value::Num(n, _) => json_number(w, *n),
        Value::Text(t) => {
            if keep(t) {
                json_text(w, t)
            } else {
                json_held(w, t)
            }
        }
        Value::Color(c) => {
            let [r, g, b, a] = c.to_rgba8();
            let f = |x: u8| f32::from(x) / 255.0;
            json_color(w, [f(r), f(g), f(b), f(a)])
        }
        Value::Enum(e, i) => match types
            .enums
            .get(e.0 as usize)
            .and_then(|d| d.variants.get(*i as usize))
        {
            Some(name) => json_text(w, name),
            None => w.write_str("null"),
        },
        Value::Record(r) => {
            let names = types.records.get(r.ty.0 as usize).map(|d| &d.fields);
            w.write_char('{')?;
            for (i, fv) in r.fields.iter().enumerate() {
                if i > 0 {
                    w.write_char(',')?;
                }
                match names.and_then(|f| f.get(i)) {
                    Some(f) => json_text(w, &f.name)?,
                    None => json_text(w, &i.to_string())?,
                }
                w.write_char(':')?;
                to_json(types, fv, w, keep)?;
            }
            w.write_char('}')
        }
        Value::List(items) | Value::Commas(items) | Value::Spaced(items) => {
            w.write_char('[')?;
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    w.write_char(',')?;
                }
                to_json(types, item, w, keep)?;
            }
            w.write_char(']')
        }
        // (dictionary) Pairs, as `persist` stores them: a key is written
        // as any value is, so a text key `keep` refuses is held back too.
        Value::Dict(d) => {
            w.write_char('[')?;
            for (i, (k, x)) in d.entries().iter().enumerate() {
                if i > 0 {
                    w.write_char(',')?;
                }
                w.write_char('[')?;
                to_json(types, k, w, keep)?;
                w.write_char(',')?;
                to_json(types, x, w, keep)?;
                w.write_char(']')?;
            }
            w.write_char(']')
        }
        _ => w.write_str("null"),
    }
}

/// [`to_json`] into a string, every text kept.
pub fn to_json_string(types: &TypeTable, v: &Value) -> String {
    let mut s = String::new();
    let _ = to_json(types, v, &mut s, &mut |_| true);
    s
}

/// The value `j` holds as a `ty`, or why it does not fit. A held-back
/// text (`{"~text": N}`) reads as N [`PLACEHOLDER`]s wherever a text is
/// expected.
pub fn from_json(types: &TypeTable, ty: &Ty, j: &Json) -> Result<Value, String> {
    let j = unhold(j);
    super::persist::decode(types, ty, &j).ok_or_else(|| format!("expected {}", types.show(ty)))
}

/// (dictionary) How many entries of `j`, read back as a `ty`, are lost
/// because two of a dictionary's keys read back as one: two held-back
/// texts of the same length are the same placeholder `x`s, and of the
/// two [`from_json`] keeps the later. A replay names the cell in its
/// fidelity notes (docs/eval.md). 0 when nothing is lost, or `j` does not
/// fit `ty`.
pub fn keys_read_as_one(types: &TypeTable, ty: &Ty, j: &Json) -> usize {
    match (ty, j) {
        (Ty::Optional(t), j) => keys_read_as_one(types, t, j),
        (Ty::List(t, _), Json::Array(items)) => {
            items.iter().map(|i| keys_read_as_one(types, t, i)).sum()
        }
        (Ty::Record(r), Json::Object(m)) => types
            .record(*r)
            .fields
            .iter()
            .filter_map(|f| Some(keys_read_as_one(types, &f.ty, m.get(&f.name)?)))
            .sum(),
        (Ty::Dict(k, t), Json::Array(pairs)) => {
            let mut keys = super::value::Dict::default();
            let mut lost = 0;
            for p in pairs {
                let Some([key, value]) = p.as_array().map(Vec::as_slice) else {
                    continue;
                };
                lost += keys_read_as_one(types, t, value);
                // `to_json` writes each key once, so two that read back
                // as one are a held-back text and another of its length.
                if let Ok(kv) = from_json(types, k, key) {
                    if keys.contains(&kv) {
                        lost += 1;
                    } else {
                        keys.insert(kv, Value::Null);
                    }
                }
            }
            lost
        }
        _ => 0,
    }
}

/// Whether `j` is a held-back text, and its length.
pub fn held_len(j: &Json) -> Option<usize> {
    let m = j.as_object()?;
    if m.len() != 1 {
        return None;
    }
    m.get(HELD_TEXT)?.as_u64().map(|n| n as usize)
}

/// `j` with each held-back text replaced by its placeholders.
fn unhold(j: &Json) -> Json {
    if let Some(n) = held_len(j) {
        return Json::String(std::iter::repeat_n(PLACEHOLDER, n).collect());
    }
    match j {
        Json::Array(a) => Json::Array(a.iter().map(unhold).collect()),
        Json::Object(m) => Json::Object(m.iter().map(|(k, v)| (k.clone(), unhold(v))).collect()),
        other => other.clone(),
    }
}
