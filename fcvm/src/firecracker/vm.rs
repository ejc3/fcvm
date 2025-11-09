use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::{Child, Command};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::{info, warn, error};

use super::FirecrackerClient;

/// Manages a Firecracker VM process
pub struct VmManager {
    vm_id: String,
    socket_path: PathBuf,
    log_path: Option<PathBuf>,
    process: Option<Child>,
    client: Option<FirecrackerClient>,
}

impl VmManager {
    pub fn new(vm_id: String, socket_path: PathBuf, log_path: Option<PathBuf>) -> Self {
        Self {
            vm_id,
            socket_path,
            log_path,
            process: None,
            client: None,
        }
    }

    /// Start the Firecracker process
    pub async fn start(&mut self, firecracker_bin: &Path, config_override: Option<&Path>) -> Result<()> {
        info!(vm_id = %self.vm_id, "starting Firecracker process");

        // Ensure socket doesn't exist
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path)
                .context("removing existing socket")?;
        }

        let mut cmd = Command::new(firecracker_bin);
        cmd.arg("--api-sock").arg(&self.socket_path);

        if let Some(config) = config_override {
            cmd.arg("--config-file").arg(config);
        }

        // Setup logging
        if let Some(log_path) = &self.log_path {
            cmd.arg("--log-path").arg(log_path);
            cmd.arg("--level").arg("Info");
            cmd.arg("--show-level");
            cmd.arg("--show-log-origin");
        }

        // Disable seccomp for now (can enable later for production)
        cmd.arg("--no-seccomp");

        // Spawn process
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn()
            .context("spawning Firecracker process")?;

        // Stream stdout/stderr to tracing
        if let Some(stdout) = child.stdout.take() {
            let vm_id = self.vm_id.clone();
            tokio::spawn(async move {
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    info!(vm_id = %vm_id, target: "firecracker", "{}", line);
                }
            });
        }

        if let Some(stderr) = child.stderr.take() {
            let vm_id = self.vm_id.clone();
            tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    warn!(vm_id = %vm_id, target: "firecracker", "{}", line);
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
        use tokio::time::{sleep, Duration};

        for _ in 0..50 {  // 5 second timeout
            if self.socket_path.exists() {
                return Ok(());
            }
            sleep(Duration::from_millis(100)).await;
        }

        bail!("Firecracker socket not ready after 5 seconds")
    }

    /// Get the API client
    pub fn client(&self) -> Result<&FirecrackerClient> {
        self.client.as_ref().context("VM not started")
    }

    /// Wait for the VM process to exit
    pub async fn wait(&mut self) -> Result<std::process::ExitStatus> {
        if let Some(mut process) = self.process.take() {
            let status = process.wait().await
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
            process.kill().await.context("killing Firecracker process")?;
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
            for _ in 0..50 {
                if console_path.exists() {
                    break;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
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
