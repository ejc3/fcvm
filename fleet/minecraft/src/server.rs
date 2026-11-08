//! The tile's HTTP side, the renderer's contract where it applies (fleet/PROTOCOL.md,
//! "Minecraft tile"): `GET /view` (WebSocket, the map and status), `POST /scene` (the agent:
//! the tile's name, the admission keys, who holds it), `GET /health`. Plain HTTP on `PORT` for
//! the host; with `TLS_CERT_FILE` the same routes over TLS on `TLS_PORT` for browsers, the
//! certificate file checked every 2 s and a new version used for later connections.

use crate::admit;
use crate::drive::{DriveMsg, MAX_PER_SEC};
use crate::tile::{Tile, unix_ms};
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// The TLS file's pair and token, as last loaded.
#[derive(Default)]
pub struct Tls {
    pub config: RwLock<Option<Arc<rustls::ServerConfig>>>,
    pub scene_token: RwLock<Option<String>>,
    pub loads: std::sync::atomic::AtomicU64,
}

#[derive(Clone)]
pub struct App {
    pub tile: Arc<Tile>,
    pub tls: Arc<Tls>,
    /// Whether a scene token is required (a fleet run has a TLS file).
    pub fleet: bool,
}

pub fn router(app: App) -> Router {
    Router::new()
        .route("/view", get(view))
        .route("/scene", post(scene))
        .route("/health", get(health))
        .with_state(app)
}

/// The host-side router: the public one plus `POST /restored`, which the restore hook calls
/// (`fcvm snapshot run --on-restore 'mctile restored'`, before the clone takes connections).
/// A restored tile holds the scene of the VM it was copied from, a fork's source borrower
/// included; this drops it, so nobody drives and, in a fleet run, no view opens until the
/// agent gives the tile its own scene. It only takes access away, and is never on TLS.
pub fn local_router(app: App) -> Router {
    router(app.clone()).merge(
        Router::new()
            .route("/restored", post(restored))
            .with_state(app),
    )
}

async fn restored(State(app): State<App>) -> Json<serde_json::Value> {
    forget_scene(&app.tile);
    Json(serde_json::json!({"ok": true}))
}

pub fn forget_scene(tile: &Tile) {
    {
        let mut s = tile.scene.write();
        s.driver = None;
        s.driver_until = None;
        s.set = false;
    }
    tile.drive.lock().set_driver(None, None);
}

async fn health(State(app): State<App>) -> Json<serde_json::Value> {
    let mut h = app.tile.health();
    h["tls"] = serde_json::json!({"loaded": app.tls.config.read().is_some(), "loads": app.tls.loads.load(Ordering::Relaxed)});
    Json(h)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SceneBody {
    tile: String,
    generation: u64,
    #[serde(default)]
    admit_keys: Vec<String>,
    #[serde(default)]
    driver: Option<String>,
    #[serde(default)]
    driver_until: Option<u64>,
}

fn authorized(app: &App, headers: &HeaderMap) -> Result<(), Box<Response>> {
    if !app.fleet {
        return Ok(());
    }
    let Some(token) = app.tls.scene_token.read().clone() else {
        return Err(Box::new(
            (StatusCode::SERVICE_UNAVAILABLE, "no scene token yet").into_response(),
        ));
    };
    let got = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match got {
        Some(g)
            if g.len() == token.len()
                && g.bytes()
                    .zip(token.bytes())
                    .fold(0u8, |a, (x, y)| a | (x ^ y))
                    == 0 =>
        {
            Ok(())
        }
        _ => Err(Box::new(
            (StatusCode::UNAUTHORIZED, "scene token required").into_response(),
        )),
    }
}

async fn scene(State(app): State<App>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    if let Err(r) = authorized(&app, &headers) {
        return *r;
    }
    let b: SceneBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("scene: {e}")).into_response(),
    };
    if b.tile.is_empty()
        || b.tile.len() > 64
        || b.driver
            .as_ref()
            .is_some_and(|d| d.is_empty() || d.len() > 128)
    {
        return (
            StatusCode::BAD_REQUEST,
            "scene: tile or driver out of range",
        )
            .into_response();
    }
    let keys = match admit::parse_keys(&b.admit_keys) {
        Ok(k) => k,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    {
        let mut s = app.tile.scene.write();
        s.tile = b.tile;
        s.generation = b.generation;
        s.admit_keys = keys;
        s.driver = b.driver.clone();
        s.driver_until = b.driver_until;
        s.set = true;
    }
    app.tile.drive.lock().set_driver(b.driver, b.driver_until);
    Json(serde_json::json!({"ok": true})).into_response()
}

