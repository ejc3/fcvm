use anyhow::{bail, Context, Result};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use std::sync::atomic::AtomicBool;

use crate::cli::RunArgs;
use crate::firecracker::FirecrackerConfig;
use crate::hypervisor::Hypervisor;
use crate::network::{NetworkConfig, NetworkManager};
use crate::state::{StateManager, VmState};
use crate::volume::VolumeConfig;

/// Everything needed to re-run the Firecracker API configuration + boot for an
/// in-place relaunch (a guest reboot). Captured once during initial setup; the
/// host-side substrate (disk, network namespace/holder, vsock listeners) is reused
/// untouched, so a relaunch only replays the per-firecracker-child config.
///
/// Consumed by the shared `configure_and_boot_vm` primitive (vm_config.rs),
/// which both the initial boot and the reboot relaunch call.
pub struct RebootSpec {
    pub firecracker_bin: PathBuf,
    pub fc_args: Option<String>,
    /// Fully-resolved launch config (rootfs_path points at the per-VM CoW disk).
    pub launch_config: FirecrackerConfig,
    pub boot_args: String,
    pub track_dirty_pages: bool,
    pub image_disk_path: Option<PathBuf>,
    /// Build identity of that disk at boot time (see
    /// `FirecrackerConfig::image_disk_identity`); verified again on every
    /// re-attach so a reboot cannot pair the captured store with a rebuilt
    /// disk. None when snapshots are disabled or the disk is not an overlay
    /// storage image.
    pub image_disk_identity: Option<String>,
    pub vsock_socket_path: PathBuf,
    /// Whether the boot plan is delivered over vsock (VMMs without a metadata service)
    /// rather than MMDS. Baked into `boot_args` too (`fcvm_bootplan=vsock`), so an
    /// in-place reboot relaunch must re-serve over the same transport.
    pub bootplan_over_vsock: bool,
}

/// Where one `podman prepare` installs its startup snapshot, and what an already-installed
/// generation there has to look like to answer for that invocation.
///
/// Resolved once during setup and carried on [`VmContext`] so the pre-boot cache check and
/// the post-health install cannot disagree about the name, the type, or the content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedTarget {
    /// Snapshot name the generation is installed under: the content-addressed key, or the
    /// caller's `--tag`. Every consumer that takes a snapshot name addresses this.
    pub name: String,
    /// Content-addressed key whose content the generation must hold.
    pub content_key: String,
    /// `System` for the content-addressed cache entry, `User` for a `--tag` artifact.
    pub snapshot_type: crate::storage::SnapshotType,
    /// Whether a matching installed generation may be published without booting.
    /// Cleared by `--force`.
    pub publish_installed: bool,
    /// What to do with a generation already installed at `name` once the disposable
    /// source is healthy.
    pub existing: super::ExistingGeneration,
}

