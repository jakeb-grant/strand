//! (M5) The theming hub's compiler side (decisions.md m5-tools, "the
//! owner-approved theming design"; architecture.md, "The theming hub"):
//! the palette with no fallback ([`Instance::palette`]), the current
//! token table, and the other mode ([`Instance::other_mode`]), which the
//! `file` outputs, `strand export` over IPC and the offline mount share.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use strand_core::{Error, Memo, Runtime, Scope};
use strand_scene::TokenTable;
use strand_theme::Palette;

use super::{Ctx, Instance};
use crate::hir::{DefId, DefKind};
use crate::lower::{ChunkId, Node, Program};
use crate::vm::value::{Slot, Value};
use crate::vm::{OtherEnv, Vm};

/// The error of a palette still loading (a wallpaper being quantised):
/// an output that reads the other mode waits for it rather than
/// reporting a failure (decisions.md m5-tools, "a template reads the
/// other mode").
pub const PALETTE_PENDING: &str = "the palette is still loading";

/// Whether `e` is [`PALETTE_PENDING`].
pub fn is_pending(e: &Error) -> bool {
    matches!(e, Error::Failed(m) if &**m == PALETTE_PENDING)
}

/// (M5) One hold on an instance's other-mode memo
/// ([`Instance::hold_other_mode`]), tied to the program it was taken on.
#[must_use = "a hold is let go of with `Instance::release_other_mode`"]
#[derive(Debug)]
pub struct OtherModeHold {
    ctx: std::rc::Weak<Ctx>,
    memo: Memo<TokenTable>,
}

impl OtherModeHold {
    /// The memo held: the other mode's token table.
    pub fn memo(&self) -> Memo<TokenTable> {
        self.memo
    }
}

/// The instance's other-mode memo while something holds it.
pub(crate) struct OtherHold {
    scope: Scope,
    memo: Memo<TokenTable>,
    holders: usize,
}

impl OtherHold {
    pub(crate) fn memo(&self) -> Memo<TokenTable> {
        self.memo
    }
}

/// The top-level `let`s whose value reaches `system.dark`, through other
/// `let`s and `fn` bodies (a chunk's reads include the `fn`s it calls),
/// by the program's static read sets.
fn lets_reaching_dark(prog: &Program) -> Vec<(DefId, ChunkId)> {
    let lets: HashMap<DefId, ChunkId> = prog
        .files
        .iter()
        .flat_map(|f| f.items.iter())
        .filter_map(|n| match n {
            Node::Let { def, value } => Some((*def, *value)),
            _ => None,
        })
        .collect();
    let direct = |c: ChunkId| {
        prog.reads(c)
            .services
            .iter()
            .any(|(s, f)| s == "system" && f.as_deref().is_none_or(|f| f == "dark"))
    };
    // To a fixed point: a `let` reaches it when it reads it directly or
    // reads a `let` that does.
    let mut reach: HashSet<DefId> = lets
        .iter()
        .filter(|(_, c)| direct(**c))
        .map(|(d, _)| *d)
        .collect();
    loop {
        let more: Vec<DefId> = lets
            .iter()
            .filter(|(d, c)| {
                !reach.contains(*d) && prog.reads(**c).defs.iter().any(|r| reach.contains(r))
            })
            .map(|(d, _)| *d)
            .collect();
        if more.is_empty() {
            break;
        }
        reach.extend(more);
    }
    let mut out: Vec<(DefId, ChunkId)> = lets
        .into_iter()
        .filter(|(d, _)| reach.contains(d))
        .collect();
    out.sort_by_key(|(d, _)| d.0);
    out
}

