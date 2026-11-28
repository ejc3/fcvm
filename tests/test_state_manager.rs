use fcvm::network::NetworkConfig;
use fcvm::state::{HealthStatus, ProcessType, StateManager, VmConfig, VmState, VmStatus};
use jiff::Timestamp;
use tempfile::TempDir;

#[tokio::test]
async fn test_state_persistence() {
    let temp_dir = TempDir::new().unwrap();
    let manager = StateManager::new(temp_dir.path().to_path_buf());

    // Initialize state directory
    manager.init().await.unwrap();

    let now = Timestamp::now();

    // Create and save a VM state
    let state = VmState {
        schema_version: 1,
        vm_id: "test-vm-1".to_string(),
        name: Some("test-vm".to_string()),
        status: VmStatus::Running,
        health_status: HealthStatus::Healthy,
        exit_code: None,
        pid: Some(12345),
        pid_start_time: None,
        lifecycle_ready: false,
        restore_acknowledged: false,
        holder_pid: None,
        netpool: None,
        confinement: None,
        vsock_epoch: 0,
        network_gate_closed: false,
        boundary_token: None,
        created_at: now,
        last_updated: now,
        config: VmConfig {
            vm_runtime_dir: None,
            image: "nginx:alpine".to_string(),
            vcpu: 2,
            memory_mib: 512,
            network: NetworkConfig {
                tap_device: "tap0".to_string(),
                tap_fd: None,
                guest_mac: "02:00:00:00:00:01".to_string(),
                guest_ip: Some("172.16.0.2".to_string()),
                host_ip: Some("172.16.0.1".to_string()),
                host_veth: Some("veth0".to_string()),
                loopback_ip: None,
                dns_server: None,
                guest_ipv6: None,
                host_ipv6: None,
                guest_ipv6_local_only: false,
                dns_search: None,
                http_proxy: None,
                https_proxy: None,
                namespace_name: None,
                guest_mtu: None,
            },
            volumes: vec![],
            extra_disks: vec![],
            pmem_devices: vec![],
            nfs_shares: vec![],
            health_check_url: None,
            snapshot_name: None,
            process_type: Some(ProcessType::Vm),
            serve_pid: None,
            uffd_mode: None,
            original_vsock_vm_id: None,
            vsock_socket_path: None,
            source_vsock_socket_path: None,
            port_mappings: vec![],
            published_guest_ports: None,
            forward_localhost: vec![],
            network_mode: Default::default(),
            ipv6_prefix: None,
            kernel_profile: None,
            image_mode: None,
            image_disk_path: None,
            image_disk_identity: None,
            tty: false,
            interactive: false,
            labels: std::collections::HashMap::new(),
            hugepages: false,
            portable_volumes: false,
            user: None,
            username: None,
            health_check_timeout: 5,
            restore_hook: None,
            guest_thp: None,
            guest_thp_restore: false,
            dns_override: None,
            hypervisor: Default::default(),
            firecracker_bin: None,
            balloon_mib: None,
            balloon_free_page_reporting: false,
            gpus: Vec::new(),
            container_start_version: None,
            guest: Default::default(),
            firmware: None,
            health_port: None,
            rdp_socket: None,
        },
    };

    // Save state
    manager.save_state(&state).await.unwrap();

    // Load state back
    let loaded = manager.load_state("test-vm-1").await.unwrap();
    assert_eq!(loaded.vm_id, state.vm_id);
    assert_eq!(loaded.pid, state.pid);
    // Note: VmStatus doesn't derive PartialEq, so we can't compare directly
    assert!(matches!(loaded.status, VmStatus::Running));

    // Verify file permissions are world-readable (Unix only)
    // State files use 0o644 so non-root users can list VMs
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let state_file = manager.state_file("test-vm-1");
        let metadata = std::fs::metadata(&state_file).unwrap();
        let permissions = metadata.permissions();
        // Check permissions are world-readable (0o644) so non-root can list VMs
        assert_eq!(permissions.mode() & 0o777, 0o644);
    }

    // Delete state
    manager.delete_state("test-vm-1").await.unwrap();

    // Verify deletion
    assert!(manager.load_state("test-vm-1").await.is_err());
}

#[tokio::test]
async fn test_list_vms() {
    let temp_dir = TempDir::new().unwrap();
    let manager = StateManager::new(temp_dir.path().to_path_buf());
    manager.init().await.unwrap();

    let now = Timestamp::now();

    // Save multiple VMs
    for i in 1..=3 {
        let state = VmState {
            schema_version: 1,
            vm_id: format!("vm-{}", i),
            name: Some(format!("test-vm-{}", i)),
            status: VmStatus::Running,
            health_status: HealthStatus::Healthy,
            exit_code: None,
            pid: Some(10000 + i),
            pid_start_time: None,
            lifecycle_ready: false,
            restore_acknowledged: false,
            holder_pid: None,
            netpool: None,
            confinement: None,
            vsock_epoch: 0,
            network_gate_closed: false,
            boundary_token: None,
            created_at: now,
            last_updated: now,
            config: VmConfig {
                vm_runtime_dir: None,
                image: "nginx:alpine".to_string(),
                vcpu: 1,
                memory_mib: 256,
                network: NetworkConfig::default(),
                volumes: vec![],
                extra_disks: vec![],
                pmem_devices: vec![],
                nfs_shares: vec![],
                health_check_url: None,
                snapshot_name: None,
                process_type: Some(ProcessType::Vm),
                serve_pid: None,
                uffd_mode: None,
                original_vsock_vm_id: None,
                vsock_socket_path: None,
                source_vsock_socket_path: None,
                port_mappings: vec![],
                published_guest_ports: None,
                forward_localhost: vec![],
                network_mode: Default::default(),
                ipv6_prefix: None,
                kernel_profile: None,
                image_mode: None,
                image_disk_path: None,
                image_disk_identity: None,
                tty: false,
                interactive: false,
                labels: std::collections::HashMap::new(),
                hugepages: false,
                portable_volumes: false,
                user: None,
                username: None,
                health_check_timeout: 5,
                restore_hook: None,
                guest_thp: None,
                guest_thp_restore: false,
                dns_override: None,
                hypervisor: Default::default(),
                firecracker_bin: None,
                balloon_mib: None,
                balloon_free_page_reporting: false,
                gpus: Vec::new(),
                container_start_version: None,
                guest: Default::default(),
                firmware: None,
                health_port: None,
                rdp_socket: None,
            },
        };
        manager.save_state(&state).await.unwrap();
    }

    // List VMs
    let vms = manager.list_vms().await.unwrap();
    assert_eq!(vms.len(), 3);

    // Verify all VMs are present
    let vm_ids: Vec<String> = vms.iter().map(|vm| vm.vm_id.clone()).collect();
    assert!(vm_ids.contains(&"vm-1".to_string()));
    assert!(vm_ids.contains(&"vm-2".to_string()));
    assert!(vm_ids.contains(&"vm-3".to_string()));
}

#[tokio::test]
async fn test_load_state_by_name_duplicate_detection() {
    let temp_dir = TempDir::new().unwrap();
    let manager = StateManager::new(temp_dir.path().to_path_buf());
    manager.init().await.unwrap();

    let now = Timestamp::now();

    // Save two VMs with the same name but different vm_ids and PIDs. The PIDs are above
    // the kernel's limit for one (2^22), so neither can be a live process: with 5000 and
    // 5001 the test failed on a host where one of them was.
    const DEAD_PIDS: [u32; 2] = [2_000_000_001, 2_000_000_002];
    for (i, pid) in [(1u32, DEAD_PIDS[0]), (2, DEAD_PIDS[1])] {
        let state = VmState {
            schema_version: 1,
            vm_id: format!("vm-dup-{}", i),
            name: Some("duplicate-name".to_string()),
            status: VmStatus::Running,
            health_status: HealthStatus::Healthy,
            exit_code: None,
            pid: Some(pid),
            pid_start_time: None,
            lifecycle_ready: false,
            restore_acknowledged: false,
            holder_pid: None,
            netpool: None,
            confinement: None,
            vsock_epoch: 0,
            network_gate_closed: false,
            boundary_token: None,
            created_at: now,
            last_updated: now,
            config: VmConfig {
                vm_runtime_dir: None,
                image: "nginx:alpine".to_string(),
                vcpu: 1,
                memory_mib: 256,
                network: NetworkConfig::default(),
                volumes: vec![],
                extra_disks: vec![],
                pmem_devices: vec![],
                nfs_shares: vec![],
                health_check_url: None,
                snapshot_name: None,
                process_type: Some(ProcessType::Vm),
                serve_pid: None,
                uffd_mode: None,
                original_vsock_vm_id: None,
                vsock_socket_path: None,
                source_vsock_socket_path: None,
                port_mappings: vec![],
                published_guest_ports: None,
                forward_localhost: vec![],
                network_mode: Default::default(),
                ipv6_prefix: None,
                kernel_profile: None,
                image_mode: None,
                image_disk_path: None,
                image_disk_identity: None,
                tty: false,
                interactive: false,
                labels: std::collections::HashMap::new(),
                hugepages: false,
                portable_volumes: false,
                user: None,
                username: None,
                health_check_timeout: 5,
                restore_hook: None,
                guest_thp: None,
                guest_thp_restore: false,
                dns_override: None,
                hypervisor: Default::default(),
                firecracker_bin: None,
                balloon_mib: None,
                balloon_free_page_reporting: false,
                gpus: Vec::new(),
                container_start_version: None,
                guest: Default::default(),
                firmware: None,
                health_port: None,
                rdp_socket: None,
            },
        };
        manager.save_state(&state).await.unwrap();
    }

    // Looking up the duplicate name should error with both PIDs listed
    let err = manager
        .load_state_by_name("duplicate-name")
        .await
        .expect_err("should fail with duplicate names");
    let msg = err.to_string();
    assert!(
        msg.contains("Multiple VMs"),
        "error should mention multiple VMs: {}",
        msg
    );
    for pid in DEAD_PIDS {
        assert!(
            msg.contains(&pid.to_string()),
            "error should list PID {pid}: {msg}"
        );
    }

    // Looking up a unique name should still work
    let state = VmState {
        schema_version: 1,
        vm_id: "vm-unique".to_string(),
        name: Some("unique-name".to_string()),
        status: VmStatus::Running,
        health_status: HealthStatus::Healthy,
        exit_code: None,
        pid: Some(6000),
        pid_start_time: None,
        lifecycle_ready: false,
        restore_acknowledged: false,
        holder_pid: None,
        netpool: None,
        confinement: None,
        vsock_epoch: 0,
        network_gate_closed: false,
        boundary_token: None,
        created_at: now,
        last_updated: now,
        config: VmConfig {
            vm_runtime_dir: None,
            image: "nginx:alpine".to_string(),
            vcpu: 1,
            memory_mib: 256,
            network: NetworkConfig::default(),
            volumes: vec![],
            extra_disks: vec![],
            pmem_devices: vec![],
            nfs_shares: vec![],
            health_check_url: None,
            snapshot_name: None,
            process_type: Some(ProcessType::Vm),
            serve_pid: None,
            uffd_mode: None,
            original_vsock_vm_id: None,
            vsock_socket_path: None,
            source_vsock_socket_path: None,
            port_mappings: vec![],
            published_guest_ports: None,
            forward_localhost: vec![],
            network_mode: Default::default(),
            ipv6_prefix: None,
            kernel_profile: None,
            image_mode: None,
            image_disk_path: None,
            image_disk_identity: None,
            tty: false,
            interactive: false,
            labels: std::collections::HashMap::new(),
            hugepages: false,
            portable_volumes: false,
            user: None,
            username: None,
            health_check_timeout: 5,
            restore_hook: None,
            guest_thp: None,
            guest_thp_restore: false,
            dns_override: None,
            hypervisor: Default::default(),
            firecracker_bin: None,
            balloon_mib: None,
            balloon_free_page_reporting: false,
            gpus: Vec::new(),
            container_start_version: None,
            guest: Default::default(),
            firmware: None,
            health_port: None,
            rdp_socket: None,
        },
    };
    manager.save_state(&state).await.unwrap();
    let found = manager.load_state_by_name("unique-name").await.unwrap();
    assert_eq!(found.pid, Some(6000));
}

