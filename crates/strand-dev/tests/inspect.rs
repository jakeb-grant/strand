//! (M5 wave 3, S-inspect) Write-back: `strand-dev inspect` writes an
//! edit into the source only as a span edit of a literal, only if the
//! file still hashes to what the running shell compiled, atomically and
//! following symlinks; a read-only target is refused with an `$EDITOR`
//! offer (m5-owner Q11).

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use serde_json::{Value as Json, json};
use strand_dev::inspect::write::{self, Moment, Refusal, Swap, hash, prop_edit, token_edit};

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

/// A node declared in an `else if` branch or a `match` arm is its own
/// element: the edit lands on it, never on the enclosing box's prop of
/// the same name (inspect-r1-1).
#[test]
fn a_node_in_else_if_or_a_match_arm_is_its_own_element() {
    let d = dir("branches");
    let f = d.join("bar.strand");
    let text = "enum Page { a, b }\nstate page = a\nstate x = false\nbar Top {\n    box {\n        width: 100\n        if x {\n        } else if true {\n            box { id: inner; width: 20 }\n        }\n        match page {\n            a => box { id: armed; width: 30 }\n            _ => box { width: 31 }\n        }\n    }\n}\n";
    fs::write(&f, text).unwrap();
    let name = f.display().to_string();
    let mut files = serde_json::Map::new();
    files.insert(name.clone(), Json::String(hash(text.as_bytes())));
    let files = Json::Object(files);
    for (decl, want) in [
        ("box { id: inner; width: 20 }", "20"),
        ("box { id: armed; width: 30 }", "30"),
    ] {
        let start = text.find(decl).unwrap();
        let node = json!({
            "id": "3.0", "kind": "box",
            "origin": {"span": [start, start + decl.len()], "file": name, "line": 1, "col": 1},
        });
        let e = prop_edit(&node, &files, "width", "300").unwrap();
        assert_eq!(&text[e.span.clone()], want, "{decl}");
        let inner = start + decl.find(want).unwrap();
        assert_eq!(e.span, inner..inner + want.len(), "{decl}");
    }
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
/// when it saved the very same bytes: (M6) found after the exchange
/// (`RENAME_EXCHANGE`) in the file it took out of place, and swapped
/// back; and on a filesystem without the exchange, by the check just
/// before the rename.
#[test]
fn a_rename_into_place_between_hash_and_write_is_refused() {
    let d = dir("race");
    let f = d.join("bar.strand");
    fs::write(&f, BAR).unwrap();
    let (node, files) = answer(&f, BAR);
    let edit = prop_edit(&node, &files, "bg", "#a6e3a1").unwrap();
    for swap in [Swap::Exchange, Swap::Rename] {
        for saved in [BAR.replace("width: 40", "width: 50"), BAR.to_string()] {
            fs::write(&f, BAR).unwrap();
            let mut called = false;
            let r = write::write_with(&edit, swap, |m, real| {
                if m == Moment::Before {
                    let tmp = real.with_extension("swp");
                    fs::write(&tmp, &saved).unwrap();
                    fs::rename(&tmp, real).unwrap();
                    called = true;
                }
            });
            assert!(called);
            assert_eq!(r, Err(Refusal::Changed(f.clone())), "{swap:?}");
            assert_eq!(fs::read_to_string(&f).unwrap(), saved, "{swap:?}");
            assert_eq!(left(&d), 1, "{swap:?}");
        }
    }
}

/// The names in `d`.
fn left(d: &Path) -> usize {
    fs::read_dir(d).unwrap().count()
}

/// The names in `d` other than `keep`, with their text.
fn others(d: &Path, keep: &Path) -> Vec<(PathBuf, String)> {
    fs::read_dir(d)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p != keep)
        .map(|p| {
            let text = fs::read_to_string(&p).unwrap();
            (p, text)
        })
        .collect()
}

