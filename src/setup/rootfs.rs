use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{info, warn};

use crate::paths;

/// Ensure rootfs exists, creating minimal Alpine + Podman if needed
pub async fn ensure_rootfs() -> Result<PathBuf> {
    let rootfs_dir = paths::rootfs_dir();
    let rootfs_path = paths::base_rootfs();

    // ALWAYS rebuild rootfs for now - ensures code changes take effect
    // TODO: Add version check or --force-rebuild flag for production
    if rootfs_path.exists() {
        info!(path = %rootfs_path.display(), "rootfs exists but rebuilding to ensure latest changes");
        tokio::fs::remove_file(&rootfs_path)
            .await
            .context("removing old rootfs")?;
    }

    println!("⚙️  Creating base rootfs (~60s)...");

    // Create directory
    tokio::fs::create_dir_all(&rootfs_dir)
        .await
        .context("creating rootfs directory")?;

    // Create rootfs
    create_alpine_rootfs(&rootfs_path)
        .await
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

    tokio::fs::create_dir_all(&mount_point)
        .await
        .context("creating temp directories")?;

    info!("creating 1GB ext4 image");
    println!("  → Creating 1GB ext4 image...");

    // Create 1GB sparse file
    let output = Command::new("dd")
        .args(&[
            "if=/dev/zero",
            &format!("of={}", output_path.display()),
            "bs=1M",
            "count=0",
            "seek=1024",
        ])
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
        bail!(
            "mkfs.ext4 failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    info!("mounting image");

    // Mount the image (requires root)
    let output = Command::new("mount")
        .args(&[
            "-o",
            "loop",
            output_path.to_str().unwrap(),
            mount_point.to_str().unwrap(),
        ])
        .output()
        .context("mounting image")?;

    if !output.status.success() {
        bail!(
            "mount failed: {}. Are you running as root?",
            String::from_utf8_lossy(&output.stderr)
        );
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

    // Setup DNS - use Google DNS (works once routing is fixed)
    let resolv_conf = mount_point.join("etc/resolv.conf");
    tokio::fs::write(&resolv_conf, "nameserver 8.8.8.8\n")
        .await
        .context("writing resolv.conf")?;

    // Install Podman + haveged + ca-certificates + chrony via chroot + apk
    // Note: Alpine uses OpenRC, not systemd
    // haveged provides entropy to fix CRNG init delays
    // ca-certificates needed for fc-agent's reqwest HTTPS requests
    // chrony provides NTP time synchronization (fixes TLS cert validation)
    // openresolv needed for dns-nameservers in /etc/network/interfaces
    let output = Command::new("chroot")
        .arg(mount_point.to_str().unwrap())
        .args(&["/bin/sh", "-c", "apk update && apk add podman crun fuse-overlayfs openrc haveged ca-certificates chrony openresolv"])
        .output()
        .context("installing packages via apk")?;

    if !output.status.success() {
        warn!(
            "apk install had issues: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        // Continue anyway - Podman might have partially installed
    }

    info!("setting up OpenRC");

    // Enable services for OpenRC
    // cgroups service is CRITICAL for Podman - mounts cgroup v1 hierarchy
    // chronyd service is CRITICAL for TLS - syncs system clock via NTP
    let _ = Command::new("chroot")
        .arg(mount_point.to_str().unwrap())
        .args(&["/bin/sh", "-c", "rc-update add devfs boot && rc-update add procfs boot && rc-update add sysfs boot && rc-update add cgroups boot && rc-update add haveged boot && rc-update add chronyd default"])
        .output();

    // Configure networking for Alpine Linux
    // Network configuration is passed via kernel cmdline (ip= parameter)
    // so we don't need static configuration in /etc/network/interfaces
    // Just configure loopback
    info!("configuring network interfaces");
    let interfaces_config = r#"auto lo
iface lo inet loopback

# eth0 configured via kernel cmdline (ip= parameter)
# No explicit configuration needed
"#;
    let interfaces_path = mount_point.join("etc/network/interfaces");
    tokio::fs::write(&interfaces_path, interfaces_config)
        .await
        .context("writing /etc/network/interfaces")?;

    // Enable serial console on ttyS0 (for Firecracker)
    info!("enabling serial console");
    let inittab_path = mount_point.join("etc/inittab");
    let inittab = tokio::fs::read_to_string(&inittab_path)
        .await
        .context("reading /etc/inittab")?;
    let inittab_fixed = inittab.replace(
        "#ttyS0::respawn:/sbin/getty -L ttyS0 115200 vt100",
        "ttyS0::respawn:/sbin/getty -L ttyS0 115200 vt100",
    );
    tokio::fs::write(&inittab_path, inittab_fixed)
        .await
        .context("writing /etc/inittab")?;

    // Configure chronyd for fast clock synchronization
    // makestep allows large clock jumps (critical when starting from 1970)
    // Use pool.ntp.org NTP servers
    info!("configuring chronyd for NTP time sync");
    let chrony_conf = r#"# NTP servers from pool.ntp.org
pool pool.ntp.org iburst

# Allow clock to be stepped (not slewed) for large time differences
# This is critical for VMs that start with clock at 1970
makestep 1.0 3

# Directory for drift and other runtime files
driftfile /var/lib/chrony/drift
"#;
    // Create /etc/chrony directory if it doesn't exist
    let chrony_dir = mount_point.join("etc/chrony");
    tokio::fs::create_dir_all(&chrony_dir)
        .await
        .context("creating /etc/chrony directory")?;

    let chrony_conf_path = chrony_dir.join("chrony.conf");
    tokio::fs::write(&chrony_conf_path, chrony_conf)
        .await
        .context("writing /etc/chrony/chrony.conf")?;

    // Install fc-agent binary and OpenRC service
    install_fc_agent(&mount_point).await?;

    // Install overlay-init script for OverlayFS support
    install_overlay_init(&mount_point).await?;

    // Add network debugging script that runs at boot
    info!("installing network debug script");
    let debug_script = r#"#!/bin/sh
# Network debugging - runs at boot and logs to /var/log/network-debug.log
exec > /var/log/network-debug.log 2>&1

echo "=== Network Debug at $(date) ==="
echo ""
echo "== Interface Status =="
ip addr
echo ""
echo "== Routing Table =="
ip route
echo ""
echo "== DNS Configuration =="
cat /etc/resolv.conf
echo ""
echo "== Ping Gateway (172.16.0.1) =="
ping -c 3 -W 2 172.16.0.1 || echo "FAILED to ping gateway"
echo ""
echo "== Test DNS to Gateway =="
nslookup registry-1.docker.io 172.16.0.1 || echo "FAILED DNS via gateway"
echo ""
echo "== Test DNS to 8.8.8.8 =="
nslookup registry-1.docker.io 8.8.8.8 || echo "FAILED DNS via 8.8.8.8"
echo ""
echo "== Ping Google DNS =="
ping -c 3 -W 2 8.8.8.8 || echo "FAILED to ping 8.8.8.8"
echo ""
echo "=== Debug Complete ==="
"#;
    let debug_script_path = mount_point.join("usr/local/bin/network-debug.sh");
    tokio::fs::write(&debug_script_path, debug_script)
        .await
        .context("writing network debug script")?;

    // Make executable
    let _ = Command::new("chmod")
        .args(&["+x", debug_script_path.to_str().unwrap()])
        .output();

    // Add to boot via local service
    let local_start = r#"#!/bin/sh
# Run network debugging
/usr/local/bin/network-debug.sh &
"#;
    let local_path = mount_point.join("etc/local.d/network-debug.start");
    tokio::fs::create_dir_all(mount_point.join("etc/local.d"))
        .await
        .context("creating local.d directory")?;
    tokio::fs::write(&local_path, local_start)
        .await
        .context("writing local.d script")?;
    let _ = Command::new("chmod")
        .args(&["+x", local_path.to_str().unwrap()])
        .output();

    // Enable local service
    let _ = Command::new("chroot")
        .arg(mount_point.to_str().unwrap())
        .args(&["/bin/sh", "-c", "rc-update add local default"])
        .output();

    Ok(())
}

/// Install fc-agent guest agent into rootfs
async fn install_fc_agent(mount_point: &Path) -> Result<()> {
    info!("installing fc-agent guest agent");

    // Find fc-agent binary - try multiple locations
    // IMPORTANT: Prefer musl target for Alpine Linux compatibility (statically linked)
    let possible_paths = vec![
        PathBuf::from(
            "/home/ubuntu/fcvm/fc-agent/target/aarch64-unknown-linux-musl/release/fc-agent",
        ), // musl (static)
        PathBuf::from("fc-agent/target/aarch64-unknown-linux-musl/release/fc-agent"), // musl relative
        PathBuf::from("/home/ubuntu/fcvm/fc-agent/target/release/fc-agent"), // gnu (fallback, won't work on Alpine)
        PathBuf::from("fc-agent/target/release/fc-agent"),                   // gnu relative
        PathBuf::from("../fc-agent/target/release/fc-agent"), // gnu relative from current dir
    ];

    let fc_agent_src = possible_paths.iter().find(|p| p.exists()).cloned();

    let fc_agent_src = match fc_agent_src {
        Some(path) => path,
        None => {
            bail!(
                "fc-agent binary not found in any of: {:?}\n\
                   Please build it first: cd fc-agent && cargo build --release",
                possible_paths
            );
        }
    };

    // Create /usr/local/bin directory if it doesn't exist
    let bin_dir = mount_point.join("usr/local/bin");
    tokio::fs::create_dir_all(&bin_dir)
        .await
        .context("creating /usr/local/bin directory")?;

    // Copy fc-agent to /usr/local/bin/fc-agent
    let fc_agent_dest = mount_point.join("usr/local/bin/fc-agent");
    tokio::fs::copy(&fc_agent_src, &fc_agent_dest)
        .await
        .context("copying fc-agent binary")?;

    // Make executable
    let output = Command::new("chmod")
        .args(&["+x", fc_agent_dest.to_str().unwrap()])
        .output()
        .context("making fc-agent executable")?;

    if !output.status.success() {
        bail!("chmod failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    // Create OpenRC service file for fc-agent
    let service_script = r#"#!/sbin/openrc-run

description="fcvm guest agent for container orchestration"

command="/usr/local/bin/fc-agent"
command_background=true
pidfile="/run/fc-agent.pid"
output_log="/dev/console"
error_log="/dev/console"

depend() {
    need net
    after podman
}
"#;

    let service_path = mount_point.join("etc/init.d/fc-agent");
    tokio::fs::write(&service_path, service_script)
        .await
        .context("writing fc-agent service")?;

    // Make service executable
    let output = Command::new("chmod")
        .args(&["+x", service_path.to_str().unwrap()])
        .output()
        .context("making service executable")?;

    if !output.status.success() {
        bail!("chmod failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    // Enable fc-agent service (add to default runlevel)
    let output = Command::new("chroot")
        .arg(mount_point.to_str().unwrap())
        .args(&["/bin/sh", "-c", "rc-update add fc-agent default"])
        .output()
        .context("enabling fc-agent service")?;

    if !output.status.success() {
        warn!(
            "Failed to enable fc-agent service: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    info!("fc-agent installed and enabled");
    println!("  ✓ fc-agent guest agent installed");

    Ok(())
}

/// Install overlay-init script for OverlayFS support
/// This custom init script mounts OverlayFS before starting the real init
async fn install_overlay_init(mount_point: &Path) -> Result<()> {
    info!("installing overlay-init script for OverlayFS");

    // Custom init script that sets up OverlayFS before pivoting to real root
    let overlay_init_script = r#"#!/bin/sh
# overlay-init: Custom init script for OverlayFS setup
# This runs as PID 1 before the real init system

# Don't exit on errors - we need to handle pre-mounted filesystems
set +e

# Mount essential filesystems (required as PID 1)
# Some may already be mounted by kernel - ignore errors
mount -t proc proc /proc 2>/dev/null || true
mount -t sysfs sysfs /sys 2>/dev/null || true
# CRITICAL: Don't mount fresh devtmpfs - it loses special device nodes!
# Instead, if /dev is not mounted, mount devtmpfs
if ! mountpoint -q /dev; then
    mount -t devtmpfs devtmpfs /dev
fi

# Re-enable exit on error for the rest of the script
set -e

# Parse overlay_root= parameter from kernel command line
OVERLAY_DEV=""
for arg in $(cat /proc/cmdline); do
    case "$arg" in
        overlay_root=*)
            OVERLAY_DEV="${arg#overlay_root=}"
            ;;
    esac
done

if [ -z "$OVERLAY_DEV" ]; then
    echo "ERROR: overlay_root= not specified in kernel command line"
    exec /bin/sh
fi

echo "overlay-init: Setting up OverlayFS with overlay device: $OVERLAY_DEV"

# Debug: Check what's in /dev before we start
echo "overlay-init: Checking /dev contents before pivot..."
ls -la /dev/net/ 2>/dev/null || echo "No /dev/net directory"

# Mount the writable overlay device
mount /dev/$OVERLAY_DEV /mnt

# Create directories for OverlayFS
mkdir -p /mnt/upper /mnt/work /mnt/mnt-overlay-root /mnt/mnt-newroot

# Bind-mount current root (vda, read-only) as lower layer
mount --bind / /mnt/mnt-overlay-root

# Create OverlayFS mount combining lower (vda) + upper (vdb)
mount -t overlay overlay \
    -o lowerdir=/mnt/mnt-overlay-root,upperdir=/mnt/upper,workdir=/mnt/work \
    /mnt/mnt-newroot

# Move necessary mounts into new root
mkdir -p /mnt/mnt-newroot/proc /mnt/mnt-newroot/sys /mnt/mnt-newroot/dev
mount --move /proc /mnt/mnt-newroot/proc
mount --move /sys /mnt/mnt-newroot/sys
mount --move /dev /mnt/mnt-newroot/dev

# Preserve runtime state (/run holds resolvconf + networking data)
mkdir -p /mnt/mnt-newroot/run
if mountpoint -q /run; then
    mount --move /run /mnt/mnt-newroot/run
else
    cp -a /run/. /mnt/mnt-newroot/run/ 2>/dev/null || true
fi

# Ensure /var/run continues to resolve to /run for OpenRC services
if [ -d /mnt/mnt-newroot/var ] && [ ! -e /mnt/mnt-newroot/var/run ]; then
    ln -s ../run /mnt/mnt-newroot/var/run
fi

# Note: Do NOT move /mnt - it hosts the overlay disk itself

# Ensure /dev/net/tun exists inside the new root (needed for virtio-net)
if [ ! -e /mnt/mnt-newroot/dev/net/tun ]; then
    echo "overlay-init: Creating /dev/net/tun in new root"
    mkdir -p /mnt/mnt-newroot/dev/net
    mknod /mnt/mnt-newroot/dev/net/tun c 10 200
    chmod 666 /mnt/mnt-newroot/dev/net/tun
fi

# Enter overlay root without pivot_root (avoids kernel network disruption)
if command -v chroot >/dev/null 2>&1; then
    echo "overlay-init: Switching root via chroot"
    exec chroot /mnt/mnt-newroot /sbin/init
elif [ -x /bin/busybox ]; then
    echo "overlay-init: Switching root via busybox chroot"
    exec /bin/busybox chroot /mnt/mnt-newroot /sbin/init
else
    echo "overlay-init: chroot binary not found; dropping to shell"
    exec /bin/sh
fi
"#;

    // Write overlay-init script to /sbin/overlay-init
    let overlay_init_path = mount_point.join("sbin/overlay-init");
    tokio::fs::write(&overlay_init_path, overlay_init_script)
        .await
        .context("writing /sbin/overlay-init")?;

    // Make executable
    let output = Command::new("chmod")
        .args(&["+x", overlay_init_path.to_str().unwrap()])
        .output()
        .context("making overlay-init executable")?;

    if !output.status.success() {
        bail!("chmod failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    info!("overlay-init script installed");
    println!("  ✓ overlay-init script installed for OverlayFS support");

    Ok(())
}
