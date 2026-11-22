//! The coordinator as the authoritative DNS server of the launch names' subzone
//! (`--dns-subzone`), which the parent zone delegates with NS records and no DS record.
//!
//! The records served are the coordinator's published state ([`zone_of`]): the actor mints a
//! launch's names and publishes them in one step, and the zone it hands this server changes
//! in that same step, before any snapshot names them. A name is answerable from the moment a
//! viewer can learn it, and a launch that is replaced or leaves the fleet stops answering.
//!
//! What is answered, for names in the subzone only (anything else is REFUSED, no recursion):
//! the apex's SOA and NS, `A` (`<label>-v4`) and `AAAA` (`<label>`) for published launches,
//! and `_acme-challenge.<subzone>` ([`Acme`]): a CNAME into the parent zone, where the renewal
//! of a zone certificate writes its DNS-01 TXT records, or, for a subzone certificate, the
//! TXT records its renewal holds ([`ChallengeTxt`]), answered from here. A missing name is NXDOMAIN and a missing type is
//! NODATA, both with the SOA, whose negative TTL is [`NEGATIVE_TTL`]. Every in-zone answer has
//! AA set. EDNS(0) is read and answered; a UDP answer larger than the client's buffer is sent
//! truncated (TC) with no records. Over UDP, responses per source prefix are limited
//! ([`RateLimit`]) so the server cannot be used to amplify traffic at a forged address.
//!
//! What the server did is logged where a lost answer would need it: each launch name's first
//! query ([`Asked`]: its type, transport, verdict and how long the name had been in the zone),
//! every one-second window in which the rate limit truncated or withheld responses (at the
//! first query after the window), and any rise in the kernel's UDP receive errors
//! ([`udp_errors`], `/proc/net/snmp`).
//!
//! Parsing never follows a compression pointer: a name in a query must be written out, so a
//! pointer loop cannot occur, and every read is bounded by the packet.

pub use super::cert::ChallengeTxt;
use anyhow::{bail, Context, Result};
use fleet_core::State;
use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

/// TTL of a launch's A and AAAA records. A name never changes address.
pub const RECORD_TTL: u32 = 60;
/// TTL of the SOA, and its minimum: how long a resolver keeps a negative answer.
pub const NEGATIVE_TTL: u32 = 5;
/// TTL of the apex's NS records and the ACME CNAME.
pub const NS_TTL: u32 = 3600;
/// TTL of a DNS-01 TXT record: the renewal checks it through a public resolver, and its
/// value changes with every order.
pub const TXT_TTL: u32 = 5;
/// The UDP payload size this server advertises and answers within (RFC 9715's default).
pub const EDNS_SIZE: u16 = 1232;
/// The longest query read; anything longer is dropped (a query is a header, one name of at
/// most 255 bytes, and an OPT record).
pub const MAX_QUERY: usize = 1024;
/// Concurrent TCP connections; more are closed as they arrive.
pub const MAX_TCP: usize = 128;
/// Queries answered on one TCP connection before it is closed.
pub const TCP_QUERIES: usize = 64;
/// How long a TCP connection may take to send a query or read an answer.
pub const TCP_IDLE: Duration = Duration::from_secs(10);
/// Source prefixes tracked per second by the rate limit; past it, new prefixes are limited.
pub const RATE_PREFIXES: usize = 16384;

/// What `_acme-challenge.<apex>` answers.
#[derive(Debug, Clone, PartialEq)]
pub enum Acme {
    /// Nothing: the name does not exist.
    None,
    /// A CNAME to this name outside the subzone, where a zone certificate's renewal writes
    /// its DNS-01 records.
    Cname(String),
    /// The TXT values a subzone certificate's renewal holds.
    Txt(ChallengeTxt),
}

/// The subzone and what is fixed about it.
#[derive(Debug, Clone, PartialEq)]
pub struct ZoneConfig {
    /// The subzone, lower case, no trailing dot.
    pub apex: String,
    /// Its nameservers' names, as the parent's NS records name them.
    pub nameservers: Vec<String>,
    /// What `_acme-challenge.<apex>` answers.
    pub acme: Acme,
}

fn normal(name: &str) -> Result<String> {
    let n = name.trim_end_matches('.').to_ascii_lowercase();
    if n.is_empty() || n.len() > 253 {
        bail!("{name:?} is not a DNS name");
    }
    for label in n.split('.') {
        if label.is_empty()
            || label.len() > 63
            || !label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            bail!("{name:?} is not a DNS name");
        }
    }
    Ok(n)
}

impl ZoneConfig {
    pub fn new(apex: &str, nameservers: &[String], acme: Acme) -> Result<Self> {
        let apex = normal(apex)?;
        if nameservers.is_empty() {
            bail!("the subzone {apex} needs at least one nameserver name");
        }
        let nameservers = nameservers
            .iter()
            .map(|n| normal(n))
            .collect::<Result<Vec<_>>>()?;
        let acme = match acme {
            Acme::Cname(t) => {
                let t = normal(&t)?;
                if in_zone(&t, &apex) {
                    bail!("the ACME target {t} must be outside {apex}");
                }
                Acme::Cname(t)
            }
            Acme::Txt(txt) => {
                let want = format!("_acme-challenge.{apex}");
                if txt.name != want {
                    bail!("the ACME challenge {} is not {want}", txt.name);
                }
                Acme::Txt(txt)
            }
            Acme::None => Acme::None,
        };
        Ok(ZoneConfig {
            apex,
            nameservers,
            acme,
        })
    }

    fn acme_name(&self) -> String {
        format!("_acme-challenge.{}", self.apex)
    }
}

/// The records the subzone holds now.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Zone {
    /// The SOA serial (the state version it was taken from).
    pub serial: u32,
    /// Owner name (lower case, no trailing dot) to addresses.
    pub a: BTreeMap<String, Vec<Ipv4Addr>>,
    pub aaaa: BTreeMap<String, Vec<Ipv6Addr>>,
}

impl Zone {
    /// Same records, whatever the serial.
    pub fn same_records(&self, other: &Zone) -> bool {
        self.a == other.a && self.aaaa == other.aaaa
    }
}

/// The subzone's records for a state: every slot's and tile's published launch and every
/// aggregator's published names (a host's `agg-<host>-<token>`, the EC2 half's
/// `agg-ec2-<token>`), `<label>` with its IPv6 address and `<label>-v4` with its IPv4
/// address.
pub fn zone_of(state: &State, apex: &str) -> Zone {
    let mut z = Zone {
        serial: state.version as u32,
        ..Zone::default()
    };
    let f = &state.fleet;
    let launches = f
        .slots
        .values()
        .chain(f.ec2.values())
        .chain(f.lambda.values())
        .chain(f.phones.values())
        .chain(f.windows.values())
        .chain(f.minecraft.values())
        .filter_map(|s| s.published.as_ref());
    let aggregators = f
        .hosts
        .values()
        .filter_map(|h| h.aggregate_published.as_ref())
        .chain(f.ec2_aggregate_published.as_ref());
    for p in launches.chain(aggregators) {
        let label = p.label.to_ascii_lowercase();
        if let Some(v6) = p.ipv6.as_deref().and_then(|a| a.parse().ok()) {
            z.aaaa.insert(format!("{label}.{apex}"), vec![v6]);
        }
        if let Some(v4) = p.ipv4.as_deref().and_then(|a| a.parse().ok()) {
            z.a.insert(format!("{label}-v4.{apex}"), vec![v4]);
        }
    }
    // With Lambda tiles, their tunnel endpoint: the coordinator task's own addresses, where
    // its front takes the tunnel (`lambda_front`).
    if state.config.lambda_max > 0 {
        let c = &state.config;
        let name = format!("{}.{apex}", super::lambda_front::TUNNEL_LABEL);
        if let Some(v6) = c.ec2_aggregate_ipv6.as_deref().and_then(|a| a.parse().ok()) {
            z.aaaa.insert(name.clone(), vec![v6]);
        }
        if let Some(v4) = c.ec2_aggregate_ipv4.as_deref().and_then(|a| a.parse().ok()) {
            z.a.insert(name, vec![v4]);
        }
    }
    z
}

