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
//! Post-GO, the same snapshot pause can still orphan the session (ACK received
//! but GO swallowed, or mid-response): the host bumps the VM's persisted
//! `vsock_epoch` after every snapshot pause/save, and every blocked response
//! read polls it via [`SnapshotOrphanGuard`] — an epoch change aborts the
//! session loudly instead of hanging, while honest silence (a command quiet
//! for hours) keeps waiting indefinitely.
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
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{debug, info};

/// Vsock port for exec commands (fc-agent listens on this)
pub const EXEC_VSOCK_PORT: u32 = 4998;

/// Maximum number of connection attempts to the exec server. With the 5ms
/// initial delay, 1.5x growth, and 2s cap this spans ~54s of total waiting —
/// the same boot-tolerance window as the previous 30 × (100ms, 2x) ladder.
const MAX_EXEC_CONNECT_ATTEMPTS: u32 = 41;

/// Initial retry delay when connecting to exec server (grows 1.5x per attempt,
/// capped at 2s). 5ms, not 100ms: a freshly restored clone's exec server
/// re-registers within single-digit milliseconds, and the adversarial review of
/// the clone-latency benchmarks showed the old 100ms quantum was
/// indistinguishable from real guest latency (UFFD arms sat in the empty
/// 100-139ms band purely from this ladder).
const INITIAL_RETRY_DELAY_MS: u64 = 5;

/// Multiply the retry delay by 3/2 (integer) each attempt, capped at 2s.
fn next_retry_delay(delay_ms: u64) -> u64 {
    std::cmp::min(delay_ms + delay_ms / 2 + 1, 2000)
}

/// Per-attempt bounded wait for the agent's ACK line. A live agent ACKs in
/// sub-millisecond time; only an orphaned/paused connection stalls this long.
const ACK_TIMEOUT: Duration = Duration::from_secs(3);

/// Attempts to send an exec request that was never acknowledged. Resends are
/// safe (fc-agent never executes before consuming GO). 5 × 3s of ACK waiting
/// (plus reconnect time) spans realistic snapshot pause durations (~15s).
const MAX_ACK_ATTEMPTS: u32 = 5;

/// How long a post-GO response read may sit idle before the client checks the
/// VM's persisted `vsock_epoch` for the snapshot-pause orphan mode (see
/// `SnapshotOrphanGuard`). Purely a polling interval, NOT a silence limit:
/// commands like phps cookie gen can be silent for >10 min — with an
/// unchanged epoch the wait continues indefinitely. Only a session whose
/// transport was reset by a snapshot pause aborts.
pub(crate) const EXEC_EPOCH_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Post-handshake / handshake write timeout.
const EXEC_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// The exec socket for a running VM: the recorded path, or the conventional one.
///
/// Snapshot bracketing must fail closed when the exact path is unknown, because
/// pausing the wrong guest is worse than not pausing. Exec has the opposite
/// requirement: a VM started before `vsock_socket_path` existed deserializes it as
/// `None`, and refusing those would make every VM already running across a CLI
/// upgrade permanently uncontrollable. The conventional path is only used when it
/// is actually there, so this still cannot invent a target.
fn exec_vsock_socket_path(vm_state: &crate::state::VmState) -> Result<PathBuf> {
    match super::common::recorded_vsock_socket_path(vm_state) {
        Ok(path) => Ok(path.to_path_buf()),
        // Resolved only here: `vm_runtime_dir` reads the loaded config, and a VM that
        // recorded its own socket has no reason to make the caller depend on that.
        Err(error) => {
            conventional_exec_socket(&crate::paths::vm_runtime_dir(&vm_state.vm_id), error)
        }
    }
}

/// The same decision against a caller-supplied runtime directory, for tests.
#[cfg(test)]
fn exec_vsock_socket_path_in(
    vm_state: &crate::state::VmState,
    runtime_dir: &Path,
) -> Result<PathBuf> {
    match super::common::recorded_vsock_socket_path(vm_state) {
        Ok(path) => Ok(path.to_path_buf()),
        Err(error) => conventional_exec_socket(runtime_dir, error),
    }
}

