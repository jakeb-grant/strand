//! (fidelity) A glitch report from several outputs replays every one:
//! each live output a screen of the sandbox with its own name, identity,
//! scale and size, input taken on the output it was on, every surface
//! held to its own, and the cells kept per screen seeded and compared on
//! each. The fixture shell is eval/fixtures/report/screens.strand: a bar
//! on every screen, its state kept per screen.

mod fx;

use std::path::Path;
use std::sync::OnceLock;

use fx::*;
use serde_json::{Value as Json, json};
use strand_core::flight::{self, ARef, NAME_IN_ARENA, Rec, kind};
use strand_eval::harness::{self, RunOptions};
use strand_eval::replay::drawn::Drawn;
use strand_eval::replay::fidelity::same_origin;
use strand_eval::scenario::Scenario;

/// One live output: connector, make, model, serial, logical size, scale
/// (in 120ths) and layout position.
struct Out {
    name: &'static str,
    make: &'static str,
    model: &'static str,
    serial: &'static str,
    size: (u16, u16),
    scale120: u16,
    at: (i32, i32),
}

impl Out {
    fn description(&self) -> String {
        format!("{} {} {}", self.make, self.model, self.serial)
    }

    /// Its `screens.all` id: `make | model | description`.
    fn id(&self) -> String {
        format!("{} | {} | {}", self.make, self.model, self.description())
    }

    /// A cell its bar keeps.
    fn cell(&self, name: &str) -> String {
        format!("Strip[{}].{name}", self.id())
    }
}

/// The four outputs, in `screens.all`'s order: two of the same make and
/// model at different scales and places, a wide one and a laptop panel
/// at scale 2.
const OUTS: [Out; 4] = [
    Out {
        name: "DP-1",
        make: "Fixture Display Co.",
        model: "Panel 27 Ultra",
        serial: "SN-A1B2C3D4",
        size: (1280, 720),
        scale120: 120,
        at: (0, 0),
    },
    Out {
        name: "DP-2",
        make: "Fixture Display Co.",
        model: "Panel 27 Ultra",
        serial: "SN-E5F6G7H8",
        size: (1280, 720),
        scale120: 180,
        at: (1280, 0),
    },
    Out {
        name: "HDMI-A-1",
        make: "Other Maker",
        model: "Wide 34",
        serial: "0001",
        size: (1920, 1080),
        scale120: 120,
        at: (2560, 0),
    },
    Out {
        name: "eDP-1",
        make: "Laptop Inc.",
        model: "Built-in",
        serial: "",
        size: (1280, 800),
        scale120: 240,
        at: (4480, 0),
    },
];

fn shell() -> String {
    std::fs::read_to_string(
        strand_eval::scenario::eval_root().join("fixtures/report/screens.strand"),
    )
    .unwrap()
}

/// The sandbox's origins of the fixture's bar and its button.
struct Origins {
    bar: String,
    button: String,
}

fn origins() -> &'static Origins {
    static O: OnceLock<Origins> = OnceLock::new();
    O.get_or_init(|| {
        let dir = tmp("screens-probe");
        let config = dir.join("config");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("shell.strand"), shell()).unwrap();
        let scn = dir.join("scenarios/local/probe.scn");
        std::fs::create_dir_all(scn.parent().unwrap()).unwrap();
        std::fs::write(
            &scn,
            format!(
                "id = \"probe\"\nscreen = {{ width = 1280, height = 720, scale = 1.0 }}\nconfig_dir = \"{}\"\n\n=== steps\nrest\n",
                config.display()
            ),
        )
        .unwrap();
        let sc = Scenario::load(&scn).unwrap();
        let t = harness::run(&sc, &RunOptions::default()).unwrap().trace;
        let button = t
            .nodes
            .values()
            .find(|m| m.id.as_deref() == Some("button"))
            .and_then(|m| m.origin.clone())
            .unwrap();
        let bar = t.nodes[&t.surfaces[0].root].origin.clone().unwrap();
        Origins { bar, button }
    })
}

/// The bar of output `i`: its surface slot and root node; its button.
fn bar_slot(i: usize) -> u16 {
    i as u16 + 1
}
fn bar_root(i: usize) -> u64 {
    bits(3 + 10 * i as u32, 1)
}
fn button(i: usize) -> u64 {
    bits(7 + 10 * i as u32, 1)
}

