//! Integration test for inception support - verifies /dev/kvm works in guest
//!
//! This test generates a custom rootfs-config.toml pointing to the inception
//! kernel (with CONFIG_KVM=y), then verifies /dev/kvm works in the VM.
//!
//! FAILS LOUDLY if /dev/kvm is not available.

#![cfg(feature = "privileged-tests")]

mod common;

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Stdio;

/// Path to the inception kernel with CONFIG_KVM=y
/// Built by kernel/build.sh
const INCEPTION_KERNEL: &str = "/mnt/fcvm-btrfs/kernels/vmlinux-6.12.10-73d51d811398.bin";

/// Generate a custom rootfs-config.toml pointing to the inception kernel
fn generate_inception_config() -> Result<std::path::PathBuf> {
    let config_dir = std::path::PathBuf::from("/tmp/fcvm-inception-test");
    std::fs::create_dir_all(&config_dir)?;

    let config_path = config_dir.join("rootfs-config.toml");

    // Read the default config and modify the kernel section
    let config_content = format!(r#"# Inception test config - points to KVM-enabled kernel

[paths]
data_dir = "/mnt/fcvm-btrfs"
assets_dir = "/mnt/fcvm-btrfs"

[base]
version = "24.04"
codename = "noble"

[base.arm64]
url = "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-arm64.img"

[base.amd64]
url = "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img"

[kernel]
# Inception kernel with CONFIG_KVM=y - local file, not URL
# The kernel was built by kernel/build.sh

[kernel.arm64]
# Local kernel path - fcvm will use this directly
path = "{}"

[kernel.amd64]
path = "{}"

[packages]
runtime = ["podman", "crun", "fuse-overlayfs", "skopeo"]
fuse = ["fuse3"]
system = ["haveged", "chrony"]
debug = ["strace"]

[services]
enable = ["haveged", "chrony", "systemd-networkd"]
disable = ["multipathd", "snapd", "cloud-init", "cloud-config", "cloud-final"]

[files."/etc/resolv.conf"]
content = """
nameserver 127.0.0.53
"""

[files."/etc/chrony/chrony.conf"]
content = """
pool pool.ntp.org iburst
makestep 1.0 3
driftfile /var/lib/chrony/drift
"""

[files."/etc/systemd/network/10-eth0.network"]
content = """
[Match]
Name=eth0

[Network]
KeepConfiguration=yes
"""

[files."/etc/systemd/network/10-eth0.network.d/mmds.conf"]
content = """
[Route]
Destination=169.254.169.254/32
Scope=link
"""

[fstab]
remove_patterns = ["LABEL=BOOT", "LABEL=UEFI"]

[cleanup]
remove_dirs = ["/usr/share/doc/*", "/usr/share/man/*", "/var/cache/apt/archives/*"]
"#, INCEPTION_KERNEL, INCEPTION_KERNEL);

    std::fs::write(&config_path, config_content)?;
    Ok(config_path)
}

#[tokio::test]
async fn test_kvm_available_in_vm() -> Result<()> {
    println!("\nInception KVM test");
    println!("==================");
    println!("Verifying /dev/kvm works with inception kernel");

    // Check if inception kernel exists
    let kernel_path = Path::new(INCEPTION_KERNEL);
    if !kernel_path.exists() {
        bail!(
            "Inception kernel not found: {}\n\
            Build it with: ./kernel/build.sh\n\
            Or run: make inception-kernel",
            INCEPTION_KERNEL
        );
    }
    println!("✓ Inception kernel found: {}", INCEPTION_KERNEL);

    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names("inception-kvm");

    // Start the VM with custom kernel via --kernel flag
    // Use --privileged so the container can access /dev/kvm
    println!("\nStarting VM with inception kernel (privileged mode)...");
    let (mut _child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "--kernel",
        INCEPTION_KERNEL,
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

    // Check inception kernel exists
    let kernel_path = Path::new(INCEPTION_KERNEL);
    if !kernel_path.exists() {
        bail!(
            "Inception kernel not found: {}\n\
            Build it with: ./kernel/build.sh",
            INCEPTION_KERNEL
        );
    }

    let fcvm_path = common::find_fcvm_binary()?;
    let fcvm_dir = fcvm_path.parent().unwrap();
    let (vm_name, _, _, _) = common::unique_names("inception-full");

    // 1. Start outer VM with volumes for fcvm binary and assets
    println!("\n1. Starting outer VM with inception kernel...");
    println!("   Mounting: /mnt/fcvm-btrfs (assets) and fcvm binary");

    let (mut _child, outer_pid) = common::spawn_fcvm(&[
        "podman", "run",
        "--name", &vm_name,
        "--network", "bridged",
        "--kernel", INCEPTION_KERNEL,
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
