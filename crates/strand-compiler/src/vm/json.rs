//! (report) The VM's values as a glitch report's JSON (docs/architecture.md,
//! "Glitch reports", Values): [`to_json`] writes a [`Value`] by the type
//! it carries (records as objects of their fields, lists as arrays, enums
//! by variant name, durations in ms, colours as `"#rrggbbaa"`) straight
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
