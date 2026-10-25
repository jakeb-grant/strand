//! (M4) `canvas { draw: (c) => … }`: the instance runs the lambda with a
//! `Canvas` of the node's laid-out size, records its calls as
//! `Prop::Draw`, and runs it again when what it read changes (a state, the
//! size); a paint naming tokens is kept for render to resolve.

use std::path::PathBuf;
use std::rc::Rc;

use strand_compiler::instantiate::{Instance, SceneMirror, Storage};
use strand_compiler::reconcile::loader::Loader;
use strand_compiler::schema::Schema;
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_core::Runtime;
use strand_scene::canvas::DrawOp;
use strand_scene::{NodeKind, Paint, Prop, PropValue};

const PANEL: &str = r#"export state level = 10
panel Chart { anchor: center; width: 200; height: 100; open: true
  canvas { width: 120; height: 40; draw: (c) => {
    c.fill($accent)
    c.rect(0, 0, level, c.height)
    c.fill(#ff0000)
    c.line(0, 0, c.width, c.height)
    c.stroke(#00ff00, 2)
    c.circle(5, 5, 2)
    c.text("hi", 1, 2)
  } }
}
"#;

fn dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("strand-canvas-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn ops(scene: &SceneMirror) -> Vec<DrawOp> {
    let node = scene.of_kind(NodeKind::Canvas)[0];
    match scene.prop(node, Prop::Draw) {
        Some(PropValue::DrawList(ops)) => ops.to_vec(),
        other => panic!("no draw list: {other:?}"),
    }
}

#[test]
fn draw_records_the_canvas_calls_and_runs_again_on_what_it_read() {
    let d = dir();
    std::fs::write(d.join("chart.strand"), PANEL).unwrap();
    let mut l = Loader::new(&d, Schema::builtin().clone(), None);
    let out = l.boot();
    assert_eq!(out.errors(), 0, "{:?}", out.diagnostics);
    let build = out.build.expect("the config compiles");
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &build.program.types));
    let inst = Instance::from_build(&rt, &build, host, Storage::none());
    let mut scene = SceneMirror::new();
    let u = inst.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    scene.apply(&u.diff).unwrap();
    let node = scene.of_kind(NodeKind::Canvas)[0];

    // Before layout the canvas is 0×0.
    let first = ops(&scene);
    assert_eq!(first.len(), 7, "{first:?}");
    assert!(matches!(first[0], DrawOp::FillThemed(PropValue::Token(_))));
    assert_eq!(
        first[1],
        DrawOp::Rect {
            x: 0.0,
            y: 0.0,
            w: 10.0,
            h: 0.0
        }
    );
    assert!(matches!(first[2], DrawOp::Fill(Paint::Solid(_))));
    assert!(matches!(first[4], DrawOp::Stroke { width: 2.0, .. }));
    assert_eq!(
        first[6],
        DrawOp::Text {
            text: "hi".into(),
            x: 1.0,
            y: 2.0
        }
    );

    // Laid out: run again with the size.
    inst.set_size(node, 120.0, 40.0);
    scene.apply(&inst.flush().diff).unwrap();
    let sized = ops(&scene);
    assert_eq!(
        sized[3],
        DrawOp::Line {
            x1: 0.0,
            y1: 0.0,
            x2: 120.0,
            y2: 40.0
        }
    );
    // A state it reads: run again.
    inst.set("chart.level", Value::float(55.0)).unwrap();
    scene.apply(&inst.flush().diff).unwrap();
    assert_eq!(
        ops(&scene)[1],
        DrawOp::Rect {
            x: 0.0,
            y: 0.0,
            w: 55.0,
            h: 40.0
        }
    );
    let _ = std::fs::remove_dir_all(d);
}

/// (flex) `move`, `to`, `arc` (angles by name, in degrees, from any
/// angle expression), `close` and an even-odd `fill` are recorded as the
/// path ops render draws; `evenodd` unset is the non-zero fill.
#[test]
fn paths_arcs_and_even_odd_fills_are_recorded() {
    let d = std::env::temp_dir().join(format!("strand-canvas-paths-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("pie.strand"),
        r#"export state share = 0.25
panel Pie { anchor: center; width: 100; height: 100; open: true
  canvas { width: 40; height: 40; draw: (c) => {
    c.move(20, 20)
    c.arc(20, 20, 16, from: -90deg, to: -90deg + share * 360deg)
    c.close()
    c.fill($accent)
    c.circle(20, 20, 10)
    c.circle(20, 20, 6)
    c.fill(#00ff00, evenodd: true)
    c.to(0, 40)
    c.to(atan2(1, 1) / 1deg, 40)
    c.stroke(#ffffff, 1)
  } }
}
"#,
    )
    .unwrap();
    let mut l = Loader::new(&d, Schema::builtin().clone(), None);
    let out = l.boot();
    assert_eq!(out.errors(), 0, "{:?}", out.diagnostics);
    let build = out.build.expect("the config compiles");
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &build.program.types));
    let inst = Instance::from_build(&rt, &build, host, Storage::none());
    let mut scene = SceneMirror::new();
    let u = inst.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    scene.apply(&u.diff).unwrap();
    let got = ops(&scene);
    assert_eq!(got[0], DrawOp::MoveTo { x: 20.0, y: 20.0 });
    assert_eq!(
        got[1],
        DrawOp::Arc {
            x: 20.0,
            y: 20.0,
            r: 16.0,
            from: -90.0,
            to: 0.0
        }
    );
    assert_eq!(got[2], DrawOp::Close);
    assert!(matches!(got[3], DrawOp::FillThemed(_)), "{:?}", got[3]);
    assert!(
        matches!(got[6], DrawOp::FillEvenOdd(Paint::Solid(_))),
        "{:?}",
        got[6]
    );
    assert_eq!(got[7], DrawOp::LineTo { x: 0.0, y: 40.0 });
    assert_eq!(got[8], DrawOp::LineTo { x: 45.0, y: 40.0 });
    assert!(matches!(got[9], DrawOp::Stroke { .. }));
    // The slice follows the state.
    inst.set("pie.share", Value::float(0.5)).unwrap();
    scene.apply(&inst.flush().diff).unwrap();
    assert!(matches!(ops(&scene)[1], DrawOp::Arc { to: 90.0, .. }));
    let _ = std::fs::remove_dir_all(d);
}
