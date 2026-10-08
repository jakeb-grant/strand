//! The schema texts of the builtin services, without their runtime: what
//! `strand-services` serves and what the language extends its builtin
//! schema with (`Schema::extend`). `strand-dev`'s LSP checks, hovers and
//! completes against them without linking tokio, zbus or PipeWire.
//!
//! Each service module of `strand-services` uses its constant here as
//! its `SCHEMA` (architecture.md, "strand-services-schema").

/// `system`: the portal's appearance settings and the host name.
pub const SYSTEM: &str = include_str!("system.schema");
/// `cpu`: processor load (procfs).
pub const CPU: &str = include_str!("cpu.schema");
/// `memory`: memory use (procfs).
pub const MEMORY: &str = include_str!("memory.schema");

/// `battery`: UPower's display device and power sources.
pub const BATTERY: &str = include_str!("battery.schema");
/// `brightness`: the backlight (sysfs, logind).
pub const BRIGHTNESS: &str = include_str!("brightness.schema");
/// `network`: NetworkManager.
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
    ]
}
