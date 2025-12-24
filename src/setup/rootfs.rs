use anyhow::{bail, Context, Result};
use nix::fcntl::{Flock, FlockArg};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::paths;

/// Plan file location (relative to workspace root)
const PLAN_FILE: &str = "rootfs-plan.toml";

/// Size of the Layer 2 disk image
const LAYER2_SIZE: &str = "10G";

// ============================================================================
// Plan File Data Structures
// ============================================================================

#[derive(Debug, Deserialize, Clone)]
pub struct Plan {
    pub base: BaseConfig,
    pub kernel: KernelConfig,
    pub packages: PackagesConfig,
    pub services: ServicesConfig,
    pub files: HashMap<String, FileConfig>,
    pub fstab: FstabConfig,
    #[serde(default)]
    pub cleanup: CleanupConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct BaseConfig {
    pub version: String,
    pub arm64: ArchConfig,
    pub amd64: ArchConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ArchConfig {
    pub url: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct KernelConfig {
    pub arm64: KernelArchConfig,
    pub amd64: KernelArchConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct KernelArchConfig {
    /// URL to the kernel archive (e.g., Kata release tarball)
    pub url: String,
    /// Path within the archive to extract
    pub path: String,
}

impl KernelConfig {
    /// Get the kernel config for the current architecture
    pub fn current_arch(&self) -> anyhow::Result<&KernelArchConfig> {
        match std::env::consts::ARCH {
            "x86_64" => Ok(&self.amd64),
            "aarch64" => Ok(&self.arm64),
            other => anyhow::bail!("unsupported architecture: {}", other),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct PackagesConfig {
    pub runtime: Vec<String>,
    pub fuse: Vec<String>,
    pub system: Vec<String>,
    #[serde(default)]
    pub debug: Vec<String>,
}

impl PackagesConfig {
    pub fn all_packages(&self) -> Vec<&str> {
        self.runtime
            .iter()
            .chain(&self.fuse)
            .chain(&self.system)
            .chain(&self.debug)
            .map(|s| s.as_str())
            .collect()
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServicesConfig {
    pub enable: Vec<String>,
    pub disable: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct FileConfig {
    pub content: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct FstabConfig {
    pub remove_patterns: Vec<String>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct CleanupConfig {
    #[serde(default)]
    pub remove_dirs: Vec<String>,
}

// ============================================================================
// Script Generation
// ============================================================================

/// Generate a setup script from the plan
///
/// Generate the install script that runs BEFORE the setup script.
/// This script installs packages from /mnt/packages and removes conflicting packages.
pub fn generate_install_script() -> String {
    r#"#!/bin/bash
set -e
echo 'FCVM: Removing conflicting packages before install...'
# Remove time-daemon provider that conflicts with chrony
apt-get remove -y --purge systemd-timesyncd 2>/dev/null || true
# Remove packages we don't need in microVM (also frees space)
apt-get remove -y --purge cloud-init snapd ubuntu-server 2>/dev/null || true

echo 'FCVM: Installing packages from initrd...'
dpkg -i /mnt/packages/*.deb || true
apt-get -f install -y || true
echo 'FCVM: Packages installed successfully'
"#
    .to_string()
}

/// Generate the init script that runs in the initrd during Layer 2 setup.
/// This script mounts filesystems, runs install + setup scripts, then powers off.
///
/// The SHA256 of this complete script determines the rootfs name, ensuring
/// any changes to mounts, commands, or embedded scripts invalidate the cache.
pub fn generate_init_script(install_script: &str, setup_script: &str) -> String {
    format!(
        r#"#!/bin/busybox sh
# FCVM Layer 2 setup initrd
# Runs package installation before systemd
# Packages are embedded in the initrd at /packages

echo "FCVM Layer 2 Setup: Starting..."

# Install busybox commands
/bin/busybox mkdir -p /bin /sbin /proc /sys /dev /newroot
/bin/busybox --install -s /bin
/bin/busybox --install -s /sbin

# Mount essential filesystems
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev

# Populate /dev with device nodes from sysfs
mdev -s

# Debug: show available block devices
echo "FCVM Layer 2 Setup: Available block devices:"
ls -la /dev/vd* 2>/dev/null || echo "No /dev/vd* devices found"

echo "FCVM Layer 2 Setup: Mounting rootfs..."
mount -o rw /dev/vda /newroot
if [ $? -ne 0 ]; then
    echo "ERROR: Failed to mount rootfs"
    sleep 5
    poweroff -f
fi

# Copy embedded packages from initrd to rootfs
# Packages are in /packages directory inside the initrd (loaded in RAM)
echo "FCVM Layer 2 Setup: Copying packages from initrd to rootfs..."
mkdir -p /newroot/mnt/packages
cp -a /packages/* /newroot/mnt/packages/
echo "FCVM Layer 2 Setup: Copied $(ls /newroot/mnt/packages/*.deb 2>/dev/null | wc -l) packages"

# Write the install script to rootfs
cat > /newroot/tmp/install-packages.sh << 'INSTALL_SCRIPT_EOF'
{}
INSTALL_SCRIPT_EOF
chmod 755 /newroot/tmp/install-packages.sh

# Write the setup script to rootfs
cat > /newroot/tmp/fcvm-setup.sh << 'SETUP_SCRIPT_EOF'
{}
SETUP_SCRIPT_EOF
chmod 755 /newroot/tmp/fcvm-setup.sh

# Set up chroot environment (proc, sys, dev)
echo "FCVM Layer 2 Setup: Setting up chroot environment..."
mount --bind /proc /newroot/proc
mount --bind /sys /newroot/sys
mount --bind /dev /newroot/dev

# Install packages using chroot
echo "FCVM Layer 2 Setup: Installing packages..."
chroot /newroot /bin/bash /tmp/install-packages.sh
INSTALL_RESULT=$?
echo "FCVM Layer 2 Setup: Package installation returned: $INSTALL_RESULT"

# Run setup script using chroot
echo "FCVM Layer 2 Setup: Running setup script..."
chroot /newroot /bin/bash /tmp/fcvm-setup.sh
SETUP_RESULT=$?
echo "FCVM Layer 2 Setup: Setup script returned: $SETUP_RESULT"

# Cleanup chroot mounts (use lazy unmount as fallback)
echo "FCVM Layer 2 Setup: Cleaning up..."
umount /newroot/dev 2>/dev/null || umount -l /newroot/dev 2>/dev/null || true
umount /newroot/sys 2>/dev/null || umount -l /newroot/sys 2>/dev/null || true
umount /newroot/proc 2>/dev/null || umount -l /newroot/proc 2>/dev/null || true
rm -rf /newroot/mnt/packages
rm -f /newroot/tmp/install-packages.sh
rm -f /newroot/tmp/fcvm-setup.sh

# Sync and unmount rootfs
sync
umount /newroot 2>/dev/null || umount -l /newroot 2>/dev/null || true

echo "FCVM_SETUP_COMPLETE"
echo "FCVM Layer 2 Setup: Complete! Powering off..."
umount /proc /sys /dev 2>/dev/null || true
poweroff -f
"#,
        install_script, setup_script
    )
}

/// The script content is deterministic - same plan always produces same script.
/// The SHA256 of this script determines the rootfs image name.
///
/// NOTE: This script does NOT install packages - they are installed from
/// install-packages.sh before this script runs.
pub fn generate_setup_script(plan: &Plan) -> String {
    let mut s = String::new();

    // Script header - runs after packages are installed from initrd
    s.push_str("#!/bin/bash\n");
    s.push_str("set -euo pipefail\n\n");

    // Note: No partition resize needed - filesystem is already resized on host
    // (we use a raw ext4 filesystem without partition table)\n

    // Note: Packages are already installed by install-packages.sh
    // We just need to include the package list in the script for SHA calculation
    let packages = plan.packages.all_packages();
    s.push_str("# Packages (installed from initrd): ");
    s.push_str(&packages.join(", "));
    s.push_str("\n\n");

    // Write configuration files (sorted for deterministic output)
    let mut file_paths: Vec<_> = plan.files.keys().collect();
    file_paths.sort();

    s.push_str("# Write configuration files\n");
    for path in file_paths {
        let config = &plan.files[path];
        // Create parent directory if needed
        if let Some(parent) = std::path::Path::new(path).parent() {
            if parent != std::path::Path::new("") && parent != std::path::Path::new("/") {
                s.push_str(&format!("mkdir -p {}\n", parent.display()));
            }
        }
        s.push_str(&format!("cat > {} << 'FCVM_EOF'\n", path));
        s.push_str(&config.content);
        if !config.content.ends_with('\n') {
            s.push('\n');
        }
        s.push_str("FCVM_EOF\n\n");
    }

    // Fix fstab (remove problematic entries)
    if !plan.fstab.remove_patterns.is_empty() {
        s.push_str("# Fix /etc/fstab\n");
        for pattern in &plan.fstab.remove_patterns {
            // Use sed to remove lines containing the pattern
            s.push_str(&format!(
                "sed -i '/{}/d' /etc/fstab\n",
                pattern.replace('/', "\\/")
            ));
        }
        s.push('\n');
    }

    // Configure container registries
    s.push_str("# Configure Podman registries\n");
    s.push_str("cat > /etc/containers/registries.conf << 'FCVM_EOF'\n");
    s.push_str("unqualified-search-registries = [\"docker.io\"]\n\n");
    s.push_str("[[registry]]\n");
    s.push_str("location = \"docker.io\"\n");
    s.push_str("FCVM_EOF\n\n");

    // Enable services
    if !plan.services.enable.is_empty() {
        s.push_str("# Enable services\n");
        s.push_str("systemctl enable");
        for svc in &plan.services.enable {
            s.push_str(&format!(" {}", svc));
        }
        s.push('\n');
    }

    // Also enable serial console
    s.push_str("systemctl enable serial-getty@ttyS0\n\n");

    // Disable services
    if !plan.services.disable.is_empty() {
        s.push_str("# Disable services\n");
        s.push_str("systemctl disable");
        for svc in &plan.services.disable {
            s.push_str(&format!(" {}", svc));
        }
        s.push_str(" || true\n\n");
    }

    // Cleanup
    if !plan.cleanup.remove_dirs.is_empty() {
        s.push_str("# Cleanup unnecessary files\n");
        for pattern in &plan.cleanup.remove_dirs {
            s.push_str(&format!("rm -rf {}\n", pattern));
        }
        s.push('\n');
    }

    // Clean apt cache for smaller image
    s.push_str("# Clean apt cache\n");
    s.push_str("apt-get clean\n");
    s.push_str("rm -rf /var/lib/apt/lists/*\n\n");

    s.push_str("echo 'FCVM_SETUP_COMPLETE'\n");
    s.push_str("# Shutdown to signal completion\n");
    s.push_str("shutdown -h now\n");
    s
}

// ============================================================================
// Plan Loading and SHA256
// ============================================================================

/// Find the plan file in the workspace
fn find_plan_file() -> Result<PathBuf> {
    // Try relative to current exe (for installed binary)
    let exe_path = std::env::current_exe().context("getting current executable path")?;
    let exe_dir = exe_path.parent().context("getting executable directory")?;

    // Check various locations
    let candidates = [
        exe_dir.join(PLAN_FILE),
        exe_dir.join("..").join(PLAN_FILE),
        exe_dir.join("../..").join(PLAN_FILE),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(PLAN_FILE),
    ];

    for path in &candidates {
        if path.exists() {
            return path.canonicalize().context("canonicalizing plan file path");
        }
    }

    // Fallback to CARGO_MANIFEST_DIR for development
    let manifest_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(PLAN_FILE);
    if manifest_path.exists() {
        return Ok(manifest_path);
    }

    bail!(
        "rootfs-plan.toml not found. Checked: {:?}",
        candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
    )
}

/// Load and parse the plan file
pub fn load_plan() -> Result<(Plan, String, String)> {
    let plan_path = find_plan_file()?;
    let plan_content = std::fs::read_to_string(&plan_path)
        .with_context(|| format!("reading plan file: {}", plan_path.display()))?;

    // Compute SHA256 of plan content (first 12 chars for image naming)
    let plan_sha = compute_sha256(plan_content.as_bytes());
    let plan_sha_short = plan_sha[..12].to_string();

    let plan: Plan = toml::from_str(&plan_content)
        .with_context(|| format!("parsing plan file: {}", plan_path.display()))?;

    info!(
        plan_file = %plan_path.display(),
        plan_sha = %plan_sha_short,
        "loaded rootfs plan"
    );

    Ok((plan, plan_sha, plan_sha_short))
}

/// Compute SHA256 of bytes, return hex string
pub fn compute_sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

// ============================================================================
// Public API
// ============================================================================

/// Ensure rootfs exists, creating if needed (NO ROOT REQUIRED)
///
/// The rootfs is named after the generated setup script SHA256: layer2-{script_sha}.raw
/// If the script changes (due to plan changes), a new rootfs is created automatically.
///
/// Layer 2 creation flow (all rootless):
/// 1. Download Ubuntu cloud image (qcow2)
/// 2. Convert to raw with qemu-img
/// 3. Expand to 10GB with truncate
/// 4. Download packages
/// 5. Create initrd with embedded packages
/// 6. Boot VM with initrd to install packages (no network needed)
/// 6. Wait for VM to shut down
/// 7. Rename to layer2-{sha}.raw
///
/// NOTE: fc-agent is NOT included in Layer 2. It will be injected per-VM at boot time.
/// Layer 2 only contains packages (podman, crun, etc.).
pub async fn ensure_rootfs() -> Result<PathBuf> {
    let (plan, _plan_sha_full, _plan_sha_short) = load_plan()?;

    // Generate all scripts and compute hash of the complete init script
    let setup_script = generate_setup_script(&plan);
    let install_script = generate_install_script();
    let init_script = generate_init_script(&install_script, &setup_script);

    // Get kernel URL for the current architecture
    let kernel_config = plan.kernel.current_arch()?;
    let kernel_url = &kernel_config.url;

    // Hash the complete init script + kernel URL
    // Any change to:
    // - init logic, install script, or setup script
    // - kernel URL (different kernel version/release)
    // invalidates the cache
    let mut combined = init_script.clone();
    combined.push_str("\n# KERNEL_URL: ");
    combined.push_str(kernel_url);
    let script_sha = compute_sha256(combined.as_bytes());
    let script_sha_short = &script_sha[..12];

    let rootfs_dir = paths::rootfs_dir();
    let rootfs_path = rootfs_dir.join(format!("layer2-{}.raw", script_sha_short));
    let lock_file = rootfs_dir.join(".rootfs-creation.lock");

    // If rootfs exists for this script, return it
    if rootfs_path.exists() {
        info!(
            path = %rootfs_path.display(),
            script_sha = %script_sha_short,
            "rootfs exists for current script (using cached)"
        );
        return Ok(rootfs_path);
    }

    // Create directory for lock file
    tokio::fs::create_dir_all(&rootfs_dir)
        .await
        .context("creating rootfs directory")?;

    // Acquire lock to prevent concurrent rootfs creation
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

    // Check again after acquiring lock
    if rootfs_path.exists() {
        info!(
            path = %rootfs_path.display(),
            "rootfs exists (created by another process)"
        );
        flock.unlock().map_err(|(_, err)| err).ok();
        let _ = std::fs::remove_file(&lock_file);
        return Ok(rootfs_path);
    }

    // Create the rootfs
    info!(
        script_sha = %script_sha_short,
        "creating Layer 2 rootfs (first-time may take 5-15 minutes)"
    );

    // Log the generated script for debugging
    debug!("generated setup script:\n{}", setup_script);

    let temp_rootfs_path = rootfs_path.with_extension("raw.tmp");
    let _ = tokio::fs::remove_file(&temp_rootfs_path).await;

    let result =
        create_layer2_rootless(&plan, script_sha_short, &setup_script, &temp_rootfs_path).await;

    if result.is_ok() {
        tokio::fs::rename(&temp_rootfs_path, &rootfs_path)
            .await
            .context("renaming temp rootfs to final path")?;
        info!(
            path = %rootfs_path.display(),
            script_sha = %script_sha_short,
            "Layer 2 rootfs creation complete"
        );
    } else {
        let _ = tokio::fs::remove_file(&temp_rootfs_path).await;
    }

    // Release lock
    flock
        .unlock()
        .map_err(|(_, err)| err)
        .context("releasing rootfs creation lock")?;
    let _ = std::fs::remove_file(&lock_file);

    result?;
    Ok(rootfs_path)
}

/// Find the fc-agent binary for per-VM injection
///
/// fc-agent is NOT included in Layer 2 (the base rootfs). Instead, it is
/// injected per-VM at boot time via initrd. This function is used to locate
/// the binary for that injection.
///
/// Both fcvm and fc-agent are workspace members built together.
/// Search order:
/// 1. Same directory as current exe
/// 2. Parent directory (for tests in target/release/deps/)
/// 3. FC_AGENT_PATH environment variable
pub fn find_fc_agent_binary() -> Result<PathBuf> {
    let exe_path = std::env::current_exe().context("getting current executable path")?;
    let exe_dir = exe_path.parent().context("getting executable directory")?;

    // Check same directory
    let fc_agent = exe_dir.join("fc-agent");
    if fc_agent.exists() {
        return Ok(fc_agent);
    }

    // Check parent directory (test case)
    if let Some(parent) = exe_dir.parent() {
        let fc_agent_parent = parent.join("fc-agent");
        if fc_agent_parent.exists() {
            return Ok(fc_agent_parent);
        }
    }

    // Fallback: environment variable
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

// ============================================================================
// fc-agent Initrd Creation
// ============================================================================

/// The fc-agent systemd service unit file content
/// Supports optional strace via kernel cmdline parameter fc_agent_strace=1
const FC_AGENT_SERVICE: &str = r#"[Unit]
Description=fcvm guest agent for container orchestration
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/fc-agent
Restart=on-failure
RestartSec=1
# Send stdout/stderr to serial console so fcvm host can see fc-agent logs
StandardOutput=journal+console
StandardError=journal+console

[Install]
WantedBy=multi-user.target
"#;

/// The fc-agent systemd service unit file with strace enabled
const FC_AGENT_SERVICE_STRACE: &str = r#"[Unit]
Description=fcvm guest agent for container orchestration (with strace)
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/fc-agent-strace-wrapper
Restart=on-failure
RestartSec=1
# Send stdout/stderr to serial console so fcvm host can see fc-agent logs
StandardOutput=journal+console
StandardError=journal+console

[Install]
WantedBy=multi-user.target
"#;

/// The init script for the initrd
/// This runs before the real init, copies fc-agent to the rootfs, then switches root
const INITRD_INIT_SCRIPT: &str = r#"#!/bin/busybox sh
# fc-agent injection initrd
# This runs before systemd, copies fc-agent to the rootfs, then switch_root

# Install busybox applets
/bin/busybox mkdir -p /bin /sbin /proc /sys /dev /newroot
/bin/busybox --install -s /bin
/bin/busybox --install -s /sbin

# Mount essential filesystems
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev

# Parse kernel cmdline to find root device and debug flags
ROOT=""
FC_AGENT_STRACE=""
for param in $(cat /proc/cmdline); do
    case "$param" in
        root=*)
            ROOT="${param#root=}"
            ;;
        fc_agent_strace=1)
            FC_AGENT_STRACE="1"
            echo "fc-agent strace debugging ENABLED"
            ;;
    esac
done

if [ -z "$ROOT" ]; then
    echo "ERROR: No root= parameter found in kernel cmdline"
    exec /bin/sh
fi

# Handle /dev/vda1 style paths
case "$ROOT" in
    /dev/*)
        # Wait for device to appear
        for i in 1 2 3 4 5; do
            if [ -b "$ROOT" ]; then
                break
            fi
            echo "Waiting for $ROOT..."
            sleep 1
        done
        ;;
esac

# Mount the real root filesystem
echo "Mounting $ROOT as real root..."
mount -o rw "$ROOT" /newroot

if [ ! -d /newroot/usr ]; then
    echo "ERROR: Failed to mount root filesystem"
    exec /bin/sh
fi

# Copy fc-agent binary
echo "Installing fc-agent..."
cp /fc-agent /newroot/usr/local/bin/fc-agent
chmod 755 /newroot/usr/local/bin/fc-agent

# Copy service file (use strace version if debugging enabled)
if [ -n "$FC_AGENT_STRACE" ]; then
    echo "Installing fc-agent with strace wrapper..."
    cp /fc-agent.service.strace /newroot/etc/systemd/system/fc-agent.service
    # Create wrapper script that tees strace to both file and serial console
    cat > /newroot/usr/local/bin/fc-agent-strace-wrapper << 'STRACE_WRAPPER'
#!/bin/bash
# Write strace output to both file and serial console (/dev/console)
# This ensures we see crash info in Firecracker serial output
exec strace -f -o >(tee /tmp/fc-agent.strace > /dev/console 2>&1) /usr/local/bin/fc-agent "$@"
STRACE_WRAPPER
    chmod 755 /newroot/usr/local/bin/fc-agent-strace-wrapper
else
    cp /fc-agent.service /newroot/etc/systemd/system/fc-agent.service
fi

# Enable the service (create symlink)
mkdir -p /newroot/etc/systemd/system/multi-user.target.wants
ln -sf ../fc-agent.service /newroot/etc/systemd/system/multi-user.target.wants/fc-agent.service

echo "fc-agent installed successfully"

# Also ensure MMDS route config exists (in case setup script failed)
mkdir -p /newroot/etc/systemd/network/10-eth0.network.d
if [ ! -f /newroot/etc/systemd/network/10-eth0.network.d/mmds.conf ]; then
    echo "Adding MMDS route config..."
    cat > /newroot/etc/systemd/network/10-eth0.network.d/mmds.conf << 'MMDSCONF'
[Route]
Destination=169.254.169.254/32
Scope=link
MMDSCONF
fi

# Also create the base network config if missing
if [ ! -f /newroot/etc/systemd/network/10-eth0.network ]; then
    echo "Adding base network config..."
    cat > /newroot/etc/systemd/network/10-eth0.network << 'NETCONF'
[Match]
Name=eth0

[Network]
KeepConfiguration=yes
NETCONF
fi

# Cleanup
umount /proc
umount /sys
umount /dev

# Switch to the real root and exec init
exec switch_root /newroot /sbin/init
"#;

/// Ensure the fc-agent initrd exists, creating if needed
///
/// The initrd is cached by a combined hash of:
/// - fc-agent binary
/// - init script content (INITRD_INIT_SCRIPT)
/// - service file content (FC_AGENT_SERVICE, FC_AGENT_SERVICE_STRACE)
///
/// This ensures the initrd is regenerated when any of these change.
///
/// Returns the path to the initrd file.
///
/// Uses file locking to prevent race conditions when multiple VMs start
/// simultaneously and all try to create the initrd.
pub async fn ensure_fc_agent_initrd() -> Result<PathBuf> {
    // Find fc-agent binary
    let fc_agent_path = find_fc_agent_binary()?;
    let fc_agent_bytes = std::fs::read(&fc_agent_path)
        .with_context(|| format!("reading fc-agent binary at {}", fc_agent_path.display()))?;

    // Compute combined hash of all initrd contents
    let mut combined = fc_agent_bytes.clone();
    combined.extend_from_slice(INITRD_INIT_SCRIPT.as_bytes());
    combined.extend_from_slice(FC_AGENT_SERVICE.as_bytes());
    combined.extend_from_slice(FC_AGENT_SERVICE_STRACE.as_bytes());
    let initrd_sha = compute_sha256(&combined);
    let initrd_sha_short = &initrd_sha[..12];

    // Check if initrd already exists for this version (fast path, no lock)
    let initrd_dir = paths::base_dir().join("initrd");
    let initrd_path = initrd_dir.join(format!("fc-agent-{}.initrd", initrd_sha_short));

    if initrd_path.exists() {
        debug!(
            path = %initrd_path.display(),
            initrd_sha = %initrd_sha_short,
            "using cached fc-agent initrd"
        );
        return Ok(initrd_path);
    }

    // Create initrd directory (needed for lock file)
    tokio::fs::create_dir_all(&initrd_dir)
        .await
        .context("creating initrd directory")?;

    // Acquire exclusive lock to prevent race conditions
    let lock_file = initrd_dir.join(format!("fc-agent-{}.lock", initrd_sha_short));
    use std::os::unix::fs::OpenOptionsExt;
    let lock_fd = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&lock_file)
        .context("opening initrd lock file")?;

    let flock = Flock::lock(lock_fd, FlockArg::LockExclusive)
        .map_err(|(_, err)| err)
        .context("acquiring exclusive lock for initrd creation")?;

    // Double-check after acquiring lock - another process may have created it
    if initrd_path.exists() {
        debug!(
            path = %initrd_path.display(),
            initrd_sha = %initrd_sha_short,
            "using cached fc-agent initrd (created by another process)"
        );
        flock
            .unlock()
            .map_err(|(_, err)| err)
            .context("releasing initrd lock")?;
        return Ok(initrd_path);
    }

    info!(
        fc_agent = %fc_agent_path.display(),
        initrd_sha = %initrd_sha_short,
        "creating fc-agent initrd"
    );

    // Create temporary directory for initrd contents
    // Use PID in temp dir name to avoid conflicts even with same sha
    let temp_dir = initrd_dir.join(format!(
        ".initrd-build-{}-{}",
        initrd_sha_short,
        std::process::id()
    ));
    let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    tokio::fs::create_dir_all(&temp_dir).await?;

    // Create directory structure
    for dir in &["bin", "sbin", "dev", "proc", "sys", "newroot"] {
        tokio::fs::create_dir_all(temp_dir.join(dir)).await?;
    }

    // Find busybox (prefer static version)
    let busybox_path = find_busybox()?;

    // Copy busybox
    tokio::fs::copy(&busybox_path, temp_dir.join("bin/busybox")).await?;

    // Make busybox executable
    Command::new("chmod")
        .args(["755", temp_dir.join("bin/busybox").to_str().unwrap()])
        .output()
        .await?;

    // Write init script
    tokio::fs::write(temp_dir.join("init"), INITRD_INIT_SCRIPT).await?;
    Command::new("chmod")
        .args(["755", temp_dir.join("init").to_str().unwrap()])
        .output()
        .await?;

    // Copy fc-agent binary
    tokio::fs::copy(&fc_agent_path, temp_dir.join("fc-agent")).await?;
    Command::new("chmod")
        .args(["755", temp_dir.join("fc-agent").to_str().unwrap()])
        .output()
        .await?;

    // Write service files (normal and strace version)
    tokio::fs::write(temp_dir.join("fc-agent.service"), FC_AGENT_SERVICE).await?;
    tokio::fs::write(
        temp_dir.join("fc-agent.service.strace"),
        FC_AGENT_SERVICE_STRACE,
    )
    .await?;

    // Create cpio archive (initrd format)
    // Use bash with pipefail so cpio errors aren't masked by gzip success (v3)
    let temp_initrd = initrd_path.with_extension("initrd.tmp");
    let output = Command::new("bash")
        .args([
            "-c",
            &format!(
                "set -o pipefail && cd {} && find . | cpio -o -H newc | gzip > {}",
                temp_dir.display(),
                temp_initrd.display()
            ),
        ])
        .output()
        .await
        .context("creating initrd cpio archive")?;

    if !output.status.success() {
        // Release lock before bailing
        let _ = flock.unlock();
        bail!(
            "Failed to create initrd: stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Rename to final path (atomic)
    tokio::fs::rename(&temp_initrd, &initrd_path).await?;

    // Cleanup temp directory
    let _ = tokio::fs::remove_dir_all(&temp_dir).await;

    info!(
        path = %initrd_path.display(),
        initrd_sha = %initrd_sha_short,
        "fc-agent initrd created"
    );

    // Release lock (file created successfully)
    flock
        .unlock()
        .map_err(|(_, err)| err)
        .context("releasing initrd lock after creation")?;

    Ok(initrd_path)
}

/// Find busybox binary (prefer static version)
fn find_busybox() -> Result<PathBuf> {
    // Check for busybox-static first
    for path in &[
        "/bin/busybox-static",
        "/usr/bin/busybox-static",
        "/bin/busybox",
        "/usr/bin/busybox",
    ] {
        let p = PathBuf::from(path);
        if p.exists() {
            return Ok(p);
        }
    }

    // Try which
    if let Ok(output) = std::process::Command::new("which").arg("busybox").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(PathBuf::from(path));
            }
        }
    }

    bail!("busybox not found. Install with: apt-get install busybox-static")
}

// ============================================================================
// Layer 2 Creation (Rootless)
// ============================================================================

/// Create Layer 2 rootfs without requiring root
///
/// 1. Download cloud image (qcow2, cached)
/// 2. Convert to raw with qemu-img (no root)
/// 3. Expand to 10GB (no root)
/// 4. Download .deb packages on host (has network)
/// 5. Create initrd with embedded packages
/// 6. Boot VM with initrd to install packages (no network needed)
/// 7. Wait for VM to shut down
///
/// NOTE: fc-agent is NOT included - it will be injected per-VM at boot time.
async fn create_layer2_rootless(
    plan: &Plan,
    script_sha_short: &str,
    script: &str,
    output_path: &Path,
) -> Result<()> {
    // Step 1: Download cloud image (cached by URL)
    let cloud_image = download_cloud_image(plan).await?;

    // Step 2: Convert qcow2 to raw (no root required!)
    info!("converting qcow2 to raw format (no root required)");
    let full_disk_path = output_path.with_extension("full");
    let output = Command::new("qemu-img")
        .args([
            "convert",
            "-f",
            "qcow2",
            "-O",
            "raw",
            path_to_str(&cloud_image)?,
            path_to_str(&full_disk_path)?,
        ])
        .output()
        .await
        .context("running qemu-img convert")?;

    if !output.status.success() {
        bail!(
            "qemu-img convert failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Step 3: Extract partition 1 (root filesystem) using fdisk and dd
    // This avoids GPT partition table issues with Firecracker
    info!("extracting root partition from GPT disk (no root required)");
    let partition_path = output_path.with_extension("converting");

    // Get partition info using sfdisk
    let output = Command::new("sfdisk")
        .args(["-J", path_to_str(&full_disk_path)?])
        .output()
        .await
        .context("getting partition info")?;

    if !output.status.success() {
        bail!("sfdisk failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    // Parse sfdisk JSON output to find partition 1
    #[derive(serde::Deserialize)]
    struct SfdiskOutput {
        partitiontable: PartitionTable,
    }
    #[derive(serde::Deserialize)]
    struct PartitionTable {
        partitions: Vec<Partition>,
    }
    #[derive(serde::Deserialize)]
    struct Partition {
        node: String,
        start: u64,
        size: u64,
        #[serde(rename = "type")]
        ptype: String,
    }

    let sfdisk_output: SfdiskOutput =
        serde_json::from_slice(&output.stdout).context("parsing sfdisk JSON output")?;

    // Find the Linux filesystem partition (type ends with 0FC63DAF-8483-4772-8E79-3D69D8477DE4 or similar)
    let root_part = sfdisk_output
        .partitiontable
        .partitions
        .iter()
        .find(|p| p.ptype.contains("0FC63DAF") || p.node.ends_with("1"))
        .ok_or_else(|| anyhow::anyhow!("Could not find root partition in GPT disk"))?;

    info!(
        partition = %root_part.node,
        start_sector = root_part.start,
        size_sectors = root_part.size,
        "found root partition"
    );

    // Extract partition using dd (sector size is 512 bytes)
    let output = Command::new("dd")
        .args([
            &format!("if={}", path_to_str(&full_disk_path)?),
            &format!("of={}", path_to_str(&partition_path)?),
            "bs=512",
            &format!("skip={}", root_part.start),
            &format!("count={}", root_part.size),
            "status=progress",
        ])
        .output()
        .await
        .context("extracting partition with dd")?;

    if !output.status.success() {
        bail!("dd failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    // Remove full disk image (no longer needed)
    let _ = tokio::fs::remove_file(&full_disk_path).await;

    // Step 4: Expand the extracted partition to 10GB
    info!("expanding partition to {}", LAYER2_SIZE);
    let output = Command::new("truncate")
        .args(["-s", LAYER2_SIZE, path_to_str(&partition_path)?])
        .output()
        .await
        .context("expanding partition")?;

    if !output.status.success() {
        bail!(
            "truncate failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Resize the ext4 filesystem to fill the partition
    info!("resizing ext4 filesystem");
    let _output = Command::new("e2fsck")
        .args(["-f", "-y", path_to_str(&partition_path)?])
        .output()
        .await
        .context("running e2fsck")?;
    // e2fsck may return non-zero even on success (exit code 1 = errors corrected)

    let output = Command::new("resize2fs")
        .args([path_to_str(&partition_path)?])
        .output()
        .await
        .context("running resize2fs")?;

    if !output.status.success() {
        bail!(
            "resize2fs failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Step 4b: Fix /etc/fstab to remove BOOT and UEFI entries
    // This MUST happen before booting - systemd reads fstab before cloud-init runs
    info!("fixing /etc/fstab to remove non-existent partition entries");
    fix_fstab_in_image(&partition_path).await?;

    // Step 5: Download packages on host (host has network!)
    let packages_dir = download_packages(plan, script_sha_short).await?;

    // Step 6: Create initrd for Layer 2 setup with embedded packages
    // The initrd runs before systemd and:
    // - Mounts rootfs at /newroot
    // - Copies packages from initrd to rootfs
    // - Runs dpkg -i to install packages
    // - Runs the setup script
    // - Powers off
    // Packages are embedded in the initrd (no second disk needed)
    let install_script = generate_install_script();

    let setup_initrd = create_layer2_setup_initrd(&install_script, script, &packages_dir).await?;

    // Step 7: Boot VM with initrd to run setup (no cloud-init needed!)
    // Now we boot a pure ext4 partition (no GPT), so root=/dev/vda works
    // Only one disk needed - packages are in the initrd
    info!(
        script_sha = %script_sha_short,
        "booting VM with setup initrd (packages embedded)"
    );

    boot_vm_for_setup(&partition_path, &setup_initrd).await?;

    // Step 8: Rename to final path
    tokio::fs::rename(&partition_path, output_path)
        .await
        .context("renaming partition to output path")?;

    info!("Layer 2 creation complete (packages embedded in initrd)");
    Ok(())
}

/// Fix /etc/fstab in an ext4 image to remove BOOT and UEFI partition entries
///
/// The Ubuntu cloud image has fstab entries for LABEL=BOOT and LABEL=UEFI
/// which cause systemd to enter emergency mode when these partitions don't exist.
/// We use debugfs to modify fstab directly in the ext4 image without mounting.
async fn fix_fstab_in_image(image_path: &Path) -> Result<()> {
    // Read current fstab using debugfs
    let output = Command::new("debugfs")
        .args(["-R", "cat /etc/fstab", path_to_str(image_path)?])
        .output()
        .await
        .context("reading fstab with debugfs")?;

    if !output.status.success() {
        bail!(
            "debugfs read failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let fstab_content = String::from_utf8_lossy(&output.stdout);

    // Filter out BOOT and UEFI entries
    let new_fstab: String = fstab_content
        .lines()
        .filter(|line| !line.contains("LABEL=BOOT") && !line.contains("LABEL=UEFI"))
        .collect::<Vec<_>>()
        .join("\n");

    debug!("new fstab content:\n{}", new_fstab);

    // Write new fstab to a temp file
    let temp_fstab = std::env::temp_dir().join("fstab.new");
    tokio::fs::write(&temp_fstab, format!("{}\n", new_fstab))
        .await
        .context("writing temp fstab")?;

    // Write the new fstab back using debugfs -w
    // debugfs command: rm /etc/fstab; write /tmp/fstab.new /etc/fstab
    let output = Command::new("debugfs")
        .args(["-w", "-R", "rm /etc/fstab", path_to_str(image_path)?])
        .output()
        .await
        .context("removing old fstab with debugfs")?;

    // rm might fail if file doesn't exist, that's OK
    if !output.status.success() {
        debug!(
            "debugfs rm fstab (might be expected): {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let output = Command::new("debugfs")
        .args([
            "-w",
            "-R",
            &format!("write {} /etc/fstab", temp_fstab.display()),
            path_to_str(image_path)?,
        ])
        .output()
        .await
        .context("writing new fstab with debugfs")?;

    if !output.status.success() {
        bail!(
            "debugfs write failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Cleanup temp file
    let _ = tokio::fs::remove_file(&temp_fstab).await;

    // Verify the change
    let output = Command::new("debugfs")
        .args(["-R", "cat /etc/fstab", path_to_str(image_path)?])
        .output()
        .await
        .context("verifying fstab with debugfs")?;

    let new_content = String::from_utf8_lossy(&output.stdout);
    if new_content.contains("LABEL=BOOT") || new_content.contains("LABEL=UEFI") {
        warn!("fstab still contains BOOT/UEFI entries after fix - VM may enter emergency mode");
    } else {
        info!("fstab fixed - removed BOOT and UEFI entries");
    }

    Ok(())
}

/// Create a Layer 2 setup initrd with embedded packages
///
/// This creates a busybox-based initrd that:
/// 1. Mounts /dev/vda (rootfs) at /newroot
/// 2. Copies packages from /packages (embedded in initrd) to rootfs
/// 3. Runs dpkg -i to install packages inside rootfs
/// 4. Runs the setup script
/// 5. Powers off the VM
///
/// Packages are embedded directly in the initrd, no second disk needed.
/// This allows using Kata's kernel which has FUSE but no ISO9660/SquashFS.
async fn create_layer2_setup_initrd(
    install_script: &str,
    setup_script: &str,
    packages_dir: &Path,
) -> Result<PathBuf> {
    info!("creating Layer 2 setup initrd with embedded packages");

    // Use UID in path to avoid permission conflicts between root and non-root
    let uid = unsafe { libc::getuid() };
    let temp_dir = PathBuf::from(format!("/tmp/fcvm-layer2-initrd-{}", uid));
    let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    tokio::fs::create_dir_all(&temp_dir).await?;

    // Create the init script that runs before systemd
    let init_script = generate_init_script(install_script, setup_script);

    // Write init script
    let init_path = temp_dir.join("init");
    tokio::fs::write(&init_path, &init_script).await?;

    // Make init executable
    let output = Command::new("chmod")
        .args(["755", path_to_str(&init_path)?])
        .output()
        .await
        .context("making init executable")?;

    if !output.status.success() {
        bail!(
            "Failed to chmod init: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Copy busybox static binary (prefer busybox-static if available)
    let busybox_src = find_busybox()?;
    let busybox_dst = temp_dir.join("bin").join("busybox");
    tokio::fs::create_dir_all(temp_dir.join("bin")).await?;
    tokio::fs::copy(&busybox_src, &busybox_dst)
        .await
        .context("copying busybox")?;

    let output = Command::new("chmod")
        .args(["755", path_to_str(&busybox_dst)?])
        .output()
        .await
        .context("making busybox executable")?;

    if !output.status.success() {
        bail!(
            "Failed to chmod busybox: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Copy packages into initrd
    let initrd_packages_dir = temp_dir.join("packages");
    tokio::fs::create_dir_all(&initrd_packages_dir).await?;

    // Copy all .deb files from packages_dir to initrd
    let mut entries = tokio::fs::read_dir(packages_dir).await?;
    let mut package_count = 0;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().map(|e| e == "deb").unwrap_or(false) {
            let dest = initrd_packages_dir.join(entry.file_name());
            tokio::fs::copy(&path, &dest).await?;
            package_count += 1;
        }
    }
    info!(count = package_count, "embedded packages in initrd");

    // Create the initrd using cpio
    // Use bash with pipefail so cpio errors aren't masked by gzip success
    let initrd_path = temp_dir.join("initrd.cpio.gz");
    let cpio_output = Command::new("bash")
        .args([
            "-c",
            &format!(
                "set -o pipefail && cd {} && find . | cpio -o -H newc | gzip > {}",
                temp_dir.display(),
                initrd_path.display()
            ),
        ])
        .output()
        .await
        .context("creating initrd cpio archive")?;

    if !cpio_output.status.success() {
        bail!(
            "Failed to create initrd: stdout={}, stderr={}",
            String::from_utf8_lossy(&cpio_output.stdout),
            String::from_utf8_lossy(&cpio_output.stderr)
        );
    }

    // Log initrd size
    if let Ok(meta) = tokio::fs::metadata(&initrd_path).await {
        let size_mb = meta.len() as f64 / 1024.0 / 1024.0;
        info!(path = %initrd_path.display(), size_mb = format!("{:.1}", size_mb), "Layer 2 setup initrd created");
    }

    Ok(initrd_path)
}

/// Download all required .deb packages on the host
///
/// Returns the path to the packages directory (not an ISO).
/// Packages will be embedded directly in the initrd.
///
/// NOTE: fc-agent is NOT included - it will be injected per-VM at boot time.
async fn download_packages(plan: &Plan, script_sha_short: &str) -> Result<PathBuf> {
    let cache_dir = paths::base_dir().join("cache");
    let packages_dir = cache_dir.join(format!("packages-{}", script_sha_short));

    // If packages directory already exists with .deb files, use it
    if packages_dir.exists() {
        if let Ok(mut entries) = tokio::fs::read_dir(&packages_dir).await {
            let mut has_debs = false;
            while let Ok(Some(entry)) = entries.next_entry().await {
                if entry
                    .path()
                    .extension()
                    .map(|e| e == "deb")
                    .unwrap_or(false)
                {
                    has_debs = true;
                    break;
                }
            }
            if has_debs {
                info!(path = %packages_dir.display(), "using cached packages directory");
                return Ok(packages_dir);
            }
        }
    }

    // Create packages directory
    let _ = tokio::fs::remove_dir_all(&packages_dir).await;
    tokio::fs::create_dir_all(&packages_dir).await?;

    // Get list of packages
    let packages = plan.packages.all_packages();
    let packages_str = packages.join(" ");

    info!(packages = %packages_str, "downloading .deb packages on host");

    // Download packages with dependencies using apt-get download
    // We need to run this in a way that downloads packages for the target system
    // Using apt-get download with proper architecture
    let output = Command::new("apt-get")
        .args([
            "download",
            "-o",
            &format!("Dir::Cache::archives={}", packages_dir.display()),
        ])
        .args(&packages)
        .current_dir(&packages_dir)
        .output()
        .await
        .context("downloading packages with apt-get")?;

    if !output.status.success() {
        // apt-get download might fail, try with apt-cache to get dependencies first
        warn!("apt-get download failed, trying alternative method");

        // Alternative: use apt-rdepends or manually download
        for pkg in &packages {
            let output = Command::new("apt-get")
                .args(["download", pkg])
                .current_dir(&packages_dir)
                .output()
                .await;

            if let Ok(out) = output {
                if !out.status.success() {
                    warn!(package = %pkg, "failed to download package, continuing...");
                }
            }
        }
    }

    // Also download dependencies
    info!("downloading package dependencies");
    let deps_output = Command::new("sh")
        .args([
            "-c",
            &format!(
                "apt-cache depends --recurse --no-recommends --no-suggests --no-conflicts \
                 --no-breaks --no-replaces --no-enhances {} | \
                 grep '^\\w' | sort -u | xargs apt-get download 2>/dev/null || true",
                packages_str
            ),
        ])
        .current_dir(&packages_dir)
        .output()
        .await;

    if let Err(e) = deps_output {
        warn!(error = %e, "failed to download some dependencies, continuing...");
    }

    // Count downloaded packages
    let mut count = 0;
    if let Ok(mut entries) = tokio::fs::read_dir(&packages_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry
                .path()
                .extension()
                .map(|e| e == "deb")
                .unwrap_or(false)
            {
                count += 1;
            }
        }
    }
    info!(count = count, "downloaded .deb packages");

    if count == 0 {
        bail!("No packages downloaded. Check network and apt configuration.");
    }

    info!(path = %packages_dir.display(), count = count, "packages downloaded");
    Ok(packages_dir)
}

/// Download cloud image (cached by URL hash)
async fn download_cloud_image(plan: &Plan) -> Result<PathBuf> {
    let cache_dir = paths::base_dir().join("cache");
    tokio::fs::create_dir_all(&cache_dir)
        .await
        .context("creating cache directory")?;

    // Get arch-specific config
    let arch_config = match std::env::consts::ARCH {
        "x86_64" => &plan.base.amd64,
        "aarch64" => &plan.base.arm64,
        other => bail!("unsupported architecture: {}", other),
    };

    let arch_name = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };

    // Cache by URL hash - changing URL triggers re-download
    let url_hash = &compute_sha256(arch_config.url.as_bytes())[..12];
    let image_path = cache_dir.join(format!(
        "ubuntu-{}-{}-{}.img",
        plan.base.version, arch_name, url_hash
    ));

    // If cached, use it
    if image_path.exists() {
        info!(path = %image_path.display(), "using cached cloud image");
        return Ok(image_path);
    }

    // Download
    info!(
        url = %arch_config.url,
        "downloading Ubuntu cloud image (this may take several minutes)"
    );

    let temp_path = image_path.with_extension("img.download");
    let output = Command::new("curl")
        .args([
            "-L",
            "-o",
            path_to_str(&temp_path)?,
            "--progress-bar",
            &arch_config.url,
        ])
        .status()
        .await
        .context("downloading cloud image")?;

    if !output.success() {
        bail!("curl failed to download cloud image");
    }

    // Rename to final path
    tokio::fs::rename(&temp_path, &image_path)
        .await
        .context("renaming downloaded image")?;

    info!(
        path = %image_path.display(),
        "cloud image downloaded"
    );

    Ok(image_path)
}

/// Boot a Firecracker VM to run the Layer 2 setup initrd
///
/// This boots with an initrd that has packages embedded:
/// - Mounts rootfs (/dev/vda) at /newroot
/// - Copies packages from /packages (in initrd RAM) to rootfs
/// - Runs dpkg -i to install packages inside rootfs via chroot
/// - Runs the setup script
/// - Powers off when complete
///
/// Only one disk is needed - packages are embedded in the initrd.
/// This allows using Kata's kernel which has FUSE but no ISO9660/SquashFS.
async fn boot_vm_for_setup(disk_path: &Path, initrd_path: &Path) -> Result<()> {
    use std::time::Duration;
    use tokio::time::timeout;

    // Create a temporary directory for this setup VM
    // Use UID in path to avoid permission conflicts between root and non-root
    let uid = unsafe { libc::getuid() };
    let temp_dir = PathBuf::from(format!("/tmp/fcvm-layer2-setup-{}", uid));
    let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    tokio::fs::create_dir_all(&temp_dir).await?;

    let api_socket = temp_dir.join("firecracker.sock");
    let log_path = temp_dir.join("firecracker.log");

    // Find kernel - downloaded from Kata release if needed
    let kernel_path = crate::setup::kernel::ensure_kernel().await?;

    // Create serial console output file
    let serial_path = temp_dir.join("serial.log");
    let serial_file =
        std::fs::File::create(&serial_path).context("creating serial console file")?;

    // Start Firecracker with serial console output
    info!(
        "starting Firecracker for Layer 2 setup (serial output: {})",
        serial_path.display()
    );
    let mut fc_process = Command::new("firecracker")
        .args([
            "--api-sock",
            path_to_str(&api_socket)?,
            "--log-path",
            path_to_str(&log_path)?,
            "--level",
            "Info",
        ])
        .stdout(serial_file.try_clone().context("cloning serial file")?)
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("starting Firecracker")?;

    // Wait for socket to be ready
    for _ in 0..50 {
        if api_socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    if !api_socket.exists() {
        fc_process.kill().await.ok();
        bail!("Firecracker API socket not created");
    }

    // Configure VM via API
    let client = crate::firecracker::api::FirecrackerClient::new(api_socket.clone())?;

    // Set boot source - boot from raw ext4 partition (no GPT)
    // The disk IS the filesystem, so use root=/dev/vda directly
    // No cloud-init needed - scripts are injected via debugfs and run by rc.local
    client
        .set_boot_source(crate::firecracker::api::BootSource {
            kernel_image_path: kernel_path.display().to_string(),
            // Boot with initrd that runs setup before trying to use systemd
            // The initrd handles everything and powers off, so we don't need to worry about systemd
            boot_args: Some("console=ttyS0 reboot=k panic=1 pci=off".to_string()),
            initrd_path: Some(initrd_path.display().to_string()),
        })
        .await?;

    // Add root drive (raw ext4 filesystem, no partition table)
    client
        .add_drive(
            "rootfs",
            crate::firecracker::api::Drive {
                drive_id: "rootfs".to_string(),
                path_on_host: disk_path.display().to_string(),
                is_root_device: true,
                is_read_only: false,
                partuuid: None,
                rate_limiter: None,
            },
        )
        .await?;

    // No packages drive needed - packages are embedded in the initrd

    // Configure machine (minimal for setup)
    client
        .set_machine_config(crate::firecracker::api::MachineConfig {
            vcpu_count: 2,
            mem_size_mib: 2048, // 2GB for package installation
            smt: Some(false),
            cpu_template: None,
            track_dirty_pages: None,
        })
        .await?;

    // No network needed! Packages are installed from local ISO.

    // Start the VM
    client
        .put_action(crate::firecracker::api::InstanceAction::InstanceStart)
        .await?;
    info!("Layer 2 setup VM started, waiting for completion (this takes several minutes)");

    // Wait for VM to shut down (setup script runs shutdown -h now when done)
    // Timeout after 15 minutes
    let start = std::time::Instant::now();
    let mut last_serial_len = 0usize;
    let result = timeout(Duration::from_secs(900), async {
        loop {
            // Check if Firecracker process has exited
            match fc_process.try_wait() {
                Ok(Some(status)) => {
                    let elapsed = start.elapsed();
                    info!(
                        "Firecracker exited with status: {:?} after {:?}",
                        status, elapsed
                    );
                    return Ok(elapsed);
                }
                Ok(None) => {
                    // Still running, check for new serial output and log it
                    if let Ok(serial_content) = tokio::fs::read_to_string(&serial_path).await {
                        if serial_content.len() > last_serial_len {
                            // Log new output (trimmed to avoid excessive logging)
                            let new_output = &serial_content[last_serial_len..];
                            for line in new_output.lines() {
                                // Skip empty lines and lines that are just timestamps
                                if !line.trim().is_empty() {
                                    debug!(target: "layer2_setup", "{}", line);
                                }
                            }
                            last_serial_len = serial_content.len();
                        }
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                Err(e) => {
                    return Err(anyhow::anyhow!("Error checking Firecracker status: {}", e));
                }
            }
        }
    })
    .await;

    // Cleanup
    fc_process.kill().await.ok();

    match result {
        Ok(Ok(elapsed)) => {
            // Check for completion marker in serial output
            let serial_content = tokio::fs::read_to_string(&serial_path)
                .await
                .unwrap_or_default();
            if !serial_content.contains("FCVM_SETUP_COMPLETE") {
                warn!("Setup failed! Serial console output:\n{}", serial_content);
                if let Ok(log_content) = tokio::fs::read_to_string(&log_path).await {
                    warn!("Firecracker log:\n{}", log_content);
                }
                let _ = tokio::fs::remove_dir_all(&temp_dir).await;
                bail!("Layer 2 setup failed (no FCVM_SETUP_COMPLETE marker found)");
            }
            let _ = tokio::fs::remove_dir_all(&temp_dir).await;
            info!(
                elapsed_secs = elapsed.as_secs(),
                "Layer 2 setup VM completed successfully"
            );
            Ok(())
        }
        Ok(Err(e)) => {
            let _ = tokio::fs::remove_dir_all(&temp_dir).await;
            Err(e)
        }
        Err(_) => {
            let _ = tokio::fs::remove_dir_all(&temp_dir).await;
            bail!("Layer 2 setup VM timed out after 15 minutes")
        }
    }
}

/// Helper to convert Path to str
fn path_to_str(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow::anyhow!("path contains invalid UTF-8: {:?}", path))
}
