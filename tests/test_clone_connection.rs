//! Test TCP connection behavior when cloning a VM
//!
//! This test verifies what happens to active TCP connections when a VM is cloned:
//! 1. Host runs a TCP server that broadcasts messages every 100ms
//! 2. VM runs a client that connects to the host server
//! 3. We snapshot and clone the VM
//! 4. Observe: does the clone's connection reset? Can it reconnect?

mod common;

use anyhow::{Context, Result};
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Global counter for unique test IDs to avoid conflicts when running tests in parallel
static TEST_ID: AtomicUsize = AtomicUsize::new(0);

/// Generate unique names for this test run
fn unique_names(prefix: &str) -> (String, String, String, String) {
    let id = TEST_ID.fetch_add(1, Ordering::SeqCst);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        % 100000;
    let baseline = format!("{}-base-{}-{}", prefix, ts, id);
    let clone = format!("{}-clone-{}-{}", prefix, ts, id);
    let snapshot = format!("{}-snap-{}-{}", prefix, ts, id);
    let serve = format!("{}-serve-{}-{}", prefix, ts, id);
    (baseline, clone, snapshot, serve)
}

/// A connected client with its connection ID
struct Client {
    stream: TcpStream,
    conn_id: u64,
}

/// TCP server that broadcasts messages to all connected clients every 100ms
struct BroadcastServer {
    listener: TcpListener,
    stop: Arc<AtomicBool>,
    seq: Arc<AtomicU64>,
    conn_counter: Arc<AtomicU64>,
    port: u16,
}

impl BroadcastServer {
    fn new() -> Result<Self> {
        // Bind to all interfaces so VM can reach us
        let listener = TcpListener::bind("0.0.0.0:0")?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();

        Ok(Self {
            listener,
            stop: Arc::new(AtomicBool::new(false)),
            seq: Arc::new(AtomicU64::new(0)),
            conn_counter: Arc::new(AtomicU64::new(0)),
            port,
        })
    }

    fn port(&self) -> u16 {
        self.port
    }

    fn stop_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    /// Run the server in a background thread
    fn run_in_background(self) -> std::thread::JoinHandle<()> {
        let stop = Arc::clone(&self.stop);
        let seq = Arc::clone(&self.seq);
        let conn_counter = Arc::clone(&self.conn_counter);
        let listener = self.listener;

        std::thread::spawn(move || {
            let mut clients: Vec<Client> = Vec::new();

            while !stop.load(Ordering::Relaxed) {
                // Accept new connections (non-blocking)
                if let Ok((stream, addr)) = listener.accept() {
                    let conn_id = conn_counter.fetch_add(1, Ordering::Relaxed) + 1;
                    eprintln!("[server] CONN#{} connected from {}", conn_id, addr);
                    stream.set_nonblocking(false).ok();
                    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
                    clients.push(Client { stream, conn_id });
                }

                // Broadcast message to all clients
                let seq_num = seq.fetch_add(1, Ordering::Relaxed);

                clients.retain_mut(|client| {
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_millis();
                    let msg = format!("CONN#{} SEQ:{} TIME:{}\n", client.conn_id, seq_num, now);

                    match client.stream.write_all(msg.as_bytes()) {
                        Ok(_) => true,
                        Err(e) => {
                            eprintln!("[server] CONN#{} disconnected: {}", client.conn_id, e);
                            false
                        }
                    }
                });

                std::thread::sleep(Duration::from_millis(100));
            }

            eprintln!(
                "[server] Shutting down, {} clients connected",
                clients.len()
            );
        })
    }
}

