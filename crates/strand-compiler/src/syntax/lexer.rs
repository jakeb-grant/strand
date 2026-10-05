//! Lossless lexer: every byte belongs to exactly one token.
//!
//! See `docs/grammar.md`, "Lexical structure".

use crate::diagnostic::{Diagnostic, did_you_mean};

use super::Span;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TokenKind {
    // Trivia.
    Whitespace,
    Newline,
    Comment,

    // Atoms.
    Ident,
    /// `$name`: a design-token reference.
    Dollar,
    /// `@name`: an attribute.
    At,
    /// `#word`: a hex colour or an SVG selector.
    Hash,
    /// Digits with an optional fraction and unit.
    Number,
    String,

    // Delimiters.
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Semi,
    Colon,
    Dot,

    // Operators.
    QuestionDot,
    QuestionQuestion,
    Question,
    FatArrow,
    ThinArrow,
    TwoWay,
    Tilde,
    Eq,
    EqEq,
    BangEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    AndAnd,
    OrOr,
    Bang,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,

    /// Characters that form no token; always reported.
    Unknown,
    /// End of input; zero-width, emitted once by the parser's token cursor.
    Eof,
}

impl TokenKind {
    pub fn is_trivia(self) -> bool {
        matches!(self, Self::Whitespace | Self::Newline | Self::Comment)
    }

