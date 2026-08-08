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

/// Read `(state, start_time)` from `/proc/<pid>/stat`. `start_time` (field 22) together
/// with the pid uniquely identifies a process even after PID reuse. `comm` (field 2) can
/// contain spaces and `)`, so parse the remainder after the last `") "`.
fn proc_state_start(pid: u32) -> Option<(char, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let after = stat.rsplit_once(") ")?.1;
    let fields: Vec<&str> = after.split_whitespace().collect();
    let state = fields.first()?.chars().next()?;
    let start = fields.get(19)?.parse::<u64>().ok()?; // field 22 = index 19 after "pid (comm) "
    Some((state, start))
}

/// Read `comm` (the executable basename, field 2 of `/proc/<pid>/stat`) — the text between
/// the FIRST `(` and the LAST `)`, which tolerates a `comm` containing `(`/`)`/spaces.
fn proc_comm(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    (open < close).then(|| stat[open + 1..close].to_string())
}

/// A terminal/zombie state means the process is gone for our purposes: `Z` (zombie,
/// SIGKILL'd-but-unreaped) and the rare dead states `X`/`x`. `/proc/<pid>` still exists in
/// these windows, so a bare existence check spuriously reports "running" (the #628 flake).
fn is_dead_state(state: char) -> bool {
    matches!(state, 'Z' | 'X' | 'x')
}

/// True iff the SAME process (matched by pid + captured `start_time`) is still present and
/// not in a dead state. Defeats BOTH the post-kill zombie window and PID reuse — the two
/// causes of the #628 flake. `start_time` is `None` when the process was already gone at capture.
fn same_process_running(pid: u32, start_time: Option<u64>) -> bool {
    let Some(start) = start_time else {
        return false;
    };
    matches!(proc_state_start(pid), Some((state, st)) if !is_dead_state(state) && st == start)
}

/// `pidfd_open(2)` — returns a fd that refers to THIS exact process for its whole lifetime.
/// Unlike a bare PID, a pidfd never aliases a reused PID, so signalling through it can never
/// hit a stranger. Returns `None` if the process is already gone.
fn pidfd_open(pid: u32) -> Option<i32> {
    // SAFETY: pidfd_open has no memory effects; we check the return value.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if fd < 0 {
        None
    } else {
        Some(fd as i32)
    }
}

/// A child process tracked by `(pid, start_time)` for liveness checks plus a `pidfd` for
/// race-free signalling. Liveness survives the post-kill zombie window and PID reuse under
/// parallel load (#628); kills target the exact captured process, never a reused PID.
///
/// `Copy` for ergonomics: the `pidfd` is intentionally never closed (test-lifetime leak of
/// at most three fds — firecracker/pasta/holder — reclaimed when the test process exits).
#[derive(Clone, Copy, Debug)]
struct Tracked {
    pid: u32,
    start: u64,
    pidfd: i32,
}

impl Tracked {
    /// Capture identity for a live pid; `None` if it is already gone/unreadable. Opens the
    /// pidfd FIRST so the handle is pinned to the process observed at this instant.
    fn capture(pid: u32) -> Option<Self> {
        let pidfd = pidfd_open(pid)?;
        let Some((_, start)) = proc_state_start(pid) else {
            // Process died between pidfd_open and the stat read; drop the (now-useless) fd.
            // SAFETY: fd was just returned by pidfd_open and is not used elsewhere.
            unsafe { libc::close(pidfd) };
            return None;
        };
        Some(Tracked { pid, start, pidfd })
    }
    /// Is this exact process still present and not in a dead/zombie state?
    fn running(&self) -> bool {
        same_process_running(self.pid, Some(self.start))
    }
    /// SIGKILL the exact captured process via its pidfd. No-op (ESRCH) if already dead, and
    /// can NEVER signal a reused stranger PID because the pidfd refers to the original process.
    fn kill_if_running(&self) {
        // SAFETY: pidfd is a valid open fd; the siginfo arg is NULL (kernel synthesises it).
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd as libc::c_long,
                libc::SIGKILL as libc::c_long,
                std::ptr::null::<libc::siginfo_t>(),
                0 as libc::c_long,
            );
        }
    }
    /// Close the pidfd. Only used on a discovery rejection path (the process turned out not to
    /// be ours), where keeping the fd would be a genuine leak rather than an intended one.
    fn close(self) {
        // SAFETY: pidfd is a valid open fd owned here and not used after this point.
        unsafe { libc::close(self.pidfd) };
    }
}

