use std::path::Path;

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::cli::RunArgs;
use crate::hypervisor::firecracker::FirecrackerBackend;
use crate::paths;
use crate::state::VmState;
use crate::storage::{SnapshotConfig, SnapshotManager, SnapshotType};
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

/// Parameters for cache snapshot creation.
///
/// Uses VmState as the single source of truth for snapshot metadata,
/// ensuring fields like `original_vsock_vm_id` are always preserved correctly.
pub struct CreateSnapshotParams<'a> {
    pub vm_manager: &'a FirecrackerBackend,
    pub snapshot_key: &'a str,
    pub vm_state: &'a VmState,
    pub disk_path: &'a Path,
    pub volume_configs: &'a [VolumeConfig],
    /// RemapFs references for portable volumes — used to serialize inode tables at snapshot time.
    pub remap_refs: &'a [Option<std::sync::Arc<fuse_pipe::RemapFs<fuse_pipe::PassthroughFs>>>],
}

/// Create a podman snapshot from a running VM.
///
/// This pauses the VM, creates a Firecracker snapshot, copies the disk,
/// saves metadata using SnapshotManager, and resumes the VM.
///
/// The snapshot is stored in snapshot_dir with snapshot_key as the name,
/// making it accessible via `fcvm snapshot run --snapshot <snapshot_key>`.
///
/// The diff parent is resolved from the VM state file while the per-VM snapshot
/// lock is held, so a concurrent `fcvm snapshot create` (which resets the KVM
/// dirty bitmap and updates `snapshot_name`) can never leave us merging a diff
/// onto a stale base.
pub async fn create_podman_snapshot(snap: &CreateSnapshotParams<'_>) -> Result<()> {
    let CreateSnapshotParams {
        vm_manager,
        snapshot_key,
        vm_state,
        disk_path,
        volume_configs,
        remap_refs,
    } = snap;
    // Snapshots stored in snapshot_dir with snapshot_key as name
    let snapshot_dir = paths::snapshot_dir().join(snapshot_key);

    // Per-snapshot lock (exclusive): prevents concurrent creation of the same key
    // and blocks restores of this snapshot while it is being (re)created.
    tokio::fs::create_dir_all(paths::snapshot_dir())
        .await
        .context("creating snapshot directory")?;
    let _snapshot_lock =
        crate::commands::common::acquire_snapshot_dir_lock(&snapshot_dir, true).await?;

    // Double-check after lock (another process might have created it)
    if snapshot_dir.join("config.json").exists() {
        info!(snapshot_key = %snapshot_key, "Snapshot already exists (created by another process)");
        return Ok(());
    }

    info!(snapshot_key = %snapshot_key, "Creating podman snapshot");

    // Per-VM lock: serialize with external `fcvm snapshot create` calls.
    let _vm_lock = crate::commands::common::acquire_vm_snapshot_lock(disk_path).await?;

    // Resolve the diff parent UNDER the per-VM lock by re-reading the state file.
    // The lock contract requires this: another process may have created a snapshot
    // (resetting the KVM dirty bitmap) and updated snapshot_name since the caller's
    // in-memory copy of the state was taken. Using that stale parent would merge a
    // diff covering only post-reset writes onto an older base, silently dropping
    // every page dirtied in between.
    let state_manager = crate::state::StateManager::new(paths::state_dir());
    let parent_snapshot_key = match state_manager.load_state(&vm_state.vm_id).await {
        Ok(state) => state.config.snapshot_name,
        Err(e) => {
            tracing::warn!(
                vm_id = %vm_state.vm_id,
                error = %e,
                "could not re-read VM state under snapshot lock; creating full snapshot"
            );
            None
        }
    };

    // Get Firecracker client
    let client = vm_manager.client().context("VM not started")?;

    // Build snapshot config from VmState (single source of truth)
    let snapshot_volumes = crate::commands::common::volume_configs_to_snapshot(volume_configs);
    let extra_disks = crate::commands::common::extra_disks_to_snapshot(vm_state);
    let snapshot_config = crate::commands::common::build_snapshot_config(
        vm_state,
        snapshot_key,
        SnapshotType::System,
        &snapshot_dir,
        snapshot_volumes,
        extra_disks,
    );

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
        parent_dir.as_deref(),
        Some(&extra_files),
    )
    .await?;

    Ok(())
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
) -> SnapshotOutcome {
    // CRITICAL: Do NOT use tokio::select! to interrupt the snapshot future.
    // create_snapshot_core pauses the VM, creates the snapshot, then resumes.
    // If we drop the future between pause and resume (via select!), the VM
    // stays paused forever. Instead, check cancellation before starting and
    // let the snapshot run to completion once started.
    if cancel.is_cancelled() {
        info!("snapshot creation skipped -- already cancelled");
        return SnapshotOutcome::Interrupted;
    }

    // Run snapshot to completion. create_snapshot_core always resumes the VM
    // before returning, even on error, so this is safe.
    match create_podman_snapshot(snap).await {
        Ok(()) => {
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
