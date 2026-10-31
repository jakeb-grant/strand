//! `keyboard`: the lock keys (`caps_lock`, `num_lock`) and the active
//! layout's name (`layout`), gap 31 (decisions.md m6-service-gaps); and
//! (services-round) the layouts it switches between (`layouts`), the
//! modifiers held while a Strand surface has the keys (`alt`, `ctrl`,
//! `shift`, `logo`), and `next_layout()` and `set_layout(index)` through
//! the compositor. Its fields are streams: nothing runs for one nobody
//! shows.
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
//!    sway, niri), subscribed only while `layout` is watched; likewise
//!    the layouts ([`WmChange::KeyboardLayouts`]) while `layouts` is.
//!    Hyprland names only the active layout, so the rest are named here
//!    from its main keyboard's xkb rule names
//!    ([`WmChange::KeyboardRules`], compiled with libxkbcommon), only
//!    while `layouts` is watched and once per change of the names: a
//!    bar that shows only `wm` or `keyboard.layout` compiles no keymap.
//!
//! 4. The layout's short code (`layout_code`), from whichever name the
//!    above gave: no compositor says it, so it is looked up by name in
//!    xkeyboard-config's lists ([`code_in`], `<xkb>/rules/evdev.xml`, then
//!    `evdev.extras.xml` for a name that one lacks, as every compositor's
//!    own names come from them), read only while `layout_code` is watched
//!    and once per name. The first lookup of a name blocks the services
//!    runtime for about 2 ms at worst (both lists, measured in the CI
//!    image; decisions.md service-fields), so it stays on it.
//!
//! The modifiers are only a focused surface's: no compositor tells
//! another client what is held, so they are false otherwise. The actions
//! go to the compositor's adapter (Hyprland's `switchxkblayout`, sway's
//! `xkb_switch_layout`, niri's `SwitchLayout`), each on a subscription of
//! its own held until it is answered; the standard protocols have none.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::watch;

use crate::wm::{LayoutSwitch, WmAction, WmChange, WmError, WmSubscription};
use crate::{Call, Cx, Msg, ServiceError, Store, service};

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
    /// (services-round) The keymap's names for every layout, by index.
    pub layouts: Vec<String>,
    /// (services-round) Alt is held.
    pub alt: bool,
    /// Control is held.
    pub ctrl: bool,
    /// Shift is held.
    pub shift: bool,
    /// The logo (Super) key is held.
    pub logo: bool,
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
    /// xkeyboard-config's directory, holding `rules/evdev.xml` and
    /// `rules/evdev.extras.xml`.
    pub xkb: PathBuf,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            sys: PathBuf::from("/sys"),
            led_period: LED_PERIOD,
            xkb: std::env::var_os("XKB_CONFIG_ROOT")
                .map_or_else(|| PathBuf::from("/usr/share/X11/xkb"), PathBuf::from),
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

static LIST_READS: AtomicU64 = AtomicU64::new(0);

/// How many times a `keyboard` service of this process read one of
/// xkeyboard-config's lists (tests: none while nobody shows `layout_code`,
/// and none for a name already looked up).
pub fn list_reads() -> u64 {
    LIST_READS.load(Ordering::Relaxed)
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

/// (services-round) `keyboard`'s actions.
#[derive(Call, Debug)]
pub enum KeyboardAction {
    /// `keyboard.next_layout()`.
    NextLayout,
    /// `keyboard.set_layout(index)`: an index into `layouts`.
    SetLayout { index: i64 },
}

/// See the module docs.
#[service(name = "keyboard", action = KeyboardAction)]
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
    /// The active layout's short code as xkeyboard-config lists it (`us`,
    /// `de`; a variant's layout, so `English (Dvorak)` is `us`), for a bar
    /// with room for two letters: `text keyboard.layout_code ?? ""`. Null
    /// when unknown, or for a layout xkeyboard-config does not list.
    #[store(stream)]
    pub layout_code: Option<String>,
    /// The layouts it switches between, by index, as the keymap names them
    /// (`["English (US)", "German"]`; empty when unknown):
    /// `keyboard.set_layout(i)` takes an index into it.
    #[store(stream)]
    pub layouts: Vec<String>,
    /// Alt is held, while a Strand surface has keyboard focus (false
    /// otherwise: no compositor tells): an alt-tab switcher commits when
    /// it turns false.
    #[store(stream)]
    pub alt: bool,
    /// Control is held, while a Strand surface has keyboard focus.
    #[store(stream)]
    pub ctrl: bool,
    /// Shift is held, while a Strand surface has keyboard focus.
    #[store(stream)]
    pub shift: bool,
    /// The logo (Super) key is held, while a Strand surface has keyboard
    /// focus.
    #[store(stream)]
    pub logo: bool,
}

