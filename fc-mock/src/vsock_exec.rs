//! Vsock exec emulation via Unix sockets.
//!
//! Emulates Firecracker's vsock CONNECT protocol for exec commands.
//! fcvm connects to vsock.sock, sends "CONNECT 4998\n", and we respond
//! with "OK 4998\n" then handle the exec request.

use anyhow::{Context, Result};
use exec_proto::{ExecRequest, Message};
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
        let line = exec_proto::rejection_line("Empty command");
        write_half.write_all(line.as_bytes()).await?;
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
    // fc-mock runs a command to completion and reports its output. Refuse what
    // it cannot honour, so a test never passes on a flag that was ignored.
    let unsupported = [
        (request.tty || request.interactive, "-i and -t"),
        (!request.env.is_empty(), "-e and --env-file"),
        (request.workdir.is_some(), "-w"),
        (request.user.is_some(), "-u"),
        (request.privileged, "--privileged"),
        (request.detach, "-d"),
    ];
    if let Some((_, flags)) = unsupported.iter().find(|(used, _)| *used) {
        let error = Message::Error(format!("{flags} are not supported in fc-mock"));
        write_half.write_all(&error.encode()).await?;
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

    // The frames and exit codes fc-agent uses for a plain exec: byte-exact
    // streams, 128+signal for a signal death, 127 when the command is missing,
    // 126 otherwise. Output goes out in 64 KiB frames, under the frame cap.
    use std::os::unix::process::ExitStatusExt;
    let (stdout, stderr, exit_code) = match result {
        Ok(output) => {
            let exit_code = output
                .status
                .code()
                .unwrap_or_else(|| 128 + output.status.signal().unwrap_or(0));
            (output.stdout, output.stderr, exit_code)
        }
        Err(e) => {
            let exit_code = if e.kind() == std::io::ErrorKind::NotFound {
                127
            } else {
                126
            };
            let text = format!("Error: cannot run {:?}: {}\n", program, e);
            (Vec::new(), text.into_bytes(), exit_code)
        }
    };
    debug!(exit_code, "exec completed");
    for chunk in stdout.chunks(exec_proto::IO_CHUNK) {
        write_half
            .write_all(&Message::Data(chunk.to_vec()).encode())
            .await?;
    }
    for chunk in stderr.chunks(exec_proto::IO_CHUNK) {
        write_half
            .write_all(&Message::Stderr(chunk.to_vec()).encode())
            .await?;
    }
    write_half
        .write_all(&Message::Exit(exit_code).encode())
        .await?;

    write_half.flush().await?;
    Ok(())
}
