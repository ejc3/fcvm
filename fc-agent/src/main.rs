mod fuse;

use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::process::Stdio;
use std::thread;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    time::{sleep, Duration},
};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Deserialize)]
struct Plan {
    image: String,
    #[serde(default)]
    env: HashMap<String, String>,
    cmd: Option<Vec<String>>,
    /// Volume mounts from host (FUSE-over-vsock)
    #[serde(default)]
    volumes: Vec<VolumeMount>,
    /// Path to OCI archive for localhost/ images (run directly without import)
    #[serde(default)]
    image_archive: Option<String>,
    /// Run container in privileged mode (allows mknod, device access, etc.)
    #[serde(default)]
    privileged: bool,
}

/// Volume mount configuration from MMDS
#[derive(Debug, Clone, Deserialize)]
struct VolumeMount {
    /// Mount path inside guest
    guest_path: String,
    /// Vsock port to connect to host VolumeServer
    vsock_port: u32,
    /// Read-only flag
    #[serde(default)]
    read_only: bool,
}

#[derive(Debug, Deserialize)]
struct LatestMetadata {
    #[serde(rename = "host-time")]
    host_time: String,
    #[serde(rename = "restore-epoch")]
    restore_epoch: Option<String>,
    /// Volume mounts for clone restore (provided by fcvm snapshot run)
    #[serde(default)]
    volumes: Vec<VolumeMount>,
}

async fn fetch_plan() -> Result<Plan> {
    // MMDS V2 requires getting a session token first
    let client = reqwest::Client::new();

    // Step 1: Get session token
    eprintln!(
        "[fc-agent] requesting MMDS V2 session token from http://169.254.169.254/latest/api/token"
    );
    let token_response = match client
        .put("http://169.254.169.254/latest/api/token")
        .header("X-metadata-token-ttl-seconds", "21600")
        .timeout(Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) => {
            eprintln!("[fc-agent] token request succeeded");
            resp
        }
        Err(e) => {
            eprintln!("[fc-agent] token request FAILED - detailed error:");
            eprintln!("[fc-agent]   error type: {:?}", e);
            if e.is_timeout() {
                eprintln!("[fc-agent]   → TIMEOUT: MMDS not responding within 5 seconds");
            } else if e.is_connect() {
                eprintln!("[fc-agent]   → CONNECTION ERROR: Cannot reach 169.254.169.254");
            } else if e.is_request() {
                eprintln!("[fc-agent]   → REQUEST ERROR: Problem building request");
            }
            return Err(e).context("requesting MMDS session token");
        }
    };

    let token_status = token_response.status();
    eprintln!(
        "[fc-agent] token response status: {} {}",
        token_status.as_u16(),
        token_status.canonical_reason().unwrap_or("")
    );

    let token = token_response
        .text()
        .await
        .context("reading session token")?;
    eprintln!(
        "[fc-agent] got token: {} bytes ({})",
        token.len(),
        if token.is_empty() { "EMPTY!" } else { "ok" }
    );

    // Step 2: Fetch plan with token from /latest/container-plan
    // IMPORTANT: Must include Accept: application/json to get JSON response instead of IMDS key list
    eprintln!("[fc-agent] fetching plan from http://169.254.169.254/latest/container-plan");
    let plan_response = match client
        .get("http://169.254.169.254/latest/container-plan")
        .header("X-metadata-token", &token)
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) => {
            eprintln!("[fc-agent] plan request succeeded");
            resp
        }
        Err(e) => {
            eprintln!("[fc-agent] plan request FAILED - detailed error:");
            eprintln!("[fc-agent]   error type: {:?}", e);
            if e.is_timeout() {
                eprintln!("[fc-agent]   → TIMEOUT: MMDS not responding within 5 seconds");
            } else if e.is_connect() {
                eprintln!("[fc-agent]   → CONNECTION ERROR: Cannot reach 169.254.169.254");
            } else if e.is_request() {
                eprintln!("[fc-agent]   → REQUEST ERROR: Problem building request");
            }
            return Err(e).context("fetching from MMDS");
        }
    };

    let plan_status = plan_response.status();
    eprintln!(
        "[fc-agent] plan response status: {} {}",
        plan_status.as_u16(),
        plan_status.canonical_reason().unwrap_or("")
    );

    if !plan_status.is_success() {
        eprintln!(
            "[fc-agent] ERROR: HTTP {} - this is NOT a 2xx success code",
            plan_status.as_u16()
        );
    }

    let body = plan_response.text().await.context("reading plan body")?;
    eprintln!(
        "[fc-agent] plan response body ({} bytes): {}",
        body.len(),
        body
    );

    let plan: Plan = match serde_json::from_str(&body) {
        Ok(p) => {
            eprintln!("[fc-agent] successfully parsed JSON into Plan struct");
            p
        }
        Err(e) => {
            eprintln!("[fc-agent] JSON PARSING FAILED:");
            eprintln!("[fc-agent]   parse error: {}", e);
            eprintln!("[fc-agent]   body was: {}", body);
            return Err(e.into());
        }
    };

    Ok(plan)
}

/// Watch for restore-epoch changes in MMDS and handle clone restore
/// This runs as a background task to handle snapshot restore scenarios
async fn watch_restore_epoch() {
    let mut last_epoch: Option<String> = None;

    // Poll every 100ms - simple and fast enough to detect restores quickly
    // The CPU overhead is negligible (~0.1% of one core)
    loop {
        sleep(Duration::from_millis(100)).await;

        // Create a fresh client each time to handle snapshot restore
        // (TCP connections are invalidated after snapshot restore)
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(500))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        // Try to fetch current metadata (including restore-epoch and volumes)
        let metadata = match fetch_latest_metadata(&client).await {
            Ok(m) => m,
            Err(_) => continue, // Ignore errors, just keep polling
        };

        // Check if epoch changed or if this is the first time we see one
        if let Some(ref current) = metadata.restore_epoch {
            match &last_epoch {
                None => {
                    // First time seeing an epoch - THIS IS A CLONE RESTORE!
                    // On fresh boot, there is no restore-epoch in MMDS yet.
                    // If we see one, we were restored from a snapshot.
                    eprintln!(
                        "[fc-agent] detected restore-epoch: {} (clone restore detected)",
                        current
                    );
                    handle_clone_restore(&metadata.volumes).await;
                    last_epoch = metadata.restore_epoch;
                }
                Some(prev) if prev != current => {
                    // Epoch changed! This means we were restored from snapshot again
                    eprintln!("[fc-agent] restore-epoch changed: {} -> {}", prev, current);
                    handle_clone_restore(&metadata.volumes).await;
                    last_epoch = metadata.restore_epoch;
                }
                _ => {
                    // No change
                }
            }
        }
    }
}

/// Handle clone restore: kill stale sockets, flush ARP, and remount volumes
async fn handle_clone_restore(volumes: &[VolumeMount]) {
    // 1. KILL all established TCP connections immediately
    // After snapshot restore, existing TCP connections are DEAD (different network namespace).
    // Processes blocked on read() will hang FOREVER because no packets arrive.
    // ss -K destroys sockets directly, waking any blocked read()/write() calls.
    kill_stale_tcp_connections().await;

    // 2. Flush ARP cache (stale MAC entries from previous network)
    flush_arp_cache().await;

    // Note: Interface bounce (ip link down/up) is NOT needed - ss -K handles socket cleanup
    // more effectively by directly destroying sockets rather than hoping they notice ENETDOWN.

    // 3. Remount FUSE volumes if any
    if !volumes.is_empty() {
        eprintln!(
            "[fc-agent] clone has {} volume(s) to remount",
            volumes.len()
        );
        remount_fuse_volumes(volumes).await;
    }
}