    /// How the token reads in "expected X, found Y" messages.
    pub fn describe(self) -> &'static str {
        use TokenKind::*;
        match self {
            Whitespace => "whitespace",
            Newline => "a line break",
            Comment => "a comment",
            Ident => "an identifier",
            Dollar => "a token reference",
            At => "an attribute",
            Hash => "a `#` word",
            Number => "a number",
            String => "a string",
            LBrace => "`{`",
            RBrace => "`}`",
            LParen => "`(`",
            RParen => "`)`",
            LBracket => "`[`",
            RBracket => "`]`",
            Comma => "`,`",
            Semi => "`;`",
            Colon => "`:`",
            Dot => "`.`",
            QuestionDot => "`?.`",
            QuestionQuestion => "`??`",
            Question => "`?`",
            FatArrow => "`=>`",
            ThinArrow => "`->`",
            TwoWay => "`<->`",
            Tilde => "`~`",
            Eq => "`=`",
            EqEq => "`==`",
            BangEq => "`!=`",
            Lt => "`<`",
            LtEq => "`<=`",
            Gt => "`>`",
            GtEq => "`>=`",
            AndAnd => "`&&`",
            OrOr => "`||`",
            Bang => "`!`",
            Plus => "`+`",
            Minus => "`-`",
            Star => "`*`",
            Slash => "`/`",
            Percent => "`%`",
            PlusEq => "`+=`",
            MinusEq => "`-=`",
            StarEq => "`*=`",
            SlashEq => "`/=`",
            Unknown => "an invalid character",
            Eof => "end of file",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

/// Units a number may carry (`docs/grammar.md`, "Numbers and units").
pub const UNITS: &[&str] = &["px", "%", "deg", "ch", "s", "ms"];

/// Splits a lexed number into its numeric text and unit suffix.
pub fn split_number(text: &str) -> (&str, &str) {
    let end = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    text.split_at(end)
}

/// Lexes `src` into tokens covering every byte, plus lexical diagnostics.
pub fn lex(src: &str) -> (Vec<Token>, Vec<Diagnostic>) {
    let mut lx = Lexer {
        src,
        bytes: src.as_bytes(),
        pos: 0,
        tokens: Vec::new(),
        diags: Vec::new(),
    };
    lx.run();
    (lx.tokens, lx.diags)
}

struct Lexer<'s> {
    src: &'s str,
    bytes: &'s [u8],
    pos: usize,
    tokens: Vec<Token>,
    diags: Vec<Diagnostic>,
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn span(start: usize, end: usize) -> Span {
    Span::new(
        u32::try_from(start).unwrap_or(u32::MAX),
        u32::try_from(end).unwrap_or(u32::MAX),
    )
}

impl Lexer<'_> {
    fn peek_at(&self, n: usize) -> u8 {
        self.bytes.get(self.pos + n).copied().unwrap_or(0)
    }

    fn push(&mut self, kind: TokenKind, start: usize) {
        self.tokens.push(Token {
            kind,
            span: span(start, self.pos),
        });
    }

    fn error(
        &mut self,
        code: &'static str,
        msg: impl Into<String>,
        start: usize,
        end: usize,
        label: &str,
    ) {
        self.diags
            .push(Diagnostic::error(code, msg).with_label(span(start, end), label));
    }

    /// Whether the previous token is a `.` touching the current position:
    /// then a number is a path segment and takes no fraction (`$space.2`).
    fn after_touching_dot(&self) -> bool {
        self.tokens
            .last()
            .is_some_and(|t| t.kind == TokenKind::Dot && t.span.end as usize == self.pos)
    }

    fn eat_ident_tail(&mut self) {
        while is_ident_continue(self.peek_at(0)) {
            self.pos += 1;
        }
    }

    /// At `\n`, or at a `\r` not followed by `\n` (a `\r\n` pair is
    /// whitespace `\r`, then the line break `\n`).
    fn at_line_break(&self) -> bool {
        match self.peek_at(0) {
            b'\n' => true,
            b'\r' => self.peek_at(1) != b'\n',
            _ => false,
        }
    }

    fn run(&mut self) {
        // A leading byte-order mark (some editors write one) is trivia.
        if self.src.starts_with('\u{feff}') {
            self.pos = '\u{feff}'.len_utf8();
            self.push(TokenKind::Whitespace, 0);
        }
        while self.pos < self.bytes.len() {
            let start = self.pos;
            let b = self.bytes[self.pos];
            match b {
                // A lone `\r` (old Mac line endings) is a line break too, as
                // it is for editors and the diagnostic renderer.
                _ if self.at_line_break() => {
                    self.pos += 1;
                    self.push(TokenKind::Newline, start);
                }
                b' ' | b'\t' | b'\r' => {
                    while matches!(self.peek_at(0), b' ' | b'\t' | b'\r') && !self.at_line_break() {
                        self.pos += 1;
                    }
                    self.push(TokenKind::Whitespace, start);
                }
                b'/' if self.peek_at(1) == b'/' => {
                    while self.pos < self.bytes.len() && !self.at_line_break() {
                        self.pos += 1;
                    }
                    self.push(TokenKind::Comment, start);
                }
                b'"' => self.string(start),
                b'0'..=b'9' => self.number(start),
                b'$' | b'@' => {
                    self.pos += 1;
                    let kind = if b == b'$' {
                        TokenKind::Dollar
                    } else {
                        TokenKind::At
                    };
                    if is_ident_start(self.peek_at(0)) {
                        self.eat_ident_tail();
                        self.push(kind, start);
                    } else {
                        self.push(TokenKind::Unknown, start);
                        let what = if b == b'$' {
                            "expected a token name after `$`"
                        } else {
                            "expected an attribute name after `@`"
                        };
                        self.error(
                            "syntax::invalid_char",
                            what,
                            start,
                            self.pos,
                            "needs a name",
                        );
                    }
                }
                b'#' => {
                    self.pos += 1;
                    let body = self.pos;
                    loop {
                        self.eat_ident_tail();
                        if self.peek_at(0) == b'-'
                            && is_ident_continue(self.peek_at(1))
                            && self.pos > body
                        {
                            self.pos += 1;
                        } else {
                            break;
                        }
                    }
                    if self.pos == body {
                        self.push(TokenKind::Unknown, start);
                        self.error(
                            "syntax::invalid_char",
                            "expected a hex colour or selector after `#`",
                            start,
                            self.pos,
                            "such as `#7aa2f7`",
                        );
                    } else {
                        self.push(TokenKind::Hash, start);
                    }
                }
                _ if is_ident_start(b) => {
                    self.eat_ident_tail();
                    self.push(TokenKind::Ident, start);
                }
                _ if b.is_ascii() => self.punct(start, b),
                _ => {
                    // One non-ASCII character outside a string.
                    let ch = self.src[self.pos..].chars().next().unwrap_or('\u{fffd}');
                    self.pos += ch.len_utf8().max(1);
                    self.push(TokenKind::Unknown, start);
                    self.error(
                        "syntax::invalid_char",
                        format!("unexpected character `{ch}`"),
                        start,
                        self.pos,
                        "not valid outside a string",
                    );
                }
            }
        }
    }

    fn string(&mut self, start: usize) {
        self.pos += 1;
        loop {
            if self.pos >= self.bytes.len() || self.at_line_break() {
                self.error(
                    "syntax::unterminated_string",
                    "unterminated string",
                    start,
                    self.pos,
                    "this string has no closing `\"` on its line",
                );
                break;
            }
            match self.peek_at(0) {
                b'"' => {
                    self.pos += 1;
                    break;
                }
                b'\\' => {
                    let esc = self.pos;
                    self.pos += 1;
                    match self.peek_at(0) {
                        b'"' | b'\\' | b'n' | b't' | b'r' | b'0' => self.pos += 1,
                        b'u' if self.peek_at(1) == b'{' => {
                            self.pos += 2;
                            let digits = self.pos;
                            while self.peek_at(0).is_ascii_hexdigit() {
                                self.pos += 1;
                            }
                            let ok = self.pos > digits
                                && self.pos - digits <= 6
                                && self.peek_at(0) == b'}';
                            if self.peek_at(0) == b'}' {
                                self.pos += 1;
                            }
                            let valid = ok
                                && u32::from_str_radix(&self.src[digits..self.pos - 1], 16)
                                    .ok()
                                    .and_then(char::from_u32)
                                    .is_some();
                            if !valid {
                                self.error(
                                    "syntax::bad_escape",
                                    "invalid unicode escape",
                                    esc,
                                    self.pos,
                                    "expected `\\u{...}` with 1 to 6 hex digits",
                                );
                            }
                        }
                        _ => {
                            let len = self.src[self.pos..].chars().next().map_or(0, |c| {
                                if matches!(c, '\n' | '\r') {
                                    0
                                } else {
                                    c.len_utf8()
                                }
                            });
                            self.pos += len;
                            self.error(
                                "syntax::bad_escape",
                                "unknown escape sequence",
                                esc,
                                self.pos,
                                "valid escapes are \\\" \\\\ \\n \\t \\r \\0 \\u{...}",
                            );
                        }
                    }
                }
                _ => {
                    let len = self.src[self.pos..]
                        .chars()
                        .next()
                        .map_or(1, char::len_utf8);
                    self.pos += len;
                }
            }
        }
        self.push(TokenKind::String, start);
    }

    fn number(&mut self, start: usize) {
        let segment = self.after_touching_dot();
        while self.peek_at(0).is_ascii_digit() {
            self.pos += 1;
        }
        if !segment && self.peek_at(0) == b'.' && self.peek_at(1).is_ascii_digit() {
            self.pos += 1;
            while self.peek_at(0).is_ascii_digit() {
                self.pos += 1;
            }
        }
        let unit_start = self.pos;
        if self.peek_at(0) == b'%' {
            self.pos += 1;
        } else if is_ident_start(self.peek_at(0)) {
            self.eat_ident_tail();
            let unit = &self.src[unit_start..self.pos];
            if !UNITS.contains(&unit) {
                let mut d =
                    Diagnostic::error("syntax::unknown_unit", format!("unknown unit `{unit}`"))
                        .with_label(
                            span(unit_start, self.pos),
                            "units are px, %, deg, ch, s and ms",
                        );
                if let Some(help) = did_you_mean(unit, UNITS.iter().copied()) {
                    d = d.with_help(help);
                }
                self.diags.push(d);
            }
        }
        self.push(TokenKind::Number, start);
    }

    fn punct(&mut self, start: usize, b: u8) {
        use TokenKind::*;
        let n1 = self.peek_at(1);
        let n2 = self.peek_at(2);
        let (kind, len) = match b {
            b'{' => (LBrace, 1),
            b'}' => (RBrace, 1),
            b'(' => (LParen, 1),
            b')' => (RParen, 1),
            b'[' => (LBracket, 1),
            b']' => (RBracket, 1),
            b',' => (Comma, 1),
            b';' => (Semi, 1),
            b':' => (Colon, 1),
            b'.' => (Dot, 1),
            b'~' => (Tilde, 1),
            b'?' if n1 == b'?' => (QuestionQuestion, 2),
            b'?' if n1 == b'.' && !n2.is_ascii_digit() => (QuestionDot, 2),
            b'?' => (Question, 1),
            b'=' if n1 == b'>' => (FatArrow, 2),
            b'=' if n1 == b'=' => (EqEq, 2),
            b'=' => (Eq, 1),
            b'!' if n1 == b'=' => (BangEq, 2),
            b'!' => (Bang, 1),
            b'<' if n1 == b'-' && n2 == b'>' => (TwoWay, 3),
            b'<' if n1 == b'=' => (LtEq, 2),
            b'<' => (Lt, 1),
            b'>' if n1 == b'=' => (GtEq, 2),
            b'>' => (Gt, 1),
            b'&' if n1 == b'&' => (AndAnd, 2),
            b'|' if n1 == b'|' => (OrOr, 2),
            b'-' if n1 == b'>' => (ThinArrow, 2),
            b'-' if n1 == b'=' => (MinusEq, 2),
            b'-' => (Minus, 1),
            b'+' if n1 == b'=' => (PlusEq, 2),
            b'+' => (Plus, 1),
            b'*' if n1 == b'=' => (StarEq, 2),
            b'*' => (Star, 1),
            b'/' if n1 == b'=' => (SlashEq, 2),
            b'/' => (Slash, 1),
            b'%' => (Percent, 1),
            _ => (Unknown, 1),
        };
        self.pos += len;
        self.push(kind, start);
        if kind == Unknown {
            let ch = b as char;
            let mut d = Diagnostic::error(
                "syntax::invalid_char",
                format!("unexpected character `{ch}`"),
            )
            .with_label(span(start, self.pos), "not part of the language");
            match b {
                b'&' => d = d.with_help("use `&&` for logical and"),
                b'|' => {
                    d = d.with_help("use `||` for logical or; alternatives are separate values")
                }
                b'\'' => d = d.with_help("strings use double quotes: \"...\""),
                _ => {}
            }
            self.diags.push(d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<(TokenKind, &str)> {
        let (toks, _) = lex(src);
        toks.iter()
            .filter(|t| !t.kind.is_trivia())
            .map(|t| (t.kind, t.span.text(src)))
            .collect()
    }

    #[test]
    fn lossless() {
        let src =
            "bar Top { edge: top; // hi\n  margin: $space.2, 8px\r\n}\n\"‹\" ¤ #7aa2f7 @reset";
        let (toks, _) = lex(src);
        let joined: String = toks.iter().map(|t| t.span.text(src)).collect();
        assert_eq!(joined, src);
    }

    #[test]
    fn leading_bom_is_trivia() {
        let src = "\u{feff}state x = 1\n";
        let (toks, diags) = lex(src);
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(toks[0].kind, TokenKind::Whitespace);
        assert_eq!(toks[0].span, Span::new(0, 3));
        let joined: String = toks.iter().map(|t| t.span.text(src)).collect();
        assert_eq!(joined, src);
        // Only at the start: elsewhere it is still an invalid character.
        assert!(!lex("a \u{feff}").1.is_empty());
    }

    #[test]
    fn numbers_and_units() {
        use TokenKind::*;
        assert_eq!(
            kinds("0.72 1.2s 200ms 40% 4ch 270deg 13px a % b"),
            vec![
                (Number, "0.72"),
                (Number, "1.2s"),
                (Number, "200ms"),
                (Number, "40%"),
                (Number, "4ch"),
                (Number, "270deg"),
                (Number, "13px"),
                (Ident, "a"),
                (Percent, "%"),
                (Ident, "b"),
            ]
        );
    }

    #[test]
    fn path_segments_take_no_fraction() {
        use TokenKind::*;
        assert_eq!(
            kinds("$space.2.5 1.max"),
            vec![
                (Dollar, "$space"),
                (Dot, "."),
                (Number, "2"),
                (Dot, "."),
                (Number, "5"),
                (Number, "1"),
                (Dot, "."),
                (Ident, "max"),
            ]
        );
    }

    #[test]
    fn operators_longest_match() {
        use TokenKind::*;
        assert_eq!(
            kinds("<-> ?. ?? ? => -> <= a<-b"),
            vec![
                (TwoWay, "<->"),
                (QuestionDot, "?."),
                (QuestionQuestion, "??"),
                (Question, "?"),
                (FatArrow, "=>"),
                (ThinArrow, "->"),
                (LtEq, "<="),
                (Ident, "a"),
                (Lt, "<"),
                (Minus, "-"),
                (Ident, "b"),
            ]
        );
    }

    #[test]
    fn hash_words() {
        use TokenKind::*;
        assert_eq!(
            kinds("#7aa2f7 #needle #a-b-c"),
            vec![(Hash, "#7aa2f7"), (Hash, "#needle"), (Hash, "#a-b-c")]
        );
    }

    #[test]
    fn errors_with_suggestions() {
        let (_, diags) = lex("12pz");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].help.as_deref(), Some("did you mean `px`?"));
        let (_, diags) = lex("\"open");
        assert_eq!(diags[0].code, "syntax::unterminated_string");
        let (_, diags) = lex("a | b");
        assert!(diags[0].help.as_deref().unwrap_or("").contains("||"));
    }
}
