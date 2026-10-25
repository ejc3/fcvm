//! The fleet's DNS names in its zone, written through the Cloudflare API with the zone's DNS
//! token. Every launch of an fcvm slot or EC2 tile gets a label never used before,
//! `slot-<n>-<token>` / `ec2-<n>-<token>` (`token`: 8 random base32 characters), and two
//! names under it, both unproxied with TTL 60:
//!
//! - `<label>.<zone>` AAAA: the slot's own IPv6 address (a routed VM's /128, a tile's).
//! - `<label>-v4.<zone>` A: an IPv4 address: a GPU host's public address, shared by its
//!   slots, each forwarded on its own port (`SlotReport::v4_port`), or a tile's own.
//!
//! A host's aggregator is named the same way under `agg-<host>-<token>`
//! (`fleet_core::state::aggregate_id`): its own IPv6 address (port 443) and the host's IPv4
//! address on the aggregator's port. Its label lasts while it reports the same addresses.
//!
//! The order is: write the records, confirm them at the zone's authoritative servers, and
//! only then tell the coordinator (`Event::Published`), which puts the frame URLs in the
//! state the page reads. A name is never asked for before it exists, so no resolver holds a
//! negative answer for it, and never changes address, so none holds a stale positive one.
//! A launch's records stay until the slot has a newer published launch or leaves the fleet;
//! names of this shape that nothing references are deleted, also at start. No record is
//! proxied: the frames go straight to the slot.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use fleet_core::state::Published;
use fleet_core::{Event, State};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// The wanted records by (name, kind).
pub type Wanted = BTreeMap<(String, String), Record>;

/// One wanted record.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Record {
    pub name: String,
    /// `A` or `AAAA`.
    pub kind: String,
    pub content: String,
    pub proxied: bool,
}

/// What the DNS task needs about one slot or tile.
#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    /// `slot-<n>` or `ec2-<n>`.
    pub id: String,
    /// The launch it is on (its observed generation).
    pub generation: u64,
    pub ipv6: Option<String>,
    pub ipv4: Option<String>,
    pub v4_port: Option<u16>,
    pub published: Option<Published>,
}

/// The facts of every slot, tile and host aggregator in the fleet. An aggregator has one
/// launch (generation 0) for as long as it reports the same addresses.
pub fn facts(state: &State) -> Vec<Fact> {
    let f = &state.fleet;
    let units = f.slots.values().chain(f.ec2.values()).map(|s| Fact {
        id: s.id.clone(),
        generation: s.observed.generation,
        ipv6: s.observed.ipv6.clone(),
        ipv4: s.observed.ipv4.clone(),
        v4_port: s.observed.v4_port,
        published: s.published.clone(),
    });
    let aggregators = f.hosts.values().filter_map(|h| {
        Some(Fact {
            id: fleet_core::state::aggregate_id(&h.name)?,
            generation: 0,
            ipv6: h.aggregate_ipv6.clone(),
            ipv4: h.aggregate_ipv4.clone(),
            v4_port: h.aggregate_v4_port,
            published: h.aggregate_published.clone(),
        })
    });
    units.chain(aggregators).collect()
}

// Addresses compare in their canonical text, so the record, the published names and an
// authoritative answer agree on one address however it was written.
use fleet_core::state::canonical_ip as canonical;

/// Whether `p` names exactly the launch `f` is on: its generation, its addresses and the
/// IPv4 port in its URL. Anything else is a new launch, with a new label.
pub fn is_current(zone: &str, p: &Published, f: &Fact) -> bool {
    *p == names(
        zone,
        &p.label,
        f.generation,
        f.ipv6.as_deref(),
        f.ipv4.as_deref(),
        f.v4_port,
    )
}

/// A launch's names: the label, its addresses and the frame URLs they give.
pub fn names(
    zone: &str,
    label: &str,
    generation: u64,
    ipv6: Option<&str>,
    ipv4: Option<&str>,
    v4_port: Option<u16>,
) -> Published {
    let port = match v4_port {
        Some(p) if p != 443 => format!(":{p}"),
        _ => String::new(),
    };
    Published {
        generation,
        label: label.into(),
        ipv6: ipv6.map(canonical),
        ipv4: ipv4.map(canonical),
        v4_port,
        frame_url: ipv6.map(|_| format!("wss://{label}.{zone}/frames")),
        frame_url_v4: ipv4.map(|_| format!("wss://{label}-v4.{zone}{port}/frames")),
    }
}

