//! `network.address`: an interface's IP address, from the kernel over
//! rtnetlink (decisions.md, m6-service-gaps).
//!
//! Neither backend knows it in every setup: iwd does not under
//! systemd-networkd or dhcpcd (and does not publish it even with its own
//! DHCP), and NetworkManager knows only the links it manages. Every
//! network stack writes the address into the kernel, so one reader serves
//! both backends and any DHCP client. A [`Follower`] is a `NETLINK_ROUTE`
//! socket (rustix's safe `net` calls) bound to the IPv4 and IPv6 address
//! groups: it dumps the addresses once (`RTM_GETADDR`), then hears each
//! `RTM_NEWADDR` and `RTM_DELADDR`; nothing is polled, and a body opens
//! one only while a visible reader reads `address`.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::fd::{AsFd, OwnedFd};

use rustix::io::Errno;
use rustix::net::netlink::SocketAddrNetlink;
use rustix::net::{AddressFamily, RecvFlags, SendFlags, SocketFlags, SocketType};
use tokio::io::unix::AsyncFd;

/// `nlmsghdr`'s length.
const HEADER: usize = 16;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const RTM_NEWADDR: u16 = 20;
const RTM_DELADDR: u16 = 21;
const RTM_GETADDR: u16 = 22;
const NLM_F_REQUEST: u16 = 0x1;
/// `NLM_F_ROOT | NLM_F_MATCH`.
const NLM_F_DUMP: u16 = 0x300;
/// `ifaddrmsg`'s length.
const IFADDRMSG: usize = 8;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFA_FLAGS: u16 = 8;
const AF_UNSPEC: u8 = 0;
const AF_INET: u8 = 2;
const AF_INET6: u8 = 10;
/// The multicast groups of address changes.
const RTMGRP_IPV4_IFADDR: u32 = 0x10;
const RTMGRP_IPV6_IFADDR: u32 = 0x100;
/// `IFA_F_SECONDARY` (IPv4) and `IFA_F_TEMPORARY` (IPv6) share a bit.
const IFA_F_SECONDARY: u32 = 0x01;
const IFA_F_DADFAILED: u32 = 0x08;
const IFA_F_DEPRECATED: u32 = 0x20;
const IFA_F_TENTATIVE: u32 = 0x40;
const RT_SCOPE_UNIVERSE: u8 = 0;

/// An address the kernel gave an interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Addr {
    ip: IpAddr,
    scope: u8,
    flags: u32,
}

/// The kernel's addresses, by interface index and address, as its
/// messages say.
#[derive(Debug, Default)]
pub struct Addresses {
    by_index: BTreeMap<(u32, IpAddr), Addr>,
    /// A dump is complete (`NLMSG_DONE` seen).
    done: bool,
}

/// A native-endian `u16` at `at`.
fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_ne_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

/// A native-endian `u32` at `at`.
fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// `n` rounded up to netlink's 4-byte alignment.
fn align(n: usize) -> usize {
    (n + 3) & !3
}

impl Addresses {
    /// Apply the netlink messages in `buf` (one datagram): new and removed
    /// addresses, a dump's end. Whether an address came or went. A
    /// malformed message ends the datagram, read no further.
    pub fn apply(&mut self, buf: &[u8]) -> bool {
        let mut changed = false;
        let mut at = 0;
        while let (Some(len), Some(kind)) = (u32_at(buf, at), u16_at(buf, at + 4)) {
            let len = len as usize;
            if len < HEADER || at + len > buf.len() {
                break;
            }
            let body = &buf[at + HEADER..at + len];
            match kind {
                NLMSG_DONE | NLMSG_ERROR => self.done = true,
                RTM_NEWADDR | RTM_DELADDR => {
                    if let Some((index, addr)) = parse_addr(body) {
                        let key = (index, addr.ip);
                        changed |= if kind == RTM_NEWADDR {
                            self.by_index.insert(key, addr) != Some(addr)
                        } else {
                            self.by_index.remove(&key).is_some()
                        };
                    }
                }
                _ => {}
            }
            at += align(len);
        }
        changed
    }

    /// Forget every address (a dump follows).
    fn clear(&mut self) {
        self.by_index.clear();
        self.done = false;
    }

