//! The inspector's commands: `inspect.node{id}`, `inspect.dump{surface?}`
//! and `reset{node}` (S-ipc's wave-2 arms over S-inspect's library,
//! `strand_compiler::instantiate::inspect`, which S-inspect reviews);
//! `inspect.pick`, `inspect.highlight` and `inspect.flash` (S-inspect)
//! are not served yet: they do not parse (`strand_core::ipc::parse`), so
//! a client hears `unknown command`, as from M4. Main's side is reached
//! through `ToMain::{Highlight, DamageFlash, PickMode}` and
//! `SnapshotKind::Layout`, and a click's pick comes back as
//! `ToLogic::Picked` (`run/mod.rs`).
//!
//! A node on the wire is its `index.generation` (`12.0`,
//! `inspect::node_ref`), as the inspector's answers spell it;
//! `inspect.dump{surface}` names a surface by its root's node ref and
//! dumps the tree under it (any node's subtree, read the same way).

use serde_json::Value as Json;
use strand_compiler::instantiate::inspect::{node_ref, parse_node_ref};
use strand_core::ipc as proto;
use strand_scene::NodeId;

use super::{Caller, Shell};

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
    /// (`Instance::inspect_node`'s shape).
    #[cold]
    #[inline(never)]
    pub(super) fn inspect_node_request(&mut self, from: Caller, id: &str) {
        let ans = node(id).and_then(|n| self.inst.inspect_node(n).map(ok).ok_or_else(|| gone(n)));
        self.answer(from, ans.unwrap_or_else(|e| e));
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
    #[cold]
    #[inline(never)]
    pub(super) fn reset_node_request(&mut self, from: Caller, id: &str) {
        let ans = node(id).and_then(|n| match self.inst.reset_subtree(n) {
            Ok(count) => Ok(serde_json::json!({"ok": true, "reset": count})),
            Err(_) => Err(gone(n)),
        });
        self.answer(from, ans.unwrap_or_else(|e| e));
    }
}
