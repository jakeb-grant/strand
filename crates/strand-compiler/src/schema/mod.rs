//! The builtin schema: every element, service, function and token the
//! language knows, as data.
//!
//! The schema is written in a small declaration language
//! ([`builtin.schema`](https://github.com/jakeb-grant/strand/blob/main/crates/strand-compiler/src/schema/builtin.schema),
//! parsed by [`Schema::extend`]) rather than in Rust, so the checker, the
//! LSP (completion, hover) and later service crates share one table. A
//! service crate contributes its schema the same way: it hands its text to
//! [`Schema::extend`] (M3: "service schemas drive type checking and LSP
//! hover"), and the checker sees its records, methods and events like the
//! builtin ones.
//!
//! The declaration language, one item after another (`//` comments, `;` or
//! line breaks between members). A `///` comment documents the entry it
//! precedes (record, field, method, event, function, value, element,
//! prop, palette role or token) and is kept in [`Schema::docs`] under a
//! [`DocKey`] for hover; parameter defaults keep their source text
//! ([`ParamSig::default`](crate::ty::ParamSig::default)).
//!
//! ```text
//! enum Edge { top, bottom, left, right }
//! opaque Palette, Spring                 // named types with no structure
//! alias AppId = text
//! record Workspace key id {              // `key`: lists of it are keyed
//!   id: int
//!   volume: float rw                     // writable (`<->`, assignment)
//!   fn format(pattern: text) -> text     // pure method
//!   action focus()                       // handlers only
//!   event received(n: Notification)      // `on svc.received(n)`
//! }
//! service battery { … }                  // a record that is a global name
//! handle record Node { … }               // a runtime handle, not data
//! provisional service audio { … }        // a stub one extension replaces
//! fn pct(x: float) -> text lift          // `lift`: null in, null out
//! fn join(sep: text, ...parts: any?) -> text
//! value t: float                         // a builtin value
//! methods color { fn alpha(a: float) -> color }   // methods on a builtin type
//! group node { … }                       // props, events shared by elements
//! element text(text -> text): node { ellipsis: Ellipsis; on click; let index: int; flags leaf }
//!                                        // flags: leaf surface uniforms selectors on_demand only_in <el>
//!                                        // `(T -> prop)`: the positional's type and the prop it fills
//! palette { surface; fg; accent }        // colour roles
//! tokens { space { 1: length }; surface.hi: color }
//! ```
//!
//! Types: `int float bool text color paint path length percent angle
//! duration font shadow insets corners any unit`, names declared
//! above, `[T]`, `T?`, `Async<T>`, `fn(A, B) -> R`, `(A, B)` (comma
//! shorthand), `A | B` (props only).

mod members;
mod parse;

pub use members::{
    ASYNC_TRANSFORMS, MemberInfo, MemberKind, async_members, builtin_methods, list_members,
    members_of,
};

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use crate::ty::{EventDef, FnSig, MethodDef, RecordId, Ty, TypeTable};

/// A prop an element accepts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropSchema {
    pub name: String,
    pub ty: Ty,
    /// May be bound two-way: `value: <-> state`.
    pub two_way: bool,
    /// Inherited by every descendant (`font`, `color`).
    pub inherited: bool,
    /// Props of the prop's sub-block (`stroke: 3, $accent { cap: round }`).
    pub sub: Vec<PropSchema>,
}

/// What may contain what.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ElementFlags {
    /// Takes no children (`text`, `icon`, `slider`).
    pub leaf: bool,
    /// A top-level surface (`bar`, `panel`, `osd`, `lock`); `popup` is an
    /// anchored surface declared inside an element.
    pub surface: bool,
    /// May only appear directly inside this element (`start` in `split`).
    pub only_in: Option<String>,
    /// Accepts any prop starting with `u_` (shader uniforms).
    pub uniforms: bool,
    /// Holds `#id { … }` selectors (`svg`).
    pub selectors: bool,
    /// Mounts its children only on demand (`popup` when opened, `tooltip`
    /// on hover, `page` while current), like an `if` branch.
    pub on_demand: bool,
}

