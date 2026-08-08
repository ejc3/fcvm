use anyhow::{anyhow, bail, Context, Result};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::utils::{spawn_streaming, strip_firecracker_prefix};

use super::FirecrackerClient;

/// Socket/device wait timeout (total wait time = RETRY_COUNT * RETRY_DELAY)
const SOCKET_WAIT_RETRY_COUNT: u32 = 500;
const SOCKET_WAIT_RETRY_DELAY: Duration = Duration::from_millis(10);
/// Total budget for the API socket to accept connections (matches the old
/// 500 × 10ms retry ladder).
const SOCKET_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Number of recent Firecracker stderr lines kept for error reporting.
const STDERR_TAIL_LINES: usize = 10;
/// How long to let the async stderr reader drain the pipe after Firecracker
/// exits, so the failure error can include what it actually printed.
const STDERR_DRAIN_DELAY: Duration = Duration::from_millis(200);

/// Manages a Firecracker VM process
///
/// IMPORTANT: PID Tracking Architecture
/// -------------------------------------
/// fcvm tracks its OWN process ID (via std::process::id()), not the Firecracker child process ID.
///
/// When `fcvm podman run` or `fcvm snapshot run` is executed, the fcvm process itself
/// stays running to manage the VM lifecycle. The PID stored in VmState is the fcvm
/// process PID, which allows:
/// - External tools to send signals to the correct process
/// - Health monitors to verify the manager process is still running
/// - Tests to track spawned fcvm processes without parsing stdout
///
/// The Firecracker process is a child of fcvm and is managed via the Child handle
/// stored in self.process. When fcvm exits (via signal or normally), it ensures
/// the Firecracker child process is also terminated.
pub struct VmManager {
    vm_id: String,
    vm_name: Option<String>,
    socket_path: PathBuf,
    log_path: Option<PathBuf>,
    namespace_id: Option<String>,
    holder_pid: Option<u32>, // namespace holder PID for rootless mode (use nsenter to run FC)
    user_namespace_path: Option<PathBuf>, // User namespace path for rootless clones (enter via setns in pre_exec)
    net_namespace_path: Option<PathBuf>, // Net namespace path for rootless clones (enter via setns in pre_exec)
    mount_redirects: Option<(Vec<PathBuf>, PathBuf)>, // (baseline_dirs, clone_dir) for mount namespace isolation
    process: Option<Child>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>, // last few Firecracker stderr lines, for launch failure errors
    /// Guest serial console lines observed on the Firecracker child's stdout
    /// since spawn. Watched by the restore path to detect a dead serial
    /// console (snapshot captured the UART mid-transmit).
    console_lines: Arc<std::sync::atomic::AtomicU64>,
    client: Option<FirecrackerClient>,
}

impl VmManager {
    pub fn new(vm_id: String, socket_path: PathBuf, log_path: Option<PathBuf>) -> Self {
        Self {
            vm_id,
            vm_name: None,
            socket_path,
            log_path,
            namespace_id: None,
            holder_pid: None,
            user_namespace_path: None,
            net_namespace_path: None,
            mount_redirects: None,
            process: None,
            stderr_tail: Arc::new(Mutex::new(VecDeque::new())),
            console_lines: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            client: None,
        }
    }

    /// Counter of guest serial console lines seen since the Firecracker child
    /// spawned (see [`crate::hypervisor::Hypervisor::console_line_counter`]).
    pub fn console_line_counter(&self) -> Arc<std::sync::atomic::AtomicU64> {
        Arc::clone(&self.console_lines)
    }

    /// Set the VM name for logging purposes
    pub fn set_vm_name(&mut self, vm_name: String) {
        self.vm_name = Some(vm_name);
    }

    /// Set the network namespace for this VM
    ///
    /// When set, Firecracker will be launched inside the specified network namespace
    /// using setns() before exec. This isolates the VM's network stack.
    pub fn set_namespace(&mut self, namespace_id: String) {
        self.namespace_id = Some(namespace_id);
    }

