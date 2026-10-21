//! (report) `system.replay`: the glitch-report recording's switch, on by
//! default, written by the config or `strand set`, and kept across
//! restarts in the persist store (as `notifications.remember` is).

use std::time::{Duration, Instant};

use strand_core::Runtime;
use strand_services::{Builtin, Buses, Data, Services, Store, system};

fn services(rt: &Runtime) -> (Services, Builtin) {
    let s = Services::new(rt, Buses::none(), || {});
    let b = Builtin::register(&s, rt);
    (s, b)
}

/// Pump until `cond` holds (10 s at most).
fn until(rt: &Runtime, s: &Services, what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        s.pump(rt);
        rt.flush();
        if cond() {
            return;
        }
        assert!(Instant::now() < deadline, "never: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn replay_field() -> usize {
    system::System::FIELDS
        .iter()
        .position(|f| f.name == "replay")
        .unwrap()
}

/// A run of `system` keeping `replay` in `dir`: its boot value.
fn boot(dir: &std::path::Path) -> (Runtime, Services, Builtin) {
    system::set_config(Some(system::Config {
        state: Some(dir.to_path_buf()),
    }));
    let rt = Runtime::new();
    let (s, b) = services(&rt);
    b.system.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    (rt, s, b)
}

/// On with nothing kept; written false it is off at once and stays off
/// after a restart; written true it is on again after the next.
#[test]
fn replay_is_kept_across_restarts() {
    let dir = std::env::temp_dir().join(format!("strand-replay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    assert!(system::System::default().replay, "on by default");
    let f = system::System::FIELDS[replay_field()];
    assert!(f.rw && (f.ty)() == "bool");

    let (rt, s, b) = boot(&dir);
    assert_eq!(b.system.cells().replay.get_untracked(&rt), Ok(true));
    b.system
        .dynamic()
        .write(&rt, replay_field(), &[], Data::Bool(false))
        .unwrap();
    assert_eq!(b.system.cells().replay.get_untracked(&rt), Ok(false));
    until(&rt, &s, "the write answered", || {
        b.system.cells().replay.pending_writes(&rt) == 0
    });
    s.shutdown();

    let (rt, s, b) = boot(&dir);
    assert_eq!(
        b.system.cells().replay.get_untracked(&rt),
        Ok(false),
        "kept off across the restart"
    );
    b.system
        .dynamic()
        .write(&rt, replay_field(), &[], Data::Bool(true))
        .unwrap();
    until(&rt, &s, "the write answered", || {
        b.system.cells().replay.pending_writes(&rt) == 0
    });
    s.shutdown();

    let (rt, s, b) = boot(&dir);
    assert_eq!(b.system.cells().replay.get_untracked(&rt), Ok(true));
    s.shutdown();
    system::set_config(None);
    let _ = std::fs::remove_dir_all(&dir);
}
