//! Text in the display list: what a text node asks to have shaped, which
//! delivered layout it draws and where, fonts, `marks` and markup spans.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use strand_scene::{
    Color, Font, LogicalRect, NodeId, NodeKind, Prop, PropValue, Rect, Scale, TokenScope,
};
use strand_text::{Ellipsis, TextAlign, TextLayout, TextSpan, TextStyle};

use super::widget::segment_specs;
use super::{Inherited, MAX_LOGICAL, default_font, inherit, number};
use crate::tree::{Node, SceneTree};

/// What a text node needs shaped.
#[derive(Clone, Debug, PartialEq)]
pub struct TextSpec {
    pub text: String,
    pub style: TextStyle,
    pub max_width: Option<f32>,
    pub scale: Scale,
    /// Which of the node's texts: 0 its own (`text`, a button's label,
    /// an `input`'s text), 1 + i the i-th label of a `segmented`.
    pub part: u8,
}

/// A delivered layout of a text node and the line box width it was shaped
/// for. A node shown on several surfaces can have one per scale and width:
/// alignment (`center`, `end`) happens inside the line box, so a layout
/// shaped for one width is wrong on a surface of another.
#[derive(Clone, Debug)]
pub struct Shaped {
    pub layout: Arc<TextLayout>,
    pub max_width: Option<f32>,
    /// See [`TextSpec::part`].
    pub part: u8,
}

/// The delivered layout of a node for line box `width` (`None`: shaped
/// unbounded), preferring `scale`, else the same width at another scale
/// (drawn resampled).
pub(crate) fn pick(shaped: &[Shaped], scale: Scale, width: Option<f32>) -> Option<Arc<TextLayout>> {
    pick_part(shaped, 0, scale, width)
}

/// [`pick`] for one of a node's texts (see [`TextSpec::part`]).
pub(crate) fn pick_part(
    shaped: &[Shaped],
    part: u8,
    scale: Scale,
    width: Option<f32>,
) -> Option<Arc<TextLayout>> {
    let same_w = |c: &&Shaped| c.part == part && c.max_width == width;
    shaped
        .iter()
        .find(|c| same_w(c) && c.layout.scale == scale)
        .or_else(|| shaped.iter().find(same_w))
        .map(|c| c.layout.clone())
}

/// A layout to draw and its logical offset in the node's box.
pub(crate) type PlacedText = (Arc<TextLayout>, f32, f32);

/// Where a text node's glyphs go in its box: the layout to draw and the
/// logical offset of its origin from the box's top-left. A text whose
/// unbounded layout fits its box is drawn from that layout, aligned in
/// the box by `align`; a narrower box draws the layout shaped for its
/// width (wrapped or ellipsised, aligned by the text engine), or the
/// unbounded one while that is being shaped. `align: center` also
/// centres it vertically in a taller box; otherwise it sits at the top.
pub(crate) fn place_text(
    shaped: &[Shaped],
    scale: Scale,
    rect: LogicalRect,
    align: TextAlign,
) -> (Option<f32>, Option<PlacedText>) {
    let natural = pick(shaped, scale, None);
    let fit = natural
        .as_ref()
        .is_some_and(|n| rect.w + 1.0 < n.size.w)
        .then(|| rect.w.round().max(0.0));
    let chosen = match fit {
        Some(w) => pick(shaped, scale, Some(w))
            .map(|l| (l, 0.0))
            .or_else(|| natural.clone().map(|l| (l, 0.0))),
        None => natural.clone().map(|l| {
            let slack = (rect.w - l.size.w).max(0.0);
            let dx = match align {
                TextAlign::Start => 0.0,
                TextAlign::Center => slack / 2.0,
                TextAlign::End => slack,
            };
            (l, dx)
        }),
    }
    // Nothing for this width at all yet: any layout of the node stands in.
    .or_else(|| {
        shaped
            .iter()
            .find(|c| c.part == 0)
            .map(|c| (c.layout.clone(), 0.0))
    });
    let placed = chosen.map(|(l, dx)| {
        let dy = match align {
            TextAlign::Center => ((rect.h - l.size.h) / 2.0).max(0.0),
            _ => 0.0,
        };
        (l, dx, dy)
    });
    (fit, placed)
}

/// A font safe to shape: non-finite or non-positive sizes fall back to the
/// default size.
pub(super) fn sane_font(mut f: Font) -> Font {
    if !(f.size.is_finite() && f.size > 0.0) {
        f.size = Font::default().size;
    }
    f.size = f.size.min(MAX_LOGICAL);
    f.weight = f.weight.clamp(1, 1000);
    f.family = with_generic(&f.family);
    f
}

