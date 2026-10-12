//! (M5 wave 3, S-inspect) Write-back: `strand-dev inspect` writes an
//! edit into the source only as a span edit of a literal, only if the
//! file still hashes to what the running shell compiled, atomically and
//! following symlinks; a read-only target is refused with an `$EDITOR`
//! offer (m5-owner Q11).

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use serde_json::{Value as Json, json};
use strand_dev::inspect::write::{self, Refusal, hash, prop_edit, token_edit};

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("strand-dev-inspect-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

const BAR: &str = "bar Top {\n    height: 30\n    box {\n        id: target\n        bg: #f38ba8\n        width: 40\n        label: \"hi\"\n        height: $probe.h\n    }\n}\n";

/// An `inspect.node` answer for the inner box of [`BAR`] at `file`.
fn answer(file: &Path, text: &str) -> (Json, Json) {
    let start = text.find("box {").unwrap();
    let end = text.rfind("    }\n").unwrap() + 5;
    let name = file.display().to_string();
    let node = json!({
        "id": "3.0", "kind": "box",
        "origin": {"span": [start, end], "file": name, "line": 3, "col": 5},
    });
    let mut files = serde_json::Map::new();
    files.insert(name, Json::String(hash(text.as_bytes())));
    (node, Json::Object(files))
}

/// One span is edited: the prop's literal and nothing else, on the
/// node's own element (not the surface's `height`).
#[test]
fn one_span_is_edited() {
    let d = dir("span");
    let f = d.join("bar.strand");
    fs::write(&f, BAR).unwrap();
    let (node, files) = answer(&f, BAR);
    let edit = prop_edit(&node, &files, "bg", "#a6e3a1").unwrap();
    assert_eq!(write::write(&edit).unwrap(), fs::canonicalize(&f).unwrap());
    assert_eq!(
        fs::read_to_string(&f).unwrap(),
        BAR.replace("#f38ba8", "#a6e3a1")
    );
    // Each kind of literal, against the new text.
    let now = fs::read_to_string(&f).unwrap();
    let (node, files) = answer(&f, &now);
    let e = prop_edit(&node, &files, "width", "-12").unwrap();
    assert_eq!(&now[e.span.clone()], "40");
    let e = prop_edit(&node, &files, "label", "\"bye\"").unwrap();
    assert_eq!(&now[e.span.clone()], "\"hi\"");
    // Not a literal: refused, the file untouched.
    assert!(matches!(
        prop_edit(&node, &files, "height", "8"),
        Err(Refusal::NotLiteral(_))
    ));
    assert!(matches!(
        prop_edit(&node, &files, "bg", "$probe.c"),
        Err(Refusal::NotLiteral(_))
    ));
    assert!(matches!(
        prop_edit(&node, &files, "padding", "4"),
        Err(Refusal::NotFound(_))
    ));
    assert_eq!(fs::read_to_string(&f).unwrap(), now);
    // No temporary file is left beside it.
    assert_eq!(fs::read_dir(&d).unwrap().count(), 1);
}

/// A token's literal is written at its definition, in whichever file
/// the running shell compiled it from; a palette's is refused.
#[test]
fn a_token_is_written_at_its_definition() {
    let d = dir("token");
    let theme = "tokens base {\n    probe {\n        h: 24\n        c: #112233\n    }\n}\ncomponent Chip tokens { pad: 4 } { box { } }\n";
    let (bar, th) = (d.join("bar.strand"), d.join("theme.strand"));
    fs::write(&bar, BAR).unwrap();
    fs::write(&th, theme).unwrap();
    let files = json!({
        bar.display().to_string(): hash(BAR.as_bytes()),
        th.display().to_string(): hash(theme.as_bytes()),
    });
    let e = token_edit(&files, "probe.h", "tokens base", "32").unwrap();
    write::write(&e).unwrap();
    assert_eq!(
        fs::read_to_string(&th).unwrap(),
        theme.replace("h: 24", "h: 32")
    );
    let theme2 = fs::read_to_string(&th).unwrap();
    let files = json!({ th.display().to_string(): hash(theme2.as_bytes()) });
    let e = token_edit(&files, "Chip.pad", "component Chip", "6").unwrap();
    assert_eq!(&theme2[e.span.clone()], "4");
    for origin in ["palette:catppuccin", "base", "overlay"] {
        assert!(matches!(
            token_edit(&files, "accent", origin, "#fff"),
            Err(Refusal::NotLiteral(_))
        ));
    }
    assert!(matches!(
        token_edit(&files, "probe.x", "tokens base", "1"),
        Err(Refusal::NotFound(_))
    ));
    // The defining file changed underneath: refused as changed.
    fs::write(&th, theme2.replace("pad: 4", "pad: 5")).unwrap();
    assert_eq!(
        token_edit(&files, "Chip.pad", "component Chip", "6"),
        Err(Refusal::Changed(th.clone()))
    );
}

/// A file changed since the shell compiled it is never overwritten.
#[test]
fn a_file_changed_underneath_is_refused() {
    let d = dir("changed");
    let f = d.join("bar.strand");
    fs::write(&f, BAR).unwrap();
    let (node, files) = answer(&f, BAR);
    let edit = prop_edit(&node, &files, "bg", "#a6e3a1").unwrap();
    let changed = BAR.replace("width: 40", "width: 41");
    fs::write(&f, &changed).unwrap();
    assert_eq!(write::write(&edit), Err(Refusal::Changed(f.clone())));
    assert_eq!(fs::read_to_string(&f).unwrap(), changed);
    // Computing a new edit against it is refused too.
    assert!(matches!(
        prop_edit(&node, &files, "bg", "#a6e3a1"),
        Err(Refusal::Changed(_))
    ));
}

/// A symlinked config stays a symlink: its target is edited.
#[test]
fn a_symlink_is_followed() {
    let d = dir("link");
    fs::create_dir_all(d.join("dotfiles")).unwrap();
    let real = d.join("dotfiles/bar.strand");
    let link = d.join("bar.strand");
    fs::write(&real, BAR).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let (node, files) = answer(&link, BAR);
    let edit = prop_edit(&node, &files, "bg", "#a6e3a1").unwrap();
    assert_eq!(
        write::write(&edit).unwrap(),
        fs::canonicalize(&real).unwrap()
    );
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_to_string(&real).unwrap(),
        BAR.replace("#f38ba8", "#a6e3a1")
    );
}

