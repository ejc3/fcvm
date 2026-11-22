//! The IPv4 front of a GPU host: port 443 of the host's IPv4 address, routed by TLS server
//! name to the place the name is for. A GPU host has one public IPv4 address for all its places
//! (GPU slots and phones, Windows sandboxes, Minecraft servers' view sockets, the aggregator).
//! Each place keeps its own TLS listener on the host, on its port of
//! `fleet/published-ports.json` (`ports.rs`); the front reads the ClientHello's server name,
//! `<id>-<token>-v4.<zone>` (the place's launch name, `coord/dns.rs`), and relays the
//! connection, the ClientHello included, byte for byte to the listener of the place reporting
//! `<id>`. TLS ends in the place as before: the front holds no key and changes no byte. A viewer
//! whose network lets out only 80 and 443 reaches every place this way.
//!
//! The front learns the routes from the reports themselves ([`Front::publish`]): a place
//! reports its own listener's port as `v4_port`, the front records it under the place's id and
//! reports the front's port (443) in its stead, so the coordinator names every IPv4 path on
//! 443 and every mode is covered by the one place reports pass through. A port belongs to one
//! id at a time: a place that reports a new id (a relaunch under another slot) takes the port
//! from the id it had.
//!
//! A connection is refused (closed without a byte sent) when its first bytes are not a TLS
//! ClientHello, when the hello names no server, when the name is not a `-v4` launch name, or
//! when no place on this host reports its id.
//!
//! Every place sees the front's connections come from this host's own address, which the
//! renderer counts only against its total (fleet/PROTOCOL.md, "Connection limits"), so the
//! front counts them by their source (an IPv4 address) instead: connections still in their
//! ClientHello ([`MAX_HELLO_PER_SOURCE`]), relayed to one place
//! ([`MAX_RELAYED_PER_SOURCE_PLACE`]) and relayed in all ([`MAX_RELAYED_PER_SOURCE`]). One
//! past a cap is closed without a byte sent, and before the place is dialled.

use crate::network::source_limit::{Counts, Held, Source};
use anyhow::{bail, Context, Result};
use fleet_core::protocol::AgentMsg;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Semaphore};

/// How long a connection may take to send its whole ClientHello.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the front waits for a place's listener to accept.
pub const BACKEND_TIMEOUT: Duration = Duration::from_secs(5);
/// The largest ClientHello the front reads. A browser's, with a post-quantum key share, is
/// under 2 KiB; the cap bounds what one connection makes the agent hold.
pub const MAX_HELLO: usize = 16 * 1024;
/// Connections relayed at once. Each holds two descriptors of the agent's (its unit's
/// `LimitNOFILE` is 65536), and the agent needs descriptors of its own to run its places;
/// connections past the cap are closed at once.
pub const MAX_CONNECTIONS: usize = 8192;
/// Connections from one source that have not finished their ClientHello. A wall page opens a
/// socket to each place of this host it shows at once (up to 8 GPU slots and the aggregator),
/// and the address-family probe one more; 32 is that for about three viewers behind one
/// address joining in the same round trip. One client then needs 256 sources to hold all
/// [`MAX_CONNECTIONS`] idle.
pub const MAX_HELLO_PER_SOURCE: usize = 32;
/// Connections from one source relayed to one place at once, as the EC2 tile's nginx allows a
/// source on its one renderer (`fleet/ami/tile/nginx-renderer.conf`): a page holds at most 3
/// sockets to one place (its stream, the address-family probe's, and across a reload the old
/// page's stream until it closes), so 32 is about ten viewers behind one address, and one
/// client needs 16 sources to hold all 512 of a renderer's pending places.
pub const MAX_RELAYED_PER_SOURCE_PLACE: usize = 32;
/// Connections from one source relayed at once, to all of this host's places together:
/// [`MAX_CONNECTIONS`] / 32, so one client needs 32 sources to fill the front. Eight viewers
/// behind one address, each with 3 sockets to each of 9 places, hold 216.
pub const MAX_RELAYED_PER_SOURCE: usize = 256;

/// The front's per-source caps (tests use smaller ones).
#[derive(Debug, Clone, Copy)]
struct Limits {
    hello_per_source: usize,
    relayed_per_source_place: usize,
    relayed_per_source: usize,
}

const LIMITS: Limits = Limits {
    hello_per_source: MAX_HELLO_PER_SOURCE,
    relayed_per_source_place: MAX_RELAYED_PER_SOURCE_PLACE,
    relayed_per_source: MAX_RELAYED_PER_SOURCE,
};