fn conventional_exec_socket(runtime_dir: &Path, error: anyhow::Error) -> Result<PathBuf> {
    let conventional = runtime_dir.join("vsock.sock");
    if conventional.exists() {
        return Ok(conventional);
    }
    Err(error.context(format!(
        "and the conventional socket {} does not exist",
        conventional.display()
    )))
}

/// Connect to the exec server via vsock with retry logic.
///
/// The guest VM takes several seconds to boot and start fc-agent with the exec server.
/// This function retries the connection with exponential backoff to handle this startup delay.
///
/// Returns a connected UnixStream on success.
fn connect_to_exec_server_with_retry(vsock_socket: &Path) -> Result<UnixStream> {
    let mut attempt = 0;
    let mut delay_ms = INITIAL_RETRY_DELAY_MS;
    let mut waited_ms: u64 = 0;

    loop {
        attempt += 1;

        // Connect to the vsock Unix socket
        let mut stream = match UnixStream::connect(vsock_socket) {
            Ok(s) => s,
            Err(e) if attempt < MAX_EXEC_CONNECT_ATTEMPTS => {
                debug!(attempt, delay_ms, "vsock socket not ready, retrying");
                std::thread::sleep(Duration::from_millis(delay_ms));
                waited_ms += delay_ms;
                delay_ms = next_retry_delay(delay_ms);
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
                waited_ms += delay_ms;
                delay_ms = next_retry_delay(delay_ms);
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
                    waited_ms += delay_ms;
                    delay_ms = next_retry_delay(delay_ms);
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
                waited_ms += delay_ms;
                delay_ms = next_retry_delay(delay_ms);
                continue;
            }

            bail!(
                "Failed to connect to guest exec server after {} attempts: {}. \
                 Make sure fc-agent is running with exec server enabled.",
                attempt,
                response_str.trim()
            );
        }

        // Success! Attempt count + cumulative retry sleep are logged so
        // benchmark timelines can attribute connect-retry waiting (the ladder's
        // quantum) separately from real guest latency.
        if attempt > 1 {
            debug!(
                attempts = attempt,
                retry_wait_ms = waited_ms,
                "connected to exec server after retries"
            );
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

/// Wait for the agent's ACK line, bounded by an absolute deadline of `timeout`
/// across the WHOLE line — mirroring the agent side's `read_line_bounded`.
///
/// Reads byte-by-byte so no bytes past the ACK newline are ever consumed —
/// everything after ACK belongs to the post-GO exec protocol (JSON lines or
/// TTY frames), which the mode loops read from the raw stream.
///
/// The socket read timeout restarts on every byte, so on its own a
/// byte-dribbling peer could stretch one ACK wait to ~MAX_ACK_LINE_LENGTH ×
/// `timeout`. The contract is a bounded per-attempt wait, so the per-read
/// socket timeout is only the polling mechanism: it is re-armed with the
/// remaining time each iteration, and the deadline decides.
fn read_ack_line(stream: &mut UnixStream, timeout: Duration) -> AckOutcome {
    // An ACK line is 4 bytes; a pre-ACK Error line carries a message. Anything
    // bigger is a protocol violation.
    const MAX_ACK_LINE_LENGTH: usize = 65_536;
    let deadline = Instant::now() + timeout;
    let mut line: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return AckOutcome::NotAcked(format!("timed out after {:?} waiting for ACK", timeout));
        }
        // Never zero here (checked above) — a zero read timeout means "block forever".
        if let Err(e) = stream.set_read_timeout(Some(remaining)) {
            return AckOutcome::NotAcked(format!("failed to arm ACK read timeout: {}", e));
        }
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
                    timeout
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

/// A post-GO exec session was orphaned by a VM snapshot pause.
///
/// The VM's persisted `vsock_epoch` changed while this session was blocked
/// waiting for a response: a snapshot create (startup snapshot or
/// `fcvm snapshot create --pid`) paused the VM and reset its vsock transport,
/// silently killing connections from before the pause — reads would otherwise
/// block forever with no error on either side. Never resent: execution may
/// already have happened.
#[derive(Debug)]
pub struct ExecOrphanedBySnapshotPause;

impl std::fmt::Display for ExecOrphanedBySnapshotPause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "exec session orphaned by a VM snapshot pause: the VM's vsock epoch \
             changed while this exec was waiting for a response — a snapshot \
             (startup snapshot or `fcvm snapshot create`) paused the VM and reset \
             vsock, silently killing this in-flight session. Not resending: the \
             command may already have executed. Retry the exec."
        )
    }
}

