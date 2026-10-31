//! (flex) `Node.pointer`: where the pointer is in a node's laid-out box,
//! read by logic as a value (decisions.md `## flex`). Each fixture under
//! `tests/fixtures/flex/` is a whole shell driven through the host loop
//! (`tests/shell`): a rotary knob turned by its angle, a sidebar resized
//! by its grip, a dock that swells smoothly with the pointer, a region
//! selector, a colour picker and a floating widget dragged by its grip,
//! each checked by its values and drawn at a chosen pointer position
//! against `tests/refs/pointer_*.png`.

mod shell;

use shell::{Shell, assert_matches_ref, px};
use strand_scene::Painter;

fn fixture(name: &str) -> Shell {
    let path = format!(
        "{}/tests/fixtures/flex/{name}.strand",
        env!("CARGO_MANIFEST_DIR")
    );
    let src = std::fs::read_to_string(&path).unwrap();
    Shell::new(&src)
}

fn near(got: f64, want: f64, tol: f64, what: &str) {
    assert!(
        (got - want).abs() <= tol,
        "{what}: {got}, want {want} ± {tol}"
    );
}

/// Blue (`#5b8def`) where `p` is drawn, as premultiplied BGRA.
fn blue(p: [u8; 4]) -> bool {
    p[3] == 255 && p[0] > 200 && p[2] < 120
}

/// The knob's value follows the angle of the pointer around the dial's
/// centre while the press that began on it is held, and stops with it.
#[test]
fn knob_turns_by_the_pointer_angle() {
    let mut sh = fixture("knob");
    near(sh.num("vol"), 0.25, 1e-9, "start");
    // The dial is 100 px at 10, 10: its centre is 60, 60. Left of the
    // centre a little below it, then the top, then right of it.
    sh.move_to(20.0, 70.0);
    sh.button(true);
    // atan2(10, -40) = 166°: (166 - 135) / 270.
    near(sh.num("vol"), 0.1148, 0.002, "press");
    for i in 1..=10 {
        let f = i as f32 / 10.0;
        sh.move_to(20.0 + 40.0 * f, 70.0 - 60.0 * f);
    }
    near(sh.num("vol"), 0.5, 0.002, "top");
    for i in 1..=10 {
        let f = i as f32 / 10.0;
        sh.move_to(60.0 + 50.0 * f, 10.0 + 60.0 * f);
    }
    // atan2(10, 50) = 11.3°: (371.3 - 135) / 270.
    near(sh.num("vol"), 0.8752, 0.002, "right");
    sh.button(false);
    // Released, the pointer turns nothing.
    sh.move_to(60.0, 10.0);
    near(sh.num("vol"), 0.8752, 0.002, "released");
    assert!(!sh.throttled(), "a drag at 60 Hz was throttled");
    let shot = sh.shot("Knob");
    assert_matches_ref("pointer_knob", shot.0, &shot.1, 2);
}

/// Dragging the grip resizes the box before it, and the grip moves with
/// it without the move feeding back: the pointer stays in the frame the
/// press began in, so the width follows the pointer exactly.
#[test]
fn grip_resizes_without_feedback() {
    let mut sh = fixture("resize");
    assert_eq!(sh.num("w"), 160.0);
    sh.drag((164.0, 60.0), (264.0, 60.0), 25);
    assert_eq!(sh.num("w"), 260.0);
    sh.drag((264.0, 60.0), (214.0, 60.0), 10);
    assert_eq!(sh.num("w"), 210.0);
    // Clamped at its least.
    sh.drag((214.0, 60.0), (0.0, 60.0), 10);
    assert_eq!(sh.num("w"), 40.0);
    sh.drag((44.0, 60.0), (124.0, 60.0), 10);
    assert_eq!(sh.num("w"), 120.0);
    assert!(!sh.throttled(), "a drag at 60 Hz was throttled");
    let shot = sh.shot("Side");
    assert!(blue(px(&shot, 124, 60)), "the grip moved with the width");
    assert_matches_ref("pointer_resize", shot.0, &shot.1, 2);
}