/// A cell write whose name and value a version-1 recorder lost together
/// from the changes ring's arena: a name too long for the ring's table
/// was kept with the value, and went with it.
fn nameless_lost(f: &mut Fx, t: u64) {
    f.changes
        .push(Rec::new(t, kind::CELL, NAME_IN_ARENA).with_value(ARef {
            off: 0xFFFF_0000,
            len: 24,
            truncated: false,
        }));
}

/// The four-output session, the report at 30 s:
/// - DP-2's button clicked at 8 s (opening its card, which grows) and
///   again at 20 s (closing it); with `lost_names`, its first click's
///   writes of its own cells lost their names with their values;
/// - `mood.level` set to 7 at 12 s, which every screen's bar counts;
/// - eDP-1's button clicked at 14 s (its card open at the report);
/// - the pointer over HDMI-A-1's bar at 16 s, clicking nothing.
fn session(lost_names: bool) -> Fx {
    let o = origins();
    let mut f = Fx::session();
    f.input = flight::Ring::new(flight::INPUT, flight::ring::INPUT);
    f.changes = flight::Ring::new(flight::CHANGES, flight::ring::CHANGES);
    f.config = shell();
    f.nodes.clear();
    f.surfaces.clear();
    f.outputs.clear();
    for (i, out) in OUTS.iter().enumerate() {
        f.output(
            i as u16,
            out.name,
            out.size.0,
            out.size.1,
            out.scale120,
            900 * MS,
        );
        f.outputs[i] = json!({
            "name": out.name, "make": out.make, "model": out.model,
            "id": out.id(), "description": out.description(), "order": i,
            "scale": f64::from(out.scale120) / 120.0,
            "size": [out.size.0, out.size.1], "position": [out.at.0, out.at.1],
        });
    }
    for (i, out) in OUTS.iter().enumerate() {
        f.node(bar_root(i), &o.bar, None);
        f.node(button(i), &o.button, Some("button"));
        f.surface(bar_slot(i), i as u8, bar_root(i), &o.bar, S, 16_666_667);
        let s = f.surfaces.last_mut().unwrap();
        let (w, scale) = (out.size.0, f64::from(out.scale120) / 120.0);
        s["size"] = json!([w, 40]);
        s["buffer"] = json!([(f64::from(w) * scale).round(), (40.0 * scale).round()]);
        s["scale"] = json!(scale);
    }
    let click = |f: &mut Fx, i: usize, t: u64, taps: u32, clicks: u32, open: bool| {
        f.pointer(bar_slot(i), kind::ENTER, 40.0, 20.0, t - S);
        f.press(bar_slot(i), 40.0, 20.0, t, Some(button(i)));
        let out = &OUTS[i];
        let at = t + 100 * MS;
        let lose = lost_names && i == 1 && t == 8 * S;
        if lose {
            nameless_lost(f, at);
        } else {
            f.cell(at, &out.cell("taps"), &format!("[{},{taps}]", taps - 1));
        }
        f.cell(at, "shell.clicks", &format!("[{},{clicks}]", clicks - 1));
        let flip = format!("[{},{open}]", !open);
        if lose {
            nameless_lost(f, at);
            nameless_lost(f, at + 33 * MS);
        } else {
            f.cell(at, &out.cell("open"), &flip);
            f.cell(at + 33 * MS, &out.cell("grown"), &flip);
        }
        f.pointer(bar_slot(i), kind::LEAVE, 0.0, 0.0, t + 500 * MS);
    };
    click(&mut f, 1, 8 * S, 1, 1, true);
    f.field(12 * S, "mood.level", "[1,7]");
    for out in &OUTS {
        f.cell(12 * S, &out.cell("seen"), "[0,1]");
    }
    click(&mut f, 3, 14 * S, 1, 2, true);
    f.pointer(bar_slot(2), kind::ENTER, 300.0, 20.0, 16 * S);
    f.pointer(bar_slot(2), kind::MOTION, 500.0, 20.0, 17 * S);
    f.pointer(bar_slot(2), kind::LEAVE, 0.0, 0.0, 18 * S);
    click(&mut f, 1, 20 * S, 2, 3, false);
    let mut cells = serde_json::Map::new();
    cells.insert("shell.clicks".into(), json!(3));
    for (i, out) in OUTS.iter().enumerate() {
        let (taps, open) = [(0, false), (2, false), (0, false), (1, true)][i];
        cells.insert(out.cell("taps"), json!(taps));
        cells.insert(out.cell("seen"), json!(1));
        cells.insert(out.cell("open"), json!(open));
        cells.insert(out.cell("grown"), json!(open));
    }
    f.cells = Json::Object(cells);
    f
}

