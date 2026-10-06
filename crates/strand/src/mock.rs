//! A mock desktop for the services that land in M3 (`STRAND_MOCK=desktop`
//! on `strand run`): workspaces, a focused window, a battery, an audio
//! sink, a tray item, two notifications and three apps, so the example
//! shells of design.md show real content before their services exist.
//! Screenshots and the sway acceptance tests use it; a shell never needs
//! it.
//!
//! `STRAND_MOCK=acceptance` is the deterministic host the M2 acceptance
//! tests drive: the same desktop with no notifications at boot, two more
//! workspaces on `HEADLESS-2`, and the clock frozen at
//! [`MOCK_TIME`] in UTC. Either mock takes the IPC command `mock`
//! ([`command`]): a notification arriving, a volume, mute or brightness
//! change, as the M3 services will report them.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value as Json;
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::{MOCK_TIME, SchemaHost};
use strand_core::Runtime;

/// Which mock `STRAND_MOCK` asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Mock {
    /// The screen the first four workspaces are on (`STRAND_MOCK_SCREEN`,
    /// default `HEADLESS-1`).
    pub screen: String,
    /// `acceptance`: no notifications at boot, workspaces on a second
    /// screen, the clock frozen.
    pub acceptance: bool,
}

/// Whether `STRAND_MOCK` (`desktop` or `acceptance`) asks for a mock.
pub(crate) fn requested() -> Option<Mock> {
    let kind = std::env::var_os("STRAND_MOCK")?;
    let acceptance = match kind.to_str()? {
        "desktop" => false,
        "acceptance" => true,
        _ => return None,
    };
    let screen = std::env::var("STRAND_MOCK_SCREEN").unwrap_or_else(|_| "HEADLESS-1".into());
    Some(Mock { screen, acceptance })
}

/// The wall time the acceptance mock's clock is frozen at.
pub(crate) fn frozen_time() -> Option<SystemTime> {
    requested()
        .filter(|m| m.acceptance)
        .map(|_| UNIX_EPOCH + Duration::from_secs(MOCK_TIME))
}

