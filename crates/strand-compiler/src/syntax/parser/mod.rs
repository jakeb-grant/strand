//! Hand-written recursive-descent parser with error recovery.
//!
//! The parser never panics and always produces a [`File`]. Newline
//! handling follows `docs/grammar.md`, "Lines and termination": a stack
//! records whether line breaks are significant in the innermost bracket.

mod expr;
mod items;

use std::cell::Cell;

use crate::diagnostic::{Diagnostic, closest};
use crate::source::FileId;

use super::ast::*;
use super::lexer::{Token, TokenKind, lex};
use super::span::Span;

use TokenKind as K;

/// The result of parsing one file: a tree (always), the lossless token
/// stream it was parsed from, and diagnostics.
#[derive(Clone, Debug)]
pub struct Parse {
    /// Which file this is; every diagnostic label is in this file.
    pub file_id: FileId,
    pub file: File,
    /// Every token, trivia included: concatenated, they are the source.
    /// Keyword spans that the tree does not store (`when`, `else`, `key`,
    /// `after`, …) are found here by offset, for semantic highlighting,
    /// formatting and hover.
    pub tokens: Vec<Token>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Parse {
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(Diagnostic::is_error)
    }
}

/// Parses one `.strand` file, `file` in the caller's
/// [`SourceMap`](crate::SourceMap).
pub fn parse(file_id: FileId, src: &str) -> Parse {
    if u32::try_from(src.len()).is_err() {
        return Parse {
            file_id,
            file: File {
                items: Vec::new(),
                span: Span::default(),
            },
            tokens: Vec::new(),
            diagnostics: vec![Diagnostic::error(
                "syntax::too_large",
                "file is larger than 4 GiB",
            )],
        };
    }
    let (tokens, mut diagnostics) = lex(src);
    let mut p = Parser::new(src, &tokens);
    let file = p.file();
    // Giving up at the end of the file skips nothing, so it is not reported.
    if p.gave_up.get() && p.pos + 1 < p.toks.len() {
        let at = p.toks[p.pos].span;
        p.diags.push(
            Diagnostic::error(
                "syntax::gave_up",
                "the parser stopped here and skipped the rest of the file",
            )
            .with_label(at, "parsing made no progress"),
        );
    }
    diagnostics.append(&mut p.diags);
    diagnostics.sort_by_key(|d| d.primary_span().map_or(0, |s| s.start));
    let diagnostics = diagnostics
        .into_iter()
        .map(|d| d.in_file(file_id))
        .collect();
    Parse {
        file_id,
        file,
        tokens,
        diagnostics,
    }
}

/// Maximum nesting of blocks and expressions before the parser reports an
/// error instead of recursing further.
pub(crate) const MAX_DEPTH: u32 = 128;

/// Maximum depth of the tree, counting the links of flat chains
/// (`a + b + c`, `a.b.c`, `else if`) as well as nesting. Chains are parsed
/// in loops, so they cost no parser stack, but every later pass that walks
/// the tree recursively sees their full depth.
pub(crate) const MAX_TREE_DEPTH: u32 = 256;

/// Peeks without consuming before the parser decides it is stuck.
const STALL_LIMIT: u32 = 50_000;

/// A significant token with its line-structure flags.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Tok {
    kind: TokenKind,
    span: Span,
    /// A line break lies between this token and the previous significant one.
    nl_before: bool,
    /// Any trivia lies between this token and the previous significant one.
    ws_before: bool,
}

/// Which kind of block an item is parsed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ctx {
    Top,
    Tree,
    Keyframes,
    Service,
}

pub(crate) struct Parser<'s> {
    src: &'s str,
    toks: Vec<Tok>,
    pos: usize,
    /// Newline significance of the innermost bracket: `true` in `{}` and at
    /// the top level, `false` in `()` and `[]`.
    nl: Vec<bool>,
    diags: Vec<Diagnostic>,
    depth: u32,
    /// Chain links open on the current path (see [`MAX_TREE_DEPTH`]).
    links: u32,
    depth_reported: bool,
    eof_reported: bool,
    last_error_at: Option<u32>,
    stall: Cell<u32>,
    gave_up: Cell<bool>,
    /// `{`s whose closing `}` was indented differently from their opening
    /// line, in the current top-level item: likely homes of a missing `}`.
    suspects: Vec<Span>,
    /// The nesting level (length of `nl`) of the space-separated value being
    /// parsed, if any: where `0 -2px` is ambiguous.
    pub(crate) value_level: Option<usize>,
    /// Blocks are closing early because a top-level declaration started;
    /// only the innermost one reports it.
    unwinding: bool,
}

