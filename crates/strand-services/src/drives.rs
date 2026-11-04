//! `drives`: removable drives and their filesystems, from UDisks2
//! (`org.freedesktop.UDisks2`) on the system bus (decisions.md drives).
//!
//! UDisks2 is an object manager: the service reads `GetManagedObjects` on
//! `/org/freedesktop/UDisks2`, then follows `InterfacesAdded` and
//! `InterfacesRemoved` there and the `PropertiesChanged` of the three
//! interfaces it shows ([`Daemon`]). Each of those is a match rule with
//! the interface as `arg0` ([`dbus::properties_rule`]), so the bus itself
//! drops the rest: `Drive.Ata` SMART refreshes and `Loop`/`Swapspace`
//! property changes never reach the process; block and filesystem changes
//! of any device (internal ones and loop devices too) do, and change
//! nothing in the state. Nothing polls. Only the drive, block, filesystem
//! and partition interfaces are kept.
//!
//! [`Drives::from_objects`] makes the state: a drive UDisks2 calls
//! `Removable` with its medium in, keyed by its whole-disk block device,
//! with the filesystems on it (an unlocked encrypted one's through its
//! `CryptoBackingDevice`). UDisks2 is D-Bus-activated when not running,
//! as GVfs and udiskie start it; restarting is read afresh; missing, it
//! lists nothing and is not `available`.
//!
//! `v.mount()` and `v.unmount()` are the filesystem's `Mount` and
//! `Unmount` with no options: UDisks2 picks the mount point, and polkit
//! may ask (the session's agent, Strand's while a config reads `polkit`).
//! `d.eject()` is safe removal ([`eject_plan`]): unmount every mounted
//! filesystem, lock what was unlocked, then one last step, never both:
//! `Eject` where the medium comes out of the drive (a disc, a card: the
//! drive stays powered for the next one), else `PowerOff` where the drive
//! can be (a stick, a disk), else `Eject`. GVfs maps `g_drive_eject` to
//! `Drive.Eject` and `g_drive_stop` to `Drive.PowerOff`, and a file
//! manager calls one or the other: a card reader or USB disc drive
//! reports both, and powering it off after its medium would take every
//! slot with it, deaf to the next card or disc until it is plugged in
//! again. The first step refused stops it. Each runs as a task
//! with no timeout (a polkit dialog may stay open as long as the user
//! leaves it, as for `session`'s power actions); one started outlives the
//! body ([`Calls`]), so a menu closing on its click (its service stopping
//! 5 s later) never leaves a drive unmounted and still powered. A call
//! repeating one in flight (a double click) is dropped. A failure is
//! `failed(device, error)` in UDisks2's words.

use std::collections::{BTreeMap, HashMap};

use tokio::task::JoinSet;
use zbus::zvariant::{ObjectPath, Value};

use crate::dbus::{self, Daemon, DaemonEvent, Props};
use crate::{Call, Cx, Event, Msg, ServiceError, Store, service};

/// The schema the `drives` service serves.
pub const SCHEMA: &str = strand_services_schema::DRIVES;

/// UDisks2's bus name.
pub const UDISKS: &str = "org.freedesktop.UDisks2";
/// UDisks2's object manager, under which every object of its lives.
const ROOT: &str = "/org/freedesktop/UDisks2";
const DRIVE: &str = "org.freedesktop.UDisks2.Drive";
const BLOCK: &str = "org.freedesktop.UDisks2.Block";
const FILESYSTEM: &str = "org.freedesktop.UDisks2.Filesystem";
const PARTITION: &str = "org.freedesktop.UDisks2.Partition";
const ENCRYPTED: &str = "org.freedesktop.UDisks2.Encrypted";
const OBJECT_MANAGER: &str = "org.freedesktop.DBus.ObjectManager";
/// The interfaces kept of each object: all the state needs.
const KEPT: [&str; 4] = [DRIVE, BLOCK, FILESYSTEM, PARTITION];

/// A filesystem on a removable drive, mounted or not.
#[derive(crate::Data, Clone, Debug, Default, PartialEq)]
#[data(name = "Volume", key = device)]
pub struct Volume {
    pub device: String,
    pub label: Option<String>,
    pub size: f64,
    pub mounted: bool,
    pub path: Option<String>,
}

/// A removable drive with its medium in.
#[derive(crate::Data, Clone, Debug, Default, PartialEq)]
#[data(name = "Drive", key = device)]
pub struct Drive {
    pub device: String,
    pub name: String,
    pub size: f64,
    pub icon: String,
    pub can_eject: bool,
    pub volumes: Vec<Volume>,
}

/// `drives`' actions.
#[derive(Call, Debug)]
pub enum DrivesAction {
    /// `v.mount()`.
    Mount { item: Volume },
    /// `v.unmount()`.
    Unmount { item: Volume },
    /// `d.eject()`.
    Eject { item: Drive },
}

impl DrivesAction {
    /// The device it acts on (the volume's or the drive's) and what it
    /// asks.
    fn target(&self) -> (&str, Ask) {
        match self {
            DrivesAction::Mount { item } => (&item.device, Ask::Mount),
            DrivesAction::Unmount { item } => (&item.device, Ask::Unmount),
            DrivesAction::Eject { item } => (&item.device, Ask::Eject),
        }
    }
}

