//! Rendering a [`Template`] over resolved token tables: pure, so the
//! output host renders on the logic thread after the tick's diff and the
//! CLI renders offline with the same code (decisions.md
//! m5-integration-w1).

use std::fmt::Write as _;

use strand_scene::{
    Color, Length, PropValue, TimeContext, TokenExpr, TokenScope, TokenTable, Transition,
};
use strand_theme::export::number;

use super::{Chan, Mode, Node, Part, Template};
use crate::diagnostic::{Diagnostic, closest};
use crate::syntax::Span;

/// Why a hole has no value.
enum Fault {
    /// Absent (a missing extra, the other mode a theme lacks): `??`
    /// replaces it; otherwise it is this error, `join` included.
    Absent(Diagnostic),
    /// A `null` literal: `??` replaces it and `join` skips it, as the
    /// shell's `join` skips nulls; otherwise it is this error.
    Null(Diagnostic),
    /// An error `??` does not catch.
    Error(Diagnostic),
}

type Eval = Result<PropValue, Fault>;

struct Ctx<'a> {
    current: &'a TokenTable,
    other: Option<&'a TokenTable>,
}

/// The table's mode, from its palette identity.
fn is_dark(t: &TokenTable) -> Option<bool> {
    match t.palette_value("dark") {
        Some(PropValue::Bool(b)) => Some(*b),
        _ => None,
    }
}

impl Template {
    /// Renders the template over `current` and, when it reads the other
    /// mode ([`Template::reads_other_mode`]), `other` (the theme
    /// evaluated with `system.dark` negated; `None` when there is none).
    /// The same tables always give the same bytes. On any error nothing
    /// is returned but every error, each at its hole.
    pub fn render(
        &self,
        current: &TokenTable,
        other: Option<&TokenTable>,
    ) -> Result<Vec<u8>, Vec<Diagnostic>> {
        let cx = Ctx { current, other };
        let mut out = String::new();
        let mut errors = Vec::new();
        for p in &self.parts {
            match p {
                Part::Text(t) => out.push_str(t),
                Part::Hole(n) => {
                    let span = n.span();
                    let r = cx
                        .eval(n)
                        .and_then(|v| write(&v, span).map_err(Fault::Error));
                    match r {
                        Ok(s) => out.push_str(&s),
                        Err(Fault::Absent(d) | Fault::Null(d) | Fault::Error(d)) => {
                            errors.push(d.in_file(self.file));
                        }
                    }
                }
            }
        }
        if errors.is_empty() {
            Ok(out.into_bytes())
        } else {
            Err(errors)
        }
    }
}

impl Node {
    fn span(&self) -> Span {
        match self {
            Node::Lit(_, s)
            | Node::Null(s)
            | Node::Token { span: s, .. }
            | Node::Palette { span: s, .. }
            | Node::Method { span: s, .. }
            | Node::Channel { span: s, .. }
            | Node::Hex { span: s, .. }
            | Node::Argb { span: s, .. }
            | Node::Join { span: s, .. } => *s,
            Node::Coalesce(a, b) => a.span().to(b.span()),
        }
    }
}

/// What a value's kind is called in an error.
fn kind(v: &PropValue) -> &'static str {
    match v {
        PropValue::Unset => "an unset value",
        PropValue::Bool(_) => "a bool",
        PropValue::Number(_) => "a number",
        PropValue::Length(_) => "a length",
        PropValue::Insets(_) => "insets",
        PropValue::Corners(_) => "corners",
        PropValue::Color(_) => "a colour",
        PropValue::Paint(_) => "a paint",
        PropValue::Border(_) => "a border",
        PropValue::Shadow(_) => "a shadow list",
        PropValue::Text(_) => "a string",
        PropValue::Font(_) => "a font",
        PropValue::Keyword(_) => "a keyword",
        PropValue::Angle(_) => "an angle",
        PropValue::Duration(_) => "a duration",
        PropValue::List(_) => "a list",
        PropValue::Token(_) => "a token expression",
        PropValue::Transition(Transition::Spring { .. }) => "a spring",
        PropValue::Transition(_) => "a transition",
        PropValue::Pose(_) => "a pose",
        PropValue::Tokens(_) => "a token set",
        PropValue::Call { .. } => "a call",
        PropValue::Node(_) => "a node",
        PropValue::Uniforms(_) => "uniforms",
        PropValue::DrawList(_) => "a drawing",
        PropValue::Shader(_) => "a shader",
        PropValue::Keyframes(_) => "keyframes",
    }
}

/// `c` as lowercase hex: `#rrggbb[aa]`, gamut-mapped first.
fn hex(c: Color, alpha: bool, hash: bool) -> String {
    let [r, g, b, a] = c.gamut_mapped().to_rgba8();
    let mut s = String::with_capacity(9);
    if hash {
        s.push('#');
    }
    let _ = write!(s, "{r:02x}{g:02x}{b:02x}");
    if alpha {
        let _ = write!(s, "{a:02x}");
    }
    s
}

