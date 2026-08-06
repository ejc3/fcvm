//! Integration tests for fc-mock (Firecracker mock binary)
//!
//! Tests verify that fc-mock correctly emulates the Firecracker API and
//! can be substituted for real Firecracker to run containers without KVM.
//!
//! Tests auto-skip if fc-mock binary is not found.
//! Build with: cargo build --release -p fc-mock

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::Duration;

/// Find the fc-mock binary. Returns None if not found (test will skip).
///
/// Checks /usr/local/bin first (installed by `make build-fc-mock`), which is
/// accessible from within user namespaces (rootless networking). Falls back
/// to the cargo target directory for direct testing.
fn find_fc_mock_binary() -> Option<PathBuf> {
    for path in &[
        PathBuf::from("/usr/local/bin/fc-mock"),
        PathBuf::from("./target/release/fc-mock"),
        PathBuf::from("./target/debug/fc-mock"),
    ] {
        if path.exists() {
            return Some(path.clone());
        }
    }
    None
}

/// Verify fc-mock --version outputs a valid Firecracker version string.
///
/// fcvm requires version >= 1.13.1 (parse_firecracker_version in common.rs:357).
/// The version must match the pattern `v?(\d+)\.(\d+)\.(\d+)`.
#[tokio::test]
async fn test_fc_mock_version() -> Result<()> {
    let fc_mock = match find_fc_mock_binary() {
        Some(p) => p,
        None => {
            println!("SKIP: fc-mock binary not found");
            return Ok(());
        }
    };

    println!("\nfc-mock version test");
    println!("====================");

    let output = tokio::process::Command::new(&fc_mock)
        .arg("--version")
        .output()
        .await
        .context("running fc-mock --version")?;

    assert!(output.status.success(), "fc-mock --version should exit 0");

    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    println!("  Version output: {}", version);

    // Must contain "Firecracker" (fcvm checks for this)
    assert!(
        version.contains("Firecracker"),
        "version should contain 'Firecracker', got: {}",
        version
    );

    // Must contain version pattern vX.Y.Z where major >= 1 and minor >= 13
    assert!(
        version.contains("v1.14.") || version.contains("v1.13.") || version.contains("v2."),
        "version should be >= v1.13.1, got: {}",
        version
    );

    println!("✅ FC-MOCK VERSION TEST PASSED!");
    Ok(())
}

