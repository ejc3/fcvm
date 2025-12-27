//! Integration test for inception support - verifies /dev/kvm works in guest
//!
//! This test generates a custom rootfs-config.toml pointing to the inception
//! kernel (with CONFIG_KVM=y), then verifies /dev/kvm works in the VM.
//!
//! # Nested Virtualization Status (2025-12-27)
//!
//! ## Implementation Complete
//! - Host kernel 6.18.2-nested with `kvm-arm.mode=nested` properly initializes NV2 mode
//! - KVM_CAP_ARM_EL2 (capability 240) returns 1, indicating nested virt is supported
//! - vCPU init with KVM_ARM_VCPU_HAS_EL2 (bit 7) + HAS_EL2_E2H0 (bit 8) succeeds
//! - Firecracker patched to:
//!   - Enable HAS_EL2 + HAS_EL2_E2H0 features (--enable-nv2 CLI flag)
//!   - Boot vCPU at EL2h (PSTATE_FAULT_BITS_64_EL2) so guest sees HYP mode
//!   - Set EL2 registers: HCR_EL2, CNTHCTL_EL2, VMPIDR_EL2, VPIDR_EL2
//!
//! ## Guest kernel boot (working)
//! - Guest dmesg shows: "CPU: All CPU(s) started at EL2"
//! - KVM initializes: "kvm [1]: nv: 554 coarse grained trap handlers"
//! - "kvm [1]: Hyp nVHE mode initialized successfully"
//! - /dev/kvm can be opened successfully
//!
//! ## Hardware
//! - c7g.metal (Graviton3 / Neoverse-V1) supports FEAT_NV2
//! - MIDR: 0x411fd401 (ARM Neoverse-V1)
//!
//! ## References
//! - KVM nested virt patches: https://lwn.net/Articles/921783/
//! - ARM boot protocol: arch/arm64/kernel/head.S (init_kernel_el)
//!
//! FAILS LOUDLY if /dev/kvm is not available.

#![cfg(feature = "privileged-tests")]

mod common;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Stdio;

const KERNEL_VERSION: &str = "6.12.10";
const KERNEL_DIR: &str = "/mnt/fcvm-btrfs/kernels";

/// Compute inception kernel path from build script contents
fn inception_kernel_path() -> Result<PathBuf> {
    let kernel_dir = Path::new("kernel");
    let mut content = Vec::new();

    // Read build.sh
    let script = kernel_dir.join("build.sh");
    if script.exists() {
        content.extend(std::fs::read(&script)?);
    }

    // Read inception.conf
    let conf = kernel_dir.join("inception.conf");
    if conf.exists() {
        content.extend(std::fs::read(&conf)?);
    }

    // Read patches/*.patch (sorted)
    let patches_dir = kernel_dir.join("patches");
    if patches_dir.exists() {
        let mut patches: Vec<_> = std::fs::read_dir(&patches_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "patch"))
            .collect();
        patches.sort_by_key(|e| e.path());
        for patch in patches {
            content.extend(std::fs::read(patch.path())?);
        }
    }

    // Compute SHA (first 12 hex chars)
    let mut hasher = Sha256::new();
    hasher.update(&content);
    let hash = hasher.finalize();
    let sha = hex::encode(&hash[..6]);

    Ok(PathBuf::from(KERNEL_DIR).join(format!("vmlinux-{}-{}.bin", KERNEL_VERSION, sha)))
}

/// Ensure inception kernel exists, building it if necessary
async fn ensure_inception_kernel() -> Result<PathBuf> {
    let kernel_path = inception_kernel_path()?;

    if kernel_path.exists() {
        println!("✓ Inception kernel found: {}", kernel_path.display());
        return Ok(kernel_path);
    }

    println!("Building inception kernel: {}", kernel_path.display());
    println!("  This may take 10-20 minutes on first run...");

    let status = tokio::process::Command::new("./kernel/build.sh")
        .env("KERNEL_PATH", &kernel_path)
        .status()
        .await
        .context("running kernel/build.sh")?;

    if !status.success() {
        bail!("Kernel build failed with exit code: {:?}", status.code());
    }

    if !kernel_path.exists() {
        bail!("Kernel build completed but file not found: {}", kernel_path.display());
    }

    println!("✓ Kernel built: {}", kernel_path.display());
    Ok(kernel_path)
}

