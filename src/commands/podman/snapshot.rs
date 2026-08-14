use std::path::Path;

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::cli::RunArgs;
use crate::hypervisor::firecracker::FirecrackerBackend;
use crate::paths;
use crate::state::VmState;
use crate::storage::{validate_snapshot_name, SnapshotConfig, SnapshotManager, SnapshotType};
use crate::volume::VolumeConfig;

use super::types::SnapshotOutcome;

/// Check if a podman snapshot exists.
/// Uses SnapshotManager to check for the snapshot with snapshot_key as name.
pub async fn check_podman_snapshot(snapshot_key: &str) -> Option<SnapshotConfig> {
    let snapshot_manager = SnapshotManager::new(paths::snapshot_dir());
    snapshot_manager.load_snapshot(snapshot_key).await.ok()
}

/// Generate the startup snapshot key from a base snapshot key.
///
/// Startup snapshots capture VM state after the container reports healthy,
/// enabling subsequent runs to skip application initialization time.
pub fn startup_snapshot_key(base_key: &str) -> String {
    format!("{}-startup", base_key)
}

/// What to do when a generation is already installed at the target name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExistingGeneration {
    /// Keep it and report [`SnapshotInstall::Existing`]. The name is the content-addressed
    /// key, so an installed generation holds this same content and another process
    /// installing it first is a race worth losing.
    Reuse,
    /// Replace it. `podman prepare --force` asks for a rebuild, and `podman prepare --tag`
    /// installs under a caller-chosen name that can hold any content at all.
    Replace,
}

/// Parameters for cache snapshot creation.
///
/// Uses VmState as the single source of truth for snapshot metadata,
/// ensuring fields like `original_vsock_vm_id` are always preserved correctly.
pub struct CreateSnapshotParams<'a> {
    pub vm_manager: &'a FirecrackerBackend,
    /// Directory name the generation is installed under, and its `config.name`.
    pub snapshot_key: &'a str,
    /// Content-addressed key whose content this generation holds. Equals `snapshot_key`
    /// for the cache entries `podman run` installs; differs under `prepare --tag`.
    pub content_key: &'a str,
    /// `System` for a content-addressed cache entry `snapshots prune` may reclaim,
    /// `User` for a caller-named artifact it must keep.
    pub snapshot_type: SnapshotType,
    pub existing: ExistingGeneration,
    pub vm_state: &'a VmState,
    pub disk_path: &'a Path,
    pub volume_configs: &'a [VolumeConfig],
    /// RemapFs references for portable volumes — used to serialize inode tables at snapshot time.
    pub remap_refs: &'a [Option<std::sync::Arc<fuse_pipe::RemapFs<fuse_pipe::PassthroughFs>>>],
}

impl CreateSnapshotParams<'_> {
    /// The parameters `podman run` uses for its content-addressed cache entries: the name
    /// is the key, the entry is prunable cache, and another process winning the race is
    /// a reusable result.
    pub fn cache_entry<'a>(
        vm_manager: &'a FirecrackerBackend,
        snapshot_key: &'a str,
        vm_state: &'a VmState,
        disk_path: &'a Path,
        volume_configs: &'a [VolumeConfig],
        remap_refs: &'a [Option<std::sync::Arc<fuse_pipe::RemapFs<fuse_pipe::PassthroughFs>>>],
    ) -> CreateSnapshotParams<'a> {
        CreateSnapshotParams {
            vm_manager,
            snapshot_key,
            content_key: snapshot_key,
            snapshot_type: SnapshotType::System,
            existing: ExistingGeneration::Reuse,
            vm_state,
            disk_path,
            volume_configs,
            remap_refs,
        }
    }
}

