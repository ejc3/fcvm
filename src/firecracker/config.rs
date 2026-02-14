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
    /// Container command (affects what runs after container starts)
    pub container_cmd: Option<Vec<String>>,
    /// Network mode (bridged or rootless)
    pub network_mode: NetworkMode,
    /// Data directory for mutable VM data (vm-disks, state).
    /// Included in cache key because Firecracker snapshots store absolute paths.
    /// Different data_dirs (e.g., root vs non-root) must use separate caches.
    pub data_dir: PathBuf,
    /// Extra disk specifications (--disk, --disk-dir, --nfs).
    /// These add block devices that must match between cache create and restore.
    /// Format: "host_spec:guest_mount[:ro]" - host_spec included because content matters.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_disks: Vec<String>,
    /// Environment variables passed to the container.
    /// Format: "KEY=value" - affects container behavior so must be in cache key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_vars: Vec<String>,
    /// Volume mount specifications.
    /// Format: "host_path:guest_path[:ro]" - affects MMDS plan so must be in cache key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volume_mounts: Vec<String>,
    /// Whether container runs in privileged mode.
    /// Affects container capabilities and MMDS plan.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub privileged: bool,
    /// Whether to allocate a TTY for the container.
    /// Affects MMDS plan and container PTY allocation.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tty: bool,
    /// Whether stdin is forwarded to the container.
    /// Affects MMDS plan and container stdin handling.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub interactive: bool,
    /// Minimum free space on root filesystem (e.g., "10G").
    /// Affects disk size after CoW copy, so must be in cache key.
    #[serde(default = "default_rootfs_size")]
    pub rootfs_size: String,
    /// Health check URL for the VM (e.g., "http://localhost/").
    /// Part of cache key because it's a property of the VM configuration —
    /// clones must inherit the same health check behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_check_url: Option<String>,
}

