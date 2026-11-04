//! A small UDisks2 for the `drives` tests (python-dbusmock has no
//! udisks2 template), in the style of `network_iwd.rs`'s iwd: it owns
//! `org.freedesktop.UDisks2` on a private bus and serves zbus's
//! `ObjectManager` at `/org/freedesktop/UDisks2`, so an object added or
//! removed is an `InterfacesAdded` or `InterfacesRemoved`, and its
//! `Drive`, `Drive.Ata`, `Block`, `Filesystem`, `Partition` and
//! `Encrypted` interfaces from a [`World`] the test changes. Each method
//! called is logged in [`World::calls`] with its device (and a mount's
//! options); [`World::refuse`] makes a method fail as UDisks2 does, and
//! [`World::stall_mount`] holds a mount until the test lets it go.
//! Dropping the connection [`serve`] gives loses the name: UDisks2 gone.
//! Never the machine's own UDisks2.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

pub use strand_services::drives::UDISKS;

pub const ROOT: &str = "/org/freedesktop/UDisks2";

/// A drive's object path.
pub fn drive_path(id: &str) -> String {
    format!("{ROOT}/drives/{id}")
}

/// A block device's object path.
pub fn block_path(id: &str) -> String {
    format!("{ROOT}/block_devices/{id}")
}

/// A drive, named by its object's last element.
#[derive(Clone, Debug, Default)]
pub struct DriveSpec {
    pub id: String,
    /// Its whole-disk device, for the log.
    pub device: String,
    pub vendor: String,
    pub model: String,
    pub size: u64,
    pub removable: bool,
    pub media_removable: bool,
    pub media_available: bool,
    pub ejectable: bool,
    pub can_power_off: bool,
    pub optical: bool,
    pub media: String,
    pub sort_key: String,
    /// It is an ATA disk (`Drive.Ata`, with its SMART readings).
    pub ata: bool,
}

/// A block device, named by its object's last element.
#[derive(Clone, Debug, Default)]
pub struct BlockSpec {
    pub id: String,
    pub device: String,
    /// The drive it is on (`/` for none: a loop device, a cleartext one).
    pub drive: Option<String>,
    pub label: String,
    pub size: u64,
    pub hint_ignore: bool,
    /// The encrypted block a cleartext one unlocks.
    pub backing: Option<String>,
    pub partition: Option<u32>,
    /// It holds a filesystem: its mount points.
    pub fs: Option<Vec<String>>,
    /// It is encrypted (`Encrypted`).
    pub encrypted: bool,
}

/// How UDisks2 refuses a method.
#[derive(Clone, Copy, Debug)]
pub enum Refusal {
    Failed,
    NotAuthorizedDismissed,
    DeviceBusy,
}

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.UDisks2.Error")]
pub enum UdError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Failed(String),
    NotAuthorizedDismissed(String),
    DeviceBusy(String),
}

#[derive(Debug, Default)]
pub struct World {
    pub drives: HashMap<String, DriveSpec>,
    pub blocks: HashMap<String, BlockSpec>,
    /// A method refused (`Mount`, `Unmount`, `Lock`, `Eject`,
    /// `PowerOff`): how, and the text.
    pub refuse: HashMap<String, (Refusal, String)>,
    /// `Mount` waits while this is set.
    pub stall_mount: bool,
    /// Each method called: `Mount /dev/sdb1 {}`, `Eject /dev/sdb`.
    pub calls: Vec<String>,
    /// The ATA disks' `SmartUpdated`.
    pub smart_updated: u64,
}

pub type W = Arc<Mutex<World>>;

impl World {
    /// The refusal of `method`, if the test set one.
    fn refusal(&self, method: &str) -> Result<(), UdError> {
        match self.refuse.get(method) {
            None => Ok(()),
            Some((Refusal::Failed, t)) => Err(UdError::Failed(t.clone())),
            Some((Refusal::NotAuthorizedDismissed, t)) => {
                Err(UdError::NotAuthorizedDismissed(t.clone()))
            }
            Some((Refusal::DeviceBusy, t)) => Err(UdError::DeviceBusy(t.clone())),
        }
    }

