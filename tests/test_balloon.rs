//! `fcvm balloon`: read and set the balloon target of a running VM.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use fcvm::firecracker::api::BalloonStats;
use std::time::{Duration, Instant};

/// How one `fcvm balloon` invocation ended and what it printed.
struct Ran {
    ok: bool,
    stdout: String,
    stderr: String,
}

async fn fcvm_balloon(args: &[&str]) -> Result<Ran> {
    let fcvm = common::find_fcvm_binary()?;
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(&fcvm)
            .arg("balloon")
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .with_context(|| format!("fcvm balloon {args:?} did not return within 60s"))?
    .with_context(|| format!("running fcvm balloon {args:?}"))?;
    Ok(Ran {
        ok: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// The report of a run that has to succeed: its standard output is one JSON line.
fn report(ran: &Ran, what: &str) -> Result<BalloonStats> {
    anyhow::ensure!(ran.ok, "{what} failed: {}", ran.stderr.trim());
    let mut lines = ran.stdout.lines();
    let (Some(line), None) = (lines.next(), lines.next()) else {
        anyhow::bail!("{what} printed {:?}, not one line", ran.stdout);
    };
    serde_json::from_str(line)
        .with_context(|| format!("{what} printed {line:?}, not the balloon report"))
}

/// `fcvm balloon` reports a VM's balloon and sets its target. A VM booted with
/// `--balloon 64` reports target 64, by PID and by name. `fcvm balloon --pid P 96`
/// sets the target: its own output, Firecracker's statistics and a later report all
/// say 96, and the guest brings the balloon to that size. The command writes no
/// state, so the VM's state still records 64, and the next snapshot, which reads the
/// device from the VMM, records 96. A target above the VM's memory is refused, and
/// a VM booted without `--balloon` is told it has no balloon device. Everything
/// after the controls is judged in one run, so one failure does not hide another.
#[tokio::test]
async fn test_balloon_command_sets_and_reports_the_target() -> Result<()> {
    const BOOT_MIB: u32 = 64;
    const SET_MIB: u32 = 96;
    let (name, bare_name, snap, _) = common::unique_names("balloon-cmd");
    let (boot, set) = (BOOT_MIB.to_string(), SET_MIB.to_string());

    // --no-snapshot makes each a cold boot of its own.
    let (mut child, pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &name,
            "--no-snapshot",
            "--balloon",
            &boot,
            "nginx:alpine",
        ],
        "balloon-cmd-vm",
    )
    .await?;
    let bare = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &bare_name,
            "--no-snapshot",
            "nginx:alpine",
        ],
        "balloon-cmd-bare",
    )
    .await;

    let result = async {
        let bare_pid = match &bare {
            Ok((_, bare_pid)) => *bare_pid,
            Err(error) => anyhow::bail!("spawning the VM with no balloon: {error:#}"),
        };
        common::poll_health_by_pid(pid, 120).await?;
        common::poll_health_by_pid(bare_pid, 120).await?;
        let (pid_arg, bare_pid_arg) = (pid.to_string(), bare_pid.to_string());
        let states = fcvm::state::StateManager::new(fcvm::paths::state_dir());

        // Controls: the command reports the target the VM booted with, however the
        // VM is named.
        let by_pid = report(
            &fcvm_balloon(&["--pid", &pid_arg]).await?,
            "fcvm balloon --pid",
        )?;
        anyhow::ensure!(
            by_pid.target_mib == BOOT_MIB,
            "control: fcvm balloon --pid reports target {} MiB, not the {BOOT_MIB} the VM \
             booted with",
            by_pid.target_mib
        );
        let by_name = report(
            &fcvm_balloon(&["--name", &name]).await?,
            "fcvm balloon --name",
        )?;
        anyhow::ensure!(
            by_name.target_mib == BOOT_MIB,
            "control: fcvm balloon --name reports target {} MiB, not {BOOT_MIB}",
            by_name.target_mib
        );

        let mut wrong: Vec<String> = Vec::new();

        // Set, and read back three ways.
        let printed = report(
            &fcvm_balloon(&["--pid", &pid_arg, &set]).await?,
            "fcvm balloon --pid P MIB",
        )?;
        if printed.target_mib != SET_MIB {
            wrong.push(format!(
                "`fcvm balloon --pid P {SET_MIB}` printed target {} MiB",
                printed.target_mib
            ));
        }
        let from_vmm = common::balloon_stats_by_pid(pid)
            .await
            .context("reading Firecracker's balloon statistics")?;
        if from_vmm.target_mib != SET_MIB {
            wrong.push(format!(
                "after `fcvm balloon --pid P {SET_MIB}` Firecracker reports the device at \
                 target {} MiB",
                from_vmm.target_mib
            ));
        } else {
            // The guest acts on the new target; the command sees it get there.
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                let read_back = report(
                    &fcvm_balloon(&["--name", &name]).await?,
                    "fcvm balloon --name",
                )?;
                if (read_back.target_mib, read_back.actual_mib) == (SET_MIB, SET_MIB) {
                    break;
                }
                if Instant::now() >= deadline {
                    wrong.push(format!(
                        "60s after the target was set to {SET_MIB} MiB the command reports \
                         target {} MiB, size {} MiB",
                        read_back.target_mib, read_back.actual_mib
                    ));
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }

        // The command writes no state.
        let state = states.load_state_by_pid(pid).await?;
        if state.config.balloon_mib != Some(BOOT_MIB) {
            wrong.push(format!(
                "the VM's state records balloon {:?} after the command, which writes no \
                 state; it booted at Some({BOOT_MIB})",
                state.config.balloon_mib
            ));
        }
        // The next snapshot reads the device from the VMM.
        common::create_snapshot_by_pid(pid, &snap)
            .await
            .context("creating a snapshot after the target was set")?;
        let in_snapshot = fcvm::storage::SnapshotManager::new(fcvm::paths::snapshot_dir())
            .load_snapshot(&snap)
            .await?
            .metadata
            .balloon_mib;
        if in_snapshot != Some(SET_MIB) {
            wrong.push(format!(
                "a snapshot taken after the target was set to {SET_MIB} MiB records \
                 {in_snapshot:?}"
            ));
        }

        // Refusals.
        let too_big = (state.config.memory_mib + 1).to_string();
        let refused = fcvm_balloon(&["--pid", &pid_arg, &too_big]).await?;
        if refused.ok || !refused.stderr.contains("is above the") {
            wrong.push(format!(
                "a target of {too_big} MiB for a VM with {} MiB of memory: succeeded={}, \
                 stderr {:?}",
                state.config.memory_mib,
                refused.ok,
                refused.stderr.trim()
            ));
        }
        for args in [
            vec!["--pid", bare_pid_arg.as_str()],
            vec!["--pid", bare_pid_arg.as_str(), set.as_str()],
        ] {
            let ran = fcvm_balloon(&args).await?;
            if ran.ok || !ran.stderr.contains("has no balloon device") {
                wrong.push(format!(
                    "fcvm balloon {args:?} on a VM booted without --balloon: succeeded={}, \
                     stderr {:?}",
                    ran.ok,
                    ran.stderr.trim()
                ));
            }
        }

        anyhow::ensure!(wrong.is_empty(), "{}", wrong.join("\n"));
        Ok(())
    }
    .await;

    common::kill_process(pid).await;
    let _ = child.kill().await;
    if let Ok((mut bare_child, bare_pid)) = bare {
        common::kill_process(bare_pid).await;
        let _ = bare_child.kill().await;
    }
    let _ = common::delete_snapshot(&snap).await;
    result
}

/// One fcvm command that was started with a failpoint armed and has reached it.
struct Held {
    child: tokio::process::Child,
    stdout: tokio::task::JoinHandle<String>,
    stderr: tokio::task::JoinHandle<String>,
}

impl Held {
    /// Wait for the command to end.
    async fn finish(mut self, what: &str) -> Result<Ran> {
        let status = tokio::time::timeout(Duration::from_secs(120), self.child.wait())
            .await
            .with_context(|| format!("{what} did not end within 120s"))?
            .with_context(|| format!("waiting for {what}"))?;
        Ok(Ran {
            ok: status.success(),
            stdout: self.stdout.await?,
            stderr: self.stderr.await?,
        })
    }
}

/// Start `fcvm <args>` with `failpoint` armed to sleep `hold_ms`, and return once the
/// command says it has reached it. Its output is read to the end either way.
async fn start_and_hold(args: &[&str], failpoint: &str, hold_ms: u64) -> Result<Held> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    let fcvm = common::find_fcvm_binary()?;
    let mut child = tokio::process::Command::new(&fcvm)
        .args(args)
        .env("FCVM_FAILPOINT", format!("{failpoint}:sleep:{hold_ms}"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting fcvm {args:?}"))?;
    let mut out = child.stdout.take().context("no stdout pipe")?;
    let err = child.stderr.take().context("no stderr pipe")?;
    let stdout = tokio::spawn(async move {
        let mut text = String::new();
        let _ = out.read_to_string(&mut text).await;
        text
    });
    let marker = format!("FAILPOINT {failpoint} reached");
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let stderr = tokio::spawn(async move {
        let mut reached_tx = Some(reached_tx);
        let mut lines = BufReader::new(err).lines();
        let mut text = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.contains(&marker) {
                if let Some(reached) = reached_tx.take() {
                    let _ = reached.send(());
                }
            }
            text.push_str(&line);
            text.push('\n');
        }
        text
    });
    // The sender is dropped when the command's stderr ends without the marker.
    match tokio::time::timeout(Duration::from_secs(120), reached_rx).await {
        Ok(Ok(())) => Ok(Held {
            child,
            stdout,
            stderr,
        }),
        _ => {
            let _ = child.kill().await;
            anyhow::bail!(
                "fcvm {args:?} did not reach failpoint {failpoint}: {}",
                stderr.await.unwrap_or_default().trim()
            )
        }
    }
}

/// A target set by `fcvm balloon` cannot land between a snapshot's read of the
/// balloon and its save. `snapshot create` is held there by a failpoint: the VM is
/// paused and the snapshot's record, 64 MiB, is already read. `fcvm balloon --pid P
/// 96` is started while it is held. The snapshot has to hold what it recorded: a
/// clone restored from it gets the device as it was saved and has to come up at 64.
/// The set is not lost: it waits for the snapshot, then sets 96 and reports 96.
/// Without the lock the PATCH lands on the paused VM, and the snapshot records 64
/// with a device saved at 96.
#[tokio::test]
async fn test_balloon_set_cannot_land_between_a_snapshots_read_and_its_save() -> Result<()> {
    const BOOT_MIB: u32 = 64;
    const SET_MIB: u32 = 96;
    const FAILPOINT: &str = "snapshot.post_balloon_read_pre_save";
    let (name, clone_name, snap, _) = common::unique_names("balloon-lock");
    let (boot, set) = (BOOT_MIB.to_string(), SET_MIB.to_string());
    let mut wrong: Vec<String> = Vec::new();

    let (mut child, pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &name,
            "--no-snapshot",
            "--balloon",
            &boot,
            "nginx:alpine",
        ],
        "balloon-lock-vm",
    )
    .await?;
    let pid_arg = pid.to_string();
    let snapshotted = async {
        common::poll_health_by_pid(pid, 120).await?;
        let snapshot = start_and_hold(
            &["snapshot", "create", "--pid", &pid_arg, "--tag", &snap],
            FAILPOINT,
            8000,
        )
        .await?;
        // The snapshot now sits between its read and its save, for 8 s.
        let set_ran = fcvm_balloon(&["--pid", &pid_arg, &set]).await?;
        let snapshot_ran = snapshot.finish("snapshot create").await?;
        anyhow::ensure!(
            snapshot_ran.ok,
            "snapshot create failed: {}",
            snapshot_ran.stderr.trim()
        );
        // The set is not lost.
        match report(&set_ran, "fcvm balloon --pid P MIB during a snapshot") {
            Ok(printed) if printed.target_mib == SET_MIB => {}
            Ok(printed) => wrong.push(format!(
                "the set asked for {SET_MIB} MiB during a snapshot and reported target {} MiB",
                printed.target_mib
            )),
            Err(error) => wrong.push(format!("{error:#}")),
        }
        let source = common::balloon_stats_by_pid(pid)
            .await
            .context("reading the VM's balloon after the snapshot and the set")?;
        if source.target_mib != SET_MIB {
            wrong.push(format!(
                "after the snapshot and the set, the VM's balloon target is {} MiB, not the \
                 {SET_MIB} the set asked for",
                source.target_mib
            ));
        }
        anyhow::Ok(())
    }
    .await;
    // The clone restores from the snapshot files, with the source gone.
    common::kill_process(pid).await;
    let _ = child.kill().await;

    let result = async {
        snapshotted?;
        let recorded = fcvm::storage::SnapshotManager::new(fcvm::paths::snapshot_dir())
            .load_snapshot(&snap)
            .await?
            .metadata
            .balloon_mib;
        let (mut clone_child, clone_pid) = common::spawn_fcvm_with_logs(
            &[
                "snapshot",
                "run",
                "--snapshot",
                &snap,
                "--name",
                &clone_name,
            ],
            "balloon-lock-c1",
        )
        .await?;
        let restored = async {
            common::poll_health_by_pid(clone_pid, 120).await?;
            common::balloon_stats_by_pid(clone_pid)
                .await
                .context("reading the restored clone's balloon")
        }
        .await;
        common::kill_process(clone_pid).await;
        let _ = clone_child.kill().await;
        let restored = restored?;
        if recorded != Some(restored.target_mib) {
            wrong.push(format!(
                "the snapshot records balloon {recorded:?} and the device it saved holds {} \
                 MiB: a target was set between the snapshot's read and its save",
                restored.target_mib
            ));
        }
        if recorded != Some(BOOT_MIB) {
            wrong.push(format!(
                "the snapshot had read the balloon before the set started and records \
                 {recorded:?}, not Some({BOOT_MIB})"
            ));
        }
        anyhow::ensure!(wrong.is_empty(), "{}", wrong.join("\n"));
        Ok(())
    }
    .await;
    let _ = common::delete_snapshot(&snap).await;
    result
}

