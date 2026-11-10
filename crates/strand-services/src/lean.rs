//! An `a{sv}` read without letting an array become a `Value`.
//!
//! zvariant holds every element of an array inside a `Value` as a
//! `Value` of its own (56-64 bytes each), so an `ay` of N bytes read as
//! `OwnedValue` costs about 60 N bytes before anything can look at its
//! size. Pixels any peer sends (a notification's `image-data`, a tray
//! item's `IconPixmap`, a menu entry's `icon-data`) are read here as
//! bytes instead: one copy of what the message carried.
//!
//! A [`Lean`] dict keeps only the keys its [`Keys`] name; every other
//! entry is skipped unread (an `ay` without even a copy). Of a kept
//! entry, a single basic value (a number, boolean, string or object path,
//! or a variant holding one) becomes an [`OwnedValue`] in
//! [`Lean::values`], read as leniently as before (a number of any width,
//! say); the array-carrying shapes the services read become a [`Typed`]
//! in [`Lean::typed`]; any other shape, a mistyped `image-data` among
//! them, is skipped without failing the whole message.

use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;

use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use zbus::zvariant::{ObjectPath, OwnedValue, Signature, Type, Value};

use crate::dbus::Props;

/// The keys a [`Lean`] dict keeps.
pub trait Keys {
    const KEYS: &'static [&'static str];
}

/// One `(width, height, ARGB32 bytes)` of an `a(iiay)` pixmap list.
pub type Pixmap = (i32, i32, Vec<u8>);

/// `image-data`: width, height, rowstride, has alpha, bits, channels,
/// pixels.
pub type ImageData = (i32, i32, i32, bool, i32, i32, Vec<u8>);

/// A kept value that carries an array, by its signature.
#[derive(Debug, Clone, PartialEq)]
pub enum Typed {
    /// `ay`.
    Bytes(Vec<u8>),
    /// `(iiibiiay)`.
    Image(ImageData),
    /// `a(iiay)`.
    Pixmaps(Vec<Pixmap>),
    /// `(sa(iiay)ss)`, a tray tooltip: its title and description (its
    /// icon name and pixmaps are skipped).
    Tip { title: String, body: String },
    /// `aas`, a DBusMenu shortcut, as text: each non-empty list joined by
    /// `+`, the lists by `, `.
    Combos(String),
}

/// An `a{sv}` keeping `K`'s keys (see the module doc).
#[derive(Debug)]
pub struct Lean<K> {
    pub values: Props,
    pub typed: HashMap<String, Typed>,
    keys: PhantomData<K>,
}

impl<K> Lean<K> {
    pub fn new(values: Props, typed: HashMap<String, Typed>) -> Self {
        Lean {
            values,
            typed,
            keys: PhantomData,
        }
    }
}

impl<K> Default for Lean<K> {
    fn default() -> Self {
        Lean::new(Props::new(), HashMap::new())
    }
}

impl<K> Type for Lean<K> {
    const SIGNATURE: &'static Signature = <Props as Type>::SIGNATURE;
}

impl<'de, K: Keys> Deserialize<'de> for Lean<K> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_map(LeanVisitor(PhantomData))
    }
}

struct LeanVisitor<K>(PhantomData<K>);

impl<'de, K: Keys> Visitor<'de> for LeanVisitor<K> {
    type Value = Lean<K>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an a{sv} dict")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Lean<K>, A::Error> {
        let mut lean = Lean::default();
        while let Some(key) = map.next_key::<&'de str>()? {
            let keep = K::KEYS.contains(&key);
            // A key sent twice: the last one counts, as in a map.
            match map.next_value_seed(Entry { keep })? {
                Got::Value(v) => {
                    lean.typed.remove(key);
                    lean.values.insert(key.to_string(), v);
                }
                Got::Typed(t) => {
                    lean.values.remove(key);
                    lean.typed.insert(key.to_string(), t);
                }
                Got::Skipped => {}
            }
        }
        Ok(lean)
    }
}

/// What one entry's variant was read as.
enum Got {
    Value(OwnedValue),
    Typed(Typed),
    Skipped,
}

/// One entry's variant; `keep: false` skips it unread.
struct Entry {
    keep: bool,
}

impl<'de> DeserializeSeed<'de> for Entry {
    type Value = Got;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Got, D::Error> {
        // A variant is read as its signature, then its value.
        d.deserialize_any(self)
    }
}