/// All state accumulated during VM setup, bundled for the event loop and cleanup.
pub struct VmContext {
    pub vm_id: String,
    pub vm_name: String,
    pub data_dir: PathBuf,
    pub vm_manager: Box<dyn Hypervisor>,
    pub holder_child: Option<tokio::process::Child>,
    /// Boot-plan vsock listener task (Some only when the plan is served over vsock for
    /// VMMs without a metadata service). Aborted during cleanup.
    pub bootplan_handle: Option<tokio::task::JoinHandle<()>>,
    pub volume_servers: crate::volume::SpawnedVolumes,
    pub network: Box<dyn NetworkManager>,
    pub network_config: NetworkConfig,
    pub state_manager: StateManager,
    pub health_cancel_token: CancellationToken,
    pub health_monitor_handle: tokio::task::JoinHandle<()>,
    pub status_handle: tokio::task::JoinHandle<()>,
    pub tty_handle: Option<std::thread::JoinHandle<Result<i32>>>,
    /// Host-side Unix socket path for the TTY vsock port (set when running with -t).
    /// Used to unblock the TTY accept thread if the guest never connected.
    pub tty_socket_path: Option<String>,
    pub output_handle: Option<tokio::task::JoinHandle<()>>,
    /// Egress proxy task (rootless mode only); aborted during cleanup.
    pub egress_proxy_handle: Option<tokio::task::JoinHandle<()>>,
    pub cache_rx: Option<mpsc::Receiver<CacheRequest>>,
    /// What this process knows the guest to be (see [`CacheVerdict`]). Written
    /// by the run loop BEFORE it resolves any cache oneshot; read by the
    /// status listener to answer every "cache-ready" ask.
    pub cache_verdict: SharedCacheVerdict,
    /// Startup-snapshot trigger from the health monitor. Carries the ack the
    /// snapshot path must send (or drop) before the monitor publishes Healthy.
    pub startup_rx: Option<oneshot::Receiver<crate::health::StartupSnapshotAck>>,
    pub snapshot_key: Option<String>,
    /// Set only for the `podman prepare` lifecycle: where its startup snapshot goes.
    pub prepare_target: Option<PreparedTarget>,
    pub volume_configs: Vec<VolumeConfig>,
    pub args: RunArgs,
    pub disk_path: PathBuf,
    pub log_tx: tokio::sync::broadcast::Sender<LogLine>,
    /// Notify the output listener to drop its current connection and re-accept.
    /// Triggered after each snapshot (vsock connections reset during snapshot).
    pub output_reconnect: Arc<tokio::sync::Notify>,
    /// VM state snapshot for cache snapshot creation. Config fields (image, vcpu,
    /// memory_mib, network, original_vsock_vm_id, etc.) are immutable after setup.
    pub vm_state: crate::state::VmState,
    /// Set by run_vm_loop right after the pre-start cache snapshot is created:
    /// instead of resuming this VM, fcvm tears it down and relaunches by
    /// restoring the snapshot it just produced, so the snapshot-miss path goes
    /// through the exact same restore flow as a snapshot hit. (Resuming the
    /// paused VM is the one lifecycle that intermittently starves Firecracker's
    /// device event loop — see #630.)
    pub restore_from_cache: Option<String>,
    /// Set by the status listener when the guest signals a reboot. run_vm_loop checks
    /// it when Firecracker exits and relaunches in place instead of terminating.
    pub reboot_requested: Arc<AtomicBool>,
    /// Set by the status listener when the container's "exit:" notification arrives.
    /// Distinguishes a real termination from a reboot in wait_for_reboot_decision and
    /// gates the exit-code read (the listener stays alive, so it can't be joined).
    pub container_exit_seen: Arc<AtomicBool>,
    /// Inputs to replay the Firecracker API config + boot on an in-place relaunch.
    pub reboot_spec: RebootSpec,
}

/// A log line from the VM's container output.
#[derive(Clone, Debug)]
pub struct LogLine {
    /// Stream type: "stdout", "stderr", or "system"
    pub stream: String,
    /// Line content
    pub content: String,
}

/// Handle to a running VM. Returned by `start_vm()`.
///
/// The VM runs in a background tokio task. Use `stop()` to gracefully shut it down
/// or `wait()` to wait for it to exit naturally.
///
/// On drop, the VM is cancelled automatically (the background task will clean up
/// resources on its next poll). For explicit shutdown with exit code, use `stop()`.
pub struct VmHandle {
    /// Unique VM identifier (e.g., "vm-abc123")
    pub vm_id: String,
    /// Human-readable VM name
    pub name: String,
    /// Process ID of the fcvm process managing this VM
    pub pid: u32,
    /// Exact host socket bound for this VM, including a custom `--vsock-dir`.
    pub(super) vsock_socket_path: PathBuf,
    pub(super) cancel: CancellationToken,
    pub(super) task: Option<tokio::task::JoinHandle<Result<Option<i32>>>>,
    pub(super) log_tx: tokio::sync::broadcast::Sender<LogLine>,
}

impl VmHandle {
    /// Get a clone of the cancellation token (for external cancellation).
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Get the vsock socket path for this VM (used for exec/terminal connections).
    pub fn vsock_socket_path(&self) -> PathBuf {
        self.vsock_socket_path.clone()
    }

    /// Gracefully stop the VM and wait for cleanup to complete.
    /// Returns the container exit code (None if the container didn't exit naturally).
    pub async fn stop(&mut self) -> Result<Option<i32>> {
        self.cancel.cancel();
        match self.task.take() {
            Some(task) => task.await?,
            None => Ok(None),
        }
    }

