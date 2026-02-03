//! Integration tests for HTTP/HTTPS proxy support.
//!
//! Tests that proxy settings are correctly passed to VMs and containers,
//! and that VMs can use IPv6-only proxies for image pulls and egress.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};

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

/// Test that a VM can use an IPv6-only proxy for egress.
///
/// This is the main IPv6 proxy test - fully self-contained with no external network access:
/// 1. Starts a local HTTP server on the host's IPv6 address (the "target")
/// 2. Starts an HTTP forward proxy on the host's IPv6 address
/// 3. Starts a VM configured to use the IPv6 proxy
/// 4. Verifies the VM can reach the local target server through the proxy
///
/// This proves the VM can:
/// - Use an IPv6-only proxy (set via http_proxy env var)
/// - Reach IPv6 services through slirp4netns
#[tokio::test]
async fn test_vm_uses_ipv6_proxy() -> Result<()> {
    // Get host's global IPv6 address
    let host_ipv6 = match get_host_ipv6().await {
        Some(ip) => ip,
        None => {
            println!("SKIP: Host has no global IPv6 address");
            return Ok(());
        }
    };

    println!("Host IPv6 address: {}", host_ipv6);

    // Start a local test server as the target (uses common helper)
    let target_server = common::LocalTestServer::start_on_available_port(&host_ipv6)
        .await
        .context("start target server")?;
    let target_port = target_server.port;
    let target_url = target_server.url.clone();

    // Find a port for the proxy
    let proxy_port = target_port + 1;
    println!(
        "Local target: {}, proxy on [{}]:{}",
        target_url, host_ipv6, proxy_port
    );

    // Start an HTTP forward proxy listening on the host's IPv6 address
    // This proxy forwards HTTP requests (used for container registry access)
    let mut proxy_server = tokio::process::Command::new("python3")
        .args([
            "-c",
            &format!(
                r#"
import http.server
import socketserver
import socket
import urllib.request
import urllib.error
import sys

class IPv6Server(socketserver.TCPServer):
    address_family = socket.AF_INET6
    allow_reuse_address = True

class ProxyHandler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self._proxy_request()

    def do_HEAD(self):
        self._proxy_request()

    def _proxy_request(self):
        try:
            url = self.path
            print(f"[Proxy] {{self.command}} {{url}}", file=sys.stderr, flush=True)

            req = urllib.request.Request(url, method=self.command)
            # Copy relevant headers
            for key in ['Host', 'User-Agent', 'Accept', 'Authorization']:
                if key in self.headers:
                    req.add_header(key, self.headers[key])

            with urllib.request.urlopen(req, timeout=30) as response:
                self.send_response(response.status)
                for key, value in response.headers.items():
                    if key.lower() not in ('transfer-encoding', 'connection'):
                        self.send_header(key, value)
                self.end_headers()
                if self.command != 'HEAD':
                    self.wfile.write(response.read())
        except urllib.error.HTTPError as e:
            print(f"[Proxy] HTTP error: {{e}}", file=sys.stderr, flush=True)
            self.send_error(e.code, str(e))
        except Exception as e:
            print(f"[Proxy] Error: {{e}}", file=sys.stderr, flush=True)
            self.send_error(502, f"Proxy error: {{e}}")

    def do_CONNECT(self):
        # Handle HTTPS CONNECT tunneling
        try:
            host, port = self.path.split(':')
            port = int(port)
            print(f"[Proxy] CONNECT {{host}}:{{port}}", file=sys.stderr, flush=True)

            # Connect to target
            sock = socket.create_connection((host, port), timeout=30)
            self.send_response(200, 'Connection established')
            self.end_headers()

            # Tunnel data
            import select
            self.connection.setblocking(False)
            sock.setblocking(False)

            while True:
                readable, _, _ = select.select([self.connection, sock], [], [], 1)
                if self.connection in readable:
                    data = self.connection.recv(65536)
                    if not data:
                        break
                    sock.sendall(data)
                if sock in readable:
                    data = sock.recv(65536)
                    if not data:
                        break
                    self.connection.sendall(data)
            sock.close()
        except Exception as e:
            print(f"[Proxy] CONNECT error: {{e}}", file=sys.stderr, flush=True)
            self.send_error(502, f"Tunnel error: {{e}}")

    def log_message(self, format, *args):
        print(f"[Proxy] {{format % args}}", file=sys.stderr, flush=True)

print(f"Starting IPv6 proxy on [{host_ipv6}]:{proxy_port}", file=sys.stderr, flush=True)
try:
    with IPv6Server(('{host_ipv6}', {proxy_port}), ProxyHandler) as httpd:
        print(f"Proxy ready", file=sys.stderr, flush=True)
        httpd.serve_forever()
except Exception as e:
    print(f"Failed to start proxy: {{e}}", file=sys.stderr, flush=True)
    sys.exit(1)
"#,
                host_ipv6 = host_ipv6,
                proxy_port = proxy_port
            ),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("start IPv6 proxy server")?;

    // Wait for proxy to be ready
    let proxy_addr = format!("[{}]:{}", host_ipv6, proxy_port);
    if let Err(e) = common::wait_for_tcp(&proxy_addr, 1000).await {
        target_server.stop().await;
        anyhow::bail!("proxy server failed to start: {}", e);
    }

    // Verify target server is working from host (direct access)
    println!("Testing target server directly: {}", target_url);

    let direct_test = tokio::process::Command::new("curl")
        .args(["-s", "--max-time", "5", &target_url])
        .output()
        .await?;

    let direct_response = String::from_utf8_lossy(&direct_test.stdout);
    if !direct_response.contains("TEST_SUCCESS") {
        target_server.stop().await;
        proxy_server.kill().await.ok();
        anyhow::bail!(
            "Target server not responding correctly: {}",
            direct_response
        );
    }
    println!("✓ Target server working");

    // Verify proxy is working from host
    let proxy_url = format!("http://[{}]:{}", host_ipv6, proxy_port);
    println!("Testing proxy from host: {} -> {}", proxy_url, target_url);

    let host_test = tokio::process::Command::new("curl")
        .args(["-s", "--max-time", "10", "-x", &proxy_url, &target_url])
        .output()
        .await?;

    let proxy_response = String::from_utf8_lossy(&host_test.stdout);
    if !proxy_response.contains("TEST_SUCCESS") {
        target_server.stop().await;
        proxy_server.kill().await.ok();
        anyhow::bail!("Proxy test failed: {}", proxy_response);
    }
    println!("✓ IPv6 proxy working on host");

    // Now start a VM that uses this proxy
    let (vm_name, _, _, _) = common::unique_names("ipv6proxyvm");

    println!("Starting VM with proxy: {}", proxy_url);

    let (mut child, pid) = common::spawn_fcvm_with_env(
        &[
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
        ],
        &[("http_proxy", &proxy_url), ("https_proxy", &proxy_url)],
    )
    .await
    .context("spawn fcvm with IPv6 proxy")?;

    // Wait for VM to be healthy (image pull goes through the proxy!)
    println!("Waiting for VM to become healthy (image pull via IPv6 proxy)...");
    if let Err(e) = common::poll_health_by_pid(pid, 180).await {
        target_server.stop().await;
        proxy_server.kill().await.ok();
        common::kill_process(pid).await;
        let _ = child.wait().await;
        anyhow::bail!("VM never became healthy: {}", e);
    }

    println!("✓ VM healthy - image pull succeeded through IPv6 proxy!");

    // Test egress from inside the container to our local target via the proxy
    // The VM reaches host's IPv6 via slirp4netns NAT, then proxy forwards to target
    println!(
        "Testing egress from container via IPv6 proxy to local target: {}",
        target_url
    );

    let egress_result =
        common::exec_in_container(pid, &["wget", "-q", "-O", "-", "--timeout=10", &target_url])
            .await;

    // Clean up
    target_server.stop().await;
    proxy_server.kill().await.ok();
    common::kill_process(pid).await;
    let _ = child.wait().await;

    match egress_result {
        Ok(output) => {
            println!("Egress response: {}", output);
            assert!(
                output.contains("TEST_SUCCESS"),
                "Expected 'TEST_SUCCESS' response, got: {}",
                output
            );
            println!("✓ Container egress works through IPv6 proxy to local target");
        }
        Err(e) => {
            anyhow::bail!(
                "Container couldn't reach local target through IPv6 proxy: {}",
                e
            );
        }
    }

    println!("✓ IPv6 proxy test completed successfully!");
    Ok(())
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

/// Test VM egress to IPv6 loopback (::1)
/// Server binds to ::1, VM connects via fd00::2 (slirp IPv6 gateway)
#[tokio::test]
async fn test_egress_ipv6_local() -> Result<()> {
    // slirp4netns translates fd00::2 → host's ::1
    test_egress_to_addr("::1", "fd00::2", "ipv6-local").await
}

/// Test VM egress to IPv6 global (host's public IPv6 address)
/// Server binds to host's global IPv6, VM connects directly via slirp IPv6 NAT
#[tokio::test]
async fn test_egress_ipv6_global() -> Result<()> {
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
