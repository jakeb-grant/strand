//! Surface props only some surfaces take (M4): `attach` and `scrim` on a
//! `popup` or a `panel` (design.md's catalogue: "`attach: top` on a popup
//! or panel", "`scrim: $shadow.alpha(0.3)` on a popup or panel"). The
//! schema's `surface` group lists them for every surface, so a `bar`,
//! `osd` or `lock` that sets one is caught here: a bar already is the
//! edge a panel attaches to, and a scrim under a bar or an OSD would dim
//! the screen for as long as it shows.
//!
//! (M6) A `popup`'s `trigger: hover` (spec.md 6.3): the pointer opens
//! it, and its `open:` (m6-owner-fixes) only holds it open, so it is
//! never two-way; the trigger is written as is, on the popup itself,
//! never in a `when` or as an expression.

use super::Checker;
use crate::hir::{self, Node};

/// The props only a `popup` or `panel` takes.
const POPUP_AND_PANEL: &[&str] = &["attach", "scrim"];

impl Checker<'_> {
    /// Reports `attach` and `scrim` on a surface of `kind` other than a
    /// `popup` or `panel`, among its props and its `when` blocks'.
    pub(super) fn surface_props(&mut self, kind: &str, props: &[hir::Prop], children: &[Node]) {
        if kind == "popup" {
            self.popup_trigger(props, children);
        }
        if !matches!(kind, "bar" | "osd" | "lock") {
            return;
        }
        let whens = children.iter().filter_map(|n| match n {
            Node::When(w) => Some(&w.props),
            _ => None,
        });
        let bad: Vec<(String, crate::syntax::Span)> = props
            .iter()
            .chain(whens.flatten())
            .filter(|p| POPUP_AND_PANEL.contains(&p.name.as_str()))
            .map(|p| (p.name.clone(), p.span))
            .collect();
        for (name, span) in bad {
            self.error(
                "check::surface_prop",
                format!("`{name}` is only for a `popup` or `panel`"),
                span,
                format!("on a `{kind}`"),
            )
            .help = Some(format!(
                "put `{name}` on the `panel` or `popup` that grows out of this `{kind}`"
            ));
        }
    }
}

