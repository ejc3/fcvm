//! What a rootless namespace holds, as text: one line per link, address, route, policy
//! rule, neighbour, bridge forwarding entry, extra queueing discipline, per-link network
//! setting (forwarding, reverse-path filter, proxy neighbour, routes on a link without
//! carrier) and cached route that holds a path MTU or a redirect, plus counts of nftables
//! tables and of sockets in use by protocol. Two namespaces built the same way read the
//! same, and a namespace a VM has left reads as it did when it was built, or it is not
//! reused.
//!
//! The pool does not clean a namespace between VMs by walking a list of what a VM might
//! have changed. The only writers inside a namespace are its holder, pasta and the guest's
//! frames, the reset undoes what those are known to leave (neighbours, pasta's own state),
//! and this text is the check that nothing else is different. A difference destroys the
//! namespace.
//!
//! It is a check of what those three writers can reach, not of everything a process with
//! `CAP_NET_ADMIN` in the namespace could change. Not read, with the reason:
//!
//! - Proxy neighbour entries, legacy xtables rules (iptables-legacy keeps its tables
//!   outside nftables), traffic-control filters and classes, and a root queueing
//!   discipline that was replaced: each takes a netlink or setsockopt request from inside
//!   the namespace, which a guest's frames cannot make. An extra discipline (ingress,
//!   clsact) is listed.
//! - A link's attributes beyond its flags, MTU, MAC and bridge: its transmit queue
//!   length, its offload features, a TAP's virtio-net header size. A VMM sets the header
//!   size and the offloads on the TAP it attaches to, every time it attaches
//!   (`TUNSETVNETHDRSZ`, `TUNSETOFFLOAD`), so the next VMM does not run with the last
//!   one's.
//! - A bridge's multicast state (its group memberships and querier state) and its STP
//!   state. Multicast snooping learns groups from the guest's IGMP and MLD reports; the
//!   entries expire by themselves and decide only which ports a multicast frame is sent
//!   to, in a bridge whose other port is pasta.
//! - TCP metrics (the round-trip time and congestion window the kernel remembers per
//!   destination). Deleting them takes `CAP_NET_ADMIN` in the initial user namespace; they
//!   shape the namespace's own next connection to an address and are not sent to a guest.
//! - Sockets that are not bound, listening or connected. The per-protocol counts are of
//!   hashed sockets; the count of all sockets (`sockets: used`) includes the unconnected
//!   sockets pasta keeps ready and refills on a timer, so it is not a function of what a
//!   VM did. See [`super::sock_destroy`] for the one such socket that matters.
//!
//! Left out because the kernel changes it on its own and it carries nothing over from one
//! VM to the next:
//! - IPv6 link-local addresses and the routes and multicast entries that follow them: they
//!   appear a moment after a link first has a carrier.
//! - A link's running and dormant flags, and a route's link-down flag: they trail the
//!   carrier by up to a second.
//! - Each link's root queueing discipline: it changes from `noop` the first time the link
//!   has a carrier.
//! - Forwarding entries the bridge learned on pasta's port: pasta's own address, learned
//!   again from its next frame.

use super::rtnl::{
    Message, Rtnl, RTM_GETADDR, RTM_GETLINK, RTM_GETNEIGH, RTM_GETNETCONF, RTM_GETQDISC,
    RTM_GETROUTE, RTM_GETRULE,
};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fmt::Write as _;

const IFLA_ADDRESS: u16 = 1;
const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;
const IFLA_MASTER: u16 = 10;
const IFLA_LINKINFO: u16 = 18;
const IFLA_INFO_KIND: u16 = 1;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const RTA_DST: u16 = 1;
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;
const RTA_TABLE: u16 = 15;
const NDA_DST: u16 = 1;
const NDA_LLADDR: u16 = 2;
const TCA_KIND: u16 = 1;
const NETCONFA_IFINDEX: u16 = 1;
const NETCONFA_FORWARDING: u16 = 2;
/// The other per-link settings a netconf dump carries: reverse-path filter, proxy
/// neighbour, and whether routes on a link without carrier are ignored.
const NETCONFA_OTHER: [(u16, &str); 3] = [
    (3, "rp_filter"),
    (5, "proxy_neigh"),
    (6, "ignore_routes_with_linkdown"),
];

