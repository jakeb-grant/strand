//! The service contract: `#[derive(Store)]`, `#[derive(Data)]`,
//! `#[derive(Call)]` and `#[service]`, patches applied on the logic
//! thread, echo suppression of local writes, actions and async calls,
//! the shared runtime and a service on its own thread, and the lifecycle
//! (start on the first reader, refcount, stop 5 s after the last on the
//! logic clock, visibility).

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use strand_core::{Runtime, VecDiff};
use strand_services::{
    Applied, Buses, Call, Cells, Cx, Data, DataError, Event, FromCall, FromData, Keyed, Msg, Patch,
    STOP_GRACE, SchemaType, Service, ServiceError, Services, Step, Store, Target, ToData, service,
};

#[derive(strand_services::Data, Clone, Copy, Debug, Default, PartialEq)]
pub enum Urgency {
    #[default]
    Low,
    Normal,
    VeryHigh,
}

/// A workspace.
#[derive(strand_services::Data, Clone, Debug, Default, PartialEq)]
#[data(name = "Workspace", key = id)]
pub struct Ws {
    pub id: i64,
    pub name: String,
    pub urgency: Urgency,
    #[data(rename = "type")]
    pub kind: Option<String>,
}

fn ws(id: i64, name: &str) -> Ws {
    Ws {
        id,
        name: name.into(),
        ..Ws::default()
    }
}

#[derive(Call, Debug, PartialEq)]
pub enum ProbeAction {
    Bump,
    Focus { item: Ws },
    Rename(String, i64),
}

#[derive(Call, Debug, PartialEq)]
pub enum ProbeCall {
    Echo { text: String },
}

const SCHEMA: &str = "service probe { level: float rw }";

/// What a test tells the probe service to do.
enum Cmd {
    Update(Box<dyn FnOnce(&mut Probe) + Send>),
    Send(Vec<ProbePatch>),
    Emit(ProbeEvent),
    Ready,
}

/// The test's end of the probe service.
struct Script {
    cmds: tokio::sync::mpsc::UnboundedSender<Cmd>,
    seen: std::sync::mpsc::Receiver<String>,
}

static CMDS: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<Cmd>>> = Mutex::new(None);
static SEEN: Mutex<Option<std::sync::mpsc::Sender<String>>> = Mutex::new(None);
/// The probe tests run one at a time (one global script).
static SERIAL: Mutex<()> = Mutex::new(());

fn script() -> (Script, MutexGuard<'static, ()>) {
    let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let (seen_tx, seen) = std::sync::mpsc::channel();
    *CMDS.lock().unwrap() = Some(rx);
    *SEEN.lock().unwrap() = Some(seen_tx);
    (Script { cmds: tx, seen }, guard)
}

fn log(s: String) {
    if let Some(tx) = SEEN.lock().unwrap().as_ref() {
        let _ = tx.send(s);
    }
}

impl Script {
    fn cmd(&self, c: Cmd) {
        self.cmds.send(c).unwrap();
    }

    /// The next thing the service saw.
    fn next(&self) -> String {
        self.seen
            .recv_timeout(Duration::from_secs(10))
            .expect("the service saw nothing")
    }

    fn quiet(&self) -> bool {
        self.seen.recv_timeout(Duration::from_millis(200)).is_err()
    }
}

/// A probe: one field of each kind, events of each arity.
#[service(name = "probe", schema = SCHEMA, action = ProbeAction, call = ProbeCall, fns = probe_fns)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Probe {
    /// A writable level.
    #[store(rw)]
    pub level: f64,
    /// A label, if any.
    pub label: Option<String>,
    /// The focused workspace.
    pub focus: Ws,
    /// Every workspace, keyed.
    #[store(keyed)]
    pub all: Vec<Ws>,
    /// A second writable value.
    #[store(rw)]
    pub gain: f64,
    /// A scan's result: produced only while a visible reader reads it.
    #[store(stream)]
    pub scan: Option<String>,
    /// A workspace arrived.
    pub received: Event<Ws>,
    /// Nothing but a ping.
    pub pinged: Event<()>,
    /// Two arguments.
    pub pair: Event<(i64, String)>,
}

