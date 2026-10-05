//! Parser for the schema declaration language (see the module docs of
//! [`super`]).
//!
//! Two passes: the text is parsed into raw declarations, every declared
//! name is registered, and then types are resolved, so declarations may
//! refer to ones further down.

use std::sync::Arc;

use super::{ElementFlags, ElementSchema, PropSchema, Schema, SchemaError, TokenSchema};
use crate::ty::{EnumDef, EventDef, FieldDef, FnSig, MethodDef, Origin, ParamSig, RecordDef, Ty};

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Word(String),
    Punct(&'static str),
    /// A number or string, used only as a parameter default.
    Lit,
}

struct Lexer;

impl Lexer {
    fn lex(text: &str) -> Vec<(Tok, u32)> {
        let mut out = Vec::new();
        let mut line = 1u32;
        let b = text.as_bytes();
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            match c {
                b'\n' => {
                    line += 1;
                    i += 1;
                }
                b' ' | b'\t' | b'\r' | b';' => i += 1,
                b'/' if b.get(i + 1) == Some(&b'/') => {
                    while i < b.len() && b[i] != b'\n' {
                        i += 1;
                    }
                }
                b'"' => {
                    i += 1;
                    while i < b.len() && b[i] != b'"' && b[i] != b'\n' {
                        i += 1;
                    }
                    i += 1;
                    out.push((Tok::Lit, line));
                }
                b'0'..=b'9' | b'#' => {
                    // Numbers (also token keys like `1`) and colours.
                    let start = i;
                    i += 1;
                    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                        i += 1;
                    }
                    let s = &text[start..i];
                    if s.bytes().all(|c| c.is_ascii_digit()) {
                        out.push((Tok::Word(s.to_string()), line));
                    } else {
                        out.push((Tok::Lit, line));
                    }
                }
                c if c.is_ascii_alphabetic() || c == b'_' => {
                    let start = i;
                    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                        i += 1;
                    }
                    out.push((Tok::Word(text[start..i].to_string()), line));
                }
                _ => {
                    const PUNCT: [&str; 17] = [
                        "...", "<->", "->", "{", "}", "(", ")", "[", "]", "<", ">", ",", ":", "?",
                        "|", "=", ".",
                    ];
                    let rest = &text[i..];
                    match PUNCT.iter().find(|p| rest.starts_with(**p)) {
                        Some(p) => {
                            out.push((Tok::Punct(p), line));
                            i += p.len();
                        }
                        None => {
                            out.push((Tok::Punct("?invalid"), line));
                            i += rest.chars().next().map_or(1, char::len_utf8);
                        }
                    }
                }
            }
        }
        out
    }
}

#[derive(Clone, Debug)]
enum RawType {
    Name(String, Vec<RawType>),
    List(Box<RawType>),
    Opt(Box<RawType>),
    Tuple(Vec<RawType>),
    Union(Vec<RawType>),
    Fn(Vec<RawType>, Option<Box<RawType>>),
}

#[derive(Clone, Debug)]
struct RawParam {
    name: String,
    ty: RawType,
    default: bool,
    variadic: bool,
}

#[derive(Clone, Debug)]
struct RawSig {
    name: String,
    params: Vec<RawParam>,
    ret: Option<RawType>,
    action: bool,
    lift: bool,
    line: u32,
}

#[derive(Clone, Debug)]
enum RawMember {
    Field {
        name: String,
        ty: RawType,
        rw: bool,
        line: u32,
    },
    Method(RawSig),
    Event {
        name: String,
        params: Vec<RawParam>,
        line: u32,
    },
}

#[derive(Clone, Debug)]
enum RawElMember {
    Prop {
        name: String,
        ty: RawType,
        two_way: bool,
        inherit: bool,
        sub: Vec<RawElMember>,
        line: u32,
    },
    Event {
        name: String,
        params: Vec<RawParam>,
        line: u32,
    },
    Scope {
        name: String,
        ty: RawType,
        line: u32,
    },
    Flags(Vec<String>, u32),
}

