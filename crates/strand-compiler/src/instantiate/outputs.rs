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
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use strand_core::{Error, Memo, Runtime};

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
        let held = Rc::new(Cell::new(false));
        let compiled: Compiled = Rc::default();
        let weak = Rc::downgrade(self);
        let (f, env2, held2, site) = (f.clone(), env.clone(), held.clone(), (e.file, e.span));
        let memo = rt.memo(move |rt| match weak.upgrade() {
            Some(ctx) => Ok(ctx.output_content(rt, &f, &env2, &held2, &compiled, site)),
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
        (file, span): (FileId, Span),
    ) -> Content {
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
                Content::Pending => continue,
            });
        }
        out
    }

    /// (M5) How many `file` outputs are mounted.
    pub fn output_count(&self) -> usize {
        self.ctx.outputs.borrow().len()
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
