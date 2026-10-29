//! The browser side: `GET /live` (WebSocket), `GET /poster.jpg` and `GET /healthz`.
//!
//! Each socket has a reader (intents, pings, verification) and a writer. The writer sends
//! the viewer's id, the current snapshot on connect and the latest one whenever the
//! version moves; receipts, clock replies, verification results and the viewer's own claim
//! reach it through a bounded channel. A client that stops reading until that channel is
//! full is disconnected.
//!
//! Every intent carries the connection's client address (`CF-Connecting-IP` when trusted,
//! else the peer address), which the per-client limits are keyed by. With a Turnstile
//! secret configured, a connection's intents are refused until it has passed `verify`.

use super::admission::{Admitted, Gate};
use super::poster::Poster;
use super::text_check::{check_text, TextChecker};
use super::turnstile::Verifier;
use super::Handle;
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
use tokio::sync::mpsc;

/// The largest message a browser may send.
const MAX_CLIENT_MESSAGE: usize = 64 * 1024;
/// Receipts and clock replies a socket may have queued before it is dropped.
const DIRECT_QUEUE: usize = 256;

pub struct LiveConfig {
    pub checker: Arc<dyn TextChecker>,
    /// Browser origins allowed besides the coordinator's own.
    pub allowed_origins: Vec<String>,
    pub verifier: Arc<dyn Verifier>,
    /// Sent in the hello when the verifier asks for a token.
    pub turnstile_site_key: Option<String>,
    /// Key clients by `CF-Connecting-IP` (only behind Cloudflare, which sets it).
    pub trust_cf_connecting_ip: bool,
    pub poster: Arc<Poster>,
    /// Who gets direct streams and who the broadcast, and their tokens.
    pub gate: Arc<Gate>,
}

#[derive(Clone)]
pub struct App {
    pub handle: Handle,
    pub cfg: Arc<LiveConfig>,
    next_viewer: Arc<AtomicU64>,
}

