use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{debug, info, warn};

use super::podman::{
    check_podman_snapshot, create_snapshot_interruptible, startup_snapshot_key,
    CreateSnapshotParams, SnapshotOutcome,
};
use crate::cli::args::RunArgs;
use crate::cli::{
    SnapshotArgs, SnapshotCommands, SnapshotCreateArgs, SnapshotRunArgs, SnapshotServeArgs,
};
use crate::firecracker::FcNetworkMode;
use crate::network::{BridgedNetwork, NetworkManager, PastaNetwork, RoutedNetwork};
use crate::paths;
use crate::state::{
    generate_vm_id, truncate_id, validate_vm_name, StateManager, VmState, VmStatus,
};
use crate::storage::SnapshotManager;
use crate::uffd::UffdServer;
use crate::volume::VolumeConfig;

use super::common::{
    MemoryBackend, RestoreParams, RuntimeConfig, SnapshotRestoreConfig, VSOCK_OUTPUT_PORT,
    VSOCK_STATUS_PORT, VSOCK_TTY_PORT,
};
use super::podman::{run_output_listener, run_status_listener};

/// Main dispatcher for snapshot commands
pub async fn cmd_snapshot(args: SnapshotArgs) -> Result<()> {
    match args.cmd {
        SnapshotCommands::Create(create_args) => cmd_snapshot_create(create_args).await,
        SnapshotCommands::Serve(serve_args) => cmd_snapshot_serve(serve_args).await,
        SnapshotCommands::Run(run_args) => cmd_snapshot_run(run_args).await,
        SnapshotCommands::Ls => cmd_snapshot_ls().await,
    }
}

async fn snapshot_restore_runtime_config(
    args: &SnapshotRunArgs,
    kernel_profile: Option<&str>,
) -> RuntimeConfig {
    let mut config = RuntimeConfig {
        firecracker_bin: args.firecracker_bin.as_ref().map(PathBuf::from),
        firecracker_args: args.firecracker_args.clone(),
        boot_args: None,
        fuse_readers: None,
    };

    // If no explicit firecracker_bin, resolve the Firecracker (and its args) from
    // the SNAPSHOT's kernel profile — the clone must run on the same binary the
    // snapshot was created with. Resolving "default" for a nested-profile
    // snapshot would restore a vEL2 (NV2) guest on a Firecracker without NV2
    // support. Profiles that define no firecracker of their own (e.g. btrfs)
    // were CREATED on the default profile's custom Firecracker (prepare_vm
    // falls back to it), so the restore must fall back the same way — a plain
    // PATH `firecracker` would be a different binary than created the snapshot.
    if config.firecracker_bin.is_none() {
        let profile_name = kernel_profile.unwrap_or("default");
        for candidate in [profile_name, "default"] {
            let Ok(Some(profile)) = crate::setup::get_kernel_profile(candidate) else {
                continue;
            };
            if config.firecracker_args.is_none() {
                config.firecracker_args = profile.firecracker_args.clone();
            }
            if profile.firecracker_repo.is_none() {
                continue;
            }
            match crate::setup::get_firecracker_for_profile(&profile, candidate).await {
                Ok(fc_path) => {
                    config.firecracker_bin = Some(fc_path);
                }
                Err(e) => {
                    warn!(error = %e, profile = candidate, "custom Firecracker not found for snapshot restore, falling back to system binary");
                }
            }
            break;
        }
    }

    config
}

/// Load the VM state targeted by `snapshot create` (selected via --name or --pid).
///
/// The state file is updated concurrently by the VM-owning process (health monitor,
/// startup snapshot), so callers re-read it whenever they need a current copy instead
/// of reusing an earlier read.
async fn load_snapshot_create_target(
    state_manager: &StateManager,
    args: &SnapshotCreateArgs,
) -> Result<VmState> {
    if let Some(name) = &args.name {
        state_manager.load_state_by_name(name).await
    } else if let Some(pid) = args.pid {
        state_manager.load_state_by_pid(pid).await
    } else {
        anyhow::bail!("Either --name or --pid must be specified");
    }
}

/// Create snapshot from running VM
async fn cmd_snapshot_create(args: SnapshotCreateArgs) -> Result<()> {
    use super::common::VSOCK_VOLUME_PORT_BASE;
    use crate::storage::snapshot::{SnapshotType, SnapshotVolumeConfig};

    // Determine which VM to snapshot
    let state_manager = StateManager::new(paths::state_dir());

    if let Some(name) = &args.name {
        info!("Creating snapshot from VM: {}", name);
    } else if let Some(pid) = args.pid {
        info!("Creating snapshot from VM with PID: {}", pid);
    }
    let vm_state = load_snapshot_create_target(&state_manager, &args)
        .await
        .context("loading VM state")?;

    // Fail fast: the disk-only capture path (freeze -> reflink -> unfreeze, no
    // memory dump) is dispatched below when args.disk_only is set.

    // Block snapshots when VM has read-write extra disks
    let rw_disks: Vec<_> = vm_state
        .config
        .extra_disks
        .iter()
        .filter(|d| !d.read_only)
        .collect();
    if !rw_disks.is_empty() {
        anyhow::bail!(
            "Cannot create snapshot: VM has {} read-write extra disk(s). \
             Use :ro suffix for disks that should be included in snapshots.",
            rw_disks.len()
        );
    }

    let snapshot_name = args.tag.clone().unwrap_or_else(|| {
        vm_state
            .name
            .clone()
            .unwrap_or_else(|| truncate_id(&vm_state.vm_id, 8).to_string())
    });

    // Connect to running VM
    let socket_path = paths::vm_runtime_dir(&vm_state.vm_id).join("firecracker.sock");

    // Check if socket exists
    if !socket_path.exists() {
        anyhow::bail!(
            "VM socket not found - VM may not be running: {}",
            socket_path.display()
        );
    }

    // Check VM disk exists
    let vm_disk_path = paths::vm_runtime_dir(&vm_state.vm_id).join("disks/rootfs.raw");
    if !vm_disk_path.exists() {
        anyhow::bail!("VM disk not found at {}", vm_disk_path.display());
    }

    // The control client is created per-backend in the memory-snapshot branch below
    // (FirecrackerClient vs ChClient on the same socket path). The disk-only path needs
    // no control client (it quiesces the guest over vsock).

    let snapshot_dir = paths::snapshot_dir().join(&snapshot_name);

    // Parse volume configs from VM state (format: HOST:GUEST[:ro])
    let volume_configs: Vec<SnapshotVolumeConfig> = vm_state
        .config
        .volumes
        .iter()
        .enumerate()
        .filter_map(|(idx, spec)| {
            let parts: Vec<&str> = spec.split(':').collect();
            if parts.len() >= 2 {
                Some(SnapshotVolumeConfig {
                    host_path: PathBuf::from(parts[0]),
                    guest_path: parts[1].to_string(),
                    read_only: parts.get(2).map(|s| *s == "ro").unwrap_or(false),
                    vsock_port: VSOCK_VOLUME_PORT_BASE + idx as u32,
                    portable: vm_state.config.portable_volumes,
                })
            } else {
                warn!("Invalid volume spec in VM state: {}", spec);
                None
            }
        })
        .collect();

    let extra_disk_configs = super::common::extra_disks_to_snapshot(&vm_state);

    // Build snapshot config from VmState (single source of truth)
    let mut snapshot_config = super::common::build_snapshot_config(
        &vm_state,
        &snapshot_name,
        SnapshotType::User,
        &snapshot_dir,
        volume_configs,
        extra_disk_configs,
    );
    if args.disk_only {
        snapshot_config.kind = crate::storage::SnapshotKind::DiskOnly;
    }

    // Acquire the per-snapshot lock (exclusive) BEFORE the per-VM lock — same order as
    // create_podman_snapshot. Re-creating an existing tag atomically swaps the snapshot
    // directory; restores of this snapshot hold the same lock shared, so a concurrent
    // `fcvm snapshot run --snapshot <tag>` can never pair one generation's disk.raw with
    // another generation's memory.bin.
    tokio::fs::create_dir_all(paths::snapshot_dir())
        .await
        .context("creating snapshot directory")?;
    let _snapshot_lock = super::common::acquire_snapshot_dir_lock(&snapshot_dir, true).await?;

    // Acquire per-VM lock BEFORE reading the parent snapshot key.
    // Without this, a concurrent startup snapshot can complete between our state read
    // and the actual Firecracker snapshot — resetting the KVM dirty bitmap while we
    // hold a stale parent reference. The merged result would be missing all boot-time
    // memory changes, causing kernel panics on clone restore.
    let _vm_lock = super::common::acquire_vm_snapshot_lock(&vm_disk_path).await?;

    // Re-read state under lock to get the current parent snapshot key.
    // The startup snapshot may have updated snapshot_name since our initial read.
    let fresh_state = load_snapshot_create_target(&state_manager, &args)
        .await
        .context("re-reading VM state under lock")?;
    let parent_dir = fresh_state
        .config
        .snapshot_name
        .as_ref()
        .map(|name| paths::snapshot_dir().join(name));

    if args.disk_only {
        // Disk-only: no vCPU pause, no memory dump — fsfreeze the guest over the
        // exec vsock, reflink the disk, unfreeze. Cold-boot clones run from it.
        // Cold-boot clones can't re-attach extra disks yet, so a capture WITH them
        // would only produce snapshots that `snapshot run` then rejects — fail at
        // capture time instead, where the user can act on it.
        if !vm_state.config.extra_disks.is_empty() {
            bail!(
                "--disk-only does not support VMs with extra disks yet ({} attached); \
                 cold-boot clones cannot re-attach them",
                vm_state.config.extra_disks.len()
            );
        }
        let vsock_socket = paths::vm_runtime_dir(&vm_state.vm_id).join("vsock.sock");
        super::common::create_disk_only_snapshot_core(
            snapshot_config.clone(),
            &vm_disk_path,
            &vsock_socket,
        )
        .await?;
    } else {
        // Memory snapshot: drive the VM's actual control plane. Both backends listen on
        // the same `firecracker.sock` path; the client type + snapshot mechanism differ.
        match vm_state.config.hypervisor {
            crate::hypervisor::Backend::Firecracker => {
                use crate::firecracker::FirecrackerClient;
                let client = FirecrackerClient::new(socket_path.clone())?;
                super::common::create_snapshot_core(
                    &client,
                    snapshot_config.clone(),
                    &vm_disk_path,
                    parent_dir.as_deref(),
                    None,
                )
                .await?;
            }
            crate::hypervisor::Backend::CloudHypervisor => {
                let client =
                    crate::hypervisor::cloud_hypervisor::api::ChClient::new(socket_path.clone());
                super::common::create_snapshot_ch(&client, snapshot_config.clone(), &vm_disk_path)
                    .await?;
            }
        }
    }

    // Track this snapshot as the latest base for future diff snapshots.
    // Use a locked read-modify-write so we only change snapshot_name — this
    // process's copy of the state is minutes old by now, and a whole-state save
    // would clobber fields the VM owner wrote in the meantime (health status,
    // exit code, its own startup-snapshot key).
    // A disk-only tag has no memory image — recording it as snapshot_name would
    // poison the diff lineage (the next `snapshot create` would silently take a
    // full snapshot instead of a diff against the still-valid memory parent).
    let recorded = if args.disk_only {
        Ok(Some(()))
    } else {
        let recorded_snapshot_name = snapshot_name.clone();
        state_manager
            .update_state(&vm_state.vm_id, |state| {
                state.config.snapshot_name = Some(recorded_snapshot_name);
            })
            .await
            .map(|opt| opt.map(|_| ()))
    }
    .context("saving snapshot name to VM state")?;
    if recorded.is_none() {
        warn!(
            vm_id = %vm_state.vm_id,
            "VM state file no longer exists; snapshot base not recorded"
        );
    }

    // Print user-friendly output
    let vm_name = vm_state
        .name
        .as_deref()
        .unwrap_or(truncate_id(&vm_state.vm_id, 8));
    if args.disk_only {
        println!(
            "✓ Disk-only snapshot '{}' created from VM '{}'",
            snapshot_name, vm_name
        );
        println!("  Kind: disk-only (no memory image)");
        println!("  Files:");
        println!("    {}", snapshot_config.disk_path.display());
        println!(
            "\nOriginal VM '{}' is still running (was briefly frozen for disk copy).",
            vm_name
        );
    } else {
        println!(
            "✓ Snapshot '{}' created from VM '{}'",
            snapshot_name, vm_name
        );
        println!("  Memory: {} MB", snapshot_config.metadata.memory_mib);
        println!("  Files:");
        // Cloud Hypervisor writes its own snapshot (config.json/state.json/memory ranges)
        // into the `ch/` subdir; Firecracker writes memory.bin alongside.
        if vm_state.config.hypervisor == crate::hypervisor::Backend::CloudHypervisor {
            println!(
                "    {}/",
                snapshot_dir
                    .join(super::common::CH_SNAPSHOT_SUBDIR)
                    .display()
            );
        } else {
            println!("    {}", snapshot_config.memory_path.display());
        }
        println!("    {}", snapshot_config.disk_path.display());
        println!(
            "\nOriginal VM '{}' has been resumed and is still running.",
            vm_name
        );
    }

    Ok(())
}

