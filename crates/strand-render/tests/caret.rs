//! (caret, docs/decisions.md caret) A focused `input`'s caret blinks as a
//! desktop's does: shown 600 ms, hidden 600 ms, shown again at once on a
//! key, a caret move, a text change or focus gained, and steady after
//! 10 s without one. The render thread alone redraws it, a turn damages
//! only the caret, and nothing blinks under `reduced_motion`, at time
//! zero (offline PNGs), once blurred or hidden, or when a host holds it
//! steady (the evaluation sandbox).

mod common;

use std::time::{Duration, Instant};

use common::*;
use strand_render::widgets::{
    CARET_BLINK_HALF, CARET_BLINK_TIMEOUT, Caret, caret_blink, caret_blink_stop,
};
use strand_render::{Flag, Renderer, Router};
use strand_scene::input::button;
use strand_scene::*;

const S: SurfaceId = SurfaceId(1);
/// The first frame's time on the presentation clock.
const T0: Duration = Duration::from_secs(10);
/// A refresh at 60 Hz.
const HZ60: Duration = Duration::from_nanos(16_666_667);

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// The island's admin-password prompt (the owner's report): a
/// `type: password` input in a row 24 px tall, `sans 13px 600`, its
/// caret in a red `$accent` on a black panel with white text, at
/// `scale`.
struct Field {
    r: Renderer,
    buf: Buffer,
    root: NodeId,
    input: NodeId,
    /// The wall-clock instant of [`T0`]: the renderer's timers read the
    /// presentation clock moved by it (`Renderer::set_now`).
    i0: Instant,
    scale: f64,
    /// The last frame painted.
    last: Duration,
}

fn field(scale: f64) -> Field {
    field_with(scale, &Shape::default())
}

/// What a field holds besides the owner's prompt.
struct Shape {
    /// The input's `text` (`None`: no `text` prop).
    text: Option<&'static str>,
    /// The input's own `bg`.
    bg: Option<&'static str>,
    /// The panel's size, logical px.
    size: (f32, f32),
    /// Rows of 40 text cells under the field (a real surface's content).
    rows: usize,
}

impl Default for Shape {
    fn default() -> Self {
        Shape {
            text: Some("hunter2"),
            bg: None,
            size: (240.0, 40.0),
            rows: 0,
        }
    }
}

fn field_with(scale: f64, shape: &Shape) -> Field {
    let mut r = renderer();
    let mut b = Builder::default();
    let mut tokens = TokenTable::default();
    tokens.insert("accent", color("#ff0000"));
    b.diff.set_tokens(tokens, Transition::Instant);
    let mut f = font(13.0);
    f.weight = 600;
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(shape.size.0)),
            (Prop::Height, num(shape.size.1)),
            (Prop::Bg, color("#000000")),
            (Prop::Color, color("#ffffff")),
            (Prop::Font, PropValue::Font(f)),
        ],
    );
    let row = b.node(
        NodeKind::Row,
        Some(root),
        vec![(Prop::Height, num(24.0)), (Prop::Pad, num(8.0))],
    );
    let mut props = vec![
        (Prop::Width, num(200.0)),
        (Prop::InputType, PropValue::Keyword("password".into())),
    ];
    props.extend(shape.text.map(|t| (Prop::Text, text(t))));
    props.extend(shape.bg.map(|c| (Prop::Bg, color(c))));
    let input = b.node(NodeKind::Input, Some(row), props);
    for i in 0..shape.rows {
        let cells = b.node(NodeKind::Row, Some(root), vec![(Prop::Gap, num(4.0))]);
        for j in 0..40 {
            b.node(
                NodeKind::Text,
                Some(cells),
                vec![(Prop::Text, text(&format!("c{i}.{j}")))],
            );
        }
    }
    assert!(r.apply(b.diff).is_empty());
    let sc = Scale::from_f64(scale).unwrap();
    let size = Size::new(
        (shape.size.0 as f64 * scale) as u32,
        (shape.size.1 as f64 * scale) as u32,
    );
    r.attach_surface(S, root);
    r.configure_surface(S, size, sc);
    let i0 = Instant::now();
    r.set_now(Some(i0));
    let mut buf = Buffer::new(size.w, size.h, sc);
    buf.paint_at(&mut r, S, 0, T0);
    Field {
        r,
        buf,
        root,
        input,
        i0,
        scale,
        last: T0,
    }
}