/// The dock's icons swell with the pointer's distance from their centres
/// as one smooth function of it, and settle back when it leaves.
#[test]
fn dock_magnifies_smoothly() {
    let mut sh = fixture("dock");
    let dock = sh.surface("Dock").origin;
    // Over icon 2's centre (92 + 2·56 = 204 in the strip).
    sh.move_to(dock.0 + 204.0, dock.1 + 84.0);
    let shot = sh.shot("Dock");
    // Icon 2 at 1.6: 48 px tall becomes 76.8, about its centre at 84.
    assert!(blue(px(&shot, 204, 50)), "icon 2 not magnified");
    // Icon 1 is 56 px away: 1.32, 63.4 px tall, its top at 52.3.
    assert!(
        !blue(px(&shot, 148, 50)),
        "icon 1 swelled as much as icon 2"
    );
    assert!(blue(px(&shot, 148, 55)), "icon 1 not magnified");
    assert_matches_ref("pointer_dock", shot.0, &shot.1, 2);
    // Half way between icons 2 and 3: both alike.
    sh.move_to(dock.0 + 232.0, dock.1 + 84.0);
    let shot = sh.shot("Dock");
    let top = |x: u32| (0..120).find(|y| blue(px(&shot, x, *y)));
    assert_eq!(top(204), top(260), "icons 2 and 3 differ half way");
    // Gone: every icon at its own size again.
    sh.move_to(dock.0 + 204.0, dock.1 - 100.0);
    let shot = sh.shot("Dock");
    assert!(!blue(px(&shot, 204, 55)), "icon 2 kept its scale");
    assert!(blue(px(&shot, 204, 62)));
}

/// Press, drag and release draw a rectangle between where the press
/// went down and where the pointer is.
#[test]
fn region_selector_draws_the_dragged_rectangle() {
    let mut sh = fixture("region");
    sh.move_to(300.0, 200.0);
    sh.button(true);
    for i in 1..=12 {
        let f = i as f32 / 12.0;
        sh.move_to(300.0 - 200.0 * f, 200.0 - 120.0 * f);
    }
    // Still held: the rectangle runs back up and left.
    assert_eq!((sh.num("bx"), sh.num("by")), (100.0, 80.0));
    assert_eq!(sh.num("taken"), 0.0);
    sh.button(false);
    assert_eq!(sh.num("taken"), 1.0);
    let shot = sh.shot("Region");
    assert_matches_ref("pointer_region", shot.0, &shot.1, 2);
}

/// The hue ring takes its hue from the angle around its centre and the
/// square its saturation and value from where in it the pointer is.
#[test]
fn colour_picker_picks_by_angle_and_place() {
    let mut sh = fixture("picker");
    // The ring is 220 px at 10, 10: its centre is 120, 120. Its top,
    // then its right.
    sh.move_to(120.0, 18.0);
    sh.button(true);
    near(sh.num("hue"), 0.0, 1e-6, "top");
    for i in 1..=8 {
        let a = std::f32::consts::FRAC_PI_2 * i as f32 / 8.0;
        sh.move_to(120.0 + 102.0 * a.sin(), 120.0 - 102.0 * a.cos());
    }
    near(sh.num("hue"), 0.25, 1e-4, "right");
    sh.button(false);
    // The square is 120 px at 60, 60.
    sh.drag((70.0, 70.0), (150.0, 90.0), 6);
    near(sh.num("sat"), 0.75, 1e-6, "sat");
    near(sh.num("val"), 0.75, 1e-6, "val");
    // Beyond its edge while held: clamped.
    sh.drag((150.0, 90.0), (230.0, 230.0), 6);
    near(sh.num("sat"), 1.0, 1e-6, "sat clamped");
    near(sh.num("val"), 0.0, 1e-6, "val clamped");
    near(sh.num("hue"), 0.25, 1e-4, "the square turned the ring");
    sh.drag((150.0, 90.0), (150.0, 90.0), 1);
    let shot = sh.shot("Picker");
    assert_matches_ref("pointer_picker", shot.0, &shot.1, 3);
}

/// A widget dragged by its grip moves its own surface (margins from
/// state) under the pointer, and that move does not feed back: the
/// surface ends where the pointer took it.
#[test]
fn floating_widget_follows_its_grip() {
    let mut sh = fixture("floating");
    assert_eq!(sh.surface("Widget").origin, (40.0, 40.0));
    sh.drag((120.0, 52.0), (320.0, 252.0), 20);
    assert_eq!((sh.num("px"), sh.num("py")), (240.0, 240.0));
    sh.settle();
    assert_eq!(sh.surface("Widget").origin, (240.0, 240.0));
    // Again, back up and left past where it began.
    sh.drag((300.0, 250.0), (100.0, 150.0), 20);
    assert_eq!((sh.num("px"), sh.num("py")), (40.0, 140.0));
    sh.settle();
    assert_eq!(sh.surface("Widget").origin, (40.0, 140.0));
    assert!(!sh.throttled(), "a drag at 60 Hz was throttled");
    let shot = sh.shot("Widget");
    assert_matches_ref("pointer_floating", shot.0, &shot.1, 2);
}