/// The short code of the layout named `name` in one of xkeyboard-config's
/// lists, `xml` (`rules/evdev.xml`, `rules/evdev.extras.xml`): a `<layout>` or one of its `<variant>`s
/// whose `<description>` is `name` gives the layout's `<name>`, the name
/// the same file gives a compositor's keymap. `None` when none does, or
/// the text is no XML.
pub fn code_in(xml: &str, name: &str) -> Option<String> {
    // The list names its DTD; nothing of it is read.
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..roxmltree::ParsingOptions::default()
    };
    let doc = roxmltree::Document::parse_with_options(xml, options).ok()?;
    let child_text = |n: roxmltree::Node<'_, '_>, tag: &str| {
        n.children()
            .find(|c| c.has_tag_name("configItem"))?
            .children()
            .find(|c| c.has_tag_name(tag))?
            .text()
            .map(str::to_owned)
    };
    doc.descendants()
        .filter(|n| n.has_tag_name("layout"))
        .find(|layout| {
            child_text(*layout, "description").as_deref() == Some(name)
                || layout
                    .descendants()
                    .filter(|n| n.has_tag_name("variant"))
                    .any(|v| child_text(v, "description").as_deref() == Some(name))
        })
        .and_then(|layout| child_text(layout, "name"))
}

/// Layout names to short codes, looked up once per name: the list is a
/// quarter of a megabyte, and a shell shows two or three layouts.
#[derive(Default)]
struct Codes(std::collections::HashMap<String, Option<String>>);

impl Codes {
    fn of(&mut self, xkb: &Path, name: &str) -> Option<String> {
        if let Some(known) = self.0.get(name) {
            return known.clone();
        }
        let code = ["evdev.xml", "evdev.extras.xml"].iter().find_map(|file| {
            LIST_READS.fetch_add(1, Ordering::Relaxed);
            let xml = std::fs::read_to_string(xkb.join("rules").join(file)).ok()?;
            code_in(&xml, name)
        });
        self.0.insert(name.to_owned(), code.clone());
        code
    }
}

/// What the compositor says of the keyboard (its hub's changes).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Compositor {
    /// The active layout's name.
    pub layout: Option<String>,
    /// The layouts, by index.
    pub layouts: Vec<String>,
    /// (services-round) Hyprland's rule names, which name its layouts.
    pub rules: Option<crate::wm::XkbNames>,
    /// (services-round) The layouts named from `rules`, and the names
    /// they were named from (named again only when those change).
    pub named: Option<(crate::wm::XkbNames, Vec<String>)>,
}

impl Compositor {
    /// (services-round) Names the layouts from `rules` when `wanted` (a
    /// reader shows `layouts`) and they changed since: Hyprland's.
    pub fn name_layouts(&mut self, wanted: bool) {
        let Some(rules) = self.rules.as_ref().filter(|_| wanted) else {
            return;
        };
        if self.named.as_ref().is_some_and(|(r, _)| r == rules) {
            return;
        }
        self.named = Some((rules.clone(), rules.layout_names()));
    }

    /// The layouts, by index: the compositor's list, else those named
    /// from its rule names (while they are current).
    pub fn all_layouts(&self) -> Vec<String> {
        if !self.layouts.is_empty() {
            return self.layouts.clone();
        }
        match (&self.rules, &self.named) {
            (Some(r), Some((n, names))) if r == n => names.clone(),
            _ => Vec::new(),
        }
    }
}