/// (M6) The write-back race is closed with `renameat2(RENAME_EXCHANGE)`:
/// nothing checked before the swap is trusted after it. An editor that
/// holds the file open writes into it after the last look the write
/// takes at it before it is replaced (`Moment::Window`): with the
/// exchange that is after the swap, and the file swapped out is found
/// changed (its hash) and swapped back, so the editor's text stays, in
/// the same file (inode), and no temporary file is left. With the plain
/// rename (where the filesystem has no exchange) it is after the check,
/// and the write is lost: the window the exchange closes. With no writer
/// the edit is in place and only the file is left.
#[test]
fn a_save_after_the_check_is_swapped_back_and_kept() {
    use std::io::{Seek as _, Write as _};
    use std::os::unix::fs::MetadataExt as _;
    let d = dir("exchange");
    let f = d.join("bar.strand");
    fs::write(&f, BAR).unwrap();
    let (node, files) = answer(&f, BAR);
    let edit = prop_edit(&node, &files, "bg", "#a6e3a1").unwrap();
    let saved = BAR.replace("width: 40", "width: 50");
    let edited = BAR.replace("#f38ba8", "#a6e3a1");
    for swap in [Swap::Exchange, Swap::Rename] {
        fs::write(&f, BAR).unwrap();
        let ino = fs::metadata(&f).unwrap().ino();
        let mut editor = fs::OpenOptions::new().write(true).open(&f).unwrap();
        let mut wrote = false;
        let r = write::write_with(&edit, swap, |m, _| {
            if m == Moment::Window {
                // In place, through the editor's own descriptor.
                editor.set_len(0).unwrap();
                editor.rewind().unwrap();
                editor.write_all(saved.as_bytes()).unwrap();
                editor.sync_all().unwrap();
                wrote = true;
            }
        });
        assert!(wrote, "{swap:?}");
        if swap == Swap::Exchange {
            assert_eq!(r, Err(Refusal::Changed(f.clone())));
            assert_eq!(fs::read_to_string(&f).unwrap(), saved, "kept");
            assert_eq!(fs::metadata(&f).unwrap().ino(), ino, "the editor's file");
        } else {
            assert_eq!(r, Ok(fs::canonicalize(&f).unwrap()));
            assert_eq!(fs::read_to_string(&f).unwrap(), edited, "the save is lost");
        }
        assert_eq!(left(&d), 1, "{swap:?}");
    }
    // Unraced: written, by the exchange.
    fs::write(&f, BAR).unwrap();
    assert_eq!(
        write::write_with(&edit, Swap::Exchange, |_, _| {}),
        Ok(fs::canonicalize(&f).unwrap())
    );
    assert_eq!(fs::read_to_string(&f).unwrap(), edited);
    assert_eq!(left(&d), 1);
}

