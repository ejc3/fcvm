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
const INCEPTION_KERNEL: &str = "/mnt/fcvm-btrfs/kernels/vmlinux-6.12.10-785344093fa0.bin";

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
