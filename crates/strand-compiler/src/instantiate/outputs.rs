//! (M5 wave 2, S-tools) `file` outputs (decisions.md m5-tools, "the `file`
//! element"; architecture.md, "The theming hub"): each mounted `file` is
//! a content memo with equality cut-off over its path, its template (a
//! watched file, or the `text:` hole), its `run:` argv, the current token
//! table and, when its template reads `.light()`/`.dark()`, the held
//! other-mode memo. Nothing watches the memos: [`Instance::take_outputs`]
//! pulls them after the tick's diff is sent, so the swap's work up to the
//! diff is unchanged and the outputs never see a spring's in-between
//! values, and hands the changed ones to the binary's writer.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use strand_core::{Error, Memo, NodeId, Runtime};

use super::theme::{is_pending, other_differs};
use super::{Ctx, Instance};
use crate::diagnostic::Diagnostic;
use crate::lower::{Element, FileOutput, FilePart};
use crate::source::FileId;
use crate::syntax::Span;
use crate::template::Template;
use crate::vm::Env;
use crate::vm::value::Value;

/// What an output holds now.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Content {
    /// A palette it reads is still loading: nothing is written.
    Pending,
    /// (live-files) A service it reads has not answered (`ready` false):
    /// nothing is written, and its `run:` does not run.
    Waiting {
        path: Option<PathBuf>,
        services: Rc<[Rc<str>]>,
    },
    /// (live-files) Its `text:` is `null`: the file is left as it is.
    Leave { path: PathBuf },
    Ready {
        path: PathBuf,
        bytes: Rc<[u8]>,
        run: Option<Rc<[String]>>,
    },
    Failed {
        path: Option<PathBuf>,
        diags: Rc<[Diagnostic]>,
        /// The template file's name and text, which `diags` point into
        /// (`None`: they point into the config).
        template: Option<(Rc<str>, Rc<str>)>,
    },
}

/// A mounted `file`.
pub(crate) struct Slot {
    id: u64,
    file: FileId,
    span: Span,
    memo: Memo<Content>,
    /// What [`Instance::take_outputs`] last handed out.
    last: RefCell<Option<Content>>,
}

/// A `file` output whose content changed since the last
/// [`Instance::take_outputs`].
#[derive(Clone, Debug, PartialEq)]
pub struct OutputChange {
    /// The target (`None`: its path could not be had).
    pub path: Option<PathBuf>,
    /// Where the `file` is written in the config.
    pub file: FileId,
    pub span: Span,
    pub content: OutputContent,
}

#[derive(Clone, Debug, PartialEq)]
pub enum OutputContent {
    /// Write these bytes, then (debounced) run `run`.
    Write {
        bytes: Vec<u8>,
        run: Option<Vec<String>>,
    },
    /// It cannot be rendered: nothing is written, the last good file
    /// stays. The diagnostics point into the template file (its name and
    /// text given) or else into the config's sources.
    Failed {
        diags: Vec<Diagnostic>,
        template: Option<(String, String)>,
    },
    /// (live-files) A service it reads has not answered: nothing is
    /// written until it has (spec.md 9.7, "Pending").
    Waiting { services: Vec<String> },
    /// (live-files) Its `text:` is `null`: nothing is written or run, and
    /// bytes not yet written there are let go.
    Leave,
}

/// Where a mounted `file` is written in the config, and (live-files) the
/// services it waits for.
struct Site {
    file: FileId,
    span: Span,
    waits: Rc<[Rc<str>]>,
}

/// A compiled template file, kept while its text is unchanged.
type Compiled = Rc<RefCell<Option<(PathBuf, Rc<str>, Arc<Template>)>>>;

