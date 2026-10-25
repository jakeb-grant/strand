//! (eval-host-loop) The host loop's reactions (`strand_render::host_loop`)
//! run in their one order: what the shell's main thread and the
//! evaluation sandbox's driver both do when logic's diff arrives, the
//! loop wakes, input comes, a surface is attached, configured, detached
//! or painted (docs/decisions.md, eval-host-loop).

mod common;

use std::convert::Infallible;
use std::time::Duration;

use common::{Buffer, num, renderer};
use strand_render::host_loop::{self, Cause, LoopHost, LoopSync};
use strand_render::{Intent, Renderer, Router, SceneError};
use strand_scene::{
    AxisDelta, InputEvent, LogicalPoint, NodeId, NodeKind, Painter, Prop, Scale, SceneDiff, Size,
    SurfaceId,
};

/// A host that writes down every call the reactions make of it.
struct Rec {
    r: Renderer,
    router: Option<Router>,
    shown: Vec<SurfaceId>,
    log: Vec<String>,
}

impl Rec {
    fn new(routes: bool) -> Rec {
        Rec {
            r: renderer(),
            router: routes.then(Router::new),
            shown: Vec::new(),
            log: Vec::new(),
        }
    }

    fn take(&mut self) -> Vec<String> {
        std::mem::take(&mut self.log)
    }
}

impl LoopHost for Rec {
    type Error = Infallible;

    fn parts(&mut self) -> (&mut Renderer, Option<&mut Router>) {
        (&mut self.r, self.router.as_mut())
    }

    fn shown(&self, out: &mut Vec<SurfaceId>) {
        out.extend(&self.shown);
    }

    fn applied(&mut self, errors: Vec<SceneError>) -> Result<(), Infallible> {
        self.log.push(format!("applied {}", errors.len()));
        Ok(())
    }

    fn intents(&mut self, cause: Cause<'_>, intents: Vec<Intent>) -> Result<(), Infallible> {
        let cause = match cause {
            Cause::Settle => "settle",
            Cause::Tick => "tick",
            Cause::Input(_) => "input",
        };
        let flags = intents
            .iter()
            .filter(|i| matches!(i, Intent::Flag { .. }))
            .count();
        self.log.push(format!("intents {cause} {flags}"));
        Ok(())
    }

    fn facts(&mut self) -> Result<(), Infallible> {
        self.log.push("facts".into());
        Ok(())
    }

    fn wake(&mut self) -> Result<(), Infallible> {
        self.log.push("wake".into());
        Ok(())
    }

    fn attaching(&mut self, surface: SurfaceId, _: NodeId) -> Result<(), Infallible> {
        self.shown.push(surface);
        self.log.push("attaching".into());
        Ok(())
    }

    fn configuring(&mut self, _: SurfaceId, _: Size, _: Scale) {
        self.log.push("configuring".into());
    }

    fn detaching(&mut self, surface: SurfaceId) -> Result<(), Infallible> {
        self.shown.retain(|s| *s != surface);
        self.log.push("detaching".into());
        Ok(())
    }

    fn detached(&mut self, _: SurfaceId) -> Result<(), Infallible> {
        self.log.push("detached".into());
        Ok(())
    }

    fn after_paint(&mut self, _: SurfaceId) -> Result<(), Infallible> {
        self.log.push("after_paint".into());
        Ok(())
    }
}

impl LoopSync for Rec {
    fn sync(&mut self) -> Result<(), Infallible> {
        let n = self.r.take_surface_changes().len();
        self.log.push(format!("sync {n}"));
        Ok(())
    }
}

const PANEL: NodeId = NodeId::new(0, 0);
const LIST: NodeId = NodeId::new(1, 0);
const S: SurfaceId = SurfaceId(1);

/// A panel of 100 x 100 holding a list of 40 rows of 20.
fn panel_with_list() -> SceneDiff {
    let mut d = SceneDiff::new();
    d.create(PANEL, NodeKind::Panel, None, 0)
        .set(PANEL, Prop::Width, num(100.0))
        .set(PANEL, Prop::Height, num(100.0))
        .create(LIST, NodeKind::List, Some(PANEL), 0)
        .set(LIST, Prop::Height, num(100.0));
    for i in 0..40 {
        let row = NodeId::new(10 + i, 0);
        d.create(row, NodeKind::Box, Some(LIST), i)
            .set(row, Prop::Height, num(20.0));
    }
    d
}

fn wheel(time: u32) -> InputEvent {
    InputEvent::PointerAxis {
        surface: S,
        position: LogicalPoint::new(50.0, 50.0),
        horizontal: AxisDelta::default(),
        vertical: AxisDelta {
            pixels: 15.0,
            value120: 120,
            stop: false,
        },
        source: None,
        time,
    }
}

/// The panel applied, its surface attached, configured and painted once
/// through the reactions; the log cleared.
fn shown(routes: bool) -> (Rec, Buffer) {
    let mut h = Rec::new(routes);
    let Ok(()) = host_loop::apply(&mut h, panel_with_list());
    let Ok(()) = host_loop::attached(&mut h, S, PANEL, None);
    let Ok(()) = host_loop::configured(&mut h, S, Size::new(100, 100), Scale::ONE);
    let mut buf = Buffer::new(100, 100, Scale::ONE);
    buf.paint_at(&mut h.r, S, 0, Duration::from_secs(1));
    let Ok(()) = host_loop::painted(&mut h, S);
    h.r.take_surface_changes();
    h.take();
    (h, buf)
}

