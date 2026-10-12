//! (M5 wave 2, S-tools) The `file` element (decisions.md m5-tools, "the
//! `file` element replaces `export template … to …`"): `file "<path>" {
//! template: "<file>"; run: [<argv>] }` or `text: <expr>` in place of
//! `template:`, at the top level (grammar.md `top_file`) or in a tree.
//!
//! The rules checked here: the path is text and never a `.strand` file;
//! exactly one of `template:` and `text:`; `text:` has the template scope
//! (compiled as a template hole, `Template::compile_expr`, never as a
//! prop) and reads tokens the config has; `run:` is a list of text whose
//! program `permit exec` allows; a `when` (an ordinary condition in the
//! enclosing scope) changes those props, the source keeping its kind; no
//! other prop, child or handler.
//!
//! [`templates`] checks literal `template:` paths against their files
//! (`strand check` and the LSP; spec.md 9.7): the file is there, its
//! holes compile and read tokens the config has.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use crate::diagnostic::{Diagnostic, closest};
use crate::hir::{self, ElementKind, Node};
use crate::source::FileId;
use crate::syntax::Span;
use crate::syntax::ast::{self, ItemKind};
use crate::template::Template;
use crate::ty::Ty;

use super::Checker;

/// What a `file` takes, for its errors.
const TAKES: &str = "a `file` takes `template:` (a template file), or `text:`, and `run:`, and \
                     `when` blocks that change them";

/// A literal `template:` path, for [`templates`]: the file it is
/// written in, the literal's span and the path as written.
pub type TemplatePath = (FileId, Span, String);