impl std::error::Error for ExecOrphanedBySnapshotPause {}

/// Detects the snapshot-pause orphan mode for post-GO exec sessions.
///
/// A snapshot pause resets the VM's vsock transport and silently orphans
/// in-flight connections: reads block forever and writes can "succeed" into
/// the dead host socket. Pre-ACK, the bounded handshake handles this (resend);
/// post-GO, resending is forbidden, so the snapshot paths bump
/// `VmState::vsock_epoch` after every pause/save (before resuming — see
/// `StateManager::bump_vsock_epoch`) and blocked exec readers poll it: a
/// changed epoch means this session's transport was reset.
///
/// The baseline is captured after the session's vsock connection is
/// established and before the request is sent (see `connect_and_start_exec`
/// for the ordering argument). `check` costs one small file read and runs only
/// after a read has been idle for `EXEC_EPOCH_POLL_INTERVAL` — never per byte.
pub struct SnapshotOrphanGuard {
    state_file: Option<PathBuf>,
    /// Epoch at capture time. `None` if the state file was missing or
    /// unreadable at capture (VM without persisted state — e.g. library-API
    /// tests against fc-mock); the guard then never fires.
    baseline: Option<u64>,
}

impl SnapshotOrphanGuard {
    /// Capture the current epoch of the VM whose state file is `state_file`.
    fn capture(state_file: PathBuf) -> Self {
        let baseline = read_vsock_epoch(&state_file);
        Self {
            state_file: Some(state_file),
            baseline,
        }
    }

    /// A guard that never fires — for TTY sessions that are not exec sessions
    /// (the `podman run -it` console socket is host-bound, not an exec vsock).
    pub fn disabled() -> Self {
        Self {
            state_file: None,
            baseline: None,
        }
    }

    /// Check whether the VM's vsock epoch moved past this session's baseline.
    ///
    /// A missing/unreadable state file is NOT an orphan signal: VM teardown
    /// deletes the state file but also kills the hypervisor, which closes the
    /// vsock socket and surfaces a loud read error on its own.
    pub fn check(&self) -> std::result::Result<(), ExecOrphanedBySnapshotPause> {
        let (Some(state_file), Some(baseline)) = (self.state_file.as_deref(), self.baseline) else {
            return Ok(());
        };
        match read_vsock_epoch(state_file) {
            Some(current) if current != baseline => Err(ExecOrphanedBySnapshotPause),
            _ => Ok(()),
        }
    }
}

/// Read `vsock_epoch` from a VM state file. A state file written before the
/// field existed reads as 0; a missing/unparseable file reads as `None`.
///
/// Deliberately lock-free: state writers write a temp file and atomically
/// rename it into place, so a plain read can never observe a torn write.
/// Taking the per-VM flock here would also recreate lock files that
/// `delete_state` just removed.
fn read_vsock_epoch(state_file: &Path) -> Option<u64> {
    let json = std::fs::read_to_string(state_file).ok()?;
    let value: serde_json::Value = serde_json::from_str(&json).ok()?;
    Some(
        value
            .get("vsock_epoch")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    )
}

/// Read adapter for post-GO exec streams.
///
/// The socket carries a short read timeout (`EXEC_EPOCH_POLL_INTERVAL`) purely
/// as a polling clock: each time a read has sat idle that long, the
/// snapshot-orphan guard is consulted and the wait continues. A command that
/// is silent for an hour with an unchanged epoch keeps waiting — this bounds
/// the ORPHAN case, not honest silence. A session orphaned by a snapshot pause
/// fails the read with `ExecOrphanedBySnapshotPause` instead of blocking
/// forever.
pub(crate) struct EpochGuardedReader {
    stream: UnixStream,
    guard: SnapshotOrphanGuard,
}

impl EpochGuardedReader {
    pub(crate) fn new(stream: UnixStream, guard: SnapshotOrphanGuard) -> Self {
        Self { stream, guard }
    }
}

impl Read for EpochGuardedReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            match self.stream.read(buf) {
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    self.guard.check().map_err(std::io::Error::other)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                other => return other,
            }
        }
    }
}

