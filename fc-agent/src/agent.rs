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

    // Start chronyd for ongoing NTP time sync — SPAWNED, not awaited.
    //
    // Nothing later in boot depends on chronyd: the clock is already set from the
    // host's time in the boot plan, and chronyd only disciplines it from there on.
    // Awaiting it here put its whole startup (kill the systemd-launched instance,
    // wait for the command socket, configure servers) on the critical path of every
    // cold boot for no benefit.
    {
        let ntp_servers = plan.ntp_servers.clone();
        tokio::spawn(async move {
            start_chronyd(ntp_servers).await;
        });
    }

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

    // Shutdown output writer
    output.shutdown().await;

    system::shutdown_vm(exit_code).await
}

/// Path chronyd is started with, so the config written here is the one in effect.
///
/// Without an explicit `-f`, chronyd reads its compiled-in default
/// (`/etc/chrony/chrony.conf` on Ubuntu) — the copy baked into the rootfs, whose
/// `makestep 1.0 3` stops stepping the clock after the third update. A restored
/// snapshot can be hours behind, so the policy below has to be the live one.
const CHRONY_CONF: &str = "/etc/chrony.conf";

/// Bound on waiting for chronyd's command socket to answer. Generous: it is a
/// deadline for a checked retry, not a delay anyone pays on the happy path.
const CHRONYD_SOCKET_TIMEOUT: Duration = Duration::from_secs(15);

/// Bound on waiting for a previous chronyd to exit after being signalled.
const CHRONYD_EXIT_TIMEOUT_MS: u16 = 5_000;

/// Start chronyd for NTP time sync.
///
/// `ntp_servers` are the host's servers, already resolved to addresses by fcvm and
/// delivered in the boot plan (a guest cannot read the host's chrony.conf — nothing
/// mounts the host's /etc into a VM).
///
/// Every step here reports what actually happened. The previous version discarded
/// the result of each `chronyc add server` after a fixed 1s sleep, so a slow command
/// socket produced a VM with ZERO NTP sources and no message saying so — worst
/// exactly after a snapshot restore, where chrony is what corrects the clock.
async fn start_chronyd(ntp_servers: Vec<String>) {
    // Servers are added with chronyc rather than written as config lines because
    // chronyd's config parser does not handle bare IPv6 addresses.
    let config = format!(
        "makestep 1 -1\ndriftfile /var/lib/chrony/drift\ncmdallow 127.0.0.1\n{}",
        // Fallback only when the host gave us nothing: a guest with no source at all
        // would drift silently. A pool needs guest DNS, which is why it is the
        // fallback and not the norm.
        if ntp_servers.is_empty() {
            "pool pool.ntp.org iburst\n"
        } else {
            ""
        }
    );

    let _ = tokio::fs::create_dir_all("/var/lib/chrony").await;
    let _ = tokio::fs::create_dir_all("/var/run/chrony").await;
    if let Err(e) = tokio::fs::write(CHRONY_CONF, &config).await {
        eprintln!("[fc-agent] ERROR: cannot write {CHRONY_CONF}: {e}; no NTP time sync");
        return;
    }

    kill_existing_chronyd().await;

    match tokio::process::Command::new("/usr/sbin/chronyd")
        .args(["-u", "root", "-f", CHRONY_CONF])
        .output()
        .await
    {
        Ok(out) if out.status.success() => {
            eprintln!("[fc-agent] chronyd started (NTP time sync)");
        }
        Ok(out) => {
            eprintln!(
                "[fc-agent] ERROR: chronyd failed to start ({}): {}; no NTP time sync",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
            return;
        }
        Err(e) => {
            eprintln!("[fc-agent] ERROR: failed to start chronyd: {e}; no NTP time sync");
            return;
        }
    }

    // Wait for the command socket by RETRYING A REAL COMMAND until it answers, then
    // add each server and CHECK the result. Both are proof, where the old code had
    // a 1s guess and threw the answers away.
    if !wait_for_chronyd_socket().await {
        eprintln!(
            "[fc-agent] ERROR: chronyd command socket did not answer within {:?}; \
             {} NTP server(s) NOT configured",
            CHRONYD_SOCKET_TIMEOUT,
            ntp_servers.len()
        );
        return;
    }

    let mut configured = 0usize;
    for addr in &ntp_servers {
        match chronyc(&["add", "server", addr, "iburst"]).await {
            Ok(_) => configured += 1,
            Err(e) => eprintln!("[fc-agent] WARNING: chronyc add server {addr} failed: {e}"),
        }
    }

    // Assert the positive: report what chronyd actually has, not what we asked for.
    // `-c` is CSV, one line per source.
    let sources = chronyc(&["-c", "-n", "sources"])
        .await
        .map(|out| out.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0);
    if sources == 0 {
        eprintln!(
            "[fc-agent] ERROR: chronyd has NO time sources after configuring \
             {configured}/{} server(s) — the guest clock will drift",
            ntp_servers.len()
        );
    } else {
        eprintln!(
            "[fc-agent] NTP: {configured}/{} host server(s) configured, {sources} source(s) live",
            ntp_servers.len()
        );
    }
}

/// Run `chronyc`, returning stdout on success and the daemon's complaint on failure.
async fn chronyc(args: &[&str]) -> Result<String, String> {
    let out = tokio::process::Command::new("/usr/bin/chronyc")
        .args(args)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        Err(format!(
            "{}: {}",
            out.status,
            if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            }
        ))
    }
}

