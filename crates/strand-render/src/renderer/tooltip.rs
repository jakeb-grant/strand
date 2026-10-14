//! Tooltips (`tooltip: expr`): shown after the pointer rests, as a
//! render-owned popup.

use std::time::{Duration, Instant};

use strand_scene::{NodeId, NodeKind, Prop, PropValue, TokenScope};

use super::Renderer;
use crate::flatten::scope_tables;

/// A tooltip: the node it describes, its text, when it shows, and its
/// render-owned popup once shown.
#[derive(Clone, Debug)]
pub(super) struct Tooltip {
    pub(super) target: NodeId,
    pub(super) text: String,
    pub(super) due: Instant,
    pub(super) popup: Option<NodeId>,
}

impl Renderer {
    /// How long the pointer rests before a tooltip shows (tests shorten
    /// it).
    pub fn set_tooltip_delay(&mut self, delay: Duration) {
        self.tooltip_delay = delay;
    }

    /// The render-owned popup of the tooltip shown, if one is.
    pub fn tooltip_popup(&self) -> Option<NodeId> {
        self.tooltip.as_ref().and_then(|t| t.popup)
    }

    /// The deepest hovered node with a `tooltip`, and its text; none
    /// while a button is held.
    pub(super) fn tooltip_wanted(&self) -> Option<(NodeId, String)> {
        let w = &self.extras.widgets;
        if !w.pressed.is_empty() {
            return None;
        }
        let depth = |mut n: NodeId| {
            let mut d = 0;
            while let Some(p) = self.tree.get(n).and_then(|x| x.parent) {
                d += 1;
                n = p;
            }
            d
        };
        w.hovered
            .iter()
            .filter(|n| self.tree.contains_live(**n))
            .filter_map(|n| {
                let node = self.tree.get(*n)?;
                let v = node.get(Prop::Tooltip)?;
                let tables = scope_tables(&self.tree, *n);
                let scope = TokenScope::new(&tables);
                match scope.resolve(v).as_deref() {
                    Some(PropValue::Text(t)) if !t.trim().is_empty() => Some((*n, t.clone())),
                    _ => None,
                }
            })
            .max_by_key(|(n, _)| (depth(*n), *n))
    }

    /// Hover or a press changed: the tooltip that should show changes
    /// with it (shown after `tooltip_delay` of rest, hidden at once).
    pub(super) fn refresh_tooltip(&mut self) {
        let want = self.tooltip_wanted();
        if let (Some(t), Some((n, text))) = (&mut self.tooltip, &want)
            && t.target == *n
        {
            if t.text != *text {
                t.text = text.clone();
                // Shown: its label takes the new text in place, and its
                // popup lays out and resizes (no new surface: a value
                // changing while hovered does not flicker).
                if let Some(p) = t.popup {
                    let label = self.tree.overlay_children(p).first().copied();
                    if let Some(l) = label {
                        self.tree
                            .set_overlay_prop(l, Prop::Text, PropValue::Text(text.clone()));
                    }
                    for s in self.surfaces.values_mut().filter(|s| s.root == p) {
                        s.mark_layout();
                    }
                    self.spec_dirty.insert(p);
                    self.refresh_specs();
                }
            }
            return;
        }
        self.hide_tooltip();
        let Some((target, text)) = want else {
            return;
        };
        let due = Instant::now() + self.tooltip_delay;
        self.tooltip = Some(Tooltip {
            target,
            text,
            due,
            popup: None,
        });
        self.arm_timer();
    }

    pub(super) fn hide_tooltip(&mut self) {
        if let Some(t) = self.tooltip.take()
            && let Some(p) = t.popup
        {
            self.tree.remove_overlay(p);
            self.refresh_specs();
        }
    }

    /// Shows the waiting tooltip once its delay has passed: a popup under
    /// its node holding its text, styled by the theme's inverse surface
    /// (`$inverse_surface`, `$inverse_on_surface`, `$radius.sm`,
    /// `$font.caption` when the table has them).
    pub(super) fn show_tooltip(&mut self) {
        let Some(t) = &self.tooltip else {
            return;
        };
        if t.popup.is_some() || Instant::now() < t.due {
            return;
        }
        if !self.tree.contains_live(t.target) {
            self.tooltip = None;
            return;
        }
        let (target, text) = (t.target, t.text.clone());
        self.tooltip_seq = self.tooltip_seq.wrapping_add(1);
        let g = self.tooltip_seq;
        let popup = NodeId::new(crate::tree::OVERLAY_INDEX, g);
        let label = NodeId::new(crate::tree::OVERLAY_INDEX + 1, g);
        let tables = scope_tables(&self.tree, target);
        let scope = TokenScope::new(&tables);
        let token = |path: &str, fallback: PropValue| {
            if scope.lookup(path).is_some() {
                PropValue::Token(strand_scene::TokenExpr::path(path))
            } else {
                fallback
            }
        };
        let entry = |prop, value| crate::tree::PropEntry {
            prop,
            value,
            transition: strand_scene::Transition::Instant,
        };
        let mut props = vec![
            entry(Prop::Name, PropValue::Text("tooltip".into())),
            entry(
                Prop::Bg,
                token(
                    "inverse_surface",
                    PropValue::Color(strand_scene::Color::from_rgba8(30, 30, 36, 240)),
                ),
            ),
            entry(
                Prop::Color,
                token(
                    "inverse_on_surface",
                    PropValue::Color(strand_scene::Color::WHITE),
                ),
            ),
            entry(Prop::Radius, token("radius.sm", PropValue::Number(6.0))),
            entry(
                Prop::Pad,
                PropValue::List(vec![PropValue::Number(4.0), PropValue::Number(8.0)]),
            ),
        ];
        if scope.lookup("font.caption").is_some() {
            props.push(entry(
                Prop::Font,
                PropValue::Token(strand_scene::TokenExpr::path("font.caption")),
            ));
        }
        self.tree.add_overlay(vec![
            crate::tree::Node {
                id: popup,
                kind: NodeKind::Popup,
                parent: Some(target),
                children: vec![label],
                props,
                epoch: 0,
            },
            crate::tree::Node {
                id: label,
                kind: NodeKind::Text,
                parent: Some(popup),
                children: Vec::new(),
                props: vec![entry(Prop::Text, PropValue::Text(text))],
                epoch: 0,
            },
        ]);
        if let Some(t) = &mut self.tooltip {
            t.popup = Some(popup);
        }
        self.spec_dirty.insert(popup);
        self.refresh_specs();
    }
}
