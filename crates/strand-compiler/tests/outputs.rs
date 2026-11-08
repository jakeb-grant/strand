//! (M5) `file` outputs (decisions.md m5-tools, "the `file` element"):
//! the top-level `file` parses (every other top-level element stays
//! misplaced), the content memo is pulled after the diff and handed out
//! only when it changes, a template file is read and re-read on an edit,
//! a pending wallpaper writes nothing, `.light()` holds the other mode
//! while mounted, and one path from two places is an error naming both.

use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use strand_compiler::SourceMap;
use strand_compiler::instantiate::{Instance, OutputChange, OutputContent, Storage};
use strand_compiler::reconcile::Build;
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_core::Runtime;

struct Shell {
    rt: Runtime,
    host: Rc<SchemaHost>,
    inst: Instance,
}

fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("strand-outputs-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn build(files: &[(&str, &str)]) -> Result<Build, String> {
    let mut map = SourceMap::new();
    for (n, t) in files {
        map.add(*n, t.to_string());
    }
    let shown = map.clone();
    Build::compile(None, map).map_err(|d| {
        strand_compiler::diagnostic::render(&d, &shown, strand_compiler::diagnostic::Style::Plain)
    })
}

fn boot(files: &[(&str, &str)], dir: &std::path::Path) -> Shell {
    let b = build(files).unwrap_or_else(|e| panic!("{e}"));
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &b.program.types));
    let screen = host.record(
        "Screen",
        &[
            ("id", Value::text("Mock | DP-1 | Display")),
            ("name", Value::text("DP-1")),
        ],
    );
    host.set(&rt, "screens.all", Value::list(vec![screen]))
        .unwrap();
    let inst = Instance::from_build(
        &rt,
        &b,
        host.clone(),
        Storage::in_dirs(dir.join("state"), dir),
    );
    let shell = Shell { rt, host, inst };
    shell.flush();
    shell
}

impl Shell {
    fn flush(&self) {
        let u = self.inst.flush();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
    }

    fn take(&self) -> Vec<OutputChange> {
        self.inst.take_outputs()
    }

    fn dark(&self, on: bool) {
        self.host
            .set(&self.rt, "system.dark", Value::Bool(on))
            .unwrap();
        self.flush();
    }
}

fn written(c: &OutputChange) -> String {
    match &c.content {
        OutputContent::Write { bytes, .. } => String::from_utf8(bytes.clone()).unwrap(),
        other => panic!("not written: {other:?}"),
    }
}

fn failed(c: &OutputChange) -> String {
    match &c.content {
        OutputContent::Failed { diags, .. } => format!("{diags:?}"),
        _ => panic!("not failed: {c:?}"),
    }
}

#[test]
fn only_file_is_allowed_at_the_top_level() {
    let ok = build(&[("a.strand", "file \"/tmp/x\" { text: $accent }\n")]);
    assert!(ok.is_ok(), "{ok:?}");
    let err = build(&[("a.strand", "row { }\n")]).unwrap_err();
    assert!(err.contains("syntax::misplaced"), "{err}");
    let err = build(&[("a.strand", "file \"/tmp/x\"\n")]).unwrap_err();
    assert!(err.contains("needs `template:` or `text:`"), "{err}");
}

