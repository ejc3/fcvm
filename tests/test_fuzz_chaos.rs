//! TIER 2: seeded lifecycle chaos harness.
//!
//! Boots ONE real rootless VM per seed and fires a randomized (but seeded)
//! schedule of lifecycle-adjacent operations at it — execs (plain + tty),
//! port-forward curls, state reads, snapshot creates, console floods, and
//! jitter sleeps — then shuts down gracefully and checks the end-state
//! oracles: every exec-appended nonce landed exactly once in the mapped file,
//! no firecracker/pasta process for the VM survived, and the state file is
//! gone. A state read that observes `Healthy` immediately chains a curl to
//! the forwarded port: the Healthy⇒live oracle, sampled at random times.
//!
//! # Determinism
//!
//! This harness is **schedule-deterministic, not cycle-deterministic**: the
//! same seed always produces the same op sequence, the same nonces/tags, and
//! the same jitter delays (`SmallRng::seed_from_u64(seed)`, one `u64` draw
//! per op), but the wall-clock interleaving against VM-internal events still
//! varies run to run. The shrinking path for a failure is therefore:
//! reproduce with the failing seed (see the replay hint in the panic), read
//! the `FUZZ seed=…` trace to identify the racing pair, then pin that exact
//! interleaving as a named failpoint case in `test_lifecycle_interleave.rs`
//! (crate `failpoint/` documents the marker contract).
//!
//! # Knobs
//!
//! - `FCVM_FUZZ_SEEDS` (default `"1"`): comma list of literal seeds
//!   (`"3,17,42"` — a trailing comma like `"7,"` forces list form for a
//!   single literal seed) or a bare count `N` meaning seeds `1..=N`.
//! - `FCVM_FUZZ_OPS` (default `10`): ops per seed.
//!
//! Run via `make fuzz SEEDS=25 OPS=30`.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{bail, ensure, Context, Result};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

/// Hard per-op timeout: an op that neither completes nor errors within this
/// FAILS the seed. Generous — every op here normally finishes in seconds.
const OP_TIMEOUT: Duration = Duration::from_secs(60);
/// Snapshot create pauses the VM and writes a multi-GB memory image; give it
/// a larger (still hard) budget than ordinary ops.
const SNAPSHOT_OP_TIMEOUT: Duration = Duration::from_secs(120);

/// Budget for a CloneRestore op. Deliberately LARGER than the waits inside it
/// (60s health poll + a 5s curl): if the outer timeout fired first, the op
/// future would be dropped mid-await and its clone teardown skipped, leaving a
/// restored VM running while the seed tears down around it (review finding on
/// #837). The registry below is the belt to this suspenders.
const CLONE_OP_TIMEOUT: Duration = Duration::from_secs(150);

/// Clone PIDs a CloneRestore op spawned and has not yet reaped.
///
/// The op kills its own clone on every path it can reach, but a future that is
/// DROPPED (outer timeout, or a panic unwinding through it) runs no cleanup at
/// all. Recording the pid the moment it exists lets the seed teardown reap what
/// the op could not, so a fuzz failure never leaves a live VM to poison the
/// next seed (review finding on #837).
static LIVE_CLONES: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

fn register_clone(pid: u32) {
    LIVE_CLONES.lock().unwrap().push(pid);
}

fn unregister_clone(pid: u32) {
    LIVE_CLONES.lock().unwrap().retain(|p| *p != pid);
}

/// Kill any clone a dropped op left running. Returns how many it reaped.
async fn reap_leaked_clones() -> usize {
    let leaked: Vec<u32> = std::mem::take(&mut *LIVE_CLONES.lock().unwrap());
    for pid in &leaked {
        eprintln!("FUZZ reaping leaked clone pid={}", pid);
        common::kill_process(*pid).await;
    }
    leaked.len()
}
/// At most this many SnapshotCreate ops per seed (disk: each is multi-GB).
const MAX_SNAPSHOTS_PER_SEED: usize = 2;
/// Bounded wait for first-healthy (covers cold-cache boot incl. the pre-start
/// startup-snapshot create + converge-on-miss relaunch).
const HEALTHY_TIMEOUT_SECS: u64 = 300;
/// Bounded wait for fcvm to exit after SIGTERM. Not exiting in time FAILS the
/// seed — graceful shutdown hanging is a bug, not something to force-kill over.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

