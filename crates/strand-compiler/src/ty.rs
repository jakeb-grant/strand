//! Types of the `.strand` language.
//!
//! One [`Ty`] describes every value the checker sees: numbers with units,
//! colours, text, records (service items, `type` declarations, settings
//! files), enums, lists (keyed or plain), nullable `T?`, `Async<T>` and
//! functions. Records and enums live in a [`TypeTable`] shared by the
//! builtin [`Schema`](crate::schema::Schema) and the user's declarations,
//! and [`Ty`] refers to them by id.

use std::fmt;
use std::sync::Arc;

/// Index of a record in a [`TypeTable`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RecordId(pub u32);

/// Index of an enum in a [`TypeTable`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EnumId(pub u32);

/// Built-in scalar and opaque types the checker reasons about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Prim {
    Bool,
    /// Whole numbers (`at: int`, `max_lines: int`).
    Int,
    /// A plain number (`0.72`, `36`).
    Float,
    /// `4px`, `4ch`; plain numbers are pixels.
    Length,
    /// `40%`: a length relative to the parent, or a fraction (`8%` = 0.08).
    Percent,
    /// `270deg`.
    Angle,
    /// `6s`, `200ms`.
    Duration,
    /// `#7aa2f7`, `$accent`.
    Color,
    /// A colour or a gradient (`linear(…)`).
    Paint,
    Text,
    /// A file path (`~` expands); text converts both ways.
    Path,
    /// `"Inter" 13px 500`, `$font.ui`.
    Font,
    /// `0 2px 8px $shadow`, a comma list of those, `$elevation.md`.
    Shadow,
    /// One to four comma-separated lengths (`pad`, `margin`).
    Insets,
    /// One to four comma-separated radii, or `full` (`radius`).
    Corners,
}

impl Prim {
    pub fn name(self) -> &'static str {
        match self {
            Prim::Bool => "bool",
            Prim::Int => "int",
            Prim::Float => "float",
            Prim::Length => "length",
            Prim::Percent => "percent",
            Prim::Angle => "angle",
            Prim::Duration => "duration",
            Prim::Color => "color",
            Prim::Paint => "paint",
            Prim::Text => "text",
            Prim::Path => "path",
            Prim::Font => "font",
            Prim::Shadow => "shadow",
            Prim::Insets => "insets",
            Prim::Corners => "corners",
        }
    }

    pub fn from_name(name: &str) -> Option<Prim> {
        Some(match name {
            "bool" => Prim::Bool,
            "int" => Prim::Int,
            "float" => Prim::Float,
            "length" => Prim::Length,
            "percent" => Prim::Percent,
            "angle" => Prim::Angle,
            "duration" => Prim::Duration,
            "color" => Prim::Color,
            "paint" => Prim::Paint,
            "text" => Prim::Text,
            "path" => Prim::Path,
            "font" => Prim::Font,
            "shadow" => Prim::Shadow,
            "insets" => Prim::Insets,
            "corners" => Prim::Corners,
            _ => return None,
        })
    }

    /// Numbers of any unit.
    pub fn is_numeric(self) -> bool {
        matches!(
            self,
            Prim::Int | Prim::Float | Prim::Length | Prim::Percent | Prim::Angle | Prim::Duration
        )
    }

    /// Unit-less numbers.
    pub fn is_scalar(self) -> bool {
        matches!(self, Prim::Int | Prim::Float)
    }
}