/// Remount FUSE volumes after clone restore.
/// The old vsock connections are broken, so we unmount and remount.
async fn remount_fuse_volumes(volumes: &[VolumeMount]) {
    for vol in volumes {
        eprintln!(
            "[fc-agent] remounting volume at {} (port {})",
            vol.guest_path, vol.vsock_port
        );

        // First, try to unmount the old (broken) FUSE mount
        // Use lazy unmount (-l) in case there are open files
        let umount_output = Command::new("umount")
            .args(["-l", &vol.guest_path])
            .output()
            .await;

        match umount_output {
            Ok(o) if o.status.success() => {
                eprintln!("[fc-agent] unmounted old FUSE mount at {}", vol.guest_path);
            }
            Ok(o) => {
                // Not mounted or error - that's fine, we'll mount fresh
                eprintln!(
                    "[fc-agent] umount {} (may not be mounted): {}",
                    vol.guest_path,
                    String::from_utf8_lossy(&o.stderr).trim()
                );
            }
            Err(e) => {
                eprintln!("[fc-agent] umount error for {}: {}", vol.guest_path, e);
            }
        }

        // Small delay to ensure unmount completes
        sleep(Duration::from_millis(100)).await;

        // Create mount point directory (in case it doesn't exist)
        if let Err(e) = std::fs::create_dir_all(&vol.guest_path) {
            eprintln!(
                "[fc-agent] ERROR: cannot create mount point {}: {}",
                vol.guest_path, e
            );
            continue;
        }

        // Mount FUSE filesystem in a background thread using fuse-pipe
        let mount_path = vol.guest_path.clone();
        let port = vol.vsock_port;

        thread::spawn(move || {
            eprintln!("[fc-agent] fuse: starting remount at {}", mount_path);
            if let Err(e) = fuse::mount_vsock(port, &mount_path) {
                eprintln!("[fc-agent] FUSE remount error at {}: {}", mount_path, e);
            }
            eprintln!("[fc-agent] fuse: remount at {} exited", mount_path);
        });

        eprintln!("[fc-agent] volume {} remount initiated", vol.guest_path);
    }

    // Give FUSE mounts time to initialize
    if !volumes.is_empty() {
        eprintln!("[fc-agent] waiting for FUSE remounts to initialize...");
        sleep(Duration::from_millis(500)).await;
        eprintln!("[fc-agent] ✓ volume remounts complete");
    }
}

async fn fetch_latest_metadata(client: &reqwest::Client) -> Result<LatestMetadata> {
    let token_response = client
        .put("http://169.254.169.254/latest/api/token")
        .header("X-metadata-token-ttl-seconds", "21600")
        .timeout(Duration::from_millis(500))
        .send()
        .await?;
    let token = token_response.text().await?;

    let response = client
        .get("http://169.254.169.254/latest")
        .header("X-metadata-token", &token)
        .header("Accept", "application/json")
        .timeout(Duration::from_millis(500))
        .send()
        .await?;

    let body = response.text().await?;
    let metadata: LatestMetadata = serde_json::from_str(&body)?;
    Ok(metadata)
}

async fn flush_arp_cache() {
    let output = Command::new("ip")
        .args(["neigh", "flush", "all"])
        .output()
        .await;

    match output {
        Ok(o) if o.status.success() => {
            eprintln!("[fc-agent] ✓ ARP cache flushed successfully");
        }
        Ok(o) => {
            eprintln!(
                "[fc-agent] WARNING: ARP flush failed: {}",
                String::from_utf8_lossy(&o.stderr)
            );
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: ARP flush error: {}", e);
        }
    }
}

/// Kill all established TCP connections in the VM.
/// After snapshot restore, these connections point to a dead network namespace.
/// Processes blocked on read() will hang FOREVER because no packets arrive.
/// Interface bounce does NOT deliver errors to blocked sockets - we must explicitly
/// destroy them so the kernel sends RST and wakes blocked threads.
///
/// This is comprehensive: we kill ALL TCP connections, not just some.
/// Applications should reconnect when their sockets die.
async fn kill_stale_tcp_connections() {
    // First, list current connections for logging
    let list_output = Command::new("ss")
        .args(["-tn", "state", "established"])
        .output()
        .await;

    if let Ok(o) = &list_output {
        let connections = String::from_utf8_lossy(&o.stdout);
        let count = connections.lines().count().saturating_sub(1); // Subtract header line
        if count > 0 {
            eprintln!(
                "[fc-agent] found {} established TCP connection(s) to kill",
                count
            );
            for line in connections.lines().skip(1) {
                eprintln!("[fc-agent]   {}", line);
            }
        } else {
            eprintln!("[fc-agent] no established TCP connections to kill");
            return;
        }
    }

    // Kill ALL established TCP connections using ss -K
    // The -K flag uses the kernel's TCP socket destroy mechanism
    // This sends RST to remote and wakes up any blocked read()/write() calls
    let kill_output = Command::new("ss")
        .args(["-K", "state", "established"])
        .output()
        .await;

    match kill_output {
        Ok(o) if o.status.success() => {
            eprintln!("[fc-agent] ✓ killed all established TCP connections");
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            // ss -K may fail if iproute2 was built without INET_DIAG_DESTROY support
            // In that case, fall back to a different approach
            if stderr.contains("INET_DIAG_DESTROY") || stderr.contains("Operation not supported") {
                eprintln!("[fc-agent] ss -K not supported, trying conntrack");
                kill_connections_via_conntrack().await;
            } else {
                eprintln!("[fc-agent] WARNING: ss -K failed: {}", stderr);
            }
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: ss -K error: {}", e);
        }
    }

    // Give the kernel a moment to process socket destruction
    sleep(Duration::from_millis(10)).await;
}

/// Fallback: try to kill connections using conntrack (if available)
/// This works for NAT'd connections tracked by nf_conntrack
async fn kill_connections_via_conntrack() {
    // conntrack -F flushes the connection tracking table
    let output = Command::new("conntrack").args(["-F"]).output().await;

    match output {
        Ok(o) if o.status.success() => {
            eprintln!("[fc-agent] ✓ flushed conntrack table");
        }
        Ok(o) => {
            // conntrack may not be available or no tracked connections
            let stderr = String::from_utf8_lossy(&o.stderr);
            if !stderr.contains("No such file") {
                eprintln!("[fc-agent] conntrack flush: {}", stderr.trim());
            }
        }
        Err(_) => {
            // conntrack not available, that's fine
        }
    }
}

