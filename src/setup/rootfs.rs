use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{info, warn};

use crate::paths;

/// Find the fc-agent binary (sibling in same target directory as fcvm)
///
/// Both fcvm and fc-agent are workspace members built together with:
///   cargo build --release --target aarch64-unknown-linux-musl
///
/// This puts both binaries in the same directory (target/.../release/).
fn find_fc_agent_binary() -> Result<PathBuf> {
    // Primary: fc-agent is built alongside fcvm in the same target directory
    let exe_path = std::env::current_exe().context("getting current executable path")?;
    let exe_dir = exe_path.parent().context("getting executable directory")?;
    let fc_agent = exe_dir.join("fc-agent");

    if fc_agent.exists() {
        return Ok(fc_agent);
    }

    // Fallback: environment variable override for special cases
    if let Ok(path) = std::env::var("FC_AGENT_PATH") {
        let p = PathBuf::from(&path);
        if p.exists() {
            return Ok(p);
        }
    }

    bail!(
        "fc-agent binary not found at {} or via FC_AGENT_PATH env var.\n\
         Build with: cargo build --release --target aarch64-unknown-linux-musl",
        fc_agent.display()
    )
}

/// Helper to convert Path to str with proper error handling
fn path_to_str(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow::anyhow!("path contains invalid UTF-8: {:?}", path))
}