fn in_zone(name: &str, apex: &str) -> bool {
    name == apex
        || name
            .strip_suffix(apex)
            .is_some_and(|rest| rest.ends_with('.'))
}

const T_A: u16 = 1;
const T_NS: u16 = 2;
const T_CNAME: u16 = 5;
const T_SOA: u16 = 6;
const T_TXT: u16 = 16;
const T_AAAA: u16 = 28;
const T_OPT: u16 = 41;
const T_IXFR: u16 = 251;
const T_AXFR: u16 = 252;
const T_ANY: u16 = 255;

const FORMERR: u16 = 1;
const NXDOMAIN: u16 = 3;
const NOTIMP: u16 = 4;
const REFUSED: u16 = 5;

struct Edns {
    size: u16,
    version: u8,
}

struct Query<'a> {
    id: u16,
    rd: bool,
    /// The question as received (name, type, class), echoed byte for byte.
    question: &'a [u8],
    /// The name, lower case, no trailing dot; a byte that cannot be in a host name is `?`.
    name: String,
    qtype: u16,
    qclass: u16,
    edns: Option<Edns>,
}

enum Parsed<'a> {
    Query(Query<'a>),
    /// Answer with this rcode and no question.
    Error {
        id: u16,
        rd: bool,
        rcode: u16,
    },
    /// Not a query: send nothing.
    Ignore,
}

fn u16_at(m: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*m.get(i)?, *m.get(i + 1)?]))
}

/// A name written out label by label, from `i`: the name and the offset after it. A
/// compression pointer, a label over 63 bytes or a name over 255 bytes is `None`.
fn read_name(m: &[u8], mut i: usize) -> Option<(String, usize)> {
    let mut name = String::new();
    let mut wire = 1;
    loop {
        let len = *m.get(i)? as usize;
        if len == 0 {
            return Some((name, i + 1));
        }
        if len > 63 {
            return None;
        }
        wire += 1 + len;
        if wire > 255 {
            return None;
        }
        let label = m.get(i + 1..i + 1 + len)?;
        if !name.is_empty() {
            name.push('.');
        }
        for &b in label {
            name.push(
                if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'*' {
                    b.to_ascii_lowercase() as char
                } else {
                    '?'
                },
            );
        }
        i += 1 + len;
    }
}

fn parse(m: &[u8]) -> Parsed<'_> {
    let (Some(id), Some(flags)) = (u16_at(m, 0), u16_at(m, 2)) else {
        return Parsed::Ignore;
    };
    if m.len() < 12 || flags & 0x8000 != 0 {
        return Parsed::Ignore;
    }
    let rd = flags & 0x0100 != 0;
    let err = |rcode| Parsed::Error { id, rd, rcode };
    if (flags >> 11) & 0xF != 0 {
        return err(NOTIMP);
    }
    let count = |i| u16_at(m, i).unwrap_or(0);
    if count(4) != 1 || count(6) != 0 || count(8) != 0 {
        return err(FORMERR);
    }
    let Some((name, end)) = read_name(m, 12) else {
        return err(FORMERR);
    };
    let (Some(qtype), Some(qclass)) = (u16_at(m, end), u16_at(m, end + 2)) else {
        return err(FORMERR);
    };
    let question = &m[12..end + 4];
    let mut i = end + 4;
    let mut edns = None;
    for _ in 0..count(10) {
        // The owner of an additional record; only OPT's (the root) is read.
        let root = m.get(i) == Some(&0);
        let after = if root {
            i + 1
        } else {
            match read_name(m, i) {
                Some((_, after)) => after,
                None => return err(FORMERR),
            }
        };
        let (Some(t), Some(class), Some(ttl_hi), Some(rdlen)) = (
            u16_at(m, after),
            u16_at(m, after + 2),
            u16_at(m, after + 4),
            u16_at(m, after + 8),
        ) else {
            return err(FORMERR);
        };
        let next = after + 10 + rdlen as usize;
        if next > m.len() {
            return err(FORMERR);
        }
        if t == T_OPT {
            if !root || edns.is_some() {
                return err(FORMERR);
            }
            edns = Some(Edns {
                size: class,
                version: (ttl_hi & 0xFF) as u8,
            });
        }
        i = next;
    }
    Parsed::Query(Query {
        id,
        rd,
        question,
        name,
        qtype,
        qclass,
        edns,
    })
}

fn put_name(out: &mut Vec<u8>, name: &str) {
    for label in name.split('.').filter(|l| !l.is_empty()) {
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
}

/// One resource record; `owner` None is the question's name (a pointer to it).
fn rr(owner: Option<&str>, rtype: u16, ttl: u32, rdata: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + rdata.len());
    match owner {
        None => out.extend_from_slice(&[0xC0, 12]),
        Some(n) => put_name(&mut out, n),
    }
    out.extend_from_slice(&rtype.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&ttl.to_be_bytes());
    out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
    out.extend_from_slice(rdata);
    out
}

fn soa(cfg: &ZoneConfig, zone: &Zone) -> Vec<u8> {
    let mut rdata = Vec::new();
    put_name(&mut rdata, &cfg.nameservers[0]);
    put_name(&mut rdata, &format!("hostmaster.{}", cfg.apex));
    for v in [zone.serial, 3600, 600, 86400, NEGATIVE_TTL] {
        rdata.extend_from_slice(&v.to_be_bytes());
    }
    rr(Some(&cfg.apex), T_SOA, NEGATIVE_TTL, &rdata)
}

struct Reply {
    rcode: u16,
    aa: bool,
    answer: Vec<Vec<u8>>,
    authority: Vec<Vec<u8>>,
}