fn probe_fns(
    cells: &ProbeCells,
    rt: &Runtime,
    method: &str,
    args: &[Data],
) -> Option<Result<Data, strand_core::Error>> {
    match method {
        "named" => {
            let want = String::from_data(args.first()?).ok()?;
            Some(cells.all.with(rt, |v| {
                Data::List(
                    v.items()
                        .iter()
                        .filter(|(_, w)| w.name == want)
                        .map(|(_, w)| w.to_data())
                        .collect(),
                )
            }))
        }
        _ => None,
    }
}

impl Probe {
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let rx = CMDS.lock().unwrap().take();
        let Some(mut cmds) = rx else {
            return Err("no script".into());
        };
        log(format!("start visible={}", cx.visible()));
        if cx.watched("scan") {
            log("scan watched at start".into());
        }
        loop {
            tokio::select! {
                c = cmds.recv() => match c {
                    Some(Cmd::Update(f)) => { cx.update(f); }
                    Some(Cmd::Send(p)) => { cx.send(p); }
                    Some(Cmd::Emit(p)) => { cx.emit(p); }
                    Some(Cmd::Ready) => { cx.ready(); }
                    None => return Ok(()),
                },
                m = cx.recv() => match m {
                    None => {
                        log("stopped".into());
                        return Ok(());
                    }
                    Some(Msg::Write(w)) => {
                        let v: f64 = w.value().map_err(ServiceError::from)?;
                        log(format!("write {}{} {v}", w.field, w.path.iter().map(ToString::to_string).collect::<String>()));
                        if w.field == "level" {
                            // The service clamps; 0.25 also moves the
                            // gain (one answer changing two fields).
                            cx.report(&w, |s| {
                                s.level = v.min(1.0);
                                if v == 0.25 {
                                    s.gain = 42.0;
                                }
                            });
                        } else if w.field == "gain" {
                            cx.report(&w, |s| s.gain = v);
                        }
                    }
                    Some(Msg::Action(a)) => log(format!("action {a:?}")),
                    Some(Msg::Call(ProbeCall::Echo { text }, reply)) => {
                        reply.send(Ok::<_, String>(text.to_uppercase()));
                    }
                    Some(Msg::Visible(v)) => log(format!("visible {v}")),
                    Some(Msg::Watch { field, on }) => {
                        assert_eq!(cx.watched(field), on);
                        log(format!("watch {field} {on}"));
                    }
                },
            }
        }
    }
}

#[test]
fn the_derives_describe_the_store() {
    let names: Vec<&str> = Probe::FIELDS.iter().map(|f| f.name).collect();
    assert_eq!(names, ["level", "label", "focus", "all", "gain", "scan"]);
    let types: Vec<String> = Probe::FIELDS.iter().map(|f| (f.ty)()).collect();
    assert_eq!(
        types,
        [
            "float",
            "text?",
            "Workspace",
            "[Workspace]",
            "float",
            "text?"
        ]
    );
    assert!(Probe::FIELDS[0].rw && !Probe::FIELDS[1].rw);
    assert!(Probe::FIELDS[3].keyed && !Probe::FIELDS[2].keyed);
    assert!(Probe::FIELDS[5].stream && !Probe::FIELDS[4].stream);
    // Events are their own type: `Cx::emit` takes no field patch.
    let p: ProbePatch = ProbeEvent::Pinged(()).into();
    assert_eq!(p.target(), Target::Event(1));
    assert_eq!(Probe::FIELDS[0].doc, "A writable level.");
    let events: Vec<(&str, usize)> = Probe::EVENTS.iter().map(|e| (e.name, e.arity)).collect();
    assert_eq!(events, [("received", 1), ("pinged", 0), ("pair", 2)]);
    assert_eq!(<Probe as Service>::NAME, "probe");
    assert_eq!(<Probe as Service>::schema(), SCHEMA);
    assert_eq!(ProbeAction::NAMES, ["bump", "focus", "rename"]);
    assert_eq!(ProbeAction::item_records(), ["Workspace"]);
    assert_eq!(ProbeCall::NAMES, ["echo"]);
    assert_eq!(<Vec<Ws>>::schema_type(), "[Workspace]");
    assert_eq!(Urgency::schema_type(), "Urgency");
}

