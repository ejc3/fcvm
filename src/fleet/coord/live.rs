//! The browser side: `GET /live` (WebSocket), `GET /poster.jpg` and `GET /healthz`.
//!
//! Each socket has a reader (intents, pings, verification) and a writer. The writer sends
//! the viewer's id, the current snapshot on connect and the latest one whenever the
//! version moves; receipts, clock replies, verification results and the viewer's own claim
//! reach it through a bounded channel. A client that stops reading until that channel is
//! full is disconnected.
//!
//! Every intent carries the connection's client key, which the per-client limits count by:
//! the client address (`CF-Connecting-IP` when trusted, else the peer address), an IPv6
//! address cut to its /64 ([`client_key`]).
//!
//! With a Turnstile secret configured, a connection that has not passed `verify` is not a
//! viewer yet: it receives snapshots and, when the fleet runs one, the broadcast, but it is
//! not joined (no arrival, so it wakes nothing, and it does not count as active), holds no
//! seat and no direct token, and its intents are refused. It joins and is admitted once
//! siteverify accepts its token; a siteverify error or timeout leaves it as it was. A seat
//! ticket this coordinator still holds, or a request the [`RequestProof`] accepts (a valid
//! Cloudflare Access JWT, [`super::access_jwt`]), counts as verified from the start.

use super::admission::{Admitted, Gate};
use super::poster::Poster;
use super::text_check::{check_text, TextChecker};
use super::turnstile::{RequestProof, Verifier, SITEVERIFY_ERROR_MARKER};
use super::{Handle, Submitted};
use crate::fleet::now_ms;
use anyhow::Result;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{ConnectInfo, State, WebSocketUpgrade};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use fleet_core::protocol::{ClientMsg, ServerMsg};
use fleet_core::state::{Outcome, TextCheck};
use fleet_core::Event;
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// The largest message a browser may send.
const MAX_CLIENT_MESSAGE: usize = 64 * 1024;
/// Receipts and clock replies a socket may have queued before it is dropped.
const DIRECT_QUEUE: usize = 256;
/// `word` intents one connection may have waiting for their receipts (a committed word's
/// receipt waits for the store); past it a word is refused unrecorded.
pub const WORDS_WAITING: usize = 8;
pub const WORDS_WAITING_REFUSAL: &str =
    "too many words from this page wait for the state to be stored; send it again later";

/// `/live` sockets one IPv6 /64 may hold open at once. A /64 is one subscriber, and a page
/// holds one socket (fleet/web wall-client.ts opens the next only after the last has closed);
/// eight lets one household watch on several devices.
pub const SOCKETS_PER_IPV6_64: usize = 8;
/// `/live` sockets one IPv4 address may hold open at once. Mobile carriers and many ISPs put
/// many subscribers behind one IPv4 address (carrier-grade NAT), so a cap of a household's
/// size would refuse the next visitor on that carrier outright. A socket costs only memory,
/// which [`MAX_SOCKETS`] already bounds; 64 still keeps one address to 1.3% of it.
pub const SOCKETS_PER_IPV4: usize = 64;
/// `/live` sockets the coordinator holds in all. The coordinator holds about 5,000 of them,
/// bound by memory at about 160 KB each (docs/fleet/ARCHITECTURE.md, "Past about 5,000
/// viewers the coordinator needs more"); past that a flood of sockets would take the task's
/// memory from the viewers already connected.
pub const MAX_SOCKETS: usize = 5_000;
/// How long a refused `/live` request is told to wait (`Retry-After`). The page does not read
/// it: a refused handshake is a failed connect to it, retried with its own backoff.
pub const FULL_RETRY_AFTER_SECS: u64 = 30;

/// The caps on open `/live` sockets: per client key of each family, and in all.
#[derive(Debug, Clone, Copy)]
pub struct SocketCaps {
    /// Per IPv6 /64, and per any other key (one that is not an IPv4 address).
    pub per_ipv6_64: usize,
    /// Per IPv4 address.
    pub per_ipv4: usize,
    pub total: usize,
}

impl SocketCaps {
    /// The cap for one client key ([`client_key`]).
    fn per_client(&self, key: &str) -> usize {
        if key.parse::<std::net::Ipv4Addr>().is_ok() {
            self.per_ipv4
        } else {
            self.per_ipv6_64
        }
    }
}

impl Default for SocketCaps {
    fn default() -> Self {
        SocketCaps {
            per_ipv6_64: SOCKETS_PER_IPV6_64,
            per_ipv4: SOCKETS_PER_IPV4,
            total: MAX_SOCKETS,
        }
    }
}

/// The open `/live` sockets: in all and per client key.
struct Sockets {
    caps: SocketCaps,
    open: Mutex<(usize, std::collections::HashMap<String, usize>)>,
}

/// One open socket's place under the caps, given back when it drops (the socket closed, or the
/// upgrade never happened).
struct SocketPermit {
    sockets: Arc<Sockets>,
    key: String,
}

/// Why a socket was refused.
enum SocketsFull {
    Client,
    Total,
}

