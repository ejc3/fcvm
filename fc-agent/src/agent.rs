use anyhow::Result;
use tokio::time::{sleep, Duration};

use crate::{container, exec, lock_test, mmds, mounts, network, output, system};

/// Main agent logic — fetches plan, runs container, triggers shutdown.
pub async fn run() -> Result<()> {
    eprintln!("[fc-agent] run_agent starting");

    system::raise_resource_limits();
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

    if let Err(e) = mmds::sync_clock_from_host().await {
        eprintln!("[fc-agent] WARNING: clock sync failed: {:?}", e);
        eprintln!("[fc-agent] continuing anyway (will rely on chronyd)");
    }

    // Create output channel — the writer task handles all vsock writes
    let (output, output_writer) = output::create();
    tokio::spawn(output_writer);

    // Start restore-epoch watcher
    let watcher_volumes = plan.volumes.clone();
    let watcher_output = output.clone();
    tokio::spawn(async move {
        eprintln!("[fc-agent] starting restore-epoch watcher");
        mmds::watch_restore_epoch(watcher_volumes, watcher_output).await;
    });

    // Start exec server
    let (exec_ready_tx, exec_ready_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async {
        exec::run_server(exec_ready_tx).await;
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

    // Prepare image (import archive or pull from registry)
    let image_ref = if let Some(archive_path) = &plan.image_archive {
        container::import_image(archive_path, &plan.image, &output).await?
    } else {
        container::pull_image(&plan).await?
    };

    // Notify host for cache snapshot
    match container::get_image_digest(&image_ref).await {
        Ok(digest) => {
            eprintln!("[fc-agent] image digest: {}", digest);
            if container::notify_cache_ready_and_wait(&digest) {
                eprintln!("[fc-agent] cache ready notification acknowledged");
            } else {
                eprintln!("[fc-agent] WARNING: cache-ready handshake failed, continuing");
            }
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: failed to get image digest: {:?}", e);
        }
    }

    // After cache-ready handshake, Firecracker may have created a pre-start snapshot.
    // Snapshot creation resets all vsock connections (VIRTIO_VSOCK_EVENT_TRANSPORT_RESET),
    // which breaks FUSE mounts and the output vsock. Reconnect the output vsock and
    // check if FUSE mounts are still healthy.
    output.reconnect();

    // Check FUSE health after potential snapshot
    mounts::check_and_remount_fuse(&plan.volumes, &mounted_fuse_paths).await;

    eprintln!("[fc-agent] launching container: {}", image_ref);
    system::wait_for_cgroup_controllers().await;

    // Build podman args
    let podman_args = container::build_podman_args(&plan, &image_ref);

    // TTY mode: blocks, never returns
    if plan.tty {
        eprintln!("[fc-agent] TTY mode enabled, using PTY");
        container::run_tty(&podman_args, &plan, &mounted_fuse_paths);
    }

    // Non-TTY mode: async
    let exit_code = container::run_async(&podman_args, &output).await?;

    // Notify host of exit
    crate::vsock::notify_container_exit(exit_code);

    // Cleanup
    mounts::unmount_paths(&mounted_fuse_paths, "FUSE volume");
    if !mounted_fuse_paths.is_empty() {
        sleep(Duration::from_millis(100)).await;
    }
    mounts::unmount_disks(&mounted_disk_paths);

    // Shutdown output writer
    output.shutdown().await;

    system::shutdown_vm(exit_code).await
}
