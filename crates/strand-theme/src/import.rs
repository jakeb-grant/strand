//! `import("…")`: palettes from other schemes, each filling the whole
//! palette schema (missing roles come from [`Partial::fill`]'s one
//! derivation table, then the contrast guard).
//!
//! Sources (`docs/decisions.md`, wave3-theme):
//!
//! - `catppuccin:<flavour>` (`mocha`, `macchiato`, `frappe`, `latte`), or
//!   `catppuccin:<flavour>:<accent>` to pick the accent (default `mauve`).
//! - `base16:<file>` and `base24:<file>`: a tinted-theming scheme (YAML,
//!   `palette:` nested or the legacy flat `base00:` form).
//! - `matugen:<file>`: matugen's JSON output (`--json hex`), either the
//!   `colors.<mode>.<role>` or the `colors.<role>.<mode>` layout; the mode
//!   is the file's `mode`, else dark.
//! - `w3c:<file>`: W3C design-token JSON; colour tokens whose path ends
//!   in a role's name (`color.accent`, `md.sys.color.on-primary`) fill
//!   that role; `{alias}` references are followed.
//!
//! Files are relative to the config directory; `~/` is the home
//! directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use strand_scene::Color;

use crate::palette::{Palette, Partial};
use crate::role::Role;

/// Why an import failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportError {
    /// Not a known source form.
    UnknownSource(String),
    /// A Catppuccin flavour or accent that does not exist.
    UnknownName {
        what: &'static str,
        name: String,
        known: Vec<&'static str>,
    },
    Io {
        path: PathBuf,
        error: String,
    },
    Parse {
        path: PathBuf,
        error: String,
    },
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::UnknownSource(s) => write!(
                f,
                "unknown palette `{s}`: use catppuccin:<flavour>, base16:<file>, base24:<file>, matugen:<file> or w3c:<file>"
            ),
            ImportError::UnknownName { what, name, known } => {
                write!(f, "unknown {what} `{name}`; known: {}", known.join(", "))
            }
            ImportError::Io { path, error } => write!(f, "cannot read {}: {error}", path.display()),
            ImportError::Parse { path, error } => write!(f, "{}: {error}", path.display()),
        }
    }
}

impl std::error::Error for ImportError {}

/// The source's file, if it names one (for watching), resolved against
/// `base`.
pub fn file_of(source: &str, base: Option<&Path>) -> Option<PathBuf> {
    let (kind, rest) = source.split_once(':')?;
    matches!(kind, "base16" | "base24" | "matugen" | "w3c").then(|| resolve(rest, base))
}

/// `~/x` against `$HOME`, a relative path against `base`.
pub fn resolve(path: &str, base: Option<&Path>) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    let p = PathBuf::from(path);
    match base {
        Some(b) if p.is_relative() => b.join(p),
        _ => p,
    }
}

/// Imports the palette `source` names (see the module docs).
pub fn import(source: &str, base: Option<&Path>) -> Result<Palette, ImportError> {
    let Some((kind, rest)) = source.split_once(':') else {
        return Err(ImportError::UnknownSource(source.to_string()));
    };
    let read = |rest: &str| -> Result<(PathBuf, String), ImportError> {
        let path = resolve(rest, base);
        std::fs::read_to_string(&path)
            .map(|t| (path.clone(), t))
            .map_err(|e| ImportError::Io {
                path,
                error: e.to_string(),
            })
    };
    let partial = match kind {
        "catppuccin" => catppuccin(rest)?,
        "base16" | "base24" => {
            let (path, text) = read(rest)?;
            base16(&text).map_err(|error| ImportError::Parse { path, error })?
        }
        "matugen" => {
            let (path, text) = read(rest)?;
            matugen(&text).map_err(|error| ImportError::Parse { path, error })?
        }
        "w3c" => {
            let (path, text) = read(rest)?;
            w3c(&text).map_err(|error| ImportError::Parse { path, error })?
        }
        _ => return Err(ImportError::UnknownSource(source.to_string())),
    };
    Ok(partial.fill())
}

