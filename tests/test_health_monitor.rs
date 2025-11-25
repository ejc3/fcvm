use chrono::Utc;
use fcvm::health::spawn_health_monitor;
use fcvm::state::{HealthStatus, StateManager, VmConfig, VmState, VmStatus};
use tempfile::TempDir;
use tokio::time::{sleep, Duration};

#[tokio::test]
async fn test_health_monitor_lifecycle() {
    let temp_dir = TempDir::new().unwrap();

    // Set FCVM_BASE_DIR so spawn_health_monitor uses same state directory
    std::env::set_var("FCVM_BASE_DIR", temp_dir.path());

    let manager = StateManager::new(temp_dir.path().join("state"));
    manager.init().await.unwrap();

    let now = Utc::now();

    // Create a VM state without a real process
    let state = VmState {
        schema_version: 1,
        vm_id: "health-test-vm".to_string(),
        name: Some("health-test".to_string()),
        status: VmStatus::Running,
        health_status: HealthStatus::Unknown,
        pid: Some(99999), // Non-existent PID
        created_at: now,
        last_updated: now,
        config: VmConfig {
            image: "test:latest".to_string(),
            vcpu: 1,
            memory_mib: 256,
            network: serde_json::json!({
                "guest_ip": "192.168.1.100",
                "host_veth": "veth-test"
            }),
            volumes: vec![],
            env: vec![],
            health_check_path: "/health".to_string(),
            snapshot_name: None,
            process_type: Some("vm".to_string()),
            serve_pid: None,
        },
    };

    // Save initial state
    manager.save_state(&state).await.unwrap();

    // Spawn health monitor
    let handle = spawn_health_monitor("health-test-vm".to_string(), Some(99999));

    // Poll for health status update (no arbitrary sleeps!)
    // Health monitor should detect the missing PID and mark as Unreachable
    let updated_state = loop {
        sleep(Duration::from_millis(50)).await;
        let state = manager.load_state("health-test-vm").await.unwrap();

        // Break when health status changes from Unknown
        if state.health_status != HealthStatus::Unknown {
            break state;
        }

        // Timeout after 1 second (health check runs every 100ms)
        // This should be more than enough time
        if state
            .last_updated
            .signed_duration_since(now)
            .num_milliseconds()
            > 1000
        {
            panic!("Health monitor did not update status within 1 second");
        }
    };

    // Cancel the health monitor
    handle.abort();

    // Verify task was cancelled
    let result = handle.await;
    assert!(result.is_err()); // Should be cancelled

    // Since PID doesn't exist, health should be Unreachable (not Unknown)
    // The health monitor should have detected the missing PID
    assert_ne!(updated_state.health_status, HealthStatus::Unknown);
    assert_eq!(updated_state.health_status, HealthStatus::Unreachable);
}

#[tokio::test]
async fn test_health_monitor_cancellation() {
    // Test that health monitor can be properly cancelled
    let handle = spawn_health_monitor("cancel-test".to_string(), None);

    // Cancel immediately
    handle.abort();

    // Should complete with cancellation error
    let result = handle.await;
    assert!(result.is_err());
    assert!(result.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn test_multiple_health_monitors() {
    // Test that multiple health monitors can run independently
    let handles = vec![
        spawn_health_monitor("vm-1".to_string(), Some(1001)),
        spawn_health_monitor("vm-2".to_string(), Some(1002)),
        spawn_health_monitor("vm-3".to_string(), Some(1003)),
    ];

    // Let them run briefly
    sleep(Duration::from_millis(100)).await;

    // Cancel all monitors
    for handle in handles {
        handle.abort();
        let _ = handle.await; // Ignore cancellation errors
    }

    // Test passes if no panic occurred
}