    /// The address interface `index` is known by: its IPv4 address (one
    /// of universe scope first, a primary one first), else a global IPv6
    /// address that is usable (not tentative, failed or deprecated), a
    /// stable one before a temporary one. Never a link-local one.
    pub fn best(&self, index: u32) -> Option<IpAddr> {
        let on = || {
            self.by_index
                .range((index, IpAddr::V4(Ipv4Addr::UNSPECIFIED))..)
                .take_while(move |((i, _), _)| *i == index)
                .map(|(_, a)| *a)
        };
        let v4 = on().filter(|a| a.ip.is_ipv4()).min_by_key(|a| {
            (
                a.scope != RT_SCOPE_UNIVERSE,
                a.flags & IFA_F_SECONDARY != 0,
                a.ip,
            )
        });
        if let Some(a) = v4 {
            return Some(a.ip);
        }
        on().filter(|a| a.ip.is_ipv6() && a.scope == RT_SCOPE_UNIVERSE)
            .filter(|a| a.flags & (IFA_F_TENTATIVE | IFA_F_DADFAILED | IFA_F_DEPRECATED) == 0)
            .min_by_key(|a| (a.flags & IFA_F_SECONDARY != 0, a.ip))
            .map(|a| a.ip)
    }
}

/// An `ifaddrmsg` and its attributes: the interface index and address.
/// IPv4's own address is `IFA_LOCAL` (`IFA_ADDRESS` is the peer's on a
/// point-to-point link), IPv6's `IFA_ADDRESS`; `IFA_FLAGS` holds the
/// flags that do not fit the header's byte.
fn parse_addr(body: &[u8]) -> Option<(u32, Addr)> {
    let family = *body.first()?;
    let mut flags = u32::from(*body.get(2)?);
    let scope = *body.get(3)?;
    let index = u32_at(body, 4)?;
    let (mut address, mut local) = (None, None);
    let mut at = IFADDRMSG;
    while let (Some(len), Some(kind)) = (u16_at(body, at), u16_at(body, at + 2)) {
        let len = usize::from(len);
        if len < 4 || at + len > body.len() {
            break;
        }
        let data = &body[at + 4..at + len];
        let ip = match (family, data.len()) {
            (AF_INET, 4) => <[u8; 4]>::try_from(data)
                .ok()
                .map(|b| IpAddr::V4(Ipv4Addr::from(b))),
            (AF_INET6, 16) => <[u8; 16]>::try_from(data)
                .ok()
                .map(|b| IpAddr::V6(Ipv6Addr::from(b))),
            _ => None,
        };
        match kind {
            IFA_ADDRESS => address = ip.or(address),
            IFA_LOCAL => local = ip.or(local),
            IFA_FLAGS => flags = u32_at(data, 0).unwrap_or(flags),
            _ => {}
        }
        at += align(len);
    }
    let ip = if family == AF_INET {
        local.or(address)
    } else {
        address.or(local)
    }?;
    Some((index, Addr { ip, scope, flags }))
}

/// An interface's address, followed: a netlink socket of its own.
#[derive(Debug)]
pub struct Follower {
    fd: AsyncFd<OwnedFd>,
    addresses: Addresses,
    /// The value last given ([`Follower::changed`]).
    last: Option<String>,
    seq: u32,
}

impl Follower {
    /// A socket bound to the address groups, its dump asked (on the
    /// tokio runtime of the caller).
    pub fn open() -> std::io::Result<Follower> {
        let fd = rustix::net::socket_with(
            AddressFamily::NETLINK,
            SocketType::RAW,
            SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
            // NETLINK_ROUTE.
            None,
        )?;
        rustix::net::bind(
            &fd,
            &SocketAddrNetlink::new(0, RTMGRP_IPV4_IFADDR | RTMGRP_IPV6_IFADDR),
        )?;
        let mut f = Follower {
            fd: AsyncFd::new(fd)?,
            addresses: Addresses::default(),
            last: None,
            seq: 0,
        };
        f.dump()?;
        Ok(f)
    }

