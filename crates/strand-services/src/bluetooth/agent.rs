//! (bt-agent) Strand's BlueZ pairing agent: `org.bluez.Agent1` at
//! [`AGENT_PATH`] on the system connection the `bluetooth` store shares
//! (the one `d.pair()` calls `Device1.Pair` on, so BlueZ routes the
//! pairings Strand starts to it), registered with `AgentManager1` as
//! [`CAPABILITY`] and asked to be BlueZ's default agent, so pairings a
//! device starts and service authorizations reach it too.
//!
//! **When.** Only while a config reads `bluetooth.request` (a
//! `#[store(stream)]` field): [`Registration::sync`] exports and registers
//! it when a reader comes and BlueZ runs (once per BlueZ owner and reader:
//! a refusal is a warning, not asked again on every signal), and
//! unregisters it and takes it down when the reader goes. A stopped store
//! does the same on a task of its own ([`TEARDOWN`]), which the next run
//! awaits before it exports again. BlueZ keeps a stack of default agents,
//! so unregistering hands the default back to the agent before.
//!
//! **Requests.** Each method BlueZ calls (only from its own unique name:
//! anyone else is refused `org.bluez.Error.Rejected`) is sent to the store
//! as an [`Asked`], and the ones that want an answer wait for it. The store
//! keeps one, [`Pairing`]: BlueZ asks an agent one thing at a time, so a
//! newer request replaces the shown one, which is answered
//! `org.bluez.Error.Canceled`; a `DisplayPasskey` for the device and code
//! shown only counts the digits typed. A request is withdrawn (answered
//! `Canceled`) when BlueZ cancels or releases, restarts, the reader goes, or
//! [`REQUEST_TIMEOUT`] passes with no answer; a `display` also when its
//! pairing ends ([`Pairing::display_ended`]). Refusing a `display`
//! ([`Answered::DisplayRefused`]) cancels its pairing only when the shell's
//! own `pair()` started it: BlueZ's `CancelPairing` of a device with no
//! pairing of its caller's running removes the device's keys.
//!
//! **Answers.** A `respond`, `accept` or `reject` carries the request it
//! was called on, and applies only to the shown one (same device, kind,
//! code and service): one typed for a request that went is dropped, so a
//! `confirm` never accepts a code that was not shown. A typed answer is a
//! [`Password`] from the call on, checked against its kind ([`parse`]) and
//! handed to BlueZ from a buffer wiped when dropped ([`Pin`]).

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use strand_auth::Password;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};
use zeroize::Zeroizing;

use super::{BLUEZ, BluetoothAction, BluetoothDevice, BluetoothRequest, BluetoothRequestKind};
use crate::dbus;

/// Where Strand's pairing agent is exported.
pub const AGENT_PATH: &str = "/dev/strand/bluetooth/agent";
/// The agent's interface.
pub const AGENT_IFACE: &str = "org.bluez.Agent1";
/// BlueZ's agent manager: its interface and object.
pub const AGENT_MANAGER: &str = "org.bluez.AgentManager1";
/// The agent manager's object.
pub const MANAGER_PATH: &str = "/org/bluez";
/// What the agent can do: show a code and type one (a shell has a screen
/// and a keyboard), so BlueZ may compare numbers with a phone, show a
/// passkey for a keyboard and ask a legacy PIN.
pub const CAPABILITY: &str = "KeyboardDisplay";
/// How long a request waits for an answer before it is withdrawn: BlueZ's
/// own agent timeout, so a `Cancel` that never comes leaves no dialog up.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Why an answer did not fit a `pin` request: BlueZ counts a PIN in
/// bytes (16 letters or digits, fewer of other characters).
pub const PIN_MISFIT: &str = "a PIN is 1 to 16 bytes";
/// Why an answer did not fit a `passkey` request.
pub const PASSKEY_MISFIT: &str = "a passkey is up to six digits";

/// BlueZ's errors, as agents answer them.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
pub enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Rejected(String),
    Canceled(String),
}

/// What a request is answered with.
pub enum Reply {
    /// A PIN (`RequestPinCode`).
    Pin(Zeroizing<String>),
    /// A passkey (`RequestPasskey`).
    Passkey(u32),
    /// Allowed (`RequestConfirmation`, `RequestAuthorization`,
    /// `AuthorizeService`).
    Done,
}

/// Where a request's answer goes.
type Answer = oneshot::Sender<Result<Reply, AgentError>>;

/// What BlueZ asked of the agent.
pub enum Asked {
    /// A request to show (and answer, when it has a reply).
    Request(Box<Asking>),
    /// `Cancel()`: BlueZ gave up on the request it made.
    Cancel,
    /// `Release()`: BlueZ dropped the agent.
    Release,
}

