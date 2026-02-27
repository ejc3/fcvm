//! Tests for signal handling and cleanup
//!
//! Verifies that when fcvm receives SIGINT/SIGTERM, it properly cleans up
//! child processes (firecracker, pasta, etc.)

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use std::process::Command;
use std::time::Duration;

/// Check if fcvm ls JSON output indicates a healthy VM (proper JSON parsing, not string matching)
fn is_vm_healthy(json_str: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(json_str)
        .ok()
        .and_then(|v| {
            v.as_array()?
                .first()?
                .get("health_status")?
                .as_str()
                .map(|s| s == "healthy")
        })
        .unwrap_or(false)
}

/// Check if a process with the given PID exists
fn process_exists(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{}", pid)).exists()
}

/// Send a signal to a process
fn send_signal(pid: u32, signal: &str) -> Result<()> {
    let output = Command::new("kill")
        .arg(format!("-{}", signal))
        .arg(pid.to_string())
        .output()
        .context("running kill command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("kill failed: {}", stderr);
    }
    Ok(())
}

/// Test that SIGINT properly kills the VM and cleans up firecracker
///
/// NOTE: This test tracks SPECIFIC PIDs rather than global process counts to work
/// correctly when running in parallel with other tests.
#[cfg(feature = "privileged-tests")]
#[test]
fn test_sigint_kills_firecracker_bridged() -> Result<()> {
    println!("\ntest_sigint_kills_firecracker_bridged");

    // Start fcvm in background
    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names("signal-int");
    let mut fcvm = Command::new(&fcvm_path)
        .args([
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "bridged",
            common::TEST_IMAGE,
        ])
        .spawn()
        .context("spawning fcvm")?;

    let fcvm_pid = fcvm.id();
    println!("Started fcvm with PID: {}", fcvm_pid);

    // Wait for VM to become healthy (max 60 seconds)
    let start = std::time::Instant::now();
    let mut healthy = false;
    while start.elapsed() < Duration::from_secs(120) {
        std::thread::sleep(common::POLL_INTERVAL);

        // IMPORTANT: Use --pid to query only OUR VM, not all VMs (parallel test safety)
        let output = Command::new(&fcvm_path)
            .args(["ls", "--json", "--pid", &fcvm_pid.to_string()])
            .output()
            .context("running fcvm ls")?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        if is_vm_healthy(&stdout) {
            healthy = true;
            println!("VM is healthy after {:?}", start.elapsed());
            break;
        }
    }

    if !healthy {
        // Kill fcvm gracefully if it didn't become healthy
        fcvm::utils::graceful_kill(fcvm_pid, 2000);
        let _ = fcvm.wait();
        anyhow::bail!("VM did not become healthy within 60 seconds");
    }

    // Find the specific firecracker process for THIS VM
    let our_fc_pid = find_firecracker_for_fcvm(fcvm_pid);
    println!("Our firecracker PID: {:?}", our_fc_pid);

    // Verify firecracker is running
    assert!(
        our_fc_pid.is_some(),
        "should have started a firecracker process"
    );
    let fc_pid = our_fc_pid.unwrap();
    assert!(
        process_exists(fc_pid),
        "firecracker should be running before SIGINT"
    );

    // Send SIGINT to fcvm (simulates Ctrl-C)
    println!("Sending SIGINT to fcvm (PID {})", fcvm_pid);
    send_signal(fcvm_pid, "INT").context("sending SIGINT to fcvm")?;

    // Wait for fcvm to exit (max 30 seconds — cleanup can be slow under CI load)
    let start = std::time::Instant::now();
    let mut exited = false;
    while start.elapsed() < Duration::from_secs(30) {
        match fcvm.try_wait() {
            Ok(Some(status)) => {
                println!("fcvm exited with status: {:?}", status);
                exited = true;
                break;
            }
            Ok(None) => {
                std::thread::sleep(common::POLL_INTERVAL);
            }
            Err(e) => {
                println!("Error waiting for fcvm: {}", e);
                break;
            }
        }
    }

    if !exited {
        println!("fcvm didn't exit after SIGINT, killing forcefully");
        let _ = fcvm.kill();
        let _ = fcvm.wait();
    }

    // Poll for firecracker cleanup (max 15 seconds — under CI load, cleanup can be slow)
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(15) {
        if !process_exists(fc_pid) {
            break;
        }
        std::thread::sleep(common::POLL_INTERVAL);
    }

    // Check if our specific firecracker is still running
    let still_running = process_exists(fc_pid);
    if still_running {
        // This is a bug - firecracker should have been killed
        println!(
            "BUG: firecracker (PID {}) is still running after fcvm exit!",
            fc_pid
        );
        // Clean up for the test
        let _ = send_signal(fc_pid, "KILL");
    }
    assert!(
        !still_running,
        "firecracker (PID {}) should be killed when fcvm receives SIGINT",
        fc_pid
    );

    // Verify fcvm process itself is gone
    assert!(
        !process_exists(fcvm_pid),
        "fcvm process (PID {}) should be terminated",
        fcvm_pid
    );

    println!("test_sigint_kills_firecracker_bridged PASSED");
    Ok(())
}

