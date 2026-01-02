//! Integration tests for nested virtualization - nested VMs using ARM64 FEAT_NV2.
//!
//! # Nested Virtualization Status (2025-12-30)
//!
//! ## L1→L2 Working!
//! - Host runs L1 with nested kernel (6.18) and `--privileged --map /mnt/fcvm-btrfs`
//! - L1 runs fcvm inside container to start L2
//! - L2 executes commands successfully
//!
//! ## Key Components
//! - **Host kernel**: 6.18.2-nested with `kvm-arm.mode=nested`
//! - **Nested kernel**: 6.18 with `CONFIG_KVM=y`, FUSE_REMAP_FILE_RANGE support
//! - **Firecracker**: Fork with NV2 support (`--enable-nv2` flag)
//! - **Shared storage**: `/mnt/fcvm-btrfs` mounted via FUSE-over-vsock
//!
//! ## How L2 Works
//! 1. Host writes L1 script to shared storage (`/mnt/fcvm-btrfs/l1-nested.sh`)
//! 2. Host runs: `fcvm podman run --kernel-profile nested --map /mnt/fcvm-btrfs --cmd /mnt/fcvm-btrfs/l1-nested.sh`
//! 3. L1's script: imports image from shared cache, runs `fcvm podman run --cmd "echo MARKER"`
//! 4. L2 echoes marker, exits
//!
//! ## For Deeper Nesting (L3+)
//! Build scripts from deepest level upward:
//! - L3 script: `echo MARKER`
//! - L2 script: import + `fcvm ... --cmd /mnt/fcvm-btrfs/l3.sh`
//! - L1 script: import + `fcvm ... --cmd /mnt/fcvm-btrfs/l2.sh`
//!
//! ## Hardware
//! - c7g.metal (Graviton3 / Neoverse-V1) with FEAT_NV2
//! - MIDR: 0x411fd401 (ARM Neoverse-V1)

#![cfg(feature = "privileged-tests")]

mod common;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Stdio;

#[tokio::test]
async fn test_kvm_available_in_vm() -> Result<()> {
    println!("\nNested KVM test");
    println!("==================");
    println!("Verifying /dev/kvm works with nested kernel profile");

    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names("nested-kvm");

    // Start the VM with nested kernel profile
    // Use --privileged so the container can access /dev/kvm
    println!("\nStarting VM with nested kernel profile (privileged mode)...");
    let (mut child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "--kernel-profile",
        "nested",
        "--privileged",
        common::TEST_IMAGE,
    ])
    .await
    .context("spawning fcvm podman run with nested profile")?;
    println!("  fcvm process started (PID: {})", fcvm_pid);

    // Wait for VM to become healthy
    println!("  Waiting for VM to become healthy...");
    if let Err(e) = common::poll_health(&mut child, 180).await {
        let _ = child.kill().await;
        return Err(e.context("VM failed to become healthy"));
    }
    println!("  ✓ VM is healthy!");

    // Test 1: Check if /dev/kvm exists - MUST EXIST
    println!("\nTest 1: Check /dev/kvm exists");
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &fcvm_pid.to_string(),
            "--vm",
            "--",
            "ls",
            "-la",
            "/dev/kvm",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("running fcvm exec")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !stderr.is_empty() {
        println!("  stderr: {}", stderr.trim());
    }

    if !output.status.success() {
        common::kill_process(fcvm_pid).await;
        bail!(
            "/dev/kvm NOT FOUND!\n\
            \n\
            The nested kernel was used but /dev/kvm doesn't exist.\n\
            stderr: {}\n\
            \n\
            Check:\n\
            1. Kernel built with CONFIG_KVM=y and CONFIG_VIRTUALIZATION=y\n\
            2. fc-agent creates /dev/kvm at boot (check fc-agent logs)\n\
            3. Host has /dev/kvm (nested virtualization requires host KVM)",
            stderr
        );
    }

    println!("  ✓ /dev/kvm exists: {}", stdout.trim());

    // Verify it's a character device
    assert!(
        stdout.contains("crw") || stdout.contains("c-"),
        "/dev/kvm must be a character device, got: {}",
        stdout
    );

    // Test 2: Check KVM is accessible from VM
    println!("\nTest 2: Verify /dev/kvm is accessible from VM");
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &fcvm_pid.to_string(),
            "--vm",
            "--",
            "sh",
            "-c",
            "test -r /dev/kvm && test -w /dev/kvm && echo 'OK' || echo 'FAIL'",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("running fcvm exec")?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    if !stdout.contains("OK") {
        common::kill_process(fcvm_pid).await;
        bail!(
            "/dev/kvm exists but is NOT accessible from VM!\n\
            Check permissions: got '{}'",
            stdout.trim()
        );
    }
    println!("  ✓ /dev/kvm is readable and writable from VM");

    // Test 3: Check KVM is accessible from CONTAINER
    // This is the real test for nested virtualization - the container needs to use KVM
    println!("\nTest 3: Verify /dev/kvm is accessible from CONTAINER");
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &fcvm_pid.to_string(),
            // No --vm flag = runs inside container
            "--",
            "sh",
            "-c",
            "test -e /dev/kvm && test -r /dev/kvm && test -w /dev/kvm && echo 'OK' || echo 'FAIL'",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("running fcvm exec in container")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !stdout.contains("OK") {
        common::kill_process(fcvm_pid).await;
        bail!(
            "/dev/kvm NOT accessible from container!\n\
            stdout: {}\n\
            stderr: {}\n\
            The container needs --privileged and --device /dev/kvm to access KVM.",
            stdout.trim(),
            stderr.trim()
        );
    }
    println!("  ✓ /dev/kvm is accessible from container");

    // Clean up
    common::kill_process(fcvm_pid).await;

    println!("\n✅ NESTED TEST PASSED - container can use /dev/kvm!");
    Ok(())
}