/// A type.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Ty {
    /// Something already reported; accepted everywhere so one mistake
    /// gives one diagnostic.
    Error,
    /// Accepts anything (`drag:`, the parts of `join`).
    Any,
    /// The type of `null`.
    Null,
    /// What actions and handlers return.
    Unit,
    Prim(Prim),
    /// A named opaque type from the schema: `Palette`, `Spring`, `Mask`…
    Opaque(Arc<str>),
    /// A value of an enum.
    Enum(EnumId),
    /// The enum itself used as a value (`segmented { options: Look }`).
    EnumType(EnumId),
    Record(RecordId),
    /// `[T]`; `keyed` when items carry an identity (`key`, or a record
    /// with a key), which a tree `for` needs.
    List(Box<Ty>, bool),
    /// `T?`.
    Optional(Box<Ty>),
    /// `Async<T>`: keeps its last value, exposes `pending` and `error`.
    Async(Box<Ty>),
    Fn(Arc<FnSig>),
    /// Comma shorthand of distinct parts (`border: 1, $border`); a prefix
    /// may be given.
    Tuple(Vec<Ty>),
    /// One of several types; schema prop types only.
    Union(Vec<Ty>),
}

impl Ty {
    pub const BOOL: Ty = Ty::Prim(Prim::Bool);
    pub const INT: Ty = Ty::Prim(Prim::Int);
    pub const FLOAT: Ty = Ty::Prim(Prim::Float);
    pub const LENGTH: Ty = Ty::Prim(Prim::Length);
    pub const PERCENT: Ty = Ty::Prim(Prim::Percent);
    pub const ANGLE: Ty = Ty::Prim(Prim::Angle);
    pub const DURATION: Ty = Ty::Prim(Prim::Duration);
    pub const COLOR: Ty = Ty::Prim(Prim::Color);
    pub const PAINT: Ty = Ty::Prim(Prim::Paint);
    pub const TEXT: Ty = Ty::Prim(Prim::Text);
    pub const PATH: Ty = Ty::Prim(Prim::Path);
    pub const FONT: Ty = Ty::Prim(Prim::Font);
    pub const SHADOW: Ty = Ty::Prim(Prim::Shadow);

    pub fn opaque(name: &str) -> Ty {
        Ty::Opaque(name.into())
    }

    pub fn list(elem: Ty) -> Ty {
        Ty::List(Box::new(elem), false)
    }

    pub fn keyed_list(elem: Ty) -> Ty {
        Ty::List(Box::new(elem), true)
    }

    /// `T?`, without doubling an existing `?` (and `null?` is `null`).
    pub fn optional(self) -> Ty {
        match self {
            Ty::Optional(_) | Ty::Null | Ty::Error | Ty::Any => self,
            t => Ty::Optional(Box::new(t)),
        }
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Ty::Error)
    }

    /// True for types that never report mismatches.
    pub fn is_lenient(&self) -> bool {
        matches!(self, Ty::Error | Ty::Any)
    }

    pub fn is_optional(&self) -> bool {
        matches!(self, Ty::Optional(_) | Ty::Null)
    }

    /// `T` for `T?`; itself otherwise.
    pub fn non_null(&self) -> &Ty {
        match self {
            Ty::Optional(t) => t,
            t => t,
        }
    }

    pub fn prim(&self) -> Option<Prim> {
        match self {
            Ty::Prim(p) => Some(*p),
            _ => None,
        }
    }

    pub fn is_numeric(&self) -> bool {
        self.prim().is_some_and(Prim::is_numeric)
    }

    /// The element type of a list (or of an `Async` list).
    pub fn list_elem(&self) -> Option<(&Ty, bool)> {
        match self {
            Ty::List(t, keyed) => Some((t, *keyed)),
            Ty::Async(inner) => inner.list_elem(),
            _ => None,
        }
    }
}

impl TypeTable {
    /// True if values of this type can be stored with `persist` or in a
    /// settings file: what the codecs in `vm::persist` round-trip
    /// (numbers, text, paths, colours, enums, and records and lists of
    /// those). Functions, pending loads, opaque values (`Palette`,
    /// `Spring`), fonts, shadows and gradients are not data, nor are
    /// `handle` records (`Node`, `Canvas`) or records with actions (live
    /// service items: `Window`, `App`); any other record is data when every
    /// field is.
    pub fn is_data(&self, ty: &Ty) -> bool {
        self.data_walk(ty, true, &mut Vec::new())
    }