/// Boot hands out each output once; a tick that changes nothing it
/// reads, and a flip that changes nothing it writes, hand out nothing; a
/// change does, once.
#[test]
fn an_output_is_handed_out_when_its_text_changes() {
    let dir = temp_dir("changes");
    let src = "export state label = \"a\"\n\
               file \"out/accent.txt\" { text: $accent; run: [\"true\"] }\n\
               file \"out/label.txt\" { text: $palette.dark }\n\
               permit exec\n\
               bar Top { text label }\n";
    let shell = boot(&[("a.strand", src)], &dir);
    let first = shell.take();
    assert_eq!(first.len(), 2, "{first:?}");
    let accent = first
        .iter()
        .find(|c| c.path == Some(dir.join("out/accent.txt")))
        .unwrap();
    assert!(written(accent).starts_with('#'));
    assert!(matches!(&accent.content, OutputContent::Write { run: Some(r), .. } if r == &["true"]));
    assert!(shell.take().is_empty(), "nothing changed");
    // A tick that reads nothing the outputs read.
    shell.inst.set("a.label", Value::text("b")).unwrap();
    shell.flush();
    assert!(shell.take().is_empty());
    // The portal flips: both change.
    shell.dark(true);
    let flipped = shell.take();
    assert_eq!(flipped.len(), 2, "{flipped:?}");
    assert!(flipped.iter().any(|c| written(c) == "true"));
    assert!(shell.take().is_empty());
    // Back and forth in one tick: no change, nothing handed out.
    shell
        .host
        .set(&shell.rt, "system.dark", Value::Bool(false))
        .unwrap();
    shell
        .host
        .set(&shell.rt, "system.dark", Value::Bool(true))
        .unwrap();
    shell.flush();
    assert!(shell.take().is_empty());
}

/// A template file beside the config is read, watched as an import, and
/// read again when the watcher reports it changed; a broken edit is a
/// failure located in the template, and the fix writes again.
#[test]
fn a_template_file_is_read_and_reloaded() {
    let dir = temp_dir("template");
    std::fs::write(dir.join("c.tmpl"), "--accent: {{ $accent }};\n").unwrap();
    let src = "file \"out/c.css\" { template: \"c.tmpl\" }\n";
    let shell = boot(&[("a.strand", src)], &dir);
    let c = shell.take();
    assert_eq!(c.len(), 1);
    assert!(written(&c[0]).starts_with("--accent: #"));
    assert!(shell.inst.theme_files().1.contains(&dir.join("c.tmpl")));
    std::fs::write(dir.join("c.tmpl"), "--fg: {{ $fg }};\n").unwrap();
    assert!(shell.inst.theme_files_changed(&[dir.join("c.tmpl")]));
    shell.flush();
    let c = shell.take();
    assert!(written(&c[0]).starts_with("--fg: #"), "{c:?}");
    std::fs::write(dir.join("c.tmpl"), "{{ $acent }}\n").unwrap();
    assert!(shell.inst.theme_files_changed(&[dir.join("c.tmpl")]));
    shell.flush();
    let c = shell.take();
    let why = failed(&c[0]);
    assert!(why.contains("template::unknown_token"), "{why}");
    assert!(
        matches!(&c[0].content, OutputContent::Failed { template: Some((n, _)), .. } if n.ends_with("c.tmpl"))
    );
}

/// While the wallpaper quantises, nothing is handed out; once it
/// resolves the output is written with its palette.
#[test]
fn a_pending_wallpaper_writes_nothing() {
    let dir = temp_dir("pending");
    let img = image::RgbImage::from_fn(64, 64, |x, _| {
        image::Rgb(if x < 48 {
            [30, 90, 200]
        } else {
            [240, 200, 40]
        })
    });
    img.save(dir.join("wall.png")).unwrap();
    let src = "use palette material(image: \"wall.png\", dark: true) ?? material(seed: #ff0000)\n\
               file \"out/a.txt\" { text: $palette.source }\n";
    let shell = boot(&[("a.strand", src)], &dir);
    let first = shell.take();
    assert!(first.is_empty(), "written while pending: {first:?}");
    let theme = shell.inst.theme().unwrap();
    assert!(theme.wait_images(&shell.rt, Duration::from_secs(30)));
    shell.flush();
    let c = shell.take();
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(written(&c[0]), "wallpaper");
}

