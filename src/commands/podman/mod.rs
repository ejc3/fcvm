use anyhow::{bail, Context, Result};
use fs2::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;
use tracing::{info, warn};

mod image;
mod listeners;
pub use listeners::ContainerLogWriter;
pub(crate) use listeners::{CONTAINER_LOG, CONTAINER_LOG_ROTATED, OUTPUT_DRAIN_BOUND};
mod namespace;
mod snapshot;
mod types;
mod vm_config;

pub use types::{
    shared_cache_verdict, CacheRequest, CacheVerdict, LogLine, PreparedRestore, PreparedTarget,
    SharedCacheVerdict, SnapshotOutcome, VmContext, VmHandle,
};
// Re-exported for the snapshot restore path's up-front reboot plan (a rebooted VM
// relaunches in place via the same shared primitive, on every lifecycle path).
pub(crate) use types::{
    checked_volume_mappings, RebootSpec, VolumeMapping, IMAGE_STORE_MOUNT_POINT,
    IMAGE_STORE_RAW_MOUNT_POINT,
};

// Re-exported for the #598 regression test (export must pin immutable content by image
// ID even when the tag is rebuilt mid-export).
pub use image::export_image_archive;
// Re-exported for the #944 regression test (building a storage image must not detach
// containers that are running in the default store).
pub use image::build_storage_image;

pub(crate) use listeners::{
    run_output_listener, run_status_listener, spawn_bootplan_listener,
    spawn_restore_completion_listener, RestoreCompletionReceiver,
};

use snapshot::{build_firecracker_config, snapshot_run_firecracker_overrides};
pub use snapshot::{
    check_podman_snapshot, create_snapshot_interruptible, startup_snapshot_key, BalloonRequirement,
    CreateSnapshotParams, ExistingGeneration, SnapshotInstall, SnapshotVmm,
};
pub(crate) use vm_config::{
    build_launch_config, build_runtime_boot_args, cleanup_nfs_exports, configure_and_boot_vm,
    guest_dns, setup_nfs_exports, GuestBootInputs, GuestDns,
};
use vm_config::{run_vm_setup, VmSetupParams};

use crate::hypervisor::firecracker::FirecrackerBackend;

use crate::cli::{NetworkMode, PodmanArgs, PodmanCommands, RunArgs};
use crate::commands::common::{
    RuntimeConfig, VSOCK_OUTPUT_PORT, VSOCK_STATUS_PORT, VSOCK_TTY_PORT, VSOCK_VOLUME_PORT_BASE,
};
use crate::network::{BridgedNetwork, NetworkManager, PastaNetwork, PortMapping, RoutedNetwork};
use crate::paths;
use crate::state::{generate_vm_id, truncate_id, validate_vm_name, StateManager, VmState};
use crate::volume::{spawn_volume_servers, VolumeConfig};
use image::{get_image_cache_ref, validate_docker_archive};
use tokio_util::sync::CancellationToken;

/// Resolve the rootfs filesystem type from CLI args and kernel profile config.
///
/// Priority: explicit `--rootfs-type` CLI flag > kernel profile config > None (ext4).
fn resolve_rootfs_type(args: &RunArgs) -> Option<String> {
    crate::setup::resolve_rootfs_type(
        args.rootfs_type.as_ref(),
        args.kernel_profile.as_deref().unwrap_or("default"),
    )
}

/// Resolve the image delivery mode from CLI args and kernel profile.
///
/// Priority: explicit `--image-mode` > auto-detect from kernel profile.
/// Auto-detect: kernel profile name containing "btrfs" → Btrfs, otherwise Overlay.
fn resolve_image_mode(args: &RunArgs) -> crate::firecracker::ImageMode {
    use crate::firecracker::ImageMode;

    // Explicit CLI flag wins
    if let Some(ref mode) = args.image_mode {
        return match mode {
            crate::cli::ImageMode::Overlay => ImageMode::Overlay,
            crate::cli::ImageMode::Btrfs => ImageMode::Btrfs,
            crate::cli::ImageMode::Archive => ImageMode::Archive,
        };
    }

    // Auto-detect from kernel profile name
    if let Some(ref profile_name) = args.kernel_profile {
        if profile_name.contains("btrfs") {
            return ImageMode::Btrfs;
        }
    }

    // Default: overlay
    ImageMode::Overlay
}

/// Whether this run exports its `localhost/` image to a disk and attaches it.
/// A disk-only clone has the image in its captured container storage and
/// exports nothing.
fn exports_localhost_image(args: &RunArgs) -> bool {
    args.image.starts_with("localhost/") && args.rootfs_override.is_none()
}

/// Whether the guest of this run gets an image disk: one exported from a
/// `localhost/` image, or the one a disk-only snapshot recorded.
fn attaches_image_disk(args: &RunArgs) -> bool {
    exports_localhost_image(args) || args.image_disk_override.is_some()
}

/// Build the VMM runtime configuration from the selected kernel profile.
///
/// Keeping the profile/fallback selection and configured-binary resolution in
/// one tested function prevents callers from accidentally turning an exact
/// Firecracker resolution failure back into a PATH fallback.
async fn runtime_config_from_kernel_profiles<Resolve, ResolveFuture>(
    profile_name: &str,
    profile: Option<crate::setup::KernelProfile>,
    default_profile: Option<crate::setup::KernelProfile>,
    mut resolve: Resolve,
) -> Result<RuntimeConfig>
where
    Resolve: FnMut(crate::setup::KernelProfile, String) -> ResolveFuture,
    ResolveFuture: std::future::Future<Output = Result<Option<PathBuf>>>,
{
    let mut config = RuntimeConfig::default();
    let Some(profile) = profile else {
        return Ok(config);
    };

    let configured_profile =
        if profile.firecracker_repo.is_some() || profile.firecracker_commit.is_some() {
            Some((profile.clone(), profile_name.to_string()))
        } else if profile_name != "default" {
            default_profile.and_then(|default_profile| {
                (default_profile.firecracker_repo.is_some()
                    || default_profile.firecracker_commit.is_some())
                .then(|| (default_profile, "default".to_string()))
            })
        } else {
            None
        };

    if let Some((configured_profile, configured_name)) = configured_profile {
        if let Some(path) = resolve(configured_profile, configured_name.clone()).await? {
            info!(firecracker_bin = %path.display(), profile = %configured_name, "from profile");
            config.firecracker_bin = Some(path);
        }
    }

    if let Some(ref fc_args) = profile.firecracker_args {
        info!(firecracker_args = %fc_args, "from profile");
        config.firecracker_args = Some(fc_args.clone());
    }
    if let Some(ref boot_args) = profile.boot_args {
        info!(boot_args = %boot_args, "from profile");
        config.boot_args = Some(boot_args.clone());
    }
    if let Some(readers) = profile.fuse_readers {
        info!(fuse_readers = %readers, "from profile");
        config.fuse_readers = Some(readers);
    }

    Ok(config)
}

/// Resolve a custom vsock directory into the exact socket path that all
/// launcher components and later snapshot commands will share.
pub(crate) fn resolve_custom_vsock_socket_path(
    configured_dir: &Path,
    current_dir: &Path,
) -> PathBuf {
    let absolute_dir = if configured_dir.is_absolute() {
        configured_dir.to_path_buf()
    } else {
        current_dir.join(configured_dir)
    };
    absolute_dir.join("vsock.sock")
}

/// Start a VM with the given args. Returns a handle to the running VM.
///
/// The VM event loop runs in a background task. The handle's `Drop` impl cancels
/// the VM, so dropping the handle triggers cleanup. Use `stop()` for explicit
/// shutdown with exit code, or `wait()` to wait for natural exit.
///
/// Note: `args.no_snapshot` is forced to `true` to skip snapshot cache lookups,
/// ensuring a fresh VM is always started.
pub async fn start_vm(mut args: RunArgs) -> Result<VmHandle> {
    // Force no-snapshot mode — start_vm always creates a fresh VM
    args.no_snapshot = true;

    let mut ctx = prepare_vm(args)
        .await?
        .ok_or_else(|| anyhow::anyhow!("unexpected snapshot cache hit with no_snapshot=true"))?;

    let vm_id = ctx.vm_id.clone();
    let name = ctx.vm_name.clone();
    let log_tx = ctx.log_tx.clone();
    let Some(vsock_socket_path) = ctx.vm_state.config.vsock_socket_path.clone() else {
        cleanup_vm_context(ctx).await;
        anyhow::bail!("prepared VM has no recorded vsock socket path");
    };
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();

    if let Err(error) =
        super::common::publish_lifecycle_ready(&ctx.state_manager, &mut ctx.vm_state).await
    {
        cleanup_vm_context(ctx).await;
        return Err(error);
    }

    // Get actual PID from VM state (set during prepare_vm)
    let actual_pid = ctx
        .state_manager
        .load_state_by_name(&name)
        .await
        .ok()
        .and_then(|s| s.pid)
        .unwrap_or(0);

    let task = tokio::spawn(async move {
        let result = run_vm_loop(&mut ctx, cancel_clone).await;
        cleanup_vm_context(ctx).await;
        result
    });

    Ok(VmHandle {
        vm_id,
        name,
        pid: actual_pid,
        vsock_socket_path,
        cancel,
        task: Some(task),
        log_tx,
    })
}

/// Main dispatcher for podman commands
pub async fn cmd_podman(args: PodmanArgs) -> Result<()> {
    match args.cmd {
        PodmanCommands::Run(run_args) => cmd_podman_run(run_args).await,
        PodmanCommands::Prepare(run_args) => cmd_podman_prepare(run_args).await,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PodmanLifecycle {
    Run,
    Prepare(PrepareOptions),
}

impl PodmanLifecycle {
    fn is_prepare(&self) -> bool {
        matches!(self, PodmanLifecycle::Prepare(_))
    }
}

/// The `podman prepare` arguments that decide where the startup snapshot is
/// installed, whether an installed one is rebuilt, and how long the disposable
/// VM may take to become healthy. None of these enter the snapshot's content
/// key: they change how one build runs, not what it produces.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PrepareOptions {
    tag: Option<String>,
    force: bool,
    ready_timeout: std::time::Duration,
}

impl Default for PrepareOptions {
    fn default() -> Self {
        Self {
            tag: None,
            force: false,
            ready_timeout: PREPARE_HEALTH_BUDGET,
        }
    }
}

fn snapshot_source_disposition(
    lifecycle: &PodmanLifecycle,
) -> super::common::SnapshotSourceDisposition {
    match lifecycle {
        PodmanLifecycle::Run => super::common::SnapshotSourceDisposition::Resume,
        PodmanLifecycle::Prepare(_) => super::common::SnapshotSourceDisposition::LeavePaused,
    }
}

fn validate_prepare_args(args: &RunArgs) -> Result<()> {
    anyhow::ensure!(
        !args.no_snapshot,
        "podman prepare cannot be used with --no-snapshot"
    );
    anyhow::ensure!(!args.tty, "podman prepare does not support --tty");
    anyhow::ensure!(
        !args.interactive,
        "podman prepare does not support --interactive"
    );
    anyhow::ensure!(
        args.vsock_dir.is_none(),
        "podman prepare does not support --vsock-dir because its source VM is disposable"
    );
    anyhow::ensure!(
        args.hypervisor == crate::cli::args::Hypervisor::Firecracker,
        "podman prepare currently requires --hypervisor firecracker"
    );
    anyhow::ensure!(
        args.rootfs_override.is_none(),
        "podman prepare cannot prepare an internal disk-only clone"
    );
    anyhow::ensure!(
        std::env::var("FCVM_NO_SNAPSHOT").map_or(true, |value| value.is_empty()),
        "podman prepare cannot run while FCVM_NO_SNAPSHOT disables snapshots"
    );
    Ok(())
}

/// Refuse a balloon target above the guest's memory. Firecracker refuses it too,
/// when a cold boot attaches the device and when a restore sets the target, and
/// by then the network and the VMM exist. Checked before either so a cache hit
/// and a cold boot refuse the same run the same way.
fn validate_balloon_target(balloon: Option<u32>, mem: u32) -> Result<()> {
    if let Some(target) = balloon {
        anyhow::ensure!(
            target <= mem,
            "--balloon {target} is above --mem {mem}: a balloon target cannot exceed the guest's memory"
        );
    }
    Ok(())
}

/// Whether this invocation restores a pre-start snapshot it finds. A `podman run` does.
/// So does a `podman prepare`: the snapshot is taken before the container starts, so a
/// prepare boots from it to the same point a cold boot reaches after the image import,
/// and then waits for Healthy and installs its startup snapshot as a cold prepare does
/// (#953). `prepare --force` cold-boots: it is for content that changed without its key
/// changing, a repointed remote image reference, and the pre-start snapshot under that
/// key holds the old content too.
fn restores_pre_start(lifecycle: &PodmanLifecycle) -> bool {
    match lifecycle {
        PodmanLifecycle::Run => true,
        PodmanLifecycle::Prepare(options) => !options.force,
    }
}

/// Whether a run takes a startup snapshot (memory and disk of the running container) at
/// its first Healthy, so the next identical run restores the container already running
/// instead of starting it with `podman run`.
///
/// Healthy is the HTTP check with `--health-check`, podman's healthcheck for an image that
/// defines one, and otherwise the container's start notification (#1161) confirmed over
/// exec (see `health::check_health_once`). The key is the pre-start key, which already
/// holds what a restore cannot change about a running container (image, command,
/// environment, volumes, ...); the name and labels stay restore-time.
///
/// A plain run takes none unless it asks (`--startup-snapshot`, which `fcvm serve` passes,
/// or `--health-check`): an app that makes unique state when it starts (keys, IDs, random
/// seeds, a registration with a server) would otherwise share it, silently, with every run
/// restored from the snapshot.
fn should_arm_startup_snapshot(
    skip_snapshot_creation: bool,
    has_snapshot_key: bool,
    takes_startup_snapshot: bool,
    lifecycle: &PodmanLifecycle,
) -> bool {
    !skip_snapshot_creation
        && has_snapshot_key
        && (lifecycle.is_prepare() || takes_startup_snapshot)
}

/// Whether a `podman run` with these arguments takes a startup snapshot once healthy, and
/// restores one that exists, see [`should_arm_startup_snapshot`]. A run that did not ask
/// must not restore one either: a snapshot another run took under the same key holds that
/// run's startup state.
fn run_takes_startup_snapshot(args: &RunArgs) -> bool {
    args.health_check.is_some() || args.startup_snapshot
}

/// Whether this invocation must stay out of the snapshot cache entirely — no
/// lookup, no creation. `env_no_snapshot` is `FCVM_NO_SNAPSHOT` set non-empty.
///
/// MAXIMUM REUSE / CACHEABILITY is a core fcvm principle: an entry joins this
/// list only when a cached artifact would BEHAVE differently for this
/// invocation and the difference cannot be reconciled at restore time. Flags
/// that only change WHERE something binds (e.g. `--vsock-dir`) are honored by
/// the restore path instead of opting out.
fn snapshot_cache_opt_out(args: &RunArgs, env_no_snapshot: bool) -> bool {
    args.no_snapshot
        // A disk-only clone cold-boots from the captured disk and must never divert
        // into the snapshot-cache / UFFD restore path: the cached memory was taken over
        // the base image's disk, not the captured one, and a restore cannot make the two
        // agree.
        || args.rootfs_override.is_some()
        || env_no_snapshot
}

/// Where one `podman prepare` invocation installs its startup snapshot, and what an
/// already-installed generation there has to look like to answer for it.
///
/// The content-addressed key is the identity either way. `--tag` only chooses the name
/// it is installed under, so a tag whose generation holds different content is a miss
/// and gets rebuilt, exactly as a changed cache key would be.
fn prepare_install_target(options: &PrepareOptions, content_key: &str) -> Result<PreparedTarget> {
    let (name, snapshot_type) = match options.tag.as_deref() {
        // A caller-named artifact. It is a User snapshot because that is what naming one
        // is for: `snapshots prune` must not reclaim a golden snapshot out from under the
        // caller that asked for it by name.
        Some(tag) => {
            crate::storage::validate_snapshot_name(tag).context("invalid --tag")?;
            (tag.to_string(), crate::storage::SnapshotType::User)
        }
        // The content-addressed cache entry `podman run` would have built. Prunable.
        None => (
            content_key.to_string(),
            crate::storage::SnapshotType::System,
        ),
    };
    Ok(PreparedTarget {
        // `--force` is the only thing that rebuilds content whose cache key did not
        // change, which is what a repointed remote image reference does.
        publish_installed: !options.force,
        // Under the content-addressed key, an installed generation holds this same
        // content, so losing that race is a result worth reusing. A caller-chosen name is
        // only reached past `publish_installed` when it held nothing or held other
        // content, and `--force` asks for a rebuild either way.
        existing: if options.tag.is_some() || options.force {
            ExistingGeneration::Replace
        } else {
            ExistingGeneration::Reuse
        },
        name,
        content_key: content_key.to_string(),
        guest_thp: None,
        snapshot_type,
        free_at_start: None,
    })
}

const MIB: u64 = 1024 * 1024;

/// A snapshot an install replaces. Its space comes back only once the new generation is
/// installed, so it counts against the free space the new one needs.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ReplacedSnapshot {
    name: String,
    /// Allocated bytes of its files. Extents it shares with other files (reflinks) are
    /// counted too, so this is what deleting it frees at most.
    bytes: u64,
}

/// The generation a prepare replaces at its target: one installed there when the target
/// replaces what it finds (`--force`, or a `--tag` holding other content).
async fn replaced_snapshot(target: &PreparedTarget) -> Option<ReplacedSnapshot> {
    if target.existing != ExistingGeneration::Replace {
        return None;
    }
    let dir = paths::snapshot_dir().join(&target.name);
    if !dir.join("config.json").exists() {
        return None;
    }
    let bytes = tokio::task::spawn_blocking(move || allocated_bytes(&dir))
        .await
        .ok()?;
    Some(ReplacedSnapshot {
        name: target.name.clone(),
        bytes,
    })
}

/// The allocated bytes of every file under `dir`, subdirectories included. Unreadable
/// entries count as nothing.
fn allocated_bytes(dir: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(meta) if meta.is_dir() => allocated_bytes(&entry.path()),
            Ok(meta) if meta.is_file() => meta.blocks() * 512,
            _ => 0,
        })
        .sum()
}

/// Refuse a run whose snapshot the snapshot filesystem cannot hold, before any VM process
/// exists (#968): `podman prepare` fails without its snapshot, and a cold `--gpu` run
/// without its pre-start snapshot, so either would otherwise boot, import and warm up for
/// as long as the workload takes and then fail. `required` is what the snapshot-time check
/// will ask for (`common::snapshot_space_required`) and `available` what the filesystem
/// has now; None (statvfs failed) refuses nothing, as at snapshot time. The check at
/// snapshot time stays: the VM's own disk and other writers can take space meanwhile.
fn refuse_unfittable_snapshot(
    what: &str,
    required: u64,
    available: Option<u64>,
    dir: &Path,
    replaced: Option<&ReplacedSnapshot>,
) -> Result<()> {
    let Some(available) = available else {
        return Ok(());
    };
    if available >= required {
        return Ok(());
    }
    let mut message = format!(
        "{what} needs up to {} MiB on {} and {} MiB are free, so no VM was started. Use --mem \
         to limit VM memory, or free space on that filesystem (or increase btrfs_size in \
         rootfs-config.toml).",
        required / MIB,
        dir.display(),
        available / MIB
    );
    if let Some(replaced) = replaced {
        message.push_str(&format!(
            " The snapshot {} that this replaces holds up to {} MiB of that filesystem until \
             the new one is installed.",
            replaced.name,
            replaced.bytes / MIB
        ));
    }
    bail!(message)
}

/// `refuse_unfittable_snapshot` against the snapshot filesystem as it is now. Returns the
/// free bytes it read, for the snapshot-time refusal to compare against.
async fn check_snapshot_space_before_boot(
    what: &str,
    memory_mib: u32,
    diff: bool,
    target: Option<&PreparedTarget>,
) -> Result<Option<u64>> {
    let dir = paths::snapshot_dir();
    let required = super::common::snapshot_space_required(memory_mib, diff);
    let available = super::common::snapshot_space_available(&dir, required).await;
    let replaced = match target {
        Some(target) => replaced_snapshot(target).await,
        None => None,
    };
    refuse_unfittable_snapshot(what, required, available, &dir, replaced.as_ref())?;
    Ok(available)
}

/// Add to a snapshot refused for space what the prepare knows and the snapshot code does
/// not: how far free space fell since the prepare checked it before its VM existed, which
/// points at the VM's own disk writes or another writer, and how much the snapshot it
/// replaces still holds. Any other error is returned as it is.
fn explain_snapshot_space_shortfall(
    error: anyhow::Error,
    free_at_start: Option<u64>,
    replaced: Option<&ReplacedSnapshot>,
) -> anyhow::Error {
    let Some(shortfall) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<super::common::SnapshotSpaceShortfall>())
    else {
        return error;
    };
    let mut parts = Vec::new();
    if let Some(start) = free_at_start {
        parts.push(format!(
            "free space fell from {} MiB when this prepare started to {} MiB: the VM's own \
             disk writes, or another writer on that filesystem, took {} MiB",
            start / MIB,
            shortfall.available_bytes / MIB,
            start.saturating_sub(shortfall.available_bytes) / MIB
        ));
    }
    if let Some(replaced) = replaced {
        parts.push(format!(
            "the snapshot {} that this replaces holds up to {} MiB until the new one is \
             installed",
            replaced.name,
            replaced.bytes / MIB
        ));
    }
    if parts.is_empty() {
        return error;
    }
    error.context(parts.join("; "))
}

/// Whether an installed generation is the one this invocation asked for.
///
/// Returns the reason it is not, for the log line that explains a rebuild.
fn prepared_generation_mismatch(
    config: &crate::storage::SnapshotConfig,
    target: &PreparedTarget,
) -> Option<String> {
    if config.name != target.name {
        return Some(format!(
            "config names snapshot {} instead of {}",
            config.name, target.name
        ));
    }
    if config.content_key() != target.content_key {
        return Some(format!(
            "holds content {} instead of {}",
            config.content_key(),
            target.content_key
        ));
    }
    if config.snapshot_type != target.snapshot_type {
        return Some(format!(
            "is a {} snapshot instead of a {} snapshot",
            config.snapshot_type, target.snapshot_type
        ));
    }
    if config.kind != crate::storage::SnapshotKind::Full {
        return Some(format!("is a {} snapshot, not a full one", config.kind));
    }
    None
}

#[derive(Clone, Copy, Debug, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum PreparedCache {
    Hit,
    Created,
}

#[derive(Debug, serde::Serialize, PartialEq, Eq)]
struct PreparedSnapshotOutput {
    status: &'static str,
    cache: PreparedCache,
    /// Snapshot name every other command addresses this artifact by.
    snapshot_key: String,
    /// Content-addressed key whose content it holds. Equal to `snapshot_key` without `--tag`.
    content_key: String,
    /// `user` for a `--tag` artifact `snapshots prune` keeps, `system` for a prunable
    /// cache entry.
    snapshot_type: String,
    generation_id: String,
    config_digest: String,
}

pub(crate) struct PreparedSnapshot {
    output: PreparedSnapshotOutput,
    // Held through JSON publication so a concurrent creator/deleter cannot replace the
    // generation between verification and the successful command response.
    _generation_lock: std::fs::File,
}

enum VmPreparation {
    Active(Box<VmContext>),
    RunCompleted,
    Prepared(PreparedSnapshot),
}

async fn verify_prepared_snapshot(
    target: &PreparedTarget,
    cache: PreparedCache,
) -> Result<Option<PreparedSnapshot>> {
    verify_prepared_snapshot_in(&paths::data_dir(), &paths::snapshot_dir(), target, cache).await
}

/// Flush a prepared generation to disk: every file under `dir`, each directory
/// after its files, then `dir` itself. The disks sit in the `disk/` subvolume,
/// so the walk descends. Returns the paths it synced in that order.
fn sync_generation(dir: &std::path::Path) -> Result<Vec<std::path::PathBuf>> {
    let mut synced = Vec::new();
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("reading prepared generation {}", dir.display()))?
    {
        let entry = entry.context("reading prepared generation entry")?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .context("reading prepared generation entry type")?;
        if file_type.is_dir() {
            synced.extend(sync_generation(&path)?);
        } else if file_type.is_file() {
            std::fs::File::open(&path)
                .with_context(|| format!("opening prepared artifact {}", path.display()))?
                .sync_all()
                .with_context(|| format!("syncing prepared artifact {}", path.display()))?;
            synced.push(path);
        }
    }
    std::fs::File::open(dir)
        .with_context(|| format!("opening prepared generation {}", dir.display()))?
        .sync_all()
        .with_context(|| format!("syncing prepared generation {}", dir.display()))?;
    synced.push(dir.to_path_buf());
    Ok(synced)
}

/// Verify the generation installed at `target.name`.
///
/// `Ok(None)` means this invocation has nothing to publish: either nothing is installed
/// there, or a caller-chosen name holds something else. Both are cache misses and the
/// caller rebuilds. `Err` means the generation cannot be published and rebuilding it
/// would not help: it is truncated, points outside itself, or sits at the
/// content-addressed key while describing different content.
async fn verify_prepared_snapshot_in(
    data_dir: &std::path::Path,
    snapshot_root: &std::path::Path,
    target: &PreparedTarget,
    cache: PreparedCache,
) -> Result<Option<PreparedSnapshot>> {
    let snapshot_key = target.name.as_str();
    let snapshot_dir = snapshot_root.join(snapshot_key);
    tokio::fs::create_dir_all(snapshot_root)
        .await
        .context("creating snapshot directory")?;
    let generation_lock = super::common::acquire_snapshot_dir_lock(&snapshot_dir, false).await?;

    let config_path = snapshot_dir.join("config.json");
    if !tokio::fs::try_exists(&config_path)
        .await
        .with_context(|| format!("checking prepared snapshot {}", config_path.display()))?
    {
        return Ok(None);
    }

    let manager = crate::storage::SnapshotManager::new(snapshot_root.to_path_buf());
    let (config, generation) = manager
        .load_snapshot_with_generation(snapshot_key)
        .await
        .with_context(|| format!("loading prepared snapshot {snapshot_key}"))?;

    if let Some(reason) = prepared_generation_mismatch(&config, target) {
        // A caller-chosen name can legitimately hold anything, so a generation that is
        // not this one is a miss and gets rebuilt. The content-addressed key cannot hold
        // anything else by construction, so a mismatch there is a hand-edited or corrupt
        // generation: say so now instead of after a ten-minute rebuild that then refuses
        // to publish it. `--force` skips this check entirely and replaces it.
        anyhow::ensure!(
            target.name != target.content_key,
            "prepared snapshot {} {}",
            snapshot_key,
            reason
        );
        info!(
            snapshot_key = %snapshot_key,
            reason = %reason,
            "installed snapshot does not answer for these arguments; rebuilding"
        );
        return Ok(None);
    }
    if config.metadata.guest_thp != target.guest_thp {
        let mode = |m: &Option<String>| m.clone().unwrap_or_else(|| "the kernel's default".into());
        let (recorded, wanted) = (mode(&config.metadata.guest_thp), mode(&target.guest_thp));
        // The mode is set at restore, so it is not keyed, and both modes share the
        // content-addressed entry. Replacing that entry would change the mode another
        // caller's restores inherit, so only `--force` or a `--tag` of its own does that.
        anyhow::ensure!(
            target.name != target.content_key,
            "prepared snapshot {snapshot_key} restores guest transparent huge pages in {recorded} \
             and this prepare asks for {wanted}; pass --tag to prepare a snapshot of its own, \
             or --force to rebuild this one in that mode"
        );
        info!(
            snapshot_key = %snapshot_key,
            recorded = %recorded,
            wanted = %wanted,
            "installed snapshot restores another transparent huge page mode; rebuilding"
        );
        return Ok(None);
    }
    anyhow::ensure!(
        config.generation_id != uuid::Uuid::nil(),
        "prepared snapshot {} has a nil generation ID",
        snapshot_key
    );

    for (label, configured, expected) in [
        (
            "memory",
            &config.memory_path,
            snapshot_dir.join("memory.bin"),
        ),
        (
            "VM state",
            &config.vmstate_path,
            snapshot_dir.join("vmstate.bin"),
        ),
        (
            "disk",
            &config.disk_path,
            crate::storage::snapshot::snapshot_disk_dir(&snapshot_dir).join("disk.raw"),
        ),
    ] {
        anyhow::ensure!(
            configured == &expected,
            "prepared snapshot {} {} path is not {}: {}; delete and recreate the snapshot",
            snapshot_key,
            label,
            expected.display(),
            configured.display()
        );
        let metadata = tokio::fs::metadata(configured).await.with_context(|| {
            format!(
                "reading prepared snapshot {} {} artifact {}",
                snapshot_key,
                label,
                configured.display()
            )
        })?;
        anyhow::ensure!(
            metadata.is_file() && metadata.len() > 0,
            "prepared snapshot {} {} artifact is not a non-empty regular file: {}",
            snapshot_key,
            label,
            configured.display()
        );
    }

    // The three core artifacts are not the whole generation. A clone also opens every
    // extra disk and, for a portable volume, its inode table — so a generation missing
    // one of those would report `prepared` here and fail (or silently renumber inodes)
    // at restore, after the cache entry is already durable and shared.
    for disk in &config.metadata.extra_disks {
        let mut components = std::path::Path::new(&disk.filename).components();
        let contained = matches!(components.next(), Some(std::path::Component::Normal(_)))
            && components.next().is_none();
        anyhow::ensure!(
            contained,
            "prepared snapshot {} extra disk {} names a path outside its generation: {}",
            snapshot_key,
            disk.drive_id,
            disk.filename
        );
        let path = crate::storage::snapshot::snapshot_disk_dir(&snapshot_dir).join(&disk.filename);
        let metadata = tokio::fs::metadata(&path).await.with_context(|| {
            format!(
                "reading prepared snapshot {} extra disk {} artifact {}",
                snapshot_key,
                disk.drive_id,
                path.display()
            )
        })?;
        anyhow::ensure!(
            metadata.is_file() && metadata.len() > 0,
            "prepared snapshot {} extra disk {} artifact is not a non-empty regular file: {}",
            snapshot_key,
            disk.drive_id,
            path.display()
        );
    }

    // Portable volumes restore their inode numbering from a table written into the
    // generation before its atomic rename (see create_podman_snapshot). A clone that
    // finds no table renumbers inodes, which is exactly the glitch the table prevents.
    for volume in config.metadata.volumes.iter().filter(|v| v.portable) {
        let path = snapshot_dir.join(crate::volume::inode_table_file_name(volume.vsock_port));
        let metadata = tokio::fs::metadata(&path).await.with_context(|| {
            format!(
                "reading prepared snapshot {} portable volume {} inode table {}",
                snapshot_key,
                volume.guest_path,
                path.display()
            )
        })?;
        anyhow::ensure!(
            metadata.is_file() && metadata.len() > 0,
            "prepared snapshot {} portable volume {} inode table is not a non-empty regular file: {}",
            snapshot_key,
            volume.guest_path,
            path.display()
        );
    }

    // The memory snapshot holds the guest's view of each pmem image as it was, and Firecracker
    // reopens the recorded path at restore, so a generation whose image changed can never be
    // restored. Remove that exact generation, as the run path does, and rebuild: a rebuild
    // under the content key would otherwise reuse the installed generation it replaces.
    if let Err(error) =
        crate::storage::pmem::check_snapshot_pmem_images(data_dir, &config.metadata.pmem_devices)
    {
        if classify_restore_failure(&error) != RestoreFault::UnusableSnapshot {
            return Err(error.context(format!("checking prepared snapshot {snapshot_key}")));
        }
        info!(
            snapshot_key = %snapshot_key,
            reason = %format!("{error:#}"),
            "installed snapshot's pmem image changed; removing it and rebuilding"
        );
        drop(generation_lock);
        manager
            .delete_snapshot_if_generation(snapshot_key, &generation)
            .await
            .with_context(|| format!("removing prepared snapshot {snapshot_key}"))?;
        return Ok(None);
    }

    // `prepare` is a durability boundary, not merely a namespace rename. Flush every file
    // in the installed generation, then the generation and snapshot-root directories, while
    // the shared generation lease prevents replacement. Success therefore survives a host
    // crash after the JSON response rather than depending on eventual writeback.
    let sync_dir = snapshot_dir.clone();
    let sync_root = snapshot_root.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<()> {
        sync_generation(&sync_dir)?;
        std::fs::File::open(&sync_root)
            .with_context(|| format!("opening snapshot root {}", sync_root.display()))?
            .sync_all()
            .with_context(|| format!("syncing snapshot root {}", sync_root.display()))?;
        Ok(())
    })
    .await
    .context("joining prepared snapshot durability sync")??;

    Ok(Some(PreparedSnapshot {
        output: PreparedSnapshotOutput {
            status: "prepared",
            cache,
            snapshot_key: snapshot_key.to_string(),
            content_key: config.content_key().to_string(),
            snapshot_type: config.snapshot_type.to_string(),
            generation_id: generation.generation_id().to_string(),
            config_digest: generation.config_digest_hex(),
        },
        _generation_lock: generation_lock,
    }))
}

fn publish_prepared_snapshot(prepared: &PreparedSnapshot) -> Result<()> {
    use std::io::Write;

    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, &prepared.output)
        .context("serializing prepared snapshot result")?;
    writeln!(&mut stdout).context("writing prepared snapshot result")?;
    stdout
        .flush()
        .context("flushing prepared snapshot result")?;
    Ok(())
}

/// True when the VM's kernel profile enables ARM64 NV2 nested virtualization
/// (the profile's firecracker_args carry --enable-nv2). Drives the
/// miss-path-converges-on-restore behavior, which exists for NV2 guests only.
fn nv2_profile(kernel_profile: &Option<String>) -> bool {
    let Some(name) = kernel_profile.as_deref() else {
        return false;
    };
    matches!(
        crate::setup::get_kernel_profile(name),
        Ok(Some(profile)) if profile.enables_nv2()
    )
}

/// Refuse `--free-page-reporting` where fcvm cannot turn it on: with no balloon
/// device to carry it, with hugepages, and on Cloud Hypervisor. Checked with the
/// balloon target, before the snapshot cache is looked up, so a cache hit and a
/// cold boot refuse the same run the same way. It is checked here and not by clap
/// so that RunArgs clap never parsed (the serve API, a cold boot from a snapshot, a
/// library caller) are checked too.
fn validate_free_page_reporting(args: &RunArgs) -> Result<()> {
    if !args.free_page_reporting {
        return Ok(());
    }
    anyhow::ensure!(
        args.balloon.is_some(),
        "--free-page-reporting needs --balloon: free page reporting is a feature of the \
         balloon device, and a VM has one only with --balloon (--balloon 0 attaches one that \
         holds no memory)"
    );
    anyhow::ensure!(
        !args.hugepages,
        "--free-page-reporting is not supported with --hugepages: fcvm supports it only when \
         guest memory is not backed by huge pages"
    );
    anyhow::ensure!(
        args.hypervisor == crate::cli::args::Hypervisor::Firecracker,
        "--free-page-reporting is not supported with --hypervisor cloud-hypervisor: fcvm turns \
         it on only on a Firecracker balloon"
    );
    Ok(())
}