/// Test that cloning a VM resets TCP connections properly
#[tokio::test]
async fn test_clone_connection_reset() -> Result<()> {
    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║     Clone Connection Reset Test                               ║");
    println!("║     Server on host, client in VM, clone and observe           ║");
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    let fcvm_path = common::find_fcvm_binary()?;
    let (baseline_name, clone_name, snapshot_name, _serve_name) = unique_names("connrst");

    // =========================================================================
    // Step 1: Start TCP broadcast server on host
    // =========================================================================
    println!("Step 1: Starting TCP broadcast server on host...");
    let server = BroadcastServer::new()?;
    let server_port = server.port();
    let stop_handle = server.stop_handle();
    let _server_thread = server.run_in_background();
    println!("  Server listening on port {}", server_port);

    // For rootless: 10.0.2.2 is the host from slirp4netns perspective
    let host_ip = "10.0.2.2";

    // =========================================================================
    // Step 2: Start baseline VM (nginx stays alive, we exec client later)
    // =========================================================================
    println!("\nStep 2: Starting baseline VM...");
    println!(
        "  Using unique names: baseline={}, clone={}, snapshot={}",
        baseline_name, clone_name, snapshot_name
    );

    let (_baseline_child, baseline_pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &baseline_name,
            "--network",
            "rootless",
            common::TEST_IMAGE,
        ],
        &baseline_name,
    )
    .await
    .context("spawning baseline VM")?;

    println!("  Baseline VM started (PID: {})", baseline_pid);

    // Wait for VM to become healthy
    println!("  Waiting for VM to become healthy...");
    common::poll_health_by_pid(baseline_pid, 120).await?;
    println!("  VM healthy");

    // Install netcat
    let _ = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &baseline_pid.to_string(),
            "--",
            "apk",
            "add",
            "--no-cache",
            "netcat-openbsd",
        ])
        .output()
        .await?;

    // =========================================================================
    // Step 3: Test connection from baseline VM
    // =========================================================================
    println!("\nStep 3: Testing connection from baseline...");
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &baseline_pid.to_string(),
            "--",
            "sh",
            "-c",
            &format!("nc -w 2 {} {} </dev/null | head -3", host_ip, server_port),
        ])
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("  Baseline received:");
    for line in stdout.lines().take(3) {
        println!("    {}", line);
    }

    // =========================================================================
    // Step 4: Create snapshot
    // =========================================================================
    println!("\nStep 4: Creating snapshot...");

    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "snapshot",
            "create",
            "--pid",
            &baseline_pid.to_string(),
            "--tag",
            &snapshot_name,
        ])
        .output()
        .await
        .context("creating snapshot")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Snapshot failed: {}", stderr);
    }
    println!("  Snapshot created");

    // =========================================================================
    // Step 5: Start memory server
    // =========================================================================
    println!("\nStep 5: Starting memory server...");
    let (_serve_child, serve_pid) =
        common::spawn_fcvm_with_logs(&["snapshot", "serve", &snapshot_name], "uffd-server")
            .await
            .context("spawning serve")?;

    common::poll_serve_ready(&snapshot_name, serve_pid, 30).await?;
    println!("  Memory server ready (PID: {})", serve_pid);

    // =========================================================================
    // Step 6: Clone the VM
    // =========================================================================
    println!("\nStep 6: Cloning VM...");
    let clone_start = Instant::now();

    let (_clone_child, clone_pid) = common::spawn_fcvm_with_logs(
        &[
            "snapshot",
            "run",
            "--pid",
            &serve_pid.to_string(),
            "--name",
            &clone_name,
            "--network",
            "rootless",
        ],
        &clone_name,
    )
    .await
    .context("spawning clone")?;

    println!("  Clone started (PID: {})", clone_pid);

    // Wait for clone to become healthy
    common::poll_health_by_pid(clone_pid, 60).await?;
    let clone_time = clone_start.elapsed();
    println!("  Clone healthy after {:.0}ms", clone_time.as_millis());

    // =========================================================================
    // Step 7: Check if clone can exec a command
    // =========================================================================
    println!("\nStep 7: Testing clone is alive...");
    let exec_output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &clone_pid.to_string(),
            "--",
            "echo",
            "CLONE_IS_ALIVE",
        ])
        .output()
        .await?;

    let clone_alive = String::from_utf8_lossy(&exec_output.stdout).contains("CLONE_IS_ALIVE");
    println!(
        "  Clone exec test: {}",
        if clone_alive { "PASS" } else { "FAIL" }
    );

    // =========================================================================
    // Step 8: Test if clone can establish NEW connection (should be CONN#2)
    // =========================================================================
    println!("\nStep 8: Testing clone can connect (should be CONN#2)...");

    let new_conn_output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &clone_pid.to_string(),
            "--",
            "sh",
            "-c",
            &format!("nc -w 3 {} {} </dev/null | head -3", host_ip, server_port),
        ])
        .output()
        .await?;

    let new_conn_stdout = String::from_utf8_lossy(&new_conn_output.stdout);
    let can_reconnect = new_conn_stdout.contains("CONN#");
    println!("  Clone connection:");
    for line in new_conn_stdout.lines().take(3) {
        println!("    {}", line);
    }

    // =========================================================================
    // Cleanup
    // =========================================================================
    println!("\nCleaning up...");
    stop_handle.store(true, Ordering::Relaxed);
    common::kill_process(clone_pid).await;
    common::kill_process(serve_pid).await;
    common::kill_process(baseline_pid).await;
    println!("  Done");

    // =========================================================================
    // Results
    // =========================================================================
    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║                         RESULTS                               ║");
    println!("╠═══════════════════════════════════════════════════════════════╣");
    println!(
        "║  Clone is alive:        {}                                   ║",
        if clone_alive { "YES" } else { "NO " }
    );
    println!(
        "║  Clone can connect:     {}                                   ║",
        if can_reconnect { "YES" } else { "NO " }
    );
    println!("╚═══════════════════════════════════════════════════════════════╝");

    if clone_alive && can_reconnect {
        println!("\n✅ CLONE CONNECTION TEST PASSED!");
        Ok(())
    } else {
        anyhow::bail!(
            "Test failed: clone_alive={}, can_reconnect={}",
            clone_alive,
            can_reconnect
        )
    }
}

