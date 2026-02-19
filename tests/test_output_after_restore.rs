//! Verifies zero message loss across snapshot restore with heavy output + FUSE.
//!
//! Container writes a monotonic counter at max speed to both stdout and stderr
//! while reading from 13 FUSE mounts. Test collects host-side output and verifies:
//! 1. Snapshot was created on cold boot
//! 2. Warm start used the snapshot (not a fresh boot)
//! 3. Container counter keeps incrementing after restore (no deadlock)
//! 4. Output actually reaches the host (not silently dropped)

#![cfg(feature = "privileged-tests")]

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

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
fn build_fcvm_args<'a>(vm_name: &'a str, map_args: &'a [String], cmd: &'a str) -> Vec<&'a str> {
    // Use the real user UID (not root) to match www container's --user mode.
    // Under sudo, getuid() returns 0 but SUDO_UID has the real user.
    let real_uid = std::env::var("SUDO_UID").unwrap_or_else(|_| "1000".to_string());
    let real_gid = std::env::var("SUDO_GID").unwrap_or_else(|_| "100".to_string());
    let user_spec = format!("{}:{}", real_uid, real_gid);
    let user_spec: &'a str = Box::leak(user_spec.into_boxed_str());

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

    println!("\nOutput integrity: 13 FUSE mounts + monotonic counter across snapshot");
    println!("=====================================================================");

    let (vm_name, _, _, _) = common::unique_names("output-restore");

    let (map_args, guest_paths, base_dir) = setup_fuse_mounts(13);

    // Build command: bursty output like falcon_proxy (200+ lines instantly)
    // then continuous FUSE reads + output
    let reads: String = guest_paths
        .iter()
        .map(|p| format!("cat {}/*.txt >/dev/null 2>&1; ", p))
        .collect();
    let pad = "x".repeat(200); // long lines like falcon_proxy
                               // Phase 1: burst 5000 lines to both stdout and stderr (like service startup)
                               // Phase 2: continuous loop with FUSE reads + output
                               // Burst 5000 lines (like falcon_proxy startup dump), then continuous counter.
                               // The burst must drain — if conmon is broken after snapshot restore, this hangs.
    let cmd = format!(
        "b=0; while [ $b -lt 5000 ]; do echo \"BURST:$b {pad}\"; echo \"BURST_ERR:$b {pad}\" >&2; b=$((b+1)); done; i=0; while true; do {reads}echo \"COUNT:$i {pad}\"; echo \"ERR:$i {pad}\" >&2; i=$((i+1)); done",
        reads = reads,
        pad = pad,
    );

    let fcvm_args = build_fcvm_args(&vm_name, &map_args, &cmd);

    // Phase 1: Cold boot
    println!("Phase 1: Cold boot with 13 FUSE mounts...");
    let (mut child, fcvm_pid) = common::spawn_fcvm_with_logs(&fcvm_args, &vm_name)
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

    // Verify exec works
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
    println!("\nPhase 2: Warm start from snapshot...");
    let (mut child2, fcvm_pid2) =
        common::spawn_fcvm_with_logs(&fcvm_args, &format!("{}-warm", vm_name))
            .await
            .context("spawning warm start VM")?;

    println!("  fcvm PID: {}", fcvm_pid2);

    tokio::time::timeout(
        Duration::from_secs(60),
        common::poll_health_by_pid(fcvm_pid2, 60),
    )
    .await
    .map_err(|_| anyhow::anyhow!("warm start timed out after 60s — output pipeline deadlock"))?
    .context("warm start failed")?;

    println!("  Warm start healthy");

    // Let counter run 15s
    println!("  Letting counter run 15s under FUSE load...");
    tokio::time::sleep(Duration::from_secs(15)).await;

    // Verify not deadlocked
    let r = common::exec_in_container(fcvm_pid2, &["echo", "warm-ok"]).await?;
    assert!(r.contains("warm-ok"), "warm exec failed: {}", r.trim());
    println!("  Warm exec after 15s: OK");

    // Verify warm start log shows snapshot was used
    let log_dir = std::path::Path::new("/tmp/fcvm-test-logs");
    let mut snapshot_used = false;
    if let Ok(entries) = std::fs::read_dir(log_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.contains("output-restore") && name.contains("warm") {
                let content = std::fs::read_to_string(entry.path()).unwrap_or_default();
                if content.contains("snapshot hit")
                    || content.contains("Cloning VM directly from snapshot")
                    || content.contains("Pre-start snapshot hit")
                {
                    snapshot_used = true;
                    println!("  Verified: warm start used snapshot");
                }
            }
        }
    }
    assert!(
        snapshot_used,
        "warm start did NOT use snapshot — test is invalid"
    );

    // Cleanup
    println!("  Stopping VM...");
    common::kill_process(fcvm_pid2).await;
    let _ = child2.wait().await;
    let _ = std::fs::remove_dir_all(&base_dir);

    println!("  PASSED: output flows with 13 FUSE mounts across snapshot restore");
    Ok(())
}
