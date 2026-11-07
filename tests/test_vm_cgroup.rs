//! Each VM runs in a cgroup of its own (issue #1028).
//!
//! `podman run`, `snapshot serve` and `snapshot run` move the fcvm process into
//! `fcvm.slice/fcvm-<pid>-<starttime>.scope` before they start anything, so the VMM, the
//! holder, pasta and the memory server are created there and what they allocate is charged
//! there, not to the cgroup of whoever started fcvm. The scope is removed when fcvm exits,
//! and a scope whose fcvm was killed is removed by the next fcvm's sweep.

#![cfg(feature = "integration-slow")]
#![cfg_attr(not(feature = "privileged-tests"), allow(dead_code, unused_imports))]

mod common;

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// The cgroup2 path of `pid`, from `/proc/<pid>/cgroup`.
fn cgroup_of(pid: u32) -> Result<String> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .with_context(|| format!("reading /proc/{pid}/cgroup"))?;
    text.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(str::to_string)
        .context("no cgroup v2 entry")
}

/// The scope fcvm process `pid` must be in, by the name fcvm gives it.
fn expected_scope(pid: u32) -> Result<PathBuf> {
    let start_time =
        fcvm::utils::process_start_time(pid).with_context(|| format!("process {pid} is gone"))?;
    Ok(Path::new(CGROUP_ROOT)
        .join(fcvm::cgroup::DEFAULT_PARENT)
        .join(fcvm::cgroup::scope_name(pid, start_time)))
}

fn scope_procs(scope: &Path) -> Result<Vec<u32>> {
    let text = std::fs::read_to_string(scope.join("cgroup.procs"))
        .with_context(|| format!("reading {}/cgroup.procs", scope.display()))?;
    Ok(text.lines().filter_map(|l| l.trim().parse().ok()).collect())
}

fn read_u64(path: &Path) -> Result<u64> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    text.trim()
        .parse()
        .with_context(|| format!("parsing {}: {text:?}", path.display()))
}

/// Every process below `pid`, from `/proc/<pid>/task/*/children`.
fn descendants(pid: u32) -> Vec<u32> {
    let mut found = Vec::new();
    let mut queue = vec![pid];
    while let Some(next) = queue.pop() {
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{next}/task")) else {
            continue;
        };
        for task in tasks.flatten() {
            let Ok(children) = std::fs::read_to_string(task.path().join("children")) else {
                continue;
            };
            for child in children.split_whitespace().filter_map(|c| c.parse().ok()) {
                found.push(child);
                queue.push(child);
            }
        }
    }
    found
}

