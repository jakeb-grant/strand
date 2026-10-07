//! The schema the `workspaces`, `windows` and `wm` services serve: the
//! texts (one per service, in `strand-services-schema`) their stores give
//! `Service::schema()` to replace the builtin schema's provisional stubs
//! (same names, fields, actions and methods),
//! plus what the real services add: `Workspace.active`, `Window.urgent`,
//! and `config_reloaded`'s `failed` (niri's `ConfigLoaded { failed }`;
//! unset from Hyprland and sway, which do not say).

/// The `windows` service's text (and its `Window` record).
pub const WINDOWS_SCHEMA: &str = strand_services_schema::WINDOWS;
/// The `workspaces` service's text (and its `Workspace` record).
pub const WORKSPACES_SCHEMA: &str = strand_services_schema::WORKSPACES;
/// The `wm` service's text.
pub const WM_SCHEMA: &str = strand_services_schema::WM;

#[cfg(test)]
mod tests {
    use super::{WINDOWS_SCHEMA, WM_SCHEMA, WORKSPACES_SCHEMA};

    /// The builtin schema's provisional declarations: the contract.
    const BUILTIN: &str = include_str!("../../../strand-compiler/src/schema/builtin.schema");

    /// The member lines of the block that starts with `head`.
    fn block<'a>(text: &'a str, head: &str) -> Vec<&'a str> {
        let start = text.find(head).unwrap_or_else(|| panic!("no `{head}`"));
        text[start..]
            .lines()
            .skip(1)
            .map(str::trim)
            .take_while(|l| *l != "}")
            .filter(|l| !l.is_empty() && !l.starts_with("//"))
            .collect()
    }

    /// The stubs declare exactly what the services serve (decisions.md,
    /// wave4-wm fixes: a config checked against the bare builtin schema
    /// checks the same as against the served one): the same members, with
    /// the same types and arities, in the same order.
    #[test]
    fn the_stubs_declare_what_the_services_serve() {
        let schema = [WINDOWS_SCHEMA, WORKSPACES_SCHEMA, WM_SCHEMA].concat();
        let schema = schema.as_str();
        for (stub, real) in [
            (
                "provisional record Window key id {",
                "record Window key id {",
            ),
            (
                "provisional record Workspace key id {",
                "record Workspace key id {",
            ),
            ("provisional service windows {", "service windows {"),
            ("provisional service workspaces {", "service workspaces {"),
            ("provisional service wm {", "service wm {"),
        ] {
            assert_eq!(block(BUILTIN, stub), block(schema, real), "{real}");
        }
        assert!(schema.contains("event config_reloaded(failed: bool?)"));
    }
}