impl Ctx {
    /// Mounts `e`, a `file` element: its content memo, registered until
    /// its scope goes.
    pub(crate) fn mount_output(
        self: &Rc<Self>,
        rt: &Runtime,
        e: &Element,
        f: &Arc<FileOutput>,
        env: &Rc<Env>,
    ) {
        let id = self.next_output.get();
        self.next_output.set(id + 1);
        // (live-files) The services its chunks read: it waits for each to
        // answer (lowering's per-`file` frame).
        let waits: Rc<[Rc<str>]> = e
            .services
            .iter()
            .flat_map(|s| s.iter())
            .filter(|(_, f)| f.is_none())
            .map(|(s, _)| Rc::from(s.as_str()))
            .collect();
        let held = Rc::new(Cell::new(false));
        let compiled: Compiled = Rc::default();
        let weak = Rc::downgrade(self);
        let site = Site {
            file: e.file,
            span: e.span,
            waits,
        };
        let (f, env2, held2) = (f.clone(), env.clone(), held.clone());
        let memo = rt.memo(move |rt| match weak.upgrade() {
            Some(ctx) => Ok(ctx.output_content(rt, &f, &env2, &held2, &compiled, &site)),
            None => Err(Error::failed("the instance is gone")),
        });
        rt.set_name(memo.id(), "file");
        self.outputs.borrow_mut().push(Slot {
            id,
            file: e.file,
            span: e.span,
            memo,
            last: RefCell::default(),
        });
        let weak = Rc::downgrade(self);
        rt.on_cleanup(move || {
            if let Some(ctx) = weak.upgrade() {
                ctx.outputs.borrow_mut().retain(|s| s.id != id);
                // Released at the next take: the runtime is disposing.
                if held.get() {
                    ctx.releases.set(ctx.releases.get() + 1);
                }
            }
        });
    }

