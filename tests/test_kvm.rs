//! Integration test for inception support - verifies /dev/kvm works in guest
//!
//! This test generates a custom rootfs-config.toml pointing to the inception
//! kernel (with CONFIG_KVM=y), then verifies /dev/kvm works in the VM.
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
/// 3. Runs fcvm inside the outer VM to create an inner VM
/// 4. Verifies the inner VM runs successfully
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
    let (mut _child, outer_pid) = common::spawn_fcvm(&[
        "podman", "run",
        "--name", &vm_name,
        "--network", "bridged",
        "--kernel", kernel_str,
        "--privileged",
        "--volume", "/mnt/fcvm-btrfs:/mnt/fcvm-btrfs",
        "--volume", &format!("{}:/opt/fcvm", fcvm_dir.display()),
        "alpine:latest", "sleep", "300",
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

    // 3. Run fcvm inside the outer VM
    println!("\n3. Running fcvm inside outer VM (INCEPTION)...");
    println!("   This will create a nested VM inside the outer VM");

    // Run fcvm with rootless networking (simpler, no iptables needed)
    // Use --setup to auto-create any missing assets
    let inner_cmd = r#"
        export PATH=/opt/fcvm:$PATH
        cd /mnt/fcvm-btrfs
        fcvm podman run \
            --name inner-test \
            --network rootless \
            alpine:latest \
            echo 'INCEPTION_SUCCESS_INNER_VM_WORKS'
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

    // 4. Cleanup
    println!("\n4. Cleaning up outer VM...");
    common::kill_process(outer_pid).await;

    // 5. Verify success
    if stdout.contains("INCEPTION_SUCCESS_INNER_VM_WORKS") {
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
