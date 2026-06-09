use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::fs;
use tracing::info;

use crate::network::NetworkConfig;

fn default_health_check_timeout() -> u64 {
    5
}

/// Type of snapshot - distinguishes user-created from system-generated
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum SnapshotType {
    /// Created by user via `fcvm snapshot create`
    User,
    /// Auto-created by podman snapshot feature (cache)
    #[default]
    System,
}

impl std::fmt::Display for SnapshotType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnapshotType::User => write!(f, "user"),
            SnapshotType::System => write!(f, "system"),
        }
    }
}

/// Kind of snapshot — distinguishes a full memory+disk snapshot from a
/// disk-only snapshot (no memory image; clones cold-boot from the captured disk).
/// Defaults to Full for backward compatibility with existing snapshots.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum SnapshotKind {
    /// Memory + disk snapshot; clones resume mid-execution via UFFD.
    #[default]
    Full,
    /// Disk-only snapshot; clones cold-boot fresh from the captured disk.
    DiskOnly,
}

impl std::fmt::Display for SnapshotKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnapshotKind::Full => write!(f, "full"),
            SnapshotKind::DiskOnly => write!(f, "disk-only"),
        }
    }
}

/// Snapshot configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotConfig {
    pub name: String,
    pub vm_id: String,
    /// Original VM ID for vsock socket path redirect.
    /// This is the VM ID whose path is stored in vmstate.bin.
    /// When a VM is restored from cache/snapshot, its vmstate still references
    /// the original VM's paths. When snapshotting such a VM, we preserve this
    /// original_vm_id so clones use the correct redirect path.
    /// Defaults to vm_id if not set (for snapshots of fresh VMs).
    #[serde(default)]
    pub original_vsock_vm_id: Option<String>,
    /// Parent snapshot name used as the diff base for this snapshot's memory.
    /// - Full snapshots: None (no diff base needed)
    /// - Diff snapshots: Some("parent-name") — memory.bin was created by merging
    ///   the diff onto parent's memory.bin
    ///
    /// This creates a walkable chain: startup → pre-start → None.
    /// Used for validation: the diff base must come from the same VM or its
    /// restore-source to avoid memory corruption.
    #[serde(default)]
    pub parent_snapshot: Option<String>,
    pub memory_path: PathBuf,
    pub vmstate_path: PathBuf,
    pub disk_path: PathBuf,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Type of snapshot: User (explicit) or System (auto-generated cache)
    /// Defaults to System for backward compatibility with existing snapshots
    #[serde(default)]
    pub snapshot_type: SnapshotType,
    /// Full (memory+disk) vs DiskOnly (no memory image — clones cold-boot).
    /// Defaults to Full so existing snapshots deserialize unchanged.
    #[serde(default)]
    pub kind: SnapshotKind,
    pub metadata: SnapshotMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMetadata {
    pub image: String,
    pub vcpu: u8,
    pub memory_mib: u32,
    pub network_config: NetworkConfig,
    /// Volume mounts from the baseline VM (for clone volume support)
    #[serde(default)]
    pub volumes: Vec<SnapshotVolumeConfig>,
    /// Health check URL from the baseline VM (None = no HTTP health check)
    #[serde(default)]
    pub health_check_url: Option<String>,
    /// Health check timeout in seconds (default 5)
    #[serde(default = "default_health_check_timeout")]
    pub health_check_timeout: u64,
    /// Whether VM uses 2MB hugepage-backed memory
    #[serde(default)]
    pub hugepages: bool,
    /// Extra disk images from the baseline VM (for clone disk-dir support)
    #[serde(default)]
    pub extra_disks: Vec<SnapshotExtraDisk>,
    /// Username for rootless container health checks (e.g., "ubuntu")
    #[serde(default)]
    pub username: Option<String>,
    /// User spec (UID:GID) for rootless podman in the VM
    #[serde(default)]
    pub user: Option<String>,
    /// Published port mappings inherited by clones
    #[serde(default)]
    pub port_mappings: Vec<crate::network::PortMapping>,
    /// Guest localhost ports forwarded to the host's 127.0.0.1 (--forward-localhost),
    /// inherited by clones so routed mode re-establishes the host-side relay on restore
    #[serde(default)]
    pub forward_localhost: Vec<u16>,
    /// Network mode (bridged, rootless, routed) inherited by clones
    #[serde(default)]
    pub network_mode: crate::firecracker::FcNetworkMode,
    /// Explicit routable IPv6 /64 prefix for routed mode (skips auto-detect and MASQUERADE)
    #[serde(default)]
    pub ipv6_prefix: Option<String>,
    /// Whether PTY is allocated for the container
    #[serde(default)]
    pub tty: bool,
    /// Whether stdin is forwarded to the container
    #[serde(default)]
    pub interactive: bool,
}