const GUEST_NONCE_FILE: &str = "/mnt/fuzz/nonces.txt";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    /// Append a seeded nonce to the mapped file via plain (non-tty) exec.
    ExecNonce,
    /// Echo a token through a tty-mode exec.
    ExecTty,
    /// Single curl to the forwarded port.
    CurlPort,
    /// Read + parse the VM state; if it claims Healthy, chain an immediate
    /// CurlPort — the Healthy⇒live oracle at a random time.
    StateRead,
    /// `fcvm snapshot create --pid <vm> --tag <seeded>` (max 2 per seed).
    SnapshotCreate,
    /// Flood the console/exec stream with 5000 lines of output.
    ConsoleFlood,
    /// Restore a clone from a snapshot this seed took and immediately hit its
    /// forwarded port — the readiness contract a caller relies on.
    ///
    /// This is the op that reaches the restore-time network path: a restored
    /// guest inherits the snapshot's conntrack table and its published-port
    /// DNAT, and fcvm declares the clone ready only after the guest answers a
    /// TCP probe. A clone handed over before that path settles shows up here
    /// as a failed curl against a clone fcvm called ready. Observed in the
    /// wild at roughly 1 in 400 restores (2026-08-15), which no single-VM op
    /// in this harness could ever reach.
    CloneRestore,
    /// Seeded 0-400ms sleep (also substituted when the snapshot budget is
    /// spent, keeping rng consumption uniform at one draw per op).
    JitterSleep,
}

/// Sampling weights. Order matters only for the cumulative walk; the draw is
/// a single `gen_range` so the schedule is a pure function of the seed.
const OP_WEIGHTS: &[(Op, u32)] = &[
    (Op::ExecNonce, 20),
    (Op::ExecTty, 10),
    (Op::CurlPort, 20),
    (Op::StateRead, 15),
    (Op::SnapshotCreate, 6),
    (Op::ConsoleFlood, 9),
    (Op::CloneRestore, 12),
    (Op::JitterSleep, 20),
];

fn sample_op(rng: &mut SmallRng) -> Op {
    let total: u32 = OP_WEIGHTS.iter().map(|(_, w)| w).sum();
    let mut x = rng.gen_range(0..total);
    for &(op, w) in OP_WEIGHTS {
        if x < w {
            return op;
        }
        x -= w;
    }
    unreachable!("weight walk exhausted");
}

/// Parse FCVM_FUZZ_SEEDS: comma list of literal seeds, or bare count N = 1..=N.
fn parse_seeds(spec: &str) -> Vec<u64> {
    let spec = spec.trim();
    if spec.contains(',') {
        let seeds: Vec<u64> = spec
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.parse()
                    .unwrap_or_else(|_| panic!("FCVM_FUZZ_SEEDS: bad seed {:?} in {:?}", s, spec))
            })
            .collect();
        assert!(
            !seeds.is_empty(),
            "FCVM_FUZZ_SEEDS: empty seed list {:?}",
            spec
        );
        seeds
    } else {
        let n: u64 = spec
            .parse()
            .unwrap_or_else(|_| panic!("FCVM_FUZZ_SEEDS: bad count {:?}", spec));
        assert!(n >= 1, "FCVM_FUZZ_SEEDS: count must be >= 1");
        (1..=n).collect()
    }
}

// ---------------------------------------------------------------------------
// /proc helpers for the leaked-process oracle
// ---------------------------------------------------------------------------

/// Fields of /proc/<pid>/stat AFTER the `(comm)` — comm may contain spaces and
/// parens, so split on the LAST ')'. Index 1 = ppid (field 4), index 19 =
/// starttime (field 22, clock ticks since boot).
fn proc_stat_after_comm(pid: u32) -> Option<Vec<String>> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    Some(rest.split_whitespace().map(str::to_string).collect())
}

fn ppid_of(pid: u32) -> Option<u32> {
    proc_stat_after_comm(pid)?.get(1)?.parse().ok()
}

fn start_time_of(pid: u32) -> Option<u64> {
    proc_stat_after_comm(pid)?.get(19)?.parse().ok()
}

