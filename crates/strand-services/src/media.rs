//! `media`: the active MPRIS player on the session bus, with our own zbus
//! client.
//!
//! Every `org.mpris.MediaPlayer2.*` name is followed (`NameOwnerChanged`
//! for the namespace, the players' `PropertiesChanged` and `Seeked` at
//! `/org/mpris/MediaPlayer2`). The active player is the one playing that
//! started playing last, else the one that paused last, else the first
//! by name. Its position is asked for (`Position`, which players do not
//! signal) only when its state, track or rate changes or it seeks, and
//! carried forward at its rate from there. A `Metadata` naming another
//! track (by `mpris:trackid`, else `xesam:url`, else `xesam:title` with
//! `xesam:artist`: the first both name) starts the position at 0 at
//! once, so a new title never shows with the old track's time; the
//! position answer then corrects it. New art or a length for the same
//! track carries on. A track change signalled as an invalidated
//! `Metadata` is judged when the re-read lands (a seek or position
//! answer newer than the re-read stands). `elapsed` and `position`
//! are `#[store(stream)]` fields, ticking once a second only while a visible
//! reader shows them and the player plays. Otherwise nothing wakes.
//!
//! Writing `position` (a fraction of the track) seeks the active player
//! (Gaps 13, decisions.md m6-w2-services): `SetPosition` with the track's
//! `mpris:trackid` when it has one, else `Seek` by the offset from where
//! it is now, in a task of its own. The position is fixed at the target
//! at once (so a dragged scrubber does not jump back), and the player's
//! `Seeked` corrects it; a call that fails asks the position again. A
//! player whose `CanSeek` is false, or a track of unknown length, keeps
//! its place.
//!
//! `can_next`, `can_previous` and `can_seek` say what the active player
//! takes (gap 32, decisions.md m6-service-gaps): its `CanGoNext`,
//! `CanGoPrevious` and `CanSeek`, all false while `CanControl` is false;
//! a property the player does not report counts as able. `can_seek` also
//! needs a known length, and is the very test a `position` write makes
//! ([`Player::can_seek`]). MPRIS signals these properties, so they follow
//! the same `PropertiesChanged` as the rest; an invalidated one is read
//! again with the player's other properties. Nothing is polled.
//!
//! (services-round) `players` lists every known player (a
//! `#[store(keyed, stream)]` field, built only while a visible reader
//! reads it), keyed by its bus name, each with the active player's
//! fields and actions plus `active` and `select()`. `select()` makes a
//! player the one `media.*` follows ([`Players::select`]) until it
//! leaves or another player starts playing after it was selected; the
//! usual choice applies again from there. While the list is read, its
//! playing players' times tick once a second as `elapsed` does.
//!
//! (power-media-kbd) `shuffle`, `loop` and `volume` are the player's own
//! MPRIS `Shuffle`, `LoopStatus` and `Volume`, followed by the same
//! `PropertiesChanged` as the rest, and each has its `can_` field: the
//! property is reported and `CanControl` is not false (MPRIS makes
//! `Shuffle` and `LoopStatus` optional, so a property not reported is one
//! the player lacks, unlike `CanGoNext`). Writing one (the active
//! player's, or an item's of `players`) fixes the value at once
//! ([`Players::set`], through [`Players::changed`], so a whole read in
//! flight cannot undo it) and sets the property on the player in a task,
//! bounded by [`dbus::CALL_TIMEOUT`]. One `Set` per player and property is
//! in flight at a time ([`Setting`]): a newer write meanwhile waits,
//! replacing an older waiting one, and goes when the call lands, so a
//! dragged slider lands in order and ends at its last value. A failed
//! `Set` with nothing waiting reads the player again, so its real value
//! comes back. A volume written is held to 0 to 1; one read is shown as
//! the player reports it, never below 0.
//!
//! (player-raise-seek) The root interface (`org.mpris.MediaPlayer2`) is
//! read whole when a player arrives (`Identity`, `CanRaise`,
//! `DesktopEntry`) and again on each of its `PropertiesChanged`, the
//! latest question's answer standing. `raise()` calls its `Raise`, unless
//! `CanRaise` is false. `seek(by)` calls `Seek` with the offset (back when
//! negative: a [`crate::Offset`]), so it needs no track length, only that
//! the player can seek, which `can_seek_by` says (`can_seek` keeps saying
//! a `position` write seeks, which needs the length); the time shows the
//! target at once, as a `position` write's does. `icon` and `app` are found from the player's
//! `DesktopEntry` (else its bus name's app part) as a window's are from
//! its app id ([`crate::apps::AppIndex::window`]): the desktop entries are
//! read on the blocking pool for the names not yet looked up, and for
//! every name again when the apps service's sources change
//! ([`crate::apps::changes`]); only the answers are kept. Until its
//! lookup lands a player shows its name as its icon and no app.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use futures_lite::StreamExt;
use zbus::MatchRule;
use zbus::message::Type as MessageType;
use zbus::zvariant::OwnedValue;

use crate::dbus::{self, Props};
use crate::{Call, Cx, Data, FromData, Msg, ServiceError, Store, art, service};

/// The schema the `media` service serves.
pub const SCHEMA: &str = strand_services_schema::MEDIA;

/// The players' bus name prefix.
pub const PREFIX: &str = "org.mpris.MediaPlayer2.";
/// The players' object path.
pub const PATH: &str = "/org/mpris/MediaPlayer2";
const ROOT_IFACE: &str = "org.mpris.MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";

/// `media`'s actions.
#[derive(Call, Debug)]
pub enum MediaAction {
    // (services-round) An item's actions come first: a name an item's
    // action shares with the service's is matched by its item's record
    // before the service's own, which takes no item.
    /// `p.play_pause()` for `p` in `media.players`.
    #[call(name = "play_pause")]
    PlayPauseOf { item: MediaPlayer },
    /// `p.next()`.
    #[call(name = "next")]
    NextOf { item: MediaPlayer },
    /// `p.previous()`.
    #[call(name = "previous")]
    PreviousOf { item: MediaPlayer },
    /// `p.select()`: `media.*` follows `p`.
    Select { item: MediaPlayer },
    /// (player-raise-seek) `p.raise()`.
    #[call(name = "raise")]
    RaiseOf { item: MediaPlayer },
    /// (player-raise-seek) `p.seek(by)`.
    #[call(name = "seek")]
    SeekOf {
        item: MediaPlayer,
        by: crate::Offset,
    },
    /// `media.play_pause()`.
    PlayPause,
    /// `media.next()`.
    Next,
    /// `media.previous()`.
    Previous,
    /// (player-raise-seek) `media.raise()`.
    Raise,
    /// (player-raise-seek) `media.seek(by)`.
    Seek { by: crate::Offset },
}

/// (power-media-kbd) What a player repeats (`media.loop`): MPRIS
/// `LoopStatus`.
#[derive(crate::Data, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[data(name = "Loop")]
pub enum Loop {
    /// `None`, and what a player without `LoopStatus` reads.
    #[default]
    None,
    /// `Track`.
    Track,
    /// `Playlist`.
    Playlist,
}

impl Loop {
    /// MPRIS's `LoopStatus` as a `Loop`: anything but `Track` and
    /// `Playlist` is `none`.
    pub fn of_mpris(status: &str) -> Loop {
        match status {
            "Track" => Loop::Track,
            "Playlist" => Loop::Playlist,
            _ => Loop::None,
        }
    }

    /// Its MPRIS name.
    pub fn mpris_name(self) -> &'static str {
        match self {
            Loop::None => "None",
            Loop::Track => "Track",
            Loop::Playlist => "Playlist",
        }
    }
}

/// (services-round) One player of `media.players`: the schema's
/// `Player`, keyed by its bus name.
#[derive(crate::Data, Clone, Debug, Default, PartialEq)]
#[data(name = "Player", key = id)]
pub struct MediaPlayer {
    pub id: String,
    pub name: String,
    pub active: bool,
    pub playing: bool,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub art: Option<String>,
    pub position: f64,
    pub elapsed: Duration,
    pub length: Option<Duration>,
    pub can_next: bool,
    pub can_previous: bool,
    pub can_seek: bool,
    pub shuffle: bool,
    pub r#loop: Loop,
    pub volume: f64,
    pub can_shuffle: bool,
    pub can_loop: bool,
    pub can_volume: bool,
    pub can_raise: bool,
    pub can_seek_by: bool,
    pub icon: String,
    pub app: Option<crate::apps::App>,
}

/// See the module docs.
#[service(name = "media", action = MediaAction)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Media {
    /// Something is playing.
    pub playing: bool,
    /// The track's title.
    pub title: Option<String>,
    /// The track's artist.
    pub artist: Option<String>,
    /// The track's album.
    pub album: Option<String>,
    /// The album art: `image media.art { fit: cover }`. Local art (a
    /// `file://` URL or a path) as the player sent it; remote (`https://`,
    /// `http://`) art fetched while a visible reader reads it and shown as
    /// a file once it is in. Null when the player sent none, while remote
    /// art loads, or when it failed, so `media.art ?? "audio-x-generic"`
    /// falls back.
    #[store(stream)]
    pub art: Option<String>,
    /// Fraction of the track played, 0 to 1. Writable: a scrubber's
    /// `slider { value: <-> media.position }` seeks the player (MPRIS
    /// `SetPosition`, or `Seek` when the track has no id). A player that
    /// cannot seek, or a track of unknown length, keeps its place.
    #[store(stream, rw)]
    pub position: f64,
    /// Time played.
    #[store(stream)]
    pub elapsed: Duration,
    /// The track's length, when known.
    pub length: Option<Duration>,
    /// The active player's name, such as `Firefox` or `mpv`; null when no
    /// player runs.
    pub player: Option<String>,
    /// The active player can skip to the next track (MPRIS `CanGoNext`):
    /// `opacity: media.can_next ? 1 : 0.4`. False when no player runs or
    /// it takes no control (`CanControl` false).
    pub can_next: bool,
    /// The active player can go back to the previous track
    /// (`CanGoPrevious`, as `can_next`).
    pub can_previous: bool,
    /// Writing `position` seeks: the active player can seek (`CanSeek`, as
    /// `can_next`) and the track's length is known. A scrubber's flag; a
    /// skip by a time needs only `can_seek_by`.
    pub can_seek: bool,
    /// The active player shuffles: `on click { media.shuffle =
    /// !media.shuffle }`. Writable: MPRIS `Shuffle`. False when no player
    /// runs or it has no shuffle. A write it cannot take (`can_shuffle`
    /// false) leaves the value as it was.
    #[store(rw)]
    pub shuffle: bool,
    /// What the active player repeats: `media.loop = playlist`. Writable:
    /// MPRIS `LoopStatus`. `none` when no player runs or it has no loop
    /// setting. A write it cannot take (`can_loop` false) leaves the value
    /// as it was.
    #[store(rw)]
    pub r#loop: Loop,
    /// The active player's own volume, apart from the system's
    /// (`audio.sink.volume`): `slider { value: <-> media.volume }`. 0 to 1,
    /// as MPRIS `Volume` reports it (above 1 when the player amplifies).
    /// Writable: a write is held to 0 to 1, and one it cannot take
    /// (`can_volume` false) leaves the value as it was. 0 when no player
    /// runs or it has no volume.
    #[store(rw)]
    pub volume: f64,
    /// Writing `shuffle` changes it: the active player has MPRIS `Shuffle`
    /// and takes control (`CanControl` not false). False when no player
    /// runs.
    pub can_shuffle: bool,
    /// Writing `loop` changes it (`LoopStatus`, as `can_shuffle`).
    pub can_loop: bool,
    /// Writing `volume` changes it (`Volume`, as `can_shuffle`).
    pub can_volume: bool,
    /// `media.raise()` brings the active player forward: its MPRIS
    /// `CanRaise` (a player that does not say counts as able). False when
    /// no player runs.
    pub can_raise: bool,
    /// `media.seek(by)` seeks the active player: its MPRIS `CanSeek` (as
    /// `can_next`), whether or not the track's length is known, so a skip
    /// button that works on a stream is `if media.can_seek_by { … }`.
    /// Unlike `can_seek` (writing `position`), it needs no length. False
    /// when no player runs.
    pub can_seek_by: bool,
    /// An icon name for the active player's app. Found as a window's `icon`
    /// is, the app named by the player's MPRIS `DesktopEntry` (else by its
    /// bus name: `vlc` for `org.mpris.MediaPlayer2.vlc.instance42`) in
    /// place of an app id: its desktop entry's icon, else that name
    /// (lowercased when only that is in the icon theme), else the name as
    /// it is, which the theme lacks. Null when no player runs.
    /// Never null while one runs, so a fallback chooses: `image media.icon`
    /// keeps the theme's icon for an app with no entry and shows nothing
    /// for one neither knows; `image media.app?.icon ?? "audio-x-generic"`
    /// always shows one, the generic one whenever no entry matches, even
    /// where the theme has the app's name.
    pub icon: Option<String>,
    /// The installed app the active player belongs to, found as `icon`'s
    /// entry is: `media.app?.name`. Null when no player runs or no entry
    /// matches.
    pub app: Option<crate::apps::App>,
    /// Every player running, keyed by its bus name (`for p in
    /// media.players key p.id { … }`): a switcher, a card per player. The
    /// one `media.*` follows is `active`; `p.select()` picks another. Read
    /// only while a visible reader reads it; its playing players' times
    /// tick once a second then.
    #[store(keyed, stream)]
    pub players: Vec<MediaPlayer>,
}

