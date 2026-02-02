//! Integration tests for slirp4netns IPv6 DNS support.
//!
//! Tests behavior on hosts with IPv6-only DNS servers and old libslirp.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

/// Test that libslirp version detection works correctly.
#[tokio::test]
async fn test_libslirp_version_detection() -> Result<()> {
    let output = tokio::process::Command::new("slirp4netns")
        .arg("--version")
        .output()
        .await
        .context("slirp4netns --version")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("slirp4netns --version output:\n{}", stdout);

    // Extract libslirp version
    let version_line = stdout
        .lines()
        .find(|l| l.starts_with("libslirp:"))
        .context("libslirp version not in output")?;

    let version_str = version_line
        .strip_prefix("libslirp: ")
        .unwrap()
        .trim();

    // Parse and verify format
    let parts: Vec<&str> = version_str.split('.').collect();
    assert!(parts.len() >= 2, "Version should be X.Y or X.Y.Z, got: {}", version_str);

    let major: u32 = parts[0].parse().context("major version")?;
    let minor: u32 = parts[1].parse().context("minor version")?;

    println!("System libslirp version: {}.{}", major, minor);

    // Document what version we have and what that means
    if major > 4 || (major == 4 && minor >= 7) {
        println!("✓ libslirp >= 4.7.0 - native IPv6 DNS proxying supported");
    } else {
        println!("✓ libslirp < 4.7.0 - IPv6 DNS proxying NOT supported");
        println!("  On IPv6-only hosts, DNS resolution in VMs will fail");
    }

    Ok(())
}

/// Test the socat relay mechanism used by the proxy forwarder.
/// This verifies that socat can relay TCP traffic correctly.
#[tokio::test]
async fn test_socat_tcp_relay() -> Result<()> {
    // Check socat is installed
    let check = tokio::process::Command::new("which")
        .arg("socat")
        .output()
        .await;

    if check.map(|o| !o.status.success()).unwrap_or(true) {
        println!("SKIP: socat not installed");
        return Ok(());
    }

    // Use find_available_port to get ports not blocked by wildcard binds
    let backend_port = common::find_available_port(19080, 1000)?;
    let relay_port = common::find_available_port(backend_port + 1, 1000)?;

    // Start a simple TCP server using socat (nc/ncat syntax varies by distro)
    // socat is more portable and works consistently
    let mut backend = tokio::process::Command::new("socat")
        .args([
            &format!("TCP-LISTEN:{},reuseaddr", backend_port),
            "STDOUT",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .context("start socat backend")?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Start socat relay: listens on relay_port, forwards to backend_port
    let mut relay = tokio::process::Command::new("socat")
        .args([
            &format!("TCP-LISTEN:{},reuseaddr", relay_port),
            &format!("TCP:127.0.0.1:{}", backend_port),
        ])
        .spawn()
        .context("start socat relay")?;

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connect to relay and send data using socat (portable)
    let _client = tokio::process::Command::new("bash")
        .args(["-c", &format!(
            "echo 'HELLO' | socat - TCP:127.0.0.1:{}", relay_port
        )])
        .output()
        .await?;

    // Read what the backend received
    let backend_stdin = backend.stdin.take();
    drop(backend_stdin); // Close stdin to signal EOF

    let backend_output = tokio::time::timeout(
        Duration::from_secs(5),
        backend.wait_with_output()
    ).await
    .context("timeout waiting for backend")?
    .context("getting backend output")?;

    // Cleanup
    let _ = relay.kill().await;

    let received = String::from_utf8_lossy(&backend_output.stdout);
    println!("Backend received: {:?}", received.trim());

    assert!(
        received.contains("HELLO"),
        "Relay didn't forward data correctly. Backend received: {:?}",
        received
    );

    println!("✓ socat TCP relay works correctly");

    Ok(())
}

/// Test DNS resolution in a VM.
#[tokio::test]
async fn test_dns_resolution_in_vm() -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names("dnstest");

    // Use spawn_fcvm helper (properly handles environment and log capture)
    // Use --no-snapshot to avoid cached snapshots that may have HTTP health checks configured
    // (This test uses alpine with sleep infinity which has no HTTP server)
    let (mut child, pid) = common::spawn_fcvm(&[
        "podman", "run",
        "--name", &vm_name,
        "--network", "rootless",
        "--no-snapshot",
        "alpine:latest",
        "sleep", "infinity",
    ])
    .await
    .context("spawn fcvm")?;

    // Wait for VM to be healthy
    if let Err(e) = common::poll_health_by_pid(pid, 120).await {
        common::kill_process(pid).await;
        let _ = child.wait().await;
        anyhow::bail!("VM never became healthy: {}", e);
    }

    println!("VM is healthy, testing DNS resolution...");

    // Test DNS resolution inside the container (alpine has nslookup via busybox)
    // We test in the container because that's what users actually run.
    // DNS uses the VM's /etc/resolv.conf which is configured by fc-agent from kernel cmdline.
    // Use facebook.com since google.com may be blocked on some corporate networks.
    let dns_result = common::exec_in_container(pid, &["nslookup", "facebook.com"]).await;

    // Clean up VM
    common::kill_process(pid).await;
    let _ = child.wait().await;

    // Verify DNS resolution worked
    let stdout = dns_result.context("DNS resolution failed")?;

    println!("nslookup output:\n{}", stdout);

    // nslookup should show resolved addresses
    assert!(
        stdout.contains("Address") || stdout.contains("Name:"),
        "DNS resolution failed - nslookup didn't return addresses.\n\
         output: {}",
        stdout
    );

    println!("✓ DNS resolution works in rootless VM");

    Ok(())
}
