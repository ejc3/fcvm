use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tracing::info;

/// Build a modern Linux LTS kernel optimized for Firecracker
///
/// This builds Linux 6.6 LTS with:
/// - virtio-MMIO support (Firecracker requirement)
/// - cgroup v2 support (for Podman/Docker containers)
/// - Minimal config for fast boot times
pub async fn build_firecracker_kernel() -> Result<PathBuf> {
    let kernel_dir = PathBuf::from("/var/lib/fcvm/kernels");
    let kernel_path = kernel_dir.join("vmlinux.bin");

    println!("⚙️  Building modern Linux 6.6 LTS kernel for Firecracker");
    println!("   This will take ~10 minutes on first run...");

    // Create kernel directory
    tokio::fs::create_dir_all(&kernel_dir).await
        .context("creating kernel directory")?;

    // Create temporary build directory
    let build_dir = PathBuf::from("/tmp/fcvm-kernel-build");
    tokio::fs::create_dir_all(&build_dir).await
        .context("creating build directory")?;

    let linux_version = "6.6.58";
    let linux_tar = format!("linux-{}.tar.xz", linux_version);
    let linux_dir = build_dir.join(format!("linux-{}", linux_version));

    // Download Linux kernel source
    if !linux_dir.exists() {
        info!("downloading Linux {} source", linux_version);
        println!("  → Downloading Linux {} source (~130MB)...", linux_version);

        let url = format!("https://cdn.kernel.org/pub/linux/kernel/v6.x/{}", linux_tar);
        let tar_path = build_dir.join(&linux_tar);

        let output = Command::new("wget")
            .args(&["-q", "-O", tar_path.to_str().unwrap(), &url])
            .output()
            .await
            .context("downloading kernel")?;

        if !output.status.success() {
            anyhow::bail!("failed to download kernel: {}", String::from_utf8_lossy(&output.stderr));
        }

        // Extract
        info!("extracting kernel source");
        println!("  → Extracting...");

        let output = Command::new("tar")
            .args(&["-xf", tar_path.to_str().unwrap()])
            .current_dir(&build_dir)
            .output()
            .await
            .context("extracting kernel")?;

        if !output.status.success() {
            anyhow::bail!("failed to extract kernel: {}", String::from_utf8_lossy(&output.stderr));
        }
    }

    // Start with defconfig
    info!("creating kernel config");
    println!("  → Creating minimal Firecracker config...");

    let output = Command::new("make")
        .args(&["ARCH=arm64", "defconfig"])
        .current_dir(&linux_dir)
        .output()
        .await
        .context("running defconfig")?;

    if !output.status.success() {
        anyhow::bail!("defconfig failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    // Enable Firecracker-specific features
    let features = vec![
        // Virtio (Firecracker requirement)
        "VIRTIO",
        "VIRTIO_MMIO",
        "VIRTIO_BLK",
        "VIRTIO_NET",
        "VIRTIO_CONSOLE",
        "HW_RANDOM_VIRTIO",
        // Serial console
        "SERIAL_8250",
        "SERIAL_8250_CONSOLE",
        // Basic kernel features
        "TTY",
        "DEVTMPFS",
        "DEVTMPFS_MOUNT",
        // Filesystems
        "EXT4_FS",
        "OVERLAY_FS",
        // cgroups (for containers!)
        "CGROUPS",
        "CGROUP_FREEZER",
        "CGROUP_PIDS",
        "CGROUP_DEVICE",
        "CGROUP_CPUACCT",
        "CGROUP_SCHED",
        "CGROUP_BPF",
        "MEMCG",
        "BLK_CGROUP",
        // Networking
        "NET",
        "INET",
        "IP_ADVANCED_ROUTER",
        "IP_MULTIPLE_TABLES",
        "NETFILTER",
        "NF_CONNTRACK",
        "NETFILTER_XT_MATCH_CONNTRACK",
    ];

    for feature in &features {
        let output = Command::new(&linux_dir.join("scripts/config"))
            .args(&["--enable", feature])
            .current_dir(&linux_dir)
            .output()
            .await
            .with_context(|| format!("enabling {}", feature))?;

        if !output.status.success() {
            anyhow::bail!("failed to enable {}: {}", feature, String::from_utf8_lossy(&output.stderr));
        }
    }

    // Build the kernel
    info!("building kernel (this may take 10-15 minutes)");
    println!("  → Building kernel with {} cores...", num_cpus::get());

    let output = Command::new("make")
        .args(&["ARCH=arm64", &format!("-j{}", num_cpus::get())])
        .current_dir(&linux_dir)
        .output()
        .await
        .context("building kernel")?;

    if !output.status.success() {
        anyhow::bail!("kernel build failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    // Copy the built kernel to fcvm kernels directory
    // For ARM64, Firecracker uses the PE Image at arch/arm64/boot/Image
    let kernel_image = linux_dir.join("arch/arm64/boot/Image");

    if !kernel_image.exists() {
        anyhow::bail!("kernel image not found at {}", kernel_image.display());
    }

    info!(src = %kernel_image.display(), dst = %kernel_path.display(), "copying kernel");
    tokio::fs::copy(&kernel_image, &kernel_path).await
        .context("copying kernel to fcvm directory")?;

    println!("  ✓ Kernel built successfully!");
    info!(path = %kernel_path.display(), "kernel ready");

    Ok(kernel_path)
}
