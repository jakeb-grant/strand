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
    ///
    /// The reload overlay's kept-over-default rows of the cells it reset
    /// go, as `reset{path}`'s do (`Overlay::forget_cell`): a cell kept
    /// under the node before and not after was reset by it, since the
    /// reset changes no cell outside the subtree.
    pub(super) fn reset_node_request(&mut self, from: Caller, id: &str) {
        let ans = node(id).and_then(|n| {
            let before = self.overlay.has_resets().then(|| kept_under(&self.inst, n));
            match self.inst.reset_subtree(n) {
                Ok(count) => {
                    if let Some(before) = before {
                        let after = kept_under(&self.inst, n);
                        let reset: Vec<String> =
                            before.into_iter().filter(|p| !after.contains(p)).collect();
                        if !reset.is_empty() {
                            self.overlay.forget_cells(&reset, &self.inst);
                        }
                    }
                    Ok(serde_json::json!({"ok": true, "reset": count}))
                }
                Err(_) => Err(gone(n)),
            }
        });
        self.answer(from, ans.unwrap_or_else(|e| e));
    }
}

/// The paths of the cells kept over a changed default that the nodes
/// under `n` show (the inspector's `kept` badge), each once.
fn kept_under(inst: &strand_compiler::instantiate::Instance, n: NodeId) -> Vec<String> {
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
        if cell["kept"] == true
            && let Some(p) = cell["path"].as_str()
            && !out.iter().any(|o| o == p)
        {
            out.push(p.to_string());
        }
    }
    out
}
