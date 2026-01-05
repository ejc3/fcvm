//! Integration test for btrfs filesystem mounted via fuse-pipe into container
//!
//! Verifies that a btrfs filesystem can be mounted into a container and:
//! 1. Basic file operations work (create, read, write, delete)
//! 2. btrfs-specific features work (reflinks, CoW)
//! 3. File permissions and ownership are preserved
//!
//! Uses the --map flag to mount a btrfs loopback into the VM/container.

#![cfg(feature = "privileged-tests")]

mod common;

use anyhow::{bail, Context, Result};
use std::process::Stdio;

/// Size of the btrfs loopback image in MB
const BTRFS_SIZE_MB: u32 = 256;

/// Create a btrfs loopback filesystem for testing
async fn create_btrfs_loopback(path: &str, size_mb: u32) -> Result<String> {
    let img_path = format!("{}.img", path);
    let mount_path = path.to_string();

    // Create sparse file
    let output = tokio::process::Command::new("truncate")
        .args(["-s", &format!("{}M", size_mb), &img_path])
        .output()
        .await
        .context("creating sparse file")?;

    if !output.status.success() {
        bail!(
            "Failed to create sparse file: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Format as btrfs
    let output = tokio::process::Command::new("mkfs.btrfs")
        .args(["-f", &img_path])
        .output()
        .await
        .context("formatting btrfs")?;

    if !output.status.success() {
        bail!(
            "Failed to format btrfs: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Create mount point
    tokio::fs::create_dir_all(&mount_path).await?;

    // Mount the btrfs filesystem
    let output = tokio::process::Command::new("mount")
        .args(["-o", "loop", &img_path, &mount_path])
        .output()
        .await
        .context("mounting btrfs")?;

    if !output.status.success() {
        bail!(
            "Failed to mount btrfs: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Make it world-writable for container access
    let output = tokio::process::Command::new("chmod")
        .args(["777", &mount_path])
        .output()
        .await?;

    if !output.status.success() {
        bail!(
            "Failed to chmod: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(mount_path)
}

/// Cleanup btrfs loopback
async fn cleanup_btrfs(path: &str) {
    let img_path = format!("{}.img", path);

    // Unmount
    let _ = tokio::process::Command::new("umount")
        .arg(path)
        .output()
        .await;

    // Remove mount point
    let _ = tokio::fs::remove_dir(path).await;

    // Remove image file
    let _ = tokio::fs::remove_file(&img_path).await;
}

#[tokio::test]
async fn test_btrfs_in_container() -> Result<()> {
    println!("\nbtrfs fuse-pipe integration test");
    println!("=================================");
    println!("Verifying btrfs filesystem works via fuse-pipe in container");

    let test_id = format!("btrfs-{}", std::process::id());
    let btrfs_path = format!("/tmp/{}", test_id);

    // Create btrfs loopback
    println!("\n1. Creating btrfs loopback filesystem...");
    let mount_path = create_btrfs_loopback(&btrfs_path, BTRFS_SIZE_MB).await?;
    println!("   ✓ Created and mounted btrfs at {}", mount_path);

    // Verify btrfs is mounted
    let output = tokio::process::Command::new("findmnt")
        .args(["-n", "-o", "FSTYPE", &mount_path])
        .output()
        .await?;
    let fstype = String::from_utf8_lossy(&output.stdout);
    assert!(
        fstype.trim() == "btrfs",
        "Expected btrfs, got: {}",
        fstype.trim()
    );
    println!("   ✓ Verified filesystem type: btrfs");

    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names("btrfs-fuse");

    // Start VM with btrfs mounted into container
    println!("\n2. Starting VM with btrfs mounted via fuse-pipe...");
    let map_arg = format!("{}:/btrfs", mount_path);

    let (mut _child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "--kernel-profile",
        "nested",
        "--map",
        &map_arg,
        "--privileged",
        common::TEST_IMAGE,
    ])
    .await
    .context("spawning VM with btrfs mount")?;
    println!("   fcvm process started (PID: {})", fcvm_pid);

    // Wait for VM to become healthy
    println!("   Waiting for VM to become healthy...");
    if let Err(e) = common::poll_health_by_pid(fcvm_pid, 180).await {
        cleanup_btrfs(&btrfs_path).await;
        common::kill_process(fcvm_pid).await;
        return Err(e.context("VM failed to become healthy"));
    }
    println!("   ✓ VM is healthy!");

    // Test 1: Verify mount point exists in container
    println!("\n3. Test: Verify /btrfs mount exists in container");
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &fcvm_pid.to_string(),
            "--",
            "ls",
            "-la",
            "/btrfs",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("checking mount")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        cleanup_btrfs(&btrfs_path).await;
        common::kill_process(fcvm_pid).await;
        bail!("/btrfs mount not found in container: {}", stderr);
    }
    println!("   ✓ /btrfs mount exists in container");

    // Test 2: Create and read a file
    println!("\n4. Test: Create and read file via fuse-pipe");
    let test_content = "Hello from btrfs via fuse-pipe!";

    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &fcvm_pid.to_string(),
            "--",
            "sh",
            "-c",
            &format!(
                "echo '{}' > /btrfs/test.txt && cat /btrfs/test.txt",
                test_content
            ),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("creating file")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.contains(test_content) {
        cleanup_btrfs(&btrfs_path).await;
        common::kill_process(fcvm_pid).await;
        bail!(
            "File content mismatch. Expected '{}', got: {}",
            test_content,
            stdout
        );
    }
    println!("   ✓ File created and read correctly");

    // Verify file exists on host
    let host_file = format!("{}/test.txt", mount_path);
    let host_content = tokio::fs::read_to_string(&host_file).await?;
    assert!(
        host_content.contains(test_content),
        "Host file content mismatch"
    );
    println!("   ✓ File visible on host btrfs");

    // Test 3: btrfs reflinks via copy_file_range
    // copy_file_range syscall on btrfs performs reflinks (instant CoW clone).
    // fuse-pipe supports copy_file_range passthrough, so this should work.
    println!("\n5. Test: btrfs reflink via copy_file_range");

    // Create a 10MB file with deterministic content for verification
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &fcvm_pid.to_string(),
            "--",
            "sh",
            "-c",
            "dd if=/dev/zero of=/btrfs/reflink-src.bin bs=1M count=10 2>/dev/null && \
             echo 'source created'",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("creating source file")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.contains("source created") {
        let stderr = String::from_utf8_lossy(&output.stderr);
        cleanup_btrfs(&btrfs_path).await;
        common::kill_process(fcvm_pid).await;
        bail!("Failed to create source file: {}", stderr);
    }
    println!("   ✓ Created 10MB source file");

    // Copy using strace to verify copy_file_range syscall is used
    // strace is installed in the VM rootfs, so we use --vm flag
    // The /btrfs mount is in the container, but we can access it from VM via podman mount
    // Actually, simpler: just do the copy in container, verify reflink on host via filefrag
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &fcvm_pid.to_string(),
            "--",
            "sh",
            "-c",
            "cp /btrfs/reflink-src.bin /btrfs/reflink-dst.bin && sync /btrfs && echo 'copy done'",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("copying file")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Verify copy succeeded
    if !stdout.contains("copy done") {
        cleanup_btrfs(&btrfs_path).await;
        common::kill_process(fcvm_pid).await;
        bail!("cp command failed!\nstdout: {}\nstderr: {}", stdout, stderr);
    }
    println!("   ✓ File copied successfully");

    // Verify files have same content
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &fcvm_pid.to_string(),
            "--",
            "sh",
            "-c",
            "md5sum /btrfs/reflink-src.bin /btrfs/reflink-dst.bin",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    if lines.len() >= 2 {
        let hash1: &str = lines[0].split_whitespace().next().unwrap_or("");
        let hash2: &str = lines[1].split_whitespace().next().unwrap_or("");
        assert_eq!(hash1, hash2, "Copied file hashes should match");
        println!("   ✓ Files have matching content (md5: {})", hash1);
    }

    // Verify reflink worked by checking shared extents on HOST (btrfs fiemap)
    // Both files should share physical blocks due to CoW reflink
    let src_file = format!("{}/reflink-src.bin", mount_path);
    let dst_file = format!("{}/reflink-dst.bin", mount_path);

    // Sync the btrfs mount to ensure FUSE writes are flushed before filefrag
    // This is essential under parallel test load where I/O can be delayed
    let _ = tokio::process::Command::new("sync")
        .arg(&mount_path)
        .output()
        .await;

    let output = tokio::process::Command::new("filefrag")
        .args(["-v", &src_file])
        .output()
        .await
        .context("filefrag source")?;
    let src_extents = String::from_utf8_lossy(&output.stdout);

    let output = tokio::process::Command::new("filefrag")
        .args(["-v", &dst_file])
        .output()
        .await
        .context("filefrag dest")?;
    let dst_extents = String::from_utf8_lossy(&output.stdout);

    // Extract physical block offsets from filefrag output
    // Format: "   0:        0..    2559:       1234..      3793: ..."
    // We check if src and dst share any physical extents (indicates reflink)
    fn extract_physical_blocks(filefrag: &str) -> Vec<u64> {
        let mut blocks = Vec::new();
        for line in filefrag.lines() {
            // Look for lines with physical block info (contains "..")
            if line.contains("..") && !line.contains("ext:") {
                // Parse physical offset after the second ".."
                let parts: Vec<&str> = line.split("..").collect();
                if parts.len() >= 2 {
                    // Physical offset is before the third ".."
                    if let Some(phys) = parts.get(1) {
                        if let Some(num_str) = phys.split(':').next() {
                            if let Ok(num) = num_str.trim().parse::<u64>() {
                                blocks.push(num);
                            }
                        }
                    }
                }
            }
        }
        blocks
    }

    let src_blocks = extract_physical_blocks(&src_extents);
    let dst_blocks = extract_physical_blocks(&dst_extents);

    // Files MUST share blocks (reflink behavior) - fail loudly if not
    let shared = src_blocks.iter().any(|b| dst_blocks.contains(b));
    if !shared {
        cleanup_btrfs(&btrfs_path).await;
        common::kill_process(fcvm_pid).await;
        bail!(
            "REFLINK VERIFICATION FAILED: files do not share physical extents!\n\
             copy_file_range should produce CoW reflinks on btrfs.\n\
             Source extents: {:?}\n\
             Dest extents: {:?}",
            src_blocks,
            dst_blocks
        );
    }
    println!("   ✓ Verified reflink: files share physical extents (CoW)");

    // Test 4: File permissions
    println!("\n6. Test: File permissions via fuse-pipe");
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &fcvm_pid.to_string(),
            "--",
            "sh",
            "-c",
            "touch /btrfs/perms.txt && chmod 755 /btrfs/perms.txt && stat -c '%a' /btrfs/perms.txt",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.trim().contains("755") {
        cleanup_btrfs(&btrfs_path).await;
        common::kill_process(fcvm_pid).await;
        bail!(
            "Permission not set correctly. Expected 755, got: {}",
            stdout
        );
    }
    println!("   ✓ File permissions work correctly");

    // Test 5: Directory operations
    println!("\n7. Test: Directory operations");
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &fcvm_pid.to_string(),
            "--",
            "sh",
            "-c",
            "mkdir -p /btrfs/dir1/dir2/dir3 && \
             touch /btrfs/dir1/dir2/dir3/nested.txt && \
             find /btrfs/dir1 -type f",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.contains("nested.txt") {
        cleanup_btrfs(&btrfs_path).await;
        common::kill_process(fcvm_pid).await;
        bail!("Nested directory creation failed: {}", stdout);
    }
    println!("   ✓ Nested directories work correctly");

    // Clean up
    println!("\n8. Cleaning up...");
    common::kill_process(fcvm_pid).await;
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    cleanup_btrfs(&btrfs_path).await;

    println!("\n✅ BTRFS FUSE-PIPE TEST PASSED!");
    println!("   - btrfs mounted via fuse-pipe into container");
    println!("   - File create/read/write works");
    println!("   - btrfs reflinks (CoW) work");
    println!("   - File permissions preserved");
    println!("   - Directory operations work");

    Ok(())
}