/// Declarations that only appear at the top level. One of these at column 0
/// inside an open block means a `}` is missing above it.
const DECL_STARTS: &[&str] = &[
    "component",
    "bar",
    "panel",
    "osd",
    "export",
    "enum",
    "type",
    "fn",
    "tokens",
    "use",
    "service",
    "keyframes",
];

/// Words that start a top-level item.
pub(crate) const TOP_KEYWORDS: &[&str] = &[
    "component",
    "bar",
    "panel",
    "osd",
    "lock",
    "state",
    "let",
    "export",
    "enum",
    "type",
    "fn",
    "tokens",
    "use",
    "service",
    "permit",
    "keyframes",
    "on",
    "after",
    "every",
];

/// Words that start a tree item.
pub(crate) const TREE_KEYWORDS: &[&str] = &[
    "when", "if", "match", "for", "on", "after", "every", "enter", "exit", "state", "let", "slot",
    "set", "play",
];

/// Words that start a handler statement.
pub(crate) const STMT_KEYWORDS: &[&str] = &["let", "if", "match", "for", "play"];

impl<'s> Parser<'s> {
    fn new(src: &'s str, tokens: &[Token]) -> Self {
        let mut toks = Vec::with_capacity(tokens.len() / 2 + 1);
        let (mut nl, mut ws) = (false, false);
        for t in tokens {
            if t.kind.is_trivia() {
                ws = true;
                nl |= t.kind == K::Newline;
                continue;
            }
            toks.push(Tok {
                kind: t.kind,
                span: t.span,
                nl_before: nl,
                ws_before: ws,
            });
            nl = false;
            ws = false;
        }
        let end = u32::try_from(src.len()).unwrap_or(u32::MAX);
        toks.push(Tok {
            kind: K::Eof,
            span: Span::at(end),
            nl_before: true,
            ws_before: true,
        });
        Self {
            src,
            toks,
            pos: 0,
            nl: vec![true],
            diags: Vec::new(),
            depth: 0,
            links: 0,
            depth_reported: false,
            eof_reported: false,
            last_error_at: None,
            stall: Cell::new(0),
            gave_up: Cell::new(false),
            suspects: Vec::new(),
            value_level: None,
            unwinding: false,
        }
    }

    // -----------------------------------------------------------------------
    // Cursor

    fn eof_tok(&self) -> Tok {
        self.toks[self.toks.len() - 1]
    }

    pub(crate) fn nth(&self, n: usize) -> Tok {
        if self.gave_up.get() {
            return self.eof_tok();
        }
        let s = self.stall.get() + 1;
        self.stall.set(s);
        if s > STALL_LIMIT {
            self.gave_up.set(true);
            return self.eof_tok();
        }
        self.toks
            .get(self.pos + n)
            .copied()
            .unwrap_or_else(|| self.eof_tok())
    }

    pub(crate) fn cur(&self) -> Tok {
        self.nth(0)
    }

    pub(crate) fn kind(&self) -> TokenKind {
        self.cur().kind
    }

    pub(crate) fn nth_kind(&self, n: usize) -> TokenKind {
        self.nth(n).kind
    }

    pub(crate) fn at(&self, kind: TokenKind) -> bool {
        self.kind() == kind
    }

