//! (M5) Templates: text of any format with `{{ expr }}` holes over the
//! theme (spec.md 9.8; decisions.md m5-tools, "template expressions and
//! formats"). A template file (`strand export --template`, a `file`'s
//! `template:`) and a `file`'s `text:` expression compile to a pure IR,
//! [`Template`], which [`Template::render`] turns into bytes over a
//! resolved [`TokenTable`] and, when a hole reads `.light()` or
//! `.dark()`, the other mode's table.
//!
//! The scope is the theme alone: token paths and their methods (the
//! render thread's `alpha`, `mix`, `lighten`, `darken`, and the
//! template-only channels `.red()`, `.green()`, `.blue()`, `.opacity()`
//! and modes `.light()`, `.dark()`), the palette's extras and identity
//! (`$palette.<name>`), literals, `??`, `join` and the format functions
//! `hex` and `argb`. Anything else (a service, a `state`, a `let`, a
//! `fn`, an operator the scope lacks) is an error saying a template reads
//! the theme only, so a rendered template depends on nothing but the
//! token tables.

mod render;

use strand_scene::{Color, Length, PropValue, TokenMethod};

use crate::diagnostic::Diagnostic;
use crate::source::FileId;
use crate::syntax::ast::{self, ArgKind, BinaryOp, ExprKind, UnaryOp, Unit};
use crate::syntax::{Span, parse_expr_in};

/// A compiled template: text runs and holes, in order.
#[derive(Clone, Debug, PartialEq)]
pub struct Template {
    file: FileId,
    parts: Vec<Part>,
    other_mode: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum Part {
    Text(String),
    Hole(Node),
}

/// Which mode a token path is read in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Current,
    Light,
    Dark,
}

/// A colour channel method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Chan {
    Red,
    Green,
    Blue,
    Opacity,
}

/// A compiled hole expression.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Node {
    /// A literal (a colour, a number, a length, a string, a bool).
    Lit(PropValue, Span),
    /// `null`: absent, so `??` replaces it.
    Null(Span),
    /// `$accent`, `$fg.muted.light()`: a token by path, without the `$`.
    Token {
        path: String,
        mode: Mode,
        span: Span,
    },
    /// `$palette.mauve`, `$palette.name`: an extra or the identity.
    Palette {
        name: String,
        mode: Mode,
        span: Span,
    },
    /// `$surface.mix($fg, 8%)`: a render-thread colour method.
    Method {
        recv: Box<Node>,
        method: TokenMethod,
        args: Vec<Node>,
        span: Span,
    },
    /// `$fg.red()`.
    Channel {
        recv: Box<Node>,
        chan: Chan,
        span: Span,
    },
    /// `a ?? b`.
    Coalesce(Box<Node>, Box<Node>),
    /// `hex(c, alpha: false, hash: true)`.
    Hex {
        color: Box<Node>,
        alpha: Option<Box<Node>>,
        hash: Option<Box<Node>>,
        span: Span,
    },
    /// `argb(c)`.
    Argb { color: Box<Node>, span: Span },
    /// `join(sep, a, b, …)`.
    Join {
        sep: Box<Node>,
        parts: Vec<Node>,
        span: Span,
    },
}

/// The diagnostic every out-of-scope name and construct gets.
fn not_theme(span: Span, what: &str) -> Diagnostic {
    Diagnostic::error(
        "template::not_theme",
        format!("a template reads the theme only: {what} is not in its scope"),
    )
    .with_label(span, "not a token, a palette entry or a template function")
    .with_help(
        "a hole reads tokens (`$accent`, `$fg.muted.light()`), `$palette.<name>`, literals, \
         `??`, `join`, `hex` and `argb`",
    )
}

/// Compiles the holes of one file (or one `text:` expression), keeping
/// every error.
struct Compiler {
    file: FileId,
    diags: Vec<Diagnostic>,
    other_mode: bool,
}

