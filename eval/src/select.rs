//! Selectors: how a scenario names nodes (docs/eval.md, "Selectors").
//!
//! - `#id`: the node's `id:` (`Instance::identity`);
//! - `@file.strand:LINE[:COL][n]`: the element written there
//!   (`Instance::origin`), `[n]` its sibling index among copies;
//! - `text("…")`: a node whose text is exactly that;
//! - a kind path from a surface root, `bar>split>end>row>box[2]`, `[n]`
//!   the index among siblings of that kind (from 0).
//!
//! A selector resolves against the nodes mounted when its step runs
//! ([`NodeView`]); one that matches nothing is a typed error at the
//! step's line.

use std::fmt;

use strand_scene::{NodeId, NodeKind};

/// A parsed selector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selector {
    Id(String),
    Origin {
        file: String,
        line: u32,
        col: Option<u32>,
        nth: Option<u32>,
    },
    Text(String),
    Path(Vec<PathSeg>),
}

/// One step of a kind path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathSeg {
    pub kind: NodeKind,
    pub nth: Option<u32>,
}

/// A mounted node as selectors see it (`labels::Labels::view`).
#[derive(Clone, Debug, PartialEq)]
pub struct NodeView {
    pub node: NodeId,
    pub key: String,
    pub kind: NodeKind,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
    pub id: Option<String>,
    /// Source file name, 1-based line and column of the element.
    pub origin: Option<(String, u32, u32)>,
    pub sibling_index: u32,
    pub text: Option<String>,
}

/// `[n]` at the end of `s`: the rest and n.
fn nth_suffix(s: &str) -> Result<(&str, Option<u32>), String> {
    let Some(body) = s.strip_suffix(']') else {
        return Ok((s, None));
    };
    let Some(open) = body.rfind('[') else {
        return Err(format!("`{s}`: `]` without `[`"));
    };
    let n = body[open + 1..]
        .parse::<u32>()
        .map_err(|_| format!("`{s}`: `[n]` takes a number from 0"))?;
    Ok((&body[..open], Some(n)))
}

impl Selector {
    /// Parses one selector token.
    pub fn parse(s: &str) -> Result<Selector, String> {
        if let Some(id) = s.strip_prefix('#') {
            if id.is_empty() || !id.chars().all(|c| c.is_alphanumeric() || c == '_') {
                return Err(format!("`{s}`: an id is a name (`#bars`)"));
            }
            return Ok(Selector::Id(id.to_string()));
        }
        if let Some(rest) = s.strip_prefix('@') {
            let (rest, nth) = nth_suffix(rest)?;
            let bad = || format!("`{s}`: an origin is `@file.strand:LINE[:COL][n]`, lines from 1");
            let at = rest.rfind(".strand:").ok_or_else(bad)?;
            let file = rest[..at + ".strand".len()].to_string();
            let num = |t: &str| t.parse::<u32>().ok().filter(|n| *n > 0);
            let mut nums = rest[at + ".strand:".len()..].split(':');
            let line = nums.next().and_then(num).ok_or_else(bad)?;
            let col = match nums.next() {
                Some(c) => Some(num(c).ok_or_else(bad)?),
                None => None,
            };
            if nums.next().is_some() {
                return Err(bad());
            }
            return Ok(Selector::Origin {
                file,
                line,
                col,
                nth,
            });
        }
        if let Some(rest) = s.strip_prefix("text(") {
            let inner = rest
                .strip_suffix(')')
                .and_then(|r| r.strip_prefix('"'))
                .and_then(|r| r.strip_suffix('"'))
                .ok_or_else(|| format!("`{s}`: write `text(\"…\")`"))?;
            return Ok(Selector::Text(unescape(inner)));
        }
        let mut segs = Vec::new();
        for part in s.split('>') {
            let (name, nth) = nth_suffix(part.trim())?;
            let kind = NodeKind::from_name(name).ok_or_else(|| {
                format!("`{s}`: `{name}` is not an element kind (a kind path is `bar>row>box[2]`)")
            })?;
            segs.push(PathSeg { kind, nth });
        }
        if segs.is_empty() {
            return Err("an empty selector".into());
        }
        Ok(Selector::Path(segs))
    }

    /// The nodes of `nodes` it names, in `nodes`' order (tree order).
    /// `roots` are the surface roots, in creation order.
    pub fn resolve(&self, nodes: &[NodeView], roots: &[NodeId]) -> Vec<NodeId> {
        match self {
            Selector::Id(id) => nodes
                .iter()
                .filter(|n| n.id.as_deref() == Some(id))
                .map(|n| n.node)
                .collect(),
            Selector::Origin {
                file,
                line,
                col,
                nth,
            } => nodes
                .iter()
                .filter(|n| {
                    n.origin.as_ref().is_some_and(|(f, l, c)| {
                        (f == file || f.ends_with(&format!("/{file}")))
                            && l == line
                            && col.is_none_or(|col| col == *c)
                    }) && nth.is_none_or(|i| i == n.sibling_index)
                })
                .map(|n| n.node)
                .collect(),
            Selector::Text(t) => nodes
                .iter()
                .filter(|n| n.text.as_deref() == Some(t))
                .map(|n| n.node)
                .collect(),
            Selector::Path(segs) => {
                let get = |id: NodeId| nodes.iter().find(|n| n.node == id);
                let pick = |cands: &[NodeId], seg: &PathSeg| -> Vec<NodeId> {
                    let of_kind: Vec<NodeId> = cands
                        .iter()
                        .copied()
                        .filter(|c| get(*c).is_some_and(|n| n.kind == seg.kind))
                        .collect();
                    match seg.nth {
                        Some(i) => of_kind.get(i as usize).copied().into_iter().collect(),
                        None => of_kind,
                    }
                };
                let mut at = pick(roots, &segs[0]);
                for seg in &segs[1..] {
                    let mut next = Vec::new();
                    for n in &at {
                        if let Some(v) = get(*n) {
                            next.extend(pick(&v.children, seg));
                        }
                    }
                    at = next;
                }
                at
            }
        }
    }
}

/// `\"` and `\\` in a quoted string.
pub(crate) fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some(c) => out.push(c),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

impl fmt::Display for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Selector::Id(id) => write!(f, "#{id}"),
            Selector::Origin {
                file,
                line,
                col,
                nth,
            } => {
                write!(f, "@{file}:{line}")?;
                if let Some(c) = col {
                    write!(f, ":{c}")?;
                }
                if let Some(n) = nth {
                    write!(f, "[{n}]")?;
                }
                Ok(())
            }
            Selector::Text(t) => write!(f, "text({t:?})"),
            Selector::Path(segs) => {
                for (i, s) in segs.iter().enumerate() {
                    if i > 0 {
                        f.write_str(">")?;
                    }
                    f.write_str(s.kind.name())?;
                    if let Some(n) = s.nth {
                        write!(f, "[{n}]")?;
                    }
                }
                Ok(())
            }
        }
    }
}
