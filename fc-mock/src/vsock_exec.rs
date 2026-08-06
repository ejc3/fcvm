//! Vsock exec emulation via Unix sockets.
//!
//! Emulates Firecracker's vsock CONNECT protocol for exec commands.
//! fcvm connects to vsock.sock, sends "CONNECT 4998\n", and we respond
//! with "OK 4998\n" then handle the exec request.

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tracing::{debug, info, warn};

const CONTAINER_NAME: &str = "fcvm-container";

/// Start the exec listener on the vsock Unix socket.
///
/// Returns a JoinHandle that can be aborted on shutdown.
pub async fn start_exec_listener(vsock_uds_path: &str) -> Result<tokio::task::JoinHandle<()>> {
    // Remove stale socket
    let _ = std::fs::remove_file(vsock_uds_path);

    let listener = UnixListener::bind(vsock_uds_path)
        .with_context(|| format!("binding vsock exec listener to {}", vsock_uds_path))?;

    info!(socket = %vsock_uds_path, "exec listener started");

    let handle = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream).await {
                            debug!("exec connection error: {}", e);
                        }
                    });
                }
                Err(e) => {
                    warn!("exec accept error: {}", e);
                }
            }
        }
    });

    Ok(handle)
}

/// Handle a single CONNECT + exec session.
async fn handle_connection(stream: tokio::net::UnixStream) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // Read CONNECT command
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .context("reading CONNECT command")?;

    let line = line.trim();
    if !line.starts_with("CONNECT ") {
        write_half
            .write_all(b"ERROR: expected CONNECT command\n")
            .await?;
        return Ok(());
    }

    let port: u32 = line
        .strip_prefix("CONNECT ")
        .unwrap()
        .trim()
        .parse()
        .unwrap_or(0);

    // Respond with OK
    let ok_msg = format!("OK {}\n", port);
    write_half.write_all(ok_msg.as_bytes()).await?;

    debug!(port, "CONNECT accepted");

    // Read the exec request (JSON line)
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .await
        .context("reading exec request")?;

    let request: ExecRequest =
        serde_json::from_str(request_line.trim()).context("parsing exec request JSON")?;

    debug!(
        command = ?request.command,
        in_container = request.in_container,
        tty = request.tty,
        interactive = request.interactive,
        "exec request received"
    );

    // Pre-ACK validation, matching fc-agent: an empty command is rejected with
    // an Error line BEFORE the ACK, so the client sees a deterministic
    // rejection instead of a mid-handshake close.
    if request.command.is_empty() {
        let resp = ExecResponse::Error("Empty command".to_string());
        let json = serde_json::to_string(&resp)?;
        write_half.write_all(json.as_bytes()).await?;
        write_half.write_all(b"\n").await?;
        return Ok(());
    }

    // Three-phase handshake (matches fc-agent, see exec_proto::HANDSHAKE_ACK):
    // ACK that the request was fully consumed, then execute only after the
    // client's GO line. Bounded so a stalled client can't leak this task.
    write_half
        .write_all(format!("{}\n", exec_proto::HANDSHAKE_ACK).as_bytes())
        .await
        .context("writing ACK")?;
    let mut go_line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        reader.read_line(&mut go_line),
    )
    .await
    .context("timed out waiting for GO; closing without executing")?
    .context("reading GO line")?;
    // Exact-byte compare (one trailing newline stripped), matching fc-agent.
    if go_line.strip_suffix('\n').unwrap_or(&go_line) != exec_proto::HANDSHAKE_GO {
        anyhow::bail!(
            "expected GO, got {:?}; closing without executing",
            go_line.trim_end_matches('\n')
        );
    }
    debug!("exec handshake complete (ACK/GO)");

    // Execute the command
    if request.tty || request.interactive {
        // TTY mode - not supported yet, send error
        let resp = ExecResponse::Error("TTY mode not supported in fc-mock".to_string());
        let json = serde_json::to_string(&resp)?;
        write_half.write_all(json.as_bytes()).await?;
        write_half.write_all(b"\n").await?;

        let exit = ExecResponse::Exit(1);
        let json = serde_json::to_string(&exit)?;
        write_half.write_all(json.as_bytes()).await?;
        write_half.write_all(b"\n").await?;
        return Ok(());
    }

    // Build the actual command to run
    let (program, args) = if request.in_container {
        // Run inside the container via podman exec
        // Include rootless storage args so podman can find the container
        let storage_args = crate::container::rootless_storage_args();
        let mut exec_args = storage_args;
        exec_args.push("exec".to_string());
        exec_args.push(CONTAINER_NAME.to_string());
        exec_args.extend(request.command.clone());
        ("podman".to_string(), exec_args)
    } else {
        // Run directly on the host (VM-level exec)
        if request.command.is_empty() {
            let resp = ExecResponse::Error("empty command".to_string());
            let json = serde_json::to_string(&resp)?;
            write_half.write_all(json.as_bytes()).await?;
            write_half.write_all(b"\n").await?;
            return Ok(());
        }
        // If the command is podman, prepend rootless storage args
        let program = request.command[0].clone();
        let args = if program == "podman" {
            let mut storage_args = crate::container::rootless_storage_args();
            storage_args.extend(request.command[1..].to_vec());
            storage_args
        } else {
            request.command[1..].to_vec()
        };
        (program, args)
    };

    debug!(program = %program, args = ?args, "executing command");

    // Spawn the command
    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    // In user namespaces, podman needs HOME and XDG_RUNTIME_DIR overrides
    // to avoid permission errors reading config/auth from the original user's dirs.
    if program == "podman" {
        crate::container::apply_user_ns_env(&mut cmd);
    }

    let result = cmd.output().await;

    match result {
        Ok(output) => {
            // Send stdout
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.is_empty() {
                let resp = ExecResponse::Stdout(stdout.to_string());
                let json = serde_json::to_string(&resp)?;
                write_half.write_all(json.as_bytes()).await?;
                write_half.write_all(b"\n").await?;
            }

            // Send stderr
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.is_empty() {
                let resp = ExecResponse::Stderr(stderr.to_string());
                let json = serde_json::to_string(&resp)?;
                write_half.write_all(json.as_bytes()).await?;
                write_half.write_all(b"\n").await?;
            }

            // Send exit code
            let exit_code = output.status.code().unwrap_or(1);
            let resp = ExecResponse::Exit(exit_code);
            let json = serde_json::to_string(&resp)?;
            write_half.write_all(json.as_bytes()).await?;
            write_half.write_all(b"\n").await?;

            debug!(exit_code, "exec completed");
        }
        Err(e) => {
            let resp = ExecResponse::Error(format!("failed to execute: {}", e));
            let json = serde_json::to_string(&resp)?;
            write_half.write_all(json.as_bytes()).await?;
            write_half.write_all(b"\n").await?;

            let exit = ExecResponse::Exit(127);
            let json = serde_json::to_string(&exit)?;
            write_half.write_all(json.as_bytes()).await?;
            write_half.write_all(b"\n").await?;
        }
    }

    write_half.flush().await?;
    Ok(())
}

/// Request sent by fcvm's exec command (matches src/commands/exec.rs)
#[derive(serde::Deserialize, Debug)]
struct ExecRequest {
    command: Vec<String>,
    in_container: bool,
    #[serde(default)]
    interactive: bool,
    #[serde(default)]
    tty: bool,
}

/// Response sent back to fcvm (matches src/commands/exec.rs)
#[derive(serde::Serialize)]
#[serde(tag = "type", content = "data")]
enum ExecResponse {
    #[serde(rename = "stdout")]
    Stdout(String),
    #[serde(rename = "stderr")]
    Stderr(String),
    #[serde(rename = "exit")]
    Exit(i32),
    #[serde(rename = "error")]
    Error(String),
}
