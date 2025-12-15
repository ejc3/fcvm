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
                health_check_port: None,
                health_check_url: None,
                dns_server: None,
            },
            volumes: vec![],
            env: vec![],
            health_check_url: None,
            snapshot_name: None,
            process_type: Some(ProcessType::Vm),
            serve_pid: None,
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

    // Verify file permissions are restrictive (Unix only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let state_file = temp_dir.path().join("test-vm-1.json");
        let metadata = std::fs::metadata(&state_file).unwrap();
        let permissions = metadata.permissions();
        // Check that only owner can read/write (0o600)
        assert_eq!(permissions.mode() & 0o777, 0o600);
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
                env: vec![],
                health_check_url: None,
                snapshot_name: None,
                process_type: Some(ProcessType::Vm),
                serve_pid: None,
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