impl Template {
    /// Compiles template text `src` (the file `file` of the caller's
    /// [`crate::SourceMap`]). Every error is reported, each with its
    /// location in `file`.
    pub fn compile(file: FileId, src: &str) -> Result<Template, Vec<Diagnostic>> {
        let mut c = Compiler {
            file,
            diags: Vec::new(),
            other_mode: false,
        };
        let mut parts = Vec::new();
        let bytes = src.as_bytes();
        let mut text_start = 0;
        let mut i = 0;
        while i + 1 < bytes.len() {
            if !(bytes[i] == b'{' && bytes[i + 1] == b'{') {
                i += 1;
                continue;
            }
            if text_start < i {
                parts.push(Part::Text(src[text_start..i].to_string()));
            }
            let open = i;
            let body = i + 2;
            match hole_end(bytes, body) {
                Some(close) => {
                    let (Ok(a), Ok(b)) = (u32::try_from(body), u32::try_from(close)) else {
                        c.error(
                            Diagnostic::error("template::too_large", "the template is too large")
                                .with_label(Span::at(0), "past 4 GiB"),
                        );
                        break;
                    };
                    if src[body..close].trim().is_empty() {
                        c.error(
                            Diagnostic::error("template::empty_hole", "this hole is empty")
                                .with_label(Span::new(a - 2, b + 2), "`{{` needs an expression")
                                .with_help("a literal `{{` is written `{{ \"{{\" }}`"),
                        );
                    } else {
                        let (e, diags) = parse_expr_in(src, a, b);
                        if diags.is_empty() {
                            let node = c.node(&e);
                            parts.push(Part::Hole(node));
                        } else {
                            for d in diags {
                                c.error(d);
                            }
                        }
                    }
                    i = close + 2;
                    text_start = i;
                }
                None => {
                    let at = u32::try_from(open).unwrap_or(u32::MAX - 2);
                    c.error(
                        Diagnostic::error("template::unclosed", "this `{{` is never closed")
                            .with_label(Span::new(at, at + 2), "no `}}` after it")
                            .with_help("a literal `{{` is written `{{ \"{{\" }}`"),
                    );
                    text_start = bytes.len();
                    break;
                }
            }
        }
        if text_start < src.len() {
            parts.push(Part::Text(src[text_start..].to_string()));
        }
        c.finish(parts)
    }

    /// Compiles one expression in the template scope (a `file`'s `text:`
    /// prop), in `file` of the config.
    pub fn compile_expr(file: FileId, e: &ast::Expr) -> Result<Template, Vec<Diagnostic>> {
        let mut c = Compiler {
            file,
            diags: Vec::new(),
            other_mode: false,
        };
        let node = c.node(e);
        c.finish(vec![Part::Hole(node)])
    }

    /// Whether a hole reads `.light()` or `.dark()`: rendering it needs
    /// the other mode's table.
    pub fn reads_other_mode(&self) -> bool {
        self.other_mode
    }

    /// The file its diagnostics point into.
    pub fn file(&self) -> FileId {
        self.file
    }

    /// The token paths the holes read (without `$`), for `strand check`
    /// against the tokens a config declares; `$palette.*` is left out,
    /// since extras depend on the palette at run time.
    pub fn token_paths(&self) -> Vec<(String, Span)> {
        fn walk(n: &Node, out: &mut Vec<(String, Span)>) {
            match n {
                Node::Token { path, span, .. } => out.push((path.clone(), *span)),
                Node::Lit(..) | Node::Null(_) | Node::Palette { .. } => {}
                Node::Method { recv, args, .. } => {
                    walk(recv, out);
                    args.iter().for_each(|a| walk(a, out));
                }
                Node::Channel { recv, .. } => walk(recv, out),
                Node::Coalesce(a, b) => {
                    walk(a, out);
                    walk(b, out);
                }
                Node::Hex {
                    color, alpha, hash, ..
                } => {
                    walk(color, out);
                    alpha.iter().chain(hash).for_each(|a| walk(a, out));
                }
                Node::Argb { color, .. } => walk(color, out),
                Node::Join { sep, parts, .. } => {
                    walk(sep, out);
                    parts.iter().for_each(|a| walk(a, out));
                }
            }
        }
        let mut out = Vec::new();
        for p in &self.parts {
            if let Part::Hole(n) = p {
                walk(n, &mut out);
            }
        }
        out
    }
}

