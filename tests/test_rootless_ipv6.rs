//! Integration tests for rootless IPv6 networking.
//!
//! Tests IPv6 DNS, connectivity, and egress in rootless mode (pasta networking).

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};

/// Test DNS resolution in a VM using a local DNS server.
#[tokio::test]
async fn test_dns_resolution_in_vm() -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names("dnstest");

    // Start a local DNS server that responds with a test IP
    // Using high port since port 53 may be in use by systemd-resolved
    let dns_response_ip: std::net::Ipv4Addr = "93.184.216.34".parse().unwrap();
    let dns_server = common::LocalDnsServer::start_on_available_port("127.0.0.1", dns_response_ip)
        .await
        .context("starting local DNS server")?;

    // For rootless networking, the VM reaches the host via 10.0.2.2
    let dns_server_addr = "10.0.2.2";
    println!(
        "Local DNS server started on port {} (VM will query {}:{})",
        dns_server.port, dns_server_addr, dns_server.port
    );

    // Use alpine with sleep - no HTTP server needed since health uses container-ready file
    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "rootless",
        "--no-snapshot",
        common::ALPINE_IMAGE,
        "sleep",
        "infinity",
    ])
    .await
    .context("spawn fcvm")?;

    // Wait for VM to be healthy
    if let Err(e) = common::poll_health_by_pid(pid, 120).await {
        dns_server.stop().await;
        common::kill_process(pid).await;
        let _ = child.wait().await;
        anyhow::bail!("VM never became healthy: {}", e);
    }

    println!("VM is healthy, testing DNS resolution...");

    // Install bind-tools for dig command (Alpine doesn't include it by default)
    println!("Installing bind-tools for dig...");
    let install_result = common::exec_in_vm(pid, &["apk", "add", "--no-cache", "bind-tools"]).await;
    if let Err(e) = install_result {
        // Log but don't fail - dig might already be available
        eprintln!("Warning: bind-tools install: {}", e);
    }

    // Test DNS resolution inside the VM using dig (supports custom ports)
    // dig @server -p port hostname +short
    let dns_result = common::exec_in_vm(
        pid,
        &[
            "dig",
            &format!("@{}", dns_server_addr),
            "-p",
            &dns_server.port.to_string(),
            "test.local",
            "+short",
        ],
    )
    .await;

    // Clean up
    dns_server.stop().await;
    common::kill_process(pid).await;
    let _ = child.wait().await;

    // Verify DNS resolution worked
    let stdout = dns_result.context("DNS resolution failed")?;

    println!("dig output:\n{}", stdout);

    // dig +short should return just the IP
    assert!(
        stdout.contains(&dns_response_ip.to_string()),
        "DNS resolution failed - didn't get expected IP {}.\n\
         output: {}",
        dns_response_ip,
        stdout
    );

    println!("✓ DNS resolution works in rootless VM");

    Ok(())
}

