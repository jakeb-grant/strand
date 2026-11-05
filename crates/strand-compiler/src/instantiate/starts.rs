//! (list-start) A `list`'s start read in the tick that sets it.
//!
//! The router takes a list's start (`selected_index: n`) as it settles
//! the diff that sets it, and says so with `Intent::SelectedIndex`; logic
//! heard it a tick later, so for one frame a binding over
//! `results.selected_index` (a preview, a highlight) showed the row
//! selected before (decisions.md list-start). Logic knows its start
//! when it sets it and, with the router's word on whether the user moved
//! the selection since it last started, when the router will take it: as
//! the list mounts, when the start changes while the user has not moved
//! it, and when it starts again: its rows replaced whole, or a new text
//! in the focused input it is the `nav:` list of.
//! Then `results.selected_index` reads the start in that tick, before
//! the diff goes, so its `on change` and what reads it are in the same
//! diff. A start that changed while the user has not moved is read
//! earlier, in the flush that changes it (`mount.rs`, `read_start`), so
//! what reads it runs once, with it; the rest are read here, at the end
//! of the flush. The router tells logic where the selection is after each start
//! it was given either way, so a start logic read and the router did not
//! take (the user's move still on its way) is put right.

use std::collections::{HashMap, HashSet};

use strand_scene::{ListAsk, NodeAsk, NodeId, NodeKind, Prop, PropValue, SceneOp};

use super::Instance;
use crate::vm::value::Value;

impl Instance {
    /// Reads the start of each list the router takes one on in this
    /// tick's ops so far as its `selected_index`; returns whether any
    /// changed, so the tick flushes again for what reads it.
    pub(super) fn read_starts(&self) -> bool {
        let em = self.ctx.em.borrow();
        let ops = &em.ops;
        let mut lists: Vec<NodeId> = Vec::new();
        let mut texts: Vec<NodeId> = Vec::new();
        for op in ops {
            match op {
                SceneOp::SetProp {
                    id,
                    prop: Prop::SelectedIndex,
                    ..
                } => lists.push(*id),
                SceneOp::SetProp {
                    id,
                    prop: Prop::Text,
                    ..
                } => texts.push(*id),
                _ => {}
            }
        }
        // The `nav:` lists of focused inputs whose text this tick set: the
        // router starts them again on the new text, as a new query.
        let typed: HashSet<NodeId> = texts
            .iter()
            .filter_map(|i| {
                let e = em.nodes.get(i).filter(|e| e.kind == NodeKind::Input)?;
                let Some(PropValue::Node(list)) = em.sent.get(i)?.get(&Prop::Nav) else {
                    return None;
                };
                e.state
                    .focused
                    .get_untracked(&self.rt)
                    .unwrap_or(false)
                    .then_some(*list)
            })
            .collect();
        if lists.is_empty() && typed.is_empty() && em.left_rows.is_empty() {
            return false;
        }
        let set: HashSet<NodeId> = lists.iter().copied().collect();
        lists.extend(typed.iter().copied());
        // Lists that lost a row logic removed (not one a window let go).
        let mut replaced: HashSet<NodeId> = HashSet::new();
        if !em.left_rows.is_empty() {
            let gone: HashSet<NodeId> = ops
                .iter()
                .filter_map(|op| match op {
                    SceneOp::Remove { id, window: false } => Some(*id),
                    _ => None,
                })
                .collect();
            for (list, row) in &em.left_rows {
                if gone.contains(row) {
                    replaced.insert(*list);
                }
            }
        }
        lists.extend(replaced.iter().copied());
        lists.sort();
        lists.dedup();
        let mut created: HashSet<NodeId> = HashSet::new();
        let mut arrived: HashMap<NodeId, HashSet<NodeId>> = HashMap::new();
        for op in ops {
            if let SceneOp::Create {
                id, parent, window, ..
            } = op
            {
                created.insert(*id);
                if let (Some(p), false) = (parent, window) {
                    arrived.entry(*p).or_default().insert(*id);
                }
            }
        }
        let asks = self.ctx.asks.borrow();
        let mut changed = false;
        for list in lists {
            let Some(entry) = em.nodes.get(&list).filter(|e| e.kind == NodeKind::List) else {
                continue;
            };
            // Where it starts, as render reads it: below 0 is the first
            // row (the cast saturates).
            let n = match em.sent.get(&list).and_then(|s| s.get(&Prop::SelectedIndex)) {
                Some(PropValue::Number(f)) if f.is_finite() => *f as u32,
                _ => continue,
            };
            // A move a handler asked of it in this tick goes after the
            // start: where it ends is the router's to say.
            let asked = asks.iter().any(|(node, ask)| {
                *node == list && matches!(ask, NodeAsk::List(a) if !matches!(a, ListAsk::Activate))
            });
            if asked {
                continue;
            }
            let state = &entry.state;
            let index = state.selected_index(&self.rt);
            let held = index
                .get_untracked(&self.rt)
                .is_ok_and(|v| !matches!(v, Value::Null));
            // Every row it has now came in this tick, and it had a
            // selection: it starts again, and forgets the user's move.
            let again = replaced.contains(&list)
                && held
                && em
                    .children_of(Some(list))
                    .iter()
                    .all(|r| arrived.get(&list).is_some_and(|a| a.contains(r)));
            let again = again || typed.contains(&list);
            let follows = created.contains(&list)
                || again
                || (set.contains(&list) && !state.list_moved.get());
            if !follows {
                continue;
            }
            if again {
                state.list_moved.set(false);
            }
            let v = Value::int(i64::from(n));
            if index.get_untracked(&self.rt).ok().as_ref() != Some(&v) {
                let _ = index.set(&self.rt, v);
                changed = true;
            }
        }
        changed
    }
}