/// True iff the tracked process (if any) is still running.
fn opt_running(t: Option<Tracked>) -> bool {
    t.is_some_and(|p| p.running())
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
    let mut cmd = Command::new(&fcvm_path);
    cmd.args([
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        // One-shot test: boot, send a signal, verify cleanup. It never restores
        // a snapshot, so skip the pre-start snapshot create (#618) — under
        // SnapshotEnabled CI its ~1GB memory dump can exceed the health-wait
        // budget when this VM is first-to-create under disk contention.
        "--no-snapshot",
        common::TEST_IMAGE,
    ]);
    common::set_test_pdeathsig_std(&mut cmd);
    let mut fcvm = cmd.spawn().context("spawning fcvm")?;

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
    let fc = our_fc_pid.unwrap();
    let fc_pid = fc.pid;
    assert!(fc.running(), "firecracker should be running before SIGINT");

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
        if !fc.running() {
            break;
        }
        std::thread::sleep(common::POLL_INTERVAL);
    }

    // Check if our specific firecracker is still running
    let still_running = fc.running();
    if still_running {
        // This is a bug - firecracker should have been killed
        println!(
            "BUG: firecracker (PID {}) is still running after fcvm exit!",
            fc_pid
        );
        // Clean up for the test (race-free: pidfd targets the exact captured process)
        fc.kill_if_running();
    }
    assert!(
        !still_running,
        "firecracker (PID {}) should be killed when fcvm receives SIGINT",
        fc_pid
    );

    // Verify fcvm process itself is gone
    // fcvm.wait()/kill above already reaped the exact child; a bare /proc check here would
    // only risk a PID-reuse false failure, so we don't re-assert it.

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
    let mut cmd = Command::new(&fcvm_path);
    cmd.args([
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        // One-shot test: boot, send a signal, verify cleanup. It never restores
        // a snapshot, so skip the pre-start snapshot create (#618) — under
        // SnapshotEnabled CI its ~1GB memory dump can exceed the health-wait
        // budget when this VM is first-to-create under disk contention.
        "--no-snapshot",
        common::TEST_IMAGE,
    ]);
    common::set_test_pdeathsig_std(&mut cmd);
    let mut fcvm = cmd.spawn().context("spawning fcvm")?;

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
    let fc = our_fc_pid.unwrap();
    let fc_pid = fc.pid;

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
        if !fc.running() {
            break;
        }
        std::thread::sleep(common::POLL_INTERVAL);
    }

    // Check if our specific firecracker is still running
    let still_running = fc.running();
    if still_running {
        println!(
            "BUG: firecracker (PID {}) is still running after fcvm exit!",
            fc_pid
        );
        // Clean up for the test (race-free: pidfd targets the exact captured process)
        fc.kill_if_running();
    }
    assert!(
        !still_running,
        "firecracker (PID {}) should be killed when fcvm receives SIGTERM",
        fc_pid
    );

    // Verify fcvm process itself is gone
    // fcvm.wait()/kill above already reaped the exact child; a bare /proc check here would
    // only risk a PID-reuse false failure, so we don't re-assert it.

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
    let mut cmd = Command::new(&fcvm_path);
    cmd.args([
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "rootless",
        // One-shot test (boot, signal, verify cleanup); never restores a
        // snapshot, so skip the pre-start snapshot create (#618).
        "--no-snapshot",
        common::TEST_IMAGE,
    ]);
    common::set_test_pdeathsig_std(&mut cmd);
    let mut fcvm = cmd.spawn().context("spawning fcvm")?;

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
        let fc_alive = opt_running(our_fc_pid);
        let pasta_alive = opt_running(our_pasta_pid);
        if !fc_alive && !pasta_alive {
            break;
        }
        std::thread::sleep(common::POLL_INTERVAL);
    }

    // Verify our SPECIFIC processes are cleaned up
    if let Some(fc) = our_fc_pid {
        let still_running = fc.running();
        assert!(
            !still_running,
            "our firecracker (PID {}) should be killed after SIGTERM",
            fc.pid
        );
        println!("Firecracker PID {} correctly cleaned up", fc.pid);
    }

    if let Some(pasta) = our_pasta_pid {
        let still_running = pasta.running();
        assert!(
            !still_running,
            "our pasta (PID {}) should be killed after SIGTERM",
            pasta.pid
        );
        println!("pasta PID {} correctly cleaned up", pasta.pid);
    }

    // Verify fcvm process itself is gone
    // fcvm.wait()/kill above already reaped the exact child; a bare /proc check here would
    // only risk a PID-reuse false failure, so we don't re-assert it.

    println!("test_sigterm_cleanup_rootless PASSED");
    Ok(())
}

