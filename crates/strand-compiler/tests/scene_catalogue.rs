//! The schema and the scene protocol name one catalogue.
//!
//! Every element the schema declares is a `strand_scene::protocol::NodeKind`
//! (components are expanded before the scene, so only builtin kinds reach
//! it), and every prop an element accepts is a scene `Prop`, except the
//! props the compiler consumes itself (`COMPILER_ONLY`), which never reach
//! the render thread. A prop added to one side and not the other fails
//! here, so lowering (M1 VM, M2 layout) never meets a prop it cannot emit.

use std::collections::BTreeSet;

use strand_compiler::schema::Schema;
use strand_scene::protocol::{NodeKind, Prop};

/// Props the compiler resolves before the scene, with no render-side
/// meaning.
const COMPILER_ONLY: &[&str] = &[
    // `id: results`: a name in scope, read as `results.hover`.
    "id",
];

/// Props design.md names that `strand_scene::protocol::Prop` does not have
/// yet; each is recorded in docs/architecture.md as a scene addition for
/// its owner. Remove an entry when the scene gains it (the test then
/// insists it stays).
const SCENE_PENDING: &[&str] = &[];

#[test]
fn every_element_is_a_scene_kind() {
    let schema = Schema::builtin();
    let missing: Vec<&str> = schema
        .elements
        .keys()
        .map(String::as_str)
        .filter(|k| NodeKind::from_name(k).is_none())
        .collect();
    assert!(missing.is_empty(), "elements with no NodeKind: {missing:?}");
    let kinds: BTreeSet<&str> = NodeKind::ALL.iter().map(|k| k.name()).collect();
    let unschemed: Vec<&&str> = kinds
        .iter()
        .filter(|k| schema.element(k).is_none())
        .collect();
    assert!(
        unschemed.is_empty(),
        "NodeKinds with no schema element: {unschemed:?}"
    );
}

#[test]
fn every_prop_is_a_scene_prop() {
    let schema = Schema::builtin();
    let mut missing = BTreeSet::new();
    let mut pending_seen = BTreeSet::new();
    for (kind, el) in &schema.elements {
        let subs = el.props.iter().flat_map(|p| p.sub.iter());
        for p in el.props.iter().chain(subs) {
            let name = p.name.as_str();
            if Prop::from_name(name).is_some() {
                assert!(
                    !SCENE_PENDING.contains(&name),
                    "`{name}` is a scene Prop now: remove it from SCENE_PENDING"
                );
            } else if SCENE_PENDING.contains(&name) {
                pending_seen.insert(name);
            } else if !COMPILER_ONLY.contains(&name) {
                missing.insert(format!("{kind}.{name}"));
            }
        }
    }
    assert!(missing.is_empty(), "props with no scene Prop: {missing:?}");
    assert_eq!(
        pending_seen.len(),
        SCENE_PENDING.len(),
        "SCENE_PENDING lists props the schema no longer has"
    );
}

/// The prop a positional argument fills (`element meter(float -> value)`)
/// is a scene `Prop`, so lowering can always emit it.
#[test]
fn every_positional_fills_a_scene_prop() {
    let schema = Schema::builtin();
    for (kind, el) in &schema.elements {
        if let Some(p) = &el.arg_prop {
            assert!(
                Prop::from_name(p).is_some(),
                "`{kind}`'s positional fills `{p}`, which is no scene Prop"
            );
        }
    }
}