impl Checker<'_> {
    /// (M6) A popup's `trigger`: a variant as written, on the popup;
    /// with `hover`, no two-way `open` beside it (in its props or a
    /// `when`).
    fn popup_trigger(&mut self, props: &[hir::Prop], children: &[Node]) {
        let whens: Vec<&hir::Prop> = children
            .iter()
            .filter_map(|n| match n {
                Node::When(w) => Some(&w.props),
                _ => None,
            })
            .flatten()
            .collect();
        for p in whens.iter().filter(|p| p.name == "trigger") {
            self.error(
                "check::popup_trigger",
                "a popup's `trigger` cannot change",
                p.span,
                "in a `when`",
            )
            .help = Some("write `trigger:` on the `popup` itself".into());
        }
        let mut hover = None;
        for p in props.iter().filter(|p| p.name == "trigger") {
            match p.value.kind {
                hir::ExprKind::Variant(e, v) => {
                    let en = self.types.enum_(e);
                    if en.variants.get(v as usize).is_some_and(|n| n == "hover") {
                        hover = Some(p.span);
                    }
                }
                hir::ExprKind::Error => {}
                _ => {
                    self.error(
                        "check::popup_trigger",
                        "a popup's `trigger` is `open` or `hover`, as written",
                        p.value.span,
                        "not a fixed trigger",
                    )
                    .help = Some(
                        "write `trigger: hover` or `trigger: open`; use `open:` for a popup a value opens"
                            .into(),
                    );
                }
            }
        }
        let file = self.file();
        let Some(trigger) = hover else {
            // `delay` and `grace` time the pointer: only with the trigger.
            for p in props
                .iter()
                .chain(whens.iter().copied())
                .filter(|p| matches!(p.name.as_str(), "delay" | "grace"))
            {
                self.error(
                    "check::hover_popup_prop",
                    format!("`{}` is only for a `trigger: hover` popup", p.name),
                    p.span,
                    "this popup's `open:` opens it",
                )
                .help = Some(format!(
                    "add `trigger: hover` (and remove `open:`), or remove `{}`",
                    p.name
                ));
            }
            return;
        };
        // (m6-owner-fixes) Its `open:` holds it open, one way: the
        // pointer, not the program, closes it.
        for p in props
            .iter()
            .chain(whens.iter().copied())
            .filter(|p| p.name == "open" && p.two_way)
        {
            self.error(
                "check::hover_popup_open",
                "a `trigger: hover` popup's `open:` cannot be two-way",
                p.span,
                "the pointer closes this popup",
            )
            .add_secondary(file, trigger, "`trigger: hover` here")
            .help = Some(
                "write `open: x` to hold it open while `x` is true (read `id.open` for whether it shows), or remove `trigger: hover` to bind `open: <-> x`"
                    .into(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::SourceMap;

    fn codes(src: &str) -> Vec<(String, String)> {
        let mut map = SourceMap::new();
        map.add("surfaces.strand", src.to_string());
        let c = crate::compile_with(&map, crate::schema::Schema::builtin());
        c.diagnostics
            .iter()
            .map(|d| (d.code.to_string(), d.message.clone()))
            .collect()
    }

    /// A `panel` and a `popup` take `attach` and `scrim` (the raw colours
    /// only warn).
    #[test]
    fn panels_and_popups_take_attach_and_scrim() {
        let src = r#"
bar Top { edge: top; height: 30
  popup { open: true; attach: top; scrim: #00000040; width: 100; height: 50 }
}
panel Dash { anchor: top_right; attach: top; scrim: #00000040; width: 300; height: 200 }
"#;
        let got = codes(src);
        assert!(got.iter().all(|(c, _)| c == "check::raw_color"), "{got:?}");
    }

    /// (M6) `trigger: hover` takes no two-way `open:` (m6-owner-fixes:
    /// a one-way one, in the props or a `when`, holds it open), and
    /// `trigger` is a fixed variant on the popup itself; `trigger: open`
    /// with `open:` and `trigger: hover` alone are fine.
    #[test]
    fn a_hover_popup_takes_no_two_way_open() {
        let src = r#"
state on = false
bar Top { edge: top; height: 30
  box { size: 20; popup { trigger: hover; delay: 150ms; grace: 400ms; box { size: 40 } } }
  box { size: 20; popup { open: on; grace: 400ms; box { size: 40 } } }
  box { size: 20; popup { trigger: open; open: on; box { size: 40 } } }
  box { size: 20; popup { trigger: hover; open: on; box { size: 40 } } }
  box { size: 20; popup { trigger: hover; when on { open: true }; box { size: 40 } } }
  box { size: 20; popup { trigger: hover; open: <-> on; box { size: 40 } } }
  box { size: 20; popup { open: on; when on { trigger: hover }; box { size: 40 } } }
  box { size: 20; popup { trigger: on ? hover : open; open: on; box { size: 40 } } }
}
"#;
        let got = codes(src);
        let ours: Vec<(&str, &str)> = got
            .iter()
            .filter(|(c, _)| c.starts_with("check::"))
            .map(|(c, m)| (c.as_str(), m.as_str()))
            .collect();
        assert_eq!(
            ours,
            [
                (
                    "check::hover_popup_prop",
                    "`grace` is only for a `trigger: hover` popup"
                ),
                (
                    "check::hover_popup_open",
                    "a `trigger: hover` popup's `open:` cannot be two-way"
                ),
                ("check::popup_trigger", "a popup's `trigger` cannot change"),
                (
                    "check::popup_trigger",
                    "a popup's `trigger` is `open` or `hover`, as written"
                ),
            ],
            "{got:?}"
        );
    }

    /// A `bar`, `osd` or `lock` that sets either is an error, in its
    /// props or a `when` block.
    #[test]
    fn other_surfaces_do_not() {
        let src = r#"
state wide = false
bar Top { edge: top; height: 30; attach: top
  when wide { scrim: #00000040 }
}
osd Level { width: 100; height: 40; scrim: #00000040 }
lock Gate { attach: left }
"#;
        let got = codes(src);
        let ours: Vec<&String> = got
            .iter()
            .filter(|(c, _)| c == "check::surface_prop")
            .map(|(_, m)| m)
            .collect();
        assert_eq!(
            ours,
            [
                "`attach` is only for a `popup` or `panel`",
                "`scrim` is only for a `popup` or `panel`",
                "`scrim` is only for a `popup` or `panel`",
                "`attach` is only for a `popup` or `panel`",
            ],
            "{got:?}"
        );
    }
}