/// Walk the ppid chain (bounded) to see whether `ancestor` is above `pid`.
fn is_descendant_of(pid: u32, ancestor: u32) -> bool {
    let mut cur = pid;
    for _ in 0..64 {
        match ppid_of(cur) {
            Some(0) | None => return false,
            Some(p) if p == ancestor => return true,
            Some(p) => cur = p,
        }
    }
    false
}

/// A (pid, starttime) pair uniquely identifies a process even after PID reuse
/// (same identity check the state manager uses).
#[derive(Debug)]
struct TrackedProc {
    pid: u32,
    start: u64,
    label: &'static str,
}

impl TrackedProc {
    fn alive(&self) -> bool {
        start_time_of(self.pid) == Some(self.start)
    }
}

/// Find live descendants of `fcvm_pid` whose cmdline matches `pattern`.
async fn find_descendants(fcvm_pid: u32, pattern: &str, label: &'static str) -> Vec<TrackedProc> {
    let mut found = Vec::new();
    let Ok(out) = tokio::process::Command::new("pgrep")
        .args(["-f", pattern])
        .output()
        .await
    else {
        return found;
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Ok(pid) = line.trim().parse::<u32>() else {
            continue;
        };
        if !is_descendant_of(pid, fcvm_pid) {
            continue;
        }
        // The pid could die between pgrep and this read; a None start just
        // means it is already gone and there is nothing to track.
        if let Some(start) = start_time_of(pid) {
            found.push(TrackedProc { pid, start, label });
        }
    }
    found
}

// ---------------------------------------------------------------------------
// State parsing
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct VmDisplay {
    #[serde(flatten)]
    vm: fcvm::state::VmState,
    #[allow(dead_code)]
    stale: bool,
}