/// Helper to create a minimal VmState with given vm_id and pid
/// `list_vms` finds a VM whose state file is replaced while the directory is being
/// scanned. Every update writes a new state file and renames it over the old one, and the
/// health monitor updates after every check. On btrfs, since Linux 6.5, a directory scan
/// stops at the last directory index that existed when it was opened, and the rename gives
/// the file a new, higher index, so a scan that opened before the rename sees neither name.
/// `fcvm ls --pid`, `load_state_by_pid`, loopback address allocation and a serve process's
/// cleanup all read state through `list_vms`; a missed VM made `fcvm exec --pid` fail, and
/// the chaos fuzz's StateRead report "found no state" for a VM that was running.
///
/// The rename runs inside the scan's window through `list_vms_after_open`, so the
/// interleaving is the same on every run. RED BEFORE THE FIX: the VM was missing from the
/// listing every time.
#[tokio::test]
async fn list_vms_finds_a_vm_whose_state_file_is_replaced_during_the_scan() {
    // The miss needs btrfs, which is what fcvm's data directory is; anywhere else this
    // test could not fail. Nothing else need have created that directory yet (a container
    // unit-test run has not), so make it.
    let data_dir = fcvm::paths::data_dir();
    std::fs::create_dir_all(&data_dir).unwrap();
    let temp_dir = TempDir::new_in(&data_dir).unwrap();
    assert_eq!(
        nix::sys::statfs::statfs(temp_dir.path())
            .unwrap()
            .filesystem_type(),
        nix::sys::statfs::BTRFS_SUPER_MAGIC,
        "{} is not on btrfs",
        temp_dir.path().display()
    );
    let manager = StateManager::new(temp_dir.path().to_path_buf());
    manager.init().await.unwrap();
    for i in 0..70 {
        let other = make_vm_state(&format!("vm-other-{i}"), &format!("other-{i}"), 10_000 + i);
        manager.save_state(&other).await.unwrap();
    }
    manager
        .save_state(&make_vm_state("vm-target", "target", 9_999))
        .await
        .unwrap();

    // What update_state does, done by hand inside the scan's window: write the new state
    // file and rename it over the old one.
    let state_file = manager.state_file("vm-target");
    let temp_file = state_file.with_extension("json.tmp");
    let replaced = std::fs::read_to_string(&state_file).unwrap();
    let vms = manager
        .list_vms_after_open(move || {
            std::fs::write(&temp_file, &replaced).unwrap();
            std::fs::rename(&temp_file, &state_file).unwrap();
        })
        .await
        .unwrap();

    assert!(
        vms.iter().any(|vm| vm.vm_id == "vm-target"),
        "list_vms missed the VM whose state file was replaced during the scan; it listed {} VMs",
        vms.len()
    );
    assert_eq!(vms.len(), 71, "every VM is listed exactly once");
}

/// Where `manager` keeps VM `vm_id`'s state file, with its bucket made, for a test that
/// writes the file itself.
fn state_path_of(manager: &StateManager, vm_id: &str) -> std::path::PathBuf {
    let path = manager.state_file(vm_id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    path
}

/// Every directory under `root`, `root` included.
fn dirs_under(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    let mut next = 0;
    while next < dirs.len() {
        for entry in std::fs::read_dir(&dirs[next]).unwrap().flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                dirs.push(entry.path());
            }
        }
        next += 1;
    }
    dirs
}

fn make_vm_state(vm_id: &str, name: &str, pid: u32) -> VmState {
    VmState {
        schema_version: 1,
        vm_id: vm_id.to_string(),
        name: Some(name.to_string()),
        status: VmStatus::Running,
        health_status: HealthStatus::Healthy,
        exit_code: None,
        pid: Some(pid),
        pid_start_time: None,
        lifecycle_ready: false,
        restore_acknowledged: false,
        holder_pid: None,
        netpool: None,
        confinement: None,
        vsock_epoch: 0,
        network_gate_closed: false,
        boundary_token: None,
        created_at: Timestamp::now(),
        last_updated: Timestamp::now(),
        config: VmConfig {
            vm_runtime_dir: None,
            image: "test:latest".to_string(),
            vcpu: 1,
            memory_mib: 256,
            network: NetworkConfig::default(),
            volumes: vec![],
            extra_disks: vec![],
            pmem_devices: vec![],
            nfs_shares: vec![],
            health_check_url: None,
            snapshot_name: None,
            process_type: Some(ProcessType::Vm),
            serve_pid: None,
            uffd_mode: None,
            original_vsock_vm_id: None,
            vsock_socket_path: None,
            source_vsock_socket_path: None,
            port_mappings: vec![],
            published_guest_ports: None,
            forward_localhost: vec![],
            network_mode: Default::default(),
            ipv6_prefix: None,
            kernel_profile: None,
            image_mode: None,
            image_disk_path: None,
            image_disk_identity: None,
            tty: false,
            interactive: false,
            labels: std::collections::HashMap::new(),
            hugepages: false,
            portable_volumes: false,
            user: None,
            username: None,
            health_check_timeout: 5,
            restore_hook: None,
            guest_thp: None,
            guest_thp_restore: false,
            dns_override: None,
            hypervisor: Default::default(),
            firecracker_bin: None,
            balloon_mib: None,
            balloon_free_page_reporting: false,
            gpus: Vec::new(),
            container_start_version: None,
            guest: Default::default(),
            firmware: None,
            health_port: None,
            rdp_socket: None,
        },
    }
}

#[tokio::test]
async fn test_load_state_by_pid_found() {
    let temp_dir = TempDir::new().unwrap();
    let manager = StateManager::new(temp_dir.path().to_path_buf());
    manager.init().await.unwrap();

    // Use our own PID (guaranteed to exist in /proc)
    let my_pid = std::process::id();
    let state = make_vm_state("vm-pid-test", "pid-test", my_pid);
    manager.save_state(&state).await.unwrap();

    let found = manager.load_state_by_pid(my_pid).await.unwrap();
    assert_eq!(found.vm_id, "vm-pid-test");
    assert_eq!(found.pid, Some(my_pid));
}

#[tokio::test]
async fn test_load_state_by_pid_not_found() {
    let temp_dir = TempDir::new().unwrap();
    let manager = StateManager::new(temp_dir.path().to_path_buf());
    manager.init().await.unwrap();

    let my_pid = std::process::id();
    let state = make_vm_state("vm-other", "other", my_pid);
    manager.save_state(&state).await.unwrap();

    // Search for a PID that no VM has
    let err = manager
        .load_state_by_pid(99999999)
        .await
        .expect_err("should fail for unknown PID");
    assert!(
        err.to_string().contains("No VM found with PID"),
        "error should mention PID: {}",
        err
    );
}