    /// Add `drive` and its `blocks`.
    pub fn plug(&mut self, (drive, blocks): (DriveSpec, Vec<BlockSpec>)) {
        for b in blocks {
            self.blocks.insert(b.id.clone(), b);
        }
        self.drives.insert(drive.id.clone(), drive);
    }
}

/// `s` as UDisks2 sends a device: bytes ending in NUL.
fn ay(s: &str) -> Vec<u8> {
    let mut b = s.as_bytes().to_vec();
    b.push(0);
    b
}

fn object(p: &str) -> OwnedObjectPath {
    OwnedObjectPath::try_from(p.to_string()).unwrap()
}

/// A USB stick `id` (`/dev/<dev>`) with two partitions: `<dev>1`
/// labelled `label`, and `<dev>2` without a label. Ejectable, and it can
/// be powered off.
pub fn stick(id: &str, dev: &str, label: &str, sort_key: &str) -> (DriveSpec, Vec<BlockSpec>) {
    let drive = DriveSpec {
        id: id.into(),
        device: format!("/dev/{dev}"),
        vendor: "Kingston".into(),
        model: "DataTraveler 3.0".into(),
        size: 16_000_000_000,
        removable: true,
        media_available: true,
        ejectable: true,
        can_power_off: true,
        media: "thumb".into(),
        sort_key: sort_key.into(),
        ..DriveSpec::default()
    };
    let part = |n: u32, label: &str| BlockSpec {
        id: format!("{dev}{n}"),
        device: format!("/dev/{dev}{n}"),
        drive: Some(id.into()),
        label: label.into(),
        size: 8_000_000_000,
        partition: Some(n),
        fs: Some(Vec::new()),
        ..BlockSpec::default()
    };
    let whole = BlockSpec {
        id: dev.into(),
        device: format!("/dev/{dev}"),
        drive: Some(id.into()),
        size: 16_000_000_000,
        ..BlockSpec::default()
    };
    (drive, vec![whole, part(1, label), part(2, "")])
}

/// The internal ATA disk (`/dev/sda`, its root partition mounted).
pub fn internal_disk() -> (DriveSpec, Vec<BlockSpec>) {
    let drive = DriveSpec {
        id: "Samsung_SSD".into(),
        device: "/dev/sda".into(),
        vendor: "ATA".into(),
        model: "Samsung SSD 870".into(),
        size: 500_000_000_000,
        media_available: true,
        sort_key: "00coldplug/00fixed/sda".into(),
        ata: true,
        ..DriveSpec::default()
    };
    let blocks = vec![
        BlockSpec {
            id: "sda".into(),
            device: "/dev/sda".into(),
            drive: Some("Samsung_SSD".into()),
            size: 500_000_000_000,
            ..BlockSpec::default()
        },
        BlockSpec {
            id: "sda2".into(),
            device: "/dev/sda2".into(),
            drive: Some("Samsung_SSD".into()),
            label: "root".into(),
            size: 499_000_000_000,
            partition: Some(2),
            fs: Some(vec!["/".into()]),
            ..BlockSpec::default()
        },
    ];
    (drive, blocks)
}

/// A card reader (`/dev/sdc`) with no card in.
pub fn card_reader() -> (DriveSpec, Vec<BlockSpec>) {
    let drive = DriveSpec {
        id: "Generic_SD_Reader".into(),
        device: "/dev/sdc".into(),
        vendor: "Generic".into(),
        model: "SD Reader".into(),
        removable: true,
        media_removable: true,
        ejectable: true,
        media: "flash_sd".into(),
        sort_key: "00coldplug/10removable/sdc".into(),
        ..DriveSpec::default()
    };
    let blocks = vec![BlockSpec {
        id: "sdc".into(),
        device: "/dev/sdc".into(),
        drive: Some("Generic_SD_Reader".into()),
        ..BlockSpec::default()
    }];
    (drive, blocks)
}

