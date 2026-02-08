//! HTTP/WebSocket API server for ComputeSDK integration.
//!
//! `fcvm serve` acts as both the ComputeSDK gateway (sandbox CRUD) and sandbox daemon
//! (exec, files, terminal). The standard `computesdk` TypeScript package connects via
//! gateway mode — no custom TypeScript SDK needed.
//!
//! Gateway endpoints: `/v1/sandboxes/*`
//! Sandbox daemon endpoints: `/s/{id}/*`

use crate::cli::ServeArgs;
use crate::commands::exec::run_exec_in_vm_captured;
use crate::commands::podman::{start_vm, VmHandle};
use crate::state::{HealthStatus, StateManager};
use anyhow::{Context, Result};
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_http::cors::CorsLayer;
use tracing::{error, info, warn};

// ============================================================================
// Server state
// ============================================================================

struct SandboxEntry {
    handle: VmHandle,
    runtime: Option<String>,
    created_at: DateTime<Utc>,
    timeout_ms: Option<u64>,
}

struct AppState {
    sandboxes: RwLock<HashMap<String, SandboxEntry>>,
    port: u16,
}

type SharedState = Arc<AppState>;

// ============================================================================
// Request/Response types
// ============================================================================

#[derive(Deserialize)]
struct CreateSandboxRequest {
    runtime: Option<String>,
    image: Option<String>,
    name: Option<String>,
    cpu: Option<u8>,
    #[serde(alias = "memoryMib")]
    memory_mib: Option<u32>,
    timeout_ms: Option<u64>,
    env: Option<HashMap<String, String>>,
    labels: Option<HashMap<String, String>>,
}

#[derive(Serialize)]
struct GatewayResponse<T: Serialize> {
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl<T: Serialize> GatewayResponse<T> {
    fn ok(data: T) -> Json<Self> {
        Json(Self {
            success: true,
            data: Some(data),
            error: None,
        })
    }
}

fn gateway_error(status: StatusCode, msg: impl Into<String>) -> Response {
    let body = serde_json::json!({
        "success": false,
        "error": msg.into(),
    });
    (status, Json(body)).into_response()
}

#[derive(Serialize)]
struct SandboxInfo {
    #[serde(rename = "sandboxId")]
    sandbox_id: String,
    url: String,
    token: String,
    provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Deserialize)]
struct RunCodeRequest {
    code: String,
    language: Option<String>,
}

#[derive(Serialize)]
struct RunCodeResponse {
    data: RunCodeData,
}

#[derive(Serialize)]
struct RunCodeData {
    output: String,
    exit_code: i32,
    language: String,
}

#[derive(Deserialize)]
struct RunCommandRequest {
    command: String,
    cwd: Option<String>,
    env: Option<HashMap<String, String>>,
}

#[derive(Serialize)]
struct RunCommandResponse {
    message: String,
    data: RunCommandData,
}

#[derive(Serialize)]
struct RunCommandData {
    command: String,
    stdout: String,
    stderr: String,
    exit_code: i32,
    duration_ms: u64,
}

#[derive(Deserialize)]
struct FilesQuery {
    path: Option<String>,
}

#[derive(Serialize)]
struct FileInfo {
    name: String,
    #[serde(rename = "type")]
    file_type: String,
    is_dir: bool,
    size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    modified_at: Option<String>,
}

#[derive(Deserialize)]
struct CreateFileRequest {
    path: String,
    content: String,
    #[serde(default)]
    encoding: Option<String>,
}

#[derive(Deserialize)]
struct CreateTerminalRequest {
    #[allow(dead_code)]
    shell: Option<String>,
}

// ============================================================================
// Runtime mapping
// ============================================================================

fn runtime_to_image(runtime: &str) -> &str {
    match runtime {
        "python" | "python3" => "python:3.12-slim",
        "node" | "nodejs" => "node:22-slim",
        "ruby" => "ruby:3.3-slim",
        "go" | "golang" => "golang:1.23-alpine",
        other => other,
    }
}

