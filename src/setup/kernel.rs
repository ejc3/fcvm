use anyhow::{bail, Context, Result};
use glob::glob;
use nix::fcntl::{Flock, FlockArg};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::paths;
use crate::setup::rootfs::{get_kernel_profile, load_plan, KernelProfile};

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
                let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok()).collect();
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

async fn build_kernel_locally(
    profile: &KernelProfile,
    profile_name: &str,
    dest: &Path,
) -> Result<()> {
    let script = profile.build_script.as_deref().unwrap_or("kernel/build.sh");
    let script_path = Path::new(script);

    if !script_path.exists() {
        bail!(
            "Build script '{}' not found.\n\n\
             Local builds require the fcvm git repository.\n\
             Clone it and run: cargo run -- setup --kernel-profile {} --build-kernels",
            script,
            profile_name
        );
    }

    let mut cmd = Command::new(script_path);
    cmd.env("KERNEL_PATH", dest);

    // Pass config paths to build script via env vars
    if let Some(ref config) = profile.kernel_config {
        cmd.env("KERNEL_CONFIG", config);
    }
    if let Some(ref patches) = profile.patches_dir {
        cmd.env("PATCHES_DIR", patches);
    }

    let output = cmd.output().await.context("running build script")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Kernel build failed: {}", stderr);
    }

    if !dest.exists() {
        bail!("Build completed but kernel not found at {}", dest.display());
    }

    Ok(())
}

// ============================================================================
// Host Kernel Installation (for EC2 setup)
// ============================================================================

/// Install profile kernel as the host kernel and configure GRUB.
///
/// `boot_args` are the kernel boot parameters from the profile config
/// (e.g., "kvm-arm.mode=nested numa=off"). These are added to GRUB_CMDLINE_LINUX_DEFAULT.
pub async fn install_host_kernel(profile_kernel: &Path, boot_args: Option<&str>) -> Result<()> {
    if !nix::unistd::geteuid().is_root() {
        bail!("Installing host kernel requires root privileges. Run with sudo.");
    }

    let filename = profile_kernel
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("vmlinux");

    let parts: Vec<&str> = filename
        .strip_prefix("vmlinux-")
        .unwrap_or(filename)
        .strip_suffix(".bin")
        .unwrap_or(filename)
        .split('-')
        .collect();

    let (profile_name, kernel_version) = if parts.len() >= 2 {
        (parts[0], parts[1])
    } else {
        ("custom", "unknown")
    };

    let kernel_name = format!("vmlinuz-{}-{}", kernel_version, profile_name);
    let boot_path = Path::new("/boot").join(&kernel_name);

    info!(src = %profile_kernel.display(), dest = %boot_path.display(), "installing kernel to /boot");

    tokio::fs::copy(profile_kernel, &boot_path)
        .await
        .context("copying kernel to /boot")?;

    println!("  → Installed kernel to {}", boot_path.display());

    update_grub_config(&kernel_name, boot_args).await?;

    println!("  → Running update-grub...");
    let output = Command::new("update-grub")
        .output()
        .await
        .context("running update-grub")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(stderr = %stderr, "update-grub had warnings");
    }

    println!("  ✓ Host kernel installed");
    println!();
    println!("  ⚠️  Reboot required: sudo reboot");

    Ok(())
}

// ============================================================================
// Profile Firecracker Setup
// ============================================================================

/// Compute SHA for profile firecracker binary (repo + branch)
fn compute_profile_firecracker_sha(profile: &KernelProfile) -> String {
    let repo = profile.firecracker_repo.as_deref().unwrap_or("");
    let branch = profile.firecracker_branch.as_deref().unwrap_or("main");

    let mut hasher = Sha256::new();
    hasher.update(repo.as_bytes());
    hasher.update(branch.as_bytes());
    let result = hasher.finalize();
    hex::encode(&result[..6]) // 12 hex chars
}

/// Get the content-addressed path for profile firecracker binary.
/// Uses assets_dir/firecracker/ alongside kernels and other assets.
pub fn get_profile_firecracker_path(
    profile: &KernelProfile,
    profile_name: &str,
) -> Option<PathBuf> {
    // Only return path if profile has a custom firecracker configured
    profile.firecracker_repo.as_ref()?;

    let sha = compute_profile_firecracker_sha(profile);
    let filename = format!("firecracker-{}-{}.bin", profile_name, sha);

    Some(paths::assets_dir().join("firecracker").join(filename))
}

/// Ensure the firecracker binary for a kernel profile exists.
///
/// Uses content-addressed naming: firecracker-{profile}-{sha}.bin
/// where SHA is computed from firecracker_repo + firecracker_branch.
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
    let sha = compute_profile_firecracker_sha(profile);

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
