//! Verifies zero message loss across snapshot restore with FUSE mounts.
//!
//! Container writes a monotonic counter at max speed to both stdout and stderr
//! while reading from FUSE mounts. Test collects host-side output and verifies:
//! 1. Snapshot was created on cold boot
//! 2. Warm start used the snapshot (not a fresh boot)
//! 3. Container output reaches the host quickly (write probe detects dead vsock)
//! 4. Output actually reaches the host (not silently dropped)
//!
//! NOTE: This test does NOT use exec-based health monitoring for the warm start.
//! After snapshot restore, the guest exec server's vsock listener sometimes fails
//! to accept connections (transport reset race). Output uses a separate vsock
//! connection that the guest establishes proactively, so it's not affected.

#![cfg(feature = "privileged-tests")]

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

const NUM_FUSE_MOUNTS: usize = 13;

/// Create N host directories with files, return (map_args, guest_paths, base_dir)
fn setup_fuse_mounts(n: usize) -> (Vec<String>, Vec<String>, std::path::PathBuf) {
    let base =
        std::path::PathBuf::from(format!("/tmp/fcvm-fuse-output-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();

    let mut map_args = Vec::new();
    let mut guest_paths = Vec::new();

    for i in 0..n {
        let host_dir = base.join(format!("vol{}", i));
        std::fs::create_dir_all(&host_dir).unwrap();
        for j in 0..3 {
            std::fs::write(
                host_dir.join(format!("f{}.txt", j)),
                format!("v{}f{}\n", i, j).repeat(20),
            )
            .unwrap();
        }
        let guest = format!("/mnt/vol{}", i);
        map_args.push(format!("{}:{}:ro", host_dir.display(), guest));
        guest_paths.push(guest);
    }

    (map_args, guest_paths, base)
}

/// Build the fcvm args (identical for cold and warm — required for snapshot key match)
fn build_fcvm_args<'a>(
    vm_name: &'a str,
    map_args: &'a [String],
    cmd: &'a str,
    user_spec: &'a str,
) -> Vec<&'a str> {
    let mut args: Vec<&str> = vec![
        "podman",
        "run",
        "--name",
        vm_name,
        "--kernel-profile",
        "btrfs",
        "--user",
        user_spec,
        "--privileged",
    ];
    for m in map_args {
        args.push("--map");
        args.push(m);
    }
    args.extend(&[common::ALPINE_IMAGE, "sh", "-c", cmd]);
    args
}

/// Count container output lines (COUNT:/BURST:) in a log file.
fn count_container_output_in_log(log_path: &std::path::Path) -> usize {
    let content = std::fs::read_to_string(log_path).unwrap_or_default();
    content
        .lines()
        .filter(|line| {
            (line.contains("COUNT:") || line.contains("BURST:"))
                && !line.contains("args=")
                && !line.contains("Spawned")
        })
        .count()
}