/// Wait until chronyd's command socket answers, by asking it something real.
///
/// chronyc fails with "506 Cannot talk to daemon" until chronyd has created
/// /var/run/chrony/chronyd.sock. Retrying the actual command means the wait ends on
/// proof that the daemon is answering — a fixed sleep ends on a guess, and when the
/// guess was short every later `chronyc add server` failed silently.
async fn wait_for_chronyd_socket() -> bool {
    let deadline = std::time::Instant::now() + CHRONYD_SOCKET_TIMEOUT;
    let mut backoff = Duration::from_millis(2);
    loop {
        if chronyc(&["-n", "tracking"]).await.is_ok() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_millis(50));
    }
}

/// Kill any chronyd already running and WAIT until it is really gone.
///
/// systemd starts chronyd as the `_chrony` user, which cannot send UDP in this VM,
/// so it is replaced by one running as root. The old process holds
/// /var/run/chrony/chronyd.sock, so the replacement must not start until it exits.
///
/// The wait is a pidfd readability event — the kernel's own "this process exited"
/// notification — replacing a 500ms sleep that was simultaneously too long (paid on
/// every cold boot) and unproven (nothing checked the process was gone). Signals go
/// through the pidfd too, so a PID reused between the scan and the kill cannot be
/// signalled by mistake.
async fn kill_existing_chronyd() {
    let pids = chronyd_pids().await;
    if pids.is_empty() {
        return;
    }
    for pid in pids {
        let gone = tokio::task::spawn_blocking(move || signal_and_await_exit(pid))
            .await
            .unwrap_or(false);
        if gone {
            eprintln!("[fc-agent] replaced existing chronyd (pid {pid})");
        } else {
            eprintln!(
                "[fc-agent] WARNING: chronyd pid {pid} did not exit; \
                 the replacement may fail to bind its command socket"
            );
        }
    }
}

/// PIDs of every running `chronyd`, from /proc (no pgrep/pkill subprocess needed).
async fn chronyd_pids() -> Vec<i32> {
    let mut pids = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir("/proc").await else {
        return pids;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        if let Ok(comm) = tokio::fs::read_to_string(format!("/proc/{pid}/comm")).await {
            if comm.trim() == "chronyd" {
                pids.push(pid);
            }
        }
    }
    pids
}

/// SIGTERM `pid` through a pidfd and block until the kernel reports it exited,
/// escalating to SIGKILL if it outstays the bound. Returns whether it is gone.
fn signal_and_await_exit(pid: i32) -> bool {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::os::fd::{AsFd, FromRawFd, OwnedFd};

    // SAFETY: pidfd_open is a plain syscall; the returned fd is immediately wrapped
    // in OwnedFd so it is closed exactly once.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if raw < 0 {
        // ESRCH: it exited between the scan and here — which is the outcome we want.
        return nix::errno::Errno::last() == nix::errno::Errno::ESRCH;
    }
    let pidfd = unsafe { OwnedFd::from_raw_fd(raw as std::os::fd::RawFd) };

    let send = |sig: libc::c_int| {
        // SAFETY: pidfd_send_signal targets the process the pidfd refers to, so a
        // recycled PID can never be signalled by accident.
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                raw,
                sig,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        }
    };
    // A pidfd becomes readable when its process exits.
    let exited = |timeout_ms: u16| {
        let mut fds = [PollFd::new(pidfd.as_fd(), PollFlags::POLLIN)];
        matches!(poll(&mut fds, PollTimeout::from(timeout_ms)), Ok(n) if n > 0)
    };

    if send(libc::SIGTERM) < 0 {
        return nix::errno::Errno::last() == nix::errno::Errno::ESRCH;
    }
    if exited(CHRONYD_EXIT_TIMEOUT_MS) {
        return true;
    }
    let _ = send(libc::SIGKILL);
    exited(CHRONYD_EXIT_TIMEOUT_MS)
}