/// The playback rates carried forward: MPRIS players report a rate
/// between their `MinimumRate` and `MaximumRate`; anything outside this
/// range is a player's mistake, clamped so the arithmetic on it cannot
/// overflow (decisions.md, wave4-a2).
const RATES: (f64, f64) = (1e-3, 1e3);

/// One player.
#[derive(Debug, Default)]
struct Player {
    /// Its unique bus name (signals come from it).
    owner: String,
    /// (player-raise-seek) Its root interface's properties (`Identity`,
    /// `CanRaise`, `DesktopEntry`), as the latest read answered.
    root: Props,
    /// Which root read is the latest (an older answer is dropped).
    root_asked: u64,
    props: Props,
    /// Its position when last asked, and when.
    at: Option<(Duration, Instant)>,
    /// When it last started playing or paused (the active player's
    /// choice).
    touched: u64,
    /// Read at least once: until then it is not a choice for the active
    /// player.
    known: bool,
    /// Changes signalled while a read is in flight, applied over its
    /// answer (over every answer until the last read in flight is in: an
    /// older read answering last cannot undo a change signalled since).
    pending: Option<Props>,
    /// Property reads in flight.
    reading: u32,
    /// Which position question is the latest (a seek or a newer question
    /// makes an older answer stale).
    asked: u64,
    /// The track its `Metadata` last named (none until a `Metadata` is
    /// known): a `Metadata` naming another one starts the position at 0.
    track: Option<Track>,
}

/// What tells one track from another: `mpris:trackid`, else
/// `xesam:url`, else `xesam:title` with `xesam:artist`.
#[derive(Debug, Clone, Default, PartialEq)]
struct Track {
    id: Option<String>,
    url: Option<String>,
    title: Option<(String, String)>,
}

impl Track {
    fn of(meta: &Props) -> Self {
        let text = |k: &str| dbus::text(meta, k).filter(|s| !s.is_empty());
        let id = meta
            .get("mpris:trackid")
            .and_then(|v| {
                v.downcast_ref::<zbus::zvariant::ObjectPath<'_>>()
                    .ok()
                    .map(|p| p.to_string())
            })
            .or_else(|| text("mpris:trackid"));
        Track {
            id,
            url: text("xesam:url"),
            title: text("xesam:title").map(|t| (t, artist(meta).unwrap_or_default())),
        }
    }

    /// The same track: judged by the first of trackid, URL and title
    /// with artist that both name (a player that adds a trackid to a
    /// track it named by title alone has not changed track); when they
    /// share none, only two that name nothing are the same.
    fn same(&self, other: &Track) -> bool {
        if let (Some(a), Some(b)) = (&self.id, &other.id) {
            return a == b;
        }
        if let (Some(a), Some(b)) = (&self.url, &other.url) {
            return a == b;
        }
        if let (Some(a), Some(b)) = (&self.title, &other.title) {
            return a == b;
        }
        self == other
    }
}

/// `xesam:artist`, a list (as MPRIS says) or one name, joined.
fn artist(meta: &Props) -> Option<String> {
    meta.get("xesam:artist")
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| Vec::<String>::try_from(v).ok())
        .map(|a| a.join(", "))
        .filter(|s| !s.is_empty())
        .or_else(|| dbus::text(meta, "xesam:artist").filter(|s| !s.is_empty()))
}

impl Player {
    /// Its MPRIS `Identity`.
    fn identity(&self) -> Option<String> {
        dbus::text(&self.root, "Identity")
    }

    /// (player-raise-seek) `Raise` does something: its `CanRaise`, a
    /// player that does not say counting as able (as [`Player::can`]).
    fn can_raise(&self) -> bool {
        dbus::boolean(&self.root, "CanRaise") != Some(false)
    }

    /// (player-raise-seek) The app it names, for its icon and desktop
    /// entry: its `DesktopEntry`, else its bus name `name` without the
    /// MPRIS prefix and an `.instance…` part (`vlc` for
    /// `org.mpris.MediaPlayer2.vlc.instance42`).
    fn app_id(&self, name: &str) -> String {
        if let Some(entry) = dbus::text(&self.root, "DesktopEntry")
            .map(|e| e.trim().trim_end_matches(".desktop").to_string())
            .filter(|e| !e.is_empty())
        {
            return entry;
        }
        let rest = name.strip_prefix(PREFIX).unwrap_or(name);
        match rest.rsplit_once('.') {
            Some((head, last)) if last.starts_with("instance") && !head.is_empty() => {
                head.to_string()
            }
            _ => rest.to_string(),
        }
    }

    fn status(&self) -> String {
        dbus::text(&self.props, "PlaybackStatus").unwrap_or_default()
    }

    fn playing(&self) -> bool {
        self.status() == "Playing"
    }

    fn rate(&self) -> f64 {
        dbus::number(&self.props, "Rate")
            .filter(|r| r.is_finite() && *r > 0.0)
            .map_or(1.0, |r| r.clamp(RATES.0, RATES.1))
    }

    /// The player says it can do `what` (`CanGoNext`, `CanGoPrevious`,
    /// `CanSeek`): false when it takes no control at all (`CanControl`
    /// false) or says it cannot; a property it does not report counts as
    /// able, so a button that may work is never dimmed (decisions.md,
    /// m6-service-gaps).
    fn can(&self, what: &str) -> bool {
        dbus::boolean(&self.props, "CanControl") != Some(false)
            && dbus::boolean(&self.props, what) != Some(false)
    }

    /// A `position` write seeks a track of `length`: the player can seek
    /// and the length is known. What `media.can_seek` says and what
    /// [`Players::seek`] does, so the field and the write agree.
    fn can_seek(&self, length: Option<Duration>) -> bool {
        self.can_seek_by() && length.is_some_and(|l| !l.is_zero())
    }

    /// (player-raise-seek) `seek(by)` seeks it: it can seek
    /// ([`Player::can`] of `CanSeek`), whatever the track's length. What
    /// `can_seek_by` says and what [`Players::seek_by`] does.
    fn can_seek_by(&self) -> bool {
        self.can("CanSeek")
    }

    /// (power-media-kbd) Writing `prop` (`Shuffle`, `LoopStatus`,
    /// `Volume`) can change it: it takes control and reports the
    /// property. Unlike [`Player::can`], a property not reported counts
    /// as missing: MPRIS makes `Shuffle` and `LoopStatus` optional, and a
    /// `Volume` that cannot be read cannot be set.
    fn can_set(&self, prop: &str) -> bool {
        dbus::boolean(&self.props, "CanControl") != Some(false) && self.props.contains_key(prop)
    }

    fn metadata(&self) -> Props {
        self.props
            .get("Metadata")
            .and_then(|v| v.try_clone().ok())
            .and_then(|v| Props::try_from(v).ok())
            .unwrap_or_default()
    }

    /// Note the track its `Metadata` names now (nothing when no
    /// `Metadata` is known); when that is another track than the one
    /// noted before, its position starts at 0 at `now`, so the title and
    /// the time change in one update (the position question asked with
    /// the change corrects it), unless `reset` is false (a newer position
    /// fix may already stand). Whether it was another track.
    fn note_track(&mut self, now: Instant, reset: bool) -> bool {
        if !self.props.contains_key("Metadata") {
            return false;
        }
        let track = Track::of(&self.metadata());
        let other = self.track.as_ref().is_some_and(|t| !t.same(&track));
        if other && reset {
            self.at = Some((Duration::ZERO, now));
        }
        self.track = Some(track);
        other
    }

    /// The position now, carried forward while playing.
    fn elapsed(&self, now: Instant, length: Option<Duration>) -> Duration {
        let Some((base, at)) = self.at else {
            return Duration::ZERO;
        };
        let mut e = base;
        if self.playing() {
            let played = now.saturating_duration_since(at).as_secs_f64() * self.rate();
            e = e.saturating_add(Duration::try_from_secs_f64(played).unwrap_or(Duration::MAX));
        }
        match length {
            Some(l) if e > l => l,
            _ => e,
        }
    }
}

/// Microseconds (MPRIS's unit, signed or not) as a duration.
fn micros(v: &OwnedValue) -> Option<Duration> {
    let mut p = Props::new();
    p.insert(String::new(), v.try_clone().ok()?);
    let us = dbus::number(&p, "")?;
    (us >= 0.0).then(|| Duration::from_micros(us as u64))
}

/// Every player, by bus name.
#[derive(Debug, Default)]
struct Players {
    by_name: BTreeMap<String, Player>,
    clock: u64,
    /// (services-round) The player `select()` chose, and the clock then:
    /// it stays the active one while it is listed and no other player
    /// started playing since.
    selected: Option<(String, u64)>,
    /// (player-raise-seek) Each app name's icon and app, as its lookup
    /// in the desktop entries answered ([`Link`]).
    links: HashMap<String, Link>,
}

/// (player-raise-seek) An app name's icon and installed app
/// ([`crate::apps::AppIndex::window`]).
type Link = (String, Option<crate::apps::App>);

/// What a player shows, the active one's fields and an item's alike.
struct Shown {
    playing: bool,
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    art: Option<String>,
    position: f64,
    elapsed: Duration,
    length: Option<Duration>,
    can_next: bool,
    can_previous: bool,
    can_seek: bool,
    shuffle: bool,
    r#loop: Loop,
    volume: f64,
    can_shuffle: bool,
    can_loop: bool,
    can_volume: bool,
    can_raise: bool,
    can_seek_by: bool,
    icon: String,
    app: Option<crate::apps::App>,
}

impl Shown {
    /// `p`, listed as `name`, at `now`; its icon and app from `links`.
    fn of(p: &Player, name: &str, links: &HashMap<String, Link>, now: Instant) -> Shown {
        let meta = p.metadata();
        let length = meta.get("mpris:length").and_then(micros);
        let elapsed = p.elapsed(now, length);
        let text = |k: &str| dbus::text(&meta, k).filter(|s| !s.is_empty());
        // Not looked up yet: the name itself, as a window shows its app id.
        let app_id = p.app_id(name);
        let (icon, app) = links.get(&app_id).cloned().unwrap_or((app_id, None));
        Shown {
            playing: p.playing(),
            title: text("xesam:title"),
            artist: artist(&meta),
            album: text("xesam:album"),
            art: text("mpris:artUrl").filter(|u| local_art(u) || art::is_remote(u)),
            position: match length {
                Some(l) if !l.is_zero() => {
                    (elapsed.as_secs_f64() / l.as_secs_f64()).clamp(0.0, 1.0)
                }
                _ => 0.0,
            },
            elapsed,
            length,
            can_next: p.can("CanGoNext"),
            can_previous: p.can("CanGoPrevious"),
            can_seek: p.can_seek(length),
            shuffle: dbus::boolean(&p.props, "Shuffle") == Some(true),
            r#loop: Loop::of_mpris(&dbus::text(&p.props, "LoopStatus").unwrap_or_default()),
            volume: dbus::number(&p.props, "Volume")
                .filter(|v| v.is_finite())
                .map_or(0.0, |v| v.max(0.0)),
            can_shuffle: p.can_set("Shuffle"),
            can_loop: p.can_set("LoopStatus"),
            can_volume: p.can_set("Volume"),
            can_raise: p.can_raise(),
            can_seek_by: p.can_seek_by(),
            icon,
            app,
        }
    }
}

impl Players {
    fn active(&self) -> Option<&Player> {
        let known = || self.by_name.values().filter(|p| p.known);
        if let Some((name, at)) = &self.selected
            && let Some(p) = self.by_name.get(name).filter(|p| p.known)
            && !known().any(|o| !std::ptr::eq(o, p) && o.playing() && o.touched > *at)
        {
            return Some(p);
        }
        let playing = known().filter(|p| p.playing()).max_by_key(|p| p.touched);
        playing.or_else(|| known().max_by_key(|p| (p.status() == "Paused", p.touched)))
    }

    /// (services-round) `p.select()`: `name` is the active player until
    /// it leaves or another starts playing. Whether it is listed.
    fn select(&mut self, name: &str) -> bool {
        if !self.by_name.get(name).is_some_and(|p| p.known) {
            return false;
        }
        self.selected = Some((name.to_string(), self.clock));
        true
    }

    /// (services-round) Every known player as `media.players` shows it,
    /// by bus name.
    fn items(&self, now: Instant) -> Vec<MediaPlayer> {
        let active = self.active_name();
        self.by_name
            .iter()
            .filter(|(_, p)| p.known)
            .map(|(name, p)| {
                let v = Shown::of(p, name, &self.links, now);
                MediaPlayer {
                    id: name.clone(),
                    name: p
                        .identity()
                        .unwrap_or_else(|| name.trim_start_matches(PREFIX).to_string()),
                    active: active.as_deref() == Some(name.as_str()),
                    playing: v.playing,
                    title: v.title,
                    artist: v.artist,
                    album: v.album,
                    art: v.art,
                    position: v.position,
                    elapsed: v.elapsed,
                    length: v.length,
                    can_next: v.can_next,
                    can_previous: v.can_previous,
                    can_seek: v.can_seek,
                    shuffle: v.shuffle,
                    r#loop: v.r#loop,
                    volume: v.volume,
                    can_shuffle: v.can_shuffle,
                    can_loop: v.can_loop,
                    can_volume: v.can_volume,
                    can_raise: v.can_raise,
                    can_seek_by: v.can_seek_by,
                    icon: v.icon,
                    app: v.app,
                }
            })
            .collect()
    }

    fn active_name(&self) -> Option<String> {
        let a = self.active()?;
        self.by_name
            .iter()
            .find(|(_, p)| std::ptr::eq(*p, a))
            .map(|(n, _)| n.clone())
    }

