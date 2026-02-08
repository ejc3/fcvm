mod fuse;
mod tty;

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
    /// Extra block devices (mounted automatically)
    #[serde(default)]
    extra_disks: Vec<ExtraDiskMount>,
    /// NFS shares from host (mounted automatically)
    #[serde(default)]
    nfs_mounts: Vec<NfsMount>,
    /// Path to Docker archive for localhost/ images (imported via podman load)
    #[serde(default)]
    image_archive: Option<String>,
    /// Run container in privileged mode (allows mknod, device access, etc.)
    #[serde(default)]
    privileged: bool,
    /// Keep STDIN open even if not attached
    #[serde(default)]
    interactive: bool,
    /// Allocate a pseudo-TTY
    #[serde(default)]
    tty: bool,
    /// HTTP proxy for container registry access
    #[serde(default)]
    http_proxy: Option<String>,
    /// HTTPS proxy for container registry access
    #[serde(default)]
    https_proxy: Option<String>,
    /// Hosts/domains that bypass the proxy
    #[serde(default)]
    no_proxy: Option<String>,
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

/// Extra disk mount configuration from MMDS
#[derive(Debug, Clone, Deserialize)]
struct ExtraDiskMount {
    /// Device path (e.g., /dev/vdb)
    device: String,
    /// Mount path inside guest (e.g., /mnt/extra-disk-0)
    mount_path: String,
    /// Read-only flag
    #[serde(default)]
    read_only: bool,
}

/// NFS mount configuration from MMDS
#[derive(Debug, Clone, Deserialize)]
struct NfsMount {
    /// Host IP address (NFS server)
    host_ip: String,
    /// Path on host being exported
    host_path: String,
    /// Mount path inside guest
    mount_path: String,
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
}

/// Ensure cgroup controllers are available for container creation.
///
/// With `--cgroups=split`, podman/crun creates container cgroups under fc-agent's cgroup,
/// bypassing systemd. For this to work, the pids controller must be enabled in:
/// 1. The root cgroup's subtree_control
/// 2. All intermediate cgroups (system.slice, fc-agent.service, etc.)
///
/// In cgroup v2, a controller must be in the parent's subtree_control for child cgroups to use it.
/// This function enables pids in the entire cgroup chain from root to fc-agent's parent cgroup.
async fn wait_for_cgroup_controllers() {
    use tokio::fs;

    const REQUIRED_CONTROLLER: &str = "pids";

    // Get fc-agent's current cgroup from /proc/self/cgroup
    // In cgroup v2, the format is "0::/path/to/cgroup"
    let my_cgroup = match fs::read_to_string("/proc/self/cgroup").await {
        Ok(content) => {
            // Parse cgroup v2 format: "0::/system.slice/fc-agent.service"
            content
                .lines()
                .find(|l| l.starts_with("0::"))
                .map(|l| l.strip_prefix("0::").unwrap_or("/").to_string())
                .unwrap_or_else(|| "/".to_string())
        }
        Err(e) => {
            eprintln!(
                "[fc-agent] WARNING: failed to read /proc/self/cgroup: {}",
                e
            );
            "/".to_string()
        }
    };

    eprintln!("[fc-agent] current cgroup: {}", my_cgroup);

    // Build the list of cgroup paths from root to our cgroup (inclusive)
    // e.g., for "/system.slice/fc-agent.service":
    //   - /sys/fs/cgroup (root)
    //   - /sys/fs/cgroup/system.slice
    //   - /sys/fs/cgroup/system.slice/fc-agent.service (our cgroup - containers go UNDER this)
    //
    // With --cgroups=split, podman/crun creates container cgroups as CHILDREN of fc-agent.service.
    // For a child to use pids, the PARENT must have pids in its subtree_control.
    // So we must enable pids in fc-agent.service/cgroup.subtree_control too.
    let mut paths_to_enable = vec!["/sys/fs/cgroup".to_string()];
    let mut current_path = "/sys/fs/cgroup".to_string();

    for component in my_cgroup.trim_start_matches('/').split('/') {
        if component.is_empty() {
            continue;
        }
        current_path = format!("{}/{}", current_path, component);
        paths_to_enable.push(current_path.clone());
    }

    eprintln!(
        "[fc-agent] enabling pids controller in cgroup chain: {:?}",
        paths_to_enable
    );

    // Enable pids in each cgroup's subtree_control
    for cgroup_path in &paths_to_enable {
        let subtree_control_path = format!("{}/cgroup.subtree_control", cgroup_path);

        // Check if pids is already enabled
        match fs::read_to_string(&subtree_control_path).await {
            Ok(controllers) => {
                let available: Vec<&str> = controllers.split_whitespace().collect();
                if available.contains(&REQUIRED_CONTROLLER) {
                    continue; // Already enabled
                }

                // Try to enable pids
                match fs::write(&subtree_control_path, format!("+{}\n", REQUIRED_CONTROLLER)).await
                {
                    Ok(()) => {
                        eprintln!(
                            "[fc-agent] enabled '{}' controller in {}",
                            REQUIRED_CONTROLLER, subtree_control_path
                        );
                    }
                    Err(e) => {
                        eprintln!(
                            "[fc-agent] WARNING: failed to enable '{}' in {}: {}",
                            REQUIRED_CONTROLLER, subtree_control_path, e
                        );
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "[fc-agent] WARNING: failed to read {}: {}",
                    subtree_control_path, e
                );
            }
        }
    }

    // Verify pids is now available in our parent's subtree_control
    let parent_subtree = if my_cgroup == "/" {
        "/sys/fs/cgroup/cgroup.subtree_control".to_string()
    } else {
        let parent_cgroup = std::path::Path::new(&my_cgroup)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "/".to_string());
        if parent_cgroup == "/" || parent_cgroup.is_empty() {
            "/sys/fs/cgroup/cgroup.subtree_control".to_string()
        } else {
            format!("/sys/fs/cgroup{}/cgroup.subtree_control", parent_cgroup)
        }
    };

    match fs::read_to_string(&parent_subtree).await {
        Ok(controllers) => {
            let available: Vec<&str> = controllers.split_whitespace().collect();
            if available.contains(&REQUIRED_CONTROLLER) {
                eprintln!(
                    "[fc-agent] cgroup controllers available in {}: {}",
                    parent_subtree,
                    controllers.trim()
                );
            } else {
                eprintln!(
                    "[fc-agent] WARNING: '{}' not available in {} after enabling (available: {})",
                    REQUIRED_CONTROLLER,
                    parent_subtree,
                    controllers.trim()
                );
            }
        }
        Err(e) => {
            eprintln!(
                "[fc-agent] WARNING: failed to verify controllers in {}: {}",
                parent_subtree, e
            );
        }
    }
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

/// Watch for restore-epoch changes in MMDS and handle clone restore.
/// `boot_volumes` are the volumes from the initial boot plan — these are used
/// directly for remount since they're always correct (clones inherit the same
/// volume config from the snapshot).
async fn watch_restore_epoch(boot_volumes: Vec<VolumeMount>) {
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

        // Try to fetch current restore-epoch from MMDS
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
                        "[fc-agent] detected restore-epoch: {} (clone restore detected, volumes: {})",
                        current, boot_volumes.len()
                    );
                    handle_clone_restore(&boot_volumes).await;
                    last_epoch = metadata.restore_epoch;
                }
                Some(prev) if prev != current => {
                    // Epoch changed! This means we were restored from snapshot again
                    eprintln!(
                        "[fc-agent] restore-epoch changed: {} -> {} (volumes: {})",
                        prev,
                        current,
                        boot_volumes.len()
                    );
                    handle_clone_restore(&boot_volumes).await;
                    last_epoch = metadata.restore_epoch;
                }
                _ => {
                    // No change
                }
            }
        }
    }
}

