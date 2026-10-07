//! D-Bus introspection of one object's properties: what a no-code
//! service (`service ppd from dbus system "net.hadess.PowerProfiles" {
//! profile: text rw = ActiveProfile }`) is checked against (design.md:
//! "checked against introspection") by `strand check`, `strand run`'s
//! loader and the LSP, and what the running service reads its
//! properties' signatures from.
//!
//! It depends on zbus alone (no service runtime), so the LSP can link it
//! (decisions.md, wave4-a3). The introspection XML is read with a small
//! scanner of its own: only `<interface name>` and `<property name type
//! access>` matter.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long introspecting may take (connecting included) before it is
/// given up: a check must not hang on a bus that does not answer.
pub const TIMEOUT: Duration = Duration::from_secs(2);

/// One property of an object, as its introspection data declares it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Property {
    /// The interface declaring it (`net.hadess.PowerProfiles`).
    pub interface: String,
    /// Its name (`ActiveProfile`).
    pub name: String,
    /// Its D-Bus type signature (`s`, `a{sv}`, `aa{sv}`).
    pub signature: String,
    /// It can be read.
    pub readable: bool,
    /// It can be written (`access="readwrite"` or `"write"`).
    pub writable: bool,
}

/// The object path a bus name's object is at by convention:
/// `net.hadess.PowerProfiles` → `/net/hadess/PowerProfiles`.
pub fn default_path(name: &str) -> String {
    let mut p = String::from("/");
    p.push_str(&name.replace('.', "/").replace('-', "_"));
    p
}

/// The properties `xml` (introspection data) declares, in order.
pub fn parse(xml: &str) -> Vec<Property> {
    let mut out = Vec::new();
    let mut interface: Option<String> = None;
    let mut rest = xml;
    while let Some(i) = rest.find('<') {
        rest = &rest[i + 1..];
        let end = match rest.find('>') {
            Some(e) => e,
            None => break,
        };
        let tag = &rest[..end];
        rest = &rest[end + 1..];
        if tag.starts_with('!') || tag.starts_with('?') {
            continue;
        }
        if let Some(t) = tag.strip_prefix('/') {
            if t.trim() == "interface" {
                interface = None;
            }
            continue;
        }
        let (kind, attrs) = tag.split_once(char::is_whitespace).unwrap_or((tag, ""));
        let kind = kind.trim_end_matches('/');
        match kind {
            "interface" => {
                interface = attr(attrs, "name");
                if tag.trim_end().ends_with('/') {
                    interface = None;
                }
            }
            "property" => {
                let (Some(iface), Some(name), Some(signature)) =
                    (&interface, attr(attrs, "name"), attr(attrs, "type"))
                else {
                    continue;
                };
                let access = attr(attrs, "access").unwrap_or_default();
                out.push(Property {
                    interface: iface.clone(),
                    name,
                    signature,
                    readable: access.contains("read"),
                    writable: access.contains("write"),
                });
            }
            _ => {}
        }
    }
    out
}

/// The value of attribute `key` in a tag's attribute text, entities
/// decoded.
fn attr(attrs: &str, key: &str) -> Option<String> {
    let mut rest = attrs;
    loop {
        let eq = rest.find('=')?;
        let name = rest[..eq].trim();
        let after = rest[eq + 1..].trim_start();
        let quote = after.chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        let body = &after[1..];
        let close = body.find(quote)?;
        let value = &body[..close];
        if name == key {
            return Some(
                value
                    .replace("&lt;", "<")
                    .replace("&gt;", ">")
                    .replace("&quot;", "\"")
                    .replace("&apos;", "'")
                    .replace("&amp;", "&"),
            );
        }
        rest = &body[close + 1..];
    }
}

/// Introspect `path` of `name` on `conn`: its properties.
pub async fn properties_on(
    conn: &zbus::Connection,
    name: &str,
    path: &str,
) -> Result<Vec<Property>, String> {
    let reply = conn
        .call_method(
            Some(name),
            path,
            Some("org.freedesktop.DBus.Introspectable"),
            "Introspect",
            &(),
        )
        .await
        .map_err(|e| e.to_string())?;
    let xml: String = reply.body().deserialize().map_err(|e| e.to_string())?;
    Ok(parse(&xml))
}

/// Which bus.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Bus {
    /// The environment's system bus (`DBUS_SYSTEM_BUS_ADDRESS`, else the
    /// system socket).
    System,
    /// The environment's session bus (`DBUS_SESSION_BUS_ADDRESS`).
    Session,
    /// An explicit address.
    Address(String),
}

