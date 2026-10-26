//! (flex) Controls and containers of the 2026-10-24 gap review
//! (decisions.md `## flex`), each a whole shell under
//! `tests/fixtures/flex/` driven through the host loop (`tests/shell`):
//! sliders and meters that run upward when laid out taller than wide, a
//! slider's `step`, a turned slider, rows and columns that wrap, and
//! scrolls along x, along both axes and with `snap`, checked by their
//! values and drawn against `tests/refs/controls_*.png`.

mod shell;

use std::time::Duration;

use shell::{Shell, assert_matches_ref, px};
use strand_scene::{NodeId, NodeKind};

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

/// Blue (`#5b8def`) as premultiplied BGRA.
fn blue(p: [u8; 4]) -> bool {
    p == [0xef, 0x8d, 0x5b, 0xff]
}

/// Orange (`#e0a040`).
fn orange(p: [u8; 4]) -> bool {
    p == [0x40, 0xa0, 0xe0, 0xff]
}

/// Green (`#6fbf73`).
fn green(p: [u8; 4]) -> bool {
    p == [0x73, 0xbf, 0x6f, 0xff]
}

/// The background (`#101418`).
fn bg(p: [u8; 4]) -> bool {
    p == [0x18, 0x14, 0x10, 0xff]
}

/// A slider laid out taller than wide runs upward: the pointer's height
/// along its track is its value, 0 at the bottom, and a meter so laid
/// out fills from the bottom.
#[test]
fn a_tall_slider_and_meter_run_upward() {
    let mut sh = fixture("sliders");
    // The slider is 24 × 160 at 16, 16: its knob runs from y 168 (0) to
    // 24 (1), the pressed knob's radius in from each end.
    sh.move_to(28.0, 132.0);
    sh.button(true);
    near(sh.num("vol"), 0.25, 0.002, "press");
    sh.move_to(28.0, 24.0);
    near(sh.num("vol"), 1.0, 1e-6, "top");
    sh.move_to(28.0, 300.0);
    near(sh.num("vol"), 0.0, 1e-6, "below");
    sh.move_to(28.0, 96.0);
    sh.button(false);
    near(sh.num("vol"), 0.5, 0.002, "released");
    let shot = sh.shot("Controls");
    // The track's filled part is below the knob, the rest above it.
    assert!(blue(px(&shot, 28, 150)), "filled below");
    assert!(!blue(px(&shot, 28, 40)), "track above");
    // The meter (8 × 160 at 56, 16), three quarters full from the
    // bottom.
    assert!(blue(px(&shot, 60, 100)), "meter filled");
    assert!(!blue(px(&shot, 60, 30)), "meter empty at the top");
    assert_matches_ref("controls_sliders", shot.0, &shot.1, 2);
}

/// A slider or meter that starts from a written 0 and grows (`width: 0;
/// grow: 1`, sharing what is left of its row) runs along the axis it
/// grows on, though the 0 written there is less than its other side: a
/// 14 px tall slider sharing a row runs across, fills across and drags
/// across, and one sharing a column runs upward.
#[test]
fn a_slider_growing_from_zero_runs_along_its_growth() {
    let mut sh = Shell::new(
        "export state a = 0.5\n\
         export state b = 0.5\n\
         panel P { anchor: top_left; width: 300; height: 200\n\
           col { width: 300; height: 200; bg: #101418\n\
             row { width: 300; height: 20\n\
               slider { value: <-> a; width: 0; grow: 1; height: 14; color: #5b8def; track: #2a3038 }\n\
             }\n\
             col { width: 20; height: 180; align: start\n\
               slider { value: <-> b; height: 0; grow: 1; width: 14; color: #6fbf73; track: #2a3038 }\n\
             }\n\
           }\n\
         }\n",
    );
    let shot = sh.shot("P");
    // Across: filled on the left half, the track on the right.
    assert!(blue(px(&shot, 60, 10)), "filled across");
    assert!(!blue(px(&shot, 240, 10)), "the track across");
    // The 300 px slider's knob runs from x 8 (0) to 292 (1).
    sh.move_to(8.0 + 0.8 * 284.0, 10.0);
    sh.button(true);
    near(sh.num("a"), 0.8, 0.002, "dragged across");
    sh.button(false);
    // The column's slider (14 × 180 at 0, 20) runs upward: its top is 1
    // and its bottom 0, whatever the pointer's x.
    sh.move_to(7.0, 21.0);
    sh.button(true);
    near(sh.num("b"), 1.0, 1e-6, "upward: the top");
    sh.move_to(7.0, 199.0);
    near(sh.num("b"), 0.0, 1e-6, "upward: the bottom");
    sh.button(false);
}

