//! TIER 1: deterministic lifecycle interleaving matrix.
//!
//! Each test pins ONE named interleaving of a lifecycle transition (snapshot
//! pause, restore, first-healthy persist) against in-flight client work (exec
//! handshake, port-forward curl, serial output), made deterministic with the
//! `failpoint` crate (host: `FCVM_FAILPOINT`, guest: `FCVM_GUEST_FAILPOINT` →
//! `fcvm_failpoint=` kernel cmdline). Every case is replayable: the failpoint
//! spec plus the marker-line sequencing in the test IS the interleaving.
//!
//! The cases guard the three merged fix branches:
//! - `exec-ready-ack` — three-phase exec handshake (request → ACK → GO): a
//!   request that never got ACKed provably never executed, so resending on a
//!   fresh connection cannot double-execute.
//! - `healthy-after-startup-snapshot` — the health monitor defers persisting
//!   the first Healthy until the startup-snapshot pause/resume completed, so
//!   an observer of Healthy always sees a live dataplane.
//! - `serial-safe-snapshots` — the guest quiesces its console before the
//!   cache-ready notification, so the pre-start snapshot can never capture the
//!   UART mid-transmit (which would poison every restore's serial console).
//! - `cache-verdict-re-ask` — a resumed source's severed cache handshake is
//!   re-asked instead of read as "I was restored": the snapshot save queues a
//!   vsock TRANSPORT_RESET the source processes at resume exactly like a
//!   restored clone would, so the host's verdict — not the transport event —
//!   classifies the VM. Without the re-ask the source never launches its
//!   container (4/4 forced misses hung, ack sent into the dead session).
//!
//! All VMs run rootless (no sudo needed beyond what `make test-root` does for
//! the runner). Test names contain `lifecycle_interleave` so
//! `make test-root FILTER=lifecycle_interleave STREAM=1` runs exactly this file.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Local helpers (marker sequencing, process tracking, oracles)
// ---------------------------------------------------------------------------

/// Search a log file for `needle` at/after byte offset `from`.
/// Returns the byte offset just past the match (a monotone cursor for
/// "this marker, then that marker" sequencing), or None if absent.
fn search_log_from(log: &Path, needle: &str, from: u64) -> Option<u64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(log).ok()?;
    let len = f.metadata().ok()?.len();
    let from = from.min(len);
    f.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let nb = needle.as_bytes();
    buf.windows(nb.len())
        .position(|w| w == nb)
        .map(|i| from + (i + nb.len()) as u64)
}

/// Whole-file substring check.
fn log_contains(log: &Path, needle: &str) -> bool {
    search_log_from(log, needle, 0).is_some()
}

/// Current length of the log file (a cursor for "only look at what comes next").
fn log_len(log: &Path) -> u64 {
    std::fs::metadata(log).map(|m| m.len()).unwrap_or(0)
}

/// Last `n` lines of a log, for failure messages.
fn log_tail(log: &Path, n: usize) -> String {
    let content = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// Poll (25ms) until `needle` appears in `log` at/after `from`; returns the
/// cursor past the match. Fails loudly with the log tail on timeout.
async fn wait_for_marker(log: &Path, needle: &str, from: u64, timeout: Duration) -> Result<u64> {
    let start = Instant::now();
    loop {
        if let Some(pos) = search_log_from(log, needle, from) {
            return Ok(pos);
        }
        if start.elapsed() > timeout {
            anyhow::bail!(
                "marker {:?} not found in {} within {:?}\n--- log tail ---\n{}",
                needle,
                log.display(),
                timeout,
                log_tail(log, 40)
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Wait for a child to exit within `timeout` (definitive: a hang FAILS, it is
/// never masked by a SIGKILL fallback).
async fn wait_exit(
    child: &mut tokio::process::Child,
    timeout: Duration,
) -> Result<std::process::ExitStatus> {
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(res) => res.context("waiting for child"),
        Err(_) => anyhow::bail!("process did not exit within {:?}", timeout),
    }
}

/// Send SIGTERM (graceful shutdown request) to a pid.
async fn sigterm(pid: u32) {
    let _ = tokio::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .output()
        .await;
}

/// `fcvm ls --json --pid` → our VM's state (None while the state file does not
/// exist yet).
async fn ls_vm_by_pid(pid: u32) -> Result<Option<fcvm::state::VmState>> {
    #[derive(serde::Deserialize)]
    struct VmDisplay {
        #[serde(flatten)]
        vm: fcvm::state::VmState,
        #[allow(dead_code)]
        stale: bool,
    }

    let fcvm_path = common::find_fcvm_binary()?;
    let output = tokio::process::Command::new(&fcvm_path)
        .args(["ls", "--json", "--pid", &pid.to_string()])
        .output()
        .await
        .context("running fcvm ls")?;
    if !output.status.success() {
        return Ok(None);
    }
    let vms: Vec<VmDisplay> = match serde_json::from_str(&String::from_utf8_lossy(&output.stdout)) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    Ok(vms.into_iter().next().map(|d| d.vm))
}

/// Parse `/proc/<pid>/stat` → (comm, state, ppid, start_time). `comm` may
/// contain spaces/parens, so split on the LAST `)`.
fn proc_stat(pid: u32) -> Option<(String, char, u32, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat.get(open + 1..close)?.to_string();
    let rest: Vec<&str> = stat.get(close + 1..)?.split_whitespace().collect();
    let state = rest.first()?.chars().next()?;
    let ppid: u32 = rest.get(1)?.parse().ok()?;
    // start_time is field 22 overall = index 19 after "pid (comm) "
    let start: u64 = rest.get(19)?.parse().ok()?;
    Some((comm, state, ppid, start))
}

/// Walk the parent chain (bounded) to test ancestry.
fn is_descendant_of(pid: u32, ancestor: u32) -> bool {
    let mut cur = pid;
    for _ in 0..15 {
        if cur == ancestor {
            return true;
        }
        if cur <= 1 {
            return false;
        }
        match proc_stat(cur) {
            Some((_, _, ppid, _)) => cur = ppid,
            None => return false,
        }
    }
    false
}

/// A process pinned by (pid, start_time): immune to PID reuse and to the
/// post-kill zombie window (Z/X states count as gone).
struct TrackedProc {
    pid: u32,
    start: u64,
    comm: String,
}

impl TrackedProc {
    fn alive(&self) -> bool {
        matches!(
            proc_stat(self.pid),
            Some((_, state, _, start)) if start == self.start && !matches!(state, 'Z' | 'X' | 'x')
        )
    }
}

/// All live descendants of `ancestor` whose comm starts with `prefix`
/// (comm is truncated to 15 chars, so match by prefix — e.g. the firecracker
/// binary `firecracker-default-<sha>.bin` shows as `firecracker-def`).
fn find_descendants_with_comm_prefix(ancestor: u32, prefix: &str) -> Vec<TrackedProc> {
    let mut found = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return found;
    };
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Some((comm, state, _, start)) = proc_stat(pid) else {
            continue;
        };
        if comm.starts_with(prefix)
            && !matches!(state, 'Z' | 'X' | 'x')
            && is_descendant_of(pid, ancestor)
        {
            found.push(TrackedProc { pid, start, comm });
        }
    }
    found
}

/// Does any state file in the state dir belong to `vm_name`?
fn state_file_for_name_exists(vm_name: &str) -> bool {
    let Ok(rd) = std::fs::read_dir(fcvm::paths::state_dir()) else {
        return false;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&p) else {
            continue;
        };
        if let Ok(state) = serde_json::from_str::<fcvm::state::VmState>(&content) {
            if state.name.as_deref() == Some(vm_name) {
                return true;
            }
        }
    }
    false
}

#[derive(Debug, Clone)]
struct VmArtifacts {
    vm_id: String,
    state: fcvm::state::VmState,
    state_path: PathBuf,
    lock_path: PathBuf,
    data_dir: PathBuf,
}

/// An exact process identity captured immediately after spawn. Signals go
/// through the pidfd, so a recycled numeric PID can never target a stranger.
#[derive(Debug)]
struct PinnedProcess {
    pid: u32,
    start: u64,
    pidfd: OwnedFd,
}

impl PinnedProcess {
    fn capture(pid: u32) -> Result<Self> {
        let before = proc_stat(pid)
            .map(|(_, _, _, start)| start)
            .with_context(|| format!("PID {pid} exited before its identity could be captured"))?;

        // SAFETY: pidfd_open(2) has no memory effects. The returned fd is
        // checked and immediately transferred into OwnedFd.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("pidfd_open on spawned PID {pid}"));
        }
        // SAFETY: `raw` is a fresh fd returned above and has not been shared.
        let pidfd = unsafe { OwnedFd::from_raw_fd(raw as i32) };

        let after = proc_stat(pid).map(|(_, _, _, start)| start);
        anyhow::ensure!(
            after == Some(before),
            "PID {pid} exited or was recycled while its identity was being captured"
        );
        Ok(Self {
            pid,
            start: before,
            pidfd,
        })
    }

    fn signal(&self, signal: libc::c_int) -> Result<()> {
        // SAFETY: this fd is owned by `self`; a NULL siginfo asks the kernel to
        // synthesize it, and flags must be zero for pidfd_send_signal(2).
        let rc = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd() as libc::c_long,
                signal as libc::c_long,
                std::ptr::null::<libc::siginfo_t>(),
                0 as libc::c_long,
            )
        };
        if rc < 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!(
                    "sending signal {signal} through pidfd for PID {} (start time {})",
                    self.pid, self.start
                )
            });
        }
        Ok(())
    }
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// Find the one state file belonging to `vm_name` and derive its exact lock
/// and runtime-data paths from the deserialized VM ID. State files are keyed by
/// VM ID, not name or PID; in particular, a rootless clone parked at either
/// restore failpoint (`restore.pre_resume`, `restore.post_network_pre_resume`)
/// intentionally still has `pid = null`, because the PID is published only
/// after the VMM resumes.
fn artifacts_for_name(vm_name: &str) -> Result<Option<VmArtifacts>> {
    let state_dir = fcvm::paths::state_dir();
    let mut found = None;

    for entry in std::fs::read_dir(&state_dir)
        .with_context(|| format!("reading state directory {}", state_dir.display()))?
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).context("reading state directory entry"),
        };
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }

        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(e).with_context(|| format!("reading state file {}", path.display()))
            }
        };
        let state: fcvm::state::VmState = serde_json::from_str(&text)
            .with_context(|| format!("deserializing state file {}", path.display()))?;
        if state.name.as_deref() != Some(vm_name) {
            continue;
        }

        anyhow::ensure!(
            found.is_none(),
            "multiple state files claim unique VM name {vm_name}"
        );
        anyhow::ensure!(
            !state.vm_id.is_empty(),
            "state file {} for {vm_name} has an empty vm_id",
            path.display()
        );
        anyhow::ensure!(
            path == state_dir.join(format!("{}.json", state.vm_id)),
            "state file {} does not match its recorded vm_id {}",
            path.display(),
            state.vm_id
        );

        let lock_path = path.with_extension("json.lock");
        let data_dir = fcvm::paths::vm_runtime_dir(&state.vm_id);
        let vm_disks = fcvm::paths::data_dir().join("vm-disks");
        anyhow::ensure!(
            data_dir.parent() == Some(vm_disks.as_path()) && data_dir != vm_disks,
            "refusing to treat {} as one VM's runtime directory",
            data_dir.display()
        );
        found = Some(VmArtifacts {
            vm_id: state.vm_id.clone(),
            state,
            state_path: path,
            lock_path,
            data_dir,
        });
    }

    Ok(found)
}

