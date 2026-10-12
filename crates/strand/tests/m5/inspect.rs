//! S-inspect: the inspector (pick, node, dump, highlight, flash) against
//! `strand run` on a headless sway with two outputs, driven by a virtual
//! pointer: a pick on the second output answers the node, whose pane has
//! its identity (that output's instance), its layout on that output and
//! its tokens' provenance;
//! pick mode holds the click from the handlers, and ending it (asked, or
//! by the client's going) gives input back; and an edit written back by
//! `strand-dev inspect`'s write-back reloads with the state kept.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use serde_json::Value as Json;
use strand_core::ipc::{Request, encode};

use crate::support;

/// `strand-dev inspect`'s write-back, compiled here from its source: a
/// dev-dependency on strand-dev would unify strand-compiler's `shaders`
/// feature into the CPU-only binary this crate's budget tests build.
#[allow(dead_code)]
#[path = "../../../strand-dev/src/inspect/write.rs"]
mod write;

use super::cli::{Desk, json, tools};

const BAR: &str = "tokens base { probe.c: #f38ba8 }\n\
export state label = \"hi\"\n\
export state clicks = 0\n\
bar Top {\n  edge: top; height: 40\n  bg: #1e1e2e\n  \
box {\n    id: hit\n    width: 200; height: 40\n    bg: $probe.c\n    \
on click { clicks += 1 }\n  }\n}\n";

/// Both outputs' layout: HEADLESS-1 at 0,0 and HEADLESS-2 at 1280,0,
/// each 1280×720.
const LAYOUT: (u32, u32) = (2560, 720);

/// A socket connection of the inspector's: answers, and the events pick
/// mode sends between them.
struct Ipc {
    w: UnixStream,
    r: BufReader<UnixStream>,
    events: Vec<Json>,
}

impl Ipc {
    fn new(desk: &Desk) -> Self {
        let s = UnixStream::connect(desk.root().join("s.sock")).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        Self {
            w: s.try_clone().unwrap(),
            r: BufReader::new(s),
            events: Vec::new(),
        }
    }

    fn line(&mut self) -> Json {
        let mut l = String::new();
        self.r.read_line(&mut l).unwrap();
        json(&l)
    }

    fn ask(&mut self, req: &Request) -> Json {
        self.w.write_all(encode(req).as_bytes()).unwrap();
        loop {
            let v = self.line();
            if v.get("event").is_some() {
                self.events.push(v);
            } else {
                return v;
            }
        }
    }

    fn event(&mut self) -> Json {
        if !self.events.is_empty() {
            return self.events.remove(0);
        }
        self.line()
    }
}

/// The desk with a second output at 1280,0, once both bars have
/// painted, and a virtual pointer on its seat.
fn two_outputs(tag: &str) -> (Desk, support::pointer::Pointer) {
    let desk = Desk::start(tag, &[("bar.strand", BAR)], &[]);
    let file = |prefix: &str| {
        std::fs::read_dir(desk.root())
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.starts_with(prefix) && !n.ends_with(".lock")
            })
            .unwrap()
            .path()
    };
    let ipc = file("sway-ipc.");
    for args in [
        &["create_output"][..],
        &[
            "output",
            "HEADLESS-2",
            "resolution",
            "1280x720",
            "position",
            "1280",
            "0",
        ],
    ] {
        let ok = std::process::Command::new("swaymsg")
            .args(args)
            .env("SWAYSOCK", &ipc)
            .output()
            .unwrap();
        assert!(ok.status.success(), "swaymsg {args:?}");
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let r = json(&desk.ok(&["report", "--json"]));
        let painted = r["surfaces"].as_array().is_some_and(|s| {
            s.len() == 2 && s.iter().all(|s| s["frames"]["drawn"].as_u64() > Some(0))
        });
        if painted {
            break;
        }
        assert!(Instant::now() < deadline, "the second bar: {r}");
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut pointer = support::pointer::Pointer::new(&file("wayland-"));
    // A new virtual pointer's first buttons reach no surface (strand
    // binds the seat's pointer late): clicked in pick mode until one is
    // picked, so the tests' clicks all land.
    let mut c = Ipc::new(&desk);
    pick(&desk, &mut c, true);
    c.r.get_ref()
        .set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    for nudge in 0.. {
        assert!(Instant::now() < deadline, "no click reached a bar");
        pointer.motion(30 + nudge % 8, 20, LAYOUT.0, LAYOUT.1);
        pointer.click(30 + nudge % 8, 20, LAYOUT.0, LAYOUT.1);
        let mut l = String::new();
        if c.r.read_line(&mut l).is_ok_and(|n| n > 0) && json(&l)["event"] == "picked" {
            break;
        }
    }
    c.r.get_ref()
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    pick(&desk, &mut c, false);
    (desk, pointer)
}

