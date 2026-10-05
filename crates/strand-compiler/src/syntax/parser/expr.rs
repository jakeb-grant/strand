//! Expressions, prop values, types, parameters and patterns.

use crate::diagnostic::Diagnostic;

use super::super::ast::*;
use super::super::lexer::split_number;
use super::super::span::Span;
use super::{K, Parser, TREE_KEYWORDS, Tok};

/// Binding power and associativity of binary operators (higher binds
/// tighter). See `docs/grammar.md`, "Expressions".
fn binary_op(kind: K) -> Option<(BinaryOp, u8, bool)> {
    Some(match kind {
        K::QuestionQuestion => (BinaryOp::Coalesce, 1, true),
        K::OrOr => (BinaryOp::Or, 2, false),
        K::AndAnd => (BinaryOp::And, 3, false),
        K::EqEq => (BinaryOp::Eq, 4, false),
        K::BangEq => (BinaryOp::Ne, 4, false),
        K::Lt => (BinaryOp::Lt, 5, false),
        K::LtEq => (BinaryOp::Le, 5, false),
        K::Gt => (BinaryOp::Gt, 5, false),
        K::GtEq => (BinaryOp::Ge, 5, false),
        K::Plus => (BinaryOp::Add, 6, false),
        K::Minus => (BinaryOp::Sub, 6, false),
        K::Star => (BinaryOp::Mul, 7, false),
        K::Slash => (BinaryOp::Div, 7, false),
        K::Percent => (BinaryOp::Rem, 7, false),
        _ => return None,
    })
}

const COMPARE: u8 = 5;

/// Tokens that need something after them: a line ending in one reads its
/// right-hand side from the next line (`docs/grammar.md`, rule 4).
pub(crate) fn demands_operand(kind: K) -> bool {
    binary_op(kind).is_some()
        || matches!(
            kind,
            K::TwoWay
                | K::Tilde
                | K::Comma
                | K::Question
                | K::Colon
                | K::Eq
                | K::FatArrow
                | K::Bang
        )
}

/// Parses the text of a `#…` colour.
fn parse_color(hex: &str) -> Option<[u8; 4]> {
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let nib = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).ok().map(|v| v * 17);
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    match hex.len() {
        3 => Some([nib(0)?, nib(1)?, nib(2)?, 255]),
        4 => Some([nib(0)?, nib(1)?, nib(2)?, nib(3)?]),
        6 => Some([byte(0)?, byte(2)?, byte(4)?, 255]),
        8 => Some([byte(0)?, byte(2)?, byte(4)?, byte(6)?]),
        _ => None,
    }
}