    /// Wait for the VM to exit naturally (without cancelling).
    /// Returns the container exit code.
    pub async fn wait(&mut self) -> Result<Option<i32>> {
        match self.task.take() {
            Some(task) => task.await?,
            None => Ok(None),
        }
    }

    /// Query current VM state (health, IP, ports, labels, etc.) from the state manager.
    pub async fn state(&self) -> Result<VmState> {
        let mgr = StateManager::new(crate::paths::state_dir());
        mgr.load_state(&self.vm_id).await
    }

    /// Subscribe to live container output (stdout/stderr) from this VM.
    /// Returns a broadcast receiver that gets each log line as it's produced.
    /// Late subscribers only see lines from the point of subscription.
    pub fn subscribe_logs(&self) -> tokio::sync::broadcast::Receiver<LogLine> {
        self.log_tx.subscribe()
    }
}

impl Drop for VmHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Request to create a podman cache snapshot.
/// Sent from status listener to main task when fc-agent signals cache-ready.
pub struct CacheRequest {
    /// Image digest from fc-agent
    pub digest: String,
    /// Oneshot channel to signal completion back to status listener
    pub ack_tx: oneshot::Sender<()>,
}

/// What the owning fcvm process knows the guest to be, for answering
/// "cache-ready" asks on the status port.
///
/// fc-agent cannot classify itself from transport events: the VMM queues a
/// vsock TRANSPORT_RESET into the guest at snapshot SAVE, so a resumed source
/// and a restored clone observe identical connection death. The guest instead
/// (re-)sends "cache-ready" until it gets a verdict, and the status listener
/// answers from this state — which the process that owns the VM maintains
/// from what it actually did:
///
/// - the podman-run loop starts at `Pending` (or `Continue` when snapshots
///   are disabled), moves to `Continue` once the snapshot decision is "keep
///   running this VM" (created-and-resumed, creation failed, or no snapshot
///   key), and to `Doomed` when the VM is being replaced by a restore of the
///   snapshot it just produced (the NV2 miss path) or shut down mid-decision;
/// - the restore path binds its listener with `Restored` BEFORE resuming the
///   clone, so a restored guest's re-ask can never be told to start cold;
/// - an in-place reboot resets to `Continue`: the rebooted guest cold-boots,
///   regardless of how its predecessor was classified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheVerdict {
    /// The snapshot decision has not been made — forward the ask to the run
    /// loop and keepalive until it answers.
    Pending,
    /// Continue cold in this VM: answer "cache-ack".
    Continue,
    /// This VM is a restored clone: answer "cache-restored" (the restore
    /// machinery in this same process drives readiness).
    Restored,
    /// This VM is being replaced or torn down: answer "cache-doomed", never
    /// "cache-ack" — a container launched here races the replacement clone.
    Doomed,
}

/// Shared verdict cell between the run/restore loop (writer) and the status
/// listener (reader). A plain mutex: transitions are rare and never held
/// across awaits.
pub type SharedCacheVerdict = Arc<std::sync::Mutex<CacheVerdict>>;

/// Fresh verdict cell.
pub fn shared_cache_verdict(initial: CacheVerdict) -> SharedCacheVerdict {
    Arc::new(std::sync::Mutex::new(initial))
}

/// Result of a snapshot creation attempt that can be interrupted by signals.
pub enum SnapshotOutcome {
    /// Snapshot created successfully
    Created,
    /// Snapshot creation failed
    Failed(anyhow::Error),
    /// Signal received during creation (caller should break and shutdown)
    Interrupted,
}

/// Parsed volume mapping from --map HOST:GUEST[:ro] specification.
pub(crate) struct VolumeMapping {
    pub host_path: PathBuf,
    pub guest_path: String,
    pub read_only: bool,
}

