//! The session lock on a real compositor: headless sway 1.9 **inside the
//! lock VM only** (scripts/container/lockvm.sh, scenario
//! `scripts/lockvm/scenarios/session_lock.sh`). Taking a session lock
//! anywhere else (the laptop's session, a container's compositor) is
//! never done: every test here skips unless it runs in the VM guest
//! (`STRAND_LOCK_VM=1`, hostname `lockvm`, PID 1 the VM's init).
//!
//! Each test starts its own sway and a "desktop" client whose bar is
//! painted red; a lock is proved by grim showing no red where the bar
//! is, and by the lock's own colours: the content (blue) on the first
//! output, the solid (green) on every other output, hotplugged ones too.
//! Unlocking takes an `UnlockToken`, minted here by `strand_auth::Client`
//! from a fake helper that accepts (PAM itself is strand-auth's and the
//! scenario's business).

mod common;

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use common::*;
use strand_auth::{Client, Password, UnlockToken, Verdict};
use strand_scene::{
    Color, Damage, InputEvent, LogicalRect, NodeId, NodeKind, PaintTarget, Painter, PropValue,
    Size, SurfaceChange, SurfaceId, SurfaceSpec,
};
use strand_surface::{Config, LockState, Monitor, SurfaceHost, SurfaceManager};

const LOCK: NodeId = NodeId::new(7, 0);
const GREEN: [u8; 3] = [0, 255, 0];

/// In the lock VM's guest, and asked to run there.
fn in_lock_vm(test: &str) -> bool {
    let asked = std::env::var_os("STRAND_LOCK_VM").is_some_and(|v| v == "1");
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    let init = std::fs::read("/proc/1/cmdline").unwrap_or_default();
    let vm = host.trim() == "lockvm" && init.windows(11).any(|w| w == b"lockvm-init");
    if !(asked && vm) {
        eprintln!(
            "skipping {test}: session-lock tests run only inside the lock VM \
             (scripts/container/lockvm.sh), never against another compositor"
        );
        return false;
    }
    true
}

/// A lock host: paints every surface plain `fill`, records the lock's
/// states, attachments and input.
#[derive(Default)]
struct LockHost {
    fill: [u8; 3],
    sizes: HashMap<SurfaceId, Size>,
    painted: HashSet<SurfaceId>,
    locks: Vec<LockState>,
    attached: Vec<(SurfaceId, NodeId)>,
    detached: Vec<SurfaceId>,
    input: Vec<InputEvent>,
}

impl Painter for LockHost {
    fn paint(&mut self, surface: SurfaceId, target: &mut PaintTarget<'_>) -> Damage {
        if self.sizes.get(&surface) == Some(&target.size) && self.painted.contains(&surface) {
            return Damage::new();
        }
        self.sizes.insert(surface, target.size);
        self.painted.insert(surface);
        let [r, g, b] = self.fill;
        for px in target.pixels.chunks_exact_mut(4) {
            px.copy_from_slice(&[b, g, r, 0xff]);
        }
        Damage::full(target.size)
    }

    fn wants_frame(&self, surface: SurfaceId) -> bool {
        !self.painted.contains(&surface)
    }
}

impl SurfaceHost for LockHost {
    fn surface_attached(&mut self, surface: SurfaceId, node: NodeId, _: Option<&Monitor>) {
        self.attached.push((surface, node));
    }

    fn surface_detached(&mut self, surface: SurfaceId) {
        self.detached.push(surface);
        self.painted.remove(&surface);
        self.sizes.remove(&surface);
    }

    fn surface_configured(&mut self, surface: SurfaceId, _: Size, _: strand_scene::Scale) {
        self.painted.remove(&surface);
    }

    fn input(&mut self, event: &InputEvent) {
        self.input.push(event.clone());
    }

    fn lock_changed(&mut self, state: LockState) {
        self.locks.push(state);
    }
}

fn lock_spec(open: bool) -> SurfaceSpec {
    let props: HashMap<strand_scene::Prop, PropValue> =
        [(strand_scene::Prop::Open, PropValue::Bool(open))]
            .into_iter()
            .collect();
    SurfaceSpec::resolve(NodeKind::Lock, |p| props.get(&p))
}

/// A lock client that may lock (`enable_session_lock`).
fn locker(sway: &Sway) -> SurfaceManager<LockHost> {
    let mut mgr = unwired_locker(sway);
    mgr.state_mut().enable_session_lock();
    mgr
}