/// Test running fcvm inside an fcvm VM (single level nesting)
///
/// This test:
/// 1. Starts an outer VM with nested kernel + privileged mode
/// 2. Mounts host fcvm binary and assets into the VM
/// 3. Verifies /dev/kvm is accessible from the guest
/// 4. Tests if nested KVM actually works (KVM_CREATE_VM ioctl)
/// 5. If nested KVM works, runs fcvm inside the outer VM
///
/// REQUIRES: ARM64 with FEAT_NV2 (ARMv8.4+) and kvm-arm.mode=nested
/// Skips if nested KVM isn't available.
#[tokio::test]
async fn test_nested_run_fcvm_inside_vm() -> Result<()> {
    println!("\nNested VM Test: Run fcvm inside fcvm");
    println!("=====================================");

    let fcvm_path = common::find_fcvm_binary()?;
    let fcvm_dir = fcvm_path.parent().unwrap();
    let (vm_name, _, _, _) = common::unique_names("nested-full");

    // 1. Start outer VM with nested kernel profile
    println!("\n1. Starting outer VM with nested kernel profile...");
    println!("   Mounting: /mnt/fcvm-btrfs (assets) and fcvm binary");

    let fcvm_volume = format!("{}:/opt/fcvm", fcvm_dir.display());
    // Mount host config dir so inner fcvm can find its config
    // Use $HOME which is set by spawn_fcvm based on the current user
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let config_mount = format!("{0}/.config/fcvm:/root/.config/fcvm:ro", home);
    // Use nginx so health check works (bridged networking does HTTP health check to port 80)
    // Note: firecracker is in /mnt/fcvm-btrfs/bin which is mounted via the btrfs mount
    let (mut _child, outer_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "--kernel-profile",
        "nested",
        "--privileged",
        "--map",
        "/mnt/fcvm-btrfs:/mnt/fcvm-btrfs",
        "--map",
        &fcvm_volume,
        "--map",
        &config_mount,
        common::TEST_IMAGE, // nginx:alpine - has HTTP server on port 80
    ])
    .await
    .context("spawning outer VM")?;

    println!("   Outer VM started (PID: {})", outer_pid);

    // Wait for outer VM
    println!("   Waiting for outer VM to be healthy...");
    if let Err(e) = common::poll_health_by_pid(outer_pid, 120).await {
        common::kill_process(outer_pid).await;
        return Err(e.context("outer VM failed to become healthy"));
    }
    println!("   ✓ Outer VM is healthy!");

    // 2. Verify mounts and /dev/kvm inside outer VM
    println!("\n2. Verifying mounts inside outer VM...");
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &outer_pid.to_string(),
            "--vm",
            "--",
            "sh",
            "-c",
            "ls -la /opt/fcvm/fcvm /mnt/fcvm-btrfs/kernels/ /dev/kvm 2>&1 | head -10",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("   {}", stdout.trim().replace('\n', "\n   "));

    if !stdout.contains("fcvm") || !stdout.contains("vmlinux") {
        common::kill_process(outer_pid).await;
        bail!("Required files not mounted in outer VM:\n{}", stdout);
    }
    println!("   ✓ All required files mounted");

    // 3. Test if nested KVM actually works
    println!("\n3. Testing if nested KVM works (KVM_CREATE_VM ioctl)...");

    // First, check kernel config and dmesg for KVM-related messages
    let debug_output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec", "--pid", &outer_pid.to_string(), "--vm", "--",
            "sh", "-c", r#"
echo "=== Kernel config (KVM/VIRTUALIZATION) ==="
zcat /proc/config.gz 2>/dev/null | grep -E "^CONFIG_(KVM|VIRTUALIZATION)" || echo "config.gz not available"

echo ""
echo "=== dmesg: KVM messages ==="
dmesg 2>/dev/null | grep -i kvm | head -20 || echo "dmesg not available"

echo ""
echo "=== dmesg: VHE/EL2 messages ==="
dmesg 2>/dev/null | grep -iE "(vhe|el2|hyp)" | head -10 || echo "none found"

echo ""
echo "=== CPU features ==="
cat /proc/cpuinfo | grep -E "^(Features|CPU implementer)" | head -2

echo ""
echo "=== /dev/kvm status ==="
ls -la /dev/kvm 2>&1
"#,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("getting debug info")?;

    let debug_stdout = String::from_utf8_lossy(&debug_output.stdout);
    println!(
        "   Debug info:\n{}",
        debug_stdout
            .lines()
            .map(|l| format!("   {}", l))
            .collect::<Vec<_>>()
            .join("\n")
    );

    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &outer_pid.to_string(),
            "--vm",
            "--",
            "python3",
            "-c",
            r#"
import os
import fcntl
KVM_GET_API_VERSION = 0xAE00
KVM_CREATE_VM = 0xAE01
try:
    fd = os.open("/dev/kvm", os.O_RDWR)
    version = fcntl.ioctl(fd, KVM_GET_API_VERSION, 0)
    vm_fd = fcntl.ioctl(fd, KVM_CREATE_VM, 0)
    os.close(vm_fd)
    os.close(fd)
    print("NESTED_KVM_WORKS")
except OSError as e:
    print(f"NESTED_KVM_FAILED: {e}")
