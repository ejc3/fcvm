//! Build pasta (passt) from a pinned upstream commit.
//!
//! The pin contains the upstream addr_seen fix for issue #661. fcvm builds
//! pasta on demand into the content-addressed shared assets directory, so
//! rootless networking does not depend on the host's distro pasta version.
//!
//! Build and publication rules:
//! - The upstream ref is a PINNED COMMIT, not a branch: the binary path is
//!   computable offline (no ls-remote, no network-failure fallback paths).
//! - Installs are atomic (temp file + rename) and serialized by an exclusive
//!   flock, double-checked after acquisition.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tokio::process::Command;
use tracing::{debug, info};

use crate::paths;
use crate::setup::rootfs::PastaConfig;

/// Content hash for the pasta binary: upstream repo + pinned commit +
/// the host libc (dynamically linked binaries must not be
/// shared across incompatible C libraries — same rule as firecracker).
fn compute_pasta_sha(config: &PastaConfig) -> String {
    let mut hasher = Sha256::new();
    hasher.update(config.repo.as_bytes());
    hasher.update(config.commit.as_bytes());
    hasher.update(super::kernel::libc_version_tag().as_bytes());
    let result = hasher.finalize();
    hex::encode(&result[..6])
}

/// Content-addressed path for the pasta binary. Pure computation — works
/// offline because the upstream ref is a pinned commit.
pub fn pasta_bin_path(config: &PastaConfig) -> PathBuf {
    let sha = compute_pasta_sha(config);
    paths::assets_dir()
        .join("pasta")
        .join(format!("pasta-{}.bin", sha))
}

/// Resolve the pasta binary to run.
///
/// With a `[pasta]` config section, the content-addressed build is REQUIRED:
/// a missing binary is an error pointing at `fcvm setup`, not a silent
/// fallback to a distro pasta without the upstream fix.
/// Without the section, the system pasta from PATH is used unchanged.
pub fn get_pasta_for_config(config: Option<&PastaConfig>) -> Result<PathBuf> {
    match config {
        Some(cfg) => {
            let path = pasta_bin_path(cfg);
            if !path.exists() {
                bail!(
                    "pinned pasta not found at {}. Run 'fcvm setup' (or 'make setup-fcvm') \
                     to build it from {} @ {}",
                    path.display(),
                    cfg.repo,
                    &cfg.commit[..cfg.commit.len().min(12)]
                );
            }
            Ok(path)
        }
        None => which::which("pasta").context("pasta not found in PATH"),
    }
}

/// Ensure the pinned pasta binary exists, building it if needed.
/// Returns `Ok(None)` when no `[pasta]` section is configured.
pub async fn ensure_pasta(config: Option<&PastaConfig>) -> Result<Option<PathBuf>> {
    let config = match config {
        Some(c) => c,
        None => return Ok(None),
    };

    let bin_path = pasta_bin_path(config);
    if bin_path.exists() {
        info!(path = %bin_path.display(), "pasta binary exists");
        return Ok(Some(bin_path));
    }

    // Serialize concurrent builds of the same binary (parallel `fcvm setup`).
    let flock = super::lock_store_dir(&bin_path.with_extension("lock"), "pasta build").await?;

    // Another process may have finished the build while we waited.
    if bin_path.exists() {
        debug!(path = %bin_path.display(), "pasta exists (built by another process)");
        flock.unlock().map_err(|(_, err)| err)?;
        return Ok(Some(bin_path));
    }

    let sha = compute_pasta_sha(config);
    println!(
        "  → Building pasta from {} @ {} (sha: {})...",
        config.repo,
        &config.commit[..config.commit.len().min(12)],
        sha
    );

    let build_dir = PathBuf::from(format!("/tmp/pasta-build-{}", sha));
    if build_dir.exists() {
        tokio::fs::remove_dir_all(&build_dir)
            .await
            .context("removing old pasta build directory")?;
    }

    if let Err(e) = build_pasta(config, &build_dir, &bin_path).await {
        // Failures keep the build tree for debugging; never leave a partial
        // binary behind (build_pasta installs atomically).
        let _ = flock.unlock();
        return Err(e);
    }

    let _ = tokio::fs::remove_dir_all(&build_dir).await;
    flock.unlock().map_err(|(_, err)| err)?;
    println!("  ✓ pasta ready: {}", bin_path.display());
    Ok(Some(bin_path))
}

async fn build_pasta(
    config: &PastaConfig,
    build_dir: &std::path::Path,
    bin_path: &std::path::Path,
) -> Result<()> {
    // Full clone then checkout of the pinned commit: shallow fetches of an
    // arbitrary SHA need uploadpack.allowReachableSHA1InWant on the server,
    // which passt.top does not advertise. The repo is ~15MB; correctness
    // over cleverness.
    // The whole build runs as the sudo invoker (see run_build_as_sudo_invoker):
    // the deterministic /tmp/pasta-build-{sha} tree must never be left
    // root-owned by a failed root build, or the next rootless build dies
    // removing it.
    let status = super::run_build_as_sudo_invoker(Command::new("git").args([
        "clone",
        &config.repo,
        build_dir.to_str().unwrap(),
    ]))
    .status()
    .await
    .context("cloning pasta repo")?;
    if !status.success() {
        bail!("failed to clone pasta repo from {}", config.repo);
    }

    let status = super::run_build_as_sudo_invoker(
        Command::new("git")
            .args(["checkout", "--detach", &config.commit])
            .current_dir(build_dir),
    )
    .status()
    .await
    .context("checking out pinned pasta commit")?;
    if !status.success() {
        bail!(
            "pinned pasta commit {} not found in {} — the pin and repo must agree",
            config.commit,
            config.repo
        );
    }

    let status =
        super::run_build_as_sudo_invoker(Command::new("make").arg("pasta").current_dir(build_dir))
            .status()
            .await
            .context("building pasta (is a C toolchain installed? apt install gcc make)")?;
    if !status.success() {
        bail!(
            "pasta build failed (build tree kept at {})",
            build_dir.display()
        );
    }

    let built = build_dir.join("pasta");
    if !built.exists() {
        bail!("pasta binary not found at {} after build", built.display());
    }

    // Atomic install: a killed copy must never leave a partial binary that
    // later runs treat as valid.
    let temp_path = bin_path.with_extension("tmp");
    let _ = tokio::fs::remove_file(&temp_path).await;
    tokio::fs::copy(&built, &temp_path)
        .await
        .context("staging pasta binary")?;
    super::publish_store_entry(&temp_path, bin_path, "pasta binary").await?;
    Ok(())
}