/// `--gpu` devices are attached when fcvm restores the pre-start snapshot, which is
/// taken without them. A run that never restores that snapshot would run without them.
fn validate_gpu_run(args: &RunArgs, lifecycle: &PodmanLifecycle, no_snapshot: bool) -> Result<()> {
    anyhow::ensure!(
        args.hypervisor == crate::cli::args::Hypervisor::Firecracker,
        "--gpu is not supported with --hypervisor cloud-hypervisor: fcvm attaches GPUs only \
         on a Firecracker restore"
    );
    anyhow::ensure!(
        !lifecycle.is_prepare(),
        "--gpu is not supported by podman prepare: a prepared snapshot is taken after the \
         container starts, and a VM with a passthrough device can not be snapshotted"
    );
    anyhow::ensure!(
        !no_snapshot,
        "--gpu needs the snapshot cache: its devices are attached when fcvm restores the \
         pre-start snapshot, and this run skips the cache (--no-snapshot, FCVM_NO_SNAPSHOT \
         or a disk-only clone)"
    );
    Ok(())
}

/// Whether the guest agent's first "cache-ready" ask is handed to the run loop:
/// when the run may take a pre-start snapshot, and when a cold boot
/// with `--free-page-reporting` owes the check of its balloon
/// (`settle_balloon_check`), which runs at that ask. `podman prepare` has no run
/// loop to hand it to, and checks after its health wait.
fn ask_reaches_run_loop(
    skip_snapshot_creation: bool,
    balloon_check_due: bool,
    lifecycle: &PodmanLifecycle,
) -> bool {
    (!skip_snapshot_creation || balloon_check_due) && !lifecycle.is_prepare()
}

/// Run the check a cold boot with `--free-page-reporting` owes, once: that the
/// guest brought its balloon device up (`require_active_balloon`, which says why
/// that can fail and why it matters). Callers run it when the guest's agent is up
/// and before a snapshot of the VM is taken. A VM that owes no check, and one
/// already checked, return at once.
async fn settle_balloon_check(ctx: &mut VmContext) -> Result<()> {
    if !ctx.balloon_check_due {
        return Ok(());
    }
    let client = crate::firecracker::FirecrackerClient::new(ctx.data_dir.join("firecracker.sock"))?;
    crate::commands::common::require_active_balloon(&client, &ctx.disk_path).await?;
    ctx.balloon_check_due = false;
    info!("the guest brought up its balloon device with free page reporting");
    Ok(())
}

/// The `snapshot run` arguments that restore this run from a cached snapshot.
///
/// `publish` is this run's own `--publish`, so the clone listens where this run asked,
/// and a spec the cache key's lenient parse skipped is refused here as it is on a cache
/// miss. The key hashes the mappings, so today they equal the snapshot's.
/// `--label KEY=VALUE` flags as the state's label map. A flag without `=` is dropped.
pub(crate) fn parse_labels(labels: &[String]) -> std::collections::HashMap<String, String> {
    labels
        .iter()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// The host identity address a run names with `--env HOST_IPV6=<addr>`, which
/// fc-agent adds to the guest's lo. The last one wins, as in the boot plan's env
/// map. A value that is not an IPv6 address names none.
fn run_host_ipv6(env: &[String]) -> Option<std::net::Ipv6Addr> {
    let value = env
        .iter()
        .rev()
        .find_map(|entry| entry.strip_prefix("HOST_IPV6="))?;
    match value.parse() {
        Ok(address) => Some(address),
        Err(_) => {
            if !value.is_empty() {
                warn!("HOST_IPV6={value} is not an IPv6 address; the restored guest gets none");
            }
            None
        }
    }
}

fn snapshot_restore_args(
    args: &RunArgs,
    snapshot_key: &str,
    startup_snapshot_base_key: Option<String>,
    (firecracker_bin, firecracker_args): (Option<String>, Option<String>),
) -> crate::cli::SnapshotRunArgs {
    crate::cli::SnapshotRunArgs {
        pid: None,
        snapshot: Some(snapshot_key.to_string()),
        name: Some(args.name.clone()),
        exec: None,
        no_dirty_tracking: false, // podman needs dirty tracking for later snapshots
        no_swap: args.no_swap,
        vsock_dir: args.vsock_dir.clone(),
        publish: args.publish.clone(),
        map: Vec::new(),
        ipv6_prefix: launch_ipv6_prefix(args),
        ipv6_address: args.ipv6_address,
        // This run's HOST_IPV6, which the snapshot key leaves out: the restored guest
        // takes it in place of the one its snapshot was booted with.
        host_ipv6: run_host_ipv6(&args.env),
        // A VM with a passthrough device can not be snapshotted, so a --gpu run takes
        // no startup snapshot.
        startup_snapshot_base_key: startup_snapshot_base_key.filter(|_| args.gpu.is_empty()),
        cpu: Some(args.cpu),
        label: args.label.clone(),
        mem: Some(args.mem),
        balloon: args.balloon,
        firecracker_bin,
        firecracker_args,
        gpu: args.gpu.clone(),
        hugepages: Some(args.hugepages),
        non_blocking_output: args.non_blocking_output,
        on_restore: args.on_restore.clone(),
        // This run's own flags or nothing: the snapshot's recorded hook and huge page
        // mode may be another run's (neither is in the cache key). No mode is the
        // guest kernel's own, which the restore asks for by name.
        inherit_recorded_hook: false,
        guest_thp: args.guest_thp.clone(),
        // This run's own check: the URL is not in the pre-start key, so the snapshot's
        // recorded check may be another run's (#953), and the timeout is in no key.
        health_check: Some(crate::cli::RestoreHealthCheck {
            url: args.health_check.clone(),
            timeout_secs: args.health_check_timeout,
        }),
        prepare: None,
    }
}

/// What a failed restore of a cached snapshot says about that snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreFault {
    /// The artifact itself can never load: delete it and boot fresh, which builds a new one.
    UnusableSnapshot,
    /// Firecracker's load failed for a reason of this run or this host (a socket left at a
    /// listener path, a memory server that went away): boot fresh and keep the entry, which
    /// the next run can still restore (#988).
    LoadFailed,
    /// Not a load fault: the run fails.
    Other,
}

/// What Firecracker or fcvm says when the cached artifact itself is unusable.
///
/// - "Failed to load snapshot state from file" wraps every error of reading vmstate.bin
///   (`SnapshotError`: CRC, magic, format version, bitcode, a short read, the size cap).
///   "Invalid data version" and "An error occurred during bitcode serialization" are two of
///   them, kept on their own because v1.17.0 reports a format 11 snapshot with the latter.
/// - "Invalid snapshot state" is the vmstate's sanity check (no DRAM, too many slots).
/// - "Unknown ... Device" is an override naming a device the vmstate does not hold, which
///   fcvm builds from the snapshot's own metadata, so the two disagree.
/// - "frequency not present in the given snapshot" is a snapshot with no TSC frequency.
/// - "image disk build changed" is `verify_image_disk_identity`'s marker: the snapshot names a
///   disk build that no longer exists at that path. "snapshot pmem image changed" is
///   `check_snapshot_pmem_images`', for the same reason: Firecracker reopens the recorded path.
const UNUSABLE_SNAPSHOT_MARKERS: &[&str] = &[
    "Failed to load snapshot state from file",
    "Invalid data version",
    "An error occurred during bitcode serialization",
    "Invalid snapshot state",
    "Unknown Network Device",
    "Unknown Vsock Device",
    "Unknown Block Device",
    "frequency not present in the given snapshot",
    "image disk build changed",
    "snapshot pmem image changed",
];

/// Classify a failed restore of a cached snapshot.
///
/// Firecracker prefixes every snapshot-load fault with "Load snapshot error", which fcvm's
/// API client embeds verbatim, whether the cause is in the artifact or on the host. Matching
/// only the prefix deleted good snapshots: a stale socket at a clone's `--vsock-dir` made the
/// load fail with "Address already in use", and both cached entries were removed (#988). So
/// only the markers in [`UNUSABLE_SNAPSHOT_MARKERS`] delete an entry. A --gpu device that
/// Firecracker could not attach (busy, or the host is not set up) is a fault of this run that
/// a fresh boot would hit too, so it fails the run.
fn classify_restore_failure(err: &anyhow::Error) -> RestoreFault {
    let rendered = format!("{err:#}");
    if rendered.contains("passthrough device on restore")
        || rendered.contains("Device passthrough on restore")
    {
        return RestoreFault::Other;
    }
    if UNUSABLE_SNAPSHOT_MARKERS
        .iter()
        .any(|marker| rendered.contains(marker))
    {
        return RestoreFault::UnusableSnapshot;
    }
    if rendered.contains("Load snapshot error") {
        return RestoreFault::LoadFailed;
    }
    RestoreFault::Other
}

/// A restore that failed for a reason of this run or host: the run boots fresh, and the
/// cached snapshot stays for the next run.
fn keep_snapshot_after_load_fault(snapshot_key: &str, err: &anyhow::Error) {
    warn!(
        snapshot_key = %snapshot_key,
        error = %format!("{err:#}"),
        "cached snapshot failed to load for a reason outside the snapshot; keeping it and \
         falling back to a fresh boot"
    );
}

/// Delete a cached snapshot that failed to load so the run can fall back to a
/// fresh boot (which re-creates the snapshot with the current Firecracker).
///
/// Invalidation takes the per-snapshot flock exclusively and revalidates the
/// exact generation used by the failed restore.  A creator may install a new
/// generation after the restore releases its shared lease but before this
/// function gets the exclusive lease; that replacement must survive.
async fn invalidate_unusable_snapshot(
    snapshot_key: &str,
    expected_generation: Option<&crate::storage::SnapshotGeneration>,
    err: &anyhow::Error,
) {
    warn!(
        snapshot_key = %snapshot_key,
        error = %format!("{err:#}"),
        "cached snapshot failed to load (incompatible Firecracker snapshot \
         format?); invalidating it and falling back to a fresh boot"
    );
    let Some(expected_generation) = expected_generation else {
        warn!(
            snapshot_key = %snapshot_key,
            "failed restore did not report its snapshot generation; refusing blind invalidation"
        );
        return;
    };

    let manager = crate::storage::SnapshotManager::new(paths::snapshot_dir());
    match manager
        .delete_snapshot_if_generation(snapshot_key, expected_generation)
        .await
    {
        Ok(true) => {}
        Ok(false) => info!(
            snapshot_key = %snapshot_key,
            "failed snapshot generation was already replaced; retaining current generation"
        ),
        Err(delete_err) => warn!(
            snapshot_key = %snapshot_key,
            error = %delete_err,
            "failed to delete unusable snapshot; the next run may hit it again"
        ),
    }
}

/// Best-effort removal of host state persisted during a failed `prepare_vm`.
///
/// `prepare_vm` creates the per-VM data directory and (for rootless and routed modes
/// with published ports) persists the VM state file with an allocated loopback IP
/// before the VM is fully set up. When setup fails partway, remove both so failed
/// runs don't leave phantom `fcvm ls` entries, allocated loopback IPs, or per-VM
/// disk directories behind.
async fn cleanup_failed_prepare(
    state_manager: &StateManager,
    vm_id: &str,
    data_dir: &std::path::Path,
) {
    if let Err(e) = state_manager.delete_state(vm_id).await {
        warn!(vm_id = %vm_id, error = %e, "failed to delete VM state after setup error");
    }
    if let Err(e) = tokio::fs::remove_dir_all(data_dir).await {
        warn!(vm_id = %vm_id, error = %e, "failed to remove VM data directory after setup error");
    }
    if let Err(e) = crate::reclaim::complete(vm_id).await {
        warn!(vm_id = %vm_id, error = %format!("{e:#}"), "failed to reclaim the VM's resources after setup error");
    }
}

/// End the owner of a launch that a cache hit served, which made nothing of its own
/// beyond what its record lists. A failure here does not fail the run that already
/// finished: what remains stays recorded for the next launch.
async fn complete_unused_owner(owner: crate::reclaim::Owner) {
    if let Err(e) = owner.complete().await {
        warn!(error = %format!("{e:#}"), "resources of a launch served from a snapshot remain");
    }
}

pub async fn prepare_vm(args: RunArgs) -> Result<Option<VmContext>> {
    match prepare_vm_for_lifecycle(args, PodmanLifecycle::Run).await? {
        VmPreparation::Active(ctx) => Ok(Some(*ctx)),
        VmPreparation::RunCompleted => Ok(None),
        VmPreparation::Prepared(_) => unreachable!("run lifecycle cannot prepare an artifact"),
    }
}

/// Format version for overlay image cache. Bump when the build process
/// changes in a way that invalidates previously-cached images.
/// v2: host-side cleanup of podman state files before ext4 packaging
const OVERLAY_CACHE_VERSION: u32 = 2;

/// The content-addressed cache path the image-delivery disk for `digest` lands
/// at once built. Purely computed — existence is the caller's question to ask.
fn expected_image_disk_path(digest: &str, mode: crate::firecracker::ImageMode) -> PathBuf {
    let cache_dir = paths::image_cache_dir().join(digest);
    match mode {
        crate::firecracker::ImageMode::Overlay => PathBuf::from(format!(
            "{}.storage-v{}.img",
            cache_dir.display(),
            OVERLAY_CACHE_VERSION
        )),
        crate::firecracker::ImageMode::Btrfs | crate::firecracker::ImageMode::Archive => {
            cache_dir.with_extension("docker.tar")
        }
    }
}

/// Export a localhost/ image and build its delivery disk (overlay storage
/// image, or the Docker archive itself for btrfs/archive modes), reusing the
/// content-addressed cache when it already holds a valid artifact.
///
/// Runs BEFORE snapshot-key computation: the returned file's build identity is
/// part of the snapshot key (see `FirecrackerConfig::image_disk_identity`).
async fn export_localhost_image_disk(
    args: &RunArgs,
    image_identifier: &str,
    localhost_image_id: Option<&str>,
) -> Result<PathBuf> {
    // Reuse the digest resolved by get_image_cache_ref (already stripped of the
    // "sha256:" prefix). Using the same inspect result for the snapshot key and
    // the export cache prevents a tag rebuilt in between from being exported
    // under a digest that no longer matches the snapshot key.
    let digest = image_identifier.to_string();

    // Use content-addressable cache: /mnt/fcvm-btrfs/image-cache/{digest}/
    let image_cache_dir = paths::image_cache_dir();
    tokio::fs::create_dir_all(&image_cache_dir)
        .await
        .context("creating image-cache directory")?;

    let cache_dir = image_cache_dir.join(&digest);

    // Lock per-digest to prevent concurrent exports of the same image
    let lock_path = image_cache_dir.join(format!("{}.lock", &digest));
    let lock_file = std::fs::File::create(&lock_path).context("creating image cache lock file")?;
    if fs2::FileExt::try_lock_exclusive(&lock_file).is_err() {
        failpoint::note("lock.wait", lock_path.display());
        lock_file
            .lock_exclusive()
            .context("acquiring image cache lock")?;
    }

    // Check if already cached (inside lock to prevent race)
    // Use Docker archive format (preserves HEALTHCHECK, single tar file) for FUSE transfer
    let archive_path = cache_dir.with_extension("docker.tar");
    let needs_export = if !archive_path.exists() {
        true
    } else {
        // A cached archive that fails validation — whether it parses cleanly but is
        // missing manifest.json (Ok(false)) or is structurally corrupt and can't be
        // parsed at all (Err) — is removed and re-exported. Only the freshly exported
        // archive below treats a validation error as fatal.
        match validate_docker_archive(&archive_path) {
            Ok(true) => {
                info!(image = %args.image, digest = %digest, "Using cached Docker archive");
                false
            }
            Ok(false) => {
                warn!(path = %archive_path.display(), "Cached archive is invalid, re-exporting");
                let _ = tokio::fs::remove_file(&archive_path).await;
                true
            }
            Err(e) => {
                warn!(path = %archive_path.display(), error = %e, "Cached archive is unreadable, re-exporting");
                let _ = tokio::fs::remove_file(&archive_path).await;
                true
            }
        }
    };

    if needs_export {
        info!(image = %args.image, digest = %digest, "Exporting localhost image as Docker archive");

        // Export into a UUID-keyed temp, then atomically rename. This avoids corrupt
        // archives from interrupted exports AND is safe under cross-VM concurrency: the
        // image-cache dir is shared across VMs and the per-digest flock above does not
        // coordinate cross-VM (see crate::setup::store_temp_path). A shared "<digest>.tmp"
        // would let one VM's rename ENOENT the other's in-flight export.
        let tmp_path = crate::setup::store_temp_path(&archive_path);

        // Export by the IMMUTABLE image ID captured at inspect time, not the mutable
        // tag (#598). A parallel build can repoint the tag between the cache-key
        // inspect and this export; `podman save <tag>` would then archive the newer
        // build under the older digest's cache entry. export_image_archive pins the
        // exact content the cache key names and writes the original repo tag into the
        // archive's RepoTags, so the guest still loads and runs it by name.
        let image_id = localhost_image_id.ok_or_else(|| {
            anyhow::anyhow!("internal: localhost image id was not captured at inspect time")
        })?;
        // A UUID-keyed orphan is never overwritten by the next run, so the temp is
        // recorded before it exists and a SIGKILLed export leaves it to the next launch
        // to reclaim (crate::reclaim).
        let temps = image::temp_owner()?;
        temps.record(crate::reclaim::Resource::Temp {
            path: tmp_path.clone(),
        })?;
        let exported = async {
            image::export_image_archive(image_id, &args.image, &tmp_path, Some(&temps.holder()?))
                .await?;
            // Atomic rename within the same filesystem
            tokio::fs::rename(&tmp_path, &archive_path)
                .await
                .context("renaming exported archive to final path")
        }
        .await;
        // Removes the temp when the export or the rename failed.
        image::complete_temps(temps).await;
        if let Err(e) = exported {
            drop(lock_file);
            return Err(e);
        }

        info!(path = %archive_path.display(), "Image exported as Docker archive");
    }

    let resolved_image_mode = resolve_image_mode(args);
    info!(image = %args.image, digest = %digest, mode = %resolved_image_mode, "Image delivery mode");

    let disk_path = match resolved_image_mode {
        crate::firecracker::ImageMode::Overlay => {
            // Pre-built overlay storage: ext4 image with podman storage.
            // Guest mounts this as additionalImageStore — no podman load needed.
            let storage_img_path =
                expected_image_disk_path(&digest, crate::firecracker::ImageMode::Overlay);
            if !storage_img_path.exists() {
                info!(image = %args.image, digest = %digest, "Building overlay storage image");
                build_storage_image(&archive_path, &storage_img_path).await?;
            } else {
                info!(image = %args.image, digest = %digest, "Using cached overlay storage image");
            }
            storage_img_path
        }
        crate::firecracker::ImageMode::Btrfs => {
            // VM-side btrfs loading: attach Docker archive as read-only block device.
            // fc-agent creates btrfs loopback on rootfs and runs `podman load` from
            // the archive device into the btrfs storage.
            archive_path.clone()
        }
        crate::firecracker::ImageMode::Archive => {
            // Docker archive: attach as raw block device.
            // fc-agent reads docker-archive:/dev/vdX via podman load at boot.
            archive_path
        }
    };

    // Lock released when lock_file is dropped
    drop(lock_file);

    Ok(disk_path)
}

/// The `--publish` mappings of a run, parsed before anything else reads the specs.
///
/// A malformed spec fails the run here, before the kernel, the rootfs and the image are
/// looked up. So do two mappings that claim one host address and port: left to network
/// setup, routed and rootless networking fail the second bind after the VM's network
/// exists, and bridged networking installs both DNAT rules and delivers only the first.
fn publish_mappings(args: &RunArgs) -> Result<Vec<PortMapping>> {
    let port_mappings: Vec<PortMapping> = args
        .publish
        .iter()
        .map(|s| PortMapping::parse(s))
        .collect::<Result<Vec<_>>>()
        .context("parsing port mappings")?;

    // --publish now DNATs each published guest port to 127.0.0.1 inside the guest,
    // and --forward-localhost BINDS 127.0.0.1:<port> there as a relay to the host.
    // Overlap turns the published port into a reflector: an external client reaches
    // the HOST's service on that port and never touches the guest. It returns a
    // successful response from the wrong machine rather than an error, so it cannot
    // be left to be discovered at runtime. The overlap is on the GUEST port, which
    // is the second field of --publish.
    let forwarded: std::collections::HashSet<u16> =
        args.forward_localhost.iter().copied().collect();
    for (spec, pm) in args.publish.iter().zip(&port_mappings) {
        // TCP only: published_guest_ports carries TCP mappings alone, and the
        // --forward-localhost relay is a TCP listener, so `--publish H:G/udp`
        // alongside `--forward-localhost G` shares a port NUMBER without ever
        // sharing a socket. Rejecting it would regress a valid combination.
        if pm.proto == crate::network::Protocol::Tcp && forwarded.contains(&pm.guest_port) {
            bail!(
                "--publish {spec} and --forward-localhost {} both claim guest port {}. \
                 --publish makes that port reach the guest's 127.0.0.1:{}, which is exactly \
                 where --forward-localhost binds its relay to the host — so the published \
                 port would answer with the HOST's service instead of the guest's. \
                 Use different ports.",
                pm.guest_port,
                pm.guest_port,
                pm.guest_port
            );
        }
    }
    PortMapping::require_distinct_host_sockets(&port_mappings)?;

    Ok(port_mappings)
}

/// Replace the `--pmem` specs with the specs of the pmem store entries they resolve to,
/// and record those devices, with the image each was copied from, in `pmem_devices`.
/// Copies an image whose generation is not in the store yet, so it blocks for as long as
/// that copy takes.
fn resolve_pmem_into_store(args: &mut RunArgs, data_dir: &Path) -> Result<()> {
    let store = crate::storage::pmem_store::PmemStore::open(data_dir)?;
    args.pmem_devices =
        crate::storage::pmem::resolve_pmem_devices(&store, &args.pmem, &args.pmem_devices)?;
    args.pmem = args.pmem_devices.iter().map(|d| d.spec()).collect();
    Ok(())
}

/// Run a blocking step on a launch's --disk-dir staging, when it has one.
async fn on_disk_dir_staging(
    staging: &mut Option<crate::storage::disk_dir::DiskDirStaging>,
    step: fn(&mut crate::storage::disk_dir::DiskDirStaging) -> Result<()>,
) -> Result<()> {
    let Some(mut held) = staging.take() else {
        return Ok(());
    };
    let (held, result) = tokio::task::spawn_blocking(move || {
        let result = step(&mut held);
        (held, result)
    })
    .await
    .context("--disk-dir staging task")?;
    *staging = Some(held);
    result
}

