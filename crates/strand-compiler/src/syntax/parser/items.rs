//! Declarations, tree items and handler statements.

use crate::diagnostic::{Diagnostic, did_you_mean, suggest};

use super::super::ast::*;
use super::super::span::Span;
use super::{Ctx, K, Parser, STMT_KEYWORDS, TOP_KEYWORDS, TREE_KEYWORDS, Tok};

/// Service source kinds.
const SOURCES: &[&str] = &["dbus", "file", "listen", "poll"];

fn error_item(span: Span) -> Item {
    Item {
        attrs: Vec::new(),
        kind: ItemKind::Error,
        span,
    }
}

fn error_stmt(span: Span) -> Stmt {
    Stmt {
        kind: StmtKind::Error,
        span,
    }
}

fn error_entry(span: Span) -> TokenEntry {
    TokenEntry {
        override_: None,
        key: TokenKey {
            dollar: false,
            segments: Vec::new(),
            span,
        },
        body: TokenBody::Value(Expr {
            kind: ExprKind::Error,
            span,
        }),
        span,
    }
}

fn error_field(span: Span) -> Field {
    Field {
        name: Ident {
            name: String::new(),
            span,
        },
        ty: Type {
            kind: TypeKind::Error,
            span,
        },
        rw: None,
        default: None,
        span,
    }
}

/// Keywords that start a handler block of statements.
const HANDLER_KEYWORDS: &[&str] = &["on", "after", "every"];

/// The help for a handler statement written where tree items go.
const STATEMENT_HELP: &str = "statements go inside a handler: `on click { … }`";

