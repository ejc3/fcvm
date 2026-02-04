//! Integration tests for HTTP/HTTPS proxy support.
//!
//! Tests that proxy settings are correctly passed to VMs and containers,
//! and that VMs can use IPv6-only proxies for image pulls and egress.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};

/// Check if we're running inside a container.
/// IPv6 tests don't work reliably in containers due to network namespace isolation.
fn is_running_in_container() -> bool {
    // Check for container marker files
    std::path::Path::new("/.dockerenv").exists()
        || std::path::Path::new("/run/.containerenv").exists()
        // FCVM_DATA_DIR is set in Makefile for container tests
        || std::env::var("FCVM_DATA_DIR")
            .map(|v| v.contains("container"))
            .unwrap_or(false)
}

/// Get the host's global IPv6 address (if available)
async fn get_host_ipv6() -> Option<String> {
    let output = tokio::process::Command::new("ip")
        .args(["-6", "addr", "show", "scope", "global"])
        .output()
        .await
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .find(|l| l.contains("inet6") && l.contains("scope global"))
        .and_then(|l| {
            l.split_whitespace()
                .nth(1)
                .map(|addr| addr.split('/').next().unwrap_or(addr).to_string())
        })
}

/// Helper to test proxy functionality for a given address family.
///
/// 1. Target server binds to `host_bind` on host
/// 2. Proxy server binds to `host_bind` on host
/// 3. VM uses curl with http_proxy env var pointing to `vm_gateway`
/// 4. VM requests target URL via the proxy
/// 5. Proxy forwards to target and returns response
async fn test_proxy_to_addr(host_bind: &str, vm_gateway: &str, addr_type: &str) -> Result<()> {
    let is_ipv6 = host_bind.contains(':');

    // Start target server
    let target_server = common::LocalTestServer::start_on_available_port(host_bind)
        .await
        .context("start target server")?;
    let target_url = if is_ipv6 {
        format!("http://[{}]:{}/", host_bind, target_server.port)
    } else {
        format!("http://{}:{}/", host_bind, target_server.port)
    };

    // Start proxy server
    let proxy_server = common::LocalProxyServer::start_on_available_port(host_bind)
        .await
        .context("start proxy server")?;
    let vm_proxy_url = if vm_gateway.contains(':') {
        format!("http://[{}]:{}", vm_gateway, proxy_server.port)
    } else {
        format!("http://{}:{}", vm_gateway, proxy_server.port)
    };

    println!(
        "[{}] Target: {} (host's {})",
        addr_type, target_url, host_bind
    );
    println!(
        "[{}] Proxy:  {} (host's {})",
        addr_type, proxy_server.url, host_bind
    );
    println!(
        "[{}] VM proxy: {} ({} → {})",
        addr_type, vm_proxy_url, vm_gateway, host_bind
    );

    let (vm_name, _, _, _) = common::unique_names(&format!("proxy-{}", addr_type));

    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "rootless",
        "--no-snapshot",
        "alpine:latest",
        "sleep",
        "infinity",
    ])
    .await
    .context("spawn fcvm")?;

    if let Err(e) = common::poll_health_by_pid(pid, 120).await {
        target_server.stop().await;
        proxy_server.stop().await;
        common::kill_process(pid).await;
        let _ = child.wait().await;
        anyhow::bail!("[{}] VM never became healthy: {}", addr_type, e);
    }

    println!("[{}] ✓ VM healthy", addr_type);

    // Install curl (busybox wget doesn't support proxies)
    println!("[{}] Installing curl...", addr_type);
    let install_result =
        common::exec_in_container(pid, &["apk", "add", "--no-cache", "curl"]).await;
    if let Err(e) = install_result {
        target_server.stop().await;
        proxy_server.stop().await;
        common::kill_process(pid).await;
        let _ = child.wait().await;
        anyhow::bail!("[{}] Failed to install curl: {}", addr_type, e);
    }

    // Test: Use curl with -x flag to explicitly use proxy
    // (env var method doesn't work reliably across all curl versions/configs)
    println!(
        "[{}] Testing: curl -x {} {}",
        addr_type, vm_proxy_url, target_url
    );
    let result = common::exec_in_container(
        pid,
        &[
            "curl",
            "-s",
            "--max-time",
            "10",
            "-x",
            &vm_proxy_url,
            &target_url,
        ],
    )
    .await;

    let requests_handled = proxy_server.request_count();
    println!(
        "[{}] Proxy handled {} requests",
        addr_type, requests_handled
    );

    target_server.stop().await;
    proxy_server.stop().await;
    common::kill_process(pid).await;
    let _ = child.wait().await;

    match result {
        Ok(output) => {
            assert!(
                output.contains("TEST_SUCCESS"),
                "[{}] Expected TEST_SUCCESS, got: {}",
                addr_type,
                output
            );
            assert!(
                requests_handled > 0,
                "[{}] Proxy should have handled at least 1 request",
                addr_type
            );
            println!(
                "[{}] ✓ VM used proxy ({}) to reach target ({})",
                addr_type, vm_gateway, host_bind
            );
            Ok(())
        }
        Err(e) => anyhow::bail!("[{}] VM couldn't reach target via proxy: {}", addr_type, e),
    }
}