impl Field {
    /// Paints the frame presented at `t` (the renderer's clock there).
    fn paint(&mut self, t: Duration) -> Damage {
        self.r.set_now(Some(self.i0 + (t - T0)));
        self.last = t;
        self.buf.paint_at(&mut self.r, S, 1, t)
    }

    /// Focuses the input and paints the next frame: the blink starts.
    fn focus(&mut self) -> Duration {
        self.r.set_widget_flag(self.input, Flag::Focused, true);
        let t = self.last + HZ60;
        self.paint(t);
        t
    }

    /// The input's box in buffer pixels.
    fn input_box(&self) -> (u32, u32, u32, u32) {
        let b = self.r.boxes(S).unwrap().rects[&self.input];
        let s = self.scale;
        (
            (b.x as f64 * s) as u32,
            (b.y as f64 * s) as u32,
            (b.w as f64 * s).ceil() as u32,
            (b.h as f64 * s).ceil() as u32,
        )
    }

    /// The caret's columns: red pixels in the input's box (the text and
    /// bullets are white, the panel black).
    fn caret(&self) -> Vec<u32> {
        let (x0, y0, w, h) = self.input_box();
        (x0..x0 + w)
            .filter(|&x| {
                (y0..y0 + h).any(|y| {
                    let p = self.buf.px(x, y);
                    p[2] > 0x80 && p[1] < 0x40 && p[0] < 0x40
                })
            })
            .collect()
    }

    fn shown(&self) -> bool {
        !self.caret().is_empty()
    }

    /// Runs the loop at 60 Hz from the last frame until `until`, as the
    /// shell's does: the renderer's own wake runs `update` (its timer
    /// thread), and a surface that wants a frame paints. Each frame
    /// painted: its time, the caret shown, its damage.
    fn run(&mut self, until: Duration) -> Vec<(Duration, bool, Damage)> {
        let mut out = Vec::new();
        let mut t = self.last;
        while t + HZ60 <= until {
            t += HZ60;
            let now = self.i0 + (t - T0);
            self.r.set_now(Some(now));
            if self.r.next_wake().is_some_and(|w| w <= now) {
                self.r.update();
            }
            if self.r.wants_frame(S) {
                let d = self.paint(t);
                out.push((t, self.shown(), d));
            }
        }
        out
    }
}

/// The schedule: shown for the first half of each 1200 ms cycle, hidden
/// for the second, and shown for good from the first whole cycle past
/// 10 s (10.8 s).
#[test]
fn the_blink_is_600_ms_on_600_ms_off_and_ends_shown() {
    assert_eq!(CARET_BLINK_HALF, ms(600));
    assert_eq!(CARET_BLINK_TIMEOUT, Duration::from_secs(10));
    assert_eq!(caret_blink_stop(), ms(10_800));
    assert_eq!(caret_blink(ms(0)), (true, Some(ms(600))));
    assert_eq!(caret_blink(ms(599)), (true, Some(ms(600))));
    assert_eq!(caret_blink(ms(600)), (false, Some(ms(1200))));
    assert_eq!(caret_blink(ms(1199)), (false, Some(ms(1200))));
    assert_eq!(caret_blink(ms(1200)), (true, Some(ms(1800))));
    assert_eq!(caret_blink(ms(10_799)), (false, Some(ms(10_800))));
    assert_eq!(caret_blink(ms(10_800)), (true, None));
    assert_eq!(caret_blink(Duration::from_secs(3600)), (true, None));
}

/// Focused, the caret shows, hides 600 ms later and shows again 600 ms
/// after that, in the frames painted then; the unfocused field shows
/// none.
#[test]
fn a_focused_caret_shows_then_hides_then_shows() {
    let mut f = field(1.5);
    assert!(!f.shown(), "no caret before focus");
    let t1 = f.focus();
    assert!(f.shown(), "shown at focus");
    f.paint(t1 + ms(300));
    assert!(f.shown(), "shown 300 ms in");
    f.paint(t1 + ms(900));
    assert!(!f.shown(), "hidden 900 ms in");
    f.paint(t1 + ms(1500));
    assert!(f.shown(), "shown 1.5 s in");
    f.paint(t1 + ms(2100));
    assert!(!f.shown(), "hidden 2.1 s in");
}