fn runtime_to_language(runtime: &str) -> &str {
    match runtime {
        "python" | "python3" => "python",
        "node" | "nodejs" => "javascript",
        "ruby" => "ruby",
        "go" | "golang" => "go",
        other => other,
    }
}

fn language_to_run_command(language: &str, code: &str) -> Vec<String> {
    match language {
        "python" | "python3" => vec!["python3".into(), "-c".into(), code.into()],
        "javascript" | "node" | "nodejs" => vec!["node".into(), "-e".into(), code.into()],
        "ruby" => vec!["ruby".into(), "-e".into(), code.into()],
        _ => vec!["sh".into(), "-c".into(), code.into()],
    }
}

// ============================================================================
// Helper: get sandbox + vsock path
// ============================================================================

async fn get_vsock_path(
    state: &SharedState,
    id: &str,
) -> std::result::Result<std::path::PathBuf, Response> {
    let sandboxes = state.sandboxes.read().await;
    let entry = sandboxes
        .get(id)
        .ok_or_else(|| gateway_error(StatusCode::NOT_FOUND, format!("Sandbox {} not found", id)))?;
    Ok(entry.handle.vsock_socket_path())
}

fn sandbox_url(port: u16, id: &str) -> String {
    format!("http://localhost:{}/s/{}", port, id)
}

// ============================================================================
// Gateway handlers
// ============================================================================

async fn create_sandbox(
    State(state): State<SharedState>,
    Json(req): Json<CreateSandboxRequest>,
) -> Response {
    let image = match (&req.runtime, &req.image) {
        (Some(runtime), _) => runtime_to_image(runtime).to_string(),
        (_, Some(image)) => image.clone(),
        (None, None) => "python:3.12-slim".to_string(), // default to python
    };

    let runtime = req.runtime.clone().or_else(|| Some("python".to_string()));
    let name = req
        .name
        .unwrap_or_else(|| format!("csdk-{}", &uuid::Uuid::new_v4().to_string()[..8]));

    // Build RunArgs for start_vm
    let args = crate::cli::RunArgs {
        name: name.clone(),
        cpu: req.cpu.unwrap_or(2),
        mem: req.memory_mib.unwrap_or(2048),
        rootfs_size: "10G".to_string(),
        map: vec![],
        disk: vec![],
        disk_dir: vec![],
        nfs: vec![],
        env: req
            .env
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect(),
        label: req
            .labels
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect(),
        cmd: Some("sh -c 'while :; do sleep 3600; done'".to_string()),
        publish: vec![],
        balloon: None,
        network: crate::cli::NetworkMode::Rootless,
        health_check: None,
        privileged: false,
        interactive: false,
        tty: false,
        strace_agent: false,
        setup: true,
        kernel: None,
        kernel_profile: None,
        vsock_dir: None,
        no_snapshot: true,
        image,
        command_args: vec![],
    };

    info!(name = %name, "Creating sandbox");

    let handle = match start_vm(args).await {
        Ok(h) => h,
        Err(e) => {
            error!(error = %e, "Failed to start VM");
            return gateway_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to create sandbox: {}", e),
            );
        }
    };

    let sandbox_id = handle.vm_id.clone();

    // Wait for healthy status (max 120s)
    let healthy = wait_for_healthy(&handle, std::time::Duration::from_secs(120)).await;
    if !healthy {
        warn!(sandbox_id = %sandbox_id, "Sandbox did not become healthy within timeout");
        // Don't fail — the SDK will poll /health itself
    }

    let url = sandbox_url(state.port, &sandbox_id);

    let entry = SandboxEntry {
        handle,
        runtime,
        created_at: Utc::now(),
        timeout_ms: req.timeout_ms,
    };

    state
        .sandboxes
        .write()
        .await
        .insert(sandbox_id.clone(), entry);

    info!(sandbox_id = %sandbox_id, "Sandbox created");

    let info = SandboxInfo {
        sandbox_id,
        url,
        token: "local".to_string(),
        provider: "fcvm".to_string(),
        status: Some("ready".to_string()),
        name: Some(name),
    };

    GatewayResponse::ok(info).into_response()
}

