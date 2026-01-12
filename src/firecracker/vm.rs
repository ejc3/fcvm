use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
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

    /// Set namespace holder PID for rootless networking
    ///
    /// When set, Firecracker will be launched inside an existing user+net namespace
    /// via nsenter. The holder process (created by `unshare --user --map-root-user --net -- cat`)
    /// keeps the namespace alive while Firecracker runs.
    ///
    /// With `--map-root-user`, the current user (UID 1000) is mapped to UID 0 inside the
    /// namespace. Combined with `--preserve-credentials` when using nsenter, no chmod
    /// is needed on sockets or files - the user has access both inside and outside.
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
        // 1. user_namespace_path set: direct Firecracker (namespaces entered via pre_exec setns)
        // 2. holder_pid set (no user_namespace_path): use nsenter to enter existing namespace (rootless baseline)
        // 3. neither: direct Firecracker (privileged/bridged mode)
        //
        // For rootless clones with mount_redirects, we MUST use pre_exec setns instead of nsenter,
        // because pre_exec runs BEFORE nsenter would enter the namespace, and we need CAP_SYS_ADMIN
        // from the user namespace to do mount operations.
        let mut cmd = if self.user_namespace_path.is_some() {
            // Use direct Firecracker - namespaces will be entered via setns in pre_exec
            // This is required for rootless clones that need mount namespace isolation
            info!(target: "vm", vm_id = %self.vm_id, "using pre_exec setns for rootless clone");
            let mut c = Command::new(firecracker_bin);
            c.arg("--api-sock").arg(&self.socket_path);
            c
        } else if let Some(holder_pid) = self.holder_pid {
            // Use nsenter to enter user+network namespace with preserved credentials
            // --preserve-credentials keeps UID, GID, and supplementary groups (including kvm)
            // This allows KVM access while being in the isolated network namespace
            // NOTE: This path is for baseline VMs that don't need mount namespace isolation
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

        // Disable seccomp for now (can enable later for production)
        cmd.arg("--no-seccomp");

        // Additional firecracker args from environment (caller controls)
        if let Ok(extra) = std::env::var("FCVM_FIRECRACKER_ARGS") {
            for arg in extra.split_whitespace() {
                cmd.arg(arg);
            }
        }

        // Setup namespace isolation if specified (network namespace and/or mount namespace)
        // We need to handle these in a single pre_exec because it can only be called once
        let ns_id_clone = self.namespace_id.clone();
        let mount_redirects_clone = self.mount_redirects.clone();
        let user_ns_path_clone = self.user_namespace_path.clone();
        let net_ns_path_clone = self.net_namespace_path.clone();

        // Ensure baseline directories exist for bind mount targets
        // The baseline VMs may have been cleaned up, but we need the directories for mount
        if let Some((ref baseline_dirs, _)) = mount_redirects_clone {
            for baseline_dir in baseline_dirs {
                if !baseline_dir.exists() {
                    std::fs::create_dir_all(baseline_dir)
                        .context("creating baseline directory for mount redirect")?;
                }
            }
        }

        if ns_id_clone.is_some()
            || mount_redirects_clone.is_some()
            || user_ns_path_clone.is_some()
            || net_ns_path_clone.is_some()
        {
            use std::ffi::CString;

            // Prepare CStrings outside the closure (async-signal-safe requirement)
            let ns_path_cstr = if let Some(ref ns_id) = ns_id_clone {
                info!(target: "vm", vm_id = %self.vm_id, namespace = %ns_id, "entering network namespace");
                Some(
                    CString::new(format!("/var/run/netns/{}", ns_id))
                        .context("namespace ID contains invalid characters (null bytes)")?,
                )
            } else {
                None
            };

            // User namespace path (for rootless clones that need CAP_SYS_ADMIN for mount ops)
            let user_ns_cstr = if let Some(ref path) = user_ns_path_clone {
                info!(target: "vm", vm_id = %self.vm_id, path = %path.display(), "will enter user namespace in pre_exec");
                Some(
                    CString::new(path.to_string_lossy().as_bytes())
                        .context("user namespace path contains invalid characters")?,
                )
            } else {
                None
            };

            // Network namespace path (for rootless clones via /proc/PID/ns/net)
            let net_ns_cstr = if let Some(ref path) = net_ns_path_clone {
                info!(target: "vm", vm_id = %self.vm_id, path = %path.display(), "will enter net namespace in pre_exec");
                Some(
                    CString::new(path.to_string_lossy().as_bytes())
                        .context("net namespace path contains invalid characters")?,
                )
            } else {
                None
            };

            let mount_paths = if let Some((ref baseline_dirs, ref clone_dir)) = mount_redirects_clone
            {
                info!(target: "vm", vm_id = %self.vm_id,
                    baseline_dirs = ?baseline_dirs.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                    clone = %clone_dir.display(),
                    "setting up mount namespace for mount redirects");
                let clone_cstr = CString::new(clone_dir.to_string_lossy().as_bytes())
                    .context("clone path contains invalid characters")?;
                let baseline_cstrs: Vec<CString> = baseline_dirs
                    .iter()
                    .map(|p| {
                        CString::new(p.to_string_lossy().as_bytes())
                            .context("baseline path contains invalid characters")
                    })
                    .collect::<Result<Vec<_>>>()?;
                Some((baseline_cstrs, clone_cstr))
            } else {
                None
            };

            // SAFETY: pre_exec runs after fork() but before exec().
            // We use it to set up namespace isolation in the child process.
            // Safety requirements:
            // 1. Only async-signal-safe functions are called (open, setns, unshare, mount are safe)
            // 2. No heap allocations after fork (CStrings created before fork)
            // 3. File descriptors are properly owned via OwnedFd
            // 4. The closure captures only CStrings and Option types
            unsafe {
                cmd.pre_exec(move || {
                    use nix::fcntl::{open, OFlag};
                    use nix::mount::{mount, MsFlags};
                    use nix::sched::{setns, unshare, CloneFlags};
                    use nix::sys::stat::Mode;
                    use std::os::unix::io::{FromRawFd, OwnedFd};

                    // Step 0: Enter user namespace if specified (for rootless clones)
                    // This MUST be done first to get CAP_SYS_ADMIN for mount operations.
                    // The user namespace was created by the holder process with --map-root-user,
                    // so entering it gives us UID 0 with full capabilities inside the namespace.
                    if let Some(ref user_ns_path) = user_ns_cstr {
                        let ns_fd_raw = open(
                            user_ns_path.as_c_str(),
                            OFlag::O_RDONLY,
                            Mode::empty(),
                        )
                        .map_err(|e| {
                            std::io::Error::other(format!("failed to open user namespace: {}", e))
                        })?;

                        let ns_fd = OwnedFd::from_raw_fd(ns_fd_raw);

                        setns(&ns_fd, CloneFlags::CLONE_NEWUSER).map_err(|e| {
                            std::io::Error::other(format!("failed to enter user namespace: {}", e))
                        })?;
                        // Now we have CAP_SYS_ADMIN inside the user namespace!
                    }

                    // Step 1: Set up mount namespace for path redirects if needed
                    // This must be done BEFORE entering network namespace
                    // Note: This now succeeds because we entered user namespace first (if needed)
                    if let Some((ref baseline_cstrs, ref clone_cstr)) = mount_paths {
                        // Create a new mount namespace so our bind mounts are isolated
                        unshare(CloneFlags::CLONE_NEWNS).map_err(|e| {
                            std::io::Error::other(format!(
                                "failed to unshare mount namespace: {}",
                                e
                            ))
                        })?;

                        // Make our mount namespace private so mounts don't propagate
                        // This is equivalent to: mount --make-rprivate /
                        mount::<str, str, str, str>(
                            None,
                            "/",
                            None,
                            MsFlags::MS_REC | MsFlags::MS_PRIVATE,
                            None,
                        )
                        .map_err(|e| {
                            std::io::Error::other(format!("failed to make mount private: {}", e))
                        })?;

                        // Bind mount clone_dir over each baseline_dir
                        // This makes Firecracker see clone's files when accessing any baseline's path
                        for baseline_cstr in baseline_cstrs {
                            mount(
                                Some(clone_cstr.as_c_str()),
                                baseline_cstr.as_c_str(),
                                None::<&str>,
                                MsFlags::MS_BIND,
                                None::<&str>,
                            )
                            .map_err(|e| {
                                std::io::Error::other(format!(
                                    "failed to bind mount {:?} over {:?}: {}",
                                    clone_cstr, baseline_cstr, e
                                ))
                            })?;
                        }
                    }

                    // Step 2: Enter network namespace if specified
                    // This can come from either:
                    // - net_ns_cstr: /proc/PID/ns/net (rootless clones via pre_exec) - preferred
                    // - ns_path_cstr: /var/run/netns/NAME (bridged mode)
                    let net_ns_to_enter = net_ns_cstr.as_ref().or(ns_path_cstr.as_ref());
                    if let Some(ns_path) = net_ns_to_enter {
                        let ns_fd_raw = open(ns_path.as_c_str(), OFlag::O_RDONLY, Mode::empty())
                            .map_err(|e| {
                                std::io::Error::other(format!(
                                    "failed to open net namespace: {}",
                                    e
                                ))
                            })?;

                        // SAFETY: from_raw_fd takes ownership of the file descriptor.
                        let ns_fd = OwnedFd::from_raw_fd(ns_fd_raw);

                        setns(&ns_fd, CloneFlags::CLONE_NEWNET).map_err(|e| {
                            std::io::Error::other(format!("failed to enter net namespace: {}", e))
                        })?;
                        // fd is automatically closed when OwnedFd is dropped
                    }

                    Ok(())
                });
            }
        }

        // Spawn process with streaming output
        let child = spawn_streaming(cmd, |line, is_stderr| {
            let clean = strip_firecracker_prefix(line);
            // fc-agent and container output at INFO/WARN, everything else at DEBUG
            let is_important = clean.contains("fc-agent") || clean.contains("[ctr:");
            if is_stderr {
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

    /// Wait for Firecracker socket to be ready
    async fn wait_for_socket(&self) -> Result<()> {
        use tokio::time::sleep;

        for _ in 0..SOCKET_WAIT_RETRY_COUNT {
            if self.socket_path.exists() {
                return Ok(());
            }
            sleep(SOCKET_WAIT_RETRY_DELAY).await;
        }

        let timeout_secs =
            SOCKET_WAIT_RETRY_COUNT as u64 * SOCKET_WAIT_RETRY_DELAY.as_millis() as u64 / 1000;
        bail!(
            "Firecracker socket not ready after {} seconds",
            timeout_secs
        )
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
