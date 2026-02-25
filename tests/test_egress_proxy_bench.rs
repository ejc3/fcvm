//! Egress proxy benchmark: 8000 concurrent TCP connections at 1MB each
//!
//! Verifies the multiplexed vsock egress proxy handles high concurrency:
//! 1. Adds a test IP (192.0.2.1) to host loopback so traffic goes through the proxy
//! 2. Starts a rootless VM with 16 vCPUs and 16GB RAM
//! 3. Starts a host-side TCP server on 0.0.0.0 that sends 1MB per connection
//! 4. Runs 8000 parallel connections from inside the VM to 192.0.2.1
//! 5. Verifies all connections succeed and measures throughput
//!
//! Traffic path: guest wget → iptables REDIRECT → guest proxy → vsock mux →
//!               host proxy → TcpStream::connect(192.0.2.1) → test server
//!
//! The IP 192.0.2.1 (RFC 5737 TEST-NET-1) is NOT in the iptables exemption
//! ranges (127.0.0.0/8 and 10.0.2.0/24), so it gets caught by the REDIRECT rule.

#![cfg(feature = "integration-slow")]

mod common;

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

const NUM_CONNECTIONS: u32 = 8000;
const PAYLOAD_SIZE: usize = 1_048_576; // 1MB per connection

/// The test IP added to host loopback. Must NOT be in 127.0.0.0/8 or 10.0.2.0/24
/// so iptables REDIRECT catches it inside the guest.
const TEST_IP: &str = "192.0.2.1";

/// Benchmark: 8000 concurrent 1MB downloads through the multiplexed egress proxy
#[tokio::test]
async fn test_egress_proxy_8000_concurrent_1mb() -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names("proxy-bench");

    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║  Egress Proxy Benchmark: {} concurrent × 1MB              ║", NUM_CONNECTIONS);
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    // Step 0: Add test IP to host loopback so host proxy can connect to it
    println!("Step 0: Adding {} to host loopback...", TEST_IP);
    let add_ip = std::process::Command::new("sudo")
        .args(["ip", "addr", "add", &format!("{}/32", TEST_IP), "dev", "lo"])
        .output()?;
    if !add_ip.status.success() {
        let stderr = String::from_utf8_lossy(&add_ip.stderr);
        if !stderr.contains("File exists") {
            anyhow::bail!("Failed to add {} to loopback: {}", TEST_IP, stderr);
        }
    }
    println!("  ✓ {} added to lo", TEST_IP);

    // Ensure cleanup on exit
    let _cleanup_guard = TestIpCleanup;

    // Step 1: Start a host-side TCP server that sends 1MB per connection
    // Bind on 0.0.0.0 so it's reachable via the test IP
    let server = start_1mb_server("0.0.0.0").await?;
    println!("  Host TCP server on port {} (1MB per connection)", server.port);

    // Step 2: Start rootless VM with high resources
    println!("\nStep 1: Starting rootless VM with 16 vCPUs, 16GB RAM...");
    let (_child, vm_pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "rootless",
            "--cpu",
            "16",
            "--mem",
            "16384",
            common::ALPINE_IMAGE,
            "sleep",
            "infinity",
        ],
        &vm_name,
    )
    .await
    .context("spawning VM")?;

    println!("  Waiting for VM to become healthy (PID: {})...", vm_pid);
    if let Err(e) = common::poll_health_by_pid(vm_pid, 180).await {
        server.stop();
        common::kill_process(vm_pid).await;
        return Err(e.context("VM failed to become healthy"));
    }
    println!("  ✓ VM healthy");

    // Step 3: Verify basic egress through the proxy works
    // Use the test IP — this MUST go through the proxy (not pasta)
    println!("\nStep 2: Verifying egress through proxy ({}:{})...", TEST_IP, server.port);
    let fcvm_path = common::find_fcvm_binary()?;

    // Retry a few times — proxy vsock connection may take a moment
    let mut verify_ok = false;
    for attempt in 1..=5 {
        let verify = tokio::process::Command::new(&fcvm_path)
            .args([
                "exec",
                "--pid",
                &vm_pid.to_string(),
                "--",
                "wget",
                "-q",
                "-O",
                "/dev/null",
                "--timeout=10",
                &format!("http://{}:{}/", TEST_IP, server.port),
            ])
            .output()
            .await?;

        if verify.status.success() {
            verify_ok = true;
            break;
        }
        if attempt < 5 {
            println!("  Attempt {}/5 failed, retrying...", attempt);
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }

    if !verify_ok {
        let port = server.port;
        server.stop();
        common::kill_process(vm_pid).await;
        anyhow::bail!("Egress through proxy to {}:{} failed after 5 attempts", TEST_IP, port);
    }
    println!("  ✓ Egress through proxy works");

    // Step 4: Write the benchmark script into the VM
    println!("\nStep 3: Running {} concurrent 1MB downloads through proxy...", NUM_CONNECTIONS);

    // Write a shell script into the VM that spawns parallel wget jobs
    // Uses TEST_IP so traffic goes through the proxy, NOT pasta
    let script = format!(
        r#"#!/bin/sh
IP={}
PORT={}
COUNT={}
OK=0
FAIL=0

# Create a temp dir for results
TMPDIR=$(mktemp -d)

# Spawn all connections in parallel
for i in $(seq 1 $COUNT); do
    (
        if wget -q -O /dev/null --timeout=60 "http://$IP:$PORT/" 2>/dev/null; then
            echo ok > "$TMPDIR/$i"
        else
            echo fail > "$TMPDIR/$i"
        fi
    ) &
    # Batch spawning to avoid overwhelming the shell
    if [ $((i % 500)) -eq 0 ]; then
        sleep 0.1
    fi
done

wait

# Count results
OK=$(grep -rl ok "$TMPDIR" | wc -l)
FAIL=$(grep -rl fail "$TMPDIR" | wc -l)
rm -rf "$TMPDIR"

echo "RESULT:$OK/$COUNT"
"#,
        TEST_IP, server.port, NUM_CONNECTIONS
    );

    // Write script to VM
    let write_result = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &vm_pid.to_string(),
            "--",
            "sh",
            "-c",
            &format!("cat > /tmp/bench.sh << 'SCRIPT_EOF'\n{}\nSCRIPT_EOF\nchmod +x /tmp/bench.sh", script),
        ])
        .output()
        .await?;

    if !write_result.status.success() {
        let stderr = String::from_utf8_lossy(&write_result.stderr);
        server.stop();
        common::kill_process(vm_pid).await;
        anyhow::bail!("Failed to write script: {}", stderr);
    }

    // Run the benchmark
    let start = Instant::now();
    let bench_result = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &vm_pid.to_string(),
            "--",
            "sh",
            "/tmp/bench.sh",
        ])
        .output()
        .await?;

    let elapsed = start.elapsed();

    let stdout = String::from_utf8_lossy(&bench_result.stdout);
    let stderr = String::from_utf8_lossy(&bench_result.stderr);

    println!("  Completed in {:.1}s", elapsed.as_secs_f64());

    // Parse results
    let (ok_count, total) = if let Some(line) = stdout.lines().find(|l| l.starts_with("RESULT:")) {
        let parts: Vec<&str> = line.trim_start_matches("RESULT:").split('/').collect();
        if parts.len() == 2 {
            (
                parts[0].parse::<u32>().unwrap_or(0),
                parts[1].parse::<u32>().unwrap_or(NUM_CONNECTIONS),
            )
        } else {
            (0, NUM_CONNECTIONS)
        }
    } else {
        eprintln!("  stdout: {}", stdout);
        eprintln!("  stderr: {}", stderr.lines().take(10).collect::<Vec<_>>().join("\n"));
        (0, NUM_CONNECTIONS)
    };

    let fail_count = total - ok_count;
    let total_bytes = ok_count as u64 * PAYLOAD_SIZE as u64;
    let throughput_mbps = total_bytes as f64 / elapsed.as_secs_f64() / 1_000_000.0;

    println!("  Success: {}/{}", ok_count, total);
    if fail_count > 0 {
        println!("  Failed:  {}", fail_count);
    }
    println!(
        "  Throughput: {:.0} MB/s ({:.1} GB total in {:.1}s)",
        throughput_mbps,
        total_bytes as f64 / 1_000_000_000.0,
        elapsed.as_secs_f64()
    );
    println!(
        "  Connections served: {} ({:.0} req/s)",
        server.connections(),
        server.connections() as f64 / elapsed.as_secs_f64()
    );

    // Cleanup
    println!("\nCleaning up...");
    server.stop();
    common::kill_process(vm_pid).await;

    // Verify all succeeded
    assert_eq!(
        ok_count, total,
        "Expected all {} connections to succeed, but {} failed",
        total, fail_count
    );

    println!(
        "\n✅ PROXY BENCHMARK PASSED: {}/{} OK, {:.0} MB/s",
        ok_count, total, throughput_mbps
    );

    Ok(())
}

