use tokio::process::Command;
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
/// Health check tests HTTP connectivity via curl to the guest IP.
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

            let health_status = if let Some(pid) = pid {
                // First check if Firecracker process is still running
                if std::fs::metadata(format!("/proc/{}", pid)).is_err() {
                    HealthStatus::Unreachable
                } else {
                    // Process exists, now check if application is responding
                    // Get guest IP, TAP device, and health check path from state
                    if let Ok(state) = state_manager.load_state(&vm_id).await {
                        if let (Some(guest_ip), Some(tap_device)) = (
                            state.config.network.get("guest_ip").and_then(|v| v.as_str()),
                            state.config.network.get("tap_device").and_then(|v| v.as_str()),
                        ) {
                            // Try HTTP request via TAP device
                            let health_path = &state.config.health_check_path;
                            match check_http_health(guest_ip, tap_device, health_path).await {
                                Ok(true) => HealthStatus::Healthy,
                                Ok(false) => HealthStatus::Unhealthy,
                                Err(_) => HealthStatus::Timeout,
                            }
                        } else {
                            // No network config yet, just check process
                            HealthStatus::Unknown
                        }
                    } else {
                        HealthStatus::Unknown
                    }
                }
            } else {
                HealthStatus::Unknown
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
    });
}

/// Check if HTTP service is responding via curl
async fn check_http_health(guest_ip: &str, tap_device: &str, health_path: &str) -> Result<bool, ()> {
    let url = format!("http://{}{}", guest_ip, health_path);

    let output = Command::new("curl")
        .args([
            "-s",           // Silent
            "-f",           // Fail on HTTP errors
            "-m", "1",      // 1 second timeout
            "--interface", tap_device,
            &url,
        ])
        .output()
        .await
        .map_err(|_| ())?;

    Ok(output.status.success())
}