/// Test that SIGTERM properly kills the VM and cleans up firecracker
///
/// NOTE: This test tracks SPECIFIC PIDs rather than global process counts to work
/// correctly when running in parallel with other tests.
#[cfg(feature = "privileged-tests")]
#[test]
fn test_sigterm_kills_firecracker_bridged() -> Result<()> {
    println!("\ntest_sigterm_kills_firecracker_bridged");

    // Start fcvm in background
    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names("signal-term");
    let mut fcvm = Command::new(&fcvm_path)
        .args([
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "bridged",
            common::TEST_IMAGE,
        ])
        .spawn()
        .context("spawning fcvm")?;

    let fcvm_pid = fcvm.id();
    println!("Started fcvm with PID: {}", fcvm_pid);

    // Wait for VM to become healthy (max 60 seconds)
    // IMPORTANT: Use --pid to query only OUR VM, not all VMs (parallel test safety)
    let start = std::time::Instant::now();
    let mut healthy = false;
    while start.elapsed() < Duration::from_secs(120) {
        std::thread::sleep(common::POLL_INTERVAL);

        let output = Command::new(&fcvm_path)
            .args(["ls", "--json", "--pid", &fcvm_pid.to_string()])
            .output()
            .context("running fcvm ls")?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        if is_vm_healthy(&stdout) {
            healthy = true;
            println!("VM is healthy after {:?}", start.elapsed());
            break;
        }
    }

    if !healthy {
        fcvm::utils::graceful_kill(fcvm_pid, 2000);
        let _ = fcvm.wait();
        anyhow::bail!("VM did not become healthy within 60 seconds");
    }

    // Find the specific firecracker process for THIS VM
    let our_fc_pid = find_firecracker_for_fcvm(fcvm_pid);
    println!("Our firecracker PID: {:?}", our_fc_pid);

    // Verify firecracker is running
    assert!(
        our_fc_pid.is_some(),
        "should have started a firecracker process"
    );
    let fc_pid = our_fc_pid.unwrap();

    // Send SIGTERM to fcvm
    println!("Sending SIGTERM to fcvm (PID {})", fcvm_pid);
    send_signal(fcvm_pid, "TERM").context("sending SIGTERM to fcvm")?;

    // Wait for fcvm to exit (max 30 seconds — cleanup can be slow under CI load)
    let start = std::time::Instant::now();
    let mut exited = false;
    while start.elapsed() < Duration::from_secs(30) {
        match fcvm.try_wait() {
            Ok(Some(status)) => {
                println!("fcvm exited with status: {:?}", status);
                exited = true;
                break;
            }
            Ok(None) => {
                std::thread::sleep(common::POLL_INTERVAL);
            }
            Err(_) => break,
        }
    }

    if !exited {
        println!("fcvm didn't exit after SIGTERM, killing forcefully");
        let _ = fcvm.kill();
        let _ = fcvm.wait();
    }

    // Poll for firecracker cleanup (max 15 seconds — under CI load, cleanup can be slow)
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(15) {
        if !process_exists(fc_pid) {
            break;
        }
        std::thread::sleep(common::POLL_INTERVAL);
    }

    // Check if our specific firecracker is still running
    let still_running = process_exists(fc_pid);
    if still_running {
        println!(
            "BUG: firecracker (PID {}) is still running after fcvm exit!",
            fc_pid
        );
        let _ = send_signal(fc_pid, "KILL");
    }
    assert!(
        !still_running,
        "firecracker (PID {}) should be killed when fcvm receives SIGTERM",
        fc_pid
    );

    // Verify fcvm process itself is gone
    assert!(
        !process_exists(fcvm_pid),
        "fcvm process (PID {}) should be terminated",
        fcvm_pid
    );

    println!("test_sigterm_kills_firecracker_bridged PASSED");
    Ok(())
}

