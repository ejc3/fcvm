use anyhow::{Context, Result};
use tokio::time::{sleep, Duration};

use crate::{bootplan, container, exec, lock_test, mmds, mounts, network, output, proxy, system};

/// Main agent logic — fetches plan, runs container, triggers shutdown.
pub async fn run() -> Result<()> {
    // Route fd 1/2 through the wedge-proof console pipe (writer thread →
    // /dev/console). Bypasses journald, which crashes after snapshot restore
    // (journal corrupted mid-write) and would take the console output path
    // with it — and guarantees a dead console can only LOSE log lines, never
    // block a thread on a full tty buffer. See fc-agent/src/console.rs.
    crate::console::init();

    eprintln!("[fc-agent] run_agent starting");

    system::raise_resource_limits();
    system::raise_cgroup_pids_limit();
    system::create_kvm_device();
    network::configure_dns_from_cmdline();
    network::configure_ipv6_from_cmdline();

    // Select the boot-plan transport (MMDS for Firecracker, vsock for VMMs without a
    // metadata service like Cloud Hypervisor) from the kernel command line.
    let transport = bootplan::detect_transport();

    // Fetch the container plan with retry.
    let plan = loop {
        match bootplan::fetch_plan(transport).await {
            Ok(p) => {
                eprintln!("[fc-agent] received container plan successfully");
                break p;
            }
            Err(e) => {
                eprintln!("[fc-agent] boot plan not ready: {:?}", e);
                eprintln!("[fc-agent] retrying in 500ms...");
                sleep(Duration::from_millis(500)).await;
            }
        }
    };

    system::save_proxy_settings(&plan);

    // For each eligible published TCP port, make a loopback-only guest service
    // reachable. The caller cannot tell which address the service will bind —
    // Chromium accepts --remote-debugging-address=0.0.0.0 and binds 127.0.0.1
    // anyway. Installed once, here, before the container starts; setup fails
    // closed and the reserved egress-proxy port is excluded.
    network::publish_to_loopback(&plan.published_guest_ports);

    if !plan.forward_localhost.is_empty() {
        network::setup_localhost_forwarding(&plan.forward_localhost);
    }

    // Egress proxy watch channel — proxy increments after each successful vsock connect.
    // Waiters use wait_for(|&v| v > captured) to detect reconnection.
    // No Notify needed: the proxy detects transport reset natively via Interest::ERROR
    // on the vsock fd (EPOLLERR fires instantly after snapshot restore).
    let egress_gen_rx = if plan.egress_proxy {
        let (gen_tx, gen_rx) = tokio::sync::watch::channel(0u64);
        eprintln!("[fc-agent] starting vsock egress proxy");
        tokio::spawn(proxy::run_egress_proxy(gen_tx));

        // Wait for initial vsock connection — ensures egress path is operational
        // before the container starts and health check reports "healthy".
        proxy::wait_for_egress_gen(
            &gen_rx,
            0,
            std::time::Duration::from_secs(10),
            "vsock connected",
        )
        .await;
        Some(gen_rx)
    } else {
        None
    };

    if let Err(e) = bootplan::sync_clock_from_host(transport).await {
        eprintln!("[fc-agent] WARNING: clock sync failed: {:?}", e);
        eprintln!("[fc-agent] continuing anyway (will rely on chronyd)");
    }

    // Create output channel — the writer task handles all vsock writes. The
    // reset watch surfaces EPOLLERR on the output connection (vsock transport
    // reset = snapshot restore) to the restore-epoch watcher.
    let (output, output_writer, vsock_reset_rx) = output::create();
    tokio::spawn(output_writer);

    // Shared flag: set by restore-epoch watcher, checked by notify_cache_ready_and_wait.
    // Breaks the 30s poll loop when POLLHUP is not delivered after snapshot restore.
    let restore_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Exec server rebind signal — shared by restore-epoch watcher.
    // After vsock transport reset, the listener's AsyncFd epoll becomes stale.
    // We use BOTH Notify (to wake the select loop) and AtomicBool (to persist the signal).
    // tokio::select! can lose Notify permits when accept() and notified() are both Ready
    // simultaneously — the AtomicBool flag catches this race.
    let exec_rebind = std::sync::Arc::new(tokio::sync::Notify::new());
    let exec_rebind_needed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Exec rebind confirmation — exec server signals after re_register() completes.
    // handle_clone_restore waits on this before reconnecting output, ensuring exec is
    // ready before the host starts health-checking via exec.
    let exec_rebind_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let exec_rebind_done_notify = std::sync::Arc::new(tokio::sync::Notify::new());

    // Start the restore-epoch watcher for the active transport: Firecracker polls MMDS,
    // Cloud Hypervisor polls the host's boot-plan vsock port (#632 P2). Both call
    // handle_clone_restore identically when a restore-epoch appears.
    {
        let restore_signals = crate::restore::RestoreSignals {
            output: output.clone(),
            restore_flag: restore_flag.clone(),
            exec_rebind: exec_rebind.clone(),
            exec_rebind_needed: exec_rebind_needed.clone(),
            exec_rebind_done: exec_rebind_done.clone(),
            exec_rebind_done_notify: exec_rebind_done_notify.clone(),
            egress_gen_rx: egress_gen_rx.clone(),
            vsock_reset_rx,
            nfs_mounts: plan.nfs_mounts.clone(),
        };
        tokio::spawn(async move {
            eprintln!("[fc-agent] starting restore-epoch watcher ({transport:?})");
            mmds::watch_restore_epoch(restore_signals, transport).await;
        });
    }

    // Write storage.conf with driver = "overlay" before starting the exec server.
    // The health monitor runs `podman inspect` as soon as exec accepts connections.
    // Without this, podman initializes its BoltDB with driver = "" (auto-detect),
    // which mismatches when mount_overlay_image() later writes driver = "overlay".
    container::write_early_storage_conf();

    // Start exec server with rebind signal for vsock transport reset recovery
    let (exec_ready_tx, exec_ready_rx) = tokio::sync::oneshot::channel();
    let exec_rebind_clone = exec_rebind.clone();
    let exec_rebind_needed_clone = exec_rebind_needed.clone();
    let exec_rebind_done_clone = exec_rebind_done.clone();
    let exec_rebind_done_notify_clone = exec_rebind_done_notify.clone();
    tokio::spawn(async move {
        exec::run_server(
            exec_ready_tx,
            exec_rebind_clone,
            exec_rebind_needed_clone,
            exec_rebind_done_clone,
            exec_rebind_done_notify_clone,
        )
        .await;
    });

    match tokio::time::timeout(Duration::from_secs(5), exec_ready_rx).await {
        Ok(Ok(())) => eprintln!("[fc-agent] exec server is ready"),
        Ok(Err(_)) => eprintln!("[fc-agent] WARNING: exec server ready signal dropped"),
        Err(_) => eprintln!("[fc-agent] WARNING: exec server did not become ready within 5s"),
    }

    // Mount filesystems. Mount failures are fatal: the container would otherwise
    // start with a plain empty directory bind-mounted where the volume should be,
    // write into the ephemeral rootfs, and the VM would still report healthy.
    // Propagating the error makes main() report container exit 1 and shut down.
    let mounted_fuse_paths = if !plan.volumes.is_empty() {
        eprintln!("[fc-agent] mounting {} FUSE volume(s)", plan.volumes.len());
        let paths = mounts::mount_fuse_volumes(&plan.volumes).context("mounting FUSE volumes")?;
        eprintln!("[fc-agent] FUSE volumes mounted successfully");
        paths
    } else {
        Vec::new()
    };
    let has_shared_volume = mounted_fuse_paths.iter().any(|p| p == "/mnt/shared");

    // Start chronyd for ongoing NTP time sync, OFF the boot critical path.
    // Nothing below depends on the daemon being up, and the setup costs a
    // process spawn plus a command-socket round trip — awaiting it inline made
    // every cold boot wait for chrony before it could mount disks and launch
    // the container. The task logs its own outcome, including the loud failure.
    //
    // The handle is KEPT, not dropped. Fire-and-forget is exactly what AGENTS.md
    // forbids ("DON'T: Use fire-and-forget without lifecycle management"), and the
    // hazard here is concrete: this task writes /etc/chrony.conf, signals an old
    // daemon and waits on a command socket. Dropping the handle means shutdown can
    // tear the VM down mid-sequence, leaving a half-written config for the next
    // boot to read. Awaited (bounded) at shutdown below.
    let chronyd_task = tokio::spawn(start_chronyd(plan.ntp_servers.clone()));

    let mounted_disk_paths = if !plan.extra_disks.is_empty() {
        eprintln!(
            "[fc-agent] mounting {} extra disk(s)",
            plan.extra_disks.len()
        );
        let paths = mounts::mount_extra_disks(&plan.extra_disks).context("mounting extra disks")?;
        eprintln!("[fc-agent] extra disks mounted successfully");
        paths
    } else {
        Vec::new()
    };

    if !plan.nfs_mounts.is_empty() {
        eprintln!("[fc-agent] mounting {} NFS share(s)", plan.nfs_mounts.len());
        mounts::mount_nfs_shares(&plan.nfs_mounts).context("mounting NFS shares")?;
        eprintln!("[fc-agent] NFS shares mounted successfully");
    }

    // Start lock test watcher if shared volume exists
    if has_shared_volume {
        let clone_id = system::get_clone_id().await;
        eprintln!(
            "[fc-agent] starting lock test watcher (clone_id={})",
            clone_id
        );
        tokio::spawn(async move {
            lock_test::watch_for_lock_test(clone_id).await;
        });
    }

    // Disk-only clone detection: a reflink of an already-provisioned rootfs carries
    // the provisioned marker. When present, this boot must preserve the captured
    // storage + container (skip the wipe, skip re-import, re-mount the loopback,
    // start the existing container) and only regenerate per-machine identity.
    let provisioned = container::is_provisioned();
    if provisioned {
        eprintln!("[fc-agent] provisioned marker present — disk-only clone cold boot");
    }

    // Set up btrfs storage if kernel supports it (avoids overlay idmap issues).
    // Skip for overlay mode — it manages its own storage.
    // For btrfs/archive/pull: creates loopback btrfs if kernel supports it.
    // For a clone, setup_btrfs_storage_if_available mounts the existing loopback
    // instead of reformatting it (guarded by the provisioned marker).
    match plan.image_mode.as_deref() {
        Some("overlay") => {
            eprintln!("[fc-agent] skipping btrfs loopback setup (image_mode=overlay)");
        }
        _ => {
            // Btrfs, archive, and pull modes all use btrfs loopback on rootfs.
            // The btrfs kernel module must be available (CONFIG_BTRFS_FS=y in btrfs profile).
            container::setup_btrfs_storage_if_available()
                .context("setting up container storage")?;
        }
    }

    // If --user is specified with a non-root UID, create the VM user BEFORE image import
    // so podman load runs as the target user (rootless podman has separate storage).
    // uid 0 is root — no user mapping needed, podman runs as root directly.
    let user_info = if let Some(ref user_spec) = plan.user {
        let uid: u32 = user_spec
            .split(':')
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if uid == 0 {
            eprintln!("[fc-agent] --user 0 (root), skipping user mapping");
            None
        } else {
            // Username comes from USER env var, which the host resolves from /etc/passwd
            // for the given UID (matching podman --userns=keep-id behavior).
            let desired_name = plan
                .env
                .get("USER")
                .map(|s| s.as_str())
                .unwrap_or("fcvm-user");
            let subuid_range = plan
                .subuid_start
                .zip(plan.subuid_count)
                .or_else(|| plan.subuid_start.map(|s| (s, 65536)));
            let (username, _uid, runtime_dir) =
                container::create_vm_user(user_spec, desired_name, subuid_range, provisioned);
            Some((username, runtime_dir))
        }
    } else {
        None
    };

    // Build the command prefix for running commands as the target user
    let cmd_prefix: Vec<String> = match &user_info {
        Some((username, runtime_dir)) => container::run_as_user_prefix(username, runtime_dir),
        None => vec![],
    };

    // Store prefix globally so exec server and health checks can use it
    container::set_podman_cmd_prefix(cmd_prefix.clone());

    // Reset root podman state to match storage.conf. The health monitor may have
    // run `podman inspect` via the exec server during setup, creating a stale
    // db.sql with the wrong graph driver. Only needed for root podman — user mode
    // already resets in create_vm_user(), and a root reset would destroy the
    // user's storage directory.
    // A clone's storage already matches storage.conf and holds the captured
    // container — a reset would erase it. Only wipe on a fresh first boot.
    if cmd_prefix.is_empty() && !provisioned {
        container::reset_podman_state();
    }

    // Prepare image based on delivery mode. A clone already has the image in
    // captured storage; re-importing is wasteful (and the host no longer ships
    // an image device), so skip straight to the launch using the recorded name.
    // Exception: overlay mode serves the image from a read-only additionalImageStore
    // device whose MOUNT doesn't survive a reboot — re-mount it (no re-import).
    let image_ref = if provisioned {
        if let (Some("overlay"), Some(device)) = (plan.image_mode.as_deref(), &plan.image_device) {
            eprintln!("[fc-agent] re-mounting overlay image store (provisioned re-boot)");
            let username = user_info.as_ref().map(|(name, _)| name.as_str());
            container::mount_overlay_image(device, &plan.image, username, false)?
        } else {
            eprintln!("[fc-agent] skipping image import (clone — image already in storage)");
            plan.image.clone()
        }
    } else {
        let image_ref = match (plan.image_mode.as_deref(), &plan.image_device) {
            (Some("overlay"), Some(device)) => {
                let username = user_info.as_ref().map(|(name, _)| name.as_str());
                container::mount_overlay_image(device, &plan.image, username, true)?
            }
            (Some("btrfs"), Some(device)) => {
                // Btrfs loopback was created in Phase 1 (setup_btrfs_storage_if_available).
                // Load the Docker archive from the block device into btrfs storage.
                container::import_image(device, &plan.image, &output, &cmd_prefix).await?
            }
            (Some("archive"), Some(device)) => {
                container::import_image(device, &plan.image, &output, &cmd_prefix).await?
            }
            (None, None) => {
                // Remote image — pull from registry
                container::pull_image(&plan).await?
            }
            (Some(mode), _) => {
                anyhow::bail!("unknown image_mode: {}", mode);
            }
            (None, Some(_)) => {
                anyhow::bail!("image_device set but image_mode is missing");
            }
        };
        // Storage + image are now in place; mark the disk provisioned so a
        // disk-only snapshot of this VM cold-boots clones without redoing it.
        // The marker deliberately does NOT assert the container exists — a capture
        // taken before `podman run` creates it is still valid (the clone's
        // container_exists probe falls back to a fresh `podman run` from the
        // preserved image, which is the correct degradation).
        container::write_provisioned_marker();
        image_ref
    };

    // Capture egress generation before cache handshake — used to detect reconnection
    // after snapshot restore (warm start). If proxy already reconnected by the time
    // we check, wait_for returns immediately because watch retains the latest value.
    let egress_gen_before = egress_gen_rx.as_ref().map(|rx| *rx.borrow());

    // Notify host for cache snapshot. notify_cache_ready_and_wait logs the
    // digest itself, then quiesces the console BEFORE the notification so the
    // host's pre-start snapshot pause can never capture the UART mid-transmit.
    // If the console cannot be proven quiet it returns Failed WITHOUT sending
    // cache-ready — the host never pauses us and the VM continues cold.
    match container::get_image_digest(&image_ref, &cmd_prefix).await {
        Ok(digest) => {
            let cache_result = container::notify_cache_ready_and_wait(&digest, &restore_flag);
            match cache_result {
                container::CacheResult::ColdStart => {
                    eprintln!("[fc-agent] cache ready: cold start (cache-ack received)");
                    // Pause/resume does NOT reset vsock — reconnect is harmless
                    // (just cycles the connection). No egress wait needed.
                    output.reconnect();
                }
                container::CacheResult::WarmStart => {
                    eprintln!("[fc-agent] cache ready: warm start (snapshot restore detected)");
                    // Vsock IS dead (VIRTIO_VSOCK_EVENT_TRANSPORT_RESET on restore).
                    // The restore-epoch watcher runs handle_clone_restore concurrently, and
                    // the host treats the first output connection as the readiness signal:
                    // it must not arrive before the exec server has re-registered and the
                    // egress proxy has reconnected (same ordering handle_clone_restore
                    // enforces). Wait on the exec_rebind_done flag rather than its Notify —
                    // exec.rs uses notify_one() and the restore handler must win that
                    // notification.
                    let rebind_wait = std::time::Instant::now();
                    while !exec_rebind_done.load(std::sync::atomic::Ordering::Acquire) {
                        if rebind_wait.elapsed() > std::time::Duration::from_secs(10) {
                            eprintln!(
                                "[fc-agent] WARNING: exec re-register not confirmed within 10s, \
                                 reconnecting output anyway"
                            );
                            break;
                        }
                        sleep(Duration::from_millis(25)).await;
                    }
                    // No explicit signal to proxy needed — it detects the dead vsock fd
                    // natively via Interest::ERROR (EPOLLERR fires instantly after restore).
                    // Just wait for the watch channel to confirm reconnection.
                    if let (Some(rx), Some(gen_before)) = (&egress_gen_rx, egress_gen_before) {
                        proxy::wait_for_egress_gen(
                            rx,
                            gen_before,
                            std::time::Duration::from_secs(5),
                            "reconnected after warm start",
                        )
                        .await;
                    }
                    // Reconnect output before starting the container — for fast-exit
                    // containers (echo + exit in ~200ms), output must be live or the
                    // container's stdout/stderr goes to the dead vsock.
                    output.reconnect();
                }
                container::CacheResult::Failed => {
                    eprintln!("[fc-agent] WARNING: cache-ready handshake failed, continuing");
                }
            }
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: failed to get image digest: {:?}", e);
        }
    }

    // VM-level setup: hostname and sysctl (runs as root before container starts).
    // When using --user, the container runs as non-root and can't do these.
    // With --network=host, the container shares the VM's hostname.
    if let Some(hostname) = plan.env.get("WWW_HOSTNAME") {
        if !hostname.is_empty() {
            let _ = std::process::Command::new("hostname")
                .arg(hostname)
                .output();
            eprintln!("[fc-agent] set hostname to {}", hostname);
        }
    }
    // net.ipv4.ip_unprivileged_port_start=0: With --user, the container runs as
    // a non-root user but needs to bind port 80. The VM is single-tenant so this is safe.
    for sysctl in &[
        "fs.file-max=2097152",
        "fs.nr_open=2097152",
        "net.ipv4.ip_unprivileged_port_start=0",
        "kernel.threads-max=4194304",
        "net.core.somaxconn=65535",
    ] {
        let _ = std::process::Command::new("sysctl")
            .args(["-w", sysctl])
            .output();
    }

    // Add host identity IPv6 to loopback and eth0 (requires root, can't do from
    // rootless container). Pass the address via HOST_IPV6 env var in the Plan.
    //
    // In routed mode, only add to lo — adding to eth0 causes the kernel to use it
    // as source address for outbound IPv6, but the bridge can't deliver replies
    // because NDP for the fbwhoami address isn't configured on the namespace side.
    if let Some(ipv6) = plan.env.get("HOST_IPV6") {
        if !ipv6.is_empty() {
            // Check if routed mode: guest_ipv6 includes /128 prefix in the boot param.
            // In routed mode, only add fbwhoami to lo — not eth0 — to avoid source
            // address conflicts (NDP for fbwhoami isn't configured on the namespace side).
            // Pasta mode uses /64 (or no prefix) and is fine with fbwhoami on eth0.
            let is_routed_mode = std::fs::read_to_string("/proc/cmdline")
                .map(|c| {
                    c.split_whitespace()
                        .any(|p| p.starts_with("ipv6=") && p.contains("/128"))
                })
                .unwrap_or(false);
            let devices: &[&str] = if is_routed_mode {
                &["lo"]
            } else {
                &["lo", "eth0"]
            };
            for dev in devices {
                let result = std::process::Command::new("ip")
                    .args(["addr", "add", &format!("{}/128", ipv6), "dev", dev])
                    .output();
                match result {
                    Ok(o) if o.status.success() => {
                        eprintln!("[fc-agent] added {} to {} for host identity", ipv6, dev);
                    }
                    Ok(o) => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        if stderr.contains("File exists") {
                            eprintln!("[fc-agent] {} already on {}", ipv6, dev);
                        }
                    }
                    Err(e) => eprintln!("[fc-agent] ip addr add failed: {}", e),
                }
            }
        }
    }

    // A clone shares its source's machine-id / SSH host keys via the reflinked
    // disk. Regenerate them so concurrent clones have distinct identities.
    if provisioned {
        container::regenerate_identity();
    }

    eprintln!("[fc-agent] launching container: {}", image_ref);
    system::wait_for_cgroup_controllers().await;

    // Build podman args (pass user info if available for rootless setup).
    // On a clone the container already exists — start it (preserving its
    // captured writable layer) instead of `podman run`-ing a fresh one.
    let user_ref = user_info
        .as_ref()
        .map(|(username, runtime_dir)| (username.as_str(), runtime_dir.as_str()));
    let podman_args = if provisioned && container::container_exists(&cmd_prefix) {
        eprintln!("[fc-agent] starting captured fcvm-container (clone cold boot)");
        container::build_start_args(&plan, user_ref)
    } else {
        container::build_podman_args(&plan, &image_ref, user_ref)
    };

    // TTY mode: blocks, never returns
    if plan.tty {
        eprintln!("[fc-agent] TTY mode enabled, using PTY");
        container::run_tty(&podman_args, &plan, &mounted_fuse_paths);
    }

    // Non-TTY mode: async
    let exit_code = container::run_async(&podman_args, &output, plan.non_blocking_output).await?;

    // Notify host of exit
    crate::vsock::notify_container_exit(exit_code);

    // Cleanup
    mounts::unmount_paths(&mounted_fuse_paths, "FUSE volume");
    if !mounted_fuse_paths.is_empty() {
        sleep(Duration::from_millis(100)).await;
    }
    mounts::unmount_disks(&mounted_disk_paths);
    if let Some("overlay") = plan.image_mode.as_deref() {
        mounts::unmount_paths(&["/mnt/image-store".to_string()], "image store");
    }

    // Let chronyd setup finish before the VM goes away, so it cannot be killed
    // between writing the config and starting the daemon.
    //
    // Bounded, and the bound is not a guess: `start_chronyd` already bounds its own
    // waits (CHRONYD_READY_TIMEOUT), so a task still running here is one whose
    // internal deadline has not yet expired. A short grace covers the ordinary case —
    // it is nearly always finished long before the container exits — without letting
    // a wedged setup hold the VM open, which would trade a boot-path stall for a
    // shutdown-path stall and defeat the point of moving it off the critical path.
    match tokio::time::timeout(Duration::from_secs(5), chronyd_task).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("[fc-agent] chronyd setup task panicked: {e}"),
        Err(_) => eprintln!(
            "[fc-agent] chronyd setup still running after 5s at shutdown; abandoning it. \
             The guest may boot next time with a partially written /etc/chrony.conf."
        ),
    }

    // Shutdown output writer
    output.shutdown().await;

    system::shutdown_vm(exit_code).await
}

