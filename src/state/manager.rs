use anyhow::{Context, Result};
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use tokio::fs;

use super::types::VmState;

/// Check if a port is available on a given IP address.
/// This is used during loopback IP allocation to skip IPs that have
/// stale processes (e.g., orphaned slirp4netns) still holding ports.
fn is_port_available(ip: &str, port: u16) -> bool {
    let addr: SocketAddr = match format!("{}:{}", ip, port).parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    // Try to bind - if it succeeds, port is available
    // The TcpListener is dropped immediately, releasing the port
    TcpListener::bind(addr).is_ok()
}

/// Manages VM state persistence
///
/// PID Tracking Note:
/// The `pid` field in VmState stores the fcvm process PID (from std::process::id()),
/// NOT the Firecracker child process PID. This allows external tools and monitors
/// to track the fcvm management process that controls the VM lifecycle.
pub struct StateManager {
    state_dir: PathBuf,
}

impl StateManager {
    pub fn new(state_dir: PathBuf) -> Self {
        Self { state_dir }
    }

    /// Initialize state directory
    pub async fn init(&self) -> Result<()> {
        fs::create_dir_all(&self.state_dir)
            .await
            .context("creating state directory")?;
        Ok(())
    }

    /// Save VM state atomically (write to temp file, then rename)
    /// Uses file locking to prevent concurrent writes
    ///
    /// If another state file claims our PID, it's stale (that process is dead
    /// and its PID was reused by the OS). We delete it to prevent collisions
    /// when querying by PID.
    pub async fn save_state(&self, state: &VmState) -> Result<()> {
        tracing::debug!(
            vm_id = %state.vm_id,
            pid = ?state.pid,
            state_dir = %self.state_dir.display(),
            "save_state: starting save"
        );

        // Clean up any stale state files that claim our PID
        // This happens when a VM crashes and its PID is later reused
        if let Some(pid) = state.pid {
            if let Ok(existing_vms) = self.list_vms().await {
                for existing in existing_vms {
                    if existing.pid == Some(pid) && existing.vm_id != state.vm_id {
                        tracing::warn!(
                            stale_vm_id = %existing.vm_id,
                            pid = pid,
                            "deleting stale state file with reused PID (previous VM crashed without cleanup)"
                        );
                        let _ = self.delete_state(&existing.vm_id).await;
                    }
                }
            }
        }

        let state_file = self.state_dir.join(format!("{}.json", state.vm_id));
        let temp_file = self.state_dir.join(format!("{}.json.tmp", state.vm_id));
        let lock_file = self.state_dir.join(format!("{}.json.lock", state.vm_id));

        // Create/open lock file for exclusive locking
        use std::os::unix::fs::OpenOptionsExt;
        let lock_fd = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&lock_file)
            .context("opening lock file")?;

        // Acquire exclusive lock (blocks if another process has lock)
        use nix::fcntl::{Flock, FlockArg};
        let flock = Flock::lock(lock_fd, FlockArg::LockExclusive)
            .map_err(|(_, err)| err)
            .context("acquiring exclusive lock on state file")?;

        // Now we have exclusive access, perform the write
        let result = async {
            // Update last_updated timestamp before saving
            let mut state = state.clone();
            state.last_updated = chrono::Utc::now();

            let state_json = serde_json::to_string_pretty(&state)?;

            // Write to temp file first
            fs::write(&temp_file, &state_json)
                .await
                .context("writing temp state file")?;

            // Set file permissions to 0600 (owner read/write only) for security
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let permissions = std::fs::Permissions::from_mode(0o600);
                tokio::fs::set_permissions(&temp_file, permissions)
                    .await
                    .context("setting file permissions on state file")?;
            }

            // Atomic rename (this is an atomic operation on Unix)
            fs::rename(&temp_file, &state_file)
                .await
                .context("renaming temp state file")?;

            tracing::debug!(
                vm_id = %state.vm_id,
                pid = ?state.pid,
                path = %state_file.display(),
                "save_state: successfully saved state"
            );