/// (build-ready) A `text:` that reads the scope and, through it, a
/// token (inside `join` or `hex`, or a `let` another reader has already
/// worked out) waits for the palette as a template does, so a generated
/// theme file is never written with the fallback's colours first; a text
/// that reads no token is written at once.
#[test]
fn a_scope_text_reading_a_token_waits_for_the_palette() {
    let dir = temp_dir("text-pending");
    let img = image::RgbImage::from_fn(64, 64, |x, _| {
        image::Rgb(if x < 48 {
            [30, 90, 200]
        } else {
            [240, 200, 40]
        })
    });
    img.save(dir.join("wall.png")).unwrap();
    let src = "use palette material(image: \"wall.png\", dark: true) ?? material(seed: #ff0000)\n\
               export let fg = hex($fg)\n\
               export state key = \"fg=\"\n\
               bar Top { text fg }\n\
               file \"out/join.txt\" { text: join(\"\", key, hex($fg)) }\n\
               file \"out/let.txt\" { text: join(\"\", key, fg) }\n\
               file \"out/plain.txt\" { text: join(\"\", key, 1) }\n";
    let shell = boot(&[("a.strand", src)], &dir);
    let first = shell.take();
    assert_eq!(first.len(), 1, "written while pending: {first:?}");
    assert_eq!(first[0].path, Some(dir.join("out/plain.txt")), "{first:?}");
    assert_eq!(written(&first[0]), "fg=1");
    let fallback = shell.inst.get("a.fg").unwrap();
    let theme = shell.inst.theme().unwrap();
    assert!(theme.wait_images(&shell.rt, Duration::from_secs(30)));
    shell.flush();
    let real = shell.inst.get("a.fg").unwrap();
    assert_ne!(fallback, real, "the wallpaper's palette is another");
    let c = shell.take();
    assert_eq!(c.len(), 2, "{c:?}");
    let want = format!("fg={}", real.as_text().unwrap());
    for o in &c {
        assert_eq!(written(o), want, "{o:?}");
    }
}

/// An output reading `.light()` holds the other mode while it is
/// mounted; unmounted, the hold goes.
#[test]
fn light_holds_the_other_mode_while_mounted() {
    let dir = temp_dir("hold");
    let src = "export state on = true\n\
               bar Top { if on { file \"out/l.txt\" { text: $surface.light() } } }\n\
               file \"out/c.txt\" { text: $surface }\n";
    let shell = boot(&[("a.strand", src)], &dir);
    shell.dark(true);
    let c = shell.take();
    assert_eq!(c.len(), 2);
    let light = c
        .iter()
        .find(|c| c.path == Some(dir.join("out/l.txt")))
        .unwrap();
    let current = c
        .iter()
        .find(|c| c.path == Some(dir.join("out/c.txt")))
        .unwrap();
    assert_ne!(written(light), written(current));
    assert_eq!(shell.inst.other_mode_holders(), 1);
    shell.inst.set("a.on", Value::Bool(false)).unwrap();
    shell.flush();
    assert!(shell.take().is_empty());
    assert_eq!(shell.inst.other_mode_holders(), 0);
    assert_eq!(shell.inst.output_count(), 1);
}

/// One path from two places: both fail, naming each other.
#[test]
fn one_path_from_two_places_is_an_error() {
    let dir = temp_dir("twice");
    let src = "file \"out/x\" { text: $accent }\nfile \"out/x\" { text: $fg }\n";
    let shell = boot(&[("a.strand", src)], &dir);
    let c = shell.take();
    assert_eq!(c.len(), 2);
    for o in &c {
        let why = failed(o);
        assert!(
            why.contains("file::twice") && why.contains("and here"),
            "{why}"
        );
    }
}

