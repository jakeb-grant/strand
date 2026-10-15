//! `keyboard`: the lock keys (`caps_lock`, `num_lock`) and the active
//! layout's name (`layout`), gap 31 (decisions.md m6-service-gaps). Its
//! fields are streams: nothing runs for one nobody shows.
//!
//! The sources, in order:
//!
//! 1. While a Strand surface has keyboard focus (a lock screen, a
//!    launcher), its own `wl_keyboard`: the host hands the modifiers'
//!    lock state and the keymap's name for the active group to
//!    [`focused`] as they change (strand-surface's
//!    `SurfaceHost::keyboard_state`), and `None` when the focus leaves.
//!    Exact as keys are pressed, and compositor-agnostic.
//! 2. Otherwise the lock keys come from the kernel's keyboard lights,
//!    `<sys>/class/leds/*::capslock` and `*::numlock` ([`read_leds`]):
//!    any keyboard's light on is on. A light's brightness raises no event
//!    (input-leds sets no `brightness_hw_changed`), and no compositor IPC
//!    reports the lock keys with events, so they are read every
//!    [`LED_PERIOD`], only while a visible reader shows `caps_lock` or
//!    `num_lock` and no focused report holds; at once when one starts
//!    to, and when the focus leaves. A read that changes nothing sends
//!    nothing, so a steady state wakes only the services thread.
//! 3. The layout, when no focused report names one, from the compositor:
//!    the shared compositor hub's [`WmChange::KeyboardLayout`] (Hyprland,
//!    sway, niri), subscribed only while `layout` is watched.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::watch;

use crate::wm::{WmChange, WmSubscription};
use crate::{Cx, Msg, ServiceError, Store, service};

/// The schema the `keyboard` service serves.
pub const SCHEMA: &str = strand_services_schema::KEYBOARD;

/// How often the keyboard lights are read while shown and unfocused.
pub const LED_PERIOD: Duration = Duration::from_millis(250);

/// What a focused Strand surface's keyboard reports.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Focused {
    /// Caps Lock is on.
    pub caps_lock: bool,
    /// Num Lock is on.
    pub num_lock: bool,
    /// The keymap's name for the active layout, if it names one.
    pub layout: Option<String>,
}

static FOCUSED: LazyLock<watch::Sender<Option<Focused>>> =
    LazyLock::new(|| watch::Sender::new(None));

/// The keyboard state of the Strand surface with keyboard focus, or
/// `None` when none has it. Any thread may call it, whether or not the
/// service runs; a running one shows it at once, a later one starts from
/// it.
pub fn focused(state: Option<Focused>) {
    FOCUSED.send_if_modified(|s| {
        if *s == state {
            return false;
        }
        *s = state;
        true
    });
}

/// Where a `keyboard` service reads the lights and how often (tests
/// point it at a sysfs of their own; [`configure`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// The sysfs root: the lights are under `<sys>/class/leds`.
    pub sys: PathBuf,
    /// How often the lights are read while shown and unfocused.
    pub led_period: Duration,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            sys: PathBuf::from("/sys"),
            led_period: LED_PERIOD,
        }
    }
}

static CONFIG: Mutex<Option<Config>> = Mutex::new(None);

/// Use `config` for `keyboard` services started from now on (`None`:
/// the defaults again).
pub fn configure(config: Option<Config>) {
    *CONFIG.lock().unwrap_or_else(PoisonError::into_inner) = config;
}

fn config() -> Config {
    CONFIG
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .unwrap_or_default()
}

static LED_READS: AtomicU64 = AtomicU64::new(0);

/// How many times a `keyboard` service of this process read the lights
/// (tests: none while nobody shows them or a surface is focused).
pub fn led_reads() -> u64 {
    LED_READS.load(Ordering::Relaxed)
}

/// The lock keys' lights.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Leds {
    pub caps_lock: bool,
    pub num_lock: bool,
}

/// Reads every keyboard's Caps Lock and Num Lock light under
/// `<sys>/class/leds` (`input3::capslock/brightness`): on when any is
/// lit. No such lights (a container, a machine without them): off.
pub fn read_leds(sys: &Path) -> Leds {
    LED_READS.fetch_add(1, Ordering::Relaxed);
    let mut leds = Leds::default();
    let Ok(dir) = std::fs::read_dir(sys.join("class/leds")) else {
        return leds;
    };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let light = if name.ends_with("::capslock") {
            &mut leds.caps_lock
        } else if name.ends_with("::numlock") {
            &mut leds.num_lock
        } else {
            continue;
        };
        if *light {
            continue;
        }
        *light = std::fs::read_to_string(entry.path().join("brightness"))
            .ok()
            .and_then(|b| b.trim().parse::<u32>().ok())
            .is_some_and(|b| b > 0);
    }
    leds
}

/// See the module docs.
#[service(name = "keyboard")]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Keyboard {
    /// Caps Lock is on: `if keyboard.caps_lock { text "Caps Lock is on" }`.
    /// False when nothing reports it.
    #[store(stream)]
    pub caps_lock: bool,
    /// Num Lock is on.
    #[store(stream)]
    pub num_lock: bool,
    /// The active layout's name as the keymap names it (`English (US)`),
    /// null when unknown: `text keyboard.layout ?? ""`.
    #[store(stream)]
    pub layout: Option<String>,
}

