//! Integration test for inception support - verifies /dev/kvm works in guest
//!
//! This test generates a custom rootfs-config.toml pointing to the inception
//! kernel (with CONFIG_KVM=y), then verifies /dev/kvm works in the VM.
//!
//! # Nested Virtualization Status (2025-12-29)
//!
//! ## Implementation Complete (L1 only)
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
//! ## Recursive Nesting Limitation (L2+)
//! L1's KVM reports KVM_CAP_ARM_EL2=0, preventing L2+ VMs from using NV2.
//! Root cause analysis (2025-12-29):
//!
//! 1. `kvm-arm.mode=nested` requires VHE mode (kernel at EL2)
//! 2. VHE requires `is_kernel_in_hyp_mode()` = true at early boot
//! 3. But NV2's `HAS_EL2_E2H0` flag forces nVHE mode (kernel at EL1)
//! 4. E2H0 is required to avoid timer trap storms in NV2 contexts
//! 5. Without VHE, L1's kernel uses `kvm-arm.mode=nvhe` and cannot advertise KVM_CAP_ARM_EL2
//!
//! The kernel's nested virt patches include recursive nesting code, but it's marked
//! as "not tested yet". Until VHE mode works reliably with NV2, recursive nesting
//! (host → L1 → L2 → L3...) is not possible.
//!
//! ## Hardware
//! - c7g.metal (Graviton3 / Neoverse-V1) supports FEAT_NV2
//! - MIDR: 0x411fd401 (ARM Neoverse-V1)
//!
//! ## References
//! - KVM nested virt patches: https://lwn.net/Articles/921783/
//! - ARM boot protocol: arch/arm64/kernel/head.S (init_kernel_el)
//! - E2H0 handling: arch/arm64/include/asm/el2_setup.h (init_el2_hcr)
//! - Nested config: arch/arm64/kvm/nested.c (case SYS_ID_AA64MMFR4_EL1)
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
const FIRECRACKER_NV2_REPO: &str = "https://github.com/ejc3/firecracker.git";
const FIRECRACKER_NV2_BRANCH: &str = "nv2-inception";

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
        bail!(
            "Kernel build completed but file not found: {}",
            kernel_path.display()
        );
    }

    println!("✓ Kernel built: {}", kernel_path.display());
    Ok(kernel_path)
}

/// Check if Firecracker supports --enable-nv2 flag
async fn firecracker_has_nv2() -> bool {
    let output = tokio::process::Command::new("firecracker")
        .arg("--help")
        .output()
        .await;

    match output {
        Ok(out) => String::from_utf8_lossy(&out.stdout).contains("enable-nv2"),
        Err(_) => false,
    }
}

/// Ensure Firecracker with NV2 support is installed
async fn ensure_firecracker_nv2() -> Result<()> {
    if firecracker_has_nv2().await {
        println!("✓ Firecracker with NV2 support found");
        return Ok(());
    }

    println!("Building Firecracker with NV2 support...");
    println!("  This may take 5-10 minutes on first run...");

    let build_dir = PathBuf::from("/tmp/firecracker-nv2-build");

    // Clone or update the repo
    if build_dir.exists() {
        println!("  Updating existing repo...");
        let status = tokio::process::Command::new("git")
            .args(["fetch", "origin", FIRECRACKER_NV2_BRANCH])
            .current_dir(&build_dir)
            .status()
            .await?;
        if !status.success() {
            // If fetch fails, remove and re-clone
            tokio::fs::remove_dir_all(&build_dir).await?;
        }
    }

    if !build_dir.exists() {
        println!("  Cloning {}...", FIRECRACKER_NV2_REPO);
        let status = tokio::process::Command::new("git")
            .args([
                "clone",
                "--depth=1",
                "-b",
                FIRECRACKER_NV2_BRANCH,
                FIRECRACKER_NV2_REPO,
                build_dir.to_str().unwrap(),
            ])
            .status()
            .await
            .context("cloning Firecracker repo")?;

        if !status.success() {
            bail!("Failed to clone Firecracker repo");
        }
    }

    // Checkout the correct branch
    let status = tokio::process::Command::new("git")
        .args(["checkout", FIRECRACKER_NV2_BRANCH])
        .current_dir(&build_dir)
        .status()
        .await?;

    if !status.success() {
        bail!("Failed to checkout branch {}", FIRECRACKER_NV2_BRANCH);
    }

    // Build Firecracker
    println!("  Building Firecracker (release)...");
    let status = tokio::process::Command::new("cargo")
        .args(["build", "--release", "-p", "firecracker"])
        .current_dir(&build_dir)
        .status()
        .await
        .context("building Firecracker")?;

    if !status.success() {
        bail!("Firecracker build failed");
    }

    // Install to /usr/local/bin (requires sudo)
    // Firecracker uses target/release when built with cargo directly
    let mut binary = build_dir.join("target/release/firecracker");
    if !binary.exists() {
        // Try alternative path (Firecracker's custom build system)
        let alt_binary = build_dir.join("build/cargo_target/release/firecracker");
        if alt_binary.exists() {
            binary = alt_binary;
        } else {
            bail!(
                "Firecracker binary not found at {} or {}",
                binary.display(),
                alt_binary.display()
            );
        }
    }

    println!("  Installing Firecracker to /usr/local/bin...");
    let status = tokio::process::Command::new("sudo")
        .args(["cp", binary.to_str().unwrap(), "/usr/local/bin/firecracker"])
        .status()
        .await
        .context("installing Firecracker")?;

    if !status.success() {
        bail!("Failed to install Firecracker");
    }

    // Verify installation
    if !firecracker_has_nv2().await {
        bail!("Firecracker installed but --enable-nv2 flag not found");
    }

    println!("✓ Firecracker with NV2 support installed");
    Ok(())
}