/// Wait until the clone has published all three exact artifacts that its
/// orderly shutdown must reap. The child exiting before publication is a hard
/// failure rather than a reason to skip the cleanup assertions.
async fn wait_for_artifacts(
    vm_name: &str,
    child: &mut tokio::process::Child,
    timeout: Duration,
) -> Result<VmArtifacts> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(artifacts) = artifacts_for_name(vm_name)? {
            if artifacts.state_path.is_file()
                && artifacts.lock_path.is_file()
                && artifacts.data_dir.is_dir()
            {
                return Ok(artifacts);
            }
        }
        if let Some(status) = child.try_wait()? {
            anyhow::bail!(
                "fcvm exited with {status:?} before publishing state, lock, and data for {vm_name}"
            );
        }
        if Instant::now() > deadline {
            anyhow::bail!(
                "state, lock, and data for {vm_name} were not all present within {timeout:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// `cleanup_vm` removes all three paths synchronously before fcvm exits, but
/// poll briefly so the log-pump and filesystem visibility cannot make the
/// assertion timing-dependent under load.
async fn wait_for_artifacts_removed(artifacts: &VmArtifacts, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let state_exists = artifacts.state_path.exists();
        let lock_exists = artifacts.lock_path.exists();
        let data_exists = artifacts.data_dir.exists();
        if !state_exists && !lock_exists && !data_exists {
            return Ok(());
        }
        if Instant::now() > deadline {
            anyhow::bail!(
                "clone {} left artifacts after orderly exit: state={}, lock={}, data={}\n\
                 state_path={}\nlock_path={}\ndata_dir={}",
                artifacts.vm_id,
                state_exists,
                lock_exists,
                data_exists,
                artifacts.state_path.display(),
                artifacts.lock_path.display(),
                artifacts.data_dir.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Fixture shutdown used after both success and failure. It returns only after
/// `Child::wait` has reaped the exact pinned process; callers must not remove VM
/// files unless this proof succeeds.
async fn stop_test_child(child: &mut tokio::process::Child, process: &PinnedProcess) -> Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }

    let term_result = process.signal(libc::SIGTERM);
    if let Ok(waited) = tokio::time::timeout(Duration::from_secs(30), child.wait()).await {
        waited.context("waiting for fixture child after SIGTERM")?;
        ignore_esrch_after_confirmed_exit(term_result)?;
        return Ok(());
    }

    let kill_result = process.signal(libc::SIGKILL);
    match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
        Ok(waited) => {
            waited.context("waiting for fixture child after SIGKILL")?;
            ignore_esrch_after_confirmed_exit(term_result)?;
            ignore_esrch_after_confirmed_exit(kill_result)?;
            Ok(())
        }
        Err(_) => anyhow::bail!(
            "fixture PID {} (start time {}) did not exit after pidfd SIGTERM and SIGKILL",
            process.pid,
            process.start
        ),
    }
}

fn ignore_esrch_after_confirmed_exit(signal_result: Result<()>) -> Result<()> {
    match signal_result {
        Err(error)
            if error.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io_error| io_error.raw_os_error() == Some(libc::ESRCH))
            }) =>
        {
            Ok(())
        }
        other => other,
    }
}

#[test]
fn confirmed_exit_ignores_only_pidfd_esrch() {
    let esrch = Err(anyhow::Error::new(std::io::Error::from_raw_os_error(
        libc::ESRCH,
    )))
    .context("sending SIGTERM through pidfd");
    ignore_esrch_after_confirmed_exit(esrch).expect("ESRCH means the pinned child is gone");

    let eperm = Err(anyhow::Error::new(std::io::Error::from_raw_os_error(
        libc::EPERM,
    )))
    .context("sending SIGTERM through pidfd");
    let error = ignore_esrch_after_confirmed_exit(eperm)
        .expect_err("non-ESRCH signal failures must remain visible");
    assert!(
        error.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io_error| io_error.raw_os_error() == Some(libc::EPERM))
        }),
        "unexpected error: {error:#}"
    );
}

/// Remove only a previously deserialized clone's exact paths after a failed
/// assertion. This is test-fixture hygiene, never part of the passing oracle.
fn cleanup_exact_artifacts(artifacts: &VmArtifacts) -> Result<()> {
    let state_dir = fcvm::paths::state_dir();
    let vm_disks = fcvm::paths::data_dir().join("vm-disks");
    anyhow::ensure!(
        artifacts.state_path == state_dir.join(format!("{}.json", artifacts.vm_id))
            && artifacts.lock_path == state_dir.join(format!("{}.json.lock", artifacts.vm_id))
            && artifacts.data_dir == vm_disks.join(&artifacts.vm_id)
            && !artifacts.vm_id.is_empty(),
        "refusing cleanup for paths that are not one exact VM's artifacts: {artifacts:?}"
    );

    for path in [&artifacts.state_path, &artifacts.lock_path] {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }
    match std::fs::remove_dir_all(&artifacts.data_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| format!("removing {}", artifacts.data_dir.display()))
        }
    }
    Ok(())
}

/// Spawn `fcvm exec --pid <pid> --vm -- sh -c <script>` with RUST_LOG=debug and
/// captured stdio, so the test can assert on the client's handshake debug lines
/// ("ACK received", "reconnecting to resend").
fn spawn_exec_capture(vm_pid: u32, script: &str) -> Result<tokio::process::Child> {
    let fcvm_path = common::find_fcvm_binary()?;
    let mut cmd = tokio::process::Command::new(fcvm_path);
    cmd.args([
        "exec",
        "--pid",
        &vm_pid.to_string(),
        "--vm",
        "--",
        "sh",
        "-c",
        script,
    ])
    .env("RUST_LOG", "debug")
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    cmd.spawn().context("spawning fcvm exec")
}

/// Exactly-once oracle for the nonce file written through a `--map`ed volume:
/// wait (≤10s) for the nonce to appear, then a 2s settle so a phantom second
/// execution would have landed too, then count matching lines.
async fn settled_nonce_count(file: &Path, nonce: &str) -> usize {
    let count = |content: String| content.lines().filter(|l| l.trim() == nonce).count();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let c = tokio::fs::read_to_string(file)
            .await
            .map(&count)
            .unwrap_or(0);
        if c > 0 || Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    tokio::fs::read_to_string(file)
        .await
        .map(&count)
        .unwrap_or(0)
}

/// Best-effort removal of snapshot cache entries this test created (unique keys
/// per run, so nothing else can be restoring them). Also removes `.creating`
/// temp dirs left by an interrupted create.
async fn cleanup_snapshot_keys(keys: &[Option<String>]) {
    for key in keys.iter().flatten() {
        let _ = common::delete_snapshot(key).await;
        let _ = common::delete_snapshot(&format!("{}-startup", key)).await;
        let _ = tokio::fs::remove_dir_all(
            fcvm::paths::snapshot_dir().join(format!("{}.creating", key)),
        )
        .await;
    }
}

/// Count non-overlapping occurrences of `needle` in `hay`.
fn count_occurrences(hay: &str, needle: &str) -> usize {
    hay.match_indices(needle).count()
}