    fn output_content(
        self: &Rc<Self>,
        rt: &Runtime,
        f: &FileOutput,
        env: &Rc<Env>,
        held: &Cell<bool>,
        compiled: &Compiled,
        site: &Site,
    ) -> Content {
        let Site { file, span, waits } = site;
        let (file, span) = (*file, *span);
        let fail = |path: Option<PathBuf>, msg: String| Content::Failed {
            path,
            diags: Rc::from(vec![Diagnostic::error("file::output", msg).with_label_in(
                file,
                span,
                "this `file`",
            )]),
            template: None,
        };
        let text = |chunk: Option<crate::lower::ChunkId>, what: &str| -> Result<String, String> {
            let Some(c) = chunk else {
                return Err(format!("no {what}"));
            };
            match self.vm.eval(rt, c, env) {
                Ok(v) => v
                    .as_text()
                    .map(str::to_string)
                    .ok_or_else(|| format!("its {what} is not text")),
                Err(e) => Err(format!("its {what}: {e}")),
            }
        };
        // (live-files) A service it reads that has not answered: its
        // values are defaults, so nothing is written (even with `??`).
        let waiting: Vec<Rc<str>> = waits
            .iter()
            .filter(|s| !self.vm.host.ready(rt, s))
            .cloned()
            .collect();
        if !waiting.is_empty() {
            let path = text(f.path, "path")
                .ok()
                .and_then(|p| self.storage.resolve(&p));
            return Content::Waiting {
                path,
                services: waiting.into(),
            };
        }
        let path = match text(f.path, "path") {
            Ok(p) => match self.storage.resolve(&p) {
                Some(p) => p,
                None => {
                    return fail(
                        None,
                        format!("`{p}` is relative and there is no config dir"),
                    );
                }
            },
            Err(e) => return fail(None, e),
        };
        // Of the props that apply (the element's own, and those of the
        // `when`s whose condition holds), the last of each kind wins.
        let mut source = None;
        let mut run_chunk = None;
        for p in &f.props {
            if let Some(c) = p.when {
                match self.vm.eval(rt, c, env) {
                    Ok(Value::Bool(true)) => {}
                    Ok(_) => continue,
                    Err(e) => return fail(Some(path), format!("a `when`: {e}")),
                }
            }
            match &p.part {
                FilePart::Run(c) => run_chunk = Some(*c),
                part => source = Some(part),
            }
        }
        let run = match run_chunk {
            None => None,
            Some(c) => match self.vm.eval(rt, c, env) {
                Ok(v) => match v.as_list() {
                    Some(items) => Some(
                        items
                            .iter()
                            .filter_map(|i| i.as_text().map(str::to_string))
                            .collect::<Rc<[String]>>(),
                    ),
                    None if matches!(v, Value::Null) => None,
                    None => return fail(Some(path), "its `run:` is not a list".into()),
                },
                Err(e) => return fail(Some(path), format!("its `run:`: {e}")),
            },
        };
        let (template, source) = match source {
            Some(FilePart::Value(c)) => {
                return self.value_content(rt, *c, env, path, run, (file, span));
            }
            Some(FilePart::Text(t)) => (t.clone(), None),
            Some(FilePart::Template(c)) => {
                let name = match text(Some(*c), "template") {
                    Ok(n) => n,
                    Err(e) => return fail(Some(path), e),
                };
                let Some(at) = self.storage.resolve(&name) else {
                    return fail(Some(path), format!("no config dir to find `{name}` in"));
                };
                let read = match self.vm.theme() {
                    Some(theme) => theme.read_template(rt, &at),
                    None => std::fs::read_to_string(&at),
                };
                let src: Rc<str> = match read {
                    Ok(s) => Rc::from(s),
                    Err(e) => {
                        return fail(
                            Some(path),
                            format!("cannot read the template {}: {e}", at.display()),
                        );
                    }
                };
                let shown: Rc<str> = Rc::from(at.display().to_string());
                let kept = compiled
                    .borrow()
                    .as_ref()
                    .filter(|(p, s, _)| *p == at && *s == src)
                    .map(|(_, _, t)| t.clone());
                let t = match kept {
                    Some(t) => t,
                    None => match Template::compile(FileId::default(), &src) {
                        Ok(t) => {
                            let t = Arc::new(t);
                            *compiled.borrow_mut() = Some((at, src.clone(), t.clone()));
                            t
                        }
                        Err(d) => {
                            return Content::Failed {
                                path: Some(path),
                                diags: Rc::from(d),
                                template: Some((shown, src)),
                            };
                        }
                    },
                };
                (t, Some((shown, src)))
            }
            _ => return fail(Some(path), "no `template:` or `text:`".into()),
        };
        // The current palette with no fallback: a wallpaper still
        // quantising writes nothing, a broken palette is this output's
        // error (the screen keeps its last good palette).
        if let Err(e) = self.palette_ok(rt) {
            return match is_pending(&e) {
                true => Content::Pending,
                false => fail(Some(path), format!("no palette: {e}")),
            };
        }
        if self.vm.theme().is_some_and(|t| t.images_pending(rt)) {
            return Content::Pending;
        }
        let current = match self.tokens_memo.get().map(|m| m.get(rt)) {
            Some(Ok(t)) => t,
            Some(Err(e)) => return fail(Some(path), format!("no token table: {e}")),
            None => return Content::Pending,
        };
        let other = if template.reads_other_mode() {
            if !held.get() {
                self.hold_other(rt);
                held.set(true);
            }
            let memo = self.other.borrow().as_ref().map(|h| h.memo());
            match memo.map(|m| m.get(rt)) {
                Some(Ok(o)) => other_differs(&current, &o).then_some(o),
                Some(Err(e)) if is_pending(&e) => return Content::Pending,
                Some(Err(e)) => {
                    return fail(Some(path), format!("the other mode has no palette: {e}"));
                }
                None => None,
            }
        } else {
            if held.replace(false) {
                self.releases.set(self.releases.get() + 1);
            }
            None
        };
        match template.render(&current, other.as_ref()) {
            Ok(bytes) => Content::Ready {
                path,
                bytes: Rc::from(bytes),
                run,
            },
            Err(d) => Content::Failed {
                path: Some(path),
                diags: Rc::from(d),
                template: source,
            },
        }
    }
}

