//! `strand export json` and `strand export base24`: pure writers over a
//! palette and the resolved token table (decisions.md m5-tools, "the
//! CLI" and "template expressions and formats"). The palette gives the
//! identity, the source and the extras; the table, overlays included,
//! every value, so `strand set` reaches both. Neither writes a file:
//! the CLI and the IPC request (S-tools) do.

use std::fmt::Write;

use strand_scene::tokens::OVERLAY_ORIGIN;
use strand_scene::{Color, Length, PropValue, TimeContext, TokenScope, TokenTable, Transition};

use crate::palette::Palette;
use crate::role::Role;

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

/// Pretty JSON (two-space indent) written as it is called, so objects
/// keep the order they are written in (serde_json's maps would sort the
/// roles).
#[derive(Default)]
struct Json {
    out: String,
    depth: usize,
    /// Nothing written in the innermost open object or array yet.
    first: bool,
}

impl Json {
    fn line(&mut self) {
        self.out.push('\n');
        self.out.extend(std::iter::repeat_n(' ', self.depth * 2));
    }

    /// Starts an item: the separator, and the key in an object.
    fn item(&mut self, key: Option<&str>) {
        if self.depth > 0 {
            if !self.first {
                self.out.push(',');
            }
            self.line();
        }
        self.first = false;
        if let Some(k) = key {
            self.out.push_str(&quoted(k));
            self.out.push_str(": ");
        }
    }

    fn raw(&mut self, key: Option<&str>, v: &str) {
        self.item(key);
        self.out.push_str(v);
    }

    fn str(&mut self, key: Option<&str>, v: &str) {
        self.raw(key, &quoted(v));
    }

    fn num(&mut self, key: Option<&str>, v: f32) {
        self.raw(key, &number(f64::from(v)));
    }

    fn open(&mut self, key: Option<&str>, bracket: char) {
        self.item(key);
        self.out.push(bracket);
        self.depth += 1;
        self.first = true;
    }

    fn close(&mut self, bracket: char) {
        self.depth -= 1;
        if !self.first {
            self.line();
        }
        self.out.push(bracket);
        self.first = false;
    }

    /// A resolved token value as `export json` writes it: colours as
    /// hex, numbers (pixels, degrees, milliseconds) bare, other lengths,
    /// strings and keywords as strings, fonts, springs and shadow lists
    /// as objects with strand-scene's field names, any other kind `null`.
    fn value(&mut self, key: Option<&str>, v: Option<&PropValue>) {
        let k = key;
        match v {
            Some(PropValue::Color(c)) => self.str(k, &hex(*c)),
            Some(PropValue::Number(n) | PropValue::Angle(n) | PropValue::Length(Length::Px(n))) => {
                self.num(k, *n)
            }
            Some(PropValue::Duration(d)) => self.raw(k, &number(d.as_secs_f64() * 1000.0)),
            Some(PropValue::Length(Length::Percent(n))) => {
                self.str(k, &format!("{}%", number(f64::from(*n))))
            }
            Some(PropValue::Length(Length::Ch(n))) => {
                self.str(k, &format!("{}ch", number(f64::from(*n))))
            }
            Some(PropValue::Length(Length::Auto)) => self.str(k, "auto"),
            Some(PropValue::Text(s) | PropValue::Keyword(s)) => self.str(k, s),
            Some(PropValue::Bool(b)) => self.raw(k, if *b { "true" } else { "false" }),
            Some(PropValue::Font(f)) => {
                self.open(k, '{');
                self.str(Some("family"), &f.family);
                self.num(Some("size"), f.size);
                self.raw(Some("weight"), &f.weight.to_string());
                self.close('}');
            }
            Some(PropValue::Transition(Transition::Spring { stiffness, damping })) => {
                self.open(k, '{');
                self.open(Some("spring"), '{');
                self.num(Some("stiffness"), *stiffness);
                self.num(Some("damping"), *damping);
                self.close('}');
                self.close('}');
            }
            Some(PropValue::Shadow(layers)) => {
                self.open(k, '[');
                for s in layers {
                    self.open(None, '{');
                    self.num(Some("x"), s.x);
                    self.num(Some("y"), s.y);
                    self.num(Some("blur"), s.blur);
                    self.num(Some("spread"), s.spread);
                    self.str(Some("color"), &hex(s.color));
                    self.close('}');
                }
                self.close(']');
            }
            _ => self.raw(k, "null"),
        }
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
    let mut w = Json::default();
    w.open(None, '{');
    for (key, v) in [("name", palette.name()), ("source", palette.source())] {
        match v {
            Some(v) => w.str(Some(key), v),
            None => w.raw(Some(key), "null"),
        }
    }
    w.raw(
        Some("dark"),
        if palette.is_dark() { "true" } else { "false" },
    );
    w.open(Some("roles"), '{');
    for &r in Role::ALL {
        w.str(Some(r.name()), &hex(role_color(&scope, palette, r)));
    }
    w.close('}');
    // Plain and derived paths are disjoint and each sorted: merged, they
    // are every path in order, with no sort.
    w.open(Some("tokens"), '{');
    let (mut plain, mut derived) = (
        table.tokens.keys().peekable(),
        table.derived.keys().peekable(),
    );
    loop {
        let path = match (plain.peek(), derived.peek()) {
            (Some(a), Some(b)) if a <= b => plain.next(),
            (Some(_), Some(_)) | (None, Some(_)) => derived.next(),
            (Some(_), None) => plain.next(),
            (None, None) => break,
        };
        if let Some(p) = path {
            w.value(Some(p), scope.lookup(p).as_ref());
        }
    }
    w.close('}');
    w.open(Some("extras"), '{');
    for (n, c) in palette.extras() {
        w.str(Some(n), &hex(*c));
    }
    w.close('}');
    w.close('}');
    w.out.push('\n');
    w.out
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
