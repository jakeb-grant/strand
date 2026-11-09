//! (doc-formats) XML documents: a linear pre-scan that bounds what
//! roxmltree is given, the parse, and the element tree as untyped
//! [`Data`] (decisions.md doc-formats).
//!
//! roxmltree recurses once per nesting level (a few thousand levels
//! overflow a thread's stack, which aborts the process) and checks
//! duplicate attributes in quadratic time, copies the namespaces in
//! scope into each element that declares one and looks every entity
//! reference up in a list, so [`prescan`] refuses, before roxmltree sees
//! it, a document nested deeper than [`MAX_DEPTH`], an element with more
//! than [`MAX_ATTRIBUTES`] attributes, more than [`MAX_NAMESPACES`]
//! namespaces in scope or [`MAX_NAMESPACE_COPIES`] copied in all, and a
//! DTD that declares more than [`MAX_ENTITIES`] entities, a parameter
//! entity, an external entity, an entity whose value holds `&`, `%` or
//! `<`, or entities whose references expand past the document budget. The scan skips comments, CDATA, processing
//! instructions and quoted values, reads a DTD's internal subset token
//! for token as roxmltree does (so it skips nothing roxmltree reads),
//! and refuses the document at a construct it cannot follow, which
//! roxmltree would refuse too; it reads each byte a bounded number of
//! times.
//!
//! The root element is the document: an element with neither
//! attributes nor child elements is its text (CDATA and entities
//! decoded, ends trimmed, inner line breaks kept); any other is a record
//! of type [`ELEMENT`] holding its attributes as `@name`, then its child
//! elements and its non-blank text runs (`#text`) in document order,
//! repeated names kept in place. Names are written with the prefix the
//! document uses (`dc:creator`); comments, processing instructions and
//! namespace declarations are not fields.

use std::borrow::Cow;
use std::collections::HashMap;

use crate::Data;

/// The type name of a record an XML element became: no identifier and
/// no XML name can be it, so the language side tells element records
/// from JSON objects by it.
pub const ELEMENT: &str = "<element>";

/// The deepest an XML document may nest (serde_json's own limit; real
/// inputs reach 12).
pub const MAX_DEPTH: usize = 128;

/// The most attributes one element may have (real ones have about 8).
pub const MAX_ATTRIBUTES: usize = 64;

/// The most namespace declarations in scope at one element (real
/// documents have about 10; an OpenDocument root about 35).
pub const MAX_NAMESPACES: usize = 64;

/// The most namespaces roxmltree copies in all: each element that
/// declares one copies those in scope (an Atom feed with XHTML entries
/// copies about a thousand).
pub const MAX_NAMESPACE_COPIES: usize = 1 << 20;

/// The most entities a DTD may declare, repeats included (real internal
/// subsets declare a handful).
pub const MAX_ENTITIES: usize = 256;

/// The most nodes roxmltree is asked to build.
const MAX_NODES: u32 = 1_000_000;

/// `text` (trimmed, starting with `<`) as an XML document's root and
/// where the root element starts in it (0 when no declaration, DOCTYPE
/// or comment precedes it), or why it is not one. `budget` bounds what
/// declared entities may expand to.
pub fn parse(text: &str, budget: usize) -> Result<(Data, usize), String> {
    prescan(text.as_bytes(), budget)?;
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        nodes_limit: MAX_NODES,
        // No resolver: an external entity is an error, nothing fetched.
        entity_resolver: None,
    };
    let doc = roxmltree::Document::parse_with_options(text, options).map_err(|e| e.to_string())?;
    let root = doc.root_element();
    Ok((convert(text, root), root.range().start))
}

// --- The pre-scan -----------------------------------------------------------

/// Where a construct ends: the index after `end`, searched from `from`.
fn after(b: &[u8], from: usize, end: &[u8]) -> Option<usize> {
    let rest = b.get(from..)?;
    rest.windows(end.len())
        .position(|w| w == end)
        .map(|i| from + i + end.len())
}

/// A quoted value starting at `i` (a quote): the index after its closing
/// quote.
fn quoted(b: &[u8], i: usize) -> Option<usize> {
    let q = *b.get(i)?;
    let close = b.get(i + 1..)?.iter().position(|c| *c == q)?;
    Some(i + 1 + close + 1)
}

fn starts(b: &[u8], i: usize, s: &[u8]) -> bool {
    b.get(i..).is_some_and(|r| r.starts_with(s))
}

/// The declared entities and how far their references have expanded.
struct Entities<'a> {
    declared: HashMap<&'a [u8], usize>,
    /// Declarations read, repeats included.
    count: usize,
    expanded: usize,
    budget: usize,
}

/// The open elements: how many namespaces each declares, how many are
/// in scope, and how many roxmltree will have copied.
#[derive(Default)]
struct Open {
    declared: Vec<usize>,
    in_scope: usize,
    copied: usize,
}

impl<'a> Entities<'a> {
    /// A reference may start at `i` (an `&`): its expansion counted.
    /// Returns where scanning goes on.
    fn reference(&mut self, b: &'a [u8], i: usize) -> Result<usize, String> {
        let name_end = b[i + 1..]
            .iter()
            .position(|c| !is_name_byte(*c))
            .map_or(b.len(), |p| i + 1 + p);
        if b.get(name_end) == Some(&b';')
            && let Some(len) = self.declared.get(&b[i + 1..name_end])
        {
            self.expanded = self.expanded.saturating_add(*len);
            if self.expanded > self.budget {
                return Err(format!(
                    "its declared entities expand past {} bytes",
                    self.budget
                ));
            }
        }
        Ok(name_end.max(i + 1))
    }
}

fn is_name_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.' | b':') || c >= 0x80
}