"#,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("testing nested KVM")?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    if !stdout.contains("NESTED_KVM_WORKS") {
        // Nested KVM not available - skip the test
        common::kill_process(outer_pid).await;
        println!("SKIPPED: Nested KVM not available (KVM_CREATE_VM failed)");
        println!("         This requires: ARM64 with FEAT_NV2 + kvm-arm.mode=nested");
        if stdout.contains("NESTED_KVM_FAILED") {
            println!("         Error: {}", stdout.trim());
        }
        return Ok(());
    }
    println!("   ✓ Nested KVM works! Proceeding with nested VM test.");

    // 4. Run fcvm inside the outer VM (only if nested KVM works)
    println!("\n4. Running fcvm inside outer VM (NESTED)...");
    println!("   This will create a nested VM inside the outer VM");

    // Run fcvm with bridged networking inside the outer VM
    // The outer VM has --privileged so iptables/namespaces work
    // Use --cmd for the container command (fcvm doesn't support trailing args after IMAGE)
    // Set HOME explicitly to ensure config file is found
    let inner_cmd = r#"
        export PATH=/opt/fcvm:/mnt/fcvm-btrfs/bin:$PATH
        export HOME=/root
        # Load tun kernel module (needed for TAP device creation)
        modprobe tun 2>/dev/null || true
        mkdir -p /dev/net
        mknod /dev/net/tun c 10 200 2>/dev/null || true
        chmod 666 /dev/net/tun
        cd /mnt/fcvm-btrfs
        # Use bridged networking (outer VM is privileged so iptables works)
        fcvm podman run \
            --name inner-test \
            --network bridged \
            --cmd "echo NESTED_SUCCESS_INNER_VM_WORKS" \
            alpine:latest
    "#;

    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &outer_pid.to_string(),
            "--vm",
            "--",
            "sh",
            "-c",
            inner_cmd,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("running fcvm inside outer VM")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    println!("   Inner VM output:");
    for line in stdout.lines().take(20) {
        println!("     {}", line);
    }
    if !stderr.is_empty() {
        println!("   Inner VM stderr (last 10 lines):");
        for line in stderr
            .lines()
            .rev()
            .take(10)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            println!("     {}", line);
        }
    }

    // 5. Cleanup
    println!("\n5. Cleaning up outer VM...");
    common::kill_process(outer_pid).await;

    // 6. Verify success
    // Check both stdout and stderr since fcvm logs container output to its own stderr
    // with [ctr:stdout] prefix, so when running via exec, the output appears in stderr
    let combined = format!("{}\n{}", stdout, stderr);
    if combined.contains("NESTED_SUCCESS_INNER_VM_WORKS") {
        println!("\n✅ NESTED TEST PASSED!");
        println!("   Successfully ran fcvm inside fcvm (nested virtualization)");
        Ok(())
    } else {
        bail!(
            "Nested virtualization failed - inner VM did not produce expected output\n\
             Expected: NESTED_SUCCESS_INNER_VM_WORKS\n\
             Got stdout: {}\n\
             Got stderr: {}",
            stdout,
            stderr
        );
    }
}

/// Build localhost/nested-test image with proper CAS invalidation
///
/// Computes a combined SHA of ALL inputs (binaries, scripts, Containerfile).
/// Rebuilds and re-exports only when inputs change.
async fn ensure_nested_image() -> Result<()> {
    let fcvm_path = common::find_fcvm_binary()?;
    let fcvm_dir = fcvm_path.parent().unwrap();

    // All inputs that affect the container image
    let src_fcvm = fcvm_dir.join("fcvm");
    let src_agent = fcvm_dir.join("fc-agent");
    // NV2 firecracker fork - installed by fcvm setup --kernel-profile nested
    let profile = fcvm::setup::get_kernel_profile("nested")?
        .ok_or_else(|| anyhow::anyhow!("nested kernel profile not found"))?;
    let src_firecracker = fcvm::setup::get_profile_firecracker_path(&profile, "nested")
        .ok_or_else(|| anyhow::anyhow!("nested profile has no custom firecracker"))?;
    if !src_firecracker.exists() {
        bail!(
            "NV2 firecracker fork not found at {}. Run: fcvm setup --kernel-profile nested",
            src_firecracker.display()
        );
    }
    let src_nested = PathBuf::from("nested.sh");
    let src_containerfile = PathBuf::from("Containerfile.nested");
    let src_config = PathBuf::from("rootfs-config.toml");

    // Compute combined SHA of all inputs
    fn file_bytes(path: &Path) -> Vec<u8> {
        std::fs::read(path).unwrap_or_default()
    }

    let mut hasher = Sha256::new();
    hasher.update(file_bytes(&src_fcvm));
    hasher.update(file_bytes(&src_agent));
    hasher.update(file_bytes(&src_firecracker));
    hasher.update(file_bytes(&src_nested));
    hasher.update(file_bytes(&src_containerfile));
    hasher.update(file_bytes(&src_config));
    let combined_sha = hex::encode(&hasher.finalize()[..6]);

    // Check if we have a marker file with the current SHA
    let marker_path = PathBuf::from("artifacts/.nested-sha");
    let cached_sha = std::fs::read_to_string(&marker_path).unwrap_or_default();

    let need_rebuild = cached_sha.trim() != combined_sha;

    if need_rebuild {
        println!(
            "Inputs changed (sha: {} → {}), rebuilding nested container...",
            if cached_sha.is_empty() {
                "none"
            } else {
                cached_sha.trim()
            },
            combined_sha
        );

        // Copy all inputs to build context
        tokio::fs::create_dir_all("artifacts").await.ok();
        std::fs::copy(&src_fcvm, "artifacts/fcvm").context("copying fcvm to artifacts/")?;
        std::fs::copy(&src_agent, "artifacts/fc-agent")
            .context("copying fc-agent to artifacts/")?;
        std::fs::copy(&src_firecracker, "artifacts/firecracker-nested").ok();

        // Force rebuild by removing old image
        tokio::process::Command::new("podman")
            .args(["rmi", "localhost/nested-test"])
            .output()
            .await
            .ok();
    }

    // Check if image exists
    let check = tokio::process::Command::new("podman")
        .args(["image", "exists", "localhost/nested-test"])
        .output()
        .await?;

    if check.status.success() && !need_rebuild {
        println!("✓ localhost/nested-test up to date (sha: {})", combined_sha);
        return Ok(());
    }

    // Build container
    println!("Building localhost/nested-test...");
    let output = tokio::process::Command::new("podman")
        .args([
            "build",
            "-t",
            "localhost/nested-test",
            "-f",
            "Containerfile.nested",
            ".",
        ])
        .output()
        .await
        .context("running podman build")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to build nested container: {}", stderr);
    }

    // Export to CAS cache so nested VMs can access it
    let digest_out = tokio::process::Command::new("podman")
        .args([
            "inspect",
            "localhost/nested-test",
            "--format",
            "{{.Digest}}",
        ])
        .output()
        .await?;
    let digest = String::from_utf8_lossy(&digest_out.stdout)
        .trim()
        .to_string();

    if !digest.is_empty() && digest.starts_with("sha256:") {
        // Use the same format as fcvm: strip sha256: prefix and use .oci.tar extension
        let digest_stripped = digest.trim_start_matches("sha256:");
        let archive_path = format!("/mnt/fcvm-btrfs/image-cache/{}.oci.tar", digest_stripped);

        if !PathBuf::from(&archive_path).exists() {
            println!("Exporting to CAS cache: {}", archive_path);
            tokio::fs::create_dir_all("/mnt/fcvm-btrfs/image-cache")
                .await
                .ok();
            let save_out = tokio::process::Command::new("podman")
                .args([
                    "save",
                    "--format",
                    "oci-archive",
                    "-o",
                    &archive_path,
                    "localhost/nested-test",
                ])
                .output()
                .await?;
            if !save_out.status.success() {
                println!(
                    "Warning: podman save failed: {}",
                    String::from_utf8_lossy(&save_out.stderr)
                );
            }
        }

        // Save the combined SHA as marker
        std::fs::write(&marker_path, &combined_sha).ok();

        println!(
            "✓ localhost/nested-test ready (sha: {}, digest: {})",
            combined_sha,
            &digest[..std::cmp::min(19, digest.len())]
        );
    } else {
        println!("✓ localhost/nested-test built (no digest available)");
    }

    Ok(())
}