/// Where the hole starting at `from` ends: the first `}}` outside a
/// string literal.
fn hole_end(bytes: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    let mut in_str = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if in_str => i += 1,
            b'"' => in_str = !in_str,
            // A string never spans lines: an unterminated one ends there,
            // as the lexer reads it.
            b'\n' if in_str => in_str = false,
            b'}' if !in_str && bytes.get(i + 1) == Some(&b'}') => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

impl Compiler {
    fn error(&mut self, d: Diagnostic) {
        self.diags.push(d.in_file(self.file));
    }

    fn finish(self, parts: Vec<Part>) -> Result<Template, Vec<Diagnostic>> {
        if self.diags.is_empty() {
            Ok(Template {
                file: self.file,
                parts,
                other_mode: self.other_mode,
            })
        } else {
            Err(self.diags)
        }
    }

    /// An error node: compilation reports and goes on, so every error of
    /// the file is found.
    fn fail(&mut self, d: Diagnostic, span: Span) -> Node {
        self.error(d);
        Node::Null(span)
    }

    fn node(&mut self, e: &ast::Expr) -> Node {
        match &e.kind {
            ExprKind::Paren(inner) => self.node(inner),
            ExprKind::Number(n) => Node::Lit(number(n.value, n.unit), e.span),
            ExprKind::String(s) => Node::Lit(PropValue::Text(s.value.clone()), e.span),
            ExprKind::Color(c) => {
                let [r, g, b, a] = c.rgba;
                Node::Lit(PropValue::Color(Color::from_rgba8(r, g, b, a)), e.span)
            }
            ExprKind::Bool(b) => Node::Lit(PropValue::Bool(*b), e.span),
            ExprKind::Null => Node::Null(e.span),
            ExprKind::Unary {
                op: UnaryOp::Neg,
                expr,
            } if matches!(expr.kind, ExprKind::Number(_)) => {
                let ExprKind::Number(n) = &expr.kind else {
                    return Node::Null(e.span);
                };
                Node::Lit(number(-n.value, n.unit), e.span)
            }
            ExprKind::Token(key) => self.path(key, Mode::Current),
            ExprKind::Binary {
                op: BinaryOp::Coalesce,
                lhs,
                rhs,
            } => Node::Coalesce(Box::new(self.node(lhs)), Box::new(self.node(rhs))),
            ExprKind::Call { callee, args } => self.call(e.span, callee, args),
            ExprKind::Name(id) => {
                let what = format!("`{}`", id.name);
                self.fail(not_theme(id.span, &what), e.span)
            }
            ExprKind::Field { .. } => self.fail(not_theme(e.span, "a field"), e.span),
            ExprKind::Binary { op, .. } => {
                let what = format!("`{}`", op.as_str());
                self.fail(not_theme(e.span, &what), e.span)
            }
            ExprKind::Unary { op, .. } => {
                let what = format!("`{}`", op.as_str());
                self.fail(not_theme(e.span, &what), e.span)
            }
            ExprKind::Ternary { .. } => self.fail(not_theme(e.span, "`?:`"), e.span),
            ExprKind::Array(_) | ExprKind::Commas(_) | ExprKind::Spaced(_) => {
                self.fail(not_theme(e.span, "a list"), e.span)
            }
            ExprKind::Index { .. } => self.fail(not_theme(e.span, "indexing"), e.span),
            ExprKind::Lambda { .. } => self.fail(not_theme(e.span, "a lambda"), e.span),
            ExprKind::Match(_) => self.fail(not_theme(e.span, "`match`"), e.span),
            ExprKind::Error => Node::Null(e.span),
        }
    }

    /// `$a.b`, read in `mode`.
    fn path(&mut self, key: &ast::TokenKey, mode: Mode) -> Node {
        let segs: Vec<&str> = key.segments.iter().map(|s| s.name.as_str()).collect();
        if mode != Mode::Current {
            self.other_mode = true;
        }
        if segs.first() == Some(&strand_scene::tokens::PALETTE_SEGMENT) {
            if segs.len() < 2 {
                return self.fail(
                    Diagnostic::error(
                        "template::palette_entry",
                        "`$palette` names the palette's extras and identity, one at a time",
                    )
                    .with_label(key.span, "name an entry")
                    .with_help("`$palette.name`, `$palette.dark`, `$palette.mauve`"),
                    key.span,
                );
            }
            return Node::Palette {
                name: segs[1..].join("."),
                mode,
                span: key.span,
            };
        }
        Node::Token {
            path: segs.join("."),
            mode,
            span: key.span,
        }
    }

    fn call(&mut self, span: Span, callee: &ast::Expr, args: &[ast::Arg]) -> Node {
        match &callee.kind {
            ExprKind::Field { base, name, .. } => self.method(span, base, name, args),
            ExprKind::Name(f) => match f.name.as_str() {
                "hex" => self.hex(span, args),
                "argb" => {
                    let given = self.positional(span, "argb", args, 1, 1);
                    let [color] = given.as_slice() else {
                        return Node::Null(span);
                    };
                    let color = self.node(color);
                    Node::Argb {
                        color: Box::new(color),
                        span,
                    }
                }
                "join" => {
                    let given = self.positional(span, "join", args, 1, usize::MAX);
                    let Some((sep, rest)) = given.split_first() else {
                        return Node::Null(span);
                    };
                    let sep = self.node(sep);
                    let parts = rest.iter().map(|a| self.node(a)).collect();
                    Node::Join {
                        sep: Box::new(sep),
                        parts,
                        span,
                    }
                }
                other => {
                    let what = format!("`{other}(…)`");
                    self.fail(not_theme(f.span, &what), span)
                }
            },
            _ => self.fail(not_theme(callee.span, "this call"), span),
        }
    }

    /// The positional arguments of `f`, between `min` and `max` of them.
    fn positional<'e>(
        &mut self,
        span: Span,
        f: &str,
        args: &'e [ast::Arg],
        min: usize,
        max: usize,
    ) -> Vec<&'e ast::Expr> {
        let mut out = Vec::new();
        for a in args {
            match &a.kind {
                ArgKind::Positional => out.push(&a.value),
                _ => {
                    self.error(
                        Diagnostic::error(
                            "template::args",
                            format!("`{f}` takes no named argument"),
                        )
                        .with_label(a.span, "not an argument of this function"),
                    );
                }
            }
        }
        if out.len() < min || out.len() > max {
            let want = match (min, max) {
                (a, b) if a == b => format!("{a}"),
                (a, usize::MAX) => format!("at least {a}"),
                (a, b) => format!("{a} to {b}"),
            };
            self.error(
                Diagnostic::error(
                    "template::args",
                    format!("`{f}` takes {want} argument(s), not {}", out.len()),
                )
                .with_label(span, "this call"),
            );
            return Vec::new();
        }
        out
    }

    /// `hex(c, alpha: bool, hash: bool)`.
    fn hex(&mut self, span: Span, args: &[ast::Arg]) -> Node {
        let mut color = None;
        let (mut alpha, mut hash) = (None, None);
        for a in args {
            match &a.kind {
                ArgKind::Positional if color.is_none() => {
                    color = Some(Box::new(self.node(&a.value)));
                }
                ArgKind::Named(n) if n.name == "alpha" && alpha.is_none() => {
                    alpha = Some(Box::new(self.node(&a.value)));
                }
                ArgKind::Named(n) if n.name == "hash" && hash.is_none() => {
                    hash = Some(Box::new(self.node(&a.value)));
                }
                _ => self.error(
                    Diagnostic::error(
                        "template::args",
                        "`hex` takes a colour, `alpha: bool` and `hash: bool`",
                    )
                    .with_label(a.span, "not an argument of `hex`"),
                ),
            }
        }
        let Some(color) = color else {
            return self.fail(
                Diagnostic::error("template::args", "`hex` needs a colour")
                    .with_label(span, "`hex(c)`"),
                span,
            );
        };
        Node::Hex {
            color,
            alpha,
            hash,
            span,
        }
    }

    /// `recv.name(args)`.
    fn method(
        &mut self,
        span: Span,
        recv: &ast::Expr,
        name: &ast::Ident,
        args: &[ast::Arg],
    ) -> Node {
        let mode = match name.name.as_str() {
            "light" => Some(Mode::Light),
            "dark" => Some(Mode::Dark),
            _ => None,
        };
        if let Some(mode) = mode {
            if !args.is_empty() {
                self.error(
                    Diagnostic::error(
                        "template::args",
                        format!("`.{}()` takes no arguments", name.name),
                    )
                    .with_label(span, "this call"),
                );
            }
            let ExprKind::Token(key) = &recv.kind else {
                return self.fail(
                    Diagnostic::error(
                        "template::mode",
                        format!("`.{}()` reads a token path in that mode", name.name),
                    )
                    .with_label(recv.span, "not a token path")
                    .with_help(
                        "name a derived value as a token in your `tokens` and read \
                         `$name.light()`",
                    ),
                    span,
                );
            };
            return self.path(key, mode);
        }
        let chan = match name.name.as_str() {
            "red" => Some(Chan::Red),
            "green" => Some(Chan::Green),
            "blue" => Some(Chan::Blue),
            "opacity" => Some(Chan::Opacity),
            _ => None,
        };
        if let Some(chan) = chan {
            if !args.is_empty() {
                self.error(
                    Diagnostic::error(
                        "template::args",
                        format!("`.{}()` takes no arguments", name.name),
                    )
                    .with_label(span, "this call"),
                );
            }
            let recv = self.node(recv);
            return Node::Channel {
                recv: Box::new(recv),
                chan,
                span,
            };
        }
        let Some(method) = TokenMethod::from_name(&name.name) else {
            let what = format!("the method `.{}()`", name.name);
            return self.fail(not_theme(name.span, &what), span);
        };
        let recv = self.node(recv);
        let mut list = Vec::new();
        for a in args {
            match a.kind {
                ArgKind::Positional => list.push(self.node(&a.value)),
                _ => self.error(
                    Diagnostic::error(
                        "template::args",
                        format!("`.{}()` takes positional arguments", method.name()),
                    )
                    .with_label(a.span, "a named argument"),
                ),
            }
        }
        Node::Method {
            recv: Box::new(recv),
            method,
            args: list,
            span,
        }
    }
}

