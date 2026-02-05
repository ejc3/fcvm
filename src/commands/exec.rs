//! Execute commands in a running VM or its container
//!
//! Uses Firecracker's vsock to connect from host to guest.
//! The guest (fc-agent) listens on vsock port 4998.
//! The host connects via the vsock.sock Unix socket using the CONNECT protocol.
//!
//! TTY mode uses a length-prefixed binary protocol (see exec_proto.rs) to cleanly
//! separate control messages from raw terminal data. Non-TTY mode continues to use
//! JSON line protocol.

use crate::cli::ExecArgs;
use crate::paths;
use crate::state::StateManager;
use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;
use tracing::{debug, info};

/// Vsock port for exec commands (fc-agent listens on this)
pub const EXEC_VSOCK_PORT: u32 = 4998;

/// Maximum number of connection attempts to the exec server
const MAX_EXEC_CONNECT_ATTEMPTS: u32 = 30;

/// Initial retry delay when connecting to exec server (doubles each attempt)
const INITIAL_RETRY_DELAY_MS: u64 = 100;

/// Connect to the exec server via vsock with retry logic.
///
/// The guest VM takes several seconds to boot and start fc-agent with the exec server.
/// This function retries the connection with exponential backoff to handle this startup delay.
///
/// Returns a connected UnixStream on success.
fn connect_to_exec_server_with_retry(vsock_socket: &Path) -> Result<UnixStream> {
    let mut attempt = 0;
    let mut delay_ms = INITIAL_RETRY_DELAY_MS;

    loop {
        attempt += 1;

        // Connect to the vsock Unix socket
        let mut stream = match UnixStream::connect(vsock_socket) {
            Ok(s) => s,
            Err(e) if attempt < MAX_EXEC_CONNECT_ATTEMPTS => {
                debug!(attempt, delay_ms, "vsock socket not ready, retrying");
                std::thread::sleep(Duration::from_millis(delay_ms));
                delay_ms = std::cmp::min(delay_ms * 2, 2000); // Cap at 2 seconds
                continue;
            }
            Err(e) => {
                bail!(
                    "Failed to connect to vsock socket at {} after {} attempts: {}.\n\
                     Make sure the VM is running.",
                    vsock_socket.display(),
                    attempt,
                    e
                );
            }
        };

        // Set timeouts for the CONNECT handshake
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;

        // Send CONNECT command to Firecracker's vsock proxy
        let connect_cmd = format!("CONNECT {}\n", EXEC_VSOCK_PORT);
        if let Err(e) = stream.write_all(connect_cmd.as_bytes()) {
            if attempt < MAX_EXEC_CONNECT_ATTEMPTS {
                debug!(attempt, delay_ms, error = %e, "failed to send CONNECT, retrying");
                std::thread::sleep(Duration::from_millis(delay_ms));
                delay_ms = std::cmp::min(delay_ms * 2, 2000);
                continue;
            }
            bail!(
                "Failed to send CONNECT command after {} attempts: {}",
                attempt,
                e
            );
        }

        // Read the response - should be "OK <port>\n" on success
        let mut response = [0u8; 32];
        let n = match stream.read(&mut response) {
            Ok(n) => n,
            Err(e) => {
                if attempt < MAX_EXEC_CONNECT_ATTEMPTS {
                    debug!(attempt, delay_ms, error = %e, "failed to read CONNECT response, retrying");
                    std::thread::sleep(Duration::from_millis(delay_ms));
                    delay_ms = std::cmp::min(delay_ms * 2, 2000);
                    continue;
                }
                bail!(
                    "Failed to read CONNECT response after {} attempts: {}",
                    attempt,
                    e
                );
            }
        };

        let response_str = String::from_utf8_lossy(&response[..n]);

        if !response_str.starts_with("OK ") {
            if attempt < MAX_EXEC_CONNECT_ATTEMPTS {
                // Exec server not ready yet, retry
                if attempt == 1 || attempt % 10 == 0 {
                    // Log occasionally to avoid spam
                    debug!(
                        attempt,
                        delay_ms,
                        response = %response_str.trim(),
                        "exec server not ready (fc-agent still starting), retrying"
                    );
                }
                std::thread::sleep(Duration::from_millis(delay_ms));
                delay_ms = std::cmp::min(delay_ms * 2, 2000);
                continue;
            }

            bail!(
                "Failed to connect to guest exec server after {} attempts: {}. \
                 Make sure fc-agent is running with exec server enabled.",
                attempt,
                response_str.trim()
            );
        }

        // Success!
        if attempt > 1 {
            debug!(attempt, "successfully connected to exec server");
        }
        return Ok(stream);
    }
}