#[tokio::test]
async fn test_kvm_available_in_vm() -> Result<()> {
    println!("\nInception KVM test");
    println!("==================");
    println!("Verifying /dev/kvm works with inception kernel");

    // Ensure prerequisites are installed
    ensure_firecracker_nv2().await?;
    let inception_kernel = ensure_inception_kernel().await?;

    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names("inception-kvm");

    // Start the VM with custom kernel via --kernel flag
    // Use --privileged so the container can access /dev/kvm
    println!("\nStarting VM with inception kernel (privileged mode)...");
    let kernel_str = inception_kernel
        .to_str()
        .context("kernel path not valid UTF-8")?;
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

    // Ensure prerequisites are installed
    ensure_firecracker_nv2().await?;
    let inception_kernel = ensure_inception_kernel().await?;

    let fcvm_path = common::find_fcvm_binary()?;
    let fcvm_dir = fcvm_path.parent().unwrap();
    let (vm_name, _, _, _) = common::unique_names("inception-full");

    // 1. Start outer VM with volumes for fcvm binary and assets
    println!("\n1. Starting outer VM with inception kernel...");
    println!("   Mounting: /mnt/fcvm-btrfs (assets) and fcvm binary");

    let kernel_str = inception_kernel
        .to_str()
        .context("kernel path not valid UTF-8")?;
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
        "--kernel",
        kernel_str,
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
            stdout,
            stderr
        );
    }
}