fn lookup(cfg: &ZoneConfig, zone: &Zone, q: &Query) -> Reply {
    let refused = Reply {
        rcode: REFUSED,
        aa: false,
        answer: vec![],
        authority: vec![],
    };
    if !in_zone(&q.name, &cfg.apex) || !(q.qclass == 1 || q.qclass == 255) {
        return refused;
    }
    if q.qtype == T_AXFR || q.qtype == T_IXFR {
        return refused;
    }
    let found = |answer: Vec<Vec<u8>>| Reply {
        rcode: 0,
        aa: true,
        answer,
        authority: vec![],
    };
    let negative = |rcode| Reply {
        rcode,
        aa: true,
        answer: vec![],
        authority: vec![soa(cfg, zone)],
    };
    let a = |want| {
        zone.a
            .get(&q.name)
            .filter(|_| want)
            .into_iter()
            .flatten()
            .map(|ip| rr(None, T_A, RECORD_TTL, &ip.octets()))
    };
    let aaaa = |want| {
        zone.aaaa
            .get(&q.name)
            .filter(|_| want)
            .into_iter()
            .flatten()
            .map(|ip| rr(None, T_AAAA, RECORD_TTL, &ip.octets()))
    };
    if q.name == cfg.apex {
        let answer: Vec<_> = match q.qtype {
            T_SOA => vec![soa(cfg, zone)],
            T_NS | T_ANY => cfg
                .nameservers
                .iter()
                .map(|n| {
                    let mut rdata = Vec::new();
                    put_name(&mut rdata, n);
                    rr(None, T_NS, NS_TTL, &rdata)
                })
                .collect(),
            _ => vec![],
        };
        return if answer.is_empty() {
            negative(0)
        } else {
            found(answer)
        };
    }
    // The challenge's TXT values now; the name exists while it holds any.
    let txt: Vec<String> = match &cfg.acme {
        Acme::Txt(t) => t.values(),
        _ => Vec::new(),
    };
    if q.name == cfg.acme_name() {
        match &cfg.acme {
            Acme::Cname(target) => {
                let mut rdata = Vec::new();
                put_name(&mut rdata, target);
                return found(vec![rr(None, T_CNAME, NS_TTL, &rdata)]);
            }
            Acme::Txt(_) if !txt.is_empty() => {
                let answer: Vec<_> = txt
                    .iter()
                    .filter(|_| q.qtype == T_TXT || q.qtype == T_ANY)
                    .filter(|v| v.len() <= 255)
                    .map(|v| {
                        let mut rdata = vec![v.len() as u8];
                        rdata.extend_from_slice(v.as_bytes());
                        rr(None, T_TXT, TXT_TTL, &rdata)
                    })
                    .collect();
                return if answer.is_empty() {
                    negative(0)
                } else {
                    found(answer)
                };
            }
            _ => {}
        }
    }
    if zone.a.contains_key(&q.name) || zone.aaaa.contains_key(&q.name) {
        let answer: Vec<_> = a(q.qtype == T_A || q.qtype == T_ANY)
            .chain(aaaa(q.qtype == T_AAAA || q.qtype == T_ANY))
            .collect();
        return if answer.is_empty() {
            negative(0)
        } else {
            found(answer)
        };
    }
    // A name with names below it exists (NODATA), though launch labels never nest.
    let below = format!(".{}", q.name);
    let acme = (matches!(cfg.acme, Acme::Cname(_)) || !txt.is_empty()).then(|| cfg.acme_name());
    if zone
        .a
        .keys()
        .chain(zone.aaaa.keys())
        .chain(acme.as_ref())
        .any(|o| o.ends_with(&below))
    {
        return negative(0);
    }
    negative(NXDOMAIN)
}

/// The response to a query packet, within a UDP payload of `limit` bytes (TC beyond it);
/// `None` when the packet is not a query and gets no response.
fn respond_within(
    cfg: &ZoneConfig,
    zone: &Zone,
    m: &[u8],
    limit: Option<usize>,
) -> Option<Vec<u8>> {
    let (id, rd, question, edns, reply) = match parse(m) {
        Parsed::Ignore => return None,
        Parsed::Error { id, rd, rcode } => (
            id,
            rd,
            &[][..],
            None,
            Reply {
                rcode,
                aa: false,
                answer: vec![],
                authority: vec![],
            },
        ),
        Parsed::Query(q) => {
            let reply = match &q.edns {
                // Only EDNS version 0 exists: BADVERS (extended rcode 16).
                Some(e) if e.version != 0 => Reply {
                    rcode: 16,
                    aa: false,
                    answer: vec![],
                    authority: vec![],
                },
                _ => lookup(cfg, zone, &q),
            };
            (q.id, q.rd, q.question, q.edns, reply)
        }
    };
    let limit = limit.unwrap_or(match &edns {
        Some(e) => (e.size as usize).clamp(512, EDNS_SIZE as usize),
        None => 512,
    });
    let build = |truncated: bool| {
        let mut out = Vec::with_capacity(128);
        out.extend_from_slice(&id.to_be_bytes());
        let mut flags: u16 = 0x8000 | (reply.rcode & 0xF);
        if reply.aa {
            flags |= 0x0400;
        }
        if truncated {
            flags |= 0x0200;
        }
        if rd {
            flags |= 0x0100;
        }
        out.extend_from_slice(&flags.to_be_bytes());
        let (an, ns) = if truncated {
            (0, 0)
        } else {
            (reply.answer.len(), reply.authority.len())
        };
        let qd = u16::from(!question.is_empty());
        for n in [qd, an as u16, ns as u16, u16::from(edns.is_some())] {
            out.extend_from_slice(&n.to_be_bytes());
        }
        out.extend_from_slice(question);
        if !truncated {
            for r in reply.answer.iter().chain(&reply.authority) {
                out.extend_from_slice(r);
            }
        }
        if edns.is_some() {
            // OPT: root owner, our payload size, the extended rcode's upper bits, version 0.
            out.push(0);
            out.extend_from_slice(&T_OPT.to_be_bytes());
            out.extend_from_slice(&EDNS_SIZE.to_be_bytes());
            out.push((reply.rcode >> 4) as u8);
            out.extend_from_slice(&[0, 0, 0, 0, 0]);
        }
        out
    };
    let full = build(false);
    Some(if full.len() > limit {
        build(true)
    } else {
        full
    })
}

/// The response to a query packet received over UDP (`tcp` false) or TCP; `None` when the
/// packet gets no response.
pub fn respond(cfg: &ZoneConfig, zone: &Zone, m: &[u8], tcp: bool) -> Option<Vec<u8>> {
    if m.len() > MAX_QUERY {
        return None;
    }
    respond_within(cfg, zone, m, tcp.then_some(65535))
}

/// The same response with every record left out and TC set: what a rate-limited source
/// gets in place of an answer, no larger than its query.
pub fn truncated(cfg: &ZoneConfig, zone: &Zone, m: &[u8]) -> Option<Vec<u8>> {
    if m.len() > MAX_QUERY {
        return None;
    }
    respond_within(cfg, zone, m, Some(0))
}

/// What to do with one UDP response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Answer,
    /// Send it truncated: a real resolver retries over TCP, a forged source gets nothing
    /// larger than what was sent.
    Slip,
    Drop,
}

/// Responses per source prefix (/24 for IPv4, /56 for IPv6) per one-second window. Past the
/// limit, every second response slips (TC, no records) and the rest are dropped. The table
/// holds at most [`RATE_PREFIXES`] prefixes a window; a new prefix past that is limited.
pub struct RateLimit {
    per_second: u32,
    window: Option<Instant>,
    counts: HashMap<(bool, u128), u32>,
    slip: bool,
    /// Responses of the current window sent truncated, and withheld.
    slipped: u32,
    dropped: u32,
}

impl RateLimit {
    pub fn new(per_second: u32) -> Self {
        RateLimit {
            per_second,
            window: None,
            counts: HashMap::new(),
            slip: false,
            slipped: 0,
            dropped: 0,
        }
    }