/// Run an nested chain test with configurable depth.
///
/// This function attempts to run VMs nested N levels deep:
/// Host → Level 1 → Level 2 → ... → Level N
///
/// Each nested level uses localhost/nested-test which has fcvm baked in.
///
/// REQUIRES: ARM64 with FEAT_NV2 (ARMv8.4+) and kvm-arm.mode=nested
#[allow(dead_code)] // Helper for future L3+ tests (currently L3 is too slow)
async fn run_nested_chain(total_levels: usize) -> Result<()> {
    let success_marker = format!("NESTED_CHAIN_{}_LEVELS_SUCCESS", total_levels);

    println!("\nNested Chain Test: {} levels of nested VMs", total_levels);
    println!("{}", "=".repeat(50));

    // Ensure prerequisites
    ensure_nested_image().await?;

    let fcvm_path = common::find_fcvm_binary()?;

    // Home dir for config mount
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let config_mount = format!("{0}/.config/fcvm:/root/.config/fcvm:ro", home);

    // Track PIDs for cleanup
    let mut level_pids: Vec<u32> = Vec::new();

    // Helper to cleanup all VMs (takes ownership to avoid lifetime issues)
    async fn cleanup_vms(pids: Vec<u32>) {
        for pid in pids.into_iter().rev() {
            common::kill_process(pid).await;
        }
    }

    // === Level 1: Start from host with localhost/nested-test ===
    // This image has fcvm baked in, fcvm handles export to cache automatically
    println!("\n[Level 1] Starting outer VM from host...");
    let (vm_name_1, _, _, _) = common::unique_names("nested-L1");

    // L1 uses 4GB RAM (needs to fit L2-L4 inside + overhead)
    let (mut _child1, pid1) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name_1,
        "--network",
        "bridged",
        "--kernel-profile",
        "nested",
        "--privileged",
        "--mem",
        "4096", // L1 gets 4GB, nested VMs get progressively less
        "--map",
        "/mnt/fcvm-btrfs:/mnt/fcvm-btrfs",
        "--map",
        &config_mount,
        "localhost/nested-test",
    ])
    .await
    .context("spawning Level 1 VM")?;

    level_pids.push(pid1);
    println!("[Level 1] Started (PID: {}), waiting for health...", pid1);

    if let Err(e) = common::poll_health_by_pid(pid1, 180).await {
        cleanup_vms(level_pids.clone()).await;
        return Err(e.context("Level 1 VM failed to become healthy"));
    }
    println!("[Level 1] ✓ Healthy!");

    // Check if nested KVM works before proceeding
    // Run in container (default) which has python3 and access to /dev/kvm (privileged)
    println!("\n[Level 1] Checking if nested KVM works...");
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &pid1.to_string(),
            // Default is container exec (no --vm flag needed)
            "--",
            "python3",
            "-c",
            r#"
import os, fcntl
try:
    fd = os.open("/dev/kvm", os.O_RDWR)
    vm_fd = fcntl.ioctl(fd, 0xAE01, 0)  # KVM_CREATE_VM
    os.close(vm_fd)
    os.close(fd)
    print("NESTED_KVM_OK")
except OSError as e:
    print(f"NESTED_KVM_FAIL: {e}")
