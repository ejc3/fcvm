//! A SIGKILLed fcvm runs no teardown. What it made on the host is recorded with its owner
//! before it is made (src/reclaim.rs), and the next launch reclaims it.
//!
//! Each test kills fcvm with SIGKILL while the resource exists, then runs another VM to
//! completion (every launch reclaims first) and checks that the resource is gone. Another
//! test's launch can reclaim it first, which changes nothing that is checked: each check
//! is that the resource is gone after a launch, and that it existed while fcvm lived.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn vm_disks(vm_id: &str) -> PathBuf {
    fcvm::paths::data_dir().join("vm-disks").join(vm_id)
}

/// The vm_id of the VM whose fcvm process is `pid`.
async fn vm_id_of(pid: u32) -> Result<String> {
    let output = tokio::process::Command::new(common::find_fcvm_binary()?)
        .args(["ls", "--json", "--pid", &pid.to_string()])
        .output()
        .await
        .context("running fcvm ls")?;
    let listed: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parsing fcvm ls --json")?;
    listed
        .get(0)
        .and_then(|vm| vm.get("vm_id"))
        .and_then(|id| id.as_str())
        .map(str::to_string)
        .with_context(|| format!("fcvm ls lists no VM for pid {pid}: {listed}"))
}

async fn sigkill(child: &mut tokio::process::Child, pid: u32) -> Result<()> {
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    )
    .context("sending SIGKILL to fcvm")?;
    child.wait().await.context("reaping the killed fcvm")?;
    Ok(())
}

/// Stop a VM the way an operator does, and wait for its teardown to finish.
async fn stop(child: &mut tokio::process::Child, pid: u32) -> Result<()> {
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .context("sending SIGTERM to fcvm")?;
    tokio::time::timeout(Duration::from_secs(60), child.wait())
        .await
        .context("fcvm did not exit within 60 s of SIGTERM")?
        .context("waiting for fcvm")?;
    Ok(())
}

/// Run a VM to completion. Its launch reclaims what killed fcvm processes left.
async fn next_launch() -> Result<()> {
    let (name, _, _, _) = common::unique_names("reclaim-next");
    let (mut child, _) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &name,
        "--no-snapshot",
        common::TEST_IMAGE,
        "true",
    ])
    .await?;
    let status = tokio::time::timeout(Duration::from_secs(180), child.wait())
        .await
        .context("the next launch did not finish within 180 s")?
        .context("waiting for the next launch")?;
    anyhow::ensure!(status.success(), "the next launch failed: {status}");
    Ok(())
}