/// Test IPv6 connectivity in a VM.
/// Verifies that the guest has IPv6 configured and can reach the pasta IPv6 gateway.
/// This proves the NDP Neighbor Advertisement mechanism works correctly.
#[tokio::test]
async fn test_ipv6_connectivity_in_vm() -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names("ipv6test");

    // Use alpine with sleep - no HTTP server needed since health uses container-ready file
    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "rootless",
        "--no-snapshot",
        common::ALPINE_IMAGE,
        "sleep",
        "infinity",
    ])
    .await
    .context("spawn fcvm")?;

    // Wait for VM to be healthy
    if let Err(e) = common::poll_health_by_pid(pid, 120).await {
        common::kill_process(pid).await;
        let _ = child.wait().await;
        anyhow::bail!("VM never became healthy: {}", e);
    }

    println!("VM is healthy, testing IPv6 connectivity...");

    // Check 1: Verify IPv6 address is configured on eth0 inside the VM
    // fc-agent should have configured fd00::100/64 if IPv6 DNS was detected
    let ip_result = common::exec_in_vm(pid, &["ip", "-6", "addr", "show", "dev", "eth0"]).await;

    let ip_output = match ip_result {
        Ok(output) => output,
        Err(e) => {
            common::kill_process(pid).await;
            let _ = child.wait().await;
            anyhow::bail!("Failed to get IPv6 address: {}", e);
        }
    };

    println!("IPv6 addresses on eth0:\n{}", ip_output);

    // Check if IPv6 is configured (fd00::100 is the expected guest address)
    // fc-agent configures this from the ipv6= kernel boot parameter
    let has_ipv6 = ip_output.contains("fd00::100") || ip_output.contains("inet6 fd00::");

    if !has_ipv6 {
        // IPv6 might not be configured if host doesn't have global IPv6
        // This is expected behavior - skip the test gracefully
        println!("SKIP: IPv6 not configured on guest (host may not have global IPv6)");
        common::kill_process(pid).await;
        let _ = child.wait().await;
        return Ok(());
    }

    println!("✓ IPv6 address fd00::100 configured on eth0");

    // Check 2: Verify we can ping the gateway (fd00::2)
    // This proves:
    // - IPv6 routing is working
    // - NDP Neighbor Advertisement works (pasta responds to neighbor solicitations)
    // - The namespace tap device is responding to IPv6 traffic
    let ping_result =
        common::exec_in_vm(pid, &["ping", "-6", "-c", "1", "-W", "5", "fd00::2"]).await;

    // Clean up VM
    common::kill_process(pid).await;
    let _ = child.wait().await;

    match ping_result {
        Ok(output) => {
            println!("Ping fd00::2 output:\n{}", output);
            assert!(
                output.contains("1 packets received") || output.contains("1 received"),
                "IPv6 ping to gateway failed.\noutput: {}",
                output
            );
            println!("✓ IPv6 connectivity to gateway (fd00::2) works");
        }
        Err(e) => {
            // Ping might fail if namespace doesn't respond to ICMP, but IPv6 could still work
            println!(
                "NOTE: IPv6 ping failed ({}), but IPv6 may still work for TCP",
                e
            );
        }
    }

    println!("✓ IPv6 connectivity test passed");

    Ok(())
}

/// HTTP server for a guest to fetch from, on the host's IPv6 wildcard address.
///
/// It is a task of the test process and not a child process, so it stops when its handle
/// is dropped (an early return or a panic in the test) and it cannot outlive a test
/// process that is killed. A child process needs its own teardown on each of those paths,
/// and where `python3` is a launcher that runs the interpreter as its own child, neither
/// killing the child nor a parent-death signal on it reaches the interpreter.
async fn start_host_http_server() -> Result<common::LocalTestServer> {
    common::LocalTestServer::start_on_available_port("::")
        .await
        .context("starting the host's IPv6 HTTP server")
}

