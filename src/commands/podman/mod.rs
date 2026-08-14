use anyhow::{bail, Context, Result};
use fs2::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;
use tracing::{info, warn};

mod image;
mod listeners;
mod namespace;
mod snapshot;
mod types;
mod vm_config;

pub use types::{
    shared_cache_verdict, CacheRequest, CacheVerdict, LogLine, PreparedTarget, SharedCacheVerdict,
    SnapshotOutcome, VmContext, VmHandle,
};
// Re-exported for the snapshot restore path's up-front reboot plan (a rebooted VM
// relaunches in place via the same shared primitive, on every lifecycle path).
pub(crate) use types::{RebootSpec, VolumeMapping};

// Re-exported for the #598 regression test (export must pin immutable content by image
// ID even when the tag is rebuilt mid-export).
pub use image::export_image_archive;

pub(crate) use listeners::{
    run_output_listener, run_status_listener, spawn_bootplan_listener,
    spawn_restore_completion_listener, RestoreCompletionReceiver,
};

use snapshot::{build_firecracker_config, snapshot_run_firecracker_overrides};
pub use snapshot::{
    check_podman_snapshot, create_snapshot_interruptible, startup_snapshot_key,
    CreateSnapshotParams, ExistingGeneration, SnapshotInstall,
};
pub(crate) use vm_config::{
    build_launch_config, build_runtime_boot_args, cleanup_nfs_exports, configure_and_boot_vm,
    setup_nfs_exports,
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
use image::{build_storage_image, get_image_cache_ref, validate_docker_archive};
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

/// The two `podman prepare` arguments that decide where the startup snapshot is
/// installed and whether an installed one is rebuilt.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct PrepareOptions {
    tag: Option<String>,
    force: bool,
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
    anyhow::ensure!(
        std::env::var("FCVM_BOOTPLAN").as_deref() != Ok("vsock"),
        "podman prepare does not support the forced FCVM_BOOTPLAN=vsock debug path"
    );
    Ok(())
}

fn should_arm_startup_snapshot(
    skip_snapshot_creation: bool,
    has_snapshot_key: bool,
    has_explicit_http_health_check: bool,
    lifecycle: &PodmanLifecycle,
) -> bool {
    !skip_snapshot_creation
        && has_snapshot_key
        && (lifecycle.is_prepare() || has_explicit_http_health_check)
}

/// Whether this invocation must stay out of the snapshot cache entirely — no
/// lookup, no creation. `env_no_snapshot` is `FCVM_NO_SNAPSHOT` set non-empty;
/// `env_forced_vsock_bootplan` is `FCVM_BOOTPLAN=vsock`.
///
/// MAXIMUM REUSE / CACHEABILITY is a core fcvm principle: an entry joins this
/// list only when a cached artifact would BEHAVE differently for this
/// invocation and the difference cannot be reconciled at restore time. Flags
/// that only change WHERE something binds (e.g. `--vsock-dir`) are honored by
/// the restore path instead of opting out.
fn snapshot_cache_opt_out(
    args: &RunArgs,
    env_no_snapshot: bool,
    env_forced_vsock_bootplan: bool,
) -> bool {
    args.no_snapshot
        // A disk-only clone cold-boots from the captured disk and must never divert
        // into the snapshot-cache / UFFD restore path.
        || args.rootfs_override.is_some()
        // Cloud Hypervisor supports explicit `snapshot create`/`run` (P2), but the
        // automatic pre-start snapshot cache for `podman run` is not wired up for CH yet
        // (a follow-on) — so never enter the snapshot-cache / restore path for it here.
        || args.hypervisor == crate::cli::args::Hypervisor::CloudHypervisor
        || env_no_snapshot
        // A FORCED boot-plan transport override (FCVM_BOOTPLAN=vsock on Firecracker, which
        // natively uses MMDS) produces a guest whose fc-agent took the vsock path and never
        // spawned the MMDS restore-epoch watcher. The snapshot cache key is a hash of
        // FirecrackerConfig, which does NOT encode the boot-plan transport, so a cached
        // vsock-built snapshot would be restored by a later NORMAL (MMDS) run under the same
        // key — the host then signals restore over MMDS that nobody polls, wedging the
        // restored VM (no exec rebind / output reconnect). Forcing the transport is a
        // test/debug path with no need to populate the shared cache, so skip caching for it.
        || env_forced_vsock_bootplan
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
        snapshot_type,
    })
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

struct PreparedSnapshot {
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
    verify_prepared_snapshot_in(&paths::snapshot_dir(), target, cache).await
}