#[test]
fn records_and_enums_convert_by_name() {
    let w = Ws {
        id: 3,
        name: "web".into(),
        urgency: Urgency::VeryHigh,
        kind: Some("tiled".into()),
    };
    let d = w.to_data();
    assert_eq!(d.field("type"), Some(&Data::text("tiled")));
    assert_eq!(
        d.field("urgency"),
        Some(&Data::Enum {
            ty: "Urgency".into(),
            variant: "very_high".into()
        })
    );
    assert_eq!(Ws::from_data(&d), Ok(w.clone()));
    // A record without an optional field reads it as null.
    let partial = Data::Record {
        ty: "Workspace".into(),
        fields: vec![
            ("id".into(), Data::Int(1)),
            ("name".into(), Data::text("a")),
            ("urgency".into(), Urgency::Low.to_data()),
        ],
    };
    assert_eq!(Ws::from_data(&partial).unwrap().kind, None);
    let bad = d
        .with_path(&[Step::Field("id".into())], Data::text("x"))
        .unwrap();
    let e: DataError = Ws::from_data(&bad).unwrap_err();
    assert!(e.0.starts_with("id:"), "{e}");
    assert_eq!(w.key(), 3);
}

#[test]
fn calls_are_typed_from_name_item_and_arguments() {
    assert_eq!(
        ProbeAction::from_call("bump", None, &[]),
        Ok(ProbeAction::Bump)
    );
    assert_eq!(
        ProbeAction::from_call("focus", Some(&ws(2, "b").to_data()), &[]),
        Ok(ProbeAction::Focus { item: ws(2, "b") })
    );
    assert_eq!(
        ProbeAction::from_call("rename", None, &[Data::text("x"), Data::Int(4)]),
        Ok(ProbeAction::Rename("x".into(), 4))
    );
    assert!(ProbeAction::from_call("focus", None, &[]).is_err());
    assert!(ProbeAction::from_call("rename", None, &[Data::Int(1)]).is_err());
    assert!(ProbeAction::from_call("nope", None, &[]).is_err());
}

#[test]
fn diffs_carry_only_what_changed() {
    let old = Probe {
        level: 0.5,
        all: vec![ws(1, "a"), ws(2, "b")],
        ..Probe::default()
    };
    let mut new = old.clone();
    new.label = Some("hi".into());
    new.all.remove(0);
    new.all.push(ws(3, "c"));
    let mut patches = Vec::new();
    Probe::diff(&old, &new, &mut patches);
    assert_eq!(patches.len(), 2, "{patches:?}");
    assert_eq!(patches[0], ProbePatch::Label(Some("hi".into())));
    let ProbePatch::All(d) = &patches[1] else {
        panic!("{patches:?}")
    };
    assert!(
        d.iter().all(|d| !matches!(d, VecDiff::Reset { .. })),
        "{d:?}"
    );
    assert_eq!(patches[1].target(), Target::Field(3));
    assert_eq!(ProbePatch::Pinged(()).target(), Target::Event(1));
    let mut replay = old.clone();
    for p in &patches {
        replay.apply(p);
    }
    assert_eq!(replay, new);
    // A whole field as one patch.
    assert_eq!(new.field_patch(0), Some(ProbePatch::Level(0.5)));
    assert!(
        matches!(new.field_patch(3), Some(ProbePatch::All(d)) if matches!(d[0], VecDiff::Reset { .. }))
    );
    assert_eq!(new.field_patch(9), None);
}