/// Watch for lock test trigger file and run lock tests when it appears
/// The trigger file contains the number of iterations to run
/// This runs in clones that have a shared volume mounted at /mnt/shared
async fn watch_for_lock_test(clone_id: String) {
    let trigger_path = "/mnt/shared/run-lock-test";
    let counter_path = "/mnt/shared/counter.txt";
    let append_path = "/mnt/shared/append.log";

    eprintln!(
        "[fc-agent] watching for lock test trigger at {}",
        trigger_path
    );

    // Poll for trigger file
    loop {
        sleep(Duration::from_millis(500)).await;

        // Check if trigger file exists
        if std::path::Path::new(trigger_path).exists() {
            // Read iterations count
            let iterations: usize = match std::fs::read_to_string(trigger_path) {
                Ok(content) => content.trim().parse().unwrap_or(100),
                Err(_) => continue,
            };

            eprintln!(
                "[fc-agent] lock test triggered! clone={} iterations={}",
                clone_id, iterations
            );

            // Run lock tests
            run_lock_tests(&clone_id, iterations, counter_path, append_path);

            // Write done file
            let done_path = format!("/mnt/shared/done-{}", clone_id);
            if let Err(e) = std::fs::write(&done_path, "done") {
                eprintln!("[fc-agent] ERROR writing done file: {}", e);
            } else {
                eprintln!("[fc-agent] ✓ lock test complete, wrote {}", done_path);
            }

            // Only run once per trigger
            break;
        }
    }
}

/// Run lock tests: counter increment + append to file
/// Uses POSIX file locking (flock) to ensure no corruption
fn run_lock_tests(clone_id: &str, iterations: usize, counter_path: &str, append_path: &str) {
    eprintln!("[fc-agent] running {} lock iterations", iterations);

    for i in 0..iterations {
        // Test 1: Counter increment with lock
        if let Err(e) = increment_counter_with_lock(counter_path) {
            eprintln!("[fc-agent] ERROR incrementing counter (iter {}): {}", i, e);
        }

        // Test 2: Append to log with lock
        if let Err(e) = append_with_lock(append_path, clone_id, i) {
            eprintln!("[fc-agent] ERROR appending to log (iter {}): {}", i, e);
        }

        // Small delay between iterations to increase chance of contention
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    eprintln!("[fc-agent] completed {} lock iterations", iterations);
}

/// Increment a counter file with exclusive lock
/// Uses flock for POSIX advisory locking
fn increment_counter_with_lock(path: &str) -> Result<()> {
    // Open file for read+write
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .context("opening counter file")?;

    // Acquire exclusive lock (blocking)
    file.lock_exclusive()
        .context("acquiring exclusive lock on counter")?;

    // Read current value
    let mut content = String::new();
    file.read_to_string(&mut content)
        .context("reading counter")?;
    let current: i64 = content.trim().parse().unwrap_or(0);

    // Increment
    let new_value = current + 1;

    // Write new value (truncate and rewrite)
    file.seek(SeekFrom::Start(0)).context("seeking to start")?;
    file.set_len(0).context("truncating file")?;
    write!(file, "{}", new_value).context("writing new counter value")?;
    file.sync_all().context("syncing counter file")?;

    // Lock is automatically released when file is dropped
    Ok(())
}

/// Append a line to a log file with exclusive lock
fn append_with_lock(path: &str, clone_id: &str, iteration: usize) -> Result<()> {
    // Open file for append
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .context("opening append file")?;

    // Acquire exclusive lock (blocking)
    file.lock_exclusive()
        .context("acquiring exclusive lock on append file")?;

    // Write line with clone ID, iteration, and timestamp
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    let line = format!("{}:{}:{}\n", clone_id, iteration, timestamp);

    // Use BufWriter for atomic-ish write
    let mut writer = std::io::BufWriter::new(&file);
    writer
        .write_all(line.as_bytes())
        .context("writing append line")?;
    writer.flush().context("flushing append file")?;

    // Lock is automatically released when file is dropped
    Ok(())
}

/// Status channel port for notifying host that container is running
const STATUS_VSOCK_PORT: u32 = 4999;

/// Exec server port for running commands from host
const EXEC_VSOCK_PORT: u32 = 4998;

/// Container output streaming port
const OUTPUT_VSOCK_PORT: u32 = 4997;

/// Host CID for vsock (always 2)
const HOST_CID: u32 = 2;

/// Request from host to execute a command
#[derive(Debug, Deserialize)]
struct ExecRequest {
    command: Vec<String>,
    #[serde(default)]
    in_container: bool,
    /// Keep STDIN open (-i)
    #[serde(default)]
    interactive: bool,
    /// Allocate a pseudo-TTY (-t)
    #[serde(default)]
    tty: bool,
}

/// Response sent back to host
#[derive(Debug, Serialize)]
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

/// Wrapper for vsock fd to use with tokio's AsyncFd
struct VsockListener {
    fd: i32,
}

impl std::os::unix::io::AsRawFd for VsockListener {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        self.fd
    }
}

/// Run the exec server that listens for commands from host via vsock
async fn run_exec_server() {
    eprintln!(
        "[fc-agent] starting exec server on vsock port {}",
        EXEC_VSOCK_PORT
    );

    // Create vsock listener socket
    let listener_fd =
        unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_NONBLOCK, 0) };

    if listener_fd < 0 {
        eprintln!(
            "[fc-agent] ERROR: failed to create vsock listener: {}",
            std::io::Error::last_os_error()
        );
        return;
    }

    // Bind to the exec port
    let addr = libc::sockaddr_vm {
        svm_family: libc::AF_VSOCK as u16,
        svm_reserved1: 0,
        svm_port: EXEC_VSOCK_PORT,
        svm_cid: libc::VMADDR_CID_ANY,
        svm_zero: [0u8; 4],
    };

    let bind_result = unsafe {
        libc::bind(
            listener_fd,
            &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_vm>() as u32,
        )
    };

    if bind_result < 0 {
        eprintln!(
            "[fc-agent] ERROR: failed to bind vsock listener: {}",
            std::io::Error::last_os_error()
        );
        unsafe { libc::close(listener_fd) };
        return;
    }

    // Start listening with larger backlog for parallel exec stress
    // Default of 5 is too small when many execs arrive simultaneously
    let listen_result = unsafe { libc::listen(listener_fd, 128) };
    if listen_result < 0 {
        eprintln!(
            "[fc-agent] ERROR: failed to listen on vsock: {}",
            std::io::Error::last_os_error()
        );
        unsafe { libc::close(listener_fd) };
        return;
    }

    eprintln!(
        "[fc-agent] ✓ exec server listening on vsock port {}",
        EXEC_VSOCK_PORT
    );

    // Wrap in AsyncFd for async accept
    let listener = VsockListener { fd: listener_fd };
    let async_fd = match tokio::io::unix::AsyncFd::new(listener) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("[fc-agent] ERROR: failed to create AsyncFd: {}", e);
            unsafe { libc::close(listener_fd) };
            return;
        }
    };

    // Accept connections in a loop
    loop {
        // Wait for the socket to be readable (i.e., a connection is pending)
        let mut guard = match async_fd.readable().await {
            Ok(g) => g,
            Err(e) => {
                eprintln!("[fc-agent] exec server: readable error: {}", e);
                continue;
            }
        };

        // Try to accept
        let client_fd = unsafe {
            libc::accept4(
                listener_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC, // Don't set NONBLOCK for client - we'll use blocking I/O
            )
        };

        if client_fd < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                // Spurious wakeup, clear readiness and try again
                guard.clear_ready();
                continue;
            }
            eprintln!("[fc-agent] exec server accept error: {}", err);
            continue;
        }

        // Handle the connection in spawn_blocking since we use blocking I/O
        tokio::task::spawn_blocking(move || {
            handle_exec_connection_blocking(client_fd);
        });
    }
}

/// Helper to write a line to the vsock fd
fn write_line_to_fd(fd: i32, data: &str) {
    let bytes = format!("{}\n", data);
    let mut written = 0;
    while written < bytes.len() {
        let n = unsafe {
            libc::write(
                fd,
                bytes[written..].as_ptr() as *const libc::c_void,
                bytes.len() - written,
            )
        };
        if n <= 0 {
            break;
        }
        written += n as usize;
    }
}

