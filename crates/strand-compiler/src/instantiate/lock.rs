//! The lock's runtime semantics (docs/architecture.md, "The lock";
//! decisions.md, m4-lock-w1).
//!
//! The binary passes on what the compositor says about the session lock
//! ([`Instance::set_session_lock`], from `ToLogic::LockState`):
//!
//! - While the session is [`SessionLock::Locked`], the `lock`'s `open`
//!   is held true: a config write of `false` (a handler, `strand set`) is
//!   ignored with a notice, so the content stays shown and live, and the
//!   scene never asks the surface manager to take the lock down (which
//!   it would refuse anyway: only an `UnlockToken` unlocks).
//! - Only an `auth` success unlocks: its token reaches the surface
//!   manager, which reports [`SessionLock::Unlocked`]; the runtime then
//!   writes `open` false through the lock's two-way binding
//!   (`open: <-> locked`), so the state that locked the session says it
//!   is unlocked and a later write of `true` locks again.
//! - [`Instance::lock_shown`] follows these reports: shown while
//!   `Locked`, not shown after `Finished` (the compositor refused or
//!   ended the lock) or `Unlocked`. Between a lock opening and the
//!   compositor's first answer (and in a host with no session lock, such
//!   as the tests and offline tools) a `lock` whose `open` is not false
//!   counts as shown, so a reload never races a lock coming up.
//!
//! The reload exemption itself is `Instance::reload`'s
//! (`EditClass::LockDeferred`; `crates/strand/src/run/tests.rs::
//! lock_edits_wait_for_the_unlock_and_then_land`).
//!
//! [`Instance::set_session_lock`]: super::Instance::set_session_lock
//! [`Instance::lock_shown`]: super::Instance::lock_shown

use std::cell::Cell;

use strand_core::{Runtime, Signal};
use strand_scene::PropValue;

/// The session lock as the compositor reports it (strand-surface's
/// `LockState`, passed on by the binary).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum SessionLock {
    /// Every output shows a lock surface.
    Locked,
    /// The compositor refused the lock or ended it.
    Finished,
    /// An `auth` success released it.
    Unlocked,
}

/// Where the lock is, as far as the compositor has said.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub(crate) enum Phase {
    /// Nothing reported since the lock last opened: its `open` decides.
    #[default]
    Unreported,
    Locked,
    /// `Finished` or `Unlocked` since it last opened.
    Ended,
}

/// The instance's lock state, carried across reloads.
pub(crate) struct LockTrack {
    phase: Cell<Phase>,
    /// True while `Locked`: read by the `lock`'s `open` binding.
    held: Signal<bool>,
    /// The ignored-write notice was given in this lock session.
    warned: Cell<bool>,
}

/// The notice an ignored `open: false` gives, once per lock session.
pub const IGNORED_CLOSE: &str = "`lock`: `open` was set to false while the session is locked; \
     only a password unlocks it, so the lock stays";

impl LockTrack {
    pub(crate) fn new(rt: &Runtime) -> LockTrack {
        LockTrack {
            phase: Cell::new(Phase::Unreported),
            held: rt.signal(false),
            warned: Cell::new(false),
        }
    }

    /// Lets go of its signal (the instance shuts down).
    pub(crate) fn dispose(&self, rt: &Runtime) {
        self.held.dispose(rt);
    }

    pub(crate) fn phase(&self) -> Phase {
        self.phase.get()
    }

    /// The value the `lock`'s `open` binding sends, given what its
    /// expression gave (`out`): `true` while the session is locked,
    /// whatever the config wrote. Read inside the binding's memo, so it
    /// follows the reports. `Some(notice)` the first time a write was
    /// ignored in this lock session.
    pub(crate) fn hold_open(&self, rt: &Runtime, out: PropValue) -> (PropValue, Option<&str>) {
        let held = self.held.get(rt).unwrap_or(false);
        if !held || out == PropValue::Bool(true) {
            return (out, None);
        }
        let notice = (!self.warned.replace(true)).then_some(IGNORED_CLOSE);
        (PropValue::Bool(true), notice)
    }