/// Extra disk configuration saved in snapshot metadata.
/// Used to copy disk-dir images when restoring clones.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotExtraDisk {
    /// Filename of disk image within snapshot directory (e.g., "disk-dir-0.raw")
    pub filename: String,
    /// Mount path inside guest
    pub mount_path: String,
    /// Read-only flag
    pub read_only: bool,
    /// Firecracker drive ID (e.g., "disk0")
    pub drive_id: String,
}

/// Volume configuration saved in snapshot metadata.
/// Used to start VolumeServers when serving snapshot for clones.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotVolumeConfig {
    /// Path on host filesystem
    pub host_path: PathBuf,
    /// Mount path inside guest
    pub guest_path: String,
    /// Read-only flag
    pub read_only: bool,
    /// Vsock port number
    pub vsock_port: u32,
    /// Use portable inode numbering
    #[serde(default)]
    pub portable: bool,
}

/// Manages VM snapshots
pub struct SnapshotManager {
    snapshots_dir: PathBuf,
}

impl SnapshotManager {
    pub fn new(snapshots_dir: PathBuf) -> Self {
        Self { snapshots_dir }
    }

    /// Save a snapshot
    pub async fn save_snapshot(&self, config: SnapshotConfig) -> Result<()> {
        info!(
            snapshot = %config.name,
            vm_id = %config.vm_id,
            "saving snapshot"
        );

        // Create snapshot directory
        let snapshot_dir = self.snapshots_dir.join(&config.name);
        fs::create_dir_all(&snapshot_dir)
            .await
            .context("creating snapshot directory")?;

        // Save metadata
        let metadata_path = snapshot_dir.join("config.json");
        let metadata_json = serde_json::to_string_pretty(&config)?;
        fs::write(&metadata_path, metadata_json)
            .await
            .context("writing snapshot metadata")?;

        info!(
            snapshot = %config.name,
            memory = %config.memory_path.display(),
            disk = %config.disk_path.display(),
            "snapshot saved successfully"
        );

        Ok(())
    }

    /// Load a snapshot configuration
    pub async fn load_snapshot(&self, name: &str) -> Result<SnapshotConfig> {
        let snapshot_dir = self.snapshots_dir.join(name);
        let metadata_path = snapshot_dir.join("config.json");

        if !metadata_path.exists() {
            anyhow::bail!("snapshot '{}' not found", name);
        }

        let metadata_json = fs::read_to_string(&metadata_path)
            .await
            .context("reading snapshot metadata")?;

        let config: SnapshotConfig =
            serde_json::from_str(&metadata_json).context("parsing snapshot metadata")?;

        Ok(config)
    }