"#,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.contains("NESTED_KVM_OK") {
        cleanup_vms(level_pids.clone()).await;
        println!("SKIPPED: Nested KVM not available");
        println!("         Requires ARM64 with FEAT_NV2 + kvm-arm.mode=nested");
        return Ok(());
    }
    println!("[Level 1] ✓ Nested KVM works!");

    // Build a nested script that chains all levels
    // Each level starts the next, innermost level echoes success
    // This creates a single deeply-nested command that runs through all levels

    // Get the exact image digest so we can pass the explicit cache path down the chain
    let nested_image = "localhost/nested-test";
    let digest_output = tokio::process::Command::new("podman")
        .args(["inspect", nested_image, "--format", "{{.Digest}}"])
        .output()
        .await
        .context("getting image digest")?;
    let image_digest = String::from_utf8_lossy(&digest_output.stdout)
        .trim()
        .to_string();
    if image_digest.is_empty() || !image_digest.starts_with("sha256:") {
        bail!(
            "Failed to get image digest: {:?}",
            String::from_utf8_lossy(&digest_output.stderr)
        );
    }
    // Strip sha256: prefix to match fcvm's cache path format
    let digest_stripped = image_digest.trim_start_matches("sha256:");
    let image_cache_path = format!("/mnt/fcvm-btrfs/image-cache/{}.oci.tar", digest_stripped);
    println!("[Setup] Image digest: {}", image_digest);
    println!("[Setup] Cache path: {}", image_cache_path);

    // Get the nested kernel path for the nesting script
    let nested_kernel =
        fcvm::setup::get_kernel_path(Some("nested")).context("getting nested kernel path")?;
    let kernel_path_str = nested_kernel.to_string_lossy();

    // The nesting script is baked into the container at /usr/local/bin/nested
    // It takes: nested <current_level> <max_level> <kernel_path> <image_cache_path>
    // Starting from level 2 (L1 is already running), going to total_levels
    let nested_cmd = format!(
        "nested 2 {} {} {}",
        total_levels, kernel_path_str, image_cache_path
    );

    println!(
        "\n[Levels 2-{}] Starting nested nested chain from Level 1...",
        total_levels
    );
    println!("  This will boot {} VMs sequentially", total_levels - 1);

    // Run in container (default, no --vm) because the nesting script is in the container
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &pid1.to_string(),
            // Default is container exec (no --vm flag)
            "--",
            "sh",
            "-c",
            &nested_cmd,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("running nested nested chain")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{}\n{}", stdout, stderr);

    println!("\n[Chain Output] (last 30 lines):");
    for line in combined
        .lines()
        .rev()
        .take(30)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        println!("  {}", line);
    }

    // Cleanup Level 1 (cascades to inner levels)
    println!("\nCleaning up all VMs...");
    cleanup_vms(level_pids.clone()).await;

    // Debug: Check what we're looking for
    println!("\n[Debug] Looking for marker: {}", success_marker);
    println!(
        "[Debug] Marker found in output: {}",
        combined.contains(&success_marker)
    );
    println!("[Debug] exec exit status: {:?}", output.status);

    // First check if the exec command itself failed
    if !output.status.success() {
        bail!(
            "Nested chain failed - exec command exited with status {:?}\n\
             Expected marker: {}\n\
             stdout (last 500 chars): {}\n\
             stderr (last 500 chars): {}",
            output.status,
            success_marker,
            stdout
                .chars()
                .rev()
                .take(500)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>(),
            stderr
                .chars()
                .rev()
                .take(500)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        );
    }

    if combined.contains(&success_marker) {
        println!("\n✅ NESTED CHAIN TEST PASSED!");
        println!("   Successfully ran {} levels of nested VMs", total_levels);
        Ok(())
    } else {
        bail!(
            "Nested chain failed at {} levels\n\
             Expected marker: {}\n\
             stdout (last 1000 chars): {}\n\
             stderr (last 1000 chars): {}",
            total_levels,
            success_marker,
            stdout
                .chars()
                .rev()
                .take(1000)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>(),
            stderr
                .chars()
                .rev()
                .take(1000)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        )
    }
}

/// Test L1→L2 nesting: run fcvm inside L1 to start L2
///
/// L1: Host starts VM with localhost/nested-test + nested kernel profile
/// L2: L1 container imports image from shared cache, then runs fcvm
///
/// Uses the shared run_nested_n_levels infrastructure for consistency.
///
/// IGNORED: Flaky due to FUSE stream corruption under high I/O load (~8K requests).
/// See README.md "Known Issues (Nested)" for details. Fix tracked in issue.
#[tokio::test]
#[ignore = "FUSE stream corruption at ~8K requests - see README Known Issues"]
async fn test_nested_l2() -> Result<()> {
    run_nested_n_levels(2, "NESTED_2_LEVELS_SUCCESS").await
}

/// Test L1→L2→L3: 3 levels of nesting
///
/// BLOCKED: 3-hop FUSE chain (L3→L2→L1→HOST) causes ~3-5 second latency per
/// request due to PassthroughFs + spawn_blocking serialization. FUSE mount
/// initialization alone takes 10+ minutes. Need to implement request pipelining
/// or async PassthroughFs before this test can complete in reasonable time.
#[tokio::test]
#[ignore]
async fn test_nested_l3() -> Result<()> {
    run_nested_n_levels(3, "MARKER_L3_OK_12345").await
}

/// Test L1→L2→L3→L4: 4 levels of nesting
///
/// BLOCKED: Same issue as L3, but worse. 4-hop FUSE chain would be even slower.
#[tokio::test]
#[ignore]
async fn test_nested_l4() -> Result<()> {
    run_nested_n_levels(4, "MARKER_L4_OK_12345").await
}

/// Benchmark script that runs at each nesting level
///
/// Measures: egress connectivity, local disk I/O, FUSE disk I/O, FUSE latency, memory usage.
/// Outputs MARKER_L{level}_OK on success.
fn benchmark_script() -> &'static str {
    r#"#!/bin/bash
set -e
LEVEL=${1:-unknown}

echo "=== BENCHMARK L${LEVEL} ==="

# Test 1: Egress - can we reach the internet?
echo "--- Egress Test ---"
if curl -s --max-time 10 http://ifconfig.me > /tmp/egress.txt 2>&1; then
    IP=$(cat /tmp/egress.txt)
    echo "EGRESS_L${LEVEL}=OK ip=${IP}"
else
    echo "EGRESS_L${LEVEL}=FAIL"
fi

# Test 2: Local disk performance (dd to /tmp which is on rootfs)
echo "--- Local Disk Test ---"
# Write 10MB
START=$(date +%s%N)
dd if=/dev/zero of=/tmp/bench.dat bs=1M count=10 conv=fsync 2>/dev/null
END=$(date +%s%N)
WRITE_MS=$(( (END - START) / 1000000 ))
echo "LOCAL_WRITE_L${LEVEL}=${WRITE_MS}ms (10MB)"