/// `when` changes a `file`'s props (spec.md 9.7), its condition an
/// ordinary expression in the enclosing scope; source order wins, as for
/// any element's props.
#[test]
fn when_changes_a_files_props() {
    let dir = temp_dir("when");
    let src = "export state compact = false\n\
               export state late = false\n\
               file \"out/accent.txt\" { text: $accent }\n\
               file \"out/fg.txt\" { text: $fg }\n\
               file \"out/x.txt\" { text: $accent; when compact { text: $fg } }\n\
               file \"out/y.txt\" { when late { text: $fg }; text: $accent }\n\
               bar Top { for p in [\"out/n1.txt\", \"out/n2.txt\"] key p { file p { text: $accent; when p == \"out/n2.txt\" { text: $fg } } } }\n";
    let shell = boot(&[("a.strand", src)], &dir);
    let text = |c: &[OutputChange], name: &str| {
        written(
            c.iter()
                .find(|c| c.path == Some(dir.join(name)))
                .unwrap_or_else(|| panic!("{name}: {c:?}")),
        )
    };
    let c = shell.take();
    let (accent, fg) = (text(&c, "out/accent.txt"), text(&c, "out/fg.txt"));
    assert_ne!(accent, fg);
    assert_eq!(text(&c, "out/x.txt"), accent);
    assert_eq!(text(&c, "out/y.txt"), accent);
    // A loop variable in the condition: each mount its own.
    assert_eq!(text(&c, "out/n1.txt"), accent);
    assert_eq!(text(&c, "out/n2.txt"), fg);
    shell.inst.set("a.compact", Value::Bool(true)).unwrap();
    shell.inst.set("a.late", Value::Bool(true)).unwrap();
    shell.flush();
    let c = shell.take();
    assert_eq!(c.len(), 1, "the own prop after the `when` wins: {c:?}");
    assert_eq!(text(&c, "out/x.txt"), fg);
    shell.inst.set("a.compact", Value::Bool(false)).unwrap();
    shell.flush();
    assert_eq!(text(&shell.take(), "out/x.txt"), accent);
}

/// A `when` keeps the `file`'s source kind and holds props only.
#[test]
fn a_when_keeps_the_source_kind() {
    let err = build(&[(
        "a.strand",
        "export state on = false\nfile \"/tmp/x\" { text: $accent; when on { template: \"t.tmpl\" } }\n",
    )])
    .unwrap_err();
    assert!(
        err.contains("check::file_source") && err.contains("gives `template:`"),
        "{err}"
    );
    let err = build(&[(
        "a.strand",
        "export state on = false\nfile \"/tmp/x\" { text: $accent; when on { width: 3 } }\n",
    )])
    .unwrap_err();
    assert!(err.contains("has no prop `width`"), "{err}");
    let err = build(&[(
        "a.strand",
        "file \"/tmp/x\" { text: $accent; when 3 { text: $fg } }\n",
    )])
    .unwrap_err();
    assert!(err.contains("`when`"), "{err}");
}

/// A `text:` reads tokens the config has: an unknown one is a check
/// error, not a run-time surprise.
#[test]
fn a_text_reads_known_tokens() {
    let err = build(&[("a.strand", "file \"/tmp/x\" { text: $acent }\n")]).unwrap_err();
    assert!(
        err.contains("check::unknown_token") && err.contains("$accent"),
        "{err}"
    );
    let ok = build(&[(
        "a.strand",
        "file \"/tmp/x\" { text: join(\" \", $fg.muted, $red, $surface.light() ?? $surface, $palette.anything ?? $accent) }\n",
    )]);
    assert!(ok.is_ok(), "{ok:?}");
}

/// `strand check` checks literal template paths: the file is there, its
/// holes compile and read known tokens; a computed path is left to run
/// time.
#[test]
fn literal_template_paths_are_checked() {
    let dir = temp_dir("check-templates");
    std::fs::write(dir.join("good.tmpl"), "{{ $accent }} {{ $fg.muted }}\n").unwrap();
    std::fs::write(dir.join("typo.tmpl"), "a\n  {{ $acent }}\n").unwrap();
    std::fs::write(dir.join("broken.tmpl"), "{{ $accent \n").unwrap();
    let src = "export state which = \"x.tmpl\"\n\
               file \"out/a\" { template: \"good.tmpl\" }\n\
               file \"out/b\" { template: \"typo.tmpl\" }\n\
               file \"out/c\" { template: \"broken.tmpl\" }\n\
               file \"out/d\" { template: \"missing.tmpl\" }\n\
               file \"out/e\" { template: which }\n";
    let mut map = SourceMap::new();
    map.add("a.strand", src.to_string());
    let compiled = strand_compiler::compile(&map);
    assert_eq!(compiled.errors(), 0, "{:?}", compiled.diagnostics);
    let diags = strand_compiler::check::files::templates(&compiled.program, Some(&dir));
    let shown: Vec<String> = diags.iter().map(|d| d.message.clone()).collect();
    assert_eq!(diags.len(), 3, "{shown:#?}");
    assert!(diags.iter().all(|d| d.code == "check::file_template"));
    assert!(
        shown[0].contains("typo.tmpl:2:6") && shown[0].contains("$acent"),
        "{shown:#?}"
    );
    assert!(shown[1].contains("broken.tmpl:1:1"), "{shown:#?}");
    assert!(shown[2].contains("missing.tmpl"), "{shown:#?}");
    // Without a config directory a relative path is not checked.
    assert!(strand_compiler::check::files::templates(&compiled.program, None).is_empty());
}

