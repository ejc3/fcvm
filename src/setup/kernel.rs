use anyhow::{bail, Context, Result};
use nix::fcntl::{Flock, FlockArg};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::paths;
use crate::setup::rootfs::{load_plan, KernelArchConfig};

/// GitHub repository for kernel releases
const GITHUB_REPO: &str = "ejc3/fcvm";

/// Inception kernel version (must match kernel/build.sh)
const INCEPTION_KERNEL_VERSION: &str = "6.18";

/// Compute SHA256 of bytes, return hex string (first 12 chars)
fn compute_sha256_short(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    hex::encode(&result[..6]) // 12 hex chars
}

/// Get the kernel URL hash for the current architecture
/// This is used to include in Layer 2 SHA calculation
pub fn get_kernel_url_hash() -> Result<String> {
    let (plan, _, _) = load_plan()?;
    let kernel_config = plan.kernel.current_arch()?;
    Ok(compute_sha256_short(kernel_config.url.as_bytes()))
}

/// Ensure kernel exists, downloading from Kata release if needed.
/// If `allow_create` is false, bail if kernel doesn't exist.
///
/// If config specifies `local_path`, uses that directly (no download).
pub async fn ensure_kernel(allow_create: bool) -> Result<PathBuf> {
    let (plan, _, _) = load_plan()?;
    let kernel_config = plan.kernel.current_arch()?;

    // Check for local path first
    if let Some(local_path) = &kernel_config.local_path {
        let path = PathBuf::from(local_path);
        if !path.exists() {
            bail!(
                "Kernel local_path not found: {}\n\
                Build it with: ./kernel/build.sh",
                path.display()
            );
        }
        info!(path = %path.display(), "using local kernel");
        return Ok(path);
    }

    // URL-based download
    if kernel_config.url.is_empty() {
        bail!("Kernel config must specify either 'url' or 'local_path'");
    }

    download_kernel(kernel_config, allow_create).await
}

/// Download kernel from Kata release tarball.
///
/// Uses file locking to prevent race conditions when multiple VMs start
/// simultaneously and all try to download the same kernel.
///
/// If `allow_create` is false, bail if kernel doesn't exist.
async fn download_kernel(config: &KernelArchConfig, allow_create: bool) -> Result<PathBuf> {
    let kernel_dir = paths::kernel_dir();

    // Cache by URL hash - changing URL triggers re-download
    let url_hash = compute_sha256_short(config.url.as_bytes());
    let kernel_path = kernel_dir.join(format!("vmlinux-{}.bin", url_hash));

    // Fast path: kernel already exists
    if kernel_path.exists() {
        info!(path = %kernel_path.display(), url_hash = %url_hash, "kernel already exists");
        return Ok(kernel_path);
    }

    // Bail if creation not allowed
    if !allow_create {
        bail!("Kernel not found. Run 'fcvm setup' first, or use --setup flag.");
    }

    // Create directory (needed for lock file)
    tokio::fs::create_dir_all(&kernel_dir)
        .await
        .context("creating kernel directory")?;

    // Acquire exclusive lock to prevent multiple downloads
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

    // Double-check after acquiring lock - another process may have downloaded it
    if kernel_path.exists() {
        debug!(
            path = %kernel_path.display(),
            url_hash = %url_hash,
            "kernel already exists (created by another process)"
        );
        flock
            .unlock()
            .map_err(|(_, err)| err)
            .context("releasing kernel lock")?;
        return Ok(kernel_path);
    }

    println!("⚙️  Downloading kernel (first run)...");
    info!(url = %config.url, path_in_archive = %config.path, "downloading kernel from Kata release");

    // Download and extract in one pipeline:
    // curl -> zstd -d -> tar --extract
    let cache_dir = paths::cache_dir();
    tokio::fs::create_dir_all(&cache_dir).await?;

    let tarball_path = cache_dir.join(format!("kata-kernel-{}.tar.zst", url_hash));

    // Download if not cached
    if !tarball_path.exists() {
        println!("  → Downloading Kata release tarball...");

        let output = Command::new("curl")
            .args(["-fSL", &config.url, "-o"])
            .arg(&tarball_path)
            .output()
            .await
            .context("running curl")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = flock.unlock();
            bail!("Failed to download kernel: {}", stderr);
        }

        info!(path = %tarball_path.display(), "downloaded Kata tarball");
    } else {
        info!(path = %tarball_path.display(), "using cached Kata tarball");
    }

    // Extract just the kernel file using tar with zstd
    println!("  → Extracting kernel from tarball...");

    // Use tar to extract, piping through zstd
    // tar expects path with ./ prefix based on how Kata packages it
    let extract_path = format!("./{}", config.path);

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

    // Move extracted kernel to final location
    let extracted_path = cache_dir.join(&config.path);
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

    // Clean up extracted files (keep tarball for cache)
    let opt_dir = cache_dir.join("opt");
    if opt_dir.exists() {
        tokio::fs::remove_dir_all(&opt_dir).await.ok();
    }

    println!("  ✓ Kernel ready");
    info!(
        path = %kernel_path.display(),
        url_hash = %url_hash,
        "kernel downloaded and cached"
    );

    // Release lock
    flock
        .unlock()
        .map_err(|(_, err)| err)
        .context("releasing kernel lock after download")?;

    Ok(kernel_path)
}

// ============================================================================
// Inception Kernel (for nested virtualization)
// ============================================================================