# Read back
START=$(date +%s%N)
dd if=/tmp/bench.dat of=/dev/null bs=1M 2>/dev/null
END=$(date +%s%N)
READ_MS=$(( (END - START) / 1000000 ))
echo "LOCAL_READ_L${LEVEL}=${READ_MS}ms (10MB)"
rm -f /tmp/bench.dat

# Test 3: FUSE disk performance (if /mnt/fcvm-btrfs is mounted)
if mountpoint -q /mnt/fcvm-btrfs 2>/dev/null; then
    echo "--- FUSE Disk Test ---"
    FUSE_DIR="/mnt/fcvm-btrfs/bench-${LEVEL}-$$"
    mkdir -p "$FUSE_DIR"

    # Write 10MB (with fsync)
    START=$(date +%s%N)
    dd if=/dev/zero of="${FUSE_DIR}/bench.dat" bs=1M count=10 conv=fsync 2>/dev/null
    END=$(date +%s%N)
    WRITE_MS=$(( (END - START) / 1000000 ))
    echo "FUSE_WRITE_L${LEVEL}=${WRITE_MS}ms (10MB)"

    # Write 10MB (no fsync - async)
    START=$(date +%s%N)
    dd if=/dev/zero of="${FUSE_DIR}/bench2.dat" bs=1M count=10 2>/dev/null
    END=$(date +%s%N)
    ASYNC_MS=$(( (END - START) / 1000000 ))
    echo "FUSE_ASYNC_L${LEVEL}=${ASYNC_MS}ms (10MB no-sync)"

    # Read back
    START=$(date +%s%N)
    dd if="${FUSE_DIR}/bench.dat" of=/dev/null bs=1M 2>/dev/null
    END=$(date +%s%N)
    READ_MS=$(( (END - START) / 1000000 ))
    echo "FUSE_READ_L${LEVEL}=${READ_MS}ms (10MB)"

    rm -rf "$FUSE_DIR"
else
    echo "FUSE_L${LEVEL}=NOT_MOUNTED"
fi

# Test 4: FUSE latency (small ops to measure per-op overhead)
if mountpoint -q /mnt/fcvm-btrfs 2>/dev/null; then
    echo "--- FUSE Latency Test ---"
    FUSE_DIR="/mnt/fcvm-btrfs/bench-lat-${LEVEL}-$$"
    mkdir -p "$FUSE_DIR"

    # Create test file
    echo "test" > "${FUSE_DIR}/lat.txt"

    # 100 stat operations
    START=$(date +%s%N)
    for i in $(seq 1 100); do stat "${FUSE_DIR}/lat.txt" > /dev/null; done
    END=$(date +%s%N)
    STAT_US=$(( (END - START) / 100000 ))
    echo "FUSE_STAT_L${LEVEL}=${STAT_US}us/op (100 ops)"

    # 100 small reads (4 bytes each)
    START=$(date +%s%N)
    for i in $(seq 1 100); do cat "${FUSE_DIR}/lat.txt" > /dev/null; done
    END=$(date +%s%N)
    READ_US=$(( (END - START) / 1000 ))
    echo "FUSE_SMALLREAD_L${LEVEL}=${READ_US}us/op (100 ops)"

    rm -rf "$FUSE_DIR"
fi

# Test 5: Large FUSE copy - DISABLED (too slow at L2 with FUSE-over-FUSE)
# if mountpoint -q /mnt/fcvm-btrfs 2>/dev/null; then
#     echo "--- Large Copy Test ---"
#     FUSE_DIR="/mnt/fcvm-btrfs/bench-copy-${LEVEL}-$$"
#     mkdir -p "$FUSE_DIR"
#     dd if=/dev/urandom of=/tmp/large.dat bs=1M count=100 2>/dev/null
#     START=$(date +%s%N)
#     cp /tmp/large.dat "${FUSE_DIR}/large.dat"
#     sync
#     END=$(date +%s%N)
#     COPY_TO_MS=$(( (END - START) / 1000000 ))
#     COPY_TO_MBS=$(( 100 * 1000 / (COPY_TO_MS + 1) ))
#     echo "FUSE_COPY_TO_L${LEVEL}=${COPY_TO_MS}ms (100MB, ${COPY_TO_MBS}MB/s)"
#     START=$(date +%s%N)
#     cp "${FUSE_DIR}/large.dat" /tmp/large2.dat
#     END=$(date +%s%N)
#     COPY_FROM_MS=$(( (END - START) / 1000000 ))
#     COPY_FROM_MBS=$(( 100 * 1000 / (COPY_FROM_MS + 1) ))
#     echo "FUSE_COPY_FROM_L${LEVEL}=${COPY_FROM_MS}ms (100MB, ${COPY_FROM_MBS}MB/s)"
#     rm -rf "$FUSE_DIR" /tmp/large.dat /tmp/large2.dat
# fi

# Test 6: Memory usage (RSS)
echo "--- Memory Test ---"
MEM_TOTAL=$(grep MemTotal /proc/meminfo | awk '{print $2}')
MEM_AVAIL=$(grep MemAvailable /proc/meminfo | awk '{print $2}')
MEM_USED=$((MEM_TOTAL - MEM_AVAIL))
MEM_USED_MB=$((MEM_USED / 1024))
MEM_TOTAL_MB=$((MEM_TOTAL / 1024))
echo "MEM_L${LEVEL}=${MEM_USED_MB}MB/${MEM_TOTAL_MB}MB"

echo "=== END BENCHMARK L${LEVEL} ==="
echo "MARKER_L${LEVEL}_OK"
"#
}

