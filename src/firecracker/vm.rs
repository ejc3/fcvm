use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use super::FirecrackerClient;

/// Socket/device wait timeout (total wait time = RETRY_COUNT * RETRY_DELAY)
const SOCKET_WAIT_RETRY_COUNT: u32 = 50;
const SOCKET_WAIT_RETRY_DELAY: Duration = Duration::from_millis(100);

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
    use_user_namespace: bool,      // DEPRECATED: Use unshare for rootless operation
    namespace_wrapper: Option<Vec<String>>, // wrapper command for rootless networking (slirp4netns)
    process: Option<Child>,
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
            use_user_namespace: false,
            namespace_wrapper: None,
            process: None,
            client: None,
        }
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

    /// Enable user namespace mode for rootless operation (DEPRECATED)
    ///
    /// When set, Firecracker will be launched via `unshare --user --map-root-user --net`
    /// which creates new user and network namespaces without requiring root.
    /// Note: This doesn't set up networking - use set_namespace_wrapper for rootless networking.
    #[deprecated(note = "Use set_namespace_wrapper for rootless networking with slirp4netns")]
    pub fn set_user_namespace(&mut self, enable: bool) {
        self.use_user_namespace = enable;
    }

    /// Set namespace wrapper command for rootless networking
    ///
    /// When set, Firecracker will be launched inside a namespace created by the wrapper.
    /// The wrapper creates user + network namespaces and sets up a TAP device with
    /// userspace networking (via slirp4netns), all without requiring root privileges.
    ///
    /// The wrapper command should be the output of SlirpNetwork::build_wrapper_command(),
    /// e.g., ["unshare", "--user", "--map-root-user", "--net", "--", ...]
    pub fn set_namespace_wrapper(&mut self, wrapper_cmd: Vec<String>) {
        self.namespace_wrapper = Some(wrapper_cmd);
    }

    /// Start the Firecracker process
    pub async fn start(
        &mut self,
        firecracker_bin: &Path,
        config_override: Option<&Path>,
    ) -> Result<()> {
        if let Some(ref name) = self.vm_name {
            info!(target: "vm", vm_name = %name, vm_id = %self.vm_id, "starting Firecracker process");
        } else {
            info!(target: "vm", vm_id = %self.vm_id, "starting Firecracker process");
        }

        // Remove existing socket (ignore errors if not exists - avoids TOCTOU race)
        let _ = std::fs::remove_file(&self.socket_path);

        // Build command based on mode:
        // 1. namespace wrapper (rootless with slirp4netns networking) - highest priority
        // 2. unshare (deprecated rootless, no networking)
        // 3. direct Firecracker (privileged mode)
        let mut cmd = if let Some(ref wrapper) = self.namespace_wrapper {
            // Use wrapper to create user + network namespaces with TAP device
            info!(target: "vm", vm_id = %self.vm_id, "using namespace wrapper for rootless networking");
            let mut c = Command::new(&wrapper[0]);
            // Add remaining wrapper args (skip first which is the command)
            for arg in &wrapper[1..] {
                c.arg(arg);
            }
            // Add Firecracker command
            c.arg(firecracker_bin)
                .arg("--api-sock")
                .arg(&self.socket_path);
            c
        } else if self.use_user_namespace {
            // Use unshare to create user + network namespaces (deprecated, no networking)
            #[allow(deprecated)]
            {
                info!(target: "vm", vm_id = %self.vm_id, "using user namespace for rootless operation (no network)");
                let mut c = Command::new("unshare");
                c.arg("--user")
                    .arg("--map-root-user")
                    .arg("--net")
                    .arg("--")
                    .arg(firecracker_bin)
                    .arg("--api-sock")
                    .arg(&self.socket_path);
                c
            }
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
            cmd.arg("--level").arg("Debug");  // Enable Debug logging for detailed diagnostics
            cmd.arg("--show-level");
            cmd.arg("--show-log-origin");
        }

        // Disable seccomp for now (can enable later for production)
        cmd.arg("--no-seccomp");

        // Setup namespace isolation if specified
        if let Some(ref ns_id) = self.namespace_id {
            use std::ffi::CString;

            // Create CString outside the closure to ensure proper null termination
            // for C API usage. This avoids String capture issues in pre_exec.
            let ns_path_cstr = CString::new(format!("/var/run/netns/{}", ns_id))
                .context("namespace ID contains invalid characters (null bytes)")?;
            info!(target: "vm", vm_id = %self.vm_id, namespace = %ns_id, "entering network namespace");

            // SAFETY: pre_exec runs after fork() but before exec().
            // We use it to enter the network namespace in the child process.
            // Safety requirements:
            // 1. Only async-signal-safe functions are called (open, setns are safe)
            // 2. No heap allocations after fork (CString created before fork)
            // 3. File descriptor is properly owned via OwnedFd
            // 4. The closure doesn't capture complex types, only CString
            unsafe {
                cmd.pre_exec(move || {
                    use nix::fcntl::{open, OFlag};
                    use nix::sched::{setns, CloneFlags};
                    use nix::sys::stat::Mode;
                    use std::os::unix::io::{FromRawFd, OwnedFd};

                    // Open namespace file descriptor
                    // Safe: ns_path_cstr is a valid CString with null termination
                    let ns_fd_raw = open(ns_path_cstr.as_c_str(), OFlag::O_RDONLY, Mode::empty())
                        .map_err(|e| {
                        std::io::Error::other(format!("failed to open namespace: {}", e))
                    })?;

                    // SAFETY: from_raw_fd takes ownership of the file descriptor.
                    // The fd is valid (just opened) and won't be used elsewhere.
                    // OwnedFd will close it on drop.
                    let ns_fd = OwnedFd::from_raw_fd(ns_fd_raw);

                    // Enter the network namespace
                    setns(&ns_fd, CloneFlags::CLONE_NEWNET).map_err(|e| {
                        std::io::Error::other(format!("failed to enter namespace: {}", e))
                    })?;

                    // fd is automatically closed when OwnedFd is dropped
                    Ok(())
                });
            }
        }

        // Spawn process
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn().context("spawning Firecracker process")?;

        // Helper function to strip Firecracker timestamp and instance prefix
        // Firecracker format: "2025-11-15T17:18:55.027478889 [anonymous-instance:main] message"
        // We want to keep only: "message"
        fn strip_firecracker_prefix(line: &str) -> &str {
            let mut result = line;

            // First, strip the timestamp if present
            if let Some(pos) = result.find(' ') {
                // Check if this looks like a timestamp (starts with year)
                if result.starts_with("20") && result.chars().nth(4) == Some('-') {
                    result = &result[pos + 1..]; // Skip past timestamp
                }
            }

            // Now strip the [anonymous-instance:xxx] prefix if present
            if result.starts_with('[') {
                if let Some(end_pos) = result.find("] ") {
                    result = &result[end_pos + 2..]; // Skip past the bracketed prefix
                }
            }

            result
        }

        // Stream stdout/stderr to tracing
        if let Some(stdout) = child.stdout.take() {
            let vm_id = self.vm_id.clone();
            let vm_name = self.vm_name.clone();
            tokio::spawn(async move {
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let clean_line = strip_firecracker_prefix(&line);
                    if let Some(ref name) = vm_name {
                        info!(target: "firecracker", vm_name = %name, vm_id = %vm_id, "{}", clean_line);
                    } else {
                        info!(target: "firecracker", vm_id = %vm_id, "{}", clean_line);
                    }
                }
            });
        }

        if let Some(stderr) = child.stderr.take() {
            let vm_id = self.vm_id.clone();
            let vm_name = self.vm_name.clone();
            tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let clean_line = strip_firecracker_prefix(&line);
                    if let Some(ref name) = vm_name {
                        warn!(target: "firecracker", vm_name = %name, vm_id = %vm_id, "{}", clean_line);
                    } else {
                        warn!(target: "firecracker", vm_id = %vm_id, "{}", clean_line);
                    }
                }
            });
        }

        self.process = Some(child);

        // Wait for socket to be ready
        self.wait_for_socket().await?;

        // Create API client
        self.client = Some(FirecrackerClient::new(self.socket_path.clone())?);

        Ok(())
    }

    /// Wait for Firecracker socket to be ready
    async fn wait_for_socket(&self) -> Result<()> {
        use tokio::time::sleep;

        for _ in 0..SOCKET_WAIT_RETRY_COUNT {
            if self.socket_path.exists() {
                return Ok(());
            }
            sleep(SOCKET_WAIT_RETRY_DELAY).await;
        }

        let timeout_secs = SOCKET_WAIT_RETRY_COUNT as u64 * SOCKET_WAIT_RETRY_DELAY.as_millis() as u64 / 1000;
        bail!("Firecracker socket not ready after {} seconds", timeout_secs)
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

    /// Wait for the VM process to exit
    pub async fn wait(&mut self) -> Result<std::process::ExitStatus> {
        if let Some(mut process) = self.process.take() {
            let status = process
                .wait()
                .await
                .context("waiting for Firecracker process")?;
            Ok(status)
        } else {
            bail!("VM process not running")
        }
    }

    /// Kill the VM process
    pub async fn kill(&mut self) -> Result<()> {
        if let Some(mut process) = self.process.take() {
            info!(vm_id = %self.vm_id, "killing Firecracker process");
            process
                .kill()
                .await
                .context("killing Firecracker process")?;
            let _ = process.wait().await; // Wait to clean up zombie
        }
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