    /// The lock's `open` turned from false to true: a new request,
    /// which the next report is about. A lock mounted open (a reload
    /// remounting it) is not one: after an unlock or a refusal the
    /// surface manager asks again only once `open` was false, and so
    /// does this.
    pub(crate) fn opened(&self) {
        if self.phase.get() == Phase::Ended {
            self.phase.set(Phase::Unreported);
        }
    }

    /// The compositor's report. True when the runtime should now write
    /// the lock's `open` false (an unlock).
    pub(crate) fn report(&self, rt: &Runtime, state: SessionLock) -> bool {
        let (phase, held) = match state {
            SessionLock::Locked => (Phase::Locked, true),
            SessionLock::Finished | SessionLock::Unlocked => (Phase::Ended, false),
        };
        self.phase.set(phase);
        if held {
            self.warned.set(false);
        }
        let _ = self.held.set(rt, held);
        state == SessionLock::Unlocked
    }
}

impl super::Ctx {
    /// The `lock`'s `open` binding's value, held true while the session
    /// is locked; an ignored write's notice goes to the tick's notices.
    pub(crate) fn hold_lock_open(&self, rt: &Runtime, out: super::PropOut) -> super::PropOut {
        let Some(track) = self.lock.borrow().clone() else {
            return out;
        };
        let (value, notice) = track.hold_open(rt, out.value);
        if let Some(n) = notice {
            self.notices.borrow_mut().push(n.to_string());
        }
        super::PropOut { value, ..out }
    }

    /// A mounted `lock`'s `open` turned from false to true.
    pub(crate) fn lock_opened(&self) {
        if let Some(track) = self.lock.borrow().as_ref() {
            track.opened();
        }
    }
}

/// (lockmon) The lock's own logic (`mount::lock_logic`): its timers and
/// its `on change` and service handlers, which write its `state`s and
/// so run once for the lock, not once per monitor's instance. They are
/// mounted in the lock's scope the first time an instance's content is
/// shown, and run while any is shown, frozen otherwise, as a surface's
/// content is.
pub(crate) struct LockLogic {
    env: std::rc::Rc<crate::vm::Env>,
    nodes: Vec<crate::lower::Node>,
    /// Their scope, under the lock's (a fault in one freezes the lock).
    block: strand_core::Scope,
    mounted: Cell<bool>,
    running: Cell<bool>,
    /// The instances' lock nodes whose content is shown.
    shown: std::cell::RefCell<std::collections::BTreeSet<strand_scene::NodeId>>,
}

impl super::Ctx {
    /// (lockmon) Keeps the lock's own logic, mounted in `env` (the
    /// lock's scope, whose `state`s it writes) once an instance shows.
    pub(crate) fn set_lock_logic(
        &self,
        rt: &Runtime,
        env: &std::rc::Rc<crate::vm::Env>,
        nodes: Vec<crate::lower::Node>,
    ) {
        let (block, ()) = rt.scope(|_| ());
        *self.lock_logic.borrow_mut() = Some(std::rc::Rc::new(LockLogic {
            env: env.clone(),
            nodes,
            block,
            mounted: Cell::new(false),
            running: Cell::new(false),
            shown: Default::default(),
        }));
    }

    /// (lockmon) The lock's scope, where its `state`s are, once mounted.
    pub(crate) fn lock_env(&self) -> Option<std::rc::Rc<crate::vm::Env>> {
        self.lock_logic.borrow().as_ref().map(|l| l.env.clone())
    }

    /// (lockmon) The content of the lock's instance `node` was shown
    /// (`open`) or hidden: the lock's logic runs while any is shown.
    pub(crate) fn lock_logic_shown(
        self: &std::rc::Rc<Self>,
        rt: &Runtime,
        node: strand_scene::NodeId,
        open: bool,
    ) {
        let Some(l) = self.lock_logic.borrow().clone() else {
            return;
        };
        let any = {
            let mut shown = l.shown.borrow_mut();
            match open {
                true => shown.insert(node),
                false => shown.remove(&node),
            };
            !shown.is_empty()
        };
        if any == l.running.get() {
            return;
        }
        l.running.set(any);
        if !any {
            let _ = rt.suspend(l.block.id());
        } else if !l.mounted.replace(true) {
            let _ = rt.with_owner(l.block.id(), |rt| {
                for n in &l.nodes {
                    match n {
                        crate::lower::Node::Handler(h) => self.mount_handler(rt, h, &l.env, None),
                        crate::lower::Node::Timer(t) => self.mount_timer(rt, t, &l.env, None),
                        _ => {}
                    }
                }
            });
        } else {
            rt.resume(l.block.id());
        }
    }

