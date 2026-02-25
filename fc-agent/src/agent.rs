use anyhow::Result;
use tokio::time::{sleep, Duration};

use crate::{container, exec, lock_test, mmds, mounts, network, output, proxy, system};

/// Main agent logic — fetches plan, runs container, triggers shutdown.
pub async fn run() -> Result<()> {
    eprintln!("[fc-agent] run_agent starting");

    system::raise_resource_limits();
    system::raise_cgroup_pids_limit();
    system::create_kvm_device();
    network::configure_dns_from_cmdline();
    network::configure_ipv6_from_cmdline();

    // Fetch plan from MMDS with retry
    let plan = loop {
        match mmds::fetch_plan().await {
            Ok(p) => {
                eprintln!("[fc-agent] received container plan successfully");
                break p;
            }
            Err(e) => {
                eprintln!("[fc-agent] MMDS not ready: {:?}", e);
                eprintln!("[fc-agent] retrying in 500ms...");
                sleep(Duration::from_millis(500)).await;
            }
        }
    };

    system::save_proxy_settings(&plan);

    if !plan.forward_localhost.is_empty() {
        network::setup_localhost_forwarding(&plan.forward_localhost);
    }

    // Egress proxy reconnect signal — signaled after snapshot events (pause/resume
    // or restore) to break the stale vsock session and reconnect immediately.
    let egress_reconnect = std::sync::Arc::new(tokio::sync::Notify::new());

    if plan.egress_proxy {
        eprintln!("[fc-agent] starting vsock egress proxy");
        let signal = egress_reconnect.clone();
        tokio::spawn(proxy::run_egress_proxy(signal));
    }

    if let Err(e) = mmds::sync_clock_from_host().await {
        eprintln!("[fc-agent] WARNING: clock sync failed: {:?}", e);
        eprintln!("[fc-agent] continuing anyway (will rely on chronyd)");
    }

    // Create output channel — the writer task handles all vsock writes
    let (output, output_writer) = output::create();
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

    // Start restore-epoch watcher
    let watcher_output = output.clone();
    let watcher_restore_flag = restore_flag.clone();
    let watcher_exec_rebind = exec_rebind.clone();
    let watcher_exec_rebind_needed = exec_rebind_needed.clone();
    let watcher_exec_rebind_done = exec_rebind_done.clone();
    let watcher_exec_rebind_done_notify = exec_rebind_done_notify.clone();
    let watcher_egress_reconnect = egress_reconnect.clone();
    tokio::spawn(async move {
        eprintln!("[fc-agent] starting restore-epoch watcher");
        mmds::watch_restore_epoch(
            watcher_output,
            watcher_restore_flag,
            watcher_exec_rebind,
            watcher_exec_rebind_needed,
            watcher_exec_rebind_done,
            watcher_exec_rebind_done_notify,
            watcher_egress_reconnect,
        )
        .await;
    });

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

    // Mount filesystems
    let mounted_fuse_paths = if !plan.volumes.is_empty() {
        eprintln!("[fc-agent] mounting {} FUSE volume(s)", plan.volumes.len());
        match mounts::mount_fuse_volumes(&plan.volumes) {
            Ok(paths) => {
                eprintln!("[fc-agent] FUSE volumes mounted successfully");
                paths
            }
            Err(e) => {
                eprintln!("[fc-agent] ERROR: failed to mount FUSE volumes: {:?}", e);
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let has_shared_volume = mounted_fuse_paths.iter().any(|p| p == "/mnt/shared");

    let mounted_disk_paths = if !plan.extra_disks.is_empty() {
        eprintln!(
            "[fc-agent] mounting {} extra disk(s)",
            plan.extra_disks.len()
        );
        match mounts::mount_extra_disks(&plan.extra_disks) {
            Ok(paths) => {
                eprintln!("[fc-agent] extra disks mounted successfully");
                paths
            }
            Err(e) => {
                eprintln!("[fc-agent] ERROR: failed to mount extra disks: {:?}", e);
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    if !plan.nfs_mounts.is_empty() {
        eprintln!("[fc-agent] mounting {} NFS share(s)", plan.nfs_mounts.len());
        match mounts::mount_nfs_shares(&plan.nfs_mounts) {
            Ok(_) => eprintln!("[fc-agent] NFS shares mounted successfully"),
            Err(e) => eprintln!("[fc-agent] ERROR: failed to mount NFS shares: {:?}", e),
        }
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

    // Set up btrfs storage if kernel supports it (avoids overlay idmap issues).
    // Skip for overlay mode — it manages its own storage.
    // For btrfs/archive/pull: creates loopback btrfs if kernel supports it.
    match plan.image_mode.as_deref() {
        Some("overlay") => {
            eprintln!("[fc-agent] skipping btrfs loopback setup (image_mode=overlay)");
        }
        _ => {
            // Btrfs, archive, and pull modes all use btrfs loopback on rootfs.
            // The btrfs kernel module must be available (CONFIG_BTRFS_FS=y in btrfs profile).
            container::setup_btrfs_storage_if_available();
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
                container::create_vm_user(user_spec, desired_name, subuid_range);
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
    if cmd_prefix.is_empty() {
        container::reset_podman_state();
    }

    // Prepare image based on delivery mode
    let image_ref = match (plan.image_mode.as_deref(), &plan.image_device) {
        (Some("overlay"), Some(device)) => {
            let username = user_info.as_ref().map(|(name, _)| name.as_str());
            container::mount_overlay_image(device, &plan.image, username)?
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

    // Notify host for cache snapshot
    match container::get_image_digest(&image_ref, &cmd_prefix).await {
        Ok(digest) => {
            eprintln!("[fc-agent] image digest: {}", digest);
            if container::notify_cache_ready_and_wait(&digest, &restore_flag) {
                eprintln!("[fc-agent] cache ready notification acknowledged");
                // Reconnect output vsock before starting the container.
                //
                // On COLD start (first run), pause/resume does NOT reset vsock —
                // this reconnect is harmless (just cycles the connection).
                //
                // On WARM start (restored from cached pre-start snapshot), vsock
                // IS dead (VIRTIO_VSOCK_EVENT_TRANSPORT_RESET on restore). The
                // restore-epoch watcher calls handle_clone_restore() which also
                // reconnects output — but there's a race: restore_flag is set
                // BEFORE handle_clone_restore completes, so notify_cache_ready_and_wait
                // returns before output.reconnect() has been called. For fast-exit
                // containers (echo + exit in ~200ms), the container runs and exits
                // with output going to the dead vsock before handle_clone_restore
                // finishes. This explicit reconnect ensures the output writer has
                // a live connection before the container starts.
                output.reconnect();
                // Signal egress proxy to reconnect its vsock. On warm start (restored
                // from cached pre-start snapshot), VIRTIO_VSOCK_EVENT_TRANSPORT_RESET
                // killed the proxy's vsock. handle_clone_restore() also signals this,
                // but there's a race: restore_flag is set before handle_clone_restore
                // completes. This explicit signal ensures the proxy reconnects before
                // the container starts making TCP connections. Harmless on cold start
                // (pause/resume doesn't break connections — proxy just cycles).
                egress_reconnect.notify_waiters();
            } else {
                eprintln!("[fc-agent] WARNING: cache-ready handshake failed, continuing");
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
    ] {
        let _ = std::process::Command::new("sysctl")
            .args(["-w", sysctl])
            .output();
    }

    // Add host identity IPv6 to loopback and eth0 (requires root, can't do from
    // rootless container). Pass the address via HOST_IPV6 env var in the Plan.
    if let Some(ipv6) = plan.env.get("HOST_IPV6") {
        if !ipv6.is_empty() {
            for dev in &["lo", "eth0"] {
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

    eprintln!("[fc-agent] launching container: {}", image_ref);
    system::wait_for_cgroup_controllers().await;

    // Build podman args (pass user info if available for rootless setup)
    let user_ref = user_info
        .as_ref()
        .map(|(username, runtime_dir)| (username.as_str(), runtime_dir.as_str()));
    let podman_args = container::build_podman_args(&plan, &image_ref, user_ref);

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
