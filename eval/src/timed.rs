//! (integration-r8) Which nodes time drives, read from the declaration
//! by eval's own walk of the compiled values (`continuity`'s and
//! `twin`'s `still` judge these nodes).
//!
//! design.md "Motion and time" and spec.md 6.6 name the time leaves: `t`,
//! `wave(…)`, and `noise(x)` of an `x` that reads one; a token reads time
//! when its value or expression does, directly or through other tokens;
//! a `tokens` override (`set { $spin: t * 20deg }`) that reads time drives
//! every node of its scope that reads a token. This walk is eval's, not
//! strand-scene's `reads_time_with` or render's `time::reads_time`, so a
//! defect there (a time read it misses) does not blind the check that
//! judges what it drives. Reading too much is safe: a node `still` holds
//! that nothing drives holds still anyway.

use std::collections::HashSet;

use strand_render::{Node, SceneTree};
use strand_scene::{Prop, PropValue, TokenExpr, TokenTable};

/// Whether `e` reads a time leaf, a token reference counting when
/// `timed` holds for its path.
pub fn expr_reads(e: &TokenExpr, timed: &dyn Fn(&str) -> bool) -> bool {
    let any = |e: &TokenExpr| expr_reads(e, timed);
    match e {
        TokenExpr::Time | TokenExpr::Wave { .. } => true,
        TokenExpr::Noise(x) => any(x),
        TokenExpr::Math { args, .. } => args.iter().any(any),
        TokenExpr::Ref(path) => timed(path),
        TokenExpr::Index | TokenExpr::Count | TokenExpr::Channel(_) => false,
        TokenExpr::Value(v) => value_reads(v, timed),
        TokenExpr::Method { receiver, args, .. } => any(receiver) || args.iter().any(any),
        TokenExpr::OklchFrom {
            base,
            l,
            c,
            h,
            alpha,
        } => any(base) || [l, c, h, alpha].into_iter().flatten().any(|e| any(e)),
        TokenExpr::Binary { lhs, rhs, .. } => any(lhs) || any(rhs),
        TokenExpr::Template {
            value,
            colors,
            numbers,
        } => value_reads(value, timed) || colors.iter().chain(numbers).flatten().any(any),
    }
}

/// Whether a prop value holds a time leaf anywhere inside it.
pub fn value_reads(v: &PropValue, timed: &dyn Fn(&str) -> bool) -> bool {
    let any = |v: &PropValue| value_reads(v, timed);
    match v {
        PropValue::Token(e) => expr_reads(e, timed),
        PropValue::List(items) | PropValue::Call { args: items, .. } => items.iter().any(any),
        PropValue::Pose(props) => props.iter().any(|(_, v)| any(v)),
        PropValue::Uniforms(entries) => entries.iter().any(|(_, v)| any(v)),
        PropValue::Keyframes(k) => k.stops.iter().flat_map(|(_, p)| p).any(|(_, v)| any(v)),
        PropValue::Tokens(table) => table_reads(table, timed),
        _ => false,
    }
}

/// Whether a prop value refers to a token anywhere inside it.
pub fn mentions_tokens(v: &PropValue) -> bool {
    fn expr(e: &TokenExpr) -> bool {
        match e {
            TokenExpr::Ref(_) => true,
            TokenExpr::Value(v) => mentions_tokens(v),
            TokenExpr::Noise(x) => expr(x),
            TokenExpr::Math { args, .. } => args.iter().any(expr),
            TokenExpr::Method { receiver, args, .. } => expr(receiver) || args.iter().any(expr),
            TokenExpr::OklchFrom {
                base,
                l,
                c,
                h,
                alpha,
            } => expr(base) || [l, c, h, alpha].into_iter().flatten().any(|e| expr(e)),
            TokenExpr::Binary { lhs, rhs, .. } => expr(lhs) || expr(rhs),
            TokenExpr::Template {
                value,
                colors,
                numbers,
            } => mentions_tokens(value) || colors.iter().chain(numbers).flatten().any(expr),
            TokenExpr::Wave { phase, .. } => expr(phase),
            TokenExpr::Time | TokenExpr::Index | TokenExpr::Count | TokenExpr::Channel(_) => false,
        }
    }
    match v {
        PropValue::Token(e) => expr(e),
        PropValue::List(items) | PropValue::Call { args: items, .. } => {
            items.iter().any(mentions_tokens)
        }
        PropValue::Pose(props) => props.iter().any(|(_, v)| mentions_tokens(v)),
        PropValue::Uniforms(entries) => entries.iter().any(|(_, v)| mentions_tokens(v)),
        PropValue::Keyframes(k) => k
            .stops
            .iter()
            .flat_map(|(_, p)| p)
            .any(|(_, v)| mentions_tokens(v)),
        _ => false,
    }
}