/// Handle clone restore: kill stale sockets, flush ARP, send gratuitous ARP, and remount volumes
async fn handle_clone_restore(volumes: &[VolumeMount]) {
    // 1. KILL all established TCP connections immediately
    // After snapshot restore, existing TCP connections are DEAD (different network namespace).
    // Processes blocked on read() will hang FOREVER because no packets arrive.
    // ss -K destroys sockets directly, waking any blocked read()/write() calls.
    kill_stale_tcp_connections().await;

    // 2. Flush ARP cache (stale MAC entries from previous network)
    flush_arp_cache().await;

    // 3. Send gratuitous ARP to teach new slirp4netns our MAC address
    // Critical for bridge-based networking: the new slirp4netns process doesn't
    // know our MAC, so health checks would fail without this.
    send_gratuitous_arp().await;

    // Note: Interface bounce (ip link down/up) is NOT needed - ss -K handles socket cleanup
    // more effectively by directly destroying sockets rather than hoping they notice ENETDOWN.

    // 4. Remount FUSE volumes if any
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
    // After snapshot restore, Firecracker places VIRTIO_VSOCK_EVENT_TRANSPORT_RESET
    // in the guest's virtio event queue. The kernel processes this asynchronously
    // after resume, killing ALL vsock connections (including newly created ones).
    // Wait for the transport reset to complete before creating new connections.
    sleep(Duration::from_millis(500)).await;

    for vol in volumes {
        // Retry remount: the first attempt may fail if the kernel is still
        // processing the vsock transport reset from the snapshot.
        for attempt in 0..3 {
            if attempt > 0 {
                eprintln!(
                    "[fc-agent] retrying remount of {} (attempt {})",
                    vol.guest_path,
                    attempt + 1
                );
                sleep(Duration::from_millis(500)).await;
            }

            eprintln!(
                "[fc-agent] remounting volume at {} (port {})",
                vol.guest_path, vol.vsock_port
            );

            // Unmount the old (broken) FUSE mount
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

            // Ensure mount point directory exists. Ignore AlreadyExists since
            // the directory is expected to exist when remounting after snapshot.
            if let Err(e) = std::fs::create_dir_all(&vol.guest_path) {
                if e.kind() != std::io::ErrorKind::AlreadyExists {
                    eprintln!(
                        "[fc-agent] ERROR: cannot create mount point {}: {}",
                        vol.guest_path, e
                    );
                    break;
                }
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

            // Wait for FUSE mount to initialize, then verify it works
            sleep(Duration::from_millis(500)).await;

            if std::fs::metadata(&vol.guest_path).is_ok() {
                eprintln!("[fc-agent] ✓ volume {} remount verified", vol.guest_path);
                break;
            } else {
                eprintln!(
                    "[fc-agent] volume {} mount not accessible after remount",
                    vol.guest_path
                );
            }
        }
    }

    if volumes.is_empty() {
        return;
    }

    // Rebind new FUSE mounts into the container's mount namespace.
    // The container has stale bind mounts from before the snapshot — podman's
    // -v flag creates bind mounts into the container's mount namespace, and
    // those don't automatically see FUSE remounts in the root namespace.
    rebind_volumes_in_container(volumes).await;

    eprintln!("[fc-agent] ✓ volume remounts complete");
}

/// After FUSE remount in root namespace, rebind into the container's mount
/// namespace so `podman exec` commands see the new FUSE mount instead of
/// the stale bind mount from before the snapshot.
///
/// Uses the new mount API (open_tree + move_mount, Linux 5.2+) because the
/// traditional mount --bind rejects cross-namespace sources (check_mnt).
async fn rebind_volumes_in_container(volumes: &[VolumeMount]) {
    // Get the container's PID (it may not be running yet during initial boot)
    let pid_output = match Command::new("podman")
        .args(["inspect", "--format", "{{.State.Pid}}", "fcvm-container"])
        .output()
        .await
    {
        Ok(o) if o.status.success() => o,
        Ok(_) => {
            eprintln!("[fc-agent] container not running, skipping mount rebind");
            return;
        }
        Err(e) => {
            eprintln!(
                "[fc-agent] podman inspect failed: {}, skipping mount rebind",
                e
            );
            return;
        }
    };

    let container_pid = String::from_utf8_lossy(&pid_output.stdout)
        .trim()
        .to_string();
    if container_pid.is_empty() || container_pid == "0" {
        eprintln!("[fc-agent] container PID is 0, skipping mount rebind");
        return;
    }

    for vol in volumes {
        let pid = container_pid.clone();
        let path = vol.guest_path.clone();

        let result = tokio::task::spawn_blocking(move || rebind_mount_cross_ns(&pid, &path)).await;

        match result {
            Ok(Ok(())) => {
                eprintln!(
                    "[fc-agent] ✓ volume {} rebound in container namespace",
                    vol.guest_path
                );
            }
            Ok(Err(e)) => {
                eprintln!(
                    "[fc-agent] WARNING: rebind {} in container failed: {}",
                    vol.guest_path, e
                );
            }
            Err(e) => {
                eprintln!(
                    "[fc-agent] WARNING: rebind task failed for {}: {}",
                    vol.guest_path, e
                );
            }
        }
    }
}

/// Rebind a FUSE mount from the root namespace into a container's mount namespace.
///
/// Uses fork + open_tree + move_mount:
/// 1. open_tree(CLONE) creates a namespace-neutral detached mount clone
/// 2. Child process enters the container's mount namespace via setns
/// 3. move_mount places the detached clone at the target path
///
/// This avoids the check_mnt restriction that blocks traditional mount --bind
/// across mount namespace boundaries.
fn rebind_mount_cross_ns(container_pid: &str, guest_path: &str) -> Result<(), String> {
    use std::ffi::CString;
    use std::os::unix::io::AsRawFd;

    // libc crate provides these for both aarch64 and x86_64
    const SYS_OPEN_TREE: libc::c_long = libc::SYS_open_tree;
    const SYS_MOVE_MOUNT: libc::c_long = libc::SYS_move_mount;

    const OPEN_TREE_CLONE: libc::c_ulong = 1;
    const MOVE_MOUNT_F_EMPTY_PATH: libc::c_ulong = 4;

    let path_c = CString::new(guest_path).map_err(|e| format!("invalid path: {}", e))?;

    // Step 1: Create a detached mount clone of the FUSE mount (from root namespace).
    // open_tree with OPEN_TREE_CLONE creates a mount that belongs to no namespace,
    // so it can be moved into any namespace with move_mount.
    let tree_fd = unsafe {
        libc::syscall(
            SYS_OPEN_TREE,
            libc::AT_FDCWD,
            path_c.as_ptr(),
            OPEN_TREE_CLONE,
        )
    };
    if tree_fd < 0 {
        return Err(format!(
            "open_tree({}) failed: {}",
            guest_path,
            std::io::Error::last_os_error()
        ));
    }
    let tree_fd = tree_fd as libc::c_int;

    // Step 2: Open references for container namespace entry
    let ns_path = format!("/proc/{}/ns/mnt", container_pid);
    let root_path = format!("/proc/{}/root", container_pid);

    let ns_file = std::fs::File::open(&ns_path).map_err(|e| {
        unsafe { libc::close(tree_fd) };
        format!("open container mount ns: {}", e)
    })?;
    let root_file = std::fs::File::open(&root_path).map_err(|e| {
        unsafe { libc::close(tree_fd) };
        format!("open container root: {}", e)
    })?;

    // Step 3: Fork a child to enter container namespace and move the mount.
    // We fork because setns() changes the calling thread's namespace permanently.
    // All syscalls in the child are async-signal-safe.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe { libc::close(tree_fd) };
        return Err(format!("fork: {}", std::io::Error::last_os_error()));
    }

    if pid == 0 {
        // === Child process ===
        // Enter container's mount namespace
        if unsafe { libc::setns(ns_file.as_raw_fd(), libc::CLONE_NEWNS) } != 0 {
            unsafe { libc::_exit(1) };
        }
        // chroot to container's root (so path resolution works after pivot_root)
        if unsafe { libc::fchdir(root_file.as_raw_fd()) } != 0 {
            unsafe { libc::_exit(2) };
        }
        if unsafe { libc::chroot(c".".as_ptr()) } != 0 {
            unsafe { libc::_exit(3) };
        }

        // Unmount the stale bind mount (lazy, ignore errors — may already be gone)
        unsafe { libc::umount2(path_c.as_ptr(), libc::MNT_DETACH) };

        // Move the detached mount clone into this namespace at guest_path
        let empty = c"".as_ptr();
        let ret = unsafe {
            libc::syscall(
                SYS_MOVE_MOUNT,
                tree_fd,
                empty,
                libc::AT_FDCWD,
                path_c.as_ptr(),
                MOVE_MOUNT_F_EMPTY_PATH,
            )
        };

        unsafe { libc::_exit(if ret == 0 { 0 } else { 5 }) };
    }

    // === Parent process ===
    unsafe { libc::close(tree_fd) };

    let mut status: libc::c_int = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };

    // WIFEXITED: (status & 0x7f) == 0
    // WEXITSTATUS: (status >> 8) & 0xff
    let exited = (status & 0x7f) == 0;
    let exit_code = (status >> 8) & 0xff;

    if exited && exit_code == 0 {
        Ok(())
    } else {
        Err(format!("rebind child failed (exit code {})", exit_code))
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

/// Send gratuitous ARP to announce guest's MAC address to the network.
/// This is critical after clone restore: the new slirp4netns process doesn't
/// know the guest's MAC address. Without this, health checks fail because
/// slirp can't route packets to the guest.
///
/// Uses ping to the gateway which forces an ARP exchange.
async fn send_gratuitous_arp() {
    // Get the default gateway IP
    let route_output = Command::new("ip")
        .args(["route", "show", "default"])
        .output()
        .await;

    let gateway = match route_output {
        Ok(o) if o.status.success() => {
            let output = String::from_utf8_lossy(&o.stdout);
            // Parse "default via 10.0.2.2 dev eth0" to get gateway IP
            output
                .split_whitespace()
                .skip_while(|&s| s != "via")
                .nth(1)
                .map(|s| s.to_string())
        }
        _ => None,
    };

    let Some(gateway) = gateway else {
        eprintln!("[fc-agent] WARNING: could not determine gateway for gratuitous ARP");
        return;
    };

    eprintln!("[fc-agent] sending gratuitous ARP to gateway {}", gateway);

    // Ping the gateway to force an ARP exchange
    // This makes slirp4netns learn our MAC address
    let ping_output = Command::new("ping")
        .args(["-c", "1", "-W", "1", &gateway])
        .output()
        .await;

    match ping_output {
        Ok(o) if o.status.success() => {
            eprintln!("[fc-agent] ✓ gratuitous ARP sent (pinged gateway)");
        }
        Ok(o) => {
            // Ping may fail (ICMP blocked) but ARP was still sent
            eprintln!(
                "[fc-agent] gratuitous ARP sent (ping returned: {})",
                String::from_utf8_lossy(&o.stderr).trim()
            );
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: failed to send gratuitous ARP: {}", e);
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

/// Container output streaming port (line-based protocol)
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

/// Wrapper for vsock fd to use with tokio's AsyncFd.
/// Implements Drop to close the fd automatically on all exit paths.
struct VsockListener {
    fd: i32,
}

impl std::os::unix::io::AsRawFd for VsockListener {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        self.fd
    }
}

impl Drop for VsockListener {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

/// Run the exec server with ready signal support.
/// This is identical to run_exec_server() but sends a signal when the server is listening.
async fn run_exec_server_with_ready_signal(ready_tx: tokio::sync::oneshot::Sender<()>) {
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

    // Wrap immediately in RAII so all error paths close the fd
    let listener = VsockListener { fd: listener_fd };

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
            listener.fd,
            &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_vm>() as u32,
        )
    };

    if bind_result < 0 {
        eprintln!(
            "[fc-agent] ERROR: failed to bind vsock listener: {}",
            std::io::Error::last_os_error()
        );
        return;
    }

    // Start listening with larger backlog for parallel exec stress
    // Default of 5 is too small when many execs arrive simultaneously
    let listen_result = unsafe { libc::listen(listener.fd, 128) };
    if listen_result < 0 {
        eprintln!(
            "[fc-agent] ERROR: failed to listen on vsock: {}",
            std::io::Error::last_os_error()
        );
        return;
    }

    eprintln!(
        "[fc-agent] ✓ exec server listening on vsock port {}",
        EXEC_VSOCK_PORT
    );

    // Wrap in AsyncFd for async accept.
    // AsyncFd::new() takes ownership of listener; if it fails, it drops
    // the listener which closes the fd via our Drop impl.
    let async_fd = match tokio::io::unix::AsyncFd::new(listener) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("[fc-agent] ERROR: failed to create AsyncFd: {}", e);
            return;
        }
    };

    // Yield to ensure the tokio runtime has fully registered the AsyncFd
    // before signaling readiness. This prevents race conditions where
    // connections arrive before the runtime is ready to dispatch events.
    tokio::task::yield_now().await;

    // Signal that we're ready (after AsyncFd creation succeeds)
    let _ = ready_tx.send(());

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
                async_fd.get_ref().fd,
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
    const MAX_EXEC_LINE_LENGTH: usize = 1_048_576; // 1MB
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
        if line.len() >= MAX_EXEC_LINE_LENGTH {
            eprintln!(
                "[fc-agent] exec request line exceeds {} bytes, rejecting",
                MAX_EXEC_LINE_LENGTH
            );
            unsafe { libc::close(fd) };
            return;
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
        // Build command for exec
        let command = if request.in_container {
            let mut cmd = vec!["podman".to_string(), "exec".to_string()];
            // Pass -i and -t to podman exec
            if request.interactive {
                cmd.push("-i".to_string());
            }
            if request.tty {
                cmd.push("-t".to_string());
            }
            // Pass proxy settings via -e flags for podman exec
            for (key, value) in read_proxy_settings() {
                cmd.push("-e".to_string());
                cmd.push(format!("{}={}", key, value));
            }
            cmd.push("--latest".to_string());
            cmd.extend(request.command.iter().cloned());
            cmd
        } else {
            request.command.clone()
        };

        // Use unified TTY handler
        // NOTE: Don't call std::process::exit() here - that would kill the entire fc-agent!
        // run_with_pty_fd already sends the exit code via exec_proto and closes the fd.
        // We just let the spawn_blocking task end naturally.
        let _exit_code = tty::run_with_pty_fd(fd, &command, request.tty, request.interactive);
    } else {
        handle_exec_pipe(fd, &request);
    }
}

