//! (services) A config's own write to a service field, replayed from
//! what it replaced. The owner's report of 2026-10-23 (the island's
//! Focus toggle and its Sound toggle clicked on its dashboard) drifted:
//! the changes ring kept each write (`WRITE`) with the value written
//! only, and the service's answer after it as edits from the value the
//! write had left, while the rewind undid only the service's own
//! changes. So a field the config alone changed (`notifications.dnd`)
//! started at its value at the report, and the replay's own toggles
//! ended it the other way; and the answer to the mute (its icon) was
//! set whole from the value before the write, unmuting what the
//! replay's handler had just muted, so `on change` saw a second change
//! live never saw. A recorder from changes dump version 3 records each
//! write as the change it made (`flags::LOCAL`), which the rewind undoes
//! and the play forward plays without setting it; an older dump's
//! writes are played under the answers after them, and what it cannot
//! know, a written field's value before its oldest write, is a limit.
//! The fixture shell is Strand's own, eval/fixtures/report/writes.strand.

mod fx;

use std::sync::OnceLock;

use fx::*;
use serde_json::{Value as Json, json};
use strand_core::flight::{self, flags, kind};
use strand_eval::harness::{self, RunOptions};
use strand_eval::replay::drawn::Drawn;
use strand_eval::scenario::Scenario;

fn shell() -> String {
    std::fs::read_to_string(
        strand_eval::scenario::eval_root().join("fixtures/report/writes.strand"),
    )
    .unwrap()
}

/// The sandbox's origins of the fixture's bar and each `id:`.
struct Origins {
    bar: String,
    ids: Vec<(&'static str, String)>,
}

const IDS: [&str; 6] = ["quiet", "mute", "knob", "moon", "off", "loud"];

fn origins() -> &'static Origins {
    static O: OnceLock<Origins> = OnceLock::new();
    O.get_or_init(|| {
        let dir = tmp("writes-probe");
        let config = dir.join("config");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("shell.strand"), shell()).unwrap();
        let scn = dir.join("scenarios/local/probe.scn");
        std::fs::create_dir_all(scn.parent().unwrap()).unwrap();
        // Every node shown: Focus on, muted, a sink over half.
        std::fs::write(
            &scn,
            format!(
                "id = \"probe\"\nscreen = {{ width = 1280, height = 720, scale = 1.0 }}\nconfig_dir = \"{}\"\n\n[seed]\n\"notifications.dnd\" = \"true\"\n\"audio.sink\" = '{}'\n\"audio.sinks\" = '[{}]'\n\n=== steps\nrest\n",
                config.display(),
                sink(true, 0.9, "muted"),
                sink(true, 0.9, "muted"),
            ),
        )
        .unwrap();
        let sc = Scenario::load(&scn).unwrap();
        let t = harness::run(&sc, &RunOptions::default()).unwrap().trace;
        let of = |id: &str| {
            t.nodes
                .values()
                .find(|m| m.id.as_deref() == Some(id))
                .and_then(|m| m.origin.clone())
                .unwrap_or_else(|| panic!("no #{id}"))
        };
        let bar = t.nodes[&t.surfaces[0].root].origin.clone().unwrap();
        Origins {
            bar,
            ids: IDS.iter().map(|id| (*id, of(id))).collect(),
        }
    })
}

/// The one sink, as the audio service hands it out.
fn sink(muted: bool, volume: f64, icon: &str) -> Json {
    json!({
        "id": 56, "name": "fixture_null", "description": "Fixture Output",
        "icon": format!("audio-volume-{icon}-symbolic"), "default": true,
        "muted": muted, "volume": volume,
    })
}

/// The bar's surface slot and root; each node's bits by its `id:`.
const BAR: u16 = 1;
fn root() -> u64 {
    bits(3, 1)
}
fn node(id: &str) -> u64 {
    let i = IDS.iter().position(|x| *x == id).unwrap() as u32;
    bits(5 + 2 * i, 1)
}