/// An element kind: its positional argument, props, events and the names
/// it brings into scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElementSchema {
    pub name: String,
    /// The type of the positional argument (`text clock.format(…)`).
    pub arg: Option<Ty>,
    /// The prop the positional argument fills (`meter x` is its `value`,
    /// `icon x` its `source`): `element meter(float -> value)`. Set
    /// whenever `arg` is.
    pub arg_prop: Option<String>,
    pub props: Vec<PropSchema>,
    pub events: Vec<EventDef>,
    /// Names in scope inside the element (`index` in `letters`).
    pub scope: Vec<(String, Ty)>,
    pub flags: ElementFlags,
}

impl ElementSchema {
    pub fn prop(&self, name: &str) -> Option<&PropSchema> {
        self.props.iter().find(|p| p.name == name)
    }

    pub fn event(&self, name: &str) -> Option<&EventDef> {
        self.events.iter().find(|e| e.name == name)
    }
}

/// A token the schema declares: a palette role or a base-tier name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenSchema {
    pub ty: Ty,
    /// A palette colour role (`surface`, `fg`, `accent`, …).
    pub palette: bool,
}

/// Everything the language knows before reading a config.
#[derive(Clone, Debug, Default)]
pub struct Schema {
    /// Records (services and their items) and enums.
    pub types: TypeTable,
    /// `alias AppId = text`.
    pub aliases: BTreeMap<String, Ty>,
    /// `opaque Palette`.
    pub opaques: BTreeMap<String, Ty>,
    /// Global service names and their records.
    pub services: BTreeMap<String, RecordId>,
    /// Builtin functions, with overloads.
    pub functions: BTreeMap<String, Vec<Arc<FnSig>>>,
    /// Builtin values (`t`).
    pub values: BTreeMap<String, Ty>,
    /// Methods on builtin types, by type name (`color`, `text`, …).
    pub methods: BTreeMap<String, Vec<MethodDef>>,
    /// Prop groups elements include.
    pub groups: BTreeMap<String, ElementSchema>,
    pub elements: BTreeMap<String, ElementSchema>,
    /// Palette roles and base-tier tokens by path (`space.2`).
    pub tokens: BTreeMap<String, TokenSchema>,
    /// `///` doc comments, by what they document (hover and completion).
    pub docs: BTreeMap<DocKey, String>,
    /// Records and services declared `provisional` (the types-only service
    /// stubs of the builtin text): the first extension that declares the
    /// same name replaces each one in place.
    pub provisional: BTreeSet<String>,
    /// BLAKE3 over every text given to [`Schema::extend`], in order; see
    /// [`Schema::fingerprint`].
    fingerprint: [u8; 32],
}

/// What a schema doc comment documents.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DocKey {
    /// A record, service, enum, alias or opaque type, by name.
    Type(String),
    /// A field, method or event of a record or service: (type, member).
    Member(String, String),
    /// A builtin function or action.
    Function(String),
    /// A builtin value (`t`).
    Value(String),
    /// A method on a builtin type (`methods color { … }`): (type, method).
    Method(String, String),
    /// An element kind or prop group.
    Element(String),
    /// A prop of an element (`stroke.dash` for a sub-prop), its event
    /// (`on click`) or a name it brings into scope (`index`): (element,
    /// name). Props an element includes from a group carry the group's
    /// docs.
    Prop(String, String),
    /// A palette role or base-tier token path (`accent`, `space.2`).
    Token(String),
}

/// A problem in schema text, with its line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaError {
    pub line: u32,
    pub message: String,
}

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

/// The builtin schema's source.
pub const BUILTIN: &str = include_str!("builtin.schema");