#[derive(Clone, Debug)]
enum RawItem {
    Enum {
        name: String,
        variants: Vec<String>,
    },
    Opaque(Vec<String>),
    Alias {
        name: String,
        ty: RawType,
    },
    Record {
        name: String,
        key: Option<Vec<String>>,
        members: Vec<RawMember>,
        service: bool,
    },
    Fn(RawSig),
    Value {
        name: String,
        ty: RawType,
    },
    Methods {
        ty_name: String,
        sigs: Vec<RawSig>,
    },
    Element {
        group: bool,
        name: String,
        arg: Option<RawType>,
        includes: Vec<String>,
        members: Vec<RawElMember>,
    },
    Palette(Vec<String>),
    Tokens(Vec<(String, RawType, u32)>),
}

struct Parser {
    toks: Vec<(Tok, u32)>,
    pos: usize,
    errors: Vec<SchemaError>,
}

type PResult<T> = Result<T, SchemaError>;

impl Parser {
    fn line(&self) -> u32 {
        self.toks
            .get(self.pos)
            .or(self.toks.last())
            .map_or(1, |t| t.1)
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|t| &t.0)
    }

    fn peek_word(&self) -> Option<&str> {
        match self.peek() {
            Some(Tok::Word(w)) => Some(w),
            _ => None,
        }
    }

    fn at(&self, p: &str) -> bool {
        matches!(self.peek(), Some(Tok::Punct(q)) if *q == p)
    }

    fn at_word(&self, w: &str) -> bool {
        self.peek_word() == Some(w)
    }

    fn err<T>(&self, message: impl Into<String>) -> PResult<T> {
        Err(SchemaError {
            line: self.line(),
            message: message.into(),
        })
    }

    fn eat(&mut self, p: &str) -> bool {
        if self.at(p) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn eat_word(&mut self, w: &str) -> bool {
        if self.at_word(w) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, p: &str) -> PResult<()> {
        if self.eat(p) {
            Ok(())
        } else {
            self.err(format!("expected `{p}`, found {:?}", self.peek()))
        }
    }

    fn word(&mut self) -> PResult<String> {
        match self.peek() {
            Some(Tok::Word(w)) => {
                let w = w.clone();
                self.pos += 1;
                Ok(w)
            }
            other => self.err(format!("expected a name, found {other:?}")),
        }
    }

    /// Skips to the start of the next top-level item.
    fn recover(&mut self) {
        let mut depth = 0i32;
        while let Some(t) = self.peek() {
            match t {
                Tok::Punct("{") => depth += 1,
                Tok::Punct("}") => {
                    depth -= 1;
                    if depth <= 0 {
                        self.pos += 1;
                        return;
                    }
                }
                _ => {}
            }
            self.pos += 1;
        }
    }

    fn items(&mut self) -> Vec<(RawItem, u32)> {
        let mut items = Vec::new();
        while self.peek().is_some() {
            let line = self.line();
            match self.item() {
                Ok(item) => items.push((item, line)),
                Err(e) => {
                    self.errors.push(e);
                    self.recover();
                }
            }
        }
        items
    }

    fn item(&mut self) -> PResult<RawItem> {
        let kw = self.word()?;
        Ok(match kw.as_str() {
            "enum" => {
                let name = self.word()?;
                self.expect("{")?;
                let mut variants = Vec::new();
                while !self.eat("}") {
                    variants.push(self.word()?);
                    self.eat(",");
                }
                RawItem::Enum { name, variants }
            }
            "opaque" => {
                let mut names = vec![self.word()?];
                while self.eat(",") {
                    names.push(self.word()?);
                }
                RawItem::Opaque(names)
            }
            "alias" => {
                let name = self.word()?;
                self.expect("=")?;
                RawItem::Alias {
                    name,
                    ty: self.ty()?,
                }
            }
            "record" | "service" => {
                let name = self.word()?;
                let key = if self.eat_word("key") {
                    let mut path = vec![self.word()?];
                    while self.eat(".") {
                        path.push(self.word()?);
                    }
                    Some(path)
                } else {
                    None
                };
                self.expect("{")?;
                let mut members = Vec::new();
                while !self.eat("}") {
                    members.push(self.member()?);
                }
                RawItem::Record {
                    name,
                    key,
                    members,
                    service: kw == "service",
                }
            }
            "fn" => RawItem::Fn(self.sig(false)?),
            "action" => RawItem::Fn(self.sig(true)?),
            "value" => {
                let name = self.word()?;
                self.expect(":")?;
                RawItem::Value {
                    name,
                    ty: self.ty()?,
                }
            }
            "methods" => {
                let ty_name = self.word()?;
                self.expect("{")?;
                let mut sigs = Vec::new();
                while !self.eat("}") {
                    let action = match self.word()?.as_str() {
                        "fn" => false,
                        "action" => true,
                        other => return self.err(format!("expected `fn`, found `{other}`")),
                    };
                    sigs.push(self.sig(action)?);
                }
                RawItem::Methods { ty_name, sigs }
            }
            "group" | "element" => {
                let name = self.word()?;
                let arg = if self.eat("(") {
                    let t = self.ty()?;
                    self.expect(")")?;
                    Some(t)
                } else {
                    None
                };
                let mut includes = Vec::new();
                if self.eat(":") {
                    includes.push(self.word()?);
                    while self.eat(",") {
                        includes.push(self.word()?);
                    }
                }
                self.expect("{")?;
                let members = self.el_members()?;
                RawItem::Element {
                    group: kw == "group",
                    name,
                    arg,
                    includes,
                    members,
                }
            }
            "palette" => {
                self.expect("{")?;
                let mut names = Vec::new();
                while !self.eat("}") {
                    names.push(self.word()?);
                    self.eat(",");
                }
                RawItem::Palette(names)
            }
            "tokens" => {
                self.expect("{")?;
                let mut out = Vec::new();
                self.token_entries("", &mut out)?;
                RawItem::Tokens(out)
            }
            other => return self.err(format!("unknown declaration `{other}`")),
        })
    }

    fn token_entries(
        &mut self,
        prefix: &str,
        out: &mut Vec<(String, RawType, u32)>,
    ) -> PResult<()> {
        while !self.eat("}") {
            let line = self.line();
            let mut key = self.word()?;
            while self.eat(".") {
                key.push('.');
                key.push_str(&self.word()?);
            }
            let path = if prefix.is_empty() {
                key
            } else {
                format!("{prefix}.{key}")
            };
            if self.eat("{") {
                self.token_entries(&path, out)?;
            } else {
                self.expect(":")?;
                out.push((path, self.ty()?, line));
            }
        }
        Ok(())
    }

    fn member(&mut self) -> PResult<RawMember> {
        let line = self.line();
        let name = self.word()?;
        match name.as_str() {
            "fn" if !self.at(":") => return Ok(RawMember::Method(self.sig(false)?)),
            "action" if !self.at(":") => return Ok(RawMember::Method(self.sig(true)?)),
            "event" if !self.at(":") => {
                let name = self.word()?;
                let params = if self.at("(") {
                    self.params()?
                } else {
                    Vec::new()
                };
                return Ok(RawMember::Event { name, params, line });
            }
            _ => {}
        }
        self.expect(":")?;
        let ty = self.ty()?;
        let rw = self.eat_word("rw");
        Ok(RawMember::Field { name, ty, rw, line })
    }

    fn el_members(&mut self) -> PResult<Vec<RawElMember>> {
        let mut members = Vec::new();
        while !self.eat("}") {
            let line = self.line();
            let name = self.word()?;
            if !self.at(":") {
                match name.as_str() {
                    "on" => {
                        let name = self.word()?;
                        let params = if self.at("(") {
                            self.params()?
                        } else {
                            Vec::new()
                        };
                        members.push(RawElMember::Event { name, params, line });
                        continue;
                    }
                    "let" => {
                        let name = self.word()?;
                        self.expect(":")?;
                        let ty = self.ty()?;
                        members.push(RawElMember::Scope { name, ty, line });
                        continue;
                    }
                    "flags" => {
                        let mut flags = Vec::new();
                        while self.line() == line
                            && let Some(w) = self.peek_word()
                        {
                            flags.push(w.to_string());
                            self.pos += 1;
                        }
                        members.push(RawElMember::Flags(flags, line));
                        continue;
                    }
                    _ => {}
                }
            }
            self.expect(":")?;
            let ty = self.ty()?;
            let mut two_way = false;
            let mut inherit = false;
            loop {
                if self.eat("<->") {
                    two_way = true;
                } else if self.eat_word("inherit") {
                    inherit = true;
                } else {
                    break;
                }
            }
            let sub = if self.eat("{") {
                self.el_members()?
            } else {
                Vec::new()
            };
            members.push(RawElMember::Prop {
                name,
                ty,
                two_way,
                inherit,
                sub,
                line,
            });
        }
        Ok(members)
    }

    fn sig(&mut self, action: bool) -> PResult<RawSig> {
        let line = self.line();
        let name = self.word()?;
        let params = self.params()?;
        let ret = if self.eat("->") {
            Some(self.ty()?)
        } else {
            None
        };
        let lift = self.eat_word("lift");
        Ok(RawSig {
            name,
            params,
            ret,
            action,
            lift,
            line,
        })
    }

    fn params(&mut self) -> PResult<Vec<RawParam>> {
        self.expect("(")?;
        let mut params = Vec::new();
        while !self.eat(")") {
            let variadic = self.eat("...");
            let name = self.word()?;
            self.expect(":")?;
            let ty = self.ty()?;
            let default = if self.eat("=") {
                // The default's value is documentation; only its presence
                // matters to the checker.
                match self.peek() {
                    Some(Tok::Word(_) | Tok::Lit) => self.pos += 1,
                    _ => return self.err("expected a default value"),
                }
                true
            } else {
                false
            };
            params.push(RawParam {
                name,
                ty,
                default,
                variadic,
            });
            if !self.eat(",") {
                self.expect(")")?;
                break;
            }
        }
        Ok(params)
    }

    fn ty(&mut self) -> PResult<RawType> {
        let first = self.ty_postfix()?;
        if !self.at("|") {
            return Ok(first);
        }
        let mut alts = vec![first];
        while self.eat("|") {
            alts.push(self.ty_postfix()?);
        }
        Ok(RawType::Union(alts))
    }

    fn ty_postfix(&mut self) -> PResult<RawType> {
        let mut t = self.ty_atom()?;
        while self.eat("?") {
            t = RawType::Opt(Box::new(t));
        }
        Ok(t)
    }

    fn ty_atom(&mut self) -> PResult<RawType> {
        if self.eat("[") {
            let t = self.ty()?;
            self.expect("]")?;
            return Ok(RawType::List(Box::new(t)));
        }
        if self.eat("(") {
            let mut parts = vec![self.ty()?];
            while self.eat(",") {
                parts.push(self.ty()?);
            }
            self.expect(")")?;
            return Ok(if parts.len() == 1 {
                parts.remove(0)
            } else {
                RawType::Tuple(parts)
            });
        }
        let name = self.word()?;
        if name == "fn" {
            self.expect("(")?;
            let mut params = Vec::new();
            while !self.eat(")") {
                params.push(self.ty()?);
                if !self.eat(",") {
                    self.expect(")")?;
                    break;
                }
            }
            let ret = if self.eat("->") {
                Some(Box::new(self.ty()?))
            } else {
                None
            };
            return Ok(RawType::Fn(params, ret));
        }
        let mut args = Vec::new();
        if self.eat("<") {
            args.push(self.ty()?);
            while self.eat(",") {
                args.push(self.ty()?);
            }
            self.expect(">")?;
        }
        Ok(RawType::Name(name, args))
    }
}