#[tokio::test]
async fn test_kvm_available_in_vm() -> Result<()> {
    println!("\nInception KVM test");
    println!("==================");
    println!("Verifying /dev/kvm works with inception kernel");

    // Ensure inception kernel exists (builds if needed)
    let inception_kernel = ensure_inception_kernel().await?;

    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names("inception-kvm");

    // Start the VM with custom kernel via --kernel flag
    // Use --privileged so the container can access /dev/kvm
    println!("\nStarting VM with inception kernel (privileged mode)...");
    let kernel_str = inception_kernel.to_str().context("kernel path not valid UTF-8")?;
    let (mut _child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "--kernel",
        kernel_str,
        "--privileged",
        common::TEST_IMAGE,
    ])
    .await
    .context("spawning fcvm podman run with inception kernel")?;
    println!("  fcvm process started (PID: {})", fcvm_pid);

    // Wait for VM to become healthy
    println!("  Waiting for VM to become healthy...");
    if let Err(e) = common::poll_health_by_pid(fcvm_pid, 180).await {
        common::kill_process(fcvm_pid).await;
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
            The inception kernel was used but /dev/kvm doesn't exist.\n\
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
    // This is the real test for inception - the container needs to use KVM
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

    println!("\n✅ INCEPTION TEST PASSED - container can use /dev/kvm!");
    Ok(())
}

/// Test running fcvm inside an fcvm VM (single level inception)
///
/// This test:
/// 1. Starts an outer VM with inception kernel + privileged mode
/// 2. Mounts host fcvm binary and assets into the VM
/// 3. Verifies /dev/kvm is accessible from the guest
/// 4. Tests if nested KVM actually works (KVM_CREATE_VM ioctl)
/// 5. If nested KVM works, runs fcvm inside the outer VM
///
/// REQUIRES: ARM64 with FEAT_NV2 (ARMv8.4+) and kvm-arm.mode=nested
/// Skips if nested KVM isn't available.
#[tokio::test]
async fn test_inception_run_fcvm_inside_vm() -> Result<()> {
    println!("\nInception Test: Run fcvm inside fcvm");
    println!("=====================================");

    // Ensure inception kernel exists (builds if needed)
    let inception_kernel = ensure_inception_kernel().await?;

    let fcvm_path = common::find_fcvm_binary()?;
    let fcvm_dir = fcvm_path.parent().unwrap();
    let (vm_name, _, _, _) = common::unique_names("inception-full");

    // 1. Start outer VM with volumes for fcvm binary and assets
    println!("\n1. Starting outer VM with inception kernel...");
    println!("   Mounting: /mnt/fcvm-btrfs (assets) and fcvm binary");

    let kernel_str = inception_kernel.to_str().context("kernel path not valid UTF-8")?;
    let fcvm_volume = format!("{}:/opt/fcvm", fcvm_dir.display());
    // Mount host config dir so inner fcvm can find its config
    // Use $HOME which is set by spawn_fcvm based on the current user
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let config_mount = format!("{0}/.config/fcvm:/root/.config/fcvm:ro", home);
    // Use nginx so health check works (bridged networking does HTTP health check to port 80)
    // Note: firecracker is in /mnt/fcvm-btrfs/bin which is mounted via the btrfs mount
    let (mut _child, outer_pid) = common::spawn_fcvm(&[
        "podman", "run",
        "--name", &vm_name,
        "--network", "bridged",
        "--kernel", kernel_str,
        "--privileged",
        "--map", "/mnt/fcvm-btrfs:/mnt/fcvm-btrfs",
        "--map", &fcvm_volume,
        "--map", &config_mount,
        common::TEST_IMAGE,  // nginx:alpine - has HTTP server on port 80
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
            "exec", "--pid", &outer_pid.to_string(), "--vm", "--",
            "sh", "-c",
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
    println!("   Debug info:\n{}", debug_stdout.lines().map(|l| format!("   {}", l)).collect::<Vec<_>>().join("\n"));

    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec", "--pid", &outer_pid.to_string(), "--vm", "--",
            "python3", "-c", r#"
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
    println!("   ✓ Nested KVM works! Proceeding with inception test.");

    // 4. Run fcvm inside the outer VM (only if nested KVM works)
    println!("\n4. Running fcvm inside outer VM (INCEPTION)...");
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
            --cmd "echo INCEPTION_SUCCESS_INNER_VM_WORKS" \
            alpine:latest
    "#;

    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec", "--pid", &outer_pid.to_string(), "--vm", "--",
            "sh", "-c", inner_cmd,
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
        for line in stderr.lines().rev().take(10).collect::<Vec<_>>().into_iter().rev() {
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
    if combined.contains("INCEPTION_SUCCESS_INNER_VM_WORKS") {
        println!("\n✅ INCEPTION TEST PASSED!");
        println!("   Successfully ran fcvm inside fcvm (nested virtualization)");
        Ok(())
    } else {
        bail!(
            "Inception failed - inner VM did not produce expected output\n\
             Expected: INCEPTION_SUCCESS_INNER_VM_WORKS\n\
             Got stdout: {}\n\
             Got stderr: {}",
            stdout, stderr
        );
    }
}