/// Verify fc-mock API server accepts all Firecracker endpoints with 204.
///
/// Starts fc-mock directly with --api-sock and sends raw HTTP requests
/// to verify all endpoints return 204 No Content.
#[tokio::test]
async fn test_fc_mock_api_accepts_all_endpoints() -> Result<()> {
    let fc_mock = match find_fc_mock_binary() {
        Some(p) => p,
        None => {
            println!("SKIP: fc-mock binary not found");
            return Ok(());
        }
    };

    println!("\nfc-mock API acceptance test");
    println!("===========================");

    let socket_path = format!("/tmp/fc-mock-api-test-{}.sock", std::process::id());

    // Start fc-mock
    let mut child = tokio::process::Command::new(&fc_mock)
        .args(["--api-sock", &socket_path])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("spawning fc-mock")?;

    let mock_pid = child.id().context("getting fc-mock PID")?;

    // Wait for socket to appear
    let start = std::time::Instant::now();
    while !std::path::Path::new(&socket_path).exists() {
        if start.elapsed() > Duration::from_secs(5) {
            child.kill().await.ok();
            anyhow::bail!("fc-mock socket didn't appear within 5s");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    println!("  fc-mock started (PID: {})", mock_pid);

    // Config endpoints (store-and-ignore)
    let config_endpoints = [
        (
            "PUT",
            "/boot-source",
            r#"{"kernel_image_path":"vmlinux","boot_args":"console=ttyS0"}"#,
        ),
        (
            "PUT",
            "/machine-config",
            r#"{"vcpu_count":1,"mem_size_mib":128}"#,
        ),
        (
            "PUT",
            "/drives/rootfs",
            r#"{"drive_id":"rootfs","path_on_host":"/dev/null","is_root_device":true,"is_read_only":false}"#,
        ),
        (
            "PUT",
            "/network-interfaces/eth0",
            r#"{"iface_id":"eth0","host_dev_name":"tap0"}"#,
        ),
        ("PUT", "/mmds/config", r#"{"network_interfaces":["eth0"]}"#),
        ("PUT", "/entropy", r#"{"rate_limiter":{}}"#),
        (
            "PUT",
            "/balloon",
            r#"{"amount_mib":0,"stats_polling_interval_s":0}"#,
        ),
    ];

    for (method, path, body) in &config_endpoints {
        let status = send_http_request(&socket_path, method, path, body)
            .await
            .with_context(|| format!("{} {}", method, path))?;
        assert_eq!(
            status, 204,
            "{} {} should return 204, got {}",
            method, path, status
        );
        println!("  ✓ {} {} → 204", method, path);
    }

    // Vsock config (stores uds_path for exec listener)
    let status = send_http_request(
        &socket_path,
        "PUT",
        "/vsock",
        r#"{"guest_cid":3,"uds_path":"/tmp/fc-mock-test-vsock.sock"}"#,
    )
    .await?;
    assert_eq!(status, 204);
    println!("  ✓ PUT /vsock → 204");

    // InstanceStart action - triggers launch_container which fails harmlessly
    // because no MMDS data has been stored yet (PUT /mmds not called).
    // The HTTP response is still 204; the error is logged in a spawned task.
    let status = send_http_request(
        &socket_path,
        "PUT",
        "/actions",
        r#"{"action_type":"InstanceStart"}"#,
    )
    .await?;
    assert_eq!(status, 204);
    println!("  ✓ PUT /actions (InstanceStart) → 204");

    // MMDS data endpoints (after InstanceStart so the launch doesn't try to run a container)
    let status = send_http_request(
        &socket_path,
        "PUT",
        "/mmds",
        r#"{"latest":{"container-plan":{"image":"alpine:latest"}}}"#,
    )
    .await?;
    assert_eq!(status, 204);
    println!("  ✓ PUT /mmds → 204");

    let status = send_http_request(
        &socket_path,
        "PATCH",
        "/mmds",
        r#"{"latest":{"extra":"data"}}"#,
    )
    .await?;
    assert_eq!(status, 204);
    println!("  ✓ PATCH /mmds → 204");

    // State management
    let status = send_http_request(&socket_path, "PATCH", "/vm", r#"{"state":"Paused"}"#).await?;
    assert_eq!(status, 204);
    println!("  ✓ PATCH /vm → 204");

    let status = send_http_request(
        &socket_path,
        "PATCH",
        "/drives/rootfs",
        r#"{"drive_id":"rootfs","path_on_host":"/dev/null"}"#,
    )
    .await?;
    assert_eq!(status, 204);
    println!("  ✓ PATCH /drives/rootfs → 204");

    // Snapshot stubs
    let status = send_http_request(
        &socket_path,
        "PUT",
        "/snapshot/create",
        r#"{"snapshot_type":"Full","snapshot_path":"s","mem_file_path":"m"}"#,
    )
    .await?;
    assert_eq!(status, 204);
    println!("  ✓ PUT /snapshot/create → 204");

    let status = send_http_request(
        &socket_path,
        "PUT",
        "/snapshot/load",
        r#"{"snapshot_path":"s","mem_backend":{"backend_type":"Uffd","backend_path":"sock"}}"#,
    )
    .await?;
    assert_eq!(status, 204);
    println!("  ✓ PUT /snapshot/load → 204");

    // Cleanup
    common::kill_process(mock_pid).await;
    let _ = std::fs::remove_file(&socket_path);

    println!("✅ FC-MOCK API ACCEPTANCE TEST PASSED! (15 endpoints verified)");
    Ok(())
}

/// Verify the vsock CONNECT exec protocol works.
///
/// Starts fc-mock, sends InstanceStart with valid MMDS + vsock config,
/// waits for the exec listener socket, then verifies the CONNECT 4998
/// handshake and exec request/response protocol.
#[tokio::test]
async fn test_fc_mock_exec_connect_protocol() -> Result<()> {
    let fc_mock = match find_fc_mock_binary() {
        Some(p) => p,
        None => {
            println!("SKIP: fc-mock binary not found");
            return Ok(());
        }
    };

    println!("\nfc-mock exec CONNECT protocol test");
    println!("====================================");

    let socket_path = format!("/tmp/fc-mock-exec-test-{}.sock", std::process::id());
    let vsock_path = format!("/tmp/fc-mock-exec-test-vsock-{}", std::process::id());
    let status_socket = format!("{}_4999", vsock_path);

    // Create a dummy status listener so fc-mock can connect to it
    let _ = std::fs::remove_file(&status_socket);
    let status_listener =
        tokio::net::UnixListener::bind(&status_socket).context("binding status listener")?;

    // Start fc-mock
    let mut child = tokio::process::Command::new(&fc_mock)
        .args(["--api-sock", &socket_path])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawning fc-mock")?;

    let mock_pid = child.id().context("getting fc-mock PID")?;

    // Wait for API socket
    let start = std::time::Instant::now();
    while !std::path::Path::new(&socket_path).exists() {
        if start.elapsed() > Duration::from_secs(5) {
            child.kill().await.ok();
            anyhow::bail!("fc-mock socket didn't appear within 5s");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    println!("  fc-mock started (PID: {})", mock_pid);

    // Configure MMDS with a container plan that will exit quickly
    send_http_request(
        &socket_path,
        "PUT",
        "/mmds",
        &format!(
            r#"{{"latest":{{"container-plan":{{"image":"{}","cmd":["sleep","300"]}}}}}}"#,
            common::ALPINE_IMAGE
        ),
    )
    .await?;
    println!("  MMDS configured");

    // Configure vsock
    send_http_request(
        &socket_path,
        "PUT",
        "/vsock",
        &format!(r#"{{"guest_cid":3,"uds_path":"{}"}}"#, vsock_path),
    )
    .await?;
    println!("  Vsock configured (uds_path: {})", vsock_path);

    // Trigger InstanceStart
    send_http_request(
        &socket_path,
        "PUT",
        "/actions",
        r#"{"action_type":"InstanceStart"}"#,
    )
    .await?;
    println!("  InstanceStart sent");

    // Accept status connection and read "ready"
    let (status_stream, _) =
        tokio::time::timeout(Duration::from_secs(30), status_listener.accept())
            .await
            .context("timeout waiting for status connection")?
            .context("accepting status connection")?;
    println!("  Status socket connected");

    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut status_reader = BufReader::new(status_stream);
    let mut ready_line = String::new();
    tokio::time::timeout(
        Duration::from_secs(30),
        status_reader.read_line(&mut ready_line),
    )
    .await
    .context("timeout reading ready")?
    .context("reading ready line")?;
    assert_eq!(
        ready_line.trim(),
        "ready",
        "status socket should receive 'ready', got: {:?}",
        ready_line
    );
    println!("  Received 'ready' on status socket");

    // Now test the exec CONNECT protocol on the vsock socket
    // Wait for vsock socket to appear (exec listener creates it)
    let start = std::time::Instant::now();
    while !std::path::Path::new(&vsock_path).exists() {
        if start.elapsed() > Duration::from_secs(10) {
            anyhow::bail!("vsock exec socket didn't appear within 10s");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    println!("  Vsock exec socket available");

    // Connect and test CONNECT protocol
    use tokio::io::AsyncWriteExt;
    let stream = tokio::net::UnixStream::connect(&vsock_path)
        .await
        .context("connecting to vsock exec socket")?;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // Send CONNECT 4998
    write_half.write_all(b"CONNECT 4998\n").await?;
    println!("  Sent: CONNECT 4998");

    // Read OK 4998
    let mut ok_line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut ok_line))
        .await
        .context("timeout reading OK")?
        .context("reading OK line")?;
    assert_eq!(
        ok_line.trim(),
        "OK 4998",
        "should get 'OK 4998', got: {:?}",
        ok_line
    );
    println!("  Received: OK 4998");

    // Send exec request (run echo command directly, not in container)
    let exec_request = serde_json::json!({
        "command": ["echo", "hello-from-exec-protocol"],
        "in_container": false,
        "interactive": false,
        "tty": false
    });
    let request_json = serde_json::to_string(&exec_request)?;
    write_half
        .write_all(format!("{}\n", request_json).as_bytes())
        .await?;
    println!("  Sent exec request: {}", request_json);

    // Three-phase handshake: the server ACKs the consumed request and executes
    // only after our GO line (see exec_proto::HANDSHAKE_ACK).
    let mut ack_line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut ack_line))
        .await
        .context("timeout reading ACK")?
        .context("reading ACK line")?;
    assert_eq!(
        ack_line.trim(),
        exec_proto::HANDSHAKE_ACK,
        "should get ACK before any exec response, got: {:?}",
        ack_line
    );
    println!("  Received: ACK");
    write_half
        .write_all(format!("{}\n", exec_proto::HANDSHAKE_GO).as_bytes())
        .await?;
    println!("  Sent: GO");

    // Read responses (stdout, then exit)
    let mut got_stdout = false;
    let mut got_exit = false;
    let mut stdout_data = String::new();
    let mut exit_code: Option<i32> = None;

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if std::time::Instant::now() > deadline {
            anyhow::bail!("timeout reading exec responses");
        }

        let mut line = String::new();
        match tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(_)) => {
                let resp: serde_json::Value =
                    serde_json::from_str(line.trim()).context("parsing exec response JSON")?;
                match resp["type"].as_str() {
                    Some("stdout") => {
                        stdout_data = resp["data"].as_str().unwrap_or("").to_string();
                        got_stdout = true;
                        println!("  Received stdout: {:?}", stdout_data);
                    }
                    Some("exit") => {
                        exit_code = resp["data"].as_i64().map(|v| v as i32);
                        got_exit = true;
                        println!("  Received exit: {:?}", exit_code);
                        break;
                    }
                    Some(t) => println!("  Received {}: {:?}", t, resp["data"]),
                    None => println!("  Unknown response: {}", line.trim()),
                }
            }
            Ok(Err(e)) => anyhow::bail!("read error: {}", e),
            Err(_) => anyhow::bail!("timeout reading response line"),
        }
    }

    assert!(got_stdout, "should have received stdout response");
    assert!(
        stdout_data.contains("hello-from-exec-protocol"),
        "stdout should contain our echo, got: {:?}",
        stdout_data
    );
    assert!(got_exit, "should have received exit response");
    assert_eq!(exit_code, Some(0), "exit code should be 0");

    // Cleanup
    common::kill_process(mock_pid).await;
    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(&vsock_path);
    let _ = std::fs::remove_file(&status_socket);

    println!("✅ FC-MOCK EXEC CONNECT PROTOCOL TEST PASSED!");
    Ok(())
}

/// Full end-to-end: fcvm launches with fc-mock, container runs and exits cleanly.
///
/// Uses FCVM_FIRECRACKER_BIN to substitute fc-mock for real Firecracker.
/// Runs a container that exits immediately to verify the full lifecycle.
/// Requires root: uses --network bridged (rootless can't remount btrfs in user ns).
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_fc_mock_container_launch() -> Result<()> {
    let fc_mock = match find_fc_mock_binary() {
        Some(p) => p,
        None => {
            println!("SKIP: fc-mock binary not found");
            return Ok(());
        }
    };

    println!("\nfc-mock container launch test");
    println!("==============================");

    let (vm_name, _, _, _) = common::unique_names("fc-mock-launch");
    let fc_mock_path = fc_mock
        .canonicalize()
        .context("canonicalizing fc-mock path")?;

    // Use --network bridged (not rootless) because rootless creates a user namespace
    // where podman can't remount its storage (btrfs). Bridged mode only creates a
    // network namespace, which fc-mock's podman can handle fine.
    println!("  Starting fcvm with fc-mock as Firecracker binary...");
    let (mut child, fcvm_pid) = common::spawn_fcvm_with_env(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "bridged",
            common::ALPINE_IMAGE,
            "echo",
            "fc-mock-works",
        ],
        &[("FCVM_FIRECRACKER_BIN", fc_mock_path.to_str().unwrap())],
    )
    .await
    .context("spawning fcvm with fc-mock")?;

    println!("  fcvm PID: {}", fcvm_pid);
    println!("  Waiting for process to exit (max 120s)...");

    let status = tokio::time::timeout(Duration::from_secs(120), child.wait())
        .await
        .context("timeout waiting for fcvm")?
        .context("waiting for child")?;

    println!("  Exit status: {}", status);
    assert!(
        status.success(),
        "fcvm with fc-mock should exit successfully, got: {}",
        status
    );

    println!("✅ FC-MOCK CONTAINER LAUNCH TEST PASSED!");
    Ok(())
}

/// Most important test: fcvm with fc-mock becomes healthy, exec works.
///
/// Verifies the full flow:
/// 1. fcvm starts fc-mock instead of Firecracker
/// 2. fc-mock launches container via podman
/// 3. Health monitor detects container as healthy
/// 4. Container exec works (via podman exec fcvm-container)
/// 5. VM-level exec works (direct command execution)
/// Requires root: uses --network bridged (rootless can't remount btrfs in user ns).
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_fc_mock_sanity() -> Result<()> {
    let fc_mock = match find_fc_mock_binary() {
        Some(p) => p,
        None => {
            println!("SKIP: fc-mock binary not found");
            return Ok(());
        }
    };

    println!("\nfc-mock sanity test (health + exec)");
    println!("====================================");

    let (vm_name, _, _, _) = common::unique_names("fc-mock-sanity");
    let fc_mock_path = fc_mock
        .canonicalize()
        .context("canonicalizing fc-mock path")?;

    // Use --network bridged (not rootless) because rootless creates a user namespace
    // where podman can't remount its storage (btrfs). Bridged mode only creates a
    // network namespace, which fc-mock's podman can handle fine.
    println!("  Starting fcvm with fc-mock + nginx...");
    let (mut _child, fcvm_pid) = common::spawn_fcvm_with_env(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "bridged",
            common::TEST_IMAGE,
        ],
        &[("FCVM_FIRECRACKER_BIN", fc_mock_path.to_str().unwrap())],
    )
    .await
    .context("spawning fcvm with fc-mock")?;

    println!("  fcvm PID: {}", fcvm_pid);

    // Wait for health to become healthy
    println!("  Waiting for health check (max 120s)...");
    if let Err(e) = common::poll_health_by_pid(fcvm_pid, 120).await {
        common::kill_process(fcvm_pid).await;
        return Err(e.context("VM failed to become healthy with fc-mock"));
    }
    println!("  Health: Healthy!");

    // Test exec in container (runs via podman exec fcvm-container)
    println!("  Testing exec in container...");
    let output = common::exec_in_container(fcvm_pid, &["cat", "/etc/os-release"]).await?;
    let first_line = output.lines().next().unwrap_or("");
    println!("  Container OS: {}", first_line);
    assert!(
        output.contains("Alpine"),
        "container should be Alpine Linux, got: {}",
        output
    );

    // Test exec on VM level (runs directly on host via fc-mock)
    println!("  Testing VM-level exec...");
    let output = common::exec_in_vm(fcvm_pid, &["echo", "hello-from-fc-mock"]).await?;
    assert!(
        output.contains("hello-from-fc-mock"),
        "VM-level exec should work via fc-mock, got: {}",
        output
    );

    // Cleanup
    println!("  Stopping...");
    common::kill_process(fcvm_pid).await;

    println!("✅ FC-MOCK SANITY TEST PASSED!");
    println!("  Health checks work with fc-mock substitute");
    println!("  Container exec works via podman exec");
    println!("  VM-level exec works via direct execution");
    Ok(())
}

/// Send a raw HTTP request over a Unix socket and return the status code.
///
/// Uses HTTP/1.0 so the server closes the connection after the response,
/// making read_to_end() work without hanging.
async fn send_http_request(socket_path: &str, method: &str, path: &str, body: &str) -> Result<u16> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("connecting to {}", socket_path))?;

    let request = format!(
        "{} {} HTTP/1.0\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        method, path, body.len(), body
    );

    stream.write_all(request.as_bytes()).await?;

    // Read response with timeout
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
        .await
        .context("timeout reading HTTP response")?
        .context("reading HTTP response")?;

    let response_str = String::from_utf8_lossy(&response);
    let status_line = response_str
        .lines()
        .next()
        .context("empty response from server")?;

    let status_code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .context("no status code in response")?
        .parse()
        .context("invalid status code")?;

    Ok(status_code)
}