/// Test that SIGTERM properly kills the VM and cleans up ALL resources in rootless mode
/// This includes: firecracker, pasta, namespace holder, and state files
///
/// NOTE: This test tracks SPECIFIC PIDs rather than global process counts to work
/// correctly when running in parallel with other tests.
#[test]
fn test_sigterm_cleanup_rootless() -> Result<()> {
    println!("\ntest_sigterm_cleanup_rootless");

    // Start fcvm in rootless mode
    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names("cleanup-rootless");
    let mut fcvm = Command::new(&fcvm_path)
        .args([
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "rootless",
            common::TEST_IMAGE,
        ])
        .spawn()
        .context("spawning fcvm")?;

    let fcvm_pid = fcvm.id();
    println!("Started fcvm with PID: {}", fcvm_pid);

    // Wait for VM to become healthy (max 60 seconds)
    // IMPORTANT: Use --pid to query only OUR VM, not all VMs (parallel test safety)
    let start = std::time::Instant::now();
    let mut healthy = false;
    while start.elapsed() < Duration::from_secs(120) {
        std::thread::sleep(common::POLL_INTERVAL);

        let output = Command::new(&fcvm_path)
            .args(["ls", "--json", "--pid", &fcvm_pid.to_string()])
            .output()
            .context("running fcvm ls")?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        if is_vm_healthy(&stdout) {
            healthy = true;
            println!("VM is healthy after {:?}", start.elapsed());
            break;
        }
    }

    if !healthy {
        fcvm::utils::graceful_kill(fcvm_pid, 2000);
        let _ = fcvm.wait();
        anyhow::bail!("VM did not become healthy within 60 seconds");
    }

    // Find the specific firecracker process for THIS VM by looking for our VM name pattern
    // The VM ID contains the unique name prefix, so we can find our specific process
    let our_fc_pid = find_firecracker_for_fcvm(fcvm_pid);
    let our_pasta_pid = find_pasta_for_fcvm(fcvm_pid);
    println!(
        "Our processes: firecracker={:?}, pasta={:?}",
        our_fc_pid, our_pasta_pid
    );

    // Verify we found our firecracker process
    assert!(
        our_fc_pid.is_some(),
        "should have started a firecracker process"
    );

    // Send SIGTERM to fcvm
    println!("Sending SIGTERM to fcvm (PID {})", fcvm_pid);
    send_signal(fcvm_pid, "TERM").context("sending SIGTERM to fcvm")?;

    // Wait for fcvm to exit (max 60 seconds — snapshot abort + cleanup can be slow)
    // When snapshots are enabled, SIGTERM may arrive during snapshot creation.
    // The abortable snapshot code cancels the in-flight snapshot before cleanup.
    let start = std::time::Instant::now();
    let mut exited = false;
    while start.elapsed() < Duration::from_secs(60) {
        match fcvm.try_wait() {
            Ok(Some(status)) => {
                println!("fcvm exited with status: {:?}", status);
                exited = true;
                break;
            }
            Ok(None) => {
                std::thread::sleep(common::POLL_INTERVAL);
            }
            Err(_) => break,
        }
    }

    if !exited {
        println!("fcvm didn't exit after SIGTERM, killing forcefully");
        let _ = fcvm.kill();
        let _ = fcvm.wait();
    }

    // Poll for child process cleanup (max 15 seconds — snapshot abort may extend cleanup)
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(15) {
        let fc_alive = our_fc_pid.is_some_and(process_exists);
        let pasta_alive = our_pasta_pid.is_some_and(process_exists);
        if !fc_alive && !pasta_alive {
            break;
        }
        std::thread::sleep(common::POLL_INTERVAL);
    }

    // Verify our SPECIFIC processes are cleaned up
    if let Some(fc_pid) = our_fc_pid {
        let still_running = process_exists(fc_pid);
        assert!(
            !still_running,
            "our firecracker (PID {}) should be killed after SIGTERM",
            fc_pid
        );
        println!("Firecracker PID {} correctly cleaned up", fc_pid);
    }

    if let Some(pasta_pid) = our_pasta_pid {
        let still_running = process_exists(pasta_pid);
        assert!(
            !still_running,
            "our pasta (PID {}) should be killed after SIGTERM",
            pasta_pid
        );
        println!("pasta PID {} correctly cleaned up", pasta_pid);
    }

    // Verify fcvm process itself is gone
    assert!(
        !process_exists(fcvm_pid),
        "fcvm process (PID {}) should be terminated",
        fcvm_pid
    );

    println!("test_sigterm_cleanup_rootless PASSED");
    Ok(())
}

