use chrono::Utc;
use fcvm::network::NetworkConfig;
use fcvm::state::{HealthStatus, ProcessType, StateManager, VmConfig, VmState, VmStatus};
use tempfile::TempDir;

#[tokio::test]
async fn test_state_persistence() {
    let temp_dir = TempDir::new().unwrap();
    let manager = StateManager::new(temp_dir.path().to_path_buf());

    // Initialize state directory
    manager.init().await.unwrap();

    let now = Utc::now();

    // Create and save a VM state
    let state = VmState {
        schema_version: 1,
        vm_id: "test-vm-1".to_string(),
        name: Some("test-vm".to_string()),
        status: VmStatus::Running,
        health_status: HealthStatus::Healthy,
        exit_code: None,
        pid: Some(12345),
        holder_pid: None,
        created_at: now,
        last_updated: now,
        config: VmConfig {
            image: "nginx:alpine".to_string(),
            vcpu: 2,
            memory_mib: 512,
            network: NetworkConfig {
                tap_device: "tap0".to_string(),
                guest_mac: "02:00:00:00:00:01".to_string(),
                guest_ip: Some("172.16.0.2".to_string()),
                host_ip: Some("172.16.0.1".to_string()),
                host_veth: Some("veth0".to_string()),
                loopback_ip: None,
                dns_server: None,
                guest_ipv6: None,
                host_ipv6: None,
                dns_search: None,
                http_proxy: None,
                namespace_name: None,
            },
            volumes: vec![],
            extra_disks: vec![],
            nfs_shares: vec![],
            health_check_url: None,
            snapshot_name: None,
            process_type: Some(ProcessType::Vm),
            serve_pid: None,
            original_vsock_vm_id: None,
            port_mappings: vec![],
            forward_localhost: vec![],
            network_mode: Default::default(),
            ipv6_prefix: None,
            tty: false,
            interactive: false,
            labels: std::collections::HashMap::new(),
            hugepages: false,
            portable_volumes: false,
            user: None,
            username: None,
            health_check_timeout: 5,
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
        let state_file = temp_dir.path().join("test-vm-1.json");
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

    let now = Utc::now();

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
            holder_pid: None,
            created_at: now,
            last_updated: now,
            config: VmConfig {
                image: "nginx:alpine".to_string(),
                vcpu: 1,
                memory_mib: 256,
                network: NetworkConfig::default(),
                volumes: vec![],
                extra_disks: vec![],
                nfs_shares: vec![],
                health_check_url: None,
                snapshot_name: None,
                process_type: Some(ProcessType::Vm),
                serve_pid: None,
                original_vsock_vm_id: None,
                port_mappings: vec![],
                forward_localhost: vec![],
                network_mode: Default::default(),
                ipv6_prefix: None,
                tty: false,
                interactive: false,
                labels: std::collections::HashMap::new(),
                hugepages: false,
                portable_volumes: false,
                user: None,
                username: None,
                health_check_timeout: 5,
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

    let now = Utc::now();

    // Save two VMs with the same name but different vm_ids and PIDs
    for (i, pid) in [(1u32, 5000u32), (2, 5001)] {
        let state = VmState {
            schema_version: 1,
            vm_id: format!("vm-dup-{}", i),
            name: Some("duplicate-name".to_string()),
            status: VmStatus::Running,
            health_status: HealthStatus::Healthy,
            exit_code: None,
            pid: Some(pid),
            holder_pid: None,
            created_at: now,
            last_updated: now,
            config: VmConfig {
                image: "nginx:alpine".to_string(),
                vcpu: 1,
                memory_mib: 256,
                network: NetworkConfig::default(),
                volumes: vec![],
                extra_disks: vec![],
                nfs_shares: vec![],
                health_check_url: None,
                snapshot_name: None,
                process_type: Some(ProcessType::Vm),
                serve_pid: None,
                original_vsock_vm_id: None,
                port_mappings: vec![],
                forward_localhost: vec![],
                network_mode: Default::default(),
                ipv6_prefix: None,
                tty: false,
                interactive: false,
                labels: std::collections::HashMap::new(),
                hugepages: false,
                portable_volumes: false,
                user: None,
                username: None,
                health_check_timeout: 5,
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
    assert!(msg.contains("5000"), "error should list PID 5000: {}", msg);
    assert!(msg.contains("5001"), "error should list PID 5001: {}", msg);

    // Looking up a unique name should still work
    let state = VmState {
        schema_version: 1,
        vm_id: "vm-unique".to_string(),
        name: Some("unique-name".to_string()),
        status: VmStatus::Running,
        health_status: HealthStatus::Healthy,
        exit_code: None,
        pid: Some(6000),
        holder_pid: None,
        created_at: now,
        last_updated: now,
        config: VmConfig {
            image: "nginx:alpine".to_string(),
            vcpu: 1,
            memory_mib: 256,
            network: NetworkConfig::default(),
            volumes: vec![],
            extra_disks: vec![],
            nfs_shares: vec![],
            health_check_url: None,
            snapshot_name: None,
            process_type: Some(ProcessType::Vm),
            serve_pid: None,
            original_vsock_vm_id: None,
            port_mappings: vec![],
            forward_localhost: vec![],
            network_mode: Default::default(),
            ipv6_prefix: None,
            tty: false,
            interactive: false,
            labels: std::collections::HashMap::new(),
            hugepages: false,
            portable_volumes: false,
            user: None,
            username: None,
            health_check_timeout: 5,
        },
    };
    manager.save_state(&state).await.unwrap();
    let found = manager.load_state_by_name("unique-name").await.unwrap();
    assert_eq!(found.pid, Some(6000));
}

/// Helper to create a minimal VmState with given vm_id and pid
fn make_vm_state(vm_id: &str, name: &str, pid: u32) -> VmState {
    VmState {
        schema_version: 1,
        vm_id: vm_id.to_string(),
        name: Some(name.to_string()),
        status: VmStatus::Running,
        health_status: HealthStatus::Healthy,
        exit_code: None,
        pid: Some(pid),
        holder_pid: None,
        created_at: Utc::now(),
        last_updated: Utc::now(),
        config: VmConfig {
            image: "test:latest".to_string(),
            vcpu: 1,
            memory_mib: 256,
            network: NetworkConfig::default(),
            volumes: vec![],
            extra_disks: vec![],
            nfs_shares: vec![],
            health_check_url: None,
            snapshot_name: None,
            process_type: Some(ProcessType::Vm),
            serve_pid: None,
            original_vsock_vm_id: None,
            port_mappings: vec![],
            forward_localhost: vec![],
            network_mode: Default::default(),
            ipv6_prefix: None,
            tty: false,
            interactive: false,
            labels: std::collections::HashMap::new(),
            hugepages: false,
            portable_volumes: false,
            user: None,
            username: None,
            health_check_timeout: 5,
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