            Ok::<(), anyhow::Error>(())
        }
        .await;

        // Release lock (happens automatically when flock is dropped, but being explicit)
        // NOTE: We intentionally do NOT delete lock files - see allocate_loopback_ip comment
        flock
            .unlock()
            .map_err(|(_, err)| err)
            .context("releasing lock on state file")?;

        result
    }

    /// Load VM state
    pub async fn load_state(&self, vm_id: &str) -> Result<VmState> {
        let state_file = self.state_dir.join(format!("{}.json", vm_id));
        let state_json = fs::read_to_string(&state_file)
            .await
            .context("reading VM state")?;
        let state: VmState = serde_json::from_str(&state_json).context("parsing VM state")?;
        Ok(state)
    }

    /// Delete VM state and associated lock/temp files
    pub async fn delete_state(&self, vm_id: &str) -> Result<()> {
        let state_file = self.state_dir.join(format!("{}.json", vm_id));
        let lock_file = self.state_dir.join(format!("{}.json.lock", vm_id));
        let temp_file = self.state_dir.join(format!("{}.json.tmp", vm_id));

        tracing::debug!(
            vm_id = vm_id,
            path = %state_file.display(),
            "delete_state: deleting state file"
        );

        // Delete state file - ignore NotFound (TOCTOU race / concurrent cleanup)
        match fs::remove_file(&state_file).await {
            Ok(()) => {
                tracing::debug!(
                    vm_id = vm_id,
                    path = %state_file.display(),
                    "delete_state: successfully deleted state file"
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(
                    vm_id = vm_id,
                    path = %state_file.display(),
                    "delete_state: state file already gone (NotFound)"
                );
            }
            Err(e) => return Err(e).context("deleting VM state"),
        }

        // Clean up lock file (ignore errors - may not exist or be held by another process)
        let _ = fs::remove_file(&lock_file).await;

        // Clean up temp file (ignore errors - may not exist)
        let _ = fs::remove_file(&temp_file).await;

        Ok(())
    }

    /// Clean up stale state files from processes that no longer exist.
    ///
    /// This frees up loopback IPs that were allocated but not properly cleaned up
    /// (e.g., due to crashes or SIGKILL). Called lazily during IP allocation.
    async fn cleanup_stale_state(&self) {
        tracing::debug!(
            state_dir = %self.state_dir.display(),
            "cleanup_stale_state: starting scan"
        );

        let entries = match std::fs::read_dir(&self.state_dir) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::debug!(
                    state_dir = %self.state_dir.display(),
                    error = %e,
                    "cleanup_stale_state: failed to read directory"
                );
                return;
            }
        };

        let mut examined = 0;
        let mut removed = 0;

        for entry in entries.flatten() {
            let path = entry.path();

            // Only process .json files
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                // Read the state file to get the PID
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(state) = serde_json::from_str::<serde_json::Value>(&content) {
                        if let Some(pid) = state.get("pid").and_then(|p| p.as_u64()) {
                            // Check if process exists
                            let proc_path = format!("/proc/{}", pid);
                            let proc_exists = std::path::Path::new(&proc_path).exists();

                            examined += 1;
                            tracing::trace!(
                                pid = pid,
                                path = %path.display(),
                                proc_exists = proc_exists,
                                "cleanup_stale_state: examined state file"
                            );

                            if !proc_exists {
                                // Process doesn't exist - remove stale state
                                tracing::warn!(
                                    pid = pid,
                                    path = %path.display(),
                                    "cleanup_stale_state: removing state file for dead process"
                                );
                                let _ = std::fs::remove_file(&path);
                                // Also remove lock file if exists
                                let lock_path = path.with_extension("json.lock");
                                let _ = std::fs::remove_file(&lock_path);
                                removed += 1;
                            }
                        }
                    }
                }
            }
        }

        tracing::debug!(
            examined = examined,
            removed = removed,
            "cleanup_stale_state: scan complete"
        );
    }

    /// Load VM state by name
    pub async fn load_state_by_name(&self, name: &str) -> Result<VmState> {
        let vms = self.list_vms().await?;
        vms.into_iter()
            .find(|vm| vm.name.as_deref() == Some(name))
            .ok_or_else(|| anyhow::anyhow!("VM not found: {}", name))
    }

    /// Load VM state by PID
    pub async fn load_state_by_pid(&self, pid: u32) -> Result<VmState> {
        tracing::debug!(pid = pid, "load_state_by_pid: searching for VM");

        let vms = self.list_vms().await?;
        let vm_count = vms.len();

        tracing::debug!(
            pid = pid,
            vm_count = vm_count,
            "load_state_by_pid: found {} VMs to search",
            vm_count
        );

        // Log each VM we're checking
        for vm in &vms {
            tracing::trace!(
                search_pid = pid,
                vm_pid = ?vm.pid,
                vm_id = %vm.vm_id,
                vm_name = ?vm.name,
                "load_state_by_pid: checking VM"
            );
        }

        match vms.into_iter().find(|vm| vm.pid == Some(pid)) {
            Some(vm) => {
                tracing::debug!(
                    pid = pid,
                    vm_id = %vm.vm_id,
                    vm_name = ?vm.name,
                    "load_state_by_pid: found matching VM"
                );
                Ok(vm)
            }
            None => {
                // Log all available PIDs to help debug
                let available_pids: Vec<u32> = self
                    .list_vms()
                    .await
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|v| v.pid)
                    .collect();

                tracing::error!(
                    search_pid = pid,
                    available_pids = ?available_pids,
                    state_dir = %self.state_dir.display(),
                    "load_state_by_pid: VM not found - no state file has this PID"
                );
                Err(anyhow::anyhow!("No VM found with PID: {}", pid))
            }
        }
    }

    /// List all VMs
    pub async fn list_vms(&self) -> Result<Vec<VmState>> {
        let mut vms = Vec::new();

        if !self.state_dir.exists() {
            tracing::trace!(
                state_dir = %self.state_dir.display(),
                "list_vms: state directory does not exist"
            );
            return Ok(vms);
        }

        tracing::trace!(
            state_dir = %self.state_dir.display(),
            "list_vms: scanning directory"
        );

        let mut entries = fs::read_dir(&self.state_dir)
            .await
            .context("reading state directory")?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                tracing::trace!(
                    path = %path.display(),
                    "list_vms: reading state file"
                );

                match fs::read_to_string(&path).await {
                    Ok(state_json) => match serde_json::from_str::<VmState>(&state_json) {
                        Ok(state) => {
                            tracing::trace!(
                                path = %path.display(),
                                vm_id = %state.vm_id,
                                pid = ?state.pid,
                                "list_vms: parsed state file"
                            );
                            vms.push(state);
                        }
                        Err(e) => {
                            tracing::warn!(
                                path = %path.display(),
                                error = %e,
                                "list_vms: failed to parse state file"
                            );
                        }
                    },
                    Err(e) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "list_vms: failed to read state file"
                        );
                    }
                }
            }
        }

        tracing::trace!(vm_count = vms.len(), "list_vms: scan complete");

        Ok(vms)
    }

    /// Update health status atomically by holding lock across read-modify-write.
    ///
    /// This prevents the race condition where concurrent health monitor updates
    /// could overwrite each other's changes. The lock is held from load through save.
    ///
    /// # Arguments
    /// * `vm_id` - VM identifier
    /// * `health_status` - New health status to set
    /// * `exit_code` - Optional exit code (for Stopped status)
    ///
    /// # Returns
    /// The previous health status before update, or None if state didn't exist
    pub async fn update_health_status(
        &self,
        vm_id: &str,
        health_status: super::HealthStatus,
        exit_code: Option<i32>,
    ) -> Result<Option<super::HealthStatus>> {
        let state_file = self.state_dir.join(format!("{}.json", vm_id));
        let temp_file = self.state_dir.join(format!("{}.json.tmp", vm_id));
        let lock_file = self.state_dir.join(format!("{}.json.lock", vm_id));

        // Create/open lock file for exclusive locking
        use std::os::unix::fs::OpenOptionsExt;
        let lock_fd = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&lock_file)
            .context("opening lock file for health update")?;

        // Acquire exclusive lock (blocks if another process has lock)
        use nix::fcntl::{Flock, FlockArg};
        let flock = Flock::lock(lock_fd, FlockArg::LockExclusive)
            .map_err(|(_, err)| err)
            .context("acquiring exclusive lock for health update")?;

        // CRITICAL: Hold lock across entire read-modify-write
        let result: Result<Option<super::HealthStatus>> = async {
            // Load current state
            let state_json = fs::read_to_string(&state_file)
                .await
                .context("reading VM state for health update")?;
            let mut state: VmState =
                serde_json::from_str(&state_json).context("parsing VM state for health update")?;

            // Capture previous status
            let previous_status = state.health_status;

            // Modify state
            state.health_status = health_status;
            if exit_code.is_some() {
                state.exit_code = exit_code;
            }
            state.last_updated = chrono::Utc::now();

            // Write to temp file
            let state_json = serde_json::to_string_pretty(&state)?;
            fs::write(&temp_file, &state_json)
                .await
                .context("writing temp state file for health update")?;

            // Set permissions
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let permissions = std::fs::Permissions::from_mode(0o600);
                tokio::fs::set_permissions(&temp_file, permissions)
                    .await
                    .context("setting file permissions on state file")?;
            }

            // Atomic rename
            fs::rename(&temp_file, &state_file)
                .await
                .context("renaming temp state file for health update")?;

            Ok(Some(previous_status))
        }
        .await;

        // Release lock (held until this point)
        // NOTE: We intentionally do NOT delete lock files - see allocate_loopback_ip comment
        flock
            .unlock()
            .map_err(|(_, err)| err)
            .context("releasing lock after health update")?;

        result
    }

    /// Allocate a unique loopback IP for rootless networking and persist it atomically
    ///
    /// Uses a global lock file to ensure atomic allocation across concurrent VM starts.
    /// The VM state is saved with the allocated IP WHILE HOLDING THE LOCK, ensuring
    /// no race conditions - no other process can allocate the same IP.
    ///
    /// Returns an IP in the 127.0.0.2 - 127.255.255.254 range.
    ///
    /// # Arguments
    /// * `vm_state` - The VM state to update and persist with the allocated IP
    pub async fn allocate_loopback_ip(&self, vm_state: &mut VmState) -> Result<String> {
        use std::collections::HashSet;

        let lock_file = self.state_dir.join("loopback-ip.lock");

        // Create/open lock file for exclusive locking
        use std::os::unix::fs::OpenOptionsExt;
        let lock_fd = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&lock_file)
            .context("opening loopback IP lock file")?;

        // Acquire exclusive lock (blocks if another process has lock)
        use nix::fcntl::{Flock, FlockArg};
        let flock = Flock::lock(lock_fd, FlockArg::LockExclusive)
            .map_err(|(_, err)| err)
            .context("acquiring exclusive lock for loopback IP allocation")?;

        // Lazily clean up stale state files from dead processes
        // This frees up loopback IPs that were allocated but not properly cleaned up
        self.cleanup_stale_state().await;

        // Collect IPs from all VM state files
        let used_ips: HashSet<String> = match self.list_vms().await {
            Ok(vms) => vms
                .into_iter()
                .filter_map(|vm| vm.config.network.loopback_ip)
                .collect(),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "failed to list VMs for loopback IP allocation, assuming no IPs in use"
                );
                HashSet::new()
            }
        };

        // Sequential allocation: 127.0.0.2, 127.0.0.3, ... 127.0.0.254
        // Then 127.0.1.2, 127.0.1.3, ... etc.
        // Also check that port 8080 is not already bound (handles stale slirp4netns processes)
        let ip = (|| {
            for b2 in 0..=255u8 {
                for b3 in 2..=254u8 {
                    // Skip 127.0.0.1 (localhost)
                    let ip = format!("127.0.{}.{}", b2, b3);
                    if !used_ips.contains(&ip) && is_port_available(&ip, 8080) {
                        return ip;
                    }
                }
            }
            // Fallback if all IPs are used (very unlikely - 65,000+ IPs)
            tracing::warn!("all loopback IPs in use, reusing 127.0.0.2");
            "127.0.0.2".to_string()
        })();

        // Update VM state with the allocated IP and SAVE WHILE HOLDING THE LOCK
        // This ensures no other process can allocate the same IP
        vm_state.config.network.loopback_ip = Some(ip.clone());
        self.save_state(vm_state).await?;

        // Release lock (only after state is persisted)
        // NOTE: We intentionally do NOT delete the lock file - deleting it creates a race
        // condition where another process could create a new file (different inode) and
        // acquire a lock on it while we still hold the original lock.
        flock
            .unlock()
            .map_err(|(_, err)| err)
            .context("releasing loopback IP lock")?;

        Ok(ip)
    }
}

// StateManager tests moved to tests/test_state_integration.rs for better integration testing
