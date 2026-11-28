//! Published builds of the pinned Firecracker and Cloud Hypervisor.
//!
//! A fresh host builds every pinned VMM from source: in CI that was 169 to 190 s of Firecracker
//! builds and 92 to 98 s of Cloud Hypervisor per job (runs 45967670147 and 45983151902, every
//! job on an empty store). `vmm.yml` builds each pin once per architecture and libc on
//! GitHub-hosted runners (no KVM is needed, and a job that publishes shares no instance with
//! pull request jobs, #2223), publishes each binary as a release asset named for the store
//! file it is, and pushes the digests to a branch for `rootfs-config.toml`'s `[vmm_artifacts]`.
//!
//! Setup installs a recorded build whose name is the one it would build. The name is the
//! content address the local build uses (repo, ref, commit, architecture, libc tag, and for
//! Firecracker the libseccomp pin), so a build from another libc or another pin is never
//! picked: that host does not find its name and builds locally. The bytes are checked against
//! the recorded SHA-256 before they are installed; a release asset replaced under the same
//! name is refused, and setup builds locally.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// `[vmm_artifacts]` in rootfs-config.toml.
#[derive(Debug, Deserialize, Serialize, Clone, Default, PartialEq)]
pub struct VmmArtifacts {
    /// The repository whose releases hold the builds, e.g. "ejc3/fcvm".
    #[serde(default)]
    pub repo: String,
    /// One entry per published binary.
    #[serde(default)]
    pub files: Vec<VmmArtifact>,
}

/// One published VMM binary.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub struct VmmArtifact {
    /// The store file name it installs as, e.g. "firecracker-default-c53ec758b0b8.bin".
    pub name: String,
    /// The commit it was built from.
    pub commit: String,
    /// The architecture it was built for (`std::env::consts::ARCH`).
    pub arch: String,
    /// The libc tag of the build (`kernel::libc_version_tag`).
    pub libc: String,
    /// SHA-256 of the file, 64 lowercase hex digits.
    pub sha256: String,
}

/// The architectures `vmm.yml` publishes builds for (`std::env::consts::ARCH`).
pub const PUBLISHED_ARCHES: &[&str] = &["x86_64", "aarch64"];

/// The libc tags `vmm.yml` publishes builds for, each with what decides it: its host legs run
/// on GitHub's Ubuntu 24.04 runners, whose glibc the self-hosted Host and Host-Root lanes share,
/// and its container legs in the test image built from `Containerfile` on Debian bookworm, the
/// Container lanes' image. A host with any other libc finds no build named as it names its own,
/// and setup builds that VMM locally.
pub const PUBLISHED_LIBCS: &[(&str, &str)] =
    &[("ubuntu-24.04", "libc-2.39"), ("bookworm", "libc-2.36")];

impl VmmArtifacts {
    /// The recorded build installed as `name`.
    pub fn find(&self, name: &str) -> Option<&VmmArtifact> {
        self.files.iter().find(|f| f.name == name)
    }

    /// Where `artifact` is downloaded from.
    pub fn url(&self, artifact: &VmmArtifact) -> String {
        format!(
            "https://github.com/{}/releases/download/{}/{}",
            self.repo,
            release_tag(&artifact.name),
            artifact.name
        )
    }
}

/// The release a build is published under: `vmm-` and its file name without `.bin`.
pub fn release_tag(name: &str) -> String {
    format!("vmm-{}", name.trim_end_matches(".bin"))
}

fn check_digest(sha256: &str) -> Result<()> {
    if sha256.len() == 64
        && sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Ok(());
    }
    bail!("[vmm_artifacts] digest '{sha256}' is not 64 lowercase hex digits")
}

/// Install the recorded build named `dest`'s file name at `dest`. `Ok(false)` when none is
/// recorded; an error when one is recorded but does not arrive with its digest, or `check`
/// refuses it. The caller builds locally in both cases.
pub async fn install(
    artifacts: &VmmArtifacts,
    dest: &Path,
    retry_delay: std::time::Duration,
    check: impl FnOnce(&Path) -> Result<()>,
) -> Result<bool> {
    let name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .context("a VMM store path has no file name")?;
    let Some(artifact) = artifacts.find(name) else {
        return Ok(false);
    };
    if artifacts.repo.is_empty() {
        bail!("[vmm_artifacts] records {name} but names no repo");
    }
    check_digest(&artifact.sha256)?;
    let temp = super::StoreTemp::new(dest)?;
    install_into(
        temp,
        &artifacts.url(artifact),
        &artifact.sha256,
        dest,
        retry_delay,
        check,
    )
    .await?;
    Ok(true)
}

