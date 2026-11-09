//! (M6) Declared actions (decisions.md m6-w2-language, the owner's ruling
//! on owner-testing Gaps 1): `action dispatch(ws: int) ["hyprctl",
//! "dispatch", "workspace", ws]` declares a command a handler runs by
//! calling it (`on click { dispatch(3) }`), and `action hold(profile:
//! text) = HoldProfile` in a `from dbus` service a method of its object
//! (`ppd.hold("performance")`).
//!
//! A command is a list literal of texts and the action's parameters: the
//! program is written out and allowed by `permit exec`, and each
//! parameter is exactly one argument when it runs (never split into
//! words, and no shell stands between strand and the program), so every
//! command a config can run is listed where it is declared and checked
//! with the rest of the config. What the program does with an argument
//! is its own: a shell's `-c` runs it as a script, and an argument
//! starting with `-` may be read as an option. A parameter right after a
//! shell's `-c` is warned about (`check::shell_script`).

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
        // (proc-list) One argument per item, so a list of what one
        // argument carries; never a list of lists or of records.
        Ty::List(item, _) => !matches!(**item, Ty::List(..)) && passable(item, false),
        _ => false,
    }
}

/// (proc-run) The members of an action's run: a field of its block may
/// not take one's name, nor `ready`, which the service serving the block
/// answers for itself (decisions.md proc-language).
const RUN_MEMBERS: &[&str] = &[
    "running",
    "started",
    "error",
    "code",
    "lines",
    "stop",
    crate::schema::READY,
];

/// (proc-run) A key name for a block field that would take a run member's
/// name (`err: text? = error`).
fn renamed(member: &str) -> &'static str {
    match member {
        "error" => "err",
        "code" => "status",
        "lines" => "rows",
        "running" => "busy",
        "started" => "began",
        "stop" => "halt",
        _ => "is_ready",
    }
}

/// (proc-permit) Where the program a command runs is, past a leading
/// `stdbuf` or `env` (spec.md 5.8): one rule, strand-core's, which the
/// runtime also names a failing program by.
pub(crate) use strand_core::command::program_at;

/// (proc-permit) `check::no_permit`'s help for a command whose items are
/// `words` and whose program checked is `program`: what stopped the
/// seeing through when that is `stdbuf` or `env` (never a permit of the
/// wrapper, which would allow any program after it), else the permit.
pub(super) fn permit_help(words: &[Option<&str>], program: &str) -> String {
    strand_core::command::not_seen_through(words)
        .unwrap_or_else(|| format!("add `permit exec \"{program}\"` at the top level"))
}

/// Whether `program` (a name or a path) is a shell that runs its `-c`
/// argument as a script.
fn is_shell(program: &str) -> bool {
    let name = program.rsplit('/').next().unwrap_or(program);
    matches!(
        name,
        "sh" | "bash" | "dash" | "zsh" | "ksh" | "mksh" | "ash" | "fish" | "busybox"
    )
}

/// Whether `arg` is a shell's `-c` (alone or among short options, `-ec`).
fn is_dash_c(arg: &str) -> bool {
    arg.strip_prefix('-')
        .is_some_and(|o| !o.is_empty() && !o.starts_with('-') && o.contains('c'))
}

impl<'a> Checker<'a> {
    /// (action-coalesced) Warn about a call of declared action `d`, which
    /// has no result, in a `for` body or after a call of it that runs
    /// first on every path of the handler: it runs once at a time, and a
    /// call while it runs replaces the one queued (spec.md 5.7), so calls
    /// made together are lost but for the first and the latest.
    pub(super) fn action_coalesced(&mut self, d: DefId, name: &str, span: crate::syntax::Span) {
        let again = self.action_calls.iter().flatten().any(|c| *c == d);
        if let Some(block) = self.action_calls.last_mut() {
            block.push(d);
        }
        let label = if self.ctx.in_loop {
            "called for each item"
        } else if again {
            "called again in this handler"
        } else {
            return;
        };
        self.warning(
            "check::action_coalesced",
            format!(
                "`{name}` has no result, so it runs once at a time: a call while it runs \
                 replaces the one waiting, and only the latest call is sure to run"
            ),
            span,
            label,
        )
        .help = Some(format!(
            "pass everything in one call, a list parameter taking every item (`action \
             {name}(items: [text]) [..., items]`, called as `{name}(xs.map(x => x.name))`), or \
             declare `action {name}(…) -> text [...]` and `await` each call; when only the \
             latest call matters, this is what you want"
        ));
    }

