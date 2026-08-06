//! Execute commands in a running VM or its container
//!
//! Uses Firecracker's vsock to connect from host to guest.
//! The guest (fc-agent) listens on vsock port 4998.
//! The host connects via the vsock.sock Unix socket using the CONNECT protocol.
//!
//! Every exec session starts with a three-phase handshake (request → ACK → GO,
//! see `exec_proto::HANDSHAKE_ACK`): a VM snapshot pause (startup snapshot or
//! `fcvm snapshot create --pid`) resets the vsock transport and silently orphans
//! in-flight connections with no error on either side. A request that never
//! receives ACK provably never executed, so it is resent on a fresh connection
//! (bounded retries); once GO is sent, resending is forbidden — execution may
//! have started — and any connection death is a loud error instead of a hang.
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

/// Per-attempt bounded wait for the agent's ACK line. A live agent ACKs in
/// sub-millisecond time; only an orphaned/paused connection stalls this long.
const ACK_TIMEOUT: Duration = Duration::from_secs(3);

/// Attempts to send an exec request that was never acknowledged. Resends are
/// safe (fc-agent never executes before consuming GO). 5 × 3s of ACK waiting
/// (plus reconnect time) spans realistic snapshot pause durations (~15s).
const MAX_ACK_ATTEMPTS: u32 = 5;

/// Post-handshake read timeout — commands like phps cookie gen can take
/// >10 min of silence. A safety net against permanent hangs (not None/infinite).
const EXEC_READ_TIMEOUT: Duration = Duration::from_secs(3600);

/// Post-handshake / handshake write timeout.
const EXEC_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

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

/// Outcome of waiting for the agent's ACK line after sending a request.
#[derive(Debug)]
enum AckOutcome {
    /// ACK consumed — the agent has the full request and awaits GO.
    Acked,
    /// No ACK arrived (timeout, EOF, or read error). The request provably
    /// never reached execution, so resending on a fresh connection is safe.
    NotAcked(String),
    /// The agent rejected the request before ACK (invalid JSON, empty
    /// command). Deterministic — resending cannot help.
    Rejected(String),
}

/// Wait for the agent's ACK line, bounded by the stream's read timeout.
///
/// Reads byte-by-byte so no bytes past the ACK newline are ever consumed —
/// everything after ACK belongs to the post-GO exec protocol (JSON lines or
/// TTY frames), which the mode loops read from the raw stream.
fn read_ack_line(stream: &mut UnixStream) -> AckOutcome {
    // An ACK line is 4 bytes; a pre-ACK Error line carries a message. Anything
    // bigger is a protocol violation.
    const MAX_ACK_LINE_LENGTH: usize = 65_536;
    let mut line: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => return AckOutcome::NotAcked("connection closed before ACK".to_string()),
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                if line.len() >= MAX_ACK_LINE_LENGTH {
                    return AckOutcome::Rejected(format!(
                        "protocol violation: pre-ACK line exceeds {} bytes",
                        MAX_ACK_LINE_LENGTH
                    ));
                }
                line.push(byte[0]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return AckOutcome::NotAcked(format!(
                    "timed out after {:?} waiting for ACK",
                    ACK_TIMEOUT
                ));
            }
            Err(e) => return AckOutcome::NotAcked(format!("read error waiting for ACK: {}", e)),
        }
    }

    if line == exec_proto::HANDSHAKE_ACK.as_bytes() {
        return AckOutcome::Acked;
    }

    // Not ACK: the agent rejects invalid requests with an Error response line
    // before ever ACKing. Surface its message rather than a raw protocol dump.
    if let Ok(ExecResponse::Error(msg)) = serde_json::from_slice::<ExecResponse>(&line) {
        return AckOutcome::Rejected(msg);
    }
    AckOutcome::Rejected(format!(
        "protocol violation: expected ACK, got {:?}",
        String::from_utf8_lossy(&line)
    ))
}