/// `s` seconds as a duration value; what a `Duration` cannot hold is the
/// longest it can.
fn secs(s: f64) -> PropValue {
    PropValue::Duration(
        std::time::Duration::try_from_secs_f64(s.max(0.0)).unwrap_or(std::time::Duration::MAX),
    )
}

/// A number literal as the token table holds it: pixels and plain
/// numbers as numbers, other units as their kinds.
fn number(v: f64, unit: Option<Unit>) -> PropValue {
    let f = v as f32;
    match unit {
        None | Some(Unit::Px) => PropValue::Number(f),
        Some(Unit::Percent) => PropValue::Length(Length::Percent(f)),
        Some(Unit::Ch) => PropValue::Length(Length::Ch(f)),
        Some(Unit::Deg) => PropValue::Angle(f),
        Some(Unit::S) => secs(v),
        Some(Unit::Ms) => secs(v / 1000.0),
        Some(Unit::Min) => secs(v * 60.0),
        Some(Unit::H) => secs(v * 3600.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holes_end_at_the_first_close_outside_a_string() {
        assert_eq!(hole_end(b" $a }} x", 0), Some(4));
        assert_eq!(hole_end(br#" "}}" }}"#, 0), Some(6));
        assert_eq!(hole_end(br#" "a\"}}" }}"#, 0), Some(9));
        assert_eq!(hole_end(b" $a } }", 0), None);
    }
}