#[tokio::test]
async fn test_load_state_by_pid_cleans_stale_on_retry() {
    let temp_dir = TempDir::new().unwrap();
    let manager = StateManager::new(temp_dir.path().to_path_buf());
    manager.init().await.unwrap();

    // Create a stale state file with a PID that doesn't exist.
    // Use a very high PID that's virtually guaranteed to not exist.
    let stale_pid = 4_000_000_000u32;
    let stale_state = make_vm_state("vm-stale", "stale", stale_pid);
    manager.save_state(&stale_state).await.unwrap();

    // Verify the stale file exists
    let vms = manager.list_vms().await.unwrap();
    assert_eq!(vms.len(), 1, "stale VM state should exist before cleanup");

    // load_state_by_pid for a non-existent PID triggers cleanup_stale_state
    let _ = manager.load_state_by_pid(99999998).await;

    // After the failed lookup, the stale state should have been cleaned up
    // (PID 4000000000 doesn't exist in /proc)
    let vms_after = manager.list_vms().await.unwrap();
    assert_eq!(
        vms_after.len(),
        0,
        "stale state file should be removed after cleanup"
    );
}

#[tokio::test]
async fn test_stale_sweep_skips_locked_writer_and_rechecks_identity() {
    use nix::fcntl::{Flock, FlockArg};

    let temp_dir = TempDir::new().unwrap();
    let manager = StateManager::new(temp_dir.path().to_path_buf());
    let state_path = state_path_of(&manager, "vm-sweep");
    let temp_path = state_path.with_extension("json.tmp");
    let lock_path = state_path.with_extension("json.lock");
    manager
        .save_state(&make_vm_state("vm-sweep", "sweep", u32::MAX))
        .await
        .unwrap();

    // Stop a writer between its temporary write and publication. The sweep
    // must return without waiting for this lock or unlinking either payload.
    let lock = Flock::lock(
        std::fs::File::open(&lock_path).unwrap(),
        FlockArg::LockExclusive,
    )
    .unwrap();
    std::fs::copy(&state_path, &temp_path).unwrap();
    assert!(manager.load_state_by_pid(u32::MAX - 1).await.is_err());
    assert!(state_path.exists(), "sweep deleted a locked writer's state");
    assert!(temp_path.exists(), "sweep deleted a locked writer's temp");
    assert!(lock_path.exists(), "sweep unlinked a held lock");

    // The skipped writer publishes a live identity. The next sweep must read
    // that identity afresh, not act on the stale record from its prior scan.
    let live = make_vm_state("vm-sweep", "sweep", std::process::id());
    std::fs::write(&temp_path, serde_json::to_vec(&live).unwrap()).unwrap();
    std::fs::rename(&temp_path, &state_path).unwrap();
    drop(lock);
    assert!(manager.load_state_by_pid(u32::MAX - 1).await.is_err());
    assert_eq!(manager.load_state("vm-sweep").await.unwrap().pid, live.pid);
}

#[tokio::test]
async fn test_stale_sweep_reaps_orphan_temps_but_preserves_live_or_unknown_state() {
    use nix::fcntl::{Flock, FlockArg};

    let temp_dir = TempDir::new().unwrap();
    let manager = StateManager::new(temp_dir.path().to_path_buf());
    for case in ["absent", "dead", "reused", "live", "unknown", "locked"] {
        let path = state_path_of(&manager, case);
        std::fs::write(path.with_extension("json.tmp"), b"interrupted write").unwrap();
        if matches!(case, "dead" | "reused" | "live") {
            let pid = if case == "dead" {
                u32::MAX
            } else {
                std::process::id()
            };
            let mut state = make_vm_state(case, case, pid);
            if case == "reused" {
                state.pid_start_time = Some(u64::MAX);
            }
            std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
        } else if case == "unknown" {
            std::fs::write(&path, b"unreadable state").unwrap();
        }
    }
    let locked_path = manager.lock_file("locked");
    let lock = Flock::lock(
        std::fs::File::create(&locked_path).unwrap(),
        FlockArg::LockExclusive,
    )
    .unwrap();
    assert!(manager.load_state_by_pid(u32::MAX - 1).await.is_err());
    for case in ["absent", "dead", "reused"] {
        assert!(
            !manager.state_file(case).with_extension("json.tmp").exists(),
            "sweep left {case} temp behind"
        );
        assert!(!manager.state_file(case).exists());
        assert!(!manager.lock_file(case).exists());
    }
    for case in ["live", "unknown", "locked"] {
        assert!(
            manager.state_file(case).with_extension("json.tmp").exists(),
            "sweep removed {case} temp"
        );
    }
    drop(lock);
    assert!(manager.load_state_by_pid(u32::MAX - 1).await.is_err());
    assert!(!manager
        .state_file("locked")
        .with_extension("json.tmp")
        .exists());
}

#[tokio::test]
async fn test_update_state_preserves_concurrent_updates() {
    let temp_dir = TempDir::new().unwrap();
    // Two manager instances over the same directory simulate two processes
    // (the VM owner's health monitor and an external `fcvm snapshot create`).
    let owner = StateManager::new(temp_dir.path().to_path_buf());
    let external = StateManager::new(temp_dir.path().to_path_buf());
    owner.init().await.unwrap();

    let my_pid = std::process::id();
    let state = make_vm_state("vm-update", "update-test", my_pid);
    owner.save_state(&state).await.unwrap();

    // The owner's health monitor records a stop with an exit code...
    owner
        .update_health_status("vm-update", HealthStatus::Stopped, Some(7))
        .await
        .unwrap();

    // ...then the external process records a snapshot name via a locked
    // read-modify-write. The health monitor's write must survive even though
    // the external process never saw it in memory.
    let updated = external
        .update_state("vm-update", |s| {
            s.config.snapshot_name = Some("snap-1".to_string());
        })
        .await
        .unwrap()
        .expect("state file should exist");
    assert_eq!(updated.config.snapshot_name.as_deref(), Some("snap-1"));

    let on_disk = owner.load_state("vm-update").await.unwrap();
    assert_eq!(on_disk.config.snapshot_name.as_deref(), Some("snap-1"));
    assert_eq!(on_disk.health_status, HealthStatus::Stopped);
    assert_eq!(on_disk.exit_code, Some(7));
}

#[tokio::test]
async fn test_delete_state_then_update_does_not_resurrect() {
    let temp_dir = TempDir::new().unwrap();
    // Two manager instances over the same directory simulate the cleanup path
    // and a health monitor task that hasn't stopped yet.
    let cleanup = StateManager::new(temp_dir.path().to_path_buf());
    let monitor = StateManager::new(temp_dir.path().to_path_buf());
    cleanup.init().await.unwrap();

    let my_pid = std::process::id();
    let state = make_vm_state("vm-deleted", "deleted", my_pid);
    cleanup.save_state(&state).await.unwrap();

    cleanup.delete_state("vm-deleted").await.unwrap();

    // A late health-monitor write after deletion must be a no-op, not a
    // recreation of the state file.
    let previous = monitor
        .update_health_status("vm-deleted", HealthStatus::Stopped, Some(0))
        .await
        .unwrap();
    assert_eq!(
        previous, None,
        "update on deleted VM should report no state"
    );

    assert!(
        !cleanup.state_file("vm-deleted").exists(),
        "state file must not be resurrected after delete_state"
    );
    assert!(cleanup.list_vms().await.unwrap().is_empty());

    // delete_state also removes its lock and temp files, and the no-op update
    // must not recreate them.
    assert!(!cleanup.lock_file("vm-deleted").exists());
    assert!(!cleanup
        .state_file("vm-deleted")
        .with_extension("json.tmp")
        .exists());
}

#[tokio::test]
async fn test_load_state_by_pid_rejects_pid_reuse() {
    let temp_dir = TempDir::new().unwrap();
    let manager = StateManager::new(temp_dir.path().to_path_buf());
    manager.init().await.unwrap();

    // Simulate a VM that crashed without cleanup and whose PID was later
    // reused by an unrelated process: the state file claims a live PID (ours)
    // but records a start time that does not match that process.
    let my_pid = std::process::id();
    let mut stale = make_vm_state("vm-reused-pid", "reused-pid", my_pid);
    stale.pid_start_time = Some(1); // bogus: a real start time is far larger
    let json = serde_json::to_string_pretty(&stale).unwrap();
    // Write the file directly — save_state would overwrite pid_start_time with
    // the real value for the live process.
    std::fs::write(state_path_of(&manager, "vm-reused-pid"), json).unwrap();

    // The lookup must not trust the stale file even though /proc/<pid> exists.
    let err = manager
        .load_state_by_pid(my_pid)
        .await
        .expect_err("stale state with reused PID must not match");
    assert!(
        err.to_string().contains("No VM found with PID"),
        "unexpected error: {}",
        err
    );

    // The stale file is removed by the cleanup pass that runs on lookup miss.
    assert!(
        manager.list_vms().await.unwrap().is_empty(),
        "stale state file with reused PID should be cleaned up"
    );

    // A state saved by the live process (start time recorded by save_state)
    // is still found.
    let live = make_vm_state("vm-live-pid", "live-pid", my_pid);
    manager.save_state(&live).await.unwrap();
    let found = manager.load_state_by_pid(my_pid).await.unwrap();
    assert_eq!(found.vm_id, "vm-live-pid");
    assert!(
        found.pid_start_time.is_some(),
        "save_state should record the process start time"
    );
}

