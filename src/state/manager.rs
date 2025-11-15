use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::fs;

use super::types::VmState;

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
    pub async fn save_state(&self, state: &VmState) -> Result<()> {
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

            Ok::<(), anyhow::Error>(())
        }.await;

        // Release lock (happens automatically when flock is dropped, but being explicit)
        flock.unlock()
            .map_err(|(_, err)| err)
            .context("releasing lock on state file")?;

        // Clean up lock file (optional, but keeps directory clean)
        let _ = std::fs::remove_file(&lock_file);

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

    /// Delete VM state
    pub async fn delete_state(&self, vm_id: &str) -> Result<()> {
        let state_file = self.state_dir.join(format!("{}.json", vm_id));
        if state_file.exists() {
            fs::remove_file(&state_file)
                .await
                .context("deleting VM state")?;
        }
        Ok(())
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
        let vms = self.list_vms().await?;
        vms.into_iter()
            .find(|vm| vm.pid == Some(pid))
            .ok_or_else(|| anyhow::anyhow!("VM not found with PID: {}", pid))
    }

    /// List all VMs
    pub async fn list_vms(&self) -> Result<Vec<VmState>> {
        let mut vms = Vec::new();

        if !self.state_dir.exists() {
            return Ok(vms);
        }

        let mut entries = fs::read_dir(&self.state_dir)
            .await
            .context("reading state directory")?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(state_json) = fs::read_to_string(&path).await {
                    if let Ok(state) = serde_json::from_str::<VmState>(&state_json) {
                        vms.push(state);
                    }
                }
            }
        }

        Ok(vms)
    }
}

// StateManager tests moved to tests/test_state_integration.rs for better integration testing