/// Flags that say how a link is configured, and whether it has a carrier
/// (`IFF_LOWER_UP`): a TAP with a carrier has a process attached to it.
const LINK_FLAGS: u32 = (libc::IFF_UP
    | libc::IFF_BROADCAST
    | libc::IFF_LOOPBACK
    | libc::IFF_NOARP
    | libc::IFF_PROMISC
    | libc::IFF_ALLMULTI
    | libc::IFF_MULTICAST) as u32
    | 0x1_0000;
const RTM_F_CLONED: u32 = 0x200;
const RTCF_REDIRECTED: u32 = 0x0004_0000;
const RTA_METRICS: u16 = 8;
const RTA_CACHEINFO: u16 = 12;
const RTAX_MTU: u16 = 2;
const NUD_PERMANENT: u16 = 0x80;
const NUD_NOARP: u16 = 0x40;
/// An entry of a device's own address lists, not of the bridge's table.
const NTF_SELF: u8 = 0x02;
/// The bridge an entry of a bridge's table belongs to.
const NDA_MASTER: u16 = 9;
/// `TC_H_ROOT`: the parent of a link's root queueing discipline.
const TC_H_ROOT: u32 = 0xffff_ffff;

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    bytes
        .get(at..at + 4)
        .map(|b| u32::from_ne_bytes(b.try_into().unwrap()))
        .unwrap_or(0)
}

fn address(family: u8, bytes: &[u8]) -> String {
    match (family as i32, bytes.len()) {
        (libc::AF_INET, 4) => {
            std::net::Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]).to_string()
        }
        (libc::AF_INET6, 16) => {
            std::net::Ipv6Addr::from(<[u8; 16]>::try_from(bytes).unwrap()).to_string()
        }
        _ => hex(bytes),
    }
}

/// Link-local unicast, multicast, or the unspecified prefix of an IPv6 default: state the
/// kernel's address configuration adds and removes by itself.
fn ipv6_automatic(family: u8, bytes: &[u8]) -> bool {
    family as i32 == libc::AF_INET6
        && bytes.len() == 16
        && (bytes[0] == 0xff || (bytes[0] == 0xfe && bytes[1] & 0xc0 == 0x80))
}

fn attr(message: &Message, skip: usize, kind: u16) -> Option<&[u8]> {
    message
        .attrs(skip)
        .into_iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, value)| value)
}

fn cstr(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes.split(|b| *b == 0).next().unwrap_or(&[])).into_owned()
}

/// Whether the link `name` of the calling process's network namespace has a carrier
/// (`IFF_LOWER_UP`). A TAP has one from the moment a process attaches to it until the last
/// one detaches; the flag does not wait for the kernel's link watch worker.
pub fn has_carrier(name: &str) -> Result<bool> {
    let mut rtnl = Rtnl::open().context("opening a netlink socket")?;
    for link in rtnl
        .dump(RTM_GETLINK, &[0u8; 16])
        .context("listing links")?
    {
        if attr(&link, 16, IFLA_IFNAME).map(cstr).as_deref() == Some(name) {
            return Ok(u32_at(&link.payload, 8) & 0x1_0000 != 0);
        }
    }
    anyhow::bail!("the namespace has no link {name}")
}

