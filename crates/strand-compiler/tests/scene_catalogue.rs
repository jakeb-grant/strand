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
use strand_compiler::ty::Ty;
use strand_scene::effect::{BlendMode, Bundled};
use strand_scene::input::DropKind;
use strand_scene::protocol::{NodeKind, Prop};
use strand_scene::{DrawOp, Easing};

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

/// Scene props the compiler sets and source never writes: the schema
/// must not declare them, or a config could write them.
const COMPILER_SET: &[Prop] = &[
    Prop::Name,
    Prop::Watch,
    Prop::TwoWay,
    // M4.
    Prop::Uniforms,
    Prop::Shader,
    Prop::Accepts,
    Prop::RowCount,
    Prop::RowFirst,
    Prop::DropRows,
];

/// Scene kinds the compiler makes from something other than an element:
/// `svg "a.svg" { #needle { … } }` selector blocks.
const SCENE_ONLY_KINDS: &[NodeKind] = &[NodeKind::SvgPart];

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
        .filter(|k| !SCENE_ONLY_KINDS.iter().any(|s| s.name() == **k))
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

#[test]
fn compiler_set_props_are_not_in_the_schema() {
    let schema = Schema::builtin();
    let groups = schema.groups.iter().chain(&schema.elements);
    for (kind, el) in groups {
        let subs = el.props.iter().flat_map(|p| p.sub.iter());
        for p in el.props.iter().chain(subs) {
            assert!(
                !COMPILER_SET.iter().any(|c| c.name() == p.name),
                "`{kind}.{}` is set by the compiler and must not be written in source",
                p.name
            );
        }
    }
    for k in SCENE_ONLY_KINDS {
        assert!(schema.element(k.name()).is_none(), "{k} is not an element");
    }
}

/// The scene's enums name exactly the schema's variants, so a value the
/// checker accepts always has a scene form.
#[test]
fn scene_enums_follow_the_schema() {
    let schema = Schema::builtin();
    let variants = |name: &str| -> Vec<String> {
        let id = schema
            .types
            .find_enum(name)
            .unwrap_or_else(|| panic!("enum {name}"));
        schema.types.enum_(id).variants.clone()
    };
    let drop: Vec<&str> = DropKind::ALL.iter().map(|k| k.name()).collect();
    assert_eq!(variants("DropKind"), drop);
    // `blend: normal` is no effect; the rest are blend modes.
    let mut blend: Vec<&str> = vec!["normal"];
    blend.extend(BlendMode::ALL.iter().map(|b| b.name()));
    assert_eq!(variants("Blend"), blend);
    // Every curve the checker accepts runs as itself.
    for c in variants("Curve") {
        assert!(Easing::named(&c).is_some(), "curve {c}");
    }
}

/// The bundled GPU effects without a node or prop of their own are
/// functions of `filter:`/`backdrop:` (decisions.md, m4-owner).
#[test]
fn bundled_effects_have_their_spelling() {
    let schema = Schema::builtin();
    let filter = Ty::opaque("Filter");
    for (name, arity) in [
        ("bloom", 1),
        ("crt", 0),
        ("chromatic", 1),
        ("wobble", 1),
        ("glass", 0),
    ] {
        assert!(
            Bundled::from_name(name).is_some(),
            "{name} is a scene Bundled"
        );
        let sigs = schema
            .functions
            .get(name)
            .unwrap_or_else(|| panic!("fn {name}"));
        assert!(
            sigs.iter()
                .any(|s| s.ret == filter && s.params.len() == arity),
            "{name}: {sigs:?}"
        );
    }
}

/// A canvas op per `Canvas` method, so the VM can record every call.
#[test]
fn canvas_ops_cover_the_canvas_record() {
    let schema = Schema::builtin();
    let id = schema.types.find_record("Canvas").expect("record Canvas");
    let mut methods: Vec<&str> = schema
        .types
        .record(id)
        .methods
        .iter()
        .map(|m| m.name.as_str())
        .collect();
    methods.sort_unstable();
    let mut ops = DrawOp::METHODS.to_vec();
    ops.sort_unstable();
    assert_eq!(methods, ops);
}

/// A scrim is a single-pixel buffer, so the schema types it `color`.
#[test]
fn a_scrim_is_a_colour() {
    let schema = Schema::builtin();
    let surface = schema.groups.get("surface").expect("group surface");
    let scrim = surface
        .props
        .iter()
        .find(|p| p.name == "scrim")
        .expect("scrim");
    assert_eq!(scrim.ty, Ty::COLOR);
}
