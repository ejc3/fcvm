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
//!
//! All VMs run rootless (no sudo needed beyond what `make test-root` does for
//! the runner). Test names contain `lifecycle_interleave` so
//! `make test-root FILTER=lifecycle_interleave STREAM=1` runs exactly this file.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
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
#[tokio::test]
async fn test_lifecycle_interleave_healthy_means_live_dataplane() -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names("ilv-healthy");
    // Unique env → unique base AND startup snapshot keys → the startup
    // snapshot is CREATED this run (a warm start would skip the pause and
    // make this test vacuous).
    let env_unique = format!("ILV_ID={}", vm_name);
    let host_port = common::find_available_high_port()?;
    let publish_arg = format!("{}:80", host_port);

    let (mut child, pid, vm_log) = common::spawn_fcvm_with_env_and_log_path(
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
    let (mut child1, pid1, log1) = common::spawn_fcvm_with_env_and_log_path(
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
    let (mut child2, pid2, log2) = common::spawn_fcvm_with_env_and_log_path(
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
#[tokio::test]
async fn test_lifecycle_interleave_no_leaks_after_interleaved_teardown() -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names("ilv-teardown");
    // Unique cmd → unique snapshot key → the pre-start snapshot is created
    // (and therefore snapshot.pre_pause is hit) on THIS run.
    let script = format!("echo boot-{}; exec sleep 300", vm_name);

    let (mut child, pid, vm_log) = common::spawn_fcvm_with_env_and_log_path(
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