/// See the module docs.
#[service(name = "drives", action = DrivesAction, refused = refused)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Drives {
    /// Every removable drive with its medium in, keyed by `device`: those
    /// there when UDisks2 started first, then in the order plugged in.
    #[store(keyed)]
    pub all: Vec<Drive>,
    /// UDisks2 answers. False without it (or without a system bus), so a
    /// menu can hide what cannot work.
    pub available: bool,
    /// Mounting, unmounting or ejecting failed (UDisks2 or polkit refused
    /// it, a filesystem was busy, UDisks2 or the system bus was missing):
    /// `on drives.failed(device, error) { … }`. `device` is the volume's for
    /// `mount()` and `unmount()` and the drive's for `eject()`; `error` is
    /// what UDisks2 says.
    pub failed: Event<(String, String)>,
}

/// An action the service could not take (no system bus, its run backing
/// off): `failed(device, error)`, as UDisks2 refusing it is.
fn refused(action: &DrivesAction, error: &str) -> Option<DrivesEvent> {
    let (device, _) = action.target();
    Some(DrivesEvent::Failed((device.to_string(), error.to_string())))
}

/// UDisks2's objects: path, then interface, then properties.
type Objects = BTreeMap<String, HashMap<String, Props>>;

/// What an action asks of UDisks2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ask {
    Mount,
    Unmount,
    Eject,
}

/// One call to UDisks2, on the object at its path.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Step {
    /// `Filesystem.Mount`.
    Mount(String),
    /// `Filesystem.Unmount`.
    Unmount(String),
    /// `Encrypted.Lock` of the encrypted block whose cleartext was
    /// unmounted.
    Lock(String),
    /// `Drive.Eject`.
    Eject(String),
    /// `Drive.PowerOff`.
    PowerOff(String),
}

impl Step {
    /// The object, its interface and the method.
    fn call(&self) -> (&str, &'static str, &'static str) {
        match self {
            Step::Mount(p) => (p, FILESYSTEM, "Mount"),
            Step::Unmount(p) => (p, FILESYSTEM, "Unmount"),
            Step::Lock(p) => (p, ENCRYPTED, "Lock"),
            Step::Eject(p) => (p, DRIVE, "Eject"),
            Step::PowerOff(p) => (p, DRIVE, "PowerOff"),
        }
    }
}