impl VolumeMapping {
    /// Parse a volume spec string: HOST:GUEST[:ro]
    pub fn parse(spec: &str) -> Result<Self> {
        let parts: Vec<&str> = spec.split(':').collect();
        if parts.len() < 2 {
            bail!("Invalid volume spec '{}': expected HOST:GUEST[:ro]", spec);
        }

        let host_path = PathBuf::from(parts[0]);
        let guest_path = parts[1].to_string();
        let read_only = parts.len() > 2 && parts[2] == "ro";

        // Validate host path exists
        if !host_path.exists() {
            bail!("Volume host path does not exist: {}", host_path.display());
        }

        // Validate guest path is absolute
        if !guest_path.starts_with('/') {
            bail!(
                "Volume guest path must be absolute: {} (from spec '{}')",
                guest_path,
                spec
            );
        }

        Ok(Self {
            host_path,
            guest_path,
            read_only,
        })
    }
}

/// The guest path of a `HOST:GUEST[:ro]` disk or NFS spec, when it has one.
fn guest_path_of_spec(spec: &str) -> Option<&str> {
    spec.strip_suffix(":ro")
        .unwrap_or(spec)
        .split_once(':')
        .map(|(_, guest)| guest)
}

/// Where fc-agent mounts the image disk of a run in overlay image mode
/// (`mount_overlay_image` in fc-agent/src/container.rs). fc-agent creates this
/// directory after the volumes are mounted, so to a read-only map around it it
/// is a mount point like any other.
/// `the_image_store_is_the_only_other_mount_point_fc_agent_creates` reads the
/// fc-agent source and fails when the two differ.
pub(crate) const IMAGE_STORE_MOUNT_POINT: &str = "/mnt/image-store";

/// The parsed `--map` arguments of a run, once no mount point of the run lies
/// inside a read-only map that does not have it
/// (`check_mount_points_inside_read_only_maps`).
///
/// Every path that boots a guest calls this first, because every boot runs an
/// fc-agent that mounts the volumes again: `fcvm podman run` before it sets
/// anything up, and each relaunch after a guest reboot.
/// `attaches_image_disk` is whether that boot gives the guest an image disk.
/// In overlay image mode fc-agent mounts it at `IMAGE_STORE_MOUNT_POINT`.
pub(crate) fn checked_volume_mappings(
    args: &RunArgs,
    attaches_image_disk: bool,
) -> Result<Vec<VolumeMapping>> {
    let maps = args
        .map
        .iter()
        .map(|s| VolumeMapping::parse(s))
        .collect::<Result<Vec<_>>>()
        .context("parsing volume mappings")?;
    let mounts_image_store = attaches_image_disk
        && super::resolve_image_mode(args) == crate::firecracker::ImageMode::Overlay;
    check_mount_points_inside_read_only_maps(
        &maps,
        &guest_mount_points(args, &maps, mounts_image_store),
    )?;
    Ok(maps)
}

/// Every guest path fc-agent creates and mounts something at for this run,
/// each with what asked for it: the `--map`, `--disk`, `--disk-dir` and
/// `--nfs` arguments, and the image store when the run mounts one.
fn guest_mount_points(
    args: &RunArgs,
    maps: &[VolumeMapping],
    mounts_image_store: bool,
) -> Vec<(String, String)> {
    let maps = args
        .map
        .iter()
        .zip(maps)
        .map(|(spec, map)| (format!("--map {spec}"), map.guest_path.clone()));
    let others = [
        ("--disk", &args.disk),
        ("--disk-dir", &args.disk_dir),
        ("--nfs", &args.nfs),
    ]
    .into_iter()
    .flat_map(|(flag, specs)| {
        specs.iter().filter_map(move |spec| {
            Some((
                format!("{flag} {spec}"),
                guest_path_of_spec(spec)?.to_string(),
            ))
        })
    });
    let image_store = mounts_image_store.then(|| {
        (
            format!("fcvm mounts the image store of {} in the guest", args.image),
            IMAGE_STORE_MOUNT_POINT.to_string(),
        )
    });
    maps.chain(others).chain(image_store).collect()
}