    fn state(&self, now: Instant) -> Media {
        let Some((name, p)) = self
            .active_name()
            .and_then(|n| self.by_name.get_key_value(&n))
        else {
            return Media::default();
        };
        let v = Shown::of(p, name, &self.links, now);
        Media {
            playing: v.playing,
            title: v.title,
            artist: v.artist,
            album: v.album,
            art: v.art,
            position: v.position,
            elapsed: v.elapsed,
            length: v.length,
            player: p.identity(),
            can_next: v.can_next,
            can_previous: v.can_previous,
            can_seek: v.can_seek,
            shuffle: v.shuffle,
            r#loop: v.r#loop,
            volume: v.volume,
            can_shuffle: v.can_shuffle,
            can_loop: v.can_loop,
            can_volume: v.can_volume,
            can_raise: v.can_raise,
            can_seek_by: v.can_seek_by,
            icon: Some(v.icon),
            app: v.app,
            players: Vec::new(),
        }
    }

    /// (player-raise-seek) `seek(by)` of the player `name`: `Seek` by
    /// `by` seconds (back when negative) from where it is at `now`, the
    /// call to make, its position fixed at the target meanwhile (never
    /// below 0, nor past a known length). Unlike a `position` write it
    /// needs no length. `None` when the player is not listed, cannot seek
    /// ([`Player::can_seek_by`]), or `by` is not a number.
    fn seek_by(&mut self, name: &str, by: f64, now: Instant) -> Option<Seek> {
        let p = self.by_name.get_mut(name).filter(|p| p.known)?;
        if !by.is_finite() || !p.can_seek_by() {
            return None;
        }
        let length = p.metadata().get("mpris:length").and_then(micros);
        let from = p.elapsed(now, length);
        let size = Duration::try_from_secs_f64(by.abs()).unwrap_or(Duration::MAX);
        let target = if by < 0.0 {
            from.saturating_sub(size)
        } else {
            from.saturating_add(size)
        };
        let target = match length {
            Some(l) if target > l => l,
            _ => target,
        };
        p.asked += 1;
        p.at = Some((target, now));
        Some(Seek {
            name: name.to_string(),
            owner: p.owner.clone(),
            // Saturating: a seek past either end is the end (MPRIS).
            call: SeekCall::Seek((by * 1e6).round() as i64),
            asked: p.asked,
        })
    }

    /// (player-raise-seek) `name` while it is listed and `Raise` does
    /// something ([`Player::can_raise`]).
    fn raisable(&self, name: String) -> Option<String> {
        self.by_name
            .get(&name)
            .is_some_and(|p| p.known && p.can_raise())
            .then_some(name)
    }

    /// (player-raise-seek) The app names to look up in the desktop
    /// entries: every listed player's, or (`all` false) only those not
    /// looked up yet. Each once, in order.
    fn to_link(&self, all: bool) -> Vec<String> {
        let mut names: Vec<String> = self
            .by_name
            .iter()
            .filter(|(_, p)| p.known)
            .map(|(name, p)| p.app_id(name))
            .filter(|n| all || !self.links.contains_key(n))
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// (player-raise-seek) A lookup's answers: kept, with the names no
    /// listed player uses any more dropped. Whether a player shows another
    /// icon or app for them (an answer that is the name itself, with no
    /// app, changes nothing: a player shows that until its answer lands).
    fn linked(&mut self, found: Vec<(String, Link)>) -> bool {
        let mut changed = false;
        for (name, link) in found {
            let shown = self.links.get(&name).cloned();
            changed |= shown.unwrap_or_else(|| (name.clone(), None)) != link;
            self.links.insert(name, link);
        }
        let used: std::collections::HashSet<String> = self
            .by_name
            .iter()
            .map(|(name, p)| p.app_id(name))
            .collect();
        self.links.retain(|n, _| used.contains(n));
        changed
    }

    /// (player-raise-seek) Apply a `PropertiesChanged` of `name`'s root
    /// interface: the changes now, and the root read whole again (its
    /// answer then stands over any read asked before).
    fn root_changed(&mut self, name: &str, changed: Props, invalidated: &[String]) -> Option<Ask> {
        let p = self.by_name.get_mut(name)?;
        p.root.extend(changed);
        for i in invalidated {
            p.root.remove(i);
        }
        p.root_asked += 1;
        Some(Ask {
            root: Some(p.root_asked),
            asked: p.asked,
            ..Ask::default()
        })
    }

    /// Seeks the active player to `fraction` of its track at `now`: the
    /// call to make, its position fixed at the target meanwhile. `None`
    /// when no player is active, it cannot seek ([`Player::can_seek`]:
    /// `CanSeek` or `CanControl` false, or its track's length unknown),
    /// or `fraction` is not a number.
    fn seek(&mut self, fraction: f64, now: Instant) -> Option<Seek> {
        let name = self.active_name()?;
        self.seek_player(&name, fraction, now)
    }

    /// (services-round) [`Players::seek`] of the player `name` (an item
    /// of `media.players` written).
    fn seek_player(&mut self, name: &str, fraction: f64, now: Instant) -> Option<Seek> {
        if !fraction.is_finite() {
            return None;
        }
        let name = name.to_string();
        let p = self.by_name.get_mut(&name)?;
        let meta = p.metadata();
        let length = meta.get("mpris:length").and_then(micros);
        if !p.can_seek(length) {
            return None;
        }
        let length = length?;
        let target = length.mul_f64(fraction.clamp(0.0, 1.0));
        let from = p.elapsed(now, Some(length));
        let us = |d: Duration| i64::try_from(d.as_micros()).unwrap_or(i64::MAX);
        // `NoTrack` is no track to set a position in (the MPRIS spec).
        let track = meta
            .get("mpris:trackid")
            .and_then(|v| v.downcast_ref::<zbus::zvariant::ObjectPath>().ok())
            .map(|p| p.to_string())
            .filter(|t| t != "/org/mpris/MediaPlayer2/TrackList/NoTrack");
        let call = match track {
            Some(track) => SeekCall::SetPosition(track, us(target)),
            None => SeekCall::Seek(us(target) - us(from)),
        };
        p.asked += 1;
        p.at = Some((target, now));
        Some(Seek {
            name,
            owner: p.owner.clone(),
            call,
            asked: p.asked,
        })
    }

    /// (services-round) `name`, while it is a known player.
    fn listed(&self, name: String) -> Option<String> {
        self.by_name
            .get(&name)
            .is_some_and(|p| p.known)
            .then_some(name)
    }

    /// (power-media-kbd) A write of `field` (`shuffle`, `loop` or
    /// `volume`) of the player `name` to `value` at `now`: the value is
    /// fixed at once, as the player's own `PropertiesChanged` would fix it
    /// (so a whole read in flight cannot undo it), and the `Set` to make
    /// is returned. A volume is held to 0 to 1. `None`: the player is not
    /// listed, cannot take it ([`Player::can_set`]), or `value` is not one
    /// (a volume that is not a number).
    fn set(&mut self, name: &str, field: &str, value: &Data, now: Instant) -> Option<PropSet> {
        let p = self.by_name.get(name).filter(|p| p.known)?;
        let (prop, v): (&'static str, zbus::zvariant::Value<'static>) = match (field, value) {
            ("shuffle", Data::Bool(on)) => ("Shuffle", (*on).into()),
            ("loop", d @ Data::Enum { .. }) => {
                ("LoopStatus", Loop::from_data(d).ok()?.mpris_name().into())
            }
            ("volume", Data::Float(x)) if x.is_finite() => ("Volume", x.clamp(0.0, 1.0).into()),
            ("volume", Data::Int(x)) => ("Volume", (*x as f64).clamp(0.0, 1.0).into()),
            _ => return None,
        };
        if !p.can_set(prop) {
            return None;
        }
        let owner = p.owner.clone();
        let fixed = OwnedValue::try_from(v).ok()?;
        let value = fixed.try_clone().ok()?;
        self.changed(name, Props::from([(prop.to_string(), fixed)]), &[], now);
        Some(PropSet {
            name: name.to_string(),
            owner,
            prop,
            value,
        })
    }

    /// (power-media-kbd) Read `name` whole again (a `Set` of `prop` it
    /// refused: its real value comes back), while `owner` still owns it:
    /// what to ask. The refused value leaves what reads in flight are
    /// answered over: the reread is asked after the write, so its answer
    /// is the truth for `prop`, and a change signalled or written after
    /// this is held again by [`Players::changed`].
    fn reread(&mut self, name: &str, owner: &str, prop: &str) -> Option<Ask> {
        let p = self.by_name.get_mut(name).filter(|p| p.owner == owner)?;
        p.reading += 1;
        p.pending.get_or_insert_with(Props::new).remove(prop);
        Some(Ask {
            props: true,
            asked: p.asked,
            ..Ask::default()
        })
    }

    fn by_owner(&mut self, owner: &str) -> Option<(&String, &mut Player)> {
        self.by_name.iter_mut().find(|(_, p)| p.owner == owner)
    }

    fn touch(&mut self, name: &str) {
        self.clock += 1;
        let c = self.clock;
        if let Some(p) = self.by_name.get_mut(name) {
            p.touched = c;
        }
    }

    /// Apply a read's answer, landed at `now`; whether anything changed
    /// (an answer for an owner that is gone, or a position overtaken, is
    /// dropped). A whole read naming another track (a track change
    /// signalled as an invalidated `Metadata`) starts its position at 0,
    /// unless a position question or seek came after the read was asked;
    /// the read's own position answer, current, then says where.
    fn answer(&mut self, r: Read, now: Instant) -> bool {
        let Some(p) = self.by_name.get_mut(&r.name) else {
            return false;
        };
        if p.owner != r.owner {
            return false;
        }
        if let Some((asked, root)) = r.root
            && asked == p.root_asked
        {
            match root {
                Ok(root) => p.root = root,
                // A player whose root does not answer: no name, raisable.
                Err(e) => log::debug!("media: {}'s root not read: {e}", r.name),
            }
        }
        let first = !p.known;
        if let Some(props) = r.props {
            match props {
                Ok(props) => p.props = props,
                // A player that does not answer: what its signals said.
                Err(e) => log::debug!("media: {} not read: {e}", r.name),
            }
            p.reading = p.reading.saturating_sub(1);
            let since = if p.reading == 0 {
                p.pending.take()
            } else {
                p.pending.as_ref().map(|since| {
                    since
                        .iter()
                        .filter_map(|(k, v)| Some((k.clone(), v.try_clone().ok()?)))
                        .collect()
                })
            };
            if let Some(since) = since {
                p.props.extend(since);
            }
            // Judged after the changes signalled since are applied: a
            // stale read naming the old track does not count as a change.
            // Another track starts at 0 only while no position question
            // or seek came after this read was asked: a newer fix (a
            // seek, or a later answer landed first) stands, and a later
            // question still in flight corrects the time when it lands.
            let reset = r.asked == p.asked;
            p.note_track(now, reset);
            p.known = true;
        }
        if let Some((asked, pos, at)) = r.position
            && asked == p.asked
        {
            p.at = Some((pos.unwrap_or_default(), at));
        }
        if first && p.known {
            self.touch(&r.name);
        }
        true
    }
}

/// Art `image` can show as sent: a `file://` URL or a path. Remote art
/// is fetched first ([`crate::art`]; decisions.md, m6-services).
fn local_art(url: &str) -> bool {
    url.starts_with("file://") || url.starts_with('/')
}

/// A player read in a task of the body's own (so a player that does not
/// answer holds up nothing but itself): what was asked, answered.
#[derive(Debug)]
struct Read {
    name: String,
    owner: String,
    /// The player's latest position question when this was asked (a newer
    /// one, or a seek, means a newer position fix may stand).
    asked: u64,
    /// (player-raise-seek) The root question's number and its answer.
    root: Option<(u64, zbus::Result<Props>)>,
    props: Option<zbus::Result<Props>>,
    /// The position question's number, its answer and when it came.
    position: Option<(u64, Option<Duration>, Instant)>,
}

/// What to ask a player.
#[derive(Debug, Clone, Copy, Default)]
struct Ask {
    /// (player-raise-seek) Read the root interface whole, as question
    /// this number.
    root: Option<u64>,
    props: bool,
    position: Option<u64>,
    /// The player's latest position question as this is asked.
    asked: u64,
}

/// Ask `name` (owned by `owner`) what `ask` says, each call bounded by
/// [`dbus::CALL_TIMEOUT`] and all at once.
async fn read(conn: zbus::Connection, name: String, owner: String, ask: Ask) -> Read {
    let root = async {
        let asked = ask.root?;
        Some((
            asked,
            dbus::timed(dbus::get_all(&conn, &name, PATH, ROOT_IFACE)).await,
        ))
    };
    let props = async {
        if !ask.props {
            return None;
        }
        Some(dbus::timed(dbus::get_all(&conn, &name, PATH, PLAYER)).await)
    };
    let position = async {
        let asked = ask.position?;
        let pos = dbus::timed(dbus::get(&conn, &name, PATH, PLAYER, "Position"))
            .await
            .ok()
            .and_then(|v| micros(&v));
        Some((asked, pos, Instant::now()))
    };
    let (root, props, position) = tokio::join!(root, props, position);
    Read {
        name,
        owner,
        asked: ask.asked,
        root,
        props,
        position,
    }
}

/// The body's tasks: reads (answers to apply) and actions (nothing).
type Tasks = tokio::task::JoinSet<Option<Read>>;

/// A seek to make ([`Players::seek`]).
#[derive(Debug, PartialEq)]
struct Seek {
    name: String,
    owner: String,
    call: SeekCall,
    /// The position question the seek's fix is (a failed call asks the
    /// position again under it).
    asked: u64,
}

/// MPRIS's two ways to seek.
#[derive(Debug, PartialEq)]
enum SeekCall {
    /// `SetPosition(track, position)`, microseconds.
    SetPosition(String, i64),
    /// `Seek(offset)`, microseconds from where it is.
    Seek(i64),
}

impl Seek {
    /// Makes the call; when it fails, the position is asked again.
    async fn run(self, conn: zbus::Connection) -> Option<Read> {
        let name = self.name.as_str();
        let r = match &self.call {
            SeekCall::SetPosition(track, us) => {
                match zbus::zvariant::ObjectPath::try_from(track.as_str()) {
                    Ok(path) => {
                        dbus::timed(conn.call_method(
                            Some(name),
                            PATH,
                            Some(PLAYER),
                            "SetPosition",
                            &(path, *us),
                        ))
                        .await
                    }
                    Err(e) => Err(e.into()),
                }
            }
            SeekCall::Seek(offset) => {
                dbus::timed(conn.call_method(Some(name), PATH, Some(PLAYER), "Seek", &(*offset,)))
                    .await
            }
        };
        let e = r.err()?;
        log::warn!("media: seeking {name}: {e}");
        let ask = Ask {
            position: Some(self.asked),
            asked: self.asked,
            ..Ask::default()
        };
        Some(read(conn, self.name, self.owner, ask).await)
    }
}

/// (power-media-kbd) A property to set on a player ([`Players::set`]).
#[derive(Debug)]
struct PropSet {
    /// The player's bus name.
    name: String,
    /// Its owner when it was written.
    owner: String,
    /// `Shuffle`, `LoopStatus` or `Volume`.
    prop: &'static str,
    value: OwnedValue,
}

/// A [`PropSet`] landed: which, and whether the player took it.
#[derive(Debug)]
struct SetDone {
    name: String,
    owner: String,
    prop: &'static str,
    ok: bool,
}

impl PropSet {
    /// `Properties.Set` on the player, bounded by [`dbus::CALL_TIMEOUT`].
    async fn run(self, conn: zbus::Connection) -> SetDone {
        let r = match self.value.try_clone() {
            Ok(v) => {
                dbus::timed(dbus::set(
                    &conn,
                    &self.name,
                    PATH,
                    PLAYER,
                    self.prop,
                    v.into(),
                ))
                .await
            }
            Err(e) => Err(e.into()),
        };
        if let Err(e) = &r {
            log::warn!("media: setting {} on {}: {e}", self.prop, self.name);
        }
        SetDone {
            name: self.name,
            owner: self.owner,
            prop: self.prop,
            ok: r.is_ok(),
        }
    }
}

/// (power-media-kbd) The `Set`s in flight, at most one per player and
/// property, each with the newest value written since waiting behind it.
#[derive(Debug, Default)]
struct Setting {
    busy: HashMap<(String, &'static str), Option<PropSet>>,
}

impl Setting {
    /// `set` to send now; `None` while one of its player and property is
    /// in flight, `set` then waiting (in place of an older waiting one).
    fn want(&mut self, set: PropSet) -> Option<PropSet> {
        match self.busy.entry((set.name.clone(), set.prop)) {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                e.insert(Some(set));
                None
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(None);
                Some(set)
            }
        }
    }

