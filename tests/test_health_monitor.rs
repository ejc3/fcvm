use chrono::Utc;
use fcvm::health::spawn_health_monitor_with_state_dir;
use fcvm::network::NetworkConfig;
use fcvm::paths;
use fcvm::state::{HealthStatus, ProcessType, StateManager, VmConfig, VmState, VmStatus};
use serial_test::serial;
use std::path::PathBuf;
use std::sync::OnceLock;
use tokio::time::{sleep, Duration};

/// Ensure all tests share a stable FCVM_BASE_DIR to avoid races from parallel execution.
fn init_test_base_dir() -> PathBuf {
    static BASE_DIR: OnceLock<PathBuf> = OnceLock::new();

    BASE_DIR
        .get_or_init(|| {
            let temp_dir = tempfile::tempdir().expect("create temp base dir");
            let path = temp_dir.into_path();

            // Configure paths module and env var before any health monitor tasks start.
            std::env::set_var("FCVM_BASE_DIR", &path);
            paths::init_base_dir(path.to_str());

            path
        })
        .clone()
}

#[tokio::test]
#[serial]
async fn test_health_monitor_behaviors() {
    // Ensure base dir is set before spawning the monitor (tests run in parallel).
    let base_dir = init_test_base_dir();
    assert_eq!(paths::base_dir(), base_dir);

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
        pid: Some(99999), // Non-existent PID
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