/// Poll `found` every 10 ms until it returns Some, for at most `limit`.
async fn wait_for<T>(
    what: &str,
    limit: Duration,
    mut found: impl FnMut() -> Option<T>,
) -> Result<T> {
    let start = Instant::now();
    loop {
        if let Some(value) = found() {
            return Ok(value);
        }
        anyhow::ensure!(start.elapsed() < limit, "{what} within {limit:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// #1183 and #1185: a VM's runtime directory, its disk inside, and pasta's PID file stay
/// when fcvm is SIGKILLed, and the next launch removes them.
#[tokio::test]
async fn a_sigkilled_vm_leaves_its_runtime_dir_and_pasta_pid_file_to_the_next_launch() -> Result<()>
{
    let (name, _, _, _) = common::unique_names("reclaim-vm");
    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &name,
        "--network",
        "rootless",
        "--no-snapshot",
        common::TEST_IMAGE,
    ])
    .await?;
    let observed = async {
        common::poll_health_by_pid(pid, 120).await?;
        let vm_id = vm_id_of(pid).await?;
        let runtime_dir = vm_disks(&vm_id);
        // Where pasta's PID file is, and where it was before it moved into the VM's
        // directory: neither may remain.
        let pid_files = [
            runtime_dir.join("pasta.pid"),
            fcvm::paths::data_dir().join(format!("pasta-{vm_id}.pid")),
        ];
        anyhow::ensure!(
            runtime_dir.join("disks/rootfs.raw").exists(),
            "the running VM has no disk in {}",
            runtime_dir.display()
        );
        anyhow::ensure!(
            pid_files.iter().any(|f| f.exists()),
            "the running rootless VM has no pasta PID file at {pid_files:?}"
        );
        Ok((runtime_dir, pid_files))
    }
    .await;
    sigkill(&mut child, pid).await?;
    let (runtime_dir, pid_files) = observed?;

    next_launch().await?;
    assert!(
        !runtime_dir.exists(),
        "the killed VM's runtime directory survived the next launch: {}",
        runtime_dir.display()
    );
    let left: Vec<_> = pid_files.iter().filter(|f| f.exists()).collect();
    assert!(
        left.is_empty(),
        "the killed VM's pasta PID file survived the next launch: {left:?}"
    );
    Ok(())
}

/// The first `.reached` file under a park directory: a process parked at the point.
fn parked(dir: &Path) -> Option<PathBuf> {
    for actor in std::fs::read_dir(dir).ok()?.flatten() {
        for event in std::fs::read_dir(actor.path()).ok()?.flatten() {
            if event.file_name().to_string_lossy().ends_with(".reached") {
                return Some(event.path());
            }
        }
    }
    None
}

/// #1181: a restore of a snapshot whose source VM is gone recreates the source's
/// directory as a mountpoint. The clone's teardown removes it, and after a SIGKILL of a
/// restore that made it (with the clone's own runtime directory, #1183) the next launch
/// removes both.
#[cfg(feature = "integration-slow")]
#[tokio::test]
async fn a_restore_removes_the_mountpoint_it_makes_over_a_gone_source() -> Result<()> {
    let (source_name, clone_name, snapshot, _) = common::unique_names("reclaim-mount");

    let (mut source, source_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &source_name,
        "--network",
        "rootless",
        "--no-snapshot",
        common::TEST_IMAGE,
    ])
    .await?;
    let created = async {
        common::poll_health_by_pid(source_pid, 120).await?;
        let source_id = vm_id_of(source_pid).await?;
        common::create_snapshot_by_pid(source_pid, &snapshot).await?;
        Ok::<_, anyhow::Error>(source_id)
    }
    .await;
    stop(&mut source, source_pid).await?;
    let source_id = created?;
    let source_dir = vm_disks(&source_id);
    anyhow::ensure!(
        !source_dir.exists(),
        "the source VM's teardown left {}",
        source_dir.display()
    );

    let result = async {
        // A clone that exits normally.
        let (mut clone, clone_pid) = common::spawn_fcvm(&[
            "snapshot",
            "run",
            "--snapshot",
            &snapshot,
            "--name",
            &clone_name,
        ])
        .await?;
        let healthy = common::poll_health_by_pid(clone_pid, 120).await;
        let made = source_dir.is_dir();
        stop(&mut clone, clone_pid).await?;
        healthy?;
        anyhow::ensure!(
            made,
            "the running clone has no mountpoint at {}",
            source_dir.display()
        );
        anyhow::ensure!(
            !source_dir.exists(),
            "the clone's teardown left the mountpoint it made at {}",
            source_dir.display()
        );

        // A restore killed after it made the mountpoint and its own directory, before
        // its VMM loaded the snapshot.
        let park = tempfile::tempdir()?;
        let spec = format!("restore.pre_load:park:{}", park.path().display());
        let (killed_name, _, _, _) = common::unique_names("reclaim-mount-killed");
        let (mut killed, killed_pid) = common::spawn_fcvm_with_env(
            &[
                "snapshot",
                "run",
                "--snapshot",
                &snapshot,
                "--name",
                &killed_name,
            ],
            &[("FCVM_FAILPOINT", spec.as_str())],
        )
        .await?;
        let observed = async {
            wait_for(
                "the restore did not park at restore.pre_load",
                Duration::from_secs(120),
                || parked(park.path()),
            )
            .await?;
            let killed_dir = vm_disks(&vm_id_of(killed_pid).await?);
            anyhow::ensure!(
                source_dir.is_dir() && killed_dir.join("disks").is_dir(),
                "the parked restore has not made {} and {}",
                source_dir.display(),
                killed_dir.display()
            );
            Ok(killed_dir)
        }
        .await;
        sigkill(&mut killed, killed_pid).await?;
        let killed_dir = observed?;

        next_launch().await?;
        anyhow::ensure!(
            !source_dir.exists(),
            "the killed restore's mountpoint survived the next launch: {}",
            source_dir.display()
        );
        anyhow::ensure!(
            !killed_dir.exists(),
            "the killed restore's runtime directory survived the next launch: {}",
            killed_dir.display()
        );
        Ok(())
    }
    .await;
    common::delete_snapshot(&snapshot).await.ok();
    result
}