async fn wait_for_healthy(handle: &VmHandle, timeout: std::time::Duration) -> bool {
    let start = std::time::Instant::now();
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        interval.tick().await;
        if start.elapsed() > timeout {
            return false;
        }
        match handle.state().await {
            Ok(s) if s.health_status == HealthStatus::Healthy => return true,
            Ok(s) if s.health_status == HealthStatus::Stopped => return false,
            _ => {}
        }
    }
}

async fn list_sandboxes(State(state): State<SharedState>) -> Response {
    let sandboxes = state.sandboxes.read().await;
    let list: Vec<SandboxInfo> = sandboxes
        .iter()
        .map(|(id, entry)| SandboxInfo {
            sandbox_id: id.clone(),
            url: sandbox_url(state.port, id),
            token: "local".to_string(),
            provider: "fcvm".to_string(),
            status: None,
            name: Some(entry.handle.name.clone()),
        })
        .collect();
    GatewayResponse::ok(list).into_response()
}

async fn get_sandbox(State(state): State<SharedState>, Path(id): Path<String>) -> Response {
    let sandboxes = state.sandboxes.read().await;
    let entry = match sandboxes.get(&id) {
        Some(e) => e,
        None => return gateway_error(StatusCode::NOT_FOUND, format!("Sandbox {} not found", id)),
    };

    let status = match entry.handle.state().await {
        Ok(s) => format!("{:?}", s.health_status).to_lowercase(),
        Err(_) => "unknown".to_string(),
    };

    let info = SandboxInfo {
        sandbox_id: id.clone(),
        url: sandbox_url(state.port, &id),
        token: "local".to_string(),
        provider: "fcvm".to_string(),
        status: Some(status),
        name: Some(entry.handle.name.clone()),
    };

    GatewayResponse::ok(info).into_response()
}

async fn destroy_sandbox(State(state): State<SharedState>, Path(id): Path<String>) -> Response {
    let mut entry = match state.sandboxes.write().await.remove(&id) {
        Some(e) => e,
        None => return gateway_error(StatusCode::NOT_FOUND, format!("Sandbox {} not found", id)),
    };

    info!(sandbox_id = %id, "Destroying sandbox");

    if let Err(e) = entry.handle.stop().await {
        warn!(sandbox_id = %id, error = %e, "Error stopping sandbox");
    }

    let body = serde_json::json!({ "success": true });
    Json(body).into_response()
}

// ============================================================================
// Sandbox daemon handlers
// ============================================================================

async fn health(State(state): State<SharedState>, Path(id): Path<String>) -> Response {
    let sandboxes = state.sandboxes.read().await;
    if !sandboxes.contains_key(&id) {
        return gateway_error(StatusCode::NOT_FOUND, format!("Sandbox {} not found", id));
    }

    let body = serde_json::json!({
        "status": "ok",
        "timestamp": Utc::now().to_rfc3339(),
    });
    Json(body).into_response()
}

async fn ready(State(state): State<SharedState>, Path(id): Path<String>) -> Response {
    let sandboxes = state.sandboxes.read().await;
    let entry = match sandboxes.get(&id) {
        Some(e) => e,
        None => return gateway_error(StatusCode::NOT_FOUND, format!("Sandbox {} not found", id)),
    };

    let healthy = match entry.handle.state().await {
        Ok(s) => s.health_status == HealthStatus::Healthy,
        Err(_) => false,
    };

    let body = serde_json::json!({
        "ready": healthy,
        "healthy": healthy,
        "servers": [],
        "overlays": [],
    });
    Json(body).into_response()
}