/// Whether any entry of a token table reads time.
pub fn table_reads(table: &TokenTable, timed: &dyn Fn(&str) -> bool) -> bool {
    table.tokens.values().any(|v| value_reads(v, timed))
        || table.derived.values().any(|e| expr_reads(e, timed))
}

/// Every path of the table whose token reads time, directly or through
/// the table's other tokens: to a fixed point, each round adding the
/// tokens that read one found so far.
pub fn time_paths(table: &TokenTable) -> HashSet<String> {
    let mut found = HashSet::new();
    loop {
        let more: Vec<String> = {
            let known = |p: &str| found.contains(p);
            let plain = table
                .tokens
                .iter()
                .filter(|(p, v)| !found.contains(*p) && value_reads(v, &known))
                .map(|(p, _)| p);
            let derived = table
                .derived
                .iter()
                .filter(|(p, e)| !found.contains(*p) && expr_reads(e, &known))
                .map(|(p, _)| p);
            plain.chain(derived).cloned().collect()
        };
        if more.is_empty() {
            return found;
        }
        found.extend(more);
    }
}

/// Whether time drives `node` by a prop of its own (its `play` aside), or
/// by a `tokens` override of it or an ancestor that reads time while it
/// reads a token. `global` is [`time_paths`] of the tree's own table.
pub fn reads_time(tree: &SceneTree, node: &Node, global: &HashSet<String>) -> bool {
    let timed = |p: &str| global.contains(p);
    let own = node.props.iter().any(|e| match (&e.prop, &e.value) {
        (Prop::Play, _) => false,
        (_, v) => value_reads(v, &timed),
    });
    if own {
        return true;
    }
    let reads_token = node
        .props
        .iter()
        .any(|e| e.prop != Prop::Tokens && mentions_tokens(&e.value));
    if !reads_token {
        return false;
    }
    let mut at = node.parent;
    while let Some(id) = at {
        let Some(a) = tree.get(id) else { break };
        if let Some(PropValue::Tokens(table)) = a.get(Prop::Tokens)
            && table_reads(table, &timed)
        {
            return true;
        }
        at = a.parent;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use strand_scene::BinOp;

    fn wave() -> TokenExpr {
        TokenExpr::Wave {
            period: Duration::from_secs(2),
            phase: Box::new(TokenExpr::value(PropValue::Number(0.0))),
        }
    }

    fn times(lhs: TokenExpr, k: f32) -> TokenExpr {
        TokenExpr::Binary {
            op: BinOp::Mul,
            lhs: Box::new(lhs),
            rhs: Box::new(TokenExpr::value(PropValue::Number(k))),
        }
    }

    /// `pulse: 8 * wave(2s)`, `glow: $pulse * 2`, `halo: $glow.alpha(..)`
    /// read time through each other; `gap: 4` and `noise(3)` do not.
    #[test]
    fn a_token_reads_time_through_the_tokens_it_reads() {
        let mut table = TokenTable::default();
        table.derived.insert("pulse".into(), times(wave(), 8.0));
        table
            .derived
            .insert("glow".into(), times(TokenExpr::path("pulse"), 2.0));
        table.derived.insert(
            "halo".into(),
            TokenExpr::Noise(Box::new(TokenExpr::path("glow"))),
        );
        table.derived.insert(
            "still".into(),
            TokenExpr::Noise(Box::new(TokenExpr::value(PropValue::Number(3.0)))),
        );
        table.tokens.insert("gap".into(), PropValue::Number(4.0));
        let paths = time_paths(&table);
        let mut got: Vec<_> = paths.iter().map(String::as_str).collect();
        got.sort_unstable();
        assert_eq!(got, ["glow", "halo", "pulse"]);
        let timed = |p: &str| paths.contains(p);
        assert!(value_reads(
            &PropValue::Token(TokenExpr::path("halo")),
            &timed
        ));
        assert!(!value_reads(
            &PropValue::Token(TokenExpr::path("gap")),
            &timed
        ));
        assert!(value_reads(
            &PropValue::List(vec![
                PropValue::Number(1.0),
                PropValue::Token(TokenExpr::Time)
            ]),
            &timed
        ));
    }

    /// A `tokens` override that reads time, and a value that mentions a
    /// token under it.
    #[test]
    fn a_timed_override_and_the_tokens_read_under_it() {
        let mut table = TokenTable::default();
        table
            .derived
            .insert("spin".into(), times(TokenExpr::Time, 20.0));
        let none = |_: &str| false;
        assert!(table_reads(&table, &none));
        assert!(value_reads(&PropValue::Tokens(Box::new(table)), &none));
        assert!(mentions_tokens(&PropValue::Token(TokenExpr::path("spin"))));
        assert!(!mentions_tokens(&PropValue::Number(1.0)));
        assert!(!mentions_tokens(&PropValue::Token(TokenExpr::Time)));
    }
}