/// Bind `name` in the abstract Unix socket namespace without close-on-exec, from a forked
/// child before it execs, so the exec'd program holds the socket. Only raw calls: this runs
/// between fork and exec.
#[cfg(feature = "privileged-tests")]
fn bind_abstract_in_child(addr: &libc::sockaddr_un, len: libc::socklen_t) -> std::io::Result<()> {
    // SAFETY: socket(2) and bind(2) with a fully initialised address of `len` bytes.
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::bind(fd, addr as *const _ as *const libc::sockaddr, len) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// A loopback address is host-wide: another user's VM holding one keeps it from a root VM
/// (#1882). A process of uid 65534 holds the lease of an address, the way a user's fcvm
/// does; a root allocation with a data directory of its own must hand out another. Before
/// the lease, the allocation read only its own state files and returned 127.0.0.2.
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn a_loopback_address_another_user_holds_is_not_handed_out() {
    use std::os::unix::process::CommandExt;

    // The first address whose lease no process holds now, and its socket address.
    let mut held = None;
    'search: for b2 in 0..=255u8 {
        for b3 in 2..=254u8 {
            let ip = format!("127.0.{b2}.{b3}");
            let name = format!("{}{ip}", fcvm::state::manager::LOOPBACK_LEASE_PREFIX);
            // SAFETY: an all-zero sockaddr_un is valid; the name fits in sun_path.
            let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
            addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
            for (slot, byte) in addr.sun_path[1..].iter_mut().zip(name.as_bytes()) {
                *slot = *byte as libc::c_char;
            }
            let len =
                (std::mem::size_of::<libc::sa_family_t>() + 1 + name.len()) as libc::socklen_t;
            let mut command = std::process::Command::new("sleep");
            command.arg("600").uid(65534).gid(65534);
            // SAFETY: the closure makes only raw socket(2) and bind(2) calls.
            unsafe {
                command.pre_exec(move || bind_abstract_in_child(&addr, len));
            }
            match command.spawn() {
                Ok(child) => {
                    held = Some((ip, child));
                    break 'search;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {}
                Err(e) => panic!("starting the uid 65534 lease holder: {e}"),
            }
        }
    }
    let (held_ip, mut holder) = held.expect("no loopback address lease was free");

    let dir = TempDir::new().unwrap();
    let manager = StateManager::new(dir.path().to_path_buf());
    let mut state = VmState::new(
        "vm-root-lease".to_string(),
        "alpine:latest".to_string(),
        1,
        512,
    );
    let allocated = manager.allocate_loopback_ip(&mut state);
    let _ = manager.delete_state("vm-root-lease").await;
    let _ = holder.kill();
    let _ = holder.wait();
    let allocated = allocated.unwrap();
    assert_ne!(
        allocated, held_ip,
        "root was handed the loopback address {held_ip} that a process of uid 65534 holds"
    );
}

/// A state file exactly as the chrono build wrote it: `created_at` with nanoseconds,
/// `last_updated` with none.
const CHRONO_STATE_JSON: &str = r#"{"schema_version":1,"vm_id":"v","name":null,"status":"starting","health_status":"unknown","exit_code":null,"pid":null,"pid_start_time":null,"lifecycle_ready":false,"holder_pid":null,"vsock_epoch":0,"created_at":"2026-11-18T11:06:40.123456789Z","last_updated":"2026-11-18T11:08:20Z","config":{"image":"i","vcpu":1,"memory_mib":64,"network":{"tap_device":"","guest_mac":"","guest_ip":null,"host_ip":null,"host_veth":null,"loopback_ip":null,"dns_server":null,"guest_ipv6":null,"host_ipv6":null,"guest_ipv6_local_only":false,"dns_search":null,"namespace_name":null,"http_proxy":null,"https_proxy":null},"volumes":[],"extra_disks":[],"pmem_devices":[],"nfs_shares":[],"health_check_url":null,"health_check_timeout":5,"restore_hook":null,"guest_thp":null,"guest_thp_restore":false,"snapshot_name":null,"process_type":"vm","serve_pid":null,"uffd_mode":null,"original_vsock_vm_id":null,"vsock_socket_path":null,"source_vsock_socket_path":null,"port_mappings":[],"published_guest_ports":null,"forward_localhost":[],"labels":{},"kernel_profile":null,"image_mode":null,"image_disk_path":null,"image_disk_identity":null,"hugepages":false,"portable_volumes":false,"user":null,"network_mode":"rootless","ipv6_prefix":null,"tty":false,"interactive":false,"username":null,"hypervisor":"Firecracker","firecracker_bin":null,"balloon_mib":null,"balloon_free_page_reporting":false,"gpus":[],"container_start_version":1,"guest":"linux"}}"#;

#[test]
fn state_json_written_by_the_chrono_build_loads_with_the_same_instants() {
    let state: VmState = serde_json::from_str(CHRONO_STATE_JSON).unwrap();
    assert_eq!(state.created_at.as_second(), 1_795_000_000);
    assert_eq!(state.created_at.subsec_nanosecond(), 123_456_789);
    assert_eq!(state.last_updated.as_second(), 1_795_000_100);
    assert_eq!(state.last_updated.subsec_nanosecond(), 0);
}

#[test]
fn state_serialization_round_trips_and_matches_what_the_chrono_build_wrote() {
    let state: VmState = serde_json::from_str(CHRONO_STATE_JSON).unwrap();
    let written = serde_json::to_string(&state).unwrap();
    assert_eq!(
        written, CHRONO_STATE_JSON,
        "byte-identical to the chrono output"
    );
    let back: VmState = serde_json::from_str(&written).unwrap();
    assert_eq!(back.created_at, state.created_at);
    assert_eq!(back.last_updated, state.last_updated);
}

/// `fcvm ls --json` of the chrono build printed offsets as `Z`, but a hand-edited or older file
/// may carry `+00:00`; both name the same instant.
#[test]
fn state_json_with_a_numeric_utc_offset_loads_with_the_same_instant() {
    let edited = CHRONO_STATE_JSON.replace("123456789Z", "123456789+00:00");
    let state: VmState = serde_json::from_str(&edited).unwrap();
    assert_eq!(state.created_at.as_second(), 1_795_000_000);
    assert_eq!(state.created_at.subsec_nanosecond(), 123_456_789);
}

/// `count` state files of other VMs, all live (owned by PID 1, with its start time), as a
/// host with that many VMs up has, and a sweep stamp saying the directory was just swept.
fn many_other_vms(dir: &std::path::Path, count: usize) {
    let init_start = fcvm::utils::process_start_time(1);
    let manager = StateManager::new(dir.to_path_buf());
    for i in 0..count {
        let mut state = make_vm_state(&format!("other-{i:05}"), &format!("other-{i}"), 1);
        state.pid_start_time = init_start;
        let path = state_path_of(&manager, &format!("other-{i:05}"));
        std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
        std::fs::write(path.with_extension("json.lock"), b"").unwrap();
    }
    manager.set_sweep_stamp(std::time::Duration::ZERO).unwrap();
}

/// Watch `dir` and every directory under it (the buckets the other VMs' states are in) for
/// opens of files in them; `opens` drains the events and counts the opened names that start
/// with `other-`. A queue overflow counts as an open: it means more
/// opens than the queue holds.
struct OpenWatch(nix::sys::inotify::Inotify);

impl OpenWatch {
    fn new(dir: &std::path::Path) -> Self {
        use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
        let inotify = Inotify::init(InitFlags::IN_NONBLOCK).unwrap();
        let watched = dirs_under(dir);
        assert!(
            watched.iter().any(|dir| std::fs::read_dir(dir)
                .unwrap()
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().starts_with("other-"))),
            "no watched directory holds another VM's state, so this watch can see nothing"
        );
        for dir in watched {
            inotify.add_watch(&dir, AddWatchFlags::IN_OPEN).unwrap();
        }
        Self(inotify)
    }

    fn opens(&self) -> usize {
        use nix::sys::inotify::AddWatchFlags;
        let mut opened = 0;
        loop {
            match self.0.read_events() {
                Ok(events) if !events.is_empty() => {
                    for event in events {
                        if event.mask.contains(AddWatchFlags::IN_Q_OVERFLOW) {
                            opened += 1;
                        }
                        if event
                            .name
                            .as_ref()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name.starts_with("other-"))
                        {
                            opened += 1;
                        }
                    }
                }
                Ok(_) | Err(nix::errno::Errno::EAGAIN) => return opened,
                Err(error) => panic!("reading inotify events: {error}"),
            }
        }
    }
}

/// Starting a VM (claim its name, lease a loopback address, save its state again) must
/// read no other VM's state: each start used to read every state several times over, so
/// with 800 VMs up a start waited about 25 s on the name lock (bench/start-rate).
///
/// The periodic sweep (`maybe_sweep`) is the one start that does read them, and only the
/// start that takes `sweep.lock`. This test holds that lock across the start, with the
/// stamp already expired, so whatever time passes the measured start cannot be the
/// sweeping one.
#[tokio::test]
async fn starting_a_vm_reads_no_other_vms_state() {
    let temp_dir = TempDir::new().unwrap();
    let manager = StateManager::new(temp_dir.path().to_path_buf());
    manager.init().await.unwrap();
    many_other_vms(temp_dir.path(), 2000);
    manager
        .set_sweep_stamp(std::time::Duration::from_secs(3600))
        .unwrap();
    let _sweeping = nix::fcntl::Flock::lock(
        std::fs::File::create(temp_dir.path().join("sweep.lock")).unwrap(),
        nix::fcntl::FlockArg::LockExclusive,
    )
    .unwrap();

    let watch = OpenWatch::new(temp_dir.path());
    let mut vm = VmState::new("vm-start".to_string(), "alpine:latest".to_string(), 1, 512);
    vm.name = Some("start".to_string());
    manager.claim_name(&mut vm).await.unwrap();
    manager.allocate_loopback_ip(&mut vm).unwrap();
    vm.status = VmStatus::Running;
    manager.save_state(&vm).await.unwrap();
    let opened = watch.opens();
    manager.delete_state("vm-start").await.unwrap();

    assert_eq!(
        opened, 0,
        "starting one VM opened other VMs' state files {opened} times"
    );
}

