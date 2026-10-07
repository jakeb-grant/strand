//! `media`: the active MPRIS player on the session bus, with our own zbus
//! client.
//!
//! Every `org.mpris.MediaPlayer2.*` name is followed (`NameOwnerChanged`
//! for the namespace, the players' `PropertiesChanged` and `Seeked` at
//! `/org/mpris/MediaPlayer2`). The active player is the one playing that
//! started playing last, else the one that paused last, else the first
//! by name. Its position is asked for (`Position`, which players do not
//! signal) only when its state, track or rate changes or it seeks, and
//! carried forward at its rate from there; `elapsed` and `position` are
//! `#[store(stream)]` fields, ticking once a second only while a visible
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
    /// The album art: `image media.art { fit: cover }`.
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
            .unwrap_or(1.0)
    }

    fn metadata(&self) -> Props {
        self.props
            .get("Metadata")
            .and_then(|v| v.try_clone().ok())
            .and_then(|v| Props::try_from(v).ok())
            .unwrap_or_default()
    }

    /// The position now, carried forward while playing.
    fn elapsed(&self, now: Instant, length: Option<Duration>) -> Duration {
        let Some((base, at)) = self.at else {
            return Duration::ZERO;
        };
        let mut e = base;
        if self.playing() {
            e += now.saturating_duration_since(at).mul_f64(self.rate());
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
        let playing = self
            .by_name
            .values()
            .filter(|p| p.playing())
            .max_by_key(|p| p.touched);
        playing.or_else(|| {
            self.by_name
                .values()
                .max_by_key(|p| (p.status() == "Paused", p.touched))
        })
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
        let artist = meta
            .get("xesam:artist")
            .and_then(|v| v.try_clone().ok())
            .and_then(|v| Vec::<String>::try_from(v).ok())
            .map(|a| a.join(", "))
            .filter(|s| !s.is_empty())
            .or_else(|| text("xesam:artist"));
        Media {
            playing: p.playing(),
            title: text("xesam:title"),
            artist,
            album: text("xesam:album"),
            art: text("mpris:artUrl"),
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
}

/// Ask a player where it is.
async fn ask_position(conn: &zbus::Connection, name: &str, p: &mut Player) {
    let pos = dbus::get(conn, name, PATH, PLAYER, "Position")
        .await
        .ok()
        .and_then(|v| micros(&v));
    p.at = Some((pos.unwrap_or_default(), Instant::now()));
}

/// Read one player.
async fn read_player(conn: &zbus::Connection, name: &str, owner: String) -> Player {
    let mut p = Player {
        owner,
        identity: dbus::get(conn, name, PATH, ROOT_IFACE, "Identity")
            .await
            .ok()
            .and_then(|v| v.downcast_ref::<&str>().ok().map(str::to_string)),
        props: dbus::get_all(conn, name, PATH, PLAYER)
            .await
            .unwrap_or_default(),
        ..Player::default()
    };
    ask_position(conn, name, &mut p).await;
    p
}

/// When the shown second next changes (while playing).
fn next_tick(players: &Players, now: Instant) -> Option<Instant> {
    let p = players.active()?;
    if !p.playing() {
        return None;
    }
    let e = p.elapsed(now, None);
    let into = Duration::from_nanos((e.as_nanos() % 1_000_000_000) as u64);
    let wait = (Duration::from_secs(1) - into).div_f64(p.rate());
    Some(now + wait.max(Duration::from_millis(5)))
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
            let p = read_player(&conn, &name, owner.to_string()).await;
            players.by_name.insert(name.clone(), p);
            players.touch(&name);
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
                        let p = read_player(&conn, &name, new).await;
                        players.by_name.insert(name.clone(), p);
                        players.touch(&name);
                    }
                    true
                }
                m = signals.next() => {
                    let Some(Ok(m)) = m else {
                        return Err(ServiceError("the session bus connection ended".into()));
                    };
                    signal(&conn, &mut players, &m).await
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
                        if let Some(name) = players.active_name() {
                            let r = conn
                                .call_method(Some(name.as_str()), PATH, Some(PLAYER), method, &())
                                .await;
                            if let Err(e) = r {
                                log::warn!("media: {method} on {name}: {e}");
                            }
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

/// Apply a player's signal; whether anything changed.
async fn signal(conn: &zbus::Connection, players: &mut Players, m: &zbus::Message) -> bool {
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
        p.at = Some((Duration::from_micros(pos.max(0) as u64), Instant::now()));
        return true;
    }
    let Some(c) = dbus::properties_changed(m) else {
        return false;
    };
    if c.iface != PLAYER {
        return false;
    }
    // Carry the position forward to now before the state changes, then
    // ask again where the state, track or rate moved it.
    let now = Instant::now();
    let length = p.metadata().get("mpris:length").and_then(micros);
    let here = p.elapsed(now, length);
    p.at = Some((here, now));
    let status_moved = c.changed.contains_key("PlaybackStatus");
    let ask = status_moved
        || c.changed.contains_key("Metadata")
        || c.changed.contains_key("Rate")
        || c.invalidated
            .iter()
            .any(|i| i == "Metadata" || i == "PlaybackStatus");
    dbus::apply_changed(conn, &name, &mut p.props, c).await;
    if ask {
        ask_position(conn, &name, p).await;
    }
    if status_moved {
        players.touch(&name);
    }
    true
}