/// See the module docs. `Ok`: roxmltree may be given it (which may still
/// refuse it).
fn prescan(b: &[u8], budget: usize) -> Result<(), String> {
    let mut entities = Entities {
        declared: HashMap::new(),
        count: 0,
        expanded: 0,
        budget,
    };
    let mut open = Open::default();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'&' => i = entities.reference(b, i)?,
            b'<' => {
                let next = if starts(b, i, b"<!--") {
                    after(b, i + 4, b"-->")
                } else if starts(b, i, b"<![CDATA[") {
                    after(b, i + 9, b"]]>")
                } else if starts(b, i, b"<?") {
                    after(b, i + 2, b"?>")
                } else if starts(b, i, b"<!DOCTYPE") {
                    doctype(b, i + 9, &mut entities)?
                } else if starts(b, i, b"</") {
                    if let Some(n) = open.declared.pop() {
                        open.in_scope -= n;
                    }
                    after(b, i + 2, b">")
                } else if starts(b, i, b"<!") {
                    after(b, i + 2, b">")
                } else {
                    tag(b, i + 1, &mut open, &mut entities)?
                };
                match next {
                    Some(n) => i = n,
                    // Not terminated, or a DTD roxmltree refuses: no
                    // XML. Refused here rather than trusting roxmltree
                    // to stop at the same place.
                    None => return Err(format!("it is not well-formed XML at byte {i}")),
                }
            }
            _ => i += 1,
        }
    }
    Ok(())
}

/// An opening tag whose name starts at `i`: its attributes and namespace
/// declarations counted, its values' references expanded, opened unless
/// it closes itself. The index after its `>`.
fn tag<'a>(
    b: &'a [u8],
    mut i: usize,
    open: &mut Open,
    entities: &mut Entities<'a>,
) -> Result<Option<usize>, String> {
    let mut attributes = 0usize;
    let mut namespaces = 0usize;
    // The last name read: an attribute's when `=` follows it.
    let mut name = (i, i);
    while i < b.len() {
        match b[i] {
            b'"' | b'\'' => {
                let Some(end) = quoted(b, i) else {
                    return Ok(None);
                };
                let mut j = i + 1;
                while j < end - 1 {
                    j = if b[j] == b'&' {
                        entities.reference(b, j)?.min(end - 1)
                    } else {
                        j + 1
                    };
                }
                i = end;
            }
            b'=' => {
                attributes += 1;
                if attributes > MAX_ATTRIBUTES {
                    return Err(format!(
                        "an element has more than {MAX_ATTRIBUTES} attributes"
                    ));
                }
                let n = &b[name.0..name.1];
                if n == b"xmlns" || n.starts_with(b"xmlns:") {
                    namespaces += 1;
                }
                i += 1;
            }
            b'>' => {
                if namespaces > 0 {
                    let in_scope = open.in_scope + namespaces;
                    if in_scope > MAX_NAMESPACES {
                        return Err(format!(
                            "it has more than {MAX_NAMESPACES} namespaces in scope"
                        ));
                    }
                    open.copied += in_scope;
                    if open.copied > MAX_NAMESPACE_COPIES {
                        return Err("its elements declare too many namespaces".into());
                    }
                }
                if b[i - 1] != b'/' {
                    open.declared.push(namespaces);
                    open.in_scope += namespaces;
                    if open.declared.len() > MAX_DEPTH {
                        return Err(format!("it nests deeper than {MAX_DEPTH} elements"));
                    }
                }
                return Ok(Some(i + 1));
            }
            c => {
                if is_name_byte(c) {
                    if !is_name_byte(b[i - 1]) {
                        name.0 = i;
                    }
                    name.1 = i + 1;
                }
                i += 1;
            }
        }
    }
    Ok(None)
}

/// XML's white space, as roxmltree skips it (`\x0c` is not).
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}

fn skip_space(b: &[u8], mut i: usize) -> usize {
    while b.get(i).copied().is_some_and(is_space) {
        i += 1;
    }
    i
}

/// A DOCTYPE whose body starts at `i`, read as roxmltree reads it (its
/// name and external ID, then `[` or `>`), its internal subset's entity
/// declarations noted (and refused as the module docs say). The index
/// after its `>`; `None`: roxmltree refuses it too.
fn doctype<'a>(
    b: &'a [u8],
    mut i: usize,
    entities: &mut Entities<'a>,
) -> Result<Option<usize>, String> {
    while i < b.len() {
        match b[i] {
            b'"' | b'\'' => match quoted(b, i) {
                Some(n) => i = n,
                None => return Ok(None),
            },
            b'[' => return subset(b, i + 1, entities),
            b'>' => return Ok(Some(i + 1)),
            _ => i += 1,
        }
    }
    Ok(None)
}

/// An internal subset starting at `i`, tokenised exactly as roxmltree's
/// `parse_doctype` does: so a declaration the scan skips is one
/// roxmltree skips, and anything it does not know ends the scan where
/// roxmltree refuses the document. `<!ELEMENT`, `<!ATTLIST` and
/// `<!NOTATION` end at the first `>`, quotes or not (roxmltree's
/// `consume_decl`). The index after the subset's `]>`.
fn subset<'a>(
    b: &'a [u8],
    mut i: usize,
    entities: &mut Entities<'a>,
) -> Result<Option<usize>, String> {
    loop {
        i = skip_space(b, i);
        let next = if starts(b, i, b"<!ENTITY") {
            entity(b, i + 8, entities)?
        } else if starts(b, i, b"<!--") {
            after(b, i + 4, b"-->")
        } else if starts(b, i, b"<?") {
            after(b, i + 2, b"?>")
        } else if starts(b, i, b"]") {
            let end = skip_space(b, i + 1);
            return Ok((b.get(end) == Some(&b'>')).then_some(end + 1));
        } else if starts(b, i, b"<!ELEMENT")
            || starts(b, i, b"<!ATTLIST")
            || starts(b, i, b"<!NOTATION")
        {
            after(b, i + 2, b">")
        } else if b.get(i) == Some(&b'%') {
            return Err("its DTD uses a parameter entity".into());
        } else {
            None
        };
        match next {
            Some(n) => i = n,
            None => return Ok(None),
        }
    }
}