/// A one-edit slip of one of `kws` (`stat` for `state`, `onn` for `on`).
/// The word must be at least as long as the keyword or three letters, so
/// a short name (`n`, `i`) is never taken for `on` or `if`.
fn keyword_slip(word: &str, kws: &[&'static str]) -> Option<&'static str> {
    let n = word.chars().count();
    kws.iter()
        .copied()
        .find(|kw| *kw != word && n >= kw.len().min(3) && strsim::osa_distance(word, kw) == 1)
}

/// `t` (the token after a word) shows the word starts a handler statement,
/// not an item: `x = 1`, `t += 1`, `n.expire()`.
fn starts_statement(t: Tok) -> bool {
    matches!(
        t.kind,
        K::Eq | K::PlusEq | K::MinusEq | K::StarEq | K::SlashEq
    ) || matches!(t.kind, K::Dot | K::QuestionDot) && !t.ws_before
}

/// No keyword is directly followed by `t`: a touching `(`, `[`, `.` or
/// `?.`, or an assignment.
fn cannot_follow_keyword(t: Tok) -> bool {
    starts_statement(t) || matches!(t.kind, K::LParen | K::LBracket) && !t.ws_before
}

impl Parser<'_> {
    // -----------------------------------------------------------------------
    // Blocks of each kind

    pub(crate) fn tree_block(&mut self, what: &str) -> Block<Item> {
        self.block(what, |p| p.item(Ctx::Tree), error_item)
    }

    pub(crate) fn stmt_block(&mut self, what: &str) -> Block<Stmt> {
        self.block(what, Parser::stmt, error_stmt)
    }

    pub(crate) fn token_block(&mut self, what: &str) -> Block<TokenEntry> {
        self.block(what, Parser::token_entry, error_entry)
    }

    fn field_block(&mut self, what: &str) -> Block<Field> {
        self.block(what, Parser::field, error_field)
    }

    // -----------------------------------------------------------------------
    // Items

    fn attributes(&mut self) -> Vec<Attribute> {
        let mut attrs = Vec::new();
        while self.at(K::At) {
            let t = self.bump();
            let text = self.text(t);
            let name = Ident {
                name: text[1..].to_string(),
                span: Span::new(t.span.start + 1, t.span.end),
            };
            let args = if self.at(K::LParen) && self.touching() {
                self.call_args()
            } else {
                Vec::new()
            };
            attrs.push(Attribute {
                name,
                args,
                span: self.finish(t.span.start),
            });
        }
        attrs
    }

    /// One item of a block of kind `ctx`.
    pub(crate) fn item(&mut self, ctx: Ctx) -> Item {
        let start = self.cur().span.start;
        let attrs = self.attributes();
        if !attrs.is_empty() && matches!(self.kind(), K::RBrace | K::Eof) {
            self.expected("an item after the attribute");
        }
        let kind = self.item_kind(ctx);
        Item {
            attrs,
            kind,
            span: self.finish(start),
        }
    }

    fn misplaced(&mut self, span: Span, what: &str, place: &str) {
        self.push_error(
            Diagnostic::error("syntax::misplaced", format!("{what} {place}"))
                .with_label(span, "not allowed here"),
        );
    }

    fn item_kind(&mut self, ctx: Ctx) -> ItemKind {
        let t = self.cur();
        match t.kind {
            K::Ident => {}
            K::Hash if ctx != Ctx::Top => return self.selector(),
            K::Number if ctx == Ctx::Keyframes => return self.keyframe_stop(),
            // A stray `{` is reported by the block loop, which skips it whole.
            K::LBrace => return ItemKind::Error,
            _ => {
                let want = match ctx {
                    Ctx::Top => "a declaration",
                    Ctx::Tree | Ctx::Keyframes => "a prop, element or `when`/`if`/`for`/`on`",
                    Ctx::Service => "a field such as `name: text = Prop`",
                };
                self.expected_at_token(want);
                return ItemKind::Error;
            }
        }
        if let Some(name) = (ctx != Ctx::Service).then(|| self.kebab_name()).flatten() {
            let prop = self.prop_after_name(name);
            if ctx == Ctx::Top {
                self.misplaced(
                    t.span,
                    "props",
                    "must be inside a surface, component or element",
                );
            }
            return ItemKind::Prop(prop);
        }
        let word = self.text(t);
        if self.nth_kind(1) == K::Colon {
            if ctx == Ctx::Service {
                return ItemKind::Field(self.field());
            }
            let prop = self.prop();
            if ctx == Ctx::Top {
                self.misplaced(
                    t.span,
                    "props",
                    "must be inside a surface, component or element",
                );
            }
            return ItemKind::Prop(prop);
        }
        if let Some(kind) = self.keyword_item(ctx, t, word) {
            return kind;
        }
        if ctx == Ctx::Service {
            let help = did_you_mean(word, ["permit"]);
            self.expected_with("a field such as `name: text = Prop`", help);
            self.bump();
            return ItemKind::Error;
        }
        let next = self.nth(1);
        if ctx == Ctx::Top && !cannot_follow_keyword(next) {
            // A slip of a declaration keyword (`compnent`, `tokns`): say
            // so once and parse what was meant. An element here would be
            // an error anyway, so nothing is lost.
            if let Some(kw) = keyword_slip(word, TOP_KEYWORDS) {
                self.push_error(
                    Diagnostic::error(
                        "syntax::expected",
                        format!("expected a declaration, found `{word}`"),
                    )
                    .with_label(t.span, "unknown declaration")
                    .with_help(format!("did you mean `{kw}`?")),
                );
                if let Some(kind) = self.keyword_item(ctx, t, kw) {
                    return kind;
                }
            }
        }
        if ctx != Ctx::Top {
            // `aftr 6s { n.expire() }`: a block of statements shows a
            // misspelt handler keyword, not an element.
            if let Some(kw) = keyword_slip(word, HANDLER_KEYWORDS)
                .filter(|_| !cannot_follow_keyword(next) && self.statement_block_follows())
            {
                self.push_error(
                    Diagnostic::error(
                        "syntax::expected",
                        format!("expected an element or handler, found `{word}`"),
                    )
                    .with_label(t.span, "its block holds statements, so this is a handler")
                    .with_help(format!("did you mean `{kw}`?")),
                );
                if let Some(kind) = self.keyword_item(ctx, t, kw) {
                    return kind;
                }
            }
        }
        let (el, statement) = self.element(ctx != Ctx::Top);
        if ctx == Ctx::Top {
            let (label, help) = if statement {
                ("statements go inside a handler", Some(STATEMENT_HELP))
            } else {
                ("elements must be inside a surface or component", None)
            };
            let mut d = Diagnostic::error(
                "syntax::misplaced",
                format!("expected a declaration, found `{word}`"),
            )
            .with_label(t.span, label);
            if let Some(h) = help {
                d = d.with_help(h);
            }
            self.push_error(d);
        }
        ItemKind::Element(el)
    }

    /// The item that keyword `word` starts in a block of kind `ctx`, or
    /// `None` when `word` starts no item there. `t` is the item's first
    /// token: the keyword, or a misspelling of it.
    fn keyword_item(&mut self, ctx: Ctx, t: Tok, word: &str) -> Option<ItemKind> {
        let top_only = |p: &mut Self, what: &str| {
            if ctx != Ctx::Top {
                p.misplaced(t.span, what, "are only allowed at the top level of a file");
            }
        };
        let tree_only = |p: &mut Self, what: &str| {
            if !matches!(ctx, Ctx::Tree | Ctx::Keyframes) {
                p.misplaced(
                    t.span,
                    what,
                    "must be inside a surface, component or element",
                );
            }
        };
        Some(match word {
            "component" => {
                top_only(self, "components");
                self.component()
            }
            "bar" | "panel" | "osd" | "lock" if ctx == Ctx::Top => self.surface(word),
            "state" if ctx != Ctx::Service => {
                if ctx == Ctx::Keyframes {
                    tree_only(self, "`state`");
                }
                self.state(None)
            }
            "let" if ctx != Ctx::Service => ItemKind::Let(self.let_decl(None)),
            "export" => {
                top_only(self, "exports");
                self.export()
            }
            "enum" => {
                top_only(self, "enums");
                self.enum_decl()
            }
            "type" => {
                top_only(self, "type declarations");
                self.type_decl()
            }
            "fn" => {
                top_only(self, "functions");
                self.fn_decl()
            }
            "tokens" => {
                top_only(self, "token sets");
                self.tokens_decl()
            }
            "use" => {
                top_only(self, "`use` declarations");
                self.use_decl()
            }
            "service" => {
                top_only(self, "services");
                self.service()
            }
            "keyframes" => {
                top_only(self, "keyframes");
                self.keyframes()
            }
            "permit" => {
                if !matches!(ctx, Ctx::Top | Ctx::Service) {
                    self.misplaced(
                        t.span,
                        "`permit`",
                        "belongs at the top level or in a service",
                    );
                }
                self.permit()
            }
            "on" => {
                if ctx == Ctx::Service {
                    self.misplaced(t.span, "handlers", "are not allowed in a service");
                }
                self.on()
            }
            "after" | "every" => {
                if ctx == Ctx::Service {
                    self.misplaced(t.span, "timers", "are not allowed in a service");
                }
                self.timer(if word == "after" {
                    TimerKind::After
                } else {
                    TimerKind::Every
                })
            }
            "when" => {
                tree_only(self, "`when`");
                self.when()
            }
            "if" => {
                tree_only(self, "`if`");
                ItemKind::If(self.if_(&mut |p: &mut Self| p.tree_block("the `if` body")))
            }
            "for" => {
                tree_only(self, "`for`");
                ItemKind::For(self.for_(&mut |p: &mut Self| p.tree_block("the `for` body")))
            }
            "match" => {
                tree_only(self, "`match`");
                ItemKind::Match(self.match_(&mut |p: &mut Self| p.tree_arm_body()))
            }
            "enter" | "exit" => {
                tree_only(self, "poses");
                self.bump();
                let kind = if word == "enter" {
                    PoseKind::Enter
                } else {
                    PoseKind::Exit
                };
                ItemKind::Pose(Pose {
                    kind,
                    body: self.tree_block("the pose"),
                })
            }
            "slot" => {
                tree_only(self, "`slot`");
                self.bump();
                ItemKind::Slot
            }
            "set" if self.nth_kind(1) == K::LBrace => {
                tree_only(self, "`set` blocks");
                self.bump();
                ItemKind::Set(self.token_block("the token overrides"))
            }
            "play" => {
                tree_only(self, "`play`");
                self.bump();
                ItemKind::Play(self.expr())
            }
            "else" => {
                self.bump();
                self.push_error(
                    Diagnostic::error("syntax::misplaced", "`else` without `if`")
                        .with_label(t.span, "no `if` before this"),
                );
                if self.at_kw("if") {
                    ItemKind::If(self.if_(&mut |p: &mut Self| p.tree_block("the `if` body")))
                } else {
                    let _ = self.tree_block("the `else` body");
                    ItemKind::Error
                }
            }
            _ => return None,
        })
    }

    // -----------------------------------------------------------------------
    // Tree items

    /// `name: value ~ transition { … }` or `name: <-> place`.
    fn prop(&mut self) -> Prop {
        let name = self.ident("a prop name");
        self.prop_after_name(name)
    }

    /// `max-width:` (words joined by `-`, touching, then `:`): names are
    /// snake_case, so report it and return the name it meant, having
    /// consumed it. The `:` is left for the prop.
    fn kebab_name(&mut self) -> Option<Ident> {
        let mut i = 0;
        loop {
            let (dash, word) = (self.nth(i + 1), self.nth(i + 2));
            if dash.kind != K::Minus || dash.ws_before || word.kind != K::Ident || word.ws_before {
                break;
            }
            i += 2;
        }
        let colon = self.nth(i + 1);
        if i == 0 || colon.kind != K::Colon || colon.ws_before {
            return None;
        }
        let start = self.cur().span.start;
        for _ in 0..=i {
            self.bump();
        }
        let span = self.finish(start);
        let written = span.text(self.src);
        let name = written.replace('-', "_");
        self.push_error(
            Diagnostic::error("syntax::kebab_case", format!("`{written}` is not a name"))
                .with_label(span, "`-` is subtraction, not part of a name")
                .with_help(format!("names are snake_case: `{name}`")),
        );
        Some(Ident { name, span })
    }

    /// The rest of a prop after its name: `: value ~ transition { … }`.
    fn prop_after_name(&mut self, name: Ident) -> Prop {
        self.bump(); // ':'
        if let Some(value) = self.missing_value(&name.name) {
            return Prop {
                name,
                two_way: None,
                value,
                transition: None,
                block: None,
            };
        }
        let (two_way, value) = if let Some(t) = self.eat(K::TwoWay) {
            (Some(t.span), self.expr())
        } else {
            (None, self.value())
        };
        let transition = self.eat(K::Tilde).map(|_| self.expr());
        let block = (self.same_line() && self.at(K::LBrace))
            .then(|| self.tree_block("the prop's sub-block"));
        Prop {
            name,
            two_way,
            value,
            transition,
            block,
        }
    }

    /// After `name:`: a line break, `;`, `}` or the end of the file means
    /// the value is missing (a prop never continues onto the next line
    /// after its `:`). Reports it and returns the error value.
    fn missing_value(&mut self, name: &str) -> Option<Expr> {
        if !(self.at_item_end() && self.prev_kind() == Some(K::Colon)) {
            return None;
        }
        let at = Span::at(self.prev_end());
        self.push_error(
            Diagnostic::error("syntax::missing_value", format!("`{name}:` has no value"))
                .with_label(at, "expected a value here, on the same line"),
        );
        Some(self.error_expr())
    }

    /// `kind [positional] { … }`. With `report_typo`, a kind one letter
    /// from a tree keyword followed by more than an element can hold
    /// (`stat x = 0`) says what was meant. Also returns whether the word
    /// starts a handler statement instead (`x = 1`, `n.expire()`), which
    /// is reported (in a tree) and skipped to the end of the line.
    fn element(&mut self, report_typo: bool) -> (Element, bool) {
        let kind = self.ident("an element");
        let next = self.cur();
        if self.same_line() && starts_statement(next) {
            if report_typo {
                self.expected_with(
                    "`;` or a line break after this element",
                    Some(STATEMENT_HELP.to_string()),
                );
            }
            self.recover_line();
            let el = Element {
                kind,
                arg: None,
                block: None,
            };
            return (el, true);
        }
        let mut arg = None;
        if self.same_line() && self.starts_expr() {
            arg = Some(if self.at(K::Ident) && self.nth_kind(1) == K::Colon {
                let name = self.ident("a prop name");
                self.bump();
                HeadArg::Named(name, self.expr())
            } else {
                HeadArg::Positional(self.expr())
            });
        }
        let block = if self.same_line() && self.at(K::LBrace) {
            Some(self.tree_block("the element body"))
        } else if self.at(K::LBrace) && self.nl_significant() {
            // Allman style: the body is clearly this element's, so keep it
            // (and check it) rather than desynchronising the braces.
            let t = self.cur();
            self.push_error(
                Diagnostic::error(
                    "syntax::brace_line",
                    "an element's `{` must be on the same line as the element",
                )
                .with_label(t.span, "on its own line")
                .with_help(format!("move it up: `{} {{`", kind.name)),
            );
            Some(self.tree_block("the element body"))
        } else {
            None
        };
        let spaced_call = matches!(self.kind(), K::LParen | K::LBracket) && self.cur().ws_before;
        if report_typo && !self.at_item_end() && !spaced_call && !cannot_follow_keyword(next) {
            // `stat x = 0` reads as element `stat`; say what was meant.
            if let Some(kw) = keyword_slip(&kind.name, TREE_KEYWORDS) {
                self.expected_with(
                    "`;` or a line break after this element",
                    Some(format!("did you mean `{kw}`?")),
                );
            }
        }
        (Element { kind, arg, block }, false)
    }

    /// From an item's first word: a `{` later on the line whose first
    /// item is a handler statement (`x = 1`, `t += 1`, `n.expire()`).
    fn statement_block_follows(&self) -> bool {
        let mut depth = 0usize;
        for i in 1..64 {
            let t = self.nth(i);
            if t.kind == K::Eof || depth == 0 && t.nl_before {
                return false;
            }
            match t.kind {
                K::LParen | K::LBracket => depth += 1,
                K::RParen | K::RBracket => depth = depth.saturating_sub(1),
                K::LBrace if depth == 0 => {
                    return self.nth_kind(i + 1) == K::Ident && starts_statement(self.nth(i + 2));
                }
                K::LBrace | K::RBrace | K::Semi => return false,
                _ => {}
            }
        }
        false
    }

    /// At a clause position: when the next word on this line is a near
    /// miss of one of `kws` (`kye` for `key`), reports it with a
    /// did-you-mean, skips it, and returns the keyword meant, so parsing
    /// goes on as if it were spelt right.
    fn clause_typo(&mut self, kws: &[&'static str]) -> Option<&'static str> {
        let kw = self.report_clause_typo(kws)?;
        self.bump();
        Some(kw)
    }

    /// [`Parser::clause_typo`] without skipping the word, for callers that
    /// hand it to the keyword's own parser (which consumes it).
    fn report_clause_typo(&mut self, kws: &[&'static str]) -> Option<&'static str> {
        if !(self.same_line() && self.at(K::Ident)) {
            return None;
        }
        let word = self.text(self.cur());
        if kws.contains(&word) {
            return None;
        }
        let kw = suggest(word, kws.iter().copied())?;
        let list = kws
            .iter()
            .map(|k| format!("`{k}`"))
            .collect::<Vec<_>>()
            .join(" or ");
        self.expected_with(&list, Some(format!("did you mean `{kw}`?")));
        Some(kw)
    }

    fn when(&mut self) -> ItemKind {
        self.bump();
        let cond = self.expr();
        ItemKind::When(When {
            cond,
            body: self.tree_block("the `when` body"),
        })
    }

    fn selector(&mut self) -> ItemKind {
        let t = self.bump();
        let name = Ident {
            name: self.text(t)[1..].to_string(),
            span: Span::new(t.span.start + 1, t.span.end),
        };
        ItemKind::Selector(Selector {
            name,
            body: self.tree_block("the selector body"),
        })
    }

    fn keyframe_stop(&mut self) -> ItemKind {
        let mut stops = vec![self.expr()];
        while self.eat(K::Comma).is_some() {
            stops.push(self.expr());
        }
        ItemKind::KeyframeStop(KeyframeStop {
            stops,
            body: self.tree_block("the keyframe"),
        })
    }

    fn tree_arm_body(&mut self) -> ArmBody<Item> {
        if self.at(K::LBrace) {
            ArmBody::Block(self.tree_block("the arm"))
        } else {
            ArmBody::Single(Box::new(self.item(Ctx::Tree)))
        }
    }

    fn stmt_arm_body(&mut self) -> ArmBody<Stmt> {
        if self.at(K::LBrace) {
            ArmBody::Block(self.stmt_block("the arm"))
        } else {
            ArmBody::Single(Box::new(self.stmt()))
        }
    }

    /// `if cond { … } else if … else { … }` with body parser `body`.
    /// `else if` chains are parsed in a loop (each link counts toward the
    /// tree-depth limit, not the parser's stack).
    pub(crate) fn if_<T>(&mut self, body: &mut impl FnMut(&mut Self) -> Block<T>) -> If<T> {
        // Each `if` of the chain: its start, condition and body.
        let mut chain = Vec::new();
        let mut last_else = None;
        let mut links = 0;
        loop {
            let start = self.bump().span.start; // if
            let cond = self.expr();
            let then = body(self);
            chain.push((start, cond, then));
            if !self.at_kw("else") && !self.else_typo() {
                break;
            }
            if self.at_kw("else") {
                self.bump();
            }
            if !self.at_kw("if") {
                last_else = Some(Else::Block(body(self)));
                break;
            }
            if !self.enter_link() {
                break;
            }
            links += 1;
        }
        self.leave_links(links);
        // Fold from the end: each `else if` holds the rest of the chain.
        let end = self.prev_end();
        let mut else_ = last_else;
        while chain.len() > 1 {
            if let Some((start, cond, then)) = chain.pop() {
                let node = If { cond, then, else_ };
                else_ = Some(Else::If(Box::new(node), Span::new(start, end.max(start))));
            }
        }
        match chain.pop() {
            Some((_, cond, then)) => If { cond, then, else_ },
            None => If {
                cond: self.error_expr(),
                then: Block {
                    items: Vec::new(),
                    span: Span::at(end),
                },
                else_,
            },
        }
    }

    /// `els { … }` right after an `if` body: a misspelt `else` (it would
    /// otherwise parse as an element). Reports and skips it.
    fn else_typo(&mut self) -> bool {
        if !(self.at(K::Ident) && matches!(self.nth_kind(1), K::LBrace | K::Ident)) {
            return false;
        }
        let word = self.text(self.cur());
        if strsim::osa_distance(word, "else") != 1 {
            return false;
        }
        if self.nth_kind(1) == K::Ident && self.text(self.nth(1)) != "if" {
            return false;
        }
        let t = self.bump();
        self.push_error(
            Diagnostic::error(
                "syntax::expected",
                format!("expected `else`, found `{word}`"),
            )
            .with_label(t.span, "after an `if` body")
            .with_help("did you mean `else`?"),
        );
        true
    }

    /// `for x in xs key e { … }`.
    pub(crate) fn for_<T>(&mut self, body: &mut impl FnMut(&mut Self) -> Block<T>) -> For<T> {
        self.bump(); // for
        let binding = self.ident("a loop variable");
        self.expect_kw("in");
        let iter = self.expr();
        let key = if self.same_line() && self.at_kw("key") {
            self.bump();
            Some(self.expr())
        } else if self.clause_typo(&["key"]).is_some() && !self.at(K::LBrace) {
            Some(self.expr())
        } else {
            None
        };
        For {
            binding,
            iter,
            key,
            body: body(self),
        }
    }

    /// `on click { … }`, `on change a, b after T { … }`.
    fn on(&mut self) -> ItemKind {
        self.bump(); // on
        let event = if self.at_kw("change")
            && !matches!(self.nth_kind(1), K::LParen | K::LBrace | K::Dot)
        {
            self.bump();
            let mut targets = vec![self.expr()];
            while self.eat(K::Comma).is_some() {
                targets.push(self.expr());
            }
            let debounce = ((self.same_line() && self.at_kw("after"))
                || self.clause_typo(&["after"]).is_some())
            .then(|| {
                if self.at_kw("after") {
                    self.bump();
                }
                self.expr()
            });
            Event::Change { targets, debounce }
        } else {
            let mut path = vec![self.ident("an event name")];
            while self.at(K::Dot) {
                self.bump();
                path.push(self.ident("an event name"));
            }
            let params = self.at(K::LParen).then(|| self.params());
            if params.is_none() && !self.at(K::LBrace) && self.same_line() {
                let help = (path.len() == 1)
                    .then(|| did_you_mean(&path[0].name, ["change"]))
                    .flatten();
                if help.is_some() {
                    self.expected_with("`{` to start the handler", help);
                }
            }
            Event::Named { path, params }
        };
        ItemKind::On(On {
            event,
            body: self.stmt_block("the handler"),
        })
    }

    /// `after T while cond { … }`, `every T while cond { … }`.
    fn timer(&mut self, kind: TimerKind) -> ItemKind {
        self.bump(); // after | every (or a slip of one)
        let duration = self.expr();
        let mut while_ = None;
        if self.same_line() && self.at_kw("while") {
            self.bump();
            while_ = Some(self.expr());
        } else if self.same_line() && self.at(K::Ident) {
            let help = did_you_mean(self.text(self.cur()), ["while"]);
            if help.is_some() {
                self.expected_with("`while` or `{`", help);
                self.bump();
                while_ = Some(self.expr());
            }
        }
        ItemKind::Timer(Timer {
            kind,
            duration,
            while_,
            body: self.stmt_block("the timer body"),
        })
    }

    // -----------------------------------------------------------------------
    // Declarations

    fn component(&mut self) -> ItemKind {
        self.bump();
        let name = self.ident("a component name");
        let params = self.at(K::LParen).then(|| self.params());
        let typo = !self.at_kw("tokens") && self.clause_typo(&["tokens"]).is_some();
        let tokens = (typo || self.at_kw("tokens")).then(|| {
            if !typo {
                self.bump();
            }
            self.token_block("the component tokens")
        });
        ItemKind::Component(Component {
            name,
            params,
            tokens,
            body: self.tree_block("the component body"),
        })
    }

    /// `bar Top { … }`; `kw` is the surface kind (the word may be a slip).
    fn surface(&mut self, kw: &str) -> ItemKind {
        let mut kind = self.ident("a surface");
        kind.name = kw.to_string();
        let name = if self.at(K::Ident) {
            Some(self.ident("a surface name"))
        } else {
            if kind.name != "lock" {
                self.expected_with(
                    &format!("a name for this `{}`", kind.name),
                    Some(format!("such as `{} Top {{ … }}`", kind.name)),
                );
            }
            None
        };
        ItemKind::Surface(Surface {
            kind,
            name,
            body: self.tree_block("the surface body"),
        })
    }

    fn export(&mut self) -> ItemKind {
        let t = self.bump();
        let typo = self.report_clause_typo(&["state", "let"]);
        if self.at_kw("state") || typo == Some("state") {
            self.state(Some(t.span))
        } else if self.at_kw("let") || typo == Some("let") {
            ItemKind::Let(self.let_decl(Some(t.span)))
        } else {
            self.expected("`state` or `let` after `export`");
            ItemKind::Error
        }
    }

    fn state(&mut self, export: Option<Span>) -> ItemKind {
        self.bump(); // state
        let name = self.ident("a state name");
        let typo = if self.at_kw("from") {
            None
        } else {
            self.clause_typo(&["from", "key"])
        };
        if self.at_kw("from") || typo == Some("from") {
            if typo.is_none() {
                self.bump();
            }
            let path = if self.at(K::String) {
                let t = self.bump();
                self.string_lit(t)
            } else {
                self.expected("a settings file path string");
                StringLit {
                    value: String::new(),
                    span: Span::at(self.prev_end()),
                }
            };
            let fields = self.field_block("the settings fields");
            return ItemKind::State(State {
                export,
                name,
                init: StateInit::File { path, fields },
            });
        }
        let ty = self.eat(K::Colon).map(|_| self.ty());
        let typo = match typo {
            Some(kw) => Some(kw),
            None if ty.is_some() && !self.at_kw("key") => self.clause_typo(&["key"]),
            None => None,
        };
        // A stray word right before `=` (`state a: Int kye = 3`) is
        // reported once and dropped, not read as a key with no expression.
        let key = (typo == Some("key") && !self.at(K::Eq)
            || typo.is_none() && self.same_line() && self.at_kw("key"))
        .then(|| {
            if typo.is_none() {
                self.bump();
            }
            self.expr()
        });
        let value = if self.eat(K::Eq).is_some() {
            self.expr()
        } else {
            self.expected_with(
                "`=` and an initial value",
                Some(format!("state needs a value: `state {} = …`", name.name)),
            );
            self.error_expr()
        };
        let persist = if self.same_line() && self.at_kw("persist") {
            Some(self.bump().span)
        } else {
            let at = self.cur().span;
            self.clause_typo(&["persist"]).map(|_| at)
        };
        ItemKind::State(State {
            export,
            name,
            init: StateInit::Value {
                ty,
                key,
                value,
                persist,
            },
        })
    }

    fn let_decl(&mut self, export: Option<Span>) -> Let {
        self.bump(); // let
        let name = self.ident("a name");
        let ty = self.eat(K::Colon).map(|_| self.ty());
        let value = if self.eat(K::Eq).is_some() {
            self.expr()
        } else {
            self.expected("`=` and a value");
            self.error_expr()
        };
        Let {
            export,
            name,
            ty,
            value,
        }
    }

    fn enum_decl(&mut self) -> ItemKind {
        self.bump();
        let name = self.ident("an enum name");
        let mut variants = Vec::new();
        if self.at(K::LBrace) {
            let open = self.bump();
            self.with_nl(true, |p| {
                loop {
                    while p.eat(K::Comma).is_some() || p.eat(K::Semi).is_some() {}
                    if p.block_ends(open, true) {
                        break;
                    }
                    if p.at(K::Ident) {
                        variants.push(p.ident("a variant"));
                    } else {
                        if !p.at(K::LBrace) {
                            p.expected_at_token("a variant name");
                        }
                        p.skip_stray();
                        continue;
                    }
                    if !(p.at(K::Comma) || p.at_item_end()) {
                        p.expected("`,` between variants");
                        p.recover_line();
                    }
                }
            });
        } else {
            self.expected("`{` and the variants");
        }
        ItemKind::Enum(EnumDecl { name, variants })
    }

    fn type_decl(&mut self) -> ItemKind {
        self.bump();
        let name = self.ident("a type name");
        ItemKind::Type(TypeDecl {
            name,
            fields: self.field_block("the record fields"),
        })
    }

    /// `name: Type rw = default`.
    fn field(&mut self) -> Field {
        let start = self.cur().span.start;
        let name = self.ident("a field name");
        let ty = if self.expect(K::Colon).is_some() {
            self.ty()
        } else {
            Type {
                kind: TypeKind::Error,
                span: Span::at(self.prev_end()),
            }
        };
        let start = start.min(name.span.start);
        let rw = if self.same_line() && self.at_kw("rw") {
            Some(self.bump().span)
        } else {
            let at = self.cur().span;
            self.clause_typo(&["rw"]).map(|_| at)
        };
        let default = (self.same_line() && self.at(K::Eq)).then(|| {
            self.bump();
            self.expr()
        });
        Field {
            name,
            ty,
            rw,
            default,
            span: self.finish(start),
        }
    }

    fn fn_decl(&mut self) -> ItemKind {
        self.bump();
        let name = self.ident("a function name");
        let params = if self.at(K::LParen) {
            self.params()
        } else {
            self.expected("`(` and the parameters");
            Vec::new()
        };
        let ret = self.eat(K::ThinArrow).map(|_| self.ty());
        ItemKind::Fn(FnDecl {
            name,
            params,
            ret,
            body: self.stmt_block("the function body"),
        })
    }

    fn tokens_decl(&mut self) -> ItemKind {
        self.bump();
        let name = self.ident("a token set name");
        let extends = ((self.same_line() && self.at_kw("extends"))
            || self.clause_typo(&["extends"]).is_some())
        .then(|| {
            if self.at_kw("extends") {
                self.bump();
            }
            self.ident("the token set to extend")
        });
        ItemKind::Tokens(TokensDecl {
            name,
            extends,
            entries: self.token_block("the tokens"),
        })
    }

    /// `override key: value` or `key { entries }`.
    fn token_entry(&mut self) -> TokenEntry {
        let start = self.cur().span.start;
        let key_follows =
            |t: Tok| !t.nl_before && matches!(t.kind, K::Ident | K::Dollar | K::Number);
        let mut override_ =
            (self.at_kw("override") && key_follows(self.nth(1))).then(|| self.bump().span);
        if override_.is_none() && self.at(K::Ident) && key_follows(self.nth(1)) {
            // `overide space { … }`: a misspelt `override` is an error,
            // never a new token (design.md, "Loud overrides").
            let t = self.cur();
            let word = self.text(t);
            if suggest(word, ["override"]).is_some() {
                self.bump();
                self.push_error(
                    Diagnostic::error(
                        "syntax::expected",
                        format!("expected a token name or `override`, found `{word}`"),
                    )
                    .with_label(t.span, "followed by another token name")
                    .with_help("did you mean `override`?"),
                );
                override_ = Some(t.span);
            }
        }
        let key = self.token_key();
        let body = if self.eat(K::Colon).is_some() {
            let name = key.span.text(self.src).to_string();
            TokenBody::Value(self.missing_value(&name).unwrap_or_else(|| self.value()))
        } else if self.at(K::LBrace) {
            TokenBody::Group(self.token_block("the token group"))
        } else {
            self.expected("`:` and a value, or `{` and a group");
            TokenBody::Value(self.error_expr())
        };
        let start = start.min(key.span.start);
        TokenEntry {
            override_,
            key,
            body,
            span: self.finish(start),
        }
    }

    fn token_segment(&mut self) -> Option<Ident> {
        let t = self.cur();
        match t.kind {
            K::Ident => Some(self.ident("a token name")),
            K::Number => {
                self.bump();
                let text = self.text(t);
                if !text.bytes().all(|b| b.is_ascii_digit()) {
                    self.push_error(
                        Diagnostic::error(
                            "syntax::expected",
                            format!("`{text}` is not a token name"),
                        )
                        .with_label(t.span, "token names are words or whole numbers"),
                    );
                }
                Some(Ident {
                    name: text.to_string(),
                    span: t.span,
                })
            }
            _ => {
                self.expected("a token name");
                None
            }
        }
    }

    fn token_key(&mut self) -> TokenKey {
        let start = self.cur().span.start;
        let mut segments = Vec::new();
        let dollar = self.at(K::Dollar);
        if dollar {
            let t = self.bump();
            segments.push(Ident {
                name: self.text(t)[1..].to_string(),
                span: Span::new(t.span.start + 1, t.span.end),
            });
        } else if let Some(seg) = self.token_segment() {
            segments.push(seg);
        }
        while !segments.is_empty() && self.at(K::Dot) {
            self.bump();
            match self.token_segment() {
                Some(seg) => segments.push(seg),
                None => break,
            }
        }
        TokenKey {
            dollar,
            segments,
            span: self.finish(start),
        }
    }

    fn use_decl(&mut self) -> ItemKind {
        self.bump();
        let mut clauses = Vec::new();
        loop {
            let start = self.cur().span.start;
            if !(self.at_kw("tokens") || self.at_kw("palette")) {
                let help = (self.at(K::Ident))
                    .then(|| did_you_mean(self.text(self.cur()), ["tokens", "palette"]))
                    .flatten();
                self.expected_with("`tokens` or `palette`", help);
                if !(self.at(K::Ident) && self.same_line()) {
                    break;
                }
            }
            let kind = self.ident("`tokens` or `palette`");
            let value = self.expr();
            let start = start.min(kind.span.start);
            clauses.push(UseClause {
                kind,
                value,
                span: self.finish(start),
            });
            if self.eat(K::Comma).is_none() {
                break;
            }
        }
        ItemKind::Use(Use { clauses })
    }

    fn service(&mut self) -> ItemKind {
        self.bump();
        let name = self.ident("a service name");
        self.expect_kw("from");
        let start = self.cur().span.start;
        let kind = self.ident("a source: dbus, file, listen or poll");
        if !kind.name.is_empty() && !SOURCES.contains(&kind.name.as_str()) {
            let mut d = Diagnostic::error(
                "syntax::unknown_source",
                format!("unknown service source `{}`", kind.name),
            )
            .with_label(kind.span, "sources are dbus, file, listen and poll");
            if let Some(h) = did_you_mean(&kind.name, SOURCES.iter().copied()) {
                d = d.with_help(h);
            }
            self.push_error(d);
        }
        let mut args = Vec::new();
        let mut every_typo = false;
        while self.same_line() && !self.at_kw("every") && self.starts_expr() {
            if kind.name == "poll" && !args.is_empty() && self.clause_typo(&["every"]).is_some() {
                every_typo = true;
                break;
            }
            args.push(self.expr());
        }
        let every = (every_typo || self.same_line() && self.at_kw("every")).then(|| {
            if !every_typo {
                self.bump();
            }
            self.expr()
        });
        let start = start.min(kind.span.start);
        let source = ServiceSource {
            kind,
            args,
            every,
            span: self.finish(start),
        };
        self.check_source(&source);
        ItemKind::Service(Service {
            name,
            source,
            body: self.block("the service fields", |p| p.item(Ctx::Service), error_item),
        })
    }

    fn check_source(&mut self, src: &ServiceSource) {
        let end = Span::at(src.span.end);
        match src.kind.name.as_str() {
            "dbus" => {
                let bus_ok = match src.args.first().map(|e| &e.kind) {
                    Some(ExprKind::Name(id)) if id.name == "system" || id.name == "session" => true,
                    Some(ExprKind::Name(id)) => {
                        let mut d = Diagnostic::error(
                            "syntax::unknown_source",
                            format!("unknown bus `{}`", id.name),
                        )
                        .with_label(id.span, "the bus is `system` or `session`");
                        if let Some(h) = did_you_mean(&id.name, ["system", "session"]) {
                            d = d.with_help(h);
                        }
                        self.push_error(d);
                        true
                    }
                    _ => false,
                };
                let name_ok = matches!(src.args.get(1).map(|e| &e.kind), Some(ExprKind::String(_)));
                if !bus_ok || !name_ok {
                    self.push_error(
                        Diagnostic::error(
                            "syntax::expected",
                            "a dbus source needs a bus and a name",
                        )
                        .with_label(
                            src.span,
                            "such as `dbus system \"net.hadess.PowerProfiles\"`",
                        ),
                    );
                }
            }
            "file" | "listen" if src.args.len() != 1 => {
                self.push_error(
                    Diagnostic::error(
                        "syntax::expected",
                        format!("a `{}` source takes one argument", src.kind.name),
                    )
                    .with_label(
                        if src.args.is_empty() { end } else { src.span },
                        "expected one path or command",
                    ),
                );
            }
            "poll" if src.args.len() != 1 || src.every.is_none() => {
                self.push_error(
                    Diagnostic::error(
                        "syntax::expected",
                        "a `poll` source takes a command and `every` interval",
                    )
                    .with_label(src.span, "such as `poll [\"sensors\", \"-j\"] every 5s`"),
                );
            }
            _ => {}
        }
    }

    fn permit(&mut self) -> ItemKind {
        self.bump();
        let typo = self.report_clause_typo(&["exec"]);
        let mut capability = self.ident("a capability such as `exec`");
        if let Some(kw) = typo {
            capability.name = kw.to_string();
        }
        let mut args = Vec::new();
        if self.same_line() && self.starts_expr() {
            args.push(self.expr());
            while self.eat(K::Comma).is_some() {
                args.push(self.expr());
            }
        }
        ItemKind::Permit(Permit { capability, args })
    }

    fn keyframes(&mut self) -> ItemKind {
        self.bump();
        let name = self.ident("a keyframes name");
        ItemKind::Keyframes(Keyframes {
            name,
            body: self.block("the keyframes", |p| p.item(Ctx::Keyframes), error_item),
        })
    }

    // -----------------------------------------------------------------------
    // Handler statements

    pub(crate) fn stmt(&mut self) -> Stmt {
        let start = self.cur().span.start;
        let kind = self.stmt_kind();
        Stmt {
            kind,
            span: self.finish(start),
        }
    }

    fn stmt_kind(&mut self) -> StmtKind {
        if self.at(K::Ident) && self.nth_kind(1) != K::Colon {
            match self.text(self.cur()) {
                "let" => return StmtKind::Let(self.let_decl(None)),
                "if" => {
                    return StmtKind::If(
                        self.if_(&mut |p: &mut Self| p.stmt_block("the `if` body")),
                    );
                }
                "for" => {
                    return StmtKind::For(
                        self.for_(&mut |p: &mut Self| p.stmt_block("the `for` body")),
                    );
                }
                "match" => {
                    return StmtKind::Match(self.match_(&mut |p: &mut Self| p.stmt_arm_body()));
                }
                "play" => {
                    self.bump();
                    return StmtKind::Play(self.expr());
                }
                "else" => {
                    let t = self.bump();
                    self.push_error(
                        Diagnostic::error("syntax::misplaced", "`else` without `if`")
                            .with_label(t.span, "no `if` before this"),
                    );
                    let _ = self.stmt_block("the `else` body");
                    return StmtKind::Error;
                }
                _ => {}
            }
        }
        if !self.starts_expr() {
            // A stray `{` is reported by the block loop, which skips it whole.
            if !self.at(K::LBrace) {
                self.expected_at_token("a statement");
            }
            return StmtKind::Error;
        }
        let target = self.expr();
        let op = match self.kind() {
            K::Eq => Some(AssignOp::Set),
            K::PlusEq => Some(AssignOp::Add),
            K::MinusEq => Some(AssignOp::Sub),
            K::StarEq => Some(AssignOp::Mul),
            K::SlashEq => Some(AssignOp::Div),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let value = self.expr();
            return StmtKind::Assign { target, op, value };
        }
        if !self.at_item_end()
            && let ExprKind::Name(id) = &target.kind
        {
            let kws = STMT_KEYWORDS.iter().copied().chain(["else"]);
            if let Some(h) = did_you_mean(&id.name, kws) {
                self.expected_with("`;` or a line break after this statement", Some(h));
            }
        }
        StmtKind::Expr(target)
    }
}