/// The bytes of an `ay` value.
fn bytes_of(v: &Value<'_>) -> Option<Vec<u8>> {
    match v {
        Value::Value(inner) => bytes_of(inner),
        Value::Array(a) => a
            .iter()
            .map(|b| match b {
                Value::U8(b) => Some(*b),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// An `ay` as text, without the NUL UDisks2 ends it with; `None` when
/// empty.
fn text_of_bytes(bytes: &[u8]) -> Option<String> {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    let s = String::from_utf8_lossy(&bytes[..end]).into_owned();
    (!s.is_empty()).then_some(s)
}

/// An `ay` property (`Device`, `PreferredDevice`) as text.
fn bytestring(p: &Props, name: &str) -> Option<String> {
    text_of_bytes(&bytes_of(p.get(name)?)?)
}

/// `MountPoints` (`aay`): every mount point, as text.
fn mount_points(p: &Props) -> Vec<String> {
    let Some(v) = p.get("MountPoints") else {
        return Vec::new();
    };
    let v: &Value<'_> = v;
    let v = match v {
        Value::Value(inner) => &**inner,
        v => v,
    };
    match v {
        Value::Array(a) => a
            .iter()
            .filter_map(|m| text_of_bytes(&bytes_of(m)?))
            .collect(),
        _ => Vec::new(),
    }
}

/// An object path property (`Drive`, `CryptoBackingDevice`), `None` for
/// `/`.
fn object_path(p: &Props, name: &str) -> Option<String> {
    let path: ObjectPath<'_> = p.get(name)?.downcast_ref().ok()?;
    let s = path.to_string();
    (s != "/").then_some(s)
}

/// A block's device: its `PreferredDevice`, else its `Device`.
fn block_device(b: &Props) -> Option<String> {
    bytestring(b, "PreferredDevice").or_else(|| bytestring(b, "Device"))
}

/// The drive a block is on: its own `Drive`, or for an unlocked
/// encrypted (cleartext) block, its backing block's.
fn block_drive(objects: &Objects, b: &Props) -> Option<String> {
    object_path(b, "Drive").or_else(|| {
        let backing = object_path(b, "CryptoBackingDevice")?;
        object_path(objects.get(&backing)?.get(BLOCK)?, "Drive")
    })
}

/// A block's partition number: its own, or its backing block's for a
/// cleartext one; 0 for neither.
fn partition_number(objects: &Objects, path: &str) -> f64 {
    let Some(ifaces) = objects.get(path) else {
        return 0.0;
    };
    if let Some(n) = ifaces
        .get(PARTITION)
        .and_then(|p| dbus::number(p, "Number"))
    {
        return n;
    }
    ifaces
        .get(BLOCK)
        .and_then(|b| object_path(b, "CryptoBackingDevice"))
        .and_then(|backing| objects.get(&backing)?.get(PARTITION))
        .and_then(|p| dbus::number(p, "Number"))
        .unwrap_or(0.0)
}

/// A drive is listed: removable, with its medium in.
fn listed(d: &Props) -> bool {
    let flag = |name| dbus::boolean(d, name).unwrap_or(false);
    flag("Removable") && (flag("MediaAvailable") || !flag("MediaRemovable"))
}

/// An icon name for the drive's kind.
fn drive_icon(d: &Props) -> String {
    let media = dbus::text(d, "Media").unwrap_or_default();
    if dbus::boolean(d, "Optical").unwrap_or(false) {
        "media-optical-symbolic"
    } else if media.starts_with("flash") || media.starts_with("thumb") {
        "media-flash-symbolic"
    } else {
        "drive-removable-media-symbolic"
    }
    .to_string()
}

/// The device of drive `path`'s whole-disk block (no partition).
fn drive_device(objects: &Objects, path: &str) -> Option<String> {
    objects.values().find_map(|ifaces| {
        let b = ifaces.get(BLOCK)?;
        if ifaces.contains_key(PARTITION) || object_path(b, "Drive").as_deref() != Some(path) {
            return None;
        }
        block_device(b)
    })
}

/// The filesystems on drive `path`: (object path, block, filesystem), in
/// partition order, then by device; hidden ones too.
fn filesystems<'o>(objects: &'o Objects, path: &str) -> Vec<(&'o str, &'o Props, &'o Props)> {
    let mut all: Vec<(&str, &Props, &Props)> = objects
        .iter()
        .filter_map(|(p, ifaces)| {
            let b = ifaces.get(BLOCK)?;
            let fs = ifaces.get(FILESYSTEM)?;
            (block_drive(objects, b).as_deref() == Some(path)).then_some((p.as_str(), b, fs))
        })
        .collect();
    all.sort_by(|a, b| {
        partition_number(objects, a.0)
            .total_cmp(&partition_number(objects, b.0))
            .then_with(|| block_device(a.1).cmp(&block_device(b.1)))
    });
    all
}

impl Drives {
    /// The state UDisks2's objects make; `available`: UDisks2 answers.
    pub fn from_objects(objects: &Objects, available: bool) -> Drives {
        let mut all: Vec<(String, Drive)> = Vec::new();
        for (path, ifaces) in objects {
            let Some(d) = ifaces.get(DRIVE) else {
                continue;
            };
            if !listed(d) {
                continue;
            }
            // A drive whose block UDisks2 has not added yet waits for it.
            let Some(device) = drive_device(objects, path) else {
                continue;
            };
            let volumes = filesystems(objects, path)
                .into_iter()
                .filter(|(_, b, _)| !dbus::boolean(b, "HintIgnore").unwrap_or(false))
                .filter_map(|(_, b, fs)| {
                    let points = mount_points(fs);
                    Some(Volume {
                        device: block_device(b)?,
                        label: dbus::text(b, "IdLabel").filter(|l| !l.is_empty()),
                        size: dbus::number(b, "Size").unwrap_or(0.0),
                        mounted: !points.is_empty(),
                        path: points.into_iter().next(),
                    })
                })
                .collect();
            let name = [dbus::text(d, "Vendor"), dbus::text(d, "Model")]
                .into_iter()
                .flatten()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            let flag = |name| dbus::boolean(d, name).unwrap_or(false);
            let drive = Drive {
                name: if name.is_empty() {
                    device.clone()
                } else {
                    name
                },
                size: dbus::number(d, "Size").unwrap_or(0.0),
                icon: drive_icon(d),
                can_eject: flag("Ejectable") || flag("CanPowerOff"),
                volumes,
                device,
            };
            all.push((dbus::text(d, "SortKey").unwrap_or_default(), drive));
        }
        all.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.device.cmp(&b.1.device)));
        Drives {
            all: all.into_iter().map(|(_, d)| d).collect(),
            available,
            failed: Event::default(),
        }
    }

    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let conn = match cx.system().await {
            Ok(c) => c,
            Err(e) => return crate::dbus::idle_without_bus(&mut cx, "system", e).await,
        };
        follow(&mut cx, &conn).await
    }
}

/// The filesystem whose device is `device`: its object path.
fn volume_path(objects: &Objects, device: &str) -> Option<String> {
    objects.iter().find_map(|(p, ifaces)| {
        let b = ifaces.get(BLOCK)?;
        ifaces.get(FILESYSTEM)?;
        let named = block_device(b).as_deref() == Some(device)
            || bytestring(b, "Device").as_deref() == Some(device);
        named.then(|| p.clone())
    })
}

/// The drive whose whole-disk block is `device`: its object path.
fn drive_path(objects: &Objects, device: &str) -> Option<String> {
    objects.iter().find_map(|(p, ifaces)| {
        ifaces.get(DRIVE)?;
        (drive_device(objects, p).as_deref() == Some(device)).then(|| p.clone())
    })
}

/// How `eject()` makes drive `path` safe to remove: unmount each mounted
/// filesystem on it, lock each encrypted one unlocked, then either eject
/// it or power it off (see the module docs): `Eject` for a medium that
/// comes out (`MediaRemovable`) of a drive that ejects, else `PowerOff`
/// where it can be, else `Eject`. A drive that can do neither is refused
/// before anything is unmounted.
fn eject_plan(objects: &Objects, path: &str) -> Result<Vec<Step>, String> {
    let d = objects
        .get(path)
        .and_then(|i| i.get(DRIVE))
        .ok_or_else(|| "no such drive".to_string())?;
    let flag = |name| dbus::boolean(d, name).unwrap_or(false);
    let (eject, power_off) = (flag("Ejectable"), flag("CanPowerOff"));
    let last = if eject && flag("MediaRemovable") {
        Step::Eject(path.to_string())
    } else if power_off {
        Step::PowerOff(path.to_string())
    } else if eject {
        Step::Eject(path.to_string())
    } else {
        return Err("the drive cannot be ejected".to_string());
    };
    let fs = filesystems(objects, path);
    let mut steps: Vec<Step> = fs
        .iter()
        .filter(|(_, _, f)| !mount_points(f).is_empty())
        .map(|(p, _, _)| Step::Unmount(p.to_string()))
        .collect();
    // Every cleartext block on the drive, its filesystem or not.
    let mut backing: Vec<(f64, String)> = objects
        .iter()
        .filter_map(|(p, ifaces)| {
            let b = ifaces.get(BLOCK)?;
            let backing = object_path(b, "CryptoBackingDevice")?;
            (block_drive(objects, b).as_deref() == Some(path))
                .then(|| (partition_number(objects, p), backing))
        })
        .collect();
    backing.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    steps.extend(backing.into_iter().map(|(_, b)| Step::Lock(b)));
    steps.push(last);
    Ok(steps)
}