impl Sockets {
    fn take(self: &Arc<Self>, key: &str) -> Result<SocketPermit, SocketsFull> {
        let mut open = self.open.lock().unwrap();
        let (total, per) = &mut *open;
        if *total >= self.caps.total {
            return Err(SocketsFull::Total);
        }
        let n = per.entry(key.to_string()).or_default();
        if *n >= self.caps.per_client(key) {
            return Err(SocketsFull::Client);
        }
        *n += 1;
        *total += 1;
        Ok(SocketPermit {
            sockets: self.clone(),
            key: key.to_string(),
        })
    }
}

impl Drop for SocketPermit {
    fn drop(&mut self) {
        let mut open = self.sockets.open.lock().unwrap();
        let (total, per) = &mut *open;
        *total = total.saturating_sub(1);
        if let Some(n) = per.get_mut(&self.key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                per.remove(&self.key);
            }
        }
    }
}

pub struct LiveConfig {
    pub checker: Arc<dyn TextChecker>,
    /// Browser origins allowed besides the coordinator's own.
    pub allowed_origins: Vec<String>,
    pub verifier: Arc<dyn Verifier>,
    /// What counts as verified without a Turnstile token (asked only when the verifier is
    /// required).
    pub proof: Arc<dyn RequestProof>,
    /// Sent in the hello when the verifier asks for a token.
    pub turnstile_site_key: Option<String>,
    /// Key clients by `CF-Connecting-IP` (only behind Cloudflare, which sets it).
    pub trust_cf_connecting_ip: bool,
    /// Key a request Access admitted with a service token by [`VIEWER_ADDRESS_HEADER`]
    /// (only behind a connector that requires and verifies Access).
    pub access_viewer_address: bool,
    pub poster: Arc<Poster>,
    /// Who gets direct streams and who the broadcast, and their tokens.
    pub gate: Arc<Gate>,
    /// How many `/live` sockets may be open.
    pub sockets: SocketCaps,
    /// Access tokens checked by the coordinator itself: on `/live` for the viewer address,
    /// and on `/agent` when agent audiences are given. `None`: Access is trusted at the edge.
    pub access: Option<Arc<super::access_jwt::AccessCheck>>,
    /// The check on the host a dialed agent's hello names; `None` admits any.
    pub agent_identity: Option<Arc<dyn super::agents::AgentIdentity>>,
}

#[derive(Clone)]
pub struct App {
    pub handle: Handle,
    pub cfg: Arc<LiveConfig>,
    next_viewer: Arc<AtomicU64>,
    sockets: Arc<Sockets>,
}

pub fn router(handle: Handle, cfg: LiveConfig) -> Router {
    let sockets = Arc::new(Sockets {
        caps: cfg.sockets,
        open: Mutex::default(),
    });
    Router::new()
        .route("/live", get(live))
        .route("/agent", get(agent_socket))
        .route("/poster.jpg", get(poster))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(App {
            handle,
            cfg: Arc::new(cfg),
            sockets,
            next_viewer: Arc::new(AtomicU64::new(1)),
        })
}

pub async fn serve(
    listener: tokio::net::TcpListener,
    handle: Handle,
    cfg: LiveConfig,
) -> Result<()> {
    axum::serve(
        listener,
        router(handle, cfg).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

/// The latest converged still of the whole image. Cached briefly: it changes at most once
/// per camera hold. Readable from any origin: it is the public picture.
async fn poster(State(app): State<App>) -> Response {
    match app.cfg.poster.latest() {
        Some(still) => (
            [
                (header::CONTENT_TYPE, still.content_type),
                (header::CACHE_CONTROL, "public, max-age=10"),
                (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
            ],
            still.bytes.clone(),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            [
                (header::CACHE_CONTROL, "no-store"),
                (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
            ],
            "no poster yet",
        )
            .into_response(),
    }
}

/// An agent that dials in (`fcvm fleet agent --dial`): the agent protocol with one JSON
/// message per text frame. Cloudflare Access admits only the agents' service token on this
/// path before a request reaches the coordinator; with agent audiences configured the
/// coordinator verifies that token itself, and with an identity check the host the hello
/// names must be one of this fleet's.
async fn agent_socket(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(app): State<App>,
) -> Response {
    // Where the agent's connection comes from (CF-Connecting-IP behind Cloudflare): the host
    // its hello names must own it.
    let source = client_address(&headers, peer, app.cfg.trust_cf_connecting_ip, false)
        .parse::<std::net::IpAddr>()
        .ok()
        .map(|ip| ip.to_canonical());
    if let Some(access) = app.cfg.access.as_ref().filter(|a| !a.agent.is_empty()) {
        if let Err(e) = access.agent_claims(&headers).await {
            tracing::warn!(
                marker = super::agents::AGENT_REFUSED_MARKER,
                reason = %format!("{e:#}"),
                "refusing /agent: its Access token does not verify"
            );
            return (StatusCode::FORBIDDEN, "agent not admitted").into_response();
        }
    }
    ws.max_message_size(crate::fleet::lines::MAX_LINE)
        .on_upgrade(move |socket| async move {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
            let (mut to_serve, serve_in) = tokio::io::duplex(1 << 20);
            let (serve_out, from_serve) = tokio::io::duplex(1 << 20);
            let (mut sink, mut stream) = socket.split();
            let inbound = tokio::spawn(async move {
                while let Some(Ok(m)) = stream.next().await {
                    match m {
                        Message::Text(t) => {
                            let mut line = t.as_bytes().to_vec();
                            line.push(b'\n');
                            if to_serve.write_all(&line).await.is_err() {
                                return;
                            }
                        }
                        Message::Close(_) => return,
                        _ => {}
                    }
                }
            });
            let outbound = tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(from_serve).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    if sink.send(Message::Text(l.into())).await.is_err() {
                        return;
                    }
                }
                let _ = sink.close().await;
            });
            if let Err(e) = super::agents::serve_checked(
                serve_in,
                serve_out,
                &app.handle,
                app.cfg.agent_identity.as_deref(),
                source,
            )
            .await
            {
                tracing::warn!(error = %format!("{e:#}"), "agent connection ended");
            }
            inbound.abort();
            let _ = outbound.await;
        })
        .into_response()
}

/// The viewer's address as the staging page's Worker forwards it (fleet/web/worker.ts). A
/// Worker's subrequest reaches Cloudflare's front door as a request of its own, so its
/// `CF-Connecting-IP` is the Worker's address, the same for every viewer.
pub const VIEWER_ADDRESS_HEADER: &str = "x-fleet-viewer-ip";

/// The address the per-client limits use: `CF-Connecting-IP` behind Cloudflare (`trust_cf`),
/// else the peer address. With `forwarded_viewer` (`--access-viewer-address` and a request
/// Access admitted with a service token, [`service_token`]), the request is keyed by
/// [`VIEWER_ADDRESS_HEADER`] instead: only the fleet's own Worker and CI hold one, and a
/// browser's Access JWT names a person, so a viewer cannot set the address it is limited by.
pub fn client_address(
    headers: &HeaderMap,
    peer: SocketAddr,
    trust_cf: bool,
    forwarded_viewer: bool,
) -> String {
    let ip = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| v.parse::<std::net::IpAddr>().is_ok())
            .map(str::to_string)
    };
    if forwarded_viewer {
        if let Some(viewer) = ip(VIEWER_ADDRESS_HEADER) {
            return viewer;
        }
    }
    if trust_cf {
        if let Some(cf) = ip("cf-connecting-ip") {
            return cf;
        }
    }
    peer.ip().to_string()
}