/// Download `url` into `temp`, refuse it unless its SHA-256 is `sha256` and `check` accepts
/// it, make it executable and publish it at `dest`. The tests make `temp` with its owner
/// recorded elsewhere.
pub(crate) async fn install_into<C>(
    temp: super::StoreTemp,
    url: &str,
    sha256: &str,
    dest: &Path,
    retry_delay: std::time::Duration,
    check: C,
) -> Result<()>
where
    C: FnOnce(&Path) -> Result<()>,
{
    use std::os::unix::fs::PermissionsExt;
    check_digest(sha256)?;
    super::fetch_verified_archive(
        "published VMM build",
        &[url.to_string()],
        sha256,
        temp.path(),
        retry_delay,
    )
    .await?;
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("making {} executable", temp.path().display()))?;
    check(temp.path())?;
    temp.publish(dest, "published VMM build").await
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn artifacts(repo: &str, name: &str, sha256: &str) -> VmmArtifacts {
        VmmArtifacts {
            repo: repo.to_string(),
            files: vec![VmmArtifact {
                name: name.to_string(),
                commit: "0".repeat(40),
                arch: std::env::consts::ARCH.to_string(),
                libc: "libc-2.39".to_string(),
                sha256: sha256.to_string(),
            }],
        }
    }

    #[test]
    fn a_build_is_found_by_its_store_name_and_downloaded_from_its_release() {
        let a = artifacts(
            "ejc3/fcvm",
            "firecracker-default-0123456789ab.bin",
            &"a".repeat(64),
        );
        assert!(a.find("firecracker-default-ffffffffffff.bin").is_none());
        let found = a.find("firecracker-default-0123456789ab.bin").unwrap();
        assert_eq!(
            a.url(found),
            "https://github.com/ejc3/fcvm/releases/download/vmm-firecracker-default-0123456789ab/firecracker-default-0123456789ab.bin"
        );
    }

    /// A published build is installed only as the bytes its entry records, and only when the
    /// caller's check accepts it; anything else leaves nothing at the store path.
    #[tokio::test]
    async fn a_published_build_is_installed_only_with_its_recorded_digest() {
        let dir = tempfile::tempdir().unwrap();
        let records = tempfile::tempdir().unwrap();
        let owners = crate::reclaim::owner_dir_in(records.path()).unwrap();
        let built = b"#!/bin/sh\necho firecracker\n".to_vec();
        let recorded = hex::encode(Sha256::digest(&built));
        let asset = dir.path().join("asset.bin");
        let url = format!("file://{}", asset.display());
        let dest = dir.path().join("firecracker-default-0123456789ab.bin");
        let no_wait = std::time::Duration::ZERO;

        std::fs::write(&asset, b"#!/bin/sh\necho something else\n").unwrap();
        let err = install_into(
            super::super::StoreTemp::new_in(&owners, &dest).unwrap(),
            &url,
            &recorded,
            &dest,
            no_wait,
            |_| Ok(()),
        )
        .await
        .expect_err("bytes that are not the recorded ones were installed");
        assert!(format!("{err:#}").contains(&recorded), "{err:#}");
        assert!(!dest.exists());

        std::fs::write(&asset, &built).unwrap();
        let err = install_into(
            super::super::StoreTemp::new_in(&owners, &dest).unwrap(),
            &url,
            &recorded,
            &dest,
            no_wait,
            |_| bail!("the version does not name the commit"),
        )
        .await
        .expect_err("a build the check refused was installed");
        assert!(
            format!("{err:#}").contains("does not name the commit"),
            "{err:#}"
        );
        assert!(!dest.exists());

        install_into(
            super::super::StoreTemp::new_in(&owners, &dest).unwrap(),
            &url,
            &recorded,
            &dest,
            no_wait,
            |_| Ok(()),
        )
        .await
        .expect("the recorded bytes install");
        assert_eq!(std::fs::read(&dest).unwrap(), built);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&dest).unwrap().permissions().mode() & 0o111,
            0o111
        );

        // no entry for the name: nothing is downloaded
        let other = dir.path().join("firecracker-default-ffffffffffff.bin");
        let none = artifacts(
            "ejc3/fcvm",
            "firecracker-default-0123456789ab.bin",
            &recorded,
        );
        assert!(!install(&none, &other, no_wait, |_| Ok(())).await.unwrap());
        assert!(!other.exists());
    }
}