    /// The `Set` of `prop` on `name` landed: the one waiting, to send next
    /// (in flight from now), or none (nothing in flight any more).
    fn landed(&mut self, name: &str, prop: &'static str) -> Option<PropSet> {
        let key = (name.to_string(), prop);
        let next = self.busy.get_mut(&key)?.take();
        if next.is_none() {
            self.busy.remove(&key);
        }
        next
    }
}

/// A remote art fetch's end: its URL, and its file (`None` when it came
/// in while no media body ran: only the shown file is kept then), or why
/// not.
type Fetched = (String, Result<Option<art::Art>, String>);

/// What a media body hands the next on this services thread: its art
/// cache, and the fetches it started, which run on (a blocking fetch is
/// not cancelled) and report here.
struct Left {
    cache: art::Cache,
    tx: tokio::sync::mpsc::UnboundedSender<Fetched>,
    rx: tokio::sync::mpsc::UnboundedReceiver<Fetched>,
    /// Whether a body runs (its fetches' files are kept).
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Default for Left {
    fn default() -> Left {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Left {
            cache: art::Cache::default(),
            tx,
            rx,
            running: Default::default(),
        }
    }
}

thread_local! {
    /// What the last media body of this services thread left
    /// (services-r2-4). Per thread, so per services runtime: the bodies
    /// of another runtime in the process (another test's) never take or
    /// replace it (services-r3-2).
    static LEFT: std::cell::RefCell<Option<Left>> = const { std::cell::RefCell::new(None) };
}

/// A media body's art cache and fetches. Fetched art's file lives only
/// while the cache keeps it, and the store's `art` holds just its path,
/// which it still publishes after the body ends (a restart after an
/// error, or the stop when nothing reads `media`) until the next body
/// publishes: so the body takes what the last one left, and leaves its
/// own (the file shown only) however it ends. A newly built
/// `image media.art` meanwhile reads a file that is there, and the next
/// body shows it without fetching it again. The fetches still running
/// are left with it, counted as running, and their ends come to the next
/// body: [`art::IN_FLIGHT`] holds across bodies, a URL still being
/// fetched is not fetched again beside itself, and a failure is
/// remembered (services-r3-1).
struct Kept(Left);

impl Kept {
    fn take() -> Kept {
        let left = LEFT
            .try_with(|l| l.borrow_mut().take())
            .ok()
            .flatten()
            .unwrap_or_default();
        left.running
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Kept(left)
    }

    /// Fetch `url` with `fetch` on the blocking pool; its end comes to
    /// [`Kept::ended`] (this body's or the next's).
    fn fetch(
        &self,
        url: String,
        fetch: impl FnOnce(&str) -> Result<art::Art, String> + Send + 'static,
    ) {
        let tx = self.0.tx.clone();
        let running = self.0.running.clone();
        tokio::task::spawn_blocking(move || {
            // A panic in a decoder of the network's bytes is a failed
            // fetch, never a URL counted as fetching for good.
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fetch(&url)))
                .unwrap_or_else(|_| Err("the fetch panicked".into()));
            // Came in while no body runs: the file goes (only the shown
            // one is kept meanwhile), the URL stops counting as fetched.
            let r = r.map(|a| {
                running
                    .load(std::sync::atomic::Ordering::SeqCst)
                    .then_some(a)
            });
            let _ = tx.send((url, r));
        });
    }

    /// The next fetch to end.
    async fn ended(&mut self) -> Option<Fetched> {
        self.0.rx.recv().await
    }

    /// Apply a fetch's end at `now`.
    fn done(&mut self, fetched: Fetched, now: Instant) {
        apply(&mut self.0.cache, fetched, now);
    }
}

/// Apply a fetch's end to `cache` at `now`.
fn apply(cache: &mut art::Cache, (url, r): Fetched, now: Instant) {
    match r {
        Ok(Some(a)) => cache.done(url, Ok(a), now),
        Ok(None) => cache.abandon(&url),
        Err(e) => cache.done(url, Err(e), now),
    }
}

impl Drop for Kept {
    fn drop(&mut self) {
        let mut left = std::mem::take(&mut self.0);
        left.running
            .store(false, std::sync::atomic::Ordering::SeqCst);
        // Ends already in but not applied: kept no more than those still
        // to come (the shown file was applied when it was shown).
        while let Ok((url, r)) = left.rx.try_recv() {
            apply(&mut left.cache, (url, r.map(|_| None)), Instant::now());
        }
        left.cache.keep_shown();
        let _ = LEFT.try_with(|l| l.borrow_mut().replace(left));
    }
}

/// The state to show: `players`' with remote art replaced by its fetched
/// file (null until it is in, or when it failed), starting its fetch on
/// the blocking pool when it is due.
/// (services-round) With `listing` (a visible reader reads
/// `media.players`), every player too, their art alike; the active
/// player's art is looked up last, so its file is the one the cache
/// keeps above all.
/// (audit-art-read) Remote art is fetched only for the players whose art
/// a visible reader can read: the active player's while `media.art` is
/// read (`art`, a `#[store(stream)]` field), every player's while the
/// list is; otherwise a URL shows its file only when it is already in.
fn shown(players: &Players, kept: &mut Kept, now: Instant, listing: bool, art: bool) -> Media {
    let mut items = if listing {
        players.items(now)
    } else {
        Vec::new()
    };
    for item in &mut items {
        item.art = art_file(kept, item.art.take(), now, true);
    }
    let mut m = players.state(now);
    m.art = art_file(kept, m.art.take(), now, art || listing);
    m.players = items;
    m
}

/// `url` as `art` shows it: local art as sent, remote art's fetched
/// file (none until it is in, starting its fetch when due and `fetch`).
fn art_file(kept: &mut Kept, url: Option<String>, now: Instant, fetch: bool) -> Option<String> {
    let url = url?;
    if !art::is_remote(&url) {
        return Some(url);
    }
    if let Some(file) = kept.0.cache.file(&url) {
        return Some(file);
    }
    if fetch && kept.0.cache.start(&url, now) {
        kept.fetch(url, art::fetch);
    }
    None
}

/// A player appeared (or restarted): follow it and read it whole.
fn arrived(
    conn: &zbus::Connection,
    players: &mut Players,
    tasks: &mut Tasks,
    name: String,
    owner: String,
) {
    let p = Player {
        owner: owner.clone(),
        root_asked: 1,
        pending: Some(Props::new()),
        reading: 1,
        asked: 1,
        ..Player::default()
    };
    players.by_name.insert(name.clone(), p);
    let ask = Ask {
        root: Some(1),
        props: true,
        position: Some(1),
        asked: 1,
    };
    let conn = conn.clone();
    tasks.spawn(async move { Some(read(conn, name, owner, ask).await) });
}

/// When the shown second next changes (while playing): the active
/// player's, or (services-round, `listing`) the soonest of every
/// playing player's.
fn next_tick(players: &Players, now: Instant, listing: bool) -> Option<Instant> {
    if listing {
        return players
            .by_name
            .values()
            .filter(|p| p.known)
            .filter_map(|p| tick_of(p, now))
            .min();
    }
    tick_of(players.active()?, now)
}

/// When `p`'s shown second next changes, while it plays.
fn tick_of(p: &Player, now: Instant) -> Option<Instant> {
    if !p.playing() {
        return None;
    }
    let e = p.elapsed(now, None);
    let into = Duration::from_nanos((e.as_nanos() % 1_000_000_000) as u64);
    // The rate is clamped (RATES): this is at most a thousand seconds.
    let wait =
        Duration::try_from_secs_f64((Duration::from_secs(1) - into).as_secs_f64() / p.rate())
            .unwrap_or(Duration::from_secs(1));
    now.checked_add(wait.max(Duration::from_millis(5)))
}