/// Blocking handler for exec connection
fn handle_exec_connection_blocking(fd: i32) {
    // Read request line using raw read syscall (File wrapper doesn't work well with vsock)
    let mut line = String::new();
    let mut buf = [0u8; 1];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 1) };
        if n <= 0 {
            unsafe { libc::close(fd) };
            return;
        }
        if buf[0] == b'\n' {
            break;
        }
        line.push(buf[0] as char);
    }

    // Parse the request
    let request: ExecRequest = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => {
            let response = ExecResponse::Error(format!("Invalid request: {}", e));
            write_line_to_fd(fd, &serde_json::to_string(&response).unwrap());
            unsafe { libc::close(fd) };
            return;
        }
    };

    if request.command.is_empty() {
        let response = ExecResponse::Error("Empty command".to_string());
        write_line_to_fd(fd, &serde_json::to_string(&response).unwrap());
        unsafe { libc::close(fd) };
        return;
    }

    // Use framed protocol for TTY or interactive modes
    // JSON line protocol only for plain non-interactive
    if request.tty || request.interactive {
        handle_exec_framed(fd, &request);
    } else {
        handle_exec_pipe(fd, &request);
    }
}

/// Handle exec with binary framing protocol
///
/// Uses the binary framing protocol (exec_proto) to cleanly separate
/// control messages (exit code) from raw terminal/pipe data.
///
/// - If `tty=true`: allocates PTY for terminal emulation
/// - If `tty=false`: uses pipes for stdin/stdout/stderr
fn handle_exec_framed(fd: i32, request: &ExecRequest) {
    use std::io::Read;
    use std::os::unix::io::FromRawFd;

    // Wrap vsock fd in File for clean I/O
    let mut vsock = unsafe { std::fs::File::from_raw_fd(fd) };

    // For TTY mode, allocate a PTY
    // For non-TTY, we'll use pipes (set up after fork)
    let (master_fd, slave_fd) = if request.tty {
        let mut master: libc::c_int = 0;
        let mut slave: libc::c_int = 0;

        let result = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };

        if result != 0 {
            let _ = exec_proto::write_error(&mut vsock, "Failed to allocate PTY");
            return;
        }
        (master, slave)
    } else {
        (-1, -1) // Will use pipes instead
    };

    // For non-TTY mode, create pipes
    let (stdin_read, stdin_write, stdout_read, stdout_write) = if !request.tty {
        let mut stdin_pipe = [0i32; 2];
        let mut stdout_pipe = [0i32; 2];
        unsafe {
            if libc::pipe(stdin_pipe.as_mut_ptr()) != 0 {
                let _ = exec_proto::write_error(&mut vsock, "Failed to create stdin pipe");
                return;
            }
            if libc::pipe(stdout_pipe.as_mut_ptr()) != 0 {
                libc::close(stdin_pipe[0]);
                libc::close(stdin_pipe[1]);
                let _ = exec_proto::write_error(&mut vsock, "Failed to create stdout pipe");
                return;
            }
        }
        (stdin_pipe[0], stdin_pipe[1], stdout_pipe[0], stdout_pipe[1])
    } else {
        (-1, -1, -1, -1)
    };

    // Fork
    let pid = unsafe { libc::fork() };

    if pid < 0 {
        let _ = exec_proto::write_error(&mut vsock, "Failed to fork");
        if request.tty {
            unsafe {
                libc::close(master_fd);
                libc::close(slave_fd);
            }
        } else {
            unsafe {
                libc::close(stdin_read);
                libc::close(stdin_write);
                libc::close(stdout_read);
                libc::close(stdout_write);
            }
        }
        return;
    }

    if pid == 0 {
        // Child process - don't let File destructor close fd
        std::mem::forget(vsock);

        if request.tty {
            unsafe {
                // Create new session and set controlling terminal
                libc::setsid();
                libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0);

                // Redirect stdin/stdout/stderr to PTY slave
                libc::dup2(slave_fd, 0);
                libc::dup2(slave_fd, 1);
                libc::dup2(slave_fd, 2);

                // Close fds we don't need
                if slave_fd > 2 {
                    libc::close(slave_fd);
                }
                libc::close(master_fd);
            }
        } else {
            unsafe {
                // Redirect stdin/stdout/stderr to pipes
                libc::dup2(stdin_read, 0);
                libc::dup2(stdout_write, 1);
                libc::dup2(stdout_write, 2); // stderr also to stdout pipe

                // Close unused pipe ends
                libc::close(stdin_read);
                libc::close(stdin_write);
                libc::close(stdout_read);
                libc::close(stdout_write);
            }
        }

        unsafe { libc::close(fd) };

        // Build and exec command
        if request.in_container {
            let mut args: Vec<std::ffi::CString> = vec![
                std::ffi::CString::new("podman").unwrap(),
                std::ffi::CString::new("exec").unwrap(),
            ];
            // Pass -i and -t separately, matching podman's semantics
            if request.interactive {
                args.push(std::ffi::CString::new("-i").unwrap());
            }
            if request.tty {
                args.push(std::ffi::CString::new("-t").unwrap());
            }
            args.push(std::ffi::CString::new("--latest").unwrap());
            for arg in &request.command {
                args.push(std::ffi::CString::new(arg.as_str()).unwrap());
            }
            let arg_ptrs: Vec<*const libc::c_char> = args
                .iter()
                .map(|s| s.as_ptr())
                .chain(std::iter::once(std::ptr::null()))
                .collect();
            unsafe {
                libc::execvp(args[0].as_ptr(), arg_ptrs.as_ptr());
            }
        } else {
            let prog = std::ffi::CString::new(request.command[0].as_str()).unwrap();
            let args: Vec<std::ffi::CString> = request
                .command
                .iter()
                .map(|s| std::ffi::CString::new(s.as_str()).unwrap())
                .collect();
            let arg_ptrs: Vec<*const libc::c_char> = args
                .iter()
                .map(|s| s.as_ptr())
                .chain(std::iter::once(std::ptr::null()))
                .collect();
            unsafe {
                libc::execvp(prog.as_ptr(), arg_ptrs.as_ptr());
            }
        }

        unsafe { libc::_exit(127) };
    }

    // Parent process - close child ends of pipes/pty
    if request.tty {
        unsafe { libc::close(slave_fd) };
    } else {
        unsafe {
            libc::close(stdin_read);
            libc::close(stdout_write);
        }
    }

    // Wrap master fd or stdout pipe in File for reading output
    let output_reader = if request.tty {
        unsafe { std::fs::File::from_raw_fd(master_fd) }
    } else {
        unsafe { std::fs::File::from_raw_fd(stdout_read) }
    };

    // For non-TTY, wrap stdin pipe for writing
    let stdin_writer = if !request.tty {
        Some(unsafe { std::fs::File::from_raw_fd(stdin_write) })
    } else {
        None
    };

    // Use output_reader as the "pty_master" equivalent
    let pty_master = output_reader;

    // Only forward stdin if interactive mode is enabled
    let writer_thread = if request.interactive {
        let vsock_for_writer = vsock.try_clone().expect("clone vsock");
        // For TTY mode, write to PTY; for non-TTY, write to stdin pipe
        let stdin_target: std::fs::File = if request.tty {
            pty_master.try_clone().expect("clone pty")
        } else {
            stdin_writer.expect("stdin_writer should exist for interactive non-TTY")
        };

        // Thread: read STDIN messages from vsock, write to PTY/stdin pipe
        Some(std::thread::spawn(move || {
            use std::io::Write;
            let mut vsock = vsock_for_writer;
            let mut target = stdin_target;

            loop {
                match exec_proto::Message::read_from(&mut vsock) {
                    Ok(exec_proto::Message::Stdin(data)) => {
                        if target.write_all(&data).is_err() {
                            break;
                        }
                        if target.flush().is_err() {
                            break;
                        }
                    }
                    Ok(exec_proto::Message::Exit(_)) | Ok(exec_proto::Message::Error(_)) => break,
                    Ok(_) => {} // Ignore unexpected message types
                    Err(_) => break,
                }
            }
            // Drop target to close stdin pipe, signaling EOF to child
            drop(target);
        }))
    } else {
        // Drop stdin_writer if not interactive (closes pipe)
        drop(stdin_writer);
        None
    };

    // Thread: read from PTY, write DATA messages to vsock
    let reader_thread = std::thread::spawn(move || {
        let mut vsock = vsock;
        let mut pty = pty_master;
        let mut buf = [0u8; 4096];

        loop {
            match pty.read(&mut buf) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    if exec_proto::write_data(&mut vsock, &buf[..n]).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }

        // Return vsock for exit message
        vsock
    });

    // Wait for child to exit
    let mut status: libc::c_int = 0;
    unsafe {
        libc::waitpid(pid, &mut status, 0);
    }

    let exit_code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        1
    };

    // Wait for reader (it will get EOF when PTY closes)
    let mut vsock = reader_thread.join().expect("reader thread");

    // Writer may be blocked on read - abort it by dropping
    drop(writer_thread);

    // Send exit code
    let _ = exec_proto::write_exit(&mut vsock, exit_code);
}