    /// True if values of this type can identify the items of a `for`
    /// (`key`): comparable values. Looser than [`TypeTable::is_data`]:
    /// opaque values and fonts compare, functions and pending loads do
    /// not.
    pub fn is_comparable(&self, ty: &Ty) -> bool {
        self.data_walk(ty, false, &mut Vec::new())
    }

    fn data_walk(&self, ty: &Ty, stored: bool, seen: &mut Vec<RecordId>) -> bool {
        match ty {
            Ty::Error | Ty::Null | Ty::Enum(_) => true,
            Ty::Prim(p) => {
                !stored
                    || !matches!(
                        p,
                        Prim::Paint | Prim::Font | Prim::Shadow | Prim::Insets | Prim::Corners
                    )
            }
            Ty::Record(r) => {
                if seen.contains(r) {
                    // A recursive type (`type Node { kids: [Node] }`):
                    // the fields already being walked decide.
                    return true;
                }
                let rec = self.record(*r);
                // A runtime handle, or a live service item whose actions
                // act on something that may be gone after a restart.
                if stored
                    && (rec.handle || rec.methods.iter().any(|m| m.sigs.iter().any(|s| s.action)))
                {
                    return false;
                }
                seen.push(*r);
                let ok = rec
                    .fields
                    .iter()
                    .all(|f| self.data_walk(&f.ty, stored, seen));
                seen.pop();
                ok
            }
            Ty::List(t, _) | Ty::Optional(t) => self.data_walk(t, stored, seen),
            Ty::Tuple(ts) => !stored && ts.iter().all(|t| self.data_walk(t, stored, seen)),
            Ty::Opaque(_) => !stored,
            Ty::Any | Ty::Unit | Ty::EnumType(_) | Ty::Async(_) | Ty::Fn(_) | Ty::Union(_) => false,
        }
    }
}

/// A function or method signature.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FnSig {
    pub params: Vec<ParamSig>,
    pub ret: Ty,
    /// An action: it changes the world (`ws.focus()`, `n.expire()`), so
    /// only handlers may call it.
    pub action: bool,
    /// Passes null through: if a nullable value reaches a non-nullable
    /// parameter, the result is nullable (`pct(x)`, `dur(y)`).
    pub lift: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ParamSig {
    pub name: String,
    pub ty: Ty,
    pub has_default: bool,
    /// The default as written in the schema (`"%H:%M"`, `0.5`), for
    /// hover; `None` for config `fn`s and parameters without one.
    pub default: Option<String>,
    /// `...parts: T`: takes every remaining positional argument.
    pub variadic: bool,
}

impl FnSig {
    pub fn new(params: Vec<ParamSig>, ret: Ty) -> Self {
        Self {
            params,
            ret,
            action: false,
            lift: false,
        }
    }

    /// A signature of unnamed positional parameters (a function type such
    /// as `fn(int) -> int`): diagnostics call them "argument 1", …
    pub fn positional(params: Vec<Ty>, ret: Ty) -> Self {
        Self::named(
            params.into_iter().map(|ty| (String::new(), ty)).collect(),
            ret,
        )
    }

    /// A signature of named positional parameters without defaults
    /// (lambdas: `(a: int) => …`). An empty name is an unnamed parameter.
    pub fn named(params: Vec<(String, Ty)>, ret: Ty) -> Self {
        Self::new(
            params
                .into_iter()
                .map(|(name, ty)| ParamSig {
                    name,
                    ty,
                    has_default: false,
                    default: None,
                    variadic: false,
                })
                .collect(),
            ret,
        )
    }
}

impl ParamSig {
    /// How diagnostics name the parameter at `index`: `` `name` `` or,
    /// for an unnamed one, `argument 2`.
    pub fn label(&self, index: usize) -> String {
        if self.name.is_empty() {
            format!("argument {}", index + 1)
        } else {
            format!("`{}`", self.name)
        }
    }
}