/// Verify the generation installed at `target.name`.
///
/// `Ok(None)` means this invocation has nothing to publish: either nothing is installed
/// there, or a caller-chosen name holds something else. Both are cache misses and the
/// caller rebuilds. `Err` means the generation cannot be published and rebuilding it
/// would not help: it is truncated, points outside itself, or sits at the
/// content-addressed key while describing different content.
async fn verify_prepared_snapshot_in(
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
        ("disk", &config.disk_path, snapshot_dir.join("disk.raw")),
    ] {
        anyhow::ensure!(
            configured == &expected,
            "prepared snapshot {} {} path points outside its generation: {}",
            snapshot_key,
            label,
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
        let path = snapshot_dir.join(&disk.filename);
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
        let path = snapshot_dir.join(format!("volume-{}-inode-table.json", volume.vsock_port));
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

    // `prepare` is a durability boundary, not merely a namespace rename. Flush every file
    // in the installed generation, then the generation and snapshot-root directories, while
    // the shared generation lease prevents replacement. Success therefore survives a host
    // crash after the JSON response rather than depending on eventual writeback.
    let sync_dir = snapshot_dir.clone();
    let sync_root = snapshot_root.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<()> {
        for entry in std::fs::read_dir(&sync_dir)
            .with_context(|| format!("reading prepared generation {}", sync_dir.display()))?
        {
            let entry = entry.context("reading prepared generation entry")?;
            if entry
                .file_type()
                .context("reading prepared generation entry type")?
                .is_file()
            {
                std::fs::File::open(entry.path())
                    .with_context(|| {
                        format!("opening prepared artifact {}", entry.path().display())
                    })?
                    .sync_all()
                    .with_context(|| {
                        format!("syncing prepared artifact {}", entry.path().display())
                    })?;
            }
        }
        std::fs::File::open(&sync_dir)
            .with_context(|| format!("opening prepared generation {}", sync_dir.display()))?
            .sync_all()
            .with_context(|| format!("syncing prepared generation {}", sync_dir.display()))?;
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

/// Best-effort removal of host state persisted during a failed `prepare_vm`.
/// True when the VM's kernel profile enables ARM64 NV2 nested virtualization
/// (the profile's firecracker_args carry --enable-nv2). Drives the
/// miss-path-converges-on-restore behavior, which exists for NV2 guests only.
fn nv2_profile(kernel_profile: &Option<String>) -> bool {
    let Some(name) = kernel_profile.as_deref() else {
        return false;
    };
    matches!(
        crate::setup::get_kernel_profile(name),
        Ok(Some(profile)) if profile
            .firecracker_args
            .as_deref()
            .is_some_and(|args| args.contains("--enable-nv2"))
    )
}

/// True when an error chain ends at Firecracker's `PUT /snapshot/load` step —
/// i.e. the cached snapshot artifact itself is unusable (most commonly: it was
/// created by an incompatible Firecracker version and no longer deserializes).
///
/// Firecracker prefixes every snapshot-load fault with "Load snapshot error",
/// which fcvm's API client embeds verbatim in the error message.
fn is_snapshot_load_failure(err: &anyhow::Error) -> bool {
    let rendered = format!("{err:#}");
    // "image disk build changed" is verify_image_disk_identity's marker: the
    // snapshot references a disk build that no longer exists at that path, so
    // it is permanently unusable — invalidate it and fall back to a fresh
    // boot, which re-stats the disk and provisions coherently.
    rendered.contains("Load snapshot error") || rendered.contains("image disk build changed")
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
    lock_file
        .lock_exclusive()
        .context("acquiring image cache lock")?;

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
        // coordinate cross-VM (see image::unique_cache_tmp). A shared "<digest>.tmp"
        // would let one VM's rename ENOENT the other's in-flight export.
        let tmp_path = image::unique_cache_tmp(&archive_path);

        // Export by the IMMUTABLE image ID captured at inspect time, not the mutable
        // tag (#598). A parallel build can repoint the tag between the cache-key
        // inspect and this export; `podman save <tag>` would then archive the newer
        // build under the older digest's cache entry. export_image_archive pins the
        // exact content the cache key names and writes the original repo tag into the
        // archive's RepoTags, so the guest still loads and runs it by name.
        let image_id = localhost_image_id.ok_or_else(|| {
            anyhow::anyhow!("internal: localhost image id was not captured at inspect time")
        })?;
        if let Err(e) = image::export_image_archive(image_id, &args.image, &tmp_path).await {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            drop(lock_file);
            return Err(e);
        }

        // Atomic rename within the same filesystem
        if let Err(e) = tokio::fs::rename(&tmp_path, &archive_path).await {
            // Clean up the UUID-keyed temp — unlike the old fixed-name temp, a UUID
            // orphan is never overwritten by the next run.
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(e).context("renaming exported archive to final path");
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

async fn prepare_vm_for_lifecycle(
    mut args: RunArgs,
    lifecycle: PodmanLifecycle,
) -> Result<VmPreparation> {
    info!(
        lifecycle = ?lifecycle,
        "Starting fcvm podman lifecycle"
    );

    if lifecycle.is_prepare() {
        validate_prepare_args(&args)?;
    }

    // Validate VM name before any setup work
    validate_vm_name(&args.name).context("invalid VM name")?;

    // Validate hugepages memory alignment (2MB pages require even MiB)
    if args.hugepages && !args.mem.is_multiple_of(2) {
        bail!(
            "--mem {} is not divisible by 2: hugepages requires 2MB-aligned memory size",
            args.mem
        );
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

    // --publish now DNATs each published guest port to 127.0.0.1 inside the guest,
    // and --forward-localhost BINDS 127.0.0.1:<port> there as a relay to the host.
    // Overlap turns the published port into a reflector: an external client reaches
    // the HOST's service on that port and never touches the guest. It returns a
    // successful response from the wrong machine rather than an error, so it cannot
    // be left to be discovered at runtime. The overlap is on the GUEST port, which
    // is the second field of --publish.
    {
        let forwarded: std::collections::HashSet<u16> =
            args.forward_localhost.iter().copied().collect();
        for spec in &args.publish {
            let Ok(pm) = crate::network::PortMapping::parse(spec) else {
                continue; // invalid specs are reported by the parser itself
            };
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
    }

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
    let runtime_config = runtime_config_from_kernel_profiles(
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
    let image_disk_path: Option<PathBuf> = if args.image.starts_with("localhost/")
        && args.rootfs_override.is_none()
    {
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

    // Build identity (inode/size/mtime) of the exact file that will be
    // attached; a rebuild at the same path is a different identity. Overlay
    // storage images ONLY: btrfs/archive modes import the archive into the
    // guest before the pre-start snapshot, so a restored VM never re-reads the
    // archive — keying those on identity would force a needless cold boot on
    // every re-export without changing restored behavior.
    let image_disk_identity = image_disk_path
        .as_deref()
        .filter(|p| crate::utils::is_overlay_storage_image(p))
        .map(|p| {
            crate::utils::file_identity(p)
                .with_context(|| format!("stat image disk {} for snapshot key", p.display()))
        })
        .transpose()?;

    // Check for snapshot cache (unless the invocation opts out — see
    // snapshot_cache_opt_out for the full list and rationale).
    // Keep fc_config and snapshot_key available for later snapshot creation on miss
    let no_snapshot = snapshot_cache_opt_out(
        &args,
        std::env::var("FCVM_NO_SNAPSHOT")
            .map(|v| !v.is_empty())
            .unwrap_or(false),
        std::env::var("FCVM_BOOTPLAN").as_deref() == Ok("vsock"),
    );
    let (fc_config, snapshot_key, prepare_target): (
        Option<crate::firecracker::FirecrackerConfig>,
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
            image_disk_identity.clone(),
            vm_config::effective_extra_boot_args(&runtime_config),
        );
        let key = config.snapshot_key();

        // Check if cached snapshot exists - prefer startup snapshot over pre-start snapshot
        let startup_key = startup_snapshot_key(&key);

        let mut prepare_target = None;
        if let PodmanLifecycle::Prepare(options) = &lifecycle {
            let target = prepare_install_target(options, &startup_key)?;
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
                return Ok(VmPreparation::Prepared(prepared));
            }
            prepare_target = Some(target);
        }

        // Check for startup snapshot first (fully initialized application)
        if !lifecycle.is_prepare() && check_podman_snapshot(&startup_key).await.is_some() {
            info!(
                snapshot_key = %startup_key,
                image = %args.image,
                "Startup snapshot hit! Restoring from fully-initialized snapshot"
            );
            let (firecracker_bin, firecracker_args) =
                snapshot_run_firecracker_overrides(&runtime_config);
            // Call snapshot run directly with startup snapshot
            // No need to create startup snapshot again since we're restoring from one
            let snapshot_args = crate::cli::SnapshotRunArgs {
                pid: None,
                snapshot: Some(startup_key.clone()),
                name: Some(args.name.clone()),
                exec: None,
                no_dirty_tracking: false, // podman needs dirty tracking for future snapshots
                no_swap: false,
                vsock_dir: args.vsock_dir.clone(),
                startup_snapshot_base_key: None, // Already using startup snapshot
                cpu: Some(args.cpu),
                mem: Some(args.mem),
                firecracker_bin,
                firecracker_args,
                hugepages: Some(args.hugepages),
                non_blocking_output: args.non_blocking_output,
            };
            let attempt = super::snapshot::cmd_snapshot_run_attempt(snapshot_args).await;
            match attempt.result {
                Ok(()) => return Ok(VmPreparation::RunCompleted),
                Err(e) if is_snapshot_load_failure(&e) => {
                    invalidate_unusable_snapshot(&startup_key, attempt.generation.as_ref(), &e)
                        .await;
                    // fall through to the pre-start check / fresh boot
                }
                Err(e) => return Err(e),
            }
        }

        // Check for pre-start snapshot (container loaded but not initialized)
        if !lifecycle.is_prepare() && check_podman_snapshot(&key).await.is_some() {
            info!(
                snapshot_key = %key,
                image = %args.image,
                "Pre-start snapshot hit! Restoring from cached snapshot"
            );
            let (firecracker_bin, firecracker_args) =
                snapshot_run_firecracker_overrides(&runtime_config);
            // Call snapshot run with startup snapshot creation enabled
            // (if health_check_url is set)
            let snapshot_args = crate::cli::SnapshotRunArgs {
                pid: None,
                snapshot: Some(key.clone()),
                name: Some(args.name.clone()),
                exec: None,
                no_dirty_tracking: false, // podman needs dirty tracking for startup snapshot
                no_swap: false,
                vsock_dir: args.vsock_dir.clone(),
                // Create startup snapshot if this config has a health check URL
                startup_snapshot_base_key: args.health_check.as_ref().map(|_| key.clone()),
                cpu: Some(args.cpu),
                mem: Some(args.mem),
                firecracker_bin,
                firecracker_args,
                hugepages: Some(args.hugepages),
                non_blocking_output: args.non_blocking_output,
            };
            let attempt = super::snapshot::cmd_snapshot_run_attempt(snapshot_args).await;
            match attempt.result {
                Ok(()) => return Ok(VmPreparation::RunCompleted),
                Err(e) if is_snapshot_load_failure(&e) => {
                    invalidate_unusable_snapshot(&key, attempt.generation.as_ref(), &e).await;
                    // fall through to a fresh boot (which re-creates the snapshot)
                }
                Err(e) => return Err(e),
            }
        }

        info!(
            snapshot_key = %key,
            image = %args.image,
            "Snapshot miss, will create snapshot after image load"
        );
        (Some(config), Some(key), prepare_target)
    } else {
        if std::env::var("FCVM_NO_SNAPSHOT")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            info!("Snapshot disabled via FCVM_NO_SNAPSHOT environment variable");
        } else {
            info!("Snapshot disabled via --no-snapshot flag");
        }
        (None, None, None)
    };

    // Generate VM ID
    let vm_id = generate_vm_id();
    let vm_name = args.name.clone();

    // Parse port mappings
    let port_mappings: Vec<PortMapping> = args
        .publish
        .iter()
        .map(|s| PortMapping::parse(s))
        .collect::<Result<Vec<_>>>()
        .context("parsing port mappings")?;

    // Parse volume mappings (HOST:GUEST[:ro])
    let volume_mappings: Vec<VolumeMapping> = args
        .map
        .iter()
        .map(|s| VolumeMapping::parse(s))
        .collect::<Result<Vec<_>>>()
        .context("parsing volume mappings")?;

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
    vm_state.config.hugepages = args.hugepages;
    vm_state.config.portable_volumes = args.portable_volumes;
    vm_state.config.port_mappings = port_mappings.clone();
    vm_state.config.forward_localhost = args.forward_localhost.clone();
    vm_state.config.network_mode = args.network.into();
    vm_state.config.hypervisor = args.hypervisor.into();
    vm_state.config.ipv6_prefix = args.ipv6_prefix.clone();
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
    vm_state.config.labels = args
        .label
        .iter()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

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

    let tap_device = format!("tap-{}", truncate_id(&vm_id, 8));
    let mut network: Box<dyn NetworkManager> = match args.network {
        NetworkMode::Bridged => Box::new(BridgedNetwork::new(
            vm_id.clone(),
            tap_device.clone(),
            port_mappings.clone(),
        )),
        NetworkMode::Routed => {
            let mut net =
                RoutedNetwork::new(vm_id.clone(), tap_device.clone(), port_mappings.clone());
            if let Some(ref prefix) = args.ipv6_prefix {
                net = net.with_ipv6_prefix(prefix.clone());
            }
            if !args.forward_localhost.is_empty() {
                net = net.with_forward_localhost(args.forward_localhost.clone());
            }
            net.preflight_check()
                .context("routed mode preflight check failed")?;
            if !port_mappings.is_empty() {
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

            Box::new(
                PastaNetwork::new(vm_id.clone(), tap_device.clone(), port_mappings.clone())
                    .with_loopback_ip(loopback_ip),
            )
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
    // A cold-boot VMM embeds the same exact path it binds. Restored clones keep
    // this source path from snapshot metadata while using a clone-local listener.
    vm_state.config.source_vsock_socket_path = Some(vsock_socket_path.clone());

    // Build VolumeConfigs and spawn VolumeServers BEFORE the VM starts
    // Each VolumeServer listens on vsock.sock_{port} (e.g., vsock.sock_5000)
    // Firecracker binds to vsock.sock and routes guest connections to the per-port sockets
    let volume_configs: Vec<VolumeConfig> = volume_mappings
        .iter()
        .enumerate()
        .map(|(idx, vol)| VolumeConfig {
            host_path: vol.host_path.clone(),
            guest_path: vol.guest_path.clone().into(),
            read_only: vol.read_only,
            port: VSOCK_VOLUME_PORT_BASE + idx as u32,
            portable: args.portable_volumes,
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
    let (cache_tx, cache_rx): (
        Option<mpsc::Sender<CacheRequest>>,
        Option<mpsc::Receiver<CacheRequest>>,
    ) = if !skip_snapshot_creation && !lifecycle.is_prepare() {
        let (tx, rx) = mpsc::channel(1);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    // What this process knows the guest to be, for answering (re-)asked
    // "cache-ready" messages. With snapshots disabled no boundary can sever
    // the handshake and no snapshot decision exists — the verdict is Continue
    // from the start; otherwise it is Pending until the run loop decides.
    let cache_verdict = shared_cache_verdict(if skip_snapshot_creation {
        CacheVerdict::Continue
    } else {
        CacheVerdict::Pending
    });

    // Create startup snapshot channel for health-triggered snapshot creation
    // Only create startup snapshots if:
    // - Not skipping snapshots (no --no-snapshot)
    // - Have a snapshot key
    // - Have a health_check URL configured (HTTP health check, not just container-ready)
    let (startup_tx, startup_rx): (
        Option<tokio::sync::oneshot::Sender<crate::health::StartupSnapshotAck>>,
        Option<tokio::sync::oneshot::Receiver<crate::health::StartupSnapshotAck>>,
    ) = if should_arm_startup_snapshot(
        skip_snapshot_creation,
        snapshot_key.is_some(),
        args.health_check.is_some(),
        &lifecycle,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    // Start status channel listener for fc-agent notifications
    // - "ready" on port 4999 -> creates container-ready file for health check
    // - "exit:{code}" on port 4999 -> creates container-exit file with exit code
    // - "cache-ready:{digest}" on port 4999 -> trigger cache creation
    let status_socket_path = format!("{}_{}", vsock_socket_path.display(), VSOCK_STATUS_PORT);
    // Set by the status listener when the guest signals a reboot / a container
    // exit; consumed by run_vm_loop to decide relaunch-in-place vs terminate.
    let reboot_requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let container_exit_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let status_handle = {
        let runtime_dir = data_dir.clone();
        let socket_path = status_socket_path.clone();
        let vm_id_clone = vm_id.clone();
        let reboot_flag = reboot_requested.clone();
        let exit_flag = container_exit_seen.clone();
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

    // For non-TTY mode, use async output listener
    let output_reconnect = Arc::new(tokio::sync::Notify::new());
    let output_handle = if !tty_mode {
        let socket_path = output_socket_path.clone();
        let vm_id_clone = vm_id.clone();
        let log_tx_clone = Some(log_tx.clone());
        let reconnect = output_reconnect.clone();
        let non_blocking_output = args.non_blocking_output;
        Some(tokio::spawn(async move {
            match run_output_listener(
                &socket_path,
                &vm_id_clone,
                log_tx_clone,
                reconnect,
                non_blocking_output,
                !lifecycle.is_prepare(),
                None,
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
        },
        network.as_mut(),
        &state_manager,
        &mut vm_state,
    )
    .await;

    // If setup failed, cleanup all resources before propagating error
    if let Err(e) = setup_result {
        warn!("VM setup failed, cleaning up resources");

        // Abort VolumeServer tasks
        for handle in volume_servers.handles {
            handle.abort();
        }

        // Abort status listener
        status_handle.abort();

        // Abort output listener task if still running
        if let Some(handle) = output_handle {
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
        egress_proxy_handle,
        cache_rx,
        startup_rx,
        snapshot_key,
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
    let join = tokio::task::spawn_blocking(move || handle.join().ok().and_then(|r| r.ok()));
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

/// Read the container exit code recorded by the status listener after VM exit.
///
/// The exit notification stays buffered in the host-side unix socket even after
/// Firecracker exits, but the listener task may not have processed it yet when the VM
/// exit is observed — wait on the exit-seen flag for a bounded window (the listener
/// itself stays alive to catch a racing reboot signal, so we can't join it).
async fn read_container_exit_code(ctx: &mut VmContext) -> Option<i32> {
    use std::sync::atomic::Ordering;
    // wait_for_reboot_decision's drain handshake already proved the listener
    // processed everything the guest sent; the short wait here is only a backstop
    // for the listener-stuck case where the drain timed out.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while !ctx.container_exit_seen.load(Ordering::Acquire) {
        if tokio::time::Instant::now() >= deadline {
            warn!("no container exit notification after VM exit");
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let exit_file = ctx.data_dir.join("container-exit");
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

/// Event loop: waits for VM exit, cancellation, or snapshot requests.
/// Returns the container exit code (None if cancelled/signalled).
pub async fn run_vm_loop(ctx: &mut VmContext, cancel: CancellationToken) -> Result<Option<i32>> {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("cancellation requested, shutting down VM");
                return Ok(None);
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
                    // A reboot is a clean cold boot from the already-provisioned disk
                    // (disk-only-clone semantics) — don't re-create the pre-start /
                    // startup snapshot. The Continue verdict set below makes the
                    // relaunched fc-agent's cache-ready resolve to a cold start,
                    // so it proceeds straight to the container.
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
                    ctx.vm_manager
                        .spawn(&relaunch_spec)
                        .await
                        .context("relaunching Firecracker after guest reboot")?;
                    let volume_mappings: Vec<VolumeMapping> = ctx
                        .args
                        .map
                        .iter()
                        .map(|s| VolumeMapping::parse(s))
                        .collect::<Result<Vec<_>>>()
                        .context("parsing volume mappings for reboot relaunch")?;
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
                    let exit_code = read_container_exit_code(ctx).await;
                    info!(container_exit_code = ?exit_code, "container exit code");
                    exit_code
                };
                let Some(code) = exit_code else {
                    // No exit code means the guest never reported the container's
                    // result (boot failure, fc-agent crash, or lost vsock
                    // notification). Fail instead of silently reporting success.
                    bail!("VM exited without reporting a container exit code");
                };
                return Ok(Some(code));
            }
            // Handle cache creation requests from fc-agent
            Some(cache_request) = async {
                match ctx.cache_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(ref key) = ctx.snapshot_key {
                    info!(snapshot_key = %key, digest = %cache_request.digest, "Creating pre-start snapshot");

                    let fc_backend = ctx
                        .vm_manager
                        .as_any()
                        .downcast_ref::<FirecrackerBackend>()
                        .context("snapshot creation requires the Firecracker backend")?;
                    let snap = CreateSnapshotParams::cache_entry(
                        fc_backend,
                        key,
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
                        SnapshotOutcome::Created => {
                            // NV2 (vEL2) guests: do NOT ack fc-agent and do NOT resume
                            // into this VM — tear it down and restore the snapshot we just
                            // produced, so the miss path runs the exact same restore flow
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
                            if nv2_profile(&ctx.vm_state.config.kernel_profile) {
                                info!(
                                    snapshot_key = %key,
                                    "Pre-start snapshot created; relaunching by restoring it \
                                     (NV2 miss path converges on the hit path)"
                                );
                                // Recorded BEFORE the return drops the oneshot: a
                                // re-asking guest must hear "cache-doomed", never
                                // silence read as "maybe ack later" or a bare ack
                                // from a raced teardown.
                                *ctx.cache_verdict.lock().unwrap() = CacheVerdict::Doomed;
                                ctx.restore_from_cache = Some(key.clone());
                                return Ok(None);
                            }
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
                        SnapshotOutcome::Failed(e) => {
                            warn!(snapshot_key = %key, error = %e, "Failed to create pre-start snapshot");
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

                if let Some(ref key) = ctx.snapshot_key {
                    let startup_key = startup_snapshot_key(key);

                    // Skip if startup snapshot already exists
                    if check_podman_snapshot(&startup_key).await.is_some() {
                        info!(snapshot_key = %startup_key, "Startup snapshot already exists, skipping");
                    } else {
                        info!(snapshot_key = %startup_key, "Creating startup snapshot (VM healthy)");

                        // Use select! so SIGTERM can abort startup snapshot immediately.
                        // This is safe: startup snapshots are optional (just caching), so
                        // if the VM is paused mid-snapshot when we cancel, cleanup will
                        // kill the VM anyway via vm_manager.kill().
                        // The diff parent is resolved inside create_podman_snapshot under
                        // the per-VM snapshot lock (re-read from the state file), so a
                        // concurrent `fcvm snapshot create` cannot leave us with a stale base.
                        let fc_backend = ctx
                            .vm_manager
                            .as_any()
                            .downcast_ref::<FirecrackerBackend>()
                            .context("snapshot creation requires the Firecracker backend")?;
                        let snap = CreateSnapshotParams::cache_entry(
                            fc_backend,
                            &startup_key,
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

/// Resolve why the wait for the disposable source's first Healthy transition ended.
///
/// Split out of [`run_prepare_loop`] so every outcome is exercised without a VM: the
/// VM-exit arm takes its future from the caller.
async fn await_prepare_healthy<F>(
    startup_rx: tokio::sync::oneshot::Receiver<crate::health::StartupSnapshotAck>,
    cancel: &CancellationToken,
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
            bail!("disposable prepare VM exited before the container became healthy: {status}")
        }
        _ = tokio::time::sleep(PREPARE_HEALTH_BUDGET) => {
            bail!(
                "disposable prepare VM did not report a healthy container within {}s",
                PREPARE_HEALTH_BUDGET.as_secs()
            )
        }
        startup_ack = startup_rx => {
            startup_ack.context("health monitor stopped before prepare snapshot creation")
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
    let startup_ack = await_prepare_healthy(startup_rx, cancel, vm_exited).await?;

    // Resolved before the boot, so the generation this installs is the one the pre-boot
    // cache check looked for.
    let target = ctx
        .prepare_target
        .clone()
        .context("prepare lifecycle has no snapshot install target")?;
    info!(
        snapshot_key = %target.name,
        content_key = %target.content_key,
        snapshot_type = %target.snapshot_type,
        "Creating prepared startup snapshot (VM healthy)"
    );

    let fc_backend = ctx
        .vm_manager
        .as_any()
        .downcast_ref::<FirecrackerBackend>()
        .context("podman prepare requires the Firecracker backend")?;
    let snap = CreateSnapshotParams {
        vm_manager: fc_backend,
        snapshot_key: &target.name,
        content_key: &target.content_key,
        snapshot_type: target.snapshot_type,
        existing: target.existing,
        vm_state: &ctx.vm_state,
        disk_path: &ctx.disk_path,
        volume_configs: &ctx.volume_configs,
        remap_refs: &ctx.volume_servers.remap_refs,
    };
    let install = snapshot::create_podman_snapshot(&snap, snapshot_source_disposition(lifecycle))
        .await
        .with_context(|| format!("creating prepared startup snapshot {}", target.name))?;

    if cancel.is_cancelled() {
        bail!("interrupted while creating prepared startup snapshot");
    }

    // Acquire a shared generation lease immediately after the creator releases its exclusive
    // install lock. Keep it through verified cleanup and JSON publication.
    let cache = match install {
        SnapshotInstall::Created => PreparedCache::Created,
        SnapshotInstall::Existing => PreparedCache::Hit,
    };
    let prepared = verify_prepared_snapshot(&target, cache)
        .await?
        .with_context(|| {
            format!(
                "prepared startup snapshot {} no longer names the generation this prepare installed",
                target.name
            )
        })?;

    // Never acknowledge Healthy in the disposable source. It remains paused until cleanup;
    // dropping this ack only releases the monitor so its task can observe cancellation.
    drop(startup_ack);
    Ok(prepared)
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

fn finish_prepare<T>(operation: Result<T>, cleanup: Result<()>) -> Result<T> {
    match (operation, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(operation), Ok(())) => Err(operation),
        (Ok(_), Err(cleanup)) => Err(cleanup),
        (Err(operation), Err(cleanup)) => Err(anyhow::anyhow!(
            "prepare failed: {operation:#}; cleanup also failed: {cleanup:#}"
        )),
    }
}

/// Cancel `gate` on SIGTERM/SIGINT, describing the resulting shutdown as `shutdown_action`.
///
/// Installed before the VM is created so a signal during the (potentially long) setup phase
/// is recorded instead of killing the process and leaving host network state, the persisted
/// VM state file, and the data directory behind. Shutdown is deferred until setup finishes,
/// then the caller's cleanup runs.
fn spawn_lifecycle_signal_handler(
    gate: &super::common::LifecycleReadyGate,
    shutdown_action: &'static str,
) -> Result<()> {
    let gate = gate.clone();
    let mut sigterm = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
    let mut sigint = signal(SignalKind::interrupt()).context("installing SIGINT handler")?;
    tokio::spawn(async move {
        let signal_name = tokio::select! {
            _ = sigterm.recv() => "SIGTERM",
            _ = sigint.recv() => "SIGINT",
        };
        gate.cancel();
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
    spawn_lifecycle_signal_handler(&lifecycle_gate, "shutting down VM")?;

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
    let restore_args = restore_key.as_ref().map(|key| crate::cli::SnapshotRunArgs {
        pid: None,
        snapshot: Some(key.clone()),
        name: Some(ctx.args.name.clone()),
        exec: None,
        no_dirty_tracking: false,
        no_swap: false,
        vsock_dir: ctx.args.vsock_dir.clone(),
        startup_snapshot_base_key: ctx.args.health_check.as_ref().map(|_| key.clone()),
        cpu: Some(ctx.args.cpu),
        mem: Some(ctx.args.mem),
        firecracker_bin: None,
        firecracker_args: None,
        hugepages: Some(ctx.args.hugepages),
        non_blocking_output: ctx.args.non_blocking_output,
    });
    cleanup_vm_context(ctx).await;

    // The run loop stopped on purpose right after producing the pre-start
    // snapshot: relaunch through the restore path (identical to a cache hit).
    if let Some(snapshot_args) = restore_args {
        // A signal during the teardown window lands on a token nothing else
        // checks anymore (cmd_snapshot_run installs fresh handlers) — honor it
        // here or the "killed" VM would resurrect as a clone.
        if cancel.is_cancelled() {
            info!("shutdown requested during snapshot relaunch, not restoring");
            bail!("interrupted by signal during snapshot relaunch");
        }
        return super::snapshot::cmd_snapshot_run(snapshot_args).await;
    }

    // Propagate a missing exit code as an error and a non-zero exit code as a failure
    if let Some(code) = result? {
        if code != 0 {
            bail!("container exited with code {}", code);
        }
    }

    Ok(())
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
    spawn_lifecycle_signal_handler(&lifecycle_gate, "cancelling podman prepare")?;

    let lifecycle = PodmanLifecycle::Prepare(PrepareOptions {
        tag: args.tag,
        force: args.force,
    });
    match prepare_vm_for_lifecycle(args.run, lifecycle.clone()).await? {
        VmPreparation::Prepared(prepared) => {
            publish_prepared_snapshot_gated(&lifecycle_gate, &prepared)
        }
        VmPreparation::RunCompleted => {
            unreachable!("prepare lifecycle never runs a cached snapshot")
        }
        VmPreparation::Active(mut ctx) => {
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
            let prepared = finish_prepare(operation, cleanup)?;
            publish_prepared_snapshot_gated(&lifecycle_gate, &prepared)
        }
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
            nfs: vec![],
            env: vec![],
            cmd: None,
            publish: vec![],
            balloon: None,
            network: NetworkMode::Rootless,
            hypervisor: crate::cli::args::Hypervisor::Firecracker,
            health_check: None,
            health_check_timeout: 5,
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
            dns: None,
            image: "alpine:latest".to_string(),
            command_args: vec![],
            rootfs_override: None,
            image_disk_override: None,
        }
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
                extra,
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
    /// the snapshot cache. It only changes WHERE the clone's listener binds,
    /// and the restore mount redirect retargets the cached vmstate's embedded
    /// vsock directory to the caller-owned one (end-to-end pin:
    /// test_vsock_dir_honored_on_snapshot_cache_hit).
    #[test]
    fn a_custom_vsock_dir_still_participates_in_the_snapshot_cache() {
        let mut args = test_args();
        args.no_snapshot = false;
        args.vsock_dir = Some("/tmp/external-vsock".to_string());
        assert!(
            !snapshot_cache_opt_out(&args, false, false),
            "--vsock-dir must keep using the snapshot cache; the restore redirect honors it"
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
        assert!(!snapshot_cache_opt_out(&baseline(), false, false));

        let mut args = baseline();
        args.no_snapshot = true;
        assert!(snapshot_cache_opt_out(&args, false, false));

        let mut args = baseline();
        args.rootfs_override = Some(std::path::PathBuf::from("disk-only.raw"));
        assert!(snapshot_cache_opt_out(&args, false, false));

        let mut args = baseline();
        args.hypervisor = crate::cli::args::Hypervisor::CloudHypervisor;
        assert!(snapshot_cache_opt_out(&args, false, false));

        assert!(snapshot_cache_opt_out(&baseline(), true, false));
        assert!(snapshot_cache_opt_out(&baseline(), false, true));
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
            // The disposable VM stays up; only the container never reports healthy.
            std::future::pending::<Result<std::process::ExitStatus>>(),
        )
        .await
        .expect_err("a container that never becomes healthy must not hang prepare");

        assert_eq!(
            format!("{error:#}"),
            format!(
                "disposable prepare VM did not report a healthy container within {}s",
                PREPARE_HEALTH_BUDGET.as_secs()
            )
        );
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
        let error = await_prepare_healthy(startup_rx, &cancel, exited)
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
        )
        .unwrap();
        config.content_key = Some(snapshot_key.to_string());
        for path in [&config.memory_path, &config.vmstate_path, &config.disk_path] {
            tokio::fs::write(path, b"durable-artifact").await.unwrap();
        }
        tokio::fs::write(
            snapshot_dir.join("config.json"),
            serde_json::to_vec_pretty(&config).unwrap(),
        )
        .await
        .unwrap();

        let target = prepare_install_target(&PrepareOptions::default(), snapshot_key).unwrap();
        let prepared = verify_prepared_snapshot_in(temp.path(), &target, PreparedCache::Created)
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
        let error =
            match verify_prepared_snapshot_in(temp.path(), &target, PreparedCache::Hit).await {
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
            force: false,
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
        )
        .unwrap();
        config.content_key = content_key.map(str::to_string);
        for path in [&config.memory_path, &config.vmstate_path, &config.disk_path] {
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
        let hit = verify_prepared_snapshot_in(temp.path(), &matching, PreparedCache::Hit)
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
            verify_prepared_snapshot_in(temp.path(), &other_content, PreparedCache::Hit)
                .await
                .unwrap()
                .is_none(),
            "a tag holding other content must be a miss so prepare rebuilds it"
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
        let error =
            match verify_prepared_snapshot_in(temp.path(), &target, PreparedCache::Hit).await {
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
        let hit = verify_prepared_snapshot_in(temp.path(), &target, PreparedCache::Hit)
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
            )
            .unwrap();
            config.content_key = Some(snapshot_key.to_string());
            config.metadata.extra_disks = extra_disks;
            config.metadata.volumes = volumes;
            for path in [&config.memory_path, &config.vmstate_path, &config.disk_path] {
                tokio::fs::write(path, b"durable-artifact").await.unwrap();
            }
            for (name, bytes) in side_files {
                tokio::fs::write(snapshot_dir.join(name), bytes)
                    .await
                    .unwrap();
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
                verify_prepared_snapshot_in(temp, &target, PreparedCache::Hit)
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
            &[("disk-dir-0.raw", b"")],
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
                    ("disk-dir-0.raw", b"disk-bytes"),
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
            verify_prepared_snapshot_in(temp.path(), &target, PreparedCache::Hit)
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
mod image_disk_identity_classifier_tests {
    #[test]
    fn identity_mismatch_is_a_snapshot_load_failure() {
        // The restore-time identity check must trigger invalidate-and-fresh-boot,
        // not a hard run failure: its marker string is classified as a load failure.
        let err = anyhow::anyhow!(
            "image disk build changed during launch: /x is now a, snapshot/key was computed against b"
        );
        assert!(super::is_snapshot_load_failure(&err));
        let unrelated = anyhow::anyhow!("some other failure");
        assert!(!super::is_snapshot_load_failure(&unrelated));
    }
}
