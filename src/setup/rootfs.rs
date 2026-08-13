use anyhow::{bail, Context, Result};
use directories::ProjectDirs;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::paths;

/// Config file name
const CONFIG_FILE: &str = "rootfs-config.toml";

/// A --config path recorded once, so every later load_config(None) resolves to
/// the same file. Without this, --config reaches only the call sites that thread
/// it explicitly, and lookups that go through load_plan() (kernel profiles, for
/// one) silently read the user config instead. The failure is confusing rather
/// than loud: fcvm reports a profile "not found in config" while naming a config
/// that defines it.
static EXPLICIT_CONFIG: OnceLock<PathBuf> = OnceLock::new();

/// Record the config file chosen on the command line. First call wins.
pub fn set_config_path(path: &str) {
    let _ = EXPLICIT_CONFIG.set(PathBuf::from(path));
}

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
    pub packages: PackagesConfig,
    pub services: ServicesConfig,
    pub files: HashMap<String, FileConfig>,
    pub fstab: FstabConfig,
    #[serde(default)]
    pub cleanup: CleanupConfig,
    /// Default Firecracker configuration (repo + branch to build from)
    /// Used when no kernel profile overrides it.
    #[serde(default)]
    pub firecracker: Option<FirecrackerConfig>,
    /// Cloud Hypervisor build configuration (repo + branch to build from), #632.
    /// Optional VMM backend (`--hypervisor cloud-hypervisor`); built on demand via
    /// `fcvm setup --cloud-hypervisor`, content-addressed like firecracker.
    #[serde(default)]
    pub cloud_hypervisor: Option<CloudHypervisorConfig>,
    /// Pinned pasta build (upstream commit + fcvm-carried patches).
    /// When set, rootless networking REQUIRES the built binary — see
    /// src/setup/pasta.rs for the why and the robustness rules.
    #[serde(default)]
    pub pasta: Option<PastaConfig>,
    /// Kernel profiles: kernel_profiles.{name}.{arch} = KernelProfile
    /// E.g., kernel_profiles.nested.arm64 = { kernel_version = "6.18", ... }
    #[serde(default)]
    pub kernel_profiles: HashMap<String, HashMap<String, KernelProfile>>,
}

/// Pinned pasta build configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct PastaConfig {
    /// Git URL of the passt repository (canonical: https://passt.top/passt)
    pub repo: String,
    /// Pinned upstream commit (full SHA). A commit — not a branch — so the
    /// content-addressed binary path is computable offline.
    pub commit: String,
}

/// Default Firecracker build configuration.
///
/// When set, `fcvm setup` builds Firecracker from the specified fork
/// and `find_firecracker()` uses it instead of the system binary.
#[derive(Debug, Deserialize, Clone)]
pub struct FirecrackerConfig {
    /// GitHub repo (e.g., "ejc3/firecracker")
    pub repo: String,
    /// Branch or lightweight tag to build from (default: "main")
    #[serde(default = "default_branch")]
    pub branch: String,
    /// Exact full commit required at `branch`.
    #[serde(default)]
    pub commit: Option<String>,
}

/// Cloud Hypervisor build configuration (#632).
///
/// When set, `fcvm setup --cloud-hypervisor` builds Cloud Hypervisor from the
/// specified repo/branch (content-addressed, like firecracker) and
/// `find_cloud_hypervisor()` uses it. CH is an optional backend, so it is built on
/// demand rather than on every `fcvm setup`. Pinned to a fork branch carrying the
/// aarch64 SVE register save/restore fix (upstream PR #8268), which landed after the
/// v52.0 release and is not in any tagged release yet.
#[derive(Debug, Deserialize, Clone)]
pub struct CloudHypervisorConfig {
    /// GitHub repo (e.g., "ejc3/cloud-hypervisor")
    pub repo: String,
    /// Branch to build from (default: "main")
    #[serde(default = "default_branch")]
    pub branch: String,
}

fn default_branch() -> String {
    "main".to_string()
}