/// Generic CSS families: a list that names one already falls back.
pub(super) const GENERIC_FAMILIES: [&str; 8] = [
    "serif",
    "sans-serif",
    "monospace",
    "cursive",
    "fantasy",
    "system-ui",
    "ui-sans-serif",
    "ui-monospace",
];

/// `family` ending in a generic family, as CSS falls back: a font that
/// is not installed (`"Inter"`) must not leave the choice of fallback per
/// character to the font library, which can pick a font that cannot draw
/// it (digits from a bitmap emoji font). A family whose name says `Mono`
/// falls back to `monospace`, any other to `sans-serif`.
pub(super) fn with_generic(family: &str) -> String {
    let has_generic = family
        .split(',')
        .map(|f| f.trim().trim_matches(['"', '\'']).to_ascii_lowercase())
        .any(|f| GENERIC_FAMILIES.contains(&f.as_str()));
    if has_generic || family.trim().is_empty() {
        return family.to_string();
    }
    let generic = if family.to_ascii_lowercase().contains("mono") {
        "monospace"
    } else {
        "sans-serif"
    };
    format!("{family}, {generic}")
}

/// `marks: h.ranges` (a list of `[start, end]` character ranges, end
/// exclusive, as fuzzy matchers report them) as text spans painted in
/// `mark_color` (default `$accent`), or bold when there is no colour.
pub(super) fn marks(
    text: &str,
    v: Option<&PropValue>,
    color: impl FnOnce() -> Option<Color>,
) -> Vec<TextSpan> {
    let Some(PropValue::List(items)) = v else {
        return Vec::new();
    };
    // Character index → byte offset.
    let bytes: Vec<usize> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain([text.len()])
        .collect();
    let at = |n: f32| bytes[(n.max(0.0) as usize).min(bytes.len() - 1)];
    let ranges: Vec<std::ops::Range<usize>> = items
        .iter()
        .filter_map(|r| match r {
            PropValue::List(pair) => match (pair.first(), pair.get(1)) {
                (Some(a), Some(b)) => Some((a.as_number()?, b.as_number()?)),
                _ => None,
            },
            _ => None,
        })
        .filter(|(a, b)| a.is_finite() && b.is_finite() && a < b)
        .take(1024)
        .map(|(a, b)| at(a)..at(b))
        .collect();
    if ranges.is_empty() {
        return Vec::new();
    }
    let color = color();
    ranges
        .into_iter()
        .map(|range| TextSpan {
            range,
            weight: color.is_none().then_some(700),
            italic: false,
            underline: false,
            color,
        })
        .collect()
}

/// The unbounded text request of a text node whose resolved props `get`
/// reads, its `align` and its span colours: markup parsed, marks as
/// spans, shaped at `scale` with `font`. Span colours are not shaped
/// with: the request carries slots ([`span_slots`]), so a mark or link
/// colour that springs (`$accent` in a theme swap) never reshapes.
pub(super) fn natural_spec<'v>(
    get: &impl Fn(Prop) -> Option<&'v PropValue>,
    scope: &TokenScope<'_>,
    font: &Font,
    scale: Scale,
) -> Option<(TextSpec, TextAlign, Vec<Color>)> {
    let Some(PropValue::Text(text)) = get(Prop::Text) else {
        return None;
    };
    let align = match get(Prop::Align) {
        Some(PropValue::Keyword(k)) if k == "center" => TextAlign::Center,
        Some(PropValue::Keyword(k)) if k == "end" => TextAlign::End,
        _ => TextAlign::Start,
    };
    let ellipsis = match get(Prop::Ellipsis) {
        Some(PropValue::Keyword(k)) => Ellipsis::from_name(k),
        Some(PropValue::Bool(true)) => Some(Ellipsis::End),
        _ => None,
    };
    let max_lines = number(get(Prop::MaxLines))
        .filter(|n| *n >= 1.0)
        .map(|n| n.min(10_000.0) as u32);
    let accent = || match scope.lookup("accent") {
        Some(PropValue::Color(c)) => Some(c),
        _ => None,
    };
    let (shown, mut spans) = match get(Prop::Markup) {
        Some(PropValue::Keyword(k)) if k == "basic" => crate::markup::parse(text, accent()),
        _ => (text.clone(), Vec::new()),
    };
    spans.extend(marks(&shown, get(Prop::Marks), || {
        match get(Prop::MarkColor) {
            Some(PropValue::Color(c)) => Some(*c),
            _ => accent(),
        }
    }));
    let colors = span_slots(&mut spans);
    Some((
        TextSpec {
            text: shown,
            style: TextStyle {
                font: font.clone(),
                line_height: None,
                align: TextAlign::Start,
                ellipsis,
                max_lines,
                spans,
            },
            max_width: None,
            scale,
            part: 0,
        },
        align,
        colors,
    ))
}