/// A failing current palette fails the output (nothing written), even
/// though the screen falls back to the built-in palette.
#[test]
fn a_failing_palette_fails_the_output() {
    let dir = temp_dir("failing");
    let src = "use palette import(\"catppuccin:nope\")\n\
               file \"out/a.txt\" { text: $accent }\n";
    let b = build(&[("a.strand", src)]).unwrap();
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &b.program.types));
    let inst = Instance::from_build(&rt, &b, host, Storage::in_dirs(dir.join("state"), &dir));
    let _ = inst.flush();
    assert!(inst.tokens_now().is_ok(), "the screen has a table");
    let c = inst.take_outputs();
    assert_eq!(c.len(), 1, "{c:?}");
    let why = failed(&c[0]);
    assert!(why.contains("no palette") && why.contains("nope"), "{why}");
}

fn targets(shell: &Shell) -> Vec<PathBuf> {
    let mut t = Vec::new();
    shell.inst.output_targets(|p| t.push(p.to_path_buf()));
    t.sort();
    t
}

/// (Review round 2, item 1) The targets the writer keeps: a removed
/// `file`, one whose path changed and one that fails leave them, so
/// nothing is still tried there (spec.md 9.7, "Removing a `file` stops
/// its updates").
#[test]
fn a_removed_or_failed_file_has_no_target() {
    let dir = temp_dir("targets");
    let src = "export state on = true\n\
               export state name = \"a\"\n\
               export state broken = false\n\
               bar Top { if on { file \"out/l.txt\" { text: $accent } } }\n\
               file join(\"\", \"out/\", name, \".txt\") { text: $fg }\n\
               file \"out/b.txt\" { template: broken ? \"missing.tmpl\" : \"b.tmpl\" }\n";
    std::fs::write(dir.join("b.tmpl"), "{{ $fg }}\n").unwrap();
    let shell = boot(&[("a.strand", src)], &dir);
    assert!(targets(&shell).is_empty(), "nothing handed out yet");
    assert_eq!(shell.take().len(), 3);
    let at = |n: &str| dir.join("out").join(n);
    assert_eq!(targets(&shell), vec![at("a.txt"), at("b.txt"), at("l.txt")]);
    // Removed: its scope goes.
    shell.inst.set("a.on", Value::Bool(false)).unwrap();
    shell.flush();
    let _ = shell.take();
    assert_eq!(targets(&shell), vec![at("a.txt"), at("b.txt")]);
    // A new path: the old one is no target.
    shell.inst.set("a.name", Value::text("c")).unwrap();
    shell.flush();
    assert_eq!(shell.take().len(), 1);
    assert_eq!(targets(&shell), vec![at("b.txt"), at("c.txt")]);
    // Failing: no target until it renders again.
    shell.inst.set("a.broken", Value::Bool(true)).unwrap();
    shell.flush();
    let c = shell.take();
    assert_eq!(c.len(), 1);
    let _ = failed(&c[0]);
    assert_eq!(targets(&shell), vec![at("c.txt")]);
}

/// Boots `files` with `before` run on the mock host first (a service
/// set not ready, say).
fn boot_with(
    files: &[(&str, &str)],
    dir: &std::path::Path,
    before: impl FnOnce(&Runtime, &SchemaHost),
) -> Shell {
    let b = build(files).unwrap_or_else(|e| panic!("{e}"));
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &b.program.types));
    before(&rt, &host);
    let inst = Instance::from_build(
        &rt,
        &b,
        host.clone(),
        Storage::in_dirs(dir.join("state"), dir),
    );
    let shell = Shell { rt, host, inst };
    shell.flush();
    shell
}

