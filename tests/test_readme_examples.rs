//! Tests for README examples
//!
//! Verifies that examples shown in README.md actually work.
//! Each test corresponds to a specific example or feature documented.
//!
//! Tests use unique names via `common::unique_names()` to allow parallel execution.
//!
//! IMPORTANT: All tests use `common::spawn_fcvm()` helper which uses
//! `Stdio::inherit()` to prevent pipe buffer deadlock. See CLAUDE.md
//! "Pipe Buffer Deadlock in Tests" for details.

#![cfg(all(feature = "integration-fast", feature = "privileged-tests"))]

mod common;

use anyhow::{Context, Result};
use serde::Deserialize;
use std::time::Duration;

/// Test read-only volume mapping (--map /host:/guest:ro)
///
/// README example:
/// ```
/// sudo fcvm podman run --name web1 --map /host/config:/config:ro nginx:alpine
/// ```
#[tokio::test]
async fn test_readonly_volume_bridged() -> Result<()> {
    println!("\ntest_readonly_volume_bridged");
    println!("============================");

    let (vm_name, _, _, _) = common::unique_names("ro-vol");
    let test_id = vm_name.clone();

    // Create test directory with a file
    let host_dir = format!("/tmp/{}", test_id);
    tokio::fs::create_dir_all(&host_dir).await?;
    tokio::fs::write(format!("{}/readonly.txt", host_dir), "original content").await?;

    // Start VM with read-only volume using bridged mode
    let map_arg = format!("{}:/config:ro", host_dir);
    let (mut child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "--map",
        &map_arg,
        common::TEST_IMAGE,
    ])
    .await
    .context("spawning fcvm")?;
    println!("Started VM with PID: {}", fcvm_pid);

    // Wait for healthy (longer timeout for FUSE volume setup - vsock connection + mount takes time)
    if let Err(e) = common::poll_health_by_pid(fcvm_pid, 180).await {
        common::kill_process(fcvm_pid).await;
        let _ = tokio::fs::remove_dir_all(&host_dir).await;
        return Err(e.context("VM failed to become healthy"));
    }
    println!("VM is healthy");

    // Test 1: Read should work
    println!("Test 1: Reading from read-only mount...");
    let output = common::exec_in_container(fcvm_pid, &["cat", "/config/readonly.txt"]).await?;
    assert!(
        output.contains("original content"),
        "Should be able to read file, got: {}",
        output
    );
    println!("  Read OK: {}", output.trim());

    // Test 2: Write should fail (will get EROFS - read-only file system)
    println!("Test 2: Writing to read-only mount should fail...");
    let result = common::exec_in_container(
        fcvm_pid,
        &[
            "sh",
            "-c",
            "echo 'new' > /config/readonly.txt 2>&1 || echo WRITE_FAILED",
        ],
    )
    .await;

    // Verify write was blocked
    match result {
        Ok(output) => {
            assert!(
                output.contains("WRITE_FAILED")
                    || output.contains("Read-only")
                    || output.contains("read-only"),
                "Write should fail on read-only mount, got: {}",
                output
            );
        }
        Err(_) => {
            // Command failing is also acceptable
        }
    }

    // Verify the file wasn't modified on host
    let content = tokio::fs::read_to_string(format!("{}/readonly.txt", host_dir)).await?;
    assert_eq!(content, "original content", "File should not be modified");
    println!("  Write correctly blocked, file unchanged");

    // Cleanup
    common::kill_process(fcvm_pid).await;
    let _ = child.wait().await;
    let _ = tokio::fs::remove_dir_all(&host_dir).await;

    println!("✅ test_readonly_volume_bridged PASSED");
    Ok(())
}

/// Test environment variables (--env KEY=VALUE)
///
/// README example:
/// ```
/// sudo fcvm podman run --name web1 --env DEBUG=1 nginx:alpine
/// ```
#[tokio::test]
async fn test_env_variables_bridged() -> Result<()> {
    println!("\ntest_env_variables_bridged");
    println!("==========================");

    let (vm_name, _, _, _) = common::unique_names("env-test");

    // Start VM with environment variables using bridged mode for reliable health checks
    let (mut child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "--env",
        "MY_VAR=hello_world",
        "--env",
        "DEBUG=1",
        "--env",
        "COMPLEX_VAR=value with spaces",
        common::TEST_IMAGE,
    ])
    .await
    .context("spawning fcvm")?;
    println!("Started VM with PID: {}", fcvm_pid);

    // Wait for healthy with longer timeout for env var processing
    if let Err(e) = common::poll_health_by_pid(fcvm_pid, 120).await {
        common::kill_process(fcvm_pid).await;
        return Err(e.context("VM failed to become healthy"));
    }
    println!("VM is healthy");

    // Test 1: Simple env var (use echo $VAR - the sh -c wrapper is added by exec_in_container)
    println!("Test 1: Check MY_VAR...");
    let output = common::exec_in_container(fcvm_pid, &["echo", "$MY_VAR"]).await?;
    assert!(
        output.trim() == "hello_world",
        "MY_VAR should be 'hello_world', got: '{}'",
        output.trim()
    );
    println!("  MY_VAR = {}", output.trim());

    // Test 2: DEBUG var
    println!("Test 2: Check DEBUG...");
    let output = common::exec_in_container(fcvm_pid, &["echo", "$DEBUG"]).await?;
    assert!(
        output.trim() == "1",
        "DEBUG should be '1', got: '{}'",
        output.trim()
    );
    println!("  DEBUG = {}", output.trim());

    // Test 3: Complex var with spaces
    println!("Test 3: Check COMPLEX_VAR...");
    let output = common::exec_in_container(fcvm_pid, &["echo", "$COMPLEX_VAR"]).await?;
    assert!(
        output.trim() == "value with spaces",
        "COMPLEX_VAR should be 'value with spaces', got: '{}'",
        output.trim()
    );
    println!("  COMPLEX_VAR = {}", output.trim());

    // Cleanup
    common::kill_process(fcvm_pid).await;
    let _ = child.wait().await;

    println!("✅ test_env_variables_bridged PASSED");
    Ok(())
}

