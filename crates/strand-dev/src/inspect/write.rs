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
//!   the same directory, flushed, swapped with the file, the directory
//!   flushed), following symlinks so a linked config stays linked. (M6)
//!   The swap is `renameat2(RENAME_EXCHANGE)`, so nothing is checked
//!   before it and trusted after: the file it took out of place is then
//!   checked to be the one hashed (same inode, same hash), and if an
//!   editor saved in between (a new file renamed into place, or the file
//!   written in place) the two are swapped back and the edit is refused,
//!   keeping the editor's text. If the file was saved again before the
//!   swap back, or the swap back fails, nothing is removed: the latest
//!   save is put in place where it can be, and the refusal says what the
//!   file holds and where the other version is kept (each write has a
//!   temporary name of its own). Where the filesystem has no exchange
//!   (`EINVAL`, or `ENOSYS` before Linux 3.15) the old way is kept: the
//!   inode and hash checked just before a plain rename, with that small
//!   window (decisions.md m6-edges). A read-only target (a Nix store
//!   file) is refused, with the reason and an offer to open it in
//!   `$EDITOR`.
//!   Read-only means this user cannot write the file itself, or the
//!   rename would change more than its text: a file another user owns
//!   (the new file would be ours), or one with an ACL (the new file
//!   would have none). A group the new file did not get from the
//!   directory is given to it, or the edit is refused.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::ops::Range;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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
    write_with(edit, Swap::Auto, |_, _| {})
}

/// (M6) How [`write_with`] puts the new text in place (tests choose).
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Swap {
    /// `RENAME_EXCHANGE`, else (unsupported) a checked rename.
    Auto,
    /// `RENAME_EXCHANGE` only: unsupported is an error.
    Exchange,
    /// The checked rename only.
    Rename,
}

/// (M6) Where [`write_with`] calls its hook (tests race an editor's
/// save there).
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Moment {
    /// The new text is written beside the file, which is not swapped
    /// yet: the path is the file's.
    Before,
    /// After the last look at the file before it is replaced. With the
    /// exchange: the file is swapped out and not yet checked, and the
    /// path is where it now is (the temporary name). With the plain
    /// rename: the file is checked and not yet renamed over, and the
    /// path is the file's.
    Window,
    /// (exchange) The file swapped out was found changed and is not yet
    /// swapped back: the path is the file's, which holds the edit.
    Back,
    /// (exchange) The swap back brought out a later save than the edit,
    /// which is not yet exchanged into place: the path is the file's,
    /// which holds the save before it (edges-r2-2). Called before each
    /// such exchange.
    Again,
    /// (exchange) The exchange found no file to swap with (`ENOENT`),
    /// and the refusal is not yet made: the path is the file's, where an
    /// editor saving by a backup may already have written anew
    /// (edges-r3-1).
    Missing,
}

/// Temporary names made by this process, so each write has its own and
/// a version kept at one never blocks a later write.
static WRITES: AtomicU64 = AtomicU64::new(0);

