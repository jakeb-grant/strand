//! `drives` against a small zbus UDisks2 on a private bus (`udisks/`;
//! python-dbusmock has no udisks2 template): the removable drives with
//! their medium in and their filesystems, followed as drives are plugged,
//! pulled, given a medium and relabelled; mounting and unmounting;
//! ejecting in order (a stick powered off, a disc or card ejected, an
//! unlocked encrypted volume locked), an eject finishing after its service
//! stops; refusals as `drives.failed`; a repeat in flight dropped; and
//! UDisks2 restarting or missing.

mod support;
mod udisks;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use strand_core::Runtime;
use strand_services::drives::{self, Volume};
use strand_services::testing::PrivateBus;
use strand_services::{Data, ToData};
use support::*;
use udisks::*;

fn all(b: &strand_services::Builtin, rt: &Runtime) -> Vec<drives::Drive> {
    b.drives
        .cells()
        .all
        .get_untracked(rt)
        .unwrap()
        .items()
        .iter()
        .map(|(_, d)| d.clone())
        .collect()
}

fn devices(b: &strand_services::Builtin, rt: &Runtime) -> Vec<String> {
    all(b, rt).into_iter().map(|d| d.device).collect()
}

/// The volume at `device`, if listed.
fn volume(b: &strand_services::Builtin, rt: &Runtime, device: &str) -> Option<Volume> {
    all(b, rt)
        .into_iter()
        .flat_map(|d| d.volumes)
        .find(|v| v.device == device)
}

fn available(b: &strand_services::Builtin, rt: &Runtime) -> bool {
    b.drives.cells().available.get_untracked(rt) == Ok(true)
}

/// The `failed(device, error)`s the service emits.
fn failures_of(b: &strand_services::Builtin) -> Arc<Mutex<Vec<(String, String)>>> {
    let failures = Arc::new(Mutex::new(Vec::new()));
    let f = failures.clone();
    b.drives.dynamic().observe(Box::new(move |_, a| {
        if let strand_services::Applied::Event { args, .. } = a
            && let [Data::Text(device), Data::Text(error)] = args.as_slice()
        {
            f.lock()
                .unwrap()
                .push((device.to_string(), error.to_string()));
        }
    }));
    failures
}

/// `v.<name>()` of the volume at `device`.
fn on_volume(rt: &Runtime, b: &strand_services::Builtin, name: &str, device: &str) {
    let item = Volume {
        device: device.into(),
        ..Volume::default()
    };
    b.drives
        .dynamic()
        .action(rt, name, Some(&item.to_data()), &[], Default::default())
        .unwrap();
}

/// `d.eject()` of the drive at `device`.
fn eject(rt: &Runtime, b: &strand_services::Builtin, device: &str) {
    let item = drives::Drive {
        device: device.into(),
        ..drives::Drive::default()
    };
    b.drives
        .dynamic()
        .action(rt, "eject", Some(&item.to_data()), &[], Default::default())
        .unwrap();
}

