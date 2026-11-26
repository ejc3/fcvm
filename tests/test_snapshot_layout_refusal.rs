//! A snapshot whose state file this Firecracker build did not write is refused, never loaded.
//!
//! The Firecracker fcvm pins ends every state file with a mark of its state's layout and
//! checks the mark before it decodes anything (`rootfs-config.toml`, `[firecracker]`). A
//! file without the mark is what every earlier build wrote: upstream's, and the fork's
//! v1.17.0-based and NV2 builds, whose snapshots restore on the binary they recorded while
//! that binary exists. When it is gone the profile's binary loads the snapshot, and it must
//! say so and stop, because the bytes may decode as state of another layout.
//!
//! The file here is the hardest case: a state file this very build wrote, so every byte of
//! it decodes, with only the mark taken off.

#![cfg(feature = "integration-slow")]

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

/// Length of the layout mark that ends a state file (ejc3/firecracker, src/vmm/src/snapshot).
const LAYOUT_MARK_LEN: u64 = 16;

async fn restore(snapshot: &str, clone: &str) -> Result<std::process::Output> {
    let run = tokio::process::Command::new(common::find_fcvm_binary()?)
        .args(["snapshot", "run", "--snapshot", snapshot, "--name", clone])
        .kill_on_drop(true)
        .output();
    tokio::time::timeout(Duration::from_secs(180), run)
        .await
        .context("the restore of a snapshot Firecracker must refuse did not return in 180 s")?
        .context("running fcvm snapshot run")
}

#[tokio::test]
async fn a_state_file_without_the_layout_mark_is_refused_with_the_recreate_hint() -> Result<()> {
    let (golden_name, clone_name, snapshot_name, _) = common::unique_names("layoutmark");
    let (mut golden_child, golden_pid, _log) = common::spawn_fcvm_with_env_and_log_path(
        &[
            "podman",
            "run",
            "--name",
            &golden_name,
            "--network",
            "rootless",
            common::TEST_IMAGE,
        ],
        &[("FCVM_NO_SNAPSHOT", "1")],
    )
    .await
    .context("spawning the source VM")?;
    let created: Result<()> = async {
        common::poll_health_by_pid(golden_pid, 180).await?;
        let output = tokio::process::Command::new(common::find_fcvm_binary()?)
            .args([
                "snapshot",
                "create",
                "--pid",
                &golden_pid.to_string(),
                "--tag",
                &snapshot_name,
            ])
            .kill_on_drop(true)
            .output()
            .await?;
        anyhow::ensure!(
            output.status.success(),
            "snapshot create failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }
    .await;
    common::kill_process(golden_pid).await;
    let _ = golden_child.wait().await;
    if let Err(error) = created {
        let _ = common::delete_snapshot(&snapshot_name).await;
        return Err(error);
    }

    let checked: Result<()> = async {
        let dir = fcvm::paths::snapshot_dir().join(&snapshot_name);
        let vmstate = dir.join("vmstate.bin");
        let marked = std::fs::read(&vmstate).with_context(|| vmstate.display().to_string())?;
        let mark_at = marked.len() - LAYOUT_MARK_LEN as usize;
        anyhow::ensure!(
            &marked[mark_at..mark_at + 8] == b"FCVMSNAP",
            "the state file this Firecracker wrote does not end with a layout mark, so this \
             build would load any earlier build's snapshot"
        );

        // The state as a build without the mark would have written it.
        std::fs::write(&vmstate, &marked[..mark_at])?;
        let refused = restore(&snapshot_name, &clone_name).await?;
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&refused.stdout),
            String::from_utf8_lossy(&refused.stderr)
        );
        anyhow::ensure!(
            !refused.status.success(),
            "a state file without the layout mark was restored:\n{said}"
        );
        anyhow::ensure!(
            said.contains(fcvm::firecracker::error_text::INVALID_DATA_VERSION)
                && said.contains("no layout mark"),
            "the refusal does not name the cause:\n{said}"
        );

        // The same file when the binary that made the snapshot is gone, which is how an
        // earlier pin's snapshot reaches this build: fcvm adds what to do about it.
        let config_path = dir.join("config.json");
        let mut config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path)?)?;
        let recorded = config
            .pointer_mut("/metadata/firecracker_bin")
            .context("the snapshot records no Firecracker binary")?;
        anyhow::ensure!(recorded.is_string(), "firecracker_bin is {recorded}");
        *recorded = serde_json::Value::String("/nonexistent/firecracker-of-an-earlier-pin".into());
        std::fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;
        let refused = restore(&snapshot_name, &format!("{clone_name}-2")).await?;
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&refused.stdout),
            String::from_utf8_lossy(&refused.stderr)
        );
        anyhow::ensure!(
            !refused.status.success(),
            "a state file without the layout mark was restored:\n{said}"
        );
        for expected in [
            fcvm::firecracker::error_text::INVALID_DATA_VERSION,
            "re-create the snapshot",
            "--firecracker-bin",
            "/nonexistent/firecracker-of-an-earlier-pin",
        ] {
            anyhow::ensure!(said.contains(expected), "no {expected:?} in:\n{said}");
        }
        Ok(())
    }
    .await;
    let deleted = common::delete_snapshot(&snapshot_name).await;
    checked?;
    deleted
}