    /// The source's prefix: (is IPv4, the address with its host bits cleared).
    fn prefix(ip: IpAddr) -> (bool, u128) {
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
            v4 => v4,
        };
        match ip {
            IpAddr::V4(v4) => (true, u128::from(u32::from(v4) & 0xFFFF_FF00)),
            IpAddr::V6(v6) => (false, u128::from(v6) & !((1u128 << 72) - 1)),
        }
    }

    pub fn check(&mut self, ip: IpAddr, now: Instant) -> Verdict {
        if self
            .window
            .is_none_or(|w| now.duration_since(w) >= Duration::from_secs(1))
        {
            if self.slipped + self.dropped > 0 {
                tracing::warn!(
                    slipped = self.slipped,
                    dropped = self.dropped,
                    prefixes = self.counts.len(),
                    per_second = self.per_second,
                    "DNS responses rate limited"
                );
            }
            self.window = Some(now);
            self.counts.clear();
            self.slipped = 0;
            self.dropped = 0;
        }
        let key = Self::prefix(ip);
        let full = self.counts.len() >= RATE_PREFIXES;
        let n = match self.counts.get_mut(&key) {
            Some(n) => n,
            None if full => return self.over(),
            None => self.counts.entry(key).or_insert(0),
        };
        *n = n.saturating_add(1);
        if *n <= self.per_second {
            Verdict::Answer
        } else {
            self.over()
        }
    }

    fn over(&mut self) -> Verdict {
        self.slip = !self.slip;
        if self.slip {
            self.slipped = self.slipped.saturating_add(1);
            Verdict::Slip
        } else {
            self.dropped = self.dropped.saturating_add(1);
            Verdict::Drop
        }
    }
}

impl Verdict {
    fn name(self) -> &'static str {
        match self {
            Verdict::Answer => "answer",
            Verdict::Slip => "slip",
            Verdict::Drop => "drop",
        }
    }
}

/// When each name of the zone appeared in the zone this server answers from, until its first
/// query is logged. `serve` refreshes it whenever the zone changes, so the time is the name's
/// publication; a name that leaves the zone is forgotten, so this holds at most the zone's
/// names.
#[derive(Default)]
pub struct Asked {
    serial: Option<u32>,
    /// Name to (when this server first had it, whether its first query was logged).
    names: HashMap<String, (Instant, bool)>,
}

impl Asked {
    /// Take the zone's names: a new one appeared `now`, one no longer held is forgotten.
    pub fn refresh(&mut self, zone: &Zone, now: Instant) {
        if self.serial == Some(zone.serial) {
            return;
        }
        self.serial = Some(zone.serial);
        let held: std::collections::HashSet<&String> =
            zone.a.keys().chain(zone.aaaa.keys()).collect();
        self.names.retain(|n, _| held.contains(n));
        for n in held {
            self.names.entry(n.clone()).or_insert((now, false));
        }
    }

    /// Note a query for `name` (lower case, no trailing dot) of `qtype` and what was done with
    /// it; logs the first one for each name the zone holds.
    pub fn note(
        &mut self,
        zone: &Zone,
        name: &str,
        qtype: u16,
        transport: &str,
        verdict: Verdict,
        now: Instant,
    ) {
        // The zone watcher in `serve` normally took this zone already; this covers a query
        // that arrives before it runs.
        self.refresh(zone, now);
        let Some((since, logged)) = self.names.get_mut(name) else {
            return;
        };
        if *logged {
            return;
        }
        *logged = true;
        tracing::info!(
            name,
            qtype,
            transport,
            verdict = verdict.name(),
            in_zone_ms = now.duration_since(*since).as_millis() as u64,
            "launch name first asked"
        );
    }
}

/// The question of a query, when it is one: its name (lower case, no trailing dot) and type.
fn question_of(m: &[u8]) -> Option<(String, u16)> {
    match parse(m) {
        Parsed::Query(q) => Some((q.name, q.qtype)),
        _ => None,
    }
}

/// `InErrors` and `RcvbufErrors` of the `Udp:` lines of `/proc/net/snmp`: datagrams the kernel
/// discarded before the server read them (a full receive buffer among them).
pub fn udp_errors(snmp: &str) -> Option<(u64, u64)> {
    let mut lines = snmp.lines().filter(|l| l.starts_with("Udp:"));
    let names: Vec<&str> = lines.next()?.split_whitespace().collect();
    let values: Vec<&str> = lines.next()?.split_whitespace().collect();
    let get = |key: &str| -> Option<u64> {
        let i = names.iter().position(|n| *n == key)?;
        values.get(i)?.parse().ok()
    };
    Some((get("InErrors")?, get("RcvbufErrors")?))
}

