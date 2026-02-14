use anyhow::{bail, Context, Result};
use directories::ProjectDirs;
use nix::fcntl::{Flock, FlockArg};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::paths;

/// Config file name
const CONFIG_FILE: &str = "rootfs-config.toml";

/// Embedded default config (used by --generate-config)
const EMBEDDED_CONFIG: &str = include_str!("../../rootfs-config.toml");

/// Size of the Layer 2 disk image
const LAYER2_SIZE: &str = "10G";

// ============================================================================
// Plan File Data Structures
// ============================================================================

#[derive(Debug, Deserialize, Clone)]
pub struct Plan {
    #[serde(default)]
    pub paths: PathsConfig,
    pub base: BaseConfig,
    pub kernel: KernelConfig,
    pub packages: PackagesConfig,
    pub services: ServicesConfig,
    pub files: HashMap<String, FileConfig>,
    pub fstab: FstabConfig,
    #[serde(default)]
    pub cleanup: CleanupConfig,
    /// Kernel profiles: kernel_profiles.{name}.{arch} = KernelProfile
    /// E.g., kernel_profiles.nested.arm64 = { kernel_version = "6.18", ... }
    #[serde(default)]
    pub kernel_profiles: HashMap<String, HashMap<String, KernelProfile>>,
}

/// Kernel profile configuration
///
/// Profiles override the default [kernel] section for special use cases.
/// Custom kernels are built from source or downloaded from GitHub releases.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct KernelProfile {
    /// Human-readable description
    #[serde(default)]
    pub description: String,

    // ========== Custom kernel (build from source) ==========
    /// Kernel version (e.g., "6.18")
    #[serde(default)]
    pub kernel_version: String,

    /// GitHub repo for kernel releases (e.g., "owner/repo")
    #[serde(default)]
    pub kernel_repo: String,

    /// Files to hash for kernel SHA (globs supported)
    /// These files determine when the kernel needs to be rebuilt.
    /// Example: ["kernel/build.sh", "kernel/nested.conf", "kernel/patches/*.patch"]
    #[serde(default)]
    pub build_inputs: Vec<String>,

    /// Base config URL for VM kernel (Firecracker's microvm config)
    /// {arch} is replaced with aarch64 or x86_64 at build time
    #[serde(default)]
    pub base_config_url: Option<String>,

    /// Kernel config fragment file path (relative to repo root)
    /// Applied on top of base_config_url
    #[serde(default)]
    pub kernel_config: Option<String>,

    /// Patches directory (relative to repo root)
    #[serde(default)]
    pub patches_dir: Option<String>,

    // ========== Runtime overrides ==========
    /// Path to firecracker binary (default: system firecracker)
    #[serde(default)]
    pub firecracker_bin: Option<String>,

    /// GitHub repo for firecracker fork (for building if binary missing)
    #[serde(default)]
    pub firecracker_repo: Option<String>,

    /// Branch to build firecracker from
    #[serde(default)]
    pub firecracker_branch: Option<String>,

    /// Extra CLI args for firecracker
    #[serde(default)]
    pub firecracker_args: Option<String>,

    /// Extra kernel boot parameters
    #[serde(default)]
    pub boot_args: Option<String>,

    /// Override FUSE reader count
    #[serde(default)]
    pub fuse_readers: Option<u32>,

    /// Host kernel configuration (for EC2 instances running fcvm)
    #[serde(default)]
    pub host_kernel: Option<HostKernelConfig>,
}

/// Host kernel build configuration.
///
/// Uses the running kernel's config as base (includes all EC2/AWS modules),
/// applies fcvm patches, and builds deb packages for installation.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct HostKernelConfig {
    /// Kernel version (e.g., "6.18.3")
    #[serde(default)]
    pub kernel_version: String,

    /// Patches directory (relative to repo root)
    #[serde(default)]
    pub patches_dir: Option<String>,

    /// Files to hash for kernel SHA (globs supported, *.vm.patch excluded)
    #[serde(default)]
    pub build_inputs: Vec<String>,
}

