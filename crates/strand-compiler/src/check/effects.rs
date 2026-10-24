//! Effect props whose values the type system cannot place (M4).
//!
//! `filter:` and `backdrop:` share the schema's `Filter` type, so
//! `filter: glass()` and `backdrop: bloom(12)` type-check (decisions.md,
//! m4-scene, "the bundled effects' signatures"). They mean nothing:
//! design.md spells liquid glass only as `backdrop: glass()`, and a
//! backdrop is a blur over your own content ("`backdrop: blur(16)` inside
//! a surface") or glass, never a colour filter or a bundled filter pass,
//! which work on the node's own subtree. So this check reports a function
//! in the wrong prop, in the props and in `when` blocks, wherever a call
//! can be seen in the value (through `?:`, `match` arms and lists).
//!
//! It also catches knobs that can only be mistakes when written as
//! literals: a negative blur or bloom radius, a `grain` outside 0..1.
//! (graph-follow) And a `range:` that is not two ends (`range: 0, 100`)
//! or `auto`: a lone number type-checks as the shorthand's first part,
//! but a range needs both of its ends, and literal ends must rise.

use super::Checker;
use crate::hir::{self, Callee, ExprKind, Node};

/// What `backdrop:` takes: a blur over what is behind the node, or glass.
const BACKDROP: &[&str] = &["blur", "glass"];

/// What `filter:` takes: the colour functions, a blur of the subtree, and
/// the bundled filter passes (decisions.md, m4-owner).
const FILTER: &[&str] = &[
    "blur",
    "grayscale",
    "saturate",
    "hue",
    "brightness",
    "contrast",
    "invert",
    "tint",
    "bloom",
    "crt",
    "chromatic",
    "wobble",
];

impl Checker<'_> {
    /// Reports misplaced `filter:` and `backdrop:` functions and literal
    /// knobs out of range, among `props` and the `when` blocks in
    /// `children`.
    pub(super) fn effect_props(&mut self, props: &[hir::Prop], children: &[Node]) {
        let whens = children.iter().filter_map(|n| match n {
            Node::When(w) => Some(&w.props),
            _ => None,
        });
        let all: Vec<&hir::Prop> = props.iter().chain(whens.flatten()).collect();
        for p in all {
            let allowed = match p.name.as_str() {
                "backdrop" => BACKDROP,
                "filter" => FILTER,
                "grain" => {
                    self.grain_amount(&p.value);
                    continue;
                }
                "range" => {
                    self.range_ends(&p.value);
                    continue;
                }
                _ => continue,
            };
            let mut calls = Vec::new();
            builtin_calls(&p.value, &mut calls);
            for (name, args, span) in calls {
                if FILTER.contains(&name) || BACKDROP.contains(&name) {
                    if !allowed.contains(&name) {
                        self.misplaced_filter(&p.name, name, span);
                    }
                    self.filter_knobs(name, args);
                }
            }
        }
    }

    fn misplaced_filter(&mut self, prop: &str, name: &str, span: crate::syntax::Span) {
        let (help, label) = if prop == "backdrop" {
            (
                format!(
                    "`{name}()` works on the node's own pixels: write `filter: {name}(…)`; \
                     a backdrop is `blur(…)` or `glass()`"
                ),
                "not a backdrop",
            )
        } else {
            (
                "glass refracts what is behind the node: write `backdrop: glass()`".to_string(),
                "not a filter",
            )
        };
        self.error(
            "check::misplaced_filter",
            format!("`{name}()` does not go in `{prop}:`"),
            span,
            label,
        )
        .help = Some(help);
    }

    /// A literal negative radius (`blur(-4)`, `bloom(-2)`) is an error: a
    /// Gaussian has no negative spread.
    fn filter_knobs(&mut self, name: &str, args: &[hir::CallArg]) {
        if !matches!(name, "blur" | "bloom") {
            return;
        }
        if let Some(a) = args.first()
            && let Some(v) = literal(&a.value)
            && v < 0.0
        {
            self.error(
                "check::effect_range",
                format!("`{name}()` takes a radius of 0 or more"),
                a.span,
                "negative",
            );
        }
    }

    /// `grain: 0.04` is an amount of noise, 0 to 1.
    fn grain_amount(&mut self, value: &hir::Expr) {
        if let Some(v) = literal(value)
            && !(0.0..=1.0).contains(&v)
        {
            self.error(
                "check::effect_range",
                "`grain` is an amount from 0 to 1",
                value.span,
                "out of range",
            )
            .help = Some("film grain reads well around `grain: 0.04`".to_string());
        }
    }
}

