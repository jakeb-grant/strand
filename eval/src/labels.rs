//! Node keys and labels (docs/eval.md, "Trace"): a node's key is
//! `id|origin|instance_path#sibling`, from `Instance::identity` and
//! `Instance::origin`, so it is the same in every run and build of a
//! scenario; `NodeId`s are only used within a run. A key is taken when the
//! node is first seen (an exiting ghost, dead to logic, keeps the one it
//! had); a second live node with the same key gets `~2`, `~3`, ….

use std::collections::{BTreeMap, HashMap};

use strand_compiler::SourceMap;
use strand_compiler::instantiate::Instance;
use strand_render::SceneTree;
use strand_scene::{NodeId, Prop, PropValue};

use crate::select::NodeView;
use crate::trace::{NodeMeta, Pose};

/// Keys and metadata of the nodes a run saw.
#[derive(Debug, Default)]
pub struct Labels {
    keys: HashMap<NodeId, String>,
    owners: HashMap<String, NodeId>,
    /// Every node seen, by key.
    pub metas: BTreeMap<String, NodeMeta>,
}

/// What `Instance` says of a node, and where it is written.
struct Source {
    id: Option<String>,
    /// (eval-finish) The other `id:`s that name it.
    also: Vec<String>,
    origin: Option<(String, u32, u32)>,
    instance_path: String,
    sibling: u32,
}

/// The 1-based line and column of byte `at` in `text`.
pub(crate) fn line_col(text: &str, at: usize) -> (u32, u32) {
    let at = at.min(text.len());
    let before = &text[..text.floor_char_boundary(at)];
    let line = before.matches('\n').count() as u32 + 1;
    let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) as u32 + 1;
    (line, col)
}

fn source(inst: &Instance, sources: &SourceMap, node: NodeId) -> Option<Source> {
    let ident = inst.identity(node)?;
    let origin = inst.origin(node).and_then(|(file, _, span)| {
        let f = sources.get(file)?;
        let (l, c) = line_col(&f.text, span.start as usize);
        Some((f.name.clone(), l, c))
    });
    Some(Source {
        id: ident.id,
        also: ident.also,
        origin,
        instance_path: ident.instance_path,
        sibling: ident.sibling_index,
    })
}

/// A number from a pose prop's value.
fn num(v: &PropValue) -> Option<f32> {
    match v {
        PropValue::Number(n) => Some(*n),
        PropValue::Length(strand_scene::Length::Px(n)) => Some(*n),
        _ => None,
    }
}

/// The node's `enter` pose, if it is a pose (not a preset keyword):
/// its opacity, offset and scale, and (unshaped-enter) the width or
/// height it lays out at, as [`exit_pose`] records an exit's.
fn enter_pose(tree: &SceneTree, node: NodeId) -> Option<Pose> {
    let v @ PropValue::Pose(_) = tree.get(node)?.get(Prop::Enter)? else {
        return None;
    };
    pose_of(v)
}

/// (eval-finish) The node has an `enter`: a pose block that sets
/// something, or one of design.md's presets ("Pose presets": `popin(0.8)
/// | slide(top) | slidefade | fade`).
fn enters(tree: &SceneTree, node: NodeId) -> bool {
    match tree.get(node).and_then(|n| n.get(Prop::Enter)) {
        Some(PropValue::Pose(props)) => !props.is_empty(),
        Some(PropValue::Keyword(k)) => k == "fade" || k == "slidefade",
        Some(PropValue::Call { name, .. }) => {
            matches!(name.as_str(), "popin" | "slide" | "slidefade")
        }
        _ => false,
    }
}