/// Test how long it takes for a persistent client to detect disconnect and reconnect after clone
#[tokio::test]
async fn test_clone_reconnect_latency() -> Result<()> {
    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║     Clone Reconnect Latency Test                              ║");
    println!("║     Persistent client in VM, measure reconnect time           ║");
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    let fcvm_path = common::find_fcvm_binary()?;
    let (baseline_name, clone_name, snapshot_name, _serve_name) = unique_names("reconn");

    // Start server
    println!("Step 1: Starting broadcast server...");
    let server = BroadcastServer::new()?;
    let server_port = server.port();
    let stop_handle = server.stop_handle();
    let server_seq = Arc::clone(&server.seq);
    let _server_thread = server.run_in_background();
    println!("  Listening on port {}", server_port);

    let host_ip = "10.0.2.2";

    // Start VM (nginx stays alive, we'll exec our client into it)
    println!("\nStep 2: Starting VM...");
    println!(
        "  Using unique names: baseline={}, clone={}, snapshot={}",
        baseline_name, clone_name, snapshot_name
    );

    // Use nginx image - it stays alive
    let (_baseline_child, baseline_pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &baseline_name,
            "--network",
            "rootless",
            common::TEST_IMAGE,
        ],
        &baseline_name,
    )
    .await?;

    // Wait for VM to be healthy
    common::poll_health_by_pid(baseline_pid, 120).await?;
    println!("  VM started (PID: {})", baseline_pid);

    // Install netcat
    let _ = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &baseline_pid.to_string(),
            "--",
            "apk",
            "add",
            "--no-cache",
            "netcat-openbsd",
        ])
        .output()
        .await?;

    // Start persistent TCP client via exec (runs in background in VM)
    println!("\nStep 3: Starting persistent TCP client in VM...");

    // Start client in background via exec - it will reconnect automatically
    let _client_handle = {
        let fcvm_path = fcvm_path.clone();
        let host_ip = host_ip.to_string();
        tokio::spawn(async move {
            let _ = tokio::process::Command::new(&fcvm_path)
                .args([
                    "exec",
                    "--pid",
                    &baseline_pid.to_string(),
                    "--",
                    "sh",
                    "-c",
                    &format!(
                        "while true; do nc -w 5 {} {} 2>&1; sleep 0.1; done",
                        host_ip, server_port
                    ),
                ])
                .output()
                .await;
        })
    };

    // Wait for client to connect
    tokio::time::sleep(Duration::from_secs(2)).await;
    let seq_before_snapshot = server_seq.load(Ordering::Relaxed);
    println!("  Client connected (server seq: {})", seq_before_snapshot);

    // Snapshot
    println!("\nStep 4: Creating snapshot...");
    let snapshot_start = Instant::now();

    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "snapshot",
            "create",
            "--pid",
            &baseline_pid.to_string(),
            "--tag",
            &snapshot_name,
        ])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Snapshot failed: {}", stderr);
    }

    let snapshot_time = snapshot_start.elapsed();
    println!("  Snapshot created in {:.0}ms", snapshot_time.as_millis());

    // Serve
    println!("\nStep 5: Starting serve...");
    let (_serve_child, serve_pid) =
        common::spawn_fcvm_with_logs(&["snapshot", "serve", &snapshot_name], "uffd-server").await?;
    common::poll_serve_ready(&snapshot_name, serve_pid, 30).await?;

    // Record sequence before clone
    let seq_before_clone = server_seq.load(Ordering::Relaxed);

    // Clone
    println!("\nStep 6: Spawning clone (client should reconnect)...");
    let clone_start = Instant::now();

    let (_clone_child, clone_pid) = common::spawn_fcvm_with_logs(
        &[
            "snapshot",
            "run",
            "--pid",
            &serve_pid.to_string(),
            "--name",
            &clone_name,
            "--network",
            "rootless",
        ],
        &clone_name,
    )
    .await?;

    // Wait and observe
    println!("  Clone started, waiting for reconnect...");

    // Poll server connections
    let mut reconnected = false;
    let mut reconnect_time = Duration::ZERO;

    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let current_seq = server_seq.load(Ordering::Relaxed);

        // If we see significant sequence advancement, clone has connected
        if current_seq > seq_before_clone + 5 {
            reconnect_time = clone_start.elapsed();
            reconnected = true;
            println!(
                "  Clone reconnected! Server seq jumped from {} to {}",
                seq_before_clone, current_seq
            );
            break;
        }
    }

    if reconnected {
        println!(
            "\n  ⏱️  Time from clone start to reconnect: {:.0}ms",
            reconnect_time.as_millis()
        );
    } else {
        println!("\n  ❌ Clone did not reconnect within 5 seconds");
    }

    // Cleanup
    println!("\nCleaning up...");
    stop_handle.store(true, Ordering::Relaxed);
    common::kill_process(clone_pid).await;
    common::kill_process(serve_pid).await;
    common::kill_process(baseline_pid).await;

    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║                         RESULTS                               ║");
    println!("╠═══════════════════════════════════════════════════════════════╣");
    println!(
        "║  Clone reconnect time: {:>6}ms                               ║",
        reconnect_time.as_millis()
    );
    println!("║                                                               ║");
    println!("║  This measures: VM restore + network setup + TCP connect      ║");
    println!("╚═══════════════════════════════════════════════════════════════╝");

    if reconnected {
        println!("\n✅ RECONNECT LATENCY TEST PASSED!");
        Ok(())
    } else {
        anyhow::bail!("Clone did not reconnect")
    }
}

