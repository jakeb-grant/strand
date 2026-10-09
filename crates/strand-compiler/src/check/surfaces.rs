//! Surface props only some surfaces take (M4): `attach` and `scrim` on a
//! `popup` or a `panel` (design.md's catalogue: "`attach: top` on a popup
//! or panel", "`scrim: $shadow.alpha(0.3)` on a popup or panel"). The
//! schema's `surface` group lists them for every surface, so a `bar`,
//! `osd` or `lock` that sets one is caught here: a bar already is the
//! edge a panel attaches to, and a scrim under a bar or an OSD would dim
//! the screen for as long as it shows.

use super::Checker;
use crate::hir::{self, Node};

/// The props only a `popup` or `panel` takes.
const POPUP_AND_PANEL: &[&str] = &["attach", "scrim"];

impl Checker<'_> {
    /// Reports `attach` and `scrim` on a surface of `kind` other than a
    /// `popup` or `panel`, among its props and its `when` blocks'.
    pub(super) fn surface_props(&mut self, kind: &str, props: &[hir::Prop], children: &[Node]) {
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