/// (eval-rules) The parts of a pose value the checks read: a pose
/// block, or a preset as design.md names them (`fade`, `slidefade`,
/// `popin(s)`; a `slide`'s distance is its box's, unknown here, so only
/// its fade is kept). anim/pose.rs `pose_props` is what render plays.
fn pose_of(v: &PropValue) -> Option<Pose> {
    let mut p = Pose::default();
    match v {
        PropValue::Pose(props) => {
            for (prop, v) in props {
                match prop {
                    Prop::Opacity => p.opacity = num(v),
                    Prop::X => p.x = num(v),
                    Prop::Y => p.y = num(v),
                    Prop::Scale => p.scale = num(v),
                    Prop::Width => p.w = num(v),
                    Prop::Height => p.h = num(v),
                    _ => {}
                }
            }
        }
        PropValue::Keyword(k) if k == "fade" => p.opacity = Some(0.0),
        PropValue::Keyword(k) if k == "slidefade" => {
            p.opacity = Some(0.0);
            p.y = Some(8.0);
        }
        PropValue::Call { name, args } if name == "popin" => {
            p.opacity = Some(0.0);
            p.scale = Some(args.first().and_then(num).unwrap_or(0.8));
        }
        PropValue::Call { name, .. } if name == "slidefade" => p.opacity = Some(0.0),
        _ => return None,
    }
    Some(p)
}

/// (eval-rules) The node's `exit` pose, else its `enter` (an exit
/// mirrors its enter, anim/pose.rs `exit_pose`).
fn exit_pose(tree: &SceneTree, node: NodeId) -> Option<Pose> {
    let n = tree.get(node)?;
    pose_of(n.get(Prop::Exit).or_else(|| n.get(Prop::Enter))?)
}

/// (eval-rules) A container's `stagger:` step in ms, when written as a
/// duration.
fn stagger_of(tree: &SceneTree, node: NodeId) -> Option<f32> {
    match tree.get(node)?.get(Prop::Stagger)? {
        PropValue::Duration(d) if !d.is_zero() => Some(d.as_secs_f32() * 1000.0),
        _ => None,
    }
}

/// (eval-rules) The node clips what it holds to its box: `clip: true`,
/// a `scroll` or a `list`.
fn clips_of(tree: &SceneTree, node: NodeId) -> bool {
    let Some(n) = tree.get(node) else {
        return false;
    };
    matches!(n.kind.name(), "scroll" | "list")
        || matches!(n.get(Prop::Clip), Some(PropValue::Bool(true)))
}

/// (eval-rules) The node's width does not give way in a full row
/// (design.md "Shrinking"; decisions.md wave3-pixels "text in its box"):
/// a `width`, `size` or `min_width` written as a length, `shrink: 0`, an
/// image or icon, or a text that cuts nothing (its minimum is its
/// longest word). An ellipsised or `max_lines` text, and a container
/// with none of these, can shrink to nothing.
fn rigid_of(tree: &SceneTree, node: NodeId) -> bool {
    let Some(n) = tree.get(node) else {
        return false;
    };
    let length = |p: Prop| match n.get(p) {
        Some(PropValue::Number(_)) => true,
        Some(PropValue::Length(l)) => {
            matches!(l, strand_scene::Length::Px(_) | strand_scene::Length::Ch(_))
        }
        _ => false,
    };
    let kind = n.kind.name();
    let cuts = ellipsis_of(tree, node) || n.get(Prop::MaxLines).is_some();
    length(Prop::Width)
        || length(Prop::Size)
        || length(Prop::MinWidth)
        || matches!(n.get(Prop::Shrink), Some(PropValue::Number(s)) if *s == 0.0)
        || matches!(kind, "image" | "icon")
        || (kind == "text" && !cuts)
}

/// (eval-rules-4) A `split`'s padding, or a split part's margin, as
/// written in numbers (`checks::placement`'s `centre`); `None` for any
/// other kind, or when a token, an expression or a split's border leaves
/// it unknown here.
fn inset_of(tree: &SceneTree, node: NodeId) -> Option<[f32; 4]> {
    let n = tree.get(node)?;
    let prop = match n.kind.name() {
        "split" => {
            if n.get(Prop::Border).is_some() {
                return None;
            }
            Prop::Pad
        }
        "start" | "center" | "end" => Prop::Margin,
        _ => return None,
    };
    match n.get(prop) {
        None => Some([0.0; 4]),
        Some(v) => {
            let i = v.insets()?;
            Some([i.top, i.right, i.bottom, i.left])
        }
    }
}