/// Print a summary of benchmark results from log content
fn print_benchmark_summary(log_content: &str) {
    println!("\n=== BENCHMARK SUMMARY ===");
    for line in log_content.lines() {
        if line.contains("EGRESS_L")
            || line.contains("LOCAL_WRITE_L")
            || line.contains("LOCAL_READ_L")
            || line.contains("FUSE_WRITE_L")
            || line.contains("FUSE_ASYNC_L")
            || line.contains("FUSE_READ_L")
            || line.contains("FUSE_STAT_L")
            || line.contains("FUSE_SMALLREAD_L")
            || line.contains("MEM_L")
        {
            // Strip ANSI codes and prefixes
            let clean = line.split("stdout]").last().unwrap_or(line).trim();
            println!("{}", clean);
        }
    }
    println!("=========================\n");
}

/// Run N levels of nesting with benchmarks at each level
///
/// Each level runs performance benchmarks (egress, local disk, FUSE disk, memory)
/// before starting the next level. Uses streaming output for real-time visibility.
async fn run_nested_n_levels(n: usize, marker: &str) -> Result<()> {
    assert!(n >= 2, "Need at least 2 levels for nesting");

    ensure_nested_image().await?;

    // Get the nested kernel path (with correct SHA computed on host)
    // This is needed because inside L1, the kernel build inputs don't exist
    // so the SHA computation would fail
    let nested_kernel =
        fcvm::setup::get_kernel_path(Some("nested")).context("getting nested kernel path")?;
    let nested_kernel_path = nested_kernel.to_string_lossy();

    // Get the digest of localhost/nested-test
    let digest_out = tokio::process::Command::new("podman")
        .args([
            "inspect",
            "localhost/nested-test",
            "--format",
            "{{.Digest}}",
        ])
        .output()
        .await?;
    let digest = String::from_utf8_lossy(&digest_out.stdout)
        .trim()
        .to_string();
    // Strip sha256: prefix for cache path (cache files don't have prefix)
    let digest_stripped = digest.trim_start_matches("sha256:");
    println!("Image digest: {}", digest);

    // Memory allocation strategy:
    // - Each VM needs enough memory to run its child's Firecracker (~2GB) + OS overhead (~500MB)
    // - Intermediate levels (L1..L(n-1)): 4GB each to accommodate child VM + OS
    // - Deepest level (Ln): 2GB (default) since it just runs echo
    let intermediate_mem = "4096"; // 4GB for VMs that spawn children

    // Build scripts from deepest level (Ln) upward to L1
    // Every level runs benchmarks
    // Ln (deepest): run benchmarks + echo marker
    // L1..L(n-1): run benchmarks + import image + run fcvm with next level's script

    let scripts_dir = "/mnt/fcvm-btrfs/nested-scripts";
    tokio::fs::create_dir_all(scripts_dir).await.ok();

    // Write the benchmark script
    let bench_path = format!("{}/bench.sh", scripts_dir);
    tokio::fs::write(&bench_path, benchmark_script()).await?;
    tokio::process::Command::new("chmod")
        .args(["+x", &bench_path])
        .status()
        .await?;

    // Deepest level (Ln): run benchmark + echo marker
    let ln_script = format!(
        "#!/bin/bash\nset -ex\n{scripts_dir}/bench.sh {n}\necho {marker}\n",
        scripts_dir = scripts_dir,
        n = n,
        marker = marker
    );
    let ln_path = format!("{}/l{}.sh", scripts_dir, n);
    tokio::fs::write(&ln_path, &ln_script).await?;
    tokio::process::Command::new("chmod")
        .args(["+x", &ln_path])
        .status()
        .await?;
    println!("L{}: benchmark + echo marker", n);

    // Build L(n-1) down to L1: each imports image and runs fcvm with next script
    // Each level needs:
    // - --map to access shared storage
    // - --mem for intermediate levels to fit child VM
    // - --kernel-profile for nested virtualization

    for level in (1..n).rev() {
        let next_script = format!("{}/l{}.sh", scripts_dir, level + 1);

        // Every level in this loop runs `fcvm podman run`, spawning a child VM.
        // Each spawned VM runs Firecracker which needs ~2GB. So every level that
        // spawns a VM needs extra memory (4GB) to fit:
        // - Firecracker process for child VM (~2GB)
        // - OS overhead and containers (~1-2GB)
        //
        // L(n) (deepest, created outside this loop) just runs echo, no child VMs.
        // All other levels (1 to n-1) spawn VMs and need 4GB.
        let mem_arg = format!("--mem {}", intermediate_mem);

        // Use both --kernel-profile (for runtime config: firecracker_bin, boot_args)
        // AND --kernel (explicit path, since build_inputs don't exist inside container)
        //
        // Each intermediate level: run benchmark FIRST, then import + start next level
        let script = format!(
            r#"#!/bin/bash
set -ex

# Run benchmarks for this level
{scripts_dir}/bench.sh {level}

echo "L{level}: Importing image from shared cache..."
podman load -i /mnt/fcvm-btrfs/image-cache/{digest}.oci.tar
podman tag sha256:{digest} localhost/nested-test 2>/dev/null || true

echo "L{level}: Starting L{next_level} VM..."
FCVM_FUSE_TRACE_RATE=100 fcvm podman run \
    --name l{next_level} \
    --network bridged \
    --privileged \
    {mem_arg} \
    --kernel-profile nested \
    --kernel {kernel} \
    --map /mnt/fcvm-btrfs:/mnt/fcvm-btrfs \
    localhost/nested-test \
    --cmd {next_script}
"#,
            scripts_dir = scripts_dir,
            level = level,
            next_level = level + 1,
            digest = digest_stripped,
            mem_arg = mem_arg,
            kernel = nested_kernel_path,
            next_script = next_script
        );
        let script_path = format!("{}/l{}.sh", scripts_dir, level);
        tokio::fs::write(&script_path, &script).await?;
        tokio::process::Command::new("chmod")
            .args(["+x", &script_path])
            .status()
            .await?;
        println!(
            "L{}: benchmark + import + fcvm {} --kernel-profile nested --kernel <path> --cmd {}",
            level, mem_arg, next_script
        );
    }

    // Run L1 from host with nested kernel profile
    // L1 needs extra memory since it spawns L2
    let l1_script = format!("{}/l1.sh", scripts_dir);
    println!(
        "\nStarting {} levels of nesting with 4GB per intermediate VM...",
        n
    );

    // Use sh -c with tee to stream output in real-time AND capture for marker check
    let log_file = format!("/tmp/nested-l{}.log", n);
    let fcvm_cmd = format!(
        "sudo ./target/release/fcvm podman run \
         --name l1-nested-{} \
         --network bridged \
         --privileged \
         --mem {} \
         --kernel-profile nested \
         --map /mnt/fcvm-btrfs:/mnt/fcvm-btrfs \
         localhost/nested-test \
         --cmd {} 2>&1 | tee {}",
        n, intermediate_mem, l1_script, log_file
    );

    let status = tokio::process::Command::new("sh")
        .args(["-c", &fcvm_cmd])
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .await?;

    // Read log file to check for marker
    let log_content = tokio::fs::read_to_string(&log_file)
        .await
        .unwrap_or_default();

    // Verify all level markers present
    for level in 1..=n {
        let level_marker = format!("MARKER_L{}_OK", level);
        assert!(
            log_content.contains(&level_marker),
            "L{} benchmark should complete (missing {}). Exit status: {:?}. Check output above.",
            level,
            level_marker,
            status
        );
    }

    // Also verify the final marker
    assert!(
        log_content.contains(marker),
        "Final marker '{}' not found. Exit status: {:?}. Check output above.",
        marker,
        status
    );

    // Extract and display benchmark summary
    print_benchmark_summary(&log_content);

    Ok(())
}

