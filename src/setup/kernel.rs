use anyhow::{bail, Context, Result};
use glob::glob;
use nix::fcntl::{Flock, FlockArg};
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::paths;
use crate::setup::rootfs::{get_kernel_profile, load_plan, KernelProfile};
use crate::utils::run_streaming;

/// Compute SHA256 of bytes, return hex string (first 12 chars)
fn compute_sha256_short(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    hex::encode(&result[..6]) // 12 hex chars
}

// ============================================================================
// Unified Kernel API
// ============================================================================

/// Ensure kernel exists, downloading or building if needed.
///
/// - If `profile` is None: uses default kernel from [kernel] section
/// - If `profile` is Some("name"): uses [kernel_profiles.name] section
///
/// If `allow_create` is false, bails if kernel doesn't exist.
/// If `allow_build` is true, falls back to local build for custom profiles.
pub async fn ensure_kernel(
    profile: Option<&str>,
    allow_create: bool,
    allow_build: bool,
) -> Result<PathBuf> {
    match profile {
        None => ensure_default_kernel(allow_create).await,
        Some(name) => ensure_profile_kernel(name, allow_create, allow_build).await,
    }
}

/// Get kernel path (without downloading/building).
///
/// Returns the path where the kernel should exist.
/// Used to check existence before running VM.
pub fn get_kernel_path(profile: Option<&str>) -> Result<PathBuf> {
    match profile {
        None => get_default_kernel_path(),
        Some(name) => get_profile_kernel_path(name),
    }
}

/// Get the kernel URL hash for the default kernel.
/// This is used to include in Layer 2 SHA calculation.
pub fn get_kernel_url_hash() -> Result<String> {
    let (plan, _, _) = load_plan()?;
    let kernel_config = plan.kernel.current_arch()?;
    Ok(compute_sha256_short(kernel_config.url.as_bytes()))
}

// ============================================================================
// Default Kernel (from [kernel] section)
// ============================================================================

fn get_default_kernel_path() -> Result<PathBuf> {
    let (plan, _, _) = load_plan()?;
    let kernel_config = plan.kernel.current_arch()?;
    let url_hash = compute_sha256_short(kernel_config.url.as_bytes());
    Ok(paths::kernel_dir().join(format!("vmlinux-{}.bin", url_hash)))
}

async fn ensure_default_kernel(allow_create: bool) -> Result<PathBuf> {
    let (plan, _, _) = load_plan()?;
    let kernel_config = plan.kernel.current_arch()?;

    // Check for local path first
    if let Some(local_path) = &kernel_config.local_path {
        let path = PathBuf::from(local_path);
        if !path.exists() {
            bail!("Kernel local_path not found: {}", path.display());
        }
        info!(path = %path.display(), "using local kernel");
        return Ok(path);
    }

    if kernel_config.url.is_empty() {
        bail!("Kernel config must specify 'url' or 'local_path'");
    }

    let kernel_dir = paths::kernel_dir();
    let url_hash = compute_sha256_short(kernel_config.url.as_bytes());
    let kernel_path = kernel_dir.join(format!("vmlinux-{}.bin", url_hash));

    // Fast path: already exists
    if kernel_path.exists() {
        info!(path = %kernel_path.display(), url_hash = %url_hash, "kernel already exists");
        return Ok(kernel_path);
    }

    if !allow_create {
        bail!("Kernel not found. Run 'fcvm setup' first, or use --setup flag.");
    }

    // Create directory and acquire lock
    tokio::fs::create_dir_all(&kernel_dir)
        .await
        .context("creating kernel directory")?;

    let lock_file = kernel_dir.join(format!("vmlinux-{}.lock", url_hash));
    use std::os::unix::fs::OpenOptionsExt;
    let lock_fd = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&lock_file)
        .context("opening kernel lock file")?;

    let flock = Flock::lock(lock_fd, FlockArg::LockExclusive)
        .map_err(|(_, err)| err)
        .context("acquiring exclusive lock for kernel download")?;

    // Double-check after lock
    if kernel_path.exists() {
        debug!(path = %kernel_path.display(), "kernel exists (created by another process)");
        flock.unlock().map_err(|(_, err)| err)?;
        return Ok(kernel_path);
    }

    // Download
    println!("⚙️  Downloading kernel...");
    info!(url = %kernel_config.url, path_in_archive = %kernel_config.path, "downloading kernel");

    let cache_dir = paths::cache_dir();
    tokio::fs::create_dir_all(&cache_dir).await?;

    let tarball_path = cache_dir.join(format!("kernel-{}.tar.zst", url_hash));

    // Download tarball if not cached
    if !tarball_path.exists() {
        println!("  → Downloading tarball...");
        let output = Command::new("curl")
            .args(["-fSL", &kernel_config.url, "-o"])
            .arg(&tarball_path)
            .output()
            .await
            .context("running curl")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = flock.unlock();
            bail!("Failed to download kernel: {}", stderr);
        }
    } else {
        info!(path = %tarball_path.display(), "using cached tarball");
    }

    // Extract kernel from tarball
    println!("  → Extracting kernel...");
    let extract_path = format!("./{}", kernel_config.path);

    let output = Command::new("tar")
        .args(["--use-compress-program=zstd", "-xf"])
        .arg(&tarball_path)
        .arg("-C")
        .arg(&cache_dir)
        .arg(&extract_path)
        .output()
        .await
        .context("extracting kernel from tarball")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = flock.unlock();
        bail!("Failed to extract kernel: {}", stderr);
    }

    // Move to final location
    let extracted_path = cache_dir.join(&kernel_config.path);
    if !extracted_path.exists() {
        let _ = flock.unlock();
        bail!(
            "Kernel not found after extraction at {}",
            extracted_path.display()
        );
    }

    tokio::fs::copy(&extracted_path, &kernel_path)
        .await
        .context("copying kernel to final location")?;

    // Clean up extracted files
    let opt_dir = cache_dir.join("opt");
    if opt_dir.exists() {
        tokio::fs::remove_dir_all(&opt_dir).await.ok();
    }

    println!("  ✓ Kernel ready");
    info!(path = %kernel_path.display(), url_hash = %url_hash, "kernel ready");

    flock.unlock().map_err(|(_, err)| err)?;
    Ok(kernel_path)
}