/// `drawn.json`: each bar's live boxes, `bars[i]` output `i`'s.
fn write_drawn_bars(dir: &Path, bars: &[Vec<Drawn>]) {
    let surfaces: Vec<Json> = bars
        .iter()
        .enumerate()
        .map(|(i, nodes)| {
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
            let r = bar_root(i);
            json!({
                "slot": bar_slot(i),
                "root": format!("{}.{}", r as u32, (r >> 32) as u32),
                "nodes": nodes,
            })
        })
        .collect();
    std::fs::write(
        dir.join("drawn.json"),
        serde_json::to_vec_pretty(&json!({"format": 1, "surfaces": surfaces})).unwrap(),
    )
    .unwrap();
}

/// (fidelity) The root cause: a report from four outputs replays on
/// each, so the cells every screen's bar keeps are written in the
/// replay as they were live (each screen's own clicks, and the service
/// change every bar counts), with no divergence and no seed left
/// unmounted. It uses only what a replay had before multi-output
/// replays: on one screen, the other three outputs' writes were never
/// made and their seeds never mounted.
#[test]
fn a_report_from_four_outputs_replays_every_one() {
    let dir = session(false).write("screens-four");
    let (r, _) = replay_of(&dir, &defaults());
    let cells: Vec<&str> = r
        .fidelity
        .divergences
        .iter()
        .filter(|d| d.what == "cell")
        .map(|d| d.text.as_str())
        .collect();
    assert!(cells.is_empty(), "cell divergences: {cells:#?}\n{}", r.text);
    assert!(
        r.fidelity.divergences.is_empty(),
        "{:#?}\n{}",
        r.fidelity.divergences,
        r.text
    );
    assert_eq!(r.fidelity.writes.0, r.fidelity.writes.1, "{}", r.text);
    assert!(
        !r.text.contains("no such cell was ever mounted"),
        "{}",
        r.text
    );
}

/// (fidelity) Every output is a screen of the sandbox: in `screens.all`'s
/// order, each with its name, identity, scale, size and place; the main
/// one the busiest. Input is taken on its own output (`screen` steps).
/// No output is a limit any more.
#[test]
fn each_output_is_a_screen_with_its_own_identity() {
    let dir = session(false).write("screens-plan");
    let (_, p, text) = plan_of(&dir, &defaults());
    let names: Vec<&str> = p.outputs.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(names, ["DP-1", "DP-2", "HDMI-A-1", "eDP-1"]);
    assert_eq!(p.output.name, "DP-2", "the most input in the last 10 s");
    let twins: Vec<(Option<String>, f64)> = p.outputs[..2]
        .iter()
        .map(|o| (o.id.clone(), o.scale))
        .collect();
    assert_eq!(
        twins,
        [(Some(OUTS[0].id()), 1.0), (Some(OUTS[1].id()), 1.5)]
    );
    assert_eq!(p.outputs[3].scale, 2.0);
    assert_eq!(p.outputs[2].at, Some([2560, 0]));
    assert!(text.contains("screens = [\n"), "{text}");
    assert!(text.contains("main_screen = \"DP-2\""), "{text}");
    let st = steps(&text);
    let screens: Vec<&str> = st
        .iter()
        .copied()
        .filter(|s| s.starts_with("screen "))
        .collect();
    assert_eq!(
        screens,
        [
            "screen DP-2",
            "screen eDP-1",
            "screen HDMI-A-1",
            "screen DP-2"
        ],
        "{text}"
    );
    // Each `screen` step comes before the input on its output.
    let first_press = st.iter().position(|s| s.starts_with("press")).unwrap();
    assert!(st.iter().position(|s| *s == "screen DP-2").unwrap() < first_press);
    assert!(
        !p.limits
            .iter()
            .any(|l| l.contains("not replayed (the sandbox")),
        "{:?}",
        p.limits
    );
    assert!(
        !p.limits.iter().any(|l| l.starts_with("other outputs:")),
        "{:?}",
        p.limits
    );
    // The scenario reads back with its four screens.
    let scn = dir.join("probe.scn");
    std::fs::write(&scn, &text).unwrap();
    let sc = Scenario::parse_in(&text, &scn, strand_eval::scenario::Place::Local).unwrap();
    assert_eq!(sc.screens.len(), 4);
    assert_eq!(sc.screens[sc.main].identity.name, "DP-2");
    assert_eq!(sc.screens[3].screen.scale, 2.0);
    assert_eq!(sc.screens[1].identity.id, OUTS[1].id());
}