/// Pump for `d`, so what would arrive has.
fn settle(rt: &Runtime, s: &strand_services::Services, d: Duration) {
    let t = Instant::now();
    while t.elapsed() < d {
        s.pump(rt);
        rt.flush();
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn world(drives: Vec<(DriveSpec, Vec<BlockSpec>)>) -> W {
    let mut w = World::default();
    for d in drives {
        w.plug(d);
    }
    Arc::new(Mutex::new(w))
}

/// The stick (`/dev/sdb`, its `STICK` partition mounted when `mounted`).
fn the_stick(mounted: bool) -> (DriveSpec, Vec<BlockSpec>) {
    let (d, mut blocks) = stick("Kingston_1", "sdb", "STICK", "01hotplug/1700000001");
    if mounted {
        blocks[1].fs = Some(vec!["/run/media/tester/STICK".into()]);
    }
    (d, blocks)
}

#[test]
fn drives_follow_udisks() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let w = world(vec![internal_disk(), card_reader(), the_stick(false)]);
    let fake = serve(&tokio, &bus.address, &w);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    b.drives.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    until(&rt, &s, "the stick", || devices(&b, &rt) == ["/dev/sdb"]);
    assert!(available(&b, &rt));
    let d = &all(&b, &rt)[0];
    assert_eq!(d.name, "Kingston DataTraveler 3.0");
    assert_eq!(d.size, 16_000_000_000.0);
    assert_eq!(d.icon, "media-flash-symbolic");
    assert!(d.can_eject);
    let v: Vec<(&str, Option<&str>, bool)> = d
        .volumes
        .iter()
        .map(|v| (v.device.as_str(), v.label.as_deref(), v.mounted))
        .collect();
    assert_eq!(
        v,
        [
            ("/dev/sdb1", Some("STICK"), false),
            ("/dev/sdb2", None, false)
        ],
        "without the internal disk's or the empty reader's"
    );
    assert_eq!(d.volumes[0].size, 8_000_000_000.0);

    // A volume relabelled (a `Block` property changed).
    set_label(&tokio, &fake, &w, "sdb2", "PHOTOS");
    until(&rt, &s, "the new label", || {
        volume(&b, &rt, "/dev/sdb2").is_some_and(|v| v.label.as_deref() == Some("PHOTOS"))
    });

    // A second stick is plugged in, then pulled out.
    plug(
        &tokio,
        &fake,
        &w,
        stick("SanDisk_2", "sdd", "DATA", "01hotplug/1700000002"),
    );
    until(&rt, &s, "the second stick", || {
        all(&b, &rt)
            .iter()
            .map(|d| (d.device.clone(), d.volumes.len()))
            .collect::<Vec<_>>()
            == [("/dev/sdb".to_string(), 2), ("/dev/sdd".to_string(), 2)]
    });
    unplug(&tokio, &fake, &w, "SanDisk_2");
    until(&rt, &s, "pulled out", || devices(&b, &rt) == ["/dev/sdb"]);

    // A card goes into the reader (`MediaAvailable` changes), then its
    // filesystem appears.
    set_media(&tokio, &fake, &w, "Generic_SD_Reader", true);
    until(&rt, &s, "the card", || {
        devices(&b, &rt) == ["/dev/sdc", "/dev/sdb"]
    });
    assert!(all(&b, &rt)[0].volumes.is_empty());
    add_block_now(
        &tokio,
        &fake,
        &w,
        BlockSpec {
            id: "sdc1".into(),
            device: "/dev/sdc1".into(),
            drive: Some("Generic_SD_Reader".into()),
            label: "CAMERA".into(),
            size: 32_000_000_000,
            partition: Some(1),
            fs: Some(Vec::new()),
            ..BlockSpec::default()
        },
    );
    until(&rt, &s, "the card's filesystem", || {
        volume(&b, &rt, "/dev/sdc1").is_some_and(|v| v.label.as_deref() == Some("CAMERA"))
    });
    s.shutdown();
}

#[test]
fn a_volume_mounts_and_unmounts_through_udisks() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let w = world(vec![the_stick(false)]);
    let _fake = serve(&tokio, &bus.address, &w);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let failures = failures_of(&b);
    b.drives.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    until(&rt, &s, "the stick", || {
        volume(&b, &rt, "/dev/sdb1").is_some()
    });
    let v = volume(&b, &rt, "/dev/sdb1").unwrap();
    b.drives
        .dynamic()
        .action(&rt, "mount", Some(&v.to_data()), &[], Default::default())
        .unwrap();
    until(&rt, &s, "mounted", || {
        volume(&b, &rt, "/dev/sdb1")
            .is_some_and(|v| v.mounted && v.path.as_deref() == Some("/run/media/tester/STICK"))
    });
    // No options: UDisks2 picks the place, and polkit may ask.
    assert_eq!(calls(&w), ["Mount /dev/sdb1 {}"]);
    on_volume(&rt, &b, "unmount", "/dev/sdb1");
    until(&rt, &s, "unmounted", || {
        volume(&b, &rt, "/dev/sdb1").is_some_and(|v| !v.mounted && v.path.is_none())
    });
    assert_eq!(calls(&w), ["Mount /dev/sdb1 {}", "Unmount /dev/sdb1"]);
    assert!(failures.lock().unwrap().is_empty());
    s.shutdown();
}

