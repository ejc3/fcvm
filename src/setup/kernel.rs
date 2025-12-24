use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tokio::process::Command;
use tracing::info;

use crate::paths;
use crate::setup::rootfs::{load_plan, KernelArchConfig};

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

/// Ensure kernel exists, downloading from Kata release if needed
pub async fn ensure_kernel() -> Result<PathBuf> {
    let (plan, _, _) = load_plan()?;
    let kernel_config = plan.kernel.current_arch()?;

    download_kernel(kernel_config).await
}

/// Download kernel from Kata release tarball
async fn download_kernel(config: &KernelArchConfig) -> Result<PathBuf> {
    let kernel_dir = paths::kernel_dir();

    // Cache by URL hash - changing URL triggers re-download
    let url_hash = compute_sha256_short(config.url.as_bytes());
    let kernel_path = kernel_dir.join(format!("vmlinux-{}.bin", url_hash));

    if kernel_path.exists() {
        info!(path = %kernel_path.display(), url_hash = %url_hash, "kernel already exists");
        return Ok(kernel_path);
    }

    println!("⚙️  Downloading kernel (first run)...");
    info!(url = %config.url, path_in_archive = %config.path, "downloading kernel from Kata release");

    // Create directory
    tokio::fs::create_dir_all(&kernel_dir)
        .await
        .context("creating kernel directory")?;

    // Download and extract in one pipeline:
    // curl -> zstd -d -> tar --extract
    let cache_dir = paths::base_dir().join("cache");
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
        .args([
            "--use-compress-program=zstd",
            "-xf",
        ])
        .arg(&tarball_path)
        .arg("-C")
        .arg(&cache_dir)
        .arg(&extract_path)
        .output()
        .await
        .context("extracting kernel from tarball")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to extract kernel: {}", stderr);
    }

    // Move extracted kernel to final location
    let extracted_path = cache_dir.join(&config.path);
    if !extracted_path.exists() {
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

    Ok(kernel_path)
}