    /// Ask the kernel for every address.
    fn dump(&mut self) -> std::io::Result<()> {
        self.addresses.clear();
        self.seq = self.seq.wrapping_add(1);
        let mut msg = Vec::with_capacity(HEADER + IFADDRMSG);
        msg.extend_from_slice(&((HEADER + IFADDRMSG) as u32).to_ne_bytes());
        msg.extend_from_slice(&RTM_GETADDR.to_ne_bytes());
        msg.extend_from_slice(&(NLM_F_REQUEST | NLM_F_DUMP).to_ne_bytes());
        msg.extend_from_slice(&self.seq.to_ne_bytes());
        msg.extend_from_slice(&0u32.to_ne_bytes());
        // An `ifaddrmsg` of every family.
        msg.extend_from_slice(&[AF_UNSPEC, 0, 0, 0, 0, 0, 0, 0]);
        rustix::net::sendto(
            self.fd.get_ref(),
            &msg,
            SendFlags::empty(),
            &SocketAddrNetlink::new(0, 0),
        )?;
        Ok(())
    }

    /// The address of interface `iface` (its name), now.
    fn best_of(&self, iface: Option<&str>) -> Option<String> {
        let index =
            rustix::net::netdevice::name_to_index(self.fd.get_ref().as_fd(), iface?).ok()?;
        self.addresses.best(index).map(|ip| ip.to_string())
    }