// ============================================================================
// Profile Kernel (from [kernel_profiles] section)
// ============================================================================

fn get_profile_kernel_path(profile_name: &str) -> Result<PathBuf> {
    let profile = get_kernel_profile(profile_name)?
        .ok_or_else(|| anyhow::anyhow!("kernel profile '{}' not found in config", profile_name))?;

    if !profile.is_custom() {
        bail!(
            "kernel profile '{}' has no kernel source configured.\n\
             Add kernel_version + kernel_repo to [kernel_profiles.{}]",
            profile_name,
            profile_name
        );
    }

    let sha = compute_profile_kernel_sha(&profile);
    let filename = custom_kernel_filename(profile_name, &profile.kernel_version, &sha);
    Ok(paths::kernel_dir().join(filename))
}

async fn ensure_profile_kernel(
    profile_name: &str,
    allow_create: bool,
    allow_build: bool,
) -> Result<PathBuf> {
    let profile = get_kernel_profile(profile_name)?.ok_or_else(|| {
        anyhow::anyhow!(
            "kernel profile '{}' not found in config. \
             Add [kernel_profiles.{}] section to rootfs-config.toml",
            profile_name,
            profile_name
        )
    })?;

    if !profile.is_custom() {
        bail!(
            "kernel profile '{}' has no kernel source configured.\n\
             Add kernel_version + kernel_repo to [kernel_profiles.{}]",
            profile_name,
            profile_name
        );
    }

    let sha = compute_profile_kernel_sha(&profile);
    let filename = custom_kernel_filename(profile_name, &profile.kernel_version, &sha);
    let kernel_dir = paths::kernel_dir();
    let kernel_path = kernel_dir.join(&filename);

    // Fast path: already exists
    if kernel_path.exists() {
        info!(
            path = %kernel_path.display(),
            profile = %profile_name,
            sha = %sha,
            "kernel already exists"
        );
        return Ok(kernel_path);
    }

    if !allow_create {
        bail!(
            "Kernel not found for profile '{}' at {}.\n\
             Run: fcvm setup --kernel-profile {}",
            profile_name,
            kernel_path.display(),
            profile_name
        );
    }

    // Create directory and acquire lock
    tokio::fs::create_dir_all(&kernel_dir)
        .await
        .context("creating kernel directory")?;

    let lock_file = kernel_dir.join(format!("{}.lock", filename));
    use std::os::unix::fs::OpenOptionsExt;
    let lock_fd = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&lock_file)
        .context("opening kernel lock file")?;

    let flock = Flock::lock(lock_fd, FlockArg::LockExclusive)
        .map_err(|(_, err)| err)
        .context("acquiring exclusive lock for kernel")?;

    // Double-check after lock
    if kernel_path.exists() {
        debug!(path = %kernel_path.display(), "kernel exists (created by another process)");
        flock.unlock().map_err(|(_, err)| err)?;
        return Ok(kernel_path);
    }

    // Try to download from GitHub releases
    let tag = format!(
        "kernel-{}-{}-{}-{}",
        profile_name,
        profile.kernel_version,
        std::env::consts::ARCH,
        sha
    );
    let download_url = format!(
        "https://github.com/{}/releases/download/{}/{}",
        profile.kernel_repo, tag, filename
    );

    println!("⚙️  Downloading kernel (profile: {})...", profile_name);
    info!(url = %download_url, tag = %tag, "downloading kernel from GitHub releases");

    let download_result = download_kernel_binary(&download_url, &kernel_path).await;

    match download_result {
        Ok(_) => {
            println!("  ✓ Kernel ready (profile: {})", profile_name);
            info!(path = %kernel_path.display(), profile = %profile_name, "kernel ready");
            flock.unlock().map_err(|(_, err)| err)?;
            Ok(kernel_path)
        }
        Err(e) => {
            warn!(error = %e, profile = %profile_name, "download failed");

            if allow_build {
                println!("  → Building locally (may take 10-20 minutes)...");
                build_kernel_locally(&profile, profile_name, &kernel_path).await?;
                println!("  ✓ Kernel built (profile: {})", profile_name);
                flock.unlock().map_err(|(_, err)| err)?;
                Ok(kernel_path)
            } else {
                flock.unlock().map_err(|(_, err)| err)?;
                bail!(
                    "Failed to download '{}' kernel: {}\n\n\
                     Options:\n\
                     1. Build locally: fcvm setup --kernel-profile {} --build-kernels\n\
                     2. Build manually: ./kernel/build.sh\n\
                     3. Wait for CI to publish pre-built kernel",
                    profile_name,
                    e,
                    profile_name
                );
            }
        }
    }
}

// ============================================================================
// Custom Kernel Helpers
// ============================================================================

/// Find the repo root by looking for Cargo.toml going up the directory tree.
fn find_repo_root() -> Option<PathBuf> {
    // Try CWD first
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if dir.join("Cargo.toml").exists() && dir.join("rootfs-config.toml").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            break;
        }
    }

    // Try relative to executable
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            // Check a few levels up from target/release/fcvm
            for ancestor in exe_dir.ancestors().take(5) {
                if ancestor.join("Cargo.toml").exists()
                    && ancestor.join("rootfs-config.toml").exists()
                {
                    return Some(ancestor.to_path_buf());
                }
            }
        }
    }

    None
}