/// Connect, send the exec request, and complete the three-phase handshake
/// (request → ACK → GO). Returns a stream on which execution has just been
/// authorized: the next bytes from the agent are exec responses.
///
/// A request that never gets ACKed (its connection was orphaned by a snapshot
/// pause's vsock reset — silent, no error on either side) provably never
/// executed, so it is resent on a fresh connection, bounded by
/// `MAX_ACK_ATTEMPTS`. After GO is sent, no resend ever happens — fc-agent may
/// have started executing — so a subsequent connection death surfaces as a
/// loud error from the mode loops instead (see `ExecConnectionClosed`).
pub fn connect_and_start_exec(vsock_socket: &Path, request: &ExecRequest) -> Result<UnixStream> {
    let request_json = serde_json::to_string(request)?;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let mut stream = connect_to_exec_server_with_retry(vsock_socket)?;
        stream.set_read_timeout(Some(ACK_TIMEOUT))?;
        stream.set_write_timeout(Some(EXEC_WRITE_TIMEOUT))?;

        // Phase 1: send the request line.
        if let Err(e) = writeln!(stream, "{}", request_json).and_then(|()| stream.flush()) {
            if attempt < MAX_ACK_ATTEMPTS {
                debug!(attempt, error = %e, "exec request send failed; reconnecting to resend");
                continue;
            }
            bail!(
                "failed to send exec request after {} attempts: {}",
                attempt,
                e
            );
        }

        // Phase 2: wait (bounded) for ACK.
        match read_ack_line(&mut stream) {
            AckOutcome::Acked => {
                debug!(attempt, "exec handshake: ACK received, sending GO");
                // Phase 3: authorize execution. From here on the request must
                // NEVER be resent: fc-agent executes as soon as it consumes GO,
                // and a local write result cannot prove whether GO was delivered.
                if let Err(e) =
                    writeln!(stream, "{}", exec_proto::HANDSHAKE_GO).and_then(|()| stream.flush())
                {
                    bail!(
                        "exec connection died while sending GO (after the request was \
                         acknowledged): {}. Not resending — the command could run twice. \
                         If a VM snapshot was being created (startup snapshot or \
                         `fcvm snapshot create`), the pause resets vsock and kills \
                         in-flight execs; retry the exec.",
                        e
                    );
                }
                debug!("exec handshake: GO sent, command starting");
                stream.set_read_timeout(Some(EXEC_READ_TIMEOUT))?;
                stream.set_write_timeout(Some(EXEC_WRITE_TIMEOUT))?;
                return Ok(stream);
            }
            AckOutcome::NotAcked(reason) => {
                if attempt < MAX_ACK_ATTEMPTS {
                    // Safe by construction: fc-agent never executes before GO,
                    // and this request never even got ACKed.
                    debug!(
                        attempt,
                        reason,
                        "exec request never acknowledged (connection likely orphaned by a \
                         snapshot pause); reconnecting to resend"
                    );
                    continue;
                }
                bail!(
                    "exec request was never acknowledged after {} attempts (last: {}). \
                     A VM snapshot pause (startup snapshot or `fcvm snapshot create`) \
                     resets vsock and orphans in-flight connections; resends are safe \
                     but retries are exhausted — is the VM healthy?",
                    attempt,
                    reason
                );
            }
            AckOutcome::Rejected(msg) => {
                bail!("fc-agent rejected the exec request: {}", msg);
            }
        }
    }
}