impl Ctx {
    /// (live-files) A `text:` that reads the scope: its value, written by
    /// spec.md 9.8's Values rule; `null` leaves the file; a token is
    /// resolved against the current table (waiting, as a template does,
    /// while the palette loads).
    fn value_content(
        self: &Rc<Self>,
        rt: &Runtime,
        chunk: crate::lower::ChunkId,
        env: &Rc<Env>,
        path: PathBuf,
        run: Option<Rc<[String]>>,
        (file, span): (FileId, Span),
    ) -> Content {
        let fail = |path: PathBuf, msg: String| Content::Failed {
            path: Some(path),
            diags: Rc::from(vec![Diagnostic::error("file::output", msg).with_label_in(
                file,
                span,
                "this `file`",
            )]),
            template: None,
        };
        let v = match self.vm.eval(rt, chunk, env) {
            Ok(Value::Async(a)) => match (&a.value, a.pending, &a.error) {
                (Some(v), _, _) => v.clone(),
                (None, true, _) => return Content::Pending,
                (None, false, Some(e)) => return fail(path, format!("its `text:`: {e}")),
                (None, false, None) => Value::Null,
            },
            Ok(v) => v,
            Err(e) => return fail(path, format!("its `text:`: {e}")),
        };
        let written = match &v {
            Value::Null => return Content::Leave { path },
            Value::Token(expr) => {
                if let Err(e) = self.palette_settled(rt) {
                    return match is_pending(&e) {
                        true => Content::Pending,
                        false => fail(path, format!("no palette: {e}")),
                    };
                }
                let value = match self.tokens_memo.get().map(|m| m.get(rt)) {
                    Some(Ok(table)) => table.eval(expr),
                    Some(Err(e)) => return fail(path, format!("no token table: {e}")),
                    None => return Content::Pending,
                };
                match value {
                    Some(pv) => crate::template::write_value(&pv, span).map_err(|d| d.message),
                    None => Err("its token has no value".to_string()),
                }
            }
            v => {
                // (build-ready) A text that read a token on its way (`join`,
                // `hex`, a `let` of one) waits for the palette as a template
                // does: the table over the fallback palette would write the
                // file once with the fallback's colours, and run its `run:`.
                if let Err(e) = rt.untrack(|rt| self.palette_settled(rt))
                    && self.read_the_palette(rt)
                {
                    // Read again tracked, so the content is worked out once
                    // the palette is (even when its table is the same).
                    let _ = self.palette_settled(rt);
                    return match is_pending(&e) {
                        true => Content::Pending,
                        false => fail(path, format!("no palette: {e}")),
                    };
                }
                value_text(&self.vm.prog.types, v)
            }
        };
        match written {
            Ok(text) => Content::Ready {
                path,
                bytes: Rc::from(text.into_bytes()),
                run,
            },
            Err(why) => fail(path, format!("its `text:`: {why}")),
        }
    }
}

/// A value as spec.md 9.8's Values rule writes it: text as is, a number
/// in shortest form (at most three decimals, no unit but `%` and `ch`),
/// a bool, a colour as `hex(c)`, an enum by its case; anything else is
/// an error naming its kind.
fn value_text(types: &crate::ty::TypeTable, v: &Value) -> Result<String, String> {
    use crate::vm::value::Num;
    use strand_theme::export::number;
    Ok(match v {
        Value::Text(t) => t.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Num(x, Num::Percent) => format!("{}%", number(*x)),
        Value::Num(x, Num::Ch) => format!("{}ch", number(*x)),
        Value::Num(x, _) => number(*x),
        Value::Color(c) => {
            if crate::template::translucent(*c) {
                return Err(
                    "the colour is translucent, and `hex` writes it opaque only \
                            when asked: write `hex(c, alpha: true)` or `argb(c)`"
                        .into(),
                );
            }
            crate::template::hex(*c, false, true)
        }
        Value::Enum(e, i) => types
            .enums
            .get(e.0 as usize)
            .and_then(|d| d.variants.get(*i as usize))
            .map(|v| v.to_string())
            .ok_or("an unknown enum case")?,
        other => {
            let kind = match other {
                Value::List(_) | Value::Commas(_) | Value::Spaced(_) => "a list",
                Value::Record(_) => "a record",
                Value::Dict(_) => "a dictionary",
                Value::Time(_) => "a value that changes with time",
                Value::Fn(_) => "a function",
                _ => "this value",
            };
            return Err(format!(
                "{kind} has no one text form; write the text it should be (`join`, `num`, `hex`)"
            ));
        }
    })
}