/// Kernel profile configuration
///
/// Every kernel is delivered through a profile. Source-built profiles download
/// content-addressed artifacts from GitHub releases or build locally from the
/// same inputs.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct KernelProfile {
    /// Human-readable description
    #[serde(default)]
    pub description: String,

    /// Root filesystem type: "ext4" (default) or "btrfs"
    /// When "btrfs", the rootfs is converted from ext4 to btrfs before setup VM boot.
    #[serde(default)]
    pub rootfs_type: Option<String>,

    // ========== URL-based kernel delivery (synthesized "default" profile) ==========
    /// URL to kernel archive (e.g., Kata release tarball)
    #[serde(default)]
    pub kernel_url: Option<String>,
    /// Path within the archive to extract the kernel binary
    #[serde(default)]
    pub kernel_archive_path: Option<String>,
    /// Local filesystem path to kernel binary (overrides URL)
    #[serde(default)]
    pub kernel_local_path: Option<String>,

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

    /// Published artifact SHA (the first 12 hex digits of build_inputs).
    ///
    /// Source checkouts recompute and verify this value. Installed binaries use
    /// it to resolve the content-addressed release without needing kernel source
    /// files alongside the executable.
    #[serde(default)]
    pub kernel_sha: Option<String>,

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

    /// Branch or lightweight tag to build firecracker from
    #[serde(default)]
    pub firecracker_branch: Option<String>,

    /// Exact full commit required at `firecracker_branch`
    #[serde(default)]
    pub firecracker_commit: Option<String>,

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
    /// Check if this profile builds a custom kernel from source
    pub fn is_custom(&self) -> bool {
        !self.kernel_version.is_empty() && !self.kernel_repo.is_empty()
    }

    /// Check if this profile uses URL-based kernel download
    pub fn is_url_based(&self) -> bool {
        self.kernel_url.is_some()
    }

    /// This profile doesn't define its own kernel — it inherits from "default"
    pub fn inherits_kernel(&self) -> bool {
        !self.is_custom() && !self.is_url_based()
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
# Configure apt to retry on transient failures (e.g. 403 from mirrors)
echo 'Acquire::Retries "3";' > /etc/apt/apt.conf.d/80retries
apt-get update -qq
# Retry apt-get download up to 3 times with delay for transient mirror errors
for attempt in 1 2 3; do
    if apt-get install --download-only --yes --no-install-recommends {packages}; then
        break
    fi
    if [ "$attempt" -lt 3 ]; then
        echo "apt-get download failed (attempt $attempt/3), retrying in $((attempt * 10))s..."
        sleep $((attempt * 10))
        apt-get update -qq
    else
        echo "apt-get download failed after 3 attempts"
        exit 1
    fi
done
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

# Fix fstab: remove entries for partitions that don't exist in microVM.
# Ubuntu cloud images have LABEL=BOOT and LABEL=UEFI entries that cause
# systemd to enter emergency mode when these partitions are missing.
echo "FCVM Layer 2 Setup: Fixing fstab..."
sed -i '/LABEL=BOOT/d;/LABEL=UEFI/d' /newroot/etc/fstab 2>/dev/null || true

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

    // Disable services by removing symlinks from target.wants directories.
    // We run in chroot (no systemd), so `systemctl disable` fails silently.
    // Manually remove all symlinks that reference these units.
    if !plan.services.disable.is_empty() {
        s.push_str("# Disable services (remove symlinks from *.target.wants)\n");
        for svc in &plan.services.disable {
            // Bare names like "multipathd" match multipathd.service, multipathd.socket, etc.
            // Full names like "podman.service" match exactly.
            let pattern = if svc.contains('.') {
                svc.to_string()
            } else {
                format!("{}.*", svc)
            };
            s.push_str(&format!(
                "find /etc/systemd/system -name '{}' -type l -delete 2>/dev/null || true\n",
                pattern
            ));
        }
        s.push('\n');
    }

    // Remove podman state files created during package installation.
    // apt's post-install scripts initialize /var/lib/containers/storage with an
    // empty db.sql (driver=""). fc-agent writes storage.conf with the actual
    // driver at boot, but podman refuses to start if db.sql already exists with
    // a different driver. Remove state files but not directories (overlay/ may
    // be busy from postinst).
    s.push_str("# Remove podman state files created by apt post-install scripts\n");
    s.push_str("rm -f /var/lib/containers/storage/db.sql /var/lib/containers/storage/storage.lock /var/lib/containers/storage/userns.lock /var/lib/containers/storage/defaultNetworkBackend 2>/dev/null || true\n");
    s.push_str("rm -rf /var/lib/containers/storage/libpod /var/lib/containers/storage/overlay-containers /var/lib/containers/storage/overlay-images 2>/dev/null || true\n\n");

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

/// The directory fcvm's config lives in.
///
/// `FCVM_CONFIG_DIR` overrides everything, and exists for exactly one caller:
/// the test harness (`scripts/with-test-config.sh`), which needs a per-run
/// config directory. It deliberately does NOT ride on `XDG_CONFIG_HOME`: that
/// variable is shared with podman and skopeo, and pointing it at a bare temp
/// dir makes container tools re-resolve `containers/storage.conf` — with
/// version- and uid-dependent fallback rules, so the test's `podman build` and
/// fcvm's `skopeo`/`podman` calls can land on DIFFERENT image stores. Observed
/// both ways: rootless skopeo missing the configured graphroot (12 local test
/// failures), and a runner's root podman honouring the redirected config while
/// a nested env-reset `sudo podman build` did not ("image not known").
/// An fcvm-private variable cannot perturb any other tool.
pub fn fcvm_config_dir() -> Result<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("FCVM_CONFIG_DIR") {
        let dir = std::path::PathBuf::from(dir);
        // Relative paths are refused rather than resolved: the value crosses
        // process boundaries (nextest -> sudo -> fcvm -> nested launches) where
        // the working directory differs, and the directories crate's silent
        // fallback on bad XDG paths is exactly the trap this replaces.
        anyhow::ensure!(
            dir.is_absolute(),
            "FCVM_CONFIG_DIR must be absolute, got: {}",
            dir.display()
        );
        return Ok(dir.join("fcvm"));
    }
    let proj_dirs =
        ProjectDirs::from("", "", "fcvm").context("Could not determine config directory")?;
    Ok(proj_dirs.config_dir().to_path_buf())
}