/// [`write`], with `swap` choosing how the new text goes in place and
/// `hook` called at each [`Moment`].
#[doc(hidden)]
pub fn write_with(
    edit: &Edit,
    swap: Swap,
    mut hook: impl FnMut(Moment, &Path),
) -> Result<PathBuf, Refusal> {
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
    if let Err(why) = replaceable(&real, &meta, rustix::process::geteuid().as_raw()) {
        return Err(Refusal::ReadOnly {
            file: edit.file.clone(),
            why,
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
    let (tmp, mut t) = match temp_beside(&dir, &name, meta.mode() & 0o7777) {
        Ok(t) => t,
        Err((_, e))
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
        Err((tmp, e)) => return Err(io(&tmp, e)),
    };
    // Created with our group, or the directory's (setgid): the file's
    // own group is kept, as an owner may give it, or the edit refused.
    let regroup = t.metadata().map_err(|e| io(&tmp, e)).and_then(|m| {
        if m.gid() == meta.gid() {
            return Ok(());
        }
        let gid = rustix::fs::Gid::from_raw(meta.gid());
        rustix::fs::fchown(&t, None, Some(gid)).map_err(|e| Refusal::ReadOnly {
            file: edit.file.clone(),
            why: format!(
                "its group ({}) cannot be kept on the rewritten file: {e}",
                meta.gid()
            ),
        })
    });
    if let Err(r) = regroup {
        drop(t);
        let _ = fs::remove_file(&tmp);
        return Err(r);
    }
    let written = t
        .write_all(&out)
        .and_then(|()| t.set_permissions(fs::Permissions::from_mode(meta.mode() & 0o7777)))
        .and_then(|()| t.sync_all())
        .and_then(|()| t.metadata());
    drop(t);
    let ours = match written {
        Ok(m) => m,
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            return Err(io(&tmp, e));
        }
    };
    hook(Moment::Before, &real);
    // The file that was hashed, holding what was hashed (not checked
    // through a link: the swap moves names, not targets).
    let hashed = |p: &Path| {
        fs::symlink_metadata(p)
            .ok()
            .filter(|m| m.dev() == meta.dev() && m.ino() == meta.ino())
            .and_then(|_| fs::read(p).ok())
            .is_some_and(|now| hash(&now) == edit.hash)
    };
    let exchanged = match swap {
        Swap::Rename => false,
        Swap::Auto | Swap::Exchange => match exchange(&tmp, &real) {
            Ok(()) => true,
            Err(e)
                if swap == Swap::Auto
                    && (e == rustix::io::Errno::INVAL || e == rustix::io::Errno::NOSYS) =>
            {
                // No exchange on this filesystem: a checked rename.
                false
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                // `RENAME_EXCHANGE` needs both names: an editor that
                // moves the file to a backup and then writes it anew
                // leaves no file for a moment, which is a change, as the
                // check before a plain rename finds it (edges-r2-4).
                // `tmp` was just made, so `ENOENT` already says the file
                // was missing at the exchange; a second look afterwards
                // could find the editor's new file and call it an I/O
                // error (edges-r3-1).
                if e == rustix::io::Errno::NOENT {
                    hook(Moment::Missing, &real);
                    return Err(Refusal::Changed(edit.file.clone()));
                }
                return Err(io(&real, e.into()));
            }
        },
    };
    if exchanged {
        // The new text is in place; what it took out of place is at
        // `tmp`, and is checked only now, so a save at any moment before
        // the swap (renamed into place, or written in place) is found.
        hook(Moment::Window, &tmp);
        if hashed(&tmp) {
            let _ = fs::remove_file(&tmp);
            sync_dir(&dir);
            return Ok(real);
        }
        let ours = Version {
            dev: ours.dev(),
            ino: ours.ino(),
            text: out,
        };
        return Err(undo(edit, &real, &tmp, &dir, ours, &mut hook));
    }
    // No exchange: still the file that was hashed, holding what was
    // hashed, just before the rename (an editor's save in the window
    // between this check and the rename is lost).
    if !hashed(&real) {
        let _ = fs::remove_file(&tmp);
        return Err(Refusal::Changed(edit.file.clone()));
    }
    hook(Moment::Window, &real);
    if let Err(e) = fs::rename(&tmp, &real) {
        let _ = fs::remove_file(&tmp);
        return Err(io(&real, e));
    }
    sync_dir(&dir);
    Ok(real)
}

/// A new file beside `name` in `dir`, created with `mode`, at a name no
/// other write of this process has used (one left by another process
/// of the same pid is passed over).
fn temp_beside(
    dir: &Path,
    name: &str,
    mode: u32,
) -> Result<(PathBuf, File), (PathBuf, std::io::Error)> {
    let mut tries = 0;
    loop {
        let n = WRITES.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".{name}.strand-dev-{}-{n}", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)
        {
            Ok(t) => return Ok((tmp, t)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && tries < 64 => tries += 1,
            Err(e) => return Err((tmp, e)),
        }
    }
}

/// A file as it was seen: its inode and its text.
struct Version {
    dev: u64,
    ino: u64,
    text: Vec<u8>,
}

impl Version {
    /// The file at `p` now (not through a link), or why it cannot be read.
    fn of(p: &Path) -> std::io::Result<Version> {
        let m = fs::symlink_metadata(p)?;
        let text = fs::read(p)?;
        Ok(Version {
            dev: m.dev(),
            ino: m.ino(),
            text,
        })
    }

    /// `p` is this file, holding only this text.
    fn at(&self, p: &Path) -> bool {
        fs::symlink_metadata(p).is_ok_and(|m| m.dev() == self.dev && m.ino() == self.ino)
            && fs::read(p).is_ok_and(|now| now == self.text)
    }
}

/// How many exchanges [`undo`] makes before it stops putting a later
/// save in place (an editor saving without pause).
const ROUNDS: usize = 8;

/// (M6) The exchange put the edit at `real`, but the file it took out
/// (now at `tmp`) was not the one hashed: an editor saved meanwhile.
/// The two are swapped back, and what each exchange brings out of
/// `real` is checked against what was expected there (the same inode,
/// holding the same text): the edit, and the edit is refused
/// (`Changed`) and removed. Anything else is a save made since, newer
/// than what was just put in place, so it is exchanged in in turn
/// (edges-r2-2): nothing found before an exchange is trusted after it.
/// When the latest save is in place, the refusal says so and where the
/// save before it is kept (each write has its own temporary name, so it
/// blocks none). Nothing is removed but the edit, checked; a failed
/// exchange is reported by what is at `real` then (edges-r2-3), or as
/// nothing there when the file is gone (edges-r3-2).
fn undo(
    edit: &Edit,
    real: &Path,
    tmp: &Path,
    dir: &Path,
    ours: Version,
    hook: &mut impl FnMut(Moment, &Path),
) -> Refusal {
    let kept = |why: String| Refusal::Io {
        file: edit.file.clone(),
        why,
    };
    let at = tmp.display();
    // What should be at `real`, and so come out of it.
    let mut expect = ours;
    for round in 0..ROUNDS {
        // `tmp` is this write's own name: what is there goes in place.
        let going = Version::of(tmp);
        hook(
            if round == 0 {
                Moment::Back
            } else {
                Moment::Again
            },
            real,
        );
        let failed = match &going {
            Ok(_) => exchange(tmp, real).err().map(|e| e.to_string()),
            Err(e) => Some(e.to_string()),
        };
        if let Some(e) = failed {
            let other = match fs::symlink_metadata(tmp) {
                Ok(_) => format!("is kept at {at}"),
                Err(_) => format!("is gone from {at}"),
            };
            // Nothing at the file (an editor moved it to a backup, or it
            // was removed): no save is there to name (edges-r3-2).
            if fs::symlink_metadata(real).is_err() {
                return kept(if round == 0 {
                    format!(
                        "it changed while the edit was written, and it could not be put back \
                         ({e}): no file is there now (it was moved or removed after it took the \
                         edit), and the version saved meanwhile {other}"
                    )
                } else {
                    format!(
                        "it was saved more than once while the edit was written ({e}): the edit \
                         is not applied, no file is there now (it was moved or removed since), \
                         and a save made meanwhile {other}"
                    )
                });
            }
            let same = expect.at(real);
            return kept(match (round, same) {
                (0, true) => format!(
                    "it changed while the edit was written, and it could not be put back ({e}): \
                     the file holds the edit, and the version saved meanwhile {other}"
                ),
                (0, false) => format!(
                    "it changed while the edit was written, and it could not be put back ({e}): \
                     the edit is not applied, the file holds a save made since, and the version \
                     saved before it {other}"
                ),
                (_, true) => format!(
                    "it was saved more than once while the edit was written, and the latest save \
                     could not be put in place ({e}): the edit is not applied, the file holds the \
                     save before it, and the latest {other}"
                ),
                (_, false) => format!(
                    "it was saved more than once while the edit was written ({e}): the edit is not \
                     applied, the file holds a save made since, and an earlier one {other}"
                ),
            });
        }
        if expect.at(tmp) {
            if round == 0 {
                let _ = fs::remove_file(tmp);
                sync_dir(dir);
                return Refusal::Changed(edit.file.clone());
            }
            sync_dir(dir);
            return kept(format!(
                "it was saved more than once while the edit was written: the edit is not \
                 applied, the file holds the latest save, and the save before it is kept at {at}"
            ));
        }
        // A save made since, now at `tmp`: what just went in is expected
        // back from `real` when it goes in in turn.
        if let Ok(v) = going {
            expect = v;
        }
    }
    sync_dir(dir);
    kept(format!(
        "it kept changing while the edit was written: the edit is not applied, and a version \
         saved meanwhile is kept at {at}"
    ))
}

/// (M6) Swaps the names `a` and `b` atomically (`renameat2` with
/// `RENAME_EXCHANGE`): both must exist.
fn exchange(a: &Path, b: &Path) -> rustix::io::Result<()> {
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        a,
        rustix::fs::CWD,
        b,
        rustix::fs::RenameFlags::EXCHANGE,
    )
}

/// Flushes directory `dir`'s entries (best effort).
fn sync_dir(dir: &Path) {
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

/// Whether user `uid` may replace the file at `real` (with `meta`, its
/// metadata) by renaming a rewritten copy over it, or why not. The
/// rename needs only the directory's write permission, so the file's
/// own is checked here (inspect-r3-2): its mode bits alone say nothing
/// of who may write it.
fn replaceable(real: &Path, meta: &fs::Metadata, uid: u32) -> Result<(), String> {
    if meta.permissions().readonly() {
        return Err("it has no write permission".into());
    }
    // Root may write anything, and the new file would still be root's
    // where the old one was not: the same refusal.
    if meta.uid() != uid {
        return Err(format!(
            "it belongs to user {}, and rewriting it would make it yours",
            meta.uid()
        ));
    }
    if rustix::fs::access(real, rustix::fs::Access::WRITE_OK).is_err() {
        return Err("you cannot write it".into());
    }
    if rustix::fs::getxattr(real, "system.posix_acl_access", &mut [0u8; 0][..]).is_ok() {
        return Err("it has an access ACL, which rewriting it would drop".into());
    }
    Ok(())
}

/// `at` falls between UTF-8 characters of `bytes`.
fn is_boundary(bytes: &[u8], at: usize) -> bool {
    at == bytes.len() || bytes.get(at).is_some_and(|b| (*b as i8) >= -0x40)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (inspect-r3-2) A file whose mode has a write bit but which
    /// another user owns is not replaced: the rename needs only the
    /// directory, and the new file would be ours (a `root:root 0644`
    /// theme in the user's own config directory).
    #[test]
    fn a_file_another_user_owns_is_not_replaceable() {
        let d = std::env::temp_dir().join(format!("strand-dev-owner-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let f = d.join("theme.strand");
        fs::write(&f, "tokens base {}\n").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o644)).unwrap();
        let meta = fs::metadata(&f).unwrap();
        assert_eq!(replaceable(&f, &meta, meta.uid()), Ok(()));
        let other = meta.uid().wrapping_add(1);
        let why = replaceable(&f, &meta, other).unwrap_err();
        assert!(why.contains("belongs to user"), "{why}");
        fs::set_permissions(&f, fs::Permissions::from_mode(0o444)).unwrap();
        let meta = fs::metadata(&f).unwrap();
        let why = replaceable(&f, &meta, meta.uid()).unwrap_err();
        assert!(why.contains("no write permission"), "{why}");
        let _ = fs::remove_dir_all(&d);
    }
}