/// A request BlueZ made.
pub struct Asking {
    /// The device's object.
    pub path: String,
    pub kind: BluetoothRequestKind,
    pub code: String,
    pub entered: i64,
    pub service: String,
    /// `None` for `display`, which BlueZ wants answered at once.
    pub reply: Option<Answer>,
}

/// A PIN handed to BlueZ, from a buffer wiped when dropped (zbus's own
/// copy in the reply it sends is beyond reach).
pub struct Pin(Zeroizing<String>);

impl serde::Serialize for Pin {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl zbus::zvariant::Type for Pin {
    const SIGNATURE: &'static zbus::zvariant::Signature = &zbus::zvariant::Signature::Str;
}

/// BlueZ's current unique name, for checking callers.
pub type Owner = Arc<Mutex<Option<String>>>;

/// The exported `org.bluez.Agent1`.
struct Agent {
    tx: mpsc::UnboundedSender<Asked>,
    bluez: Owner,
}

/// What a stopped store answers.
const STOPPED: &str = "Strand's pairing agent stopped";

impl Agent {
    /// Refuses a call from anyone but BlueZ.
    fn sent_by_bluez(&self, h: &zbus::message::Header<'_>) -> Result<(), AgentError> {
        let owner = self
            .bluez
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        match (owner, h.sender()) {
            (Some(o), Some(s)) if o == s.as_str() => Ok(()),
            _ => Err(AgentError::Rejected("only BlueZ is answered".into())),
        }
    }

    /// Shows a request and waits for its answer.
    async fn ask(
        &self,
        h: &zbus::message::Header<'_>,
        device: OwnedObjectPath,
        kind: BluetoothRequestKind,
        code: String,
        service: String,
    ) -> Result<Reply, AgentError> {
        self.sent_by_bluez(h)?;
        let (tx, rx) = oneshot::channel();
        let asking = Asking {
            path: device.to_string(),
            kind,
            code,
            entered: 0,
            service,
            reply: Some(tx),
        };
        if self.tx.send(Asked::Request(Box::new(asking))).is_err() {
            return Err(AgentError::Canceled(STOPPED.into()));
        }
        rx.await
            .unwrap_or_else(|_| Err(AgentError::Canceled(STOPPED.into())))
    }

    /// Shows a code to type on the device: answered at once.
    fn display(
        &self,
        h: &zbus::message::Header<'_>,
        device: OwnedObjectPath,
        code: String,
        entered: i64,
    ) -> Result<(), AgentError> {
        self.sent_by_bluez(h)?;
        let asking = Asking {
            path: device.to_string(),
            kind: BluetoothRequestKind::Display,
            code,
            entered,
            service: String::new(),
            reply: None,
        };
        // A stopped store shows nothing; BlueZ wants no answer anyway.
        let _ = self.tx.send(Asked::Request(Box::new(asking)));
        Ok(())
    }
}

/// An answer of the wrong kind (a passkey to a PIN request).
fn mismatch() -> AgentError {
    AgentError::Rejected("the answer did not fit the request".into())
}

#[zbus::interface(name = "org.bluez.Agent1")]
impl Agent {
    /// BlueZ dropped the agent (it unregistered it, or is stopping).
    fn release(&self, #[zbus(header)] h: zbus::message::Header<'_>) -> Result<(), AgentError> {
        self.sent_by_bluez(&h)?;
        let _ = self.tx.send(Asked::Release);
        Ok(())
    }

    /// A PIN to type (legacy pairing).
    async fn request_pin_code(
        &self,
        #[zbus(header)] h: zbus::message::Header<'_>,
        device: OwnedObjectPath,
    ) -> Result<Pin, AgentError> {
        let kind = BluetoothRequestKind::Pin;
        match self
            .ask(&h, device, kind, String::new(), String::new())
            .await?
        {
            Reply::Pin(p) => Ok(Pin(p)),
            _ => Err(mismatch()),
        }
    }

    /// A PIN to show while it is typed on the device.
    fn display_pin_code(
        &self,
        #[zbus(header)] h: zbus::message::Header<'_>,
        device: OwnedObjectPath,
        pincode: String,
    ) -> Result<(), AgentError> {
        self.display(&h, device, pincode, 0)
    }

    /// A passkey to type.
    async fn request_passkey(
        &self,
        #[zbus(header)] h: zbus::message::Header<'_>,
        device: OwnedObjectPath,
    ) -> Result<u32, AgentError> {
        let kind = BluetoothRequestKind::Passkey;
        match self
            .ask(&h, device, kind, String::new(), String::new())
            .await?
        {
            Reply::Passkey(n) => Ok(n),
            _ => Err(mismatch()),
        }
    }