/// The one change for `name`, if any.
fn change<'c>(
    c: &'c [OutputChange],
    dir: &std::path::Path,
    name: &str,
) -> Option<&'c OutputChange> {
    c.iter().find(|c| c.path == Some(dir.join(name)))
}

/// (live-files, a) A `text:` that reads the scope (a `state`, a settings
/// field, a `let`, a `fn` and a service) is an ordinary expression,
/// written again on each change of what it reads.
#[test]
fn a_text_reads_the_files_scope() {
    let dir = temp_dir("scope");
    let src = "export state label = \"a\"\n\
               export state prefs from \"prefs.toml\" { size: int = 11 }\n\
               let twice = prefs.size * 2\n\
               fn shout(s: text) -> text { s.upper() }\n\
               file \"out/a.txt\" { text: shout(label) + \" \" + num(twice) + \" \" + (media.title ?? \"-\") }\n";
    let shell = boot(&[("a.strand", src)], &dir);
    let next = |shell: &Shell| {
        let c = shell.take();
        assert_eq!(c.len(), 1, "{c:?}");
        written(&c[0])
    };
    assert_eq!(next(&shell), "A 22 -");
    shell.inst.set("a.label", Value::text("b")).unwrap();
    shell.flush();
    assert_eq!(next(&shell), "B 22 -");
    shell.inst.set("a.prefs.size", Value::int(12)).unwrap();
    shell.flush();
    assert_eq!(next(&shell), "B 24 -");
    shell
        .host
        .set(&shell.rt, "media.title", Value::text("song"))
        .unwrap();
    shell.flush();
    assert_eq!(next(&shell), "B 24 song");
    // Values by 9.8's rule: a number shortest, a bool, a colour as hex.
    let src = "export state n = 1.5\n\
               file \"out/n.txt\" { text: n * 2 }\n\
               file \"out/b.txt\" { text: n > 1 }\n\
               file \"out/c.txt\" { text: n > 1 ? #ff8800 : #000000 }\n\
               file \"out/t.txt\" { text: n > 1 ? $accent : $fg }\n\
               file \"out/x.txt\" { text: [n] }\n";
    let shell = boot(&[("a.strand", src)], &dir);
    let c = shell.take();
    assert_eq!(written(change(&c, &dir, "out/n.txt").unwrap()), "3");
    assert_eq!(written(change(&c, &dir, "out/b.txt").unwrap()), "true");
    assert_eq!(written(change(&c, &dir, "out/c.txt").unwrap()), "#ff8800");
    // A token from the scope: the current table's value, as `$accent`.
    let accent = boot(
        &[("a.strand", "file \"out/t.txt\" { text: $accent }\n")],
        &dir,
    )
    .take();
    assert_eq!(
        written(change(&c, &dir, "out/t.txt").unwrap()),
        written(&accent[0])
    );
    let why = failed(change(&c, &dir, "out/x.txt").unwrap());
    assert!(why.contains("a list has no one text form"), "{why}");
}

/// (live-files, b) `text: null` writes nothing and runs nothing: the
/// file is left as it is, and a later text writes again.
#[test]
fn a_null_text_leaves_the_file() {
    let dir = temp_dir("null");
    let src = "export state on = true\n\
               permit exec\n\
               file \"out/b.txt\" { text: on ? \"x\" : null; run: [\"true\"] }\n\
               file \"out/n.txt\" { text: null; run: [\"true\"] }\n";
    let shell = boot(&[("a.strand", src)], &dir);
    let c = shell.take();
    assert_eq!(written(change(&c, &dir, "out/b.txt").unwrap()), "x");
    let n = change(&c, &dir, "out/n.txt").unwrap();
    assert_eq!(n.content, OutputContent::Leave, "{n:?}");
    assert_eq!(targets(&shell), vec![dir.join("out/b.txt")]);
    shell.inst.set("a.on", Value::Bool(false)).unwrap();
    shell.flush();
    let c = shell.take();
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(c[0].content, OutputContent::Leave, "no write, no run");
    assert!(
        targets(&shell).is_empty(),
        "nothing is tried there any more"
    );
    shell.inst.set("a.on", Value::Bool(true)).unwrap();
    shell.flush();
    assert_eq!(written(&shell.take()[0]), "x");
}

