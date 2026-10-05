//! A retained mirror of the scene, built from the diffs an instance
//! emits: what render would hold. Tests snapshot it as text, and it
//! checks the diffs are consistent (no op on a dead node, no duplicate
//! create, indices in range).

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use strand_scene::{
    Border, Font, Length, NodeId, NodeKind, Paint, Prop, PropValue, SceneDiff, SceneOp, Shadow,
    TokenExpr, TokenMethod, TokenTable, Transition,
};

#[derive(Clone, Debug)]
struct MNode {
    kind: NodeKind,
    parent: Option<NodeId>,
    children: Vec<NodeId>,
    props: BTreeMap<Prop, PropValue>,
}

/// See the module docs.
#[derive(Clone, Debug, Default)]
pub struct SceneMirror {
    nodes: HashMap<NodeId, MNode>,
    roots: Vec<NodeId>,
    pub tokens: TokenTable,
    /// How many `SetTokens` arrived, and with which transition last.
    pub token_swaps: usize,
    pub last_token_transition: Option<Transition>,
}

impl SceneMirror {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a diff; an inconsistent op is an error naming it.
    pub fn apply(&mut self, diff: &SceneDiff) -> Result<(), String> {
        for op in &diff.ops {
            self.op(op)?;
        }
        Ok(())
    }

    fn op(&mut self, op: &SceneOp) -> Result<(), String> {
        match op {
            SceneOp::Create {
                id,
                kind,
                parent,
                index,
            } => {
                if self.nodes.contains_key(id) {
                    return Err(format!("create of live node {id:?}"));
                }
                let list = self.list_mut(*parent)?;
                if *index as usize > list.len() {
                    return Err(format!(
                        "create of {id:?} at {index} past {} children",
                        list.len()
                    ));
                }
                list.insert(*index as usize, *id);
                self.nodes.insert(
                    *id,
                    MNode {
                        kind: *kind,
                        parent: *parent,
                        children: Vec::new(),
                        props: BTreeMap::new(),
                    },
                );
            }
            SceneOp::Remove { id } => {
                let parent = self
                    .nodes
                    .get(id)
                    .ok_or_else(|| format!("remove of dead node {id:?}"))?
                    .parent;
                self.list_mut(parent)?.retain(|n| n != id);
                self.drop_subtree(*id);
            }
            SceneOp::Move { id, parent, index } => {
                let old = self
                    .nodes
                    .get(id)
                    .ok_or_else(|| format!("move of dead node {id:?}"))?
                    .parent;
                self.list_mut(old)?.retain(|n| n != id);
                let list = self.list_mut(*parent)?;
                if *index as usize > list.len() {
                    return Err(format!("move of {id:?} to {index} past {}", list.len()));
                }
                list.insert(*index as usize, *id);
                if let Some(n) = self.nodes.get_mut(id) {
                    n.parent = *parent;
                }
            }
            SceneOp::SetProp {
                id, prop, value, ..
            } => {
                let n = self
                    .nodes
                    .get_mut(id)
                    .ok_or_else(|| format!("set {prop} on dead node {id:?}"))?;
                if matches!(value, PropValue::Unset) {
                    n.props.remove(prop);
                } else {
                    n.props.insert(*prop, value.clone());
                }
            }
            SceneOp::SetTokens { table, transition } => {
                self.tokens = table.clone();
                self.token_swaps += 1;
                self.last_token_transition = Some(transition.clone());
            }
        }
        Ok(())
    }

    fn list_mut(&mut self, parent: Option<NodeId>) -> Result<&mut Vec<NodeId>, String> {
        match parent {
            None => Ok(&mut self.roots),
            Some(p) => self
                .nodes
                .get_mut(&p)
                .map(|n| &mut n.children)
                .ok_or_else(|| format!("parent {p:?} is dead")),
        }
    }

    fn drop_subtree(&mut self, id: NodeId) {
        if let Some(n) = self.nodes.remove(&id) {
            for c in n.children {
                self.drop_subtree(c);
            }
        }
    }