/// The shell's calls under way, each bringing back its device and, if it
/// failed, why. Dropped with the body (it returned, or the service
/// stopped and its future was dropped), they are let go rather than
/// cancelled: an eject is several calls the client makes in turn, and
/// cancelled after its `Unmount` it would leave the drive unmounted but
/// still powered, its `failed` never sent. They finish on the services
/// runtime; what they bring back is dropped.
#[derive(Default)]
struct Calls(JoinSet<(String, Option<String>)>);

impl Drop for Calls {
    fn drop(&mut self) {
        self.0.detach_all();
    }
}

/// The shell's calls in flight, by task: each one's device and what it
/// asks.
#[derive(Debug, Default)]
struct InFlight {
    calls: HashMap<tokio::task::Id, (String, Ask)>,
}

impl InFlight {
    /// `ask` of `device` repeats a call in flight on it.
    fn repeats(&self, device: &str, ask: Ask) -> bool {
        self.calls.values().any(|(d, a)| d == device && *a == ask)
    }

    /// Task `id` asks `ask` of `device`.
    fn insert(&mut self, id: tokio::task::Id, device: String, ask: Ask) {
        self.calls.insert(id, (device, ask));
    }

    /// Task `id` ended.
    fn remove(&mut self, id: tokio::task::Id) {
        self.calls.remove(&id);
    }
}

/// What a failed call to UDisks2 says: its own words ("Error unmounting
/// /dev/sdb1: target is busy", "Not authorized to perform operation"),
/// else the error's name.
fn udisks_error(e: zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(name, text, _) => text
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| name.to_string()),
        zbus::Error::Failure(text) => text,
        e => e.to_string(),
    }
}

/// Make `steps`' calls in order; the first refused stops them, saying why.
/// No call is bounded: polkit's dialog may stay open as long as the user
/// leaves it, and UDisks2 acts once it is answered.
async fn act(conn: &zbus::Connection, steps: &[Step]) -> Result<(), String> {
    for step in steps {
        let (path, iface, method) = step.call();
        let options: HashMap<&str, Value<'_>> = HashMap::new();
        conn.call_method(Some(UDISKS), path, Some(iface), method, &(options,))
            .await
            .map_err(udisks_error)?;
    }
    Ok(())
}

/// The `drives` body: UDisks2 followed, read afresh when it restarts.
async fn follow(cx: &mut Cx<Drives>, conn: &zbus::Connection) -> Result<(), ServiceError> {
    let mut daemon = Daemon::new(conn, UDISKS).await?;
    daemon.subscribe("manager", dbus::path_rule(ROOT)?).await?;
    for (key, iface) in [("drive", DRIVE), ("block", BLOCK), ("fs", FILESYSTEM)] {
        daemon
            .subscribe(key, dbus::properties_rule(ROOT, iface)?)
            .await?;
    }
    // Mounts, unmounts and ejects in flight: let go, not cancelled, when
    // the body ends.
    let mut calls = Calls::default();
    let mut busy = InFlight::default();
    loop {
        let mut objects = read(&daemon)
            .await
            .map_err(|e| ServiceError(format!("UDisks2 did not answer: {e}")))?;
        let available = daemon.owner().is_some();
        if !cx.update(|s| *s = Drives::from_objects(&objects, available)) {
            return Ok(());
        }
        cx.ready();
        loop {
            tokio::select! {
                Some(done) = calls.0.join_next_with_id(), if !calls.0.is_empty() => {
                    let failure = match done {
                        Ok((id, (device, error))) => {
                            busy.remove(id);
                            error.map(|e| (device, e))
                        }
                        Err(e) => {
                            busy.remove(e.id());
                            None
                        }
                    };
                    if let Some(failure) = failure
                        && !cx.emit(DrivesEvent::Failed(failure))
                    {
                        return Ok(());
                    }
                }
                ev = daemon.next() => match ev {
                    None => return Err(ServiceError("the system bus connection ended".into())),
                    // UDisks2 restarted (or went): read it again.
                    Some(DaemonEvent::Owner) => break,
                    Some(DaemonEvent::Signal(m)) => {
                        if signal(&daemon, &mut objects, &m).await
                            && !cx.update(|s| *s = Drives::from_objects(&objects, available))
                        {
                            return Ok(());
                        }
                    }
                },
                m = cx.recv() => match m {
                    None => return Ok(()),
                    Some(Msg::Action(a)) => {
                        let (device, ask) = a.target();
                        let device = device.to_string();
                        if busy.repeats(&device, ask) {
                            log::debug!("drives: {ask:?} {device} already runs");
                            continue;
                        }
                        let steps = plan(&objects, &device, ask, daemon.owner().is_some());
                        let steps = match steps {
                            Ok(s) => s,
                            Err(e) => {
                                log::warn!("drives: {ask:?} {device}: {e}");
                                if !cx.emit(DrivesEvent::Failed((device, e))) {
                                    return Ok(());
                                }
                                continue;
                            }
                        };
                        let conn = conn.clone();
                        let dev = device.clone();
                        let task = calls.0.spawn(async move {
                            let r = act(&conn, &steps).await;
                            if let Err(e) = &r {
                                log::warn!("drives: {ask:?} {dev}: {e}");
                            }
                            (dev, r.err())
                        });
                        busy.insert(task.id(), device, ask);
                    }
                    Some(_) => {}
                },
            }
        }
    }
}