/// `step:` moves a dragged slider in whole steps of its range's units,
/// the knob and what it writes alike.
#[test]
fn a_stepped_slider_moves_in_whole_steps() {
    let mut sh = fixture("sliders");
    assert_eq!(sh.num("level"), 30.0);
    // 180 wide at 80, 16: the knob runs from x 88 (0) to 252 (100).
    sh.move_to(88.0 + 0.47 * 164.0, 25.0);
    sh.button(true);
    assert_eq!(sh.num("level"), 50.0);
    sh.move_to(88.0 + 0.71 * 164.0, 25.0);
    assert_eq!(sh.num("level"), 70.0);
    // The knob is drawn at the step, not under the pointer.
    let slider = sh.nodes("Controls", NodeKind::Slider)[1];
    let drag = sh.r.widgets().drags.get(&slider).copied();
    assert_eq!(drag, Some(0.7));
    sh.move_to(500.0, 25.0);
    assert_eq!(sh.num("level"), 100.0);
    sh.button(false);
    assert_eq!(sh.num("level"), 100.0);
}

/// A slider keeps the axis its drag began on while the press is held,
/// even when its box turns wider than tall under the drag.
#[test]
fn a_drag_keeps_its_axis() {
    let mut sh = Shell::new(
        "export state v = 0.5\n\
         export state w = 24\n\
         panel P { anchor: top_left; width: 400; height: 200\n\
           slider { value: <-> v; width: w; height: 160 }\n\
         }\n",
    );
    // 24 × 160 at the origin: the knob runs from y 152 (0) to 8 (1).
    sh.move_to(12.0, 80.0);
    sh.button(true);
    near(sh.num("v"), 0.5, 0.002, "press");
    sh.set("w", strand_compiler::vm::Value::int(300));
    sh.move_to(12.0, 8.0);
    near(sh.num("v"), 1.0, 1e-6, "still upright");
    sh.button(false);
}

/// A decimal `step` writes the step's value as written in decimals
/// (`0.9`, not `0.90000004`), from a decimal low end too.
#[test]
fn a_decimal_step_writes_whole_decimals() {
    let mut sh = Shell::new(
        "export state a = 0.0\n\
         export state b = 0.15\n\
         panel P { anchor: top_left; width: 300; height: 100\n\
           col { width: 200\n\
             slider { value: <-> a; range: 0, 1; step: 0.1; width: 200; height: 20 }\n\
             slider { value: <-> b; range: 0.15, 2.15; step: 0.05; width: 200; height: 20 }\n\
           }\n\
         }\n",
    );
    // The knob runs from x 8 to 192 on each.
    let x = |f: f32| 8.0 + f * 184.0;
    sh.move_to(x(0.9), 10.0);
    sh.button(true);
    assert_eq!(sh.num("a"), 0.9);
    sh.button(false);
    for (f, want) in [
        (0.1, 0.35),
        (0.15, 0.45),
        (0.275, 0.7),
        (0.325, 0.8),
        (0.4, 0.95),
        (0.45, 1.05),
        (0.975, 2.1),
    ] {
        sh.move_to(x(f), 30.0);
        sh.button(true);
        assert_eq!(sh.num("b"), want, "at {f}");
        sh.button(false);
    }
}

/// A slider turned by `rotate` follows the pointer along its drawn
/// track: turned a quarter anticlockwise, it runs upward.
#[test]
fn a_turned_slider_follows_its_drawn_track() {
    let mut sh = fixture("sliders");
    // Centred in a 200 px panel at 400, 0: its track stands from y 48
    // (1) to 152 (0) at x 500.
    sh.move_to(500.0, 100.0);
    sh.button(true);
    near(sh.num("turned"), 0.5, 0.01, "press at the middle");
    sh.move_to(500.0, 60.0);
    near(sh.num("turned"), 0.5 + 40.0 / 104.0, 0.01, "up");
    sh.move_to(500.0, 170.0);
    near(sh.num("turned"), 0.0, 1e-6, "past the bottom");
    sh.button(false);
    let shot = sh.shot("Turned");
    assert_matches_ref("controls_turned", shot.0, &shot.1, 2);
}