/// Pick mode on or off, once main has it: `inspect.pick` is answered
/// when logic has told main, and a click may reach main before that
/// message does, so a `report` (a snapshot from main, asked after it, on
/// the same channel) follows it.
fn pick(desk: &Desk, c: &mut Ipc, on: bool) {
    assert_eq!(c.ask(&Request::InspectPick { on })["ok"], true);
    desk.ok(&["report", "--json"]);
}

fn clicks(desk: &Desk) -> String {
    desk.ok(&["get", "bar.clicks"]).trim().to_string()
}

/// Waits (bounded) for `bar.clicks` to read `want`.
fn wait_clicks(desk: &Desk, want: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while clicks(desk) != want {
        assert!(
            Instant::now() < deadline,
            "clicks is {}, not {want}\n{}",
            clicks(desk),
            desk.log_text()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A virtual-pointer pick on output 2 answers the node there, and
/// `inspect.node` gives its identity (that output's instance), its
/// layout on that output's surface and its `bg`'s provenance; the pick
/// never reaches `on click`.
#[test]
fn a_pick_on_output_2_answers_identity_layout_and_provenance() {
    if !tools() {
        return;
    }
    let (desk, mut ptr) = two_outputs("ipick");
    let mut c = Ipc::new(&desk);
    pick(&desk, &mut c, true);
    let (w, h) = LAYOUT;
    ptr.motion(1280 + 30, 20, w, h);
    ptr.click(1280 + 50, 20, w, h);
    let picked = c.event();
    assert_eq!(picked["event"], "picked", "{picked}");
    let id = picked["node"]
        .as_str()
        .unwrap_or_else(|| panic!("{picked}"));
    let surface = picked["surface"].clone();
    let pane = c.ask(&Request::InspectNode { id: id.into() });
    assert_eq!(pane["ok"], true, "{pane}");
    let n = &pane["node"];
    assert_eq!(n["kind"], "box", "{pane}");
    let identity = &n["identity"];
    assert_eq!(identity["id"], "hit", "{pane}");
    let instance = identity["instance_path"].clone();
    assert!(
        n["origin"]["file"]
            .as_str()
            .unwrap()
            .ends_with("bar.strand")
    );
    // Its layout: on the surface picked on, the box as drawn.
    let layout = pane["layout"].as_array().unwrap();
    assert_eq!(layout.len(), 1, "{pane}");
    assert_eq!(layout[0]["surface"], surface);
    for (k, v) in [("x", 0.0), ("y", 0.0), ("width", 200.0), ("height", 40.0)] {
        assert_eq!(layout[0][k].as_f64(), Some(v), "{k}: {pane}");
    }
    // `bg`'s provenance: the token, from `tokens base`.
    let bg = n["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["prop"] == "bg")
        .unwrap_or_else(|| panic!("{pane}"));
    assert_eq!(bg["chain"][0]["path"], "probe.c");
    assert_eq!(bg["chain"][0]["origin"], "tokens base");
    // The same box picked on output 1 is that output's instance, on
    // that output's surface.
    ptr.motion(30, 20, w, h);
    ptr.click(50, 20, w, h);
    let one = c.event();
    assert_eq!(one["event"], "picked", "{one}");
    assert_ne!(one["surface"], surface);
    let pane1 = c.ask(&Request::InspectNode {
        id: one["node"].as_str().unwrap().into(),
    });
    assert_eq!(pane1["node"]["identity"]["id"], "hit", "{pane1}");
    assert_ne!(pane1["node"]["identity"]["instance_path"], instance);
    assert_eq!(pane1["layout"][0]["surface"], one["surface"]);
    // The picks went to the inspector, not the handler.
    assert_eq!(clicks(&desk), "0");
    // The outline and the flash are the client's to set.
    assert_eq!(
        c.ask(&Request::InspectHighlight {
            id: Some(id.into())
        })["ok"],
        true
    );
    assert_eq!(c.ask(&Request::InspectFlash { on: true })["ok"], true);
    assert_eq!(c.ask(&Request::InspectFlash { on: false })["ok"], true);
}

/// Ending pick mode gives the click back to `on click`: asked
/// (`inspect.pick{on:false}`), and on the client's going.
#[test]
fn ending_pick_mode_returns_input_to_the_handlers() {
    if !tools() {
        return;
    }
    let (desk, mut ptr) = two_outputs("iend");
    let (w, h) = LAYOUT;
    let mut c = Ipc::new(&desk);
    pick(&desk, &mut c, true);
    ptr.click(50, 20, w, h);
    assert_eq!(c.event()["event"], "picked");
    assert_eq!(clicks(&desk), "0");
    pick(&desk, &mut c, false);
    ptr.click(1280 + 50, 20, w, h);
    wait_clicks(&desk, "1");
    // Pick mode again, then the client goes: input comes back. Main
    // hears of the going asynchronously, so a click that lands first is
    // picked and nobody hears it; the next reaches the handler.
    pick(&desk, &mut c, true);
    drop(c);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        ptr.click(50, 20, w, h);
        let t = Instant::now() + Duration::from_millis(500);
        while clicks(&desk) == "1" && Instant::now() < t {
            std::thread::sleep(Duration::from_millis(20));
        }
        if clicks(&desk) != "1" {
            break;
        }
        assert!(Instant::now() < deadline, "pick mode outlived its client");
    }
    assert_eq!(clicks(&desk), "2");
}

/// An edit written back (`strand-dev inspect`'s write-back on the picked
/// node's `width`) is saved into the config, which reloads with the
/// state kept; the stale pane's next edit is refused as changed.
#[test]
fn a_written_back_edit_reloads_and_keeps_state() {
    if !tools() {
        return;
    }
    let (desk, mut ptr) = two_outputs("iwrite");
    let (w, h) = LAYOUT;
    ptr.click(50, 20, w, h);
    wait_clicks(&desk, "1");
    let mut c = Ipc::new(&desk);
    pick(&desk, &mut c, true);
    ptr.click(50, 20, w, h);
    let picked = c.event();
    let id = picked["node"]
        .as_str()
        .unwrap_or_else(|| panic!("{picked}"));
    let pane = c.ask(&Request::InspectNode { id: id.into() });
    assert_eq!(pane["node"]["identity"]["id"], "hit", "{pane}");
    let edit = write::prop_edit(&pane["node"], &pane["files"], "width", "300").unwrap();
    let file = desk.root().join("config").join("bar.strand");
    assert_eq!(
        write::write(&edit).unwrap(),
        std::fs::canonicalize(&file).unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        BAR.replace("width: 200", "width: 300")
    );
    // The shell reloads it: the box is 300 wide on both outputs.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let dump = c.ask(&Request::InspectDump { surface: None });
        let widths: Vec<&Json> = dump["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|n| n["identity"]["id"] == "hit")
            .map(|n| &n["props"]["width"])
            .collect();
        if widths.len() == 2 && widths.iter().all(|w| *w == "300") {
            break;
        }
        assert!(Instant::now() < deadline, "not reloaded: {widths:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
    // With the state kept.
    assert_eq!(clicks(&desk), "1");
    assert_eq!(desk.ok(&["get", "bar.label"]), "hi\n");
    // The pane read before the reload is stale now.
    assert_eq!(
        write::prop_edit(&pane["node"], &pane["files"], "width", "100"),
        Err(write::Refusal::Changed(file.clone()))
    );
}
