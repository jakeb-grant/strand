//! (bt-agent) `bluetooth`'s pairing agent against python-dbusmock's
//! `bluez5` template on a private bus. The template's `AgentManager1`
//! records the registrations; methods added to its root object call
//! Strand's `org.bluez.Agent1` from BlueZ's own bus name, as bluetoothd
//! does, and record how each call ended.
//!
//! The agent registers only while `request` is read (and unregisters
//! when the reader goes or the store stops); each kind of request shows
//! and is answered; an answer goes only to the request it was typed for;
//! only BlueZ is answered; and a request goes with its reader, its
//! pairing and BlueZ. Not shown here: BlueZ routing a `Pair` to the agent
//! its caller registered (the template never calls an agent itself).

mod support;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use strand_core::Runtime;
use strand_services::bluetooth::{
    AGENT_IFACE, AGENT_PATH, BLUEZ, Bluetooth, BluetoothRequest, BluetoothRequestKind, CAPABILITY,
};
use strand_services::testing::{DbusMock, PrivateBus};
use strand_services::{Builtin, Data, Services, Store, ToData};
use support::*;
use zbus::zvariant::{ObjectPath, OwnedValue};

const HEADPHONES: &str = "11:22:33:44:55:66";
const STRANGER: &str = "AA:BB:CC:DD:EE:FF";
const STRANGER_PATH: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";
const HEADPHONES_PATH: &str = "/org/bluez/hci0/dev_11_22_33_44_55_66";
const MANAGER: &str = "/org/bluez";
const MOCK_IFACE: &str = "org.freedesktop.DBus.Mock";

/// The mock's addition on `/`. `Ask(agent, key, method, device, text,
/// number, entered)` calls the agent's `method` from BlueZ's bus name with
/// the arguments it takes (a device path; a PIN or a service UUID as
/// `text`; a passkey as `number`; the digits typed as `entered`), and
/// records how it ended (`Outcome(key)`: "ok", "ok:<answer>", or the
/// error's name; "" while it runs).
const ASK: &str = "
self.outcomes = getattr(self, 'outcomes', {})
self.outcomes[args[1]] = ''
dev = dbus.ObjectPath(args[3])
sigs = {
    'RequestPinCode': ('o', (dev,)),
    'RequestPasskey': ('o', (dev,)),
    'DisplayPinCode': ('os', (dev, args[4])),
    'DisplayPasskey': ('ouq', (dev, dbus.UInt32(args[5]), dbus.UInt16(args[6]))),
    'RequestConfirmation': ('ou', (dev, dbus.UInt32(args[5]))),
    'RequestAuthorization': ('o', (dev,)),
    'AuthorizeService': ('os', (dev, args[4])),
    'Cancel': ('', ()),
    'Release': ('', ()),
}
sig, body = sigs[args[2]]
ok = lambda *r, s=self, k=args[1]: s.outcomes.__setitem__(k, 'ok' + ''.join(':' + str(x) for x in r))
err = lambda e, s=self, k=args[1]: s.outcomes.__setitem__(k, e.get_dbus_name())
self.connection.call_async(args[0], '/dev/strand/bluetooth/agent', 'org.bluez.Agent1', args[2], \
    sig, body, ok, err, timeout=600)
";
const OUTCOME: &str = "ret = getattr(self, 'outcomes', {}).get(args[0], 'never asked')";

/// BlueZ on `bus`: an adapter, the headphones paired, a stranger in
/// range, and the `Ask`/`Outcome` pair.
fn bluez(
    tokio: &tokio::runtime::Runtime,
    bus: &PrivateBus,
    conn: &zbus::Connection,
) -> Option<DbusMock> {
    let m = DbusMock::start(bus, "bluez5", true, None, BLUEZ)?;
    let mock_iface = |method: &str, body: &(&str, &str)| {
        call(tokio, conn, BLUEZ, "/", "org.bluez.Mock", method, body)
    };
    mock_iface("AddAdapter", &("hci0", "my-laptop"));
    for (address, name) in [(HEADPHONES, "Headphones"), (STRANGER, "Stranger")] {
        call(
            tokio,
            conn,
            BLUEZ,
            "/",
            "org.bluez.Mock",
            "AddDevice",
            &("hci0", address, name),
        );
    }
    mock_iface("PairDevice", &("hci0", HEADPHONES));
    let add = |name: &str, ins: &str, outs: &str, code: &str| {
        mock(
            tokio,
            conn,
            BLUEZ,
            "/",
            "AddMethod",
            &(MOCK_IFACE, name, ins, outs, code),
        );
    };
    add("Ask", "sssssuq", "", ASK);
    add("Outcome", "s", "s", OUTCOME);
    Some(m)
}