impl Schema {
    /// The builtin schema, parsed once.
    pub fn builtin() -> &'static Schema {
        static BUILTIN_SCHEMA: OnceLock<Schema> = OnceLock::new();
        BUILTIN_SCHEMA.get_or_init(|| {
            let mut s = Schema::default();
            // The builtin text is part of the compiler and has a test
            // (`builtin_schema_parses`); a broken entry is skipped here
            // rather than taking the compiler down, so this applies the
            // rest even on error, unlike `extend`.
            s.fingerprint = s.next_fingerprint(BUILTIN);
            let _ = parse::extend(&mut s, BUILTIN);
            s
        })
    }

    /// Adds the declarations in `text` (the language described in the
    /// module docs). Names may refer to anything declared before, in this
    /// text, or in earlier calls. An extension adds and never replaces,
    /// except that it may replace a `provisional` record or service once.
    ///
    /// Atomic: on error nothing is added and the fingerprint is unchanged.
    pub fn extend(&mut self, text: &str) -> Result<(), Vec<SchemaError>> {
        let mut staged = self.clone();
        parse::extend(&mut staged, text)?;
        staged.fingerprint = self.next_fingerprint(text);
        *self = staged;
        Ok(())
    }

    /// The entries without a `///` doc (design.md: hovers are generated
    /// from the schemas, so every service, record member, function,
    /// method, value, element, prop, event and token has one). Empty for
    /// the builtin schema and for every service crate's extension (tests
    /// hold both to it).
    pub fn undocumented(&self) -> Vec<String> {
        let s = self;
        let mut missing = Vec::new();
        let mut need = |k: DocKey| {
            if s.doc(&k).is_none() {
                missing.push(format!("{k:?}"));
            }
        };
        for name in s.services.keys() {
            need(DocKey::Type(name.clone()));
        }
        for r in &s.types.records {
            need(DocKey::Type(r.name.clone()));
            let member = |n: &str| DocKey::Member(r.name.clone(), n.to_string());
            r.fields.iter().for_each(|f| need(member(&f.name)));
            r.methods.iter().for_each(|m| need(member(&m.name)));
            r.events.iter().for_each(|e| need(member(&e.name)));
        }
        for name in s.functions.keys() {
            need(DocKey::Function(name.clone()));
        }
        for name in s.values.keys() {
            need(DocKey::Value(name.clone()));
        }
        for (ty, ms) in &s.methods {
            for m in ms {
                need(DocKey::Method(ty.clone(), m.name.clone()));
            }
        }
        for (name, e) in s.groups.iter().chain(&s.elements) {
            need(DocKey::Element(name.clone()));
            let prop = |n: String| DocKey::Prop(name.clone(), n);
            for p in &e.props {
                need(prop(p.name.clone()));
                for q in &p.sub {
                    need(prop(format!("{}.{}", p.name, q.name)));
                }
            }
            e.events
                .iter()
                .for_each(|ev| need(prop(format!("on {}", ev.name))));
            e.scope.iter().for_each(|(n, _)| need(prop(n.clone())));
        }
        for path in s.tokens.keys() {
            if s.token_doc(path).is_none() {
                missing.push(format!("token {path}"));
            }
        }
        missing
    }

    /// The builtin schema extended by each of `texts` in order (the
    /// service crates a binary links: `strand_services::schemas()`), for
    /// `strand check`, `strand run` and the LSP alike. Fails with the
    /// index of the first text that does not apply, and its errors.
    pub fn builtin_with(texts: &[&str]) -> Result<Schema, (usize, Vec<SchemaError>)> {
        let mut s = Schema::builtin().clone();
        for (i, t) in texts.iter().enumerate() {
            s.extend(t).map_err(|e| (i, e))?;
        }
        Ok(s)
    }

    /// The fingerprint after `text` is added.
    fn next_fingerprint(&self, text: &str) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"strand-schema\0");
        h.update(&self.fingerprint);
        h.update(&(text.len() as u64).to_le_bytes());
        h.update(text.as_bytes());
        *h.finalize().as_bytes()
    }

    /// The schema hash: BLAKE3 chained over the builtin text and every
    /// text a service crate added with [`Schema::extend`], in order. The
    /// compiled-output cache is keyed by source hash, compiler version and
    /// this, so a service crate that changes its schema invalidates
    /// configs compiled against the old one (design.md, live reload).
    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// The doc comment of an entry, if it has one.
    pub fn doc(&self, key: &DocKey) -> Option<&str> {
        self.docs.get(key).map(String::as_str)
    }

    /// The doc of token `path`, or of the nearest group holding it
    /// (`space.2` reads the doc of `space`).
    pub fn token_doc(&self, path: &str) -> Option<&str> {
        let mut p = path;
        loop {
            if let Some(d) = self.docs.get(&DocKey::Token(p.to_string())) {
                return Some(d);
            }
            p = &p[..p.rfind('.')?];
        }
    }

    /// A service's record.
    pub fn service(&self, name: &str) -> Option<RecordId> {
        self.services.get(name).copied()
    }

    pub fn element(&self, name: &str) -> Option<&ElementSchema> {
        self.elements.get(name)
    }

    /// Methods on a builtin type such as `color` or `text`.
    pub fn methods_of(&self, type_name: &str) -> &[MethodDef] {
        self.methods.get(type_name).map_or(&[], Vec::as_slice)
    }

    /// Palette role names, in declaration order of the token table.
    pub fn palette_roles(&self) -> impl Iterator<Item = &str> {
        self.tokens
            .iter()
            .filter(|(_, t)| t.palette)
            .map(|(k, _)| k.as_str())
    }

    /// The named type `name` (records, enums, aliases, opaques and the
    /// builtin type names).
    pub fn named_type(&self, name: &str) -> Option<Ty> {
        if let Some(p) = crate::ty::Prim::from_name(name) {
            return Some(Ty::Prim(p));
        }
        match name {
            "any" => return Some(Ty::Any),
            "unit" => return Some(Ty::Unit),
            _ => {}
        }
        if let Some(t) = self.aliases.get(name).or_else(|| self.opaques.get(name)) {
            return Some(t.clone());
        }
        if let Some(r) = self.types.find_record(name) {
            return Some(Ty::Record(r));
        }
        self.types.find_enum(name).map(Ty::Enum)
    }

    /// `[T]` of the schema's type `elem`: keyed when `elem` is a record
    /// with a key.
    pub fn list_of(types: &TypeTable, elem: Ty) -> Ty {
        let keyed = match &elem {
            Ty::Record(r) => types.record(*r).key.is_some(),
            _ => false,
        };
        Ty::List(Box::new(elem), keyed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ty::Prim;

    #[test]
    fn builtin_schema_parses() {
        let mut s = Schema::default();
        if let Err(errors) = s.extend(BUILTIN) {
            panic!(
                "{}",
                errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
        assert!(s.elements.len() >= 40, "{}", s.elements.len());
    }

    /// Every element kind, service and token family design.md uses.
    #[test]
    fn builtin_schema_covers_the_design() {
        let s = Schema::builtin();
        for el in [
            "bar",
            "panel",
            "osd",
            "popup",
            "lock",
            "row",
            "col",
            "stack",
            "grid",
            "scroll",
            "list",
            "split",
            "start",
            "center",
            "end",
            "spacer",
            "pages",
            "page",
            "text",
            "icon",
            "image",
            "box",
            "button",
            "slider",
            "input",
            "meter",
            "segmented",
            "tooltip",
            "arc",
            "graph",
            "spectrum",
            "shader",
            "canvas",
            "svg",
            "lottie",
            "thumbnail",
            "effect",
            "particles",
            "letters",
            "merge",
        ] {
            assert!(s.element(el).is_some(), "element {el}");
        }
        for svc in [
            "clock",
            "calendar",
            "battery",
            "windows",
            "workspaces",
            "audio",
            "brightness",
            "tray",
            "notifications",
            "apps",
            "media",
            "system",
            "cpu",
            "screens",
            "wm",
        ] {
            assert!(s.service(svc).is_some(), "service {svc}");
        }
        for f in [
            "pct", "dur", "join", "material", "import", "oklch", "linear", "radial", "conic",
            "spring", "ease", "bezier", "wave", "noise", "grow", "blur", "fade", "popin", "slide",
        ] {
            assert!(s.functions.contains_key(f), "function {f}");
        }
        for t in [
            "surface",
            "fg",
            "accent",
            "on_accent",
            "error",
            "outline",
            "shadow",
            "secondary",
            "tertiary",
            "space.2",
            "radius.lg",
            "font.ui",
            "motion.spatial",
            "elevation.md",
            "surface.hi",
            "fg.muted",
            "border",
        ] {
            assert!(s.tokens.contains_key(t), "token {t}");
        }
        let text = s.element("text").unwrap();
        assert_eq!(text.arg, Some(Ty::TEXT));
        assert!(text.prop("font").unwrap().inherited);
        assert!(text.flags.leaf);
        let slider = s.element("slider").unwrap();
        assert!(slider.prop("value").unwrap().two_way);
        let audio = s.types.record(s.service("audio").unwrap());
        let Ty::Record(sink) = audio.field("sink").unwrap().ty else {
            panic!()
        };
        assert!(s.types.record(sink).field("volume").unwrap().rw);
        assert_eq!(
            s.tokens["space.2"].ty,
            Ty::Prim(Prim::Length),
            "base tiers are typed"
        );
    }

    #[test]
    fn services_can_be_contributed() {
        let mut s = Schema::builtin().clone();
        s.extend(
            "record Profile key name { name: text }\n\
             service ppd { profile: text rw; profiles: [Profile]; action cycle() }",
        )
        .unwrap();
        let ppd = s.types.record(s.service("ppd").unwrap());
        assert!(ppd.field("profile").unwrap().rw);
        assert!(matches!(
            ppd.field("profiles").unwrap().ty,
            Ty::List(_, true)
        ));
        assert!(ppd.method("cycle").unwrap().sigs[0].action);
    }

    #[test]
    fn a_positional_names_the_prop_it_fills() {
        let s = Schema::builtin();
        assert_eq!(
            s.element("meter").unwrap().arg_prop.as_deref(),
            Some("value")
        );
        assert_eq!(
            s.element("icon").unwrap().arg_prop.as_deref(),
            Some("source")
        );
        // `letters` takes no positional, so it fills nothing.
        assert_eq!(s.element("letters").unwrap().arg_prop, None);
        for el in s.elements.values() {
            assert_eq!(el.arg.is_some(), el.arg_prop.is_some(), "{}", el.name);
        }
        let mut s = s.clone();
        let err = s
            .extend("element gauge(float): node { }")
            .expect_err("no prop");
        assert!(err[0].message.contains("`(T -> prop)`"), "{err:?}");
    }

    #[test]
    fn extensions_add_but_never_replace() {
        for text in [
            "element text(int -> text): node { }",
            "group node { }",
            "alias AppId = int",
            "value t: text",
            "palette { accent }",
            "tokens { space { 2: length } }",
            "fn pct(fraction: float) -> text",
        ] {
            let mut s = Schema::builtin().clone();
            let err = s.extend(text).expect_err(text);
            assert!(err[0].message.contains("declared twice"), "{text}: {err:?}");
        }
        // The builtin `text` element is untouched by the refused text.
        let mut s = Schema::builtin().clone();
        let _ = s.extend("element text(int -> text): node { }");
        assert_eq!(s.element("text").unwrap().arg, Some(Ty::TEXT));
        // An overload with other parameters is still allowed.
        let mut s = Schema::builtin().clone();
        s.extend("fn pct(part: int, whole: int) -> text").unwrap();
        assert_eq!(s.functions["pct"].len(), 2);
        // Records and services that are not provisional stay as they are,
        // and so do builtin methods.
        for (text, rec, fields) in [
            ("record Range { start: text }", "Range", 2),
            ("record Node { x: int }", "Node", 6),
        ] {
            let mut s = Schema::builtin().clone();
            let err = s.extend(text).expect_err(text);
            assert!(err[0].message.contains("declared twice"), "{text}: {err:?}");
            let id = s.types.find_record(rec).unwrap();
            assert_eq!(s.types.record(id).fields.len(), fields, "{text}");
        }
        let mut s = Schema::builtin().clone();
        let err = s
            .extend("methods color { fn mix(other: color, amount: float) -> int }")
            .expect_err("same parameters as the builtin mix");
        assert!(err[0].message.contains("declared twice"), "{err:?}");
    }

    #[test]
    fn record_keys_name_fields() {
        let mut s = Schema::builtin().clone();
        let err = s
            .extend("record Foo key nope { a: int }")
            .expect_err("no field `nope`");
        assert!(err[0].message.contains("key `nope` of `Foo`"), "{err:?}");
        assert!(s.types.find_record("Foo").is_none(), "atomic");
        // Replacing a stub that a builtin key runs through (`Hit key
        // app.id`) without that field breaks the key: refused.
        let err = s
            .extend("record App key name { name: text }")
            .expect_err("`Hit key app.id` broken");
        assert!(err[0].message.contains("key `app.id` of `Hit`"), "{err:?}");
        let app = s.types.find_record("App").unwrap();
        assert!(s.types.record(app).field("id").is_some(), "unchanged");
    }

    #[test]
    fn a_contributed_service_replaces_its_stub_once() {
        let builtin = Schema::builtin();
        let stub = builtin.service("battery").unwrap();
        assert!(builtin.provisional.contains("battery"));
        let mut s = builtin.clone();
        s.extend(
            "/// The real one.\n\
             service battery { percent: float; level: float rw }\n\
             record Window key id { id: text; title: text; pid: int }",
        )
        .unwrap();
        // Same id, so `[Window]` fields of other services see the new one.
        assert_eq!(s.service("battery"), Some(stub));
        let bat = s.types.record(stub);
        assert_eq!(bat.fields.len(), 2);
        assert!(bat.field("level").unwrap().rw);
        assert_eq!(
            s.doc(&DocKey::Type("battery".into())),
            Some("The real one.")
        );
        let win = s.types.find_record("Window").unwrap();
        assert!(s.types.record(win).field("pid").is_some());
        assert!(!s.provisional.contains("battery"));
        // A second contribution is refused, and leaves the first alone.
        let err = s
            .extend("service battery { other: int }")
            .expect_err("replaced once");
        assert!(err[0].message.contains("declared twice"), "{err:?}");
        assert_eq!(s.types.record(stub).fields.len(), 2);
    }

    #[test]
    fn a_failed_extend_changes_nothing() {
        let mut s = Schema::builtin().clone();
        let before = s.fingerprint();
        let err = s.extend("value fresh: int\nvalue t: text").unwrap_err();
        assert!(err[0].message.contains("declared twice"), "{err:?}");
        assert!(!s.values.contains_key("fresh"));
        assert_eq!(s.fingerprint(), before);
        let err = s.extend("service ppd { level: Nope }").unwrap_err();
        assert!(err[0].message.contains("Nope"), "{err:?}");
        assert!(s.service("ppd").is_none());
        assert_eq!(s.fingerprint(), before);
    }

    #[test]
    fn fingerprint_tracks_every_extension() {
        let builtin = Schema::builtin();
        let mut again = Schema::default();
        let _ = again.extend(BUILTIN);
        assert_eq!(builtin.fingerprint(), again.fingerprint(), "stable");
        assert_ne!(builtin.fingerprint(), Schema::default().fingerprint());
        let mut a = builtin.clone();
        a.extend("service ppd { profile: text rw }").unwrap();
        assert_ne!(a.fingerprint(), builtin.fingerprint());
        let mut b = builtin.clone();
        b.extend("service ppd { profile: text }").unwrap();
        assert_ne!(a.fingerprint(), b.fingerprint());
        // Order matters: the same texts in another order are another
        // schema (a later text may refer to an earlier one).
        let (x, y) = ("enum P { a }", "enum Q { b }");
        let mut xy = Schema::default();
        xy.extend(x).unwrap();
        xy.extend(y).unwrap();
        let mut yx = Schema::default();
        yx.extend(y).unwrap();
        yx.extend(x).unwrap();
        assert_ne!(xy.fingerprint(), yx.fingerprint());
    }

    #[test]
    fn doc_comments_and_defaults_are_kept() {
        let mut s = Schema::builtin().clone();
        s.extend(
            "/// Power profiles.\n\
             service ppd {\n\
               /// The active profile.\n\
               profile: text rw\n\
               // not a doc\n\
               /// Cycles to the next one.\n\
               action cycle()\n\
             }\n\
             /// Formats a profile.\n\
             fn fmt(p: text, upper: bool = false, sep: text = \" / \") -> text\n\
             element ring: node {\n\
               /// How full.\n\
               value: float\n\
             }\n\
             tokens { /// Ring thickness.\n ring { width: length } }",
        )
        .unwrap();
        let doc = |k: DocKey| s.doc(&k).map(str::to_string);
        assert_eq!(
            doc(DocKey::Type("ppd".into())).as_deref(),
            Some("Power profiles.")
        );
        let ppd = s.types.record(s.service("ppd").unwrap());
        assert_eq!(ppd.doc.as_deref(), Some("Power profiles."));
        assert_eq!(
            doc(DocKey::Member("ppd".into(), "profile".into())).as_deref(),
            Some("The active profile.")
        );
        assert_eq!(
            doc(DocKey::Member("ppd".into(), "cycle".into())).as_deref(),
            Some("Cycles to the next one.")
        );
        assert_eq!(
            doc(DocKey::Function("fmt".into())).as_deref(),
            Some("Formats a profile.")
        );
        assert_eq!(
            doc(DocKey::Prop("ring".into(), "value".into())).as_deref(),
            Some("How full.")
        );
        assert_eq!(
            doc(DocKey::Token("ring".into())).as_deref(),
            Some("Ring thickness.")
        );
        let fmt = &s.functions["fmt"][0];
        assert_eq!(fmt.params[1].default.as_deref(), Some("false"));
        assert_eq!(fmt.params[2].default.as_deref(), Some("\" / \""));
        assert_eq!(fmt.params[0].default, None);
        // The builtin schema documents its entries; group props reach the
        // elements that include them.
        let b = Schema::builtin();
        assert!(b.doc(&DocKey::Function("join".into())).is_some());
        assert!(b.doc(&DocKey::Type("Drop".into())).is_some());
        assert!(
            b.doc(&DocKey::Member("apps".into(), "search".into()))
                .is_some()
        );
        assert!(b.doc(&DocKey::Value("t".into())).is_some());
    }

    /// Hovers are generated from the schemas (design.md, "System
    /// services"): every service, record member, function, method, value,
    /// element, prop, event and token of the builtin schema has a doc.
    #[test]
    fn builtin_schema_is_documented() {
        let s = Schema::builtin();
        let missing = s.undocumented();
        assert!(missing.is_empty(), "undocumented: {missing:#?}");
        // A token reads its group's doc; its own comes first.
        assert_eq!(
            s.token_doc("space.2"),
            s.doc(&DocKey::Token("space".into()))
        );
        assert_ne!(s.token_doc("surface.hi"), s.token_doc("surface"));
    }

    #[test]
    fn schema_errors_name_the_line() {
        let mut s = Schema::default();
        let errs = s.extend("record A {\n  x: Nope\n}").unwrap_err();
        assert_eq!(errs[0].line, 2);
        assert!(errs[0].message.contains("Nope"), "{}", errs[0].message);
    }
}
