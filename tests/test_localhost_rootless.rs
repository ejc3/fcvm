//! Integration test for rootless localhost container with btrfs storage
//!
//! Validates that:
//! 1. A locally-built container image can run in rootless mode
//! 2. When the kernel supports btrfs, fc-agent auto-configures btrfs storage
//! 3. Podman uses the btrfs driver (not overlay with chown-copy fallback)
//!
//! This catches the containers/storage issue where checkAndRecordIDMappedSupport()
//! hard-disables idmapped overlay detection for rootless, forcing --userns=keep-id
//! through the expensive storage-chown-by-maps path.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};

/// Build a simple test container image using podman with a unique tag.
/// Each test uses its own image NAME and its own CONTENT to avoid parallel races.
/// Unique names stop concurrent `podman build` to the same tag from making
/// `podman save` see a digest change mid-export. Unique content is also required:
/// fcvm caches the delivered image archive by content digest and the guest loads
/// it tagged with whichever name first populated that digest, so identical
/// content under different names would make a later test's `podman run <name>`
/// miss the cached archive and fall back to pulling from registry `localhost`
/// (which fails). Embedding `image_name` in the CMD makes each image's content —
/// and therefore its digest — distinct.
async fn build_test_image(image_name: &str) -> Result<()> {
    use std::io::Write;
    use tempfile::TempDir;

    let temp_dir = TempDir::new().context("creating temp dir")?;
    let containerfile_path = temp_dir.path().join("Containerfile");

    let mut file = std::fs::File::create(&containerfile_path)?;
    writeln!(
        file,
        r#"FROM public.ecr.aws/nginx/nginx:alpine
CMD ["sh", "-c", "echo 'btrfs-rootless-test-ok {image_name}' && sleep 600"]"#
    )?;

    let output = tokio::process::Command::new("podman")
        .args([
            "build",
            "-t",
            image_name,
            "-f",
            &containerfile_path.to_string_lossy(),
            temp_dir.path().to_str().unwrap(),
        ])
        .output()
        .await
        .context("running podman build")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to build test image: {}", stderr);
    }

    println!("  Built {} image", image_name);
    Ok(())
}

/// Test rootless btrfs storage with native btrfs rootfs (profile default).
#[tokio::test]
async fn test_localhost_rootless_btrfs_storage() -> Result<()> {
    run_btrfs_storage_test(None).await
}

/// Test rootless btrfs storage with ext4 rootfs + loopback.
#[tokio::test]
async fn test_localhost_rootless_btrfs_storage_ext4() -> Result<()> {
    run_btrfs_storage_test(Some("ext4")).await
}

