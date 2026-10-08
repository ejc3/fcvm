use anyhow::{Context, Result};
use tokio::time::{sleep, Duration};

use crate::{bootplan, container, exec, lock_test, mmds, mounts, network, output, proxy, system};

/// Deadline for a warm start's restore-readiness publication. Generous: the
/// watcher's own work (exec rebind, egress reconnect) is sub-second, so this
/// only has to exceed a slow host's restore, and its job is to convert an
/// indefinite stall into a diagnosable failure.
const WARM_START_READINESS_TIMEOUT: Duration = Duration::from_secs(120);

/// Main agent logic — fetches plan, runs container, triggers shutdown.
pub async fn run() -> Result<()> {
    // Route fd 1/2 through the wedge-proof console pipe (writer thread →
    // /dev/console). Bypasses journald, which crashes after snapshot restore
    // (journal corrupted mid-write) and would take the console output path
    // with it — and guarantees a dead console can only LOSE log lines, never
    // block a thread on a full tty buffer. See fc-agent/src/console.rs.
    crate::console::init();

    // Periodic guest vitals. A plain OS thread, not a tokio task: a wedged
    // procfs read must not be able to starve the runtime that serves execs.
    // The `[fcvm-vitals]` prefix routes these to the per-VM debug file and NOT
    // the job log (src/firecracker/vm.rs classifies console lines), which is
    // what lets this run for every VM without flooding a runner.
    //
    // Every 10s the compact line. Every second, while the guest is piled up,
    // the line that names the threads (`vitals::Pileup`). The thread has its
    // own name so that it does not count itself as `fc-agent`.
    let sampler = std::thread::Builder::new()
        .name("fcvm-vitals".to_string())
        .spawn(|| {
            let start = std::time::Instant::now();
            let mut pileup = crate::vitals::Pileup::default();
            let (mut tick, mut previous) = (0u64, None);
            loop {
                if crate::vitals::sample_due(previous, tick) {
                    eprintln!("[fcvm-vitals] {}", crate::vitals::sample_line());
                }
                if let Some(line) = pileup.tick(tick) {
                    eprintln!("[fcvm-vitals] pileup {line}");
                }
                // To the next deadline still ahead, so a slow scan neither
                // stretches the cadence nor is followed by a burst of scans.
                previous = Some(tick);
                tick = crate::vitals::next_tick(tick, start.elapsed());
                let next = start + std::time::Duration::from_secs(tick);
                std::thread::sleep(next.saturating_duration_since(std::time::Instant::now()));
            }
        });
    if let Err(error) = sampler {
        eprintln!("[fc-agent] guest vitals sampler did not start: {error}");
    }

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
    // Before anything can accept a connection: make the host's health-check
    // address resolve to the bridge, not to whichever ARP reply arrives first.
    network::pin_namespace_neighbour().await;

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
        .await
        .context("waiting for initial egress proxy readiness")?;
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

    // Pending until the cache handshake proves this is a cold start or the
    // restore watcher completes every restore phase. The state owns the only
    // explicit output reconnect edge, so a failed restore cannot race an
    // independent WarmStart reconnect below.
    let restore_status = crate::restore::RestoreStatus::new(output.clone());

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
            restore_status: restore_status.clone(),
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

    // Name the overlay driver in storage.conf before any podman process runs.
    // Podman records the driver in its database the first time it runs, and
    // mount_overlay_image() later rewrites storage.conf with the same driver, so a
    // podman command exec'd before that rewrite still matches it.
    container::write_early_storage_conf();

    // Mount filesystems. Mount failures are fatal: the container would otherwise
    // start with a plain empty directory bind-mounted where the volume should be,
    // write into the ephemeral rootfs, and the VM would still report healthy.
    // Propagating the error makes main() report container exit 1 and shut down.
    //
    // Every mount fc-agent makes before the container starts goes into this record: a
    // FUSE volume once its mount is ready, after every volume of its level has started,
    // and every other mount right after it is made. The record is checked after the
    // last one.
    let mut mount_record = mounts::MountRecord::default();
    let mounted_fuse_paths = if !plan.volumes.is_empty() {
        eprintln!("[fc-agent] mounting {} FUSE volume(s)", plan.volumes.len());
        let paths = mounts::mount_fuse_volumes(&plan.volumes, &mut mount_record)
            .context("mounting FUSE volumes")?;
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
        let paths = mounts::mount_extra_disks(&plan.extra_disks, &mut mount_record)
            .context("mounting extra disks")?;
        eprintln!("[fc-agent] extra disks mounted successfully");
        paths
    } else {
        Vec::new()
    };

    // pmem mounts are guest kernel state carried in the snapshot, as extra disks
    // are, so a restore does not mount them again.
    let mounted_pmem_paths = if !plan.pmem_mounts.is_empty() {
        eprintln!(
            "[fc-agent] mounting {} pmem device(s)",
            plan.pmem_mounts.len()
        );
        let paths = mounts::mount_pmem_devices(&plan.pmem_mounts, &mut mount_record)
            .context("mounting pmem devices")?;
        eprintln!("[fc-agent] pmem devices mounted with DAX");
        paths
    } else {
        Vec::new()
    };

    if !plan.nfs_mounts.is_empty() {
        eprintln!("[fc-agent] mounting {} NFS share(s)", plan.nfs_mounts.len());
        mounts::mount_nfs_shares(&plan.nfs_mounts, Some(&mut mount_record))
            .context("mounting NFS shares")?;
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
            container::setup_btrfs_storage_if_available(&mut mount_record)
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

    // Open exec only now, with the rebind signal for vsock transport reset
    // recovery. The host's health monitor runs `podman inspect` over exec as soon
    // as the server accepts connections, so no podman command can start before
    // storage setup, the container user and the command prefix are done. Overlay
    // image mode rewrites storage.conf after this point, in mount_overlay_image(),
    // with the driver write_early_storage_conf() already named. The host's exec
    // client retries until the server listens.
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

    // Prepare image based on delivery mode. A clone already has the image in
    // captured storage; re-importing is wasteful (and the host no longer ships
    // an image device), so skip straight to the launch using the recorded name.
    // Exception: overlay mode serves the image from a read-only additionalImageStore
    // device whose MOUNT doesn't survive a reboot — re-mount it (no re-import).
    let image_ref = if provisioned {
        if let (Some("overlay"), Some(device)) = (plan.image_mode.as_deref(), &plan.image_device) {
            eprintln!("[fc-agent] re-mounting overlay image store (provisioned re-boot)");
            let username = user_info.as_ref().map(|(name, _)| name.as_str());
            container::mount_overlay_image(device, &plan.image, username, &mut mount_record)?
        } else {
            eprintln!("[fc-agent] skipping image import (clone — image already in storage)");
            plan.image.clone()
        }
    } else {
        let image_ref = match (plan.image_mode.as_deref(), &plan.image_device) {
            (Some("overlay"), Some(device)) => {
                let username = user_info.as_ref().map(|(name, _)| name.as_str());
                container::mount_overlay_image(device, &plan.image, username, &mut mount_record)?
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

    // fc-agent mounts nothing more before the container starts. A mount can cover one
    // made before it through a symlink in the guest, such as Ubuntu's /var/run -> /run,
    // which the host's check of the guest paths compares as text and cannot see: a pmem
    // device over an extra disk, or an NFS share over a pmem device. Checked before the
    // cache-ready handshake so no snapshot captures a covered mount. A restored clone
    // remounts its NFS shares after this check by design: the restore reproduces the
    // mounts this check passed. The check consumes the record, closing the descriptors
    // that hold its mounts, so the plain umount of the extra disks at shutdown works.
    mount_record
        .check_none_covered()
        .context("checking that no mount fc-agent made covers an earlier one")?;

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
                    // A cold verdict can still arrive after a snapshot boundary
                    // severed our established vsock connections (the VMM queues
                    // a transport reset at snapshot SAVE, processed when the
                    // resumed source runs again). The output writer detects its
                    // dead connection natively and reconnects; requesting the
                    // reconnect here is harmless when no boundary was crossed
                    // (it just cycles the connection). No egress wait needed.
                    restore_status
                        .succeed()
                        .context("publishing cold-start output readiness")?;
                }
                container::CacheResult::WarmStart => {
                    eprintln!("[fc-agent] cache ready: warm start (snapshot restore detected)");
                    // The watcher publishes Succeeded only after exec and egress are
                    // ready, and requests output reconnect before waking this wait.
                    // Failed is terminal and propagates to main(), which shuts the clone
                    // down without ever publishing host-visible readiness.
                    //
                    // Bounded: the watcher publishes NEITHER verdict if it never
                    // observes a restore epoch (e.g. every MMDS fetch fails, which
                    // mmds.rs logs and retries forever). Without a deadline this
                    // await never returns, so the guest neither starts the container
                    // nor reports a failure — the host just sees silence until its
                    // own health timeout. Fail closed inside the guest instead, so
                    // the reason reaches the serial console.
                    tokio::time::timeout(
                        WARM_START_READINESS_TIMEOUT,
                        restore_status.wait_for_output_readiness(),
                    )
                    .await
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "warm-start restore readiness was not published within {:?}: \
                             the restore watcher never observed a restore epoch",
                            WARM_START_READINESS_TIMEOUT
                        )
                    })
                    .context("waiting for warm-start restore readiness")??;
                }
                container::CacheResult::Doomed => {
                    // The host answered "cache-doomed": this VM produced the
                    // pre-start snapshot and is being replaced by a restore of
                    // it. Launching the container here would race the
                    // replacement clone's startup against shared volumes and
                    // double-run container startup. Park; the host tears this
                    // VMM down momentarily (and the health deadline reaps us
                    // if it somehow does not).
                    eprintln!("[fc-agent] cache ready: doomed (VM being replaced); parking");
                    std::future::pending::<()>().await;
                    unreachable!("std::future::pending never resolves");
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

    // TTY mode: never returns
    if plan.tty {
        eprintln!("[fc-agent] TTY mode enabled, using PTY");
        container::run_tty(&podman_args, &plan, &mounted_fuse_paths).await;
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
    // The pmem devices mounted after the extra disks, so they unmount before them.
    mounts::unmount_paths(&mounted_pmem_paths, "pmem device");
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

#[cfg(test)]
mod tests {
    /// fc-agent opens its exec server only after storage.conf names the overlay
    /// driver, storage setup has run, and the container user and podman command
    /// prefix exist.
    ///
    /// The host's health monitor runs `podman inspect` over exec from the moment
    /// the server accepts connections, two to five times before the container
    /// starts in every boot measured. A podman process that runs before
    /// storage.conf names the right driver records the wrong one in podman's
    /// database, and one that runs while podman's state is being reset fails
    /// with "attempt to write a readonly database". Overlay image mode rewrites
    /// storage.conf after exec opens, so it relies on the early write naming the
    /// same driver.
    #[test]
    fn exec_opens_after_container_storage_is_final() {
        let source = include_str!("agent.rs");
        let body = &source[..source
            .find("\n#[cfg(test)]\nmod tests {")
            .expect("agent.rs has no test module")];
        let exec_opens = body
            .find("exec::run_server(")
            .expect("fc-agent no longer starts the exec server");
        for step in [
            "container::write_early_storage_conf()",
            "container::setup_btrfs_storage_if_available(",
            "container::create_vm_user(",
            "container::set_podman_cmd_prefix(",
        ] {
            let at = body
                .find(step)
                .unwrap_or_else(|| panic!("fc-agent no longer calls {step}"));
            assert!(
                at < exec_opens,
                "fc-agent opens its exec server before {step}. A podman command \
                 exec'd in that window runs against storage setup that has not \
                 finished."
            );
        }
    }

    /// Shutdown unmounts in reverse mount order. pmem devices mount after the extra
    /// disks, so a pmem mount point inside a disk has to go first, or the disk's
    /// unmount fails busy and a read-write disk can be left dirty.
    #[test]
    fn pmem_devices_unmount_before_the_extra_disks() {
        let source = include_str!("agent.rs");
        let body = &source[..source
            .find("\n#[cfg(test)]\nmod tests {")
            .expect("agent.rs has no test module")];
        let pmem = body
            .find("mounts::unmount_paths(&mounted_pmem_paths")
            .expect("fc-agent no longer unmounts pmem devices");
        let disks = body
            .find("mounts::unmount_disks(&mounted_disk_paths)")
            .expect("fc-agent no longer unmounts extra disks");
        assert!(
            pmem < disks,
            "fc-agent unmounts the extra disks before the pmem devices mounted after them"
        );
    }

    /// Every mount fc-agent makes before the container starts goes into the boot's mount
    /// record, and the record is checked after the last one and before the cache-ready
    /// handshake, so a pre-start snapshot never captures a covered mount.
    ///
    /// The test reads every file under fc-agent/src for code that mounts: running
    /// mount(8), a mount.<type> helper or fusermount, a mount system call through libc or
    /// nix, or a FUSE mount through a fuse_pipe:: or fuser:: path. Each function that does
    /// is listed below with the call in agent.rs that reaches it, so a new one fails this
    /// test until it is listed. Every listed call in agent.rs must pass the mount record
    /// and come before the check. Calls from other files are not checked: restore.rs
    /// remounts a restored clone's NFS shares after the check by design, because the
    /// restore reproduces the mounts the check passed.
    ///
    /// Each line that mounts must be followed by a .record( call in its function before
    /// the next line that mounts, where a function ends at the next fn line of its file.
    /// The FUSE mount in mount_vsock_reconnectable blocks for as long as the volume is
    /// mounted, so mount_fuse_volumes must record the volume after waiting for it
    /// instead. The scan cannot see a mount through an aliased import, a program name
    /// held in a constant (Command::new(CONST)), or a shell that runs mount (sh -c), and
    /// it cannot tell whether a record call is on the path a successful mount takes.
    #[test]
    fn mounts_are_checked_after_the_last_mount() {
        fn without_tests(source: &str) -> &str {
            &source[..source
                .find("\n#[cfg(test)]\nmod tests {")
                .unwrap_or(source.len())]
        }
        /// The name of the function a source line declares.
        fn function_name(line: &str) -> Option<&str> {
            let mut rest = line.trim_start();
            loop {
                let before = rest;
                for prefix in [
                    "pub(crate) ",
                    "pub(super) ",
                    "pub ",
                    "async ",
                    "unsafe ",
                    "const ",
                ] {
                    rest = rest.strip_prefix(prefix).unwrap_or(rest);
                }
                if rest == before {
                    break;
                }
            }
            rest.strip_prefix("fn ")?.split(['(', '<']).next()
        }
        /// Whether a source line mounts something.
        fn mounts_something(line: &str) -> bool {
            let program = line
                .split("Command::new(\"")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .map(|program| program.rsplit('/').next().unwrap_or(program));
            program.is_some_and(|program| {
                program == "mount"
                    || program.starts_with("mount.")
                    || program.starts_with("fusermount")
            }) || [
                "libc::mount(",
                "libc::fsmount(",
                "libc::move_mount(",
                "SYS_mount",
                "SYS_fsmount",
                "SYS_move_mount",
                "nix::mount",
                "fuse_pipe::mount",
                "fuser::mount",
                "fuser::spawn_mount",
            ]
            .iter()
            .any(|call| line.contains(call))
        }
        /// The arguments of a call, from the text after its opening parenthesis.
        fn arguments(after_paren: &str) -> &str {
            let mut depth = 1;
            for (at, c) in after_paren.char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            return &after_paren[..at];
                        }
                    }
                    _ => {}
                }
            }
            after_paren
        }

        let mut mounting = Vec::new();
        // Each line that mounts with no record call after it in its function before the
        // next line that mounts, as "function at file:line".
        let mut unrecorded = Vec::new();
        let mut pending = vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                    continue;
                }
                if path.extension().is_none_or(|ext| ext != "rs") {
                    continue;
                }
                let source = std::fs::read_to_string(&path).unwrap();
                let mut function = None;
                // The last line that mounts in this function with no record call after it.
                let mut awaiting = None;
                for (index, line) in without_tests(&source).lines().enumerate() {
                    if let Some(name) = function_name(line) {
                        unrecorded.extend(awaiting.take());
                        function = Some(name.to_string());
                    }
                    if line.trim_start().starts_with("//") {
                        continue;
                    }
                    if mounts_something(line) {
                        let name = function.clone().unwrap_or_else(|| {
                            panic!("a mount outside any function in {}", path.display())
                        });
                        unrecorded.extend(awaiting.take());
                        // This FUSE mount blocks on the thread start_fuse_mount starts for
                        // as long as the volume is mounted, so mount_fuse_volumes records
                        // the volume once it is ready (checked below).
                        if name != "mount_vsock_reconnectable" {
                            awaiting = Some(format!("{name} at {}:{}", path.display(), index + 1));
                        }
                        mounting.push(name);
                    } else if line.contains(".record(") {
                        awaiting = None;
                    }
                }
                unrecorded.extend(awaiting);
            }
        }
        assert!(
            unrecorded.is_empty(),
            "no .record( call follows these mounts in their function before the next mount, \
             so the check after the last mount cannot see them: {unrecorded:?}"
        );
        let fuse = include_str!("mounts.rs");
        let fuse = &fuse[fuse
            .find("pub fn mount_fuse_volumes(")
            .expect("mounts.rs no longer has mount_fuse_volumes")..];
        let fuse = &fuse[..fuse.find("\n}\n").expect("mount_fuse_volumes has no end")];
        let ready = fuse
            .find("wait_for_fuse_mount(")
            .expect("mount_fuse_volumes no longer waits for each volume's mount");
        assert!(
            fuse[ready..].contains(".record("),
            "mount_fuse_volumes does not record a FUSE volume once its mount is ready"
        );
        mounting.sort();
        mounting.dedup();

        // Each function that mounts, and the call in agent.rs that reaches it.
        let reached_by = [
            ("mount_extra_disks", "mounts::mount_extra_disks("),
            ("mount_nfs_shares", "mounts::mount_nfs_shares("),
            ("mount_overlay_image", "container::mount_overlay_image("),
            // mount_pmem_device runs inside mount_pmem_devices.
            ("mount_pmem_device", "mounts::mount_pmem_devices("),
            // In fuse/mod.rs, run on the thread start_fuse_mount starts for each volume.
            ("mount_vsock_reconnectable", "mounts::mount_fuse_volumes("),
            (
                "setup_btrfs_storage_if_available",
                "container::setup_btrfs_storage_if_available(",
            ),
        ];
        assert_eq!(
            mounting,
            reached_by.map(|(function, _)| function),
            "the functions that mount changed: list each with the agent.rs call that reaches it"
        );

        let body = without_tests(include_str!("agent.rs"));
        let check = body
            .find(".check_none_covered()")
            .expect("fc-agent no longer checks that no mount covers an earlier one");
        for (_, call) in reached_by {
            let calls: Vec<usize> = body.match_indices(call).map(|(at, _)| at).collect();
            assert!(!calls.is_empty(), "agent.rs does not call {call}");
            for at in calls {
                assert!(
                    arguments(&body[at + call.len()..]).contains("&mut mount_record"),
                    "agent.rs calls {call} without the mount record, so the check after the \
                     last mount cannot see what it mounts"
                );
                assert!(
                    at < check,
                    "agent.rs calls {call} after it checks that no mount covers an earlier \
                     one, so that mount can cover one unseen"
                );
            }
        }

        let handshake = body
            .find("container::notify_cache_ready_and_wait(")
            .expect("fc-agent no longer sends the cache-ready handshake");
        assert!(
            check < handshake,
            "fc-agent checks the mounts after the cache-ready handshake, so the pre-start \
             snapshot can capture a covered mount"
        );
    }
}
