//! A container process that floods a pipe to conmon must not make the VM unreachable.
//!
//! When the client of a container exec dies, the command keeps running, as it
//! does under podman. If that command writes without pause (`yes`), conmon
//! drains and discards its output, and the two wake each other tens of
//! thousands of times a second. The command runs in the container's cgroup,
//! conmon in `fc-agent.service` above it, so every sleep and wake changes the
//! load of a group runqueue and the kernel reweights the group entities above
//! it, up to the root.
//!
//! Linux 6.18.44 and older re-place the running entity on each of those
//! reweights (`reweight_entity()` calling `place_entity()`, reverted upstream
//! by 101f3498b4bd and in stable from 6.18.45). Under the storm the guest gives
//! runnable kernel threads no CPU at all. The virtio-vsock receive worker is
//! one of them, so the guest accepts no exec, health check or output
//! connection until the command exits. Measured on a 2-vCPU guest: `yes` and
//! conmon took 4,959 to 4,987 ms of every 5,000 ms, and
//! `kworker/1:0-virtio_vsock` stayed runnable for 4.2 s without running.
//!
//! Delayed dequeue hides most of those reweights, which made the failure
//! intermittent (8 of 28 runs of a harness that left two such commands
//! behind). With it off 6.18.44 stalled in every run of that harness (28 of
//! 28, then 33 of 33) and 6.18.50 in 0 of 70, so the test turns it off.
//!
//! This test on a 6.18.44 build failed 6 of 6 runs: the slowest trivial exec
//! took between 5.6 and 13.1 s, and 8 to 22 execs fit in the window. On
//! 6.18.50 it passed 3 of 3 with the slowest between 0.22 and 0.34 s. With
//! two flooding commands instead of four, 6.18.44 failed 5 of 6 at a 5 s
//! limit.

#![cfg(feature = "integration-slow")]

mod common;

use anyhow::{Context, Result};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;

/// Pairs of container execs left flooding conmon.
const ORPHAN_PAIRS: usize = 2;
/// How long the flooding commands live once their clients are gone. Longer
/// than the test, whose teardown of the VM ends them.
const FLOOD_SECS: u64 = 120;
/// How long trivial execs are timed while the flood runs.
const WATCH: Duration = Duration::from_secs(15);
/// Prints how many `yes` processes the container has and the CPU ticks
/// (USER_HZ, 100 a second) they have used. One script: the exec helper runs
/// its arguments under `sh -c`.
const FLOOD_PROBE: &str = "n=0; t=0; for p in $(pidof yes); do n=$((n+1)); \
     s=$(awk '{print $14+$15}' /proc/$p/stat 2>/dev/null); t=$((t+${s:-0})); done; echo \"$n $t\"";
/// Four flooding commands share two vCPUs with conmon for at least the 15 s
/// watch and used 1,630 and 1,654 ticks in two runs. Commands that are alive
/// but blocked on a full pipe use next to nothing.
const MIN_FLOOD_TICKS: u64 = 300;
/// A healthy guest answers a trivial exec in 0.2 to 0.4 s here. A starved
/// guest took 5.6 s at best. nextest runs this test alone, so the ceiling
/// measures the guest and not the other tests' load.
const EXEC_LIMIT: Duration = Duration::from_secs(3);

