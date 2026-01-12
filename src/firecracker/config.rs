//! Firecracker launch configuration.
//!
//! This module is the SINGLE SOURCE OF TRUTH for Firecracker VM configuration.
//! The same config struct is used for:
//! 1. Computing cache keys (hash the JSON)
//! 2. Actually launching Firecracker (via apply method)
//!
//! This ensures the cache key exactly matches what Firecracker receives.
//! If you need a new parameter that affects VM state, add it HERE.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Complete Firecracker VM launch configuration.
/// Serialize this to JSON for cache key computation.
/// All fields here affect the cached VM state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirecrackerConfig {
    /// Boot source configuration
    pub boot_source: BootSource,
    /// Machine configuration (CPU, memory)
    pub machine_config: MachineConfig,
    /// Root drive configuration
    pub drives: Vec<Drive>,
    /// Container image to pull (for fc-agent)
    pub container_image: String,
    /// Network mode (bridged or rootless)
    pub network_mode: NetworkMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootSource {
    /// Path to kernel image (content-addressed, SHA in filename)
    pub kernel_image_path: PathBuf,
    /// Path to initrd (content-addressed, SHA in filename)
    pub initrd_path: PathBuf,
    /// Static kernel boot arguments (without per-instance values like IP)
    pub boot_args: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineConfig {
    pub vcpu_count: u8,
    pub mem_size_mib: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Drive {
    pub drive_id: String,
    /// Path to drive image (content-addressed for rootfs)
    pub path_on_host: PathBuf,
    pub is_root_device: bool,
    pub is_read_only: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    Bridged,
    Rootless,
}

/// Static boot arguments that affect cached VM state.
/// Does NOT include per-instance values like IP addresses.
/// Architecture-specific because reboot method differs (ARM64=k, x86=t).
pub fn static_boot_args() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        // Triple-fault - only reliable method on x86 Firecracker
        "console=ttyS0 reboot=t panic=1 pci=off random.trust_cpu=1 systemd.log_color=no root=/dev/vda rw"
    } else {
        // Keyboard controller - works on ARM64 via PSCI
        "console=ttyS0 reboot=k panic=1 pci=off random.trust_cpu=1 systemd.log_color=no root=/dev/vda rw"
    }
}

impl FirecrackerConfig {
    /// Create a new Firecracker config for a podman VM.
    pub fn new(
        kernel_path: PathBuf,
        initrd_path: PathBuf,
        rootfs_path: PathBuf,
        container_image: String,
        cpu: u8,
        mem: u32,
        network_mode: NetworkMode,
    ) -> Self {
        Self {
            boot_source: BootSource {
                kernel_image_path: kernel_path,
                initrd_path,
                boot_args: static_boot_args().to_string(),
            },
            machine_config: MachineConfig {
                vcpu_count: cpu,
                mem_size_mib: mem,
            },
            drives: vec![Drive {
                drive_id: "rootfs".to_string(),
                path_on_host: rootfs_path,
                is_root_device: true,
                is_read_only: false,
            }],
            container_image,
            network_mode,
        }
    }

    /// Compute cache key by hashing the JSON representation.
    pub fn cache_key(&self) -> String {
        use crate::setup::rootfs::compute_sha256;
        let json = serde_json::to_string(self).expect("FirecrackerConfig serialization failed");
        compute_sha256(json.as_bytes())[..12].to_string()
    }

    /// Apply this config to a Firecracker client.
    ///
    /// `runtime_boot_args` contains per-instance values (IPs, strace, etc.)
    /// that don't affect cache but are needed for launch.
    pub async fn apply(
        &self,
        client: &super::api::FirecrackerClient,
        runtime_boot_args: &str,
    ) -> Result<()> {
        // Build full boot args: static (cached) + runtime (per-instance)
        let full_boot_args = if runtime_boot_args.is_empty() {
            self.boot_source.boot_args.clone()
        } else {
            format!("{} {}", self.boot_source.boot_args, runtime_boot_args)
        };

        // Set boot source
        client
            .set_boot_source(super::api::BootSource {
                kernel_image_path: self.boot_source.kernel_image_path.display().to_string(),
                initrd_path: Some(self.boot_source.initrd_path.display().to_string()),
                boot_args: Some(full_boot_args),
            })
            .await?;

        // Set machine config
        client
            .set_machine_config(super::api::MachineConfig {
                vcpu_count: self.machine_config.vcpu_count,
                mem_size_mib: self.machine_config.mem_size_mib,
                smt: Some(false),
                cpu_template: None,
                track_dirty_pages: Some(true), // Enable snapshot support
            })
            .await?;

        // Add drives
        for drive in &self.drives {
            client
                .add_drive(
                    &drive.drive_id,
                    super::api::Drive {
                        drive_id: drive.drive_id.clone(),
                        path_on_host: drive.path_on_host.display().to_string(),
                        is_root_device: drive.is_root_device,
                        is_read_only: drive.is_read_only,
                        partuuid: None,
                        rate_limiter: None,
                    },
                )
                .await?;
        }

        Ok(())
    }

    /// Serialize to JSON string (for debugging/logging).
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("FirecrackerConfig serialization failed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_key_deterministic() {
        let config1 = FirecrackerConfig::new(
            "/mnt/fcvm-btrfs/kernels/vmlinux-abc123.bin".into(),
            "/mnt/fcvm-btrfs/initrd/fc-agent-def456.initrd".into(),
            "/mnt/fcvm-btrfs/rootfs/layer2-789abc.raw".into(),
            "nginx:alpine".to_string(),
            2,
            2048,
            NetworkMode::Bridged,
        );

        let config2 = FirecrackerConfig::new(
            "/mnt/fcvm-btrfs/kernels/vmlinux-abc123.bin".into(),
            "/mnt/fcvm-btrfs/initrd/fc-agent-def456.initrd".into(),
            "/mnt/fcvm-btrfs/rootfs/layer2-789abc.raw".into(),
            "nginx:alpine".to_string(),
            2,
            2048,
            NetworkMode::Bridged,
        );

        assert_eq!(config1.cache_key(), config2.cache_key());
    }

    #[test]
    fn test_cache_key_changes_with_config() {
        let config1 = FirecrackerConfig::new(
            "/mnt/fcvm-btrfs/kernels/vmlinux-abc123.bin".into(),
            "/mnt/fcvm-btrfs/initrd/fc-agent-def456.initrd".into(),
            "/mnt/fcvm-btrfs/rootfs/layer2-789abc.raw".into(),
            "nginx:alpine".to_string(),
            2,
            2048,
            NetworkMode::Bridged,
        );

        // Different network mode
        let config2 = FirecrackerConfig::new(
            "/mnt/fcvm-btrfs/kernels/vmlinux-abc123.bin".into(),
            "/mnt/fcvm-btrfs/initrd/fc-agent-def456.initrd".into(),
            "/mnt/fcvm-btrfs/rootfs/layer2-789abc.raw".into(),
            "nginx:alpine".to_string(),
            2,
            2048,
            NetworkMode::Rootless,
        );

        assert_ne!(config1.cache_key(), config2.cache_key());
    }
}
