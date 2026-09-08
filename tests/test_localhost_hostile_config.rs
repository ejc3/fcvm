//! Rootless localhost exports must use Podman's resolved store, not the host config.
//! This needs the host's subordinate-ID mappings. A nested rootless CI container
//! does not map the subordinate IDs needed by the rootless Podman child.

#![cfg(feature = "privileged-tests")]

use anyhow::{Context, Result};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

const CHILD_ROOT: &str = "FCVM_HOSTILE_STORAGE_TEST_ROOT";
const TEST_NAME: &str = "test_localhost_export_with_root_only_configured_runroot";

/// #852: skopeo does not apply Podman's rootless runroot remapping. The child
/// receives its own config and store so neither environment nor image mutations
/// can affect another test. No VM, registry image, or host config change is needed.
/// Removing export_image_archive's explicit store argument makes skopeo fail at
/// mkdir of the configured /run/fcvm-hostile-storage-* path with EACCES.
#[test]
fn test_localhost_export_with_root_only_configured_runroot() -> Result<()> {
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let result =
            tokio::runtime::Runtime::new()?.block_on(export_from_hostile_config(Path::new(&root)));
        // Only this child uses the private store/runtime. Migrate stops its
        // rootless pause process before the parent removes those directories.
        let cleanup = Command::new("podman")
            .args(["system", "migrate"])
            .output()?;
        anyhow::ensure!(
            cleanup.status.success(),
            "private store cleanup: {}",
            String::from_utf8_lossy(&cleanup.stderr)
        );
        return result;
    }

    let root = tempfile::TempDir::new()?;
    let runtime = root.path().join("runtime");
    std::fs::create_dir(&runtime)?;
    let mut child = Command::new(std::env::current_exe()?);
    child
        .args(["--exact", TEST_NAME, "--nocapture"])
        .env(CHILD_ROOT, root.path())
        .env("CONTAINERS_STORAGE_CONF", root.path().join("storage.conf"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_RUNTIME_DIR", &runtime);

    if nix::unistd::geteuid().is_root() {
        let uid = std::env::var("SUDO_UID")
            .context("root test needs the invoking sudo user's SUDO_UID")?
            .parse()
            .context("SUDO_UID must be a numeric user ID")?;
        let user = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))?
            .context("SUDO_UID must identify an existing user")?;
        anyhow::ensure!(!user.uid.is_root(), "rootless test must not run as root");
        for directory in [root.path(), runtime.as_path()] {
            std::os::unix::fs::chown(directory, Some(user.uid.as_raw()), Some(user.gid.as_raw()))?;
        }
        child
            .uid(user.uid.as_raw())
            .gid(user.gid.as_raw())
            .env("HOME", user.dir)
            .env("USER", &user.name)
            .env("LOGNAME", user.name);
    }

    let output = child
        .output()
        .context("running isolated rootless export test")?;
    anyhow::ensure!(
        output.status.success(),
        "rootless export test failed: {}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

async fn export_from_hostile_config(root: &Path) -> Result<()> {
    anyhow::ensure!(
        !nix::unistd::geteuid().is_root(),
        "test child must be rootless"
    );
    let nonce = uuid::Uuid::new_v4();
    let forbidden_runroot = format!("/run/fcvm-hostile-storage-{nonce}");
    let graphroot = root.join("storage");
    let config = format!(
        "[storage]\ndriver = \"vfs\"\ngraphroot = {:?}\nrootless_storage_path = {:?}\nrunroot = {:?}\n",
        graphroot, graphroot, forbidden_runroot
    );
    std::fs::write(root.join("storage.conf"), config)?;
    let marker = format!("hostile-storage-{nonce}");
    std::fs::write(
        root.join("Containerfile"),
        format!("FROM scratch\nCMD [\"echo\", \"{marker}\"]\n"),
    )?;
    let image = format!("localhost/hostile-storage-{nonce}:latest");
    let built = Command::new("podman")
        .args(["build", "-t", &image, "-f"])
        .arg(root.join("Containerfile"))
        .arg(root)
        .output()?;
    anyhow::ensure!(
        built.status.success(),
        "podman build: {}",
        String::from_utf8_lossy(&built.stderr)
    );

    let info = Command::new("podman")
        .args([
            "info",
            "--format",
            "{{.Store.GraphRoot}}\n{{.Store.RunRoot}}",
        ])
        .output()?;
    anyhow::ensure!(
        info.status.success(),
        "podman info: {}",
        String::from_utf8_lossy(&info.stderr)
    );
    let store = String::from_utf8(info.stdout)?;
    let (actual_graphroot, actual_runroot) = store
        .trim()
        .split_once('\n')
        .context("podman store paths")?;
    anyhow::ensure!(
        Path::new(actual_graphroot) == graphroot,
        "Podman did not use the private graphroot: {store}"
    );
    anyhow::ensure!(
        Path::new(actual_runroot).starts_with(root.join("runtime")),
        "Podman did not use the private runtime: {store}"
    );
    anyhow::ensure!(
        actual_runroot != forbidden_runroot,
        "Podman did not remap the root-only runroot"
    );
    anyhow::ensure!(
        !Path::new(&forbidden_runroot).exists(),
        "test created the forbidden runroot"
    );

    let inspected = Command::new("podman")
        .args(["image", "inspect", &image, "--format", "{{.Id}}"])
        .output()?;
    anyhow::ensure!(
        inspected.status.success(),
        "podman image inspect: {}",
        String::from_utf8_lossy(&inspected.stderr)
    );
    let image_id = String::from_utf8(inspected.stdout)?.trim().to_owned();
    let archive = root.join("image.tar");
    let result = async {
        fcvm::commands::podman::export_image_archive(&image_id, &image, &archive).await?;
        let inspected = Command::new("skopeo")
            .args(["inspect", "--config"])
            .arg(format!("docker-archive:{}", archive.display()))
            .output()?;
        anyhow::ensure!(
            inspected.status.success(),
            "skopeo inspect: {}",
            String::from_utf8_lossy(&inspected.stderr)
        );
        let config: serde_json::Value = serde_json::from_slice(&inspected.stdout)?;
        anyhow::ensure!(
            config["config"]["Cmd"] == serde_json::json!(["echo", marker]),
            "archive contains the wrong image: {config}"
        );
        Ok(())
    }
    .await;
    let removed = Command::new("podman").args(["rmi", &image_id]).output()?;
    anyhow::ensure!(
        removed.status.success(),
        "removing test image: {}",
        String::from_utf8_lossy(&removed.stderr)
    );
    result
}
