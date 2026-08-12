use anyhow::{bail, Context, Result};
use glob::glob;
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::paths;
use crate::setup::rootfs::{get_kernel_profile, KernelProfile};
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
/// Every kernel is a profile. The shipped default, nested, and btrfs profiles
/// are source-built and normally downloaded as content-addressed releases;
/// user profiles may also point at an archive URL.
///
/// If `allow_create` is false, bails if kernel doesn't exist.
/// If `allow_build` is true, falls back to local build for custom profiles.
pub async fn ensure_kernel(
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

    if profile.inherits_kernel() {
        // Runtime-only profile — uses default kernel
        if profile_name == "default" {
            bail!(
                "'default' kernel profile must define kernel_url or \
                 kernel_version/kernel_repo — cannot inherit from itself"
            );
        }
        debug!(profile = %profile_name, "profile inherits kernel from default");
        return Box::pin(ensure_kernel("default", allow_create, allow_build)).await;
    }

    if profile.is_url_based() {
        ensure_url_kernel(&profile, allow_create).await
    } else {
        ensure_custom_kernel(&profile, profile_name, allow_create, allow_build).await
    }
}

/// Rebuild a source-built profile's kernel from source, bypassing the release
/// download entirely, and return its content-addressed path.
///
/// [`ensure_kernel`] prefers a published release and only builds after a FAILED
/// download, so a release-refresh job cannot force a rebuild by deleting the
/// cached file: the release it is about to replace is still published, the
/// download succeeds, and the job republishes the exact artifact the operator
/// asked to replace. A post-run "the file exists" assertion cannot catch that,
/// because a download produces the same file. This is the path for "the
/// published content is wrong, build it again from source".
pub async fn rebuild_kernel_from_source(profile_name: &str) -> Result<PathBuf> {
    let profile = get_kernel_profile(profile_name)?.ok_or_else(|| {
        anyhow::anyhow!(
            "kernel profile '{}' not found in config. \
             Add [kernel_profiles.{}] section to rootfs-config.toml",
            profile_name,
            profile_name
        )
    })?;
    anyhow::ensure!(
        !profile.inherits_kernel(),
        "kernel profile '{}' inherits its kernel; rebuild the profile it inherits from instead",
        profile_name
    );
    anyhow::ensure!(
        !profile.is_url_based(),
        "kernel profile '{}' is URL-based; there is no source to rebuild from",
        profile_name
    );

    let sha = compute_profile_kernel_sha(&profile)?;
    let filename = custom_kernel_filename(profile_name, &profile.kernel_version, &sha);
    let kernel_dir = paths::kernel_dir();
    let kernel_path = kernel_dir.join(&filename);

    let flock = super::lock_store_dir(
        &kernel_dir.join(format!("{}.lock", filename)),
        "kernel rebuild",
    )
    .await?;

    // Build to a unique sibling and rename into place, so a build that fails
    // partway leaves the existing artifact untouched instead of destroying a
    // usable cached kernel (measured: an early version deleted first, and a
    // build that died on a DNS failure took the cache with it). Unique per
    // builder via uuid, not pid — separate PID namespaces reuse numbers — and
    // the rename is atomic and happens under the same lock.
    let staging_path = kernel_dir.join(format!("{}.rebuild-{}", filename, uuid::Uuid::new_v4()));
    println!("⚙️  Rebuilding kernel from source (profile: {profile_name})...");
    info!(profile = %profile_name, path = %kernel_path.display(), staging = %staging_path.display(), "forced source rebuild, skipping release download");
    let publish_result = match build_kernel_locally(&profile, profile_name, &staging_path).await {
        Ok(()) => super::publish_store_entry(&staging_path, &kernel_path, "rebuilt kernel").await,
        Err(error) => {
            let _ = tokio::fs::remove_file(&staging_path).await;
            Err(error)
        }
    };
    flock.unlock().map_err(|(_, err)| err)?;
    publish_result?;
    println!("  ✓ Kernel rebuilt from source (profile: {profile_name})");
    Ok(kernel_path)
}

/// Get kernel path (without downloading/building).
///
/// Returns the path where the kernel should exist.
/// Used to check existence before running VM.
pub fn get_kernel_path(profile_name: &str) -> Result<PathBuf> {
    let profile = get_kernel_profile(profile_name)?
        .ok_or_else(|| anyhow::anyhow!("kernel profile '{}' not found in config", profile_name))?;

    if profile.inherits_kernel() {
        if profile_name == "default" {
            bail!(
                "'default' kernel profile must define kernel_url or \
                 kernel_version/kernel_repo — cannot inherit from itself"
            );
        }
        return get_kernel_path("default");
    }

    if profile.is_url_based() {
        get_url_kernel_path(&profile)
    } else {
        get_custom_kernel_path(&profile, profile_name)
    }
}

/// Get the kernel identity hash for a profile.
/// For URL-based profiles, this is the URL hash.
/// Used by callers that need the selected kernel's content identity.
pub fn get_kernel_url_hash(profile_name: &str) -> Result<String> {
    let profile = get_kernel_profile(profile_name)?
        .ok_or_else(|| anyhow::anyhow!("kernel profile '{}' not found in config", profile_name))?;

    if profile.inherits_kernel() {
        if profile_name == "default" {
            bail!(
                "'default' kernel profile must define kernel_url or \
                 kernel_version/kernel_repo — cannot inherit from itself"
            );
        }
        return get_kernel_url_hash("default");
    }

    if let Some(ref url) = profile.kernel_url {
        Ok(compute_sha256_short(url.as_bytes()))
    } else {
        compute_profile_kernel_sha(&profile)
    }
}

// ============================================================================
// URL-Based Kernel (profiles backed by an archive URL)
// ============================================================================

fn get_url_kernel_path(profile: &KernelProfile) -> Result<PathBuf> {
    let url = profile
        .kernel_url
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("kernel profile missing kernel_url"))?;
    let url_hash = compute_sha256_short(url.as_bytes());
    Ok(paths::kernel_dir().join(format!("vmlinux-{}.bin", url_hash)))
}

async fn ensure_url_kernel(profile: &KernelProfile, allow_create: bool) -> Result<PathBuf> {
    // Check for local path first
    if let Some(ref local_path) = profile.kernel_local_path {
        let path = PathBuf::from(local_path);
        if !path.exists() {
            bail!("Kernel local_path not found: {}", path.display());
        }
        info!(path = %path.display(), "using local kernel");
        return Ok(path);
    }

    let url = profile.kernel_url.as_ref().ok_or_else(|| {
        anyhow::anyhow!("kernel profile must specify kernel_url or kernel_local_path")
    })?;
    let archive_path = profile
        .kernel_archive_path
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("kernel profile must specify kernel_archive_path"))?;

    if url.is_empty() {
        bail!("Kernel config must specify 'kernel_url' or 'kernel_local_path'");
    }

    let kernel_dir = paths::kernel_dir();
    let url_hash = compute_sha256_short(url.as_bytes());
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
    let flock = super::lock_store_dir(
        &kernel_dir.join(format!("vmlinux-{}.lock", url_hash)),
        "kernel download",
    )
    .await?;

    // Double-check after lock
    if kernel_path.exists() {
        debug!(path = %kernel_path.display(), "kernel exists (created by another process)");
        flock.unlock().map_err(|(_, err)| err)?;
        return Ok(kernel_path);
    }

    // Download
    println!("⚙️  Downloading kernel...");
    info!(url = %url, path_in_archive = %archive_path, "downloading kernel");

    let cache_dir = paths::cache_dir();
    tokio::fs::create_dir_all(&cache_dir).await?;
    super::give_store_entry_to_invoker(&cache_dir);

    let tarball_path = cache_dir.join(format!("kernel-{}.tar.zst", url_hash));
    let tarball_temp = cache_dir.join(format!("kernel-{}.tar.zst.downloading", url_hash));

    // Download tarball if not cached (atomic: download to temp, then rename)
    if !tarball_path.exists() {
        println!("  → Downloading tarball...");
        // Clean up any leftover temp file from previous failed attempt
        let _ = tokio::fs::remove_file(&tarball_temp).await;

        let output = Command::new("curl")
            .args(["-fSL", url, "-o"])
            .arg(&tarball_temp)
            .output()
            .await
            .context("running curl")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = tokio::fs::remove_file(&tarball_temp).await;
            let _ = flock.unlock();
            bail!("Failed to download kernel: {}", stderr);
        }

        // Atomic rename on success
        super::publish_store_entry(&tarball_temp, &tarball_path, "downloaded kernel tarball")
            .await?;
    } else {
        info!(path = %tarball_path.display(), "using cached tarball");
    }

    // Extract kernel from tarball (atomic: extract to temp, then move)
    println!("  → Extracting kernel...");
    let extract_temp = cache_dir.join(format!("kernel-{}-extract", url_hash));
    let _ = tokio::fs::remove_dir_all(&extract_temp).await;
    tokio::fs::create_dir_all(&extract_temp).await?;
    // Deterministic name: a killed root run would otherwise leave a directory
    // whose contents the next rootless run's remove_dir_all cannot unlink.
    super::give_store_entry_to_invoker(&extract_temp);

    let extract_path = format!("./{}", archive_path);
    let output = Command::new("tar")
        .args(["--use-compress-program=zstd", "-xf"])
        .arg(&tarball_path)
        .arg("-C")
        .arg(&extract_temp)
        .arg(&extract_path)
        .output()
        .await
        .context("extracting kernel from tarball")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Delete corrupted tarball and temp dir so next run re-downloads
        let _ = tokio::fs::remove_file(&tarball_path).await;
        let _ = tokio::fs::remove_dir_all(&extract_temp).await;
        let _ = flock.unlock();
        bail!("Failed to extract kernel: {}", stderr);
    }

    // Move to final location
    let extracted_path = extract_temp.join(archive_path);
    if !extracted_path.exists() {
        let _ = tokio::fs::remove_dir_all(&extract_temp).await;
        let _ = flock.unlock();
        bail!(
            "Kernel not found after extraction at {}",
            extracted_path.display()
        );
    }

    // Copy to a temp name in the same directory, then atomically rename onto the
    // final content-addressed path. An interrupted or failed copy (kill, ENOSPC)
    // must never leave a partial file that later runs treat as a valid kernel.
    let kernel_temp = kernel_path.with_extension("downloading");
    let _ = tokio::fs::remove_file(&kernel_temp).await;
    if let Err(e) = tokio::fs::copy(&extracted_path, &kernel_temp).await {
        let _ = tokio::fs::remove_file(&kernel_temp).await;
        let _ = tokio::fs::remove_dir_all(&extract_temp).await;
        let _ = flock.unlock();
        return Err(e).context("copying kernel to staging location");
    }
    if let Err(e) =
        super::publish_store_entry(&kernel_temp, &kernel_path, "downloaded kernel").await
    {
        let _ = tokio::fs::remove_file(&kernel_temp).await;
        let _ = tokio::fs::remove_dir_all(&extract_temp).await;
        let _ = flock.unlock();
        return Err(e);
    }

    // Clean up temp extraction dir
    let _ = tokio::fs::remove_dir_all(&extract_temp).await;

    println!("  ✓ Kernel ready");
    info!(path = %kernel_path.display(), url_hash = %url_hash, "kernel ready");

    flock.unlock().map_err(|(_, err)| err)?;
    Ok(kernel_path)
}

// ============================================================================
// Source-Built Kernel (default and named profiles such as nested or btrfs)
// ============================================================================

fn get_custom_kernel_path(profile: &KernelProfile, profile_name: &str) -> Result<PathBuf> {
    let sha = compute_profile_kernel_sha(profile)?;
    let filename = custom_kernel_filename(profile_name, &profile.kernel_version, &sha);
    Ok(paths::kernel_dir().join(filename))
}