/// What the service shows: a focused surface's report wins, its layout
/// falling back to the compositor's; without one, the lights and the
/// compositor's layout.
pub fn merge(focused: Option<&Focused>, leds: Leds, compositor: &Compositor) -> Keyboard {
    let layouts = |own: &[String]| {
        if own.iter().any(|n| !n.is_empty()) {
            own.to_vec()
        } else {
            compositor.all_layouts()
        }
    };
    match focused {
        Some(f) => Keyboard {
            caps_lock: f.caps_lock,
            num_lock: f.num_lock,
            layout: f.layout.clone().or_else(|| compositor.layout.clone()),
            // Looked up after: only while watched (`shown_code`).
            layout_code: None,
            layouts: layouts(&f.layouts),
            alt: f.alt,
            ctrl: f.ctrl,
            shift: f.shift,
            logo: f.logo,
        },
        None => Keyboard {
            caps_lock: leds.caps_lock,
            num_lock: leds.num_lock,
            layout: compositor.layout.clone(),
            layouts: compositor.all_layouts(),
            ..Keyboard::default()
        },
    }
}

/// The lights are read: a visible reader shows a lock key and no focused
/// surface reports them.
fn polls(cx: &Cx<Keyboard>, focused: &Option<Focused>) -> bool {
    focused.is_none() && cx.visible() && (cx.watched("caps_lock") || cx.watched("num_lock"))
}

/// The compositor hub's subscription, while `layout`, `layout_code` or
/// `layouts` is watched.
fn sync_layout(cx: &Cx<Keyboard>, sub: &mut Option<WmSubscription>) {
    let wanted = ["layout", "layout_code", "layouts"]
        .iter()
        .any(|f| cx.watched(f));
    if !(cx.visible() && wanted) {
        // Its last layout is kept; a new subscription's first batch
        // brings the current one.
        *sub = None;
    } else if sub.is_none() {
        *sub = crate::wm::hub().map(|h| h.subscribe());
    }
}

/// The short code of the layout now shown, only while `layout_code` is
/// watched: a bar that shows only `layout` reads no list.
fn shown_code(
    cx: &Cx<Keyboard>,
    focused: &Option<Focused>,
    compositor: &Compositor,
    codes: &mut Codes,
    xkb: &Path,
) -> Option<String> {
    if !cx.watched("layout_code") {
        return None;
    }
    let name = focused
        .as_ref()
        .and_then(|f| f.layout.as_deref())
        .or(compositor.layout.as_deref())?;
    codes.of(xkb, name)
}

/// How long an action waits for the compositor's adapter to come up on
/// a subscription made for it.
const ACTION_WAIT: Duration = Duration::from_secs(3);

/// (services-round) Runs a layout switch on a subscription of its own
/// (which starts the compositor service when nothing else runs it): an
/// adapter still connecting answers `NotConnected`, so it is asked again
/// after each batch until [`ACTION_WAIT`]. Failures are logged.
async fn switch_layout(to: LayoutSwitch) {
    let Some(hub) = crate::wm::hub() else {
        return;
    };
    let mut sub = hub.subscribe();
    let action = WmAction::SwitchLayout(to);
    let deadline = tokio::time::Instant::now() + ACTION_WAIT;
    loop {
        match sub.request(action.clone()).await {
            Ok(()) => return,
            Err(WmError::NotConnected) if tokio::time::Instant::now() < deadline => {
                match tokio::time::timeout_at(deadline, sub.recv()).await {
                    Ok(Some(_)) => {}
                    // Ended, or never connected in time.
                    Ok(None) | Err(_) => {
                        log::warn!("keyboard {action:?}: the compositor is not connected");
                        return;
                    }
                }
            }
            Err(e) => {
                log::warn!("keyboard {action:?}: {e}");
                return;
            }
        }
    }
}

