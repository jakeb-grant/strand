//! The schema texts of the builtin services, without their runtime: what
//! `strand-services` serves and what the language extends its builtin
//! schema with (`Schema::extend`). `strand-dev`'s LSP checks, hovers and
//! completes against them without linking tokio, zbus or PipeWire.
//!
//! Each service module of `strand-services` uses its constant here as
//! its `SCHEMA` (architecture.md, "strand-services-schema").

/// `system`: the portal's appearance settings, the host name, kernel and
/// uptime.
pub const SYSTEM: &str = include_str!("system.schema");
/// `cpu`: processor load (procfs).
pub const CPU: &str = include_str!("cpu.schema");
/// `memory`: memory use (procfs).
pub const MEMORY: &str = include_str!("memory.schema");

/// (history) The builtin services that sample on a clock, each with how
/// often it samples while a reader is visible: kept by a `graph` and
/// hidden, each samples at the larger of that and its kept period, and
/// its hidden samples are drawn as ramps over that period. The eval
/// sandbox's stand-ins say the same, so a replayed graph draws them as
/// the live one did. Every other builtin reports its changes as they
/// happen.
pub const POLLED: [(&str, std::time::Duration); 3] = [
    ("cpu", std::time::Duration::from_secs(1)),
    ("memory", std::time::Duration::from_secs(1)),
    ("disk", std::time::Duration::from_secs(10)),
];

/// `battery`: UPower's display device and power sources.
pub const BATTERY: &str = include_str!("battery.schema");
/// `brightness`: the backlight (sysfs, logind).
pub const BRIGHTNESS: &str = include_str!("brightness.schema");
/// `network`: NetworkManager or iwd.
pub const NETWORK: &str = include_str!("network.schema");
/// `bluetooth`: BlueZ.
pub const BLUETOOTH: &str = include_str!("bluetooth.schema");
/// `notifications`: the notification server.
pub const NOTIFICATIONS: &str = include_str!("notifications.schema");
/// `media`: the active MPRIS player.
pub const MEDIA: &str = include_str!("media.schema");
/// `tray`: StatusNotifierItem host and DBusMenu.
pub const TRAY: &str = include_str!("tray.schema");

/// `apps`: installed apps (desktop entries), fuzzy search, frecency.
pub const APPS: &str = include_str!("apps.schema");
/// `windows`: toplevels (`ext-foreign-toplevel-list`, compositor IPC).
pub const WINDOWS: &str = include_str!("windows.schema");
/// `workspaces`: `ext-workspace-v1` and compositor IPC.
pub const WORKSPACES: &str = include_str!("workspaces.schema");
/// `wm`: the compositor's name and config reloads.
pub const WM: &str = include_str!("wm.schema");
/// `audio`: PipeWire devices.
pub const AUDIO: &str = include_str!("audio.schema");
/// `auth`: the lock screen's password check (the `strand-auth` helper).
pub const AUTH: &str = include_str!("auth.schema");
/// `session`: the login session and the machine's power (logind).
pub const SESSION: &str = include_str!("session.schema");
/// `outputs`: the monitors' configuration (wlr-output-management).
pub const OUTPUTS: &str = include_str!("outputs.schema");
/// `clipboard`: the selection, copies from handlers and the history
/// (`ext-data-control-v1`, else `wlr-data-control`).
pub const CLIPBOARD: &str = include_str!("clipboard.schema");
/// `disk`: the mounted filesystems' usage (`statvfs`).
pub const DISK: &str = include_str!("disk.schema");
/// `keyboard`: the lock keys and the layout.
pub const KEYBOARD: &str = include_str!("keyboard.schema");

/// Every builtin service's schema text, in registration order.
pub fn schemas() -> Vec<&'static str> {
    vec![
        SYSTEM,
        CPU,
        MEMORY,
        BATTERY,
        BRIGHTNESS,
        NETWORK,
        BLUETOOTH,
        NOTIFICATIONS,
        MEDIA,
        TRAY,
        APPS,
        WINDOWS,
        WORKSPACES,
        WM,
        AUDIO,
        AUTH,
        SESSION,
        OUTPUTS,
        CLIPBOARD,
        DISK,
        KEYBOARD,
    ]
}
