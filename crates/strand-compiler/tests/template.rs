//! (M5) Templates (spec.md 9.8; decisions.md m5-tools, "template
//! expressions and formats"): the value formats' goldens, every
//! diagnostic with its location, the other mode, the channel methods and
//! byte-identical re-renders. No fixture imitates an application.

use strand_compiler::diagnostic::{Style, render};
use strand_compiler::template::Template;
use strand_compiler::{FileId, SourceMap};
use strand_scene::{Color, Font, PropValue, TokenTable};
use strand_theme::defaults::base_tokens;

/// The built-in base tokens over `catppuccin:<flavour>`'s palette, as a
/// shell resolves its table.
fn table(flavour: &str) -> TokenTable {
    let mut t = base_tokens();
    let p = strand_theme::import(&format!("catppuccin:{flavour}"), None).unwrap();
    p.insert_into(&mut t);
    t
}

fn compile(src: &str) -> Template {
    Template::compile(FileId::default(), src).unwrap_or_else(|d| panic!("{}", show(src, &d)))
}

fn show(src: &str, d: &[strand_compiler::Diagnostic]) -> String {
    let (map, _) = SourceMap::single("theme.css.tmpl", String::from(src));
    render(d, &map, Style::Plain)
}

fn rendered(src: &str, current: &TokenTable, other: Option<&TokenTable>) -> String {
    let t = compile(src);
    let out = t
        .render(current, other)
        .unwrap_or_else(|d| panic!("{}", show(src, &d)));
    String::from_utf8(out).unwrap()
}

/// What a template that fails reports, rendered as `strand check` does.
fn errors(src: &str, current: &TokenTable, other: Option<&TokenTable>) -> String {
    match Template::compile(FileId::default(), src) {
        Err(d) => show(src, &d),
        Ok(t) => match t.render(current, other) {
            Err(d) => show(src, &d),
            Ok(out) => panic!("rendered: {}", String::from_utf8_lossy(&out)),
        },
    }
}

/// A golden file under `tests/template/`; `STRAND_BLESS=1` rewrites it.
fn golden(name: &str, got: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/template")
        .join(name);
    if std::env::var_os("STRAND_BLESS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, got).unwrap();
    }
    let want = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("{} missing: run with STRAND_BLESS=1", path.display()));
    assert_eq!(got, want, "{name} differs from its golden");
}

#[test]
fn the_neutral_css_example_renders() {
    let src = ":root {\n  --accent: {{ $accent }};\n  --on-accent: {{ $on_accent }};\n  \
               --surface: {{ $surface }};\n  --fg: {{ $fg }};\n  \
               --fg-muted: {{ hex($fg.muted, alpha: true) }};\n  \
               --glow: {{ hex($accent.alpha(0.4), alpha: true) }};\n}\n";
    let got = rendered(src, &table("mocha"), None);
    golden("css.golden", &got);
    // Byte-identical when rendered again.
    assert_eq!(got, rendered(src, &table("mocha"), None));
}

#[test]
fn every_value_kind_has_its_format() {
    let mut t = table("mocha");
    t.insert(
        "pct",
        PropValue::Length(strand_scene::Length::Percent(40.0)),
    );
    t.insert("wide", PropValue::Length(strand_scene::Length::Ch(4.0)));
    t.insert("label", PropValue::Text("bar".into()));
    t.insert("flag", PropValue::Bool(true));
    let src = "number {{ 0.65 }} {{ 1.23456 }} {{ -2 }}\n\
               pixels {{ $space.2 }}px\n\
               percent {{ $pct }} ch {{ $wide }}\n\
               angle {{ 90deg }} duration {{ 120ms }} {{ 1.5s }}\n\
               string {{ $label }} {{ \"{{\" }} bool {{ $flag }} {{ $palette.dark }}\n\
               name {{ $palette.name }} source {{ $palette.source }}\n\
               extra {{ $palette.mauve }} {{ $palette.nope ?? $accent }}\n\
               hex {{ hex($accent) }} {{ hex($accent, hash: false) }} \
               {{ hex($accent, alpha: true) }}\n\
               argb {{ argb($accent.alpha(0.5)) }}\n\
               channels rgb({{ $fg.red() }}, {{ $fg.green() }}, {{ $fg.blue() }}) \
               {{ $fg.muted.opacity() }}\n\
               methods {{ $surface.mix($fg, 8%) }} {{ $accent.lighten(0.1) }} \
               {{ $accent.darken(0.1) }}\n\
               join {{ join(\", \", $accent, null, $palette.missing ?? null, 4) }}\n\
               spans {{\n  $accent\n}} and }} stays text\n";
    let got = rendered(src, &t, None);
    golden("values.golden", &got);
}

#[test]
fn composite_values_are_errors_naming_their_type() {
    let mut t = table("mocha");
    t.insert(
        "font.mine",
        PropValue::Font(Font {
            family: "Inter".into(),
            size: 13.0,
            weight: 500,
        }),
    );
    let out = errors("{{ $font.mine }}", &t, None);
    assert!(out.contains("a font has no one text form"), "{out}");
    assert!(out.contains("theme.css.tmpl:1:4"), "{out}");
}