/// (live-files, c) A service the text reads that has not answered: the
/// file waits, even with `??`, and is written once it has.
#[test]
fn a_file_waits_for_the_services_it_reads() {
    let dir = temp_dir("waits");
    let src = "file \"out/c.txt\" { text: media.title ?? \"none\" }\n\
               file \"out/t.txt\" { text: $accent }\n";
    let shell = boot_with(&[("a.strand", src)], &dir, |rt, host| {
        host.set_ready(rt, "media", false).unwrap();
    });
    let c = shell.take();
    let waiting = change(&c, &dir, "out/c.txt").unwrap();
    assert_eq!(
        waiting.content,
        OutputContent::Waiting {
            services: vec!["media".into()]
        }
    );
    // A theme text reads no service: it is written.
    let _ = written(change(&c, &dir, "out/t.txt").unwrap());
    shell.flush();
    assert!(shell.take().is_empty(), "still waiting: nothing new");
    shell.host.set_ready(&shell.rt, "media", true).unwrap();
    shell.flush();
    let c = shell.take();
    assert_eq!(c.len(), 1);
    assert_eq!(written(&c[0]), "none");
}

/// (live-files, d) A service that says its values no longer hold and then
/// clears them: the file waits, and the cleared values are never written.
#[test]
fn an_unready_service_clearing_writes_nothing() {
    let dir = temp_dir("unready");
    let src = "file \"out/d.txt\" { text: media.title ?? \"none\" }\n";
    let shell = boot(&[("a.strand", src)], &dir);
    shell
        .host
        .set(&shell.rt, "media.title", Value::text("song"))
        .unwrap();
    shell.flush();
    let c = shell.take();
    assert_eq!(written(c.last().unwrap()), "song");
    shell.host.set_ready(&shell.rt, "media", false).unwrap();
    shell.flush();
    shell
        .host
        .set(&shell.rt, "media.title", Value::Null)
        .unwrap();
    shell.flush();
    let c = shell.take();
    assert_eq!(c.len(), 1, "{c:?}");
    assert!(
        matches!(&c[0].content, OutputContent::Waiting { .. }),
        "{c:?}"
    );
    shell.flush();
    assert!(shell.take().is_empty());
}

/// (live-files) A file that turns to waiting keeps its target, so the
/// writer still lands the newest content it was pacing (written while
/// the service was ready) instead of letting it go.
#[test]
fn a_waiting_file_keeps_its_target() {
    let dir = temp_dir("waiting-target");
    let src = "file \"out/w.txt\" { text: media.title ?? \"none\" }\n";
    let shell = boot(&[("a.strand", src)], &dir);
    let _ = shell.take();
    let at = dir.join("out").join("w.txt");
    assert_eq!(targets(&shell), vec![at.clone()]);
    shell.host.set_ready(&shell.rt, "media", false).unwrap();
    shell.flush();
    let c = shell.take();
    assert!(
        matches!(&c[0].content, OutputContent::Waiting { .. }),
        "{c:?}"
    );
    assert_eq!(targets(&shell), vec![at]);
}

