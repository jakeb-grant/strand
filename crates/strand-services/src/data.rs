//! [`Data`]: the plain values that cross between a service and the
//! language.
//!
//! `strand-services` never sees the VM's `Value` (architecture.md,
//! "Several service crates, one host"): service state is typed Rust, and
//! where something must be handled by name (the language side converting
//! a field to a `Value`, a write or an action's arguments coming back) it
//! goes through `Data`, a small dynamic mirror of the schema's types:
//! records by type and field name, enums by variant name.
//!
//! [`ToData`], [`FromData`] and [`SchemaType`] are implemented for the
//! primitives here and derived for records and enums
//! (`#[derive(Data)]`).

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

/// A name in [`Data`]: static for what services produce, owned for what
/// comes back from the language.
pub type Name = Cow<'static, str>;

/// A colour as the schema's `color`: straight sRGB channels, 0 to 1.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Rgba {
    /// An opaque colour.
    pub fn rgb(r: f32, g: f32, b: f32) -> Rgba {
        Rgba { r, g, b, a: 1.0 }
    }
}

/// A value of a schema type, by name.
#[derive(Clone, Debug, PartialEq)]
pub enum Data {
    /// `null`, and an absent optional.
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(Arc<str>),
    Duration(Duration),
    Color(Rgba),
    List(Vec<Data>),
    /// A record value: its schema type and its fields by name.
    Record {
        ty: Name,
        fields: Vec<(Name, Data)>,
    },
    /// An enum variant by name.
    Enum {
        ty: Name,
        variant: Name,
    },
}

static NULL: Data = Data::Null;

impl Data {
    /// A short name of the kind of value, for errors.
    pub fn kind(&self) -> &'static str {
        match self {
            Data::Null => "null",
            Data::Bool(_) => "bool",
            Data::Int(_) => "int",
            Data::Float(_) => "float",
            Data::Text(_) => "text",
            Data::Duration(_) => "duration",
            Data::Color(_) => "color",
            Data::List(_) => "list",
            Data::Record { .. } => "record",
            Data::Enum { .. } => "enum",
        }
    }

    /// A text value.
    pub fn text(s: impl Into<Arc<str>>) -> Data {
        Data::Text(s.into())
    }

    /// The field `name` of a record (`None` for anything else, or a
    /// record without it).
    pub fn field(&self, name: &str) -> Option<&Data> {
        match self {
            Data::Record { fields, .. } => fields.iter().find(|(n, _)| n == name).map(|(_, d)| d),
            _ => None,
        }
    }

    /// `self` with the value at `path` replaced by `value`: what a write
    /// into a field below a service field does (`audio.sink.volume`).
    pub fn with_path(&self, path: &[Step], value: Data) -> Result<Data, DataError> {
        let Some((first, rest)) = path.split_first() else {
            return Ok(value);
        };
        match (first, self) {
            (Step::Field(name), Data::Record { ty, fields }) => {
                let mut fields = fields.clone();
                let Some(slot) = fields.iter_mut().find(|(n, _)| n == name) else {
                    return Err(DataError::new(format!("{ty} has no field `{name}`")));
                };
                slot.1 = slot.1.with_path(rest, value)?;
                Ok(Data::Record {
                    ty: ty.clone(),
                    fields,
                })
            }
            (Step::Index(i), Data::List(items)) => {
                let mut items = items.clone();
                let Some(slot) = items.get_mut(*i) else {
                    return Err(DataError::new(format!(
                        "index {i} out of range for {}",
                        items.len()
                    )));
                };
                *slot = slot.with_path(rest, value)?;
                Ok(Data::List(items))
            }
            (step, other) => Err(DataError::new(format!(
                "cannot write `{step}` inside a {}",
                other.kind()
            ))),
        }
    }
}

/// One step of a path below a service field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Field(String),
    Index(usize),
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::Field(n) => write!(f, ".{n}"),
            Step::Index(i) => write!(f, "[{i}]"),
        }
    }
}

/// A value that did not fit the type asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataError(pub String);

impl DataError {
    pub fn new(msg: impl Into<String>) -> DataError {
        DataError(msg.into())
    }

    /// "expected `ty`, got a `kind`".
    pub fn expected(ty: &str, got: &Data) -> DataError {
        DataError(format!("expected {ty}, got {}", got.kind()))
    }

    /// The error inside field or argument `name`.
    pub fn within(self, name: &str) -> DataError {
        DataError(format!("{name}: {}", self.0))
    }
}

impl fmt::Display for DataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DataError {}

/// The field `name` of a record of type `ty` ([`Data::Null`] when the
/// record lacks it, so an optional field may be left out).
pub fn record_field<'a>(d: &'a Data, ty: &str, name: &str) -> Result<&'a Data, DataError> {
    match d {
        Data::Record { fields, .. } => Ok(fields
            .iter()
            .find(|(n, _)| n == name)
            .map_or(&NULL, |(_, v)| v)),
        other => Err(DataError::expected(ty, other)),
    }
}

/// Into [`Data`].
pub trait ToData {
    fn to_data(&self) -> Data;
}

/// Out of [`Data`].
pub trait FromData: Sized {
    fn from_data(d: &Data) -> Result<Self, DataError>;
}

/// The type as the schema language writes it (`float`, `[Workspace]`,
/// `text?`): what a store's fields are checked against.
pub trait SchemaType {
    fn schema_type() -> String;
}