/// Config file fc-agent writes and hands to chronyd with an explicit `-f`.
///
/// The `-f` is not cosmetic: chronyd's compiled-in default on this rootfs is
/// `/etc/chrony/chrony.conf` (the file `rootfs-config.toml` ships), so a config
/// written anywhere else is read by nobody — and takes `makestep 1 -1` with it,
/// the one directive that lets the clock be stepped at ANY time and therefore
/// the one that matters after a snapshot restore.
const CHRONY_CONF: &str = "/etc/chrony.conf";

/// The distro config shipped in the rootfs (see `rootfs-config.toml`), used when
/// the host did not supply its own servers. fc-agent copies its `server`/`pool`
/// directives rather than restating them, so changing the servers stays a
/// rootfs-config change.
const ROOTFS_CHRONY_CONF: &str = "/etc/chrony/chrony.conf";

/// Used only if the rootfs config carries no `server`/`pool` directive at all.
const FALLBACK_NTP_SOURCE: &str = "pool pool.ntp.org iburst";

/// Bound on waiting for chronyd to answer chronyc AND report an NTP source.
///
/// The wait ends when a real `chronyc` round trip reports a source, so this only
/// bounds a daemon that never comes up — or one whose pool never resolves, which
/// is equally worth an error. Generous because this runs off the critical path:
/// nothing waits on it, so the only cost of patience is a later log line, while
/// impatience would mean crying wolf about a guest whose pool was just slow.
const CHRONYD_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound on waiting for a pre-existing chronyd to exit after SIGTERM, before
/// escalating to SIGKILL.
const CHRONYD_TERM_TIMEOUT: Duration = Duration::from_secs(2);