/// The next element of a variant's sequence (its signature, its value).
fn next<'de, A: SeqAccess<'de>, T: Deserialize<'de>>(seq: &mut A) -> Result<T, A::Error> {
    seq.next_element()?
        .ok_or_else(|| de::Error::custom("a variant without its value"))
}

/// Skips the value of signature `sig`: an `ay` as a borrowed slice, with
/// no copy and no work per byte.
fn skip<'de, A: SeqAccess<'de>>(seq: &mut A, sig: &Signature) -> Result<Got, A::Error> {
    if *sig == "ay" {
        next::<A, &'de [u8]>(seq)?;
    } else {
        next::<A, IgnoredAny>(seq)?;
    }
    Ok(Got::Skipped)
}

impl<'de> Visitor<'de> for Entry {
    type Value = Got;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a variant")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Got, A::Error> {
        let sig: Signature = next(&mut seq)?;
        if !self.keep {
            return skip(&mut seq, &sig);
        }
        fn value<E: de::Error>(v: Value<'_>) -> Result<Got, E> {
            OwnedValue::try_from(v).map(Got::Value).map_err(E::custom)
        }
        match &sig {
            Signature::U8 => value(next::<A, u8>(&mut seq)?.into()),
            Signature::Bool => value(next::<A, bool>(&mut seq)?.into()),
            Signature::I16 => value(next::<A, i16>(&mut seq)?.into()),
            Signature::U16 => value(next::<A, u16>(&mut seq)?.into()),
            Signature::I32 => value(next::<A, i32>(&mut seq)?.into()),
            Signature::U32 => value(next::<A, u32>(&mut seq)?.into()),
            Signature::I64 => value(next::<A, i64>(&mut seq)?.into()),
            Signature::U64 => value(next::<A, u64>(&mut seq)?.into()),
            Signature::F64 => value(next::<A, f64>(&mut seq)?.into()),
            Signature::Str => value(next::<A, &str>(&mut seq)?.into()),
            Signature::ObjectPath => value(next::<A, ObjectPath<'_>>(&mut seq)?.into()),
            // A variant in the variant: kept when it holds a basic value.
            Signature::Variant => match seq
                .next_element_seed(Entry { keep: true })?
                .ok_or_else(|| de::Error::custom("a variant without its value"))?
            {
                Got::Value(inner) => value(Value::Value(Box::new(Value::from(inner)))),
                _ => Ok(Got::Skipped),
            },
            s if *s == "ay" => Ok(Got::Typed(Typed::Bytes(next::<A, Bytes>(&mut seq)?.0))),
            s if *s == "(iiibiiay)" => {
                let (w, h, stride, alpha, bits, channels, Bytes(pixels)) = next(&mut seq)?;
                Ok(Got::Typed(Typed::Image((
                    w, h, stride, alpha, bits, channels, pixels,
                ))))
            }
            s if *s == "a(iiay)" => {
                let list: Vec<(i32, i32, Bytes)> = next(&mut seq)?;
                Ok(Got::Typed(Typed::Pixmaps(
                    list.into_iter().map(|(w, h, b)| (w, h, b.0)).collect(),
                )))
            }
            s if *s == "(sa(iiay)ss)" => {
                let (_, _, title, body): (IgnoredAny, IgnoredAny, String, String) = next(&mut seq)?;
                Ok(Got::Typed(Typed::Tip { title, body }))
            }
            s if *s == "aas" => Ok(Got::Typed(Typed::Combos(next::<A, Combos>(&mut seq)?.0))),
            s => skip(&mut seq, s),
        }
    }
}

/// An `ay` as one copy of its bytes (never an element at a time).
struct Bytes(Vec<u8>);

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Bytes;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("bytes")
            }
            fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Bytes, E> {
                Ok(Bytes(v.to_vec()))
            }
            fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Bytes, E> {
                Ok(Bytes(v))
            }
        }
        d.deserialize_byte_buf(V)
    }
}

/// An `aas` as shortcut text ([`Typed::Combos`]), built as it is read.
struct Combos(String);