impl<'a> Checker<'a> {
    /// A `file` element, at the top level or in a tree: an element of
    /// kind `file` whose `text:` props (if any) are kept compiled in
    /// [`Checker::file_texts`], by the value's span, and whose `when`
    /// blocks are its children.
    #[inline(never)]
    pub(super) fn file_element(&mut self, el: &'a ast::Element, span: Span) -> Node {
        self.add_ref(el.kind.span, super::Target::Element("file".into()));
        let node = self.new_node();
        let arg = match &el.arg {
            Some(ast::HeadArg::Positional(e)) => {
                if let ast::ExprKind::String(s) = &e.kind
                    && s.value.ends_with(".strand")
                {
                    self.error(
                        "check::file_target",
                        "a `file` never writes a `.strand` file",
                        e.span,
                        "a config file",
                    )
                    .help = Some("write the output beside the config, not into it".into());
                }
                Some(self.expect(e, &Ty::TEXT, "`file`'s path"))
            }
            Some(ast::HeadArg::Named(n, e)) => {
                self.error(
                    "check::file_path",
                    "a `file`'s path comes first, unnamed",
                    n.span,
                    "named",
                )
                .help = Some("`file \"~/.cache/strand/colors.css\" { … }`".into());
                self.expr(e, None);
                None
            }
            None => {
                self.error(
                    "check::file_path",
                    "a `file` needs the path it writes",
                    el.kind.span,
                    "no path",
                )
                .help = Some("`file \"~/.cache/strand/colors.css\" { template: … }`".into());
                None
            }
        };
        let mut props = Vec::new();
        let mut whens = Vec::new();
        let mut seen = Vec::new();
        // The element's own source: `template` or `text`, and where.
        let mut source: Option<(&'static str, Span)> = None;
        let items = el.block.as_ref().map_or(&[][..], |b| &b.items[..]);
        for item in items {
            match &item.kind {
                ItemKind::Prop(p) => {
                    if let Some(prop) = self.file_prop(p, &mut seen) {
                        if let Some(kind) = source_kind(&prop.name) {
                            if let Some((_, first)) = source {
                                let file = self.file();
                                self.error(
                                    "check::file_source",
                                    "a `file` takes one of `template:` and `text:`",
                                    p.name.span,
                                    "a second source",
                                )
                                .add_secondary(file, first, "the first")
                                .help = Some(
                                    "keep one: a template file, or a `text:` expression".into(),
                                );
                            }
                            source = Some((kind, p.name.span));
                        }
                        props.push(prop);
                    }
                }
                ItemKind::When(w) => whens.push((w, item.span)),
                _ => {
                    self.error(
                        "check::file_item",
                        "a `file` holds its props and `when` blocks only",
                        item.span,
                        "not a prop of `file`",
                    )
                    .help = Some(TAKES.into());
                }
            }
        }
        if source.is_none() {
            self.error(
                "check::file_source",
                "a `file` needs `template:` or `text:`",
                el.kind.span,
                "nothing to write",
            )
            .help = Some(
                "`template: \"colors.css.tmpl\"` (a file beside the config), or `text: …`".into(),
            );
        }
        // `when` blocks: ordinary conditions in the enclosing scope; the
        // source they change keeps the element's kind (exactly one of
        // `template:` and `text:` whichever `when`s apply).
        let mut children = Vec::new();
        for (w, wspan) in whens {
            let cond = self.expect(&w.cond, &Ty::BOOL, "`when`");
            let mut seen = Vec::new();
            let mut wprops = Vec::new();
            for item in &w.body.items {
                let ItemKind::Prop(p) = &item.kind else {
                    self.error(
                        "check::file_item",
                        "a `when` in a `file` holds props only",
                        item.span,
                        "not a prop",
                    )
                    .help = Some(TAKES.into());
                    continue;
                };
                let Some(prop) = self.file_prop(p, &mut seen) else {
                    continue;
                };
                if let (Some(kind), Some((own, at))) = (source_kind(&prop.name), source)
                    && kind != own
                {
                    let file = self.file();
                    self.error(
                        "check::file_source",
                        format!("this `when` gives `{kind}:` to a `file` written with `{own}:`"),
                        p.name.span,
                        format!("not `{own}:`"),
                    )
                    .add_secondary(file, at, "the `file`'s source")
                    .help = Some(format!(
                        "a `when` changes the `file`'s own source: give it another `{own}:`"
                    ));
                    continue;
                }
                wprops.push(prop);
            }
            children.push(Node::When(hir::When {
                cond,
                props: wprops,
                span: wspan,
            }));
        }
        Node::Element(hir::Element {
            node,
            kind: ElementKind::Builtin("file".into()),
            span,
            arg,
            id: None,
            props,
            children,
        })
    }

    /// One prop of a `file` (or of a `when` in it): checked, or `None`
    /// for one it does not have. `seen`: the names this block set
    /// already (a second is an error).
    fn file_prop(&mut self, p: &'a ast::Prop, seen: &mut Vec<String>) -> Option<hir::Prop> {
        let name = p.name.name.as_str();
        if p.two_way.is_some() || p.transition.is_some() || p.block.is_some() {
            self.error(
                "check::file_item",
                format!("`{name}` of a `file` is a plain value"),
                p.name.span,
                "no `<->`, `~` or block here",
            );
        }
        if !matches!(name, "template" | "text" | "run") {
            self.error(
                "check::unknown_prop",
                format!("a `file` has no prop `{name}`"),
                p.name.span,
                "unknown prop",
            )
            .help = Some(TAKES.into());
            return None;
        }
        if seen.iter().any(|s| s == name) {
            self.error(
                "check::duplicate_prop",
                format!("`{name}` is set twice in one block"),
                p.name.span,
                "set again",
            );
        }
        seen.push(name.to_string());
        let value = match name {
            "template" => {
                if let ast::ExprKind::String(s) = &p.value.kind {
                    self.file_templates
                        .push((self.file(), p.value.span, s.value.clone()));
                }
                self.expect(&p.value, &Ty::TEXT, "`template:`")
            }
            "run" => {
                self.file_run(&p.value);
                self.expect(&p.value, &Ty::list(Ty::TEXT), "`run:`")
            }
            // `text:` has the template scope: compiled as a hole, and
            // never read as a prop (`$surface.light()` is a template
            // method, a load error in a prop).
            _ => {
                match Template::compile_expr(self.file(), &p.value) {
                    Ok(t) => {
                        let key = (self.file(), p.value.span);
                        self.file_texts.push((key, Arc::new(t)));
                    }
                    Err(d) => self.diags.extend(d),
                }
                hir::Expr {
                    kind: hir::ExprKind::Null,
                    ty: Ty::Any,
                    span: p.value.span,
                }
            }
        };
        Some(hir::Prop {
            name: name.to_string(),
            span: p.name.span,
            value,
            two_way: false,
            transition: None,
            sub: Vec::new(),
            inherited: false,
        })
    }

    /// `run:`'s program against `permit exec`, as a `from poll` source's.
    fn file_run(&mut self, e: &'a ast::Expr) {
        let program = super::collect::first_program(e);
        let allowed = self.permits.iter().any(|p| match (p, program.as_deref()) {
            (None, _) => true,
            (Some(list), Some(prog)) => list.iter().any(|x| x == prog),
            (Some(_), None) => false,
        });
        if !allowed {
            let prog = program.as_deref().unwrap_or("the command");
            self.error(
                "check::no_permit",
                format!("running `{prog}` needs `permit exec`"),
                e.span,
                "not permitted",
            )
            .help = Some(format!("add `permit exec \"{prog}\"` at the top level"));
        }
    }
}

/// `template` or `text`: the prop names a `file`'s source.
fn source_kind(name: &str) -> Option<&'static str> {
    match name {
        "template" => Some("template"),
        "text" => Some("text"),
        _ => None,
    }
}

