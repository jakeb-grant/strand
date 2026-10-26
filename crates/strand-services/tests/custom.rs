//! No-code services run through the registry as `strand run` runs them:
//! a `from listen` command printing faster than a frame wakes the logic
//! thread at most once a frame, its last line always shown; (M6) a `from
//! dbus` declaration's actions call its object's methods; (services-round)
//! once a handler hears it, every line a `from listen` command prints is
//! one `line` event, none lost to the merge; (services-round) a `from
//! dir` declaration lists a directory's files and follows them as they
//! come, go, change and move, and the directory itself as it is created
//! and removed; a `from file` rewritten in place (truncated, then
//! written) never shows the empty file in between.

mod support;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use strand_core::Runtime;
use strand_services::custom::{
    self, ActionSpec, Custom, CustomAction, FieldSpec, LISTEN_FLUSH, Source, Spec,
};
use strand_services::testing::{DbusMock, PrivateBus};
use strand_services::{Applied, Buses, Data, Services};
use support::{connect, mock_calls, tokio, until};

/// (M6) `action hold(profile: text, reason: text, app: text) =
/// HoldProfile` and `action release(cookie: int) = ReleaseProfile` on
/// dbusmock's power-profiles-daemon: each call reaches the method with
/// its arguments as the method's D-Bus types (three strings, one with
/// spaces kept whole; an `int` sent as the `u` it takes), and an action
/// naming no method of the object is refused there without stopping the
/// service.
#[test]
fn a_dbus_declarations_actions_call_its_methods() {
    const PPD: &str = "net.hadess.PowerProfiles";
    const PATH: &str = "/net/hadess/PowerProfiles";
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let Some(_ppd) = DbusMock::start(&bus, "power_profiles_daemon", true, None, PPD) else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let rt = Runtime::new();
    let services = Services::new(&rt, bus.buses(), || {});
    let client = services.register_as::<Custom>(&rt, "ppd");
    let id = custom::register(Spec {
        name: "ppd".into(),
        source: Source::Dbus {
            system: true,
            name: PPD.into(),
            path: None,
        },
        fields: vec![FieldSpec {
            name: "profile".into(),
            key: vec!["ActiveProfile".into()],
            rw: false,
        }],
        actions: vec![
            ActionSpec {
                name: "hold".into(),
                method: "HoldProfile".into(),
            },
            ActionSpec {
                name: "release".into(),
                method: "ReleaseProfile".into(),
            },
            ActionSpec {
                name: "gone".into(),
                method: "NoSuchMethod".into(),
            },
        ],
    });
    client.seed(&rt, |s| *s = Custom::seeded(id, 1)).unwrap();
    client.acquire(&rt);
    let profile = || {
        client
            .cells()
            .values
            .with_untracked(&rt, |v| v.get(&0i64).map(|v| v.value.clone()))
            .ok()
            .flatten()
    };
    until(&rt, &services, "the profile is read", || {
        profile() == Some(Data::Text("balanced".into()))
    });
    let act = |index: usize, args: Vec<Data>| {
        client.act(&rt, CustomAction { index, args }).unwrap();
        services.pump(&rt);
    };
    act(2, Vec::new());
    act(
        0,
        vec![
            Data::Text("performance".into()),
            Data::Text("a reason with spaces".into()),
            Data::Text("strand".into()),
        ],
    );
    act(1, vec![Data::Int(7)]);
    let texts = |args: &[zbus::zvariant::OwnedValue]| -> Vec<String> {
        args.iter()
            .map(|v| String::try_from(v.try_clone().unwrap()).unwrap_or_default())
            .collect()
    };
    until(&rt, &services, "both methods were called", || {
        mock_calls(&tokio, &conn, PPD, PATH, "ReleaseProfile").len() == 1
    });
    let held = mock_calls(&tokio, &conn, PPD, PATH, "HoldProfile");
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(
        texts(&held[0]),
        ["performance", "a reason with spaces", "strand"]
    );
    let released = mock_calls(&tokio, &conn, PPD, PATH, "ReleaseProfile");
    assert_eq!(
        u32::try_from(released[0][0].try_clone().unwrap()).unwrap(),
        7
    );
    // The refused action left the service running: it still reads.
    assert!(client.running(), "the service stopped");
    client.release(&rt);
    services.shutdown();
    custom::forget(id);
}

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
        actions: Vec::new(),
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