/// The records of one launch's names.
fn records(zone: &str, p: &Published) -> Vec<((String, String), Record)> {
    let mut out = Vec::new();
    let mut add = |name: String, kind: &str, content: &str| {
        out.push((
            (name.clone(), kind.to_string()),
            Record {
                name,
                kind: kind.into(),
                content: content.into(),
                proxied: false,
            },
        ))
    };
    if let Some(v6) = &p.ipv6 {
        add(format!("{}.{zone}", p.label), "AAAA", v6);
    }
    if let Some(v4) = &p.ipv4 {
        add(format!("{}-v4.{zone}", p.label), "A", v4);
    }
    out
}

/// An address a slot reported that is not one of its family (`ipv6` must be IPv6, `ipv4`
/// IPv4): naming it would only publish a record nobody can use.
pub fn bad_address(f: &Fact) -> Option<&str> {
    [(&f.ipv6, true), (&f.ipv4, false)]
        .into_iter()
        .find(|(a, v6)| {
            a.as_ref()
                .is_some_and(|a| match a.parse::<std::net::IpAddr>() {
                    Ok(ip) => ip.is_ipv6() != *v6,
                    Err(_) => true,
                })
        })
        .and_then(|(a, _)| a.as_deref())
}

/// Whether a slot needs names for the launch it is on: it has an address, every address it
/// reported is one, and it has no published names for exactly this launch.
pub fn needs_names(zone: &str, f: &Fact) -> bool {
    (f.ipv6.is_some() || f.ipv4.is_some())
        && bad_address(f).is_none()
        && !f.published.as_ref().is_some_and(|p| is_current(zone, p, f))
}

/// A label for one launch of `id`, never made before (8 random base32 characters).
pub fn mint(id: &str) -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut r = rand::thread_rng();
    let token: String = (0..8)
        .map(|_| ALPHABET[r.gen_range(0..ALPHABET.len())] as char)
        .collect();
    format!("{id}-{token}")
}

/// A launch token: 8 characters of the base32 alphabet.
fn is_token(t: &str) -> bool {
    t.len() == 8
        && t.bytes()
            .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
}

/// Whether a name in the zone is one this module owns: `slot-<n>` or `ec2-<n>`, optionally
/// with a launch token, or `agg-<host>-<token>` (an aggregator), each optionally with
/// `-v4`. Anything else in the zone is never touched.
fn owned(name: &str, zone: &str) -> bool {
    let Some(label) = name.strip_suffix(&format!(".{zone}")) else {
        return false;
    };
    let label = label.strip_suffix("-v4").unwrap_or(label);
    if let Some(rest) = label.strip_prefix("agg-") {
        return rest.rsplit_once('-').is_some_and(|(host, token)| {
            fleet_core::state::aggregate_id(host).is_some() && is_token(token)
        });
    }
    let Some(rest) = label
        .strip_prefix("slot-")
        .or_else(|| label.strip_prefix("ec2-"))
    else {
        return false;
    };
    let (num, token) = match rest.split_once('-') {
        Some((n, t)) => (n, Some(t)),
        None => (rest, None),
    };
    let digits = !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit());
    digits && token.is_none_or(is_token)
}

/// Whether the names of a launch answer at the zone's authoritative servers.
#[async_trait]
pub trait Confirm: Send + Sync {
    async fn confirmed(&self, records: &[Record]) -> bool;
}

/// Asks each of the zone's authoritative servers (`--dns-nameservers`) directly, over UDP,
/// for every record; all must answer with its address. With no servers given it confirms
/// at once (local runs).
pub struct Authoritative {
    pub servers: Vec<String>,
}

