//! `strand export json` and `strand export base24`: pure writers over a
//! palette and the resolved token table (decisions.md m5-tools, "the
//! CLI" and "template expressions and formats"). The palette gives the
//! identity, the source and the extras; the table, overlays included,
//! every value, so `strand set` reaches both. Neither writes a file:
//! the CLI and the IPC request (S-tools) do.

use std::collections::BTreeSet;
use std::fmt::Write;

use strand_scene::tokens::OVERLAY_ORIGIN;
use strand_scene::{Color, Length, PropValue, TimeContext, TokenScope, TokenTable, Transition};

use crate::palette::Palette;
use crate::role::Role;

/// A JSON value, written in the order it is built.
enum J {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

/// `v` in its shortest form, at most 3 decimals (`8`, `0.65`, `-1.5`).
pub fn number(v: f64) -> String {
    let r = (v * 1000.0).round() / 1000.0;
    if !r.is_finite() {
        return "null".into();
    }
    // `-0` is `0`.
    format!("{}", r + 0.0)
}

/// `c` as lowercase `#rrggbb`, or `#rrggbbaa` when translucent, after
/// gamut mapping.
pub fn hex(c: Color) -> String {
    let [r, g, b, a] = c.gamut_mapped().to_rgba8();
    if a == 255 {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else {
        format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
    }
}

fn quoted(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

impl J {
    fn write(&self, out: &mut String, indent: usize) {
        let pad = |out: &mut String, n: usize| out.extend(std::iter::repeat_n(' ', n));
        match self {
            J::Null => out.push_str("null"),
            J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            J::Num(n) => out.push_str(&number(*n)),
            J::Str(s) => out.push_str(&quoted(s)),
            J::Arr(items) if items.is_empty() => out.push_str("[]"),
            J::Obj(items) if items.is_empty() => out.push_str("{}"),
            J::Arr(items) => {
                out.push_str("[\n");
                for (i, v) in items.iter().enumerate() {
                    pad(out, indent + 2);
                    v.write(out, indent + 2);
                    out.push_str(if i + 1 < items.len() { ",\n" } else { "\n" });
                }
                pad(out, indent);
                out.push(']');
            }
            J::Obj(items) => {
                out.push_str("{\n");
                for (i, (k, v)) in items.iter().enumerate() {
                    pad(out, indent + 2);
                    out.push_str(&quoted(k));
                    out.push_str(": ");
                    v.write(out, indent + 2);
                    out.push_str(if i + 1 < items.len() { ",\n" } else { "\n" });
                }
                pad(out, indent);
                out.push('}');
            }
        }
    }
}

/// A resolved token value as `export json` writes it: colours as hex,
/// numbers (pixels, degrees, milliseconds) bare, other lengths, strings
/// and keywords as strings, fonts, springs and shadow lists as objects
/// with strand-scene's field names, any other kind `null`.
fn value(v: &PropValue) -> J {
    let num = |n: f32| J::Num(f64::from(n));
    match v {
        PropValue::Color(c) => J::Str(hex(*c)),
        PropValue::Number(n) | PropValue::Angle(n) | PropValue::Length(Length::Px(n)) => num(*n),
        PropValue::Duration(d) => J::Num(d.as_secs_f64() * 1000.0),
        PropValue::Length(Length::Percent(n)) => J::Str(format!("{}%", number(f64::from(*n)))),
        PropValue::Length(Length::Ch(n)) => J::Str(format!("{}ch", number(f64::from(*n)))),
        PropValue::Length(Length::Auto) => J::Str("auto".into()),
        PropValue::Text(s) | PropValue::Keyword(s) => J::Str(s.clone()),
        PropValue::Bool(b) => J::Bool(*b),
        PropValue::Font(f) => J::Obj(vec![
            ("family".into(), J::Str(f.family.clone())),
            ("size".into(), num(f.size)),
            ("weight".into(), J::Num(f64::from(f.weight))),
        ]),
        PropValue::Transition(Transition::Spring { stiffness, damping }) => J::Obj(vec![(
            "spring".into(),
            J::Obj(vec![
                ("stiffness".into(), num(*stiffness)),
                ("damping".into(), num(*damping)),
            ]),
        )]),
        PropValue::Shadow(layers) => J::Arr(
            layers
                .iter()
                .map(|s| {
                    J::Obj(vec![
                        ("x".into(), num(s.x)),
                        ("y".into(), num(s.y)),
                        ("blur".into(), num(s.blur)),
                        ("spread".into(), num(s.spread)),
                        ("color".into(), J::Str(hex(s.color))),
                    ])
                })
                .collect(),
        ),
        _ => J::Null,
    }
}

/// The colour of `role` in `table` (overlays included), else the
/// palette's.
fn role_color(scope: &TokenScope, palette: &Palette, role: Role) -> Color {
    match scope.lookup(role.name()) {
        Some(PropValue::Color(c)) => c,
        _ => palette.get(role),
    }
}

/// The scope tokens are resolved in: the global table at `t = 0`, so the
/// output never depends on when it runs.
fn scope<'a>(levels: &'a [&'a TokenTable; 1]) -> TokenScope<'a> {
    TokenScope::new(levels).with_time(Some(TimeContext::default()))
}

/// `strand export json`: one object with `name`, `source`, `dark`, the
/// 62 roles in schema order, every token of `table` resolved and sorted
/// by path, and the extras in source order; two-space indented, ending
/// with a newline, deterministic.
pub fn json(palette: &Palette, table: &TokenTable) -> String {
    let levels = [table];
    let scope = scope(&levels);
    let text = |s: Option<&str>| s.map_or(J::Null, |s| J::Str(s.into()));
    let roles = Role::ALL
        .iter()
        .map(|&r| (r.name().into(), J::Str(hex(role_color(&scope, palette, r)))))
        .collect();
    let paths: BTreeSet<&String> = table.tokens.keys().chain(table.derived.keys()).collect();
    let tokens = paths
        .into_iter()
        .map(|p| (p.clone(), scope.lookup(p).as_ref().map_or(J::Null, value)))
        .collect();
    let extras = palette
        .extras()
        .iter()
        .map(|(n, c)| (n.clone(), J::Str(hex(*c))))
        .collect();
    let doc = J::Obj(vec![
        ("name".into(), text(palette.name())),
        ("source".into(), text(palette.source())),
        ("dark".into(), J::Bool(palette.is_dark())),
        ("roles".into(), J::Obj(roles)),
        ("tokens".into(), J::Obj(tokens)),
        ("extras".into(), J::Obj(extras)),
    ]);
    let mut out = String::new();
    doc.write(&mut out, 0);
    out.push('\n');
    out
}

/// base24's slots and the role each is written from (decisions.md
/// m5-tools, "the CLI": the styling guide's meaning of each slot).
pub const BASE24_SLOTS: [Role; 24] = [
    Role::Bg,
    Role::SurfaceLow,
    Role::SurfaceHigh,
    Role::Outline,
    Role::FgVariant,
    Role::Fg,
    Role::Fg,
    Role::InverseSurface,
    Role::Red,
    Role::Orange,
    Role::Yellow,
    Role::Green,
    Role::Cyan,
    Role::Blue,
    Role::Magenta,
    Role::Tertiary,
    Role::SurfaceDim,
    Role::SurfaceLowest,
    Role::BrightRed,
    Role::BrightYellow,
    Role::BrightGreen,
    Role::BrightCyan,
    Role::BrightBlue,
    Role::BrightMagenta,
];

/// `strand export base24`: a base24 scheme (YAML) with the palette's
/// name, `author: "strand export"`, its variant and `base00`–`base17` as
/// lowercase `"#rrggbb"`. A palette imported from a `base16:` or
/// `base24:` scheme writes its own slots back (its extras), so the
/// scheme passes through unchanged, except a slot whose role carries a
/// runtime overlay; every other slot is its role's resolved colour.
pub fn base24(palette: &Palette, table: &TokenTable) -> String {
    let levels = [table];
    let scope = scope(&levels);
    let yaml = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let passes = palette
        .source()
        .is_some_and(|s| s.starts_with("base16:") || s.starts_with("base24:"));
    let mut out = format!(
        "system: \"base24\"\nname: \"{}\"\nauthor: \"strand export\"\nvariant: \"{}\"\npalette:\n",
        yaml(palette.name().unwrap_or("strand")).replace(['\n', '\r'], " "),
        if palette.is_dark() { "dark" } else { "light" },
    );
    for (i, &role) in BASE24_SLOTS.iter().enumerate() {
        let slot = format!("base{i:02x}");
        let key = format!("base{i:02X}");
        let own = passes
            .then(|| palette.extra(&slot))
            .flatten()
            .filter(|_| table.origin(role.name()) != Some(OVERLAY_ORIGIN));
        let c = own.unwrap_or_else(|| role_color(&scope, palette, role));
        let [r, g, b, _] = c.gamut_mapped().to_rgba8();
        let _ = writeln!(out, "  {key}: \"#{r:02x}{g:02x}{b:02x}\"");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_short() {
        assert_eq!(number(8.0), "8");
        assert_eq!(number(0.65f32 as f64), "0.65");
        assert_eq!(number(-0.0), "0");
        assert_eq!(number(1.23456), "1.235");
        assert_eq!(number(f64::NAN), "null");
    }

    #[test]
    fn colours_are_hex() {
        assert_eq!(hex(Color::from_hex("#FF8800").unwrap()), "#ff8800");
        assert_eq!(hex(Color::from_hex("#ff880080").unwrap()), "#ff880080");
    }
}