/// Find the firecracker process spawned by a specific fcvm process
/// by looking at the parent PID chain
fn find_firecracker_for_fcvm(fcvm_pid: u32) -> Option<u32> {
    // Get all firecracker PIDs
    let output = Command::new("pgrep")
        .args(["-f", "firecracker.*--api-sock"])
        .output()
        .ok()?;

    println!(
        "  pgrep status: {:?}, stdout: {:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout).trim()
    );

    if !output.status.success() {
        println!("  pgrep failed, checking all processes...");
        // Fallback: show all firecracker processes
        if let Ok(ps) = Command::new("ps").args(["aux"]).output() {
            for line in String::from_utf8_lossy(&ps.stdout).lines() {
                if line.contains("firecracker") {
                    println!("  ps: {}", line);
                }
            }
        }
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Ok(fc_pid) = line.trim().parse::<u32>() {
            let is_desc = is_descendant_of(fc_pid, fcvm_pid);
            println!(
                "  firecracker PID {} is_descendant_of({}) = {}",
                fc_pid, fcvm_pid, is_desc
            );
            // Check if this firecracker's parent chain includes our fcvm PID
            if is_desc {
                return Some(fc_pid);
            }
        }
    }
    None
}

/// Find the pasta process spawned by a specific fcvm process
fn find_pasta_for_fcvm(fcvm_pid: u32) -> Option<u32> {
    let output = Command::new("pgrep").args(["-f", "pasta"]).output().ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Ok(pasta_pid) = line.trim().parse::<u32>() {
            // Check if this pasta's parent chain includes our fcvm PID
            if is_descendant_of(pasta_pid, fcvm_pid) {
                return Some(pasta_pid);
            }
        }
    }
    None
}

/// Check if a process is a descendant of another process
fn is_descendant_of(pid: u32, ancestor_pid: u32) -> bool {
    let mut current = pid;
    let mut chain = vec![pid];
    // Walk up the parent chain (max 10 levels to prevent infinite loops)
    for _ in 0..10 {
        if current == ancestor_pid {
            println!(
                "    parent chain: {:?} -> found ancestor {}",
                chain, ancestor_pid
            );
            return true;
        }
        if current <= 1 {
            println!("    parent chain: {:?} -> hit init/0", chain);
            return false;
        }
        // Read parent PID from /proc/[pid]/stat
        let stat_path = format!("/proc/{}/stat", current);
        if let Ok(content) = std::fs::read_to_string(&stat_path) {
            // Format: pid (comm) state ppid ...
            // Find the closing paren for comm (can contain spaces/parens)
            if let Some(paren_end) = content.rfind(')') {
                let after_comm = &content[paren_end + 1..];
                let fields: Vec<&str> = after_comm.split_whitespace().collect();
                // fields[0] is state, fields[1] is ppid
                if let Some(ppid_str) = fields.get(1) {
                    if let Ok(ppid) = ppid_str.parse::<u32>() {
                        current = ppid;
                        chain.push(ppid);
                        continue;
                    }
                }
            }
        }
        println!("    parent chain: {:?} -> failed to read /proc", chain);
        return false;
    }
    println!("    parent chain: {:?} -> max depth reached", chain);
    false
}