/// SIGKILL on fcvm must reap the ENTIRE rootless process tree via PR_SET_PDEATHSIG:
/// Firecracker, the namespace holder, AND pasta.
///
/// Unlike SIGTERM/SIGINT, SIGKILL cannot be caught — fcvm gets no chance to run cleanup
/// code, so nothing in `cleanup_vm` executes. Each child survives or dies purely on whether
/// its own `pre_exec` armed `PR_SET_PDEATHSIG(SIGKILL)`; teardown is per-hop, and one
/// unprotected hop orphans everything under it.
///
/// pasta is asserted here because it is the hop that most recently gained its own
/// pdeathsig, and because an orphaned pasta is worse than wasted memory: it would keep
/// listening on the loopback IP:port it was forwarding for a VM that no longer exists, and
/// loopback IPs are allocated sequentially and recycled, so a later clone can be handed the
/// same address while the old listener is still bound.
///
/// Scope, honestly: this asserts the END STATE — nothing from the VM tree outlives fcvm —
/// not the mechanism by which each process dies. It does NOT isolate pasta's own pdeathsig,
/// and it cannot. pasta joins the holder by PID and passt exits when that PID exits, so
/// pasta dies here even with its `pre_exec` removed (measured; it also still dies with the
/// holder's netns pinned open, ruling out namespace teardown as the trigger). What this
/// test does catch is any regression that lets the whole chain go slack — a dropped
/// pdeathsig on the holder, or a passt version that stops watching the PID — which is
/// exactly the failure that left ~490 orphaned processes on two runners.
#[test]
fn test_sigkill_reaps_rootless_vm_tree() -> Result<()> {
    println!("\ntest_sigkill_reaps_rootless_vm_tree");

    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names("sigkill-rootless");
    let mut cmd = Command::new(&fcvm_path);
    cmd.args([
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "rootless",
        // One-shot test (boot, signal, verify cleanup); never restores a
        // snapshot, so skip the pre-start snapshot create (#618).
        "--no-snapshot",
        common::TEST_IMAGE,
    ]);
    common::set_test_pdeathsig_std(&mut cmd);
    let mut fcvm = cmd.spawn().context("spawning fcvm")?;

    let fcvm_pid = fcvm.id();
    println!("Started fcvm with PID: {}", fcvm_pid);

    // Wait for VM to become healthy
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
        anyhow::bail!("VM did not become healthy within 120 seconds");
    }

    // Find the specific child processes for THIS VM. A healthy rootless VM ALWAYS has all
    // three, so a missing one is a failure, not a reason to skip an assertion — silently
    // not-asserting is how an orphaned pasta stayed invisible in the first place.
    // Each is tracked by (pid, start_time) + pidfd so the post-kill checks survive both the
    // SIGKILL'd-but-unreaped zombie window and PID reuse under parallel load (#628).
    //
    // Bailing out of here must tear the VM down first. `set_test_pdeathsig_std` only fires
    // when the whole test BINARY exits, which can be many minutes and many tests away — so
    // an early return would leave a live microVM holding a loopback port for the rest of
    // the run, i.e. exactly the leak this test exists to catch.
    let discover = || -> Result<(Tracked, Tracked, Tracked)> {
        let fc = find_firecracker_for_fcvm(fcvm_pid)
            .context("no firecracker process found under this fcvm")?;
        let holder =
            find_holder_for_fcvm(fcvm_pid).context("no namespace holder found under this fcvm")?;
        let pasta =
            find_pasta_for_fcvm(fcvm_pid).context("no pasta process found under this fcvm")?;
        anyhow::ensure!(fc.running(), "firecracker should be running before SIGKILL");
        anyhow::ensure!(holder.running(), "holder should be running before SIGKILL");
        anyhow::ensure!(pasta.running(), "pasta should be running before SIGKILL");
        Ok((fc, holder, pasta))
    };
    let (fc, holder, pasta) = match discover() {
        Ok(tree) => tree,
        Err(e) => {
            fcvm::utils::graceful_kill(fcvm_pid, 2000);
            let _ = fcvm.wait();
            return Err(e);
        }
    };
    println!(
        "Our tree: firecracker={}, holder={}, pasta={}",
        fc.pid, holder.pid, pasta.pid
    );

    // Send SIGKILL to fcvm — no cleanup handler runs
    println!("Sending SIGKILL to fcvm (PID {})", fcvm_pid);
    send_signal(fcvm_pid, "KILL").context("sending SIGKILL to fcvm")?;

    // Wait for fcvm to be reaped
    let _ = fcvm.wait();
    println!("fcvm process reaped");

    // PR_SET_PDEATHSIG delivers SIGKILL to child processes when parent dies.
    // Poll briefly in case of scheduling delay.
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if !fc.running() && !holder.running() && !pasta.running() {
            break;
        }
        std::thread::sleep(common::POLL_INTERVAL);
    }

    // Snapshot all three BEFORE asserting, then clean up whatever survived, so a failure of
    // this test cannot itself leak a microVM or a stray listener onto the runner.
    let survivors: Vec<(&str, Tracked)> =
        [("firecracker", fc), ("holder", holder), ("pasta", pasta)]
            .into_iter()
            .filter(|(_, t)| t.running())
            .collect();
    for (role, t) in &survivors {
        println!(
            "BUG: {} (PID {}) still running after fcvm SIGKILL!",
            role, t.pid
        );
        t.kill_if_running();
    }

    assert!(
        survivors.is_empty(),
        "every child must die via PR_SET_PDEATHSIG when fcvm is SIGKILL'd, but these \
         survived and were orphaned to init: {:?}. A surviving pasta is the dangerous one: \
         it keeps the dead VM's forwarded loopback port bound, and that IP is recycled to a \
         later clone.",
        survivors
            .iter()
            .map(|(role, t)| format!("{} (PID {})", role, t.pid))
            .collect::<Vec<_>>()
    );
    println!("firecracker, holder and pasta all reaped by the kernel");

    // fcvm.wait() above already reaped the exact child, so no procfs assert needed (a bare
    // /proc check would only add a PID-reuse false failure).

    println!("test_sigkill_reaps_rootless_vm_tree PASSED");
    Ok(())
}