/// Replaces the colours of `spans` by slots and returns the colours: the
/// `i`th distinct colour becomes [`slot`]`(i)`, a stand-in that only
/// keeps the glyph runs apart. The painter maps a run's slot back with
/// [`slot_color`].
pub(crate) fn span_slots(spans: &mut [TextSpan]) -> Vec<Color> {
    let mut colors: Vec<Color> = Vec::new();
    for sp in spans.iter_mut() {
        if let Some(c) = sp.color {
            let i = match colors.iter().position(|k| *k == c) {
                Some(i) => i,
                None => {
                    colors.push(c);
                    colors.len() - 1
                }
            };
            sp.color = Some(slot(i));
        }
    }
    colors
}

/// The stand-in colour of span slot `i`.
pub(super) fn slot(i: usize) -> Color {
    Color {
        r: i as f32,
        g: 0.0,
        b: 0.0,
        a: 1.0,
    }
}

/// The colour a glyph run of `run` colour paints in: its slot's colour in
/// `spans`, or the node's colour.
pub(crate) fn slot_color(run: Option<Color>, spans: &[Color], node: Color) -> Color {
    run.and_then(|c| spans.get(c.r as usize).copied())
        .unwrap_or(node)
}

/// The unbounded text requests of the text nodes under the surface node
/// `root` (not in nested surfaces) that its content pass laid out
/// (`laid`), at `scale`: what laying it out by its content measures,
/// before any surface shows it. Rows a virtualised list left out are not
/// walked, so a 2,000-row list asks for its visible rows only.
pub fn natural_texts(
    tree: &SceneTree,
    root: NodeId,
    scale: Scale,
    laid: &HashMap<NodeId, LogicalRect>,
) -> Vec<(NodeId, TextSpec)> {
    let mut out = Vec::new();
    let Some(node) = tree.get(root) else {
        return out;
    };
    let mut inh = Inherited {
        color: None,
        font: None,
        weight: None,
        tokens: vec![&tree.tokens],
        ctx: 0,
        clip: Rect::default(),
        offset: (0.0, 0.0),
        inert: false,
    };
    let mut ancestors = Vec::new();
    let mut up = node.parent;
    while let Some(a) = up.and_then(|p| tree.get(p)) {
        ancestors.push(a);
        up = a.parent;
    }
    for a in ancestors.into_iter().rev() {
        inherit(a, &mut inh);
    }
    fn walk<'a>(
        tree: &'a SceneTree,
        node: &'a Node,
        inh: &Inherited<'a>,
        scale: Scale,
        laid: &HashMap<NodeId, LogicalRect>,
        out: &mut Vec<(NodeId, TextSpec)>,
    ) {
        if !laid.contains_key(&node.id) {
            return;
        }
        let mut inh = inh.clone();
        inherit(node, &mut inh);
        if matches!(node.kind, NodeKind::Text | NodeKind::Button) {
            let scope = TokenScope::new(&inh.tokens);
            let props: Vec<(Prop, Cow<'_, PropValue>)> = node
                .props
                .iter()
                .filter(|e| e.prop != Prop::Tokens)
                .filter_map(|e| scope.resolve(&e.value).map(|v| (e.prop, v)))
                .collect();
            let get = |p: Prop| props.iter().find(|(q, _)| *q == p).map(|(_, v)| v.as_ref());
            let mut font = inh.font.clone().unwrap_or_else(|| default_font(&scope));
            if let Some(w) = inh.weight {
                font.weight = w;
            }
            if let Some((spec, _, _)) = natural_spec(&get, &scope, &font, scale) {
                out.push((node.id, spec));
            }
        }
        if node.kind == NodeKind::Segmented {
            let scope = TokenScope::new(&inh.tokens);
            let options = node.get(Prop::Options).and_then(|v| scope.resolve(v));
            // `inherit` took the node's own `font` and `weight`.
            let mut font = inh.font.clone().unwrap_or_else(|| default_font(&scope));
            if let Some(w) = inh.weight {
                font.weight = w;
            }
            for spec in segment_specs(options.as_deref(), &font, scale) {
                out.push((node.id, spec));
            }
        }
        for c in &node.children {
            if let Some(child) = tree.get(*c).filter(|n| !crate::layout::out_of_flow(n.kind)) {
                walk(tree, child, &inh, scale, laid, out);
            }
        }
    }
    walk(tree, node, &inh, scale, laid, &mut out);
    out
}