impl<'de> Deserialize<'de> for Combos {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        /// One `as`: its strings joined by `+`, and whether it had any.
        struct Combo(String, bool);
        impl<'de> Deserialize<'de> for Combo {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct V;
                impl<'de> Visitor<'de> for V {
                    type Value = Combo;
                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        f.write_str("an as")
                    }
                    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Combo, A::Error> {
                        let (mut text, mut any) = (String::new(), false);
                        while let Some(key) = seq.next_element::<&str>()? {
                            if any {
                                text.push('+');
                            }
                            text.push_str(key);
                            any = true;
                        }
                        Ok(Combo(text, any))
                    }
                }
                d.deserialize_seq(V)
            }
        }
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Combos;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an aas")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Combos, A::Error> {
                let mut text = String::new();
                while let Some(Combo(combo, any)) = seq.next_element()? {
                    if !any {
                        continue;
                    }
                    if !text.is_empty() {
                        text.push_str(", ");
                    }
                    text.push_str(&combo);
                }
                Ok(Combos(text))
            }
        }
        d.deserialize_seq(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::serialized::Context;
    use zbus::zvariant::{LE, SerializeValue, to_bytes};

    struct Wanted;
    impl Keys for Wanted {
        const KEYS: &'static [&'static str] = &[
            "byte", "text", "path", "nested", "pixels", "image", "mistyped", "pixmaps", "tip",
            "keys",
        ];
    }

    fn decode(body: &HashMap<&str, Value<'_>>) -> Lean<Wanted> {
        let bytes = to_bytes(Context::new_dbus(LE, 0), body).unwrap();
        bytes.deserialize::<Lean<Wanted>>().unwrap().0
    }

    /// Basic values stay `Value`s; the array shapes are typed; unknown
    /// keys and other shapes are left out without failing the read.
    #[test]
    fn a_lean_dict_keeps_its_keys_values_and_typed_arrays() {
        let tip = Value::from((
            "icon",
            vec![(1i32, 1i32, vec![9u8, 9, 9, 9])],
            "Title",
            "Body",
        ));
        let lean = decode(&HashMap::from([
            ("byte", Value::from(2u8)),
            ("text", Value::from("hello")),
            ("path", Value::from(ObjectPath::try_from("/a/b").unwrap())),
            ("nested", Value::Value(Box::new(Value::from(7u32)))),
            ("pixels", Value::from(vec![1u8, 2, 3])),
            (
                "image",
                Value::from((1i32, 1i32, 4i32, true, 8i32, 4i32, vec![1u8, 2, 3, 4])),
            ),
            // image-data shaped wrong: left out, the rest still read.
            ("mistyped", Value::from((1i32, 1i32, 4i32))),
            ("pixmaps", Value::from(vec![(2i32, 1i32, vec![0u8; 8])])),
            ("tip", tip),
            (
                "keys",
                Value::from(vec![vec!["Control", "S"], vec![], vec!["Alt", "F4"]]),
            ),
            ("unknown", Value::from(vec![5u8; 64])),
            ("unwanted", Value::from(1u32)),
        ]));
        assert_eq!(crate::dbus::number(&lean.values, "byte"), Some(2.0));
        assert_eq!(
            crate::dbus::text(&lean.values, "text").as_deref(),
            Some("hello")
        );
        assert!(
            lean.values["path"]
                .downcast_ref::<ObjectPath<'_>>()
                .is_ok_and(|p| p.as_str() == "/a/b")
        );
        assert_eq!(crate::dbus::number(&lean.values, "nested"), Some(7.0));
        assert_eq!(lean.typed["pixels"], Typed::Bytes(vec![1, 2, 3]));
        assert_eq!(
            lean.typed["image"],
            Typed::Image((1, 1, 4, true, 8, 4, vec![1, 2, 3, 4]))
        );
        assert_eq!(
            lean.typed["pixmaps"],
            Typed::Pixmaps(vec![(2, 1, vec![0; 8])])
        );
        assert_eq!(
            lean.typed["tip"],
            Typed::Tip {
                title: "Title".into(),
                body: "Body".into()
            }
        );
        assert_eq!(
            lean.typed["keys"],
            Typed::Combos("Control+S, Alt+F4".into())
        );
        for gone in ["mistyped", "unknown", "unwanted"] {
            assert!(
                !lean.values.contains_key(gone) && !lean.typed.contains_key(gone),
                "{gone}"
            );
        }
    }

    /// The bytes of an `ay` are read as bytes, without a `Value` each.
    #[test]
    fn pixels_are_read_as_bytes_not_values() {
        const N: usize = 1 << 20;
        let pixels = vec![7u8; N];
        let body = HashMap::from([
            ("pixels", SerializeValue(&pixels)),
            ("unknown", SerializeValue(&pixels)),
        ]);
        let bytes = to_bytes(Context::new_dbus(LE, 0), &body).unwrap();
        let (lean, _): (Lean<Wanted>, _) = bytes.deserialize().unwrap();
        assert_eq!(lean.typed["pixels"], Typed::Bytes(pixels));
        assert!(!lean.typed.contains_key("unknown"));
    }
}