/// `<!ENTITY name "value">` from after `<!ENTITY`, read as roxmltree
/// reads it: an internal entity whose value is plain text is declared;
/// anything else is refused.
fn entity<'a>(
    b: &'a [u8],
    mut i: usize,
    entities: &mut Entities<'a>,
) -> Result<Option<usize>, String> {
    i = skip_space(b, i);
    if b.get(i) == Some(&b'%') {
        return Err("its DTD declares a parameter entity".into());
    }
    let start = i;
    while b.get(i).is_some_and(|c| is_name_byte(*c)) {
        i += 1;
    }
    let name = &b[start..i];
    i = skip_space(b, i);
    match b.get(i) {
        Some(b'"' | b'\'') => {
            let Some(end) = quoted(b, i) else {
                return Ok(None);
            };
            let value = &b[i + 1..end - 1];
            if value.iter().any(|c| matches!(c, b'&' | b'%' | b'<')) {
                return Err(format!(
                    "its DTD declares an entity `{}` holding `&`, `%` or `<`",
                    String::from_utf8_lossy(name)
                ));
            }
            // roxmltree looks each reference up in a list of every
            // declaration, repeats included.
            entities.count += 1;
            if entities.count > MAX_ENTITIES {
                return Err(format!(
                    "its DTD declares more than {MAX_ENTITIES} entities"
                ));
            }
            // roxmltree expands the first declaration of a name (as XML
            // says); the larger is counted, whichever that is.
            let len = entities.declared.entry(name).or_insert(0);
            *len = (*len).max(value.len());
            let close = skip_space(b, end);
            Ok((b.get(close) == Some(&b'>')).then_some(close + 1))
        }
        None => Ok(None),
        Some(_) => Err(format!(
            "its DTD declares an external entity `{}`",
            String::from_utf8_lossy(name)
        )),
    }
}

// --- The tree as data -------------------------------------------------------

/// An element's name as the document writes it (`dc:creator`).
fn qname<'i>(text: &'i str, node: roxmltree::Node<'_, 'i>) -> &'i str {
    let r = node.range();
    text.get(r.start + 1..r.end)
        .and_then(|t| {
            t.split(|c: char| c.is_whitespace() || c == '>' || c == '/')
                .next()
        })
        .filter(|n| !n.is_empty())
        .unwrap_or(node.tag_name().name())
}

/// One element being converted: its record's fields so far, the text
/// run being gathered, and its children still to go.
struct Frame<'a, 'i> {
    name: &'i str,
    children: roxmltree::Children<'a, 'i>,
    fields: Vec<(crate::data::Name, Data)>,
    run: String,
    /// It has attributes or child elements: a record, not text.
    record: bool,
}

impl<'a, 'i> Frame<'a, 'i> {
    fn new(text: &'i str, node: roxmltree::Node<'a, 'i>) -> Frame<'a, 'i> {
        let fields: Vec<(crate::data::Name, Data)> = node
            .attributes()
            .filter(|a| !a.name().starts_with("xmlns"))
            .map(|a| {
                let name = text.get(a.range_qname()).unwrap_or(a.name());
                (format!("@{name}").into(), Data::text(a.value()))
            })
            .collect();
        Frame {
            name: qname(text, node),
            children: node.children(),
            record: !fields.is_empty(),
            fields,
            run: String::new(),
        }
    }

    /// The text run gathered so far, as a `#text` field when it is not
    /// blank.
    fn flush(&mut self) {
        let run = std::mem::take(&mut self.run);
        let t = run.trim();
        if !t.is_empty() {
            self.fields.push(("#text".into(), Data::text(t)));
        }
    }

    fn finish(mut self) -> (&'i str, Data) {
        if !self.record {
            return (self.name, Data::text(self.run.trim()));
        }
        self.flush();
        (
            self.name,
            Data::Record {
                ty: ELEMENT.into(),
                fields: self.fields,
            },
        )
    }
}

/// `root` as data (see the module docs), with an explicit stack.
fn convert(text: &str, root: roxmltree::Node<'_, '_>) -> Data {
    let mut stack = vec![Frame::new(text, root)];
    loop {
        let Some(top) = stack.last_mut() else {
            return Data::Null;
        };
        match top.children.next() {
            Some(child) if child.is_element() => {
                top.record = true;
                top.flush();
                stack.push(Frame::new(text, child));
            }
            Some(child) => {
                if let Some(t) = child.text().filter(|_| child.is_text()) {
                    top.run.push_str(t);
                }
            }
            None => {
                let Some(done) = stack.pop() else {
                    return Data::Null;
                };
                let (name, data) = done.finish();
                match stack.last_mut() {
                    Some(parent) => parent.fields.push((name.to_string().into(), data)),
                    None => return data,
                }
            }
        }
    }
}

// --- Names ------------------------------------------------------------------

/// Whether `d` is a record an XML element became.
pub fn is_element(d: &Data) -> bool {
    matches!(d, Data::Record { ty, .. } if ty == ELEMENT)
}

/// The matches of `name` in record `d`, in order: the matches of the
/// first of these steps that has any (decisions.md doc-formats):
///
/// 1. fields named exactly `name`;
/// 2. (an element, `name` plain: no `:`, `@` or `#`) the attribute
///    `@name`;
/// 3. (`name` plain) fields `p:name`, any prefix;
/// 4. (an element, `name` plain) attributes `@p:name`;
/// 5. (an element, `name` is `text`) its text ([`element_text`]), when
///    it has some.
///
/// So RSS's `link` never finds `atom:link`, `creator` finds
/// `dc:creator`, `href` finds `@href`, and a child beats an attribute of
/// its name, which `"@name"` still reaches. Anything not a record has
/// none.
pub fn lookup<'d>(d: &'d Data, name: &str) -> Vec<Cow<'d, Data>> {
    lookup_named(d, name, true)
}

/// Whether a key path reads `root` only through the `text` alias: the
/// root element's own text, with nothing named `text` in it. That does
/// not make a fragment data ([`super::Document::fields`]): a script's
/// `<span foreground="red">92%</span>` read as `text: text` is markup.
pub fn by_text_alias(root: &Data, key: &[String]) -> bool {
    matches!(key, [k] if k == "text")
        && is_element(root)
        && lookup_named(root, "text", false).is_empty()
        && !element_text(root).is_empty()
}