/// The calls `ask` of `device` makes; why it cannot be asked, if not.
fn plan(objects: &Objects, device: &str, ask: Ask, running: bool) -> Result<Vec<Step>, String> {
    let missing = |what: &str| {
        if running {
            format!("no such {what}")
        } else {
            "UDisks2 is not running".to_string()
        }
    };
    match ask {
        Ask::Mount | Ask::Unmount => {
            let path = volume_path(objects, device).ok_or_else(|| missing("filesystem"))?;
            Ok(vec![if ask == Ask::Mount {
                Step::Mount(path)
            } else {
                Step::Unmount(path)
            }])
        }
        Ask::Eject => {
            let path = drive_path(objects, device).ok_or_else(|| missing("drive"))?;
            eject_plan(objects, &path)
        }
    }
}

/// Every object UDisks2 manages, with only the interfaces kept.
///
/// A hung UDisks2 (no answer in [`dbus::READ_TIMEOUT`]) is an error: the
/// run fails and is retried.
async fn read(daemon: &Daemon) -> zbus::Result<Objects> {
    if daemon.owner().is_none() {
        return Ok(Objects::new());
    }
    let reply = dbus::timed_for(
        dbus::READ_TIMEOUT,
        daemon.conn().call_method(
            Some(UDISKS),
            ROOT,
            Some(OBJECT_MANAGER),
            "GetManagedObjects",
            &(),
        ),
    )
    .await;
    let reply = match reply {
        Err(e) if dbus::is_timeout(&e) => return Err(e),
        r => r,
    };
    let objects = reply.and_then(|r| {
        r.body()
            .deserialize::<HashMap<zbus::zvariant::OwnedObjectPath, HashMap<String, Props>>>()
    });
    Ok(match objects {
        Ok(o) => o
            .into_iter()
            .filter_map(|(p, mut i)| {
                i.retain(|name, _| KEPT.contains(&name.as_str()));
                (!i.is_empty()).then(|| (p.to_string(), i))
            })
            .collect(),
        Err(e) => {
            log::debug!("drives: no objects: {e}");
            Objects::new()
        }
    })
}

