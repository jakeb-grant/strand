//! (lock-polish, decisions.md) A lock plays its way out before the
//! session unlocks (design.md, "Lock screen"). Logic closes the lock and
//! unmounts its content in one diff, as a popup's; render keeps the lock
//! open while what left plays its `exit`, and says so
//! (`SurfaceSpec::closing`), so strand-surface holds the unlock until the
//! spec closes. What left with no pose of its own stays drawn as it was
//! until then: the lock leaves as one screen, never empty. (lock-int)
//! Closed, the lock keeps its exit's end on screen until its surface goes
//! (the session unlocks): no frame after it draws the lock's background
//! alone.

mod common;

use std::time::Duration;

use common::*;
use strand_scene::*;

const LOCK: SurfaceId = SurfaceId(4);
const T0: Duration = Duration::from_secs(1);

fn frame(k: u32) -> Duration {
    T0 + Duration::from_micros(16_667 * u64::from(k))
}

/// A 320 × 200 lock in blue holding a green box over all of it with no
/// pose (as a still of the desktop would be) and over that, with
/// `card_exit`, a light card that fades out on `exit`; painted once at
/// `T0` (a lock stacks its content, centred). Returns the renderer, the
/// buffer and the lock, the box and the card.
fn lock(card_exit: bool) -> (strand_render::Renderer, Buffer, [NodeId; 3]) {
    lock_with(card_exit, false)
}

/// [`lock`], with `lock_exit` an `exit { opacity: 0 }` on the lock
/// itself.
fn lock_with(card_exit: bool, lock_exit: bool) -> (strand_render::Renderer, Buffer, [NodeId; 3]) {
    let mut b = Builder::default();
    let mut lock_props = vec![
        (Prop::Open, PropValue::Bool(true)),
        (
            Prop::TwoWay,
            PropValue::List(vec![PropValue::Keyword("open".into())]),
        ),
        (Prop::Bg, color("#2050d0")),
        (Prop::Align, PropValue::Keyword("center".into())),
    ];
    if lock_exit {
        lock_props.push((Prop::Exit, PropValue::Pose(vec![(Prop::Opacity, num(0.0))])));
    }
    let lock = b.node(NodeKind::Lock, None, lock_props);
    let still = b.node(
        NodeKind::Box,
        Some(lock),
        vec![
            (Prop::Width, num(320.0)),
            (Prop::Height, num(200.0)),
            (Prop::Bg, color("#00ff00")),
        ],
    );
    let mut props = vec![
        (Prop::Width, num(200.0)),
        (Prop::Height, num(100.0)),
        (Prop::Bg, color("#e0e0e0")),
    ];
    if card_exit {
        props.push((Prop::Exit, PropValue::Pose(vec![(Prop::Opacity, num(0.0))])));
    }
    let card = b.node(NodeKind::Box, Some(lock), props);
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.take_surface_changes();
    r.set_surface_bounds(LOCK, Some(LogicalSize::new(320.0, 200.0)));
    r.attach_surface(LOCK, lock);
    r.configure_surface(LOCK, Size::new(320, 200), Scale::ONE);
    let mut buf = Buffer::new(320, 200, Scale::ONE);
    assert!(!buf.paint_at(&mut r, LOCK, 0, T0).is_empty());
    r.take_surface_changes();
    (r, buf, [lock, still, card])
}

/// What logic sends when the password is accepted: the lock's content
/// unmounted and its `open` written false.
fn close(r: &mut strand_render::Renderer, lock: NodeId, content: &[NodeId]) {
    let mut d = SceneDiff::new();
    for id in content {
        d.push(SceneOp::Remove {
            id: *id,
            window: false,
        });
    }
    d.set(lock, Prop::Open, PropValue::Bool(false));
    assert!(r.apply(d).is_empty());
}

const GREEN: [u8; 4] = [0x00, 0xff, 0x00, 0xff];
const CARD: [u8; 4] = [0xe0, 0xe0, 0xe0, 0xff];