/// Where a record or enum was declared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    Schema,
    /// A user declaration: file and name span.
    User(crate::FileId, crate::syntax::Span),
}

/// A field of a record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldDef {
    pub name: String,
    pub ty: Ty,
    /// Writable: `rw` service fields and settings-file fields.
    pub rw: bool,
}

/// A method of a record or service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MethodDef {
    pub name: String,
    /// Overloads, tried in order.
    pub sigs: Vec<Arc<FnSig>>,
}

/// An event a service emits (`on notifications.received(n)`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventDef {
    pub name: String,
    pub params: Vec<ParamSig>,
}

/// A record: a service, a service item, a `type`, a settings file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordDef {
    pub name: String,
    pub fields: Vec<FieldDef>,
    pub methods: Vec<MethodDef>,
    pub events: Vec<EventDef>,
    /// The identity of items of this record, as a field path (`id`,
    /// `app.id`): lists of it are keyed.
    pub key: Option<Vec<String>>,
    /// Values are live runtime handles (`Node`, `Canvas`), not data:
    /// they compare, but `persist` and settings files cannot store them.
    pub handle: bool,
    pub origin: Origin,
    pub doc: Option<String>,
}

impl RecordDef {
    pub fn new(name: impl Into<String>, origin: Origin) -> Self {
        Self {
            name: name.into(),
            fields: Vec::new(),
            methods: Vec::new(),
            events: Vec::new(),
            key: None,
            handle: false,
            origin,
            doc: None,
        }
    }

    pub fn field(&self, name: &str) -> Option<&FieldDef> {
        self.fields.iter().find(|f| f.name == name)
    }

    pub fn method(&self, name: &str) -> Option<&MethodDef> {
        self.methods.iter().find(|m| m.name == name)
    }

    pub fn event(&self, name: &str) -> Option<&EventDef> {
        self.events.iter().find(|e| e.name == name)
    }