    /// (M6, the review of the owner's ruling) Warn about a parameter that
    /// a shell reads as its script (`["sh", "-c", cmd]`): its value is
    /// run as shell code, which a text from a service or a user may not
    /// be meant to be.
    fn shell_scripts(&mut self, a: &'a ast::ActionDecl, argv: &[ArgvPart], from: usize) {
        let ActionBody::Command(e) = &a.body else {
            return;
        };
        let ast::ExprKind::Array(items) = &e.kind else {
            return;
        };
        for (i, w) in argv.windows(2).enumerate().skip(from) {
            let (flag, p, list) = match w {
                [ArgvPart::Text(flag), ArgvPart::Param(p)] => (flag, p, false),
                [ArgvPart::Text(flag), ArgvPart::Spread(p)] => (flag, p, true),
                _ => continue,
            };
            if !is_dash_c(flag) {
                continue;
            }
            let Some(item) = items.get(i + 1) else {
                continue;
            };
            let name = a.params.get(*p).map_or("?", |p| p.name.name.as_str());
            let help = if list {
                // (proc-list) Its first item would be the script and the
                // rest its `$0`, `$1`…
                format!(
                    "write the script out and pass `{name}` after it, where the script reads \
                     its items as `\"$@\"`: `[\"sh\", \"-c\", \"pacman -Qi -- \\\"$@\\\"\", \"sh\", \
                     {name}]`"
                )
            } else {
                format!(
                    "write the script out and pass `{name}` after it, where the script reads \
                     it as `$1`: `[\"sh\", \"-c\", \"… \\\"$1\\\"\", \"sh\", {name}]`"
                )
            };
            self.warning(
                "check::shell_script",
                format!("`{name}` is run as a shell script"),
                item.span,
                "the shell's script",
            )
            .help = Some(help);
        }
    }

    /// (proc-ok) The statuses an `ok` clause lists, each 0 to 255
    /// (`check::ok_status`), 0 among them (`check::ok_without_zero`).
    pub(super) fn ok_statuses(&mut self, c: &ast::OkClause) -> Vec<i32> {
        let mut out: Vec<i32> = Vec::new();
        for st in &c.statuses {
            // A number that was not whole: the parser reported it.
            let Some(v) = st.value else {
                continue;
            };
            match u8::try_from(v) {
                Ok(b) => {
                    if !out.contains(&i32::from(b)) {
                        out.push(i32::from(b));
                    }
                }
                Err(_) => {
                    self.error(
                        "check::ok_status",
                        format!("no program ends with status {v}"),
                        st.span,
                        "not 0 to 255",
                    )
                    .help = Some(
                        "an exit status is 0 to 255 (a shell's `$?`); a program killed by a \
                         signal is never ok"
                            .into(),
                    );
                }
            }
        }
        let whole = c.statuses.iter().all(|s| s.value.is_some());
        if whole && !out.is_empty() && !out.contains(&0) {
            let listed: Vec<String> = out.iter().map(i32::to_string).collect();
            self.error(
                "check::ok_without_zero",
                "`ok` lists 0 too",
                c.span,
                "0 is missing",
            )
            .help = Some(format!(
                "`ok 0, {}`: the clause lists every status that is no failure",
                listed.join(", ")
            ));
        }
        if !out.contains(&0) {
            out.push(0);
        }
        out.sort_unstable();
        out
    }