fn field(name: &str) -> usize {
    Bluetooth::FIELDS
        .iter()
        .position(|f| f.name == name)
        .unwrap()
}

/// A private bus with BlueZ, the services, and the test's own connection.
struct World {
    rt: Runtime,
    services: Services,
    b: Builtin,
    conn: zbus::Connection,
    tokio: tokio::runtime::Runtime,
    mock: Option<DbusMock>,
    failures: Arc<Mutex<Vec<(String, String)>>>,
    bus: PrivateBus,
}

impl World {
    fn start() -> Option<World> {
        let bus = PrivateBus::start()?;
        let tokio = tokio();
        let conn = connect(&tokio, &bus.address);
        let mock = bluez(&tokio, &bus, &conn)?;
        let rt = Runtime::new();
        let (services, b) = services(&rt, bus.buses());
        let failures = Arc::new(Mutex::new(Vec::new()));
        let f = failures.clone();
        b.bluetooth.dynamic().observe(Box::new(move |_, a| {
            if let strand_services::Applied::Event { args, .. } = a
                && let [Data::Text(address), Data::Text(error)] = args.as_slice()
            {
                f.lock()
                    .unwrap()
                    .push((address.to_string(), error.to_string()));
            }
        }));
        b.bluetooth.acquire(&rt);
        assert!(services.wait_ready(&rt, Duration::from_secs(10)));
        Some(World {
            rt,
            services,
            b,
            conn,
            tokio,
            mock: Some(mock),
            failures,
            bus,
        })
    }

    /// `start`, with `request` read and the agent registered.
    fn reading() -> Option<World> {
        let w = World::start()?;
        w.b.bluetooth.acquire_field(field("request"));
        w.until("registered", |w| w.calls("RequestDefaultAgent").len() == 1);
        Some(w)
    }

    fn until(&self, what: &str, cond: impl Fn(&World) -> bool) {
        until(&self.rt, &self.services, what, || cond(self));
    }

