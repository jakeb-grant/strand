//! Members of a value: the fields and methods `x.` offers.
//!
//! One table for the checker (which types `x.name` and `x.name(…)` by
//! it), completion after `.`, hover and the generated docs. Records,
//! services and builtin types take their members from the schema; lists
//! and `Async` values, whose members are generic over the item type, are
//! built here for the item type at hand.

use std::sync::Arc;

use super::{DocKey, Schema};
use crate::ty::{FnSig, ParamSig, Prim, Ty, TypeTable};

/// What a member is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberKind {
    /// Read with `x.name`.
    Field,
    /// Called with `x.name(…)`.
    Method,
}

/// One member of a value of some type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberInfo {
    pub name: String,
    pub kind: MemberKind,
    /// A field's type; [`Ty::Unit`] for a method.
    pub ty: Ty,
    /// A method's overloads, in the order they are tried.
    pub sigs: Vec<Arc<FnSig>>,
    /// A field that may be assigned or bound with `<->`; a method that
    /// changes its receiver (`push`) or an action (handlers only).
    pub writes: bool,
    pub doc: Option<String>,
}

impl MemberInfo {
    fn field(name: &str, ty: Ty, doc: &str) -> Self {
        Self {
            name: name.to_string(),
            kind: MemberKind::Field,
            ty,
            sigs: Vec::new(),
            writes: false,
            doc: Some(doc.to_string()),
        }
    }

    fn method(name: &str, sig: FnSig, writes: bool, doc: &str) -> Self {
        Self {
            name: name.to_string(),
            kind: MemberKind::Method,
            ty: Ty::Unit,
            sigs: vec![Arc::new(sig)],
            writes,
            doc: Some(doc.to_string()),
        }
    }
}

/// List methods that transform a loading list into a loading list
/// (`hits.take(3)` is `Async<[Hit]>`).
pub const ASYNC_TRANSFORMS: &[&str] = &["filter", "map", "sort_by", "take", "skip", "reverse"];

fn param(name: &str, ty: Ty) -> ParamSig {
    ParamSig {
        name: name.into(),
        ty,
        has_default: false,
        default: None,
        variadic: false,
    }
}