#[test]
fn cells_apply_reports_and_ignore_echoes_of_local_writes() {
    let rt = Runtime::new();
    let cells = ProbeCells::new(&rt, "probe", &Probe::default());
    let gens: Arc<Mutex<Vec<strand_core::Generation>>> = Arc::default();
    let write = |v: f64| {
        let gens = gens.clone();
        cells
            .write(
                &rt,
                0,
                &Data::Float(v),
                Box::new(move |_, g| gens.lock().unwrap().push(g)),
            )
            .unwrap();
    };
    write(0.8);
    write(0.9);
    assert_eq!(cells.level.get_untracked(&rt), Ok(0.9));
    let g = gens.lock().unwrap().clone();
    assert_eq!(g.len(), 2);
    // The echo of the first write arrives late: ignored, the slider stays.
    let how = strand_services::How::Report(Some(g[0]));
    cells.apply(&rt, &ProbePatch::Level(0.8), how).unwrap();
    assert_eq!(cells.level.get_untracked(&rt), Ok(0.9));
    // The second's answer: the service settled on 0.9.
    let how = strand_services::How::Report(Some(g[1]));
    cells.apply(&rt, &ProbePatch::Level(0.9), how).unwrap();
    assert_eq!(cells.level.pending_writes(&rt), 0);
    // An outside change wins.
    let how = strand_services::How::Report(None);
    cells.apply(&rt, &ProbePatch::Level(0.3), how).unwrap();
    assert_eq!(cells.level.get_untracked(&rt), Ok(0.3));
    // A wrongly typed write is refused before anything is sent.
    assert!(
        cells
            .write(&rt, 0, &Data::text("x"), Box::new(|_, _| panic!("sent")))
            .is_err()
    );
    // Keyed lists apply diffs and say so; events say so too.
    let applied = cells
        .apply(
            &rt,
            &ProbePatch::All(vec![VecDiff::Insert {
                index: 0,
                key: 7,
                value: ws(7, "g"),
            }]),
            strand_services::How::Report(None),
        )
        .unwrap();
    assert!(matches!(
        applied,
        Some(Applied::Keyed { field: 3, ref diffs }) if diffs.len() == 1
    ));
    assert_eq!(
        cells.all.get_key(&rt, &7).unwrap().map(|w| w.name),
        Some("g".into())
    );
    let applied = cells
        .apply(
            &rt,
            &ProbePatch::Pair((4, "x".into())),
            strand_services::How::Report(None),
        )
        .unwrap();
    assert_eq!(
        applied,
        Some(Applied::Event {
            event: 2,
            args: vec![Data::Int(4), Data::text("x")]
        })
    );
    let snap = cells.snapshot(&rt).unwrap();
    assert_eq!(snap.level, 0.3);
    assert_eq!(snap.all, vec![ws(7, "g")]);
}

/// A registry whose waker counts.
fn services(rt: &Runtime) -> (Services, Arc<Mutex<u32>>) {
    let woken = Arc::new(Mutex::new(0u32));
    let w = woken.clone();
    let s = Services::new(rt, Buses::none(), move || *w.lock().unwrap() += 1);
    (s, woken)
}

