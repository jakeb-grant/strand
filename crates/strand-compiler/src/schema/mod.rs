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
//! line breaks between members):
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
        parse::extend(self, text)
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
    fn schema_errors_name_the_line() {
        let mut s = Schema::default();
        let errs = s.extend("record A {\n  x: Nope\n}").unwrap_err();
        assert_eq!(errs[0].line, 2);
        assert!(errs[0].message.contains("Nope"), "{}", errs[0].message);
    }
}
