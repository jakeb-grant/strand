//! The formatter behind `strand fmt` and LSP formatting.
//!
//! It is a layout normaliser over the lossless token stream, guided by the
//! syntax tree, in the style of `docs/design.md`'s examples:
//!
//! - **Line breaks are the author's.** Items stay on the lines they were
//!   written on (`edge: top; height: 36` stays one line, a one-line `when`
//!   stays one line); lines are never joined or split, so nothing is
//!   wrapped. At most one blank line is kept between lines, none after a
//!   `{` that ends a line or before a line starting with `}`, and none at
//!   the start or end of the file. Line endings become `\n` and the file
//!   ends with one.
//! - **Indentation is two spaces per block,** counted from the line that
//!   opened the block. A block whose `{` is followed by more on its line
//!   (`col { width: 600; …`) may hang: a line of it written at the column
//!   of that first item stays there (design.md's launcher and toasts align
//!   props under `width:`). A line continuing an expression keeps its
//!   offset from the line its item started on (inside `( … )` and
//!   `[ … ]`, from the line that opened the bracket), at least two spaces;
//!   a line starting with the closing `)` or `]` sits at the opening
//!   line's indentation, and `else`, or a block's `{` on a line of its
//!   own, at the line it belongs to.
//! - **Spacing inside a line is canonical:** one space around binary
//!   operators, `=>`, `->`, `<->`, `~`, `=`, after `,`, `;` and `:`, and
//!   inside `{ … }`; none inside `( … )` and `[ … ]`, around `.` and `?.`,
//!   after a prefix `-` or `!`, before `:` (except a ternary's) and inside
//!   types (`Async<[Hit]>`). Whether a `(` or `[` touches the word before
//!   it is never changed, since that decides between a call and a new
//!   term (`docs/grammar.md`, "Calls touch").
//! - **Alignment the author made is kept.** Where a `{`, `=>`, `=`, a
//!   prop's value or a trailing `//` comment was set apart by two or more
//!   spaces, it keeps its column if the line still fits before it (design
//!   aligns `when` blocks, match arms, token values and comments that
//!   way); trailing comments keep their absolute column, the others their
//!   column relative to the line's indentation.
//! - **Separators.** A `;` that ends a line or comes before `}` or another
//!   `;` is dropped (a line break or the brace already separates), unless
//!   the next line starts with a token that would otherwise continue the
//!   line.
//!
//! Comments are kept where they are. A file with syntax errors is not
//! formatted. Every result is re-parsed: if the syntax tree differs from
//! the input's in anything but spans, the formatter reports
//! [`FormatError::Unstable`] instead of changing the file, so formatting
//! can never change what a file means. The formatter is idempotent
//! (`tests/fmt.rs`).

use std::collections::HashSet;
use std::fmt;

use crate::diagnostic::Diagnostic;
use crate::source::FileId;
use crate::syntax::ast::*;
use crate::syntax::lexer::{Token, TokenKind as K, lex};
use crate::syntax::{Parse, Span, parse};

/// Why a file was left as written.
#[derive(Clone, Debug, PartialEq)]
pub enum FormatError {
    /// The file has syntax errors (the parser's error diagnostics).
    Syntax(Vec<Diagnostic>),
    /// The formatted text would not parse to the same tree. This is a
    /// formatter bug; the message says what differed.
    Unstable(String),
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatError::Syntax(d) => {
                let n = d.len();
                write!(
                    f,
                    "the file has {n} syntax error{}; fix {} first",
                    if n == 1 { "" } else { "s" },
                    if n == 1 { "it" } else { "them" }
                )
            }
            FormatError::Unstable(why) => write!(f, "formatting would change the file: {why}"),
        }
    }
}

impl std::error::Error for FormatError {}

/// Formats one `.strand` file.
pub fn format(src: &str) -> Result<String, FormatError> {
    let parsed = parse(FileId::default(), src);
    format_parsed(src, &parsed)
}