    /// (lockmon) The lock's instance `node` went (its monitor did): it
    /// no longer counts as shown. Nothing is frozen here (a cleanup);
    /// the next hide does that.
    pub(crate) fn lock_logic_gone(&self, node: strand_scene::NodeId) {
        if let Some(l) = self.lock_logic.borrow().as_ref() {
            l.shown.borrow_mut().remove(&node);
        }
    }
}

/// (lockmon) The lock's instances' keys and twins (`Prop::Twin`).
impl super::Ctx {
    /// (lockmon) `ident` is one monitor's instance of the lock
    /// (`<prefix>[<screen>]`): its elements' twins are keyed without it.
    pub(crate) fn note_lock_instance(&self, ident: std::rc::Rc<str>) {
        if let Some(cut) = ident.rfind('[') {
            let base: std::rc::Rc<str> = ident[..cut].into();
            self.lock_instances.borrow_mut().insert(ident, base);
        }
    }

    /// (lockmon) The key an element of a lock's instance (its reload key,
    /// `<instance>/…/e<n>`) shares with the same element on every other
    /// monitor: the key without the instance's screen.
    pub(crate) fn twin_key(&self, key: &str) -> Option<String> {
        self.lock_instances
            .borrow()
            .iter()
            .find_map(|(inst, base)| {
                let rest = key.strip_prefix(&**inst)?;
                rest.starts_with('/').then(|| format!("{base}{rest}"))
            })
    }

    /// (lockmon review) Whether input `e`, mounted in `env`, has its
    /// `text` bound two-way to a `state` every instance of the lock
    /// shares: one the lock's scope reads as the same cell (the lock's
    /// own, or a top-level one), not a component's or a `for` item's.
    pub(crate) fn shares_text(&self, e: &crate::lower::Element, env: &crate::vm::Env) -> bool {
        use crate::lower::PlaceRoot;
        use crate::vm::value::Slot;
        let Some(lock) = self.lock_env() else {
            return false;
        };
        e.props.iter().chain(e.arg.iter()).any(|p| {
            let Some(tw) = &p.two_way else {
                return false;
            };
            let PlaceRoot::Def(d) = tw.place.root else {
                return false;
            };
            p.prop == Some(strand_scene::Prop::Text)
                && matches!(
                    (env.def(d), lock.def(d)),
                    (Some(Slot::Signal(a)), Some(Slot::Signal(b))) if a.id() == b.id()
                )
        })
    }
}

/// (lockmon) `nodes` without the lock's own logic, which goes to `out`:
/// the timers and the `on change` and service handlers in the lock's
/// body and its elements, where its `state`s are (not in `if`/`match`
/// branches, `for` items or components, which are each instance's).
/// Element events (`on key`, `on activate`, `on show`) stay: they
/// happen to one monitor's element.
pub(crate) fn lock_logic(
    nodes: &std::sync::Arc<Vec<crate::lower::Node>>,
    out: &mut Vec<crate::lower::Node>,
) -> std::sync::Arc<Vec<crate::lower::Node>> {
    let mut kept = Vec::with_capacity(nodes.len());
    for n in nodes.iter() {
        match n {
            crate::lower::Node::Handler(h)
                if !matches!(h.event, crate::lower::Event::Element(_)) =>
            {
                out.push(n.clone())
            }
            crate::lower::Node::Timer(_) => out.push(n.clone()),
            crate::lower::Node::Element(e)
                if !matches!(e.kind, crate::lower::ElementKind::Component(_)) =>
            {
                let mut e = e.clone();
                e.children = lock_logic(&e.children, out);
                kept.push(crate::lower::Node::Element(e));
            }
            _ => kept.push(n.clone()),
        }
    }
    std::sync::Arc::new(kept)
}