    /// Live nodes.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn roots(&self) -> &[NodeId] {
        &self.roots
    }

    pub fn kind(&self, id: NodeId) -> Option<NodeKind> {
        self.nodes.get(&id).map(|n| n.kind)
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.nodes.get(&id).and_then(|n| n.parent)
    }

    /// The nearest ancestor of `id` (itself included) of `kind`.
    pub fn ancestor(&self, id: NodeId, kind: NodeKind) -> Option<NodeId> {
        let mut cur = Some(id);
        while let Some(n) = cur {
            if self.kind(n) == Some(kind) {
                return Some(n);
            }
            cur = self.parent(n);
        }
        None
    }

    pub fn children(&self, id: NodeId) -> &[NodeId] {
        self.nodes.get(&id).map_or(&[], |n| n.children.as_slice())
    }

    pub fn prop(&self, id: NodeId, prop: Prop) -> Option<&PropValue> {
        self.nodes.get(&id).and_then(|n| n.props.get(&prop))
    }

    /// Every node, depth first from the roots.
    pub fn walk(&self) -> Vec<NodeId> {
        let mut out = Vec::new();
        fn go(m: &SceneMirror, id: NodeId, out: &mut Vec<NodeId>) {
            out.push(id);
            for &c in m.children(id) {
                go(m, c, out);
            }
        }
        for &r in &self.roots {
            go(self, r, &mut out);
        }
        out
    }

    /// Nodes of `kind`, depth first.
    pub fn of_kind(&self, kind: NodeKind) -> Vec<NodeId> {
        self.walk()
            .into_iter()
            .filter(|&n| self.kind(n) == Some(kind))
            .collect()
    }

    /// The first node whose `text` prop is `text`.
    pub fn find_text(&self, text: &str) -> Option<NodeId> {
        self.walk()
            .into_iter()
            .find(|&n| matches!(self.prop(n, Prop::Text), Some(PropValue::Text(t)) if t == text))
    }

    /// Texts of `text` nodes, depth first.
    pub fn texts(&self) -> Vec<String> {
        self.walk()
            .into_iter()
            .filter_map(|n| match self.prop(n, Prop::Text) {
                Some(PropValue::Text(t)) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    /// The tree as indented text: one node per line, its props in prop
    /// order.
    pub fn render(&self) -> String {
        let mut out = String::new();
        fn go(m: &SceneMirror, id: NodeId, depth: usize, out: &mut String) {
            let Some(n) = m.nodes.get(&id) else { return };
            let _ = write!(out, "{}{}", "  ".repeat(depth), n.kind.name());
            for (p, v) in &n.props {
                let _ = write!(out, " {}={}", p.name(), show(v));
            }
            out.push('\n');
            for &c in &n.children {
                go(m, c, depth + 1, out);
            }
        }
        for &r in &self.roots {
            go(self, r, 0, &mut out);
        }
        out
    }

    /// The token table as text, one token per line.
    pub fn render_tokens(&self) -> String {
        let mut out = String::new();
        for (k, v) in &self.tokens.tokens {
            let _ = writeln!(out, "{k} = {}", show(v));
        }
        for (k, e) in &self.tokens.derived {
            let _ = writeln!(out, "{k} := {}", show_expr(e));
        }
        out
    }
}

fn num(n: f32) -> String {
    crate::vm::value::number(n as f64)
}

/// A prop value as compact text.
pub fn show(v: &PropValue) -> String {
    match v {
        PropValue::Unset => "unset".into(),
        PropValue::Bool(b) => b.to_string(),
        PropValue::Number(n) => num(*n),
        PropValue::Length(l) => match l {
            Length::Px(n) => format!("{}px", num(*n)),
            Length::Percent(n) => format!("{}%", num(*n)),
            Length::Ch(n) => format!("{}ch", num(*n)),
            Length::Auto => "auto".into(),
        },
        PropValue::Insets(i) => format!(
            "{}, {}, {}, {}",
            num(i.top),
            num(i.right),
            num(i.bottom),
            num(i.left)
        ),
        PropValue::Corners(c) => format!(
            "{}, {}, {}, {}",
            num(c.top_left),
            num(c.top_right),
            num(c.bottom_right),
            num(c.bottom_left)
        ),
        PropValue::Color(c) => crate::vm::value::hex(*c),
        PropValue::Paint(p) => paint(p),
        PropValue::Border(Border { width, paint: p }) => format!("{}, {}", num(*width), paint(p)),
        PropValue::Shadow(list) => list.iter().map(shadow).collect::<Vec<_>>().join(", "),
        PropValue::Text(t) => format!("{t:?}"),
        PropValue::Font(Font {
            family,
            size,
            weight,
        }) => format!("{family:?} {}px {weight}", num(*size)),
        PropValue::Keyword(k) => k.clone(),
        PropValue::Angle(a) => format!("{}deg", num(*a)),
        PropValue::Duration(d) => format!("{}ms", d.as_millis()),
        PropValue::List(items) => format!(
            "[{}]",
            items.iter().map(show).collect::<Vec<_>>().join(", ")
        ),
        PropValue::Token(e) => show_expr(e),
        PropValue::Transition(t) => format!("{t:?}"),
        PropValue::Pose(props) => format!(
            "{{{}}}",
            props
                .iter()
                .map(|(p, v)| format!("{}: {}", p.name(), show(v)))
                .collect::<Vec<_>>()
                .join("; ")
        ),
        PropValue::Tokens(t) => {
            let mut parts: Vec<String> = t
                .tokens
                .iter()
                .map(|(k, v)| format!("${k}: {}", show(v)))
                .collect();
            parts.extend(
                t.derived
                    .iter()
                    .map(|(k, e)| format!("${k}: {}", show_expr(e))),
            );
            format!("{{{}}}", parts.join("; "))
        }
        PropValue::Call { name, args } => format!(
            "{name}({})",
            args.iter().map(show).collect::<Vec<_>>().join(", ")
        ),
    }
}

fn paint(p: &Paint) -> String {
    let stops = |s: &[strand_scene::GradientStop]| {
        s.iter()
            .map(|g| crate::vm::value::hex(g.color))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match p {
        Paint::Solid(c) => crate::vm::value::hex(*c),
        Paint::Linear { angle, stops: s } => format!("linear({}deg, {})", num(*angle), stops(s)),
        Paint::Radial { stops: s } => format!("radial({})", stops(s)),
        Paint::Conic { from, stops: s } => format!("conic(from {}deg, {})", num(*from), stops(s)),
    }
}

fn shadow(s: &Shadow) -> String {
    format!(
        "{} {} {} {} {}",
        num(s.x),
        num(s.y),
        num(s.blur),
        num(s.spread),
        crate::vm::value::hex(s.color)
    )
}

/// A token expression as text: `$fg.alpha(0.65)`.
pub fn show_expr(e: &TokenExpr) -> String {
    match e {
        TokenExpr::Ref(p) => format!("${p}"),
        TokenExpr::Value(v) => show(v),
        TokenExpr::Method {
            receiver,
            method,
            args,
        } => {
            let name = match method {
                TokenMethod::Alpha => "alpha",
                TokenMethod::Mix => "mix",
                TokenMethod::Lighten => "lighten",
                TokenMethod::Darken => "darken",
            };
            format!(
                "{}.{name}({})",
                show_expr(receiver),
                args.iter().map(show_expr).collect::<Vec<_>>().join(", ")
            )
        }
        TokenExpr::OklchFrom {
            base,
            l,
            c,
            h,
            alpha,
        } => {
            let mut parts = vec![format!("from {}", show_expr(base))];
            for (n, x) in [("l", l), ("c", c), ("h", h), ("alpha", alpha)] {
                if let Some(x) = x {
                    parts.push(format!("{n}: {}", show_expr(x)));
                }
            }
            format!("oklch({})", parts.join(", "))
        }
        TokenExpr::Channel(c) => format!("{c:?}").to_lowercase(),
        TokenExpr::Binary { op, lhs, rhs } => {
            let o = match op {
                strand_scene::BinOp::Add => "+",
                strand_scene::BinOp::Sub => "-",
                strand_scene::BinOp::Mul => "*",
                strand_scene::BinOp::Div => "/",
            };
            format!("({} {o} {})", show_expr(lhs), show_expr(rhs))
        }
        TokenExpr::Template { value, colors } => {
            let slots: Vec<String> = colors
                .iter()
                .map(|c| c.as_ref().map_or("_".into(), show_expr))
                .collect();
            format!("template({} <- {})", show(value), slots.join(", "))
        }
    }
}