/// (M6) The file is saved once after the exchange (written in place,
/// found changed) and again before the swap back (`Moment::Back`: a new
/// file renamed over the edit, or the edit written into). Nothing is
/// removed: the latest save is put back in place, the one before it is
/// kept at the write's temporary name, and the refusal says so. That
/// name is the write's own, so the next edit of the file is written.
#[test]
fn a_second_save_before_the_swap_back_is_kept_in_place() {
    let d = dir("twice");
    let f = d.join("bar.strand");
    fs::write(&f, BAR).unwrap();
    let (node, files) = answer(&f, BAR);
    let edit = prop_edit(&node, &files, "bg", "#a6e3a1").unwrap();
    let first = BAR.replace("width: 40", "width: 50");
    let latest = BAR.replace("width: 40", "width: 60");
    for renamed in [true, false] {
        fs::write(&f, BAR).unwrap();
        let mut moments = Vec::new();
        let r = write::write_with(&edit, Swap::Exchange, |m, p| {
            moments.push(m);
            match m {
                Moment::Window => fs::write(p, &first).unwrap(),
                Moment::Back if renamed => {
                    let swp = p.with_extension("swp");
                    fs::write(&swp, &latest).unwrap();
                    fs::rename(&swp, p).unwrap();
                }
                Moment::Back => fs::write(p, &latest).unwrap(),
                Moment::Before | Moment::Again => {}
            }
        });
        assert_eq!(
            moments,
            [Moment::Before, Moment::Window, Moment::Back, Moment::Again]
        );
        assert_eq!(
            fs::read_to_string(&f).unwrap(),
            latest,
            "renamed: {renamed}"
        );
        let kept = others(&d, &f);
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(kept[0].1, first, "the save before it");
        let Err(Refusal::Io { file, why }) = r else {
            panic!("{r:?}");
        };
        assert_eq!(file, f);
        assert!(why.contains("saved more than once"), "{why}");
        assert!(why.contains("the edit is not applied"), "{why}");
        assert!(why.contains(&kept[0].0.display().to_string()), "{why}");
        // The next edit is not blocked by the version kept.
        let (node, files) = answer(&f, &latest);
        let next = prop_edit(&node, &files, "bg", "#a6e3a1").unwrap();
        assert_eq!(write::write(&next), Ok(fs::canonicalize(&f).unwrap()));
        assert_eq!(
            fs::read_to_string(&f).unwrap(),
            latest.replace("#f38ba8", "#a6e3a1")
        );
        fs::remove_file(&kept[0].0).unwrap();
        assert_eq!(left(&d), 1);
    }
}

/// (M6) A swap back that fails (the directory made read-only before
/// it) leaves the edit in place and the editor's save at the write's
/// temporary name, and the refusal says both.
#[test]
fn a_swap_back_that_fails_says_the_edit_is_in_place() {
    if is_root() {
        return;
    }
    let d = dir("noback");
    let f = d.join("bar.strand");
    fs::write(&f, BAR).unwrap();
    let (node, files) = answer(&f, BAR);
    let edit = prop_edit(&node, &files, "bg", "#a6e3a1").unwrap();
    let saved = BAR.replace("width: 40", "width: 50");
    let r = write::write_with(&edit, Swap::Exchange, |m, p| match m {
        Moment::Window => fs::write(p, &saved).unwrap(),
        Moment::Back => {
            fs::set_permissions(p.parent().unwrap(), fs::Permissions::from_mode(0o555)).unwrap()
        }
        Moment::Before | Moment::Again => {}
    });
    fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        fs::read_to_string(&f).unwrap(),
        BAR.replace("#f38ba8", "#a6e3a1")
    );
    let kept = others(&d, &f);
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(kept[0].1, saved);
    let Err(Refusal::Io { why, .. }) = r else {
        panic!("{r:?}");
    };
    assert!(why.contains("the file holds the edit"), "{why}");
    assert!(why.contains(&kept[0].0.display().to_string()), "{why}");
}

/// (M6, edges-r2-2) A third save lands after the swap back brought out
/// the second, before that one is exchanged into place
/// (`Moment::Again`): renamed over the file, or written into it. What
/// the exchange then brings out is checked, not assumed to be the save
/// before: the third, newest save ends in place, not hidden at the
/// temporary name, and the refusal's account of the file holds.
#[test]
fn a_third_save_before_the_latest_goes_in_place_is_not_hidden() {
    let d = dir("thrice");
    let f = d.join("bar.strand");
    fs::write(&f, BAR).unwrap();
    let (node, files) = answer(&f, BAR);
    let edit = prop_edit(&node, &files, "bg", "#a6e3a1").unwrap();
    let first = BAR.replace("width: 40", "width: 50");
    let second = BAR.replace("width: 40", "width: 60");
    let third = BAR.replace("width: 40", "width: 70");
    let rename = |p: &Path, text: &str| {
        let swp = p.with_extension("swp");
        fs::write(&swp, text).unwrap();
        fs::rename(&swp, p).unwrap();
    };
    for renamed in [true, false] {
        fs::write(&f, BAR).unwrap();
        let mut again = 0;
        let r = write::write_with(&edit, Swap::Exchange, |m, p| match m {
            Moment::Window => fs::write(p, &first).unwrap(),
            Moment::Back => rename(p, &second),
            Moment::Again => {
                again += 1;
                if again == 1 {
                    if renamed {
                        rename(p, &third);
                    } else {
                        fs::write(p, &third).unwrap();
                    }
                }
            }
            Moment::Before => {}
        });
        assert_eq!(fs::read_to_string(&f).unwrap(), third, "renamed: {renamed}");
        let kept = others(&d, &f);
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(kept[0].1, second, "the save before the latest");
        let Err(Refusal::Io { why, .. }) = r else {
            panic!("{r:?}");
        };
        assert!(why.contains("the file holds the latest save"), "{why}");
        assert!(why.contains(&kept[0].0.display().to_string()), "{why}");
        fs::remove_file(&kept[0].0).unwrap();
    }
}

