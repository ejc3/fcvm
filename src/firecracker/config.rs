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
    /// Container image identifier (digest for localhost images, name for remote).
    /// Used in snapshot cache key computation.
    pub container_image: String,
    /// Original image name for the MMDS plan (what the guest uses to find the image).
    /// Excluded from cache key — content hash in container_image handles cache correctness.
    #[serde(skip)]
    pub container_image_name: String,
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
    /// Non-blocking output: fc-agent drops container output when channel is full.
    /// Part of cache key because fc-agent reads this from the Plan at boot,
    /// and that value is baked into the snapshot memory.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub non_blocking_output: bool,
    /// Minimum free space on root filesystem (e.g., "10G").
    /// Affects disk size after CoW copy, so must be in cache key.
    #[serde(default = "default_rootfs_size")]
    pub rootfs_size: String,
    /// Health check URL for the VM (e.g., "http://localhost/").
    /// Part of cache key because it's a property of the VM configuration —
    /// clones must inherit the same health check behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_check_url: Option<String>,
    /// User specification (uid:gid) for rootless podman inside the VM.
    /// Triggers --userns=keep-id in fc-agent. Must be in cache key because
    /// it changes how podman sets up user namespaces and storage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Published port mappings (host:guest forwarding).
    /// Part of VM identity — clones inherit these from snapshot metadata.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub port_mappings: Vec<crate::network::PortMapping>,
    /// Ports to forward from guest localhost to host localhost.
    /// Affects fc-agent's iptables setup, must be in cache key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forward_localhost: Vec<u16>,
    /// How localhost images are delivered to the guest.
    /// Affects whether guest mounts overlay store, btrfs store, or runs podman load.
    #[serde(default = "default_image_mode")]
    pub image_mode: ImageMode,
    /// Root filesystem type ("ext4" or "btrfs").
    /// Different rootfs types produce different VM states and must not share snapshots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rootfs_type: Option<String>,
    /// IPv6 prefix for routed mode (--ipv6-prefix).
    /// Part of cache key so a run requesting a different prefix never silently
    /// reuses a snapshot recorded with another prefix (the restore path applies
    /// the prefix stored in snapshot metadata, not the CLI flag).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipv6_prefix: Option<String>,
    /// Portable FUSE volumes (--portable-volumes).
    /// Part of cache key because per-volume inode tables are baked into the
    /// snapshot at create time — a portable run must not reuse a non-portable
    /// snapshot (and vice versa).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub portable_volumes: bool,
    /// Firecracker binary path used to create this snapshot.
    /// Content-addressed (e.g., firecracker-default-76c9e1236dab.bin), so changing
    /// the binary automatically invalidates the cache. Required because snapshots
    /// created by one FC version cannot be restored by another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firecracker_bin: Option<PathBuf>,
    /// Guest failpoint spec (FCVM_GUEST_FAILPOINT), forwarded to fc-agent on the
    /// kernel cmdline as `fcvm_failpoint=`. The runtime boot-args string is
    /// excluded from the cache key, so this dedicated field puts the spec in the
    /// key: a snapshot whose guest booted with failpoints armed must never be
    /// restored by a normal run (and vice versa) — fuzz VMs get their own cache
    /// entries. None (the default) is skip-serialized so existing keys are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest_failpoint: Option<String>,
    /// Build identity (inode:size:mtime) of the attached image-delivery disk
    /// (overlay storage image or Docker archive). The disk's PATH is
    /// content-addressed by image digest and so survives rebuilds — but
    /// `podman load` randomizes overlay layer link IDs per build, so a
    /// pre-start snapshot that provisioned its container against one build
    /// fails against another ("readlink .../overlay/l/<id>: no such file or
    /// directory", 2026-08-13). Keying on the build identity turns a rebuilt
    /// disk into a snapshot cache miss. None (registry-pulled images, no
    /// attached disk) is skip-serialized so those keys are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_disk_identity: Option<String>,
}