pub fn router(handle: Handle, cfg: LiveConfig) -> Router {
    Router::new()
        .route("/live", get(live))
        .route("/agent", get(agent_socket))
        .route("/poster.jpg", get(poster))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(App {
            handle,
            cfg: Arc::new(cfg),
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
/// path before a request reaches the coordinator.
async fn agent_socket(ws: WebSocketUpgrade, State(app): State<App>) -> Response {
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
            if let Err(e) = super::agents::serve_connection(serve_in, serve_out, &app.handle).await
            {
                tracing::warn!(error = %format!("{e:#}"), "agent connection ended");
            }
            inbound.abort();
            let _ = outbound.await;
        })
        .into_response()
}

/// The address the per-client limits use.
pub fn client_address(headers: &HeaderMap, peer: SocketAddr, trust_cf: bool) -> String {
    if trust_cf {
        if let Some(ip) = headers
            .get("cf-connecting-ip")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| v.parse::<std::net::IpAddr>().is_ok())
        {
            return ip.to_string();
        }
    }
    peer.ip().to_string()
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
    let client = client_address(&headers, peer, app.cfg.trust_cf_connecting_ip);
    let page = Page {
        origin: origin.map(|o| o.trim_end_matches('/').to_string()),
    };
    ws.max_message_size(MAX_CLIENT_MESSAGE)
        .on_upgrade(move |socket| viewer(socket, app, client, page))
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

/// What the page said about itself when it connected.
struct Page {
    /// Its origin, which a playback token is bound to.
    origin: Option<String>,
}

/// How long a connection waits for its first message, which may hold a seat ticket, before
/// it is admitted without one. The page sends one at once, so only a client that sends
/// nothing waits this long.
pub const SEAT_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Why a late viewer's `claim_gpu` or `claim_phone` is refused: a private GPU is a direct
/// stream too, and a borrowed phone is shown from its direct stream.
pub const LATE_CLAIM: &str = "direct streams are full, so this page watches the broadcast";

async fn viewer(socket: WebSocket, app: App, client: String, page: Page) {
    let viewer = format!("v-{}", app.next_viewer.fetch_add(1, Ordering::Relaxed));
    app.handle
        .send(Event::ViewerJoined {
            at: 0,
            viewer: viewer.clone(),
            client: Some(client.clone()),
        })
        .await;
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
        app.handle
            .send(Event::ViewerLeft {
                at: 0,
                viewer: viewer.clone(),
            })
            .await;
        return;
    }
    // Then how it receives the image. A seat ticket comes in the first message; any other
    // first message is handled below like the ones after it.
    let mut pending: Option<Message> = None;
    let seat = match tokio::time::timeout(SEAT_WAIT, stream.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => match serde_json::from_str::<ClientMsg>(&t) {
            Ok(ClientMsg::Seat { ticket }) => ticket,
            _ => {
                pending = Some(Message::Text(t));
                None
            }
        },
        Ok(Some(Ok(m))) => {
            pending = Some(m);
            None
        }
        _ => None,
    };
    // A seat is held from here until the connection ends.
    let gate = app.cfg.gate.clone();
    let admitted = gate.seats.admit(seat.as_deref(), std::time::Instant::now());
    let late = admitted == Admitted::Late;
    let admission = gate.message(
        &admitted,
        &viewer,
        page.origin.as_deref(),
        now_ms(),
        std::time::Instant::now(),
    );
    let release = |gate: &Gate| {
        if let Admitted::Seat { ticket } = &admitted {
            gate.seats.release(ticket, std::time::Instant::now());
        }
    };
    if sink
        .send(Message::Text(
            text(&ServerMsg::Admission(admission.clone())).into(),
        ))
        .await
        .is_err()
    {
        release(&gate);
        app.handle
            .send(Event::ViewerLeft {
                at: 0,
                viewer: viewer.clone(),
            })
            .await;
        return;
    }
    let (direct, mut direct_rx) = mpsc::channel::<ServerMsg>(DIRECT_QUEUE);
    // Fresh tokens before the old ones expire, and a late viewer told when a seat frees.
    let renew = {
        let gate = gate.clone();
        let admitted = admitted.clone();
        let viewer = viewer.clone();
        let origin = page.origin.clone();
        let direct = direct.clone();
        tokio::spawn(async move {
            let mut last = admission;
            let mut sent_at = std::time::Instant::now();
            loop {
                tokio::time::sleep(gate.tick).await;
                let now = std::time::Instant::now();
                let free_changed = late && gate.seats.free(now) != last.seat_free;
                if !free_changed && now.duration_since(sent_at) < super::admission::DIRECT_REFRESH {
                    continue;
                }
                last = gate.message(&admitted, &viewer, origin.as_deref(), now_ms(), now);
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

    let verified = Arc::new(AtomicBool::new(!app.cfg.verifier.required()));
    // The one `word` this viewer has at the text check, if any.
    let checking: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
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
            Ok(ClientMsg::Visibility { visible }) => {
                app.handle
                    .send(Event::Visibility {
                        at: 0,
                        viewer: viewer.clone(),
                        visible,
                    })
                    .await;
                None
            }
            Ok(ClientMsg::Verify { token }) => {
                let r = app.cfg.verifier.verify(&token, &client).await;
                let (ok, reason) = match r {
                    Ok(None) => (true, None),
                    Ok(Some(why)) => (false, Some(why)),
                    Err(e) => (false, Some(format!("{e:#}"))),
                };
                if ok {
                    verified.store(true, Ordering::SeqCst);
                }
                Some(ServerMsg::Verified { ok, reason })
            }
            Ok(ClientMsg::Intent { id, name, args }) => {
                if id.is_empty() || id.len() > 128 {
                    Some(ServerMsg::Error {
                        reason: "an intent id must be 1 to 128 bytes".into(),
                    })
                } else if !verified.load(Ordering::SeqCst) {
                    Some(unrecorded_refusal(id, "verify first"))
                } else if late && (name == "claim_gpu" || name == "claim_phone") {
                    Some(unrecorded_refusal(id, LATE_CLAIM))
                } else if name == "word" {
                    let app = app.clone();
                    let viewer = viewer.clone();
                    let client = client.clone();
                    let direct = direct.clone();
                    let checking = checking.clone();
                    tasks.push(tokio::spawn(async move {
                        if let Some(r) = word(&app, &viewer, &client, id, args, &checking).await {
                            let _ = direct.try_send(r);
                        }
                    }));
                    None
                } else {
                    let claim = name == "claim_gpu";
                    let r = app
                        .handle
                        .apply(Event::Intent {
                            at: 0,
                            viewer: viewer.clone(),
                            client: Some(client.clone()),
                            id,
                            name,
                            args,
                            check: None,
                            secret: claim.then(|| uuid::Uuid::new_v4().simple().to_string()),
                        })
                        .await;
                    if claim && r.as_ref().is_some_and(|r| r.outcome == Outcome::Committed) {
                        tasks.push(tokio::spawn(tell_claim(
                            app.handle.clone(),
                            viewer.clone(),
                            direct.clone(),
                        )));
                    }
                    r.map(|r| ServerMsg::receipt(&r))
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
    renew.abort();
    writer.abort();
    release(&gate);
    app.handle
        .send(Event::ViewerLeft {
            at: 0,
            viewer: viewer.clone(),
        })
        .await;
}

/// Keep the claiming viewer, and only it, told about its GPU: a `claim` message whenever
/// the claim changes (restoring, then running with its frame URL, a new camera), and a
/// last one when it ends.
async fn tell_claim(handle: Handle, viewer: String, direct: mpsc::Sender<ServerMsg>) {
    let mut last: Option<fleet_core::protocol::ClaimMsg> = None;
    loop {
        match handle.claim_for(&viewer).await {
            Some(c) => {
                if last.as_ref() != Some(&c) {
                    if direct
                        .send(ServerMsg::Claim { claim: c.clone() })
                        .await
                        .is_err()
                    {
                        return;
                    }
                    last = Some(c);
                }
            }
            None => {
                let Some(mut c) = last else { return };
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
            // A check that cannot run degrades the coordinator until one runs again.
            app.handle
                .send(Event::Degraded {
                    at: 0,
                    component: "text_check".into(),
                    reason: (verdict.verdict == "error").then(|| verdict.reason.clone()),
                })
                .await;
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
    use super::origin_allowed;

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