/// Handle exec in pipe mode (non-TTY)
fn handle_exec_pipe(fd: i32, request: &ExecRequest) {
    use std::io::{BufRead, BufReader};

    // Read proxy settings for external network access
    let proxy_settings = read_proxy_settings();

    // Build the command using std::process::Command (blocking)
    let mut cmd = if request.in_container {
        // Execute inside the container using podman exec
        let mut cmd = std::process::Command::new("podman");
        cmd.arg("exec");
        // Pass -i flag if interactive mode requested
        if request.interactive {
            cmd.arg("-i");
        }
        // Pass proxy settings via -e flags for podman exec
        for (key, value) in &proxy_settings {
            cmd.arg("-e").arg(format!("{}={}", key, value));
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
        // Set proxy environment variables for VM-level commands
        for (key, value) in &proxy_settings {
            cmd.env(key, value);
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

/// Get the digest of a pulled image using podman inspect.
/// Returns the digest string (e.g., "sha256:abc123...")
async fn get_image_digest(image: &str) -> Result<String> {
    let output = Command::new("podman")
        .args(["image", "inspect", "--format", "{{.Digest}}", image])
        .output()
        .await
        .context("running podman image inspect")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("podman image inspect failed: {}", stderr);
    }

    let digest = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(digest)
}

/// Notify host that image is loaded and wait for cache creation acknowledgment.
///
/// Sends "cache-ready:{digest}\n" to host and waits for "cache-ack\n" response.
/// The host will pause the VM, create a snapshot, resume, then send ack.
/// This blocks indefinitely until ack is received (no timeout).
fn notify_cache_ready_and_wait(digest: &str) -> bool {
    use nix::fcntl::{fcntl, FcntlArg, OFlag};
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, VsockAddr};
    use nix::unistd::{read, write};
    use std::os::fd::{AsFd, AsRawFd};

    // Create vsock socket
    let sock = match socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::empty(),
        None,
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "[fc-agent] WARNING: failed to create vsock socket for cache: {}",
                e
            );
            return false;
        }
    };

    // Connect to host
    let addr = VsockAddr::new(HOST_CID, STATUS_VSOCK_PORT);
    if let Err(e) = connect(sock.as_raw_fd(), &addr) {
        eprintln!(
            "[fc-agent] WARNING: failed to connect vsock for cache: {}",
            e
        );
        return false;
    }

    // Send cache-ready message
    let msg = format!("cache-ready:{}\n", digest);
    match write(&sock, msg.as_bytes()) {
        Ok(n) if n == msg.len() => {}
        Ok(_) => {
            eprintln!("[fc-agent] WARNING: failed to send complete cache-ready message");
            return false;
        }
        Err(e) => {
            eprintln!(
                "[fc-agent] WARNING: failed to send cache-ready message: {}",
                e
            );
            return false;
        }
    }

    eprintln!("[fc-agent] sent cache-ready:{}, waiting for ack...", digest);

    // Set socket to non-blocking to prevent read() from blocking after restore
    // After snapshot restore, the kernel might think data is available but read() would block
    if let Ok(flags) = fcntl(sock.as_raw_fd(), FcntlArg::F_GETFL) {
        let new_flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
        let _ = fcntl(sock.as_raw_fd(), FcntlArg::F_SETFL(new_flags));
    }

    // Wait for cache-ack response with timeout (handles snapshot restore case)
    // After restore, the vsock connection is dead and poll will timeout
    let mut buf = [0u8; 64];
    let mut total_read = 0;

    loop {
        // Use poll() to check if data is available with timeout
        let mut poll_fds = [PollFd::new(sock.as_fd(), PollFlags::POLLIN)];

        match poll(&mut poll_fds, PollTimeout::from(100u16)) {
            Err(e) => {
                eprintln!("[fc-agent] cache-ack poll error: {}", e);
                return false;
            }
            Ok(0) => {
                // Timeout - likely restored from snapshot with dead connection
                eprintln!("[fc-agent] cache-ack poll timeout (restored from snapshot?)");
                return false;
            }
            Ok(_) => {}
        }

        // Check for hangup/error
        if let Some(revents) = poll_fds[0].revents() {
            if revents.contains(PollFlags::POLLHUP) || revents.contains(PollFlags::POLLERR) {
                eprintln!("[fc-agent] cache-ack connection closed or error");
                return false;
            }
        }

        // Data available (POLLIN set), do non-blocking read
        match read(sock.as_raw_fd(), &mut buf[total_read..]) as Result<usize, nix::errno::Errno> {
            Err(nix::errno::Errno::EAGAIN) => {
                // Non-blocking read returned EAGAIN/EWOULDBLOCK - no data actually available
                // This can happen after snapshot restore when poll returns but data isn't really there
                eprintln!("[fc-agent] cache-ack read would block (likely restored from snapshot)");
                return false;
            }
            Err(e) => {
                eprintln!("[fc-agent] cache-ack read error: {}", e);
                return false;
            }
            Ok(0) => {
                // Connection closed
                eprintln!("[fc-agent] cache-ack connection closed");
                return false;
            }
            Ok(n) => {
                total_read += n;
            }
        }

        // Check if we received "cache-ack\n"
        let received = std::str::from_utf8(&buf[..total_read]).unwrap_or("");
        if received.contains("cache-ack") {
            eprintln!("[fc-agent] ✓ received cache-ack from host");
            return true;
        }

        // Prevent buffer overflow
        if total_read >= buf.len() {
            eprintln!("[fc-agent] cache-ack buffer overflow, giving up");
            return false;
        }
    }
    // sock is automatically closed when dropped
}