/// Logic's diff: the router observes it, render applies it (its refused
/// ops told), routing settles on it, then the surfaces sync. A host that
/// routes nothing (the demo bar) skips routing.
#[test]
fn a_diff_is_applied_settled_then_synced() {
    let mut h = Rec::new(true);
    let Ok(()) = host_loop::apply(&mut h, panel_with_list());
    assert_eq!(h.take(), ["applied 0", "intents settle 0", "sync 1"]);
    let mut bad = SceneDiff::new();
    bad.set(NodeId::new(99, 0), Prop::Width, num(1.0));
    let Ok(()) = host_loop::apply(&mut h, bad);
    assert_eq!(h.take(), ["applied 1", "intents settle 0", "sync 0"]);

    let mut h = Rec::new(false);
    let Ok(()) = host_loop::apply(&mut h, panel_with_list());
    assert_eq!(h.take(), ["applied 0", "sync 1"]);
}

/// A wake ticks the router, then `update` runs and the surfaces sync.
#[test]
fn a_wake_ticks_routing_updates_then_syncs() {
    let (mut h, _) = shown(true);
    let Ok(()) = host_loop::woke(&mut h);
    assert_eq!(h.take(), ["intents tick 0", "sync 0"]);
    let (mut h, _) = shown(false);
    let Ok(()) = host_loop::woke(&mut h);
    assert_eq!(h.take(), ["sync 0"]);
}

/// A surface attached is the renderer's, then the host's, then
/// routing's; configured, it is laid out, the host told, its facts sent.
#[test]
fn a_surface_is_attached_then_configured() {
    let mut h = Rec::new(true);
    let Ok(()) = host_loop::apply(&mut h, panel_with_list());
    h.take();
    let Ok(()) = host_loop::attached(&mut h, S, PANEL, None);
    assert_eq!(h.take(), ["attaching"]);
    let Ok(()) = host_loop::configured(&mut h, S, Size::new(100, 100), Scale::ONE);
    assert_eq!(h.take(), ["configuring", "facts"]);
    assert!(h.r.wants_frame(S), "configured, it wants its first frame");
}

/// Input is routed, layout facts go, and the loop wakes only when the
/// event left an idle surface wanting a frame: a wheel step on the still
/// list wakes it, a second while the step springs and pointer motion do
/// not.
#[test]
fn input_wakes_the_loop_only_for_a_surface_it_woke() {
    let (mut h, mut buf) = shown(true);
    let mut idle = Vec::new();
    let enter = InputEvent::PointerEnter {
        surface: S,
        position: LogicalPoint::new(50.0, 50.0),
    };
    let Ok(()) = host_loop::input(&mut h, &enter, &mut idle);
    let log = h.take();
    // Facts go before the event's intents (a pointer read reaches logic
    // before the flags) and after them.
    assert_eq!(log[0], "facts", "{log:?}");
    assert!(log[1].starts_with("intents input"), "{log:?}");
    assert_eq!(log[2..], ["facts"]);
    h.r.take_surface_changes();
    assert!(!h.r.wants_frame(S));
    let Ok(()) = host_loop::input(&mut h, &wheel(1010), &mut idle);
    assert_eq!(h.take(), ["facts", "intents input 0", "facts", "wake"]);
    assert!(h.r.wants_frame(S));
    let Ok(()) = host_loop::input(&mut h, &wheel(1020), &mut idle);
    assert_eq!(h.take(), ["facts", "intents input 0", "facts"]);
    // Settled, a step wakes it again; each paint sends its facts first.
    let mut ms = 1016;
    while h.r.wants_frame(S) {
        buf.paint_at(&mut h.r, S, 1, Duration::from_millis(ms));
        let Ok(()) = host_loop::painted(&mut h, S);
        assert_eq!(
            h.take(),
            ["facts", "after_paint"],
            "a frame leaves no change"
        );
        ms += 16;
        assert!(ms < 10_000, "the wheel never settled");
    }
    h.r.take_surface_changes();
    let Ok(()) = host_loop::input(&mut h, &wheel(ms as u32), &mut idle);
    assert_eq!(h.take(), ["facts", "intents input 0", "facts", "wake"]);
}

/// A surface destroyed: the renderer forgets it, then the host, then
/// routing (its pointer gone), and the router ticks.
#[test]
fn a_surface_detached_is_forgotten_then_ticked() {
    let (mut h, _) = shown(true);
    let mut idle = Vec::new();
    let enter = InputEvent::PointerEnter {
        surface: S,
        position: LogicalPoint::new(50.0, 50.0),
    };
    let Ok(()) = host_loop::input(&mut h, &enter, &mut idle);
    h.take();
    assert!(h.router.as_ref().is_some_and(|r| r.pointer(S).is_some()));
    let Ok(()) = host_loop::detached(&mut h, S);
    assert_eq!(h.take(), ["detaching", "detached", "intents tick 0"]);
    assert!(h.router.as_ref().is_some_and(|r| r.pointer(S).is_none()));
    assert!(!h.r.wants_frame(S), "the renderer has it no more");
}