/// What the first bytes of a connection say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hello {
    /// A ClientHello that is not complete yet.
    Incomplete,
    /// A complete ClientHello and its server name (lowercase, no trailing dot).
    Name(String),
    /// A complete ClientHello that names no server.
    NoName,
    /// Not a TLS ClientHello, or one over [`MAX_HELLO`].
    NotTls,
}

/// Read a ClientHello from the start of `buf`: the TLS records (content type 22, handshake)
/// that carry it, which may split it anywhere, then its server name extension (RFC 6066).
pub fn parse_hello(buf: &[u8]) -> Hello {
    // The handshake bytes, gathered from the records.
    let mut hs = Vec::new();
    let mut at = 0;
    loop {
        if hs.len() >= 4 {
            if hs[0] != 1 {
                return Hello::NotTls; // the first handshake message must be a ClientHello
            }
            let len = ((hs[1] as usize) << 16) | ((hs[2] as usize) << 8) | hs[3] as usize;
            if len > MAX_HELLO {
                return Hello::NotTls;
            }
            if hs.len() >= 4 + len {
                return server_name(&hs[4..4 + len]).unwrap_or(Hello::NotTls);
            }
        }
        let Some(header) = buf.get(at..at + 5) else {
            return Hello::Incomplete;
        };
        // Handshake records only, TLS 1.0 to 1.3 record versions (3.1 to 3.4; 3.1 is what
        // TLS 1.3 clients write), and no empty or over-long record (RFC 8446, 5.1).
        let len = ((header[3] as usize) << 8) | header[4] as usize;
        if header[0] != 22 || header[1] != 3 || len == 0 || len > 1 << 14 {
            return Hello::NotTls;
        }
        let Some(body) = buf.get(at + 5..at + 5 + len) else {
            return Hello::Incomplete;
        };
        hs.extend_from_slice(body);
        at += 5 + len;
    }
}

/// A reader over a ClientHello's body that refuses to read past it.
struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if n > self.0.len() {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }
    fn u8(&mut self) -> Option<usize> {
        self.take(1).map(|b| b[0] as usize)
    }
    fn u16(&mut self) -> Option<usize> {
        self.take(2).map(|b| ((b[0] as usize) << 8) | b[1] as usize)
    }
    /// A field whose length is given by the `width`-byte prefix before it.
    fn vec(&mut self, width: usize) -> Option<Cursor<'a>> {
        let n = if width == 1 { self.u8()? } else { self.u16()? };
        self.take(n).map(Cursor)
    }
}

/// The server name of a ClientHello body; `None` when the body is malformed.
fn server_name(body: &[u8]) -> Option<Hello> {
    let mut c = Cursor(body);
    c.take(2 + 32)?; // legacy_version, random
    c.vec(1)?; // legacy_session_id
    c.vec(2)?; // cipher_suites
    c.vec(1)?; // legacy_compression_methods
    if c.0.is_empty() {
        return Some(Hello::NoName); // a hello with no extensions
    }
    let mut ext = c.vec(2)?;
    if !c.0.is_empty() {
        return None;
    }
    while !ext.0.is_empty() {
        let kind = ext.u16()?;
        let mut data = ext.vec(2)?;
        if kind != 0 {
            continue;
        }
        let mut list = data.vec(2)?;
        while !list.0.is_empty() {
            let name_type = list.u8()?;
            let name = list.vec(2)?.0;
            if name_type != 0 {
                continue;
            }
            if name.is_empty() || name.len() > 253 || !name.iter().all(|b| b.is_ascii_graphic()) {
                return None;
            }
            let name = std::str::from_utf8(name).ok()?.to_ascii_lowercase();
            return Some(Hello::Name(name.trim_end_matches('.').to_string()));
        }
        return Some(Hello::NoName);
    }
    Some(Hello::NoName)
}

/// The place id a server name is routed to: `<id>-<token>-v4.<zone>` gives `<id>`, the token
/// being the 8 base32 characters every launch label ends with (`coord/dns.rs`, `mint`). Any
/// other name has none.
pub fn place_of(name: &str) -> Option<&str> {
    let (first, zone) = name.split_once('.')?;
    if zone.is_empty() {
        return None;
    }
    let label = first.strip_suffix("-v4")?;
    let (id, token) = label.rsplit_once('-')?;
    let token_ok = token.len() == 8
        && token
            .bytes()
            .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b));
    (token_ok && !id.is_empty()).then_some(id)
}

