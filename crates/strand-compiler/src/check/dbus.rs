//! No-code `from dbus` services checked against D-Bus introspection
//! (design.md: "checked against introspection"): each field's property
//! exists on the object, its D-Bus type converts to the field's declared
//! type, and an `rw` field's property is writable.
//!
//! The compiler has no bus of its own: the caller hands it an
//! [`Introspect`] (`strand check`, `strand run`'s loader and the LSP use
//! `strand-introspect`). A bus or object that cannot be reached is a
//! warning that says the check did not run, never an error: a config is
//! not broken because a daemon is down.

use crate::diagnostic::{Diagnostic, closest};
use crate::hir::{self, SourceSpec};
use crate::ty::{Prim, Ty};

/// One property of an introspected object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BusProperty {
    pub interface: String,
    pub name: String,
    /// Its D-Bus type signature.
    pub signature: String,
    pub writable: bool,
}

/// Reads an object's properties from a bus.
pub trait Introspect {
    /// The properties of `path` of bus name `name` on the system (or
    /// session) bus; `Err` says why it could not be read. `None` while
    /// the answer is still being asked for (the LSP does not wait on a
    /// bus): the service is not checked now, nor warned about, and the
    /// caller checks again once the answer is in.
    fn properties(
        &self,
        system: bool,
        name: &str,
        path: &str,
    ) -> Option<Result<Vec<BusProperty>, String>>;
}

/// The object path a bus name's object is at by convention.
pub fn default_path(name: &str) -> String {
    format!("/{}", name.replace('.', "/").replace('-', "_"))
}

/// Whether a D-Bus value of type `sig` converts to `ty` (see the module
/// docs; `v` converts to anything).
pub fn compatible(ty: &Ty, sig: &str) -> bool {
    const INTS: &[&str] = &["y", "n", "q", "i", "u", "x", "t"];
    if sig == "v" {
        return true;
    }
    match ty {
        Ty::Optional(inner) => compatible(inner, sig),
        Ty::Prim(Prim::Bool) => sig == "b",
        Ty::Prim(Prim::Int) => INTS.contains(&sig),
        Ty::Prim(Prim::Float | Prim::Percent | Prim::Duration) => sig == "d" || INTS.contains(&sig),
        Ty::Prim(Prim::Text | Prim::Path) => matches!(sig, "s" | "o" | "g"),
        Ty::Prim(Prim::Color) => sig == "s",
        Ty::Enum(_) => sig == "s",
        Ty::List(item, _) => match sig.strip_prefix('a') {
            Some(rest) if !rest.starts_with('{') => compatible(item, rest),
            _ => false,
        },
        Ty::Record(_) => sig.starts_with("a{s"),
        _ => false,
    }
}

/// Check every `from dbus` service of `program` against `intro`.
pub fn check(program: &hir::Program, intro: &dyn Introspect) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for file in &program.files {
        for item in &file.items {
            let hir::Item::Service(s) = item else {
                continue;
            };
            let Some(SourceSpec::Dbus { system, name, path }) = &s.spec else {
                continue;
            };
            let path = path.clone().unwrap_or_else(|| default_path(name));
            let service = &program.def(s.def).name;
            let props = match intro.properties(*system, name, &path) {
                None => continue,
                Some(Ok(p)) => p,
                Some(Err(why)) => {
                    let mut d = Diagnostic::warning(
                        "check::dbus_unchecked",
                        format!("`{service}` was not checked against D-Bus introspection: {why}"),
                    )
                    .with_label_in(s.file, s.source_span, "not checked");
                    d.help = Some(format!(
                        "it is checked whenever `{name}` answers on the {} bus",
                        if *system { "system" } else { "session" }
                    ));
                    out.push(d);
                    continue;
                }
            };
            if props.is_empty() {
                let mut d = Diagnostic::warning(
                    "check::dbus_unchecked",
                    format!("`{name}` has no properties at `{path}`"),
                )
                .with_label_in(s.file, s.source_span, "nothing to read");
                d.help = Some(
                    "name its object path: `from dbus system \"name\" \"/object/path\"`".into(),
                );
                out.push(d);
                continue;
            }
            for f in &s.fields {
                let Some(prop) = f.key.first() else {
                    continue;
                };
                // The interface named like the bus name first.
                let found = props
                    .iter()
                    .filter(|p| p.name == *prop)
                    .min_by_key(|p| p.interface != *name);
                let Some(p) = found else {
                    let names: Vec<&str> = props.iter().map(|p| p.name.as_str()).collect();
                    let fix = closest(prop, names.iter().copied());
                    let mut d = Diagnostic::error(
                        "check::dbus_property",
                        format!("`{name}` has no property `{prop}`"),
                    )
                    .with_label_in(s.file, f.key_span, "not a property");
                    if let Some(fix) = fix {
                        d.suggest(f.key_span, fix);
                        d = d.in_file(s.file);
                    } else {
                        d.help = Some(format!("its properties: {}", names.join(", ")));
                    }
                    out.push(d);
                    continue;
                };
                if !compatible(&f.ty, &p.signature) {
                    let mut d = Diagnostic::error(
                        "check::dbus_type",
                        format!(
                            "`{name}`'s `{prop}` is a D-Bus `{}`, which is not a {}",
                            p.signature,
                            program.types.show(&f.ty)
                        ),
                    )
                    .with_label_in(s.file, f.span, "the declared type");
                    d.help = Some(format!("declare it as {}", suggested_type(&p.signature)));
                    out.push(d);
                }
                if f.rw && !p.writable {
                    let mut d = Diagnostic::error(
                        "check::dbus_read_only",
                        format!("`{name}`'s `{prop}` cannot be written"),
                    )
                    .with_label_in(s.file, f.span, "declared `rw`");
                    d.help = Some("drop `rw`: the property is read-only".into());
                    out.push(d);
                }
            }
        }
    }
    out
}

/// The schema type a D-Bus signature reads as best.
fn suggested_type(sig: &str) -> String {
    match sig {
        "b" => "`bool`".into(),
        "y" | "n" | "q" | "i" | "u" | "x" | "t" => "`int` (or `float`)".into(),
        "d" => "`float`".into(),
        "s" | "o" | "g" => "`text`".into(),
        s if s.starts_with("a{") => "a record type".into(),
        s if s.starts_with('a') => {
            format!("a list: [{}]", suggested_type(&s[1..]).trim_matches('`'))
        }
        _ => format!("something a `{sig}` converts to"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_convert_to_declared_types() {
        assert!(compatible(&Ty::TEXT, "s"));
        assert!(compatible(&Ty::FLOAT, "u"));
        assert!(compatible(&Ty::FLOAT, "d"));
        assert!(!compatible(&Ty::Prim(Prim::Int), "d"));
        assert!(compatible(&Ty::list(Ty::TEXT), "as"));
        assert!(!compatible(&Ty::list(Ty::TEXT), "a{sv}"));
        assert!(!compatible(&Ty::TEXT, "b"));
        assert!(compatible(
            &Ty::Optional(Box::new(Ty::Prim(Prim::Bool))),
            "b"
        ));
        assert!(compatible(&Ty::TEXT, "v"));
        assert_eq!(
            default_path("net.hadess.PowerProfiles"),
            "/net/hadess/PowerProfiles"
        );
    }
}
