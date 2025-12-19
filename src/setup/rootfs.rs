use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{info, warn};

use crate::paths;

/// Find the fc-agent binary
///
/// Both fcvm and fc-agent are workspace members built together with:
///   cargo build --release
///
/// Search order:
/// 1. Same directory as current exe (for cargo install)
/// 2. Parent directory (for tests running from target/release/deps/)
/// 3. FC_AGENT_PATH environment variable
fn find_fc_agent_binary() -> Result<PathBuf> {
    let exe_path = std::env::current_exe().context("getting current executable path")?;
    let exe_dir = exe_path.parent().context("getting executable directory")?;

    // Check same directory (cargo install case)
    let fc_agent = exe_dir.join("fc-agent");
    if fc_agent.exists() {
        return Ok(fc_agent);
    }

    // Check parent directory (test case: exe in target/release/deps/, agent in target/release/)
    if let Some(parent) = exe_dir.parent() {
        let fc_agent_parent = parent.join("fc-agent");
        if fc_agent_parent.exists() {
            return Ok(fc_agent_parent);
        }
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
         Build with: cargo build --release",
        fc_agent.display()
    )
}

/// Helper to convert Path to str with proper error handling
fn path_to_str(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow::anyhow!("path contains invalid UTF-8: {:?}", path))
}

/// Ensure rootfs exists, creating minimal Ubuntu + Podman if needed
///
/// Caches the rootfs filesystem - only creates it once.
/// The base rootfs is immutable after creation to prevent corruption when VMs start in parallel.
pub async fn ensure_rootfs() -> Result<PathBuf> {
    let rootfs_dir = paths::rootfs_dir();
    let rootfs_path = paths::base_rootfs();
    let lock_file = rootfs_dir.join(".rootfs-creation.lock");

    // If rootfs exists, return it immediately (it's immutable after creation)
    // DO NOT modify the base rootfs on every VM start - this causes:
    // 1. Filesystem corruption when VMs start in parallel
    // 2. Unnecessary latency (~100ms per VM start)
    // 3. Violates the "base rootfs is immutable" principle
    //
    // To update fc-agent: delete the rootfs and it will be recreated, OR
    // explicitly run `fcvm setup rootfs` (TODO: implement setup command)
    if rootfs_path.exists() {
        info!(path = %rootfs_path.display(), "rootfs exists (using cached)");
        return Ok(rootfs_path);
    }

    // Create directory for lock file
    tokio::fs::create_dir_all(&rootfs_dir)
        .await
        .context("creating rootfs directory")?;

    // Acquire lock to prevent concurrent rootfs creation
    // If multiple VMs start simultaneously, only one creates the rootfs
    info!("acquiring rootfs creation lock");
    use std::os::unix::fs::OpenOptionsExt;
    let lock_fd = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&lock_file)
        .context("opening rootfs creation lock file")?;

    use nix::fcntl::{Flock, FlockArg};
    let flock = Flock::lock(lock_fd, FlockArg::LockExclusive)
        .map_err(|(_, err)| err)
        .context("acquiring rootfs creation lock")?;

    // Check again after acquiring lock (another process may have created it)
    if rootfs_path.exists() {
        info!(path = %rootfs_path.display(), "rootfs exists (created by another process)");
        flock.unlock().map_err(|(_, err)| err).ok();
        let _ = std::fs::remove_file(&lock_file);
        return Ok(rootfs_path);
    }

    // Now we have exclusive access, create the rootfs
    info!("creating base rootfs from Ubuntu cloud image");
    info!("note: first-time cloud image download may take 5-15 minutes");
    info!("cached rootfs creation takes ~45 seconds");

    let result = create_ubuntu_rootfs(&rootfs_path)
        .await
        .context("creating Ubuntu rootfs");

    // Release lock
    flock
        .unlock()
        .map_err(|(_, err)| err)
        .context("releasing rootfs creation lock")?;
    let _ = std::fs::remove_file(&lock_file);

    result?;

    info!("rootfs creation complete");

    Ok(rootfs_path)
}