/// The loop paints one frame per turn, the frame nearest it (a turn
/// counts from half a frame before it), and each shows the caret's new
/// phase: 18 turns, then nothing.
#[test]
fn the_loop_paints_once_per_turn_then_stops() {
    let mut f = field(1.0);
    let t1 = f.focus();
    let frames = f.run(t1 + Duration::from_secs(12));
    let turns: Vec<(Duration, bool)> = frames.iter().map(|(t, s, _)| (*t - t1, *s)).collect();
    assert_eq!(turns.len(), 18, "one frame per turn: {turns:?}");
    for (i, (at, shown)) in turns.iter().enumerate() {
        let want = CARET_BLINK_HALF * (i as u32 + 1);
        assert!(
            at.abs_diff(want) <= HZ60,
            "turn {i} at {at:?}, not {want:?}: {turns:?}"
        );
        assert_eq!(*shown, i % 2 == 1, "turn {i} at {at:?}");
    }
    let (end, shown) = turns[turns.len() - 1];
    assert!(shown && end.abs_diff(caret_blink_stop()) <= HZ60);
    assert!(!f.r.wants_frame(S), "steady: no frame");
    assert_eq!(f.r.next_wake(), None, "steady: no wake");
}

/// A key, a caret move (even one that leaves it where it was, Left at
/// the start), a click and a text change from logic each show a hidden
/// caret in the next frame and restart the blink from there.
#[test]
fn a_key_a_move_or_new_text_shows_the_caret_and_restarts_the_blink() {
    let mut f = field(1.0);
    let t1 = f.focus();
    // A caret move from the renderer's side (what the router does).
    f.paint(t1 + ms(700));
    assert!(!f.shown());
    f.r.set_caret(f.input, Some(Caret::at(7)));
    assert!(f.r.wants_frame(S), "a hidden caret asks for a frame");
    let t2 = t1 + ms(717);
    f.paint(t2);
    assert!(f.shown(), "shown at once");
    f.paint(t2 + ms(590));
    assert!(f.shown(), "the blink restarted at the key");
    f.paint(t2 + ms(610));
    assert!(!f.shown(), "and hides 600 ms after it");

    // New text from logic (a field cleared, a value bound elsewhere).
    let mut d = SceneDiff::new();
    d.set(f.input, Prop::Text, text("hunter22"));
    assert!(f.r.apply(d).is_empty());
    f.r.update();
    assert!(f.r.wants_frame(S));
    let t3 = t2 + ms(627);
    f.paint(t3);
    assert!(f.shown(), "shown with the new text");
    f.paint(t3 + ms(590));
    assert!(f.shown(), "the blink restarted at the change");
    f.paint(t3 + ms(610));
    assert!(!f.shown());

    // Keys through the router: a click puts the caret at the start, and
    // Left there moves nothing, yet still shows the caret.
    let mut router = Router::new();
    router.attached(S, f.root);
    let b = f.r.boxes(S).unwrap().rects[&f.input];
    let at = LogicalPoint::new(b.x + 1.0, b.y + b.h / 2.0);
    for state in [ButtonState::Pressed, ButtonState::Released] {
        router.handle(
            &InputEvent::PointerButton {
                surface: S,
                position: at,
                button: button::LEFT,
                state,
                time: 0,
            },
            &mut f.r,
        );
    }
    let t4 = t3 + ms(627);
    f.paint(t4);
    assert!(f.shown(), "shown after the click");
    f.paint(t4 + ms(700));
    assert!(!f.shown());
    let left = InputEvent::Key {
        surface: S,
        key: KeyInput {
            name: "Left".into(),
            text: String::new(),
            state: ButtonState::Pressed,
            repeat: false,
            modifiers: Modifiers::default(),
            time: 0,
            latin: None,
        },
    };
    router.handle(&left, &mut f.r);
    f.r.update();
    assert!(f.r.wants_frame(S), "a key that moved nothing still asks");
    let t5 = t4 + ms(717);
    f.paint(t5);
    assert!(f.shown(), "shown at the key");
    f.paint(t5 + ms(590));
    assert!(f.shown(), "the blink restarted at the key");
}

/// A key while the caret shows asks for one frame, which starts the
/// blink there, and changes no pixel.
#[test]
fn a_key_while_shown_restarts_the_blink_in_one_unchanged_frame() {
    let mut f = field(1.0);
    let t1 = f.focus();
    f.paint(t1 + ms(400));
    assert!(f.shown());
    f.r.set_caret(f.input, Some(Caret::at(7)));
    f.r.update();
    assert!(f.r.wants_frame(S), "the restart is painted");
    let t2 = t1 + ms(417);
    let d = f.paint(t2);
    assert!(d.is_empty(), "nothing changed on screen: {d:?}");
    assert!(!f.r.wants_frame(S), "once");
    f.paint(t2 + ms(590));
    assert!(f.shown(), "restarted at the key, not still at focus");
}