impl Default for FirecrackerConfig {
    fn default() -> Self {
        Self {
            boot_source: BootSource::default(),
            machine_config: MachineConfig::default(),
            drives: Vec::new(),
            container_image: String::new(),
            container_image_name: String::new(),
            container_cmd: None,
            network_mode: NetworkMode::default(),
            data_dir: PathBuf::new(),
            extra_disks: Vec::new(),
            env_vars: Vec::new(),
            volume_mounts: Vec::new(),
            privileged: false,
            tty: false,
            interactive: false,
            non_blocking_output: false,
            rootfs_size: "10G".to_string(),
            health_check_url: None,
            user: None,
            port_mappings: Vec::new(),
            forward_localhost: Vec::new(),
            image_mode: ImageMode::Overlay,
            rootfs_type: None,
            ipv6_prefix: None,
            portable_volumes: false,
            firecracker_bin: None,
            guest_failpoint: None,
            image_disk_identity: None,
        }
    }
}

fn default_image_mode() -> ImageMode {
    ImageMode::Overlay
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

impl Default for BootSource {
    fn default() -> Self {
        Self {
            kernel_image_path: PathBuf::new(),
            initrd_path: PathBuf::new(),
            boot_args: static_boot_args().to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    Bridged,
    /// Default for FcNetworkMode is Rootless (safest — no root required).
    /// The CLI also defaults to Rootless (`--network rootless`).
    /// Use `.into()` to convert from `cli::args::NetworkMode`.
    #[default]
    Rootless,
    Routed,
}

impl From<crate::cli::args::NetworkMode> for NetworkMode {
    fn from(mode: crate::cli::args::NetworkMode) -> Self {
        match mode {
            crate::cli::args::NetworkMode::Bridged => NetworkMode::Bridged,
            crate::cli::args::NetworkMode::Rootless => NetworkMode::Rootless,
            crate::cli::args::NetworkMode::Routed => NetworkMode::Routed,
        }
    }
}

/// How localhost container images are delivered to the guest VM.
/// Part of snapshot cache key — different modes produce different VM states.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ImageMode {
    /// Pre-built overlay storage image as additionalImageStore (read-only, instant)
    #[default]
    Overlay,
    /// Pre-built btrfs storage image with real subvolumes as graphroot (read-write, instant)
    Btrfs,
    /// Docker archive loaded via podman at boot (slow, works with any driver)
    Archive,
}

impl std::fmt::Display for ImageMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImageMode::Overlay => write!(f, "overlay"),
            ImageMode::Btrfs => write!(f, "btrfs"),
            ImageMode::Archive => write!(f, "archive"),
        }
    }
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
    /// Compute snapshot key by hashing the JSON representation.
    pub fn snapshot_key(&self) -> String {
        use crate::setup::rootfs::compute_sha256;
        // SnapshotConfig generation IDs are required as of schema 2. Include the
        // schema in the cache key so an old cache directory cannot become a
        // permanent parse miss that also blocks creation at the same tag.
        const SNAPSHOT_SCHEMA_VERSION: u32 = 2;
        let json = serde_json::to_string(&(SNAPSHOT_SCHEMA_VERSION, self))
            .expect("FirecrackerConfig serialization failed");
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
    ///
    /// `track_dirty_pages`: enable KVM dirty page tracking for diff snapshots.
    /// Should be true when creating a snapshot cache (need accurate diffs),
    /// false when snapshots are disabled (avoids splitting hugepage 2MB Stage 2
    /// block mappings to 4K).
    pub async fn apply(
        &self,
        client: &super::api::FirecrackerClient,
        runtime_boot_args: &str,
        track_dirty_pages: bool,
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
        // Dirty page tracking enables diff snapshots but has a cost with hugepages:
        // KVM splits 2MB Stage 2 block mappings to 4K for per-page tracking.
        // Only enable when we'll actually create a snapshot (cache miss path).
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

    /// Build the MMDS container-plan JSON.
    ///
    /// User-input fields come from `self` (part of snapshot key).
    /// Runtime parameters are values that legitimately differ between launches
    /// using the same snapshot (network config, resolved proxies, timestamps).
    pub fn to_mmds_json(&self, runtime: MmdsRuntime) -> serde_json::Value {
        // Parse env vars from "KEY=value" format to HashMap
        let env: std::collections::HashMap<&str, &str> = self
            .env_vars
            .iter()
            .map(|e| {
                let parts: Vec<&str> = e.splitn(2, '=').collect();
                (parts[0], parts.get(1).copied().unwrap_or(""))
            })
            .collect();
        let mut seen_guest_ports = std::collections::HashSet::new();
        let published_guest_ports = self
            .port_mappings
            .iter()
            .filter(|mapping| matches!(mapping.proto, crate::network::Protocol::Tcp))
            .filter(|mapping| seen_guest_ports.insert(mapping.guest_port))
            .map(|mapping| mapping.guest_port.to_string())
            .collect::<Vec<_>>();

        serde_json::json!({
            "latest": {
                "container-plan": {
                    "image": self.container_image_name,
                    "env": env,
                    "cmd": self.container_cmd,
                    "volumes": runtime.volumes,
                    "extra_disks": runtime.extra_disks,
                    "nfs_mounts": runtime.nfs_mounts,
                    "image_device": runtime.image_device,
                    "image_mode": runtime.image_device.as_ref().map(|_| self.image_mode.to_string()),
                    "privileged": self.privileged,
                    "user": self.user.as_deref(),
                    "subuid_start": runtime.subuid_start,
                    "subuid_count": runtime.subuid_count,
                    "non_blocking_output": self.non_blocking_output,
                    "forward_localhost": self.forward_localhost.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
                    // Guest ports reachable from the host. fc-agent DNATs each to
                    // 127.0.0.1 so a service that binds guest loopback ONLY is still
                    // reachable through --publish. Chromium's CDP is the motivating
                    // case: it ignores --remote-debugging-address and binds
                    // 127.0.0.1:9222 regardless, so without this the only way in was a
                    // userspace relay inside the guest.
                    "published_guest_ports": published_guest_ports,
                    "egress_proxy": matches!(self.network_mode, NetworkMode::Rootless),
                    "interactive": self.interactive,
                    "tty": self.tty,
                    "http_proxy": runtime.http_proxy,
                    "https_proxy": runtime.https_proxy,
                    "no_proxy": runtime.no_proxy,
                    "ntp_servers": runtime.ntp_servers,
                },
                "host-time": runtime.host_time,
            }
        })
    }
}

/// Runtime-only MMDS parameters that differ between launches using the same snapshot.
/// These are NOT part of the cache key.
pub struct MmdsRuntime {
    /// Volume mount details with assigned vsock ports
    pub volumes: Vec<serde_json::Value>,
    /// Extra disk device assignments (e.g., /dev/vdb)
    pub extra_disks: Vec<serde_json::Value>,
    /// NFS mount details with host IP
    pub nfs_mounts: Vec<serde_json::Value>,
    /// Device path for localhost image (e.g., "/dev/vdb"), used by all image modes
    pub image_device: Option<String>,
    /// Resolved HTTP proxy URL (IP, not hostname)
    pub http_proxy: Option<String>,
    /// Resolved HTTPS proxy URL
    pub https_proxy: Option<String>,
    /// NO_PROXY value from environment
    pub no_proxy: Option<String>,
    /// Host user's subordinate UID range start (from /etc/subuid)
    pub subuid_start: Option<u64>,
    /// Host user's subordinate UID range count
    pub subuid_count: Option<u64>,
    /// Host timestamp (UTC epoch seconds)
    pub host_time: String,
    /// The host's NTP servers, already resolved to addresses, for the guest's chronyd.
    ///
    /// Resolved host-side for the same reason proxies are: the guest writes them
    /// into chrony.conf as `server <addr>` directives, and shipping addresses
    /// means the guest needs no DNS of its own to end up with real sources.
    pub ntp_servers: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a default test config with optional overrides
    fn test_config() -> FirecrackerConfig {
        FirecrackerConfig {
            boot_source: BootSource {
                kernel_image_path: "/mnt/fcvm-btrfs/kernels/vmlinux-abc123.bin".into(),
                initrd_path: "/mnt/fcvm-btrfs/initrd/fc-agent-def456.initrd".into(),
                // Fixed boot args (not the arch-dependent static default, which uses
                // reboot=t on x86 vs reboot=k on ARM64) so the golden snapshot_key below
                // is identical on x86_64 and aarch64.
                boot_args: "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw".to_string(),
            },
            machine_config: MachineConfig {
                vcpu_count: 2,
                mem_size_mib: 2048,
                ..Default::default()
            },
            drives: vec![Drive {
                drive_id: "rootfs".to_string(),
                path_on_host: "/mnt/fcvm-btrfs/rootfs/layer2-789abc.raw".into(),
                is_root_device: true,
                is_read_only: false,
            }],
            container_image: "nginx:alpine".to_string(),
            container_image_name: "nginx:alpine".to_string(),
            network_mode: NetworkMode::Bridged,
            data_dir: "/mnt/fcvm-btrfs".into(),
            ..Default::default()
        }
    }

    #[test]
    fn test_snapshot_key_deterministic() {
        let config1 = test_config();
        let config2 = test_config();
        assert_eq!(config1.snapshot_key(), config2.snapshot_key());
    }

    /// Byte-identity guard (#632 P0). `snapshot_key()` hashes the serialized JSON of
    /// `FirecrackerConfig`, so ANY change to this struct's serialization — a renamed or
    /// reordered field, or a new field that isn't `skip`/`skip_serializing_if` for its
    /// default — changes the key and silently invalidates every cached snapshot. This
    /// pins the key of a known config so such a change fails loudly. If you intend to
    /// change the schema (e.g. add a backend dimension to the cache key), update this
    /// hash deliberately and document the cache invalidation.
    #[test]
    fn test_snapshot_key_golden() {
        assert_eq!(test_config().snapshot_key(), "4278f265ad63");
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

    /// Guest failpoints ride the kernel cmdline and are baked into the snapshot,
    /// so a run with FCVM_GUEST_FAILPOINT set must compute a different key —
    /// fuzz VMs never pollute (or reuse) normal snapshot caches.
    #[test]
    fn test_snapshot_key_changes_with_guest_failpoint() {
        let config1 = test_config();
        let mut config2 = test_config();
        config2.guest_failpoint = Some("exec.post_accept_pre_read:sleep:100".to_string());
        assert_ne!(config1.snapshot_key(), config2.snapshot_key());
    }

    #[test]
    fn test_snapshot_key_changes_with_image_disk_identity() {
        // The image disk's PATH is content-addressed by image digest and does
        // not change on rebuild — but `podman load` randomizes overlay link
        // IDs per build, so a snapshot provisioned against one build must
        // never restore against another. Two builds at the same path differ
        // only in this identity; the key must differ with it, and a rebuild
        // must also miss a snapshot created before the field existed (None).
        let config1 = test_config();
        let mut config2 = test_config();
        config2.image_disk_identity = Some("1841:73678848:1765574672.000000000".to_string());
        let mut config3 = test_config();
        config3.image_disk_identity = Some("2007:73678848:1765581300.000000000".to_string());
        assert_ne!(config1.snapshot_key(), config2.snapshot_key());
        assert_ne!(config2.snapshot_key(), config3.snapshot_key());
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

    #[test]
    fn test_snapshot_key_changes_with_user() {
        let config1 = test_config();
        let mut config2 = test_config();
        config2.user = Some("1000:1000".to_string());
        assert_ne!(config1.snapshot_key(), config2.snapshot_key());
    }

    #[test]
    fn test_snapshot_key_changes_with_forward_localhost() {
        let config1 = test_config();
        let mut config2 = test_config();
        config2.forward_localhost = vec![8080];
        assert_ne!(config1.snapshot_key(), config2.snapshot_key());
    }

    #[test]
    fn test_snapshot_key_changes_with_image_mode() {
        let config1 = test_config();
        let mut config2 = test_config();
        config2.image_mode = ImageMode::Btrfs;
        assert_ne!(config1.snapshot_key(), config2.snapshot_key());

        let mut config3 = test_config();
        config3.image_mode = ImageMode::Archive;
        assert_ne!(config1.snapshot_key(), config3.snapshot_key());
        assert_ne!(config2.snapshot_key(), config3.snapshot_key());
    }

    #[test]
    fn test_snapshot_key_changes_with_rootfs_type() {
        let config1 = test_config();
        let mut config2 = test_config();
        config2.rootfs_type = Some("btrfs".to_string());
        assert_ne!(config1.snapshot_key(), config2.snapshot_key());
    }

    #[test]
    fn test_snapshot_key_changes_with_ipv6_prefix() {
        let config1 = test_config();
        let mut config2 = test_config();
        config2.ipv6_prefix = Some("2001:db8::/64".to_string());
        assert_ne!(config1.snapshot_key(), config2.snapshot_key());
    }

    #[test]
    fn test_snapshot_key_changes_with_portable_volumes() {
        let config1 = test_config();
        let mut config2 = test_config();
        config2.portable_volumes = true;
        assert_ne!(config1.snapshot_key(), config2.snapshot_key());
    }

    #[test]
    fn test_snapshot_key_changes_with_firecracker_bin() {
        let config1 = test_config();
        let mut config2 = test_config();
        config2.firecracker_bin = Some(PathBuf::from("/path/to/firecracker"));
        assert_ne!(config1.snapshot_key(), config2.snapshot_key());
    }

    #[test]
    fn test_snapshot_key_changes_with_port_mappings() {
        let config1 = test_config();
        let mut config2 = test_config();
        config2.port_mappings = vec![crate::network::PortMapping {
            host_ip: None,
            host_port: 8080,
            guest_port: 80,
            proto: crate::network::types::Protocol::Tcp,
        }];
        assert_ne!(config1.snapshot_key(), config2.snapshot_key());
    }

    #[test]
    fn mmds_publishes_each_tcp_guest_port_once() {
        let mut config = test_config();
        config.port_mappings = vec![
            crate::network::PortMapping {
                host_ip: None,
                host_port: 8080,
                guest_port: 80,
                proto: crate::network::types::Protocol::Tcp,
            },
            crate::network::PortMapping {
                host_ip: None,
                host_port: 8081,
                guest_port: 80,
                proto: crate::network::types::Protocol::Tcp,
            },
            crate::network::PortMapping {
                host_ip: None,
                host_port: 8082,
                guest_port: 80,
                proto: crate::network::types::Protocol::Udp,
            },
            crate::network::PortMapping {
                host_ip: None,
                host_port: 8083,
                guest_port: 81,
                proto: crate::network::types::Protocol::Tcp,
            },
        ];
        let plan = config.to_mmds_json(MmdsRuntime {
            volumes: vec![],
            extra_disks: vec![],
            nfs_mounts: vec![],
            image_device: None,
            http_proxy: None,
            https_proxy: None,
            no_proxy: None,
            subuid_start: None,
            subuid_count: None,
            host_time: "0".to_string(),
            ntp_servers: vec![],
        });

        assert_eq!(
            plan.pointer("/latest/container-plan/published_guest_ports"),
            Some(&serde_json::json!(["80", "81"]))
        );
    }
}