/// Generate default config file at XDG config directory.
///
/// Writes the embedded default config to ~/.config/fcvm/rootfs-config.toml
pub fn generate_config(force: bool) -> Result<PathBuf> {
    let config_dir = fcvm_config_dir()?;
    let config_dir = config_dir.as_path();
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
///    5b. Current working directory (for test runners like nextest)
///    5c. CARGO_MANIFEST_DIR (debug builds only)
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

    // 1b. A --config recorded earlier in this process. Missing is an error, not a
    // reason to fall back: silently reading a different file is the failure this
    // whole path exists to remove.
    if let Some(p) = EXPLICIT_CONFIG.get() {
        if !p.exists() {
            bail!("Config file not found: {}", p.display());
        }
        return Ok(p.clone());
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
    if let Ok(config_dir) = fcvm_config_dir() {
        let p = config_dir.join(CONFIG_FILE);
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

    // 5b. Current working directory (for test runners like nextest)
    if let Ok(cwd) = std::env::current_dir() {
        let p = cwd.join(CONFIG_FILE);
        if p.exists() {
            return p.canonicalize().context("canonicalizing config path");
        }
    }

    // 5c. Check CARGO_MANIFEST_DIR for development builds (debug only)
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
         <binary-dir>/{}\n  \
         <cwd>/{}\n\n\
         Generate the default config with:\n  \
         fcvm setup --generate-config",
        CONFIG_FILE,
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

    let mut config: Plan = toml::from_str(&config_content)
        .with_context(|| format!("parsing config file: {}", config_path.display()))?;

    // A profile may override the VMM selection. Otherwise the explicit default
    // profile inherits the global Firecracker build just like every cold boot.
    apply_default_firecracker_config(&mut config);

    let arch = config_arch();
    if config
        .kernel_profiles
        .get("default")
        .and_then(|profiles| profiles.get(arch))
        .is_none()
    {
        bail!("rootfs config must define [kernel_profiles.default.{arch}] for this architecture");
    }

    info!(
        config_file = %config_path.display(),
        config_sha = %config_sha_short,
        "loaded rootfs config"
    );

    Ok((config, config_sha, config_sha_short))
}

/// Resolve the Firecracker binary for the rootfs setup VM.
///
/// FCVM_FIRECRACKER_BIN first, then PATH. Without this the setup VM shelled out
/// to a bare "firecracker", so a host that has never installed one system-wide
/// failed with a plain "No such file or directory" even though fcvm had just
/// built a Firecracker of its own under the assets directory.
fn setup_vm_firecracker_bin() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("FCVM_FIRECRACKER_BIN") {
        let p = PathBuf::from(&path);
        if !p.exists() {
            bail!("FCVM_FIRECRACKER_BIN={} does not exist", path);
        }
        return Ok(p);
    }
    which::which("firecracker").context(
        "firecracker not found in PATH; set FCVM_FIRECRACKER_BIN to the binary \
         fcvm built under <assets_dir>/firecracker/",
    )
}

/// Apply the global Firecracker selection to explicit default profiles.
///
/// A profile-level repository is an intentional override, so its repository,
/// branch, and commit remain untouched. Otherwise all three come from
/// `[firecracker]`; the commit travels with the repository so a profile build
/// keeps the full Firecracker identity that `setup` verifies.
fn apply_default_firecracker_config(plan: &mut Plan) {
    let Some(firecracker) = plan.firecracker.as_ref() else {
        return;
    };
    let Some(default_profiles) = plan.kernel_profiles.get_mut("default") else {
        return;
    };

    for profile in default_profiles.values_mut() {
        if profile.firecracker_repo.is_none() {
            profile.firecracker_repo = Some(firecracker.repo.clone());
            profile.firecracker_branch = Some(firecracker.branch.clone());
            profile.firecracker_commit = firecracker.commit.clone();
        }
    }
}

/// Load the discovered config path used by runtime helpers.
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

/// Resolve rootfs filesystem type from CLI override and kernel profile name.
///
/// Priority: explicit CLI `--rootfs-type` flag > kernel profile config > None (ext4 default).
/// Used by both `setup` and `podman run` commands.
pub fn resolve_rootfs_type(
    cli_rootfs_type: Option<&crate::cli::RootfsType>,
    kernel_profile_name: &str,
) -> Option<String> {
    // CLI override wins
    if let Some(rt) = cli_rootfs_type {
        return match rt {
            crate::cli::RootfsType::Btrfs => Some("btrfs".to_string()),
            crate::cli::RootfsType::Ext4 => None,
        };
    }

    // Read from kernel profile config
    if let Ok(Some(profile)) = get_kernel_profile(kernel_profile_name) {
        return profile.rootfs_type;
    }

    None
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
pub async fn ensure_rootfs(allow_create: bool, rootfs_type: Option<&str>) -> Result<PathBuf> {
    let (plan, _plan_sha_full, _plan_sha_short) = load_plan()?;

    // Generate all scripts and compute hash of the complete init script
    let setup_script = generate_setup_script(&plan);
    let install_script = generate_install_script();
    let init_script = generate_init_script(&install_script, &setup_script);
    let download_script = generate_download_script(&plan);

    // Hash the complete init script + download script + rootfs_type. The setup
    // kernel is deliberately excluded: it executes the initrd but is never
    // installed into Layer 2, so changing its release cannot change rootfs
    // contents and must not churn a multi-gigabyte rootfs artifact.
    // Any change to:
    // - init logic, install script, or setup script
    // - download method (podman image, codename, packages)
    // - rootfs filesystem type (ext4 vs btrfs)
    // invalidates the cache
    let mut combined = init_script.clone();
    combined.push_str("\n# DOWNLOAD_SCRIPT:\n");
    combined.push_str(&download_script);
    combined.push_str("\n# FC_AGENT_SERVICE:\n");
    combined.push_str(FC_AGENT_SERVICE);
    combined.push_str("\n# FC_AGENT_SERVICE_STRACE:\n");
    combined.push_str(FC_AGENT_SERVICE_STRACE);
    if let Some(fs_type) = rootfs_type {
        combined.push_str("\n# ROOTFS_TYPE: ");
        combined.push_str(fs_type);
    }
    let script_sha = compute_sha256(combined.as_bytes());
    let script_sha_short = &script_sha[..12];

    let rootfs_dir = paths::rootfs_dir();
    // Different rootfs types use different cache filenames
    let rootfs_path = if rootfs_type == Some("btrfs") {
        rootfs_dir.join(format!("layer2-{}-btrfs.raw", script_sha_short))
    } else {
        rootfs_dir.join(format!("layer2-{}.raw", script_sha_short))
    };
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

    // Acquire lock to prevent concurrent rootfs creation
    info!("acquiring rootfs creation lock");
    let flock = super::lock_store_dir(&lock_file, "rootfs creation").await?;

    // Check again after acquiring lock
    if rootfs_path.exists() {
        info!(
            path = %rootfs_path.display(),
            "rootfs exists (created by another process)"
        );
        flock.unlock().map_err(|(_, err)| err).ok();
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

    let result = create_layer2_rootless(
        &plan,
        script_sha_short,
        &setup_script,
        &temp_rootfs_path,
        rootfs_type,
    )
    .await;

    if result.is_ok() {
        super::publish_store_entry(&temp_rootfs_path, &rootfs_path, "rootfs").await?;
        info!(
            path = %rootfs_path.display(),
            script_sha = %script_sha_short,
            "Layer 2 rootfs creation complete"
        );
    } else {
        let _ = tokio::fs::remove_file(&temp_rootfs_path).await;
    }

    // Release lock. Deliberately leave the lock file in place (matching the kernel
    // and initrd locks): unlinking it lets a process still blocked in flock() on the
    // old inode and a new arrival that re-creates the path both "hold" the lock at
    // the same time, allowing two concurrent rootfs builds to clobber each other.
    flock
        .unlock()
        .map_err(|(_, err)| err)
        .context("releasing rootfs creation lock")?;

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
# Send stdout/stderr directly to kernel console (/dev/ttyS0).
# Do NOT use journal+console — journald crashes after snapshot restore.
# Delegate cgroup control so podman can use pids/memory/cpu controllers
Delegate=yes
StandardOutput=console
StandardError=console

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

# Install a systemd service that tells the host, on a guest `reboot`, to relaunch
# the VM in place (disk-only-clone semantics) instead of terminating.
#
# It is WantedBy=reboot.target, so it is pulled in ONLY on reboot — never on
# poweroff/halt (those isolate to poweroff.target/halt.target). It is ordered
# Before=systemd-reboot.service so it runs while the full rootfs is still mounted
# and the fc-agent binary + vsock are available — crucially BEFORE systemd pivots
# to the finalrd shutdown ramdisk (a system-shutdown hook would run in that
# minimal ramdisk, where /usr/local/bin/fc-agent does not exist).
#
# fc-agent's own post-container shutdown uses `poweroff -f`, which never reaches
# reboot.target, so a normal shutdown never emits the reboot signal.
cat > /newroot/etc/systemd/system/fcvm-reboot-notify.service << 'REBOOTUNIT'
[Unit]
Description=Signal fcvm host of reboot intent (relaunch in place)
DefaultDependencies=no
Before=systemd-reboot.service reboot.target shutdown.target
[Service]
Type=oneshot
ExecStart=/usr/local/bin/fc-agent --notify-reboot
TimeoutStartSec=5
[Install]
WantedBy=reboot.target
REBOOTUNIT
mkdir -p /newroot/etc/systemd/system/reboot.target.wants
ln -sf ../fcvm-reboot-notify.service \
    /newroot/etc/systemd/system/reboot.target.wants/fcvm-reboot-notify.service

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

    // Combined hash of all initrd contents: sha256(fc-agent bytes ++ init script
    // ++ service files). Reading + hashing the ~4.4MB fc-agent binary costs
    // ~17ms and sits on every VM-launch / clone hot path, so the result is
    // memoised per binary identity (path+mtime+size — the version-cache
    // pattern). The embedded scripts are compile-time constants; their hash
    // goes into the cache namespace so an fcvm rebuild that changes only the
    // scripts can never reuse a stale combined SHA.
    let scripts_sha = {
        let mut scripts = Vec::new();
        scripts.extend_from_slice(INITRD_INIT_SCRIPT.as_bytes());
        scripts.extend_from_slice(FC_AGENT_SERVICE.as_bytes());
        scripts.extend_from_slice(FC_AGENT_SERVICE_STRACE.as_bytes());
        compute_sha256(&scripts)
    };
    let initrd_sha = crate::version_cache::derived(
        &fc_agent_path,
        &format!("initrd-sha:{}", scripts_sha),
        || {
            let mut combined = std::fs::read(&fc_agent_path).with_context(|| {
                format!("reading fc-agent binary at {}", fc_agent_path.display())
            })?;
            combined.extend_from_slice(INITRD_INIT_SCRIPT.as_bytes());
            combined.extend_from_slice(FC_AGENT_SERVICE.as_bytes());
            combined.extend_from_slice(FC_AGENT_SERVICE_STRACE.as_bytes());
            Ok(compute_sha256(&combined))
        },
    )?;
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

    // Acquire exclusive lock to prevent race conditions
    let flock = super::lock_store_dir(
        &initrd_dir.join(format!("fc-agent-{}.lock", initrd_sha_short)),
        "initrd creation",
    )
    .await?;

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
    // A killed root run would otherwise leave a directory whose contents a
    // rootless run's remove_dir_all cannot unlink.
    super::give_store_entry_to_invoker(&temp_dir);

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
    super::publish_store_entry(&temp_initrd, &initrd_path, "initrd").await?;

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

/// Convert an ext4 image file to btrfs in-place using btrfs-convert.
///
/// Requires a clean ext4 filesystem (runs e2fsck first).
/// btrfs-convert works on image files, not just block devices.
async fn convert_to_btrfs(image_path: &Path) -> Result<()> {
    // Verify btrfs-convert is available (from btrfs-progs package)
    if which::which("btrfs-convert").is_err() {
        bail!("btrfs-convert not found — install btrfs-progs: apt-get install btrfs-progs");
    }

    // e2fsck first — btrfs-convert requires a clean ext4 filesystem
    let e2fsck_output = Command::new("e2fsck")
        .args(["-f", "-y", path_to_str(image_path)?])
        .output()
        .await
        .context("e2fsck before btrfs-convert")?;
    // e2fsck exit codes: 0=clean, 1=corrected, 2=corrected+reboot needed, >=4=uncorrectable
    if e2fsck_output.status.code().unwrap_or(255) >= 4 {
        bail!(
            "e2fsck found uncorrectable errors: {}",
            String::from_utf8_lossy(&e2fsck_output.stderr)
        );
    }

    let output = Command::new("btrfs-convert")
        .arg(path_to_str(image_path)?)
        .output()
        .await
        .context("running btrfs-convert")?;

    if !output.status.success() {
        bail!(
            "btrfs-convert failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    info!("rootfs converted from ext4 to btrfs");
    Ok(())
}

/// Create Layer 2 rootfs without requiring root
///
/// NOTE: fc-agent is NOT included - it will be injected per-VM at boot time.
async fn create_layer2_rootless(
    plan: &Plan,
    script_sha_short: &str,
    script: &str,
    output_path: &Path,
    rootfs_type: Option<&str>,
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

    // Step 4b: Convert ext4 to btrfs if requested (before setup VM boot)
    if rootfs_type == Some("btrfs") {
        info!("converting rootfs from ext4 to btrfs");
        convert_to_btrfs(&partition_path).await?;
    }

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

    // Step 7: Boot VM with initrd to run setup
    // Boots a partition (ext4 or btrfs) with root=/dev/vda
    // Uses btrfs kernel profile when rootfs is btrfs (needs CONFIG_BTRFS_FS)
    info!(
        script_sha = %script_sha_short,
        "booting VM with setup initrd (packages embedded)"
    );

    boot_vm_for_setup(&partition_path, &setup_initrd, rootfs_type).await?;

    // Step 8: Rename to final path
    tokio::fs::rename(&partition_path, output_path)
        .await
        .context("renaming partition to output path")?;

    info!("Layer 2 creation complete (packages embedded in initrd)");
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
/// Packages are embedded directly in the initrd, so the setup kernel does not
/// need ISO9660 or SquashFS support and no second disk is required.
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

    // If packages directory already exists with .deb files, use it.
    // The directory is populated under a temp name and renamed into place only
    // after the download succeeds, so its existence implies a complete set.
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

    // Download into a temp directory, then atomically rename to the final cache
    // path on success. An interrupted or failed download must never leave a
    // partial package set at the content-addressed path.
    let download_dir = cache_dir.join(format!("packages-{}.tmp", script_sha_short));
    let _ = tokio::fs::remove_dir_all(&packages_dir).await;
    let _ = tokio::fs::remove_dir_all(&download_dir).await;
    tokio::fs::create_dir_all(&download_dir).await?;
    super::give_store_entry_to_invoker(&cache_dir);

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

    // Pass through proxy environment variables, normalizing case.
    // For each protocol, check lowercase then uppercase. If neither is set,
    // fall back to the other protocol's value (e.g., HTTPS_PROXY → http_proxy)
    // since apt repos use http:// URLs but the env may only have HTTPS_PROXY.
    let http_val = std::env::var("http_proxy")
        .or_else(|_| std::env::var("HTTP_PROXY"))
        .ok();
    let https_val = std::env::var("https_proxy")
        .or_else(|_| std::env::var("HTTPS_PROXY"))
        .ok();
    let http_final = http_val.as_deref().or(https_val.as_deref());
    let https_final = https_val.as_deref().or(http_val.as_deref());
    if let Some(val) = http_final {
        podman_args.extend(["-e".to_string(), format!("http_proxy={}", val)]);
        podman_args.extend(["-e".to_string(), format!("HTTP_PROXY={}", val)]);
    }
    if let Some(val) = https_final {
        podman_args.extend(["-e".to_string(), format!("https_proxy={}", val)]);
        podman_args.extend(["-e".to_string(), format!("HTTPS_PROXY={}", val)]);
    }

    podman_args.extend([
        "-v".to_string(),
        format!("{}:/packages", download_dir.display()),
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
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = tokio::fs::remove_dir_all(&download_dir).await;
        bail!(
            "Package download failed (podman exited with {:?}). stdout={}, stderr={}",
            output.status.code(),
            stdout.trim(),
            stderr.trim()
        );
    }

    // Count downloaded packages
    let mut count = 0;
    if let Ok(mut entries) = tokio::fs::read_dir(&download_dir).await {
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
        let _ = tokio::fs::remove_dir_all(&download_dir).await;
        bail!(
            "No packages downloaded. stdout={}, stderr={}",
            stdout.trim(),
            stderr.trim()
        );
    }

    // Atomically publish the completed download as the cache directory.
    // The .deb files inside may stay root-owned after a sudo run; that is
    // fine — the handed-back parent directory is what governs a rootless
    // run's ability to read, replace, or unlink them.
    super::publish_store_entry(&download_dir, &packages_dir, "downloaded packages").await?;

    info!(path = %packages_dir.display(), count = count, "packages downloaded");
    Ok(packages_dir)
}

/// Download cloud image (cached by URL hash)
async fn download_cloud_image(plan: &Plan) -> Result<PathBuf> {
    let cache_dir = paths::cache_dir();
    tokio::fs::create_dir_all(&cache_dir)
        .await
        .context("creating cache directory")?;
    super::give_store_entry_to_invoker(&cache_dir);

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
    // Clean up any leftover temp file from a previous failed attempt
    let _ = tokio::fs::remove_file(&temp_path).await;

    // -f makes curl fail on HTTP errors instead of saving the error page as the
    // image; -S still prints the error despite --progress-bar.
    let output = Command::new("curl")
        .args([
            "-fSL",
            "-o",
            path_to_str(&temp_path)?,
            "--progress-bar",
            &arch_config.url,
        ])
        .status()
        .await
        .context("downloading cloud image")?;

    if !output.success() {
        let _ = tokio::fs::remove_file(&temp_path).await;
        bail!(
            "curl failed to download cloud image from {}",
            arch_config.url
        );
    }

    // Rename to final path
    super::publish_store_entry(&temp_path, &image_path, "downloaded cloud image").await?;

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
/// Only one disk is needed because packages are embedded in the initrd; the
/// setup kernel does not need ISO9660 or SquashFS support.
async fn boot_vm_for_setup(
    disk_path: &Path,
    initrd_path: &Path,
    rootfs_type: Option<&str>,
) -> Result<()> {
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

    // Find kernel — rootfs_type ("btrfs") matches the kernel profile name by convention.
    // Falls back to "default" profile for ext4 rootfs.
    let kernel_profile_name = rootfs_type.unwrap_or("default");
    let kernel_path = crate::setup::kernel::ensure_kernel(kernel_profile_name, true, false).await?;

    // Create serial console output file
    let serial_path = temp_dir.join("serial.log");
    let serial_file =
        std::fs::File::create(&serial_path).context("creating serial console file")?;

    // Start Firecracker with serial console output
    info!(
        "starting Firecracker for Layer 2 setup (serial output: {})",
        serial_path.display()
    );
    let firecracker_bin = setup_vm_firecracker_bin()?;
    let mut fc_cmd = Command::new(&firecracker_bin);
    fc_cmd
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
        // Several `?` between here and the wait loop below return without killing the VM.
        // kill_on_drop closes those windows while fcvm is still alive; pdeathsig covers the
        // case where fcvm itself dies without unwinding.
        .kill_on_drop(true);
    // The Layer 2 build boots a VM for MINUTES. Every other VMM spawn in fcvm goes
    // through this helper for PR_SET_PDEATHSIG; this one did not, so a `fcvm setup`
    // that died without running its cleanup (SIGKILL, a cancelled CI job) orphaned a
    // live Firecracker to init. No namespaces here — the setup VM runs on the host
    // network — so the helper installs only the parent-death hook.
    crate::utils::install_namespace_pre_exec(
        &mut fc_cmd,
        &crate::utils::NamespaceParams {
            vm_id: "layer2-setup".to_string(),
            ..Default::default()
        },
    )?;
    let mut fc_process = fc_cmd.spawn().context("starting Firecracker")?;

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

    // Set boot source - boot from raw partition (ext4 or btrfs, no GPT)
    // The disk IS the filesystem, so use root=/dev/vda directly
    // No cloud-init needed - scripts are injected via initrd
    client
        .set_boot_source(crate::firecracker::api::BootSource {
            kernel_image_path: kernel_path.display().to_string(),
            // Boot with initrd that runs setup before trying to use systemd
            // The initrd handles everything and powers off, so we don't need to worry about systemd
            boot_args: Some("console=ttyS0 reboot=k panic=1 pci=off".to_string()),
            initrd_path: Some(initrd_path.display().to_string()),
        })
        .await?;

    // Add root drive (raw filesystem, no partition table)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firecracker_commit_pins_deserialize_for_global_and_profile_configs() {
        let global: FirecrackerConfig = toml::from_str(
            r#"
repo = "ejc3/firecracker"
branch = "agent/nv2"
commit = "27305f49ab3a5d862dc56b5108713b6536d2baa7"
"#,
        )
        .unwrap();
        assert_eq!(
            global.commit.as_deref(),
            Some("27305f49ab3a5d862dc56b5108713b6536d2baa7")
        );

        let profile: KernelProfile = toml::from_str(
            r#"
firecracker_repo = "ejc3/firecracker"
firecracker_branch = "agent/nv2"
firecracker_commit = "27305f49ab3a5d862dc56b5108713b6536d2baa7"
"#,
        )
        .unwrap();
        assert_eq!(
            profile.firecracker_commit.as_deref(),
            Some("27305f49ab3a5d862dc56b5108713b6536d2baa7")
        );

        // The commit travels with the repository into every explicit default
        // profile. Dropping it here would leave a profile build with a branch
        // name and no pinned identity, which is what `setup` refuses to do.
        let mut plan: Plan = toml::from_str(EMBEDDED_CONFIG).unwrap();
        plan.firecracker = Some(global);
        apply_default_firecracker_config(&mut plan);

        for (arch, profile) in plan.kernel_profiles.get("default").unwrap() {
            assert_eq!(
                profile.firecracker_repo.as_deref(),
                Some("ejc3/firecracker"),
                "default.{arch} lost the global Firecracker repository"
            );
            assert_eq!(
                profile.firecracker_branch.as_deref(),
                Some("agent/nv2"),
                "default.{arch} lost the global Firecracker branch"
            );
            assert_eq!(
                profile.firecracker_commit.as_deref(),
                Some("27305f49ab3a5d862dc56b5108713b6536d2baa7"),
                "default.{arch} lost the pinned Firecracker commit"
            );
        }
    }

    /// FCVM_FIRECRACKER_BIN is a process-global, so these run in one test.
    #[test]
    fn setup_vm_firecracker_bin_honors_env_var() {
        let real = std::env::current_exe().expect("test binary path");

        // Set and existing: returned as-is, rather than searching PATH.
        std::env::set_var("FCVM_FIRECRACKER_BIN", &real);
        assert_eq!(setup_vm_firecracker_bin().unwrap(), real);

        // Set and missing: the error names the variable, so the reader knows
        // which knob is wrong instead of getting a bare ENOENT.
        std::env::set_var("FCVM_FIRECRACKER_BIN", "/nonexistent/firecracker");
        let err = setup_vm_firecracker_bin().unwrap_err().to_string();
        assert!(
            err.contains("FCVM_FIRECRACKER_BIN"),
            "error should name the variable, got: {err}"
        );

        std::env::remove_var("FCVM_FIRECRACKER_BIN");
    }

    /// A --config path that does not exist must fail, not quietly fall back to
    /// the discovered config, which is the bug this whole path removes.
    #[test]
    fn find_config_file_rejects_a_missing_explicit_path() {
        let err = find_config_file(Some("/nonexistent/rootfs-config.toml"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Config file not found"),
            "should refuse a missing --config, got: {err}"
        );
    }

    #[test]
    fn explicit_default_profiles_inherit_global_firecracker_without_overriding_profile() {
        let mut plan: Plan = toml::from_str(EMBEDDED_CONFIG).unwrap();
        let arm64 = plan
            .kernel_profiles
            .get_mut("default")
            .unwrap()
            .get_mut("arm64")
            .unwrap();
        arm64.firecracker_repo = Some("owner/profile-firecracker".to_string());
        arm64.firecracker_branch = Some("profile-branch".to_string());

        apply_default_firecracker_config(&mut plan);

        let defaults = plan.kernel_profiles.get("default").unwrap();
        let arm64 = defaults.get("arm64").unwrap();
        assert_eq!(
            arm64.firecracker_repo.as_deref(),
            Some("owner/profile-firecracker")
        );
        assert_eq!(arm64.firecracker_branch.as_deref(), Some("profile-branch"));

        let amd64 = defaults.get("amd64").unwrap();
        assert_eq!(amd64.firecracker_repo.as_deref(), Some("ejc3/firecracker"));
        assert_eq!(
            amd64.firecracker_branch.as_deref(),
            Some("bump-vsock-max-connections")
        );
    }
}