    /// Field and method names, for did-you-mean.
    pub fn member_names(&self) -> impl Iterator<Item = &str> {
        self.fields
            .iter()
            .map(|f| f.name.as_str())
            .chain(self.methods.iter().map(|m| m.name.as_str()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumDef {
    pub name: String,
    pub variants: Vec<String>,
    pub origin: Origin,
}

impl EnumDef {
    pub fn variant(&self, name: &str) -> Option<u32> {
        self.variants
            .iter()
            .position(|v| v == name)
            .map(|i| i as u32)
    }
}

/// Every record and enum of a program: the schema's first, then the
/// user's.
#[derive(Clone, Debug, Default)]
pub struct TypeTable {
    pub records: Vec<RecordDef>,
    pub enums: Vec<EnumDef>,
}

impl TypeTable {
    pub fn record(&self, id: RecordId) -> &RecordDef {
        &self.records[id.0 as usize]
    }

    pub fn record_mut(&mut self, id: RecordId) -> &mut RecordDef {
        &mut self.records[id.0 as usize]
    }

    pub fn enum_(&self, id: EnumId) -> &EnumDef {
        &self.enums[id.0 as usize]
    }

    pub fn add_record(&mut self, def: RecordDef) -> RecordId {
        self.records.push(def);
        RecordId(self.records.len() as u32 - 1)
    }

    pub fn add_enum(&mut self, def: EnumDef) -> EnumId {
        self.enums.push(def);
        EnumId(self.enums.len() as u32 - 1)
    }

    pub fn find_record(&self, name: &str) -> Option<RecordId> {
        self.records
            .iter()
            .rposition(|r| r.name == name)
            .map(|i| RecordId(i as u32))
    }

    /// Whether the config declares a record or enum called `name` (which
    /// then hides a schema type of that name).
    pub fn user_named(&self, name: &str) -> bool {
        let user = |o: &Origin| matches!(o, Origin::User(..));
        self.records
            .iter()
            .any(|r| r.name == name && user(&r.origin))
            || self.enums.iter().any(|e| e.name == name && user(&e.origin))
    }

    pub fn find_enum(&self, name: &str) -> Option<EnumId> {
        self.enums
            .iter()
            .rposition(|e| e.name == name)
            .map(|i| EnumId(i as u32))
    }

    /// Whether a value of type `from` may be used where `to` is expected.
    pub fn assignable(&self, from: &Ty, to: &Ty) -> bool {
        use Prim::*;
        if from == to || from.is_lenient() || to.is_lenient() {
            return true;
        }
        match (from, to) {
            (_, Ty::Union(ts)) => ts.iter().any(|t| self.assignable(from, t)),
            (Ty::Union(ts), _) => ts.iter().all(|t| self.assignable(t, to)),
            (Ty::Null, Ty::Optional(_)) => true,
            (Ty::Optional(a), Ty::Optional(b)) => self.assignable(a, b),
            (_, Ty::Optional(b)) => self.assignable(from, b),
            (Ty::Async(a), Ty::Async(b)) => self.assignable(a, b),
            (Ty::List(a, _), Ty::List(b, _)) => self.assignable(a, b),
            (Ty::Tuple(a), Ty::Tuple(b)) => {
                !a.is_empty()
                    && a.len() <= b.len()
                    && a.iter().zip(b).all(|(x, y)| self.assignable(x, y))
            }
            // A single value is the first part of a shorthand (`glow: 12`).
            (_, Ty::Tuple(b)) => b.first().is_some_and(|t| self.assignable(from, t)),
            (Ty::Fn(a), Ty::Fn(b)) => {
                a.params.len() <= b.params.len()
                    && a.params
                        .iter()
                        .zip(&b.params)
                        .all(|(x, y)| self.assignable(&y.ty, &x.ty))
                    && (b.ret == Ty::Unit || self.assignable(&a.ret, &b.ret))
            }
            (Ty::Prim(a), Ty::Prim(b)) => matches!(
                (a, b),
                (Int, Float | Length | Angle)
                    | (Float, Length | Angle)
                    | (Percent, Float | Length)
                    | (Color, Paint)
                    | (Text, Path)
                    | (Path, Text)
                    | (Int | Float | Length | Percent, Insets | Corners)
                    | (Shadow, Shadow)
            ),
            _ => false,
        }
    }

    /// The type both branches of a ternary or `match` fit, if any.
    pub fn join(&self, a: &Ty, b: &Ty) -> Option<Ty> {
        if a.is_error() || b.is_error() {
            return Some(Ty::Error);
        }
        match (a, b) {
            (Ty::Null, t) | (t, Ty::Null) => return Some(t.clone().optional()),
            (Ty::Optional(x), y) | (y, Ty::Optional(x)) if !y.is_optional() => {
                return self.join(x, y).map(Ty::optional);
            }
            (Ty::Prim(Prim::Int), Ty::Prim(Prim::Float))
            | (Ty::Prim(Prim::Float), Ty::Prim(Prim::Int)) => return Some(Ty::FLOAT),
            (Ty::List(x, k1), Ty::List(y, k2)) => {
                return self.join(x, y).map(|t| Ty::List(Box::new(t), *k1 && *k2));
            }
            _ => {}
        }
        if self.assignable(a, b) {
            Some(b.clone())
        } else if self.assignable(b, a) {
            Some(a.clone())
        } else {
            None
        }
    }

    /// Displays a type with record and enum names.
    pub fn show<'a>(&'a self, ty: &'a Ty) -> ShowTy<'a> {
        ShowTy { table: self, ty }
    }

    /// The type of the field path `path` inside record `rec`, if every
    /// segment exists.
    pub fn field_path(&self, rec: RecordId, path: &[String]) -> Option<Ty> {
        let mut ty = Ty::Record(rec);
        for seg in path {
            let Ty::Record(r) = ty.non_null().clone() else {
                return None;
            };
            ty = self.record(r).field(seg)?.ty.clone();
        }
        Some(ty)
    }
}

/// [`TypeTable::show`].
#[derive(Debug)]
pub struct ShowTy<'a> {
    table: &'a TypeTable,
    ty: &'a Ty,
}

impl fmt::Display for ShowTy<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let show = |ty| self.table.show(ty);
        // A schema type the config hides with its own `type`/`enum` of
        // the same name: `builtin Align`, so "expects `builtin Align`,
        // found `Align`" never reads as a contradiction.
        let named = |f: &mut fmt::Formatter<'_>, name: &str, schema: bool| {
            if schema && self.table.user_named(name) {
                write!(f, "builtin {name}")
            } else {
                f.write_str(name)
            }
        };
        let schema = |o: &Origin| matches!(o, Origin::Schema);
        match self.ty {
            Ty::Error => f.write_str("{unknown}"),
            Ty::Any => f.write_str("any"),
            Ty::Null => f.write_str("null"),
            Ty::Unit => f.write_str("()"),
            Ty::Prim(p) => f.write_str(p.name()),
            Ty::Opaque(n) => named(f, n, true),
            Ty::Enum(e) => {
                let e = self.table.enum_(*e);
                named(f, &e.name, schema(&e.origin))
            }
            Ty::EnumType(e) => {
                let e = self.table.enum_(*e);
                f.write_str("enum ")?;
                named(f, &e.name, schema(&e.origin))
            }
            Ty::Record(r) => {
                let r = self.table.record(*r);
                named(f, &r.name, schema(&r.origin))
            }
            Ty::List(t, _) => write!(f, "[{}]", show(t)),
            Ty::Optional(t) => match &**t {
                Ty::Fn(_) | Ty::Union(_) => write!(f, "({})?", show(t)),
                _ => write!(f, "{}?", show(t)),
            },
            Ty::Async(t) => write!(f, "Async<{}>", show(t)),
            Ty::Fn(sig) => {
                f.write_str("fn(")?;
                for (i, p) in sig.params.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{}", show(&p.ty))?;
                }
                f.write_str(")")?;
                if sig.ret != Ty::Unit {
                    write!(f, " -> {}", show(&sig.ret))?;
                }
                Ok(())
            }
            Ty::Tuple(ts) => {
                for (i, t) in ts.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{}", show(t))?;
                }
                Ok(())
            }
            Ty::Union(ts) => {
                for (i, t) in ts.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" | ")?;
                    }
                    write!(f, "{}", show(t))?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_widen_but_durations_need_units() {
        let t = TypeTable::default();
        assert!(t.assignable(&Ty::INT, &Ty::FLOAT));
        assert!(t.assignable(&Ty::FLOAT, &Ty::LENGTH));
        assert!(t.assignable(&Ty::PERCENT, &Ty::FLOAT));
        assert!(!t.assignable(&Ty::FLOAT, &Ty::DURATION));
        assert!(!t.assignable(&Ty::FLOAT, &Ty::INT));
    }

    #[test]
    fn null_and_async_are_not_their_value() {
        let t = TypeTable::default();
        assert!(t.assignable(&Ty::Null, &Ty::TEXT.optional()));
        assert!(!t.assignable(&Ty::TEXT.optional(), &Ty::TEXT));
        assert!(t.assignable(&Ty::TEXT, &Ty::TEXT.optional()));
        assert!(!t.assignable(&Ty::Async(Box::new(Ty::TEXT)), &Ty::TEXT));
        assert_eq!(t.join(&Ty::Null, &Ty::TEXT), Some(Ty::TEXT.optional()));
        assert_eq!(t.join(&Ty::INT, &Ty::FLOAT), Some(Ty::FLOAT));
    }

    #[test]
    fn types_display() {
        let t = TypeTable::default();
        let ty = Ty::Async(Box::new(Ty::list(Ty::TEXT.optional())));
        assert_eq!(t.show(&ty).to_string(), "Async<[text?]>");
    }
}