    /// A passkey to show while it is typed on the device, with how many
    /// of its digits have been typed.
    fn display_passkey(
        &self,
        #[zbus(header)] h: zbus::message::Header<'_>,
        device: OwnedObjectPath,
        passkey: u32,
        entered: u16,
    ) -> Result<(), AgentError> {
        self.display(&h, device, passkey_code(passkey), i64::from(entered))
    }

    /// Whether to pair, the device showing `passkey` too.
    async fn request_confirmation(
        &self,
        #[zbus(header)] h: zbus::message::Header<'_>,
        device: OwnedObjectPath,
        passkey: u32,
    ) -> Result<(), AgentError> {
        let kind = BluetoothRequestKind::Confirm;
        self.ask(&h, device, kind, passkey_code(passkey), String::new())
            .await
            .map(drop)
    }

    /// Whether to pair, with nothing to compare.
    async fn request_authorization(
        &self,
        #[zbus(header)] h: zbus::message::Header<'_>,
        device: OwnedObjectPath,
    ) -> Result<(), AgentError> {
        let kind = BluetoothRequestKind::Confirm;
        self.ask(&h, device, kind, String::new(), String::new())
            .await
            .map(drop)
    }

    /// Whether the device may use service `uuid`.
    async fn authorize_service(
        &self,
        #[zbus(header)] h: zbus::message::Header<'_>,
        device: OwnedObjectPath,
        uuid: String,
    ) -> Result<(), AgentError> {
        let kind = BluetoothRequestKind::Service;
        self.ask(&h, device, kind, String::new(), service_name(&uuid))
            .await
            .map(drop)
    }

    /// BlueZ gave up on its request (the device did, or it timed out).
    fn cancel(&self, #[zbus(header)] h: zbus::message::Header<'_>) -> Result<(), AgentError> {
        self.sent_by_bluez(&h)?;
        let _ = self.tx.send(Asked::Cancel);
        Ok(())
    }
}

// ---- codes, answers and names ----------------------------------------------

/// A passkey as BlueZ's own agents show it: six digits, zero-padded.
pub fn passkey_code(passkey: u32) -> String {
    format!("{passkey:06}")
}

/// `answer` as the reply to a request of `kind`, or why it does not fit:
/// a PIN is 1 to 16 bytes (no NUL), a passkey 1 to 6 ASCII digits.
pub fn parse(kind: BluetoothRequestKind, answer: &Password) -> Result<Reply, &'static str> {
    let b = answer.as_bytes();
    match kind {
        BluetoothRequestKind::Passkey => {
            if b.is_empty() || b.len() > 6 || !b.iter().all(u8::is_ascii_digit) {
                return Err(PASSKEY_MISFIT);
            }
            let n = b.iter().fold(0u32, |n, d| n * 10 + u32::from(d - b'0'));
            Ok(Reply::Passkey(n))
        }
        _ => {
            if b.is_empty() || b.len() > 16 || b.contains(&0) {
                return Err(PIN_MISFIT);
            }
            let s = std::str::from_utf8(b).map_err(|_| PIN_MISFIT)?;
            let mut pin = Zeroizing::new(String::with_capacity(s.len()));
            pin.push_str(s);
            Ok(Reply::Pin(pin))
        }
    }
}

/// The Bluetooth base UUID's tail: a 16-bit service class is
/// `0000xxxx` followed by it.
const BASE_UUID: &str = "-0000-1000-8000-00805f9b34fb";

/// A service by the name the Bluetooth SIG gives its class, for the ones a
/// pairing dialog meets; any other UUID as given.
pub fn service_name(uuid: &str) -> String {
    let lower = uuid.to_ascii_lowercase();
    let class = lower
        .strip_suffix(BASE_UUID)
        .and_then(|h| h.strip_prefix("0000"))
        .filter(|h| h.len() == 4)
        .and_then(|h| u16::from_str_radix(h, 16).ok());
    let name = match class {
        Some(0x1101) => "Serial Port",
        Some(0x1105) => "Object Push",
        Some(0x1106) => "File Transfer",
        Some(0x1108) => "Headset",
        Some(0x110a) => "Audio Source",
        Some(0x110b) => "Audio Sink",
        Some(0x110c | 0x110e) => "Remote Control",
        Some(0x1112) => "Headset Gateway",
        Some(0x1115) => "Personal Area Network",
        Some(0x1116) => "Network Access Point",
        Some(0x111e) => "Hands-Free",
        Some(0x111f) => "Hands-Free Gateway",
        Some(0x1124 | 0x1812) => "Human Interface Device",
        Some(0x112f) => "Phonebook Access",
        Some(0x1132) => "Message Access",
        _ => return uuid.to_string(),
    };
    name.to_string()
}

