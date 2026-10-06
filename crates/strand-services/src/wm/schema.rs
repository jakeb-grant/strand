//! The schema the `workspaces`, `windows` and `wm` services serve: the
//! text their stores give `Service::schema()` to replace the builtin
//! schema's provisional stubs (same names, fields, actions and methods),
//! plus what the real services add: `Workspace.active`, `Window.urgent`,
//! and `config_reloaded`'s `failed` (niri's `ConfigLoaded { failed }`;
//! unset from Hyprland and sway, which do not say).

/// See the module docs.
pub const SCHEMA: &str = r#"
/// A toplevel window, from `ext-foreign-toplevel-list`, with what the
/// compositor's IPC adds.
record Window key id {
  /// Its identity: the compositor's id when its IPC is used, else the
  /// protocol's identifier.
  id: text
  /// Its title: `windows.focused?.title`.
  title: text
  /// The app's id, such as `firefox`.
  app_id: text
  /// An icon name for its app.
  icon: text
  /// The workspace it is on, when known.
  workspace: int?
  /// It has focus.
  focused: bool
  /// It is minimised.
  minimized: bool
  /// It is fullscreen.
  fullscreen: bool
  /// It asks for attention.
  urgent: bool
  /// Raises and focuses it.
  action focus()
  /// Asks it to close.
  action close()
  /// Minimises it.
  action minimize()
}

/// A workspace, from `ext-workspace-v1`, keyed by `id`.
record Workspace key id {
  /// Its identity.
  id: int
  /// Its name.
  name: text
  /// It is the focused workspace (at most one is; with the standard
  /// protocols alone and several screens, none is).
  focused: bool
  /// It is shown on its screen (one per screen).
  active: bool
  /// It holds windows.
  occupied: bool
  /// A window on it asks for attention.
  urgent: bool
  /// The name of the screen it is on.
  screen: text
  /// Its windows, keyed by `id`.
  windows: [Window]
  /// Switches to it: `on click { ws.focus() }`.
  action focus()
}

/// Toplevel windows, from `ext-foreign-toplevel-list`.
service windows {
  /// The focused window, if any: `windows.focused?.title`.
  focused: Window?
  /// Every window, keyed by `id`.
  all: [Window]
}

/// Workspaces, from `ext-workspace-v1`; Hyprland, niri and sway IPC fill
/// in what the protocol does not cover yet.
service workspaces {
  /// Every workspace, keyed by `id`.
  all: [Workspace]
  /// The focused workspace, if any.
  focused: Workspace?
  /// The workspaces on `screen`: `for ws in workspaces.on(screen)`.
  fn on(screen: Screen) -> [Workspace]
}

/// The compositor.
service wm {
  /// Its name, such as `Hyprland`, `niri` or `sway` (the desktop's name
  /// when only the standard protocols serve it).
  name: text
  /// The compositor reloaded its config; `failed` says whether the new
  /// config failed to load, when the compositor tells (niri).
  event config_reloaded(failed: bool?)
}
"#;

#[cfg(test)]
mod tests {
    use super::SCHEMA;

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
            let ours = block(SCHEMA, real);
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
        assert!(SCHEMA.contains("event config_reloaded(failed: bool?)"));
    }
}