async fn prepare_vm_for_lifecycle(
    mut args: RunArgs,
    lifecycle: PodmanLifecycle,
) -> Result<VmPreparation> {
    // Every podman launch, the library's start_vm and prepare_vm included, comes here first.
    crate::cgroup::require_swap_disabled(args.no_swap)?;
    info!(
        lifecycle = ?lifecycle,
        "Starting fcvm podman lifecycle"
    );

    if lifecycle.is_prepare() {
        validate_prepare_args(&args)?;
    }

    // Validate VM name before any setup work
    validate_vm_name(&args.name).context("invalid VM name")?;
    crate::network::require_routed_for_address(args.network.into(), args.ipv6_address)?;

    // Validate hugepages memory alignment (2MB pages require even MiB)
    if args.hugepages && !args.mem.is_multiple_of(2) {
        bail!(
            "--mem {} is not divisible by 2: hugepages requires 2MB-aligned memory size",
            args.mem
        );
    }
    validate_balloon_target(args.balloon, args.mem)?;
    validate_free_page_reporting(&args)?;
    // Refuse a bad --pmem before the snapshot cache is looked up, so a cache hit
    // and a cold boot refuse it the same way.
    if !args.pmem.is_empty()
        && matches!(
            args.hypervisor,
            crate::cli::args::Hypervisor::CloudHypervisor
        )
    {
        bail!("--pmem is not supported with --hypervisor cloud-hypervisor");
    }

    // Normalize --forward-localhost: a repeated port would otherwise fail the
    // host-side bind in routed mode. Bridged mode has no host-side relay for the
    // guest's 10.0.2.2 gateway target, so reject it instead of silently ignoring it.
    args.forward_localhost.sort_unstable();
    args.forward_localhost.dedup();
    if !args.forward_localhost.is_empty() && matches!(args.network, NetworkMode::Bridged) {
        bail!(
            "--forward-localhost is not supported with --network bridged \
             (supported modes: rootless, routed)"
        );
    }

    let port_mappings = publish_mappings(&args)?;

    // Parse the volume mappings (HOST:GUEST[:ro]) and refuse a mount point
    // inside a read-only map that does not have it. Done before the kernel,
    // the rootfs and the image are looked up and before the snapshot-cache
    // branch, so every run is checked, cached or not, and a refusal sets up
    // nothing.
    let volume_mappings = types::checked_volume_mappings(&args, attaches_image_disk(&args))?;

    // Disallow --setup when running as root
    // Root users should run `fcvm setup` explicitly
    if args.setup && nix::unistd::geteuid().is_root() {
        bail!("--setup is not allowed when running as root. Run 'fcvm setup' first.");
    }

    // Validate --user format and resolve username from /etc/passwd (matching podman behavior).
    // Accepts "uid:gid" (numeric) - username is looked up from host passwd, just like
    // podman --userns=keep-id resolves the username from the container's passwd.
    if let Some(ref user) = args.user {
        let parts: Vec<&str> = user.split(':').collect();
        if parts.len() != 2 {
            bail!("invalid --user format '{}': expected 'uid:gid'", user);
        }
        let uid: u32 = parts[0]
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid --user uid '{}': must be numeric", parts[0]))?;
        parts[1]
            .parse::<u32>()
            .map_err(|_| anyhow::anyhow!("invalid --user gid '{}': must be numeric", parts[1]))?;

        // Resolve username from host /etc/passwd (like podman does with keep-id)
        if !args.env.iter().any(|e| e.starts_with("USER=")) {
            let username = resolve_username(uid);
            args.env.push(format!("USER={}", username));
        }
    }

    // Resolve 0 → host values for cpu and mem
    if args.cpu == 0 {
        // Firecracker allows max 32 vCPUs (and must be 1 or even)
        let host_cpus = std::thread::available_parallelism()
            .map(|n| n.get().min(32) as u8)
            .unwrap_or(2);
        args.cpu = host_cpus;
        info!("Using host CPUs (capped at 32): {}", args.cpu);
    }
    info!("VM memory: {} MiB", args.mem);

    // Build RuntimeConfig from kernel profile (replaces env var config passing)
    // Uses explicit --kernel-profile if given, otherwise falls back to "default" profile
    // for [firecracker] config (custom Firecracker binary from rootfs-config.toml).
    let effective_profile_name = args.kernel_profile.as_deref().unwrap_or("default");
    let profile = crate::setup::get_kernel_profile(effective_profile_name)?;
    if profile.is_some() && args.kernel_profile.is_some() {
        info!(profile = %effective_profile_name, "using kernel profile");
    }
    let default_profile = if effective_profile_name == "default" {
        None
    } else {
        crate::setup::get_kernel_profile("default")?
    };
    let mut runtime_config = runtime_config_from_kernel_profiles(
        effective_profile_name,
        profile,
        default_profile,
        |profile, name| async move {
            crate::setup::get_configured_firecracker_for_profile(&profile, &name).await
        },
    )
    .await?;

    // Get kernel path
    // Priority: --kernel (explicit) > profile (named or "default")
    let kernel_profile_name = args.kernel_profile.as_deref().unwrap_or("default");
    let kernel_path = if let Some(custom_kernel) = &args.kernel {
        // Explicit kernel path - use directly
        let path = PathBuf::from(custom_kernel);
        if !path.exists() {
            bail!("Custom kernel not found: {}", path.display());
        }
        info!(kernel = %path.display(), "using custom kernel");
        path
    } else {
        // Profile kernel (named or "default")
        crate::setup::ensure_kernel(kernel_profile_name, args.setup, false)
            .await
            .context("setting up kernel")?
    };
    // A profile with an NVIDIA driver image attaches it to every cold boot, GPU or
    // not, so its snapshots carry the mounted driver and any restore can take a GPU.
    // The modules are built against the profile's kernel, so --kernel gets none.
    if args.kernel.is_none() {
        runtime_config.gpu_driver_image =
            crate::setup::profile_nvidia_driver_image(kernel_profile_name, args.setup)
                .await
                .context("setting up the NVIDIA driver image")?
                .map(crate::commands::common::GpuDriverImage::at)
                .transpose()?;
    }

    // Resolve rootfs type: CLI override > kernel profile config > default (ext4)
    let rootfs_type = resolve_rootfs_type(&args);

    // Disk-only clone: cold-boot from the captured disk instead of the
    // content-addressed base rootfs. create_cow_disk reflinks whatever it's given.
    let base_rootfs = match &args.rootfs_override {
        Some(p) => p.clone(),
        None => crate::setup::ensure_rootfs(args.setup, rootfs_type.as_deref())
            .await
            .context("setting up rootfs")?,
    };
    let initrd_path = crate::setup::ensure_fc_agent_initrd(args.setup)
        .await
        .context("setting up fc-agent initrd")?;

    // Parse optional container command EARLY - it's part of cache key
    // Either from trailing args or --cmd flag
    let cmd_args = if !args.command_args.is_empty() {
        // Trailing args take precedence (e.g., "alpine:latest sh -c 'echo hello'")
        Some(args.command_args.clone())
    } else if let Some(cmd) = &args.cmd {
        // Fall back to --cmd flag with shell parsing
        Some(shell_words::split(cmd).with_context(|| format!("parsing --cmd argument: {}", cmd))?)
    } else {
        None
    };

    // Resolve the cache key (manifest digest for localhost/, reference for remote) AND
    // the immutable export id in ONE inspect, so they are an atomic view of the same
    // image: a parallel build that repoints the tag can't make the cache key name one
    // image while the export pins another (#598). For remote images image_id is None.
    // A disk-only clone has the image baked into the captured rootfs, so it must
    // NOT require the original host image tag to still exist (or re-export it).
    let (image_identifier, localhost_image_id) = if args.rootfs_override.is_some() {
        (args.image.clone(), None)
    } else {
        let image_ref = get_image_cache_ref(&args.image).await?;
        (image_ref.cache_key, image_ref.image_id)
    };

    // Resolve the image-delivery disk BEFORE computing the snapshot key. The
    // pre-start snapshot provisions the container against the layer link IDs
    // inside one specific build of the storage image, and `podman load`
    // randomizes those IDs on every build — so a rebuilt storage image
    // invalidates every snapshot provisioned against its predecessor. The
    // disk's file identity is therefore part of the snapshot key (2026-08-13:
    // a cached pre-start snapshot paired with a rebuilt storage image failed
    // every localhost run with "readlink .../overlay/l/<id>: no such file or
    // directory").
    //
    // A disk-only clone never attaches an image device — the image already
    // lives in the captured container storage on the reflinked rootfs — so
    // skip export (and don't require the original host image tag to still
    // exist). None for registry-pulled images.
    let image_disk_path: Option<PathBuf> = if exports_localhost_image(&args) {
        let resolved_mode = resolve_image_mode(&args);
        let expected = expected_image_disk_path(&image_identifier, resolved_mode);
        if resolved_mode == crate::firecracker::ImageMode::Overlay && expected.exists() {
            // Warm-cache fast path: stat-only, preserves clone hot-path
            // latency. Non-overlay modes attach the archive itself and keep
            // the full path for its validation/re-export logic.
            info!(image = %args.image, digest = %image_identifier, "Using cached overlay storage image");
            Some(expected)
        } else {
            Some(
                export_localhost_image_disk(
                    &args,
                    &image_identifier,
                    localhost_image_id.as_deref(),
                )
                .await?,
            )
        }
    } else {
        args.image_disk_override.clone()
    };

    // Build identity (SHA-256, `utils::image_disk_digest`) of the exact file that
    // will be attached; a rebuild at the same path is a different identity. Overlay
    // storage images ONLY: btrfs/archive modes import the archive into the
    // guest before the pre-start snapshot, so a restored VM never re-reads the
    // archive — keying those on identity would force a needless cold boot on
    // every re-export without changing restored behavior.
    let image_disk_identity = image_disk_path
        .as_deref()
        .filter(|p| crate::utils::is_overlay_storage_image(p))
        .map(|p| {
            crate::utils::image_disk_digest(p)
                .with_context(|| format!("hash image disk {} for snapshot key", p.display()))
        })
        .transpose()?;

    // Copy each image into the pmem store, or find its generation there, once. This comes
    // after the arguments are checked and the kernel, rootfs, initrd and container image
    // are found, so a run refused by any of those copies nothing. The snapshot key and the
    // attach both read the resulting entry specs, so they name the same generation, and no
    // VM maps the file the user named.
    if !args.pmem.is_empty() {
        let data_dir = paths::data_dir();
        args = tokio::task::spawn_blocking(move || {
            resolve_pmem_into_store(&mut args, &data_dir).map(|()| args)
        })
        .await
        .context("resolving --pmem images")??;
    }

    // Check for snapshot cache (unless the invocation opts out — see
    // snapshot_cache_opt_out for the full list and rationale).
    // Keep fc_config and snapshot_key available for later snapshot creation on miss
    let no_snapshot = snapshot_cache_opt_out(
        &args,
        std::env::var("FCVM_NO_SNAPSHOT")
            .map(|v| !v.is_empty())
            .unwrap_or(false),
    );
    if !args.gpu.is_empty() {
        validate_gpu_run(&args, &lifecycle, no_snapshot)?;
        args.gpu = crate::gpu::canonical_sbdfs(&args.gpu)?;
        crate::gpu::require_pci_transport(runtime_config.effective_firecracker_args().as_deref())?;
        let profile_name = args.kernel_profile.as_deref().unwrap_or("default");
        let module_type = crate::setup::profile_module_type(profile_name)?;
        for sbdf in &args.gpu {
            // The lease is released at once: this VM cold-boots without the GPU,
            // and the restore that attaches it takes its own lease and checks again.
            drop(crate::gpu::check_host_device(
                Path::new("/sys"),
                Path::new("/dev"),
                &crate::gpu::scrub::state_dir(),
                sbdf,
            )?);
            crate::gpu::check_driver_supports_device(
                Path::new("/sys"),
                sbdf,
                profile_name,
                module_type,
            )?;
        }
    }
    // Resolved ONCE for the whole launch. The same values feed the snapshot
    // key (via fc_config or the fallback launch config) and the bridged
    // network's guest DNS below, so the hashed value and the guest-visible
    // value cannot diverge on a mid-launch resolv.conf change (#863).
    let boot_inputs = GuestBootInputs::resolve(args.dns.as_deref(), &runtime_config);

    // What VMs of processes killed without teardown left on the host goes first.
    crate::reclaim::reclaim_abandoned().await;
    // Generated before the snapshot key so the staging path of a read-only --disk-dir
    // is in this VM's data directory, which teardown removes.
    let vm_id = generate_vm_id();
    let vm_name = args.name.clone();
    // The data directory outlives this process unless teardown removes it, so it is
    // recorded before anything creates it, and a SIGKILLed run leaves it to the next
    // launch to reclaim (crate::reclaim).
    let reclaim_owner = crate::reclaim::Owner::begin(&vm_id)?;
    reclaim_owner.record(crate::reclaim::Resource::RuntimeDir {
        path: paths::vm_runtime_dir(&vm_id),
    })?;

    // Resolve the extra disks for the snapshot key ONCE: the key carries each read-only
    // --disk-dir's content hash, taken in place, so a cache hit copies nothing. A cold
    // boot copies the directory into this VM's data directory, checks the copy has that
    // hash and builds the image from it, so a directory changing during the launch
    // cannot put new content under an old key. A read-only --disk carries its image's
    // file identity. Without the snapshot cache nothing is keyed or staged.
    let mut disk_dir_staging = None;
    if !no_snapshot && (!args.disk.is_empty() || !args.disk_dir.is_empty()) {
        let root = paths::vm_runtime_dir(&vm_id).join("disk-dir-staging");
        let (disk, disk_dir) = (args.disk.clone(), args.disk_dir.clone());
        let (staging, resolved) = tokio::task::spawn_blocking(move || {
            crate::storage::disk_dir::DiskDirStaging::resolve(&root, &disk, &disk_dir)
        })
        .await
        .context("resolving --disk and --disk-dir for the snapshot key")??;
        args.resolved_extra_disks = Some(resolved);
        disk_dir_staging = Some(staging);
    }

    let (fc_config, snapshot_key, startup_snapshot_base_key, prepare_target): (
        Option<crate::firecracker::FirecrackerConfig>,
        Option<String>,
        Option<String>,
        Option<PreparedTarget>,
    ) = if !no_snapshot {
        let resolved_mode = resolve_image_mode(&args);
        let config = build_firecracker_config(
            &args,
            &image_identifier,
            &kernel_path,
            &base_rootfs,
            &initrd_path,
            cmd_args.clone(),
            resolved_mode,
            runtime_config.firecracker_bin.as_deref(),
            runtime_config.gpu_driver_image.as_ref(),
            image_disk_identity.clone(),
            vm_config::effective_extra_boot_args(&runtime_config),
            boot_inputs.clone(),
            &volume_mappings,
        );
        let key = config.snapshot_key();
        // The startup snapshot's name also holds what decides when the run is Healthy
        // (the health-check URL); the pre-start key does not (#953).
        let startup_base = config.startup_snapshot_base_key();

        // Check if cached snapshot exists - prefer startup snapshot over pre-start snapshot.
        // The startup snapshot is this run's balloon target's own; the pre-start one is
        // shared between targets.
        let startup_key =
            startup_snapshot_key(&startup_base, args.balloon, run_host_ipv6(&args.env));

        let mut prepare_target = None;
        if let PodmanLifecycle::Prepare(options) = &lifecycle {
            let mut target = prepare_install_target(options, &startup_key)?;
            target.guest_thp = args.guest_thp.clone();
            if !target.publish_installed {
                info!(
                    snapshot_key = %target.name,
                    content_key = %target.content_key,
                    "Rebuilding prepared startup snapshot (--force)"
                );
            } else if let Some(prepared) =
                verify_prepared_snapshot(&target, PreparedCache::Hit).await?
            {
                info!(
                    snapshot_key = %target.name,
                    generation_id = %prepared.output.generation_id,
                    "Prepared startup snapshot hit"
                );
                complete_unused_owner(reclaim_owner).await;
                return Ok(VmPreparation::Prepared(prepared));
            }
            prepare_target = Some(target);
        }

        // Check for startup snapshot first (fully initialized application)
        // A --gpu run never restores a startup snapshot: its container started without the
        // GPU. It restores the pre-start snapshot, which is taken before the container runs.
        if !lifecycle.is_prepare()
            && args.gpu.is_empty()
            && run_takes_startup_snapshot(&args)
            && check_podman_snapshot(&startup_key).await.is_some()
        {
            info!(
                snapshot_key = %startup_key,
                image = %args.image,
                "Startup snapshot hit! Restoring from fully-initialized snapshot"
            );
            // Call snapshot run directly with startup snapshot
            // No need to create startup snapshot again since we're restoring from one
            let snapshot_args = snapshot_restore_args(
                &args,
                &startup_key,
                None, // Already using startup snapshot
                snapshot_run_firecracker_overrides(&runtime_config),
            );
            let attempt = super::snapshot::cmd_snapshot_run_attempt(snapshot_args).await;
            match attempt.result {
                Ok(()) => {
                    complete_unused_owner(reclaim_owner).await;
                    return Ok(VmPreparation::RunCompleted);
                }
                Err(e) => match classify_restore_failure(&e) {
                    RestoreFault::UnusableSnapshot => {
                        invalidate_unusable_snapshot(&startup_key, attempt.generation.as_ref(), &e)
                            .await;
                        // fall through to the pre-start check / fresh boot
                    }
                    RestoreFault::LoadFailed => keep_snapshot_after_load_fault(&startup_key, &e),
                    RestoreFault::Other => return Err(e),
                },
            }
        }

        // Check for pre-start snapshot (container loaded but not initialized)
        if restores_pre_start(&lifecycle) && check_podman_snapshot(&key).await.is_some() {
            info!(
                snapshot_key = %key,
                image = %args.image,
                startup_snapshot_key = %startup_key,
                "Pre-start snapshot hit! Restoring from cached snapshot"
            );
            // Call snapshot run with startup snapshot creation enabled
            // (see should_arm_startup_snapshot). A prepare installs its own startup
            // snapshot at its target instead.
            let mut snapshot_args = snapshot_restore_args(
                &args,
                &key,
                (!lifecycle.is_prepare() && run_takes_startup_snapshot(&args))
                    .then(|| startup_base.clone()),
                snapshot_run_firecracker_overrides(&runtime_config),
            );
            if let PodmanLifecycle::Prepare(options) = &lifecycle {
                let target = prepare_target
                    .as_mut()
                    .context("prepare lifecycle has no snapshot install target")?;
                // The startup snapshot of a restored VM is a diff on this pre-start snapshot.
                target.free_at_start = check_snapshot_space_before_boot(
                    "podman prepare's startup snapshot",
                    args.mem,
                    true,
                    Some(target),
                )
                .await?;
                snapshot_args.prepare = Some(PreparedRestore {
                    target: target.clone(),
                    ready_timeout: options.ready_timeout,
                });
            }
            let attempt = super::snapshot::cmd_snapshot_run_attempt(snapshot_args).await;
            match attempt.result {
                Ok(()) => {
                    complete_unused_owner(reclaim_owner).await;
                    if lifecycle.is_prepare() {
                        return Ok(VmPreparation::Prepared(attempt.prepared.context(
                            "the restored prepare VM finished without installing its snapshot",
                        )?));
                    }
                    return Ok(VmPreparation::RunCompleted);
                }
                Err(e) => match classify_restore_failure(&e) {
                    RestoreFault::UnusableSnapshot => {
                        invalidate_unusable_snapshot(&key, attempt.generation.as_ref(), &e).await;
                        // fall through to a fresh boot (which re-creates the snapshot)
                    }
                    RestoreFault::LoadFailed => keep_snapshot_after_load_fault(&key, &e),
                    RestoreFault::Other => return Err(e),
                },
            }
        }

        info!(
            snapshot_key = %key,
            image = %args.image,
            startup_snapshot_key = %startup_key,
            "Snapshot miss, will create snapshot after image load"
        );
        failpoint::hit_async("cache.miss").await;
        // A cold boot's first snapshot is a full one. A prepare fails without its startup
        // snapshot and a --gpu run without its pre-start snapshot, so both are refused here
        // when it cannot fit. Any other run only skips caching when its snapshot is refused.
        if let Some(target) = prepare_target.as_mut() {
            target.free_at_start = check_snapshot_space_before_boot(
                "podman prepare's startup snapshot",
                args.mem,
                false,
                Some(target),
            )
            .await?;
        } else if !args.gpu.is_empty() {
            check_snapshot_space_before_boot(
                "the pre-start snapshot that --gpu restores with its devices attached",
                args.mem,
                false,
                None,
            )
            .await?;
        }
        // The cold boot builds each read-only --disk-dir image from a private copy, and
        // refuses one whose content is no longer what the key names.
        on_disk_dir_staging(&mut disk_dir_staging, |s| s.stage()).await?;
        (Some(config), Some(key), Some(startup_base), prepare_target)
    } else {
        if std::env::var("FCVM_NO_SNAPSHOT")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            info!("Snapshot disabled via FCVM_NO_SNAPSHOT environment variable");
        } else {
            info!("Snapshot disabled via --no-snapshot flag");
        }
        (None, None, None, None)
    };

    if !volume_mappings.is_empty() {
        info!(
            "Volumes to mount: {}",
            volume_mappings
                .iter()
                .map(|v| format!(
                    "{}:{}{}",
                    v.host_path.display(),
                    v.guest_path,
                    if v.read_only { ":ro" } else { "" }
                ))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Setup paths
    //
    // Checked before anything is created: the VM's Unix sockets live in this
    // directory, and a directory a few characters too deep pushes them past
    // sun_path. bind() then fails with ENAMETOOLONG and surfaces far away from
    // the cause (observed as "VolumeServer failed to signal ready: channel
    // closed"), so refuse it here where the path can still be named.
    paths::check_socket_path_budget(&vm_id)?;
    let data_dir = paths::vm_runtime_dir(&vm_id);
    tokio::fs::create_dir_all(&data_dir)
        .await
        .context("creating VM data directory")?;

    // All image modes use the cache path directly (read-only):
    // - Overlay: ext4 storage image as additionalImageStore
    // - Btrfs: Docker archive (fc-agent creates btrfs loopback on rootfs)
    // - Archive: Docker archive for podman load

    let socket_path = data_dir.join("firecracker.sock");

    // Create VM state
    // Note: env vars are NOT stored in state (they may contain secrets and state is world-readable)
    // Instead, env is passed directly to MMDS at VM start time
    let mut vm_state = VmState::new(vm_id.clone(), args.image.clone(), args.cpu, args.mem);
    vm_state.name = Some(vm_name.clone());
    vm_state.config.volumes = args.map.clone();
    vm_state.config.health_check_url = args.health_check.clone();
    vm_state.config.health_check_timeout = args.health_check_timeout;
    vm_state.config.restore_hook = args.on_restore.clone();
    vm_state.config.guest_thp = args.guest_thp.clone();
    // This boot's fc-agent is the one that ships with this fcvm, which applies the mode.
    vm_state.config.guest_thp_restore = true;
    vm_state.config.dns_override = Some(args.dns.clone());
    vm_state.config.hugepages = args.hugepages;
    vm_state.config.balloon_mib = args.balloon;
    vm_state.config.balloon_free_page_reporting = args.free_page_reporting;
    vm_state.config.portable_volumes = args.portable_volumes;
    vm_state.config.port_mappings = port_mappings.clone();
    vm_state.config.forward_localhost = args.forward_localhost.clone();
    vm_state.config.network_mode = args.network.into();
    vm_state.config.hypervisor = args.hypervisor.into();
    vm_state.config.ipv6_prefix = launch_ipv6_prefix(&args);
    vm_state.config.tty = args.tty;
    vm_state.config.interactive = args.interactive;
    vm_state.config.user = args.user.clone();
    // Recorded so snapshots of this VM carry what a cold-boot clone / reboot plan
    // needs: the kernel profile (a btrfs-profile disk needs a btrfs kernel) and the
    // image device (overlay/archive image layers live on a separate read-only disk).
    vm_state.config.kernel_profile = args.kernel_profile.clone();
    vm_state.config.image_disk_path = image_disk_path.clone();
    vm_state.config.image_disk_identity = image_disk_identity.clone();
    vm_state.config.image_mode = image_disk_path
        .as_ref()
        .map(|_| resolve_image_mode(&args).to_string());
    // Store the username for health checks (runuser -u <username>).
    // USER env var was resolved from host /etc/passwd above (or explicitly passed).
    if args.user.is_some() {
        vm_state.config.username = args
            .env
            .iter()
            .find_map(|s| s.strip_prefix("USER="))
            .map(|s| s.to_string());
    }
    vm_state.config.labels = parse_labels(&args.label);

    // Initialize state manager
    let state_manager = StateManager::new(paths::state_dir());
    state_manager.init().await?;

    // Setup networking based on mode
    // Bridged mode requires root for iptables and network namespace setup
    if matches!(args.network, NetworkMode::Bridged | NetworkMode::Routed)
        && !nix::unistd::geteuid().is_root()
    {
        bail!(
            "Bridged/routed networking requires root. Either:\n  \
             - Run with sudo: sudo fcvm podman run ...\n  \
             - Use rootless mode: fcvm podman run --network rootless ..."
        );
    }
    // Rootless with sudo is pointless - bridged would be faster
    if matches!(args.network, NetworkMode::Rootless) && nix::unistd::geteuid().is_root() {
        warn!(
            "Running rootless mode as root is unnecessary. \
             Consider using --network bridged or --network routed for better performance."
        );
    }

    // The VM's first state is written here, under its name, and refused when a live
    // VM already has that name. Every error from here on removes the state again.
    if let Err(e) = state_manager
        .claim_name(&mut vm_state)
        .await
        .context("claiming the VM name")
    {
        cleanup_failed_prepare(&state_manager, &vm_id, &data_dir).await;
        return Err(e);
    }

    // Bridged and routed re-derive this from the namespace they reserve (see
    // network::names); read the settled name back from network_config.
    let tap_device = format!("tap-{}", truncate_id(&vm_id, 8));
    let network: Result<Box<dyn NetworkManager>> = async {
        let network: Box<dyn NetworkManager> = match args.network {
            NetworkMode::Bridged => {
                // The guest's resolver is the first hashed host_dns entry,
                // threaded from the same resolution the snapshot key saw instead
                // of re-read inside setup() (#863). Fail closed like the old
                // in-setup read did: a bridged guest NATs straight out and needs
                // a real resolver, and --dns is the override.
                if args.dns.is_none() && boot_inputs.host_dns().is_empty() {
                    bail!(
                        "no usable host DNS server for bridged mode: neither {} named a \
                     nameserver this guest can reach (the warning above says what each \
                     source held). Pass --dns to name one.",
                        crate::network::RESOLV_CONF_SOURCES.join(" nor ")
                    );
                }
                Box::new(
                    BridgedNetwork::new(vm_id.clone(), tap_device.clone(), port_mappings.clone())
                        .with_dns_server(boot_inputs.host_dns().first().cloned()),
                )
            }
            NetworkMode::Routed => {
                let mut net =
                    RoutedNetwork::new(vm_id.clone(), tap_device.clone(), port_mappings.clone())
                        .with_ipv6_address(args.ipv6_address);
                if let Some(ref prefix) = args.ipv6_prefix {
                    net = net.with_ipv6_prefix(prefix.clone());
                }
                // The IPv6 resolver comes from the launch's single resolv.conf
                // snapshot, the same one the snapshot key and the guest's search
                // domains are derived from. Letting routed read the host again
                // would give the guest one resolver while its search list and its
                // key describe another.
                net = net.with_ipv6_dns(crate::network::first_ipv6_nameserver(&boot_inputs.dns));
                if !args.forward_localhost.is_empty() {
                    net = net.with_forward_localhost(args.forward_localhost.clone());
                }
                net.preflight_check()
                    .context("routed mode preflight check failed")?;
                if net.needs_loopback_ip() {
                    let loopback_ip = state_manager
                        .allocate_loopback_ip(&mut vm_state)
                        .await
                        .context("allocating loopback IP for routed mode")?;
                    net = net.with_loopback_ip(loopback_ip);
                }
                Box::new(net)
            }
            NetworkMode::Rootless => {
                // For rootless mode, allocate loopback IP atomically with state persistence
                // This prevents race conditions when starting multiple VMs concurrently
                let loopback_ip = state_manager
                    .allocate_loopback_ip(&mut vm_state)
                    .await
                    .context("allocating loopback IP")?;

                // --dns reaches pasta as well as the guest: naming the pasta
                // gateway as the resolver means "the service on host
                // 127.0.0.1:53", and pasta must be told not to claim port 53 for
                // the host's own resolver instead (see resolver_is_pasta_gateway).
                Box::new(
                    PastaNetwork::new(vm_id.clone(), tap_device.clone(), port_mappings.clone())
                        .with_loopback_ip(loopback_ip)
                        .with_dns_server(args.dns.clone()),
                )
            }
        };
        Ok(network)
    }
    .await;
    let mut network = match network {
        Ok(network) => network,
        Err(e) => {
            cleanup_failed_prepare(&state_manager, &vm_id, &data_dir).await;
            return Err(e);
        }
    };

    // network.setup() may fail partway through (it tears nothing down itself),
    // so any error from here until the run_vm_setup error handler below must
    // run network.cleanup() to remove partially-created host network state.
    let mut network_config = match network.setup().await.context("setting up network") {
        Ok(config) => config,
        Err(e) => {
            if let Err(cleanup_err) = network.cleanup().await {
                warn!(
                    "failed to cleanup network after setup error: {}",
                    cleanup_err
                );
            }
            cleanup_failed_prepare(&state_manager, &vm_id, &data_dir).await;
            return Err(e);
        }
    };

    // --dns overrides the mode's DNS choice. fc-agent writes it to the
    // guest's resolv.conf at boot, so a snapshot taken from this VM bakes it
    // in; restored clones resolve through it with no further configuration.
    if let Some(ref dns) = args.dns {
        network_config.dns_server = Some(dns.clone());
    }

    info!(tap = %network_config.tap_device, mac = %network_config.guest_mac, "network configured");

    // Generate vsock socket base path for volume servers
    // Firecracker binds to vsock.sock, VolumeServers listen on vsock.sock_{port}
    // Use custom vsock_dir if provided (for predictable socket paths)
    let vsock_socket_path = if let Some(ref vsock_dir) = args.vsock_dir {
        let resolved: Result<PathBuf> = async {
            let configured = PathBuf::from(vsock_dir);
            let current_dir =
                std::env::current_dir().context("resolving current directory for --vsock-dir")?;
            let socket_path = resolve_custom_vsock_socket_path(&configured, &current_dir);
            let absolute_dir = socket_path
                .parent()
                .expect("a custom vsock socket path always has a parent");
            tokio::fs::create_dir_all(absolute_dir)
                .await
                .with_context(|| format!("creating vsock dir: {:?}", absolute_dir))?;
            Ok(socket_path)
        }
        .await;
        match resolved {
            Ok(path) => path,
            Err(e) => {
                if let Err(cleanup_err) = network.cleanup().await {
                    warn!(
                        "failed to cleanup network after setup error: {}",
                        cleanup_err
                    );
                }
                cleanup_failed_prepare(&state_manager, &vm_id, &data_dir).await;
                return Err(e);
            }
        }
    } else {
        data_dir.join("vsock.sock")
    };
    // Snapshot control must connect to the socket the VMM actually bound. In
    // particular, `--vsock-dir` deliberately places it outside `data_dir`, so
    // reconstructing the path from vm_id would target the wrong socket.
    vm_state.config.vsock_socket_path = Some(vsock_socket_path.clone());
    // A cold-boot VMM embeds the same exact path it binds. A restored clone
    // records the path its VMM state names: its own listener when the load
    // overrides the vsock path, else the snapshot's embedded path.
    vm_state.config.source_vsock_socket_path = Some(vsock_socket_path.clone());

    // Build VolumeConfigs and spawn VolumeServers BEFORE the VM starts
    // Each VolumeServer listens on vsock.sock_{port} (e.g., vsock.sock_5000)
    // Firecracker binds to vsock.sock and routes guest connections to the per-port sockets
    let mount_paths = types::guest_mount_paths(&args, &volume_mappings, attaches_image_disk(&args));
    let volume_configs: Vec<VolumeConfig> = volume_mappings
        .iter()
        .enumerate()
        .map(|(idx, vol)| VolumeConfig {
            host_path: vol.host_path.clone(),
            guest_path: vol.guest_path.clone().into(),
            read_only: vol.read_only,
            port: VSOCK_VOLUME_PORT_BASE + idx as u32,
            portable: args.portable_volumes,
            mount_points: crate::volume::mount_points_inside(
                std::path::Path::new(&vol.guest_path),
                mount_paths.iter().map(std::path::PathBuf::as_path),
            ),
        })
        .collect();

    let volume_servers = match spawn_volume_servers(&volume_configs, &vsock_socket_path)
        .await
        .context("spawning VolumeServers")
    {
        Ok(servers) => servers,
        Err(e) => {
            if let Err(cleanup_err) = network.cleanup().await {
                warn!(
                    "failed to cleanup network after setup error: {}",
                    cleanup_err
                );
            }
            cleanup_failed_prepare(&state_manager, &vm_id, &data_dir).await;
            return Err(e);
        }
    };

    // Create snapshot channel for snapshot-ready notifications
    // Skip snapshot creation when:
    // - --no-snapshot flag or FCVM_NO_SNAPSHOT env var is set
    // Note: FUSE volumes survive snapshot/restore — fc-agent remounts them on clone restore
    let skip_snapshot_creation = no_snapshot;
    // This is a cold boot. With --free-page-reporting it owes a check that the
    // guest brought its balloon up, before any snapshot of the VM is taken.
    let balloon_check_due = args.free_page_reporting;
    let forward_ask = ask_reaches_run_loop(skip_snapshot_creation, balloon_check_due, &lifecycle);
    let (cache_tx, cache_rx): (
        Option<mpsc::Sender<CacheRequest>>,
        Option<mpsc::Receiver<CacheRequest>>,
    ) = if forward_ask {
        let (tx, rx) = mpsc::channel(1);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    // What this process knows the guest to be, for answering (re-)asked
    // "cache-ready" messages. With snapshots disabled no boundary can sever
    // the handshake and no snapshot decision exists, so the verdict is Continue
    // from the start, unless the run loop has the balloon check to run at the
    // guest's first ask. Otherwise it is Pending until the run loop decides.
    let cache_verdict = shared_cache_verdict(if skip_snapshot_creation && !forward_ask {
        CacheVerdict::Continue
    } else {
        CacheVerdict::Pending
    });

    // Create startup snapshot channel for health-triggered snapshot creation
    // (see should_arm_startup_snapshot).
    let (startup_tx, startup_rx): (
        Option<tokio::sync::oneshot::Sender<crate::health::StartupSnapshotAck>>,
        Option<tokio::sync::oneshot::Receiver<crate::health::StartupSnapshotAck>>,
    ) = if should_arm_startup_snapshot(
        skip_snapshot_creation,
        snapshot_key.is_some(),
        run_takes_startup_snapshot(&args),
        &lifecycle,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    // Start status channel listener for fc-agent notifications
    // - "ready" / "ready:healthcheck" on port 4999 -> podman reports the container
    //   running: creates the container-ready file the health monitor reads
    // - "exit:{code}" on port 4999 -> creates container-exit file with exit code
    // - "cache-ready:{digest}" on port 4999 -> trigger cache creation
    let status_socket_path = format!("{}_{}", vsock_socket_path.display(), VSOCK_STATUS_PORT);
    // Set by the status listener when the guest signals a reboot / a container
    // exit; consumed by run_vm_loop to decide relaunch-in-place vs terminate.
    let reboot_requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let container_exit_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Fired by the status listener when the container starts; wakes the health
    // monitor so it checks at once instead of at its next poll.
    let container_started = Arc::new(tokio::sync::Notify::new());
    let status_handle = {
        let runtime_dir = data_dir.clone();
        let socket_path = status_socket_path.clone();
        let vm_id_clone = vm_id.clone();
        let reboot_flag = reboot_requested.clone();
        let exit_flag = container_exit_seen.clone();
        let container_started = container_started.clone();
        let verdict = cache_verdict.clone();
        tokio::spawn(async move {
            if let Err(e) = run_status_listener(
                &socket_path,
                &runtime_dir,
                &vm_id_clone,
                cache_tx,
                verdict,
                reboot_flag,
                exit_flag,
                container_started,
            )
            .await
            {
                tracing::warn!("Status listener error: {}", e);
            }
        })
    };

    // Start I/O listener for container stdin/stdout/stderr
    // TTY mode: use binary exec_proto on port 4996 (blocking, raw terminal)
    // Non-TTY mode: use line-based protocol on port 4997 (async)
    let tty_mode = args.tty;
    let interactive = args.interactive;
    let tty_socket_path = format!("{}_{}", vsock_socket_path.display(), VSOCK_TTY_PORT);
    let output_socket_path = format!("{}_{}", vsock_socket_path.display(), VSOCK_OUTPUT_PORT);

    // For TTY mode, we spawn a blocking thread that handles the TTY I/O
    // This must be set up BEFORE VM starts so we're ready to accept connection
    let tty_handle = if tty_mode {
        let socket_path = tty_socket_path.clone();
        Some(std::thread::spawn(move || {
            super::tty::run_tty_session(&socket_path, true, interactive)
        }))
    } else {
        None
    };

    // Broadcast channel for container output (used by VmHandle::subscribe_logs())
    let (log_tx, _) = tokio::sync::broadcast::channel::<LogLine>(1000);

    // `podman prepare` forwards none of the container's output, so keep it in the VM's directory;
    // teardown stops the writer, waits for it, and copies the file out (listeners::CONTAINER_LOG).
    // Subscribed before the output listener starts, so no line is sent before there is a receiver.
    let container_log = lifecycle
        .is_prepare()
        .then(|| listeners::ContainerLogWriter::spawn(log_tx.subscribe(), &data_dir));

    // For non-TTY mode, use async output listener
    let output_reconnect = Arc::new(tokio::sync::Notify::new());
    let output_handle = if !tty_mode {
        let socket_path = output_socket_path.clone();
        let vm_id_clone = vm_id.clone();
        let log_tx_clone = Some(log_tx.clone());
        let reconnect = output_reconnect.clone();
        let non_blocking_output = args.non_blocking_output;
        let drain = container_log
            .as_ref()
            .map(|writer| writer.listener_drain.clone());
        Some(tokio::spawn(async move {
            match run_output_listener(
                &socket_path,
                &vm_id_clone,
                log_tx_clone,
                reconnect,
                non_blocking_output,
                !lifecycle.is_prepare(),
                None,
                drain,
            )
            .await
            {
                Ok(()) => {}
                Err(e) => {
                    tracing::warn!("Output listener error: {}", e);
                }
            }
        }))
    } else {
        None
    };

    // Start egress proxy for rootless mode only.
    // Routed mode uses native IPv6 kernel routing — no proxy needed.
    // Services use mutual TLS with client certs, not source IP matching.
    let egress_proxy_handle = if matches!(args.network, NetworkMode::Rootless) {
        let socket_path = vsock_socket_path.clone();
        Some(tokio::spawn(async move {
            if let Err(e) = crate::network::egress_proxy::run_egress_proxy(&socket_path).await {
                tracing::warn!("Egress proxy error: {}", e);
            }
        }))
    } else {
        None
    };

    // Run the main VM setup in a helper to ensure cleanup on error
    let setup_result = run_vm_setup(
        VmSetupParams {
            args: &args,
            vm_id: &vm_id,
            data_dir: &data_dir,
            base_rootfs: &base_rootfs,
            socket_path: &socket_path,
            kernel_path: &kernel_path,
            initrd_path: &initrd_path,
            network_config: &network_config,
            cmd_args,
            volume_mappings: &volume_mappings,
            vsock_socket_path: &vsock_socket_path,
            image_disk_path: image_disk_path.as_deref(),
            fc_config,
            runtime_config: &runtime_config,
            boot_inputs,
        },
        network.as_mut(),
        &state_manager,
        &mut vm_state,
    )
    .await;

    // The disk-dir images are built, and a reboot relaunch re-attaches them, so the
    // staged copies are no longer needed. A failed removal leaves them to teardown,
    // which removes the VM's data directory.
    if let Err(error) = on_disk_dir_staging(&mut disk_dir_staging, |s| {
        s.remove();
        Ok(())
    })
    .await
    {
        warn!(%error, "removing --disk-dir staging after boot");
    }

    // If setup failed, cleanup all resources before propagating error
    if let Err(e) = setup_result {
        warn!("VM setup failed, cleaning up resources");

        // Abort VolumeServer tasks
        for handle in volume_servers.handles {
            handle.abort();
        }

        // Abort status listener
        status_handle.abort();

        // A prepare drains its output listener and joins its writer, so the lines the guest sent
        // are kept and nothing writes while cleanup_failed_prepare removes the directory.
        if let Some(writer) = container_log {
            if let Err(error) = writer
                .finish_after_listener(output_handle, listeners::OUTPUT_DRAIN_BOUND)
                .await
            {
                warn!("{error}");
            }
        } else if let Some(handle) = output_handle {
            handle.abort();
        }

        // Abort egress proxy if running
        if let Some(handle) = egress_proxy_handle {
            handle.abort();
        }

        // Cleanup network
        if let Err(cleanup_err) = network.cleanup().await {
            warn!(
                "failed to cleanup network after setup error: {}",
                cleanup_err
            );
        }

        // Remove the persisted state file and per-VM data directory
        cleanup_failed_prepare(&state_manager, &vm_id, &data_dir).await;
        return Err(e);
    }

    let (vm_manager, holder_child, reboot_spec, bootplan_handle) = setup_result.unwrap();

    info!(vm_id = %vm_id, "VM started successfully");

    // Create cancellation token for graceful health monitor shutdown
    let health_cancel_token = CancellationToken::new();

    // Spawn health monitor task with startup snapshot trigger support
    let health_monitor_handle = crate::health::spawn_health_monitor_full(
        vm_id.clone(),
        vm_state.pid,
        paths::state_dir(),
        Some(health_cancel_token.clone()),
        Some(container_started.clone()),
        startup_tx,
    );

    let disk_path = data_dir.join("disks/rootfs.raw");

    Ok(VmPreparation::Active(Box::new(VmContext {
        restore_from_cache: None,
        cache_verdict,
        vm_id,
        vm_name,
        data_dir,
        vm_manager,
        holder_child,
        bootplan_handle,
        volume_servers,
        network,
        network_config,
        state_manager,
        health_cancel_token,
        health_monitor_handle,
        status_handle,
        tty_handle,
        tty_socket_path: tty_mode.then_some(tty_socket_path),
        output_handle,
        container_log,
        egress_proxy_handle,
        cache_rx,
        startup_rx,
        balloon_check_due,
        snapshot_key,
        startup_snapshot_base_key,
        prepare_target,
        volume_configs,
        args,
        disk_path,
        log_tx,
        output_reconnect,
        vm_state,
        reboot_requested,
        container_exit_seen,
        reboot_spec,
        reclaim_owner,
    })))
}

/// Resolve a UID to a username via the host's /etc/passwd.
/// Falls back to "u{uid}" if the UID isn't found (matching podman's numeric fallback).
fn resolve_username(uid: u32) -> String {
    // Use nix::unistd::User which reads /etc/passwd
    match nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid)) {
        Ok(Some(user)) => user.name,
        _ => format!("u{}", uid),
    }
}

/// Join the TTY session thread after the VM has exited, with a bounded wait.
///
/// The TTY thread may still be blocked in `accept()` if the guest never connected
/// (boot or fc-agent failure before the TTY attach). Connect to the listener from the
/// host side to unblock it, then join via `spawn_blocking` with a timeout so a wedged
/// thread can never hang shutdown.
async fn join_tty_session(
    handle: std::thread::JoinHandle<Result<i32>>,
    tty_socket_path: Option<String>,
) -> Option<i32> {
    if let Some(socket_path) = tty_socket_path {
        let _ = std::os::unix::net::UnixStream::connect(&socket_path);
    }
    let join = tokio::task::spawn_blocking(move || {
        handle.join().ok().and_then(|session| {
            session
                .map_err(|error| warn!(error = %format!("{error:#}"), "TTY session ended without an exit code"))
                .ok()
        })
    });
    match tokio::time::timeout(std::time::Duration::from_secs(10), join).await {
        Ok(Ok(code)) => code,
        Ok(Err(e)) => {
            warn!(error = %e, "failed to join TTY session thread");
            None
        }
        Err(_) => {
            warn!("timed out waiting for TTY session thread");
            None
        }
    }
}

/// A container that ran to its end with a non-zero code. fcvm exits with that code
/// (`process_exit_status`), as `podman run` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerExited(pub i32);

impl std::fmt::Display for ContainerExited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "container exited with code {}", self.0)
    }
}

impl std::error::Error for ContainerExited {}

/// How every run that waited for its VM to exit reports the container: `fcvm podman run`
/// on a cold boot, a restored VM (a snapshot hit, or a miss that converges on the
/// restore), and a disk-only clone. `code` is what the guest reported on the status
/// channel or the TTY session; `None` means it reported nothing (boot failure, fc-agent
/// crash, a guest that powered off on its own), which is a failure, not a success.
pub(crate) fn container_exit_status(code: Option<i32>) -> Result<()> {
    match code {
        Some(0) => Ok(()),
        Some(code) => Err(ContainerExited(code).into()),
        None => bail!("VM exited without reporting a container exit code"),
    }
}

/// The exit status fcvm ends with for `error`: the container's own code for a
/// [`ContainerExited`] (1 when the code does not fit an exit status, so a failure can
/// never read as 0), and 1 for any other failure.
pub fn process_exit_status(error: &anyhow::Error) -> i32 {
    match error.downcast_ref::<ContainerExited>() {
        Some(ContainerExited(code)) if (1..=255).contains(code) => *code,
        _ => 1,
    }
}