/// The key every per-client limit counts by: an IPv4 address as it is, an IPv6 address as its
/// /64 (`2001:db8:1:2::/64`). One subscriber or one cloud instance is commonly given a whole
/// /64 (RFC 6177 recommends at least a /64 per end site, and an EC2 interface takes its IPv6
/// addresses from its subnet's /64), so keying by the full address let one client rotate
/// through 2^64 keys and pass every limit. The Minecraft join guard counts sources the same way
/// (`agent::joinguard::SOURCE_MASK`). An IPv4-mapped IPv6 address (`::ffff:a.b.c.d`, a peer on a
/// dual-stack listener) is its IPv4 address. Anything that is not an address is kept as given.
pub fn client_key(address: &str) -> String {
    match address.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.to_string(),
        Ok(std::net::IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let mut o = v6.octets();
                o[8..].fill(0);
                format!("{}/64", std::net::Ipv6Addr::from(o))
            }
        },
        Err(_) => address.to_string(),
    }
}

/// Whether Access admitted this request with a service token: its `Cf-Access-Jwt-Assertion`
/// names one (`common_name`, the token's client id) and no person (`email`). With an
/// [`super::access_jwt::AccessCheck`] the token must verify (signature, issuer, a `/live`
/// audience, expiry). Without one the payload is read unverified, which is trusted only behind
/// a tunnel connector that requires and verifies Access (`access.required`, ejc3/aws
/// fcvm-dev.tf).
pub async fn service_token(
    access: Option<&super::access_jwt::AccessCheck>,
    headers: &HeaderMap,
) -> bool {
    match access {
        Some(a) => a
            .live_claims(headers)
            .await
            .is_some_and(|c| c.service_token()),
        None => unverified_service_token(headers),
    }
}

fn unverified_service_token(headers: &HeaderMap) -> bool {
    let Some(jwt) = headers
        .get("cf-access-jwt-assertion")
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some(payload) = jwt.split('.').nth(1) else {
        return false;
    };
    let Some(claims) = super::admission::b64url_decode(payload)
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
    else {
        return false;
    };
    let named = |k: &str| {
        claims
            .get(k)
            .and_then(|v| v.as_str())
            .is_some_and(|v| !v.is_empty())
    };
    named("common_name") && !named("email")
}

