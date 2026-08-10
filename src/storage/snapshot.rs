use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::info;

use crate::network::NetworkConfig;

/// Validate a snapshot tag before it can participate in any path operation.
pub fn validate_snapshot_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 128 {
        anyhow::bail!("snapshot name must contain 1 to 128 characters");
    }
    if name == "."
        || name == ".."
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character))
    {
        anyhow::bail!(
            "invalid snapshot name '{}': use only ASCII letters, digits, '-', '_', or '.'",
            name
        );
    }
    Ok(())
}

fn default_health_check_timeout() -> u64 {
    5
}

/// Host/guest protocol version for the immutable TCP generation boundary that
/// is captured in every current memory snapshot.
pub const SNAPSHOT_NETWORK_BOUNDARY_VERSION: u32 = 1;

/// Type of snapshot - distinguishes user-created from system-generated
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SnapshotType {
    /// Created by user via `fcvm snapshot create`
    User,
    /// Auto-created by podman snapshot feature (cache)
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
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SnapshotKind {
    /// Memory + disk snapshot; clones resume mid-execution via UFFD.
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
    /// Unique identity for this atomically-installed snapshot generation.
    ///
    /// Cache invalidation carries this value from the exact generation that a
    /// restore attempted. A later creator can replace the same tag after the
    /// restore releases its shared generation lease; conditional invalidation
    /// must not delete that replacement.
    pub generation_id: uuid::Uuid,
    /// Required restore-safety protocol. A full snapshot without this exact
    /// version may resume with external ingress live before socket cleanup, so
    /// it must be rejected by the host before the VMM is started.
    pub network_boundary_version: u32,
    /// Original VM lineage retained for disk redirects and diagnostics.
    /// When a VM is restored from cache/snapshot, its vmstate still references
    /// the original VM's paths. When snapshotting such a VM, we preserve this
    /// original_vm_id. `source_vsock_socket_path` below is authoritative for
    /// vsock redirection; an ID cannot represent a custom `--vsock-dir`.
    #[serde(default)]
    pub original_vsock_vm_id: Option<String>,
    /// Exact host-side vsock base path embedded in the VMM state.
    ///
    /// This is not necessarily `vm_runtime_dir(original_vsock_vm_id)/vsock.sock`:
    /// a cold source may use `--vsock-dir`, and a snapshot of a restored clone
    /// still embeds its ancestor's path while listeners are clone-local.
    pub source_vsock_socket_path: PathBuf,
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
    pub snapshot_type: SnapshotType,
    /// Full (memory+disk) vs DiskOnly (no memory image — clones cold-boot).
    pub kind: SnapshotKind,
    pub metadata: SnapshotMetadata,
}

/// Identity of one installed snapshot generation.
///
/// The UUID makes every newly-created generation distinct. The config digest
/// also detects an in-place metadata change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotGeneration {
    generation_id: uuid::Uuid,
    config_digest: [u8; 32],
}