/// A disc drive (`/dev/sr0`) with a disc labelled `FILMS` in: it ejects,
/// and cannot be powered off.
pub fn disc_drive() -> (DriveSpec, Vec<BlockSpec>) {
    let drive = DriveSpec {
        id: "DVD_RW".into(),
        device: "/dev/sr0".into(),
        vendor: "HL-DT-ST".into(),
        model: "DVDRAM GP65NB60".into(),
        size: 4_700_000_000,
        removable: true,
        media_removable: true,
        media_available: true,
        ejectable: true,
        optical: true,
        media: "optical_dvd".into(),
        sort_key: "00coldplug/10removable/sr0".into(),
        ..DriveSpec::default()
    };
    let blocks = vec![BlockSpec {
        id: "sr0".into(),
        device: "/dev/sr0".into(),
        drive: Some("DVD_RW".into()),
        label: "FILMS".into(),
        size: 4_700_000_000,
        fs: Some(Vec::new()),
        ..BlockSpec::default()
    }];
    (drive, blocks)
}

pub struct Drive(pub W, pub String);

impl Drive {
    fn spec(&self) -> DriveSpec {
        self.0
            .lock()
            .unwrap()
            .drives
            .get(&self.1)
            .cloned()
            .unwrap_or_default()
    }
}

#[zbus::interface(name = "org.freedesktop.UDisks2.Drive")]
impl Drive {
    #[zbus(property)]
    fn vendor(&self) -> String {
        self.spec().vendor
    }
    #[zbus(property)]
    fn model(&self) -> String {
        self.spec().model
    }
    #[zbus(property)]
    fn size(&self) -> u64 {
        self.spec().size
    }
    #[zbus(property)]
    fn removable(&self) -> bool {
        self.spec().removable
    }
    #[zbus(property)]
    fn media_removable(&self) -> bool {
        self.spec().media_removable
    }
    #[zbus(property)]
    fn media_available(&self) -> bool {
        self.spec().media_available
    }
    #[zbus(property)]
    fn ejectable(&self) -> bool {
        self.spec().ejectable
    }
    #[zbus(property)]
    fn can_power_off(&self) -> bool {
        self.spec().can_power_off
    }
    #[zbus(property)]
    fn optical(&self) -> bool {
        self.spec().optical
    }
    #[zbus(property)]
    fn media(&self) -> String {
        self.spec().media
    }
    #[zbus(property)]
    fn sort_key(&self) -> String {
        self.spec().sort_key
    }

    /// Ejects its medium: one that can be taken out is gone.
    async fn eject(
        &self,
        _options: HashMap<String, OwnedValue>,
        #[zbus(signal_emitter)] e: SignalEmitter<'_>,
    ) -> Result<(), UdError> {
        let out = {
            let mut w = self.0.lock().unwrap();
            let device = self.spec_in(&w).device;
            w.calls.push(format!("Eject {device}"));
            w.refusal("Eject")?;
            match w.drives.get_mut(&self.1) {
                Some(d) if d.media_removable => {
                    d.media_available = false;
                    true
                }
                _ => false,
            }
        };
        if out {
            self.media_available_changed(&e).await?;
        }
        Ok(())
    }