    /// The address of `iface` once it differs from the one last given
    /// (the first call: once the dump is read). A kernel that dropped
    /// messages (`ENOBUFS`) is asked for every address again.
    pub async fn changed(&mut self, iface: Option<&str>) -> std::io::Result<Option<String>> {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            if self.addresses.done {
                let now = self.best_of(iface);
                if now != self.last {
                    self.last = now.clone();
                    return Ok(now);
                }
            }
            let mut guard = self.fd.readable().await?;
            let read = guard.try_io(|fd| {
                rustix::net::recv(fd.get_ref(), &mut buf[..], RecvFlags::empty())
                    .map(|(_, n)| n)
                    .map_err(std::io::Error::from)
            });
            match read {
                Ok(Ok(n)) => {
                    self.addresses.apply(&buf[..n.min(buf.len())]);
                }
                Ok(Err(e)) if e.raw_os_error() == Some(Errno::NOBUFS.raw_os_error()) => {
                    log::debug!("network: address messages were dropped; reading them again");
                    self.dump()?;
                }
                Ok(Err(e)) => return Err(e),
                // Not readable after all.
                Err(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One `RTM_NEWADDR` or `RTM_DELADDR` message: `ifaddrmsg` and its
    /// attributes.
    fn msg(kind: u16, family: u8, scope: u8, index: u32, attrs: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut body = vec![family, 24, 0, scope];
        body.extend_from_slice(&index.to_ne_bytes());
        for (t, data) in attrs {
            body.extend_from_slice(&((4 + data.len()) as u16).to_ne_bytes());
            body.extend_from_slice(&t.to_ne_bytes());
            body.extend_from_slice(data);
            while body.len() % 4 != 0 {
                body.push(0);
            }
        }
        let mut m = Vec::new();
        m.extend_from_slice(&((HEADER + body.len()) as u32).to_ne_bytes());
        m.extend_from_slice(&kind.to_ne_bytes());
        m.extend_from_slice(&0u16.to_ne_bytes());
        m.extend_from_slice(&1u32.to_ne_bytes());
        m.extend_from_slice(&0u32.to_ne_bytes());
        m.extend_from_slice(&body);
        m
    }

    fn v4(kind: u16, index: u32, ip: [u8; 4], scope: u8) -> Vec<u8> {
        msg(
            kind,
            AF_INET,
            scope,
            index,
            &[(IFA_ADDRESS, ip.to_vec()), (IFA_LOCAL, ip.to_vec())],
        )
    }

    fn v6(index: u32, ip: &str, scope: u8, flags: u32) -> Vec<u8> {
        let ip: Ipv6Addr = ip.parse().unwrap();
        msg(
            RTM_NEWADDR,
            AF_INET6,
            scope,
            index,
            &[
                (IFA_ADDRESS, ip.octets().to_vec()),
                (IFA_FLAGS, flags.to_ne_bytes().to_vec()),
            ],
        )
    }

    fn done() -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(&20u32.to_ne_bytes());
        m.extend_from_slice(&NLMSG_DONE.to_ne_bytes());
        // Flags, sequence, port and the error code (0).
        m.extend_from_slice(&[0; 14]);
        m
    }

    fn best(a: &Addresses, index: u32) -> Option<String> {
        a.best(index).map(|ip| ip.to_string())
    }

    #[test]
    fn new_and_removed_addresses_are_followed() {
        let mut a = Addresses::default();
        assert!(a.apply(&v4(RTM_NEWADDR, 3, [192, 168, 1, 23], 0)));
        assert!(
            !a.apply(&v4(RTM_NEWADDR, 3, [192, 168, 1, 23], 0)),
            "no change"
        );
        assert_eq!(best(&a, 3).as_deref(), Some("192.168.1.23"));
        assert_eq!(best(&a, 4), None, "another interface");
        // A point-to-point link: its own address is IFA_LOCAL.
        a.apply(&msg(
            RTM_NEWADDR,
            AF_INET,
            0,
            5,
            &[
                (IFA_ADDRESS, vec![10, 0, 0, 1]),
                (IFA_LOCAL, vec![10, 0, 0, 2]),
            ],
        ));
        assert_eq!(best(&a, 5).as_deref(), Some("10.0.0.2"));
        assert!(a.apply(&v4(RTM_DELADDR, 3, [192, 168, 1, 23], 0)));
        assert_eq!(best(&a, 3), None);
        assert!(!a.apply(&v4(RTM_DELADDR, 3, [192, 168, 1, 23], 0)));
        // A truncated message is read no further.
        let whole = v4(RTM_NEWADDR, 6, [10, 1, 1, 1], 0);
        assert!(!a.apply(&whole[..whole.len() - 3]));
    }

    #[test]
    fn the_best_address_is_ipv4_then_global_ipv6() {
        let mut a = Addresses::default();
        // IPv6 only: link-local, tentative, temporary and stable global.
        a.apply(&v6(2, "fe80::1", 253, 0x80));
        assert_eq!(best(&a, 2), None, "never link-local");
        a.apply(&v6(2, "2001:db8::5", 0, IFA_F_TENTATIVE));
        a.apply(&v6(2, "2001:db8::6", 0, IFA_F_DEPRECATED));
        a.apply(&v6(2, "2001:db8::7", 0, IFA_F_DADFAILED));
        assert_eq!(best(&a, 2), None, "none usable");
        a.apply(&v6(2, "2001:db8::1", 0, IFA_F_SECONDARY));
        assert_eq!(best(&a, 2).as_deref(), Some("2001:db8::1"), "temporary");
        a.apply(&v6(2, "2001:db8::9", 0, 0x200));
        assert_eq!(best(&a, 2).as_deref(), Some("2001:db8::9"), "stable first");
        // IPv4 wins, universe scope first.
        a.apply(&v4(RTM_NEWADDR, 2, [127, 0, 0, 9], 254));
        assert_eq!(best(&a, 2).as_deref(), Some("127.0.0.9"));
        a.apply(&v4(RTM_NEWADDR, 2, [192, 168, 0, 4], 0));
        assert_eq!(best(&a, 2).as_deref(), Some("192.168.0.4"));
    }

    #[test]
    fn a_dump_is_parsed_across_messages() {
        let mut a = Addresses::default();
        let mut first = v4(RTM_NEWADDR, 1, [127, 0, 0, 1], 254);
        first.extend(v6(1, "::1", 254, 0x80));
        a.apply(&first);
        assert!(!a.done, "the dump goes on");
        let mut second = v4(RTM_NEWADDR, 7, [192, 168, 7, 7], 0);
        second.extend(done());
        a.apply(&second);
        assert!(a.done);
        assert_eq!(best(&a, 1).as_deref(), Some("127.0.0.1"));
        assert_eq!(best(&a, 7).as_deref(), Some("192.168.7.7"));
    }

    /// The real kernel, over a real socket: `lo` has 127.0.0.1 (in a
    /// container's network namespace too).
    #[test]
    fn the_kernel_gives_lo_its_address() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut f = Follower::open().unwrap();
            let a = tokio::time::timeout(std::time::Duration::from_secs(5), f.changed(Some("lo")))
                .await
                .expect("the dump never ended")
                .unwrap();
            assert_eq!(a.as_deref(), Some("127.0.0.1"));
            // No change: nothing more until one comes.
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(200), f.changed(Some("lo")))
                    .await
                    .is_err()
            );
            // Another interface: its address (none for one not there).
            let none = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                f.changed(Some("strand-nonexistent0")),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(none, None);
        });
    }
}