async fn view(State(app): State<App>, ws: WebSocketUpgrade) -> Response {
    if app.fleet && !app.tile.scene.read().set {
        return (StatusCode::SERVICE_UNAVAILABLE, "no scene yet").into_response();
    }
    ws.on_upgrade(move |socket| view_socket(app, socket))
}

fn text(v: serde_json::Value) -> Message {
    Message::Text(v.to_string().into())
}

/// Who the socket acts as, from its first message: the viewer its direct token names, or with
/// no admission keys (a local run) the `viewer` it says.
async fn admit_first(app: &App, socket: &mut WebSocket) -> Option<String> {
    let first = tokio::time::timeout(Duration::from_secs(5), socket.recv())
        .await
        .ok()??
        .ok()?;
    let Message::Text(t) = first else { return None };
    let v: serde_json::Value = serde_json::from_str(&t).ok()?;
    let keys = app.tile.scene.read().admit_keys.clone();
    if keys.is_empty() {
        return Some(
            v.get("viewer")
                .and_then(|x| x.as_str())
                .unwrap_or("local")
                .to_string(),
        );
    }
    admit::viewer_of(&keys, v.get("admit")?.as_str()?, unix_ms() / 1000)
}

async fn view_socket(app: App, mut socket: WebSocket) {
    let Some(viewer) = admit_first(&app, &mut socket).await else {
        let _ = socket
            .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                code: 1008,
                reason: "admission refused".into(),
            })))
            .await;
        return;
    };
    let tile = &app.tile;
    tile.viewers.fetch_add(1, Ordering::Relaxed);
    let mut rx = tile.tx.subscribe();
    let hello = {
        let s = tile.scene.read();
        serde_json::json!({
            "type": "hello", "tile": s.tile, "generation": s.generation, "viewer": viewer,
            "materials": crate::palette::MATERIALS.to_vec(), "bots": tile.cfg.bots,
            "bot_names": (0..tile.cfg.bots).map(|n| crate::bots::name(tile, n)).collect::<Vec<_>>(),
            "driver": s.driver,
        })
    };
    let map = tile.grid.read().as_ref().map(|g| {
        serde_json::json!({"type": "map", "seq": tile.map_seq.load(Ordering::Relaxed), "region": g.region, "cells": g.encode()})
    });
    let mut ok = socket.send(text(hello)).await.is_ok();
    if let Some(m) = map {
        ok = ok && socket.send(text(m)).await.is_ok();
    }
    let mut window = (unix_ms(), 0u32);
    while ok {
        tokio::select! {
            m = rx.recv() => match m {
                Ok(m) => ok = socket.send(Message::Text(m.to_string().into())).await.is_ok(),
                // A socket that fell behind gets the whole map again.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let map = tile.grid.read().as_ref().map(|g| serde_json::json!({"type": "map", "seq": tile.map_seq.load(Ordering::Relaxed), "region": g.region, "cells": g.encode()}));
                    if let Some(m) = map { ok = socket.send(text(m)).await.is_ok(); }
                }
                Err(_) => break,
            },
            m = socket.recv() => match m {
                Some(Ok(Message::Text(t))) => {
                    let now = unix_ms();
                    if now >= window.0 + 1000 { window = (now, 0); }
                    window.1 += 1;
                    let reply = if window.1 > MAX_PER_SEC {
                        Err("too many drive messages; at most 30 a second".to_string())
                    } else {
                        match serde_json::from_str::<DriveMsg>(&t) {
                            Ok(d) if d.kind == "drive" => tile.drive.lock().take(&viewer, &d, now).map(|_| d.seq),
                            Ok(_) => Err("unknown message type".into()),
                            Err(e) => Err(format!("not a drive message: {e}")),
                        }
                    };
                    let seq = serde_json::from_str::<serde_json::Value>(&t).ok().and_then(|v| v.get("seq").and_then(|s| s.as_u64()));
                    ok = socket.send(text(match reply {
                        Ok(seq) => serde_json::json!({"type": "drive_ack", "seq": seq, "ok": true, "at": now}),
                        Err(e) => serde_json::json!({"type": "drive_ack", "seq": seq, "ok": false, "reason": e}),
                    })).await.is_ok();
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            }
        }
    }
    tile.viewers.fetch_sub(1, Ordering::Relaxed);
}