async fn run_code(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(req): Json<RunCodeRequest>,
) -> Response {
    let (vsock_path, language) = {
        let sandboxes = state.sandboxes.read().await;
        let entry = match sandboxes.get(&id) {
            Some(e) => e,
            None => {
                return gateway_error(StatusCode::NOT_FOUND, format!("Sandbox {} not found", id))
            }
        };
        let lang = req.language.clone().unwrap_or_else(|| {
            entry
                .runtime
                .as_deref()
                .map(runtime_to_language)
                .unwrap_or("python")
                .to_string()
        });
        (entry.handle.vsock_socket_path(), lang)
    };

    let cmd = language_to_run_command(&language, &req.code);

    let output = match run_exec_in_vm_captured(&vsock_path, &cmd, true).await {
        Ok(o) => o,
        Err(e) => {
            return gateway_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Exec failed: {}", e),
            );
        }
    };

    let combined = if output.stderr.is_empty() {
        output.stdout.clone()
    } else if output.stdout.is_empty() {
        output.stderr.clone()
    } else {
        format!("{}{}", output.stdout, output.stderr)
    };

    let resp = RunCodeResponse {
        data: RunCodeData {
            output: combined,
            exit_code: output.exit_code,
            language,
        },
    };
    Json(resp).into_response()
}

async fn run_command(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(req): Json<RunCommandRequest>,
) -> Response {
    let vsock_path = match get_vsock_path(&state, &id).await {
        Ok(p) => p,
        Err(e) => return e,
    };

    let shell_cmd = build_shell_command(&req.command, req.cwd.as_deref(), req.env.as_ref());
    let start = std::time::Instant::now();

    let output = match run_exec_in_vm_captured(&vsock_path, &shell_cmd, true).await {
        Ok(o) => o,
        Err(e) => {
            return gateway_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Exec failed: {}", e),
            );
        }
    };

    let duration_ms = start.elapsed().as_millis() as u64;

    let resp = RunCommandResponse {
        message: "Command executed".to_string(),
        data: RunCommandData {
            command: req.command,
            stdout: output.stdout,
            stderr: output.stderr,
            exit_code: output.exit_code,
            duration_ms,
        },
    };
    Json(resp).into_response()
}

fn build_shell_command(
    command: &str,
    cwd: Option<&str>,
    env: Option<&HashMap<String, String>>,
) -> Vec<String> {
    let mut parts = String::new();
    if let Some(dir) = cwd {
        parts.push_str(&format!("cd '{}' && ", dir.replace('\'', "'\\''")));
    }
    if let Some(vars) = env {
        for (k, v) in vars {
            parts.push_str(&format!("export {}='{}'; ", k, v.replace('\'', "'\\''")));
        }
    }
    parts.push_str(command);
    vec!["sh".into(), "-c".into(), parts]
}

// ============================================================================
// File operation handlers
// ============================================================================

async fn list_files(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Query(query): Query<FilesQuery>,
) -> Response {
    let vsock_path = match get_vsock_path(&state, &id).await {
        Ok(p) => p,
        Err(e) => return e,
    };

    let path = query.path.as_deref().unwrap_or("/");

    // Use stat to get file info in a parseable format
    // Use printf to produce real tab characters (stat --format doesn't interpret \t)
    let cmd = vec![
        "sh".into(),
        "-c".into(),
        format!(
            "for f in '{}'/*; do [ -e \"$f\" ] && stat --printf='%n\\t%F\\t%s\\t%Y\\n' \"$f\" 2>/dev/null; done",
            path.replace('\'', "'\\''")
        ),
    ];

    let output = match run_exec_in_vm_captured(&vsock_path, &cmd, true).await {
        Ok(o) => o,
        Err(e) => {
            return gateway_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to list files: {}", e),
            );
        }
    };

    let files: Vec<FileInfo> = output
        .stdout
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() >= 4 {
                let name = std::path::Path::new(parts[0])
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| parts[0].to_string());
                let is_dir = parts[1].contains("directory");
                let file_type = if is_dir { "directory" } else { "file" }.to_string();
                let size = parts[2].parse().unwrap_or(0);
                let modified_at = parts[3]
                    .parse::<i64>()
                    .ok()
                    .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
                    .map(|dt| dt.to_rfc3339());
                Some(FileInfo {
                    name,
                    file_type,
                    is_dir,
                    size,
                    modified_at,
                })
            } else {
                None
            }
        })
        .collect();

    let body = serde_json::json!({
        "message": "Files listed",
        "data": {
            "files": files,
            "path": path,
        }
    });
    Json(body).into_response()
}

