//! Drag and drop offline (M4, design.md "Lists": `drag:` and `on drop`):
//! the dragged source is painted at the pointer above its siblings, it
//! springs back into its box when nothing takes it, and a drop that
//! logic answers by moving the row by key springs from where it was
//! let go into its new slot.

mod common;

use std::time::Duration;

use common::*;
use strand_render::{Intent, NodeEvent, Renderer, Router};
use strand_scene::input::button;
use strand_scene::*;

const S: SurfaceId = SurfaceId(1);
const T0: Duration = Duration::from_secs(1);

fn frame(k: u32) -> Duration {
    T0 + Duration::from_micros(16_667 * k as u64)
}

/// A 60×80 panel holding a 60 px tall `col` whose `on drop` takes `Pin`, with a
/// red, a green and a blue 20 px `drag: Pin` row; painted once.
struct Desk {
    r: Renderer,
    router: Router,
    buf: Buffer,
    col: NodeId,
    rows: [NodeId; 3],
}

impl Desk {
    fn new() -> Self {
        let mut b = Builder::default();
        let root = b.node(
            NodeKind::Panel,
            None,
            vec![
                (Prop::Width, num(60.0)),
                (Prop::Height, num(80.0)),
                (Prop::Bg, color("#1e1e2e")),
            ],
        );
        let pin = || PropValue::Keyword("Pin".into());
        let col = b.node(
            NodeKind::Col,
            Some(root),
            vec![
                (Prop::Height, num(60.0)),
                (Prop::Accepts, PropValue::List(vec![pin()])),
            ],
        );
        let row = |b: &mut Builder, c: &str| {
            b.node(
                NodeKind::Box,
                Some(col),
                vec![
                    (Prop::Size, num(20.0)),
                    (Prop::Bg, color(c)),
                    (Prop::Drag, pin()),
                ],
            )
        };
        let rows = [
            row(&mut b, "#ff0000"),
            row(&mut b, "#00ff00"),
            row(&mut b, "#0000ff"),
        ];
        let mut r = renderer();
        assert!(r.apply(b.diff).is_empty());
        r.attach_surface(S, root);
        let mut buf = Buffer::new(60, 80, Scale::ONE);
        buf.paint_at(&mut r, S, 0, T0);
        let mut router = Router::default();
        router.attached(S, root);
        Desk {
            r,
            router,
            buf,
            col,
            rows,
        }
    }

    fn input(&mut self, e: InputEvent) -> Vec<Intent> {
        self.router.handle(&e, &mut self.r)
    }

    fn motion(&mut self, y: f32) -> Vec<Intent> {
        self.input(InputEvent::PointerMotion {
            surface: S,
            position: LogicalPoint::new(10.0, y),
            time: 0,
        })
    }

    fn left(&mut self, y: f32, state: ButtonState) -> Vec<Intent> {
        self.input(InputEvent::PointerButton {
            surface: S,
            position: LogicalPoint::new(10.0, y),
            button: button::LEFT,
            state,
            time: 0,
        })
    }

    /// Presses the red row at y 10 and drags it to `y`.
    fn lift_red(&mut self, y: f32) {
        self.motion(10.0);
        self.left(10.0, ButtonState::Pressed);
        self.motion(y);
        assert_eq!(self.router.drag().map(|d| d.source), Some(self.rows[0]));
    }

    fn paint(&mut self, k: u32) {
        self.buf.paint_at(&mut self.r, S, 1, frame(k));
    }

    /// The first row of column 10 that is red.
    fn red_top(&self) -> Option<u32> {
        (0..80).find(|y| {
            let p = self.buf.px(10, *y);
            p[2] > 128 && p[1] < 64
        })
    }

    /// Paints until nothing moves, collecting the red row's top.
    fn settle(&mut self, mut k: u32) -> Vec<u32> {
        let mut tops = Vec::new();
        while self.r.wants_frame(S) {
            self.paint(k);
            tops.extend(self.red_top());
            k += 1;
            assert!(k < 400, "never settled");
        }
        tops
    }
}

/// The dragged row is drawn at the pointer, 25 px down from its box,
/// over the green and blue rows after it (it paints last), while its
/// box keeps its place in the layout.
#[test]
fn the_dragged_row_follows_the_pointer_above_its_siblings() {
    let mut d = Desk::new();
    d.lift_red(35.0);
    d.paint(1);
    assert_eq!(
        d.r.boxes(S).unwrap().rects[&d.rows[0]].y,
        0.0,
        "laid out in place"
    );
    assert_eq!(d.red_top(), Some(25));
    let red = |p: [u8; 4]| p[2] > 200 && p[1] < 40 && p[0] < 40;
    assert!(red(d.buf.px(10, 30)), "over the green row");
    assert!(red(d.buf.px(10, 42)), "over the blue row");
    assert!(!red(d.buf.px(10, 10)), "its box is bare meanwhile");
    assert_matches_ref("drag_lift", &d.buf, 2);
}

/// Let go where nothing takes it (below the column), the row springs
/// back into its box from where it was, with no drop and no click.
#[test]
fn a_row_dropped_nowhere_springs_back() {
    let mut d = Desk::new();
    d.lift_red(75.0);
    d.paint(1);
    assert_eq!(d.red_top(), Some(65));
    let out = d.left(75.0, ButtonState::Released);
    assert!(
        !out.iter().any(|i| matches!(i, Intent::Event { .. })),
        "{out:?}"
    );
    let tops = d.settle(2);
    assert!(tops.len() > 4, "it moves over frames: {tops:?}");
    assert!(tops[0] > 40, "starts from where it was let go: {tops:?}");
    assert!(tops.windows(2).all(|w| w[1] <= w[0]), "{tops:?}");
    assert_eq!(d.red_top(), Some(0));
}

/// Dropped past the blue row's middle, the drop lands at index 2 of the
/// column; logic moves the row there by key in the same turn, and it
/// springs from where it was let go (45 px down) into its new slot (40),
/// not from its old slot at the top.
#[test]
fn a_reorder_by_key_springs_from_the_drop_point() {
    let mut d = Desk::new();
    d.lift_red(55.0);
    d.paint(1);
    assert_eq!(d.red_top(), Some(45));
    let out = d.left(55.0, ButtonState::Released);
    assert_eq!(
        out.into_iter()
            .filter(|i| matches!(i, Intent::Event { .. }))
            .collect::<Vec<_>>(),
        [Intent::Event {
            node: d.col,
            event: NodeEvent::Drop {
                payload: DropPayload::Node(d.rows[0]),
                at: 2
            }
        }]
    );
    let mut diff = SceneDiff::new();
    diff.push(SceneOp::Move {
        id: d.rows[0],
        parent: Some(d.col),
        index: 2,
    });
    assert!(d.r.apply(diff).is_empty());
    d.paint(2);
    assert_eq!(d.r.boxes(S).unwrap().rects[&d.rows[0]].y, 40.0);
    let first = d.red_top().unwrap();
    assert!((40..=47).contains(&first), "near the drop point: {first}");
    d.settle(3);
    assert_eq!(d.red_top(), Some(40));
    // The green row slid up into the red row's old slot.
    let p = d.buf.px(10, 5);
    assert!(p[1] > 200 && p[2] < 40, "green on top: {p:?}");
}