/// `wrap: true` runs a row's children on in another row when one is
/// full, `gap` between rows too; `justify` places each line; a column
/// with a height wraps into further columns.
#[test]
fn rows_and_columns_wrap() {
    let mut sh = fixture("wrap");
    let shot = sh.shot("Wrap");
    let at = |x, y| px(&shot, x, y);
    // Tags 50 × 30, 8 apart, in a 180 px row at 10, 10: three a line.
    for (x, y) in [
        (35, 25),
        (93, 25),
        (151, 25),
        (35, 63),
        (151, 63),
        (35, 101),
    ] {
        assert!(blue(at(x, y)), "a tag at {x}, {y}");
    }
    assert!(bg(at(93, 101)), "seven tags: the third line holds one");
    // A centred wrapped row (190 px at 200, 10): its second line's one
    // item is centred, at 270.
    assert!(orange(at(237, 25)), "first line, first item");
    assert!(orange(at(295, 63)), "second line centred");
    assert!(bg(at(237, 63)), "nothing at the second line's start");
    // A wrapped column 100 tall at 200, 88: two 30 px items a column,
    // 6 apart, columns 46 apart.
    for (x, y) in [(220, 103), (220, 139), (266, 103), (266, 139), (312, 103)] {
        assert!(green(at(x, y)), "a dot at {x}, {y}");
    }
    assert!(bg(at(312, 139)), "five dots: the third column holds one");
    assert_matches_ref("controls_wrap", shot.0, &shot.1, 2);
}

fn first(sh: &Shell, surface: &str, kind: NodeKind) -> NodeId {
    sh.nodes(surface, kind)[0]
}

/// The offset of scroll `id` along x.
fn across(sh: &Shell, id: NodeId) -> f32 {
    sh.r.scroll_state(id)
        .and_then(|s| s.across.as_ref())
        .map_or(0.0, |a| a.offset)
}

fn down(sh: &Shell, id: NodeId) -> f32 {
    sh.r.scroll_state(id).map_or(0.0, |s| s.offset)
}

/// Lets time pass with no input, so a touchpad's recent motion is old.
fn pause(sh: &mut Shell) {
    sh.now += Duration::from_millis(300);
    sh.run(2);
}

/// `axis: horizontal; snap: true`: a strip of 120 px cards 10 apart
/// scrolls sideways under a plain wheel, a notch to the next card; a
/// touchpad moves it freely and its lift settles on the nearest card,
/// carried on by a fling.
#[test]
fn a_snapping_strip_settles_on_a_card() {
    let mut sh = fixture("snap");
    let strip = first(&sh, "Strip", NodeKind::Scroll);
    sh.move_to(150.0, 50.0);
    sh.wheel(0.0, 1.0);
    sh.settle();
    assert_eq!(across(&sh, strip), 130.0, "a notch goes to the next card");
    sh.wheel(0.0, 1.0);
    sh.settle();
    assert_eq!(across(&sh, strip), 260.0);
    sh.wheel(0.0, -1.0);
    sh.settle();
    assert_eq!(across(&sh, strip), 130.0, "and back");
    assert_eq!(down(&sh, strip), 0.0, "nothing along y");
    let shot = sh.shot("Strip");
    assert_matches_ref("controls_snap_strip", shot.0, &shot.1, 2);
    // Fingers move it 120 px and rest: their lift settles at 260.
    for _ in 0..3 {
        sh.touch(40.0, 0.0);
    }
    assert_eq!(across(&sh, strip), 250.0, "a touchpad follows the fingers");
    pause(&mut sh);
    sh.lift();
    sh.settle();
    assert_eq!(across(&sh, strip), 260.0, "the nearest card");
    // A quick swipe flings on past where the fingers left it, onto a
    // card's start.
    for _ in 0..4 {
        sh.touch(30.0, 0.0);
    }
    let lifted = across(&sh, strip);
    sh.lift();
    sh.settle();
    let at = across(&sh, strip);
    assert!(at > lifted + 30.0, "flung on: {lifted} to {at}");
    assert!(at % 130.0 == 0.0 || at == 990.0, "on a card: {at}");
}

