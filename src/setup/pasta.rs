//! Build pasta (passt) from a pinned upstream commit plus one carried patch.
//!
//! The pin contains the upstream addr_seen fix for issue #661, and the fix
//! that keeps `-a`, `-g` and `-n` on a host without IPv4. On top of it fcvm
//! carries one patch (see `PATCHES`) that guards `udp_sock_errs()` against a
//! NULL flow, so a published UDP port's listening socket cannot crash pasta on
//! a socket error; it is submitted upstream and dropped once the pin moves past
//! the merge. fcvm builds pasta on demand into the content-addressed shared
//! assets directory, so rootless networking does not depend on the host's
//! distro pasta version.
//!
//! Build and publication rules:
//! - The upstream ref is a PINNED COMMIT, not a branch: the binary path is
//!   computable offline (no ls-remote, no network-failure fallback paths).
//! - Patches are EMBEDDED in the fcvm binary (include_str!), not read from the
//!   repo checkout: builds work from any working directory, and editing a patch
//!   automatically produces a new content hash (and thus a rebuild).
//! - Installs are atomic (temp file + rename) and serialized by an exclusive
//!   flock, double-checked after acquisition. The temp file's name is unique to
//!   the builder, because that flock does not hold between VMs that share the
//!   store over FUSE.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tracing::{debug, info};

use crate::paths;
use crate::setup::rootfs::PastaConfig;

/// Patches applied on top of the pinned upstream commit, in order.
/// Embedded so the build is independent of the working directory and the
/// content hash tracks patch edits automatically. The same file is applied by
/// `scripts/build-passt.sh` (CI, AMI, runners), and both references are pinned
/// together by `tests/test_pasta_pin.rs`.
const PATCHES: &[(&str, &str)] = &[(
    "passt-udp-sock-errs-null-flow.patch",
    include_str!("../../scripts/passt-udp-sock-errs-null-flow.patch"),
)];

/// Content hash for the pasta binary: upstream repo + pinned commit + every
/// carried patch + the host libc (dynamically linked binaries must not be
/// shared across incompatible C libraries — same rule as firecracker).
fn compute_pasta_sha(config: &PastaConfig) -> String {
    let mut hasher = Sha256::new();
    hasher.update(config.repo.as_bytes());
    hasher.update(config.commit.as_bytes());
    for (name, content) in PATCHES {
        hasher.update(name.as_bytes());
        hasher.update(content.as_bytes());
    }
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
/// fallback to a distro pasta without the upstream fix and the carried patch.
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
        "  → Building pasta from {} @ {} ({} patch(es), sha: {})...",
        config.repo,
        &config.commit[..config.commit.len().min(12)],
        PATCHES.len(),
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

async fn build_pasta(config: &PastaConfig, build_dir: &Path, bin_path: &Path) -> Result<()> {
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

    for (name, content) in PATCHES {
        let patch_path = build_dir.join(name);
        tokio::fs::write(&patch_path, content)
            .await
            .with_context(|| format!("writing carried patch {}", name))?;
        let status = super::run_build_as_sudo_invoker(
            Command::new("git")
                .args(["apply", "--verbose", name])
                .current_dir(build_dir),
        )
        .status()
        .await
        .with_context(|| format!("applying pasta patch {}", name))?;
        if !status.success() {
            bail!(
                "pasta patch {} does not apply to {} @ {} — rebase the patch or move the pin",
                name,
                config.repo,
                &config.commit[..config.commit.len().min(12)]
            );
        }
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
    let staged = stage_pasta_binary(&built, bin_path).await?;
    if let Err(e) = super::publish_store_entry(&staged, bin_path, "pasta binary").await {
        let _ = tokio::fs::remove_file(&staged).await;
        return Err(e);
    }
    Ok(())
}

/// Where one builder stages a binary inside the store before renaming it onto
/// `bin_path`. The name is the builder's own. The build lock is an flock, and on
/// a store that several VMs share over FUSE each guest kernel grants it by
/// itself, so it does not keep two VMs from building the same binary at once.
/// The nonce is a uuid and not a PID: separate PID namespaces reuse numbers.
fn pasta_install_temp_path(bin_path: &Path, nonce: uuid::Uuid) -> PathBuf {
    let name = bin_path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "pasta".into());
    bin_path.with_file_name(format!(".{name}.{nonce}.tmp"))
}

/// Copy a built binary into the store beside its final path, ready to be renamed
/// onto it.
async fn stage_pasta_binary(built: &Path, bin_path: &Path) -> Result<PathBuf> {
    let temp_path = pasta_install_temp_path(bin_path, uuid::Uuid::new_v4());
    if let Err(e) = tokio::fs::copy(built, &temp_path).await {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(e).context("staging pasta binary");
    }
    Ok(temp_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The store can be a directory several VMs share over FUSE. The build lock
    /// is an flock, which each guest kernel grants on its own there, so two VMs
    /// can build the same binary at once. A staging name fixed per binary is
    /// then one file both builders write: the first to publish renames a file
    /// the second is still writing onto the final path.
    #[tokio::test]
    async fn two_builders_of_one_binary_stage_to_separate_files() {
        let store = tempfile::tempdir().unwrap();
        let bin_path = store.path().join("pasta-0123456789ab.bin");
        let first_built = store.path().join("first-build-tree-pasta");
        let second_built = store.path().join("second-build-tree-pasta");
        std::fs::write(&first_built, b"first builder").unwrap();
        std::fs::write(&second_built, b"second builder").unwrap();

        // The second builder stages after the first has staged and before the
        // first has published.
        let first = stage_pasta_binary(&first_built, &bin_path).await.unwrap();
        let second = stage_pasta_binary(&second_built, &bin_path).await.unwrap();
        crate::setup::publish_store_entry(&first, &bin_path, "pasta binary")
            .await
            .unwrap();

        assert_eq!(
            std::fs::read(&bin_path).unwrap(),
            b"first builder",
            "the first builder published the file the second builder was writing"
        );
        assert_eq!(std::fs::read(&second).unwrap(), b"second builder");
        assert_eq!(first.parent(), bin_path.parent());
        assert_eq!(second.parent(), bin_path.parent());
    }
}