/// A lock client as a build that routes no token to `unlock` has it.
fn unwired_locker(sway: &Sway) -> SurfaceManager<LockHost> {
    let host = LockHost {
        fill: BLUE,
        ..LockHost::default()
    };
    let mut mgr = SurfaceManager::with_connection(sway.connect(), host, Config::default())
        .expect("the lock client connects");
    mgr.state_mut()
        .set_lock_color(Color::new(0.0, 1.0, 0.0, 1.0));
    mgr
}

/// The desktop: a bar on every output, painted red.
fn desktop(sway: &Sway, outputs: usize) -> SurfaceManager<TestHost> {
    let mut host = TestHost::default();
    host.set_square(Some(LogicalRect::new(0.0, 0.0, 4000.0, 100.0)));
    let mut mgr = SurfaceManager::with_connection(sway.connect(), host, Config::default())
        .expect("the desktop connects");
    mgr.state_mut()
        .apply_surface_change(BAR, SurfaceChange::Created(bar_spec("Top", 36.0)));
    wait_for_bars(&mut mgr, outputs);
    pump(&mut mgr, Duration::from_millis(200));
    mgr
}

fn wait_lock(mgr: &mut SurfaceManager<LockHost>, state: LockState) {
    let ok = mgr
        .dispatch_until(WAIT, |s| s.host().locks.contains(&state))
        .unwrap();
    assert!(ok, "never {state:?}: {:?}", mgr.state().host().locks);
}

/// Pumps both clients for `d`.
fn settle(lock: &mut SurfaceManager<LockHost>, desk: &mut SurfaceManager<TestHost>, d: Duration) {
    let end = std::time::Instant::now() + d;
    while std::time::Instant::now() < end {
        let _ = lock.dispatch(Some(Duration::from_millis(10)));
        let _ = desk.dispatch(Some(Duration::from_millis(10)));
    }
}

/// The colour where the bar is, and in the middle, of `output`.
fn shot(sway: &Sway, output: &str) -> [[u8; 3]; 2] {
    let img = sway.grim(output);
    [img.rgb(10, 10), img.rgb(img.width / 2, img.height / 2)]
}

