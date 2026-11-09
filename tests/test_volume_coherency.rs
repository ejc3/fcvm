//! Volume coherency integration tests
//!
//! Verifies that read-only FUSE volume mounts work correctly:
//! 1. Baseline VM: host writes file, container reads it
//! 2. Clone VM: after snapshot/restore, host writes file, clone reads it

#![cfg(feature = "integration-slow")]

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

/// Test read-only volume coherency on a baseline VM.
///
/// Host writes a file, container reads it through FUSE mount.
#[tokio::test]
async fn test_volume_coherency_baseline() -> Result<()> {
    let id = std::process::id();
    let vm_name = format!("vol-baseline-{}", id);
    let host_dir = format!("/tmp/fcvm-vol-test-{}", id);

    tokio::fs::create_dir_all(&host_dir).await?;

    // Start VM with read-only volume mount
    let map_arg = format!("{}:/mnt/test:ro", host_dir);
    let (_child, pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "rootless",
            "--map",
            &map_arg,
            common::TEST_IMAGE,
        ],
        &vm_name,
    )
    .await
    .context("spawning VM")?;

    println!("Waiting for VM to become healthy...");
    common::poll_health_by_pid(pid, 180).await?;
    println!("VM healthy (PID: {})", pid);

    // Write a file on host
    let test_content = format!("coherency-test-{}", id);
    tokio::fs::write(format!("{}/test.txt", host_dir), &test_content).await?;
    println!("Wrote test.txt on host: {}", test_content);

    // Read from container with retry (up to 1s for cache coherency)
    let mut last_err = None;
    for attempt in 0..10 {
        match common::exec_in_container(pid, &["cat", "/mnt/test/test.txt"]).await {
            Ok(output) => {
                let trimmed = output.trim().to_string();
                if trimmed == test_content {
                    println!("Container read matches host write (attempt {})", attempt);
                    last_err = None;
                    break;
                } else {
                    last_err = Some(format!(
                        "content mismatch: expected '{}', got '{}'",
                        test_content, trimmed
                    ));
                }
            }
            Err(e) => {
                last_err = Some(format!("exec failed: {}", e));
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Cleanup
    common::kill_process(pid).await;
    let _ = tokio::fs::remove_dir_all(&host_dir).await;

    if let Some(err) = last_err {
        anyhow::bail!("Volume coherency failed: {}", err);
    }

    println!("VOLUME COHERENCY BASELINE TEST PASSED!");
    Ok(())
}

/// Test read-only volume coherency after snapshot/clone.
///
/// 1. Start VM with volume mount
/// 2. Snapshot it
/// 3. Clone from snapshot (volumes reconstituted from snapshot metadata)
/// 4. Host writes new file, clone reads it
#[tokio::test]
async fn test_volume_coherency_clone() -> Result<()> {
    let id = std::process::id();
    let vm_name = format!("vol-clone-{}", id);
    let clone_name = format!("vol-clone-c-{}", id);
    let snapshot_name = format!("vol-snap-{}", id);
    let host_dir = format!("/tmp/fcvm-vol-clone-{}", id);

    tokio::fs::create_dir_all(&host_dir).await?;

    // Step 1: Start VM with read-only volume mount
    println!("Step 1: Starting baseline VM with volume...");
    let map_arg = format!("{}:/mnt/test:ro", host_dir);
    let (_child, baseline_pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "rootless",
            "--map",
            &map_arg,
            common::TEST_IMAGE,
        ],
        &vm_name,
    )
    .await
    .context("spawning baseline VM")?;

    common::poll_health_by_pid(baseline_pid, 180).await?;
    println!("  Baseline VM healthy (PID: {})", baseline_pid);

    // Step 2: Create snapshot
    println!("Step 2: Creating snapshot...");
    common::create_snapshot_by_pid(baseline_pid, &snapshot_name).await?;
    println!("  Snapshot created");

    // Step 3: Start memory server
    println!("Step 3: Starting memory server...");
    let (_serve_child, serve_pid) = common::start_memory_server(&snapshot_name).await?;
    println!("  Memory server ready (PID: {})", serve_pid);

    // Step 4: Spawn clone (volumes reconstituted from snapshot metadata)
    println!("Step 4: Spawning clone...");
    let (_clone_child, clone_pid) = common::spawn_clone(serve_pid, &clone_name).await?;

    common::poll_health_by_pid(clone_pid, 120).await?;
    println!("  Clone healthy (PID: {})", clone_pid);

    // Step 5: Write a NEW file on host (after clone started)
    let test_content = format!("clone-coherency-{}", id);
    tokio::fs::write(format!("{}/clone_test.txt", host_dir), &test_content).await?;
    println!("Step 5: Wrote clone_test.txt on host: {}", test_content);

    // Step 6: Read from clone container with retry
    // FUSE remount after clone restore takes time: watcher polls every 100ms,
    // then remount involves unmount + vsock reconnect + mount. Allow up to 5s.
    println!("Step 6: Reading from clone...");
    let mut last_err = None;
    for attempt in 0..25 {
        match common::exec_in_container(clone_pid, &["cat", "/mnt/test/clone_test.txt"]).await {
            Ok(output) => {
                let trimmed = output.trim().to_string();
                if trimmed == test_content {
                    println!("  Clone read matches host write (attempt {})", attempt);
                    last_err = None;
                    break;
                } else {
                    last_err = Some(format!(
                        "content mismatch: expected '{}', got '{}'",
                        test_content, trimmed
                    ));
                }
            }
            Err(e) => {
                last_err = Some(format!("exec failed: {}", e));
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Cleanup
    println!("Cleaning up...");
    common::kill_process(clone_pid).await;
    common::kill_process(serve_pid).await;
    common::kill_process(baseline_pid).await;
    let _ = tokio::fs::remove_dir_all(&host_dir).await;

    if let Some(err) = last_err {
        anyhow::bail!("Volume coherency after clone failed: {}", err);
    }

    println!("VOLUME COHERENCY CLONE TEST PASSED!");
    Ok(())
}

/// What a clone reads through the file and the directory a container process held in a
/// `--map` without `--portable-volumes` across the snapshot (#1684).
///
/// The held descriptor and the held cwd name inodes the source's volume server gave the
/// guest. Opening `/proc/<pid>/fd/3` sends an OPEN for the held file's inode, listing
/// `/proc/<pid>/cwd` an OPENDIR for the held directory's, and resolving a name below it a
/// LOOKUP with the held directory as parent. A clone's server that does not know those
/// inodes answers each with EBADF.
#[tokio::test]
async fn test_clone_serves_inodes_held_in_a_plain_map() -> Result<()> {
    let id = std::process::id();
    let vm_name = format!("vol-held-{id}");
    let clone_name = format!("vol-held-c-{id}");
    let snapshot_name = format!("vol-held-snap-{id}");
    let host_dir = format!("/tmp/fcvm-vol-held-{id}");
    let _ = tokio::fs::remove_dir_all(&host_dir).await;
    tokio::fs::create_dir_all(format!("{host_dir}/sub")).await?;
    let held = format!("held-{id}");
    let other = format!("other-{id}");
    tokio::fs::write(format!("{host_dir}/sub/held.txt"), &held).await?;
    tokio::fs::write(format!("{host_dir}/sub/other.txt"), &other).await?;

    let mut started = Vec::new();
    let result = async {
        let map_arg = format!("{host_dir}:/mnt/vol");
        let (_child, pid) = common::spawn_fcvm_with_logs(
            &[
                "podman",
                "run",
                "--name",
                &vm_name,
                "--network",
                "rootless",
                "--map",
                &map_arg,
                common::TEST_IMAGE,
            ],
            &vm_name,
        )
        .await
        .context("spawning the VM")?;
        started.push(pid);
        common::poll_health_by_pid(pid, 180).await?;

        // A process that holds sub/held.txt open as fd 3 and sub as its cwd.
        common::exec_in_container(
            pid,
            &["cd /mnt/vol/sub && (exec 3<held.txt; exec sleep 1000001) \
               </dev/null >/dev/null 2>&1 &"],
        )
        .await?;
        let holder = "$(pgrep -f 'sleep 100000[1]')";
        let check = format!(
            "P={holder}; test -n \"$P\" || {{ echo no holder; exit 1; }}; \
             echo fd=$(cat /proc/$P/fd/3 2>&1); \
             echo ls=$(ls /proc/$P/cwd/ 2>&1 | sort | tr '\\n' ' '); \
             echo below=$(cat /proc/$P/cwd/other.txt 2>&1)"
        );
        let want = format!("fd={held}\nls=held.txt other.txt\nbelow={other}");
        let in_vm = common::exec_in_container(pid, &[&check]).await?;
        anyhow::ensure!(
            in_vm.trim() == want,
            "the running VM does not read its own holds:\n{in_vm}"
        );

        common::create_snapshot_by_pid(pid, &snapshot_name).await?;
        common::kill_process(pid).await;
        started.retain(|p| *p != pid);

        let (_serve, serve_pid) = common::start_memory_server(&snapshot_name).await?;
        started.push(serve_pid);
        let (_clone, clone_pid) = common::spawn_clone(serve_pid, &clone_name).await?;
        started.push(clone_pid);
        common::poll_health_by_pid(clone_pid, 120).await?;

        let in_clone = common::exec_in_container(clone_pid, &[&check]).await?;
        anyhow::ensure!(
            in_clone.trim() == want,
            "the clone does not serve the inodes the guest held across the snapshot.\n\
             want:\n{want}\ngot:\n{}",
            in_clone.trim()
        );
        Ok(())
    }
    .await;

    for pid in started.iter().rev() {
        common::kill_process(*pid).await;
    }
    common::delete_snapshot(&snapshot_name).await.ok();
    let _ = tokio::fs::remove_dir_all(&host_dir).await;
    result
}