/// Read the container exit code recorded by the status listener after VM exit.
///
/// The exit notification stays buffered in the host-side unix socket even after
/// Firecracker exits, but the listener task may not have processed it yet when the VM
/// exit is observed — wait on the exit-seen flag for a bounded window (the listener
/// itself stays alive to catch a racing reboot signal, so we can't join it).
///
/// Shared by both VM run loops (podman run and snapshot restore): the listener writes
/// `container-exit` under `data_dir` in both.
pub(crate) async fn read_container_exit_code(
    container_exit_seen: &std::sync::atomic::AtomicBool,
    data_dir: &Path,
) -> Option<i32> {
    use std::sync::atomic::Ordering;
    // wait_for_reboot_decision's drain handshake already proved the listener
    // processed everything the guest sent; the short wait here is only a backstop
    // for the listener-stuck case where the drain timed out.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while !container_exit_seen.load(Ordering::Acquire) {
        if tokio::time::Instant::now() >= deadline {
            warn!("no container exit notification after VM exit");
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let exit_file = data_dir.join("container-exit");
    std::fs::read_to_string(&exit_file)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
}

/// After the Firecracker child exits, decide whether it was a guest reboot (→ relaunch
/// in place) or a real termination (poweroff / container exit / crash → stop).
///
/// Every guest signal ("exit:", "reboot") is SENT before the firecracker reset/poweroff
/// (the reboot-notify unit runs Before=systemd-reboot.service; fc-agent sends "exit:"
/// before `poweroff -f`), so once `wait()` wakes us the messages are at worst buffered
/// in the host-side listener socket. Rather than guessing with timing windows, this
/// performs a positive drain handshake: connect to our own status socket and send
/// "drain". The listener processes connections strictly in accept order, so its
/// "drain-ack" proves every guest message sent before the exit has been handled — at
/// that point the reboot flag is authoritative. The 2s timeout is only a backstop for
/// a dead/stuck listener (in which case no more signals are coming anyway).
///
/// Shared by both VM run loops (podman run and snapshot restore).
pub(crate) async fn wait_for_reboot_decision(
    reboot_requested: &std::sync::atomic::AtomicBool,
    status_handle: &tokio::task::JoinHandle<()>,
    status_socket_path: &str,
) -> bool {
    use std::sync::atomic::Ordering;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    if reboot_requested.load(Ordering::Acquire) {
        return true;
    }
    if !status_handle.is_finished() {
        let drain = async {
            let mut stream = tokio::net::UnixStream::connect(status_socket_path)
                .await
                .ok()?;
            stream.write_all(b"drain\n").await.ok()?;
            let mut buf = [0u8; 16];
            let n = stream.read(&mut buf).await.ok()?;
            (n > 0).then_some(())
        };
        match tokio::time::timeout(std::time::Duration::from_secs(2), drain).await {
            Ok(Some(())) => {}
            _ => warn!("status listener did not ack drain probe; deciding on current flags"),
        }
    }
    reboot_requested.load(Ordering::Acquire)
}

/// How long the guest gets to power off after the container's exit status reached fcvm.
///
/// fc-agent sends the exit status and then calls `poweroff -f` (arm64) or `reboot -f` (x86); a
/// healthy guest takes well under a second from there (#990). A guest that wedges in shutdown used
/// to leave `fcvm podman run` waiting on Firecracker forever (#998). The bound is generous
/// because Firecracker also has to unmap a multi-GiB guest before the process exits.
///
/// The countdown starts within 100 ms of the container's exit even while the run loop is busy in
/// another arm's handler, such as a startup snapshot that takes minutes.
pub(crate) const GUEST_POWEROFF_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

/// Something the power-off watchdog can kill; the run loop's VMM is one.
pub(crate) trait Killable {
    fn start_kill(&mut self) -> Result<()>;
}

impl Killable for dyn crate::hypervisor::Hypervisor {
    fn start_kill(&mut self) -> Result<()> {
        crate::hypervisor::Hypervisor::start_kill(self)
    }
}

/// Bounds how long the run loop waits for a guest to power off after the container has exited
/// (#998). Fires once per guest lifetime: `rearm` starts a new one when the loop relaunches a
/// rebooted guest, whose container_exit_seen flag the loop has cleared.
/// The guest's power-off deadline and which lifecycle it belongs to. A tracker task carries the
/// generation it was started for; `rearm` bumps the generation under the same lock that guards the
/// deadline, so a retired tracker that is mid-write when it is aborted (`abort` does not stop a task
/// that is running, only one that reaches its next await) cannot leave a stale deadline behind.
#[derive(Default)]
struct DeadlineState {
    generation: u64,
    at: Option<tokio::time::Instant>,
}

impl DeadlineState {
    fn stamp_if_current(&mut self, generation: u64, at: tokio::time::Instant) {
        if self.generation == generation {
            self.at = Some(at);
        }
    }

    /// Start a new lifecycle: forget the deadline and return the generation its tracker must use.
    fn retire(&mut self) -> u64 {
        self.generation += 1;
        self.at = None;
        self.generation
    }
}

pub(crate) struct PoweroffWatchdog {
    fired: bool,
    /// When the guest must have powered off by: stamped by `tracker` within 100 ms of the container's
    /// exit, whether or not the run loop is polling `expired` at that moment. The loop can spend
    /// minutes inside another arm's handler (a startup snapshot), so a countdown started by the loop's
    /// next poll would begin late, and one held in the future `expired` returns would restart each
    /// time another select arm wins.
    deadline: Arc<std::sync::Mutex<DeadlineState>>,
    container_exit_seen: Arc<std::sync::atomic::AtomicBool>,
    deadline_after_exit: std::time::Duration,
    tracker: tokio::task::JoinHandle<()>,
}

/// Waits for the container's exit flag, stamps the deadline once, and ends. `rearm` starts a new one
/// for a relaunched guest, so nothing depends on observing the flag go false in between.
fn spawn_deadline_tracker(
    container_exit_seen: Arc<std::sync::atomic::AtomicBool>,
    deadline: Arc<std::sync::Mutex<DeadlineState>>,
    generation: u64,
    deadline_after_exit: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while !container_exit_seen.load(std::sync::atomic::Ordering::Acquire) {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        deadline.lock().unwrap().stamp_if_current(
            generation,
            tokio::time::Instant::now() + deadline_after_exit,
        );
    })
}

impl PoweroffWatchdog {
    pub(crate) fn new(
        container_exit_seen: Arc<std::sync::atomic::AtomicBool>,
        deadline_after_exit: std::time::Duration,
    ) -> Self {
        let deadline = Arc::new(std::sync::Mutex::new(DeadlineState::default()));
        let tracker = spawn_deadline_tracker(
            container_exit_seen.clone(),
            deadline.clone(),
            0,
            deadline_after_exit,
        );
        Self {
            fired: false,
            deadline,
            container_exit_seen,
            deadline_after_exit,
            tracker,
        }
    }

    /// Resolves once the guest's deadline has passed; never resolves while the container is
    /// running or once the watchdog has fired.
    pub(crate) async fn expired(&mut self) {
        if self.fired {
            return std::future::pending().await;
        }
        loop {
            let at = self.deadline.lock().unwrap().at;
            match at {
                Some(at) => return tokio::time::sleep_until(at).await,
                None => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
            }
        }
    }

    /// The deadline passed: kill the VMM. The run loop's wait arm then wakes as for any exit.
    pub(crate) fn fire<K: Killable + ?Sized>(&mut self, vmm: &mut K) -> Result<()> {
        self.fired = true;
        vmm.start_kill()
    }

    /// A rebooted guest was relaunched: watch its next power-off too. The old tracker is replaced,
    /// so a replacement container that exits before anything polls is still timed.
    pub(crate) fn rearm(&mut self) {
        self.tracker.abort();
        self.fired = false;
        let generation = self.deadline.lock().unwrap().retire();
        self.tracker = spawn_deadline_tracker(
            self.container_exit_seen.clone(),
            self.deadline.clone(),
            generation,
            self.deadline_after_exit,
        );
    }
}

impl Drop for PoweroffWatchdog {
    fn drop(&mut self) {
        self.tracker.abort();
    }
}

/// Event loop: waits for VM exit, cancellation, or snapshot requests.
/// Returns the container exit code (None if cancelled/signalled).
pub async fn run_vm_loop(ctx: &mut VmContext, cancel: CancellationToken) -> Result<Option<i32>> {
    let mut watchdog =
        PoweroffWatchdog::new(ctx.container_exit_seen.clone(), GUEST_POWEROFF_DEADLINE);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("cancellation requested, shutting down VM");
                return Ok(None);
            }
            // The container has exited and the guest has not powered off in time. Kill the VMM;
            // the arm below then wakes as for any exit and reports the status fcvm already has.
            _ = watchdog.expired() => {
                warn!(
                    deadline_s = GUEST_POWEROFF_DEADLINE.as_secs(),
                    log = %ctx.data_dir.join("firecracker.log").display(),
                    "the guest did not power off after the container exited; killing the VMM and reporting the container's exit code"
                );
                watchdog
                    .fire(ctx.vm_manager.as_mut())
                    .context("killing a VMM whose guest did not power off")?;
            }
            status = ctx.vm_manager.wait() => {
                info!(status = ?status, "Firecracker child exited");

                // A guest `reboot` exits the firecracker child just like a poweroff,
                // but the guest first sends "reboot" on the status channel. If that
                // signal arrived (grace-polled to absorb the race with the exit
                // notification), relaunch Firecracker in place against the same disk
                // — the provisioned rootfs makes the re-boot behave like a disk-only
                // clone (storage preserved, captured container restarted) — and keep
                // looping. The fcvm process, network, holder, listeners, and health
                // monitor all stay alive, so the fcvm PID is stable across the reboot.
                let status_socket_path = format!(
                    "{}_{}",
                    ctx.reboot_spec.vsock_socket_path.display(),
                    VSOCK_STATUS_PORT
                );
                let rebooted = wait_for_reboot_decision(
                    &ctx.reboot_requested,
                    &ctx.status_handle,
                    &status_socket_path,
                )
                .await;
                // TTY sessions accept exactly one connection and remove their socket
                // on exit — a relaunch would come back with no terminal. Fail loud
                // and treat the reboot as termination instead of half-relaunching.
                if rebooted && ctx.args.tty {
                    warn!(
                        "guest rebooted but reboot-in-place is not supported for TTY \
                         VMs; treating as termination"
                    );
                }
                if rebooted && !ctx.args.tty {
                    ctx.reboot_requested
                        .store(false, std::sync::atomic::Ordering::Release);
                    // Clear any racing pre-reboot exit signal/files: the relaunched
                    // VM starts a fresh lifecycle, and a stale container-exit file
                    // would make the health monitor (and a later exit-code read)
                    // see the OLD container as stopped.
                    ctx.container_exit_seen
                        .store(false, std::sync::atomic::Ordering::Release);
                    let _ = std::fs::remove_file(ctx.data_dir.join("container-exit"));
                    let _ = std::fs::remove_file(ctx.data_dir.join("container-ready"));
                    info!("guest rebooted — relaunching VM in place");
                    watchdog.rearm();
                    // A reboot is a clean cold boot from the already-provisioned disk
                    // (disk-only-clone semantics) — don't re-create the pre-start /
                    // startup snapshot. The Continue verdict set below makes the
                    // relaunched fc-agent's cache-ready resolve to a cold start,
                    // so it proceeds straight to the container.
                    // The balloon check of a --free-page-reporting VM is not run
                    // after a relaunch. The relaunch replays the launch config the
                    // first boot used (`ctx.reboot_spec`), so a guest that passed the
                    // check brings its balloon up again. A guest that reboots before
                    // its first ask is not checked by this run. A memory snapshot of
                    // it still makes the test (`snapshot_balloon_device`).
                    ctx.cache_rx = None;
                    ctx.startup_rx = None;
                    // The relaunched fc-agent re-sends cache-ready; a rebooted
                    // guest cold-boots from the provisioned disk regardless of
                    // how its predecessor was classified (a restored clone that
                    // reboots is NOT restored again — nothing will publish
                    // restore readiness to it).
                    *ctx.cache_verdict.lock().unwrap() = CacheVerdict::Continue;
                    // The rebooted VM's memory is a fresh boot, not a descendant of
                    // any snapshot — a later diff snapshot against the recorded
                    // parent would mix incompatible memory lineages. Clear it so a
                    // future `snapshot create` takes a full snapshot.
                    ctx.vm_state.config.snapshot_name = None;
                    let _ = ctx
                        .state_manager
                        .update_state(&ctx.vm_id, |state| {
                            state.config.snapshot_name = None;
                        })
                        .await;
                    // Fresh Firecracker child on the same api socket; spawn() removes
                    // the stale socket and re-enters the same network namespace via the
                    // holder_pid/namespace fields the backend's VmManager still holds (the
                    // relaunch spec carries only the binary + args). Then the shared
                    // configure-and-boot primitive replays the per-child config; None
                    // skips the host-once-only steps (the live substrate is reused).
                    let relaunch_spec = crate::hypervisor::ProcessSpec {
                        binary: ctx.reboot_spec.firecracker_bin.clone(),
                        extra_args: ctx.reboot_spec.fc_args.clone(),
                        ..Default::default()
                    };
                    // The relaunched fc-agent mounts the volumes again, so the
                    // maps are parsed and checked again, before the new VMM exists.
                    let volume_mappings = types::checked_volume_mappings(
                        &ctx.args,
                        ctx.reboot_spec.image_disk_path.is_some(),
                    )?;
                    ctx.vm_manager
                        .spawn(&relaunch_spec)
                        .await
                        .context("relaunching Firecracker after guest reboot")?;
                    // Reuse the original boot-plan transport; the relaunched guest's
                    // boot args still carry fcvm_bootplan=vsock when applicable.
                    let bootplan_over_vsock = ctx.reboot_spec.bootplan_over_vsock;
                    if let Some(old) = ctx.bootplan_handle.take() {
                        old.abort();
                    }
                    ctx.bootplan_handle = vm_config::configure_and_boot_vm(
                        ctx.vm_manager.as_mut(),
                        &ctx.reboot_spec,
                        &ctx.args,
                        &ctx.network_config,
                        &mut ctx.vm_state,
                        &ctx.data_dir,
                        &ctx.vm_id,
                        &volume_mappings,
                        None,
                        bootplan_over_vsock,
                    )
                    .await
                    .context("relaunching VM after guest reboot")?;
                    continue;
                }

                let exit_code = if let Some(handle) = ctx.tty_handle.take() {
                    let socket_path = ctx.tty_socket_path.take();
                    let exit_code = join_tty_session(handle, socket_path).await;
                    info!(container_exit_code = ?exit_code, "TTY container exit code");
                    exit_code
                } else {
                    let exit_code =
                        read_container_exit_code(&ctx.container_exit_seen, &ctx.data_dir).await;
                    info!(container_exit_code = ?exit_code, "container exit code");
                    exit_code
                };
                // No exit code means the guest never reported the container's result.
                // The library's VmHandle reads the code itself, so only that case is
                // decided here.
                if exit_code.is_none() {
                    container_exit_status(None)?;
                }
                return Ok(exit_code);
            }
            // Handle cache creation requests from fc-agent
            Some(cache_request) = async {
                match ctx.cache_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                // The guest's agent is up, so its kernel has probed its devices. A
                // cold boot with --free-page-reporting is checked here: before the
                // pre-start snapshot below, and before the guest is told to go on.
                // The check takes the per-VM snapshot lock, which a `fcvm snapshot create`
                // of this VM can hold for minutes, so it is raced with cancellation (#1075).
                let settled = tokio::select! {
                    settled = settle_balloon_check(ctx) => Some(settled),
                    _ = cancel.cancelled() => None,
                };
                match settled {
                    Some(Ok(())) => {}
                    // The request is dropped unanswered, and that must not read as
                    // "start the container": this VM is about to be torn down.
                    Some(Err(error)) => {
                        *ctx.cache_verdict.lock().unwrap() = CacheVerdict::Doomed;
                        return Err(error);
                    }
                    None => {
                        info!("shutdown requested while checking the VM's balloon device");
                        *ctx.cache_verdict.lock().unwrap() = CacheVerdict::Doomed;
                        return Ok(None);
                    }
                }
                if let Some(ref key) = ctx.snapshot_key {
                    info!(snapshot_key = %key, digest = %cache_request.digest, "Creating pre-start snapshot");

                    let snap = CreateSnapshotParams::cache_entry(
                        SnapshotVmm::of(ctx.vm_manager.as_ref())?,
                        key,
                        // Taken before the workload starts and shared between targets.
                        BalloonRequirement::Any,
                        &ctx.vm_state,
                        &ctx.disk_path,
                        &ctx.volume_configs,
                        &ctx.volume_servers.remap_refs,
                    );
                    match create_snapshot_interruptible(
                        &snap,
                        &cancel,
                        snapshot_source_disposition(&PodmanLifecycle::Run),
                    )
                    .await
                    {
                        SnapshotOutcome::Interrupted => {
                            // Shutdown mid-decision: the guest must not launch
                            // its container into the teardown.
                            *ctx.cache_verdict.lock().unwrap() = CacheVerdict::Doomed;
                            return Ok(None);
                        }
                        outcome @ (SnapshotOutcome::Created | SnapshotOutcome::Existing) => {
                            // NV2 (vEL2) guests: do NOT ack fc-agent and do NOT resume
                            // into this VM — tear it down and restore the snapshot we just
                            // produced (or that another process installed under this key
                            // first), so the miss path runs the exact same restore flow
                            // as a hit. Resuming the paused VM is the lifecycle that
                            // intermittently starves Firecracker's device event loop under
                            // NV2 timer/vsock churn (#630: NETDEV watchdog, stalled FUSE,
                            // 100x guest slowdowns; create+resume stormed 3/3 under load,
                            // restore was clean 12/12).
                            //
                            // All other profiles keep the resume flow: the storms were
                            // never observed outside NV2, resume has long CI mileage
                            // there, and several flows (RW extra disks, rootless port
                            // forwarding, NFS shares) historically never restored because
                            // per-run paths make their cache keys unique — forcing them
                            // through restore regressed all three classes.
                            //
                            // A --gpu run converges too: its devices are attached by the
                            // restore, so this cold-booted VM has none of them.
                            if nv2_profile(&ctx.vm_state.config.kernel_profile)
                                || !ctx.args.gpu.is_empty()
                            {
                                info!(
                                    snapshot_key = %key,
                                    "Pre-start snapshot created; relaunching by restoring it \
                                     (the miss path converges on the hit path)"
                                );
                                // Recorded BEFORE the return drops the oneshot: a
                                // re-asking guest must hear "cache-doomed", never
                                // silence read as "maybe ack later" or a bare ack
                                // from a raced teardown.
                                *ctx.cache_verdict.lock().unwrap() = CacheVerdict::Doomed;
                                ctx.restore_from_cache = Some(key.clone());
                                return Ok(None);
                            }
                            if matches!(outcome, SnapshotOutcome::Existing) {
                                // Another process's snapshot holds another guest's memory.
                                // This cold-booted VM records no parent, so its next
                                // snapshot is a full one (#1231).
                                info!(
                                    snapshot_key = %key,
                                    "Pre-start snapshot installed by another process; not \
                                     recorded as this VM's diff parent"
                                );
                            } else {
                                info!(snapshot_key = %key, "Pre-start snapshot created successfully");
                                ctx.vm_state.config.snapshot_name = Some(key.clone());
                                // Locked read-modify-write: only update snapshot_name so the
                                // health monitor's concurrent writes are not clobbered.
                                let _ = ctx
                                    .state_manager
                                    .update_state(&ctx.vm_state.vm_id, |state| {
                                        state.config.snapshot_name = Some(key.clone());
                                    })
                                    .await;
                            }
                        }
                        SnapshotOutcome::Failed(e) if !ctx.args.gpu.is_empty() => {
                            // Continuing cold would run the container without its GPU.
                            *ctx.cache_verdict.lock().unwrap() = CacheVerdict::Doomed;
                            return Err(e.context(
                                "creating the pre-start snapshot that --gpu restores with its \
                                 devices attached",
                            ));
                        }
                        SnapshotOutcome::Failed(e) => {
                            warn!(snapshot_key = %key, error = %e, "Failed to create pre-start snapshot");
                        }
                        SnapshotOutcome::NotTaken => {
                            // Only a startup snapshot is declined. Continue cold like a failure.
                        }
                    }
                    // Continue cold in this VM (snapshot kept alongside a
                    // resumed source, or creation failed). Recorded BEFORE the
                    // oneshot resolves so the listener's answer can never race
                    // ahead of the decision.
                    //
                    // failpoint: widen the resume→ack window. The snapshot save
                    // queued a vsock TRANSPORT_RESET into the guest, so on
                    // resume the guest's handshake session is severed while
                    // this ack is still on its way — the interleaving that
                    // hung the source until the re-ask protocol (#799).
                    *ctx.cache_verdict.lock().unwrap() = CacheVerdict::Continue;
                    failpoint::hit_async("cache.pre_ack").await;
                    let _ = cache_request.ack_tx.send(());
                } else {
                    // No snapshot key: nothing to decide, continue cold.
                    *ctx.cache_verdict.lock().unwrap() = CacheVerdict::Continue;
                    let _ = cache_request.ack_tx.send(());
                }
                // Continue waiting for VM exit or cancellation
            }
            // Handle startup snapshot creation when health becomes healthy. The health
            // monitor defers publishing Healthy until `startup_ack` is sent (or dropped
            // by the abort paths below), so no client can observe Healthy while the
            // snapshot pause has the vCPUs stopped.
            Ok(startup_ack) = async {
                match ctx.startup_rx.as_mut() {
                    Some(rx) => rx.await,
                    None => std::future::pending().await,
                }
            } => {
                // Oneshot channel - prevent further attempts
                ctx.startup_rx = None;

                // The balloon check normally ran at the agent's first ask. It runs
                // here if that ask never came (fc-agent skips it when it cannot
                // quiesce the console), so the startup snapshot is behind it too.
                // Raced with cancellation like the one at the ask (#1075).
                tokio::select! {
                    settled = settle_balloon_check(ctx) => settled?,
                    _ = cancel.cancelled() => {
                        info!("shutdown requested while checking the VM's balloon device");
                        return Ok(None);
                    }
                }

                if let Some(ref base) = ctx.startup_snapshot_base_key {
                    let startup_key =
                        startup_snapshot_key(base, ctx.args.balloon, run_host_ipv6(&ctx.args.env));

                    // Skip if startup snapshot already exists
                    if check_podman_snapshot(&startup_key).await.is_some() {
                        info!(snapshot_key = %startup_key, "Startup snapshot already exists, skipping");
                    } else {
                        info!(snapshot_key = %startup_key, "Creating startup snapshot (VM healthy)");
                        // failpoint: hold between the existence check above and the
                        // create's generation lock, where another process can install
                        // the same content-addressed snapshot first.
                        failpoint::hit_async("snapshot.startup.pre_create").await;

                        // Use select! so SIGTERM can abort startup snapshot immediately.
                        // This is safe: startup snapshots are optional (just caching), so
                        // if the VM is paused mid-snapshot when we cancel, cleanup will
                        // kill the VM anyway via vm_manager.kill().
                        // The diff parent is resolved inside create_podman_snapshot under
                        // the per-VM snapshot lock (re-read from the state file), so a
                        // concurrent `fcvm snapshot create` cannot leave us with a stale base.
                        let snap = CreateSnapshotParams::cache_entry(
                            SnapshotVmm::of(ctx.vm_manager.as_ref())?,
                            &startup_key,
                            BalloonRequirement::StartedAt(ctx.args.balloon),
                            &ctx.vm_state,
                            &ctx.disk_path,
                            &ctx.volume_configs,
                            &ctx.volume_servers.remap_refs,
                        );
                        tokio::select! {
                            outcome = create_snapshot_interruptible(
                                &snap,
                                &cancel,
                                snapshot_source_disposition(&PodmanLifecycle::Run),
                            ) => {
                                match outcome {
                                    SnapshotOutcome::Interrupted => {
                                        return Ok(None);
                                    }
                                    SnapshotOutcome::Existing => {
                                        // Another process's snapshot holds another guest's
                                        // memory. The VM keeps its pre-start snapshot as its
                                        // diff parent (#1231).
                                        info!(
                                            snapshot_key = %startup_key,
                                            "Startup snapshot installed by another process; not \
                                             recorded as this VM's diff parent"
                                        );
                                    }
                                    SnapshotOutcome::Created => {
                                        info!(snapshot_key = %startup_key, "Startup snapshot created successfully");
                                        ctx.vm_state.config.snapshot_name = Some(startup_key.clone());
                                        // Locked read-modify-write: only update snapshot_name so the
                                        // health monitor's concurrent writes are not clobbered.
                                        let _ = ctx
                                            .state_manager
                                            .update_state(&ctx.vm_state.vm_id, |state| {
                                                state.config.snapshot_name =
                                                    Some(startup_key.clone());
                                            })
                                            .await;
                                    }
                                    SnapshotOutcome::Failed(e) => {
                                        warn!(snapshot_key = %startup_key, error = %e, "Failed to create startup snapshot");
                                    }
                                    SnapshotOutcome::NotTaken => {
                                        // The creator logged why. The run keeps the pre-start
                                        // snapshot as its parent and saves no startup snapshot.
                                    }
                                }
                            }
                            _ = cancel.cancelled() => {
                                info!(snapshot_key = %startup_key, "Startup snapshot aborted by shutdown signal");
                                return Ok(None);
                            }
                        }
                    }
                }
                // Snapshot attempt over (created, skipped, or failed) and the VM is
                // resumed — let the health monitor publish Healthy. The early-return
                // abort paths above drop `startup_ack`, which unblocks it the same way.
                let _ = startup_ack.send(());
                // Continue waiting for VM exit or cancellation
            }
        }
    }
}

/// How long `podman prepare` waits for its disposable source to first report Healthy.
///
/// Generous, because this covers guest boot plus container startup on a loaded runner. It
/// exists so an image whose container never becomes healthy fails with a diagnostic instead
/// of hanging the command until its caller runs out of wall clock.
const PREPARE_HEALTH_BUDGET: std::time::Duration = std::time::Duration::from_secs(600);

/// The disposable source stopped before its container became healthy: its VM exited, or its
/// health monitor ended (it ends when it sees the container stopped). A type of its own, so
/// [`run_prepare_loop`] can add the container's exit code to these outcomes and to no other.
#[derive(Debug)]
enum PrepareSourceStopped {
    VmExited(std::process::ExitStatus),
    MonitorStopped,
}

impl std::fmt::Display for PrepareSourceStopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::VmExited(status) => write!(
                f,
                "disposable prepare VM exited before the container became healthy: {status}"
            ),
            Self::MonitorStopped => {
                write!(f, "health monitor stopped before prepare snapshot creation")
            }
        }
    }
}

impl std::error::Error for PrepareSourceStopped {}

/// How many of the container's last output lines a failed `podman prepare` puts in its error.
const PREPARE_ERROR_TAIL_LINES: usize = 20;

/// How much of the end of the saved container log is read for those lines.
const PREPARE_ERROR_TAIL_BYTES: u64 = 16 << 10;

/// A failed prepare's error followed by the end of the container's output, and where the output
/// fcvm kept is: `podman prepare` forwards none of it, and teardown copies it to `saved` and the
/// lines before its last rotation to `rotated` (`common::save_vm_logs`). An error that named
/// neither left the caller to guess the VM id.
///
/// The copies are in /tmp, where another local user can put something at those names before
/// teardown writes them, so a path is read only when it is a regular file, opened without
/// following a symlink or waiting on a FIFO.
pub(crate) fn with_container_output(
    error: anyhow::Error,
    saved: &std::path::Path,
    rotated: &std::path::Path,
) -> anyhow::Error {
    use std::io::{Read, Seek};
    use std::os::unix::fs::OpenOptionsExt;
    let open_regular = |path: &std::path::Path| {
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(path)
            .ok()
            .filter(|file| file.metadata().is_ok_and(|m| m.is_file()))
    };
    let Some(file) = open_regular(saved) else {
        return error;
    };
    let rotated_file = open_regular(rotated);
    let kept = if rotated_file.is_some() {
        format!(
            "{}, and the lines before them in {}",
            saved.display(),
            rotated.display()
        )
    } else {
        saved.display().to_string()
    };
    // An empty current copy with a rotated one beside it (the rotation's note failed to write)
    // is not "printed nothing": the last lines are in the rotated copy.
    let mut file = match rotated_file {
        Some(rotated) if file.metadata().is_ok_and(|m| m.len() == 0) => rotated,
        _ => file,
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(PREPARE_ERROR_TAIL_BYTES);
    let mut bytes = Vec::new();
    if file.seek(std::io::SeekFrom::Start(start)).is_err() || file.read_to_end(&mut bytes).is_err()
    {
        return error;
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<std::borrow::Cow<'_, str>> = text.lines().map(Into::into).collect();
    if start > 0 {
        // The read began inside a line: mark the line as cut rather than drop it, so a last
        // line longer than the read is still shown.
        if let Some(first) = lines.first_mut() {
            *first = format!("[...]{first}").into();
        }
    }
    let tail = &lines[lines.len().saturating_sub(PREPARE_ERROR_TAIL_LINES)..];
    if tail.is_empty() {
        anyhow::anyhow!("{error:#}\nthe container printed nothing (its output file is {kept})")
    } else {
        anyhow::anyhow!(
            "{error:#}\nthe container's last {} lines of output (the output fcvm kept is in {kept}):\n{}",
            tail.len(),
            tail.join("\n")
        )
    }
}

/// Resolve why the wait for the disposable source's first Healthy transition ended.
///
/// Split out of [`run_prepare_loop`] so every outcome is exercised without a VM: the
/// VM-exit arm takes its future from the caller.
/// A prepare's wait for Healthy that failed because its source stopped says what the
/// container exited with; any other failure is returned as it is. A cold prepare and one
/// restored from a pre-start snapshot both report through this.
pub(crate) async fn with_container_exit_code(
    error: anyhow::Error,
    container_exit_seen: &std::sync::atomic::AtomicBool,
    data_dir: &Path,
) -> anyhow::Error {
    if error.downcast_ref::<PrepareSourceStopped>().is_none() {
        return error;
    }
    match read_container_exit_code(container_exit_seen, data_dir).await {
        Some(code) => error.context(format!("the container exited with code {code}")),
        None => error.context("the container reported no exit code"),
    }
}

pub(crate) async fn await_prepare_healthy<F>(
    startup_rx: tokio::sync::oneshot::Receiver<crate::health::StartupSnapshotAck>,
    cancel: &CancellationToken,
    budget: std::time::Duration,
    vm_exited: F,
) -> Result<crate::health::StartupSnapshotAck>
where
    F: std::future::Future<Output = Result<std::process::ExitStatus>>,
{
    tokio::select! {
        _ = cancel.cancelled() => {
            bail!("interrupted before the container became healthy")
        }
        status = vm_exited => {
            let status = status.context("waiting for disposable prepare VM")?;
            Err(PrepareSourceStopped::VmExited(status).into())
        }
        _ = tokio::time::sleep(budget) => {
            bail!(
                "disposable prepare VM did not report a healthy container within {}s \
                 (raise --ready-timeout for workloads with long warmups)",
                budget.as_secs()
            )
        }
        startup_ack = startup_rx => {
            startup_ack.map_err(|_| PrepareSourceStopped::MonitorStopped.into())
        }
    }
}

/// Wait for the ordinary health monitor's first Healthy transition, capture the startup
/// artifact, and leave the disposable source paused. No prepare-specific readiness probe
/// exists: image HEALTHCHECK (when present), container-running state otherwise, and the
/// optional HTTP health check retain exactly the same authority as a normal run.
async fn run_prepare_loop(
    ctx: &mut VmContext,
    lifecycle: &PodmanLifecycle,
    cancel: &CancellationToken,
) -> Result<PreparedSnapshot> {
    let startup_rx = ctx
        .startup_rx
        .take()
        .context("prepare lifecycle has no startup health trigger")?;

    let vm_exited = ctx.vm_manager.wait();
    let budget = match lifecycle {
        PodmanLifecycle::Prepare(options) => options.ready_timeout,
        PodmanLifecycle::Run => PREPARE_HEALTH_BUDGET,
    };
    let startup_ack = match await_prepare_healthy(startup_rx, cancel, budget, vm_exited).await {
        Ok(startup_ack) => startup_ack,
        Err(error) => {
            return Err(
                with_container_exit_code(error, &ctx.container_exit_seen, &ctx.data_dir).await,
            )
        }
    };

    // The container is healthy, so the guest's agent is up. A cold boot with
    // --free-page-reporting is checked before the snapshot below is taken.
    settle_balloon_check(ctx).await?;

    // Resolved before the boot, so the generation this installs is the one the pre-boot
    // cache check looked for.
    let target = ctx
        .prepare_target
        .clone()
        .context("prepare lifecycle has no snapshot install target")?;
    let fc_backend = ctx
        .vm_manager
        .as_any()
        .downcast_ref::<FirecrackerBackend>()
        .context("podman prepare requires the Firecracker backend")?;
    let prepared = install_prepared_startup_snapshot(
        fc_backend,
        &target,
        ctx.args.balloon,
        &ctx.vm_state,
        &ctx.disk_path,
        &ctx.volume_configs,
        &ctx.volume_servers.remap_refs,
        cancel,
    )
    .await?;

    // Never acknowledge Healthy in the disposable source. It remains paused until cleanup;
    // dropping this ack only releases the monitor so its task can observe cancellation.
    drop(startup_ack);
    Ok(prepared)
}

/// Install a prepare's startup snapshot from its healthy disposable VM, with the VM left
/// paused, and verify the generation it installed. The cold prepare (`run_prepare_loop`)
/// and a prepare restored from its pre-start snapshot (`snapshot run` with
/// `SnapshotRunArgs::prepare`) both end here.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn install_prepared_startup_snapshot(
    fc_backend: &FirecrackerBackend,
    target: &PreparedTarget,
    balloon: Option<u32>,
    vm_state: &VmState,
    disk_path: &Path,
    volume_configs: &[VolumeConfig],
    remap_refs: &[Option<Arc<fuse_pipe::RemapFs<fuse_pipe::PassthroughFs>>>],
    cancel: &CancellationToken,
) -> Result<PreparedSnapshot> {
    info!(
        snapshot_key = %target.name,
        content_key = %target.content_key,
        snapshot_type = %target.snapshot_type,
        "Creating prepared startup snapshot (VM healthy)"
    );
    let snap = CreateSnapshotParams {
        vm_manager: SnapshotVmm::Firecracker(fc_backend),
        snapshot_key: &target.name,
        content_key: &target.content_key,
        snapshot_type: target.snapshot_type,
        existing: target.existing,
        // What prepare installs is a startup snapshot.
        balloon: BalloonRequirement::StartedAt(balloon),
        vm_state,
        disk_path,
        volume_configs,
        remap_refs,
    };
    let install = match snapshot::create_podman_snapshot(
        &snap,
        super::common::SnapshotSourceDisposition::LeavePaused,
        Some(cancel),
    )
    .await
    {
        Ok(install) => install,
        Err(error) => {
            let replaced = replaced_snapshot(target).await;
            return Err(explain_snapshot_space_shortfall(
                error,
                target.free_at_start,
                replaced.as_ref(),
            )
            .context(format!(
                "creating prepared startup snapshot {}",
                target.name
            )));
        }
    };

    if cancel.is_cancelled() {
        bail!("interrupted while creating prepared startup snapshot");
    }

    // Acquire a shared generation lease immediately after the creator releases its exclusive
    // install lock. Keep it through verified cleanup and JSON publication.
    let cache = match install {
        SnapshotInstall::Created => PreparedCache::Created,
        SnapshotInstall::Existing => PreparedCache::Hit,
        SnapshotInstall::BalloonTargetChanged { started_at, now } => bail!(
            "the VM's balloon target was changed from {} to {} while it was being prepared, \
             so its workload did not initialize under the target the prepared snapshot is \
             named for; nothing was installed",
            snapshot::balloon_text(started_at),
            snapshot::balloon_text(now)
        ),
    };
    verify_prepared_snapshot(target, cache)
        .await?
        .with_context(|| {
            format!(
                "prepared startup snapshot {} no longer names the generation this prepare installed",
                target.name
            )
        })
}

/// Clean up all resources associated with a VM.
pub async fn cleanup_vm_context(mut ctx: VmContext) {
    // Cancel status listener (podman-specific)
    ctx.status_handle.abort();

    // Stop the egress proxy task (rootless mode only)
    if let Some(handle) = ctx.egress_proxy_handle.take() {
        handle.abort();
    }

    // Stop the boot-plan vsock listener (vsock transport only)
    if let Some(handle) = ctx.bootplan_handle.take() {
        handle.abort();
    }

    // Cleanup common resources (includes NFS export removal)
    super::common::cleanup_vm(
        super::common::CleanupContext {
            vm_id: ctx.vm_id,
            volume_server_handles: ctx.volume_servers.handles,
            remap_refs: ctx.volume_servers.remap_refs,
            data_dir: ctx.data_dir,
            health_cancel_token: Some(ctx.health_cancel_token),
            health_monitor_handle: Some(ctx.health_monitor_handle),
            output_listener_handle: ctx.output_handle,
            container_log: ctx.container_log,
        },
        ctx.vm_manager.as_mut(),
        &mut ctx.holder_child,
        ctx.network.as_mut(),
        &ctx.state_manager,
    )
    .await;
}

/// Verified counterpart to [`cleanup_vm_context`] for finite lifecycle commands.
async fn cleanup_vm_context_verified(mut ctx: VmContext) -> Result<()> {
    let mut companion_errors = Vec::new();

    let mut companions = vec![("status listener", ctx.status_handle)];
    if let Some(handle) = ctx.egress_proxy_handle.take() {
        companions.push(("egress proxy", handle));
    }
    if let Some(handle) = ctx.bootplan_handle.take() {
        companions.push(("boot-plan listener", handle));
    }
    for (_, handle) in &companions {
        handle.abort();
    }
    for (name, handle) in companions {
        if let Err(error) = handle.await {
            if !error.is_cancelled() {
                companion_errors.push(format!("joining {name}: {error}"));
            }
        }
    }

    let cleanup_result = super::common::cleanup_vm_verified(
        super::common::CleanupContext {
            vm_id: ctx.vm_id,
            volume_server_handles: ctx.volume_servers.handles,
            remap_refs: ctx.volume_servers.remap_refs,
            data_dir: ctx.data_dir,
            health_cancel_token: Some(ctx.health_cancel_token),
            health_monitor_handle: Some(ctx.health_monitor_handle),
            output_listener_handle: ctx.output_handle,
            container_log: ctx.container_log,
        },
        ctx.vm_manager.as_mut(),
        &mut ctx.holder_child,
        ctx.network.as_mut(),
        &ctx.state_manager,
    )
    .await;

    if let Err(error) = cleanup_result {
        companion_errors.push(format!("common VM resources: {error:#}"));
    }
    if companion_errors.is_empty() {
        Ok(())
    } else {
        bail!(
            "verified prepare cleanup failed: {}",
            companion_errors.join("; ")
        )
    }
}

pub(crate) fn finish_prepare<T>(operation: Result<T>, cleanup: Result<()>) -> Result<T> {
    match (operation, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(operation), Ok(())) => Err(operation),
        (Ok(_), Err(cleanup)) => Err(cleanup),
        (Err(operation), Err(cleanup)) => Err(anyhow::anyhow!(
            "prepare failed: {operation:#}; cleanup also failed: {cleanup:#}"
        )),
    }
}

