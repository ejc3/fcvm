use chrono::Utc;
use fcvm::health::spawn_health_monitor_with_state_dir;
use fcvm::network::NetworkConfig;
use fcvm::state::{HealthStatus, ProcessType, StateManager, VmConfig, VmState, VmStatus};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::time::{sleep, Duration};

/// Counter for generating unique test IDs
static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Create a unique temp directory for this test instance
fn create_unique_test_dir() -> std::path::PathBuf {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let temp_dir = tempfile::tempdir().expect("create temp base dir");
    let path = temp_dir.into_path();
    // Rename to include unique suffix for debugging
    let unique_path = std::path::PathBuf::from(format!("/tmp/fcvm-test-health-{}-{}", pid, id));
    let _ = std::fs::remove_dir_all(&unique_path);
    std::fs::rename(&path, &unique_path).unwrap_or_else(|_| {
        // If rename fails, just use original path
        std::fs::create_dir_all(&unique_path).ok();
    });
    unique_path
}

#[tokio::test]
async fn test_health_monitor_behaviors() {
    // Create unique temp directory for this test instance
    let base_dir = create_unique_test_dir();

    // Use the shared base dir so the monitor and test agree on where state lives.
    let manager = StateManager::new(base_dir.join("state"));
    manager.init().await.unwrap();

    let now = Utc::now();

    // Create a VM state without a real process
    let state = VmState {
        schema_version: 1,
        vm_id: "health-test-vm".to_string(),
        name: Some("health-test".to_string()),
        status: VmStatus::Running,
        health_status: HealthStatus::Unknown,
        exit_code: None,
        pid: Some(99999), // Non-existent PID
        holder_pid: None,
        created_at: now,
        last_updated: now,
        config: VmConfig {
            image: "test:latest".to_string(),
            vcpu: 1,
            memory_mib: 256,
            network: NetworkConfig {
                tap_device: "tap-test".to_string(),
                guest_mac: "02:00:00:00:00:01".to_string(),
                guest_ip: Some("192.168.1.100".to_string()),
                host_ip: Some("192.168.1.1".to_string()),
                host_veth: Some("veth-test".to_string()),
                loopback_ip: None,
                health_check_port: None,
                health_check_url: Some("http://localhost/health".to_string()),
                dns_server: None,
            },
            volumes: vec![],
            env: vec![],
            health_check_url: Some("http://localhost/health".to_string()),
            snapshot_name: None,
            process_type: Some(ProcessType::Vm),
            serve_pid: None,
        },
    };

    // Save initial state
    manager.save_state(&state).await.unwrap();

    // Run a single health check iteration
    let status =
        fcvm::health::run_health_check_once("health-test-vm", Some(99999), base_dir.join("state"))
            .await
            .expect("health check should complete");

    // Since PID doesn't exist, health should be Unreachable (not Unknown)
    // The health monitor should have detected the missing PID
    let updated_state = manager.load_state("health-test-vm").await.unwrap();
    assert_ne!(updated_state.health_status, HealthStatus::Unknown);
    assert_eq!(updated_state.health_status, HealthStatus::Unreachable);
    assert_eq!(status, HealthStatus::Unreachable);

    // Test that health monitor can be properly cancelled
    let handle = spawn_health_monitor_with_state_dir(
        "cancel-test".to_string(),
        None,
        base_dir.join("state"),
    );

    // Cancel immediately
    handle.abort();

    // Should complete with cancellation error
    let result = handle.await;
    assert!(result.is_err());
    assert!(result.unwrap_err().is_cancelled());

    // Test that multiple health monitors can run independently
    let handles = vec![
        spawn_health_monitor_with_state_dir("vm-1".to_string(), Some(1001), base_dir.join("state")),
        spawn_health_monitor_with_state_dir("vm-2".to_string(), Some(1002), base_dir.join("state")),
        spawn_health_monitor_with_state_dir("vm-3".to_string(), Some(1003), base_dir.join("state")),
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