    /// Set namespace holder PID for rootless networking
    ///
    /// When set, Firecracker will be launched inside an existing user+net namespace
    /// via nsenter. The holder process (created by `unshare --user --net -- sleep infinity`)
    /// keeps the namespace alive while Firecracker runs.
    ///
    /// UID/GID mappings are written externally by `setup_namespace_mappings()`, which
    /// maps the current user (UID 1000) to UID 0 inside the namespace. Combined with
    /// `--preserve-credentials` when using nsenter, no chmod is needed on sockets or
    /// files - the user has access both inside and outside.
    pub fn set_holder_pid(&mut self, pid: u32) {
        self.holder_pid = Some(pid);
    }

    /// Set user namespace path for rootless clones
    ///
    /// When set along with mount_redirects, pre_exec will enter this user namespace
    /// first (via setns) before doing mount operations. This gives CAP_SYS_ADMIN
    /// inside the user namespace, allowing unshare(CLONE_NEWNS) to succeed.
    ///
    /// Use this instead of set_holder_pid when mount namespace isolation is needed,
    /// since nsenter wrapper runs AFTER pre_exec.
    pub fn set_user_namespace_path(&mut self, path: PathBuf) {
        self.user_namespace_path = Some(path);
    }

    /// Set network namespace path for rootless clones
    ///
    /// When set, pre_exec will enter this network namespace (via setns) after
    /// completing mount operations. Use with set_user_namespace_path for
    /// rootless clones that need mount namespace isolation.
    pub fn set_net_namespace_path(&mut self, path: PathBuf) {
        self.net_namespace_path = Some(path);
    }

    /// Set mount redirects for mount namespace isolation
    ///
    /// When set, Firecracker will be launched in a new mount namespace with
    /// clone_dir bind-mounted over each baseline_dir. This allows multiple clones
    /// from the same snapshot to each have their own vsock socket and disk file bindings,
    /// even though vmstate.bin stores the baseline's paths.
    ///
    /// Multiple baseline_dirs are needed because:
    /// - Vsock paths in vmstate.bin reference the original cache VM's directory
    /// - Disk paths in vmstate.bin reference the snapshotted VM's directory (after patch_drive)
    ///
    /// The bind mount makes Firecracker see clone's directory contents when
    /// accessing any baseline's path, so each clone binds to its own socket files.
    pub fn set_mount_redirects(&mut self, baseline_dirs: Vec<PathBuf>, clone_dir: PathBuf) {
        self.mount_redirects = Some((baseline_dirs, clone_dir));
    }