async fn ensure_custom_kernel(
    profile: &KernelProfile,
    profile_name: &str,
    allow_create: bool,
    allow_build: bool,
) -> Result<PathBuf> {
    let sha = compute_profile_kernel_sha(profile)?;
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
    let flock =
        super::lock_store_dir(&kernel_dir.join(format!("{}.lock", filename)), "kernel").await?;

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
                build_kernel_locally(profile, profile_name, &kernel_path).await?;
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

    // Try the repo path baked in at build time. This keeps locally-built
    // binaries working when invoked from another directory while target/ is a
    // symlink outside the repo (then /proc/self/exe resolves outside the repo
    // and the executable-path fallback below cannot find Cargo.toml).
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if manifest_dir.join("Cargo.toml").exists() && manifest_dir.join("rootfs-config.toml").exists()
    {
        return Some(manifest_dir);
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
///
/// Errors when configured build inputs cannot be resolved (no matching files, or a
/// matched file cannot be read). Silently degrading the cache key would make the
/// content-addressed kernel name stop reflecting the configured patches/config.
pub fn compute_profile_kernel_sha(profile: &KernelProfile) -> Result<String> {
    compute_profile_kernel_sha_at_root(profile, find_repo_root().as_deref())
}

/// Resolve a profile's content-addressed artifact SHA.
///
/// In a source checkout the manifest value is never trusted blindly: the
/// configured inputs are hashed and must match. A packaged binary has no
/// `kernel/` tree, so a published profile's validated manifest SHA is the
/// authoritative release identifier there.
fn compute_profile_kernel_sha_at_root(
    profile: &KernelProfile,
    repo_root: Option<&Path>,
) -> Result<String> {
    let manifest_sha = profile
        .kernel_sha
        .as_deref()
        .map(validate_manifest_kernel_sha)
        .transpose()?;

    if repo_root.is_none() {
        if let Some(sha) = manifest_sha {
            return Ok(sha.to_string());
        }
    }

    let computed = compute_profile_kernel_sha_from_inputs(profile, repo_root)?;
    if let Some(expected) = manifest_sha {
        if computed != expected {
            bail!(
                "kernel_sha '{}' does not match build_inputs hash '{}'; update the manifest and publish the new artifact",
                expected,
                computed
            );
        }
    }

    Ok(computed)
}

fn validate_manifest_kernel_sha(sha: &str) -> Result<&str> {
    if sha.len() != 12
        || !sha
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("kernel_sha must be exactly 12 lowercase hexadecimal characters, got '{sha}'");
    }
    Ok(sha)
}

fn compute_profile_kernel_sha_from_inputs(
    profile: &KernelProfile,
    repo_root: Option<&Path>,
) -> Result<String> {
    if profile.build_inputs.is_empty() {
        warn!("kernel profile has no build_inputs, using empty SHA");
        return Ok("000000000000".to_string());
    }

    // Resolve relative inputs from the supplied source checkout.
    if let Some(root) = repo_root {
        debug!(repo_root = %root.display(), "found repo root for build_inputs");
    } else {
        debug!("repo root not found, using CWD for build_inputs");
    }

    let mut content = Vec::new();

    for pattern in &profile.build_inputs {
        // If pattern is relative and we have a repo root, prepend it
        let full_pattern = if !pattern.starts_with('/') {
            if let Some(root) = repo_root {
                root.join(pattern).to_string_lossy().into_owned()
            } else {
                pattern.clone()
            }
        } else {
            pattern.clone()
        };

        // Expand glob pattern
        let paths: Vec<PathBuf> = {
            let entries = glob(&full_pattern)
                .with_context(|| format!("invalid build_inputs glob pattern: {}", full_pattern))?;
            let mut all_matches: Vec<PathBuf> = entries.filter_map(|e| e.ok()).collect();
            // Every configured pattern must match at least one file on disk; otherwise a
            // misspelled or moved pattern would be silently dropped from the cache key
            // and the kernel name would stop reflecting the configured input set.
            // (.disabled files count as matches so patches can be disabled without
            // breaking the pattern check.)
            if all_matches.is_empty() {
                bail!(
                    "kernel build_inputs pattern '{}' matched no files (resolved to '{}'). \
                     Fix the pattern in rootfs-config.toml or run from the fcvm repository",
                    pattern,
                    full_pattern
                );
            }
            // Filter out .disabled files (allows disabling patches without changing SHA)
            all_matches.retain(|p| !p.to_string_lossy().ends_with(".disabled"));
            all_matches.sort(); // Deterministic order
            all_matches
        };

        if paths.is_empty() {
            debug!(pattern = %full_pattern, "all files matched by pattern are .disabled");
        }

        for path in paths {
            let data = std::fs::read(&path)
                .with_context(|| format!("reading kernel build input {}", path.display()))?;
            debug!(path = %path.display(), bytes = data.len(), "hashing build input");
            content.extend(data);
        }
    }

    if content.is_empty() {
        bail!(
            "no kernel build input files matched build_inputs {:?} (repo root: {}). \
             Cannot compute the kernel cache key without them - run from the fcvm \
             repository or fix build_inputs in rootfs-config.toml",
            profile.build_inputs,
            repo_root
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "not found".to_string()),
        );
    }

    Ok(compute_sha256_short(&content))
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
    let max_retries = 3;

    for attempt in 1..=max_retries {
        let output = Command::new("curl")
            .args(["-fSL", url, "-o"])
            .arg(&temp_path)
            .output()
            .await
            .context("running curl")?;

        if output.status.success() {
            break;
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = tokio::fs::remove_file(&temp_path).await;

        if attempt < max_retries {
            warn!(
                attempt,
                max_retries,
                error = %stderr.trim(),
                "kernel download failed, retrying in 5s"
            );
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        } else {
            bail!("curl failed after {} attempts: {}", max_retries, stderr);
        }
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

    super::publish_store_entry(&temp_path, dest, "release kernel").await?;

    Ok(())
}

/// Shared shell fragment for publishing a complete kernel-source archive.
///
/// VM and host builds deliberately use the same protocol so a cancelled setup
/// cannot leave either build path permanently pinned to a partial tarball.
fn kernel_source_cache_shell() -> &'static str {
    r#"# A valid final-name tarball is a commit marker. Never remove or write it
# while downloading: stage into a unique sibling, validate the entire archive,
# then replace the marker atomically. A setup process killed during curl leaves
# the prior marker untouched, and the next run validates and retries it.
validate_kernel_tarball() {
    tar -tJf "$1" >/dev/null 2>&1
}

if ! validate_kernel_tarball "$KERNEL_TARBALL"; then
    if [[ -f "$KERNEL_TARBALL" ]]; then
        echo "Cached kernel source archive is incomplete; replacing it..."
    fi
    echo "Downloading kernel source..."
    KERNEL_TARBALL_TEMP=$(mktemp "${KERNEL_TARBALL}.downloading.XXXXXX")
    cleanup_kernel_tarball_temp() {
        if [[ -n "${KERNEL_TARBALL_TEMP:-}" ]]; then
            rm -f -- "$KERNEL_TARBALL_TEMP"
        fi
    }
    trap cleanup_kernel_tarball_temp EXIT

    if ! curl -fSL "$KERNEL_URL" -o "$KERNEL_TARBALL_TEMP"; then
        echo "ERROR: Failed to download kernel source" >&2
        exit 1
    fi
    if ! validate_kernel_tarball "$KERNEL_TARBALL_TEMP"; then
        echo "ERROR: Downloaded kernel source is not a complete tar.xz archive" >&2
        exit 1
    fi

    mv -f -- "$KERNEL_TARBALL_TEMP" "$KERNEL_TARBALL"
    KERNEL_TARBALL_TEMP=""
fi"#
}

/// Generate VM kernel build script dynamically from profile config.
///
/// The script is written to a temp file and executed. This allows us to:
/// - Factor common logic between VM and host kernel builds
/// - Drive all config (version, URLs, paths) from TOML
/// - Not maintain separate shell scripts in source control
fn generate_vm_kernel_build_script(
    profile: &KernelProfile,
    profile_name: &str,
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

    // Get config from profile
    // Empty string means no patches, None means use default
    let patches_dir = match profile.patches_dir.as_deref() {
        Some("") => None, // Explicitly disabled
        Some(p) => Some(repo_root.join(p)),
        None => Some(repo_root.join("kernel/patches")), // Default
    };

    let kernel_config = profile.kernel_config.as_deref().map(|p| repo_root.join(p));

    let base_config_url = profile
        .base_config_url
        .as_deref()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "kernel profile must specify base_config_url (e.g., firecracker microvm-kernel-ci config)"
            )
        })?;

    let script = format!(
        r##"#!/bin/bash
# Generated VM kernel build script
# DO NOT EDIT - generated by fcvm from rootfs-config.toml
set -euo pipefail

KERNEL_VERSION="{kernel_version}"
KERNEL_MAJOR="{kernel_major}"
# Per-asset build dir: concurrent builds of different profiles/SHAs hold different
# locks, so they must not share a build tree (one would rm -rf the other's source).
BUILD_DIR="${{BUILD_DIR:-/tmp/kernel-build-{profile_name}-{sha}}}"
NPROC="${{NPROC:-$(nproc)}}"
SOURCE_DIR="$BUILD_DIR/linux-${{KERNEL_VERSION}}"
SHA_MARKER="$SOURCE_DIR/.fcvm-patches-sha"
BUILD_SHA="{sha}"
KERNEL_PATH="{kernel_path}"
{patches_dir_line}
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

{kernel_source_cache}

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

{apply_patches_block}

# Download Firecracker base config
echo "Downloading Firecracker base config..."
curl -fSL "$BASE_CONFIG_URL" -o .config

# Apply options from config fragment
{apply_config_fragment}

# Update config with defaults for new options
make ARCH="$KERNEL_ARCH" olddefconfig

# Every requested built-in option is part of the artifact contract. Kconfig can
# silently turn an option off when a dependency is missing; catch that before a
# kernel which cannot implement its profile is published.
if [[ -n "${{KERNEL_CONFIG:-}}" ]] && [[ -f "$KERNEL_CONFIG" ]]; then
    while IFS= read -r line; do
        if [[ "$line" =~ ^(CONFIG_[A-Z0-9_]+)=y ]]; then
            opt="${{BASH_REMATCH[1]}}"
            if ! grep -qx "${{opt}}=y" .config; then
                # To stderr: the runner only surfaces the script's stderr, so an
                # error on stdout leaves CI failing with no visible reason.
                echo "ERROR: requested kernel option ${{opt}}=y was disabled by Kconfig" >&2
                exit 1
            fi
        fi
    done < "$KERNEL_CONFIG"
fi

# Show enabled options
echo ""
echo "Verifying configuration:"
if ! grep -E "^CONFIG_(FUSE_FS|KVM|VIRTUALIZATION|BTRFS_FS|TUN|VETH|INET_DIAG|INET_DIAG_DESTROY|PACKET)=" .config; then
    echo "ERROR: built kernel exposes none of the profile summary options" >&2
    exit 1
fi
echo ""

# Build kernel
echo "Building kernel with $NPROC parallel jobs..."
make ARCH="$KERNEL_ARCH" -j"$NPROC" "$KERNEL_IMAGE"

# Copy output (Firecracker needs uncompressed ELF vmlinux, not bzImage)
# Copy to a temp name then atomically rename so an interrupted copy never
# leaves a partial kernel at the content-addressed path.
echo "Copying kernel to $KERNEL_PATH..."
case "$KERNEL_ARCH" in
    arm64)  cp "arch/arm64/boot/Image" "$KERNEL_PATH.tmp" ;;
    x86_64) cp "vmlinux" "$KERNEL_PATH.tmp" ;;
esac
mv -f "$KERNEL_PATH.tmp" "$KERNEL_PATH"