impl SnapshotGeneration {
    fn from_config(config: &SnapshotConfig, config_json: &[u8]) -> Self {
        Self {
            generation_id: config.generation_id,
            config_digest: Sha256::digest(config_json).into(),
        }
    }
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
    /// NFS shares from the baseline VM. The restore path re-exports these for
    /// the new VM (the baseline's /etc/exports.d entry dies with the baseline)
    /// and fc-agent remounts them in the guest after the transport reset.
    #[serde(default)]
    pub nfs_shares: Vec<crate::state::types::NfsShare>,
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
    /// Kernel profile the source VM booted with (None = "default"). Cold-boot
    /// clones and reboot plans resolve the same profile so a btrfs/nested-profile
    /// disk boots with a kernel that can actually mount it.
    #[serde(default)]
    pub kernel_profile: Option<String>,
    /// Image delivery mode of the source ("overlay" | "btrfs" | "archive");
    /// None for registry-pulled images.
    #[serde(default)]
    pub image_mode: Option<String>,
    /// Host path of the read-only image device the source had attached (overlay
    /// additionalImageStore or docker archive). Content-addressed cache file —
    /// cold-boot clones re-attach it so the captured container's image layers
    /// stay reachable.
    #[serde(default)]
    pub image_disk_path: Option<PathBuf>,
    /// Which VMM backend created this snapshot. Restore must use the same backend
    /// (the memory image format is VMM-specific). Defaults to Firecracker for snapshots
    /// written before this field existed (#632 P2).
    #[serde(default)]
    pub hypervisor: crate::hypervisor::Backend,
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
        validate_snapshot_name(&config.name)?;
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
        self.load_snapshot_with_generation(name)
            .await
            .map(|(config, _)| config)
    }

    /// Load a snapshot configuration together with the identity of the exact
    /// generation whose config was read.
    pub async fn load_snapshot_with_generation(
        &self,
        name: &str,
    ) -> Result<(SnapshotConfig, SnapshotGeneration)> {
        validate_snapshot_name(name)?;
        let snapshot_dir = self.snapshots_dir.join(name);
        let metadata_path = snapshot_dir.join("config.json");

        if !metadata_path.exists() {
            anyhow::bail!("snapshot '{}' not found", name);
        }

        let metadata_json = fs::read(&metadata_path)
            .await
            .context("reading snapshot metadata")?;

        let config = parse_snapshot_config(name, &metadata_json)?;
        let generation = SnapshotGeneration::from_config(&config, &metadata_json);

        Ok((config, generation))
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
        validate_snapshot_name(name)?;
        let snapshot_dir = self.snapshots_dir.join(name);
        // Serialize deletion with creators and restores.  Removing the directory
        // without this lock allowed a tag to be recreated while a reader was
        // loading it, producing a disk/memory generation split.  Keep the lock
        // inode after deletion: unlinking it while held would let a new creator
        // lock a different inode and enter the critical section concurrently.
        let _generation_lock =
            crate::commands::common::acquire_snapshot_dir_lock(&snapshot_dir, true)
                .await
                .with_context(|| format!("locking snapshot '{}' for deletion", name))?;

        if snapshot_dir.exists() {
            fs::remove_dir_all(&snapshot_dir)
                .await
                .context("removing snapshot directory")?;

            info!(snapshot = name, "snapshot deleted");
        }

        Ok(())
    }

    /// Delete `name` only if it is still the generation a failed restore used.
    ///
    /// The comparison happens while holding the generation lock exclusively.
    /// This closes the release-shared/acquire-exclusive gap in cache
    /// invalidation: if another creator installs a replacement in that gap, the
    /// replacement is retained rather than being mistaken for the failed one.
    pub async fn delete_snapshot_if_generation(
        &self,
        name: &str,
        expected: &SnapshotGeneration,
    ) -> Result<bool> {
        validate_snapshot_name(name)?;
        let snapshot_dir = self.snapshots_dir.join(name);
        let _generation_lock =
            crate::commands::common::acquire_snapshot_dir_lock(&snapshot_dir, true)
                .await
                .with_context(|| format!("locking snapshot '{}' for invalidation", name))?;

        if !snapshot_dir.exists() {
            return Ok(false);
        }

        let (_, current) = self
            .load_snapshot_with_generation(name)
            .await
            .with_context(|| format!("revalidating snapshot '{}' before invalidation", name))?;
        if &current != expected {
            info!(
                snapshot = name,
                "snapshot generation changed before invalidation; retaining replacement"
            );
            return Ok(false);
        }

        fs::remove_dir_all(&snapshot_dir)
            .await
            .context("removing invalid snapshot generation")?;
        info!(snapshot = name, "invalid snapshot generation deleted");
        Ok(true)
    }
}