impl Media {
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let conn = match cx.session().await {
            Ok(c) => c,
            Err(e) => return crate::dbus::idle_without_bus(&mut cx, "session", e).await,
        };
        let dbus_proxy = zbus::fdo::DBusProxy::new(&conn).await?;
        // Players coming and going.
        let owners_rule = MatchRule::builder()
            .msg_type(MessageType::Signal)
            .sender("org.freedesktop.DBus")?
            .interface("org.freedesktop.DBus")?
            .member("NameOwnerChanged")?
            .arg0ns("org.mpris.MediaPlayer2")?
            .build();
        let mut owners = zbus::MessageStream::for_match_rule(owners_rule, &conn, Some(64)).await?;
        let mut signals =
            zbus::MessageStream::for_match_rule(dbus::path_rule(PATH)?, &conn, Some(256)).await?;
        let mut players = Players::default();
        let mut tasks = Tasks::new();
        // (power-media-kbd) Property writes in flight, one per player and
        // property.
        let mut setting = Setting::default();
        let mut sets: tokio::task::JoinSet<SetDone> = tokio::task::JoinSet::new();
        // (player-raise-seek) The players' apps, looked up in the desktop
        // entries on the blocking pool, one batch at a time: the names
        // asked, and the answers to come. `relook`: the entries changed,
        // so every name is looked up again (the older answers standing
        // until then).
        let apps_config = crate::apps::Config::current();
        let mut apps_changes = crate::apps::changes();
        let mut apps_open = true;
        let mut relook = false;
        type Found = Vec<(String, Link)>;
        let mut looking: Option<(Vec<String>, tokio::task::JoinHandle<Found>)> = None;
        for name in dbus_proxy.list_names().await? {
            let name = name.to_string();
            if !name.starts_with(PREFIX) {
                continue;
            }
            let Ok(bus_name) = zbus::names::BusName::try_from(name.as_str()) else {
                continue;
            };
            let Ok(owner) = dbus_proxy.get_name_owner(bus_name).await else {
                continue;
            };
            arrived(&conn, &mut players, &mut tasks, name, owner.to_string());
        }
        // The players there at the start, read all at once (each bounded).
        while players.by_name.values().any(|p| !p.known) {
            match tasks.join_next().await {
                Some(Ok(Some(r))) => {
                    players.answer(r, Instant::now());
                }
                Some(_) => {}
                None => break,
            }
        }
        let mut kept = Kept::take();
        let state = shown(
            &players,
            &mut kept,
            Instant::now(),
            cx.watched("players"),
            cx.watched("art"),
        );
        if !cx.update(|s| *s = state) {
            return Ok(());
        }
        cx.ready();
        loop {
            if looking.is_none() {
                let names = players.to_link(std::mem::take(&mut relook));
                if !names.is_empty() {
                    let config = apps_config.clone();
                    let asked = names.clone();
                    looking = Some((
                        asked,
                        tokio::task::spawn_blocking(move || {
                            let index = crate::apps::AppIndex::shared(&config);
                            names
                                .into_iter()
                                .map(|n| {
                                    let link = index.window(&n);
                                    (n, link)
                                })
                                .collect()
                        }),
                    ));
                }
            }
            let listing = cx.watched("players");
            let ticking = cx.watched("elapsed") || cx.watched("position");
            let tick = if ticking || listing {
                next_tick(&players, Instant::now(), listing)
            } else {
                None
            };
            let changed = tokio::select! {
                Some(done) = tasks.join_next(), if !tasks.is_empty() => match done {
                    Ok(Some(r)) => players.answer(r, Instant::now()),
                    _ => false,
                },
                Some(done) = sets.join_next(), if !sets.is_empty() => match done {
                    Ok(done) => {
                        if let Some(next) = setting.landed(&done.name, done.prop) {
                            sets.spawn(next.run(conn.clone()));
                        } else if !done.ok
                            && let Some(ask) = players.reread(&done.name, &done.owner, done.prop)
                        {
                            // Refused, nothing waiting: its real value back.
                            let conn = conn.clone();
                            tasks.spawn(async move { Some(read(conn, done.name, done.owner, ask).await) });
                        }
                        false
                    }
                    Err(_) => false,
                },
                found = async {
                    match looking.as_mut() {
                        Some((_, j)) => j.await,
                        None => std::future::pending().await,
                    }
                }, if looking.is_some() => {
                    let asked = looking.take().map(|(asked, _)| asked).unwrap_or_default();
                    let found = found.unwrap_or_else(|e| {
                        // Not asked again until the entries change: as
                        // if none matched.
                        log::warn!("media: finding the players' apps: {e}");
                        asked.into_iter().map(|n| (n.clone(), (n, None))).collect()
                    });
                    players.linked(found)
                }
                r = apps_changes.changed(), if apps_open => {
                    match r {
                        Ok(()) => relook = true,
                        Err(_) => apps_open = false,
                    }
                    false
                }
                // Never closed: `kept` holds a sender.
                Some(done) = kept.ended() => {
                    kept.done(done, Instant::now());
                    true
                }
                o = owners.next() => {
                    let Some(Ok(m)) = o else {
                        return Err(ServiceError("the session bus connection ended".into()));
                    };
                    let Ok((name, _old, new)) = m.body().deserialize::<(String, String, String)>() else {
                        continue;
                    };
                    if !name.starts_with(PREFIX) {
                        continue;
                    }
                    if new.is_empty() {
                        players.by_name.remove(&name);
                    } else {
                        arrived(&conn, &mut players, &mut tasks, name, new);
                    }
                    true
                }
                m = signals.next() => {
                    let Some(Ok(m)) = m else {
                        return Err(ServiceError("the session bus connection ended".into()));
                    };
                    signal(&conn, &mut players, &mut tasks, &m)
                }
                () = async {
                    match tick {
                        Some(t) => tokio::time::sleep_until(t.into()).await,
                        None => std::future::pending().await,
                    }
                } => true,
                m = cx.recv() => match m {
                    None => return Ok(()),
                    Some(Msg::Action(a)) => {
                        let (iface, method, name) = match a {
                            MediaAction::PlayPause => (PLAYER, "PlayPause", players.active_name()),
                            MediaAction::Next => (PLAYER, "Next", players.active_name()),
                            MediaAction::Previous => (PLAYER, "Previous", players.active_name()),
                            // (services-round) An item's: that player,
                            // while it is listed.
                            MediaAction::PlayPauseOf { item } => (PLAYER, "PlayPause", players.listed(item.id)),
                            MediaAction::NextOf { item } => (PLAYER, "Next", players.listed(item.id)),
                            MediaAction::PreviousOf { item } => (PLAYER, "Previous", players.listed(item.id)),
                            // (player-raise-seek) Not asked when it says
                            // it cannot.
                            MediaAction::Raise => {
                                (ROOT_IFACE, "Raise", players.active_name().and_then(|n| players.raisable(n)))
                            }
                            MediaAction::RaiseOf { item } => (ROOT_IFACE, "Raise", players.raisable(item.id)),
                            MediaAction::Seek { by } | MediaAction::SeekOf { by, .. } => {
                                let name = match a {
                                    MediaAction::SeekOf { item, .. } => players.listed(item.id),
                                    _ => players.active_name(),
                                };
                                if seek_by(&conn, &mut players, &mut tasks, name, by) {
                                    // The time shows the target now.
                                    let state = shown(&players, &mut kept, Instant::now(), listing, cx.watched("art"));
                                    if !cx.update(|s| *s = state) {
                                        return Ok(());
                                    }
                                }
                                continue;
                            }
                            MediaAction::Select { item } => {
                                if !players.select(&item.id) {
                                    log::info!("media: no player {} to select", item.id);
                                }
                                // The active player may have changed.
                                let state = shown(&players, &mut kept, Instant::now(), listing, cx.watched("art"));
                                if !cx.update(|s| *s = state) {
                                    return Ok(());
                                }
                                continue;
                            }
                        };
                        // A task of its own: a player that does not answer
                        // holds up nothing (dropped with the body).
                        if let Some(name) = name {
                            let conn = conn.clone();
                            tasks.spawn(async move {
                                let r = dbus::timed(conn.call_method(Some(name.as_str()), PATH, Some(iface), method, &())).await;
                                if let Err(e) = r {
                                    log::warn!("media: {method} on {name}: {e}");
                                }
                                None
                            });
                        }
                        false
                    }
                    // (power-media-kbd) `media.shuffle = …`, or an item's
                    // `p.volume = …`: the active player, or that one.
                    Some(Msg::Write(w)) if matches!(w.field, "shuffle" | "loop" | "volume")
                        || (w.field == "players" && player_mode(&w.path).is_some()) => {
                        let now = Instant::now();
                        let (name, field) = if w.field == "players" {
                            let name = match &w.key {
                                Some(Data::Text(n)) => Some(n.to_string()),
                                _ => None,
                            };
                            (name, player_mode(&w.path).unwrap_or_default())
                        } else {
                            (players.active_name(), w.field)
                        };
                        match name.and_then(|n| players.set(&n, field, &w.value, now)) {
                            Some(set) => {
                                if let Some(set) = setting.want(set) {
                                    sets.spawn(set.run(conn.clone()));
                                }
                            }
                            None => log::info!("media: no player takes {field} = {:?}", w.value),
                        }
                        // The value stands (or is kept): the write is
                        // answered with it.
                        let state = shown(&players, &mut kept, now, listing, cx.watched("art"));
                        if !cx.report(&w, |s| *s = state) {
                            return Ok(());
                        }
                        false
                    }
                    Some(Msg::Write(w)) if w.field == "position" || w.field == "players" => {
                        let now = Instant::now();
                        let seek = if w.field == "position" {
                            players.seek(w.value().unwrap_or(f64::NAN), now)
                        } else {
                            // (services-round) `p.position = x` of an item:
                            // that player, by its bus name.
                            let to = match (w.path.as_slice(), &w.value) {
                                ([crate::Step::Field(f)], Data::Float(x)) if f.as_str() == "position" => *x,
                                ([crate::Step::Field(f)], Data::Int(x)) if f.as_str() == "position" => *x as f64,
                                _ => f64::NAN,
                            };
                            match &w.key {
                                Some(Data::Text(name)) => players.seek_player(name, to, now),
                                _ => None,
                            }
                        };
                        match seek {
                            Some(seek) => {
                                let conn = conn.clone();
                                tasks.spawn(seek.run(conn));
                            }
                            None => log::info!("media: no player can seek to that"),
                        }
                        // The fix stands (or the place is kept): the
                        // write is answered with it.
                        let state = shown(&players, &mut kept, now, listing, cx.watched("art"));
                        if !cx.report(&w, |s| *s = state) {
                            return Ok(());
                        }
                        false
                    }
                    // Watching starts or stops the ticks (next turn).
                    Some(_) => true,
                },
            };
            if changed {
                let state = shown(
                    &players,
                    &mut kept,
                    Instant::now(),
                    cx.watched("players"),
                    cx.watched("art"),
                );
                if !cx.update(|s| *s = state) {
                    return Ok(());
                }
            }
        }
    }
}

/// (player-raise-seek) `seek(by)` of the player `name` (`by` seconds,
/// back when negative): the `Seek` sent in a task, the time fixed at its
/// target ([`Players::seek_by`]). Whether anything changed.
fn seek_by(
    conn: &zbus::Connection,
    players: &mut Players,
    tasks: &mut Tasks,
    name: Option<String>,
    crate::Offset(by): crate::Offset,
) -> bool {
    match name.and_then(|n| players.seek_by(&n, by, Instant::now())) {
        Some(seek) => {
            tasks.spawn(seek.run(conn.clone()));
            true
        }
        None => {
            log::info!("media: no player can seek by {by} s");
            false
        }
    }
}

/// (power-media-kbd) The player field an item write's `path` names, when
/// it is `shuffle`, `loop` or `volume` (`p.volume = 0.5`).
fn player_mode(path: &[crate::Step]) -> Option<&'static str> {
    match path {
        [crate::Step::Field(f)] => ["shuffle", "loop", "volume"]
            .into_iter()
            .find(|m| *m == f.as_str()),
        _ => None,
    }
}

/// Apply a player's signal; whether anything changed. What must be asked
/// again (invalidated properties, the position after a change) is asked
/// in a task, never awaited here.
fn signal(
    conn: &zbus::Connection,
    players: &mut Players,
    tasks: &mut Tasks,
    m: &zbus::Message,
) -> bool {
    let Some(sender) = m.header().sender().map(|s| s.to_string()) else {
        return false;
    };
    let member = dbus::member(m);
    let Some((name, p)) = players.by_owner(&sender) else {
        return false;
    };
    let name = name.clone();
    if member.as_deref() == Some("Seeked") && dbus::interface(m).as_deref() == Some(PLAYER) {
        let pos: i64 = m.body().deserialize().unwrap_or(0);
        players.seeked(&name, pos, Instant::now());
        return true;
    }
    let owner = p.owner.clone();
    let Some(c) = dbus::properties_changed(m) else {
        return false;
    };
    if c.iface == ROOT_IFACE {
        let Some(ask) = players.root_changed(&name, c.changed, &c.invalidated) else {
            return false;
        };
        let conn = conn.clone();
        tasks.spawn(async move { Some(read(conn, name, owner, ask).await) });
        return true;
    }
    if c.iface != PLAYER {
        return false;
    }
    let ask = players.changed(&name, c.changed, &c.invalidated, Instant::now());
    if ask.props || ask.position.is_some() {
        let conn = conn.clone();
        tasks.spawn(async move { Some(read(conn, name, owner, ask).await) });
    }
    true
}

impl Players {
    /// Apply a `Seeked` from `name`'s player (to `pos` microseconds) at
    /// `now`: the position fix stands over any answer asked before it.
    fn seeked(&mut self, name: &str, pos: i64, now: Instant) {
        if let Some(p) = self.by_name.get_mut(name) {
            p.asked += 1;
            p.at = Some((Duration::from_micros(pos.max(0) as u64), now));
        }
    }