impl Ctx {
    /// `use palette` evaluated with no fallback: the built-in palette
    /// only when the program has none; a failing palette is its error
    /// and a pending one [`PALETTE_PENDING`] (with `take_pending`, its
    /// last value when it has one, as the offline export takes on a
    /// timeout). Tracked by whatever computation runs it.
    pub(crate) fn palette_strict(
        self: &Rc<Self>,
        rt: &Runtime,
        take_pending: bool,
    ) -> Result<Palette, Error> {
        let prog = self.vm.prog.clone();
        let Some(c) = prog.use_palette else {
            return self.default_palette(rt);
        };
        let root = self.vm.root.clone();
        match self.vm.eval(rt, c, &root)? {
            Value::Palette(p) => Ok((*p).clone()),
            Value::Async(a) => match (&a.value, a.pending) {
                (Some(Value::Palette(p)), false) => Ok((**p).clone()),
                (Some(Value::Palette(p)), true) if take_pending => Ok((**p).clone()),
                (_, true) => Err(Error::failed(PALETTE_PENDING)),
                (_, false) => Err(Error::failed(match &a.error {
                    Some(e) => format!("`use palette`: {e}"),
                    None => "`use palette` gave no palette".to_string(),
                })),
            },
            other => Err(Error::failed(format!(
                "`use palette` gave {}, not a palette",
                if other.is_null() { "null" } else { "a value" }
            ))),
        }
    }

    /// The other mode's token table: the theme's `use palette` and `use
    /// tokens` evaluated in `env` (`system.dark` negated, the `let`s that
    /// reach it fresh), with no fallback, the overlays applied. Never
    /// remembers or reads the last palette.
    fn other_table(self: &Rc<Self>, rt: &Runtime, env: &Rc<OtherEnv>) -> Result<TokenTable, Error> {
        let vm = self.vm.clone();
        vm.in_other_mode(env, || {
            let p = self.palette_strict(rt, false)?;
            self.table_with(rt, &p)
        })
    }

    /// Takes a hold on the other-mode memo, making it with the first
    /// holder (decisions.md m5-tools: one per instance, held by each
    /// output that reads `.light()`/`.dark()` and by an IPC `export`
    /// request whose template does).
    pub(crate) fn hold_other(self: &Rc<Self>, rt: &Runtime) -> Memo<TokenTable> {
        let mut slot = self.other.borrow_mut();
        if let Some(h) = slot.as_mut() {
            h.holders += 1;
            return h.memo;
        }
        let ctx = self.clone();
        let vm: Rc<Vm> = self.vm.clone();
        // Owned by the instance's root scope, not by whichever output or
        // request makes it: it outlives its first holder.
        let owner = vm.root.owner().filter(|o| rt.exists(*o));
        let make = |rt: &Runtime| {
            rt.scope(|rt| {
                let env = Rc::new(OtherEnv::default());
                let root = vm.root.clone();
                for (d, chunk) in lets_reaching_dark(&vm.prog) {
                    if !matches!(vm.prog.def(d).kind, DefKind::Let) {
                        continue;
                    }
                    let (vm2, env2, root2) = (vm.clone(), env.clone(), root.clone());
                    let m =
                        rt.memo(move |rt| vm2.in_other_mode(&env2, || vm2.eval(rt, chunk, &root2)));
                    rt.set_name(m.id(), format!("{} (other mode)", vm.prog.def(d).name));
                    env.bind(d, Slot::Memo(m));
                }
                let ctx = ctx.clone();
                let m = rt.memo(move |rt| ctx.other_table(rt, &env));
                rt.set_name(m.id(), "tokens (other mode)");
                m
            })
        };
        let (scope, memo) = rt.untrack(|rt| match owner {
            Some(o) => rt.with_owner(o, |rt| make(rt)).unwrap_or_else(|_| make(rt)),
            None => make(rt),
        });
        *slot = Some(OtherHold {
            scope,
            memo,
            holders: 1,
        });
        memo
    }

    /// Lets go of a hold from [`Ctx::hold_other`]: the memo goes with the
    /// last holder.
    pub(crate) fn release_other(&self, rt: &Runtime) {
        let mut slot = self.other.borrow_mut();
        let Some(h) = slot.as_mut() else { return };
        h.holders = h.holders.saturating_sub(1);
        if h.holders == 0 {
            let scope = h.scope;
            *slot = None;
            drop(slot);
            scope.dispose(rt);
        }
    }