/// The `sudo` hop in the CI process tree must not break VM teardown.
///
/// `make test-root` runs cargo-nextest UNPRIVILEGED and elevates only the test binary, via
/// `CARGO_TARGET_*_RUNNER='sudo -E env PATH=... scripts/root-test-runner.sh'`. The resulting
/// tree is `nextest (uid 1000) -> sudo (ruid 1000) -> test binary (uid 0) -> fcvm -> firecracker`.
/// When nextest's `slow-timeout.terminate-after` fires it signals the test's process group —
/// but the kernel refuses uid 1000 -> uid 0 signals, so the ONLY member it can actually kill is
/// the `sudo` front-end, and SIGKILL cannot be caught, so `sudo` never forwards it. Everything
/// below `sudo` is orphaned to init and keeps running; because the test binary never dies, the
/// PR_SET_PDEATHSIG chain beneath it (test binary -> fcvm -> firecracker) never fires either.
/// A self-hosted runner accumulated ~490 live `firecracker` processes this way and wedged.
///
/// So this test reproduces that topology exactly — the real runner script, a real `sudo` hop, a
/// real VM — and SIGKILLs ONLY the `sudo` front-end, the single process nextest can reach.
/// Everything below it must be gone. Drop `setpriv --pdeathsig KILL` from the runner script and
/// this test fails with fcvm reparented to init and its Firecracker still serving a live VM.
#[cfg(feature = "privileged-tests")]
#[test]
fn test_root_test_runner_reaps_vm_when_sudo_is_killed() -> Result<()> {
    println!("\ntest_root_test_runner_reaps_vm_when_sudo_is_killed");

    // The SAME runner the Makefile installs as CARGO_TARGET_*_RUNNER — one source of truth.
    let runner = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/root-test-runner.sh");
    assert!(
        std::path::Path::new(runner).is_file(),
        "privileged test runner missing at {runner}"
    );

    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names("sudo-hop");
    let path_env = std::env::var("PATH").unwrap_or_default();

    // Never Stdio::piped() without a consumer (pipe-buffer deadlock); a file keeps the VM's
    // output for post-mortem.
    std::fs::create_dir_all("/tmp/fcvm-test-logs").ok();
    let log_path = format!("/tmp/fcvm-test-logs/{}.log", vm_name);
    let log = std::fs::File::create(&log_path).context("creating VM log file")?;

    // Reproduce the runner invocation verbatim: sudo -E env PATH=... <runner> <program> <args>.
    let mut sudo = Command::new("sudo")
        .args([
            "-E",
            "env",
            &format!("PATH={}", path_env),
            runner,
            fcvm_path.to_str().context("fcvm path is not UTF-8")?,
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "rootless",
            // One-shot test (boot, signal, verify cleanup); never restores a snapshot,
            // so skip the pre-start snapshot create (#618).
            "--no-snapshot",
            common::TEST_IMAGE,
        ])
        .stdout(log.try_clone().context("cloning log file")?)
        .stderr(log)
        .spawn()
        .context("spawning sudo runner")?;

    let sudo_pid = sudo.id();
    println!("sudo front-end PID: {} (log: {})", sudo_pid, log_path);

    // fcvm replaces the runner's own image (env -> sh -> setpriv -> fcvm all exec in place),
    // so it is a descendant of sudo. Search by ancestry rather than assuming a direct child,
    // which also covers the pty path where sudo interposes a monitor process.
    let start = std::time::Instant::now();
    let fcvm = loop {
        if let Some(t) = find_descendant_by_comm(sudo_pid, "fcvm") {
            break t;
        }
        if start.elapsed() > Duration::from_secs(60) {
            let _ = sudo.kill();
            anyhow::bail!(
                "fcvm never appeared under the sudo runner (see {})",
                log_path
            );
        }
        std::thread::sleep(common::POLL_INTERVAL);
    };
    println!("fcvm PID under sudo: {}", fcvm.pid);

    // Wait for the VM to be fully up, so the kill lands on a real running microVM.
    let start = std::time::Instant::now();
    let mut healthy = false;
    while start.elapsed() < Duration::from_secs(180) {
        std::thread::sleep(common::POLL_INTERVAL);
        let output = Command::new(&fcvm_path)
            .args(["ls", "--json", "--pid", &fcvm.pid.to_string()])
            .output()
            .context("running fcvm ls")?;
        if is_vm_healthy(&String::from_utf8_lossy(&output.stdout)) {
            healthy = true;
            println!("VM is healthy after {:?}", start.elapsed());
            break;
        }
    }
    if !healthy {
        fcvm.kill_if_running();
        let _ = sudo.kill();
        let _ = sudo.wait();
        anyhow::bail!("VM did not become healthy within 180s (see {})", log_path);
    }

    let fc = find_firecracker_for_fcvm(fcvm.pid);
    assert!(
        fc.is_some(),
        "should have started a firecracker process under fcvm (PID {})",
        fcvm.pid
    );
    let fc = fc.unwrap();
    println!("firecracker PID: {}", fc.pid);

    // Exactly what cargo-nextest can reach: the sudo front-end, and nothing else.
    println!("SIGKILL to the sudo front-end (PID {}) only", sudo_pid);
    send_signal(sudo_pid, "KILL").context("sending SIGKILL to sudo")?;
    let _ = sudo.wait();

    // PR_SET_PDEATHSIG chain: sudo dies -> kernel SIGKILLs fcvm -> kernel SIGKILLs firecracker.
    // Each hop is one scheduler wakeup; 30s is orders of magnitude of headroom for a loaded runner.
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        if !fcvm.running() && !fc.running() {
            break;
        }
        std::thread::sleep(common::POLL_INTERVAL);
    }
    let fcvm_alive = fcvm.running();
    let fc_alive = fc.running();

    // Clean up before asserting so a failure of THIS test cannot itself leak a microVM.
    if fcvm_alive {
        fcvm.kill_if_running();
    }
    if fc_alive {
        fc.kill_if_running();
    }

    assert!(
        !fcvm_alive,
        "fcvm (PID {}) survived the death of its sudo parent — the privileged test runner is \
         not setting PR_SET_PDEATHSIG, so a nextest timeout kill orphans the whole VM tree",
        fcvm.pid
    );
    assert!(
        !fc_alive,
        "firecracker (PID {}) survived the death of the sudo parent of its fcvm (PID {})",
        fc.pid, fcvm.pid
    );

    println!("test_root_test_runner_reaps_vm_when_sudo_is_killed PASSED");
    Ok(())
}

/// Find the SHALLOWEST live descendant of `root_pid` whose `comm` starts with `comm_prefix`,
/// tracked by (pid, start_time) + pidfd so later liveness checks survive PID reuse and the
/// post-kill zombie window.
///
/// Breadth-first on purpose: fcvm spawns its own short-lived `fcvm exec` health-check children,
/// which have the same `comm`. A flat /proc scan could return one of those (and then "the
/// process died" would mean nothing); the shallowest match is always the VM's own fcvm.
#[cfg(feature = "privileged-tests")]
fn find_descendant_by_comm(root_pid: u32, comm_prefix: &str) -> Option<Tracked> {
    // One /proc pass -> pid -> ppid, so the walk sees a single consistent snapshot.
    let mut parents: Vec<(u32, u32)> = Vec::new();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if let Some(ppid) = proc_ppid(pid) {
            parents.push((pid, ppid));
        }
    }

    let mut frontier = vec![root_pid];
    for _ in 0..6 {
        let children: Vec<u32> = parents
            .iter()
            .filter(|(_, ppid)| frontier.contains(ppid))
            .map(|(pid, _)| *pid)
            .collect();
        if children.is_empty() {
            return None;
        }
        for pid in &children {
            if proc_comm(*pid).is_some_and(|c| c.starts_with(comm_prefix)) {
                if let Some(t) = capture_descendant(*pid, root_pid, comm_prefix) {
                    return Some(t);
                }
            }
        }
        frontier = children;
    }
    None
}