/// Every 10 s, log a rise in the kernel's UDP receive errors. The counters are the network
/// namespace's (in the coordinator's task, also cloudflared's QUIC sockets), so a rise says
/// datagrams were lost on arrival in this task, and the first-query lines say which names.
async fn watch_udp_errors() {
    let mut last: Option<(u64, u64)> = None;
    loop {
        if let Some(now) = std::fs::read_to_string("/proc/net/snmp")
            .ok()
            .as_deref()
            .and_then(udp_errors)
        {
            if let Some((in_errors, rcvbuf)) = last {
                if now.0 > in_errors || now.1 > rcvbuf {
                    tracing::warn!(
                        in_errors = now.0.saturating_sub(in_errors),
                        rcvbuf_errors = now.1.saturating_sub(rcvbuf),
                        "the kernel discarded received UDP datagrams in this network namespace"
                    );
                }
            }
            last = Some(now);
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

/// The server: the subzone and the latest records.
#[derive(Clone)]
pub struct Authority {
    pub cfg: Arc<ZoneConfig>,
    pub zone: watch::Receiver<Arc<Zone>>,
}

/// The addresses `Authority::serve` bound: one per protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Served {
    pub udp: SocketAddr,
    pub tcp: SocketAddr,
}

impl std::fmt::Display for Served {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.udp == self.tcp {
            write!(f, "{}", self.udp)
        } else {
            write!(f, "udp {} tcp {}", self.udp, self.tcp)
        }
    }
}

impl Authority {
    /// Answer UDP and TCP on `listen` until the process ends. Both sockets bind `listen` as
    /// given, scope id included: a scoped link-local IPv6 address rebuilt from its parts
    /// loses the scope and the bind fails with EINVAL. A port of 0 is asked of the kernel
    /// once per protocol. UDP and TCP have separate port spaces, so a port the kernel found
    /// free for UDP says nothing about TCP, and binding TCP to it fails whenever any other
    /// socket on the host holds that TCP port.
    pub async fn serve(self, listen: SocketAddr, per_second: u32) -> Result<Served> {
        let udp = tokio::net::UdpSocket::bind(listen)
            .await
            .with_context(|| format!("binding UDP {listen}"))?;
        let tcp = tokio::net::TcpListener::bind(listen)
            .await
            .with_context(|| format!("binding TCP {listen}"))?;
        let served = Served {
            udp: udp.local_addr()?,
            tcp: tcp.local_addr()?,
        };
        let asked = Arc::new(std::sync::Mutex::new(Asked::default()));
        asked
            .lock()
            .unwrap()
            .refresh(&self.zone.borrow(), Instant::now());
        {
            // Each name's time in `Asked` is when this server got the zone that holds it.
            let mut zone = self.zone.clone();
            let asked = asked.clone();
            tokio::spawn(async move {
                while zone.changed().await.is_ok() {
                    let z = zone.borrow_and_update().clone();
                    asked.lock().unwrap().refresh(&z, Instant::now());
                }
            });
        }
        tokio::spawn(
            self.clone()
                .udp(udp, RateLimit::new(per_second), asked.clone()),
        );
        tokio::spawn(self.tcp(tcp, asked));
        tokio::spawn(watch_udp_errors());
        Ok(served)
    }

    async fn udp(
        self,
        sock: tokio::net::UdpSocket,
        mut limit: RateLimit,
        asked: Arc<std::sync::Mutex<Asked>>,
    ) {
        let mut buf = vec![0u8; MAX_QUERY + 1];
        loop {
            let (n, peer) = match sock.recv_from(&mut buf).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "DNS UDP receive failed");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                }
            };
            let zone = self.zone.borrow().clone();
            let m = &buf[..n];
            let now = Instant::now();
            let verdict = limit.check(peer.ip(), now);
            if let Some((name, qtype)) = question_of(m) {
                asked
                    .lock()
                    .unwrap()
                    .note(&zone, &name, qtype, "udp", verdict, now);
            }
            let out = match verdict {
                Verdict::Answer => respond(&self.cfg, &zone, m, false),
                Verdict::Slip => truncated(&self.cfg, &zone, m),
                Verdict::Drop => None,
            };
            if let Some(out) = out {
                if let Err(e) = sock.send_to(&out, peer).await {
                    tracing::debug!(error = %e, %peer, "DNS UDP send failed");
                }
            }
        }
    }

    async fn tcp(self, listener: tokio::net::TcpListener, asked: Arc<std::sync::Mutex<Asked>>) {
        let slots = Arc::new(tokio::sync::Semaphore::new(MAX_TCP));
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "DNS TCP accept failed");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                }
            };
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                continue; // dropped: closed at once
            };
            let me = self.clone();
            let asked = asked.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let _ = me.connection(stream, asked).await;
            });
        }
    }

    async fn connection(
        &self,
        mut s: tokio::net::TcpStream,
        asked: Arc<std::sync::Mutex<Asked>>,
    ) -> Result<()> {
        let mut buf = vec![0u8; MAX_QUERY];
        for _ in 0..TCP_QUERIES {
            let len = tokio::time::timeout(TCP_IDLE, s.read_u16()).await?? as usize;
            if len == 0 || len > MAX_QUERY {
                return Ok(());
            }
            tokio::time::timeout(TCP_IDLE, s.read_exact(&mut buf[..len])).await??;
            let zone = self.zone.borrow().clone();
            if let Some((name, qtype)) = question_of(&buf[..len]) {
                asked.lock().unwrap().note(
                    &zone,
                    &name,
                    qtype,
                    "tcp",
                    Verdict::Answer,
                    Instant::now(),
                );
            }
            let Some(out) = respond(&self.cfg, &zone, &buf[..len], true) else {
                return Ok(());
            };
            let mut framed = Vec::with_capacity(out.len() + 2);
            framed.extend_from_slice(&(out.len() as u16).to_be_bytes());
            framed.extend_from_slice(&out);
            tokio::time::timeout(TCP_IDLE, s.write_all(&framed)).await??;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Logs written while `f` runs on this thread (and by tasks a current-thread runtime runs
    /// on it).
    struct Logs(Arc<std::sync::Mutex<Vec<u8>>>);
    impl Logs {
        fn capture() -> (Logs, tracing::subscriber::DefaultGuard) {
            #[derive(Clone)]
            struct W(Arc<std::sync::Mutex<Vec<u8>>>);
            impl std::io::Write for W {
                fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                    self.0.lock().unwrap().extend_from_slice(b);
                    Ok(b.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            let buf = Arc::new(std::sync::Mutex::new(Vec::new()));
            let w = W(buf.clone());
            let sub = tracing_subscriber::fmt()
                .with_writer(move || w.clone())
                .with_ansi(false)
                .finish();
            (Logs(buf), tracing::subscriber::set_default(sub))
        }
        fn lines(&self, containing: &str) -> Vec<String> {
            String::from_utf8(self.0.lock().unwrap().clone())
                .unwrap()
                .lines()
                .filter(|l| l.contains(containing))
                .map(str::to_string)
                .collect()
        }
    }

    /// One UDP query to `server`; the answer, or None within 500 ms.
    async fn ask_udp(server: SocketAddr, q: &[u8]) -> Option<Vec<u8>> {
        let s = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        s.send_to(q, server).await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = tokio::time::timeout(Duration::from_millis(500), s.recv(&mut buf))
            .await
            .ok()?
            .unwrap();
        Some(buf[..n].to_vec())
    }

    /// A launch name's first query is logged once: its name and type, the transport, what
    /// the server did with it and how long the name had been in its zone. On 2026-11-15,
    /// 11-20 and 11-22 a relaunched tile's fresh name got no answer from 1.1.1.1 within 2 s,
    /// and nothing on the server said whether its query ever arrived.
    #[tokio::test(flavor = "current_thread")]
    async fn a_launch_names_first_query_is_logged_once() {
        let (logs, _guard) = Logs::capture();
        let (tx, rx) = watch::channel(Arc::new(Zone::default()));
        let served = Authority {
            cfg: Arc::new(cfg()),
            zone: rx,
        }
        .serve("127.0.0.1:0".parse().unwrap(), 1000)
        .await
        .unwrap();
        // Published while nothing is asked: its time in the zone counts from here.
        tx.send(Arc::new(zone())).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let name = "slot-1-abcdefgh.tiles.fcvm.example";
        for _ in 0..2 {
            assert!(ask_udp(served.udp, &query(name, T_AAAA, Some(1232)))
                .await
                .is_some());
        }
        // A name the zone does not hold is not a launch name.
        ask_udp(
            served.udp,
            &query("slot-9-zzzzzzzz.tiles.fcvm.example", T_AAAA, Some(1232)),
        )
        .await;
        let first = logs.lines("launch name first asked");
        assert_eq!(first.len(), 1, "{first:?}");
        for want in [name, "qtype=28", "transport=\"udp\"", "verdict=\"answer\""] {
            assert!(first[0].contains(want), "{want} missing: {}", first[0]);
        }
        let in_zone: u64 = first[0]
            .split("in_zone_ms=")
            .nth(1)
            .and_then(|v| v.split_whitespace().next())
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("no in_zone_ms: {}", first[0]));
        assert!(
            in_zone >= 300,
            "the name was published 300 ms before it was asked: {}",
            first[0]
        );
    }

    #[test]
    fn udp_receive_errors_are_read_from_proc_net_snmp() {
        let snmp = "Ip: Forwarding DefaultTTL\nIp: 1 64\n\
                    Udp: InDatagrams NoPorts InErrors OutDatagrams RcvbufErrors SndbufErrors\n\
                    Udp: 1000 3 17 990 12 0\n\
                    UdpLite: InDatagrams NoPorts InErrors OutDatagrams RcvbufErrors SndbufErrors\n\
                    UdpLite: 0 0 0 0 0 0\n";
        assert_eq!(udp_errors(snmp), Some((17, 12)));
        assert_eq!(udp_errors("Ip: Forwarding\nIp: 1\n"), None);
    }

    /// Responses the rate limit truncated or withheld are counted and logged once their
    /// one-second window ends, so a resolver that got none can be checked against them.
    #[tokio::test(flavor = "current_thread")]
    async fn rate_limited_responses_are_logged_per_window() {
        let (logs, _guard) = Logs::capture();
        let (_tx, rx) = watch::channel(Arc::new(zone()));
        let served = Authority {
            cfg: Arc::new(cfg()),
            zone: rx,
        }
        .serve("127.0.0.1:0".parse().unwrap(), 1)
        .await
        .unwrap();
        let q = query("slot-1-abcdefgh.tiles.fcvm.example", T_AAAA, Some(1232));
        // Answered, slipped (TC), dropped.
        for _ in 0..3 {
            ask_udp(served.udp, &q).await;
        }
        tokio::time::sleep(Duration::from_millis(1100)).await;
        ask_udp(served.udp, &q).await;
        let limited = logs.lines("DNS responses rate limited");
        assert_eq!(limited.len(), 1, "{limited:?}");
        assert!(limited[0].contains("slipped=1"), "{}", limited[0]);
        assert!(limited[0].contains("dropped=1"), "{}", limited[0]);
    }

    fn cfg() -> ZoneConfig {
        ZoneConfig::new(
            "tiles.fcvm.example",
            &[
                "ns1-tiles.fcvm.example".into(),
                "ns2-tiles.fcvm.example".into(),
            ],
            Acme::Cname("_acme-challenge.fcvm.example".into()),
        )
        .unwrap()
    }

    fn zone() -> Zone {
        let mut z = Zone {
            serial: 7,
            ..Zone::default()
        };
        z.aaaa.insert(
            "slot-1-abcdefgh.tiles.fcvm.example".into(),
            vec!["2001:db8::1".parse().unwrap()],
        );
        z.a.insert(
            "slot-1-abcdefgh-v4.tiles.fcvm.example".into(),
            vec!["192.0.2.1".parse().unwrap()],
        );
        z
    }

    /// A query: id 0xBEEF, RD, one question, and an OPT record with `edns` as its size.
    fn query(name: &str, qtype: u16, edns: Option<u16>) -> Vec<u8> {
        let mut m = vec![
            0xBE,
            0xEF,
            0x01,
            0x00,
            0,
            1,
            0,
            0,
            0,
            0,
            0,
            u8::from(edns.is_some()),
        ];
        put_name(&mut m, name);
        m.extend_from_slice(&qtype.to_be_bytes());
        m.extend_from_slice(&1u16.to_be_bytes());
        if let Some(size) = edns {
            m.push(0);
            m.extend_from_slice(&T_OPT.to_be_bytes());
            m.extend_from_slice(&size.to_be_bytes());
            m.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        }
        m
    }

    /// What a response says, read independently of the code under test.
    #[derive(Debug)]
    struct Read {
        id: u16,
        qr: bool,
        aa: bool,
        tc: bool,
        ra: bool,
        rcode: u16,
        qd: u16,
        /// (type, ttl, rdata) of every answer record.
        answer: Vec<(u16, u32, Vec<u8>)>,
        authority: Vec<(u16, u32, Vec<u8>)>,
        opt: Option<u16>,
    }

    fn skip(m: &[u8], mut i: usize) -> usize {
        loop {
            let l = m[i] as usize;
            if l & 0xC0 == 0xC0 {
                return i + 2;
            }
            if l == 0 {
                return i + 1;
            }
            i += 1 + l;
        }
    }

    fn read(m: &[u8]) -> Read {
        let u = |i: usize| u16::from_be_bytes([m[i], m[i + 1]]);
        let flags = u(2);
        let (qd, an, ns, ar) = (u(4), u(6), u(8), u(10));
        let mut i = 12;
        for _ in 0..qd {
            i = skip(m, i) + 4;
        }
        let section = |n: u16, i: &mut usize| {
            let mut out = Vec::new();
            for _ in 0..n {
                let j = skip(m, *i);
                let len = u(j + 8) as usize;
                let ttl = u32::from_be_bytes([m[j + 4], m[j + 5], m[j + 6], m[j + 7]]);
                out.push((u(j), ttl, m[j + 10..j + 10 + len].to_vec()));
                *i = j + 10 + len;
            }
            out
        };
        let answer = section(an, &mut i);
        let authority = section(ns, &mut i);
        let additional = section(ar, &mut i);
        assert_eq!(i, m.len(), "trailing bytes");
        let opt = (ar > 0).then(|| {
            assert_eq!(additional[0].0, T_OPT);
            // The OPT's class is its payload size: read it back from the record.
            let j = m.len() - 11;
            u(j + 3)
        });
        Read {
            id: u(0),
            qr: flags & 0x8000 != 0,
            aa: flags & 0x0400 != 0,
            tc: flags & 0x0200 != 0,
            ra: flags & 0x0080 != 0,
            rcode: flags & 0xF,
            qd,
            answer,
            authority,
            opt,
        }
    }

    fn ask(name: &str, qtype: u16) -> Read {
        read(&respond(&cfg(), &zone(), &query(name, qtype, None), false).unwrap())
    }

    #[test]
    fn a_published_launch_answers_with_its_addresses() {
        let r = ask("slot-1-abcdefgh.tiles.fcvm.example", T_AAAA);
        assert!(r.qr && r.aa && !r.tc && !r.ra, "{r:?}");
        assert_eq!((r.id, r.rcode, r.qd), (0xBEEF, 0, 1));
        let v6: Ipv6Addr = "2001:db8::1".parse().unwrap();
        assert_eq!(r.answer, vec![(T_AAAA, RECORD_TTL, v6.octets().to_vec())]);
        assert!(r.authority.is_empty());
        // Case does not matter, and is echoed as asked.
        let m = query("SLOT-1-abcdefgh-V4.Tiles.FCVM.example", T_A, None);
        let out = respond(&cfg(), &zone(), &m, false).unwrap();
        assert_eq!(
            &out[12..m.len()],
            &m[12..],
            "the question is not echoed as sent"
        );
        let r = read(&out);
        assert_eq!(r.answer, vec![(T_A, RECORD_TTL, vec![192, 0, 2, 1])]);
        assert!(r.aa);
    }

    #[test]
    fn a_missing_name_is_nxdomain_and_a_missing_type_nodata_with_the_soa() {
        let r = ask("slot-2-zzzzzzzz.tiles.fcvm.example", T_AAAA);
        assert!(r.aa, "{r:?}");
        assert_eq!(r.rcode, NXDOMAIN);
        assert!(r.answer.is_empty());
        assert_eq!(r.authority.len(), 1);
        assert_eq!(r.authority[0].0, T_SOA);
        assert_eq!(r.authority[0].1, NEGATIVE_TTL);
        let soa = &r.authority[0].2;
        let minimum = u32::from_be_bytes(soa[soa.len() - 4..].try_into().unwrap());
        assert_eq!(minimum, NEGATIVE_TTL);
        // The name exists with an AAAA record only.
        let r = ask("slot-1-abcdefgh.tiles.fcvm.example", T_A);
        assert_eq!((r.rcode, r.aa), (0, true), "{r:?}");
        assert!(r.answer.is_empty());
        assert_eq!(r.authority[0].0, T_SOA);
        // Below a launch name there is nothing.
        let r = ask("x.slot-1-abcdefgh.tiles.fcvm.example", T_AAAA);
        assert_eq!(r.rcode, NXDOMAIN);
    }

    #[test]
    fn with_lambda_tiles_the_tunnel_name_is_the_coordinators_addresses() {
        let mut c = fleet_core::state::Config {
            ec2_aggregate_ipv6: Some("2001:db8::9".into()),
            ec2_aggregate_ipv4: Some("192.0.2.9".into()),
            ..fleet_core::state::Config::default()
        };
        let none = zone_of(&State::new(c.clone(), 0), "tiles.fcvm.example");
        assert!(!none.aaaa.contains_key("tunnel.tiles.fcvm.example"));
        c.lambda_max = 2;
        let z = zone_of(&State::new(c, 0), "tiles.fcvm.example");
        assert_eq!(
            z.aaaa["tunnel.tiles.fcvm.example"],
            vec!["2001:db8::9".parse::<Ipv6Addr>().unwrap()]
        );
        assert_eq!(
            z.a["tunnel.tiles.fcvm.example"],
            vec!["192.0.2.9".parse::<std::net::Ipv4Addr>().unwrap()]
        );
    }

    #[test]
    fn the_apex_answers_soa_and_ns() {
        let r = ask("tiles.fcvm.example", T_SOA);
        assert_eq!((r.rcode, r.aa, r.answer.len()), (0, true, 1), "{r:?}");
        let soa = &r.answer[0].2;
        let serial = u32::from_be_bytes(soa[soa.len() - 20..soa.len() - 16].try_into().unwrap());
        assert_eq!(serial, 7);
        let r = ask("tiles.fcvm.example", T_NS);
        assert_eq!(r.answer.len(), 2, "{r:?}");
        let mut ns1 = Vec::new();
        put_name(&mut ns1, "ns1-tiles.fcvm.example");
        assert_eq!(r.answer[0], (T_NS, NS_TTL, ns1));
        let r = ask("tiles.fcvm.example", T_A);
        assert_eq!((r.rcode, r.answer.len(), r.authority.len()), (0, 0, 1));
    }

    #[test]
    fn the_acme_challenge_is_a_cname_into_the_parent_zone() {
        for t in [16u16, T_CNAME, T_A] {
            let r = ask("_acme-challenge.tiles.fcvm.example", t);
            let mut target = Vec::new();
            put_name(&mut target, "_acme-challenge.fcvm.example");
            assert_eq!((r.rcode, r.aa), (0, true), "{r:?}");
            assert_eq!(r.answer, vec![(T_CNAME, NS_TTL, target)]);
        }
        let no_acme = ZoneConfig {
            acme: Acme::None,
            ..cfg()
        };
        let m = query("_acme-challenge.tiles.fcvm.example", 16, None);
        let r = read(&respond(&no_acme, &zone(), &m, false).unwrap());
        assert_eq!(r.rcode, NXDOMAIN);
    }

    /// A subzone certificate's challenge is answered here: the renewal's TXT values, with no
    /// CNAME into the parent zone; before and after the order the name does not exist.
    #[test]
    fn a_subzone_certificate_challenge_is_answered_with_its_txt_values() {
        let txt = ChallengeTxt::new("_acme-challenge.tiles.fcvm.example");
        let own = ZoneConfig::new(
            "tiles.fcvm.example",
            &["ns1-tiles.fcvm.example".into()],
            Acme::Txt(txt.clone()),
        )
        .unwrap();
        let ask_own = |t| {
            let m = query("_acme-challenge.tiles.fcvm.example", t, None);
            read(&respond(&own, &zone(), &m, false).unwrap())
        };
        assert_eq!(ask_own(T_TXT).rcode, NXDOMAIN);
        let mut w = txt.clone();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        use super::super::cert::Dns01;
        rt.block_on(w.add_txt("_acme-challenge.tiles.fcvm.example", "abc"))
            .unwrap();
        let r = ask_own(T_TXT);
        assert_eq!((r.rcode, r.aa), (0, true), "{r:?}");
        assert_eq!(r.answer, vec![(T_TXT, TXT_TTL, b"\x03abc".to_vec())]);
        // Another type at the name: NODATA, not a CNAME.
        let r = ask_own(T_CNAME);
        assert_eq!((r.rcode, r.answer.len()), (0, 0), "{r:?}");
        rt.block_on(w.delete("abc")).unwrap();
        assert_eq!(ask_own(T_TXT).rcode, NXDOMAIN);
        // A challenge that is not the subzone's own name is refused.
        assert!(ZoneConfig::new(
            "tiles.fcvm.example",
            &["ns1-tiles.fcvm.example".into()],
            Acme::Txt(ChallengeTxt::new("_acme-challenge.fcvm.example")),
        )
        .is_err());
    }

    #[test]
    fn names_outside_the_zone_are_refused_without_recursion() {
        for name in [
            "fcvm.example",
            "_acme-challenge.fcvm.example",
            "example.com",
            "xtiles.fcvm.example",
            "",
        ] {
            let r = ask(name, T_A);
            assert_eq!((r.rcode, r.aa, r.ra), (REFUSED, false, false), "{name}");
            assert!(r.answer.is_empty() && r.authority.is_empty());
        }
        // A zone transfer, and the CHAOS class, are refused too.
        assert_eq!(ask("tiles.fcvm.example", T_AXFR).rcode, REFUSED);
        let mut m = query("tiles.fcvm.example", T_SOA, None);
        let n = m.len();
        m[n - 1] = 3;
        assert_eq!(
            read(&respond(&cfg(), &zone(), &m, false).unwrap()).rcode,
            REFUSED
        );
    }

    #[test]
    fn edns_is_answered_and_an_oversized_udp_answer_is_truncated() {
        let m = query("tiles.fcvm.example", T_NS, Some(4096));
        let r = read(&respond(&cfg(), &zone(), &m, false).unwrap());
        assert_eq!(r.opt, Some(EDNS_SIZE));
        assert_eq!(r.answer.len(), 2);
        // An answer that does not fit the payload goes out with TC and no records.
        let out = respond_within(&cfg(), &zone(), &m, Some(60)).unwrap();
        assert!(out.len() <= 60, "{}", out.len());
        let r = read(&out);
        assert!(
            r.tc && r.answer.is_empty() && r.authority.is_empty(),
            "{r:?}"
        );
        // Over TCP the same answer is whole.
        let r = read(&respond(&cfg(), &zone(), &m, true).unwrap());
        assert!(!r.tc && r.answer.len() == 2);
        // An EDNS version other than 0 is BADVERS (16: extended bits in the OPT).
        let mut v1 = m.clone();
        let n = v1.len();
        v1[n - 5] = 1;
        let out = respond(&cfg(), &zone(), &v1, false).unwrap();
        assert_eq!(out[out.len() - 6], 1, "extended rcode");
        assert_eq!(out[3] & 0xF, 0);
    }

    #[test]
    fn malformed_packets_are_refused_or_dropped_without_panic() {
        let cfg = cfg();
        let z = zone();
        // Too short, or a response: nothing.
        assert!(respond(&cfg, &z, &[], false).is_none());
        assert!(respond(&cfg, &z, &[1, 2, 3], false).is_none());
        let mut resp = query("tiles.fcvm.example", T_A, None);
        resp[2] |= 0x80;
        assert!(respond(&cfg, &z, &resp, false).is_none());
        // A question whose name is a pointer to itself, and one that points backwards.
        let mut looped = vec![
            0xBE, 0xEF, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0xC0, 12, 0, 1, 0, 1,
        ];
        assert_eq!(
            read(&respond(&cfg, &z, &looped, false).unwrap()).rcode,
            FORMERR
        );
        looped[12..14].copy_from_slice(&[0xC0, 0]);
        assert_eq!(
            read(&respond(&cfg, &z, &looped, false).unwrap()).rcode,
            FORMERR
        );
        // A label longer than the packet, a name over 255 bytes, a label over 63.
        let cut = vec![0xBE, 0xEF, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 40, b'a'];
        assert_eq!(
            read(&respond(&cfg, &z, &cut, false).unwrap()).rcode,
            FORMERR
        );
        let long = query(&vec!["a".repeat(60); 5].join("."), T_A, None);
        assert_eq!(
            read(&respond(&cfg, &z, &long, false).unwrap()).rcode,
            FORMERR
        );
        let mut wide = vec![0xBE, 0xEF, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 64];
        wide.extend(std::iter::repeat_n(b'a', 64));
        wide.extend_from_slice(&[0, 0, 1, 0, 1]);
        assert_eq!(
            read(&respond(&cfg, &z, &wide, false).unwrap()).rcode,
            FORMERR
        );
        // Two questions; an OPT whose length runs past the end; a non-query opcode.
        let mut two = query("tiles.fcvm.example", T_A, None);
        two[5] = 2;
        assert_eq!(
            read(&respond(&cfg, &z, &two, false).unwrap()).rcode,
            FORMERR
        );
        let mut opt = query("tiles.fcvm.example", T_A, Some(1232));
        let n = opt.len();
        opt[n - 1] = 200;
        assert_eq!(
            read(&respond(&cfg, &z, &opt, false).unwrap()).rcode,
            FORMERR
        );
        let mut notify = query("tiles.fcvm.example", T_SOA, None);
        notify[2] = 4 << 3;
        assert_eq!(
            read(&respond(&cfg, &z, &notify, false).unwrap()).rcode,
            NOTIMP
        );
        // Longer than any query: nothing.
        assert!(respond(&cfg, &z, &vec![0; MAX_QUERY + 1], false).is_none());
    }

    #[test]
    fn random_packets_never_panic_and_every_response_is_well_formed() {
        use rand::{Rng, SeedableRng};
        let cfg = cfg();
        let z = zone();
        let mut rng = rand::rngs::StdRng::seed_from_u64(1256);
        let seeds = [
            query("slot-1-abcdefgh.tiles.fcvm.example", T_AAAA, Some(1232)),
            query("tiles.fcvm.example", T_NS, None),
            query("_acme-challenge.tiles.fcvm.example", 16, Some(512)),
        ];
        for round in 0..200_000 {
            let m: Vec<u8> = if round % 2 == 0 {
                let len = rng.random_range(0..80);
                (0..len).map(|_| rng.random()).collect()
            } else {
                let mut m = seeds[round % seeds.len()].clone();
                for _ in 0..rng.random_range(1..4) {
                    let i = rng.random_range(0..m.len());
                    m[i] = rng.random();
                }
                m.truncate(rng.random_range(0..=m.len()));
                m
            };
            for tcp in [false, true] {
                if let Some(out) = respond(&cfg, &z, &m, tcp) {
                    let r = read(&out);
                    assert!(r.qr, "{m:?}");
                    assert_eq!(r.id, u16::from_be_bytes([m[0], m[1]]));
                    assert!(tcp || out.len() <= EDNS_SIZE as usize);
                }
            }
        }
    }

    #[test]
    fn the_rate_limit_slips_and_drops_past_the_limit_per_prefix() {
        let mut rl = RateLimit::new(3);
        let t = Instant::now();
        let a: IpAddr = "198.51.100.7".parse().unwrap();
        let same: IpAddr = "198.51.100.200".parse().unwrap();
        let other: IpAddr = "203.0.113.1".parse().unwrap();
        for _ in 0..3 {
            assert_eq!(rl.check(a, t), Verdict::Answer);
        }
        let over: Vec<_> = (0..4).map(|_| rl.check(same, t)).collect();
        assert_eq!(
            over,
            vec![Verdict::Slip, Verdict::Drop, Verdict::Slip, Verdict::Drop]
        );
        assert_eq!(rl.check(other, t), Verdict::Answer);
        // IPv6: one /56 shares a budget; the next /56 has its own.
        let v6 = |s: &str| s.parse::<IpAddr>().unwrap();
        for _ in 0..3 {
            assert_eq!(rl.check(v6("2001:db8:0:ff::1"), t), Verdict::Answer);
        }
        assert_ne!(rl.check(v6("2001:db8:0:1::9"), t), Verdict::Answer);
        assert_eq!(rl.check(v6("2001:db8:0:100::1"), t), Verdict::Answer);
        // An IPv4-mapped address counts as its IPv4 prefix.
        assert_ne!(rl.check(v6("::ffff:198.51.100.9"), t), Verdict::Answer);
        // The next second starts over.
        assert_eq!(rl.check(a, t + Duration::from_secs(1)), Verdict::Answer);
        // A slip is never larger than the query.
        let m = query("tiles.fcvm.example", T_NS, Some(1232));
        let out = truncated(&cfg(), &zone(), &m).unwrap();
        assert!(out.len() <= m.len(), "{} > {}", out.len(), m.len());
        assert!(read(&out).tc);
    }

    #[test]
    fn the_rate_limit_table_is_bounded() {
        let mut rl = RateLimit::new(1000);
        let t = Instant::now();
        for i in 0..RATE_PREFIXES as u32 {
            let ip = IpAddr::V4(Ipv4Addr::from(i << 8));
            assert_eq!(rl.check(ip, t), Verdict::Answer);
        }
        let late = IpAddr::V4(Ipv4Addr::from((RATE_PREFIXES as u32 + 1) << 8));
        assert_ne!(rl.check(late, t), Verdict::Answer);
        assert_eq!(rl.counts.len(), RATE_PREFIXES);
    }

    #[test]
    fn the_zone_config_is_checked() {
        assert!(ZoneConfig::new("tiles.fcvm.example.", &[], Acme::None).is_err());
        assert!(
            ZoneConfig::new("ti les.fcvm.example", &["ns.example".into()], Acme::None).is_err()
        );
        assert!(ZoneConfig::new(
            "tiles.fcvm.example",
            &["ns.example".into()],
            Acme::Cname("_acme-challenge.tiles.fcvm.example".into())
        )
        .is_err());
        let c =
            ZoneConfig::new("Tiles.FCVM.example.", &["NS1.example".into()], Acme::None).unwrap();
        assert_eq!(
            (c.apex.as_str(), c.nameservers[0].as_str()),
            ("tiles.fcvm.example", "ns1.example")
        );
    }
}
