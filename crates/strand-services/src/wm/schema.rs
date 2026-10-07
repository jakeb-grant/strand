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

    fn key(member: &str) -> &str {
        member.split(['(', ':']).next().unwrap_or(member).trim()
    }

    /// Every field the stubs declare is declared alike (same type); every
    /// action, method and event keeps its name.
    #[test]
    fn the_schema_serves_every_provisional_declaration() {
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
            let ours = block(schema, real);
            for member in block(BUILTIN, stub) {
                let callable = ["action ", "fn ", "event "]
                    .iter()
                    .any(|p| member.starts_with(p));
                if callable {
                    assert!(
                        ours.iter().any(|m| key(m) == key(member)),
                        "{real} lacks {member}"
                    );
                } else {
                    assert!(ours.contains(&member), "{real} lacks `{member}`");
                }
            }
        }
        assert!(schema.contains("event config_reloaded(failed: bool?)"));
    }
}