/// What the service shows: a focused surface's report wins, its layout
/// falling back to the compositor's; without one, the lights and the
/// compositor's layout.
pub fn merge(focused: Option<&Focused>, leds: Leds, compositor: Option<&str>) -> Keyboard {
    let compositor = compositor.map(str::to_string);
    match focused {
        Some(f) => Keyboard {
            caps_lock: f.caps_lock,
            num_lock: f.num_lock,
            layout: f.layout.clone().or(compositor),
        },
        None => Keyboard {
            caps_lock: leds.caps_lock,
            num_lock: leds.num_lock,
            layout: compositor,
        },
    }
}

/// The lights are read: a visible reader shows a lock key and no focused
/// surface reports them.
fn polls(cx: &Cx<Keyboard>, focused: &Option<Focused>) -> bool {
    focused.is_none() && cx.visible() && (cx.watched("caps_lock") || cx.watched("num_lock"))
}

/// The compositor hub's subscription, while `layout` is watched.
fn sync_layout(cx: &Cx<Keyboard>, sub: &mut Option<WmSubscription>) {
    if !(cx.visible() && cx.watched("layout")) {
        // Its last layout is kept; a new subscription's first batch
        // brings the current one.
        *sub = None;
    } else if sub.is_none() {
        *sub = crate::wm::hub().map(|h| h.subscribe());
    }
}

impl Keyboard {
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let config = config();
        let mut focus = FOCUSED.subscribe();
        let mut focused = focus.borrow_and_update().clone();
        let mut leds = Leds::default();
        let mut compositor: Option<String> = None;
        let mut sub: Option<WmSubscription> = None;
        let mut tick = tokio::time::interval(config.led_period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut polling = polls(&cx, &focused);
        if polling {
            leds = read_leds(&config.sys);
        }
        tick.reset();
        sync_layout(&cx, &mut sub);
        if !cx.update(|s| *s = merge(focused.as_ref(), leds, compositor.as_deref())) {
            return Ok(());
        }
        cx.ready();
        loop {
            tokio::select! {
                r = focus.changed() => {
                    if r.is_err() {
                        // The sender is a static: never.
                        return Ok(());
                    }
                    focused = focus.borrow_and_update().clone();
                }
                _ = tick.tick(), if polling => {
                    leds = read_leds(&config.sys);
                }
                b = async {
                    match sub.as_mut() {
                        Some(s) => s.recv().await,
                        None => std::future::pending().await,
                    }
                } => match b {
                    Some(batch) => {
                        for change in batch {
                            if let WmChange::KeyboardLayout(l) = change {
                                compositor = l;
                            }
                        }
                    }
                    // The hub is gone (the services shutting down).
                    None => sub = None,
                },
                m = cx.recv() => match m {
                    None => return Ok(()),
                    Some(Msg::Visible(_) | Msg::Watch { .. }) => sync_layout(&cx, &mut sub),
                    Some(_) => continue,
                },
            }
            let now = polls(&cx, &focused);
            if now && !polling {
                // Shown again, or the focus left: the lights now, then
                // each period.
                leds = read_leds(&config.sys);
                tick.reset();
            }
            polling = now;
            if !cx.update(|s| *s = merge(focused.as_ref(), leds, compositor.as_deref())) {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sysfs with lights: `(name, brightness)`.
    fn sysfs(lights: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, b) in lights {
            let d = dir.path().join("class/leds").join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("brightness"), b).unwrap();
        }
        dir
    }

    #[test]
    fn the_lights_say_which_lock_keys_are_on() {
        let on = sysfs(&[
            ("input3::capslock", "1\n"),
            ("input3::numlock", "0\n"),
            ("input3::scrolllock", "1\n"),
            ("tpacpi::kbd_backlight", "2\n"),
        ]);
        assert_eq!(
            read_leds(on.path()),
            Leds {
                caps_lock: true,
                num_lock: false
            }
        );
        // A second keyboard off, one on: on.
        let two = sysfs(&[
            ("input3::capslock", "0\n"),
            ("input7::capslock", "1\n"),
            ("input7::numlock", "1\n"),
        ]);
        assert_eq!(
            read_leds(two.path()),
            Leds {
                caps_lock: true,
                num_lock: true
            }
        );
        // No lights at all (a container), or one that does not read: off.
        let none = tempfile::tempdir().unwrap();
        assert_eq!(read_leds(none.path()), Leds::default());
        let odd = sysfs(&[("input3::capslock", "on")]);
        assert_eq!(read_leds(odd.path()), Leds::default());
    }

    #[test]
    fn a_focused_report_wins_and_the_compositor_names_the_rest() {
        let leds = Leds {
            caps_lock: false,
            num_lock: true,
        };
        let f = Focused {
            caps_lock: true,
            num_lock: false,
            layout: Some("Strand One".into()),
        };
        assert_eq!(
            merge(Some(&f), leds, Some("German")),
            Keyboard {
                caps_lock: true,
                num_lock: false,
                layout: Some("Strand One".into()),
            }
        );
        // A keymap that names no layout: the compositor's.
        let unnamed = Focused {
            layout: None,
            ..f.clone()
        };
        assert_eq!(
            merge(Some(&unnamed), leds, Some("German"))
                .layout
                .as_deref(),
            Some("German")
        );
        // Unfocused: the lights and the compositor.
        assert_eq!(
            merge(None, leds, Some("German")),
            Keyboard {
                caps_lock: false,
                num_lock: true,
                layout: Some("German".into()),
            }
        );
        assert_eq!(merge(None, Leds::default(), None), Keyboard::default());
    }
}