/// Starting and stopping a VM truncates no lock file. Lock files hold nothing, and opening one
/// with O_TRUNC takes the inode lock for writing and changes the file: at the teardown of
/// 1000 clones, 1,075 waiters queued on the host-wide name lock of the layout before this
/// one for 6 s each (bench/start-rate).
/// A truncation of an existing file is an IN_MODIFY event; creating one is not.
#[tokio::test]
async fn starting_and_stopping_a_vm_truncates_no_lock_file() {
    use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
    let temp_dir = TempDir::new().unwrap();
    let manager = StateManager::new(temp_dir.path().to_path_buf());
    manager.init().await.unwrap();
    let start_and_stop = |id: &'static str| {
        let manager = &manager;
        async move {
            let mut vm = VmState::new(id.to_string(), "alpine:latest".to_string(), 1, 512);
            vm.name = Some("same-name".to_string());
            manager.claim_name(&mut vm).await.unwrap();
            manager.allocate_loopback_ip(&mut vm).unwrap();
            vm.status = VmStatus::Running;
            manager.save_state(&vm).await.unwrap();
            manager.delete_state(id).await.unwrap();
        }
    };
    // The first start makes every directory a start of this vm_id, name and process
    // uses. The second makes the VM's lock file anew (the first's delete removed it) and
    // opens it again for each later save and for the delete: those opens are what a
    // truncating open would show up in.
    start_and_stop("vm-repeat").await;
    let inotify = Inotify::init(InitFlags::IN_NONBLOCK).unwrap();
    let dirs = dirs_under(temp_dir.path());
    let lock_dir = manager
        .lock_file("vm-repeat")
        .parent()
        .unwrap()
        .to_path_buf();
    assert!(
        dirs.contains(&lock_dir),
        "the VM's lock file is in no watched directory"
    );
    for dir in dirs {
        inotify.add_watch(&dir, AddWatchFlags::IN_MODIFY).unwrap();
    }
    start_and_stop("vm-repeat").await;
    let mut truncated = Vec::new();
    loop {
        match inotify.read_events() {
            Ok(events) if !events.is_empty() => {
                for event in events {
                    let name = event.name.map(|n| n.to_string_lossy().into_owned());
                    if event.mask.contains(AddWatchFlags::IN_Q_OVERFLOW)
                        || name.as_deref().is_some_and(|n| n.ends_with(".lock"))
                    {
                        truncated.push(name.unwrap_or_else(|| "(queue overflow)".into()));
                    }
                }
            }
            Ok(_) | Err(nix::errno::Errno::EAGAIN) => break,
            Err(error) => panic!("reading inotify events: {error}"),
        }
    }
    assert!(
        truncated.is_empty(),
        "starting and stopping a VM modified lock files: {truncated:?}"
    );
}

/// A clone finds its memory server by PID. That lookup must read the server's state and
/// no other VM's.
#[tokio::test]
async fn finding_a_vm_by_pid_reads_no_other_vms_state() {
    let temp_dir = TempDir::new().unwrap();
    let manager = StateManager::new(temp_dir.path().to_path_buf());
    manager.init().await.unwrap();
    let mut serve = VmState::new("vm-serve".to_string(), "alpine:latest".to_string(), 1, 512);
    serve.name = Some("serve".to_string());
    manager.claim_name(&mut serve).await.unwrap();
    many_other_vms(temp_dir.path(), 2000);

    let watch = OpenWatch::new(temp_dir.path());
    let found = manager.load_state_by_pid(std::process::id()).await.unwrap();
    let opened = watch.opens();
    manager.delete_state("vm-serve").await.unwrap();

    assert_eq!(found.vm_id, "vm-serve");
    assert_eq!(
        opened, 0,
        "finding one VM by PID opened other VMs' state files {opened} times"
    );
}

/// Many processes claiming one name at once, each with a state manager of its own (so
/// its own lock descriptions): exactly one gets it. And addresses leased at once are all
/// different, also for VMs whose search starts at the same address: each leases with the
/// same vm_id in a data directory of its own, so only the host-wide lease tells them apart.
#[test]
fn of_many_concurrent_claims_of_one_name_exactly_one_wins_and_addresses_differ() {
    let temp_dir = TempDir::new().unwrap();
    let dir = temp_dir.path().to_path_buf();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
    let threads: Vec<_> = (0..16)
        .map(|i| {
            let (dir, barrier) = (dir.clone(), barrier.clone());
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async {
                    let manager = StateManager::new(dir.clone());
                    let mut contender = VmState::new(
                        format!("vm-contender-{i}"),
                        "alpine:latest".to_string(),
                        1,
                        512,
                    );
                    contender.name = Some("contested".to_string());
                    let own_dir = dir.join(format!("own-{i}"));
                    let own_manager = StateManager::new(own_dir);
                    own_manager.init().await.unwrap();
                    let mut own =
                        VmState::new("vm-own".to_string(), "alpine:latest".to_string(), 1, 512);
                    barrier.wait();
                    let won = manager.claim_name(&mut contender).await.is_ok();
                    let ip = own_manager.allocate_loopback_ip(&mut own).unwrap();
                    (won, ip)
                })
            })
        })
        .collect();
    let results: Vec<(bool, String)> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    let winners = results.iter().filter(|(won, _)| *won).count();
    let ips: std::collections::HashSet<&String> = results.iter().map(|(_, ip)| ip).collect();
    let manager = StateManager::new(dir.clone());
    let runtime = tokio::runtime::Runtime::new().unwrap();
    for i in 0..16 {
        let _ = runtime.block_on(manager.delete_state(&format!("vm-contender-{i}")));
        // Every lease of this process under vm_id `vm-own` goes with the first delete.
        let _ = runtime
            .block_on(StateManager::new(dir.join(format!("own-{i}"))).delete_state("vm-own"));
    }
    assert_eq!(
        winners, 1,
        "{winners} of 16 concurrent claims of one name won"
    );
    assert_eq!(ips.len(), 16, "16 VMs leased only {} addresses", ips.len());
}

/// Not a test of its own: the rootless half of
/// `a_rootless_launch_writes_its_state_after_a_root_launch_in_the_same_directory`, run as
/// another uid through `setpriv`. With `FCVM_TEST_ROOTLESS_STATE_DIR` set it does a VM's
/// start-path writes in that state directory; without it, it returns at once.
#[tokio::test]
async fn rootless_launch_child() {
    let Ok(dir) = std::env::var("FCVM_TEST_ROOTLESS_STATE_DIR") else {
        return;
    };
    let manager = StateManager::new(std::path::PathBuf::from(dir));
    let mut vm = VmState::new(
        "vm-rootless".to_string(),
        "alpine:latest".to_string(),
        1,
        512,
    );
    vm.name = Some("rootless".to_string());
    manager.claim_name(&mut vm).await.unwrap();
    manager.allocate_loopback_ip(&mut vm).unwrap();
    vm.status = VmStatus::Running;
    manager.save_state(&vm).await.unwrap();
    assert_eq!(
        manager
            .load_state_by_pid(std::process::id())
            .await
            .unwrap()
            .vm_id,
        "vm-rootless"
    );
    manager.delete_state("vm-rootless").await.unwrap();
}