/// Handle exec in pipe mode (non-TTY)
fn handle_exec_pipe(fd: i32, request: &ExecRequest) {
    use std::io::{BufRead, BufReader};

    // Build the command using std::process::Command (blocking)
    let mut cmd = if request.in_container {
        // Execute inside the container using podman exec
        let mut cmd = std::process::Command::new("podman");
        cmd.arg("exec");
        // Pass -i flag if interactive mode requested
        if request.interactive {
            cmd.arg("-i");
        }
        // Use the first running container (there should only be one)
        cmd.arg("--latest");
        cmd.args(&request.command);
        cmd
    } else {
        // Execute directly in the VM
        let mut cmd = std::process::Command::new(&request.command[0]);
        if request.command.len() > 1 {
            cmd.args(&request.command[1..]);
        }
        cmd
    };

    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    // Spawn the command
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let response = ExecResponse::Error(format!("Failed to spawn command: {}", e));
            write_line_to_fd(fd, &serde_json::to_string(&response).unwrap());
            unsafe { libc::close(fd) };
            return;
        }
    };

    // Stream stdout and stderr
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // Use mutex to protect fd writes from multiple threads
    let fd_mutex = std::sync::Arc::new(std::sync::Mutex::new(fd));

    // Spawn threads to stream stdout and stderr
    let fd_stdout = fd_mutex.clone();
    let stdout_thread = std::thread::spawn(move || {
        if let Some(stdout) = stdout {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                let response = ExecResponse::Stdout(format!("{}\n", line));
                if let Ok(fd) = fd_stdout.lock() {
                    write_line_to_fd(*fd, &serde_json::to_string(&response).unwrap());
                }
            }
        }
    });

    let fd_stderr = fd_mutex.clone();
    let stderr_thread = std::thread::spawn(move || {
        if let Some(stderr) = stderr {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                let response = ExecResponse::Stderr(format!("{}\n", line));
                if let Ok(fd) = fd_stderr.lock() {
                    write_line_to_fd(*fd, &serde_json::to_string(&response).unwrap());
                }
            }
        }
    });

    // Wait for the command to complete
    let status = child.wait();
    let exit_code = status.map(|s| s.code().unwrap_or(1)).unwrap_or(1);

    // Wait for output threads to complete
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();

    // Send exit code
    let response = ExecResponse::Exit(exit_code);
    if let Ok(fd) = fd_mutex.lock() {
        write_line_to_fd(*fd, &serde_json::to_string(&response).unwrap());
    }

    // Close the fd
    unsafe { libc::close(fd) };
}

/// Create /dev/kvm device node for nested virtualization support.
/// This allows running Firecracker inside Firecracker (nested virtualization).
/// Requires kernel with CONFIG_KVM=y.
fn create_kvm_device() {
    use std::path::Path;

    let kvm_path = Path::new("/dev/kvm");
    if kvm_path.exists() {
        eprintln!("[fc-agent] /dev/kvm already exists");
        return;
    }

    // /dev/kvm is a character device with major 10, minor 232
    // (MISC_DYNAMIC_MINOR for kvm, but historically it's 232)
    // We use libc::mknod to create it
    let dev = libc::makedev(10, 232);
    let result = unsafe {
        libc::mknod(
            c"/dev/kvm".as_ptr(),
            libc::S_IFCHR | 0o666, // char device, rw-rw-rw-
            dev,
        )
    };

    if result == 0 {
        eprintln!("[fc-agent] ✓ created /dev/kvm (10:232)");
    } else {
        let err = std::io::Error::last_os_error();
        // ENOENT means the kernel doesn't have KVM support
        // This is expected with standard Firecracker kernel
        if err.kind() == std::io::ErrorKind::NotFound || err.raw_os_error() == Some(libc::ENOENT) {
            eprintln!("[fc-agent] /dev/kvm not available (kernel needs CONFIG_KVM)");
        } else {
            eprintln!("[fc-agent] WARNING: failed to create /dev/kvm: {}", err);
        }
    }
}

/// Raise resource limits for high parallelism workloads.
/// This prevents EMFILE (too many open files) errors when running
/// tests with many parallel jobs.
fn raise_resource_limits() {
    use libc::{rlimit, setrlimit, RLIMIT_NOFILE};

    // Target 65536 open files (default is often 1024)
    let new_limit = rlimit {
        rlim_cur: 65536,
        rlim_max: 65536,
    };

    let result = unsafe { setrlimit(RLIMIT_NOFILE, &new_limit) };
    if result == 0 {
        eprintln!("[fc-agent] ✓ raised RLIMIT_NOFILE to 65536");
    } else {
        eprintln!(
            "[fc-agent] WARNING: failed to raise RLIMIT_NOFILE: {}",
            std::io::Error::last_os_error()
        );
    }
}

/// Send a message to the host via vsock status channel.
///
/// Creates a vsock connection to the host on STATUS_VSOCK_PORT and sends the message.
/// Returns true if the message was sent successfully.
fn send_status_to_host(message: &[u8]) -> bool {
    // Create vsock socket
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        eprintln!(
            "[fc-agent] WARNING: failed to create vsock socket: {}",
            std::io::Error::last_os_error()
        );
        return false;
    }

    // Build sockaddr_vm structure
    let addr = libc::sockaddr_vm {
        svm_family: libc::AF_VSOCK as u16,
        svm_reserved1: 0,
        svm_port: STATUS_VSOCK_PORT,
        svm_cid: HOST_CID,
        svm_zero: [0u8; 4],
    };

    // Connect to host
    let result = unsafe {
        libc::connect(
            fd,
            &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_vm>() as u32,
        )
    };

    if result < 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        eprintln!("[fc-agent] WARNING: failed to connect vsock: {}", err);
        return false;
    }

    // Send message
    let written =
        unsafe { libc::write(fd, message.as_ptr() as *const libc::c_void, message.len()) };
    unsafe { libc::close(fd) };

    written == message.len() as isize
}

