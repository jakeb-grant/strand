//! [`Data`] (what `strand-services` speaks) to and from the VM's
//! [`Value`], by the schema's names: records by type and field name,
//! enums by variant name. Only the language side knows both.

use std::rc::Rc;

use strand_compiler::ty::{EnumId, TypeTable};
use strand_compiler::vm::host::PathSeg;
use strand_compiler::vm::schema_host::default_value;
use strand_compiler::vm::{Num, Value};
use strand_core::Error;
use strand_scene::Color;
use strand_services::{Data, Rgba, Step};

/// `d` as a value of `types`. A record or enum the table does not know
/// becomes null (the schema and the store disagree: a test holds them to
/// each other); a record field the data lacks takes its type's default.
pub fn to_value(types: &TypeTable, d: &Data) -> Value {
    match d {
        Data::Null => Value::Null,
        Data::Bool(b) => Value::Bool(*b),
        Data::Int(n) => Value::int(*n),
        Data::Float(n) => Value::float(*n),
        Data::Text(t) => Value::Text(Rc::from(&**t)),
        Data::Duration(t) => Value::from(*t),
        Data::NegDuration(t) => Value::Num(-t.as_secs_f64() * 1000.0, Num::Ms),
        Data::Color(c) => Value::Color(Color::new(c.r, c.g, c.b, c.a)),
        Data::List(items) => Value::list(items.iter().map(|d| to_value(types, d)).collect()),
        Data::Record { ty, fields } => {
            // The schema's record: a config's own `type App` hides it by
            // name only in the config's code. A name only the config
            // declares (a document read as `any`) is the config's.
            let Some(r) = types
                .find_schema_record(ty)
                .or_else(|| types.find_record(ty))
            else {
                return Value::Null;
            };
            let def = types.record(r);
            let values = def
                .fields
                .iter()
                .map(|f| {
                    fields
                        .iter()
                        .find(|(n, _)| *n == f.name)
                        .map_or_else(|| default_value(types, &f.ty), |(_, d)| to_value(types, d))
                })
                .collect();
            Value::record(r, values)
        }
        Data::Enum { ty, variant } => types
            .enums
            .iter()
            .position(|e| e.name == **ty)
            .and_then(|i| {
                let e = EnumId(i as u32);
                types.enum_(e).variant(variant).map(|v| Value::Enum(e, v))
            })
            .unwrap_or(Value::Null),
    }
}

/// `v` (a value of `types`) as [`Data`]. Values with no meaning outside
/// the program (closures, nodes, tokens) become null.
pub fn to_data(types: &TypeTable, v: &Value) -> Data {
    match v {
        Value::Null | Value::Unit => Data::Null,
        Value::Bool(b) => Data::Bool(*b),
        Value::Num(n, Num::Int) => Data::Int(*n as i64),
        // (player-raise-seek) A duration below zero keeps its sign
        // (`media.seek(by: -15s)`); one no `Duration` holds is zero.
        Value::Num(ms, Num::Ms) => {
            let size =
                std::time::Duration::try_from_secs_f64(ms.abs() / 1000.0).unwrap_or_default();
            if *ms < 0.0 && !size.is_zero() {
                Data::NegDuration(size)
            } else {
                Data::Duration(size)
            }
        }
        Value::Num(n, Num::Percent) => Data::Float(n / 100.0),
        Value::Num(n, _) => Data::Float(*n),
        Value::Text(t) => Data::text(&**t),
        Value::Color(c) => Data::Color(Rgba {
            r: c.r,
            g: c.g,
            b: c.b,
            a: c.a,
        }),
        Value::List(items) | Value::Commas(items) | Value::Spaced(items) => {
            Data::List(items.iter().map(|v| to_data(types, v)).collect())
        }
        Value::Record(r) => {
            let Some(def) = types.records.get(r.ty.0 as usize) else {
                return Data::Null;
            };
            Data::Record {
                ty: def.name.clone().into(),
                fields: def
                    .fields
                    .iter()
                    .zip(&r.fields)
                    .map(|(f, v)| (f.name.clone().into(), to_data(types, v)))
                    .collect(),
            }
        }
        Value::Enum(e, i) => {
            let Some(def) = types.enums.get(e.0 as usize) else {
                return Data::Null;
            };
            match def.variants.get(*i as usize) {
                Some(name) => Data::Enum {
                    ty: def.name.clone().into(),
                    variant: name.clone().into(),
                },
                None => Data::Null,
            }
        }
        _ => Data::Null,
    }
}

