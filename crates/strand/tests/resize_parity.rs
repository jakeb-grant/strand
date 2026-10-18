//! (popup-shrink) Render lays a held content-sized surface out where its
//! resized box will be, and glides a grown one from where it was, by
//! `strand_render::resize_share` and `edge_shift`. Those mirror where the
//! surface manager puts the box (strand-surface's `placement`), which
//! render cannot depend on: held here to it for every kind of surface,
//! anchor, `attach:` and parent, growing and shrinking, at odd sizes.

use strand_render::{edge_shift, resize_share};
use strand_scene::{LogicalRect, NodeKind, Prop, PropValue, SurfaceSpec};
use strand_surface::placement::{layer_config, popup_config, popup_request};

fn spec(kind: NodeKind, props: &[(Prop, &str)]) -> SurfaceSpec {
    let mut s = SurfaceSpec::resolve(kind, |p| {
        props
            .iter()
            .find(|(q, _)| *q == p)
            .map(|(_, v)| PropValue::Keyword((*v).into()))
    });
    s.width = Some(100.0);
    s.height = Some(40.0);
    s
}

const ANCHORS: [&str; 9] = [
    "center",
    "top",
    "bottom",
    "left",
    "right",
    "top_left",
    "top_right",
    "bottom_left",
    "bottom_right",
];
const EDGES: [&str; 4] = ["top", "bottom", "left", "right"];

/// The share of a resize a layer surface keeps on one axis, as the
/// compositor places it from its anchors: an edge it is anchored to
/// alone stays; anchored to neither, it is centred.
fn layer_share(start: bool, end: bool) -> Option<f32> {
    match (start, end) {
        (true, false) => Some(0.0),
        (false, true) => Some(1.0),
        (false, false) => Some(0.5),
        // Stretched between both: it does not resize on its own.
        (true, true) => None,
    }
}

#[test]
fn a_layer_surface_keeps_the_edges_it_is_anchored_to() {
    let mut checked = 0;
    for kind in [NodeKind::Panel, NodeKind::Osd] {
        let mut props: Vec<Vec<(Prop, &str)>> =
            ANCHORS.iter().map(|a| vec![(Prop::Anchor, *a)]).collect();
        if kind == NodeKind::Panel {
            for a in ANCHORS {
                for e in EDGES {
                    props.push(vec![(Prop::Anchor, a), (Prop::Attach, e)]);
                }
            }
        }
        for p in props {
            let s = spec(kind, &p);
            let c = layer_config(&s).unwrap_or_else(|e| panic!("{p:?}: {e}"));
            let (fx, fy) = resize_share(&s, None);
            let a = c.anchors;
            if let Some(want) = layer_share(a.left, a.right) {
                assert_eq!(fx, want, "{kind:?} {p:?} across ({a:?})");
                checked += 1;
            }
            if let Some(want) = layer_share(a.top, a.bottom) {
                assert_eq!(fy, want, "{kind:?} {p:?} down ({a:?})");
                checked += 1;
            }
        }
    }
    assert!(checked > 60, "{checked}");
}

/// Where the popup's box lies (its window geometry, as the positioner
/// asks for it) at `w` × `h`.
fn popup_box(s: &SurfaceSpec, parent: &SurfaceSpec, w: u32, h: u32) -> (i32, i32) {
    let mut s = s.clone();
    s.width = Some(w as f32);
    s.height = Some(h as f32);
    let c = popup_config(&s, parent).unwrap_or_else(|e| panic!("{e}"));
    let (x, y, _, _) = popup_request(&c, c.anchor_rect, (0, 0));
    (x, y)
}

#[test]
fn a_popup_keeps_the_side_facing_its_anchor_in_whole_pixels() {
    let mut parents = vec![spec(NodeKind::Panel, &[]), spec(NodeKind::Popup, &[])];
    for e in EDGES {
        let mut bar = spec(NodeKind::Bar, &[(Prop::Edge, e)]);
        bar.width = Some(30.0);
        bar.height = Some(30.0);
        parents.push(bar);
    }
    let mut popups: Vec<(SurfaceSpec, String)> = Vec::new();
    for a in ANCHORS {
        popups.push((
            spec(NodeKind::Popup, &[(Prop::Anchor, a)]),
            format!("anchor {a}"),
        ));
    }
    for e in EDGES {
        popups.push((
            spec(NodeKind::Popup, &[(Prop::Attach, e)]),
            format!("attach {e}"),
        ));
    }
    let mut tip = spec(NodeKind::Popup, &[]);
    tip.tooltip = true;
    popups.push((tip, "tooltip".into()));
    let mut checked = 0;
    for parent in &parents {
        for (p, what) in &popups {
            let mut p = p.clone();
            p.anchor_rect = Some(LogicalRect::new(101.0, 7.0, 31.0, 21.0));
            let share = resize_share(&p, Some(parent));
            // Odd and even sizes both ways: a shrink and a growth.
            for ((w1, h1), (w2, h2)) in [((221, 91), (150, 50)), ((150, 50), (221, 91))] {
                let (x1, y1) = popup_box(&p, parent, w1, h1);
                let (x2, y2) = popup_box(&p, parent, w2, h2);
                let want = (
                    edge_shift(w1 as f32, w2 as f32, share.0),
                    edge_shift(h1 as f32, h2 as f32, share.1),
                );
                assert_eq!(
                    ((x2 - x1) as f32, (y2 - y1) as f32),
                    want,
                    "{what} in a {:?} ({:?}): {w1}x{h1} -> {w2}x{h2}, share {share:?}",
                    parent.kind,
                    parent.edge
                );
                checked += 1;
            }
        }
    }
    assert_eq!(checked, parents.len() * popups.len() * 2);
}