/// The address a device object's path names (`…/dev_AA_BB_CC_DD_EE_FF`),
/// for a device BlueZ has not listed yet; "" for another path.
pub fn address_of_path(path: &str) -> String {
    path.rsplit('/')
        .next()
        .and_then(|last| last.strip_prefix("dev_"))
        .map(|a| a.replace('_', ":"))
        .unwrap_or_default()
}

/// Whether `item` is the request `shown`: the same device, kind, code and
/// service (the digits typed on a keyboard may have moved on).
pub fn same(shown: &BluetoothRequest, item: &BluetoothRequest) -> bool {
    shown.device.address == item.device.address
        && shown.kind == item.kind
        && shown.code == item.code
        && shown.service == item.service
}

// ---- the shown request -----------------------------------------------------

/// The request shown, with its answer and when it is withdrawn.
pub struct Shown {
    /// As shown (its `device` rebuilt from BlueZ's objects on each
    /// publish).
    pub request: BluetoothRequest,
    /// Its device's object.
    pub path: String,
    /// Whether its device was paired when it came (a `display` ends when
    /// its device becomes paired).
    pub paired: bool,
    /// Whether BlueZ has listed its device since it came: a `display` ends
    /// when its device goes only once it was there to go (a device pairing
    /// before Strand has read it listed waits for [`REQUEST_TIMEOUT`]).
    pub listed: bool,
    reply: Option<Answer>,
    deadline: Instant,
}

impl Shown {
    /// Answers it (a `display` has nothing to answer).
    fn answer(mut self, r: Result<Reply, AgentError>) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(r);
        }
    }
}

/// What an action did to the shown request.
#[derive(Debug, PartialEq, Eq)]
pub enum Answered {
    /// Nothing: it was not the shown request's, or not for its kind.
    Ignored,
    /// Answered: the request went.
    Gone,
    /// The answer did not fit: `failed(address, why)`, the request stays.
    Misfit(String, &'static str),
    /// A `display` was refused: the request went. The pairing of the
    /// device at `path` (`address`) is to be cancelled if the shell's own
    /// `pair()` of it runs, and left alone otherwise.
    DisplayRefused { path: String, address: String },
}

/// The one request BlueZ has asked and the shell has not answered.
#[derive(Default)]
pub struct Pairing {
    shown: Option<Shown>,
}

impl Pairing {
    /// The request shown.
    pub fn shown(&self) -> Option<&Shown> {
        self.shown.as_ref()
    }

    /// When the shown request is withdrawn unanswered.
    pub fn deadline(&self) -> Option<Instant> {
        self.shown.as_ref().map(|s| s.deadline)
    }

    /// BlueZ asks `a` of `device` at `now`, the device paired or not as it
    /// comes (`None` when BlueZ has not listed it yet): it replaces the
    /// shown request, which is answered `Canceled`, unless it is the shown
    /// `display` again with more digits typed.
    pub fn ask(&mut self, a: Asking, device: BluetoothDevice, paired: Option<bool>, now: Instant) {
        if let Some(s) = &mut self.shown
            && s.request.kind == BluetoothRequestKind::Display
            && a.kind == BluetoothRequestKind::Display
            && s.path == a.path
            && s.request.code == a.code
        {
            s.request.entered = a.entered;
            return;
        }
        self.withdraw("another request came");
        self.shown = Some(Shown {
            request: BluetoothRequest {
                device,
                kind: a.kind,
                code: a.code,
                entered: a.entered,
                service: a.service,
            },
            path: a.path,
            paired: paired.unwrap_or(false),
            listed: paired.is_some(),
            reply: a.reply,
            deadline: now + REQUEST_TIMEOUT,
        });
    }

    /// Withdraws the shown request, answering it `Canceled` with `why`;
    /// whether one was shown.
    pub fn withdraw(&mut self, why: &str) -> bool {
        match self.shown.take() {
            Some(s) => {
                log::debug!("bluetooth: a pairing request withdrawn: {why}");
                s.answer(Err(AgentError::Canceled(why.to_string())));
                true
            }
            None => false,
        }
    }