/// Root and rootless launches share one state directory (CI runs the privileged tests as
/// root beside the unprivileged ones in one data directory). A root launch must leave
/// nothing in a state directory another user owns that the owner's next launch cannot
/// write: the bucket directories it made, the slot of a name a root VM held when it was
/// killed, or the sweep's stamp. The owner's launch runs as uid 65534 through `setpriv`, from a
/// copy of this test binary that uid can read.
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn a_rootless_launch_writes_its_state_after_a_root_launch_in_the_same_directory() {
    use std::os::unix::fs::PermissionsExt;

    let base = TempDir::new().unwrap();
    std::fs::set_permissions(base.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let state = base.path().join("state");
    std::fs::create_dir(&state).unwrap();
    let nobody = Some(nix::unistd::Uid::from_raw(65534));
    nix::unistd::chown(&state, nobody, Some(nix::unistd::Gid::from_raw(65534))).unwrap();

    // The root launch: claim, lease, save; its VM stays up.
    let manager = StateManager::new(state.clone());
    let mut root_vm = VmState::new("vm-root".to_string(), "alpine:latest".to_string(), 1, 512);
    root_vm.name = Some("root".to_string());
    manager.claim_name(&mut root_vm).await.unwrap();
    manager.allocate_loopback_ip(&mut root_vm).unwrap();
    manager.save_state(&root_vm).await.unwrap();
    // A root VM that held the owner's name and was killed: its slot, handed to the state
    // directory's owner as a root claim hands it, with the dead VM's token in it. The
    // owner's claim has to unlink that token.
    let slot = manager.name_slot("rootless");
    let gid = Some(nix::unistd::Gid::from_raw(65534));
    // Its bucket may be the one root's own name made.
    let _ = std::fs::create_dir(slot.parent().unwrap());
    nix::unistd::chown(slot.parent().unwrap(), nobody, gid).unwrap();
    std::fs::create_dir(&slot).unwrap();
    nix::unistd::chown(&slot, nobody, gid).unwrap();
    std::fs::write(slot.join("4000000000.1.vm-crashed"), b"").unwrap();
    // The sweep is due, so the owner's claim rewrites the stamp root's claim wrote.
    let stamp = state.join("sweep.stamp");
    let hour = std::time::Duration::from_secs(3600);
    manager.set_sweep_stamp(hour).unwrap();
    let aged = std::fs::read_to_string(&stamp).unwrap();

    let binary = base.path().join("test_state_manager");
    std::fs::copy(std::env::current_exe().unwrap(), &binary).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = std::process::Command::new("setpriv")
        .args(["--reuid=65534", "--regid=65534", "--clear-groups", "--"])
        .arg(&binary)
        .args(["--exact", "rootless_launch_child", "--nocapture"])
        .env("FCVM_TEST_ROOTLESS_STATE_DIR", &state)
        .output()
        .unwrap();
    let after_the_owner = std::fs::read_to_string(&stamp).unwrap();
    // The reverse: the sweep is due again, and root's claim rewrites the stamp the owner
    // wrote.
    manager.set_sweep_stamp(hour).unwrap();
    let aged_again = std::fs::read_to_string(&stamp).unwrap();
    let mut second = VmState::new("vm-root-2".to_string(), "alpine:latest".to_string(), 1, 512);
    second.name = Some("root-2".to_string());
    manager.claim_name(&mut second).await.unwrap();
    let after_root = std::fs::read_to_string(&stamp).unwrap();
    manager.delete_state("vm-root-2").await.unwrap();
    manager.delete_state("vm-root").await.unwrap();

    assert!(
        output.status.success(),
        "the owner's launch after a root launch failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // A stamp is `<boot id> <nanoseconds since boot>` of the sweep that last began. An aged
    // one is another boot's when this host has been up for less than its age.
    let this_boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
    let newer = |stamp: &str, than: &str| {
        let parts = |stamp: &str| -> (String, u64) {
            let (boot, began) = stamp.split_once(' ').unwrap();
            (boot.to_string(), began.parse().unwrap())
        };
        let (stamp, than) = (parts(stamp), parts(than));
        stamp.0 == this_boot.trim() && (than.0 != stamp.0 || stamp.1 > than.1)
    };
    assert!(
        newer(&after_the_owner, &aged),
        "the owner's claim could not rewrite the sweep stamp root wrote: {aged:?} then \
         {after_the_owner:?}"
    );
    assert!(
        newer(&after_root, &aged_again),
        "root's claim did not rewrite the sweep stamp the owner wrote: {aged_again:?} then \
         {after_root:?}"
    );
}

/// A state that is published stays saved when its PID index entry cannot be written (here
/// `pids` is a file, not the index): the index only speeds up lookups by PID, which scan when
/// it has nothing, so `save_state` must not fail a launch whose state is already on disk.
#[tokio::test]
async fn an_unwritable_pid_index_does_not_fail_a_save() {
    let temp_dir = TempDir::new().unwrap();
    let manager = StateManager::new(temp_dir.path().to_path_buf());
    manager.init().await.unwrap();
    std::fs::write(temp_dir.path().join("pids"), b"not the index").unwrap();
    let state = make_vm_state("vm-unindexed", "unindexed", std::process::id());

    manager.save_state(&state).await.unwrap();
    assert_eq!(
        manager
            .load_state_by_pid(std::process::id())
            .await
            .unwrap()
            .vm_id,
        "vm-unindexed"
    );
    manager.delete_state("vm-unindexed").await.unwrap();
}

/// Deleting a VM whose PID index entry cannot be removed (here it is a directory with a
/// file in it) still retires its lock and gives its name back: the three are independent,
/// and once the state is gone nothing would name the others again.
#[tokio::test]
async fn a_pid_entry_that_cannot_be_removed_does_not_stop_the_rest_of_a_delete() {
    let temp_dir = TempDir::new().unwrap();
    let dir = temp_dir.path();
    let manager = StateManager::new(dir.to_path_buf());
    manager.init().await.unwrap();
    let mut vm = VmState::new("vm-stuck".to_string(), "alpine:latest".to_string(), 1, 512);
    vm.name = Some("stuck".to_string());
    manager.claim_name(&mut vm).await.unwrap();
    let entry = dir
        .join("pids")
        .join(fcvm::state::manager::bucket_of(
            &std::process::id().to_string(),
        ))
        .join(format!("{}.vm-stuck", std::process::id()));
    std::fs::remove_file(&entry).unwrap();
    std::fs::create_dir(&entry).unwrap();
    std::fs::write(entry.join("keep"), b"").unwrap();

    let deleted = manager.delete_state("vm-stuck").await;
    assert!(deleted.is_ok(), "{deleted:?}");
    assert!(
        !manager.lock_file("vm-stuck").exists(),
        "the VM's lock was left behind"
    );
    assert!(
        !manager.name_slot("stuck").exists(),
        "the VM's name slot was left behind"
    );
}

/// A dead state that holds no token (one saved without a claim) is not found by a claim of
/// its name, so the claim does not remove it, and it stays until the next sweep. A lookup by that name must
/// still find the one live VM, not report two.
#[tokio::test]
async fn a_dead_state_without_a_name_entry_does_not_hide_the_live_vm_by_name() {
    let temp_dir = TempDir::new().unwrap();
    let manager = StateManager::new(temp_dir.path().to_path_buf());
    manager.init().await.unwrap();
    // Above the kernel's PID_MAX_LIMIT, so no process has it.
    let dead = make_vm_state("vm-before-index", "web", 4_000_000_000);
    std::fs::write(
        state_path_of(&manager, "vm-before-index"),
        serde_json::to_vec(&dead).unwrap(),
    )
    .unwrap();
    // A sweep ran just now, so this claim does not sweep the dead state away.
    manager.set_sweep_stamp(std::time::Duration::ZERO).unwrap();
    let mut live = VmState::new("vm-live".to_string(), "alpine:latest".to_string(), 1, 512);
    live.name = Some("web".to_string());
    manager.claim_name(&mut live).await.unwrap();
    assert!(
        manager.state_file("vm-before-index").exists(),
        "the dead state is gone, so this test checks nothing"
    );

    let found = manager.load_state_by_name("web").await;
    manager.delete_state("vm-live").await.unwrap();
    assert_eq!(found.unwrap().vm_id, "vm-live");
}

/// What a writer killed between writing its temporary file and renaming it leaves: the
/// last published state, and a temporary file beside it. Every reader still gets the
/// published state, the next write replaces the temporary file, and a kill after the rename
/// leaves the new state, which is the only other thing a write can leave.
#[tokio::test]
async fn a_write_killed_before_its_rename_leaves_the_last_published_state() {
    let temp_dir = TempDir::new().unwrap();
    let manager = StateManager::new(temp_dir.path().to_path_buf());
    let mut vm = VmState::new("vm-ack".to_string(), "alpine:latest".to_string(), 1, 512);
    vm.name = Some("ack".to_string());
    manager.claim_name(&mut vm).await.unwrap();
    manager
        .update_health_status("vm-ack", HealthStatus::Healthy, None)
        .await
        .unwrap();

    // The killed writer got as far as half of its temporary file.
    let published = std::fs::read(manager.state_file("vm-ack")).unwrap();
    let temp = manager.state_file("vm-ack").with_extension("json.tmp");
    std::fs::write(&temp, &published[..published.len() / 2]).unwrap();

    let me = std::process::id();
    for (reader, state) in [
        ("load_state", manager.load_state("vm-ack").await.unwrap()),
        (
            "load_state_by_pid",
            manager.load_state_by_pid(me).await.unwrap(),
        ),
        (
            "load_state_by_name",
            manager.load_state_by_name("ack").await.unwrap(),
        ),
        ("list_vms", manager.list_vms().await.unwrap().remove(0)),
    ] {
        assert_eq!(state.vm_id, "vm-ack", "{reader}");
        assert_eq!(state.name.as_deref(), Some("ack"), "{reader}");
        assert!(
            matches!(state.health_status, HealthStatus::Healthy),
            "{reader} lost the acknowledged health status"
        );
    }
    assert_eq!(manager.list_vms().await.unwrap().len(), 1);

    // A writer killed after publishing and before unlinking the state it replaced leaves
    // that older state at the temporary name; the published one is still what is read.
    let mut older: serde_json::Value = serde_json::from_slice(&published).unwrap();
    older["health_status"] = "unknown".into();
    std::fs::write(&temp, serde_json::to_vec(&older).unwrap()).unwrap();
    assert!(matches!(
        manager.load_state("vm-ack").await.unwrap().health_status,
        HealthStatus::Healthy
    ));
    assert_eq!(manager.list_vms().await.unwrap().len(), 1);

    // The next write is published whole, over what the killed one left.
    manager
        .update_health_status("vm-ack", HealthStatus::Stopped, Some(3))
        .await
        .unwrap();
    assert!(!temp.exists(), "the next write left the temporary file");
    assert_eq!(
        manager.load_state("vm-ack").await.unwrap().exit_code,
        Some(3)
    );
    manager.delete_state("vm-ack").await.unwrap();
    assert!(!manager.state_file("vm-ack").exists() && !temp.exists());
}

/// While a VM's state is rewritten over and over, every reader gets a state that parses
/// and carries the VM's name: a write is published by rename, never in place.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn readers_never_see_a_state_being_written() {
    let temp_dir = TempDir::new().unwrap();
    let dir = temp_dir.path().to_path_buf();
    let manager = StateManager::new(dir.clone());
    let mut vm = VmState::new("vm-busy".to_string(), "alpine:latest".to_string(), 1, 512);
    vm.name = Some("busy".to_string());
    manager.claim_name(&mut vm).await.unwrap();

    // Each side does a fixed number of operations while the other runs: the reader reads
    // until it has read `ROUNDS` times and the writer has written `ROUNDS` times, and the
    // writer writes until the reader is done. The deadline only catches a stall.
    const ROUNDS: u32 = 50;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writes = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let writer = {
        let (dir, stop, writes) = (dir.clone(), stop.clone(), writes.clone());
        tokio::spawn(async move {
            let manager = StateManager::new(dir);
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let written = writes.load(std::sync::atomic::Ordering::Relaxed);
                manager
                    .update_state("vm-busy", |state| state.exit_code = Some(written as i32))
                    .await
                    .unwrap()
                    .expect("the state is there");
                writes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        })
    };
    let me = std::process::id();
    let mut reads = 0;
    let stalled = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while (reads < ROUNDS || writes.load(std::sync::atomic::Ordering::Relaxed) < ROUNDS)
        && !writer.is_finished()
    {
        assert!(
            std::time::Instant::now() < stalled,
            "stalled after {reads} reads and {} writes",
            writes.load(std::sync::atomic::Ordering::Relaxed)
        );
        let reader = StateManager::new(dir.clone());
        let listed = reader.list_vms().await.unwrap();
        assert_eq!(listed.len(), 1, "the VM vanished from a listing");
        for state in [
            listed.into_iter().next().unwrap(),
            reader.load_state("vm-busy").await.unwrap(),
            reader.load_state_by_pid(me).await.unwrap(),
            reader.load_state_by_name("busy").await.unwrap(),
        ] {
            assert_eq!(state.vm_id, "vm-busy");
            assert_eq!(state.name.as_deref(), Some("busy"));
        }
        reads += 1;
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    // A writer that failed ended the loop early; its panic is reported here.
    writer.await.unwrap();
    manager.delete_state("vm-busy").await.unwrap();
}

/// Leasing a loopback address writes no state: the lease is the abstract socket, and a
/// save of its own here was one of six state writes in every start. A start leases before
/// its name claim, whose write carries the address
/// (`a_starts_first_state_carries_its_loopback_address`).
#[tokio::test]
async fn leasing_a_loopback_address_writes_no_state() {
    use std::os::unix::fs::MetadataExt;
    let temp_dir = TempDir::new().unwrap();
    let manager = StateManager::new(temp_dir.path().to_path_buf());
    manager.init().await.unwrap();
    let mut vm = VmState::new("vm-lease".to_string(), "alpine:latest".to_string(), 1, 512);
    vm.name = Some("lease".to_string());
    manager.claim_name(&mut vm).await.unwrap();
    let state_file = manager.state_file("vm-lease");
    let before = std::fs::metadata(&state_file).unwrap();

    let ip = manager.allocate_loopback_ip(&mut vm).unwrap();
    assert_eq!(vm.config.network.loopback_ip.as_deref(), Some(ip.as_str()));
    let after = std::fs::metadata(&state_file).unwrap();
    assert_eq!(
        (before.ino(), before.mtime(), before.mtime_nsec()),
        (after.ino(), after.mtime(), after.mtime_nsec()),
        "leasing the address rewrote the state file"
    );
    assert_eq!(
        manager
            .load_state("vm-lease")
            .await
            .unwrap()
            .config
            .network
            .loopback_ip,
        None
    );

    // The launch's next save persists it.
    manager.save_state(&vm).await.unwrap();
    assert_eq!(
        manager
            .load_state("vm-lease")
            .await
            .unwrap()
            .config
            .network
            .loopback_ip,
        Some(ip)
    );
    manager.delete_state("vm-lease").await.unwrap();
}

/// A start leases its loopback address before it claims its name, so the VM's first
/// state carries it. A restore's published port listens on the address before the
/// restore's next state write (`restore.post_network_pre_resume` holds there), and a VM
/// listed without the address its ports are on cannot be reached by whoever listed it:
/// `test_lifecycle_interleave_early_connection_cannot_poison_restored_ingress` and
/// `test_restored_clone_udp_port_is_closed_until_the_restore_ack` read it at that hold.
/// Once kept, the lease goes with the state.
#[tokio::test]
async fn a_starts_first_state_carries_its_loopback_address() {
    let temp_dir = TempDir::new().unwrap();
    let manager = StateManager::new(temp_dir.path().to_path_buf());
    manager.init().await.unwrap();
    let mut vm = VmState::new("vm-first".to_string(), "alpine:latest".to_string(), 1, 512);
    vm.name = Some("first".to_string());

    let lease = manager.lease_loopback_ip_for_claim(&mut vm).unwrap();
    let ip = vm.config.network.loopback_ip.clone();
    assert!(ip.is_some());
    manager.claim_name(&mut vm).await.unwrap();
    lease.keep();
    assert_eq!(
        manager
            .load_state("vm-first")
            .await
            .unwrap()
            .config
            .network
            .loopback_ip,
        ip,
        "the state the name claim wrote does not hold the address"
    );
    assert!(StateManager::holds_loopback_lease("vm-first"));
    manager.delete_state("vm-first").await.unwrap();
    assert!(!StateManager::holds_loopback_lease("vm-first"));
}

/// A start that leases an address and then fails at its name claim gives the address
/// back, and so does one whose future is dropped before the claim has written the state:
/// neither has a state for `delete_state` to release it with.
#[tokio::test]
async fn a_start_that_fails_at_its_name_claim_gives_its_loopback_address_back() {
    let temp_dir = TempDir::new().unwrap();
    let manager = StateManager::new(temp_dir.path().to_path_buf());
    manager.init().await.unwrap();
    let named = |vm_id: &str| {
        let mut vm = VmState::new(vm_id.to_string(), "alpine:latest".to_string(), 1, 512);
        vm.name = Some("wanted".to_string());
        vm
    };
    // The name's owner is this process, which is alive.
    let mut owner = named("vm-owner");
    manager.claim_name(&mut owner).await.unwrap();

    let mut refused = named("vm-refused");
    let start = async {
        let lease = manager.lease_loopback_ip_for_claim(&mut refused)?;
        manager.claim_name(&mut refused).await?;
        lease.keep();
        anyhow::Ok(())
    };
    assert!(start.await.is_err(), "the name is taken by a live VM");
    assert!(
        !StateManager::holds_loopback_lease("vm-refused"),
        "a start refused at its name claim kept its loopback address"
    );

    let mut dropped = named("vm-dropped");
    let lease = manager.lease_loopback_ip_for_claim(&mut dropped).unwrap();
    assert!(StateManager::holds_loopback_lease("vm-dropped"));
    drop(lease);
    assert!(!StateManager::holds_loopback_lease("vm-dropped"));

    manager.delete_state("vm-owner").await.unwrap();
}

/// A state write that cannot be completed (a full tmpfs) fails, and the state that was published stays whole. The write goes to a
/// temporary file first, so a reader never sees a state cut short. The file size limit
/// stands in for the full filesystem: both fail the write of the temporary file.
#[tokio::test]
async fn a_state_write_that_fails_leaves_the_published_state_whole() {
    let temp_dir = TempDir::new().unwrap();
    let manager = StateManager::new(temp_dir.path().to_path_buf());
    manager.init().await.unwrap();
    let mut vm = VmState::new("vm-full".to_string(), "alpine:latest".to_string(), 1, 512);
    vm.name = Some("full".to_string());
    manager.claim_name(&mut vm).await.unwrap();
    let before = std::fs::read(manager.state_file("vm-full")).unwrap();

    // SIGXFSZ ignored, so a write past the limit returns EFBIG and does not kill the test.
    unsafe { libc::signal(libc::SIGXFSZ, libc::SIG_IGN) };
    let mut old = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_FSIZE, &mut old) }, 0);
    let small = libc::rlimit {
        rlim_cur: 64,
        rlim_max: old.rlim_max,
    };
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &small) }, 0);
    let updated = manager
        .update_health_status("vm-full", fcvm::state::HealthStatus::Healthy, None)
        .await;
    vm.status = VmStatus::Running;
    let saved = manager.save_state(&vm).await;
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &old) }, 0);

    assert!(
        updated.is_err(),
        "an update that could not be written reported success"
    );
    assert!(
        saved.is_err(),
        "a save that could not be written reported success"
    );
    assert_eq!(
        std::fs::read(manager.state_file("vm-full")).unwrap(),
        before,
        "a failed write changed the published state"
    );
    let read = manager.load_state("vm-full").await.unwrap();
    assert_eq!(read.health_status, fcvm::state::HealthStatus::Unknown);

    // With room again the same writes succeed.
    manager
        .update_health_status("vm-full", fcvm::state::HealthStatus::Healthy, None)
        .await
        .unwrap();
    manager.delete_state("vm-full").await.unwrap();
}