/// Test IPv6 proxy: VM uses fd00::2 to reach proxy on all interfaces
#[tokio::test]
async fn test_proxy_ipv6() -> Result<()> {
    if is_running_in_container() {
        println!(
            "SKIP: IPv6 tests don't work reliably in containers due to network namespace isolation"
        );
        return Ok(());
    }
    // Note: We use :: (all interfaces) instead of ::1 because slirp4netns IPv6
    // doesn't have host loopback translation like IPv4's 10.0.2.2 → 127.0.0.1
    test_proxy_to_addr("::", "fd00::2", "ipv6").await
}

/// Test IPv4 proxy: VM uses 10.0.2.2 to reach proxy on 127.0.0.1
#[tokio::test]
async fn test_proxy_ipv4() -> Result<()> {
    test_proxy_to_addr("127.0.0.1", "10.0.2.2", "ipv4").await
}

/// Helper to test VM egress to a specific bind address.
///
/// The test binds to `bind_addr` on the host, but the VM must use the
/// appropriate slirp4netns gateway to reach it:
/// - 127.0.0.1 → VM uses 10.0.2.2 (IPv4 host loopback)
/// - ::1       → VM uses fd00::2 (IPv6 host loopback)
/// - 0.0.0.0   → VM uses 10.0.2.2 (all interfaces, reached via IPv4 gateway)
/// - IPv6 global → VM can reach directly via slirp IPv6 NAT
async fn test_egress_to_addr(bind_addr: &str, vm_target_addr: &str, addr_type: &str) -> Result<()> {
    let test_server = common::LocalTestServer::start_on_available_port(bind_addr)
        .await
        .context("start test server")?;

    // Build the URL that the VM will use (may differ from server's bind address)
    let vm_url = if vm_target_addr.contains(':') {
        format!("http://[{}]:{}/", vm_target_addr, test_server.port)
    } else {
        format!("http://{}:{}/", vm_target_addr, test_server.port)
    };

    println!(
        "[{}] Server binds to {}, VM connects to {}",
        addr_type, test_server.url, vm_url
    );

    let (vm_name, _, _, _) = common::unique_names(&format!("egress-{}", addr_type));

    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "rootless",
        "--no-snapshot",
        "alpine:latest",
        "sleep",
        "infinity",
    ])
    .await
    .context("spawn fcvm")?;

    if let Err(e) = common::poll_health_by_pid(pid, 120).await {
        test_server.stop().await;
        common::kill_process(pid).await;
        let _ = child.wait().await;
        anyhow::bail!("[{}] VM never became healthy: {}", addr_type, e);
    }

    let result =
        common::exec_in_container(pid, &["wget", "-q", "-O", "-", "--timeout=10", &vm_url]).await;

    test_server.stop().await;
    common::kill_process(pid).await;
    let _ = child.wait().await;

    match result {
        Ok(output) => {
            assert!(
                output.contains("TEST_SUCCESS"),
                "[{}] Expected TEST_SUCCESS, got: {}",
                addr_type,
                output
            );
            println!(
                "[{}] ✓ VM reached {} (bound to {})",
                addr_type, vm_target_addr, bind_addr
            );
            Ok(())
        }
        Err(e) => anyhow::bail!(
            "[{}] Container couldn't reach {} (bound to {}): {}",
            addr_type,
            vm_target_addr,
            bind_addr,
            e
        ),
    }
}

/// Test VM egress to IPv4 loopback (127.0.0.1)
/// Server binds to 127.0.0.1, VM connects via 10.0.2.2 (slirp IPv4 gateway)
#[tokio::test]
async fn test_egress_ipv4_local() -> Result<()> {
    // slirp4netns translates 10.0.2.2 → host's 127.0.0.1
    test_egress_to_addr("127.0.0.1", "10.0.2.2", "ipv4-local").await
}

/// Test VM egress to IPv4 all interfaces (0.0.0.0)
/// Server binds to 0.0.0.0, VM connects via 10.0.2.2 (slirp IPv4 gateway)
#[tokio::test]
async fn test_egress_ipv4_global() -> Result<()> {
    // 0.0.0.0 accepts from all interfaces including from slirp4netns
    test_egress_to_addr("0.0.0.0", "10.0.2.2", "ipv4-global").await
}

/// Test VM egress to IPv6 all interfaces (::)
/// Server binds to ::, VM connects via fd00::2 (slirp IPv6 gateway)
#[tokio::test]
async fn test_egress_ipv6_local() -> Result<()> {
    if is_running_in_container() {
        println!(
            "SKIP: IPv6 tests don't work reliably in containers due to network namespace isolation"
        );
        return Ok(());
    }
    // Note: We use :: (all interfaces) instead of ::1 because slirp4netns IPv6
    // doesn't have host loopback translation like IPv4's 10.0.2.2 → 127.0.0.1
    test_egress_to_addr("::", "fd00::2", "ipv6-local").await
}

/// Test VM egress to IPv6 global (host's public IPv6 address)
/// Server binds to host's global IPv6, VM connects directly via slirp IPv6 NAT
#[tokio::test]
async fn test_egress_ipv6_global() -> Result<()> {
    if is_running_in_container() {
        println!(
            "SKIP: IPv6 tests don't work reliably in containers due to network namespace isolation"
        );
        return Ok(());
    }
    let host_ipv6 = match get_host_ipv6().await {
        Some(ip) => ip,
        None => {
            println!("SKIP: Host has no global IPv6 address");
            return Ok(());
        }
    };
    // VM can reach host's global IPv6 directly through slirp4netns IPv6 NAT
    test_egress_to_addr(&host_ipv6, &host_ipv6, "ipv6-global").await
}