impl Ctx {
    /// (build-ready) Whether what the running `file` content has read so
    /// far reaches the token table or the palette, through any memo
    /// between (a `let` another reader already worked out is read from
    /// its memo, never through the token hook, so counting hook calls
    /// would miss it).
    fn read_the_palette(&self, rt: &Runtime) -> bool {
        let targets: Vec<NodeId> = [
            self.tokens_memo.get().map(|m| m.id()),
            self.palette_memo.get().map(|m| m.id()),
        ]
        .into_iter()
        .flatten()
        .collect();
        if targets.is_empty() {
            return false;
        }
        let mut seen = HashSet::new();
        let mut todo = rt.reads_so_far();
        while let Some(id) = todo.pop() {
            if targets.contains(&id) {
                return true;
            }
            if seen.insert(id)
                && let Ok(sources) = rt.sources(id)
            {
                todo.extend(sources);
            }
        }
        false
    }

    /// (build-ready) [`Ctx::palette_ok`], and no wallpaper still being
    /// quantised (while one is, `??` gives the screen a fallback and the
    /// table is made over it): what a template waits for, as one result.
    fn palette_settled(self: &Rc<Self>, rt: &Runtime) -> Result<(), Error> {
        self.palette_ok(rt)?;
        match self.vm.theme().is_some_and(|t| t.images_pending(rt)) {
            true => Err(Error::failed(super::theme::PALETTE_PENDING)),
            false => Ok(()),
        }
    }

    /// Whether the current palette can be had with no fallback (else its
    /// error, or [`super::theme::PALETTE_PENDING`]): one memo every
    /// output reads, so an inline `material(seed: …)` is solved once per
    /// change, not once per output. Made with the first output, owned by
    /// the instance's root scope.
    fn palette_ok(self: &Rc<Self>, rt: &Runtime) -> Result<(), Error> {
        let memo = match self.palette_ok.get() {
            Some(m) => m,
            None => {
                let weak = Rc::downgrade(self);
                let make = move |rt: &Runtime| {
                    let weak = weak.clone();
                    let m = rt.memo(move |rt| match weak.upgrade() {
                        Some(ctx) => ctx.palette_strict(rt, false).map(|_| ()),
                        None => Err(Error::failed("the instance is gone")),
                    });
                    rt.set_name(m.id(), "palette (no fallback)");
                    m
                };
                let owner = self.vm.root.owner().filter(|o| rt.exists(*o));
                let m = rt.untrack(|rt| match owner {
                    Some(o) => rt.with_owner(o, &make).unwrap_or_else(|_| make(rt)),
                    None => make(rt),
                });
                self.palette_ok.set(Some(m));
                m
            }
        };
        memo.get(rt)
    }
}

impl Instance {
    /// (M5) The `file` outputs whose content changed since the last call,
    /// pulled now (call it after the tick's diff is sent). A pending one
    /// is left for later; two mounted `file`s writing one path are both
    /// failures naming each other.
    pub fn take_outputs(&self) -> Vec<OutputChange> {
        let ctx = &self.ctx;
        for _ in 0..ctx.releases.replace(0) {
            ctx.release_other(&self.rt);
        }
        let slots: Vec<(u64, Memo<Content>, FileId, Span)> = ctx
            .outputs
            .borrow()
            .iter()
            .map(|s| (s.id, s.memo, s.file, s.span))
            .collect();
        if slots.is_empty() {
            return Vec::new();
        }
        let mut now: Vec<(u64, FileId, Span, Content)> = slots
            .into_iter()
            .map(|(id, m, file, span)| {
                let c = m
                    .get_untracked(&self.rt)
                    .unwrap_or_else(|e| Content::Failed {
                        path: None,
                        diags: Rc::from(vec![
                            Diagnostic::error("file::output", e.to_string()).with_label_in(
                                file,
                                span,
                                "this `file`",
                            ),
                        ]),
                        template: None,
                    });
                (id, file, span, c)
            })
            .collect();
        // Rendering may have taken or let go of other-mode holds.
        for _ in 0..ctx.releases.replace(0) {
            ctx.release_other(&self.rt);
        }
        twice(&mut now);
        let mut out = Vec::new();
        let outputs = ctx.outputs.borrow();
        for (id, file, span, c) in now {
            if c == Content::Pending {
                continue;
            }
            let Some(slot) = outputs.iter().find(|s| s.id == id) else {
                continue;
            };
            // Unchanged: the memo hands back the same `Rc`s, compared by
            // pointer first.
            if slot.last.borrow().as_ref() == Some(&c) {
                continue;
            }
            *slot.last.borrow_mut() = Some(c.clone());
            out.push(match c {
                Content::Ready { path, bytes, run } => OutputChange {
                    path: Some(path),
                    file,
                    span,
                    content: OutputContent::Write {
                        bytes: bytes.to_vec(),
                        run: run.map(|r| r.to_vec()),
                    },
                },
                Content::Failed {
                    path,
                    diags,
                    template,
                } => OutputChange {
                    path,
                    file,
                    span,
                    content: OutputContent::Failed {
                        diags: diags.to_vec(),
                        template: template.map(|(n, t)| (n.to_string(), t.to_string())),
                    },
                },
                Content::Waiting { path, services } => OutputChange {
                    path,
                    file,
                    span,
                    content: OutputContent::Waiting {
                        services: services.iter().map(|s| s.to_string()).collect(),
                    },
                },
                Content::Leave { path } => OutputChange {
                    path: Some(path),
                    file,
                    span,
                    content: OutputContent::Leave,
                },
                Content::Pending => continue,
            });
        }
        out
    }