/// A read-only target (a Nix store file) is refused with the reason and
/// an offer to open it in `$EDITOR`; so is one in a read-only directory.
#[test]
fn a_read_only_target_is_refused_offering_editor() {
    let d = dir("ro");
    let f = d.join("bar.strand");
    fs::write(&f, BAR).unwrap();
    let (node, files) = answer(&f, BAR);
    let edit = prop_edit(&node, &files, "bg", "#a6e3a1").unwrap();
    fs::set_permissions(&f, fs::Permissions::from_mode(0o444)).unwrap();
    let r = write::write(&edit).unwrap_err();
    assert!(matches!(r, Refusal::ReadOnly { .. }), "{r:?}");
    assert_eq!(r.editor_offer(), Some(f.as_path()));
    assert!(r.to_string().contains("$EDITOR"), "{r}");
    assert_eq!(fs::read_to_string(&f).unwrap(), BAR);
    // A writable file in a read-only directory (the store's layout when
    // the file is a copy): the atomic rename cannot happen there.
    fs::set_permissions(&f, fs::Permissions::from_mode(0o644)).unwrap();
    fs::set_permissions(&d, fs::Permissions::from_mode(0o555)).unwrap();
    let r = write::write(&edit);
    fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
    // Root ignores permissions; the container runs as the owner's uid.
    if !is_root() {
        let r = r.unwrap_err();
        assert!(matches!(r, Refusal::ReadOnly { .. }), "{r:?}");
        assert_eq!(r.editor_offer(), Some(f.as_path()));
        assert_eq!(fs::read_to_string(&f).unwrap(), BAR);
    }
}

fn is_root() -> bool {
    fs::metadata("/proc/self")
        .map(|m| std::os::unix::fs::MetadataExt::uid(&m) == 0)
        .unwrap_or(false)
}

/// An editor's atomic save (a new file renamed into place) between the
/// hash check and the write is refused, keeping the editor's text, even
/// when it saved the very same bytes.
#[test]
fn a_rename_into_place_between_hash_and_write_is_refused() {
    let d = dir("race");
    let f = d.join("bar.strand");
    fs::write(&f, BAR).unwrap();
    let (node, files) = answer(&f, BAR);
    let edit = prop_edit(&node, &files, "bg", "#a6e3a1").unwrap();
    for saved in [BAR.replace("width: 40", "width: 50"), BAR.to_string()] {
        fs::write(&f, BAR).unwrap();
        let mut called = false;
        let r = write::write_with(&edit, |real| {
            let tmp = real.with_extension("swp");
            fs::write(&tmp, &saved).unwrap();
            fs::rename(&tmp, real).unwrap();
            called = true;
        });
        assert!(called);
        assert_eq!(r, Err(Refusal::Changed(f.clone())));
        assert_eq!(fs::read_to_string(&f).unwrap(), saved);
        let left: Vec<_> = fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left.len(), 1, "{left:?}");
    }
}
