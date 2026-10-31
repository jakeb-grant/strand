//! (eval-host-loop) The host loop's reactions (docs/architecture.md,
//! "Render loop"): what a host does with the [`Renderer`] and its input
//! [`Router`] when logic's diff arrives, the loop wakes, an input event
//! comes, a surface is attached, configured or detached, and a frame was
//! painted, each in the one order every host follows.
//!
//! Two hosts run them: the shell's main thread (`strand run` and the
//! demo, crates/strand/src/demo), with the surface manager, the logic
//! thread and the loop's ping behind it, and the evaluation sandbox's
//! driver (eval/src/harness.rs), with its virtual surfaces, the logic VM
//! and its virtual clock. Each gives its side through [`LoopHost`] (and
//! [`LoopSync`] where the reaction hands surface changes over); what
//! either host does between and around the reactions (when they run,
//! how long logic takes to answer, which surface paints when) is its
//! own, and the order inside one is not: the sandbox runs the frame
//! sequence the shell runs because both run this code
//! (docs/decisions.md, eval-host-loop).
//!
//! Every function is generic over the host and does only what the host
//! did by hand before, in the same order, so the shipped binary pays
//! nothing for it.

use strand_scene::{InputEvent, LogicalSize, NodeId, Painter, Scale, SceneDiff, Size, SurfaceId};

use crate::input::{Intent, Router};
use crate::renderer::Renderer;
use crate::tree::SceneError;

/// Which routing call produced a batch of [`Intent`]s
/// ([`LoopHost::intents`]).
#[derive(Clone, Copy, Debug)]
pub enum Cause<'a> {
    /// [`Router::settle`], after logic's diff was applied.
    Settle,
    /// [`Router::tick`]: the loop woke (a hover popup's delay or grace
    /// ending), or a surface went.
    Tick,
    /// [`Router::handle`] of this event.
    Input(&'a InputEvent),
    /// [`Router::repoint`]: a frame moved what is under a still pointer.
    Repoint,
}

/// A host's side of the reactions: the renderer and router it owns, and
/// where what they produce goes.
pub trait LoopHost {
    /// What a host's own side can fail with (the shell's cannot:
    /// [`std::convert::Infallible`]).
    type Error;

    /// The renderer and, when the host routes input, its router (the
    /// demo bar routes none).
    fn parts(&mut self) -> (&mut Renderer, Option<&mut Router>);

    /// The surfaces it shows now, into `out` (cleared by the caller):
    /// those of them not wanting a frame before an input event are woken
    /// by it if they want one after.
    fn shown(&self, out: &mut Vec<SurfaceId>);

    /// Logic's diff was applied ([`Renderer::apply`]); these ops failed.
    fn applied(&mut self, errors: Vec<SceneError>) -> Result<(), Self::Error>;

