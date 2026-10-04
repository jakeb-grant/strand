//! Change sources that become writes into the reactive graph.
//!
//! Directory (never file) inotify watches acting on `CLOSE_WRITE` and
//! `MOVED_TO`, symlink-target watches, settings TOML and wallpapers, portal
//! `SettingChanged`, monitor hotplug, compositor reload events and IPC.
//! Saves are coalesced (15 ms) and deduplicated by BLAKE3 hash.
//!
//! See `docs/design.md`, "Live reload and real-time config changes". Lands in M1.