    /// Start the Firecracker process
    ///
    /// `firecracker_args` provides extra CLI arguments (e.g., "--enable-nv2") passed
    /// explicitly from RuntimeConfig instead of reading the FCVM_FIRECRACKER_ARGS env var.
    pub async fn start(
        &mut self,
        firecracker_bin: &Path,
        config_override: Option<&Path>,
        firecracker_args: Option<&str>,
    ) -> Result<()> {
        if let Some(ref name) = self.vm_name {
            info!(target: "vm", vm_name = %name, vm_id = %self.vm_id, "starting Firecracker process");
        } else {
            info!(target: "vm", vm_id = %self.vm_id, "starting Firecracker process");
        }

        // Remove existing socket (ignore errors if not exists - avoids TOCTOU race)
        let _ = std::fs::remove_file(&self.socket_path);

        // Build command based on mode:
        // 1. user_namespace_path set: direct Firecracker (namespaces entered via pre_exec setns)
        // 2. holder_pid set (no user_namespace_path): use nsenter to enter existing namespace (rootless baseline)
        // 3. neither: direct Firecracker (privileged/bridged mode)
        //
        // For rootless clones with mount_redirects, we MUST use pre_exec setns instead of nsenter,
        // because pre_exec runs BEFORE nsenter would enter the namespace, and we need CAP_SYS_ADMIN
        // from the user namespace to do mount operations.
        let mut cmd = if self.user_namespace_path.is_some() {
            // Use direct Firecracker - namespaces will be entered via setns in pre_exec.
            // Used for ALL rootless VMs (baselines + clones) because nsenter's internal
            // setns(CLONE_NEWUSER) clears PR_SET_PDEATHSIG, leaving Firecracker orphaned
            // if fcvm is SIGKILL'd. The pre_exec path sets pdeathsig AFTER setns.
            info!(target: "vm", vm_id = %self.vm_id, "using pre_exec setns for rootless VM");
            let mut c = Command::new(firecracker_bin);
            c.arg("--api-sock").arg(&self.socket_path);
            c
        } else if let Some(holder_pid) = self.holder_pid {
            // Fallback: nsenter to enter user+network namespace.
            // NOTE: This path is not normally reached — rootless VMs now use pre_exec
            // setns (above) which correctly preserves PR_SET_PDEATHSIG. nsenter's
            // internal setns(CLONE_NEWUSER) clears pdeathsig. Kept as safety net.
            info!(target: "vm", vm_id = %self.vm_id, holder_pid = holder_pid, "using nsenter for rootless networking");
            let mut c = Command::new("nsenter");
            c.args([
                "-t",
                &holder_pid.to_string(),
                "-U",
                "-n",
                "--preserve-credentials",
                "--",
            ]);
            c.arg(firecracker_bin)
                .arg("--api-sock")
                .arg(&self.socket_path);
            c
        } else {
            // Direct Firecracker invocation (privileged mode)
            let mut c = Command::new(firecracker_bin);
            c.arg("--api-sock").arg(&self.socket_path);
            c
        };

        if let Some(config) = config_override {
            cmd.arg("--config-file").arg(config);
        }

        // Setup logging
        if let Some(log_path) = &self.log_path {
            cmd.arg("--log-path").arg(log_path);
            cmd.arg("--level").arg("Debug"); // Enable Debug logging for detailed diagnostics
            cmd.arg("--show-level");
            cmd.arg("--show-log-origin");
        }

        // Seccomp is enabled by default (Firecracker's security boundary).
        // Set FCVM_NO_SECCOMP=1 to disable for debugging.
        if std::env::var("FCVM_NO_SECCOMP").is_ok() {
            warn!("Seccomp disabled via FCVM_NO_SECCOMP - reduced security");
            cmd.arg("--no-seccomp");
        }

        // Additional firecracker args from RuntimeConfig (passed explicitly by caller)
        if let Some(extra) = firecracker_args {
            for arg in extra.split_whitespace() {
                cmd.arg(arg);
            }
        }

        // FCVM_STRACE_FC: wrap Firecracker with strace for debugging.
        // Output goes to /tmp/fcvm-strace-fc-{vm_id}.log on the host.
        // Traces ioctl, mmap, and signal syscalls by default.
        // Set FCVM_STRACE_FC=all for full trace.
        if let Ok(trace_filter) = std::env::var("FCVM_STRACE_FC") {
            let strace_log = format!("/tmp/fcvm-strace-fc-{}.log", &self.vm_id);
            let filter = if trace_filter == "all" {
                String::new()
            } else if trace_filter.contains('=') || trace_filter.contains(',') {
                // Custom filter like "ioctl,mmap" or "trace=ioctl"
                format!("-e {}", trace_filter)
            } else {
                "-e trace=ioctl,mmap,madvise,mprotect".to_string()
            };
            warn!(
                strace_log = %strace_log,
                filter = %filter,
                "FCVM_STRACE_FC: wrapping Firecracker with strace"
            );
            // Prepend strace to the command
            let program = cmd.as_std().get_program().to_owned();
            let args: Vec<_> = cmd.as_std().get_args().map(|a| a.to_owned()).collect();
            cmd = Command::new("strace");
            cmd.arg("-f").arg("-tt").arg("-o").arg(&strace_log);
            if !filter.is_empty() {
                for part in filter.split_whitespace() {
                    cmd.arg(part);
                }
            }
            cmd.arg(program);
            for arg in args {
                cmd.arg(arg);
            }
        }

        // Apply namespace/mount isolation + pdeathsig (shared with the Cloud Hypervisor
        // backend so both VMMs get identical isolation + parent-death behavior).
        crate::utils::install_namespace_pre_exec(
            &mut cmd,
            &crate::utils::NamespaceParams {
                vm_id: self.vm_id.clone(),
                namespace_id: self.namespace_id.clone(),
                user_namespace_path: self.user_namespace_path.clone(),
                net_namespace_path: self.net_namespace_path.clone(),
                mount_redirects: self.mount_redirects.clone(),
            },
        )?;

        // Spawn process with streaming output
        let stderr_tail = Arc::clone(&self.stderr_tail);
        let console_lines = Arc::clone(&self.console_lines);
        let child = spawn_streaming(cmd, move |line, is_stderr| {
            let clean = strip_firecracker_prefix(line);
            // fc-agent and container output at INFO/WARN, everything else at DEBUG
            let is_important = clean.contains("fc-agent") || clean.contains("[ctr:");
            if !is_stderr {
                // Firecracker writes the guest serial console to its stdout
                // (its own logs go to --log-path), so every stdout line is
                // guest console output.
                console_lines.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            if is_stderr {
                // Keep the last few stderr lines so launch failures can report
                // what Firecracker actually printed (its errors only appear at
                // DEBUG level in the normal log stream).
                if let Ok(mut tail) = stderr_tail.lock() {
                    if tail.len() >= STDERR_TAIL_LINES {
                        tail.pop_front();
                    }
                    tail.push_back(clean.to_string());
                }
                if is_important {
                    warn!(target: "firecracker", "{}", clean);
                } else {
                    debug!(target: "firecracker", "{}", clean);
                }
            } else if is_important {
                info!(target: "firecracker", "{}", clean);
            } else {
                debug!(target: "firecracker", "{}", clean);
            }
        })
        .context("spawning Firecracker process")?;

        self.process = Some(child);

        // Wait for socket to be ready
        self.wait_for_socket().await?;

        // Create API client
        self.client = Some(FirecrackerClient::new(self.socket_path.clone())?);

        Ok(())
    }

    /// Wait for the Firecracker API server to accept connections
    ///
    /// The socket file becoming visible is not enough: there is a window between
    /// Firecracker's bind() (file exists) and listen() where connect() fails with
    /// ECONNREFUSED. Probe by actually connecting so the first real API request
    /// never races that window.
    ///
    /// Event-driven: an inotify watch on the socket's parent directory (registered
    /// BEFORE the first probe, so a file appearing mid-check always produces an
    /// event) wakes the probe the instant Firecracker binds — the old fixed 10ms
    /// poll cost most of that interval on every launch/clone. The bind→listen
    /// window has no filesystem event, so ECONNREFUSED retries at 1ms. A coarse
    /// safety tick re-checks even without events, and the deadline bounds all
    /// paths, so a lost event can only add latency, never hang.
    ///
    /// Also watches the Firecracker child process on every iteration (including
    /// the 1ms ECONNREFUSED path): if it exits before the API server accepts
    /// connections, fail with its exit status and recent stderr output instead
    /// of timing out with a generic socket error.
    async fn wait_for_socket(&mut self) -> Result<()> {
        use tokio::time::sleep;

        let mut watch =
            self.socket_path
                .parent()
                .and_then(|dir| match crate::utils::DirWatch::new(dir) {
                    Ok(w) => Some(w),
                    Err(e) => {
                        warn!(
                            error = %e,
                            "inotify unavailable for API-socket wait; degrading to 10ms polling"
                        );
                        None
                    }
                });

        let deadline = tokio::time::Instant::now() + SOCKET_WAIT_TIMEOUT;
        loop {
            // ECONNREFUSED retries on a tight 1ms cadence (set below) instead of
            // waiting for a directory event — but still falls through to the
            // child-liveness check first: if Firecracker binds the socket and
            // then dies, the socket FILE persists and refuses connections
            // forever, and skipping try_wait would spin out the whole deadline
            // and report a generic timeout instead of the exit status + stderr.
            let mut refused = false;
            match tokio::net::UnixStream::connect(&self.socket_path).await {
                Ok(stream) => {
                    // Only probing readiness — drop the connection immediately.
                    drop(stream);
                    return Ok(());
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Socket file not created yet — wait for a directory event below.
                }
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                    // bind() done, listen() imminent (sub-millisecond window
                    // with no fs event).
                    refused = true;
                }
                Err(e) => bail!(
                    "unexpected error connecting to Firecracker API socket {}: {}",
                    self.socket_path.display(),
                    e
                ),
            }

            if let Some(status) = self.try_wait()? {
                // Give the async stderr reader a moment to drain the pipe so
                // the error includes what Firecracker actually printed.
                sleep(STDERR_DRAIN_DELAY).await;
                bail!(
                    "Firecracker exited with {} before its API socket became ready{}",
                    status,
                    self.stderr_tail_message()
                );
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }

            if refused {
                // Liveness verified above — retry the connect at 1ms.
                sleep(Duration::from_millis(1)).await;
                continue;
            }

            match watch.as_mut() {
                Some(w) => {
                    tokio::select! {
                        event = w.next_event() => {
                            // Directory activity — loop re-probes the socket.
                            // A (persistent) watch error degrades to the old
                            // fixed cadence via this sleep, bounded by deadline.
                            if event.is_err() {
                                sleep(SOCKET_WAIT_RETRY_DELAY).await;
                            }
                        }
                        _ = tokio::time::sleep_until(deadline) => {}
                        _ = sleep(Duration::from_millis(100)) => {
                            // Safety tick: re-check child liveness + socket
                            // even if no event arrives.
                        }
                    }
                }
                None => sleep(SOCKET_WAIT_RETRY_DELAY).await,
            }
        }

        bail!(
            "Firecracker socket not ready after {} seconds",
            SOCKET_WAIT_TIMEOUT.as_secs()
        )
    }