/// The card fades out while the box under it, which has no pose, stays
/// drawn;
/// the lock's spec stays open and says it is closing until the card's
/// exit ends, then closes; what it kept stays mounted while its surface
/// shows it (lock-int) and unmounts once that goes.
#[test]
fn a_lock_stays_open_while_its_content_plays_out() {
    let (mut r, mut buf, [lock, still, card]) = lock(true);
    // At rest: the card in the middle, the green box around it.
    assert_eq!(buf.px(10, 100), GREEN);
    assert_eq!(buf.px(160, 100), CARD);
    let spec = r.surface_spec(lock).unwrap();
    assert!(spec.open && !spec.closing);

    close(&mut r, lock, &[still, card]);
    let spec = r.surface_spec(lock).cloned().unwrap();
    assert!(spec.open, "held open while its exit plays");
    assert!(spec.closing, "and says why");
    assert!(
        r.take_surface_changes().iter().any(|(n, c)| *n == lock
            && matches!(c, SurfaceChange::Updated { spec, .. } if spec.open && spec.closing)),
        "strand-surface hears it is closing"
    );

    let mut cards = Vec::new();
    let mut k = 1;
    let mut mid = None;
    while r.surface_spec(lock).is_some_and(|s| s.open) {
        buf.paint_at(&mut r, LOCK, 1, frame(k));
        assert_eq!(buf.px(10, 100), GREEN, "frame {k}: the box stays drawn");
        cards.push(buf.px(160, 100));
        if mid.is_none() && buf.px(160, 100) != CARD && buf.px(160, 100) != GREEN {
            mid = Some(buf.clone());
        }
        k += 1;
        assert!(k < 200, "the lock never closed");
    }
    assert!(k > 4, "the exit plays over several frames ({k})");
    let mid = mid.expect("a frame with the card part way out");
    assert!(
        cards.windows(2).all(|w| w[1][0] <= w[0][0]),
        "the card fades frame by frame to the green under it: {cards:?}"
    );
    assert_eq!(*cards.last().unwrap(), GREEN, "gone by the last frame");
    assert_matches_ref("lock_exit_mid", &mid, 2);

    let spec = r.surface_spec(lock).cloned().unwrap();
    assert!(!spec.open && !spec.closing, "closed once it has played out");
    assert!(
        r.take_surface_changes().iter().any(|(n, c)| *n == lock
            && matches!(c, SurfaceChange::Updated { spec, .. } if !spec.open && !spec.closing)),
        "strand-surface hears it closed"
    );
    assert!(r.tree().ghost_count() > 0, "the still kept while shown");
    r.detach_surface(LOCK);
    r.update();
    assert_eq!(r.tree().ghost_count(), 0, "what it kept unmounted");
}

/// (lock-int) Closed once its exit has played, the lock keeps its last
/// frame until its surface goes: the frame after the close draws the
/// exit's end (the box with no pose, the card gone), and none after that
/// draws anything or wants to, so the compositor never shows the lock's
/// background alone in the moment before the unlock lands. Before, the
/// frame after the close unmounted the box and drew the lock's blue alone
/// (the owner's "blip to dark": a near-black `bg` under a still of the
/// desktop). Opened again, it draws anew.
#[test]
fn a_closed_lock_keeps_its_exits_end_until_it_goes() {
    // The lock's #2050d0, as the buffer holds it (BGRA).
    const BLUE: [u8; 4] = [0xd0, 0x50, 0x20, 0xff];
    let (mut r, mut buf, [lock, still, card]) = lock(true);
    close(&mut r, lock, &[still, card]);
    let mut k = 1;
    while r.surface_spec(lock).is_some_and(|s| s.open) {
        buf.paint_at(&mut r, LOCK, 1, frame(k));
        k += 1;
        assert!(k < 200, "the lock never closed");
    }
    assert_eq!(buf.px(10, 100), GREEN);
    assert_eq!(buf.px(160, 100), GREEN, "the card gone");
    // The frames after the close: each keeps the exit's end, whatever it
    // drew (a whole new buffer each time here, as a presented frame is).
    for j in 0..4 {
        let mut fresh = Buffer::new(320, 200, Scale::ONE);
        let wanted = r.wants_frame(LOCK);
        let d = fresh.paint_at(&mut r, LOCK, 0, frame(k + j));
        if d.is_empty() {
            assert!(j > 0, "the settled frame is drawn once");
            assert!(
                !wanted,
                "frame {j} after the close: wanted with nothing to draw"
            );
            continue;
        }
        assert_eq!(j, 0, "frame {j} after the close drew again: {d:?}");
        assert_eq!(
            fresh.px(10, 100),
            GREEN,
            "the frame after the close lost the box"
        );
        assert_ne!(fresh.px(10, 100), BLUE);
        assert_eq!(fresh.px(160, 100), GREEN, "the card stays gone");
    }
    assert!(!r.wants_frame(LOCK), "frozen until it goes");
    // Opened again (the lock asked for again): it draws anew.
    let again = NodeId::new(100, 0);
    let mut d = SceneDiff::new();
    d.create(again, NodeKind::Box, Some(lock), u32::MAX);
    d.set(again, Prop::Bg, color("#ff0000"))
        .set(again, Prop::Size, num(20.0));
    d.set(lock, Prop::Open, PropValue::Bool(true));
    assert!(r.apply(d).is_empty());
    assert!(r.wants_frame(LOCK), "opened again, it draws");
    let mut fresh = Buffer::new(320, 200, Scale::ONE);
    assert!(!fresh.paint_at(&mut r, LOCK, 0, frame(k + 10)).is_empty());
    assert_eq!(
        fresh.px(10, 100),
        BLUE,
        "the kept box gone for logic's content"
    );
}

