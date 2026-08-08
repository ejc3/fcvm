use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::fs;

use super::types::VmState;

/// Open or create a lock file with world-readable/writable permissions.
/// Uses fchmod after creation to ensure permissions are set regardless of umask.
/// This allows both root and non-root processes to coordinate via flock.
fn open_lock_file(path: &Path) -> Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o666)
        .open(path)?;
    // Force permissions regardless of umask (only effective if we own the file or are root)
    let _ = file.set_permissions(std::fs::Permissions::from_mode(0o666));
    Ok(file)
}

use crate::utils::process_start_time;

/// Check whether the process recorded in a state file is still the same
/// process that wrote it. Returns false when the state records a start time
/// and the process currently at that PID has a different one (PID reuse by an
/// unrelated process). States without a recorded start time fall back to a
/// PID-only match.
fn pid_identity_matches(state: &VmState) -> bool {
    match (state.pid, state.pid_start_time) {
        (Some(pid), Some(recorded)) => process_start_time(pid) == Some(recorded),
        _ => true,
    }
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
    /// This overwrites the entire state file with the caller's copy. It is
    /// intended for initial creation and for saves where the in-memory state
    /// is authoritative (single writer). To change individual fields after the
    /// VM is running (when the health monitor or another process may also be
    /// writing), use `update_state` so concurrent updates are not clobbered.
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
        let lock_fd = open_lock_file(&lock_file).context("opening lock file")?;

        // Acquire exclusive lock (blocks if another process has lock).
        // NOTE: Flock::lock() is technically blocking I/O in an async context, but
        // the lock is held for microseconds with near-zero contention (only this
        // process writes its own state file). Using spawn_blocking would add more
        // overhead than the lock itself. If contention becomes an issue, switch to
        // FlockArg::LockExclusiveNonblock with retry + tokio::task::yield_now().
        use nix::fcntl::{Flock, FlockArg};
        let flock = Flock::lock(lock_fd, FlockArg::LockExclusive)
            .map_err(|(_, err)| err)
            .context("acquiring exclusive lock on state file")?;

        // Now we have exclusive access, perform the write
        let result = async {
            // Update last_updated timestamp before saving
            let mut state = state.clone();
            state.last_updated = chrono::Utc::now();
            // Record the start time of the process at `pid` so later lookups
            // can detect PID reuse by an unrelated process (see pid_identity_matches).
            state.pid_start_time = state.pid.and_then(process_start_time);

            let state_json = serde_json::to_string_pretty(&state)?;

            // Write to temp file first
            fs::write(&temp_file, &state_json)
                .await
                .context("writing temp state file")?;

            // Set file permissions to 0644 (world-readable) so non-root can list VMs
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let permissions = std::fs::Permissions::from_mode(0o644);
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
    ///
    /// Holds the same per-VM lock used by `save_state`/`update_state` while
    /// removing the state file. A concurrent locked read-modify-write (e.g.
    /// the health monitor's `update_health_status`) therefore either completes
    /// before the deletion (and its result is removed), or acquires the lock
    /// afterwards, finds no state file, and becomes a no-op — it can never
    /// resurrect the state file of a deleted VM.
    pub async fn delete_state(&self, vm_id: &str) -> Result<()> {
        let state_file = self.state_dir.join(format!("{}.json", vm_id));
        let lock_file = self.state_dir.join(format!("{}.json.lock", vm_id));
        let temp_file = self.state_dir.join(format!("{}.json.tmp", vm_id));

        tracing::debug!(
            vm_id = vm_id,
            path = %state_file.display(),
            "delete_state: deleting state file"
        );

        // Acquire the per-VM lock so deletion is serialized against in-flight
        // locked writes (save_state / update_state / update_health_status).
        let lock_fd = open_lock_file(&lock_file).context("opening lock file for state delete")?;
        use nix::fcntl::{Flock, FlockArg};
        let flock = Flock::lock(lock_fd, FlockArg::LockExclusive)
            .map_err(|(_, err)| err)
            .context("acquiring exclusive lock for state delete")?;

        let result = async {
            // Delete state file - ignore NotFound (concurrent cleanup)
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

            // Clean up temp file while still holding the lock (ignore errors - may not exist)
            let _ = fs::remove_file(&temp_file).await;

            Ok(())
        }
        .await;

        flock
            .unlock()
            .map_err(|(_, err)| err)
            .context("releasing lock after state delete")?;

        // Remove the lock file only after the state file is gone. A writer that
        // creates a fresh lock file after this point finds no state file and
        // does nothing (update_state treats a missing file as a no-op), so
        // there is no window where two lock-file inodes guard live state.
        if result.is_ok() {
            let _ = fs::remove_file(&lock_file).await;
        }

        result
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

                            // Even if /proc/<pid> exists, the PID may have been
                            // reused by an unrelated process. Compare the recorded
                            // process start time (if any) with the current one.
                            let recorded_start_time =
                                state.get("pid_start_time").and_then(|t| t.as_u64());
                            let identity_matches = match recorded_start_time {
                                Some(recorded) => process_start_time(pid as u32) == Some(recorded),
                                None => true,
                            };

                            examined += 1;
                            tracing::trace!(
                                pid = pid,
                                path = %path.display(),
                                proc_exists = proc_exists,
                                identity_matches = identity_matches,
                                "cleanup_stale_state: examined state file"
                            );

                            if !proc_exists || !identity_matches {
                                // Process doesn't exist (or PID was reused by an
                                // unrelated process) - remove stale state
                                tracing::warn!(
                                    pid = pid,
                                    path = %path.display(),
                                    proc_exists = proc_exists,
                                    "cleanup_stale_state: removing state file for dead or replaced process"
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
        let matches: Vec<_> = vms
            .into_iter()
            .filter(|vm| vm.name.as_deref() == Some(name))
            .collect();
        match matches.len() {
            0 => anyhow::bail!("VM not found: {}", name),
            1 => Ok(matches.into_iter().next().unwrap()),
            n => {
                let pids: Vec<String> = matches
                    .iter()
                    .map(|vm| {
                        vm.pid
                            .map(|p| p.to_string())
                            .unwrap_or_else(|| "none".to_string())
                    })
                    .collect();
                anyhow::bail!(
                    "Multiple VMs named '{}' ({}). Use --pid to specify: {}",
                    name,
                    n,
                    pids.join(", ")
                )
            }
        }
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

        // A state file matching the PID is only trusted if the process at that
        // PID is still the process that wrote it (start time matches). A stale
        // file whose PID was reused by an unrelated process is skipped here and
        // removed by cleanup_stale_state below.
        if let Some(vm) = vms
            .into_iter()
            .find(|vm| vm.pid == Some(pid) && pid_identity_matches(vm))
        {
            tracing::debug!(
                pid = pid,
                vm_id = %vm.vm_id,
                vm_name = ?vm.name,
                "load_state_by_pid: found matching VM"
            );
            return Ok(vm);
        }

        // PID not found. Clean stale state files (dead or replaced PIDs) and
        // retry once. Stale files from killed VMs can shadow the target if the
        // stale PID was reused by the OS — save_state deletes the collision,
        // but cleanup_stale_state handles the general case.
        self.cleanup_stale_state().await;
        let vms = self.list_vms().await?;
        let available_pids: Vec<u32> = vms.iter().filter_map(|v| v.pid).collect();
        if let Some(vm) = vms
            .into_iter()
            .find(|vm| vm.pid == Some(pid) && pid_identity_matches(vm))
        {
            tracing::debug!(
                pid = pid,
                vm_id = %vm.vm_id,
                "load_state_by_pid: found VM after stale cleanup"
            );
            return Ok(vm);
        }

        // Still not found after cleanup

        tracing::error!(
            search_pid = pid,
            available_pids = ?available_pids,
            state_dir = %self.state_dir.display(),
            "load_state_by_pid: VM not found - no state file has this PID"
        );
        Err(anyhow::anyhow!("No VM found with PID: {}", pid))
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

    /// Update VM state atomically by holding the per-VM lock across read-modify-write.
    ///
    /// Loads the current on-disk state, applies `mutate`, and writes the result
    /// back while holding the per-VM flock for the entire operation. This is the
    /// safe way to change individual fields once a VM is running: a whole-state
    /// `save_state` from a stale in-memory copy would silently revert fields
    /// written by other tasks/processes (e.g. the health monitor) in the meantime.
    ///
    /// Returns `Ok(None)` without writing anything if the state file does not
    /// exist (e.g. the VM was deleted concurrently by `delete_state`), so a
    /// late update cannot resurrect a deleted VM's state file.
    pub async fn update_state<F>(&self, vm_id: &str, mutate: F) -> Result<Option<VmState>>
    where
        F: FnOnce(&mut VmState),
    {
        let state_file = self.state_dir.join(format!("{}.json", vm_id));
        let temp_file = self.state_dir.join(format!("{}.json.tmp", vm_id));
        let lock_file = self.state_dir.join(format!("{}.json.lock", vm_id));

        // Fast path: if the state file is already gone (VM deleted), don't
        // recreate a lock file just to discover that under the lock.
        if !state_file.exists() {
            tracing::debug!(
                vm_id = vm_id,
                path = %state_file.display(),
                "update_state: state file does not exist, skipping update"
            );
            return Ok(None);
        }

        // Create/open lock file for exclusive locking
        let lock_fd = open_lock_file(&lock_file).context("opening lock file for state update")?;

        // Acquire exclusive lock (blocks if another process has lock)
        use nix::fcntl::{Flock, FlockArg};
        let flock = Flock::lock(lock_fd, FlockArg::LockExclusive)
            .map_err(|(_, err)| err)
            .context("acquiring exclusive lock for state update")?;

        // CRITICAL: Hold lock across entire read-modify-write
        let result: Result<Option<VmState>> = async {
            // Load current state. The file may have been deleted while we
            // waited for the lock (delete_state holds the same lock) — treat
            // that as "nothing to update" rather than recreating it.
            let state_json = match fs::read_to_string(&state_file).await {
                Ok(json) => json,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    tracing::debug!(
                        vm_id = vm_id,
                        path = %state_file.display(),
                        "update_state: state file deleted concurrently, skipping update"
                    );
                    return Ok(None);
                }
                Err(e) => return Err(e).context("reading VM state for update"),
            };
            let mut state: VmState =
                serde_json::from_str(&state_json).context("parsing VM state for update")?;

            // Apply caller's modification
            mutate(&mut state);
            state.last_updated = chrono::Utc::now();

            // Write to temp file
            let state_json = serde_json::to_string_pretty(&state)?;
            fs::write(&temp_file, &state_json)
                .await
                .context("writing temp state file for update")?;

            // Set permissions (world-readable so non-root can list VMs)
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let permissions = std::fs::Permissions::from_mode(0o644);
                tokio::fs::set_permissions(&temp_file, permissions)
                    .await
                    .context("setting file permissions on state file")?;
            }

            // Atomic rename
            fs::rename(&temp_file, &state_file)
                .await
                .context("renaming temp state file for update")?;

            Ok(Some(state))
        }
        .await;

        // Release lock (held until this point)
        // NOTE: We intentionally do NOT delete lock files - see allocate_loopback_ip comment
        flock
            .unlock()
            .map_err(|(_, err)| err)
            .context("releasing lock after state update")?;

        result
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
    /// The previous health status before update, or None if the state file no
    /// longer exists (e.g. the VM was deleted concurrently — nothing is written).
    pub async fn update_health_status(
        &self,
        vm_id: &str,
        health_status: super::HealthStatus,
        exit_code: Option<i32>,
    ) -> Result<Option<super::HealthStatus>> {
        let mut previous_status = None;
        self.update_state(vm_id, |state| {
            previous_status = Some(state.health_status);
            state.health_status = health_status;
            if exit_code.is_some() {
                state.exit_code = exit_code;
            }
        })
        .await?;
        Ok(previous_status)
    }

    /// Record a host-side vsock transport reset for this VM by bumping the
    /// persisted `vsock_epoch` (locked read-modify-write via `update_state`).
    ///
    /// Ordering contract: call this AFTER a snapshot pause/save and BEFORE the
    /// VM is resumed. Exec clients capture the epoch right after their vsock
    /// connection is established — which is only possible against a running
    /// guest — so bumping while the VM is still paused guarantees that every
    /// connection from before the pause observes the change (loud orphan abort
    /// instead of an indefinite hang) and every post-resume connection reads
    /// the already-bumped value (no false abort).
    ///
    /// Returns the new epoch, or `None` if the state file no longer exists
    /// (VM mid-teardown — its exec sessions get a socket error when the
    /// hypervisor exits, so no epoch signal is needed).
    pub async fn bump_vsock_epoch(&self, vm_id: &str) -> Result<Option<u64>> {
        let updated = self
            .update_state(vm_id, |state| {
                state.vsock_epoch += 1;
            })
            .await?;
        Ok(updated.map(|state| state.vsock_epoch))
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
        let lock_fd = open_lock_file(&lock_file).context("opening loopback IP lock file")?;

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
        // Note: We rely on state file cleanup (cleanup_stale_state) to handle dead processes.
        // We don't check if port 8080 is available because wildcard binds (0.0.0.0:8080)
        // would cause false negatives. Real port conflicts are detected at pasta bind time.
        let ip = (|| -> Result<String> {
            for b2 in 0..=255u8 {
                for b3 in 2..=254u8 {
                    // Skip 127.0.0.1 (localhost)
                    let ip = format!("127.0.{}.{}", b2, b3);
                    if !used_ips.contains(&ip) {
                        return Ok(ip);
                    }
                }
            }
            anyhow::bail!("all loopback IPs exhausted (65,000+ VMs)")
        })()?;

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
