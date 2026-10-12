//! The inspector's commands: `inspect.node{id}`, `inspect.dump{surface?}`
//! and `reset{node}` (S-ipc's wave-2 arms over S-inspect's library,
//! `strand_compiler::instantiate::inspect`), and `inspect.pick{on}`,
//! `inspect.highlight{id?}` and `inspect.flash{on}` (S-inspect, wave 3).
//! Main's side is reached through `ToMain::{Highlight, DamageFlash,
//! PickMode}` and `SnapshotKind::Layout` (handled on main by
//! `crate::ipc::inspect`), and a click's pick comes back as
//! `ToLogic::Picked` (`run/mod.rs`).
//!
//! A node on the wire is its `index.generation` (`12.0`,
//! `inspect::node_ref`), as the inspector's answers spell it;
//! `inspect.dump{surface}` names a surface by its root's node ref and
//! dumps the tree under it (any node's subtree, read the same way).
//! `inspect.node` adds the node's `layout` on each surface showing it,
//! from main (`[{"surface", "x", "y", "width", "height"}]`, logical
//! pixels; empty when no surface draws it).
//!
//! Pick mode, the outline and the flash belong to the socket client that
//! asked (a socket request only, as `report`): each ends when that client
//! disconnects, pick mode also when a lock starts (and it is refused
//! while one is shown) or when main ends it (the surface it picked on
//! unmapped). While pick mode is on, its client hears `{"event":
//! "picked", "surface": n, "node": "12.0" | null}` for each left press,
//! and `{"event": "pick_ended"}` when pick mode ends without its asking.

use serde_json::{Value as Json, json};
use strand_compiler::instantiate::inspect::{node_ref, parse_node_ref};
use strand_core::ipc as proto;
use strand_scene::NodeId;

use super::{Caller, Shell};
use crate::ipc::ClientId;
use crate::run::{PickEvent, Snapshot, SnapshotId, SnapshotKind, ToMain};

/// The inspector's state on the logic thread: which socket client owns
/// pick mode, the outline and the flash, and `inspect.node` answers
/// waiting for main's layouts.
#[derive(Debug, Default)]
pub(in crate::run) struct Inspector {
    pick: Option<ClientId>,
    highlight: Option<ClientId>,
    flash: Option<ClientId>,
    nodes: Vec<(SnapshotId, Caller, Json)>,
}

impl Inspector {
    /// Whether `reply` is an `inspect.node`'s layout snapshot.
    pub(in crate::run) fn waits_for(&self, reply: SnapshotId) -> bool {
        self.nodes.iter().any(|(id, ..)| *id == reply)
    }
}

/// `{"event": "pick_ended"}`.
fn pick_ended() -> Json {
    json!({"event": "pick_ended"})
}

/// `id` read as a node ref, or the refusal saying what one is.
fn node(id: &str) -> Result<NodeId, Json> {
    parse_node_ref(id).ok_or_else(|| {
        proto::refused(format!(
            "`{id}` is not a node id (`index.generation`, as the inspector spells it)"
        ))
    })
}

/// No node `id` is on the scene.
fn gone(id: NodeId) -> Json {
    proto::refused(format!("no node `{}`", node_ref(id)))
}

/// The library's answer object with `"ok": true` added.
fn ok(v: Json) -> Json {
    match v {
        Json::Object(mut m) => {
            m.insert("ok".into(), Json::Bool(true));
            Json::Object(m)
        }
        other => other,
    }
}

impl Shell {
    /// `inspect.node{id}`: `{"ok": true, "node": {...}, "files": {...}}`
    /// (`Instance::inspect_node`'s shape), with the node's `layout` from
    /// main once it answers (none without a main thread).
    #[cold]
    #[inline(never)]
    pub(super) fn inspect_node_request(&mut self, from: Caller, id: &str) {
        let ans = node(id).and_then(|n| {
            let pane = self.inst.inspect_node(n).map(ok).ok_or_else(|| gone(n))?;
            Ok((n, pane))
        });
        let (n, mut pane) = match ans {
            Ok(v) => v,
            Err(e) => return self.answer(from, e),
        };
        let reply = SnapshotId(self.next_snapshot);
        self.next_snapshot += 1;
        let asked = self.to_main.as_ref().is_some_and(|tx| {
            tx.send(ToMain::Snapshot {
                kind: SnapshotKind::Layout { node: n },
                reply,
            })
            .is_ok()
        });
        if !asked {
            pane["layout"] = Json::Array(Vec::new());
            return self.answer(from, pane);
        }
        self.inspector.nodes.push((reply, from, pane));
    }