/// The TLS file: `{"cert", "key", "scene_token"}`.
pub fn load_tls(path: &PathBuf) -> anyhow::Result<(Arc<rustls::ServerConfig>, Option<String>)> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let field = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let certs = rustls_pemfile::certs(&mut field("cert").unwrap_or_default().as_bytes())
        .collect::<Result<Vec<_>, _>>()?;
    anyhow::ensure!(!certs.is_empty(), "the TLS file holds no certificate");
    let key = rustls_pemfile::private_key(&mut field("key").unwrap_or_default().as_bytes())?
        .ok_or_else(|| anyhow::anyhow!("the TLS file holds no key"))?;
    let mut c = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])?
    .with_no_client_auth()
    .with_single_cert(certs, key)?;
    c.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok((Arc::new(c), field("scene_token").filter(|t| !t.is_empty())))
}

/// Check the TLS file every 2 s and load a new version of it.
pub async fn watch_tls(path: PathBuf, tls: Arc<Tls>) {
    let mut last = None;
    loop {
        let stamp = std::fs::metadata(&path).ok().map(|m| {
            use std::os::unix::fs::MetadataExt;
            (m.ino(), m.len(), m.mtime(), m.mtime_nsec())
        });
        if stamp.is_none() {
            // A removed file authorizes nothing.
            *tls.scene_token.write() = None;
        } else if stamp != last {
            match load_tls(&path) {
                Ok((c, token)) => {
                    *tls.config.write() = Some(c);
                    *tls.scene_token.write() = token;
                    tls.loads.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "the TLS file did not load; keeping the pair in use")
                }
            }
        }
        last = stamp;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

pub async fn serve_plain(addr: std::net::SocketAddr, router: Router) -> anyhow::Result<()> {
    let l = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(l, router).await?;
    Ok(())
}

pub async fn serve_tls(
    addr: std::net::SocketAddr,
    router: Router,
    tls: Arc<Tls>,
) -> anyhow::Result<()> {
    let l = tokio::net::TcpListener::bind(addr).await?;
    loop {
        let (tcp, _) = l.accept().await?;
        let Some(config) = tls.config.read().clone() else {
            continue;
        };
        let router = router.clone();
        tokio::spawn(async move {
            let Ok(Ok(stream)) = tokio::time::timeout(
                Duration::from_secs(10),
                tokio_rustls::TlsAcceptor::from(config).accept(tcp),
            )
            .await
            else {
                return;
            };
            let svc = hyper_util::service::TowerToHyperService::new(router);
            let _ =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection_with_upgrades(hyper_util::rt::TokioIo::new(stream), svc)
                    .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMsg;

    const KEY: [u8; 32] = [9u8; 32];

    async fn serve(fleet: bool) -> (std::net::SocketAddr, Arc<Tile>) {
        let tile = Tile::new(crate::tile::Cfg {
            bots: 0,
            server: "127.0.0.1:1".into(),
            region_side: 16,
            bot_prefix: "b".into(),
        });
        let tls = Arc::new(Tls::default());
        if fleet {
            *tls.scene_token.write() = Some("tok".into());
        }
        let app = App {
            tile: tile.clone(),
            tls,
            fleet,
        };
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, local_router(app)).await.unwrap() });
        (addr, tile)
    }

    async fn post_scene(
        addr: std::net::SocketAddr,
        auth: Option<&str>,
        body: serde_json::Value,
    ) -> u16 {
        let mut req = "POST /scene HTTP/1.1\r\nhost: x\r\ncontent-type: application/json\r\nconnection: close\r\n".to_string();
        if let Some(a) = auth {
            req.push_str(&format!("authorization: Bearer {a}\r\n"));
        }
        let b = body.to_string();
        req.push_str(&format!("content-length: {}\r\n\r\n{b}", b.len()));
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out.split(' ').nth(1).unwrap().parse().unwrap()
    }

    fn scene(driver: Option<&str>) -> serde_json::Value {
        serde_json::json!({"tile": "mc-1", "generation": 2, "admit_keys": [hex::encode(KEY)], "driver": driver})
    }

    /// Invariant 3: the view socket opens only with a direct token valid under the scene's keys.
    #[tokio::test]
    async fn the_view_needs_a_scene_then_a_valid_direct_token() {
        let (addr, _) = serve(true).await;
        let url = format!("ws://{addr}/view");
        assert!(
            tokio_tungstenite::connect_async(&url).await.is_err(),
            "no view before a scene"
        );
        assert_eq!(post_scene(addr, None, scene(None)).await, 401);
        assert_eq!(post_scene(addr, Some("nope"), scene(None)).await, 401);
        assert_eq!(post_scene(addr, Some("tok"), scene(None)).await, 200);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let forged = crate::admit::make_token(&[1u8; 32], "v-1", unix_ms() / 1000 + 60);
        ws.send(WsMsg::text(
            serde_json::json!({"admit": forged}).to_string(),
        ))
        .await
        .unwrap();
        match ws.next().await {
            Some(Ok(WsMsg::Close(Some(f)))) => assert_eq!(u16::from(f.code), 1008),
            other => panic!("a forged token was not refused: {other:?}"),
        }
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let good = crate::admit::make_token(&KEY, "v-1", unix_ms() / 1000 + 60);
        ws.send(WsMsg::text(serde_json::json!({"admit": good}).to_string()))
            .await
            .unwrap();
        let hello: serde_json::Value = match ws.next().await {
            Some(Ok(WsMsg::Text(t))) => serde_json::from_str(&t).unwrap(),
            other => panic!("no hello: {other:?}"),
        };
        assert_eq!(hello["type"], "hello");
        assert_eq!(hello["viewer"], "v-1");
        assert_eq!(hello["tile"], "mc-1");
    }

    async fn drive_reply(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        seq: u64,
    ) -> serde_json::Value {
        ws.send(WsMsg::text(
            serde_json::json!({"type": "drive", "seq": seq, "keys": ["forward"], "yaw": 0.0})
                .to_string(),
        ))
        .await
        .unwrap();
        loop {
            if let Some(Ok(WsMsg::Text(t))) = ws.next().await {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "drive_ack" {
                    return v;
                }
            } else {
                panic!("the socket closed");
            }
        }
    }

    /// A restored tile forgets the scene it was copied with (a fork's source borrower among
    /// it): nobody drives, and no view opens until the agent posts the tile's own scene.
    #[tokio::test]
    async fn the_restore_hook_drops_the_copied_driver_and_closes_the_view() {
        let (addr, tile) = serve(true).await;
        assert_eq!(post_scene(addr, Some("tok"), scene(Some("v-1"))).await, 200);
        assert_eq!(tile.drive.lock().driver.as_deref(), Some("v-1"));
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        s.write_all(
            b"POST /restored HTTP/1.1\r\nhost: x\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        )
        .await
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        assert!(out.starts_with("HTTP/1.1 200"), "{out}");
        assert_eq!(tile.drive.lock().driver, None);
        assert!(
            tokio_tungstenite::connect_async(format!("ws://{addr}/view"))
                .await
                .is_err(),
            "a view before the new scene"
        );
        assert_eq!(post_scene(addr, Some("tok"), scene(None)).await, 200);
        assert!(
            tokio_tungstenite::connect_async(format!("ws://{addr}/view"))
                .await
                .is_ok()
        );
    }

    /// Invariant 1: only the viewer the scene names as driver moves a bot, and a new driver
    /// replaces the old one at once.
    #[tokio::test]
    async fn only_the_scenes_driver_drives() {
        let (addr, tile) = serve(true).await;
        assert_eq!(post_scene(addr, Some("tok"), scene(Some("v-2"))).await, 200);
        let url = format!("ws://{addr}/view");
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let t = crate::admit::make_token(&KEY, "v-1", unix_ms() / 1000 + 60);
        ws.send(WsMsg::text(serde_json::json!({"admit": t}).to_string()))
            .await
            .unwrap();
        let r = drive_reply(&mut ws, 1).await;
        assert_eq!(r["ok"], false, "{r}");
        assert_eq!(r["reason"], "another viewer holds this tile");
        assert!(tile.drive.lock().keys.is_empty());
        assert_eq!(post_scene(addr, Some("tok"), scene(Some("v-1"))).await, 200);
        let r = drive_reply(&mut ws, 2).await;
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(tile.drive.lock().keys, vec![crate::drive::Key::Forward]);
        assert_eq!(post_scene(addr, Some("tok"), scene(None)).await, 200);
        assert!(
            tile.drive.lock().keys.is_empty(),
            "the borrow's end drops its keys"
        );
        let r = drive_reply(&mut ws, 3).await;
        assert_eq!(r["reason"], "nobody holds this tile; take it over first");
    }
}