/// Compute SHA for custom kernel based on build inputs from profile config.
///
/// Reads the files listed in `profile.build_inputs` (supports globs) and
/// computes SHA256 of their concatenated contents. This is purely config-driven -
/// the binary has no hardcoded knowledge of which files matter.
///
/// Patterns are resolved relative to the repo root (directory containing Cargo.toml
/// and rootfs-config.toml).
pub fn compute_profile_kernel_sha(profile: &KernelProfile) -> String {
    if profile.build_inputs.is_empty() {
        warn!("kernel profile has no build_inputs, using empty SHA");
        return "000000000000".to_string();
    }

    // Find repo root for relative path resolution
    let repo_root = find_repo_root();
    if let Some(ref root) = repo_root {
        debug!(repo_root = %root.display(), "found repo root for build_inputs");
    } else {
        debug!("repo root not found, using CWD for build_inputs");
    }

    let mut content = Vec::new();

    for pattern in &profile.build_inputs {
        // If pattern is relative and we have a repo root, prepend it
        let full_pattern = if !pattern.starts_with('/') {
            if let Some(ref root) = repo_root {
                root.join(pattern).to_string_lossy().into_owned()
            } else {
                pattern.clone()
            }
        } else {
            pattern.clone()
        };

        // Expand glob pattern
        let paths: Vec<PathBuf> = match glob(&full_pattern) {
            Ok(entries) => {
                let mut paths: Vec<PathBuf> = entries
                    .filter_map(|e| e.ok())
                    // Filter out .disabled files (allows disabling patches without changing SHA)
                    .filter(|p| !p.to_string_lossy().ends_with(".disabled"))
                    .collect();
                paths.sort(); // Deterministic order
                paths
            }
            Err(e) => {
                warn!(pattern = %full_pattern, error = %e, "invalid glob pattern");
                continue;
            }
        };

        if paths.is_empty() {
            debug!(pattern = %full_pattern, "no files matched pattern");
        }

        for path in paths {
            match std::fs::read(&path) {
                Ok(data) => {
                    debug!(path = %path.display(), bytes = data.len(), "hashing build input");
                    content.extend(data);
                }
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "failed to read build input");
                }
            }
        }
    }

    if content.is_empty() {
        warn!("no build input files found, using empty SHA");
        return "000000000000".to_string();
    }

    compute_sha256_short(&content)
}

/// Get the custom kernel filename.
pub fn custom_kernel_filename(profile_name: &str, kernel_version: &str, sha: &str) -> String {
    format!(
        "vmlinux-{}-{}-{}-{}.bin",
        profile_name,
        kernel_version,
        std::env::consts::ARCH,
        sha
    )
}

async fn download_kernel_binary(url: &str, dest: &Path) -> Result<()> {
    let temp_path = dest.with_extension("downloading");

    let output = Command::new("curl")
        .args(["-fSL", url, "-o"])
        .arg(&temp_path)
        .output()
        .await
        .context("running curl")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = tokio::fs::remove_file(&temp_path).await;
        bail!("curl failed: {}", stderr);
    }

    // Verify it's a valid kernel binary
    let output = Command::new("file")
        .arg(&temp_path)
        .output()
        .await
        .context("running file command")?;

    let file_type = String::from_utf8_lossy(&output.stdout);
    if !file_type.contains("ELF") && !file_type.contains("Linux kernel") {
        let _ = tokio::fs::remove_file(&temp_path).await;
        bail!("Downloaded file is not a valid kernel: {}", file_type);
    }

    tokio::fs::rename(&temp_path, dest)
        .await
        .context("moving kernel to final location")?;

    Ok(())
}