/// Focus gained again restarts the blink; lost, the caret goes and
/// nothing wakes; hidden (opacity 0 above it) nothing wakes either.
#[test]
fn blurred_or_hidden_the_caret_wakes_nothing() {
    let mut f = field(1.0);
    let t1 = f.focus();
    f.paint(t1 + ms(700));
    assert!(!f.shown());
    f.r.set_widget_flag(f.input, Flag::Focused, false);
    let t2 = t1 + ms(717);
    f.paint(t2);
    assert!(!f.shown());
    assert!(!f.r.wants_frame(S) && f.r.next_wake().is_none(), "blurred");
    assert!(f.run(t2 + Duration::from_secs(2)).is_empty(), "no frame");

    // Focused again: shown at once, blinking from there.
    let t3 = f.focus();
    assert!(f.shown());
    f.paint(t3 + ms(590));
    assert!(f.shown(), "restarted at focus");
    assert!(f.r.next_wake().is_some() || f.r.wants_frame(S));

    // Hidden: its subtree is not drawn, so its clock stops.
    let mut d = SceneDiff::new();
    d.set(f.input, Prop::Opacity, num(0.0));
    assert!(f.r.apply(d).is_empty());
    let t4 = t3 + ms(607);
    f.paint(t4);
    assert!(!f.shown());
    let mut more = f.run(t4 + Duration::from_secs(1));
    more.retain(|(t, ..)| *t > t4 + ms(400));
    assert!(more.is_empty(), "frames while hidden: {more:?}");
    assert_eq!(f.r.next_wake(), None, "no wake while hidden");
}

/// `reduced_motion` holds the caret steady: shown, no frame, no wake;
/// motion back, it blinks again.
#[test]
fn reduced_motion_holds_the_caret_steady() {
    let mut f = field(1.0);
    f.r.set_reduced_motion(true);
    let t1 = f.focus();
    assert!(f.shown());
    let frames = f.run(t1 + Duration::from_secs(3));
    assert!(frames.is_empty(), "frames: {frames:?}");
    assert_eq!(f.r.next_wake(), None);
    for at in [700, 1900, 2500] {
        f.paint(t1 + ms(at));
        assert!(f.shown(), "{at} ms");
    }
    f.r.set_reduced_motion(false);
    let t2 = t1 + ms(2517);
    f.paint(t2);
    assert!(f.shown());
    f.paint(t2 + ms(700));
    assert!(!f.shown(), "motion back: it blinks");
}

/// Offline frames at time zero (the pixel references) show the caret
/// steady, whenever they are painted, and arm no wake.
#[test]
fn a_frame_at_time_zero_shows_the_caret_steady() {
    let mut f = field(1.0);
    f.r.set_widget_flag(f.input, Flag::Focused, true);
    for _ in 0..3 {
        f.buf.paint(&mut f.r, S, 1);
        assert!(f.shown());
    }
    assert!(!f.r.wants_frame(S));
    assert_eq!(f.r.next_wake(), None);
}

/// A host that holds the caret steady (`set_caret_blink(false)`, the
/// evaluation sandbox) sees it shown in every frame, with no wake.
#[test]
fn a_host_may_hold_the_caret_steady() {
    let mut f = field(1.0);
    f.r.set_caret_blink(false);
    let t1 = f.focus();
    assert!(f.run(t1 + Duration::from_secs(3)).is_empty());
    assert_eq!(f.r.next_wake(), None);
    f.paint(t1 + ms(700));
    assert!(f.shown());
    f.r.set_caret_blink(true);
    let t2 = t1 + ms(717);
    f.paint(t2);
    assert!(f.shown());
    f.paint(t2 + ms(700));
    assert!(!f.shown(), "blinking again");
}