/// How long serve shutdown waits for clones to exit after SIGTERM before escalating to SIGKILL.
const CLONE_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// Reject a disk-only snapshot in the memory-restore (UFFD) paths — `snapshot
/// serve` and `snapshot run --pid/--snapshot` operate on a memory image, which a
/// disk-only snapshot does not have. Clones of a disk-only tag must cold-boot
/// (the P4 `snapshot run --tag` dispatcher), not resume. Guarding here keeps the
/// old paths from panicking on a missing memory.bin.
fn ensure_not_disk_only(kind: crate::storage::SnapshotKind, command: &str) -> Result<()> {
    if kind == crate::storage::SnapshotKind::DiskOnly {
        anyhow::bail!(
            "snapshot is disk-only (no memory image); `{command}` needs a full \
             snapshot. Disk-only snapshots are cold-booted, not resumed (cold-boot \
             support is in progress)."
        );
    }
    Ok(())
}

/// Serve snapshot memory (foreground)
async fn cmd_snapshot_serve(args: SnapshotServeArgs) -> Result<()> {
    info!(
        "Starting memory server for snapshot: {}",
        args.snapshot_name
    );

    // Load snapshot configuration
    let snapshot_manager = SnapshotManager::new(paths::snapshot_dir());
    let snapshot_config = snapshot_manager
        .load_snapshot(&args.snapshot_name)
        .await
        .context("loading snapshot configuration")?;
    ensure_not_disk_only(snapshot_config.kind, "snapshot serve")?;

    info!(
        snapshot = %args.snapshot_name,
        mem_file = %snapshot_config.memory_path.display(),
        mem_size_mb = snapshot_config.metadata.memory_mib,
        "loaded snapshot configuration"
    );

    // Generate unique socket name with PID to allow multiple serves per snapshot
    let my_pid = std::process::id();
    let socket_path =
        paths::data_dir().join(format!("uffd-{}-{}.sock", args.snapshot_name, my_pid));

    // Create UFFD server with custom socket path
    let server = UffdServer::new_with_path(
        args.snapshot_name.clone(),
        &snapshot_config.memory_path,
        &socket_path,
    )
    .await
    .context("creating UFFD server")?;

    // Save serve state for tracking
    let serve_id = generate_vm_id();
    let mut serve_state = VmState::new(serve_id.clone(), "".to_string(), 0, 0);
    serve_state.pid = Some(my_pid);
    serve_state.config.snapshot_name = Some(args.snapshot_name.clone());
    serve_state.config.process_type = Some(crate::state::ProcessType::Serve);
    serve_state.status = VmStatus::Running;

    let state_manager = Arc::new(StateManager::new(paths::state_dir()));
    state_manager.init().await?;
    state_manager
        .save_state(&serve_state)
        .await
        .context("saving serve state")?;

    info!(
        serve_id = %serve_id,
        pid = my_pid,
        "serve state saved"
    );

    println!("Serving snapshot: {}", args.snapshot_name);
    println!("  Serve PID: {}", my_pid);
    println!("  Socket: {}", socket_path.display());
    println!("  Memory: {} MB", snapshot_config.metadata.memory_mib);
    println!("  Waiting for VMs to connect...");
    println!();
    println!("Clone VMs with: fcvm snapshot run --pid {}", my_pid);
    println!("Press Ctrl-C to stop");
    println!();

    // Setup signal handlers
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    // Run server in background task with cancellation token
    let server_cancel = tokio_util::sync::CancellationToken::new();
    let server_cancel_clone = server_cancel.clone();
    let mut server_handle = tokio::spawn(async move { server.run(server_cancel_clone).await });

    // Clone state_manager for signal handler use
    let state_manager_for_signal = state_manager.clone();

    // Wait for signal or server exit
    // First Ctrl-C warns about clones, second one shuts down
    //
    // Shutdown ordering matters: clones are stopped BEFORE the UFFD server is cancelled
    // (see the cleanup below). Cancelling the server first would close the clones'
    // userfaultfds while they are still running, and the kernel would then resolve their
    // page faults with zero pages instead of snapshot contents — silent memory corruption.
    let mut shutdown_requested = false;
    let mut confirm_deadline: Option<tokio::time::Instant> = None;
    let mut server_exited = false;
    loop {
        let timeout = if let Some(deadline) = confirm_deadline {
            tokio::time::sleep_until(deadline)
        } else {
            // Far future - effectively disabled
            tokio::time::sleep(std::time::Duration::from_secs(86400))
        };

        tokio::select! {
            biased;

            _ = sigterm.recv() => {
                info!("received SIGTERM");
                break;
            }
            _ = sigint.recv() => {
                info!("received SIGINT");
                if shutdown_requested {
                    // Second Ctrl-C - force shutdown
                    info!("received second SIGINT, forcing shutdown");
                    println!("\nForcing shutdown...");
                    break;
                }

                // First Ctrl-C - check for running clones
                let all_vms: Vec<crate::state::VmState> = state_manager_for_signal.list_vms().await?;
                let running_clones: Vec<crate::state::VmState> = all_vms
                    .into_iter()
                    .filter(|vm| vm.config.serve_pid == Some(my_pid))
                    .filter(|vm| vm.pid.map(crate::utils::is_process_alive).unwrap_or(false))
                    .collect();

                if running_clones.is_empty() {
                    println!("\nNo running clones, shutting down...");
                    break;
                } else {
                    println!("\n⚠️  {} clone(s) still running!", running_clones.len());
                    for clone in &running_clones {
                        if let Some(pid) = clone.pid {
                            let name = clone.name.as_deref().unwrap_or(&clone.vm_id);
                            println!("   - {} (PID {})", name, pid);
                        }
                    }
                    println!("\nPress Ctrl-C again within 3 seconds to kill clones and shut down...");
                    shutdown_requested = true;
                    confirm_deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(3));
                }
            }
            _ = timeout, if shutdown_requested => {
                println!("Timeout expired, continuing to serve...");
                shutdown_requested = false;
                confirm_deadline = None;
            }
            result = &mut server_handle => {
                info!("server exited: {:?}", result);
                server_exited = true;
                break;
            }
        }
    }

    println!("\nShutting down memory server...");

    // Cleanup ordering: stop clones FIRST, then the UFFD server.
    //
    // The per-VM page-fault handlers own the clones' userfaultfds. Cancelling the server
    // before the clones have exited closes those uffds while the clones are still running,
    // and the kernel then resolves their outstanding/future faults with zero pages instead
    // of snapshot contents — silent guest memory corruption that can be flushed to host
    // volumes. So: signal the clones, wait for them to exit (SIGKILL stragglers after a
    // bounded timeout), and only then cancel the server and remove the socket/state.
    info!("cleaning up clones connected to serve PID {}", my_pid);
    let my_clones: Vec<crate::state::VmState> = match state_manager.list_vms().await {
        Ok(all_vms) => all_vms
            .into_iter()
            .filter(|vm| vm.config.serve_pid == Some(my_pid))
            .collect(),
        Err(e) => {
            warn!("failed to list VMs during serve shutdown: {}", e);
            Vec::new()
        }
    };

    // Only signal PIDs that are alive AND still belong to an fcvm process. State files
    // outlive crashed clones, so a recorded PID may have been reused by an unrelated
    // process — never send signals to a PID we cannot identify as one of our clones.
    let clone_pids: Vec<u32> = my_clones
        .iter()
        .filter_map(|clone| {
            let pid = clone.pid?;
            let clone_id = truncate_id(&clone.vm_id, 8);
            if !crate::utils::is_process_alive(pid) {
                debug!("clone {} (PID {}) already exited", clone_id, pid);
                return None;
            }
            if !crate::utils::is_same_process_name(pid) {
                warn!(
                    "PID {} recorded for clone {} no longer belongs to an fcvm process (PID reuse), not signalling it",
                    pid, clone_id
                );
                return None;
            }
            info!("stopping clone {} (PID {})", clone_id, pid);
            Some(pid)
        })
        .collect();

    if !clone_pids.is_empty() {
        println!("Stopping {} clone(s)...", clone_pids.len());

        for &pid in &clone_pids {
            if let Err(e) = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            ) {
                warn!("failed to send SIGTERM to clone PID {}: {}", pid, e);
            }
        }

        // Wait for the clones to exit; escalate to SIGKILL after the timeout.
        let term_deadline = tokio::time::Instant::now() + CLONE_SHUTDOWN_TIMEOUT;
        while clone_pids
            .iter()
            .any(|&pid| crate::utils::is_process_alive(pid))
        {
            if tokio::time::Instant::now() >= term_deadline {
                for &pid in &clone_pids {
                    if crate::utils::is_process_alive(pid) {
                        warn!(
                            "clone PID {} did not exit within {:?}, sending SIGKILL",
                            pid, CLONE_SHUTDOWN_TIMEOUT
                        );
                        let _ = nix::sys::signal::kill(
                            nix::unistd::Pid::from_raw(pid as i32),
                            nix::sys::signal::Signal::SIGKILL,
                        );
                    }
                }
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // Give SIGKILL'd clones a moment to be reaped so their uffds are closed before the
        // server's handlers go away.
        let kill_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < kill_deadline {
            if clone_pids
                .iter()
                .all(|&pid| !crate::utils::is_process_alive(pid))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let stragglers: Vec<u32> = clone_pids
            .iter()
            .copied()
            .filter(|&pid| crate::utils::is_process_alive(pid))
            .collect();
        if !stragglers.is_empty() {
            warn!("clone PIDs {:?} still running after SIGKILL", stragglers);
        }
    }

    // All clones are gone — now it is safe to stop the UFFD server.
    if !server_exited {
        server_cancel.cancel();
        match tokio::time::timeout(Duration::from_secs(10), &mut server_handle).await {
            Ok(Ok(Ok(()))) => info!("UFFD server stopped"),
            Ok(Ok(Err(e))) => warn!("UFFD server exited with error: {}", e),
            Ok(Err(e)) => warn!("UFFD server task join error: {}", e),
            Err(_) => warn!("UFFD server did not stop within 10s, continuing cleanup"),
        }
    }

    // Clean up socket file
    if let Err(e) = std::fs::remove_file(&socket_path) {
        warn!(
            "failed to remove socket file {}: {}",
            socket_path.display(),
            e
        );
    } else {
        info!("removed socket file: {}", socket_path.display());
    }

    // Delete serve state
    if let Err(e) = state_manager.delete_state(&serve_id).await {
        warn!("failed to delete serve state {}: {}", serve_id, e);
    } else {
        info!("deleted serve state");
    }

    println!("Memory server stopped");

    Ok(())
}

/// Rebuild `HOST:GUEST[:ro]` volume specs and the portable flag from snapshot volume metadata.
///
/// Clones never go through the `podman run` path that populates `config.volumes` and
/// `config.portable_volumes`, but `snapshot create` builds a snapshot's volume metadata from
/// exactly those fields. Reconstructing them here ensures a snapshot taken from a clone
/// preserves the baseline's volume configuration.
fn volume_state_from_snapshot(
    volumes: &[crate::storage::snapshot::SnapshotVolumeConfig],
) -> (Vec<String>, bool) {
    let specs = volumes
        .iter()
        .map(|vol| {
            if vol.read_only {
                format!("{}:{}:ro", vol.host_path.display(), vol.guest_path)
            } else {
                format!("{}:{}", vol.host_path.display(), vol.guest_path)
            }
        })
        .collect();
    let portable = volumes.iter().any(|vol| vol.portable);
    (specs, portable)
}

/// Run clone from snapshot
///
/// Two modes:
/// - `--pid <serve_pid>`: Clone via the UFFD serve process (lazy on-demand paging,
///   for multiple concurrent clones)
/// - `--snapshot <name>`: Clone directly from snapshot files (simpler, no serve process needed)
///
/// This is public so podman.rs can call it directly for cache hits.
pub async fn cmd_snapshot_run(args: SnapshotRunArgs) -> Result<()> {
    // Determine mode and get snapshot name
    let (snapshot_name, serve_pid, use_uffd) = match (&args.pid, &args.snapshot) {
        (Some(pid), None) => {
            // UFFD mode: verify serve process is alive
            if !crate::utils::is_process_alive(*pid) {
                anyhow::bail!(
                    "serve process (PID {}) is not running - start with 'fcvm snapshot serve'",
                    pid
                );
            }

            // Load serve state by PID to get snapshot name
            let state_manager = StateManager::new(paths::state_dir());
            let serve_state = state_manager
                .load_state_by_pid(*pid)
                .await
                .context("loading serve process state - is serve running?")?;

            let name = serve_state
                .config
                .snapshot_name
                .ok_or_else(|| anyhow::anyhow!("serve process has no snapshot_name"))?;

            info!("Cloning VM from serve PID {} (snapshot: {})", pid, name);
            (name, Some(*pid), true)
        }
        (None, Some(name)) => {
            // Direct file mode: no serve process needed
            info!("Cloning VM directly from snapshot: {}", name);
            (name.clone(), None, false)
        }
        (None, None) => {
            anyhow::bail!("Either --pid or --snapshot must be specified");
        }
        (Some(_), Some(_)) => {
            // clap's conflicts_with should prevent this, but just in case
            anyhow::bail!("Cannot specify both --pid and --snapshot");
        }
    };

    let state_manager = StateManager::new(paths::state_dir());

    // Hold the per-snapshot lock SHARED from reading config.json until the restore has
    // opened memory.bin/vmstate.bin and reflinked disk.raw (released after
    // restore_from_snapshot below). Snapshot creators take the same lock exclusively
    // while atomically replacing the directory, so a concurrent re-create of this tag
    // can never swap generations under us mid-restore (mixed disk/memory) or remove
    // the directory between our reads.
    tokio::fs::create_dir_all(paths::snapshot_dir())
        .await
        .context("creating snapshot directory")?;
    let snapshot_shared_lock = super::common::acquire_snapshot_dir_lock(
        &paths::snapshot_dir().join(&snapshot_name),
        false,
    )
    .await?;

    // Load snapshot configuration
    let snapshot_manager = SnapshotManager::new(paths::snapshot_dir());
    let snapshot_config = snapshot_manager
        .load_snapshot(&snapshot_name)
        .await
        .context("loading snapshot configuration")?;
    // The --pid (UFFD) and --snapshot (direct memory restore) paths both resume
    // from a memory image; a disk-only tag has none — it cold-boots a fresh VM
    // from the captured disk instead. Dispatch to that path before any
    // memory-restore setup runs (and hand off the dir lock so the reflink is
    // protected against a concurrent re-create of the tag).
    if snapshot_config.kind == crate::storage::SnapshotKind::DiskOnly {
        return cmd_snapshot_run_disk_only(
            snapshot_name,
            snapshot_config,
            args,
            snapshot_shared_lock,
        )
        .await;
    }

    info!(
        snapshot = %snapshot_name,
        image = %snapshot_config.metadata.image,
        vcpu = snapshot_config.metadata.vcpu,
        mem_mib = snapshot_config.metadata.memory_mib,
        "loaded snapshot configuration"
    );

    // Generate VM ID and name. Restored VMs always get a FRESH vm_id and a
    // fresh state file (vsock_epoch starts at 0) — including the podman-run
    // snapshot-miss path, which tears its throwaway VM down (deleting its
    // state) and relaunches through here. No pre-restore exec session can
    // reference this state file, so restore needs no vsock-epoch bump.
    let vm_id = generate_vm_id();
    let runtime_config =
        snapshot_restore_runtime_config(&args, snapshot_config.metadata.kernel_profile.as_deref())
            .await;
    let vm_name = args.name.unwrap_or_else(|| {
        // Auto-generate: snapshot-name + random suffix
        format!("{}-{}", snapshot_name, &vm_id[..6])
    });

    // Validate VM name (whether user-provided or auto-generated)
    validate_vm_name(&vm_name).context("invalid VM name")?;

    state_manager.init().await?;

    let mut vm_state = VmState::new(
        vm_id.clone(),
        snapshot_config.metadata.image.clone(),
        args.cpu.unwrap_or(snapshot_config.metadata.vcpu),
        args.mem.unwrap_or(snapshot_config.metadata.memory_mib),
    );
    vm_state.name = Some(vm_name.clone());

    // Save snapshot tracking info in clone state
    vm_state.config.snapshot_name = Some(snapshot_name.clone());
    vm_state.config.process_type = Some(crate::state::ProcessType::Clone);
    vm_state.config.serve_pid = serve_pid; // Track which serve spawned us (None for direct mode)

    // Carry the snapshot's volume metadata into the clone's state so a `snapshot create`
    // taken from this clone records the same volumes as the baseline.
    let (volume_specs, portable_volumes) =
        volume_state_from_snapshot(&snapshot_config.metadata.volumes);
    vm_state.config.volumes = volume_specs;
    vm_state.config.portable_volumes = portable_volumes;
    // Same for the boot-plan fields: a snapshot taken FROM this clone must record
    // the original kernel profile / image device, or grand-clones lose them.
    vm_state.config.kernel_profile = snapshot_config.metadata.kernel_profile.clone();
    vm_state.config.image_mode = snapshot_config.metadata.image_mode.clone();
    vm_state.config.image_disk_path = snapshot_config.metadata.image_disk_path.clone();
    // The clone runs the same VMM that created the snapshot (the memory image format is
    // VMM-specific). Recorded so `fcvm ls` and any later snapshot of the clone are correct.
    vm_state.config.hypervisor = snapshot_config.metadata.hypervisor;

    // Setup paths
    let data_dir = paths::vm_runtime_dir(&vm_id);
    tokio::fs::create_dir_all(&data_dir)
        .await
        .context("creating VM data directory")?;

    let socket_path = data_dir.join("firecracker.sock");

    // Build UFFD socket path for memory server (only for UFFD mode)
    let uffd_socket = if use_uffd {
        let pid = serve_pid.expect("serve_pid must be set for UFFD mode");
        let socket = paths::data_dir().join(format!("uffd-{}-{}.sock", snapshot_name, pid));
        info!(
            uffd_socket = %socket.display(),
            serve_pid = pid,
            "connecting to memory server"
        );
        Some(socket)
    } else {
        info!(
            memory_file = %snapshot_config.memory_path.display(),
            "loading memory directly from file"
        );
        None
    };

    // Setup VolumeServers for clones if snapshot has volumes
    //
    // Mount namespace isolation for vsock:
    // - Firecracker's vmstate.bin stores the baseline's vsock uds_path
    // - Multiple clones from the same snapshot would all try to bind() to the same path
    // - This causes "Address in use" errors for all but the first clone
    //
    // Solution: Each clone's Firecracker runs in a mount namespace where the baseline's
    // runtime directory is bind-mounted over the clone's runtime directory.
    // - Firecracker thinks it's binding to /baseline_dir/vsock.sock
    // - But the bind mount redirects this to /clone_dir/vsock.sock
    // - Each clone has its own mount namespace, so each creates unique socket files
    // - VolumeServers listen on the clone's actual socket paths
    // Clone's vsock socket base path
    // With mount namespace isolation, Firecracker will create sockets here
    // (it thinks it's writing to baseline's path but bind mount redirects to clone's)
    let clone_vsock_base = data_dir.join("vsock.sock");

    // Build VolumeConfigs from snapshot metadata and spawn VolumeServers
    let volume_configs: Vec<VolumeConfig> = snapshot_config
        .metadata
        .volumes
        .iter()
        .map(|vol| VolumeConfig {
            host_path: vol.host_path.clone(),
            guest_path: vol.guest_path.clone().into(),
            read_only: vol.read_only,
            port: vol.vsock_port,
            portable: vol.portable,
        })
        .collect();

    // Load serialized inode tables from snapshot (if available) for portable volumes.
    // This restores the RemapFs inode mappings so clones see the same inodes as the baseline,
    // avoiding the 1s TTL glitch window where old inodes return EIO.
    let snap_dir = snapshot_config
        .memory_path
        .parent()
        .expect("snapshot memory_path must have parent");
    let mut inode_tables: Vec<Option<String>> = Vec::with_capacity(volume_configs.len());
    for vol in &snapshot_config.metadata.volumes {
        if vol.portable {
            let table_path = snap_dir.join(format!("volume-{}-inode-table.json", vol.vsock_port));
            let table = tokio::fs::read_to_string(&table_path).await.ok();
            if table.is_some() {
                info!(port = vol.vsock_port, "loaded inode table from snapshot");
            }
            inode_tables.push(table);
        } else {
            inode_tables.push(None);
        }
    }

    let volume_servers = crate::volume::spawn_volume_servers_with_tables(
        &volume_configs,
        &clone_vsock_base,
        &inode_tables,
    )
    .await
    .context("spawning VolumeServers for clone")?;

    // Setup TTY/output socket paths (inherited from snapshot metadata)
    let tty_mode = snapshot_config.metadata.tty;
    let interactive = snapshot_config.metadata.interactive;
    let non_blocking_output = args.non_blocking_output;
    let tty_socket_path = format!("{}_{}", clone_vsock_base.display(), VSOCK_TTY_PORT);
    let output_socket_path = format!("{}_{}", clone_vsock_base.display(), VSOCK_OUTPUT_PORT);

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

    // For non-TTY mode, use async output listener.
    // The reconnect_notify is fired after restore_from_snapshot() to signal the output
    // listener to drop its dead vsock stream and re-accept. Without this, the listener
    // stays stuck reading from the old (dead) connection after VM resume resets vsock.
    let output_reconnect = Arc::new(tokio::sync::Notify::new());
    // Channel to know when fc-agent's output connection arrives (gates health monitor)
    let (output_connected_tx, mut output_connected_rx) = tokio::sync::oneshot::channel();
    let output_handle = if !tty_mode {
        let socket_path = output_socket_path.clone();
        let vm_id_clone = vm_id.clone();
        let reconnect = output_reconnect.clone();
        Some(tokio::spawn(async move {
            match run_output_listener(
                &socket_path,
                &vm_id_clone,
                None,
                reconnect,
                non_blocking_output,
                Some(output_connected_tx),
            )
            .await
            {
                Ok(lines) => lines,
                Err(e) => {
                    tracing::warn!("Output listener error: {}", e);
                    Vec::new()
                }
            }
        }))
    } else {
        None
    };

    // Network mode inherited from snapshot metadata
    let network_mode = snapshot_config.metadata.network_mode;

    // Start egress proxy for rootless mode only
    let _egress_proxy_handle = if matches!(network_mode, FcNetworkMode::Rootless) {
        let socket_path = clone_vsock_base.clone();
        Some(tokio::spawn(async move {
            if let Err(e) = crate::network::egress_proxy::run_egress_proxy(&socket_path).await {
                tracing::warn!("Egress proxy error: {}", e);
            }
        }))
    } else {
        None
    };

    // Setup networking - use saved network config from snapshot
    let tap_device = format!("tap-{}", truncate_id(&vm_id, 8));
    let port_mappings = snapshot_config.metadata.port_mappings.clone();

    // Extract guest_ip from snapshot metadata for network config reuse
    let saved_network = &snapshot_config.metadata.network_config;

    // Bridged/routed mode requires root for iptables and network namespace setup
    if matches!(network_mode, FcNetworkMode::Bridged | FcNetworkMode::Routed)
        && !nix::unistd::geteuid().is_root()
    {
        bail!(
            "Bridged/routed networking requires root. Either:\n  \
             - Run with sudo: sudo fcvm snapshot run ...\n  \
             - Use rootless mode (create baseline with --network rootless)"
        );
    }
    // Rootless with sudo is pointless - bridged would be faster
    if matches!(network_mode, FcNetworkMode::Rootless) && nix::unistd::geteuid().is_root() {
        warn!(
            "Running rootless mode as root is unnecessary. \
             Consider creating the baseline with --network bridged or --network routed for better performance."
        );
    }

    // Setup networking based on mode - reuse guest_ip from snapshot if available
    let mut network: Box<dyn NetworkManager> = match network_mode {
        FcNetworkMode::Bridged => {
            let mut net =
                BridgedNetwork::new(vm_id.clone(), tap_device.clone(), port_mappings.clone());
            // If snapshot has saved network config with guest_ip, use it
            if let Some(ref guest_ip) = saved_network.guest_ip {
                net = net.with_guest_ip(guest_ip.clone());
                info!(
                    guest_ip = %guest_ip,
                    "clone will use same network config as snapshot"
                );
            }
            Box::new(net)
        }
        FcNetworkMode::Routed => {
            let mut net =
                RoutedNetwork::new(vm_id.clone(), tap_device.clone(), port_mappings.clone());
            if let Some(ref prefix) = snapshot_config.metadata.ipv6_prefix {
                net = net.with_ipv6_prefix(prefix.clone());
            }
            if !snapshot_config.metadata.forward_localhost.is_empty() {
                net =
                    net.with_forward_localhost(snapshot_config.metadata.forward_localhost.clone());
            }
            net.preflight_check()
                .context("routed mode preflight check failed")?;
            if !port_mappings.is_empty() {
                let loopback_ip = state_manager
                    .allocate_loopback_ip(&mut vm_state)
                    .await
                    .context("allocating loopback IP for routed clone")?;
                net = net.with_loopback_ip(loopback_ip);
            }
            Box::new(net)
        }
        FcNetworkMode::Rootless => {
            // For rootless mode, allocate loopback IP atomically with state persistence
            // This prevents race conditions when starting multiple clones concurrently
            let loopback_ip = state_manager
                .allocate_loopback_ip(&mut vm_state)
                .await
                .context("allocating loopback IP")?;

            // With bridge mode, guest IP is always 10.0.2.100 on pasta network
            // Each clone runs in its own namespace, so no IP conflict
            let net = PastaNetwork::new(vm_id.clone(), tap_device.clone(), port_mappings.clone())
                .with_loopback_ip(loopback_ip)
                .with_restore_mode();
            Box::new(net)
        }
    };

    // network.setup() may fail partway through (it tears nothing down itself),
    // so any error from here until the restore_from_snapshot error handler below
    // must run network.cleanup() to remove partially-created host network state.
    let network_config = match network.setup().await.context("setting up network") {
        Ok(config) => config,
        Err(e) => {
            if let Err(cleanup_err) = network.cleanup().await {
                warn!(
                    "failed to cleanup network after setup error: {}",
                    cleanup_err
                );
            }
            return Err(e);
        }
    };

    // For routed mode clones: the snapshot's guest IPv6 (baked into boot params) is shared
    // across all clones. After restore, fc-agent will be told to swap it to the unique
    // per-clone vm_ipv6. Store the new vm_ipv6 so the exec reconfigure command can use it.
    let clone_ipv6_swap: Option<(String, String)> = if let Some(routed_net) =
        network
            .as_any()
            .downcast_ref::<crate::network::RoutedNetwork>()
    {
        match (&saved_network.guest_ipv6, routed_net.vm_ipv6()) {
            (Some(old), Some(new)) if old != new => {
                info!(
                    old_ipv6 = %old,
                    new_ipv6 = %new,
                    "will reconfigure guest IPv6 after restore"
                );
                Some((old.clone(), new.to_string()))
            }
            _ => None,
        }
    } else {
        None
    };

    // Health check URL comes from snapshot metadata — it's a property of the VM image.
    // The cache key includes health_check_url, so each config gets its own snapshot.
    vm_state.config.health_check_url = snapshot_config.metadata.health_check_url.clone();
    vm_state.config.health_check_timeout = snapshot_config.metadata.health_check_timeout;
    vm_state.config.hugepages = args.hugepages.unwrap_or(snapshot_config.metadata.hugepages);
    // Restore username for rootless health checks (runuser -u <username>).
    vm_state.config.username = snapshot_config.metadata.username.clone();
    vm_state.config.user = snapshot_config.metadata.user.clone();
    vm_state.config.port_mappings = port_mappings;
    vm_state.config.forward_localhost = snapshot_config.metadata.forward_localhost.clone();
    vm_state.config.network_mode = network_mode;
    vm_state.config.ipv6_prefix = snapshot_config.metadata.ipv6_prefix.clone();
    vm_state.config.tty = tty_mode;
    vm_state.config.interactive = interactive;

    // NFS shares recorded with the snapshot: re-export them for this VM. The
    // baseline's /etc/exports.d entry died with the baseline, and fc-agent
    // remounts the shares in the guest right after the restore signal — the
    // export must be active before that. Bridged clones reach the host through
    // the in-namespace gateway DNAT and arrive masqueraded as their veth IP
    // (with a possibly non-privileged port → insecure); other modes connect
    // with the guest IP like a baseline VM.
    vm_state.config.nfs_shares = snapshot_config.metadata.nfs_shares.clone();
    if !vm_state.config.nfs_shares.is_empty() {
        let bridged_veth_ip = network
            .as_any()
            .downcast_ref::<BridgedNetwork>()
            .and_then(|net| net.veth_inner_ip().map(str::to_string));
        let (client_spec, insecure) = match (bridged_veth_ip, network_config.guest_ip.clone()) {
            (Some(ip), _) => (ip, true),
            (None, Some(ip)) => (ip, false),
            (None, None) => {
                // A malformed exports entry would fail exportfs cryptically and
                // hang the guest's hard mount — fail fast and clean like the
                // other pre-restore error paths.
                if let Err(cleanup_err) = network.cleanup().await {
                    warn!(
                        "failed to cleanup network after NFS client error: {}",
                        cleanup_err
                    );
                }
                anyhow::bail!("no client IP available for NFS exports of restored VM");
            }
        };
        if let Err(e) = crate::commands::podman::setup_nfs_exports(
            &vm_id,
            &vm_state.config.nfs_shares,
            &client_spec,
            insecure,
        )
        .await
        .context("re-creating NFS exports for restored VM")
        {
            if let Err(cleanup_err) = network.cleanup().await {
                warn!(
                    "failed to cleanup network after NFS export error: {}",
                    cleanup_err
                );
            }
            return Err(e);
        }
    }

    info!(
        tap = %network_config.tap_device,
        mac = %network_config.guest_mac,
        "network configured for clone"
    );

    // Build restore configuration
    // For snapshots of cache-restored VMs:
    // - original_vsock_vm_id (vm-AAA) = vsock paths in vmstate.bin (unchanged from cache)
    // - vm_id (vm-BBB) = disk paths in vmstate.bin (patched during cache restore)
    // For snapshots of fresh VMs:
    // - vm_id is used for both (no separate original_vsock_vm_id)
    let original_vm_id = snapshot_config
        .original_vsock_vm_id
        .clone()
        .unwrap_or_else(|| snapshot_config.vm_id.clone());

    // snapshot_vm_id is the VM ID where disk paths point (snapshot_config.vm_id)
    // Only set if different from original_vm_id (for cache-restored VMs)
    let snapshot_vm_id = if snapshot_config.original_vsock_vm_id.is_some() {
        // Snapshot of cache-restored VM: disk paths point to snapshot's vm_id
        Some(snapshot_config.vm_id.clone())
    } else {
        // Snapshot of fresh VM: disk and vsock both use same vm_id
        None
    };

    // Choose memory backend based on mode
    // Hugepages require UFFD restore (Firecracker rejects File backend for hugepage snapshots).
    // When restoring from cache (no explicit serve process), start an implicit in-process
    // UFFD server as a background tokio task.
    let hugepages = args.hugepages.unwrap_or(snapshot_config.metadata.hugepages);
    let implicit_uffd_cancel = tokio_util::sync::CancellationToken::new();

    // Which VMM created this snapshot — restore must use the same backend (the memory image
    // format is VMM-specific). Cloud Hypervisor restores from its own `ch/` subdir via
    // `--restore`, so the Firecracker MemoryBackend below is unused for it.
    let is_ch = snapshot_config.metadata.hypervisor == crate::hypervisor::Backend::CloudHypervisor;

    let memory_backend = if is_ch {
        // Unused by the CH restore path (it reads ch/memory-ranges via --restore); a
        // placeholder so the shared RestoreParams shape is satisfied.
        MemoryBackend::File {
            memory_path: snapshot_config.memory_path.clone(),
        }
    } else if let Some(ref uffd_socket_path) = uffd_socket {
        // Explicit UFFD mode (--pid): connect to existing serve process
        MemoryBackend::Uffd {
            socket_path: uffd_socket_path.clone(),
        }
    } else {
        // Use file-backed restore by default, UFFD when required.
        // Hugepages require UFFD (Firecracker rejects File backend for hugepage snapshots).
        // FCVM_FORCE_UFFD=1 forces UFFD for debugging/testing.
        if hugepages || std::env::var("FCVM_FORCE_UFFD").is_ok() {
            let implicit_socket_path = data_dir.join("uffd.sock");
            let reason = if hugepages {
                "hugepages require UFFD"
            } else {
                "FCVM_FORCE_UFFD"
            };
            info!(
                socket = %implicit_socket_path.display(),
                reason = %reason,
                "starting implicit UFFD server for snapshot restore"
            );

            let server = match UffdServer::new_with_path(
                format!("implicit-{}", truncate_id(&vm_id, 8)),
                &snapshot_config.memory_path,
                &implicit_socket_path,
            )
            .await
            .context("creating implicit UFFD server")
            {
                Ok(server) => server,
                Err(e) => {
                    if let Err(cleanup_err) = network.cleanup().await {
                        warn!(
                            "failed to cleanup network after setup error: {}",
                            cleanup_err
                        );
                    }
                    crate::commands::podman::cleanup_nfs_exports(&vm_id).await;
                    return Err(e);
                }
            };

            let cancel = implicit_uffd_cancel.clone();
            tokio::spawn(async move {
                if let Err(e) = server.run(cancel).await {
                    tracing::error!(target: "uffd", error = ?e, "implicit UFFD server error");
                }
            });

            for i in 0..100 {
                if implicit_socket_path.exists() {
                    break;
                }
                if i == 99 {
                    if let Err(cleanup_err) = network.cleanup().await {
                        warn!(
                            "failed to cleanup network after setup error: {}",
                            cleanup_err
                        );
                    }
                    crate::commands::podman::cleanup_nfs_exports(&vm_id).await;
                    bail!("implicit UFFD server did not bind socket within 5s");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            MemoryBackend::Uffd {
                socket_path: implicit_socket_path,
            }
        } else {
            MemoryBackend::File {
                memory_path: snapshot_config.memory_path.clone(),
            }
        }
    };

    let snapshot_dir = paths::snapshot_dir().join(&snapshot_name);
    let restore_config = SnapshotRestoreConfig {
        vmstate_path: snapshot_config.vmstate_path.clone(),
        memory_backend,
        source_disk_path: snapshot_config.disk_path.clone(),
        original_vm_id,
        snapshot_vm_id,
        hugepages,
        extra_disks: snapshot_config.metadata.extra_disks.clone(),
        snapshot_dir: Some(snapshot_dir),
    };

    // Run clone setup using shared restore function
    // Dirty tracking: KVM CoW-copies file-backed pages so it can track which
    // pages are modified (needed for diff snapshots from this VM).
    // Without it, pages stay shared through the host page cache — multiple
    // clones from the same snapshot share physical memory.
    // CLI: --no-dirty-tracking disables it for clones.
    // Internal: startup_snapshot_base_key forces it on (needs diff snapshot).
    // Hugepages: always disable — KVM splits 2MB Stage 2 block mappings to 4K
    // for dirty tracking, negating the TLB benefit of hugepages.
    let needs_dirty_tracking = if hugepages {
        false // hugepage VMs must not split 2MB TLB entries
    } else if args.startup_snapshot_base_key.is_some() {
        true // podman path — needs dirty tracking for startup snapshot
    } else {
        !args.no_dirty_tracking // CLI default: on. --no-dirty-tracking: off.
    };
    // Reboot-in-place support: listen for the guest's reboot signal on the status
    // port (also records the container ready/exit notifications the restore path
    // previously dropped). Spawned BEFORE the restore resumes the VM — fc-agent's
    // status messages retry only briefly, so the socket must already exist when the
    // guest starts running (same ordering as the podman path).
    let reboot_requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let container_exit_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let status_socket_path = format!("{}_{}", clone_vsock_base.display(), VSOCK_STATUS_PORT);
    let status_handle = {
        let socket_path = status_socket_path.clone();
        let runtime_dir = data_dir.clone();
        let vm_id_clone = vm_id.clone();
        let reboot_flag = reboot_requested.clone();
        let exit_flag = container_exit_seen.clone();
        tokio::spawn(async move {
            if let Err(e) = run_status_listener(
                &socket_path,
                &runtime_dir,
                &vm_id_clone,
                None,
                reboot_flag,
                exit_flag,
            )
            .await
            {
                warn!("Status listener error: {}", e);
            }
        })
    };

    let restore_params = RestoreParams {
        vm_id: &vm_id,
        vm_name: &vm_name,
        data_dir: &data_dir,
        socket_path: &socket_path,
        runtime_config: &runtime_config,
        restore_config: &restore_config,
        network_config: &network_config,
        clone_ipv6: clone_ipv6_swap.as_ref().map(|(_, new)| new.clone()),
        track_dirty_pages: needs_dirty_tracking,
    };
    // Restore via the backend that created the snapshot. Both are boxed as `dyn Hypervisor`
    // so the downstream health/exit/cleanup handling is backend-agnostic.
    // failpoint: hold right before the restore resumes/unblocks the VM (the resume
    // happens inside restore_from_snapshot{,_ch} below) — clone infra (state file,
    // status listener, network) is already live, so "client arrives before the
    // restored guest ever runs" becomes deterministic.
    failpoint::hit_async("restore.pre_resume").await;
    let setup_result: Result<(
        Box<dyn crate::hypervisor::Hypervisor>,
        Option<tokio::process::Child>,
    )> = if is_ch {
        async {
            // Serve the restore-epoch over the boot-plan vsock port BEFORE the restore
            // resumes the VM, so the restored guest's watcher can run handle_clone_restore
            // (reconnect output/exec vsock + clock sync) as soon as it resumes. The
            // listener task runs detached for the clone's (process) lifetime.
            let restore_epoch = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let mut latest = serde_json::json!({
                "host-time": chrono::Utc::now().timestamp().to_string(),
                "restore-epoch": restore_epoch.to_string(),
            });
            if let Some((_, ref new_ipv6)) = clone_ipv6_swap {
                latest["clone-ipv6"] = serde_json::Value::String(new_ipv6.clone());
            }
            let bootplan_socket = format!(
                "{}_{}",
                clone_vsock_base.display(),
                super::common::VSOCK_BOOTPLAN_PORT
            );
            super::podman::spawn_bootplan_listener(&bootplan_socket, &latest)
                .context("spawning CH restore boot-plan listener")?;
            let (backend, holder) = super::common::restore_from_snapshot_ch(
                restore_params,
                network.as_mut(),
                &state_manager,
                &mut vm_state,
            )
            .await?;
            Ok((
                Box::new(backend) as Box<dyn crate::hypervisor::Hypervisor>,
                holder,
            ))
        }
        .await
    } else {
        super::common::restore_from_snapshot(
            restore_params,
            network.as_mut(),
            &state_manager,
            &mut vm_state,
        )
        .await
        .map(|(b, h)| (Box::new(b) as Box<dyn crate::hypervisor::Hypervisor>, h))
    };

    // The restore has opened/reflinked everything it needs from the snapshot directory;
    // release the shared per-snapshot lock so creators are not blocked for the lifetime
    // of this clone.
    drop(snapshot_shared_lock);

    // If setup failed, cleanup all resources before propagating error
    if let Err(e) = setup_result {
        warn!("Clone setup failed, cleaning up resources");

        // Stop implicit UFFD server if running
        implicit_uffd_cancel.cancel();

        // Abort VolumeServer tasks
        for handle in volume_servers.handles {
            handle.abort();
        }

        // Cleanup network
        if let Err(cleanup_err) = network.cleanup().await {
            warn!(
                "failed to cleanup network after setup error: {}",
                cleanup_err
            );
        }

        // Remove the NFS exports created for this VM (no-op without NFS)
        crate::commands::podman::cleanup_nfs_exports(&vm_id).await;

        // Cleanup data directory
        if data_dir.exists() {
            if let Err(cleanup_err) = tokio::fs::remove_dir_all(&data_dir).await {
                warn!(
                    "failed to cleanup data_dir after setup error: {}",
                    cleanup_err
                );
            }
        }

        // Cleanup state file
        if let Err(cleanup_err) = state_manager.delete_state(&vm_id).await {
            warn!("failed to cleanup state after setup error: {}", cleanup_err);
        }

        return Err(e);
    }

    let (mut vm_manager, mut holder_child) = setup_result.unwrap();

    // Build the cold-boot relaunch plan up front (consumed if the guest reboots).
    let clone_disk_path = data_dir.join("disks/rootfs.raw");
    // Reboot-in-place relaunches via the Firecracker cold-boot plan (build_clone_reboot_plan
    // is FC-specific). Cloud Hypervisor clones don't support it yet — a guest reboot
    // terminates the clone (same as TTY clones). Tracked for a future increment.
    let reboot_plan = if is_ch {
        None
    } else {
        match build_clone_reboot_plan(
            &snapshot_config.metadata,
            &vm_name,
            args.cpu.unwrap_or(snapshot_config.metadata.vcpu),
            args.mem.unwrap_or(snapshot_config.metadata.memory_mib),
            args.non_blocking_output,
            &network_config,
            &runtime_config,
            &clone_disk_path,
            &clone_vsock_base,
        )
        .await
        {
            Ok(plan) => Some(plan),
            Err(e) => {
                warn!(
                    error = %e,
                    "reboot-in-place unavailable for this clone — a guest reboot will terminate it"
                );
                None
            }
        }
    };

    // Disable swap for Firecracker if requested via --no-swap
    if args.no_swap {
        if let Ok(pid) = vm_manager.pid() {
            super::common::disable_cgroup_swap(pid);
        }
    }

    // For routed mode clones: fc-agent reconfigures eth0 with the new vm_ipv6 via MMDS.
    // The state already has the correct guest_ipv6 = vm_ipv6 (set by restore_from_snapshot).
    // Subsequent snapshots from this clone will record the vm_ipv6 that the guest actually uses.

    // fc-agent's handle_clone_restore() now drives the output reconnect sequence:
    // exec rebind → wait for confirmation → output.reconnect(). No host-side
    // notify needed — the listener will accept fc-agent's new connection naturally.

    let is_uffd = use_uffd || std::env::var("FCVM_FORCE_UFFD").is_ok() || hugepages;
    if is_uffd {
        info!(vm_id = %vm_id, vm_name = %vm_name, "VM cloned with UFFD memory");
        println!(
            "✓ VM '{}' cloned from snapshot '{}' (UFFD mode)",
            vm_name, snapshot_name
        );
        if use_uffd {
            println!("  Memory pages served on-demand by UFFD serve process");
        } else {
            println!("  Memory pages served on-demand from snapshot file");
        }
    } else {
        info!(vm_id = %vm_id, vm_name = %vm_name, "VM cloned from snapshot files");
        println!(
            "✓ VM '{}' cloned from snapshot '{}' (direct mode)",
            vm_name, snapshot_name
        );
        println!("  Memory loaded from file");
    }
    println!("  Disk uses CoW overlay");

    // Handle --exec: run command in container then cleanup and exit
    if let Some(exec_cmd) = &args.exec {
        info!("executing command in clone: {}", exec_cmd);

        // Run the exec steps inside an inner block so any failure (parse error, vsock not
        // ready, exec error) still reaches the cleanup below. Returning early here would
        // leak the network namespace, state file, loopback IP, and data directory that only
        // cleanup_vm removes.
        let exec_result: Result<i32> = async {
            // Parse command using shell_words (same as --cmd in podman run)
            let cmd_args: Vec<String> = shell_words::split(exec_cmd)
                .with_context(|| format!("parsing --exec argument: {}", exec_cmd))?;

            // Wait for vsock socket to be ready (poll instead of blind sleep)
            let vsock_socket = data_dir.join("vsock.sock");
            let poll_start = std::time::Instant::now();
            const MAX_VSOCK_WAIT: Duration = Duration::from_millis(5000);
            const VSOCK_POLL_INTERVAL: Duration = Duration::from_millis(10);

            loop {
                if poll_start.elapsed() > MAX_VSOCK_WAIT {
                    bail!("vsock socket not ready after {:?}", poll_start.elapsed());
                }

                // Check if socket exists and is connectable
                if vsock_socket.exists() {
                    if let Ok(_stream) = std::os::unix::net::UnixStream::connect(&vsock_socket) {
                        debug!("vsock socket ready after {:?}", poll_start.elapsed());
                        break;
                    }
                }

                tokio::time::sleep(VSOCK_POLL_INTERVAL).await;
            }
            crate::commands::exec::run_exec_in_vm(
                &vsock_socket,
                &cmd_args,
                true, // in_container
                &vm_id,
            )
            .await
        }
        .await;

        // Cleanup resources (exec path has no health monitor)
        info!(result = ?exec_result, "exec finished, cleaning up");

        // Stop implicit UFFD server if running (hugepage cache restore)
        implicit_uffd_cancel.cancel();
        status_handle.abort();

        super::common::cleanup_vm(
            super::common::CleanupContext {
                vm_id: vm_id.clone(),
                volume_server_handles: volume_servers.handles,
                remap_refs: volume_servers.remap_refs,
                data_dir: data_dir.clone(),
                health_cancel_token: None, // no health monitor in exec path
                health_monitor_handle: None,
                output_listener_handle: output_handle, // abort output listener task
            },
            vm_manager.as_mut(),
            &mut holder_child,
            network.as_mut(),
            &state_manager,
        )
        .await;

        // Propagate exec errors only after cleanup has run
        let exit_code = exec_result?;
        if exit_code != 0 {
            bail!("exec command exited with code {}", exit_code);
        }

        return Ok(());
    }

    // Create cancellation token for graceful health monitor shutdown
    let health_cancel_token = tokio_util::sync::CancellationToken::new();

    // Create startup snapshot channel if:
    // - startup_snapshot_base_key is set (passed from podman run on cache hit)
    // - snapshot has a health check URL (needed to know when VM is fully initialized)
    let (startup_tx, mut startup_rx): (
        Option<tokio::sync::oneshot::Sender<crate::health::StartupSnapshotAck>>,
        Option<tokio::sync::oneshot::Receiver<crate::health::StartupSnapshotAck>>,
    ) = if args.startup_snapshot_base_key.is_some()
        && snapshot_config.metadata.health_check_url.is_some()
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    // Wait for fc-agent output connection before starting health monitor.
    // This ensures the deterministic handshake chain is complete:
    //   exec_rebind → exec_re_register → rebind_done → output.reconnect() → HERE
    // Without this gate, the health monitor could start exec calls before
    // the exec server has re-registered its AsyncFd after restore.
    // No timeout — after snapshot restore, the VM may be CPU-starved (HHVM, EdenFS,
    // falcon all resume simultaneously) and fc-agent's MMDS poll + restore handler
    // can take minutes. Proceeding early causes exec failures; waiting is correct.
    // But poll VM liveness to avoid hanging forever if Firecracker crashes.
    if !tty_mode {
        let mut liveness_interval = tokio::time::interval(std::time::Duration::from_secs(5));
        liveness_interval.tick().await; // consume immediate first tick
        let mut output_connected = false;
        loop {
            tokio::select! {
                result = &mut output_connected_rx => {
                    match result {
                        Ok(()) => {
                            info!(vm_id = %vm_id, "fc-agent output connected, exec server ready");
                            output_connected = true;
                        }
                        Err(_) => warn!(vm_id = %vm_id, "output connected_tx dropped"),
                    }
                    break;
                }
                _ = liveness_interval.tick() => {
                    match vm_manager.try_wait() {
                        Ok(Some(status)) => {
                            warn!(vm_id = %vm_id, ?status, "VM exited before fc-agent connected");
                            break;
                        }
                        Ok(None) => {} // still running
                        Err(e) => {
                            warn!(vm_id = %vm_id, error = %e, "VM liveness check failed");
                            break;
                        }
                    }
                }
            }
        }
        // Dead-serial detection: the output vsock reconnecting proves the guest
        // and its virtio transport are alive, but the serial console can still
        // be dead if the snapshot was captured with UART TX bytes in flight —
        // the restored guest's 8250 driver then waits forever for a TX
        // interrupt the re-created serial device never delivers, and every log
        // line from the VM silently disappears. A healthy restored fc-agent
        // always prints restore-progress lines BEFORE reconnecting output, so
        // zero console lines shortly after this point is proof of a poisoned
        // snapshot. One loud error naming the snapshot instead of a trail of
        // mystery test failures. The mid-TX-UART diagnosis is Firecracker-only
        // (8250 on ttyS0); Cloud Hypervisor restores use the hvc0 virtio
        // console, so they get a backend-neutral missing-console error.
        if output_connected {
            let console_lines = vm_manager.console_line_counter();
            let backend = vm_manager.backend();
            let snapshot_name = snapshot_name.clone();
            let vm_id = vm_id.clone();
            // 30s, not a few seconds: a freshly restored VM can be CPU-starved for a
            // while (see the untimed output-connect wait above), and fc-agent's
            // console gate releases up to ~500ms after WarmStart. A poisoned
            // snapshot's serial is dead FOREVER, so a generous window costs nothing.
            // Cancel-aware so it can't fire after the VM is torn down.
            let watchdog_cancel = health_cancel_token.clone();
            tokio::spawn(async move {
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
                loop {
                    if console_lines.load(std::sync::atomic::Ordering::Relaxed) > 0 {
                        return;
                    }
                    if tokio::time::Instant::now() >= deadline {
                        break;
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {}
                        _ = watchdog_cancel.cancelled() => return,
                    }
                }
                match backend {
                    crate::hypervisor::Backend::Firecracker => tracing::error!(
                        vm_id = %vm_id,
                        snapshot = %snapshot_name,
                        "fc-agent's output vsock reconnected after restore but NO serial \
                         console line arrived within 30s — snapshot '{}' almost certainly \
                         captured the guest UART mid-transmit and every restore of it will \
                         have a dead serial console. Recreate the snapshot.",
                        snapshot_name
                    ),
                    crate::hypervisor::Backend::CloudHypervisor => tracing::error!(
                        vm_id = %vm_id,
                        snapshot = %snapshot_name,
                        "fc-agent's output vsock reconnected after restore but NO console \
                         output arrived within 30s — restores of snapshot '{}' come up with \
                         a dead guest console. Recreate the snapshot.",
                        snapshot_name
                    ),
                }
            });
        }
    }

    // Verify pasta's L2 forwarding path is ready before starting health monitor.
    // After snapshot restore, pasta may not have learned the guest's MAC yet.
    // This pings the guest to trigger ARP resolution, then probes each forwarded
    // port to confirm end-to-end forwarding works.
    //
    // On failure (the VM crashed during the wait above, or pasta's port probe timed out)
    // skip the monitor/wait section and fall through to the shared cleanup below before
    // propagating the error — returning early here would leak the network namespace,
    // state file, loopback IP, and data directory that only cleanup_vm removes.
    let verify_result = network
        .verify_port_forwarding()
        .await
        .context("port forwarding verification failed after snapshot restore");

    // Track container exit code (from TTY mode)
    let mut container_exit_code: Option<i32> = None;
    let mut health_monitor_handle = None;

    if verify_result.is_ok() {
        // Spawn health monitor task with startup snapshot trigger support
        health_monitor_handle = Some(crate::health::spawn_health_monitor_full(
            vm_id.clone(),
            vm_state.pid,
            paths::state_dir(),
            Some(health_cancel_token.clone()),
            startup_tx,
        ));

        // Setup signal handlers with cancellation token
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
            let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler");
            tokio::select! {
                _ = sigterm.recv() => { info!("received SIGTERM, shutting down VM"); }
                _ = sigint.recv() => { info!("received SIGINT, shutting down VM"); }
            }
            cancel_clone.cancel();
        });

        // Get disk path for startup snapshot creation
        let disk_path = data_dir.join("disks/rootfs.raw");

        // Wait for cancellation, VM exit, or startup snapshot trigger
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    container_exit_code = None;
                    break;
                }
                status = vm_manager.wait() => {
                    info!(status = ?status, "Firecracker child exited");

                    // Guest reboot? Relaunch in place as a cold boot from the
                    // provisioned disk (disk-only-clone semantics): same fcvm
                    // process, network, holder, listeners, and health monitor.
                    if super::podman::wait_for_reboot_decision(
                        &reboot_requested,
                        &status_handle,
                        &status_socket_path,
                    )
                    .await
                    {
                        reboot_requested.store(false, std::sync::atomic::Ordering::Release);
                        // Clear racing pre-reboot exit signal/files (fresh lifecycle).
                        container_exit_seen.store(false, std::sync::atomic::Ordering::Release);
                        let _ = std::fs::remove_file(data_dir.join("container-exit"));
                        let _ = std::fs::remove_file(data_dir.join("container-ready"));
                        if let Some((plan, synth_args, volume_mappings)) = reboot_plan.as_ref() {
                            info!("guest rebooted — relaunching restored clone in place (cold boot)");
                            let relaunch_result = async {
                                // The backend's VmManager still holds the restore-time
                                // namespace fields; a minimal spec reuses them.
                                let relaunch_spec = crate::hypervisor::ProcessSpec {
                                    binary: plan.firecracker_bin.clone(),
                                    extra_args: plan.fc_args.clone(),
                                    ..Default::default()
                                };
                                vm_manager
                                    .spawn(&relaunch_spec)
                                    .await
                                    .context("relaunching Firecracker after guest reboot")?;
                                super::podman::configure_and_boot_vm(
                                    vm_manager.as_mut(),
                                    plan,
                                    synth_args,
                                    &network_config,
                                    &mut vm_state,
                                    &data_dir,
                                    &vm_id,
                                    volume_mappings,
                                    None,
                                    plan.bootplan_over_vsock,
                                )
                                .await
                                .map(|_bootplan_handle| ())
                            }
                            .await;
                            match relaunch_result {
                                Ok(()) => {
                                    // The rebooted VM's memory is a fresh boot, not a
                                    // descendant of the restored snapshot — clear the
                                    // recorded parent so a later `snapshot create`
                                    // takes a full snapshot, never a bogus diff.
                                    vm_state.config.snapshot_name = None;
                                    // The rebooted clone no longer depends on (or
                                    // belongs to) its serve process — a serve
                                    // shutdown must not SIGTERM it.
                                    vm_state.config.serve_pid = None;
                                    let _ = state_manager
                                        .update_state(&vm_id, |state| {
                                            state.config.snapshot_name = None;
                                            state.config.serve_pid = None;
                                        })
                                        .await;
                                    startup_rx = None;
                                    continue;
                                }
                                Err(e) => {
                                    warn!(error = %e, "in-place relaunch after reboot failed; treating as VM exit");
                                }
                            }
                        } else {
                            warn!("guest rebooted but reboot-in-place is unavailable for this clone; treating as VM exit");
                        }
                    }

                    // If in TTY mode, get exit code from TTY handle
                    if let Some(handle) = tty_handle {
                        container_exit_code = handle.join().ok().and_then(|r| r.ok());
                        info!(container_exit_code = ?container_exit_code, "TTY container exit code");
                    } else {
                        container_exit_code = None;
                    }
                    break;
                }
                // Handle startup snapshot creation when health becomes healthy. The
                // health monitor defers publishing Healthy until `startup_ack` is sent
                // (or dropped by the abort paths below), so no client can observe
                // Healthy while the snapshot pause has the vCPUs stopped.
                Ok(startup_ack) = async {
                    match startup_rx.as_mut() {
                        Some(rx) => rx.await,
                        None => std::future::pending().await,
                    }
                } => {
                    // Oneshot channel - prevent further attempts
                    startup_rx = None;

                    if let Some(ref base_key) = args.startup_snapshot_base_key {
                        let startup_key = startup_snapshot_key(base_key);

                        // Skip if startup snapshot already exists. The startup-snapshot cache
                        // path is Firecracker-specific (diff snapshots via create_snapshot_core)
                        // and only runs for the podman-cache restore (FC); a Cloud Hypervisor
                        // clone forces no_snapshot, so the downcast never fails for it in
                        // practice — and if a non-FC backend ever reaches here, skip gracefully.
                        if check_podman_snapshot(&startup_key).await.is_some() {
                            info!(snapshot_key = %startup_key, "Startup snapshot already exists, skipping");
                        } else if let Some(fc_backend) = vm_manager
                            .as_any()
                            .downcast_ref::<crate::hypervisor::firecracker::FirecrackerBackend>()
                        {
                            info!(snapshot_key = %startup_key, "Creating startup snapshot (VM healthy)");

                            // Use select! so SIGTERM can abort startup snapshot immediately.
                            // Startup snapshots are optional (just caching), so if the VM is
                            // paused mid-snapshot, cleanup will kill it via vm_manager.kill().
                            // The diff parent is resolved inside create_podman_snapshot under
                            // the per-VM snapshot lock (re-read from the state file), so a
                            // concurrent `fcvm snapshot create` cannot leave us with a stale base.
                            let snap = CreateSnapshotParams {
                                vm_manager: fc_backend,
                                snapshot_key: &startup_key,
                                vm_state: &vm_state,
                                disk_path: &disk_path,
                                volume_configs: &volume_configs,
                                remap_refs: &volume_servers.remap_refs,
                            };
                            tokio::select! {
                                outcome = create_snapshot_interruptible(&snap, &cancel) => {
                                    match outcome {
                                        SnapshotOutcome::Interrupted => {
                                            container_exit_code = None;
                                            break;
                                        }
                                        SnapshotOutcome::Created => {
                                            info!(snapshot_key = %startup_key, "Startup snapshot created successfully");
                                            vm_state.config.snapshot_name = Some(startup_key.clone());
                                            // Locked read-modify-write: only update snapshot_name so the
                                            // health monitor's concurrent writes are not clobbered.
                                            let _ = state_manager
                                                .update_state(&vm_state.vm_id, |state| {
                                                    state.config.snapshot_name = Some(startup_key.clone());
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
                                    container_exit_code = None;
                                    break;
                                }
                            }
                        }
                    }
                    // Snapshot attempt over (created, skipped, or failed) and the VM is
                    // resumed — let the health monitor publish Healthy. The break/abort
                    // paths above drop `startup_ack`, which unblocks it the same way.
                    let _ = startup_ack.send(());
                    // Continue waiting for VM exit or signals
                }
            }
        }
    }

    // Stop implicit UFFD server if running
    implicit_uffd_cancel.cancel();
    // The status listener never exits on its own (no idle timeout) — abort it.
    status_handle.abort();

    // Cleanup common resources
    super::common::cleanup_vm(
        super::common::CleanupContext {
            vm_id: vm_id.clone(),
            volume_server_handles: volume_servers.handles,
            remap_refs: volume_servers.remap_refs,
            data_dir: data_dir.clone(),
            health_cancel_token: Some(health_cancel_token),
            health_monitor_handle,
            output_listener_handle: output_handle, // abort output listener task
        },
        vm_manager.as_mut(),
        &mut holder_child,
        network.as_mut(),
        &state_manager,
    )
    .await;

    // Propagate post-restore verification failure only after cleanup has run
    verify_result?;

    // Return error if container exited with non-zero code
    if let Some(code) = container_exit_code {
        if code != 0 {
            std::process::exit(code);
        }
    }

    Ok(())
}

/// Build the up-front reboot plan for a restored clone: everything needed to
/// relaunch it in place as a cold boot from its current provisioned disk when the
/// guest reboots. Returns the plan plus the synthesized RunArgs / volume mappings
/// the shared configure-and-boot primitive consumes.
///
/// Errors when the clone's shape isn't relaunchable yet (extra disks need their
/// already-restored images re-attached, which the cold-boot path doesn't do).
#[allow(clippy::too_many_arguments)]
async fn build_clone_reboot_plan(
    meta: &crate::storage::SnapshotMetadata,
    vm_name: &str,
    cpu: u8,
    mem: u32,
    non_blocking_output: bool,
    network_config: &crate::network::NetworkConfig,
    runtime_config: &RuntimeConfig,
    disk_path: &std::path::Path,
    vsock_socket_path: &std::path::Path,
) -> Result<(
    super::podman::RebootSpec,
    RunArgs,
    Vec<super::podman::VolumeMapping>,
)> {
    if !meta.extra_disks.is_empty() {
        bail!(
            "clone has {} extra disk(s); reboot-in-place doesn't re-attach them yet",
            meta.extra_disks.len()
        );
    }
    if meta.tty {
        bail!("clone uses TTY mode; reboot-in-place doesn't re-create the TTY session yet");
    }

    let synth_args = run_args_from_snapshot_metadata(
        meta,
        vm_name.to_string(),
        cpu,
        mem,
        non_blocking_output,
        None,
    );

    // Same kernel/initrd the source booted with (no setup side effects). The
    // recorded profile matters: a btrfs-profile disk needs a btrfs-capable kernel.
    let kernel_profile = meta.kernel_profile.as_deref().unwrap_or("default");
    let kernel_path = crate::setup::ensure_kernel(kernel_profile, false, false)
        .await
        .context("resolving kernel for reboot plan")?;
    let initrd_path = crate::setup::ensure_fc_agent_initrd(false)
        .await
        .context("resolving fc-agent initrd for reboot plan")?;

    let firecracker_bin = crate::commands::common::find_firecracker(runtime_config)?;
    let fc_args_env = std::env::var("FCVM_FIRECRACKER_ARGS").ok();
    let fc_args = runtime_config.firecracker_args.clone().or(fc_args_env);

    let launch_config = super::podman::build_launch_config(
        &synth_args,
        disk_path,
        &kernel_path,
        &initrd_path,
        &None,
        runtime_config,
    );
    let boot_args =
        super::podman::build_runtime_boot_args(&synth_args, network_config, runtime_config);

    let volume_mappings = synth_args
        .map
        .iter()
        .map(|s| super::podman::VolumeMapping::parse(s))
        .collect::<Result<Vec<_>>>()
        .context("parsing volume mappings for reboot plan")?;

    let plan = super::podman::RebootSpec {
        firecracker_bin,
        fc_args,
        launch_config,
        boot_args,
        track_dirty_pages: false,
        // Re-attach the recorded image device (content-addressed cache file) so an
        // overlay/archive-mode container's image layers survive the reboot.
        image_disk_path: meta.image_disk_path.clone(),
        vsock_socket_path: vsock_socket_path.to_path_buf(),
        // Clones restore from Firecracker snapshots and use MMDS; CH clone/restore is P2.
        bootplan_over_vsock: false,
    };
    Ok((plan, synth_args, volume_mappings))
}

/// Synthesize the `RunArgs` equivalent of a snapshot's captured host-side config.
///
/// Both consumers boot a provisioned disk through the shared podman machinery:
///   * the disk-only cold-boot dispatcher (rootfs_override = captured disk)
///   * the restore path's up-front reboot plan (a rebooted restored clone
///     cold-boots from its current disk in place)
///
/// Container-internal config (command, env, privileged) lives in the captured
/// container, which fc-agent `podman start`s — so it doesn't flow through RunArgs.
fn run_args_from_snapshot_metadata(
    meta: &crate::storage::SnapshotMetadata,
    name: String,
    cpu: u8,
    mem: u32,
    non_blocking_output: bool,
    rootfs_override: Option<PathBuf>,
) -> RunArgs {
    use crate::cli::args::NetworkMode as CliNetworkMode;

    // Map the captured FcNetworkMode back to the CLI network mode RunArgs expects.
    let network = match meta.network_mode {
        FcNetworkMode::Bridged => CliNetworkMode::Bridged,
        FcNetworkMode::Rootless => CliNetworkMode::Rootless,
        FcNetworkMode::Routed => CliNetworkMode::Routed,
    };

    let publish: Vec<String> = meta
        .port_mappings
        .iter()
        .map(|pm| format!("{}:{}/{}", pm.host_port, pm.guest_port, pm.proto))
        .collect();
    let map: Vec<String> = meta
        .volumes
        .iter()
        .map(|v| {
            if v.read_only {
                format!("{}:{}:ro", v.host_path.display(), v.guest_path)
            } else {
                format!("{}:{}", v.host_path.display(), v.guest_path)
            }
        })
        .collect();
    // NFS shares re-enter through the normal fresh-boot flow (export + plan +
    // guest mount) — a cold-boot clone of an NFS VM keeps its shares.
    let nfs: Vec<String> = meta
        .nfs_shares
        .iter()
        .map(|s| {
            if s.read_only {
                format!("{}:{}:ro", s.host_path, s.mount_path)
            } else {
                format!("{}:{}", s.host_path, s.mount_path)
            }
        })
        .collect();

    RunArgs {
        name,
        cpu,
        mem,
        rootfs_size: "10G".to_string(),
        map,
        disk: vec![],
        disk_dir: vec![],
        nfs,
        // fc-agent derives the rootless username from env USER; without it a --user
        // clone would set up "fcvm-user" and diverge from the captured passwd entry.
        env: match (&meta.user, &meta.username) {
            (Some(_), Some(username)) => vec![format!("USER={username}")],
            _ => vec![],
        },
        cmd: None,
        publish,
        balloon: None,
        network,
        // Cold-boot the clone/reboot under the SAME backend that created the snapshot —
        // a CH disk-only/reboot clone must not be launched under Firecracker (and would
        // fail outright on a CH-only host). meta.hypervisor is recorded at snapshot create.
        hypervisor: meta.hypervisor.into(),
        health_check: meta.health_check_url.clone(),
        health_check_timeout: meta.health_check_timeout,
        privileged: false,
        interactive: meta.interactive,
        tty: meta.tty,
        strace_agent: false,
        setup: false,
        kernel: None,
        // Same kernel profile as the source: a btrfs/nested-profile disk needs a
        // kernel that can boot it. prepare_vm resolves the full profile (kernel,
        // custom Firecracker, boot args) from this.
        kernel_profile: meta.kernel_profile.clone(),
        vsock_dir: None,
        no_snapshot: true,
        user: meta.user.clone(),
        forward_localhost: meta.forward_localhost.clone(),
        hugepages: meta.hugepages,
        portable_volumes: meta.volumes.iter().any(|v| v.portable),
        // Recorded delivery mode so the MMDS plan tells fc-agent how to re-attach
        // the image device on a provisioned boot (overlay re-mounts its store).
        image_mode: meta.image_mode.as_deref().and_then(|m| match m {
            "overlay" => Some(crate::cli::ImageMode::Overlay),
            "btrfs" => Some(crate::cli::ImageMode::Btrfs),
            "archive" => Some(crate::cli::ImageMode::Archive),
            _ => None,
        }),
        rootfs_type: None,
        non_blocking_output,
        label: vec![],
        ipv6_prefix: meta.ipv6_prefix.clone(),
        image: meta.image.clone(),
        command_args: vec![],
        rootfs_override,
        image_disk_override: meta.image_disk_path.clone(),
    }
}

/// Cold-boot a clone from a disk-only snapshot.
///
/// A disk-only tag has no memory image, so there is nothing to resume — the
/// clone is a fresh VM whose rootfs is a reflink of the captured disk. The
/// captured disk carries the fc-agent provisioned marker, so the guest preserves
/// the captured storage + container and only regenerates its identity. This
/// reuses the exact `podman run` boot/loop/cleanup path via a synthesized
/// `RunArgs` with `rootfs_override` set to the captured disk.
///
/// `dir_lock` is the shared per-snapshot lock held by the caller; it is dropped
/// once `prepare_vm` has reflinked the disk out of the snapshot directory, so a
/// concurrent re-create of the tag can't swap the disk mid-reflink.
async fn cmd_snapshot_run_disk_only(
    snapshot_name: String,
    snapshot_config: crate::storage::SnapshotConfig,
    args: SnapshotRunArgs,
    dir_lock: std::fs::File,
) -> Result<()> {
    let meta = &snapshot_config.metadata;

    // Flags the cold-boot path doesn't implement yet: fail loud, never silently drop.
    if args.exec.is_some() {
        bail!("--exec is not supported for disk-only clones yet (cold boot has no one-shot exec mode)");
    }
    if args.no_swap {
        bail!("--no-swap is not supported for disk-only clones yet");
    }

    // Extra disks aren't reflinked/attached on the cold-boot path yet. Fail loud
    // rather than silently booting a clone missing its data disks.
    if !meta.extra_disks.is_empty() {
        bail!(
            "disk-only clone of '{}' has {} extra disk(s); extra disks are not yet \
             supported for cold-boot clones",
            snapshot_name,
            meta.extra_disks.len()
        );
    }

    let disk_path = paths::snapshot_dir().join(&snapshot_name).join("disk.raw");
    if !disk_path.exists() {
        bail!(
            "disk-only snapshot '{}' is missing disk.raw at {}",
            snapshot_name,
            disk_path.display()
        );
    }

    let vm_name = args
        .name
        .clone()
        .unwrap_or_else(|| format!("{}-clone", snapshot_name));
    validate_vm_name(&vm_name).context("invalid VM name")?;

    let run_args = run_args_from_snapshot_metadata(
        meta,
        vm_name,
        args.cpu.unwrap_or(meta.vcpu),
        args.mem.unwrap_or(meta.memory_mib),
        args.non_blocking_output,
        Some(disk_path),
    );

    info!(
        snapshot = %snapshot_name,
        image = %meta.image,
        network = ?run_args.network,
        "cold-booting disk-only clone"
    );

    // SIGTERM/SIGINT → cancellation, mirroring `fcvm podman run`.
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => info!("received SIGTERM, shutting down clone"),
            _ = sigint.recv() => info!("received SIGINT, shutting down clone"),
        }
        cancel_clone.cancel();
    });

    // Box::pin breaks the static async cycle: prepare_vm can call back into
    // cmd_snapshot_run (snapshot-cache restore), which dispatches here.
    let Some(mut ctx) = Box::pin(super::podman::prepare_vm(run_args)).await? else {
        return Ok(());
    };
    // The disk has been reflinked into the clone's own data dir; the snapshot
    // directory is no longer needed, so release the shared lock.
    drop(dir_lock);

    if cancel.is_cancelled() {
        info!("shutdown requested during clone setup, cleaning up");
        super::podman::cleanup_vm_context(ctx).await;
        bail!("interrupted by signal during clone setup");
    }

    let result = super::podman::run_vm_loop(&mut ctx, cancel).await;
    super::podman::cleanup_vm_context(ctx).await;

    if let Some(code) = result? {
        if code != 0 {
            bail!("clone container exited with code {}", code);
        }
    }
    Ok(())
}