/// [`format`] for a file the caller has already parsed.
pub fn format_parsed(src: &str, parsed: &Parse) -> Result<String, FormatError> {
    if parsed.has_errors() {
        return Err(FormatError::Syntax(
            parsed
                .diagnostics
                .iter()
                .filter(|d| d.is_error())
                .cloned()
                .collect(),
        ));
    }
    let out = Layout::new(src, parsed).run();
    let again = parse(parsed.file_id, &out);
    if again.has_errors() {
        return Err(FormatError::Unstable(format!(
            "the result has a syntax error: {}",
            again
                .diagnostics
                .iter()
                .find(|d| d.is_error())
                .map_or("", |d| d.message.as_str())
        )));
    }
    if shape(&again.file) != shape(&parsed.file) {
        return Err(FormatError::Unstable("the syntax tree changed".into()));
    }
    Ok(out)
}

/// The syntax tree with every span removed: two files with equal shapes
/// mean the same thing, however they are laid out.
pub fn shape(file: &File) -> String {
    let full = format!("{file:?}");
    let mut out = String::with_capacity(full.len() / 2);
    let mut rest = full.as_str();
    const OPEN: &str = "Span { start: ";
    while let Some(i) = rest.find(OPEN) {
        out.push_str(&rest[..i]);
        out.push_str("Span");
        let after = &rest[i + OPEN.len()..];
        match after.find(" }") {
            Some(j) => rest = &after[j + 2..],
            None => {
                rest = after;
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------
// What the tree says about tokens

/// Facts about token positions that only the tree knows.
#[derive(Default)]
struct Facts {
    /// Offsets where an item, statement, field, token entry, match arm or
    /// enum variant starts: a line starting there is not a continuation.
    item_starts: HashSet<u32>,
    /// Where attributes end: the declaration after each one starts an
    /// item too when it opens a line (`@reset` / `state x = 1`).
    attr_ends: Vec<u32>,
    /// Offsets of prefix `-` and `!`.
    unary: HashSet<u32>,
    /// The gaps holding a ternary's `?` and `:`.
    ternary_gaps: Vec<Span>,
    /// Spans of types (`Async<[Hit]>`, `T?`).
    types: Vec<Span>,
}

impl Facts {
    fn of(file: &File) -> Facts {
        let mut f = Facts::default();
        f.items(&file.items);
        f
    }

    fn items(&mut self, items: &[Item]) {
        for it in items {
            self.item(it);
        }
    }

    fn item(&mut self, it: &Item) {
        self.item_starts.insert(it.span.start);
        for a in &it.attrs {
            self.item_starts.insert(a.span.start);
            self.attr_ends.push(a.span.end);
            self.args(&a.args);
        }
        match &it.kind {
            ItemKind::Component(c) => {
                if let Some(ps) = &c.params {
                    self.params(ps);
                }
                if let Some(t) = &c.tokens {
                    self.entries(&t.items);
                }
                self.items(&c.body.items);
            }
            ItemKind::Surface(s) => self.items(&s.body.items),
            ItemKind::State(s) => match &s.init {
                StateInit::Value { ty, key, value, .. } => {
                    if let Some(t) = ty {
                        self.ty(t);
                    }
                    if let Some(k) = key {
                        self.expr(k);
                    }
                    self.expr(value);
                }
                StateInit::File { fields, .. } => self.fields(&fields.items),
            },
            ItemKind::Let(l) => self.let_(l),
            ItemKind::Enum(e) => {
                for v in &e.variants {
                    self.item_starts.insert(v.span.start);
                }
            }
            ItemKind::Type(t) => self.fields(&t.fields.items),
            ItemKind::Fn(f) => {
                self.params(&f.params);
                if let Some(t) = &f.ret {
                    self.ty(t);
                }
                self.stmts(&f.body.items);
            }
            ItemKind::Tokens(t) => self.entries(&t.entries.items),
            ItemKind::Use(u) => {
                for c in &u.clauses {
                    self.expr(&c.value);
                }
            }
            ItemKind::Service(s) => {
                self.exprs(&s.source.args);
                if let Some(e) = &s.source.every {
                    self.expr(e);
                }
                self.items(&s.body.items);
            }
            ItemKind::Permit(p) => self.exprs(&p.args),
            ItemKind::Keyframes(k) => self.items(&k.body.items),
            ItemKind::Field(f) => self.field(f),
            ItemKind::KeyframeStop(k) => {
                self.exprs(&k.stops);
                self.items(&k.body.items);
            }
            ItemKind::Prop(p) => {
                self.expr(&p.value);
                if let Some(t) = &p.transition {
                    self.expr(t);
                }
                if let Some(b) = &p.block {
                    self.items(&b.items);
                }
            }
            ItemKind::Element(e) => {
                match &e.arg {
                    Some(HeadArg::Positional(x)) | Some(HeadArg::Named(_, x)) => self.expr(x),
                    None => {}
                }
                if let Some(b) = &e.block {
                    self.items(&b.items);
                }
            }
            ItemKind::When(w) => {
                self.expr(&w.cond);
                self.items(&w.body.items);
            }
            ItemKind::If(i) => self.if_item(i),
            ItemKind::For(f) => {
                self.expr(&f.iter);
                if let Some(k) = &f.key {
                    self.expr(k);
                }
                self.items(&f.body.items);
            }
            ItemKind::Match(m) => {
                self.expr(&m.scrutinee);
                for arm in &m.arms {
                    self.item_starts.insert(arm.span.start);
                    self.pattern(&arm.pattern);
                    match &arm.body {
                        ArmBody::Block(b) => self.items(&b.items),
                        ArmBody::Single(i) => self.item(i),
                    }
                }
            }
            ItemKind::On(o) => {
                match &o.event {
                    Event::Named { params, .. } => {
                        if let Some(ps) = params {
                            self.params(ps);
                        }
                    }
                    Event::Change { targets, debounce } => {
                        self.exprs(targets);
                        if let Some(d) = debounce {
                            self.expr(d);
                        }
                    }
                }
                self.stmts(&o.body.items);
            }
            ItemKind::Timer(t) => {
                self.expr(&t.duration);
                if let Some(w) = &t.while_ {
                    self.expr(w);
                }
                self.stmts(&t.body.items);
            }
            ItemKind::Pose(p) => self.items(&p.body.items),
            ItemKind::Set(b) => self.entries(&b.items),
            ItemKind::Selector(s) => self.items(&s.body.items),
            ItemKind::Play(e) => self.expr(e),
            ItemKind::Slot | ItemKind::Error => {}
        }
    }

    fn if_item(&mut self, i: &If<Item>) {
        self.expr(&i.cond);
        self.items(&i.then.items);
        match &i.else_ {
            Some(Else::If(next, _)) => self.if_item(next),
            Some(Else::Block(b)) => self.items(&b.items),
            None => {}
        }
    }

    fn let_(&mut self, l: &Let) {
        if let Some(t) = &l.ty {
            self.ty(t);
        }
        self.expr(&l.value);
    }

    fn fields(&mut self, fields: &[Field]) {
        for f in fields {
            self.field(f);
        }
    }

    fn field(&mut self, f: &Field) {
        self.item_starts.insert(f.span.start);
        self.ty(&f.ty);
        if let Some(d) = &f.default {
            self.expr(d);
        }
    }

    fn entries(&mut self, entries: &[TokenEntry]) {
        for e in entries {
            self.item_starts.insert(e.span.start);
            match &e.body {
                TokenBody::Value(v) => self.expr(v),
                TokenBody::Group(g) => self.entries(&g.items),
            }
        }
    }

    fn params(&mut self, ps: &[Param]) {
        for p in ps {
            if let Some(t) = &p.ty {
                self.ty(t);
            }
            if let Some(d) = &p.default {
                self.expr(d);
            }
        }
    }

    fn ty(&mut self, t: &Type) {
        self.types.push(t.span);
    }

    fn pattern(&mut self, p: &Pattern) {
        if let PatternKind::Literal(e) = &p.kind {
            self.expr(e);
        }
    }

    fn stmts(&mut self, stmts: &[Stmt]) {
        for s in stmts {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        self.item_starts.insert(s.span.start);
        match &s.kind {
            StmtKind::Let(l) => self.let_(l),
            StmtKind::Assign { target, value, .. } => {
                self.expr(target);
                self.expr(value);
            }
            StmtKind::Expr(e) | StmtKind::Play(e) => self.expr(e),
            StmtKind::If(i) => self.if_stmt(i),
            StmtKind::For(f) => {
                self.expr(&f.iter);
                if let Some(k) = &f.key {
                    self.expr(k);
                }
                self.stmts(&f.body.items);
            }
            StmtKind::Match(m) => {
                self.expr(&m.scrutinee);
                for arm in &m.arms {
                    self.item_starts.insert(arm.span.start);
                    self.pattern(&arm.pattern);
                    match &arm.body {
                        ArmBody::Block(b) => self.stmts(&b.items),
                        ArmBody::Single(st) => self.stmt(st),
                    }
                }
            }
            StmtKind::Error => {}
        }
    }

    fn if_stmt(&mut self, i: &If<Stmt>) {
        self.expr(&i.cond);
        self.stmts(&i.then.items);
        match &i.else_ {
            Some(Else::If(next, _)) => self.if_stmt(next),
            Some(Else::Block(b)) => self.stmts(&b.items),
            None => {}
        }
    }

    fn args(&mut self, args: &[Arg]) {
        for a in args {
            self.expr(&a.value);
        }
    }

    fn exprs(&mut self, es: &[Expr]) {
        for e in es {
            self.expr(e);
        }
    }

    fn expr(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Number(_)
            | ExprKind::String(_)
            | ExprKind::Color(_)
            | ExprKind::Bool(_)
            | ExprKind::Null
            | ExprKind::Name(_)
            | ExprKind::Token(_)
            | ExprKind::Error => {}
            ExprKind::Array(xs) | ExprKind::Commas(xs) | ExprKind::Spaced(xs) => self.exprs(xs),
            ExprKind::Paren(x) => self.expr(x),
            ExprKind::Unary { op, expr } => {
                if matches!(op, UnaryOp::Neg | UnaryOp::Not) {
                    self.unary.insert(e.span.start);
                }
                self.expr(expr);
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            ExprKind::Ternary { cond, then, else_ } => {
                self.ternary_gaps
                    .push(Span::new(cond.span.end, then.span.start));
                self.ternary_gaps
                    .push(Span::new(then.span.end, else_.span.start));
                self.expr(cond);
                self.expr(then);
                self.expr(else_);
            }
            ExprKind::Field { base, .. } => self.expr(base),
            ExprKind::Call { callee, args } => {
                self.expr(callee);
                self.args(args);
            }
            ExprKind::Index { base, index } => {
                self.expr(base);
                self.expr(index);
            }
            ExprKind::Lambda { params, body } => {
                self.params(params);
                match body {
                    LambdaBody::Expr(x) => self.expr(x),
                    LambdaBody::Block(b) => self.stmts(&b.items),
                }
            }
            ExprKind::Match(m) => {
                self.expr(&m.scrutinee);
                for arm in &m.arms {
                    self.item_starts.insert(arm.span.start);
                    self.pattern(&arm.pattern);
                    self.expr(&arm.body);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Layout

/// An open bracket while laying out.
struct Frame {
    kind: K,
    /// Output indentation of the line that opened it.
    opener_out: usize,
    /// Source indentation of that line.
    opener_src: usize,
    /// Braces: where a hanging item sits (source column, output column).
    hang: Option<(usize, usize)>,
    /// Braces: the last line that started an item here (source and output
    /// indentation); continuation lines keep their offset from it.
    anchor: (usize, usize),
}

/// One source line: its significant tokens (comments included), by index.
struct Line {
    toks: Vec<usize>,
    /// Source column of the first token.
    indent: usize,
}

struct Layout<'a> {
    src: &'a str,
    toks: &'a [Token],
    facts: Facts,
    /// Token index → a ternary's `?` or `:`.
    ternary: HashSet<usize>,
    /// Token index → inside a type, with the type's index.
    in_type: Vec<Option<usize>>,
    /// Source column of each token.
    col: Vec<usize>,
    out: String,
    frames: Vec<Frame>,
    /// The anchor of top-level lines (see [`Frame::anchor`]).
    root_anchor: (usize, usize),
}

/// Rule 3 of "Lines and termination": a line starting with one of these
/// continues the line before.
fn continues(kind: K, text: &str) -> bool {
    matches!(
        kind,
        K::QuestionQuestion
            | K::OrOr
            | K::AndAnd
            | K::EqEq
            | K::BangEq
            | K::Lt
            | K::LtEq
            | K::Gt
            | K::GtEq
            | K::Plus
            | K::Minus
            | K::Star
            | K::Slash
            | K::Percent
            | K::Dot
            | K::QuestionDot
            | K::Question
            | K::Colon
            | K::Tilde
            | K::FatArrow
            | K::TwoWay
    ) || kind == K::Ident && text == "else"
}

/// Tokens that end an operand: a touching `(` or `[` after one is a call
/// or an index, a spaced one a new term.
fn ends_operand(kind: K) -> bool {
    matches!(
        kind,
        K::Ident
            | K::Number
            | K::String
            | K::Dollar
            | K::Hash
            | K::At
            | K::RParen
            | K::RBracket
            | K::RBrace
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Gap {
    None,
    Space,
}

impl<'a> Layout<'a> {
    fn new(src: &'a str, parsed: &'a Parse) -> Self {
        let toks = parsed.tokens.as_slice();
        let mut facts = Facts::of(&parsed.file);
        let index_at = |offset: u32| toks.partition_point(|t| t.span.start < offset);
        for &end in &facts.attr_ends {
            if let Some(t) = toks[index_at(end)..].iter().find(|t| !t.kind.is_trivia()) {
                facts.item_starts.insert(t.span.start);
            }
        }
        let mut ternary = HashSet::new();
        for gap in &facts.ternary_gaps {
            let mut i = index_at(gap.start);
            while i < toks.len() && toks[i].span.start < gap.end {
                if matches!(toks[i].kind, K::Question | K::Colon) {
                    ternary.insert(i);
                    break;
                }
                i += 1;
            }
        }
        let mut in_type = vec![None; toks.len()];
        for (n, t) in facts.types.iter().enumerate() {
            let mut i = index_at(t.start);
            while i < toks.len() && toks[i].span.end <= t.end {
                in_type[i] = Some(n);
                i += 1;
            }
        }
        let mut col = vec![0; toks.len()];
        let mut c = 0;
        for (i, t) in toks.iter().enumerate() {
            col[i] = c;
            if t.kind == K::Newline {
                c = 0;
            } else {
                c += t.span.text(src).chars().count();
            }
        }
        Layout {
            src,
            toks,
            facts,
            ternary,
            in_type,
            col,
            out: String::with_capacity(src.len() + src.len() / 8),
            frames: Vec::new(),
            root_anchor: (0, 0),
        }
    }

    fn text(&self, i: usize) -> &'a str {
        self.toks[i].span.text(self.src)
    }

    fn kind(&self, i: usize) -> K {
        self.toks[i].kind
    }

    fn lines(&self) -> Vec<Line> {
        let mut lines = Vec::new();
        let mut cur = Vec::new();
        for (i, t) in self.toks.iter().enumerate() {
            match t.kind {
                K::Newline => {
                    lines.push(self.line(std::mem::take(&mut cur)));
                }
                K::Whitespace => {}
                _ => cur.push(i),
            }
        }
        lines.push(self.line(cur));
        lines
    }

    fn line(&self, toks: Vec<usize>) -> Line {
        let indent = toks.first().map_or(0, |&i| self.col[i]);
        Line { toks, indent }
    }

    /// The next significant (non-trivia) token after `i`, across lines.
    fn next_significant(&self, i: usize) -> Option<usize> {
        (i + 1..self.toks.len()).find(|&j| !self.kind(j).is_trivia())
    }

    /// Drops separators that a line break or brace already gives.
    fn kept(&self, line: &Line) -> Vec<usize> {
        let code: Vec<usize> = line
            .toks
            .iter()
            .copied()
            .filter(|&i| self.kind(i) != K::Comment)
            .collect();
        let mut kept: Vec<usize> = Vec::with_capacity(line.toks.len());
        for (n, &i) in code.iter().enumerate() {
            if self.kind(i) == K::Semi {
                let next = code.get(n + 1).copied();
                let drop = match kept.last().map(|&p| self.kind(p)) {
                    Some(K::LBrace | K::Semi) => true,
                    // A line starting with `;`: dropping it must not make
                    // the line continue the one before.
                    None => next.is_none_or(|j| !continues(self.kind(j), self.text(j))),
                    Some(_) => false,
                } || match next.map(|j| self.kind(j)) {
                    Some(K::RBrace | K::Semi) => true,
                    Some(_) => false,
                    // At the end of the line: only if the next line does
                    // not continue this one without it.
                    None => self
                        .next_significant(i)
                        .is_none_or(|j| !continues(self.kind(j), self.text(j))),
                };
                if drop {
                    continue;
                }
            }
            kept.push(i);
        }
        if let Some(&c) = line.toks.last().filter(|&&i| self.kind(i) == K::Comment) {
            kept.push(c);
        }
        kept
    }

    fn run(mut self) -> String {
        let lines = self.lines();
        let mut blank = 0usize;
        // The last code token written, to decide on blank lines.
        let mut last_written: Option<usize> = None;
        for line in &lines {
            let kept = self.kept(line);
            let Some(&first) = kept.first() else {
                if line.toks.is_empty() {
                    blank += 1;
                }
                continue;
            };
            let after_open = last_written.is_some_and(|i| self.kind(i) == K::LBrace);
            if blank > 0 && last_written.is_some() && !after_open && self.kind(first) != K::RBrace {
                self.out.push('\n');
            }
            blank = 0;
            let indent = self.indent_of(line, first);
            self.write_line(line, &kept, indent);
            if let Some(&l) = kept.iter().rev().find(|&&i| self.kind(i) != K::Comment) {
                last_written = Some(l);
            } else if last_written.is_none() {
                // A comment-only first line still counts as written.
                last_written = Some(first);
            }
        }
        self.out
    }

    /// The innermost brace frame, if any.
    fn brace(&self) -> Option<&Frame> {
        self.frames.iter().rev().find(|f| f.kind == K::LBrace)
    }

    fn brace_mut(&mut self) -> Option<&mut Frame> {
        self.frames.iter_mut().rev().find(|f| f.kind == K::LBrace)
    }

    /// Output indentation for a line whose first written token is `first`.
    fn indent_of(&mut self, line: &Line, first: usize) -> usize {
        let kind = self.kind(first);
        let src = line.indent;
        if kind == K::RBrace {
            return self.brace().map_or(0, |f| f.opener_out);
        }
        if let Some(top) = self.frames.last()
            && top.kind != K::LBrace
        {
            // Inside `( … )` or `[ … ]`: relative to the line that opened
            // it; the closing bracket at that line's indentation.
            if matches!(kind, K::RParen | K::RBracket) {
                return top.opener_out;
            }
            return top.opener_out + src.saturating_sub(top.opener_src).max(2);
        }
        let start = self.toks[first].span.start;
        let is_item = kind == K::Comment || self.facts.item_starts.contains(&start);
        if is_item {
            let (child, hang) = self
                .brace()
                .map_or((0, None), |f| (f.opener_out + 2, f.hang));
            let out = match hang {
                Some((hs, ho)) if hs == src => ho,
                _ => child,
            };
            if kind != K::Comment {
                match self.brace_mut() {
                    Some(f) => f.anchor = (src, out),
                    None => self.root_anchor = (src, out),
                }
            }
            return out;
        }
        let anchor = self.brace().map_or(self.root_anchor, |f| f.anchor);
        // `else`, and a mandatory block's `{` on its own line (grammar
        // rule 6), sit at the line they belong to.
        if kind == K::LBrace || kind == K::Ident && self.text(first) == "else" {
            return anchor.1;
        }
        anchor.1 + src.saturating_sub(anchor.0).max(2)
    }
}

impl Layout<'_> {
    fn write_line(&mut self, line: &Line, kept: &[usize], indent: usize) {
        self.out.extend(std::iter::repeat_n(' ', indent));
        let mut col = indent;
        let mut prev: Option<usize> = None;
        let mut hang_next = false;
        for &i in kept {
            let kind = self.kind(i);
            if let Some(p) = prev {
                let mut spaces = match self.gap(p, i) {
                    Gap::None => 0,
                    Gap::Space => 1,
                };
                if spaces == 1 && self.ws_before(i) >= 2 && self.aligns(p, i) {
                    let want = if kind == K::Comment {
                        self.col[i]
                    } else {
                        indent + self.col[i].saturating_sub(line.indent)
                    };
                    spaces = want.saturating_sub(col).max(1);
                }
                self.out.extend(std::iter::repeat_n(' ', spaces));
                col += spaces;
            }
            if hang_next {
                hang_next = false;
                if !matches!(kind, K::Comment | K::RBrace)
                    && let Some(f) = self.frames.last_mut()
                {
                    f.hang = Some((self.col[i], col));
                    f.anchor = (self.col[i], col);
                }
            }
            let text = if kind == K::Comment {
                self.text(i).trim_end()
            } else {
                self.text(i)
            };
            self.out.push_str(text);
            col += text.chars().count();
            match kind {
                K::LBrace | K::LParen | K::LBracket => {
                    self.frames.push(Frame {
                        kind,
                        opener_out: indent,
                        opener_src: line.indent,
                        hang: None,
                        anchor: (line.indent, indent),
                    });
                    hang_next = kind == K::LBrace;
                }
                K::RBrace => {
                    while let Some(f) = self.frames.pop() {
                        if f.kind == K::LBrace {
                            break;
                        }
                    }
                }
                K::RParen | K::RBracket => {
                    let open = if kind == K::RParen {
                        K::LParen
                    } else {
                        K::LBracket
                    };
                    if self.frames.last().is_some_and(|f| f.kind == open) {
                        self.frames.pop();
                    }
                }
                _ => {}
            }
            prev = Some(i);
        }
        self.out.push('\n');
    }

    /// Whitespace characters directly before token `i`.
    fn ws_before(&self, i: usize) -> usize {
        match i.checked_sub(1) {
            Some(p) if self.kind(p) == K::Whitespace => self.text(p).chars().count(),
            _ => 0,
        }
    }

    /// `b` may be aligned to a column after `a`.
    fn aligns(&self, a: usize, b: usize) -> bool {
        matches!(
            self.kind(b),
            K::LBrace
                | K::FatArrow
                | K::Eq
                | K::PlusEq
                | K::MinusEq
                | K::StarEq
                | K::SlashEq
                | K::Comment
        ) || self.kind(a) == K::Colon && !self.ternary.contains(&a)
    }

    /// The canonical gap between two tokens written on one line.
    fn gap(&self, p: usize, i: usize) -> Gap {
        let (a, b) = (self.kind(p), self.kind(i));
        if b == K::Comment {
            return Gap::Space;
        }
        let touching = self.toks[p].span.end == self.toks[i].span.start;
        if let (Some(x), Some(y)) = (self.in_type[p], self.in_type[i])
            && x == y
        {
            return if a == K::Comma { Gap::Space } else { Gap::None };
        }
        let g = match (a, b) {
            (_, K::Comma | K::Semi | K::RParen | K::RBracket) => Gap::None,
            (K::LParen | K::LBracket, _) => Gap::None,
            (K::Dot | K::QuestionDot, _) | (_, K::Dot | K::QuestionDot) => {
                // `1 . 5` must not become the number `1.5`.
                if !touching && (a == K::Number || b == K::Number) {
                    Gap::Space
                } else {
                    Gap::None
                }
            }
            // `- -b` stays apart: `--b` reads like a decrement.
            (K::Minus, _)
                if self.facts.unary.contains(&self.toks[p].span.start)
                    && self.text(i).starts_with('-') =>
            {
                Gap::Space
            }
            (K::Minus | K::Bang, _) if self.facts.unary.contains(&self.toks[p].span.start) => {
                Gap::None
            }
            (_, K::LParen | K::LBracket) => {
                if touching && ends_operand(a) {
                    Gap::None
                } else {
                    Gap::Space
                }
            }
            (_, K::Question | K::Colon) if self.ternary.contains(&i) => Gap::Space,
            (_, K::Colon) => Gap::None,
            (_, K::Question) => {
                if touching {
                    Gap::None
                } else {
                    Gap::Space
                }
            }
            (K::LBrace, K::RBrace) => Gap::None,
            _ => Gap::Space,
        };
        if g == Gap::None && !touching && !self.glues(p, i) {
            Gap::Space
        } else {
            g
        }
    }

    /// Writing `p` and `i` together lexes back to the same two tokens.
    fn glues(&self, p: usize, i: usize) -> bool {
        let (a, b) = (self.text(p), self.text(i));
        let joined = format!("{a}{b}");
        let (toks, diags) = lex(&joined);
        diags.is_empty()
            && toks.len() == 2
            && toks[0].kind == self.kind(p)
            && toks[1].kind == self.kind(i)
            && toks[0].span.len() as usize == a.len()
    }
}