/// The members of a list of `elem`. `key` is the type of the list's key
/// (`[T] key field`, or a keyed record's), [`Ty::Error`] when it has
/// none. `map`'s result is `[any]` here; the checker types it by the
/// function it is given.
pub fn list_members(elem: &Ty, keyed: bool, key: &Ty) -> Vec<MemberInfo> {
    let t = elem.clone();
    let list = Ty::List(Box::new(t.clone()), keyed);
    let pred = Ty::Fn(Arc::new(FnSig::positional(vec![t.clone()], Ty::BOOL)));
    let any_fn = Ty::Fn(Arc::new(FnSig::positional(vec![t.clone()], Ty::Any)));
    let key = key.clone();
    let m = |name: &str, params: Vec<ParamSig>, ret: Ty, writes: bool, doc: &str| {
        MemberInfo::method(name, FnSig::new(params, ret), writes, doc)
    };
    vec![
        MemberInfo::field("len", Ty::INT, "How many items."),
        MemberInfo::field(
            "first",
            t.clone().optional(),
            "The first item; null if empty.",
        ),
        MemberInfo::field(
            "last",
            t.clone().optional(),
            "The last item; null if empty.",
        ),
        m(
            "filter",
            vec![param("keep", pred.clone())],
            list.clone(),
            false,
            "The items `keep` is true for.",
        ),
        m(
            "map",
            vec![param("f", any_fn.clone())],
            Ty::List(Box::new(Ty::Any), keyed),
            false,
            "Each item through `f`.",
        ),
        m(
            "sort_by",
            vec![param("by", any_fn)],
            list.clone(),
            false,
            "The items ordered by `by`.",
        ),
        m(
            "take",
            vec![param("count", Ty::INT)],
            list.clone(),
            false,
            "The first `count` items.",
        ),
        m(
            "skip",
            vec![param("count", Ty::INT)],
            list.clone(),
            false,
            "All but the first `count` items.",
        ),
        m(
            "reverse",
            vec![],
            list,
            false,
            "The items in reverse order.",
        ),
        m(
            "join",
            vec![param("sep", Ty::TEXT)],
            Ty::TEXT,
            false,
            "The text items joined with `sep`.",
        ),
        m(
            "contains",
            vec![param("item", t.clone())],
            Ty::BOOL,
            false,
            "Whether `item` is in the list.",
        ),
        m(
            "any",
            vec![param("test", pred.clone())],
            Ty::BOOL,
            false,
            "Whether `test` is true for some item.",
        ),
        m(
            "all",
            vec![param("test", pred.clone())],
            Ty::BOOL,
            false,
            "Whether `test` is true for every item.",
        ),
        m(
            "count",
            vec![param("test", pred.clone())],
            Ty::INT,
            false,
            "How many items `test` is true for.",
        ),
        m(
            "find",
            vec![param("test", pred)],
            t.clone().optional(),
            false,
            "The first item `test` is true for, or null.",
        ),
        m(
            "push",
            vec![param("item", t.clone())],
            Ty::Unit,
            true,
            "Adds `item` at the end (handlers only).",
        ),
        m(
            "insert",
            vec![param("at", Ty::INT), param("item", t.clone())],
            Ty::Unit,
            true,
            "Inserts `item` at `at` (handlers only).",
        ),
        m(
            "remove",
            vec![param("at", Ty::INT)],
            Ty::Unit,
            true,
            "Removes the item at `at` (handlers only).",
        ),
        m(
            "clear",
            vec![],
            Ty::Unit,
            true,
            "Removes every item (handlers only).",
        ),
        m(
            "remove_key",
            vec![param("key", key.clone())],
            Ty::Unit,
            true,
            "Removes the item with `key` (keyed lists, handlers only).",
        ),
        m(
            "move",
            vec![param("key", key.clone()), param("to", Ty::INT)],
            Ty::Unit,
            true,
            "Moves the item with `key` to `to` (keyed lists, handlers only).",
        ),
        m(
            "update",
            vec![
                param("key", key),
                param("f", Ty::Fn(Arc::new(FnSig::positional(vec![t.clone()], t)))),
            ],
            Ty::Unit,
            true,
            "Replaces the item with `key` by `f` of it (keyed lists, handlers only).",
        ),
    ]
}

/// The members of an `Async<inner>`: `pending`, `error`, `value`, and for
/// a list its `len` and the transforms that keep it loading.
pub fn async_members(inner: &Ty, key: &Ty) -> Vec<MemberInfo> {
    let mut v = vec![
        MemberInfo::field("pending", Ty::BOOL, "Whether a newer value is loading."),
        MemberInfo::field(
            "error",
            Ty::TEXT.optional(),
            "Why the last load failed, or null.",
        ),
        MemberInfo::field(
            "value",
            inner.clone().optional(),
            "The last value; null before the first.",
        ),
    ];
    if let Ty::List(elem, keyed) = inner {
        for mut m in list_members(elem, *keyed, key) {
            if m.name == "len" {
                m.doc = Some("How many items the last result had (0 before the first).".into());
                v.push(m);
            } else if ASYNC_TRANSFORMS.contains(&m.name.as_str()) {
                for s in &mut m.sigs {
                    let mut sig = (**s).clone();
                    sig.ret = Ty::Async(Box::new(sig.ret));
                    *s = Arc::new(sig);
                }
                v.push(m);
            }
        }
    }
    v
}