// ---------------------------------------------------------------------------
// Catppuccin

/// Catppuccin colour names, in the flavour tables' order.
pub const CATPPUCCIN_NAMES: [&str; 26] = [
    "rosewater",
    "flamingo",
    "pink",
    "mauve",
    "red",
    "maroon",
    "peach",
    "yellow",
    "green",
    "teal",
    "sky",
    "sapphire",
    "blue",
    "lavender",
    "text",
    "subtext1",
    "subtext0",
    "overlay2",
    "overlay1",
    "overlay0",
    "surface2",
    "surface1",
    "surface0",
    "base",
    "mantle",
    "crust",
];

/// The four flavours (catppuccin/palette v1).
pub const CATPPUCCIN: [(&str, [&str; 26]); 4] = [
    (
        "mocha",
        [
            "#f5e0dc", "#f2cdcd", "#f5c2e7", "#cba6f7", "#f38ba8", "#eba0ac", "#fab387", "#f9e2af",
            "#a6e3a1", "#94e2d5", "#89dceb", "#74c7ec", "#89b4fa", "#b4befe", "#cdd6f4", "#bac2de",
            "#a6adc8", "#9399b2", "#7f849c", "#6c7086", "#585b70", "#45475a", "#313244", "#1e1e2e",
            "#181825", "#11111b",
        ],
    ),
    (
        "macchiato",
        [
            "#f4dbd6", "#f0c6c6", "#f5bde6", "#c6a0f6", "#ed8796", "#ee99a0", "#f5a97f", "#eed49f",
            "#a6da95", "#8bd5ca", "#91d7e3", "#7dc4e4", "#8aadf4", "#b7bdf8", "#cad3f5", "#b8c0e0",
            "#a5adcb", "#939ab7", "#8087a2", "#6e738d", "#5b6078", "#494d64", "#363a4f", "#24273a",
            "#1e2030", "#181926",
        ],
    ),
    (
        "frappe",
        [
            "#f2d5cf", "#eebebe", "#f4b8e4", "#ca9ee6", "#e78284", "#ea999c", "#ef9f76", "#e5c890",
            "#a6d189", "#81c8be", "#99d1db", "#85c1dc", "#8caaee", "#babbf1", "#c6d0f5", "#b5bfe2",
            "#a5adce", "#949cbb", "#838ba7", "#737994", "#626880", "#51576d", "#414559", "#303446",
            "#292c3c", "#232634",
        ],
    ),
    (
        "latte",
        [
            "#dc8a78", "#dd7878", "#ea76cb", "#8839ef", "#d20f39", "#e64553", "#fe640b", "#df8e1d",
            "#40a02b", "#179299", "#04a5e5", "#209fb5", "#1e66f5", "#7287fd", "#4c4f69", "#5c5f77",
            "#6c6f85", "#7c7f93", "#8c8fa1", "#9ca0b0", "#acb0be", "#bcc0cc", "#ccd0da", "#eff1f5",
            "#e6e9ef", "#dce0e8",
        ],
    ),
];

/// The accents Catppuccin's style guide allows.
const CATPPUCCIN_ACCENTS: [&str; 14] = [
    "rosewater",
    "flamingo",
    "pink",
    "mauve",
    "red",
    "maroon",
    "peach",
    "yellow",
    "green",
    "teal",
    "sky",
    "sapphire",
    "blue",
    "lavender",
];