/// (fidelity) The replay's trace says which output each surface is on and
/// at what scale it drew, and the verdict holds each live surface to the
/// replayed one on its own output: four bars from one place, only
/// eDP-1's with its card open at the report, all VERIFIED; the card
/// moved on eDP-1 alone is DRIFTED there and nowhere else.
#[test]
fn every_output_is_traced_and_held_to_its_own() {
    let dir = session(false).write("screens-verdict");
    let (r, out) = replay_of(&dir, &defaults());
    assert_eq!(r.verdict.word(), "NOT CHECKED", "{}", r.verdict.line());
    let o = origins();
    let mut bars: Vec<Vec<Drawn>> = vec![Vec::new(); 4];
    for ((origin, nodes), output) in r.verdict.replayed.iter().zip(&r.verdict.replayed_on) {
        assert!(same_origin(origin, &o.bar), "{origin}");
        let i = OUTS
            .iter()
            .position(|x| Some(x.name) == output.as_deref())
            .unwrap_or_else(|| panic!("a surface on {output:?}"));
        bars[i] = nodes.clone();
    }
    let is_card = |d: &Drawn| d.id.as_deref() == Some("card");
    assert_eq!(
        bars.iter()
            .map(|b| b.iter().any(is_card))
            .collect::<Vec<_>>(),
        [false, false, false, true],
        "eDP-1's card alone is open at the report"
    );
    let trace: Json =
        serde_json::from_str(&std::fs::read_to_string(find(&out, "trace.json")).unwrap()).unwrap();
    let outputs: Vec<&str> = trace["surfaces"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["output"].as_str())
        .collect();
    assert_eq!(outputs.len(), 4, "{outputs:?}");
    let with = trace["frames"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["scales"].is_object())
        .expect("a frame with each surface's scale");
    let mut scales: Vec<f64> = with["scales"]
        .as_object()
        .unwrap()
        .values()
        .map(|v| v.as_f64().unwrap())
        .collect();
    scales.sort_by(f64::total_cmp);
    assert_eq!(scales, [1.0, 1.0, 1.5, 2.0]);
    write_drawn_bars(&dir, &bars);
    let (r, _) = replay_of(&dir, &defaults());
    assert_eq!(
        r.verdict.word(),
        "VERIFIED",
        "{}",
        r.verdict.lines().join("\n")
    );
    assert_eq!(r.verdict.surfaces, 4);
    // The card 3 px off on eDP-1's bar: drifted, named with its output.
    let mut moved = bars.clone();
    for n in moved[3].iter_mut().filter(|n| is_card(n)) {
        n.rect[0] += 3.0;
    }
    write_drawn_bars(&dir, &moved);
    let (r, _) = replay_of(&dir, &defaults());
    assert_eq!(r.verdict.word(), "DRIFTED", "{}", r.verdict.line());
    assert_eq!(r.verdict.drifts.len(), 1, "{:?}", r.verdict.lines());
    assert_eq!(r.verdict.drifts[0].output.as_deref(), Some("eDP-1"));
    assert!(
        r.verdict.lines()[0].contains("on eDP-1"),
        "{:?}",
        r.verdict.lines()
    );
}