/// Parent pid from `/proc/<pid>/stat` (field 4). `comm` (field 2) can contain spaces and
/// parens, so parse the remainder after the last `") "`.
#[cfg(feature = "privileged-tests")]
fn proc_ppid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    stat.rsplit_once(") ")?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// Find the firecracker process spawned by a specific fcvm process
/// by looking at the parent PID chain
fn find_firecracker_for_fcvm(fcvm_pid: u32) -> Option<Tracked> {
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
                if let Some(t) = capture_descendant(fc_pid, fcvm_pid, "firecracker") {
                    return Some(t);
                }
            }
        }
    }
    None
}

/// Pin `pid` via a pidfd, then RE-confirm it is STILL the process we want: a descendant of
/// `fcvm_pid`, still alive, and whose `comm` still starts with `comm_prefix`. The ancestry
/// pre-filter and the capture are separate procfs reads, so the pid could be reaped and reused
/// in between; re-checking ancestry rejects a reused non-descendant, and re-checking `comm`
/// rejects the (vanishingly rare) case where the pid is reused by ANOTHER descendant of the
/// same fcvm with a different role.
///
/// `comm` is the executable basename truncated to 15 chars (`TASK_COMM_LEN`), e.g. the
/// firecracker binary `firecracker-default-….bin` shows as `firecracker-def`, so we match by
/// prefix rather than equality. Returns `None`, closing the pidfd, if gone or no longer ours.
fn capture_descendant(pid: u32, fcvm_pid: u32, comm_prefix: &str) -> Option<Tracked> {
    let t = Tracked::capture(pid)?;
    let ours = is_descendant_of(t.pid, fcvm_pid)
        && t.running()
        && proc_comm(t.pid).is_some_and(|c| c.starts_with(comm_prefix));
    if ours {
        Some(t)
    } else {
        t.close();
        None
    }
}

/// Find the pasta process spawned by a specific fcvm process
fn find_pasta_for_fcvm(fcvm_pid: u32) -> Option<Tracked> {
    let output = Command::new("pgrep").args(["-f", "pasta"]).output().ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Ok(pasta_pid) = line.trim().parse::<u32>() {
            // Check if this pasta's parent chain includes our fcvm PID
            if is_descendant_of(pasta_pid, fcvm_pid) {
                if let Some(t) = capture_descendant(pasta_pid, fcvm_pid, "pasta") {
                    return Some(t);
                }
            }
        }
    }
    None
}

/// Holder discovery is coupled to production by a literal string. Pin it.
///
/// `find_holder_for_fcvm` pgreps for `sleep infinity`, which is what
/// `PastaNetwork::build_holder_command` emits. Nothing enforces that agreement, and
/// when it broke (verified by changing the argv to `sleep 2147483647`) discovery
/// returned `None`, the leak assertions in the two teardown tests silently stopped
/// checking anything, and both tests still passed while a real orphan sat on the box.
///
/// Honest scope: this is GREEN today. It is a drift alarm, not a regression test for
/// a present defect — it fires the moment someone changes the holder's argv, which is
/// the one change that would re-disarm those assertions without any other symptom.
#[test]
fn holder_discovery_pattern_tracks_production_holder_argv() {
    let argv = fcvm::network::PastaNetwork::new("t".into(), "tap-t".into(), vec![])
        .build_holder_command()
        .join(" ");
    assert!(
        argv.contains("sleep infinity"),
        "find_holder_for_fcvm() pgreps for `sleep infinity`, but the production holder \
         argv is `{argv}`. Holder discovery would silently return None and every \
         holder-leak assertion in this file would stop checking anything. Update BOTH."
    );
}