/// Pump until `cond` holds (10 s at most).
fn until(rt: &Runtime, s: &Services, what: &str, cond: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        s.pump(rt);
        rt.flush();
        if cond() {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "never: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_service_patches_its_cells_through_the_shared_runtime() {
    let (sc, _guard) = script();
    let rt = Runtime::new();
    let (s, woken) = services(&rt);
    let probe = s.register::<Probe>(&rt);
    assert!(!probe.running() && !s.runtime_started());
    let dynamic = probe.dynamic();
    let events: Arc<Mutex<Vec<Applied>>> = Arc::default();
    let e = events.clone();
    dynamic.observe(Box::new(move |_, a| e.lock().unwrap().push(a.clone())));
    let typed = Arc::new(Mutex::new(Vec::new()));
    let t = typed.clone();
    probe
        .cells()
        .received
        .on(&rt, move |_, w: &Ws| {
            t.lock().unwrap().push(w.id);
            Ok(())
        })
        .unwrap();
    probe.acquire(&rt);
    assert!(probe.running() && s.runtime_started());
    assert_eq!(sc.next(), "start visible=true");
    // Not ready until it says so: the first frame's wait times out.
    assert!(!s.wait_ready(&rt, Duration::from_millis(50)));
    sc.cmd(Cmd::Update(Box::new(|p| {
        p.level = 0.25;
        p.all = vec![ws(1, "a"), ws(2, "b")];
    })));
    sc.cmd(Cmd::Ready);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert!(
        *woken.lock().unwrap() >= 2,
        "the host was woken per envelope"
    );
    assert_eq!(probe.cells().level.get_untracked(&rt), Ok(0.25));
    assert_eq!(dynamic.read(&rt, 0), Ok(Data::Float(0.25)));
    assert_eq!(
        dynamic.keyed_items(&rt, 3).unwrap(),
        vec![ws(1, "a").to_data(), ws(2, "b").to_data()]
    );
    // The first update's keyed change was mirrored too.
    assert!(matches!(
        events.lock().unwrap().as_slice(),
        [Applied::Keyed { field: 3, .. }]
    ));
    events.lock().unwrap().clear();
    // A keyed change by hand and events: mirrored to observers, and the
    // typed queue delivers on the next flush.
    sc.cmd(Cmd::Send(vec![ProbePatch::All(vec![VecDiff::Remove {
        index: 0,
        key: 1,
    }])]));
    sc.cmd(Cmd::Emit(ProbeEvent::Received(ws(9, "z"))));
    sc.cmd(Cmd::Emit(ProbeEvent::Pinged(())));
    until(&rt, &s, "the events", || typed.lock().unwrap().len() == 1);
    assert_eq!(*typed.lock().unwrap(), [9]);
    until(&rt, &s, "the ping", || events.lock().unwrap().len() >= 3);
    let ev = events.lock().unwrap().clone();
    assert!(
        matches!(&ev[0], Applied::Keyed { field: 3, diffs } if matches!(diffs[0], VecDiff::Remove { key: Data::Int(1), .. }))
    );
    assert_eq!(
        ev[1],
        Applied::Event {
            event: 0,
            args: vec![ws(9, "z").to_data()]
        }
    );
    assert_eq!(
        ev[2],
        Applied::Event {
            event: 1,
            args: vec![]
        }
    );
    // `fn` methods run on the logic thread over the cells.
    assert_eq!(
        dynamic.call(&rt, "named", &[Data::text("b")]),
        Some(Ok(Data::List(vec![ws(2, "b").to_data()])))
    );
    assert_eq!(dynamic.call(&rt, "nope", &[]), None);
    s.shutdown();
    assert!(!probe.running());
}

#[test]
fn writes_actions_and_async_calls_reach_the_service() {
    let (sc, _guard) = script();
    let rt = Runtime::new();
    let (s, _) = services(&rt);
    let probe = s.register::<Probe>(&rt);
    let dynamic = probe.dynamic();
    probe.acquire(&rt);
    assert_eq!(sc.next(), "start visible=true");
    sc.cmd(Cmd::Ready);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    // A write shows at once, goes to the service, and its clamped answer
    // replaces it.
    dynamic.write(&rt, 0, &[], Data::Float(1.5)).unwrap();
    assert_eq!(probe.cells().level.get_untracked(&rt), Ok(1.5));
    assert_eq!(sc.next(), "write level 1.5");
    until(&rt, &s, "the clamped level", || {
        probe.cells().level.get_untracked(&rt) == Ok(1.0)
    });
    // A leaf below a field: the service gets the leaf and the path.
    dynamic
        .write(&rt, 2, &[Step::Field("id".into())], Data::Int(5))
        .unwrap();
    assert_eq!(probe.cells().focus.get_untracked(&rt).unwrap().id, 5);
    assert_eq!(sc.next(), "write focus.id 5");
    // Actions, typed.
    dynamic.action(&rt, "bump", None, &[]).unwrap();
    assert_eq!(sc.next(), "action Bump");
    dynamic
        .action(&rt, "focus", Some(&ws(4, "d").to_data()), &[])
        .unwrap();
    assert_eq!(
        sc.next(),
        format!("action {:?}", ProbeAction::Focus { item: ws(4, "d") })
    );
    assert!(dynamic.action(&rt, "rename", None, &[]).is_err());
    // An async call completes with the service's answer.
    let fut = dynamic.fetch(&rt, "echo", &[Data::text("hi")]);
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert_eq!(tokio.block_on(fut), Ok(Data::text("HI")));
    let bad = dynamic.fetch(&rt, "echo", &[Data::Int(1)]);
    assert!(tokio.block_on(bad).is_err());
    s.shutdown();
    // Shut down: a call made now fails instead of hanging.
    let late = dynamic.fetch(&rt, "echo", &[Data::text("x")]);
    assert!(tokio.block_on(late).is_err());
}

#[test]
fn a_write_answer_tags_only_the_written_field() {
    let (sc, _guard) = script();
    let rt = Runtime::new();
    let (s, _) = services(&rt);
    let probe = s.register::<Probe>(&rt);
    let dynamic = probe.dynamic();
    probe.acquire(&rt);
    assert_eq!(sc.next(), "start visible=true");
    sc.cmd(Cmd::Ready);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    // Two writes of `gain`, both answered: its generations move on.
    for v in [1.0, 2.0] {
        dynamic.write(&rt, 4, &[], Data::Float(v)).unwrap();
        assert_eq!(sc.next(), format!("write gain {v}"));
        until(&rt, &s, "the gain's answer", || {
            probe.cells().gain.pending_writes(&rt) == 0
        });
    }
    // A write of `level` whose answer also moves `gain`: the gain's change
    // is an outside change, not an echo of anything.
    dynamic.write(&rt, 0, &[], Data::Float(0.25)).unwrap();
    assert_eq!(sc.next(), "write level 0.25");
    until(&rt, &s, "the gain the service set", || {
        probe.cells().gain.get_untracked(&rt) == Ok(42.0)
    });
    assert_eq!(probe.cells().level.get_untracked(&rt), Ok(0.25));
    s.shutdown();
}

#[test]
fn writes_and_actions_start_a_stopped_service_for_a_moment() {
    let (sc, _guard) = script();
    let rt = Runtime::new();
    let (s, _) = services(&rt);
    let probe = s.register::<Probe>(&rt);
    let dynamic = probe.dynamic();
    let now = std::cell::Cell::new(Duration::ZERO);
    let at = |d: Duration| {
        now.set(now.get() + d);
        rt.tick(now.get());
    };
    // `strand set brightness.level` with nothing reading it: started,
    // the write delivered, stopped 5 s later.
    dynamic.write(&rt, 0, &[], Data::Float(0.5)).unwrap();
    assert_eq!(sc.next(), "start visible=true");
    assert_eq!(sc.next(), "visible false");
    assert_eq!(sc.next(), "write level 0.5");
    assert_eq!(probe.readers(), 0);
    assert!(probe.running());
    // Still inside its grace: the action goes to the running service.
    dynamic.action(&rt, "bump", None, &[]).unwrap();
    assert_eq!(sc.next(), "action Bump");
    assert_eq!(probe.starts(), 1);
    at(STOP_GRACE + Duration::from_millis(1));
    assert!(!probe.running());
    assert_eq!(probe.stops(), 1);
    s.shutdown();
}

#[test]
fn a_stream_field_is_watched_only_while_a_visible_reader_reads_it() {
    let (sc, _guard) = script();
    let rt = Runtime::new();
    let (s, _) = services(&rt);
    let probe = s.register::<Probe>(&rt);
    // A bar reads `level` (a plain field: nobody is told).
    probe.acquire(&rt);
    probe.acquire_field(0);
    assert_eq!(sc.next(), "start visible=true");
    assert!(sc.quiet(), "a plain field's readers are not announced");
    // A popup opens reading `scan`: its stream starts; a second reader
    // changes nothing.
    probe.acquire(&rt);
    probe.acquire_field(5);
    assert_eq!(sc.next(), "watch scan true");
    probe.acquire_field(5);
    assert!(sc.quiet());
    probe.release_field(5);
    assert!(sc.quiet());
    // It closes: the stream stops at once, the service carries on.
    probe.release_field(5);
    probe.release(&rt);
    assert_eq!(sc.next(), "watch scan false");
    assert!(probe.running());
    assert_eq!(probe.field_readers(5), 0);
    // A run started while the field is read starts watching it.
    probe.release_field(0);
    probe.release(&rt);
    assert_eq!(sc.next(), "visible false");
    s.shutdown();
    let sc2 = script_again();
    probe.acquire(&rt);
    probe.acquire_field(5);
    assert_eq!(sc2.next(), "start visible=true");
    assert_eq!(sc2.next(), "watch scan true");
    probe.release_field(5);
    probe.release(&rt);
    s.shutdown();
    let sc3 = script_again();
    probe.acquire_field(5);
    probe.acquire(&rt);
    assert_eq!(sc3.next(), "start visible=true");
    assert_eq!(sc3.next(), "scan watched at start");
    s.shutdown();
}

#[test]
fn a_service_stops_five_seconds_after_its_last_reader() {
    let (sc, _guard) = script();
    let rt = Runtime::new();
    let (s, _) = services(&rt);
    let probe = s.register::<Probe>(&rt);
    let now = std::cell::Cell::new(Duration::ZERO);
    let at = |d: Duration| {
        now.set(now.get() + d);
        rt.tick(now.get());
    };
    probe.acquire(&rt);
    probe.acquire(&rt);
    assert_eq!(sc.next(), "start visible=true");
    assert_eq!((probe.readers(), probe.starts()), (2, 1));
    probe.release(&rt);
    // One reader left: still visible, no timer.
    assert!(sc.quiet());
    assert_eq!(rt.next_deadline(), None);
    probe.release(&rt);
    assert_eq!(sc.next(), "visible false");
    assert_eq!(rt.next_deadline(), Some(now.get() + STOP_GRACE));
    at(Duration::from_millis(4900));
    assert!(probe.running(), "still inside the grace");
    // Back inside the 5 s: the stop is cancelled, nothing restarts.
    probe.acquire(&rt);
    assert_eq!(sc.next(), "visible true");
    // The pending stop went with it: nothing wakes 5 s later.
    assert_eq!(rt.next_deadline(), None);
    at(Duration::from_secs(10));
    assert!(probe.running());
    assert_eq!((probe.starts(), probe.stops()), (1, 0));
    // The last reader again: 5 s on the logic clock, then it stops.
    probe.release(&rt);
    assert_eq!(sc.next(), "visible false");
    at(Duration::from_millis(4999));
    assert!(probe.running());
    at(Duration::from_millis(2));
    assert!(!probe.running());
    assert_eq!((probe.starts(), probe.stops()), (1, 1));
    // Nothing more is scheduled: an idle shell sleeps.
    assert_eq!(rt.next_deadline(), None);
    // A new reader starts it again, from the values it had.
    let sc2 = script_again();
    probe.acquire(&rt);
    assert_eq!(sc2.next(), "start visible=true");
    assert_eq!(probe.starts(), 2);
    // An unbalanced release is ignored.
    probe.release(&rt);
    probe.release(&rt);
    assert_eq!(probe.readers(), 0);
    s.shutdown();
}

/// A fresh script for a second run, under the lock the test holds.
fn script_again() -> Script {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let (seen_tx, seen) = std::sync::mpsc::channel();
    *CMDS.lock().unwrap() = Some(rx);
    *SEEN.lock().unwrap() = Some(seen_tx);
    Script { cmds: tx, seen }
}

#[test]
fn a_seeded_value_is_where_the_service_starts() {
    let (sc, _guard) = script();
    let rt = Runtime::new();
    let (s, _) = services(&rt);
    let probe = s.register::<Probe>(&rt);
    probe.seed(&rt, |p| p.level = 0.7).unwrap();
    assert_eq!(probe.cells().level.get_untracked(&rt), Ok(0.7));
    probe.acquire(&rt);
    assert_eq!(sc.next(), "start visible=true");
    // The service's copy starts at the seed: setting the same value
    // sends nothing; another value is sent.
    sc.cmd(Cmd::Update(Box::new(|p| {
        assert_eq!(p.level, 0.7);
        p.label = Some("seen".into());
    })));
    until(&rt, &s, "the label", || {
        probe.cells().label.get_untracked(&rt) == Ok(Some("seen".into()))
    });
    assert_eq!(probe.cells().level.get_untracked(&rt), Ok(0.7));
    s.shutdown();
}

/// A service on its own thread (as PipeWire's will be).
#[service(name = "threaded", schema = "service threaded { n: int }", thread)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Threaded {
    pub n: i64,
}