/// The fingerprint of the calling process's network namespace. `tap` is the VMM's TAP: a
/// forwarding entry the bridge learned on it belongs to a guest, so it is listed.
pub fn fingerprint(tap: &str) -> Result<String> {
    let mut rtnl = Rtnl::open().context("opening a netlink socket")?;
    let mut lines = Vec::new();

    // Links, and the index-to-name map every other table is printed through.
    let mut names = BTreeMap::new();
    let links = rtnl
        .dump(RTM_GETLINK, &[0u8; 16])
        .context("listing links")?;
    for link in &links {
        let index = u32_at(&link.payload, 4);
        let name = attr(link, 16, IFLA_IFNAME).map(cstr).unwrap_or_default();
        names.insert(index, name);
    }
    let name = |index: u32| {
        names
            .get(&index)
            .cloned()
            .unwrap_or_else(|| format!("#{index}"))
    };
    for link in &links {
        let index = u32_at(&link.payload, 4);
        let kind = attr(link, 16, IFLA_LINKINFO)
            .and_then(|info| {
                Message {
                    kind: 0,
                    payload: info.to_vec(),
                }
                .attrs(0)
                .into_iter()
                .find(|(k, _)| *k == IFLA_INFO_KIND)
                .map(|(_, v)| cstr(v))
            })
            .unwrap_or_else(|| "-".into());
        lines.push(format!(
            "link {} flags={:#x} mtu={} mac={} master={} kind={kind}",
            name(index),
            u32_at(&link.payload, 8) & LINK_FLAGS,
            attr(link, 16, IFLA_MTU).map(|v| u32_at(v, 0)).unwrap_or(0),
            attr(link, 16, IFLA_ADDRESS).map(hex).unwrap_or_default(),
            attr(link, 16, IFLA_MASTER)
                .map(|v| name(u32_at(v, 0)))
                .unwrap_or_else(|| "-".into()),
        ));
    }

    for entry in rtnl
        .dump(RTM_GETADDR, &[0u8; 8])
        .context("listing addresses")?
    {
        let (family, prefix, scope) = (entry.payload[0], entry.payload[1], entry.payload[3]);
        let Some(local) = attr(&entry, 8, IFA_LOCAL).or_else(|| attr(&entry, 8, IFA_ADDRESS))
        else {
            continue;
        };
        if ipv6_automatic(family, local) {
            continue;
        }
        lines.push(format!(
            "addr {} {}/{prefix} scope={scope}",
            name(u32_at(&entry.payload, 4)),
            address(family, local)
        ));
    }

    for route in rtnl
        .dump(RTM_GETROUTE, &[0u8; 12])
        .context("listing routes")?
    {
        let p = &route.payload;
        let (family, dst_len, protocol, scope, kind) = (p[0], p[1], p[5], p[6], p[7]);
        if u32_at(p, 8) & RTM_F_CLONED != 0 {
            continue;
        }
        let dst = attr(&route, 12, RTA_DST);
        if dst.is_some_and(|dst| ipv6_automatic(family, dst)) {
            continue;
        }
        let table = attr(&route, 12, RTA_TABLE)
            .map(|v| u32_at(v, 0))
            .unwrap_or(p[4] as u32);
        lines.push(format!(
            "route table={table} {}/{dst_len} via={} dev={} type={kind} proto={protocol} scope={scope}",
            dst.map(|dst| address(family, dst)).unwrap_or_else(|| "default".into()),
            attr(&route, 12, RTA_GATEWAY)
                .map(|gw| address(family, gw))
                .unwrap_or_else(|| "-".into()),
            attr(&route, 12, RTA_OIF)
                .map(|v| name(u32_at(v, 0)))
                .unwrap_or_else(|| "-".into()),
        ));
    }

    for family in [libc::AF_INET, libc::AF_INET6, libc::AF_BRIDGE] {
        let mut header = [0u8; 12];
        header[0] = family as u8;
        for entry in rtnl
            .dump(RTM_GETNEIGH, &header)
            .with_context(|| format!("listing neighbours of family {family}"))?
        {
            let p = &entry.payload;
            let device = name(u32_at(p, 4));
            let state = u16::from_ne_bytes([p[8], p[9]]);
            let flags = p[10];
            let mac = attr(&entry, 12, NDA_LLADDR).map(hex).unwrap_or_default();
            if family == libc::AF_BRIDGE {
                // The bridge's own table; a device's unicast and multicast lists (shown
                // "self") follow its addresses.
                if flags & NTF_SELF != 0 || attr(&entry, 12, NDA_MASTER).is_none() {
                    continue;
                }
                if state & (NUD_PERMANENT | NUD_NOARP) != 0 {
                    lines.push(format!("fdb {device} {mac} static"));
                } else if device == tap {
                    lines.push(format!("fdb {device} {mac} learned"));
                }
                continue;
            }
            // Entries nothing resolved: multicast and loopback.
            if state & NUD_NOARP != 0 {
                continue;
            }
            let Some(dst) = attr(&entry, 12, NDA_DST) else {
                continue;
            };
            if ipv6_automatic(family as u8, dst) {
                continue;
            }
            lines.push(format!(
                "neigh {device} {} {mac} state={state:#x}",
                address(family as u8, dst)
            ));
        }
    }

    for qdisc in rtnl
        .dump(RTM_GETQDISC, &[0u8; 20])
        .context("listing queueing disciplines")?
    {
        // Anything that is not a link's root discipline: an ingress or clsact hook.
        if u32_at(&qdisc.payload, 12) == TC_H_ROOT {
            continue;
        }
        lines.push(format!(
            "qdisc {} {}",
            name(u32_at(&qdisc.payload, 4)),
            attr(&qdisc, 20, TCA_KIND).map(cstr).unwrap_or_default()
        ));
    }

    // Whether each address family forwards, per link and for the namespace ("all" and
    // "default"), read over netlink: the same values as net.ipv4.conf.*.forwarding without
    // opening anything under /proc/sys.
    for family in [libc::AF_INET, libc::AF_INET6] {
        for conf in rtnl
            .dump(RTM_GETNETCONF, &[family as u8, 0, 0, 0])
            .with_context(|| format!("listing the forwarding switches of family {family}"))?
        {
            let Some(forwarding) = attr(&conf, 4, NETCONFA_FORWARDING) else {
                continue;
            };
            let device = match attr(&conf, 4, NETCONFA_IFINDEX).map(|v| u32_at(v, 0) as i32) {
                Some(-1) => "all".to_string(),
                Some(-2) => "default".to_string(),
                Some(index) => name(index as u32),
                None => continue,
            };
            lines.push(format!(
                "forwarding family={family} {device}={}",
                u32_at(forwarding, 0)
            ));
            for (kind, setting) in NETCONFA_OTHER {
                if let Some(value) = attr(&conf, 4, kind) {
                    lines.push(format!(
                        "{setting} family={family} {device}={}",
                        u32_at(value, 0)
                    ));
                }
            }
        }
    }

    // Policy routing rules, whole: a namespace is built with the kernel's three (local,
    // main, default) per family and nothing adds one.
    for family in [libc::AF_INET, libc::AF_INET6] {
        let mut header = [0u8; 12];
        header[0] = family as u8;
        for rule in rtnl
            .dump(RTM_GETRULE, &header)
            .with_context(|| format!("listing the policy rules of family {family}"))?
        {
            lines.push(format!(
                "rule family={family} {}",
                rule.payload
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            ));
        }
    }

    // Cached routes that hold something: a path MTU a guest's "fragmentation needed"
    // set, or a gateway a redirect set. The kernel also keeps a plain entry per destination
    // it has sent to, and an entry the route generation has left behind is emptied in place
    // at its next use; those say nothing a guest could have chosen and are not listed.
    for family in [libc::AF_INET, libc::AF_INET6] {
        let mut header = [0u8; 12];
        header[0] = family as u8;
        header[8..12].copy_from_slice(&RTM_F_CLONED.to_ne_bytes());
        for route in rtnl
            .dump(RTM_GETROUTE, &header)
            .with_context(|| format!("listing cached routes of family {family}"))?
        {
            let redirected = u32_at(&route.payload, 8) & RTCF_REDIRECTED != 0;
            // struct rta_cacheinfo: clntref, lastuse, then expires.
            let expires = attr(&route, 12, RTA_CACHEINFO).is_some_and(|info| u32_at(info, 8) != 0);
            let mtu = attr(&route, 12, RTA_METRICS).and_then(|metrics| {
                Message {
                    kind: 0,
                    payload: metrics.to_vec(),
                }
                .attrs(0)
                .into_iter()
                .find(|(kind, _)| *kind == RTAX_MTU)
                .map(|(_, value)| u32_at(value, 0))
            });
            if !redirected && !expires && mtu.is_none() {
                continue;
            }
            lines.push(format!(
                "cached route {} mtu={} redirected={redirected}",
                attr(&route, 12, RTA_DST)
                    .map(|dst| address(family as u8, dst))
                    .unwrap_or_else(|| "?".into()),
                mtu.map(|mtu| mtu.to_string()).unwrap_or_else(|| "-".into()),
            ));
        }
    }

    lines.sort();
    let mut text = lines.join("\n");
    let _ = write!(text, "\nnft tables={}", nft_tables()?);
    for file in ["sockstat", "sockstat6"] {
        let _ = write!(text, "\n{}", sockets_in_use(file)?);
    }
    text.push('\n');
    Ok(text)
}