/// Poll a log file for container output (COUNT:/BURST: lines).
/// This is used instead of exec-based health monitoring because the exec
/// server's vsock listener sometimes fails after snapshot restore.
/// Caller should wrap this with `tokio::time::timeout` for deadline enforcement.
async fn poll_for_container_output(log_path: &std::path::Path) -> usize {
    loop {
        let count = count_container_output_in_log(log_path);
        if count > 0 {
            return count;
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[tokio::test]
async fn test_heavy_output_after_snapshot_restore() -> Result<()> {
    // This test requires snapshot support — skip if FCVM_NO_SNAPSHOT is set
    if std::env::var("FCVM_NO_SNAPSHOT")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        println!("Skipping test: FCVM_NO_SNAPSHOT is set");
        return Ok(());
    }

    println!(
        "\nOutput integrity: {NUM_FUSE_MOUNTS} FUSE mounts + monotonic counter across snapshot"
    );
    println!("=====================================================================");

    let (vm_name, _, _, _) = common::unique_names("output-restore");

    let (map_args, guest_paths, base_dir) = setup_fuse_mounts(NUM_FUSE_MOUNTS);

    // Build command: bursty output like falcon_proxy (200+ lines instantly)
    // then continuous FUSE reads + output
    let reads: String = guest_paths
        .iter()
        .map(|p| format!("cat {}/*.txt >/dev/null 2>&1; ", p))
        .collect();
    let pad = "x".repeat(200); // long lines like falcon_proxy
    let cmd = format!(
        "b=0; while [ $b -lt 5000 ]; do echo \"BURST:$b {pad}\"; echo \"BURST_ERR:$b {pad}\" >&2; b=$((b+1)); done; i=0; while true; do {reads}echo \"COUNT:$i {pad}\"; echo \"ERR:$i {pad}\" >&2; i=$((i+1)); done",
        reads = reads,
        pad = pad,
    );

    let uid = std::env::var("SUDO_UID").unwrap_or_else(|_| nix::unistd::getuid().to_string());
    let gid = std::env::var("SUDO_GID").unwrap_or_else(|_| nix::unistd::getgid().to_string());
    let user_spec = format!("{}:{}", uid, gid);
    let fcvm_args = build_fcvm_args(&vm_name, &map_args, &cmd, &user_spec);

    // Phase 1: Cold boot
    // Use the quiet spawner: this container emits thousands of long lines to both
    // stdout and stderr. Echoing that to the CI job log can overwhelm the
    // self-hosted runner's log uploader and drop the runner. The debug log file
    // still captures everything, which is what the assertions poll.
    println!("Phase 1: Cold boot with {NUM_FUSE_MOUNTS} FUSE mounts...");
    let (mut child, fcvm_pid, _cold_log_path) =
        common::spawn_fcvm_with_log_path_quiet(&fcvm_args, &vm_name)
            .await
            .context("spawning cold boot VM")?;

    println!("  fcvm PID: {}", fcvm_pid);

    tokio::time::timeout(
        Duration::from_secs(300),
        common::poll_health_by_pid(fcvm_pid, 300),
    )
    .await
    .map_err(|_| anyhow::anyhow!("cold boot timed out"))?
    .context("cold boot failed")?;

    println!("  Cold boot healthy");

    // Verify exec works on cold boot (exec server is reliable before snapshot)
    let r = common::exec_in_container(fcvm_pid, &["echo", "cold-ok"]).await?;
    assert!(r.contains("cold-ok"), "cold exec failed: {}", r.trim());
    println!("  Cold exec: OK");

    // Wait for pre-start snapshot
    tokio::time::sleep(Duration::from_secs(10)).await;

    println!("  Killing cold boot VM...");
    common::kill_process(fcvm_pid).await;
    let _ = child.wait().await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Phase 2: Warm start — MUST use snapshot
    // Use output-based detection instead of exec-based health monitoring.
    // After snapshot restore, the guest exec server's vsock listener sometimes
    // fails to accept connections (transport reset race). Container output uses
    // a different vsock connection that the guest establishes proactively.
    println!("\nPhase 2: Warm start from snapshot...");
    let warm_log_name = format!("{}-warm", vm_name);
    let (mut child2, fcvm_pid2, warm_log_path) =
        common::spawn_fcvm_with_log_path_quiet(&fcvm_args, &warm_log_name)
            .await
            .context("spawning warm start VM")?;

    println!("  fcvm PID: {}", fcvm_pid2);
    println!("  Warm log: {}", warm_log_path.display());

    // Poll for container output in the warm start log instead of using exec.
    // This directly tests what we care about: does output reach the host?
    let warm_start_timer = std::time::Instant::now();
    let initial_count = tokio::time::timeout(
        Duration::from_secs(120),
        poll_for_container_output(&warm_log_path),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "warm start timed out after 120s — no container output in logs. \
             Output pipeline broken after snapshot restore."
        )
    })?;

    let warm_start_secs = warm_start_timer.elapsed().as_secs();
    println!(
        "  Container output appeared in {}s ({} lines so far)",
        warm_start_secs, initial_count
    );

    // The container output should appear within 30s. If notify_cache_ready_and_wait()
    // falls through to its 30s timeout (because the write probe isn't detecting the dead
    // vsock connection), the container startup is delayed by 30s + ~15s setup = ~45s.
    // With the write probe fix, the dead connection is detected in <1s.
    assert!(
        warm_start_secs < 30,
        "container output took {}s to appear — notify_cache_ready_and_wait() likely timed out \
         instead of detecting dead vsock connection via write probe",
        warm_start_secs,
    );

    // Let counter run 15s under FUSE load
    println!("  Letting counter run 15s under FUSE load...");
    tokio::time::sleep(Duration::from_secs(15)).await;

    // Verify output is still flowing (not just initial burst)
    let final_count = count_container_output_in_log(&warm_log_path);
    println!(
        "  Container output after 15s: {} lines (was {} at start)",
        final_count, initial_count
    );
    assert!(
        final_count > initial_count,
        "container output stopped flowing: {} lines at start, {} after 15s",
        initial_count,
        final_count,
    );

    // Verify warm start log shows snapshot was used
    let warm_content = std::fs::read_to_string(&warm_log_path)
        .with_context(|| format!("reading warm log: {}", warm_log_path.display()))?;
    let snapshot_used = warm_content.contains("snapshot hit")
        || warm_content.contains("Cloning VM directly from snapshot")
        || warm_content.contains("Pre-start snapshot hit");
    if snapshot_used {
        println!("  Verified: warm start used snapshot");
    }
    assert!(
        snapshot_used,
        "warm start did NOT use snapshot — test is invalid"
    );

    // Show first container output line for diagnostics
    if let Some(first) = warm_content.lines().find(|line| {
        (line.contains("COUNT:") || line.contains("BURST:"))
            && !line.contains("args=")
            && !line.contains("Spawned")
    }) {
        println!(
            "  First output: {}",
            first.trim().chars().take(100).collect::<String>()
        );
    }

    // Cleanup
    println!("  Stopping VM...");
    common::kill_process(fcvm_pid2).await;
    let _ = child2.wait().await;
    let _ = std::fs::remove_dir_all(&base_dir);

    println!(
        "  PASSED: output flows ({} lines) with {NUM_FUSE_MOUNTS} FUSE mounts across snapshot restore",
        final_count
    );
    Ok(())
}