impl Keyboard {
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let config = config();
        let mut focus = FOCUSED.subscribe();
        let mut focused = focus.borrow_and_update().clone();
        let mut leds = Leds::default();
        let mut compositor = Compositor::default();
        let mut codes = Codes::default();
        let mut calls = tokio::task::JoinSet::new();
        let mut sub: Option<WmSubscription> = None;
        let mut tick = tokio::time::interval(config.led_period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut polling = polls(&cx, &focused);
        if polling {
            leds = read_leds(&config.sys);
        }
        tick.reset();
        sync_layout(&cx, &mut sub);
        let code = shown_code(&cx, &focused, &compositor, &mut codes, &config.xkb);
        if !cx.update(|s| {
            *s = Keyboard {
                layout_code: code,
                ..merge(focused.as_ref(), leds, &compositor)
            };
        }) {
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
                            match change {
                                WmChange::KeyboardLayout(l) => compositor.layout = l,
                                WmChange::KeyboardLayouts(l) => compositor.layouts = l,
                                WmChange::KeyboardRules(r) => compositor.rules = r,
                                _ => {}
                            }
                        }
                        compositor.name_layouts(cx.watched("layouts"));
                    }
                    // The hub is gone (the services shutting down).
                    None => sub = None,
                },
                m = cx.recv() => match m {
                    None => return Ok(()),
                    Some(Msg::Visible(_) | Msg::Watch { .. }) => {
                        sync_layout(&cx, &mut sub);
                        compositor.name_layouts(cx.watched("layouts"));
                    }
                    Some(Msg::Action(a)) => {
                        match a {
                            KeyboardAction::NextLayout => {
                                calls.spawn(switch_layout(LayoutSwitch::Next));
                            }
                            KeyboardAction::SetLayout { index } => match u32::try_from(index) {
                                Ok(i) => {
                                    calls.spawn(switch_layout(LayoutSwitch::Index(i)));
                                }
                                Err(_) => log::warn!("keyboard.set_layout({index}): no such layout"),
                            },
                        }
                        continue;
                    }
                    Some(_) => continue,
                },
                Some(_) = calls.join_next(), if !calls.is_empty() => continue,
            }
            let now = polls(&cx, &focused);
            if now && !polling {
                // Shown again, or the focus left: the lights now, then
                // each period.
                leds = read_leds(&config.sys);
                tick.reset();
            }
            polling = now;
            let code = shown_code(&cx, &focused, &compositor, &mut codes, &config.xkb);
            if !cx.update(|s| {
                *s = Keyboard {
                    layout_code: code,
                    ..merge(focused.as_ref(), leds, &compositor)
                };
            }) {
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

    /// (bead #20) A layout's name to its code: its own description or a
    /// variant's, the variant giving its layout's code; an unknown name
    /// and text that is no XML give none. An entity in a description is
    /// read, as a keymap names it.
    #[test]
    fn a_layouts_code_comes_from_its_name() {
        let xml = r#"<?xml version="1.0"?>
            <!DOCTYPE xkbConfigRegistry SYSTEM "xkb.dtd">
            <xkbConfigRegistry><layoutList>
              <layout><configItem><name>us</name>
                <shortDescription>en</shortDescription>
                <description>English (US)</description></configItem>
                <variantList><variant><configItem><name>dvorak</name>
                  <description>English (Dvorak)</description>
                </configItem></variant></variantList>
              </layout>
              <layout><configItem><name>de</name>
                <description>German</description></configItem></layout>
              <layout><configItem><name>xy</name>
                <description>Bits &amp; Bobs</description></configItem></layout>
            </layoutList></xkbConfigRegistry>"#;
        let code = |name| code_in(xml, name);
        assert_eq!(code("English (US)").as_deref(), Some("us"));
        assert_eq!(code("English (Dvorak)").as_deref(), Some("us"));
        assert_eq!(code("German").as_deref(), Some("de"));
        assert_eq!(code("Bits & Bobs").as_deref(), Some("xy"));
        assert_eq!(code("Klingon"), None);
        assert_eq!(code_in("not xml", "German"), None);
    }

    /// A name `evdev.xml` lacks is looked up in `evdev.extras.xml`, and the
    /// first list wins for a name in both. A name in neither, and a missing
    /// list, give none.
    #[test]
    fn a_name_the_list_lacks_comes_from_the_extras() {
        let list = |name: &str, code: &str| {
            format!(
                "<xkbConfigRegistry><layoutList><layout><configItem><name>{code}</name>\
                 <description>{name}</description></configItem></layout>\
                 </layoutList></xkbConfigRegistry>"
            )
        };
        let dir = tempfile::tempdir().unwrap();
        let rules = dir.path().join("rules");
        std::fs::create_dir_all(&rules).unwrap();
        let mut codes = Codes::default();
        assert_eq!(codes.of(dir.path(), "German"), None, "no lists");
        std::fs::write(rules.join("evdev.xml"), list("German", "de")).unwrap();
        std::fs::write(rules.join("evdev.extras.xml"), list("Avestan", "ave")).unwrap();
        let mut codes = Codes::default();
        assert_eq!(codes.of(dir.path(), "Avestan").as_deref(), Some("ave"));
        assert_eq!(codes.of(dir.path(), "German").as_deref(), Some("de"));
        assert_eq!(codes.of(dir.path(), "Klingon"), None);
        std::fs::write(rules.join("evdev.extras.xml"), list("German", "xx")).unwrap();
        let mut codes = Codes::default();
        assert_eq!(codes.of(dir.path(), "German").as_deref(), Some("de"));
    }

    /// The real lists. Sway depends on xkeyboard-config, so where sway must
    /// be there (`STRAND_REQUIRE_SWAY`: CI and the container) a missing list
    /// fails; no variable of its own is needed for a file sway brings.
    #[test]
    fn the_installed_list_gives_codes() {
        let dir = Config::default().xkb;
        let Ok(xml) = std::fs::read_to_string(dir.join("rules/evdev.xml")) else {
            assert!(
                std::env::var_os("STRAND_REQUIRE_SWAY").is_none(),
                "no xkeyboard-config in {} but STRAND_REQUIRE_SWAY is set",
                dir.display()
            );
            eprintln!("skipped: no xkeyboard-config");
            return;
        };
        let mut codes = Codes::default();
        assert_eq!(codes.of(&dir, "German (Bone)").as_deref(), Some("de"));
        assert_eq!(code_in(&xml, "English (US)").as_deref(), Some("us"));
        assert_eq!(code_in(&xml, "German").as_deref(), Some("de"));
        assert_eq!(code_in(&xml, "English (Dvorak)").as_deref(), Some("us"));
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

    /// (services-round) Hyprland's layouts are named from its rule names
    /// only while `layouts` is shown, once per change of the names, and
    /// only while they are the current ones.
    #[test]
    fn rule_names_are_named_only_while_layouts_are_read() {
        let rules = |layout: &str| crate::wm::XkbNames {
            layout: layout.into(),
            ..Default::default()
        };
        let mut c = Compositor {
            rules: Some(rules("us,de")),
            ..Compositor::default()
        };
        c.name_layouts(false);
        assert_eq!(c.named, None, "not read: no keymap compiled");
        assert!(c.all_layouts().is_empty());
        c.name_layouts(true);
        assert_eq!(c.all_layouts(), ["English (US)", "German"]);
        // New names, not read: the old names are not shown for them.
        c.rules = Some(rules("de"));
        c.name_layouts(false);
        assert!(c.all_layouts().is_empty());
        c.name_layouts(true);
        assert_eq!(c.all_layouts(), ["German"]);
        // A compositor's own list wins.
        c.layouts = vec!["Strand".into()];
        assert_eq!(c.all_layouts(), ["Strand"]);
    }

    #[test]
    fn a_focused_report_wins_and_the_compositor_names_the_rest() {
        let leds = Leds {
            caps_lock: false,
            num_lock: true,
        };
        let compositor = Compositor {
            layout: Some("German".into()),
            layouts: vec!["English (US)".into(), "German".into()],
            ..Compositor::default()
        };
        let f = Focused {
            caps_lock: true,
            num_lock: false,
            layout: Some("Strand One".into()),
            layouts: vec!["Strand One".into(), "Strand Two".into()],
            alt: true,
            ..Focused::default()
        };
        assert_eq!(
            merge(Some(&f), leds, &compositor),
            Keyboard {
                caps_lock: true,
                num_lock: false,
                layout: Some("Strand One".into()),
                layouts: vec!["Strand One".into(), "Strand Two".into()],
                alt: true,
                ..Keyboard::default()
            }
        );
        // A keymap that names no layout: the compositor's.
        let unnamed = Focused {
            layout: None,
            layouts: vec![String::new()],
            ..f.clone()
        };
        let k = merge(Some(&unnamed), leds, &compositor);
        assert_eq!(k.layout.as_deref(), Some("German"));
        assert_eq!(k.layouts, compositor.layouts);
        // Unfocused: the lights and the compositor, nothing held.
        assert_eq!(
            merge(None, leds, &compositor),
            Keyboard {
                caps_lock: false,
                num_lock: true,
                layout: Some("German".into()),
                layouts: compositor.layouts.clone(),
                ..Keyboard::default()
            }
        );
        assert_eq!(
            merge(None, Leds::default(), &Compositor::default()),
            Keyboard::default()
        );
    }
}