/// Refuse a mount point that fc-agent would have to create inside a read-only
/// map.
///
/// fc-agent mounts a read-only map read-only, so a mount point that lies
/// inside one cannot be created in the guest: it has to be a directory of the
/// map's host directory already. The innermost map around a mount point is the
/// filesystem the mount point is created in, so that is the one checked.
///
/// A symlink on the way is left to the guest, which resolves it in its own
/// namespace.
fn check_mount_points_inside_read_only_maps(
    maps: &[VolumeMapping],
    mount_points: &[(String, String)],
) -> Result<()> {
    for (argument, guest_path) in mount_points {
        let guest_path = Path::new(guest_path);
        let enclosing = maps
            .iter()
            .filter(|map| {
                let outer = Path::new(&map.guest_path);
                guest_path != outer && guest_path.starts_with(outer)
            })
            .max_by_key(|map| Path::new(&map.guest_path).components().count());
        let Some(outer) = enclosing.filter(|map| map.read_only) else {
            continue;
        };
        let inside = guest_path
            .strip_prefix(&outer.guest_path)
            .expect("the map's guest path is a prefix of the mount point");
        let mut on_host = outer.host_path.clone();
        for component in inside.components() {
            let Component::Normal(name) = component else {
                break;
            };
            on_host.push(name);
            let found = std::fs::symlink_metadata(&on_host);
            match &found {
                Ok(meta) if meta.is_dir() => continue,
                Ok(meta) if meta.file_type().is_symlink() => break,
                _ => {}
            }
            let detail = found.err().map(|e| format!(" ({e})")).unwrap_or_default();
            bail!(
                "{argument}: {} is inside the read-only map --map {}:{}:ro, which is mounted \
                 read-only in the guest, so the mount point cannot be created there, and {} is \
                 not a directory on the host{detail}. Create that directory on the host, or map \
                 {} without :ro.",
                guest_path.display(),
                outer.host_path.display(),
                outer.guest_path,
                on_host.display(),
                outer.guest_path
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(host: &Path, guest: &str, read_only: bool) -> VolumeMapping {
        VolumeMapping {
            host_path: host.to_path_buf(),
            guest_path: guest.to_string(),
            read_only,
        }
    }

    fn mount_point(argument: &str, guest: &str) -> (String, String) {
        (argument.to_string(), guest.to_string())
    }

    /// A map inside a read-only map whose mount point the host directory does
    /// not have fails before boot, and the error names both maps and the
    /// directory to create. With that directory on the host the same maps pass.
    ///
    /// RED BEFORE THE FIX: nothing was checked. The VM booted and fc-agent
    /// created the mount point on the host through the outer map, which the
    /// guest had mounted read-write.
    #[test]
    fn a_mount_point_missing_from_a_read_only_map_fails_and_names_both_maps() {
        let outer = tempfile::tempdir().unwrap();
        let inner = tempfile::tempdir().unwrap();
        let maps = [
            map(outer.path(), "/data", true),
            map(inner.path(), "/data/cache", false),
        ];
        let mount_points = [
            mount_point("--map outer:/data:ro", "/data"),
            mount_point("--map inner:/data/cache", "/data/cache"),
        ];

        let error = check_mount_points_inside_read_only_maps(&maps, &mount_points)
            .expect_err("a mount point the read-only outer map does not have was accepted")
            .to_string();
        let missing = outer.path().join("cache");
        for named in [
            "--map inner:/data/cache".to_string(),
            format!("--map {}:/data:ro", outer.path().display()),
            missing.display().to_string(),
        ] {
            assert!(
                error.contains(&named),
                "the error does not name {named}: {error}"
            );
        }

        std::fs::create_dir(&missing).unwrap();
        check_mount_points_inside_read_only_maps(&maps, &mount_points)
            .expect("the mount point is a directory of the outer map's host directory");
    }

    /// What decides: the innermost map around the mount point, whether that
    /// map is read-only, and what its host directory has at the mount point.
    #[test]
    fn only_a_read_only_innermost_map_needs_the_mount_point_on_the_host() {
        let outer = tempfile::tempdir().unwrap();
        let middle = tempfile::tempdir().unwrap();
        std::fs::create_dir(outer.path().join("middle")).unwrap();
        std::fs::write(outer.path().join("file"), "not a directory").unwrap();
        std::os::unix::fs::symlink("/somewhere/in/the/guest", outer.path().join("link")).unwrap();

        let read_only = [map(outer.path(), "/a", true)];
        let read_write = [map(outer.path(), "/a", false)];
        let nested = [
            map(outer.path(), "/a", true),
            map(middle.path(), "/a/middle", false),
        ];

        let (read_only, read_write, nested) = (
            read_only.as_slice(),
            read_write.as_slice(),
            nested.as_slice(),
        );
        // (maps, mount point, whether the run is refused)
        let cases: Vec<(&[VolumeMapping], (String, String), bool)> = vec![
            // A read-write outer map: fc-agent creates the mount point through it.
            (read_write, mount_point("--map x:/a/new", "/a/new"), false),
            // Compared by path component: /ab is not inside /a.
            (read_only, mount_point("--map x:/ab", "/ab"), false),
            // A map is not inside itself.
            (read_only, mount_point("--map x:/a:ro", "/a"), false),
            // Disks and NFS shares are mounted by fc-agent too.
            (
                read_only,
                mount_point("--disk-dir x:/a/disk", "/a/disk"),
                true,
            ),
            (
                read_only,
                mount_point("--nfs x:/a/middle", "/a/middle"),
                false,
            ),
            // The parent is missing as well.
            (
                read_only,
                mount_point("--map x:/a/new/deep", "/a/new/deep"),
                true,
            ),
            // Something other than a directory is in the way.
            (read_only, mount_point("--map x:/a/file", "/a/file"), true),
            // The guest resolves a symlink in its own namespace.
            (
                read_only,
                mount_point("--map x:/a/link/sub", "/a/link/sub"),
                false,
            ),
            // The innermost map is read-write, so its mount point is created in it.
            (
                nested,
                mount_point("--map x:/a/middle/new", "/a/middle/new"),
                false,
            ),
        ];
        for (maps, point, refused) in cases {
            let result =
                check_mount_points_inside_read_only_maps(maps, std::slice::from_ref(&point));
            assert_eq!(
                result.is_err(),
                refused,
                "{} with {} map(s): {result:?}",
                point.0,
                maps.len()
            );
            if let Err(error) = result {
                assert!(
                    error.to_string().starts_with(&point.0),
                    "the error does not start with the argument {}: {error}",
                    point.0
                );
            }
        }
    }

    /// The RunArgs of `fcvm podman run --name test <extra>`, parsed by clap.
    fn parse_run(extra: &[&str]) -> RunArgs {
        use clap::Parser;
        let mut argv = vec!["fcvm", "podman", "run", "--name", "test"];
        argv.extend_from_slice(extra);
        let cli = crate::cli::Cli::try_parse_from(argv).expect("the command line parses");
        match cli.cmd {
            crate::cli::Commands::Podman(podman) => match podman.cmd {
                crate::cli::PodmanCommands::Run(run) => run,
                crate::cli::PodmanCommands::Prepare(_) => panic!("expected podman run"),
            },
            _ => panic!("expected podman run"),
        }
    }

    /// Every argument that makes fc-agent mount something is listed with its
    /// guest path, with and without `:ro`, in the order maps, disks, disk
    /// directories, NFS shares, and the image store last when the run mounts
    /// one.
    #[test]
    fn every_mounting_argument_is_listed_with_its_guest_path() {
        let host = tempfile::tempdir().unwrap();
        let host = host.path().display().to_string();
        let specs = [
            ("--map", format!("{host}:/data")),
            ("--map", format!("{host}:/data/ro:ro")),
            ("--disk", "/images/a.raw:/disk".to_string()),
            ("--disk", "/images/b.raw:/disk/ro:ro".to_string()),
            ("--disk-dir", "/dirs/a:/disk-dir".to_string()),
            ("--disk-dir", "/dirs/b:/disk-dir/ro:ro".to_string()),
            ("--nfs", "/shares/a:/nfs".to_string()),
            ("--nfs", "/shares/b:/nfs/ro:ro".to_string()),
        ];
        // Flags interleaved, so the order of the list is not the order typed.
        let mut argv: Vec<&str> = Vec::new();
        for (flag, spec) in specs.iter().rev() {
            argv.extend([*flag, spec.as_str()]);
        }
        argv.push("localhost/app:latest");
        let args = parse_run(&argv);
        let maps: Vec<VolumeMapping> = args
            .map
            .iter()
            .map(|spec| VolumeMapping::parse(spec).unwrap())
            .collect();
        assert_eq!(
            maps.iter().map(|map| map.read_only).collect::<Vec<_>>(),
            [true, false],
            "the maps as parsed"
        );

        let listed = |argument: String, guest: &str| (argument, guest.to_string());
        let mut want = vec![
            listed(format!("--map {host}:/data/ro:ro"), "/data/ro"),
            listed(format!("--map {host}:/data"), "/data"),
            listed("--disk /images/b.raw:/disk/ro:ro".into(), "/disk/ro"),
            listed("--disk /images/a.raw:/disk".into(), "/disk"),
            listed("--disk-dir /dirs/b:/disk-dir/ro:ro".into(), "/disk-dir/ro"),
            listed("--disk-dir /dirs/a:/disk-dir".into(), "/disk-dir"),
            listed("--nfs /shares/b:/nfs/ro:ro".into(), "/nfs/ro"),
            listed("--nfs /shares/a:/nfs".into(), "/nfs"),
        ];
        assert_eq!(guest_mount_points(&args, &maps, false), want);

        want.push(listed(
            "fcvm mounts the image store of localhost/app:latest in the guest".into(),
            IMAGE_STORE_MOUNT_POINT,
        ));
        assert_eq!(guest_mount_points(&args, &maps, true), want);

        assert_eq!(guest_path_of_spec("/images/a.raw"), None);
    }

    /// The host's list of mount points is fc-agent's. Besides the mount points
    /// of the plan's volumes, disks and NFS shares (fc-agent/src/mounts.rs),
    /// the one directory fc-agent creates with an error that fails the boot is
    /// the image store, at the path this file checks, and it creates it after
    /// the volumes are mounted.
    #[test]
    fn the_image_store_is_the_only_other_mount_point_fc_agent_creates() {
        let read = |file: &str| {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("fc-agent/src")
                .join(file);
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        };
        // Every create_dir_all whose failure is not thrown away, per file.
        let fatal_creations = |file: &str| -> Vec<String> {
            read(file)
                .lines()
                .map(str::trim)
                .filter(|line| line.contains("create_dir_all(") && !line.starts_with("let _ ="))
                .map(str::to_string)
                .collect()
        };

        let image_store = format!("let mount_path = \"{IMAGE_STORE_MOUNT_POINT}\";");
        let container = read("container.rs");
        let mount_overlay_image = &container[container
            .find("pub fn mount_overlay_image(")
            .expect("mount_overlay_image is in container.rs")..];
        let mount_overlay_image =
            &mount_overlay_image[..mount_overlay_image.find("\n}\n").unwrap()];
        assert!(
            mount_overlay_image.contains(&image_store),
            "fc-agent does not mount the image store at {IMAGE_STORE_MOUNT_POINT}. Expected \
             line in mount_overlay_image: {image_store}"
        );
        assert_eq!(
            fatal_creations("container.rs"),
            ["std::fs::create_dir_all(mount_path).context(\"creating image store mount point\")?;"],
            "fc-agent/src/container.rs creates another directory whose failure fails the boot. \
             If it does so after the volumes are mounted, add it to guest_mount_points"
        );
        assert_eq!(
            fatal_creations("agent.rs"),
            Vec::<String>::new(),
            "fc-agent/src/agent.rs creates a directory whose failure fails the boot. If it \
             does so after the volumes are mounted, add it to guest_mount_points"
        );
        assert_eq!(
            fatal_creations("mounts.rs"),
            [
                "if let Err(e) = std::fs::create_dir_all(&vol.guest_path) {",
                "if let Err(e) = std::fs::create_dir_all(&disk.mount_path) {",
                "if let Err(e) = std::fs::create_dir_all(&share.mount_path) {",
            ],
            "fc-agent/src/mounts.rs creates a mount point guest_mount_points does not know"
        );

        let agent = read("agent.rs");
        let volumes = agent
            .find("mounts::mount_fuse_volumes(")
            .expect("agent.rs mounts the FUSE volumes");
        let image = agent
            .find("container::mount_overlay_image(")
            .expect("agent.rs mounts the overlay image");
        assert!(
            volumes < image,
            "fc-agent mounts the image store before the volumes, so a map can cover it"
        );
    }
}