    /// Apply a `PropertiesChanged` from `name`'s player at `now`; what to
    /// ask it again.
    fn changed(&mut self, name: &str, changed: Props, invalidated: &[String], now: Instant) -> Ask {
        let mut ask = Ask::default();
        let Some(p) = self.by_name.get_mut(name) else {
            return ask;
        };
        // Carry the position forward to now before the state changes, then
        // ask again where the state, track or rate moved it.
        let length = p.metadata().get("mpris:length").and_then(micros);
        let here = p.elapsed(now, length);
        p.at = Some((here, now));
        let status_moved = changed.contains_key("PlaybackStatus")
            || invalidated.iter().any(|i| i == "PlaybackStatus");
        let metadata = changed.contains_key("Metadata");
        if status_moved
            || metadata
            || changed.contains_key("Rate")
            || invalidated.iter().any(|i| i == "Metadata" || i == "Rate")
        {
            p.asked += 1;
            ask.position = Some(p.asked);
        }
        for (k, v) in changed {
            if let Some(pending) = &mut p.pending
                && let Ok(v2) = v.try_clone()
            {
                pending.insert(k.clone(), v2);
            }
            p.props.insert(k, v);
        }
        // Another track starts at 0 now, in the same update as its title
        // (the question just asked corrects it); the same track with new
        // art or a length keeps its carried position.
        if metadata {
            p.note_track(now, true);
        }
        if !invalidated.is_empty() {
            for i in invalidated {
                p.props.remove(i);
                if let Some(pending) = &mut p.pending {
                    pending.remove(i);
                }
            }
            // Read it whole again (one call for any number of properties).
            ask.props = true;
            p.reading += 1;
            p.pending.get_or_insert_with(Props::new);
        }
        ask.asked = p.asked;
        if status_moved {
            self.touch(name);
        }
        ask
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fetched picture's file, distinct by `i`.
    fn art_file(i: u8) -> art::Art {
        art::Art {
            file: crate::pixmap::write_file(&[0x89, b'P', b'N', b'G', i, 7], "png").unwrap(),
            bytes: 6,
        }
    }

    /// (m6-services, services-r3-1) A body ends while its fetches run (a
    /// blocking fetch is not cancelled): the next body counts them, so
    /// the URL is not fetched again beside itself and IN_FLIGHT holds
    /// across bodies; a failure that ends after the body is remembered by
    /// the next; and a picture that comes in while no body runs is not
    /// kept (only the shown file is), its URL free to be fetched again.
    #[test]
    fn fetches_running_when_a_body_ends_are_counted_by_the_next() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _enter = rt.enter();
        let t0 = Instant::now();
        let (fail, failing) = std::sync::mpsc::channel::<()>();
        let (finish, finishing) = std::sync::mpsc::channel::<()>();
        let (bad, slow) = ("https://a/bad", "https://a/slow");
        let file = std::sync::Arc::new(std::sync::Mutex::new(None));
        {
            let mut kept = Kept::take();
            assert!(kept.0.cache.start(bad, t0));
            kept.fetch(bad.into(), move |_| {
                let _ = failing.recv();
                Err("refused".into())
            });
            assert!(kept.0.cache.start(slow, t0));
            let f = file.clone();
            kept.fetch(slow.into(), move |_| {
                let _ = finishing.recv();
                let a = art_file(1);
                *f.lock().unwrap() = Some(a.file.text());
                Ok(a)
            });
        }
        let mut kept = Kept::take();
        assert_eq!(kept.0.cache.in_flight(), 2, "the running fetches counted");
        assert!(!kept.0.cache.start(bad, t0), "fetched again beside itself");
        drop(kept);

        // Both end while no body runs.
        fail.send(()).unwrap();
        finish.send(()).unwrap();
        let ended = || LEFT.with(|l| l.borrow().as_ref().map_or(0, |l| l.rx.len()));
        let t = Instant::now();
        while ended() < 2 {
            assert!(
                t.elapsed() < Duration::from_secs(10),
                "the fetches never ended"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut kept = Kept::take();
        for _ in 0..2 {
            let ended = rt.block_on(kept.ended()).unwrap();
            kept.done(ended, t0);
        }
        assert_eq!(kept.0.cache.in_flight(), 0);
        assert!(!kept.0.cache.start(bad, t0), "the failure remembered");
        assert_eq!(kept.0.cache.kept(), 0, "nothing kept while stopped");
        let path = file.lock().unwrap().clone().unwrap();
        assert!(!std::path::Path::new(&path).exists(), "its file went");
        assert!(kept.0.cache.start(slow, t0));
    }

    /// (m6-services, services-r3-2) What a media body leaves is its
    /// services runtime's (one thread each): a body of another runtime in
    /// the process ending meanwhile neither takes it nor replaces it.
    #[test]
    fn another_runtime_s_body_does_not_take_what_this_one_left() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _enter = rt.enter();
        let t0 = Instant::now();
        let url = "https://a/shown";
        {
            let mut kept = Kept::take();
            assert!(kept.0.cache.start(url, t0));
            kept.done((url.into(), Ok(Some(art_file(2)))), t0);
            assert!(kept.0.cache.file(url).is_some());
        }
        std::thread::spawn(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let _enter = rt.enter();
            drop(Kept::take());
        })
        .join()
        .unwrap();
        let mut kept = Kept::take();
        let file = kept
            .0
            .cache
            .file(url)
            .expect("the shown file is still left");
        assert!(std::path::Path::new(&file).exists());
    }

    /// Two reads in flight, a change signalled between their answers, and
    /// the older read answering last: the change stands.
    #[test]
    fn a_stale_read_answering_last_does_not_undo_a_signal() {
        use zbus::zvariant::{OwnedValue, Value};
        let status = |s: &str| {
            Props::from([(
                "PlaybackStatus".to_string(),
                OwnedValue::try_from(Value::from(s)).unwrap(),
            )])
        };
        let mut players = Players::default();
        players.by_name.insert(
            "org.mpris.MediaPlayer2.p".into(),
            Player {
                owner: ":1.5".into(),
                pending: Some(Props::new()),
                reading: 2,
                ..Player::default()
            },
        );
        let answer = |props: Props| Read {
            name: "org.mpris.MediaPlayer2.p".into(),
            owner: ":1.5".into(),
            asked: 0,
            root: None,
            props: Some(Ok(props)),
            position: None,
        };
        assert!(players.answer(answer(status("Paused")), Instant::now()));
        // Signalled now: playing.
        let p = players.by_name.get_mut("org.mpris.MediaPlayer2.p").unwrap();
        p.props.extend(status("Playing"));
        p.pending.as_mut().unwrap().extend(status("Playing"));
        // The other read, asked before the signal, answers last.
        assert!(players.answer(answer(status("Paused")), Instant::now()));
        let p = &players.by_name["org.mpris.MediaPlayer2.p"];
        assert_eq!(p.status(), "Playing");
        assert_eq!(p.reading, 0);
        assert!(p.pending.is_none(), "nothing in flight: nothing held");
    }

    #[test]
    fn local_art_is_shown_as_sent() {
        assert!(local_art("file:///tmp/a.png"));
        assert!(local_art("/tmp/a.png"));
        assert!(!local_art("https://i.scdn.co/image/ab67"));
        assert!(!local_art("http://x/a.jpg"));
    }

    const P: &str = "org.mpris.MediaPlayer2.p";

    /// A `Metadata` value: these entries, with a fixed length and art.
    fn meta(entries: &[(&str, zbus::zvariant::Value<'static>)]) -> OwnedValue {
        use zbus::zvariant::Value;
        let mut m: std::collections::HashMap<String, Value<'static>> =
            std::collections::HashMap::from([
                ("mpris:length".to_string(), Value::from(200_000_000i64)),
                ("mpris:artUrl".to_string(), Value::from("file:///a.png")),
            ]);
        for (k, v) in entries {
            m.insert((*k).to_string(), v.try_clone().unwrap());
        }
        OwnedValue::try_from(Value::from(m)).unwrap()
    }

    fn id(path: &'static str) -> zbus::zvariant::Value<'static> {
        zbus::zvariant::ObjectPath::try_from(path).unwrap().into()
    }

    fn text(s: &'static str) -> zbus::zvariant::Value<'static> {
        s.into()
    }

    /// One player, known and playing `metadata` at 120 s as of `t0`.
    fn playing(metadata: OwnedValue, t0: Instant) -> Players {
        let mut players = Players::default();
        let mut p = Player {
            owner: ":1.5".into(),
            known: true,
            asked: 1,
            at: Some((Duration::from_secs(120), t0)),
            ..Player::default()
        };
        p.props.insert(
            "PlaybackStatus".into(),
            OwnedValue::try_from(zbus::zvariant::Value::from("Playing")).unwrap(),
        );
        p.props.insert("Metadata".into(), metadata);
        p.note_track(t0, true);
        players.by_name.insert(P.into(), p);
        players
    }

    fn metadata_changed(players: &mut Players, m: OwnedValue, now: Instant) -> Ask {
        players.changed(P, Props::from([("Metadata".to_string(), m)]), &[], now)
    }

    /// A `Metadata` naming another track: the title and a time of 0 in
    /// the same state, with the position asked again.
    #[test]
    fn another_trackid_starts_the_position_at_zero_at_once() {
        let t0 = Instant::now();
        let a = || {
            meta(&[
                ("mpris:trackid", id("/t/1")),
                ("xesam:title", text("First")),
            ])
        };
        let mut players = playing(a(), t0);
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(players.state(t1).elapsed, Duration::from_secs(121));
        let b = meta(&[
            ("mpris:trackid", id("/t/2")),
            ("xesam:title", text("Second")),
        ]);
        let ask = metadata_changed(&mut players, b, t1);
        assert_eq!(ask.position, Some(2), "the position is asked again");
        let s = players.state(t1);
        assert_eq!(s.title.as_deref(), Some("Second"));
        assert_eq!(s.elapsed, Duration::ZERO);
        assert_eq!(s.position, 0.0);
        // It plays on from 0.
        let t2 = t1 + Duration::from_secs(2);
        assert_eq!(players.state(t2).elapsed, Duration::from_secs(2));
        // The position answer still says where it is.
        let p = &players.by_name[P];
        let r = Read {
            name: P.into(),
            owner: p.owner.clone(),
            asked: 2,
            root: None,
            props: None,
            position: Some((2, Some(Duration::from_millis(1500)), t2)),
        };
        assert!(players.answer(r, t2));
        assert_eq!(players.state(t2).elapsed, Duration::from_millis(1500));
    }

    /// New art or a length for the track playing: the time carries on.
    #[test]
    fn the_same_trackid_with_new_art_keeps_its_position() {
        let t0 = Instant::now();
        let mut players = playing(
            meta(&[
                ("mpris:trackid", id("/t/1")),
                ("xesam:title", text("First")),
            ]),
            t0,
        );
        let t1 = t0 + Duration::from_secs(1);
        let new_art = meta(&[
            ("mpris:trackid", id("/t/1")),
            ("xesam:title", text("First")),
            ("mpris:artUrl", text("file:///b.png")),
            ("mpris:length", zbus::zvariant::Value::from(300_000_000i64)),
        ]);
        metadata_changed(&mut players, new_art, t1);
        let s = players.state(t1);
        assert_eq!(s.art.as_deref(), Some("file:///b.png"));
        assert_eq!(s.length, Some(Duration::from_secs(300)));
        assert_eq!(s.elapsed, Duration::from_secs(121));
        // A trackid string (not an object path) is read as one too, and a
        // retitled track with the same trackid is still the same track.
        let renamed = meta(&[
            ("mpris:trackid", text("/t/1")),
            ("xesam:title", text("Renamed")),
        ]);
        metadata_changed(&mut players, renamed, t1);
        assert_eq!(players.state(t1).elapsed, Duration::from_secs(121));
    }

    /// No trackid: the URL tells tracks apart, else the title and artist.
    #[test]
    fn without_a_trackid_the_url_then_the_title_and_artist_tell_tracks_apart() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        // By URL: a retitled URL is the same track; another URL is not.
        let mut players = playing(
            meta(&[
                ("xesam:url", text("file:///1.ogg")),
                ("xesam:title", text("First")),
            ]),
            t0,
        );
        let retitled = meta(&[
            ("xesam:url", text("file:///1.ogg")),
            ("xesam:title", text("Uno")),
        ]);
        metadata_changed(&mut players, retitled, t1);
        assert_eq!(players.state(t1).elapsed, Duration::from_secs(121));
        let other = meta(&[
            ("xesam:url", text("file:///2.ogg")),
            ("xesam:title", text("Uno")),
        ]);
        metadata_changed(&mut players, other, t1);
        assert_eq!(players.state(t1).elapsed, Duration::ZERO);

        // By title and artist (the test player's case).
        let artist = |a: &'static str| zbus::zvariant::Value::from(vec![a]);
        let mut players = playing(
            meta(&[
                ("xesam:title", text("First")),
                ("xesam:artist", artist("Ann")),
            ]),
            t0,
        );
        let new_art = meta(&[
            ("xesam:title", text("First")),
            ("xesam:artist", artist("Ann")),
            ("mpris:artUrl", text("file:///b.png")),
        ]);
        metadata_changed(&mut players, new_art, t1);
        assert_eq!(players.state(t1).elapsed, Duration::from_secs(121));
        let cover = meta(&[
            ("xesam:title", text("First")),
            ("xesam:artist", artist("Bo")),
        ]);
        metadata_changed(&mut players, cover, t1);
        assert_eq!(players.state(t1).elapsed, Duration::ZERO);
    }

    /// Track identity compares the first key both name.
    #[test]
    fn tracks_compare_by_the_first_key_both_name() {
        let t = |id: Option<&str>, url: Option<&str>, title: Option<&str>| Track {
            id: id.map(Into::into),
            url: url.map(Into::into),
            title: title.map(|t| (t.into(), String::new())),
        };
        // A trackid added to a track named by title alone: the same.
        assert!(t(None, None, Some("A")).same(&t(Some("/1"), None, Some("A"))));
        // Trackids differ: another track, whatever the titles say.
        assert!(!t(Some("/1"), None, Some("A")).same(&t(Some("/2"), None, Some("A"))));
        // Nothing shared: only two empty ones are the same.
        assert!(t(None, None, None).same(&t(None, None, None)));
        assert!(!t(None, None, None).same(&t(None, None, Some("A"))));
        assert!(!t(Some("/1"), None, None).same(&t(None, Some("u"), None)));
    }

    /// A track change signalled as an invalidated `Metadata`: the time
    /// starts at 0 when the whole read lands (its own position answer,
    /// when current, says where instead); a re-read naming the same track
    /// leaves the time alone.
    #[test]
    fn an_invalidated_metadata_naming_another_track_starts_at_zero_when_read() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        let t2 = t1 + Duration::from_secs(1);
        let (a, b, whole) = (track_a, track_b, whole_read);
        let invalidate = |players: &mut Players| {
            let ask = players.changed(P, Props::new(), &["Metadata".to_string()], t1);
            assert!(ask.props);
            assert_eq!(ask.position, Some(2));
            assert_eq!(ask.asked, 2);
        };

        // Another track found by a read that asked no position (only
        // something else was invalidated), nothing asked since: 0 from
        // when it landed.
        let mut players = playing(a(), t0);
        let ask = players.changed(P, Props::new(), &["Volume".to_string()], t1);
        assert!(ask.props);
        assert_eq!((ask.position, ask.asked), (None, 1));
        assert!(players.answer(whole(1, b(), None), t2));
        let s = players.state(t2);
        assert_eq!(s.title.as_deref(), Some("Second"));
        assert_eq!(s.elapsed, Duration::ZERO);
        let t3 = t2 + Duration::from_secs(4);
        assert_eq!(players.state(t3).elapsed, Duration::from_secs(4));

        // Another track with its current position answer: that stands.
        let mut players = playing(a(), t0);
        invalidate(&mut players);
        let answer = Some((2, Some(Duration::from_millis(300)), t2));
        assert!(players.answer(whole(2, b(), answer), t2));
        assert_eq!(players.state(t2).elapsed, Duration::from_millis(300));

        // The same track re-read: the carried time stands.
        let mut players = playing(a(), t0);
        invalidate(&mut players);
        assert!(players.answer(whole(2, a(), None), t2));
        assert_eq!(players.state(t2).elapsed, Duration::from_secs(122));
    }

    /// A position fix newer than an invalidated `Metadata`'s whole read
    /// (a seek, or a later question's answer landing first) stands when
    /// that read lands naming another track; a later question still in
    /// flight when it lands corrects the time once it answers.
    #[test]
    fn a_position_fix_newer_than_the_whole_read_survives_it() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        let t2 = t1 + Duration::from_secs(1);
        let later = t2 + Duration::from_secs(10);
        let (a, b, whole) = (track_a, track_b, whole_read);
        let invalidate = |players: &mut Players| {
            let ask = players.changed(P, Props::new(), &["Metadata".to_string()], t1);
            assert_eq!((ask.position, ask.asked), (Some(2), 2));
        };
        let position = |asked: u64, pos: Duration, at: Instant| Read {
            name: P.into(),
            owner: ":1.5".into(),
            asked,
            root: None,
            props: None,
            position: Some((asked, Some(pos), at)),
        };
        let stale_answer = Some((2, Some(Duration::from_secs(9)), t2));

        // A seek to 50 s, then the read lands with its stale answer.
        let mut players = playing(a(), t0);
        invalidate(&mut players);
        let seek_at = t1 + Duration::from_millis(500);
        players.seeked(P, 50_000_000, seek_at);
        assert!(players.answer(whole(2, b(), stale_answer), t2));
        let s = players.state(later);
        assert_eq!(s.title.as_deref(), Some("Second"));
        assert_eq!(s.elapsed, Duration::from_millis(60_500));

        // The status moves (question 3); its answer lands first, then
        // the read.
        let mut players = playing(a(), t0);
        invalidate(&mut players);
        let ask = players.changed(P, status("Playing"), &[], t1);
        assert_eq!((ask.position, ask.asked), (Some(3), 3));
        assert!(players.answer(position(3, Duration::from_secs(5), t1), t1));
        assert!(players.answer(whole(2, b(), stale_answer), t2));
        let s = players.state(later);
        assert_eq!(s.title.as_deref(), Some("Second"));
        assert_eq!(s.elapsed, Duration::from_secs(16));

        // Question 3 still in flight when the read lands: the read leaves
        // the time alone and question 3's answer then says where.
        let mut players = playing(a(), t0);
        invalidate(&mut players);
        players.changed(P, status("Playing"), &[], t1);
        assert!(players.answer(whole(2, b(), stale_answer), t2));
        assert_eq!(players.state(t2).title.as_deref(), Some("Second"));
        let t3 = t2 + Duration::from_millis(100);
        assert!(players.answer(position(3, Duration::from_secs(1), t3), t3));
        let s = players.state(later);
        assert_eq!(s.elapsed, Duration::from_millis(10_900));
    }

    fn track_a() -> OwnedValue {
        meta(&[
            ("mpris:trackid", id("/t/1")),
            ("xesam:title", text("First")),
        ])
    }

    fn track_b() -> OwnedValue {
        meta(&[
            ("mpris:trackid", id("/t/2")),
            ("xesam:title", text("Second")),
        ])
    }

    fn status(s: &str) -> Props {
        Props::from([(
            "PlaybackStatus".to_string(),
            OwnedValue::try_from(zbus::zvariant::Value::from(s)).unwrap(),
        )])
    }

    /// A whole read asked when the player's latest question was `asked`,
    /// answering playing `m`.
    fn whole_read(
        asked: u64,
        m: OwnedValue,
        position: Option<(u64, Option<Duration>, Instant)>,
    ) -> Read {
        let mut props = status("Playing");
        props.insert("Metadata".to_string(), m);
        Read {
            name: P.into(),
            owner: ":1.5".into(),
            asked,
            root: None,
            props: Some(Ok(props)),
            position,
        }
    }

    /// A read asked before a track change answering after it (with the
    /// old track): no second reset, the new track's answered time stands.
    #[test]
    fn a_stale_read_naming_the_old_track_does_not_reset_the_new_one() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        let a = || {
            meta(&[
                ("mpris:trackid", id("/t/1")),
                ("xesam:title", text("First")),
            ])
        };
        let b = || {
            meta(&[
                ("mpris:trackid", id("/t/2")),
                ("xesam:title", text("Second")),
            ])
        };
        let mut players = playing(a(), t0);
        {
            let p = players.by_name.get_mut(P).unwrap();
            p.reading = 1;
            p.pending = Some(Props::new());
        }
        let ask = metadata_changed(&mut players, b(), t1);
        assert_eq!(players.state(t1).elapsed, Duration::ZERO);
        let t2 = t1 + Duration::from_secs(1);
        let pos = Read {
            name: P.into(),
            owner: ":1.5".into(),
            asked: ask.asked,
            root: None,
            props: None,
            position: Some((ask.position.unwrap(), Some(Duration::from_secs(3)), t2)),
        };
        assert!(players.answer(pos, t2));
        let stale = Read {
            name: P.into(),
            owner: ":1.5".into(),
            // Asked before the track change (question 2).
            asked: 1,
            root: None,
            props: Some(Ok(Props::from([
                (
                    "PlaybackStatus".to_string(),
                    OwnedValue::try_from(zbus::zvariant::Value::from("Playing")).unwrap(),
                ),
                ("Metadata".to_string(), a()),
            ]))),
            position: None,
        };
        let t3 = t2 + Duration::from_secs(1);
        assert!(players.answer(stale, t3));
        let s = players.state(t3);
        assert_eq!(s.title.as_deref(), Some("Second"));
        assert_eq!(s.elapsed, Duration::from_secs(4));
    }