impl Checker<'_> {
    /// (graph-follow) `range:` is two ends or `auto`: a single number (a
    /// literal or anything of a number's type) is the comma shorthand's
    /// first part to the type checker, an end with no other; and two
    /// literal ends must rise (`range: 100, 0` and `range: 5, 5` hold
    /// nothing).
    fn range_ends(&mut self, value: &hir::Expr) {
        let ends = match &value.kind {
            ExprKind::Commas(items) => items,
            _ if value.ty.is_numeric() => {
                self.error(
                    "check::range_ends",
                    "`range` takes its two ends, or `auto`",
                    value.span,
                    "one end",
                )
                .help = Some(
                    "write both: `range: 0, 100`; `range: 0, auto` follows the values for the \
                     top"
                    .into(),
                );
                return;
            }
            _ => return,
        };
        if let [lo, hi] = ends.as_slice()
            && let (Some(a), Some(b)) = (literal(lo), literal(hi))
            && a >= b
        {
            self.error(
                "check::range_ends",
                "`range` runs from its lower end to its upper one",
                value.span,
                if a == b { "empty" } else { "reversed" },
            )
            .help = Some(if a == b {
                "its two ends must differ".to_string()
            } else {
                format!("the lower end first: `range: {b}, {a}`")
            });
        }
    }
}

/// A literal number, negated or not.
fn literal(e: &hir::Expr) -> Option<f64> {
    match &e.kind {
        ExprKind::Number { value, .. } => Some(*value),
        ExprKind::Unary {
            op: hir::UnaryOp::Neg,
            expr,
        } => literal(expr).map(|v| -v),
        _ => None,
    }
}