/// Shutdown the VM with the given exit code.
///
/// This function handles the shutdown sequence:
/// 1. Sync filesystems
/// 2. Call poweroff/reboot based on architecture
/// 3. Fallback to sysrq if needed
///
/// This function never returns.
async fn shutdown_vm(exit_code: i32) -> ! {
    eprintln!("[fc-agent] shutting down VM (exit_code={})", exit_code);

    // Check what filesystems are mounted and might need syncing
    if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
        let fuse_mounts: Vec<&str> = mounts.lines().filter(|l| l.contains("fuse")).collect();
        if !fuse_mounts.is_empty() {
            eprintln!("[fc-agent] FUSE mounts before shutdown: {:?}", fuse_mounts);
        }
    }

    // Try sync with timeout - spawn it and wait max 2 seconds
    eprintln!("[fc-agent] starting sync...");
    let sync_start = std::time::Instant::now();
    if let Ok(mut sync_child) = Command::new("sync").spawn() {
        // Wait for sync with 2 second timeout
        for _ in 0..20 {
            match sync_child.try_wait() {
                Ok(Some(status)) => {
                    eprintln!(
                        "[fc-agent] sync completed in {:?} with status: {:?}",
                        sync_start.elapsed(),
                        status
                    );
                    break;
                }
                Ok(None) => {
                    sleep(Duration::from_millis(100)).await;
                }
                Err(e) => {
                    eprintln!("[fc-agent] sync wait error: {}", e);
                    break;
                }
            }
        }
        if sync_start.elapsed().as_secs() >= 2 {
            eprintln!("[fc-agent] sync timed out after 2s, killing it");
            let _ = sync_child.kill().await;
        }
    }

    // Now shutdown - method depends on architecture:
    // - ARM64: poweroff -f triggers PSCI SYSTEM_OFF via pm_power_off callback
    // - x86: reboot -f with reboot=t boot param triggers triple-fault (Firecracker has no ACPI)
    #[cfg(target_arch = "aarch64")]
    {
        eprintln!("[fc-agent] calling poweroff -f (PSCI SYSTEM_OFF)...");
        let _ = Command::new("poweroff").args(["-f"]).spawn();
    }
    #[cfg(target_arch = "x86_64")]
    {
        eprintln!("[fc-agent] calling reboot -f (triple-fault via reboot=t)...");
        let _ = Command::new("reboot").args(["-f"]).spawn();
    }

    // Wait for shutdown to take effect
    sleep(Duration::from_secs(2)).await;

    // Fallback: try the other command
    #[cfg(target_arch = "aarch64")]
    {
        eprintln!("[fc-agent] poweroff didn't complete after 2s, trying reboot -f");
        let _ = Command::new("reboot").args(["-f"]).spawn();
    }
    #[cfg(target_arch = "x86_64")]
    {
        eprintln!("[fc-agent] reboot didn't complete after 2s, trying sysrq");
    }

    // Wait a bit more
    sleep(Duration::from_secs(2)).await;
    eprintln!("[fc-agent] shutdown didn't complete, trying sysrq reboot");

    // Last resort: use the reboot syscall directly via sysrq
    let _ = std::fs::write("/proc/sysrq-trigger", "b");

    sleep(Duration::from_secs(1)).await;
    eprintln!("[fc-agent] VM shutdown completely failed!");
    std::process::exit(exit_code)
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

    // Wait for each FUSE mount to become accessible (up to 30s per mount)
    for vol in volumes {
        let path = std::path::Path::new(&vol.guest_path);
        let mut ready = false;
        for attempt in 1..=60 {
            if let Ok(entries) = std::fs::read_dir(path) {
                let count = entries.count();
                eprintln!(
                    "[fc-agent] ✓ mount {} ready ({} entries, {}ms)",
                    vol.guest_path,
                    count,
                    (attempt - 1) * 500
                );
                ready = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        if !ready {
            return Err(anyhow::anyhow!(
                "mount {} not accessible after 30s",
                vol.guest_path
            ));
        }
    }

    Ok(mounted_paths)
}

/// Mount extra block devices at their specified mount paths.
/// Returns list of mount points that need to be cleaned up on exit.
fn mount_extra_disks(disks: &[ExtraDiskMount]) -> Result<Vec<String>> {
    let mut mounted_paths = Vec::new();

    for disk in disks {
        eprintln!(
            "[fc-agent] mounting extra disk {} at {} ({})",
            disk.device,
            disk.mount_path,
            if disk.read_only { "ro" } else { "rw" }
        );

        // Create mount point directory
        if let Err(e) = std::fs::create_dir_all(&disk.mount_path) {
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(e)
                    .with_context(|| format!("creating mount point: {}", disk.mount_path));
            }
        }

        // Wait for device to appear (may take a moment after VM boot)
        let device_path = std::path::Path::new(&disk.device);
        for attempt in 1..=10 {
            if device_path.exists() {
                break;
            }
            if attempt == 10 {
                anyhow::bail!("Device {} not found after 10 attempts", disk.device);
            }
            eprintln!(
                "[fc-agent] waiting for device {} (attempt {}/10)",
                disk.device, attempt
            );
            std::thread::sleep(std::time::Duration::from_millis(500));
        }

        // Mount the block device
        let mut mount_cmd = std::process::Command::new("mount");
        if disk.read_only {
            mount_cmd.arg("-o").arg("ro");
        }
        mount_cmd.arg(&disk.device).arg(&disk.mount_path);

        let output = mount_cmd
            .output()
            .with_context(|| format!("mounting {} at {}", disk.device, disk.mount_path))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "Failed to mount {} at {}: {}",
                disk.device,
                disk.mount_path,
                stderr
            );
        }

        eprintln!(
            "[fc-agent] ✓ extra disk {} mounted at {}",
            disk.device, disk.mount_path
        );
        mounted_paths.push(disk.mount_path.clone());
    }

    Ok(mounted_paths)
}