/// (eval-rules-6) How the node sits in its parent's flow, as written
/// ([`crate::trace::Flow`] bits): out of it (`place: absolute`), with a
/// margin of its own, with a width of its own (`width`, `size`,
/// `min_width`, in any form; an image or icon), with a `shrink:` that
/// is not a number over 0, and, for a container, a `justify:` that
/// spreads its children (`space_between`, `space_around`,
/// `space_evenly`). A form not read here (a token, an expression) counts
/// as set: the checks then leave the node alone.
fn flow_of(tree: &SceneTree, node: NodeId) -> u8 {
    use crate::trace::Flow;
    let Some(n) = tree.get(node) else {
        return 0;
    };
    let mut bits = 0;
    match n.get(Prop::Place) {
        None => {}
        Some(PropValue::Keyword(k)) if k != "absolute" => {}
        Some(_) => bits |= Flow::ABSOLUTE,
    }
    if n.get(Prop::Margin).is_some() {
        bits |= Flow::MARGIN;
    }
    if [Prop::Width, Prop::Size, Prop::MinWidth]
        .into_iter()
        .any(|p| n.get(p).is_some())
        || matches!(n.kind.name(), "image" | "icon")
    {
        bits |= Flow::SIZED;
    }
    match n.get(Prop::Shrink) {
        None => {}
        Some(PropValue::Number(s)) if *s > 0.0 => {}
        Some(_) => bits |= Flow::NO_SHRINK,
    }
    match n.get(Prop::Justify) {
        None => {}
        Some(PropValue::Keyword(k)) if !k.starts_with("space") => {}
        Some(_) => bits |= Flow::SPREAD,
    }
    bits
}

/// (integration-r7) [`crate::trace::NodeMeta::x_share`]: where the
/// node's parent places it on x, read from the declared props by
/// spec.md 6.7 and CSS flex, never from the product's layout. A `row`
/// places its children by `justify:` (`start` when unset); a `col` or
/// `scroll` by `align:` (stretched when unset, so a child's width is
/// the column's, not its own); a `stack`, `box` or surface by `align:`
/// on both axes (a child with no width fills the cell when unset). A
/// child with a `margin:` or out of the flow, and a split's part (its
/// `center` centred on the whole split), are left unknown.
fn x_share_of(tree: &SceneTree, node: NodeId) -> Option<f32> {
    use crate::trace::Flow;
    let n = tree.get(node)?;
    if flow_of(tree, node) & (Flow::ABSOLUTE | Flow::MARGIN) != 0 {
        return None;
    }
    let p = tree.get(n.parent?)?;
    let word = |prop: Prop| match p.get(prop) {
        Some(PropValue::Keyword(k)) => Some(Some(k.as_str())),
        None => Some(None),
        _ => None,
    };
    let at = |k: &str| match k {
        "start" => Some(0.0),
        "center" => Some(0.5),
        "end" => Some(1.0),
        _ => None,
    };
    match p.kind.name() {
        "row" => at(word(Prop::Justify)?.unwrap_or("start")),
        "col" | "scroll" => at(word(Prop::Align)?.unwrap_or("stretch")),
        "stack" | "box" | "popup" | "panel" | "bar" | "osd" | "lock" => {
            let own = n.get(Prop::Width).is_some() || n.get(Prop::Size).is_some();
            match word(Prop::Align)? {
                Some(k) => at(k),
                None if own => Some(0.0),
                None => None,
            }
        }
        _ => None,
    }
}