/// List running snapshot servers
async fn cmd_snapshot_ls() -> Result<()> {
    let state_manager = StateManager::new(paths::state_dir());
    let all_vms = state_manager.list_vms().await?;

    // Filter to serve processes only
    let serves: Vec<_> = all_vms
        .iter()
        .filter(|vm| vm.config.process_type == Some(crate::state::ProcessType::Serve))
        .collect();

    if serves.is_empty() {
        println!("No snapshot servers running");
        return Ok(());
    }

    // Print header
    println!(
        "{:<12} {:<10} {:<12} {:<20} {:<8}",
        "SERVE_ID", "PID", "HEALTH", "SNAPSHOT", "CLONES"
    );

    // Print each serve with clone count
    for serve in serves {
        let serve_pid = serve.pid.unwrap_or(0);

        // Count clones connected to this serve
        let clone_count = all_vms
            .iter()
            .filter(|vm| vm.config.serve_pid == Some(serve_pid))
            .count();

        println!(
            "{:<12} {:<10} {:<12} {:<20} {:<8}",
            truncate_id(&serve.vm_id, 8),
            serve_pid,
            format!("{:?}", serve.health_status),
            serve.config.snapshot_name.as_deref().unwrap_or("-"),
            clone_count,
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::snapshot::SnapshotVolumeConfig;
    use crate::storage::SnapshotKind;

    /// The synthesized RunArgs must carry the recorded boot-plan metadata:
    /// kernel profile (a btrfs disk needs a btrfs kernel), image mode + device
    /// (overlay layers live on a separate read-only disk), and the USER env
    /// (fc-agent maps the rootless username from it — without it a --user clone
    /// would diverge from the captured passwd entry).
    #[test]
    fn run_args_from_metadata_carries_boot_plan_fields() {
        let meta = crate::storage::SnapshotMetadata {
            image: "localhost/myapp:latest".to_string(),
            vcpu: 2,
            memory_mib: 1024,
            network_config: crate::network::NetworkConfig::default(),
            volumes: vec![],
            health_check_url: None,
            health_check_timeout: 5,
            hugepages: false,
            extra_disks: vec![],
            nfs_shares: vec![],
            username: Some("ubuntu".to_string()),
            user: Some("1000:1000".to_string()),
            port_mappings: vec![],
            forward_localhost: vec![],
            network_mode: crate::firecracker::FcNetworkMode::Rootless,
            ipv6_prefix: None,
            tty: false,
            interactive: false,
            kernel_profile: Some("btrfs".to_string()),
            image_mode: Some("overlay".to_string()),
            image_disk_path: Some(std::path::PathBuf::from("/cache/img.storage-v2.img")),
            hypervisor: Default::default(),
        };
        let args =
            run_args_from_snapshot_metadata(&meta, "clone".to_string(), 2, 1024, false, None);
        assert_eq!(args.kernel_profile.as_deref(), Some("btrfs"));
        assert_eq!(args.image_mode, Some(crate::cli::ImageMode::Overlay));
        assert_eq!(
            args.image_disk_override.as_deref(),
            Some(std::path::Path::new("/cache/img.storage-v2.img"))
        );
        assert_eq!(args.env, vec!["USER=ubuntu".to_string()]);
        assert_eq!(args.user.as_deref(), Some("1000:1000"));

        // Without a recorded user there must be no USER env.
        let mut meta2 = meta.clone();
        meta2.user = None;
        meta2.username = None;
        let args2 = run_args_from_snapshot_metadata(&meta2, "c".to_string(), 1, 512, false, None);
        assert!(args2.env.is_empty());

        // Recorded NFS shares must survive into the cold-boot RunArgs (a
        // disk-only clone of an NFS VM used to silently lose its shares).
        let mut meta3 = meta.clone();
        meta3.nfs_shares = vec![
            crate::state::types::NfsShare {
                host_path: "/srv/data".to_string(),
                mount_path: "/mydata".to_string(),
                read_only: true,
            },
            crate::state::types::NfsShare {
                host_path: "/srv/rw".to_string(),
                mount_path: "/rw".to_string(),
                read_only: false,
            },
        ];
        let args3 = run_args_from_snapshot_metadata(&meta3, "c".to_string(), 1, 512, false, None);
        assert_eq!(
            args3.nfs,
            vec![
                "/srv/data:/mydata:ro".to_string(),
                "/srv/rw:/rw".to_string()
            ]
        );
    }

    /// The synthesized cold-boot RunArgs must launch under the SAME backend that created
    /// the snapshot — a CH disk-only/reboot clone launched under Firecracker would mis-boot
    /// (and fail outright on a CH-only host). Regression for the hard-coded Firecracker.
    #[test]
    fn run_args_from_metadata_propagates_backend() {
        let base = crate::storage::SnapshotMetadata {
            image: "localhost/app:latest".to_string(),
            vcpu: 1,
            memory_mib: 512,
            network_config: crate::network::NetworkConfig::default(),
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
            network_mode: crate::firecracker::FcNetworkMode::Rootless,
            ipv6_prefix: None,
            tty: false,
            interactive: false,
            kernel_profile: None,
            image_mode: None,
            image_disk_path: None,
            hypervisor: crate::hypervisor::Backend::CloudHypervisor,
        };
        let args = run_args_from_snapshot_metadata(&base, "c".to_string(), 1, 512, false, None);
        assert_eq!(
            args.hypervisor,
            crate::cli::args::Hypervisor::CloudHypervisor
        );

        let mut fc = base.clone();
        fc.hypervisor = crate::hypervisor::Backend::Firecracker;
        let args_fc = run_args_from_snapshot_metadata(&fc, "c".to_string(), 1, 512, false, None);
        assert_eq!(
            args_fc.hypervisor,
            crate::cli::args::Hypervisor::Firecracker
        );
    }

    #[test]
    fn ensure_not_disk_only_rejects_disk_only_and_allows_full() {
        // Memory-restore paths must refuse a disk-only tag (no memory image)...
        assert!(ensure_not_disk_only(SnapshotKind::DiskOnly, "snapshot serve").is_err());
        assert!(
            ensure_not_disk_only(SnapshotKind::DiskOnly, "snapshot run --pid/--snapshot").is_err()
        );
        // ...but full snapshots pass through unchanged.
        assert!(ensure_not_disk_only(SnapshotKind::Full, "snapshot serve").is_ok());
    }

    #[test]
    fn test_volume_state_from_snapshot_rebuilds_specs_and_portable_flag() {
        let volumes = vec![
            SnapshotVolumeConfig {
                host_path: PathBuf::from("/data/shared"),
                guest_path: "/mnt/shared".to_string(),
                read_only: false,
                vsock_port: 5000,
                portable: true,
            },
            SnapshotVolumeConfig {
                host_path: PathBuf::from("/data/config"),
                guest_path: "/etc/app".to_string(),
                read_only: true,
                vsock_port: 5001,
                portable: true,
            },
        ];

        let (specs, portable) = volume_state_from_snapshot(&volumes);
        // Specs must round-trip through the HOST:GUEST[:ro] parser used by `snapshot create`.
        assert_eq!(
            specs,
            vec![
                "/data/shared:/mnt/shared".to_string(),
                "/data/config:/etc/app:ro".to_string(),
            ]
        );
        assert!(portable);

        let parts: Vec<&str> = specs[1].split(':').collect();
        assert_eq!(parts[0], "/data/config");
        assert_eq!(parts[1], "/etc/app");
        assert_eq!(parts.get(2).map(|s| *s == "ro"), Some(true));
    }

    #[test]
    fn test_volume_state_from_snapshot_empty() {
        let (specs, portable) = volume_state_from_snapshot(&[]);
        assert!(specs.is_empty());
        assert!(!portable);
    }

    #[tokio::test]
    async fn test_snapshot_restore_runtime_config_preserves_firecracker_overrides() {
        let args = SnapshotRunArgs {
            pid: None,
            snapshot: Some("snap".to_string()),
            name: Some("clone".to_string()),
            exec: None,
            startup_snapshot_base_key: None,
            cpu: None,
            mem: None,
            firecracker_bin: Some("/opt/firecracker-profile".to_string()),
            firecracker_args: Some("--enable-nv2".to_string()),
            hugepages: None,
            non_blocking_output: false,
            no_dirty_tracking: false,
            no_swap: false,
        };

        let runtime = snapshot_restore_runtime_config(&args, Some("nested")).await;
        assert_eq!(
            runtime.firecracker_bin,
            Some(PathBuf::from("/opt/firecracker-profile"))
        );
        assert_eq!(runtime.firecracker_args, Some("--enable-nv2".to_string()));
    }
}
