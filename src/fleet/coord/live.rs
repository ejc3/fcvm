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
    ws.max_message_size(MAX_CLIENT_MESSAGE)
        .on_upgrade(move |socket| viewer(socket, app, client))
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

async fn viewer(socket: WebSocket, app: App, client: String) {
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
    let (direct, mut direct_rx) = mpsc::channel::<ServerMsg>(DIRECT_QUEUE);
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
    while let Some(Ok(msg)) = stream.next().await {
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
    writer.abort();
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