fn find_holder_for_fcvm(fcvm_pid: u32) -> Option<Tracked> {
    let output = Command::new("pgrep")
        .args(["-f", "sleep infinity"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Ok(pid) = line.trim().parse::<u32>() {
            if is_descendant_of(pid, fcvm_pid) {
                // The holder is `sleep infinity`, whose comm is "sleep".
                if let Some(t) = capture_descendant(pid, fcvm_pid, "sleep") {
                    return Some(t);
                }
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
    let mut cmd = Command::new(&fcvm_path);
    cmd.args([
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        // One-shot test: boot, send a signal, verify cleanup. It never restores
        // a snapshot, so skip the pre-start snapshot create (#618) — under
        // SnapshotEnabled CI its ~1GB memory dump can exceed the health-wait
        // budget when this VM is first-to-create under disk contention.
        "--no-snapshot",
        common::TEST_IMAGE,
    ]);
    common::set_test_pdeathsig_std(&mut cmd);
    let mut fcvm = cmd.spawn().context("spawning fcvm")?;

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
        let fc_alive = opt_running(our_fc_pid);
        if !fc_alive {
            break;
        }
        std::thread::sleep(common::POLL_INTERVAL);
    }

    // Verify our SPECIFIC processes are cleaned up
    if let Some(fc) = our_fc_pid {
        let still_running = fc.running();
        assert!(
            !still_running,
            "our firecracker (PID {}) should be killed after SIGTERM",
            fc.pid
        );
        println!("Firecracker PID {} correctly cleaned up", fc.pid);
    }

    // Verify fcvm process itself is gone
    // fcvm.wait()/kill above already reaped the exact child; a bare /proc check here would
    // only risk a PID-reuse false failure, so we don't re-assert it.

    println!("test_sigterm_cleanup_bridged PASSED");
    Ok(())
}

/// Test that SIGTERM properly cleans up ALL routed network resources.
///
/// Routed mode creates: network namespace, veth pair, host IPv6 route,
/// proxy NDP entry, ip6tables MASQUERADE rule, TCP proxy tasks.
/// After SIGTERM, every one of these must be gone.
///
/// This test extracts the exact namespace name, veth name, and VM IPv6
/// from fcvm's JSON state so it checks specific resources — not counts.
#[cfg(feature = "privileged-tests")]
#[test]
fn test_sigterm_cleanup_routed() -> Result<()> {
    println!("\ntest_sigterm_cleanup_routed");

    // Start fcvm in routed mode with port forwarding
    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names("cleanup-routed");
    let host_port = common::find_available_high_port().context("finding available port")?;
    let publish_arg = format!("{}:80", host_port);

    let mut cmd = Command::new(&fcvm_path);
    cmd.args([
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "routed",
        // One-shot test (boot, signal, verify cleanup); never restores a
        // snapshot, so skip the pre-start snapshot create (#618).
        "--no-snapshot",
        "--publish",
        &publish_arg,
        common::TEST_IMAGE,
    ]);
    common::set_test_pdeathsig_std(&mut cmd);
    let mut fcvm = cmd.spawn().context("spawning fcvm")?;

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
        let fc_alive = opt_running(our_fc_pid);
        if !fc_alive {
            break;
        }
        std::thread::sleep(common::POLL_INTERVAL);
    }

    // === Verify ALL resources are cleaned up ===

    // 1. Firecracker process
    if let Some(fc) = our_fc_pid {
        assert!(
            !fc.running(),
            "firecracker (PID {}) should be killed after SIGTERM",
            fc.pid
        );
        println!("  [OK] Firecracker process cleaned up");
    }

    // 2. TCP proxy tasks are in-process (killed with fcvm), no external PIDs to check.

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
    // fcvm.wait()/kill above already reaped the exact child; a bare /proc check here would
    // only risk a PID-reuse false failure, so we don't re-assert it.
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

/// Regression test for #628: a SIGKILL'd-but-unreaped child is a zombie whose `/proc/<pid>`
/// still exists, so the OLD liveness check (bare `/proc/<pid>` existence) reported it as "still
/// running" right after the kill — exactly the spurious failure the cleanup tests hit.
///
/// This reproduces the bug deterministically with no VM and no root: spawn a child, SIGKILL it
/// WITHOUT reaping, and assert the zombie window is handled correctly.
#[test]
fn test_zombie_is_not_running_regression_628() {
    // Spawn a child we fully own (no root needed) and pin its identity while alive.
    let mut child = Command::new("sleep")
        .arg("1000")
        .spawn()
        .expect("spawn sleep child");
    let pid = child.id();

    let tracked = Tracked::capture(pid).expect("capture live child");
    assert!(
        tracked.running(),
        "child should be running right after spawn"
    );

    // Kill it but DELIBERATELY do not reap (no wait()), so it lingers as a zombie — the exact
    // state that produced the #628 flake.
    send_signal(pid, "KILL").expect("SIGKILL child");

    // Wait for it to actually enter the zombie state (Z). Bounded so a stuck test fails loudly.
    let start = std::time::Instant::now();
    let mut became_zombie = false;
    while start.elapsed() < Duration::from_secs(5) {
        if matches!(proc_state_start(pid), Some(('Z', _))) {
            became_zombie = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        became_zombie,
        "child (PID {}) should be a zombie after SIGKILL without reaping",
        pid
    );

    // The trap that broke the old check: the bare procfs entry STILL exists for a zombie.
    assert!(
        std::path::Path::new(&format!("/proc/{}", pid)).exists(),
        "/proc/<pid> still exists for a zombie — this is exactly why a bare existence check \
         spuriously reported the killed process as 'running' (#628)"
    );

    // THE FIX: identity-aware liveness treats the zombie as gone.
    assert!(
        !tracked.running(),
        "Tracked::running() must report a SIGKILL'd zombie as NOT running (#628 regression)"
    );

    // Reap so we leave no zombie behind.
    let _ = child.wait();

    // After reaping, the pid is gone entirely.
    assert!(
        !tracked.running(),
        "Tracked::running() must stay false after the zombie is reaped"
    );
}

/// Regression test for #628 (PID-reuse half): identity is `(pid, start_time)`, so a live pid
/// whose `start_time` differs from the captured one (i.e. the PID was reused by a different
/// process) is correctly reported as NOT our process — a bare PID check would be fooled.
#[test]
fn test_start_time_identity_defeats_pid_reuse_628() {
    // Our own process is guaranteed alive; field 22 start_time is stable for its lifetime.
    let me = std::process::id();
    let (_, real_start) = proc_state_start(me).expect("read own /proc stat");

    // Same pid, correct start_time -> recognised as alive.
    assert!(
        same_process_running(me, Some(real_start)),
        "matching (pid, start_time) should report running"
    );

    // Same (alive) pid, WRONG start_time -> recognised as a different (reused) process.
    let wrong_start = real_start.wrapping_add(1);
    assert!(
        !same_process_running(me, Some(wrong_start)),
        "a mismatched start_time must defeat PID reuse, even though the PID is alive (#628)"
    );

    // No captured start_time (process was already gone at capture) -> never running.
    assert!(
        !same_process_running(me, None),
        "absent start_time must report not running"
    );

    // Dead/zombie states are all treated as gone.
    for s in ['Z', 'X', 'x'] {
        assert!(is_dead_state(s), "{} should count as a dead state", s);
    }
    for s in ['R', 'S', 'D', 'T', 't', 'I'] {
        assert!(!is_dead_state(s), "{} should count as a live state", s);
    }
}

/// The bench fast-teardown path must not leak a microVM. Clone flavour.
///
/// `bench/chromium/reqbench.py` (arm `cdp-fast`) delivers the rendered image the instant
/// it is in hand and then SIGKILLs the clone's `fcvm`. It never signals Firecracker, the
/// namespace holder or pasta itself: `PR_SET_PDEATHSIG(SIGKILL)` is armed on all three
/// (`src/utils.rs::install_namespace_pre_exec`, `src/commands/common.rs::spawn_namespace_holder`,
/// `src/network/pasta.rs`), so the kernel's `exit_notify`/`forget_original_parent` pass
/// delivers SIGKILL to every one of them in a single go — concurrently by construction, with
/// no ordering for the harness to get wrong and no cleanup code of ours that has to survive
/// a SIGKILL in order to run.
///
/// `test_sigkill_reaps_rootless_vm_tree` already proves that chain for a `podman run` VM.
/// This test proves it for the shape the bench actually uses: a CLONE restored from a
/// `snapshot serve` memory server, which reaches Firecracker through the restore path in
/// `src/commands/snapshot.rs` rather than the fresh-boot path. Same guarantee, different
/// spawn site — and a regression in either one is a repeat of #730, where a broken pdeathsig
/// chain orphaned ~490 microVMs and wedged two CI runners at load 523.
///
/// ALL THREE CHILDREN MUST BE FOUND, and a missing one is a hard failure rather than a
/// skipped assertion. Until 2026-08-08 the holder was captured as a bare `Option` and the
/// leak assertion read `assert!(!holder.as_ref().is_some_and(|h| h.running()))` — which is
/// `assert!(!false)` when discovery returns `None`, i.e. an assertion that CANNOT FAIL when
/// the thing it checks is absent. Discovery is coupled to production by string
/// (`find_holder_for_fcvm` pgreps for the literal `sleep infinity` that
/// `PastaNetwork::build_holder_command` emits), so any drift in that argv silently converted
/// this test into coverage-shaped nothing. Verified by fault injection: with the holder argv
/// changed to `sleep 2147483647` the OLD test still passed while a real `sleep` orphan sat on
/// the box; with this `.context(...)?` it fails at discovery. Same reasoning applies to pasta,
/// which the old version did not look for at all.
///
/// Scope, honestly (same caveat as `test_sigkill_reaps_rootless_vm_tree`): this asserts the
/// END STATE — nothing from the VM tree outlives fcvm — not the mechanism by which each
/// process dies. pasta joins the holder by PID and passt exits when that PID exits, so pasta
/// dies here even with its own `pre_exec` removed.
///
/// It also pins the OTHER half of the fast-path contract: because SIGKILL cannot be caught,
/// `cleanup_vm` never runs, so the state file AND the clone's data directory survive the
/// kill. The harness is therefore REQUIRED to reap both synchronously (it does; there is no
/// janitor). Asserting that they are still there after the kill is what stops someone
/// "simplifying" the harness by deleting that step and silently leaking state files and
/// multi-GB reflinked rootfs images instead of VMs.
#[test]
fn test_bench_fast_teardown_leaks_nothing_clone() -> Result<()> {
    println!("\ntest_bench_fast_teardown_leaks_nothing_clone");

    let rt = tokio::runtime::Runtime::new().context("creating tokio runtime")?;
    rt.block_on(async {
        let fcvm_path = common::find_fcvm_binary()?;
        let (base_name, clone_name, snapshot_name, serve_name) =
            common::unique_names("fastteardown");

        // Baseline VM -> snapshot -> memory server: the exact topology the bench uses.
        let (mut base_child, base_pid) = common::spawn_fcvm_with_logs(
            &["podman", "run", "--name", &base_name, common::TEST_IMAGE],
            &base_name,
        )
        .await
        .context("spawning baseline VM")?;
        common::poll_health_by_pid(base_pid, 120).await?;

        let out = tokio::process::Command::new(&fcvm_path)
            .args([
                "snapshot",
                "create",
                "--pid",
                &base_pid.to_string(),
                "--tag",
                &snapshot_name,
            ])
            .output()
            .await
            .context("running snapshot create")?;
        anyhow::ensure!(
            out.status.success(),
            "snapshot create failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let (mut serve_child, serve_pid) =
            common::spawn_fcvm_with_logs(&["snapshot", "serve", &snapshot_name], &serve_name)
                .await
                .context("spawning memory server")?;
        common::poll_serve_ready(&snapshot_name, serve_pid, 60).await?;

        // The clone: no --exec, exactly as the `cdp-fast` arm restores it.
        let (mut clone_child, clone_pid) = common::spawn_fcvm_with_logs(
            &[
                "snapshot",
                "run",
                "--pid",
                &serve_pid.to_string(),
                "--name",
                &clone_name,
            ],
            &clone_name,
        )
        .await
        .context("spawning clone")?;
        common::poll_health_by_pid(clone_pid, 120).await?;
        println!("clone healthy (fcvm PID {})", clone_pid);

        // Capture the exact children by (pid, start_time) + pidfd, so neither the post-kill
        // zombie window nor PID reuse can fake a pass (#628).
        //
        // A healthy rootless clone ALWAYS has all three, so a missing one is a failure, not a
        // reason to skip an assertion — an assertion that cannot fail when its subject is
        // absent reads as coverage while proving nothing. Bailing out must tear the fixture
        // down first, or a discovery failure leaks the very microVM this test polices.
        let discover = || -> Result<(Tracked, Tracked, Tracked)> {
            let fc =
                find_firecracker_for_fcvm(clone_pid).context("clone has no firecracker child")?;
            let holder = find_holder_for_fcvm(clone_pid).context(
                "clone has no namespace holder child — the rootless restore path spawns \
                 `unshare --user --net -- sleep infinity`, and find_holder_for_fcvm pgreps for \
                 that exact argv. If discovery drifts, the holder-leak assertion below checks \
                 nothing at all",
            )?;
            let pasta = find_pasta_for_fcvm(clone_pid)
                .context("clone has no pasta child — rootless networking always spawns one")?;
            anyhow::ensure!(
                fc.running(),
                "firecracker should be running before teardown"
            );
            anyhow::ensure!(holder.running(), "holder should be running before teardown");
            anyhow::ensure!(pasta.running(), "pasta should be running before teardown");
            Ok((fc, holder, pasta))
        };
        let (fc, holder, pasta) = match discover() {
            Ok(tree) => tree,
            Err(e) => {
                common::kill_process(clone_pid).await;
                let _ = clone_child.wait().await;
                let _ = serve_child.kill().await;
                let _ = base_child.kill().await;
                common::kill_process(base_pid).await;
                common::kill_process(serve_pid).await;
                return Err(e);
            }
        };
        println!(
            "clone tree: firecracker={}, holder={}, pasta={}",
            fc.pid, holder.pid, pasta.pid
        );

        // State files are keyed by vm_id (a UUID), NOT by the VM's name — find this
        // clone's file by its recorded fcvm pid, the same way reqbench.py does. The data
        // directory is `<data_root>/vm-disks/<vm_id>`; both must survive the SIGKILL.
        let (state_file, data_dir) = {
            let dir = fcvm::paths::state_dir();
            let mut found = None;
            for entry in std::fs::read_dir(&dir).context("reading state dir")? {
                let path = entry?.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                if v.get("pid").and_then(|p| p.as_u64()) == Some(clone_pid as u64) {
                    // NEVER default this. `vm_runtime_dir("")` is `<data_dir>/vm-disks`
                    // ITSELF, and this test later calls `remove_dir_all` on that path — so
                    // a state file missing `vm_id` would delete every VM's disks on the
                    // machine, including those of tests running concurrently in CI. A
                    // missing id means the state file is not what we think it is; fail
                    // loudly instead of computing a path that is catastrophic when wrong.
                    let vm_id = v
                        .get("vm_id")
                        .and_then(|s| s.as_str())
                        .filter(|s| !s.is_empty())
                        .with_context(|| {
                            format!(
                                "state file {} matched the clone PID but carries no vm_id; \
                                 refusing to derive a runtime directory from an empty id",
                                path.display()
                            )
                        })?
                        .to_string();
                    let dir = fcvm::paths::vm_runtime_dir(&vm_id);
                    anyhow::ensure!(
                        dir.starts_with(fcvm::paths::data_dir().join("vm-disks"))
                            && dir != fcvm::paths::data_dir().join("vm-disks"),
                        "refusing to treat {} as a per-VM runtime directory",
                        dir.display()
                    );
                    found = Some((path, dir));
                    break;
                }
            }
            found.context("no state file recorded for the clone")?
        };

        // THE FAST TEARDOWN: one signal to fcvm. Nothing else is signalled by us.
        send_signal(clone_pid, "KILL").context("SIGKILL to clone fcvm")?;
        let _ = clone_child.wait().await;

        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            if !fc.running() && !holder.running() && !pasta.running() {
                break;
            }
            tokio::time::sleep(common::POLL_INTERVAL).await;
        }

        // Snapshot all three BEFORE asserting, then clean up whatever survived, so a failure
        // of this test cannot itself leak a microVM or a stray forwarding listener.
        let survivors: Vec<(&str, Tracked)> =
            [("firecracker", fc), ("holder", holder), ("pasta", pasta)]
                .into_iter()
                .filter(|(_, t)| t.running())
                .collect();
        for (role, t) in &survivors {
            println!("LEAK: {} (PID {}) survived the fast teardown", role, t.pid);
            t.kill_if_running();
        }

        let state_survived = state_file.exists();
        let data_survived = data_dir.is_dir();

        // Clean up the rest of the fixture before asserting, so a failure does not also leak.
        let _ = serve_child.kill().await;
        let _ = base_child.kill().await;
        common::kill_process(base_pid).await;
        common::kill_process(serve_pid).await;
        std::fs::remove_file(&state_file).ok();
        std::fs::remove_dir_all(&data_dir).ok();

        assert!(
            survivors.is_empty(),
            "LEAK: the fast teardown's single SIGKILL to fcvm left these orphaned to init: \
             {:?}. The PR_SET_PDEATHSIG chain through the snapshot-restore spawn path is \
             broken (#730). A surviving pasta is the dangerous one: it keeps the dead clone's \
             forwarded loopback port bound, and that IP is recycled to a later clone.",
            survivors
                .iter()
                .map(|(role, t)| format!("{} (PID {})", role, t.pid))
                .collect::<Vec<_>>()
        );

        // The other half of the contract: SIGKILL cannot be caught, so cleanup_vm did NOT run
        // and BOTH on-disk artifacts are still there. The harness must reap both synchronously.
        assert!(
            state_survived,
            "expected the clone's state file to SURVIVE the SIGKILL (at {}). If it is gone, \
             fcvm gained a cleanup path that runs under SIGKILL and reqbench.py's synchronous \
             on-disk reap should be revisited.",
            state_file.display()
        );
        assert!(
            data_survived,
            "expected the clone's data dir to SURVIVE the SIGKILL (at {}). It holds a reflinked \
             copy of the golden rootfs, so a harness that does not reap it leaks disk without \
             leaking a process — invisible to every stray-VM guard.",
            data_dir.display()
        );

        println!("test_bench_fast_teardown_leaks_nothing_clone PASSED");
        Ok::<(), anyhow::Error>(())
    })
}

/// `vm_runtime_dir("")` is the shared `vm-disks` directory, not a per-VM one.
///
/// A test that derives a runtime directory from a state file and then `remove_dir_all`s it
/// must never accept an empty id: doing so deletes EVERY VM's disks on the machine,
/// including those of tests running concurrently in CI. This pins the arithmetic so the
/// guard above cannot be simplified away by someone who has not thought about it.
#[test]
fn an_empty_vm_id_resolves_to_the_shared_disk_root_and_must_be_rejected() {
    let shared = fcvm::paths::data_dir().join("vm-disks");

    assert_eq!(
        fcvm::paths::vm_runtime_dir(""),
        shared,
        "an empty vm_id collapses to the SHARED vm-disks root. Any caller that deletes its \
         computed runtime directory would wipe every VM on the host — which is why the \
         lookup rejects an empty id rather than defaulting it."
    );
    assert_ne!(
        fcvm::paths::vm_runtime_dir("vm-abc123"),
        shared,
        "a real vm_id must resolve BELOW the shared root, never to it"
    );
    assert!(
        fcvm::paths::vm_runtime_dir("vm-abc123").starts_with(&shared),
        "a per-VM directory must still live under the shared root"
    );
}