fn parse_snapshot_config(name: &str, metadata_json: &[u8]) -> Result<SnapshotConfig> {
    let value: serde_json::Value =
        serde_json::from_slice(metadata_json).context("parsing snapshot metadata JSON")?;
    let object = value.as_object().ok_or_else(|| {
        anyhow::anyhow!(
            "snapshot '{name}' has unsupported metadata schema: config.json must contain an object; delete and recreate the snapshot"
        )
    })?;
    let missing_or_null: Vec<&str> = [
        "generation_id",
        "network_boundary_version",
        "source_vsock_socket_path",
        "snapshot_type",
        "kind",
    ]
    .into_iter()
    .filter(|field| object.get(*field).map_or(true, serde_json::Value::is_null))
    .collect();
    if !missing_or_null.is_empty() {
        anyhow::bail!(
            "snapshot '{name}' has unsupported metadata schema (required fields missing or null: {}); delete and recreate the snapshot",
            missing_or_null.join(", ")
        );
    }
    let config: SnapshotConfig = serde_json::from_value(value).with_context(|| {
        format!(
            "snapshot '{name}' metadata is incompatible with this fcvm build; delete and recreate the snapshot"
        )
    })?;
    let source_parent = config.source_vsock_socket_path.parent();
    if !config.source_vsock_socket_path.is_absolute()
        || config.source_vsock_socket_path.file_name() != Some(std::ffi::OsStr::new("vsock.sock"))
        || source_parent.is_none()
        || source_parent == Some(Path::new("/"))
    {
        anyhow::bail!(
            "snapshot '{name}' has invalid source_vsock_socket_path {}; expected an absolute path named vsock.sock under a dedicated directory; delete and recreate the snapshot",
            config.source_vsock_socket_path.display()
        );
    }
    if config.kind == SnapshotKind::Full
        && config.network_boundary_version != SNAPSHOT_NETWORK_BOUNDARY_VERSION
    {
        anyhow::bail!(
            "snapshot '{name}' uses unsupported network generation boundary version {} \
             (expected {}); delete and recreate the snapshot",
            config.network_boundary_version,
            SNAPSHOT_NETWORK_BOUNDARY_VERSION
        );
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_names_are_single_safe_path_components() {
        for name in ["snapshot-1", "cache_ab.cd", "A"] {
            validate_snapshot_name(name).unwrap();
        }
        for name in ["", ".", "..", "../outside", "a/b", "a\\b", "bad name"] {
            assert!(
                validate_snapshot_name(name).is_err(),
                "unsafe snapshot name was accepted: {name:?}"
            );
        }
    }

    #[tokio::test]
    async fn traversal_delete_cannot_escape_the_snapshot_root() {
        let temp_dir = tempfile::tempdir().unwrap();
        let snapshots = temp_dir.path().join("snapshots");
        let outside = temp_dir.path().join("outside");
        tokio::fs::create_dir_all(&outside).await.unwrap();
        tokio::fs::write(outside.join("marker"), b"keep")
            .await
            .unwrap();
        let manager = SnapshotManager::new(snapshots);
        assert!(manager.delete_snapshot("../outside").await.is_err());
        assert!(outside.join("marker").exists());
        assert!(manager.load_snapshot("../outside").await.is_err());
    }

    #[tokio::test]
    async fn old_snapshot_schema_is_rejected_with_recreation_instructions() {
        let temp_dir = tempfile::tempdir().unwrap();
        let snapshots = temp_dir.path().join("snapshots");
        let manager = SnapshotManager::new(snapshots.clone());
        for (name, config, rejected_fields) in [
            (
                "missing-all",
                serde_json::json!({"name": "missing-all", "vm_id": "vm-old"}),
                "generation_id, network_boundary_version, source_vsock_socket_path, snapshot_type, kind",
            ),
            (
                "missing-generation",
                serde_json::json!({
                    "name": "missing-generation", "vm_id": "vm-old",
                    "network_boundary_version": SNAPSHOT_NETWORK_BOUNDARY_VERSION,
                    "source_vsock_socket_path": "/source/vsock.sock",
                    "snapshot_type": "User", "kind": "Full"
                }),
                "generation_id",
            ),
            (
                "missing-type",
                serde_json::json!({
                    "name": "missing-type", "vm_id": "vm-old",
                    "generation_id": uuid::Uuid::nil(),
                    "network_boundary_version": SNAPSHOT_NETWORK_BOUNDARY_VERSION,
                    "source_vsock_socket_path": "/source/vsock.sock",
                    "kind": "Full"
                }),
                "snapshot_type",
            ),
            (
                "missing-kind",
                serde_json::json!({
                    "name": "missing-kind", "vm_id": "vm-old",
                    "generation_id": uuid::Uuid::nil(),
                    "network_boundary_version": SNAPSHOT_NETWORK_BOUNDARY_VERSION,
                    "source_vsock_socket_path": "/source/vsock.sock",
                    "snapshot_type": "User"
                }),
                "kind",
            ),
            (
                "null-generation",
                serde_json::json!({
                    "name": "null-generation", "vm_id": "vm-old",
                    "generation_id": null,
                    "network_boundary_version": SNAPSHOT_NETWORK_BOUNDARY_VERSION,
                    "source_vsock_socket_path": "/source/vsock.sock",
                    "snapshot_type": "User", "kind": "Full"
                }),
                "generation_id",
            ),
            (
                "missing-network-boundary",
                serde_json::json!({
                    "name": "missing-network-boundary", "vm_id": "vm-old",
                    "generation_id": uuid::Uuid::nil(),
                    "source_vsock_socket_path": "/source/vsock.sock",
                    "snapshot_type": "User", "kind": "Full"
                }),
                "network_boundary_version",
            ),
            (
                "missing-source-vsock",
                serde_json::json!({
                    "name": "missing-source-vsock", "vm_id": "vm-old",
                    "generation_id": uuid::Uuid::nil(),
                    "network_boundary_version": SNAPSHOT_NETWORK_BOUNDARY_VERSION,
                    "snapshot_type": "User", "kind": "Full"
                }),
                "source_vsock_socket_path",
            ),
        ] {
            let snapshot_dir = snapshots.join(name);
            tokio::fs::create_dir_all(&snapshot_dir).await.unwrap();
            tokio::fs::write(
                snapshot_dir.join("config.json"),
                serde_json::to_vec(&config).unwrap(),
            )
            .await
            .unwrap();

            let error = manager
                .load_snapshot(name)
                .await
                .expect_err("old schemas must fail closed instead of inventing defaults");
            let message = format!("{error:#}");
            assert!(message.contains("unsupported metadata schema"), "{message}");
            assert!(message.contains(rejected_fields), "{message}");
            assert!(message.contains("delete and recreate"), "{message}");
        }
    }

    #[test]
    fn test_snapshot_config_json_roundtrip() {
        let config = SnapshotConfig {
            name: "test-snapshot".to_string(),
            vm_id: "abc123".to_string(),
            generation_id: uuid::Uuid::new_v4(),
            network_boundary_version: SNAPSHOT_NETWORK_BOUNDARY_VERSION,
            original_vsock_vm_id: None,
            source_vsock_socket_path: PathBuf::from("/source/vsock.sock"),
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
                nfs_shares: vec![],
                username: None,
                user: None,
                port_mappings: vec![],
                forward_localhost: vec![],
                network_mode: Default::default(),
                ipv6_prefix: None,
                tty: false,
                interactive: false,
                kernel_profile: None,
                image_mode: None,
                image_disk_path: None,
                hypervisor: Default::default(),
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

        let mut incompatible = config;
        incompatible.network_boundary_version = SNAPSHOT_NETWORK_BOUNDARY_VERSION + 1;
        let bytes = serde_json::to_vec(&incompatible).unwrap();
        let error = parse_snapshot_config("test-snapshot", &bytes)
            .expect_err("an unknown network boundary must be rejected before restore");
        let message = format!("{error:#}");
        assert!(
            message.contains("unsupported network generation boundary"),
            "{message}"
        );
        assert!(message.contains("delete and recreate"), "{message}");
    }

    #[test]
    fn test_snapshot_config_json_parsing() {
        // Test parsing a JSON config like what would be saved on disk
        let json = r#"{
            "name": "nginx-snap",
            "vm_id": "def456",
            "generation_id": "fc9642dc-babd-4876-8c7f-48bccd9554e8",
            "network_boundary_version": 1,
            "source_vsock_socket_path": "/source/vsock.sock",
            "memory_path": "/mnt/fcvm-btrfs/snapshots/nginx-snap/memory.bin",
            "vmstate_path": "/mnt/fcvm-btrfs/snapshots/nginx-snap/vmstate.bin",
            "disk_path": "/mnt/fcvm-btrfs/snapshots/nginx-snap/disk.raw",
            "created_at": "2024-01-15T10:30:00Z",
            "snapshot_type": "User",
            "kind": "Full",
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
            generation_id: uuid::Uuid::new_v4(),
            network_boundary_version: SNAPSHOT_NETWORK_BOUNDARY_VERSION,
            original_vsock_vm_id: None,
            source_vsock_socket_path: PathBuf::from("/source/vsock.sock"),
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
                nfs_shares: vec![],
                username: None,
                user: None,
                port_mappings: vec![],
                forward_localhost: vec![],
                network_mode: Default::default(),
                ipv6_prefix: None,
                tty: false,
                interactive: false,
                kernel_profile: None,
                image_mode: None,
                image_disk_path: None,
                hypervisor: Default::default(),
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
                generation_id: uuid::Uuid::new_v4(),
                network_boundary_version: SNAPSHOT_NETWORK_BOUNDARY_VERSION,
                original_vsock_vm_id: None,
                source_vsock_socket_path: PathBuf::from("/source/vsock.sock"),
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
                    nfs_shares: vec![],
                    username: None,
                    user: None,
                    port_mappings: vec![],
                    forward_localhost: vec![],
                    network_mode: Default::default(),
                    ipv6_prefix: None,
                    tty: false,
                    interactive: false,
                    kernel_profile: None,
                    image_mode: None,
                    image_disk_path: None,
                    hypervisor: Default::default(),
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
            generation_id: uuid::Uuid::new_v4(),
            network_boundary_version: SNAPSHOT_NETWORK_BOUNDARY_VERSION,
            original_vsock_vm_id: None,
            source_vsock_socket_path: PathBuf::from("/source/vsock.sock"),
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
                nfs_shares: vec![],
                username: None,
                user: None,
                port_mappings: vec![],
                forward_localhost: vec![],
                network_mode: Default::default(),
                ipv6_prefix: None,
                tty: false,
                interactive: false,
                kernel_profile: None,
                image_mode: None,
                image_disk_path: None,
                hypervisor: Default::default(),
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
    async fn snapshot_delete_waits_for_generation_readers_and_keeps_lock_inode() {
        let temp_dir = tempfile::tempdir().unwrap();
        let snapshot_dir = temp_dir.path().join("leased");
        tokio::fs::create_dir_all(&snapshot_dir).await.unwrap();
        tokio::fs::write(snapshot_dir.join("config.json"), b"generation")
            .await
            .unwrap();
        let shared = crate::commands::common::acquire_snapshot_dir_lock(&snapshot_dir, false)
            .await
            .unwrap();

        let manager = SnapshotManager::new(temp_dir.path().to_path_buf());
        let mut deletion = tokio::spawn(async move { manager.delete_snapshot("leased").await });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut deletion)
                .await
                .is_err(),
            "exclusive delete entered while a shared generation lease was live"
        );

        drop(shared);
        tokio::time::timeout(std::time::Duration::from_secs(2), deletion)
            .await
            .expect("delete did not acquire the released generation lock")
            .expect("delete task panicked")
            .expect("delete failed");
        assert!(!snapshot_dir.exists());
        assert!(
            temp_dir.path().join("leased.lock").exists(),
            "deleting the held lock pathname would let a new generation bypass old readers"
        );
    }

    #[tokio::test]
    async fn conditional_invalidation_preserves_a_replacement_generation() {
        let temp_dir = tempfile::tempdir().unwrap();
        let manager = SnapshotManager::new(temp_dir.path().to_path_buf());
        let config = |vm_id: &str, generation_id: uuid::Uuid| -> SnapshotConfig {
            serde_json::from_value(serde_json::json!({
                "name": "cached",
                "vm_id": vm_id,
                "generation_id": generation_id,
                "network_boundary_version": SNAPSHOT_NETWORK_BOUNDARY_VERSION,
                "source_vsock_socket_path": "/source/vsock.sock",
                "memory_path": "/memory.bin",
                "vmstate_path": "/vmstate.bin",
                "disk_path": "/disk.raw",
                "created_at": "2026-08-09T00:00:00Z",
                "snapshot_type": "System",
                "kind": "Full",
                "metadata": {
                    "image": "alpine",
                    "vcpu": 1,
                    "memory_mib": 256,
                    "network_config": {
                        "tap_device": "tap",
                        "guest_mac": "00:00:00:00:00:00"
                    }
                }
            }))
            .unwrap()
        };

        let failed_id = uuid::Uuid::new_v4();
        manager
            .save_snapshot(config("failed-vm", failed_id))
            .await
            .unwrap();
        let (_, failed_generation) = manager
            .load_snapshot_with_generation("cached")
            .await
            .unwrap();

        // Model the exact invalidation gap: the failed restore has released its
        // shared lease, and a creator installs G2 before invalidation obtains the
        // exclusive lease.  The expected G1 identity must protect G2.
        let replacement_id = uuid::Uuid::new_v4();
        manager
            .save_snapshot(config("replacement-vm", replacement_id))
            .await
            .unwrap();
        assert!(
            !manager
                .delete_snapshot_if_generation("cached", &failed_generation)
                .await
                .unwrap(),
            "conditional invalidation deleted a replacement generation"
        );

        let (replacement, replacement_generation) = manager
            .load_snapshot_with_generation("cached")
            .await
            .unwrap();
        assert_eq!(replacement.vm_id, "replacement-vm");
        assert_eq!(replacement.generation_id, replacement_id);

        assert!(
            manager
                .delete_snapshot_if_generation("cached", &replacement_generation)
                .await
                .unwrap(),
            "the current generation did not invalidate"
        );
        assert!(manager.load_snapshot("cached").await.is_err());
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
    fn test_snapshot_type_display() {
        assert_eq!(format!("{}", SnapshotType::User), "user");
        assert_eq!(format!("{}", SnapshotType::System), "system");
    }

    #[test]
    fn test_snapshot_type_json_roundtrip() {
        // Test User type serializes and deserializes correctly
        let user_config = SnapshotConfig {
            name: "user-snapshot".to_string(),
            vm_id: "user123".to_string(),
            generation_id: uuid::Uuid::new_v4(),
            network_boundary_version: SNAPSHOT_NETWORK_BOUNDARY_VERSION,
            original_vsock_vm_id: None,
            source_vsock_socket_path: PathBuf::from("/source/vsock.sock"),
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
                nfs_shares: vec![],
                username: None,
                user: None,
                port_mappings: vec![],
                forward_localhost: vec![],
                network_mode: Default::default(),
                ipv6_prefix: None,
                tty: false,
                interactive: false,
                kernel_profile: None,
                image_mode: None,
                image_disk_path: None,
                hypervisor: Default::default(),
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
    fn test_snapshot_config_kind_disk_only_roundtrip() {
        // A SnapshotConfig tagged DiskOnly must preserve kind through JSON — the
        // run dispatch in later PRs depends on this field surviving.
        let json = r#"{
            "name": "do",
            "vm_id": "x",
            "generation_id": "5d2cb020-b470-498d-a252-f6b6ca75b712",
            "network_boundary_version": 1,
            "source_vsock_socket_path": "/source/vsock.sock",
            "memory_path": "/m",
            "vmstate_path": "/v",
            "disk_path": "/d",
            "created_at": "2024-01-15T10:30:00Z",
            "snapshot_type": "User",
            "kind": "DiskOnly",
            "metadata": {
                "image": "alpine",
                "vcpu": 1,
                "memory_mib": 256,
                "network_config": { "tap_device": "t", "guest_mac": "AA:BB:CC:DD:EE:FF" }
            }
        }"#;
        let config: SnapshotConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.kind, SnapshotKind::DiskOnly);
        // ...and survives a re-serialize round-trip.
        let reparsed: SnapshotConfig =
            serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
        assert_eq!(reparsed.kind, SnapshotKind::DiskOnly);
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
            nfs_shares: vec![],
            username: None,
            user: None,
            port_mappings: vec![],
            forward_localhost: vec![],
            network_mode: Default::default(),
            ipv6_prefix: Some("2600:1f1c:494:201".to_string()),
            tty: false,
            interactive: false,
            kernel_profile: None,
            image_mode: None,
            image_disk_path: None,
            hypervisor: Default::default(),
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