/// A tick that flushed again for a start (from op `from` on) still sends
/// one value per prop, as any tick does (design.md, "One tick, one
/// diff"): a prop set again then is sent once, at its first place and
/// with its first transition (a node created in the tick keeps its
/// initial one), with its last value. The router reads some props op by
/// op (an `input`'s text answering its edits, a list's start), so it
/// never sees a value no frame was to show.
pub(super) fn merge_sets(ops: &mut Vec<SceneOp>, from: usize) {
    let mut last: HashMap<(NodeId, Prop), usize> = HashMap::new();
    for (i, op) in ops.iter().enumerate().skip(from) {
        if let SceneOp::SetProp { id, prop, .. } = op {
            last.insert((*id, *prop), i);
        }
    }
    if last.is_empty() {
        return;
    }
    let mut first: HashMap<(NodeId, Prop), usize> = HashMap::new();
    let mut drop = vec![false; ops.len()];
    for i in 0..ops.len() {
        let SceneOp::SetProp { id, prop, .. } = &ops[i] else {
            continue;
        };
        let key = (*id, *prop);
        if !last.contains_key(&key) {
            continue;
        }
        match first.get(&key) {
            None => {
                first.insert(key, i);
                if let Some(&j) = last.get(&key).filter(|j| **j != i)
                    && let SceneOp::SetProp { value, .. } = &ops[j]
                {
                    let value = value.clone();
                    if let SceneOp::SetProp { value: v, .. } = &mut ops[i] {
                        *v = value;
                    }
                }
            }
            Some(_) => drop[i] = true,
        }
    }
    let mut i = 0;
    ops.retain(|_| {
        let keep = !drop[i];
        i += 1;
        keep
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use strand_scene::Transition;

    fn set(id: NodeId, prop: Prop, v: f32, transition: Transition) -> SceneOp {
        SceneOp::SetProp {
            id,
            prop,
            value: PropValue::Number(v),
            transition,
        }
    }

    /// A prop set again after `from` is sent once, where it was first set
    /// and with that transition, with its last value; props set once, and
    /// one set twice before `from` only, are left as they are.
    #[test]
    fn a_prop_set_again_for_a_start_is_sent_once() {
        let (a, b) = (NodeId::new(1, 0), NodeId::new(2, 0));
        let create = SceneOp::Create {
            id: b,
            kind: NodeKind::Box,
            parent: Some(a),
            index: 0,
            window: false,
        };
        let mut ops = vec![
            set(a, Prop::Width, 1.0, Transition::Instant),
            set(a, Prop::Height, 5.0, Transition::Default),
            set(a, Prop::Height, 6.0, Transition::Default),
            create.clone(),
            set(a, Prop::Width, 2.0, Transition::Default),
            set(b, Prop::Width, 3.0, Transition::Default),
            set(a, Prop::Width, 4.0, Transition::Default),
        ];
        merge_sets(&mut ops, 4);
        assert_eq!(
            ops,
            [
                set(a, Prop::Width, 4.0, Transition::Instant),
                set(a, Prop::Height, 5.0, Transition::Default),
                set(a, Prop::Height, 6.0, Transition::Default),
                create,
                set(b, Prop::Width, 3.0, Transition::Default),
            ]
        );
    }
}