/// Start chronyd for ongoing NTP time sync.
///
/// Spawned as its own task by [`run`], never awaited: nothing in the boot
/// sequence depends on the daemon, so this work belongs off the critical path.
///
/// Both waits inside are real signals, not delays. Replacing the daemon systemd
/// may have started waits on a pidfd for that process to actually exit; the
/// daemon's readiness waits on chronyc actually getting an answer. The previous
/// fixed 500ms + 1s pair cost every cold boot 1.5s AND failed silently: when the
/// command socket took longer than the guess, every configuration step was
/// discarded with no error, leaving a VM with ZERO NTP sources and a clock free
/// to drift — worst exactly after a snapshot restore, where chrony is the
/// correction.
async fn start_chronyd(host_servers: Vec<String>) {
    // The HOST's own servers win when it has any. They arrive already resolved to
    // addresses (see `host_ntp_servers` in vm_config.rs), which is what makes them
    // usable here: a guest on a restricted network typically cannot reach the
    // public pool the rootfs names, and often cannot resolve it either.
    let (mut sources, origin): (Vec<String>, String) = if host_servers.is_empty() {
        (rootfs_ntp_sources().await, ROOTFS_CHRONY_CONF.to_string())
    } else {
        (
            host_servers
                .iter()
                .map(|addr| format!("server {addr} iburst"))
                .collect(),
            "the host's NTP servers, via the boot plan".to_string(),
        )
    };
    if sources.is_empty() {
        eprintln!(
            "[fc-agent] WARNING: no server/pool directive in {ROOTFS_CHRONY_CONF}; \
             falling back to '{FALLBACK_NTP_SOURCE}'"
        );
        sources.push(FALLBACK_NTP_SOURCE.to_string());
    }

    // `makestep 1 -1`: step the clock at ANY update, not just the first few. A
    // restored VM can resume hours off; slewing that back would take days.
    let config = format!(
        "# Written by fc-agent at boot; chronyd is started with -f on this path.\n\
         # NTP sources from {origin}.\n\
         {}\n\
         makestep 1 -1\n\
         driftfile /var/lib/chrony/drift\n\
         cmdallow 127.0.0.1\n",
        sources.join("\n")
    );

    let _ = tokio::fs::create_dir_all("/var/lib/chrony").await;
    let _ = tokio::fs::create_dir_all("/var/run/chrony").await;
    if let Err(e) = tokio::fs::write(CHRONY_CONF, config).await {
        eprintln!("[fc-agent] ERROR: cannot write {CHRONY_CONF}: {e}; no NTP time sync");
        return;
    }

    // The rootfs enables chrony.service, so systemd starts its own chronyd as the
    // _chrony user — which cannot send UDP in this VM. Replace it, and wait for it
    // to be GONE, because the replacement binds the same command socket.
    stop_running_chronyd().await;

    // chronyd daemonizes: this command returns as soon as the daemon is spawned,
    // which is BEFORE the daemon has bound its command socket.
    match tokio::process::Command::new("/usr/sbin/chronyd")
        .args(["-f", CHRONY_CONF, "-u", "root"])
        .output()
        .await
    {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            eprintln!(
                "[fc-agent] ERROR: chronyd failed to start ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
            return;
        }
        Err(e) => {
            eprintln!("[fc-agent] ERROR: failed to exec chronyd: {e}");
            return;
        }
    }

    // Positive verification. Waiting for chronyc to answer proves the daemon is
    // up; counting its sources proves the config we wrote was actually loaded.
    // Zero sources is a REAL failure — the clock will drift with nothing to
    // correct it — so it is reported as an error rather than passed over.
    match wait_for_chrony_sources().await {
        Ok(count) => eprintln!("[fc-agent] chronyd started with {count} NTP source(s)"),
        Err(e) => eprintln!("[fc-agent] ERROR: chronyd is not usable: {e}"),
    }
}

