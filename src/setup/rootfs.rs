use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{info, warn};

/// Ensure rootfs exists, creating minimal Alpine + Podman if needed
pub async fn ensure_rootfs() -> Result<PathBuf> {
    let rootfs_dir = PathBuf::from("/var/lib/fcvm/rootfs");
    let rootfs_path = rootfs_dir.join("base.ext4");
    
    if rootfs_path.exists() {
        info!(path = %rootfs_path.display(), "rootfs already exists");
        return Ok(rootfs_path);
    }
    
    println!("⚙️  Creating base rootfs (first run, ~60s)...");
    
    // Create directory
    tokio::fs::create_dir_all(&rootfs_dir).await
        .context("creating rootfs directory")?;
    
    // Create rootfs
    create_alpine_rootfs(&rootfs_path).await
        .context("creating Alpine rootfs")?;
    
    println!("  ✓ Rootfs ready");
    
    Ok(rootfs_path)
}

/// Create minimal Alpine Linux rootfs with Podman
async fn create_alpine_rootfs(output_path: &Path) -> Result<()> {
    let temp_dir = PathBuf::from("/tmp/fcvm-rootfs-build");
    let mount_point = temp_dir.join("mnt");
    
    // Cleanup any previous failed attempts
    let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    
    tokio::fs::create_dir_all(&mount_point).await
        .context("creating temp directories")?;
    
    info!("creating 1GB ext4 image");
    println!("  → Creating 1GB ext4 image...");
    
    // Create 1GB sparse file
    let output = Command::new("dd")
        .args(&["if=/dev/zero", &format!("of={}", output_path.display()), "bs=1M", "count=0", "seek=1024"])
        .output()
        .context("creating sparse file")?;
    
    if !output.status.success() {
        bail!("dd failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    
    // Format as ext4
    let output = Command::new("mkfs.ext4")
        .args(&["-F", output_path.to_str().unwrap()])
        .output()
        .context("formatting ext4")?;
    
    if !output.status.success() {
        bail!("mkfs.ext4 failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    
    info!("mounting image");
    
    // Mount the image (requires root)
    let output = Command::new("mount")
        .args(&["-o", "loop", output_path.to_str().unwrap(), mount_point.to_str().unwrap()])
        .output()
        .context("mounting image")?;
    
    if !output.status.success() {
        bail!("mount failed: {}. Are you running as root?", String::from_utf8_lossy(&output.stderr));
    }
    
    // Download and extract Alpine minirootfs
    let result = download_and_extract_alpine(&mount_point).await;
    
    // Always unmount even if setup failed
    let _ = Command::new("umount")
        .arg(mount_point.to_str().unwrap())
        .output();
    
    // Clean up temp directory
    let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    
    result?;
    
    Ok(())
}

/// Download Alpine minirootfs and install Podman
async fn download_and_extract_alpine(mount_point: &Path) -> Result<()> {
    let alpine_version = "3.19";
    let arch = std::env::consts::ARCH; // x86_64 or aarch64
    let url = format!(
        "https://dl-cdn.alpinelinux.org/alpine/v{}/releases/{}/alpine-minirootfs-{}.0-{}.tar.gz",
        alpine_version, arch, alpine_version, arch
    );
    
    info!(url = %url, "downloading Alpine minirootfs");
    println!("  → Downloading Alpine {}...", alpine_version);
    
    let tarball_path = "/tmp/alpine-minirootfs.tar.gz";
    
    // Download
    let output = Command::new("curl")
        .args(&["-L", "-o", tarball_path, &url])
        .output()
        .context("downloading Alpine")?;
    
    if !output.status.success() {
        bail!("curl failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    
    info!("extracting Alpine rootfs");
    println!("  → Extracting...");
    
    // Extract to mount point
    let output = Command::new("tar")
        .args(&["-xzf", tarball_path, "-C", mount_point.to_str().unwrap()])
        .output()
        .context("extracting tarball")?;
    
    if !output.status.success() {
        bail!("tar failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    
    // Clean up tarball
    let _ = tokio::fs::remove_file(tarball_path).await;
    
    info!("installing Podman and dependencies");
    println!("  → Installing Podman (this may take 30s)...");
    
    // Setup DNS for chroot
    let resolv_conf = mount_point.join("etc/resolv.conf");
    tokio::fs::write(&resolv_conf, "nameserver 8.8.8.8\n").await
        .context("writing resolv.conf")?;
    
    // Install Podman via chroot + apk
    // Note: Alpine uses OpenRC, not systemd
    let output = Command::new("chroot")
        .arg(mount_point.to_str().unwrap())
        .args(&["/bin/sh", "-c", "apk update && apk add podman crun fuse-overlayfs openrc"])
        .output()
        .context("installing packages via apk")?;

    if !output.status.success() {
        warn!("apk install had issues: {}", String::from_utf8_lossy(&output.stderr));
        // Continue anyway - Podman might have partially installed
    }

    info!("setting up OpenRC");

    // Enable services for OpenRC
    let _ = Command::new("chroot")
        .arg(mount_point.to_str().unwrap())
        .args(&["/bin/sh", "-c", "rc-update add devfs boot && rc-update add procfs boot && rc-update add sysfs boot"])
        .output();
    
    // TODO: Copy fc-agent binary into /usr/local/bin/fc-agent
    // TODO: Create systemd service for fc-agent
    
    Ok(())
}