impl KernelProfile {
    /// Check if this profile has a custom kernel configured
    pub fn is_custom(&self) -> bool {
        !self.kernel_version.is_empty() && !self.kernel_repo.is_empty()
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct PathsConfig {
    /// Directory for mutable VM data (vm-disks, state, snapshots)
    #[serde(default = "default_base_dir")]
    pub data_dir: String,
    /// Directory for shared content-addressed assets (kernels, rootfs, initrd, image-cache)
    #[serde(default = "default_base_dir")]
    pub assets_dir: String,
    /// Size of the btrfs loopback filesystem (e.g., "60G")
    #[serde(default = "default_btrfs_size")]
    pub btrfs_size: String,
}

fn default_btrfs_size() -> String {
    "60G".to_string()
}

fn default_base_dir() -> String {
    "/mnt/fcvm-btrfs".to_string()
}

impl Default for PathsConfig {
    fn default() -> Self {
        Self {
            data_dir: default_base_dir(),
            assets_dir: default_base_dir(),
            btrfs_size: default_btrfs_size(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct BaseConfig {
    pub version: String,
    /// Ubuntu codename (e.g., "noble" for 24.04) - used to download packages
    pub codename: String,
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
    /// Required unless `local_path` is provided
    #[serde(default)]
    pub url: String,
    /// Path within the archive to extract (only used with URL)
    #[serde(default)]
    pub path: String,
    /// Local filesystem path to kernel binary (overrides url if provided)
    /// Use for custom-built kernels (e.g., profile kernel with CONFIG_KVM)
    #[serde(default)]
    pub local_path: Option<String>,
}

impl KernelArchConfig {
    /// Check if this config uses a local path
    pub fn is_local(&self) -> bool {
        self.local_path.is_some()
    }
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

/// Package groups for rootfs. Each field must be added to all_packages().
/// Using deny_unknown_fields to catch config typos that would silently be ignored.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct PackagesConfig {
    pub runtime: Vec<String>,
    pub fuse: Vec<String>,
    #[serde(default)]
    pub nfs: Vec<String>,
    pub system: Vec<String>,
    #[serde(default)]
    pub debug: Vec<String>,
}

impl PackagesConfig {
    pub fn all_packages(&self) -> Vec<&str> {
        self.runtime
            .iter()
            .chain(&self.fuse)
            .chain(&self.nfs)
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
set -euo pipefail

# Set PATH - required when running in chroot environment
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

echo 'FCVM: Removing conflicting packages before install...'
# Remove time-daemon provider that conflicts with chrony
apt-get remove -y --purge systemd-timesyncd || true
# Remove packages we don't need in microVM (also frees space)
apt-get remove -y --purge cloud-init snapd ubuntu-server || true

echo 'FCVM: Installing packages from initrd...'
PKG_COUNT=$(ls /mnt/packages/*.deb 2>/dev/null | wc -l)
echo "FCVM: Found $PKG_COUNT .deb files"

# Capture dpkg output for error reporting
DPKG_LOG=/tmp/dpkg-install.log
dpkg -i /mnt/packages/*.deb 2>&1 | tee "$DPKG_LOG"
DPKG_STATUS=${PIPESTATUS[0]}

if [ $DPKG_STATUS -ne 0 ]; then
    echo ''
    echo '=========================================='
    echo 'FCVM ERROR: dpkg -i failed!'
    echo '=========================================='
    echo 'Failed packages:'
    grep -E '^dpkg: error|^Errors were encountered' "$DPKG_LOG" || true
    echo ''
    echo 'Dependency problems:'
    grep -E 'dependency problems|depends on' "$DPKG_LOG" || true
    echo '=========================================='
    exit 1
fi

echo 'FCVM: Packages installed successfully'
"#
    .to_string()
}

/// Generate the bash script that runs INSIDE the ubuntu container to download packages.
/// This script is included in the hash to ensure cache invalidation when the
/// download method or package list changes. The same script is used for execution
/// in download_packages().
pub fn generate_download_script(plan: &Plan) -> String {
    let packages = plan.packages.all_packages();
    let packages_str = packages.join(" ");
    let codename = &plan.base.codename;

    // This is the script that runs inside the ubuntu container
    // Format: codename is used for the container image, packages for apt-get
    format!(
        r#"# Download packages for Ubuntu {codename}
set -euo pipefail
# Disable APT sandbox - required for proxy auth via BPF interception
# The _apt user doesn't have credentials, so apt must run as root
echo 'APT::Sandbox::User "root";' > /etc/apt/apt.conf.d/10sandbox
# Configure apt proxy if http_proxy is set
if [ -n "${{http_proxy:-}}" ]; then
    echo "Acquire::http::Proxy \"$http_proxy\";" > /etc/apt/apt.conf.d/99proxy
    echo "Acquire::https::Proxy \"$http_proxy\";" >> /etc/apt/apt.conf.d/99proxy
fi
apt-get update -qq
apt-get install --download-only --yes --no-install-recommends {packages}
cp /var/cache/apt/archives/*.deb /packages/ 2>/dev/null || true
"#,
        codename = codename,
        packages = packages_str
    )
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
    echo 1 > /proc/sys/kernel/sysrq 2>/dev/null || true
    echo o > /proc/sysrq-trigger 2>/dev/null || poweroff -f
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
if [ $INSTALL_RESULT -ne 0 ]; then
    echo "FCVM_SETUP_FAILED: Package installation failed with exit code $INSTALL_RESULT"
    echo 1 > /proc/sys/kernel/sysrq 2>/dev/null || true
    echo o > /proc/sysrq-trigger 2>/dev/null || poweroff -f
fi

# Run setup script using chroot
echo "FCVM Layer 2 Setup: Running setup script..."
chroot /newroot /bin/bash /tmp/fcvm-setup.sh
SETUP_RESULT=$?
echo "FCVM Layer 2 Setup: Setup script returned: $SETUP_RESULT"
if [ $SETUP_RESULT -ne 0 ]; then
    echo "FCVM_SETUP_FAILED: Setup script failed with exit code $SETUP_RESULT"
    echo 1 > /proc/sys/kernel/sysrq 2>/dev/null || true
    echo o > /proc/sysrq-trigger 2>/dev/null || poweroff -f
fi

# Cleanup chroot mounts (use lazy unmount as fallback)
echo "FCVM Layer 2 Setup: Cleaning up..."
umount /newroot/dev 2>/dev/null || umount -l /newroot/dev 2>/dev/null || true
umount /newroot/sys 2>/dev/null || umount -l /newroot/sys 2>/dev/null || true
umount /newroot/proc 2>/dev/null || umount -l /newroot/proc 2>/dev/null || true
rm -rf /newroot/mnt/packages
rm -f /newroot/tmp/install-packages.sh
rm -f /newroot/tmp/fcvm-setup.sh

# Sanity checks before writing marker file
echo "FCVM Layer 2 Setup: Running sanity checks..."
SANITY_FAILED=0

# Check critical binaries exist
for bin in podman crun; do
    if [ ! -x "/newroot/usr/bin/$bin" ]; then
        echo "FCVM ERROR: $bin not found at /newroot/usr/bin/$bin"
        SANITY_FAILED=1
    fi
done

# Check systemd exists
if [ ! -x "/newroot/lib/systemd/systemd" ] && [ ! -x "/newroot/usr/lib/systemd/systemd" ]; then
    echo "FCVM ERROR: systemd not found"
    SANITY_FAILED=1
fi

# Check resolv.conf exists
if [ ! -f "/newroot/etc/resolv.conf" ]; then
    echo "FCVM ERROR: /etc/resolv.conf not found"
    SANITY_FAILED=1
fi

if [ $SANITY_FAILED -ne 0 ]; then
    echo "FCVM_SETUP_FAILED: Sanity checks failed"
    mount -t proc proc /proc 2>/dev/null || true
    echo o > /proc/sysrq-trigger 2>/dev/null || poweroff -f
fi

echo "FCVM Layer 2 Setup: Sanity checks passed"

# Write marker file to rootfs (proves setup completed successfully)
date -u '+%Y-%m-%dT%H:%M:%SZ' > /newroot/etc/fcvm-setup-complete
echo "FCVM Layer 2 Setup: Wrote marker file /etc/fcvm-setup-complete"

# Sync and unmount rootfs
sync
umount /newroot 2>/dev/null || umount -l /newroot 2>/dev/null || true

echo "FCVM_SETUP_COMPLETE"
echo "FCVM Layer 2 Setup: Complete! Powering off..."

# Re-mount /proc in case bind unmount affected it, then use sysrq for reliable shutdown
mount -t proc proc /proc 2>/dev/null || true
echo 1 > /proc/sys/kernel/sysrq 2>/dev/null || true
echo o > /proc/sysrq-trigger 2>/dev/null || true

# Fallback methods if sysrq didn't work
sleep 1
reboot -f 2>/dev/null || true
poweroff -f 2>/dev/null || true

# Last resort: halt via kernel
echo b > /proc/sysrq-trigger 2>/dev/null || true
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
        // Remove dangling symlinks (e.g., /etc/resolv.conf -> /run/systemd/...)
        s.push_str(&format!("rm -f {} 2>/dev/null || true\n", path));
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
// Config File Loading
// ============================================================================

/// Generate default config file at XDG config directory.
///
/// Writes the embedded default config to ~/.config/fcvm/rootfs-config.toml
pub fn generate_config(force: bool) -> Result<PathBuf> {
    let proj_dirs =
        ProjectDirs::from("", "", "fcvm").context("Could not determine config directory")?;
    let config_dir = proj_dirs.config_dir();
    let config_path = config_dir.join(CONFIG_FILE);

    if config_path.exists() && !force {
        bail!(
            "Config file already exists at {}\n\n\
             Use --force to overwrite, or edit the existing file.",
            config_path.display()
        );
    }

    std::fs::create_dir_all(config_dir)
        .with_context(|| format!("creating config directory: {}", config_dir.display()))?;
    std::fs::write(&config_path, EMBEDDED_CONFIG)
        .with_context(|| format!("writing config file: {}", config_path.display()))?;

    info!("Generated config at {}", config_path.display());
    Ok(config_path)
}

/// Find the config file using the lookup chain.
///
/// Lookup order:
/// 1. Explicit path (--config flag)
/// 2. SUDO_USER's config (when running with sudo, use invoking user's config)
/// 3. XDG user config (~/.config/fcvm/rootfs-config.toml)
/// 4. System config (/etc/fcvm/rootfs-config.toml)
/// 5. Next to binary (development)
/// 6. ERROR (no embedded fallback)
pub fn find_config_file(explicit_path: Option<&str>) -> Result<PathBuf> {
    // 1. Explicit --config
    if let Some(path) = explicit_path {
        let p = PathBuf::from(path);
        if !p.exists() {
            bail!("Config file not found: {}", path);
        }
        return Ok(p);
    }

    // 2. SUDO_USER's config (when running with sudo)
    if let Ok(sudo_user) = std::env::var("SUDO_USER") {
        // Get the invoking user's home directory
        match nix::unistd::User::from_name(&sudo_user) {
            Ok(Some(user)) => {
                let p = user.dir.join(".config/fcvm").join(CONFIG_FILE);
                if p.exists() {
                    return Ok(p);
                }
            }
            Ok(None) => {
                tracing::debug!("SUDO_USER '{}' not found in passwd database", sudo_user);
            }
            Err(e) => {
                tracing::debug!("Failed to lookup SUDO_USER '{}': {}", sudo_user, e);
            }
        }
    }

    // 3. XDG user config
    if let Some(proj_dirs) = ProjectDirs::from("", "", "fcvm") {
        let p = proj_dirs.config_dir().join(CONFIG_FILE);
        if p.exists() {
            return Ok(p);
        }
    }

    // 4. System config
    let system = Path::new("/etc/fcvm").join(CONFIG_FILE);
    if system.exists() {
        return Ok(system);
    }

    // 5. Next to binary (development)
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            // Check next to binary
            let p = exe_dir.join(CONFIG_FILE);
            if p.exists() {
                return Ok(p);
            }
            // Check parent directories (for development)
            for parent in &[".", "..", "../.."] {
                let p = exe_dir.join(parent).join(CONFIG_FILE);
                if p.exists() {
                    return p.canonicalize().context("canonicalizing config path");
                }
            }
        }
    }

    // 5. Check CARGO_MANIFEST_DIR for development builds (debug only)
    // In release builds (cargo install), this path would be stale and misleading
    #[cfg(debug_assertions)]
    {
        let manifest_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(CONFIG_FILE);
        if manifest_path.exists() {
            return Ok(manifest_path);
        }
    }

    // 6. Error with helpful message
    bail!(
        "No rootfs config found.\n\n\
         Searched:\n  \
         ~/.config/fcvm/{}\n  \
         /etc/fcvm/{}\n  \
         <binary-dir>/{}\n\n\
         Generate the default config with:\n  \
         fcvm setup --generate-config",
        CONFIG_FILE,
        CONFIG_FILE,
        CONFIG_FILE
    );
}

/// Load and parse the config file
pub fn load_config(explicit_path: Option<&str>) -> Result<(Plan, String, String)> {
    let config_path = find_config_file(explicit_path)?;
    let config_content = std::fs::read_to_string(&config_path)
        .with_context(|| format!("reading config file: {}", config_path.display()))?;

    // Compute SHA256 of config content (first 12 chars for image naming)
    let config_sha = compute_sha256(config_content.as_bytes());
    let config_sha_short = config_sha[..12].to_string();

    let config: Plan = toml::from_str(&config_content)
        .with_context(|| format!("parsing config file: {}", config_path.display()))?;

    info!(
        config_file = %config_path.display(),
        config_sha = %config_sha_short,
        "loaded rootfs config"
    );

    Ok((config, config_sha, config_sha_short))
}

/// Legacy alias for load_config (for backward compatibility during migration)
pub fn load_plan() -> Result<(Plan, String, String)> {
    load_config(None)
}

/// Get the arch name used in config files ("arm64" or "amd64")
fn config_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    }
}

/// Get a kernel profile by name for the current architecture.
///
/// Looks up kernel_profiles.{name}.{arch} (e.g., kernel_profiles.nested.arm64).
/// Returns the profile config if found, or None if not defined.
pub fn get_kernel_profile(name: &str) -> Result<Option<KernelProfile>> {
    let (plan, _, _) = load_plan()?;
    let arch = config_arch();
    Ok(plan
        .kernel_profiles
        .get(name)
        .and_then(|arch_profiles| arch_profiles.get(arch))
        .cloned())
}

/// Detect kernel profile from kernel path.
///
/// Checks if the kernel filename matches a configured profile name.
/// Returns the profile name if matched.
pub fn detect_kernel_profile(kernel_path: &Path) -> Option<String> {
    let name = kernel_path.file_name()?.to_str()?;

    // Load config to get all profile names
    if let Ok((config, _, _)) = load_config(None) {
        for profile_name in config.kernel_profiles.keys() {
            // Check if filename contains the profile name
            if name.contains(profile_name) {
                return Some(profile_name.clone());
            }
        }
    }

    None
}

/// Get the active kernel profile from env var or auto-detection
///
/// Checks FCVM_KERNEL_PROFILE first, then tries to detect from kernel path.
pub fn get_active_kernel_profile(kernel_path: Option<&Path>) -> Result<Option<KernelProfile>> {
    // First check env var
    if let Ok(profile_name) = std::env::var("FCVM_KERNEL_PROFILE") {
        if let Some(profile) = get_kernel_profile(&profile_name)? {
            info!(profile = %profile_name, "using kernel profile from FCVM_KERNEL_PROFILE");
            return Ok(Some(profile));
        } else {
            warn!(profile = %profile_name, "FCVM_KERNEL_PROFILE specified but profile not found in config");
        }
    }

    // Then try auto-detection from kernel path
    if let Some(path) = kernel_path {
        if let Some(profile_name) = detect_kernel_profile(path) {
            if let Some(profile) = get_kernel_profile(&profile_name)? {
                info!(profile = %profile_name, path = %path.display(), "auto-detected kernel profile from path");
                return Ok(Some(profile));
            }
        }
    }

    Ok(None)
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
///
/// If `allow_create` is false, bail if rootfs doesn't exist.
pub async fn ensure_rootfs(allow_create: bool) -> Result<PathBuf> {
    let (plan, _plan_sha_full, _plan_sha_short) = load_plan()?;

    // Generate all scripts and compute hash of the complete init script
    let setup_script = generate_setup_script(&plan);
    let install_script = generate_install_script();
    let init_script = generate_init_script(&install_script, &setup_script);
    let download_script = generate_download_script(&plan);

    // Get kernel URL for the current architecture
    let kernel_config = plan.kernel.current_arch()?;
    let kernel_url = &kernel_config.url;

    // Hash the complete init script + kernel URL + download script
    // Any change to:
    // - init logic, install script, or setup script
    // - kernel URL (different kernel version/release)
    // - download method (podman image, codename, packages)
    // invalidates the cache
    let mut combined = init_script.clone();
    combined.push_str("\n# KERNEL_URL: ");
    combined.push_str(kernel_url);
    combined.push_str("\n# DOWNLOAD_SCRIPT:\n");
    combined.push_str(&download_script);
    combined.push_str("\n# FC_AGENT_SERVICE:\n");
    combined.push_str(FC_AGENT_SERVICE);
    combined.push_str("\n# FC_AGENT_SERVICE_STRACE:\n");
    combined.push_str(FC_AGENT_SERVICE_STRACE);
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

    // Bail if creation not allowed
    if !allow_create {
        bail!("Rootfs not found. Run 'fcvm setup' first, or use --setup flag.");
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
# Delegate cgroup control so podman can use pids/memory/cpu controllers
Delegate=yes

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
# Delegate cgroup control so podman can use pids/memory/cpu controllers
Delegate=yes
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
///
/// If `allow_create` is false, bail if initrd doesn't exist.
pub async fn ensure_fc_agent_initrd(allow_create: bool) -> Result<PathBuf> {
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
    let initrd_dir = paths::initrd_dir();
    let initrd_path = initrd_dir.join(format!("fc-agent-{}.initrd", initrd_sha_short));

    if initrd_path.exists() {
        debug!(
            path = %initrd_path.display(),
            initrd_sha = %initrd_sha_short,
            "using cached fc-agent initrd"
        );
        return Ok(initrd_path);
    }

    // Bail if creation not allowed
    if !allow_create {
        bail!("fc-agent initrd not found. Run 'fcvm setup' first, or use --setup flag.");
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
    let cache_dir = paths::cache_dir();
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

    let codename = &plan.base.codename;
    let container_image = format!("ubuntu:{}", codename);

    info!(codename = %codename, "downloading .deb packages using container");

    // Use the same script that's included in the hash
    let download_script = generate_download_script(plan);

    // Build podman args, including proxy env vars if set
    let mut podman_args = vec![
        "run".to_string(),
        "--rm".to_string(),
        "--cgroups=disabled".to_string(),
        "--network=host".to_string(),
    ];

    // Pass through proxy environment variables if set
    for var in ["http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY"] {
        if let Ok(val) = std::env::var(var) {
            podman_args.push("-e".to_string());
            podman_args.push(format!("{}={}", var, val));
        }
    }

    podman_args.extend([
        "-v".to_string(),
        format!("{}:/packages", packages_dir.display()),
        container_image.clone(),
        "bash".to_string(),
        "-c".to_string(),
        download_script.clone(),
    ]);

    let output = Command::new("podman")
        .args(&podman_args)
        .output()
        .await
        .context("downloading packages with podman")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(stderr = %stderr, "podman download had errors, checking results...");
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

    if count == 0 {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "No packages downloaded. stdout={}, stderr={}",
            stdout.trim(),
            stderr.trim()
        );
    }

    info!(path = %packages_dir.display(), count = count, "packages downloaded");
    Ok(packages_dir)
}

/// Download cloud image (cached by URL hash)
async fn download_cloud_image(plan: &Plan) -> Result<PathBuf> {
    let cache_dir = paths::cache_dir();
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

    // Create log file (Firecracker requires it to exist)
    std::fs::File::create(&log_path).context("creating Firecracker log file")?;

    // Find kernel - downloaded from Kata release if needed
    // Use default kernel (None profile), allow_create=true, allow_build=false
    let kernel_path = crate::setup::kernel::ensure_kernel(None, true, false).await?;

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
            huge_pages: None,
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
                    // Still running, stream serial output to show progress
                    if let Ok(serial_content) = tokio::fs::read_to_string(&serial_path).await {
                        if serial_content.len() > last_serial_len {
                            let new_output = &serial_content[last_serial_len..];
                            for line in new_output.lines() {
                                if !line.trim().is_empty() {
                                    info!(target: "layer2_setup", "{}", line);
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
            if serial_content.contains("FCVM_SETUP_FAILED") {
                warn!("Setup failed! Serial console output:\n{}", serial_content);
                if let Ok(log_content) = tokio::fs::read_to_string(&log_path).await {
                    warn!("Firecracker log:\n{}", log_content);
                }
                let _ = tokio::fs::remove_dir_all(&temp_dir).await;
                bail!("Layer 2 setup failed (script exited with error - check logs above)");
            }
            if !serial_content.contains("FCVM_SETUP_COMPLETE") {
                warn!("Setup failed! Serial console output:\n{}", serial_content);
                if let Ok(log_content) = tokio::fs::read_to_string(&log_path).await {
                    warn!("Firecracker log:\n{}", log_content);
                }
                let _ = tokio::fs::remove_dir_all(&temp_dir).await;
                bail!("Layer 2 setup failed (no FCVM_SETUP_COMPLETE marker found)");
            }

            // Verify marker file exists in the rootfs using debugfs (no root needed)
            let debugfs_output = Command::new("debugfs")
                .args([
                    "-R",
                    "stat /etc/fcvm-setup-complete",
                    path_to_str(disk_path)?,
                ])
                .output()
                .await?;
            let marker_exists = debugfs_output.status.success()
                && !String::from_utf8_lossy(&debugfs_output.stdout).contains("not found");
            if !marker_exists {
                warn!("Setup failed! Serial console output:\n{}", serial_content);
                let _ = tokio::fs::remove_dir_all(&temp_dir).await;
                bail!("Layer 2 setup failed: marker file /etc/fcvm-setup-complete not found in rootfs");
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
            // Print serial log on timeout for debugging
            if let Ok(serial_content) = tokio::fs::read_to_string(&serial_path).await {
                eprintln!(
                    "=== Layer 2 setup VM timed out! Serial console output: ===\n{}",
                    serial_content
                );
            }
            if let Ok(log_content) = tokio::fs::read_to_string(&log_path).await {
                eprintln!("=== Firecracker log: ===\n{}", log_content);
            }
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