/// The `server`/`pool` directives from the rootfs's chrony config.
///
/// Copied verbatim so their options (`iburst`, …) and address syntax survive
/// unchanged — which also sidesteps having to re-render addresses that chronyd's
/// parser is picky about.
async fn rootfs_ntp_sources() -> Vec<String> {
    let Ok(content) = tokio::fs::read_to_string(ROOTFS_CHRONY_CONF).await else {
        return Vec::new();
    };
    content
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("server ") || line.starts_with("pool "))
        .map(str::to_string)
        .collect()
}

/// Stop systemd's chrony unit, then terminate any chronyd still running and wait
/// until each one has actually exited.
///
/// Stopping the UNIT first is what makes this race-free. `chrony.service` is
/// enabled in the rootfs, so systemd starts its own chronyd concurrently with
/// fc-agent; killing by PID alone would lose to the ordering where systemd has
/// not spawned it yet, and the unit would come up moments later and fight ours
/// for the command socket. `systemctl stop` is authoritative over both a running
/// service and a queued start job. Failure is fine and expected in a guest whose
/// systemd is not up yet — the PID sweep below is the backstop either way.
///
/// The per-process wait is a pidfd — readable exactly when that process dies —
/// rather than a fixed delay, which was simultaneously too long for the common
/// case (the daemon exits in ~1ms) and unreliable under load. Signals go through
/// `pidfd_send_signal` so a PID recycled between the /proc scan and the signal
/// can never be hit: the pidfd pins the process it was opened for.
async fn stop_running_chronyd() {
    let _ = tokio::process::Command::new("systemctl")
        .args(["stop", "chrony"])
        .output()
        .await;

    for pid in running_chronyd_pids() {
        let pidfd = match PidFd::open(pid) {
            Some(fd) => fd,
            // Already gone between the scan and the open — nothing to wait for.
            None => continue,
        };
        if !pidfd.send_signal(libc::SIGTERM) {
            continue;
        }
        if pidfd.wait_for_exit(CHRONYD_TERM_TIMEOUT).await {
            continue;
        }
        eprintln!("[fc-agent] chronyd pid {pid} ignored SIGTERM; sending SIGKILL");
        if pidfd.send_signal(libc::SIGKILL) {
            // SIGKILL cannot be caught; a process that still has not exited is
            // wedged in the kernel (uninterruptible), and the new daemon will
            // report the socket conflict itself.
            pidfd.wait_for_exit(CHRONYD_TERM_TIMEOUT).await;
        }
    }
}

