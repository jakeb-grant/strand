//! `enter`/`exit` poses: the props a pose sets, and the sizes it gives.

use strand_scene::{LogicalRect, Prop, PropValue, TokenScope};

use super::motion::number;
use crate::tree::{Node, SceneTree};

/// The props of a pose: `enter { x: 420; opacity: 0 }`, or a preset
/// (`fade`, `slidefade`, `popin(0.8)`, `slide(top)`), for a node laid out
/// in `rect` (a `slide` moves by the node's own size).
pub(crate) fn pose_props(v: &PropValue, rect: Option<LogicalRect>) -> Vec<(Prop, PropValue)> {
    let n = PropValue::Number;
    match v {
        PropValue::Pose(props) => props.clone(),
        PropValue::Keyword(k) => match k.as_str() {
            "fade" => vec![(Prop::Opacity, n(0.0))],
            // Fades in while rising a little: the design gives no
            // distance (decisions.md, wave3-pixels).
            "slidefade" => vec![(Prop::Opacity, n(0.0)), (Prop::Y, n(8.0))],
            _ => Vec::new(),
        },
        PropValue::Call { name, args } => match name.as_str() {
            "popin" => {
                let s = args.first().and_then(number).unwrap_or(0.8);
                vec![(Prop::Scale, n(s)), (Prop::Opacity, n(0.0))]
            }
            "slide" | "slidefade" => {
                let edge = match args.first() {
                    Some(PropValue::Keyword(k)) => k.as_str(),
                    _ => "bottom",
                };
                let (w, h) = rect.map_or((0.0, 0.0), |r| (r.w, r.h));
                let mut out = match edge {
                    "top" => vec![(Prop::Y, n(-h))],
                    "left" => vec![(Prop::X, n(-w))],
                    "right" => vec![(Prop::X, n(w))],
                    _ => vec![(Prop::Y, n(h))],
                };
                if name == "slidefade" {
                    out.push((Prop::Opacity, n(0.0)));
                }
                out
            }
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// The `exit` pose of a node, else its `enter` (exit mirrors enter).
pub(crate) fn exit_pose(node: &Node) -> Option<&PropValue> {
    node.get(Prop::Exit).or_else(|| node.get(Prop::Enter))
}

/// True if a pose value moves anything.
pub(crate) fn is_pose(v: Option<&PropValue>) -> bool {
    !pose_props(v.unwrap_or(&PropValue::Unset), None).is_empty()
        || matches!(v, Some(PropValue::Call { name, .. }) if name == "slide" || name == "slidefade")
}

/// The `[width, height]` a pose gives `node` (plain lengths only).
pub(super) fn pose_sizes(tree: &SceneTree, node: &Node, pose: &PropValue) -> [Option<f32>; 2] {
    let scopes = crate::flatten::scope_tables(tree, node.id);
    let scope = TokenScope::new(&scopes);
    let Some(pose) = scope.resolve(pose) else {
        return [None, None];
    };
    let mut out = [None, None];
    for (p, v) in pose_props(&pose, None) {
        let n = scope
            .resolve(&v)
            .and_then(|v| number(&v))
            .map(|v| v.max(0.0));
        match p {
            Prop::Width => out[0] = n.or(out[0]),
            Prop::Height => out[1] = n.or(out[1]),
            Prop::Size => {
                out[0] = out[0].or(n);
                out[1] = out[1].or(n);
            }
            _ => {}
        }
    }
    out
}
