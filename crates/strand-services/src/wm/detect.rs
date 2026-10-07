//! Which compositor runs, from the environment it gives its clients.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::model::CompositorKind;

/// The IPC endpoint of a detected compositor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Backend {
    /// Hyprland: `.socket.sock` takes one request per connection,
    /// `.socket2.sock` streams events.
    Hyprland {
        /// The request socket.
        requests: PathBuf,
        /// The event socket.
        events: PathBuf,
    },
    /// niri: one JSON socket (`$NIRI_SOCKET`).
    Niri {
        /// The socket.
        socket: PathBuf,
    },
    /// sway: the i3 IPC socket (`$SWAYSOCK`).
    Sway {
        /// The socket.
        socket: PathBuf,
    },
}

impl Backend {
    /// The compositor this backend talks to.
    pub fn kind(&self) -> CompositorKind {
        match self {
            Self::Hyprland { .. } => CompositorKind::Hyprland,
            Self::Niri { .. } => CompositorKind::Niri,
            Self::Sway { .. } => CompositorKind::Sway,
        }
    }

    /// Hyprland's sockets for an instance signature under a runtime dir:
    /// `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/` (Hyprland
    /// 0.40 and later).
    pub fn hyprland_in(runtime_dir: &Path, signature: &str) -> Self {
        let dir = runtime_dir.join("hypr").join(signature);
        Self::Hyprland {
            requests: dir.join(".socket.sock"),
            events: dir.join(".socket2.sock"),
        }
    }

    fn socket_exists(&self) -> bool {
        match self {
            Self::Hyprland { events, .. } => events.exists(),
            Self::Niri { socket } | Self::Sway { socket } => socket.exists(),
        }
    }
}

/// Detects the compositor from the process environment.
pub fn detect() -> Option<Backend> {
    detect_with(|k| std::env::var_os(k))
}

/// Detects the compositor from `env`: `HYPRLAND_INSTANCE_SIGNATURE`,
/// `NIRI_SOCKET` (with the `niri` feature) and `SWAYSOCK`, keeping only
/// those whose socket exists. A nested compositor inherits its parent's
/// variables, so when several remain the one `XDG_CURRENT_DESKTOP` names
/// wins, then Hyprland, niri, sway in that order.
pub fn detect_with(env: impl Fn(&str) -> Option<OsString>) -> Option<Backend> {
    let mut found = Vec::new();
    if let Some(sig) = env("HYPRLAND_INSTANCE_SIGNATURE").filter(|s| !s.is_empty()) {
        let sig = sig.to_string_lossy().into_owned();
        let mut candidates = Vec::new();
        if let Some(rt) = env("XDG_RUNTIME_DIR").filter(|s| !s.is_empty()) {
            candidates.push(Backend::hyprland_in(Path::new(&rt), &sig));
        }
        // Before 0.40 the sockets lived under /tmp/hypr.
        candidates.push(Backend::hyprland_in(Path::new("/tmp"), &sig));
        if let Some(b) = candidates.into_iter().find(Backend::socket_exists) {
            found.push(b);
        }
    }
    #[cfg(feature = "niri")]
    if let Some(s) = env("NIRI_SOCKET").filter(|s| !s.is_empty()) {
        found.push(Backend::Niri {
            socket: PathBuf::from(s),
        });
    }
    if let Some(s) = env("SWAYSOCK").filter(|s| !s.is_empty()) {
        found.push(Backend::Sway {
            socket: PathBuf::from(s),
        });
    }
    found.retain(Backend::socket_exists);
    let desktop = env("XDG_CURRENT_DESKTOP")
        .map(|d| d.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let named = found.iter().position(|b| {
        desktop
            .split(':')
            .any(|d| d == b.kind().name().to_ascii_lowercase())
    });
    match named {
        Some(i) => Some(found.swap_remove(i)),
        None => found.into_iter().next(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::os::unix::net::UnixListener;

    fn env(vars: &[(&str, &Path)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.as_os_str().to_owned()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn finds_each_compositor_by_its_socket() {
        let dir = tempfile::tempdir().unwrap();
        let hypr = dir.path().join("hypr/abc");
        std::fs::create_dir_all(&hypr).unwrap();
        let _h = UnixListener::bind(hypr.join(".socket2.sock")).unwrap();
        let sway = dir.path().join("sway-ipc.sock");
        let _s = UnixListener::bind(&sway).unwrap();
        let niri = dir.path().join("niri.sock");
        let _n = UnixListener::bind(&niri).unwrap();

        let b = detect_with(env(&[
            ("XDG_RUNTIME_DIR", dir.path()),
            ("HYPRLAND_INSTANCE_SIGNATURE", Path::new("abc")),
        ]))
        .unwrap();
        assert_eq!(b.kind(), CompositorKind::Hyprland);
        assert_eq!(
            b,
            Backend::Hyprland {
                requests: hypr.join(".socket.sock"),
                events: hypr.join(".socket2.sock")
            }
        );
        let b = detect_with(env(&[("SWAYSOCK", &sway)])).unwrap();
        assert_eq!(
            b,
            Backend::Sway {
                socket: sway.clone()
            }
        );
        #[cfg(feature = "niri")]
        assert_eq!(
            detect_with(env(&[("NIRI_SOCKET", &niri)])),
            Some(Backend::Niri {
                socket: niri.clone()
            })
        );
        // A stale variable (no socket) is skipped.
        assert_eq!(
            detect_with(env(&[("SWAYSOCK", &dir.path().join("gone"))])),
            None
        );
        // sway nested in Hyprland: XDG_CURRENT_DESKTOP decides.
        let b = detect_with(env(&[
            ("XDG_RUNTIME_DIR", dir.path()),
            ("HYPRLAND_INSTANCE_SIGNATURE", Path::new("abc")),
            ("SWAYSOCK", &sway),
            ("XDG_CURRENT_DESKTOP", Path::new("sway:wlroots")),
        ]))
        .unwrap();
        assert_eq!(b.kind(), CompositorKind::Sway);
        assert_eq!(detect_with(env(&[])), None);
    }
}