/// Where each button is pressed on the bar.
fn x_of(id: &str) -> f32 {
    match id {
        "quiet" => 40.0,
        "mute" => 120.0,
        _ => 180.0,
    }
}

/// A change record with `flags`.
fn change_with(f: &mut Fx, t: u64, k: u8, name: &str, value: &str, fl: u8) {
    let v = value.to_string();
    f.changes.push_named(
        t,
        k,
        name,
        &[],
        fl,
        Some(&mut |w: &mut flight::ArenaWriter<'_>| w.put(v.as_bytes())),
    );
}

/// A press on button `id` at `t`; `on click` runs on its release.
fn click(f: &mut Fx, id: &str, t: u64) {
    f.press(BAR, x_of(id), 20.0, t, Some(node(id)));
}

/// The config's write at `t` as the recorder writes it: the `WRITE`
/// (the value written) and, from changes dump version 3, the change it
/// made (`LOCAL`).
fn wrote(f: &mut Fx, t: u64, name: &str, value: &str, field: &str, change: &str, local: bool) {
    change_with(f, t, kind::WRITE, name, value, 0);
    if local {
        change_with(f, t, kind::FIELD, field, change, flags::LOCAL);
    }
}

/// The session, the report at 30 s: Focus off, the sink at 0.8 unmuted.
/// - 3 s: the volume set to 0.6 from outside (keys): the sink and the
///   list's item change, and the `on change` counts it;
/// - 8 s: Focus clicked on (a write of the field whole);
/// - 10 s: Sound clicked: muted, and the volume set to 0.3 in the same
///   handler; the service answers 50 ms after the release with the
///   sink's icon (the write set the rest), and the list's item whole;
/// - 12 s and 14 s: Focus clicked off and on again;
/// - 16 s: Sound clicked again: unmuted (the volume written 0.3 again,
///   which changes nothing), the service answering likewise;
/// - 18 s: the knob clicked: the list's item written to 0.9 through the
///   component's parameter (an item write), the sink answering.
///
/// At the report Focus is on, the sink unmuted at 0.9. The audio
/// service's boot value came at 0.5 s, before the bar mapped. With
/// `local`, the changes ring is a version 3 recorder's, each write
/// recorded as the change it made; without, as before (the `WRITE`
/// alone). With `lock`, a lock from 4 s to 5 s: the window starts after
/// it, and the changes ring holds nothing of the shell's run before.
fn session(local: bool, lock: bool) -> Fx {
    session_with(local, lock, false)
}

/// [`session`]; with `odd`, the live shell's handlers wrote other values
/// than the fixture's make: the volume 0.35 at 10 s (the fixture writes
/// 0.3), and the knob's item 0.85 at 18 s (it writes 0.9); and Focus
/// was written off a millisecond before it was written on at 8 s.
fn session_with(local: bool, lock: bool, odd: bool) -> Fx {
    let (vol, knob) = if odd {
        ("0.35", "0.85")
    } else {
        ("0.3", "0.9")
    };
    let o = origins();
    let mut f = Fx::session();
    f.input = flight::Ring::new(flight::INPUT, flight::ring::INPUT);
    f.changes = flight::Ring::new(flight::CHANGES, flight::ring::CHANGES);
    f.config = shell();
    f.nodes.clear();
    f.surfaces.clear();
    f.outputs.clear();
    f.output(0, "DP-1", 1280, 720, 120, 900 * MS);
    f.node(root(), &o.bar, None);
    for (id, origin) in &o.ids {
        f.node(node(id), origin, Some(id));
    }
    f.surface(BAR, 0, root(), &o.bar, S, 16_666_667);
    f.pointer(BAR, kind::ENTER, 300.0, 20.0, 2 * S);
    let (high, medium, muted) = (
        "audio-volume-high-symbolic",
        "audio-volume-medium-symbolic",
        "audio-volume-muted-symbolic",
    );
    // The service's boot value, before the window.
    change_with(
        &mut f,
        500 * MS,
        kind::FIELD,
        "audio.sink",
        &format!(
            "[{},{}]",
            json!({"id": 0, "name": "", "description": "", "icon": "", "default": false, "muted": false, "volume": 0.0}),
            sink(false, 0.8, "high")
        ),
        flags::BOOT,
    );
    // 3 s: from outside.
    f.field(
        3 * S,
        "audio.sink",
        &format!(r#"{{"e":[["volume",0.8,0.6],["icon","{high}","{medium}"]]}}"#),
    );
    f.change(
        3 * S,
        kind::KEYED,
        "audio.sinks",
        &[],
        &format!(r#"{{"d":[{{"patch":[0,56,[["volume",0.8,0.6],["icon","{high}","{medium}"]]]}}],"r":[]}}"#),
    );
    f.cell(3 * S, "shell.seen", "[0,1]");
    f.cell(3 * S, "shell.said", r#"["","loud"]"#);
    if lock {
        f.changes
            .push(flight::Rec::new(4 * S, kind::CHANGE_GAP_BEGINS, 0).u8_at(0, flight::gap::LOCK));
        f.changes
            .push(flight::Rec::new(5 * S, kind::CHANGE_GAP_ENDS, 0).u8_at(0, flight::gap::LOCK));
    }
    // 8 s: Focus on.
    click(&mut f, "quiet", 8 * S);
    let up = 8 * S + 100 * MS;
    if odd {
        // Written twice within a tick, the first replaced at once.
        change_with(
            &mut f,
            up - MS,
            kind::WRITE,
            "notifications.dnd",
            "false",
            0,
        );
    }
    wrote(
        &mut f,
        up,
        "notifications.dnd",
        "true",
        "notifications.dnd",
        "[false,true]",
        local,
    );
    // 10 s: Sound: muted and the volume, in one handler.
    click(&mut f, "mute", 10 * S);
    let up = 10 * S + 100 * MS;
    wrote(
        &mut f,
        up,
        "audio.sink.muted",
        "true",
        "audio.sink",
        r#"{"e":[["muted",false,true]]}"#,
        local,
    );
    wrote(
        &mut f,
        up,
        "audio.sink.volume",
        vol,
        "audio.sink",
        &format!(r#"{{"e":[["volume",0.6,{vol}]]}}"#),
        local,
    );
    f.cell(up, "shell.seen", "[1,2]");
    f.cell(up, "shell.said", r#"["loud","muted"]"#);
    f.field(
        up + 50 * MS,
        "audio.sink",
        &format!(r#"{{"e":[["icon","{medium}","{muted}"]]}}"#),
    );
    f.change(
        up + 50 * MS,
        kind::KEYED,
        "audio.sinks",
        &[],
        &format!(r#"{{"d":[{{"patch":[0,56,[["volume",0.6,{vol}],["muted",false,true],["icon","{medium}","{muted}"]]]}}],"r":[]}}"#),
    );
    // 12 s and 14 s: Focus off and on.
    for (t, from, to) in [(12 * S, true, false), (14 * S, false, true)] {
        click(&mut f, "quiet", t);
        wrote(
            &mut f,
            t + 100 * MS,
            "notifications.dnd",
            &to.to_string(),
            "notifications.dnd",
            &format!("[{from},{to}]"),
            local,
        );
    }
    // 16 s: Sound again: unmuted; the volume written as it was.
    click(&mut f, "mute", 16 * S);
    let up = 16 * S + 100 * MS;
    wrote(
        &mut f,
        up,
        "audio.sink.muted",
        "false",
        "audio.sink",
        r#"{"e":[["muted",true,false]]}"#,
        local,
    );
    change_with(&mut f, up, kind::WRITE, "audio.sink.volume", "0.3", 0);
    f.cell(up, "shell.seen", "[2,3]");
    f.cell(up, "shell.said", r#"["muted","loud"]"#);
    f.field(
        up + 50 * MS,
        "audio.sink",
        &format!(r#"{{"e":[["icon","{muted}","{medium}"]]}}"#),
    );
    f.change(
        up + 50 * MS,
        kind::KEYED,
        "audio.sinks",
        &[],
        &format!(r#"{{"d":[{{"patch":[0,56,[["muted",true,false],["icon","{muted}","{medium}"]]]}}],"r":[]}}"#),
    );
    // 18 s: the knob writes the list's item; the sink answers.
    click(&mut f, "knob", 18 * S);
    let up = 18 * S + 100 * MS;
    if local {
        change_with(
            &mut f,
            up,
            kind::KEYED,
            "audio.sinks",
            &format!(r#"{{"d":[{{"patch":[0,56,[["volume",0.3,{knob}]]]}}],"r":[]}}"#),
            flags::LOCAL,
        );
    }
    f.field(
        up + 50 * MS,
        "audio.sink",
        &format!(r#"{{"e":[["volume",0.3,{knob}]]}}"#),
    );
    f.cell(up + 50 * MS, "shell.seen", "[3,4]");
    f.pointer(BAR, kind::LEAVE, 0.0, 0.0, 20 * S);
    f.services = json!({
        "notifications": {"running": true, "fields": {"dnd": true, "count": 0, "all": [], "popups": []}},
        "audio": {"running": true, "fields": {
            "sink": sink(false, 0.9, "medium"),
            "sinks": [sink(false, 0.9, "medium")],
        }},
    });
    f.cells = json!({"shell.seen": 4, "shell.said": "loud"});
    f
}

/// Writes `f`, its changes dump marked version `version`.
fn write_as(f: &Fx, tag: &str, version: u16) -> std::path::PathBuf {
    let dir = f.write(tag);
    let p = dir.join("changes.flight");
    let mut bytes = std::fs::read(&p).unwrap();
    bytes[8..10].copy_from_slice(&version.to_le_bytes());
    std::fs::write(&p, bytes).unwrap();
    dir
}

/// `drawn.json`: the bar as the live shell drew it at the report.
fn write_drawn_bar(dir: &std::path::Path, nodes: &[Drawn]) {
    let nodes: Vec<Json> = nodes
        .iter()
        .map(|n| {
            json!({
                "ref": "0.0",
                "origin": format!("{LIVE}/{}", n.origin),
                "id": n.id, "key": n.key, "box": n.rect, "opacity": n.opacity,
                "travel": n.travel, "fade": n.fade,
            })
        })
        .collect();
    let r = root();
    let drawn = json!({"format": 1, "surfaces": [{
        "slot": BAR, "root": format!("{}.{}", r as u32, (r >> 32) as u32), "nodes": nodes,
    }]});
    std::fs::write(
        dir.join("drawn.json"),
        serde_json::to_vec_pretty(&drawn).unwrap(),
    )
    .unwrap();
}

/// The `id:`s a frame drew.
fn ids_of(nodes: &[Drawn]) -> Vec<String> {
    nodes.iter().filter_map(|n| n.id.clone()).collect()
}

/// The live shell's frame at the report: the bar with Focus on (`moon`)
/// and the sink unmuted (no `off`) over half (`loud`), each where the
/// replay of the version 3 session draws it.
fn live_frame() -> Vec<Drawn> {
    static F: OnceLock<Vec<Drawn>> = OnceLock::new();
    F.get_or_init(|| {
        let dir = write_as(&session(true, false), "writes-frame", 3);
        let (r, _) = replay_of(&dir, &defaults());
        let nodes = r.verdict.replayed[0].1.clone();
        let ids = ids_of(&nodes);
        for id in ["quiet", "mute", "knob", "moon", "loud"] {
            assert!(ids.iter().any(|x| x == id), "#{id} not drawn: {ids:?}");
        }
        assert!(!ids.iter().any(|x| x == "off"), "{ids:?}");
        nodes
    })
    .clone()
}

/// (services) The root cause, on what a recorder before version 3
/// writes (the `WRITE` alone): the answer to the mute is played under
/// the write, not set whole from the value before it, so the replay's
/// `on change` sees the one change live saw; and a place the config
/// wrote starts at what it held before its oldest write, read from the
/// changes ring, which holds every change since the shell started:
/// Focus, never reported, at its default (off), the mute where the
/// audio service's boot value left it. It fails on 3ab9d868: Focus
/// started on (its value at the report) and the replay's three toggles
/// ended it off, and the answer at 10.15 s set the sink unmuted at 0.6
/// under the replay's own mute (`said = "muted"` then `"loud"` in the
/// replay only), so the second Sound click muted it.
#[test]
fn a_write_recorded_alone_is_played_under_the_answer_after_it() {
    let dir = write_as(&session(false, false), "writes-v2", 2);
    let (_, p, text) = plan_of(&dir, &defaults());
    assert_eq!(p.seeds["notifications.dnd"], json!(false), "{text}");
    assert_eq!(p.seeds["audio.sink"], sink(false, 0.8, "high"), "{text}");
    assert!(
        p.limits.iter().all(|l| !l.starts_with("service field `")),
        "{:#?}",
        p.limits
    );
    let noted = |what: &str, how: &str| {
        p.notes
            .iter()
            .any(|n| n.starts_with(&format!("service field `{what}`: ")) && n.contains(how))
    };
    assert!(
        noted(
            "notifications.dnd",
            "so it held its default, false: the replay starts it there"
        ),
        "{:#?}",
        p.notes
    );
    assert!(
        noted(
            "audio.sink.muted",
            "the last change recorded before it, at -29.50s, left it at false"
        ),
        "{:#?}",
        p.notes
    );
    write_drawn_bar(&dir, &live_frame());
    let (r, _) = replay_of(&dir, &defaults());
    assert!(
        r.fidelity.divergences.is_empty(),
        "{:#?}\n{}",
        r.fidelity.divergences,
        r.text
    );
    assert_eq!(r.verdict.word(), "VERIFIED", "{}", r.verdict.line());
    assert!(!r.verdict.line().contains("parted"), "{}", r.verdict.line());
}

/// (services) What a recorder before version 3 cannot say is said: after
/// a lock the changes ring holds nothing of the shell's run before it,
/// so a place the config wrote with no change of it recorded since has
/// no value known before its oldest write. A limit names each place
/// written, in replay.txt and fidelity.json: Focus (the replay starts it
/// on, as at the report, and its three toggles end it off: the frame
/// drifts), the mute, and the volume (its change at 3 s is before the
/// lock).
#[test]
fn a_written_place_with_no_value_known_before_it_is_a_limit() {
    let dir = write_as(&session(false, true), "writes-v2-lock", 2);
    let (r, _) = replay_of(&dir, &defaults());
    let limits: Vec<&String> = r
        .plan
        .limits
        .iter()
        .filter(|l| l.starts_with("service field `"))
        .collect();
    assert_eq!(limits.len(), 3, "{limits:#?}");
    assert!(
        limits[0].starts_with(
            "service field `audio.sink.muted`: the config wrote it 2 times in the window (first at \
             -19.90s), and this report's recorder (changes dump version 2, before version 3) \
             kept what each write set but not what it replaced, and no change recorded says \
             what it was before, so the replay starts it at false"
        ),
        "{}",
        limits[0]
    );
    assert!(
        limits[1].starts_with("service field `audio.sink.volume`: the config wrote it 2 times"),
        "{}",
        limits[1]
    );
    assert!(
        limits[2].starts_with(
            "service field `notifications.dnd`: the config wrote it 3 times in the window (first \
             at -21.90s)"
        ) && limits[2].contains("so the replay starts it at true, the value rewound to"),
        "{}",
        limits[2]
    );
    // Said everywhere the replay's limits are.
    assert!(
        r.text.contains("  - service field `notifications.dnd`: "),
        "{}",
        r.text
    );
    assert!(
        r.json["limits"].as_array().unwrap().iter().any(|l| l
            .as_str()
            .unwrap()
            .starts_with("service field `notifications.dnd`")),
        "{}",
        r.json
    );
    // The frame: Focus ends off in the replay, as the limit warns.
    write_drawn_bar(&dir, &live_frame());
    let (r, _) = replay_of(&dir, &defaults());
    assert_eq!(r.verdict.word(), "DRIFTED", "{}", r.verdict.line());
    assert!(
        r.text
            .lines()
            .any(|l| l.contains("#moon") && l.ends_with("not in the replay")),
        "{}",
        r.text
    );
}

/// (services) A version 3 recorder's session replays as it was live:
/// every written field starts at its value before the window's first
/// write (Focus off, unmuted at 0.8, the list's item at 0.8), no write
/// is set (the replay's handlers make them), the answers play on what
/// the writes left, and the frame at the report is VERIFIED with Focus
/// on and the sink unmuted over half, no divergence before it.
#[test]
fn a_config_write_is_replayed_from_what_it_replaced() {
    let dir = write_as(&session(true, false), "writes-v3", 3);
    let (_, p, text) = plan_of(&dir, &defaults());
    assert_eq!(p.seeds["notifications.dnd"], json!(false), "{text}");
    assert_eq!(p.seeds["audio.sink"], sink(false, 0.8, "high"), "{text}");
    assert_eq!(
        p.seeds["audio.sinks"],
        json!([sink(false, 0.8, "high")]),
        "{text}"
    );
    let st = steps(&text);
    assert!(
        !st.iter().any(|s| s.starts_with("set notifications.dnd")),
        "a write is the replay's own: {st:#?}"
    );
    // The answer to the mute plays on what the write left.
    let answer = st
        .iter()
        .filter(|s| s.starts_with("set audio.sink "))
        .nth(1)
        .unwrap();
    let v: Json = serde_json::from_str(answer.trim_start_matches("set audio.sink ")).unwrap();
    assert_eq!(v, sink(true, 0.3, "muted"), "{st:#?}");
    assert!(
        p.limits.iter().all(|l| !l.starts_with("service field `")),
        "{:#?}",
        p.limits
    );
    write_drawn_bar(&dir, &live_frame());
    let (r, _) = replay_of(&dir, &defaults());
    assert!(
        r.fidelity.divergences.is_empty(),
        "{:#?}\n{}",
        r.fidelity.divergences,
        r.text
    );
    assert_eq!(r.verdict.word(), "VERIFIED", "{}", r.verdict.line());
    assert!(!r.verdict.line().contains("parted"), "{}", r.verdict.line());
    let ids = ids_of(&r.verdict.replayed[0].1);
    for id in ["moon", "loud"] {
        assert!(ids.iter().any(|x| x == id), "#{id}: {ids:?}");
    }
    assert!(!ids.iter().any(|x| x == "off"), "{ids:?}");
}

/// (services) A written place in an older report with nothing recorded
/// before it starts at the default its service's store starts with:
/// its type's (a record of them for a record field), except `system`'s,
/// which starts from what it kept across restarts before the recorder
/// starts, and so is never assumed.
#[test]
fn a_service_default_is_its_stores_start_and_never_systems() {
    assert_eq!(
        harness::service_default("notifications.dnd"),
        Some(json!(false))
    );
    let s = harness::service_default("audio.sink").unwrap();
    assert_eq!(
        (&s["muted"], &s["volume"]),
        (&json!(false), &json!(0)),
        "{s}"
    );
    assert_eq!(harness::service_default("brightness.level"), Some(json!(0)));
    assert_eq!(harness::service_default("system.dark"), None);
    assert_eq!(harness::service_default("system.replay"), None);
    assert_eq!(harness::service_default("notifications.nothing"), None);
}

/// (services) A version-1 or -2 ring has no record of a config's write
/// to an item of a keyed list (`dev.volume = …` for `dev` from
/// `audio.sinks`): no `WRITE`, and its echo is never a change. Here the
/// knob's write at 18 s is the only change of `audio.sinks` in the
/// window, so the rewind leaves the list at its value at the report
/// (0.9, where live it was 0.3 until 18 s) and nothing else in the
/// ring says so: the planner names each place the config writes an
/// item, by the lists it may land in. A version-3 ring records the
/// write (`LOCAL`), and the list starts where it was, with no such
/// limit.
#[test]
fn an_item_write_an_older_ring_did_not_record_is_named() {
    let item_only = |local: bool| {
        let o = origins();
        let mut f = Fx::session();
        f.input = flight::Ring::new(flight::INPUT, flight::ring::INPUT);
        f.changes = flight::Ring::new(flight::CHANGES, flight::ring::CHANGES);
        f.config = shell();
        f.nodes.clear();
        f.surfaces.clear();
        f.outputs.clear();
        f.output(0, "DP-1", 1280, 720, 120, 900 * MS);
        f.node(root(), &o.bar, None);
        for (id, origin) in &o.ids {
            f.node(node(id), origin, Some(id));
        }
        f.surface(BAR, 0, root(), &o.bar, S, 16_666_667);
        f.pointer(BAR, kind::ENTER, 300.0, 20.0, 2 * S);
        let empty = json!({"id": 0, "name": "", "description": "", "icon": "", "default": false, "muted": false, "volume": 0.0});
        change_with(
            &mut f,
            500 * MS,
            kind::FIELD,
            "audio.sink",
            &format!("[{empty},{}]", sink(false, 0.3, "low")),
            flags::BOOT,
        );
        change_with(
            &mut f,
            500 * MS,
            kind::KEYED,
            "audio.sinks",
            &format!(
                r#"{{"d":[{{"reset":[[56,{}]]}}],"r":[]}}"#,
                sink(false, 0.3, "low")
            ),
            flags::BOOT,
        );
        click(&mut f, "knob", 18 * S);
        let up = 18 * S + 100 * MS;
        if local {
            change_with(
                &mut f,
                up,
                kind::KEYED,
                "audio.sinks",
                r#"{"d":[{"patch":[0,56,[["volume",0.3,0.9]]]}],"r":[]}"#,
                flags::LOCAL,
            );
        }
        f.field(up + 50 * MS, "audio.sink", r#"{"e":[["volume",0.3,0.9]]}"#);
        f.cell(up + 50 * MS, "shell.seen", "[0,1]");
        f.cell(up + 50 * MS, "shell.said", r#"["","loud"]"#);
        f.pointer(BAR, kind::LEAVE, 0.0, 0.0, 20 * S);
        f.services = json!({
            "notifications": {"running": true, "fields": {"dnd": false, "count": 0, "all": [], "popups": []}},
            "audio": {"running": true, "fields": {
                "sink": sink(false, 0.9, "low"),
                "sinks": [sink(false, 0.9, "low")],
            }},
        });
        f.cells = json!({"shell.seen": 1, "shell.said": "loud"});
        f
    };
    let dir = write_as(&item_only(false), "writes-item-v2", 2);
    let (r, _) = replay_of(&dir, &defaults());
    // Unrecorded, the write is not undone: the list starts as at the
    // report, as the limit warns.
    assert_eq!(
        r.plan.seeds["audio.sinks"][0]["volume"],
        json!(0.9),
        "{}",
        r.text
    );
    let named: Vec<&String> = r
        .plan
        .limits
        .iter()
        .filter(|l| l.starts_with("service list "))
        .collect();
    assert_eq!(named.len(), 1, "{:#?}", r.plan.limits);
    assert!(
        named[0].starts_with(
            "service list `audio.sinks` or `audio.sources`: the config writes an item of it \
             (`volume` at shell.strand:11), and this report's recorder (changes dump version 2, \
             before version 3) kept nothing of a config's write to an item of a keyed list, so \
             the rewind cannot undo one made in the window: the list starts with it already \
             made"
        ),
        "{}",
        named[0]
    );
    assert!(
        r.text
            .contains("  - service list `audio.sinks` or `audio.sources`: "),
        "{}",
        r.text
    );
    assert!(
        r.json["limits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l.as_str().unwrap().starts_with("service list ")),
        "{}",
        r.json
    );
    // Recorded, it is undone, and nothing needs saying.
    let dir = write_as(&item_only(true), "writes-item-v3", 3);
    let (r, _) = replay_of(&dir, &defaults());
    assert_eq!(
        r.plan.seeds["audio.sinks"][0]["volume"],
        json!(0.3),
        "{}",
        r.text
    );
    assert!(
        r.plan
            .limits
            .iter()
            .all(|l| !l.starts_with("service list ")),
        "{:#?}",
        r.plan.limits
    );
    assert!(
        r.fidelity.divergences.is_empty(),
        "{:#?}\n{}",
        r.fidelity.divergences,
        r.text
    );
}

/// (services) The replay's own handlers make every write the config
/// made live (none is played as a `set`), so each is held to the live
/// one: here the live shell wrote the volume 0.35 at 10 s and the
/// knob's item 0.85 at 18 s, where the fixture's handlers write 0.3 and
/// 0.9. Each is a `service` divergence at its own time, so the verdict
/// says the replay parted there, whatever the frame at the report. In
/// the faithful session every write is held, both ways (the tests
/// above: no divergence). A write replaced less than a tick later (two
/// written at once, a drag's motion on a faster output) is below the
/// replay's clock: a note, not a divergence.
#[test]
fn a_service_write_the_replay_made_otherwise_diverges() {
    let dir = write_as(&session_with(true, false, true), "writes-odd", 3);
    let (r, _) = replay_of(&dir, &defaults());
    let service: Vec<&str> = r
        .fidelity
        .divergences
        .iter()
        .filter(|d| d.what == "service")
        .map(|d| d.text.as_str())
        .collect();
    assert_eq!(
        service,
        [
            "service write audio.sink.volume = 0.35 live, the replay wrote audio.sink.volume = 0.3",
            "service write an item of audio.sinks ([{\"patch\":[0,56,[[\"volume\",0.3,0.85]]]}]) \
             live, the replay wrote AudioDevice(56).volume = 0.9",
        ],
        "{:#?}\n{}",
        r.fidelity.divergences,
        r.text
    );
    let at = |text: &str| {
        r.fidelity
            .divergences
            .iter()
            .find(|d| d.text == text)
            .map(|d| d.t)
            .unwrap()
    };
    let len = r.plan.end - r.plan.start;
    // At the writes' own times: 10.1 s and 18.1 s of a report at 30 s.
    assert_eq!(len - at(service[0]), 19_900_000_000, "{}", r.text);
    assert_eq!(len - at(service[1]), 11_900_000_000, "{}", r.text);
    let line = strand_eval::replay::verdict_line(&r.verdict, &r.fidelity, &r.plan);
    assert!(line.contains("parted from the live shell"), "{line}");
    assert_eq!(
        r.json["service_writes"],
        json!({"live": 9, "replay": 8}),
        "{}",
        r.json
    );
    // The write replaced within a tick is below the replay's clock: not
    // required of it, and said.
    assert!(
        r.fidelity.notes.iter().any(|n| n
            == "1 live service write that the next write of its place replaced less than a \
                tick (16.7 ms) later was not made in the replay, which takes input once a tick: \
                notifications.dnd ×1"),
        "{:#?}",
        r.fidelity.notes
    );
}