/// Execute a command in a VM or its container (programmatic API)
///
/// This is a simpler API for programmatic use (e.g., from snapshot run --exec).
/// For CLI use, see `cmd_exec`.
///
/// Returns the command's exit code.
pub async fn run_exec_in_vm(
    vsock_socket: &Path,
    command: &[String],
    in_container: bool,
) -> Result<i32> {
    debug!(
        socket = %vsock_socket.display(),
        command = ?command,
        in_container,
        "executing command in VM"
    );

    // Connect to the exec server with retry logic
    let mut stream = connect_to_exec_server_with_retry(vsock_socket)?;

    // Set timeouts for non-interactive mode
    stream.set_read_timeout(Some(Duration::from_secs(300)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;

    debug!("connected to guest exec server");

    // Build the exec request (non-interactive, no TTY)
    let request = ExecRequest {
        command: command.to_vec(),
        in_container,
        interactive: false,
        tty: false,
    };

    // Send request as JSON followed by newline
    let request_json = serde_json::to_string(&request)?;
    writeln!(stream, "{}", request_json)?;
    stream.flush()?;

    // Run in line mode and capture exit code
    run_line_mode_with_exit_code(stream)
}

pub async fn cmd_exec(args: ExecArgs) -> Result<()> {
    // Find the VM by name or PID
    let state_manager = StateManager::new(paths::state_dir());
    state_manager.init().await?;

    let vm_state = if let Some(pid) = args.pid {
        // Look up by PID
        state_manager
            .load_state_by_pid(pid)
            .await
            .with_context(|| format!("No VM found with PID {}", pid))?
    } else if let Some(name) = &args.name {
        // Look up by name
        state_manager
            .load_state_by_name(name)
            .await
            .with_context(|| format!("No VM found with name '{}'", name))?
    } else {
        bail!("Either --pid or name is required");
    };

    // Get the vsock socket path for this VM
    let vm_dir = paths::vm_runtime_dir(&vm_state.vm_id);
    let vsock_socket = vm_dir.join("vsock.sock");

    // Suppress logs when in TTY or quiet mode (they mix with command output)
    let quiet = args.quiet || args.tty;
    if !quiet {
        info!(
            vm_id = %vm_state.vm_id,
            socket = %vsock_socket.display(),
            port = EXEC_VSOCK_PORT,
            "connecting to VM exec server via vsock"
        );
    }

    // Connect to the exec server with retry logic
    let mut stream = connect_to_exec_server_with_retry(&vsock_socket)?;

    // Set timeouts (longer for TTY mode)
    let timeout = if args.tty { 3600 } else { 300 };
    stream.set_read_timeout(Some(Duration::from_secs(timeout)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;

    if !quiet {
        info!("connected to guest exec server");
    }

    // Check if stdin is a TTY
    let stdin_is_tty = unsafe { libc::isatty(libc::STDIN_FILENO) == 1 };

    // Auto-detect: if running a shell and stdin is a TTY, enable -it
    let is_shell = args
        .command
        .first()
        .map(|cmd| {
            let basename = std::path::Path::new(cmd)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(cmd);
            matches!(
                basename,
                "bash" | "sh" | "zsh" | "fish" | "ash" | "dash" | "ksh" | "csh" | "tcsh"
            )
        })
        .unwrap_or(false);

    // Determine effective flags:
    // - If explicitly set, use those
    // - If running a shell with TTY stdin, auto-enable -it
    let (interactive, tty) = if args.interactive || args.tty {
        // User explicitly specified flags
        (args.interactive, args.tty)
    } else if is_shell && stdin_is_tty {
        // Auto-detect: shell + TTY stdin = interactive mode
        if !quiet {
            info!("auto-detected shell with TTY, enabling -it");
        }
        (true, true)
    } else {
        (false, false)
    };

    // Build the exec request
    // Default is to exec in container, --vm flag runs in VM instead
    let request = ExecRequest {
        command: args.command.clone(),
        in_container: !args.vm,
        interactive,
        tty,
    };

    // Send request as JSON followed by newline
    let request_json = serde_json::to_string(&request)?;
    writeln!(stream, "{}", request_json)?;
    stream.flush()?;

    if !quiet {
        info!(
            command = ?args.command,
            in_container = !args.vm,
            interactive,
            tty,
            "sent exec request"
        );
    }

    // Use binary framing for any mode needing TTY or stdin forwarding
    // JSON line mode only for plain non-interactive commands
    if tty || interactive {
        let exit_code = super::tty::run_tty_session_connected(stream, tty, interactive)?;
        if exit_code != 0 {
            std::process::exit(exit_code);
        }
        Ok(())
    } else {
        run_line_mode(stream)
    }
}

/// Run in line-buffered mode (non-TTY), returns exit code
fn run_line_mode_with_exit_code(stream: UnixStream) -> Result<i32> {
    let reader = BufReader::new(stream);
    let mut exit_code = 0i32;

    for line in reader.lines() {
        let line = line.context("reading from exec socket")?;

        // Parse the line as JSON
        if let Ok(response) = serde_json::from_str::<ExecResponse>(&line) {
            match response {
                ExecResponse::Stdout(data) => {
                    print!("{}", data);
                }
                ExecResponse::Stderr(data) => {
                    eprint!("{}", data);
                }
                ExecResponse::Exit(code) => {
                    exit_code = code;
                    break;
                }
                ExecResponse::Error(msg) => {
                    eprintln!("Error: {}", msg);
                    exit_code = 1;
                    break;
                }
            }
        }
    }

    Ok(exit_code)
}

/// Run in line-buffered mode (non-TTY)
fn run_line_mode(stream: UnixStream) -> Result<()> {
    let exit_code = run_line_mode_with_exit_code(stream)?;

    // Exit with the command's exit code
    if exit_code != 0 {
        std::process::exit(exit_code);
    }

    Ok(())
}

/// Request sent to fc-agent exec server
#[derive(serde::Serialize, serde::Deserialize)]
pub struct ExecRequest {
    pub command: Vec<String>,
    pub in_container: bool,
    /// Keep STDIN open (-i)
    #[serde(default)]
    pub interactive: bool,
    /// Allocate a pseudo-TTY (-t)
    #[serde(default)]
    pub tty: bool,
}

/// Response from fc-agent exec server
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum ExecResponse {
    #[serde(rename = "stdout")]
    Stdout(String),
    #[serde(rename = "stderr")]
    Stderr(String),
    #[serde(rename = "exit")]
    Exit(i32),
    #[serde(rename = "error")]
    Error(String),
}
