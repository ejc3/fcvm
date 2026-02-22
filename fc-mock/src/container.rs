//! Container lifecycle management.
//!
//! Launches a podman container based on the MMDS container-plan,
//! connects to the vsock status/output sockets, and manages the
//! container lifecycle.

use anyhow::{bail, Context, Result};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

/// Container name used by fcvm's health monitor (hardcoded in health.rs)
const CONTAINER_NAME: &str = "fcvm-container";

/// Launch a container from the MMDS plan and manage its lifecycle.
///
/// 1. Parse MMDS container-plan
/// 2. Start vsock exec listener
/// 3. Launch container via podman
/// 4. Connect to status socket → send "ready"
/// 5. Connect to output socket → forward stdout/stderr
/// 6. Wait for container exit → send "exit:{code}"
/// 7. Signal shutdown so fc-mock exits
pub async fn launch_container(
    mmds_data: Option<serde_json::Value>,
    vsock_uds_path: Option<String>,
    shutdown: Option<Arc<Notify>>,
) -> Result<()> {
    let mmds = mmds_data.context("no MMDS data stored (PUT /mmds not called)")?;
    let vsock_path = vsock_uds_path.context("no vsock config (PUT /vsock not called)")?;

    // Parse container plan from MMDS
    let plan = &mmds["latest"]["container-plan"];
    if plan.is_null() {
        bail!("MMDS data has no latest.container-plan");
    }

    let image = plan["image"]
        .as_str()
        .context("container-plan missing 'image'")?;

    let cmd: Option<Vec<String>> = plan["cmd"].as_array().map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    });

    let env: Vec<(String, String)> = plan["env"]
        .as_object()
        .map(|obj| {
            obj.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default();

    let privileged = plan["privileged"].as_bool().unwrap_or(false);
    let tty = plan["tty"].as_bool().unwrap_or(false);
    let user = plan["user"].as_str().map(String::from);

    info!(
        image = %image,
        cmd = ?cmd,
        env_count = env.len(),
        privileged,
        tty,
        "parsed container plan"
    );

    // Start vsock exec listener BEFORE anything else so health checks can connect
    let exec_handle = crate::vsock_exec::start_exec_listener(&vsock_path).await?;

    // Remove any existing container with the same name
    cleanup_container().await;

    // Build podman command
    let mut podman_args = vec![
        "run".to_string(),
        "--name".to_string(),
        CONTAINER_NAME.to_string(),
        "--rm".to_string(),
    ];

    for (key, val) in &env {
        podman_args.push("-e".to_string());
        podman_args.push(format!("{}={}", key, val));
    }

    if privileged {
        podman_args.push("--privileged".to_string());
    }

    if tty {
        podman_args.push("-t".to_string());
    }

    if let Some(ref u) = user {
        podman_args.push("--user".to_string());
        podman_args.push(u.clone());
    }

    podman_args.push(image.to_string());

    if let Some(ref cmd_args) = cmd {
        podman_args.extend(cmd_args.clone());
    }

    info!(args = ?podman_args, "launching container");

    // Spawn podman with piped stdout/stderr
    let mut child = tokio::process::Command::new("podman")
        .args(&podman_args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawning podman")?;

    let stdout = child.stdout.take().context("no stdout")?;
    let stderr = child.stderr.take().context("no stderr")?;

    // Connect to status socket (created by fcvm's run_status_listener)
    let status_socket_path = format!("{}_4999", vsock_path);
    let output_socket_path = format!("{}_4997", vsock_path);

    // Retry connecting to status socket (it might not be ready yet)
    let status_stream = connect_with_retry(&status_socket_path, 50, 100)
        .await
        .context("connecting to status socket")?;
    info!("connected to status socket");

    // Send "ready" immediately (container process is running)
    let mut status_writer = status_stream;
    status_writer
        .write_all(b"ready\n")
        .await
        .context("sending ready")?;
    status_writer.flush().await?;
    info!("sent ready to status socket");

    // Connect to output socket and forward container output
    let output_stream = connect_with_retry(&output_socket_path, 50, 100).await;
    let output_writer = match output_stream {
        Ok(s) => {
            info!("connected to output socket");
            Some(Arc::new(tokio::sync::Mutex::new(s)))
        }
        Err(e) => {
            warn!(
                "could not connect to output socket: {} (output won't be forwarded)",
                e
            );
            None
        }
    };

    // Spawn stdout forwarder
    let out_writer = output_writer.clone();
    let stdout_task = tokio::spawn(async move {
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(ref w) = out_writer {
                let msg = format!("stdout:{}\n", line);
                let mut w = w.lock().await;
                let _ = w.write_all(msg.as_bytes()).await;
            }
        }
    });

    // Spawn stderr forwarder
    let err_writer = output_writer.clone();
    let stderr_task = tokio::spawn(async move {
        let reader = BufReader::new(stderr);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(ref w) = err_writer {
                let msg = format!("stderr:{}\n", line);
                let mut w = w.lock().await;
                let _ = w.write_all(msg.as_bytes()).await;
            }
        }
    });

    // Wait for container to exit
    let exit_status = child.wait().await.context("waiting for container")?;
    let exit_code = exit_status.code().unwrap_or(1);

    info!(exit_code, "container exited");

    // Send exit code to status socket
    let exit_msg = format!("exit:{}\n", exit_code);
    let _ = status_writer.write_all(exit_msg.as_bytes()).await;
    let _ = status_writer.flush().await;

    // Wait for output tasks to finish
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let _ = stdout_task.await;
        let _ = stderr_task.await;
    })
    .await;

    // Abort the exec listener
    exec_handle.abort();

    // Signal shutdown so fc-mock exits (like real Firecracker after PSCI shutdown)
    if let Some(shutdown) = shutdown {
        shutdown.notify_one();
    }

    Ok(())
}

/// Clean up any existing container with our name.
pub async fn cleanup_container() {
    debug!("cleaning up container {}", CONTAINER_NAME);
    let output = tokio::process::Command::new("podman")
        .args(["rm", "-f", "-t", "0", CONTAINER_NAME])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .await;
    if let Ok(o) = output {
        if o.status.success() {
            debug!("removed existing container {}", CONTAINER_NAME);
        }
    }
}

/// Connect to a Unix socket with retries.
async fn connect_with_retry(
    path: &str,
    max_attempts: u32,
    delay_ms: u64,
) -> Result<tokio::net::UnixStream> {
    for attempt in 1..=max_attempts {
        match tokio::net::UnixStream::connect(path).await {
            Ok(stream) => return Ok(stream),
            Err(e) if attempt < max_attempts => {
                if attempt == 1 || attempt % 10 == 0 {
                    debug!(attempt, path, "socket not ready, retrying: {}", e);
                }
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
            Err(e) => bail!(
                "failed to connect to {} after {} attempts: {}",
                path,
                max_attempts,
                e
            ),
        }
    }
    unreachable!()
}
