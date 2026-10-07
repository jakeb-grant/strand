//! A fake Hyprland: two Unix sockets that replay Hyprland 0.56.2's
//! traffic as reconstructed from its source
//! (`tests/fixtures/hyprland-0.56.2`).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use strand_services::wm::Backend;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use super::fixture;

enum Ev {
    Burst(Vec<u8>),
    /// Close the event connection (Hyprland restarting, a lost socket).
    Drop,
}

/// A fake Hyprland instance answering from one fixture directory.
pub struct FakeHyprland {
    _dir: Option<tempfile::TempDir>,
    pub backend: Backend,
    scene: Arc<Mutex<&'static str>>,
    requests: Arc<Mutex<Vec<String>>>,
    events: UnboundedSender<Ev>,
}

impl FakeHyprland {
    pub fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let backend = Backend::hyprland_in(dir.path(), "a1b2c3_1759780000_123456789");
        let mut fake = Self::start_at(backend);
        fake._dir = Some(dir);
        fake
    }

    /// A fake listening where `backend` (a Hyprland one) points.
    pub fn start_at(backend: Backend) -> Self {
        let Backend::Hyprland { requests, events } = backend.clone() else {
            unreachable!()
        };
        std::fs::create_dir_all(requests.parent().unwrap()).unwrap();
        let scene = Arc::new(Mutex::new("boot"));
        let log = Arc::new(Mutex::new(Vec::new()));
        let s1 = UnixListener::bind(&requests).unwrap();
        let s2 = UnixListener::bind(&events).unwrap();
        {
            let scene = scene.clone();
            let log = log.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut conn, _)) = s1.accept().await else {
                        return;
                    };
                    // Hyprland reads one request of up to 1023 bytes.
                    let mut buf = vec![0u8; 1023];
                    let n = conn.read(&mut buf).await.unwrap();
                    let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let reply = answer(*scene.lock().unwrap(), &req);
                    log.lock().unwrap().push(req);
                    let _ = conn.write_all(&reply).await;
                    // Then closes.
                }
            });
        }
        let (tx, mut rx) = unbounded_channel::<Ev>();
        tokio::spawn(async move {
            loop {
                let Ok((mut conn, _)) = s2.accept().await else {
                    return;
                };
                loop {
                    match rx.recv().await {
                        Some(Ev::Burst(b)) => {
                            // One write: the burst arrives together.
                            if conn.write_all(&b).await.is_err() {
                                break;
                            }
                        }
                        Some(Ev::Drop) => break,
                        None => return,
                    }
                }
            }
        });
        Self {
            _dir: None,
            backend,
            scene,
            requests: log,
            events: tx,
        }
    }

    pub fn set_scene(&self, s: &'static str) {
        *self.scene.lock().unwrap() = s;
    }

    pub fn send(&self, burst: &str) {
        self.send_bytes(burst.as_bytes());
    }

    pub fn send_bytes(&self, burst: &[u8]) {
        self.events.send(Ev::Burst(burst.to_vec())).unwrap();
    }

    /// Closes the event connection (Hyprland restarting, a lost socket).
    pub fn drop_events(&self) {
        self.events.send(Ev::Drop).unwrap();
    }

    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

/// The title Hyprland copies byte for byte from an XWayland window whose
/// `WM_NAME` is Latin-1 (`STRING`): `café ÿ`, not UTF-8.
pub const LATIN1_TITLE: &[u8] = b"caf\xe9 \xff";

pub fn answer(scene: &str, req: &str) -> Vec<u8> {
    // `latin1`: the `opened` moment with foot's title in Latin-1, raw in
    // the JSON as HyprCtl's escapeJSONStrings leaves bytes over 0x7f.
    let (dir_scene, latin1) = match scene {
        "latin1" => ("opened", true),
        s => (s, false),
    };
    let dir: PathBuf = fixture("hyprland-0.56.2").join(dir_scene);
    let file = |name: &str| std::fs::read(dir.join(name)).unwrap();
    match req {
        "j/monitors" => file("monitors.json"),
        "j/workspaces" => file("workspaces.json"),
        "j/clients" if latin1 => {
            let text = String::from_utf8(file("clients.json")).unwrap();
            let (head, tail) = text.split_once("\"title\": \"foot\"").unwrap();
            let mut out = head.as_bytes().to_vec();
            out.extend_from_slice(b"\"title\": \"");
            out.extend_from_slice(LATIN1_TITLE);
            out.extend_from_slice(b"\"");
            out.extend_from_slice(tail.as_bytes());
            out
        }
        "j/clients" => file("clients.json"),
        "j/activewindow" => file("activewindow.json"),
        r if r.starts_with("dispatch ") => b"ok".to_vec(),
        _ => b"unknown request".to_vec(),
    }
}
