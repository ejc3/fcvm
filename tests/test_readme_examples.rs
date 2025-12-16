//! Tests for README examples
//!
//! Verifies that examples shown in README.md actually work.
//! Each test corresponds to a specific example or feature documented.

mod common;

use anyhow::{Context, Result};
use serde::Deserialize;
use std::process::Stdio;
use std::time::Duration;

// Note: Read-only volume mapping (--map /host:/guest:ro) is tested in test_fuse_in_vm.rs
// which runs pjdfstest with FUSE mounts. The :ro flag is tested implicitly there.

/// Test environment variables (--env KEY=VALUE)
///
/// README example:
/// ```
/// sudo fcvm podman run --name web1 --env DEBUG=1 nginx:alpine
/// ```
#[tokio::test]
async fn test_env_variables() -> Result<()> {
    println!("\ntest_env_variables");
    println!("==================");

    let fcvm_path = common::find_fcvm_binary()?;
    let vm_name = format!("env-test-{}", std::process::id());

    // Start VM with environment variables
    let mut child = tokio::process::Command::new(&fcvm_path)
        .args([
            "podman", "run",
            "--name", &vm_name,
            "--network", "rootless",
            "--env", "MY_VAR=hello_world",
            "--env", "DEBUG=1",
            "--env", "COMPLEX_VAR=value with spaces",
            common::TEST_IMAGE,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning fcvm")?;

    let fcvm_pid = child.id().ok_or_else(|| anyhow::anyhow!("no PID"))?;
    println!("Started VM with PID: {}", fcvm_pid);

    // Wait for healthy
    if let Err(e) = common::poll_health_by_pid(fcvm_pid, 90).await {
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

    println!("✅ test_env_variables PASSED");
    Ok(())
}

/// Test custom CPU and memory resources (--cpu N --mem MiB)
///
/// README example:
/// ```
/// sudo fcvm podman run --name web1 --cpu 4 --mem 4096 nginx:alpine
/// ```
#[tokio::test]
async fn test_custom_resources() -> Result<()> {
    println!("\ntest_custom_resources");
    println!("=====================");

    let fcvm_path = common::find_fcvm_binary()?;
    let vm_name = format!("resources-test-{}", std::process::id());

    // Start VM with custom resources
    let mut child = tokio::process::Command::new(&fcvm_path)
        .args([
            "podman", "run",
            "--name", &vm_name,
            "--network", "rootless",
            "--cpu", "4",
            "--mem", "1024",
            common::TEST_IMAGE,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning fcvm")?;

    let fcvm_pid = child.id().ok_or_else(|| anyhow::anyhow!("no PID"))?;
    println!("Started VM with PID: {}", fcvm_pid);

    // Wait for healthy
    if let Err(e) = common::poll_health_by_pid(fcvm_pid, 90).await {
        common::kill_process(fcvm_pid).await;
        return Err(e.context("VM failed to become healthy"));
    }
    println!("VM is healthy");

    // Test 1: Check CPU count (via /proc/cpuinfo in VM)
    println!("Test 1: Check CPU count...");
    let output = common::exec_in_vm(fcvm_pid, &["grep", "-c", "^processor", "/proc/cpuinfo"]).await?;
    let cpu_count: i32 = output.trim().parse().unwrap_or(0);
    assert_eq!(
        cpu_count, 4,
        "Should have 4 CPUs, got: {}",
        cpu_count
    );
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
        mem_mb >= 950 && mem_mb <= 1100,
        "Memory should be ~1024 MB, got: {} MB",
        mem_mb
    );
    println!("  Memory = {} MB (requested 1024 MB)", mem_mb);

    // Cleanup
    common::kill_process(fcvm_pid).await;
    let _ = child.wait().await;

    println!("✅ test_custom_resources PASSED");
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
async fn test_fcvm_ls() -> Result<()> {
    println!("\ntest_fcvm_ls");
    println!("============");

    let fcvm_path = common::find_fcvm_binary()?;
    let vm_name = format!("ls-test-{}", std::process::id());

    // Start a VM to list
    let mut child = tokio::process::Command::new(&fcvm_path)
        .args([
            "podman", "run",
            "--name", &vm_name,
            "--network", "rootless",
            common::TEST_IMAGE,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning fcvm")?;

    let fcvm_pid = child.id().ok_or_else(|| anyhow::anyhow!("no PID"))?;
    println!("Started VM with PID: {}", fcvm_pid);

    // Wait for healthy
    if let Err(e) = common::poll_health_by_pid(fcvm_pid, 90).await {
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

    let vms: Vec<VmDisplay> = serde_json::from_str(&stdout)
        .context("JSON should be valid")?;

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

    let vms: Vec<VmDisplay> = serde_json::from_str(&stdout)
        .context("JSON should be valid")?;

    assert_eq!(vms.len(), 1, "Should find exactly one VM with --pid filter");
    assert_eq!(
        vms[0].vm.pid, Some(fcvm_pid),
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

    assert!(output.status.success(), "fcvm ls --pid should succeed even with no matches");
    let stdout = String::from_utf8_lossy(&output.stdout);

    let vms: Vec<VmDisplay> = serde_json::from_str(&stdout)
        .context("JSON should be valid")?;

    assert!(vms.is_empty(), "Should find no VMs with non-existent PID");
    println!("  Correctly returned empty list for non-existent PID");

    // Cleanup
    common::kill_process(fcvm_pid).await;
    let _ = child.wait().await;

    println!("✅ test_fcvm_ls PASSED");
    Ok(())
}

// Note: The --cmd flag is tested in test_fuse_in_vm.rs which uses --cmd to run pjdfstest.