/// RAII guard to remove test IP from loopback on drop
struct TestIpCleanup;

impl Drop for TestIpCleanup {
    fn drop(&mut self) {
        let _ = std::process::Command::new("sudo")
            .args(["ip", "addr", "del", &format!("{}/32", TEST_IP), "dev", "lo"])
            .output();
    }
}

/// Simple TCP server that sends 1MB of data per connection.
/// Runs on a background thread (not async) for maximum throughput.
struct OneMbServer {
    port: u16,
    stop_flag: Arc<std::sync::atomic::AtomicBool>,
    conn_count: Arc<AtomicU32>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl OneMbServer {
    fn connections(&self) -> u32 {
        self.conn_count.load(Ordering::Relaxed)
    }

    fn stop(mut self) {
        self.stop_flag.store(true, Ordering::Release);
        // Connect to unblock accept()
        let _ = std::net::TcpStream::connect(format!("127.0.0.1:{}", self.port));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

async fn start_1mb_server(bind_addr: &str) -> Result<OneMbServer> {
    let listener = std::net::TcpListener::bind(format!("{}:0", bind_addr))?;
    let port = listener.local_addr()?.port();
    let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let conn_count = Arc::new(AtomicU32::new(0));

    // Generate 1MB payload with HTTP header
    let body = vec![b'X'; PAYLOAD_SIZE];
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        PAYLOAD_SIZE
    );
    let mut response = Vec::with_capacity(header.len() + body.len());
    response.extend_from_slice(header.as_bytes());
    response.extend_from_slice(&body);
    let response = Arc::new(response);

    let stop = stop_flag.clone();
    let count = conn_count.clone();

    // Set SO_REUSEADDR and non-blocking accept timeout
    listener.set_nonblocking(false)?;

    let handle = std::thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let resp = response.clone();
                    let c = count.clone();
                    std::thread::spawn(move || {
                        use std::io::Write;
                        // Read the HTTP request first (required for wget)
                        let mut buf = [0u8; 2048];
                        let _ = std::io::Read::read(&mut stream, &mut buf);
                        // Send response
                        let _ = stream.write_all(&resp);
                        c.fetch_add(1, Ordering::Relaxed);
                    });
                }
                Err(_) => {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                }
            }
        }
    });

    Ok(OneMbServer {
        port,
        stop_flag,
        conn_count,
        handle: Some(handle),
    })
}