/// Two sets do not interleave: each reports the target it set. The first is held by
/// a failpoint between its PATCH and its report, and a second, with another target,
/// is started while it is held. The first has to report its own 96 and the second
/// its own 80, and the VM ends at 80, the later of the two. Without the lock the
/// second set's PATCH lands inside the first, which then reports 80.
#[tokio::test]
async fn test_two_balloon_sets_each_report_their_own_target() -> Result<()> {
    const BOOT_MIB: u32 = 64;
    const FIRST_MIB: u32 = 96;
    const SECOND_MIB: u32 = 80;
    const FAILPOINT: &str = "balloon.post_set_pre_report";
    let (name, _, _, _) = common::unique_names("balloon-two");
    let (boot, first_mib, second_mib) = (
        BOOT_MIB.to_string(),
        FIRST_MIB.to_string(),
        SECOND_MIB.to_string(),
    );

    let (mut child, pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &name,
            "--no-snapshot",
            "--balloon",
            &boot,
            "nginx:alpine",
        ],
        "balloon-two-vm",
    )
    .await?;
    let pid_arg = pid.to_string();
    let result = async {
        common::poll_health_by_pid(pid, 120).await?;
        let first =
            start_and_hold(&["balloon", "--pid", &pid_arg, &first_mib], FAILPOINT, 5000).await?;
        // The first set has sent its PATCH and not read its report yet, for 5 s.
        let second = fcvm_balloon(&["--pid", &pid_arg, &second_mib]).await?;
        let first = first.finish("the first set").await?;

        let mut wrong: Vec<String> = Vec::new();
        for (which, ran, asked) in [
            ("first", &first, FIRST_MIB),
            ("second", &second, SECOND_MIB),
        ] {
            match report(ran, &format!("the {which} set")) {
                Ok(printed) if printed.target_mib == asked => {}
                Ok(printed) => wrong.push(format!(
                    "the {which} set asked for {asked} MiB and reported target {} MiB",
                    printed.target_mib
                )),
                Err(error) => wrong.push(format!("{error:#}")),
            }
        }
        let end = common::balloon_stats_by_pid(pid)
            .await
            .context("reading the VM's balloon after both sets")?;
        if end.target_mib != SECOND_MIB {
            wrong.push(format!(
                "the VM ends at target {} MiB, not the {SECOND_MIB} of the set that ran last",
                end.target_mib
            ));
        }
        anyhow::ensure!(wrong.is_empty(), "{}", wrong.join("\n"));
        Ok(())
    }
    .await;
    common::kill_process(pid).await;
    let _ = child.kill().await;
    result
}