/// The host's IPv4 front.
pub struct Front {
    /// Where it listens.
    listen: SocketAddr,
    /// The port viewers reach it on at the host's public IPv4 address, reported for every
    /// place: `gpu_host_ipv4.sni_port` of `fleet/published-ports.json`.
    public_port: u16,
    /// The id the host's aggregator is named under (`agg-<host>`), for its reports.
    aggregate_id: Option<String>,
    /// Each place's listener port, by the id it last reported.
    routes: Mutex<HashMap<String, u16>>,
    limits: Limits,
    /// Connections in their ClientHello, by source.
    hellos: Arc<Counts<Source>>,
    /// Connections relayed, by source.
    relayed: Arc<Counts<Source>>,
    /// Connections relayed, by source and place id.
    relayed_to_place: Arc<Counts<(Source, String)>>,
}

impl Front {
    /// Listen on `listen` and relay what arrives. `public_port` is what the reports carry.
    pub async fn bind(
        listen: SocketAddr,
        public_port: u16,
        aggregate_id: Option<String>,
    ) -> Result<Arc<Front>> {
        Front::bind_limited(listen, public_port, aggregate_id, LIMITS).await
    }

    async fn bind_limited(
        listen: SocketAddr,
        public_port: u16,
        aggregate_id: Option<String>,
        limits: Limits,
    ) -> Result<Arc<Front>> {
        let listener = TcpListener::bind(listen)
            .await
            .with_context(|| format!("binding the IPv4 front on {listen}"))?;
        let front = Arc::new(Front {
            listen: listener.local_addr()?,
            public_port,
            aggregate_id,
            routes: Mutex::new(HashMap::new()),
            limits,
            hellos: Counts::new(),
            relayed: Counts::new(),
            relayed_to_place: Counts::new(),
        });
        tokio::spawn(front.clone().accept(listener));
        tracing::info!(listen = %front.listen, public_port, "IPv4 front listening");
        Ok(front)
    }

    /// The address it listens on.
    pub fn local_addr(&self) -> SocketAddr {
        self.listen
    }

    /// Record that `id`'s listener is on `port`, taking the port from any other id.
    fn route(&self, id: &str, port: u16) {
        let mut r = self.routes.lock().unwrap_or_else(|e| e.into_inner());
        r.retain(|i, p| *p != port || i == id);
        r.insert(id.to_string(), port);
    }

    /// The listener port of the place `name` is for.
    pub fn backend(&self, name: &str) -> Option<u16> {
        let id = place_of(name)?;
        let r = self.routes.lock().unwrap_or_else(|e| e.into_inner());
        r.get(id).copied()
    }

    /// A report as the coordinator gets it: a place's IPv4 path (its own listener's port) is
    /// recorded under the place's id and reported on the front's port instead.
    pub fn publish(&self, msg: AgentMsg) -> AgentMsg {
        match msg {
            AgentMsg::Slot(mut r) => {
                if let (Some(_), Some(port)) = (&r.ipv4, r.v4_port) {
                    self.route(&r.id, port);
                    r.v4_port = Some(self.public_port);
                }
                AgentMsg::Slot(r)
            }
            AgentMsg::Aggregate {
                frame_url,
                ipv6,
                ipv4,
                v4_port,
            } => {
                let v4_port = match (&ipv4, v4_port, &self.aggregate_id) {
                    (Some(_), Some(port), Some(id)) => {
                        self.route(id, port);
                        Some(self.public_port)
                    }
                    _ => v4_port,
                };
                AgentMsg::Aggregate {
                    frame_url,
                    ipv6,
                    ipv4,
                    v4_port,
                }
            }
            other => other,
        }
    }

