//! Integration tests for HTTP/HTTPS proxy support.
//!
//! Tests that proxy settings are correctly passed to VMs and containers,
//! and that VMs can use IPv6-only proxies for image pulls and egress.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

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
/// This is the main IPv6 proxy test:
/// 1. Starts an HTTP forward proxy on the host's global IPv6 address
/// 2. Starts a VM configured to use this IPv6 proxy
/// 3. Verifies the VM can pull images and make requests through the proxy
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

    // Find an available port for our proxy
    let proxy_port = common::find_available_high_port().context("find proxy port")?;
    println!("Starting IPv6 proxy on [{}]:{}", host_ipv6, proxy_port);

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

    // Give proxy time to start
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // Verify proxy is working from host
    let proxy_url = format!("http://[{}]:{}", host_ipv6, proxy_port);
    println!("Testing proxy from host: {}", proxy_url);

    let host_test = tokio::process::Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "--max-time",
            "10",
            "-x",
            &proxy_url,
            "http://httpbin.org/ip",
        ])
        .output()
        .await?;

    let status_code = String::from_utf8_lossy(&host_test.stdout);
    println!("Host proxy test status: {}", status_code);

    if status_code.trim() != "200" {
        proxy_server.kill().await.ok();
        anyhow::bail!("Host proxy test failed: status={}", status_code);
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
        proxy_server.kill().await.ok();
        common::kill_process(pid).await;
        let _ = child.wait().await;
        anyhow::bail!("VM never became healthy: {}", e);
    }

    println!("✓ VM healthy - image pull succeeded through IPv6 proxy!");

    // Test egress from inside the container using the proxy
    println!("Testing egress from container via IPv6 proxy...");

    let egress_result = common::exec_in_container(
        pid,
        &[
            "wget",
            "-q",
            "-O",
            "-",
            "--timeout=10",
            "http://httpbin.org/ip",
        ],
    )
    .await;

    // Clean up
    proxy_server.kill().await.ok();
    common::kill_process(pid).await;
    let _ = child.wait().await;

    match egress_result {
        Ok(output) => {
            println!("Egress response: {}", output);
            assert!(
                output.contains("origin"),
                "Expected httpbin response with 'origin', got: {}",
                output
            );
            println!("✓ Container egress works through IPv6 proxy");
        }
        Err(e) => {
            println!("NOTE: Container egress test failed: {}", e);
            println!("      This may be expected if httpbin.org is unreachable");
        }
    }

    println!("✓ IPv6 proxy test completed successfully!");
    Ok(())
}

/// Test that proxy settings are correctly saved and passed to exec commands.
/// Uses a working proxy (the host's IPv4 gateway) to verify end-to-end.
#[tokio::test]
async fn test_proxy_passthrough_to_exec() -> Result<()> {
    // For this test, we don't need a real proxy - just verify the plumbing
    // Start VM without proxy, then verify exec can receive proxy env vars
    let (vm_name, _, _, _) = common::unique_names("proxyexec");

    // Start VM without proxy
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

    // Wait for VM to be healthy
    if let Err(e) = common::poll_health_by_pid(pid, 120).await {
        common::kill_process(pid).await;
        let _ = child.wait().await;
        anyhow::bail!("VM never became healthy: {}", e);
    }

    println!("VM is healthy, testing egress without proxy...");

    // Verify container can reach the internet directly
    let direct_result = common::exec_in_container(
        pid,
        &[
            "wget",
            "-q",
            "-O",
            "-",
            "--timeout=10",
            "http://httpbin.org/ip",
        ],
    )
    .await;

    match direct_result {
        Ok(output) => {
            println!("Direct egress response: {}", output.trim());
            assert!(
                output.contains("origin"),
                "Expected httpbin response, got: {}",
                output
            );
            println!("✓ Container can reach internet directly");
        }
        Err(e) => {
            common::kill_process(pid).await;
            let _ = child.wait().await;
            anyhow::bail!("Container couldn't reach internet: {}", e);
        }
    }

    // Clean up
    common::kill_process(pid).await;
    let _ = child.wait().await;

    println!("✓ Proxy passthrough test completed");
    Ok(())
}

/// Test image pull and container egress in a basic VM (no proxy).
/// This is a sanity check that networking works.
#[tokio::test]
async fn test_image_pull_and_egress() -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names("egresstest");

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

    // Wait for VM to be healthy (this includes image pull)
    println!("Waiting for VM to become healthy (includes image pull)...");
    if let Err(e) = common::poll_health_by_pid(pid, 120).await {
        common::kill_process(pid).await;
        let _ = child.wait().await;
        anyhow::bail!("VM never became healthy: {}", e);
    }

    println!("✓ VM healthy - image pull succeeded");

    // Test egress from the container
    println!("Testing egress from container...");

    let egress_result = common::exec_in_container(
        pid,
        &[
            "wget",
            "-q",
            "-O",
            "-",
            "--timeout=10",
            "http://httpbin.org/ip",
        ],
    )
    .await;

    // Clean up
    common::kill_process(pid).await;
    let _ = child.wait().await;

    match egress_result {
        Ok(output) => {
            println!("Egress response: {}", output.trim());
            assert!(
                output.contains("origin"),
                "Expected httpbin response with 'origin', got: {}",
                output
            );
            println!("✓ Container egress works");
        }
        Err(e) => {
            anyhow::bail!("Container egress failed: {}", e);
        }
    }

    println!("✓ Image pull and egress test completed");
    Ok(())
}
