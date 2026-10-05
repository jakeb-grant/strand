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
//! fn pct(x: float) -> text lift          // `lift`: null in, null out
//! fn join(sep: text, ...parts: any?) -> text
//! value t: float                         // a builtin value
//! methods color { fn alpha(a: float) -> color }   // methods on a builtin type
//! group node { … }                       // props, events shared by elements
//! element text(text): node { ellipsis: Ellipsis; on click; let index: int; flags leaf }
//! palette { surface; fg; accent }        // colour roles
//! tokens { space { 1: length }; surface.hi: color }
//! ```
//!
//! Types: `int float bool text color paint path length percent angle
//! duration font shadow insets corners any unit`, names declared
//! above, `[T]`, `T?`, `Async<T>`, `fn(A, B) -> R`, `(A, B)` (comma
//! shorthand), `A | B` (props only).

mod parse;

use std::collections::BTreeMap;
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
}

/// An element kind: its positional argument, props, events and the names
/// it brings into scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElementSchema {
    pub name: String,
    /// The type of the positional argument (`text clock.format(…)`).
    pub arg: Option<Ty>,
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
            // rather than taking the compiler down.
            let _ = s.extend(BUILTIN);
            s
        })
    }

    /// Adds the declarations in `text` (the language described in the
    /// module docs). Names may refer to anything declared before, in this
    /// text, or in earlier calls. Entries with errors are skipped and
    /// reported; the rest are added.
    pub fn extend(&mut self, text: &str) -> Result<(), Vec<SchemaError>> {
        let mut h = blake3::Hasher::new();
        h.update(b"strand-schema\0");
        h.update(&self.fingerprint);
        h.update(&(text.len() as u64).to_le_bytes());
        h.update(text.as_bytes());
        self.fingerprint = *h.finalize().as_bytes();
        parse::extend(self, text)
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
    fn extensions_add_but_never_replace() {
        for text in [
            "element text(int): node { }",
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
        let _ = s.extend("element text(int): node { }");
        assert_eq!(s.element("text").unwrap().arg, Some(Ty::TEXT));
        // An overload with other parameters is still allowed.
        let mut s = Schema::builtin().clone();
        s.extend("fn pct(part: int, whole: int) -> text").unwrap();
        assert_eq!(s.functions["pct"].len(), 2);
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

    #[test]
    fn schema_errors_name_the_line() {
        let mut s = Schema::default();
        let errs = s.extend("record A {\n  x: Nope\n}").unwrap_err();
        assert_eq!(errs[0].line, 2);
        assert!(errs[0].message.contains("Nope"), "{}", errs[0].message);
    }
}