/// Execute a command in a VM or its container (programmatic API)
///
/// This is a simpler API for programmatic use (e.g., from snapshot run --exec).
/// For CLI use, see `cmd_exec`.
///
/// Returns the command's exit code.
///
/// The blocking vsock I/O is offloaded to a blocking thread pool so this
/// function is safe to call from async contexts without starving the runtime.
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

    let vsock_socket = vsock_socket.to_path_buf();
    let command = command.to_vec();

    tokio::task::spawn_blocking(move || {
        // Build the exec request (non-interactive, no TTY)
        let request = ExecRequest {
            command,
            in_container,
            interactive: false,
            tty: false,
        };

        // Connect, send the request, and complete the ACK/GO handshake
        let stream = connect_and_start_exec(&vsock_socket, &request)?;
        debug!("exec handshake complete");

        // Run in line mode and capture exit code
        run_line_mode_with_exit_code(stream)
    })
    .await
    .context("exec task panicked")?
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

    // Connect, send the request, and complete the ACK/GO handshake
    let stream = connect_and_start_exec(&vsock_socket, &request)?;

    if !quiet {
        info!(
            command = ?args.command,
            in_container = !args.vm,
            interactive,
            tty,
            "exec request acknowledged, command starting"
        );
    }

    // Use binary framing for any mode needing TTY or stdin forwarding
    // JSON line mode only for plain non-interactive commands
    let result = if tty || interactive {
        match super::tty::run_tty_session_connected(stream, tty, interactive) {
            Ok(exit_code) => {
                if exit_code != 0 {
                    std::process::exit(exit_code);
                }
                Ok(())
            }
            Err(e) => Err(e),
        }
    } else {
        run_line_mode(stream)
    };

    // Downgrade the benign "stream closed before exit" race to debug ONLY for quiet
    // (subprocess) callers — e.g. the health monitor's `--quiet` inspect during
    // teardown. Still exit non-zero. A user-invoked exec (not quiet) propagates the
    // error so `main` logs a visible ERROR rather than failing silently.
    if let Err(e) = &result {
        if is_benign_quiet_exec_close(quiet, e) {
            debug!("{:#}", e);
            std::process::exit(1);
        }
    }
    result
}

/// Read JSON-line exec responses until an Exit (or Error) message arrives.
///
/// Stdout/stderr payloads are passed to the provided callbacks. Returns the
/// command's exit code (Error messages map to exit code 1).
///
/// If the stream ends before an Exit message is received (fc-agent crash, VM
/// reboot, vsock reset), the command's outcome is unknown, so this returns an
/// error rather than reporting success.
fn read_exec_responses<R: BufRead>(
    reader: R,
    mut on_stdout: impl FnMut(&str),
    mut on_stderr: impl FnMut(&str),
) -> Result<i32> {
    for line in reader.lines() {
        let line = line.context("reading from exec socket")?;

        // Parse the line as JSON; skip lines that aren't valid responses
        let Ok(response) = serde_json::from_str::<ExecResponse>(&line) else {
            continue;
        };

        match response {
            ExecResponse::Stdout(data) => on_stdout(&data),
            ExecResponse::Stderr(data) => on_stderr(&data),
            ExecResponse::Exit(code) => return Ok(code),
            ExecResponse::Error(msg) => {
                on_stderr(&format!("Error: {}\n", msg));
                return Ok(1);
            }
        }
    }

    Err(ExecConnectionClosed.into())
}

/// The exec stream ended before an Exit message arrived.
///
/// This is a real failure — the command's outcome is unknown, so the caller
/// still exits non-zero. But it is also the expected, benign terminal
/// condition when an exec races VM or container shutdown (for example the
/// health monitor's `--quiet podman inspect` healthcheck during teardown), so
/// `cmd_exec` logs it at debug rather than alarming at ERROR *for quiet
/// (subprocess) callers only* — see `is_benign_quiet_exec_close`.
#[derive(Debug)]
pub struct ExecConnectionClosed;

impl std::fmt::Display for ExecConnectionClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "exec connection closed before an exit status was received \
             (the VM or agent may have exited, or a VM snapshot pause reset \
             vsock and killed the exec mid-flight)"
        )
    }
}

impl std::error::Error for ExecConnectionClosed {}

/// Whether an exec error is the benign "stream closed before exit" race AND the caller
/// opted into quiet mode (the health monitor's `--quiet` inspect subprocess).
///
/// Only quiet/subprocess callers get the log downgrade: a user-invoked `fcvm exec`
/// whose VM/container dies or vsock resets before an Exit frame must still surface a
/// visible ERROR (not exit 1 silently), so the downgrade is scoped to `quiet` here
/// rather than applied globally in `main`.
fn is_benign_quiet_exec_close(quiet: bool, err: &anyhow::Error) -> bool {
    quiet && err.downcast_ref::<ExecConnectionClosed>().is_some()
}