/// Build a localhost image whose content, and so whose digest, is this test's own, and
/// return its image-cache key (the manifest digest, or the image id when there is none).
async fn unique_localhost_image(name: &str) -> Result<String> {
    let context = tempfile::tempdir()?;
    std::fs::write(
        context.path().join("Containerfile"),
        format!(
            "FROM {}\nLABEL fcvm.reclaim.test=\"{}\"\n",
            common::TEST_IMAGE,
            uuid::Uuid::new_v4()
        ),
    )?;
    let built = tokio::process::Command::new("podman")
        .args(["build", "-q", "-t", name])
        .arg(context.path())
        .output()
        .await
        .context("running podman build")?;
    anyhow::ensure!(
        built.status.success(),
        "podman build failed: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    let inspected = tokio::process::Command::new("podman")
        .args(["image", "inspect", name, "--format", "{{.Digest}}\t{{.Id}}"])
        .output()
        .await
        .context("running podman image inspect")?;
    anyhow::ensure!(inspected.status.success(), "podman image inspect failed");
    // Only the line end: a locally built image has an empty digest, so the line starts
    // with the tab.
    let line = String::from_utf8_lossy(&inspected.stdout)
        .trim_end()
        .to_string();
    let (digest, id) = line
        .split_once('\t')
        .with_context(|| format!("unexpected inspect output {line:?}"))?;
    let key = if digest.is_empty() { id } else { digest };
    Ok(key.trim_start_matches("sha256:").to_string())
}

/// Image-cache temporaries of `key`: names beginning with it and ending in `.tmp`.
fn cache_temps(key: &str) -> Vec<PathBuf> {
    std::fs::read_dir(fcvm::paths::image_cache_dir())
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    let name = p.file_name().unwrap_or_default().to_string_lossy();
                    name.starts_with(key) && name.ends_with(".tmp")
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Every temporary a record of the fcvm process `pid` lists, in this user's owner
/// directory.
fn recorded_temps(pid: u32) -> Vec<PathBuf> {
    let dir = fcvm::paths::data_dir()
        .join("reclaim")
        .join(nix::unistd::geteuid().as_raw().to_string());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut temps = Vec::new();
    for entry in entries.flatten() {
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        let Ok(record) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        if record.get("pid").and_then(|p| p.as_u64()) != Some(u64::from(pid)) {
            continue;
        }
        for resource in record["resources"].as_array().into_iter().flatten() {
            if resource["kind"] == "temp" {
                if let Some(path) = resource["path"].as_str() {
                    temps.push(PathBuf::from(path));
                }
            }
        }
    }
    temps
}

/// #1186: a SIGKILLed export of a localhost image leaves its temporary archive, and a
/// SIGKILLed storage image build leaves its temporary store and image. The next launch
/// removes them.
#[cfg(feature = "integration-slow")]
#[tokio::test]
async fn a_sigkilled_image_export_leaves_its_temporaries_to_the_next_launch() -> Result<()> {
    let image = format!("localhost/fcvm-reclaim-{}", uuid::Uuid::new_v4().simple());
    let key = unique_localhost_image(&image).await?;
    let result = async {
        // Killed while podman writes the archive.
        let (name, _, _, _) = common::unique_names("reclaim-export");
        let (mut child, pid) =
            common::spawn_fcvm(&["podman", "run", "--name", &name, "--no-snapshot", &image])
                .await?;
        let observed = wait_for(
            "no temporary archive appeared",
            Duration::from_secs(120),
            || {
                cache_temps(&key)
                    .into_iter()
                    .find(|p| p.to_string_lossy().contains(".docker.tar."))
            },
        )
        .await;
        sigkill(&mut child, pid).await?;
        let archive_temp = observed?;
        next_launch().await?;
        anyhow::ensure!(
            !archive_temp.exists(),
            "the killed export's archive survived the next launch: {}",
            archive_temp.display()
        );

        // Killed while the storage image is built from the published archive.
        let (name, _, _, _) = common::unique_names("reclaim-storage");
        let (mut child, pid) =
            common::spawn_fcvm(&["podman", "run", "--name", &name, "--no-snapshot", &image])
                .await?;
        let observed = wait_for(
            "no temporary storage image appeared",
            Duration::from_secs(180),
            || {
                cache_temps(&key)
                    .into_iter()
                    .find(|p| p.to_string_lossy().contains(".storage-"))
            },
        )
        .await;
        let recorded = recorded_temps(pid);
        sigkill(&mut child, pid).await?;
        let storage_temp = observed?;
        let store = recorded.iter().find(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("tmp-storage-"))
        });
        anyhow::ensure!(
            store.is_some_and(|s| s.is_dir()),
            "no record of the killed build lists an existing temporary store: {recorded:?}"
        );
        next_launch().await?;
        let left: Vec<_> = std::iter::once(&storage_temp)
            .chain(recorded.iter())
            .filter(|p| p.exists())
            .collect();
        anyhow::ensure!(
            left.is_empty(),
            "the killed build's temporaries survived the next launch: {left:?}"
        );
        Ok(())
    }
    .await;
    let _ = tokio::process::Command::new("podman")
        .args(["rmi", "-f", &image])
        .output()
        .await;
    // The image's cache entries: no other test uses this digest.
    for entry in std::fs::read_dir(fcvm::paths::image_cache_dir())?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&key) && !name.ends_with(".lock") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    result
}