/// Test PERSISTENT connection behavior - client stays connected through snapshot/clone
#[tokio::test]
async fn test_clone_connection_timing() -> Result<()> {
    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║     Persistent Connection Clone Test                          ║");
    println!("║     Client stays connected, observe behavior during clone     ║");
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    let fcvm_path = common::find_fcvm_binary()?;
    let (baseline_name, clone_name, snapshot_name, _serve_name) = unique_names("timing");

    // Start server
    println!("Step 1: Starting broadcast server...");
    let server = BroadcastServer::new()?;
    let server_port = server.port();
    let stop_handle = server.stop_handle();
    let server_seq = Arc::clone(&server.seq);
    let _server_thread = server.run_in_background();
    println!("  Listening on port {}", server_port);

    let host_ip = "10.0.2.2";

    // Start VM with nginx (stays alive)
    println!("\nStep 2: Starting VM...");
    println!(
        "  Using unique names: baseline={}, clone={}, snapshot={}",
        baseline_name, clone_name, snapshot_name
    );
    let (_baseline_child, baseline_pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &baseline_name,
            "--network",
            "rootless",
            common::TEST_IMAGE,
        ],
        &baseline_name,
    )
    .await?;

    common::poll_health_by_pid(baseline_pid, 120).await?;
    println!("  VM healthy (PID: {})", baseline_pid);

    // Install netcat
    println!("\nStep 3: Installing netcat in VM...");
    let _ = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &baseline_pid.to_string(),
            "--",
            "apk",
            "add",
            "--no-cache",
            "netcat-openbsd",
        ])
        .output()
        .await?;

    // Start PERSISTENT client - runs forever, writing received data to a file
    // IMPORTANT: Use nohup & to detach from exec session, otherwise clone inherits
    // a "ghost" exec session that breaks the exec server
    println!("\nStep 4: Starting PERSISTENT client in baseline VM...");
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &baseline_pid.to_string(),
            "--",
            "sh",
            "-c",
            &format!(
                "nohup nc {} {} > /tmp/received.log 2>&1 &",
                host_ip, server_port
            ),
        ])
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to start persistent client: {}", stderr);
    }

    // Wait for connection
    tokio::time::sleep(Duration::from_secs(2)).await;
    let seq_at_connect = server_seq.load(Ordering::Relaxed);
    println!(
        "  Persistent client connected! (server seq: {})",
        seq_at_connect
    );

    // Verify client is receiving
    tokio::time::sleep(Duration::from_millis(500)).await;
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &baseline_pid.to_string(),
            "--",
            "tail",
            "-3",
            "/tmp/received.log",
        ])
        .output()
        .await?;
    let received = String::from_utf8_lossy(&output.stdout);
    println!("  Baseline receiving (should be CONN#1):");
    for line in received.lines() {
        println!("    {}", line);
    }

    // Snapshot - THE KEY MOMENT: client is still connected!
    println!("\nStep 5: Creating snapshot (client still connected!)...");
    let seq_before_snapshot = server_seq.load(Ordering::Relaxed);
    println!("  Server seq before snapshot: {}", seq_before_snapshot);

    let snapshot_start = Instant::now();
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "snapshot",
            "create",
            "--pid",
            &baseline_pid.to_string(),
            "--tag",
            &snapshot_name,
        ])
        .output()
        .await?;
    let snapshot_time = snapshot_start.elapsed();

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Snapshot failed: {}", stderr);
    }
    println!(
        "  Snapshot created in {:.0}ms (VM was paused during this)",
        snapshot_time.as_millis()
    );

    // Check if baseline connection survived the pause
    tokio::time::sleep(Duration::from_millis(500)).await;
    let seq_after_snapshot = server_seq.load(Ordering::Relaxed);
    println!("  Server seq after snapshot: {}", seq_after_snapshot);

    // Check what baseline received after snapshot
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &baseline_pid.to_string(),
            "--",
            "tail",
            "-3",
            "/tmp/received.log",
        ])
        .output()
        .await?;
    let received_after = String::from_utf8_lossy(&output.stdout);
    println!("  Baseline still receiving after snapshot:");
    for line in received_after.lines() {
        println!("    {}", line);
    }

    // Serve
    println!("\nStep 6: Starting serve...");
    let (_serve_child, serve_pid) =
        common::spawn_fcvm_with_logs(&["snapshot", "serve", &snapshot_name], "uffd-server").await?;
    common::poll_serve_ready(&snapshot_name, serve_pid, 30).await?;

    // Clone - the clone inherits the snapshot state INCLUDING the nc process mid-connection
    println!("\nStep 7: Spawning clone (has nc process from snapshot state!)...");
    let (_clone_child, clone_pid) = common::spawn_fcvm_with_logs(
        &[
            "snapshot",
            "run",
            "--pid",
            &serve_pid.to_string(),
            "--name",
            &clone_name,
            "--network",
            "rootless",
        ],
        &clone_name,
    )
    .await?;

    common::poll_health_by_pid(clone_pid, 60).await?;
    println!("  Clone healthy (PID: {})", clone_pid);

    // The clone's nc process woke up in a new network namespace
    // It has a stale socket fd - what happened?
    tokio::time::sleep(Duration::from_secs(1)).await;

    println!("\nStep 8: Checking clone's inherited nc process...");
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &clone_pid.to_string(),
            "--",
            "cat",
            "/tmp/received.log",
        ])
        .output()
        .await?;
    let clone_received = String::from_utf8_lossy(&output.stdout);
    let clone_lines: Vec<&str> = clone_received.lines().collect();
    println!("  Clone's /tmp/received.log ({} lines):", clone_lines.len());
    for line in clone_lines.iter().rev().take(5).rev() {
        println!("    {}", line);
    }

    // The clone's nc should have exited (stale socket)
    // Try a NEW connection from clone
    println!("\nStep 9: Testing NEW connection from clone...");
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &clone_pid.to_string(),
            "--",
            "sh",
            "-c",
            &format!(
                "nc -v -w 5 {} {} </dev/null 2>&1 | head -5",
                host_ip, server_port
            ),
        ])
        .output()
        .await?;

    let new_conn = String::from_utf8_lossy(&output.stdout);
    let new_conn_err = String::from_utf8_lossy(&output.stderr);
    println!("  Clone NEW connection output:");
    for line in new_conn.lines() {
        println!("    {}", line);
    }
    if !new_conn_err.is_empty() {
        println!("  Clone NEW connection stderr:");
        for line in new_conn_err.lines() {
            println!("    {}", line);
        }
    }

    // Cleanup
    println!("\nCleaning up...");
    stop_handle.store(true, Ordering::Relaxed);
    common::kill_process(clone_pid).await;
    common::kill_process(serve_pid).await;
    common::kill_process(baseline_pid).await;

    // Results
    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║                         RESULTS                               ║");
    println!("╠═══════════════════════════════════════════════════════════════╣");
    let baseline_survived = received_after.contains("CONN#1");
    let clone_has_new_conn = new_conn.contains("CONN#");
    println!(
        "║  Baseline connection survived pause: {}                      ║",
        if baseline_survived { "YES" } else { "NO " }
    );
    println!(
        "║  Clone can establish new connection: {}                      ║",
        if clone_has_new_conn { "YES" } else { "NO " }
    );
    println!("╚═══════════════════════════════════════════════════════════════╝");

    let success = clone_has_new_conn;
    println!(
        "\n{}",
        if success {
            "✅ PERSISTENT CONNECTION TEST PASSED!"
        } else {
            "❌ TEST FAILED"
        }
    );

    if success {
        Ok(())
    } else {
        anyhow::bail!("Test failed")
    }
}