/// (fidelity) A version-1 recorder lost a long name with its value: the
/// first click on DP-2 wrote that bar's `taps`, `open` and `grown`, and
/// the ring kept neither the names nor the values. Rewinding from the
/// report past them, those cells' values at the window's start are not
/// known (the rewind used to start the card grown, so its growth never
/// played); the window starts with the shell, so they start as the config
/// declares them, said in a note; the lost writes are held to the
/// replay's by time, and nothing diverges.
#[test]
fn a_name_lost_with_its_value_starts_its_cell_as_declared() {
    let dir = session(true).write("screens-lost");
    let (_, p, text) = plan_of(&dir, &defaults());
    let b = &OUTS[1];
    for c in ["taps", "open", "grown"] {
        assert!(
            !p.seeds.contains_key(&b.cell(c)),
            "{c} is left unseeded: {text}"
        );
        assert!(
            p.notes
                .iter()
                .any(|n| n.starts_with(&format!("`{}`", b.cell(c)))
                    && n.contains("as the config declares it")),
            "{:#?}",
            p.notes
        );
    }
    assert_eq!(
        p.writes.iter().filter(|w| w.path.is_none()).count(),
        3,
        "the lost writes are compared by time"
    );
    let (r, _) = replay_of(&dir, &defaults());
    assert!(
        r.fidelity.divergences.is_empty(),
        "{:#?}\n{}",
        r.fidelity.divergences,
        r.text
    );
    assert!(
        r.fidelity
            .notes
            .iter()
            .any(|n| n.contains("nearest in time")),
        "{:?}",
        r.fidelity.notes
    );
    // The card grew in the replay on DP-2 after the first click.
    assert!(
        r.text
            .contains(&format!("cell {} = true (replay)", b.cell("grown"))),
        "{}",
        r.text
    );
}

/// (fidelity) VERIFIED and DRIFTED are of the frame at the report; a
/// replay that parted from the live shell earlier in the window says so
/// in the same line, everywhere the verdict is given (replay.txt's first
/// line, fidelity.json, the page, a clip): how many times, the first's
/// time and what. Here DP-1's bar counted a click live at 10 s that the
/// rings kept no input for: the frame at the report holds every box (its
/// colour is not a box), and the line says the replay parted there.
#[test]
fn a_verified_frame_says_where_the_replay_parted_before_it() {
    let mut f = session(false);
    let a = &OUTS[0];
    f.cell(10 * S, &a.cell("taps"), "[0,1]");
    f.cells[a.cell("taps")] = json!(1);
    let dir = f.write("screens-parted");
    let (r, _) = replay_of(&dir, &defaults());
    let mut bars: Vec<Vec<Drawn>> = vec![Vec::new(); 4];
    for ((_, nodes), output) in r.verdict.replayed.iter().zip(&r.verdict.replayed_on) {
        let i = OUTS
            .iter()
            .position(|x| Some(x.name) == output.as_deref())
            .unwrap();
        bars[i] = nodes.clone();
    }
    write_drawn_bars(&dir, &bars);
    let (r, out) = replay_of(&dir, &defaults());
    assert_eq!(r.verdict.word(), "VERIFIED", "{}", r.verdict.line());
    let want = format!(
        "; but the replay parted from the live shell once earlier in the window, first at \
         -20.00s (cell: cell {} = 1 live, the replay never wrote it)",
        a.cell("taps")
    );
    let first = r.text.lines().next().unwrap();
    assert!(first.starts_with("verdict: VERIFIED: "), "{first}");
    assert!(first.ends_with(&want), "{first}");
    let j: Json =
        serde_json::from_str(&std::fs::read_to_string(out.join("fidelity.json")).unwrap()).unwrap();
    assert!(
        j["verdict"]["line"].as_str().unwrap().ends_with(&want),
        "{}",
        j["verdict"]["line"]
    );
    assert_eq!(j["verdict"]["parted"]["count"], json!(1));
    assert_eq!(j["verdict"]["parted"]["first"]["what"], json!("cell"));
    let page = std::fs::read_to_string(out.join("index.html")).unwrap();
    let data = strand_eval::report::html::embedded(&page).unwrap();
    assert!(
        data["replay"]["line"].as_str().unwrap().ends_with(&want),
        "{}",
        data["replay"]["line"]
    );
    // A clip of the report's last second: each VERIFIED surface says the
    // replay parted before, and the earlier divergence is named.
    let rp = strand_eval::replay::clip::Replay::find(&out).unwrap();
    let spec = strand_eval::replay::clip::Spec {
        from: "-1s".into(),
        to: None,
        nodes: vec!["#button".into()],
        note: String::new(),
    };
    let m = strand_eval::replay::clip::make(&rp, &spec, false).unwrap();
    let held: Vec<&str> = m
        .text
        .lines()
        .filter(|l| l.contains("VERIFIED, every node"))
        .collect();
    assert!(!held.is_empty(), "{}", m.text);
    assert!(
        held.iter()
            .all(|l| l.contains("but the replay parted from the live shell once")),
        "{}",
        m.text
    );
    assert!(
        held.iter()
            .any(|l| l.starts_with("  @") && l.contains(" on DP-1: ")),
        "{}",
        m.text
    );
    assert!(
        m.text
            .contains("1 earlier in the window, the first at -20.000 s (cell: "),
        "{}",
        m.text
    );
}