    /// What routing asked for, from `cause`: for logic (flags, node
    /// events, two-way writes) and the surface manager (clipboard asks).
    /// Called for every routing call, with no intents too.
    fn intents(&mut self, cause: Cause<'_>, intents: Vec<Intent>) -> Result<(), Self::Error>;

    /// Laid-out boxes that changed ([`Renderer::take_layout_facts`]) for
    /// logic, at the points the loop hands them over.
    fn facts(&mut self) -> Result<(), Self::Error>;

    /// The loop is to wake ([`woke`] runs at its next turn): surface
    /// changes wait to be handed over, or an idle surface wants a frame.
    fn wake(&mut self) -> Result<(), Self::Error>;

    /// `surface` was attached to `node` by the renderer, before routing
    /// hears of it: the host's own bookkeeping.
    fn attaching(&mut self, surface: SurfaceId, node: NodeId) -> Result<(), Self::Error> {
        let _ = (surface, node);
        Ok(())
    }

    /// `surface` was configured by the renderer, before its facts go:
    /// the host's own bookkeeping (its size to logic).
    fn configuring(&mut self, surface: SurfaceId, size: Size, scale: Scale) {
        let _ = (surface, size, scale);
    }

    /// The renderer forgot `surface`, routing has not yet.
    fn detaching(&mut self, surface: SurfaceId) -> Result<(), Self::Error> {
        let _ = surface;
        Ok(())
    }

    /// Routing forgot `surface` too, before it ticks for it.
    fn detached(&mut self, surface: SurfaceId) -> Result<(), Self::Error> {
        let _ = surface;
        Ok(())
    }

    /// `surface`'s frame was painted and its facts went, before the loop
    /// is woken for what the paint left.
    fn after_paint(&mut self, surface: SurfaceId) -> Result<(), Self::Error> {
        let _ = surface;
        Ok(())
    }
}

/// A host that hands the renderer's surface changes to its surfaces (the
/// surface manager, or the sandbox's own) as part of a reaction.
pub trait LoopSync: LoopHost {
    /// [`Renderer::take_surface_changes`] to the surfaces, then whatever
    /// the host does with them (the shell: layout facts to logic, every
    /// surface asked for a frame).
    fn sync(&mut self) -> Result<(), Self::Error>;
}

/// Logic's diff arrived: the router observes it (logic's `input` texts
/// answer the edits in flight), the renderer applies it, routing settles
/// on the new scene, the router's next wake is the renderer's input wake,
/// and the surfaces follow.
pub fn apply<H: LoopSync>(h: &mut H, diff: SceneDiff) -> Result<(), H::Error> {
    if let (_, Some(router)) = h.parts() {
        router.observe(&diff);
    }
    let errors = h.parts().0.apply(diff);
    h.applied(errors)?;
    let (r, router) = h.parts();
    if let Some(router) = router {
        let intents = router.settle(r);
        h.intents(Cause::Settle, intents)?;
        input_wake(h);
    }
    h.sync()
}

/// The loop woke (the text worker delivered layouts, the renderer's or
/// the router's due time came, a call left surface changes): the router
/// ticks (a hover popup's delay or grace), [`Renderer::update`] collects
/// and lays out, and the surfaces follow.
pub fn woke<H: LoopSync>(h: &mut H) -> Result<(), H::Error> {
    tick(h)?;
    h.parts().0.update();
    h.sync()
}

/// An input event, after the host's own handling of it (a lock's, a
/// rebase's): routed, the renderer's pointer and input wake follow the
/// router, layout facts go, and the loop wakes if the event left surface
/// changes or made an idle surface want a frame with no diff to follow
/// (a key scrolling a list to the row it selects). `idle` is scratch
/// space a host keeps, so pointer motion allocates nothing.
pub fn input<H: LoopHost>(
    h: &mut H,
    event: &InputEvent,
    idle: &mut Vec<SurfaceId>,
) -> Result<(), H::Error> {
    idle.clear();
    h.shown(idle);
    let (r, router) = h.parts();
    idle.retain(|s| !r.wants_frame(*s));
    // (flex) Where a press went down: an `anchor: pointer` popup it
    // opens opens there; a key after it opens one at its element.
    match event {
        InputEvent::PointerButton {
            surface,
            position,
            state: strand_scene::ButtonState::Pressed,
            ..
        } => r.pressed_at(*surface, *position),
        InputEvent::PointerButton { surface, .. } => r.released_at(*surface),
        InputEvent::Key { .. } => r.key_step(),
        _ => {}
    }
    if let Some(router) = router {
        let intents = router.handle(event, r);
        let s = event.surface();
        r.set_pointer(s, router.pointer(s));
        // (flex) `Node.pointer`: reported by the frame this wants, or at
        // once when a press begins, and handed over before the flags so
        // `on change self.pressed` reads where the press went down.
        r.pointer_input(s, router.pointer(s), router.hovered(s), router.left_held(s));
        h.facts()?;
        h.intents(Cause::Input(event), intents)?;
        input_wake(h);
    }
    h.facts()?;
    let r = h.parts().0;
    if r.has_surface_changes() || idle.iter().any(|s| r.wants_frame(*s)) {
        h.wake()?;
    }
    Ok(())
}

/// The surface manager attached `surface` to `node` on an output of
/// logical size `bounds` (`None`: not known): the renderer paints it and
/// lays a content-sized one out no larger than the output, routing hears
/// of it, and changes it left wake the loop.
pub fn attached<H: LoopHost>(
    h: &mut H,
    surface: SurfaceId,
    node: NodeId,
    bounds: Option<LogicalSize>,
) -> Result<(), H::Error> {
    let r = h.parts().0;
    r.attach_surface(surface, node);
    r.set_surface_bounds(surface, bounds);
    h.attaching(surface, node)?;
    if let (_, Some(router)) = h.parts() {
        router.attached(surface, node);
    }
    wake_if_changed(h)
}

/// The surface manager configured `surface` at `size` and `scale`: the
/// renderer lays it out (its first layout's boxes go as facts, so a
/// container query answers before the first frame), and changes it left
/// wake the loop.
pub fn configured<H: LoopHost>(
    h: &mut H,
    surface: SurfaceId,
    size: Size,
    scale: Scale,
) -> Result<(), H::Error> {
    h.parts().0.configure_surface(surface, size, scale);
    h.configuring(surface, size, scale);
    h.facts()?;
    wake_if_changed(h)
}

/// The surface manager destroyed `surface`: the renderer and routing
/// forget it, and the router ticks (the pointer gone with it may start a
/// hover popup's grace).
pub fn detached<H: LoopHost>(h: &mut H, surface: SurfaceId) -> Result<(), H::Error> {
    h.parts().0.detach_surface(surface);
    h.detaching(surface)?;
    if let (_, Some(router)) = h.parts() {
        router.detached(surface);
    }
    h.detached(surface)?;
    tick(h)
}

/// `surface`'s frame was painted: if it moved what is under a still
/// pointer, the pointer is routed again; its layout facts go, then the
/// host's own after-paint work, and changes the paint left (an exit that
/// ended closes or shrinks a surface) wake the loop.
pub fn painted<H: LoopHost>(h: &mut H, surface: SurfaceId) -> Result<(), H::Error> {
    repoint(h, surface)?;
    h.facts()?;
    h.after_paint(surface)?;
    wake_if_changed(h)
}

/// (still-pointer) A frame moved `surface`'s hit boxes under the pointer:
/// hover, `pointer` and the cursor follow what is there now. The loop wakes
/// when that changed, for a cursor no diff would otherwise bring.
fn repoint<H: LoopHost>(h: &mut H, surface: SurfaceId) -> Result<(), H::Error> {
    let (r, router) = h.parts();
    let Some(router) = router else {
        return Ok(());
    };
    if !r.take_hits_moved(surface) {
        return Ok(());
    }
    let intents = router.repoint(surface, r);
    if intents.is_empty() {
        return Ok(());
    }
    r.pointer_input(
        surface,
        router.pointer(surface),
        router.hovered(surface),
        router.left_held(surface),
    );
    h.facts()?;
    h.intents(Cause::Repoint, intents)?;
    input_wake(h);
    h.wake()
}

/// The router ticks, its intents go, and its next wake is the renderer's.
fn tick<H: LoopHost>(h: &mut H) -> Result<(), H::Error> {
    let (r, router) = h.parts();
    if let Some(router) = router {
        let intents = router.tick(r);
        h.intents(Cause::Tick, intents)?;
        input_wake(h);
    }
    Ok(())
}

/// The router's next due time (a hover popup's delay or grace) wakes the
/// renderer's loop.
fn input_wake<H: LoopHost>(h: &mut H) {
    if let (r, Some(router)) = h.parts() {
        r.set_input_wake(router.next_wake());
    }
}

fn wake_if_changed<H: LoopHost>(h: &mut H) -> Result<(), H::Error> {
    if h.parts().0.has_surface_changes() {
        h.wake()?;
    }
    Ok(())
}