    /// Render the captured Firecracker stderr tail for error messages.
    fn stderr_tail_message(&self) -> String {
        let lines: Vec<String> = self
            .stderr_tail
            .lock()
            .map(|tail| tail.iter().cloned().collect())
            .unwrap_or_default();
        if lines.is_empty() {
            "; no stderr output captured (run with RUST_LOG=debug for full Firecracker output)"
                .to_string()
        } else {
            format!("; last stderr output:\n  {}", lines.join("\n  "))
        }
    }

    /// Get the API client
    pub fn client(&self) -> Result<&FirecrackerClient> {
        self.client.as_ref().context("VM not started")
    }

    /// Get the VM process PID
    pub fn pid(&self) -> Result<u32> {
        if let Some(process) = &self.process {
            let pid_opt = process.id();
            info!("Firecracker Child.id() returned: {:?}", pid_opt);
            pid_opt.ok_or_else(|| anyhow!("process ID not available from tokio::process::Child"))
        } else {
            bail!("VM process not running")
        }
    }

    /// Non-blocking check if the VM process has exited.
    /// Returns Some(ExitStatus) if the process has exited, None if still running.
    pub fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        if let Some(ref mut process) = self.process {
            match process
                .try_wait()
                .context("checking Firecracker process status")?
            {
                Some(status) => {
                    self.process = None;
                    Ok(Some(status))
                }
                None => Ok(None),
            }
        } else {
            bail!("VM process not running")
        }
    }

    /// Wait for the VM process to exit
    ///
    /// NOTE: This does NOT take ownership of the process handle.
    /// Use `kill()` to terminate and cleanup the process.
    pub async fn wait(&mut self) -> Result<std::process::ExitStatus> {
        if let Some(ref mut process) = self.process {
            let status = process
                .wait()
                .await
                .context("waiting for Firecracker process")?;
            // Process exited naturally, clear the handle
            self.process = None;
            Ok(status)
        } else {
            bail!("VM process not running")
        }
    }

    /// SIGKILL the VM process without waiting for it to exit. Pair with [`Self::reap`].
    pub fn start_kill(&mut self) -> Result<()> {
        if let Some(ref mut process) = self.process {
            info!(vm_id = %self.vm_id, "signalling Firecracker process (SIGKILL)");
            process
                .start_kill()
                .context("signalling Firecracker process")?;
        }
        Ok(())
    }

    /// Wait for the signalled VM process to exit, reaping it.
    pub async fn reap(&mut self) {
        if let Some(mut process) = self.process.take() {
            let _ = process.wait().await; // Wait to clean up zombie
        }
    }

    /// Kill the VM process and reap it.
    pub async fn kill(&mut self) -> Result<()> {
        self.start_kill()?;
        self.reap().await;
        Ok(())
    }

    /// Stream serial console output
    pub async fn stream_console(&self, console_path: &Path) -> Result<mpsc::Receiver<String>> {
        let (tx, rx) = mpsc::channel(100);
        let console_path = console_path.to_owned();

        tokio::spawn(async move {
            // Wait for console device to appear
            for _ in 0..SOCKET_WAIT_RETRY_COUNT {
                if console_path.exists() {
                    break;
                }
                tokio::time::sleep(SOCKET_WAIT_RETRY_DELAY).await;
            }

            if !console_path.exists() {
                error!("console device not found at {:?}", console_path);
                return;
            }

            // Open and stream the console
            match tokio::fs::File::open(&console_path).await {
                Ok(file) => {
                    let reader = BufReader::new(file);
                    let mut lines = reader.lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        if tx.send(line).await.is_err() {
                            break; // Receiver dropped
                        }
                    }
                }
                Err(e) => error!("failed to open console: {}", e),
            }
        });

        Ok(rx)
    }

    /// Get VM ID
    pub fn vm_id(&self) -> &str {
        &self.vm_id
    }
}