/// PIDs of all running `chronyd` processes, from `/proc/<pid>/comm`.
fn running_chronyd_pids() -> Vec<libc::pid_t> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut pids = Vec::new();
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<libc::pid_t>() else {
            continue;
        };
        if let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) {
            if comm.trim() == "chronyd" {
                pids.push(pid);
            }
        }
    }
    pids
}

/// A pidfd: a handle to a specific process that is immune to PID reuse and
/// becomes readable when that process exits.
struct PidFd(std::os::fd::OwnedFd);

impl PidFd {
    /// Open a pidfd for `pid`, or `None` if the process is already gone.
    fn open(pid: libc::pid_t) -> Option<Self> {
        // SAFETY: pidfd_open takes a pid and flags and returns a new fd or -1.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            return None;
        }
        // SAFETY: pidfd_open returned a fresh, owned fd.
        Some(Self(unsafe {
            <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd as std::os::fd::RawFd)
        }))
    }

    /// Signal the pinned process. Returns false if it is already gone.
    fn send_signal(&self, signal: libc::c_int) -> bool {
        use std::os::fd::AsRawFd;
        // SAFETY: the fd is a live pidfd owned by self; a null siginfo means
        // "behave like kill()".
        let rc = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        rc == 0
    }

    /// Wait until the pinned process exits, bounded by `timeout`.
    ///
    /// Returns true if it exited. A pidfd becomes readable on exit, so this is
    /// an epoll wakeup rather than a poll loop.
    async fn wait_for_exit(&self, timeout: Duration) -> bool {
        use std::os::fd::AsRawFd;
        let Ok(async_fd) = tokio::io::unix::AsyncFd::new(self.0.as_raw_fd()) else {
            return false;
        };
        tokio::time::timeout(timeout, async_fd.readable())
            .await
            .is_ok_and(|guard| guard.is_ok())
    }
}