/// Create Ubuntu rootfs from official cloud image
///
/// Downloads Ubuntu 24.04 cloud image (cached), customizes it with virt-customize,
/// extracts to ext4, then installs packages.
async fn create_ubuntu_rootfs(output_path: &Path) -> Result<()> {
    // Download Ubuntu cloud image (cached)
    let cloud_image = download_ubuntu_cloud_image().await?;

    info!("customizing Ubuntu cloud image with virt-customize");

    // Customize the qcow2 image BEFORE extracting
    customize_ubuntu_cloud_image(&cloud_image).await?;

    // Extract root partition from customized cloud image
    info!("extracting customized root partition");
    extract_root_partition(&cloud_image, output_path).await?;

    // Install packages after extraction (virt-customize has networking issues)
    info!("installing packages in extracted rootfs");
    install_packages_in_rootfs(output_path).await?;

    Ok(())
}

/// Download Ubuntu cloud image (cached)
async fn download_ubuntu_cloud_image() -> Result<PathBuf> {
    let cache_dir = paths::base_dir().join("cache");
    tokio::fs::create_dir_all(&cache_dir)
        .await
        .context("creating cache directory")?;

    // Detect architecture and use appropriate cloud image
    let (arch_name, cloud_arch) = match std::env::consts::ARCH {
        "x86_64" => ("amd64", "amd64"),
        "aarch64" => ("arm64", "arm64"),
        other => bail!("unsupported architecture: {}", other),
    };

    let image_url = format!(
        "https://cloud-images.ubuntu.com/releases/24.04/release/ubuntu-24.04-server-cloudimg-{cloud_arch}.img"
    );
    let image_path = cache_dir.join(format!("ubuntu-24.04-{arch_name}.img"));

    // Return cached image if it exists
    if image_path.exists() {
        info!(path = %image_path.display(), "using cached Ubuntu cloud image");
        return Ok(image_path);
    }

    info!(url = %image_url, "downloading Ubuntu 24.04 cloud image");
    info!("download size: ~644MB (one-time, cached for future use)");
    info!("download may take 5-15 minutes depending on network speed");

    // Download with reqwest
    let client = reqwest::Client::new();
    let response = client
        .get(image_url)
        .send()
        .await
        .context("downloading cloud image")?;

    if !response.status().is_success() {
        bail!("download failed with status: {}", response.status());
    }

    // Get content length for progress reporting
    let total_size = response.content_length().unwrap_or(0);
    let total_mb = total_size as f64 / 1024.0 / 1024.0;

    // Stream to file with progress
    let mut file = File::create(&image_path)
        .await
        .context("creating image file")?;

    let bytes = response.bytes().await.context("reading response body")?;
    let downloaded_mb = bytes.len() as f64 / 1024.0 / 1024.0;

    file.write_all(&bytes).await.context("writing image file")?;
    file.flush().await.context("flushing image file")?;

    info!(path = %image_path.display(),
          downloaded_mb = downloaded_mb,
          expected_mb = total_mb,
          "cloud image download complete");

    Ok(image_path)
}