fn lookup_named<'d>(d: &'d Data, name: &str, alias: bool) -> Vec<Cow<'d, Data>> {
    let Data::Record { fields, .. } = d else {
        return Vec::new();
    };
    let element = is_element(d);
    let matching = |want: &dyn Fn(&str) -> bool| -> Vec<Cow<'d, Data>> {
        fields
            .iter()
            .filter(|(n, _)| want(n))
            .map(|(_, v)| Cow::Borrowed(v))
            .collect()
    };
    let found = matching(&|n| n == name);
    if !found.is_empty() {
        return found;
    }
    let plain = !name.is_empty() && !name.contains([':', '@', '#']);
    let local = |n: &str| {
        n.rsplit_once(':')
            .is_some_and(|(p, l)| !p.is_empty() && l == name)
    };
    if plain {
        if element {
            let found = matching(&|n| n.strip_prefix('@') == Some(name));
            if !found.is_empty() {
                return found;
            }
        }
        let found = matching(&|n| !n.starts_with('@') && local(n));
        if !found.is_empty() {
            return found;
        }
        if element {
            let found = matching(&|n| n.strip_prefix('@').is_some_and(local));
            if !found.is_empty() {
                return found;
            }
        }
    }
    if alias && element && name == "text" {
        let t = element_text(d);
        if !t.is_empty() {
            return vec![Cow::Owned(Data::text(t))];
        }
    }
    Vec::new()
}

/// The text of an element record (DOM's text content): its `#text` runs
/// and its descendants' text in document order, attributes left out,
/// whitespace runs one space where it has child elements, trimmed. Text
/// is itself; anything else has none. The runs were trimmed when read,
/// so a space joins two, except before closing punctuation and after
/// opening punctuation.
pub fn element_text(d: &Data) -> String {
    let mut out = String::new();
    let mut children = false;
    // Depth first, in order, with an explicit stack of field iterators.
    let mut stack: Vec<std::slice::Iter<'_, (crate::data::Name, Data)>> = match d {
        Data::Text(t) => return t.trim().to_string(),
        Data::Record { fields, .. } if is_element(d) => vec![fields.iter()],
        _ => return out,
    };
    // The runs were trimmed: a space joins two, except before closing
    // punctuation and after opening punctuation (`<em>para</em>.`).
    let push = |out: &mut String, t: &str| {
        let closes = t.starts_with([
            '.', ',', ';', ':', '!', '?', ')', ']', '}', '\u{2019}', '\u{201d}',
        ]);
        let opened = out.ends_with(['(', '[', '{', '\u{2018}', '\u{201c}']);
        if !out.is_empty() && !closes && !opened {
            out.push(' ');
        }
        out.push_str(t);
    };
    while let Some(top) = stack.last_mut() {
        let Some((name, v)) = top.next() else {
            stack.pop();
            continue;
        };
        if name.starts_with('@') {
            continue;
        }
        if name != "#text" {
            children = true;
        }
        match v {
            Data::Text(t) => push(&mut out, t),
            Data::Record { fields, .. } if is_element(v) => stack.push(fields.iter()),
            _ => {}
        }
    }
    if children {
        out.split_whitespace().collect::<Vec<_>>().join(" ")
    } else {
        out.trim().to_string()
    }
}

/// The value at `key` in an XML document's root `root` (decisions.md
/// doc-formats): the first segment looked up in the root, else searched
/// for depth first; each later one in the first match of the one before
/// (a number picks that match); the last one's matches are the value,
/// several a list. A root that is only text is every key's value.
pub fn get(root: &Data, key: &[String]) -> Option<Data> {
    if let Data::Text(_) = root {
        return Some(root.clone());
    }
    let (first, rest) = key.split_first()?;
    let mut found = lookup(root, first);
    if found.is_empty() {
        found = find(root, first, 0);
    }
    for k in rest {
        found = match k.parse::<usize>() {
            Ok(n) => found.into_iter().nth(n).into_iter().collect(),
            Err(_) => match found.into_iter().next() {
                Some(Cow::Borrowed(d)) => lookup(d, k),
                Some(Cow::Owned(d)) => lookup(&d, k)
                    .into_iter()
                    .map(|c| Cow::Owned(c.into_owned()))
                    .collect(),
                None => Vec::new(),
            },
        };
    }
    match found.len() {
        0 => None,
        1 => found.pop().map(Cow::into_owned),
        _ => Some(Data::List(found.into_iter().map(Cow::into_owned).collect())),
    }
}