#[async_trait]
impl Confirm for Authoritative {
    async fn confirmed(&self, records: &[Record]) -> bool {
        for server in &self.servers {
            for r in records {
                match query(server, &r.name, &r.kind).await {
                    Ok(addrs) if addrs.iter().any(|a| canonical(a) == canonical(&r.content)) => {}
                    Ok(addrs) => {
                        tracing::debug!(server, name = %r.name, ?addrs, "not yet at the authoritative server");
                        return false;
                    }
                    Err(e) => {
                        tracing::warn!(server, name = %r.name, error = %format!("{e:#}"), "authoritative query failed");
                        return false;
                    }
                }
            }
        }
        true
    }
}

/// The addresses a nameserver answers for `name` (`A` or `AAAA`), asked without recursion.
pub async fn query(server: &str, name: &str, kind: &str) -> Result<Vec<String>> {
    let qtype: u16 = if kind == "AAAA" { 28 } else { 1 };
    let id: u16 = rand::random();
    let mut q = Vec::with_capacity(64);
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
    for part in name.trim_end_matches('.').split('.') {
        q.push(part.len() as u8);
        q.extend_from_slice(part.as_bytes());
    }
    q.push(0);
    q.extend_from_slice(&qtype.to_be_bytes());
    q.extend_from_slice(&1u16.to_be_bytes());
    let addr = tokio::net::lookup_host((server, 53))
        .await
        .with_context(|| format!("resolving {server}"))?
        .next()
        .with_context(|| format!("no address for {server}"))?;
    let sock = tokio::net::UdpSocket::bind(if addr.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    })
    .await?;
    sock.connect(addr).await?;
    sock.send(&q).await?;
    let mut buf = [0u8; 1500];
    let n = tokio::time::timeout(Duration::from_secs(3), sock.recv(&mut buf))
        .await
        .context("no answer within 3 s")??;
    parse_answers(&buf[..n], id, qtype)
}

fn parse_answers(m: &[u8], id: u16, qtype: u16) -> Result<Vec<String>> {
    let u16_at = |i: usize| -> Result<u16> {
        Ok(u16::from_be_bytes([
            *m.get(i).context("short answer")?,
            *m.get(i + 1).context("short answer")?,
        ]))
    };
    if u16_at(0)? != id {
        bail!("answer for another query");
    }
    let flags = u16_at(2)?;
    let rcode = flags & 0xF;
    if rcode == 3 {
        return Ok(Vec::new()); // NXDOMAIN
    }
    if rcode != 0 {
        bail!("rcode {rcode}");
    }
    // Only the zone's own server may confirm a launch name; a cache or forwarder answers
    // NOERROR with AA clear.
    if flags & 0x0400 == 0 {
        bail!("answer is not authoritative (AA bit clear)");
    }
    let (qd, an) = (u16_at(4)?, u16_at(6)?);
    let skip_name = |mut i: usize| -> Result<usize> {
        loop {
            let len = *m.get(i).context("short answer")?;
            if len & 0xC0 == 0xC0 {
                return Ok(i + 2);
            }
            if len == 0 {
                return Ok(i + 1);
            }
            i += 1 + len as usize;
        }
    };
    let mut i = 12;
    for _ in 0..qd {
        i = skip_name(i)? + 4;
    }
    let mut out = Vec::new();
    for _ in 0..an {
        i = skip_name(i)?;
        let (t, len) = (u16_at(i)?, u16_at(i + 8)? as usize);
        let rdata = m.get(i + 10..i + 10 + len).context("short answer")?;
        if t == qtype && t == 1 && len == 4 {
            out.push(std::net::Ipv4Addr::new(rdata[0], rdata[1], rdata[2], rdata[3]).to_string());
        } else if t == qtype && t == 28 && len == 16 {
            let b: [u8; 16] = rdata.try_into().unwrap();
            out.push(std::net::Ipv6Addr::from(b).to_string());
        }
        i += 10 + len;
    }
    Ok(out)
}

pub struct Cloudflare {
    token: String,
    pub zone: String,
    zone_id: Option<String>,
    base: String,
    http: reqwest::Client,
    /// How a launch's records are confirmed before they are published.
    confirm: Arc<dyn Confirm>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct Existing {
    id: String,
    name: String,
    #[serde(rename = "type")]
    kind: String,
    content: String,
    #[serde(default)]
    proxied: bool,
}

impl Cloudflare {
    pub fn new(token: String, zone: String, zone_id: Option<String>) -> Result<Self> {
        Cloudflare::with_endpoint(token, zone, zone_id, "https://api.cloudflare.com/client/v4")
    }

