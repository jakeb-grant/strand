//! The idle budget: with nothing changing in the compositor, the running
//! service's threads (its runtime, the protocol thread, async-io's
//! reactor under swayipc-async) are not woken at all. Alone in its binary
//! so no other test's threads share these names.

mod common;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use common::Sway;
use strand_services::wm::{self, Backend, WaylandTarget, WmChange, WmConfig};

/// Context switches (voluntary + involuntary) of this process's threads,
/// by thread id, for threads whose name is in `names`.
fn switches(names: &[&str]) -> HashMap<String, (String, u64)> {
    let mut out = HashMap::new();
    for task in std::fs::read_dir("/proc/self/task").unwrap().flatten() {
        let dir = task.path();
        let comm = std::fs::read_to_string(dir.join("comm")).unwrap_or_default();
        let comm = comm.trim().to_string();
        if !names.contains(&comm.as_str()) {
            continue;
        }
        let status = std::fs::read_to_string(dir.join("status")).unwrap_or_default();
        let n: u64 = status
            .lines()
            .filter(|l| l.contains("ctxt_switches"))
            .filter_map(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
            .sum();
        out.insert(task.file_name().to_string_lossy().into_owned(), (comm, n));
    }
    out
}

#[test]
fn an_idle_compositor_wakes_no_service_thread() {
    let Some(sway) = Sway::start("an_idle_compositor_wakes_no_service_thread") else {
        return;
    };
    let (tx, rx) = std::sync::mpsc::channel::<Vec<WmChange>>();
    let config = WmConfig {
        backend: Some(Backend::Sway {
            socket: sway.ipc.clone(),
        }),
        wayland: Some(WaylandTarget::Socket(sway.socket())),
        events: None,
    };
    // The service on its own current-thread runtime, as in Strand.
    let (_req_tx, req_rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("wm-idle".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(wm::run(
                config,
                move |b| {
                    let _ = tx.send(b);
                },
                req_rx,
            ));
        })
        .unwrap();
    // Boot, then let everything settle.
    let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(!first.is_empty());
    std::thread::sleep(Duration::from_millis(300));
    while rx.try_recv().is_ok() {}

    let names = ["wm-idle", "strand-toplevel", "async-io"];
    let before = switches(&names);
    let seen: Vec<&str> = before.values().map(|(n, _)| n.as_str()).collect();
    for n in names {
        assert!(seen.contains(&n), "thread {n} not found: {before:?}");
    }
    let start = Instant::now();
    std::thread::sleep(Duration::from_secs(2));
    let after = switches(&names);
    for (tid, (name, n)) in &before {
        let m = after.get(tid).map_or(*n, |(_, m)| *m);
        assert_eq!(
            m - n,
            0,
            "{name} ({tid}) woke {} times in {:?} of idle",
            m - n,
            start.elapsed()
        );
    }
    assert!(rx.try_recv().is_err(), "nothing was published while idle");

    // A real change still gets through (the threads were asleep, not dead).
    sway.msg(&["workspace", "5"]);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let batch = rx.recv_timeout(left).expect("the switch to 5 arrives");
        if batch
            .iter()
            .any(|c| matches!(c, WmChange::FocusedWorkspace(Some(w)) if w.name == "5"))
        {
            break;
        }
    }
}
