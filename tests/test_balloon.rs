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

/// How one run of a guest that leaves its balloon inactive ended: the run's log,
/// and each thing that was wrong with the way it ended.
struct InactiveBalloonRun {
    log_path: std::path::PathBuf,
    wrong: Vec<String>,
}

/// One `podman run --balloon 0 --free-page-reporting` of a guest booted with
/// `init_on_free=1`, with `extra` among its arguments. The run has to end by
/// itself, with an error that names the flag and the known causes and carries
/// what Firecracker answered. `FCVM_NO_SNAPSHOT` is taken out of the run's
/// environment, so only a `--no-snapshot` in `extra` keeps the run out of the
/// snapshot cache.
async fn run_with_an_inactive_balloon(name: &str, extra: &[&str]) -> Result<InactiveBalloonRun> {
    enum Ended {
        Exited(std::process::ExitStatus),
        Healthy,
    }

    let mut args = vec!["podman", "run", "--name", name];
    args.extend_from_slice(extra);
    args.extend_from_slice(&[
        "--balloon",
        "0",
        "--free-page-reporting",
        common::TEST_IMAGE,
    ]);
    let (mut child, pid, log_path) = common::spawn_fcvm_snapshots_enabled_with_env_and_log_path(
        &args,
        &[("FCVM_BOOT_ARGS", "init_on_free=1")],
    )
    .await?;
    let mut wrong = Vec::new();

    // The run has to end by itself. One that reaches healthy was not checked. The
    // health poll gives up when fcvm exits, and then the wait for the exit decides.
    let ended = tokio::time::timeout(Duration::from_secs(300), async {
        tokio::select! {
            status = child.wait() => status.map(Ended::Exited).context("waiting for fcvm"),
            Ok(()) = common::poll_health_by_pid(pid, 240) => Ok(Ended::Healthy),
        }
    })
    .await;
    let failed = match ended {
        Ok(Ok(Ended::Exited(status))) => {
            if status.success() {
                wrong.push(format!(
                    "the run of a VM whose guest left the balloon inactive exited with {status}"
                ));
            }
            !status.success()
        }
        Ok(Ok(Ended::Healthy)) => {
            // Said with the failure: whether the guest did boot with the argument.
            let cmdline = common::exec_in_vm(pid, &["/usr/bin/cat /proc/cmdline"])
                .await
                .unwrap_or_else(|error| format!("(not read: {error:#})"));
            common::kill_process(pid).await;
            let _ = child.kill().await;
            wrong.push(format!(
                "the run became healthy: fcvm did not check that the guest brought up its \
                 balloon device. The guest's command line {} init_on_free=1: {}",
                if cmdline.contains("init_on_free=1") {
                    "has"
                } else {
                    "does not have"
                },
                cmdline.trim()
            ));
            false
        }
        Ok(Err(error)) => {
            common::kill_process(pid).await;
            let _ = child.kill().await;
            return Err(error);
        }
        Err(_) => {
            common::kill_process(pid).await;
            let _ = child.kill().await;
            anyhow::bail!("the run neither ended nor became healthy within 300s");
        }
    };

    if let Err(error) = common::wait_for_log_eof(&log_path, Duration::from_secs(30)).await {
        wrong.push(format!("the run's log did not end: {error:#}"));
    }
    if failed {
        // The harness writes the command line and the environment into the same
        // file, so the flag and `init_on_free` are looked for on the line that
        // carries Firecracker's refusal, which only the error has.
        let log = std::fs::read_to_string(&log_path)
            .with_context(|| format!("reading {}", log_path.display()))?;
        let refusals: Vec<&str> = log
            .lines()
            .filter(|line| line.contains("Device not activated yet"))
            .collect();
        if refusals.is_empty() {
            wrong.push(format!(
                "the failed run's output ({}) does not carry Firecracker's refusal, 'Device \
                 not activated yet'",
                log_path.display()
            ));
        } else if !refusals.iter().any(|line| {
            ["--free-page-reporting", "init_on_free", "page poisoning"]
                .iter()
                .all(|part| line.contains(part))
        }) {
            wrong.push(format!(
                "the failed run's error does not name --free-page-reporting, init_on_free and \
                 page poisoning beside Firecracker's refusal: {refusals:?}"
            ));
        }
    }
    Ok(InactiveBalloonRun { log_path, wrong })
}