    /// (M5) How many `file` outputs are mounted.
    pub fn output_count(&self) -> usize {
        self.ctx.outputs.borrow().len()
    }

    /// (M5) Calls `each` with every path a mounted `file` writes now:
    /// the outputs whose last [`Instance::take_outputs`] change was a
    /// write (one pending since keeps its target), or that wait for a
    /// service (live-files: what was written while it was ready still
    /// lands, so a paced write is not dropped when it turns unready). A
    /// removed `file`, one whose path changed and one that fails have no
    /// target here, so the writer lets go of what it was still trying to
    /// write there (spec.md 9.7, "Removing a `file` stops its updates").
    pub fn output_targets(&self, mut each: impl FnMut(&Path)) {
        for s in self.ctx.outputs.borrow().iter() {
            match s.last.borrow().as_ref() {
                Some(Content::Ready { path, .. })
                | Some(Content::Waiting {
                    path: Some(path), ..
                }) => each(path),
                _ => {}
            }
        }
    }
}

/// Two mounted `file`s with one path: both fail, each naming the other
/// (a `file` in a `bar` on every monitor is one at the same place).
/// Linear in the outputs, cloning a path only for a clash.
fn twice(now: &mut [(u64, FileId, Span, Content)]) {
    let mut first: HashMap<&Path, usize> = HashMap::new();
    // Each output's clash: the other output's index.
    let mut clash: Vec<Option<usize>> = vec![None; now.len()];
    for (i, (_, _, _, c)) in now.iter().enumerate() {
        let Content::Ready { path, .. } = c else {
            continue;
        };
        match first.get(path.as_path()) {
            Some(&j) => {
                clash[i] = Some(j);
                if clash[j].is_none() {
                    clash[j] = Some(i);
                }
            }
            None => {
                first.insert(path.as_path(), i);
            }
        }
    }
    if clash.iter().all(Option::is_none) {
        return;
    }
    let marks: Vec<Option<(FileId, Span, PathBuf)>> = clash
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let j = (*c)?;
            match &now[i].3 {
                Content::Ready { path, .. } => Some((now[j].1, now[j].2, path.clone())),
                _ => None,
            }
        })
        .collect();
    for (slot, c) in now.iter_mut().zip(marks) {
        let Some((file, span, p)) = c else { continue };
        let same = (file, span) == (slot.1, slot.2);
        let d = Diagnostic::error(
            "file::twice",
            format!("`{}` is written from two places", p.display()),
        )
        .with_label_in(
            slot.1,
            slot.2,
            if same {
                "mounted more than once (a `file` in a component used twice, or in a `bar` on every monitor)"
            } else {
                "written here"
            },
        );
        let d = if same {
            d
        } else {
            d.with_secondary_in(file, span, "and here")
        };
        slot.3 = Content::Failed {
            path: Some(p),
            diags: Rc::from(vec![d.with_help(
                "give each its own path, or move the `file` to the top level",
            )]),
            template: None,
        };
    }
}