    /// Whether the shown request is a `display` whose pairing has ended,
    /// its device now `paired` as BlueZ lists it (`None`: not listed): it
    /// became paired, or it went after it was listed. A device not listed
    /// yet is noted when it is.
    pub fn display_ended(&mut self, paired: Option<bool>) -> bool {
        let Some(s) = &mut self.shown else {
            return false;
        };
        if s.request.kind != BluetoothRequestKind::Display {
            return false;
        }
        match paired {
            None => s.listed,
            Some(now) => {
                s.listed = true;
                now && !s.paired
            }
        }
    }

    /// Withdraws the shown request if its deadline has passed at `now`.
    pub fn expire(&mut self, now: Instant) -> bool {
        if self.deadline().is_some_and(|d| d <= now) {
            return self.withdraw("it was not answered within a minute");
        }
        false
    }

    /// The config's `respond`, `accept` or `reject`, applied to the shown
    /// request when it is the one it was called on (an answer typed for
    /// one that went is dropped, and wiped).
    pub fn answer(&mut self, a: BluetoothAction) -> Answered {
        use BluetoothRequestKind as K;
        let Some(s) = &self.shown else {
            return Answered::Ignored;
        };
        let (item, said) = match a {
            BluetoothAction::Respond { item, answer } => (item, Said::Respond(answer)),
            BluetoothAction::Accept { item } => (item, Said::Accept),
            BluetoothAction::Reject { item } => (item, Said::Reject),
            _ => return Answered::Ignored,
        };
        if !same(&s.request, &item) {
            log::info!("bluetooth: an answer for a pairing request no longer shown was dropped");
            return Answered::Ignored;
        }
        let kind = s.request.kind;
        let reply = match (said, kind) {
            (Said::Respond(answer), K::Pin | K::Passkey) => match parse(kind, &answer) {
                Ok(r) => Ok(r),
                Err(why) => return Answered::Misfit(s.request.device.address.clone(), why),
            },
            (Said::Accept, K::Confirm | K::Service) => Ok(Reply::Done),
            (Said::Reject, K::Display) => {
                let refused = Answered::DisplayRefused {
                    path: s.path.clone(),
                    address: s.request.device.address.clone(),
                };
                self.shown = None;
                return refused;
            }
            (Said::Reject, _) => Err(AgentError::Rejected("refused".into())),
            (Said::Respond(_), _) | (Said::Accept, _) => {
                log::debug!("bluetooth: an answer that does not fit a {kind:?} request ignored");
                return Answered::Ignored;
            }
        };
        if let Some(s) = self.shown.take() {
            s.answer(reply);
        }
        Answered::Gone
    }
}

/// Which request action was called.
enum Said {
    Respond(Password),
    Accept,
    Reject,
}

// ---- registration ----------------------------------------------------------

/// The last stopped store's teardown, awaited by the next run before it
/// exports its own agent.
static TEARDOWN: Mutex<Option<tokio::task::JoinHandle<()>>> = Mutex::new(None);

/// The agent exported and registered with BlueZ, as a reader of
/// `request` wants: dropped with the store's body (returned from, or
/// stopped mid-await), it unregisters and takes the agent down on a task
/// of its own, so BlueZ never calls an agent that is gone.
pub struct Registration {
    conn: zbus::Connection,
    tx: mpsc::UnboundedSender<Asked>,
    bluez: Owner,
    exported: bool,
    registered: bool,
    /// The BlueZ registered with (or refused by) since a reader came: not
    /// asked again on every signal.
    tried: Option<String>,
}

impl Registration {
    /// Nothing exported yet; the agent sends what BlueZ asks to `tx`, and
    /// answers only `bluez`.
    pub fn new(conn: &zbus::Connection, tx: mpsc::UnboundedSender<Asked>, bluez: Owner) -> Self {
        Registration {
            conn: conn.clone(),
            tx,
            bluez,
            exported: false,
            registered: false,
            tried: None,
        }
    }