    pub(crate) fn text(&self, tok: Tok) -> &'s str {
        tok.span.text(self.src)
    }

    /// The current token is the identifier `word`.
    pub(crate) fn at_kw(&self, word: &str) -> bool {
        let t = self.cur();
        t.kind == K::Ident && self.text(t) == word
    }

    pub(crate) fn bump(&mut self) -> Tok {
        let t = self.cur();
        if t.kind != K::Eof && !self.gave_up.get() {
            self.pos += 1;
            self.stall.set(0);
        }
        t
    }

    pub(crate) fn eat(&mut self, kind: TokenKind) -> Option<Tok> {
        if self.at(kind) {
            Some(self.bump())
        } else {
            None
        }
    }

    /// End offset of the last consumed token.
    pub(crate) fn prev_end(&self) -> u32 {
        if self.pos == 0 {
            0
        } else {
            self.toks[self.pos - 1].span.end
        }
    }

    pub(crate) fn prev_kind(&self) -> Option<TokenKind> {
        self.pos.checked_sub(1).map(|i| self.toks[i].kind)
    }

    /// Span from `start` to the end of the last consumed token. A node that
    /// consumed nothing is empty at the end of the last consumed token, like
    /// an error node, so it never lies outside its parent.
    pub(crate) fn finish(&self, start: u32) -> Span {
        let end = self.prev_end();
        if end < start {
            Span::at(end)
        } else {
            Span::new(start, end)
        }
    }

    // -----------------------------------------------------------------------
    // Line structure

    pub(crate) fn nl_significant(&self) -> bool {
        self.nl.last().copied().unwrap_or(true)
    }

    /// The current token starts a new line where line breaks matter.
    pub(crate) fn on_new_line(&self) -> bool {
        self.nl_significant() && self.cur().nl_before
    }

    pub(crate) fn same_line(&self) -> bool {
        !self.on_new_line()
    }

    /// The current token directly touches the previous one (no trivia).
    pub(crate) fn touching(&self) -> bool {
        !self.cur().ws_before
    }

    /// The current item has ended: `;`, `}`, end of file, a significant line
    /// break, or the previous token closed a block.
    pub(crate) fn at_item_end(&self) -> bool {
        matches!(self.kind(), K::Semi | K::RBrace | K::Eof)
            || self.on_new_line()
            || self.prev_kind() == Some(K::RBrace)
    }

    pub(crate) fn with_nl<T>(&mut self, significant: bool, f: impl FnOnce(&mut Self) -> T) -> T {
        self.nl.push(significant);
        let out = f(self);
        self.nl.pop();
        out
    }

    /// Enters one level of nesting; false (after reporting once) when too
    /// deep.
    pub(crate) fn enter(&mut self) -> bool {
        if self.depth >= MAX_DEPTH || self.depth + self.links >= MAX_TREE_DEPTH {
            self.too_deep();
            return false;
        }
        self.depth += 1;
        true
    }

    pub(crate) fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// Adds one link to a flat chain; false (after reporting once) when the
    /// tree would get too deep.
    pub(crate) fn enter_link(&mut self) -> bool {
        if self.depth + self.links >= MAX_TREE_DEPTH {
            self.too_deep();
            return false;
        }
        self.links += 1;
        true
    }

    pub(crate) fn leave_links(&mut self, n: u32) {
        self.links = self.links.saturating_sub(n);
    }

    fn too_deep(&mut self) {
        if self.depth_reported {
            return;
        }
        self.depth_reported = true;
        let span = self.cur().span;
        let msg = if self.depth >= MAX_DEPTH {
            format!("nesting is deeper than {MAX_DEPTH} levels")
        } else {
            format!("this is more than {MAX_TREE_DEPTH} levels deep, counting chained operators")
        };
        self.push_error(
            Diagnostic::error("syntax::too_deep", msg).with_label(span, "too deeply nested"),
        );
    }

    // -----------------------------------------------------------------------
    // Errors

    pub(crate) fn push_error(&mut self, d: Diagnostic) {
        let at = d.primary_span().map(|s| s.start);
        if at.is_some() && at == self.last_error_at {
            return;
        }
        self.last_error_at = at;
        self.diags.push(d);
    }

    /// How the current token reads in "found …".
    pub(crate) fn found(&self) -> String {
        let t = self.cur();
        if self.on_new_line() && t.kind != K::Eof {
            return "a line break".into();
        }
        match t.kind {
            K::Ident | K::Number | K::Dollar | K::At | K::Hash => format!("`{}`", self.text(t)),
            k => k.describe().into(),
        }
    }

    /// Where an "expected …" label goes: on the current token, or just after
    /// the previous one when the current token is on a later line.
    pub(crate) fn error_span(&self) -> Span {
        let t = self.cur();
        if t.kind == K::Eof || self.on_new_line() {
            Span::at(self.prev_end())
        } else {
            t.span
        }
    }

    /// Reports "expected `what`, found …" at the current token.
    pub(crate) fn expected(&mut self, what: &str) {
        self.expected_with(what, None);
    }

    pub(crate) fn expected_with(&mut self, what: &str, help: Option<String>) {
        if self.kind() == K::Unknown && self.same_line() {
            return; // the lexer already reported it
        }
        let mut d = Diagnostic::error(
            "syntax::expected",
            format!("expected {what}, found {}", self.found()),
        )
        .with_label(self.error_span(), format!("expected {what}"));
        if let Some(h) = help {
            d = d.with_help(h);
        }
        self.push_error(d);
    }

    /// [`Parser::expected_with`] whose help is "did you mean `fix`?", with
    /// `fix` proposed as the replacement for the text at `at` (the
    /// misspelt word, which is not always where the error points).
    pub(crate) fn expected_suggesting(&mut self, what: &str, at: Span, fix: Option<String>) {
        if self.kind() == K::Unknown && self.same_line() {
            return; // the lexer already reported it
        }
        let mut d = Diagnostic::error(
            "syntax::expected",
            format!("expected {what}, found {}", self.found()),
        )
        .with_label(self.error_span(), format!("expected {what}"));
        d.suggest_opt(at, fix);
        self.push_error(d);
    }

    /// Reports "expected `what`, found …" on the current token itself, even
    /// at the start of a line. For the start of an item, where a line break
    /// is normal and the bad token is what to point at.
    pub(crate) fn expected_at_token(&mut self, what: &str) {
        let t = self.cur();
        if t.kind == K::Unknown {
            return; // the lexer already reported it
        }
        if t.kind == K::Eof {
            self.expected(what);
            return;
        }
        let found = match t.kind {
            K::Ident | K::Number | K::Dollar | K::At | K::Hash => format!("`{}`", self.text(t)),
            k => k.describe().into(),
        };
        self.push_error(
            Diagnostic::error(
                "syntax::expected",
                format!("expected {what}, found {found}"),
            )
            .with_label(t.span, format!("expected {what}")),
        );
    }

    /// The current token starts a new line that begins a prop, field or
    /// token entry: a name directly touching `:` (`color:`, `$fg:`,
    /// `radius.lg:`, the token key `1:`). Such a line never continues the
    /// previous one.
    pub(crate) fn at_entry_line(&self) -> bool {
        if !self.on_new_line() {
            return false;
        }
        let mut i = 0;
        let first = self.cur();
        let key_start = match first.kind {
            K::Ident | K::Dollar => true,
            K::Number => self.text(first).bytes().all(|b| b.is_ascii_digit()),
            _ => false,
        };
        if !key_start {
            return false;
        }
        loop {
            i += 1;
            let t = self.nth(i);
            if t.ws_before {
                return false;
            }
            match t.kind {
                K::Colon => return true,
                K::Dot if matches!(self.nth_kind(i + 1), K::Ident | K::Number) => i += 1,
                _ => return false,
            }
        }
    }

    /// Where an operand is required: if a line break and a new prop come
    /// first (`value: <->` then `color: …`), the line ended without its
    /// right-hand side. Reports it at the end of the line and returns true;
    /// the next line is left to be its own item.
    ///
    /// The same goes for a closing bracket or the end of the file on the
    /// next line (`height: 36 ~` then `}`): the operand was never written.
    pub(crate) fn dangling_at_line_end(&mut self) -> bool {
        let t = self.cur();
        let closes = t.nl_before && matches!(t.kind, K::RBrace | K::RParen | K::RBracket | K::Eof);
        if !(closes || self.at_entry_line()) {
            return false;
        }
        let op = self
            .pos
            .checked_sub(1)
            .map_or("", |i| self.text(self.toks[i]));
        let at = Span::at(self.prev_end());
        self.push_error(
            Diagnostic::error(
                "syntax::missing_value",
                format!("`{op}` at the end of the line has nothing after it"),
            )
            .with_label(at, "expected a value here, on the same line")
            .with_help(match t.kind {
                K::Eof => "the file ends after this line".to_string(),
                _ if closes => format!(
                    "the next line starts with `{}`, so this line ends here",
                    self.text(t)
                ),
                _ => "the next line starts a new prop, so this line ends here".to_string(),
            }),
        );
        true
    }

    /// Consumes `kind` or reports it missing.
    pub(crate) fn expect(&mut self, kind: TokenKind) -> Option<Tok> {
        if let Some(t) = self.eat(kind) {
            return Some(t);
        }
        self.expected(kind.describe());
        None
    }

    /// Consumes keyword `word` or reports it missing, suggesting it when
    /// the current word is a near miss.
    pub(crate) fn expect_kw(&mut self, word: &str) -> bool {
        if self.at_kw(word) {
            self.bump();
            return true;
        }
        let fix = (self.kind() == K::Ident)
            .then(|| closest(self.text(self.cur()), [word]))
            .flatten();
        let at = self.cur().span;
        self.expected_suggesting(&format!("`{word}`"), at, fix);
        if help_was_typo(self, word) {
            self.bump();
        }
        false
    }

    pub(crate) fn ident(&mut self, what: &str) -> Ident {
        if self.at(K::Ident) {
            let t = self.bump();
            return Ident {
                name: self.text(t).to_string(),
                span: t.span,
            };
        }
        self.expected(what);
        // At the end of what was consumed, so it stays inside its parent.
        Ident {
            name: String::new(),
            span: Span::at(self.prev_end()),
        }
    }

    /// Reports that the item should have ended and skips to the end of it.
    pub(crate) fn expect_item_end(&mut self, after: &str) {
        if self.at_item_end() {
            return;
        }
        let t = self.cur();
        let help = if matches!(t.kind, K::LParen | K::LBracket) && t.ws_before {
            Some(format!(
                "to call or index, remove the space before `{}`",
                self.text(t)
            ))
        } else {
            None
        };
        self.expected_with(&format!("`;` or a line break after {after}"), help);
        self.recover_line();
    }

    /// Skips to the next `;`, line break or `}` at the current brace depth.
    pub(crate) fn recover_line(&mut self) {
        let mut depth = 0usize;
        let mut first = true;
        loop {
            let t = self.cur();
            match t.kind {
                K::Eof => break,
                K::RBrace | K::Semi if depth == 0 => break,
                _ if depth == 0 && !first && self.on_new_line() => break,
                K::LBrace => depth += 1,
                K::RBrace => depth -= 1,
                _ => {}
            }
            first = false;
            self.bump();
        }
    }

    // -----------------------------------------------------------------------
    // Blocks

    /// Parses `{ item SEP item … }` with recovery. `optional_same_line`
    /// blocks must start on the current line; the caller checks that.
    pub(crate) fn block<T>(
        &mut self,
        what: &str,
        mut item: impl FnMut(&mut Self) -> T,
        error_item: impl Fn(Span) -> T,
    ) -> Block<T> {
        let Some(open) = self.eat(K::LBrace) else {
            self.expected(&format!("`{{` to start {what}"));
            return Block {
                items: Vec::new(),
                span: Span::at(self.prev_end()),
            };
        };
        if !self.enter() {
            self.skip_balanced();
            return Block {
                items: Vec::new(),
                span: self.finish(open.span.start),
            };
        }
        let items = self.with_nl(true, |p| p.items(Some(open), &mut item, &error_item));
        self.leave();
        Block {
            items,
            span: self.finish(open.span.start),
        }
    }

    /// The item loop shared by blocks and the file. Consumes the closing
    /// `}` when `open` is given.
    fn items<T>(
        &mut self,
        open: Option<Tok>,
        item: &mut impl FnMut(&mut Self) -> T,
        error_item: &impl Fn(Span) -> T,
    ) -> Vec<T> {
        let mut out = Vec::new();
        loop {
            while self.eat(K::Semi).is_some() {}
            if let Some(open) = open {
                if self.block_ends(open, false) {
                    break;
                }
            } else {
                self.unwinding = false;
                self.suspects.clear();
            }
            match self.kind() {
                K::Eof => break,
                K::RBrace => {
                    let t = self.bump();
                    self.push_error(
                        Diagnostic::error("syntax::unmatched", "unmatched `}`")
                            .with_label(t.span, "no `{` opens this"),
                    );
                    continue;
                }
                _ => {}
            }
            let before = self.pos;
            let start = self.cur().span.start;
            let it = item(self);
            if self.pos == before {
                // The item consumed nothing (and reported why): skip what
                // cannot start an item, keeping braces balanced.
                self.skip_stray();
                out.push(error_item(self.finish(start)));
                continue;
            }
            out.push(it);
            self.expect_item_end("this item");
        }
        out
    }

    /// At the head of each turn of a `{ … }` loop: consumes the closing `}`,
    /// or sees the end of the file or a declaration at column 0 and reports
    /// the block unclosed (once, for the innermost block). True when the
    /// loop should stop.
    ///
    /// `arms` is for `match` arms and `enum` variants, which no item can
    /// start, so `let` and `state` at column 0 also end them.
    pub(crate) fn block_ends(&mut self, open: Tok, arms: bool) -> bool {
        if self.at_column_zero_decl(arms) {
            if !self.unwinding {
                self.unwinding = true;
                let at = self.cur().span;
                self.unclosed(open, at, "the next declaration starts here");
            }
            return true;
        }
        match self.kind() {
            K::Eof => {
                if !self.eof_reported {
                    self.eof_reported = true;
                    self.unclosed(open, Span::at(self.prev_end()), "the file ends here");
                }
                true
            }
            K::RBrace => {
                let close = self.bump();
                if close.nl_before
                    && self.indent_of(close.span.start) != self.indent_of(open.span.start)
                {
                    self.suspects.push(open.span);
                }
                true
            }
            _ => false,
        }
    }

    fn unclosed(&mut self, open: Tok, cut: Span, cut_label: &str) {
        let mut d = Diagnostic::error("syntax::unclosed", "unclosed `{`")
            .with_label(open.span, "this `{` has no matching `}`")
            .with_secondary(cut, cut_label);
        // Only a sloppy `}` inside the unclosed block can explain it. In a
        // cascade each `}` closes the block one level out, so the innermost
        // suspect (the one opened last) is where the `}` went missing.
        let suspect = self
            .suspects
            .iter()
            .filter(|s| s.start > open.span.start)
            .max_by_key(|s| s.start)
            .copied();
        if let Some(s) = suspect {
            d = d.with_secondary(
                s,
                "probably missing its `}`: the `}` that closed it is indented differently",
            );
        }
        self.push_error(d);
    }

    /// Leading whitespace of the line holding `offset`.
    fn indent_of(&self, offset: u32) -> usize {
        let before = self.src.get(..offset as usize).unwrap_or("");
        let line = &before[before.rfind(['\n', '\r']).map_or(0, |i| i + 1)..];
        line.len() - line.trim_start_matches([' ', '\t']).len()
    }

    /// A top-level declaration keyword at the very start of a line (with
    /// `also_items`, `let` and `state` count too).
    fn at_column_zero_decl(&self, also_items: bool) -> bool {
        let t = self.cur();
        if t.kind != K::Ident || !t.nl_before || self.indent_of(t.span.start) != 0 {
            return false;
        }
        let word = self.text(t);
        (DECL_STARTS.contains(&word) || also_items && matches!(word, "let" | "state"))
            && self.nth_kind(1) == K::Ident
    }

    /// Skips what cannot start an item. A `{` on its own line (Allman
    /// style after a prop) takes its whole block with it, so the braces
    /// stay in step; other junk is skipped up to the next token on its line
    /// that could start an item (`; ) height: 4` keeps `height: 4`).
    pub(crate) fn skip_stray(&mut self) {
        match self.kind() {
            K::LBrace => {
                let t = self.bump();
                self.push_error(
                    Diagnostic::error("syntax::brace_line", "this `{` belongs to nothing")
                        .with_label(t.span, "a block cannot start an item")
                        .with_help(
                            "an element's or prop's `{` must be on the same line as the \
                             element or prop",
                        ),
                );
                self.skip_balanced();
            }
            _ => {
                self.bump();
                while !self.on_new_line()
                    && !matches!(
                        self.kind(),
                        K::Ident | K::At | K::Hash | K::LBrace | K::RBrace | K::Semi | K::Eof
                    )
                {
                    self.bump();
                }
            }
        }
    }

    /// Too deep to parse what starts here: skip it whole when it is a
    /// bracketed group, so each enclosing level still finds its closing
    /// bracket and the one `too_deep` error is the only one.
    pub(crate) fn skip_group(&mut self) {
        if !matches!(self.kind(), K::LParen | K::LBracket | K::LBrace) {
            return;
        }
        let mut depth = 0usize;
        loop {
            match self.bump().kind {
                K::Eof => break,
                K::LParen | K::LBracket | K::LBrace => depth += 1,
                K::RParen | K::RBracket | K::RBrace => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
    }

    /// Skips a balanced `{ … }` whose `{` was just consumed.
    fn skip_balanced(&mut self) {
        let mut depth = 1usize;
        while depth > 0 {
            match self.bump().kind {
                K::Eof => break,
                K::LBrace => depth += 1,
                K::RBrace => depth -= 1,
                _ => {}
            }
        }
    }

    fn file(&mut self) -> File {
        let items = self.items(None, &mut |p| p.item(Ctx::Top), &|span| Item {
            attrs: Vec::new(),
            kind: ItemKind::Error,
            span,
        });
        let end = u32::try_from(self.src.len()).unwrap_or(u32::MAX);
        File {
            items,
            span: Span::new(0, end),
        }
    }
}

/// After a failed `expect_kw`, skip the misspelt keyword itself so parsing
/// continues as if it were right (`for x im xs` still parses `xs`).
fn help_was_typo(p: &Parser<'_>, word: &str) -> bool {
    p.kind() == K::Ident
        && p.same_line()
        && crate::diagnostic::suggest(p.text(p.cur()), [word]).is_some()
}