/// Ensure rootfs exists, creating minimal Alpine + Podman if needed
pub async fn ensure_rootfs() -> Result<PathBuf> {
    let rootfs_dir = paths::rootfs_dir();
    let rootfs_path = paths::base_rootfs();

    // If rootfs exists, check if we should rebuild
    if rootfs_path.exists() {
        // Only rebuild if running as root (uid 0)
        // Non-root users use the existing rootfs to enable true rootless operation
        if unsafe { libc::getuid() } == 0 {
            info!(path = %rootfs_path.display(), "rootfs exists but rebuilding to ensure latest changes");
            tokio::fs::remove_file(&rootfs_path)
                .await
                .context("removing old rootfs")?;
        } else {
            info!(path = %rootfs_path.display(), "rootfs exists, using as-is (non-root mode)");
            return Ok(rootfs_path);
        }
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
        .args([
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
        .args(["-F", path_to_str(output_path)?])
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
        .args([
            "-o",
            "loop",
            path_to_str(output_path)?,
            path_to_str(&mount_point)?,
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
    if let Ok(mount_str) = path_to_str(&mount_point) {
        let _ = Command::new("umount").arg(mount_str).output();
    }

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
        .args(["-L", "-o", tarball_path, &url])
        .output()
        .context("downloading Alpine")?;

    if !output.status.success() {
        bail!("curl failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    info!("extracting Alpine rootfs");
    println!("  → Extracting...");

    // Extract to mount point
    let output = Command::new("tar")
        .args(["-xzf", tarball_path, "-C", path_to_str(mount_point)?])
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
    // gcompat provides glibc compatibility for running glibc-linked binaries (fc-agent with fuser)
    // fuse3 provides libfuse3 for FUSE filesystem support
    let output = Command::new("chroot")
        .arg(path_to_str(mount_point)?)
        .args(["/bin/sh", "-c", "apk update && apk add podman crun fuse-overlayfs openrc haveged ca-certificates chrony openresolv gcompat fuse3"])
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
    if let Ok(mount_str) = path_to_str(mount_point) {
        let _ = Command::new("chroot")
            .arg(mount_str)
            .args(["/bin/sh", "-c", "rc-update add devfs boot && rc-update add procfs boot && rc-update add sysfs boot && rc-update add cgroups boot && rc-update add haveged boot && rc-update add chronyd default"])
            .output();
    }

    // Configure networking for Alpine Linux
    // Network configuration is passed via kernel cmdline (ip= parameter)
    // so we don't need static configuration in /etc/network/interfaces
    // Just configure loopback
    info!("configuring network interfaces");
    let interfaces_config = r#"auto lo
iface lo inet loopback

# eth0 configured via kernel cmdline (ip= parameter)
auto eth0
iface eth0 inet manual
    # Add route for MMDS server (Firecracker metadata service)
    post-up ip route add 169.254.169.254/32 dev eth0
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
    install_fc_agent(mount_point).await?;

    // Install diagnostic scripts for debugging VM boot sequence
    install_diagnostic_scripts(mount_point).await?;

    // Install overlay-init script for OverlayFS support
    install_overlay_init(mount_point).await?;

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
    if let Ok(path_str) = path_to_str(&debug_script_path) {
        let _ = Command::new("chmod").args(["+x", path_str]).output();
    }

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
    if let Ok(path_str) = path_to_str(&local_path) {
        let _ = Command::new("chmod").args(["+x", path_str]).output();
    }

    // Note: ARP cache flushing for snapshot restore is handled by fc-agent
    // via MMDS restore-epoch signaling - not via boot scripts (which don't run on restore)

    // Enable local service
    if let Ok(mount_str) = path_to_str(mount_point) {
        let _ = Command::new("chroot")
            .arg(mount_str)
            .args(["/bin/sh", "-c", "rc-update add local default"])
            .output();
    }

    Ok(())
}

/// Install diagnostic scripts for observing VM boot sequence
async fn install_diagnostic_scripts(mount_point: &Path) -> Result<()> {
    info!("installing diagnostic scripts");

    // 1. Kernel message logger - outputs dmesg periodically to serial console
    let dmesg_logger_script = r#"#!/bin/sh
# dmesg-logger: Continuously output kernel messages to serial console
# This helps debug kernel-level issues during VM boot and resume

echo "[dmesg-logger] Starting kernel message logger..." > /dev/console

# Initial kernel buffer dump
echo "=== Initial kernel messages ===" > /dev/console
dmesg | tail -20 > /dev/console

# Monitor new kernel messages every second for 30 seconds
# (Most interesting boot activity happens in first 30s)
for i in $(seq 1 30); do
    sleep 1
    NEW_MSGS=$(dmesg | tail -5)
    if [ -n "$NEW_MSGS" ]; then
        echo "[dmesg +${i}s]" > /dev/console
        echo "$NEW_MSGS" > /dev/console
    fi
done

echo "[dmesg-logger] Kernel message monitoring complete" > /dev/console
"#;

    let dmesg_logger_path = mount_point.join("usr/local/bin/dmesg-logger.sh");
    tokio::fs::write(&dmesg_logger_path, dmesg_logger_script)
        .await
        .context("writing dmesg-logger script")?;

    let output = Command::new("chmod")
        .args(["+x", path_to_str(&dmesg_logger_path)?])
        .output()
        .context("making dmesg-logger executable")?;

    if !output.status.success() {
        bail!("chmod failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    // 2. Process state monitor - tracks what ALL processes are doing after resume
    let process_monitor_script = r#"#!/bin/sh
# process-monitor: Monitor process states to understand what's blocking during clone resume
# This runs continuously and logs process states to help debug the 6-second delay

echo "[proc-monitor] Starting process state monitor..." > /dev/console

# Monitor process states every 200ms for 10 seconds
for i in $(seq 1 50); do
    # Get timestamp in milliseconds (approximate)
    TIME=$((i * 200))

    # Find all processes and show their state
    # Format: PID STAT COMMAND
    # STAT codes: R=running, S=sleeping, D=uninterruptible sleep (IO wait), Z=zombie, T=stopped
    echo "[proc-monitor T+${TIME}ms] Process states:" > /dev/console
    ps -eo pid,stat,comm 2>/dev/null | grep -E '(nginx|conmon|podman|sleep)' | while read line; do
        echo "[proc-monitor T+${TIME}ms]   $line" > /dev/console
    done

    # Check what's listening on network ports
    LISTENERS=$(netstat -tuln 2>/dev/null | grep -E ':80|:443' | wc -l)
    if [ "$LISTENERS" -gt 0 ]; then
        echo "[proc-monitor T+${TIME}ms] Listening on port 80: YES" > /dev/console
    else
        echo "[proc-monitor T+${TIME}ms] Listening on port 80: NO" > /dev/console
    fi

    # Check for uninterruptible sleep (IO wait) - this is often the culprit
    IO_WAIT=$(ps -eo stat | grep -c '^D')
    if [ "$IO_WAIT" -gt 0 ]; then
        echo "[proc-monitor T+${TIME}ms] ⚠️  $IO_WAIT processes in uninterruptible sleep (IO wait)" > /dev/console
        ps -eo pid,stat,wchan:20,comm 2>/dev/null | grep '^[0-9]* D' | while read line; do
            echo "[proc-monitor T+${TIME}ms]     IO-WAIT: $line" > /dev/console
        done
    fi

    sleep 0.2
done

echo "[proc-monitor] Process monitoring complete" > /dev/console
"#;

    let process_monitor_path = mount_point.join("usr/local/bin/process-monitor.sh");
    tokio::fs::write(&process_monitor_path, process_monitor_script)
        .await
        .context("writing process-monitor script")?;

    let output = Command::new("chmod")
        .args(["+x", path_to_str(&process_monitor_path)?])
        .output()
        .context("making process-monitor executable")?;

    if !output.status.success() {
        bail!("chmod failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    // 3. Create local.d startup scripts to run diagnostics at boot
    let local_d_dir = mount_point.join("etc/local.d");
    tokio::fs::create_dir_all(&local_d_dir)
        .await
        .context("creating local.d directory")?;

    // Script to launch dmesg logger early in boot
    let dmesg_start = r#"#!/bin/sh
/usr/local/bin/dmesg-logger.sh &
"#;
    let dmesg_start_path = local_d_dir.join("dmesg-logger.start");
    tokio::fs::write(&dmesg_start_path, dmesg_start)
        .await
        .context("writing dmesg-logger.start")?;

    let output = Command::new("chmod")
        .args(["+x", path_to_str(&dmesg_start_path)?])
        .output()
        .context("making dmesg-logger.start executable")?;

    if !output.status.success() {
        bail!("chmod failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    // Script to launch process monitor immediately (runs on boot AND resume from snapshot!)
    let process_start = r#"#!/bin/sh
/usr/local/bin/process-monitor.sh &
"#;
    let process_start_path = local_d_dir.join("process-monitor.start");
    tokio::fs::write(&process_start_path, process_start)
        .await
        .context("writing process-monitor.start")?;

    let output = Command::new("chmod")
        .args(["+x", path_to_str(&process_start_path)?])
        .output()
        .context("making process-monitor.start executable")?;

    if !output.status.success() {
        bail!("chmod failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    // Enable local service (runs scripts in /etc/local.d/*.start)
    let output = Command::new("chroot")
        .arg(path_to_str(mount_point)?)
        .args(["/bin/sh", "-c", "rc-update add local default 2>/dev/null || true"])
        .output()
        .context("enabling local service")?;

    if !output.status.success() {
        warn!(
            "Failed to enable local service: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    info!("diagnostic scripts installed");
    println!("  ✓ Diagnostic scripts installed (dmesg-logger, process-monitor)");

    Ok(())
}

/// Install fc-agent guest agent into rootfs
async fn install_fc_agent(mount_point: &Path) -> Result<()> {
    info!("installing fc-agent guest agent");

    // Find fc-agent binary - it's built alongside fcvm in the same target directory
    // Both binaries are workspace members built with: cargo build --release --target aarch64-unknown-linux-musl
    let fc_agent_src = find_fc_agent_binary()?;

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
        .args(["+x", path_to_str(&fc_agent_dest)?])
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
        .args(["+x", path_to_str(&service_path)?])
        .output()
        .context("making service executable")?;

    if !output.status.success() {
        bail!("chmod failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    // Enable fc-agent service (add to default runlevel)
    let output = Command::new("chroot")
        .arg(path_to_str(mount_point)?)
        .args(["/bin/sh", "-c", "rc-update add fc-agent default"])
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
        .args(["+x", path_to_str(&overlay_init_path)?])
        .output()
        .context("making overlay-init executable")?;

    if !output.status.success() {
        bail!("chmod failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    info!("overlay-init script installed");
    println!("  ✓ overlay-init script installed for OverlayFS support");

    Ok(())
}