/// Request this process's shutdown on SIGTERM/SIGINT, describing it as `shutdown_action`.
///
/// Installed before the VM is created so a signal during the (potentially long) setup phase
/// is recorded instead of killing the process and leaving host network state, the persisted
/// VM state file, and the data directory behind. Shutdown is deferred until setup finishes,
/// then the caller's cleanup runs. The request cancels every lifecycle gate of the process,
/// including one a cache-hit restore creates later (`common::request_process_shutdown`).
fn spawn_lifecycle_signal_handler(shutdown_action: &'static str) -> Result<()> {
    let mut sigterm = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
    let mut sigint = signal(SignalKind::interrupt()).context("installing SIGINT handler")?;
    tokio::spawn(async move {
        let signal_name = tokio::select! {
            _ = sigterm.recv() => "SIGTERM",
            _ = sigint.recv() => "SIGINT",
        };
        super::common::request_process_shutdown();
        info!("received {signal_name}, {shutdown_action}");
    });
    Ok(())
}

fn publish_prepared_snapshot_gated(
    lifecycle_gate: &super::common::LifecycleReadyGate,
    prepared: &PreparedSnapshot,
) -> Result<()> {
    match lifecycle_gate.claim_terminal_publication()? {
        super::common::LifecycleReadyOutcome::Published => publish_prepared_snapshot(prepared),
        super::common::LifecycleReadyOutcome::Cancelled => {
            bail!("interrupted before prepared snapshot publication")
        }
    }
}

/// CLI entrypoint for `fcvm podman run`. Thin wrapper around prepare_vm/run_vm_loop/cleanup.
///
/// Public so the disk-only `snapshot run` dispatcher can reuse the exact same
/// boot/loop/cleanup sequence after synthesizing RunArgs (rootfs_override set to
/// the captured disk).
pub async fn cmd_podman_run(args: RunArgs) -> Result<()> {
    let lifecycle_gate = super::common::LifecycleReadyGate::new();
    let cancel = lifecycle_gate.cancellation_token();
    spawn_lifecycle_signal_handler("shutting down VM")?;

    let Some(mut ctx) = prepare_vm(args).await? else {
        return Ok(()); // Snapshot cache hit, already handled
    };

    // A signal may have arrived while setup was still running — clean up and report
    // the interruption instead of entering the run loop (or exiting successfully).
    if cancel.is_cancelled() {
        info!("shutdown requested during VM setup, cleaning up");
        cleanup_vm_context(ctx).await;
        bail!("interrupted by signal during VM setup");
    }

    // PID/state are intentionally visible during setup, but exact child capture and
    // external lifecycle actions wait for this atomic barrier. At this point the signal
    // streams, VMM/holder/network children, and every companion listener are all owned.
    match lifecycle_gate
        .publish(&ctx.state_manager, &mut ctx.vm_state)
        .await
    {
        Ok(super::common::LifecycleReadyOutcome::Published) => {}
        Ok(super::common::LifecycleReadyOutcome::Cancelled) => {
            cleanup_vm_context(ctx).await;
            bail!("interrupted by signal during VM setup");
        }
        Err(error) => {
            cleanup_vm_context(ctx).await;
            return Err(error);
        }
    }

    // Run the VM loop, then always clean up — even when the loop reports an error.
    let result = run_vm_loop(&mut ctx, cancel.clone()).await;
    let restore_key = ctx.restore_from_cache.take();
    let restore_args = restore_key.as_ref().map(|key| {
        snapshot_restore_args(
            &ctx.args,
            key,
            ctx.startup_snapshot_base_key
                .clone()
                .filter(|_| run_takes_startup_snapshot(&ctx.args)),
            (None, None),
        )
    });
    // The restore listens on this VM's published host listeners, so its ports stay
    // open across the teardown (#1025). Taken before the teardown, which would close
    // them.
    let port_listeners = if restore_args.is_some() {
        ctx.network.take_port_listeners()
    } else {
        Default::default()
    };
    cleanup_vm_context(ctx).await;

    // The run loop stopped on purpose right after producing the pre-start
    // snapshot: relaunch through the restore path (identical to a cache hit).
    if let Some(snapshot_args) = restore_args {
        // A signal during the teardown window cancelled this process's gates, so
        // the restore's own gate is created cancelled and it stops before
        // allocating anything (`common::ShutdownRegistry`).
        return super::snapshot::cmd_snapshot_run_with_listeners(snapshot_args, port_listeners)
            .await;
    }

    // None here is a cancelled run; the loop already failed a VM that exited unreported.
    match result? {
        Some(code) => container_exit_status(Some(code)),
        None => Ok(()),
    }
}

/// CLI entrypoint for `fcvm podman prepare`.
///
/// A cache hit verifies and publishes the exact installed startup generation without booting
/// a VM. A miss boots one disposable source, waits for normal health, atomically installs a
/// startup snapshot while leaving the source paused, reaps every host resource, then emits one
/// JSON record while holding a shared lease on that exact generation.
pub async fn cmd_podman_prepare(args: crate::cli::PrepareArgs) -> Result<()> {
    let lifecycle_gate = super::common::LifecycleReadyGate::new();
    let cancel = lifecycle_gate.cancellation_token();
    spawn_lifecycle_signal_handler("cancelling podman prepare")?;

    let lifecycle = PodmanLifecycle::Prepare(PrepareOptions {
        tag: args.tag,
        force: args.force,
        ready_timeout: std::time::Duration::from_secs(args.ready_timeout),
    });
    match prepare_vm_for_lifecycle(args.run, lifecycle.clone()).await? {
        VmPreparation::Prepared(prepared) => {
            publish_prepared_snapshot_gated(&lifecycle_gate, &prepared)
        }
        VmPreparation::RunCompleted => {
            unreachable!("prepare lifecycle never runs a cached snapshot")
        }
        VmPreparation::Active(mut ctx) => {
            // Teardown copies the container's output here (`common::save_vm_logs`); a failed
            // prepare's error ends with its last lines and names the file.
            let saved_output = ctx.container_log.as_ref().map(|_| {
                let tmp = std::path::Path::new("/tmp");
                (
                    super::common::saved_log_path(tmp, "container", &ctx.vm_id),
                    super::common::saved_log_path(tmp, "container.1", &ctx.vm_id),
                )
            });
            let operation = async {
                if cancel.is_cancelled() {
                    bail!("interrupted during disposable VM setup");
                }
                // The source is an implementation detail, not a runnable workload. Leave its
                // lifecycle barrier unpublished so external commands cannot adopt/snapshot it.
                run_prepare_loop(&mut ctx, &lifecycle, &cancel).await
            }
            .await;

            let cleanup = cleanup_vm_context_verified(*ctx).await;
            let operation = match (operation, saved_output) {
                (Err(error), Some((saved, rotated))) => {
                    Err(with_container_output(error, &saved, &rotated))
                }
                (operation, _) => operation,
            };
            let prepared = finish_prepare(operation, cleanup)?;
            publish_prepared_snapshot_gated(&lifecycle_gate, &prepared)
        }
    }
}