fn default_rootfs_size() -> String {
    "10G".to_string()
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
    /// 2MB hugepage backing ("2M" or None)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub huge_pages: Option<String>,
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
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kernel_path: PathBuf,
        initrd_path: PathBuf,
        rootfs_path: PathBuf,
        container_image: String,
        container_cmd: Option<Vec<String>>,
        cpu: u8,
        mem: u32,
        network_mode: NetworkMode,
        data_dir: PathBuf,
        extra_disks: Vec<String>,
        env_vars: Vec<String>,
        volume_mounts: Vec<String>,
        privileged: bool,
        tty: bool,
        interactive: bool,
        rootfs_size: String,
        health_check_url: Option<String>,
        hugepages: bool,
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
                huge_pages: if hugepages {
                    Some("2M".to_string())
                } else {
                    None
                },
            },
            drives: vec![Drive {
                drive_id: "rootfs".to_string(),
                path_on_host: rootfs_path,
                is_root_device: true,
                is_read_only: false,
            }],
            container_image,
            container_cmd,
            network_mode,
            data_dir,
            extra_disks,
            env_vars,
            volume_mounts,
            privileged,
            tty,
            interactive,
            rootfs_size,
            health_check_url,
        }
    }

    /// Compute snapshot key by hashing the JSON representation.
    pub fn snapshot_key(&self) -> String {
        use crate::setup::rootfs::compute_sha256;
        let json = serde_json::to_string(self).expect("FirecrackerConfig serialization failed");
        compute_sha256(json.as_bytes())[..12].to_string()
    }

    /// Return a copy of this config with the rootfs path replaced.
    ///
    /// This is used when launching a VM: the snapshot key is computed using the
    /// content-addressed base rootfs path, but the actual launch uses a
    /// per-instance CoW copy path.
    pub fn with_rootfs_path(&self, new_rootfs_path: PathBuf) -> Self {
        let mut config = self.clone();
        for drive in &mut config.drives {
            if drive.is_root_device {
                drive.path_on_host = new_rootfs_path.clone();
            }
        }
        config
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
        // Hugepages negate dirty page tracking benefits (KVM forces 4K page tables),
        // but diff snapshots still work via mincore(2) fallback.
        let track_dirty_pages = self.machine_config.huge_pages.is_none();
        client
            .set_machine_config(super::api::MachineConfig {
                vcpu_count: self.machine_config.vcpu_count,
                mem_size_mib: self.machine_config.mem_size_mib,
                smt: Some(false),
                cpu_template: None,
                track_dirty_pages: Some(track_dirty_pages),
                huge_pages: self.machine_config.huge_pages.clone(),
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

    /// Helper to create a default test config with optional overrides
    fn test_config() -> FirecrackerConfig {
        FirecrackerConfig::new(
            "/mnt/fcvm-btrfs/kernels/vmlinux-abc123.bin".into(),
            "/mnt/fcvm-btrfs/initrd/fc-agent-def456.initrd".into(),
            "/mnt/fcvm-btrfs/rootfs/layer2-789abc.raw".into(),
            "nginx:alpine".to_string(),
            None,
            2,
            2048,
            NetworkMode::Bridged,
            "/mnt/fcvm-btrfs".into(),
            vec![],
            vec![],
            vec![],
            false,
            false,
            false,
            "10G".to_string(),
            None,
            false,
        )
    }

    #[test]
    fn test_snapshot_key_deterministic() {
        let config1 = test_config();
        let config2 = test_config();
        assert_eq!(config1.snapshot_key(), config2.snapshot_key());
    }

    #[test]
    fn test_snapshot_key_changes_with_config() {
        let config1 = test_config();
        let mut config2 = test_config();
        config2.network_mode = NetworkMode::Rootless;
        assert_ne!(config1.snapshot_key(), config2.snapshot_key());
    }

    #[test]
    fn test_snapshot_key_changes_with_cmd() {
        let config1 = test_config();
        let mut config2 = test_config();
        config2.container_cmd = Some(vec!["true".to_string()]);
        assert_ne!(config1.snapshot_key(), config2.snapshot_key());
    }

    #[test]
    fn test_snapshot_key_changes_with_extra_disks() {
        let config1 = test_config();
        let mut config2 = test_config();
        config2.extra_disks = vec!["/tmp/data:/mydata:ro".to_string()];
        assert_ne!(config1.snapshot_key(), config2.snapshot_key());
    }

    #[test]
    fn test_snapshot_key_changes_with_env_vars() {
        let config1 = test_config();
        let mut config2 = test_config();
        config2.env_vars = vec!["MY_VAR=test_value".to_string()];
        assert_ne!(config1.snapshot_key(), config2.snapshot_key());
    }

    #[test]
    fn test_snapshot_key_changes_with_volumes() {
        let config1 = test_config();
        let mut config2 = test_config();
        config2.volume_mounts = vec!["/tmp/data:/data:ro".to_string()];
        assert_ne!(config1.snapshot_key(), config2.snapshot_key());
    }

    #[test]
    fn test_snapshot_key_changes_with_privileged() {
        let config1 = test_config();
        let mut config2 = test_config();
        config2.privileged = true;
        assert_ne!(config1.snapshot_key(), config2.snapshot_key());
    }

    #[test]
    fn test_snapshot_key_changes_with_tty() {
        let config1 = test_config();
        let mut config2 = test_config();
        config2.tty = true;
        assert_ne!(config1.snapshot_key(), config2.snapshot_key());
    }

    #[test]
    fn test_snapshot_key_changes_with_interactive() {
        let config1 = test_config();
        let mut config2 = test_config();
        config2.interactive = true;
        assert_ne!(config1.snapshot_key(), config2.snapshot_key());
    }

    #[test]
    fn test_snapshot_key_changes_with_data_dir() {
        // Different data_dirs must produce different snapshot keys
        // This ensures root and non-root snapshots don't collide
        let config1 = test_config();
        let mut config2 = test_config();
        config2.data_dir = "/mnt/fcvm-btrfs/root".into();
        assert_ne!(config1.snapshot_key(), config2.snapshot_key());
    }

    #[test]
    fn test_snapshot_key_changes_with_hugepages() {
        let config1 = test_config();
        let mut config2 = test_config();
        config2.machine_config.huge_pages = Some("2M".to_string());
        assert_ne!(config1.snapshot_key(), config2.snapshot_key());
    }

    #[test]
    fn test_snapshot_key_changes_with_health_check_url() {
        let config1 = test_config();
        let mut config2 = test_config();
        config2.health_check_url = Some("http://localhost/".to_string());
        assert_ne!(config1.snapshot_key(), config2.snapshot_key());
    }
}