/// (fidelity) A cell whose one change in the window kept its name but
/// lost its value: each change holds the value before it, so with none
/// older the cell's value at the window's start is not known. The rewind
/// used to start it at the value after that change, so the replay never
/// made it (eDP-1's card started open and grown, and never grew); in a
/// window from the shell's start it starts as the config declares it,
/// said in a note, and the lost writes are held to the replay's by time.
#[test]
fn a_cells_oldest_change_lost_starts_it_as_declared() {
    let mut f = session(false);
    let e = &OUTS[3];
    let lost = ARef {
        off: 0xFFFF_0000,
        len: 8,
        truncated: false,
    };
    let mut kept = flight::Ring::new(flight::CHANGES, flight::ring::CHANGES);
    for en in flight::read(&f.changes.dump(f.now)).unwrap().entries() {
        let name = en.name.clone().unwrap_or_default();
        if name == e.cell("open") || name == e.cell("grown") {
            let slot = kept.name(&name);
            assert_ne!(slot, NAME_IN_ARENA, "{name} is short enough for the table");
            kept.push(Rec::new(en.rec.t, kind::CELL, slot).with_value(lost));
        } else {
            let v = en.value.clone().unwrap_or_default();
            kept.push_named(
                en.rec.t,
                en.rec.kind,
                &name,
                &en.rec.payload[..6],
                0,
                Some(&mut |w: &mut flight::ArenaWriter<'_>| w.put(&v)),
            );
        }
    }
    f.changes = kept;
    let dir = f.write("screens-oldest-lost");
    let (_, p, text) = plan_of(&dir, &defaults());
    for c in ["open", "grown"] {
        assert!(!p.seeds.contains_key(&e.cell(c)), "{c} is seeded: {text}");
        assert!(
            p.notes
                .iter()
                .any(|n| n.starts_with(&format!("`{}`", e.cell(c)))
                    && n.contains("starts it as the config declares it")),
            "{:#?}",
            p.notes
        );
    }
    let open_grown = [Some(e.cell("open")), Some(e.cell("grown"))];
    assert_eq!(
        p.writes
            .iter()
            .filter(|w| w.value.is_none() && open_grown.contains(&w.path))
            .count(),
        2,
        "{:?}",
        p.writes
    );
    let (r, _) = replay_of(&dir, &defaults());
    assert!(
        r.fidelity.divergences.is_empty(),
        "{:#?}\n{}",
        r.fidelity.divergences,
        r.text
    );
    assert!(
        r.text
            .contains(&format!("cell {} = true (replay)", e.cell("grown"))),
        "{}",
        r.text
    );
}

/// (fidelity) A live write the replay did not make is the divergence,
/// at its own time, though the replay made the same write later: that
/// one is held to the live write it matches. DP-2's card closed live at
/// 18 s with no input to close it, and again with the click at 20 s.
#[test]
fn a_write_the_replay_missed_diverges_at_its_own_time() {
    let mut f = session(false);
    let b = &OUTS[1];
    f.cell(18 * S, &b.cell("open"), "[true,false]");
    let dir = f.write("screens-missed");
    let (r, _) = replay_of(&dir, &defaults());
    let cells: Vec<String> = r
        .fidelity
        .divergences
        .iter()
        .filter(|d| d.what == "cell")
        .map(|d| d.text.clone())
        .collect();
    assert_eq!(
        cells,
        [format!(
            "cell {} = false live, the replay never wrote it",
            b.cell("open")
        )],
        "{}",
        r.text
    );
    assert!(
        r.text.contains(&format!(
            "  -12.00s cell: cell {} = false live",
            b.cell("open")
        )),
        "{}",
        r.text
    );
}

/// The first file named `name` under `dir`.
fn find(dir: &Path, name: &str) -> std::path::PathBuf {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().is_some_and(|n| n == name) {
                return p;
            }
        }
    }
    panic!("no {name} under {}", dir.display());
}