/// The IPv6 prefix a run gives its VM: the run's own for routed networking, and none for
/// the other modes, which never read it. A snapshot-cache hit restores under it, so the
/// prefix is in no snapshot key (#1079).
pub(crate) fn launch_ipv6_prefix(args: &RunArgs) -> Option<String> {
    use crate::cli::args::NetworkMode;
    match args.network {
        NetworkMode::Routed => args.ipv6_prefix.clone(),
        NetworkMode::Bridged | NetworkMode::Rootless => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::{ImageMode as CliImageMode, NetworkMode};
    use crate::firecracker::ImageMode;

    #[tokio::test]
    async fn configured_firecracker_resolution_error_reaches_cold_boot() {
        let profile = crate::setup::KernelProfile {
            firecracker_repo: Some("ejc3/firecracker".to_string()),
            firecracker_commit: Some("27305f49ab3a5d862dc56b5108713b6536d2baa7".to_string()),
            ..Default::default()
        };
        let error = runtime_config_from_kernel_profiles(
            "nested",
            Some(profile),
            None,
            |_profile, _name| async { bail!("missing exact configured Firecracker artifact") },
        )
        .await
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("missing exact configured Firecracker artifact"),
            "{error:#}"
        );
    }

    fn test_args() -> RunArgs {
        RunArgs {
            name: "test".to_string(),
            cpu: 2,
            mem: 2048,
            rootfs_size: "10G".to_string(),
            map: vec![],
            disk: vec![],
            disk_dir: vec![],
            pmem: vec![],
            gpu: vec![],
            nfs: vec![],
            env: vec![],
            cmd: None,
            publish: vec![],
            balloon: None,
            free_page_reporting: false,
            network: NetworkMode::Rootless,
            hypervisor: crate::cli::args::Hypervisor::Firecracker,
            health_check: None,
            startup_snapshot: false,
            health_check_timeout: 5,
            on_restore: None,
            guest_thp: None,
            privileged: false,
            interactive: false,
            tty: false,
            strace_agent: false,
            setup: false,
            kernel: None,
            kernel_profile: None,
            vsock_dir: None,
            no_snapshot: true,
            user: None,
            forward_localhost: vec![],
            hugepages: false,
            portable_volumes: false,
            image_mode: None,
            rootfs_type: None,
            non_blocking_output: false,
            label: vec![],
            ipv6_prefix: None,
            ipv6_address: None,
            dns: None,
            image: "alpine:latest".to_string(),
            command_args: vec![],
            rootfs_override: None,
            image_disk_override: None,
            pmem_devices: vec![],
            resolved_extra_disks: None,
            no_swap: false,
        }
    }

    /// A malformed --publish fails the run by name, and so does a published guest port
    /// that --forward-localhost also claims.
    #[test]
    fn publish_specs_are_parsed_strictly_for_a_run() {
        let mut args = test_args();
        args.publish = vec!["8080:80".to_string(), "[::1:9090:90".to_string()];
        let error = format!("{:#}", publish_mappings(&args).unwrap_err());
        assert!(error.contains("[::1:9090:90"), "{error}");

        args.publish = vec!["8080:80".to_string(), "127.0.0.1:5300:53/udp".to_string()];
        let written: Vec<String> = publish_mappings(&args)
            .unwrap()
            .iter()
            .map(|mapping| mapping.to_string())
            .collect();
        assert_eq!(written, ["8080:80/tcp", "127.0.0.1:5300:53/udp"]);

        args.forward_localhost = vec![80];
        let error = format!("{:#}", publish_mappings(&args).unwrap_err());
        assert!(error.contains("--forward-localhost 80"), "{error}");

        // One host address and port cannot go to two guest ports.
        args.forward_localhost = vec![];
        args.publish = vec!["8080:80".to_string(), "8080:443".to_string()];
        let error = format!("{:#}", publish_mappings(&args).unwrap_err());
        assert!(
            error.contains("8080:80/tcp") && error.contains("8080:443/tcp"),
            "{error}"
        );
    }

    /// `--gpu` devices are attached by a restore of the pre-start snapshot, so every
    /// run that never makes that restore is refused before any VM boots.
    #[test]
    fn a_gpu_run_must_restore_the_pre_start_snapshot() {
        let mut args = test_args();
        args.gpu = vec!["0000:01:00.0".to_string()];
        assert!(validate_gpu_run(&args, &PodmanLifecycle::Run, false).is_ok());

        let err = format!(
            "{:#}",
            validate_gpu_run(&args, &PodmanLifecycle::Run, true).unwrap_err()
        );
        assert!(err.contains("needs the snapshot cache"), "{err}");
        let prepare = PodmanLifecycle::Prepare(PrepareOptions::default());
        let err = format!(
            "{:#}",
            validate_gpu_run(&args, &prepare, false).unwrap_err()
        );
        assert!(err.contains("podman prepare"), "{err}");
        args.hypervisor = crate::cli::args::Hypervisor::CloudHypervisor;
        let err = format!(
            "{:#}",
            validate_gpu_run(&args, &PodmanLifecycle::Run, false).unwrap_err()
        );
        assert!(err.contains("cloud-hypervisor"), "{err}");
    }

    /// A GPU restore can never take a startup snapshot (Firecracker refuses to snapshot
    /// a VM with a passthrough device), and it carries its devices to `snapshot run`.
    #[test]
    fn a_gpu_restore_takes_no_startup_snapshot() {
        let mut args = test_args();
        let plain = snapshot_restore_args(&args, "k", Some("k".to_string()), (None, None));
        assert_eq!(plain.startup_snapshot_base_key.as_deref(), Some("k"));
        assert!(plain.gpu.is_empty());

        args.gpu = vec!["0000:01:00.0".to_string()];
        let gpu = snapshot_restore_args(&args, "k", Some("k".to_string()), (None, None));
        assert_eq!(gpu.startup_snapshot_base_key, None);
        assert_eq!(gpu.gpu, ["0000:01:00.0"]);
    }

    /// A cache hit restores through `snapshot run`, where a clone chooses where it
    /// publishes. The restore has to publish where THIS run asked.
    #[test]
    fn a_cache_restore_publishes_where_this_run_asked() {
        let mut args = test_args();
        args.publish = vec!["[::]:80:80".to_string(), "127.0.0.1:8443:443".to_string()];
        args.vsock_dir = Some("/run/vsock".to_string());
        let restore = snapshot_restore_args(
            &args,
            "0123456789ab",
            Some("0123456789ab".to_string()),
            (Some("/opt/firecracker".to_string()), None),
        );
        assert_eq!(restore.publish, args.publish);

        // The other fields every cache restore carries.
        assert_eq!(restore.pid, None);
        assert_eq!(restore.snapshot.as_deref(), Some("0123456789ab"));
        assert_eq!(restore.name.as_deref(), Some("test"));
        assert_eq!(restore.vsock_dir.as_deref(), Some("/run/vsock"));
        assert_eq!(
            restore.startup_snapshot_base_key.as_deref(),
            Some("0123456789ab")
        );
        assert_eq!((restore.cpu, restore.mem), (Some(2), Some(2048)));
        assert_eq!(restore.firecracker_bin.as_deref(), Some("/opt/firecracker"));
        assert_eq!(restore.firecracker_args, None);
        assert_eq!(restore.hugepages, Some(false));
        assert!(!restore.no_dirty_tracking && !restore.no_swap && restore.exec.is_none());

        // A run that publishes nothing asks for nothing, and the clone inherits.
        let plain = snapshot_restore_args(&test_args(), "k", None, (None, None));
        assert!(plain.publish.is_empty());
    }

    /// Guest-visible launch inputs must change the snapshot key. --dns is
    /// baked into the guest's resolv.conf, so two runs differing only in --dns
    /// must never share a snapshot; before FirecrackerConfig carried these
    /// fields the keys came out equal and a cache hit silently kept the old
    /// resolver. Same guarantee for --strace-agent and extra boot args.
    #[test]
    fn guest_visible_inputs_change_snapshot_key() {
        use std::path::Path;
        let key = |args: &RunArgs, extra: Option<String>| {
            build_firecracker_config(
                args,
                "sha256:test",
                Path::new("/kernel"),
                Path::new("/rootfs"),
                Path::new("/initrd"),
                None,
                ImageMode::Overlay,
                None,
                None,
                None,
                extra,
                GuestBootInputs::default(),
                &[],
            )
            .snapshot_key()
        };
        let base = test_args();

        let mut with_dns = test_args();
        with_dns.dns = Some("10.0.2.2".to_string());
        assert_ne!(
            key(&base, None),
            key(&with_dns, None),
            "--dns must change the key"
        );

        let mut with_strace = test_args();
        with_strace.strace_agent = true;
        assert_ne!(
            key(&base, None),
            key(&with_strace, None),
            "--strace-agent must change the key"
        );

        assert_ne!(
            key(&base, None),
            key(&base, Some("arm64.nv2".to_string())),
            "extra boot args must change the key"
        );
    }

    /// A snapshot is portable across hosts: two runs that differ only in the host
    /// identity address they name share its key, and a cache hit hands the restored
    /// guest this run's address, not the one the snapshot was booted with.
    #[test]
    fn host_ipv6_is_given_at_restore_and_not_keyed() {
        use std::path::Path;
        let key = |args: &RunArgs| {
            build_firecracker_config(
                args,
                "sha256:test",
                Path::new("/kernel"),
                Path::new("/rootfs"),
                Path::new("/initrd"),
                None,
                ImageMode::Overlay,
                None,
                None,
                None,
                None,
                GuestBootInputs::default(),
                &[],
            )
            .snapshot_key()
        };
        let mut prepare = test_args();
        prepare.env = vec!["HOST_IPV6=2001:db8:a::1".to_string()];
        let mut restore = test_args();
        restore.env = vec!["HOST_IPV6=2001:db8:b::1".to_string()];
        assert_eq!(key(&prepare), key(&restore));

        let args = snapshot_restore_args(&restore, "key", None, (None, None));
        assert_eq!(args.host_ipv6, Some("2001:db8:b::1".parse().unwrap()));
        assert_eq!(
            snapshot_restore_args(&test_args(), "key", None, (None, None)).host_ipv6,
            None
        );
    }

    /// #1079: a cache hit restores under this run's prefix; a run in another network mode
    /// passes none.
    #[test]
    fn a_cache_hit_restores_under_the_runs_own_prefix() {
        let mut args = test_args();
        args.network = NetworkMode::Routed;
        args.ipv6_prefix = Some("2001:db8:bbbb:2::/112".to_string());
        let restore = snapshot_restore_args(&args, "key", None, (None, None));
        assert_eq!(
            restore.ipv6_prefix.as_deref(),
            Some("2001:db8:bbbb:2::/112")
        );
        args.network = NetworkMode::Rootless;
        let restore = snapshot_restore_args(&args, "key", None, (None, None));
        assert_eq!(restore.ipv6_prefix, None);
    }

    /// A cache hit restores in this run's huge page mode and never inherits the one the
    /// snapshot recorded, which may be another run's: the mode is not in the cache key.
    #[test]
    fn a_cache_hit_restores_in_the_runs_own_huge_page_mode() {
        let mut args = test_args();
        let restore = snapshot_restore_args(&args, "key", None, (None, None));
        assert_eq!(restore.guest_thp, None);
        assert!(!restore.inherit_recorded_hook);
        args.guest_thp = Some("never".to_string());
        let restore = snapshot_restore_args(&args, "key", None, (None, None));
        assert_eq!(restore.guest_thp.as_deref(), Some("never"));
        assert_eq!(
            crate::commands::snapshot::per_restore_setting(
                restore.guest_thp.as_deref(),
                restore.inherit_recorded_hook,
                Some("madvise"),
            )
            .as_deref(),
            Some("never")
        );
        assert_eq!(
            crate::commands::snapshot::per_restore_setting(None, false, Some("never")),
            None,
            "a run without the flag does not inherit the snapshot's mode"
        );
        assert_eq!(
            crate::commands::snapshot::restore_thp_mode(&None),
            "default"
        );
    }

    /// #1183: a launch records its runtime directory with its owner before anything in
    /// it exists (the --disk-dir staging, then the directory itself), and reclaims what
    /// killed launches left before it records anything. A launch killed in between
    /// leaves the directory to the next launch only if the record comes first.
    #[test]
    fn a_launch_records_its_runtime_dir_before_creating_it() {
        let source = include_str!("mod.rs");
        let body = &source[source
            .find("async fn prepare_vm_for_lifecycle(")
            .expect("podman's lifecycle")..];
        let at = |needle: &str| {
            body.find(needle)
                .unwrap_or_else(|| panic!("prepare_vm_for_lifecycle has no {needle:?}"))
        };
        let reclaimed = at("crate::reclaim::reclaim_abandoned().await");
        let owned = at("crate::reclaim::Owner::begin(&vm_id)?");
        let recorded = at("crate::reclaim::Resource::RuntimeDir {");
        let staged = at("DiskDirStaging::resolve(");
        let created = at("tokio::fs::create_dir_all(&data_dir)");
        assert!(
            reclaimed < owned && owned < recorded,
            "the launch must reclaim, then begin its owner, then record"
        );
        assert!(
            recorded < staged && recorded < created,
            "the runtime directory is created before it is recorded"
        );

        // The same order in a restore.
        let source = include_str!("../snapshot.rs");
        let body = &source[source
            .find("async fn cmd_snapshot_run_inner(")
            .expect("snapshot run")..];
        let at = |needle: &str| {
            body.find(needle)
                .unwrap_or_else(|| panic!("cmd_snapshot_run_inner has no {needle:?}"))
        };
        let reclaimed = at("crate::reclaim::reclaim_abandoned().await");
        let owned = at("crate::reclaim::Owner::begin(&vm_id)?");
        let recorded = at("crate::reclaim::Resource::RuntimeDir {");
        let created = at("tokio::fs::create_dir_all(&data_dir)");
        assert!(
            reclaimed < owned && owned < recorded && recorded < created,
            "a restore must reclaim, begin its owner and record its runtime directory, in \
             that order, before creating the directory"
        );
    }

    /// A refused name claim must remove what the launch created before it, the runtime
    /// directory among them, as every later setup error does.
    #[test]
    fn a_refused_name_claim_cleans_up_the_launch() {
        let source = include_str!("mod.rs");
        let claim = source
            .find(".claim_name(&mut vm_state)")
            .expect("podman run claims the VM's name");
        let next_step = claim
            + source[claim..]
                .find("let tap_device")
                .expect("the network is set up after the claim");
        assert!(
            source[claim..next_step].contains("cleanup_failed_prepare("),
            "the claim's error path does not call cleanup_failed_prepare"
        );
    }

    /// A cache hit gives the restored VM this run's --ipv6-address.
    #[test]
    fn a_cache_hit_restores_at_the_runs_own_address() {
        let mut args = test_args();
        args.network = NetworkMode::Routed;
        args.ipv6_address = Some("2001:db8:bbbb:2::5".parse().unwrap());
        let restore = snapshot_restore_args(&args, "key", None, (None, None));
        assert_eq!(restore.ipv6_address, args.ipv6_address);
    }

    /// The restore swaps in the run's --ipv6-address, so the address is in no snapshot
    /// key: every slot of a fleet restores one snapshot.
    #[test]
    fn ipv6_address_is_in_no_snapshot_key() {
        let key = |address: Option<&str>| {
            let mut args = test_args();
            args.network = NetworkMode::Routed;
            args.ipv6_address = address.map(|a| a.parse().unwrap());
            key_for(&args, GuestBootInputs::default())
        };
        assert_eq!(key(None), key(Some("2001:db8::5")));
        assert_eq!(key(Some("2001:db8::5")), key(Some("2001:db8::6")));
    }

    /// The restore publishes the run's own host address and port for each published guest
    /// port, so only the guest side of `--publish` is in the snapshot key: a fleet host's
    /// slots publish their renderers on different host ports and restore one snapshot.
    #[test]
    fn the_host_side_of_publish_is_in_no_snapshot_key() {
        let key = |publish: &[&str]| {
            let mut args = test_args();
            args.network = NetworkMode::Routed;
            args.publish = publish.iter().map(|p| p.to_string()).collect();
            key_for(&args, GuestBootInputs::default())
        };
        assert_eq!(
            key(&["0.0.0.0:20101:8080", "0.0.0.0:20201:443"]),
            key(&["0.0.0.0:20102:8080", "0.0.0.0:20202:443"])
        );
        assert_ne!(key(&["0.0.0.0:20101:8080"]), key(&["0.0.0.0:20101:8081"]));
        assert_ne!(key(&[]), key(&["0.0.0.0:20101:8080"]));
    }

    /// #1079: a restore takes the run's IPv6 prefix, so the prefix is in no snapshot key. A
    /// routed run on a host whose prefix changed keeps the snapshot it has, and exporting
    /// FCVM_IPV6_PREFIX costs no run its snapshot.
    #[test]
    fn ipv6_prefix_is_in_no_snapshot_key() {
        let key = |network: NetworkMode, prefix: Option<&str>| {
            let mut args = test_args();
            args.network = network;
            args.ipv6_prefix = prefix.map(str::to_string);
            key_for(&args, GuestBootInputs::default())
        };
        for network in [
            NetworkMode::Rootless,
            NetworkMode::Bridged,
            NetworkMode::Routed,
        ] {
            assert_eq!(
                key(network, Some("2001:db8:aaaa:1::/112")),
                key(network, Some("2001:db8:bbbb:2::/112")),
                "{network:?}: the prefix is honored at restore time"
            );
            assert_eq!(
                key(network, None),
                key(network, Some("2001:db8:aaaa:1::/112")),
                "{network:?}: a run without a prefix shares the key"
            );
        }
    }

    /// The snapshot key says whether a balloon device exists, and not its target.
    /// A device cannot be added to a restored VM, so a run with --balloon must not
    /// share a snapshot with a run without it. The target can be set on a restored
    /// VM, and the restore sets it, so runs that differ only in the target share
    /// one. With the target in the key each of them cold-booted and kept a
    /// snapshot of its own (#1053).
    #[test]
    fn the_snapshot_key_says_whether_a_balloon_exists_not_its_target() {
        let key = |balloon: Option<u32>| {
            let mut args = test_args();
            args.balloon = balloon;
            key_for(&args, GuestBootInputs::default())
        };
        assert_ne!(
            key(None),
            key(Some(0)),
            "a snapshot taken with no balloon device must not serve --balloon 0"
        );
        assert_eq!(
            key(Some(0)),
            key(Some(1024)),
            "runs that differ only in the balloon target must share a snapshot"
        );
    }

    /// A startup snapshot holds a workload that initialised under one balloon
    /// target, and a restore cannot replay that. Two targets share the pre-start
    /// snapshot and have a startup snapshot each. A run without --balloon keeps
    /// both names it had.
    #[test]
    fn two_balloon_targets_share_the_pre_start_snapshot_and_not_the_startup_one() {
        let names = |balloon: Option<u32>| {
            let mut args = test_args();
            args.balloon = balloon;
            let key = key_for(&args, GuestBootInputs::default());
            let startup = startup_snapshot_key(&key, args.balloon, None);
            (key, startup)
        };
        let (key_64, startup_64) = names(Some(64));
        let (key_96, startup_96) = names(Some(96));
        assert_eq!(
            key_64, key_96,
            "runs that differ only in the balloon target share the pre-start snapshot"
        );
        assert_ne!(
            startup_64, startup_96,
            "a 96 MiB run must not find the startup snapshot a 64 MiB run left"
        );
        assert_eq!(startup_64, format!("{key_64}-balloon64-startup"));
        assert_eq!(startup_96, format!("{key_64}-balloon96-startup"));
        // A device at target 0 is a target like any other.
        assert_eq!(names(Some(0)).1, format!("{key_64}-balloon0-startup"));

        let (key_none, startup_none) = names(None);
        assert_ne!(key_none, key_64);
        assert_eq!(
            startup_none,
            format!("{key_none}-startup"),
            "a run without --balloon keeps the startup name it had"
        );
        assert_eq!(
            startup_snapshot_key("abc123def456", None, None),
            "abc123def456-startup"
        );
    }

    /// A startup snapshot holds a container that runs with the host identity
    /// address in its environment, which a restore cannot change, so runs with
    /// different addresses share the pre-start snapshot and have a startup
    /// snapshot each. A run without one keeps the startup name it had.
    #[test]
    fn two_host_identities_share_the_pre_start_snapshot_and_not_the_startup_one() {
        let names = |address: Option<&str>| {
            let mut args = test_args();
            args.env = address
                .map(|a| vec![format!("HOST_IPV6={a}")])
                .unwrap_or_default();
            let key = key_for(&args, GuestBootInputs::default());
            let startup = startup_snapshot_key(&key, args.balloon, run_host_ipv6(&args.env));
            (key, startup)
        };
        let (key_a, startup_a) = names(Some("2001:db8:a::1"));
        let (key_b, startup_b) = names(Some("2001:db8:b::1"));
        let (key_none, startup_none) = names(None);
        assert_eq!(
            key_a, key_b,
            "the pre-start snapshot is shared between hosts"
        );
        assert_eq!(key_a, key_none);
        assert_ne!(
            startup_a, startup_b,
            "a run on host B must not find the startup snapshot whose container runs as host A"
        );
        assert_ne!(startup_a, startup_none);
        assert_eq!(startup_none, format!("{key_none}-startup"));
        crate::storage::validate_snapshot_name(&startup_snapshot_key(
            &key_a,
            Some(u32::MAX),
            Some("2001:db8:a::1".parse().unwrap()),
        ))
        .expect("the longest startup name is a valid snapshot name");

        // Every caller names the startup snapshot for the run's own address.
        let code = |source: &str| -> String {
            source[..source.find("\n#[cfg(test)]\nmod tests {").unwrap()]
                .split_whitespace()
                .collect()
        };
        let podman = code(include_str!("mod.rs"));
        for call in [
            "startup_snapshot_key(&startup_base,args.balloon,run_host_ipv6(&args.env))",
            "startup_snapshot_key(base,ctx.args.balloon,run_host_ipv6(&ctx.args.env))",
        ] {
            assert!(podman.contains(call), "podman run no longer calls {call}");
        }
        assert_eq!(podman.matches("startup_snapshot_key(").count(), 2);
        assert!(
            code(include_str!("../snapshot.rs"))
                .contains("startup_snapshot_key(base_key,args.balloon,args.host_ipv6)"),
            "a clone saves the startup snapshot under the address it restored with"
        );
    }

    /// Both config builders give a read-only --disk-dir its content hash: the snapshot
    /// key changes with the directory's bytes, stays when the same content is recreated,
    /// and does not name the host path.
    #[test]
    fn the_snapshot_key_names_the_disk_dir_content() {
        use std::path::Path;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("data");
        std::fs::create_dir(&dir).unwrap();
        let resolved_key = |staging: &str| {
            let mut args = test_args();
            args.disk_dir = vec![format!("{}:/data:ro", dir.display())];
            let (_staging, resolved) = crate::storage::disk_dir::DiskDirStaging::resolve(
                &tmp.path().join(staging),
                &args.disk,
                &args.disk_dir,
            )
            .unwrap();
            args.resolved_extra_disks = Some(resolved);
            let launch = build_launch_config(
                &args,
                Path::new("/rootfs"),
                Path::new("/kernel"),
                Path::new("/initrd"),
                &None,
                &RuntimeConfig::default(),
                GuestBootInputs::default(),
                &[],
            );
            let key_config = build_firecracker_config(
                &args,
                "sha256:test",
                Path::new("/kernel"),
                Path::new("/rootfs"),
                Path::new("/initrd"),
                None,
                ImageMode::Overlay,
                None,
                None,
                None,
                None,
                GuestBootInputs::default(),
                &[],
            );
            assert_eq!(launch.extra_disks, key_config.extra_disks);
            assert!(
                !key_config.extra_disks[0].contains(&*dir.to_string_lossy()),
                "the key names the host path: {:?}",
                key_config.extra_disks
            );
            key_config.snapshot_key()
        };
        std::fs::write(dir.join("f"), b"one").unwrap();
        let one = resolved_key("s1");
        std::fs::remove_file(dir.join("f")).unwrap();
        std::fs::write(dir.join("f"), b"one").unwrap();
        assert_eq!(one, resolved_key("s2"), "recreated content changed the key");
        std::fs::write(dir.join("f"), b"two").unwrap();
        assert_ne!(one, resolved_key("s3"), "new content kept the key");
    }

    /// The device is attached from the launch config, at the target it carries.
    /// With snapshots off that config is built on its own. With snapshots on it is
    /// the key config, whose JSON leaves the target out. Both have to carry
    /// --balloon.
    #[test]
    fn the_launch_config_carries_the_balloon() {
        use crate::firecracker::BalloonDevice;
        use std::path::Path;
        for free_page_reporting in [false, true] {
            let mut args = test_args();
            args.balloon = Some(512);
            args.free_page_reporting = free_page_reporting;
            let device = Some(BalloonDevice {
                target_mib: 512,
                free_page_reporting,
            });
            let launch_config = build_launch_config(
                &args,
                Path::new("/rootfs"),
                Path::new("/kernel"),
                Path::new("/initrd"),
                &None,
                &RuntimeConfig::default(),
                GuestBootInputs::default(),
                &[],
            );
            assert_eq!(launch_config.balloon, device);
            let key_config = build_firecracker_config(
                &args,
                "sha256:test",
                Path::new("/kernel"),
                Path::new("/rootfs"),
                Path::new("/initrd"),
                None,
                ImageMode::Overlay,
                None,
                None,
                None,
                None,
                GuestBootInputs::default(),
                &[],
            );
            assert_eq!(key_config.balloon, device);
        }
    }

    /// A cache hit restores through `snapshot run`. The snapshot holds the balloon
    /// target of the run that made it, so the restore has to be told THIS run's.
    #[test]
    fn a_cache_restore_asks_for_this_runs_balloon_target() {
        let mut args = test_args();
        args.balloon = Some(512);
        let restore = snapshot_restore_args(&args, "k", None, (None, None));
        assert_eq!(restore.balloon, Some(512));

        // A run with no --balloon matched a snapshot with no device, and sets nothing.
        let plain = snapshot_restore_args(&test_args(), "k", None, (None, None));
        assert_eq!(plain.balloon, None);
    }

    /// The pre-start key leaves out the health-check URL (#953), so a cache hit can
    /// restore a snapshot that another run's check recorded, and no key holds the timeout.
    /// The restored VM has to monitor THIS run's check: its URL, or the container's start
    /// when it gave none, and its timeout.
    #[test]
    fn a_cache_restore_monitors_this_runs_health_check() {
        let recorded = (Some("http://localhost/status"), 30);

        let mut args = test_args();
        args.health_check = Some("http://localhost/ready".to_string());
        args.health_check_timeout = 7;
        let restore = snapshot_restore_args(&args, "k", None, (None, None));
        let monitored = crate::commands::snapshot::restore_health_check(
            restore.health_check.as_ref(),
            recorded.0,
            recorded.1,
        );
        assert_eq!(
            monitored,
            crate::cli::RestoreHealthCheck {
                url: Some("http://localhost/ready".to_string()),
                timeout_secs: 7,
            }
        );

        // A run without --health-check is healthy when its container starts, whatever the
        // snapshot's creator checked.
        let plain = snapshot_restore_args(&test_args(), "k", None, (None, None));
        let monitored = crate::commands::snapshot::restore_health_check(
            plain.health_check.as_ref(),
            recorded.0,
            recorded.1,
        );
        assert_eq!(monitored.url, None);
        assert_eq!(monitored.timeout_secs, test_args().health_check_timeout);

        // A command-line `snapshot run` hands over nothing and monitors the recorded check.
        let recorded_check =
            crate::commands::snapshot::restore_health_check(None, recorded.0, recorded.1);
        assert_eq!(
            recorded_check.url.as_deref(),
            Some("http://localhost/status")
        );
        assert_eq!(recorded_check.timeout_secs, 30);
    }

    /// The restore puts the check `restore_health_check` chose into the clone's state,
    /// which is what the health monitor reads. Taking the snapshot's recorded URL there
    /// instead would undo the test above for every restore.
    #[test]
    fn the_restore_records_the_health_check_it_was_handed() {
        let source = include_str!("../snapshot.rs");
        let body = &source[source.find("async fn cmd_snapshot_run_inner(").unwrap()..];
        let body = &body[..body.find("\n}\n").unwrap()];
        assert!(
            body.contains("restore_health_check(\n        args.health_check.as_ref(),"),
            "cmd_snapshot_run_inner does not choose the clone's health check with restore_health_check"
        );
        assert!(
            !body.contains("= snapshot_config.metadata.health_check_url"),
            "cmd_snapshot_run_inner takes the snapshot's recorded URL directly"
        );
    }

    /// A prepare whose snapshot cannot fit is refused before its VM exists (#968): it fails
    /// without the snapshot, so a refusal at snapshot time loses the whole boot, warm-up
    /// included. The check takes the free space as a number, so no small filesystem is
    /// needed. The numbers are the issue's: a 128 GiB guest, 80791 MiB free.
    #[test]
    fn a_snapshot_that_cannot_fit_is_refused_before_the_vm_boots() {
        let dir = Path::new("/mnt/fcvm-btrfs/snapshots");
        let need = crate::commands::common::snapshot_space_required(131072, false);
        assert_eq!(
            need,
            131072 * MIB,
            "a full snapshot needs all of guest memory"
        );
        assert_eq!(
            crate::commands::common::snapshot_space_required(131072, true),
            131072 * MIB / 10,
            "a diff snapshot needs a tenth of it, as at snapshot time"
        );

        let refused = refuse_unfittable_snapshot(
            "podman prepare's startup snapshot",
            need,
            Some(80791 * MIB),
            dir,
            None,
        )
        .expect_err("a prepare that cannot fit its snapshot must be refused");
        let text = format!("{refused:#}");
        assert!(
            text.contains("podman prepare's startup snapshot needs up to 131072 MiB on /mnt/fcvm-btrfs/snapshots")
                && text.contains("80791 MiB are free")
                && text.contains("no VM was started"),
            "{text}"
        );

        assert!(refuse_unfittable_snapshot("x", need, Some(need), dir, None).is_ok());
        // statvfs failed: nothing is refused, as at snapshot time.
        assert!(refuse_unfittable_snapshot("x", need, None, dir, None).is_ok());

        let replaced = ReplacedSnapshot {
            name: "warm-tested".to_string(),
            bytes: 53248 * MIB,
        };
        let text = format!(
            "{:#}",
            refuse_unfittable_snapshot("x", need, Some(MIB), dir, Some(&replaced)).unwrap_err()
        );
        assert!(
            text.contains("The snapshot warm-tested that this replaces holds up to 53248 MiB"),
            "{text}"
        );
    }

    /// The check runs before any VM process exists: on a pre-start hit before the restore,
    /// and on a miss before the VM's directory or processes are made. A prepare checks on
    /// both paths, a --gpu run on a miss (#968).
    #[test]
    fn snapshot_space_is_checked_before_the_vm_is_started() {
        let source = include_str!("mod.rs");
        let body = &source[source.find("async fn prepare_vm_for_lifecycle(").unwrap()..];
        let body = &body[..body.find("\n}\n").unwrap()];
        let at = |needle: &str| {
            body.find(needle)
                .unwrap_or_else(|| panic!("no {needle:?} in prepare_vm_for_lifecycle"))
        };
        let checks: Vec<usize> = body
            .match_indices("check_snapshot_space_before_boot(")
            .map(|(at, _)| at)
            .collect();
        assert_eq!(
            checks.len(),
            3,
            "prepare_vm_for_lifecycle checks snapshot space at {checks:?}, not once on a \
             prepare's pre-start hit and twice on a miss (prepare, --gpu)"
        );
        let hit = at("\"Pre-start snapshot hit! Restoring from cached snapshot\"");
        let restore = hit + body[hit..].find("cmd_snapshot_run_attempt(").unwrap();
        assert!(
            hit < checks[0] && checks[0] < restore,
            "a prepare's pre-start hit restores at {restore} before it checks at {}",
            checks[0]
        );
        let miss = at("\"Snapshot miss, will create snapshot after image load\"");
        let data_dir = at(".context(\"creating VM data directory\")");
        let setup = at("run_vm_setup(");
        assert!(
            miss < checks[1] && checks[2] < data_dir && data_dir < setup,
            "a miss makes the VM's directory at {data_dir} and starts it at {setup}, \
             checks at {checks:?}"
        );
    }

    /// The check at snapshot time stays, because the VM's own disk and other writers take
    /// space during the boot. When it refuses a prepare, the refusal says how far free space
    /// fell since the prepare started and what the snapshot it replaces still holds (#968).
    #[test]
    fn the_snapshot_time_refusal_says_how_far_free_space_fell() {
        let shortfall = crate::commands::common::SnapshotSpaceShortfall {
            kind: "full",
            required_bytes: 131072 * MIB,
            available_bytes: 80791 * MIB,
            path: "/mnt/fcvm-btrfs/snapshots/warm-tested".into(),
        };
        // The text log searches and the snapshot code always printed.
        assert_eq!(
            shortfall.to_string(),
            "Not enough disk space for full snapshot: need 131072 MiB, have 80791 MiB free on \
             /mnt/fcvm-btrfs/snapshots/warm-tested. Use --mem to limit VM memory, or increase \
             btrfs_size in rootfs-config.toml."
        );
        let replaced = ReplacedSnapshot {
            name: "warm-tested".to_string(),
            bytes: 53248 * MIB,
        };
        let error = anyhow::Error::from(shortfall).context("creating podman snapshot");
        let text = format!(
            "{:#}",
            explain_snapshot_space_shortfall(error, Some(136192 * MIB), Some(&replaced))
        );
        assert!(
            text.contains("free space fell from 136192 MiB when this prepare started to 80791 MiB")
                && text.contains("took 55401 MiB"),
            "{text}"
        );
        assert!(
            text.contains("the snapshot warm-tested that this replaces holds up to 53248 MiB"),
            "{text}"
        );
        assert!(
            text.contains("Not enough disk space for full snapshot"),
            "{text}"
        );

        let other = explain_snapshot_space_shortfall(
            anyhow::anyhow!("pausing the VM failed"),
            Some(MIB),
            Some(&replaced),
        );
        assert_eq!(format!("{other:#}"), "pausing the VM failed");

        // The snapshot code refuses with the type the explanation looks for.
        let common = include_str!("../common.rs");
        let core = &common[common.find("pub async fn create_snapshot_core(").unwrap()..];
        let core = &core[..core.find("\n}\n").unwrap()];
        assert!(
            core.contains("return Err(SnapshotSpaceShortfall {"),
            "create_snapshot_core does not refuse with SnapshotSpaceShortfall"
        );
    }

    /// A prepare boots from a pre-start snapshot it finds, as a run does: the snapshot is
    /// taken before the container starts, so the image import it holds is shared (#953).
    /// `--force` cold-boots, because it is for content that changed under an unchanged key,
    /// which the pre-start snapshot holds too.
    #[test]
    fn a_prepare_restores_the_pre_start_snapshot_unless_forced() {
        assert!(restores_pre_start(&PodmanLifecycle::Run));
        assert!(restores_pre_start(&PodmanLifecycle::Prepare(
            PrepareOptions::default()
        )));
        assert!(!restores_pre_start(&PodmanLifecycle::Prepare(
            PrepareOptions {
                force: true,
                ..PrepareOptions::default()
            }
        )));
    }

    /// A balloon target above the guest's memory is refused before the snapshot
    /// cache is looked up, so the run fails the same way on a hit and on a miss.
    /// Firecracker refuses it on both paths, but only once the network and the VMM
    /// exist.
    #[test]
    fn a_balloon_target_above_the_guests_memory_is_refused() {
        assert!(validate_balloon_target(None, 1024).is_ok());
        assert!(validate_balloon_target(Some(0), 1024).is_ok());
        assert!(validate_balloon_target(Some(1024), 1024).is_ok());
        let error = validate_balloon_target(Some(1025), 1024)
            .expect_err("a 1025 MiB balloon in a 1024 MiB guest was accepted")
            .to_string();
        assert!(
            error.contains("--balloon 1025") && error.contains("--mem 1024"),
            "{error}"
        );

        let source = include_str!("mod.rs");
        let start = source
            .find("async fn prepare_vm_for_lifecycle(")
            .expect("no prepare_vm_for_lifecycle");
        // The function ends at the first closing brace in column one.
        let end = source[start..]
            .find("\n}\n")
            .expect("prepare_vm_for_lifecycle has no end");
        let prepare = &source[start..start + end];
        let checked = prepare
            .find("validate_balloon_target(args.balloon, args.mem)?;")
            .expect("the balloon target is never checked");
        let looked_up = prepare
            .find("check_podman_snapshot(")
            .expect("no snapshot cache lookup");
        assert!(
            checked < looked_up,
            "prepare_vm_for_lifecycle looks the snapshot cache up before it checks the balloon target"
        );
        let switch_checked = prepare
            .find("validate_free_page_reporting(&args)?;")
            .expect("--free-page-reporting is never checked");
        assert!(
            switch_checked < looked_up,
            "prepare_vm_for_lifecycle looks the snapshot cache up before it checks --free-page-reporting"
        );
    }

    /// `--free-page-reporting` is refused by name where fcvm cannot turn it on: with
    /// no balloon device, with hugepages and on Cloud Hypervisor. The refusal runs
    /// before the snapshot cache is looked up (pinned in the test above), so a hit
    /// and a miss refuse alike, and it covers RunArgs that clap never parsed.
    #[test]
    fn free_page_reporting_is_refused_without_a_balloon_with_hugepages_and_on_cloud_hypervisor() {
        type Change = fn(&mut RunArgs);
        let with = |change: Change| {
            let mut args = test_args();
            args.balloon = Some(0);
            args.free_page_reporting = true;
            change(&mut args);
            args
        };
        validate_free_page_reporting(&with(|_| {}))
            .expect("a Firecracker VM with a balloon device takes the switch");

        let refusals: [(Change, &str); 3] = [
            (|args| args.balloon = None, "--balloon"),
            (|args| args.hugepages = true, "--hugepages"),
            (
                |args| args.hypervisor = crate::cli::args::Hypervisor::CloudHypervisor,
                "cloud-hypervisor",
            ),
        ];
        for (change, names) in refusals {
            let error = validate_free_page_reporting(&with(change))
                .expect_err(&format!("the switch was accepted with {names}"))
                .to_string();
            assert!(
                error.contains("--free-page-reporting") && error.contains(names),
                "{names}: {error}"
            );

            // Without the switch the same run is none of this function's business.
            let mut plain = with(change);
            plain.free_page_reporting = false;
            validate_free_page_reporting(&plain).expect("a run without the switch");
        }
    }

    /// A VM cold-booted with --free-page-reporting has its balloon checked before
    /// this process takes a snapshot of it: when the guest's agent first asks for
    /// its cache verdict, which is before the pre-start snapshot, again before the
    /// startup snapshot in case that ask never came, and in `podman prepare` between
    /// its health wait and its snapshot. A --no-snapshot run takes no snapshot of
    /// its own, but `fcvm snapshot create` can, so its agent's ask reaches the run
    /// loop too. No fake VMM drives these loops, so the order is pinned by source.
    #[test]
    fn a_cold_boot_with_free_page_reporting_checks_the_balloon_before_any_snapshot() {
        let source = include_str!("mod.rs");
        let code = &source[..source.find("\n#[cfg(test)]\nmod tests {").unwrap()];
        let body_of = |function: &str| {
            let from = code
                .find(function)
                .unwrap_or_else(|| panic!("no {function}"));
            let body = &code[from..];
            &body[..body.find("\n}\n").unwrap()]
        };
        let position = |body: &str, needle: &str| {
            body.find(needle)
                .unwrap_or_else(|| panic!("no {needle:?} in the function"))
        };
        let check = "settle_balloon_check(ctx).await";

        let run_loop = body_of("pub async fn run_vm_loop");
        // The run loop races each check with cancellation (#1075), as a select! arm.
        let raced = "settled = settle_balloon_check(ctx) =>";
        let checks: Vec<usize> = run_loop.match_indices(raced).map(|(at, _)| at).collect();
        assert_eq!(
            checks.len(),
            2,
            "run_vm_loop checks the balloon at {checks:?}, not once at the agent's ask and \
             once before the startup snapshot"
        );
        let asked = position(run_loop, "Some(cache_request) = async {");
        let pre_start = position(run_loop, "\"Creating pre-start snapshot\"");
        let healthy = position(run_loop, "Ok(startup_ack) = async {");
        let startup = position(run_loop, "\"Creating startup snapshot (VM healthy)\"");
        assert!(
            asked < checks[0] && checks[0] < pre_start,
            "the pre-start snapshot is not behind the check: ask at {asked}, check at {}, \
             snapshot at {pre_start}",
            checks[0]
        );
        assert!(
            healthy < checks[1] && checks[1] < startup,
            "the startup snapshot is not behind the check: healthy at {healthy}, check at {}, \
             snapshot at {startup}",
            checks[1]
        );

        let prepare = body_of("async fn run_prepare_loop(");
        let waited = position(prepare, "await_prepare_healthy(");
        let checked = position(prepare, check);
        let saved = position(prepare, "install_prepared_startup_snapshot(");
        assert!(
            waited < checked && checked < saved,
            "podman prepare's snapshot is not behind the check: health wait at {waited}, check \
             at {checked}, snapshot at {saved}"
        );

        // The ask reaches the run loop whenever a snapshot may be taken,
        // and on a run that takes none when the check is due. `podman prepare` has
        // no run loop to hand it to.
        let run = PodmanLifecycle::Run;
        let prepare = PodmanLifecycle::Prepare(PrepareOptions::default());
        assert!(ask_reaches_run_loop(false, false, &run));
        assert!(ask_reaches_run_loop(false, true, &run));
        assert!(!ask_reaches_run_loop(true, false, &run));
        assert!(
            ask_reaches_run_loop(true, true, &run),
            "a --no-snapshot run with --free-page-reporting never hears that its guest is up"
        );
        assert!(!ask_reaches_run_loop(false, true, &prepare));
        assert!(!ask_reaches_run_loop(true, true, &prepare));
    }

    /// `podman run` resolves the --pmem specs into the store once, after every check that
    /// can refuse the run and before the snapshot key or the attach reads them, so a
    /// refused run copies nothing and the key and the attach name the same generation.
    #[test]
    fn podman_run_resolves_the_pmem_specs_once() {
        let source = include_str!("mod.rs");
        let body = &source[..source
            .find("\n#[cfg(test)]\nmod tests {")
            .expect("mod.rs has no test module")];
        let call = "resolve_pmem_into_store(&mut args, &data_dir)";
        assert_eq!(
            body.matches(call).count(),
            1,
            "podman run no longer replaces its --pmem specs with store entries"
        );
        let at = body.find(call).unwrap();
        for check in [
            "\"--setup is not allowed when running as root",
            "\"invalid --user format",
            "get_kernel_profile(effective_profile_name)?",
            "\"Custom kernel not found",
            "get_image_cache_ref(&args.image).await?",
        ] {
            assert_eq!(body.matches(check).count(), 1, "{check}");
            assert!(
                body.find(check).unwrap() < at,
                "the --pmem images are copied before {check} can refuse the run"
            );
        }
        let config = body
            .find("let config = build_firecracker_config(")
            .expect("podman run no longer builds the snapshot key's config");
        assert!(
            at < config,
            "the snapshot key is built before the --pmem specs name store entries"
        );
    }

    /// The snapshot key names the image generation a --pmem spec resolves to: a symlink
    /// and its target share a key, a rewrite of the image changes it, and so does
    /// repointing the symlink to another image, so a cache hit never restores a snapshot
    /// of another generation.
    #[test]
    fn the_snapshot_key_names_the_pmem_image_generation() {
        let dir = tempfile::tempdir_in(crate::storage::pmem_store::test_image_dir()).unwrap();
        let data = tempfile::tempdir().unwrap();
        let (a, b, link) = (
            dir.path().join("a.ext4"),
            dir.path().join("b.ext4"),
            dir.path().join("current.ext4"),
        );
        for image in [&a, &b] {
            let file = std::fs::File::create(image).unwrap();
            file.set_len(2 << 20).unwrap();
            // A modification time long past, so the rewrite below gives the image another
            // identity at any timestamp granularity.
            file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1 << 30))
                .unwrap();
        }
        std::os::unix::fs::symlink(&a, &link).unwrap();
        let key = |image: &std::path::Path| {
            let mut args = test_args();
            args.pmem = vec![format!("{}:/mnt/cache:ro", image.display())];
            resolve_pmem_into_store(&mut args, data.path()).unwrap();
            key_for(&args, GuestBootInputs::default())
        };
        let through_link = key(&link);
        assert_eq!(
            through_link,
            key(&a),
            "a symlink and its target are one image"
        );
        std::fs::write(&a, vec![7u8; 2 << 20]).unwrap();
        let rewritten = key(&a);
        assert_ne!(
            rewritten, through_link,
            "the key did not change with the image generation"
        );
        assert_eq!(key(&link), rewritten, "the same generation has one key");
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&b, &link).unwrap();
        assert_ne!(key(&link), rewritten, "the symlink names another image now");
    }

    /// #1097: the snapshot key names the bytes of a --pmem image, not its path or its file
    /// identity. Two paths holding the same bytes, one of them a hard link, share a key and
    /// a store entry, and so does an image touched without changing a byte; changing one
    /// byte changes the key.
    #[test]
    fn the_snapshot_key_names_the_pmem_image_content() {
        let dir = tempfile::tempdir_in(crate::storage::pmem_store::test_image_dir()).unwrap();
        let data = tempfile::tempdir().unwrap();
        let (a, b, link) = (
            dir.path().join("a.ext4"),
            dir.path().join("b.ext4"),
            dir.path().join("link.ext4"),
        );
        let mut bytes = vec![0u8; 2 << 20];
        bytes[..4096].fill(0x5a);
        bytes[1 << 20..(1 << 20) + 4096].fill(0xa5);
        std::fs::write(&a, &bytes).unwrap();
        std::fs::write(&b, &bytes).unwrap();
        std::fs::hard_link(&a, &link).unwrap();
        let resolve = |image: &std::path::Path| {
            let mut args = test_args();
            args.pmem = vec![format!("{}:/mnt/cache:ro", image.display())];
            resolve_pmem_into_store(&mut args, data.path()).unwrap();
            (
                key_for(&args, GuestBootInputs::default()),
                args.pmem_devices[0].path.clone(),
            )
        };
        let (key, entry) = resolve(&a);
        assert_eq!(
            resolve(&b),
            (key.clone(), entry.clone()),
            "two copies of one image"
        );
        assert_eq!(resolve(&link), (key.clone(), entry.clone()), "a hard link");
        std::fs::File::options()
            .write(true)
            .open(&a)
            .unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
            .unwrap();
        assert_eq!(resolve(&a), (key.clone(), entry.clone()), "a touched image");
        bytes[4095] ^= 1;
        std::fs::write(&b, &bytes).unwrap();
        let (changed, changed_entry) = resolve(&b);
        assert_ne!(changed, key, "one byte changed and the key did not");
        assert_ne!(changed_entry, entry);
    }

    /// A mount point at or under a --pmem mount point is refused before boot: the pmem
    /// image is a read-only filesystem mounted after the extra disks, so it would hide
    /// the disk, or the mount point could not be created on it. A sibling path is not
    /// under it.
    #[test]
    fn a_mount_point_under_a_pmem_mount_point_is_refused() {
        let mut args = test_args();
        args.pmem = vec!["/images/cache.ext4:/mnt/cache:ro".to_string()];
        args.disk = vec!["/images/scratch.raw:/mnt/cache/scratch".to_string()];
        let error = checked_volume_mappings(&args, false)
            .err()
            .expect("a disk under a pmem mount point was accepted")
            .to_string();
        for named in [
            "--disk /images/scratch.raw:/mnt/cache/scratch",
            "--pmem /images/cache.ext4:/mnt/cache:ro",
        ] {
            assert!(
                error.contains(named),
                "the error does not name {named}: {error}"
            );
        }
        args.disk = vec!["/images/scratch.raw:/mnt/cachex".to_string()];
        checked_volume_mappings(&args, false)
            .expect("a sibling of the pmem mount point is not under it");
    }

    /// fc-agent mounts the NFS shares after the pmem devices, so a share at or above a pmem
    /// mount point would hide the image. A disk there is mounted before the image and is
    /// not refused.
    #[test]
    fn an_nfs_share_above_a_pmem_mount_point_is_refused() {
        let mut args = test_args();
        args.pmem = vec!["/images/cache.ext4:/mnt/data:ro".to_string()];
        args.nfs = vec!["/shares/a:/mnt".to_string()];
        let error = checked_volume_mappings(&args, false)
            .err()
            .expect("an NFS share above a pmem mount point was accepted")
            .to_string();
        for named in [
            "--nfs /shares/a:/mnt",
            "--pmem /images/cache.ext4:/mnt/data:ro",
        ] {
            assert!(
                error.contains(named),
                "the error does not name {named}: {error}"
            );
        }
        args.nfs.clear();
        args.disk = vec!["/images/scratch.raw:/mnt".to_string()];
        checked_volume_mappings(&args, false)
            .expect("a disk above a pmem mount point is mounted before it");
    }

    /// #821 helper: snapshot key of a config built through the real
    /// constructor from the given guest boot inputs. The host-derived DNS
    /// values and the FUSE knobs are guest-visible boot inputs, baked into
    /// the guest at boot and therefore into any snapshot taken from it, so
    /// two runs differing only in one of them must never share a snapshot.
    /// Before FirecrackerConfig carried these fields the keys came out equal
    /// and a cache hit silently served a guest built with the other value.
    fn key_for(args: &RunArgs, inputs: GuestBootInputs) -> String {
        use std::path::Path;
        let volume_mappings = parsed_maps(args);
        build_firecracker_config(
            args,
            "sha256:test",
            Path::new("/kernel"),
            Path::new("/rootfs"),
            Path::new("/initrd"),
            None,
            ImageMode::Overlay,
            None,
            None,
            None,
            None,
            inputs,
            &volume_mappings,
        )
        .snapshot_key()
    }

    /// The `--map` arguments of `args`, parsed. Their host paths have to exist.
    fn parsed_maps(args: &RunArgs) -> Vec<VolumeMapping> {
        args.map
            .iter()
            .map(|spec| VolumeMapping::parse(spec).unwrap())
            .collect()
    }

    /// The kernel command line a cold boot of `args` gets from fcvm.
    fn boot_args_for(args: &RunArgs, inputs: GuestBootInputs) -> String {
        use std::path::Path;
        let launch_config = build_launch_config(
            args,
            Path::new("/rootfs"),
            Path::new("/kernel"),
            Path::new("/initrd"),
            &None,
            &RuntimeConfig::default(),
            inputs,
            &parsed_maps(args),
        );
        build_runtime_boot_args(&crate::network::NetworkConfig::default(), &launch_config)
    }

    fn boot_inputs_key(inputs: GuestBootInputs) -> String {
        key_for(&test_args(), inputs)
    }

    /// One resolv.conf source's contribution: the servers it named and the
    /// search domains that belong to them. Grouped, because a narrowing mode
    /// drops a server together with its domains.
    fn resolvers(servers: &[&str], search: &[&str]) -> GuestBootInputs {
        GuestBootInputs {
            dns: vec![crate::network::ResolverGroup {
                servers: servers.iter().map(|s| s.to_string()).collect(),
                search_domains: search.iter().map(|s| s.to_string()).collect(),
            }],
            ..Default::default()
        }
    }

    /// #863: bridged mode forwards exactly one resolver to the guest
    /// (network_config.dns_server is the first host entry), so the key must
    /// hash exactly that. Keying the whole host list cold-boots a guest whose
    /// cmdline is identical whenever a secondary nameserver changes. Rootless
    /// mode emits the whole list, so every entry must keep keying there.
    #[test]
    fn bridged_mode_keys_only_the_emitted_dns() {
        let dns = |servers: &[&str]| resolvers(servers, &[]);
        let bridged = |servers: &[&str]| {
            let mut args = test_args();
            args.network = NetworkMode::Bridged;
            key_for(&args, dns(servers))
        };
        assert_eq!(
            bridged(&["1.1.1.1", "8.8.8.8"]),
            bridged(&["1.1.1.1", "9.9.9.9"]),
            "a secondary nameserver never reaches a bridged guest and must not change its key"
        );
        assert_ne!(
            bridged(&["1.1.1.1", "8.8.8.8"]),
            bridged(&["8.8.8.8", "8.8.8.8"]),
            "the first resolver IS emitted to a bridged guest and must change the key"
        );
        assert_ne!(
            boot_inputs_key(dns(&["1.1.1.1", "8.8.8.8"])),
            boot_inputs_key(dns(&["1.1.1.1", "9.9.9.9"])),
            "rootless mode emits the whole list, so a secondary change must still key"
        );
    }

    /// Routed forwards one resolver as well: the first IPv6 nameserver the
    /// sources name (src/network/routed.rs, detect_ipv6_dns). An IPv4
    /// secondary never reaches a routed guest, so it must not key; the
    /// selected IPv6 server does reach it, so it must.
    ///
    /// This narrowed what routed hashes, so a routed snapshot key computed
    /// before it changes value once and those cached snapshots are rebuilt.
    /// The steady state shares more: hosts differing only in a resolver the
    /// routed guest never sees now hit the same key.
    #[test]
    fn routed_mode_keys_only_the_emitted_dns() {
        let routed = |groups: Vec<crate::network::ResolverGroup>| {
            let mut args = test_args();
            args.network = NetworkMode::Routed;
            key_for(
                &args,
                GuestBootInputs {
                    dns: groups,
                    ..Default::default()
                },
            )
        };
        let two = |ipv6: &str, ipv4: &str| {
            vec![
                crate::network::ResolverGroup {
                    servers: vec![ipv4.to_string()],
                    search_domains: vec!["corp.example".to_string()],
                },
                crate::network::ResolverGroup {
                    servers: vec![ipv6.to_string()],
                    search_domains: vec!["lab.example".to_string()],
                },
            ]
        };

        assert_eq!(
            routed(two("2001:db8::53", "10.1.0.2")),
            routed(two("2001:db8::53", "192.0.2.53")),
            "an IPv4 resolver never reaches a routed guest and must not change its key"
        );
        assert_ne!(
            routed(two("2001:db8::53", "10.1.0.2")),
            routed(two("2001:db8::54", "10.1.0.2")),
            "the selected IPv6 resolver IS emitted and must change the key"
        );
    }

    /// #863: with no --map, fc-agent never calls mount_fuse_volumes
    /// (fc-agent/src/agent.rs gates it on plan.volumes, and the four knobs are
    /// read only inside fc-agent/src/fuse/mod.rs's mount path), so the knobs
    /// cannot affect a volume-free guest and must not fragment its key.
    #[test]
    fn fuse_knobs_do_not_fragment_volume_free_keys() {
        let knobs = || GuestBootInputs {
            fuse_readers: Some("8".to_string()),
            fuse_trace_rate: Some("100".to_string()),
            fuse_max_write: Some("32768".to_string()),
            no_writeback_cache: true,
            ..Default::default()
        };
        assert_eq!(
            boot_inputs_key(GuestBootInputs::default()),
            boot_inputs_key(knobs()),
            "FUSE knobs are dormant without volumes and must not change the key"
        );
        assert_ne!(
            key_for(&fuse_volume_args(), GuestBootInputs::default()),
            key_for(&fuse_volume_args(), knobs()),
            "with a volume mapped the knobs shape the guest's FUSE mounts and must key"
        );
    }

    /// Args with a FUSE volume mapped: the four FUSE knobs only reach the
    /// guest's mount path when the run maps volumes.
    fn fuse_volume_args() -> RunArgs {
        let mut args = test_args();
        args.map = vec!["/tmp:/data".to_string()];
        args
    }

    #[test]
    fn host_dns_fallback_changes_snapshot_key() {
        let dns = |servers: &[&str]| resolvers(servers, &[]);
        assert_ne!(
            boot_inputs_key(GuestBootInputs::default()),
            boot_inputs_key(dns(&["1.1.1.1"])),
            "host DNS fallback must change the key"
        );
        assert_ne!(
            boot_inputs_key(dns(&["1.1.1.1"])),
            boot_inputs_key(dns(&["8.8.8.8"])),
            "hosts with different resolv.conf servers must not share a snapshot"
        );
    }

    #[test]
    fn dns_search_changes_snapshot_key() {
        // Same resolver on both sides, so only the search domains vary and the
        // assertion cannot pass on a difference in the server list.
        assert_ne!(
            boot_inputs_key(resolvers(&["1.1.1.1"], &[])),
            boot_inputs_key(resolvers(&["1.1.1.1"], &["corp.example"])),
            "DNS search domains must change the key"
        );
    }

    #[test]
    fn fuse_readers_changes_snapshot_key() {
        assert_ne!(
            key_for(
                &fuse_volume_args(),
                GuestBootInputs {
                    fuse_readers: Some("8".to_string()),
                    ..Default::default()
                }
            ),
            key_for(
                &fuse_volume_args(),
                GuestBootInputs {
                    fuse_readers: Some("64".to_string()),
                    ..Default::default()
                }
            ),
            "fuse_readers must change the key of a run with volumes"
        );
    }

    #[test]
    fn fuse_trace_rate_changes_snapshot_key() {
        assert_ne!(
            key_for(&fuse_volume_args(), GuestBootInputs::default()),
            key_for(
                &fuse_volume_args(),
                GuestBootInputs {
                    fuse_trace_rate: Some("100".to_string()),
                    ..Default::default()
                }
            ),
            "fuse_trace_rate must change the key of a run with volumes"
        );
    }

    #[test]
    fn fuse_max_write_changes_snapshot_key() {
        assert_ne!(
            key_for(&fuse_volume_args(), GuestBootInputs::default()),
            key_for(
                &fuse_volume_args(),
                GuestBootInputs {
                    fuse_max_write: Some("32768".to_string()),
                    ..Default::default()
                }
            ),
            "fuse_max_write must change the key of a run with volumes"
        );
    }

    #[test]
    fn no_writeback_cache_changes_snapshot_key() {
        assert_ne!(
            key_for(&fuse_volume_args(), GuestBootInputs::default()),
            key_for(
                &fuse_volume_args(),
                GuestBootInputs {
                    no_writeback_cache: true,
                    ..Default::default()
                }
            ),
            "no_writeback_cache must change the key of a run with volumes"
        );
    }

    /// A read-only volume is never mounted with the writeback cache
    /// (fuse_pipe::MountSettings::for_volume), so FCVM_NO_WRITEBACK_CACHE
    /// changes nothing in a guest whose volumes are all read-only. Such a run
    /// gets the same snapshot key and the same kernel command line with and
    /// without it. One read-write volume among them and the switch counts.
    ///
    /// RED BEFORE THE FIX: for_launch dropped the switch only for a run with
    /// no volume at all, so two runs that boot the same guest had two keys and
    /// one of them carried a no_writeback_cache=1 that nothing acted on.
    #[test]
    fn no_writeback_cache_does_not_fragment_keys_of_runs_with_only_read_only_maps() {
        let switch = || GuestBootInputs {
            no_writeback_cache: true,
            ..Default::default()
        };
        let run = |maps: &[&str]| {
            let mut args = test_args();
            args.map = maps.iter().map(|map| map.to_string()).collect();
            args
        };

        let read_only = run(&["/tmp:/data:ro", "/tmp:/more:ro"]);
        assert_eq!(
            key_for(&read_only, GuestBootInputs::default()),
            key_for(&read_only, switch()),
            "the switch changed the key of a run whose maps are all read-only"
        );
        assert_eq!(
            boot_args_for(&read_only, GuestBootInputs::default()),
            boot_args_for(&read_only, switch()),
            "the switch changed the kernel command line of a run whose maps are all read-only"
        );

        let mixed = run(&["/tmp:/data:ro", "/tmp:/more"]);
        assert_ne!(
            key_for(&mixed, GuestBootInputs::default()),
            key_for(&mixed, switch()),
            "the switch must change the key of a run with a read-write map"
        );
        let with_switch = boot_args_for(&mixed, switch());
        assert!(
            with_switch.contains("no_writeback_cache=1")
                && !boot_args_for(&mixed, GuestBootInputs::default())
                    .contains("no_writeback_cache"),
            "the switch must reach a guest with a read-write map: {with_switch:?}"
        );

        // The other FUSE knobs shape a read-only mount too and still key it.
        assert_ne!(
            key_for(&read_only, GuestBootInputs::default()),
            key_for(
                &read_only,
                GuestBootInputs {
                    fuse_readers: Some("8".to_string()),
                    ..Default::default()
                }
            ),
            "fuse_readers must change the key of a run with read-only maps"
        );
    }

    /// A read-only map around /mnt/image-store is refused when the run makes
    /// fc-agent mount the image store there and the map's host directory has
    /// no image-store, and only then.
    ///
    /// RED BEFORE THE FIX: the host check knew the mount points of the
    /// arguments only. A localhost/ image with `--map DIR:/mnt:ro` booted and
    /// fc-agent failed in the guest on mkdir /mnt/image-store with EROFS.
    #[test]
    fn a_read_only_map_around_the_image_store_is_refused_when_the_run_mounts_one() {
        let host = tempfile::tempdir().unwrap();
        let map = format!("{}:/mnt:ro", host.path().display());
        let run = |image: &str, mode: Option<CliImageMode>| {
            let mut args = test_args();
            args.image = image.to_string();
            args.image_mode = mode;
            args.map = vec![map.clone()];
            args
        };
        let check = |args: &RunArgs| {
            types::checked_volume_mappings(args, attaches_image_disk(args)).map(|_| ())
        };

        let localhost = run("localhost/app:latest", None);
        let error = check(&localhost)
            .expect_err("a run that mounts the image store inside a read-only map was accepted")
            .to_string();
        let missing = host.path().join("image-store");
        for named in [
            "fcvm mounts the image store of localhost/app:latest in the guest: /mnt/image-store"
                .to_string(),
            format!("--map {map}"),
            missing.display().to_string(),
        ] {
            assert!(
                error.contains(&named),
                "the error does not name {named}: {error}"
            );
        }

        // A registry image is pulled in the guest: no image disk, no image store.
        check(&run("nginx:alpine", None)).expect("a registry image mounts no image store");
        // Btrfs and archive modes load the image from the disk and mount nothing there.
        for mode in [CliImageMode::Btrfs, CliImageMode::Archive] {
            check(&run("localhost/app:latest", Some(mode)))
                .expect("only overlay mode mounts the image store");
        }
        // A disk-only clone mounts the image store when its snapshot recorded an image disk.
        let mut clone = run("localhost/app:latest", None);
        clone.rootfs_override = Some(PathBuf::from("disk.raw"));
        check(&clone).expect("a clone without an image disk mounts no image store");
        clone.image_disk_override = Some(PathBuf::from("store.img"));
        check(&clone).expect_err("a clone with an image disk mounts the image store");

        std::fs::create_dir(&missing).unwrap();
        check(&localhost).expect("the map's host directory has image-store");
    }

    /// Every boot of a guest goes through configure_and_boot_vm, and each of
    /// its three callers has the maps checked first: the first boot in
    /// prepare_vm_for_lifecycle, before any asset, the snapshot cache or the VM
    /// is touched, and each relaunch after a guest reboot before the new VMM
    /// is spawned. No fake hypervisor drives these paths, so the wiring is
    /// pinned by its source.
    #[test]
    fn every_boot_has_its_maps_checked_first() {
        // Put together here so this test's own text is not a call site.
        let boot = ["configure_and_boot_vm", "("].concat();
        let check = "checked_volume_mappings(";
        let source = |file: &str| {
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(file);
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        };
        let body_of = |text: &str, function: &str| -> String {
            let from = text
                .find(function)
                .unwrap_or_else(|| panic!("no {function}"));
            let body = &text[from..];
            body[..body.find("\n}\n").unwrap()].to_string()
        };
        let position = |text: &str, needle: &str| {
            text.find(needle)
                .unwrap_or_else(|| panic!("no {needle:?} in the function"))
        };

        // The call sites, by file. A definition is not one.
        let mut call_sites: Vec<(String, usize)> = Vec::new();
        let mut pending = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    let calls =
                        text.matches(&boot).count() - text.matches(&format!("fn {boot}")).count();
                    if calls > 0 {
                        let file = path.strip_prefix(env!("CARGO_MANIFEST_DIR")).unwrap();
                        call_sites.push((file.display().to_string(), calls));
                    }
                }
            }
        }
        call_sites.sort();
        assert_eq!(
            call_sites,
            [
                ("src/commands/podman/mod.rs".to_string(), 1),
                ("src/commands/podman/vm_config.rs".to_string(), 1),
                ("src/commands/snapshot.rs".to_string(), 1),
            ],
            "a new caller boots a guest: check its maps first and pin it here"
        );

        // First boot: run_vm_setup_inner boots, and the one way into it starts
        // in prepare_vm_for_lifecycle, which checks before anything else.
        let vm_config = source("src/commands/podman/vm_config.rs");
        assert!(body_of(&vm_config, "async fn run_vm_setup_inner(").contains(&boot));
        let podman = source("src/commands/podman/mod.rs");
        // Without the tests, whose text names what it looks for.
        let podman = &podman[..podman.find("\n#[cfg(test)]\nmod tests {").unwrap()];
        assert_eq!(podman.matches("= run_vm_setup(").count(), 1);
        let prepare = body_of(podman, "async fn prepare_vm_for_lifecycle(");
        let checked = position(&prepare, check);
        for later in [
            "ensure_kernel(",
            "ensure_rootfs(",
            "get_image_cache_ref(",
            "check_podman_snapshot(",
            "generate_vm_id()",
            "= run_vm_setup(",
        ] {
            assert!(
                checked < position(&prepare, later),
                "prepare_vm_for_lifecycle reaches {later} before it checks the maps"
            );
        }

        // Relaunch of a podman run VM.
        let run_loop = body_of(podman, "pub async fn run_vm_loop");
        let relaunch = position(&run_loop, "guest rebooted — relaunching VM in place");
        let checked = position(&run_loop, check);
        let spawned = position(&run_loop, ".spawn(&relaunch_spec)");
        assert!(
            relaunch < checked && checked < spawned && spawned < position(&run_loop, &boot),
            "the reboot relaunch does not check the maps before it spawns the VMM"
        );

        // Relaunch of a restored clone: its plan is built when the guest
        // reboots, and building it checks the maps.
        let snapshot = source("src/commands/snapshot.rs");
        assert!(
            body_of(&snapshot, "async fn build_clone_reboot_plan(").contains(check),
            "build_clone_reboot_plan does not check the maps"
        );
        let planned = position(&snapshot, "match build_clone_reboot_plan(");
        let booted = position(&snapshot, &boot);
        assert!(
            planned < booted
                && snapshot[planned..booted]
                    .matches("build_clone_reboot_plan(")
                    .count()
                    == 1
                && snapshot[planned..booted].contains("reboot_plan.as_ref()"),
            "the clone's relaunch does not boot from the plan it has just built"
        );
    }

    #[test]
    fn test_resolve_image_mode_default_is_overlay() {
        let args = test_args();

        assert_eq!(resolve_image_mode(&args), ImageMode::Overlay);
    }

    #[test]
    fn test_resolve_image_mode_btrfs_profile_auto_detects() {
        let mut args = test_args();
        args.kernel_profile = Some("btrfs".to_string());

        assert_eq!(resolve_image_mode(&args), ImageMode::Btrfs);
    }

    #[test]
    fn test_resolve_image_mode_btrfs_in_profile_name() {
        let mut args = test_args();
        args.kernel_profile = Some("nested-btrfs-test".to_string());

        assert_eq!(resolve_image_mode(&args), ImageMode::Btrfs);
    }

    #[test]
    fn test_resolve_image_mode_non_btrfs_profile_is_overlay() {
        let mut args = test_args();
        args.kernel_profile = Some("nested".to_string());

        assert_eq!(resolve_image_mode(&args), ImageMode::Overlay);
    }

    #[test]
    fn test_resolve_image_mode_explicit_overrides_profile() {
        let mut args = test_args();
        args.kernel_profile = Some("btrfs".to_string());
        args.image_mode = Some(CliImageMode::Archive);

        assert_eq!(resolve_image_mode(&args), ImageMode::Archive);
    }

    #[test]
    fn test_resolve_image_mode_explicit_overlay() {
        let mut args = test_args();
        args.image_mode = Some(CliImageMode::Overlay);

        assert_eq!(resolve_image_mode(&args), ImageMode::Overlay);
    }

    #[test]
    fn test_resolve_image_mode_explicit_btrfs_no_profile() {
        let mut args = test_args();
        args.image_mode = Some(CliImageMode::Btrfs);

        assert_eq!(resolve_image_mode(&args), ImageMode::Btrfs);
    }

    fn prepare_lifecycle() -> PodmanLifecycle {
        PodmanLifecycle::Prepare(PrepareOptions::default())
    }

    #[test]
    fn prepare_and_run_have_explicit_source_dispositions() {
        assert_eq!(
            snapshot_source_disposition(&PodmanLifecycle::Run),
            super::super::common::SnapshotSourceDisposition::Resume
        );
        assert_eq!(
            snapshot_source_disposition(&prepare_lifecycle()),
            super::super::common::SnapshotSourceDisposition::LeavePaused
        );
    }

    #[test]
    fn prepare_arms_normal_health_without_requiring_an_http_override() {
        assert!(should_arm_startup_snapshot(
            false,
            true,
            false,
            &prepare_lifecycle()
        ));
        assert!(!should_arm_startup_snapshot(
            false,
            true,
            false,
            &PodmanLifecycle::Run
        ));
        assert!(should_arm_startup_snapshot(
            false,
            true,
            true,
            &PodmanLifecycle::Run
        ));
        assert!(!should_arm_startup_snapshot(
            true,
            true,
            true,
            &PodmanLifecycle::Run
        ));
    }

    /// A plain run takes (and restores) no startup snapshot; --startup-snapshot or
    /// --health-check asks for one.
    #[test]
    fn a_run_takes_a_startup_snapshot_only_when_it_asks() {
        let mut args = test_args();
        assert!(args.health_check.is_none() && !args.startup_snapshot);
        assert!(!run_takes_startup_snapshot(&args));
        args.startup_snapshot = true;
        assert!(run_takes_startup_snapshot(&args));
        args.startup_snapshot = false;
        args.health_check = Some("http://localhost/".to_string());
        assert!(run_takes_startup_snapshot(&args));
    }

    #[test]
    fn prepare_rejects_modes_that_cannot_produce_a_finite_verified_artifact() {
        let mut args = test_args();
        let error = validate_prepare_args(&args).unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "podman prepare cannot be used with --no-snapshot"
        );

        args.no_snapshot = false;
        args.tty = true;
        let error = validate_prepare_args(&args).unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "podman prepare does not support --tty"
        );

        args.tty = false;
        args.vsock_dir = Some("/tmp/external-vsock".to_string());
        let error = validate_prepare_args(&args).unwrap_err();
        assert!(format!("{error:#}").contains("does not support --vsock-dir"));
    }

    /// MAXIMUM REUSE / CACHEABILITY: `--vsock-dir` must NOT opt the run out of
    /// the snapshot cache. It only changes WHERE the clone's listener binds, and
    /// the restore puts it there: the snapshot load points the vsock device at
    /// the caller-owned directory (Firecracker 1.16.0 and later), or, before
    /// that, the mount redirect retargets the cached vmstate's embedded vsock
    /// directory to it (end-to-end pin: test_vsock_dir_honored_on_snapshot_cache_hit).
    #[test]
    fn a_custom_vsock_dir_still_participates_in_the_snapshot_cache() {
        let mut args = test_args();
        args.no_snapshot = false;
        args.vsock_dir = Some("/tmp/external-vsock".to_string());
        assert!(
            !snapshot_cache_opt_out(&args, false),
            "--vsock-dir must keep using the snapshot cache; the restore honors it"
        );
    }

    /// Each opt-out trigger stands alone; none depends on another being set.
    #[test]
    fn every_snapshot_cache_opt_out_trigger_stands_alone() {
        let baseline = || {
            let mut args = test_args();
            args.no_snapshot = false;
            args
        };
        assert!(!snapshot_cache_opt_out(&baseline(), false));

        let mut args = baseline();
        args.no_snapshot = true;
        assert!(snapshot_cache_opt_out(&args, false));

        let mut args = baseline();
        args.rootfs_override = Some(std::path::PathBuf::from("disk-only.raw"));
        assert!(snapshot_cache_opt_out(&args, false));

        // A Cloud Hypervisor run uses the cache like a Firecracker one (#806).
        let mut args = baseline();
        args.hypervisor = crate::cli::args::Hypervisor::CloudHypervisor;
        assert!(!snapshot_cache_opt_out(&args, false));

        assert!(snapshot_cache_opt_out(&baseline(), true));
    }

    #[test]
    fn prepare_publication_requires_successful_cleanup_and_no_cancellation() {
        assert_eq!(finish_prepare(Ok("artifact"), Ok(())).unwrap(), "artifact");

        let cleanup_error =
            finish_prepare(Ok("artifact"), Err(anyhow::anyhow!("holder still alive"))).unwrap_err();
        assert_eq!(format!("{cleanup_error:#}"), "holder still alive");

        let combined = finish_prepare::<()>(
            Err(anyhow::anyhow!("snapshot install failed")),
            Err(anyhow::anyhow!("network cleanup failed")),
        )
        .unwrap_err();
        assert_eq!(
            format!("{combined:#}"),
            "prepare failed: snapshot install failed; cleanup also failed: network cleanup failed"
        );

        let cancelled_gate = super::super::common::LifecycleReadyGate::new();
        cancelled_gate.cancel();
        assert_eq!(
            cancelled_gate.claim_terminal_publication().unwrap(),
            super::super::common::LifecycleReadyOutcome::Cancelled
        );

        let publication_gate = super::super::common::LifecycleReadyGate::new();
        assert_eq!(
            publication_gate.claim_terminal_publication().unwrap(),
            super::super::common::LifecycleReadyOutcome::Published
        );
        publication_gate.cancel();
        assert_eq!(
            publication_gate.claim_terminal_publication().unwrap(),
            super::super::common::LifecycleReadyOutcome::Published,
            "a terminal publication that linearized first remains the winner"
        );
    }

    /// A never-healthy container must end `prepare` with a diagnostic, not hang it.
    ///
    /// `run` has no equivalent deadline because an unhealthy VM is still a VM the caller
    /// owns and can inspect. `prepare` is finite, so without a bound this arm waits
    /// forever and the only symptom is a CI job that eventually runs out of wall clock.
    #[tokio::test(start_paused = true)]
    async fn prepare_gives_up_when_the_container_never_becomes_healthy() {
        // Held, so the startup channel stays pending instead of resolving to a closed error.
        let (_startup_tx, startup_rx) = tokio::sync::oneshot::channel();
        let cancel = CancellationToken::new();

        let error = await_prepare_healthy(
            startup_rx,
            &cancel,
            PREPARE_HEALTH_BUDGET,
            // The disposable VM stays up; only the container never reports healthy.
            std::future::pending::<Result<std::process::ExitStatus>>(),
        )
        .await
        .expect_err("a container that never becomes healthy must not hang prepare");

        let message = format!("{error:#}");
        assert!(
            message.contains(&format!(
                "disposable prepare VM did not report a healthy container within {}s",
                PREPARE_HEALTH_BUDGET.as_secs()
            )),
            "{message}"
        );
    }

    /// A failed prepare's error ends with the container's last lines of output and names the
    /// file holding all of it: `podman prepare` forwards none of the output, and an error that
    /// said only that the VM exited left the caller to find the file by the VM's id (#1027).
    #[test]
    fn a_failed_prepare_names_the_container_output_and_ends_with_its_last_lines() {
        let dir = tempfile::tempdir().unwrap();
        let saved = dir.path().join("fcvm-container-vm-x.log");
        let rotated = dir.path().join("fcvm-container.1-vm-x.log");
        let lines: Vec<String> = (0..30).map(|n| format!("stdout: line {n}")).collect();
        std::fs::write(&saved, lines.join("\n") + "\n").unwrap();
        let error = with_container_output(anyhow::anyhow!("the VM exited"), &saved, &rotated);
        let message = format!("{error:#}");
        assert!(message.starts_with("the VM exited\n"), "{message}");
        assert!(
            message.contains(&format!(
                "the container's last {PREPARE_ERROR_TAIL_LINES} lines of output (the output fcvm kept is in {}):",
                saved.display()
            )),
            "{message}"
        );
        assert!(message.ends_with("stdout: line 29"), "{message}");
        assert!(message.contains("stdout: line 10\n"), "{message}");
        assert!(!message.contains("stdout: line 9\n"), "{message}");

        // Output that rotated: the earlier lines' copy is named too.
        std::fs::write(&rotated, "stdout: line -1\n").unwrap();
        let message = format!(
            "{:#}",
            with_container_output(anyhow::anyhow!("x"), &saved, &rotated)
        );
        assert!(
            message.contains(&format!(
                "{}, and the lines before them in {}",
                saved.display(),
                rotated.display()
            )),
            "{message}"
        );

        // An empty current copy beside a rotated one shows the rotated copy's last lines.
        std::fs::write(&saved, "").unwrap();
        let message = format!(
            "{:#}",
            with_container_output(anyhow::anyhow!("x"), &saved, &rotated)
        );
        assert!(message.ends_with("stdout: line -1"), "{message}");
        std::fs::remove_file(&rotated).unwrap();

        std::fs::write(&saved, "").unwrap();
        let message = format!(
            "{:#}",
            with_container_output(anyhow::anyhow!("x"), &saved, &rotated)
        );
        assert!(
            message.contains("the container printed nothing"),
            "{message}"
        );

        // A last line longer than the bytes read is shown cut, not taken for no output.
        let long = "y".repeat(2 * PREPARE_ERROR_TAIL_BYTES as usize);
        std::fs::write(&saved, format!("stdout: {long}\n")).unwrap();
        let message = format!(
            "{:#}",
            with_container_output(anyhow::anyhow!("x"), &saved, &rotated)
        );
        assert!(
            message.contains("the container's last 1 lines of output")
                && message.contains("[...]yyy"),
            "{}",
            &message[..200]
        );

        let gone = dir.path().join("missing.log");
        let message = format!(
            "{:#}",
            with_container_output(anyhow::anyhow!("x"), &gone, &rotated)
        );
        assert_eq!(message, "x", "a file teardown did not write is not named");

        // Something another user left at the name is not read: a FIFO would block the open,
        // a symlink would be followed.
        let fifo = dir.path().join("fifo.log");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
        let message = format!(
            "{:#}",
            with_container_output(anyhow::anyhow!("x"), &fifo, &rotated)
        );
        assert_eq!(message, "x", "a FIFO is not read");
        let link = dir.path().join("link.log");
        std::os::unix::fs::symlink(&saved, &link).unwrap();
        let message = format!(
            "{:#}",
            with_container_output(anyhow::anyhow!("x"), &link, &rotated)
        );
        assert_eq!(message, "x", "a symlink is not followed");
    }

    /// The outcomes in which the source stopped (its VM exited, or its health monitor ended on
    /// seeing the container stopped) carry their own type, so the prepare loop adds the
    /// container's exit code to them and to nothing else. A container that exits ends the
    /// monitor first: the prepare sanity test saw that outcome, not the VM exit.
    #[tokio::test(start_paused = true)]
    async fn the_outcomes_where_the_source_stopped_are_told_apart() {
        use std::os::unix::process::ExitStatusExt;
        let stopped =
            |error: &anyhow::Error| error.downcast_ref::<PrepareSourceStopped>().is_some();

        let (_startup_tx, startup_rx) = tokio::sync::oneshot::channel();
        let vm_exited = await_prepare_healthy(
            startup_rx,
            &CancellationToken::new(),
            PREPARE_HEALTH_BUDGET,
            std::future::ready(Ok(std::process::ExitStatus::from_raw(0))),
        )
        .await
        .expect_err("the VM exited");
        assert!(stopped(&vm_exited), "{vm_exited:#}");

        let (startup_tx, startup_rx) = tokio::sync::oneshot::channel();
        drop(startup_tx);
        let monitor_ended = await_prepare_healthy(
            startup_rx,
            &CancellationToken::new(),
            PREPARE_HEALTH_BUDGET,
            std::future::pending::<Result<std::process::ExitStatus>>(),
        )
        .await
        .expect_err("the monitor ended");
        assert!(stopped(&monitor_ended), "{monitor_ended:#}");

        let (_startup_tx, startup_rx) = tokio::sync::oneshot::channel();
        let timed_out = await_prepare_healthy(
            startup_rx,
            &CancellationToken::new(),
            PREPARE_HEALTH_BUDGET,
            std::future::pending::<Result<std::process::ExitStatus>>(),
        )
        .await
        .expect_err("the budget ran out");
        assert!(!stopped(&timed_out), "{timed_out:#}");
    }

    #[tokio::test(start_paused = true)]
    async fn prepare_health_wait_honors_a_custom_budget() {
        // --ready-timeout exists for workloads whose warmup cannot fit the
        // default budget; the deadline and its error must follow the flag.
        let (_startup_tx, startup_rx) = tokio::sync::oneshot::channel();
        let cancel = CancellationToken::new();
        let error = await_prepare_healthy(
            startup_rx,
            &cancel,
            std::time::Duration::from_secs(3600),
            std::future::pending::<Result<std::process::ExitStatus>>(),
        )
        .await
        .expect_err("a pending container must still hit the custom deadline");
        let message = format!("{error:#}");
        assert!(message.contains("within 3600s"), "{message}");
        assert!(message.contains("--ready-timeout"), "{message}");
    }

    #[tokio::test(start_paused = true)]
    async fn prepare_health_wait_reports_each_non_healthy_outcome() {
        // Cancellation (SIGTERM/SIGINT) outranks the deadline.
        let (_startup_tx, startup_rx) = tokio::sync::oneshot::channel();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = await_prepare_healthy(
            startup_rx,
            &cancel,
            PREPARE_HEALTH_BUDGET,
            std::future::pending::<Result<std::process::ExitStatus>>(),
        )
        .await
        .expect_err("cancellation must end the wait");
        assert_eq!(
            format!("{error:#}"),
            "interrupted before the container became healthy"
        );

        // A source that dies before reporting healthy names its exit status.
        let (_startup_tx, startup_rx) = tokio::sync::oneshot::channel();
        let cancel = CancellationToken::new();
        let exited = async {
            Ok(std::os::unix::process::ExitStatusExt::from_raw(
                256, // WEXITSTATUS 1
            ))
        };
        let error = await_prepare_healthy(startup_rx, &cancel, PREPARE_HEALTH_BUDGET, exited)
            .await
            .expect_err("a dead source must end the wait");
        assert!(
            format!("{error:#}")
                .starts_with("disposable prepare VM exited before the container became healthy:"),
            "unexpected error: {error:#}"
        );

        // The health monitor going away without a first Healthy is not a silent success.
        let (startup_tx, startup_rx) = tokio::sync::oneshot::channel();
        drop(startup_tx);
        let cancel = CancellationToken::new();
        let error = await_prepare_healthy(
            startup_rx,
            &cancel,
            PREPARE_HEALTH_BUDGET,
            std::future::pending::<Result<std::process::ExitStatus>>(),
        )
        .await
        .expect_err("a dropped health trigger must end the wait");
        assert!(
            format!("{error:#}")
                .starts_with("health monitor stopped before prepare snapshot creation"),
            "unexpected error: {error:#}"
        );
    }

    /// A prepared snapshot whose pmem store entry is gone cannot be restored: Firecracker
    /// reopens the recorded entry. `prepare` must rebuild it, not report a hit the next run
    /// would refuse.
    #[tokio::test]
    async fn a_prepared_snapshot_whose_pmem_entry_is_gone_is_a_miss() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot_key = "0123456789ab-startup";
        let snapshot_dir = temp.path().join(snapshot_key);
        tokio::fs::create_dir_all(&snapshot_dir).await.unwrap();
        let images = tempfile::tempdir_in(crate::storage::pmem_store::test_image_dir()).unwrap();
        let image = images.path().join("cache.ext4");
        std::fs::File::create(&image)
            .unwrap()
            .set_len(2 * 1024 * 1024)
            .unwrap();

        let mut vm_state = VmState::new(
            "vm-prepare-pmem".to_string(),
            "alpine:latest".to_string(),
            1,
            512,
        );
        vm_state.config.source_vsock_socket_path =
            Some(std::path::PathBuf::from("/run/test-vsock/vsock.sock"));
        let data = tempfile::tempdir().unwrap();
        let store = crate::storage::pmem_store::PmemStore::open(data.path()).unwrap();
        vm_state.config.pmem_devices = crate::storage::pmem::resolve_pmem_devices(
            &store,
            &[format!("{}:/mnt/cache:ro", image.display())],
            &[],
        )
        .unwrap();
        let mut config = super::super::common::build_snapshot_config(
            &vm_state,
            snapshot_key,
            crate::storage::SnapshotType::System,
            &snapshot_dir,
            Vec::new(),
            Vec::new(),
            None,
        )
        .unwrap();
        config.content_key = Some(snapshot_key.to_string());
        for path in [&config.memory_path, &config.vmstate_path, &config.disk_path] {
            tokio::fs::create_dir_all(path.parent().unwrap())
                .await
                .unwrap();
            tokio::fs::write(path, b"durable-artifact").await.unwrap();
        }
        tokio::fs::write(
            snapshot_dir.join("config.json"),
            serde_json::to_vec_pretty(&config).unwrap(),
        )
        .await
        .unwrap();

        let target = prepare_install_target(&PrepareOptions::default(), snapshot_key).unwrap();
        assert!(
            verify_prepared_snapshot_in(data.path(), temp.path(), &target, PreparedCache::Hit)
                .await
                .unwrap()
                .is_some(),
            "a generation whose pmem entry is present is a hit"
        );
        std::fs::remove_file(&vm_state.config.pmem_devices[0].path).unwrap();
        assert!(
            verify_prepared_snapshot_in(data.path(), temp.path(), &target, PreparedCache::Hit)
                .await
                .unwrap()
                .is_none(),
            "a generation whose pmem entry is gone was reported as prepared"
        );
        assert!(
            !snapshot_dir.join("config.json").exists(),
            "the generation whose pmem entry is gone is still installed, so a rebuild under              the content key would reuse it"
        );
    }

    #[tokio::test]
    async fn prepared_snapshot_verification_pins_exact_complete_generation() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot_key = "0123456789ab-startup";
        let snapshot_dir = temp.path().join(snapshot_key);
        tokio::fs::create_dir_all(&snapshot_dir).await.unwrap();

        let mut vm_state = VmState::new(
            "vm-prepare-verify".to_string(),
            "alpine:latest".to_string(),
            1,
            512,
        );
        vm_state.config.source_vsock_socket_path =
            Some(std::path::PathBuf::from("/run/test-vsock/vsock.sock"));
        let mut config = super::super::common::build_snapshot_config(
            &vm_state,
            snapshot_key,
            crate::storage::SnapshotType::System,
            &snapshot_dir,
            Vec::new(),
            Vec::new(),
            None,
        )
        .unwrap();
        config.content_key = Some(snapshot_key.to_string());
        for path in [&config.memory_path, &config.vmstate_path, &config.disk_path] {
            tokio::fs::create_dir_all(path.parent().unwrap())
                .await
                .unwrap();
            tokio::fs::write(path, b"durable-artifact").await.unwrap();
        }
        tokio::fs::write(
            snapshot_dir.join("config.json"),
            serde_json::to_vec_pretty(&config).unwrap(),
        )
        .await
        .unwrap();

        let target = prepare_install_target(&PrepareOptions::default(), snapshot_key).unwrap();
        let prepared =
            verify_prepared_snapshot_in(temp.path(), temp.path(), &target, PreparedCache::Created)
                .await
                .unwrap()
                .expect("complete installed generation should verify");
        assert_eq!(prepared.output.status, "prepared");
        assert_eq!(prepared.output.cache, PreparedCache::Created);
        assert_eq!(prepared.output.snapshot_key, snapshot_key);
        assert_eq!(
            prepared.output.generation_id,
            config.generation_id.to_string()
        );
        assert_eq!(prepared.output.config_digest.len(), 64);

        let contender_dir = snapshot_dir.clone();
        let contender = tokio::spawn(async move {
            super::super::common::acquire_snapshot_dir_lock(&contender_dir, true)
                .await
                .unwrap()
        });
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        assert!(
            !contender.is_finished(),
            "verified generation must stay pinned until publication"
        );
        drop(prepared);
        let exclusive = tokio::time::timeout(std::time::Duration::from_secs(1), contender)
            .await
            .expect("exclusive replacement did not acquire released generation lease")
            .unwrap();
        drop(exclusive);

        tokio::fs::remove_file(&config.disk_path).await.unwrap();
        let error = match verify_prepared_snapshot_in(
            temp.path(),
            temp.path(),
            &target,
            PreparedCache::Hit,
        )
        .await
        {
            Ok(_) => panic!("missing disk artifact must fail verification"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("disk artifact"),
            "unexpected verification error: {error:#}"
        );
    }

    const CONTENT_KEY: &str = "0123456789ab-startup";

    fn tagged(tag: &str) -> PrepareOptions {
        PrepareOptions {
            tag: Some(tag.to_string()),
            ..Default::default()
        }
    }

    /// Write one installed generation to `root/<name>` and return its config.
    async fn install_generation(
        root: &std::path::Path,
        name: &str,
        content_key: Option<&str>,
        snapshot_type: crate::storage::SnapshotType,
    ) -> crate::storage::SnapshotConfig {
        let dir = root.join(name);
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let mut vm_state = VmState::new(
            "vm-prepare-target".to_string(),
            "alpine:latest".to_string(),
            1,
            512,
        );
        vm_state.config.source_vsock_socket_path =
            Some(std::path::PathBuf::from("/run/test-vsock/vsock.sock"));
        let mut config = super::super::common::build_snapshot_config(
            &vm_state,
            name,
            snapshot_type,
            &dir,
            Vec::new(),
            Vec::new(),
            None,
        )
        .unwrap();
        config.content_key = content_key.map(str::to_string);
        for path in [&config.memory_path, &config.vmstate_path, &config.disk_path] {
            tokio::fs::create_dir_all(path.parent().unwrap())
                .await
                .unwrap();
            tokio::fs::write(path, b"durable-artifact").await.unwrap();
        }
        tokio::fs::write(
            dir.join("config.json"),
            serde_json::to_vec_pretty(&config).unwrap(),
        )
        .await
        .unwrap();
        config
    }

    /// `--tag` puts the artifact where `snapshot serve`, `snapshot run --snapshot`,
    /// `snapshots ls` and `snapshots delete` already look: a snapshot directory named by
    /// the caller, with `config.name` matching, exactly as `snapshot create --tag` writes it.
    #[test]
    fn a_tag_names_the_installed_snapshot_and_no_tag_uses_the_content_key() {
        let untagged = prepare_install_target(&PrepareOptions::default(), CONTENT_KEY).unwrap();
        assert_eq!(untagged.name, CONTENT_KEY);
        assert_eq!(untagged.content_key, CONTENT_KEY);

        let tagged = prepare_install_target(&tagged("cb-req-golden"), CONTENT_KEY).unwrap();
        assert_eq!(tagged.name, "cb-req-golden");
        assert_eq!(
            tagged.content_key, CONTENT_KEY,
            "the tag renames the artifact, it does not change what identifies its content"
        );
    }

    /// The tag becomes a directory name, so it takes the same validation every other
    /// snapshot name takes rather than a second, weaker rule.
    #[test]
    fn a_tag_that_is_not_a_valid_snapshot_name_is_rejected() {
        for bad in ["../escape", "has space", "", "."] {
            let error = prepare_install_target(&tagged(bad), CONTENT_KEY)
                .unwrap_err()
                .to_string();
            assert_eq!(error, "invalid --tag", "tag {bad:?} was accepted");
        }
    }

    /// The tag composes with the content-addressed cache instead of replacing it: the
    /// generation under the tag answers only for the content it was built from.
    #[tokio::test]
    async fn a_tag_hits_only_for_the_content_it_holds() {
        let temp = tempfile::tempdir().unwrap();
        install_generation(
            temp.path(),
            "cb-req-golden",
            Some(CONTENT_KEY),
            crate::storage::SnapshotType::User,
        )
        .await;

        let matching = prepare_install_target(&tagged("cb-req-golden"), CONTENT_KEY).unwrap();
        let hit =
            verify_prepared_snapshot_in(temp.path(), temp.path(), &matching, PreparedCache::Hit)
                .await
                .unwrap()
                .expect("a tag holding this content is a hit");
        assert_eq!(hit.output.snapshot_key, "cb-req-golden");
        assert_eq!(hit.output.content_key, CONTENT_KEY);
        assert_eq!(hit.output.snapshot_type, "user");
        drop(hit);

        // The image changed, so the cache key changed, so the tag is stale.
        let other_content =
            prepare_install_target(&tagged("cb-req-golden"), "ffffffffffff-startup").unwrap();
        assert!(
            verify_prepared_snapshot_in(
                temp.path(),
                temp.path(),
                &other_content,
                PreparedCache::Hit
            )
            .await
            .unwrap()
            .is_none(),
            "a tag holding other content must be a miss so prepare rebuilds it"
        );
    }

    /// A prepared generation records the huge page mode `snapshot run` inherits, so a
    /// prepare asking for another mode must not report the installed one as its result.
    #[tokio::test]
    async fn a_prepared_hit_answers_only_for_the_huge_page_mode_it_records() {
        let temp = tempfile::tempdir().unwrap();
        for (name, snapshot_type) in [
            ("cb-req-golden", crate::storage::SnapshotType::User),
            (CONTENT_KEY, crate::storage::SnapshotType::System),
        ] {
            let mut config =
                install_generation(temp.path(), name, Some(CONTENT_KEY), snapshot_type).await;
            config.metadata.guest_thp = Some("always".to_string());
            tokio::fs::write(
                temp.path().join(name).join("config.json"),
                serde_json::to_vec_pretty(&config).unwrap(),
            )
            .await
            .unwrap();
        }
        let with_mode = |options: &PrepareOptions, mode: &str| {
            let mut target = prepare_install_target(options, CONTENT_KEY).unwrap();
            target.guest_thp = Some(mode.to_string());
            target
        };

        let same = with_mode(&tagged("cb-req-golden"), "always");
        assert!(
            verify_prepared_snapshot_in(temp.path(), temp.path(), &same, PreparedCache::Hit)
                .await
                .unwrap()
                .is_some(),
            "a tag recording this mode is a hit"
        );
        let other = with_mode(&tagged("cb-req-golden"), "never");
        assert!(
            verify_prepared_snapshot_in(temp.path(), temp.path(), &other, PreparedCache::Hit)
                .await
                .unwrap()
                .is_none(),
            "a tag recording another mode must be a miss so prepare rebuilds it"
        );
        let shared = with_mode(&PrepareOptions::default(), "never");
        let error = match verify_prepared_snapshot_in(
            temp.path(),
            temp.path(),
            &shared,
            PreparedCache::Hit,
        )
        .await
        {
            Ok(_) => panic!("the shared entry recording another mode was reported as a hit"),
            Err(error) => format!("{error:#}"),
        };
        assert!(
            error.contains("in always and this prepare asks for never")
                && error.contains("--force"),
            "{error}"
        );
    }

    /// The content-addressed key cannot hold another config's content, so a generation
    /// there that does not match is corrupt. Report it instead of rebuilding for ten
    /// minutes and then refusing to publish the result.
    #[tokio::test]
    async fn a_mismatch_at_the_content_addressed_key_is_an_error_not_a_rebuild() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = install_generation(
            temp.path(),
            CONTENT_KEY,
            Some(CONTENT_KEY),
            crate::storage::SnapshotType::System,
        )
        .await;
        config.kind = crate::storage::SnapshotKind::DiskOnly;
        tokio::fs::write(
            temp.path().join(CONTENT_KEY).join("config.json"),
            serde_json::to_vec_pretty(&config).unwrap(),
        )
        .await
        .unwrap();

        let target = prepare_install_target(&PrepareOptions::default(), CONTENT_KEY).unwrap();
        let error = match verify_prepared_snapshot_in(
            temp.path(),
            temp.path(),
            &target,
            PreparedCache::Hit,
        )
        .await
        {
            Ok(_) => panic!("a disk-only generation at the cache key cannot be published"),
            Err(error) => error,
        };
        assert_eq!(
            format!("{error:#}"),
            format!("prepared snapshot {CONTENT_KEY} is a disk-only snapshot, not a full one")
        );
    }

    /// A generation installed under its own content-addressed key self-identifies, so the
    /// cache entries that exist on disk today keep hitting without a migration.
    #[tokio::test]
    async fn a_generation_without_a_recorded_content_key_falls_back_to_its_name() {
        let temp = tempfile::tempdir().unwrap();
        install_generation(
            temp.path(),
            CONTENT_KEY,
            None,
            crate::storage::SnapshotType::System,
        )
        .await;

        let target = prepare_install_target(&PrepareOptions::default(), CONTENT_KEY).unwrap();
        let hit =
            verify_prepared_snapshot_in(temp.path(), temp.path(), &target, PreparedCache::Hit)
                .await
                .unwrap()
                .expect("a pre-existing cache entry named by its key must still hit");
        assert_eq!(hit.output.content_key, CONTENT_KEY);
    }

    /// A `podman prepare --tag` artifact and a `snapshot create --tag` artifact are the
    /// same kind of thing, so `snapshots prune` has to treat them the same: keep both by
    /// default, reclaim only the content-addressed cache.
    #[test]
    fn a_default_prune_keeps_a_tagged_prepare_and_reclaims_an_untagged_one() {
        let mut vm_state =
            VmState::new("vm-prune".to_string(), "alpine:latest".to_string(), 1, 512);
        vm_state.config.source_vsock_socket_path =
            Some(std::path::PathBuf::from("/run/test-vsock/vsock.sock"));
        let config_for = |target: &PreparedTarget| {
            super::super::common::build_snapshot_config(
                &vm_state,
                &target.name,
                target.snapshot_type,
                std::path::Path::new("/snapshots"),
                Vec::new(),
                Vec::new(),
                None,
            )
            .unwrap()
        };

        let tagged_config =
            config_for(&prepare_install_target(&tagged("cb-req-golden"), CONTENT_KEY).unwrap());
        let untagged_config =
            config_for(&prepare_install_target(&PrepareOptions::default(), CONTENT_KEY).unwrap());

        assert!(
            !crate::commands::snapshots::prune_reclaims(&tagged_config, false, None),
            "a golden snapshot must not evaporate on the next prune"
        );
        assert!(crate::commands::snapshots::prune_reclaims(
            &untagged_config,
            false,
            None
        ));
        assert!(
            crate::commands::snapshots::prune_reclaims(&tagged_config, true, None),
            "--all still reclaims it"
        );
    }

    /// A generation is only complete if every artifact a clone opens is in it. The three
    /// The durability sync reaches the disks in `disk/`, not only the generation's
    /// top-level files: a prepared generation whose disk is still in the page cache
    /// at a host crash would verify as prepared and restore a torn disk.
    #[test]
    fn the_durability_sync_reaches_the_disk_directory() {
        let temp = tempfile::tempdir().unwrap();
        let generation = temp.path().join("0123456789ab-startup");
        let disk_dir = crate::storage::snapshot::snapshot_disk_dir(&generation);
        std::fs::create_dir_all(&disk_dir).unwrap();
        for name in ["memory.bin", "vmstate.bin", "config.json"] {
            std::fs::write(generation.join(name), b"x").unwrap();
        }
        std::fs::write(disk_dir.join("disk.raw"), b"x").unwrap();
        std::fs::write(disk_dir.join("disk-dir-0.raw"), b"x").unwrap();

        let synced = super::sync_generation(&generation).unwrap();
        for wanted in [
            generation.join("memory.bin"),
            disk_dir.join("disk.raw"),
            disk_dir.join("disk-dir-0.raw"),
            disk_dir.clone(),
            generation.clone(),
        ] {
            assert!(
                synced.contains(&wanted),
                "{} was not synced",
                wanted.display()
            );
        }
        let dir_at = synced.iter().position(|p| p == &disk_dir).unwrap();
        let file_at = synced
            .iter()
            .position(|p| p == &disk_dir.join("disk.raw"))
            .unwrap();
        assert!(
            file_at < dir_at,
            "a directory must be synced after its files"
        );
        assert_eq!(
            synced.last(),
            Some(&generation),
            "the generation itself is synced last"
        );
    }

    /// core files are not the whole set: an extra disk or a portable volume's inode table
    /// can be absent while memory, vmstate and disk are all present and non-empty.
    #[tokio::test]
    async fn a_generation_missing_a_disk_or_inode_table_is_not_prepared() {
        async fn verify_with(
            temp: &std::path::Path,
            extra_disks: Vec<crate::storage::snapshot::SnapshotExtraDisk>,
            volumes: Vec<crate::storage::SnapshotVolumeConfig>,
            side_files: &[(&str, &[u8])],
        ) -> Result<bool> {
            let snapshot_key = "0123456789ab-startup";
            let snapshot_dir = temp.join(snapshot_key);
            tokio::fs::create_dir_all(&snapshot_dir).await.unwrap();

            let mut vm_state = VmState::new(
                "vm-prepare-parts".to_string(),
                "alpine:latest".to_string(),
                1,
                512,
            );
            vm_state.config.source_vsock_socket_path =
                Some(std::path::PathBuf::from("/run/test-vsock/vsock.sock"));
            let mut config = super::super::common::build_snapshot_config(
                &vm_state,
                snapshot_key,
                crate::storage::SnapshotType::System,
                &snapshot_dir,
                Vec::new(),
                Vec::new(),
                None,
            )
            .unwrap();
            config.content_key = Some(snapshot_key.to_string());
            config.metadata.extra_disks = extra_disks;
            config.metadata.volumes = volumes;
            for path in [&config.memory_path, &config.vmstate_path, &config.disk_path] {
                tokio::fs::create_dir_all(path.parent().unwrap())
                    .await
                    .unwrap();
                tokio::fs::write(path, b"durable-artifact").await.unwrap();
            }
            // Side files are named relative to the generation; a disk sits in `disk/`.
            for (name, bytes) in side_files {
                let path = snapshot_dir.join(name);
                tokio::fs::create_dir_all(path.parent().unwrap())
                    .await
                    .unwrap();
                tokio::fs::write(path, bytes).await.unwrap();
            }
            tokio::fs::write(
                snapshot_dir.join("config.json"),
                serde_json::to_vec_pretty(&config).unwrap(),
            )
            .await
            .unwrap();

            let target = prepare_install_target(&PrepareOptions::default(), snapshot_key).unwrap();
            // The lease is released with the returned value; these cases only ask
            // whether the generation verified at all.
            Ok(
                verify_prepared_snapshot_in(temp, temp, &target, PreparedCache::Hit)
                    .await?
                    .is_some(),
            )
        }

        fn disk(filename: &str) -> crate::storage::snapshot::SnapshotExtraDisk {
            crate::storage::snapshot::SnapshotExtraDisk {
                filename: filename.to_string(),
                mount_path: "/data".to_string(),
                read_only: false,
                drive_id: "disk0".to_string(),
            }
        }

        fn volume(portable: bool) -> crate::storage::SnapshotVolumeConfig {
            crate::storage::SnapshotVolumeConfig {
                host_path: std::path::PathBuf::from("/srv/data"),
                guest_path: "/data".to_string(),
                read_only: false,
                vsock_port: 5001,
                portable,
            }
        }

        // An extra disk recorded in metadata but absent from the generation.
        let temp = tempfile::tempdir().unwrap();
        let error = verify_with(temp.path(), vec![disk("disk-dir-0.raw")], Vec::new(), &[])
            .await
            .expect_err("a missing extra disk must not verify");
        assert!(
            format!("{error:#}").contains("extra disk disk0 artifact"),
            "unexpected error: {error:#}"
        );

        // Present but empty is the same failure at restore.
        let temp = tempfile::tempdir().unwrap();
        let error = verify_with(
            temp.path(),
            vec![disk("disk-dir-0.raw")],
            Vec::new(),
            &[("disk/disk-dir-0.raw", b"")],
        )
        .await
        .expect_err("an empty extra disk must not verify");
        assert!(
            format!("{error:#}").contains("is not a non-empty regular file"),
            "unexpected error: {error:#}"
        );

        // A filename that escapes the generation is never followed.
        let temp = tempfile::tempdir().unwrap();
        let error = verify_with(temp.path(), vec![disk("../escape.raw")], Vec::new(), &[])
            .await
            .expect_err("an escaping extra disk filename must not verify");
        assert!(
            format!("{error:#}").contains("names a path outside its generation"),
            "unexpected error: {error:#}"
        );

        // A portable volume with no inode table renumbers inodes on the clone.
        let temp = tempfile::tempdir().unwrap();
        let error = verify_with(temp.path(), Vec::new(), vec![volume(true)], &[])
            .await
            .expect_err("a missing inode table must not verify");
        assert!(
            format!("{error:#}").contains("portable volume /data inode table"),
            "unexpected error: {error:#}"
        );

        // A non-portable volume writes no table, so its absence is correct.
        let temp = tempfile::tempdir().unwrap();
        assert!(
            verify_with(temp.path(), Vec::new(), vec![volume(false)], &[])
                .await
                .unwrap(),
            "a non-portable volume needs no inode table"
        );

        // Complete generation: both artifacts present and non-empty.
        let temp = tempfile::tempdir().unwrap();
        assert!(
            verify_with(
                temp.path(),
                vec![disk("disk-dir-0.raw")],
                vec![volume(true)],
                &[
                    ("disk/disk-dir-0.raw", b"disk-bytes"),
                    ("volume-5001-inode-table.json", b"{}"),
                ],
            )
            .await
            .unwrap(),
            "a complete generation must verify"
        );
    }

    /// A generation whose type does not match what this invocation would install is not a
    /// hit: a `snapshot create` artifact sitting on the tag cannot be republished as if
    /// `prepare` had built it.
    #[tokio::test]
    async fn a_tag_holding_a_snapshot_this_prepare_did_not_build_is_a_miss() {
        let temp = tempfile::tempdir().unwrap();
        // `snapshot create --tag cb-req-golden` writes exactly this: User type, no content key.
        install_generation(
            temp.path(),
            "cb-req-golden",
            None,
            crate::storage::SnapshotType::User,
        )
        .await;

        let target = prepare_install_target(&tagged("cb-req-golden"), CONTENT_KEY).unwrap();
        assert!(
            verify_prepared_snapshot_in(temp.path(), temp.path(), &target, PreparedCache::Hit)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            target.existing,
            ExistingGeneration::Replace,
            "and the rebuild must replace it rather than short-circuit on it"
        );
    }

    /// `--force` has to defeat both short circuits, or an installed generation still wins:
    /// the pre-boot cache check that publishes without booting, and the under-lock check
    /// that reuses whatever is installed once the source is healthy.
    #[test]
    fn force_rebuilds_instead_of_reusing_an_installed_generation() {
        for tag in [None, Some("cb-req-golden")] {
            let forced = PrepareOptions {
                tag: tag.map(str::to_string),
                force: true,
                ..Default::default()
            };
            let target = prepare_install_target(&forced, CONTENT_KEY).unwrap();
            assert!(
                !target.publish_installed,
                "--force must not publish an installed generation (tag {tag:?})"
            );
            assert_eq!(
                target.existing,
                ExistingGeneration::Replace,
                "--force must install over whatever is there (tag {tag:?})"
            );
        }

        let unforced = prepare_install_target(&PrepareOptions::default(), CONTENT_KEY).unwrap();
        assert!(unforced.publish_installed);
        assert_eq!(
            unforced.existing,
            ExistingGeneration::Reuse,
            "without --force the content-addressed key reuses what is installed"
        );

        let unforced_tag = prepare_install_target(&tagged("cb-req-golden"), CONTENT_KEY).unwrap();
        assert!(
            unforced_tag.publish_installed,
            "a tag holding the right content still skips the boot"
        );
        assert_eq!(
            unforced_tag.existing,
            ExistingGeneration::Replace,
            "a caller-chosen name is only reached past the hit check when it held \
             nothing or held other content"
        );
    }

    /// The install decision `create_podman_snapshot` makes under the generation lock.
    #[test]
    fn only_a_reuse_policy_keeps_a_generation_installed_by_another_process() {
        assert!(snapshot::keeps_installed_generation(
            ExistingGeneration::Reuse,
            true
        ));
        assert!(!snapshot::keeps_installed_generation(
            ExistingGeneration::Reuse,
            false
        ));
        assert!(!snapshot::keeps_installed_generation(
            ExistingGeneration::Replace,
            true
        ));
        assert!(!snapshot::keeps_installed_generation(
            ExistingGeneration::Replace,
            false
        ));
    }

    #[test]
    fn custom_vsock_path_is_absolute_and_stable_across_launcher_directories() {
        let launcher_dir = Path::new("/work/launcher");
        assert_eq!(
            resolve_custom_vsock_socket_path(Path::new("relative-vsock"), launcher_dir),
            Path::new("/work/launcher/relative-vsock/vsock.sock")
        );
        assert_eq!(
            resolve_custom_vsock_socket_path(Path::new("/srv/vsock"), launcher_dir),
            Path::new("/srv/vsock/vsock.sock")
        );
    }

    #[tokio::test]
    async fn test_cleanup_failed_prepare_removes_state_and_data_dir() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let data_dir = temp.path().join("vm-data");
        tokio::fs::create_dir_all(&data_dir).await.unwrap();
        tokio::fs::write(data_dir.join("firecracker.log"), "log")
            .await
            .unwrap();

        let state_manager = StateManager::new(state_dir.clone());
        state_manager.init().await.unwrap();
        let vm_state = VmState::new(
            "vm-test-cleanup".to_string(),
            "alpine:latest".to_string(),
            1,
            512,
        );
        state_manager.save_state(&vm_state).await.unwrap();
        assert!(state_dir.join("vm-test-cleanup.json").exists());

        cleanup_failed_prepare(&state_manager, "vm-test-cleanup", &data_dir).await;

        assert!(
            !state_dir.join("vm-test-cleanup.json").exists(),
            "state file should be removed after a failed prepare"
        );
        assert!(
            !data_dir.exists(),
            "data dir should be removed after a failed prepare"
        );

        // Error paths can run before any state was persisted — calling the helper
        // again with nothing left to remove must not panic.
        cleanup_failed_prepare(&state_manager, "vm-test-cleanup", &data_dir).await;
    }
}