/// Run in line-buffered mode (non-TTY), returns exit code
fn run_line_mode_with_exit_code(stream: UnixStream) -> Result<i32> {
    let reader = BufReader::new(stream);
    read_exec_responses(
        reader,
        |data| print!("{}", data),
        |data| eprint!("{}", data),
    )
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

/// Captured output from an exec command (for programmatic/server use).
pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Execute a command in a VM and capture stdout/stderr into strings.
///
/// Unlike `run_exec_in_vm` which prints to stdout/stderr directly,
/// this returns the output as strings for programmatic use.
///
/// The blocking vsock I/O is offloaded to a blocking thread pool so this
/// function is safe to call from async contexts without starving the runtime.
pub async fn run_exec_in_vm_captured(
    vsock_socket: &Path,
    command: &[String],
    in_container: bool,
) -> Result<ExecOutput> {
    debug!(
        socket = %vsock_socket.display(),
        command = ?command,
        in_container,
        "executing command in VM (captured)"
    );

    let vsock_socket = vsock_socket.to_path_buf();
    let command = command.to_vec();

    tokio::task::spawn_blocking(move || {
        let request = ExecRequest {
            command,
            in_container,
            interactive: false,
            tty: false,
        };

        // Connect, send the request, and complete the ACK/GO handshake
        let stream = connect_and_start_exec(&vsock_socket, &request)?;

        // Read lines and capture into strings instead of printing
        let reader = BufReader::new(stream);
        let mut stdout = String::new();
        let mut stderr = String::new();
        let exit_code = read_exec_responses(
            reader,
            |data| stdout.push_str(data),
            |data| stderr.push_str(data),
        )?;

        Ok(ExecOutput {
            stdout,
            stderr,
            exit_code,
        })
    })
    .await
    .context("exec task panicked")?
}

/// Connect, complete the exec handshake for `request`, and return a tokio
/// async UnixStream on which execution has just been authorized (the next
/// bytes from the agent are exec responses / TTY frames).
///
/// Useful for building WebSocket↔vsock bridges (terminal sessions).
///
/// The blocking connect/handshake logic is offloaded to a blocking thread pool
/// so this function is safe to call from async contexts.
pub async fn start_exec_session_async(
    vsock_socket: &Path,
    request: ExecRequest,
) -> Result<tokio::net::UnixStream> {
    let vsock_socket = vsock_socket.to_path_buf();
    let std_stream =
        tokio::task::spawn_blocking(move || connect_and_start_exec(&vsock_socket, &request))
            .await
            .context("connect task panicked")??;
    std_stream.set_nonblocking(true)?;
    tokio::net::UnixStream::from_std(std_stream).context("converting to tokio UnixStream")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response_line(response: &ExecResponse) -> String {
        format!("{}\n", serde_json::to_string(response).unwrap())
    }

    #[test]
    fn read_exec_responses_returns_exit_code() {
        let mut input = String::new();
        input.push_str(&response_line(&ExecResponse::Stdout("hello\n".into())));
        input.push_str(&response_line(&ExecResponse::Stderr("warning\n".into())));
        input.push_str(&response_line(&ExecResponse::Exit(7)));

        let mut stdout = String::new();
        let mut stderr = String::new();
        let code = read_exec_responses(
            input.as_bytes(),
            |d| stdout.push_str(d),
            |d| stderr.push_str(d),
        )
        .unwrap();

        assert_eq!(code, 7);
        assert_eq!(stdout, "hello\n");
        assert_eq!(stderr, "warning\n");
    }

    #[test]
    fn read_exec_responses_error_message_yields_exit_one() {
        let input = response_line(&ExecResponse::Error("spawn failed".into()));

        let mut stderr = String::new();
        let code = read_exec_responses(input.as_bytes(), |_| {}, |d| stderr.push_str(d)).unwrap();

        assert_eq!(code, 1);
        assert_eq!(stderr, "Error: spawn failed\n");
    }

    #[test]
    fn read_exec_responses_eof_without_exit_is_an_error() {
        // Connection dropped after some output but before the Exit message:
        // the command's outcome is unknown, so this must not report success.
        let input = response_line(&ExecResponse::Stdout("partial output\n".into()));

        let err = read_exec_responses(input.as_bytes(), |_| {}, |_| {}).unwrap_err();
        assert!(
            err.to_string().contains("before an exit status"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn read_exec_responses_truncated_final_line_is_an_error() {
        // A connection drop mid-message leaves a partial JSON line and no Exit.
        let mut input = response_line(&ExecResponse::Stdout("ok\n".into()));
        input.push_str("{\"type\":\"exit\",\"da");

        let err = read_exec_responses(input.as_bytes(), |_| {}, |_| {}).unwrap_err();
        assert!(
            err.to_string().contains("before an exit status"),
            "unexpected error: {err}"
        );
    }

    /// ACK arrives → Acked, and NOTHING past the ACK newline is consumed —
    /// bytes after ACK belong to the post-GO exec protocol.
    #[test]
    fn read_ack_line_acked_consumes_nothing_past_newline() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        server
            .write_all(
                format!(
                    "{}\n{{\"type\":\"exit\",\"data\":0}}\n",
                    exec_proto::HANDSHAKE_ACK
                )
                .as_bytes(),
            )
            .unwrap();

        assert!(matches!(read_ack_line(&mut client), AckOutcome::Acked));

        // The exec response following ACK must still be readable in full.
        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line, "{\"type\":\"exit\",\"data\":0}\n");
    }

    /// Peer closes without ACK → NotAcked (safe to resend).
    #[test]
    fn read_ack_line_eof_is_not_acked() {
        let (mut client, server) = UnixStream::pair().unwrap();
        drop(server);
        match read_ack_line(&mut client) {
            AckOutcome::NotAcked(reason) => assert!(reason.contains("closed"), "{reason}"),
            other => panic!("expected NotAcked, got {:?}", other),
        }
    }

    /// Silence (the snapshot-pause orphan shape) → bounded NotAcked timeout,
    /// not a hang.
    #[test]
    fn read_ack_line_timeout_is_not_acked() {
        let (mut client, _server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let start = std::time::Instant::now();
        match read_ack_line(&mut client) {
            AckOutcome::NotAcked(reason) => assert!(reason.contains("timed out"), "{reason}"),
            other => panic!("expected NotAcked, got {:?}", other),
        }
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    /// A pre-ACK Error response (invalid/empty request) → Rejected with the
    /// agent's message; deterministic, so the client must not resend.
    #[test]
    fn read_ack_line_error_response_is_rejected() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let error_line = response_line(&ExecResponse::Error("Empty command".into()));
        server.write_all(error_line.as_bytes()).unwrap();

        match read_ack_line(&mut client) {
            AckOutcome::Rejected(msg) => assert_eq!(msg, "Empty command"),
            other => panic!("expected Rejected, got {:?}", other),
        }
    }

    /// #607 regression (Codex P2): the log downgrade for the benign "stream closed
    /// before exit" race must be scoped to quiet (subprocess) callers. A user-invoked
    /// exec (not quiet) must NOT be downgraded — it has to surface a visible error
    /// instead of exiting 1 silently. This fails if the downgrade is applied globally.
    #[test]
    fn benign_close_downgrade_is_scoped_to_quiet() {
        let closed: anyhow::Error = ExecConnectionClosed.into();
        // Quiet subprocess (health monitor): downgrade the benign close.
        assert!(is_benign_quiet_exec_close(true, &closed));
        // User-invoked exec (not quiet): must stay visible (the regression Codex flagged).
        assert!(!is_benign_quiet_exec_close(false, &closed));

        // Only the benign close qualifies — other errors are never downgraded.
        let other = anyhow::anyhow!("some other failure");
        assert!(!is_benign_quiet_exec_close(true, &other));
        assert!(!is_benign_quiet_exec_close(false, &other));
    }
}