/// A start under a umask of 077 leaves every directory 0755, every lock file 0666 and the
/// state and the name token 0644. Root and the store's owner share a state directory, and
/// a bucket or a claim made with the umask's mode shut out the one that did not make it.
#[tokio::test]
async fn a_start_under_umask_077_makes_nothing_with_the_umasks_mode() {
    use std::os::unix::fs::MetadataExt;
    let temp_dir = TempDir::new().unwrap();
    let state_dir = temp_dir.path().join("state");
    let old = nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o077));
    let manager = StateManager::new(state_dir.clone());
    let started = async {
        manager.init().await?;
        let mut vm = VmState::new("vm-umask".to_string(), "alpine:latest".to_string(), 1, 512);
        vm.name = Some("umask".to_string());
        manager.claim_name(&mut vm).await?;
        manager
            .update_health_status("vm-umask", HealthStatus::Healthy, None)
            .await
    }
    .await;
    nix::sys::stat::umask(old);
    started.unwrap();

    let mut wrong = Vec::new();
    let mut dirs = vec![state_dir.clone()];
    while let Some(dir) = dirs.pop() {
        let mode = std::fs::metadata(&dir).unwrap().mode() & 0o7777;
        if mode != 0o755 {
            wrong.push(format!("{} is {mode:o}, not 755", dir.display()));
        }
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            let metadata = entry.metadata().unwrap();
            if metadata.is_dir() {
                dirs.push(path);
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let expected = if name.ends_with(".lock") || name == "sweep.stamp" {
                0o666
            } else {
                0o644
            };
            let mode = metadata.mode() & 0o7777;
            if mode != expected {
                wrong.push(format!("{} is {mode:o}, not {expected:o}", path.display()));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "made with the umask's mode:\n{}",
        wrong.join("\n")
    );
    manager.delete_state("vm-umask").await.unwrap();
}

/// A VM id whose state bucket is `bucket`, so two launches can be put in one bucket.
#[cfg(feature = "privileged-tests")]
fn vm_id_in_bucket(prefix: &str, bucket: &str) -> String {
    (0..)
        .map(|n| format!("{prefix}-{n}"))
        .find(|id| fcvm::state::manager::bucket_of(id) == bucket)
        .unwrap()
}

/// `root_and_the_stores_owner_each_start_on_a_runtime_directory_the_other_made`, run as
/// root in the parent and as the store's owner through `setpriv`. With
/// `FCVM_TEST_BIND_DATA_DIR` set it binds that data directory, with
/// `FCVM_TEST_BIND_RUN` standing in for `/run`, and does a VM's start and stop there;
/// without it, it returns at once.
#[tokio::test]
async fn bound_launch_child() {
    let Ok(data_dir) = std::env::var("FCVM_TEST_BIND_DATA_DIR") else {
        return;
    };
    let run = std::env::var("FCVM_TEST_BIND_RUN").unwrap();
    let vm_id = std::env::var("FCVM_TEST_BIND_VM_ID").unwrap();
    bound_launch(
        std::path::Path::new(&data_dir),
        std::path::Path::new(&run),
        &vm_id,
        true,
    )
    .await;
}

/// What a launch does to the state of `data_dir`: locate its runtime directory with `run`
/// standing in for `/run`, bind it, claim a name with a leased address, save, publish
/// health, sweep, and with `stop` delete the state. Returns the state directory.
async fn bound_launch(
    data_dir: &std::path::Path,
    run: &std::path::Path,
    vm_id: &str,
    stop: bool,
) -> std::path::PathBuf {
    use fcvm::runtime_dir::{bind, locate, Access, Host};
    let host = Host::current().unwrap();
    let located = locate(None, None, data_dir, run, &host);
    let runtime = bind(data_dir, &located, Access::Write, &host)
        .unwrap()
        .expect("a writer binds the store");
    let state_dir = runtime.dir.join("state");
    let manager = StateManager::new(state_dir.clone());
    manager.init().await.unwrap();
    let mut vm = VmState::new(vm_id.to_string(), "alpine:latest".to_string(), 1, 512);
    vm.name = Some(format!("name-of-{vm_id}"));
    let lease = manager.lease_loopback_ip_for_claim(&mut vm).unwrap();
    manager.claim_name(&mut vm).await.unwrap();
    lease.keep();
    vm.status = VmStatus::Running;
    manager.save_state(&vm).await.unwrap();
    manager
        .update_health_status(vm_id, HealthStatus::Healthy, None)
        .await
        .unwrap();
    manager.sweep_stale().await;
    assert_eq!(
        manager.load_state(vm_id).await.unwrap().health_status,
        HealthStatus::Healthy
    );
    if stop {
        manager.delete_state(vm_id).await.unwrap();
    }
    state_dir
}

/// Root and the owner of a store share its runtime directory, whichever of them is first
/// to bind it in a boot, in both places a store's states can be by default:
///
/// - root first, no base: root makes `<run>/fcvm-<owner>` for the owner, and the states
///   are under it;
/// - the owner first, no base: the states are in `<data_dir>/runtime`, and root follows
///   the store's record there;
/// - the owner first, the base made beforehand as `sudo fcvm setup` and the tmpfiles.d
///   rule make it: the states are under the base.
///
/// Whatever root makes on the way (the base, the place, this boot's directory, `state/`,
/// a bucket, a claim directory) must be the owner's to write in afterwards, and what the
/// owner makes must pass root's ownership check. Both launches put their VM in the same
/// state bucket. The owner's launches run as uid 65534 through `setpriv`, from a copy of
/// this test binary that uid can read.
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn root_and_the_stores_owner_each_start_on_a_runtime_directory_the_other_made() {
    use std::os::unix::fs::PermissionsExt;

    let top = TempDir::new().unwrap();
    std::fs::set_permissions(top.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let binary = top.path().join("test_state_manager");
    std::fs::copy(std::env::current_exe().unwrap(), &binary).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let nobody = Some(nix::unistd::Uid::from_raw(65534));
    let nogroup = Some(nix::unistd::Gid::from_raw(65534));
    let as_owner = |data: &std::path::Path, run: &std::path::Path, vm_id: &str| {
        let output = std::process::Command::new("setpriv")
            .args(["--reuid=65534", "--regid=65534", "--clear-groups", "--"])
            .arg(&binary)
            .args(["--exact", "bound_launch_child", "--nocapture"])
            .env("FCVM_TEST_BIND_DATA_DIR", data)
            .env("FCVM_TEST_BIND_RUN", run)
            .env("FCVM_TEST_BIND_VM_ID", vm_id)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "the owner's launch of {vm_id} failed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    };

    for (name, root_first, base_made, on_store) in [
        ("root-first", true, false, false),
        ("owner-first", false, false, true),
        ("owner-first-with-a-base", false, true, false),
    ] {
        let case = top.path().join(name);
        let (data, run) = (case.join("data"), case.join("run"));
        std::fs::create_dir_all(&data).unwrap();
        // The store is the owner's; `run` is root's and nobody else writes it, as /run is.
        std::fs::create_dir_all(&run).unwrap();
        nix::unistd::chown(&data, nobody, nogroup).unwrap();
        let base = run.join("fcvm-65534");
        if base_made {
            std::fs::create_dir(&base).unwrap();
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
            nix::unistd::chown(&base, nobody, nogroup).unwrap();
        }
        let (root_vm, owner_vm, owner_again) = (
            vm_id_in_bucket("vm-root", "a"),
            vm_id_in_bucket("vm-owner", "a"),
            vm_id_in_bucket("vm-again", "a"),
        );

        if !root_first {
            as_owner(&data, &run, &owner_vm);
        }
        // Root's VM stays up while the owner starts and stops one in the same bucket.
        let state_dir = bound_launch(&data, &run, &root_vm, false).await;
        let place = if on_store {
            data.join("runtime")
        } else {
            base.clone()
        };
        assert!(
            state_dir.starts_with(&place),
            "{name}: the states are in {}, not under {}",
            state_dir.display(),
            place.display()
        );
        assert_eq!(
            fcvm::runtime_dir::bound_to(&data).unwrap().as_deref(),
            state_dir.parent(),
            "{name}: what a tool is told about this data directory"
        );
        as_owner(&data, &run, &owner_again);
        StateManager::new(state_dir.clone())
            .delete_state(&root_vm)
            .await
            .unwrap();
        as_owner(&data, &run, &owner_vm);

        // Nothing root made on the way to the states stayed root's directory.
        use std::os::unix::fs::MetadataExt;
        let mut dirs = vec![place];
        while let Some(dir) = dirs.pop() {
            assert_eq!(
                std::fs::metadata(&dir).unwrap().uid(),
                65534,
                "{name}: {} is not the store owner's",
                dir.display()
            );
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                if entry.file_type().unwrap().is_dir() {
                    dirs.push(entry.path());
                }
            }
        }
    }
}

/// What `sudo fcvm setup` installs for a store's owner: the base under `/run`, the
/// owner's and closed to everyone else, and the tmpfiles.d rule that makes it the same at
/// every boot. With it the owner's first start of a boot goes under the base with no root
/// step, which is what `owner-first-with-a-base` above starts from.
#[cfg(feature = "privileged-tests")]
#[test]
fn setup_makes_the_owners_base_and_the_rule_that_makes_it_at_boot() {
    use fcvm::runtime_dir::{install_base, locate, Host, Place};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let top = TempDir::new().unwrap();
    let (data, run, rules) = (
        top.path().join("data"),
        top.path().join("run"),
        top.path().join("tmpfiles.d"),
    );
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&run).unwrap();
    nix::unistd::chown(
        &data,
        Some(nix::unistd::Uid::from_raw(65534)),
        Some(nix::unistd::Gid::from_raw(65533)),
    )
    .unwrap();
    let host = Host::current().unwrap();
    // Under a umask that would close the rule to systemd-tmpfiles' reader.
    let old = nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o077));
    let installed = install_base(&data, &run, &rules, &host);
    nix::sys::stat::umask(old);
    let (base, rule) = installed.unwrap();

    assert_eq!(base, run.join("fcvm-65534"));
    let made = std::fs::metadata(&base).unwrap();
    assert_eq!(
        (made.uid(), made.gid(), made.permissions().mode() & 0o7777),
        (65534, 65533, 0o700)
    );
    assert_eq!(rule, rules.join("fcvm-65534.conf"));
    let text = std::fs::read_to_string(&rule).unwrap();
    assert_eq!(
        text.lines().last(),
        Some(format!("d {} 0700 65534 65533 -", base.display()).as_str()),
        "{text}"
    );
    assert!(text
        .lines()
        .all(|line| line.starts_with('#') || line.starts_with("d ")));
    assert_eq!(
        std::fs::metadata(&rule).unwrap().permissions().mode() & 0o7777,
        0o644
    );
    // Installing again changes nothing.
    assert_eq!(
        install_base(&data, &run, &rules, &host).unwrap(),
        (base, rule)
    );

    // The owner, with no root step from here on, is given the tmpfs.
    let owner = Host {
        euid: 65534,
        ..host
    };
    assert_eq!(locate(None, None, &data, &run, &owner).place, Place::Run);
}
