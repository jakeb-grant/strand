//! Write-back (design.md, "Developer experience"; m5-owner Q11): an edit
//! made in the inspector goes into the source only where it is a literal
//! and only if the file is still what the running shell compiled.
//!
//! - **What can change:** a literal prop value of the picked node's own
//!   element, at its `origin` span (`bg: #f38ba8`, `width: 200`, `label:
//!   "hi"`, `-4`), or a token's literal value at its definition in a
//!   `tokens <name> { }` block or a component's `tokens { }`. A value
//!   that is an expression, a binding or a token reference, and a token
//!   the palette or an overlay sets, is refused: the edit would change
//!   what the value means, not just what it is.
//! - **When:** only if the file's bytes still hash (BLAKE3) to the hash
//!   the running program was compiled from (the answer's `files`), so a
//!   file changed underneath is never overwritten.
//! - **How:** as a span edit, written atomically (a temporary file in
//!   the same directory, flushed, renamed over the file, the directory
//!   flushed), following symlinks so a linked config stays linked. A
//!   file renamed into place between the hash check and the rename (an
//!   editor's atomic save) is refused, as is a read-only target (a Nix
//!   store file), with the reason and an offer to open it in `$EDITOR`.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::ops::Range;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use serde_json::Value as Json;
use strand_compiler::FileId;
use strand_compiler::syntax::ast::{
    ArmBody, Block, Else, ExprKind, Item, ItemKind, TokenBody, TokenEntry, UnaryOp,
};
use strand_compiler::syntax::{Span, parse, parse_expr_in};

/// One span edit, computed against the text that hashes to `hash`.
#[derive(Clone, Debug, PartialEq)]
pub struct Edit {
    /// The file as the running shell names it.
    pub file: PathBuf,
    /// Bytes of the file the new text replaces.
    pub span: Range<usize>,
    /// The new literal.
    pub text: String,
    /// The BLAKE3 hash (hex) the running program compiled the file from.
    pub hash: String,
}

/// Why an edit is not written.
#[derive(Clone, Debug, PartialEq)]
pub enum Refusal {
    /// The file changed since the running shell compiled it, or was
    /// replaced while the edit was being written.
    Changed(PathBuf),
    /// The file (or its directory) cannot be written.
    ReadOnly { file: PathBuf, why: String },
    /// The value written there is not a literal.
    NotLiteral(String),
    /// Nothing the edit names was found.
    NotFound(String),
    /// The file could not be read or written.
    Io { file: PathBuf, why: String },
}

impl Refusal {
    /// The file a read-only refusal offers to open in `$EDITOR`.
    pub fn editor_offer(&self) -> Option<&Path> {
        match self {
            Refusal::ReadOnly { file, .. } => Some(file),
            _ => None,
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Changed(p) => write!(
                f,
                "{} changed since the shell compiled it: not written (save it so the \
                 shell reloads, then edit again)",
                p.display()
            ),
            Refusal::ReadOnly { file, why } => write!(
                f,
                "{} is read-only ({why}): not written; open it in $EDITOR instead",
                file.display()
            ),
            Refusal::NotLiteral(what) => write!(f, "{what} is not a literal: not written"),
            Refusal::NotFound(what) => write!(f, "{what}: not found"),
            Refusal::Io { file, why } => write!(f, "{}: {why}", file.display()),
        }
    }
}