/// (dictionary) What a service is told when it would be handed a
/// dictionary: [`Data`] has no map until D-Bus and JSON maps land
/// (decisions.md, dictionary, phase 2).
pub const DICT_NOT_YET: &str = "a service cannot take a dictionary yet: pass a list of a \
     `type` you declare (`type Pair { key: text; value: int }`)";

/// (dictionary) [`to_data`] for what goes to a service (a field write, an
/// action's argument or item, a method's argument): a value holding a
/// dictionary anywhere is the typed "not yet" error, never a silent
/// null. The checker refuses one where it can see it
/// (`check::dict_service`); this holds where it cannot.
pub fn to_service_data(types: &TypeTable, v: &Value) -> Result<Data, Error> {
    if holds_dict(v) {
        return Err(Error::failed(DICT_NOT_YET));
    }
    Ok(to_data(types, v))
}

fn holds_dict(v: &Value) -> bool {
    match v {
        Value::Dict(_) => true,
        Value::List(items) | Value::Commas(items) | Value::Spaced(items) => {
            items.iter().any(holds_dict)
        }
        Value::Record(r) => r.fields.iter().any(holds_dict),
        _ => false,
    }
}

/// A written path below a service field, as services see it.
pub fn steps(path: &[PathSeg]) -> Vec<Step> {
    path.iter()
        .map(|p| match p {
            PathSeg::Field(n) => Step::Field(n.clone()),
            PathSeg::Index(i) => Step::Index(*i),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use strand_compiler::schema::Schema;

    #[test]
    fn values_and_data_convert_both_ways() {
        let types = &Schema::builtin().types;
        for v in [
            Value::Null,
            Value::Bool(true),
            Value::int(4),
            Value::float(0.25),
            Value::text("hi"),
            Value::from(std::time::Duration::from_millis(1500)),
            Value::Num(-15_000.0, Num::Ms),
            Value::Color(Color::new(0.5, 0.25, 1.0, 1.0)),
            Value::list(vec![Value::float(0.1), Value::float(0.2)]),
        ] {
            assert_eq!(to_value(types, &to_data(types, &v)), v, "{v:?}");
        }
        // (player-raise-seek) A duration below zero keeps its sign, as
        // `media.seek(by: -15s)` needs; one no duration holds is zero.
        assert_eq!(
            to_data(types, &Value::Num(-15_000.0, Num::Ms)),
            Data::NegDuration(std::time::Duration::from_secs(15))
        );
        assert_eq!(
            to_data(types, &Value::Num(f64::NAN, Num::Ms)),
            Data::Duration(std::time::Duration::ZERO)
        );
        // A record by field names; a missing field takes its default.
        let d = Data::Record {
            ty: "Range".into(),
            fields: vec![("end".into(), Data::Int(3))],
        };
        let v = to_value(types, &d);
        assert_eq!(v.field(types, "end"), Some(&Value::int(3)));
        assert_eq!(v.field(types, "start"), Some(&Value::int(0)));
        assert_eq!(
            to_data(types, &v),
            Data::Record {
                ty: "Range".into(),
                fields: vec![("start".into(), Data::Int(0)), ("end".into(), Data::Int(3))],
            }
        );
        // Enums by variant name.
        let crit = Data::Enum {
            ty: "Urgency".into(),
            variant: "critical".into(),
        };
        assert_eq!(to_data(types, &to_value(types, &crit)), crit);
        // Unknown names are null, not a panic.
        let unknown = Data::Record {
            ty: "Nope".into(),
            fields: vec![],
        };
        assert_eq!(to_value(types, &unknown), Value::Null);
        // (dictionary) A dictionary, or a value holding one, is the typed
        // "not yet" error for a service; anything else converts.
        let dict = Value::Dict(Rc::new(strand_compiler::vm::Dict::from_entries(vec![(
            Value::int(1),
            Value::text("a"),
        )])));
        for v in [
            dict.clone(),
            Value::list(vec![Value::int(1), dict.clone()]),
            Value::record(
                types.find_record("Range").unwrap(),
                vec![Value::int(0), dict],
            ),
        ] {
            assert_eq!(
                to_service_data(types, &v).unwrap_err().to_string(),
                DICT_NOT_YET,
                "{v:?}"
            );
        }
        assert_eq!(
            to_service_data(types, &Value::int(4)).unwrap(),
            Data::Int(4)
        );
        assert_eq!(
            steps(&[PathSeg::Field("sink".into()), PathSeg::Index(2)]),
            [Step::Field("sink".into()), Step::Index(2)]
        );
    }
}