async fn live(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(app): State<App>,
) -> Response {
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    if !origin_allowed(origin, host, &app.cfg.allowed_origins) {
        tracing::info!(origin = ?origin, "refusing /live from a foreign origin");
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }
    let forwarded_viewer =
        app.cfg.access_viewer_address && service_token(app.cfg.access.as_deref(), &headers).await;
    let address = client_address(
        &headers,
        peer,
        app.cfg.trust_cf_connecting_ip,
        forwarded_viewer,
    );
    let client = Client {
        key: client_key(&address),
        address,
    };
    let permit = match app.sockets.take(&client.key) {
        Ok(p) => p,
        Err(full) => {
            let why = match full {
                SocketsFull::Client => "too many connections from this network",
                SocketsFull::Total => "the coordinator is full",
            };
            tracing::info!(client = %client.key, why, "refusing /live");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [
                    (header::RETRY_AFTER, FULL_RETRY_AFTER_SECS.to_string()),
                    (header::CACHE_CONTROL, "no-store".to_string()),
                ],
                why,
            )
                .into_response();
        }
    };
    // Without a verifier every connection is verified; with one, only a request whose
    // headers prove it is, before any token.
    let proven = !app.cfg.verifier.required() || app.cfg.proof.verified(&headers).await;
    let page = Page {
        origin: origin.map(|o| o.trim_end_matches('/').to_string()),
        proven,
    };
    ws.max_message_size(MAX_CLIENT_MESSAGE)
        .on_upgrade(move |socket| async move {
            // Held for the socket's life; an upgrade that never happens drops it unused.
            let _permit = permit;
            viewer(socket, app, client, page).await
        })
        .into_response()
}

pub fn origin_allowed(origin: Option<&str>, host: Option<&str>, allowed: &[String]) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    let origin = origin.trim_end_matches('/');
    if allowed
        .iter()
        .any(|a| a.trim_end_matches('/').eq_ignore_ascii_case(origin))
    {
        return true;
    }
    let authority = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"));
    matches!((authority, host), (Some(a), Some(h)) if a.eq_ignore_ascii_case(h))
}

fn text(msg: &ServerMsg) -> String {
    serde_json::to_string(msg).expect("server messages serialize")
}

/// A refusal that is not recorded (the intent was never applied), so the client may send
/// the same id again once the reason is gone.
fn unrecorded_refusal(id: String, reason: &str) -> ServerMsg {
    ServerMsg::Receipt {
        id,
        outcome: Outcome::Refused,
        reason: Some(reason.into()),
        version: 0,
        check: None,
    }
}

/// Who a connection is: its address, which Turnstile's siteverify is given, and the key every
/// per-client limit counts by.
struct Client {
    address: String,
    key: String,
}

/// What the page said about itself when it connected.
struct Page {
    /// Its origin, which a playback token is bound to.
    origin: Option<String>,
    /// Verified before any Turnstile token: no verifier is configured, or the request proved
    /// itself ([`RequestProof`]).
    proven: bool,
}

/// How long a connection waits for its first message, which may hold a seat ticket, before
/// it is admitted without one. The page sends one at once, so only a client that sends
/// nothing waits this long.
pub const SEAT_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Why a late viewer's `claim_gpu`, `resume_claim`, `claim_phone`, `claim_windows` or
/// `claim_minecraft` is refused: a private GPU is a direct stream too (a resumed one as much
/// as a new one), a borrowed phone or sandbox is shown from its direct stream, and a borrowed
/// server is driven over its tile's view socket, which needs a direct token.
pub const LATE_CLAIM: &str = "direct streams are full, so this page watches the broadcast";

/// The intents a late viewer is refused with [`LATE_CLAIM`].
pub const LATE_REFUSED: [&str; 5] = [
    "claim_gpu",
    "resume_claim",
    "claim_phone",
    "claim_windows",
    "claim_minecraft",
];

