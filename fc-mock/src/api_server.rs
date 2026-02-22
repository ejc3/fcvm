//! Firecracker-compatible REST API server on a Unix socket.
//!
//! Accepts all Firecracker API endpoints, stores MMDS and vsock config,
//! and triggers container launch on InstanceStart.

use anyhow::{Context, Result};
use hyper::service::service_fn;
use hyper::{Body, Method, Request, Response, StatusCode};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};
use tracing::{debug, info, warn};

/// Shared mock state across all API requests.
#[derive(Clone)]
pub struct SharedState(Arc<Mutex<MockState>>);

struct MockState {
    /// MMDS data (contains container-plan)
    mmds_data: Option<serde_json::Value>,
    /// Vsock config (contains uds_path)
    vsock_uds_path: Option<String>,
    /// Whether InstanceStart has been called
    started: bool,
    /// Shutdown notifier (triggers when container exits)
    shutdown: Option<Arc<Notify>>,
}

impl SharedState {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(MockState {
            mmds_data: None,
            vsock_uds_path: None,
            started: false,
            shutdown: None,
        })))
    }

    pub fn set_shutdown(&self, shutdown: Arc<Notify>) {
        // Can't await in a sync context, so use try_lock
        if let Ok(mut state) = self.0.try_lock() {
            state.shutdown = Some(shutdown);
        }
    }
}

/// Start the API server on a Unix socket.
///
/// Returns a JoinHandle that resolves when the server exits.
pub async fn start_server(
    socket_path: PathBuf,
    state: SharedState,
    shutdown: Arc<Notify>,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    // Remove stale socket
    let _ = std::fs::remove_file(&socket_path);

    // Store shutdown notifier in state
    state.set_shutdown(shutdown.clone());

    let listener = tokio::net::UnixListener::bind(&socket_path).context("binding API socket")?;

    info!(socket = %socket_path.display(), "API socket bound");

    let handle = tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    warn!("accept error: {}", e);
                    continue;
                }
            };

            let state = state.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req| {
                    let state = state.clone();
                    async move { handle_request(req, state).await }
                });
                if let Err(e) = hyper::server::conn::Http::new()
                    .serve_connection(stream, service)
                    .await
                {
                    // Connection reset is normal during shutdown
                    if !e.is_incomplete_message() {
                        debug!("connection error: {}", e);
                    }
                }
            });
        }

        #[allow(unreachable_code)]
        Ok::<_, anyhow::Error>(())
    });

    Ok(handle)
}

/// Handle a single API request.
async fn handle_request(
    req: Request<Body>,
    state: SharedState,
) -> Result<Response<Body>, hyper::Error> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    debug!(%method, %path, "API request");

    let body_bytes = hyper::body::to_bytes(req.into_body()).await?;

    let result: Result<(), String> = match (method, path.as_str()) {
        // Config endpoints - store and return 204
        (Method::PUT, "/boot-source") => Ok(()),
        (Method::PUT, "/machine-config") => Ok(()),
        (Method::PUT, p) if p.starts_with("/drives/") => Ok(()),
        (Method::PUT, p) if p.starts_with("/network-interfaces/") => Ok(()),
        (Method::PUT, "/mmds/config") => Ok(()),
        (Method::PUT, "/entropy") => Ok(()),
        (Method::PUT, "/balloon") => Ok(()),

        // MMDS data - store for container plan extraction
        (Method::PUT, "/mmds") => {
            let data: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap_or_default();
            let mut s = state.0.lock().await;
            s.mmds_data = Some(data);
            info!("MMDS data stored");
            Ok(())
        }

        // MMDS patch - merge into existing data
        (Method::PATCH, "/mmds") => {
            let patch: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap_or_default();
            let mut s = state.0.lock().await;
            if let Some(ref mut existing) = s.mmds_data {
                json_merge_patch(existing, &patch);
            } else {
                s.mmds_data = Some(patch);
            }
            debug!("MMDS data patched");
            Ok(())
        }

        // Vsock config - store uds_path
        (Method::PUT, "/vsock") => {
            #[derive(serde::Deserialize)]
            struct VsockConfig {
                #[allow(dead_code)]
                guest_cid: u32,
                uds_path: String,
            }
            if let Ok(config) = serde_json::from_slice::<VsockConfig>(&body_bytes) {
                let mut s = state.0.lock().await;
                s.vsock_uds_path = Some(config.uds_path.clone());
                info!(uds_path = %config.uds_path, "vsock config stored");
            }
            Ok(())
        }

        // InstanceStart - launch the container
        (Method::PUT, "/actions") => {
            #[derive(serde::Deserialize)]
            struct Action {
                action_type: String,
            }
            if let Ok(action) = serde_json::from_slice::<Action>(&body_bytes) {
                if action.action_type == "InstanceStart" {
                    info!("InstanceStart received, launching container");
                    let mut s = state.0.lock().await;
                    if s.started {
                        warn!("InstanceStart called twice, ignoring");
                        return Ok(response_204());
                    }
                    s.started = true;

                    let mmds_data = s.mmds_data.clone();
                    let vsock_uds_path = s.vsock_uds_path.clone();
                    let shutdown = s.shutdown.clone();
                    drop(s); // Release lock before spawning

                    tokio::spawn(async move {
                        if let Err(e) = crate::container::launch_container(
                            mmds_data,
                            vsock_uds_path,
                            shutdown.clone(),
                        )
                        .await
                        {
                            tracing::error!("container launch failed: {}", e);
                            // Trigger shutdown so fcvm doesn't hang
                            if let Some(s) = shutdown {
                                s.notify_one();
                            }
                        }
                    });
                }
            }
            Ok(())
        }

        // Snapshot endpoints - no-op stubs
        (Method::PUT, "/snapshot/create") => Ok(()),
        (Method::PUT, "/snapshot/load") => Ok(()),

        // VM state (Pause/Resume) - no-op
        (Method::PATCH, "/vm") => Ok(()),

        // Drive patch - no-op
        (Method::PATCH, p) if p.starts_with("/drives/") => Ok(()),

        // Balloon stats - no-op
        (Method::PATCH, "/balloon/statistics") => Ok(()),

        // Unknown endpoint
        _ => {
            warn!(path = %path, "unknown API endpoint");
            Ok(())
        }
    };

    match result {
        Ok(()) => Ok(response_204()),
        Err(e) => {
            let msg = format!("{{\"fault_message\": \"{}\"}}", e);
            Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from(msg))
                .unwrap())
        }
    }
}

fn response_204() -> Response<Body> {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap()
}

/// RFC 7396 JSON Merge Patch
fn json_merge_patch(target: &mut serde_json::Value, patch: &serde_json::Value) {
    if let serde_json::Value::Object(patch_obj) = patch {
        if !target.is_object() {
            *target = serde_json::Value::Object(serde_json::Map::new());
        }
        let target_obj = target.as_object_mut().unwrap();
        for (key, value) in patch_obj {
            if value.is_null() {
                target_obj.remove(key);
            } else if value.is_object() {
                let entry = target_obj
                    .entry(key.clone())
                    .or_insert(serde_json::Value::Null);
                json_merge_patch(entry, value);
            } else {
                target_obj.insert(key.clone(), value.clone());
            }
        }
    } else {
        *target = patch.clone();
    }
}