    /// Exports and registers the agent while `wanted` and BlueZ runs (each
    /// BlueZ once), unregisters and takes it down when not `wanted`; a
    /// warning to raise when BlueZ refused it.
    pub async fn sync(&mut self, wanted: bool) -> Option<String> {
        let owner = self
            .bluez
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if !wanted {
            self.down(owner.is_some()).await;
            return None;
        }
        let Some(owner) = owner else {
            // BlueZ went: its registrations went with it.
            self.registered = false;
            self.tried = None;
            return None;
        };
        if !self.exported {
            let prev = TEARDOWN
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            if let Some(t) = prev {
                let _ = t.await;
            }
            let server = self.conn.object_server();
            let _ = server.remove::<Agent, _>(AGENT_PATH).await;
            let agent = Agent {
                tx: self.tx.clone(),
                bluez: self.bluez.clone(),
            };
            if let Err(e) = server.at(AGENT_PATH, agent).await {
                return Some(format!("Strand's pairing agent could not be exported: {e}"));
            }
            self.exported = true;
        }
        if self.tried.as_deref() == Some(owner.as_str()) {
            return None;
        }
        self.tried = Some(owner);
        self.registered = false;
        let Ok(path) = ObjectPath::try_from(AGENT_PATH) else {
            return None;
        };
        let r = manager(&self.conn, "RegisterAgent", &(path.clone(), CAPABILITY)).await;
        match r {
            Ok(()) => {}
            Err(e) if super::is_error(&e, "org.bluez.Error.AlreadyExists") => {}
            Err(e) => {
                return Some(format!(
                    "BlueZ refused Strand's pairing agent, so pairing codes go to the \
                     system's agent: {}",
                    super::bluez_error(e)
                ));
            }
        }
        self.registered = true;
        log::info!("bluetooth: Strand is BlueZ's pairing agent");
        if let Err(e) = manager(&self.conn, "RequestDefaultAgent", &(path,)).await {
            return Some(format!(
                "BlueZ did not make Strand's pairing agent its default one, so only the \
                 pairings Strand starts reach it: {}",
                super::bluez_error(e)
            ));
        }
        None
    }

    /// BlueZ released the agent: it is no longer registered (and not
    /// registered again until BlueZ restarts or a reader comes again).
    pub fn released(&mut self) {
        self.registered = false;
    }

    /// Unregisters the agent (while BlueZ runs) and takes it down.
    async fn down(&mut self, bluez_runs: bool) {
        self.tried = None;
        if std::mem::take(&mut self.registered)
            && bluez_runs
            && let Ok(path) = ObjectPath::try_from(AGENT_PATH)
        {
            match manager(&self.conn, "UnregisterAgent", &(path,)).await {
                Ok(()) => log::info!("bluetooth: Strand's pairing agent unregistered"),
                Err(e) => log::debug!("bluetooth: unregistering the pairing agent: {e}"),
            }
        }
        if std::mem::take(&mut self.exported) {
            let _ = self
                .conn
                .object_server()
                .remove::<Agent, _>(AGENT_PATH)
                .await;
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if !self.exported && !self.registered {
            return;
        }
        let conn = self.conn.clone();
        let registered = self.registered;
        let task = async move {
            if registered && let Ok(path) = ObjectPath::try_from(AGENT_PATH) {
                match manager(&conn, "UnregisterAgent", &(path,)).await {
                    Ok(()) => log::info!("bluetooth: Strand's pairing agent unregistered"),
                    Err(e) => log::debug!("bluetooth: unregistering the pairing agent: {e}"),
                }
            }
            let _ = conn.object_server().remove::<Agent, _>(AGENT_PATH).await;
        };
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                let t = h.spawn(task);
                *TEARDOWN.lock().unwrap_or_else(PoisonError::into_inner) = Some(t);
            }
            // No runtime (the process ends): BlueZ drops the agent with
            // its connection.
            Err(_) => log::debug!("bluetooth: no runtime to unregister the pairing agent on"),
        }
    }
}