/// A turn damages the caret alone (its pixels grown by one, not the
/// field), and the partial redraw leaves the buffer as a full paint of
/// the field draws it: hidden, as the field unfocused; shown, as the
/// frame at focus. So in an empty field that draws its own background
/// (a password prompt just opened: no text, so no glyphs to stand
/// among). Prints what a turn costs.
#[test]
fn a_turn_damages_only_the_caret() {
    let shapes = [
        Shape::default(),
        Shape {
            bg: Some("#202020"),
            ..Shape::default()
        },
        Shape {
            text: Some(""),
            bg: Some("#202020"),
            ..Shape::default()
        },
        Shape {
            text: None,
            bg: Some("#202020"),
            ..Shape::default()
        },
    ];
    for (shape, scale) in shapes.iter().flat_map(|s| [1.0, 1.5].map(|k| (s, k))) {
        let what = format!("{:?} with bg {:?} at {scale}", shape.text, shape.bg);
        let mut f = field_with(scale, shape);
        let blank = f.buf.pixels.clone();
        let t1 = f.focus();
        let shown = f.buf.pixels.clone();
        let (_, _, w, h) = f.input_box();
        let mut areas = Vec::new();
        let mut spent = Duration::ZERO;
        let frames = 6;
        for k in 1..=frames {
            let t = t1 + CARET_BLINK_HALF * k;
            let start = Instant::now();
            let d = f.paint(t);
            spent += start.elapsed();
            let hidden = k % 2 == 1;
            assert_eq!(f.shown(), !hidden, "turn {k}, {what}");
            let caret_w = (1.5 * scale).ceil() as u64 + 4;
            let line = (13.0 * 1.4 * scale).ceil() as u64 + 4;
            assert!(
                d.area() <= caret_w * line,
                "turn {k}, {what}: damage {d:?} past the caret ({caret_w}×{line})"
            );
            assert!(d.area() > 0, "turn {k} damages the caret");
            assert!(d.area() * 20 < u64::from(w * h), "far under the field");
            let want = if hidden { &blank } else { &shown };
            assert!(
                f.buf.pixels == *want,
                "turn {k}, {what}: the partial redraw differs from a full paint"
            );
            areas.push(d.area());
        }
        eprintln!(
            "caret turn, {what}: {:?} per frame (CPU paint), damage {areas:?} px",
            spent / frames
        );
    }
}

/// A glitch report's drawn nodes carry a blinking caret's box, shown or
/// hidden at the report (the replay holds it steady and leaves those
/// pixels out), and none once it is steady.
#[test]
fn a_reports_drawn_input_carries_its_blinking_caret() {
    let mut f = field(1.5);
    let t1 = f.focus();
    let caret = |f: &mut Field| {
        let input = f.input;
        f.r.drawn_nodes(S)
            .unwrap()
            .nodes
            .into_iter()
            .find(|n| n.node == input)
            .unwrap()
            .caret
    };
    let shown = caret(&mut f).expect("blinking, shown");
    let b = f.r.boxes(S).unwrap().rects[&f.input];
    assert!(shown.x >= b.x && shown.x + shown.w <= b.x + b.w + 0.01);
    assert!(
        shown.w > 1.0 && shown.w < 2.0 && shown.h > 10.0,
        "{shown:?}"
    );
    f.paint(t1 + ms(900));
    assert!(!f.shown());
    assert_eq!(caret(&mut f), Some(shown), "hidden, the same box");
    f.paint(t1 + Duration::from_secs(11));
    assert_eq!(caret(&mut f), None, "steady after the timeout");
}

/// A turn reuses the frame's scene, flattening nothing, so it costs the
/// same on a surface full of other content as on the field alone; and a
/// key flattens as often as it does with the caret held steady (the
/// frame reuses the key's preview). Prints what a turn and a key cost,
/// on the field alone and on an 800 × 600 panel at scale 1.5 holding
/// 400 text cells besides it.
#[test]
fn a_turn_flattens_nothing_and_a_key_no_more_than_a_steady_caret() {
    let big = || Shape {
        size: (800.0, 600.0),
        rows: 10,
        ..Shape::default()
    };
    for (shape, scale) in [(Shape::default(), 1.0), (big(), 1.5)] {
        let mut f = field_with(scale, &shape);
        let t1 = f.focus();
        let before = f.r.flattens();
        let start = Instant::now();
        let frames = f.run(t1 + ms(3700));
        let spent = start.elapsed();
        assert_eq!(frames.len(), 6, "a frame per turn: {frames:?}");
        assert_eq!(f.r.flattens(), before, "a turn flattens nothing");
        let per_turn = spent / frames.len() as u32;
        let (blinking, per_key) = typing(&mut f);
        let mut steady = field_with(scale, &shape);
        steady.r.set_caret_blink(false);
        steady.focus();
        let (held, held_per_key) = typing(&mut steady);
        assert_eq!(
            blinking, held,
            "a key flattens no more with the caret blinking than held steady"
        );
        eprintln!(
            "caret on {:?} at {scale}: a turn {per_turn:?} (the loop's update and paint); \
             a key {per_key:?} blinking, {held_per_key:?} steady (apply, update, paint), \
             {blinking} flattens in 30 keys",
            shape.size
        );
    }
}

