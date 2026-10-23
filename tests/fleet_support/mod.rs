//! Shared by the fleet tests: fake renderers and the fake fcvm's event log.
#![allow(dead_code)]

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub fn fake_fcvm() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-fcvm.sh")
}

/// `count` consecutive free TCP ports on 127.0.0.1, already bound.
pub fn consecutive_listeners(count: usize) -> (u16, Vec<std::net::TcpListener>) {
    for _ in 0..200 {
        let first = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = first.local_addr().unwrap().port();
        if base as usize + count > 65535 {
            continue;
        }
        let mut ls = vec![first];
        for i in 1..count {
            match std::net::TcpListener::bind(("127.0.0.1", base + i as u16)) {
                Ok(l) => ls.push(l),
                Err(_) => break,
            }
        }
        if ls.len() == count {
            return (base, ls);
        }
    }
    panic!("no {count} consecutive free ports");
}

#[derive(Clone)]
struct Renderer {
    frames: Arc<std::sync::atomic::AtomicU64>,
    marker: PathBuf,
    scenes: Arc<Mutex<Vec<serde_json::Value>>>,
    budgets: Arc<Mutex<Vec<serde_json::Value>>>,
}

/// Scene bodies each fake renderer received, by port index.
pub type Scenes = Vec<Arc<Mutex<Vec<serde_json::Value>>>>;

/// One fake renderer per listener. It answers `/health` (with a frame) and accepts
/// `/scene` only while the fake fcvm on its port is up.
pub fn start_renderers(dir: &Path, base: u16, listeners: Vec<std::net::TcpListener>) -> Scenes {
    start_renderers_with_budgets(dir, base, listeners).0
}

/// As [`start_renderers`], also returning the `/budget` bodies each renderer received.
pub fn start_renderers_with_budgets(
    dir: &Path,
    base: u16,
    listeners: Vec<std::net::TcpListener>,
) -> (Scenes, Scenes) {
    let mut out = Vec::new();
    let mut budgets = Vec::new();
    for (i, l) in listeners.into_iter().enumerate() {
        l.set_nonblocking(true).unwrap();
        let state = Renderer {
            frames: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            marker: dir.join(format!("port-{}.up", base + i as u16)),
            scenes: Arc::new(Mutex::new(Vec::new())),
            budgets: Arc::new(Mutex::new(Vec::new())),
        };
        out.push(state.scenes.clone());
        budgets.push(state.budgets.clone());
        let app = Router::new()
            .route("/health", get(health))
            .route("/scene", post(scene))
            .route("/budget", post(budget))
            .with_state(state);
        let listener = tokio::net::TcpListener::from_std(l).unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    }
    (out, budgets)
}

async fn health(State(r): State<Renderer>) -> Result<Json<serde_json::Value>, StatusCode> {
    if !r.marker.exists() {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    // Like the fleet renderer: a frame counter that moves while it is up.
    let frame = r.frames.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    Ok(Json(serde_json::json!({
        "fps": 30.0, "t": 1.5, "frame": frame, "ready": true, "budget_fit": 1.0e6,
        "gpu": {"name": "fake", "temp_c": 40},
        "startup_ms": {"gpu_ready": 1500, "first_frame_drawn": 1600, "first_frame_sent": -1}
    })))
}

async fn scene(State(r): State<Renderer>, Json(body): Json<serde_json::Value>) -> StatusCode {
    if !r.marker.exists() {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    r.scenes.lock().unwrap().push(body);
    StatusCode::OK
}

async fn budget(State(r): State<Renderer>, Json(body): Json<serde_json::Value>) -> StatusCode {
    if !r.marker.exists() {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    r.budgets.lock().unwrap().push(body);
    StatusCode::OK
}

#[derive(Debug, Clone, PartialEq)]
pub struct LogEvent {
    pub kind: String,
    pub name: String,
    pub pid: u32,
    pub ns: u128,
}

pub fn events(dir: &Path) -> Vec<LogEvent> {
    let text = std::fs::read_to_string(dir.join("events.log")).unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let p: Vec<&str> = l.split_whitespace().collect();
            Some(LogEvent {
                kind: p.first()?.to_string(),
                name: p.get(1)?.to_string(),
                pid: p.get(2)?.parse().ok()?,
                ns: p.get(3)?.parse().ok()?,
            })
        })
        .collect()
}
