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
//! `Metadata` is judged when the re-read lands. `elapsed` and `position`
//! are `#[store(stream)]` fields, ticking once a second only while a visible
//! reader shows them and the player plays. Otherwise nothing wakes.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use futures_lite::StreamExt;
use zbus::MatchRule;
use zbus::message::Type as MessageType;
use zbus::zvariant::OwnedValue;

use crate::dbus::{self, Props};
use crate::{Call, Cx, Msg, ServiceError, Store, service};

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
    /// `media.play_pause()`.
    PlayPause,
    /// `media.next()`.
    Next,
    /// `media.previous()`.
    Previous,
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
    /// The album art: `image media.art { fit: cover }`. Local art only (a
    /// `file://` URL or a path); null when the player sent none or a remote
    /// URL, so `media.art ?? "audio-x-generic"` falls back.
    pub art: Option<String>,
    /// Fraction of the track played, 0 to 1.
    #[store(stream)]
    pub position: f64,
    /// Time played.
    #[store(stream)]
    pub elapsed: Duration,
    /// The track's length, when known.
    pub length: Option<Duration>,
    /// The active player's name, such as `Firefox` or `mpv`; null when no
    /// player runs.
    pub player: Option<String>,
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
    identity: Option<String>,
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
    /// the change corrects it). Whether it was another track.
    fn note_track(&mut self, now: Instant) -> bool {
        if !self.props.contains_key("Metadata") {
            return false;
        }
        let track = Track::of(&self.metadata());
        let other = self.track.as_ref().is_some_and(|t| !t.same(&track));
        if other {
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
}

impl Players {
    fn active(&self) -> Option<&Player> {
        let known = || self.by_name.values().filter(|p| p.known);
        let playing = known().filter(|p| p.playing()).max_by_key(|p| p.touched);
        playing.or_else(|| known().max_by_key(|p| (p.status() == "Paused", p.touched)))
    }

    fn active_name(&self) -> Option<String> {
        let a = self.active()?;
        self.by_name
            .iter()
            .find(|(_, p)| std::ptr::eq(*p, a))
            .map(|(n, _)| n.clone())
    }

    fn state(&self, now: Instant) -> Media {
        let Some(p) = self.active() else {
            return Media::default();
        };
        let meta = p.metadata();
        let length = meta.get("mpris:length").and_then(micros);
        let elapsed = p.elapsed(now, length);
        let text = |k: &str| dbus::text(&meta, k).filter(|s| !s.is_empty());
        Media {
            playing: p.playing(),
            title: text("xesam:title"),
            artist: artist(&meta),
            album: text("xesam:album"),
            art: text("mpris:artUrl").filter(|u| local_art(u)),
            position: match length {
                Some(l) if !l.is_zero() => {
                    (elapsed.as_secs_f64() / l.as_secs_f64()).clamp(0.0, 1.0)
                }
                _ => 0.0,
            },
            elapsed,
            length,
            player: p.identity.clone(),
        }
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
    /// unless the read's own position answer, current, says where.
    fn answer(&mut self, r: Read, now: Instant) -> bool {
        let Some(p) = self.by_name.get_mut(&r.name) else {
            return false;
        };
        if p.owner != r.owner {
            return false;
        }
        if let Some(identity) = r.identity {
            p.identity = identity;
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
            p.note_track(now);
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

/// Art `image` can show: a `file://` URL or a path. Remote art (the
/// `https://` URLs some players send) is left out, so a shell's fallback
/// (`media.art ?? "audio-x-generic"`) shows instead of a blank picture
/// (decisions.md, wave4-a2).
fn local_art(url: &str) -> bool {
    url.starts_with("file://") || url.starts_with('/')
}

/// A player read in a task of the body's own (so a player that does not
/// answer holds up nothing but itself): what was asked, answered.
#[derive(Debug)]
struct Read {
    name: String,
    owner: String,
    identity: Option<Option<String>>,
    props: Option<zbus::Result<Props>>,
    /// The position question's number, its answer and when it came.
    position: Option<(u64, Option<Duration>, Instant)>,
}

/// What to ask a player.
#[derive(Debug, Clone, Copy, Default)]
struct Ask {
    identity: bool,
    props: bool,
    position: Option<u64>,
}

/// Ask `name` (owned by `owner`) what `ask` says, each call bounded by
/// [`dbus::CALL_TIMEOUT`] and all at once.
async fn read(conn: zbus::Connection, name: String, owner: String, ask: Ask) -> Read {
    let identity = async {
        if !ask.identity {
            return None;
        }
        Some(
            dbus::timed(dbus::get(&conn, &name, PATH, ROOT_IFACE, "Identity"))
                .await
                .ok()
                .and_then(|v| v.downcast_ref::<&str>().ok().map(str::to_string)),
        )
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
    let (identity, props, position) = tokio::join!(identity, props, position);
    Read {
        name,
        owner,
        identity,
        props,
        position,
    }
}

/// The body's tasks: reads (answers to apply) and actions (nothing).
type Tasks = tokio::task::JoinSet<Option<Read>>;

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
        pending: Some(Props::new()),
        reading: 1,
        asked: 1,
        ..Player::default()
    };
    players.by_name.insert(name.clone(), p);
    let ask = Ask {
        identity: true,
        props: true,
        position: Some(1),
    };
    let conn = conn.clone();
    tasks.spawn(async move { Some(read(conn, name, owner, ask).await) });
}

/// When the shown second next changes (while playing).
fn next_tick(players: &Players, now: Instant) -> Option<Instant> {
    let p = players.active()?;
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
        if !cx.update(|s| *s = players.state(Instant::now())) {
            return Ok(());
        }
        cx.ready();
        loop {
            let ticking = cx.watched("elapsed") || cx.watched("position");
            let tick = if ticking {
                next_tick(&players, Instant::now())
            } else {
                None
            };
            let changed = tokio::select! {
                Some(done) = tasks.join_next(), if !tasks.is_empty() => match done {
                    Ok(Some(r)) => players.answer(r, Instant::now()),
                    _ => false,
                },
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
                        let method = match a {
                            MediaAction::PlayPause => "PlayPause",
                            MediaAction::Next => "Next",
                            MediaAction::Previous => "Previous",
                        };
                        // A task of its own: a player that does not answer
                        // holds up nothing (dropped with the body).
                        if let Some(name) = players.active_name() {
                            let conn = conn.clone();
                            tasks.spawn(async move {
                                let r = dbus::timed(conn.call_method(Some(name.as_str()), PATH, Some(PLAYER), method, &())).await;
                                if let Err(e) = r {
                                    log::warn!("media: {method} on {name}: {e}");
                                }
                                None
                            });
                        }
                        false
                    }
                    // Watching starts or stops the ticks (next turn).
                    Some(_) => true,
                },
            };
            if changed && !cx.update(|s| *s = players.state(Instant::now())) {
                return Ok(());
            }
        }
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
        p.asked += 1;
        p.at = Some((Duration::from_micros(pos.max(0) as u64), Instant::now()));
        return true;
    }
    let owner = p.owner.clone();
    let Some(c) = dbus::properties_changed(m) else {
        return false;
    };
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
            p.note_track(now);
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
        if status_moved {
            self.touch(name);
        }
        ask
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            identity: None,
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
    fn only_local_art_is_shown() {
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
        p.note_track(t0);
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
            identity: None,
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
        let whole = |m: OwnedValue, position| Read {
            name: P.into(),
            owner: ":1.5".into(),
            identity: None,
            props: Some(Ok(Props::from([
                (
                    "PlaybackStatus".to_string(),
                    OwnedValue::try_from(zbus::zvariant::Value::from("Playing")).unwrap(),
                ),
                ("Metadata".to_string(), m),
            ]))),
            position,
        };
        let invalidate = |players: &mut Players| {
            let ask = players.changed(P, Props::new(), &["Metadata".to_string()], t1);
            assert!(ask.props);
            assert_eq!(ask.position, Some(2));
        };

        // Another track; the read's position question was overtaken (a
        // newer one is in flight): 0 from when it landed.
        let mut players = playing(a(), t0);
        invalidate(&mut players);
        players.by_name.get_mut(P).unwrap().asked = 3;
        let t2 = t1 + Duration::from_secs(1);
        assert!(players.answer(whole(b(), Some((2, Some(Duration::from_secs(9)), t2))), t2));
        let s = players.state(t2);
        assert_eq!(s.title.as_deref(), Some("Second"));
        assert_eq!(s.elapsed, Duration::ZERO);

        // Another track with its current position answer: that stands.
        let mut players = playing(a(), t0);
        invalidate(&mut players);
        let answer = Some((2, Some(Duration::from_millis(300)), t2));
        assert!(players.answer(whole(b(), answer), t2));
        assert_eq!(players.state(t2).elapsed, Duration::from_millis(300));

        // The same track re-read: the carried time stands.
        let mut players = playing(a(), t0);
        invalidate(&mut players);
        players.by_name.get_mut(P).unwrap().asked = 3;
        assert!(players.answer(whole(a(), None), t2));
        assert_eq!(players.state(t2).elapsed, Duration::from_secs(122));
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
            identity: None,
            props: None,
            position: Some((ask.position.unwrap(), Some(Duration::from_secs(3)), t2)),
        };
        assert!(players.answer(pos, t2));
        let stale = Read {
            name: P.into(),
            owner: ":1.5".into(),
            identity: None,
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
}