/// Every member of a value of type `t` (through `T?` and type aliases
/// alike): what completion after `x.` lists. `types` is the program's
/// type table (the schema's records plus the config's own).
pub fn members_of(t: &Ty, schema: &Schema, types: &TypeTable) -> Vec<MemberInfo> {
    match t {
        Ty::Optional(inner) => members_of(inner, schema, types),
        Ty::List(elem, keyed) => {
            let key = list_key(elem, types);
            list_members(elem, *keyed, &key)
        }
        Ty::Async(inner) => {
            let key = inner
                .list_elem()
                .map_or(Ty::Error, |(e, _)| list_key(e, types));
            async_members(inner, &key)
        }
        Ty::Record(r) => {
            let rec = types.record(*r);
            let doc = |m: &str| {
                schema
                    .doc(&DocKey::Member(rec.name.clone(), m.to_string()))
                    .map(str::to_string)
            };
            let mut v: Vec<MemberInfo> = rec
                .fields
                .iter()
                .map(|f| MemberInfo {
                    name: f.name.clone(),
                    kind: MemberKind::Field,
                    ty: f.ty.clone(),
                    sigs: Vec::new(),
                    writes: f.rw,
                    doc: doc(&f.name),
                })
                .collect();
            v.extend(rec.methods.iter().map(|m| MemberInfo {
                name: m.name.clone(),
                kind: MemberKind::Method,
                ty: Ty::Unit,
                sigs: m.sigs.clone(),
                writes: m.sigs.iter().any(|s| s.action),
                doc: doc(&m.name),
            }));
            v
        }
        Ty::Prim(p) => {
            let mut v = Vec::new();
            if matches!(p, Prim::Text | Prim::Path) {
                v.push(MemberInfo::field("len", Ty::INT, "How many characters."));
            }
            let mut names = vec![p.name()];
            if *p == Prim::Int {
                names.push("float");
            }
            for n in names {
                v.extend(builtin_methods(schema, n));
            }
            v
        }
        Ty::Opaque(n) => builtin_methods(schema, n),
        _ => Vec::new(),
    }
}

/// The schema's methods on a builtin type (`methods color { … }`).
pub fn builtin_methods(schema: &Schema, type_name: &str) -> Vec<MemberInfo> {
    schema
        .methods_of(type_name)
        .iter()
        .map(|m| MemberInfo {
            name: m.name.clone(),
            kind: MemberKind::Method,
            ty: Ty::Unit,
            sigs: m.sigs.clone(),
            writes: m.sigs.iter().any(|s| s.action),
            doc: schema
                .doc(&DocKey::Method(type_name.to_string(), m.name.clone()))
                .map(str::to_string),
        })
        .collect()
}

/// A keyed record's key type, [`Ty::Error`] when it has none.
fn list_key(elem: &Ty, types: &TypeTable) -> Ty {
    match elem {
        Ty::Record(r) => types
            .record(*r)
            .key
            .as_ref()
            .and_then(|path| types.field_path(*r, path))
            .unwrap_or(Ty::Error),
        _ => Ty::Error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_async_and_records_list_their_members() {
        let schema = Schema::builtin();
        let types = &schema.types;
        let hit = Ty::Record(types.find_record("Hit").unwrap());
        let hits = Ty::Async(Box::new(Ty::List(Box::new(hit.clone()), true)));
        let names = |t: &Ty| -> Vec<String> {
            members_of(t, schema, types)
                .into_iter()
                .map(|m| m.name)
                .collect()
        };
        let a = names(&hits);
        for n in ["pending", "error", "value", "len", "take", "filter"] {
            assert!(a.contains(&n.to_string()), "{n}: {a:?}");
        }
        assert!(!a.contains(&"first".to_string()), "element reads need `??`");
        let take = members_of(&hits, schema, types)
            .into_iter()
            .find(|m| m.name == "take")
            .unwrap();
        assert!(matches!(take.sigs[0].ret, Ty::Async(_)));
        let l = names(&Ty::List(Box::new(hit), true));
        assert!(l.contains(&"remove_key".to_string()) && l.contains(&"first".to_string()));
        let battery = Ty::Record(schema.service("battery").unwrap());
        assert!(names(&battery).contains(&"percent".to_string()));
        let search = members_of(&Ty::Record(schema.service("apps").unwrap()), schema, types)
            .into_iter()
            .find(|m| m.name == "search")
            .unwrap();
        assert!(search.doc.is_some(), "schema docs reach the table");
        assert!(names(&Ty::COLOR).contains(&"alpha".to_string()));
        assert!(names(&Ty::TEXT).contains(&"len".to_string()));
    }
}