/// Apply a signal; whether anything changed.
async fn signal(daemon: &Daemon, objects: &mut Objects, m: &zbus::Message) -> bool {
    if let Some(c) = dbus::properties_changed(m) {
        let Some(props) = objects.get_mut(&c.path).and_then(|i| i.get_mut(&c.iface)) else {
            return false;
        };
        dbus::apply_changed(daemon.conn(), UDISKS, props, c).await;
        return true;
    }
    if dbus::interface(m).as_deref() != Some(OBJECT_MANAGER) {
        return false;
    }
    match dbus::member(m).as_deref() {
        Some("InterfacesAdded") => {
            let Ok((path, mut ifaces)) = m
                .body()
                .deserialize::<(zbus::zvariant::OwnedObjectPath, HashMap<String, Props>)>()
            else {
                return false;
            };
            ifaces.retain(|name, _| KEPT.contains(&name.as_str()));
            if ifaces.is_empty() {
                return false;
            }
            objects.entry(path.to_string()).or_default().extend(ifaces);
            true
        }
        Some("InterfacesRemoved") => {
            let Ok((path, ifaces)) = m
                .body()
                .deserialize::<(zbus::zvariant::OwnedObjectPath, Vec<String>)>()
            else {
                return false;
            };
            let key = path.to_string();
            let Some(o) = objects.get_mut(&key) else {
                return false;
            };
            let mut changed = false;
            for i in ifaces {
                changed |= o.remove(&i).is_some();
            }
            if o.is_empty() {
                objects.remove(&key);
            }
            changed
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::OwnedValue;

    fn v(x: Value<'_>) -> OwnedValue {
        OwnedValue::try_from(x).unwrap()
    }

    fn ay(s: &str) -> OwnedValue {
        let mut b = s.as_bytes().to_vec();
        b.push(0);
        v(Value::from(b))
    }

    fn aay(points: &[&str]) -> OwnedValue {
        let all: Vec<Vec<u8>> = points
            .iter()
            .map(|p| {
                let mut b = p.as_bytes().to_vec();
                b.push(0);
                b
            })
            .collect();
        v(Value::from(all))
    }

    fn path(p: &str) -> OwnedValue {
        v(Value::from(ObjectPath::try_from(p).unwrap()))
    }

    fn props(entries: Vec<(&str, OwnedValue)>) -> Props {
        entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
    }

    const D: &str = "/org/freedesktop/UDisks2/drives/";
    const B: &str = "/org/freedesktop/UDisks2/block_devices/";

    /// A drive object.
    #[allow(clippy::too_many_arguments)]
    fn drive(
        o: &mut Objects,
        name: &str,
        (vendor, model): (&str, &str),
        removable: bool,
        (media_removable, media_available): (bool, bool),
        (ejectable, can_power_off): (bool, bool),
        (optical, media): (bool, &str),
        sort_key: &str,
    ) {
        o.insert(
            format!("{D}{name}"),
            HashMap::from([(
                DRIVE.to_string(),
                props(vec![
                    ("Vendor", v(Value::from(vendor))),
                    ("Model", v(Value::from(model))),
                    ("Size", v(Value::from(8_000_000_000u64))),
                    ("Removable", v(Value::from(removable))),
                    ("MediaRemovable", v(Value::from(media_removable))),
                    ("MediaAvailable", v(Value::from(media_available))),
                    ("Ejectable", v(Value::from(ejectable))),
                    ("CanPowerOff", v(Value::from(can_power_off))),
                    ("Optical", v(Value::from(optical))),
                    ("Media", v(Value::from(media))),
                    ("SortKey", v(Value::from(sort_key))),
                ]),
            )]),
        );
    }

    /// A block object `name` on drive `on` (`/` for none): its
    /// partition number, and its filesystem's mount points when it has
    /// one.
    fn block(
        o: &mut Objects,
        name: &str,
        on: &str,
        part: Option<u32>,
        label: &str,
        fs: Option<&[&str]>,
    ) {
        let mut ifaces = HashMap::from([(
            BLOCK.to_string(),
            props(vec![
                ("Device", ay(&format!("/dev/{name}"))),
                ("PreferredDevice", ay(&format!("/dev/{name}"))),
                (
                    "Drive",
                    path(&if on == "/" {
                        "/".to_string()
                    } else {
                        format!("{D}{on}")
                    }),
                ),
                ("IdLabel", v(Value::from(label))),
                ("Size", v(Value::from(1_000_000u64))),
                ("HintIgnore", v(Value::from(false))),
                ("CryptoBackingDevice", path("/")),
            ]),
        )]);
        if let Some(n) = part {
            ifaces.insert(
                PARTITION.to_string(),
                props(vec![("Number", v(Value::from(n)))]),
            );
        }
        if let Some(points) = fs {
            ifaces.insert(
                FILESYSTEM.to_string(),
                props(vec![("MountPoints", aay(points))]),
            );
        }
        o.insert(format!("{B}{name}"), ifaces);
    }

    fn set(o: &mut Objects, path: &str, iface: &str, name: &str, value: OwnedValue) {
        o.get_mut(path)
            .unwrap()
            .get_mut(iface)
            .unwrap()
            .insert(name.to_string(), value);
    }

    /// An internal NVMe disk, a stick with two partitions, an empty card
    /// reader, a disc drive with a disc and a loop device.
    fn objects() -> Objects {
        let mut o = Objects::new();
        drive(
            &mut o,
            "nvme",
            ("", "Samsung SSD 980"),
            false,
            (false, true),
            (false, false),
            (false, ""),
            "00coldplug/00fixed/nvme0n1",
        );
        block(&mut o, "nvme0n1", "nvme", None, "", None);
        block(&mut o, "nvme0n1p2", "nvme", Some(2), "root", Some(&["/"]));
        drive(
            &mut o,
            "stick",
            ("Kingston ", " DataTraveler 3.0"),
            true,
            (false, true),
            (true, true),
            (false, "thumb"),
            "01hotplug/1700000002",
        );
        block(&mut o, "sdb", "stick", None, "", None);
        block(&mut o, "sdb1", "stick", Some(1), "STICK", Some(&[]));
        block(&mut o, "sdb2", "stick", Some(2), "", Some(&[]));
        drive(
            &mut o,
            "reader",
            ("Generic", "SD Reader"),
            true,
            (true, false),
            (true, false),
            (false, "flash_sd"),
            "01hotplug/1700000001",
        );
        block(&mut o, "sdc", "reader", None, "", None);
        drive(
            &mut o,
            "dvd",
            ("", ""),
            true,
            (true, true),
            (true, false),
            (true, "optical_dvd"),
            "00coldplug/10removable/sr0",
        );
        block(&mut o, "sr0", "dvd", None, "FILMS", Some(&[]));
        block(&mut o, "loop0", "/", None, "snap", Some(&["/snap/x"]));
        o
    }

    #[test]
    fn removable_drives_with_their_medium_are_listed() {
        let d = Drives::from_objects(&objects(), true);
        assert!(d.available);
        let devices: Vec<&str> = d.all.iter().map(|d| d.device.as_str()).collect();
        assert_eq!(devices, ["/dev/sr0", "/dev/sdb"], "in SortKey order");
        let (disc, stick) = (&d.all[0], &d.all[1]);
        assert_eq!(stick.name, "Kingston DataTraveler 3.0");
        assert_eq!(stick.size, 8_000_000_000.0);
        assert_eq!(stick.icon, "media-flash-symbolic");
        assert!(stick.can_eject);
        assert_eq!(disc.name, "/dev/sr0", "no vendor or model");
        assert_eq!(disc.icon, "media-optical-symbolic");
        assert!(disc.can_eject);
        assert_eq!(disc.volumes.len(), 1, "the disc's own filesystem");
        assert!(
            !Drives::from_objects(&Objects::new(), false).available,
            "no UDisks2"
        );
        // A USB disk with neither: listed, not ejectable; a plain drive's
        // icon.
        let mut o = objects();
        set(
            &mut o,
            &format!("{D}stick"),
            DRIVE,
            "Ejectable",
            v(Value::from(false)),
        );
        set(
            &mut o,
            &format!("{D}stick"),
            DRIVE,
            "CanPowerOff",
            v(Value::from(false)),
        );
        set(
            &mut o,
            &format!("{D}stick"),
            DRIVE,
            "Media",
            v(Value::from("")),
        );
        let d = Drives::from_objects(&o, true);
        assert!(!d.all[1].can_eject);
        assert_eq!(d.all[1].icon, "drive-removable-media-symbolic");
        // The card goes in.
        set(
            &mut o,
            &format!("{D}reader"),
            DRIVE,
            "MediaAvailable",
            v(Value::from(true)),
        );
        let d = Drives::from_objects(&o, true);
        let devices: Vec<&str> = d.all.iter().map(|d| d.device.as_str()).collect();
        assert_eq!(devices, ["/dev/sr0", "/dev/sdc", "/dev/sdb"]);
        assert!(d.all[1].volumes.is_empty(), "no filesystem on it yet");
    }

    #[test]
    fn filesystems_are_read_from_their_blocks() {
        let mut o = objects();
        block(
            &mut o,
            "sdb10",
            "stick",
            Some(10),
            "TEN",
            Some(&["/run/media/you/TEN", "/mnt/ten"]),
        );
        block(&mut o, "sdb3", "stick", Some(3), "EFI", Some(&[]));
        set(
            &mut o,
            &format!("{B}sdb3"),
            BLOCK,
            "HintIgnore",
            v(Value::from(true)),
        );
        let d = Drives::from_objects(&o, true);
        let stick = &d.all[1];
        let devices: Vec<&str> = stick.volumes.iter().map(|v| v.device.as_str()).collect();
        assert_eq!(
            devices,
            ["/dev/sdb1", "/dev/sdb2", "/dev/sdb10"],
            "partition order, the hidden one left out"
        );
        let (one, two, ten) = (&stick.volumes[0], &stick.volumes[1], &stick.volumes[2]);
        assert_eq!(one.label.as_deref(), Some("STICK"));
        assert_eq!(two.label, None, "an empty label reads as null");
        assert_eq!(one.size, 1_000_000.0);
        assert!(!one.mounted && one.path.is_none());
        assert!(ten.mounted);
        assert_eq!(ten.path.as_deref(), Some("/run/media/you/TEN"), "the first");
        // Mounted, without the NUL.
        set(
            &mut o,
            &format!("{B}sdb1"),
            FILESYSTEM,
            "MountPoints",
            aay(&["/run/media/you/STICK"]),
        );
        let d = Drives::from_objects(&o, true);
        let one = &d.all[1].volumes[0];
        assert!(one.mounted);
        assert_eq!(one.path.as_deref(), Some("/run/media/you/STICK"));
        assert_eq!(
            bytestring(&o[&format!("{B}sdb1")][BLOCK], "Device").as_deref(),
            Some("/dev/sdb1")
        );
    }

    /// The stick's second partition encrypted: locked, then unlocked by
    /// another tool (a cleartext block, its `Drive` `/`).
    fn encrypted() -> Objects {
        let mut o = objects();
        o.get_mut(&format!("{B}sdb2")).unwrap().remove(FILESYSTEM);
        o
    }

    fn unlock(o: &mut Objects, mounted: bool) {
        let points: &[&str] = if mounted {
            &["/run/media/you/SECRET"]
        } else {
            &[]
        };
        block(o, "dm_2d0", "/", None, "SECRET", Some(points));
        let cleartext = format!("{B}dm_2d0");
        set(
            o,
            &cleartext,
            BLOCK,
            "CryptoBackingDevice",
            path(&format!("{B}sdb2")),
        );
        set(
            o,
            &cleartext,
            BLOCK,
            "PreferredDevice",
            ay("/dev/mapper/luks-1234"),
        );
    }

    #[test]
    fn an_unlocked_encrypted_filesystem_belongs_to_its_drive() {
        let mut o = encrypted();
        let d = Drives::from_objects(&o, true);
        let devices: Vec<&str> = d.all[1].volumes.iter().map(|v| v.device.as_str()).collect();
        assert_eq!(devices, ["/dev/sdb1"], "the locked one is not listed");
        unlock(&mut o, false);
        let d = Drives::from_objects(&o, true);
        let devices: Vec<&str> = d.all[1].volumes.iter().map(|v| v.device.as_str()).collect();
        assert_eq!(devices, ["/dev/sdb1", "/dev/mapper/luks-1234"]);
        assert_eq!(d.all[1].volumes[1].label.as_deref(), Some("SECRET"));
        assert_eq!(d.all.len(), 2, "not a drive of its own");
        assert_eq!(
            volume_path(&o, "/dev/mapper/luks-1234").as_deref(),
            Some(format!("{B}dm_2d0").as_str())
        );
        assert_eq!(
            volume_path(&o, "/dev/dm_2d0").as_deref(),
            Some(format!("{B}dm_2d0").as_str()),
            "by its `Device` too"
        );
    }

    #[test]
    fn eject_takes_only_the_steps_the_drive_allows() {
        let stick = format!("{D}stick");
        let mut o = objects();
        set(
            &mut o,
            &format!("{B}sdb1"),
            FILESYSTEM,
            "MountPoints",
            aay(&["/run/media/you/STICK"]),
        );
        // A stick (its medium never comes out), ejectable and able to be
        // powered off: powered off only.
        assert_eq!(
            eject_plan(&o, &stick),
            Ok(vec![
                Step::Unmount(format!("{B}sdb1")),
                Step::PowerOff(stick.clone()),
            ])
        );
        assert_eq!(drive_path(&o, "/dev/sdb"), Some(stick.clone()));
        // A disc: its medium ejected, the drive not powered off.
        let dvd = format!("{D}dvd");
        set(
            &mut o,
            &format!("{B}sr0"),
            FILESYSTEM,
            "MountPoints",
            aay(&["/run/media/you/FILMS"]),
        );
        let disc_out = Ok(vec![
            Step::Unmount(format!("{B}sr0")),
            Step::Eject(dvd.clone()),
        ]);
        assert_eq!(eject_plan(&o, &dvd), disc_out);
        // A USB disc drive, which can be powered off too: still only the
        // disc out, so it hears the next one.
        set(&mut o, &dvd, DRIVE, "CanPowerOff", v(Value::from(true)));
        assert_eq!(eject_plan(&o, &dvd), disc_out);
        // A card in a USB reader that can be powered off: the card out,
        // the reader (every slot of it) left on.
        let reader = format!("{D}reader");
        set(
            &mut o,
            &reader,
            DRIVE,
            "MediaAvailable",
            v(Value::from(true)),
        );
        set(&mut o, &reader, DRIVE, "CanPowerOff", v(Value::from(true)));
        block(
            &mut o,
            "sdc1",
            "reader",
            Some(1),
            "CAMERA",
            Some(&["/run/media/you/CAMERA"]),
        );
        assert_eq!(
            eject_plan(&o, &reader),
            Ok(vec![
                Step::Unmount(format!("{B}sdc1")),
                Step::Eject(reader.clone())
            ])
        );
        // A USB disk that is not ejectable: powered off.
        set(&mut o, &stick, DRIVE, "Ejectable", v(Value::from(false)));
        assert_eq!(
            eject_plan(&o, &stick),
            Ok(vec![
                Step::Unmount(format!("{B}sdb1")),
                Step::PowerOff(stick.clone())
            ])
        );
        // Ejectable, its medium fixed, and no power off: ejected.
        set(&mut o, &stick, DRIVE, "Ejectable", v(Value::from(true)));
        set(&mut o, &stick, DRIVE, "CanPowerOff", v(Value::from(false)));
        assert_eq!(
            eject_plan(&o, &stick),
            Ok(vec![
                Step::Unmount(format!("{B}sdb1")),
                Step::Eject(stick.clone())
            ])
        );
        // Neither: refused, nothing unmounted.
        set(&mut o, &stick, DRIVE, "Ejectable", v(Value::from(false)));
        assert!(eject_plan(&o, &stick).is_err());
        assert!(plan(&o, "/dev/sdb", Ask::Eject, true).is_err());
        // An unlocked encrypted filesystem is unmounted, then locked.
        let mut o = encrypted();
        unlock(&mut o, true);
        assert_eq!(
            eject_plan(&o, &stick),
            Ok(vec![
                Step::Unmount(format!("{B}dm_2d0")),
                Step::Lock(format!("{B}sdb2")),
                Step::PowerOff(stick.clone()),
            ])
        );
        // What is not there.
        let o = Objects::new();
        assert_eq!(
            plan(&o, "/dev/sdb", Ask::Eject, false),
            Err("UDisks2 is not running".to_string())
        );
        assert_eq!(
            plan(&o, "/dev/sdb1", Ask::Mount, true),
            Err("no such filesystem".to_string())
        );
        assert_eq!(
            plan(&objects(), "/dev/sdb1", Ask::Mount, true),
            Ok(vec![Step::Mount(format!("{B}sdb1"))])
        );
    }

    #[test]
    fn a_call_repeating_one_in_flight_is_dropped() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _in = rt.enter();
        let mut tasks: JoinSet<()> = JoinSet::new();
        let mut f = InFlight::default();
        let mount = tasks.spawn(async {}).id();
        f.insert(mount, "/dev/sdb1".into(), Ask::Mount);
        assert!(f.repeats("/dev/sdb1", Ask::Mount), "a double click");
        assert!(!f.repeats("/dev/sdb1", Ask::Unmount));
        assert!(!f.repeats("/dev/sdb2", Ask::Mount), "another volume");
        assert!(!f.repeats("/dev/sdb", Ask::Eject), "its drive");
        f.remove(mount);
        assert!(!f.repeats("/dev/sdb1", Ask::Mount), "ended");
    }

    #[test]
    fn udisks_errors_read_as_udisks_says_them() {
        let msg = zbus::Message::method_call("/", "Mount")
            .unwrap()
            .build(&())
            .unwrap();
        let name = |n: &str| zbus::names::OwnedErrorName::try_from(n.to_string()).unwrap();
        assert_eq!(
            udisks_error(zbus::Error::MethodError(
                name("org.freedesktop.UDisks2.Error.DeviceBusy"),
                Some("Error unmounting /dev/sdb1: target is busy".into()),
                msg.clone(),
            )),
            "Error unmounting /dev/sdb1: target is busy"
        );
        assert_eq!(
            udisks_error(zbus::Error::MethodError(
                name("org.freedesktop.UDisks2.Error.NotAuthorizedDismissed"),
                None,
                msg,
            )),
            "org.freedesktop.UDisks2.Error.NotAuthorizedDismissed"
        );
        assert_eq!(
            udisks_error(zbus::Error::Failure("no answer".into())),
            "no answer"
        );
    }
}