    /// Powers it off: the drive and its blocks are gone.
    async fn power_off(
        &self,
        _options: HashMap<String, OwnedValue>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> Result<(), UdError> {
        {
            let mut w = self.0.lock().unwrap();
            let device = self.spec_in(&w).device;
            w.calls.push(format!("PowerOff {device}"));
            w.refusal("PowerOff")?;
        }
        remove_drive(server, &self.0, &self.1).await;
        Ok(())
    }
}

impl Drive {
    fn spec_in(&self, w: &World) -> DriveSpec {
        w.drives.get(&self.1).cloned().unwrap_or_default()
    }
}

pub struct Ata(pub W);

#[zbus::interface(name = "org.freedesktop.UDisks2.Drive.Ata")]
impl Ata {
    #[zbus(property)]
    fn smart_updated(&self) -> u64 {
        self.0.lock().unwrap().smart_updated
    }
}

pub struct Block(pub W, pub String);

impl Block {
    fn spec(&self) -> BlockSpec {
        self.0
            .lock()
            .unwrap()
            .blocks
            .get(&self.1)
            .cloned()
            .unwrap_or_default()
    }
}

#[zbus::interface(name = "org.freedesktop.UDisks2.Block")]
impl Block {
    #[zbus(property)]
    fn device(&self) -> Vec<u8> {
        ay(&self.spec().device)
    }
    #[zbus(property)]
    fn preferred_device(&self) -> Vec<u8> {
        ay(&self.spec().device)
    }
    #[zbus(property)]
    fn drive(&self) -> OwnedObjectPath {
        match self.spec().drive {
            Some(d) => object(&drive_path(&d)),
            None => object("/"),
        }
    }
    #[zbus(property)]
    fn id_label(&self) -> String {
        self.spec().label
    }
    #[zbus(property)]
    fn size(&self) -> u64 {
        self.spec().size
    }
    #[zbus(property)]
    fn hint_ignore(&self) -> bool {
        self.spec().hint_ignore
    }
    #[zbus(property)]
    fn crypto_backing_device(&self) -> OwnedObjectPath {
        match self.spec().backing {
            Some(b) => object(&block_path(&b)),
            None => object("/"),
        }
    }
}

pub struct Partition(pub W, pub String);

#[zbus::interface(name = "org.freedesktop.UDisks2.Partition")]
impl Partition {
    #[zbus(property)]
    fn number(&self) -> u32 {
        let w = self.0.lock().unwrap();
        w.blocks.get(&self.1).and_then(|b| b.partition).unwrap_or(0)
    }
}

pub struct Filesystem(pub W, pub String);

#[zbus::interface(name = "org.freedesktop.UDisks2.Filesystem")]
impl Filesystem {
    #[zbus(property)]
    fn mount_points(&self) -> Vec<Vec<u8>> {
        let w = self.0.lock().unwrap();
        w.blocks
            .get(&self.1)
            .and_then(|b| b.fs.clone())
            .unwrap_or_default()
            .iter()
            .map(|p| ay(p))
            .collect()
    }