/// (eval-rules) The axes on which a surface is sized by its content: a
/// `popup` or `panel` with no `width` (or `size`) is as wide as what it
/// holds, one with no `height` as tall. A bar is sized by its screen
/// edge and its `height`.
pub fn content_sized(tree: &SceneTree, node: NodeId) -> [bool; 2] {
    let Some(n) = tree.get(node) else {
        return [false, false];
    };
    if !matches!(n.kind.name(), "popup" | "panel") {
        return [false, false];
    }
    let size = n.get(Prop::Size).is_some();
    [
        !size && n.get(Prop::Width).is_none(),
        !size && n.get(Prop::Height).is_none(),
    ]
}

/// The node's shared-element `morph:` name, when it is written as text.
fn morph_of(tree: &SceneTree, node: NodeId) -> Option<String> {
    match tree.get(node)?.get(Prop::Morph)? {
        PropValue::Text(k) | PropValue::Keyword(k) if !k.is_empty() => Some(k.clone()),
        _ => None,
    }
}

/// Whether the node cuts its text with an ellipsis (`ellipsis: end`,
/// `start`, `middle` or `true`).
fn ellipsis_of(tree: &SceneTree, node: NodeId) -> bool {
    match tree.get(node).and_then(|n| n.get(Prop::Ellipsis)) {
        Some(PropValue::Keyword(k)) => strand_text::Ellipsis::from_name(k).is_some(),
        Some(PropValue::Bool(b)) => *b,
        _ => false,
    }
}

fn text_of(tree: &SceneTree, node: NodeId) -> Option<String> {
    match tree.get(node)?.get(Prop::Text)? {
        PropValue::Text(t) => Some(t.clone()),
        _ => None,
    }
}

impl Labels {
    pub fn new() -> Labels {
        Labels::default()
    }

    /// `node`'s key, made (with its metadata) the first time it is asked.
    pub fn key(
        &mut self,
        node: NodeId,
        inst: &Instance,
        tree: &SceneTree,
        sources: &SourceMap,
    ) -> String {
        if let Some(k) = self.keys.get(&node) {
            return k.clone();
        }
        let parent = tree.get(node).and_then(|n| n.parent);
        let parent_key = parent.map(|p| self.key(p, inst, tree, sources));
        let kind = tree.get(node).map_or("?", |n| n.kind.name());
        let src = source(inst, sources, node);
        let base = match &src {
            Some(s) => {
                let origin = s
                    .origin
                    .as_ref()
                    .map_or(String::new(), |(f, l, c)| format!("{f}:{l}:{c}"));
                format!(
                    "{}|{origin}|{}#{}",
                    s.id.as_deref().unwrap_or(""),
                    s.instance_path,
                    s.sibling
                )
            }
            // Render's own (a tooltip) or a node logic no longer has: by
            // its place under its parent.
            None => {
                let at = parent
                    .and_then(|p| tree.get(p))
                    .and_then(|p| p.children.iter().position(|c| *c == node))
                    .unwrap_or(0);
                format!("|{}>{kind}|#{at}", parent_key.as_deref().unwrap_or(""))
            }
        };
        let mut key = base.clone();
        let mut n = 1;
        while let Some(other) = self.owners.get(&key) {
            if *other == node || !tree.contains(*other) {
                break;
            }
            n += 1;
            key = format!("{base}~{n}");
        }
        self.owners.insert(key.clone(), node);
        self.keys.insert(node, key.clone());
        let text = text_of(tree, node);
        let id = src.as_ref().and_then(|s| s.id.clone());
        let origin = src
            .as_ref()
            .and_then(|s| s.origin.as_ref())
            .map(|(f, l, c)| format!("{f}:{l}:{c}"));
        let label = match (&id, &text, &src) {
            (Some(id), _, _) => format!("{kind}#{id}"),
            (None, Some(t), _) => {
                let short: String = t.chars().take(24).collect();
                format!("{kind}({short:?})")
            }
            (
                None,
                None,
                Some(Source {
                    origin: Some((f, l, _)),
                    ..
                }),
            ) => format!("{kind}@{f}:{l}"),
            _ => kind.to_string(),
        };
        self.metas.entry(key.clone()).or_insert(NodeMeta {
            label,
            kind: kind.to_string(),
            origin,
            parent: parent_key,
            id,
            also: src.as_ref().map(|s| s.also.clone()).unwrap_or_default(),
            text,
            enter: enter_pose(tree, node),
            enters: enters(tree, node),
            morph: morph_of(tree, node),
            ellipsis: ellipsis_of(tree, node),
            exit: exit_pose(tree, node),
            stagger: stagger_of(tree, node),
            clips: clips_of(tree, node),
            rigid: rigid_of(tree, node),
            inset: inset_of(tree, node),
            flow: flow_of(tree, node),
            x_share: x_share_of(tree, node),
        });
        key
    }