async fn create_file(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(req): Json<CreateFileRequest>,
) -> Response {
    let vsock_path = match get_vsock_path(&state, &id).await {
        Ok(p) => p,
        Err(e) => return e,
    };

    // Create parent directory and write file
    let dir = std::path::Path::new(&req.path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "/".to_string());

    let is_base64 = req.encoding.as_deref() == Some("base64");

    let write_cmd = if is_base64 {
        format!(
            "mkdir -p '{}' && echo '{}' | base64 -d > '{}'",
            dir.replace('\'', "'\\''"),
            req.content.replace('\'', "'\\''"),
            req.path.replace('\'', "'\\''"),
        )
    } else {
        // Use a randomized heredoc delimiter to prevent content injection
        let delimiter = format!("FCVM_EOF_{}", uuid::Uuid::new_v4().simple());
        format!(
            "mkdir -p '{}' && cat > '{}' << '{}'\n{}\n{}",
            dir.replace('\'', "'\\''"),
            req.path.replace('\'', "'\\''"),
            delimiter,
            req.content,
            delimiter,
        )
    };

    let cmd = vec!["sh".into(), "-c".into(), write_cmd];

    if let Err(e) = run_exec_in_vm_captured(&vsock_path, &cmd, true).await {
        return gateway_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to write file: {}", e),
        );
    }

    let body = serde_json::json!({
        "message": "File created",
        "data": {
            "file": {
                "name": std::path::Path::new(&req.path).file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
                "type": "file",
                "path": req.path,
            }
        }
    });
    (StatusCode::CREATED, Json(body)).into_response()
}

async fn read_file(
    State(state): State<SharedState>,
    Path((id, file_path)): Path<(String, String)>,
) -> Response {
    let vsock_path = match get_vsock_path(&state, &id).await {
        Ok(p) => p,
        Err(e) => return e,
    };

    let decoded_path = percent_encoding::percent_decode_str(&file_path).decode_utf8_lossy();
    let abs_path = if decoded_path.starts_with('/') {
        decoded_path.to_string()
    } else {
        format!("/{}", decoded_path)
    };

    let cmd = vec![
        "sh".into(),
        "-c".into(),
        format!("cat '{}'", abs_path.replace('\'', "'\\''")),
    ];

    let output = match run_exec_in_vm_captured(&vsock_path, &cmd, true).await {
        Ok(o) => o,
        Err(e) => {
            return gateway_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to read file: {}", e),
            );
        }
    };

    if output.exit_code != 0 {
        return gateway_error(
            StatusCode::NOT_FOUND,
            format!("File not found: {}", abs_path),
        );
    }

    let body = serde_json::json!({
        "message": "File read",
        "data": {
            "content": output.stdout,
            "path": abs_path,
            "encoding": "utf-8",
        }
    });
    Json(body).into_response()
}