#[test]
fn diagnostics_have_their_location_and_all_are_reported() {
    let t = table("mocha");
    let src = "a {{ $acent }}\nb {{ state.x }}\nc {{ $fg.muted }}\nd {{ $palette.nope }}\n\
               e {{ $accent + 1 }}\nf {{ $accent. }}\ng {{ }}\nh {{ $accent";
    let out = errors(src, &t, None);
    insta::assert_snapshot!("template_compile_errors", out);
    // The compile errors stop before rendering; the render errors are
    // all reported too.
    let src = "a {{ $acent }}\nc {{ $fg.muted }}\nd {{ $palette.nope }}\n\
               x {{ hex($accent, alpha: 1) }}\ny {{ argb(\"x\") }}\nz {{ $surface.light() }}";
    let out = errors(src, &t, None);
    insta::assert_snapshot!("template_render_errors", out);
}

#[test]
fn the_other_mode_reads_the_other_table() {
    let (dark, light) = (table("mocha"), table("latte"));
    let src = "{{ $surface }} {{ $surface.light() }} {{ $surface.dark() }} \
               {{ $palette.name.light() }} {{ $accent.light().red() }}";
    let t = compile(src);
    assert!(t.reads_other_mode());
    assert!(!compile("{{ $surface }}").reads_other_mode());
    let got = String::from_utf8(t.render(&dark, Some(&light)).unwrap()).unwrap();
    let hexed = |c: &TokenTable| match c.lookup("surface") {
        Some(PropValue::Color(c)) => strand_theme::export::hex(c),
        _ => unreachable!(),
    };
    let red = match light.lookup("accent") {
        Some(PropValue::Color(c)) => c.gamut_mapped().to_rgba8()[0],
        _ => unreachable!(),
    };
    assert_eq!(
        got,
        format!(
            "{d} {l} {d} Catppuccin Latte {red}",
            d = hexed(&dark),
            l = hexed(&light)
        )
    );
    // From the light side, the same template reads the dark table.
    let got = String::from_utf8(t.render(&light, Some(&dark)).unwrap()).unwrap();
    assert!(got.starts_with(&format!(
        "{} {} {}",
        hexed(&light),
        hexed(&light),
        hexed(&dark)
    )));
    // A theme with one mode: the other is absent, an error unless `??`
    // replaces it.
    let out = errors("{{ $surface.light() }}", &dark, Some(&table("mocha")));
    assert!(out.contains("the theme has no light mode"), "{out}");
    assert_eq!(
        rendered("{{ $surface.light() ?? $surface }}", &dark, None),
        hexed(&dark)
    );
}

/// `join` skips nulls only: a missing extra or an absent other mode is
/// not null, so inside `join` it is the same located error as outside
/// (spec.md 9.5, 9.8; review round 3 item 1).
#[test]
fn join_skips_nulls_but_not_absent_values() {
    let dark = table("mocha");
    let out = errors("{{ join(\" \", $palette.mauvee, $accent) }}", &dark, None);
    assert!(out.contains("has no `$palette.mauvee`"), "{out}");
    assert!(out.contains("theme.css.tmpl:1:14"), "{out}");
    let out = errors("{{ join(\",\", $surface.light()) }}", &dark, None);
    assert!(out.contains("the theme has no light mode"), "{out}");
    // `??` turns either into a null `join` skips.
    assert_eq!(
        rendered(
            "{{ join(\",\", $surface.light() ?? null, $palette.mauvee ?? null, 1) }}",
            &dark,
            None
        ),
        "1"
    );
}

#[test]
fn channels_are_methods_and_never_token_paths() {
    let mut t = table("mocha");
    // `$accent.red` is a token path: unknown here...
    let out = errors("{{ $accent.red }}", &t, None);
    assert!(out.contains("there is no token `$accent.red`"), "{out}");
    // ...and the user's token when the config declares one, while
    // `$accent.red()` stays the channel.
    t.insert("accent.red", PropValue::Number(7.0));
    let accent = match t.lookup("accent") {
        Some(PropValue::Color(c)) => c,
        _ => unreachable!(),
    };
    let [r, ..] = accent.gamut_mapped().to_rgba8();
    assert_eq!(
        rendered("{{ $accent.red }} {{ $accent.red() }}", &t, None),
        format!("7 {r}")
    );
}

#[test]
fn a_template_reads_the_theme_only() {
    let t = table("mocha");
    for src in [
        "{{ audio.volume }}",
        "{{ dark }}",
        "{{ fmt(1) }}",
        "{{ dark ? 1 : 2 }}",
    ] {
        let out = errors(src, &t, None);
        assert!(
            out.contains("a template reads the theme only"),
            "{src}: {out}"
        );
    }
}

#[test]
fn a_translucent_colour_is_never_written_opaque() {
    let t = table("mocha");
    for src in ["{{ $fg.muted }}", "{{ hex($accent.alpha(0.4)) }}"] {
        let out = errors(src, &t, None);
        assert!(out.contains("this colour is translucent"), "{src}: {out}");
    }
    let c = Color::from_hex("#11223380").unwrap();
    let mut t = TokenTable::default();
    t.insert("c", PropValue::Color(c));
    assert_eq!(
        rendered("{{ hex($c, alpha: true) }} {{ argb($c) }}", &t, None),
        "#11223380 0x80112233"
    );
}