    /// List all snapshots
    pub async fn list_snapshots(&self) -> Result<Vec<String>> {
        let mut snapshots = Vec::new();

        if !self.snapshots_dir.exists() {
            return Ok(snapshots);
        }

        let mut entries = fs::read_dir(&self.snapshots_dir)
            .await
            .context("reading snapshots directory")?;

        while let Some(entry) = entries.next_entry().await? {
            if entry.file_type().await?.is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    snapshots.push(name.to_string());
                }
            }
        }

        Ok(snapshots)
    }

    /// Delete a snapshot
    pub async fn delete_snapshot(&self, name: &str) -> Result<()> {
        let snapshot_dir = self.snapshots_dir.join(name);

        if snapshot_dir.exists() {
            fs::remove_dir_all(&snapshot_dir)
                .await
                .context("removing snapshot directory")?;

            info!(snapshot = name, "snapshot deleted");
        }

        // Also remove the lock file if it exists
        let lock_file = self.snapshots_dir.join(format!("{}.lock", name));
        if lock_file.exists() {
            let _ = fs::remove_file(&lock_file).await;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_config_json_roundtrip() {
        let config = SnapshotConfig {
            name: "test-snapshot".to_string(),
            vm_id: "abc123".to_string(),
            original_vsock_vm_id: None,
            parent_snapshot: None,
            memory_path: PathBuf::from("/path/to/memory.bin"),
            vmstate_path: PathBuf::from("/path/to/vmstate.bin"),
            disk_path: PathBuf::from("/path/to/disk.raw"),
            created_at: chrono::Utc::now(),
            snapshot_type: SnapshotType::User,
            kind: SnapshotKind::Full,
            metadata: SnapshotMetadata {
                image: "nginx:alpine".to_string(),
                vcpu: 2,
                memory_mib: 512,
                network_config: NetworkConfig {
                    tap_device: "tap-abc123".to_string(),
                    guest_mac: "AA:BB:CC:DD:EE:FF".to_string(),
                    guest_ip: Some("172.30.0.2".to_string()),
                    host_ip: Some("172.30.0.1".to_string()),
                    host_veth: Some("veth0-abc123".to_string()),
                    loopback_ip: None,
                    dns_server: None,
                    guest_ipv6: None,
                    host_ipv6: None,
                    dns_search: None,
                    http_proxy: None,
                    namespace_name: None,
                },
                volumes: vec![],
                health_check_url: None,
                health_check_timeout: 5,
                hugepages: false,
                extra_disks: vec![],
                username: None,
                user: None,
                port_mappings: vec![],
                forward_localhost: vec![],
                network_mode: Default::default(),
                ipv6_prefix: None,
                tty: false,
                interactive: false,
            },
        };

        // Serialize to JSON
        let json = serde_json::to_string_pretty(&config).unwrap();

        // Deserialize back
        let parsed: SnapshotConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.name, "test-snapshot");
        assert_eq!(parsed.vm_id, "abc123");
        assert_eq!(parsed.snapshot_type, SnapshotType::User);
        assert_eq!(parsed.metadata.image, "nginx:alpine");
        assert_eq!(parsed.metadata.vcpu, 2);
        assert_eq!(parsed.metadata.memory_mib, 512);
        assert_eq!(
            parsed.metadata.network_config.guest_ip,
            Some("172.30.0.2".to_string())
        );
    }

    #[test]
    fn test_snapshot_config_json_parsing() {
        // Test parsing a JSON config like what would be saved on disk
        let json = r#"{
            "name": "nginx-snap",
            "vm_id": "def456",
            "memory_path": "/mnt/fcvm-btrfs/snapshots/nginx-snap/memory.bin",
            "vmstate_path": "/mnt/fcvm-btrfs/snapshots/nginx-snap/vmstate.bin",
            "disk_path": "/mnt/fcvm-btrfs/snapshots/nginx-snap/disk.raw",
            "created_at": "2024-01-15T10:30:00Z",
            "metadata": {
                "image": "nginx:alpine",
                "vcpu": 4,
                "memory_mib": 1024,
                "network_config": {
                    "tap_device": "tap-def456",
                    "guest_mac": "11:22:33:44:55:66",
                    "guest_ip": "172.30.100.2",
                    "host_ip": "172.30.100.1",
                    "host_veth": "veth0-def456"
                }
            }
        }"#;

        let config: SnapshotConfig = serde_json::from_str(json).unwrap();

        assert_eq!(config.name, "nginx-snap");
        assert_eq!(config.metadata.vcpu, 4);
        assert_eq!(config.metadata.memory_mib, 1024);
        assert_eq!(
            config.metadata.network_config.guest_mac,
            "11:22:33:44:55:66"
        );
    }

    #[test]
    fn test_snapshot_metadata_json_parsing() {
        let json = r#"{
            "image": "redis:alpine",
            "vcpu": 1,
            "memory_mib": 256,
            "network_config": {
                "tap_device": "tap-test",
                "guest_mac": "AA:BB:CC:DD:EE:FF",
                "guest_ip": null,
                "host_ip": null,
                "host_veth": null
            }
        }"#;

        let metadata: SnapshotMetadata = serde_json::from_str(json).unwrap();

        assert_eq!(metadata.image, "redis:alpine");
        assert_eq!(metadata.vcpu, 1);
        assert_eq!(metadata.memory_mib, 256);
        assert!(metadata.network_config.guest_ip.is_none());
    }

    #[tokio::test]
    async fn test_snapshot_manager_save_and_load() {
        // Create temp directory for test
        let temp_dir = tempfile::tempdir().unwrap();
        let manager = SnapshotManager::new(temp_dir.path().to_path_buf());

        let config = SnapshotConfig {
            name: "test-snap".to_string(),
            vm_id: "test123".to_string(),
            original_vsock_vm_id: None,
            parent_snapshot: None,
            memory_path: PathBuf::from("/memory.bin"),
            vmstate_path: PathBuf::from("/vmstate.bin"),
            disk_path: PathBuf::from("/disk.raw"),
            created_at: chrono::Utc::now(),
            snapshot_type: SnapshotType::User,
            kind: SnapshotKind::Full,
            metadata: SnapshotMetadata {
                image: "alpine:latest".to_string(),
                vcpu: 2,
                memory_mib: 512,
                network_config: NetworkConfig {
                    tap_device: "tap-test".to_string(),
                    guest_mac: "AA:BB:CC:DD:EE:FF".to_string(),
                    guest_ip: Some("172.30.0.2".to_string()),
                    host_ip: Some("172.30.0.1".to_string()),
                    host_veth: None,
                    loopback_ip: None,
                    dns_server: None,
                    guest_ipv6: None,
                    host_ipv6: None,
                    dns_search: None,
                    http_proxy: None,
                    namespace_name: None,
                },
                volumes: vec![],
                health_check_url: None,
                health_check_timeout: 5,
                hugepages: false,
                extra_disks: vec![],
                username: None,
                user: None,
                port_mappings: vec![],
                forward_localhost: vec![],
                network_mode: Default::default(),
                ipv6_prefix: None,
                tty: false,
                interactive: false,
            },
        };

        // Save snapshot
        manager.save_snapshot(config.clone()).await.unwrap();

        // Verify file was created
        let config_file = temp_dir.path().join("test-snap").join("config.json");
        assert!(config_file.exists());

        // Load and verify
        let loaded = manager.load_snapshot("test-snap").await.unwrap();
        assert_eq!(loaded.name, "test-snap");
        assert_eq!(loaded.vm_id, "test123");
        assert_eq!(loaded.snapshot_type, SnapshotType::User);
        assert_eq!(loaded.metadata.image, "alpine:latest");
    }

    #[tokio::test]
    async fn test_snapshot_manager_list() {
        let temp_dir = tempfile::tempdir().unwrap();
        let manager = SnapshotManager::new(temp_dir.path().to_path_buf());

        // Initially empty
        let list = manager.list_snapshots().await.unwrap();
        assert!(list.is_empty());

        // Create two snapshots
        for name in ["snap1", "snap2"] {
            let config = SnapshotConfig {
                name: name.to_string(),
                vm_id: format!("vm-{}", name),
                original_vsock_vm_id: None,
                parent_snapshot: None,
                memory_path: PathBuf::from("/memory.bin"),
                vmstate_path: PathBuf::from("/vmstate.bin"),
                disk_path: PathBuf::from("/disk.raw"),
                created_at: chrono::Utc::now(),
                snapshot_type: SnapshotType::System,
                kind: SnapshotKind::Full,
                metadata: SnapshotMetadata {
                    image: "alpine".to_string(),
                    vcpu: 1,
                    memory_mib: 256,
                    network_config: NetworkConfig {
                        tap_device: "tap".to_string(),
                        guest_mac: "00:00:00:00:00:00".to_string(),
                        guest_ip: None,
                        host_ip: None,
                        host_veth: None,
                        loopback_ip: None,
                        dns_server: None,
                        guest_ipv6: None,
                        host_ipv6: None,
                        dns_search: None,
                        http_proxy: None,
                        namespace_name: None,
                    },
                    volumes: vec![],
                    health_check_url: None,
                    health_check_timeout: 5,
                    hugepages: false,
                    extra_disks: vec![],
                    username: None,
                    user: None,
                    port_mappings: vec![],
                    forward_localhost: vec![],
                    network_mode: Default::default(),
                    ipv6_prefix: None,
                    tty: false,
                    interactive: false,
                },
            };
            manager.save_snapshot(config).await.unwrap();
        }

        let list = manager.list_snapshots().await.unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.contains(&"snap1".to_string()));
        assert!(list.contains(&"snap2".to_string()));
    }

    #[tokio::test]
    async fn test_snapshot_manager_delete() {
        let temp_dir = tempfile::tempdir().unwrap();
        let manager = SnapshotManager::new(temp_dir.path().to_path_buf());

        let config = SnapshotConfig {
            name: "to-delete".to_string(),
            vm_id: "vm123".to_string(),
            original_vsock_vm_id: None,
            parent_snapshot: None,
            memory_path: PathBuf::from("/memory.bin"),
            vmstate_path: PathBuf::from("/vmstate.bin"),
            disk_path: PathBuf::from("/disk.raw"),
            created_at: chrono::Utc::now(),
            snapshot_type: SnapshotType::System,
            kind: SnapshotKind::Full,
            metadata: SnapshotMetadata {
                image: "alpine".to_string(),
                vcpu: 1,
                memory_mib: 256,
                network_config: NetworkConfig {
                    tap_device: "tap".to_string(),
                    guest_mac: "00:00:00:00:00:00".to_string(),
                    guest_ip: None,
                    host_ip: None,
                    host_veth: None,
                    loopback_ip: None,
                    dns_server: None,
                    guest_ipv6: None,
                    host_ipv6: None,
                    dns_search: None,
                    http_proxy: None,
                    namespace_name: None,
                },
                volumes: vec![],
                health_check_url: None,
                health_check_timeout: 5,
                hugepages: false,
                extra_disks: vec![],
                username: None,
                user: None,
                port_mappings: vec![],
                forward_localhost: vec![],
                network_mode: Default::default(),
                ipv6_prefix: None,
                tty: false,
                interactive: false,
            },
        };
        manager.save_snapshot(config).await.unwrap();

        // Verify exists
        assert!(manager.load_snapshot("to-delete").await.is_ok());

        // Delete
        manager.delete_snapshot("to-delete").await.unwrap();

        // Verify gone
        assert!(manager.load_snapshot("to-delete").await.is_err());
    }

    #[tokio::test]
    async fn test_snapshot_manager_load_nonexistent() {
        let temp_dir = tempfile::tempdir().unwrap();
        let manager = SnapshotManager::new(temp_dir.path().to_path_buf());

        let result = manager.load_snapshot("does-not-exist").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_snapshot_type_default_is_system() {
        // Verify that SnapshotType::default() is System (for backward compatibility)
        assert_eq!(SnapshotType::default(), SnapshotType::System);
    }

    #[test]
    fn test_snapshot_type_display() {
        assert_eq!(format!("{}", SnapshotType::User), "user");
        assert_eq!(format!("{}", SnapshotType::System), "system");
    }

    #[test]
    fn test_snapshot_config_backward_compatibility() {
        // Test that JSON without snapshot_type field defaults to System
        // This ensures existing snapshots (created before this feature) load correctly
        let json = r#"{
            "name": "old-snapshot",
            "vm_id": "abc123",
            "memory_path": "/path/to/memory.bin",
            "vmstate_path": "/path/to/vmstate.bin",
            "disk_path": "/path/to/disk.raw",
            "created_at": "2024-01-15T10:30:00Z",
            "metadata": {
                "image": "nginx:alpine",
                "vcpu": 2,
                "memory_mib": 512,
                "network_config": {
                    "tap_device": "tap-test",
                    "guest_mac": "AA:BB:CC:DD:EE:FF"
                }
            }
        }"#;

        let config: SnapshotConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.name, "old-snapshot");
        // Missing snapshot_type should default to System
        assert_eq!(config.snapshot_type, SnapshotType::System);
        // Missing kind should default to Full (existing snapshots are memory+disk)
        assert_eq!(config.kind, SnapshotKind::Full);
    }

    #[test]
    fn test_snapshot_type_json_roundtrip() {
        // Test User type serializes and deserializes correctly
        let user_config = SnapshotConfig {
            name: "user-snapshot".to_string(),
            vm_id: "user123".to_string(),
            original_vsock_vm_id: None,
            parent_snapshot: None,
            memory_path: PathBuf::from("/memory.bin"),
            vmstate_path: PathBuf::from("/vmstate.bin"),
            disk_path: PathBuf::from("/disk.raw"),
            created_at: chrono::Utc::now(),
            snapshot_type: SnapshotType::User,
            kind: SnapshotKind::Full,
            metadata: SnapshotMetadata {
                image: "alpine".to_string(),
                vcpu: 1,
                memory_mib: 256,
                network_config: NetworkConfig {
                    tap_device: "tap".to_string(),
                    guest_mac: "00:00:00:00:00:00".to_string(),
                    guest_ip: None,
                    host_ip: None,
                    host_veth: None,
                    loopback_ip: None,
                    dns_server: None,
                    guest_ipv6: None,
                    host_ipv6: None,
                    dns_search: None,
                    http_proxy: None,
                    namespace_name: None,
                },
                volumes: vec![],
                health_check_url: None,
                health_check_timeout: 5,
                hugepages: false,
                extra_disks: vec![],
                username: None,
                user: None,
                port_mappings: vec![],
                forward_localhost: vec![],
                network_mode: Default::default(),
                ipv6_prefix: None,
                tty: false,
                interactive: false,
            },
        };

        let json = serde_json::to_string(&user_config).unwrap();
        let parsed: SnapshotConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.snapshot_type, SnapshotType::User);

        // Test System type
        let system_config = SnapshotConfig {
            snapshot_type: SnapshotType::System,
            ..user_config
        };

        let json = serde_json::to_string(&system_config).unwrap();
        let parsed: SnapshotConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.snapshot_type, SnapshotType::System);
    }

    #[test]
    fn test_snapshot_kind_default_is_full() {
        // Verify SnapshotKind::default() is Full (existing snapshots are memory+disk)
        assert_eq!(SnapshotKind::default(), SnapshotKind::Full);
    }

    #[test]
    fn test_snapshot_kind_display() {
        assert_eq!(format!("{}", SnapshotKind::Full), "full");
        assert_eq!(format!("{}", SnapshotKind::DiskOnly), "disk-only");
    }

    #[test]
    fn test_snapshot_kind_json_roundtrip() {
        // Both kinds round-trip through JSON unchanged.
        for kind in [SnapshotKind::Full, SnapshotKind::DiskOnly] {
            let json = serde_json::to_string(&kind).unwrap();
            let parsed: SnapshotKind = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, kind);
        }
    }

    #[test]
    fn test_snapshot_metadata_ipv6_prefix_roundtrip() {
        let metadata = SnapshotMetadata {
            image: "nginx:alpine".to_string(),
            vcpu: 2,
            memory_mib: 512,
            network_config: NetworkConfig::default(),
            volumes: vec![],
            health_check_url: None,
            health_check_timeout: 5,
            hugepages: false,
            extra_disks: vec![],
            username: None,
            user: None,
            port_mappings: vec![],
            forward_localhost: vec![],
            network_mode: Default::default(),
            ipv6_prefix: Some("2600:1f1c:494:201".to_string()),
            tty: false,
            interactive: false,
        };

        let json = serde_json::to_string(&metadata).unwrap();
        assert!(json.contains("2600:1f1c:494:201"));

        let parsed: SnapshotMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.ipv6_prefix,
            Some("2600:1f1c:494:201".to_string()),
            "ipv6_prefix must survive serialization roundtrip"
        );
    }

    #[test]
    fn test_snapshot_metadata_ipv6_prefix_backward_compat() {
        // Old snapshots won't have ipv6_prefix — must deserialize to None
        let json = r#"{
            "image": "nginx:alpine",
            "vcpu": 2,
            "memory_mib": 512,
            "network_config": {
                "tap_device": "tap-old",
                "guest_mac": "AA:BB:CC:DD:EE:FF"
            }
        }"#;

        let metadata: SnapshotMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(
            metadata.ipv6_prefix, None,
            "missing ipv6_prefix must default to None for backward compat"
        );
        assert_eq!(metadata.image, "nginx:alpine");
        assert_eq!(metadata.vcpu, 2);
    }
}