// ---------------------------------------------------------------------------
// CASE: exec_resend_across_agent_stall
// ---------------------------------------------------------------------------

/// Pins: the exec handshake RESEND path against a REAL VM — an exec request
/// orphaned by a snapshot pause is resent on a fresh connection and executes
/// exactly once (fixed by the `exec-ready-ack` branch: request → ACK → GO;
/// fc-agent never executes before consuming GO, so an un-ACKed request is
/// provably safe to resend).
///
/// Interleaving (fully marker-sequenced, no timing guesses):
/// 1. Guest arms `exec.post_accept_pre_read:sleep:2000` — every exec accept
///    parks 2s before reading the request. 2s is deliberately UNDER the
///    client's 3s ACK timeout: a stall above 3s would time out every one of
///    the 5 resend attempts by construction (each accept re-parks), so the
///    exec could never exit 0 — the pause below, not the stall alone, is what
///    forces the resend.
/// 2. Host arms `snapshot.pre_pause:block_until_file` on a `fcvm snapshot
///    create` process, which parks fully-prepared, right before the Pause API
///    call ("FAILPOINT snapshot.pre_pause reached" on its stderr).
/// 3. One exec (nonce append via a --map'd dir) is launched; its accept marker
///    in the VM log proves connection #1 is parked INSIDE the agent's window.
/// 4. The go-file is created → the pause lands within the ~1.8s left of the
///    guest hold → the snapshot's vsock reset orphans connection #1 (its
///    request was consumed by nobody: the agent was parked pre-read).
/// 5. The client gets no ACK → reconnects and resends; the post-resume accept
///    parks 2s again, ACKs at +2s (< 3s timeout), GO authorizes execution.
///
/// Oracle: exec exits 0; the client log shows the resend actually happened;
/// the nonce appears EXACTLY once (the double-execution tripwire: if fc-agent
/// ever executed connection #1's request without GO, the count would be 2).
#[tokio::test]
async fn test_lifecycle_interleave_exec_resend_across_agent_stall() -> Result<()> {
    let (vm_name, _, snap_tag, _) = common::unique_names("ilv-resend");
    let host_dir = PathBuf::from(format!("/tmp/{}-map", vm_name));
    std::fs::create_dir_all(&host_dir)?;
    let map_arg = format!("{}:/mnt/test", host_dir.display());
    // Unique env → unique snapshot key → deterministic cold boot every run.
    let env_unique = format!("ILV_ID={}", vm_name);

    let (mut child, pid, vm_log) = common::spawn_fcvm_with_env_and_log_path(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--map",
            &map_arg,
            "--env",
            &env_unique,
            // HTTP health checks: with a health URL the monitor never uses
            // `fcvm exec` (podman-inspect) probes, so the ONLY exec.* failpoint
            // hits in the VM log are this test's own exec.
            "--health-check",
            "http://localhost/",
            common::TEST_IMAGE,
        ],
        &[(
            "FCVM_GUEST_FAILPOINT",
            "exec.post_accept_pre_read:sleep:2000",
        )],
    )
    .await?;

    common::poll_health_by_pid(pid, 300).await?;
    let base_key = ls_vm_by_pid(pid)
        .await?
        .and_then(|s| s.config.snapshot_name);

    // Park a manual snapshot create right before its Pause call.
    let go_file = PathBuf::from(format!("/tmp/{}-go", vm_name));
    let _ = std::fs::remove_file(&go_file);
    let pid_str = pid.to_string();
    let failpoint_spec = format!("snapshot.pre_pause:block_until_file:{}", go_file.display());
    let (mut create_child, _create_pid, create_log) = common::spawn_fcvm_with_env_and_log_path(
        &["snapshot", "create", "--pid", &pid_str, "--tag", &snap_tag],
        &[("FCVM_FAILPOINT", &failpoint_spec)],
    )
    .await?;
    wait_for_marker(
        &create_log,
        "FAILPOINT snapshot.pre_pause reached",
        0,
        Duration::from_secs(90),
    )
    .await?;

    // Launch the exec and wait until its connection is parked inside the
    // agent's post-accept window (manual creates do NOT quiesce the guest
    // console, so the guest marker streams to the VM log immediately).
    let nonce = format!("nonce-{}", vm_name);
    let script = format!("echo {} >> /mnt/test/nonce.txt", nonce);
    let vm_cursor = log_len(&vm_log);
    let exec_child = spawn_exec_capture(pid, &script)?;
    wait_for_marker(
        &vm_log,
        "FAILPOINT exec.post_accept_pre_read reached",
        vm_cursor,
        Duration::from_secs(30),
    )
    .await?;

    // Release the pause INTO the parked window.
    std::fs::write(&go_file, b"go").context("creating go-file")?;

    let create_status = wait_exit(&mut create_child, Duration::from_secs(180)).await?;
    let exec_output = tokio::time::timeout(Duration::from_secs(120), exec_child.wait_with_output())
        .await
        .context("exec did not finish within 120s")?
        .context("collecting exec output")?;
    let exec_stderr = String::from_utf8_lossy(&exec_output.stderr).to_string();
    let nonce_count = settled_nonce_count(&host_dir.join("nonce.txt"), &nonce).await;

    // Teardown before asserting so a failed assert can't leak the VM.
    let _ = std::fs::remove_file(&go_file);
    common::kill_process(pid).await;
    let _ = child.wait().await;
    cleanup_snapshot_keys(&[base_key, Some(snap_tag.clone())]).await;
    let _ = std::fs::remove_dir_all(&host_dir);

    assert!(
        create_status.success(),
        "snapshot create must succeed (status {:?}); create log tail:\n{}",
        create_status,
        log_tail(&create_log, 40)
    );
    assert!(
        exec_output.status.success(),
        "exec must exit 0 after the resend (status {:?}); exec stderr:\n{}",
        exec_output.status,
        exec_stderr
    );
    assert!(
        exec_stderr.contains("reconnecting to resend"),
        "the pause must orphan connection #1 and force a resend — without it \
         this test pins nothing; exec stderr:\n{}",
        exec_stderr
    );
    assert!(
        exec_stderr.contains("ACK received, sending GO"),
        "the resent request must complete the ACK/GO handshake; exec stderr:\n{}",
        exec_stderr
    );
    assert_eq!(
        nonce_count, 1,
        "nonce must appear EXACTLY once (double-execution tripwire); exec stderr:\n{}",
        exec_stderr
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// CASE: go_floor_across_ack_stall
// ---------------------------------------------------------------------------

/// Pins: the GO read's 2s fresh floor after a long post-ACK stall (part of the
/// `exec-ready-ack` branch). fc-agent's whole handshake shares one 10s
/// deadline; the GO read recomputes `go_deadline = deadline.max(now + 2s)`
/// with `now` taken AFTER the `exec.post_ack_pre_go` hold. The client sends GO
/// immediately after ACK, so the GO bytes sit in the socket buffer across the
/// stall.
///
/// The stall is 12s — deliberately LONGER than the agent's 10s handshake
/// deadline. A shorter stall (e.g. 4s) leaves ~6s of the shared deadline and
/// passes even WITHOUT the floor, pinning nothing. At 12s the deadline is
/// exhausted when the hold releases: only the recomputed 2s floor lets the
/// agent consume the buffered GO. If the floor regresses (deadline computed
/// before the hold), the agent closes without executing and the client
/// surfaces a loud post-GO error → exit != 0 → this test fails.
///
/// Oracle: exec exits 0, the nonce appears exactly once, and the client log
/// shows a single handshake attempt (ACK on attempt 1, no resend).
#[tokio::test]
async fn test_lifecycle_interleave_go_floor_across_ack_stall() -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names("ilv-gofloor");
    let host_dir = PathBuf::from(format!("/tmp/{}-map", vm_name));
    std::fs::create_dir_all(&host_dir)?;
    let map_arg = format!("{}:/mnt/test", host_dir.display());
    let env_unique = format!("ILV_ID={}", vm_name);

    let (mut child, pid, vm_log) = common::spawn_fcvm_with_env_and_log_path(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--map",
            &map_arg,
            "--env",
            &env_unique,
            "--health-check",
            "http://localhost/",
            common::TEST_IMAGE,
        ],
        &[("FCVM_GUEST_FAILPOINT", "exec.post_ack_pre_go:sleep:12000")],
    )
    .await?;

    common::poll_health_by_pid(pid, 300).await?;
    let base_key = ls_vm_by_pid(pid)
        .await?
        .and_then(|s| s.config.snapshot_name);

    let nonce = format!("nonce-{}", vm_name);
    let script = format!("echo {} >> /mnt/test/nonce.txt", nonce);
    let vm_cursor = log_len(&vm_log);
    let exec_child = spawn_exec_capture(pid, &script)?;

    // Non-vacuity: the stall must actually be armed and hit.
    wait_for_marker(
        &vm_log,
        "FAILPOINT exec.post_ack_pre_go reached",
        vm_cursor,
        Duration::from_secs(30),
    )
    .await?;

    let exec_output = tokio::time::timeout(Duration::from_secs(90), exec_child.wait_with_output())
        .await
        .context("exec did not finish within 90s (12s stall + command)")?
        .context("collecting exec output")?;
    let exec_stderr = String::from_utf8_lossy(&exec_output.stderr).to_string();
    let nonce_count = settled_nonce_count(&host_dir.join("nonce.txt"), &nonce).await;

    common::kill_process(pid).await;
    let _ = child.wait().await;
    cleanup_snapshot_keys(&[base_key]).await;
    let _ = std::fs::remove_dir_all(&host_dir);

    assert!(
        exec_output.status.success(),
        "exec must exit 0 across the 12s post-ACK stall (2s GO floor); \
         status {:?}; exec stderr:\n{}",
        exec_output.status,
        exec_stderr
    );
    assert_eq!(
        count_occurrences(&exec_stderr, "ACK received, sending GO"),
        1,
        "exactly one handshake attempt (ACK on the first connection); exec stderr:\n{}",
        exec_stderr
    );
    assert!(
        !exec_stderr.contains("reconnecting to resend")
            && !exec_stderr.contains("never acknowledged"),
        "no resend may happen — ACK arrived before the stall; exec stderr:\n{}",
        exec_stderr
    );
    assert_eq!(
        nonce_count, 1,
        "nonce must appear exactly once; exec stderr:\n{}",
        exec_stderr
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// CASE: healthy_means_live_dataplane
// ---------------------------------------------------------------------------

/// Pins: the first-healthy gate (`healthy-after-startup-snapshot` branch).
/// The first healthy check triggers startup-snapshot creation, which PAUSES
/// the VM; the fix makes the health monitor defer persisting Healthy until
/// that pause/resume cycle completed and was acked. So the very first
/// externally observable Healthy implies running vCPUs and a live dataplane.
///
/// `snapshot.pre_pause:sleep:3000` in the VM process inflates every snapshot
/// pause window to ≥3s (pre-start AND startup snapshot). The test polls the
/// state file every 50ms and, at the FIRST observation of Healthy, fires a
/// single `curl --max-time 2` at the forwarded loopback ip:port. If the gate
/// ever regresses (Healthy persisted before/during the startup-snapshot
/// pause), the 50ms poll lands inside the ≥3s frozen window and the one-shot
/// 2s curl deterministically times out.
///
/// This test's SUBJECT is the startup-snapshot lifecycle, so it opts into
/// snapshots explicitly (`spawn_fcvm_snapshots_enabled_*`) instead of
/// inheriting the lane's env — the SnapshotDisabled CI lane's
/// `FCVM_NO_SNAPSHOT=1` covers the no-snapshot path of ORDINARY tests and
/// must not silently turn off the very pause this test pins.
#[tokio::test]
async fn test_lifecycle_interleave_healthy_means_live_dataplane() -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names("ilv-healthy");
    // Unique env → unique base AND startup snapshot keys → the startup
    // snapshot is CREATED this run (a warm start would skip the pause and
    // make this test vacuous).
    let env_unique = format!("ILV_ID={}", vm_name);
    let host_port = common::find_available_high_port()?;
    let publish_arg = format!("{}:80", host_port);

    let (mut child, pid, vm_log) = common::spawn_fcvm_snapshots_enabled_with_env_and_log_path(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--publish",
            &publish_arg,
            "--env",
            &env_unique,
            "--health-check",
            "http://localhost/",
            common::TEST_IMAGE,
        ],
        &[("FCVM_FAILPOINT", "snapshot.pre_pause:sleep:3000")],
    )
    .await?;

    // Discover vm_id + loopback IP as soon as the state file exists (long
    // before healthy), then poll the state FILE directly at 50ms — `fcvm ls`
    // subprocesses are too slow to pin a 3s window.
    let discover_deadline = Instant::now() + Duration::from_secs(180);
    let (vm_id, loopback_ip) = loop {
        if let Some(state) = ls_vm_by_pid(pid).await? {
            if let Some(ip) = state.config.network.loopback_ip.clone() {
                break (state.vm_id, ip);
            }
        }
        if child.try_wait()?.is_some() {
            anyhow::bail!(
                "fcvm exited before creating VM state; log tail:\n{}",
                log_tail(&vm_log, 40)
            );
        }
        if Instant::now() > discover_deadline {
            anyhow::bail!("VM state with loopback_ip never appeared");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let state_path = fcvm::paths::state_dir().join(format!("{}.json", vm_id));

    // 50ms poll for the FIRST Healthy observation.
    let poll_deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let healthy = std::fs::read_to_string(&state_path)
            .ok()
            .and_then(|c| serde_json::from_str::<fcvm::state::VmState>(&c).ok())
            .map(|s| s.health_status == fcvm::state::HealthStatus::Healthy)
            .unwrap_or(false);
        if healthy {
            break;
        }
        if child.try_wait()?.is_some() {
            anyhow::bail!(
                "fcvm exited before Healthy; log tail:\n{}",
                log_tail(&vm_log, 40)
            );
        }
        if Instant::now() > poll_deadline {
            anyhow::bail!(
                "VM never became Healthy; log tail:\n{}",
                log_tail(&vm_log, 40)
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // FIRST observation of Healthy → immediately one single-shot curl.
    let curl = tokio::process::Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            "2",
            &format!("http://{}:{}/", loopback_ip, host_port),
        ])
        .output()
        .await
        .context("running curl")?;
    let curl_ok = curl.status.success() && !curl.stdout.is_empty();
    let curl_err = String::from_utf8_lossy(&curl.stderr).to_string();

    // Non-vacuity: this run must actually have created the startup snapshot
    // (the pause the gate defers Healthy across) with the inflated window.
    let created_startup = log_contains(&vm_log, "Creating startup snapshot");
    let prepause_hit = log_contains(&vm_log, "FAILPOINT snapshot.pre_pause reached");
    let base_key = ls_vm_by_pid(pid)
        .await?
        .and_then(|s| s.config.snapshot_name);

    common::kill_process(pid).await;
    let _ = child.wait().await;
    cleanup_snapshot_keys(&[base_key]).await;

    assert!(
        created_startup,
        "the startup snapshot must be created THIS run (cold boot) or the gate \
         is never exercised; log tail:\n{}",
        log_tail(&vm_log, 40)
    );
    assert!(
        prepause_hit,
        "the snapshot.pre_pause hold must have fired (failpoint armed); log tail:\n{}",
        log_tail(&vm_log, 40)
    );
    assert!(
        curl_ok,
        "single-shot curl at the FIRST observed Healthy must succeed — Healthy \
         with a paused/frozen dataplane is the exact bug the healthy-after-\
         startup-snapshot fix removed (curl status {:?}, stderr: {})",
        curl.status, curl_err
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// CASE: quiesce_survives_long_prepause
// ---------------------------------------------------------------------------

/// Pins: serial-safe pre-start snapshot creation (`serial-safe-snapshots`
/// branch). The guest quiesces its console (flush + gate + TIOCOUTQ drain)
/// BEFORE sending cache-ready; the host's pre-start snapshot pause can
/// therefore never capture the UART mid-transmit. A mid-transmit capture
/// poisons the snapshot: every restore has a dead serial console (the guest
/// 8250 driver waits forever for a TX interrupt that never comes).
///
/// Interleaving: the container spams console output; the guest holds 1s
/// between quiesce and the cache-ready send (`cache_ready.pre_send:sleep:1000`)
/// and the host holds 2s more before the pause (`snapshot.pre_pause:sleep:2000`)
/// — a ≥3s window in which un-quiesced output WOULD put bytes in the UART.
///
/// Run 1 (unique cmd → cold) CREATES the pre-start snapshot inside that
/// widened window. Run 2 (same cmd/env → same key) RESTORES it. Oracle on run
/// 2: the restore path was taken (no silent cold-boot fallback), fc-agent
/// serial lines and the container's console output appear after restore, the
/// dead-serial watchdog ERROR ("captured the guest UART mid-transmit") never
/// fires, the run reaches healthy, and SIGTERM produces an orderly exit.
///
/// Both runs' SUBJECT is the pre-start snapshot lifecycle, so they opt into
/// snapshots explicitly (`spawn_fcvm_snapshots_enabled_*`) — the
/// SnapshotDisabled lane's `FCVM_NO_SNAPSHOT=1` must not leak in and remove
/// the create/restore path this test pins.
#[tokio::test]
async fn test_lifecycle_interleave_quiesce_survives_long_prepause() -> Result<()> {
    let (run1_name, run2_name, _, _) = common::unique_names("ilv-quiesce");
    let canary = format!("serial-canary-{}", run1_name);
    // Unique canary in the cmd → unique snapshot key → run 1 always cold.
    let script = format!(
        "i=0; while true; do echo {} $i; i=$((i+1)); sleep 1; done",
        canary
    );
    let env: [(&str, &str); 2] = [
        ("FCVM_FAILPOINT", "snapshot.pre_pause:sleep:2000"),
        ("FCVM_GUEST_FAILPOINT", "cache_ready.pre_send:sleep:1000"),
    ];

    // --- Run 1: cold boot, creates the pre-start snapshot inside the widened
    // quiesce window, and must itself have a live serial console.
    let (mut child1, pid1, log1) = common::spawn_fcvm_snapshots_enabled_with_env_and_log_path(
        &[
            "podman",
            "run",
            "--name",
            &run1_name,
            common::ALPINE_IMAGE,
            "sh",
            "-c",
            &script,
        ],
        &env,
    )
    .await?;
    common::poll_health_by_pid(pid1, 300).await?;
    wait_for_marker(
        &log1,
        "Pre-start snapshot created successfully",
        0,
        Duration::from_secs(60),
    )
    .await?;
    // The console guard buffers the guest hold's marker during the quiesce and
    // flushes it afterwards — its presence proves the guest hold really ran.
    wait_for_marker(
        &log1,
        "FAILPOINT cache_ready.pre_send reached",
        0,
        Duration::from_secs(60),
    )
    .await?;
    wait_for_marker(&log1, &canary, 0, Duration::from_secs(30)).await?;
    let run1_cold = log_contains(&log1, "Snapshot miss");
    let run1_prepause = log_contains(&log1, "FAILPOINT snapshot.pre_pause reached");
    let base_key = ls_vm_by_pid(pid1)
        .await?
        .and_then(|s| s.config.snapshot_name);
    common::kill_process(pid1).await;
    let _ = child1.wait().await;

    assert!(run1_cold, "run 1 must be a cold boot (unique cmd)");
    assert!(
        run1_prepause,
        "run 1 must hit the inflated snapshot.pre_pause hold"
    );

    // --- Run 2: same cmd + env → pre-start snapshot hit → restore.
    let (mut child2, pid2, log2) = common::spawn_fcvm_snapshots_enabled_with_env_and_log_path(
        &[
            "podman",
            "run",
            "--name",
            &run2_name,
            common::ALPINE_IMAGE,
            "sh",
            "-c",
            &script,
        ],
        &env,
    )
    .await?;
    let restore_hit = wait_for_marker(
        &log2,
        "Pre-start snapshot hit! Restoring from cached snapshot",
        0,
        Duration::from_secs(90),
    )
    .await;
    if let Err(e) = restore_hit {
        common::kill_process(pid2).await;
        let _ = child2.wait().await;
        cleanup_snapshot_keys(std::slice::from_ref(&base_key)).await;
        return Err(e.context("run 2 must restore the pre-start snapshot"));
    }
    common::poll_health_by_pid(pid2, 300).await?;
    // fc-agent serial lines after restore: the quiesce guard's buffered lines
    // and restore-progress lines all arrive over the restored UART.
    wait_for_marker(&log2, "[fc-agent]", 0, Duration::from_secs(30)).await?;
    // The container's console output flows after restore too.
    wait_for_marker(&log2, &canary, 0, Duration::from_secs(90)).await?;

    let watchdog_fired = log_contains(&log2, "captured the guest UART mid-transmit");
    let run2_fell_back_cold = log_contains(&log2, "Snapshot miss");

    // Orderly shutdown is part of the oracle: raw SIGTERM, no SIGKILL fallback.
    sigterm(pid2).await;
    let exit2 = wait_exit(&mut child2, Duration::from_secs(60)).await;
    cleanup_snapshot_keys(&[base_key]).await;

    assert!(
        !watchdog_fired,
        "the dead-serial watchdog fired: the pre-start snapshot captured the \
         UART mid-transmit — the serial-safe-snapshots quiesce regressed; \
         run 2 log tail:\n{}",
        log_tail(&log2, 40)
    );
    assert!(
        !run2_fell_back_cold,
        "run 2 silently fell back to a cold boot instead of restoring; \
         log tail:\n{}",
        log_tail(&log2, 40)
    );
    let exit2 = exit2.context("run 2 must exit in bounded time after SIGTERM")?;
    println!("run 2 exited after SIGTERM with {:?}", exit2);
    Ok(())
}

// ---------------------------------------------------------------------------
// CASE: no_leaks_after_interleaved_teardown
// ---------------------------------------------------------------------------

/// Pins: teardown racing a HELD snapshot pause window leaks nothing. SIGTERM
/// is delivered while the VM process is parked inside the pre-start snapshot's
/// `snapshot.pre_pause` hold (sequenced on the failpoint marker, and proven by
/// the "released" marker not yet being in the log at signal time). The hold is
/// 10s: wide enough that the harness's marker-to-signal latency can never
/// escape the window.
///
/// By design the in-flight snapshot runs to completion after the signal
/// (dropping the create future between pause and resume would wedge the VM —
/// see `create_snapshot_interruptible`), then shutdown proceeds. The oracle is
/// the postcondition of the exec-orphaned/serial-safe/healthy-gate era
/// teardown work: the process exits within a bounded window WITHOUT SIGKILL,
/// and afterwards this VM's firecracker and pasta processes are gone and its
/// state file is deleted.
///
/// The interleaving under test IS the pre-start snapshot's pause window, so
/// the VM opts into snapshots explicitly (`spawn_fcvm_snapshots_enabled_*`) —
/// under the SnapshotDisabled lane's inherited `FCVM_NO_SNAPSHOT=1` the
/// snapshot.pre_pause hold would never be reached.
#[tokio::test]
async fn test_lifecycle_interleave_no_leaks_after_interleaved_teardown() -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names("ilv-teardown");
    // Unique cmd → unique snapshot key → the pre-start snapshot is created
    // (and therefore snapshot.pre_pause is hit) on THIS run.
    let script = format!("echo boot-{}; exec sleep 300", vm_name);

    let (mut child, pid, vm_log) = common::spawn_fcvm_snapshots_enabled_with_env_and_log_path(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            common::ALPINE_IMAGE,
            "sh",
            "-c",
            &script,
        ],
        &[("FCVM_FAILPOINT", "snapshot.pre_pause:sleep:10000")],
    )
    .await?;

    // Wait until the VM process is parked in the pre-pause hold.
    let reached_at = wait_for_marker(
        &vm_log,
        "FAILPOINT snapshot.pre_pause reached",
        0,
        Duration::from_secs(300),
    )
    .await?;

    // PID identity is published before lifecycle supervision is completely installed.
    // The readiness bit is the durable barrier that makes this exact child capture
    // meaningful rather than a race against final resource ownership publication.
    let ready_state = ls_vm_by_pid(pid)
        .await?
        .context("VM state disappeared at the held snapshot window")?;
    anyhow::ensure!(
        ready_state.lifecycle_ready,
        "snapshot.pre_pause must run only after lifecycle readiness is published"
    );

    // Capture THIS VM's firecracker and pasta (pid + start_time, immune to
    // PID reuse) while it is parked.
    let firecrackers = find_descendants_with_comm_prefix(pid, "firecracker");
    let pastas = find_descendants_with_comm_prefix(pid, "pasta");
    assert!(
        !firecrackers.is_empty(),
        "a running VM must have a firecracker descendant"
    );
    assert!(
        !pastas.is_empty(),
        "a rootless VM must have a pasta descendant"
    );

    // Prove we are still INSIDE the held window, then SIGTERM.
    assert!(
        search_log_from(&vm_log, "FAILPOINT snapshot.pre_pause released", reached_at).is_none(),
        "the pre-pause hold ended before SIGTERM could be delivered — the \
         interleaving did not happen (harness too slow?)"
    );
    sigterm(pid).await;

    // Bounded orderly exit. The bound covers the remaining hold (≤10s), the
    // to-completion snapshot (pause/save/resume of a 1GiB VM), and teardown.
    // No SIGKILL fallback: a hang here IS the bug this case exists to catch.
    let status = wait_exit(&mut child, Duration::from_secs(60))
        .await
        .with_context(|| {
            format!(
                "fcvm did not exit within 60s of SIGTERM during a held pre-pause \
                 window; log tail:\n{}",
                log_tail(&vm_log, 40)
            )
        })?;
    println!("fcvm exited after SIGTERM with {:?}", status);

    // No leaked processes for this VM (poll ≤15s: cleanup finishes just before
    // fcvm exits, but give reaping a moment under load).
    let leak_deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let leaked: Vec<String> = firecrackers
            .iter()
            .chain(pastas.iter())
            .filter(|p| p.alive())
            .map(|p| format!("{} (pid {})", p.comm, p.pid))
            .collect();
        if leaked.is_empty() {
            break;
        }
        if Instant::now() > leak_deadline {
            panic!(
                "leaked processes after teardown during held pause window: {:?}",
                leaked
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // No state file for this VM remains.
    assert!(
        !state_file_for_name_exists(&vm_name),
        "state file for {} must be deleted on teardown",
        vm_name
    );

    // Best-effort cache cleanup: the interrupted-but-completed pre-start
    // snapshot left a cache entry under this run's unique key; recover the key
    // from the log line "Creating pre-start snapshot snapshot_key=<key>".
    if let Some(pos) = search_log_from(&vm_log, "Creating pre-start snapshot", 0) {
        let content = std::fs::read_to_string(&vm_log).unwrap_or_default();
        // Find the line containing that byte offset and extract snapshot_key=…
        let upto = (pos as usize).min(content.len());
        let line_start = content[..upto].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line = content[line_start..].lines().next().unwrap_or("");
        if let Some(key) = line
            .split_whitespace()
            .find_map(|tok| tok.strip_prefix("snapshot_key="))
        {
            let key = key.trim_matches('"').to_string();
            cleanup_snapshot_keys(&[Some(key)]).await;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// CASE: cancellation_wins_before_lifecycle_ready_claim
// ---------------------------------------------------------------------------

/// Holds the owner after its old cancellation precheck but before the atomic
/// lifecycle-ready claim. SIGTERM must claim cancellation while the hold is
/// active, and releasing the hold must lead directly to cleanup without ever
/// persisting `lifecycle_ready=true`.
#[tokio::test]
async fn test_lifecycle_interleave_cancellation_wins_before_lifecycle_ready_claim() -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names("ilv-ready-cancel");
    let go_file = PathBuf::from(format!("/tmp/{}-ready-go", vm_name));
    remove_file_if_exists(&go_file).context("removing stale lifecycle-ready go-file")?;

    let failpoint_spec = format!(
        "lifecycle.before_ready_claim:block_until_file:{}",
        go_file.display()
    );
    let (child, pid, vm_log) = common::spawn_fcvm_with_env_and_log_path(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "rootless",
            "--no-snapshot",
            common::ALPINE_IMAGE,
            "sleep",
            "300",
        ],
        &[("FCVM_FAILPOINT", &failpoint_spec)],
    )
    .await
    .context("spawning VM held before lifecycle-ready claim")?;
    let process = PinnedProcess::capture(pid).context("pinning held VM process")?;
    let mut fixture = Some((child, process));
    let mut exact_artifacts = None;

    let exercise: Result<()> = async {
        let reached_at = wait_for_marker(
            &vm_log,
            "FAILPOINT lifecycle.before_ready_claim reached",
            0,
            Duration::from_secs(180),
        )
        .await?;

        let (child, _) = fixture.as_mut().context("held VM fixture disappeared")?;
        let artifacts = wait_for_artifacts(&vm_name, child, Duration::from_secs(10)).await?;
        anyhow::ensure!(
            artifacts.state.pid == Some(pid),
            "readiness claim must follow PID publication, found {:?}",
            artifacts.state.pid
        );
        anyhow::ensure!(
            !artifacts.state.lifecycle_ready,
            "VM became lifecycle-ready before the held atomic claim"
        );
        exact_artifacts = Some(artifacts.clone());

        let (_, process) = fixture.as_ref().context("held VM identity disappeared")?;
        process.signal(libc::SIGTERM)?;
        wait_for_marker(
            &vm_log,
            "received SIGTERM, shutting down VM",
            reached_at,
            Duration::from_secs(10),
        )
        .await
        .context("signal handler did not claim cancellation while readiness was held")?;
        anyhow::ensure!(
            search_log_from(
                &vm_log,
                "FAILPOINT lifecycle.before_ready_claim released",
                reached_at
            )
            .is_none(),
            "readiness hold released before cancellation was observed"
        );

        let held_state = fcvm::state::StateManager::new(fcvm::paths::state_dir())
            .load_state(&artifacts.vm_id)
            .await
            .context("loading state after cancellation won readiness gate")?;
        anyhow::ensure!(
            !held_state.lifecycle_ready,
            "cancellation winner published lifecycle readiness while the claim was held"
        );

        std::fs::write(&go_file, b"go").context("releasing lifecycle-ready claim")?;
        wait_for_marker(
            &vm_log,
            "FAILPOINT lifecycle.before_ready_claim released",
            reached_at,
            Duration::from_secs(10),
        )
        .await?;

        let (child, _) = fixture
            .as_mut()
            .context("held VM disappeared before cleanup")?;
        wait_exit(child, Duration::from_secs(60))
            .await
            .with_context(|| {
                format!(
                    "VM did not clean up after cancellation won readiness; log tail:\n{}",
                    log_tail(&vm_log, 60)
                )
            })?;
        wait_for_artifacts_removed(&artifacts, Duration::from_secs(15)).await?;
        Ok(())
    }
    .await;

    let mut cleanup_errors = Vec::new();
    if let Err(error) = std::fs::write(&go_file, b"cleanup") {
        cleanup_errors.push(format!("releasing readiness hold during cleanup: {error}"));
    }
    let mut child_stopped = true;
    if let Some((child, process)) = fixture.as_mut() {
        if let Err(error) = stop_test_child(child, process).await {
            child_stopped = false;
            cleanup_errors.push(format!("stopping held VM fixture: {error:#}"));
        }
    }
    if exercise.is_err() && child_stopped {
        if exact_artifacts.is_none() {
            match artifacts_for_name(&vm_name) {
                Ok(found) => exact_artifacts = found,
                Err(error) => cleanup_errors.push(format!(
                    "finding held VM artifacts during cleanup: {error:#}"
                )),
            }
        }
        if let Some(artifacts) = &exact_artifacts {
            if let Err(error) = cleanup_exact_artifacts(artifacts) {
                cleanup_errors.push(format!("cleaning held VM artifacts: {error:#}"));
            }
        }
    }
    if let Err(error) = remove_file_if_exists(&go_file) {
        cleanup_errors.push(format!("removing lifecycle-ready go-file: {error:#}"));
    }

    if let Err(error) = exercise {
        if !cleanup_errors.is_empty() {
            return Err(error.context(format!(
                "fixture cleanup also failed:\n- {}",
                cleanup_errors.join("\n- ")
            )));
        }
        return Err(error);
    }
    if !cleanup_errors.is_empty() {
        anyhow::bail!("fixture cleanup failed:\n- {}", cleanup_errors.join("\n- "));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// CASE: early_published_connection_cannot_poison_restored_ingress
// ---------------------------------------------------------------------------

/// Pins the external-flow generation boundary against the exact hostile
/// ordering that used to wedge a later request for roughly one TCP timeout:
/// rootless pasta has already published the clone's host port, but the restored
/// VMM has not resumed yet. One client completes a host-side TCP connect and
/// closes it inside that window. After release, the clone must finish
/// cookie-bound cleanup and the first fresh HTTP request must succeed within a
/// short bound; retrying for 108 seconds would hide the failure.
#[tokio::test]
async fn test_lifecycle_interleave_early_connection_cannot_poison_restored_ingress() -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let (baseline_name, clone_name, snapshot_name, _) = common::unique_names("ilv-early-ingress");
    let go_file = PathBuf::from(format!("/tmp/{clone_name}-restore-go"));
    remove_file_if_exists(&go_file).context("removing stale restore go-file")?;
    let host_port = common::find_available_high_port()?;
    let publish_arg = format!("{host_port}:80");

    let mut baseline: Option<(tokio::process::Child, PinnedProcess)> = None;
    let mut clone: Option<(tokio::process::Child, PinnedProcess, PathBuf)> = None;
    let mut clone_artifacts: Option<VmArtifacts> = None;
    let mut snapshot_created = false;

    let exercise: Result<()> = async {
        let (baseline_child, baseline_pid) = common::spawn_fcvm_with_logs(
            &[
                "podman",
                "run",
                "--name",
                &baseline_name,
                "--network",
                "rootless",
                "--no-snapshot",
                "--publish",
                &publish_arg,
                "--health-check",
                "http://localhost:80",
                common::TEST_IMAGE,
            ],
            &baseline_name,
        )
        .await
        .context("spawning published baseline")?;
        let baseline_process =
            PinnedProcess::capture(baseline_pid).context("pinning published baseline")?;
        baseline = Some((baseline_child, baseline_process));
        let (baseline_child, _) = baseline.as_mut().expect("baseline stored above");
        common::poll_health(baseline_child, 180)
            .await
            .context("waiting for published baseline health")?;
        common::create_snapshot_by_pid(baseline_pid, &snapshot_name)
            .await
            .context("creating published manual snapshot")?;
        snapshot_created = true;

        let (baseline_child, baseline_process) =
            baseline.as_mut().context("baseline fixture disappeared")?;
        stop_test_child(baseline_child, baseline_process)
            .await
            .context("stopping baseline before clone restore")?;
        baseline = None;

        let failpoint_spec = format!(
            "restore.post_network_pre_resume:block_until_file:{}",
            go_file.display()
        );
        let (clone_child, clone_pid, clone_log) = common::spawn_fcvm_with_env_and_log_path(
            &[
                "snapshot",
                "run",
                "--snapshot",
                &snapshot_name,
                "--name",
                &clone_name,
            ],
            &[("FCVM_FAILPOINT", &failpoint_spec)],
        )
        .await
        .context("spawning clone held before VMM resume")?;
        let clone_process = PinnedProcess::capture(clone_pid).context("pinning held clone")?;
        clone = Some((clone_child, clone_process, clone_log.clone()));

        let reached_at = wait_for_marker(
            &clone_log,
            "FAILPOINT restore.post_network_pre_resume reached",
            0,
            Duration::from_secs(120),
        )
        .await?;
        let (clone_child, _, _) = clone.as_mut().context("clone fixture disappeared")?;
        let artifacts = wait_for_artifacts(&clone_name, clone_child, Duration::from_secs(10))
            .await
            .context("waiting for held clone network state")?;
        anyhow::ensure!(
            artifacts.state.pid.is_none() && !artifacts.state.lifecycle_ready,
            "restore hold must precede VMM/lifecycle publication: pid={:?} ready={}",
            artifacts.state.pid,
            artifacts.state.lifecycle_ready
        );
        let loopback_ip = artifacts
            .state
            .config
            .network
            .loopback_ip
            .clone()
            .context("held rootless clone has no published loopback IP")?;
        clone_artifacts = Some(artifacts);

        // Non-vacuity: the hold is placed after the restore path's network
        // post-start, so pasta and its published-port listener are live, and
        // before the VMM resume, so the guest behind that listener has not run.
        // This connect+close is the adversarial flow; no retry is allowed, and
        // the absence of the release marker proves the guest cannot have run.
        let mut early = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::TcpStream::connect(format!("{loopback_ip}:{host_port}")),
        )
        .await
        .context("early published-port connect timed out")?
        .context("early published-port connect failed")?;
        early
            .shutdown()
            .await
            .context("closing adversarial early connection")?;
        drop(early);
        anyhow::ensure!(
            search_log_from(
                &clone_log,
                "FAILPOINT restore.post_network_pre_resume released",
                reached_at
            )
            .is_none(),
            "the VMM resumed before the adversarial connection completed"
        );

        std::fs::write(&go_file, b"go").context("releasing restore.post_network_pre_resume")?;
        let released_at = wait_for_marker(
            &clone_log,
            "FAILPOINT restore.post_network_pre_resume released",
            reached_at,
            Duration::from_secs(10),
        )
        .await?;
        let (clone_child, _, _) = clone.as_mut().context("clone fixture disappeared")?;
        common::poll_health(clone_child, 180)
            .await
            .with_context(|| {
                format!(
                    "clone did not become healthy; log tail:\n{}",
                    log_tail(&clone_log, 60)
                )
            })?;
        wait_for_marker(
            &clone_log,
            "snapshot network cleanup complete",
            released_at,
            Duration::from_secs(30),
        )
        .await
        .context("missing exact cookie-cleanup completion diagnostic")?;

        // One fresh request is the oracle. The regression stalls this request
        // for ~108s; retries or a matching timeout would conceal it.
        let fresh = common::curl_check_with_diag(&loopback_ip, host_port, 5, Some(clone_pid)).await;
        anyhow::ensure!(
            fresh.success && fresh.body_len > 0,
            "the first post-restore request was poisoned by the early connection: {}",
            fresh.error
        );
        Ok(())
    }
    .await;

    // Release first so fixture teardown can never wait for the failpoint's
    // hard cap. Stop exact pinned children before deleting their snapshot.
    let mut cleanup_errors = Vec::new();
    if let Err(error) = std::fs::write(&go_file, b"cleanup") {
        cleanup_errors.push(format!("releasing restore failpoint: {error}"));
    }
    let mut all_children_stopped = true;
    if let Some((child, process, _)) = clone.as_mut() {
        if let Err(error) = stop_test_child(child, process).await {
            all_children_stopped = false;
            cleanup_errors.push(format!("stopping clone: {error:#}"));
        }
    }
    if let Some((child, process)) = baseline.as_mut() {
        if let Err(error) = stop_test_child(child, process).await {
            all_children_stopped = false;
            cleanup_errors.push(format!("stopping baseline: {error:#}"));
        }
    }
    if all_children_stopped {
        if let Some(artifacts) = &clone_artifacts {
            if let Err(error) = wait_for_artifacts_removed(artifacts, Duration::from_secs(15)).await
            {
                cleanup_errors.push(format!("clone production cleanup: {error:#}"));
                if exercise.is_err() {
                    if let Err(cleanup_error) = cleanup_exact_artifacts(artifacts) {
                        cleanup_errors.push(format!(
                            "cleaning exact failed clone artifacts: {cleanup_error:#}"
                        ));
                    }
                }
            }
        }
    }
    if snapshot_created && all_children_stopped {
        if let Err(error) = common::delete_snapshot(&snapshot_name).await {
            cleanup_errors.push(format!("deleting snapshot {snapshot_name}: {error:#}"));
        }
    }
    if let Err(error) = remove_file_if_exists(&go_file) {
        cleanup_errors.push(format!("removing restore go-file: {error:#}"));
    }

    if let Err(error) = exercise {
        if cleanup_errors.is_empty() {
            return Err(error);
        }
        return Err(error.context(format!(
            "fixture cleanup also failed:\n- {}",
            cleanup_errors.join("\n- ")
        )));
    }
    anyhow::ensure!(
        cleanup_errors.is_empty(),
        "fixture cleanup failed:\n- {}",
        cleanup_errors.join("\n- ")
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// CASE: snapshot_run_sigterm_before_resume_cleans_exact_artifacts
// ---------------------------------------------------------------------------

/// Pins the signal-registration boundary of `fcvm snapshot run`. A rootless
/// clone is held at `restore.pre_resume:block_until_file` after clone setup has
/// published its state, lock, and runtime-data directory, but before the restore
/// function starts. No restored VMM exists at this boundary and state still has
/// `pid = null`. SIGTERM must already be owned by fcvm in this window; otherwise
/// the kernel's default action exits without cleanup and strands those setup
/// artifacts.
///
/// The marker and go-file make the interleaving deterministic:
///
/// 1. wait for `restore.pre_resume reached` and all three exact clone artifacts;
/// 2. prove the state still has no PID, deliver SIGTERM, and require fcvm's
///    signal-handler log while the failpoint remains held;
/// 3. release the failpoint, require a bounded non-success exit, and require the
///    exact `state/<vm_id>.json`, `.json.lock`, and `vm-disks/<vm_id>` paths to
///    be gone.
///
/// All fixture processes, the unique snapshot, and exact failed-run artifacts
/// are cleaned even when an assertion fails.
#[tokio::test]
async fn test_lifecycle_interleave_snapshot_run_sigterm_before_resume_cleans_exact_artifacts(
) -> Result<()> {
    let (baseline_name, clone_name, snapshot_name, _) = common::unique_names("ilv-restore-term");
    let go_file = PathBuf::from(format!("/tmp/{}-restore-go", clone_name));
    remove_file_if_exists(&go_file).context("removing stale restore go-file")?;

    let mut baseline: Option<(tokio::process::Child, PinnedProcess)> = None;
    let mut clone: Option<(tokio::process::Child, PinnedProcess, PathBuf)> = None;
    let mut clone_artifacts: Option<VmArtifacts> = None;

    let exercise: Result<()> = async {
        // A manual full snapshot gives this test one explicit restore generation
        // without involving the podman pre-start snapshot cache.
        let (baseline_child, baseline_pid) = common::spawn_fcvm_with_logs(
            &[
                "podman",
                "run",
                "--name",
                &baseline_name,
                "--network",
                "rootless",
                "--no-snapshot",
                common::TEST_IMAGE,
            ],
            &baseline_name,
        )
        .await
        .context("spawning rootless baseline")?;
        let baseline_process = PinnedProcess::capture(baseline_pid)
            .context("pinning rootless baseline process")?;
        baseline = Some((baseline_child, baseline_process));
        common::poll_health_by_pid(baseline_pid, 180)
            .await
            .context("waiting for baseline health")?;
        common::create_snapshot_by_pid(baseline_pid, &snapshot_name)
            .await
            .context("creating manual snapshot")?;

        // The direct snapshot owns its disk and memory, so the baseline can be
        // stopped before the clone starts. Use the same orderly signal path the
        // clone must later exercise.
        let (baseline_child, baseline_process) = baseline
            .as_mut()
            .context("baseline child disappeared from fixture")?;
        baseline_process.signal(libc::SIGTERM)?;
        let baseline_status = wait_exit(baseline_child, Duration::from_secs(60))
            .await
            .context("baseline did not stop after SIGTERM")?;
        anyhow::ensure!(
            baseline_status.success(),
            "baseline exited unsuccessfully: {baseline_status:?}"
        );

        let failpoint_spec = format!(
            "restore.pre_resume:block_until_file:{}",
            go_file.display()
        );
        let (clone_child, clone_pid, clone_log) =
            common::spawn_fcvm_with_env_and_log_path(
                &[
                    "snapshot",
                    "run",
                    "--snapshot",
                    &snapshot_name,
                    "--name",
                    &clone_name,
                ],
                &[("FCVM_FAILPOINT", &failpoint_spec)],
            )
            .await
            .context("spawning clone held before restore resume")?;
        let clone_process = PinnedProcess::capture(clone_pid).context("pinning clone process")?;
        clone = Some((clone_child, clone_process, clone_log.clone()));

        let reached_at = wait_for_marker(
            &clone_log,
            "FAILPOINT restore.pre_resume reached",
            0,
            Duration::from_secs(120),
        )
        .await?;

        let (clone_child, _, _) = clone
            .as_mut()
            .context("clone child disappeared from fixture")?;
        let artifacts = wait_for_artifacts(&clone_name, clone_child, Duration::from_secs(10))
            .await
            .with_context(|| {
                format!(
                    "clone artifacts were not published inside the held restore window; log tail:\n{}",
                    log_tail(&clone_log, 40)
                )
            })?;
        anyhow::ensure!(
            artifacts.state.pid.is_none(),
            "restore.pre_resume must precede PID publication, but clone {} recorded PID {:?}",
            artifacts.vm_id,
            artifacts.state.pid
        );
        anyhow::ensure!(
            !artifacts.state.lifecycle_ready,
            "restore.pre_resume must precede lifecycle readiness publication for clone {}",
            artifacts.vm_id
        );
        clone_artifacts = Some(artifacts.clone());

        anyhow::ensure!(
            search_log_from(
                &clone_log,
                "FAILPOINT restore.pre_resume released",
                reached_at
            )
            .is_none(),
            "restore.pre_resume was released before SIGTERM; the intended interleaving did not happen"
        );

        let (_, clone_process, _) = clone
            .as_ref()
            .context("clone identity disappeared from fixture")?;
        clone_process.signal(libc::SIGTERM)?;
        wait_for_marker(
            &clone_log,
            "received SIGTERM, shutting down VM",
            reached_at,
            Duration::from_secs(10),
        )
        .await
        .with_context(|| {
            format!(
                "fcvm did not handle SIGTERM while restore.pre_resume was held; log tail:\n{}",
                log_tail(&clone_log, 40)
            )
        })?;
        anyhow::ensure!(
            search_log_from(
                &clone_log,
                "FAILPOINT restore.pre_resume released",
                reached_at
            )
            .is_none(),
            "the handler observation happened after the failpoint released"
        );

        std::fs::write(&go_file, b"go").context("releasing restore.pre_resume")?;
        let released_at = wait_for_marker(
            &clone_log,
            "FAILPOINT restore.pre_resume released",
            reached_at,
            Duration::from_secs(10),
        )
        .await?;

        let (clone_child, _, _) = clone
            .as_mut()
            .context("clone child disappeared before exit")?;
        let clone_status = wait_exit(clone_child, Duration::from_secs(180))
            .await
            .with_context(|| {
                format!(
                    "clone did not exit after releasing the held restore; log tail:\n{}",
                    log_tail(&clone_log, 60)
                )
            })?;
        anyhow::ensure!(
            !clone_status.success(),
            "an interrupted clone setup reported success ({clone_status:?}); log tail:\n{}",
            log_tail(&clone_log, 60)
        );
        wait_for_marker(
            &clone_log,
            "clone setup cleanup complete",
            released_at,
            Duration::from_secs(10),
        )
        .await
        .context("waiting for the log consumer to drain the cleanup completion record")?;
        wait_for_marker(
            &clone_log,
            "interrupted by signal during clone setup",
            released_at,
            Duration::from_secs(10),
        )
        .await
        .context("waiting for the explicit interrupted-setup diagnostic")?;
        wait_for_artifacts_removed(&artifacts, Duration::from_secs(15)).await?;

        Ok(())
    }
    .await;

    // Always unblock a held restore before fixture teardown. If the production
    // handler regresses, the clone may already be dead; writing the unique file
    // is harmless and prevents the failpoint's 60s cap from delaying cleanup.
    let mut cleanup_errors = Vec::new();
    if let Err(e) = std::fs::write(&go_file, b"cleanup") {
        cleanup_errors.push(format!(
            "unblocking restore failpoint during cleanup: {:#}",
            anyhow::Error::new(e)
        ));
    }

    let mut all_children_stopped = true;
    if let Some((child, process, _)) = clone.as_mut() {
        if let Err(e) = stop_test_child(child, process).await {
            all_children_stopped = false;
            cleanup_errors.push(format!("stopping clone fixture: {e:#}"));
        }
    }
    if let Some((child, process)) = baseline.as_mut() {
        if let Err(e) = stop_test_child(child, process).await {
            all_children_stopped = false;
            cleanup_errors.push(format!("stopping baseline fixture: {e:#}"));
        }
    }

    // If the assertion failed before the normal cleanup oracle completed,
    // remove only the exact paths derived from this clone's deserialized state.
    // Preserve the original failure as the test result; fixture-cleanup errors
    // are added as context when there was no earlier failure.
    if exercise.is_err() && all_children_stopped {
        if clone_artifacts.is_none() {
            match artifacts_for_name(&clone_name) {
                Ok(found) => clone_artifacts = found,
                Err(e) => cleanup_errors.push(format!("finding failed clone artifacts: {:#}", e)),
            }
        }
        if let Some(artifacts) = &clone_artifacts {
            if let Err(e) = cleanup_exact_artifacts(artifacts) {
                cleanup_errors.push(format!("cleaning exact failed clone artifacts: {:#}", e));
            }
        }
    } else if exercise.is_err() && !all_children_stopped {
        cleanup_errors.push(
            "skipped exact artifact cleanup because a fixture child was not proven stopped"
                .to_string(),
        );
    }

    if all_children_stopped {
        if let Err(e) = common::delete_snapshot(&snapshot_name).await {
            cleanup_errors.push(format!("deleting unique manual snapshot: {e:#}"));
        }
        if common::snapshot_exists(&snapshot_name) {
            cleanup_errors.push(format!(
                "unique manual snapshot {snapshot_name} survived fixture cleanup"
            ));
        }
        if let Err(e) = remove_file_if_exists(&go_file) {
            cleanup_errors.push(format!("removing restore go-file: {e:#}"));
        }
    } else {
        cleanup_errors.push(
            "skipped snapshot and go-file removal because a fixture child was not proven stopped"
                .to_string(),
        );
    }

    if let Err(e) = exercise {
        if !cleanup_errors.is_empty() {
            return Err(e.context(format!(
                "fixture cleanup also failed:\n- {}",
                cleanup_errors.join("\n- ")
            )));
        }
        return Err(e);
    }
    if !cleanup_errors.is_empty() {
        anyhow::bail!("fixture cleanup failed:\n- {}", cleanup_errors.join("\n- "));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// cache-verdict-re-ask
// ---------------------------------------------------------------------------

/// A pre-start snapshot MISS on the resume flow, with the resume→ack window
/// deliberately widened (`cache.pre_ack:sleep:3000`). The snapshot save queues
/// a vsock TRANSPORT_RESET into the guest, so on resume the guest's handshake
/// session is severed 3s before the host's cache-ack can arrive — the guest
/// MUST classify from the host's verdict (re-ask → "cache-ack"), never from
/// the severed transport. Pre-protocol code read the severance as "I was
/// restored" and waited forever for a restore epoch no one publishes to a
/// source: this test then fails with the VM never Healthy and a single
/// cache-ready in the log.
#[tokio::test]
async fn test_lifecycle_interleave_resumed_source_survives_late_ack() -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names("ilv-reask");
    // Unique env → unique snapshot key → the pre-start snapshot is CREATED
    // this run (a hit would restore instead and never cross the resume flow).
    let env_unique = format!("ILV_ID={}", vm_name);

    let (mut child, pid, vm_log) = common::spawn_fcvm_snapshots_enabled_with_env_and_log_path(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--env",
            &env_unique,
            common::TEST_IMAGE,
        ],
        &[("FCVM_FAILPOINT", "cache.pre_ack:sleep:3000")],
    )
    .await?;

    // The boundary this test exists for: the miss path must actually create
    // the pre-start snapshot (pause → save → resume) this run.
    let created_at = wait_for_marker(
        &vm_log,
        "Pre-start snapshot created successfully",
        0,
        Duration::from_secs(300),
    )
    .await
    .context("pre-start snapshot was never created — the miss path did not run")?;

    // Outcome: the resumed source must still launch its container and become
    // Healthy. This is the line that hangs without the re-ask protocol.
    let vm_id = loop {
        if let Some(state) = ls_vm_by_pid(pid).await? {
            break state.vm_id;
        }
        anyhow::ensure!(
            child.try_wait()?.is_none(),
            "fcvm exited before creating VM state; log tail:\n{}",
            log_tail(&vm_log, 40)
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let state_path = fcvm::paths::state_dir().join(format!("{}.json", vm_id));
    let poll_deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let healthy = std::fs::read_to_string(&state_path)
            .ok()
            .and_then(|c| serde_json::from_str::<fcvm::state::VmState>(&c).ok())
            .map(|s| s.health_status == fcvm::state::HealthStatus::Healthy)
            .unwrap_or(false);
        if healthy {
            break;
        }
        anyhow::ensure!(
            child.try_wait()?.is_none(),
            "fcvm exited before Healthy; log tail:\n{}",
            log_tail(&vm_log, 40)
        );
        anyhow::ensure!(
            Instant::now() <= poll_deadline,
            "resumed source never became Healthy after its pre-start snapshot — \
             the severed handshake was read as a classification again; log tail:\n{}",
            log_tail(&vm_log, 40)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Non-vacuity, from the host's own markers:
    // (1) the widened window was actually armed and hit;
    let pre_ack_hit = log_contains(&vm_log, "FAILPOINT cache.pre_ack reached");
    // (2) the guest actually RE-ASKED after the severed session — the second
    //     "Cache-ready notification received" is the mechanism under test,
    //     and it must come after the snapshot boundary.
    let reask_after_boundary =
        search_log_from(&vm_log, "Cache-ready notification received", created_at).is_some();
    let ask_count = {
        let content = std::fs::read_to_string(&vm_log).unwrap_or_default();
        count_occurrences(&content, "Cache-ready notification received")
    };
    let base_key = ls_vm_by_pid(pid)
        .await?
        .and_then(|s| s.config.snapshot_name);

    common::kill_process(pid).await;
    let _ = child.wait().await;
    cleanup_snapshot_keys(&[base_key]).await;

    assert!(
        pre_ack_hit,
        "the cache.pre_ack failpoint never fired — the widened window was not exercised; \
         log tail:\n{}",
        log_tail(&vm_log, 40)
    );
    assert!(
        reask_after_boundary && ask_count >= 2,
        "expected a re-asked cache-ready after the snapshot boundary (got {} asks, \
         re-ask-after-boundary={}); the healthy outcome would be vacuous without it; \
         log tail:\n{}",
        ask_count,
        reask_after_boundary,
        log_tail(&vm_log, 40)
    );
    Ok(())
}
