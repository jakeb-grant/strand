//! The inspector's commands on the main thread (S-inspect, M5 wave 3):
//! logic's `ToMain::{Highlight, DamageFlash, PickMode}` and
//! `SnapshotKind::Layout`, answered from the renderer between frames
//! (the logic half is `run/request/inspect.rs`), and the lock's guard:
//! pick mode and the outline end when a lock is asked for, and pick mode
//! is not turned on while one is.

use calloop::channel::Sender;
use serde_json::{Value as Json, json};
use strand_surface::State;

use crate::demo::host::Host;
use crate::run::{PickEvent, Snapshot, SnapshotKind, ToLogic, ToMain};

/// One of logic's inspector messages (a `report` snapshot is answered by
/// the caller).
pub(crate) fn on_main(state: &mut State<Host>, msg: ToMain, answer: &Sender<ToLogic>) {
    let locked = state.lock_active();
    let host = state.host_mut();
    match msg {
        ToMain::Snapshot {
            kind: SnapshotKind::Layout { node },
            reply,
        } => {
            let r = &host.renderer;
            let data = r
                .surfaces_of(node)
                .into_iter()
                .filter_map(|s| {
                    let b = r.node_layout(s, node).rect?;
                    Some(json!({
                        "surface": s.0,
                        "x": b.x,
                        "y": b.y,
                        "width": b.w,
                        "height": b.h,
                    }))
                })
                .collect();
            let data = Json::Array(data);
            let _ = answer.send(ToLogic::Snapshot(Snapshot { reply, data }));
            return;
        }
        ToMain::Snapshot { .. } => return,
        ToMain::Highlight(node) => host.renderer.set_highlight(node),
        ToMain::DamageFlash(on) => host.renderer.set_damage_flash(on),
        ToMain::PickMode(on) => {
            host.set_picking(on && !locked);
            if on && locked {
                let _ = answer.send(ToLogic::Picked(PickEvent::Ended));
            }
        }
    }
    // What changed is drawn: the surfaces that want a frame get one.
    crate::demo::text_ready(state);
}

/// While a lock is asked for or held: pick mode ends (logic is told)
/// and nothing is outlined. Nothing to do otherwise.
pub(crate) fn lock_guard(state: &mut State<Host>) {
    if state.lock_active() {
        state.host_mut().lock_started();
    }
}