    pub fn with_endpoint(
        token: String,
        zone: String,
        zone_id: Option<String>,
        base: &str,
    ) -> Result<Self> {
        Ok(Cloudflare {
            token,
            zone,
            zone_id,
            base: base.trim_end_matches('/').into(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()?,
            confirm: Arc::new(Authoritative {
                servers: Vec::new(),
            }),
        })
    }

    /// Confirm launches' records with `confirm` before publishing them.
    pub fn with_confirm(mut self, confirm: Arc<dyn Confirm>) -> Self {
        self.confirm = confirm;
        self
    }

    async fn api(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let mut req = self
            .http
            .request(method.clone(), format!("{}{path}", self.base))
            .bearer_auth(&self.token);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let r = req
            .send()
            .await
            .with_context(|| format!("Cloudflare {method} {path}"))?;
        let status = r.status();
        let v: serde_json::Value = r.json().await.unwrap_or_default();
        if !status.is_success() || v.get("success").and_then(|s| s.as_bool()) == Some(false) {
            bail!(
                "Cloudflare {method} {path} answered {status}: {}",
                v.get("errors").cloned().unwrap_or_default()
            );
        }
        Ok(v)
    }

    pub async fn zone_id(&mut self) -> Result<String> {
        if let Some(id) = &self.zone_id {
            return Ok(id.clone());
        }
        let v = self
            .api(
                reqwest::Method::GET,
                &format!("/zones?name={}", self.zone),
                None,
            )
            .await?;
        let id = v["result"][0]["id"]
            .as_str()
            .with_context(|| format!("no Cloudflare zone named {}", self.zone))?
            .to_string();
        self.zone_id = Some(id.clone());
        Ok(id)
    }

    async fn existing(&mut self) -> Result<Vec<Existing>> {
        let zid = self.zone_id().await?;
        let mut out = Vec::new();
        for page in 1.. {
            let v = self
                .api(
                    reqwest::Method::GET,
                    &format!("/zones/{zid}/dns_records?per_page=500&page={page}"),
                    None,
                )
                .await?;
            let batch: Vec<Existing> =
                serde_json::from_value(v["result"].clone()).unwrap_or_default();
            let n = batch.len();
            out.extend(batch.into_iter().filter(|r| owned(&r.name, &self.zone)));
            let pages = v["result_info"]["total_pages"].as_u64().unwrap_or(1);
            if n == 0 || page as u64 >= pages {
                break;
            }
        }
        Ok(out)
    }

    /// Make the zone's owned records exactly `want`; returns the changes made.
    pub async fn reconcile(&mut self, want: &BTreeMap<(String, String), Record>) -> Result<usize> {
        let zid = self.zone_id().await?;
        let have = self.existing().await?;
        let mut changes = 0;
        let mut seen = std::collections::BTreeSet::new();
        for e in &have {
            let key = (e.name.clone(), e.kind.clone());
            match want.get(&key) {
                Some(w) if !seen.contains(&key) => {
                    seen.insert(key);
                    if w.content != e.content || w.proxied != e.proxied {
                        self.api(
                            reqwest::Method::PUT,
                            &format!("/zones/{zid}/dns_records/{}", e.id),
                            Some(body(w)),
                        )
                        .await?;
                        changes += 1;
                    }
                }
                // Unwanted, or a duplicate of one already kept.
                _ => {
                    self.api(
                        reqwest::Method::DELETE,
                        &format!("/zones/{zid}/dns_records/{}", e.id),
                        None,
                    )
                    .await?;
                    changes += 1;
                }
            }
        }
        for (key, w) in want {
            if !seen.contains(key) {
                self.api(
                    reqwest::Method::POST,
                    &format!("/zones/{zid}/dns_records"),
                    Some(body(w)),
                )
                .await?;
                changes += 1;
            }
        }
        Ok(changes)
    }

    /// A TXT record (the ACME DNS-01 challenge); returns its id.
    pub async fn add_txt(&mut self, name: &str, content: &str) -> Result<String> {
        let zid = self.zone_id().await?;
        let v = self
            .api(
                reqwest::Method::POST,
                &format!("/zones/{zid}/dns_records"),
                Some(
                    serde_json::json!({"type": "TXT", "name": name, "content": content, "ttl": 60}),
                ),
            )
            .await?;
        v["result"]["id"]
            .as_str()
            .map(str::to_string)
            .context("Cloudflare named no record")
    }

    pub async fn delete(&mut self, id: &str) -> Result<()> {
        let zid = self.zone_id().await?;
        self.api(
            reqwest::Method::DELETE,
            &format!("/zones/{zid}/dns_records/{id}"),
            None,
        )
        .await
        .map(|_| ())
    }
}

fn body(r: &Record) -> serde_json::Value {
    serde_json::json!({
        "type": r.kind,
        "name": r.name,
        "content": r.content,
        "proxied": r.proxied,
        "ttl": 60,
    })
}

/// Delete every launch name of the zone (`slot-<n>-<token>`, `ec2-<n>-<token>` and their
/// `-v4`), once: with the launch names served from a subzone, none of them is referenced.
pub async fn sweep(mut cf: Cloudflare) {
    match cf.reconcile(&Wanted::new()).await {
        Ok(n) => {
            tracing::info!(deleted = n, zone = %cf.zone, "launch names swept from the parent zone")
        }
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "launch names in the parent zone not swept")
        }
    }
}