/// The `text:` holes' token reads against the tokens the config has
/// (`tokens`, every path it can read): an unknown token is the error a
/// prop's would be (a `$palette.<name>` extra is left to run time).
pub(super) fn text_tokens(
    texts: &[((FileId, Span), Arc<Template>)],
    tokens: &BTreeMap<String, Ty>,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for ((file, _), t) in texts {
        for (path, span) in t.token_paths() {
            if tokens.contains_key(&path) {
                continue;
            }
            let mut d =
                Diagnostic::error("check::unknown_token", format!("unknown token `${path}`"))
                    .with_label_in(*file, span, "not a token");
            if let Some(s) = closest(&path, tokens.keys().map(String::as_str)) {
                d.suggest(span, format!("${s}"));
            }
            out.push(d);
        }
    }
    out
}

/// The literal `template:` paths of `program` checked against their
/// files (spec.md 9.7: "`strand check` checks literal template paths"):
/// the file can be read, its holes compile, and they read tokens the
/// config has. `~/` is the home directory and a relative path is under
/// `config_dir` (not checked without one), as at run time. The template
/// is not one of the config's sources, so each problem is reported on
/// the `template:` value, the template's own line and column in its
/// message. It touches the disk, so it is not part of
/// [`crate::compile`]: `strand check` and the LSP run it, and `strand
/// run` reports a template's errors on its output instead (the template
/// is watched there, and a broken one never holds the config back).
pub fn templates(program: &hir::Program, config_dir: Option<&Path>) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut done: Vec<&TemplatePath> = Vec::new();
    for t in &program.file_templates {
        if done.contains(&t) {
            continue;
        }
        done.push(t);
        let (file, span, path) = t;
        let full = if let Some(rest) = path.strip_prefix("~/") {
            match std::env::var_os("HOME") {
                Some(h) => std::path::PathBuf::from(h).join(rest),
                None => continue,
            }
        } else if Path::new(path).is_relative() {
            match config_dir {
                Some(d) => d.join(path),
                None => continue,
            }
        } else {
            std::path::PathBuf::from(path)
        };
        let err = |msg: String, label: &str| {
            Diagnostic::error("check::file_template", msg).with_label_in(*file, *span, label)
        };
        let text = match super::shaders::read_file(&full) {
            Ok(t) => t,
            Err(e) => {
                out.push(
                    err(
                        format!("cannot read the template `{path}`: {e}"),
                        "this template",
                    )
                    .with_help(format!(
                        "a relative path is under the config directory ({})",
                        full.display()
                    )),
                );
                continue;
            }
        };
        let at = |s: Span| {
            let before = &text[..(s.start as usize).min(text.len())];
            let line = before.matches('\n').count() + 1;
            let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
            format!("{path}:{line}:{col}")
        };
        let compiled = match Template::compile(FileId::default(), &text) {
            Ok(c) => c,
            Err(diags) => {
                for d in diags {
                    let where_ = d.primary_span().map_or_else(|| path.clone(), at);
                    out.push(err(
                        format!("the template {where_}: {}", d.message),
                        "in this template",
                    ));
                }
                continue;
            }
        };
        for (token, s) in compiled.token_paths() {
            if program.tokens.contains_key(&token) {
                continue;
            }
            let mut d = err(
                format!("the template {}: unknown token `${token}`", at(s)),
                "in this template",
            );
            if let Some(m) = closest(&token, program.tokens.keys().map(String::as_str)) {
                d = d.with_help(format!("did you mean `${m}`?"));
            }
            out.push(d);
        }
    }
    out
}