/// Test custom CPU and memory resources (--cpu N --mem MiB)
///
/// README example:
/// ```
/// sudo fcvm podman run --name web1 --cpu 4 --mem 4096 nginx:alpine
/// ```
#[tokio::test]
async fn test_custom_resources_bridged() -> Result<()> {
    println!("\ntest_custom_resources_bridged");
    println!("=============================");

    let (vm_name, _, _, _) = common::unique_names("resources-test");

    // Start VM with custom resources using bridged mode for reliable health checks
    let (mut child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "--cpu",
        "4",
        "--mem",
        "1024",
        common::TEST_IMAGE,
    ])
    .await
    .context("spawning fcvm")?;
    println!("Started VM with PID: {}", fcvm_pid);

    // Wait for healthy
    if let Err(e) = common::poll_health_by_pid(fcvm_pid, 120).await {
        common::kill_process(fcvm_pid).await;
        return Err(e.context("VM failed to become healthy"));
    }
    println!("VM is healthy");

    // Test 1: Check CPU count (via /proc/cpuinfo in VM)
    println!("Test 1: Check CPU count...");
    let output =
        common::exec_in_vm(fcvm_pid, &["grep", "-c", "^processor", "/proc/cpuinfo"]).await?;
    let cpu_count: i32 = output.trim().parse().unwrap_or(0);
    assert_eq!(cpu_count, 4, "Should have 4 CPUs, got: {}", cpu_count);
    println!("  CPU count = {}", cpu_count);

    // Test 2: Check memory (via /proc/meminfo in VM)
    println!("Test 2: Check memory...");
    let output = common::exec_in_vm(fcvm_pid, &["grep", "MemTotal", "/proc/meminfo"]).await?;
    println!("  MemTotal: {}", output.trim());

    // Parse memory - should be around 1024 MB (some overhead is normal)
    // MemTotal format: "MemTotal:       1015432 kB"
    let mem_kb: i64 = output
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mem_mb = mem_kb / 1024;

    // Allow 5% tolerance for kernel overhead
    assert!(
        (950..=1100).contains(&mem_mb),
        "Memory should be ~1024 MB, got: {} MB",
        mem_mb
    );
    println!("  Memory = {} MB (requested 1024 MB)", mem_mb);

    // Cleanup
    common::kill_process(fcvm_pid).await;
    let _ = child.wait().await;

    println!("✅ test_custom_resources_bridged PASSED");
    Ok(())
}

