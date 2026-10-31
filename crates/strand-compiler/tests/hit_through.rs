//! (hit-through) `hit: none` on a surface's root: the pointer passes through
//! the surface wherever nothing on it takes input (spec.md 6.7), by its input
//! region. Without it a surface takes the pointer in its whole box.

mod shell;

use shell::Shell;
use strand_scene::{LogicalPoint, Painter};

fn takes(sh: &Shell, name: &str, x: f32, y: f32) -> bool {
    sh.r.takes_input_at(sh.surface(name).id, LogicalPoint::new(x, y))
}

const PANEL: &str = r#"
export state wide = 100
panel P { anchor: top_left; width: 300; height: 200; hit: none
  col { hit: none; gap: 20
    box { width: wide; height: 50; bg: #ff0000 }
    box { width: 40; height: 40; on click { wide = 120 } }
  }
}
"#;

/// Its empty area passes the pointer; what is on it takes it, a box that
/// draws nothing but handles a click included, and the gap between children
/// of a `hit: none` column does not. The region follows what moves.
#[test]
fn a_hit_none_panel_takes_the_pointer_only_on_what_is_on_it() {
    let mut sh = Shell::new(PANEL);
    assert!(takes(&sh, "P", 50.0, 25.0), "the drawn box");
    assert!(takes(&sh, "P", 20.0, 90.0), "the box that only handles");
    assert!(!takes(&sh, "P", 290.0, 190.0), "the empty area");
    assert!(!takes(&sh, "P", 20.0, 60.0), "the gap");
    assert!(!takes(&sh, "P", 110.0, 25.0));
    sh.set("wide", strand_compiler::vm::value::Value::float(120.0));
    sh.settle();
    assert!(takes(&sh, "P", 110.0, 25.0), "the region follows the box");
}

/// A press held on it takes its whole box, as on a hover popup: a compositor
/// that does not keep the pointer on the pressed surface would end the drag
/// at the box's edge. The release gives the region back.
#[test]
fn a_held_press_takes_the_whole_box() {
    let mut sh = Shell::new(PANEL);
    sh.move_to(50.0, 25.0);
    sh.button(true);
    sh.settle();
    assert!(takes(&sh, "P", 290.0, 190.0), "held");
    sh.button(false);
    sh.settle();
    assert!(!takes(&sh, "P", 290.0, 190.0), "released");
}

/// Without `hit: none` on the root the panel keeps its whole box, whatever
/// its children say: its own box is what lies below them.
#[test]
fn a_panel_without_it_keeps_its_box() {
    let sh = Shell::new(&PANEL.replace("height: 200; hit: none", "height: 200"));
    assert_eq!(Painter::input_region(&sh.r, sh.surface("P").id), None);
    assert!(takes(&sh, "P", 290.0, 190.0));
}

/// A shadow lies outside the region, which sits where the box is in the
/// buffer; the shadow's overhang takes nothing.
#[test]
fn a_shadow_takes_nothing() {
    let sh = Shell::new(
        r#"
panel P { anchor: top_left; width: 300; height: 200; hit: none
  box { width: 100; height: 50; bg: #ff0000; shadow: 0 0 20px #000000 }
}
"#,
    );
    let id = sh.surface("P").id;
    let region = Painter::input_region(&sh.r, id).expect("a region");
    assert_eq!(region.len(), 1, "{region:?}");
    let r = region[0];
    assert_eq!((r.w, r.h), (100.0, 50.0), "{region:?}");
    assert!(!takes(&sh, "P", r.x - 2.0, r.y + 2.0), "the shadow");
    assert!(takes(&sh, "P", r.x + 2.0, r.y + 2.0), "the box");
}

/// A hit box cut by a clip (rows scrolled out of a list) takes the pointer
/// only where it shows, and a child of a box that took it adds no rect.
#[test]
fn the_region_is_what_shows_and_stays_small() {
    let rows: String = (0..40)
        .map(|_| "    box { height: 40; bg: #333333; box { size: 10 } }\n".to_string())
        .collect();
    let sh = Shell::new(&format!(
        "panel P {{ anchor: top_left; width: 300; height: 200; hit: none\n  \
         col {{ hit: none; width: 100; height: 100\n    list {{ height: 100\n{rows}    }}\n  }}\n}}\n"
    ));
    let region = Painter::input_region(&sh.r, sh.surface("P").id).expect("a region");
    assert_eq!(region.len(), 1, "the list holds its rows: {region:?}");
    assert!(takes(&sh, "P", 50.0, 90.0));
    assert!(!takes(&sh, "P", 50.0, 110.0), "past the list's clip");
}

/// An OSD takes no input and keeps saying so: `hit: none` asks nothing.
#[test]
fn an_osd_is_left_alone() {
    let sh = Shell::new(
        r#"
osd O { anchor: top; hit: none
  box { width: 100; height: 50; bg: #ff0000 }
}
"#,
    );
    assert_eq!(Painter::input_region(&sh.r, sh.surface("O").id), None);
}

/// A panel spanning its output (decisions.md centred-surface) takes clicks
/// only on its box with or without `hit: none`; with it, only on what is in
/// that box, however wide its buffer.
#[test]
fn a_spanned_panel_takes_clicks_on_its_box_alone() {
    const SPANNED: &str = r#"
panel P { anchor: top; size: auto ~ spring(400, 0.8)
  row { gap: 40
    box { width: 60; height: 40; bg: #ff0000 }
    box { width: 60; height: 40; bg: #00ff00 }
  }
}
"#;
    let mut plain = Shell::new(SPANNED);
    strand_render::host_loop::woke(&mut plain).unwrap();
    plain.settle();
    let a = plain.surface("P");
    assert!(a.size.w > 400, "it spans: {:?}", a.size);
    let mid = a.size.w as f32 / 2.0;
    assert!(takes(&plain, "P", mid, 20.0), "the row's gap, in its box");
    assert!(!takes(&plain, "P", 5.0, 20.0), "the margin");

    let mut through = Shell::new(
        &SPANNED
            .replace("spring(400, 0.8)", "spring(400, 0.8); hit: none")
            .replace("row { gap: 40", "row { hit: none; gap: 40"),
    );
    strand_render::host_loop::woke(&mut through).unwrap();
    through.settle();
    assert!(!takes(&through, "P", mid, 20.0), "the gap passes it");
    assert!(takes(&through, "P", mid - 30.0, 20.0), "the first box");
    assert!(takes(&through, "P", mid + 30.0, 20.0), "the second");
    assert!(!takes(&through, "P", 5.0, 20.0), "the margin");
}
