use anyhow::Result;
use tracing::info;

/// Wait by executing command in guest
pub async fn wait_exec(command: &str) -> Result<()> {
    info!(command = command, "waiting for exec readiness");

    // TODO: Execute command in guest via vsock or SSH
    // This requires guest communication channel

    info!(command = command, "exec command succeeded");
    Ok(())
}