impl Drop for VmManager {
    fn drop(&mut self) {
        // Clean up socket on drop
        if self.socket_path.exists() {
            let _ = std::fs::remove_file(&self.socket_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A Firecracker process that exits before creating its API socket must fail
    /// `start()` with its exit status and stderr output, not the generic
    /// "socket not ready after 5 seconds" timeout.
    #[tokio::test]
    async fn start_reports_immediate_firecracker_exit() {
        let dir = tempfile::tempdir().expect("create temp dir");

        let fake_fc = dir.path().join("fake-firecracker.sh");
        std::fs::write(
            &fake_fc,
            "#!/bin/sh\necho 'boom: bad firecracker argument' >&2\nexit 2\n",
        )
        .expect("write fake firecracker");
        std::fs::set_permissions(&fake_fc, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake firecracker");

        let socket_path = dir.path().join("api.sock");
        let mut vm = VmManager::new("test-vm".to_string(), socket_path, None);

        let err = vm
            .start(&fake_fc, None, None)
            .await
            .expect_err("start should fail when Firecracker exits immediately");
        let msg = format!("{:#}", err);

        assert!(
            msg.contains("Firecracker exited with"),
            "error should report the child exit status, got: {msg}"
        );
        assert!(
            msg.contains("boom: bad firecracker argument"),
            "error should include Firecracker's stderr output, got: {msg}"
        );
        assert!(
            !msg.contains("socket not ready"),
            "launch failure must not be reported as a socket timeout, got: {msg}"
        );
    }
}