/// A lock whose content has no exit closes in the diff that closes it:
/// nothing holds the unlock.
#[test]
fn a_lock_with_nothing_to_play_closes_at_once() {
    let (mut r, _buf, [lock, still, card]) = lock(false);
    close(&mut r, lock, &[still, card]);
    let spec = r.surface_spec(lock).cloned().unwrap();
    assert!(!spec.open && !spec.closing, "closed at once: {spec:?}");
    assert_eq!(r.tree().ghost_count(), 0, "nothing kept");
}

/// Opened again while it plays out (the lock asked for again), it is a
/// lock again: open and not closing, the kept content gone for logic's.
#[test]
fn a_lock_opened_again_as_it_leaves_is_not_closing() {
    let (mut r, mut buf, [lock, still, card]) = lock(true);
    close(&mut r, lock, &[still, card]);
    buf.paint_at(&mut r, LOCK, 1, frame(1));
    assert!(r.surface_spec(lock).unwrap().closing);
    let again = NodeId::new(100, 0);
    let mut d = SceneDiff::new();
    d.create(again, NodeKind::Box, Some(lock), u32::MAX);
    d.set(again, Prop::Bg, color("#ff0000"))
        .set(again, Prop::Size, num(20.0));
    d.set(lock, Prop::Open, PropValue::Bool(true));
    assert!(r.apply(d).is_empty());
    let spec = r.surface_spec(lock).cloned().unwrap();
    assert!(spec.open && !spec.closing, "a lock again: {spec:?}");
}

/// The lock's own `exit { }` carries all of it (design.md, "Lock
/// screen"): with an `exit { opacity: 0 }` on the lock and none on its
/// content, the whole lock fades as one, painted in its buffer (a lock
/// surface takes no compositor pose), the still and the card together;
/// the spec stays open and closing until the fade ends, then closes.
#[test]
fn the_locks_own_exit_carries_all_of_it() {
    let (mut r, mut buf, [lock, still, card]) = lock_with(false, true);
    assert_eq!(buf.px(10, 100), GREEN);
    assert_eq!(buf.px(160, 100), CARD);
    close(&mut r, lock, &[still, card]);
    let spec = r.surface_spec(lock).cloned().unwrap();
    assert!(
        spec.open && spec.closing,
        "held open for its exit: {spec:?}"
    );

    let mut alphas = Vec::new();
    let mut mid = None;
    let mut k = 1;
    while r.surface_spec(lock).is_some_and(|s| s.open) {
        buf.paint_at(&mut r, LOCK, 1, frame(k));
        let (g, c) = (buf.px(10, 100), buf.px(160, 100));
        assert!(
            g[3] == c[3] && g[1] >= g[0] && c[0] == c[1],
            "frame {k}: the still and the card fade as one: {g:?} {c:?}"
        );
        if mid.is_none() && (1..0xff).contains(&g[3]) {
            mid = Some(buf.clone());
        }
        alphas.push(g[3]);
        k += 1;
        assert!(k < 200, "the lock never closed");
    }
    assert!(k > 4, "the exit plays over several frames ({k})");
    assert!(
        alphas.windows(2).all(|w| w[1] <= w[0]),
        "the lock fades frame by frame: {alphas:?}"
    );
    let mid = mid.expect("a frame with the lock part way out");
    assert_matches_ref("lock_exit_root_mid", &mid, 2);
    let spec = r.surface_spec(lock).cloned().unwrap();
    assert!(!spec.open && !spec.closing, "closed once it has played out");
}