fn catppuccin(rest: &str) -> Result<Partial, ImportError> {
    let (flavour, accent) = rest.split_once(':').unwrap_or((rest, "mauve"));
    let flavour = if flavour == "frappé" {
        "frappe"
    } else {
        flavour
    };
    let Some((_, table)) = CATPPUCCIN.iter().find(|(n, _)| *n == flavour) else {
        return Err(ImportError::UnknownName {
            what: "Catppuccin flavour",
            name: flavour.to_string(),
            known: CATPPUCCIN.iter().map(|(n, _)| *n).collect(),
        });
    };
    if !CATPPUCCIN_ACCENTS.contains(&accent) {
        return Err(ImportError::UnknownName {
            what: "Catppuccin accent",
            name: accent.to_string(),
            known: CATPPUCCIN_ACCENTS.to_vec(),
        });
    }
    let c = |name: &str| {
        CATPPUCCIN_NAMES
            .iter()
            .position(|n| *n == name)
            .and_then(|i| Color::from_hex(table[i]))
            .unwrap_or(Color::BLACK)
    };
    let latte = flavour == "latte";
    let on = if latte { c("base") } else { c("crust") };
    let accent = c(accent);
    let mut p = Partial {
        dark: Some(!latte),
        ..Partial::default()
    };
    for (role, color) in [
        (Role::Accent, accent),
        (Role::OnAccent, on),
        (Role::OnAccentContainer, c("text")),
        (Role::Secondary, c("sky")),
        (Role::OnSecondary, on),
        (Role::OnSecondaryContainer, c("text")),
        (Role::Tertiary, c("peach")),
        (Role::OnTertiary, on),
        (Role::OnTertiaryContainer, c("text")),
        (Role::Error, c("red")),
        (Role::OnError, on),
        (Role::OnErrorContainer, c("text")),
        (Role::Bg, c("base")),
        (Role::OnBg, c("text")),
        (Role::Surface, c("base")),
        (Role::Fg, c("text")),
        (Role::SurfaceVariant, c("surface0")),
        (Role::FgVariant, c("subtext0")),
        (Role::SurfaceDim, c("crust")),
        (Role::SurfaceBright, c("surface1")),
        (Role::SurfaceLowest, c("crust")),
        (Role::SurfaceLow, c("mantle")),
        (Role::SurfaceContainer, c("surface0")),
        (Role::SurfaceHigh, c("surface1")),
        (Role::SurfaceHighest, c("surface2")),
        (Role::InverseSurface, c("text")),
        (Role::InverseFg, c("base")),
        (Role::Outline, c("overlay0")),
        (Role::OutlineVariant, c("surface1")),
    ] {
        p.set(role, color);
    }
    Ok(p)
}

// ---------------------------------------------------------------------------
// base16 / base24

/// `key: value` pairs of a flat or once-nested YAML scheme, comments and
/// quotes removed.
fn yaml_pairs(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        // A `#` after whitespace (or first) and outside quotes starts a
        // comment; inside quotes it is a colour.
        let mut quote = None;
        let mut cut = line.len();
        let mut prev = ' ';
        for (i, ch) in line.char_indices() {
            match (quote, ch) {
                (None, '"' | '\'') => quote = Some(ch),
                (Some(q), c) if c == q => quote = None,
                (None, '#') if prev.is_whitespace() => {
                    cut = i;
                    break;
                }
                _ => {}
            }
            prev = ch;
        }
        let line = &line[..cut];
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let Some((k, v)) = t.split_once(':') else {
            continue;
        };
        let v = v.trim();
        let v = v
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(v);
        if !v.is_empty() {
            out.insert(k.trim().to_string(), v.to_string());
        }
    }
    out
}