#[tokio::test]
async fn test_a_container_flooding_conmon_leaves_the_vm_reachable() -> Result<()> {
    let fcvm = common::find_fcvm_binary()?;
    let (name, _, _, _) = common::unique_names("exec-flood");
    let (mut vm, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &name,
        "--network",
        "rootless",
        common::TEST_IMAGE,
    ])
    .await
    .context("spawning fcvm podman run")?;
    common::poll_health_by_pid(pid, 300).await?;

    // One script string: the helper runs its arguments under `sh -c`.
    let features = common::exec_in_vm(
        pid,
        &["mount -t debugfs none /sys/kernel/debug 2>/dev/null; \
           echo NO_DELAY_DEQUEUE > /sys/kernel/debug/sched/features && \
           cat /sys/kernel/debug/sched/features"],
    )
    .await
    .context(
        "turning delayed dequeue off in the guest (needs CONFIG_DEBUG_FS, which both \
         Firecracker base configs set)",
    )?;
    assert!(
        features.split_whitespace().any(|f| f == "NO_DELAY_DEQUEUE"),
        "the guest scheduler did not take NO_DELAY_DEQUEUE, so this run could pass on a \
         broken kernel: {features:?}"
    );

    // Container execs whose clients die mid-flood. Each pair is one without
    // stdin and one with endless stdin.
    for _ in 0..ORPHAN_PAIRS {
        leave_a_flooding_orphan(&fcvm, pid, false).await?;
        leave_a_flooding_orphan(&fcvm, pid, true).await?;
    }

    let started = Instant::now();
    let mut slowest = Duration::ZERO;
    let mut execs = 0u32;
    while started.elapsed() < WATCH {
        for in_vm in [true, false] {
            let took = time_a_trivial_exec(&fcvm, pid, in_vm).await?;
            slowest = slowest.max(took);
            execs += 1;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    println!(
        "slowest trivial exec: {:.2} s over {execs} execs",
        slowest.as_secs_f64()
    );

    // A starved guest is the finding. Only fast execs need proof that the
    // floods were running, and asking a starved guest for it would replace the
    // finding with a complaint about the floods.
    let flood = if slowest < EXEC_LIMIT {
        Some(common::exec_in_container(pid, &[FLOOD_PROBE]).await)
    } else {
        None
    };

    common::kill_process(pid).await;
    let _ = vm.wait().await;

    assert!(
        slowest < EXEC_LIMIT,
        "a trivial exec took {:.1} s (limit {} s, {execs} execs) while {} container \
         commands flooded conmon. The guest kernel is starving its own threads, the \
         virtio-vsock worker among them; see the header of this file",
        slowest.as_secs_f64(),
        EXEC_LIMIT.as_secs(),
        2 * ORPHAN_PAIRS
    );

    // Fast execs prove nothing unless the floods ran the whole time.
    let flood = flood
        .expect("fast execs are followed by the flood probe")
        .context("probing the flooding commands (needs pidof and awk in the container)")?;
    let mut fields = flood.split_whitespace().map(str::parse::<u64>);
    let (Some(Ok(alive)), Some(Ok(cpu_ticks))) = (fields.next(), fields.next()) else {
        panic!("unreadable flood probe output: {flood:?}");
    };
    println!("flooding commands alive: {alive}, CPU they used: {cpu_ticks} ticks");
    assert_eq!(
        alive,
        2 * ORPHAN_PAIRS as u64,
        "the container commands were not all still running when the watch ended, so this \
         run says nothing about the guest's scheduler"
    );
    assert!(
        cpu_ticks >= MIN_FLOOD_TICKS,
        "the container commands used {cpu_ticks} ticks of CPU (at least {MIN_FLOOD_TICKS} \
         expected), so they were not flooding conmon and this run says nothing about the \
         guest's scheduler"
    );
    Ok(())
}

/// Start `timeout FLOOD_SECS yes` in the container, read its first bytes, and
/// kill the client. The command keeps flooding conmon until its timeout.
async fn leave_a_flooding_orphan(fcvm: &Path, pid: u32, with_stdin: bool) -> Result<()> {
    let pid_arg = pid.to_string();
    let flood_secs = FLOOD_SECS.to_string();
    let mut args = vec!["exec", "--pid", &pid_arg];
    if with_stdin {
        args.push("-i");
    }
    args.extend(["--", "timeout", &flood_secs, "yes"]);
    let stdin = if with_stdin {
        Stdio::from(std::fs::File::open("/dev/zero").context("opening /dev/zero")?)
    } else {
        Stdio::null()
    };
    let mut client = tokio::process::Command::new(fcvm)
        .args(&args)
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawning the flooding exec")?;
    let mut first = [0u8; 2];
    let mut stdout = client
        .stdout
        .take()
        .context("the exec's stdout was piped")?;
    tokio::time::timeout(Duration::from_secs(60), stdout.read_exact(&mut first))
        .await
        .context("the flooding exec produced no output within 60 s")?
        .context("reading the flooding exec's output")?;
    assert_eq!(&first, b"y\n", "the flood is `yes`");
    client.kill().await.context("killing the exec client")?;
    Ok(())
}

/// Run `true` through `fcvm exec` and return how long it took.
async fn time_a_trivial_exec(fcvm: &Path, pid: u32, in_vm: bool) -> Result<Duration> {
    let pid_arg = pid.to_string();
    let mut args = vec!["exec", "--pid", &pid_arg];
    if in_vm {
        args.push("--vm");
    }
    args.extend(["--", "true"]);
    let started = Instant::now();
    let client = tokio::process::Command::new(fcvm)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawning a trivial exec")?;
    // Longer than the flood, so a starved guest is reported by its duration
    // and not by this cap.
    let status = tokio::time::timeout(Duration::from_secs(90), client.wait_with_output())
        .await
        .context("a trivial exec did not return within 90 s")?
        .context("waiting for a trivial exec")?
        .status;
    anyhow::ensure!(status.success(), "a trivial exec failed: {status}");
    Ok(started.elapsed())
}