fn translucent(c: Color) -> bool {
    c.gamut_mapped().to_rgba8()[3] < 255
}

fn translucent_error(span: Span) -> Diagnostic {
    Diagnostic::error(
        "template::translucent",
        "this colour is translucent, and `hex` writes it opaque only when asked",
    )
    .with_label(span, "its alpha is under 1")
    .with_help("write `hex(c, alpha: true)` for `#rrggbbaa`, or `argb(c)`")
}

/// A value as a hole writes it (spec.md 9.8, "Values").
fn write(v: &PropValue, span: Span) -> Result<String, Diagnostic> {
    Ok(match v {
        PropValue::Color(c) => {
            if translucent(*c) {
                return Err(translucent_error(span));
            }
            hex(*c, false, true)
        }
        PropValue::Number(n) | PropValue::Angle(n) | PropValue::Length(Length::Px(n)) => {
            number(f64::from(*n))
        }
        PropValue::Duration(d) => number(d.as_secs_f64() * 1000.0),
        PropValue::Length(Length::Percent(n)) => format!("{}%", number(f64::from(*n))),
        PropValue::Length(Length::Ch(n)) => format!("{}ch", number(f64::from(*n))),
        PropValue::Length(Length::Auto) => "auto".into(),
        PropValue::Text(s) | PropValue::Keyword(s) => s.clone(),
        PropValue::Bool(b) => b.to_string(),
        other => {
            return Err(Diagnostic::error(
                "template::value",
                format!("{} has no one text form", kind(other)),
            )
            .with_label(span, format!("this is {}", kind(other)))
            .with_help("write the parts you need as tokens of their own"));
        }
    })
}

/// A method argument as the render thread's token methods read it: a
/// percentage is a fraction (`mix($fg, 8%)`), as the VM gives it.
fn arg(v: PropValue) -> TokenExpr {
    match v {
        PropValue::Length(Length::Percent(p)) => TokenExpr::value(PropValue::Number(p / 100.0)),
        PropValue::Length(Length::Px(n)) => TokenExpr::value(PropValue::Number(n)),
        v => TokenExpr::value(v),
    }
}