    fn flag(b: bool) -> OwnedValue {
        OwnedValue::from(b)
    }

    /// (gap 32, m6-service-gaps) What the active player can do, from its
    /// properties: an absent property counts as able, `CanControl` false
    /// makes all false, `can_seek` needs a known length too, and the
    /// `position` write asks the very same question.
    #[test]
    fn the_state_says_what_the_active_player_can_do() {
        let t0 = Instant::now();
        assert!(!Players::default().state(t0).can_next, "no player");
        let mut players = playing(meta(&[("xesam:title", text("First"))]), t0);
        let s = players.state(t0);
        assert!(s.can_next && s.can_previous && s.can_seek, "absent: able");
        let set = |players: &mut Players, k: &str, v: bool| {
            players
                .by_name
                .get_mut(P)
                .unwrap()
                .props
                .insert(k.into(), flag(v));
        };
        set(&mut players, "CanGoNext", false);
        set(&mut players, "CanGoPrevious", true);
        let s = players.state(t0);
        assert!(!s.can_next && s.can_previous && s.can_seek);
        set(&mut players, "CanSeek", false);
        assert!(!players.state(t0).can_seek);
        assert_eq!(players.seek(0.5, t0), None, "the write agrees");
        set(&mut players, "CanSeek", true);
        set(&mut players, "CanControl", false);
        let s = players.state(t0);
        assert!(!s.can_next && !s.can_previous && !s.can_seek);
        assert_eq!(players.seek(0.5, t0), None, "the write agrees");
        set(&mut players, "CanControl", true);
        assert!(players.state(t0).can_seek);
        // A track of unknown length cannot be sought.
        let mut m: std::collections::HashMap<String, zbus::zvariant::Value<'static>> =
            std::collections::HashMap::new();
        m.insert("xesam:title".into(), text("Live"));
        let unknown = OwnedValue::try_from(zbus::zvariant::Value::from(m)).unwrap();
        metadata_changed(&mut players, unknown, t0);
        let s = players.state(t0);
        assert!(
            !s.can_next && s.can_previous && !s.can_seek,
            "CanGoNext is still false"
        );
        assert_eq!(players.seek(0.5, t0), None, "the write agrees");
    }

    /// An invalidated capability is read again (with the rest of the
    /// player's properties), never left unknown.
    #[test]
    fn an_invalidated_capability_is_read_again() {
        let t0 = Instant::now();
        let mut players = playing(meta(&[("xesam:title", text("First"))]), t0);
        players
            .by_name
            .get_mut(P)
            .unwrap()
            .props
            .insert("CanGoNext".into(), flag(false));
        let ask = players.changed(P, Props::new(), &["CanGoNext".to_string()], t0);
        assert!(ask.props, "read again");
    }

    fn put(players: &mut Players, k: &str, v: OwnedValue) {
        players
            .by_name
            .get_mut(P)
            .unwrap()
            .props
            .insert(k.into(), v);
    }

    fn loop_status(s: &str) -> OwnedValue {
        OwnedValue::try_from(zbus::zvariant::Value::from(s)).unwrap()
    }