/// Mount NFS shares from host.
/// Returns list of mount points that need to be cleaned up on exit.
fn mount_nfs_shares(shares: &[NfsMount]) -> Result<Vec<String>> {
    let mut mounted_paths = Vec::new();

    for share in shares {
        let nfs_source = format!("{}:{}", share.host_ip, share.host_path);
        eprintln!(
            "[fc-agent] mounting NFS {} at {} ({})",
            nfs_source,
            share.mount_path,
            if share.read_only { "ro" } else { "rw" }
        );

        // Create mount point directory
        if let Err(e) = std::fs::create_dir_all(&share.mount_path) {
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(e)
                    .with_context(|| format!("creating NFS mount point: {}", share.mount_path));
            }
        }

        // Mount the NFS share
        // Use -o nfsvers=4 for NFS v4, which is more firewall-friendly
        let mut mount_cmd = std::process::Command::new("mount");
        mount_cmd.arg("-t").arg("nfs");

        let opts = if share.read_only {
            "ro,nfsvers=4,nolock"
        } else {
            "rw,nfsvers=4,nolock"
        };
        mount_cmd.arg("-o").arg(opts);
        mount_cmd.arg(&nfs_source).arg(&share.mount_path);

        let output = mount_cmd
            .output()
            .with_context(|| format!("mounting NFS {} at {}", nfs_source, share.mount_path))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "Failed to mount NFS {} at {}: {}",
                nfs_source,
                share.mount_path,
                stderr
            );
        }

        eprintln!(
            "[fc-agent] ✓ NFS {} mounted at {}",
            nfs_source, share.mount_path
        );
        mounted_paths.push(share.mount_path.clone());
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

    // Check for fcvm_dns= boot parameter (host's real DNS servers, pipe-separated)
    // This overrides slirp's 10.0.2.3 for direct DNS resolution on IPv6-only hosts
    let nameservers: Vec<String> = cmdline
        .split_whitespace()
        .find(|s| s.starts_with("fcvm_dns="))
        .map(|s| {
            s.trim_start_matches("fcvm_dns=")
                .split('|')
                .map(|ns| ns.to_string())
                .collect()
        })
        .unwrap_or_else(|| vec![nameserver.to_string()]);

    // Check for fcvm_dns_search= boot parameter (search domains, pipe-separated)
    let search_domains: Option<String> = cmdline
        .split_whitespace()
        .find(|s| s.starts_with("fcvm_dns_search="))
        .map(|s| s.trim_start_matches("fcvm_dns_search=").replace('|', " "));

    // Write to /etc/resolv.conf
    let mut resolv_conf = String::new();
    if let Some(ref search) = search_domains {
        resolv_conf.push_str(&format!("search {}\n", search));
    }
    for ns in &nameservers {
        resolv_conf.push_str(&format!("nameserver {}\n", ns));
    }

    match std::fs::write("/etc/resolv.conf", &resolv_conf) {
        Ok(_) => {
            eprintln!("[fc-agent] ✓ configured DNS: {}", resolv_conf.trim());
        }
        Err(e) => {
            eprintln!(
                "[fc-agent] WARNING: failed to write /etc/resolv.conf: {}",
                e
            );
        }
    }
}

