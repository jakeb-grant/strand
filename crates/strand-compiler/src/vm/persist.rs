//! `persist`: state kept across restarts, stored with a hash of its
//! default.
//!
//! Each persisted cell is stored under its path (`toasts.dnd`, or
//! `Component.name` for a component's state) as JSON shaped by its
//! declared type (enums by variant name, records by field name), next to
//! the BLAKE3 hash of the default it was declared with. On load:
//!
//! - nothing stored: the default;
//! - stored under the same default: the stored value;
//! - the default changed and the stored value is the old default (never
//!   changed by the user): the new default is adopted;
//! - the default changed and the user had changed the value: it is kept,
//!   and [`Restored::kept`] says so (`launcher.query: kept "fir" (default
//!   changed)`);
//! - a value that no longer fits the type (a renamed variant, a changed
//!   type): the default.
//!
//! strand-core has no persistence of its own, so the store lives here:
//! [`MemoryStore`] for tests and [`FileStore`], one JSON file written by
//! temp file and rename.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value as Json};
use strand_scene::Color;

use super::value::{Num, Value};
use crate::ty::{Prim, Ty, TypeTable};

/// One stored cell.
#[derive(Clone, Debug, PartialEq)]
pub struct Stored {
    /// BLAKE3 (hex) of the default's encoding when the value was stored.
    pub default_hash: String,
    pub value: Json,
}

/// Where persisted cells live.
pub trait PersistStore {
    fn load(&self, key: &str) -> Option<Stored>;
    fn save(&self, key: &str, stored: Stored);
}

/// An in-memory store (tests, `strand reload --hard` simulations).
#[derive(Debug, Default)]
pub struct MemoryStore {
    cells: RefCell<BTreeMap<String, Stored>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn keys(&self) -> Vec<String> {
        self.cells.borrow().keys().cloned().collect()
    }
}

impl PersistStore for MemoryStore {
    fn load(&self, key: &str) -> Option<Stored> {
        self.cells.borrow().get(key).cloned()
    }

    fn save(&self, key: &str, stored: Stored) {
        self.cells.borrow_mut().insert(key.to_string(), stored);
    }
}

/// A JSON file holding every cell: `{ "toasts.dnd": { "default":
/// "…hash…", "value": false } }`. Writes replace the file through a
/// temp file and a rename, so a crash never leaves half a file.
#[derive(Debug)]
pub struct FileStore {
    path: PathBuf,
    cells: RefCell<BTreeMap<String, Stored>>,
    /// The last write error, for the overlay.
    pub error: RefCell<Option<String>>,
}

impl FileStore {
    /// Opens (or starts) the store at `path`. A missing or unreadable
    /// file starts empty.
    pub fn open(path: impl Into<PathBuf>) -> FileStore {
        let path = path.into();
        let cells = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Json>(&t).ok())
            .and_then(|j| match j {
                Json::Object(m) => Some(m),
                _ => None,
            })
            .map(|m| {
                m.into_iter()
                    .filter_map(|(k, v)| {
                        let default_hash = v.get("default")?.as_str()?.to_string();
                        let value = v.get("value")?.clone();
                        Some((
                            k,
                            Stored {
                                default_hash,
                                value,
                            },
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        FileStore {
            path,
            cells: RefCell::new(cells),
            error: RefCell::new(None),
        }
    }

    /// `$XDG_STATE_HOME/strand/persist.json` (or `~/.local/state/…`).
    pub fn default_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME").map(|h| Path::new(&h).join(".local").join("state"))
            })?;
        Some(base.join("strand").join("persist.json"))
    }

    fn write(&self) -> io::Result<()> {
        let mut m = Map::new();
        for (k, s) in self.cells.borrow().iter() {
            let mut cell = Map::new();
            cell.insert("default".into(), Json::String(s.default_hash.clone()));
            cell.insert("value".into(), s.value.clone());
            m.insert(k.clone(), Json::Object(cell));
        }
        let text = serde_json::to_string_pretty(&Json::Object(m)).map_err(io::Error::other)?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &self.path)
    }
}

impl PersistStore for FileStore {
    fn load(&self, key: &str) -> Option<Stored> {
        self.cells.borrow().get(key).cloned()
    }

