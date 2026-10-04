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