/// A node's pointer is null until the pointer is over it, a value while
/// it is (one a frame however many moves the frame had), and null again
/// once it leaves.
#[test]
fn pointer_is_null_off_the_node_and_once_a_frame() {
    let src = "export state x = 0.0\n\
               export state y = 0.0\n\
               panel P { anchor: top_left; width: 200; height: 100\n\
                 row { width: 200; height: 100\n\
                   box { width: 100; height: 100 }\n\
                   box { id: b; width: 100; height: 100\n\
                     on change self.pointer { x = self.pointer?.x ?? -1; y = self.pointer?.y ?? -1 }\n\
                   }\n\
                 }\n\
               }\n";
    let mut sh = Shell::new(src);
    let at = |sh: &Shell| (sh.num("x"), sh.num("y"));
    assert_eq!(at(&sh), (0.0, 0.0));
    sh.move_to(150.0, 40.0);
    assert_eq!(at(&sh), (50.0, 40.0));
    // Off it, on the surface still: null.
    sh.move_to(50.0, 40.0);
    assert_eq!(at(&sh), (-1.0, -1.0));
    // Many moves before a frame: one value, the last.
    sh.move_to(150.0, 40.0);
    let before = sh.pointer_facts;
    for i in 0..10 {
        let id = sh.surface("P").id;
        sh.send(strand_scene::InputEvent::PointerMotion {
            surface: id,
            position: strand_scene::LogicalPoint::new(150.0 + i as f32, 50.0),
            time: 0,
        });
    }
    sh.run(1);
    assert_eq!(sh.pointer_facts - before, 1, "one value for the frame");
    assert_eq!(at(&sh), (59.0, 50.0));
    // Off the surface: null.
    sh.move_to(600.0, 600.0);
    assert_eq!(at(&sh), (-1.0, -1.0));
}

/// (still-pointer) Rows that move under a pointer that stays still: `hover`
/// and the cursor follow what is under it (spec.md 6.3), where they stayed
/// on the row it last moved over until it moved again. Clicks always landed
/// right; a held press keeps its chain.
const ROWS: &str = r#"
export state over = -1
export state gone = false
export state clicked = -1
panel P { width: 100; height: 100; anchor: top_left
  list { height: 100
    for i in [0, 1, 2, 3, 4, 5, 6, 7, 8, 9] key i {
      if i > 0 || !gone {
        box { height: 40; cursor: i % 2 == 0 ? pointer : crosshair
          on change self.hover { if self.hover { over = i } }
          on click { clicked = i }
        }
      }
    }
  }
}
"#;

#[test]
fn hover_and_the_cursor_follow_rows_moving_under_a_still_pointer() {
    use strand_scene::CursorShape;
    let mut sh = Shell::new(ROWS);
    let id = sh.surface("P").id;
    sh.move_to(50.0, 10.0);
    sh.settle();
    assert_eq!(sh.num("over"), 0.0);
    assert_eq!(sh.r.cursor(id), CursorShape::Pointer);
    // A wheel moves the list 45 px: row 1 is under the pointer, which has
    // not moved.
    sh.wheel(0.0, 3.0);
    sh.settle();
    assert_eq!(sh.num("over"), 1.0, "hover follows a scrolled list");
    assert_eq!(sh.r.cursor(id), CursorShape::Crosshair, "and the cursor");
    // The click agrees.
    sh.button(true);
    sh.button(false);
    sh.settle();
    assert_eq!(sh.num("clicked"), 1.0);
    // Back to the top, then the first row goes: the next takes its place.
    sh.wheel(0.0, -3.0);
    sh.settle();
    assert_eq!(sh.num("over"), 0.0);
    sh.set("gone", strand_compiler::vm::value::Value::Bool(true));
    sh.settle();
    assert_eq!(sh.num("over"), 1.0, "hover follows a row that closed");
    assert_eq!(sh.r.cursor(id), CursorShape::Crosshair);
}

/// (still-pointer) A press held on a node keeps its `hover` and cursor
/// while what is under the pointer moves.
#[test]
fn a_held_press_keeps_hover_when_rows_move_under_it() {
    let mut sh = Shell::new(ROWS);
    sh.move_to(50.0, 10.0);
    sh.settle();
    sh.button(true);
    sh.wheel(0.0, 3.0);
    sh.settle();
    assert_eq!(sh.num("over"), 0.0, "the pressed row stays hovered");
    // Released, the pointer is on what is under it now.
    sh.button(false);
    sh.settle();
    assert_eq!(sh.num("over"), 1.0, "hover right after the release");
    sh.wheel(0.0, 3.0);
    sh.settle();
    assert_eq!(sh.num("over"), 2.0);
}