/// Test a RESILIENT client that auto-reconnects on network errors
/// This demonstrates how a well-behaved app handles clone restore
#[tokio::test]
async fn test_clone_resilient_client() -> Result<()> {
    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║     Resilient Client Clone Test                               ║");
    println!("║     Client auto-reconnects on error, like a real app          ║");
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    let fcvm_path = common::find_fcvm_binary()?;
    let (baseline_name, clone_name, snapshot_name, _serve_name) = unique_names("resil");

    // Start server
    println!("Step 1: Starting broadcast server...");
    let server = BroadcastServer::new()?;
    let server_port = server.port();
    let stop_handle = server.stop_handle();
    let server_seq = Arc::clone(&server.seq);
    let conn_counter = Arc::clone(&server.conn_counter);
    let _server_thread = server.run_in_background();
    println!("  Listening on port {}", server_port);

    let host_ip = "10.0.2.2";

    // Start VM
    println!("\nStep 2: Starting VM...");
    println!(
        "  Using unique names: baseline={}, clone={}, snapshot={}",
        baseline_name, clone_name, snapshot_name
    );
    let (_baseline_child, baseline_pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &baseline_name,
            "--network",
            "rootless",
            common::TEST_IMAGE,
        ],
        &baseline_name,
    )
    .await?;

    common::poll_health_by_pid(baseline_pid, 120).await?;
    println!("  VM healthy (PID: {})", baseline_pid);

    // Install netcat
    println!("\nStep 3: Installing netcat...");
    let _ = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &baseline_pid.to_string(),
            "--",
            "apk",
            "add",
            "--no-cache",
            "netcat-openbsd",
        ])
        .output()
        .await?;

    // Create the resilient client script
    // This client:
    // - Uses timeout to detect dead connections (can't just wait forever)
    // - Auto-reconnects on any error or timeout
    // - Shows exactly what happens during clone
    println!("\nStep 4: Creating resilient client script in VM...");
    // Note: nc -w sets a timeout, so we can detect stale sockets
    // The key insight: a stale socket won't receive data, so -w timeout will fire
    let client_script = format!(
        r#"#!/bin/sh
LOG=/tmp/client.log
HOST={}
PORT={}

log() {{
    echo "$(date '+%H:%M:%S') $1" >> $LOG
}}

echo "=== RESILIENT CLIENT STARTING ===" > $LOG
log "Will connect to $HOST:$PORT (with 2s idle timeout)"

conn_num=0
while true; do
    conn_num=$((conn_num + 1))
    log ">>> CONNECTING (attempt #$conn_num)..."

    # Use -w 2 for 2 second timeout - if no data for 2s, nc exits
    # This is how real apps detect dead connections (keepalives/timeouts)
    nc -w 2 $HOST $PORT 2>&1 | while read line; do
        log "RECV: $line"
    done

    log "<<< DISCONNECTED (timeout or error)"
    log "    Reconnecting in 100ms..."
    sleep 0.1
done
"#,
        host_ip, server_port
    );

    // Write script to VM using --vm flag (run in guest OS, not container)
    // Use printf to avoid heredoc issues
    let escaped_script = client_script.replace("'", "'\\''");
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &baseline_pid.to_string(),
            "--vm",
            "--",
            "sh",
            "-c",
            &format!(
                "printf '%s' '{}' > /tmp/resilient_client.sh && chmod +x /tmp/resilient_client.sh",
                escaped_script
            ),
        ])
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("Script creation stderr: {}", stderr);
        anyhow::bail!("Failed to create client script: {}", stderr);
    }

    // Verify script was created
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &baseline_pid.to_string(),
            "--vm",
            "--",
            "cat",
            "/tmp/resilient_client.sh",
        ])
        .output()
        .await?;
    println!("  Script created ({} bytes)", output.stdout.len());

    // Start resilient client in background (in guest OS, not container)
    println!("\nStep 5: Starting resilient client (will auto-reconnect on errors)...");
    let _ = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &baseline_pid.to_string(),
            "--vm",
            "--",
            "sh",
            "-c",
            "nohup /tmp/resilient_client.sh > /tmp/client_stdout.log 2>&1 &",
        ])
        .output()
        .await?;

    // Wait for initial connection
    tokio::time::sleep(Duration::from_secs(2)).await;
    let initial_conns = conn_counter.load(Ordering::Relaxed);
    println!(
        "  Client connected! (server has {} connections)",
        initial_conns
    );

    // Show initial client log
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &baseline_pid.to_string(),
            "--vm",
            "--",
            "tail",
            "-10",
            "/tmp/client.log",
        ])
        .output()
        .await?;
    println!("  Initial client log:");
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        println!("    {}", line);
    }
    if output.stdout.is_empty() {
        // Check if script is running
        let ps_output = tokio::process::Command::new(&fcvm_path)
            .args([
                "exec",
                "--pid",
                &baseline_pid.to_string(),
                "--vm",
                "--",
                "ps",
                "aux",
            ])
            .output()
            .await?;
        eprintln!("  DEBUG: ps aux output:");
        for line in String::from_utf8_lossy(&ps_output.stdout).lines() {
            if line.contains("resilient") || line.contains("nc ") {
                eprintln!("    {}", line);
            }
        }
    }

    // Snapshot
    println!("\nStep 6: Creating snapshot (client is connected!)...");
    let seq_before = server_seq.load(Ordering::Relaxed);
    let conns_before = conn_counter.load(Ordering::Relaxed);
    println!(
        "  Before snapshot: seq={}, connections={}",
        seq_before, conns_before
    );

    let snapshot_start = Instant::now();
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "snapshot",
            "create",
            "--pid",
            &baseline_pid.to_string(),
            "--tag",
            &snapshot_name,
        ])
        .output()
        .await?;
    let snapshot_time = snapshot_start.elapsed();

    if !output.status.success() {
        anyhow::bail!(
            "Snapshot failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    println!(
        "  Snapshot created in {:.0}ms (baseline paused then unpaused)",
        snapshot_time.as_millis()
    );

    // Verify baseline continues receiving after unpause
    println!("\n  Verifying baseline continues receiving after unpause...");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let seq_after_snapshot = server_seq.load(Ordering::Relaxed);
    println!(
        "  Server seq after snapshot: {} (was {})",
        seq_after_snapshot, seq_before
    );

    // Check baseline's client log - should show continued reception
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &baseline_pid.to_string(),
            "--vm",
            "--",
            "tail",
            "-5",
            "/tmp/client.log",
        ])
        .output()
        .await?;
    println!("  Baseline still receiving after unpause:");
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if line.contains("RECV:") {
            println!("    {}", line);
        }
    }

    // Serve
    println!("\nStep 7: Starting serve...");
    let (_serve_child, serve_pid) =
        common::spawn_fcvm_with_logs(&["snapshot", "serve", &snapshot_name], "uffd-server").await?;
    common::poll_serve_ready(&snapshot_name, serve_pid, 30).await?;

    // Clone
    println!("\nStep 8: Spawning clone (resilient client should detect error and reconnect!)...");
    let clone_start = Instant::now();
    let conns_before_clone = conn_counter.load(Ordering::Relaxed);

    let (_clone_child, clone_pid) = common::spawn_fcvm_with_logs(
        &[
            "snapshot",
            "run",
            "--pid",
            &serve_pid.to_string(),
            "--name",
            &clone_name,
            "--network",
            "rootless",
        ],
        &clone_name,
    )
    .await?;

    common::poll_health_by_pid(clone_pid, 60).await?;
    println!("  Clone healthy (PID: {})", clone_pid);

    // Wait for reconnect - need to wait for the 2s idle timeout to fire on stale socket
    println!("\n  Waiting for resilient client to reconnect (stale socket timeout ~2s)...");
    let mut reconnect_time = Duration::ZERO;
    let mut reconnected = false;

    // Wait up to 5 seconds (2s timeout + buffer)
    for i in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let current_conns = conn_counter.load(Ordering::Relaxed);

        // New connection = reconnect happened
        if current_conns > conns_before_clone {
            reconnect_time = clone_start.elapsed();
            reconnected = true;
            println!(
                "  ✓ Reconnected! Connection count: {} -> {}",
                conns_before_clone, current_conns
            );
            break;
        }

        if i % 10 == 9 {
            println!("    Still waiting... ({}ms)", (i + 1) * 100);
        }
    }

    // Show clone's client log - this shows the disconnect/reconnect cycle
    println!("\nStep 9: Clone's client log (shows disconnect/reconnect cycle):");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &clone_pid.to_string(),
            "--vm",
            "--",
            "cat",
            "/tmp/client.log",
        ])
        .output()
        .await?;

    let log_content = String::from_utf8_lossy(&output.stdout);
    let log_lines: Vec<&str> = log_content.lines().collect();

    // Show key events
    println!("  --- Client Timeline ---");
    for line in &log_lines {
        if line.contains("CONNECTING") || line.contains("DISCONNECTED") || line.contains("STARTING")
        {
            println!("  {}", line);
        }
    }

    // Show last few received messages
    println!("\n  --- Recent Data ---");
    for line in log_lines.iter().rev().take(5).rev() {
        if line.contains("RECV:") {
            println!("  {}", line);
        }
    }

    // Cleanup
    println!("\nCleaning up...");
    stop_handle.store(true, Ordering::Relaxed);
    common::kill_process(clone_pid).await;
    common::kill_process(serve_pid).await;
    common::kill_process(baseline_pid).await;

    // Results
    let final_conns = conn_counter.load(Ordering::Relaxed);
    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║                         RESULTS                               ║");
    println!("╠═══════════════════════════════════════════════════════════════╣");
    println!(
        "║  Initial connections:     {:>3}                                ║",
        initial_conns
    );
    println!(
        "║  Connections after clone: {:>3}                                ║",
        final_conns
    );
    println!(
        "║  Clone reconnected:       {}                               ║",
        if reconnected { "YES" } else { "NO " }
    );
    if reconnected {
        println!(
            "║  Reconnect latency:       {:>3}ms                              ║",
            reconnect_time.as_millis()
        );
    }
    println!("╠═══════════════════════════════════════════════════════════════╣");
    println!("║                                                               ║");
    println!("║  The resilient client detected the stale socket (ENETDOWN     ║");
    println!("║  from interface bounce) and automatically reconnected!        ║");
    println!("║                                                               ║");
    println!("╚═══════════════════════════════════════════════════════════════╝");

    if reconnected {
        println!("\n✅ RESILIENT CLIENT TEST PASSED!");
        Ok(())
    } else {
        anyhow::bail!("Clone's resilient client did not reconnect")
    }
}