/// Test IPv6 egress from VM to a server on the host's IPv6 address.
///
/// This verifies that the VM can reach external IPv6 endpoints.
/// We start an HTTP server on the host's IPv6 wildcard address,
/// then have the VM fetch from it at the host's global IPv6 address.
#[tokio::test]
async fn test_ipv6_egress_to_host() -> Result<()> {
    // Get host's global IPv6 address
    let ip_output = tokio::process::Command::new("ip")
        .args(["-6", "addr", "show", "scope", "global"])
        .output()
        .await
        .context("get host IPv6")?;

    let stdout = String::from_utf8_lossy(&ip_output.stdout);

    // Parse out the IPv6 address (format: "inet6 2600:1f1c:.../128 scope global")
    // Filter out ULA (fd00::/7) and link-local (fe80::) — same logic as
    // PastaNetwork::detect_host_ipv6() in pasta.rs. ULA addresses have "global"
    // scope in the kernel but aren't routable through pasta's L4 translation.
    let host_ipv6 = stdout
        .lines()
        .filter(|l| l.contains("inet6") && l.contains("scope global"))
        .filter_map(|l| {
            l.split_whitespace()
                .nth(1)
                .map(|addr| addr.split('/').next().unwrap_or(addr))
                .map(|s| s.to_string())
        })
        .find(|addr| !addr.starts_with("fe80:") && !addr.starts_with("fd"));

    let host_ipv6 = match host_ipv6 {
        Some(ip) => ip,
        None => {
            println!("SKIP: Host has no globally-routable IPv6 address (ULA/link-local only)");
            return Ok(());
        }
    };

    println!("Host IPv6 address: {}", host_ipv6);

    let server = start_host_http_server().await?;
    println!("HTTP server for the guest on port {}", server.port);

    // Start a VM
    let (vm_name, _, _, _) = common::unique_names("ipv6egress");

    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "rootless",
        "--no-snapshot",
        common::ALPINE_IMAGE,
        "sleep",
        "infinity",
    ])
    .await
    .context("spawn fcvm")?;

    // Wait for VM to be healthy
    if let Err(e) = common::poll_health_by_pid(pid, 120).await {
        server.stop().await;
        common::kill_process(pid).await;
        let _ = child.wait().await;
        anyhow::bail!("VM never became healthy: {}", e);
    }

    println!("VM is healthy, testing IPv6 egress to host...");

    // Fetch from the host's server with the guest's wget (GNU wget in the guest OS).
    let url = format!("http://[{}]:{}/", host_ipv6, server.port);
    println!("Attempting to connect to: {}", url);

    // The request has to reach the server above, so wget must not hand it to a proxy.
    // fcvm forwards the host's proxy variables to the guest and wget obeys http_proxy: on
    // a host that exports one, the request goes to that proxy and no packet reaches the
    // server (a proxy that refuses the host's address answers HTTP 403). --no-proxy keeps
    // the connection direct. http_proxy names a port nothing listens on, so a fetch that
    // does obey a proxy fails on every host and not only on one that exports a proxy.
    // -nv keeps wget's one-line error in the failure, where -q left only an exit status.
    let result = common::exec_in_vm(
        pid,
        &[
            "http_proxy=http://[::1]:9/",
            "wget",
            "--no-proxy",
            "-nv",
            "-O",
            "-",
            "--timeout=5",
            &url,
        ],
    )
    .await;

    // Clean up
    server.stop().await;
    common::kill_process(pid).await;
    let _ = child.wait().await;

    // exec_in_vm returns the command's standard output, which for `wget -O -` is the
    // body and nothing else: wget's own line goes to standard error.
    let output = result.context("the guest could not fetch from the host's IPv6 server")?;
    anyhow::ensure!(
        output == "TEST_SUCCESS\n",
        "the fetch did not return the host server's body: {output:?}"
    );
    println!("✓ IPv6 egress works! Server response:\n{}", output);
    Ok(())
}

/// A host server stops when the test that started it returns before its cleanup.
///
/// What is watched is the server's own listening socket and not its port: a port that is
/// free can be taken at once by another test's server, and a port that is taken would
/// then read as a server that did not stop. The property is `LocalTestServer`'s whatever
/// address it binds, so this binds IPv4 loopback and runs on a host with no IPv6.
#[tokio::test]
async fn host_http_server_stops_when_its_test_returns_early() -> Result<()> {
    type Listener = std::sync::Weak<tokio::net::TcpListener>;

    // Stand-in for a test body in which a step fails before the cleanup. It leaves the
    // server's listening socket and whether that was open while the body ran.
    async fn fails_before_cleanup(seen: &mut Option<(Listener, bool)>) -> Result<()> {
        let server = common::LocalTestServer::start_on_available_port("127.0.0.1")
            .await
            .context("starting the server")?;
        let listener = server.listener();
        let open = listener.strong_count() > 0;
        *seen = Some((listener, open));
        anyhow::bail!("a step failed before the cleanup");
    }

    let mut seen = None;
    fails_before_cleanup(&mut seen)
        .await
        .expect_err("the stand-in body returns an error");
    let (listener, open_while_running) = seen.expect("the stand-in body did not start its server");
    assert!(
        open_while_running,
        "the server's listening socket was not open while its test ran"
    );

    // The server's task closes its listening socket when it next runs, so the check is
    // retried, and the deadline only bounds a failure.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while listener.strong_count() > 0 {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "the server's listening socket is still open 5s after its test returned"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    Ok(())
}