/// Fills `host` with the mock desktop.
pub(crate) fn desktop(rt: &Runtime, host: &SchemaHost, mock: &Mock) {
    let screen = mock.screen.as_str();
    let set = |path: &str, v: Value| {
        if let Err(e) = host.set(rt, path, v) {
            log::warn!("mock {path}: {e}");
        }
    };
    let ws = |id: i64, focused: bool, occupied: bool| {
        let screen = if id > 4 { "HEADLESS-2" } else { screen };
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
    let mut all = vec![
        ws(1, false, true),
        ws(2, true, true),
        ws(3, false, true),
        ws(4, false, false),
    ];
    if mock.acceptance {
        all.extend([ws(5, false, true), ws(6, false, false)]);
    }
    set("workspaces.focused", all[1].clone());
    set("workspaces.all", Value::list(all));
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
    set("brightness.available", Value::Bool(true));
    set("brightness.level", Value::float(0.5));
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
    if !mock.acceptance {
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
    }
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

/// The sink's icon name for a volume and mute state, as PipeWire's
/// desktop integrations name them.
fn volume_icon(volume: f64, muted: bool) -> &'static str {
    if muted || volume <= 0.0 {
        "audio-volume-muted-symbolic"
    } else if volume < 1.0 / 3.0 {
        "audio-volume-low-symbolic"
    } else if volume < 2.0 / 3.0 {
        "audio-volume-medium-symbolic"
    } else {
        "audio-volume-high-symbolic"
    }
}

/// A notification from the IPC command's `notify` object: `id` (an
/// integer), `app`, `icon`, `summary`, `body`, `urgency` (`low`,
/// `normal`, `critical`), `timeout_ms` and `actions` (labels).
fn notification(host: &SchemaHost, n: &Json) -> Result<Value, String> {
    let text = |k: &str| n.get(k).and_then(Json::as_str).unwrap_or("");
    let id = n
        .get("id")
        .and_then(Json::as_i64)
        .ok_or("`notify` needs an integer `id`")?;
    let app = host.record(
        "NotificationApp",
        &[
            ("name", Value::text(text("app"))),
            ("icon", Value::text(text("icon"))),
        ],
    );
    let urgency = match n.get("urgency").and_then(Json::as_str) {
        None => "normal",
        Some(u @ ("low" | "normal" | "critical")) => u,
        Some(u) => return Err(format!("unknown urgency `{u}`")),
    };
    let timeout = match n.get("timeout_ms").and_then(Json::as_u64) {
        Some(ms) => Value::from(Duration::from_millis(ms)),
        None => Value::Null,
    };
    let actions = n
        .get("actions")
        .and_then(Json::as_array)
        .map(|a| {
            a.iter()
                .enumerate()
                .filter_map(|(i, l)| {
                    let label = l.as_str()?;
                    Some(host.record(
                        "NotificationAction",
                        &[
                            ("id", Value::text(format!("a{i}"))),
                            ("label", Value::text(label)),
                        ],
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(host.record(
        "Notification",
        &[
            ("id", Value::int(id)),
            ("app", app),
            ("summary", Value::text(text("summary"))),
            ("body", Value::text(text("body"))),
            ("urgency", host.variant("Urgency", urgency)),
            ("timeout", timeout),
            ("actions", Value::list(actions)),
        ],
    ))
}

/// The IPC command `mock` (`{"v": 1, "cmd": "mock", …}`): the mock's
/// services report a change, in this order when one command holds
/// several: `notify` (an object, see [`notification`]: it joins the
/// popups and `notifications.received` fires), `volume` (0 to 1),
/// `muted` (a boolean), `brightness` (0 to 1). A write the shell makes
/// (`audio.sink.volume -= …`) needs no command: the mock applies it.
pub(crate) fn command(rt: &Runtime, host: &SchemaHost, req: &Json) -> Result<(), String> {
    let err = |e: strand_core::Error| e.to_string();
    let mut done = false;
    if let Some(n) = req.get("notify") {
        let n = notification(host, n)?;
        let types = host.types();
        let key = n.identity(types);
        let cur = host.get(rt, "notifications.popups").map_err(err)?;
        let mut list: Vec<Value> = cur
            .as_list()
            .map(<[Value]>::to_vec)
            .unwrap_or_default()
            .into_iter()
            .filter(|m| m.identity(types) != key)
            .collect();
        list.push(n.clone());
        host.set(rt, "notifications.popups", Value::list(list))
            .map_err(err)?;
        host.emit(rt, "notifications.received", vec![n])
            .map_err(err)?;
        done = true;
    }
    let sink = |field: &str| host.get(rt, &format!("audio.sink.{field}"));
    let volume = req.get("volume").and_then(Json::as_f64);
    let muted = req.get("muted").and_then(Json::as_bool);
    if volume.is_some() || muted.is_some() {
        if let Some(v) = volume {
            host.set(rt, "audio.sink.volume", Value::float(v.clamp(0.0, 1.0)))
                .map_err(err)?;
        }
        if let Some(m) = muted {
            host.set(rt, "audio.sink.muted", Value::Bool(m))
                .map_err(err)?;
        }
        let v = sink("volume").map_err(err)?.as_f64().unwrap_or(0.0);
        let m = sink("muted").map_err(err)?.as_bool().unwrap_or(false);
        host.set(rt, "audio.sink.icon", Value::text(volume_icon(v, m)))
            .map_err(err)?;
        done = true;
    }
    if let Some(b) = req.get("brightness").and_then(Json::as_f64) {
        host.set(rt, "brightness.level", Value::float(b.clamp(0.0, 1.0)))
            .map_err(err)?;
        done = true;
    }
    if done {
        Ok(())
    } else {
        Err("`mock` needs `notify`, `volume`, `muted` or `brightness`".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_follow_volume_and_mute() {
        assert_eq!(volume_icon(0.5, true), "audio-volume-muted-symbolic");
        assert_eq!(volume_icon(0.0, false), "audio-volume-muted-symbolic");
        assert_eq!(volume_icon(0.2, false), "audio-volume-low-symbolic");
        assert_eq!(volume_icon(0.5, false), "audio-volume-medium-symbolic");
        assert_eq!(volume_icon(0.9, false), "audio-volume-high-symbolic");
    }

    #[test]
    fn mock_commands_reach_the_services() {
        let rt = Runtime::new();
        let types = strand_compiler::schema::Schema::builtin().types.clone();
        let host = SchemaHost::new(&rt, &types, None);
        let mock = Mock {
            screen: "HEADLESS-1".into(),
            acceptance: true,
        };
        desktop(&rt, &host, &mock);
        let popups = |host: &SchemaHost| {
            host.get(&rt, "notifications.popups")
                .unwrap()
                .as_list()
                .map_or(0, <[Value]>::len)
        };
        assert_eq!(popups(&host), 0, "the acceptance mock boots with none");
        let req: Json = serde_json::from_str(
            r#"{"notify": {"id": 7, "app": "Chat", "summary": "Hi", "urgency": "critical",
                "timeout_ms": 1500, "actions": ["Reply"]}}"#,
        )
        .unwrap();
        command(&rt, &host, &req).unwrap();
        command(&rt, &host, &req).unwrap();
        assert_eq!(popups(&host), 1, "the same id replaces");
        let n = host.get(&rt, "notifications.popups").unwrap();
        let n = &n.as_list().unwrap()[0];
        let t = host.types();
        assert_eq!(n.field(t, "summary"), Some(&Value::text("Hi")));
        assert_eq!(
            n.field(t, "timeout"),
            Some(&Value::from(Duration::from_millis(1500)))
        );
        command(
            &rt,
            &host,
            &serde_json::json!({"volume": 0.2, "brightness": 0.9}),
        )
        .unwrap();
        assert_eq!(
            host.get(&rt, "audio.sink.volume").unwrap(),
            Value::float(0.2)
        );
        assert_eq!(
            host.get(&rt, "audio.sink.icon").unwrap(),
            Value::text("audio-volume-low-symbolic")
        );
        assert_eq!(
            host.get(&rt, "brightness.level").unwrap(),
            Value::float(0.9)
        );
        command(&rt, &host, &serde_json::json!({"muted": true})).unwrap();
        assert_eq!(
            host.get(&rt, "audio.sink.icon").unwrap(),
            Value::text("audio-volume-muted-symbolic")
        );
        assert!(command(&rt, &host, &serde_json::json!({})).is_err());
        assert!(
            command(
                &rt,
                &host,
                &serde_json::json!({"notify": {"id": 1, "urgency": "loud"}})
            )
            .is_err()
        );
        // Six workspaces, two on the second screen.
        let all = host.get(&rt, "workspaces.all").unwrap();
        assert_eq!(all.as_list().map_or(0, <[Value]>::len), 6);
    }
}