#[test]
fn refusals_arrive_as_drives_failed() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let w = world(vec![the_stick(false)]);
    {
        let mut w = w.lock().unwrap();
        w.refuse.insert(
            "Mount".into(),
            (
                Refusal::NotAuthorizedDismissed,
                "Not authorized to perform operation".into(),
            ),
        );
        w.refuse.insert(
            "Unmount".into(),
            (
                Refusal::DeviceBusy,
                "Error unmounting /dev/sdb1: target is busy".into(),
            ),
        );
    }
    let _fake = serve(&tokio, &bus.address, &w);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let failures = failures_of(&b);
    b.drives.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    until(&rt, &s, "the stick", || {
        volume(&b, &rt, "/dev/sdb1").is_some()
    });
    let before = volume(&b, &rt, "/dev/sdb1");
    on_volume(&rt, &b, "mount", "/dev/sdb1");
    until(&rt, &s, "the mount's failure", || {
        failures.lock().unwrap().len() == 1
    });
    on_volume(&rt, &b, "unmount", "/dev/sdb1");
    until(&rt, &s, "the unmount's failure", || {
        failures.lock().unwrap().len() == 2
    });
    assert_eq!(
        *failures.lock().unwrap(),
        [
            (
                "/dev/sdb1".to_string(),
                "Not authorized to perform operation".to_string()
            ),
            (
                "/dev/sdb1".to_string(),
                "Error unmounting /dev/sdb1: target is busy".to_string()
            ),
        ]
    );
    assert_eq!(volume(&b, &rt, "/dev/sdb1"), before, "unchanged");
    s.shutdown();
}

#[test]
fn a_stick_is_unmounted_then_powered_off() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let w = world(vec![the_stick(true)]);
    let _fake = serve(&tokio, &bus.address, &w);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let failures = failures_of(&b);
    b.drives.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    until(&rt, &s, "the stick mounted", || {
        volume(&b, &rt, "/dev/sdb1").is_some_and(|v| v.mounted)
    });
    eject(&rt, &b, "/dev/sdb");
    until(&rt, &s, "gone", || all(&b, &rt).is_empty());
    settle(&rt, &s, Duration::from_millis(300));
    // Ejectable and able to be powered off, its medium fixed: powered off
    // only.
    assert_eq!(calls(&w), ["Unmount /dev/sdb1", "PowerOff /dev/sdb"]);
    assert!(failures.lock().unwrap().is_empty());
    assert!(available(&b, &rt));
    s.shutdown();
}

#[test]
fn a_disc_or_card_is_ejected_and_its_drive_left_on() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    // A USB disc drive and a USB card reader: each ejects, and each can
    // be powered off.
    let (mut dvd, mut disc) = disc_drive();
    dvd.can_power_off = true;
    disc[0].fs = Some(vec!["/run/media/tester/FILMS".into()]);
    let (mut reader, mut slot) = card_reader();
    reader.media_available = true;
    reader.can_power_off = true;
    slot.push(BlockSpec {
        id: "sdc1".into(),
        device: "/dev/sdc1".into(),
        drive: Some("Generic_SD_Reader".into()),
        label: "CAMERA".into(),
        size: 32_000_000_000,
        partition: Some(1),
        fs: Some(vec!["/run/media/tester/CAMERA".into()]),
        ..BlockSpec::default()
    });
    let w = world(vec![(dvd, disc), (reader, slot)]);
    let _fake = serve(&tokio, &bus.address, &w);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let failures = failures_of(&b);
    b.drives.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    until(&rt, &s, "the disc and the card", || {
        devices(&b, &rt) == ["/dev/sdc", "/dev/sr0"]
    });
    let disc = all(&b, &rt).into_iter().find(|d| d.device == "/dev/sr0");
    assert_eq!(
        disc.map(|d| (d.icon, d.can_eject)),
        Some(("media-optical-symbolic".to_string(), true))
    );
    eject(&rt, &b, "/dev/sr0");
    until(&rt, &s, "the disc out", || devices(&b, &rt) == ["/dev/sdc"]);
    eject(&rt, &b, "/dev/sdc");
    until(&rt, &s, "the card out", || all(&b, &rt).is_empty());
    settle(&rt, &s, Duration::from_millis(300));
    assert_eq!(
        calls(&w),
        [
            "Unmount /dev/sr0",
            "Eject /dev/sr0",
            "Unmount /dev/sdc1",
            "Eject /dev/sdc"
        ],
        "neither powered off"
    );
    {
        let w = w.lock().unwrap();
        let mut left: Vec<(&str, bool)> = w
            .drives
            .values()
            .map(|d| (d.device.as_str(), d.media_available))
            .collect();
        left.sort();
        assert_eq!(
            left,
            [("/dev/sdc", false), ("/dev/sr0", false)],
            "both drives still there, empty, for the next medium"
        );
    }
    assert!(failures.lock().unwrap().is_empty());
    s.shutdown();
}