/// Shared btrfs storage test — works with any rootfs type.
///
/// Uses --kernel-profile btrfs to get a kernel with CONFIG_BTRFS_FS=y.
/// Verifies btrfs storage driver is active and container runs successfully.
async fn run_btrfs_storage_test(rootfs_type: Option<&str>) -> Result<()> {
    let label = rootfs_type.unwrap_or("default (btrfs from profile)");
    println!(
        "\nRootless Localhost + Btrfs Storage Test (rootfs: {})",
        label
    );
    println!("===================================================");

    // Each test gets a unique image name to avoid parallel podman build races
    let suffix = format!("btrfs-{}", rootfs_type.unwrap_or("native"));
    let image_name = format!("localhost/test-btrfs-rootless-{}", suffix);

    // Step 1: Build test image
    println!("\n1. Building test container image {}...", image_name);
    build_test_image(&image_name).await?;

    // Step 2: Start VM in rootless mode with btrfs kernel
    println!(
        "\n2. Starting VM (rootless, --kernel-profile btrfs, rootfs: {})...",
        label
    );
    let (vm_name, _, _, _) = common::unique_names(&suffix);

    let mut args = vec![
        "podman",
        "run",
        "--name",
        &vm_name,
        "--kernel-profile",
        "btrfs",
    ];
    if let Some(rt) = rootfs_type {
        args.push("--rootfs-type");
        args.push(rt);
    }
    args.push(&image_name);

    let (mut _child, fcvm_pid) = common::spawn_fcvm(&args).await.context("spawning fcvm")?;
    println!("  fcvm PID: {}", fcvm_pid);

    // Step 3: Wait for healthy
    println!("\n3. Waiting for VM to become healthy...");
    common::poll_health_by_pid(fcvm_pid, 180)
        .await
        .context("VM failed to become healthy")?;
    println!("  VM is healthy");

    // Step 4: Verify btrfs storage driver
    println!("\n4. Checking podman storage driver...");
    let driver = common::exec_in_vm(
        fcvm_pid,
        &["podman", "info", "--format", "{{.Store.GraphDriverName}}"],
    )
    .await
    .context("checking storage driver")?;
    let driver = driver.trim();
    println!("  GraphDriverName: {}", driver);
    assert_eq!(
        driver, "btrfs",
        "Expected btrfs storage driver, got: {}",
        driver
    );

    // Step 5: Verify btrfs at storage path (works with both native btrfs root and loopback)
    // Use -T (target) flag so findmnt finds the mount containing the path,
    // not just exact mount points. On native btrfs, storage is under / (no separate mount).
    println!("\n5. Checking btrfs at storage path...");
    let fstype = common::exec_in_vm(
        fcvm_pid,
        &[
            "findmnt",
            "-n",
            "-o",
            "FSTYPE",
            "--target",
            "/var/lib/containers/storage",
        ],
    )
    .await
    .context("checking btrfs at storage path")?;
    let fstype = fstype.trim();
    println!("  /var/lib/containers/storage fstype: {}", fstype);
    assert_eq!(
        fstype, "btrfs",
        "Expected btrfs filesystem at /var/lib/containers/storage, got: {}",
        fstype
    );

    // Step 6: Verify container ran (check podman ps -a for our container)
    println!("\n6. Checking container execution...");
    let ps_output = common::exec_in_vm(fcvm_pid, &["podman", "ps", "-a", "--format", "{{.Names}}"])
        .await
        .context("checking container status")?;
    println!("  Container names: {}", ps_output.trim());
    assert!(
        ps_output.contains("fcvm-container"),
        "Container should have run: {}",
        ps_output
    );

    // Cleanup
    println!("\n7. Cleaning up...");
    common::kill_process(fcvm_pid).await;

    println!(
        "\n  ROOTLESS + BTRFS STORAGE TEST PASSED (rootfs: {})",
        label
    );
    println!("  - localhost image built and loaded in rootless mode");
    println!("  - btrfs storage driver auto-detected and configured");
    println!("  - Container ran successfully on btrfs storage");

    Ok(())
}