/// `fcvm ls --json --pid <pid>` parsed into typed state (never regex on JSON).
async fn read_vm_state(pid: u32) -> Result<Vec<VmDisplay>> {
    let fcvm_path = common::find_fcvm_binary()?;
    let output = tokio::process::Command::new(&fcvm_path)
        .args(["ls", "--json", "--pid", &pid.to_string()])
        .output()
        .await
        .context("running fcvm ls --json")?;
    ensure!(
        output.status.success(),
        "fcvm ls failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).context("parsing fcvm ls JSON")
}

/// State files in the state dir claiming this VM name (post-shutdown oracle).
fn state_files_for_vm(vm_name: &str) -> Vec<PathBuf> {
    let dir = fcvm::paths::state_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .filter(|p| {
            std::fs::read_to_string(p)
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(|n| n == vm_name))
                .unwrap_or(false)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Ops
// ---------------------------------------------------------------------------

/// Echo a token through a tty-mode exec (`script` provides the PTY, exactly
/// like test_exec.rs — the harness itself has no controlling terminal).
async fn exec_tty_echo(pid: u32, token: &str) -> Result<()> {
    let fcvm_path = common::find_fcvm_binary()?;
    let fcvm_cmd = format!(
        "{} exec --pid {} -t -- echo {}",
        shell_words::quote(&fcvm_path.to_string_lossy()),
        pid,
        token
    );
    // kill_on_drop: the per-op timeout drops this future on expiry, which must
    // kill the spawned exec instead of orphaning it.
    let child = tokio::process::Command::new("script")
        .args(["-q", "-c", &fcvm_cmd, "/dev/null"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawning tty exec via script")?;
    let output = child
        .wait_with_output()
        .await
        .context("waiting for tty exec")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    ensure!(
        output.status.success(),
        "tty exec exited with {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(
        stdout.contains(token),
        "tty exec output missing token {:?}: {:?}",
        token,
        stdout
    );
    Ok(())
}

async fn curl_once(ip: &str, port: u16) -> Result<String> {
    let r = common::curl_check(ip, port, 5).await;
    ensure!(
        r.success && r.body_len > 0,
        "curl {}:{} failed (success={} body_len={} err={})",
        ip,
        port,
        r.success,
        r.body_len,
        r.error
    );
    Ok(format!("body_len={}", r.body_len))
}

/// Execute one op. `draw` is the op's pre-drawn randomness (one u64 per op).
/// Returns a short human detail for the trace line.
#[allow(clippy::too_many_arguments)]
async fn apply_op(
    op: Op,
    draw: u64,
    seed: u64,
    op_idx: usize,
    pid: u32,
    serve_pid: u32,
    ip: &str,
    port: u16,
    snap_base: &str,
    nonces: &mut Vec<String>,
    snapshots: &mut Vec<String>,
) -> Result<String> {
    match op {
        Op::ExecNonce => {
            let nonce = format!("nonce-{}-{}-{:016x}", seed, op_idx, draw);
            common::exec_in_container(pid, &[&format!("echo {} >> {}", nonce, GUEST_NONCE_FILE)])
                .await
                .with_context(|| format!("appending nonce {}", nonce))?;
            nonces.push(nonce.clone());
            Ok(format!("nonce={}", nonce))
        }
        Op::ExecTty => {
            let token = format!("fuzz-tty-{}-{}", seed, op_idx);
            exec_tty_echo(pid, &token).await?;
            Ok(format!("token={}", token))
        }
        Op::CurlPort => curl_once(ip, port).await,
        Op::StateRead => {
            let vms = read_vm_state(pid).await?;
            ensure!(
                !vms.is_empty(),
                "state read mid-run found no state for pid {}",
                pid
            );
            let status = vms[0].vm.health_status;
            if matches!(status, fcvm::state::HealthStatus::Healthy) {
                // Healthy⇒live oracle: a state file claiming Healthy must be
                // backed by a VM that actually serves traffic RIGHT NOW.
                let detail = curl_once(ip, port)
                    .await
                    .context("state says Healthy but the forwarded port is dead")?;
                Ok(format!("status={:?} chained-curl {}", status, detail))
            } else {
                Ok(format!("status={:?} (no chained curl)", status))
            }
        }
        Op::SnapshotCreate => {
            let tag = format!("{}-{}", snap_base, op_idx);
            common::create_snapshot_by_pid(pid, &tag).await?;
            snapshots.push(tag.clone());
            Ok(format!("tag={}", tag))
        }
        Op::ConsoleFlood => {
            let out = common::exec_in_container(pid, &["seq 1 5000"]).await?;
            ensure!(
                out.lines().any(|l| l.trim() == "5000"),
                "console flood output truncated (no final '5000' line, got {} bytes)",
                out.len()
            );
            Ok(format!("{} bytes", out.len()))
        }
        Op::CloneRestore => {
            let clone_name = format!("{}-clone-{}", snap_base, op_idx);
            // No --publish here: `snapshot run` takes none, because a clone
            // INHERITS the snapshot's port mappings. In rootless mode each
            // clone gets its own loopback IP, so the inherited host port is
            // reached at that address (same shape as
            // test_clone_port_forward_rootless).
            let (mut clone_child, clone_pid) = common::spawn_fcvm(&[
                "snapshot",
                "run",
                "--pid",
                &serve_pid.to_string(),
                "--name",
                &clone_name,
            ])
            .await
            .with_context(|| format!("restoring clone via serve {}", serve_pid))?;
            register_clone(clone_pid);

            // fcvm only reports a clone up after its own post-restore
            // port-forward verification, so a curl that fails here means the
            // readiness contract was declared on something the data path does
            // not honour — the exact defect this op exists to catch.
            let verdict = async {
                common::poll_health_by_pid(clone_pid, 60)
                    .await
                    .context("restored clone never became healthy")?;
                let clone_ip = common::get_loopback_ip(clone_pid)
                    .await
                    .context("clone has no loopback IP after restore")?;
                curl_once(&clone_ip, port)
                    .await
                    .context("clone reported healthy but its inherited forwarded port is dead")
            }
            .await;

            common::kill_process(clone_pid).await;
            let _ = clone_child.wait().await;
            unregister_clone(clone_pid);
            let detail = verdict?;
            Ok(format!("clone={} {}", clone_name, detail))
        }
        Op::JitterSleep => {
            let ms = draw % 401; // 0-400ms
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Ok(format!("{}ms", ms))
        }
    }
}

// ---------------------------------------------------------------------------
// Per-seed run
// ---------------------------------------------------------------------------

async fn run_seed(seed: u64, total_ops: usize) -> Result<()> {
    let mut rng = SmallRng::seed_from_u64(seed);
    let (vm_name, _, snap_base, _) = common::unique_names(&format!("fuzz{}", seed));
    let scratch = PathBuf::from(format!("/tmp/fcvm-fuzz-{}-{}", std::process::id(), vm_name));
    let host_dir = scratch.join("host");
    std::fs::create_dir_all(&host_dir).context("creating scratch dir")?;

    let host_port = common::find_available_high_port()?;
    let map_arg = format!("{}:/mnt/fuzz", host_dir.display());
    let publish_arg = format!("{}:80", host_port);

    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "rootless",
        "--map",
        &map_arg,
        "--publish",
        &publish_arg,
        "--health-check",
        "http://localhost/",
        common::TEST_IMAGE,
    ])
    .await?;
    println!(
        "FUZZ seed={} vm={} pid={} port={} scratch={}",
        seed,
        vm_name,
        pid,
        host_port,
        scratch.display()
    );

    let mut nonces: Vec<String> = Vec::new();
    let mut snapshots: Vec<String> = Vec::new();
    let body = run_seed_body(
        seed,
        total_ops,
        &mut rng,
        &mut child,
        pid,
        &vm_name,
        &host_dir,
        host_port,
        &snap_base,
        &mut nonces,
        &mut snapshots,
    )
    .await;

    // Cleanup runs on both paths. On failure the VM may still be up — kill it
    // so a failing seed doesn't leak a VM into the rest of the suite.
    if body.is_err() {
        common::kill_process(pid).await;
        let _ = child.wait().await;
    }
    // Reap before deleting snapshots: a leaked clone still holds the snapshot
    // it restored from, and deleting underneath it is how a "cleanup" turns
    // into the next seed's mystery.
    let reaped = reap_leaked_clones().await;
    if reaped > 0 {
        eprintln!(
            "FUZZ seed={} reaped {} clone(s) a dropped op left behind",
            seed, reaped
        );
    }
    for tag in &snapshots {
        let _ = common::delete_snapshot(tag).await;
    }
    let _ = std::fs::remove_dir_all(&scratch);

    body
}

/// Boot-wait, op loop, graceful shutdown, oracles. Split out so `run_seed`
/// can guarantee cleanup around any failure.
#[allow(clippy::too_many_arguments)]
async fn run_seed_body(
    seed: u64,
    total_ops: usize,
    rng: &mut SmallRng,
    child: &mut tokio::process::Child,
    pid: u32,
    vm_name: &str,
    host_dir: &std::path::Path,
    host_port: u16,
    snap_base: &str,
    nonces: &mut Vec<String>,
    snapshots: &mut Vec<String>,
) -> Result<()> {
    // -- Boot: bounded wait for first-healthy -------------------------------
    let boot_start = Instant::now();
    common::poll_health(child, HEALTHY_TIMEOUT_SECS)
        .await
        .with_context(|| format!("seed {}: VM never became healthy", seed))?;
    println!(
        "FUZZ seed={} healthy in {:.1}s",
        seed,
        boot_start.elapsed().as_secs_f64()
    );

    // One baseline snapshot BEFORE the op loop, so CloneRestore has something
    // to restore from its first draw. Without it most CloneRestore ops fall
    // through as "no snapshot yet" (5 of 6 in the first run of this rung), and
    // a defect that appears roughly once in 400 restores needs every restore
    // the budget allows. Taken outside the loop, so it consumes no rng draw
    // and the schedule stays a pure function of the seed.
    let baseline_tag = format!("{}-base", snap_base);
    // Bounded by the same budget a SnapshotCreate op gets: unbounded, a stalled
    // snapshot would sit here until nextest's two-hour test timeout instead of
    // failing the seed at the documented budget (review finding on #837).
    tokio::time::timeout(
        SNAPSHOT_OP_TIMEOUT,
        common::create_snapshot_by_pid(pid, &baseline_tag),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "seed {}: baseline snapshot did not finish within {:?}",
            seed,
            SNAPSHOT_OP_TIMEOUT
        )
    })?
    .with_context(|| format!("seed {}: baseline snapshot for clone restores", seed))?;
    snapshots.push(baseline_tag.clone());

    // Restore through a UFFD memory server, not the file backend.
    //
    // This is the path the request benchmark actually uses (BACKEND=uffd is
    // its default) and the path both 2026-08-15 failures came from. An earlier
    // version of this rung restored with `--snapshot`, i.e. the FILE backend,
    // and 351 clean restores said nothing about the failing path at all.
    let (mut serve_child, serve_pid) = common::spawn_fcvm(&["snapshot", "serve", &baseline_tag])
        .await
        .with_context(|| format!("seed {}: starting UFFD serve", seed))?;
    common::poll_serve_ready(&baseline_tag, serve_pid, 60)
        .await
        .with_context(|| format!("seed {}: UFFD serve never became ready", seed))?;

    let ip = common::get_loopback_ip(pid).await?;
    let vm_id = {
        let vms = read_vm_state(pid).await?;
        ensure!(
            !vms.is_empty(),
            "no state for freshly-healthy VM pid {}",
            pid
        );
        vms[0].vm.vm_id.clone()
    };

    // Capture the VM's firecracker/pasta processes NOW (pid + starttime), so
    // the post-shutdown leak oracle checks these exact processes.
    let mut tracked = find_descendants(pid, "firecracker", "firecracker").await;
    tracked.extend(find_descendants(pid, "pasta", "pasta").await);
    ensure!(
        tracked.iter().any(|t| t.label == "firecracker"),
        "found no firecracker descendant of fcvm pid {} — leak oracle would be vacuous",
        pid
    );
    println!(
        "FUZZ seed={} tracking {:?}",
        seed,
        tracked
            .iter()
            .map(|t| format!("{}={}", t.label, t.pid))
            .collect::<Vec<_>>()
    );

    // -- Op loop ------------------------------------------------------------
    for i in 1..=total_ops {
        let mut op = sample_op(rng);
        // One uniform draw per op keeps rng consumption identical whatever
        // the op does with it (nonce, jitter ms, or nothing).
        let draw: u64 = rng.gen();
        if op == Op::SnapshotCreate && snapshots.len() >= MAX_SNAPSHOTS_PER_SEED {
            // Deterministic downgrade: pure function of the schedule so far.
            op = Op::JitterSleep;
        }
        println!("FUZZ seed={} op={}/{} {:?}", seed, i, total_ops, op);
        let timeout = match op {
            Op::SnapshotCreate => SNAPSHOT_OP_TIMEOUT,
            Op::CloneRestore => CLONE_OP_TIMEOUT,
            _ => OP_TIMEOUT,
        };
        let t0 = Instant::now();
        let outcome = tokio::time::timeout(
            timeout,
            apply_op(
                op, draw, seed, i, pid, serve_pid, &ip, host_port, snap_base, nonces, snapshots,
            ),
        )
        .await;
        let took = t0.elapsed().as_secs_f64();
        match outcome {
            Err(_) => bail!(
                "op {}/{} {:?} neither completed nor errored within {:?}",
                i,
                total_ops,
                op,
                timeout
            ),
            Ok(Err(e)) => bail!(
                "op {}/{} {:?} failed after {:.2}s: {:#}",
                i,
                total_ops,
                op,
                took,
                e
            ),
            Ok(Ok(detail)) => println!(
                "FUZZ seed={} op={}/{} {:?} -> OK in {:.2}s ({})",
                seed, i, total_ops, op, took, detail
            ),
        }
    }

    // The UFFD serve goes first: it is an fcvm process this seed started, and
    // the leak oracles below (correctly) refuse to see stray fcvm/firecracker
    // processes after shutdown. Killing it here also proves the clones that
    // used it are already gone — a serve with live clones refuses to exit.
    common::kill_process(serve_pid).await;
    let _ = serve_child.wait().await;

    // -- Graceful shutdown --------------------------------------------------
    println!("FUZZ seed={} shutting down (SIGTERM to {})", seed, pid);
    let term = tokio::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .output()
        .await
        .context("sending SIGTERM")?;
    ensure!(term.status.success(), "kill -TERM {} failed", pid);
    match tokio::time::timeout(SHUTDOWN_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => println!("FUZZ seed={} fcvm exited: {:?}", seed, status),
        Ok(Err(e)) => bail!("waiting for fcvm exit: {}", e),
        Err(_) => bail!(
            "fcvm (pid {}) did not exit within {:?} of SIGTERM",
            pid,
            SHUTDOWN_TIMEOUT
        ),
    }

    // -- Oracles ------------------------------------------------------------
    // 1) Every ExecNonce nonce appears exactly once in the mapped file.
    let nonce_path = host_dir.join("nonces.txt");
    let content = std::fs::read_to_string(&nonce_path).unwrap_or_default();
    for nonce in nonces.iter() {
        let count = content.lines().filter(|l| l.trim() == nonce).count();
        ensure!(
            count == 1,
            "nonce {} appears {} times in {} (expected exactly once); file:\n{}",
            nonce,
            count,
            nonce_path.display(),
            content
        );
    }
    println!(
        "FUZZ seed={} nonce oracle OK ({} nonces, each exactly once)",
        seed,
        nonces.len()
    );

    // 2) No leaked firecracker/pasta for this VM. Cleanup finishes with fcvm's
    //    exit; poll briefly (bounded) for reaping, then assert hard.
    let deadline = Instant::now() + Duration::from_secs(10);
    while tracked.iter().any(TrackedProc::alive) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    for t in &tracked {
        ensure!(
            !t.alive(),
            "{} (pid {}) leaked: still running after fcvm exit",
            t.label,
            t.pid
        );
    }
    // Belt-and-braces: nothing left on the whole host referencing this VM's
    // name or id (fcvm itself, stray helpers holding the runtime dir, ...).
    for needle in [vm_name, vm_id.as_str()] {
        let out = tokio::process::Command::new("pgrep")
            .args(["-af", needle])
            .output()
            .await
            .context("pgrep leak scan")?;
        // pgrep: 0 = matches (leak), 1 = no matches (clean), 2/3 = pgrep itself
        // broke — which must fail the oracle rather than pass as "no leak".
        ensure!(
            out.status.code() == Some(1),
            "leak scan for {:?} did not come back clean (pgrep exit {:?}):\n{}",
            needle,
            out.status.code(),
            String::from_utf8_lossy(&out.stdout)
        );
    }
    println!("FUZZ seed={} process-leak oracle OK", seed);

    // 3) State file removed.
    let leftover = state_files_for_vm(vm_name);
    ensure!(
        leftover.is_empty(),
        "state file(s) not removed after graceful shutdown: {:?}",
        leftover
    );
    let listed = read_vm_state(pid).await?;
    ensure!(
        listed.is_empty(),
        "fcvm ls still reports pid {} after shutdown",
        pid
    );
    println!("FUZZ seed={} state-file oracle OK", seed);

    Ok(())
}

// ---------------------------------------------------------------------------
// Entry point — always runs (defaults keep it ~2-3 min: 1 seed, 10 ops)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_fuzz_lifecycle_chaos() {
    let seeds_spec = std::env::var("FCVM_FUZZ_SEEDS").unwrap_or_else(|_| "1".to_string());
    let ops: usize = std::env::var("FCVM_FUZZ_OPS")
        .map(|v| {
            v.trim()
                .parse()
                .unwrap_or_else(|_| panic!("FCVM_FUZZ_OPS: bad count {:?}", v))
        })
        .unwrap_or(10);
    let seeds = parse_seeds(&seeds_spec);
    println!(
        "FUZZ run: {} seed(s) {:?}, {} ops each",
        seeds.len(),
        seeds,
        ops
    );

    for &seed in &seeds {
        println!("FUZZ ===== seed {} starting ({} ops) =====", seed, ops);
        let t0 = Instant::now();
        if let Err(e) = run_seed(seed, ops).await {
            panic!(
                "FUZZ seed {} FAILED after {:.1}s: {:#}\n\
                 Replay: make fuzz SEEDS={}, OPS={}\n\
                 (trailing comma = literal seed list; same seed reproduces the same \
                 op schedule — see the FUZZ trace above for the failing interleaving, \
                 then pin it as a failpoint case in test_lifecycle_interleave.rs)",
                seed,
                t0.elapsed().as_secs_f64(),
                e,
                seed,
                ops
            );
        }
        println!(
            "FUZZ ===== seed {} PASSED in {:.1}s =====",
            seed,
            t0.elapsed().as_secs_f64()
        );
    }
    println!("FUZZ all {} seed(s) passed", seeds.len());
}