/// (services-round) `on svc.line(doc)`: a command printing 2 000 lines as
/// fast as `sh` can, with a field the even lines leave out. Once heard,
/// every line arrives as one `line` event, in order, its fields from that
/// line alone (null where it has none), while the fields themselves stay
/// merged and the last line's. A declaration nobody hears sends no event.
#[test]
fn a_heard_listen_command_sends_every_line() {
    const LINES: i64 = 2_000;
    let rt = Runtime::new();
    let services = Services::new(&rt, Buses::none(), || {});
    let script = format!(
        "i=0; while [ $i -lt {LINES} ]; do \
         if [ $((i % 2)) -eq 1 ]; then echo \"{{\\\"t\\\": $i, \\\"odd\\\": true}}\"; \
         else echo \"{{\\\"t\\\": $i}}\"; fi; i=$((i+1)); done; sleep 30"
    );
    let spec = |name: &str| Spec {
        name: name.into(),
        source: Source::Listen {
            command: vec!["sh".into(), "-c".into(), script.clone()],
        },
        fields: vec![
            FieldSpec {
                name: "t".into(),
                key: vec!["t".into()],
                rw: false,
            },
            FieldSpec {
                name: "odd".into(),
                key: vec!["odd".into()],
                rw: false,
            },
        ],
        actions: Vec::new(),
    };
    let heard = services.register_as::<Custom>(&rt, "heard");
    let quiet = services.register_as::<Custom>(&rt, "quiet");
    let heard_id = custom::register(spec("heard"));
    let quiet_id = custom::register(spec("quiet"));
    custom::hear_lines(heard_id);
    heard
        .seed(&rt, |s| *s = Custom::seeded(heard_id, 2))
        .unwrap();
    quiet
        .seed(&rt, |s| *s = Custom::seeded(quiet_id, 2))
        .unwrap();
    let lines: Arc<Mutex<Vec<Vec<Data>>>> = Arc::default();
    let log = lines.clone();
    heard.dynamic().observe(Box::new(move |_, a| {
        if let Applied::Event { args, .. } = a {
            log.lock().unwrap().push(args.clone());
        }
    }));
    let quiet_events = Arc::new(AtomicU32::new(0));
    let q = quiet_events.clone();
    quiet.dynamic().observe(Box::new(move |_, a| {
        if matches!(a, Applied::Event { .. }) {
            q.fetch_add(1, Ordering::SeqCst);
        }
    }));
    heard.acquire(&rt);
    quiet.acquire(&rt);
    let last = Data::Int(LINES - 1);
    let value = |c: &strand_services::Client<Custom>| {
        c.cells()
            .values
            .with_untracked(&rt, |v| v.get(&0i64).map(|v| v.value.clone()))
            .ok()
            .flatten()
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        services.pump(&rt);
        rt.flush();
        if lines.lock().unwrap().len() as i64 >= LINES
            && value(&heard).as_ref() == Some(&last)
            && value(&quiet).as_ref() == Some(&last)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{} of {LINES} lines heard",
            lines.lock().unwrap().len()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let lines = lines.lock().unwrap();
    assert_eq!(lines.len() as i64, LINES, "one event a line, none merged");
    for (i, args) in lines.iter().enumerate() {
        let i = i as i64;
        let odd = if i % 2 == 1 {
            Data::Bool(true)
        } else {
            Data::Null
        };
        assert_eq!(args, &vec![Data::List(vec![Data::Int(i), odd])], "line {i}");
    }
    // The merged fields keep the last value each field had.
    let odd = heard
        .cells()
        .values
        .with_untracked(&rt, |v| v.get(&1i64).map(|v| v.value.clone()))
        .ok()
        .flatten();
    assert_eq!(odd, Some(Data::Bool(true)));
    assert_eq!(quiet_events.load(Ordering::SeqCst), 0, "nobody heard it");
    heard.release(&rt);
    quiet.release(&rt);
    services.shutdown();
    custom::forget(heard_id);
    custom::forget(quiet_id);
}

/// (services-round) `service walls from dir "…" { files: [File] }`: the
/// directory's regular files by name (hidden ones and subdirectories
/// left out, a link to a file listed), each with its path, name, size
/// and time; a file added, rewritten, renamed or removed is shown at
/// once; the directory removed lists nothing (null), and made again is
/// followed again. Nothing polls.
#[test]
fn a_dir_is_listed_and_followed() {
    let rt = Runtime::new();
    let services = Services::new(&rt, Buses::none(), || {});
    let client = services.register_as::<Custom>(&rt, "walls");
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("walls");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("b.png"), b"bb").unwrap();
    std::fs::write(dir.join("a.jpg"), b"a").unwrap();
    std::fs::write(dir.join(".hidden"), b"x").unwrap();
    std::fs::create_dir(dir.join("sub")).unwrap();
    std::fs::write(tmp.path().join("outside.png"), b"ccc").unwrap();
    std::os::unix::fs::symlink(tmp.path().join("outside.png"), dir.join("c.png")).unwrap();
    let id = custom::register(Spec {
        name: "walls".into(),
        source: Source::Dir { path: dir.clone() },
        fields: vec![FieldSpec {
            name: "files".into(),
            key: vec!["files".into()],
            rw: false,
        }],
        actions: Vec::new(),
    });
    client.seed(&rt, |s| *s = Custom::seeded(id, 1)).unwrap();
    client.acquire(&rt);
    let files = || -> Option<Vec<(String, String, i64)>> {
        let v = client
            .cells()
            .values
            .with_untracked(&rt, |v| v.get(&0i64).map(|v| v.value.clone()))
            .ok()
            .flatten()?;
        let Data::List(items) = v else { return None };
        Some(
            items
                .iter()
                .map(|f| {
                    let text = |k: &str| match f.field(k) {
                        Some(Data::Text(t)) => t.to_string(),
                        other => panic!("{k}: {other:?}"),
                    };
                    let size = match f.field("size") {
                        Some(Data::Int(n)) => *n,
                        other => panic!("size: {other:?}"),
                    };
                    assert!(
                        matches!(f.field("modified"), Some(Data::Record { .. })),
                        "{f:?}"
                    );
                    (text("name"), text("path"), size)
                })
                .collect(),
        )
    };
    let names = || files().map(|f| f.into_iter().map(|(n, ..)| n).collect::<Vec<_>>());
    let wait = |what: &str, want: Option<&[&str]>| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            services.pump(&rt);
            rt.flush();
            let now = names();
            if now
                .as_deref()
                .map(|v| v.iter().map(String::as_str).collect::<Vec<_>>())
                == want.map(<[&str]>::to_vec)
            {
                return;
            }
            assert!(Instant::now() < deadline, "never {what}: {now:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    wait("listed", Some(&["a.jpg", "b.png", "c.png"]));
    let listed = files().unwrap();
    assert_eq!(
        listed[1],
        (
            "b.png".to_string(),
            dir.join("b.png").display().to_string(),
            2
        )
    );
    assert_eq!(listed[2].2, 3, "a link is listed with its file's size");

    std::fs::write(dir.join("d.webp"), b"dddd").unwrap();
    wait("a new file", Some(&["a.jpg", "b.png", "c.png", "d.webp"]));
    std::fs::write(dir.join("a.jpg"), b"aaaaa").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while files().is_none_or(|f| f[0].2 != 5) {
        services.pump(&rt);
        rt.flush();
        assert!(Instant::now() < deadline, "never the rewritten size");
        std::thread::sleep(Duration::from_millis(5));
    }
    std::fs::rename(dir.join("b.png"), dir.join("e.png")).unwrap();
    wait("a rename", Some(&["a.jpg", "c.png", "d.webp", "e.png"]));
    std::fs::rename(dir.join("e.png"), tmp.path().join("e.png")).unwrap();
    std::fs::remove_file(dir.join("d.webp")).unwrap();
    wait("a move out and a removal", Some(&["a.jpg", "c.png"]));

    // The directory goes: nothing to list; made again: followed again.
    std::fs::remove_dir_all(&dir).unwrap();
    wait("the directory gone", None);
    std::fs::create_dir(&dir).unwrap();
    wait("the directory made again", Some(&[]));
    std::fs::write(dir.join("z.png"), b"z").unwrap();
    wait("a file in the new directory", Some(&["z.png"]));

    client.release(&rt);
    services.shutdown();
    custom::forget(id);
}

/// A `from file` document rewritten in place, as `echo 5 > f` and a
/// plain `write` do (truncated, then written), is read once it settles:
/// the field goes from one value to the next, never through the empty
/// file `MODIFY` shows in between; a file left empty is read empty.
#[test]
fn a_file_rewritten_in_place_never_shows_it_empty() {
    use std::io::Write;
    let rt = Runtime::new();
    let services = Services::new(&rt, Buses::none(), || {});
    let client = services.register_as::<Custom>(&rt, "mood");
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("mood.json");
    std::fs::write(&path, r#"{"level": 1}"#).unwrap();
    let id = custom::register(Spec {
        name: "mood".into(),
        source: Source::File { path: path.clone() },
        fields: vec![FieldSpec {
            name: "level".into(),
            key: vec!["level".into()],
            rw: false,
        }],
        actions: Vec::new(),
    });
    client.seed(&rt, |s| *s = Custom::seeded(id, 1)).unwrap();
    client.acquire(&rt);
    let level = || {
        services.pump(&rt);
        rt.flush();
        client
            .cells()
            .values
            .with_untracked(&rt, |v| v.get(&0i64).map(|v| v.value.clone()))
            .ok()
            .flatten()
    };
    let seen = Arc::new(Mutex::new(Vec::new()));
    let wait = |want: Data| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let now = level();
            // (Null is the seed, before the first read.)
            if let Some(v) = now.as_ref().filter(|v| **v != Data::Null) {
                let mut s = seen.lock().unwrap();
                if s.last() != Some(v) {
                    s.push(v.clone());
                }
            }
            if now.as_ref() == Some(&want) {
                return;
            }
            assert!(Instant::now() < deadline, "never {want:?}: {now:?}");
            std::thread::sleep(Duration::from_millis(1));
        }
    };
    wait(Data::Int(1));
    for n in 2..=6 {
        let mut f = std::fs::File::create(&path).unwrap();
        // The truncated file stays empty a while before its bytes come,
        // as a slow writer's would.
        let until = Instant::now() + Duration::from_millis(15);
        while Instant::now() < until {
            let _ = level();
            std::thread::sleep(Duration::from_millis(1));
        }
        write!(f, r#"{{"level": {n}}}"#).unwrap();
        drop(f);
        wait(Data::Int(n));
    }
    assert_eq!(
        *seen.lock().unwrap(),
        (1..=6).map(Data::Int).collect::<Vec<_>>(),
        "one value after the other, never the empty file"
    );
    std::fs::write(&path, "").unwrap();
    wait(Data::text(""));

    client.release(&rt);
    services.shutdown();
    custom::forget(id);
}

/// (services-round) A directory past `MAX_DIR_FILES` lists the first
/// files by name (whatever order the directory keeps them in), a
/// subdirectory among them not counting as one.
#[test]
fn a_huge_dir_lists_its_first_files_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let n = custom::MAX_DIR_FILES;
    std::fs::create_dir(tmp.path().join("0")).unwrap();
    for i in (0..n + 50).rev() {
        std::fs::write(tmp.path().join(format!("{i:06}.png")), b"").unwrap();
    }
    let listed = custom::list_dir(tmp.path()).unwrap().unwrap();
    assert_eq!(listed.len(), n);
    let name = |d: &Data| match d.field("name") {
        Some(Data::Text(t)) => t.to_string(),
        other => panic!("{other:?}"),
    };
    assert_eq!(name(&listed[0]), "000000.png");
    assert_eq!(name(&listed[n - 1]), format!("{:06}.png", n - 1));
}