/// A token, as the PAM helper's success mints it: a fake helper that
/// accepts every password.
fn token() -> UnlockToken {
    let dir = std::env::temp_dir().join(format!("strand-lock-helper-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path: PathBuf = dir.join("accept");
    std::fs::write(
        &path,
        "#!/bin/sh\nprintf '\\003\\000\\000\\000\\001\\001\\000'; sleep 0.2; \
         printf '\\002\\000\\000\\000\\003\\000'; sleep 5\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    fn no_hook() {}
    let mut c = Client::new(path, no_hook).with_timeout(Duration::from_secs(5));
    match c.submit(Password::from("x".to_string())) {
        Verdict::Unlocked(t) => t,
        v => panic!("the fake helper did not accept: {v:?}"),
    }
}

#[test]
fn locks_every_output_including_hotplug() {
    let test = "locks_every_output_including_hotplug";
    if !in_lock_vm(test) {
        return;
    }
    let Some(sway) = Sway::start(test) else {
        return;
    };
    let second = sway.create_output();
    let mut desk = desktop(&sway, 2);
    for o in ["HEADLESS-1", second.as_str()] {
        assert_eq!(
            shot(&sway, o)[0],
            RED,
            "{o}: the desktop shows before the lock"
        );
    }

    let mut lock = locker(&sway);
    lock.state_mut()
        .apply_surface_change(LOCK, SurfaceChange::Created(lock_spec(true)));
    wait_lock(&mut lock, LockState::Locked);
    assert!(lock.state().is_locked());
    let content = lock.state().lock_content().expect("a content surface");
    assert!(lock.state().host().attached.contains(&(content, LOCK)));
    let painted = lock
        .dispatch_until(WAIT, |s| {
            s.surface(content).is_some_and(|i| i.stats.commits > 0)
        })
        .unwrap();
    assert!(painted, "the content is painted");
    settle(&mut lock, &mut desk, Duration::from_millis(300));
    assert_eq!(
        shot(&sway, "HEADLESS-1"),
        [BLUE, BLUE],
        "content on the first output"
    );
    assert_eq!(
        shot(&sway, &second),
        [GREEN, GREEN],
        "the solid on the second"
    );
    assert_eq!(
        lock.state().lock_solid_outputs(),
        std::slice::from_ref(&second)
    );
    // The solid is one single-pixel buffer the viewporter stretches
    // (design.md: "Scrims and lock backgrounds use single-pixel
    // buffers"); sway 1.9 offers both protocols.
    assert!(lock.state().compositor_caps().single_pixel_buffer);
    assert_eq!(
        lock.state().lock_solid_is_single_pixel(&second),
        Some(true),
        "the solid on {second} is a single pixel"
    );

    // An output plugged in while locked gets a solid too.
    let third = sway.create_output();
    let ok = lock
        .dispatch_until(WAIT, |s| s.lock_solid_outputs().contains(&third))
        .unwrap();
    assert!(ok, "the hotplugged output gets a lock surface");
    settle(&mut lock, &mut desk, Duration::from_millis(500));
    assert_eq!(shot(&sway, &third), [GREEN, GREEN], "{third}");
    assert_eq!(lock.state().lock_solid_is_single_pixel(&third), Some(true));

    // Only a token unlocks; then the desktop shows again.
    assert!(lock.state_mut().unlock(token()));
    assert_eq!(lock.state().host().locks.last(), Some(&LockState::Unlocked));
    assert!(!lock.state().is_locked());
    assert_eq!(lock.state().lock_content(), None);
    settle(&mut lock, &mut desk, Duration::from_millis(500));
    for o in ["HEADLESS-1", second.as_str()] {
        assert_eq!(shot(&sway, o)[0], RED, "{o}: the desktop after the unlock");
    }
    // Still open, the spec does not lock again until it closes and opens.
    settle(&mut lock, &mut desk, Duration::from_millis(200));
    assert!(!lock.state().lock_active());
    lock.state_mut().apply_surface_change(
        LOCK,
        SurfaceChange::Updated {
            spec: lock_spec(false),
            recreate: false,
        },
    );
    lock.state_mut().apply_surface_change(
        LOCK,
        SurfaceChange::Updated {
            spec: lock_spec(true),
            recreate: false,
        },
    );
    let n = lock.state().host().locks.len();
    let ok = lock
        .dispatch_until(WAIT, |s| s.host().locks[n..].contains(&LockState::Locked))
        .unwrap();
    assert!(ok, "reopened, it locks again");
    assert!(lock.state_mut().unlock(token()));
}

#[test]
fn finished_is_reported_and_not_shown() {
    let test = "finished_is_reported_and_not_shown";
    if !in_lock_vm(test) {
        return;
    }
    let Some(sway) = Sway::start(test) else {
        return;
    };
    let mut desk = desktop(&sway, 1);
    let mut first = locker(&sway);
    first
        .state_mut()
        .apply_surface_change(LOCK, SurfaceChange::Created(lock_spec(true)));
    wait_lock(&mut first, LockState::Locked);

    // A second locker is refused while the first holds the lock.
    let mut second = locker(&sway);
    second
        .state_mut()
        .apply_surface_change(LOCK, SurfaceChange::Created(lock_spec(true)));
    wait_lock(&mut second, LockState::Finished);
    assert_eq!(second.state().host().locks, [LockState::Finished]);
    assert!(!second.state().is_locked());
    assert!(
        !second.state().lock_active(),
        "a refused lock counts as not shown"
    );
    assert_eq!(second.state().lock_content(), None);
    // It does not ask again while its spec stays open.
    pump_lock(&mut second, Duration::from_millis(300));
    assert_eq!(second.state().host().locks, [LockState::Finished]);

    // The first still holds the session.
    settle(&mut first, &mut desk, Duration::from_millis(300));
    assert!(first.state().is_locked());
    assert_eq!(shot(&sway, "HEADLESS-1"), [BLUE, BLUE]);
    assert!(first.state_mut().unlock(token()));
}

fn pump_lock(mgr: &mut SurfaceManager<LockHost>, d: Duration) {
    let _ = mgr.dispatch_until(d, |_| false).unwrap();
}

/// Fail closed: closing or removing the lock's spec, and the client
/// going away, never unlock; a new client takes the lock over, and only
/// its token unlocks.
#[test]
fn only_a_token_unlocks_whatever_the_shell_does() {
    let test = "only_a_token_unlocks_whatever_the_shell_does";
    if !in_lock_vm(test) {
        return;
    }
    let Some(sway) = Sway::start(test) else {
        return;
    };
    let mut desk = desktop(&sway, 1);
    let mut lock = locker(&sway);
    lock.state_mut()
        .apply_surface_change(LOCK, SurfaceChange::Created(lock_spec(true)));
    wait_lock(&mut lock, LockState::Locked);
    let content = lock.state().lock_content().unwrap();

    // `open: false` (a config write) leaves it locked.
    lock.state_mut().apply_surface_change(
        LOCK,
        SurfaceChange::Updated {
            spec: lock_spec(false),
            recreate: false,
        },
    );
    settle(&mut lock, &mut desk, Duration::from_millis(300));
    assert!(lock.state().is_locked());
    assert_ne!(shot(&sway, "HEADLESS-1")[0], RED);

    // The spec going away (logic gone, a reload) leaves it locked, with
    // the content surface kept for the built-in fallback.
    lock.state_mut()
        .apply_surface_change(LOCK, SurfaceChange::Removed);
    settle(&mut lock, &mut desk, Duration::from_millis(300));
    assert!(lock.state().is_locked());
    assert_eq!(lock.state().lock_content(), Some(content));
    assert!(!lock.state().host().detached.contains(&content));
    assert_ne!(shot(&sway, "HEADLESS-1")[0], RED);

    // The client goes away: the compositor keeps the session locked.
    drop(lock);
    pump(&mut desk, Duration::from_millis(500));
    let [bar, middle] = shot(&sway, "HEADLESS-1");
    assert_ne!(bar, RED, "the desktop shows after the locker died");
    assert_ne!(middle, RED);

    // A new client takes the lock over; its token unlocks.
    let mut again = locker(&sway);
    again
        .state_mut()
        .apply_surface_change(LOCK, SurfaceChange::Created(lock_spec(true)));
    wait_lock(&mut again, LockState::Locked);
    settle(&mut again, &mut desk, Duration::from_millis(300));
    assert_eq!(shot(&sway, "HEADLESS-1"), [BLUE, BLUE]);
    assert!(again.state_mut().unlock(token()));
    settle(&mut again, &mut desk, Duration::from_millis(500));
    assert_eq!(shot(&sway, "HEADLESS-1")[0], RED);
}

/// Keys typed while locked reach the content surface (a virtual
/// keyboard), and a lock with no spec (`State::lock`, the binary's
/// fallback when no lock is compiled) still gets a content surface.
/// A virtual keyboard on sway's seat with one key, `a` (evdev 30).
struct VirtualKeyboard {
    queue: wayland_client::EventQueue<Kbd>,
    keyboard: wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
    time: u32,
    _conn: wayland_client::Connection,
}

struct Kbd;

mod kbd {
    use super::Kbd;
    use wayland_client::globals::GlobalListContents;
    use wayland_client::protocol::{wl_registry, wl_seat};
    use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
    use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
        zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
        zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
    };

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Kbd {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    delegate_noop!(Kbd: ignore ZwpVirtualKeyboardManagerV1);
    delegate_noop!(Kbd: ignore ZwpVirtualKeyboardV1);
    delegate_noop!(Kbd: ignore wl_seat::WlSeat);
}

impl VirtualKeyboard {
    fn new(sway: &Sway) -> VirtualKeyboard {
        use std::io::Write as _;
        use std::os::fd::AsFd;
        use wayland_client::globals::registry_queue_init;
        use wayland_client::protocol::wl_seat;
        use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
        let conn = sway.connect();
        let (globals, mut queue) = registry_queue_init::<Kbd>(&conn).unwrap();
        let qh = queue.handle();
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ()).unwrap();
        let manager: ZwpVirtualKeyboardManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
        let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());
        // One key, `a` (keycode 38 = evdev 30 + 8).
        let keymap = "xkb_keymap {\n\
            xkb_keycodes \"strand\" { minimum = 8; maximum = 255; <AC01> = 38; };\n\
            xkb_types \"strand\" { type \"ONE_LEVEL\" { modifiers = none; level_name[Level1] = \"Any\"; }; };\n\
            xkb_compatibility \"strand\" { };\n\
            xkb_symbols \"strand\" { key <AC01> { [ a ] }; };\n\
            };\n";
        let path = std::env::temp_dir().join(format!("strand-lock-keymap-{}", std::process::id()));
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(keymap.as_bytes()).unwrap();
        file.write_all(&[0]).unwrap();
        file.flush().unwrap();
        let file = std::fs::File::open(&path).unwrap();
        keyboard.keymap(1, file.as_fd(), keymap.len() as u32 + 1);
        queue.roundtrip(&mut Kbd).unwrap();
        let _ = std::fs::remove_file(&path);
        VirtualKeyboard {
            queue,
            keyboard,
            time: 0,
            _conn: conn,
        }
    }

    /// Presses and releases `a`.
    fn tap(&mut self) {
        use wayland_client::protocol::wl_keyboard;
        self.time += 1;
        self.keyboard
            .key(self.time, 30, wl_keyboard::KeyState::Pressed.into());
        self.time += 1;
        self.keyboard
            .key(self.time, 30, wl_keyboard::KeyState::Released.into());
        self.queue.roundtrip(&mut Kbd).unwrap();
    }
}

/// Waits until a key reached `surface` after the first `from` input
/// events; whether one did.
fn key_reached(mgr: &mut SurfaceManager<LockHost>, from: usize, surface: SurfaceId) -> bool {
    mgr.dispatch_until(WAIT, |s| {
        s.host().input[from..]
            .iter()
            .any(|e| matches!(e, InputEvent::Key { surface: to, .. } if *to == surface))
    })
    .unwrap()
}

#[test]
fn keys_reach_the_lock_and_a_lock_without_a_spec_has_content() {
    let test = "keys_reach_the_lock_and_a_lock_without_a_spec_has_content";
    if !in_lock_vm(test) {
        return;
    }
    let Some(sway) = Sway::start(test) else {
        return;
    };
    let mut desk = desktop(&sway, 1);
    let mut lock = locker(&sway);
    let _input = lock.take_input().unwrap();
    lock.state_mut()
        .lock()
        .expect("sway offers ext-session-lock");
    wait_lock(&mut lock, LockState::Locked);
    let content = lock.state().lock_content().expect("content without a spec");
    assert!(
        lock.state()
            .host()
            .attached
            .contains(&(content, strand_surface::LOCK_FALLBACK_NODE))
    );
    let mut keyboard = VirtualKeyboard::new(&sway);
    settle(&mut lock, &mut desk, Duration::from_millis(300));
    keyboard.tap();
    assert!(
        key_reached(&mut lock, 0, content),
        "the key reached the lock content: {:?}",
        lock.state().host().input
    );
    assert!(lock.state_mut().unlock(token()));
}

/// (m4-audit) Keys follow the content when it moves to another output
/// (the focused monitor changes) while the compositor's keyboard focus
/// stays on a solid that is not replaced: no new enter comes, yet the
/// keys reach the new content, as a solid's focus counts as the
/// content's. The content visits the three outputs in an order that
/// leaves the focus on a surviving solid in at least one move.
#[test]
fn keys_follow_the_lock_content_to_another_output() {
    let test = "keys_follow_the_lock_content_to_another_output";
    if !in_lock_vm(test) {
        return;
    }
    let Some(sway) = Sway::start(test) else {
        return;
    };
    let second = sway.create_output();
    let third = sway.create_output();
    let mut desk = desktop(&sway, 3);
    let mut lock = locker(&sway);
    let _input = lock.take_input().unwrap();
    lock.state_mut()
        .lock()
        .expect("sway offers ext-session-lock");
    wait_lock(&mut lock, LockState::Locked);
    let mut keyboard = VirtualKeyboard::new(&sway);
    settle(&mut lock, &mut desk, Duration::from_millis(300));
    let first = lock.state().lock_content().expect("a content surface");
    keyboard.tap();
    assert!(
        key_reached(&mut lock, 0, first),
        "{:?}",
        lock.state().host().input
    );
    // sway refocuses a replaced lock surface's focus to the first
    // mapped one in output order; this order leaves the focus on a solid
    // that survives a move whichever surface it starts on.
    let outputs = [
        third.as_str(),
        second.as_str(),
        "HEADLESS-1",
        third.as_str(),
        second.as_str(),
    ];
    for (round, target) in outputs.into_iter().enumerate() {
        let monitor = lock
            .state()
            .monitors()
            .into_iter()
            .find(|m| m.connector.as_deref() == Some(target))
            .unwrap();
        lock.state_mut().set_focused_monitor(Some(monitor.id));
        let ok = lock
            .dispatch_until(WAIT, |s| {
                let solids = s.lock_solid_outputs();
                solids.len() == 2
                    && !solids.iter().any(|o| o == target)
                    && s.lock_content()
                        .and_then(|c| s.surface(c))
                        .is_some_and(|i| i.stats.commits > 0)
            })
            .unwrap();
        assert!(ok, "round {round}: the content never moved to {target}");
        settle(&mut lock, &mut desk, Duration::from_millis(300));
        let content = lock.state().lock_content().unwrap();
        let from = lock.state().host().input.len();
        keyboard.tap();
        assert!(
            key_reached(&mut lock, from, content),
            "round {round}: no key reached the content on {target} (focus {:?}): {:?}",
            lock.state().keyboard_focus(),
            &lock.state().host().input[from..]
        );
    }
    assert!(lock.state_mut().unlock(token()));
}

/// Nothing locks before `enable_session_lock`: an open `lock` spec is
/// only a warning and `lock()` is refused, so a build that cannot unlock
/// never locks. Enabling it then takes the lock the open spec asked for.
#[test]
fn nothing_locks_until_the_session_lock_is_enabled() {
    let test = "nothing_locks_until_the_session_lock_is_enabled";
    if !in_lock_vm(test) {
        return;
    }
    let Some(sway) = Sway::start(test) else {
        return;
    };
    let mut desk = desktop(&sway, 1);
    let mut lock = unwired_locker(&sway);
    assert!(!lock.state().session_lock_enabled());
    lock.state_mut()
        .apply_surface_change(LOCK, SurfaceChange::Created(lock_spec(true)));
    assert_eq!(
        lock.state_mut().lock(),
        Err(strand_surface::LockError::NotEnabled)
    );
    settle(&mut lock, &mut desk, Duration::from_millis(500));
    assert!(!lock.state().lock_active());
    assert_eq!(lock.state().lock_content(), None);
    assert!(lock.state().host().locks.is_empty());
    assert_eq!(shot(&sway, "HEADLESS-1")[0], RED, "the desktop still shows");

    lock.state_mut().enable_session_lock();
    wait_lock(&mut lock, LockState::Locked);
    settle(&mut lock, &mut desk, Duration::from_millis(300));
    assert_eq!(shot(&sway, "HEADLESS-1"), [BLUE, BLUE]);
    assert!(lock.state_mut().unlock(token()));
}

/// A token that arrives before `locked` is dispatched (the password
/// check runs on another thread while the lock comes up) is kept and
/// unlocks once `locked` arrives: the pending lock is never destroyed,
/// which is a protocol error once `locked` is on the wire and would end
/// the connection with the session locked.
#[test]
fn a_token_before_locked_unlocks_once_locked() {
    let test = "a_token_before_locked_unlocks_once_locked";
    if !in_lock_vm(test) {
        return;
    }
    let Some(sway) = Sway::start(test) else {
        return;
    };
    let mut desk = desktop(&sway, 1);
    let token = token();
    let conn = sway.connect();
    let host = LockHost {
        fill: BLUE,
        ..LockHost::default()
    };
    let mut lock = SurfaceManager::with_connection(conn.clone(), host, Config::default())
        .expect("the lock client connects");
    lock.state_mut().enable_session_lock();
    lock.state_mut()
        .apply_surface_change(LOCK, SurfaceChange::Created(lock_spec(true)));
    // The lock request reaches sway, and its answer waits unread.
    conn.flush().unwrap();
    pump(&mut desk, Duration::from_millis(1500));
    assert!(lock.state().lock_active());
    assert!(!lock.state().is_locked(), "`locked` not dispatched yet");
    assert!(lock.state_mut().unlock(token), "the token is kept");
    assert!(lock.state().lock_active(), "still pending");
    wait_lock(&mut lock, LockState::Unlocked);
    assert_eq!(
        lock.state().host().locks,
        [LockState::Locked, LockState::Unlocked]
    );
    assert!(!lock.state().lock_active());
    settle(&mut lock, &mut desk, Duration::from_millis(500));
    lock.dispatch(Some(Duration::from_millis(100)))
        .expect("the lock client's connection survives");
    assert_eq!(shot(&sway, "HEADLESS-1")[0], RED, "the desktop shows again");
}

/// A reload that mounts the lock again sends the new node's `Created`
/// before the old node's `Removed`. The new node takes over: while
/// locked its content is attached to the new node (not left on the gone
/// one, which would show only the fallback); after an unlock with `open`
/// still true the remount does not lock again, and only closing and
/// opening the new node does.
#[test]
fn a_remounted_lock_takes_over() {
    let test = "a_remounted_lock_takes_over";
    if !in_lock_vm(test) {
        return;
    }
    let Some(sway) = Sway::start(test) else {
        return;
    };
    const AGAIN: NodeId = NodeId::new(8, 0);
    const THIRD: NodeId = NodeId::new(9, 0);
    let mut desk = desktop(&sway, 1);
    let mut lock = locker(&sway);
    lock.state_mut()
        .apply_surface_change(LOCK, SurfaceChange::Created(lock_spec(true)));
    wait_lock(&mut lock, LockState::Locked);

    // Remounted while locked.
    lock.state_mut()
        .apply_surface_change(AGAIN, SurfaceChange::Created(lock_spec(true)));
    lock.state_mut()
        .apply_surface_change(LOCK, SurfaceChange::Removed);
    let content = lock.state().lock_content().expect("a content surface");
    assert!(
        lock.state().host().attached.contains(&(content, AGAIN)),
        "the content follows the new node: {:?}",
        lock.state().host().attached
    );
    settle(&mut lock, &mut desk, Duration::from_millis(300));
    assert!(lock.state().is_locked());
    assert_eq!(shot(&sway, "HEADLESS-1"), [BLUE, BLUE]);

    // Unlocked with `open` still true, then remounted: no new lock.
    assert!(lock.state_mut().unlock(token()));
    lock.state_mut()
        .apply_surface_change(THIRD, SurfaceChange::Created(lock_spec(true)));
    lock.state_mut()
        .apply_surface_change(AGAIN, SurfaceChange::Removed);
    settle(&mut lock, &mut desk, Duration::from_millis(500));
    assert!(
        !lock.state().lock_active(),
        "a remount is not a new request"
    );
    assert_eq!(shot(&sway, "HEADLESS-1")[0], RED);

    // Closed and opened: a new request.
    for open in [false, true] {
        lock.state_mut().apply_surface_change(
            THIRD,
            SurfaceChange::Updated {
                spec: lock_spec(open),
                recreate: false,
            },
        );
    }
    let n = lock.state().host().locks.len();
    let ok = lock
        .dispatch_until(WAIT, |s| s.host().locks[n..].contains(&LockState::Locked))
        .unwrap();
    assert!(ok, "reopened, it locks again");
    assert!(lock.state_mut().unlock(token()));
}

/// (M4) A lock surface is never lent to or handed off to the GPU thread
/// (docs/architecture.md, "Surface hand-off"): its handles are refused,
/// so a promoted lock is read back (bounded by `HUNG_AFTER`), and the
/// manager keeps committing it, so the lock's own commits never wait on
/// the GPU.
#[test]
fn a_lock_surface_is_never_handed_to_the_gpu() {
    let test = "a_lock_surface_is_never_handed_to_the_gpu";
    if !in_lock_vm(test) {
        return;
    }
    let Some(sway) = Sway::start(test) else {
        return;
    };
    let mut desk = desktop(&sway, 1);
    let mut lock = locker(&sway);
    lock.state_mut()
        .apply_surface_change(LOCK, SurfaceChange::Created(lock_spec(true)));
    wait_lock(&mut lock, LockState::Locked);
    let content = lock.state().lock_content().expect("a content surface");
    let painted = lock
        .dispatch_until(WAIT, |s| {
            s.surface(content).is_some_and(|i| i.stats.commits > 0)
        })
        .unwrap();
    assert!(painted, "the content is painted");
    #[cfg(feature = "gpu")]
    assert!(
        lock.state_mut().raw_handles(content).is_none(),
        "a lock surface's handles are never lent"
    );
    assert!(
        !lock.state_mut().hand_off(content),
        "a lock surface is never handed off"
    );
    assert!(!lock.state().is_handed_off(content));
    settle(&mut lock, &mut desk, Duration::from_millis(300));
    assert_eq!(
        shot(&sway, "HEADLESS-1"),
        [BLUE, BLUE],
        "the manager still draws the lock"
    );
    assert!(lock.state_mut().unlock(token()));
}