/// Resolves raw types against the schema.
fn resolve(schema: &Schema, t: &RawType, line: u32) -> Result<Ty, SchemaError> {
    let r = |t: &RawType| resolve(schema, t, line);
    Ok(match t {
        RawType::Name(n, args) if n == "Async" => match args.as_slice() {
            [a] => Ty::Async(Box::new(r(a)?)),
            _ => {
                return Err(SchemaError {
                    line,
                    message: "`Async` takes one type".into(),
                });
            }
        },
        RawType::Name(n, args) => {
            if !args.is_empty() {
                return Err(SchemaError {
                    line,
                    message: format!("`{n}` takes no type arguments"),
                });
            }
            schema.named_type(n).ok_or_else(|| SchemaError {
                line,
                message: format!("unknown type `{n}`"),
            })?
        }
        RawType::List(t) => Schema::list_of(&schema.types, r(t)?),
        RawType::Opt(t) => r(t)?.optional(),
        RawType::Tuple(ts) => Ty::Tuple(ts.iter().map(r).collect::<Result<_, _>>()?),
        RawType::Union(ts) => Ty::Union(ts.iter().map(r).collect::<Result<_, _>>()?),
        RawType::Fn(ps, ret) => Ty::Fn(Arc::new(FnSig::positional(
            ps.iter().map(r).collect::<Result<_, _>>()?,
            match ret {
                Some(t) => r(t)?,
                None => Ty::Unit,
            },
        ))),
    })
}

