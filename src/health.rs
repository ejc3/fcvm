use tokio::time::Duration;
use tracing::info;

use crate::paths;
use crate::state::{HealthStatus, StateManager};

/// Spawn a background health monitoring task for a VM
///
/// The task polls the VM process health at adaptive intervals:
/// - 100ms during startup (until healthy)
/// - 10s after VM is healthy
///
/// The task runs indefinitely until the tokio runtime shuts down.
pub fn spawn_health_monitor(vm_id: String, pid: Option<u32>) {
    tokio::spawn(async move {
        let state_manager = StateManager::new(paths::state_dir());

        // Adaptive polling: 100ms during startup, 10s after healthy
        let mut poll_interval = Duration::from_millis(100);
        let mut is_healthy = false;

        loop {
            tokio::time::sleep(poll_interval).await;

            // Check if process is still running
            if let Some(pid) = pid {
                let health_status = if std::fs::metadata(format!("/proc/{}", pid)).is_ok() {
                    HealthStatus::Healthy
                } else {
                    HealthStatus::Unreachable
                };

                // Update state file
                if let Ok(mut state) = state_manager.load_state(&vm_id).await {
                    state.health_status = health_status;
                    state.last_updated = chrono::Utc::now();
                    let _ = state_manager.save_state(&state).await;
                }

                // Switch to slower polling once healthy
                if health_status == HealthStatus::Healthy && !is_healthy {
                    is_healthy = true;
                    poll_interval = Duration::from_secs(10);
                    info!(vm_id = %vm_id, "VM healthy, switching to 10s polling");
                }
            }
        }
    });
}