/// `snap: true` on a virtualised list settles on rows, mounted or not:
/// a notch moves a row, a far wheel lands on a row's top, and fingers
/// resting between rows settle on the nearest.
#[test]
fn a_snapping_list_settles_on_rows() {
    let mut sh = fixture("snap");
    let list = first(&sh, "Rows", NodeKind::List);
    sh.move_to(500.0, 100.0);
    sh.wheel(0.0, 1.0);
    sh.settle();
    assert_eq!(down(&sh, list), 40.0);
    sh.touch(0.0, 30.0);
    assert_eq!(down(&sh, list), 70.0);
    pause(&mut sh);
    sh.lift();
    sh.settle();
    assert_eq!(down(&sh, list), 80.0);
    // 200 notches: 3,000 px, far past the mounted rows.
    sh.wheel(0.0, 200.0);
    sh.settle();
    assert_eq!(down(&sh, list), 3080.0);
    assert_eq!(
        sh.r.list_frames().top_row,
        77,
        "the row at the top is whole"
    );
    assert_eq!(sh.r.list_frames().gaps, 0);
}

/// A vertical swipe or wheel over a sideways strip inside a page scrolls
/// the page, not the strip: a plain wheel turns a strip only where
/// nothing around it scrolls down, and a touchpad's fingers never do.
#[test]
fn a_strip_in_a_page_leaves_vertical_scrolling_to_the_page() {
    let mut sh = fixture("snap");
    let scrolls = sh.nodes("Page", NodeKind::Scroll);
    let (page, strip) = (scrolls[0], scrolls[1]);
    sh.move_to(800.0, 150.0);
    sh.touch(0.0, 40.0);
    assert_eq!(down(&sh, page), 40.0, "the page follows the fingers");
    assert_eq!(across(&sh, strip), 0.0);
    pause(&mut sh);
    sh.lift();
    sh.settle();
    sh.move_to(800.0, 100.0);
    sh.wheel(0.0, 1.0);
    sh.settle();
    assert_eq!(down(&sh, page), 55.0, "a wheel scrolls the page");
    assert_eq!(across(&sh, strip), 0.0, "not the strip");
    // Sideways, the strip.
    sh.wheel(2.0, 0.0);
    sh.settle();
    assert_eq!(across(&sh, strip), 30.0);
}

/// A touchpad gesture keeps the axis its first motion took: a swipe
/// that starts sideways on an `axis: both` scroll stays sideways, so it
/// neither steps between the axes nor leaves one between children.
#[test]
fn a_touchpad_gesture_keeps_its_axis() {
    let mut sh = fixture("snap");
    let both = first(&sh, "Both", NodeKind::Scroll);
    sh.move_to(100.0, 375.0);
    sh.touch(30.0, 20.0);
    sh.touch(10.0, 30.0);
    sh.touch(10.0, 30.0);
    assert_eq!(across(&sh, both), 50.0);
    assert_eq!(down(&sh, both), 0.0, "never along y");
    pause(&mut sh);
    sh.lift();
    sh.settle();
    // The next gesture picks again.
    sh.touch(0.0, 20.0);
    assert_eq!(down(&sh, both), 20.0);
    assert_eq!(across(&sh, both), 50.0);
}

/// `axis: both`: a wheel scrolls it down, a sideways wheel across.
#[test]
fn a_scroll_along_both_axes() {
    let mut sh = fixture("snap");
    let both = first(&sh, "Both", NodeKind::Scroll);
    sh.move_to(100.0, 375.0);
    sh.wheel(0.0, 1.0);
    sh.settle();
    assert_eq!(down(&sh, both), 15.0);
    assert_eq!(across(&sh, both), 0.0);
    sh.wheel(2.0, 0.0);
    sh.settle();
    assert_eq!(across(&sh, both), 30.0);
    assert_eq!(down(&sh, both), 15.0);
    // At most the content's size less the view's.
    sh.wheel(0.0, 100.0);
    sh.wheel(100.0, 0.0);
    sh.settle();
    assert_eq!(down(&sh, both), 300.0);
    assert_eq!(across(&sh, both), 400.0);
}