#[cfg(test)]
mod restore_fault_classifier_tests {
    use super::{classify_restore_failure, RestoreFault};

    fn classify(text: &str) -> RestoreFault {
        classify_restore_failure(&anyhow::anyhow!("{text}"))
    }

    #[test]
    fn identity_mismatch_is_an_unusable_snapshot() {
        // The restore-time identity check must trigger invalidate-and-fresh-boot,
        // not a hard run failure.
        assert_eq!(
            classify(
                "image disk build changed during launch: /x is now a, snapshot/key was \
                 computed against b"
            ),
            RestoreFault::UnusableSnapshot
        );
        assert_eq!(
            classify("snapshot pmem image changed: /x (mounted at /m) is missing"),
            RestoreFault::UnusableSnapshot
        );
        assert_eq!(classify("some other failure"), RestoreFault::Other);
    }

    /// A format or version rejection is the artifact: it can never load with this binary.
    #[test]
    fn a_format_rejection_is_an_unusable_snapshot() {
        for text in [
            "Load snapshot error: Failed to restore from snapshot: Failed to get snapshot \
             state from file: Failed to load snapshot state from file: Invalid data version: 11",
            "Load snapshot error: Failed to restore from snapshot: Failed to get snapshot \
             state from file: Failed to load snapshot state from file: An error occurred \
             during bitcode serialization: invalid",
            "Load snapshot error: Failed to restore from snapshot: Failed to get snapshot \
             state from file: Failed to load snapshot state from file: CRC64 validation failed",
            "Load snapshot error: Failed to restore from snapshot: Invalid snapshot state: \
             No DRAM memory region defined.",
        ] {
            assert_eq!(classify(text), RestoreFault::UnusableSnapshot, "{text}");
        }
    }

