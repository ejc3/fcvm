//! The browser side: `GET /live` (WebSocket) and `GET /healthz`.
//!
//! Each socket has a reader (intents, pings) and a writer. The writer sends the current
//! snapshot on connect and the latest one whenever the version moves; receipts, clock
//! replies and errors reach it through a bounded channel. A client that stops reading
//! until that channel is full is disconnected.

use super::text_check::{check_text, TextChecker};
use super::Handle;
use crate::fleet::now_ms;
use anyhow::Result;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use fleet_core::protocol::{ClientMsg, ServerMsg};
use fleet_core::state::TextCheck;
use fleet_core::Event;
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// The largest message a browser may send.
const MAX_CLIENT_MESSAGE: usize = 64 * 1024;
/// Receipts and clock replies a socket may have queued before it is dropped.
const DIRECT_QUEUE: usize = 256;

#[derive(Clone)]
pub struct App {
    pub handle: Handle,
    pub checker: Arc<dyn TextChecker>,
    next_viewer: Arc<AtomicU64>,
}

pub fn router(handle: Handle, checker: Arc<dyn TextChecker>) -> Router {
    Router::new()
        .route("/live", get(live))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(App {
            handle,
            checker,
            next_viewer: Arc::new(AtomicU64::new(1)),
        })
}

pub async fn serve(
    listener: tokio::net::TcpListener,
    handle: Handle,
    checker: Arc<dyn TextChecker>,
) -> Result<()> {
    axum::serve(listener, router(handle, checker)).await?;
    Ok(())
}

async fn live(ws: WebSocketUpgrade, State(app): State<App>) -> impl IntoResponse {
    ws.max_message_size(MAX_CLIENT_MESSAGE)
        .on_upgrade(move |socket| viewer(socket, app))
}

fn text(msg: &ServerMsg) -> String {
    serde_json::to_string(msg).expect("server messages serialize")
}

async fn viewer(socket: WebSocket, app: App) {
    let viewer = format!("v-{}", app.next_viewer.fetch_add(1, Ordering::Relaxed));
    app.handle
        .send(Event::ViewerJoined {
            at: 0,
            viewer: viewer.clone(),
        })
        .await;
    let (mut sink, mut stream) = socket.split();
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

    // The one `word` this viewer has at the text check, if any.
    let checking: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
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
                reason: format!("message is not a ping or an intent: {e}"),
            }),
            Ok(ClientMsg::Ping { client_unix_ms }) => Some(ServerMsg::Clock {
                server_unix_ms: now_ms(),
                client_unix_ms,
            }),
            Ok(ClientMsg::Intent { id, name, args }) => {
                if id.is_empty() || id.len() > 128 {
                    Some(ServerMsg::Error {
                        reason: "an intent id must be 1 to 128 bytes".into(),
                    })
                } else if name == "word" {
                    let app = app.clone();
                    let viewer = viewer.clone();
                    let direct = direct.clone();
                    let checking = checking.clone();
                    tokio::spawn(async move {
                        if let Some(r) = word(&app, &viewer, id, args, &checking).await {
                            let _ = direct.try_send(r);
                        }
                    });
                    None
                } else {
                    app.handle
                        .apply(Event::Intent {
                            at: 0,
                            viewer: viewer.clone(),
                            id,
                            name,
                            args,
                            check: None,
                        })
                        .await
                        .map(|r| ServerMsg::receipt(&r))
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
    writer.abort();
    app.handle
        .send(Event::ViewerLeft {
            at: 0,
            viewer: viewer.clone(),
        })
        .await;
}

/// A `word` intent: its first receipt if the id is known; otherwise the pre-checks, then
/// the text check, then the event with the verdict. Returns the receipt to send, or `None`
/// when another task already holds this id (that task sends the receipt).
async fn word(
    app: &App,
    viewer: &str,
    id: String,
    args: serde_json::Value,
    checking: &Mutex<Option<String>>,
) -> Option<ServerMsg> {
    if let Some(r) = app.handle.receipt(&id).await {
        return Some(ServerMsg::receipt(&r));
    }
    let check = if app.handle.precheck_word(viewer, &args).await.is_some() {
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
            })
        } else {
            let text = args
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or_default();
            let normalised = fleet_core::event::normalise_word(text).unwrap_or_default();
            let verdict = check_text(app.checker.as_ref(), &normalised).await;
            *checking.lock().unwrap() = None;
            Some(verdict)
        }
    };
    app.handle
        .apply(Event::Intent {
            at: 0,
            viewer: viewer.to_string(),
            id,
            name: "word".into(),
            args,
            check,
        })
        .await
        .map(|r| ServerMsg::receipt(&r))
}
