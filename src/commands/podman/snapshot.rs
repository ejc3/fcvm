use std::path::Path;

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::cli::RunArgs;
use crate::firecracker::VmManager;
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
    pub vm_manager: &'a VmManager,
    pub snapshot_key: &'a str,
    pub vm_state: &'a VmState,
    pub disk_path: &'a Path,
    pub volume_configs: &'a [VolumeConfig],
    pub parent_snapshot_key: Option<&'a str>,
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
pub async fn create_podman_snapshot(snap: &CreateSnapshotParams<'_>) -> Result<()> {
    let CreateSnapshotParams {
        vm_manager,
        snapshot_key,
        vm_state,
        disk_path,
        volume_configs,
        parent_snapshot_key,
    } = snap;
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

    let network_mode = match args.network {
        crate::cli::args::NetworkMode::Bridged => FcNetworkMode::Bridged,
        crate::cli::args::NetworkMode::Rootless => FcNetworkMode::Rootless,
        crate::cli::args::NetworkMode::Routed => FcNetworkMode::Routed,
    };

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
        forward_localhost: args.forward_localhost.clone(),
        image_mode,
        non_blocking_output: args.non_blocking_output,
        rootfs_type: super::resolve_rootfs_type(args),
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
