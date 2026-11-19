//! Connections counted by their source, for relays: a server behind a relay sees the
//! relay's address for every client, so it cannot tell its clients apart and the relay has
//! to. Used by routed networking's published ports (`tcp_proxy::start_port_forwards`) and
//! the fleet agent's IPv4 front (`fleet::agent::sni`). The fleet's servers count the
//! connections that reach them directly by the same rule (fleet/renderer/src/access.h,
//! `pending_source`; fleet/PROTOCOL.md, "Connection limits").

use std::collections::HashMap;
use std::hash::Hash;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};

/// Where a connection comes from, for counting: an IPv4 address (an IPv4-mapped IPv6
/// address is its IPv4 address), or an IPv6 /64, which an IPv6 subscriber usually holds
/// whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    V4(Ipv4Addr),
    V6Net([u8; 8]),
}

impl Source {
    pub fn of(ip: IpAddr) -> Source {
        match ip {
            IpAddr::V4(a) => Source::V4(a),
            IpAddr::V6(a) => match a.to_ipv4_mapped() {
                Some(v4) => Source::V4(v4),
                None => {
                    let o = a.octets();
                    let mut net = [0u8; 8];
                    net.copy_from_slice(&o[..8]);
                    Source::V6Net(net)
                }
            },
        }
    }
}

/// Whether `ip` is an address no viewer connects from directly: loopback, private (10/8,
/// 172.16/12, 192.168/16, 100.64/10, fc00::/7), link-local and 0/8. These are the host's own
/// relays and readers, each carrying many viewers; the renderer's `pending_source` leaves
/// the same addresses out.
pub fn is_relay_address(ip: IpAddr) -> bool {
    let v4 = |a: Ipv4Addr| {
        let o = a.octets();
        o[0] == 127
            || o[0] == 10
            || (o[0] == 172 && (o[1] & 0xf0) == 16)
            || (o[0] == 192 && o[1] == 168)
            || (o[0] == 100 && (o[1] & 0xc0) == 64)
            || (o[0] == 169 && o[1] == 254)
            || o[0] == 0
    };
    match ip {
        IpAddr::V4(a) => v4(a),
        IpAddr::V6(a) => match a.to_ipv4_mapped() {
            Some(m) => v4(m),
            None => {
                let o = a.octets();
                a.is_loopback() || (o[0] & 0xfe) == 0xfc || (o[0] == 0xfe && (o[1] & 0xc0) == 0x80)
            }
        },
    }
}

/// How many connections each key holds. A key is kept only while it holds one, so the
/// table never holds more keys than there are connections.
pub struct Counts<K> {
    held: Mutex<HashMap<K, usize>>,
}

impl<K: Hash + Eq + Clone> Counts<K> {
    pub fn new() -> Arc<Counts<K>> {
        Arc::new(Counts {
            held: Mutex::new(HashMap::new()),
        })
    }

    /// One more connection for `key`, or `None` (and nothing taken) when `key` holds `cap`.
    /// The place is given back when the returned value is dropped, on whatever path ends
    /// the connection.
    pub fn take(self: &Arc<Self>, key: K, cap: usize) -> Option<Held<K>> {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        let n = held.entry(key.clone()).or_insert(0);
        if *n >= cap {
            if *n == 0 {
                held.remove(&key);
            }
            return None;
        }
        *n += 1;
        Some(Held {
            counts: Arc::clone(self),
            key,
        })
    }

    /// How many `key` holds.
    pub fn held(&self, key: &K) -> usize {
        let held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        held.get(key).copied().unwrap_or(0)
    }

    /// How many keys hold a connection.
    pub fn keys(&self) -> usize {
        self.held.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// One connection's place in a [`Counts`], given back on drop.
pub struct Held<K: Hash + Eq + Clone> {
    counts: Arc<Counts<K>>,
    key: K,
}

impl<K: Hash + Eq + Clone> Drop for Held<K> {
    fn drop(&mut self) {
        let mut held = self.counts.held.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = held.get_mut(&self.key) {
            *n -= 1;
            if *n == 0 {
                held.remove(&self.key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn an_ipv6_source_is_its_64_and_a_mapped_address_is_ipv4() {
        assert_eq!(
            Source::of(ip("2001:db8:1:2::1")),
            Source::of(ip("2001:db8:1:2:ffff:ffff:ffff:ffff"))
        );
        assert_ne!(
            Source::of(ip("2001:db8:1:2::1")),
            Source::of(ip("2001:db8:1:3::1"))
        );
        assert_eq!(
            Source::of(ip("::ffff:192.0.2.7")),
            Source::of(ip("192.0.2.7"))
        );
        assert_ne!(Source::of(ip("192.0.2.7")), Source::of(ip("192.0.2.8")));
    }

    #[test]
    fn relay_addresses_are_the_renderers_local_ones() {
        for a in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "100.64.0.1",
            "100.127.255.255",
            "169.254.169.254",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fc00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_relay_address(ip(a)), "{a}");
        }
        for a in [
            "192.0.2.1",
            "172.32.0.1",
            "172.15.255.255",
            "100.128.0.1",
            "11.0.0.1",
            "2001:db8::1",
            "fec0::1",
            "::ffff:192.0.2.1",
        ] {
            assert!(!is_relay_address(ip(a)), "{a}");
        }
    }

    #[test]
    fn a_key_holds_at_most_its_cap_and_gives_places_back_on_drop() {
        let counts = Counts::new();
        let a = Source::of(ip("192.0.2.1"));
        let b = Source::of(ip("192.0.2.2"));
        let first = counts.take(a, 2).unwrap();
        let second = counts.take(a, 2).unwrap();
        assert!(counts.take(a, 2).is_none());
        assert_eq!(counts.held(&a), 2);
        let other = counts.take(b, 2).unwrap();
        drop(first);
        let third = counts.take(a, 2).unwrap();
        assert!(counts.take(a, 2).is_none());
        drop((second, third, other));
        assert_eq!(counts.keys(), 0, "a key that holds nothing is not kept");
        // A cap of zero takes nothing and keeps no key.
        assert!(counts.take(a, 0).is_none());
        assert_eq!(counts.keys(), 0);
    }
}