/// Generate VM kernel build script dynamically from profile config.
///
/// The script is written to a temp file and executed. This allows us to:
/// - Factor common logic between VM and host kernel builds
/// - Drive all config (version, URLs, paths) from TOML
/// - Not maintain separate shell scripts in source control
fn generate_vm_kernel_build_script(
    profile: &KernelProfile,
    sha: &str,
    dest: &Path,
    repo_root: &Path,
) -> Result<String> {
    let kernel_version = &profile.kernel_version;
    let kernel_major = kernel_version.split('.').next().unwrap_or(kernel_version);

    // Get architecture-specific values
    let (kernel_arch, kernel_image) = match std::env::consts::ARCH {
        "aarch64" => ("arm64", "Image"),
        "x86_64" => ("x86_64", "bzImage"),
        arch => bail!("Unsupported architecture: {}", arch),
    };

    let arch_for_url = match std::env::consts::ARCH {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        arch => bail!("Unsupported architecture: {}", arch),
    };

    // Get config from profile
    let patches_dir = profile
        .patches_dir
        .as_deref()
        .map(|p| repo_root.join(p))
        .unwrap_or_else(|| repo_root.join("kernel/patches"));

    let kernel_config = profile.kernel_config.as_deref().map(|p| repo_root.join(p));

    let base_config_url = profile
        .base_config_url
        .as_deref()
        .map(|url| url.replace("{arch}", arch_for_url))
        .unwrap_or_else(|| {
            format!(
                "https://raw.githubusercontent.com/firecracker-microvm/firecracker/main/resources/guest_configs/microvm-kernel-ci-{}-6.1.config",
                arch_for_url
            )
        });

    let script = format!(
        r##"#!/bin/bash
# Generated VM kernel build script
# DO NOT EDIT - generated by fcvm from rootfs-config.toml
set -euo pipefail

KERNEL_VERSION="{kernel_version}"
KERNEL_MAJOR="{kernel_major}"
BUILD_DIR="${{BUILD_DIR:-/tmp/kernel-build}}"
NPROC="${{NPROC:-$(nproc)}}"
SOURCE_DIR="$BUILD_DIR/linux-${{KERNEL_VERSION}}"
SHA_MARKER="$SOURCE_DIR/.fcvm-patches-sha"
BUILD_SHA="{sha}"
KERNEL_PATH="{kernel_path}"
PATCHES_DIR="{patches_dir}"
KERNEL_ARCH="{kernel_arch}"
KERNEL_IMAGE="{kernel_image}"
BASE_CONFIG_URL="{base_config_url}"
{kernel_config_line}

echo "=== fcvm VM Kernel Build ==="
echo "Kernel version: $KERNEL_VERSION"
echo "Architecture: $KERNEL_ARCH"
echo "Build SHA: $BUILD_SHA"
echo "Output: $KERNEL_PATH"
echo ""

# Check if already built
if [[ -f "$KERNEL_PATH" ]]; then
    echo "Kernel already exists: $KERNEL_PATH"
    echo "Skipping build."
    exit 0
fi

# Create directories
mkdir -p "$(dirname "$KERNEL_PATH")" "$BUILD_DIR"
cd "$BUILD_DIR"

# Download kernel source if needed
KERNEL_TARBALL="linux-${{KERNEL_VERSION}}.tar.xz"
KERNEL_URL="https://cdn.kernel.org/pub/linux/kernel/v${{KERNEL_MAJOR}}.x/${{KERNEL_TARBALL}}"

if [[ ! -f "$KERNEL_TARBALL" ]]; then
    echo "Downloading kernel source..."
    curl -fSL "$KERNEL_URL" -o "$KERNEL_TARBALL"
fi

# Check if source exists and has matching SHA
if [[ -d "$SOURCE_DIR" ]]; then
    if [[ -f "$SHA_MARKER" ]] && [[ "$(cat "$SHA_MARKER")" == "$BUILD_SHA" ]]; then
        echo "Source already patched with current SHA, reusing..."
    else
        echo "Source exists but SHA mismatch (patches changed), re-extracting..."
        rm -rf "$SOURCE_DIR"
    fi
fi

if [[ ! -d "$SOURCE_DIR" ]]; then
    echo "Extracting kernel source..."
    tar xf "$KERNEL_TARBALL"
fi

cd "$SOURCE_DIR"

# Apply patches (VM kernel applies all: *.patch + *.vm.patch)
if [[ -f "$SHA_MARKER" ]] && [[ "$(cat "$SHA_MARKER")" == "$BUILD_SHA" ]]; then
    echo "Patches already applied (SHA: $BUILD_SHA)"
else
    echo "Applying patches..."

    # Track applied patches to avoid duplicates (*.patch glob also matches *.vm.patch)
    declare -A applied_patches

    for patch_file in "$PATCHES_DIR"/*.patch "$PATCHES_DIR"/*.vm.patch; do
        [[ ! -f "$patch_file" ]] && continue
        [[ -n "${{applied_patches[$patch_file]:-}}" ]] && continue
        applied_patches[$patch_file]=1
        patch_name=$(basename "$patch_file")

        echo "  Checking $patch_name..."
        if patch -p1 --forward --dry-run < "$patch_file" >/dev/null 2>&1; then
            echo "  Applying $patch_name..."
            patch -p1 --forward < "$patch_file"
        else
            # Check if already applied (reversed)
            if patch -p1 --reverse --dry-run < "$patch_file" >/dev/null 2>&1; then
                echo "    Already applied: $patch_name"
            else
                echo "    ERROR: Patch does not apply cleanly: $patch_name"
                patch -p1 --forward --dry-run < "$patch_file" || true
                cd "$BUILD_DIR"
                rm -rf "$SOURCE_DIR"
                echo "    Re-run this script to rebuild from fresh source."
                exit 1
            fi
        fi
    done

    echo "$BUILD_SHA" > "$SHA_MARKER"
    echo "Patches applied successfully (SHA: $BUILD_SHA)"
fi

# Download Firecracker base config
echo "Downloading Firecracker base config..."
curl -fSL "$BASE_CONFIG_URL" -o .config

# Apply options from config fragment
{apply_config_fragment}

# Always enable BTRFS
./scripts/config --enable CONFIG_BTRFS_FS

# Update config with defaults for new options
make ARCH="$KERNEL_ARCH" olddefconfig

# Show enabled options
echo ""
echo "Verifying configuration:"
grep -E "^CONFIG_(FUSE_FS|KVM|VIRTUALIZATION|BTRFS_FS|TUN|VETH)=" .config || true
echo ""

# Build kernel
echo "Building kernel with $NPROC parallel jobs..."
make ARCH="$KERNEL_ARCH" -j"$NPROC" "$KERNEL_IMAGE"

# Copy output
echo "Copying kernel to $KERNEL_PATH..."
case "$KERNEL_ARCH" in
    arm64)  cp "arch/arm64/boot/Image" "$KERNEL_PATH" ;;
    x86_64) cp "arch/x86/boot/bzImage" "$KERNEL_PATH" ;;
esac

echo ""
echo "=== Build Complete ==="
echo "Kernel: $KERNEL_PATH"
echo "Size: $(du -h "$KERNEL_PATH" | cut -f1)"
"##,
        kernel_version = kernel_version,
        kernel_major = kernel_major,
        sha = sha,
        kernel_path = dest.display(),
        patches_dir = patches_dir.display(),
        kernel_arch = kernel_arch,
        kernel_image = kernel_image,
        base_config_url = base_config_url,
        kernel_config_line = kernel_config
            .as_ref()
            .map(|p| format!("KERNEL_CONFIG=\"{}\"", p.display()))
            .unwrap_or_default(),
        apply_config_fragment = if kernel_config.is_some() {
            r#"if [[ -n "${KERNEL_CONFIG:-}" ]] && [[ -f "$KERNEL_CONFIG" ]]; then
    echo "Applying options from $KERNEL_CONFIG..."
    while IFS= read -r line; do
        [[ "$line" =~ ^[[:space:]]*# ]] && continue
        [[ -z "${line// }" ]] && continue
        if [[ "$line" =~ ^(CONFIG_[A-Z0-9_]+)=y ]]; then
            opt="${BASH_REMATCH[1]}"
            echo "  Enabling $opt"
            ./scripts/config --enable "$opt"
        fi
    done < "$KERNEL_CONFIG"
fi"#
        } else {
            "# No config fragment specified"
        },
    );

    Ok(script)
}

async fn build_kernel_locally(
    profile: &KernelProfile,
    profile_name: &str,
    dest: &Path,
) -> Result<()> {
    // Find repo root for config file paths
    let repo_root = find_repo_root().ok_or_else(|| {
        anyhow::anyhow!(
            "Cannot find fcvm repository root.\n\n\
             Local builds require the fcvm git repository.\n\
             Clone it and run: cargo run -- setup --kernel-profile {} --build-kernels",
            profile_name
        )
    })?;

    // Compute SHA for this build
    let sha = compute_profile_kernel_sha(profile);

    // Generate the build script
    let script_content = generate_vm_kernel_build_script(profile, &sha, dest, &repo_root)?;

    // Write to temp file
    let script_path = std::env::temp_dir().join(format!("fcvm-kernel-build-{}.sh", sha));
    std::fs::write(&script_path, &script_content).context("writing build script")?;
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
        .context("setting script permissions")?;

    info!(script = %script_path.display(), "generated kernel build script");

    let cmd = Command::new(&script_path);
    let status = run_streaming(cmd, "kernel_build")
        .await
        .context("running build script")?;

    // Clean up script
    let _ = std::fs::remove_file(&script_path);

    if !status.success() {
        bail!("Kernel build failed with exit code: {:?}", status.code());
    }

    if !dest.exists() {
        bail!("Build completed but kernel not found at {}", dest.display());
    }

    Ok(())
}

// ============================================================================
// Host Kernel Installation (for EC2 setup)
// ============================================================================

use crate::setup::rootfs::HostKernelConfig;

/// Generate host kernel build script dynamically from config.
///
/// Uses the running kernel's config as base (includes EC2/AWS modules),
/// applies fcvm patches (only *.patch, skips *.vm.patch), and builds deb packages.
fn generate_host_kernel_build_script(
    config: &HostKernelConfig,
    sha: &str,
    repo_root: &Path,
) -> Result<String> {
    let kernel_version = &config.kernel_version;
    let kernel_major = kernel_version.split('.').next().unwrap_or(kernel_version);

    let patches_dir = config
        .patches_dir
        .as_deref()
        .map(|p| repo_root.join(p))
        .unwrap_or_else(|| repo_root.join("kernel/patches"));

    let script = format!(
        r##"#!/bin/bash
# Generated host kernel build script
# DO NOT EDIT - generated by fcvm from rootfs-config.toml
#
# Uses the running kernel's config as base (includes EC2/AWS modules),
# applies fcvm patches, and builds deb packages for installation.
set -euo pipefail

KERNEL_VERSION="{kernel_version}"
KERNEL_MAJOR="{kernel_major}"
BUILD_DIR="${{BUILD_DIR:-/tmp/kernel-build-host}}"
NPROC="${{NPROC:-$(nproc)}}"
SOURCE_DIR="$BUILD_DIR/linux-${{KERNEL_VERSION}}"
SHA_MARKER="$SOURCE_DIR/.fcvm-patches-sha"
BUILD_SHA="{sha}"
PATCHES_DIR="{patches_dir}"
LOCALVERSION="-fcvm-${{BUILD_SHA}}"
DEB_NAME="linux-image-${{KERNEL_VERSION}}${{LOCALVERSION}}"

echo "=== fcvm Host Kernel Build ==="
echo "Kernel version: $KERNEL_VERSION"
echo "Build SHA: $BUILD_SHA"
echo "LOCALVERSION: $LOCALVERSION"
echo ""

# Check if already built (look for installed deb or deb file)
if dpkg -l 2>/dev/null | grep -q "${{DEB_NAME}}"; then
    echo "Kernel already installed: ${{DEB_NAME}}"
    echo "Skipping build."
    exit 0
fi

if ls "$BUILD_DIR"/${{DEB_NAME}}*.deb 2>/dev/null | head -1; then
    echo "Deb already built: $(ls "$BUILD_DIR"/${{DEB_NAME}}*.deb | head -1)"
    echo "Run: sudo dpkg -i $BUILD_DIR/${{DEB_NAME}}*.deb"
    exit 0
fi

# Create build directory
mkdir -p "$BUILD_DIR"
cd "$BUILD_DIR"

# Download kernel source if needed
KERNEL_TARBALL="linux-${{KERNEL_VERSION}}.tar.xz"
KERNEL_URL="https://cdn.kernel.org/pub/linux/kernel/v${{KERNEL_MAJOR}}.x/${{KERNEL_TARBALL}}"

if [[ ! -f "$KERNEL_TARBALL" ]]; then
    echo "Downloading kernel source..."
    curl -fSL "$KERNEL_URL" -o "$KERNEL_TARBALL"
fi

# Check if source exists and has matching SHA
if [[ -d "$SOURCE_DIR" ]]; then
    if [[ -f "$SHA_MARKER" ]] && [[ "$(cat "$SHA_MARKER")" == "$BUILD_SHA" ]]; then
        echo "Source already patched with current SHA, reusing..."
    else
        echo "Source exists but SHA mismatch (patches changed), re-extracting..."
        rm -rf "$SOURCE_DIR"
    fi
fi

if [[ ! -d "$SOURCE_DIR" ]]; then
    echo "Extracting kernel source..."
    tar xf "$KERNEL_TARBALL"
fi

cd "$SOURCE_DIR"

# Apply patches (host kernel: *.patch only, skip *.vm.patch)
if [[ -f "$SHA_MARKER" ]] && [[ "$(cat "$SHA_MARKER")" == "$BUILD_SHA" ]]; then
    echo "Patches already applied (SHA: $BUILD_SHA)"
else
    echo "Applying patches..."
    for patch_file in "$PATCHES_DIR"/*.patch; do
        [[ ! -f "$patch_file" ]] && continue
        [[ "$patch_file" == *.vm.patch ]] && continue  # Skip VM-only patches
        patch_name=$(basename "$patch_file")

        echo "  Checking $patch_name..."
        if patch -p1 --forward --dry-run < "$patch_file" >/dev/null 2>&1; then
            echo "  Applying $patch_name..."
            patch -p1 --forward < "$patch_file"
        else
            # Check if already applied (reversed)
            if patch -p1 --reverse --dry-run < "$patch_file" >/dev/null 2>&1; then
                echo "    Already applied: $patch_name"
            else
                echo "    ERROR: Patch does not apply cleanly: $patch_name"
                patch -p1 --forward --dry-run < "$patch_file" || true
                cd "$BUILD_DIR"
                rm -rf "$SOURCE_DIR"
                echo "    Re-run this script to rebuild from fresh source."
                exit 1
            fi
        fi
    done

    # Mark source as patched with this SHA
    echo "$BUILD_SHA" > "$SHA_MARKER"
    echo "Patches applied successfully (SHA: $BUILD_SHA)"
fi

# Copy current kernel config as base (includes all EC2/AWS modules)
echo "Using current kernel config as base..."
CURRENT_VERSION=$(uname -r)
if [[ -f "/boot/config-${{CURRENT_VERSION}}" ]]; then
    cp "/boot/config-${{CURRENT_VERSION}}" .config
    echo "  Copied /boot/config-${{CURRENT_VERSION}}"
elif [[ -f /proc/config.gz ]]; then
    zcat /proc/config.gz > .config
    echo "  Extracted from /proc/config.gz"
else
    echo "ERROR: Cannot find current kernel config"
    exit 1
fi

# Update config for new kernel version
echo "Updating config for kernel ${{KERNEL_VERSION}}..."
make ARCH=arm64 olddefconfig

# Build deb packages
echo ""
echo "Building kernel deb packages with $NPROC parallel jobs..."
echo "LOCALVERSION=$LOCALVERSION"
echo "This takes 15-30 minutes..."
echo ""

make -j"$NPROC" ARCH=arm64 LOCALVERSION="$LOCALVERSION" bindeb-pkg

echo ""
echo "=== Build Complete ==="
echo "Deb packages:"
ls -la "$BUILD_DIR"/*.deb | grep -v dbg || true
echo ""
echo "To install:"
echo "  sudo dpkg -i $BUILD_DIR/linux-image-${{KERNEL_VERSION}}${{LOCALVERSION}}*.deb"
echo "  sudo update-grub"
echo "  sudo reboot"
"##,
        kernel_version = kernel_version,
        kernel_major = kernel_major,
        sha = sha,
        patches_dir = patches_dir.display(),
    );

    Ok(script)
}

/// Compute SHA for host kernel build from config.
/// Includes: kernel version + patches (*.patch only) + current host kernel config.
fn compute_host_kernel_sha(config: &HostKernelConfig, repo_root: &Path) -> Result<String> {
    let mut content = Vec::new();

    // Include kernel version in SHA
    content.extend(config.kernel_version.as_bytes());

    // NOTE: We intentionally do NOT include the running kernel's config in the SHA.
    // The host kernel uses the running kernel's config as a base, but the SHA should
    // only reflect what WE control (version + patches). This makes builds reproducible
    // across reboots and different base kernels.

    // Read patches from build_inputs (with *.vm.patch filter)
    for pattern in &config.build_inputs {
        let full_pattern = repo_root.join(pattern).to_string_lossy().into_owned();

        let paths: Vec<PathBuf> = match glob(&full_pattern) {
            Ok(entries) => {
                let mut paths: Vec<PathBuf> = entries
                    .filter_map(|e| e.ok())
                    // Skip .vm.patch files (VM-only)
                    .filter(|p| !p.to_string_lossy().ends_with(".vm.patch"))
                    .collect();
                paths.sort();
                paths
            }
            Err(e) => {
                warn!(pattern = %full_pattern, error = %e, "invalid glob pattern");
                continue;
            }
        };

        for path in paths {
            if let Ok(data) = std::fs::read(&path) {
                debug!(path = %path.display(), bytes = data.len(), "hashing host kernel build input");
                content.extend(data);
            }
        }
    }

    if content.is_empty() {
        bail!("No build inputs found for host kernel");
    }

    Ok(compute_sha256_short(&content))
}

/// Build and install host kernel with fcvm patches.
///
/// Uses the running kernel's config as base (includes EC2/AWS modules),
/// applies fcvm patches, and builds deb packages for installation.
///
/// `boot_args` are the kernel boot parameters from the profile config
/// (e.g., "kvm-arm.mode=nested numa=off"). These are added to GRUB_CMDLINE_LINUX_DEFAULT.
pub async fn install_host_kernel(profile: &KernelProfile, boot_args: Option<&str>) -> Result<()> {
    if !nix::unistd::geteuid().is_root() {
        bail!("Installing host kernel requires root privileges. Run with sudo.");
    }

    // Get host kernel config from profile
    let host_config = profile.host_kernel.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "Profile does not have host_kernel configuration.\n\
             Add [kernel_profiles.<name>.host_kernel] section to rootfs-config.toml"
        )
    })?;

    // Find repo root for config file paths
    let repo_root = find_repo_root().ok_or_else(|| {
        anyhow::anyhow!(
            "Cannot find fcvm repository root.\n\n\
             Host kernel builds require the fcvm git repository."
        )
    })?;

    // Compute SHA from build inputs
    let sha = compute_host_kernel_sha(host_config, &repo_root)?;
    let kernel_version = &host_config.kernel_version;
    let localversion = format!("-fcvm-{}", sha);
    let expected_pkg = format!("linux-image-{}{}", kernel_version, localversion);

    info!(sha = %sha, package = %expected_pkg, "computed host kernel SHA");

    // Check if already installed
    let output = Command::new("dpkg")
        .args(["-l", &expected_pkg])
        .output()
        .await
        .context("checking installed packages")?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.contains("ii ") {
            println!("  ✓ Host kernel already installed: {}", expected_pkg);
            println!();

            // Still update GRUB config in case boot_args changed
            let kernel_name = format!("{}{}", kernel_version, localversion);
            update_grub_config(&kernel_name, boot_args).await?;

            println!("  ⚠️  Reboot if not already running this kernel: sudo reboot");
            return Ok(());
        }
    }

    // Generate and run build script
    println!("Building host kernel with fcvm patches...");
    println!("  SHA: {}", sha);
    println!("  This takes 15-30 minutes...");
    println!();

    let script_content = generate_host_kernel_build_script(host_config, &sha, &repo_root)?;

    // Write to temp file
    let script_path = std::env::temp_dir().join(format!("fcvm-host-kernel-build-{}.sh", sha));
    std::fs::write(&script_path, &script_content).context("writing build script")?;
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
        .context("setting script permissions")?;

    info!(script = %script_path.display(), "generated host kernel build script");

    let cmd = Command::new(&script_path);
    let status = run_streaming(cmd, "host_kernel_build")
        .await
        .context("running host kernel build script")?;

    // Clean up script
    let _ = std::fs::remove_file(&script_path);

    if !status.success() {
        bail!(
            "Host kernel build failed with exit code: {:?}",
            status.code()
        );
    }

    // Find the linux-image deb (exclude dbg packages)
    let build_dir = Path::new("/tmp/kernel-build-host");
    let pattern = format!("{}/linux-image-*.deb", build_dir.display());
    let debs: Vec<_> = glob::glob(&pattern)
        .context("globbing for deb files")?
        .filter_map(|r| r.ok())
        .filter(|p| !p.to_string_lossy().contains("-dbg"))
        .collect();

    if debs.is_empty() {
        bail!("No linux-image deb found in {}", build_dir.display());
    }

    let deb_path = &debs[0];
    println!("  → Installing {}", deb_path.display());

    // Install deb package
    let output = Command::new("dpkg")
        .args(["-i"])
        .arg(deb_path)
        .output()
        .await
        .context("running dpkg -i")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("dpkg -i failed: {}", stderr);
    }

    // Extract kernel name from deb filename for GRUB config
    // e.g., linux-image-6.18.3-fcvm-abc123_6.18.3-1_arm64.deb -> 6.18.3-fcvm-abc123
    let deb_name = deb_path.file_name().unwrap().to_string_lossy();
    let kernel_name = deb_name
        .strip_prefix("linux-image-")
        .and_then(|s| s.split('_').next())
        .unwrap_or("unknown");

    // Update GRUB with boot args
    update_grub_config(kernel_name, boot_args).await?;

    println!("  → Running update-grub...");
    let output = Command::new("update-grub")
        .output()
        .await
        .context("running update-grub")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(stderr = %stderr, "update-grub had warnings");
    }

    println!("  ✓ Host kernel installed: {}", expected_pkg);
    println!();
    println!("  ⚠️  Reboot required: sudo reboot");

    Ok(())
}

// ============================================================================
// Profile Firecracker Setup
// ============================================================================

/// Fetch the latest commit hash for a repo/branch via git ls-remote.
async fn fetch_remote_commit_hash(repo: &str, branch: &str) -> Result<String> {
    let url = format!("https://github.com/{}", repo);
    let output = tokio::process::Command::new("git")
        .args(["ls-remote", &url, branch])
        .output()
        .await
        .context("running git ls-remote")?;

    if !output.status.success() {
        bail!(
            "git ls-remote failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let commit = stdout
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no commit hash in ls-remote output"))?;

    Ok(commit[..12].to_string()) // First 12 chars of commit hash
}

/// Compute SHA for profile firecracker binary (repo + branch + commit)
fn compute_profile_firecracker_sha_with_commit(
    profile: &KernelProfile,
    commit_hash: &str,
) -> String {
    let repo = profile.firecracker_repo.as_deref().unwrap_or("");
    let branch = profile.firecracker_branch.as_deref().unwrap_or("main");

    let mut hasher = Sha256::new();
    hasher.update(repo.as_bytes());
    hasher.update(branch.as_bytes());
    hasher.update(commit_hash.as_bytes());
    let result = hasher.finalize();
    hex::encode(&result[..6]) // 12 hex chars
}

/// Get the content-addressed path for profile firecracker binary.
/// Uses assets_dir/firecracker/ alongside kernels and other assets.
/// Fetches latest commit hash to ensure we detect updates.
pub async fn get_profile_firecracker_path(
    profile: &KernelProfile,
    profile_name: &str,
) -> Option<PathBuf> {
    // Only return path if profile has a custom firecracker configured
    let repo = profile.firecracker_repo.as_ref()?;
    let branch = profile.firecracker_branch.as_deref().unwrap_or("main");

    // Fetch latest commit hash to detect updates
    let commit_hash = fetch_remote_commit_hash(repo, branch).await.ok()?;
    let sha = compute_profile_firecracker_sha_with_commit(profile, &commit_hash);
    let filename = format!("firecracker-{}-{}.bin", profile_name, sha);

    Some(paths::assets_dir().join("firecracker").join(filename))
}

/// Ensure the firecracker binary for a kernel profile exists.
///
/// Uses content-addressed naming: firecracker-{profile}-{sha}.bin
/// where SHA is computed from firecracker_repo + firecracker_branch + commit_hash.
/// Automatically detects and rebuilds when new commits are pushed.
pub async fn ensure_profile_firecracker(
    profile: &KernelProfile,
    profile_name: &str,
) -> Result<Option<PathBuf>> {
    // Check if profile needs custom firecracker
    let repo = match &profile.firecracker_repo {
        Some(r) => r,
        None => return Ok(None), // No custom firecracker needed
    };

    let branch = profile.firecracker_branch.as_deref().unwrap_or("main");

    // Fetch latest commit hash to detect updates
    let commit_hash = fetch_remote_commit_hash(repo, branch).await?;
    let sha = compute_profile_firecracker_sha_with_commit(profile, &commit_hash);

    // Content-addressed path in assets dir (alongside kernels)
    let firecracker_dir = paths::assets_dir().join("firecracker");
    let filename = format!("firecracker-{}-{}.bin", profile_name, sha);
    let bin_path = firecracker_dir.join(&filename);

    // Already exists
    if bin_path.exists() {
        info!(
            path = %bin_path.display(),
            profile = %profile_name,
            sha = %sha,
            "firecracker binary exists"
        );
        return Ok(Some(bin_path));
    }

    // Create directory
    tokio::fs::create_dir_all(&firecracker_dir)
        .await
        .context("creating firecracker directory")?;

    // Acquire lock
    let lock_file = firecracker_dir.join(format!("{}.lock", filename));
    use std::os::unix::fs::OpenOptionsExt;
    let lock_fd = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&lock_file)
        .context("opening firecracker lock file")?;

    let flock = Flock::lock(lock_fd, FlockArg::LockExclusive)
        .map_err(|(_, err)| err)
        .context("acquiring exclusive lock for firecracker build")?;

    // Double-check after lock
    if bin_path.exists() {
        debug!(path = %bin_path.display(), "firecracker exists (built by another process)");
        flock.unlock().map_err(|(_, err)| err)?;
        return Ok(Some(bin_path));
    }

    println!(
        "  → Building firecracker from {} (branch: {}, sha: {})...",
        repo, branch, sha
    );
    println!("    This may take 5-10 minutes...");

    // Build in temp directory
    let build_dir = PathBuf::from("/tmp/firecracker-profile-build");

    // Clean up old build
    if build_dir.exists() {
        tokio::fs::remove_dir_all(&build_dir)
            .await
            .context("removing old firecracker build directory")?;
    }

    // Clone repo
    let clone_url = format!("https://github.com/{}", repo);
    let status = Command::new("git")
        .args([
            "clone",
            "--depth=1",
            "-b",
            branch,
            &clone_url,
            build_dir.to_str().unwrap(),
        ])
        .status()
        .await
        .context("cloning firecracker repo")?;

    if !status.success() {
        flock.unlock().map_err(|(_, err)| err)?;
        bail!("Failed to clone firecracker repo from {}", clone_url);
    }

    // Build firecracker
    let status = Command::new("cargo")
        .args(["build", "--release", "-p", "firecracker"])
        .current_dir(&build_dir)
        .status()
        .await
        .context("building firecracker")?;

    if !status.success() {
        flock.unlock().map_err(|(_, err)| err)?;
        bail!("Firecracker build failed");
    }

    // Find the built binary
    let mut binary = build_dir.join("target/release/firecracker");
    if !binary.exists() {
        // Try alternative path (Firecracker's custom build system)
        let alt_binary = build_dir.join("build/cargo_target/release/firecracker");
        if alt_binary.exists() {
            binary = alt_binary;
        } else {
            flock.unlock().map_err(|(_, err)| err)?;
            bail!(
                "Firecracker binary not found at {} or {}",
                binary.display(),
                alt_binary.display()
            );
        }
    }

    // Copy to content-addressed path
    tokio::fs::copy(&binary, &bin_path)
        .await
        .context("installing firecracker binary")?;

    flock.unlock().map_err(|(_, err)| err)?;

    info!(
        path = %bin_path.display(),
        profile = %profile_name,
        sha = %sha,
        "firecracker binary installed"
    );
    println!("  ✓ Firecracker ready: {}", bin_path.display());

    Ok(Some(bin_path))
}

async fn update_grub_config(kernel_name: &str, boot_args: Option<&str>) -> Result<()> {
    let grub_default = Path::new("/etc/default/grub");

    if !grub_default.exists() {
        bail!("/etc/default/grub not found");
    }

    let content = tokio::fs::read_to_string(grub_default)
        .await
        .context("reading /etc/default/grub")?;

    let mut modified = false;
    let mut new_lines = Vec::new();

    for line in content.lines() {
        if line.starts_with("GRUB_CMDLINE_LINUX_DEFAULT=") {
            // Add boot_args from profile if provided
            if let Some(args) = boot_args {
                // Check if any of the args are already present
                let args_to_add: Vec<&str> = args
                    .split_whitespace()
                    .filter(|arg| !line.contains(arg))
                    .collect();

                if !args_to_add.is_empty() {
                    let args_str = args_to_add.join(" ");
                    let new_line = if line.contains("=\"\"") {
                        line.replace("=\"\"", &format!("=\"{}\"", args_str))
                    } else {
                        line.replacen("=\"", &format!("=\"{} ", args_str), 1)
                    };
                    new_lines.push(new_line);
                    modified = true;
                    println!("  → Added boot args: {}", args_str);
                } else {
                    new_lines.push(line.to_string());
                }
            } else {
                new_lines.push(line.to_string());
            }
        } else if line.starts_with("GRUB_DEFAULT=") {
            let new_default = format!(
                "GRUB_DEFAULT=\"Advanced options for Ubuntu>Ubuntu, with Linux {}\"",
                kernel_name
            );
            new_lines.push(new_default);
            modified = true;
            println!("  → Set GRUB_DEFAULT to {}", kernel_name);
        } else {
            new_lines.push(line.to_string());
        }
    }

    if modified {
        let backup = grub_default.with_extension("grub.bak");
        tokio::fs::copy(grub_default, &backup).await?;

        let new_content = new_lines.join("\n") + "\n";
        tokio::fs::write(grub_default, new_content).await?;

        info!(backup = %backup.display(), "GRUB config updated");
    }

    Ok(())
}