macro_rules! prim {
    ($t:ty, $schema:literal, $to:expr, $from:expr) => {
        impl ToData for $t {
            fn to_data(&self) -> Data {
                #[allow(clippy::redundant_closure_call)]
                ($to)(self)
            }
        }
        impl FromData for $t {
            fn from_data(d: &Data) -> Result<Self, DataError> {
                #[allow(clippy::redundant_closure_call)]
                ($from)(d).ok_or_else(|| DataError::expected($schema, d))
            }
        }
        impl SchemaType for $t {
            fn schema_type() -> String {
                $schema.to_string()
            }
        }
    };
}

prim!(
    bool,
    "bool",
    |b: &bool| Data::Bool(*b),
    |d: &Data| match d {
        Data::Bool(b) => Some(*b),
        _ => None,
    }
);
prim!(i64, "int", |n: &i64| Data::Int(*n), |d: &Data| match d {
    Data::Int(n) => Some(*n),
    _ => None,
});
prim!(
    i32,
    "int",
    |n: &i32| Data::Int(i64::from(*n)),
    |d: &Data| {
        match d {
            Data::Int(n) => i32::try_from(*n).ok(),
            _ => None,
        }
    }
);
prim!(
    u32,
    "int",
    |n: &u32| Data::Int(i64::from(*n)),
    |d: &Data| {
        match d {
            Data::Int(n) => u32::try_from(*n).ok(),
            _ => None,
        }
    }
);
prim!(
    u64,
    "int",
    |n: &u64| Data::Int(i64::try_from(*n).unwrap_or(i64::MAX)),
    |d: &Data| match d {
        Data::Int(n) => u64::try_from(*n).ok(),
        _ => None,
    }
);
prim!(
    f64,
    "float",
    |n: &f64| Data::Float(*n),
    |d: &Data| match d {
        Data::Float(n) => Some(*n),
        Data::Int(n) => Some(*n as f64),
        _ => None,
    }
);
prim!(
    f32,
    "float",
    |n: &f32| Data::Float(f64::from(*n)),
    |d: &Data| match d {
        Data::Float(n) => Some(*n as f32),
        Data::Int(n) => Some(*n as f32),
        _ => None,
    }
);
prim!(
    String,
    "text",
    |s: &String| Data::Text(s.as_str().into()),
    |d: &Data| match d {
        Data::Text(s) => Some(s.to_string()),
        _ => None,
    }
);
prim!(
    Arc<str>,
    "text",
    |s: &Arc<str>| Data::Text(s.clone()),
    |d: &Data| match d {
        Data::Text(s) => Some(s.clone()),
        _ => None,
    }
);
prim!(
    Duration,
    "duration",
    |t: &Duration| Data::Duration(*t),
    |d: &Data| match d {
        Data::Duration(t) => Some(*t),
        _ => None,
    }
);
prim!(
    Rgba,
    "color",
    |c: &Rgba| Data::Color(*c),
    |d: &Data| match d {
        Data::Color(c) => Some(*c),
        _ => None,
    }
);

impl ToData for () {
    fn to_data(&self) -> Data {
        Data::Null
    }
}

impl<T: ToData> ToData for Option<T> {
    fn to_data(&self) -> Data {
        self.as_ref().map_or(Data::Null, ToData::to_data)
    }
}

impl<T: FromData> FromData for Option<T> {
    fn from_data(d: &Data) -> Result<Self, DataError> {
        match d {
            Data::Null => Ok(None),
            d => T::from_data(d).map(Some),
        }
    }
}

impl<T: SchemaType> SchemaType for Option<T> {
    fn schema_type() -> String {
        format!("{}?", T::schema_type())
    }
}

impl<T: ToData> ToData for Vec<T> {
    fn to_data(&self) -> Data {
        Data::List(self.iter().map(ToData::to_data).collect())
    }
}

impl<T: FromData> FromData for Vec<T> {
    fn from_data(d: &Data) -> Result<Self, DataError> {
        match d {
            Data::List(items) => items.iter().map(T::from_data).collect(),
            Data::Null => Ok(Vec::new()),
            other => Err(DataError::expected("list", other)),
        }
    }
}

impl<T: SchemaType> SchemaType for Vec<T> {
    fn schema_type() -> String {
        format!("[{}]", T::schema_type())
    }
}

impl ToData for Data {
    fn to_data(&self) -> Data {
        self.clone()
    }
}

impl FromData for Data {
    fn from_data(d: &Data) -> Result<Self, DataError> {
        Ok(d.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitives_round_trip() {
        assert_eq!(f64::from_data(&0.5f64.to_data()), Ok(0.5));
        assert_eq!(f64::from_data(&Data::Int(2)), Ok(2.0));
        assert_eq!(Option::<f64>::from_data(&Data::Null), Ok(None));
        assert_eq!(
            Vec::<f64>::from_data(&vec![0.1f64, 0.2].to_data()),
            Ok(vec![0.1, 0.2])
        );
        assert!(bool::from_data(&Data::Int(1)).is_err());
        assert_eq!(<Vec<Option<f64>>>::schema_type(), "[float?]");
        assert_eq!(<Option<Duration>>::schema_type(), "duration?");
    }

    #[test]
    fn paths_replace_one_leaf() {
        let sink = Data::Record {
            ty: "AudioDevice".into(),
            fields: vec![
                ("volume".into(), Data::Float(0.4)),
                ("muted".into(), Data::Bool(false)),
            ],
        };
        let new = sink
            .with_path(&[Step::Field("volume".into())], Data::Float(0.8))
            .unwrap();
        assert_eq!(new.field("volume"), Some(&Data::Float(0.8)));
        assert_eq!(new.field("muted"), Some(&Data::Bool(false)));
        assert!(
            sink.with_path(&[Step::Field("nope".into())], Data::Null)
                .is_err()
        );
        assert!(sink.with_path(&[Step::Index(0)], Data::Null).is_err());
    }
}