/// Configure IPv6 from kernel boot parameters
/// Parses ipv6= parameter and configures eth0 with the address and route
fn configure_ipv6_from_cmdline() {
    eprintln!("[fc-agent] checking for IPv6 configuration");

    // Read kernel command line
    let cmdline = match std::fs::read_to_string("/proc/cmdline") {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[fc-agent] WARNING: failed to read /proc/cmdline: {}", e);
            return;
        }
    };

    // Find ipv6= parameter
    // Format: ipv6=<client>|<gateway> (using | as delimiter since : is in IPv6 addresses)
    // Example: ipv6=fd00:1::2|fd00:1::1
    let ipv6_param = cmdline
        .split_whitespace()
        .find(|s| s.starts_with("ipv6="))
        .map(|s| s.trim_start_matches("ipv6="));

    let ipv6_param = match ipv6_param {
        Some(p) => p,
        None => {
            eprintln!("[fc-agent] no ipv6= parameter, IPv6 not configured");
            return;
        }
    };
    eprintln!("[fc-agent] ipv6 param: {}", ipv6_param);

    // Parse client|gateway format (| delimiter to avoid conflict with : in IPv6 addresses)
    let parts: Vec<&str> = ipv6_param.split('|').collect();
    if parts.len() != 2 {
        eprintln!("[fc-agent] WARNING: invalid ipv6= format, expected <client>|<gateway>");
        return;
    }
    // Format is client|gateway
    let client = parts[0];
    let gateway = parts[1];

    eprintln!("[fc-agent] IPv6: client={}, gateway={}", client, gateway);

    // Add IPv6 address to eth0
    let addr_output = std::process::Command::new("ip")
        .args([
            "-6",
            "addr",
            "add",
            &format!("{}/64", client),
            "dev",
            "eth0",
        ])
        .output();

    match addr_output {
        Ok(output) if output.status.success() => {
            eprintln!("[fc-agent] ✓ added IPv6 address {}/64 to eth0", client);
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // RTNETLINK: File exists means address is already configured
            if stderr.contains("File exists") {
                eprintln!("[fc-agent] IPv6 address already exists on eth0");
            } else {
                eprintln!("[fc-agent] WARNING: failed to add IPv6 address: {}", stderr);
            }
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: failed to run ip -6 addr add: {}", e);
        }
    }

    // Add IPv6 default route
    let route_output = std::process::Command::new("ip")
        .args([
            "-6", "route", "add", "default", "via", gateway, "dev", "eth0",
        ])
        .output();

    match route_output {
        Ok(output) if output.status.success() => {
            eprintln!("[fc-agent] ✓ added IPv6 default route via {}", gateway);
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // RTNETLINK: File exists means route is already configured
            if stderr.contains("File exists") {
                eprintln!("[fc-agent] IPv6 default route already exists");
            } else {
                eprintln!("[fc-agent] WARNING: failed to add IPv6 route: {}", stderr);
            }
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: failed to run ip -6 route add: {}", e);
        }
    }
}

/// Save proxy settings from the plan to a file so exec commands can use them.
///
/// This is needed because exec commands run via vsock don't have access to the
/// original plan.
const PROXY_SETTINGS_FILE: &str = "/etc/fcvm-proxy.env";

fn save_proxy_settings(plan: &Plan) {
    use std::io::Write as _;

    let mut content = String::new();
    let mut env_vars = Vec::new();

    if let Some(ref proxy) = plan.http_proxy {
        content.push_str(&format!("http_proxy={}\n", proxy));
        content.push_str(&format!("HTTP_PROXY={}\n", proxy));
        env_vars.push(("http_proxy", proxy.clone()));
        env_vars.push(("HTTP_PROXY", proxy.clone()));
    }
    if let Some(ref proxy) = plan.https_proxy {
        content.push_str(&format!("https_proxy={}\n", proxy));
        content.push_str(&format!("HTTPS_PROXY={}\n", proxy));
        env_vars.push(("https_proxy", proxy.clone()));
        env_vars.push(("HTTPS_PROXY", proxy.clone()));
    }
    if let Some(ref no_proxy) = plan.no_proxy {
        content.push_str(&format!("no_proxy={}\n", no_proxy));
        content.push_str(&format!("NO_PROXY={}\n", no_proxy));
        env_vars.push(("no_proxy", no_proxy.clone()));
        env_vars.push(("NO_PROXY", no_proxy.clone()));
    }

    if content.is_empty() {
        eprintln!("[fc-agent] no proxy settings configured");
        return;
    }

    // Set environment variables in current process so child processes (TTY mode) inherit them
    for (key, value) in &env_vars {
        std::env::set_var(key, value);
    }
    eprintln!(
        "[fc-agent] ✓ set {} proxy environment variables",
        env_vars.len()
    );

    // Also save to file for exec handler to read
    match std::fs::File::create(PROXY_SETTINGS_FILE) {
        Ok(mut file) => {
            if let Err(e) = file.write_all(content.as_bytes()) {
                eprintln!("[fc-agent] WARNING: failed to write proxy settings: {}", e);
            } else {
                eprintln!(
                    "[fc-agent] ✓ saved proxy settings to {}",
                    PROXY_SETTINGS_FILE
                );
            }
        }
        Err(e) => {
            eprintln!(
                "[fc-agent] WARNING: failed to create proxy settings file: {}",
                e
            );
        }
    }
}