/// One launch whose names are being written or confirmed.
struct Pending {
    names: Published,
    sent: bool,
}

/// The records the facts and the pending launches want: every slot's published names, kept
/// until it has newer ones or leaves the fleet, and the names being written.
fn wanted(zone: &str, facts: &[Fact], pending: &BTreeMap<String, Pending>) -> Wanted {
    let mut out = BTreeMap::new();
    for f in facts {
        if let Some(p) = &f.published {
            out.extend(records(zone, p));
        }
        if let Some(p) = pending.get(&f.id) {
            out.extend(records(zone, &p.names));
        }
    }
    out
}

/// Follow the facts: mint a label for each new launch that has an address, write its
/// records, confirm them, then publish them to the coordinator; delete names nothing
/// references. Retries a failed step.
pub async fn run(
    mut cf: Cloudflare,
    mut facts_rx: watch::Receiver<Vec<Fact>>,
    handle: super::Handle,
) {
    let confirm = cf.confirm.clone();
    let mut pending: BTreeMap<String, Pending> = BTreeMap::new();
    let mut done: Option<Wanted> = None;
    let mut backoff = Duration::from_secs(1);
    loop {
        let facts = facts_rx.borrow_and_update().clone();
        // Launches: a slot on a generation with an address and no published names for it
        // (or for these addresses) needs a label; a pending launch the slot moved past, or
        // whose names it has published, is dropped.
        let mut live = BTreeMap::new();
        for f in &facts {
            if let Some(a) = bad_address(f) {
                tracing::warn!(slot = %f.id, address = %a, "not naming a slot whose reported address is not one");
                continue;
            }
            if !needs_names(&cf.zone, f) {
                continue;
            }
            let p = pending
                .remove(&f.id)
                .filter(|p| is_current(&cf.zone, &p.names, f));
            let p = p.unwrap_or_else(|| Pending {
                names: names(
                    &cf.zone,
                    &mint(&f.id),
                    f.generation,
                    f.ipv6.as_deref(),
                    f.ipv4.as_deref(),
                    f.v4_port,
                ),
                sent: false,
            });
            live.insert(f.id.clone(), p);
        }
        pending = live;
        let want = wanted(&cf.zone, &facts, &pending);
        let mut failed = false;
        if done.as_ref() != Some(&want) {
            match cf.reconcile(&want).await {
                Ok(n) => {
                    if n > 0 {
                        tracing::info!(changes = n, records = want.len(), "fleet DNS updated");
                    }
                    done = Some(want);
                }
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "fleet DNS not updated; retrying");
                    failed = true;
                }
            }
        }
        // Publish what is written and answers at the authoritative servers.
        let mut waiting = false;
        if !failed {
            for (id, p) in pending.iter_mut().filter(|(_, p)| !p.sent) {
                let recs: Vec<Record> = records(&cf.zone, &p.names)
                    .into_iter()
                    .map(|(_, r)| r)
                    .collect();
                if confirm.confirmed(&recs).await {
                    tracing::info!(slot = %id, label = %p.names.label, "names published");
                    handle
                        .send(Event::Published {
                            at: 0,
                            slot: id.clone(),
                            names: p.names.clone(),
                        })
                        .await;
                    p.sent = true;
                } else {
                    waiting = true;
                }
            }
        }
        let wait = if failed {
            let w = backoff;
            backoff = (backoff * 2).min(Duration::from_secs(60));
            Some(w)
        } else {
            backoff = Duration::from_secs(1);
            waiting.then_some(Duration::from_secs(1))
        };
        match wait {
            Some(w) => {
                tokio::select! {
                    _ = tokio::time::sleep(w) => {}
                    r = facts_rx.changed() => if r.is_err() { return },
                }
            }
            None => {
                if facts_rx.changed().await.is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{mint, names, owned, parse_answers};

    #[test]
    fn only_fleet_names_are_owned() {
        let z = "fcvm.dev";
        for name in [
            "slot-3.fcvm.dev",
            "slot-12-v4.fcvm.dev",
            "ec2-1.fcvm.dev",
            "slot-3-abcdefgh.fcvm.dev",
            "slot-3-abcd2345-v4.fcvm.dev",
            "ec2-8-zzzzzzzz.fcvm.dev",
            "agg-i-0123456789abcdef0-abcdefgh.fcvm.dev",
            "agg-i-0123456789abcdef0-abcdefgh-v4.fcvm.dev",
            "agg-gpu-a-zzzz2345.fcvm.dev",
        ] {
            assert!(owned(name, z), "{name}");
        }
        for name in [
            "stage.fcvm.dev",
            "fleet-api-stage.fcvm.dev",
            "fleet-api.fcvm.dev",
            "fcvm.dev",
            "dnstest-1.fcvm.dev",
            "dnstest-slot-1.fcvm.dev",
            "slot-.fcvm.dev",
            "slot-3a.fcvm.dev",
            "slot-3-abc.fcvm.dev",
            "slot-3-ABCDEFGH.fcvm.dev",
            "slot-3-abcdefgh-x.fcvm.dev",
            "slot-3.other.dev",
            "agg-gpu-a.fcvm.dev",
            "agg-gpu-a-v4.fcvm.dev",
            "agg-abcdefgh.fcvm.dev",
            "agg--abcdefgh.fcvm.dev",
            "agg-gpu_a-abcdefgh.fcvm.dev",
            "agg-GPU-abcdefgh.fcvm.dev",
            "agg-gpu-a-abcdefg1.fcvm.dev",
            "dnstest-agg-gpu-a-abcdefgh.fcvm.dev",
        ] {
            assert!(!owned(name, z), "{name}");
        }
    }

    #[test]
    fn labels_are_owned_and_not_reused() {
        let mut seen = std::collections::BTreeSet::new();
        for id in ["slot-3", "agg-i-0123456789abcdef0"] {
            let l = mint(id);
            assert!(owned(&format!("{l}.fcvm.dev"), "fcvm.dev"), "{l}");
            assert!(owned(&format!("{l}-v4.fcvm.dev"), "fcvm.dev"), "{l}");
        }
        for _ in 0..1000 {
            let l = mint("slot-3");
            assert!(owned(&format!("{l}.fcvm.dev"), "fcvm.dev"), "{l}");
            assert!(owned(&format!("{l}-v4.fcvm.dev"), "fcvm.dev"), "{l}");
            assert!(seen.insert(l.clone()), "minted twice: {l}");
        }
    }

    #[test]
    fn a_launch_names_its_frame_urls_with_the_v4_port() {
        let p = names(
            "fcvm.dev",
            "slot-1-abcdefgh",
            3,
            Some("2001:db8::2"),
            Some("192.0.2.1"),
            Some(20200),
        );
        assert_eq!(
            p.frame_url.as_deref(),
            Some("wss://slot-1-abcdefgh.fcvm.dev/frames")
        );
        assert_eq!(
            p.frame_url_v4.as_deref(),
            Some("wss://slot-1-abcdefgh-v4.fcvm.dev:20200/frames")
        );
        let t = names(
            "fcvm.dev",
            "ec2-2-abcdefgh",
            1,
            None,
            Some("192.0.2.9"),
            Some(443),
        );
        assert_eq!(t.frame_url, None);
        assert_eq!(
            t.frame_url_v4.as_deref(),
            Some("wss://ec2-2-abcdefgh-v4.fcvm.dev/frames")
        );
    }

    #[test]
    fn addresses_are_canonical_and_a_new_port_is_a_new_launch() {
        use super::{is_current, Fact};
        let p = names(
            "fcvm.dev",
            "ec2-1-abcdefgh",
            2,
            Some("2001:0db8:0::2"),
            Some("192.0.2.1"),
            None,
        );
        assert_eq!(p.ipv6.as_deref(), Some("2001:db8::2"));
        let mut f = Fact {
            id: "ec2-1".into(),
            generation: 2,
            ipv6: Some("2001:db8::0:2".into()),
            ipv4: Some("192.0.2.1".into()),
            v4_port: None,
            published: None,
        };
        assert!(is_current("fcvm.dev", &p, &f));
        f.v4_port = Some(20201);
        assert!(
            !is_current("fcvm.dev", &p, &f),
            "a new IPv4 port must be a new launch"
        );
        f.v4_port = None;
        f.generation = 3;
        assert!(!is_current("fcvm.dev", &p, &f));
    }

    #[test]
    fn an_authoritative_answer_is_read() {
        // id 0x1234, QR|AA, rcode 0, 1 question, 1 answer: slot-1.fcvm.dev AAAA 2001:db8::2.
        let mut m = vec![0x12, 0x34, 0x84, 0x00, 0, 1, 0, 1, 0, 0, 0, 0];
        for part in ["slot-1", "fcvm", "dev"] {
            m.push(part.len() as u8);
            m.extend_from_slice(part.as_bytes());
        }
        m.extend_from_slice(&[0, 0, 28, 0, 1]);
        m.extend_from_slice(&[0xC0, 12, 0, 28, 0, 1, 0, 0, 0, 60, 0, 16]);
        m.extend_from_slice(
            &"2001:db8::2"
                .parse::<std::net::Ipv6Addr>()
                .unwrap()
                .octets(),
        );
        assert_eq!(
            parse_answers(&m, 0x1234, 28).unwrap(),
            vec!["2001:db8::2".to_string()]
        );
        assert!(parse_answers(&m, 0x9999, 28).is_err());
        // NXDOMAIN
        let mut nx = m[..12].to_vec();
        nx[3] = 3;
        nx[7] = 0;
        assert!(parse_answers(&nx, 0x1234, 28).unwrap().is_empty());
    }

    #[test]
    fn an_answer_without_the_aa_bit_is_refused() {
        // Same response as above with AA clear (flags QR only): a resolver's or cache's
        // answer, not the zone's own server.
        let mut m = vec![0x12, 0x34, 0x80, 0x00, 0, 1, 0, 1, 0, 0, 0, 0];
        for part in ["slot-1", "fcvm", "dev"] {
            m.push(part.len() as u8);
            m.extend_from_slice(part.as_bytes());
        }
        m.extend_from_slice(&[0, 0, 28, 0, 1]);
        m.extend_from_slice(&[0xC0, 12, 0, 28, 0, 1, 0, 0, 0, 60, 0, 16]);
        m.extend_from_slice(
            &"2001:db8::2"
                .parse::<std::net::Ipv6Addr>()
                .unwrap()
                .octets(),
        );
        let err = parse_answers(&m, 0x1234, 28).unwrap_err();
        assert!(format!("{err:#}").contains("not authoritative"), "{err:#}");
        // The same bytes with AA set are read.
        m[2] = 0x84;
        assert_eq!(
            parse_answers(&m, 0x1234, 28).unwrap(),
            vec!["2001:db8::2".to_string()]
        );
    }
}