    /// A sender whose messages reach `inner` through [`Front::publish`]. The forwarding ends
    /// when every clone of the returned sender is dropped, which drops `inner`.
    pub fn forward(
        self: &Arc<Self>,
        inner: mpsc::UnboundedSender<AgentMsg>,
    ) -> mpsc::UnboundedSender<AgentMsg> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let front = self.clone();
        tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                if inner.send(front.publish(m)).is_err() {
                    break;
                }
            }
        });
        tx
    }

    async fn accept(self: Arc<Self>, listener: TcpListener) {
        let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        loop {
            let (sock, peer) = match listener.accept().await {
                Ok(c) => c,
                Err(e) => {
                    // Out of descriptors, mostly: wait for some to close.
                    tracing::warn!(error = %e, "IPv4 front: accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let Ok(slot) = slots.clone().try_acquire_owned() else {
                continue; // dropping `sock` closes it
            };
            // Taken here, in accept order, so a source's connections past the cap are the
            // ones refused; given back once the hello is read or the connection ends.
            let source = Source::of(peer.ip());
            let Some(hello) = self.hellos.take(source, self.limits.hello_per_source) else {
                tracing::debug!(%peer, "IPv4 front: refused, its source has as many connections in their ClientHello as it may");
                continue;
            };
            let front = self.clone();
            tokio::spawn(async move {
                let _slot = slot;
                if let Err(e) = front.relay(sock, source, hello).await {
                    tracing::debug!(error = %format!("{e:#}"), "IPv4 front: connection refused or ended");
                }
            });
        }
    }

    /// Relay one connection to its place: the ClientHello, then both directions until both
    /// ends close.
    async fn relay(
        &self,
        mut client: TcpStream,
        source: Source,
        hello: Held<Source>,
    ) -> Result<()> {
        crate::network::relay_socket::set_relay_nodelay(
            &client,
            format_args!("SNI proxy, viewer socket"),
        );
        // The place's listener is on the address the viewer reached (the host's private
        // address behind its public one), as a viewer connecting to its port directly would.
        let local = client.local_addr()?;
        let mut buf = Vec::with_capacity(2048);
        let name = tokio::time::timeout(HELLO_TIMEOUT, read_hello(&mut client, &mut buf))
            .await
            .context("no ClientHello in time")??;
        drop(hello);
        let port = self
            .backend(&name)
            .with_context(|| format!("no place here for server name {name:?}"))?;
        let id = place_of(&name).unwrap_or_default().to_string();
        let _relayed = self
            .relayed
            .take(source, self.limits.relayed_per_source)
            .context("its source has as many connections relayed as it may")?;
        let _to_place = self
            .relayed_to_place
            .take((source, id), self.limits.relayed_per_source_place)
            .context("its source has as many connections relayed to this place as it may")?;
        let mut place = tokio::time::timeout(
            BACKEND_TIMEOUT,
            TcpStream::connect(SocketAddr::new(local.ip(), port)),
        )
        .await
        .context("the place's listener did not accept in time")?
        .with_context(|| format!("connecting to the place's listener on port {port}"))?;
        crate::network::relay_socket::set_relay_nodelay(
            &place,
            format_args!("SNI proxy, place socket"),
        );
        place.write_all(&buf).await?;
        tokio::io::copy_bidirectional(&mut client, &mut place).await?;
        Ok(())
    }
}

/// Read until `buf` holds a whole ClientHello, and give its server name.
pub async fn read_hello(client: &mut TcpStream, buf: &mut Vec<u8>) -> Result<String> {
    loop {
        match parse_hello(buf) {
            Hello::Name(n) => return Ok(n),
            Hello::NoName => bail!("the ClientHello names no server"),
            Hello::NotTls => bail!("not a TLS ClientHello"),
            Hello::Incomplete => {}
        }
        // The hello and its record headers; a record is at least one byte, so a hello
        // split into more records than this is refused.
        if buf.len() > MAX_HELLO + 4 + 5 * 64 {
            bail!("a ClientHello over {MAX_HELLO} bytes");
        }
        let mut chunk = [0u8; 4096];
        let n = client.read(&mut chunk).await?;
        if n == 0 {
            bail!("closed before its ClientHello ended");
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_core::protocol::SlotReport;
    use rustls::pki_types::ServerName;

    fn provider() -> Arc<rustls::crypto::CryptoProvider> {
        Arc::new(rustls::crypto::ring::default_provider())
    }

    /// A certificate for every launch name of `zone`, and the client trust that takes it.
    fn identity(zone: &str) -> (Arc<rustls::ServerConfig>, Arc<rustls::ClientConfig>) {
        let ck = rcgen::generate_simple_self_signed(vec![format!("*.{zone}")]).unwrap();
        let server = rustls::ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![ck.cert.der().clone()],
                rustls::pki_types::PrivateKeyDer::Pkcs8(ck.signing_key.serialize_der().into()),
            )
            .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ck.cert.der().clone()).unwrap();
        let client = rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        (Arc::new(server), Arc::new(client))
    }

    /// The first flight of a client opening `name` (no SNI for an address).
    fn client_hello(name: &str) -> Vec<u8> {
        let (_, client) = identity("fleet.example");
        let mut c =
            rustls::ClientConnection::new(client, ServerName::try_from(name.to_string()).unwrap())
                .unwrap();
        let mut out = Vec::new();
        while c.wants_write() {
            c.write_tls(&mut out).unwrap();
        }
        out
    }

    const NAME: &str = "slot-1-abcdefgh-v4.tiles.fleet.example";

    #[test]
    fn a_hello_gives_its_server_name() {
        let hello = client_hello(NAME);
        assert_eq!(parse_hello(&hello), Hello::Name(NAME.into()));
        // Every prefix is incomplete, never a wrong answer.
        for n in 0..hello.len() {
            assert_eq!(parse_hello(&hello[..n]), Hello::Incomplete, "{n} bytes");
        }
        // Bytes after the hello (none are sent before the server answers) change nothing.
        let mut more = hello.clone();
        more.extend_from_slice(&[22, 3, 3, 0, 1, 0]);
        assert_eq!(parse_hello(&more), Hello::Name(NAME.into()));
        // The name as the client wrote it, in capitals, is the same name.
        let upper = client_hello(&NAME.to_ascii_uppercase());
        assert_eq!(parse_hello(&upper), Hello::Name(NAME.into()));
    }

    #[test]
    fn a_hello_split_across_records_gives_its_name() {
        let hello = client_hello(NAME);
        let body = &hello[5..];
        for cut in [1, 4, 40, body.len() - 1] {
            let mut split = Vec::new();
            for part in [&body[..cut], &body[cut..]] {
                split.extend_from_slice(&[22, 3, 1, (part.len() >> 8) as u8, part.len() as u8]);
                split.extend_from_slice(part);
            }
            assert_eq!(
                parse_hello(&split),
                Hello::Name(NAME.into()),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn a_hello_without_a_name_and_other_bytes_are_refused() {
        // An address names no server.
        assert_eq!(parse_hello(&client_hello("192.0.2.1")), Hello::NoName);
        assert_eq!(
            parse_hello(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"),
            Hello::NotTls
        );
        // An alert record, a handshake message that is not a ClientHello, an empty record.
        assert_eq!(parse_hello(&[21, 3, 3, 0, 2, 2, 40]), Hello::NotTls);
        assert_eq!(parse_hello(&[22, 3, 3, 0, 4, 2, 0, 0, 0]), Hello::NotTls);
        assert_eq!(parse_hello(&[22, 3, 1, 0, 0]), Hello::NotTls);
        // A hello declared longer than the cap is refused before it arrives.
        assert_eq!(parse_hello(&[22, 3, 1, 0, 4, 1, 0, 0x40, 1]), Hello::NotTls);
        // A hello one byte short of what its extensions declare: its record and handshake
        // lengths say it ends there, its extensions' length runs past it.
        let hello = client_hello(NAME);
        let mut bad = hello[..hello.len() - 1].to_vec();
        let rec = (((bad[3] as usize) << 8) | bad[4] as usize) - 1;
        bad[3..5].copy_from_slice(&[(rec >> 8) as u8, rec as u8]);
        let hs = rec - 4;
        bad[6..9].copy_from_slice(&[(hs >> 16) as u8, (hs >> 8) as u8, hs as u8]);
        assert_eq!(parse_hello(&bad), Hello::NotTls);
    }

    #[test]
    fn only_a_v4_launch_name_has_a_place() {
        assert_eq!(place_of(NAME), Some("slot-1"));
        assert_eq!(place_of("win-3-a2b3c4d5-v4.fcvm.dev"), Some("win-3"));
        assert_eq!(place_of("mc-2-zzzzzzzz-v4.fcvm.dev"), Some("mc-2"));
        assert_eq!(place_of("phone-1-abcdefgh-v4.fcvm.dev"), Some("phone-1"));
        assert_eq!(
            place_of("agg-i-0123456789abcdef0-abcdefgh-v4.fcvm.dev"),
            Some("agg-i-0123456789abcdef0")
        );
        for name in [
            "slot-1-abcdefgh.fcvm.dev",     // the IPv6 name
            "slot-1-abcdefgh-v4",           // no zone
            "slot-1-abcdefgh-v4.",          // no zone
            "slot-1-abcdefg-v4.fcvm.dev",   // a token of 7
            "slot-1-abcdefgh1-v4.fcvm.dev", // a token of 9
            "slot-1-abcdefg1-v4.fcvm.dev",  // 1 is not base32
            "-abcdefgh-v4.fcvm.dev",        // no id
            "abcdefgh-v4.fcvm.dev",         // no id
            "fcvm.dev",
        ] {
            assert_eq!(place_of(name), None, "{name}");
        }
    }

    fn report(id: &str, port: Option<u16>) -> AgentMsg {
        AgentMsg::Slot(SlotReport {
            id: id.into(),
            phase: "running".into(),
            ipv6: Some("2001:db8::3".into()),
            ipv4: port.map(|_| "192.0.2.10".into()),
            v4_port: port,
            ..Default::default()
        })
    }

    fn v4_port(m: &AgentMsg) -> Option<u16> {
        match m {
            AgentMsg::Slot(r) => r.v4_port,
            AgentMsg::Aggregate { v4_port, .. } => *v4_port,
            _ => panic!("{m:?}"),
        }
    }

    /// Every mode's IPv4 path is reported on 443, its own port kept as the route.
    #[tokio::test]
    async fn every_ipv4_path_is_reported_on_the_fronts_port() {
        let front = Front::bind(
            "127.0.0.1:0".parse().unwrap(),
            443,
            Some("agg-host-1".into()),
        )
        .await
        .unwrap();
        for (id, port) in [
            ("slot-3", 20200),
            ("phone-1", 20207),
            ("win-2", 20208),
            ("mc-1", 20212),
        ] {
            assert_eq!(v4_port(&front.publish(report(id, Some(port)))), Some(443));
            assert_eq!(
                front.backend(&format!("{id}-abcdefgh-v4.fcvm.dev")),
                Some(port)
            );
        }
        let agg = front.publish(AgentMsg::Aggregate {
            frame_url: "ws://h:1/frames".into(),
            ipv6: None,
            ipv4: Some("192.0.2.10".into()),
            v4_port: Some(20216),
        });
        assert_eq!(v4_port(&agg), Some(443));
        assert_eq!(
            front.backend("agg-host-1-abcdefgh-v4.fcvm.dev"),
            Some(20216)
        );
        // A place with no IPv4 path keeps none, and other messages pass unchanged.
        assert_eq!(v4_port(&front.publish(report("slot-4", None))), None);
        assert_eq!(front.backend("slot-4-abcdefgh-v4.fcvm.dev"), None);
        let d = AgentMsg::Draining { reason: "x".into() };
        assert_eq!(front.publish(d.clone()), d);
        // A place that reports a new id takes its port from the old one.
        front.publish(report("slot-9", Some(20200)));
        assert_eq!(front.backend("slot-3-abcdefgh-v4.fcvm.dev"), None);
        assert_eq!(front.backend("slot-9-abcdefgh-v4.fcvm.dev"), Some(20200));
    }

    /// A place's TLS listener: completes the handshake and answers each connection with the
    /// server name it saw, so a client learns which place it reached.
    async fn place(server: Arc<rustls::ServerConfig>, tag: &'static str) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let acceptor = tokio_rustls::TlsAcceptor::from(server);
        tokio::spawn(async move {
            loop {
                let (s, _) = l.accept().await.unwrap();
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(s).await else {
                        return;
                    };
                    let sni = tls.get_ref().1.server_name().unwrap_or("").to_string();
                    let _ = tls.write_all(format!("{tag} {sni}").as_bytes()).await;
                    let _ = tls.shutdown().await;
                });
            }
        });
        port
    }

    /// Open `name` through `addr` and read what the place answers.
    async fn open(
        addr: SocketAddr,
        client: Arc<rustls::ClientConfig>,
        name: &str,
    ) -> Result<String> {
        let tcp = TcpStream::connect(addr).await?;
        let mut tls = tokio_rustls::TlsConnector::from(client)
            .connect(ServerName::try_from(name.to_string())?, tcp)
            .await?;
        let mut got = String::new();
        tls.read_to_string(&mut got).await?;
        Ok(got)
    }

    /// Each place's name reaches that place, with its own certificate (the front holds none,
    /// so a completed handshake is the place's); an unknown name and no name reach nothing.
    #[tokio::test]
    async fn each_place_is_routed_and_unknown_or_missing_names_are_refused() {
        let zone = "tiles.fleet.example";
        let (server, client) = identity(zone);
        let front = Front::bind("127.0.0.1:0".parse().unwrap(), 443, None)
            .await
            .unwrap();
        let ids = ["slot-1", "phone-2", "win-3", "mc-4"];
        let tags = ["wall", "phone", "windows", "minecraft"];
        for (id, tag) in ids.iter().zip(tags) {
            let port = place(server.clone(), tag).await;
            front.publish(report(id, Some(port)));
        }
        for (id, tag) in ids.iter().zip(tags) {
            let name = format!("{id}-abcdefgh-v4.{zone}");
            let got = open(front.local_addr(), client.clone(), &name)
                .await
                .unwrap();
            assert_eq!(got, format!("{tag} {name}"));
        }
        // A launch name of a place this host does not run.
        let unknown = format!("slot-7-abcdefgh-v4.{zone}");
        assert!(open(front.local_addr(), client.clone(), &unknown)
            .await
            .is_err());
        // The IPv6 name of a place this host runs.
        let v6 = format!("slot-1-abcdefgh.{zone}");
        assert!(open(front.local_addr(), client.clone(), &v6).await.is_err());
        // No server name: the closed connection is all the client gets.
        let mut raw = TcpStream::connect(front.local_addr()).await.unwrap();
        raw.write_all(&client_hello("192.0.2.1")).await.unwrap();
        let mut rest = Vec::new();
        raw.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty(), "the front answered {} bytes", rest.len());
        // Bytes that are not TLS.
        let mut raw = TcpStream::connect(front.local_addr()).await.unwrap();
        raw.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
        let mut rest = Vec::new();
        raw.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty());
    }

    /// What the client sends reaches the place unchanged, the ClientHello included.
    #[tokio::test]
    async fn the_relay_changes_no_byte() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let front = Front::bind("127.0.0.1:0".parse().unwrap(), 443, None)
            .await
            .unwrap();
        front.publish(report("slot-1", Some(port)));
        let mut sent = client_hello(NAME);
        sent.extend((0..200_000u32).map(|i| (i * 7) as u8));
        let mut c = TcpStream::connect(front.local_addr()).await.unwrap();
        c.write_all(&sent).await.unwrap();
        c.shutdown().await.unwrap();
        let (mut s, _) = l.accept().await.unwrap();
        let mut got = Vec::new();
        s.read_to_end(&mut got).await.unwrap();
        assert_eq!(got.len(), sent.len());
        assert!(got == sent);
        // And back.
        s.write_all(b"from the place").await.unwrap();
        drop(s);
        let mut back = Vec::new();
        c.read_to_end(&mut back).await.unwrap();
        assert_eq!(back, b"from the place");
    }

    /// A place's listener that reads each connection's ClientHello, answers one byte and holds
    /// the connection until the client closes it; it counts the connections it accepted.
    async fn holding_place() -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = accepted.clone();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = l.accept().await.unwrap();
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !matches!(parse_hello(&buf), Hello::Name(_)) {
                        match s.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    if s.write_all(b"x").await.is_err() {
                        return;
                    }
                    while matches!(s.read(&mut chunk).await, Ok(n) if n > 0) {}
                });
            }
        });
        (port, accepted)
    }

    /// A connection to `front` from `source` (any loopback address).
    async fn connect_from(front: &Front, source: &str) -> TcpStream {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind(format!("{source}:0").parse().unwrap()).unwrap();
        socket.connect(front.local_addr()).await.unwrap()
    }

    /// Whether the front closed `c` without a byte: true when it did, false when the
    /// connection is still open after a while.
    async fn closed_without_a_byte(c: &mut TcpStream) -> bool {
        let mut b = [0u8; 1];
        match tokio::time::timeout(Duration::from_millis(500), c.read(&mut b)).await {
            Err(_) => false,
            Ok(Ok(0)) | Ok(Err(_)) => true,
            Ok(Ok(n)) => panic!("the front sent {n} bytes"),
        }
    }

    /// Send `id`'s ClientHello from `source` and say whether the place answered (the
    /// connection was relayed) or the front closed it without a byte.
    async fn ask(front: &Front, source: &str, id: &str) -> (bool, TcpStream) {
        let mut c = connect_from(front, source).await;
        c.write_all(&client_hello(&format!("{id}-abcdefgh-v4.fcvm.dev")))
            .await
            .unwrap();
        let mut b = [0u8; 1];
        let n = tokio::time::timeout(Duration::from_secs(5), c.read(&mut b))
            .await
            .expect("neither relayed nor refused")
            .unwrap_or(0);
        (n == 1, c)
    }

    /// Wait until every count the front holds is given back.
    async fn wait_until_nothing_is_held(front: &Front) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while front.hellos.keys() + front.relayed.keys() + front.relayed_to_place.keys() > 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the front still holds counts: {} in hello, {} relayed, {} to a place",
                front.hellos.keys(),
                front.relayed.keys(),
                front.relayed_to_place.keys()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// One source holds at most its cap of connections that have sent no whole ClientHello;
    /// the next is closed without a byte, another source still gets through, and a connection
    /// that finishes its hello gives its place back.
    #[tokio::test]
    async fn a_source_holds_at_most_its_cap_of_connections_in_their_hello() {
        let limits = Limits {
            hello_per_source: 2,
            relayed_per_source_place: 8,
            relayed_per_source: 8,
        };
        let front = Front::bind_limited("127.0.0.1:0".parse().unwrap(), 443, None, limits)
            .await
            .unwrap();
        let (port, _) = holding_place().await;
        front.publish(report("slot-1", Some(port)));
        let mut idle = Vec::new();
        for _ in 0..2 {
            idle.push(connect_from(&front, "127.0.0.1").await);
        }
        let mut third = connect_from(&front, "127.0.0.1").await;
        assert!(
            closed_without_a_byte(&mut third).await,
            "a third idle connection from one source was not refused"
        );
        for c in idle.iter_mut() {
            assert!(
                !closed_without_a_byte(c).await,
                "an idle connection within the cap was closed"
            );
        }
        let (relayed, other) = ask(&front, "127.0.0.2", "slot-1").await;
        assert!(relayed, "another source was refused");
        // One idle connection sends its hello and is relayed: its hello place is free again.
        idle[0].write_all(&client_hello(NAME_SLOT_1)).await.unwrap();
        let mut b = [0u8; 1];
        assert_eq!(idle[0].read(&mut b).await.unwrap(), 1);
        let mut next = connect_from(&front, "127.0.0.1").await;
        assert!(!closed_without_a_byte(&mut next).await);
        drop((idle, next, other));
        wait_until_nothing_is_held(&front).await;
    }

    const NAME_SLOT_1: &str = "slot-1-abcdefgh-v4.fcvm.dev";

    /// One source has at most its cap of connections relayed to one place and its cap in all;
    /// one past either is closed without a byte and never reaches the place, another source is
    /// still relayed, and a closed connection gives its places back.
    #[tokio::test]
    async fn a_source_is_relayed_at_most_its_caps_to_a_place_and_in_all() {
        let limits = Limits {
            hello_per_source: 8,
            relayed_per_source_place: 2,
            relayed_per_source: 3,
        };
        let front = Front::bind_limited("127.0.0.1:0".parse().unwrap(), 443, None, limits)
            .await
            .unwrap();
        let mut places = HashMap::new();
        for id in ["slot-1", "slot-2", "slot-3"] {
            let (port, accepted) = holding_place().await;
            front.publish(report(id, Some(port)));
            places.insert(id, accepted);
        }
        let accepted = |id: &str| places[id].load(std::sync::atomic::Ordering::SeqCst);
        let mut held = Vec::new();
        for _ in 0..2 {
            let (relayed, c) = ask(&front, "127.0.0.1", "slot-1").await;
            assert!(relayed, "a connection within the per-place cap was refused");
            held.push(c);
        }
        assert!(
            !ask(&front, "127.0.0.1", "slot-1").await.0,
            "a third connection from one source to one place was relayed"
        );
        let (relayed, c) = ask(&front, "127.0.0.1", "slot-2").await;
        assert!(relayed, "a connection to another place was refused");
        held.push(c);
        assert!(
            !ask(&front, "127.0.0.1", "slot-3").await.0,
            "a fourth connection from one source was relayed"
        );
        let (relayed, c) = ask(&front, "127.0.0.2", "slot-1").await;
        assert!(relayed, "another source was refused");
        held.push(c);
        assert_eq!(
            (accepted("slot-1"), accepted("slot-2"), accepted("slot-3")),
            (3, 1, 0),
            "a refused connection reached its place"
        );
        // A closed connection gives back both of its places once the front has seen it end.
        drop(held.remove(0));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let again = loop {
            let (relayed, c) = ask(&front, "127.0.0.1", "slot-3").await;
            if relayed {
                break c;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "a closed connection kept its place"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        drop((held, again));
        wait_until_nothing_is_held(&front).await;
    }
}
