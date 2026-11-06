//! (lock-polish, decisions.md) A lock plays its way out before the
//! session unlocks (design.md, "Lock screen"). Logic closes the lock and
//! unmounts its content in one diff, as a popup's; render keeps the lock
//! open while what left plays its `exit`, and says so
//! (`SurfaceSpec::closing`), so strand-surface holds the unlock until the
//! spec closes. What left with no pose of its own stays drawn as it was
//! until then: the lock leaves as one screen, never empty.

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
/// exit ends, then closes, and what it kept unmounts.
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
    assert_eq!(r.tree().ghost_count(), 0, "what it kept unmounted");
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