/// Create a vsock connection to host for container output streaming.
/// Returns the file descriptor if successful, or -1 on failure.
fn create_output_vsock() -> i32 {
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        eprintln!(
            "[fc-agent] WARNING: failed to create output vsock socket: {}",
            std::io::Error::last_os_error()
        );
        return -1;
    }

    let addr = libc::sockaddr_vm {
        svm_family: libc::AF_VSOCK as u16,
        svm_reserved1: 0,
        svm_port: OUTPUT_VSOCK_PORT,
        svm_cid: HOST_CID,
        svm_zero: [0u8; 4],
    };

    let result = unsafe {
        libc::connect(
            fd,
            &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_vm>() as u32,
        )
    };

    if result < 0 {
        eprintln!(
            "[fc-agent] WARNING: failed to connect output vsock: {}",
            std::io::Error::last_os_error()
        );
        unsafe { libc::close(fd) };
        return -1;
    }

    fd
}

/// Send a line of container output to host via vsock.
/// Format: stdout:line or stderr:line (raw, no JSON)
fn send_output_line(fd: i32, stream: &str, line: &str) {
    if fd < 0 {
        return;
    }
    // Raw format: stream:line\n
    let data = format!("{}:{}\n", stream, line);
    unsafe {
        libc::write(fd, data.as_ptr() as *const libc::c_void, data.len());
    }
}

/// Notify host of container exit status via vsock.
///
/// Sends "exit:{code}\n" message to the host on the status vsock port.
/// The host side can use this to determine if the container succeeded or failed.
fn notify_container_exit(exit_code: i32) {
    let msg = format!("exit:{}\n", exit_code);
    if send_status_to_host(msg.as_bytes()) {
        eprintln!(
            "[fc-agent] ✓ notified host of exit code {} via vsock",
            exit_code
        );
    } else {
        eprintln!("[fc-agent] WARNING: failed to send exit status to host");
    }
}

/// Notify host that container has started via vsock.
///
/// Sends "ready\n" message to the host on the status vsock port.
/// The host side listens on vsock.sock_4999 and uses this to determine
/// when the container is running for health checks.
fn notify_container_started() {
    if send_status_to_host(b"ready\n") {
        eprintln!("[fc-agent] ✓ container started, notified host via vsock");
    } else {
        eprintln!("[fc-agent] WARNING: failed to send ready status to host");
    }
}

/// Extract clone ID from MMDS or hostname
/// Clones are named "clone-lock-{N}" so we extract the number
async fn get_clone_id() -> String {
    // Try to get from hostname first
    if let Ok(output) = Command::new("hostname").output().await {
        let hostname = String::from_utf8_lossy(&output.stdout).trim().to_string();
        // Clone VMs have names like "clone-lock-0", extract just the number
        if hostname.starts_with("clone-lock-") {
            if let Some(id) = hostname.strip_prefix("clone-lock-") {
                return id.to_string();
            }
        }
        // Return hostname if it looks like a clone ID
        if hostname.chars().all(|c| c.is_ascii_digit()) {
            return hostname;
        }
    }

    // Fallback: use process ID as clone ID (unique per VM)
    std::process::id().to_string()
}

/// Mount FUSE volumes from host via vsock.
/// Returns list of mount points that need to be cleaned up on exit.
fn mount_fuse_volumes(volumes: &[VolumeMount]) -> Result<Vec<String>> {
    let mut mounted_paths = Vec::new();

    for vol in volumes {
        eprintln!(
            "[fc-agent] mounting FUSE volume at {} via vsock port {}",
            vol.guest_path, vol.vsock_port
        );

        // Try to unmount any stale FUSE mount from a previous failed attempt
        // This handles the case where fc-agent was restarted by systemd after a failure
        let mount_path = std::path::Path::new(&vol.guest_path);
        if mount_path.exists() {
            eprintln!("[fc-agent] mount point exists, attempting to unmount stale mount...");
            // Use lazy unmount (MNT_DETACH) to handle stale FUSE mounts
            let _ = std::process::Command::new("umount")
                .arg("-l")
                .arg(&vol.guest_path)
                .output();
        }

        // Create mount point directory (ok if it already exists)
        if let Err(e) = std::fs::create_dir_all(&vol.guest_path) {
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(e).with_context(|| format!("creating mount point: {}", vol.guest_path));
            }
        }

        // Mount FUSE filesystem in a background thread using fuse-pipe
        // fuse-pipe's mount_vsock blocks, so we run it in a dedicated thread
        let mount_path = vol.guest_path.clone();
        let port = vol.vsock_port;

        thread::spawn(move || {
            eprintln!("[fc-agent] fuse: starting mount at {}", mount_path);
            if let Err(e) = fuse::mount_vsock(port, &mount_path) {
                eprintln!("[fc-agent] FUSE mount error at {}: {}", mount_path, e);
            }
            eprintln!("[fc-agent] fuse: mount at {} exited", mount_path);
        });

        mounted_paths.push(vol.guest_path.clone());
    }

    // Give FUSE mounts time to initialize
    if !volumes.is_empty() {
        eprintln!("[fc-agent] waiting for FUSE mounts to initialize...");
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Verify each mount point is accessible
        for vol in volumes {
            let path = std::path::Path::new(&vol.guest_path);
            if let Ok(entries) = std::fs::read_dir(path) {
                let count = entries.count();
                eprintln!(
                    "[fc-agent] ✓ mount {} accessible ({} entries)",
                    vol.guest_path, count
                );
            } else {
                eprintln!("[fc-agent] ✗ mount {} NOT accessible", vol.guest_path);
            }
        }
    }

    Ok(mounted_paths)
}