/// `method` of BlueZ's agent manager, bounded.
async fn manager<B>(conn: &zbus::Connection, method: &str, body: &B) -> zbus::Result<()>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    dbus::timed_for(
        dbus::CALL_TIMEOUT,
        conn.call_method(Some(BLUEZ), MANAGER_PATH, Some(AGENT_MANAGER), method, body),
    )
    .await
    .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pw(s: &str) -> Password {
        Password::from_bytes(s.as_bytes().to_vec())
    }

    #[test]
    fn answers_fit_their_kind() {
        let pin = |s: &str| match parse(BluetoothRequestKind::Pin, &pw(s)) {
            Ok(Reply::Pin(p)) => Ok(p.to_string()),
            Ok(_) => panic!("not a PIN"),
            Err(e) => Err(e),
        };
        assert_eq!(pin("0000"), Ok("0000".to_string()));
        assert_eq!(pin("a"), Ok("a".to_string()));
        assert_eq!(pin(&"x".repeat(16)), Ok("x".repeat(16)));
        assert_eq!(pin(""), Err(PIN_MISFIT));
        assert_eq!(pin(&"x".repeat(17)), Err(PIN_MISFIT));
        // Counted in bytes, as BlueZ does: eight two-byte letters fit,
        // nine do not.
        assert_eq!(pin(&"é".repeat(8)), Ok("é".repeat(8)));
        assert_eq!(pin(&"é".repeat(9)), Err(PIN_MISFIT));
        assert_eq!(pin("a\0b"), Err(PIN_MISFIT));
        let passkey = |s: &str| match parse(BluetoothRequestKind::Passkey, &pw(s)) {
            Ok(Reply::Passkey(n)) => Ok(n),
            Ok(_) => panic!("not a passkey"),
            Err(e) => Err(e),
        };
        assert_eq!(passkey("012345"), Ok(12345));
        assert_eq!(passkey("999999"), Ok(999_999));
        assert_eq!(passkey("7"), Ok(7));
        for bad in ["", "1234567", "12a", " 12"] {
            assert_eq!(passkey(bad), Err(PASSKEY_MISFIT), "{bad:?}");
        }
    }

    #[test]
    fn passkeys_show_as_six_digits() {
        assert_eq!(passkey_code(42), "000042");
        assert_eq!(passkey_code(999_999), "999999");
        assert_eq!(passkey_code(0), "000000");
    }

    #[test]
    fn services_are_named_where_known() {
        assert_eq!(
            service_name("0000110b-0000-1000-8000-00805f9b34fb"),
            "Audio Sink"
        );
        assert_eq!(
            service_name("0000110B-0000-1000-8000-00805F9B34FB"),
            "Audio Sink"
        );
        assert_eq!(
            service_name("00001812-0000-1000-8000-00805f9b34fb"),
            "Human Interface Device"
        );
        assert_eq!(
            service_name("0000111e-0000-1000-8000-00805f9b34fb"),
            "Hands-Free"
        );
        let unknown = "6E400001-B5A3-F393-E0A9-E50E24DCCA9E";
        assert_eq!(service_name(unknown), unknown);
        assert_eq!(service_name("110b"), "110b");
    }

    #[test]
    fn a_path_names_its_device() {
        assert_eq!(
            address_of_path("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF"),
            "AA:BB:CC:DD:EE:FF"
        );
        assert_eq!(address_of_path("/org/bluez/hci0"), "");
    }

    fn request(address: &str, kind: BluetoothRequestKind, code: &str) -> BluetoothRequest {
        BluetoothRequest {
            device: BluetoothDevice {
                address: address.into(),
                ..BluetoothDevice::default()
            },
            kind,
            code: code.into(),
            entered: 0,
            service: String::new(),
        }
    }

    #[test]
    fn an_item_matches_the_shown_request_but_for_entered() {
        use BluetoothRequestKind as K;
        let shown = request("AA:BB:CC:DD:EE:FF", K::Confirm, "111111");
        assert!(same(&shown, &shown.clone()));
        let mut typed = shown.clone();
        typed.entered = 3;
        typed.device.name = "Phone".into();
        assert!(
            same(&shown, &typed),
            "the digits typed and its name move on"
        );
        assert!(!same(
            &shown,
            &request("AA:BB:CC:DD:EE:FF", K::Confirm, "222222")
        ));
        assert!(!same(
            &shown,
            &request("AA:BB:CC:DD:EE:FF", K::Display, "111111")
        ));
        assert!(!same(
            &shown,
            &request("11:22:33:44:55:66", K::Confirm, "111111")
        ));
        let mut other = shown.clone();
        other.service = "Audio Sink".into();
        assert!(!same(&shown, &other));
    }

    fn asking(
        kind: BluetoothRequestKind,
        code: &str,
    ) -> (Asking, oneshot::Receiver<Result<Reply, AgentError>>) {
        let (tx, rx) = oneshot::channel();
        let a = Asking {
            path: "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF".into(),
            kind,
            code: code.into(),
            entered: 0,
            service: String::new(),
            reply: Some(tx),
        };
        (a, rx)
    }

    fn device() -> BluetoothDevice {
        BluetoothDevice {
            address: "AA:BB:CC:DD:EE:FF".into(),
            ..BluetoothDevice::default()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_unanswered_for_a_minute_is_withdrawn() {
        let mut p = Pairing::default();
        let (a, mut rx) = asking(BluetoothRequestKind::Pin, "");
        p.ask(a, device(), Some(false), Instant::now());
        tokio::time::sleep(REQUEST_TIMEOUT - Duration::from_secs(1)).await;
        assert!(!p.expire(Instant::now()), "not yet");
        assert!(rx.try_recv().is_err(), "still waiting");
        let deadline = p.deadline().unwrap();
        tokio::time::sleep_until(deadline).await;
        assert!(p.expire(Instant::now()));
        assert!(p.shown().is_none());
        assert!(matches!(rx.await, Ok(Err(AgentError::Canceled(_)))));
    }

    #[test]
    fn a_newer_request_replaces_the_shown_one() {
        use BluetoothRequestKind as K;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let _in = rt.enter();
        let now = Instant::now();
        let mut p = Pairing::default();
        let (a, mut first) = asking(K::Confirm, "111111");
        p.ask(a, device(), Some(false), now);
        let item = p.shown().unwrap().request.clone();
        let (b, mut second) = asking(K::Confirm, "222222");
        p.ask(b, device(), Some(false), now);
        assert!(matches!(first.try_recv(), Ok(Err(AgentError::Canceled(_)))));
        // The first one's item answers nothing.
        assert_eq!(
            p.answer(BluetoothAction::Accept { item }),
            Answered::Ignored
        );
        assert!(second.try_recv().is_err());
        // A `respond` to a confirm, ignored; its `accept`, answered.
        let item = p.shown().unwrap().request.clone();
        assert_eq!(
            p.answer(BluetoothAction::Respond {
                item: item.clone(),
                answer: pw("1234"),
            }),
            Answered::Ignored
        );
        assert_eq!(p.answer(BluetoothAction::Accept { item }), Answered::Gone);
        assert!(matches!(second.try_recv(), Ok(Ok(Reply::Done))));
        // A display counts the digits typed; its reject withdraws it.
        let display = |entered| Asking {
            path: "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF".into(),
            kind: K::Display,
            code: "000042".into(),
            entered,
            service: String::new(),
            reply: None,
        };
        p.ask(display(0), device(), Some(false), now);
        p.ask(display(3), device(), Some(false), now);
        let item = p.shown().unwrap().request.clone();
        assert_eq!(item.entered, 3);
        assert_eq!(
            p.answer(BluetoothAction::Reject { item }),
            Answered::DisplayRefused {
                path: "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF".into(),
                address: "AA:BB:CC:DD:EE:FF".into(),
            }
        );
        assert!(p.shown().is_none());
        // A PIN that does not fit stays asked.
        let (c, mut third) = asking(K::Pin, "");
        p.ask(c, device(), Some(false), now);
        let item = p.shown().unwrap().request.clone();
        assert_eq!(
            p.answer(BluetoothAction::Respond {
                item: item.clone(),
                answer: pw(&"9".repeat(17)),
            }),
            Answered::Misfit("AA:BB:CC:DD:EE:FF".into(), PIN_MISFIT)
        );
        assert!(third.try_recv().is_err());
        assert_eq!(
            p.answer(BluetoothAction::Respond {
                item,
                answer: pw("0000"),
            }),
            Answered::Gone
        );
        assert!(matches!(third.try_recv(), Ok(Ok(Reply::Pin(p))) if p.as_str() == "0000"));
    }

    #[test]
    fn a_display_ends_when_its_device_pairs_or_goes_once_listed() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let _in = rt.enter();
        let now = Instant::now();
        let display = || Asking {
            path: "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF".into(),
            kind: BluetoothRequestKind::Display,
            code: "000042".into(),
            entered: 0,
            service: String::new(),
            reply: None,
        };
        // Listed and unpaired: it ends when the device pairs, or goes.
        let mut p = Pairing::default();
        p.ask(display(), device(), Some(false), now);
        assert!(!p.display_ended(Some(false)));
        assert!(p.display_ended(Some(true)));
        assert!(p.display_ended(None));
        // Paired already (pairing again): only going ends it.
        let mut p = Pairing::default();
        p.ask(display(), device(), Some(true), now);
        assert!(!p.display_ended(Some(true)));
        assert!(p.display_ended(None));
        // Not listed yet: not listed is not gone, until it was listed.
        let mut p = Pairing::default();
        p.ask(display(), device(), None, now);
        assert!(!p.display_ended(None));
        assert!(!p.display_ended(Some(false)), "listed now");
        assert!(p.display_ended(None), "went once listed");
        // Not listed yet, then listed paired: its pairing ended.
        let mut p = Pairing::default();
        p.ask(display(), device(), None, now);
        assert!(p.display_ended(Some(true)));
        // Only a display ends this way.
        let mut p = Pairing::default();
        let (a, _rx) = asking(BluetoothRequestKind::Confirm, "111111");
        p.ask(a, device(), Some(false), now);
        assert!(!p.display_ended(None));
        assert!(!p.display_ended(Some(true)));
        assert!(!Pairing::default().display_ended(None));
    }

    #[test]
    fn an_answer_never_shows_in_debug() {
        let a = BluetoothAction::Respond {
            item: BluetoothRequest::default(),
            answer: pw("1234"),
        };
        let shown = format!("{a:?}");
        assert!(shown.contains("answer: Password(<4 bytes>)"), "{shown}");
        assert!(!shown.contains("1234"), "{shown}");
    }
}