/// Types 30 characters into `f`'s field, a frame 100 ms after each:
/// the scenes flattened, and the time a key took.
fn typing(f: &mut Field) -> (u64, Duration) {
    let keys = 30u32;
    let mut t = f.last;
    let before = f.r.flattens();
    let start = Instant::now();
    for k in 0..keys {
        let mut d = SceneDiff::new();
        let typed = "x".repeat(k as usize + 1);
        d.set(f.input, Prop::Text, text(&typed));
        assert!(f.r.apply(d).is_empty());
        f.r.set_caret(f.input, Some(Caret::at(typed.len())));
        f.r.update();
        t += ms(100);
        f.paint(t);
        assert!(f.shown(), "key {k}: shown");
    }
    (f.r.flattens() - before, start.elapsed() / keys)
}

/// A key that edits and moves nothing (one the input does not take, a
/// copy, an arrow through its `nav:` list) shows a hidden caret and
/// starts the blink anew, as every key does.
#[test]
fn a_key_that_edits_nothing_still_shows_the_caret() {
    let mut f = field(1.0);
    let mut router = Router::new();
    router.attached(S, f.root);
    let b = f.r.boxes(S).unwrap().rects[&f.input];
    let at = LogicalPoint::new(b.x + b.w - 1.0, b.y + b.h / 2.0);
    for state in [ButtonState::Pressed, ButtonState::Released] {
        router.handle(
            &InputEvent::PointerButton {
                surface: S,
                position: at,
                button: button::LEFT,
                state,
                time: 0,
            },
            &mut f.r,
        );
    }
    let mut from = f.last + HZ60;
    f.paint(from);
    assert!(f.shown(), "focused by the click");
    for (name, modifiers) in [
        ("F5", Modifiers::default()),
        (
            "c",
            Modifiers {
                ctrl: true,
                ..Modifiers::default()
            },
        ),
    ] {
        let t = from + ms(700);
        f.paint(t);
        assert!(!f.shown(), "{name}: hidden before the key");
        router.handle(&key(name, modifiers), &mut f.r);
        f.r.update();
        assert!(f.r.wants_frame(S), "{name} asks for a frame");
        from = t + HZ60;
        f.paint(from);
        assert!(f.shown(), "{name}: shown at the key");
        f.paint(from + ms(590));
        assert!(f.shown(), "{name}: the blink restarted at the key");
    }
}

/// The keyboard back on the surface (from a popup that grabbed it, no
/// leave between) keeps the input focused, and its caret shows again
/// and blinks anew, as on focus gained.
#[test]
fn the_keyboard_back_shows_the_caret_and_restarts_the_blink() {
    let mut f = field(1.0);
    let mut router = Router::new();
    router.attached(S, f.root);
    router.handle(&InputEvent::KeyboardEnter { surface: S }, &mut f.r);
    let b = f.r.boxes(S).unwrap().rects[&f.input];
    let at = LogicalPoint::new(b.x + 1.0, b.y + b.h / 2.0);
    for state in [ButtonState::Pressed, ButtonState::Released] {
        router.handle(
            &InputEvent::PointerButton {
                surface: S,
                position: at,
                button: button::LEFT,
                state,
                time: 0,
            },
            &mut f.r,
        );
    }
    let t1 = f.last + HZ60;
    f.paint(t1);
    assert!(f.shown());
    f.paint(t1 + ms(700));
    assert!(!f.shown(), "hidden before the keyboard comes back");
    router.handle(&InputEvent::KeyboardEnter { surface: S }, &mut f.r);
    f.r.update();
    assert!(f.r.wants_frame(S), "the keyboard back asks for a frame");
    let t2 = t1 + ms(717);
    f.paint(t2);
    assert!(f.shown(), "shown with the keyboard back");
    f.paint(t2 + ms(590));
    assert!(f.shown(), "the blink restarted there");
}

fn key(name: &str, modifiers: Modifiers) -> InputEvent {
    InputEvent::Key {
        surface: S,
        key: KeyInput {
            name: name.into(),
            text: String::new(),
            state: ButtonState::Pressed,
            repeat: false,
            modifiers,
            time: 0,
            latin: None,
        },
    }
}
