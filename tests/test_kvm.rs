//! Integration tests for inception support - nested VMs using ARM64 FEAT_NV2.
//!
//! # Nested Virtualization Status (2025-12-30)
//!
//! ## L1→L2 Working!
//! - Host runs L1 with inception kernel (6.18) and `--privileged --map /mnt/fcvm-btrfs`
//! - L1 runs fcvm inside container to start L2
//! - L2 executes commands successfully
//!
//! ## Key Components
//! - **Host kernel**: 6.18.2-nested with `kvm-arm.mode=nested`
//! - **Inception kernel**: 6.18 with `CONFIG_KVM=y`, FUSE_REMAP_FILE_RANGE support
//! - **Firecracker**: Fork with NV2 support (`--enable-nv2` flag)
//! - **Shared storage**: `/mnt/fcvm-btrfs` mounted via FUSE-over-vsock
//!
//! ## How L2 Works
//! 1. Host writes L1 script to shared storage (`/mnt/fcvm-btrfs/l1-inception.sh`)
//! 2. Host runs: `fcvm podman run --kernel {inception} --map /mnt/fcvm-btrfs --cmd /mnt/fcvm-btrfs/l1-inception.sh`
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
use nix::fcntl::{Flock, FlockArg};
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

const KERNEL_VERSION: &str = "6.18";
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

    // Acquire exclusive lock to prevent parallel builds
    let lock_file = PathBuf::from("/tmp/firecracker-nv2-build.lock");
    let lock_fd = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&lock_file)
        .context("opening firecracker build lock file")?;

    let flock = Flock::lock(lock_fd, FlockArg::LockExclusive)
        .map_err(|(_, err)| err)
        .context("acquiring exclusive lock for firecracker build")?;

    // Double-check after acquiring lock - another process may have built it
    if firecracker_has_nv2().await {
        println!("✓ Firecracker with NV2 support found (built by another process)");
        let _ = flock.unlock();
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
    let status = tokio::process::Command::new("cp")
        .args([binary.to_str().unwrap(), "/usr/local/bin/firecracker"])
        .status()
        .await
        .context("installing Firecracker")?;

    if !status.success() {
        bail!("Failed to install Firecracker");
    }

    // Verify installation
    if !firecracker_has_nv2().await {
        let _ = flock.unlock();
        bail!("Firecracker installed but --enable-nv2 flag not found");
    }

    let _ = flock.unlock();
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

/// Build localhost/inception-test image with proper CAS invalidation
///
/// Computes a combined SHA of ALL inputs (binaries, scripts, Containerfile).
/// Rebuilds and re-exports only when inputs change.
async fn ensure_inception_image() -> Result<()> {
    let fcvm_path = common::find_fcvm_binary()?;
    let fcvm_dir = fcvm_path.parent().unwrap();

    // All inputs that affect the container image
    let src_fcvm = fcvm_dir.join("fcvm");
    let src_agent = fcvm_dir.join("fc-agent");
    // NV2 firecracker fork is REQUIRED for inception tests - no fallbacks
    let src_firecracker =
        PathBuf::from("/home/ubuntu/firecracker/build/cargo_target/release/firecracker");
    if !src_firecracker.exists() {
        bail!(
            "NV2 firecracker fork not found at {}. Run 'make build-firecracker-nv2' first.",
            src_firecracker.display()
        );
    }
    let src_inception = PathBuf::from("inception.sh");
    let src_containerfile = PathBuf::from("Containerfile.inception");

    // Compute combined SHA of all inputs
    fn file_bytes(path: &Path) -> Vec<u8> {
        std::fs::read(path).unwrap_or_default()
    }

    let mut hasher = Sha256::new();
    hasher.update(file_bytes(&src_fcvm));
    hasher.update(file_bytes(&src_agent));
    hasher.update(file_bytes(&src_firecracker));
    hasher.update(file_bytes(&src_inception));
    hasher.update(file_bytes(&src_containerfile));
    let combined_sha = hex::encode(&hasher.finalize()[..6]);

    // Check if we have a marker file with the current SHA
    let marker_path = PathBuf::from("artifacts/.inception-sha");
    let cached_sha = std::fs::read_to_string(&marker_path).unwrap_or_default();

    let need_rebuild = cached_sha.trim() != combined_sha;

    if need_rebuild {
        println!(
            "Inputs changed (sha: {} → {}), rebuilding inception container...",
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
        std::fs::copy(&src_firecracker, "artifacts/firecracker-nv2").ok();

        // Force rebuild by removing old image
        tokio::process::Command::new("podman")
            .args(["rmi", "localhost/inception-test"])
            .output()
            .await
            .ok();
    }

    // Check if image exists
    let check = tokio::process::Command::new("podman")
        .args(["image", "exists", "localhost/inception-test"])
        .output()
        .await?;

    if check.status.success() && !need_rebuild {
        println!(
            "✓ localhost/inception-test up to date (sha: {})",
            combined_sha
        );
        return Ok(());
    }

    // Build container
    println!("Building localhost/inception-test...");
    let output = tokio::process::Command::new("podman")
        .args([
            "build",
            "-t",
            "localhost/inception-test",
            "-f",
            "Containerfile.inception",
            ".",
        ])
        .output()
        .await
        .context("running podman build")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to build inception container: {}", stderr);
    }

    // Export to CAS cache so nested VMs can access it
    let digest_out = tokio::process::Command::new("podman")
        .args([
            "inspect",
            "localhost/inception-test",
            "--format",
            "{{.Digest}}",
        ])
        .output()
        .await?;
    let digest = String::from_utf8_lossy(&digest_out.stdout)
        .trim()
        .to_string();

    if !digest.is_empty() && digest.starts_with("sha256:") {
        let cache_dir = format!("/mnt/fcvm-btrfs/image-cache/{}", digest);

        if !PathBuf::from(&cache_dir).exists() {
            println!("Exporting to CAS cache: {}", cache_dir);
            tokio::process::Command::new("mkdir")
                .args(["-p", &cache_dir])
                .output()
                .await?;
            let skopeo_out = tokio::process::Command::new("skopeo")
                .args([
                    "copy",
                    "containers-storage:localhost/inception-test",
                    &format!("dir:{}", cache_dir),
                ])
                .output()
                .await?;
            if !skopeo_out.status.success() {
                println!(
                    "Warning: skopeo export failed: {}",
                    String::from_utf8_lossy(&skopeo_out.stderr)
                );
            }
        }

        // Save the combined SHA as marker
        std::fs::write(&marker_path, &combined_sha).ok();

        println!(
            "✓ localhost/inception-test ready (sha: {}, digest: {})",
            combined_sha,
            &digest[..std::cmp::min(19, digest.len())]
        );
    } else {
        println!("✓ localhost/inception-test built (no digest available)");
    }

    Ok(())
}

/// Run an inception chain test with configurable depth.
///
/// This function attempts to run VMs nested N levels deep:
/// Host → Level 1 → Level 2 → ... → Level N
///
/// Each nested level uses localhost/inception-test which has fcvm baked in.
///
/// REQUIRES: ARM64 with FEAT_NV2 (ARMv8.4+) and kvm-arm.mode=nested
#[allow(dead_code)] // Helper for future L3+ tests (currently L3 is too slow)
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
    ensure_inception_image().await?;

    let fcvm_path = common::find_fcvm_binary()?;
    let kernel_str = inception_kernel
        .to_str()
        .context("kernel path not valid UTF-8")?;

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

    // === Level 1: Start from host with localhost/inception-test ===
    // This image has fcvm baked in, fcvm handles export to cache automatically
    println!("\n[Level 1] Starting outer VM from host...");
    let (vm_name_1, _, _, _) = common::unique_names("inception-L1");

    // L1 uses 4GB RAM (needs to fit L2-L4 inside + overhead)
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
        "--mem",
        "4096", // L1 gets 4GB, nested VMs get progressively less
        "--map",
        "/mnt/fcvm-btrfs:/mnt/fcvm-btrfs",
        "--map",
        &config_mount,
        "localhost/inception-test",
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
    let nested_image = "localhost/inception-test";
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
    let image_cache_path = format!("/mnt/fcvm-btrfs/image-cache/{}", image_digest);
    println!("[Setup] Image digest: {}", image_digest);
    println!("[Setup] Cache path: {}", image_cache_path);

    // The inception script is baked into the container at /usr/local/bin/inception
    // It takes: inception <current_level> <max_level> <kernel_path> <image_cache_path>
    // Starting from level 2 (L1 is already running), going to total_levels
    let inception_cmd = format!(
        "inception 2 {} {} {}",
        total_levels, kernel_str, image_cache_path
    );

    println!(
        "\n[Levels 2-{}] Starting nested inception chain from Level 1...",
        total_levels
    );
    println!("  This will boot {} VMs sequentially", total_levels - 1);

    // Run in container (default, no --vm) because the inception script is in the container
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &pid1.to_string(),
            // Default is container exec (no --vm flag)
            "--",
            "sh",
            "-c",
            &inception_cmd,
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
            "Inception chain failed - exec command exited with status {:?}\n\
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

/// Test L1→L2 inception: run fcvm inside L1 to start L2
///
/// L1: Host starts VM with localhost/inception-test + inception kernel
/// L2: L1 container imports image from shared cache, then runs fcvm
#[tokio::test]
async fn test_inception_l2() -> Result<()> {
    ensure_inception_image().await?;
    let inception_kernel = ensure_inception_kernel().await?;
    let kernel_str = inception_kernel
        .to_str()
        .context("kernel path not valid UTF-8")?;

    // Get the digest of localhost/inception-test so L2 can import from shared cache
    let digest_out = tokio::process::Command::new("podman")
        .args([
            "inspect",
            "localhost/inception-test",
            "--format",
            "{{.Digest}}",
        ])
        .output()
        .await?;
    let digest = String::from_utf8_lossy(&digest_out.stdout)
        .trim()
        .to_string();
    println!("Image digest: {}", digest);

    // Create inception-scripts directory
    tokio::fs::create_dir_all("/mnt/fcvm-btrfs/inception-scripts").await?;

    // Benchmark script that runs at each level
    // Tests: egress, local disk, FUSE disk
    let bench_script = r#"#!/bin/bash
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
    READ_US=$(( (END - START) / 100000 ))
    echo "FUSE_SMALLREAD_L${LEVEL}=${READ_US}us/op (100 ops)"

    rm -rf "$FUSE_DIR"
fi

# Test 5: Memory usage (RSS)
echo "--- Memory Test ---"
MEM_TOTAL=$(grep MemTotal /proc/meminfo | awk '{print $2}')
MEM_AVAIL=$(grep MemAvailable /proc/meminfo | awk '{print $2}')
MEM_USED=$((MEM_TOTAL - MEM_AVAIL))
MEM_USED_MB=$((MEM_USED / 1024))
MEM_TOTAL_MB=$((MEM_TOTAL / 1024))
echo "MEM_L${LEVEL}=${MEM_USED_MB}MB/${MEM_TOTAL_MB}MB"

echo "=== END BENCHMARK L${LEVEL} ==="
echo "MARKER_L${LEVEL}_OK"
"#;

    let bench_path = "/mnt/fcvm-btrfs/inception-scripts/bench.sh";
    tokio::fs::write(bench_path, bench_script).await?;
    tokio::process::Command::new("chmod")
        .args(["+x", bench_path])
        .status()
        .await?;

    // L2 script: just run benchmark
    let l2_script = r#"#!/bin/bash
set -ex
/mnt/fcvm-btrfs/inception-scripts/bench.sh 2
"#;
    let l2_path = "/mnt/fcvm-btrfs/inception-scripts/l2.sh";
    tokio::fs::write(l2_path, l2_script).await?;
    tokio::process::Command::new("chmod")
        .args(["+x", l2_path])
        .status()
        .await?;

    // L1 script: run L1 benchmark, import image, start L2 with benchmark
    let l1_script = format!(
        r#"#!/bin/bash
set -ex

# Run L1 benchmark first
/mnt/fcvm-btrfs/inception-scripts/bench.sh 1

echo "L1: Importing image from shared cache..."
skopeo copy dir:/mnt/fcvm-btrfs/image-cache/{digest} containers-storage:localhost/inception-test

echo "L1: Starting L2 VM with benchmarks (tracing enabled)..."
FCVM_FUSE_TRACE_RATE=100 fcvm podman run --name l2 --network bridged --privileged \
    --map /mnt/fcvm-btrfs:/mnt/fcvm-btrfs \
    localhost/inception-test \
    --cmd /mnt/fcvm-btrfs/inception-scripts/l2.sh
"#,
        digest = digest
    );

    let l1_path = "/mnt/fcvm-btrfs/inception-scripts/l1.sh";
    tokio::fs::write(l1_path, &l1_script).await?;
    tokio::process::Command::new("chmod")
        .args(["+x", l1_path])
        .status()
        .await?;
    println!("Wrote inception scripts to /mnt/fcvm-btrfs/inception-scripts/");

    // Run L1 with --cmd that executes the script
    let output = tokio::process::Command::new("./target/release/fcvm")
        .args([
            "podman",
            "run",
            "--name",
            "l1-inception",
            "--network",
            "bridged",
            "--privileged",
            "--mem",
            "4096",
            "--kernel",
            kernel_str,
            "--map",
            "/mnt/fcvm-btrfs:/mnt/fcvm-btrfs",
            "localhost/inception-test",
            "--cmd",
            "/mnt/fcvm-btrfs/inception-scripts/l1.sh",
        ])
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("stdout: {}", stdout);
    println!("stderr: {}", stderr);

    // Check for L1 and L2 markers
    assert!(
        stderr.contains("MARKER_L1_OK"),
        "L1 benchmark should complete. Check stderr above."
    );
    assert!(
        stderr.contains("MARKER_L2_OK"),
        "L2 benchmark should complete. Check stderr above."
    );

    // Extract and display benchmark results
    println!("\n=== BENCHMARK SUMMARY ===");
    for line in stderr.lines() {
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

    Ok(())
}

/// Test L1→L2→L3 inception: 3 levels of nesting
///
/// BLOCKED: 3-hop FUSE chain (L3→L2→L1→HOST) causes ~3-5 second latency per
/// request due to PassthroughFs + spawn_blocking serialization. FUSE mount
/// initialization alone takes 10+ minutes. Need to implement request pipelining
/// or async PassthroughFs before this test can complete in reasonable time.
#[tokio::test]
#[ignore]
async fn test_inception_l3() -> Result<()> {
    run_inception_n_levels(3, "MARKER_L3_OK_12345").await
}

/// Test L1→L2→L3→L4 inception: 4 levels of nesting
///
/// BLOCKED: Same issue as L3, but worse. 4-hop FUSE chain would be even slower.
#[tokio::test]
#[ignore]
async fn test_inception_l4() -> Result<()> {
    run_inception_n_levels(4, "MARKER_L4_OK_12345").await
}

/// Run N levels of inception, building scripts from deepest level upward
async fn run_inception_n_levels(n: usize, marker: &str) -> Result<()> {
    assert!(n >= 2, "Need at least 2 levels for inception");

    ensure_inception_image().await?;
    let inception_kernel = ensure_inception_kernel().await?;
    let kernel_str = inception_kernel
        .to_str()
        .context("kernel path not valid UTF-8")?;

    // Get the digest of localhost/inception-test
    let digest_out = tokio::process::Command::new("podman")
        .args([
            "inspect",
            "localhost/inception-test",
            "--format",
            "{{.Digest}}",
        ])
        .output()
        .await?;
    let digest = String::from_utf8_lossy(&digest_out.stdout)
        .trim()
        .to_string();
    println!("Image digest: {}", digest);

    // Memory allocation strategy:
    // - Each VM needs enough memory to run its child's Firecracker (~2GB) + OS overhead (~500MB)
    // - Intermediate levels (L1..L(n-1)): 4GB each to accommodate child VM + OS
    // - Deepest level (Ln): 2GB (default) since it just runs echo
    let intermediate_mem = "4096"; // 4GB for VMs that spawn children

    // Build scripts from deepest level (Ln) upward to L1
    // Ln (deepest): just echo the marker
    // L1..L(n-1): import image + run fcvm with next level's script

    let scripts_dir = "/mnt/fcvm-btrfs/inception-scripts";
    tokio::fs::create_dir_all(scripts_dir).await.ok();

    // Deepest level (Ln): just echo the marker
    let ln_script = format!("#!/bin/bash\necho {}\n", marker);
    let ln_path = format!("{}/l{}.sh", scripts_dir, n);
    tokio::fs::write(&ln_path, &ln_script).await?;
    tokio::process::Command::new("chmod")
        .args(["+x", &ln_path])
        .status()
        .await?;
    println!("L{}: echo marker", n);

    // Build L(n-1) down to L1: each imports image and runs fcvm with next script
    // Each level needs:
    // - --map to access shared storage
    // - --mem for intermediate levels to fit child VM
    // - --kernel for intermediate levels that spawn VMs (need KVM)
    //
    // The inception kernel path is accessible via the shared FUSE mount.
    let inception_kernel_path = kernel_str; // Same kernel used at all levels

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
        // ALL levels need --kernel because they all spawn VMs with Firecracker
        let kernel_arg = format!("--kernel {}", inception_kernel_path);

        let script = format!(
            r#"#!/bin/bash
set -ex
echo "L{}: Importing image from shared cache..."
skopeo copy dir:/mnt/fcvm-btrfs/image-cache/{} containers-storage:localhost/inception-test
echo "L{}: Starting L{} VM..."
fcvm podman run --name l{} --network bridged --privileged {} {} --map /mnt/fcvm-btrfs:/mnt/fcvm-btrfs localhost/inception-test --cmd {}
"#,
            level,
            digest,
            level,
            level + 1,
            level + 1,
            mem_arg,
            kernel_arg,
            next_script
        );
        let script_path = format!("{}/l{}.sh", scripts_dir, level);
        tokio::fs::write(&script_path, &script).await?;
        tokio::process::Command::new("chmod")
            .args(["+x", &script_path])
            .status()
            .await?;
        println!(
            "L{}: import + fcvm {} {} --map + --cmd {}",
            level, mem_arg, kernel_arg, next_script
        );
    }

    // Run L1 from host with inception kernel
    // L1 needs extra memory since it spawns L2
    let l1_script = format!("{}/l1.sh", scripts_dir);
    println!(
        "\nStarting {} levels of inception with 4GB per intermediate VM...",
        n
    );

    // Use sh -c with tee to stream output in real-time AND capture for marker check
    let log_file = format!("/tmp/inception-l{}.log", n);
    let fcvm_cmd = format!(
        "sudo ./target/release/fcvm podman run \
         --name l1-inception-{} \
         --network bridged \
         --privileged \
         --mem {} \
         --kernel {} \
         --map /mnt/fcvm-btrfs:/mnt/fcvm-btrfs \
         localhost/inception-test \
         --cmd {} 2>&1 | tee {}",
        n, intermediate_mem, kernel_str, l1_script, log_file
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

    // Look for the marker in output
    assert!(
        log_content.contains(marker),
        "L{} VM should echo marker '{}'. Exit status: {:?}. Check output above.",
        n,
        marker,
        status
    );
    Ok(())
}

/// Test skopeo import performance over FUSE (localhost/inception-test)
///
/// Measures how long it takes to import the full inception container image
/// inside a VM when the image layers are accessed over FUSE-over-vsock.
#[tokio::test]
async fn test_skopeo_import_over_fuse() -> Result<()> {
    println!("\nSkopeo Over FUSE Performance Test");
    println!("==================================\n");

    // 1. Ensure localhost/inception-test exists
    println!("1. Ensuring localhost/inception-test exists...");
    ensure_inception_image().await?;
    println!("   ✓ Image ready");

    // 2. Get image digest and export to CAS cache
    println!("2. Getting image digest...");
    let digest_output = tokio::process::Command::new("podman")
        .args([
            "inspect",
            "localhost/inception-test",
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

    let cache_dir = format!("/mnt/fcvm-btrfs/image-cache/{}", digest);
    println!("   Digest: {}", &digest[..19]);

    // Get image size
    let size_output = tokio::process::Command::new("podman")
        .args([
            "images",
            "localhost/inception-test",
            "--format",
            "{{.Size}}",
        ])
        .output()
        .await?;
    let size = String::from_utf8_lossy(&size_output.stdout)
        .trim()
        .to_string();
    println!("   Size: {}", size);

    // Check if already in CAS cache
    if !std::path::Path::new(&cache_dir).exists() {
        println!("   Exporting to CAS cache...");
        tokio::process::Command::new("mkdir")
            .args(["-p", &cache_dir])
            .output()
            .await?;

        let export_output = tokio::process::Command::new("skopeo")
            .args([
                "copy",
                "containers-storage:localhost/inception-test",
                &format!("dir:{}", cache_dir),
            ])
            .output()
            .await?;

        if !export_output.status.success() {
            let stderr = String::from_utf8_lossy(&export_output.stderr);
            bail!("Failed to export to CAS: {}", stderr);
        }
        println!("   ✓ Exported to CAS cache");
    } else {
        println!("   ✓ Already in CAS cache");
    }

    // 3. Start L1 VM with FUSE mount
    println!("3. Starting L1 VM with FUSE mount...");

    let inception_kernel = ensure_inception_kernel().await?;
    let kernel_str = inception_kernel
        .to_str()
        .context("kernel path not valid UTF-8")?;

    let (vm_name, _, _, _) = common::unique_names("fuse-large");
    let fcvm_path = common::find_fcvm_binary()?;

    let (mut _child, vm_pid) = common::spawn_fcvm(&[
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

    // 4. Time the skopeo import inside the VM
    println!("\n4. Timing skopeo import inside VM...");
    println!("   Source: {}", cache_dir);
    println!("   Image size: {}", size);

    let start = std::time::Instant::now();

    let import_cmd = format!(
        "time skopeo copy dir:{} containers-storage:localhost/imported 2>&1",
        cache_dir
    );

    let import_output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &vm_pid.to_string(),
            "--vm",
            "--",
            "sh",
            "-c",
            &import_cmd,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    let elapsed = start.elapsed();

    let stdout = String::from_utf8_lossy(&import_output.stdout);
    println!("\n   Output:\n   {}", stdout.trim().replace('\n', "\n   "));

    // 5. Verify the image was imported
    println!("\n5. Verifying image was imported...");
    let verify_output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &vm_pid.to_string(),
            "--vm",
            "--",
            "podman",
            "images",
            "localhost/imported",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    let verify_stdout = String::from_utf8_lossy(&verify_output.stdout);
    println!("   {}", verify_stdout.trim().replace('\n', "\n   "));

    if !verify_stdout.contains("localhost/imported") {
        common::kill_process(vm_pid).await;
        bail!("Image was not imported correctly");
    }
    println!("   ✓ Image imported!");

    // 6. Clean up
    println!("\n6. Cleaning up...");
    common::kill_process(vm_pid).await;

    // 7. Report results
    println!("\n==========================================");
    println!("RESULT: {} image import over FUSE took {:?}", size, elapsed);
    println!("==========================================");

    // Calculate throughput
    let size_mb: f64 = if size.contains("MB") {
        size.replace(" MB", "").parse().unwrap_or(0.0)
    } else if size.contains("GB") {
        size.replace(" GB", "").parse::<f64>().unwrap_or(0.0) * 1024.0
    } else {
        0.0
    };

    if size_mb > 0.0 && elapsed.as_secs_f64() > 0.0 {
        let throughput = size_mb / elapsed.as_secs_f64();
        println!("Throughput: {:.1} MB/s", throughput);
    }

    if elapsed.as_secs() > 300 {
        println!("\n⚠️  Import is VERY SLOW (>5min) - need optimization");
    } else if elapsed.as_secs() > 60 {
        println!("\n⚠️  Import is SLOW (>60s) - consider optimization");
    } else if elapsed.as_secs() > 10 {
        println!("\n⚠️  Import is MODERATE (10-60s)");
    } else {
        println!("\n✓ Import is FAST (<10s)");
    }

    Ok(())
}