/// (M6, edges-r2-3) The swap back fails after the editor renamed a new
/// save over the edit: the refusal says what the file holds then (that
/// save, the edit not applied), not that it holds the edit, so the
/// version kept is not mistaken for one to copy back over it.
#[test]
fn a_failed_swap_back_says_what_the_file_holds() {
    if is_root() {
        return;
    }
    let d = dir("noback-saved");
    let f = d.join("bar.strand");
    fs::write(&f, BAR).unwrap();
    let (node, files) = answer(&f, BAR);
    let edit = prop_edit(&node, &files, "bg", "#a6e3a1").unwrap();
    let saved = BAR.replace("width: 40", "width: 50");
    let latest = BAR.replace("width: 40", "width: 60");
    let r = write::write_with(&edit, Swap::Exchange, |m, p| match m {
        Moment::Window => fs::write(p, &saved).unwrap(),
        Moment::Back => {
            let swp = p.with_extension("swp");
            fs::write(&swp, &latest).unwrap();
            fs::rename(&swp, p).unwrap();
            fs::set_permissions(p.parent().unwrap(), fs::Permissions::from_mode(0o555)).unwrap()
        }
        Moment::Before | Moment::Again => {}
    });
    fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(fs::read_to_string(&f).unwrap(), latest);
    let kept = others(&d, &f);
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(kept[0].1, saved);
    let Err(Refusal::Io { why, .. }) = r else {
        panic!("{r:?}");
    };
    assert!(!why.contains("the file holds the edit"), "{why}");
    assert!(why.contains("the edit is not applied"), "{why}");
    assert!(why.contains("the file holds a save made since"), "{why}");
    assert!(why.contains(&kept[0].0.display().to_string()), "{why}");
}

/// (M6, edges-r2-4) An editor that saves by moving the file to a backup
/// and writing it anew leaves no file for a moment. An exchange in that
/// moment finds no file to swap with (`ENOENT`): the edit is refused as
/// `Changed`, as the check before a plain rename refuses it, not as an
/// opaque I/O error, and nothing of the write is left.
#[test]
fn a_file_missing_at_the_swap_is_refused_as_changed() {
    let d = dir("missing");
    let f = d.join("bar.strand");
    let bak = d.join("bar.strand~");
    fs::write(&f, BAR).unwrap();
    let (node, files) = answer(&f, BAR);
    let edit = prop_edit(&node, &files, "bg", "#a6e3a1").unwrap();
    for swap in [Swap::Auto, Swap::Exchange, Swap::Rename] {
        fs::write(&f, BAR).unwrap();
        let r = write::write_with(&edit, swap, |m, p| {
            if m == Moment::Before {
                fs::rename(p, &bak).unwrap();
            }
        });
        assert_eq!(r, Err(Refusal::Changed(f.clone())), "{swap:?}");
        assert!(!f.exists(), "{swap:?}");
        assert_eq!(fs::read_to_string(&bak).unwrap(), BAR, "{swap:?}");
        assert_eq!(left(&d), 1, "{swap:?}");
        fs::remove_file(&bak).unwrap();
    }
}