/// A base16 or base24 scheme, mapped per tinted-theming's styling guide:
/// base00 the background, 01/02 lighter backgrounds, 03 comments, 04
/// dark foreground, 05 the foreground, 08 red, 0C cyan, 0D blue
/// (functions), 0E magenta; base24's 10/11 darker backgrounds.
pub fn base16(text: &str) -> Result<Partial, String> {
    let pairs = yaml_pairs(text);
    let get = |k: &str| -> Option<Color> {
        let v = pairs.get(k).or_else(|| pairs.get(&k.to_lowercase()))?;
        Color::from_hex(v)
    };
    let need = |k: &str| get(k).ok_or_else(|| format!("missing or bad `{k}`"));
    let b = |n: &str| get(&format!("base{n}"));
    let (bg, fg) = (need("base00")?, need("base05")?);
    let dark = match pairs.get("variant").map(String::as_str) {
        Some("light") => Some(false),
        Some("dark") => Some(true),
        _ => None,
    };
    let mut p = Partial {
        dark,
        ..Partial::default()
    };
    p.set(Role::Bg, bg);
    p.set(Role::Surface, bg);
    p.set(Role::Fg, fg);
    p.set(Role::OnBg, fg);
    let mut put = |role: Role, c: Option<Color>| {
        if let Some(c) = c {
            p.set(role, c);
        }
    };
    put(Role::SurfaceLow, b("01"));
    put(Role::SurfaceContainer, b("01"));
    put(Role::SurfaceHigh, b("02"));
    put(Role::SurfaceVariant, b("02"));
    put(Role::OutlineVariant, b("02"));
    put(Role::Outline, b("03"));
    put(Role::FgVariant, b("04"));
    put(Role::InverseSurface, b("05"));
    put(Role::InverseFg, Some(bg));
    put(Role::Error, b("08"));
    put(Role::Secondary, b("0C"));
    put(Role::Accent, b("0D"));
    put(Role::Tertiary, b("0E"));
    put(Role::SurfaceDim, b("10"));
    put(Role::SurfaceLowest, b("11"));
    put(Role::InverseAccent, b("16"));
    Ok(p)
}

// ---------------------------------------------------------------------------
// JSON helpers