#[test]
fn an_unlocked_encrypted_volume_is_unmounted_then_locked() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    // The stick's second partition is LUKS, unlocked and mounted.
    let (d, mut blocks) = the_stick(false);
    blocks[2].fs = None;
    blocks[2].encrypted = true;
    blocks.push(BlockSpec {
        id: "dm_2d0".into(),
        device: "/dev/mapper/luks-1234".into(),
        drive: None,
        label: "SECRET".into(),
        size: 7_900_000_000,
        backing: Some("sdb2".into()),
        fs: Some(vec!["/run/media/tester/SECRET".into()]),
        ..BlockSpec::default()
    });
    let w = world(vec![(d, blocks)]);
    // Powering off refused, so what the lock left can be seen.
    w.lock().unwrap().refuse.insert(
        "PowerOff".into(),
        (
            Refusal::NotAuthorizedDismissed,
            "Not authorized to perform operation".into(),
        ),
    );
    let _fake = serve(&tokio, &bus.address, &w);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let failures = failures_of(&b);
    b.drives.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    until(&rt, &s, "the unlocked volume", || {
        volume(&b, &rt, "/dev/mapper/luks-1234").is_some_and(|v| v.mounted)
    });
    let vols = |b: &strand_services::Builtin| -> Vec<String> {
        all(b, &rt)
            .into_iter()
            .flat_map(|d| d.volumes)
            .map(|v| v.device)
            .collect()
    };
    assert_eq!(vols(&b), ["/dev/sdb1", "/dev/mapper/luks-1234"]);
    eject(&rt, &b, "/dev/sdb");
    until(&rt, &s, "the power off refused", || {
        !failures.lock().unwrap().is_empty()
    });
    assert_eq!(
        calls(&w),
        [
            "Unmount /dev/mapper/luks-1234",
            "Lock /dev/sdb2",
            "PowerOff /dev/sdb"
        ]
    );
    assert_eq!(
        *failures.lock().unwrap(),
        [(
            "/dev/sdb".to_string(),
            "Not authorized to perform operation".to_string()
        )]
    );
    until(&rt, &s, "the cleartext volume gone", || {
        vols(&b) == ["/dev/sdb1"]
    });
    assert_eq!(devices(&b, &rt), ["/dev/sdb"], "still there");
    s.shutdown();
}

#[test]
fn an_eject_under_way_finishes_after_its_service_stops() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let w = world(vec![the_stick(true)]);
    // The unmount takes a while (umount flushing a slow stick).
    w.lock().unwrap().stall_unmount = true;
    let _fake = serve(&tokio, &bus.address, &w);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    b.drives.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    until(&rt, &s, "the stick mounted", || {
        volume(&b, &rt, "/dev/sdb1").is_some_and(|v| v.mounted)
    });
    eject(&rt, &b, "/dev/sdb");
    until(&rt, &s, "the unmount asked", || calls(&w).len() == 1);
    // The menu closes on its click: nobody reads `drives`, and it stops
    // with its unmount still under way.
    b.drives.release(&rt);
    rt.tick(strand_services::STOP_GRACE + Duration::from_millis(1));
    until(&rt, &s, "drives stopped", || !b.drives.running());
    w.lock().unwrap().stall_unmount = false;
    strand_test_support::wait::until(
        "the power off",
        Duration::from_secs(30),
        Duration::from_millis(20),
        || calls(&w).len() == 2,
    );
    assert_eq!(calls(&w), ["Unmount /dev/sdb1", "PowerOff /dev/sdb"]);
    assert!(w.lock().unwrap().drives.is_empty(), "powered off");
    s.shutdown();
}

