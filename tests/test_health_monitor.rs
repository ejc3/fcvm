use chrono::Utc;
use fcvm::health::spawn_health_monitor_with_state_dir;
use fcvm::network::NetworkConfig;
use fcvm::paths;
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
    let path = temp_dir.keep();
    // Rename to include unique suffix for debugging
    let unique_path = std::path::PathBuf::from(format!("/tmp/fcvm-test-health-{}-{}", pid, id));
    let _ = std::fs::remove_dir_all(&unique_path);
    std::fs::rename(&path, &unique_path).unwrap_or_else(|_| {
        // If rename fails, just use original path
        std::fs::create_dir_all(&unique_path).ok();
    });
    unique_path
}

/// The state of a running VM with an HTTP health check on `health_check_url`.
fn vm_state(vm_id: &str, pid: u32, network: NetworkConfig, health_check_url: &str) -> VmState {
    let now = Utc::now();
    VmState {
        schema_version: 1,
        vm_id: vm_id.to_string(),
        name: Some("health-test".to_string()),
        status: VmStatus::Running,
        health_status: HealthStatus::Unknown,
        exit_code: None,
        pid: Some(pid),
        pid_start_time: None,
        lifecycle_ready: false,
        holder_pid: None,
        vsock_epoch: 0,
        created_at: now,
        last_updated: now,
        config: VmConfig {
            image: "test:latest".to_string(),
            vcpu: 1,
            memory_mib: 256,
            network,
            volumes: vec![],
            extra_disks: vec![],
            nfs_shares: vec![],
            health_check_url: Some(health_check_url.to_string()),
            snapshot_name: None,
            process_type: Some(ProcessType::Vm),
            serve_pid: None,
            uffd_mode: None,
            original_vsock_vm_id: None,
            vsock_socket_path: None,
            source_vsock_socket_path: None,
            port_mappings: vec![],
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
            hypervisor: Default::default(),
        },
    }
}

#[tokio::test]
async fn test_health_monitor_behaviors() {
    // Create unique temp directory for this test instance
    let base_dir = create_unique_test_dir();

    // Initialize paths module with test directory (required for paths::vm_runtime_dir calls)
    // OnceLock::set() is idempotent - safe to call multiple times
    paths::init_with_paths(&base_dir, &base_dir);

    // Use the shared base dir so the monitor and test agree on where state lives.
    let manager = StateManager::new(base_dir.join("state"));
    manager.init().await.unwrap();

    // Create a VM state without a real process: the PID is above /proc/sys/kernel/pid_max.
    let state = vm_state(
        "health-test-vm",
        4194305,
        NetworkConfig {
            tap_device: "tap-test".to_string(),
            guest_mac: "02:00:00:00:00:01".to_string(),
            guest_ip: Some("192.168.1.100".to_string()),
            host_ip: Some("192.168.1.1".to_string()),
            host_veth: Some("veth-test".to_string()),
            loopback_ip: None,
            dns_server: None,
            guest_ipv6: None,
            host_ipv6: None,
            dns_search: None,
            http_proxy: None,
            namespace_name: None,
        },
        "http://localhost/health",
    );

    // Save initial state
    manager.save_state(&state).await.unwrap();

    // Run a single health check iteration
    let status = fcvm::health::run_health_check_once(
        "health-test-vm",
        Some(4194305),
        base_dir.join("state"),
    )
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

/// Answer every request on `listener` with `status`, counting them in `hits`.
fn answer_with(
    listener: tokio::net::TcpListener,
    status: &'static str,
    hits: std::sync::Arc<AtomicUsize>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            hits.fetch_add(1, Ordering::SeqCst);
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request).await;
            let reply =
                format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = stream.write_all(reply.as_bytes()).await;
        }
    });
}

/// A bridged VM's HTTP health check goes to the VM's own veth address (#948).
///
/// Every VM restored from one snapshot has the snapshot's guest address, and the host's
/// route to that address belongs to whichever of them set up last. A probe aimed at the
/// guest address therefore reaches a sibling, or nothing. The address that is a VM's own is
/// the peer of the host end of its veth /30.
///
/// Loopback stands in for both, because all of 127/8 is local and binds with no setup. The
/// host end is 127.0.0.1, so the VM's own address is 127.0.0.2 and answers 200. 127.0.0.3
/// plays the shared guest address and answers 503.
#[tokio::test]
async fn bridged_health_check_goes_to_the_vms_own_veth_address() {
    let base_dir = create_unique_test_dir();
    paths::init_with_paths(&base_dir, &base_dir);
    let manager = StateManager::new(base_dir.join("state"));
    manager.init().await.unwrap();

    // The same port has to be free on both addresses.
    let (own, shared, port) = {
        let mut attempt = 0;
        loop {
            let own = tokio::net::TcpListener::bind("127.0.0.2:0")
                .await
                .expect("binding the VM's own address");
            let port = own.local_addr().unwrap().port();
            match tokio::net::TcpListener::bind(("127.0.0.3", port)).await {
                Ok(shared) => break (own, shared, port),
                Err(e) => {
                    attempt += 1;
                    assert!(attempt < 20, "no port free on both addresses: {e}");
                }
            }
        }
    };
    let own_hits = std::sync::Arc::new(AtomicUsize::new(0));
    let shared_hits = std::sync::Arc::new(AtomicUsize::new(0));
    answer_with(own, "200 OK", own_hits.clone());
    answer_with(shared, "503 Service Unavailable", shared_hits.clone());

    let state = vm_state(
        "own-address-vm",
        std::process::id(),
        NetworkConfig {
            guest_ip: Some("127.0.0.3".to_string()),
            host_ip: Some("127.0.0.1".to_string()),
            // No device to bind to: loopback has no veth.
            host_veth: None,
            ..Default::default()
        },
        &format!("http://localhost:{port}/health"),
    );
    manager.save_state(&state).await.unwrap();

    let status = fcvm::health::run_health_check_once(
        "own-address-vm",
        Some(std::process::id()),
        base_dir.join("state"),
    )
    .await
    .expect("health check should complete");

    assert_eq!(
        shared_hits.load(Ordering::SeqCst),
        0,
        "the probe went to the guest address, which a sibling restored from the same \
         snapshot may hold"
    );
    assert_eq!(status, HealthStatus::Healthy);
    assert_eq!(own_hits.load(Ordering::SeqCst), 1);
}