    /// How many hold the other-mode memo (tests: dropped after a reply).
    pub(crate) fn other_holders(&self) -> usize {
        self.other.borrow().as_ref().map_or(0, |h| h.holders)
    }
}

impl Instance {
    /// (M5) The palette `use palette` gives, evaluated with no fallback
    /// (architecture.md, "The theming hub"): the built-in palette only
    /// when the program has none; a failing palette is its error, never
    /// the last good or the built-in one. A pending wallpaper gives its
    /// last value if it has one, else [`PALETTE_PENDING`]. The offline
    /// mount's pinned palette (the persisted one) when it has one.
    ///
    /// Evaluated in a scope of its own, disposed before it returns: an
    /// inline `material(image: …)` or `import(…)` holds its file only
    /// while it is read, so a request leaves no cleanup and no hold
    /// behind (review round 3, item 3).
    pub fn palette(&self) -> Result<Palette, Error> {
        if let Some(p) = self.ctx.vm.theme().and_then(|t| t.pinned().cloned()) {
            return Ok(p);
        }
        let ctx = self.ctx.clone();
        let (scope, p) = self
            .rt
            .untrack(|rt| rt.scope(|rt| ctx.palette_strict(rt, true)));
        scope.dispose(&self.rt);
        p
    }

    /// (M5) The current token table, overlays included, as the last tick
    /// resolved it.
    pub fn tokens_now(&self) -> Result<TokenTable, Error> {
        match self.tokens {
            Some(m) => m.get_untracked(&self.rt),
            None => Err(Error::failed("no token table")),
        }
    }

    /// (M5) Holds the other-mode memo (the theme evaluated with
    /// `system.dark` negated, decisions.md m5-tools);
    /// [`Instance::release_other_mode`] lets go. Holds are counted, so a
    /// request and the outputs, or two requests, share one memo and one
    /// evaluation. The hold belongs to the instance's program as it is
    /// now: after a reload it lets go of nothing the new one holds.
    pub fn hold_other_mode(&self) -> OtherModeHold {
        let memo = self.ctx.hold_other(&self.rt);
        OtherModeHold {
            ctx: Rc::downgrade(&self.ctx),
            memo,
        }
    }

    /// (M5) Lets go of `hold`, on the program it was taken on (a reload
    /// since then disposed of that program's memo with it).
    pub fn release_other_mode(&self, hold: OtherModeHold) {
        if let Some(ctx) = hold.ctx.upgrade() {
            ctx.release_other(&self.rt);
        }
    }

    /// (M5) How many hold the other-mode memo now.
    pub fn other_mode_holders(&self) -> usize {
        self.ctx.other_holders()
    }

    /// (M5) The other mode's table, pulled now: `Ok(None)` when the
    /// theme gives the same mode either way (the mode is absent),
    /// [`PALETTE_PENDING`] while its palette loads. Takes and releases a
    /// hold around the pull, so an existing holder's memo is shared.
    pub fn other_table(&self) -> Result<Option<TokenTable>, Error> {
        let memo = self.ctx.hold_other(&self.rt);
        let r = memo.get_untracked(&self.rt);
        self.ctx.release_other(&self.rt);
        let other = r?;
        let current = self.tokens_now()?;
        Ok(other_differs(&current, &other).then_some(other))
    }

    /// (M5) Waits up to `timeout` for running wallpaper quantisations
    /// (the offline export), then ends a tick so their palettes are
    /// read.
    pub fn wait_images(&self, timeout: std::time::Duration) {
        if let Some(theme) = self.ctx.vm.theme() {
            theme.wait_images(&self.rt, timeout);
        }
        let _ = self.flush();
    }
}

/// Whether `other`'s palette is the other mode of `current`'s: its
/// `$palette.dark` differs (otherwise the mode is absent).
pub fn other_differs(current: &TokenTable, other: &TokenTable) -> bool {
    current.palette_value("dark") != other.palette_value("dark")
}
