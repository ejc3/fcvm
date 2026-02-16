use std::path::Path;

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::cli::RunArgs;
use crate::firecracker::VmManager;
use crate::network::NetworkConfig;
use crate::paths;
use crate::storage::{
    SnapshotConfig, SnapshotManager, SnapshotMetadata, SnapshotType, SnapshotVolumeConfig,
};
use crate::volume::VolumeConfig;

use super::types::{SnapshotCreationParams, SnapshotOutcome};

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

/// Create a podman snapshot from a running VM.
///
/// This pauses the VM, creates a Firecracker snapshot, copies the disk,
/// saves metadata using SnapshotManager, and resumes the VM.
///
/// The snapshot is stored in snapshot_dir with snapshot_key as the name,
/// making it accessible via `fcvm snapshot run --snapshot <snapshot_key>`.
///
/// If `parent_snapshot_key` is provided, the parent's memory.bin will be copied
/// (via reflink) as a base, enabling diff snapshots for new directories.
#[allow(clippy::too_many_arguments)]
pub async fn create_podman_snapshot(
    vm_manager: &VmManager,
    snapshot_key: &str,
    vm_id: &str,
    params: &SnapshotCreationParams,
    disk_path: &Path,
    network_config: &NetworkConfig,
    volume_configs: &[VolumeConfig],
    parent_snapshot_key: Option<&str>,
) -> Result<()> {
    // Snapshots stored in snapshot_dir with snapshot_key as name
    let snapshot_dir = paths::snapshot_dir().join(snapshot_key);

    // Lock to prevent concurrent snapshot creation
    let lock_path = snapshot_dir.with_extension("lock");
    tokio::fs::create_dir_all(paths::snapshot_dir())
        .await
        .context("creating snapshot directory")?;

    let lock_file = std::fs::File::create(&lock_path).context("creating snapshot lock file")?;

    // Use try_lock in a loop so we yield to the async runtime and can be interrupted
    use fs2::FileExt;
    loop {
        match lock_file.try_lock_exclusive() {
            Ok(()) => break,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Lock is held by another process, yield and retry
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(e) => return Err(anyhow::anyhow!("acquiring snapshot lock: {}", e)),
        }
    }

    // Double-check after lock (another process might have created it)
    if snapshot_dir.join("config.json").exists() {
        info!(snapshot_key = %snapshot_key, "Snapshot already exists (created by another process)");
        return Ok(());
    }

    info!(snapshot_key = %snapshot_key, "Creating podman snapshot");

    // Get Firecracker client
    let client = vm_manager.client().context("VM not started")?;

    // Convert VolumeConfig to SnapshotVolumeConfig for metadata
    let snapshot_volumes: Vec<SnapshotVolumeConfig> = volume_configs
        .iter()
        .map(|v| SnapshotVolumeConfig {
            host_path: v.host_path.clone(),
            guest_path: v.guest_path.to_string_lossy().to_string(),
            read_only: v.read_only,
            vsock_port: v.port,
            portable: v.portable,
        })
        .collect();

    // Build final paths (create_snapshot_core handles temp dir)
    let final_memory_path = snapshot_dir.join("memory.bin");
    let final_vmstate_path = snapshot_dir.join("vmstate.bin");
    let final_disk_path = snapshot_dir.join("disk.raw");

    // Build snapshot config with final paths
    let snapshot_config = SnapshotConfig {
        name: snapshot_key.to_string(),
        vm_id: vm_id.to_string(),
        original_vsock_vm_id: None, // Fresh VM, no redirect needed
        memory_path: final_memory_path,
        vmstate_path: final_vmstate_path,
        disk_path: final_disk_path,
        created_at: chrono::Utc::now(),
        snapshot_type: SnapshotType::System, // Auto-generated cache snapshot
        metadata: SnapshotMetadata {
            image: params.image.clone(),
            vcpu: params.vcpu,
            memory_mib: params.memory_mib,
            network_config: network_config.clone(),
            volumes: snapshot_volumes,
            health_check_url: params.health_check_url.clone(),
            hugepages: params.hugepages,
            extra_disks: vec![],
        },
    };

    // Use shared core function for snapshot creation
    // If parent key provided, resolve to directory path
    let parent_dir = parent_snapshot_key.map(|key| paths::snapshot_dir().join(key));
    crate::commands::common::create_snapshot_core(
        client,
        snapshot_config,
        disk_path,
        parent_dir.as_deref(),
    )
    .await
}

/// Create a snapshot with signal interruption support.
///
/// This wraps `create_podman_snapshot` in a cancellation check, allowing
/// graceful shutdown during snapshot creation.
///
/// Returns `SnapshotOutcome::Interrupted` if a signal is received - caller
/// should break their event loop and proceed to cleanup.
#[allow(clippy::too_many_arguments)]
pub async fn create_snapshot_interruptible(
    vm_manager: &VmManager,
    snapshot_key: &str,
    vm_id: &str,
    params: &SnapshotCreationParams,
    disk_path: &Path,
    network_config: &NetworkConfig,
    volume_configs: &[VolumeConfig],
    parent_snapshot_key: Option<&str>,
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
    match create_podman_snapshot(
        vm_manager,
        snapshot_key,
        vm_id,
        params,
        disk_path,
        network_config,
        volume_configs,
        parent_snapshot_key,
    )
    .await
    {
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
pub(super) fn build_firecracker_config(
    args: &RunArgs,
    image_identifier: &str,
    kernel_path: &Path,
    rootfs_path: &Path,
    initrd_path: &Path,
    cmd_args: Option<Vec<String>>,
    image_mode: crate::firecracker::ImageMode,
) -> crate::firecracker::FirecrackerConfig {
    use crate::firecracker::{FcNetworkMode, FirecrackerConfig};

    let network_mode = match args.network {
        crate::cli::args::NetworkMode::Bridged => FcNetworkMode::Bridged,
        crate::cli::args::NetworkMode::Rootless => FcNetworkMode::Rootless,
    };

    // Collect extra disk specifications for cache key.
    // These are block devices that must match between cache create and restore.
    let mut extra_disks: Vec<String> = Vec::new();
    extra_disks.extend(args.disk.iter().cloned());
    extra_disks.extend(args.disk_dir.iter().cloned());
    extra_disks.extend(args.nfs.iter().cloned());

    // Collect env vars for cache key (affects container behavior)
    let env_vars: Vec<String> = args.env.to_vec();

    // Collect volume mounts for cache key (affects MMDS plan)
    let volume_mounts: Vec<String> = args.map.to_vec();

    FirecrackerConfig::new(
        kernel_path.to_path_buf(),
        initrd_path.to_path_buf(),
        rootfs_path.to_path_buf(),
        image_identifier.to_string(),
        cmd_args,
        args.cpu,
        args.mem,
        network_mode,
        crate::paths::data_dir(),
        extra_disks,
        env_vars,
        volume_mounts,
        args.privileged,
        args.tty,
        args.interactive,
        args.rootfs_size.clone(),
        args.health_check.clone(),
        args.hugepages,
        args.user.clone(),
        args.forward_localhost.clone(),
        image_mode,
    )
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
