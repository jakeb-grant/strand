//! No-code services run through the registry as `strand run` runs them:
//! a `from listen` command printing faster than a frame wakes the logic
//! thread at most once a frame, its last line always shown.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use strand_core::Runtime;
use strand_services::custom::{self, Custom, FieldSpec, LISTEN_FLUSH, Source, Spec};
use strand_services::{Buses, Data, Services};

/// `while :; do echo "{\"t\": $i}"; done`: 20 000 changing lines as fast
/// as `sh` prints them. Each envelope the service sends wakes the logic
/// thread once; merged, the wakes are bounded by the frames the run
/// took, not by the lines, and the value shown is the last line's.
#[test]
fn a_fast_listen_command_wakes_the_logic_thread_once_a_frame() {
    const LINES: u32 = 20_000;
    let rt = Runtime::new();
    let wakes = Arc::new(AtomicU32::new(0));
    let w = wakes.clone();
    let services = Services::new(&rt, Buses::none(), move || {
        w.fetch_add(1, Ordering::SeqCst);
    });
    let client = services.register_as::<Custom>(&rt, "l");
    let script = format!(
        "i=0; while [ $i -lt {LINES} ]; do echo \"{{\\\"t\\\": $i}}\"; i=$((i+1)); done; \
         echo done >&2; sleep 30"
    );
    let id = custom::register(Spec {
        name: "l".into(),
        source: Source::Listen {
            command: vec!["sh".into(), "-c".into(), script],
        },
        fields: vec![FieldSpec {
            name: "t".into(),
            key: vec!["t".into()],
            rw: false,
        }],
    });
    client.seed(&rt, |s| *s = Custom::seeded(id, 1)).unwrap();
    let start = Instant::now();
    client.acquire(&rt);
    let last = Data::Int(i64::from(LINES - 1));
    let value = || {
        client
            .cells()
            .values
            .with_untracked(&rt, |v| v.get(&0i64).map(|v| v.value.clone()))
            .ok()
            .flatten()
    };
    let deadline = start + Duration::from_secs(60);
    loop {
        services.pump(&rt);
        rt.flush();
        if value().as_ref() == Some(&last) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "never read the last line: {:?}",
            value()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let took = start.elapsed();
    let woken = wakes.load(Ordering::SeqCst);
    // One wake a frame (plus the ready envelope and slack for timer
    // lateness), never one a line.
    let frames = (took.as_secs_f64() / LISTEN_FLUSH.as_secs_f64()).ceil() as u32;
    assert!(
        woken <= frames + 8 && woken < LINES / 4,
        "{woken} wakes for {LINES} lines in {took:?} ({frames} frames)"
    );
    client.release(&rt);
    services.shutdown();
    custom::forget(id);
}