/// Test that SIGTERM properly cleans up resources in bridged mode
///
/// NOTE: This test tracks SPECIFIC PIDs rather than global process counts to work
/// correctly when running in parallel with other tests.
#[cfg(feature = "privileged-tests")]
#[test]
fn test_sigterm_cleanup_bridged() -> Result<()> {
    println!("\ntest_sigterm_cleanup_bridged");

    // Start fcvm in bridged mode
    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names("cleanup-bridged");
    let mut fcvm = Command::new(&fcvm_path)
        .args([
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "bridged",
            common::TEST_IMAGE,
        ])
        .spawn()
        .context("spawning fcvm")?;

    let fcvm_pid = fcvm.id();
    println!("Started fcvm with PID: {}", fcvm_pid);

    // Wait for VM to become healthy
    // IMPORTANT: Use --pid to query only OUR VM, not all VMs (parallel test safety)
    let start = std::time::Instant::now();
    let mut healthy = false;
    while start.elapsed() < Duration::from_secs(120) {
        std::thread::sleep(common::POLL_INTERVAL);

        let output = Command::new(&fcvm_path)
            .args(["ls", "--json", "--pid", &fcvm_pid.to_string()])
            .output()
            .context("running fcvm ls")?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        if is_vm_healthy(&stdout) {
            healthy = true;
            println!("VM is healthy after {:?}", start.elapsed());
            break;
        }
    }

    if !healthy {
        fcvm::utils::graceful_kill(fcvm_pid, 2000);
        let _ = fcvm.wait();
        anyhow::bail!("VM did not become healthy within 60 seconds");
    }

    // Find the specific firecracker process for THIS VM
    let our_fc_pid = find_firecracker_for_fcvm(fcvm_pid);
    println!("Our firecracker PID: {:?}", our_fc_pid);

    // Verify we found our firecracker process
    assert!(
        our_fc_pid.is_some(),
        "should have started a firecracker process"
    );

    // Send SIGTERM
    println!("Sending SIGTERM to fcvm (PID {})", fcvm_pid);
    send_signal(fcvm_pid, "TERM").context("sending SIGTERM to fcvm")?;

    // Wait for fcvm to exit (max 30 seconds — cleanup can be slow under CI load)
    let start = std::time::Instant::now();
    let mut exited = false;
    while start.elapsed() < Duration::from_secs(30) {
        match fcvm.try_wait() {
            Ok(Some(status)) => {
                println!("fcvm exited with status: {:?}", status);
                exited = true;
                break;
            }
            Ok(None) => std::thread::sleep(common::POLL_INTERVAL),
            Err(_) => break,
        }
    }

    if !exited {
        println!("fcvm didn't exit after SIGTERM, killing forcefully");
        let _ = fcvm.kill();
        let _ = fcvm.wait();
    }

    // Poll for child process cleanup (max 5 seconds)
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        let fc_alive = our_fc_pid.is_some_and(process_exists);
        if !fc_alive {
            break;
        }
        std::thread::sleep(common::POLL_INTERVAL);
    }

    // Verify our SPECIFIC processes are cleaned up
    if let Some(fc_pid) = our_fc_pid {
        let still_running = process_exists(fc_pid);
        assert!(
            !still_running,
            "our firecracker (PID {}) should be killed after SIGTERM",
            fc_pid
        );
        println!("Firecracker PID {} correctly cleaned up", fc_pid);
    }

    // Verify fcvm process itself is gone
    assert!(
        !process_exists(fcvm_pid),
        "fcvm process (PID {}) should be terminated",
        fcvm_pid
    );

    println!("test_sigterm_cleanup_bridged PASSED");
    Ok(())
}

