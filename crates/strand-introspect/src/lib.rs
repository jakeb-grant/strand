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

use std::time::Duration;

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
#[derive(Clone, Debug, PartialEq, Eq)]
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

#[cfg(test)]
mod tests {
    use super::*;

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