/// Run an inception chain test with configurable depth.
///
/// This function attempts to run VMs nested N levels deep:
/// Host → Level 1 → Level 2 → ... → Level N
///
/// LIMITATION (2025-12-29): Recursive nesting beyond L1 is NOT currently possible.
/// L1's KVM reports KVM_CAP_ARM_EL2=0 because:
/// - VHE mode is required for `kvm-arm.mode=nested`
/// - But NV2's E2H0 flag forces nVHE mode to avoid timer trap storms
/// - Without VHE, L1 cannot advertise nested virt capability
///
/// This test is kept for documentation and future testing when VHE+NV2 works.
///
/// REQUIRES: ARM64 with FEAT_NV2 (ARMv8.4+) and kvm-arm.mode=nested
async fn run_inception_chain(total_levels: usize) -> Result<()> {
    let success_marker = format!("INCEPTION_CHAIN_{}_LEVELS_SUCCESS", total_levels);

    println!(
        "\nInception Chain Test: {} levels of nested VMs",
        total_levels
    );
    println!("{}", "=".repeat(50));

    // Ensure prerequisites
    ensure_firecracker_nv2().await?;
    let inception_kernel = ensure_inception_kernel().await?;

    let fcvm_path = common::find_fcvm_binary()?;
    let fcvm_dir = fcvm_path.parent().unwrap();
    let kernel_str = inception_kernel
        .to_str()
        .context("kernel path not valid UTF-8")?;

    // Home dir for config mount
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let config_mount = format!("{0}/.config/fcvm:/root/.config/fcvm:ro", home);
    let fcvm_volume = format!("{}:/opt/fcvm", fcvm_dir.display());

    // Track PIDs for cleanup
    let mut level_pids: Vec<u32> = Vec::new();

    // Helper to cleanup all VMs (takes ownership to avoid lifetime issues)
    async fn cleanup_vms(pids: Vec<u32>) {
        for pid in pids.into_iter().rev() {
            common::kill_process(pid).await;
        }
    }

    // === Level 1: Start from host ===
    println!("\n[Level 1] Starting outer VM from host...");
    let (vm_name_1, _, _, _) = common::unique_names("inception-L1");

    let (mut _child1, pid1) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name_1,
        "--network",
        "bridged",
        "--kernel",
        kernel_str,
        "--privileged",
        "--map",
        "/mnt/fcvm-btrfs:/mnt/fcvm-btrfs",
        "--map",
        &fcvm_volume,
        "--map",
        &config_mount,
        common::TEST_IMAGE,
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
    println!("\n[Level 1] Checking if nested KVM works...");
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &pid1.to_string(),
            "--vm",
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

    // Start from innermost level and work outward
    let mut nested_cmd = format!("echo {}", success_marker);

    // Build the nested inception chain from inside out (Level N -> ... -> Level 2)
    for level in (2..=total_levels).rev() {
        let vm_name = format!("inception-L{}-{}", level, std::process::id());

        // Use alpine for all levels to speed up boot
        let image = "alpine:latest";

        // Escape the inner command for shell embedding
        let escaped_cmd = nested_cmd.replace('\'', "'\\''");

        nested_cmd = format!(
            r#"export PATH=/opt/fcvm:/mnt/fcvm-btrfs/bin:$PATH
export HOME=/root
modprobe tun 2>/dev/null || true
mkdir -p /dev/net
mknod /dev/net/tun c 10 200 2>/dev/null || true
chmod 666 /dev/net/tun 2>/dev/null || true
cd /mnt/fcvm-btrfs
echo "[L{level}] Starting nested VM..."
fcvm podman run \
    --name {vm_name} \
    --network bridged \
    --kernel {kernel} \
    --privileged \
    --map /mnt/fcvm-btrfs:/mnt/fcvm-btrfs \
    --map /opt/fcvm:/opt/fcvm \
    --map /root/.config/fcvm:/root/.config/fcvm:ro \
    --cmd '{escaped_cmd}' \
    {image}"#,
            level = level,
            vm_name = vm_name,
            kernel = kernel_str,
            escaped_cmd = escaped_cmd,
            image = image
        );
    }

    println!(
        "\n[Levels 2-{}] Starting nested inception chain from Level 1...",
        total_levels
    );
    println!(
        "  This will boot {} VMs sequentially",
        total_levels - 1
    );

    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &pid1.to_string(),
            "--vm",
            "--",
            "sh",
            "-c",
            &nested_cmd,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("running nested inception chain")?;

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

    if combined.contains(&success_marker) {
        println!("\n✅ INCEPTION CHAIN TEST PASSED!");
        println!("   Successfully ran {} levels of nested VMs", total_levels);
        Ok(())
    } else {
        bail!(
            "Inception chain failed at {} levels\n\
             Expected marker: {}\n\
             stdout (last 1000 chars): {}\n\
             stderr (last 1000 chars): {}",
            total_levels,
            success_marker,
            stdout.chars().rev().take(1000).collect::<String>().chars().rev().collect::<String>(),
            stderr.chars().rev().take(1000).collect::<String>().chars().rev().collect::<String>()
        )
    }
}

/// Test 4 levels of nested VMs (inception chain)
///
/// BLOCKED: Recursive nesting not possible - L1's KVM_CAP_ARM_EL2=0.
/// See module docs for root cause analysis. Keeping for future testing.
#[tokio::test]
#[ignore]
async fn test_inception_chain_4_levels() -> Result<()> {
    run_inception_chain(4).await
}

/// Test 32 levels of nested VMs (deep inception chain)
///
/// BLOCKED: Recursive nesting not possible - L1's KVM_CAP_ARM_EL2=0.
#[tokio::test]
#[ignore]
async fn test_inception_chain_32_levels() -> Result<()> {
    run_inception_chain(32).await
}

/// Test 64 levels of nested VMs (extreme inception chain)
///
/// BLOCKED: Recursive nesting not possible - L1's KVM_CAP_ARM_EL2=0.
#[tokio::test]
#[ignore]
async fn test_inception_chain_64_levels() -> Result<()> {
    run_inception_chain(64).await
}