    /// The load fault from #988: a stale socket at a clone's `--vsock-dir` listener. The
    /// snapshot is intact, so the run boots fresh and the entry stays.
    #[test]
    fn a_host_side_load_fault_keeps_the_snapshot() {
        assert_eq!(
            classify(
                "Load snapshot error: Failed to restore from snapshot: Failed to build microVM \
                 from snapshot: Failed to restore devices: Error restoring MMIO devices: \
                 VsockUnixBackend: Error binding to the host-side Unix socket: Address already \
                 in use (os error 98)"
            ),
            RestoreFault::LoadFailed
        );
        assert_eq!(
            classify(
                "Load snapshot error: Failed to restore from snapshot: Failed to load guest \
                 memory: Error creating guest memory from uffd: Failed to connect to UDS Unix \
                 stream: Connection refused (os error 111)"
            ),
            RestoreFault::LoadFailed
        );
    }

    #[test]
    fn a_gpu_that_cannot_attach_fails_the_run_and_keeps_the_snapshot() {
        // Firecracker's rendering of a VFIO attach failure during PUT /snapshot/load.
        // The snapshot is fine; invalidating it would delete a shared cache entry, and a
        // fresh boot would hit the same busy device.
        assert_eq!(
            classify(
                "Load snapshot error: Failed to restore from snapshot: Failed to attach a \
                 passthrough device on restore: Attaching the passthrough device failed: busy"
            ),
            RestoreFault::Other
        );
        assert_eq!(
            classify(
                "Load snapshot error: Failed to restore from snapshot: Device passthrough on \
                 restore requires a KVM VM"
            ),
            RestoreFault::Other
        );
    }
}

#[cfg(test)]
mod poweroff_deadline_tests {
    use super::{DeadlineState, Killable, PoweroffWatchdog, GUEST_POWEROFF_DEADLINE};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[derive(Default)]
    struct FakeVmm {
        kills: usize,
    }

    impl Killable for FakeVmm {
        fn start_kill(&mut self) -> anyhow::Result<()> {
            self.kills += 1;
            Ok(())
        }
    }

    /// Nothing ends the wait while the container is still running, however long that takes.
    #[tokio::test(start_paused = true)]
    async fn a_running_container_never_expires_the_deadline() {
        let seen = Arc::new(AtomicBool::new(false));
        let expired = tokio::time::timeout(
            Duration::from_secs(86_400),
            PoweroffWatchdog::new(seen.clone(), GUEST_POWEROFF_DEADLINE).expired(),
        )
        .await;
        assert!(
            expired.is_err(),
            "the deadline expired with no container exit"
        );
    }

    /// The deadline is measured from the container's exit, not from the start of the loop.
    #[tokio::test(start_paused = true)]
    async fn the_deadline_runs_from_the_container_exit() {
        let seen = Arc::new(AtomicBool::new(false));
        let setter = seen.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(600)).await;
            setter.store(true, Ordering::Release);
        });
        let start = tokio::time::Instant::now();
        PoweroffWatchdog::new(seen.clone(), GUEST_POWEROFF_DEADLINE)
            .expired()
            .await;
        let waited = start.elapsed();
        assert!(
            waited >= Duration::from_secs(600) + GUEST_POWEROFF_DEADLINE,
            "expired {waited:?} after the loop started, before the exit plus the deadline"
        );
        assert!(
            waited < Duration::from_secs(600) + GUEST_POWEROFF_DEADLINE + Duration::from_secs(1),
            "expired {waited:?} after the loop started, long after the exit plus the deadline"
        );
    }

    /// Firing kills the VMM once and disarms the watchdog until it is rearmed.
    #[tokio::test(start_paused = true)]
    async fn firing_kills_the_vmm_once_and_then_stays_quiet() {
        let seen = Arc::new(AtomicBool::new(true));
        let mut watchdog = PoweroffWatchdog::new(seen.clone(), GUEST_POWEROFF_DEADLINE);
        let mut vmm = FakeVmm::default();
        watchdog.expired().await;
        watchdog.fire(&mut vmm).unwrap();
        assert_eq!(vmm.kills, 1);
        let again = tokio::time::timeout(Duration::from_secs(86_400), watchdog.expired()).await;
        assert!(again.is_err(), "a fired watchdog expired a second time");
    }

    /// A rebooted guest is relaunched after the watchdog fired; its next hang must be caught too.
    #[tokio::test(start_paused = true)]
    async fn a_relaunched_guest_gets_a_fresh_deadline() {
        let seen = Arc::new(AtomicBool::new(true));
        let mut watchdog = PoweroffWatchdog::new(seen.clone(), GUEST_POWEROFF_DEADLINE);
        let mut vmm = FakeVmm::default();
        watchdog.expired().await;
        watchdog.fire(&mut vmm).unwrap();
        // What the loop does when it relaunches a rebooted guest: clear the exit flag, rearm, and
        // let the new guest's container exit again later.
        seen.store(false, Ordering::Release);
        watchdog.rearm();
        tokio::time::sleep(Duration::from_secs(1)).await;
        seen.store(true, Ordering::Release);
        let second = tokio::time::timeout(
            GUEST_POWEROFF_DEADLINE + Duration::from_secs(1),
            watchdog.expired(),
        )
        .await;
        assert!(second.is_ok(), "the watchdog stayed off after a relaunch");
    }

    /// The run loop drops and recreates the watchdog's future whenever another select arm wins.
    /// The countdown must survive that: it runs from the container's exit, not from the last
    /// time the loop came round (CodeRabbit on #1002).
    #[tokio::test(start_paused = true)]
    async fn a_competing_event_does_not_restart_the_countdown() {
        let seen = Arc::new(AtomicBool::new(true));
        let mut watchdog = PoweroffWatchdog::new(seen.clone(), GUEST_POWEROFF_DEADLINE);
        let start = tokio::time::Instant::now();
        // Another arm wins after 30 s, and the loop comes round again.
        tokio::select! {
            _ = watchdog.expired() => panic!("expired before the deadline"),
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
        }
        watchdog.expired().await;
        let waited = start.elapsed();
        assert!(
            waited <= GUEST_POWEROFF_DEADLINE + Duration::from_secs(1),
            "the countdown restarted: expired {waited:?} after the exit, deadline {GUEST_POWEROFF_DEADLINE:?}"
        );
    }

    /// The container can exit while the loop is inside another arm's handler (a startup snapshot
    /// takes minutes), where nothing polls the watchdog. The countdown still runs from the exit,
    /// not from the next time the loop asks (Codex on #1002).
    #[tokio::test(start_paused = true)]
    async fn an_exit_during_a_long_handler_still_counts_from_the_exit() {
        let seen = Arc::new(AtomicBool::new(true));
        let mut watchdog = PoweroffWatchdog::new(seen.clone(), GUEST_POWEROFF_DEADLINE);
        // The loop is busy for 100 s and never polls the watchdog.
        tokio::time::sleep(Duration::from_secs(100)).await;
        let asked = tokio::time::Instant::now();
        watchdog.expired().await;
        let waited = asked.elapsed();
        assert!(
            waited <= Duration::from_secs(1),
            "the countdown began when the loop next asked: waited another {waited:?} after a 100 s handler"
        );
    }

    /// A relaunched guest's replacement container can exit before any poll sees the exit flag go
    /// false; the loop's clear-then-rearm must not depend on being observed (Codex on #1002).
    #[tokio::test(start_paused = true)]
    async fn a_rearm_does_not_depend_on_anyone_seeing_the_flag_clear() {
        let seen = Arc::new(AtomicBool::new(true));
        let mut watchdog = PoweroffWatchdog::new(seen.clone(), GUEST_POWEROFF_DEADLINE);
        let mut vmm = FakeVmm::default();
        watchdog.expired().await;
        watchdog.fire(&mut vmm).unwrap();
        // The loop stores false and the new container's exit stores true again, all inside one
        // 100 ms poll: nothing ever observes false.
        seen.store(false, Ordering::Release);
        watchdog.rearm();
        seen.store(true, Ordering::Release);
        let second = tokio::time::timeout(
            GUEST_POWEROFF_DEADLINE + Duration::from_secs(1),
            watchdog.expired(),
        )
        .await;
        assert!(
            second.is_ok(),
            "the watchdog never re-stamped a deadline for the new guest"
        );
    }

    /// `abort()` does not stop a tracker that is already past its last await, so a retired tracker
    /// can still write its deadline after `rearm` cleared it. The write must not survive (Codex on #1002).
    #[test]
    fn a_retired_trackers_late_write_does_not_survive_a_rearm() {
        let mut state = DeadlineState::default();
        let old_generation = state.generation;
        let new_generation = state.retire();
        let at = tokio::time::Instant::now();
        state.stamp_if_current(old_generation, at);
        assert!(
            state.at.is_none(),
            "a retired tracker's late write survived the rearm"
        );
        state.stamp_if_current(new_generation, at);
        assert!(
            state.at.is_some(),
            "the current tracker's write was dropped"
        );
    }

    /// The loop must consult the watchdog, kill through it, and rearm it where it relaunches a
    /// guest. No fake hypervisor can drive `run_vm_loop`, so the wiring is pinned by its source.
    #[test]
    fn the_run_loop_uses_the_watchdog_and_rearms_it_on_relaunch() {
        let source = include_str!("mod.rs");
        let body = &source[source.find("pub async fn run_vm_loop").unwrap()..];
        let body = &body[..body.find("\n}\n").unwrap()];
        assert!(body.contains("watchdog.expired()"), "no watchdog arm");
        assert!(
            body.contains("PoweroffWatchdog::new(ctx.container_exit_seen.clone()"),
            "the watchdog does not track the loop's exit flag"
        );
        assert!(
            body.contains("watchdog\n                    .fire(ctx.vm_manager.as_mut())"),
            "the arm does not kill the VMM"
        );
        let relaunch = body
            .find("guest rebooted — relaunching VM in place")
            .unwrap();
        let rearm = body
            .find("watchdog.rearm()")
            .expect("relaunch never rearms the watchdog");
        assert!(
            rearm > relaunch && rearm - relaunch < 200,
            "rearm is not at the relaunch site"
        );
    }
}

#[cfg(test)]
mod container_exit_status_tests {
    use super::*;

    #[test]
    fn a_failed_container_is_fcvms_exit_status() {
        assert!(container_exit_status(Some(0)).is_ok());

        let failed = container_exit_status(Some(7)).unwrap_err();
        assert_eq!(
            failed.downcast_ref::<ContainerExited>(),
            Some(&ContainerExited(7))
        );
        assert_eq!(format!("{failed:#}"), "container exited with code 7");
        assert_eq!(process_exit_status(&failed), 7);
        let wrapped = failed.context("running the clone");
        assert_eq!(process_exit_status(&wrapped), 7, "context hid the code");

        // A code that does not fit an exit status must still fail: 256 would read as 0.
        for code in [256, 512, -1] {
            let error = container_exit_status(Some(code)).unwrap_err();
            assert_eq!(process_exit_status(&error), 1, "code {code}");
        }

        let unreported = container_exit_status(None).unwrap_err();
        assert!(
            format!("{unreported:#}").contains("without reporting a container exit code"),
            "{unreported:#}"
        );
        assert_eq!(process_exit_status(&unreported), 1);
        assert_eq!(process_exit_status(&anyhow::anyhow!("anything else")), 1);
    }

    /// An NV2 or --gpu snapshot miss tears its VM down and restores it in one process. The
    /// routed VM's published host listeners are taken before the teardown closes them and
    /// handed to the restore's network, so a client of the published address is never
    /// refused in between and no other process can take the port (#1025;
    /// `handed_over_listeners_stay_open_from_the_teardown_to_the_restore` covers the
    /// sockets). The run needs an NV2 host to reach, so the wiring is pinned by source.
    #[test]
    fn a_restore_in_the_same_process_listens_on_the_torn_down_vms_listeners() {
        fn body<'a>(source: &'a str, start: &str) -> &'a str {
            let from = source.find(start).unwrap_or_else(|| panic!("no {start:?}"));
            let rest = &source[from..];
            &rest[..rest.find("\n}\n").expect("unterminated fn")]
        }
        let run = body(include_str!("mod.rs"), "pub async fn cmd_podman_run(");
        let at = |needle: &str| {
            run.find(needle)
                .unwrap_or_else(|| panic!("cmd_podman_run has no {needle:?}"))
        };
        let taken = at("ctx.network.take_port_listeners()");
        let torn_down = at("cleanup_vm_context(ctx).await;\n\n    // The run loop stopped");
        let restored = at("cmd_snapshot_run_with_listeners(snapshot_args, port_listeners)");
        assert!(
            taken < torn_down && torn_down < restored,
            "the listeners must be taken before the teardown and handed to the restore"
        );
        assert!(
            !run.contains("super::snapshot::cmd_snapshot_run(snapshot_args)"),
            "the restore must not bind its ports again"
        );

        let restore = body(
            include_str!("../snapshot.rs"),
            "async fn cmd_snapshot_run_inner(",
        );
        assert!(
            restore.contains(".with_port_listeners(port_listeners.take().unwrap_or_default())"),
            "a routed restore must be given the handed-over listeners"
        );
    }

    /// Every run that waits for its VM to exit reports the container through
    /// `container_exit_status`, and fcvm exits with `process_exit_status`. The loops need a
    /// VM to drive, so the wiring is pinned by source; tests/test_podman_snapshot.rs runs
    /// the cold and restore paths.
    #[test]
    fn every_run_reports_its_container_through_one_rule() {
        fn body<'a>(source: &'a str, start: &str) -> &'a str {
            let from = source.find(start).unwrap_or_else(|| panic!("no {start:?}"));
            let rest = &source[from..];
            &rest[..rest.find("\n}\n").expect("unterminated fn")]
        }
        let podman = include_str!("mod.rs");
        let snapshot = include_str!("../snapshot.rs");
        for (name, body) in [
            (
                "cmd_podman_run",
                body(podman, "pub async fn cmd_podman_run("),
            ),
            (
                "cmd_snapshot_run_inner",
                body(snapshot, "async fn cmd_snapshot_run_inner("),
            ),
            (
                "cmd_snapshot_run_disk_only",
                body(snapshot, "async fn cmd_snapshot_run_disk_only("),
            ),
        ] {
            assert!(
                body.contains("container_exit_status("),
                "{name} does not report through container_exit_status"
            );
            assert!(
                !body.contains("process::exit") && !body.contains("container exited with code"),
                "{name} reports the container by its own rule"
            );
        }
        assert!(
            body(podman, "pub async fn run_vm_loop").contains("container_exit_status(None)"),
            "run_vm_loop does not fail an unreported exit through container_exit_status"
        );
        assert!(
            include_str!("../../main.rs").contains("process_exit_status(e)"),
            "main does not exit with the container's code"
        );
    }
}