/// Test btrfs snapshot restore with --user (keep-id mode).
///
/// This validates that snapshot clones work correctly when:
/// 1. The btrfs image is read-write (each clone needs its own copy)
/// 2. The username is preserved across snapshot/restore for health checks
/// 3. The container starts correctly in the restored VM
///
/// Runs the VM twice with the same args — first creates a snapshot, second restores from it.
/// The key test is that the second run (from snapshot) also succeeds.
///
/// VM-side btrfs loading: no host-side rootless podman needed, so this runs everywhere.
#[tokio::test]
async fn test_localhost_rootless_btrfs_snapshot_restore() -> Result<()> {
    println!("\nBtrfs Snapshot Restore Test");
    println!("==========================");

    let image_name = "localhost/test-btrfs-rootless-snap";

    // Step 1: Build test image (unique name to avoid parallel build races)
    println!("\n1. Building test container image {}...", image_name);
    build_test_image(image_name).await?;

    // Step 2: First run (creates snapshot cache)
    println!("\n2. First run (fresh boot, creates snapshot)...");
    {
        let (vm_name, _, _, _) = common::unique_names("btrfs-snap-1st");

        let (mut _child, fcvm_pid) = common::spawn_fcvm(&[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--kernel-profile",
            "btrfs",
            "--user",
            "1000:1000",
            image_name,
        ])
        .await
        .context("spawning fcvm (first run)")?;
        println!("  fcvm PID: {}", fcvm_pid);

        common::poll_health_by_pid(fcvm_pid, 120)
            .await
            .context("first run: VM failed to become healthy")?;
        println!("  First run: VM is healthy");

        // Verify container runs
        let vm_username = common::exec_in_vm(fcvm_pid, &["getent", "passwd", "1000"])
            .await
            .context("resolving UID 1000 username")?;
        let vm_username = vm_username
            .trim()
            .split(':')
            .next()
            .unwrap_or("ubuntu")
            .to_string();

        // Wait for container to appear
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(60);
        loop {
            if tokio::time::Instant::now() > deadline {
                anyhow::bail!("First run: timed out waiting for container");
            }
            if let Ok(ps) = common::exec_in_vm(
                fcvm_pid,
                &[
                    "runuser",
                    "-u",
                    &vm_username,
                    "--",
                    "podman",
                    "ps",
                    "-a",
                    "--format",
                    "{{.Names}}",
                ],
            )
            .await
            {
                if ps.contains("fcvm-container") {
                    println!("  First run: container is running as {}", vm_username);
                    break;
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }

        common::kill_process(fcvm_pid).await;
        println!("  First run: completed (snapshot should be cached)");
    }

    // Step 3: Second run (should restore from snapshot)
    println!("\n3. Second run (snapshot restore)...");
    {
        let (vm_name, _, _, _) = common::unique_names("btrfs-snap-2nd");

        let (mut _child, fcvm_pid) = common::spawn_fcvm(&[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--kernel-profile",
            "btrfs",
            "--user",
            "1000:1000",
            image_name,
        ])
        .await
        .context("spawning fcvm (second run)")?;
        println!("  fcvm PID: {}", fcvm_pid);

        // Pre-start snapshot restore still needs to run `podman run` from scratch
        // (container doesn't exist yet), so it needs the same timeout as a fresh boot.
        common::poll_health_by_pid(fcvm_pid, 120)
            .await
            .context("second run (from snapshot): VM failed to become healthy")?;
        println!("  Second run: VM is healthy (restored from snapshot!)");

        // Verify btrfs storage driver
        let vm_username = common::exec_in_vm(fcvm_pid, &["getent", "passwd", "1000"])
            .await
            .context("resolving UID 1000 username")?;
        let vm_username = vm_username
            .trim()
            .split(':')
            .next()
            .unwrap_or("ubuntu")
            .to_string();

        // Wait for container
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(60);
        loop {
            if tokio::time::Instant::now() > deadline {
                anyhow::bail!("Second run: timed out waiting for container");
            }
            if let Ok(ps) = common::exec_in_vm(
                fcvm_pid,
                &[
                    "runuser",
                    "-u",
                    &vm_username,
                    "--",
                    "podman",
                    "ps",
                    "-a",
                    "--format",
                    "{{.Names}}",
                ],
            )
            .await
            {
                if ps.contains("fcvm-container") {
                    println!("  Second run: container is running as {}", vm_username);
                    break;
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }

        // Verify btrfs driver in restored VM
        let driver = common::exec_in_vm(
            fcvm_pid,
            &[
                "runuser",
                "-u",
                &vm_username,
                "--",
                "podman",
                "info",
                "--format",
                "{{.Store.GraphDriverName}}",
            ],
        )
        .await
        .context("checking storage driver")?;
        let driver = driver.trim();
        println!("  GraphDriverName: {}", driver);
        assert_eq!(
            driver, "btrfs",
            "Expected btrfs after snapshot restore, got: {}",
            driver
        );

        // Step 4: Exec stress test — fire 10 parallel exec calls with tight timeouts.
        // Catches the double-rebind regression where exec hangs for ~150s after restore.
        // Each call must complete within 10s (the bug caused 150s hangs).
        println!("\n4. Post-restore exec stress test (10 parallel calls, 10s timeout each)...");
        let stress_start = std::time::Instant::now();
        let mut handles: Vec<tokio::task::JoinHandle<anyhow::Result<std::time::Duration>>> =
            Vec::new();
        for i in 0..10 {
            let handle = tokio::spawn(async move {
                let call_start = std::time::Instant::now();
                let result = tokio::time::timeout(
                    tokio::time::Duration::from_secs(10),
                    common::exec_in_vm(fcvm_pid, &["echo", &format!("stress-{}", i)]),
                )
                .await;
                let elapsed = call_start.elapsed();
                match result {
                    Ok(Ok(output)) => {
                        let output = output.trim().to_string();
                        assert_eq!(
                            output,
                            format!("stress-{}", i),
                            "exec call {} returned wrong output",
                            i
                        );
                        println!(
                            "    exec call {} completed in {:.1}ms",
                            i,
                            elapsed.as_secs_f64() * 1000.0
                        );
                        Ok(elapsed)
                    }
                    Ok(Err(e)) => {
                        panic!("exec call {} failed: {}", i, e);
                    }
                    Err(_) => {
                        panic!(
                            "exec call {} timed out after 10s — exec server likely broken by double rebind",
                            i
                        );
                    }
                }
            });
            handles.push(handle);
        }

        let mut max_latency = std::time::Duration::ZERO;
        for handle in handles {
            let elapsed = handle.await.expect("exec stress task panicked")?;
            if elapsed > max_latency {
                max_latency = elapsed;
            }
        }
        let total_elapsed = stress_start.elapsed();
        println!(
            "  All 10 exec calls completed in {:.1}ms (max single: {:.1}ms)",
            total_elapsed.as_secs_f64() * 1000.0,
            max_latency.as_secs_f64() * 1000.0
        );
        // Sanity check: all 10 parallel calls should finish in under 30s total.
        // With the bug, they'd each timeout at 10s = 10s total (parallel), or
        // if sequential would be 150s+ total.
        assert!(
            total_elapsed.as_secs() < 30,
            "exec stress test took {}s — expected <30s for 10 parallel calls",
            total_elapsed.as_secs()
        );

        common::kill_process(fcvm_pid).await;
    }

    println!("\n  BTRFS SNAPSHOT RESTORE TEST PASSED");
    println!("  - First run: fresh boot, btrfs image, snapshot created");
    println!("  - Second run: restored from snapshot, btrfs image isolated per-clone");
    println!("  - Username preserved across snapshot for health checks");
    println!("  - Post-restore exec stress: 10 parallel calls completed within timeout");

    Ok(())
}

/// Test rootless btrfs + keep-id with native btrfs rootfs (profile default).
#[tokio::test]
async fn test_localhost_rootless_btrfs_keepid() -> Result<()> {
    run_btrfs_keepid_test(None).await
}

/// Test rootless btrfs + keep-id with ext4 rootfs + loopback.
#[tokio::test]
async fn test_localhost_rootless_btrfs_keepid_ext4() -> Result<()> {
    run_btrfs_keepid_test(Some("ext4")).await
}

/// Shared btrfs keep-id test — works with any rootfs type.
///
/// Tests --user 1000:1000 (keep-id) with btrfs storage.
/// With btrfs, user namespaces work natively — no chown fallback needed.
async fn run_btrfs_keepid_test(rootfs_type: Option<&str>) -> Result<()> {
    let label = rootfs_type.unwrap_or("default (btrfs from profile)");
    println!(
        "\nRootless Localhost + Btrfs + keep-id Test (rootfs: {})",
        label
    );
    println!("=========================================================");

    // Each test gets a unique image name to avoid parallel podman build races
    let suffix = format!("btrfs-kid-{}", rootfs_type.unwrap_or("native"));
    let image_name = format!("localhost/test-btrfs-rootless-{}", suffix);

    // Step 1: Build test image (unique per test, snapshot key includes --user)
    println!("\n1. Building test container image {}...", image_name);
    build_test_image(&image_name).await?;

    // Step 2: Start VM with --user 1000:1000 (triggers keep-id in fc-agent)
    println!(
        "\n2. Starting VM (rootless, --kernel-profile btrfs, --user 1000:1000, rootfs: {})...",
        label
    );
    let (vm_name, _, _, _) = common::unique_names(&suffix);

    let mut args = vec![
        "podman",
        "run",
        "--name",
        &vm_name,
        "--kernel-profile",
        "btrfs",
    ];
    if let Some(rt) = rootfs_type {
        args.push("--rootfs-type");
        args.push(rt);
    }
    args.extend_from_slice(&["--user", "1000:1000", &image_name]);

    let (mut _child, fcvm_pid) = common::spawn_fcvm(&args).await.context("spawning fcvm")?;
    println!("  fcvm PID: {}", fcvm_pid);

    // Step 3: Wait for VM health and resolve the VM username for UID 1000.
    println!("\n3. Waiting for VM to become healthy...");
    common::poll_health_by_pid(fcvm_pid, 120).await?;
    println!("  VM is healthy");

    // Resolve the actual username for UID 1000 in the guest
    let vm_username = common::exec_in_vm(fcvm_pid, &["getent", "passwd", "1000"])
        .await
        .context("resolving UID 1000 username")?;
    let vm_username = vm_username
        .trim()
        .split(':')
        .next()
        .unwrap_or("ubuntu")
        .to_string();
    println!("  VM user for UID 1000: {}", vm_username);

    // Step 4: Wait for container to start by polling rootless podman.
    println!("\n4. Waiting for container to start...");
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(120);
    loop {
        if tokio::time::Instant::now() > deadline {
            anyhow::bail!("Timed out waiting for container to start");
        }
        if let Ok(ps) = common::exec_in_vm(
            fcvm_pid,
            &[
                "runuser",
                "-u",
                &vm_username,
                "--",
                "podman",
                "ps",
                "-a",
                "--format",
                "{{.Names}}",
            ],
        )
        .await
        {
            if ps.contains("fcvm-container") {
                println!("  Container is running");
                break;
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    // Step 5: Verify btrfs storage driver
    println!("\n5. Checking podman storage driver...");
    let driver = common::exec_in_vm(
        fcvm_pid,
        &[
            "runuser",
            "-u",
            &vm_username,
            "--",
            "podman",
            "info",
            "--format",
            "{{.Store.GraphDriverName}}",
        ],
    )
    .await
    .context("checking storage driver")?;
    let driver = driver.trim();
    println!("  GraphDriverName: {}", driver);
    assert_eq!(
        driver, "btrfs",
        "Expected btrfs storage driver, got: {}",
        driver
    );

    // Step 6: Verify btrfs at storage path (works with both native btrfs root and loopback)
    println!("\n6. Checking btrfs at storage path...");
    let fstype = common::exec_in_vm(
        fcvm_pid,
        &[
            "findmnt",
            "-n",
            "-o",
            "FSTYPE",
            "--target",
            "/var/lib/containers/storage",
        ],
    )
    .await
    .context("checking btrfs at storage path")?;
    let fstype = fstype.trim();
    println!("  /var/lib/containers/storage fstype: {}", fstype);
    assert_eq!(
        fstype, "btrfs",
        "Expected btrfs filesystem at /var/lib/containers/storage, got: {}",
        fstype
    );

    // Step 7: Verify container ran with correct user
    println!("\n7. Checking container execution...");
    let ps_output = common::exec_in_vm(
        fcvm_pid,
        &[
            "runuser",
            "-u",
            &vm_username,
            "--",
            "podman",
            "ps",
            "-a",
            "--format",
            "{{.Names}}",
        ],
    )
    .await
    .context("checking container status")?;
    println!("  Container names: {}", ps_output.trim());
    assert!(
        ps_output.contains("fcvm-container"),
        "Container should have run: {}",
        ps_output
    );

    // Step 8: Verify userns=keep-id was used (container process runs as uid 1000)
    println!("\n8. Checking container runs as expected user...");
    let container_uid = common::exec_in_vm(
        fcvm_pid,
        &[
            "runuser",
            "-u",
            &vm_username,
            "--",
            "podman",
            "exec",
            "fcvm-container",
            "id",
            "-u",
        ],
    )
    .await
    .context("checking container uid")?;
    let container_uid = container_uid.trim();
    println!("  Container UID: {}", container_uid);
    assert_eq!(
        container_uid, "1000",
        "Expected container to run as uid 1000 (keep-id), got: {}",
        container_uid
    );

    // Cleanup
    println!("\n9. Cleaning up...");
    common::kill_process(fcvm_pid).await;

    println!(
        "\n  ROOTLESS + BTRFS + KEEP-ID TEST PASSED (rootfs: {})",
        label
    );
    println!("  - localhost image with --user 1000:1000 (keep-id mode)");
    println!("  - btrfs storage driver avoids storage-chown-by-maps fallback");
    println!("  - Container ran successfully with user namespace mapping");

    Ok(())
}

/// Build `image_name` with a directory only its owner, container uid 42, may enter
/// (#1145). ubuntu:24.04 ships one, `/var/cache/apt/archives/partial` (mode 0700, owned
/// by `_apt`), so every image built on it has it. Rootless podman stores the directory
/// as a subordinate uid of the invoking user, who therefore cannot read it.
async fn build_private_dir_image(image_name: &str, marker: &str) -> Result<()> {
    use std::io::Write;

    let temp_dir = tempfile::TempDir::new().context("creating temp dir")?;
    let containerfile_path = temp_dir.path().join("Containerfile");
    let mut file = std::fs::File::create(&containerfile_path)?;
    writeln!(
        file,
        r#"FROM public.ecr.aws/nginx/nginx:alpine
RUN mkdir /private && echo '{marker}' > /private/file && chown -R 42:42 /private && chmod 0700 /private && chmod 0600 /private/file
CMD ["sleep", "600"]"#
    )?;
    let output = tokio::process::Command::new("podman")
        .args(["build", "-t", image_name, "-f"])
        .arg(&containerfile_path)
        .arg(temp_dir.path())
        .output()
        .await
        .context("running podman build")?;
    anyhow::ensure!(
        output.status.success(),
        "podman build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

async fn remove_image(image_name: &str) {
    let _ = tokio::process::Command::new("podman")
        .args(["rmi", "-f", image_name])
        .output()
        .await;
}

/// A rootless run of a `localhost/` image with a directory private to a container uid
/// other than 0. On main the overlay storage image is built by the invoking user, and
/// the run fails before the VM boots with `mkfs.ext4 -d failed: __populate_fs:
/// Permission denied while changing working directory to "private"`.
///
/// The container must also see the directory as the image built it: owned by uid and
/// gid 42, mode 0700, with its file readable by the container's root.
#[tokio::test]
async fn test_localhost_rootless_image_with_private_directory() -> Result<()> {
    let image_name = format!(
        "localhost/test-rootless-private-dir-{}",
        std::process::id() % 10000
    );
    let marker = format!("private-dir-ok {image_name}");
    build_private_dir_image(&image_name, &marker).await?;

    let (vm_name, _, _, _) = common::unique_names("private-dir");
    let (mut child, fcvm_pid) =
        common::spawn_fcvm(&["podman", "run", "--name", &vm_name, &image_name])
            .await
            .context("spawning fcvm")?;
    let result = async {
        common::poll_health(&mut child, 180)
            .await
            .context("VM failed to become healthy")?;
        let stat = common::exec_in_container(fcvm_pid, &["stat", "-c", "%u:%g:%a", "/private"])
            .await
            .context("stat of /private in the container")?;
        anyhow::ensure!(
            stat.trim() == "42:42:700",
            "/private in the container is {:?}, expected 42:42:700",
            stat.trim()
        );
        let content = common::exec_in_container(fcvm_pid, &["cat", "/private/file"])
            .await
            .context("reading /private/file in the container")?;
        anyhow::ensure!(
            content.trim() == marker,
            "/private/file in the container holds {:?}, expected {marker:?}",
            content.trim()
        );
        Ok(())
    }
    .await;
    common::kill_process(fcvm_pid).await;
    remove_image(&image_name).await;
    result
}

/// Building the storage image of that image removes its temporary podman store. On main
/// the store is removed by the invoking user, who cannot unlink what a subordinate uid
/// owns, and a `tmp-storage-<uuid>` directory is left next to the image for every build.
#[tokio::test]
async fn test_storage_image_build_removes_its_store_of_private_directories() -> Result<()> {
    // build_storage_image runs mkfs.ext4 by name, and an unprivileged PATH often leaves
    // out /usr/sbin, where it lives. This test owns its process (nextest runs one test
    // per process) and its runtime has started no other thread yet.
    let path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{path}:/usr/sbin:/sbin"));
    let image_name = format!(
        "localhost/test-storage-private-dir-{}",
        std::process::id() % 10000
    );
    build_private_dir_image(&image_name, &format!("storage-private-dir {image_name}")).await?;
    let id = tokio::process::Command::new("podman")
        .args(["image", "inspect", "--format", "{{.Id}}", &image_name])
        .output()
        .await
        .context("podman image inspect")?;
    let id = String::from_utf8(id.stdout)?.trim().to_string();

    let dir = tempfile::TempDir::new_in("/mnt/fcvm-btrfs").context("creating a work dir")?;
    let archive = dir.path().join("image.docker.tar");
    let image = dir.path().join("image.storage.img");
    let result = async {
        fcvm::commands::podman::export_image_archive(&id, &image_name, &archive)
            .await
            .context("export_image_archive")?;
        fcvm::commands::podman::build_storage_image(&archive, &image)
            .await
            .context("build_storage_image")?;
        let left: Vec<std::path::PathBuf> = std::fs::read_dir(dir.path())?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path != &archive && path != &image)
            .collect();
        anyhow::ensure!(
            left.is_empty(),
            "build_storage_image left files next to the image: {left:?}"
        );
        Ok(())
    }
    .await;
    // What a failing run leaves is owned by subordinate ids: remove it as they are.
    let _ = tokio::process::Command::new("podman")
        .args(["unshare", "rm", "-rf", "--"])
        .arg(dir.path())
        .output()
        .await;
    remove_image(&image_name).await;
    result
}