/// The BLAKE3 hash (hex) of `bytes`, as `source_hash` spells it.
pub fn hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// The edit setting prop `prop` of the node `pane` describes (an
/// `inspect.node` answer's `node`) to the literal `value`, with
/// `files` the answer's hashes.
pub fn prop_edit(pane: &Json, files: &Json, prop: &str, value: &str) -> Result<Edit, Refusal> {
    let origin = &pane["origin"];
    let file = origin["file"]
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| Refusal::NotFound("the node's source file".into()))?;
    let span = match origin["span"].as_array().map(Vec::as_slice) {
        Some([a, b]) => match (a.as_u64(), b.as_u64()) {
            (Some(a), Some(b)) => Span::new(a as u32, b as u32),
            _ => return Err(Refusal::NotFound("the node's span".into())),
        },
        _ => return Err(Refusal::NotFound("the node's span".into())),
    };
    let (text, hash) = current(&file, files)?;
    let parsed = parse(FileId(0), &text);
    let element = innermost_at(&parsed.file.items, span)
        .ok_or_else(|| Refusal::NotFound(format!("the node's element in {}", file.display())))?;
    let body = match &element.kind {
        ItemKind::Element(e) => e.block.as_ref(),
        ItemKind::Surface(s) => Some(&s.body),
        _ => None,
    };
    let found = body
        .into_iter()
        .flat_map(|b| &b.items)
        .find_map(|i| match &i.kind {
            ItemKind::Prop(p) if p.name.name == prop => Some(p),
            _ => None,
        })
        .ok_or_else(|| Refusal::NotFound(format!("`{prop}:` written on the node's own element")))?;
    if found.two_way.is_some() || found.transition.is_some() || !literal(&found.value.kind) {
        return Err(Refusal::NotLiteral(format!("`{prop}:`'s value")));
    }
    checked_literal(value)?;
    Ok(Edit {
        file,
        span: found.value.span.start as usize..found.value.span.end as usize,
        text: value.to_string(),
        hash,
    })
}

/// The edit setting token `path` (as a provenance step names it, no `$`)
/// to the literal `value` at its definition: `origin` is the step's
/// origin, `tokens base` for `tokens base { }` and `component Toast` for
/// a component's `tokens { }` (any other origin, the built-in theme's
/// `base`, a palette or an overlay, is not written in a file). `files` is an answer's hashes: every file
/// the running program was compiled from.
pub fn token_edit(files: &Json, path: &str, origin: &str, value: &str) -> Result<Edit, Refusal> {
    let component = origin.strip_prefix("component ");
    let tier = origin.strip_prefix("tokens ");
    if component.is_none() && tier.is_none() {
        return Err(Refusal::NotLiteral(format!(
            "`${path}` (set by {origin}, not written in a `tokens` block)"
        )));
    }
    let names = files.as_object().into_iter().flat_map(|m| m.keys());
    // A file changed since the shell compiled it may be the one: if the
    // token is found in no other, that is the refusal.
    let mut unread = None;
    for name in names {
        let file = PathBuf::from(name);
        let (text, hash) = match current(&file, files) {
            Ok(t) => t,
            Err(r) => {
                unread.get_or_insert(r);
                continue;
            }
        };
        let parsed = parse(FileId(0), &text);
        for item in &parsed.file.items {
            let entries = match (&item.kind, component, tier) {
                (ItemKind::Tokens(t), None, Some(tier)) if t.name.name == tier => &t.entries,
                (ItemKind::Component(c), Some(name), None) if c.name.name == name => {
                    match &c.tokens {
                        Some(t) => t,
                        None => continue,
                    }
                }
                _ => continue,
            };
            // A component's token paths are written without its name.
            let local = match component {
                Some(c) => path.strip_prefix(c).and_then(|p| p.strip_prefix('.')),
                None => Some(path),
            };
            let Some(local) = local else { continue };
            if let Some(e) = entry(entries, "", local) {
                let TokenBody::Value(v) = &e.body else {
                    return Err(Refusal::NotLiteral(format!("`${path}`")));
                };
                if !literal(&v.kind) {
                    return Err(Refusal::NotLiteral(format!("`${path}`'s value")));
                }
                checked_literal(value)?;
                return Ok(Edit {
                    file,
                    span: v.span.start as usize..v.span.end as usize,
                    text: value.to_string(),
                    hash,
                });
            }
        }
    }
    Err(unread.unwrap_or_else(|| Refusal::NotFound(format!("`${path}` in the {origin} block"))))
}