/// Sync VM clock from host time provided via MMDS
/// This avoids the need to wait for slow NTP synchronization
async fn sync_clock_from_host() -> Result<()> {
    eprintln!("[fc-agent] syncing VM clock from host time via MMDS");

    let client = reqwest::Client::new();

    // Get session token
    let token_response = client
        .put("http://169.254.169.254/latest/api/token")
        .header("X-metadata-token-ttl-seconds", "21600")
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .context("getting MMDS token for time sync")?;

    let token = token_response.text().await?;

    // Fetch host-time from /latest
    let metadata_response = client
        .get("http://169.254.169.254/latest")
        .header("X-metadata-token", &token)
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .context("fetching host-time from MMDS")?;

    let body = metadata_response.text().await?;
    let metadata: LatestMetadata =
        serde_json::from_str(&body).context("parsing host-time from MMDS")?;

    eprintln!("[fc-agent] received host time: {}", metadata.host_time);

    // Set system clock using `date` command with Unix timestamp
    // Format: @1731301800 (seconds since epoch)
    // BusyBox date supports this with -s @TIMESTAMP
    let output = Command::new("date")
        .arg("-u")
        .arg("-s")
        .arg(format!("@{}", metadata.host_time))
        .output()
        .await
        .context("setting system clock")?;

    if !output.status.success() {
        eprintln!(
            "[fc-agent] WARNING: failed to set clock: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        eprintln!("[fc-agent] continuing anyway (will rely on chronyd)");
    } else {
        eprintln!("[fc-agent] ✓ system clock synchronized from host");
    }

    Ok(())
}

/// Configure DNS from kernel boot parameters
/// Parses ip= parameter to extract DNS server and writes to /etc/resolv.conf
fn configure_dns_from_cmdline() {
    eprintln!("[fc-agent] configuring DNS from kernel cmdline");

    // Read kernel command line
    let cmdline = match std::fs::read_to_string("/proc/cmdline") {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[fc-agent] WARNING: failed to read /proc/cmdline: {}", e);
            return;
        }
    };
    eprintln!("[fc-agent] cmdline: {}", cmdline.trim());

    // Find ip= parameter by searching for "ip=" and extracting until whitespace
    // Format: ip=<client>::<gateway>:<netmask>::eth0:off[:<dns>]
    let ip_param = cmdline
        .split_whitespace()
        .find(|s| s.starts_with("ip="))
        .map(|s| s.trim_start_matches("ip="));

    let ip_param = match ip_param {
        Some(p) => p,
        None => {
            eprintln!("[fc-agent] WARNING: no ip= parameter in cmdline, skipping DNS config");
            return;
        }
    };
    eprintln!("[fc-agent] ip param: {}", ip_param);

    // Split by colons
    let fields: Vec<&str> = ip_param.split(':').collect();
    eprintln!("[fc-agent] ip fields: {:?}", fields);

    // Field 3 is gateway (0-indexed field 2)
    // Field 8 is DNS (0-indexed field 7)
    let gateway = fields.get(2).copied().unwrap_or("");
    let dns = fields.get(7).copied().unwrap_or("");

    eprintln!("[fc-agent] gateway={}, dns={}", gateway, dns);

    let nameserver = if !dns.is_empty() {
        dns
    } else if !gateway.is_empty() {
        gateway
    } else {
        eprintln!("[fc-agent] WARNING: no DNS or gateway found, skipping DNS config");
        return;
    };

    // Write to /etc/resolv.conf
    let resolv_conf = format!("nameserver {}\n", nameserver);
    match std::fs::write("/etc/resolv.conf", &resolv_conf) {
        Ok(_) => {
            eprintln!("[fc-agent] ✓ configured DNS: nameserver {}", nameserver);
        }
        Err(e) => {
            eprintln!(
                "[fc-agent] WARNING: failed to write /etc/resolv.conf: {}",
                e
            );
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing (fuse-pipe uses tracing for logging)
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,fuse_pipe=debug")),
        )
        .with_target(true)
        .with_writer(std::io::stderr)
        .init();

    eprintln!("[fc-agent] starting");

    // Raise resource limits early to support high parallelism workloads
    raise_resource_limits();

    // Create /dev/kvm device for nested virtualization support (nested virtualization)
    // This is a no-op if kernel doesn't have CONFIG_KVM
    create_kvm_device();

    // Configure DNS from kernel boot parameters before any network operations
    configure_dns_from_cmdline();

    // Wait for MMDS to be ready
    let plan = loop {
        match fetch_plan().await {
            Ok(p) => {
                eprintln!("[fc-agent] ✓ received container plan successfully");
                break p;
            }
            Err(e) => {
                eprintln!("[fc-agent] MMDS not ready - full error chain:");
                eprintln!("[fc-agent]   {:?}", e);
                eprintln!("[fc-agent] retrying in 500ms...");
                sleep(Duration::from_millis(500)).await;
            }
        }
    };

    // Sync VM clock from host before launching container
    // This ensures TLS certificate validation works immediately
    if let Err(e) = sync_clock_from_host().await {
        eprintln!("[fc-agent] WARNING: clock sync failed: {:?}", e);
        eprintln!("[fc-agent] continuing anyway (will rely on chronyd)");
    }

    // Start background task to watch for restore-epoch changes
    // This handles ARP cache flushing when VM is restored from snapshot
    tokio::spawn(async {
        eprintln!("[fc-agent] starting restore-epoch watcher for ARP flush");
        watch_restore_epoch().await;
    });

    // Start exec server to allow host to run commands in VM
    tokio::spawn(async {
        run_exec_server().await;
    });

    // Mount FUSE volumes from host before launching container
    // Note: mounted_volumes tracks which mounts succeeded, but we bind from plan.volumes
    // since they use the same guest_path for both FUSE mount and container bind
    let mounted_fuse_paths: Vec<String> = if !plan.volumes.is_empty() {
        eprintln!(
            "[fc-agent] mounting {} FUSE volume(s) from host",
            plan.volumes.len()
        );
        match mount_fuse_volumes(&plan.volumes) {
            Ok(paths) => {
                eprintln!("[fc-agent] ✓ FUSE volumes mounted successfully");
                paths
            }
            Err(e) => {
                eprintln!("[fc-agent] ERROR: failed to mount FUSE volumes: {:?}", e);
                // Continue without volumes - container can still run
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let has_shared_volume = mounted_fuse_paths.iter().any(|p| p == "/mnt/shared");

    // If we have a shared volume, start lock test watcher
    // This allows clones to run POSIX lock tests on demand
    if has_shared_volume {
        let clone_id = get_clone_id().await;
        eprintln!(
            "[fc-agent] starting lock test watcher (clone_id={})",
            clone_id
        );
        tokio::spawn(async move {
            watch_for_lock_test(clone_id).await;
        });
    }

    // Determine the image reference for podman run
    // If image_archive is set, we run directly from the OCI archive (no import needed)
    // Otherwise, pull from registry
    let image_ref = if let Some(archive_path) = &plan.image_archive {
        eprintln!("[fc-agent] using OCI archive: {}", archive_path);

        format!("oci-archive:{}", archive_path)
    } else {
        // Pull image with retries to handle transient DNS/network errors
        const MAX_RETRIES: u32 = 3;
        const RETRY_DELAY_SECS: u64 = 2;

        let mut last_error = String::new();
        let mut pull_succeeded = false;

        for attempt in 1..=MAX_RETRIES {
            eprintln!("[fc-agent] ==========================================");
            eprintln!(
                "[fc-agent] PULLING IMAGE: {} (attempt {}/{})",
                plan.image, attempt, MAX_RETRIES
            );
            eprintln!("[fc-agent] ==========================================");

            // Spawn podman pull and stream output in real-time
            let mut child = Command::new("podman")
                .arg("pull")
                .arg(&plan.image)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .context("spawning podman pull")?;

            // Stream stdout in real-time
            let stdout_task = child.stdout.take().map(|stdout| {
                tokio::spawn(async move {
                    let reader = BufReader::new(stdout);
                    let mut lines = reader.lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        eprintln!("[fc-agent] [podman] {}", line);
                    }
                })
            });

            // Stream stderr in real-time and capture for error reporting
            let stderr_task = child.stderr.take().map(|stderr| {
                tokio::spawn(async move {
                    let reader = BufReader::new(stderr);
                    let mut lines = reader.lines();
                    let mut captured = Vec::new();
                    while let Ok(Some(line)) = lines.next_line().await {
                        eprintln!("[fc-agent] [podman] {}", line);
                        captured.push(line);
                    }
                    captured
                })
            });

            // Wait for podman to finish
            let status = child.wait().await.context("waiting for podman pull")?;

            // Wait for output streaming to complete
            if let Some(task) = stdout_task {
                let _ = task.await;
            }
            let stderr_lines = if let Some(task) = stderr_task {
                task.await.unwrap_or_default()
            } else {
                Vec::new()
            };

            if status.success() {
                eprintln!("[fc-agent] ✓ image pulled successfully");
                pull_succeeded = true;
                break;
            }

            // Capture error for final bail message
            last_error = stderr_lines.join("\n");
            eprintln!("[fc-agent] ==========================================");
            eprintln!(
                "[fc-agent] IMAGE PULL FAILED (attempt {}/{})",
                attempt, MAX_RETRIES
            );
            eprintln!("[fc-agent] exit code: {:?}", status.code());
            eprintln!("[fc-agent] ==========================================");

            if attempt < MAX_RETRIES {
                eprintln!("[fc-agent] retrying in {} seconds...", RETRY_DELAY_SECS);
                tokio::time::sleep(std::time::Duration::from_secs(RETRY_DELAY_SECS)).await;
            }
        }

        if !pull_succeeded {
            eprintln!("[fc-agent] ==========================================");
            eprintln!(
                "[fc-agent] FATAL: IMAGE PULL FAILED AFTER {} ATTEMPTS",
                MAX_RETRIES
            );
            eprintln!("[fc-agent] ==========================================");
            anyhow::bail!(
                "Failed to pull image after {} attempts:\n{}",
                MAX_RETRIES,
                last_error
            );
        }

        // Return the image name for podman run
        plan.image.clone()
    };

    eprintln!("[fc-agent] launching container: {}", image_ref);

    // Build Podman command
    let mut cmd = Command::new("podman");
    cmd.arg("run")
        .arg("--rm")
        .arg("--network=host")
        // Raise ulimit for containers running parallel tests
        .arg("--ulimit")
        .arg("nofile=65536:65536");

    // Privileged mode: allows mknod, device access, etc. for POSIX compliance tests
    if plan.privileged {
        eprintln!("[fc-agent] privileged mode enabled");
        cmd.arg("--device-cgroup-rule=b *:* rwm") // Allow block device nodes
            .arg("--device-cgroup-rule=c *:* rwm") // Allow char device nodes
            .arg("--privileged");
    }

    // Add environment variables
    for (key, val) in &plan.env {
        cmd.arg("-e").arg(format!("{}={}", key, val));
    }

    // Add FUSE-mounted volumes as bind mounts to container
    // The FUSE mount is already at guest_path, so we bind it to same path in container
    for vol in &plan.volumes {
        let mount_spec = if vol.read_only {
            format!("{}:{}:ro", vol.guest_path, vol.guest_path)
        } else {
            format!("{}:{}", vol.guest_path, vol.guest_path)
        };
        cmd.arg("-v").arg(mount_spec);
    }

    // Image (either oci-archive:/path or image name from registry)
    cmd.arg(&image_ref);

    // Command override
    if let Some(cmd_args) = &plan.cmd {
        cmd.args(cmd_args);
    }

    // Spawn container with piped stdin/stdout/stderr for bidirectional I/O
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().context("spawning Podman container")?;

    // Notify host that container has started via vsock
    // The host listens on vsock.sock_4999 for status messages
    notify_container_started();

    // Create vsock connection for container output streaming
    // Port 4997 is dedicated for stdout/stderr
    let output_fd = create_output_vsock();
    if output_fd >= 0 {
        eprintln!(
            "[fc-agent] output vsock connected (port {})",
            OUTPUT_VSOCK_PORT
        );
    }

    // Stream stdout via vsock (wrapped in Arc for sharing across tasks)
    let output_fd_arc = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(output_fd));
    let stdout_task = if let Some(stdout) = child.stdout.take() {
        let fd = output_fd_arc.clone();
        Some(tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                send_output_line(
                    fd.load(std::sync::atomic::Ordering::Relaxed),
                    "stdout",
                    &line,
                );
            }
        }))
    } else {
        None
    };

    // Stream stderr via vsock
    let stderr_task = if let Some(stderr) = child.stderr.take() {
        let fd = output_fd_arc.clone();
        Some(tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                send_output_line(
                    fd.load(std::sync::atomic::Ordering::Relaxed),
                    "stderr",
                    &line,
                );
            }
        }))
    } else {
        None
    };

    // Read stdin from vsock and forward to container (bidirectional I/O)
    let stdin_task = if output_fd >= 0 {
        if let Some(mut stdin) = child.stdin.take() {
            // Duplicate the fd for reading (original used for writing)
            let read_fd = unsafe { libc::dup(output_fd) };
            if read_fd >= 0 {
                Some(tokio::spawn(async move {
                    use std::os::unix::io::FromRawFd;
                    use tokio::io::AsyncWriteExt;
                    // Convert to async file for reading
                    let file = unsafe { std::fs::File::from_raw_fd(read_fd) };
                    let file = tokio::fs::File::from_std(file);
                    let reader = BufReader::new(file);
                    let mut lines = reader.lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        // Parse stdin:content format
                        if let Some(content) = line.strip_prefix("stdin:") {
                            // Write to container stdin
                            if stdin.write_all(content.as_bytes()).await.is_err() {
                                break;
                            }
                            if stdin.write_all(b"\n").await.is_err() {
                                break;
                            }
                        }
                    }
                }))
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    // Wait for container to exit
    let status = child.wait().await?;
    let exit_code = status.code().unwrap_or(1);

    // Abort stdin task (container exited, no more input needed)
    if let Some(task) = stdin_task {
        task.abort();
    }

    // Wait for output streams to complete before closing vsock
    if let Some(task) = stdout_task {
        let _ = task.await;
    }
    if let Some(task) = stderr_task {
        let _ = task.await;
    }

    // Close output vsock
    if output_fd >= 0 {
        unsafe { libc::close(output_fd) };
    }

    if status.success() {
        eprintln!("[fc-agent] container exited successfully");
    } else {
        eprintln!(
            "[fc-agent] container exited with error: {} (code {})",
            status, exit_code
        );
    }

    // Notify host of container exit status via vsock
    // The host can use this to determine if the container succeeded
    notify_container_exit(exit_code);

    // Unmount FUSE volumes before shutting down
    // This prevents poweroff from hanging on busy FUSE mounts
    if !mounted_fuse_paths.is_empty() {
        eprintln!(
            "[fc-agent] unmounting {} FUSE volume(s) before shutdown",
            mounted_fuse_paths.len()
        );
        for path in &mounted_fuse_paths {
            eprintln!("[fc-agent] unmounting FUSE volume at {}", path);
            // Use lazy unmount (-l) to detach immediately even if busy
            // This allows the FUSE threads to exit cleanly
            match std::process::Command::new("umount")
                .arg("-l")
                .arg(path)
                .output()
            {
                Ok(output) => {
                    if output.status.success() {
                        eprintln!("[fc-agent] ✓ unmounted {}", path);
                    } else {
                        eprintln!(
                            "[fc-agent] umount {} failed: {}",
                            path,
                            String::from_utf8_lossy(&output.stderr)
                        );
                    }
                }
                Err(e) => {
                    eprintln!("[fc-agent] umount {} error: {}", path, e);
                }
            }
        }
        // Give FUSE threads time to notice the unmount and exit
        sleep(Duration::from_millis(100)).await;
    }

    // Shut down the VM when the container exits (success or failure)
    // This is the expected behavior - the VM exists to run one container
    eprintln!("[fc-agent] shutting down VM");
    let _ = Command::new("poweroff").spawn();

    // Give poweroff a moment to execute, then exit with container's exit code
    sleep(Duration::from_millis(100)).await;
    std::process::exit(exit_code)
}