async fn file_exists(
    State(state): State<SharedState>,
    Path((id, file_path)): Path<(String, String)>,
) -> Response {
    let vsock_path = match get_vsock_path(&state, &id).await {
        Ok(p) => p,
        Err(e) => return e,
    };

    let decoded_path = percent_encoding::percent_decode_str(&file_path).decode_utf8_lossy();
    let abs_path = if decoded_path.starts_with('/') {
        decoded_path.to_string()
    } else {
        format!("/{}", decoded_path)
    };

    let cmd = vec![
        "sh".into(),
        "-c".into(),
        format!("test -e '{}'", abs_path.replace('\'', "'\\''")),
    ];

    let output = match run_exec_in_vm_captured(&vsock_path, &cmd, true).await {
        Ok(o) => o,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    if output.exit_code == 0 {
        StatusCode::OK.into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

async fn delete_file(
    State(state): State<SharedState>,
    Path((id, file_path)): Path<(String, String)>,
) -> Response {
    let vsock_path = match get_vsock_path(&state, &id).await {
        Ok(p) => p,
        Err(e) => return e,
    };

    let decoded_path = percent_encoding::percent_decode_str(&file_path).decode_utf8_lossy();
    let abs_path = if decoded_path.starts_with('/') {
        decoded_path.to_string()
    } else {
        format!("/{}", decoded_path)
    };

    let cmd = vec![
        "sh".into(),
        "-c".into(),
        format!("rm -rf '{}'", abs_path.replace('\'', "'\\''")),
    ];

    if let Err(e) = run_exec_in_vm_captured(&vsock_path, &cmd, true).await {
        return gateway_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to delete file: {}", e),
        );
    }

    StatusCode::NO_CONTENT.into_response()
}

// ============================================================================
// Terminal handlers
// ============================================================================

async fn create_terminal(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(_req): Json<CreateTerminalRequest>,
) -> Response {
    let sandboxes = state.sandboxes.read().await;
    if !sandboxes.contains_key(&id) {
        return gateway_error(StatusCode::NOT_FOUND, format!("Sandbox {} not found", id));
    }

    let terminal_id = uuid::Uuid::new_v4().to_string();

    let body = serde_json::json!({
        "message": "Terminal created",
        "data": {
            "id": terminal_id,
            "pty": true,
            "status": "running",
        }
    });
    (StatusCode::CREATED, Json(body)).into_response()
}

async fn ws_terminal(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    let vsock_path = {
        let sandboxes = state.sandboxes.read().await;
        match sandboxes.get(&id) {
            Some(entry) => entry.handle.vsock_socket_path(),
            None => {
                return gateway_error(StatusCode::NOT_FOUND, format!("Sandbox {} not found", id));
            }
        }
    };

    ws.on_upgrade(move |socket| ws_terminal_handler(socket, vsock_path))
}

async fn ws_terminal_handler(mut ws: axum::extract::ws::WebSocket, vsock_path: std::path::PathBuf) {
    use axum::extract::ws::Message as WsMessage;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Connect to exec server
    let stream = match crate::commands::exec::connect_to_exec_server_async(&vsock_path).await {
        Ok(s) => s,
        Err(e) => {
            error!(error = %e, "Failed to connect to exec server for terminal");
            let _ = ws
                .send(WsMessage::Close(Some(axum::extract::ws::CloseFrame {
                    code: 1011,
                    reason: format!("Failed to connect: {}", e).into(),
                })))
                .await;
            return;
        }
    };

    // Send exec request for interactive bash
    let exec_req = crate::commands::exec::ExecRequest {
        command: vec!["/bin/bash".into()],
        in_container: true,
        interactive: true,
        tty: true,
    };
    let (mut vsock_read, mut vsock_write) = stream.into_split();

    // Write the exec request as JSON line
    let req_json = match serde_json::to_string(&exec_req) {
        Ok(j) => j,
        Err(e) => {
            error!(error = %e, "Failed to serialize exec request");
            return;
        }
    };
    if let Err(e) = vsock_write
        .write_all(format!("{}\n", req_json).as_bytes())
        .await
    {
        error!(error = %e, "Failed to send exec request");
        return;
    }

    // Bridge WS ↔ vsock
    // Use a channel for vsock→WS since we can't hold &mut to both ws and vsock in select!
    let (vsock_tx, mut vsock_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);

    // Spawn task to read from vsock and send to channel
    tokio::spawn(async move {
        let mut buf = vec![0u8; 8192];
        loop {
            match vsock_read.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if vsock_tx.send(buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Main loop: read from WS and from vsock channel
    loop {
        tokio::select! {
            Some(data) = vsock_rx.recv() => {
                if ws.send(WsMessage::Binary(data.into())).await.is_err() {
                    break;
                }
            }
            result = ws.recv() => {
                match result {
                    Some(Ok(WsMessage::Binary(data))) => {
                        if vsock_write.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(WsMessage::Text(text))) => {
                        // Text frames: encode as exec-proto stdin message
                        let mut buf = Vec::new();
                        if exec_proto::write_stdin(&mut buf, text.as_bytes()).is_err() {
                            break;
                        }
                        if vsock_write.write_all(&buf).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }
}

// ============================================================================
// Cleanup
// ============================================================================

async fn cleanup_orphans() {
    let state_dir = crate::paths::state_dir();
    let mgr = StateManager::new(state_dir);

    if let Ok(vms) = mgr.list_vms().await {
        for vm in vms {
            if let Some(pid) = vm.pid {
                // Check if process is still alive
                if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok() {
                    warn!(pid = pid, vm_id = %vm.vm_id, "Killing orphaned VM");
                    let _ = nix::sys::signal::kill(
                        nix::unistd::Pid::from_raw(pid as i32),
                        nix::sys::signal::Signal::SIGTERM,
                    );
                }
            }
            let _ = mgr.delete_state(&vm.vm_id).await;
        }
    }
}

async fn reaper_task(state: SharedState) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
    loop {
        interval.tick().await;
        let now = Utc::now();
        let mut expired = Vec::new();

        {
            let sandboxes = state.sandboxes.read().await;
            for (id, entry) in sandboxes.iter() {
                if let Some(timeout_ms) = entry.timeout_ms {
                    let elapsed = (now - entry.created_at).num_milliseconds();
                    if elapsed > timeout_ms as i64 {
                        expired.push(id.clone());
                    }
                }
            }
        }

        for id in expired {
            if let Some(mut entry) = state.sandboxes.write().await.remove(&id) {
                info!(sandbox_id = %id, "Sandbox expired, destroying");
                let _ = entry.handle.stop().await;
            }
        }
    }
}

// ============================================================================
// Main entry point
// ============================================================================

pub async fn cmd_serve(args: ServeArgs) -> Result<()> {
    info!(port = args.port, "Starting fcvm serve");

    // Clean up orphaned VMs from previous runs
    cleanup_orphans().await;

    let state = Arc::new(AppState {
        sandboxes: RwLock::new(HashMap::new()),
        port: args.port,
    });

    // Spawn idle timeout reaper
    tokio::spawn(reaper_task(state.clone()));

    let app = Router::new()
        // Gateway endpoints
        .route("/v1/sandboxes", post(create_sandbox).get(list_sandboxes))
        .route(
            "/v1/sandboxes/{id}",
            get(get_sandbox).delete(destroy_sandbox),
        )
        // Sandbox daemon endpoints
        .route("/s/{id}/health", get(health))
        .route("/s/{id}/ready", get(ready))
        .route("/s/{id}/run/code", post(run_code))
        .route("/s/{id}/run/command", post(run_command))
        .route("/s/{id}/files", get(list_files).post(create_file))
        .route(
            "/s/{id}/files/{*path}",
            get(read_file).head(file_exists).delete(delete_file),
        )
        .route("/s/{id}/terminals", post(create_terminal))
        .route("/s/{id}", get(ws_terminal))
        .layer(CorsLayer::permissive())
        .with_state(state.clone());

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], args.port));
    info!(addr = %addr, "Listening");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context("Failed to bind to port")?;

    // Graceful shutdown on SIGTERM/SIGINT
    let shutdown_state = state.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let ctrl_c = tokio::signal::ctrl_c();
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("install SIGTERM handler");

            tokio::select! {
                _ = ctrl_c => info!("Received SIGINT"),
                _ = sigterm.recv() => info!("Received SIGTERM"),
            }

            info!("Shutting down — stopping all sandboxes");
            let mut sandboxes = shutdown_state.sandboxes.write().await;
            for (id, entry) in sandboxes.drain() {
                info!(sandbox_id = %id, "Stopping sandbox");
                let mut handle = entry.handle;
                let _ = handle.stop().await;
            }
        })
        .await
        .context("Server error")?;

    info!("Server stopped");
    Ok(())
}