/// Introspect `path` of `name` on `bus`, blocking (a runtime of its own),
/// at most [`TIMEOUT`]. For callers without a tokio runtime: `strand
/// check`, the loader, the LSP.
pub fn properties(bus: &Bus, name: &str, path: &str) -> Result<Vec<Property>, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async {
        let work = async {
            let conn = match bus {
                Bus::System => zbus::Connection::system().await,
                Bus::Session => zbus::Connection::session().await,
                Bus::Address(a) => match zbus::connection::Builder::address(a.as_str()) {
                    Ok(b) => b.build().await,
                    Err(e) => Err(e),
                },
            }
            .map_err(|e| format!("cannot reach the bus: {e}"))?;
            properties_on(&conn, name, path).await
        };
        match tokio::time::timeout(TIMEOUT, work).await {
            Ok(r) => r,
            Err(_) => Err(format!("no answer within {TIMEOUT:?}")),
        }
    })
}

/// What introspecting one object answered: its properties, or why they
/// could not be read.
pub type Answer = Result<Vec<Property>, String>;

type Fetch = dyn Fn(&Bus, &str, &str) -> Answer + Send + Sync;

/// Bus, name and path.
type Key = (Bus, String, String);

#[derive(Default)]
struct Entry {
    /// When it was last answered, and the answer.
    known: Option<(Instant, Answer)>,
    /// A background ask ([`Cache::properties_or_ask`]) is under way.
    asking: bool,
}

/// [`properties`] answers, failures included, remembered for a while: a
/// reload burst or an LSP's keystrokes ask the bus once per object, and a
/// daemon that does not answer costs one bounded wait per `ttl`. `strand
/// check`, the loader and the LSP share it.
pub struct Cache {
    ttl: Duration,
    fetch: Box<Fetch>,
    seen: Mutex<HashMap<Key, Entry>>,
}

impl std::fmt::Debug for Cache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cache").field("ttl", &self.ttl).finish()
    }
}

/// How long [`Cache::new`]'s answers are reused.
pub const TTL: Duration = Duration::from_secs(10);

impl Default for Cache {
    fn default() -> Self {
        Self::new(TTL)
    }
}

impl Cache {
    /// Introspection on the buses ([`properties`]), answers reused `ttl`.
    pub fn new(ttl: Duration) -> Self {
        Self::with_fetch(ttl, properties)
    }

    /// The same over another way of asking (tests).
    pub fn with_fetch(
        ttl: Duration,
        fetch: impl Fn(&Bus, &str, &str) -> Answer + Send + Sync + 'static,
    ) -> Self {
        Self {
            ttl,
            fetch: Box::new(fetch),
            seen: Mutex::new(HashMap::new()),
        }
    }

    fn key(bus: &Bus, name: &str, path: &str) -> Key {
        (bus.clone(), name.to_string(), path.to_string())
    }

    fn remember(&self, key: Key, answer: &Answer) -> bool {
        let Ok(mut seen) = self.seen.lock() else {
            return true;
        };
        let e = seen.entry(key).or_default();
        let changed = e.known.as_ref().is_none_or(|(_, a)| a != answer);
        e.known = Some((Instant::now(), answer.clone()));
        e.asking = false;
        changed
    }

    /// `path`'s properties, blocking at most [`TIMEOUT`] when the answer
    /// is not remembered (or is older than the ttl).
    pub fn properties(&self, bus: &Bus, name: &str, path: &str) -> Answer {
        let key = Self::key(bus, name, path);
        if let Ok(seen) = self.seen.lock()
            && let Some((at, answer)) = seen.get(&key).and_then(|e| e.known.as_ref())
            && at.elapsed() < self.ttl
        {
            return answer.clone();
        }
        let answer = (self.fetch)(bus, name, path);
        self.remember(key, &answer);
        answer
    }

