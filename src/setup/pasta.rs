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

/// Delay before the second attempt of an archive download; it doubles for the third.
const ARCHIVE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

/// Download the pinned source archive to `dest` from the first of `urls` that
/// serves bytes with `sha256`. Each URL gets three attempts, `retry_delay`
/// apart (doubling), so one dropped TLS handshake costs a retry, not the setup
/// (#1008). The checksum decides what is built, never the host it came from.
async fn fetch_verified_archive(
    urls: &[String],
    sha256: &str,
    dest: &Path,
    retry_delay: std::time::Duration,
) -> Result<()> {
    let mut failures = Vec::new();
    for url in urls {
        let mut delay = retry_delay;
        for attempt in 1..=3 {
            let download = super::run_build_as_sudo_invoker(Command::new("curl").args([
                "-fsSL",
                "--connect-timeout",
                "20",
                "--max-time",
                "600",
                "-o",
            ]))
            .arg(dest)
            .arg(url)
            .output()
            .await
            .context("running curl (is curl installed?)")?;
            let outcome = if !download.status.success() {
                format!(
                    "curl {}: {}",
                    download.status,
                    String::from_utf8_lossy(&download.stderr).trim()
                )
            } else {
                let bytes = tokio::fs::read(dest)
                    .await
                    .with_context(|| format!("reading {}", dest.display()))?;
                let got = hex::encode(Sha256::digest(&bytes));
                if got == sha256 {
                    return Ok(());
                }
                format!("sha256 {got}, want {sha256}")
            };
            debug!(url, attempt, outcome, "pasta source archive attempt failed");
            failures.push(format!("{url} (attempt {attempt}): {outcome}"));
            let _ = tokio::fs::remove_file(dest).await;
            if attempt < 3 {
                tokio::time::sleep(delay).await;
                delay *= 2;
            }
        }
    }
    bail!(
        "no source served the pinned pasta archive (sha256 {sha256}):\n  {}",
        failures.join("\n  ")
    )
}

async fn build_pasta(config: &PastaConfig, build_dir: &Path, bin_path: &Path) -> Result<()> {
    // The whole build runs as the sudo invoker (see run_build_as_sudo_invoker):
    // the deterministic /tmp/pasta-build-{sha} tree must never be left
    // root-owned by a failed root build, or the next rootless build dies
    // removing it.
    let status = super::run_build_as_sudo_invoker(Command::new("mkdir").arg("-p").arg(build_dir))
        .status()
        .await
        .context("creating the pasta build directory")?;
    if !status.success() {
        bail!("failed to create {}", build_dir.display());
    }
    let archive = build_dir.join("passt.tar.xz");
    fetch_verified_archive(
        &config.archives,
        &config.archive_sha256,
        &archive,
        ARCHIVE_RETRY_DELAY,
    )
    .await?;
    let status = super::run_build_as_sudo_invoker(
        Command::new("tar")
            .args(["-xJf", "passt.tar.xz", "--strip-components=1"])
            .current_dir(build_dir),
    )
    .status()
    .await
    .context("extracting the pasta source archive")?;
    if !status.success() {
        bail!("failed to extract {}", archive.display());
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

    let status = super::run_build_as_sudo_invoker(
        Command::new("make")
            .arg("pasta")
            // The archive has no .git, so name the version as scripts/build-passt.sh does.
            .arg(format!("VERSION={}", config.commit))
            .current_dir(build_dir),
    )
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

    /// One archive, two sources: the first does not answer (or serves other
    /// bytes) and the second serves the pinned archive. The fetch takes the
    /// second, and fails only when no source serves the pinned bytes (#1008).
    #[tokio::test]
    async fn the_pasta_archive_falls_through_to_the_next_source() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.tar.xz");
        let other = dir.path().join("other.tar.xz");
        std::fs::write(&good, b"the pinned archive").unwrap();
        std::fs::write(&other, b"some other archive").unwrap();
        let sha = hex::encode(Sha256::digest(b"the pinned archive"));
        let url = |path: &Path| format!("file://{}", path.display());
        let absent = url(&dir.path().join("absent.tar.xz"));
        let dest = dir.path().join("dest.tar.xz");
        let no_wait = std::time::Duration::ZERO;

        for first in [absent.clone(), url(&other)] {
            fetch_verified_archive(&[first.clone(), url(&good)], &sha, &dest, no_wait)
                .await
                .unwrap_or_else(|e| panic!("{first} then the pinned archive: {e:#}"));
            assert_eq!(std::fs::read(&dest).unwrap(), b"the pinned archive");
        }

        let err = fetch_verified_archive(&[absent.clone(), url(&other)], &sha, &dest, no_wait)
            .await
            .expect_err("no source served the pinned archive");
        let text = format!("{err:#}");
        assert!(
            text.contains(&absent) && text.contains("other.tar.xz"),
            "{text}"
        );
        assert!(
            !dest.exists(),
            "a rejected download was left at {}",
            dest.display()
        );
    }

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