/// Connect, send the exec request, and complete the three-phase handshake
/// (request → ACK → GO). Returns a stream on which execution has just been
/// authorized — the next bytes from the agent are exec responses — plus the
/// session's `SnapshotOrphanGuard` for the post-GO response wait.
///
/// A request that never gets ACKed (its connection was orphaned by a snapshot
/// pause's vsock reset — silent, no error on either side) provably never
/// executed, so it is resent on a fresh connection, bounded by
/// `MAX_ACK_ATTEMPTS`. After GO is sent, no resend ever happens — fc-agent may
/// have started executing — so a subsequent connection death surfaces as a
/// loud error from the mode loops instead (see `ExecConnectionClosed` and
/// `ExecOrphanedBySnapshotPause`).
pub fn connect_and_start_exec(
    vsock_socket: &Path,
    request: &ExecRequest,
    vm_id: &str,
) -> Result<(UnixStream, SnapshotOrphanGuard)> {
    let request_json = serde_json::to_string(request)?;
    let state_file = paths::state_dir().join(format!("{}.json", vm_id));
    let mut attempt = 0;
    loop {
        attempt += 1;
        let mut stream = connect_to_exec_server_with_retry(vsock_socket)?;
        // Capture the epoch baseline HERE — after this attempt's connection
        // exists, before the request is sent. Snapshot paths bump the epoch
        // after the pause but BEFORE the VM resumes, and a vsock CONNECT can
        // only complete against a running guest, so:
        //   - a session orphaned after ACK captured its baseline strictly
        //     before the pause → the bump is observed → loud abort, no hang;
        //   - a session connected after the resume reads the already-bumped
        //     value → no false abort.
        // Re-captured on every retry so a resend starts from the current epoch.
        let guard = SnapshotOrphanGuard::capture(state_file.clone());
        // Read timeouts are armed by read_ack_line (handshake) and below (post-GO).
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

        // Phase 2: wait (bounded by an absolute deadline) for ACK.
        match read_ack_line(&mut stream, ACK_TIMEOUT) {
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
                // Post-GO reads are bounded by the epoch guard, not a wall
                // clock: this short timeout is the guard's polling interval
                // (see EpochGuardedReader), so honest long silences keep
                // waiting while a snapshot-pause orphan aborts within ~one
                // interval.
                stream.set_read_timeout(Some(EXEC_EPOCH_POLL_INTERVAL))?;
                stream.set_write_timeout(Some(EXEC_WRITE_TIMEOUT))?;
                return Ok((stream, guard));
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
    vm_id: &str,
) -> Result<i32> {
    debug!(
        socket = %vsock_socket.display(),
        command = ?command,
        in_container,
        vm_id,
        "executing command in VM"
    );

    let vsock_socket = vsock_socket.to_path_buf();
    let command = command.to_vec();
    let vm_id = vm_id.to_string();

    tokio::task::spawn_blocking(move || {
        // Build the exec request (non-interactive, no TTY)
        let request = ExecRequest {
            command,
            in_container,
            interactive: false,
            tty: false,
        };

        // Connect, send the request, and complete the ACK/GO handshake
        let (stream, guard) = connect_and_start_exec(&vsock_socket, &request, &vm_id)?;
        debug!("exec handshake complete");

        // Run in line mode and capture exit code
        run_line_mode_with_exit_code(stream, guard)
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

    // Use the exact persisted path. `--vsock-dir` deliberately places the
    // socket outside vm_runtime_dir, so reconstructing it breaks exec/health.
    let vsock_socket = exec_vsock_socket_path(&vm_state)?;

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
    let (stream, guard) = connect_and_start_exec(&vsock_socket, &request, &vm_state.vm_id)?;

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
        match super::tty::run_tty_session_connected(stream, tty, interactive, guard) {
            Ok(exit_code) => {
                if exit_code != 0 {
                    std::process::exit(exit_code);
                }
                Ok(())
            }
            Err(e) => Err(e),
        }
    } else {
        run_line_mode(stream, guard)
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
fn run_line_mode_with_exit_code(stream: UnixStream, guard: SnapshotOrphanGuard) -> Result<i32> {
    let reader = BufReader::new(EpochGuardedReader::new(stream, guard));
    read_exec_responses(
        reader,
        |data| print!("{}", data),
        |data| eprint!("{}", data),
    )
}

/// Run in line-buffered mode (non-TTY)
fn run_line_mode(stream: UnixStream, guard: SnapshotOrphanGuard) -> Result<()> {
    let exit_code = run_line_mode_with_exit_code(stream, guard)?;

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
    vm_id: &str,
) -> Result<ExecOutput> {
    debug!(
        socket = %vsock_socket.display(),
        command = ?command,
        in_container,
        vm_id,
        "executing command in VM (captured)"
    );

    let vsock_socket = vsock_socket.to_path_buf();
    let command = command.to_vec();
    let vm_id = vm_id.to_string();

    tokio::task::spawn_blocking(move || {
        let request = ExecRequest {
            command,
            in_container,
            interactive: false,
            tty: false,
        };

        // Connect, send the request, and complete the ACK/GO handshake
        let (stream, guard) = connect_and_start_exec(&vsock_socket, &request, &vm_id)?;

        // Read lines and capture into strings instead of printing
        let reader = BufReader::new(EpochGuardedReader::new(stream, guard));
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
/// bytes from the agent are exec responses / TTY frames), plus the session's
/// `SnapshotOrphanGuard`.
///
/// Useful for building WebSocket↔vsock bridges (terminal sessions). The tokio
/// stream is nonblocking, so the socket read timeout used by the blocking mode
/// loops does not apply — the async consumer must run the guard on its own
/// idle poll interval (see `serve.rs`'s websocket terminal).
///
/// The blocking connect/handshake logic is offloaded to a blocking thread pool
/// so this function is safe to call from async contexts.
pub async fn start_exec_session_async(
    vsock_socket: &Path,
    request: ExecRequest,
    vm_id: &str,
) -> Result<(tokio::net::UnixStream, SnapshotOrphanGuard)> {
    let vsock_socket = vsock_socket.to_path_buf();
    let vm_id = vm_id.to_string();
    let (std_stream, guard) = tokio::task::spawn_blocking(move || {
        connect_and_start_exec(&vsock_socket, &request, &vm_id)
    })
    .await
    .context("connect task panicked")??;
    std_stream.set_nonblocking(true)?;
    let stream =
        tokio::net::UnixStream::from_std(std_stream).context("converting to tokio UnixStream")?;
    Ok((stream, guard))
}

// TIER 0 protocol-interleaving fuzz: scripted fake agents enumerating connection
// death at every handshake stage, plus the exactly-once execution property
// (kept in its own file — see exec_fuzz_tests.rs).
#[cfg(test)]
#[path = "exec_fuzz_tests.rs"]
mod exec_fuzz_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_uses_exact_persisted_custom_vsock_path() {
        let mut state =
            crate::state::VmState::new("vm-custom-exec".to_string(), "alpine".to_string(), 1, 128);
        state.config.vsock_socket_path = Some(PathBuf::from("/srv/fcvm-vsock/vsock.sock"));
        assert_eq!(
            exec_vsock_socket_path(&state).unwrap(),
            PathBuf::from("/srv/fcvm-vsock/vsock.sock")
        );

        // The absent-path cases live in
        // `exec_reaches_a_pre_upgrade_vm_through_its_conventional_socket`, which
        // supplies its own runtime directory rather than reading the host config.
    }

    /// A VM that was already running when the CLI gained `vsock_socket_path`
    /// deserializes it as `None`. Refusing those would make every VM running across
    /// an upgrade permanently uncontrollable, so exec falls back to the conventional
    /// socket. Snapshot bracketing stays fail-closed: it never takes this path.
    #[test]
    fn exec_reaches_a_pre_upgrade_vm_through_its_conventional_socket() {
        let runtime_dir = tempfile::tempdir().expect("runtime dir");
        let mut state =
            crate::state::VmState::new("vm-pre-upgrade".to_string(), "alpine".to_string(), 1, 128);
        state.config.vsock_socket_path = None;

        // Nothing there yet: a missing socket must not be invented.
        let error = exec_vsock_socket_path_in(&state, runtime_dir.path()).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("no recorded vsock socket path")
                && message.contains("conventional socket"),
            "the error must name both attempts: {message}"
        );

        let conventional = runtime_dir.path().join("vsock.sock");
        std::fs::write(&conventional, b"").expect("create conventional socket");
        assert_eq!(
            exec_vsock_socket_path_in(&state, runtime_dir.path()).unwrap(),
            conventional,
            "a pre-upgrade VM whose conventional socket exists must stay reachable"
        );

        // A recorded path always wins, so --vsock-dir is never silently bypassed.
        state.config.vsock_socket_path = Some(PathBuf::from("/srv/custom/vsock.sock"));
        assert_eq!(
            exec_vsock_socket_path_in(&state, runtime_dir.path()).unwrap(),
            PathBuf::from("/srv/custom/vsock.sock")
        );
    }

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

        assert!(matches!(
            read_ack_line(&mut client, ACK_TIMEOUT),
            AckOutcome::Acked
        ));

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
        match read_ack_line(&mut client, Duration::from_secs(1)) {
            AckOutcome::NotAcked(reason) => assert!(reason.contains("closed"), "{reason}"),
            other => panic!("expected NotAcked, got {:?}", other),
        }
    }

    /// Silence (the snapshot-pause orphan shape) → bounded NotAcked timeout,
    /// not a hang.
    #[test]
    fn read_ack_line_timeout_is_not_acked() {
        let (mut client, _server) = UnixStream::pair().unwrap();
        let start = std::time::Instant::now();
        match read_ack_line(&mut client, Duration::from_millis(100)) {
            AckOutcome::NotAcked(reason) => assert!(reason.contains("timed out"), "{reason}"),
            other => panic!("expected NotAcked, got {:?}", other),
        }
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    /// CodeRabbit finding: the per-read socket timeout restarts on every byte,
    /// so a peer dribbling one byte just inside each window could stretch a
    /// single ACK wait to ~MAX_ACK_LINE_LENGTH × timeout. The deadline must be
    /// absolute across the whole line (contract: bounded per-attempt wait).
    #[test]
    fn read_ack_line_dribbled_bytes_hit_absolute_deadline() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let writer = std::thread::spawn(move || {
            // Dribble a byte every 25ms, ~1s total — each byte arrives well
            // inside a freshly-restarted per-read timeout, so only the
            // absolute deadline can end the wait.
            for _ in 0..40 {
                if server.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        });

        let start = std::time::Instant::now();
        let outcome = read_ack_line(&mut client, Duration::from_millis(200));
        let elapsed = start.elapsed();
        match outcome {
            AckOutcome::NotAcked(reason) => assert!(reason.contains("timed out"), "{reason}"),
            other => panic!("expected NotAcked timeout, got {:?}", other),
        }
        assert!(
            elapsed < Duration::from_millis(800),
            "deadline was not absolute: dribbled bytes kept the read alive for {:?}",
            elapsed
        );

        drop(client);
        writer.join().unwrap();
    }

    /// A pre-ACK Error response (invalid/empty request) → Rejected with the
    /// agent's message; deterministic, so the client must not resend.
    #[test]
    fn read_ack_line_error_response_is_rejected() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let error_line = response_line(&ExecResponse::Error("Empty command".into()));
        server.write_all(error_line.as_bytes()).unwrap();

        match read_ack_line(&mut client, Duration::from_secs(1)) {
            AckOutcome::Rejected(msg) => assert_eq!(msg, "Empty command"),
            other => panic!("expected Rejected, got {:?}", other),
        }
    }

    /// Write a minimal VM state file with the given epoch; returns its path.
    fn write_state_with_epoch(dir: &Path, vm_id: &str, epoch: u64) -> PathBuf {
        let path = dir.join(format!("{}.json", vm_id));
        std::fs::write(
            &path,
            format!(r#"{{"vm_id":"{}","vsock_epoch":{}}}"#, vm_id, epoch),
        )
        .unwrap();
        path
    }

    /// An unchanged epoch keeps the session waiting — honest silence (a
    /// command quiet for an hour) is NOT bounded, only the orphan case is.
    #[test]
    fn snapshot_orphan_guard_unchanged_epoch_keeps_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let state_file = write_state_with_epoch(dir.path(), "vm-guard-same", 3);
        let guard = SnapshotOrphanGuard::capture(state_file);
        assert!(guard.check().is_ok());
        assert!(guard.check().is_ok(), "repeated checks must stay quiet");
    }

    /// A bumped epoch means a snapshot pause reset the vsock transport while
    /// this session was in flight — the check must fail loudly, naming the
    /// snapshot-pause orphan mode.
    #[test]
    fn snapshot_orphan_guard_bumped_epoch_aborts_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let state_file = write_state_with_epoch(dir.path(), "vm-guard-bump", 3);
        let guard = SnapshotOrphanGuard::capture(state_file);

        write_state_with_epoch(dir.path(), "vm-guard-bump", 4);
        let err = guard.check().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("snapshot"), "must name the cause: {msg}");
        assert!(msg.contains("pause"), "must name the orphan mode: {msg}");
        assert!(msg.contains("Not resending"), "must forbid resend: {msg}");
    }

    /// State files written before vsock_epoch existed read as epoch 0, so a
    /// bump (0 → 1) is still detected against them.
    #[test]
    fn snapshot_orphan_guard_missing_field_reads_as_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vm-guard-old.json");
        std::fs::write(&path, r#"{"vm_id":"vm-guard-old"}"#).unwrap();

        let guard = SnapshotOrphanGuard::capture(path);
        assert!(guard.check().is_ok());

        write_state_with_epoch(dir.path(), "vm-guard-old", 1);
        assert!(guard.check().is_err(), "bump from implicit 0 must fire");
    }

    /// A state file that disappears is NOT an orphan signal: teardown deletes
    /// state but also kills the hypervisor, which errors the socket loudly on
    /// its own. The guard must not abort a session it cannot classify.
    #[test]
    fn snapshot_orphan_guard_missing_state_file_is_no_signal() {
        let dir = tempfile::tempdir().unwrap();
        let state_file = write_state_with_epoch(dir.path(), "vm-guard-gone", 2);
        let guard = SnapshotOrphanGuard::capture(state_file.clone());

        std::fs::remove_file(&state_file).unwrap();
        assert!(guard.check().is_ok());
    }

    /// No baseline (state file missing at capture — e.g. library API against
    /// fc-mock) → the guard never fires, even if a state file appears later.
    #[test]
    fn snapshot_orphan_guard_without_baseline_never_fires() {
        let dir = tempfile::tempdir().unwrap();
        let guard = SnapshotOrphanGuard::capture(dir.path().join("vm-guard-none.json"));
        assert!(guard.check().is_ok());

        write_state_with_epoch(dir.path(), "vm-guard-none", 7);
        assert!(guard.check().is_ok());

        let disabled = SnapshotOrphanGuard::disabled();
        assert!(disabled.check().is_ok());
    }

    /// The post-GO read loop: an idle read timeout is only a polling tick
    /// (data flowing and honest silence both keep the session alive), while an
    /// epoch bump during the idle wait fails the read with
    /// ExecOrphanedBySnapshotPause instead of hanging.
    #[test]
    fn epoch_guarded_reader_aborts_on_bump_and_passes_data_through() {
        let dir = tempfile::tempdir().unwrap();
        let state_file = write_state_with_epoch(dir.path(), "vm-guard-read", 1);

        let (client, mut server) = UnixStream::pair().unwrap();
        // Short poll interval so the test doesn't wait EXEC_EPOCH_POLL_INTERVAL.
        client
            .set_read_timeout(Some(Duration::from_millis(30)))
            .unwrap();
        let guard = SnapshotOrphanGuard::capture(state_file);
        let mut reader = EpochGuardedReader::new(client, guard);

        // Data flows through even though several poll ticks elapse first.
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            server.write_all(b"hi").unwrap();
            server
        });
        let mut buf = [0u8; 2];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hi");
        let _server = writer.join().unwrap();

        // Now bump the epoch while the socket is idle: the blocked read must
        // abort with the orphan error instead of waiting forever.
        write_state_with_epoch(dir.path(), "vm-guard-read", 2);
        let err = reader.read(&mut buf).unwrap_err();
        let inner = err.get_ref().expect("orphan error must be attached");
        assert!(
            inner.is::<ExecOrphanedBySnapshotPause>(),
            "unexpected error: {err}"
        );
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