/// Test podman load performance over FUSE (localhost/nested-test)
///
/// Measures how long it takes to import the nested container image
/// inside a VM when the image archive is accessed over FUSE-over-vsock.
#[tokio::test]
async fn test_podman_load_over_fuse() -> Result<()> {
    println!("\nPodman Load Over FUSE Performance Test");
    println!("======================================\n");

    // 1. Ensure localhost/nested-test exists and is cached
    println!("1. Ensuring localhost/nested-test exists...");
    ensure_nested_image().await?;
    println!("   ✓ Image ready");

    // 2. Get image digest and verify cache exists
    println!("2. Getting image digest...");
    let digest_output = tokio::process::Command::new("podman")
        .args([
            "inspect",
            "localhost/nested-test",
            "--format",
            "{{.Digest}}",
        ])
        .output()
        .await?;
    let digest = String::from_utf8_lossy(&digest_output.stdout)
        .trim()
        .to_string();

    if digest.is_empty() || !digest.starts_with("sha256:") {
        bail!("Invalid digest: {}", digest);
    }

    let digest_stripped = digest.trim_start_matches("sha256:");
    let archive_path = format!("/mnt/fcvm-btrfs/image-cache/{}.oci.tar", digest_stripped);
    println!("   Digest: sha256:{}...", &digest_stripped[..12]);

    if !std::path::Path::new(&archive_path).exists() {
        bail!("OCI archive not found at {}", archive_path);
    }

    // Get archive size
    let metadata = std::fs::metadata(&archive_path)?;
    let size_mb = metadata.len() as f64 / (1024.0 * 1024.0);
    println!("   Archive size: {:.1} MB", size_mb);

    // 3. Start L1 VM with FUSE mount
    println!("3. Starting L1 VM with FUSE mount...");

    let (vm_name, _, _, _) = common::unique_names("podman-load");

    let (mut _child, vm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "--kernel-profile",
        "nested",
        "--privileged",
        "--map",
        "/mnt/fcvm-btrfs:/mnt/fcvm-btrfs",
        common::TEST_IMAGE,
    ])
    .await
    .context("spawning VM")?;

    println!("   VM started (PID: {})", vm_pid);
    println!("   Waiting for VM to be healthy...");

    if let Err(e) = common::poll_health_by_pid(vm_pid, 120).await {
        common::kill_process(vm_pid).await;
        return Err(e.context("VM failed to become healthy"));
    }
    println!("   ✓ VM is healthy");

    // 4. Time the podman load inside the VM
    println!("\n4. Timing podman load inside VM...");
    println!("   Source: {}", archive_path);

    let fcvm_path = common::find_fcvm_binary()?;
    let start = std::time::Instant::now();

    let load_cmd = format!("time podman load -i {} 2>&1", archive_path);

    let load_output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &vm_pid.to_string(),
            "--vm",
            "--",
            "sh",
            "-c",
            &load_cmd,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    let elapsed = start.elapsed();

    let stdout = String::from_utf8_lossy(&load_output.stdout);
    println!("\n   Output:\n   {}", stdout.trim().replace('\n', "\n   "));

    // 5. Clean up
    println!("\n5. Cleaning up...");
    common::kill_process(vm_pid).await;

    // 6. Report results
    println!("\n==========================================");
    println!(
        "RESULT: {:.1} MB image load over FUSE took {:?}",
        size_mb, elapsed
    );
    println!("==========================================");

    if elapsed.as_secs_f64() > 0.0 {
        let throughput = size_mb / elapsed.as_secs_f64();
        println!("Throughput: {:.1} MB/s", throughput);
    }

    if elapsed.as_secs() > 300 {
        println!("\n⚠️  Load is VERY SLOW (>5min) - need optimization");
    } else if elapsed.as_secs() > 60 {
        println!("\n⚠️  Load is SLOW (>60s) - consider optimization");
    } else if elapsed.as_secs() > 10 {
        println!("\n⚠️  Load is MODERATE (10-60s)");
    } else {
        println!("\n✓ Load is FAST (<10s)");
    }

    Ok(())
}
