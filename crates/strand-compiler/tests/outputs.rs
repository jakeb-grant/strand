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
        OutputContent::Failed { diags, .. } => panic!("failed: {diags:?}"),
    }
}

fn failed(c: &OutputChange) -> String {
    match &c.content {
        OutputContent::Failed { diags, .. } => format!("{diags:?}"),
        OutputContent::Write { .. } => panic!("written: {c:?}"),
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