/// (first-read) `isl.width` read for the first time as something mounts (a
/// popup's `on show`, a binding in it) gives the laid-out width: the element
/// is measured from its own mount. It read 0 until the answer to that first
/// read came back, which a handler never sees again.
#[test]
fn a_width_first_read_as_a_popup_mounts_is_already_known() {
    let mut sh = Shell::new(
        r#"
export state o = false
export state seen = -1.0
export state seen_h = -1.0
panel P { anchor: top_left; width: 200; height: 100
  box { id: isl; width: 150; height: 30; x: 0
    popup { open: o
      on show { seen = isl.width; seen_h = isl.height }
      box { width: 10; height: 10 }
    }
  }
}
"#,
    );
    assert_eq!(sh.num("seen"), -1.0, "not read yet");
    sh.set("o", strand_compiler::vm::value::Value::Bool(true));
    sh.settle();
    assert_eq!(sh.num("seen"), 150.0);
    assert_eq!(sh.num("seen_h"), 30.0);
}

/// (first-read) Measuring from the mount is for elements something reads: a
/// shell that reads no size asks render for none.
#[test]
fn only_what_is_read_is_measured() {
    let sh = Shell::new(
        r#"
panel P { anchor: top_left; width: 200; height: 100
  box { id: isl; width: 150; height: 30
    on click { }
  }
}
"#,
    );
    let tree = sh.r.tree();
    let boxes = sh.nodes("P", strand_scene::NodeKind::Box);
    assert_eq!(boxes.len(), 1);
    assert!(
        tree.get(boxes[0])
            .unwrap()
            .get(strand_scene::Prop::Watch)
            .is_none(),
        "not read, not measured"
    );
}

/// (still-pointer) A shell whose box moves itself by `POSE` when hovered,
/// counting its hover's flips.
fn lifting(pose: &str) -> Shell {
    let mut sh = Shell::new(&format!(
        r#"
export state n = 0
export state on = false
panel P {{ width: 200; height: 100; anchor: top_left
  col {{ pad: 20
    box {{ width: 100; height: 40; bg: #ff0000
      when hover {{ {pose} }}
      on change self.hover {{ n = n + 1; on = self.hover }}
    }}
  }}
}}
"#
    ));
    sh.settle();
    // The clock stays: nothing below rests a second by accident.
    sh.router.set_now(Some(strand_scene::time::Instant::now()));
    sh
}

/// (still-pointer) A node that moves itself off a still pointer when hovered
/// lets its surface come to rest, hovered: `hover` leaves and comes back twice
/// and stays, whatever moves it. It used to flip every other frame for good.
#[test]
fn a_hover_lift_comes_to_rest() {
    for (pose, at) in [
        ("y: -10", (50.0, 55.0)),
        ("y: 10", (50.0, 25.0)),
        ("x: 40", (25.0, 40.0)),
        ("y: -10 ~ instant", (50.0, 55.0)),
        ("y: -10 ~ spring(120, 0.5)", (50.0, 55.0)),
        ("scale: 0.5", (25.0, 25.0)),
        ("width: 20", (100.0, 40.0)),
    ] {
        let mut sh = lifting(pose);
        let id = sh.surface("P").id;
        sh.move_to(at.0, at.1);
        sh.run(600);
        assert!(!sh.r.wants_frame(id), "{pose}: it comes to rest");
        assert_eq!(sh.get("on").as_bool(), Some(true), "{pose}: hovered");
        assert_eq!(sh.num("n"), 5.0, "{pose}: on, off, on, off and on again");
        // And stays so.
        sh.run(60);
        assert_eq!(sh.num("n"), 5.0, "{pose}");
        // The pointer moving away ends it, and it can start again.
        sh.move_to(190.0, 95.0);
        sh.settle();
        assert_eq!(sh.get("on").as_bool(), Some(false), "{pose}: left");
    }
}

/// A node sliding away under the still pointer and back: it loses `hover` and
/// takes it back, every time it is given a rest between (a pager's slider
/// knob), and the rule for a node that moves itself never holds it.
const SLIDING: &str = r#"
export state away = false
export state on = false
panel P { width: 400; height: 100; anchor: top_left
  box { width: 100; height: 40; bg: #ff0000
    x: away ? 200 : 0
    on change self.hover { on = self.hover }
  }
}
"#;