/// Extract root partition from qcow2 cloud image to a raw ext4 file
async fn extract_root_partition(qcow2_path: &Path, output_path: &Path) -> Result<()> {
    info!("extracting root partition from cloud image");

    // Find a free NBD device
    let nbd_device = "/dev/nbd0";

    // Load nbd kernel module if not already loaded
    let _ = Command::new("modprobe")
        .arg("nbd")
        .arg("max_part=8")
        .output()
        .await;

    // Connect qcow2 to NBD device
    info!("connecting qcow2 to NBD device");
    let output = Command::new("qemu-nbd")
        .args(["--connect", nbd_device, "-r", path_to_str(qcow2_path)?])
        .output()
        .await
        .context("running qemu-nbd connect")?;

    if !output.status.success() {
        bail!(
            "qemu-nbd connect failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Force kernel to re-read partition table - required on some systems (e.g., CI runners)
    // Try partprobe first (from parted), fall back to partx (from util-linux)
    info!("scanning partition table");
    let partprobe_result = Command::new("partprobe").arg(nbd_device).output().await;
    if partprobe_result.is_err()
        || !partprobe_result
            .as_ref()
            .map(|o| o.status.success())
            .unwrap_or(false)
    {
        // Fallback to partx
        let _ = Command::new("partx")
            .args(["-a", nbd_device])
            .output()
            .await;
    }

    // Wait for partition to appear with retry loop
    let partition = format!("{}p1", nbd_device);

    // Small delay to allow kernel to create partition device nodes
    // This is needed because partprobe/partx returns before udev creates the nodes
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut retries = 10;
    while retries > 0 && !std::path::Path::new(&partition).exists() {
        info!(
            partition = %partition,
            retries_left = retries,
            "waiting for partition to appear"
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        retries -= 1;
    }

    // If partition still doesn't exist, try to create the device node manually.
    // This is needed when running in a container where the host kernel creates
    // the partition device on the host's devtmpfs, but the container has its own.
    // NBD major is 43, partition 1 is minor 1.
    if !std::path::Path::new(&partition).exists() {
        info!("partition not auto-created, trying mknod");

        // Get partition info from sysfs
        let sysfs_path = "/sys/block/nbd0/nbd0p1/dev";
        let dev_info = tokio::fs::read_to_string(sysfs_path).await;

        if let Ok(dev_str) = dev_info {
            // dev_str is "major:minor" e.g., "43:1"
            let dev_str = dev_str.trim();
            info!(dev = %dev_str, "found partition info in sysfs");

            // Create device node with mknod
            let mknod_result = Command::new("mknod")
                .args([&partition, "b", "43", "1"])
                .output()
                .await;

            if let Ok(output) = mknod_result {
                if output.status.success() {
                    info!(partition = %partition, "created partition device node");
                } else {
                    warn!("mknod failed: {}", String::from_utf8_lossy(&output.stderr));
                }
            }
        } else {
            // Try mknod with assumed minor number (1 for first partition)
            info!("sysfs info not available, trying mknod with assumed minor 1");
            let _ = Command::new("mknod")
                .args([&partition, "b", "43", "1"])
                .output()
                .await;
        }
    }

    // Final check
    if !std::path::Path::new(&partition).exists() {
        // List what devices exist for debugging
        let ls_output = Command::new("sh")
            .args([
                "-c",
                "ls -la /dev/nbd0* 2>/dev/null || echo 'no nbd devices'",
            ])
            .output()
            .await;
        let devices = ls_output
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_else(|_| "failed to list".to_string());

        // Also check sysfs for partition info
        let sysfs_output = Command::new("sh")
            .args([
                "-c",
                "cat /sys/block/nbd0/nbd0p1/dev 2>/dev/null || echo 'no sysfs info'",
            ])
            .output()
            .await;
        let sysfs_info = sysfs_output
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_else(|_| "no sysfs".to_string());

        bail!(
            "partition {} not found after waiting. Devices: {}, Sysfs: {}",
            partition,
            devices.trim(),
            sysfs_info.trim()
        );
    }

    info!(partition = %partition, "copying root partition");
    let output = Command::new("dd")
        .args([
            &format!("if={}", partition),
            &format!("of={}", path_to_str(output_path)?),
            "bs=4M",
        ])
        .output()
        .await;

    // Always disconnect NBD
    let disconnect_output = Command::new("qemu-nbd")
        .args(["--disconnect", nbd_device])
        .output()
        .await;

    // Check dd result
    let output = output.context("running dd")?;
    if !output.status.success() {
        bail!("dd failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    // Check disconnect result
    if let Ok(disc_out) = disconnect_output {
        if !disc_out.status.success() {
            warn!(
                "qemu-nbd disconnect warning: {}",
                String::from_utf8_lossy(&disc_out.stderr)
            );
        }
    }

    // Resize the extracted ext4 to 10GB (plenty of space for containers)
    info!("resizing filesystem to 10GB");

    // First resize the file itself to 10GB
    let output = Command::new("truncate")
        .args(["-s", "10G", path_to_str(output_path)?])
        .output()
        .await
        .context("running truncate")?;

    if !output.status.success() {
        bail!(
            "truncate failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Check and fix filesystem
    let output = Command::new("e2fsck")
        .args(["-f", "-y", path_to_str(output_path)?])
        .output()
        .await
        .context("running e2fsck")?;

    if !output.status.success()
        && !output
            .status
            .code()
            .map(|c| c == 1 || c == 2)
            .unwrap_or(false)
    {
        // Exit codes 1-2 are warnings, not errors
        warn!(
            "e2fsck warnings: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Resize filesystem to fill the file
    let output = Command::new("resize2fs")
        .arg(path_to_str(output_path)?)
        .output()
        .await
        .context("running resize2fs")?;

    if !output.status.success() {
        bail!(
            "resize2fs failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(())
}

/// Customize Ubuntu cloud image using virt-customize
///
/// This modifies the qcow2 image in-place, adding Podman, fc-agent, and all configs.
/// Much simpler and more robust than manual mount/chroot/unmount.
async fn customize_ubuntu_cloud_image(image_path: &Path) -> Result<()> {
    // Find fc-agent binary
    let fc_agent_src = find_fc_agent_binary()?;

    info!("running virt-customize on cloud image");

    let mut cmd = Command::new("virt-customize");
    cmd.arg("-a").arg(path_to_str(image_path)?);

    // Disable networking to avoid passt errors (packages installed later via chroot)
    cmd.arg("--no-network");

    // 1. Fix /etc/fstab - remove BOOT and UEFI partitions that don't exist
    cmd.arg("--run-command")
        .arg("sed -i '/LABEL=BOOT/d;/LABEL=UEFI/d' /etc/fstab");

    // 2. Copy fc-agent binary (packages installed later via chroot)
    // Note: universe repository already enabled in base cloud image
    info!("adding fc-agent binary");
    cmd.arg("--run-command").arg("mkdir -p /usr/local/bin");
    cmd.arg("--copy-in")
        .arg(format!("{}:/usr/local/bin/", fc_agent_src.display()));
    cmd.arg("--chmod").arg("0755:/usr/local/bin/fc-agent");

    // 4. Write chrony config (create directory first)
    info!("adding chrony config");
    cmd.arg("--run-command").arg("mkdir -p /etc/chrony");
    let chrony_conf = "# NTP servers from pool.ntp.org\npool pool.ntp.org iburst\n\n\
                       # Allow clock to be stepped (not slewed) for large time differences\n\
                       makestep 1.0 3\n\n\
                       # Directory for drift and other runtime files\n\
                       driftfile /var/lib/chrony/drift\n";
    cmd.arg("--write")
        .arg(format!("/etc/chrony/chrony.conf:{}", chrony_conf));

    // 5. Write systemd-networkd config
    info!("adding network config");
    cmd.arg("--run-command")
        .arg("mkdir -p /etc/systemd/network /etc/systemd/network/10-eth0.network.d");

    let network_config = "[Match]\nName=eth0\n\n[Network]\n# Keep kernel IP configuration from ip= boot parameter\nKeepConfiguration=yes\n# DNS is provided via kernel ip= boot parameter (gateway IP where dnsmasq listens)\n";
    cmd.arg("--write").arg(format!(
        "/etc/systemd/network/10-eth0.network:{}",
        network_config
    ));

    let mmds_route = "[Route]\nDestination=169.254.169.254/32\nScope=link\n";
    cmd.arg("--write").arg(format!(
        "/etc/systemd/network/10-eth0.network.d/mmds.conf:{}",
        mmds_route
    ));

    // 6. Write DNS setup script and service
    // This extracts the gateway IP from kernel cmdline and configures it as DNS
    // The kernel ip= parameter format is: ip=client::gateway:netmask::device:autoconf[:dns]
    info!("adding DNS setup script");
    let dns_setup_script = r#"#!/bin/bash
# Extract gateway from kernel cmdline and configure as DNS
# Format: ip=<client>::<gateway>:<netmask>::eth0:off[:<dns>]
set -e

echo "[fcvm-dns] starting DNS configuration"
CMDLINE=$(cat /proc/cmdline)
echo "[fcvm-dns] cmdline: $CMDLINE"

if [[ $CMDLINE =~ ip=([^[:space:]]+) ]]; then
    IP_PARAM="${BASH_REMATCH[1]}"
    echo "[fcvm-dns] ip param: $IP_PARAM"
    # Extract gateway (3rd field, after ::)
    GATEWAY=$(echo "$IP_PARAM" | cut -d: -f3)
    # Check if explicit DNS was provided (8th field)
    DNS=$(echo "$IP_PARAM" | cut -d: -f8)
    echo "[fcvm-dns] gateway=$GATEWAY dns=$DNS"
    if [ -n "$DNS" ]; then
        # Use explicit DNS from boot args
        echo "nameserver $DNS" > /etc/resolv.conf
        echo "[fcvm-dns] configured DNS from boot args: $DNS"
    elif [ -n "$GATEWAY" ]; then
        # Fall back to gateway as DNS (dnsmasq)
        echo "nameserver $GATEWAY" > /etc/resolv.conf
        echo "[fcvm-dns] configured DNS from gateway: $GATEWAY"
    else
        echo "[fcvm-dns] ERROR: no DNS or gateway found in ip= parameter"
        exit 1
    fi
else
    echo "[fcvm-dns] ERROR: no ip= parameter found in cmdline"
    exit 1
fi

echo "[fcvm-dns] /etc/resolv.conf:"
cat /etc/resolv.conf
echo "[fcvm-dns] done"
"#;
    cmd.arg("--write").arg(format!(
        "/usr/local/bin/fcvm-setup-dns:{}",
        dns_setup_script
    ));
    cmd.arg("--chmod").arg("0755:/usr/local/bin/fcvm-setup-dns");

    let dns_setup_service = "[Unit]\n\
                             Description=Configure DNS from kernel boot parameters\n\
                             DefaultDependencies=no\n\
                             Before=network.target systemd-resolved.service\n\
                             After=local-fs.target\n\n\
                             [Service]\n\
                             Type=oneshot\n\
                             ExecStart=/usr/local/bin/fcvm-setup-dns\n\
                             RemainAfterExit=yes\n\
                             StandardOutput=journal+console\n\
                             StandardError=journal+console\n\n\
                             [Install]\n\
                             WantedBy=sysinit.target\n";
    cmd.arg("--write").arg(format!(
        "/etc/systemd/system/fcvm-setup-dns.service:{}",
        dns_setup_service
    ));

    // 7. Write fc-agent systemd service
    info!("adding fc-agent service");
    let fc_agent_service = "[Unit]\nDescription=fcvm guest agent for container orchestration\n\
                            After=network.target\nWants=network.target\n\n\
                            [Service]\nType=simple\nExecStart=/usr/local/bin/fc-agent\n\
                            Restart=on-failure\nRestartSec=5\n\
                            StandardOutput=journal+console\nStandardError=journal+console\n\n\
                            [Install]\nWantedBy=multi-user.target\n";
    cmd.arg("--write").arg(format!(
        "/etc/systemd/system/fc-agent.service:{}",
        fc_agent_service
    ));

    // 9. Enable services (fc-agent + dns-setup, other services enabled after package install)
    info!("enabling systemd services");
    cmd.arg("--run-command")
        .arg("systemctl enable fc-agent fcvm-setup-dns systemd-networkd serial-getty@ttyS0");

    info!("executing virt-customize (this should be quick)");

    let output = cmd.output().await.context("running virt-customize")?;

    if !output.status.success() {
        bail!(
            "virt-customize failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    info!("virt-customize completed successfully");

    Ok(())
}

/// Install packages in extracted rootfs using mount + chroot
///
/// This is done AFTER extraction because virt-customize has networking issues.
/// Still much simpler than the old approach - single-purpose mount+chroot.
async fn install_packages_in_rootfs(rootfs_path: &Path) -> Result<()> {
    let temp_dir = PathBuf::from("/tmp/fcvm-rootfs-install");
    let mount_point = temp_dir.join("mnt");

    // Cleanup any previous mounts
    let _ = Command::new("umount")
        .arg("-R")
        .arg(path_to_str(&mount_point).unwrap_or("/tmp/fcvm-rootfs-install/mnt"))
        .output()
        .await;
    let _ = tokio::fs::remove_dir_all(&temp_dir).await;

    tokio::fs::create_dir_all(&mount_point)
        .await
        .context("creating temp mount directory")?;

    // Mount the rootfs
    let output = Command::new("mount")
        .args([
            "-o",
            "loop",
            path_to_str(rootfs_path)?,
            path_to_str(&mount_point)?,
        ])
        .output()
        .await
        .context("mounting rootfs for package installation")?;

    if !output.status.success() {
        bail!(
            "mount failed: {}. Are you running as root?",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Mount required filesystems for chroot
    for (fs, target) in [
        ("proc", "proc"),
        ("sysfs", "sys"),
        ("devtmpfs", "dev"),
        ("devpts", "dev/pts"),
    ] {
        let target_path = mount_point.join(target);
        let _ = Command::new("mount")
            .args(["-t", fs, fs, path_to_str(&target_path)?])
            .output()
            .await;
    }

    // Copy DNS resolution config into chroot for apt-get update
    let resolv_conf_dest = mount_point.join("etc/resolv.conf");
    // Remove existing resolv.conf (might be a symlink)
    let _ = tokio::fs::remove_file(&resolv_conf_dest).await;
    tokio::fs::copy("/etc/resolv.conf", &resolv_conf_dest)
        .await
        .context("copying /etc/resolv.conf into chroot")?;

    // Install packages via chroot
    let result = async {
        // Update apt cache (universe already enabled in base cloud image)
        info!("running apt-get update in chroot");
        let output = Command::new("chroot")
            .arg(path_to_str(&mount_point)?)
            .args(["apt-get", "update", "-y"])
            .output()
            .await
            .context("running apt-get update in chroot")?;

        // apt-get update completed successfully - no need to log verbose output

        if !output.status.success() {
            bail!(
                "apt-get update failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Install packages (with verbose output)
        info!("installing packages: podman crun fuse-overlayfs fuse3 haveged chrony");
        info!("package installation typically takes 30-60 seconds");

        let output = Command::new("chroot")
            .arg(path_to_str(&mount_point)?)
            .env("DEBIAN_FRONTEND", "noninteractive")
            .args([
                "apt-get",
                "install",
                "-y",
                "-o",
                "Dpkg::Options::=--force-confnew", // Force install new config files
                "podman",
                "crun",
                "fuse-overlayfs",
                "fuse3",
                "haveged",
                "chrony",
            ])
            .output()
            .await
            .context("installing packages in chroot")?;

        // Log apt output for debugging
        info!(
            "apt-get install stdout:\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
        if !output.stderr.is_empty() {
            info!(
                "apt-get install stderr:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        if !output.status.success() {
            bail!(
                "apt-get install failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Enable services
        let output = Command::new("chroot")
            .arg(path_to_str(&mount_point)?)
            .args(["systemctl", "enable", "haveged", "chrony"])
            .output()
            .await
            .context("enabling services in chroot")?;

        if !output.status.success() {
            bail!(
                "systemctl enable failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Configure Podman registries (after packages installed to avoid conffile conflict)
        info!("configuring Podman container registries");
        let registries_conf_path = mount_point.join("etc/containers/registries.conf");
        let registries_content = "unqualified-search-registries = [\"docker.io\"]\n\n\
                                  [[registry]]\n\
                                  location = \"docker.io\"\n";
        tokio::fs::write(&registries_conf_path, registries_content)
            .await
            .context("writing registries.conf")?;

        // Write initial resolv.conf - will be overwritten by fcvm-setup-dns.service at boot
        // The startup script extracts gateway IP from kernel cmdline and configures DNS
        info!("configuring initial resolv.conf (will be updated at boot)");
        let resolv_conf_path = mount_point.join("etc/resolv.conf");
        tokio::fs::write(
            &resolv_conf_path,
            "# Placeholder - fcvm-setup-dns.service configures DNS at boot from kernel cmdline\nnameserver 127.0.0.53\n",
        )
        .await
        .context("writing resolv.conf")?;

        Ok(())
    }
    .await;

    // Always unmount (in reverse order)
    for target in ["dev/pts", "dev", "sys", "proc", ""] {
        let target_path = if target.is_empty() {
            mount_point.clone()
        } else {
            mount_point.join(target)
        };
        let _ = Command::new("umount")
            .arg(path_to_str(&target_path).unwrap_or(""))
            .output()
            .await;
    }

    // Cleanup
    let _ = tokio::fs::remove_dir_all(&temp_dir).await;

    result?;

    info!("packages installed successfully");

    Ok(())
}