    fn save(&self, key: &str, stored: Stored) {
        self.cells.borrow_mut().insert(key.to_string(), stored);
        *self.error.borrow_mut() = self.write().err().map(|e| e.to_string());
    }
}

/// What loading a persisted cell gave.
#[derive(Clone, Debug, PartialEq)]
pub struct Restored {
    pub value: Value,
    /// The default changed but the user's value was kept.
    pub kept: bool,
}

/// The hash of a default's encoding.
pub fn default_hash(json: &Json) -> String {
    blake3::hash(json.to_string().as_bytes())
        .to_hex()
        .to_string()
}

/// Loads cell `key` of type `ty` declared with `default`.
pub fn restore(
    store: &dyn PersistStore,
    key: &str,
    types: &TypeTable,
    ty: &Ty,
    default: &Value,
) -> Restored {
    let fresh = Restored {
        value: default.clone(),
        kept: false,
    };
    let Some(stored) = store.load(key) else {
        return fresh;
    };
    let Some(def_json) = encode(types, ty, default) else {
        return fresh;
    };
    let hash = default_hash(&def_json);
    let changed = stored.default_hash != hash;
    if changed && default_hash(&stored.value) == stored.default_hash {
        // Still the old default: take the new one.
        return fresh;
    }
    match decode(types, ty, &stored.value) {
        Some(v) => Restored {
            kept: changed && v != *default,
            value: v,
        },
        None => fresh,
    }
}

/// Stores cell `key`.
pub fn save(
    store: &dyn PersistStore,
    key: &str,
    types: &TypeTable,
    ty: &Ty,
    default: &Value,
    value: &Value,
) {
    let (Some(d), Some(v)) = (encode(types, ty, default), encode(types, ty, value)) else {
        return;
    };
    store.save(
        key,
        Stored {
            default_hash: default_hash(&d),
            value: v,
        },
    );
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
        assert_eq!(decode(&t, &ty, &j), Some(v));
        let list = Ty::list(Ty::INT);
        let v = Value::list(vec![Value::int(1), Value::int(2)]);
        assert_eq!(decode(&t, &list, &encode(&t, &list, &v).unwrap()), Some(v));
        assert_eq!(decode(&t, &Ty::INT, &serde_json::json!(1.5)), None);
    }

    #[test]
    fn a_changed_default_is_adopted_only_if_never_changed() {
        let t = types();
        let store = MemoryStore::new();
        let ty = Ty::TEXT;
        // Stored at the old default "a".
        save(&store, "x", &t, &ty, &Value::text("a"), &Value::text("a"));
        let r = restore(&store, "x", &t, &ty, &Value::text("b"));
        assert_eq!(
            r,
            Restored {
                value: Value::text("b"),
                kept: false
            }
        );
        // Changed by the user to "z" under default "a": kept, and said so.
        save(&store, "x", &t, &ty, &Value::text("a"), &Value::text("z"));
        let r = restore(&store, "x", &t, &ty, &Value::text("b"));
        assert_eq!(
            r,
            Restored {
                value: Value::text("z"),
                kept: true
            }
        );
        // Same default: the stored value.
        let r = restore(&store, "x", &t, &ty, &Value::text("a"));
        assert_eq!(
            r,
            Restored {
                value: Value::text("z"),
                kept: false
            }
        );
        // A value that no longer fits the type: the default.
        let r = restore(&store, "x", &t, &Ty::BOOL, &Value::Bool(true));
        assert_eq!(r.value, Value::Bool(true));
    }

    #[test]
    fn file_store_survives_a_reopen() {
        let dir = std::env::temp_dir().join(format!("strand-persist-{}", std::process::id()));
        let path = dir.join("persist.json");
        let t = types();
        {
            let s = FileStore::open(&path);
            save(
                &s,
                "toasts.dnd",
                &t,
                &Ty::BOOL,
                &Value::Bool(false),
                &Value::Bool(true),
            );
            assert!(s.error.borrow().is_none());
        }
        let s = FileStore::open(&path);
        let r = restore(&s, "toasts.dnd", &t, &Ty::BOOL, &Value::Bool(false));
        assert_eq!(r.value, Value::Bool(true));
        let _ = std::fs::remove_dir_all(dir);
    }
}