    /// (proc-ok) `ok` where no command's status is read.
    pub(super) fn ok_not_here(&mut self, c: &ast::OkClause, why: &str) {
        self.error(
            "check::ok_not_here",
            "`ok` lists a command's exit statuses, and there is none to read here",
            c.span,
            why,
        )
        .help = Some(
            "`ok` follows an action's command (`action updates() -> [text] \
             [\"checkupdates\"] ok 0, 2`) or a polled command's interval"
                .into(),
        );
    }

    /// (proc-ok, proc-run) An `ok` or a block on a `from dbus` method's
    /// action: neither has a meaning there.
    pub(super) fn method_action_extras(&mut self, a: &ast::ActionDecl) {
        if let Some(c) = &a.ok {
            self.ok_not_here(c, "a D-Bus method answers or fails; it has no exit status");
        }
        if let Some(b) = &a.fields {
            self.error(
                "check::misplaced",
                "a D-Bus method's action has no block of fields",
                b.span,
                "it prints no lines",
            )
            .help = Some("read what it changes as a field of the service".into());
        }
    }

    /// (proc-run) The fields of action `a`'s block, its record's, and its
    /// `line` event (`on backup.line(doc)`), as a `from listen` service's.
    pub(super) fn action_block_fields(&mut self, a: &'a ast::ActionDecl) {
        let Some(b) = &a.fields else {
            return;
        };
        let Some(&id) = self.item_defs.get(&(self.module, a.name.span)) else {
            return;
        };
        let Some(&rid) = self.action_blocks.get(&id) else {
            return;
        };
        let mut fields: Vec<crate::ty::FieldDef> = Vec::new();
        for f in &b.items {
            let name = f.name.name.as_str();
            if RUN_MEMBERS.contains(&name) {
                let ty = self.resolve_type(&f.ty);
                let shown = self.types.show(&ty);
                self.error(
                    "check::reserved_field",
                    format!("`{}.{name}` is its run's already", a.name.name),
                    f.name.span,
                    "a member of every action's run",
                )
                .help = Some(format!(
                    "name the field otherwise and read the key `{name}` with a path: \
                     `{}: {shown} = {name}`",
                    renamed(name)
                ));
                continue;
            }
            if fields.iter().any(|g| g.name == name) {
                self.error(
                    "check::redeclared",
                    format!("field `{name}` is declared twice"),
                    f.name.span,
                    "declared again here",
                );
                continue;
            }
            let ty = self.resolve_type(&f.ty);
            fields.push(crate::ty::FieldDef {
                name: name.to_string(),
                ty,
                rw: false,
            });
        }
        let rec = self.types.record_mut(rid);
        rec.fields = fields;
        rec.events = vec![crate::ty::EventDef {
            name: "line".into(),
            params: vec![ParamSig {
                name: "doc".into(),
                ty: Ty::Record(rid),
                has_default: false,
                default: None,
                variadic: false,
            }],
        }];
    }

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
                    if super::tree::holds_dict(&self.types, &ty, &mut Vec::new()) {
                        self.dict_service(&ty, t.span, "an action's parameter");
                    } else if !passable(&ty, dbus) {
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
                            "use `text`, `path`, `int`, `float`, `bool`, an enum or a list of \
                             one of these: each is one argument, as its text (a list one per \
                             item); pass anything else as `json(v)` in a `text`"
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
    /// (flex) With `-> T` (`action calc(q: text) -> text ["qalc", "-t",
    /// q]`) the command's output is its value: the call is an
    /// `Async<T>`, loaded in a binding as `apps.search(q)` is and awaited
    /// in a handler, so it is no action there.
    pub(super) fn action_sig(&mut self, a: &'a ast::ActionDecl) {
        let Some(&id) = self.item_defs.get(&(self.module, a.name.span)) else {
            return;
        };
        let params = self.action_params(&a.params, false);
        let ret = a.ret.as_ref().map(|t| {
            let ty = self.resolve_type(t);
            // (flex review) What a `from poll` field may hold: data,
            // records the config declares and the builtin `File` and
            // `Date` (services-round), never a live handle (`Window`,
            // `Canvas`).
            if super::tree::holds_dict(&self.types, &ty, &mut Vec::new()) {
                self.dict_service(&ty, t.span, "a command's output");
                return Ty::Error;
            }
            if !super::tree::readable(&self.types, &ty, &mut Vec::new()) {
                let shown = self.types.show(&ty);
                self.error(
                    "check::type_mismatch",
                    format!("a command's output cannot be read as a `{shown}`"),
                    t.span,
                    "not read from what it prints",
                )
                .help = Some(
                    "use `text` for what it prints as it is, or a number, `bool`, enum, a \
                     `type` you declare, or a list or optional of these, read from its JSON \
                     or `key=value` lines"
                        .into(),
                );
                return Ty::Error;
            }
            ty
        });
        let sig = match ret {
            Some(t) => FnSig::new(params, Ty::Async(Box::new(t))),
            None => {
                let mut sig = FnSig::new(params, Ty::Unit);
                sig.action = true;
                sig
            }
        };
        self.defs[id.0 as usize].ty = Ty::Fn(Arc::new(sig));
    }

    /// The command of top-level action `id`: texts and parameters, the
    /// program written out and allowed by `permit exec`.
    pub(super) fn action_decl(&mut self, id: DefId, a: &'a ast::ActionDecl) -> hir::ActionDecl {
        let params: Vec<String> = a.params.iter().map(|p| p.name.name.clone()).collect();
        // (proc-list) Which parameters are lists: each spreads its items.
        let lists: Vec<bool> = match &self.defs[id.0 as usize].ty {
            Ty::Fn(sig) => sig
                .params
                .iter()
                .map(|p| matches!(p.ty, Ty::List(..)))
                .collect(),
            _ => Vec::new(),
        };
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
                                    argv.push(if lists.get(p) == Some(&true) {
                                        ArgvPart::Spread(p)
                                    } else {
                                        ArgvPart::Param(p)
                                    });
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
        // (proc-permit) The program past `stdbuf`/`env`; any of the chain
        // a permit names allows it (`permit exec "env"` allows any).
        let words: Vec<Option<&str>> = argv
            .iter()
            .map(|p| match p {
                ArgvPart::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        let at = program_at(&words);
        let chain: Vec<String> = words[..argv.len().min(at + 1)]
            .iter()
            .flatten()
            .map(|w| (*w).to_string())
            .collect();
        if let Some(ArgvPart::Text(program)) = argv.get(at) {
            let program = program.clone();
            let allowed = self.permits.iter().any(|p| match p {
                None => true,
                Some(list) => chain.iter().any(|w| list.contains(w)),
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
                .help = Some(permit_help(&words, &program));
                // Never run, as any other refused command.
                ok = false;
            }
            if ok && is_shell(&program) {
                self.shell_scripts(a, &argv, at);
            }
        }
        let statuses = match &a.ok {
            Some(c) => self.ok_statuses(c),
            None => vec![0],
        };
        // (proc-run) The block: fields its stdout lines fill, on an action
        // without a result alone.
        let fields = match (&a.fields, self.action_blocks.get(&id).copied()) {
            (Some(b), Some(rid)) => {
                let declared: Vec<&'a ast::Field> = b
                    .items
                    .iter()
                    .filter(|f| !RUN_MEMBERS.contains(&f.name.name.as_str()))
                    .collect();
                let fields = self.source_fields("action", Some(rid), &declared);
                Some(hir::ActionBlock {
                    record: rid,
                    fields,
                })
            }
            (Some(b), None) if a.ret.is_some() => {
                self.error(
                    "check::block_with_result",
                    "an action with a result reads its output whole, as its `-> T`",
                    b.span,
                    "its lines are not fields",
                )
                .help = Some(
                    "drop the `-> T` to follow its lines as fields, or drop the block and read \
                     what it prints as a `type` you declare"
                        .into(),
                );
                None
            }
            _ => None,
        };
        if ok {
            for (i, p) in a.params.iter().enumerate() {
                if !argv.contains(&ArgvPart::Param(i)) && !argv.contains(&ArgvPart::Spread(i)) {
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
            ok: statuses,
            fields,
        }
    }
}