/// Wait for chronyd's command socket to answer, then report how many NTP
/// sources it has configured.
///
/// Each attempt is a real `chronyc` round trip, so the loop ends on the daemon
/// being reachable — not on elapsed time. Sources are counted afterwards and
/// re-checked until at least one appears, because a `pool` directive only
/// materializes its sources once the pool name resolves.
async fn wait_for_chrony_sources() -> Result<usize> {
    let deadline = tokio::time::Instant::now() + CHRONYD_READY_TIMEOUT;

    loop {
        // Whatever went wrong on THIS attempt — reported if the deadline lands
        // next, so the error describes the state we actually gave up in.
        let failure = match chronyc_sources().await {
            Ok(count) if count > 0 => return Ok(count),
            Ok(_) => "chronyd is running but has 0 NTP sources — its config was not \
                      loaded, or its pool did not resolve"
                .to_string(),
            Err(e) => e,
        };

        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("{failure} (after {CHRONYD_READY_TIMEOUT:?})");
        }
        // chronyc is a fork+exec per attempt (~2ms); this keeps the retries from
        // becoming a spin without materially delaying the answer.
        sleep(Duration::from_millis(20)).await;
    }
}

/// Number of NTP sources chronyd reports, or the reason chronyc could not ask.
///
/// `chronyc -n sources` prints a column header, a `=====` rule, then one line
/// per source; it exits non-zero when the daemon cannot be reached. Counting
/// from the rule rather than a fixed offset keeps this correct if the header
/// ever gains a line.
async fn chronyc_sources() -> std::result::Result<usize, String> {
    let out = tokio::process::Command::new("/usr/bin/chronyc")
        .args(["-n", "sources"])
        .output()
        .await
        .map_err(|e| format!("cannot exec chronyc: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "chronyc -n sources failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    Ok(stdout
        .lines()
        .skip_while(|line| !line.contains("====="))
        .skip(1)
        .filter(|line| !line.trim().is_empty())
        .count())
}