    /// Main's layouts for a waiting `inspect.node`: answered with them.
    pub(in crate::run) fn layout_answered(&mut self, snap: Snapshot) {
        let nodes = &mut self.inspector.nodes;
        let Some(i) = nodes.iter().position(|(id, ..)| *id == snap.reply) else {
            return;
        };
        let (_, from, mut pane) = nodes.swap_remove(i);
        pane["layout"] = snap.data;
        self.answer(from, pane);
    }

    /// `inspect.pick{on}` (a socket client's): pick mode on, owned by
    /// that client (one that owned it before hears `pick_ended`), or off.
    /// Refused while a lock is shown, and with no main thread.
    #[cold]
    #[inline(never)]
    pub(super) fn inspect_pick_request(&mut self, from: Caller, on: bool) {
        let Caller::Socket(id) = from else {
            return self.not_served(from, "inspect.pick");
        };
        if on && self.inst.lock_shown() {
            return self.answer(
                from,
                proto::refused("the session is locked: nothing is pickable"),
            );
        }
        if !self.tell_main(ToMain::PickMode(on)) {
            return self.answer(from, proto::refused("no display thread to pick on"));
        }
        let before = std::mem::replace(&mut self.inspector.pick, on.then_some(id));
        if let Some(old) = before.filter(|old| *old != id) {
            self.send_event(old, &pick_ended());
        }
        self.answer(from, proto::ok());
    }

    /// `inspect.highlight{id?}` (a socket client's): outline the node on
    /// every surface showing it, or nothing; it goes with the client.
    #[cold]
    #[inline(never)]
    pub(super) fn inspect_highlight_request(&mut self, from: Caller, id: Option<&str>) {
        let Caller::Socket(client) = from else {
            return self.not_served(from, "inspect.highlight");
        };
        let target = match id.map(node).transpose() {
            Ok(t) => t,
            Err(e) => return self.answer(from, e),
        };
        if let Some(n) = target
            && self.inst.identity(n).is_none()
        {
            return self.answer(from, gone(n));
        }
        if !self.tell_main(ToMain::Highlight(target)) {
            return self.answer(from, proto::refused("no display thread to draw on"));
        }
        self.inspector.highlight = target.map(|_| client);
        self.answer(from, proto::ok());
    }

    /// `inspect.flash{on}` (a socket client's): repaint flashing on or
    /// off; it goes with the client.
    #[cold]
    #[inline(never)]
    pub(super) fn inspect_flash_request(&mut self, from: Caller, on: bool) {
        let Caller::Socket(client) = from else {
            return self.not_served(from, "inspect.flash");
        };
        if !self.tell_main(ToMain::DamageFlash(on)) {
            return self.answer(from, proto::refused("no display thread to draw on"));
        }
        self.inspector.flash = on.then_some(client);
        self.answer(from, proto::ok());
    }

    /// Main's pick-mode news: a pick goes to the client in pick mode; an
    /// end it did not ask for is told to it.
    pub(in crate::run) fn picked(&mut self, event: PickEvent) {
        let Some(owner) = self.inspector.pick else {
            return;
        };
        match event {
            PickEvent::Picked { surface, node } => {
                let node = node.map_or(Json::Null, |n| Json::String(node_ref(n)));
                let event = json!({"event": "picked", "surface": surface.0, "node": node});
                self.send_event(owner, &event);
            }
            PickEvent::Ended => {
                self.inspector.pick = None;
                self.send_event(owner, &pick_ended());
            }
        }
    }