/// The sockets in use in this namespace by protocol, from `/proc/net/sockstat` or
/// `sockstat6`: the kernel's per-namespace counters, read without walking a socket table.
/// Only the `inuse` figures: memory and orphan counts are host-wide in those files.
fn sockets_in_use(file: &str) -> Result<String> {
    let path = format!("/proc/net/{file}");
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?;
    Ok(sockets_in_use_from(file, &text))
}

fn sockets_in_use_from(file: &str, text: &str) -> String {
    let mut counts = Vec::new();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let Some(protocol) = words.next().and_then(|word| word.strip_suffix(':')) else {
            continue;
        };
        let words: Vec<&str> = words.collect();
        if let Some(at) = words.iter().position(|word| *word == "inuse") {
            if let Some(count) = words.get(at + 1) {
                counts.push(format!("{protocol}={count}"));
            }
        }
    }
    format!("sockets {file}: {}", counts.join(" "))
}

/// How many nftables tables this namespace has. A kernel without nftables has none.
fn nft_tables() -> Result<usize> {
    const NFNL_SUBSYS_NFTABLES: u16 = 10;
    const NFT_MSG_GETTABLE: u16 = 1;
    let mut nfnl = match Rtnl::open_protocol(libc::NETLINK_NETFILTER) {
        Ok(socket) => socket,
        Err(error) if error.raw_os_error() == Some(libc::EPROTONOSUPPORT) => return Ok(0),
        Err(error) => return Err(error).context("opening a netfilter netlink socket"),
    };
    // struct nfgenmsg: family, version, resource id.
    match nfnl.dump(NFNL_SUBSYS_NFTABLES << 8 | NFT_MSG_GETTABLE, &[0u8; 4]) {
        Ok(tables) => Ok(tables.len()),
        // The subsystem is not loaded: no table exists anywhere.
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::EINVAL) | Some(libc::EOPNOTSUPP) | Some(libc::ENOENT)
            ) =>
        {
            Ok(0)
        }
        Err(error) => Err(error).context("listing nftables tables"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the per-namespace `inuse` counts are read; `orphan`, `tw`, `alloc` and `mem`
    /// are the host's and change with every other namespace's traffic.
    #[test]
    fn socket_counts_are_the_in_use_figures_only() {
        let sockstat = "sockets: used 7\nTCP: inuse 2 orphan 31 tw 9 alloc 400 mem 77\n\
                        UDP: inuse 1 mem 3\nUDPLITE: inuse 0\nRAW: inuse 0\nFRAG: inuse 0 memory 0\n";
        assert_eq!(
            sockets_in_use_from("sockstat", sockstat),
            "sockets sockstat: TCP=2 UDP=1 UDPLITE=0 RAW=0 FRAG=0"
        );
    }

    /// The addresses the kernel configures by itself are not part of the fingerprint.
    #[test]
    fn link_local_and_multicast_ipv6_is_automatic() {
        let v6 = libc::AF_INET6 as u8;
        let parse = |text: &str| text.parse::<std::net::Ipv6Addr>().unwrap().octets();
        assert!(ipv6_automatic(v6, &parse("fe80::1")));
        assert!(ipv6_automatic(v6, &parse("ff02::1")));
        assert!(!ipv6_automatic(v6, &parse("fd00::100")));
        assert!(!ipv6_automatic(libc::AF_INET as u8, &[10, 0, 2, 1]));
    }
}