    /// `path`'s properties without waiting on the bus: the remembered
    /// answer (even one past the ttl, which is asked again meanwhile), or
    /// `None` when nothing was ever answered. A question this call starts
    /// runs on a thread of its own and calls `done` when its answer
    /// differs from what was remembered (the caller checks again). For
    /// the LSP, whose analyses must not wait on a daemon.
    pub fn properties_or_ask(
        self: &Arc<Self>,
        bus: &Bus,
        name: &str,
        path: &str,
        done: impl FnOnce() + Send + 'static,
    ) -> Option<Answer> {
        let key = Self::key(bus, name, path);
        let remembered = {
            let Ok(mut seen) = self.seen.lock() else {
                return Some((self.fetch)(bus, name, path));
            };
            let e = seen.entry(key.clone()).or_default();
            let remembered = e
                .known
                .as_ref()
                .map(|(at, a)| (at.elapsed() < self.ttl, a.clone()));
            if let Some((true, answer)) = remembered {
                return Some(answer);
            }
            if e.asking {
                return remembered.map(|(_, a)| a);
            }
            e.asking = true;
            remembered.map(|(_, a)| a)
        };
        let me = self.clone();
        let (b, n, p) = key.clone();
        let spawned = std::thread::Builder::new()
            .name("strand-introspect".into())
            .spawn(move || {
                let answer = (me.fetch)(&b, &n, &p);
                if me.remember((b, n, p), &answer) {
                    done();
                }
            });
        if spawned.is_err() {
            // No thread to ask on: ask here.
            let answer = (self.fetch)(bus, name, path);
            self.remember(key, &answer);
            return Some(answer);
        }
        remembered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cache asks once per ttl, blocking or not; a non-blocking
    /// question is answered on its own thread and `done` says so only
    /// when the answer changed.
    #[test]
    fn cached_answers_are_reused_and_asked_off_the_caller() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let asked = Arc::new(AtomicUsize::new(0));
        let a = asked.clone();
        let cache = Arc::new(Cache::with_fetch(
            Duration::from_millis(200),
            move |_, _, _| {
                a.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(50));
                Err("down".to_string())
            },
        ));
        let bus = Bus::Session;
        assert_eq!(cache.properties(&bus, "a.B", "/a"), Err("down".into()));
        assert_eq!(cache.properties(&bus, "a.B", "/a"), Err("down".into()));
        assert_eq!(asked.load(Ordering::SeqCst), 1);
        // Another object, not blocking: nothing known yet, asked on a
        // thread; the caller is not held up by the 50 ms answer.
        let (tx, rx) = std::sync::mpsc::channel();
        let t = Instant::now();
        let tx1 = tx.clone();
        assert_eq!(
            cache.properties_or_ask(&bus, "c.D", "/c", move || {
                let _ = tx1.send(());
            }),
            None
        );
        assert!(t.elapsed() < Duration::from_millis(40));
        // Asked once while the first question is out.
        assert_eq!(cache.properties_or_ask(&bus, "c.D", "/c", || {}), None);
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(asked.load(Ordering::SeqCst), 2);
        assert_eq!(
            cache.properties_or_ask(&bus, "c.D", "/c", || {}),
            Some(Err("down".into()))
        );
        // Past the ttl: the old answer now, asked again behind it; the
        // same answer again is no news (no `done`).
        std::thread::sleep(Duration::from_millis(250));
        let tx2 = tx.clone();
        assert_eq!(
            cache.properties_or_ask(&bus, "c.D", "/c", move || {
                let _ = tx2.send(());
            }),
            Some(Err("down".into()))
        );
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
        assert_eq!(asked.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn introspection_xml_gives_properties_by_interface() {
        let xml = r#"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">
<node>
  <interface name="org.freedesktop.DBus.Properties">
    <method name="Get"><arg type="s" direction="in"/></method>
  </interface>
  <interface name="net.hadess.PowerProfiles">
    <property type="s" name="ActiveProfile" access="readwrite">
      <annotation name="x" value="y"/>
    </property>
    <property name='Profiles' type='aa{sv}' access='read'/>
    <property name="Secret" type="s" access="write"/>
    <signal name="Changed"/>
  </interface>
  <interface name="empty.Thing"/>
  <node name="child"/>
</node>"#;
        let p = parse(xml);
        assert_eq!(p.len(), 3);
        assert_eq!(
            p[0],
            Property {
                interface: "net.hadess.PowerProfiles".into(),
                name: "ActiveProfile".into(),
                signature: "s".into(),
                readable: true,
                writable: true,
            }
        );
        assert_eq!(p[1].signature, "aa{sv}");
        assert!(p[1].readable && !p[1].writable);
        assert!(!p[2].readable && p[2].writable);
        assert_eq!(
            default_path("net.hadess.PowerProfiles"),
            "/net/hadess/PowerProfiles"
        );
        assert_eq!(attr(r#"a="&lt;x&gt;""#, "a").as_deref(), Some("<x>"));
    }
}