fn resolve_sig(schema: &Schema, s: &RawSig) -> Result<Arc<FnSig>, SchemaError> {
    let params = resolve_params(schema, &s.params, s.line)?;
    let ret = match &s.ret {
        Some(t) => resolve(schema, t, s.line)?,
        None => Ty::Unit,
    };
    Ok(Arc::new(FnSig {
        params,
        ret,
        action: s.action,
        lift: s.lift,
    }))
}

fn resolve_params(
    schema: &Schema,
    params: &[RawParam],
    line: u32,
) -> Result<Vec<ParamSig>, SchemaError> {
    params
        .iter()
        .map(|p| {
            Ok(ParamSig {
                name: p.name.clone(),
                ty: resolve(schema, &p.ty, line)?,
                has_default: p.default,
                variadic: p.variadic,
            })
        })
        .collect()
}

fn add_method(methods: &mut Vec<MethodDef>, name: &str, sig: Arc<FnSig>) {
    match methods.iter_mut().find(|m| m.name == name) {
        Some(m) => m.sigs.push(sig),
        None => methods.push(MethodDef {
            name: name.to_string(),
            sigs: vec![sig],
        }),
    }
}

pub(super) fn extend(schema: &mut Schema, text: &str) -> Result<(), Vec<SchemaError>> {
    let mut p = Parser {
        toks: Lexer::lex(text),
        pos: 0,
        errors: Vec::new(),
    };
    let items = p.items();
    let mut errors = std::mem::take(&mut p.errors);

    // Pass 1: names.
    for (item, line) in &items {
        let dup = |schema: &Schema, name: &str| {
            schema.types.find_record(name).is_some()
                || schema.types.find_enum(name).is_some()
                || schema.opaques.contains_key(name)
                || schema.aliases.contains_key(name)
        };
        let all_items = &items;
        match item {
            RawItem::Enum { name, variants } => {
                if dup(schema, name) || declared_later_as_opaque(all_items, name) {
                    errors.push(SchemaError {
                        line: *line,
                        message: format!("`{name}` is declared twice"),
                    });
                    continue;
                }
                schema.types.add_enum(EnumDef {
                    name: name.clone(),
                    variants: variants.clone(),
                    origin: Origin::Schema,
                });
            }
            RawItem::Opaque(names) => {
                for n in names {
                    if dup(schema, n) {
                        errors.push(SchemaError {
                            line: *line,
                            message: format!("`{n}` is declared twice"),
                        });
                        continue;
                    }
                    schema.opaques.insert(n.clone(), Ty::opaque(n));
                }
            }
            RawItem::Record {
                name, key, service, ..
            } => {
                if dup(schema, name) || declared_later_as_opaque(all_items, name) {
                    errors.push(SchemaError {
                        line: *line,
                        message: format!("`{name}` is declared twice"),
                    });
                    continue;
                }
                let mut rec = RecordDef::new(name.clone(), Origin::Schema);
                rec.key = key.clone();
                let id = schema.types.add_record(rec);
                if *service {
                    schema.services.insert(name.clone(), id);
                }
            }
            _ => {}
        }
    }
    // Aliases may refer to each other in order.
    for (item, line) in &items {
        if let RawItem::Alias { name, ty } = item {
            match resolve(schema, ty, *line) {
                Ok(t) => {
                    schema.aliases.insert(name.clone(), t);
                }
                Err(e) => errors.push(e),
            }
        }
    }

    // Pass 2: structure.
    for (item, line) in &items {
        let line = *line;
        match item {
            RawItem::Record { name, members, .. } => {
                let Some(id) = schema.types.find_record(name) else {
                    continue;
                };
                let mut fields = Vec::new();
                let mut methods = Vec::new();
                let mut events = Vec::new();
                for m in members {
                    match m {
                        RawMember::Field { name, ty, rw, line } => {
                            match resolve(schema, ty, *line) {
                                Ok(ty) => fields.push(FieldDef {
                                    name: name.clone(),
                                    ty,
                                    rw: *rw,
                                }),
                                Err(e) => errors.push(e),
                            }
                        }
                        RawMember::Method(sig) => match resolve_sig(schema, sig) {
                            Ok(s) => add_method(&mut methods, &sig.name, s),
                            Err(e) => errors.push(e),
                        },
                        RawMember::Event { name, params, line } => {
                            match resolve_params(schema, params, *line) {
                                Ok(params) => events.push(EventDef {
                                    name: name.clone(),
                                    params,
                                }),
                                Err(e) => errors.push(e),
                            }
                        }
                    }
                }
                let rec = schema.types.record_mut(id);
                rec.fields = fields;
                rec.methods = methods;
                rec.events = events;
            }
            RawItem::Fn(sig) => match resolve_sig(schema, sig) {
                Ok(s) => schema
                    .functions
                    .entry(sig.name.clone())
                    .or_default()
                    .push(s),
                Err(e) => errors.push(e),
            },
            RawItem::Value { name, ty } => match resolve(schema, ty, line) {
                Ok(t) => {
                    schema.values.insert(name.clone(), t);
                }
                Err(e) => errors.push(e),
            },
            RawItem::Methods { ty_name, sigs } => {
                let mut list = schema.methods.remove(ty_name).unwrap_or_default();
                for sig in sigs {
                    match resolve_sig(schema, sig) {
                        Ok(s) => add_method(&mut list, &sig.name, s),
                        Err(e) => errors.push(e),
                    }
                }
                schema.methods.insert(ty_name.clone(), list);
            }
            RawItem::Element {
                group,
                name,
                arg,
                includes,
                members,
            } => {
                let mut el = ElementSchema {
                    name: name.clone(),
                    arg: None,
                    props: Vec::new(),
                    events: Vec::new(),
                    scope: Vec::new(),
                    flags: ElementFlags::default(),
                };
                for inc in includes {
                    match schema.groups.get(inc) {
                        Some(g) => {
                            el.props.extend(g.props.iter().cloned());
                            el.events.extend(g.events.iter().cloned());
                            el.scope.extend(g.scope.iter().cloned());
                        }
                        None => errors.push(SchemaError {
                            line,
                            message: format!("unknown group `{inc}`"),
                        }),
                    }
                }
                if let Some(a) = arg {
                    match resolve(schema, a, line) {
                        Ok(t) => el.arg = Some(t),
                        Err(e) => errors.push(e),
                    }
                }
                if let Err(e) = el_members(schema, members, &mut el) {
                    errors.extend(e);
                }
                if *group {
                    schema.groups.insert(name.clone(), el);
                } else {
                    schema.elements.insert(name.clone(), el);
                }
            }
            RawItem::Palette(names) => {
                for n in names {
                    schema.tokens.insert(
                        n.clone(),
                        TokenSchema {
                            ty: Ty::COLOR,
                            palette: true,
                        },
                    );
                }
            }
            RawItem::Tokens(entries) => {
                for (path, ty, line) in entries {
                    match resolve(schema, ty, *line) {
                        Ok(ty) => {
                            schema
                                .tokens
                                .insert(path.clone(), TokenSchema { ty, palette: false });
                        }
                        Err(e) => errors.push(e),
                    }
                }
            }
            RawItem::Enum { .. } | RawItem::Opaque(_) | RawItem::Alias { .. } => {}
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Whether `name` is also an `opaque` in this text (two kinds of type
/// under one name).
fn declared_later_as_opaque(items: &[(RawItem, u32)], name: &str) -> bool {
    items
        .iter()
        .any(|(i, _)| matches!(i, RawItem::Opaque(ns) if ns.iter().any(|n| n == name)))
}

fn el_members(
    schema: &Schema,
    members: &[RawElMember],
    el: &mut ElementSchema,
) -> Result<(), Vec<SchemaError>> {
    let mut errors = Vec::new();
    for m in members {
        match m {
            RawElMember::Prop { .. } => match prop(schema, m) {
                Ok(p) => {
                    // A later declaration refines an included one.
                    el.props.retain(|q| q.name != p.name);
                    el.props.push(p);
                }
                Err(e) => errors.extend(e),
            },
            RawElMember::Event { name, params, line } => {
                match resolve_params(schema, params, *line) {
                    Ok(params) => {
                        el.events.retain(|e| &e.name != name);
                        el.events.push(EventDef {
                            name: name.clone(),
                            params,
                        });
                    }
                    Err(e) => errors.push(e),
                }
            }
            RawElMember::Scope { name, ty, line } => match resolve(schema, ty, *line) {
                Ok(t) => el.scope.push((name.clone(), t)),
                Err(e) => errors.push(e),
            },
            RawElMember::Flags(flags, line) => {
                let mut it = flags.iter();
                while let Some(f) = it.next() {
                    match f.as_str() {
                        "leaf" => el.flags.leaf = true,
                        "surface" => el.flags.surface = true,
                        "uniforms" => el.flags.uniforms = true,
                        "selectors" => el.flags.selectors = true,
                        "only_in" => el.flags.only_in = it.next().cloned(),
                        other => errors.push(SchemaError {
                            line: *line,
                            message: format!("unknown flag `{other}`"),
                        }),
                    }
                }
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn prop(schema: &Schema, m: &RawElMember) -> Result<PropSchema, Vec<SchemaError>> {
    let RawElMember::Prop {
        name,
        ty,
        two_way,
        inherit,
        sub,
        line,
    } = m
    else {
        return Err(Vec::new());
    };
    let ty = resolve(schema, ty, *line).map_err(|e| vec![e])?;
    let mut holder = ElementSchema {
        name: name.clone(),
        arg: None,
        props: Vec::new(),
        events: Vec::new(),
        scope: Vec::new(),
        flags: ElementFlags::default(),
    };
    el_members(schema, sub, &mut holder)?;
    Ok(PropSchema {
        name: name.clone(),
        ty,
        two_way: *two_way,
        inherited: *inherit,
        sub: holder.props,
    })
}
