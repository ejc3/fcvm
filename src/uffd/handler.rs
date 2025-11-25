use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::process::{Child, Command};
use tokio::time::{sleep, Duration};
use tracing::{info, warn};

/// Manages the UFFD page fault handler process for memory sharing across VM clones
pub struct UffdHandler {
    process: Child,
    socket_path: PathBuf,
}

impl UffdHandler {
    /// Start UFFD handler process for serving memory pages on demand
    ///
    /// The handler will:
    /// 1. Bind to the Unix socket at `socket_path`
    /// 2. Memory-map the snapshot file at `mem_file_path`
    /// 3. Serve page faults from Firecracker via userfaultfd protocol
    /// 4. Enable true copy-on-write memory sharing across clones
    pub async fn start(socket_path: PathBuf, mem_file_path: &Path) -> Result<Self> {
        info!(
            socket = %socket_path.display(),
            mem_file = %mem_file_path.display(),
            "starting UFFD handler for page-level memory sharing"
        );

        // Find uffd_handler binary (should be in same directory as fcvm)
        let handler_bin = std::env::current_exe()
            .ok()
            .and_then(|exe_path| {
                let exe_dir = exe_path.parent()?;
                let handler = exe_dir.join("uffd_handler");
                if handler.exists() {
                    Some(handler)
                } else {
                    None
                }
            })
            .or_else(|| which::which("uffd_handler").ok())
            .context("uffd_handler binary not found - should be installed alongside fcvm")?;

        info!(handler_bin = %handler_bin.display(), "found uffd_handler binary");

        // Remove stale socket (ignore errors if not exists - avoids TOCTOU race)
        let _ = std::fs::remove_file(&socket_path);

        // Spawn uffd handler process
        // Args: <socket_path> <mem_file_path>
        let mut process = Command::new(&handler_bin)
            .arg(&socket_path)
            .arg(mem_file_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("spawning uffd_handler process")?;

        info!(pid = ?process.id(), "uffd_handler process spawned");

        // Wait for socket to exist (handler binds to it)
        let mut attempts = 0;
        const MAX_ATTEMPTS: u32 = 100; // 10 seconds total
        const RETRY_DELAY_MS: u64 = 100;

        while !socket_path.exists() {
            if attempts >= MAX_ATTEMPTS {
                // Check if process died
                if let Ok(Some(status)) = process.try_wait() {
                    anyhow::bail!("uffd_handler process exited early with status: {}", status);
                }
                anyhow::bail!(
                    "uffd_handler did not create socket after {} attempts",
                    MAX_ATTEMPTS
                );
            }

            sleep(Duration::from_millis(RETRY_DELAY_MS)).await;
            attempts += 1;
        }

        info!(
            socket = %socket_path.display(),
            attempts,
            "uffd_handler socket ready"
        );

        Ok(Self {
            process,
            socket_path,
        })
    }

    /// Get the Unix socket path where the handler is listening
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Check if the handler process is still running
    pub fn is_alive(&mut self) -> bool {
        matches!(self.process.try_wait(), Ok(None))
    }

    /// Kill the handler process
    pub async fn kill(&mut self) -> Result<()> {
        info!("killing uffd_handler process");
        self.process.kill().await.context("killing uffd_handler")
    }
}

impl Drop for UffdHandler {
    fn drop(&mut self) {
        // Best-effort cleanup
        if self.is_alive() {
            warn!("uffd_handler still running during drop, sending kill signal");
            let _ = self.process.start_kill();
        }

        // Clean up socket file
        if self.socket_path.exists() {
            if let Err(e) = std::fs::remove_file(&self.socket_path) {
                warn!(error = %e, "failed to remove uffd socket during cleanup");
            }
        }
    }
}
