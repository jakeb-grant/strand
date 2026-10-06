//! A mock desktop for the services that land in M3 (`STRAND_MOCK=desktop`
//! on `strand run`): workspaces, a focused window, a battery, an audio
//! sink, a tray item, two notifications and three apps, so the example
//! shells of design.md show real content before their services exist.
//! Screenshots and the sway acceptance tests use it; a shell never needs
//! it.

use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_core::Runtime;

/// Whether `STRAND_MOCK=desktop` asks for the mock desktop, and on which
/// screen its workspaces are (`STRAND_MOCK_SCREEN`, default `HEADLESS-1`).
pub(crate) fn requested() -> Option<String> {
    (std::env::var_os("STRAND_MOCK")? == "desktop")
        .then(|| std::env::var("STRAND_MOCK_SCREEN").unwrap_or_else(|_| "HEADLESS-1".into()))
}

/// Fills `host` with the mock desktop; its workspaces are on `screen`.
pub(crate) fn desktop(rt: &Runtime, host: &SchemaHost, screen: &str) {
    let set = |path: &str, v: Value| {
        if let Err(e) = host.set(rt, path, v) {
            log::warn!("mock {path}: {e}");
        }
    };
    let ws = |id: i64, focused: bool, occupied: bool| {
        host.record(
            "Workspace",
            &[
                ("id", Value::int(id)),
                ("name", Value::text(id.to_string())),
                ("screen", Value::text(screen)),
                ("focused", Value::Bool(focused)),
                ("occupied", Value::Bool(occupied)),
            ],
        )
    };
    set(
        "workspaces.all",
        Value::list(vec![
            ws(1, false, true),
            ws(2, true, true),
            ws(3, false, true),
            ws(4, false, false),
        ]),
    );
    let win = host.record(
        "Window",
        &[
            ("id", Value::text("w1")),
            ("title", Value::text("strand — docs/design.md — Helix")),
        ],
    );
    set("windows.focused", win);
    set("battery.present", Value::Bool(true));
    set("battery.percent", Value::float(0.87));
    set("battery.icon", Value::text("battery-good-symbolic"));
    set(
        "battery.time_left",
        Value::from(std::time::Duration::from_secs(4 * 3600 + 20 * 60)),
    );
    let sink = host.record(
        "AudioDevice",
        &[
            ("id", Value::int(40)),
            ("name", Value::text("speakers")),
            ("volume", Value::float(0.6)),
            ("icon", Value::text("audio-volume-medium-symbolic")),
        ],
    );
    set("audio.sink", sink);
    let tray = host.record(
        "TrayItem",
        &[
            ("id", Value::text("nm-applet")),
            ("icon", Value::text("network-wireless")),
        ],
    );
    set("tray.items", Value::list(vec![tray]));
    // Icon names the Adwaita theme ships (as `-symbolic`; the lookup
    // falls back to that variant), so screenshots show them.
    let note = |id: i64, app: (&str, &str), summary: &str, body: &str, urgency: &str| {
        let napp = host.record(
            "NotificationApp",
            &[("name", Value::text(app.0)), ("icon", Value::text(app.1))],
        );
        host.record(
            "Notification",
            &[
                ("id", Value::int(id)),
                ("app", napp),
                ("summary", Value::text(summary)),
                ("body", Value::text(body)),
                ("urgency", host.variant("Urgency", urgency)),
            ],
        )
    };
    set(
        "notifications.popups",
        Value::list(vec![
            note(
                1,
                ("Mail", "mail-unread"),
                "New message",
                "<b>Ada</b>: the <i>layout</i> pass is in — see <a href=\"https://x\">the PR</a>",
                "normal",
            ),
            note(
                2,
                ("Battery", "battery-caution"),
                "Battery low",
                "Plug in soon",
                "critical",
            ),
        ]),
    );
    // With a `comment`, as real .desktop entries have: the launcher's
    // second line.
    let app = |id: &str, name: &str, icon: &str, comment: &str| {
        host.record(
            "App",
            &[
                ("id", Value::text(id)),
                ("name", Value::text(name)),
                ("icon", Value::text(icon)),
                ("comment", Value::text(comment)),
            ],
        )
    };
    set(
        "apps.all",
        Value::list(vec![
            app("firefox", "Firefox", "web-browser", "Browse the web"),
            app("foot", "Foot", "utilities-terminal", "Terminal emulator"),
            app("files", "Files", "system-file-manager", "Manage files"),
        ]),
    );
}
