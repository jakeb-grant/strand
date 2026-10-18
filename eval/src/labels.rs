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
    origin: Option<(String, u32, u32)>,
    instance_path: String,
    sibling: u32,
}

/// The 1-based line and column of byte `at` in `text`.
fn line_col(text: &str, at: usize) -> (u32, u32) {
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

/// The node's `enter` pose, if it is a pose (not a preset keyword).
fn enter_pose(tree: &SceneTree, node: NodeId) -> Option<Pose> {
    let PropValue::Pose(props) = tree.get(node)?.get(Prop::Enter)? else {
        return None;
    };
    let mut p = Pose::default();
    for (prop, v) in props {
        match prop {
            Prop::Opacity => p.opacity = num(v),
            Prop::X => p.x = num(v),
            Prop::Y => p.y = num(v),
            Prop::Scale => p.scale = num(v),
            _ => {}
        }
    }
    Some(p)
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
            text,
            enter: enter_pose(tree, node),
            morph: morph_of(tree, node),
            ellipsis: ellipsis_of(tree, node),
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
                    Selector::Id(id) => m.id.as_deref() == Some(id),
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