/// (lock-int) An `exit` nested under a node with none plays (the owner's
/// island, a `stack` with its poses in a full-screen `col` that has
/// none): its `enter` played, so its exit does too. The lock's content
/// is one column over all of it with no pose, holding the card, which
/// fades out on `exit`; the column stays drawn at rest (its own green),
/// the card fades frame by frame to it, and the lock is held open and
/// closing until it has.
#[test]
fn an_exit_under_a_node_with_none_plays() {
    let mut b = Builder::default();
    let lock = b.node(
        NodeKind::Lock,
        None,
        vec![
            (Prop::Open, PropValue::Bool(true)),
            (
                Prop::TwoWay,
                PropValue::List(vec![PropValue::Keyword("open".into())]),
            ),
            (Prop::Bg, color("#2050d0")),
        ],
    );
    let col = b.node(
        NodeKind::Col,
        Some(lock),
        vec![
            (Prop::Width, num(320.0)),
            (Prop::Height, num(200.0)),
            (Prop::Align, PropValue::Keyword("center".into())),
            (Prop::Bg, color("#00ff00")),
        ],
    );
    let stack = b.node(
        NodeKind::Stack,
        Some(col),
        vec![
            (Prop::Width, num(200.0)),
            (Prop::Height, num(100.0)),
            (Prop::Y, num(50.0)),
            (Prop::Exit, PropValue::Pose(vec![(Prop::Opacity, num(0.0))])),
        ],
    );
    b.node(
        NodeKind::Box,
        Some(stack),
        vec![
            (Prop::Width, num(200.0)),
            (Prop::Height, num(100.0)),
            (Prop::Bg, color("#e0e0e0")),
        ],
    );
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.take_surface_changes();
    r.set_surface_bounds(LOCK, Some(LogicalSize::new(320.0, 200.0)));
    r.attach_surface(LOCK, lock);
    r.configure_surface(LOCK, Size::new(320, 200), Scale::ONE);
    let mut buf = Buffer::new(320, 200, Scale::ONE);
    assert!(!buf.paint_at(&mut r, LOCK, 0, T0).is_empty());
    assert_eq!(buf.px(10, 100), GREEN);
    assert_eq!(buf.px(160, 100), CARD);

    close(&mut r, lock, &[col]);
    let spec = r.surface_spec(lock).cloned().unwrap();
    assert!(
        spec.open && spec.closing,
        "held open for the nested exit: {spec:?}"
    );
    let mut cards = Vec::new();
    let mut mid = None;
    let mut k = 1;
    while r.surface_spec(lock).is_some_and(|s| s.open) {
        buf.paint_at(&mut r, LOCK, 1, frame(k));
        assert_eq!(buf.px(10, 100), GREEN, "frame {k}: the column stays drawn");
        let c = buf.px(160, 100);
        if mid.is_none() && c != CARD && c != GREEN {
            mid = Some(buf.clone());
        }
        cards.push(c);
        k += 1;
        assert!(k < 200, "the lock never closed");
    }
    assert!(k > 4, "the exit plays over several frames ({k}): {cards:?}");
    assert!(
        cards.windows(2).all(|w| w[1][0] <= w[0][0]),
        "the card fades frame by frame to the column under it: {cards:?}"
    );
    assert_eq!(*cards.last().unwrap(), GREEN, "gone by the last frame");
    assert_matches_ref(
        "lock_exit_nested_mid",
        &mid.expect("a frame part way out"),
        2,
    );
    let spec = r.surface_spec(lock).cloned().unwrap();
    assert!(!spec.open && !spec.closing, "closed once it has played out");
    r.detach_surface(LOCK);
    r.update();
    assert_eq!(r.tree().ghost_count(), 0, "what it kept unmounted");
}