async fn viewer(socket: WebSocket, app: App, who: Client, page: Page) {
    let Client {
        address,
        key: client,
    } = who;
    let viewer = format!("v-{}", app.next_viewer.fetch_add(1, Ordering::Relaxed));
    let (mut sink, mut stream) = socket.split();
    // Tell the viewer who it is before anything else, so it can find its own actions.
    if sink
        .send(Message::Text(
            text(&ServerMsg::Hello {
                viewer: viewer.clone(),
                turnstile: app
                    .cfg
                    .verifier
                    .required()
                    .then(|| app.cfg.turnstile_site_key.clone())
                    .flatten(),
            })
            .into(),
        ))
        .await
        .is_err()
    {
        return;
    }
    // Then how it receives the image. A seat ticket comes in the first message; any other
    // first message is handled below like the ones after it.
    let mut pending: Option<Message> = None;
    let (seat, returning) = match tokio::time::timeout(SEAT_WAIT, stream.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => match serde_json::from_str::<ClientMsg>(&t) {
            Ok(ClientMsg::Seat { ticket, returning }) => {
                let returning = returning || ticket.is_some();
                (ticket, returning)
            }
            _ => {
                pending = Some(Message::Text(t));
                (None, false)
            }
        },
        Ok(Some(Ok(m))) => {
            pending = Some(m);
            (None, false)
        }
        _ => (None, false),
    };
    // A seat is held from admission until the connection ends. A connection that has not
    // proved itself is admitted only when it brings a seat ticket this coordinator still holds
    // (given to a connection that had verified); otherwise it waits for `verify`.
    let gate = app.cfg.gate.clone();
    let first = if page.proven {
        gate.seats
            .admit(seat.as_deref(), &client, std::time::Instant::now())
    } else {
        seat.as_deref()
            .and_then(|t| gate.seats.resume(t, std::time::Instant::now()))
            .unwrap_or(Admitted::Unverified)
    };
    // Verified: joined as a viewer. Set once, never cleared.
    let verified = Arc::new(AtomicBool::new(first != Admitted::Unverified));
    // A page that kept a ticket, or says it connected before, is reconnecting: it joins
    // without counting as an arrival (PROTOCOL.md, `fleet.power`). Nothing it sends is
    // applied before this. An unverified connection joins when it verifies.
    if verified.load(Ordering::SeqCst) {
        app.handle
            .send(Event::ViewerJoined {
                at: 0,
                viewer: viewer.clone(),
                client: Some(client.clone()),
                returning,
            })
            .await;
    }
    let admission = gate.message(
        &first,
        &viewer,
        &client,
        page.origin.as_deref(),
        now_ms(),
        std::time::Instant::now(),
    );
    let admitted = Arc::new(Mutex::new(first));
    // The seat back and the viewer gone, once the connection ends.
    let leave = |gate: &Gate, admitted: &Mutex<Admitted>, verified: &AtomicBool| {
        if let Admitted::Seat { ticket } = &*admitted.lock().unwrap() {
            gate.seats.release(ticket, std::time::Instant::now());
        }
        verified.load(Ordering::SeqCst).then(|| Event::ViewerLeft {
            at: 0,
            viewer: viewer.clone(),
        })
    };
    if sink
        .send(Message::Text(
            text(&ServerMsg::Admission(admission.clone())).into(),
        ))
        .await
        .is_err()
    {
        if let Some(left) = leave(&gate, &admitted, &verified) {
            app.handle.send(left).await;
        }
        return;
    }
    let (direct, mut direct_rx) = mpsc::channel::<ServerMsg>(DIRECT_QUEUE);
    // Fresh tokens before the old ones expire, and a late viewer told when a seat frees.
    let renew = {
        let gate = gate.clone();
        let admitted = admitted.clone();
        let viewer = viewer.clone();
        let client = client.clone();
        let origin = page.origin.clone();
        let direct = direct.clone();
        tokio::spawn(async move {
            let mut last = admission;
            let mut sent_at = std::time::Instant::now();
            loop {
                tokio::time::sleep(gate.tick).await;
                let now = std::time::Instant::now();
                let admitted = admitted.lock().unwrap().clone();
                // Only a viewer late for the capacity is told of a free seat: one whose client
                // holds its seats would be late again after a reload.
                let free_changed =
                    admitted == Admitted::Late && gate.seats.free(&client, now) != last.seat_free;
                if !free_changed && now.duration_since(sent_at) < super::admission::DIRECT_REFRESH {
                    continue;
                }
                last = gate.message(
                    &admitted,
                    &viewer,
                    &client,
                    origin.as_deref(),
                    now_ms(),
                    now,
                );
                sent_at = now;
                if direct
                    .send(ServerMsg::Admission(last.clone()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        })
    };
    let mut snapshots = app.handle.snapshot.clone();
    let writer = tokio::spawn(async move {
        let mut sent: Option<u64> = None;
        loop {
            let snap = snapshots.borrow_and_update().clone();
            if sent.is_none_or(|v| snap.version > v) && !snap.text.is_empty() {
                if sink
                    .send(Message::Text(snap.text.clone().into()))
                    .await
                    .is_err()
                {
                    return;
                }
                sent = Some(snap.version);
            }
            tokio::select! {
                biased;
                m = direct_rx.recv() => match m {
                    Some(m) => {
                        if sink.send(Message::Text(text(&m).into())).await.is_err() {
                            return;
                        }
                    }
                    None => return,
                },
                changed = snapshots.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }
    });

    // Receipts in the order their intents arrived, each sent once a stored state holds its
    // change (a committed one; a refused one at once), so a coordinator that crashes has
    // acknowledged nothing it did not store. Waiting here keeps the reader reading. A receipt
    // that no stored state ever holds is never sent. A claim's messages start after its
    // receipt, from the claim as it was when its intent applied: one that ended while the
    // receipt waited for the store is still told, as ended.
    type Queued = (
        ServerMsg,
        oneshot::Receiver<bool>,
        Option<fleet_core::protocol::ClaimMsg>,
    );
    let (receipts, mut receipts_rx) = mpsc::channel::<Queued>(DIRECT_QUEUE);
    let forward = {
        let direct = direct.clone();
        let handle = app.handle.clone();
        let viewer = viewer.clone();
        tokio::spawn(async move {
            let mut claims = tokio::task::JoinSet::new();
            while let Some((msg, stored, claim)) = receipts_rx.recv().await {
                // A watcher whose claim ended has returned: its slot goes now, not with the
                // connection.
                while claims.try_join_next().is_some() {}
                if stored.await != Ok(true) {
                    continue;
                }
                if direct.send(msg).await.is_err() {
                    return;
                }
                if let Some(seen) = claim {
                    claims.spawn(tell_claim(
                        handle.clone(),
                        viewer.clone(),
                        direct.clone(),
                        Some(seen),
                    ));
                }
            }
        })
    };
    // The one `word` this viewer has at the text check, if any.
    let checking: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    // The `word` intent ids of this connection with a task working on them: a repeated id
    // joins its task, and at most WORDS_WAITING wait, so a store outage cannot pile up tasks.
    let words: Arc<Mutex<std::collections::HashSet<String>>> = Arc::default();
    let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    // When this connection last asked for a fresh playback token.
    let mut last_playback: Option<std::time::Instant> = None;
    // The visibility an unverified page reported last, applied when it joins: a background tab
    // whose check passes on its own sends nothing more until it is shown.
    let mut visible_before_join: Option<bool> = None;
    loop {
        let msg = match pending.take() {
            Some(m) => m,
            None => match stream.next().await {
                Some(Ok(m)) => m,
                _ => break,
            },
        };
        let body = match msg {
            Message::Text(t) => t.to_string(),
            Message::Close(_) => break,
            Message::Binary(_) => {
                if direct
                    .try_send(ServerMsg::Error {
                        reason: "binary messages are not accepted".into(),
                    })
                    .is_err()
                {
                    break;
                }
                continue;
            }
            _ => continue,
        };
        let reply = match serde_json::from_str::<ClientMsg>(&body) {
            Err(e) => Some(ServerMsg::Error {
                reason: format!("message is not a ping, a verify or an intent: {e}"),
            }),
            Ok(ClientMsg::Ping { client_unix_ms }) => Some(ServerMsg::Clock {
                server_unix_ms: now_ms(),
                client_unix_ms,
            }),
            // Only a first message carries a seat ticket.
            Ok(ClientMsg::Seat { .. }) => None,
            // A playback token plays once, so a player that loads again asks for a fresh one.
            Ok(ClientMsg::Playback {}) => {
                let now = std::time::Instant::now();
                if last_playback.is_some_and(|t| {
                    now.duration_since(t) < super::admission::PLAYBACK_REQUEST_EVERY
                }) {
                    Some(ServerMsg::Error {
                        reason: format!(
                            "a fresh playback token at most every {} s",
                            super::admission::PLAYBACK_REQUEST_EVERY.as_secs()
                        ),
                    })
                } else {
                    last_playback = Some(now);
                    let admitted = admitted.lock().unwrap().clone();
                    Some(ServerMsg::Admission(gate.message(
                        &admitted,
                        &viewer,
                        &client,
                        page.origin.as_deref(),
                        now_ms(),
                        now,
                    )))
                }
            }
            // An unverified connection is not a viewer, so it is neither active nor hidden.
            Ok(ClientMsg::Visibility { visible }) => {
                if !verified.load(Ordering::SeqCst) {
                    visible_before_join = Some(visible);
                } else {
                    app.handle
                        .send(Event::Visibility {
                            at: 0,
                            viewer: viewer.clone(),
                            visible,
                        })
                        .await;
                }
                None
            }
            Ok(ClientMsg::Verify { token }) => {
                let r = app.cfg.verifier.verify(&token, &address).await;
                // A siteverify error or timeout fails closed: the connection stays unverified.
                let (ok, reason) = match r {
                    Ok(None) => (true, None),
                    Ok(Some(why)) => (false, Some(why)),
                    Err(e) => {
                        let e = format!("{e:#}");
                        tracing::warn!(
                            marker = SITEVERIFY_ERROR_MARKER,
                            %viewer,
                            error = %e,
                            "Turnstile siteverify failed; the connection stays unverified"
                        );
                        (false, Some(e))
                    }
                };
                if direct.try_send(ServerMsg::Verified { ok, reason }).is_err() {
                    tracing::info!(%viewer, "viewer is not reading; closing");
                    break;
                }
                if ok && !verified.load(Ordering::SeqCst) {
                    // Admitted now: joined (an arrival unless the page said it is returning),
                    // then seated if a seat is free.
                    app.handle
                        .send(Event::ViewerJoined {
                            at: 0,
                            viewer: viewer.clone(),
                            client: Some(client.clone()),
                            returning,
                        })
                        .await;
                    if visible_before_join == Some(false) {
                        app.handle
                            .send(Event::Visibility {
                                at: 0,
                                viewer: viewer.clone(),
                                visible: false,
                            })
                            .await;
                    }
                    verified.store(true, Ordering::SeqCst);
                    let now = std::time::Instant::now();
                    let a = gate.seats.admit(None, &client, now);
                    let msg =
                        gate.message(&a, &viewer, &client, page.origin.as_deref(), now_ms(), now);
                    *admitted.lock().unwrap() = a;
                    Some(ServerMsg::Admission(msg))
                } else {
                    None
                }
            }
            Ok(ClientMsg::Intent { id, name, args }) => {
                if id.is_empty() || id.len() > 128 {
                    Some(ServerMsg::Error {
                        reason: "an intent id must be 1 to 128 bytes".into(),
                    })
                } else if !verified.load(Ordering::SeqCst) {
                    Some(unrecorded_refusal(id, "verify first"))
                } else if admitted.lock().unwrap().late() && LATE_REFUSED.contains(&name.as_str()) {
                    Some(unrecorded_refusal(id, LATE_CLAIM))
                } else if name == "word" {
                    let admitted = {
                        let mut w = words.lock().unwrap();
                        if w.contains(&id) {
                            // Its task answers it.
                            Some(false)
                        } else if w.len() >= WORDS_WAITING {
                            None
                        } else {
                            w.insert(id.clone());
                            Some(true)
                        }
                    };
                    match admitted {
                        None => Some(unrecorded_refusal(id, WORDS_WAITING_REFUSAL)),
                        Some(false) => None,
                        Some(true) => {
                            let app = app.clone();
                            let viewer = viewer.clone();
                            let client = client.clone();
                            let direct = direct.clone();
                            let checking = checking.clone();
                            let words = words.clone();
                            tasks.retain(|t| !t.is_finished());
                            tasks.push(tokio::spawn(async move {
                                let r =
                                    word(&app, &viewer, &client, id.clone(), args, &checking).await;
                                words.lock().unwrap().remove(&id);
                                if let Some(r) = r {
                                    let _ = direct.try_send(r);
                                }
                            }));
                            None
                        }
                    }
                } else {
                    let claim = name == "claim_gpu";
                    // A page taking back its claim after a restart is told about it as a new
                    // claim is.
                    let tells = claim || name == "resume_claim";
                    // The claim's token is the page's own, so a page that reconnects before it
                    // heard about its claim can still take it back (`resume_claim`); a claim
                    // without one is refused, unrecorded.
                    let secret = if claim {
                        match claim_token(&args) {
                            Ok(t) => Some(t),
                            Err(why) => {
                                if direct.try_send(unrecorded_refusal(id, why)).is_err() {
                                    break;
                                }
                                continue;
                            }
                        }
                    } else {
                        None
                    };
                    let r = app
                        .handle
                        .submit(Event::Intent {
                            at: 0,
                            viewer: viewer.clone(),
                            client: Some(client.clone()),
                            id,
                            name,
                            args,
                            check: None,
                            secret,
                        })
                        .await;
                    if let Some(Submitted {
                        receipt: Some(r),
                        stored,
                        claim: seen,
                    }) = r
                    {
                        if r.outcome != Outcome::Committed {
                            // A refusal changed nothing: sent now, not behind a committed
                            // receipt that waits for the store.
                            if direct.try_send(ServerMsg::receipt(&r)).is_err() {
                                tracing::info!(%viewer, "viewer is not reading; closing");
                                break;
                            }
                        } else if receipts
                            .try_send((
                                ServerMsg::receipt(&r),
                                stored,
                                // The claim as the intent left it, before the receipt
                                // waits for the store.
                                seen.filter(|_| tells),
                            ))
                            .is_err()
                        {
                            tracing::info!(%viewer, "viewer is not reading its receipts; closing");
                            break;
                        }
                    }
                    None
                }
            }
        };
        if let Some(r) = reply {
            if direct.try_send(r).is_err() {
                tracing::info!(%viewer, "viewer is not reading; closing");
                break;
            }
        }
    }
    for t in tasks {
        t.abort();
    }
    forward.abort();
    renew.abort();
    writer.abort();
    if let Some(left) = leave(&gate, &admitted, &verified) {
        app.handle.send(left).await;
    }
}

/// Why a `claim_gpu` without a well-formed token is refused (unrecorded).
pub const CLAIM_TOKEN_REFUSAL: &str =
    "claim_gpu needs args.token: 32 to 128 letters, digits, - or _, made by the page";

/// The token of a `claim_gpu`: the page's own (`args.token`, 32 to 128 letters, digits, `-`
/// or `_`), which the page needs to take the claim back after a reconnect.
pub fn claim_token(args: &serde_json::Value) -> Result<String, &'static str> {
    match args.get("token") {
        Some(serde_json::Value::String(t))
            if (32..=128).contains(&t.len())
                && t.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') =>
        {
            Ok(t.clone())
        }
        _ => Err(CLAIM_TOKEN_REFUSAL),
    }
}

/// Keep the claiming viewer, and only it, told about its GPU: a `claim` message whenever
/// the claim changes (restoring, then running with its frame URL, a new camera), and a
/// last one when it ends. `seen` is the claim as it was before its receipt waited for the
/// store, so a claim that ended meanwhile is told as ended.
async fn tell_claim(
    handle: Handle,
    viewer: String,
    direct: mpsc::Sender<ServerMsg>,
    seen: Option<fleet_core::protocol::ClaimMsg>,
) {
    // The last message sent, and the claim as last seen.
    let mut last: Option<fleet_core::protocol::ClaimMsg> = None;
    let mut known = seen;
    loop {
        match handle.claim_for(&viewer).await {
            // Another claim of this viewer (its earlier one ended while the receipt
            // waited, and it claimed again): this watcher's claim is over, and the other one
            // has a watcher of its own.
            Some(c) if known.as_ref().is_some_and(|k| k.id != c.id) => {
                let Some(mut k) = known else { return };
                k.ended = Some(
                    if now_ms() >= k.expires_at {
                        "expired"
                    } else {
                        "released"
                    }
                    .into(),
                );
                k.phase = "ended".into();
                k.frame_url = None;
                k.frame_url_v4 = None;
                let _ = direct.send(ServerMsg::Claim { claim: k }).await;
                return;
            }
            Some(c) => {
                if last.as_ref() != Some(&c) {
                    if direct
                        .send(ServerMsg::Claim { claim: c.clone() })
                        .await
                        .is_err()
                    {
                        return;
                    }
                    last = Some(c.clone());
                }
                known = Some(c);
            }
            None => {
                let Some(mut c) = known else { return };
                let now = now_ms();
                c.ended = Some(
                    if now >= c.expires_at {
                        "expired"
                    } else {
                        "released"
                    }
                    .into(),
                );
                c.phase = "ended".into();
                c.frame_url = None;
                c.frame_url_v4 = None;
                let _ = direct.send(ServerMsg::Claim { claim: c }).await;
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A `word` intent: its first receipt if the id is known; otherwise the pre-checks, then
/// the text check of the whole sky, then the event with the verdict. Returns the receipt
/// to send, or `None` when another task already holds this id (that task sends it).
async fn word(
    app: &App,
    viewer: &str,
    client: &str,
    id: String,
    args: serde_json::Value,
    checking: &Mutex<Option<String>>,
) -> Option<ServerMsg> {
    if let Some(r) = app.handle.receipt(&id).await {
        return Some(ServerMsg::receipt(&r));
    }
    let check = if app.handle.precheck_word(client, &args).await.is_some() {
        // `apply` refuses it for the same reason without a check.
        None
    } else {
        let busy = {
            let mut c = checking.lock().unwrap();
            match c.as_deref() {
                Some(other) if other == id => return None,
                Some(_) => true,
                None => {
                    *c = Some(id.clone());
                    false
                }
            }
        };
        if busy {
            Some(TextCheck {
                verdict: "error".into(),
                reason: "another word from this viewer is being checked".into(),
                sky: app.handle.sky().await,
            })
        } else {
            let text = args
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or_default();
            let normalised = fleet_core::event::normalise_word(text).unwrap_or_default();
            let sky = app.handle.sky().await;
            let verdict = check_text(app.cfg.checker.as_ref(), &sky, &normalised).await;
            *checking.lock().unwrap() = None;
            // A check that cannot run degrades the coordinator until one runs again. One held
            // back by the global rate ran nothing and says nothing about the checker.
            if verdict.reason != super::text_check::CHECK_BUSY {
                app.handle
                    .send(Event::Degraded {
                        at: 0,
                        component: "text_check".into(),
                        reason: (verdict.verdict == "error").then(|| verdict.reason.clone()),
                    })
                    .await;
            }
            Some(verdict)
        }
    };
    app.handle
        .apply(Event::Intent {
            at: 0,
            viewer: viewer.to_string(),
            client: Some(client.to_string()),
            id,
            name: "word".into(),
            args,
            check,
            secret: None,
        })
        .await
        .map(|r| ServerMsg::receipt(&r))
}

#[cfg(test)]
mod tests {
    use super::{client_key, origin_allowed};

    /// One subscriber holds a whole /64, so every address in it shares one key; IPv4 stays
    /// per address.
    #[test]
    fn ipv6_clients_are_keyed_by_their_slash_64() {
        let a = client_key("2001:db8:1:2::1");
        assert_eq!(a, "2001:db8:1:2::/64");
        assert_eq!(client_key("2001:db8:1:2:ffff:ffff:ffff:fffe"), a);
        assert_eq!(client_key("2001:db8:1:2:abcd::9"), a);
        assert_ne!(client_key("2001:db8:1:3::1"), a);
        assert_eq!(client_key("198.51.100.1"), "198.51.100.1");
        assert_ne!(client_key("198.51.100.2"), client_key("198.51.100.1"));
        assert_eq!(client_key("::ffff:198.51.100.1"), "198.51.100.1");
        assert_eq!(client_key("v-3"), "v-3");
    }

    /// A claim watcher that returned (its claim ended) leaves the connection's set at the next
    /// receipt, not when the connection closes: a long-lived page claims again and again.
    #[test]
    fn finished_claim_watchers_are_reaped_while_the_connection_lasts() {
        let src = include_str!("live.rs");
        let body = src
            .split("let mut claims = tokio::task::JoinSet::new();")
            .nth(1)
            .expect("the forwarding task");
        let lp = body
            .find("while let Some((msg, stored, claim)) = receipts_rx.recv().await {")
            .expect("its loop");
        let reap = body
            .find("while claims.try_join_next().is_some() {}")
            .expect("finished watchers are reaped");
        let spawn = body.find("claims.spawn(").expect("a watcher is spawned");
        assert!(lp < reap && reap < spawn, "the reaping is not in the loop");
    }

    #[test]
    fn only_listed_or_own_origins_pass() {
        let allowed = vec!["https://wall.example".to_string()];
        assert!(origin_allowed(None, Some("coord:1"), &allowed));
        assert!(origin_allowed(
            Some("https://wall.example"),
            Some("coord:1"),
            &allowed
        ));
        assert!(origin_allowed(
            Some("https://WALL.example/"),
            Some("coord:1"),
            &allowed
        ));
        assert!(origin_allowed(
            Some("http://coord:1"),
            Some("coord:1"),
            &allowed
        ));
        assert!(!origin_allowed(
            Some("https://evil.example"),
            Some("coord:1"),
            &allowed
        ));
        assert!(!origin_allowed(
            Some("http://coord:1.evil.example"),
            Some("coord:1"),
            &allowed
        ));
        assert!(!origin_allowed(Some("null"), Some("coord:1"), &allowed));
        assert!(!origin_allowed(Some("http://coord:1"), None, &[]));
    }
}
