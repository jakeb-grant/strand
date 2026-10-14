//! (M6) Declared actions (decisions.md m6-w2-language, the owner's ruling
//! on owner-testing Gaps 1): `action dispatch(ws: int) ["hyprctl",
//! "dispatch", "workspace", ws]` declares a command a handler runs by
//! calling it (`on click { dispatch(3) }`), and `action hold(profile:
//! text) = HoldProfile` in a `from dbus` service a method of its object
//! (`ppd.hold("performance")`).
//!
//! A command is a list literal of texts and the action's parameters: the
//! program is written out and allowed by `permit exec`, and each
//! parameter is exactly one argument when it runs (never split, never
//! read by a shell), so every command a config can run is listed where
//! it is declared and checked with the rest of the config.

use std::sync::Arc;

use super::Checker;
use crate::hir::{self, ArgvPart, DefId};
use crate::syntax::ast::{self, ActionBody};
use crate::ty::{FnSig, ParamSig, Prim, Ty};

/// What an action's parameter may be: a value one argument carries as its
/// text (a command's), or that converts to a D-Bus argument (a method's).
fn passable(ty: &Ty, dbus: bool) -> bool {
    match ty {
        Ty::Error | Ty::Enum(_) => true,
        Ty::Prim(Prim::Text | Prim::Path | Prim::Int | Prim::Float | Prim::Bool) => true,
        Ty::List(item, _) if dbus => matches!(**item, Ty::Prim(Prim::Text)),
        _ => false,
    }
}

impl<'a> Checker<'a> {
    /// An action's parameters, each typed as an argument can carry it.
    /// `dbus`: a method's (a list of texts is an `as` there).
    pub(super) fn action_params(&mut self, params: &[ast::Param], dbus: bool) -> Vec<ParamSig> {
        let mut out: Vec<ParamSig> = Vec::new();
        for p in params {
            if out.iter().any(|q| q.name == p.name.name) {
                self.error(
                    "check::redeclared",
                    format!("parameter `{}` is declared twice", p.name.name),
                    p.name.span,
                    "declared again here",
                );
            }
            let ty = match &p.ty {
                Some(t) => {
                    let ty = self.resolve_type(t);
                    if !passable(&ty, dbus) {
                        let shown = self.types.show(&ty);
                        self.error(
                            "check::type_mismatch",
                            format!("an action's parameter cannot be a `{shown}`"),
                            t.span,
                            "not passed as an argument",
                        )
                        .help = Some(if dbus {
                            "use `text`, `path`, `int`, `float`, `bool`, an enum or `[text]`".into()
                        } else {
                            "use `text`, `path`, `int`, `float`, `bool` or an enum: each is \
                             one argument, as its text"
                                .into()
                        });
                    }
                    ty
                }
                None => {
                    self.error(
                        "check::needs_type",
                        format!("parameter `{}` needs a type", p.name.name),
                        p.name.span,
                        "add `: T`",
                    )
                    .help = Some(format!("`{}: text`", p.name.name));
                    Ty::Error
                }
            };
            if let Some(d) = &p.default {
                self.error(
                    "check::misplaced",
                    "action parameters take no defaults",
                    d.span,
                    "every argument is passed",
                );
            }
            out.push(ParamSig {
                name: p.name.name.clone(),
                ty,
                has_default: false,
                default: None,
                variadic: false,
            });
        }
        out
    }

    /// The signature a handler calls: `dispatch(ws: int)`, an action.
    pub(super) fn action_sig(&mut self, a: &'a ast::ActionDecl) {
        let Some(&id) = self.item_defs.get(&(self.module, a.name.span)) else {
            return;
        };
        let params = self.action_params(&a.params, false);
        let mut sig = FnSig::new(params, Ty::Unit);
        sig.action = true;
        self.defs[id.0 as usize].ty = Ty::Fn(Arc::new(sig));
    }

    /// The command of top-level action `id`: texts and parameters, the
    /// program written out and allowed by `permit exec`.
    pub(super) fn action_decl(&mut self, id: DefId, a: &'a ast::ActionDecl) -> hir::ActionDecl {
        let params: Vec<String> = a.params.iter().map(|p| p.name.name.clone()).collect();
        let mut argv = Vec::new();
        let mut ok = true;
        match &a.body {
            ActionBody::Command(e) => match &e.kind {
                ast::ExprKind::Array(items) if !items.is_empty() => {
                    for (i, item) in items.iter().enumerate() {
                        match &item.kind {
                            ast::ExprKind::String(s) => argv.push(ArgvPart::Text(s.value.clone())),
                            ast::ExprKind::Name(n) if params.contains(&n.name) => {
                                if i == 0 {
                                    self.error(
                                        "check::not_constant",
                                        "an action's program is written out",
                                        item.span,
                                        "a parameter",
                                    )
                                    .help = Some(
                                        "name the program (`[\"hyprctl\", …]`): `permit exec` \
                                         allows programs by name"
                                            .into(),
                                    );
                                    ok = false;
                                } else if let Some(p) = params.iter().position(|p| *p == n.name) {
                                    argv.push(ArgvPart::Param(p));
                                }
                            }
                            _ => {
                                let d = self.error(
                                    "check::not_constant",
                                    "an action's command holds texts and its parameters",
                                    item.span,
                                    "neither a text nor a parameter",
                                );
                                d.help = Some(match &item.kind {
                                    ast::ExprKind::Name(n) => format!(
                                        "`{}` is not a parameter of `{}`; add it: `action {}({}: text)`",
                                        n.name, a.name.name, a.name.name, n.name
                                    ),
                                    _ => "each parameter is one argument as it is: compute \
                                          the value in the handler and pass it"
                                        .into(),
                                });
                                ok = false;
                            }
                        }
                    }
                }
                _ => {
                    self.error(
                        "check::type_mismatch",
                        "an action's command is a list",
                        e.span,
                        "not a list of arguments",
                    )
                    .help = Some("`[\"hyprctl\", \"dispatch\", \"workspace\", ws]`".into());
                    ok = false;
                }
            },
            // Only a service's action calls a method; the parser takes
            // `= Method` there alone.
            ActionBody::Method(_) | ActionBody::Missing => ok = false,
        }
        if let Some(ArgvPart::Text(program)) = argv.first() {
            let program = program.clone();
            let allowed = self.permits.iter().any(|p| match p {
                None => true,
                Some(list) => list.contains(&program),
            });
            if !allowed {
                let shown = if program.is_empty() { "\"\"" } else { &program };
                let span = match &a.body {
                    ActionBody::Command(e) => e.span,
                    _ => a.span,
                };
                self.error(
                    "check::no_permit",
                    format!("running `{shown}` needs `permit exec`"),
                    span,
                    "not permitted",
                )
                .help = Some(format!("add `permit exec \"{program}\"` at the top level"));
            }
        }
        if ok {
            for (i, p) in a.params.iter().enumerate() {
                if !argv.contains(&ArgvPart::Param(i)) {
                    self.warning(
                        "check::unknown_param",
                        format!("parameter `{}` is not in the command", p.name.name),
                        p.name.span,
                        "never passed",
                    )
                    .help = Some(format!(
                        "add `{}` to the list where it goes, or remove the parameter",
                        p.name.name
                    ));
                }
            }
        }
        hir::ActionDecl {
            def: id,
            file: self.file(),
            span: a.span,
            params,
            argv: if ok { argv } else { Vec::new() },
        }
    }
}