/// The entry of `block` whose path (under `prefix`) is `path`.
fn entry<'a>(block: &'a Block<TokenEntry>, prefix: &str, path: &str) -> Option<&'a TokenEntry> {
    for e in &block.items {
        let key: Vec<&str> = e.key.segments.iter().map(|s| s.name.as_str()).collect();
        let full = match prefix {
            "" => key.join("."),
            p => format!("{p}.{}", key.join(".")),
        };
        match &e.body {
            TokenBody::Value(_) if full == path => return Some(e),
            TokenBody::Group(g) if path.starts_with(&format!("{full}.")) => {
                if let Some(found) = entry(g, &full, path) {
                    return Some(found);
                }
            }
            _ => {}
        }
    }
    None
}

/// The innermost element or surface whose span holds `span`: every
/// item that holds a tree (an element's block, a surface's or a
/// component's body, `when`, `for`, each branch of an `if`/`else if`
/// chain and each `match` arm) is searched.
fn innermost_at(items: &[Item], span: Span) -> Option<&Item> {
    let mut best: Option<&Item> = None;
    let mut stack: Vec<&Item> = items.iter().collect();
    while let Some(i) = stack.pop() {
        if i.span.start > span.start || i.span.end < span.end {
            continue;
        }
        if matches!(i.kind, ItemKind::Element(_) | ItemKind::Surface(_))
            && best.is_none_or(|b| i.span.len() <= b.span.len())
        {
            best = Some(i);
        }
        match &i.kind {
            ItemKind::Element(e) => stack.extend(e.block.iter().flat_map(|b| &b.items)),
            ItemKind::Surface(s) => stack.extend(&s.body.items),
            ItemKind::Component(c) => stack.extend(&c.body.items),
            ItemKind::When(w) => stack.extend(&w.body.items),
            ItemKind::For(f) => stack.extend(&f.body.items),
            ItemKind::If(f) => {
                let mut branch = Some(f);
                while let Some(f) = branch {
                    stack.extend(&f.then.items);
                    branch = match &f.else_ {
                        Some(Else::If(next, _)) => Some(next),
                        Some(Else::Block(b)) => {
                            stack.extend(&b.items);
                            None
                        }
                        None => None,
                    };
                }
            }
            ItemKind::Match(m) => {
                for arm in &m.arms {
                    match &arm.body {
                        ArmBody::Block(b) => stack.extend(&b.items),
                        ArmBody::Single(item) => stack.push(item),
                    }
                }
            }
            _ => {}
        }
    }
    best
}

/// A value the edit may replace and write: a number (with its unit,
/// negative or not), a string, a colour or a boolean.
fn literal(e: &ExprKind) -> bool {
    match e {
        ExprKind::Number(_) | ExprKind::String(_) | ExprKind::Color(_) | ExprKind::Bool(_) => true,
        ExprKind::Unary {
            op: UnaryOp::Neg,
            expr,
        } => matches!(expr.kind, ExprKind::Number(_)),
        _ => false,
    }
}

/// `value` parsed alone must be one literal.
fn checked_literal(value: &str) -> Result<(), Refusal> {
    let end = u32::try_from(value.len()).map_err(|_| Refusal::NotLiteral("the value".into()))?;
    let (e, diags) = parse_expr_in(value, 0, end);
    if diags.iter().any(|d| d.is_error()) || !literal(&e.kind) || value.contains('\n') {
        return Err(Refusal::NotLiteral(format!("`{value}`")));
    }
    Ok(())
}

/// `file`'s text, which must hash to the running program's hash of it.
fn current(file: &Path, files: &Json) -> Result<(String, String), Refusal> {
    let want = files[file.to_string_lossy().as_ref()]
        .as_str()
        .ok_or_else(|| Refusal::NotFound(format!("{} in the running shell", file.display())))?
        .to_string();
    let bytes = fs::read(file).map_err(|e| Refusal::Io {
        file: file.to_path_buf(),
        why: e.to_string(),
    })?;
    if hash(&bytes) != want {
        return Err(Refusal::Changed(file.to_path_buf()));
    }
    let text = String::from_utf8(bytes).map_err(|_| Refusal::Io {
        file: file.to_path_buf(),
        why: "not UTF-8".into(),
    })?;
    Ok((text, want))
}