/// (live-files, e) A `text:` that reads the theme only is the template
/// it was: the same bytes as a template file's hole, `.light()` included.
#[test]
fn a_theme_text_is_still_a_template() {
    let dir = temp_dir("theme-text");
    std::fs::write(dir.join("a.tmpl"), "{{ $accent }}").unwrap();
    std::fs::write(dir.join("l.tmpl"), "{{ $surface.light() }}").unwrap();
    let src = "file \"out/a.txt\" { text: $accent }\n\
               file \"out/at.txt\" { template: \"a.tmpl\" }\n\
               file \"out/l.txt\" { text: $surface.light() }\n\
               file \"out/lt.txt\" { template: \"l.tmpl\" }\n";
    let shell = boot(&[("a.strand", src)], &dir);
    shell.dark(true);
    let c = shell.take();
    let get = |n: &str| written(change(&c, &dir, n).unwrap());
    assert_eq!(get("out/a.txt"), get("out/at.txt"));
    assert_eq!(get("out/l.txt"), get("out/lt.txt"));
    assert!(get("out/a.txt").starts_with('#'));
    assert_eq!(shell.inst.other_mode_holders(), 2);
}

/// (live-files) A token inside `join`, in a text that reads the scope, is
/// written as its value (as a template's `join` writes it), never as the
/// token's inner form.
#[test]
fn a_token_joined_in_a_scope_text_is_its_value() {
    let dir = temp_dir("join-token");
    std::fs::write(dir.join("a.tmpl"), "{{ $accent }}").unwrap();
    let src = "export state size = 11\n\
               file \"out/j.txt\" { text: join(\"\\n\", join(\"=\", \"fg\", $accent), \
               join(\"=\", \"size\", size)) }\n\
               file \"out/m.txt\" { text: join(\"\", $accent.alpha(0.5), size) }\n\
               file \"out/at.txt\" { template: \"a.tmpl\" }\n";
    let shell = boot(&[("a.strand", src)], &dir);
    shell.dark(true);
    let c = shell.take();
    let get = |n: &str| written(change(&c, &dir, n).unwrap());
    let accent = get("out/at.txt");
    assert!(accent.starts_with('#'), "{accent}");
    assert_eq!(get("out/j.txt"), format!("fg={accent}\nsize=11"));
    // A translucent token is refused as a template refuses it.
    assert!(
        matches!(
            &change(&c, &dir, "out/m.txt").unwrap().content,
            OutputContent::Failed { .. }
        ),
        "{c:?}"
    );
}

/// (live-files, f) The other mode needs a text that reads only the theme.
#[test]
fn the_other_mode_in_a_scope_text_is_an_error() {
    let err = build(&[(
        "a.strand",
        "export state x = \"a\"\nfile \"/tmp/x\" { text: x + hex($surface.light()) }\n",
    )])
    .unwrap_err();
    assert!(err.contains("file::other_mode"), "{err}");
    assert!(err.contains("`.light()`"), "{err}");
    let err = build(&[(
        "a.strand",
        "export state x = true\nfile \"/tmp/x\" { text: x ? hex($surface.dark()) : \"\" }\n",
    )])
    .unwrap_err();
    assert!(err.contains("file::other_mode"), "{err}");
}

/// (live-files, g) A top-level `file` holds the services it reads.
#[test]
fn a_top_level_file_holds_what_it_reads() {
    let dir = temp_dir("holds");
    let src = "file \"out/g.txt\" { text: media.title ?? \"\" }\n";
    let shell = boot(&[("a.strand", src)], &dir);
    assert!(shell.host.readers("media") > 0, "media is held");
    assert_eq!(shell.host.readers("battery"), 0);
}

/// (live-files, h) A service read only inside a `fn` the text calls still
/// makes the file wait.
#[test]
fn a_service_read_through_a_fn_makes_the_file_wait() {
    let dir = temp_dir("fn-waits");
    let src = "fn playing() -> text { media.title ?? \"none\" }\n\
               file \"out/h.txt\" { text: playing() }\n";
    let shell = boot_with(&[("a.strand", src)], &dir, |rt, host| {
        host.set_ready(rt, "media", false).unwrap();
    });
    let c = shell.take();
    assert!(
        matches!(&c[0].content, OutputContent::Waiting { services } if services == &["media"]),
        "{c:?}"
    );
    shell.host.set_ready(&shell.rt, "media", true).unwrap();
    shell.flush();
    assert_eq!(written(&shell.take()[0]), "none");
}