/// Whether a generation already installed at the target name ends this create.
///
/// Read under the exclusive generation lock, so `installed` is the state no other
/// creator can change until this create finishes.
pub(crate) fn keeps_installed_generation(existing: ExistingGeneration, installed: bool) -> bool {
    installed && existing == ExistingGeneration::Reuse
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotInstall {
    Created,
    Existing,
}

/// Create a podman snapshot from a running VM.
///
/// This pauses the VM, creates a Firecracker snapshot, copies the disk, saves metadata,
/// and applies the requested source disposition.
///
/// The snapshot is stored in snapshot_dir with snapshot_key as the name,
/// making it accessible via `fcvm snapshot run --snapshot <snapshot_key>`.
///
/// The diff parent is resolved from the VM state file while the per-VM snapshot
/// lock is held, so a concurrent `fcvm snapshot create` (which resets the KVM
/// dirty bitmap and updates `snapshot_name`) can never leave us merging a diff
/// onto a stale base.
pub async fn create_podman_snapshot(
    snap: &CreateSnapshotParams<'_>,
    source_disposition: crate::commands::common::SnapshotSourceDisposition,
) -> Result<SnapshotInstall> {
    let CreateSnapshotParams {
        vm_manager,
        snapshot_key,
        content_key,
        snapshot_type,
        existing,
        vm_state,
        disk_path,
        volume_configs,
        remap_refs,
    } = snap;
    // Snapshots stored in snapshot_dir with snapshot_key as name
    let snapshot_dir = paths::snapshot_dir().join(snapshot_key);

    tokio::fs::create_dir_all(paths::snapshot_dir())
        .await
        .context("creating snapshot directory")?;
    let state_manager = crate::state::StateManager::new(paths::state_dir());
    let mut expected_parent_key = vm_state.config.snapshot_name.clone();
    let (_generation_locks, _vm_lock, parent_snapshot_key, vsock_socket_path) = loop {
        if let Some(parent) = expected_parent_key.as_deref() {
            validate_snapshot_name(parent).context("invalid parent snapshot name in VM state")?;
        }
        let expected_parent_dir = expected_parent_key
            .as_ref()
            .map(|key| paths::snapshot_dir().join(key));
        let generation_locks = crate::commands::common::acquire_snapshot_create_generation_locks(
            &snapshot_dir,
            expected_parent_dir.as_deref(),
        )
        .await?;

        // Another VM process may have finished this content-addressed snapshot while we
        // waited for its generation lock.
        if keeps_installed_generation(*existing, snapshot_dir.join("config.json").exists()) {
            info!(snapshot_key = %snapshot_key, "Snapshot already exists (created by another process)");
            return Ok(SnapshotInstall::Existing);
        }

        // Serialize dirty-bitmap resets, then ensure both the owning process identity and
        // lineage still match the observations used to choose generation locks.
        let vm_lock = crate::commands::common::acquire_vm_snapshot_lock(disk_path).await?;
        let fresh_state = state_manager
            .load_state(&vm_state.vm_id)
            .await
            .context("re-reading VM state under snapshot lock")?;
        crate::commands::common::validate_snapshot_vm_identity(vm_state, &fresh_state)?;
        if let Some(parent) = fresh_state.config.snapshot_name.as_deref() {
            validate_snapshot_name(parent).context("invalid parent snapshot name in VM state")?;
        }
        if fresh_state.config.snapshot_name != expected_parent_key {
            info!(
                vm_id = %vm_state.vm_id,
                expected_parent = ?expected_parent_key,
                current_parent = ?fresh_state.config.snapshot_name,
                "snapshot lineage advanced while acquiring locks; retrying"
            );
            expected_parent_key = fresh_state.config.snapshot_name;
            drop(vm_lock);
            drop(generation_locks);
            continue;
        }

        let vsock_socket_path =
            crate::commands::common::recorded_vsock_socket_path(&fresh_state)?.to_path_buf();
        break (
            generation_locks,
            vm_lock,
            expected_parent_key.clone(),
            vsock_socket_path,
        );
    };

    info!(snapshot_key = %snapshot_key, "Creating podman snapshot");

    // Get Firecracker client
    let client = vm_manager.client().context("VM not started")?;

    // Build snapshot config from VmState (single source of truth)
    let snapshot_volumes = crate::commands::common::volume_configs_to_snapshot(volume_configs);
    let extra_disks = crate::commands::common::extra_disks_to_snapshot(vm_state);
    let mut snapshot_config = crate::commands::common::build_snapshot_config(
        vm_state,
        snapshot_key,
        *snapshot_type,
        &snapshot_dir,
        snapshot_volumes,
        extra_disks,
    )?;
    // The only record of which content a caller-named generation holds.
    snapshot_config.content_key = Some(content_key.to_string());

    // Inode tables for portable volumes are written into the temp (.creating) directory
    // by create_snapshot_core BEFORE the atomic rename, so a finalized snapshot can never
    // exist without them. They are loaded by clone VolumeServers via restore_from_table()
    // to preserve inode numbering across snapshot/restore — eliminating the TTL glitch window.
    let extra_files = || -> Vec<(String, Vec<u8>)> {
        let mut files = Vec::new();
        for (idx, remap_ref) in remap_refs.iter().enumerate() {
            if let Some(remap) = remap_ref {
                let port = volume_configs.get(idx).map(|c| c.port).unwrap_or(0);
                let json = remap.serialize_table();
                tracing::info!(
                    port,
                    bytes = json.len(),
                    "serialized inode table for snapshot"
                );
                files.push((
                    format!("volume-{}-inode-table.json", port),
                    json.into_bytes(),
                ));
            }
        }
        files
    };

    // Use shared core function for snapshot creation
    // If parent key provided, resolve to directory path
    let parent_dir = parent_snapshot_key
        .as_deref()
        .map(|key| paths::snapshot_dir().join(key));
    crate::commands::common::create_snapshot_core(
        client,
        snapshot_config,
        disk_path,
        &vsock_socket_path,
        parent_dir.as_deref(),
        Some(&extra_files),
        source_disposition,
    )
    .await?;

    Ok(SnapshotInstall::Created)
}

/// Create a snapshot with signal interruption support.
///
/// This wraps `create_podman_snapshot` in a cancellation check, allowing
/// graceful shutdown during snapshot creation.
///
/// Returns `SnapshotOutcome::Interrupted` if a signal is received - caller
/// should break their event loop and proceed to cleanup.
pub async fn create_snapshot_interruptible(
    snap: &CreateSnapshotParams<'_>,
    cancel: &CancellationToken,
    source_disposition: crate::commands::common::SnapshotSourceDisposition,
) -> SnapshotOutcome {
    // CRITICAL: Do NOT interrupt the snapshot future after it starts. A normal source must
    // reach its resume step; a disposable source must finish atomic artifact installation.
    // Check cancellation before starting and let the snapshot reach its requested terminal
    // disposition once started.
    if cancel.is_cancelled() {
        info!("snapshot creation skipped -- already cancelled");
        return SnapshotOutcome::Interrupted;
    }

    // Run snapshot to completion so its source disposition is always honored.
    match create_podman_snapshot(snap, source_disposition).await {
        Ok(_) => {
            if cancel.is_cancelled() {
                // Snapshot succeeded but we're shutting down
                SnapshotOutcome::Interrupted
            } else {
                SnapshotOutcome::Created
            }
        }
        Err(e) => {
            if cancel.is_cancelled() {
                info!("snapshot creation failed during shutdown: {:#}", e);
                SnapshotOutcome::Interrupted
            } else {
                SnapshotOutcome::Failed(e)
            }
        }
    }
}

/// Build FirecrackerConfig from run args.
/// The config is the single source of truth for both cache key and VM launch.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_firecracker_config(
    args: &RunArgs,
    image_identifier: &str,
    kernel_path: &Path,
    rootfs_path: &Path,
    initrd_path: &Path,
    cmd_args: Option<Vec<String>>,
    image_mode: crate::firecracker::ImageMode,
    firecracker_bin: Option<&Path>,
    image_disk_identity: Option<String>,
) -> crate::firecracker::FirecrackerConfig {
    // image_identifier is the digest for localhost images (content-addressed cache key).
    // args.image is the original name (what the guest uses to find the image).
    // FirecrackerConfig stores both: container_image for cache key, container_image_name for MMDS.
    use crate::firecracker::{BootSource, Drive, FcNetworkMode, FirecrackerConfig, MachineConfig};

    let network_mode: FcNetworkMode = args.network.into();

    let port_mappings = crate::network::PortMapping::parse_all_lenient(&args.publish);

    // Collect extra disk specifications for cache key.
    // These are block devices that must match between cache create and restore.
    let mut extra_disks: Vec<String> = Vec::new();
    extra_disks.extend(args.disk.iter().cloned());
    extra_disks.extend(args.disk_dir.iter().cloned());
    extra_disks.extend(args.nfs.iter().cloned());

    FirecrackerConfig {
        boot_source: BootSource {
            kernel_image_path: kernel_path.to_path_buf(),
            initrd_path: initrd_path.to_path_buf(),
            ..Default::default()
        },
        machine_config: MachineConfig {
            vcpu_count: args.cpu,
            mem_size_mib: args.mem,
            huge_pages: if args.hugepages {
                Some("2M".to_string())
            } else {
                None
            },
        },
        drives: vec![Drive {
            drive_id: "rootfs".to_string(),
            path_on_host: rootfs_path.to_path_buf(),
            is_root_device: true,
            is_read_only: false,
        }],
        container_image: image_identifier.to_string(),
        // Set the original image name for MMDS (separate from cache key identifier)
        container_image_name: args.image.clone(),
        container_cmd: cmd_args,
        network_mode,
        data_dir: crate::paths::data_dir(),
        extra_disks,
        env_vars: args.env.to_vec(),
        volume_mounts: args.map.to_vec(),
        privileged: args.privileged,
        tty: args.tty,
        interactive: args.interactive,
        rootfs_size: args.rootfs_size.clone(),
        health_check_url: args.health_check.clone(),
        user: args.user.clone(),
        port_mappings,
        forward_localhost: args.forward_localhost.clone(),
        image_mode,
        non_blocking_output: args.non_blocking_output,
        rootfs_type: super::resolve_rootfs_type(args),
        ipv6_prefix: args.ipv6_prefix.clone(),
        portable_volumes: args.portable_volumes,
        firecracker_bin: firecracker_bin.map(|p| p.to_path_buf()),
        // Cache-key isolation for guest failpoints (see field docs): the spec is
        // forwarded to the guest by build_runtime_boot_args from the same env var.
        guest_failpoint: std::env::var("FCVM_GUEST_FAILPOINT").ok(),
        image_disk_identity,
    }
}

/// Extract Firecracker binary path and args from RuntimeConfig for snapshot restore.
pub(super) fn snapshot_run_firecracker_overrides(
    runtime_config: &crate::commands::common::RuntimeConfig,
) -> (Option<String>, Option<String>) {
    (
        runtime_config
            .firecracker_bin
            .as_ref()
            .map(|path| path.display().to_string()),
        runtime_config.firecracker_args.clone(),
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn test_snapshot_run_firecracker_overrides_preserve_runtime_config() {
        let runtime_config = crate::commands::common::RuntimeConfig {
            firecracker_bin: Some(PathBuf::from("/opt/firecracker-nested")),
            firecracker_args: Some("--enable-nv2".to_string()),
            boot_args: None,
            fuse_readers: None,
        };

        let (bin, args) = snapshot_run_firecracker_overrides(&runtime_config);
        assert_eq!(bin, Some("/opt/firecracker-nested".to_string()));
        assert_eq!(args, Some("--enable-nv2".to_string()));
    }
}