/// Resolves escapes in a string token's text (quotes included).
fn unescape(raw: &str) -> String {
    let inner = raw.strip_prefix('"').unwrap_or(raw);
    let inner = inner.strip_suffix('"').unwrap_or(inner);
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some('u') => {
                let rest = chars.as_str();
                let decoded = rest
                    .strip_prefix('{')
                    .and_then(|r| r.split_once('}'))
                    .and_then(|(hex, _)| u32::from_str_radix(hex, 16).ok().map(|v| (hex.len(), v)))
                    .and_then(|(len, v)| char::from_u32(v).map(|c| (len, c)));
                match decoded {
                    Some((len, ch)) => {
                        out.push(ch);
                        chars = rest[len + 2..].chars();
                    }
                    None => out.push('u'),
                }
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

impl Parser<'_> {
    pub(crate) fn error_expr(&self) -> Expr {
        Expr {
            kind: ExprKind::Error,
            span: Span::at(self.prev_end()),
        }
    }

    /// The current token can start an expression.
    pub(crate) fn starts_expr(&self) -> bool {
        matches!(
            self.kind(),
            K::Ident
                | K::Dollar
                | K::Number
                | K::String
                | K::Hash
                | K::LParen
                | K::LBracket
                | K::Bang
                | K::Minus
        )
    }

    /// The current token can start another term of a space-separated group.
    fn starts_term(&self) -> bool {
        match self.kind() {
            K::Dollar | K::Number | K::String | K::Hash | K::LParen | K::LBracket => true,
            K::Ident => {
                let word = self.text(self.cur());
                self.nth_kind(1) != K::Colon && !TREE_KEYWORDS.contains(&word) && word != "else"
            }
            _ => false,
        }
    }

    /// A prop or token value: comma shorthand of space-separated groups.
    pub(crate) fn value(&mut self) -> Expr {
        let first = self.spaced();
        if !self.at(K::Comma) {
            return first;
        }
        let start = first.span.start;
        let mut items = vec![first];
        while self.eat(K::Comma).is_some() {
            items.push(self.spaced());
        }
        Expr {
            kind: ExprKind::Commas(items),
            span: self.finish(start),
        }
    }

    fn spaced(&mut self) -> Expr {
        let outer = self.value_level.replace(self.nl.len());
        let e = self.spaced_terms();
        self.value_level = outer;
        e
    }

    fn spaced_terms(&mut self) -> Expr {
        let first = self.expr();
        if !(self.same_line() && self.starts_term()) {
            return first;
        }
        let start = first.span.start;
        let mut terms = vec![first];
        while self.same_line() && self.starts_term() {
            let before = self.pos;
            if self.at(K::LParen) {
                self.spaced_call_hint(terms.last());
            }
            terms.push(self.expr());
            if self.pos == before {
                break;
            }
        }
        Expr {
            kind: ExprKind::Spaced(terms),
            span: self.finish(start),
        }
    }

    /// `bg: f (x)` is two values, `f` and `(x)`; warn when the first looks
    /// like something meant to be called.
    fn spaced_call_hint(&mut self, prev: Option<&Expr>) {
        let callee = match prev.map(|e| &e.kind) {
            Some(ExprKind::Name(id)) => id.name.clone(),
            Some(ExprKind::Field { name, .. }) => name.name.clone(),
            _ => return,
        };
        let paren = self.cur().span;
        self.diags.push(
            Diagnostic::warning(
                "syntax::spaced_call",
                format!("`{callee} (…)` is two values, not a call"),
            )
            .with_label(paren, "a separate value")
            .with_help(format!("to call `{callee}`, remove the space before `(`")),
        );
    }

    /// In a space-separated value, `0 -2px` (space before the sign, none
    /// after) could mean a negative term or a subtraction; it parses as a
    /// subtraction, so say so instead of silently changing the meaning.
    fn check_sign(&mut self, op: Tok) {
        if self.value_level != Some(self.nl.len()) || !op.ws_before || self.cur().ws_before {
            return;
        }
        let sign = self.text(op);
        let operand = self.text(self.cur());
        self.push_error(
            Diagnostic::error(
                "syntax::ambiguous_sign",
                format!("`{sign}{operand}` here is ambiguous"),
            )
            .with_label(
                op.span.to(self.cur().span),
                format!("this subtracts `{operand}` from the term before it"),
            )
            .with_help(if sign == "-" {
                format!(
                    "write `(-{operand})` for a separate term, \
                     or put a space after the sign (`- {operand}`) to subtract"
                )
            } else {
                // There is no unary `+`: a positive term is written bare.
                format!(
                    "write `{operand}` (no sign) for a separate term, \
                     or put a space after the sign (`+ {operand}`) to add"
                )
            }),
        );
    }

    /// A full expression, including lambdas.
    pub(crate) fn expr(&mut self) -> Expr {
        if !self.enter() {
            self.skip_group();
            return self.error_expr();
        }
        let e = if self.at_lambda() {
            self.lambda()
        } else {
            self.ternary()
        };
        self.leave();
        e
    }

    fn at_lambda(&self) -> bool {
        match self.kind() {
            K::Ident => self.nth_kind(1) == K::FatArrow,
            K::LParen => {
                // Only names, types and punctuation of a parameter list may
                // appear before `) =>`.
                for i in 1..512 {
                    match self.nth_kind(i) {
                        K::RParen => return self.nth_kind(i + 1) == K::FatArrow,
                        K::Ident
                        | K::Colon
                        | K::Comma
                        | K::LBracket
                        | K::RBracket
                        | K::Lt
                        | K::Gt
                        | K::Question
                        | K::Dot => {}
                        _ => return false,
                    }
                }
                false
            }
            _ => false,
        }
    }

    fn lambda(&mut self) -> Expr {
        let start = self.cur().span.start;
        let params = if self.at(K::Ident) {
            let name = self.ident("a parameter");
            vec![Param {
                span: name.span,
                name,
                ty: None,
                default: None,
            }]
        } else {
            self.params()
        };
        self.expect(K::FatArrow);
        let body = if self.at(K::LBrace) {
            LambdaBody::Block(self.stmt_block("the lambda body"))
        } else {
            LambdaBody::Expr(Box::new(self.expr()))
        };
        Expr {
            kind: ExprKind::Lambda { params, body },
            span: self.finish(start),
        }
    }

    fn ternary(&mut self) -> Expr {
        let cond = self.binary(0);
        if !self.at(K::Question) {
            return cond;
        }
        let start = cond.span.start;
        self.bump();
        let then = self.expr();
        let else_ = if self.expect(K::Colon).is_some() {
            self.expr()
        } else {
            self.error_expr()
        };
        Expr {
            kind: ExprKind::Ternary {
                cond: Box::new(cond),
                then: Box::new(then),
                else_: Box::new(else_),
            },
            span: self.finish(start),
        }
    }

    fn binary(&mut self, min: u8) -> Expr {
        let mut lhs = self.unary();
        // An error operand sits at the end of the previous token, which
        // may be before the current one: start the node at whichever is
        // first so children stay inside it.
        let start = lhs.span.start;
        let mut last_compare = false;
        // Each link of a left-associative chain deepens the tree, so it
        // counts toward the tree-depth limit.
        let mut links = 0;
        while let Some((op, prec, right)) = binary_op(self.kind()) {
            // A line starting with a sign touching its operand (`-1 => b`,
            // `-y.f()`) starts a new item; `- 1` continues the line.
            if self.kind() == K::Minus && self.on_new_line() && !self.nth(1).ws_before {
                break;
            }
            if prec < min || !self.enter_link() {
                break;
            }
            links += 1;
            let op_tok = self.bump();
            if matches!(op, BinaryOp::Add | BinaryOp::Sub) {
                self.check_sign(op_tok);
            }
            let chained = prec == COMPARE && last_compare;
            last_compare = prec == COMPARE;
            let rhs = if self.enter() {
                let r = self.binary(if right { prec } else { prec + 1 });
                self.leave();
                r
            } else {
                self.error_expr()
            };
            if chained {
                let help = self.chain_help(&lhs, op_tok, &rhs);
                self.push_error(
                    Diagnostic::error(
                        "syntax::chained_comparison",
                        "comparisons cannot be chained",
                    )
                    .with_label(op_tok.span, "second comparison")
                    .with_help(help),
                );
            }
            lhs = Expr {
                kind: ExprKind::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span: self.finish(start),
            };
        }
        self.leave_links(links);
        lhs
    }

    /// The fix for `a < b < c`, in the user's own operands when they are
    /// short: `a < b && b < c`.
    fn chain_help(&self, lhs: &Expr, op: Tok, rhs: &Expr) -> String {
        let generic = "combine them with `&&`: `a < b && b < c`".to_string();
        let ExprKind::Binary { rhs: middle, .. } = &lhs.kind else {
            return generic;
        };
        let (first, middle, last) = (
            lhs.span.text(self.src),
            middle.span.text(self.src),
            rhs.span.text(self.src),
        );
        if middle.is_empty() || last.is_empty() || first.len() + middle.len() + last.len() > 60 {
            return generic;
        }
        let op = self.text(op);
        format!("combine them with `&&`: `{first} && {middle} {op} {last}`")
    }

    fn unary(&mut self) -> Expr {
        let start = self.cur().span.start;
        let op = match self.kind() {
            K::Bang => Some(UnaryOp::Not),
            K::Minus => Some(UnaryOp::Neg),
            K::Ident if self.at_kw("await") && self.nth_kind(1) != K::FatArrow => {
                Some(UnaryOp::Await)
            }
            _ => None,
        };
        let Some(op) = op else {
            return self.postfix();
        };
        self.bump();
        let expr = if self.enter() {
            let e = self.unary();
            self.leave();
            e
        } else {
            self.error_expr()
        };
        Expr {
            kind: ExprKind::Unary {
                op,
                expr: Box::new(expr),
            },
            span: self.finish(start),
        }
    }

    fn postfix(&mut self) -> Expr {
        let mut e = self.primary();
        let start = e.span.start;
        let mut links = 0;
        loop {
            let link = matches!(self.kind(), K::Dot | K::QuestionDot)
                || (matches!(self.kind(), K::LParen | K::LBracket) && self.touching());
            if !link || !self.enter_link() {
                break;
            }
            links += 1;
            match self.kind() {
                K::Dot | K::QuestionDot => {
                    let optional = self.bump().kind == K::QuestionDot;
                    let name = self.field_name();
                    e = Expr {
                        kind: ExprKind::Field {
                            base: Box::new(e),
                            name,
                            optional,
                        },
                        span: self.finish(start),
                    };
                }
                K::LParen if self.touching() => {
                    let args = self.call_args();
                    e = Expr {
                        kind: ExprKind::Call {
                            callee: Box::new(e),
                            args,
                        },
                        span: self.finish(start),
                    };
                }
                K::LBracket if self.touching() => {
                    self.bump();
                    let index = self.with_nl(false, |p| {
                        let i = p.expr();
                        p.expect(K::RBracket);
                        i
                    });
                    e = Expr {
                        kind: ExprKind::Index {
                            base: Box::new(e),
                            index: Box::new(index),
                        },
                        span: self.finish(start),
                    };
                }
                _ => break,
            }
        }
        self.leave_links(links);
        e
    }

    /// A field name after `.`: a word, or digits (`x.0`). It must be on the
    /// same line: `audio.` at the end of a line is unfinished, and the next
    /// line is its own item.
    fn field_name(&mut self) -> Ident {
        if self.on_new_line() {
            self.expected("a field name");
            return Ident {
                name: String::new(),
                span: Span::at(self.prev_end()),
            };
        }
        let t = self.cur();
        if t.kind == K::Number && self.text(t).bytes().all(|b| b.is_ascii_digit()) {
            self.bump();
            return Ident {
                name: self.text(t).to_string(),
                span: t.span,
            };
        }
        self.ident("a field name")
    }

    fn primary(&mut self) -> Expr {
        if self.prev_kind().is_some_and(demands_operand) && self.dangling_at_line_end() {
            return self.error_expr();
        }
        let t = self.cur();
        let kind = match t.kind {
            K::Number => {
                self.bump();
                ExprKind::Number(self.number(t))
            }
            K::String => {
                self.bump();
                ExprKind::String(self.string_lit(t))
            }
            K::Hash => {
                self.bump();
                ExprKind::Color(self.color(t))
            }
            K::Dollar => return self.token_path(),
            K::Ident => match self.text(t) {
                "true" => {
                    self.bump();
                    ExprKind::Bool(true)
                }
                "false" => {
                    self.bump();
                    ExprKind::Bool(false)
                }
                "null" => {
                    self.bump();
                    ExprKind::Null
                }
                "match" if self.nth_kind(1) != K::FatArrow => {
                    let m = self.match_(&mut |p: &mut Self| p.expr());
                    ExprKind::Match(Box::new(m))
                }
                _ => ExprKind::Name(self.ident("a name")),
            },
            K::LParen => {
                self.bump();
                let inner = self.with_nl(false, |p| {
                    let e = p.expr();
                    p.expect(K::RParen);
                    e
                });
                ExprKind::Paren(Box::new(inner))
            }
            K::LBracket => {
                self.bump();
                let items = self.with_nl(false, |p| {
                    let mut items = Vec::new();
                    while !matches!(
                        p.kind(),
                        K::RBracket | K::Eof | K::LBrace | K::RBrace | K::Semi
                    ) {
                        let before = p.pos;
                        items.push(p.expr());
                        if p.eat(K::Comma).is_none() || p.pos == before {
                            break;
                        }
                    }
                    p.expect(K::RBracket);
                    items
                });
                ExprKind::Array(items)
            }
            _ => {
                self.expected("an expression");
                return self.error_expr();
            }
        };
        Expr {
            kind,
            span: self.finish(t.span.start),
        }
    }

    fn number(&mut self, t: Tok) -> Number {
        let (digits, suffix) = split_number(self.text(t));
        let value: f64 = digits.parse().unwrap_or(0.0);
        // Integers past 2^53 (and anything past f64) are not stored exactly.
        if !value.is_finite() || (!digits.contains('.') && value > 9_007_199_254_740_992.0) {
            self.push_error(
                Diagnostic::warning(
                    "syntax::number_precision",
                    format!("`{digits}` is too large to be stored exactly"),
                )
                .with_label(t.span, format!("this is read as {value:e}"))
                .with_help("numbers are 64-bit floats; whole numbers are exact up to 2^53"),
            );
        }
        Number {
            value,
            fraction: digits.contains('.'),
            unit: Unit::from_suffix(suffix),
        }
    }

    pub(crate) fn string_lit(&self, t: Tok) -> StringLit {
        StringLit {
            value: unescape(self.text(t)),
            span: t.span,
        }
    }

    fn color(&mut self, t: Tok) -> Color {
        let text = self.text(t);
        match parse_color(&text[1..]) {
            Some(rgba) => Color { rgba },
            None => {
                let mut d =
                    Diagnostic::error("syntax::bad_colour", format!("`{text}` is not a colour"))
                        .with_label(
                            t.span,
                            "colours are `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa`",
                        );
                if !text[1..].bytes().all(|b| b.is_ascii_hexdigit()) {
                    d = d.with_help(
                        "an SVG selector such as `#needle { … }` goes at the start of an item",
                    );
                }
                self.push_error(d);
                Color {
                    rgba: [0, 0, 0, 255],
                }
            }
        }
    }

    /// `$a.b.2`; stops before a segment that is a method call.
    fn token_path(&mut self) -> Expr {
        let t = self.bump();
        let mut segments = vec![Ident {
            name: self.text(t)[1..].to_string(),
            span: Span::new(t.span.start + 1, t.span.end),
        }];
        loop {
            if !self.at(K::Dot) {
                break;
            }
            let seg = self.nth(1);
            if seg.nl_before && self.nl_significant() {
                break; // `$a.` ending a line: postfix reports it
            }
            let seg_ok = seg.kind == K::Ident
                || (seg.kind == K::Number && self.text(seg).bytes().all(|b| b.is_ascii_digit()));
            let call = self.nth_kind(2) == K::LParen && !self.nth(2).ws_before;
            if !seg_ok || call {
                break;
            }
            self.bump();
            let s = self.bump();
            segments.push(Ident {
                name: self.text(s).to_string(),
                span: s.span,
            });
        }
        let span = self.finish(t.span.start);
        self.check_kebab_token(span);
        Expr {
            kind: ExprKind::Token(TokenKey {
                dollar: true,
                segments,
                span,
            }),
            span,
        }
    }

    /// `$fg-muted` (touching `-` and word) is `$fg - muted`; token names
    /// are snake_case, so it was almost certainly meant as one name.
    fn check_kebab_token(&mut self, path: Span) {
        let (dash, word) = (self.cur(), self.nth(1));
        if dash.kind != K::Minus || dash.ws_before || word.kind != K::Ident || word.ws_before {
            return;
        }
        let written = format!("{}-{}", self.text_span(path), self.text(word));
        self.diags.push(
            Diagnostic::warning(
                "syntax::kebab_case",
                format!("`{written}` subtracts `{}` from a token", self.text(word)),
            )
            .with_label(path.to(word.span), "read as a subtraction")
            .with_help(format!(
                "names are snake_case: `{}`; or put spaces around `-` to subtract",
                written.replace('-', "_")
            )),
        );
    }

    fn text_span(&self, span: Span) -> &str {
        span.text(self.src)
    }

    /// `(args)` of a call or attribute; the current token is `(`.
    pub(crate) fn call_args(&mut self) -> Vec<Arg> {
        self.bump();
        self.with_nl(false, |p| {
            let mut args = Vec::new();
            while !matches!(
                p.kind(),
                K::RParen | K::Eof | K::LBrace | K::RBrace | K::Semi
            ) {
                let before = p.pos;
                args.push(p.arg());
                if p.eat(K::Comma).is_none() || p.pos == before {
                    break;
                }
            }
            p.expect(K::RParen);
            args
        })
    }

    fn arg(&mut self) -> Arg {
        let start = self.cur().span.start;
        let kind = if self.at(K::Ident) && self.nth_kind(1) == K::Colon {
            let name = self.ident("an argument name");
            self.bump();
            ArgKind::Named(name)
        } else if self.at_kw("from") && self.relative_source_follows() {
            ArgKind::From(self.bump().span)
        } else {
            ArgKind::Positional
        };
        let value = self.expr();
        let start = start.min(value.span.start);
        Arg {
            kind,
            value,
            span: self.finish(start),
        }
    }

    /// After `from` in an argument: does an expression follow (relative
    /// colour source), rather than `from` itself being the value?
    fn relative_source_follows(&self) -> bool {
        let next = self.nth(1);
        match next.kind {
            K::Ident | K::Dollar | K::Number | K::String | K::Hash | K::LBracket | K::Bang => true,
            K::LParen => next.ws_before,
            _ => false,
        }
    }

    /// `(name: Type = default, …)`; the current token is `(`.
    pub(crate) fn params(&mut self) -> Vec<Param> {
        self.bump();
        self.with_nl(false, |p| {
            let mut params = Vec::new();
            while p.at(K::Ident) {
                let start = p.cur().span.start;
                let name = p.ident("a parameter name");
                let ty = p.eat(K::Colon).map(|_| p.ty());
                let default = p.eat(K::Eq).map(|_| p.expr());
                params.push(Param {
                    name,
                    ty,
                    default,
                    span: p.finish(start),
                });
                if p.eat(K::Comma).is_none() {
                    break;
                }
            }
            p.expect(K::RParen);
            params
        })
    }

    /// `Name`, `[T]`, `Async<T>`, `T?`.
    pub(crate) fn ty(&mut self) -> Type {
        let start = self.cur().span.start;
        if !self.enter() {
            self.skip_group();
            return Type {
                kind: TypeKind::Error,
                span: Span::at(self.prev_end()),
            };
        }
        let mut ty = match self.kind() {
            K::LBracket => {
                self.bump();
                let inner = self.with_nl(false, |p| {
                    let t = p.ty();
                    p.expect(K::RBracket);
                    t
                });
                Type {
                    kind: TypeKind::List(Box::new(inner)),
                    span: self.finish(start),
                }
            }
            K::Ident => {
                let mut path = vec![self.ident("a type")];
                while self.at(K::Dot) && self.nth_kind(1) == K::Ident {
                    self.bump();
                    path.push(self.ident("a type"));
                }
                let mut args = Vec::new();
                if self.at(K::Lt) {
                    self.bump();
                    self.with_nl(false, |p| {
                        loop {
                            args.push(p.ty());
                            if p.eat(K::Comma).is_none() {
                                break;
                            }
                        }
                        p.expect(K::Gt);
                    });
                }
                Type {
                    kind: TypeKind::Named { path, args },
                    span: self.finish(start),
                }
            }
            _ => {
                self.expected("a type");
                Type {
                    kind: TypeKind::Error,
                    span: Span::at(self.prev_end()),
                }
            }
        };
        while self.at(K::Question) && self.touching() {
            self.bump();
            ty = Type {
                kind: TypeKind::Optional(Box::new(ty)),
                span: self.finish(start),
            };
        }
        self.leave();
        ty
    }

    fn pattern(&mut self) -> Pattern {
        let start = self.cur().span.start;
        let t = self.cur();
        let kind = match t.kind {
            K::Ident if self.text(t) == "_" => {
                self.bump();
                PatternKind::Wildcard
            }
            K::Ident if !matches!(self.text(t), "true" | "false" | "null") => {
                let mut path = vec![self.ident("a pattern")];
                while self.at(K::Dot) {
                    self.bump();
                    path.push(self.ident("a variant name"));
                }
                PatternKind::Path(path)
            }
            K::Ident | K::Number | K::String | K::Hash | K::Minus => {
                PatternKind::Literal(self.unary())
            }
            _ => {
                self.expected_at_token("a pattern");
                PatternKind::Error
            }
        };
        Pattern {
            kind,
            span: self.finish(start),
        }
    }

    /// `match x { pat => body, … }` with arm bodies parsed by `arm_body`.
    pub(crate) fn match_<B>(&mut self, arm_body: &mut impl FnMut(&mut Self) -> B) -> Match<B> {
        self.bump(); // match
        let scrutinee = self.expr();
        let mut arms = Vec::new();
        let Some(open) = self.eat(K::LBrace) else {
            self.expected("`{` and the match arms");
            return Match {
                scrutinee,
                arms,
                arms_span: Span::at(self.prev_end()),
            };
        };
        if !self.enter() {
            return Match {
                scrutinee,
                arms,
                arms_span: self.finish(open.span.start),
            };
        }
        self.with_nl(true, |p| {
            loop {
                while p.eat(K::Comma).is_some() || p.eat(K::Semi).is_some() {}
                if p.block_ends(open, true) {
                    break;
                }
                let before = p.pos;
                let start = p.cur().span.start;
                let pattern = p.pattern();
                if p.pos == before {
                    // Not an arm at all (and reported): skip it.
                    p.skip_stray();
                    continue;
                }
                p.expect(K::FatArrow);
                let body = arm_body(p);
                let start = start.min(pattern.span.start);
                arms.push(Arm {
                    pattern,
                    body,
                    span: p.finish(start),
                });
                if p.pos == before {
                    p.skip_stray();
                    continue;
                }
                if !(p.at(K::Comma) || p.at_item_end()) {
                    p.expected("`,` or a line break between arms");
                    p.recover_line();
                }
            }
        });
        self.leave();
        Match {
            scrutinee,
            arms,
            arms_span: self.finish(open.span.start),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours() {
        assert_eq!(parse_color("7aa2f7"), Some([0x7a, 0xa2, 0xf7, 255]));
        assert_eq!(parse_color("fff"), Some([255, 255, 255, 255]));
        assert_eq!(parse_color("00000080"), Some([0, 0, 0, 0x80]));
        assert_eq!(parse_color("needle"), None);
        assert_eq!(parse_color("12345"), None);
    }

    #[test]
    fn escapes() {
        assert_eq!(unescape(r#""a\"b\n\u{2039}""#), "a\"b\n‹");
        assert_eq!(unescape(r#""bad \u{zz}""#), "bad u{zz}");
    }
}