/// A guest that does not accept free page reporting leaves Firecracker's whole
/// balloon device inactive: the device has a queue for the reports, the guest never
/// sets it up, and Firecracker then activates nothing. `init_on_free=1` on the
/// kernel command line is one way a guest refuses the feature. A VM booted that way
/// with `--free-page-reporting` must not go on with a dead balloon, because a
/// snapshot of it hands that device to every run restored from it. So the run
/// fails, and its error names the flag and the known causes and carries what
/// Firecracker answered. Two runs are judged together. The first has
/// `--no-snapshot`: a cold boot whatever the snapshot cache holds, and the case
/// where fcvm takes no snapshot of its own to check before. The second would take
/// a pre-start snapshot: it has to fail before it starts one, and install none.
/// Its `--env` is this test run's own, so its snapshot key is too, and no other
/// run can have left it a snapshot to restore.
#[tokio::test]
async fn test_free_page_reporting_run_fails_when_the_guest_leaves_the_balloon_inactive(
) -> Result<()> {
    let (name, cached_name, _snap, _serve) = common::unique_names("fpr-inactive");
    let mut wrong = Vec::new();

    let cold = run_with_an_inactive_balloon(&name, &["--no-snapshot"]).await?;
    wrong.extend(
        cold.wrong
            .iter()
            .map(|what| format!("the --no-snapshot run: {what}")),
    );

    let env_unique = format!("FPR_INACTIVE_ID={cached_name}");
    let cached = run_with_an_inactive_balloon(&cached_name, &["--env", &env_unique]).await?;
    wrong.extend(
        cached
            .wrong
            .iter()
            .map(|what| format!("the run that takes snapshots: {what}")),
    );
    match common::cache_choice(&cached.log_path) {
        Some(("cold boot", key)) => {
            if common::snapshot_exists(&key) {
                wrong.push(format!(
                    "the run that takes snapshots installed the snapshot {key} of a VM whose \
                     guest left the balloon inactive"
                ));
                if let Err(error) = common::delete_snapshot(&key).await {
                    wrong.push(format!("the snapshot {key} was not deleted: {error:#}"));
                }
            }
        }
        other => wrong.push(format!(
            "control: the run that takes snapshots was to start as a cold boot on its way to \
             a pre-start snapshot, and its log says {other:?}"
        )),
    }
    let log = std::fs::read_to_string(&cached.log_path)
        .with_context(|| format!("reading {}", cached.log_path.display()))?;
    if log
        .lines()
        .any(|line| line.contains("Creating pre-start snapshot"))
    {
        wrong.push(
            "the run that takes snapshots logged `Creating pre-start snapshot`: it got to \
             its pre-start snapshot before the balloon check stopped it"
                .to_string(),
        );
    }
    anyhow::ensure!(wrong.is_empty(), "{}", wrong.join("\n"));
    Ok(())
}

/// nginx, started only once the test has made `/tmp/go` in the container. Until
/// then the health check fails, so the run cannot reach its startup snapshot.
#[cfg(feature = "privileged-tests")]
const HELD_NGINX: &str =
    "sh -c 'while [ ! -e /tmp/go ]; do sleep 0.2; done; exec nginx -g \"daemon off;\"'";