/// (services-round) A directory that keeps changing (a download, a
/// cache) is listed at most every `DIR_RELIST`, not after each change.
#[test]
fn a_busy_dir_is_listed_at_most_every_relist() {
    let rt = Runtime::new();
    let services = Services::new(&rt, Buses::none(), || {});
    let client = services.register_as::<Custom>(&rt, "busy");
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let id = custom::register(Spec {
        name: "busy".into(),
        source: Source::Dir { path: dir.clone() },
        fields: vec![FieldSpec {
            name: "files".into(),
            key: vec!["files".into()],
            rw: false,
        }],
        actions: Vec::new(),
    });
    client.seed(&rt, |s| *s = Custom::seeded(id, 1)).unwrap();
    client.acquire(&rt);
    assert!(services.wait_ready(&rt, Duration::from_secs(10)));
    let before = client.reports();
    // A file grows every 20 ms for 2 s: a hundred changes.
    let busy = Duration::from_secs(2);
    let started = Instant::now();
    let mut i = 0u32;
    while started.elapsed() < busy {
        std::fs::write(dir.join("part"), vec![0u8; i as usize]).unwrap();
        i += 1;
        services.pump(&rt);
        std::thread::sleep(Duration::from_millis(20));
    }
    services.pump(&rt);
    let lists = client.reports() - before;
    let most = (busy.as_millis() / custom::DIR_RELIST.as_millis()) as u64 + 2;
    assert!(
        (1..=most).contains(&lists),
        "{lists} listings for {i} changes in {busy:?}"
    );
    client.release(&rt);
    services.shutdown();
    custom::forget(id);
}