fn comm(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Assert fcvm process `pid` is in its scope, and with it every process it started.
/// Returns the scope and the processes in it.
fn assert_in_own_scope(pid: u32, role: &str) -> Result<(PathBuf, Vec<u32>)> {
    let scope = expected_scope(pid)?;
    let actual = cgroup_of(pid)?;
    let relative = scope.strip_prefix(CGROUP_ROOT).unwrap();
    assert_eq!(
        Path::new(&actual),
        Path::new("/").join(relative),
        "{role} fcvm {pid} is in {actual}, not in its own scope"
    );
    let procs = scope_procs(&scope)?;
    for child in descendants(pid) {
        assert!(
            procs.contains(&child),
            "{role}: process {child} ({}) started by fcvm {pid} is in {}, not in {}",
            comm(child),
            cgroup_of(child).unwrap_or_default(),
            scope.display()
        );
    }
    Ok((scope, procs))
}

/// The first process below `pid` whose command name starts with `name` (pasta runs as
/// `pasta` or as an architecture build such as `pasta.avx2`).
fn find_child(pid: u32, name: &str) -> Result<u32> {
    descendants(pid)
        .into_iter()
        .find(|&p| comm(p).starts_with(name))
        .with_context(|| format!("no {name} process below fcvm {pid}"))
}

/// SIGTERM `child` and wait for it to exit, so fcvm's own teardown runs.
async fn stop(child: &mut tokio::process::Child, pid: u32) -> Result<()> {
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .context("sending SIGTERM")?;
    tokio::time::timeout(Duration::from_secs(60), child.wait())
        .await
        .context("fcvm did not exit within 60 s of SIGTERM")??;
    Ok(())
}

/// Run an fcvm command that enters a scope and fails at once, which makes it sweep.
async fn run_a_sweep() -> Result<()> {
    let fcvm = common::find_fcvm_binary()?;
    let mut command = tokio::process::Command::new(&fcvm);
    command.args([
        "snapshot",
        "run",
        "--snapshot",
        "fcvm-cgroup-test-no-such-snapshot",
    ]);
    common::set_test_pdeathsig(&mut command);
    let output = command.output().await.context("running fcvm")?;
    if output.status.success() {
        bail!("a snapshot run of a missing snapshot succeeded");
    }
    Ok(())
}

async fn wait_until_empty(scope: &Path) -> Result<()> {
    for _ in 0..100 {
        match scope_procs(scope) {
            Ok(procs) if procs.is_empty() => return Ok(()),
            Err(_) => return Ok(()), // already removed
            Ok(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    bail!(
        "{} still holds processes 10 s after its fcvm was killed",
        scope.display()
    )
}

/// The VM, the memory server and a clone each run in a cgroup of their own: the fcvm
/// process and everything it started (Firecracker, the holder, pasta) are in it, the guest's
/// memory is charged to it, `--no-swap` sets its `memory.swap.max` to 0, and the scope is
/// gone after fcvm exits, removed by fcvm itself or, when fcvm was killed, by the next
/// fcvm's sweep.
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_each_vm_and_memory_server_has_its_own_cgroup() -> Result<()> {
    let (baseline_name, clone_name, snapshot_name, _) = common::unique_names("vmcgroup");
    let (mut baseline, baseline_pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &baseline_name,
            "--network",
            "rootless",
            common::TEST_IMAGE,
        ],
        &baseline_name,
    )
    .await?;
    common::poll_health_by_pid(baseline_pid, 180).await?;

    let (baseline_scope, procs) = assert_in_own_scope(baseline_pid, "podman run")?;
    let firecracker = find_child(baseline_pid, "firecracker")?;
    let holder = common::get_holder_pid_for_diag(&common::find_fcvm_binary()?, baseline_pid)
        .await
        .context("no holder pid in fcvm ls")? as u32;
    let pasta = find_child(baseline_pid, "pasta")?;
    for (pid, role) in [
        (firecracker, "firecracker"),
        (holder, "holder"),
        (pasta, "pasta"),
    ] {
        assert!(
            procs.contains(&pid),
            "{role} {pid} is not in {}",
            baseline_scope.display()
        );
    }

    // Guest memory is charged to the scope: 256 MiB written to the guest's tmpfs are new
    // anonymous pages of Firecracker's.
    let before = read_u64(&baseline_scope.join("memory.current"))?;
    common::exec_in_vm(
        baseline_pid,
        &[
            "dd",
            "if=/dev/zero",
            "of=/dev/shm/fcvm-cgroup-fill",
            "bs=1M",
            "count=256",
        ],
    )
    .await?;
    let after = read_u64(&baseline_scope.join("memory.current"))?;
    let grown_mib = after.saturating_sub(before) >> 20;
    println!("  scope memory.current grew {grown_mib} MiB for 256 MiB the guest wrote");
    assert!(
        grown_mib >= 128,
        "the scope's memory.current grew {grown_mib} MiB while the guest wrote 256 MiB \
         ({before} -> {after}); guest memory is charged somewhere else"
    );
    common::exec_in_vm(baseline_pid, &["rm", "-f", "/dev/shm/fcvm-cgroup-fill"]).await?;

    common::create_snapshot_by_pid(baseline_pid, &snapshot_name).await?;

    let serve_log = format!("{snapshot_name}-serve");
    let (mut serve, serve_pid) =
        common::spawn_fcvm_with_logs(&["snapshot", "serve", &snapshot_name], &serve_log).await?;
    common::poll_serve_ready(&snapshot_name, serve_pid, 30).await?;
    let (serve_scope, _) = assert_in_own_scope(serve_pid, "snapshot serve")?;

    let (mut clone, clone_pid) = common::spawn_fcvm_with_logs(
        &[
            "snapshot",
            "run",
            "--pid",
            &serve_pid.to_string(),
            "--name",
            &clone_name,
            "--no-swap",
        ],
        &clone_name,
    )
    .await?;
    common::poll_health_by_pid(clone_pid, 120).await?;
    let (clone_scope, procs) = assert_in_own_scope(clone_pid, "snapshot run")?;
    let clone_firecracker = find_child(clone_pid, "firecracker")?;
    assert!(procs.contains(&clone_firecracker));
    assert_eq!(
        std::fs::read_to_string(clone_scope.join("memory.swap.max"))?.trim(),
        "0",
        "--no-swap left the clone's memory.swap.max unset"
    );
    assert_eq!(
        std::fs::read_to_string(baseline_scope.join("memory.swap.max"))?.trim(),
        "max",
        "a VM without --no-swap had its swap limited"
    );

    // A normal exit removes the scope.
    stop(&mut clone, clone_pid).await?;
    assert!(
        !clone_scope.exists(),
        "{} is still there after its fcvm exited",
        clone_scope.display()
    );
    stop(&mut baseline, baseline_pid).await?;
    assert!(
        !baseline_scope.exists(),
        "{} is still there after its fcvm exited",
        baseline_scope.display()
    );

    // A killed fcvm cannot remove its scope; the next fcvm's sweep does. The kill is reaped
    // first: until then the process exists as a zombie with its start time, and its owner
    // is not gone.
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(serve_pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    )?;
    serve.wait().await?;
    wait_until_empty(&serve_scope).await?;
    run_a_sweep().await?;
    assert!(
        !serve_scope.exists(),
        "{} of a killed memory server survived the next fcvm's sweep",
        serve_scope.display()
    );
    Ok(())
}