/// Compute SHA for inception kernel based on build inputs.
/// This matches the SHA computed in kernel/build.sh and CI workflow.
pub fn compute_inception_kernel_sha() -> Result<String> {
    let kernel_dir = Path::new("kernel");
    let mut content = Vec::new();

    // Read build.sh
    let script = kernel_dir.join("build.sh");
    if script.exists() {
        content.extend(std::fs::read(&script).context("reading kernel/build.sh")?);
    } else {
        bail!("kernel/build.sh not found");
    }

    // Read inception.conf
    let conf = kernel_dir.join("inception.conf");
    if conf.exists() {
        content.extend(std::fs::read(&conf).context("reading kernel/inception.conf")?);
    }

    // Read patches/*.patch (sorted for determinism)
    let patches_dir = kernel_dir.join("patches");
    if patches_dir.exists() {
        let mut patches: Vec<_> = std::fs::read_dir(&patches_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "patch"))
            .collect();
        patches.sort_by_key(|e| e.path());
        for patch in patches {
            content.extend(std::fs::read(patch.path())?);
        }
    }

    Ok(compute_sha256_short(&content))
}

/// Get the inception kernel filename.
pub fn inception_kernel_filename(sha: &str) -> String {
    format!("vmlinux-inception-{}-{}.bin", INCEPTION_KERNEL_VERSION, sha)
}

/// Get the inception kernel release tag.
pub fn inception_kernel_tag(sha: &str) -> String {
    format!("kernel-inception-{}-{}", INCEPTION_KERNEL_VERSION, sha)
}

/// Ensure inception kernel exists.
///
/// 1. Check if already downloaded locally
/// 2. Try to download from GitHub releases
/// 3. If `allow_build` is true and download fails, build locally
///
/// Returns the path to the kernel binary.
pub async fn ensure_inception_kernel(allow_build: bool) -> Result<PathBuf> {
    let sha = compute_inception_kernel_sha()?;
    let filename = inception_kernel_filename(&sha);
    let kernel_dir = paths::kernel_dir();
    let kernel_path = kernel_dir.join(&filename);

    // Fast path: already exists
    if kernel_path.exists() {
        info!(
            path = %kernel_path.display(),
            sha = %sha,
            "inception kernel already exists"
        );
        return Ok(kernel_path);
    }

    // Create directory
    tokio::fs::create_dir_all(&kernel_dir)
        .await
        .context("creating kernel directory")?;

    // Acquire lock to prevent race conditions
    let lock_file = kernel_dir.join(format!("{}.lock", filename));
    use std::os::unix::fs::OpenOptionsExt;
    let lock_fd = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&lock_file)
        .context("opening inception kernel lock file")?;

    let flock = Flock::lock(lock_fd, FlockArg::LockExclusive)
        .map_err(|(_, err)| err)
        .context("acquiring exclusive lock for inception kernel")?;

    // Double-check after lock
    if kernel_path.exists() {
        debug!(
            path = %kernel_path.display(),
            "inception kernel already exists (created by another process)"
        );
        flock.unlock().map_err(|(_, err)| err)?;
        return Ok(kernel_path);
    }

    // Try to download from GitHub releases
    let tag = inception_kernel_tag(&sha);
    let download_url = format!(
        "https://github.com/{}/releases/download/{}/{}",
        GITHUB_REPO, tag, filename
    );

    println!("⚙️  Downloading inception kernel...");
    info!(url = %download_url, tag = %tag, "downloading inception kernel from GitHub releases");

    let download_result = download_inception_kernel(&download_url, &kernel_path).await;

    match download_result {
        Ok(_) => {
            println!("  ✓ Inception kernel downloaded");
            info!(path = %kernel_path.display(), "inception kernel ready");
            flock.unlock().map_err(|(_, err)| err)?;
            Ok(kernel_path)
        }
        Err(e) => {
            warn!(error = %e, "failed to download inception kernel");

            if allow_build {
                println!("  → Download failed, building locally (this may take 10-20 minutes)...");
                build_inception_kernel_locally(&kernel_path).await?;
                println!("  ✓ Inception kernel built");
                flock.unlock().map_err(|(_, err)| err)?;
                Ok(kernel_path)
            } else {
                flock.unlock().map_err(|(_, err)| err)?;
                bail!(
                    "Failed to download inception kernel: {}\n\
                    \n\
                    The kernel release may not exist yet. Options:\n\
                    1. Wait for CI to build it (push to main triggers kernel build)\n\
                    2. Build locally with: fcvm setup --build-kernels\n\
                    3. Build manually with: ./kernel/build.sh",
                    e
                );
            }
        }
    }
}

/// Download inception kernel from URL.
async fn download_inception_kernel(url: &str, dest: &Path) -> Result<()> {
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

    // Verify it's a valid ELF binary
    let output = Command::new("file")
        .arg(&temp_path)
        .output()
        .await
        .context("running file command")?;

    let file_type = String::from_utf8_lossy(&output.stdout);
    if !file_type.contains("ELF") {
        let _ = tokio::fs::remove_file(&temp_path).await;
        bail!(
            "Downloaded file is not a valid kernel (not ELF): {}",
            file_type
        );
    }

    // Move to final location
    tokio::fs::rename(&temp_path, dest)
        .await
        .context("moving downloaded kernel to final location")?;

    Ok(())
}

/// Build inception kernel locally using kernel/build.sh.
async fn build_inception_kernel_locally(dest: &Path) -> Result<()> {
    let script = Path::new("kernel/build.sh");
    if !script.exists() {
        bail!("kernel/build.sh not found - are you in the fcvm repository root?");
    }

    let output = Command::new(script)
        .env("KERNEL_PATH", dest)
        .output()
        .await
        .context("running kernel/build.sh")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Kernel build failed: {}", stderr);
    }

    if !dest.exists() {
        bail!(
            "Kernel build completed but file not found at {}",
            dest.display()
        );
    }

    Ok(())
}