/// Writes `edit` (see the module docs). Returns the file written (the
/// symlink's target when the shell's file is a link).
pub fn write(edit: &Edit) -> Result<PathBuf, Refusal> {
    write_with(edit, |_| {})
}

/// [`write`], calling `between` with the file's real path after its
/// hash is checked and before the new text is renamed over it (tests
/// race an editor's save there).
#[doc(hidden)]
pub fn write_with(edit: &Edit, between: impl FnOnce(&Path)) -> Result<PathBuf, Refusal> {
    let io = |file: &Path, e: std::io::Error| Refusal::Io {
        file: file.to_path_buf(),
        why: e.to_string(),
    };
    // Following symlinks: the link stays, its target is edited.
    let real = fs::canonicalize(&edit.file).map_err(|e| io(&edit.file, e))?;
    let mut f = File::open(&real).map_err(|e| io(&real, e))?;
    let meta = f.metadata().map_err(|e| io(&real, e))?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes).map_err(|e| io(&real, e))?;
    if hash(&bytes) != edit.hash {
        return Err(Refusal::Changed(edit.file.clone()));
    }
    if meta.permissions().readonly() {
        return Err(Refusal::ReadOnly {
            file: edit.file.clone(),
            why: "it has no write permission".into(),
        });
    }
    let (a, b) = (edit.span.start, edit.span.end);
    if a > b || b > bytes.len() || !is_boundary(&bytes, a) || !is_boundary(&bytes, b) {
        return Err(Refusal::Changed(edit.file.clone()));
    }
    let mut out = Vec::with_capacity(bytes.len() + edit.text.len());
    out.extend_from_slice(&bytes[..a]);
    out.extend_from_slice(edit.text.as_bytes());
    out.extend_from_slice(&bytes[b..]);
    let dir = real.parent().unwrap_or(Path::new("/")).to_path_buf();
    let name = real
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.strand-dev-{}", std::process::id()));
    let mut t = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(meta.mode() & 0o7777)
        .open(&tmp)
    {
        Ok(t) => t,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem
            ) =>
        {
            return Err(Refusal::ReadOnly {
                file: edit.file.clone(),
                why: format!("its directory cannot be written: {e}"),
            });
        }
        Err(e) => return Err(io(&tmp, e)),
    };
    let written = t
        .write_all(&out)
        .and_then(|()| t.set_permissions(fs::Permissions::from_mode(meta.mode() & 0o7777)))
        .and_then(|()| t.sync_all());
    drop(t);
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(io(&tmp, e));
    }
    between(&real);
    // Still the file that was hashed, holding what was hashed: an
    // editor's save renamed into place meanwhile is never overwritten.
    let same = fs::metadata(&real)
        .ok()
        .filter(|m| m.dev() == meta.dev() && m.ino() == meta.ino())
        .and_then(|_| fs::read(&real).ok())
        .is_some_and(|now| hash(&now) == edit.hash);
    if !same {
        let _ = fs::remove_file(&tmp);
        return Err(Refusal::Changed(edit.file.clone()));
    }
    if let Err(e) = fs::rename(&tmp, &real) {
        let _ = fs::remove_file(&tmp);
        return Err(io(&real, e));
    }
    if let Ok(d) = File::open(&dir) {
        let _ = d.sync_all();
    }
    Ok(real)
}

/// `at` falls between UTF-8 characters of `bytes`.
fn is_boundary(bytes: &[u8], at: usize) -> bool {
    at == bytes.len() || bytes.get(at).is_some_and(|b| (*b as i8) >= -0x40)
}