    /// Pump for `ms` milliseconds.
    fn settle(&self, ms: u64) {
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(ms) {
            self.services.pump(&self.rt);
            self.rt.flush();
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The agent manager's calls of `method`.
    fn calls(&self, method: &str) -> Vec<Vec<OwnedValue>> {
        mock_calls(&self.tokio, &self.conn, BLUEZ, MANAGER, method)
    }

    fn serving(&self) -> Option<String> {
        serving(&self.tokio, &self.conn, AGENT_PATH, AGENT_IFACE)
    }

    /// The unique name serving Strand's agent.
    fn agent(&self) -> String {
        self.serving()
            .unwrap_or_else(|| panic!("no bus name serves Strand's agent"))
    }

    fn request(&self) -> Option<BluetoothRequest> {
        self.b
            .bluetooth
            .cells()
            .request
            .get_untracked(&self.rt)
            .unwrap()
    }

    /// The request shown once `cond` holds of it.
    fn shown(&self, what: &str, cond: impl Fn(&BluetoothRequest) -> bool) -> BluetoothRequest {
        self.until(what, |w| w.request().is_some_and(|r| cond(&r)));
        self.request().unwrap()
    }

    /// BlueZ calls the agent's `method` (recorded as `key`).
    fn ask(&self, key: &str, method: &str, device: &str, text: &str, number: u32, entered: u16) {
        let agent = self.agent();
        mock(
            &self.tokio,
            &self.conn,
            BLUEZ,
            "/",
            "Ask",
            &(agent.as_str(), key, method, device, text, number, entered),
        );
    }

    fn outcome(&self, key: &str) -> String {
        mock(&self.tokio, &self.conn, BLUEZ, "/", "Outcome", &(key,))
            .body()
            .deserialize()
            .unwrap()
    }

    fn until_outcome(&self, key: &str, want: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.services.pump(&self.rt);
            self.rt.flush();
            let got = self.outcome(key);
            if got == want {
                return;
            }
            assert!(Instant::now() < deadline, "{key}: {got:?}, not {want:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// `r.<name>(args…)`.
    fn act(&self, name: &str, r: &BluetoothRequest, args: &[Data]) {
        self.b
            .bluetooth
            .dynamic()
            .action(&self.rt, name, Some(&r.to_data()), args, Default::default())
            .unwrap();
        self.rt.flush();
    }

    fn failures(&self) -> Vec<(String, String)> {
        self.failures.lock().unwrap().clone()
    }

    fn warnings(&self) -> Vec<String> {
        self.services
            .take_diagnostics()
            .into_iter()
            .filter(|d| d.service == "bluetooth")
            .map(|d| d.message)
            .collect()
    }
}

fn text(s: &str) -> Data {
    Data::Text(s.into())
}

fn path_of(v: &OwnedValue) -> String {
    let p: ObjectPath<'_> = v.downcast_ref().unwrap();
    p.to_string()
}

#[test]
fn the_agent_registers_only_while_a_request_is_read() {
    let Some(w) = World::start() else {
        return;
    };
    // `bluetooth` runs, `request` unread: no agent.
    w.settle(300);
    assert!(w.calls("RegisterAgent").is_empty());
    assert_eq!(w.serving(), None);
    assert_eq!(w.request(), None);

    // Read: exported, registered as a keyboard and display, the default.
    let request = field("request");
    w.b.bluetooth.acquire_field(request);
    w.until("registered", |w| w.calls("RequestDefaultAgent").len() == 1);
    let reg = w.calls("RegisterAgent");
    assert_eq!(reg.len(), 1);
    assert_eq!(path_of(&reg[0][0]), AGENT_PATH);
    assert_eq!(
        String::try_from(reg[0][1].try_clone().unwrap()).unwrap(),
        CAPABILITY
    );
    assert_eq!(CAPABILITY, "KeyboardDisplay");
    assert_eq!(path_of(&w.calls("RequestDefaultAgent")[0][0]), AGENT_PATH);
    assert!(w.serving().is_some());

    // The reader goes: unregistered and taken down.
    w.b.bluetooth.release_field(request);
    w.until("unregistered", |w| w.calls("UnregisterAgent").len() == 1);
    assert_eq!(path_of(&w.calls("UnregisterAgent")[0][0]), AGENT_PATH);
    w.until("taken down", |w| w.serving().is_none());

    // A second reader: registered again.
    w.b.bluetooth.acquire_field(request);
    w.until("registered again", |w| {
        w.calls("RegisterAgent").len() == 2 && w.calls("RequestDefaultAgent").len() == 2
    });
    assert!(w.serving().is_some());

    // Kept read until the store stops: unregistered as it stops.
    w.b.bluetooth.release(&w.rt);
    w.rt.tick(strand_services::STOP_GRACE + Duration::from_millis(1));
    w.until("unregistered as it stopped", |w| {
        w.calls("UnregisterAgent").len() == 2
    });
    w.until("taken down as it stopped", |w| w.serving().is_none());

    // BlueZ refuses it: one warning saying so, not asked again.
    mock(
        &w.tokio,
        &w.conn,
        BLUEZ,
        MANAGER,
        "AddMethod",
        &(
            "org.bluez.AgentManager1",
            "RegisterAgent",
            "os",
            "",
            "raise dbus.exceptions.DBusException('No way', name='org.bluez.Error.Failed')",
        ),
    );
    let _ = w.warnings();
    w.b.bluetooth.acquire(&w.rt);
    w.until("refused", |w| w.calls("RegisterAgent").len() == 3);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut warned = Vec::new();
    while warned.is_empty() {
        assert!(Instant::now() < deadline, "no warning");
        w.settle(50);
        warned = w.warnings();
    }
    w.settle(500);
    warned.extend(w.warnings());
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert!(
        warned[0].contains("BlueZ refused Strand's pairing agent") && warned[0].contains("No way"),
        "{warned:?}"
    );
    assert_eq!(w.calls("RegisterAgent").len(), 3);
    assert_eq!(w.calls("RequestDefaultAgent").len(), 2);
    w.services.shutdown();
}

#[test]
fn a_pin_or_passkey_is_typed_by_the_shell() {
    let Some(w) = World::reading() else {
        return;
    };
    w.ask("p1", "RequestPinCode", STRANGER_PATH, "", 0, 0);
    let r = w.shown("a PIN asked", |r| r.kind == BluetoothRequestKind::Pin);
    assert_eq!(r.device.address, STRANGER);
    assert_eq!(r.device.name, "Stranger");
    assert_eq!(
        (r.code.as_str(), r.entered, r.service.as_str()),
        ("", 0, "")
    );
    assert_eq!(w.outcome("p1"), "", "the call waits");

    // Too long: not sent, `failed` says why, the request stays.
    w.act("respond", &r, &[text(&"1".repeat(17))]);
    w.until("the misfit", |w| !w.failures().is_empty());
    assert_eq!(
        w.failures()[0],
        (
            STRANGER.to_string(),
            "a PIN is 1 to 16 characters".to_string()
        )
    );
    assert_eq!(w.request(), Some(r.clone()));
    assert_eq!(w.outcome("p1"), "");
    w.act("respond", &r, &[text("0000")]);
    w.until_outcome("p1", "ok:0000");
    w.until("gone", |w| w.request().is_none());

    // A passkey.
    w.ask("k1", "RequestPasskey", STRANGER_PATH, "", 0, 0);
    let r = w.shown("a passkey asked", |r| {
        r.kind == BluetoothRequestKind::Passkey
    });
    w.act("respond", &r, &[text("12a")]);
    w.until("the second misfit", |w| w.failures().len() == 2);
    assert_eq!(
        w.failures()[1],
        (
            STRANGER.to_string(),
            "a passkey is up to six digits".to_string()
        )
    );
    assert_eq!(w.outcome("k1"), "");
    w.act("respond", &r, &[text("012345")]);
    w.until_outcome("k1", "ok:12345");
    w.until("gone", |w| w.request().is_none());
    w.services.shutdown();
}

#[test]
fn a_code_is_confirmed_shown_or_a_service_authorized() {
    let Some(w) = World::reading() else {
        return;
    };
    use BluetoothRequestKind as K;
    // A code to compare.
    w.ask("c1", "RequestConfirmation", STRANGER_PATH, "", 4213, 0);
    let r = w.shown("a confirmation", |r| r.kind == K::Confirm);
    assert_eq!(r.code, "004213");
    w.act("accept", &r, &[]);
    w.until_outcome("c1", "ok");
    w.until("gone", |w| w.request().is_none());

    // Nothing to compare, refused.
    w.ask("a1", "RequestAuthorization", STRANGER_PATH, "", 0, 0);
    let r = w.shown("an authorization", |r| r.kind == K::Confirm);
    assert_eq!(r.code, "");
    w.act("reject", &r, &[]);
    w.until_outcome("a1", "org.bluez.Error.Rejected");
    w.until("gone", |w| w.request().is_none());

    // A service, by name.
    w.ask(
        "s1",
        "AuthorizeService",
        HEADPHONES_PATH,
        "0000110b-0000-1000-8000-00805f9b34fb",
        0,
        0,
    );
    let r = w.shown("a service", |r| r.kind == K::Service);
    assert_eq!(r.service, "Audio Sink");
    assert_eq!(r.device.name, "Headphones");
    w.act("accept", &r, &[]);
    w.until_outcome("s1", "ok");
    w.until("gone", |w| w.request().is_none());

    // A PIN to show: answered at once, shown until it ends.
    w.ask("d1", "DisplayPinCode", STRANGER_PATH, "1234", 0, 0);
    w.until_outcome("d1", "ok");
    let r = w.shown("a PIN shown", |r| r.kind == K::Display);
    assert_eq!(r.code, "1234");
    // A passkey to show, its digits typed one by one: one request.
    w.ask("d2", "DisplayPasskey", STRANGER_PATH, "", 42, 0);
    w.until_outcome("d2", "ok");
    w.shown("a passkey shown", |r| r.code == "000042" && r.entered == 0);
    w.ask("d3", "DisplayPasskey", STRANGER_PATH, "", 42, 3);
    let r = w.shown("three typed", |r| r.entered == 3);
    assert_eq!((r.kind, r.code.as_str()), (K::Display, "000042"));
    w.ask("x1", "Cancel", STRANGER_PATH, "", 0, 0);
    w.until_outcome("x1", "ok");
    w.until("cancelled", |w| w.request().is_none());

    // An answer of the wrong kind changes nothing.
    w.ask("c2", "RequestConfirmation", STRANGER_PATH, "", 7, 0);
    let r = w.shown("a confirmation", |r| r.code == "000007");
    w.act("respond", &r, &[text("1234")]);
    w.settle(300);
    assert_eq!(w.outcome("c2"), "");
    assert_eq!(w.request(), Some(r));
    w.ask("p1", "RequestPinCode", STRANGER_PATH, "", 0, 0);
    w.until_outcome("c2", "org.bluez.Error.Canceled");
    let r = w.shown("a PIN asked", |r| r.kind == K::Pin);
    w.act("accept", &r, &[]);
    w.settle(300);
    assert_eq!(w.outcome("p1"), "");
    assert_eq!(w.request(), Some(r));
    assert!(w.failures().is_empty(), "{:?}", w.failures());
    w.services.shutdown();
}

#[test]
fn an_answer_goes_only_to_the_request_it_was_typed_for() {
    let Some(w) = World::reading() else {
        return;
    };
    w.ask("A", "RequestConfirmation", STRANGER_PATH, "", 111_111, 0);
    let a = w.shown("A", |r| r.code == "111111");
    // BlueZ moves on to B: A is cancelled.
    w.ask("B", "RequestConfirmation", STRANGER_PATH, "", 222_222, 0);
    w.until_outcome("A", "org.bluez.Error.Canceled");
    let b = w.shown("B", |r| r.code == "222222");
    // Accepting A, which the user saw, does not accept B.
    w.act("accept", &a, &[]);
    w.settle(300);
    assert_eq!(w.outcome("B"), "");
    assert_eq!(w.request(), Some(b.clone()));
    w.act("accept", &b, &[]);
    w.until_outcome("B", "ok");
    w.until("gone", |w| w.request().is_none());

    // A PIN typed for a request BlueZ cancelled is sent nowhere.
    w.ask("P", "RequestPinCode", STRANGER_PATH, "", 0, 0);
    let p = w.shown("P", |r| r.kind == BluetoothRequestKind::Pin);
    w.ask("x", "Cancel", STRANGER_PATH, "", 0, 0);
    w.until_outcome("P", "org.bluez.Error.Canceled");
    w.until("cancelled", |w| w.request().is_none());
    w.act("respond", &p, &[text("0000")]);
    w.settle(300);
    assert_eq!(w.request(), None);
    assert!(w.failures().is_empty(), "{:?}", w.failures());
    w.services.shutdown();
}

#[test]
fn only_bluez_is_answered() {
    let Some(w) = World::reading() else {
        return;
    };
    let agent = w.agent();
    let device = ObjectPath::try_from(STRANGER_PATH).unwrap();
    let r = w.tokio.block_on(w.conn.call_method(
        Some(agent.as_str()),
        AGENT_PATH,
        Some(AGENT_IFACE),
        "RequestPinCode",
        &(device,),
    ));
    match r {
        Err(zbus::Error::MethodError(name, _, _)) => {
            assert_eq!(name.as_str(), "org.bluez.Error.Rejected")
        }
        other => panic!("{other:?}"),
    }
    w.settle(200);
    assert_eq!(w.request(), None);
    w.services.shutdown();
}

#[test]
fn a_shown_request_goes_with_its_reader_its_pairing_or_bluez() {
    let Some(mut w) = World::reading() else {
        return;
    };
    let request = field("request");
    // The reader goes: refused, then unregistered.
    w.ask("p1", "RequestPinCode", STRANGER_PATH, "", 0, 0);
    w.shown("a PIN asked", |r| r.kind == BluetoothRequestKind::Pin);
    w.b.bluetooth.release_field(request);
    w.until_outcome("p1", "org.bluez.Error.Canceled");
    w.until("unregistered", |w| w.calls("UnregisterAgent").len() == 1);
    assert_eq!(w.request(), None);

    // Read again: a passkey shown goes when its device has paired.
    w.b.bluetooth.acquire_field(request);
    w.until("registered again", |w| {
        w.calls("RequestDefaultAgent").len() == 2
    });
    w.ask("d1", "DisplayPasskey", STRANGER_PATH, "", 42, 0);
    w.shown("a passkey shown", |r| {
        r.kind == BluetoothRequestKind::Display
    });
    call(
        &w.tokio,
        &w.conn,
        BLUEZ,
        "/",
        "org.bluez.Mock",
        "PairDevice",
        &("hci0", STRANGER),
    );
    w.until("paired: gone", |w| w.request().is_none());

    // BlueZ restarts: the request goes, and the new BlueZ is registered
    // with.
    w.ask("p2", "RequestPinCode", HEADPHONES_PATH, "", 0, 0);
    w.shown("a PIN asked", |r| r.device.address == HEADPHONES);
    drop(w.mock.take());
    w.until("BlueZ gone: gone", |w| w.request().is_none());
    w.mock = bluez(&w.tokio, &w.bus, &w.conn);
    assert!(w.mock.is_some());
    w.until("registered with the new BlueZ", |w| {
        w.calls("RequestDefaultAgent").len() == 1
    });
    assert_eq!(w.calls("RegisterAgent").len(), 1);
    assert_eq!(w.b.bluetooth.starts(), 1);
    w.services.shutdown();
}