impl Ctx<'_> {
    /// The table `mode` reads.
    fn table(&self, mode: Mode, span: Span, path: &str) -> Result<&TokenTable, Fault> {
        let want = match mode {
            Mode::Current => return Ok(self.current),
            Mode::Light => false,
            Mode::Dark => true,
        };
        if is_dark(self.current) == Some(want) {
            return Ok(self.current);
        }
        match self.other {
            Some(o) if is_dark(o) == Some(want) => Ok(o),
            _ => {
                let name = if want { "dark" } else { "light" };
                Err(Fault::Absent(
                    Diagnostic::error("template::no_mode", format!("the theme has no {name} mode"))
                        .with_label(span, format!("reads the {name} mode"))
                        .with_help(format!(
                            "the theme gives the same palette whichever way `system.dark` is set; \
                         `??` gives a fallback: `{{{{ ${path}.{name}() ?? ${path} }}}}`"
                        )),
                ))
            }
        }
    }

    fn eval(&self, n: &Node) -> Eval {
        match n {
            Node::Lit(v, _) => Ok(v.clone()),
            Node::Null(span) => Err(Fault::Null(
                Diagnostic::error("template::null", "this hole has no value")
                    .with_label(*span, "null")
                    .with_help("`??` gives a fallback"),
            )),
            Node::Token { path, mode, span } => {
                let t = self.table(*mode, *span, path)?;
                let levels = [t];
                let scope = TokenScope::new(&levels).with_time(Some(TimeContext::default()));
                if let Some(v) = scope.lookup(path) {
                    return Ok(v);
                }
                if t.tokens.contains_key(path) || t.derived.contains_key(path) {
                    return Err(Fault::Error(
                        Diagnostic::error(
                            "template::token_value",
                            format!("`${path}` has no value"),
                        )
                        .with_label(*span, "it does not evaluate"),
                    ));
                }
                let names = t.tokens.keys().chain(t.derived.keys()).map(String::as_str);
                let mut d = Diagnostic::error(
                    "template::unknown_token",
                    format!("there is no token `${path}`"),
                )
                .with_label(*span, "unknown token");
                if let Some(s) = closest(path, names) {
                    d = d.with_help(format!("did you mean `${s}`?"));
                }
                Err(Fault::Error(d))
            }
            Node::Palette { name, mode, span } => {
                let t = self.table(*mode, *span, &format!("palette.{name}"))?;
                if let Some(v) = t.palette_value(name) {
                    return Ok(v.clone());
                }
                let which = match t.palette_value("name") {
                    Some(PropValue::Text(n)) => format!(" ({n})"),
                    _ => String::new(),
                };
                let mut d = Diagnostic::error(
                    "template::no_extra",
                    format!("this palette{which} has no `$palette.{name}`"),
                )
                .with_label(*span, "not in this palette")
                .with_help(format!(
                    "extras depend on the palette; `??` gives a fallback: \
                     `$palette.{name} ?? $accent`"
                ));
                if let Some(s) = closest(name, t.palette.iter().map(|(n, _)| n.as_str())) {
                    d = d.with_help(format!(
                        "did you mean `$palette.{s}`? Extras depend on the palette; `??` \
                         gives a fallback"
                    ));
                }
                Err(Fault::Absent(d))
            }
            Node::Coalesce(a, b) => match self.eval(a) {
                Err(Fault::Absent(_) | Fault::Null(_)) => self.eval(b),
                r => r,
            },
            Node::Method {
                recv,
                method,
                args,
                span,
            } => {
                let base = self.color(recv, &format!("`.{}()`", method.name()))?;
                let mut list = Vec::with_capacity(args.len());
                for a in args {
                    list.push(arg(self.eval(a)?));
                }
                let e = TokenExpr::value(PropValue::Color(base)).call(*method, list);
                match self.current.eval(&e) {
                    Some(v) => Ok(v),
                    None => Err(Fault::Error(
                        Diagnostic::error(
                            "template::method",
                            format!("`.{}()` got arguments it cannot use", method.name()),
                        )
                        .with_label(*span, "this call"),
                    )),
                }
            }
            Node::Channel { recv, chan, .. } => {
                let name = match chan {
                    Chan::Red => "`.red()`",
                    Chan::Green => "`.green()`",
                    Chan::Blue => "`.blue()`",
                    Chan::Opacity => "`.opacity()`",
                };
                let c = self.color(recv, name)?;
                let [r, g, b, _] = c.gamut_mapped().to_rgba8();
                Ok(PropValue::Number(match chan {
                    Chan::Red => f32::from(r),
                    Chan::Green => f32::from(g),
                    Chan::Blue => f32::from(b),
                    Chan::Opacity => c.a.clamp(0.0, 1.0),
                }))
            }
            Node::Hex {
                color,
                alpha,
                hash,
                span,
            } => {
                let c = self.color(color, "`hex`")?;
                let flag =
                    |n: &Option<Box<Node>>, default: bool, what: &str| -> Result<bool, Fault> {
                        match n {
                            None => Ok(default),
                            Some(n) => match self.eval(n)? {
                                PropValue::Bool(b) => Ok(b),
                                other => Err(Fault::Error(
                                    Diagnostic::error(
                                        "template::args",
                                        format!("`{what}:` is a bool, not {}", kind(&other)),
                                    )
                                    .with_label(n.span(), "true or false"),
                                )),
                            },
                        }
                    };
                let alpha = flag(alpha, false, "alpha")?;
                let hash = flag(hash, true, "hash")?;
                if !alpha && translucent(c) {
                    return Err(Fault::Error(translucent_error(*span)));
                }
                Ok(PropValue::Text(hex(c, alpha, hash)))
            }
            Node::Argb { color, .. } => {
                let c = self.color(color, "`argb`")?;
                let [r, g, b, a] = c.gamut_mapped().to_rgba8();
                Ok(PropValue::Text(format!("0x{a:02x}{r:02x}{g:02x}{b:02x}")))
            }
            Node::Join { sep, parts, .. } => {
                let sep = self.text(sep)?;
                let mut out = Vec::with_capacity(parts.len());
                for p in parts {
                    // `join` skips nulls, as in the shell; an absent
                    // extra or mode is not null and stays an error
                    // unless `??` replaces it (spec.md 9.5, 9.8).
                    match self.eval(p) {
                        Ok(v) => out.push(write(&v, p.span()).map_err(Fault::Error)?),
                        Err(Fault::Null(_)) => {}
                        Err(e) => return Err(e),
                    }
                }
                Ok(PropValue::Text(out.join(&sep)))
            }
        }
    }

    /// `n` as text.
    fn text(&self, n: &Node) -> Result<String, Fault> {
        let v = self.eval(n)?;
        write(&v, n.span()).map_err(Fault::Error)
    }

    /// `n`, which `what` needs to be a colour.
    fn color(&self, n: &Node, what: &str) -> Result<Color, Fault> {
        match self.eval(n)? {
            PropValue::Color(c) => Ok(c),
            other => Err(Fault::Error(
                Diagnostic::error(
                    "template::not_colour",
                    format!("{what} needs a colour, not {}", kind(&other)),
                )
                .with_label(n.span(), format!("this is {}", kind(&other))),
            )),
        }
    }
}