fn json_color(v: &serde_json::Value) -> Option<Color> {
    match v {
        serde_json::Value::String(s) => Color::from_hex(s.trim()),
        serde_json::Value::Object(o) => {
            for k in ["color", "hex", "$value", "value"] {
                if let Some(c) = o.get(k).and_then(json_color) {
                    return Some(c);
                }
            }
            // W3C 2025 colour objects without `hex`.
            let comps = o.get("components")?.as_array()?;
            let space = o
                .get("colorSpace")
                .and_then(|s| s.as_str())
                .unwrap_or("srgb");
            if space != "srgb" || comps.len() != 3 {
                return None;
            }
            let n: Vec<f32> = comps
                .iter()
                .filter_map(|c| c.as_f64())
                .map(|c| c as f32)
                .collect();
            let a = o.get("alpha").and_then(|a| a.as_f64()).unwrap_or(1.0) as f32;
            (n.len() == 3).then(|| Color::new(n[0], n[1], n[2], a))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// matugen

/// matugen's JSON (`matugen image wall.jpg --json hex`). Material 3 role
/// names map 1:1 onto the palette ([`Role::m3`]).
pub fn matugen(text: &str) -> Result<Partial, String> {
    let json: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let colors = json
        .get("colors")
        .and_then(|c| c.as_object())
        .ok_or("no `colors` object")?;
    let mode = json
        .get("mode")
        .and_then(|m| m.as_str())
        .unwrap_or("dark")
        .to_lowercase();
    let mode = if mode == "light" { "light" } else { "dark" };
    let mut p = Partial {
        dark: Some(mode == "dark"),
        ..Partial::default()
    };
    // colors.<mode>.<role>
    if let Some(by_mode) = colors.get(mode).and_then(|m| m.as_object())
        && by_mode.values().any(|v| json_color(v).is_some())
    {
        for (k, v) in by_mode {
            if let (Some(r), Some(c)) = (Role::from_name(k), json_color(v)) {
                p.set(r, c);
            }
        }
    } else {
        // colors.<role>.<mode> (matugen 3 and later), or a plain value.
        for (k, v) in colors {
            let Some(r) = Role::from_name(k) else {
                continue;
            };
            let c = v
                .get(mode)
                .or_else(|| v.get("default"))
                .and_then(json_color)
                .or_else(|| json_color(v));
            if let Some(c) = c {
                p.set(r, c);
            }
        }
    }
    if p.roles.is_empty() {
        return Err(format!("no Material 3 roles for the `{mode}` mode"));
    }
    Ok(p)
}

// ---------------------------------------------------------------------------
// W3C design tokens

fn collect_tokens(
    v: &serde_json::Value,
    path: &mut Vec<String>,
    ty: Option<&str>,
    out: &mut Vec<(Vec<String>, serde_json::Value, Option<String>)>,
) {
    let Some(o) = v.as_object() else { return };
    let ty = o.get("$type").and_then(|t| t.as_str()).or(ty);
    if let Some(value) = o.get("$value") {
        out.push((path.clone(), value.clone(), ty.map(str::to_string)));
        return;
    }
    for (k, child) in o {
        if k.starts_with('$') {
            continue;
        }
        path.push(k.clone());
        collect_tokens(child, path, ty, out);
        path.pop();
    }
}

/// The role a token path names: the longest suffix of its words
/// (`md.sys.color.on-primary-container` → `on_primary_container`) that
/// is a role's Strand or Material 3 name.
fn role_of_path(path: &[String]) -> Option<Role> {
    let words: Vec<String> = path
        .iter()
        .flat_map(|s| {
            s.to_lowercase()
                .split(['-', '_', ' '])
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|w| !w.is_empty())
        .collect();
    (0..words.len()).find_map(|i| Role::from_name(&words[i..].join("_")))
}

/// W3C design-token JSON (DTCG format).
pub fn w3c(text: &str) -> Result<Partial, String> {
    let json: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let mut tokens = Vec::new();
    collect_tokens(&json, &mut Vec::new(), None, &mut tokens);
    let by_path: BTreeMap<String, serde_json::Value> = tokens
        .iter()
        .map(|(p, v, _)| (p.join("."), v.clone()))
        .collect();
    let resolve = |v: &serde_json::Value| -> Option<Color> {
        let mut v = v.clone();
        for _ in 0..16 {
            match v.as_str().map(str::trim) {
                Some(s) if s.starts_with('{') && s.ends_with('}') => {
                    v = by_path.get(&s[1..s.len() - 1])?.clone();
                }
                _ => return json_color(&v),
            }
        }
        None
    };
    let mut p = Partial::default();
    for (path, value, ty) in &tokens {
        if ty.as_deref().is_some_and(|t| t != "color") {
            continue;
        }
        if let (Some(r), Some(c)) = (role_of_path(path), resolve(value)) {
            p.set(r, c);
        }
    }
    if p.roles.is_empty() {
        return Err("no colour token names a palette role".into());
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_paths_name_roles_by_their_longest_suffix() {
        let p = |s: &str| s.split('.').map(str::to_string).collect::<Vec<_>>();
        assert_eq!(
            role_of_path(&p("md.sys.color.on-primary-container")),
            Some(Role::OnAccentContainer)
        );
        assert_eq!(role_of_path(&p("color.accent")), Some(Role::Accent));
        assert_eq!(role_of_path(&p("color.on_accent")), Some(Role::OnAccent));
        assert_eq!(
            role_of_path(&p("colors.surface.container.high")),
            Some(Role::SurfaceHigh)
        );
        assert_eq!(
            role_of_path(&p("colors.surface_container_high")),
            Some(Role::SurfaceHigh)
        );
        assert_eq!(role_of_path(&p("color.fg.muted")), None);
    }

    #[test]
    fn yaml_pairs_strip_quotes_and_comments() {
        let y = "system: \"base16\"\n# a comment\npalette:\n  base00: \"#1e1e2e\" # bg\n  base01: '181825'\n";
        let p = yaml_pairs(y);
        assert_eq!(p["base00"], "#1e1e2e");
        assert_eq!(p["base01"], "181825");
        assert_eq!(p["system"], "base16");
    }
}