/// Test that SIGTERM properly cleans up ALL routed network resources.
///
/// Routed mode creates: network namespace, veth pair, host IPv6 route,
/// proxy NDP entry, ip6tables MASQUERADE rule, socat port forwarders.
/// After SIGTERM, every one of these must be gone.
///
/// This test extracts the exact namespace name, veth name, and VM IPv6
/// from fcvm's JSON state so it checks specific resources — not counts.
#[cfg(feature = "privileged-tests")]
#[test]
fn test_sigterm_cleanup_routed() -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    if !rt.block_on(common::has_global_ipv6()) {
        println!("SKIP: Host has no global IPv6 address (required for routed networking)");
        return Ok(());
    }
    println!("\ntest_sigterm_cleanup_routed");

    // Start fcvm in routed mode with port forwarding (to test socat cleanup)
    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names("cleanup-routed");
    let host_port = common::find_available_high_port().context("finding available port")?;
    let publish_arg = format!("{}:80", host_port);

    let mut fcvm = Command::new(&fcvm_path)
        .args([
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "routed",
            "--publish",
            &publish_arg,
            common::TEST_IMAGE,
        ])
        .spawn()
        .context("spawning fcvm")?;

    let fcvm_pid = fcvm.id();
    println!("Started fcvm with PID: {}", fcvm_pid);

    // Wait for VM to become healthy and extract state JSON
    let start = std::time::Instant::now();
    let mut state_json = String::new();
    let mut healthy = false;
    while start.elapsed() < Duration::from_secs(120) {
        std::thread::sleep(common::POLL_INTERVAL);

        let output = Command::new(&fcvm_path)
            .args(["ls", "--json", "--pid", &fcvm_pid.to_string()])
            .output()
            .context("running fcvm ls")?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        if is_vm_healthy(&stdout) {
            healthy = true;
            state_json = stdout;
            println!("VM is healthy after {:?}", start.elapsed());
            break;
        }
    }

    if !healthy {
        fcvm::utils::graceful_kill(fcvm_pid, 2000);
        let _ = fcvm.wait();
        anyhow::bail!("VM did not become healthy within 120 seconds");
    }

    // Parse state to get exact resource names
    let state: serde_json::Value =
        serde_json::from_str(&state_json).context("parsing fcvm ls JSON")?;
    let vm = &state[0];
    let network = &vm["config"]["network"];
    let ns_name = network["namespace_name"]
        .as_str()
        .context("namespace_name missing from state")?;
    let host_veth = network["host_veth"]
        .as_str()
        .context("host_veth missing from state")?;
    let vm_ipv6 = network["guest_ipv6"]
        .as_str()
        .context("guest_ipv6 missing from state")?;
    println!(
        "Resources: namespace={}, host_veth={}, vm_ipv6={}",
        ns_name, host_veth, vm_ipv6
    );

    // Record processes BEFORE killing
    let our_fc_pid = find_firecracker_for_fcvm(fcvm_pid);
    println!("Our firecracker PID: {:?}", our_fc_pid);
    assert!(
        our_fc_pid.is_some(),
        "should have started a firecracker process"
    );

    let socat_pids = find_socat_for_port(host_port);
    println!("Socat PIDs for port {}: {:?}", host_port, socat_pids);

    // Verify resources exist BEFORE cleanup
    assert!(
        std::path::Path::new(&format!("/var/run/netns/{}", ns_name)).exists(),
        "namespace {} should exist before SIGTERM",
        ns_name
    );
    let link_output = Command::new("ip")
        .args(["link", "show", host_veth])
        .output()
        .context("checking host veth")?;
    assert!(
        link_output.status.success(),
        "host veth {} should exist before SIGTERM",
        host_veth
    );
    println!("Verified resources exist before SIGTERM");

    // Send SIGTERM
    println!("Sending SIGTERM to fcvm (PID {})", fcvm_pid);
    send_signal(fcvm_pid, "TERM").context("sending SIGTERM to fcvm")?;

    // Wait for fcvm to exit
    let start = std::time::Instant::now();
    let mut exited = false;
    while start.elapsed() < Duration::from_secs(60) {
        match fcvm.try_wait() {
            Ok(Some(status)) => {
                println!("fcvm exited with status: {:?}", status);
                exited = true;
                break;
            }
            Ok(None) => std::thread::sleep(common::POLL_INTERVAL),
            Err(_) => break,
        }
    }

    if !exited {
        println!("fcvm didn't exit after SIGTERM, killing forcefully");
        let _ = fcvm.kill();
        let _ = fcvm.wait();
    }

    // Poll for child process cleanup (max 15 seconds)
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(15) {
        let fc_alive = our_fc_pid.is_some_and(process_exists);
        if !fc_alive {
            break;
        }
        std::thread::sleep(common::POLL_INTERVAL);
    }

    // === Verify ALL resources are cleaned up ===

    // 1. Firecracker process
    if let Some(fc_pid) = our_fc_pid {
        assert!(
            !process_exists(fc_pid),
            "firecracker (PID {}) should be killed after SIGTERM",
            fc_pid
        );
        println!("  [OK] Firecracker process cleaned up");
    }

    // 2. Socat port-forwarder processes
    for socat_pid in &socat_pids {
        assert!(
            !process_exists(*socat_pid),
            "socat (PID {}) should be killed after SIGTERM",
            socat_pid
        );
    }
    if !socat_pids.is_empty() {
        println!("  [OK] Socat processes cleaned up");
    }

    // 3. Network namespace deleted
    assert!(
        !std::path::Path::new(&format!("/var/run/netns/{}", ns_name)).exists(),
        "namespace {} should be deleted after SIGTERM",
        ns_name
    );
    println!("  [OK] Network namespace {} deleted", ns_name);

    // 4. Host veth interface deleted
    let link_output = Command::new("ip")
        .args(["link", "show", host_veth])
        .output()
        .context("checking host veth after cleanup")?;
    assert!(
        !link_output.status.success(),
        "host veth {} should be deleted after SIGTERM",
        host_veth
    );
    println!("  [OK] Host veth {} deleted", host_veth);

    // 5. Host IPv6 route for VM removed
    let route_output = Command::new("ip")
        .args(["-6", "route", "show", &format!("{}/128", vm_ipv6)])
        .output()
        .context("checking IPv6 route after cleanup")?;
    let route_stdout = String::from_utf8_lossy(&route_output.stdout);
    assert!(
        route_stdout.trim().is_empty(),
        "host route for {}/128 should be removed after SIGTERM, got: {}",
        vm_ipv6,
        route_stdout.trim()
    );
    println!("  [OK] Host IPv6 route for {} removed", vm_ipv6);

    // 6. Proxy NDP entry removed
    let neigh_output = Command::new("ip")
        .args(["-6", "neigh", "show", "proxy"])
        .output()
        .context("checking proxy NDP after cleanup")?;
    let neigh_stdout = String::from_utf8_lossy(&neigh_output.stdout);
    assert!(
        !neigh_stdout.contains(vm_ipv6),
        "proxy NDP entry for {} should be removed after SIGTERM, found in: {}",
        vm_ipv6,
        neigh_stdout.trim()
    );
    println!("  [OK] Proxy NDP entry for {} removed", vm_ipv6);

    // 7. ip6tables MASQUERADE rule removed
    let ip6t_output = Command::new("ip6tables")
        .args(["-t", "nat", "-S", "POSTROUTING"])
        .output()
        .context("checking ip6tables after cleanup")?;
    let ip6t_stdout = String::from_utf8_lossy(&ip6t_output.stdout);
    assert!(
        !ip6t_stdout.contains(vm_ipv6),
        "ip6tables MASQUERADE for {} should be removed after SIGTERM, found in: {}",
        vm_ipv6,
        ip6t_stdout.trim()
    );
    println!("  [OK] ip6tables MASQUERADE rule for {} removed", vm_ipv6);

    // 8. fcvm process itself is gone
    assert!(
        !process_exists(fcvm_pid),
        "fcvm process (PID {}) should be terminated",
        fcvm_pid
    );
    println!("  [OK] fcvm process terminated");

    // 9. State file cleaned up
    let ls_output = Command::new(&fcvm_path)
        .args(["ls", "--json", "--pid", &fcvm_pid.to_string()])
        .output()
        .context("running fcvm ls after cleanup")?;
    let ls_stdout = String::from_utf8_lossy(&ls_output.stdout);
    let post_state: serde_json::Value =
        serde_json::from_str(&ls_stdout).unwrap_or(serde_json::Value::Array(vec![]));
    assert!(
        post_state.as_array().map(|a| a.is_empty()).unwrap_or(true),
        "state file should be cleaned up after SIGTERM, got: {}",
        ls_stdout.trim()
    );
    println!("  [OK] State file cleaned up");

    println!("test_sigterm_cleanup_routed PASSED");
    Ok(())
}

/// Find socat processes listening on a specific port
#[cfg(feature = "privileged-tests")]
fn find_socat_for_port(port: u16) -> Vec<u32> {
    let output = Command::new("pgrep")
        .args(["-f", &format!("socat.*TCP-LISTEN:{}", port)])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            stdout
                .lines()
                .filter_map(|l| l.trim().parse::<u32>().ok())
                .collect()
        }
        _ => vec![],
    }
}