/// The builtin calls a value can evaluate to: the call itself, the
/// branches of a `?:` or `match`, and the items of a list.
fn builtin_calls<'e>(
    e: &'e hir::Expr,
    out: &mut Vec<(&'e str, &'e [hir::CallArg], crate::syntax::Span)>,
) {
    match &e.kind {
        ExprKind::Call {
            callee: Callee::Builtin { name, .. },
            args,
        } => out.push((name.as_str(), args.as_slice(), e.span)),
        ExprKind::Ternary { then, else_, .. } => {
            builtin_calls(then, out);
            builtin_calls(else_, out);
        }
        ExprKind::Match { arms, .. } => {
            for (_, arm) in arms {
                builtin_calls(arm, out);
            }
        }
        ExprKind::List(items) | ExprKind::Commas(items) | ExprKind::Spaced(items) => {
            for i in items {
                builtin_calls(i, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use crate::SourceMap;

    fn codes(src: &str) -> Vec<(String, String)> {
        let mut map = SourceMap::new();
        map.add("effects.strand", src.to_string());
        let c = crate::compile_with(&map, crate::schema::Schema::builtin());
        c.diagnostics
            .iter()
            .filter(|d| d.code != "check::raw_color")
            .map(|d| (d.code.to_string(), d.message.clone()))
            .collect()
    }

    /// Every function in the prop design.md gives it checks clean.
    #[test]
    fn filters_and_backdrops_in_their_props_check() {
        let src = r#"
state on = false
bar Top { edge: top; height: 30
  box { filter: grayscale(1) }
  box { filter: saturate(1.3) }
  box { filter: hue(30deg) }
  box { filter: brightness(0.9) }
  box { filter: contrast(1.2) }
  box { filter: invert(1) }
  box { filter: tint($accent) }
  box { filter: blur(30) }
  box { filter: bloom(12) }
  box { filter: crt() }
  box { filter: chromatic(2) }
  box { filter: wobble(4) }
  box { filter: [grayscale(1), brightness(0.9)] }
  box { backdrop: blur(16) }
  box { backdrop: glass() }
  box { backdrop: on ? glass() : blur(8); grain: 0.04 }
}
"#;
        assert_eq!(codes(src), Vec::<(String, String)>::new());
    }

    /// A function in the other prop is an error, written plainly, in a
    /// branch, in a list or in a `when`.
    #[test]
    fn misplaced_filter_and_backdrop_values_are_errors() {
        let src = r#"
state on = false
bar Top { edge: top; height: 30
  box { filter: glass() }
  box { backdrop: bloom(12) }
  box { backdrop: grayscale(1) }
  box { backdrop: on ? blur(4) : crt() }
  box { filter: [grayscale(1), glass()] }
  box { when on { backdrop: tint($accent) } }
}
"#;
        let got = codes(src);
        let msgs: Vec<&str> = got
            .iter()
            .filter(|(c, _)| c == "check::misplaced_filter")
            .map(|(_, m)| m.as_str())
            .collect();
        assert_eq!(
            msgs,
            [
                "`glass()` does not go in `filter:`",
                "`bloom()` does not go in `backdrop:`",
                "`grayscale()` does not go in `backdrop:`",
                "`crt()` does not go in `backdrop:`",
                "`glass()` does not go in `filter:`",
                "`tint()` does not go in `backdrop:`",
            ],
            "{got:?}"
        );
    }

    /// Literal knobs out of range: a negative blur or bloom radius, grain
    /// past 1.
    #[test]
    fn literal_knobs_out_of_range_are_errors() {
        let src = r#"
bar Top { edge: top; height: 30
  box { filter: blur(-4) }
  box { filter: bloom(-1) }
  box { grain: 2 }
  box { grain: 0.5; filter: blur(0) }
}
"#;
        let got = codes(src);
        let msgs: Vec<&str> = got
            .iter()
            .filter(|(c, _)| c == "check::effect_range")
            .map(|(_, m)| m.as_str())
            .collect();
        assert_eq!(
            msgs,
            [
                "`blur()` takes a radius of 0 or more",
                "`bloom()` takes a radius of 0 or more",
                "`grain` is an amount from 0 to 1",
            ],
            "{got:?}"
        );
    }

    /// (graph-follow) `range` on a slider, a meter, an arc and a graph,
    /// and a graph's `style`, `line_width`, `gap` and lists of values,
    /// colours and fills, check clean.
    #[test]
    fn ranges_and_graph_props_check() {
        let src = r#"
state level = 40.0
bar Top { edge: top; height: 30
  slider { value: <-> level; range: 0, 100 }
  meter level { range: 0, auto }
  meter cpu.usage { range: auto }
  arc { value: level; range: 0, 100 }
  graph cpu.usage { range: 0, 1; style: bars; line_width: 3; gap: 0 }
  graph [cpu.usage, cpu.usage * 2] { color: #89b4fa, #f38ba8; fill: linear(180deg, #89b4fa, #89b4fa00), #f38ba855; style: steps }
  graph cpu.cores { range: auto, 100; color: #a6e3a1 }
}
"#;
        assert_eq!(codes(src), Vec::<(String, String)>::new());
    }

    /// (graph-follow) A `range` of one end, or of literal ends that do
    /// not rise, is an error; a slider's ends are numbers (it writes
    /// values between them), and a graph's `style` is one of its three.
    #[test]
    fn bad_ranges_and_graph_styles_are_errors() {
        let src = r#"
state level = 40.0
bar Top { edge: top; height: 30
  graph cpu.usage { range: 5 }
  graph cpu.usage { range: 1, 0 }
  meter level { range: 3, 3 }
  slider { value: <-> level; range: 0, auto }
  graph cpu.usage { style: wave }
}
"#;
        let got = codes(src);
        let mut ends: Vec<&str> = got
            .iter()
            .filter(|(c, _)| c == "check::range_ends")
            .map(|(_, m)| m.as_str())
            .collect();
        ends.sort_unstable();
        assert_eq!(
            ends,
            [
                "`range` runs from its lower end to its upper one",
                "`range` runs from its lower end to its upper one",
                "`range` takes its two ends, or `auto`",
                "a slider's `range` is two numbers",
            ],
            "{got:?}"
        );
        assert_eq!(got.len(), 5, "and the style's type error: {got:?}");
    }

    /// (graph-follow review) A slider's `auto`, whole or for one end, is
    /// said to be a slider's limit, not an unknown name with a guess at
    /// another; and `range`'s type reads its keyword as `auto`.
    #[test]
    fn a_sliders_auto_range_says_why() {
        let src = r#"
state level = 40.0
bar Top { edge: top; height: 30
  slider { value: <-> level; range: auto }
  slider { value: <-> level; range: 0, auto }
  meter level { range: "a", 1 }
}
"#;
        let got = codes(src);
        assert_eq!(
            got.iter()
                .filter(
                    |(c, m)| c == "check::range_ends" && m == "a slider's `range` is two numbers"
                )
                .count(),
            2,
            "{got:?}"
        );
        assert!(
            !got.iter().any(|(c, _)| c == "check::unknown_name"),
            "{got:?}"
        );
        assert!(
            got.iter()
                .any(|(_, m)| m.contains("`float | auto`") && !m.contains("Keyword")),
            "{got:?}"
        );
    }
}