    /// A lock started: pick mode and the outline end (main has ended them
    /// on its side already).
    pub(in crate::run) fn inspector_locked(&mut self) {
        if let Some(owner) = self.inspector.pick.take() {
            self.send_event(owner, &pick_ended());
        }
        self.inspector.highlight = None;
    }

    /// Pick mode, the outline and the flash of a socket client that has
    /// gone end with it.
    pub(in crate::run) fn inspector_clients_gone(&mut self) {
        let Some(s) = &self.server else {
            return;
        };
        let i = &mut self.inspector;
        let gone = |c: &mut Option<ClientId>| c.take_if(|id| !s.has(*id)).is_some();
        let (pick, highlight, flash) = (
            gone(&mut i.pick),
            gone(&mut i.highlight),
            gone(&mut i.flash),
        );
        if pick {
            self.tell_main(ToMain::PickMode(false));
        }
        if highlight {
            self.tell_main(ToMain::Highlight(None));
        }
        if flash {
            self.tell_main(ToMain::DamageFlash(false));
        }
    }

    /// Sends main `msg`; false without a main thread.
    fn tell_main(&self, msg: ToMain) -> bool {
        self.to_main.as_ref().is_some_and(|tx| tx.send(msg).is_ok())
    }

    /// An event line to socket client `id`.
    fn send_event(&mut self, id: ClientId, event: &Json) {
        if let Some(s) = &mut self.server {
            s.send(id, event);
        }
    }

    /// `inspect.dump{surface?}`: `{"ok": true, "nodes": [...], "files":
    /// {...}}` of the tree under the surface root `surface`, or of every
    /// surface (`Instance::dump`'s shape).
    #[cold]
    #[inline(never)]
    pub(super) fn inspect_dump_request(&mut self, from: Caller, surface: Option<&str>) {
        // `dump` answers `None` only for a root it does not hold.
        let ans = surface.map(node).transpose().and_then(|root| {
            self.inst
                .dump(root)
                .map(ok)
                .ok_or_else(|| root.map_or_else(|| proto::refused("no scene"), gone))
        });
        self.answer(from, ans.unwrap_or_else(|e| e));
    }

    /// `reset{node}`, "reset subtree": the state under the node back to
    /// its defaults (`Instance::reset_subtree`), `{"ok": true, "reset":
    /// n}` with how many cells changed.
    ///
    /// The reload overlay's kept-over-default rows of the cells it reset
    /// go, as `reset{path}`'s do (`Overlay::forget_cell`): every cell the
    /// nodes under it show, except the persisted ones, which the reset
    /// leaves alone. A cell changed since it was kept no longer has the
    /// inspector's `kept` badge but still has its row, so the badge
    /// cannot tell which rows to drop.
    #[cold]
    #[inline(never)]
    pub(super) fn reset_node_request(&mut self, from: Caller, id: &str) {
        let ans = node(id).and_then(|n| {
            let under = self
                .overlay
                .has_resets()
                .then(|| reset_under(&self.inst, n));
            match self.inst.reset_subtree(n) {
                Ok(count) => {
                    if let Some(reset) = under.filter(|r| !r.is_empty()) {
                        self.overlay.forget_cells(&reset, &self.inst);
                    }
                    Ok(serde_json::json!({"ok": true, "reset": count}))
                }
                Err(_) => Err(gone(n)),
            }
        });
        self.answer(from, ans.unwrap_or_else(|e| e));
    }
}

/// The paths of the cells the nodes under `n` show that a subtree reset
/// puts back to their defaults (all but the persisted ones), each once.
fn reset_under(inst: &strand_compiler::instantiate::Instance, n: NodeId) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Some(dump) = inst.dump(Some(n)) else {
        return out;
    };
    let nodes = dump["nodes"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    for cell in nodes
        .iter()
        .filter_map(|node| node["cells"].as_array())
        .flatten()
    {
        if cell["persisted"] != true
            && let Some(p) = cell["path"].as_str()
            && !out.iter().any(|o| o == p)
        {
            out.push(p.to_string());
        }
    }
    out
}
