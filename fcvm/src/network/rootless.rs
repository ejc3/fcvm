use anyhow::{Context, Result, bail};
use std::process::Stdio;
use tokio::process::{Child, Command};
use tracing::info;

use super::{NetworkConfig, NetworkManager, PortMapping, types::generate_mac};

/// Rootless networking using slirp4netns
pub struct RootlessNetwork {
    vm_id: String,
    tap_device: String,
    port_mappings: Vec<PortMapping>,
    slirp_process: Option<Child>,
}

impl RootlessNetwork {
    pub fn new(vm_id: String, tap_device: String, port_mappings: Vec<PortMapping>) -> Self {
        Self {
            vm_id,
            tap_device,
            port_mappings,
            slirp_process: None,
        }
    }
}

#[async_trait::async_trait]
impl NetworkManager for RootlessNetwork {
    async fn setup(&mut self) -> Result<NetworkConfig> {
        info!(vm_id = %self.vm_id, "setting up rootless network with slirp4netns");

        // For rootless mode, we'll use slirp4netns to provide userspace networking
        // The TAP device will be created by Firecracker itself
        // We'll configure port forwarding via slirp4netns after the VM starts

        // Generate MAC address
        let guest_mac = generate_mac();

        // Note: slirp4netns setup happens after Firecracker starts
        // For now, just return the config
        Ok(NetworkConfig {
            tap_device: self.tap_device.clone(),
            guest_mac,
            guest_ip: Some("10.0.2.15".to_string()), // Default slirp4netns IP
            host_ip: Some("10.0.2.2".to_string()),
        })
    }

    async fn cleanup(&mut self) -> Result<()> {
        if let Some(mut process) = self.slirp_process.take() {
            info!(vm_id = %self.vm_id, "killing slirp4netns process");
            let _ = process.kill().await;
            let _ = process.wait().await;
        }
        Ok(())
    }

    fn tap_device(&self) -> &str {
        &self.tap_device
    }
}

/// Setup slirp4netns for a running VM
pub async fn setup_slirp4netns(
    pid: u32,
    tap_device: &str,
    port_mappings: &[PortMapping],
) -> Result<Child> {
    let mut cmd = Command::new("slirp4netns");
    cmd.arg("--configure");
    cmd.arg("--mtu=65520");

    // Add port forwarding
    for mapping in port_mappings {
        let hostfwd = format!(
            "{}:{}:{}",
            mapping.proto,
            mapping.host_port,
            mapping.guest_port
        );
        cmd.arg("--port").arg(hostfwd);
    }

    cmd.arg(pid.to_string());
    cmd.arg(tap_device);

    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());

    let child = cmd.spawn()
        .context("spawning slirp4netns")?;

    info!(pid = pid, "slirp4netns started");
    Ok(child)
}