/// Test fcvm ls command variants
///
/// README examples:
/// ```
/// fcvm ls
/// fcvm ls --json
/// fcvm ls --pid 12345
/// ```
#[tokio::test]
async fn test_fcvm_ls_bridged() -> Result<()> {
    println!("\ntest_fcvm_ls_bridged");
    println!("====================");

    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names("ls-test");

    // Start a VM to list using bridged mode for reliable health checks
    let (mut child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        common::TEST_IMAGE,
    ])
    .await
    .context("spawning fcvm")?;
    println!("Started VM with PID: {}", fcvm_pid);

    // Wait for healthy
    if let Err(e) = common::poll_health_by_pid(fcvm_pid, 120).await {
        common::kill_process(fcvm_pid).await;
        return Err(e.context("VM failed to become healthy"));
    }
    println!("VM is healthy");

    // Test 1: Basic fcvm ls (text output)
    println!("\nTest 1: fcvm ls (text output)...");
    let output = tokio::process::Command::new(&fcvm_path)
        .args(["ls"])
        .output()
        .await
        .context("running fcvm ls")?;

    assert!(output.status.success(), "fcvm ls should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("  Output:\n{}", stdout);

    // Should contain header and our VM
    assert!(
        stdout.contains("NAME") || stdout.contains("PID"),
        "ls output should have headers"
    );
    assert!(
        stdout.contains(&vm_name) || stdout.contains(&fcvm_pid.to_string()),
        "ls output should contain our VM"
    );

    // Test 2: fcvm ls --json
    println!("\nTest 2: fcvm ls --json...");
    let output = tokio::process::Command::new(&fcvm_path)
        .args(["ls", "--json"])
        .output()
        .await
        .context("running fcvm ls --json")?;

    assert!(output.status.success(), "fcvm ls --json should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should be valid JSON array
    #[derive(Deserialize)]
    struct VmDisplay {
        #[serde(flatten)]
        vm: fcvm::state::VmState,
        #[allow(dead_code)]
        stale: bool,
    }

    let vms: Vec<VmDisplay> = serde_json::from_str(&stdout).context("JSON should be valid")?;

    println!("  Found {} VMs in JSON output", vms.len());
    assert!(!vms.is_empty(), "Should have at least one VM");

    // Test 3: fcvm ls --pid (filter by PID)
    println!("\nTest 3: fcvm ls --pid {}...", fcvm_pid);
    let output = tokio::process::Command::new(&fcvm_path)
        .args(["ls", "--json", "--pid", &fcvm_pid.to_string()])
        .output()
        .await
        .context("running fcvm ls --pid")?;

    assert!(output.status.success(), "fcvm ls --pid should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);

    let vms: Vec<VmDisplay> = serde_json::from_str(&stdout).context("JSON should be valid")?;

    assert_eq!(vms.len(), 1, "Should find exactly one VM with --pid filter");
    assert_eq!(
        vms[0].vm.pid,
        Some(fcvm_pid),
        "Filtered VM should match requested PID"
    );
    println!("  Correctly filtered to PID {}", fcvm_pid);

    // Test 4: fcvm ls --pid with non-existent PID
    println!("\nTest 4: fcvm ls --pid with non-existent PID...");
    let output = tokio::process::Command::new(&fcvm_path)
        .args(["ls", "--json", "--pid", "99999999"])
        .output()
        .await
        .context("running fcvm ls --pid")?;

    assert!(
        output.status.success(),
        "fcvm ls --pid should succeed even with no matches"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    let vms: Vec<VmDisplay> = serde_json::from_str(&stdout).context("JSON should be valid")?;

    assert!(vms.is_empty(), "Should find no VMs with non-existent PID");
    println!("  Correctly returned empty list for non-existent PID");

    // Cleanup
    common::kill_process(fcvm_pid).await;
    let _ = child.wait().await;

    println!("✅ test_fcvm_ls_bridged PASSED");
    Ok(())
}

/// Test --cmd custom command
///
/// README example:
/// ```
/// sudo fcvm podman run --name web1 --cmd "nginx -g 'daemon off;'" nginx:alpine
/// ```
#[tokio::test]
async fn test_custom_command_bridged() -> Result<()> {
    println!("\ntest_custom_command_bridged");
    println!("===========================");

    let (vm_name, _, _, _) = common::unique_names("cmd-test");

    // Use nginx:alpine with a custom command that:
    // 1. Creates a marker file to prove our command ran
    // 2. Then starts nginx normally (so health checks pass)
    // This matches the README pattern: --cmd "nginx -g 'daemon off;'"
    let custom_cmd = "sh -c 'echo CUSTOM_CMD_MARKER > /tmp/marker.txt && nginx -g \"daemon off;\"'";

    let (mut child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "--cmd",
        custom_cmd,
        common::TEST_IMAGE,
    ])
    .await
    .context("spawning fcvm")?;
    println!("Started VM with PID: {}", fcvm_pid);

    // Wait for healthy
    if let Err(e) = common::poll_health_by_pid(fcvm_pid, 120).await {
        common::kill_process(fcvm_pid).await;
        return Err(e.context("VM failed to become healthy"));
    }
    println!("VM is healthy");

    // Give the command a moment to execute
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Check that our custom command ran by looking for the marker file
    println!("Checking for marker file from custom command...");
    let output = common::exec_in_container(fcvm_pid, &["cat", "/tmp/marker.txt"]).await?;

    assert!(
        output.contains("CUSTOM_CMD_MARKER"),
        "Custom command should have created marker file, got: {}",
        output
    );
    println!("  Found marker: {}", output.trim());

    // Verify nginx is running by checking if we can curl localhost
    // (more reliable than pgrep which may not exist in alpine)
    println!("Checking nginx is serving requests...");
    let output =
        common::exec_in_container(fcvm_pid, &["wget", "-q", "-O", "-", "http://localhost/"]).await;
    assert!(
        output.is_ok(),
        "nginx should be responding to requests: {:?}",
        output.err()
    );
    println!("  nginx is serving requests");

    // Cleanup
    common::kill_process(fcvm_pid).await;
    let _ = child.wait().await;

    println!("✅ test_custom_command_bridged PASSED");
    Ok(())
}
