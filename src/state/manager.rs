use anyhow::{bail, Context, Result};
use nix::fcntl::{Flock, FlockArg};
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

pub(crate) fn lock_state_file(path: &Path, arg: FlockArg) -> Result<Flock<std::fs::File>> {
    lock_opened_state_file(path, open_lock_file(path)?, arg)
}

fn lock_opened_state_file(
    path: &Path,
    mut file: std::fs::File,
    arg: FlockArg,
) -> Result<Flock<std::fs::File>> {
    use std::os::unix::fs::MetadataExt;

    loop {
        let guard = Flock::lock(file, arg)
            .map_err(|(_, error)| error)
            .context("acquiring state lock")?;
        let held = guard
            .metadata()
            .context("reading held state lock identity")?;
        let current = match std::fs::metadata(path) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("reading state lock path identity"),
        };
        if current.is_some_and(|current| current.dev() == held.dev() && current.ino() == held.ino())
        {
            return Ok(guard);
        }
        // Cleanup can unlink a lock after this waiter opened it. No payload
        // may be touched until we hold the inode now published at the path.
        drop(guard);
        file = open_lock_file(path)?;
    }
}

pub(crate) fn remove_if_exists(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
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

/// Whether the process a state file names is still running and is still the process
/// that wrote it: its PID exists, has not exited (a zombie has), and has the recorded
/// start time. A state without a PID names no process, and every state this fcvm
/// writes has one from its first save (`StateManager::claim_name`).
pub fn owner_is_live(state: &VmState) -> bool {
    let Some(pid) = state.pid else {
        return false;
    };
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // Fields after the last ')' start at field 3 (state); starttime is field 22.
    let Some((_, after_comm)) = stat.rsplit_once(')') else {
        return false;
    };
    let mut fields = after_comm.split_whitespace();
    if matches!(fields.next(), None | Some("Z") | Some("X")) {
        return false;
    }
    match state.pid_start_time {
        Some(recorded) => fields.nth(18).and_then(|t| t.parse::<u64>().ok()) == Some(recorded),
        None => true,
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

/// The state directory's entries, as paths. The directory is opened and read on one
/// blocking thread, with `after_open` between the two: a test seam for the window a
/// rename has to land in to be missed by a btrfs scan (`list_vms_after_open`).
async fn scan_state_dir(
    dir: &Path,
    after_open: impl FnOnce() + Send + 'static,
) -> Result<Vec<PathBuf>> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || -> std::io::Result<Vec<PathBuf>> {
        let entries = std::fs::read_dir(&dir)?;
        after_open();
        entries
            .map(|entry| entry.map(|entry| entry.path()))
            .collect()
    })
    .await
    .context("scanning state directory")?
    .context("reading state directory")
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

        // Acquire exclusive lock (blocks if another process has lock).
        // NOTE: Flock::lock() is technically blocking I/O in an async context, but
        // the lock is held for microseconds with near-zero contention (only this
        // process writes its own state file). Using spawn_blocking would add more
        // overhead than the lock itself. If contention becomes an issue, switch to
        // FlockArg::LockExclusiveNonblock with retry + tokio::task::yield_now().
        let flock = lock_state_file(&lock_file, FlockArg::LockExclusive)
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

    /// Write a new VM's first state, under its name, while no other live VM has that name.
    ///
    /// The check and the write happen under one host-wide lock (`names.lock` in the state
    /// directory), and every launch writes its first state here, so of two launches with
    /// one name exactly one gets it. The state is written with this process's PID and
    /// start time, which is what makes it live to the next claim; it stops being live when
    /// the VM's state is deleted or its process exits, so a SIGKILLed VM frees its name
    /// without any cleanup. A state with this name whose owner is not live is deleted
    /// here, so lookups by name never see it beside the new VM.
    pub async fn claim_name(&self, vm_state: &mut VmState) -> Result<()> {
        self.claim_name_after_check(vm_state, || {}).await
    }

    /// `claim_name` with a seam for tests: `after_check` runs once the name is known to be
    /// free and before the state is written, with the name lock held.
    #[doc(hidden)]
    pub async fn claim_name_after_check(
        &self,
        vm_state: &mut VmState,
        after_check: impl FnOnce(),
    ) -> Result<()> {
        let name = vm_state
            .name
            .clone()
            .context("a VM must have a name before it claims one")?;
        let lock_path = self.state_dir.join("names.lock");
        let lock = tokio::task::spawn_blocking(move || -> Result<Flock<std::fs::File>> {
            let file = open_lock_file(&lock_path).context("opening the VM name lock")?;
            Flock::lock(file, FlockArg::LockExclusive)
                .map_err(|(_, error)| error)
                .context("acquiring the VM name lock")
        })
        .await
        .context("waiting for the VM name lock")??;

        self.cleanup_stale_state().await;
        let vms = self
            .list_vms()
            .await
            .context("listing VMs to check that the name is free")?;
        for vm in vms
            .iter()
            .filter(|vm| vm.vm_id != vm_state.vm_id && vm.name.as_deref() == Some(name.as_str()))
        {
            if owner_is_live(vm) {
                bail!(
                    "a live VM is already named '{}' (pid {}, vm_id {}); VM names are unique \
                     among running VMs, so stop that VM or choose another --name",
                    name,
                    vm.pid
                        .map_or_else(|| "none".to_string(), |pid| pid.to_string()),
                    vm.vm_id
                );
            }
            self.delete_state(&vm.vm_id).await.with_context(|| {
                format!(
                    "deleting the state of exited VM {} that was named '{}'",
                    vm.vm_id, name
                )
            })?;
        }
        after_check();

        let pid = std::process::id();
        vm_state.pid = Some(pid);
        vm_state.pid_start_time = process_start_time(pid);
        self.save_state(vm_state).await?;

        // Released only after the state is on disk: the next claim must see it.
        lock.unlock()
            .map_err(|(_, error)| error)
            .context("releasing the VM name lock")?;
        Ok(())
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
        let flock = lock_state_file(&lock_file, FlockArg::LockExclusive)
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

            remove_if_exists(&temp_file).context("deleting temporary VM state")?;
            // Retire the lock while still holding it, after the last payload
            // operation. Waiters must validate their inode before proceeding.
            remove_if_exists(&lock_file).context("deleting VM state lock")?;

            Ok(())
        }
        .await;

        flock
            .unlock()
            .map_err(|(_, err)| err)
            .context("releasing lock after state delete")?;

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

        let mut seen = std::collections::HashSet::new();
        for entry in entries.flatten() {
            let mut path = entry.path();
            if path.extension().is_some_and(|extension| extension == "tmp") {
                path = path.with_extension("");
            }
            if path.extension().is_none_or(|extension| extension != "json")
                || !seen.insert(path.clone())
            {
                continue;
            }
            let lock_path = path.with_extension("json.lock");
            let _guard = match lock_state_file(&lock_path, FlockArg::LockExclusiveNonblock) {
                Ok(guard) => guard,
                Err(error) => {
                    if error.downcast_ref::<nix::errno::Errno>()
                        != Some(&nix::errno::Errno::EWOULDBLOCK)
                    {
                        tracing::warn!(path = %path.display(), %error, "cleanup_stale_state: lock failed");
                    }
                    continue;
                }
            };

            // Read identity only after acquiring the writer's lock. A missing
            // sibling makes a temp orphaned; an unreadable sibling proves nothing.
            let stale = match std::fs::read_to_string(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                Err(_) => continue,
                Ok(content) => {
                    let Ok(state) = serde_json::from_str::<serde_json::Value>(&content) else {
                        continue;
                    };
                    let Some(pid) = state
                        .get("pid")
                        .and_then(|p| p.as_u64())
                        .and_then(|p| u32::try_from(p).ok())
                    else {
                        continue;
                    };
                    let Ok(exists) = Path::new(&format!("/proc/{pid}")).try_exists() else {
                        continue;
                    };
                    let replaced = state
                        .get("pid_start_time")
                        .and_then(|t| t.as_u64())
                        .zip(process_start_time(pid))
                        .is_some_and(|(recorded, current)| recorded != current);
                    !exists || replaced
                }
            };
            examined += 1;
            if !stale {
                continue;
            }

            let temp_path = path.with_extension("json.tmp");
            // Keep the guard through the final unlink. If removing either
            // payload fails, retain the lock and retry on a later sweep.
            let result = [&path, &temp_path, &lock_path]
                .into_iter()
                .try_for_each(|path| remove_if_exists(path));
            match result {
                Ok(()) => removed += 1,
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "cleanup_stale_state: removal failed")
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
        self.list_vms_after_open(|| {}).await
    }

    /// `list_vms` with a seam for tests: `after_open` runs once the state directory is
    /// open and before any of its entries are read, which is the window a state-file
    /// rename has to land in to be missed by a btrfs directory scan.
    #[doc(hidden)]
    pub async fn list_vms_after_open(
        &self,
        after_open: impl FnOnce() + Send + 'static,
    ) -> Result<Vec<VmState>> {
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

        let paths = scan_state_dir(&self.state_dir, after_open).await?;

        // A state file is replaced by rename on every update. On btrfs a scan that
        // overlaps the rename can miss the name: since Linux 6.5 btrfs readdir stops at
        // the last directory index that existed when the scan began, and the rename
        // gives the file a new, higher index. A VM's lock file (`<vm_id>.json.lock`) is
        // made before its first save, kept until its state is deleted, and never
        // renamed, so every VM that existed when the scan began shows up through its
        // state file, its lock file, or both. Each state file is then read by path,
        // which always finds the current one.
        let mut vm_ids = std::collections::BTreeSet::new();
        for path in paths {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if let Some(vm_id) = name
                .strip_suffix(".json.lock")
                .or_else(|| name.strip_suffix(".json"))
            {
                vm_ids.insert(vm_id.to_string());
            }
        }

        for vm_id in vm_ids {
            let path = self.state_dir.join(format!("{vm_id}.json"));
            {
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
                    // A VM that exits deletes its state file before its lock file,
                    // and a new VM makes its lock file before its state file, so a
                    // name from the scan can have no state file by the time it is
                    // read. Not an error.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        tracing::debug!(
                            path = %path.display(),
                            "list_vms: no state file for this name (VM starting or gone)"
                        );
                    }
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

        // Acquire exclusive lock (blocks if another process has lock)
        let flock = lock_state_file(&lock_file, FlockArg::LockExclusive)
            .context("acquiring exclusive lock for state update")?;

        // CRITICAL: Hold lock across entire read-modify-write
        let result: Result<Option<VmState>> = async {
            // Load current state. The file may have been deleted while we
            // waited for the lock (delete_state holds the same lock) — treat
            // that as "nothing to update" rather than recreating it.
            let state_json = match fs::read_to_string(&state_file).await {
                Ok(json) => json,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // We may have reopened the lock after a deleter retired
                    // its inode. Retire this no-op lock under the same guard.
                    remove_if_exists(&temp_file).context("deleting orphan temporary state")?;
                    remove_if_exists(&lock_file).context("deleting unused state lock")?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything `list_vms` logs at WARN or above while scanning `dir`.
    fn warnings_from_listing(dir: &std::path::Path) -> (usize, String) {
        #[derive(Clone, Default)]
        struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Captured {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
            type Writer = Captured;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }
        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .finish();
        let manager = StateManager::new(dir.to_path_buf());
        let listed = tracing::subscriber::with_default(subscriber, || {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(manager.list_vms())
                .expect("list_vms")
                .len()
        });
        let log = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        (listed, log)
    }

    /// A VM that exits deletes its state file, and another process may be
    /// listing the directory at that moment. That is not worth a warning: the
    /// default log level prints warnings, and `fcvm exec` shares its stderr
    /// with the command it runs.
    #[test]
    fn a_state_file_that_vanishes_during_a_listing_is_not_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        // Listed by read_dir, gone by the time it is read.
        std::os::unix::fs::symlink(dir.path().join("deleted"), dir.path().join("vm-gone.json"))
            .unwrap();
        let (listed, log) = warnings_from_listing(dir.path());
        assert_eq!(listed, 0);
        assert_eq!(log, "", "a vanished state file was reported");
    }

    /// Control for the test above: the capture does see `list_vms` warn.
    #[test]
    fn an_unparsable_state_file_is_still_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("vm-bad.json"), "not json").unwrap();
        let (listed, log) = warnings_from_listing(dir.path());
        assert_eq!(listed, 0);
        assert!(log.contains("failed to parse state file"), "{log:?}");
    }

    #[test]
    fn state_lock_waiter_rejects_unlinked_inode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vm.json.lock");
        let holder = lock_state_file(&path, FlockArg::LockExclusive).unwrap();
        // A waiter has opened A but has not acquired its lock when cleanup
        // unlinks it. A new writer then opens and locks B at the same path.
        let waiter = open_lock_file(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let replacement = lock_state_file(&path, FlockArg::LockExclusive).unwrap();
        drop(holder);
        let result = lock_opened_state_file(&path, waiter, FlockArg::LockExclusiveNonblock);
        assert!(
            result.is_err(),
            "waiter acquired retired inode A while B was held"
        );
        assert_eq!(
            result.unwrap_err().downcast_ref::<nix::errno::Errno>(),
            Some(&nix::errno::Errno::EWOULDBLOCK)
        );
        drop(replacement);
        let current = lock_state_file(&path, FlockArg::LockExclusiveNonblock).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            current.metadata().unwrap().ino(),
            std::fs::metadata(path).unwrap().ino()
        );
    }

    fn named_state(vm_id: &str, name: &str) -> VmState {
        let mut state = VmState::new(vm_id.to_string(), "alpine:latest".to_string(), 1, 512);
        state.name = Some(name.to_string());
        state
    }

    /// Write a state file as it is, bypassing `save_state`, which records the start time
    /// of whatever process is at the PID now.
    fn write_state_file(dir: &Path, state: &VmState) {
        std::fs::write(
            dir.join(format!("{}.json", state.vm_id)),
            serde_json::to_string(state).unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn a_live_vm_keeps_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let manager = StateManager::new(dir.path().to_path_buf());
        let mut first = named_state("vm-first", "slot-3");
        manager.claim_name(&mut first).await.unwrap();
        assert_eq!(first.pid, Some(std::process::id()));

        let mut second = named_state("vm-second", "slot-3");
        let error = format!("{:#}", manager.claim_name(&mut second).await.unwrap_err());
        assert!(
            error.contains("'slot-3'")
                && error.contains(&format!("pid {}", std::process::id()))
                && error.contains("vm-first"),
            "the refusal must name the live VM and its pid: {error}"
        );
        assert!(
            !dir.path().join("vm-second.json").exists(),
            "a refused claim must write no state"
        );
        let mut other = named_state("vm-other", "slot-4");
        manager.claim_name(&mut other).await.unwrap();
    }

    /// A VM whose process exited, or whose PID now belongs to another process, does not
    /// hold its name, and its state is removed so a lookup by name finds one VM.
    #[tokio::test]
    async fn an_exited_vm_frees_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let manager = StateManager::new(dir.path().to_path_buf());
        // Above the kernel's PID_MAX_LIMIT (4194304), so no process has it.
        let mut exited = named_state("vm-exited", "slot-3");
        exited.pid = Some(4_000_000_000);
        write_state_file(dir.path(), &exited);
        let mut replaced = named_state("vm-replaced", "slot-3");
        replaced.pid = Some(std::process::id());
        replaced.pid_start_time =
            crate::utils::process_start_time(std::process::id()).map(|start| start + 1);
        write_state_file(dir.path(), &replaced);
        let mut unowned = named_state("vm-unowned", "slot-3");
        unowned.pid = None;
        write_state_file(dir.path(), &unowned);

        let mut new = named_state("vm-new", "slot-3");
        manager.claim_name(&mut new).await.unwrap();
        let named: Vec<String> = manager
            .list_vms()
            .await
            .unwrap()
            .into_iter()
            .filter(|vm| vm.name.as_deref() == Some("slot-3"))
            .map(|vm| vm.vm_id)
            .collect();
        assert_eq!(named, vec!["vm-new".to_string()]);
        assert_eq!(
            manager.load_state_by_name("slot-3").await.unwrap().vm_id,
            "vm-new"
        );
    }

    /// Two claims of one name at once: the second runs entirely inside the first's
    /// window between its check and its write. Exactly one may succeed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn of_two_concurrent_claims_of_one_name_exactly_one_wins() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().to_path_buf();
        let manager = StateManager::new(state_dir.clone());
        let mut first = named_state("vm-first", "slot-3");
        let mut second_thread = None;
        let mut second_finished_inside_window = false;
        manager
            .claim_name_after_check(&mut first, || {
                let (done_tx, done_rx) = std::sync::mpsc::channel();
                second_thread = Some(std::thread::spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    let result = runtime.block_on(async {
                        let mut second = named_state("vm-second", "slot-3");
                        StateManager::new(state_dir).claim_name(&mut second).await
                    });
                    let _ = done_tx.send(());
                    result
                }));
                // Correct: the second claim waits on the name lock and cannot finish
                // here. Without the lock it finishes within milliseconds.
                second_finished_inside_window = done_rx
                    .recv_timeout(std::time::Duration::from_millis(500))
                    .is_ok();
            })
            .await
            .unwrap();
        let second = second_thread.unwrap().join().unwrap();
        assert!(
            !second_finished_inside_window,
            "the second claim finished while the first held the name between check and write"
        );
        let error = format!("{:#}", second.unwrap_err());
        assert!(error.contains("vm-first"), "{error}");
    }

    #[test]
    fn a_zombie_is_not_a_live_owner() {
        let child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        let start = crate::utils::process_start_time(pid);
        let mut state = named_state("vm-zombie", "slot-3");
        state.pid = Some(pid);
        state.pid_start_time = start;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
            let state_field = stat.rsplit_once(')').unwrap().1.split_whitespace().next();
            if state_field == Some("Z") {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "`true` did not exit");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            !owner_is_live(&state),
            "an exited, unreaped process is not live"
        );
        let mut child = child;
        child.wait().unwrap();

        state.pid = Some(std::process::id());
        state.pid_start_time = crate::utils::process_start_time(std::process::id());
        assert!(owner_is_live(&state), "this test process is live");
    }
}