echo ""
echo "=== Build Complete ==="
echo "Kernel: $KERNEL_PATH"
echo "Size: $(du -h "$KERNEL_PATH" | cut -f1)"
"##,
        kernel_version = kernel_version,
        kernel_major = kernel_major,
        profile_name = profile_name,
        sha = sha,
        kernel_path = dest.display(),
        patches_dir_line = patches_dir
            .as_ref()
            .map(|p| format!("PATCHES_DIR=\"{}\"", p.display()))
            .unwrap_or_else(|| "# No patches directory".to_string()),
        apply_patches_block = if patches_dir.is_some() {
            r#"# Apply patches (VM kernel applies all: *.patch + *.vm.patch)
if [[ -f "$SHA_MARKER" ]] && [[ "$(cat "$SHA_MARKER")" == "$BUILD_SHA" ]]; then
    echo "Patches already applied (SHA: $BUILD_SHA)"
else
    echo "Applying patches..."

    # Track applied patches to avoid duplicates (*.patch glob also matches *.vm.patch)
    declare -A applied_patches

    for patch_file in "$PATCHES_DIR"/*.patch "$PATCHES_DIR"/*.vm.patch; do
        [[ ! -f "$patch_file" ]] && continue
        [[ -n "${applied_patches[$patch_file]:-}" ]] && continue
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
fi"#
        } else {
            "# No patches to apply"
        },
        kernel_arch = kernel_arch,
        kernel_image = kernel_image,
        kernel_source_cache = kernel_source_cache_shell(),
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
    let sha = compute_profile_kernel_sha(profile)?;

    // Generate the build script
    let script_content =
        generate_vm_kernel_build_script(profile, profile_name, &sha, dest, &repo_root)?;

    // Write to temp file
    let script_path = std::env::temp_dir().join(format!("fcvm-kernel-build-{}.sh", sha));
    std::fs::write(&script_path, &script_content).context("writing build script")?;
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
        .context("setting script permissions")?;

    info!(script = %script_path.display(), "generated kernel build script");

    // Kernel compilation needs no privilege (the rootless Makefile path runs
    // this same script as the user); under sudo, drop the child so a failed
    // build cannot leave a root-owned /tmp build tree that blocks the next
    // rootless build of the same profile.
    let mut cmd = Command::new(&script_path);
    super::run_build_as_sudo_invoker(&mut cmd);
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

    // Empty string means no patches, None means use default
    let patches_dir = match config.patches_dir.as_deref() {
        Some("") => None, // Explicitly disabled
        Some(p) => Some(repo_root.join(p)),
        None => Some(repo_root.join("kernel/patches")), // Default
    };

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
{patches_dir_line}
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

{kernel_source_cache}

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

{apply_patches_block}

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

# Detect kernel architecture from running system
case "$(uname -m)" in
    x86_64)  KERNEL_ARCH="x86" ;;
    aarch64) KERNEL_ARCH="arm64" ;;
    *)       echo "Unsupported architecture: $(uname -m)"; exit 1 ;;
esac
echo "Detected architecture: $KERNEL_ARCH"

# Update config for new kernel version
echo "Updating config for kernel ${{KERNEL_VERSION}}..."
make ARCH="$KERNEL_ARCH" olddefconfig

# Disable module signing (we don't have AWS signing keys)
echo "Disabling module signing..."
scripts/config --disable MODULE_SIG
scripts/config --disable MODULE_SIG_ALL
scripts/config --set-str SYSTEM_TRUSTED_KEYS ""
scripts/config --set-str SYSTEM_REVOCATION_KEYS ""
make ARCH="$KERNEL_ARCH" olddefconfig

# Build deb packages
echo ""
echo "Building kernel deb packages with $NPROC parallel jobs..."
echo "LOCALVERSION=$LOCALVERSION"
echo "This takes 15-30 minutes..."
echo ""

make -j"$NPROC" ARCH="$KERNEL_ARCH" LOCALVERSION="$LOCALVERSION" bindeb-pkg

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
        kernel_source_cache = kernel_source_cache_shell(),
        patches_dir_line = patches_dir
            .as_ref()
            .map(|p| format!("PATCHES_DIR=\"{}\"", p.display()))
            .unwrap_or_else(|| "# No patches directory".to_string()),
        apply_patches_block = if patches_dir.is_some() {
            r#"# Apply patches (host kernel: *.patch only, skip *.vm.patch)
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
fi"#
        } else {
            "# No patches to apply"
        },
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

    // Stays root by design: this interactive EC2 flow pairs the build with
    // dpkg -i and update-grub, which require root anyway.
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

    // Find the linux-image deb for THIS build (exclude dbg packages). The build
    // dir is shared across builds and may still hold debs from older SHAs, so
    // filter by the expected package name instead of taking any linux-image-*.deb.
    let build_dir = Path::new("/tmp/kernel-build-host");
    let pattern = format!("{}/{}_*.deb", build_dir.display(), expected_pkg);
    let debs: Vec<_> = glob::glob(&pattern)
        .context("globbing for deb files")?
        .filter_map(|r| r.ok())
        .filter(|p| !p.to_string_lossy().contains("-dbg"))
        .collect();

    if debs.is_empty() {
        bail!(
            "No {} deb found in {} after build",
            expected_pkg,
            build_dir.display()
        );
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

/// Fetch the short commit identity used by the unpinned Cloud Hypervisor path.
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

/// Parse the commit selected for a Firecracker ref.
///
/// Kept separate from the command runner so ref-resolution identity can be
/// tested without mutating process-global PATH or contacting GitHub.
fn parse_firecracker_remote_commit(stdout: &str) -> Result<String> {
    let commits: Vec<&str> = stdout
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    if commits.len() != 1 {
        bail!(
            "expected exactly one firecracker ref, found {}",
            commits.len()
        );
    }
    validate_full_git_commit(commits[0], "resolved firecracker ref")?;
    Ok(commits[0].to_string())
}

/// Fetch the commit selected for a Firecracker repo/ref.
async fn fetch_remote_firecracker_commit(repo: &str, branch: &str) -> Result<String> {
    let url = format!("https://github.com/{}", repo);
    let output = tokio::process::Command::new("git")
        .args(["ls-remote", "--refs", &url, branch])
        .output()
        .await
        .context("running git ls-remote for firecracker")?;

    if !output.status.success() {
        bail!(
            "git ls-remote failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    parse_firecracker_remote_commit(&String::from_utf8_lossy(&output.stdout))
}

/// Select the source commit after resolving the configured remote ref.
fn select_firecracker_commit(pinned_commit: Option<&str>, resolved_commit: &str) -> Result<String> {
    validate_full_git_commit(resolved_commit, "resolved firecracker ref")?;
    if let Some(pinned_commit) = pinned_commit {
        validate_full_git_commit(pinned_commit, "configured firecracker commit")?;
        if pinned_commit != resolved_commit {
            bail!(
                "configured firecracker commit {} does not match resolved ref commit {}",
                pinned_commit,
                resolved_commit
            );
        }
        return Ok(pinned_commit.to_string());
    }
    Ok(resolved_commit.to_string())
}

/// Verify that the clone used for a build has the source identity we selected.
fn verify_firecracker_checkout_commit(expected: &str, actual: &str) -> Result<()> {
    validate_full_git_commit(expected, "selected firecracker commit")?;
    validate_full_git_commit(actual, "cloned firecracker commit")?;
    if expected != actual {
        bail!(
            "cloned firecracker commit {} does not match selected commit {}",
            actual,
            expected
        );
    }
    Ok(())
}

fn validate_full_git_commit(commit: &str, source: &str) -> Result<()> {
    if commit.len() != 40
        || !commit
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!(
            "{} must be a full 40-character lowercase Git commit, got {:?}",
            source,
            commit
        );
    }
    Ok(())
}

/// Compute SHA for a profile Firecracker binary.
///
/// Repo, ref, full commit, architecture, and libc all affect the executable;
/// every one is part of the cache identity.
fn compute_profile_firecracker_sha_with_commit(
    profile: &KernelProfile,
    commit_hash: &str,
) -> String {
    compute_profile_firecracker_sha_for(
        profile,
        commit_hash,
        std::env::consts::ARCH,
        &libc_version_tag(),
    )
}

fn compute_profile_firecracker_sha_for(
    profile: &KernelProfile,
    commit_hash: &str,
    arch: &str,
    libc_tag: &str,
) -> String {
    let repo = profile.firecracker_repo.as_deref().unwrap_or("");
    let branch = profile.firecracker_branch.as_deref().unwrap_or("main");

    compute_firecracker_sha_for_fields(repo, branch, commit_hash, arch, libc_tag)
}

fn compute_firecracker_sha_for_fields(
    repo: &str,
    branch: &str,
    commit_hash: &str,
    arch: &str,
    libc_tag: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"fcvm-firecracker-cache-v2\0");
    // Length-prefix every field so shifting a byte across a field boundary
    // cannot alias another valid source/build identity.
    for field in [repo, branch, commit_hash, arch, libc_tag] {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field.as_bytes());
    }
    let result = hasher.finalize();
    hex::encode(&result[..6]) // 12 hex chars
}

fn profile_firecracker_path_for_build(
    dir: &Path,
    profile_name: &str,
    repo: &str,
    branch: &str,
    commit_hash: &str,
    arch: &str,
    libc_tag: &str,
) -> PathBuf {
    let sha = compute_firecracker_sha_for_fields(repo, branch, commit_hash, arch, libc_tag);
    dir.join(format!("firecracker-{profile_name}-{sha}.bin"))
}

fn firecracker_install_temp_path(bin_path: &Path, nonce: uuid::Uuid) -> PathBuf {
    let name = bin_path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "firecracker".into());
    bin_path.with_file_name(format!(".{name}.{nonce}.tmp"))
}

fn firecracker_build_dir(profile_name: &str, sha: &str, nonce: uuid::Uuid) -> PathBuf {
    PathBuf::from(format!(
        "/tmp/firecracker-build-{profile_name}-{sha}-{nonce}"
    ))
}

/// Return a string identifying the C library (e.g. "glibc-2.39" or "musl-1.2.4").
/// Used to namespace the firecracker binary cache per build environment.
///
/// Memoised for the lifetime of the process: the host's libc cannot change while
/// fcvm runs, and several content-addressed SHAs (firecracker, pasta,
/// cloud-hypervisor) mix it in, which otherwise costs one `ldd --version` exec
/// each on the VM-launch path.
pub(crate) fn libc_version_tag() -> String {
    static TAG: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TAG.get_or_init(probe_libc_version_tag).clone()
}

fn probe_libc_version_tag() -> String {
    // Try GNU libc first (most common on host and Ubuntu containers)
    if let Ok(output) = std::process::Command::new("ldd").arg("--version").output() {
        // ldd --version prints to stdout on glibc, stderr on musl
        let text = String::from_utf8_lossy(&output.stdout);
        let text = if text.is_empty() {
            String::from_utf8_lossy(&output.stderr).to_string()
        } else {
            text.to_string()
        };
        // Extract version like "2.39" from "ldd (Ubuntu GLIBC 2.39-0ubuntu8.4) 2.39"
        for line in text.lines() {
            if let Some(ver) = line.split_whitespace().last() {
                if ver.contains('.') {
                    return format!("libc-{}", ver);
                }
            }
        }
    }
    // Fallback: unknown libc, binary won't be shared
    "libc-unknown".to_string()
}

// ============================================================================
// Profile Firecracker resolution cache
// ============================================================================

/// Default freshness window for a cached firecracker resolution.
const FIRECRACKER_RESOLVE_TTL_DEFAULT_SECS: u64 = 3600;

/// Env var overriding the resolution TTL, in seconds. `0` forces a remote refresh.
const FIRECRACKER_RESOLVE_TTL_ENV: &str = "FCVM_FIRECRACKER_RESOLVE_TTL_SECS";

/// A previously completed remote resolution of a profile's firecracker binary.
///
/// Persisted under `assets_dir/firecracker/` with a profile plus full
/// repo/ref/architecture/libc identity so VM launches can reuse it instead of
/// paying a `git ls-remote` round trip to GitHub. Independent build
/// environments never overwrite one another's record.
///
/// Concurrency: written with a unique temp file + atomic rename, never a lock —
/// `assets_dir` can be shared across nesting levels over fuse-pipe, where
/// `flock` does not cross the VM boundary (see the cross-VM cache race notes in
/// AGENTS.md). Readers therefore always see a whole file. Two writers racing
/// resolve the *same* repo/branch, so the loser's entry is equally valid; if the
/// branch moved between them the older commit simply expires at the next TTL,
/// and `fcvm setup` rewrites the entry unconditionally.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct FirecrackerResolution {
    repo: String,
    branch: String,
    commit_hash: String,
    resolved_path: PathBuf,
    /// Unix seconds at which the `git ls-remote` that produced `commit_hash` ran.
    resolved_at_secs: u64,
}

/// Directory holding the profile firecracker binaries and their resolution cache.
fn firecracker_cache_dir() -> PathBuf {
    paths::assets_dir().join("firecracker")
}

fn firecracker_resolution_identity(repo: &str, branch: &str, arch: &str, libc_tag: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"fcvm-firecracker-resolution-v1\0");
    for field in [repo, branch, arch, libc_tag] {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field.as_bytes());
    }
    hex::encode(hasher.finalize())
}

#[cfg(test)]
fn firecracker_resolution_path(
    dir: &Path,
    profile_name: &str,
    repo: &str,
    branch: &str,
) -> PathBuf {
    firecracker_resolution_path_for_build(
        dir,
        profile_name,
        repo,
        branch,
        std::env::consts::ARCH,
        &libc_version_tag(),
    )
}

fn firecracker_resolution_path_for_build(
    dir: &Path,
    profile_name: &str,
    repo: &str,
    branch: &str,
    arch: &str,
    libc_tag: &str,
) -> PathBuf {
    let identity = firecracker_resolution_identity(repo, branch, arch, libc_tag);
    dir.join(format!("{profile_name}-{identity}.resolved.json"))
}

/// How long a cached resolution stays usable before the remote is re-queried.
fn firecracker_resolve_ttl_secs() -> u64 {
    parse_resolve_ttl(std::env::var(FIRECRACKER_RESOLVE_TTL_ENV).ok().as_deref())
}

/// Parse `FCVM_FIRECRACKER_RESOLVE_TTL_SECS`. Unset or unparsable falls back to
/// the default; `0` means "always re-query the remote".
fn parse_resolve_ttl(raw: Option<&str>) -> u64 {
    let Some(raw) = raw else {
        return FIRECRACKER_RESOLVE_TTL_DEFAULT_SECS;
    };
    raw.trim().parse::<u64>().unwrap_or_else(|_| {
        warn!(
            var = FIRECRACKER_RESOLVE_TTL_ENV,
            value = %raw,
            default = FIRECRACKER_RESOLVE_TTL_DEFAULT_SECS,
            "invalid firecracker resolve TTL, using default"
        );
        FIRECRACKER_RESOLVE_TTL_DEFAULT_SECS
    })
}

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Return the cached resolution for `profile_name` if it is usable right now:
/// same repo/branch, within the TTL, and pointing at a binary that still exists.
///
/// Any parse/IO problem is a miss (the caller re-resolves), never an error —
/// a corrupt cache must not be able to block a VM launch.
fn fresh_cached_firecracker_resolution_in(
    dir: &Path,
    profile_name: &str,
    repo: &str,
    branch: &str,
    ttl_secs: u64,
) -> Option<PathBuf> {
    fresh_cached_firecracker_resolution_in_for_build(
        dir,
        profile_name,
        repo,
        branch,
        ttl_secs,
        std::env::consts::ARCH,
        &libc_version_tag(),
    )
}

fn fresh_cached_firecracker_resolution_in_for_build(
    dir: &Path,
    profile_name: &str,
    repo: &str,
    branch: &str,
    ttl_secs: u64,
    arch: &str,
    libc_tag: &str,
) -> Option<PathBuf> {
    if ttl_secs == 0 {
        return None;
    }
    let path =
        firecracker_resolution_path_for_build(dir, profile_name, repo, branch, arch, libc_tag);
    let raw = std::fs::read_to_string(&path).ok()?;
    let entry: FirecrackerResolution = match serde_json::from_str(&raw) {
        Ok(e) => e,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "ignoring unreadable firecracker resolution cache");
            return None;
        }
    };
    if let Err(error) = validate_full_git_commit(&entry.commit_hash, "cached firecracker commit") {
        warn!(
            path = %path.display(),
            error = %error,
            "ignoring firecracker resolution cache without a full commit identity"
        );
        return None;
    }
    if entry.repo != repo || entry.branch != branch {
        return None;
    }
    let expected_path = profile_firecracker_path_for_build(
        dir,
        profile_name,
        repo,
        branch,
        &entry.commit_hash,
        arch,
        libc_tag,
    );
    if entry.resolved_path != expected_path {
        warn!(
            path = %path.display(),
            recorded_path = %entry.resolved_path.display(),
            expected_path = %expected_path.display(),
            "ignoring firecracker resolution cache for another build identity"
        );
        return None;
    }
    // A resolution stamped in the future (clock moved backwards) is treated as
    // stale rather than as infinitely fresh.
    let age = unix_now_secs().checked_sub(entry.resolved_at_secs)?;
    if age >= ttl_secs {
        return None;
    }
    if !entry.resolved_path.exists() {
        return None;
    }
    debug!(
        profile = %profile_name,
        path = %entry.resolved_path.display(),
        commit = %entry.commit_hash,
        age_secs = age,
        ttl_secs,
        "using cached firecracker resolution (no git ls-remote)"
    );
    Some(entry.resolved_path)
}

/// Persist a completed remote resolution so later launches can skip the network.
///
/// Written atomically (unique temp + rename) because several fcvm processes may
/// resolve the same profile concurrently; readers then always see a whole file.
/// Only called with a `resolved_path` that exists, so the cache never advertises
/// a binary that was never built.
fn record_firecracker_resolution_in(
    dir: &Path,
    profile_name: &str,
    repo: &str,
    branch: &str,
    commit_hash: &str,
    resolved_path: &Path,
) {
    record_firecracker_resolution_in_for_build(
        dir,
        profile_name,
        repo,
        branch,
        commit_hash,
        resolved_path,
        FirecrackerBuildEnvironment {
            arch: std::env::consts::ARCH,
            libc_tag: &libc_version_tag(),
        },
    );
}

#[derive(Clone, Copy)]
struct FirecrackerBuildEnvironment<'a> {
    arch: &'a str,
    libc_tag: &'a str,
}

fn record_firecracker_resolution_in_for_build(
    dir: &Path,
    profile_name: &str,
    repo: &str,
    branch: &str,
    commit_hash: &str,
    resolved_path: &Path,
    build: FirecrackerBuildEnvironment<'_>,
) {
    let entry = FirecrackerResolution {
        repo: repo.to_string(),
        branch: branch.to_string(),
        commit_hash: commit_hash.to_string(),
        resolved_path: resolved_path.to_path_buf(),
        resolved_at_secs: unix_now_secs(),
    };
    let final_path = firecracker_resolution_path_for_build(
        dir,
        profile_name,
        repo,
        branch,
        build.arch,
        build.libc_tag,
    );
    if let Err(e) = write_json_atomic(&final_path, &entry) {
        // Non-fatal: the next launch just pays the ls-remote again.
        warn!(
            path = %final_path.display(),
            error = %e,
            "could not persist firecracker resolution cache"
        );
        return;
    }
    debug!(
        profile = %profile_name,
        path = %resolved_path.display(),
        commit = %commit_hash,
        "recorded firecracker resolution"
    );
}

/// [`record_firecracker_resolution_in`] against the real assets dir.
fn record_firecracker_resolution(
    profile_name: &str,
    repo: &str,
    branch: &str,
    commit_hash: &str,
    resolved_path: &Path,
) {
    record_firecracker_resolution_in(
        &firecracker_cache_dir(),
        profile_name,
        repo,
        branch,
        commit_hash,
        resolved_path,
    );
}

/// Serialize `value` as JSON to `final_path` via a unique temp file + rename.
///
/// The temp name carries a uuid (not a pid — separate PID namespaces reuse
/// numbers) so concurrent writers never share a temp file, and the rename makes
/// the swap atomic for readers.
fn write_json_atomic<T: serde::Serialize>(final_path: &Path, value: &T) -> Result<()> {
    let dir = final_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no parent directory for {}", final_path.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        final_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "cache".to_string()),
        uuid::Uuid::new_v4()
    ));
    let json = serde_json::to_vec_pretty(value).context("serializing cache entry")?;
    if let Err(e) = std::fs::write(&tmp, &json) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("writing {}", tmp.display()));
    }
    if let Err(e) = std::fs::rename(&tmp, final_path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("renaming into {}", final_path.display()));
    }
    super::give_store_entry_to_invoker(final_path);
    Ok(())
}

/// Locate `cargo` for source builds (firecracker, cloud-hypervisor).
///
/// `sudo fcvm setup` runs with root's PATH, which does not carry the invoking
/// user's rustup cargo, so spawning bare "cargo" dies with ENOENT (proven by
/// the Build Btrfs Kernel job: `building firecracker: No such file or
/// directory`). Resolution order: `$CARGO`, then PATH, then `.cargo/bin/cargo`
/// under `$HOME` and under the sudo invoker's home.
fn cargo_program() -> Result<PathBuf> {
    let homes = cargo_fallback_homes();
    resolve_cargo(std::env::var_os("CARGO"), std::env::var_os("PATH"), &homes).ok_or_else(|| {
        anyhow::anyhow!(
            "cargo not found: not in $CARGO, not on PATH, and no .cargo/bin/cargo under {}. \
             Building this component needs a Rust toolchain — install rustup, or when running \
             under sudo keep the invoking user's rustup install in place",
            homes
                .iter()
                .map(|h| h.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
        )
    })
}

fn cargo_fallback_homes() -> Vec<PathBuf> {
    fallback_homes_for(
        crate::setup::sudo_invoker().map(|user| user.dir.clone()),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

/// Resolve for the identity that will execute. With a sudo invoker, the build
/// child drops to that user, so only their home is a usable fallback: root's
/// $HOME (/root) is typically 0700 and a cargo found there would fail the
/// dropped child's spawn with EACCES despite the invoker's own cargo working.
fn fallback_homes_for(invoker_home: Option<PathBuf>, env_home: Option<PathBuf>) -> Vec<PathBuf> {
    match invoker_home {
        Some(home) => vec![home],
        None => env_home.into_iter().collect(),
    }
}

/// Absolutize a candidate against the caller's cwd, then require a regular
/// executable file. The build command sets current_dir(build_dir), so a
/// relative candidate must be pinned to the file that was validated — the
/// child would otherwise re-resolve it inside the cloned repo. The exec check
/// matches execvp, which skips a PATH candidate it cannot execute and keeps
/// searching.
fn absolute_executable(candidate: &Path) -> Option<PathBuf> {
    let candidate = std::path::absolute(candidate).ok()?;
    let executable = std::fs::metadata(&candidate)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111) != 0)
        .unwrap_or(false);
    executable.then_some(candidate)
}

/// The `which` crate is not used here on purpose: `which_in` implements the
/// full POSIX rule where an empty PATH entry means the working directory, and
/// this resolver frequently runs as root — it must never pick up a ./cargo
/// from whatever directory setup happens to run in. Empty entries are skipped
/// instead, and the five tests below pin the rest of the ladder.
fn resolve_cargo(
    cargo_env: Option<std::ffi::OsString>,
    path_env: Option<std::ffi::OsString>,
    homes: &[PathBuf],
) -> Option<PathBuf> {
    if let Some(cargo) = cargo_env {
        if !cargo.is_empty() {
            if let Some(cargo) = absolute_executable(&PathBuf::from(cargo)) {
                return Some(cargo);
            }
        }
    }
    if let Some(path) = path_env {
        for dir in std::env::split_paths(&path) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            if let Some(cargo) = absolute_executable(&dir.join("cargo")) {
                return Some(cargo);
            }
        }
    }
    for home in homes {
        if let Some(cargo) = absolute_executable(&home.join(".cargo/bin/cargo")) {
            return Some(cargo);
        }
    }
    None
}

/// Get the content-addressed path for profile firecracker binary.
/// Uses assets_dir/firecracker/ alongside kernels and other assets.
///
/// Returns `Ok(None)` when the profile does not configure a custom firecracker.
///
/// # Resolution contract
///
/// This runs on the **VM launch / snapshot clone hot path**, so it does NOT
/// contact the network on every call. Resolution order:
///
/// When `firecracker_commit` is set, its exact content-addressed path is
/// computed locally. Launch never contacts the network and never falls back to
/// another cached build. Setup verifies both the remote ref and cloned HEAD.
///
/// Without a commit pin, resolution order is:
///
/// 1. The profile's repo/ref/architecture/libc-specific resolution record —
///    the result of the last remote resolution for that exact build identity.
///    Used when it is younger than the TTL and still points at its exact binary.
/// 2. `git ls-remote` against the configured repo/branch, which then rewrites
///    the cache (only when the resolved binary exists).
/// 3. Offline fallback: the same exact identity's stale resolution record.
///    Errors if the remote is unreachable and that record/binary is absent.
///
/// The TTL is `FCVM_FIRECRACKER_RESOLVE_TTL_SECS` seconds (default 3600);
/// setting it to `0` forces a remote refresh on every call.
///
/// **A rebuild must never leave a launcher on a stale binary.**
/// [`ensure_profile_firecracker`] — the `fcvm setup` path, including
/// `--build-kernels` / an explicit `--kernel-profile` — always performs the
/// remote resolution and rewrites this cache with the binary it just
/// installed. So `fcvm setup` is what makes an updated fork visible, and it
/// takes effect immediately rather than after the TTL expires.
pub async fn get_profile_firecracker_path(
    profile: &KernelProfile,
    profile_name: &str,
) -> Result<Option<PathBuf>> {
    get_profile_firecracker_path_in(profile, profile_name, &firecracker_cache_dir()).await
}

async fn get_profile_firecracker_path_in(
    profile: &KernelProfile,
    profile_name: &str,
    firecracker_dir: &Path,
) -> Result<Option<PathBuf>> {
    // Only return path if profile has a custom firecracker configured
    let repo = match profile.firecracker_repo.as_deref() {
        Some(r) => r,
        None if profile.firecracker_commit.is_some() => {
            bail!("firecracker_commit requires firecracker_repo")
        }
        None => return Ok(None),
    };
    let branch = profile.firecracker_branch.as_deref().unwrap_or("main");

    // An immutable pin makes the launch path fully offline and forbids falling
    // back to a different cached build. `fcvm setup` already verified that the
    // configured ref and cloned checkout both named this exact commit.
    if let Some(commit_hash) = profile.firecracker_commit.as_deref() {
        validate_full_git_commit(commit_hash, "configured firecracker commit")?;
        let resolved = profile_firecracker_path_for_build(
            firecracker_dir,
            profile_name,
            repo,
            branch,
            commit_hash,
            std::env::consts::ARCH,
            &libc_version_tag(),
        );
        return Ok(Some(resolved));
    }

    // Fast path: reuse the last remote resolution and skip the network entirely.
    let ttl_secs = firecracker_resolve_ttl_secs();
    if let Some(cached) = fresh_cached_firecracker_resolution_in(
        firecracker_dir,
        profile_name,
        repo,
        branch,
        ttl_secs,
    ) {
        return Ok(Some(cached));
    }

    // Fetch latest commit hash to detect updates
    match fetch_remote_firecracker_commit(repo, branch).await {
        Ok(resolved_commit) => {
            let commit_hash = select_firecracker_commit(None, &resolved_commit)?;
            let resolved = profile_firecracker_path_for_build(
                firecracker_dir,
                profile_name,
                repo,
                branch,
                &commit_hash,
                std::env::consts::ARCH,
                &libc_version_tag(),
            );
            // Only cache resolutions that name a binary that actually exists, so
            // a not-yet-built profile keeps re-resolving (and keeps failing loudly
            // in get_firecracker_for_profile) instead of caching a dead path.
            if resolved.exists() {
                record_firecracker_resolution_in(
                    firecracker_dir,
                    profile_name,
                    repo,
                    branch,
                    &commit_hash,
                    &resolved,
                );
            }
            Ok(Some(resolved))
        }
        Err(e) => match offline_cached_firecracker_resolution_in(
            firecracker_dir,
            profile_name,
            repo,
            branch,
            std::env::consts::ARCH,
            &libc_version_tag(),
        )? {
            Some(cached) => {
                warn!(
                    profile = %profile_name,
                    error = %e,
                    path = %cached.display(),
                    "could not query remote firecracker commit; using cached binary"
                );
                Ok(Some(cached))
            }
            None => Err(e.context(format!(
                "could not query remote firecracker commit for profile '{}' and no cached \
                 firecracker-{}-*.bin binary exists",
                profile_name, profile_name
            ))),
        },
    }
}

/// Find the most recently modified cached firecracker binary for a profile.
fn offline_cached_firecracker_resolution_in(
    firecracker_dir: &Path,
    profile_name: &str,
    repo: &str,
    branch: &str,
    arch: &str,
    libc_tag: &str,
) -> Result<Option<PathBuf>> {
    Ok(fresh_cached_firecracker_resolution_in_for_build(
        firecracker_dir,
        profile_name,
        repo,
        branch,
        u64::MAX,
        arch,
        libc_tag,
    ))
}

/// Get the firecracker binary path for a kernel profile.
///
/// Returns the custom firecracker from the profile if configured (firecracker_repo),
/// otherwise falls back to the system firecracker via which.
///
/// This is the canonical way to get the firecracker path for a profile.
pub async fn get_firecracker_for_profile(
    profile: &KernelProfile,
    profile_name: &str,
) -> Result<PathBuf> {
    // Check for custom firecracker from profile
    if let Some(custom_fc) = get_profile_firecracker_path(profile, profile_name).await? {
        if !custom_fc.exists() {
            bail!(
                "Custom firecracker not found at {}. Run: fcvm setup --kernel-profile {}",
                custom_fc.display(),
                profile_name
            );
        }
        return Ok(custom_fc);
    }

    // Fall back to system firecracker
    which::which("firecracker").context("firecracker not found in PATH")
}

/// Resolve a profile's explicitly configured Firecracker, if it has one.
///
/// This seam keeps cold boot and snapshot restore on one resolution policy.
pub async fn get_configured_firecracker_for_profile(
    profile: &KernelProfile,
    profile_name: &str,
) -> Result<Option<PathBuf>> {
    if profile.firecracker_repo.is_none() && profile.firecracker_commit.is_none() {
        return Ok(None);
    }
    get_firecracker_for_profile(profile, profile_name)
        .await
        .map(Some)
        .with_context(|| format!("resolving configured Firecracker for profile '{profile_name}'"))
}

/// Ensure the firecracker binary for a kernel profile exists.
///
/// Uses content-addressed naming: firecracker-{profile}-{sha}.bin, where SHA
/// covers repo + ref + full commit + architecture + libc. With a configured
/// commit pin, setup fails closed unless both the remote ref and cloned HEAD
/// equal that pin. Without a pin it detects and builds the current ref commit.
///
/// This is the `fcvm setup` path and it **always** performs the remote
/// resolution (`git ls-remote`), then rewrites the launch-time resolution cache
/// read by [`get_profile_firecracker_path`]. That is what makes an updated fork
/// take effect for VM launches immediately rather than after the cache TTL —
/// `fcvm setup` is the sanctioned way to pick up a new firecracker build.
pub async fn ensure_profile_firecracker(
    profile: &KernelProfile,
    profile_name: &str,
) -> Result<Option<PathBuf>> {
    // Check if profile needs custom firecracker
    let repo = match profile.firecracker_repo.as_deref() {
        Some(r) => r,
        None if profile.firecracker_commit.is_some() => {
            bail!("firecracker_commit requires firecracker_repo")
        }
        None => return Ok(None), // No custom firecracker needed
    };

    let branch = profile.firecracker_branch.as_deref().unwrap_or("main");

    // Fetch latest commit hash to detect updates
    let resolved_commit = fetch_remote_firecracker_commit(repo, branch).await?;
    let commit_hash =
        select_firecracker_commit(profile.firecracker_commit.as_deref(), &resolved_commit)?;
    let sha = compute_profile_firecracker_sha_with_commit(profile, &commit_hash);

    // Content-addressed path in assets dir (alongside kernels)
    let firecracker_dir = paths::assets_dir().join("firecracker");
    let filename = format!("firecracker-{}-{}.bin", profile_name, sha);
    let bin_path = firecracker_dir.join(&filename);

    // Already exists — use it
    if bin_path.exists() {
        info!(
            path = %bin_path.display(),
            profile = %profile_name,
            sha = %sha,
            "firecracker binary exists"
        );
        record_firecracker_resolution(profile_name, repo, branch, &commit_hash, &bin_path);
        return Ok(Some(bin_path));
    }

    // Resolve the build toolchain before creating anything: a missing cargo
    // must not cost a network clone or leave a fresh dir/lock behind.
    let cargo = cargo_program()?;

    // Create directory and acquire lock
    let flock = super::lock_store_dir(
        &firecracker_dir.join(format!("{}.lock", filename)),
        "firecracker build",
    )
    .await?;

    // Double-check after lock (another process may have built it)
    if bin_path.exists() {
        debug!(path = %bin_path.display(), "firecracker exists (built by another process)");
        flock.unlock().map_err(|(_, err)| err)?;
        record_firecracker_resolution(profile_name, repo, branch, &commit_hash, &bin_path);
        return Ok(Some(bin_path));
    }

    println!(
        "  → Building firecracker from {} (branch: {}, sha: {})...",
        repo, branch, sha
    );
    println!("    This may take 5-10 minutes...");

    // Build in a per-invocation temp directory. The flock above may live on a
    // fuse-pipe shared assets directory, where separate guest kernels do not
    // share flock state; unique paths keep those builders from deleting or
    // compiling inside one another's checkout.
    let build_dir = firecracker_build_dir(profile_name, &sha, uuid::Uuid::new_v4());

    // Clean up old build
    if build_dir.exists() {
        tokio::fs::remove_dir_all(&build_dir)
            .await
            .context("removing old firecracker build directory")?;
    }

    // Clone repo (as the sudo invoker, so the checkout is not root-owned)
    let clone_url = format!("https://github.com/{}", repo);
    let mut clone_cmd = Command::new("git");
    clone_cmd.args([
        "clone",
        "--depth=1",
        "--single-branch",
        "--no-tags",
        "-b",
        branch,
        &clone_url,
        build_dir.to_str().unwrap(),
    ]);
    super::run_build_as_sudo_invoker(&mut clone_cmd);
    let status = clone_cmd
        .status()
        .await
        .context("cloning firecracker repo")?;

    if !status.success() {
        flock.unlock().map_err(|(_, err)| err)?;
        bail!("Failed to clone firecracker repo from {}", clone_url);
    }

    let checkout = Command::new("git")
        .args(["-C", build_dir.to_str().unwrap(), "rev-parse", "HEAD"])
        .output()
        .await
        .context("reading cloned firecracker HEAD")?;
    if !checkout.status.success() {
        flock.unlock().map_err(|(_, err)| err)?;
        bail!(
            "git rev-parse failed: {}",
            String::from_utf8_lossy(&checkout.stderr)
        );
    }
    if let Err(error) = verify_firecracker_checkout_commit(
        &commit_hash,
        String::from_utf8_lossy(&checkout.stdout).trim(),
    ) {
        flock.unlock().map_err(|(_, err)| err)?;
        return Err(error);
    }

    // Build firecracker
    let mut build_cmd = Command::new(&cargo);
    build_cmd
        .args(["build", "--release", "-p", "firecracker"])
        .current_dir(&build_dir);
    super::run_build_as_sudo_invoker(&mut build_cmd);
    let status = build_cmd.status().await.context("building firecracker")?;

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

    // Copy to a temp name in the same directory, then atomically rename onto the
    // content-addressed path. An interrupted or failed copy (kill, ENOSPC) must
    // never leave a partial binary that later runs treat as valid.
    let temp_path = firecracker_install_temp_path(&bin_path, uuid::Uuid::new_v4());
    let _ = tokio::fs::remove_file(&temp_path).await;
    if let Err(e) = tokio::fs::copy(&binary, &temp_path).await {
        let _ = tokio::fs::remove_file(&temp_path).await;
        let _ = flock.unlock();
        return Err(e).context("installing firecracker binary");
    }
    if let Err(e) = super::publish_store_entry(&temp_path, &bin_path, "firecracker binary").await {
        let _ = tokio::fs::remove_file(&temp_path).await;
        let _ = flock.unlock();
        return Err(e);
    }

    // Clean up the build tree on success (failures keep it for debugging)
    let _ = tokio::fs::remove_dir_all(&build_dir).await;

    flock.unlock().map_err(|(_, err)| err)?;

    info!(
        path = %bin_path.display(),
        profile = %profile_name,
        sha = %sha,
        "firecracker binary installed"
    );
    println!("  ✓ Firecracker ready: {}", bin_path.display());

    record_firecracker_resolution(profile_name, repo, branch, &commit_hash, &bin_path);

    Ok(Some(bin_path))
}

// ============================================================================
// Cloud Hypervisor Setup (#632)
// ============================================================================
//
// Cloud Hypervisor is an OPTIONAL VMM backend (`--hypervisor cloud-hypervisor`).
// Unlike firecracker — which is per-kernel-profile — CH is a single global binary
// built from the `[cloud_hypervisor]` config (repo + branch). It is content-addressed
// exactly like firecracker (SHA over repo+branch+commit+libc), built on demand via
// `fcvm setup --cloud-hypervisor`, and resolved at launch by `find_cloud_hypervisor()`.

/// Compute the content-addressed SHA for the cloud-hypervisor binary.
/// Includes the target architecture and libc version: the built binary is an
/// arch-specific, dynamically-linked ELF, so an assets_dir copied or shared between
/// an x86_64 and an aarch64 builder must NOT resolve the same content-addressed name
/// (otherwise the second arch sees a false cache hit and runs an `Exec format error`
/// binary). Arch + libc segregate the cache entries.
fn compute_cloud_hypervisor_sha(repo: &str, branch: &str, commit_hash: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(repo.as_bytes());
    hasher.update(branch.as_bytes());
    hasher.update(commit_hash.as_bytes());
    hasher.update(std::env::consts::ARCH.as_bytes());
    hasher.update(libc_version_tag().as_bytes());
    let result = hasher.finalize();
    hex::encode(&result[..6]) // 12 hex chars
}

/// Find the most recently modified cached cloud-hypervisor binary.
///
/// Used both as an offline fallback during setup and as the primary resolution
/// path at VM-launch time (so launching never needs network / git ls-remote).
pub fn newest_cached_cloud_hypervisor() -> Option<PathBuf> {
    let ch_dir = paths::assets_dir().join("cloud-hypervisor");
    let pattern = format!("{}/cloud-hypervisor-*.bin", ch_dir.display());
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    let entries = match glob(&pattern) {
        Ok(e) => e,
        Err(_) => return None,
    };
    for path in entries.filter_map(|e| e.ok()) {
        let mtime = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if newest.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
            newest = Some((mtime, path));
        }
    }
    newest.map(|(_, path)| path)
}

/// Ensure the cloud-hypervisor binary exists, building it from the configured
/// repo/branch if missing.
///
/// Uses content-addressed naming: cloud-hypervisor-{sha}.bin where SHA is computed
/// from repo + branch + commit_hash + libc. Automatically detects and rebuilds when
/// new commits are pushed to the branch. Mirrors `ensure_profile_firecracker`.
pub async fn ensure_cloud_hypervisor(repo: &str, branch: &str) -> Result<PathBuf> {
    // Fetch latest commit hash to detect updates. This drives the fast-path existence
    // check; the binary's final name is re-derived from the actually-built HEAD below
    // (the branch could move between this ls-remote and the clone).
    let commit_hash = fetch_remote_commit_hash(repo, branch).await?;
    let mut sha = compute_cloud_hypervisor_sha(repo, branch, &commit_hash);

    // Content-addressed path in assets dir (alongside kernels and firecracker)
    let ch_dir = paths::assets_dir().join("cloud-hypervisor");
    let filename = format!("cloud-hypervisor-{}.bin", sha);
    let mut bin_path = ch_dir.join(&filename);

    // Already exists — use it
    if bin_path.exists() {
        info!(
            path = %bin_path.display(),
            sha = %sha,
            "cloud-hypervisor binary exists"
        );
        return Ok(bin_path);
    }

    // Resolve the build toolchain before creating anything: a missing cargo
    // must not cost a network clone or leave a fresh dir/lock behind.
    let cargo = cargo_program()?;

    // Acquire per-filename lock so concurrent setups don't collide
    let flock = super::lock_store_dir(
        &ch_dir.join(format!("{}.lock", filename)),
        "cloud-hypervisor build",
    )
    .await?;

    // Double-check after lock (another process may have built it)
    if bin_path.exists() {
        debug!(path = %bin_path.display(), "cloud-hypervisor exists (built by another process)");
        flock.unlock().map_err(|(_, err)| err)?;
        return Ok(bin_path);
    }

    println!(
        "  → Building cloud-hypervisor from {} (branch: {}, sha: {})...",
        repo, branch, sha
    );
    println!("    This may take 5-10 minutes...");

    // Build in a UNIQUE temp directory. The flock above is per assets_dir/<filename>,
    // so two setups with DIFFERENT assets_dirs (e.g. a custom --config) take different
    // locks for the same commit; a build dir keyed only on the sha would let one delete
    // the other's checkout mid-build. A per-invocation uuid removes that overlap (uuid,
    // not pid — separate PID namespaces reuse numbers; see the cross-VM cache race lesson).
    let build_dir = PathBuf::from(format!(
        "/tmp/cloud-hypervisor-build-{}-{}",
        sha,
        uuid::Uuid::new_v4()
    ));
    if build_dir.exists() {
        tokio::fs::remove_dir_all(&build_dir)
            .await
            .context("removing old cloud-hypervisor build directory")?;
    }

    // Clone repo (shallow, single branch, as the sudo invoker so the
    // checkout is not root-owned)
    let clone_url = format!("https://github.com/{}", repo);
    let mut clone_cmd = Command::new("git");
    clone_cmd.args([
        "clone",
        "--depth=1",
        "-b",
        branch,
        &clone_url,
        build_dir.to_str().unwrap(),
    ]);
    super::run_build_as_sudo_invoker(&mut clone_cmd);
    let status = clone_cmd
        .status()
        .await
        .context("cloning cloud-hypervisor repo")?;

    if !status.success() {
        let _ = tokio::fs::remove_dir_all(&build_dir).await;
        flock.unlock().map_err(|(_, err)| err)?;
        bail!("Failed to clone cloud-hypervisor repo from {}", clone_url);
    }

    // The cache identity (sha/bin_path) was computed from the ls-remote commit, but the
    // clone tracked the MUTABLE branch — if it moved in between, the binary would be built
    // from a different commit than its content-addressed name claims. Re-derive the name
    // from the actually-checked-out HEAD so the name always matches the bytes installed.
    let head_out = Command::new("git")
        .args(["-C", build_dir.to_str().unwrap(), "rev-parse", "HEAD"])
        .output()
        .await
        .context("reading cloned cloud-hypervisor HEAD")?;
    if head_out.status.success() {
        let head = String::from_utf8_lossy(&head_out.stdout);
        let head = head.trim();
        if let Some(built) = head.get(..12) {
            if built != commit_hash {
                let actual_sha = compute_cloud_hypervisor_sha(repo, branch, built);
                warn!(
                    resolved = %commit_hash,
                    built = %built,
                    sha = %actual_sha,
                    "cloud-hypervisor branch moved during build; naming binary after the built commit"
                );
                sha = actual_sha;
                bin_path = ch_dir.join(format!("cloud-hypervisor-{}.bin", sha));
                // Another process may have already built this exact commit.
                if bin_path.exists() {
                    let _ = tokio::fs::remove_dir_all(&build_dir).await;
                    flock.unlock().map_err(|(_, err)| err)?;
                    return Ok(bin_path);
                }
            }
        }
    }

    // Build cloud-hypervisor
    let mut build_cmd = Command::new(&cargo);
    build_cmd
        .args(["build", "--release", "--bin", "cloud-hypervisor"])
        .current_dir(&build_dir);
    super::run_build_as_sudo_invoker(&mut build_cmd);
    let status = build_cmd
        .status()
        .await
        .context("building cloud-hypervisor")?;

    if !status.success() {
        flock.unlock().map_err(|(_, err)| err)?;
        bail!("cloud-hypervisor build failed");
    }

    let binary = build_dir.join("target/release/cloud-hypervisor");
    if !binary.exists() {
        flock.unlock().map_err(|(_, err)| err)?;
        bail!("cloud-hypervisor binary not found at {}", binary.display());
    }

    // Copy to a temp name in the same directory, then atomically rename onto the
    // content-addressed path. An interrupted copy (kill, ENOSPC) must never leave
    // a partial binary that later runs treat as valid.
    let temp_path = bin_path.with_extension("tmp");
    let _ = tokio::fs::remove_file(&temp_path).await;
    if let Err(e) = tokio::fs::copy(&binary, &temp_path).await {
        let _ = tokio::fs::remove_file(&temp_path).await;
        let _ = flock.unlock();
        return Err(e).context("installing cloud-hypervisor binary");
    }
    if let Err(e) =
        super::publish_store_entry(&temp_path, &bin_path, "cloud-hypervisor binary").await
    {
        let _ = tokio::fs::remove_file(&temp_path).await;
        let _ = flock.unlock();
        return Err(e);
    }

    // Clean up the build tree on success (failures keep it for debugging)
    let _ = tokio::fs::remove_dir_all(&build_dir).await;

    flock.unlock().map_err(|(_, err)| err)?;

    info!(
        path = %bin_path.display(),
        sha = %sha,
        "cloud-hypervisor binary installed"
    );
    println!("  ✓ Cloud Hypervisor ready: {}", bin_path.display());

    Ok(bin_path)
}

async fn update_grub_config(kernel_name: &str, boot_args: Option<&str>) -> Result<()> {
    let grub_default = Path::new("/etc/default/grub");
    let grub_d = Path::new("/etc/default/grub.d");

    if !grub_default.exists() {
        bail!("/etc/default/grub not found");
    }

    // Cloud images have /etc/default/grub.d/50-cloudimg-settings.cfg that overrides
    // GRUB_CMDLINE_LINUX_DEFAULT. We need to use a higher-numbered file to override it.
    if grub_d.exists() {
        if let Some(args) = boot_args {
            let fcvm_cfg = grub_d.join("99-fcvm.cfg");
            let content = format!(
                "# fcvm kernel boot parameters\n\
                 # Overrides cloud-image defaults for nested virtualization\n\
                 GRUB_CMDLINE_LINUX_DEFAULT=\"$GRUB_CMDLINE_LINUX_DEFAULT {}\"\n",
                args
            );
            tokio::fs::write(&fcvm_cfg, &content)
                .await
                .context("writing /etc/default/grub.d/99-fcvm.cfg")?;
            println!("  → Created {} with boot args", fcvm_cfg.display());
        }
    }

    // Also update /etc/default/grub for GRUB_DEFAULT (kernel selection)
    let content = tokio::fs::read_to_string(grub_default)
        .await
        .context("reading /etc/default/grub")?;

    let mut modified = false;
    let mut new_lines = Vec::new();

    for line in content.lines() {
        if line.starts_with("GRUB_DEFAULT=") {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write_executable(path: &Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Whether a finished child failed only because a fixture executable was
    /// still open for writing somewhere.
    ///
    /// The outer script execs fixtures of its own (the fake `curl`, `make`,
    /// `cargo` on its PATH), and a shell reports that as exit 126 with "Text
    /// file busy" on stderr. Retrying only the direct spawn error misses
    /// every one of those, which is the shape that actually flakes.
    fn child_hit_text_file_busy(output: &std::process::Output) -> bool {
        output.status.code() == Some(126)
            && String::from_utf8_lossy(&output.stderr).contains("Text file busy")
    }

    /// Run a freshly written script, retrying "Text file busy" briefly.
    ///
    /// A sibling test thread can fork while `write_executable`'s descriptor is
    /// open; until that child execs (closing its CLOEXEC copy), execve of the
    /// script fails with ETXTBSY. The window cannot be closed from here: the
    /// libtest harness runs tests as threads in one process, so an unrelated
    /// test's `Command::spawn` can fork at any instant, and neither writing
    /// through a temp file nor renaming helps (an inherited descriptor names
    /// the same inode). Retrying both shapes — the direct spawn error and a
    /// child's 126 — covers what is observable; a still-busy fixture after
    /// the deadline fails loudly with the child's own output rather than an
    /// unwrap panic that names neither.
    fn output_retrying_etxtbsy(command: &mut std::process::Command) -> std::process::Output {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let retryable = match command.output() {
                Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) => None,
                Err(error) => panic!("running the generated kernel build script: {error}"),
                Ok(output) if child_hit_text_file_busy(&output) => Some(output),
                Ok(output) => return output,
            };
            if std::time::Instant::now() >= deadline {
                match retryable {
                    Some(output) => panic!(
                        "a fixture executable stayed busy for the whole retry budget\n\
                         stdout:\n{}\nstderr:\n{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    ),
                    None => panic!(
                        "the generated kernel build script stayed busy for the whole \
                         retry budget"
                    ),
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn a_busy_fixture_is_recognised_from_the_childs_own_report() {
        use std::os::unix::process::ExitStatusExt;
        let busy = std::process::Output {
            status: std::process::ExitStatus::from_raw(126 << 8),
            stdout: Vec::new(),
            stderr: b"/tmp/bin/curl: Text file busy\n".to_vec(),
        };
        assert!(child_hit_text_file_busy(&busy));

        // A genuine 126 (not executable) must NOT be retried away.
        let not_executable = std::process::Output {
            status: std::process::ExitStatus::from_raw(126 << 8),
            stdout: Vec::new(),
            stderr: b"/tmp/bin/curl: Permission denied\n".to_vec(),
        };
        assert!(!child_hit_text_file_busy(&not_executable));

        // Nor may an ordinary failure be mistaken for it.
        let ordinary = std::process::Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: Vec::new(),
            stderr: b"Text file busy\n".to_vec(),
        };
        assert!(!child_hit_text_file_busy(&ordinary));
    }

    #[test]
    fn vm_kernel_build_replaces_interrupted_cached_source_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let build_dir = tmp.path().join("build");
        let fixture_dir = tmp.path().join("fixture");
        let fixture_source = fixture_dir.join("linux-1.2.3");
        let fake_bin = tmp.path().join("bin");
        let output = tmp.path().join("output/vmlinux.bin");
        std::fs::create_dir_all(&build_dir).unwrap();
        std::fs::create_dir_all(&fixture_source).unwrap();
        std::fs::create_dir_all(&fake_bin).unwrap();

        // Model a setup process killed while curl was still writing the old
        // final-name cache entry. The old implementation saw this file,
        // skipped the download, and handed truncated bytes directly to tar.
        let cached_tarball = build_dir.join("linux-1.2.3.tar.xz");
        std::fs::write(&cached_tarball, b"interrupted kernel download").unwrap();

        std::fs::write(fixture_source.join("README"), b"kernel source fixture\n").unwrap();
        let valid_tarball = tmp.path().join("valid-linux-1.2.3.tar.xz");
        let tar_status = std::process::Command::new("tar")
            .args(["-cJf"])
            .arg(&valid_tarball)
            .arg("-C")
            .arg(&fixture_dir)
            .arg("linux-1.2.3")
            .status()
            .unwrap();
        assert!(
            tar_status.success(),
            "creating kernel source fixture failed"
        );

        write_executable(
            &fake_bin.join("curl"),
            r#"#!/bin/bash
set -euo pipefail
url=""
output=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        -o) output="$2"; shift 2 ;;
        -*) shift ;;
        *) url="$1"; shift ;;
    esac
done
if [[ "$url" == *.tar.xz ]]; then
    # The invalid final-name entry remains the cache's old commit marker until
    # the complete staging archive atomically replaces it. Removing it first
    # could race another builder that has just published a valid archive.
    grep -Fq 'interrupted kernel download' "$FCVM_TEST_CACHED_TARBALL"
    cp "$FCVM_TEST_KERNEL_TARBALL" "$output"
else
    printf 'CONFIG_FUSE_FS=y\n' >"$output"
fi
"#,
        );
        write_executable(
            &fake_bin.join("make"),
            r#"#!/bin/bash
set -euo pipefail
mkdir -p arch/arm64/boot
printf 'complete kernel image\n' >vmlinux
cp vmlinux arch/arm64/boot/Image
"#,
        );

        let profile = KernelProfile {
            kernel_version: "1.2.3".to_string(),
            base_config_url: Some("https://fixture.invalid/base.config".to_string()),
            patches_dir: Some(String::new()),
            ..Default::default()
        };
        let script = generate_vm_kernel_build_script(
            &profile,
            "fixture",
            "0123456789ab",
            &output,
            tmp.path(),
        )
        .unwrap();
        let script_path = tmp.path().join("kernel-build.sh");
        write_executable(&script_path, &script);

        let path = std::env::var_os("PATH").unwrap_or_default();
        let status = output_retrying_etxtbsy(
            std::process::Command::new(&script_path)
                .env("BUILD_DIR", &build_dir)
                .env("FCVM_TEST_CACHED_TARBALL", &cached_tarball)
                .env("FCVM_TEST_KERNEL_TARBALL", &valid_tarball)
                .env(
                    "PATH",
                    format!("{}:{}", fake_bin.display(), path.to_string_lossy()),
                ),
        );

        assert!(
            status.status.success(),
            "generated kernel build did not recover from the interrupted archive\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&status.stdout),
            String::from_utf8_lossy(&status.stderr)
        );
        assert_eq!(std::fs::read(&output).unwrap(), b"complete kernel image\n");
        assert!(
            std::process::Command::new("xz")
                .arg("-t")
                .arg(&cached_tarball)
                .status()
                .unwrap()
                .success(),
            "the final cache entry must be a complete xz stream"
        );
    }

    fn profile_with_inputs(build_inputs: Vec<String>) -> KernelProfile {
        KernelProfile {
            build_inputs,
            ..Default::default()
        }
    }

    #[test]
    fn profile_kernel_sha_hashes_resolved_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("kernel.conf");
        std::fs::write(&input, "CONFIG_KVM=y\n").unwrap();

        let profile = profile_with_inputs(vec![input.display().to_string()]);
        let sha = compute_profile_kernel_sha(&profile).unwrap();
        assert_eq!(sha.len(), 12);
        assert_ne!(sha, "000000000000");

        // Changing an input changes the cache key
        std::fs::write(&input, "CONFIG_KVM=y\nCONFIG_BTRFS_FS=y\n").unwrap();
        let sha2 = compute_profile_kernel_sha(&profile).unwrap();
        assert_ne!(sha, sha2);
    }

    #[test]
    fn profile_kernel_sha_errors_when_inputs_unresolvable() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist-*.patch");

        let profile = profile_with_inputs(vec![missing.display().to_string()]);
        let err = compute_profile_kernel_sha(&profile).unwrap_err();
        assert!(
            err.to_string().contains("matched no files"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn profile_kernel_sha_without_inputs_uses_constant() {
        let profile = profile_with_inputs(vec![]);
        assert_eq!(
            compute_profile_kernel_sha(&profile).unwrap(),
            "000000000000"
        );
    }

    #[test]
    fn published_profile_uses_manifest_without_source_checkout() {
        let profile = KernelProfile {
            build_inputs: vec!["kernel/not-installed.conf".to_string()],
            kernel_sha: Some("0123456789ab".to_string()),
            ..Default::default()
        };

        assert_eq!(
            compute_profile_kernel_sha_at_root(&profile, None).unwrap(),
            "0123456789ab"
        );
    }

    #[test]
    fn published_profile_rejects_manifest_that_disagrees_with_source() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("kernel.conf");
        std::fs::write(&input, "CONFIG_FUSE_FS=y\n").unwrap();

        let profile = KernelProfile {
            build_inputs: vec![input.display().to_string()],
            kernel_sha: Some("000000000000".to_string()),
            ..Default::default()
        };
        let err = compute_profile_kernel_sha_at_root(&profile, Some(dir.path())).unwrap_err();
        assert!(
            err.to_string().contains("does not match build_inputs hash"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn published_profile_rejects_malformed_manifest_sha() {
        let profile = KernelProfile {
            kernel_sha: Some("NOT-A-SHA".to_string()),
            ..Default::default()
        };
        let err = compute_profile_kernel_sha_at_root(&profile, None).unwrap_err();
        assert!(
            err.to_string().contains("12 lowercase hexadecimal"),
            "unexpected error: {err:#}"
        );
    }

    // ---------------------------------------------------------------------
    // Firecracker resolution cache (kills the git ls-remote on the hot path)
    // ---------------------------------------------------------------------

    const REPO: &str = "ejc3/firecracker";
    const BRANCH: &str = "bump-vsock-max-connections";
    const TTL: u64 = 3600;
    const PINNED_COMMIT: &str = "27305f49ab3a5d862dc56b5108713b6536d2baa7";
    const MOVED_COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn firecracker_remote_resolution_preserves_full_commit() {
        let output = format!("{PINNED_COMMIT}\trefs/heads/agent/nv2\n");
        assert_eq!(
            parse_firecracker_remote_commit(&output).unwrap(),
            PINNED_COMMIT,
            "the resolution manifest and checkout verifier need the full object id"
        );
    }

    #[test]
    fn firecracker_remote_resolution_rejects_ambiguous_or_malformed_refs() {
        let ambiguous =
            format!("{PINNED_COMMIT}\trefs/heads/release\n{MOVED_COMMIT}\trefs/tags/release\n");
        assert!(parse_firecracker_remote_commit(&ambiguous).is_err());
        assert!(parse_firecracker_remote_commit("1234\trefs/heads/main\n").is_err());
        assert!(
            select_firecracker_commit(Some("ABCDEF"), PINNED_COMMIT).is_err(),
            "a short or non-canonical pin must fail before any build"
        );
    }

    #[test]
    fn pinned_firecracker_ref_movement_is_rejected() {
        let err = select_firecracker_commit(Some(PINNED_COMMIT), MOVED_COMMIT).unwrap_err();
        assert!(
            err.to_string().contains(PINNED_COMMIT) && err.to_string().contains(MOVED_COMMIT),
            "the mismatch error must name both identities: {err:#}"
        );
    }

    #[test]
    fn firecracker_checkout_mismatch_is_rejected() {
        let err = verify_firecracker_checkout_commit(PINNED_COMMIT, MOVED_COMMIT).unwrap_err();
        assert!(
            err.to_string().contains(PINNED_COMMIT) && err.to_string().contains(MOVED_COMMIT),
            "the mismatch error must name both identities: {err:#}"
        );
    }

    #[test]
    fn firecracker_cache_key_includes_target_architecture() {
        let profile = KernelProfile {
            firecracker_repo: Some(REPO.to_string()),
            firecracker_branch: Some(BRANCH.to_string()),
            ..Default::default()
        };
        let arm =
            compute_profile_firecracker_sha_for(&profile, PINNED_COMMIT, "aarch64", "glibc-2.39");
        let x86 =
            compute_profile_firecracker_sha_for(&profile, PINNED_COMMIT, "x86_64", "glibc-2.39");
        assert_ne!(
            arm, x86,
            "shared assets must not alias Firecracker binaries for different architectures"
        );
    }

    #[test]
    fn firecracker_cache_key_separates_field_boundaries() {
        let first = KernelProfile {
            firecracker_repo: Some("ejc3/fire".to_string()),
            firecracker_branch: Some("cracker".to_string()),
            ..Default::default()
        };
        let second = KernelProfile {
            firecracker_repo: Some("ejc3/firec".to_string()),
            firecracker_branch: Some("racker".to_string()),
            ..Default::default()
        };
        assert_ne!(
            compute_profile_firecracker_sha_for(&first, PINNED_COMMIT, "aarch64", "glibc-2.39"),
            compute_profile_firecracker_sha_for(&second, PINNED_COMMIT, "aarch64", "glibc-2.39"),
            "adjacent cache-key fields must not admit boundary-shifting collisions"
        );
    }

    #[tokio::test]
    async fn configured_firecracker_pin_without_repo_is_rejected() {
        let profile = KernelProfile {
            firecracker_commit: Some(PINNED_COMMIT.to_string()),
            ..Default::default()
        };
        let error = get_configured_firecracker_for_profile(&profile, "pin-without-repo")
            .await
            .unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("firecracker_repo"), "{error}");
    }

    #[tokio::test]
    async fn missing_pinned_firecracker_never_falls_back_to_system_binary() {
        let profile = KernelProfile {
            firecracker_repo: Some(REPO.to_string()),
            firecracker_branch: Some(BRANCH.to_string()),
            firecracker_commit: Some(MOVED_COMMIT.to_string()),
            ..Default::default()
        };
        let profile_name = format!("missing-pin-{}", uuid::Uuid::new_v4());
        let error = get_configured_firecracker_for_profile(&profile, &profile_name)
            .await
            .unwrap_err();
        let error = format!("{error:#}");
        assert!(
            error.contains("Run: fcvm setup"),
            "the missing exact artifact must fail before PATH fallback: {error}"
        );
    }

    #[test]
    fn concurrent_firecracker_installs_use_unique_temp_paths() {
        let final_path = Path::new("/assets/firecracker/firecracker-nested-deadbeef.bin");
        let first = firecracker_install_temp_path(
            final_path,
            uuid::Uuid::from_u128(0x11111111111111111111111111111111),
        );
        let second = firecracker_install_temp_path(
            final_path,
            uuid::Uuid::from_u128(0x22222222222222222222222222222222),
        );
        assert_ne!(
            first, second,
            "concurrent builders must not share a temp file"
        );
        assert_eq!(first.parent(), final_path.parent());
        assert_eq!(second.parent(), final_path.parent());
    }

    #[test]
    fn concurrent_firecracker_builds_use_unique_checkout_paths() {
        let first = firecracker_build_dir(
            "nested",
            "deadbeef0000",
            uuid::Uuid::from_u128(0x11111111111111111111111111111111),
        );
        let second = firecracker_build_dir(
            "nested",
            "deadbeef0000",
            uuid::Uuid::from_u128(0x22222222222222222222222222222222),
        );
        assert_ne!(
            first, second,
            "builders with different assets-dir locks must not share a checkout"
        );
    }

    /// Create a stand-in binary at the exact current-build cache path.
    fn resolution_fixture(dir: &Path, commit: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let bin = profile_firecracker_path_for_build(
            dir,
            "default",
            REPO,
            BRANCH,
            commit,
            std::env::consts::ARCH,
            &libc_version_tag(),
        );
        std::fs::write(&bin, b"fake firecracker").unwrap();
        bin
    }

    #[test]
    fn resolution_cache_hit_returns_recorded_path() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let bin = resolution_fixture(dir, PINNED_COMMIT);

        // Nothing recorded yet -> miss (caller pays the ls-remote).
        assert!(
            fresh_cached_firecracker_resolution_in(dir, "default", REPO, BRANCH, TTL).is_none()
        );

        record_firecracker_resolution_in(dir, "default", REPO, BRANCH, PINNED_COMMIT, &bin);
        assert_eq!(
            fresh_cached_firecracker_resolution_in(dir, "default", REPO, BRANCH, TTL),
            Some(bin)
        );
    }

    #[test]
    fn resolution_cache_rejects_truncated_commit_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let bin = resolution_fixture(dir, "27305f49ab3a");
        record_firecracker_resolution_in(dir, "default", REPO, BRANCH, "27305f49ab3a", &bin);

        assert!(
            fresh_cached_firecracker_resolution_in(dir, "default", REPO, BRANCH, TTL).is_none(),
            "a cache entry without the full source identity must be refreshed"
        );
    }

    #[test]
    fn resolution_cache_rejects_other_build_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let arm_path = profile_firecracker_path_for_build(
            dir,
            "default",
            REPO,
            BRANCH,
            PINNED_COMMIT,
            "aarch64",
            "glibc-2.39",
        );
        std::fs::write(&arm_path, b"arm firecracker").unwrap();
        record_firecracker_resolution_in(dir, "default", REPO, BRANCH, PINNED_COMMIT, &arm_path);

        assert!(
            fresh_cached_firecracker_resolution_in_for_build(
                dir,
                "default",
                REPO,
                BRANCH,
                TTL,
                "x86_64",
                "glibc-2.39",
            )
            .is_none(),
            "a manifest path for another architecture must not bypass the cache key"
        );
        assert!(
            fresh_cached_firecracker_resolution_in_for_build(
                dir,
                "default",
                REPO,
                BRANCH,
                TTL,
                "aarch64",
                "musl-1.2.5",
            )
            .is_none(),
            "a manifest path for another libc must not bypass the cache key"
        );
    }

    #[test]
    fn resolution_cache_keeps_independent_build_identities() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let arm_path = profile_firecracker_path_for_build(
            dir,
            "default",
            REPO,
            BRANCH,
            PINNED_COMMIT,
            "aarch64",
            "glibc-2.39",
        );
        let x86_path = profile_firecracker_path_for_build(
            dir,
            "default",
            REPO,
            BRANCH,
            PINNED_COMMIT,
            "x86_64",
            "glibc-2.39",
        );
        std::fs::write(&arm_path, b"arm firecracker").unwrap();
        std::fs::write(&x86_path, b"x86 firecracker").unwrap();

        record_firecracker_resolution_in_for_build(
            dir,
            "default",
            REPO,
            BRANCH,
            PINNED_COMMIT,
            &arm_path,
            FirecrackerBuildEnvironment {
                arch: "aarch64",
                libc_tag: "glibc-2.39",
            },
        );
        record_firecracker_resolution_in_for_build(
            dir,
            "default",
            REPO,
            BRANCH,
            PINNED_COMMIT,
            &x86_path,
            FirecrackerBuildEnvironment {
                arch: "x86_64",
                libc_tag: "glibc-2.39",
            },
        );

        assert_eq!(
            fresh_cached_firecracker_resolution_in_for_build(
                dir,
                "default",
                REPO,
                BRANCH,
                TTL,
                "aarch64",
                "glibc-2.39",
            ),
            Some(arm_path),
            "publishing x86 must not evict the ARM resolution manifest"
        );
        assert_eq!(
            fresh_cached_firecracker_resolution_in_for_build(
                dir,
                "default",
                REPO,
                BRANCH,
                TTL,
                "x86_64",
                "glibc-2.39",
            ),
            Some(x86_path),
            "both exact binaries must remain available to offline launches"
        );
    }

    #[test]
    fn offline_fallback_rejects_unidentified_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let unidentified = dir.join("firecracker-default-unidentified.bin");
        std::fs::write(&unidentified, b"unidentified firecracker").unwrap();

        assert!(
            offline_cached_firecracker_resolution_in(
                dir,
                "default",
                REPO,
                BRANCH,
                "aarch64",
                "glibc-2.39",
            )
            .unwrap()
            .is_none(),
            "offline launch must not select an arbitrary same-profile binary: {}",
            unidentified.display()
        );
    }

    #[test]
    fn offline_fallback_reuses_only_exact_stale_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let binary = resolution_fixture(dir, PINNED_COMMIT);
        record_firecracker_resolution_in(dir, "default", REPO, BRANCH, PINNED_COMMIT, &binary);
        let manifest = firecracker_resolution_path(dir, "default", REPO, BRANCH);
        let mut entry: FirecrackerResolution =
            serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        entry.resolved_at_secs = 1;
        std::fs::write(&manifest, serde_json::to_vec_pretty(&entry).unwrap()).unwrap();

        assert_eq!(
            offline_cached_firecracker_resolution_in(
                dir,
                "default",
                REPO,
                BRANCH,
                std::env::consts::ARCH,
                &libc_version_tag(),
            )
            .unwrap(),
            Some(binary)
        );
    }

    #[tokio::test]
    async fn pinned_lookup_does_not_refresh_remote_resolution_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let profile = KernelProfile {
            firecracker_repo: Some(REPO.to_string()),
            firecracker_branch: Some(BRANCH.to_string()),
            firecracker_commit: Some(PINNED_COMMIT.to_string()),
            ..Default::default()
        };
        let binary = profile_firecracker_path_for_build(
            dir,
            "default",
            REPO,
            BRANCH,
            PINNED_COMMIT,
            std::env::consts::ARCH,
            &libc_version_tag(),
        );
        std::fs::write(&binary, b"pinned firecracker").unwrap();
        record_firecracker_resolution_in(dir, "default", REPO, BRANCH, PINNED_COMMIT, &binary);
        let manifest = firecracker_resolution_path(dir, "default", REPO, BRANCH);
        let mut entry: FirecrackerResolution =
            serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        entry.resolved_at_secs = 1;
        std::fs::write(&manifest, serde_json::to_vec_pretty(&entry).unwrap()).unwrap();

        assert_eq!(
            get_profile_firecracker_path_in(&profile, "default", dir)
                .await
                .unwrap(),
            Some(binary)
        );
        let after: FirecrackerResolution =
            serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        assert_eq!(
            after.resolved_at_secs, 1,
            "offline pinned lookup must not impersonate a new remote resolution"
        );
    }

    #[test]
    fn resolution_cache_expires_with_ttl() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let bin = resolution_fixture(dir, PINNED_COMMIT);
        record_firecracker_resolution_in(dir, "default", REPO, BRANCH, PINNED_COMMIT, &bin);

        // Age the entry past any plausible TTL by rewriting its timestamp.
        let path = firecracker_resolution_path(dir, "default", REPO, BRANCH);
        let mut entry: FirecrackerResolution =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        entry.resolved_at_secs -= 7200;
        std::fs::write(&path, serde_json::to_vec(&entry).unwrap()).unwrap();

        assert!(
            fresh_cached_firecracker_resolution_in(dir, "default", REPO, BRANCH, TTL).is_none(),
            "an entry older than the TTL must not be reused"
        );
        // A longer TTL still accepts it.
        assert!(
            fresh_cached_firecracker_resolution_in(dir, "default", REPO, BRANCH, 86_400).is_some()
        );
    }

    #[test]
    fn resolution_cache_ttl_zero_forces_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let bin = resolution_fixture(dir, PINNED_COMMIT);
        record_firecracker_resolution_in(dir, "default", REPO, BRANCH, PINNED_COMMIT, &bin);

        assert!(
            fresh_cached_firecracker_resolution_in(dir, "default", REPO, BRANCH, 0).is_none(),
            "FCVM_FIRECRACKER_RESOLVE_TTL_SECS=0 must always re-query the remote"
        );
    }

    #[test]
    fn resolution_cache_invalidated_by_repo_or_branch_change() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let bin = resolution_fixture(dir, PINNED_COMMIT);
        record_firecracker_resolution_in(dir, "default", REPO, BRANCH, PINNED_COMMIT, &bin);

        assert!(
            fresh_cached_firecracker_resolution_in(dir, "default", "other/fork", BRANCH, TTL)
                .is_none()
        );
        assert!(
            fresh_cached_firecracker_resolution_in(dir, "default", REPO, "main", TTL).is_none()
        );
        // Different profile name = different cache file.
        assert!(fresh_cached_firecracker_resolution_in(dir, "nested", REPO, BRANCH, TTL).is_none());
    }

    #[test]
    fn resolution_cache_ignores_missing_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let bin = resolution_fixture(dir, PINNED_COMMIT);
        record_firecracker_resolution_in(dir, "default", REPO, BRANCH, PINNED_COMMIT, &bin);

        std::fs::remove_file(&bin).unwrap();
        assert!(
            fresh_cached_firecracker_resolution_in(dir, "default", REPO, BRANCH, TTL).is_none(),
            "a resolution pointing at a deleted binary must not be reused"
        );
    }

    #[test]
    fn resolution_cache_ignores_corrupt_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            firecracker_resolution_path(dir, "default", REPO, BRANCH),
            b"{not json",
        )
        .unwrap();

        assert!(
            fresh_cached_firecracker_resolution_in(dir, "default", REPO, BRANCH, TTL).is_none(),
            "a corrupt cache must degrade to a remote resolution, not fail the launch"
        );
    }

    #[test]
    fn setup_style_rebuild_overwrites_the_resolution() {
        // Models `fcvm setup` after a fork rebuild: ensure_profile_firecracker
        // always re-resolves and rewrites the cache, so a launcher picks up the
        // NEW binary immediately instead of waiting out the TTL.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let old = resolution_fixture(dir, PINNED_COMMIT);
        record_firecracker_resolution_in(dir, "default", REPO, BRANCH, PINNED_COMMIT, &old);
        assert_eq!(
            fresh_cached_firecracker_resolution_in(dir, "default", REPO, BRANCH, TTL),
            Some(old.clone())
        );

        let new = resolution_fixture(dir, MOVED_COMMIT);
        record_firecracker_resolution_in(dir, "default", REPO, BRANCH, MOVED_COMMIT, &new);
        assert_eq!(
            fresh_cached_firecracker_resolution_in(dir, "default", REPO, BRANCH, TTL),
            Some(new),
            "setup must not leave launches pinned to the pre-rebuild binary"
        );
    }

    #[test]
    fn resolve_ttl_env_parsing() {
        // Pure parser (no std::env mutation — that would race the other tests in
        // this process, and env writes are not thread-safe).
        assert_eq!(
            parse_resolve_ttl(None),
            FIRECRACKER_RESOLVE_TTL_DEFAULT_SECS
        );
        assert_eq!(parse_resolve_ttl(Some("0")), 0);
        assert_eq!(parse_resolve_ttl(Some("42")), 42);
        assert_eq!(parse_resolve_ttl(Some(" 90 ")), 90);
        assert_eq!(
            parse_resolve_ttl(Some("not-a-number")),
            FIRECRACKER_RESOLVE_TTL_DEFAULT_SECS
        );
        assert_eq!(
            parse_resolve_ttl(Some("")),
            FIRECRACKER_RESOLVE_TTL_DEFAULT_SECS
        );
    }

    // resolve_cargo: `sudo fcvm setup` gets root's PATH, which has no cargo,
    // and the old bare Command::new("cargo") died with ENOENT there (the
    // Build Btrfs Kernel job). These pin the fallback ladder.

    fn fake_cargo_in(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let cargo = dir.join("cargo");
        write_executable(&cargo, "#!/bin/sh\nexit 0\n");
        cargo
    }

    #[test]
    fn resolve_cargo_prefers_cargo_env() {
        let tmp = tempfile::tempdir().unwrap();
        let env_cargo = fake_cargo_in(&tmp.path().join("env-bin"));
        let path_cargo_dir = tmp.path().join("path-bin");
        fake_cargo_in(&path_cargo_dir);
        let found = resolve_cargo(
            Some(env_cargo.clone().into_os_string()),
            Some(path_cargo_dir.into_os_string()),
            &[],
        );
        assert_eq!(found, Some(env_cargo));
    }

    #[test]
    fn resolve_cargo_ignores_dangling_cargo_env() {
        let tmp = tempfile::tempdir().unwrap();
        let path_dir = tmp.path().join("path-bin");
        let path_cargo = fake_cargo_in(&path_dir);
        let found = resolve_cargo(
            Some(tmp.path().join("missing/cargo").into_os_string()),
            Some(path_dir.into_os_string()),
            &[],
        );
        assert_eq!(found, Some(path_cargo));
    }

    #[test]
    fn resolve_cargo_scans_path_entries_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let without = tmp.path().join("without-cargo");
        std::fs::create_dir_all(&without).unwrap();
        let with = tmp.path().join("with-cargo");
        let path_cargo = fake_cargo_in(&with);
        let joined = std::env::join_paths([without, with]).unwrap();
        let found = resolve_cargo(None, Some(joined), &[]);
        assert_eq!(found, Some(path_cargo));
    }

    #[test]
    fn resolve_cargo_falls_back_to_home_rustup_install() {
        // The sudo case: PATH carries no cargo, but the invoking user's home
        // has the standard rustup layout. The old code had no fallback at all.
        let tmp = tempfile::tempdir().unwrap();
        let empty_path_dir = tmp.path().join("no-cargo-here");
        std::fs::create_dir_all(&empty_path_dir).unwrap();
        let home = tmp.path().join("home");
        let home_cargo = fake_cargo_in(&home.join(".cargo/bin"));
        let found = resolve_cargo(None, Some(empty_path_dir.into_os_string()), &[home]);
        assert_eq!(found, Some(home_cargo));
    }

    #[test]
    fn resolve_cargo_skips_non_executable_candidates() {
        // execvp skips a PATH entry it cannot execute and keeps searching;
        // resolution must do the same or a stray non-executable "cargo"
        // file turns into a hard EACCES at spawn time.
        let tmp = tempfile::tempdir().unwrap();
        let broken_dir = tmp.path().join("broken");
        std::fs::create_dir_all(&broken_dir).unwrap();
        std::fs::write(broken_dir.join("cargo"), "not a program").unwrap();
        let good_dir = tmp.path().join("good");
        let good = fake_cargo_in(&good_dir);
        let joined = std::env::join_paths([broken_dir, good_dir]).unwrap();
        let found = resolve_cargo(None, Some(joined), &[]);
        assert_eq!(found, Some(good));
    }

    #[test]
    fn resolve_cargo_returns_absolute_paths_for_relative_candidates() {
        // The build command sets current_dir(build_dir), so a relative
        // candidate validated against the caller's cwd would be re-resolved
        // by the child inside the cloned repo — a different file entirely.
        let tmp = tempfile::tempdir().unwrap();
        fake_cargo_in(&tmp.path().join("bin"));
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let found = resolve_cargo(None, Some(std::ffi::OsString::from("bin")), &[]);
        std::env::set_current_dir(prev).unwrap();
        let found = found.expect("relative PATH entry with a real cargo resolves");
        assert!(
            found.is_absolute(),
            "resolved cargo must be absolute, got {}",
            found.display()
        );
    }

    #[test]
    fn fallback_homes_prefer_the_identity_that_executes() {
        // With a sudo invoker the child drops to that user, who typically
        // cannot traverse root's 0700 $HOME — root's cargo would spawn-fail
        // EACCES for the child, so it must not even be a candidate.
        let invoker = PathBuf::from("/home/operator");
        let root_home = PathBuf::from("/root");
        assert_eq!(
            fallback_homes_for(Some(invoker.clone()), Some(root_home.clone())),
            vec![invoker]
        );
        assert_eq!(
            fallback_homes_for(None, Some(root_home.clone())),
            vec![root_home]
        );
        assert!(fallback_homes_for(None, None).is_empty());
    }

    #[test]
    fn resolve_cargo_reports_nothing_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let bare_home = tmp.path().join("bare-home");
        std::fs::create_dir_all(&bare_home).unwrap();
        let found = resolve_cargo(None, Some(empty.into_os_string()), &[bare_home]);
        assert_eq!(found, None);
    }
}