/// Read proxy settings from the saved file.
/// Returns a Vec of (key, value) pairs.
fn read_proxy_settings() -> Vec<(String, String)> {
    let content = match std::fs::read_to_string(PROXY_SETTINGS_FILE) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    content
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

#[tokio::main]
async fn main() {
    // Initialize tracing (fuse-pipe uses tracing for logging)
    // Disable ANSI codes since output goes through serial console/vsock
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,fuse_pipe=debug")),
        )
        .with_target(true)
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .init();

    eprintln!("[fc-agent] starting");

    // Run the agent and handle any errors by shutting down the VM
    if let Err(e) = run_agent().await {
        eprintln!("[fc-agent] ==========================================");
        eprintln!("[fc-agent] FATAL ERROR: Container failed to start");
        eprintln!("[fc-agent] Error: {:?}", e);
        eprintln!("[fc-agent] ==========================================");
        // Notify host of failure (exit code 1)
        notify_container_exit(1);
        // Shutdown the VM so it doesn't hang indefinitely
        shutdown_vm(1).await;
    }
}

/// Main agent logic - fetches plan, runs container, and triggers shutdown.
async fn run_agent() -> Result<()> {
    eprintln!("[fc-agent] run_agent starting");

    // Raise resource limits early to support high parallelism workloads
    raise_resource_limits();

    // Create /dev/kvm device for nested virtualization support (nested virtualization)
    // This is a no-op if kernel doesn't have CONFIG_KVM
    create_kvm_device();

    // Configure DNS from kernel boot parameters before any network operations
    configure_dns_from_cmdline();

    // Configure IPv6 if specified in kernel parameters
    configure_ipv6_from_cmdline();

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

    // Save proxy settings for exec commands to use
    save_proxy_settings(&plan);

    // Sync VM clock from host before launching container
    // This ensures TLS certificate validation works immediately
    if let Err(e) = sync_clock_from_host().await {
        eprintln!("[fc-agent] WARNING: clock sync failed: {:?}", e);
        eprintln!("[fc-agent] continuing anyway (will rely on chronyd)");
    }

    // Start background task to watch for restore-epoch changes
    // This handles ARP cache flushing when VM is restored from snapshot
    let watcher_volumes = plan.volumes.clone();
    tokio::spawn(async move {
        eprintln!("[fc-agent] starting restore-epoch watcher for ARP flush");
        watch_restore_epoch(watcher_volumes).await;
    });

    // Start exec server to allow host to run commands in VM
    // Use a oneshot channel to wait for the server to be listening before continuing.
    // This ensures health checks (which use fcvm exec) work immediately.
    let (exec_ready_tx, exec_ready_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async {
        run_exec_server_with_ready_signal(exec_ready_tx).await;
    });

    // Wait for exec server to be listening (with timeout to avoid hanging)
    match tokio::time::timeout(Duration::from_secs(5), exec_ready_rx).await {
        Ok(Ok(())) => {
            eprintln!("[fc-agent] exec server is ready");
        }
        Ok(Err(_)) => {
            eprintln!("[fc-agent] WARNING: exec server ready signal dropped");
        }
        Err(_) => {
            eprintln!("[fc-agent] WARNING: exec server did not become ready within 5s");
        }
    }

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

    // Mount extra block devices before launching container
    let mounted_disk_paths: Vec<String> = if !plan.extra_disks.is_empty() {
        eprintln!(
            "[fc-agent] mounting {} extra disk(s)",
            plan.extra_disks.len()
        );
        match mount_extra_disks(&plan.extra_disks) {
            Ok(paths) => {
                eprintln!("[fc-agent] ✓ extra disks mounted successfully");
                paths
            }
            Err(e) => {
                eprintln!("[fc-agent] ERROR: failed to mount extra disks: {:?}", e);
                // Continue without extra disks - container can still run
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    // Mount NFS shares from host before launching container
    let _mounted_nfs_paths: Vec<String> = if !plan.nfs_mounts.is_empty() {
        eprintln!("[fc-agent] mounting {} NFS share(s)", plan.nfs_mounts.len());
        match mount_nfs_shares(&plan.nfs_mounts) {
            Ok(paths) => {
                eprintln!("[fc-agent] ✓ NFS shares mounted successfully");
                paths
            }
            Err(e) => {
                eprintln!("[fc-agent] ERROR: failed to mount NFS shares: {:?}", e);
                // Continue without NFS - container can still run
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

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
    // If image_archive is set, import into podman storage first (so snapshot captures it)
    // Otherwise, pull from registry
    let image_ref = if let Some(archive_path) = &plan.image_archive {
        eprintln!("[fc-agent] importing Docker archive: {}", archive_path);

        // Import into podman storage so the pre-start snapshot captures the loaded image.
        // Without this, every snapshot restore would re-read the entire tar from /dev/vdb.
        let output = Command::new("podman")
            .args(["load", "-i", archive_path])
            .output()
            .await
            .context("running podman load")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("podman load failed: {}", stderr);
        }

        let loaded_output = String::from_utf8_lossy(&output.stdout);
        eprintln!("[fc-agent] podman load: {}", loaded_output.trim());
        eprintln!("[fc-agent] ✓ image imported as: {}", plan.image);
        plan.image.clone()
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
            let mut cmd = Command::new("podman");
            cmd.arg("pull").arg(&plan.image);
            // Pass proxy environment variables if configured
            if let Some(ref proxy) = plan.http_proxy {
                cmd.env("http_proxy", proxy);
                cmd.env("HTTP_PROXY", proxy);
            }
            if let Some(ref proxy) = plan.https_proxy {
                cmd.env("https_proxy", proxy);
                cmd.env("HTTPS_PROXY", proxy);
            }
            if let Some(ref no_proxy) = plan.no_proxy {
                cmd.env("no_proxy", no_proxy);
                cmd.env("NO_PROXY", no_proxy);
            }
            let mut child = cmd
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

    // Notify host that image is ready for caching
    // Image is always in podman storage at this point (pulled or loaded from archive)
    match get_image_digest(&image_ref).await {
        Ok(digest) => {
            eprintln!("[fc-agent] image digest: {}", digest);
            if notify_cache_ready_and_wait(&digest) {
                eprintln!("[fc-agent] ✓ cache ready notification acknowledged");
            } else {
                eprintln!("[fc-agent] WARNING: cache-ready handshake failed, continuing anyway");
            }
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: failed to get image digest: {:?}", e);
            eprintln!("[fc-agent] continuing without cache notification");
        }
    }

    // After cache-ready handshake, Firecracker may have created a pre-start snapshot.
    // Snapshot creation resets all vsock connections (VIRTIO_VSOCK_EVENT_TRANSPORT_RESET),
    // which breaks FUSE mounts. Check if mounts are still healthy and remount if needed.
    if !mounted_fuse_paths.is_empty() {
        let mut broken = false;
        for path in &mounted_fuse_paths {
            if std::fs::metadata(path).is_err() {
                eprintln!(
                    "[fc-agent] FUSE mount at {} broken after snapshot (vsock reset), will remount",
                    path
                );
                broken = true;
                break;
            }
        }
        if broken {
            remount_fuse_volumes(&plan.volumes).await;
        }
    }

    eprintln!("[fc-agent] launching container: {}", image_ref);

    // Wait for cgroup controllers to be available.
    // With Delegate=yes on fc-agent.service, systemd should delegate controllers to our cgroup.
    // But there can be a race condition where we start before delegation is complete.
    // This is especially important for --cgroups=split which uses cgroupfs directly.
    wait_for_cgroup_controllers().await;

    // Build Podman args (used for both TTY and non-TTY modes)
    //
    // CRITICAL: --cgroups=split is REQUIRED for snapshot restore.
    //
    // Root cause: After Firecracker snapshot restore, systemd's timer/event loop
    // gets confused because the VM clock jumps from snapshot-time to current-time.
    // This is a known class of issues (see systemd/systemd#23032) where:
    //   1. systemd's CLOCK_REALTIME-based timers break on clock discontinuity
    //   2. epoll_wait() and timerfd handling in the event loop become confused
    //   3. The watchdog/keepalive pings don't get sent correctly
    //   4. D-Bus calls to systemd time out waiting for responses
    //
    // When crun tries to create cgroups via sd-bus, systemd never responds:
    //   "OCI runtime error: crun: sd-bus call: Connection timed out"
    //
    // Solution: --cgroups=split uses cgroups v2 directly via cgroupfs, bypassing
    // the broken D-Bus/systemd communication. This gives us:
    // - Full cgroup v2 functionality (resource limits, isolation)
    // - No dependency on systemd's event loop
    let mut podman_args = vec![
        "podman".to_string(),
        "run".to_string(),
        "--rm".to_string(),
        "--name".to_string(),
        "fcvm-container".to_string(),
        "--network=host".to_string(),
        "--cgroups=split".to_string(),
        "--ulimit".to_string(),
        "nofile=65536:65536".to_string(),
    ];

    // Privileged mode: allows mknod, device access, etc. for POSIX compliance tests
    if plan.privileged {
        eprintln!("[fc-agent] privileged mode enabled");
        podman_args.push("--device-cgroup-rule=b *:* rwm".to_string());
        podman_args.push("--device-cgroup-rule=c *:* rwm".to_string());
        podman_args.push("--privileged".to_string());
    }

    // Interactive/TTY modes
    if plan.interactive {
        podman_args.push("-i".to_string());
    }
    if plan.tty {
        podman_args.push("-t".to_string());
    }

    // Add environment variables
    for (key, val) in &plan.env {
        podman_args.push("-e".to_string());
        podman_args.push(format!("{}={}", key, val));
    }

    // Add FUSE-mounted volumes as bind mounts to container
    for vol in &plan.volumes {
        let mount_spec = if vol.read_only {
            format!("{}:{}:ro", vol.guest_path, vol.guest_path)
        } else {
            format!("{}:{}", vol.guest_path, vol.guest_path)
        };
        podman_args.push("-v".to_string());
        podman_args.push(mount_spec);
    }

    // Add extra disk mounts as bind mounts to container
    for disk in &plan.extra_disks {
        let mount_spec = if disk.read_only {
            format!("{}:{}:ro", disk.mount_path, disk.mount_path)
        } else {
            format!("{}:{}", disk.mount_path, disk.mount_path)
        };
        podman_args.push("-v".to_string());
        podman_args.push(mount_spec);
    }

    // Add NFS mounts as bind mounts to container
    for share in &plan.nfs_mounts {
        let mount_spec = if share.read_only {
            format!("{}:{}:ro", share.mount_path, share.mount_path)
        } else {
            format!("{}:{}", share.mount_path, share.mount_path)
        };
        podman_args.push("-v".to_string());
        podman_args.push(mount_spec);
    }

    // Image name (from registry pull or archive load)
    podman_args.push(image_ref.clone());

    // Command override
    if let Some(cmd_args) = &plan.cmd {
        podman_args.extend(cmd_args.iter().cloned());
    }

    // TTY mode: use PTY and binary protocol (blocking, not async)
    if plan.tty {
        eprintln!("[fc-agent] TTY mode enabled, using PTY");

        // Notify host that container is starting
        notify_container_started();

        // Run container with TTY (blocks until container exits)
        let exit_code = tty::run_with_pty(&podman_args, plan.tty, plan.interactive);

        // Notify host of container exit
        notify_container_exit(exit_code);

        // Unmount FUSE volumes before shutdown
        if !mounted_fuse_paths.is_empty() {
            eprintln!(
                "[fc-agent] unmounting {} FUSE volume(s) before shutdown",
                mounted_fuse_paths.len()
            );
            for path in &mounted_fuse_paths {
                eprintln!("[fc-agent] unmounting FUSE volume at {}", path);
                let _ = std::process::Command::new("umount")
                    .arg("-l")
                    .arg(path)
                    .output();
            }
        }

        // Power off the VM
        eprintln!("[fc-agent] powering off VM");
        let _ = std::process::Command::new("poweroff").arg("-f").spawn();

        // Exit with container's exit code
        std::process::exit(exit_code);
    }

    // Non-TTY mode: use async I/O with line-based protocol
    let mut cmd = Command::new(&podman_args[0]);
    cmd.args(&podman_args[1..]);

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

    // Unmount extra disks before shutting down
    if !mounted_disk_paths.is_empty() {
        eprintln!(
            "[fc-agent] unmounting {} extra disk(s) before shutdown",
            mounted_disk_paths.len()
        );
        for path in &mounted_disk_paths {
            eprintln!("[fc-agent] unmounting extra disk at {}", path);
            match std::process::Command::new("umount").arg(path).output() {
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
    }

    // Shut down the VM when the container exits (success or failure)
    // This is the expected behavior - the VM exists to run one container
    shutdown_vm(exit_code).await
}