static NOTIFIED: Mutex<u32> = Mutex::new(0);

impl Threaded {
    fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        cx.set_notify(|| *NOTIFIED.lock().unwrap() += 1);
        cx.update(|t| t.n = 1);
        cx.ready();
        while let Some(m) = cx.blocking_recv() {
            if let Msg::Visible(false) = m {
                cx.update(|t| t.n += 1);
            }
        }
        assert!(cx.stopped());
        Ok(())
    }
}

#[test]
fn a_service_on_its_own_thread_speaks_the_same_protocol() {
    let rt = Runtime::new();
    let (s, _) = services(&rt);
    let t = s.register::<Threaded>(&rt);
    t.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert_eq!(t.cells().n.get_untracked(&rt), Ok(1));
    assert!(!s.runtime_started(), "no shared runtime for it");
    t.release(&rt);
    until(&rt, &s, "n = 2", || t.cells().n.get_untracked(&rt) == Ok(2));
    assert!(*NOTIFIED.lock().unwrap() >= 1);
    let before = *NOTIFIED.lock().unwrap();
    s.shutdown();
    assert!(!t.running());
    assert!(*NOTIFIED.lock().unwrap() > before, "told it stopped");
}

#[test]
fn errors_from_a_body_end_the_run_and_a_reader_restarts_it() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // No script: the probe's body fails at once.
    *CMDS.lock().unwrap() = None;
    let rt = Runtime::new();
    let (s, _) = services(&rt);
    let probe = s.register::<Probe>(&rt);
    let now = std::cell::Cell::new(Duration::ZERO);
    let at = |d: Duration| {
        now.set(now.get() + d);
        rt.tick(now.get());
    };
    probe.acquire(&rt);
    // Ended counts as ready: the first frame does not wait for it.
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert!(!probe.running(), "an ended body is not running");
    // Still read: started again after 1 s, then 2 s (backoff).
    assert_eq!(rt.next_deadline(), Some(now.get() + Duration::from_secs(1)));
    at(Duration::from_secs(1));
    assert_eq!(probe.starts(), 2);
    until(&rt, &s, "the second failure", || {
        rt.next_deadline() == Some(now.get() + Duration::from_secs(2))
    });
    // A run that comes up and says it is ready resets the backoff.
    let sc = script_again();
    at(Duration::from_secs(2));
    assert_eq!(sc.next(), "start visible=true");
    assert_eq!(probe.starts(), 3);
    assert!(probe.running());
    sc.cmd(Cmd::Ready);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    drop(sc);
    // Its script gone, the body returns Ok: no retry for a clean end.
    until(&rt, &s, "the clean end", || !probe.running());
    assert_eq!(rt.next_deadline(), None);
    probe.release(&rt);
    // A new reader starts an ended service at once.
    let sc = script_again();
    probe.acquire(&rt);
    assert_eq!(sc.next(), "start visible=true");
    assert_eq!(probe.starts(), 4);
    s.shutdown();
}

#[test]
fn keyed_helpers_dedupe_and_diff() {
    let kv = strand_services::keyed_vec_of(vec![ws(1, "a"), ws(1, "dup"), ws(2, "b")]);
    assert_eq!(kv.len(), 2);
    let d = strand_services::keyed_changes(&[ws(1, "a")], &[ws(1, "A")]);
    assert!(matches!(d[0], VecDiff::Update { key: 1, .. }));
    let mut items = vec![ws(1, "a")];
    strand_services::apply_keyed(&mut items, &d);
    assert_eq!(items, vec![ws(1, "A")]);
    let as_data = strand_services::diff_data(&d[0]);
    assert!(matches!(
        as_data,
        VecDiff::Update {
            key: Data::Int(1),
            ..
        }
    ));
}