    /// Mounts it at `/run/media/tester/<label>` (its device's name without
    /// one), as UDisks2 picks.
    async fn mount(
        &self,
        options: HashMap<String, OwnedValue>,
        #[zbus(signal_emitter)] e: SignalEmitter<'_>,
    ) -> Result<String, UdError> {
        {
            let mut w = self.0.lock().unwrap();
            let device = w
                .blocks
                .get(&self.1)
                .map(|b| b.device.clone())
                .unwrap_or_default();
            let mut keys: Vec<&str> = options.keys().map(String::as_str).collect();
            keys.sort();
            w.calls
                .push(format!("Mount {device} {{{}}}", keys.join(",")));
            w.refusal("Mount")?;
        }
        while self.0.lock().unwrap().stall_mount {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let at = {
            let mut w = self.0.lock().unwrap();
            let Some(b) = w.blocks.get_mut(&self.1) else {
                return Err(UdError::Failed("no such device".into()));
            };
            let name = if b.label.is_empty() {
                b.id.clone()
            } else {
                b.label.clone()
            };
            let at = format!("/run/media/tester/{name}");
            b.fs = Some(vec![at.clone()]);
            at
        };
        self.mount_points_changed(&e).await?;
        Ok(at)
    }

    async fn unmount(
        &self,
        _options: HashMap<String, OwnedValue>,
        #[zbus(signal_emitter)] e: SignalEmitter<'_>,
    ) -> Result<(), UdError> {
        {
            let mut w = self.0.lock().unwrap();
            let device = w
                .blocks
                .get(&self.1)
                .map(|b| b.device.clone())
                .unwrap_or_default();
            w.calls.push(format!("Unmount {device}"));
            w.refusal("Unmount")?;
            if let Some(b) = w.blocks.get_mut(&self.1) {
                b.fs = Some(Vec::new());
            }
        }
        self.mount_points_changed(&e).await?;
        Ok(())
    }
}

pub struct Encrypted(pub W, pub String);

#[zbus::interface(name = "org.freedesktop.UDisks2.Encrypted")]
impl Encrypted {
    /// Locks it: its cleartext block is gone.
    async fn lock(
        &self,
        _options: HashMap<String, OwnedValue>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> Result<(), UdError> {
        let cleartext: Vec<String> = {
            let mut w = self.0.lock().unwrap();
            let device = w
                .blocks
                .get(&self.1)
                .map(|b| b.device.clone())
                .unwrap_or_default();
            w.calls.push(format!("Lock {device}"));
            w.refusal("Lock")?;
            let ids: Vec<String> = w
                .blocks
                .values()
                .filter(|b| b.backing.as_deref() == Some(self.1.as_str()))
                .map(|b| b.id.clone())
                .collect();
            for id in &ids {
                w.blocks.remove(id);
            }
            ids
        };
        for id in cleartext {
            remove_block(server, &id).await;
        }
        Ok(())
    }
}

/// Serve block `id`'s interfaces as the world says.
async fn add_block(server: &zbus::ObjectServer, w: &W, id: &str) {
    let b = w.lock().unwrap().blocks.get(id).cloned();
    let Some(b) = b else {
        return;
    };
    let p = block_path(id);
    server
        .at(p.as_str(), Block(w.clone(), id.into()))
        .await
        .unwrap();
    if b.partition.is_some() {
        server
            .at(p.as_str(), Partition(w.clone(), id.into()))
            .await
            .unwrap();
    }
    if b.encrypted {
        server
            .at(p.as_str(), Encrypted(w.clone(), id.into()))
            .await
            .unwrap();
    }
    if b.fs.is_some() {
        server
            .at(p.as_str(), Filesystem(w.clone(), id.into()))
            .await
            .unwrap();
    }
}

/// Serve drive `id` and its blocks.
async fn add_drive(server: &zbus::ObjectServer, w: &W, id: &str) {
    let (ata, blocks) = {
        let w = w.lock().unwrap();
        let ata = w.drives.get(id).is_some_and(|d| d.ata);
        let mut blocks: Vec<String> = w
            .blocks
            .values()
            .filter(|b| b.drive.as_deref() == Some(id))
            .map(|b| b.id.clone())
            .collect();
        blocks.sort();
        (ata, blocks)
    };
    let p = drive_path(id);
    server
        .at(p.as_str(), Drive(w.clone(), id.into()))
        .await
        .unwrap();
    if ata {
        server.at(p.as_str(), Ata(w.clone())).await.unwrap();
    }
    for b in blocks {
        add_block(server, w, &b).await;
    }
}

/// Stop serving block `id` (each interface it may have).
async fn remove_block(server: &zbus::ObjectServer, id: &str) {
    let p = block_path(id);
    for iface in [
        "org.freedesktop.UDisks2.Filesystem",
        "org.freedesktop.UDisks2.Encrypted",
        "org.freedesktop.UDisks2.Partition",
        "org.freedesktop.UDisks2.Block",
    ] {
        let name = zbus::names::InterfaceName::from_static_str_unchecked(iface);
        let _ = server.remove_named(p.as_str(), name).await;
    }
}

/// Stop serving drive `id` and its blocks, and forget them.
async fn remove_drive(server: &zbus::ObjectServer, w: &W, id: &str) {
    let blocks: Vec<String> = {
        let mut w = w.lock().unwrap();
        let ids: Vec<String> = w
            .blocks
            .values()
            .filter(|b| b.drive.as_deref() == Some(id))
            .map(|b| b.id.clone())
            .collect();
        for b in &ids {
            w.blocks.remove(b);
        }
        w.drives.remove(id);
        ids
    };
    for b in blocks {
        remove_block(server, &b).await;
    }
    let p = drive_path(id);
    for iface in [
        "org.freedesktop.UDisks2.Drive.Ata",
        "org.freedesktop.UDisks2.Drive",
    ] {
        let name = zbus::names::InterfaceName::from_static_str_unchecked(iface);
        let _ = server.remove_named(p.as_str(), name).await;
    }
}

/// UDisks2 on the bus at `address`, with the world's drives and blocks
/// (the blocks of no drive too), owning its name once they are served.
pub fn serve(tokio: &tokio::runtime::Runtime, address: &str, w: &W) -> zbus::Connection {
    tokio.block_on(async {
        let conn = zbus::connection::Builder::address(address)
            .unwrap()
            .serve_at(ROOT, zbus::fdo::ObjectManager)
            .unwrap()
            .build()
            .await
            .unwrap();
        let (drives, loose) = {
            let w = w.lock().unwrap();
            let mut drives: Vec<String> = w.drives.keys().cloned().collect();
            drives.sort();
            let mut loose: Vec<String> = w
                .blocks
                .values()
                .filter(|b| b.drive.is_none())
                .map(|b| b.id.clone())
                .collect();
            loose.sort();
            (drives, loose)
        };
        let server = conn.object_server();
        for d in drives {
            add_drive(server, w, &d).await;
        }
        for b in loose {
            add_block(server, w, &b).await;
        }
        conn.request_name(UDISKS).await.unwrap();
        conn
    })
}

/// Plug `drive` in: its objects added, as UDisks2 adds them.
pub fn plug(
    tokio: &tokio::runtime::Runtime,
    conn: &zbus::Connection,
    w: &W,
    drive: (DriveSpec, Vec<BlockSpec>),
) {
    let id = drive.0.id.clone();
    w.lock().unwrap().plug(drive);
    tokio.block_on(add_drive(conn.object_server(), w, &id));
}

/// Pull drive `id` out: its objects removed.
pub fn unplug(tokio: &tokio::runtime::Runtime, conn: &zbus::Connection, w: &W, id: &str) {
    tokio.block_on(remove_drive(conn.object_server(), w, id));
}

/// Drive `id`'s medium put in or taken out (`MediaAvailable` changed).
pub fn set_media(
    tokio: &tokio::runtime::Runtime,
    conn: &zbus::Connection,
    w: &W,
    id: &str,
    available: bool,
) {
    if let Some(d) = w.lock().unwrap().drives.get_mut(id) {
        d.media_available = available;
    }
    tokio.block_on(async {
        let d = conn
            .object_server()
            .interface::<_, Drive>(drive_path(id).as_str())
            .await
            .unwrap();
        d.get()
            .await
            .media_available_changed(d.signal_emitter())
            .await
            .unwrap();
    });
}

/// A new filesystem on block `id` (a card's), served.
pub fn add_block_now(
    tokio: &tokio::runtime::Runtime,
    conn: &zbus::Connection,
    w: &W,
    b: BlockSpec,
) {
    let id = b.id.clone();
    w.lock().unwrap().blocks.insert(id.clone(), b);
    tokio.block_on(add_block(conn.object_server(), w, &id));
}

/// The ATA disks' SMART readings refreshed (`SmartUpdated` changed on
/// drive `id`), as udisksd does every ten minutes.
pub fn refresh_smart(tokio: &tokio::runtime::Runtime, conn: &zbus::Connection, w: &W, id: &str) {
    w.lock().unwrap().smart_updated += 1;
    tokio.block_on(async {
        let a = conn
            .object_server()
            .interface::<_, Ata>(drive_path(id).as_str())
            .await
            .unwrap();
        a.get()
            .await
            .smart_updated_changed(a.signal_emitter())
            .await
            .unwrap();
    });
}

/// The calls logged so far.
pub fn calls(w: &W) -> Vec<String> {
    w.lock().unwrap().calls.clone()
}