    /// Nodes seen so far (mounted or not) that `sel` names by id, origin
    /// or text (a kind path needs the live tree).
    pub fn seen_matching(&self, sel: &crate::select::Selector) -> Vec<(NodeId, &str)> {
        use crate::select::Selector;
        let mut out: Vec<(NodeId, &str)> = self
            .keys
            .iter()
            .filter(|(_, k)| {
                let Some(m) = self.metas.get(*k) else {
                    return false;
                };
                match sel {
                    Selector::Id(id) => m.named(id),
                    Selector::Text(t) => m.text.as_deref() == Some(t),
                    Selector::Origin {
                        file, line, col, ..
                    } => m.origin.as_deref().is_some_and(|o| {
                        let mut it = o.rsplitn(3, ':');
                        let (c, l, f) = (it.next(), it.next(), it.next());
                        f.is_some_and(|f| f == file || f.ends_with(&format!("/{file}")))
                            && l == Some(&line.to_string())
                            && col.is_none_or(|col| c == Some(&col.to_string()))
                    }),
                    Selector::Path(_) => false,
                }
            })
            .map(|(n, k)| (*n, k.as_str()))
            .collect();
        out.sort_by(|a, b| a.1.cmp(b.1));
        out.dedup_by(|a, b| a.1 == b.1);
        out
    }

    /// The key already made for `node`.
    pub fn known(&self, node: NodeId) -> Option<&str> {
        self.keys.get(&node).map(String::as_str)
    }

    /// Every node mounted now, in tree order (surface roots in creation
    /// order, each node before its children), for selectors.
    pub fn views(
        &mut self,
        inst: &Instance,
        tree: &SceneTree,
        sources: &SourceMap,
    ) -> Vec<NodeView> {
        let mut out = Vec::new();
        let mut stack: Vec<NodeId> = tree.roots().iter().rev().copied().collect();
        while let Some(n) = stack.pop() {
            let Some(node) = tree.get(n) else { continue };
            let key = self.key(n, inst, tree, sources);
            let src = source(inst, sources, n);
            out.push(NodeView {
                node: n,
                key,
                kind: node.kind,
                parent: node.parent,
                children: node.children.clone(),
                id: src.as_ref().and_then(|s| s.id.clone()),
                also: src.as_ref().map(|s| s.also.clone()).unwrap_or_default(),
                origin: src.as_ref().and_then(|s| s.origin.clone()),
                sibling_index: src.as_ref().map_or(0, |s| s.sibling),
                text: text_of(tree, n),
            });
            stack.extend(node.children.iter().rev().copied());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_and_columns_count_from_one() {
        let t = "ab\ncdé f\n";
        assert_eq!(line_col(t, 0), (1, 1));
        assert_eq!(line_col(t, 3), (2, 1));
        assert_eq!(line_col(t, 8), (2, 5), "é is one column");
        assert_eq!(line_col(t, 99), (3, 1));
    }
}