/// The matches of `name` in the first record below `d` that has any,
/// depth first (at most 16 levels, as JSON's search).
fn find<'d>(d: &'d Data, name: &str, depth: usize) -> Vec<Cow<'d, Data>> {
    if depth > 16 {
        return Vec::new();
    }
    let children: Box<dyn Iterator<Item = &'d Data>> = match d {
        Data::Record { fields, .. } => Box::new(fields.iter().map(|(_, v)| v)),
        Data::List(items) => Box::new(items.iter()),
        _ => return Vec::new(),
    };
    for c in children {
        let here = lookup(c, name);
        if !here.is_empty() {
            return here;
        }
        let below = find(c, name, depth + 1);
        if !below.is_empty() {
            return below;
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::custom::{Document, MAX_DOCUMENT};

    macro_rules! fixture {
        ($name:literal) => {
            include_str!(concat!("../../tests/fixtures/xml/", $name))
        };
    }

    const FIXTURES: &[(&str, &str)] = &[
        ("rss.xml", fixture!("rss.xml")),
        ("rdf.xml", fixture!("rdf.xml")),
        ("atom.xml", fixture!("atom.xml")),
        ("atom-xhtml.xml", fixture!("atom-xhtml.xml")),
        ("ecb.xml", fixture!("ecb.xml")),
        ("gpx.xml", fixture!("gpx.xml")),
        ("opml.xml", fixture!("opml.xml")),
        ("nmap.xml", fixture!("nmap.xml")),
        ("dbus-doctype.xml", fixture!("dbus-doctype.xml")),
        ("fontconfig.xml", fixture!("fontconfig.xml")),
    ];

    fn key(k: &str) -> Vec<String> {
        k.split('.').map(str::to_string).collect()
    }

    fn xml(text: &str) -> Data {
        match Document::parse(text) {
            Document::Xml(root, _) => root,
            other => panic!("not XML: {other:?}"),
        }
    }

    fn text(t: &str) -> Data {
        Data::text(t)
    }

    fn element(fields: Vec<(&'static str, Data)>) -> Data {
        Data::Record {
            ty: ELEMENT.into(),
            fields: fields.into_iter().map(|(n, d)| (n.into(), d)).collect(),
        }
    }

    fn texts(found: Vec<Cow<'_, Data>>) -> Vec<String> {
        found
            .iter()
            .map(|d| match &**d {
                Data::Text(t) => t.to_string(),
                d => format!("{d:?}"),
            })
            .collect()
    }

    #[test]
    fn xml_elements_become_records_text_and_order() {
        let root = xml(concat!(
            "<?xml version=\"1.0\"?>\n<!-- a comment --><r a=\"1\" xmlns:x=\"urn:x\">\n",
            "  <t>  padded\n  two lines  </t>\n",
            "  <c><![CDATA[<b>&</b>]]> and <![CDATA[more]]> &amp; &#233;&#x41;</c>\n",
            "  <e/><e2></e2>\n",
            "  lead <x:k>v</x:k> tail<?pi skip?>\n",
            "  <a>1</a><a>2</a>\n",
            "</r>"
        ));
        assert_eq!(
            root,
            element(vec![
                ("@a", text("1")),
                ("t", text("padded\n  two lines")),
                ("c", text("<b>&</b> and more & éA")),
                ("e", text("")),
                ("e2", text("")),
                ("#text", text("lead")),
                ("x:k", text("v")),
                ("#text", text("tail")),
                ("a", text("1")),
                ("a", text("2")),
            ])
        );
        // An element with an attribute and text is a record.
        let guid = xml("<guid isPermaLink=\"false\"> tag:x </guid>");
        assert_eq!(
            guid,
            element(vec![
                ("@isPermaLink", text("false")),
                ("#text", text("tag:x"))
            ])
        );
        // A root of text alone is every key's value, as a JSON scalar.
        let d = Document::parse("<v> 42 </v>");
        assert_eq!(d.get(&key("anything")), Some(text("42")));
        // A BOM goes.
        assert!(matches!(
            Document::parse("\u{feff}<r><a>1</a></r>"),
            Document::Xml(..)
        ));
    }

    #[test]
    fn xml_names_match_exact_then_by_local_part() {
        let channel = xml(concat!(
            "<channel xmlns:atom=\"http://www.w3.org/2005/Atom\" xmlns:dc=\"urn:dc\">",
            "<atom:link href=\"self\" rel=\"self\"/><link>home</link>",
            "<dc:creator>Ada</dc:creator><state state=\"attr\">child</state>",
            "<title type=\"html\">T</title><c dc:x=\"px\"/></channel>"
        ));
        // RSS's `link` is the child named so, never `atom:link`.
        assert_eq!(texts(lookup(&channel, "link")), ["home"]);
        assert_eq!(lookup(&channel, "atom:link").len(), 1);
        // `creator` finds `dc:creator` by its local name.
        assert_eq!(texts(lookup(&channel, "creator")), ["Ada"]);
        assert_eq!(texts(lookup(&channel, "dc:creator")), ["Ada"]);
        // A child beats an attribute of its name; `@` reaches it.
        let state = lookup(&channel, "state");
        assert_eq!(
            texts(lookup(&state[0], "state")),
            ["attr"],
            "no child named so"
        );
        assert_eq!(texts(lookup(&state[0], "@state")), ["attr"]);
        assert_eq!(texts(lookup(&state[0], "#text")), ["child"]);
        let title = lookup(&channel, "title");
        assert_eq!(texts(lookup(&title[0], "type")), ["html"]);
        // `text` is the element's text, last.
        assert_eq!(texts(lookup(&title[0], "text")), ["T"]);
        let opml = xml("<outline text=\"Tech\"><outline text=\"a\"/></outline>");
        assert_eq!(
            texts(lookup(&opml, "text")),
            ["Tech"],
            "the attribute first"
        );
        // Prefixed attributes by their local part.
        let c = lookup(&channel, "c");
        assert_eq!(texts(lookup(&c[0], "x")), ["px"]);
        // JSON objects: exact names only, and `p:name` by its local part.
        let json = Document::parse(r#"{"o": {"@a": 1, "dc:b": 2, "text": 3}}"#).into_data();
        let o = json.field("o").unwrap();
        assert!(lookup(o, "a").is_empty(), "no attributes in JSON");
        assert_eq!(lookup(o, "b").len(), 1);
        assert_eq!(lookup(o, "text").len(), 1);
        assert!(lookup(&text("x"), "x").is_empty());
    }

    #[test]
    fn xml_key_paths_find_items_anywhere() {
        let doc = |name: &str| {
            Document::parse(
                FIXTURES
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, t)| *t)
                    .unwrap(),
            )
        };
        let count = |d: Option<Data>| match d {
            Some(Data::List(items)) => items.len(),
            Some(_) => 1,
            None => 0,
        };
        // RSS: rss/channel/item; RDF: items beside the channel.
        let rss = doc("rss.xml");
        assert_eq!(count(rss.get(&key("item"))), 3);
        assert_eq!(count(rss.get(&key("channel.item"))), 3);
        assert_eq!(
            rss.get(&key("channel.title")),
            Some(text("Example Linux: Recent news"))
        );
        let rdf = doc("rdf.xml");
        assert_eq!(count(rdf.get(&key("item"))), 2);
        assert_eq!(
            rdf.get(&key("channel.date")),
            Some(text("2026-11-07T20:37:15+00:00"))
        );
        // A number picks one match.
        assert_eq!(rdf.get(&key("item.1.title")), Some(text("Wind turned on")));
        assert_eq!(rss.get(&key("item.0.category")), Some(text("security")));
        // The last segment collects several.
        assert_eq!(
            rss.get(&key("item.1.category")),
            Some(Data::List(vec![text("tools"), text("packaging")]))
        );
        let atom = doc("atom.xml");
        assert_eq!(count(atom.get(&key("entry"))), 2);
        assert_eq!(count(atom.get(&key("link"))), 2, "the feed's own links");
        // ECB: `Cube.Cube` is the dated cube, its rates the cubes in it.
        let ecb = doc("ecb.xml");
        assert_eq!(ecb.get(&key("Cube.Cube.time")), Some(text("2026-11-06")));
        assert_eq!(count(ecb.get(&key("Cube.Cube.Cube"))), 3);
        assert_eq!(ecb.get(&key("subject")), Some(text("Reference rates")));
        // A feed with no entries has no key `entry`.
        let empty =
            Document::parse("<feed xmlns=\"http://www.w3.org/2005/Atom\"><title>q</title></feed>");
        assert_eq!(empty.get(&key("entry")), None);
        // nmap: attributes by name, hosts searched for.
        let nmap = doc("nmap.xml");
        assert_eq!(nmap.get(&key("start")), Some(text("1794084315")));
        assert_eq!(count(nmap.get(&key("host"))), 2);
        assert_eq!(
            nmap.get(&key("host.0.ports.port.1.portid")),
            Some(text("80"))
        );
        // A string key is one exact segment.
        assert_eq!(nmap.get(&["@version".to_string()]), Some(text("7.98")));
        assert_eq!(
            rss.get(&["dc:creator".to_string()]),
            Some(text("Ada Example"))
        );
    }

    #[test]
    fn one_line_xml_is_not_pairs_and_html_stays_text() {
        assert_eq!(
            Document::parse("<title>a:b</title>").get(&key("title")),
            Some(text("a:b"))
        );
        assert!(matches!(
            Document::parse("<title>a=b</title>"),
            Document::Xml(..)
        ));
        // An HTML page served for a feed: text, saying why.
        let page = Document::parse(
            "<!doctype html>\n<html><head><title>Checking</title></head><body><p>wait<br>a bit</p></body></html>",
        );
        assert!(page.not_xml().is_some(), "{page:?}");
        assert!(matches!(page.get(&key("item")), Some(Data::Text(_))));
        // Text before the `<` is not XML; pairs and text as before.
        assert!(matches!(Document::parse("junk <r/>"), Document::Text(_)));
        assert!(matches!(Document::parse("a=<b>\nc=d"), Document::Pairs(_)));
    }

    /// Markup a source prints stays markup for a `text` field: an HTML
    /// fragment, a status script's `<span>`, `<b>CPU</b>`.
    #[test]
    fn a_text_field_reads_markup_as_written() {
        use crate::custom::{FieldSpec, TextField};
        let field = |name: &str, text: TextField| FieldSpec {
            name: name.into(),
            key: key(name),
            rw: false,
            text,
        };
        let one = |d: &Document, f: FieldSpec| d.fields("", &[f]).remove(0);
        let html = "<div><h2>Hi</h2><p>Today <b>3</b> updates</p></div>";
        let span = "<span foreground=\"#e06c75\">CPU 92%</span>";
        for doc in [html, span] {
            let d = Document::parse(doc);
            assert!(matches!(d, Document::Xml(..)));
            // `text`: a key the document lacks reads it as written.
            assert_eq!(one(&d, field("body", TextField::Required)), Some(text(doc)));
            // `text?` keeps null for a missing key, as with JSON; other
            // types read the element tree.
            assert_eq!(one(&d, field("body", TextField::Optional)), None);
            assert_eq!(one(&d, field("body", TextField::No)), None);
        }
        // Its own text through a field named `text` is no field held: a
        // bar script's coloured label stays markup.
        let d = Document::parse(span);
        assert_eq!(
            one(&d, field("text", TextField::Required)),
            Some(text(span))
        );
        let both = d.fields(
            "",
            &[
                field("text", TextField::Required),
                field("foreground", TextField::Optional),
            ],
        );
        assert_eq!(both, [Some(text("CPU 92%")), Some(text("#e06c75"))]);
        // A key the document holds reads the element, as before.
        let span = Document::parse(span);
        assert_eq!(
            one(&span, field("foreground", TextField::Required)),
            Some(text("#e06c75"))
        );
        // One element of text alone: as written for `text` and `text?`,
        // its text for anything else (`<value>42</value>` as an `int`).
        let bold = Document::parse("<b>CPU</b>");
        for t in [TextField::Required, TextField::Optional] {
            assert_eq!(one(&bold, field("label", t)), Some(text("<b>CPU</b>")));
        }
        assert_eq!(one(&bold, field("label", TextField::No)), Some(text("CPU")));
        // A feed's fields read as they did.
        let feed = Document::parse(fixture!("rss.xml"));
        assert_eq!(
            one(&feed, field("title", TextField::Required)),
            Some(text("Example Linux: Recent news"))
        );
    }

    /// A document that holds any field's key is data, not a fragment: a
    /// `text` field whose key it lacks is absent (a feed with no
    /// `subtitle` does not show its source; a listen line leaves the
    /// field alone), and a document with a declaration or DOCTYPE is
    /// never read as written, so `<string>` from a SOAP-style endpoint
    /// is its text.
    #[test]
    fn a_document_holding_a_field_is_data_not_markup() {
        use crate::custom::{FieldSpec, TextField};
        let field = |name: &str, text: TextField| FieldSpec {
            name: name.into(),
            key: key(name),
            rw: false,
            text,
        };
        let fs = [
            field("title", TextField::Required),
            field("state", TextField::Required),
        ];
        let status = Document::parse("<status state=\"play\"/>");
        assert_eq!(status.fields("", &fs), vec![None, Some(text("play"))]);
        let feed = Document::parse(fixture!("rss.xml"));
        let got = feed.fields(
            "",
            &[
                field("title", TextField::Required),
                field("subtitle", TextField::Required),
            ],
        );
        assert_eq!(got[0], Some(text("Example Linux: Recent news")));
        assert_eq!(got[1], None);
        // Nothing found: the fragment, as written.
        let span = "<span foreground=\"red\">92%</span>";
        assert_eq!(
            Document::parse(span).fields("", &fs),
            vec![Some(text(span)), Some(text(span))]
        );
        // A prolog makes it data: the element's text, and a missing key
        // absent.
        for doc in [
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><string xmlns=\"http://tempuri.org/\">EUR 1.0842</string>",
            "<!DOCTYPE string><string>EUR 1.0842</string>",
            "<!-- rate --><string>EUR 1.0842</string>",
        ] {
            let d = Document::parse(doc);
            assert_eq!(
                d.fields("", &[field("rate", TextField::Required)]),
                vec![Some(text("EUR 1.0842"))],
                "{doc}"
            );
        }
        let d = Document::parse("<?xml version=\"1.0\"?><div><p>Hi</p></div>");
        assert_eq!(
            d.fields("", &[field("body", TextField::Required)]),
            vec![None]
        );
    }

    #[test]
    fn a_doctype_is_read_and_its_entities_bounded() {
        for name in ["nmap.xml", "dbus-doctype.xml", "fontconfig.xml"] {
            let (_, t) = FIXTURES.iter().find(|(n, _)| *n == name).unwrap();
            assert!(matches!(Document::parse(t), Document::Xml(..)), "{name}");
        }
        let dbus = xml(fixture!("dbus-doctype.xml"));
        let doc = Document::Xml(dbus, None);
        assert_eq!(
            doc.get(&key("method.annotation.value")),
            Some(text("org.example.Device.GeneralError")),
            "the declared entity expanded"
        );
        let refused = |t: &str| match Document::parse(t) {
            Document::NotXml { why, .. } => why,
            other => panic!("read: {other:?}"),
        };
        let laughs = concat!(
            "<?xml version=\"1.0\"?>\n<!DOCTYPE lolz [\n <!ENTITY lol \"lol\">\n",
            " <!ENTITY lol1 \"&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;\">\n",
            " <!ENTITY lol2 \"&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;\">\n",
            "]>\n<lolz>&lol2;</lolz>"
        );
        assert!(refused(laughs).contains("holding"), "{}", refused(laughs));
        assert!(refused("<!DOCTYPE r [ <!ENTITY % p \"x\"> ]><r/>").contains("parameter"));
        assert!(refused("<!DOCTYPE r [ %p; ]><r/>").contains("parameter"));
        assert!(
            refused(
                "<!DOCTYPE r [ <!ENTITY xxe SYSTEM \"file:///etc/passwd\"> ]><r><v>&xxe;</v></r>"
            )
            .contains("external")
        );
        assert!(refused("<!DOCTYPE r [ <!ENTITY a \"<b/>\"> ]><r>&a;</r>").contains("holding"));
        // Many references to a large entity: past the budget, not read.
        let big = "x".repeat(10_000);
        let refs = "&e;".repeat(MAX_DOCUMENT / 10_000 + 1);
        let over = format!("<!DOCTYPE r [ <!ENTITY e \"{big}\"> ]><r a=\"&e;\">{refs}</r>");
        assert!(refused(&over).contains("expand"), "{}", refused(&over));
        // A name declared twice: roxmltree expands the first, so the
        // larger counts.
        let twice = format!("<!DOCTYPE r [ <!ENTITY e \"{big}\"><!ENTITY e \"\"> ]><r>{refs}</r>");
        assert!(refused(&twice).contains("expand"), "{}", refused(&twice));
        // Under it, entities read, in attributes too.
        let under = "<!DOCTYPE r [ <!ENTITY e \"ab\"> ]><r a=\"&e;\">&e;&e;</r>";
        assert_eq!(
            xml(under),
            element(vec![("@a", text("ab")), ("#text", text("abab"))])
        );
    }

    /// The scan reads a DTD's declarations where roxmltree does: an
    /// `<!ATTLIST` ends at its first `>`, quotes or not, so a quote in
    /// one hides neither the elements after the DTD nor an entity.
    #[test]
    fn the_scan_reads_a_dtd_as_roxmltree_does() {
        let refused = |t: String| {
            std::thread::Builder::new()
                .stack_size(256 * 1024)
                .spawn(move || Document::parse(&t).not_xml().map(str::to_string))
                .unwrap()
                .join()
                .unwrap()
                .unwrap_or_default()
        };
        let deep = "<a>".repeat(20_000);
        // An unmatched quote: roxmltree ends the ATTLIST at `>` and
        // recurses into every `<a>`.
        let lone = format!("<!DOCTYPE r [<!ATTLIST r a CDATA '>]><r>{deep}");
        assert!(refused(lone).contains("deeper"));
        // A quote matched two declarations on: the ENTITY between is
        // declared all the same.
        let big = "x".repeat(10_000);
        let refs = "&e;".repeat(MAX_DOCUMENT / 10_000 + 1);
        let hidden = format!(
            "<!DOCTYPE r [<!ATTLIST r a CDATA '><!ENTITY e \"{big}\"><!ATTLIST r b CDATA '>]><r>{refs}</r>"
        );
        assert!(refused(hidden).contains("expand"));
        // What roxmltree refuses in a DTD the scan refuses too.
        for t in [
            "<!DOCTYPE r [<!ATTLIST r a CDATA \"x>y\">]><r/>",
            "<!DOCTYPE r [<!ENTITY e \"x\" junk>]><r/>",
            "<!DOCTYPE r [ junk ]><r/>",
            "<!DOCTYPE r [ ] junk><r/>",
        ] {
            assert!(!refused(t.to_string()).is_empty(), "{t}");
        }
        // Declarations roxmltree reads still read.
        let ok = "<!DOCTYPE r [\n <!ELEMENT r (#PCDATA)>\n <!ATTLIST r a CDATA #IMPLIED>\n <!-- c -->\n <?pi x?>\n <!NOTATION n SYSTEM \"n\">\n <!ENTITY e 'v'>\n] >\n<r>&e;</r>";
        assert!(matches!(Document::parse(ok), Document::Xml(Data::Text(t), _) if &*t == "v"));
    }

    #[test]
    fn xml_that_is_not_well_formed_is_text_and_says_why() {
        for (t, why) in [
            ("<rss><title>a&nbsp;b</title></rss>", "nbsp"),
            ("<rss><dc:creator>x</dc:creator></rss>", "dc"),
            ("<a><b></a>", ""),
            ("<a", ""),
            ("<a><!-- open", ""),
        ] {
            let d = Document::parse(t);
            let Some(reason) = d.not_xml() else {
                panic!("{t}: {d:?}")
            };
            assert!(reason.contains(why), "{t}: {reason}");
            assert_eq!(d.get(&key("x")), Some(text(t)), "read whole as text");
        }
    }

    /// Bounded work and stack on hostile input: refused or read, never an
    /// abort, a panic or a quadratic parse. No wall clock: sizes only.
    #[test]
    fn xml_is_bounded() {
        let deep = format!("{}{}", "<a>".repeat(100_000), "</a>".repeat(100_000));
        let shallow = format!("{}x{}", "<a>".repeat(MAX_DEPTH), "</a>".repeat(MAX_DEPTH));
        // A small stack, as the services' threads may have: refused
        // before roxmltree recurses.
        let r = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                (
                    Document::parse(&deep).not_xml().map(str::to_string),
                    Document::parse(&shallow),
                )
            })
            .unwrap()
            .join()
            .unwrap();
        assert!(r.0.is_some_and(|w| w.contains("deeper")));
        assert!(matches!(r.1, Document::Xml(..)), "128 levels read");
        let attrs: String = (0..5000).map(|i| format!(" a{i}=\"{i}\"")).collect();
        let many = Document::parse(&format!("<r{attrs}/>"));
        assert!(many.not_xml().is_some_and(|w| w.contains("attributes")));
        let ok: String = (0..MAX_ATTRIBUTES)
            .map(|i| format!(" a{i}=\"{i}\""))
            .collect();
        assert!(matches!(
            Document::parse(&format!("<r{ok}/>")),
            Document::Xml(..)
        ));
        // Namespaces: a chain declaring 64 each is refused at the second
        // element, many leaves under the most allowed in scope once
        // their copies pass the bound, and real ones read.
        let decl = |n: usize, from: usize| -> String {
            (from..from + n)
                .map(|i| format!(" xmlns:p{i}=\"u\""))
                .collect()
        };
        let chain = format!(
            "{}{}",
            (0..127)
                .map(|e| format!("<e{}>", decl(MAX_ATTRIBUTES, e * MAX_ATTRIBUTES)))
                .collect::<String>(),
            "</e>".repeat(127)
        );
        let refused = Document::parse(&chain);
        assert!(refused.not_xml().is_some_and(|w| w.contains("in scope")));
        let leaves = format!(
            "<r{}>{}</r>",
            decl(MAX_NAMESPACES - 1, 0),
            "<e xmlns:z=\"u\"/>".repeat(MAX_NAMESPACE_COPIES / MAX_NAMESPACES + 1)
        );
        assert!(
            Document::parse(&leaves)
                .not_xml()
                .is_some_and(|w| w.contains("too many"))
        );
        let fewer = format!(
            "<r{}>{}</r>",
            decl(MAX_NAMESPACES - 1, 0),
            "<e xmlns:z=\"u\"/>".repeat(1000)
        );
        assert!(matches!(Document::parse(&fewer), Document::Xml(..)));
        // A namespace closed with its element leaves the scope.
        let closed = format!(
            "<r>{}</r>",
            format!("<e{}/>", decl(MAX_ATTRIBUTES, 0)).repeat(100)
        );
        assert!(matches!(Document::parse(&closed), Document::Xml(..)));
        // Entities: past MAX_ENTITIES declarations, repeats included.
        let entities = |n: usize| -> String {
            let d: String = (0..n).map(|_| "<!ENTITY a \"\">").collect();
            format!("<!DOCTYPE r [{d}]><r>{}</r>", "&a;".repeat(1000))
        };
        assert!(matches!(
            Document::parse(&entities(MAX_ENTITIES)),
            Document::Xml(..)
        ));
        assert!(
            Document::parse(&entities(MAX_ENTITIES + 1))
                .not_xml()
                .is_some_and(|w| w.contains("entities"))
        );
        // 262k siblings in about 1 MiB.
        let siblings = format!("<r>{}</r>", "<a/>".repeat(262_000));
        let Document::Xml(Data::Record { fields, .. }, _) = Document::parse(&siblings) else {
            panic!("siblings")
        };
        assert_eq!(fields.len(), 262_000);
        // Unterminated constructs stop the scan; roxmltree says why.
        for t in [
            format!("<r>{}", "<!--".repeat(50_000)),
            format!("<r a=\"{}", "x".repeat(100_000)),
            format!("<!DOCTYPE r [{}", "<!ENTITY a \"".repeat(10_000)),
            format!("<r>{}", "<![CDATA[".repeat(50_000)),
            "<".repeat(100_000),
        ] {
            assert!(Document::parse(&t).not_xml().is_some());
        }
    }

    /// Every prefix of every fixture, and seeded byte flips of each: no
    /// panic, and each is XML or text.
    #[test]
    fn xml_survives_mutation() {
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let check = |b: &[u8]| {
            let t = String::from_utf8_lossy(b);
            let d = Document::parse(&t);
            assert!(matches!(
                d,
                Document::Xml(..)
                    | Document::NotXml { .. }
                    | Document::Text(_)
                    | Document::Json(_)
                    | Document::Pairs(_)
            ),);
            if let Document::Xml(root, _) = &d {
                let _ = get(root, &key("item.0.title"));
                let _ = element_text(root);
            }
        };
        for (_, t) in FIXTURES {
            let b = t.as_bytes();
            for end in 0..=b.len() {
                check(&b[..end]);
            }
            for _ in 0..300 {
                let mut m = b.to_vec();
                for _ in 0..1 + next() % 4 {
                    let i = (next() as usize) % m.len();
                    m[i] = b"<>&;\"'/![]-=x \n%"[(next() % 16) as usize];
                }
                check(&m);
            }
        }
    }

    #[test]
    fn every_fixture_is_xml() {
        for (name, t) in FIXTURES {
            let d = Document::parse(t);
            assert!(
                matches!(d, Document::Xml(Data::Record { .. }, _)),
                "{name}: {d:?}"
            );
        }
    }

    #[test]
    fn element_text_reads_mixed_content_in_order() {
        let atom = Document::parse(fixture!("atom-xhtml.xml"));
        let summary = atom.get(&key("entry.summary")).unwrap();
        assert_eq!(element_text(&summary), "Hello big world.");
        let content = atom.get(&key("entry.content")).unwrap();
        assert_eq!(element_text(&content), "First para. one two");
        let dbus = Document::parse(fixture!("dbus-doctype.xml"));
        let para = dbus.get(&key("para")).unwrap();
        assert_eq!(
            element_text(&para),
            "Found through the org.example.Power object with Enumerate."
        );
        assert_eq!(element_text(&text(" a ")), "a");
        assert_eq!(element_text(&element(vec![("@href", text("x"))])), "");
    }
}
