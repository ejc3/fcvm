use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{info, warn};

/// Ensure rootfs exists, creating minimal Alpine + Podman if needed
pub async fn ensure_rootfs() -> Result<PathBuf> {
    let rootfs_dir = PathBuf::from("/var/lib/fcvm/rootfs");
    let rootfs_path = rootfs_dir.join("base.ext4");

    // ALWAYS rebuild rootfs for now - ensures code changes take effect
    // TODO: Add version check or --force-rebuild flag for production
    if rootfs_path.exists() {
        info!(path = %rootfs_path.display(), "rootfs exists but rebuilding to ensure latest changes");
        tokio::fs::remove_file(&rootfs_path).await
            .context("removing old rootfs")?;
    }

    println!("⚙️  Creating base rootfs (~60s)...");

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
    
    // Install Podman + haveged + ca-certificates + chrony via chroot + apk
    // Note: Alpine uses OpenRC, not systemd
    // haveged provides entropy to fix CRNG init delays
    // ca-certificates needed for fc-agent's reqwest HTTPS requests
    // chrony provides NTP time synchronization (fixes TLS cert validation)
    let output = Command::new("chroot")
        .arg(mount_point.to_str().unwrap())
        .args(&["/bin/sh", "-c", "apk update && apk add podman crun fuse-overlayfs openrc haveged ca-certificates chrony"])
        .output()
        .context("installing packages via apk")?;

    if !output.status.success() {
        warn!("apk install had issues: {}", String::from_utf8_lossy(&output.stderr));
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

    // Configure networking for Alpine Linux with static IP
    // This matches the TAP device configuration (172.16.0.1/30)
    info!("configuring network interfaces with static IP");
    let interfaces_config = r#"auto lo
iface lo inet loopback

auto eth0
iface eth0 inet static
    address 172.16.0.2
    netmask 255.255.255.252
    gateway 172.16.0.1
"#;
    let interfaces_path = mount_point.join("etc/network/interfaces");
    tokio::fs::write(&interfaces_path, interfaces_config).await
        .context("writing /etc/network/interfaces")?;

    // Enable networking service
    let _ = Command::new("chroot")
        .arg(mount_point.to_str().unwrap())
        .args(&["/bin/sh", "-c", "rc-update add networking boot"])
        .output();

    // Enable serial console on ttyS0 (for Firecracker)
    info!("enabling serial console");
    let inittab_path = mount_point.join("etc/inittab");
    let inittab = tokio::fs::read_to_string(&inittab_path).await
        .context("reading /etc/inittab")?;
    let inittab_fixed = inittab.replace(
        "#ttyS0::respawn:/sbin/getty -L ttyS0 115200 vt100",
        "ttyS0::respawn:/sbin/getty -L ttyS0 115200 vt100"
    );
    tokio::fs::write(&inittab_path, inittab_fixed).await
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
    tokio::fs::create_dir_all(&chrony_dir).await
        .context("creating /etc/chrony directory")?;

    let chrony_conf_path = chrony_dir.join("chrony.conf");
    tokio::fs::write(&chrony_conf_path, chrony_conf).await
        .context("writing /etc/chrony/chrony.conf")?;

    // Install fc-agent binary and OpenRC service
    install_fc_agent(&mount_point).await?;

    Ok(())
}

/// Install fc-agent guest agent into rootfs
async fn install_fc_agent(mount_point: &Path) -> Result<()> {
    info!("installing fc-agent guest agent");

    // Find fc-agent binary - try multiple locations
    // IMPORTANT: Prefer musl target for Alpine Linux compatibility (statically linked)
    let possible_paths = vec![
        PathBuf::from("/home/ubuntu/fcvm/fc-agent/target/aarch64-unknown-linux-musl/release/fc-agent"),  // musl (static)
        PathBuf::from("fc-agent/target/aarch64-unknown-linux-musl/release/fc-agent"),  // musl relative
        PathBuf::from("/home/ubuntu/fcvm/fc-agent/target/release/fc-agent"),  // gnu (fallback, won't work on Alpine)
        PathBuf::from("fc-agent/target/release/fc-agent"),  // gnu relative
        PathBuf::from("../fc-agent/target/release/fc-agent"),  // gnu relative from current dir
    ];

    let fc_agent_src = possible_paths.iter()
        .find(|p| p.exists())
        .cloned();

    let fc_agent_src = match fc_agent_src {
        Some(path) => path,
        None => {
            bail!("fc-agent binary not found in any of: {:?}\n\
                   Please build it first: cd fc-agent && cargo build --release",
                   possible_paths);
        }
    };

    // Create /usr/local/bin directory if it doesn't exist
    let bin_dir = mount_point.join("usr/local/bin");
    tokio::fs::create_dir_all(&bin_dir).await
        .context("creating /usr/local/bin directory")?;

    // Copy fc-agent to /usr/local/bin/fc-agent
    let fc_agent_dest = mount_point.join("usr/local/bin/fc-agent");
    tokio::fs::copy(&fc_agent_src, &fc_agent_dest).await
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
    tokio::fs::write(&service_path, service_script).await
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
        warn!("Failed to enable fc-agent service: {}", String::from_utf8_lossy(&output.stderr));
    }

    info!("fc-agent installed and enabled");
    println!("  ✓ fc-agent guest agent installed");

    Ok(())
}
