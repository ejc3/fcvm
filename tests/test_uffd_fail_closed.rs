//! The UFFD memory server must FAIL CLOSED.
//!
//! A clone's guest memory is served on demand by a `fcvm snapshot serve` process. If that
//! server's page-fault handler for a clone dies, the clone's userfaultfd closes — and a
//! closed userfaultfd does not stop the guest. The kernel simply resolves every subsequent
//! fault with a **zero page**, so the VM keeps executing against memory that is silently
//! wrong and can commit that wrongness to its disk, to a volume, or to a response it hands
//! back to a caller.
//!
//! The server used to only log that the VM "should be killed". This test proves it now
//! kills it: a handler failure must leave a DEAD clone that reports failure, never a
//! running one serving corrupt results. It also proves the blast radius is bounded — the
//! server and its other work survive the clone it had to kill.

// A full baseline VM + snapshot + clone lifecycle, same class as the other snapshot/clone
// integration tests — not part of the unprivileged unit lane.
#![cfg(feature = "integration-slow")]

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

/// Fail the handler after this many faults.
///
/// Comfortably inside a restore (which faults thousands of pages for any real snapshot),
/// so the failure lands deterministically while the server is actively serving this clone
/// — never before the first fault, never after the clone has finished with the server.
const FAIL_AFTER_FAULTS: &str = "200";

/// A page-fault handler failure must kill the clone and report it, not leave it running.
#[tokio::test]
async fn test_uffd_handler_failure_kills_and_fails_the_clone() -> Result<()> {
    let (baseline_name, clone_name, snapshot_name, _serve_name) =
        common::unique_names("uffd-failclosed");

    // ---- baseline VM + snapshot -------------------------------------------------
    println!("Step 1: starting baseline VM '{baseline_name}'...");
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
    common::poll_health_by_pid(baseline_pid, 120).await?;
    println!("  ✓ baseline healthy (PID {baseline_pid})");

    println!("Step 2: creating snapshot '{snapshot_name}'...");
    common::create_snapshot_by_pid(baseline_pid, &snapshot_name).await?;
    println!("  ✓ snapshot created");

    // ---- memory server, armed to fail one clone's handler -----------------------
    println!("Step 3: starting memory server with a handler failure armed...");
    let (_serve_child, serve_pid, serve_log) = common::spawn_fcvm_with_env_and_log_path(
        &["snapshot", "serve", &snapshot_name],
        &[("FCVM_UFFD_FAIL_AFTER_FAULTS", FAIL_AFTER_FAULTS)],
    )
    .await
    .context("spawning memory server")?;
    let socket = common::poll_serve_ready(serve_pid, 30).await?;
    println!(
        "  ✓ memory server ready (PID {serve_pid}, socket {})",
        socket.display()
    );

    // ---- the clone whose memory the server will stop being able to serve --------
    println!("Step 4: spawning clone '{clone_name}' (its handler will fail)...");
    let serve_pid_str = serve_pid.to_string();
    let (mut clone_child, clone_pid) = common::spawn_fcvm_with_logs(
        &[
            "snapshot",
            "run",
            "--pid",
            &serve_pid_str,
            "--name",
            &clone_name,
        ],
        &clone_name,
    )
    .await
    .context("spawning clone")?;

    // ---- THE ASSERTION: the clone dies, and says why ----------------------------
    //
    // Wait on the process rather than polling for a status: `wait()` returns exactly when
    // the clone exits, so there is no interval to tune and no window in which a
    // still-running clone could be mistaken for a dead one.
    let status = tokio::time::timeout(Duration::from_secs(180), clone_child.wait())
        .await
        .context(
            "clone was STILL RUNNING 180s after its memory server could no longer serve it — \
             this is the silent-corruption failure mode: a live guest whose faults the kernel \
             now answers with zero pages",
        )?
        .context("waiting for clone process")?;

    println!("  clone exited: {status:?}");
    assert!(
        !status.success(),
        "clone exited SUCCESSFULLY ({status:?}) after its page faults could not be served. \
         A clone that was killed because its memory server failed must report failure — a \
         silent exit 0 tells the caller a corrupt run was a good one."
    );

    // The clone process is gone, so nothing is left running on unserved memory.
    assert!(
        !fcvm::utils::is_process_alive(clone_pid),
        "clone process {clone_pid} is still alive after reporting exit"
    );

    // ---- the server said so, loudly, and named what it did ----------------------
    let serve_output = tokio::fs::read_to_string(&serve_log)
        .await
        .with_context(|| format!("reading serve log {}", serve_log.display()))?;
    assert!(
        serve_output.contains("FAIL CLOSED"),
        "memory server log must record the fail-closed kill; log tail:\n{}",
        tail(&serve_output, 40)
    );

    // ---- blast radius: killing one clone must not take the server with it -------
    assert!(
        fcvm::utils::is_process_alive(serve_pid),
        "memory server {serve_pid} died along with the clone it killed — failing closed on \
         one clone must not tear down the server serving the others"
    );

    println!("✅ handler failure produced a DEAD, FAILED clone and a surviving server");

    common::kill_process(serve_pid).await;
    common::kill_process(baseline_pid).await;
    Ok(())
}

/// Last `n` lines of `s`, for assertion messages.
fn tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}