    /// (power-media-kbd) A player that reports no `Shuffle`, `LoopStatus`
    /// or `Volume` has none to set: its `can_` fields are false, its
    /// values the defaults. Reported, they are read and settable; an
    /// unknown `LoopStatus` is `none`; a volume above 1 shows as reported,
    /// one below 0 as 0; no player is the defaults.
    #[test]
    fn a_player_without_shuffle_or_loop_says_so() {
        let t0 = Instant::now();
        let none = Players::default().state(t0);
        assert!(!none.shuffle && !none.can_shuffle && !none.can_loop && !none.can_volume);
        assert_eq!((none.r#loop, none.volume), (Loop::None, 0.0));
        let mut players = playing(meta(&[("xesam:title", text("First"))]), t0);
        let s = players.state(t0);
        assert!(
            !s.can_shuffle && !s.can_loop && !s.can_volume,
            "not reported"
        );
        assert!(!s.shuffle);
        assert_eq!((s.r#loop, s.volume), (Loop::None, 0.0));
        assert!(
            players.set(P, "shuffle", &Data::Bool(true), t0).is_none(),
            "the write agrees"
        );
        put(&mut players, "Shuffle", flag(true));
        put(&mut players, "LoopStatus", loop_status("Track"));
        put(&mut players, "Volume", OwnedValue::from(1.5f64));
        let s = players.state(t0);
        assert!(s.shuffle && s.can_shuffle && s.can_loop && s.can_volume);
        assert_eq!((s.r#loop, s.volume), (Loop::Track, 1.5));
        let item = &players.items(t0)[0];
        assert!(item.shuffle && item.can_volume);
        assert_eq!((item.r#loop, item.volume), (Loop::Track, 1.5));
        put(&mut players, "LoopStatus", loop_status("Playlist"));
        assert_eq!(players.state(t0).r#loop, Loop::Playlist);
        put(&mut players, "LoopStatus", loop_status("Shuffle"));
        assert_eq!(players.state(t0).r#loop, Loop::None);
        put(&mut players, "Volume", OwnedValue::from(-0.2f64));
        assert_eq!(players.state(t0).volume, 0.0);
        put(&mut players, "CanControl", flag(false));
        let s = players.state(t0);
        assert!(!s.can_shuffle && !s.can_loop && !s.can_volume, "no control");
        assert!(players.set(P, "volume", &Data::Float(0.5), t0).is_none());
        for l in [Loop::None, Loop::Track, Loop::Playlist] {
            assert_eq!(Loop::of_mpris(l.mpris_name()), l);
        }
    }

    /// (power-media-kbd) A write fixes the value in the player's
    /// properties and in what a read in flight will be answered over, so
    /// a stale whole read landing later does not undo it; a volume is held
    /// to 0 to 1 and a value of the wrong kind is no write; and one `Set`
    /// per player and property is in flight, the newest waiting value
    /// replacing older ones.
    #[test]
    fn a_write_fixes_the_value_and_waits_its_turn() {
        use crate::ToData;
        let t0 = Instant::now();
        let mut players = playing(meta(&[("xesam:title", text("First"))]), t0);
        put(&mut players, "Shuffle", flag(false));
        put(&mut players, "LoopStatus", loop_status("None"));
        put(&mut players, "Volume", OwnedValue::from(0.8f64));
        {
            // A whole read in flight, asked before the writes.
            let p = players.by_name.get_mut(P).unwrap();
            p.reading = 1;
            p.pending = Some(Props::new());
        }
        let shuffle = players.set(P, "shuffle", &Data::Bool(true), t0).unwrap();
        assert_eq!((shuffle.prop, shuffle.owner.as_str()), ("Shuffle", ":1.5"));
        assert_eq!(
            dbus::boolean(
                &Props::from([("v".into(), shuffle.value.try_clone().unwrap())]),
                "v"
            ),
            Some(true)
        );
        let looped = players
            .set(P, "loop", &Loop::Playlist.to_data(), t0)
            .unwrap();
        assert_eq!(looped.prop, "LoopStatus");
        assert_eq!(looped.value.downcast_ref::<&str>().unwrap(), "Playlist");
        let loud = players.set(P, "volume", &Data::Float(1.4), t0).unwrap();
        assert_eq!(loud.value.downcast_ref::<f64>().unwrap(), 1.0, "held to 1");
        let s = players.state(t0);
        assert!(s.shuffle);
        assert_eq!((s.r#loop, s.volume), (Loop::Playlist, 1.0), "at once");
        assert!(
            players
                .set(P, "volume", &Data::Float(f64::NAN), t0)
                .is_none()
        );
        assert!(players.set(P, "shuffle", &Data::Float(1.0), t0).is_none());
        assert!(
            players
                .set(
                    "org.mpris.MediaPlayer2.gone",
                    "shuffle",
                    &Data::Bool(true),
                    t0
                )
                .is_none()
        );
        // The stale read lands, saying what the player had before.
        let mut before = status("Playing");
        before.insert("Shuffle".into(), flag(false));
        before.insert("LoopStatus".into(), loop_status("None"));
        before.insert("Volume".into(), OwnedValue::from(0.8f64));
        before.insert("Metadata".into(), meta(&[("xesam:title", text("First"))]));
        let stale = Read {
            name: P.into(),
            owner: ":1.5".into(),
            asked: 1,
            root: None,
            props: Some(Ok(before)),
            position: None,
        };
        assert!(players.answer(stale, t0));
        let s = players.state(t0);
        assert!(s.shuffle, "the stale read did not undo it");
        assert_eq!((s.r#loop, s.volume), (Loop::Playlist, 1.0));

        // One in flight per player and property.
        let volume =
            |players: &mut Players, x: f64| players.set(P, "volume", &Data::Float(x), t0).unwrap();
        let mut setting = Setting::default();
        let first = setting.want(volume(&mut players, 0.1)).unwrap();
        assert!(setting.want(volume(&mut players, 0.2)).is_none(), "waits");
        assert!(
            setting.want(volume(&mut players, 0.3)).is_none(),
            "replaces"
        );
        // Another property goes at once.
        assert!(
            setting
                .want(players.set(P, "shuffle", &Data::Bool(false), t0).unwrap())
                .is_some()
        );
        assert_eq!(players.state(t0).volume, 0.3, "the last written stands");
        let next = setting.landed(&first.name, first.prop).unwrap();
        assert_eq!(next.value.downcast_ref::<f64>().unwrap(), 0.3, "the newest");
        assert!(
            setting.landed(&next.name, next.prop).is_none(),
            "nothing waits"
        );
        assert!(
            setting.want(volume(&mut players, 0.4)).is_some(),
            "free again"
        );
        // A refused one reads the player whole again, while it is the same.
        assert!(
            players.reread(P, ":1.9", "Volume").is_none(),
            "another owner"
        );
        let ask = players.reread(P, ":1.5", "Volume").unwrap();
        assert!(ask.props);
    }

    /// A refused write made while a whole read is in flight: the reread's
    /// answer is the player's real value, not the refused one laid over
    /// it, whichever read lands last; a slider drag on a player refusing
    /// every volume settles on the player's value too; and a write made
    /// after the reread was asked still stands over it.
    #[test]
    fn a_refused_write_comes_back_with_a_read_in_flight() {
        let t0 = Instant::now();
        let whole = |asked: u64, volume: f64| {
            let mut props = status("Playing");
            props.insert("Volume".into(), OwnedValue::from(volume));
            props.insert("Metadata".into(), meta(&[("xesam:title", text("First"))]));
            Read {
                name: P.into(),
                owner: ":1.5".into(),
                asked,
                root: None,
                props: Some(Ok(props)),
                position: None,
            }
        };
        let in_flight = |players: &mut Players| {
            let p = players.by_name.get_mut(P).unwrap();
            p.reading = 1;
            p.pending = Some(Props::new());
        };
        let volume =
            |players: &mut Players, x: f64| players.set(P, "volume", &Data::Float(x), t0).unwrap();

        // A whole read in flight, a write of 0.6 refused, reread.
        let mut players = playing(meta(&[("xesam:title", text("First"))]), t0);
        put(&mut players, "Volume", OwnedValue::from(0.3f64));
        in_flight(&mut players);
        volume(&mut players, 0.6);
        assert_eq!(players.state(t0).volume, 0.6, "at once");
        assert!(players.reread(P, ":1.5", "Volume").is_some());
        // The stale read lands, then the reread.
        players.answer(whole(1, 0.3), t0);
        players.answer(whole(1, 0.3), t0);
        assert_eq!(players.state(t0).volume, 0.3, "the player's own value");
        assert!(players.by_name[P].pending.is_none(), "nothing in flight");

        // A drag: 0.5 refused, 0.7 written while that reread is in
        // flight and refused too.
        let mut players = playing(meta(&[("xesam:title", text("First"))]), t0);
        put(&mut players, "Volume", OwnedValue::from(0.3f64));
        volume(&mut players, 0.5);
        assert!(players.reread(P, ":1.5", "Volume").is_some());
        volume(&mut players, 0.7);
        assert!(players.reread(P, ":1.5", "Volume").is_some());
        players.answer(whole(1, 0.3), t0);
        players.answer(whole(1, 0.3), t0);
        assert_eq!(players.state(t0).volume, 0.3, "the drag settles");

        // A write after the reread was asked stands over its answer.
        let mut players = playing(meta(&[("xesam:title", text("First"))]), t0);
        put(&mut players, "Volume", OwnedValue::from(0.3f64));
        in_flight(&mut players);
        volume(&mut players, 0.6);
        assert!(players.reread(P, ":1.5", "Volume").is_some());
        volume(&mut players, 0.9);
        players.answer(whole(1, 0.3), t0);
        players.answer(whole(1, 0.3), t0);
        assert_eq!(players.state(t0).volume, 0.9, "the newer write holds");
    }

    /// (player-raise-seek) `seek(by)` sends `Seek` with the offset in
    /// microseconds, back when negative, and fixes the time at its target
    /// at once: never below 0, never past a known length, and with no
    /// length known at all (a stream), which a `position` write cannot
    /// seek. A player that cannot seek, or takes no control, is not asked,
    /// nor is one not listed; an offset that is no number is no seek.
    /// `can_seek_by` says the same: true on the stream where `can_seek` is
    /// false, false when the seek would not be asked.
    #[test]
    fn seeking_by_an_offset_moves_from_where_it_is() {
        let t0 = Instant::now();
        let mut players = playing(meta(&[("xesam:title", text("First"))]), t0);
        let seek = players.seek_by(P, -15.0, t0).unwrap();
        assert_eq!(seek.call, SeekCall::Seek(-15_000_000));
        assert_eq!((seek.name.as_str(), seek.owner.as_str()), (P, ":1.5"));
        assert_eq!(seek.asked, players.by_name[P].asked, "a newer fix");
        assert_eq!(players.state(t0).elapsed, Duration::from_secs(105));
        let seek = players.seek_by(P, 30.5, t0).unwrap();
        assert_eq!(seek.call, SeekCall::Seek(30_500_000));
        assert_eq!(players.state(t0).elapsed, Duration::from_millis(135_500));
        // Past either end: the end.
        players.seek_by(P, -500.0, t0).unwrap();
        assert_eq!(players.state(t0).elapsed, Duration::ZERO);
        let seek = players.seek_by(P, 1e6, t0).unwrap();
        assert_eq!(seek.call, SeekCall::Seek(1_000_000_000_000));
        assert_eq!(players.state(t0).elapsed, Duration::from_secs(200));
        // A stream of unknown length: sought all the same.
        let mut players = playing(meta(&[("xesam:title", text("Live"))]), t0);
        let p = players.by_name.get_mut(P).unwrap();
        p.props.insert(
            "Metadata".into(),
            OwnedValue::try_from(zbus::zvariant::Value::from(
                std::collections::HashMap::from([(
                    "xesam:title".to_string(),
                    zbus::zvariant::Value::from("Live"),
                )]),
            ))
            .unwrap(),
        );
        let s = players.state(t0);
        assert!(!s.can_seek, "no position write");
        assert!(s.can_seek_by, "a seek by an offset");
        assert!(players.items(t0)[0].can_seek_by);
        let seek = players.seek_by(P, 15.0, t0).unwrap();
        assert_eq!(seek.call, SeekCall::Seek(15_000_000));
        assert_eq!(players.state(t0).elapsed, Duration::from_secs(135));
        // Not asked.
        assert!(players.seek_by(P, f64::NAN, t0).is_none());
        assert!(
            players
                .seek_by("org.mpris.MediaPlayer2.gone", 5.0, t0)
                .is_none()
        );
        put(&mut players, "CanSeek", flag(false));
        assert!(players.seek_by(P, 5.0, t0).is_none(), "cannot seek");
        assert!(!players.state(t0).can_seek_by && !players.items(t0)[0].can_seek_by);
        put(&mut players, "CanSeek", flag(true));
        put(&mut players, "CanControl", flag(false));
        assert!(players.seek_by(P, 5.0, t0).is_none(), "no control");
        assert!(!players.state(t0).can_seek_by);
        assert_eq!(players.state(t0).elapsed, Duration::from_secs(135));
        assert!(!Players::default().state(t0).can_seek_by, "no player");
    }

    /// (player-raise-seek) The root interface says the player's name,
    /// whether `Raise` does anything (a player that does not say counts
    /// as able; no player cannot) and its desktop entry; an older root
    /// read answering after a newer one asked is dropped, and a root
    /// signal's change shows at once.
    #[test]
    fn the_root_names_the_player_and_whether_it_raises() {
        let t0 = Instant::now();
        assert!(!Players::default().state(t0).can_raise, "no player");
        assert_eq!(Players::default().state(t0).icon, None);
        let mut players = playing(meta(&[("xesam:title", text("First"))]), t0);
        assert!(players.state(t0).can_raise, "absent: able");
        assert_eq!(players.raisable(P.into()).as_deref(), Some(P));
        let root = |entries: &[(&str, OwnedValue)], asked: u64| Read {
            name: P.into(),
            owner: ":1.5".into(),
            asked: 1,
            root: Some((
                asked,
                Ok(entries
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.try_clone().unwrap()))
                    .collect()),
            )),
            props: None,
            position: None,
        };
        let name = |s: &str| OwnedValue::try_from(zbus::zvariant::Value::from(s)).unwrap();
        players.by_name.get_mut(P).unwrap().root_asked = 1;
        players.answer(
            root(
                &[
                    ("Identity", name("Firefox")),
                    ("CanRaise", flag(false)),
                    ("DesktopEntry", name("firefox")),
                ],
                1,
            ),
            t0,
        );
        let s = players.state(t0);
        assert_eq!(s.player.as_deref(), Some("Firefox"));
        assert!(!s.can_raise);
        assert_eq!(players.raisable(P.into()), None, "not asked");
        assert_eq!(s.icon.as_deref(), Some("firefox"), "not looked up yet");
        assert_eq!(players.to_link(false), ["firefox"]);
        // A root signal: shown now, and read again as question 2.
        let ask = players
            .root_changed(P, Props::from([("CanRaise".to_string(), flag(true))]), &[])
            .unwrap();
        assert_eq!(ask.root, Some(2));
        assert!(!ask.props && ask.position.is_none());
        assert!(players.state(t0).can_raise);
        // Question 1's answer, landing late, is dropped; question 2's
        // stands.
        players.answer(root(&[("CanRaise", flag(false))], 1), t0);
        assert!(players.state(t0).can_raise, "a stale root read");
        players.answer(
            root(
                &[
                    ("Identity", name("Firefox")),
                    ("CanRaise", flag(true)),
                    ("DesktopEntry", name("org.mozilla.firefox.desktop")),
                ],
                2,
            ),
            t0,
        );
        let s = players.state(t0);
        assert!(s.can_raise);
        assert_eq!(s.icon.as_deref(), Some("org.mozilla.firefox"));
        assert_eq!(players.to_link(false), ["org.mozilla.firefox"]);
    }

    /// (player-raise-seek) A player's app name is its `DesktopEntry`, else
    /// its bus name without the prefix and an `.instance…` part; its icon
    /// and app are the lookup's answer for that name once it lands (the
    /// name itself until then), on `media` and on its item alike; a name
    /// looked up is not looked up again unless every name is, and the
    /// answers for names no player uses any more are dropped.
    #[test]
    fn a_players_icon_and_app_come_from_its_desktop_entry() {
        let t0 = Instant::now();
        let p = Player::default();
        assert_eq!(p.app_id("org.mpris.MediaPlayer2.vlc"), "vlc");
        assert_eq!(
            p.app_id("org.mpris.MediaPlayer2.chromium.instance12345"),
            "chromium"
        );
        assert_eq!(
            p.app_id("org.mpris.MediaPlayer2.firefox.instance_1_42"),
            "firefox"
        );
        assert_eq!(
            p.app_id("org.mpris.MediaPlayer2.kdeconnect.mpris_x"),
            "kdeconnect.mpris_x"
        );
        let mut players = playing(meta(&[("xesam:title", text("First"))]), t0);
        assert_eq!(players.state(t0).icon.as_deref(), Some("p"));
        assert_eq!(players.to_link(false), ["p"]);
        let app = crate::apps::App {
            id: "p".into(),
            name: "Player P".into(),
            icon: "multimedia-player".into(),
            ..Default::default()
        };
        assert!(
            !players.linked(vec![("p".into(), ("p".into(), None))]),
            "the name itself: nothing changes"
        );
        // An unmatched name (one the theme may have): `icon` is the name
        // and `app` null, so `media.app?.icon ?? "audio-x-generic"` gives
        // it up and `image media.icon` keeps it (player-raise-seek).
        let s = players.state(t0);
        assert_eq!((s.icon.as_deref(), s.app.as_ref()), (Some("p"), None));
        let item = &players.items(t0)[0];
        assert_eq!((item.icon.as_str(), item.app.as_ref()), ("p", None));
        assert!(players.linked(vec![
            ("p".into(), ("multimedia-player".into(), Some(app.clone()))),
            ("gone".into(), ("gone".into(), None)),
        ]));
        let s = players.state(t0);
        assert_eq!(s.icon.as_deref(), Some("multimedia-player"));
        assert_eq!(s.app.as_ref(), Some(&app));
        let item = &players.items(t0)[0];
        assert_eq!(
            (item.icon.as_str(), item.app.as_ref()),
            ("multimedia-player", Some(&app))
        );
        assert!(players.to_link(false).is_empty(), "looked up");
        assert_eq!(players.to_link(true), ["p"], "all again");
        assert!(!players.links.contains_key("gone"), "unused, dropped");
    }
}