#[test]
fn an_eject_stops_at_the_first_refusal() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let w = world(vec![the_stick(true)]);
    w.lock().unwrap().refuse.insert(
        "Unmount".into(),
        (
            Refusal::DeviceBusy,
            "Error unmounting /dev/sdb1: target is busy".into(),
        ),
    );
    let _fake = serve(&tokio, &bus.address, &w);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let failures = failures_of(&b);
    b.drives.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    until(&rt, &s, "the stick", || devices(&b, &rt) == ["/dev/sdb"]);
    eject(&rt, &b, "/dev/sdb");
    until(&rt, &s, "the failure", || {
        !failures.lock().unwrap().is_empty()
    });
    assert_eq!(
        *failures.lock().unwrap(),
        [(
            "/dev/sdb".to_string(),
            "Error unmounting /dev/sdb1: target is busy".to_string()
        )]
    );
    settle(&rt, &s, Duration::from_millis(300));
    assert_eq!(calls(&w), ["Unmount /dev/sdb1"], "no eject, no power off");
    assert_eq!(devices(&b, &rt), ["/dev/sdb"], "still there");
    assert!(volume(&b, &rt, "/dev/sdb1").is_some_and(|v| v.mounted));
    s.shutdown();
}

#[test]
fn a_second_mount_while_one_runs_is_dropped() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let w = world(vec![the_stick(false)]);
    w.lock().unwrap().stall_mount = true;
    let _fake = serve(&tokio, &bus.address, &w);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let failures = failures_of(&b);
    b.drives.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    until(&rt, &s, "the stick", || {
        volume(&b, &rt, "/dev/sdb1").is_some()
    });
    on_volume(&rt, &b, "mount", "/dev/sdb1");
    until(&rt, &s, "the mount asked", || calls(&w).len() == 1);
    on_volume(&rt, &b, "mount", "/dev/sdb1");
    settle(&rt, &s, Duration::from_millis(500));
    assert_eq!(calls(&w), ["Mount /dev/sdb1 {}"], "asked once");
    w.lock().unwrap().stall_mount = false;
    until(&rt, &s, "mounted", || {
        volume(&b, &rt, "/dev/sdb1").is_some_and(|v| v.mounted)
    });
    settle(&rt, &s, Duration::from_millis(300));
    assert_eq!(calls(&w), ["Mount /dev/sdb1 {}"]);
    assert!(failures.lock().unwrap().is_empty());
    s.shutdown();
}

#[test]
fn udisks_restarting_is_read_afresh() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let w = world(vec![the_stick(false)]);
    let fake = serve(&tokio, &bus.address, &w);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    b.drives.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    until(&rt, &s, "the stick", || devices(&b, &rt) == ["/dev/sdb"]);
    drop(fake);
    until(&rt, &s, "UDisks2 gone", || {
        !available(&b, &rt) && all(&b, &rt).is_empty()
    });
    let w2 = world(vec![stick("Other_1", "sde", "OTHER", "01hotplug/1")]);
    let _fake2 = serve(&tokio, &bus.address, &w2);
    until(&rt, &s, "UDisks2 back", || {
        available(&b, &rt) && devices(&b, &rt) == ["/dev/sde"]
    });
    assert_eq!(b.drives.starts(), 1, "no service restart");
    s.shutdown();
}

#[test]
fn without_udisks_mount_and_eject_fail_as_events() {
    // No system bus: the body fails and backs off, and an action is
    // refused as a `failed` naming its device.
    let gone = strand_services::Bus::Address("unix:path=/nonexistent/strand-test-bus".into());
    let rt = Runtime::new();
    let (s, b) = services(
        &rt,
        strand_services::Buses {
            session: gone.clone(),
            system: gone,
        },
    );
    let failures = failures_of(&b);
    b.drives.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    until(&rt, &s, "the failed body", || !b.drives.running());
    assert!(!available(&b, &rt));
    on_volume(&rt, &b, "mount", "/dev/sdb1");
    eject(&rt, &b, "/dev/sdb");
    until(&rt, &s, "the refusals", || {
        failures.lock().unwrap().len() == 2
    });
    let f = failures.lock().unwrap().clone();
    assert_eq!(f[0].0, "/dev/sdb1");
    assert_eq!(f[1].0, "/dev/sdb");
    for (_, error) in &f {
        assert!(error.contains("not running"), "{f:?}");
    }
    s.shutdown();

    // A bus without UDisks2 (nothing to activate it): not available, and
    // an action fails saying so.
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let failures = failures_of(&b);
    b.drives.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert!(!available(&b, &rt));
    on_volume(&rt, &b, "mount", "/dev/sdb1");
    until(&rt, &s, "the failure", || {
        !failures.lock().unwrap().is_empty()
    });
    assert_eq!(
        *failures.lock().unwrap(),
        [(
            "/dev/sdb1".to_string(),
            "UDisks2 is not running".to_string()
        )]
    );
    s.shutdown();
}