#[test]
fn a_node_slid_away_and_back_follows_each_time_after_a_rest() {
    use std::time::Duration;
    use strand_compiler::vm::value::Value;
    let mut sh = Shell::new(SLIDING);
    sh.settle();
    let t0 = strand_scene::time::Instant::now();
    sh.router.set_now(Some(t0));
    sh.move_to(50.0, 20.0);
    sh.settle();
    assert_eq!(sh.get("on").as_bool(), Some(true));
    for i in 1..4u32 {
        sh.set("away", Value::Bool(true));
        sh.settle();
        assert_eq!(sh.get("on").as_bool(), Some(false), "round {i}: away");
        sh.set("away", Value::Bool(false));
        sh.settle();
        assert_eq!(sh.get("on").as_bool(), Some(true), "round {i}: back");
        // The surface rests.
        sh.router
            .set_now(Some(t0 + Duration::from_secs(u64::from(i) * 2)));
    }
}

/// With no rest between, a node that came back twice keeps `hover` when it
/// goes a third time, until the pointer moves: what ends the loop for one that
/// moves itself.
#[test]
fn a_node_back_under_the_pointer_with_no_rest_keeps_hover_until_the_pointer_moves() {
    use strand_compiler::vm::value::Value;
    let mut sh = Shell::new(SLIDING);
    sh.settle();
    sh.router.set_now(Some(strand_scene::time::Instant::now()));
    sh.move_to(50.0, 20.0);
    sh.settle();
    for _ in 0..2 {
        sh.set("away", Value::Bool(true));
        sh.settle();
        assert_eq!(sh.get("on").as_bool(), Some(false));
        sh.set("away", Value::Bool(false));
        sh.settle();
        assert_eq!(sh.get("on").as_bool(), Some(true));
    }
    sh.set("away", Value::Bool(true));
    sh.settle();
    assert_eq!(sh.get("on").as_bool(), Some(true), "held");
    sh.move_to(51.0, 20.0);
    sh.settle();
    assert_eq!(sh.get("on").as_bool(), Some(false), "the pointer moved");
}

/// A wheel is input: rows brought back and forth under a still pointer with
/// no rest between keep following.
#[test]
fn a_wheel_back_and_forth_keeps_hover_following() {
    let mut sh = Shell::new(ROWS);
    sh.router.set_now(Some(strand_scene::time::Instant::now()));
    sh.move_to(50.0, 10.0);
    sh.settle();
    for _ in 0..3 {
        sh.wheel(0.0, 3.0);
        sh.settle();
        assert_eq!(sh.num("over"), 1.0);
        sh.wheel(0.0, -3.0);
        sh.settle();
        assert_eq!(sh.num("over"), 0.0);
    }
}

/// A node that goes gives up its hover whatever moves it made, and hover goes
/// on to what takes its place, passing what lies below it on the way.
#[test]
fn a_node_gone_gives_up_its_hover() {
    use strand_compiler::vm::value::Value;
    let mut sh = Shell::new(
        r#"
export state away = false
export state gone = false
export state on = false
panel P { width: 400; height: 100; anchor: top_left
  col {
    if !gone {
      box { width: 100; height: 40; bg: #ff0000
        x: away ? 200 : 0
      }
    }
    box { width: 100; height: 40; bg: #00ff00
      on change self.hover { on = self.hover }
    }
  }
}
"#,
    );
    sh.settle();
    sh.router.set_now(Some(strand_scene::time::Instant::now()));
    sh.move_to(50.0, 20.0);
    sh.settle();
    for _ in 0..2 {
        sh.set("away", Value::Bool(true));
        sh.settle();
        sh.set("away", Value::Bool(false));
        sh.settle();
    }
    sh.set("gone", Value::Bool(true));
    sh.settle();
    assert_eq!(
        sh.get("on").as_bool(),
        Some(true),
        "the next box took its place"
    );
}

/// Passing the same gap between rows again and again is no loop: a fling with
/// no input in it lights each row it brings under the pointer.
#[test]
fn rows_passing_with_gaps_between_keep_hover_following() {
    let rows: String = (0..30)
        .map(|i| format!("    box {{ height: 20; shrink: 0; on change self.hover {{ if self.hover {{ over = {i} }} }} }}\n"))
        .collect();
    let mut sh = Shell::new(&format!(
        "export state over = -1\nexport state top = 0\npanel P {{ width: 100; height: 100; anchor: top_left\n  col {{ gap: 4; y: top ~ 600ms\n{rows}  }}\n}}\n"
    ));
    sh.settle();
    sh.router.set_now(Some(strand_scene::time::Instant::now()));
    sh.move_to(50.0, 10.0);
    sh.settle();
    assert_eq!(sh.num("over"), 0.0);
    sh.set("top", strand_compiler::vm::value::Value::float(-240.0));
    sh.settle();
    assert_eq!(sh.num("over"), 10.0, "the row it came to rest on");
}