/// One `podman run --balloon 64 --health-check` whose workload is held. Returns
/// once its container is running, which is after its pre-start snapshot, with the
/// run's cache choice.
#[cfg(feature = "privileged-tests")]
async fn held_startup_run(
    run: &str,
    name: &str,
    env_unique: &str,
    cleanup_pids: &mut Vec<u32>,
) -> Result<(u32, (&'static str, String))> {
    let (_child, pid, log) = common::spawn_fcvm_snapshots_enabled_with_env_and_log_path(
        &[
            "podman",
            "run",
            "--name",
            name,
            "--network",
            "routed",
            "--env",
            env_unique,
            "--balloon",
            "64",
            "--health-check",
            "http://localhost/",
            "--cmd",
            HELD_NGINX,
            "nginx:alpine",
        ],
        &[],
    )
    .await
    .with_context(|| format!("spawning {run}"))?;
    cleanup_pids.push(pid);
    let deadline = Instant::now() + Duration::from_secs(300);
    while common::exec_in_container(pid, &["true"]).await.is_err() {
        anyhow::ensure!(
            Instant::now() < deadline,
            "{run}'s container was not running 300s after the run was spawned"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let choice = common::cache_choice(&log)
        .with_context(|| format!("{run}'s log has no line that says how it started"))?;
    Ok((pid, choice))
}

/// Set the held run's balloon target when `set_mib` is given, then let its workload
/// start and wait for the run to turn healthy. The run takes or declines its startup
/// snapshot before it reports healthy.
#[cfg(feature = "privileged-tests")]
async fn release_held_run(run: &str, pid: u32, set_mib: Option<u32>) -> Result<()> {
    if let Some(mib) = set_mib {
        let printed = report(
            &fcvm_balloon(&["--pid", &pid.to_string(), &mib.to_string()]).await?,
            &format!("fcvm balloon on {run}"),
        )?;
        anyhow::ensure!(
            printed.target_mib == mib,
            "control: the set on {run} reported target {} MiB, not {mib}",
            printed.target_mib
        );
    }
    // Control: the workload is still held, so the run has not turned healthy.
    let health = fcvm::state::StateManager::new(fcvm::paths::state_dir())
        .load_state_by_pid(pid)
        .await?
        .health_status;
    anyhow::ensure!(
        !matches!(health, fcvm::state::HealthStatus::Healthy),
        "control: {run} is healthy before its workload was let go"
    );
    common::exec_in_container(pid, &["touch /tmp/go"])
        .await
        .with_context(|| format!("letting {run}'s workload start"))?;
    common::poll_health_by_pid(pid, 300)
        .await
        .with_context(|| format!("{run} never turned healthy"))
}

/// A startup snapshot is named for the balloon target its workload initialized
/// under. When `fcvm balloon` changes the target while the workload initializes, the
/// run does not take its startup snapshot: the workload is the startup state of
/// neither target, and a later run at the named target would restore it. Run 1
/// cold-boots at 64 MiB with its workload held, is set to 96, and is then let go: it
/// has to turn healthy with no startup snapshot, under either name. Run 2 is the
/// same run again, so it restores the pre-start snapshot, and it is set to 96 the
/// same way: that covers the save in the restore loop. Run 3 is the control: nobody
/// changes its target, and it has to leave the 64 MiB startup snapshot.
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_startup_snapshot_is_not_taken_after_the_balloon_target_was_changed() -> Result<()> {
    const START_MIB: u32 = 64;
    const SET_MIB: u32 = 96;
    let (name_1, name_2, _, _) = common::unique_names("balloon-init");
    let name_3 = format!("{name_1}-untouched");
    // The same unique --env on every run: one snapshot key, cold on the first run.
    let env_unique = format!("BALLOONINIT_ID={name_1}");
    let states = fcvm::state::StateManager::new(fcvm::paths::state_dir());

    let mut cleanup_pids = Vec::new();
    let mut pre_start_key: Option<String> = None;
    let result: Result<()> = async {
        let (pid_1, (how_1, key)) =
            held_startup_run("run 1", &name_1, &env_unique, &mut cleanup_pids).await?;
        anyhow::ensure!(
            how_1 == "cold boot",
            "control: run 1 starts from a cold cache and started from a {how_1}"
        );
        pre_start_key = Some(key.clone());
        let startup = fcvm::commands::podman::startup_snapshot_key(&key, Some(START_MIB));
        let at_new_target = fcvm::commands::podman::startup_snapshot_key(&key, Some(SET_MIB));
        let pre_start = ("pre-start snapshot", key.clone());

        release_held_run("run 1", pid_1, Some(SET_MIB)).await?;
        anyhow::ensure!(
            !common::snapshot_exists(&startup),
            "run 1's balloon target was changed from {START_MIB} to {SET_MIB} MiB before its \
             workload initialized, and the run saved the startup snapshot {startup}, which \
             later {START_MIB} MiB runs restore"
        );
        anyhow::ensure!(
            !common::snapshot_exists(&at_new_target),
            "run 1 saved a startup snapshot under the target it was set to, {at_new_target}"
        );
        let recorded = states.load_state_by_pid(pid_1).await?.config.snapshot_name;
        anyhow::ensure!(
            recorded.as_deref() == Some(key.as_str()),
            "run 1's state names snapshot {recorded:?}, not its pre-start snapshot {key}"
        );
        common::kill_process(pid_1).await;

        let (pid_2, choice_2) =
            held_startup_run("run 2", &name_2, &env_unique, &mut cleanup_pids).await?;
        anyhow::ensure!(
            choice_2 == pre_start,
            "run 2 has only the pre-start snapshot {key} to restore and started from {choice_2:?}"
        );
        release_held_run("run 2", pid_2, Some(SET_MIB)).await?;
        anyhow::ensure!(
            !common::snapshot_exists(&startup),
            "run 2 restored the pre-start snapshot, its balloon target was changed from \
             {START_MIB} to {SET_MIB} MiB before its workload initialized, and the run saved \
             the startup snapshot {startup}"
        );
        anyhow::ensure!(
            !common::snapshot_exists(&at_new_target),
            "run 2 saved a startup snapshot under the target it was set to, {at_new_target}"
        );
        common::kill_process(pid_2).await;

        let (pid_3, choice_3) =
            held_startup_run("run 3", &name_3, &env_unique, &mut cleanup_pids).await?;
        anyhow::ensure!(
            choice_3 == pre_start,
            "run 3 has only the pre-start snapshot {key} to restore and started from {choice_3:?}"
        );
        release_held_run("run 3", pid_3, None).await?;
        anyhow::ensure!(
            common::snapshot_exists(&startup),
            "control: nobody changed run 3's balloon target, and it is healthy with no startup \
             snapshot {startup}"
        );
        Ok(())
    }
    .await;

    for pid in cleanup_pids.into_iter().rev() {
        common::kill_process(pid).await;
    }
    // Every snapshot of this test's runs is a directory named from run 1's pre-start
    // key, which the unique --env makes this test's own.
    if let Some(pre_start) = &pre_start_key {
        let named_from_it = format!("{pre_start}-");
        if let Ok(entries) = std::fs::read_dir(fcvm::paths::snapshot_dir()) {
            for entry in entries.flatten() {
                let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
                let Ok(name) = entry.file_name().into_string() else {
                    continue;
                };
                if is_dir && (&name == pre_start || name.starts_with(&named_from_it)) {
                    let _ = common::delete_snapshot(&name).await;
                }
            }
        }
    }
    result
}
