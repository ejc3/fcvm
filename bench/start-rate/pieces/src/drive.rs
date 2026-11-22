//! The parent: holds ALIVE instances, prepares COUNT more, starts them at CONCURRENCY, samples
//! the steady cost of everything held, then tears it all down at CONCURRENCY. CPU comes from
//! this process's own cgroup (every descendant, alive or reaped) and from the host's
//! /proc/stat (which also sees kernel threads: netns cleanup, RCU, btrfs commits).

use crate::pieces::PieceArgs;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

#[derive(clap::Args, Clone, Debug)]
pub struct DriveArgs {
    #[command(flatten)]
    pub piece: PieceArgs,
    /// Ops in the timed phase.
    #[arg(long, default_value_t = 100)]
    pub count: usize,
    /// Most ops in flight at once.
    #[arg(long, default_value_t = 1)]
    pub concurrency: usize,
    /// Instances of the piece made and held before the timed phase.
    #[arg(long, default_value_t = 0)]
    pub alive: usize,
    /// Concurrency for making the held and prepared instances (untimed).
    #[arg(long, default_value_t = 64)]
    pub prep_concurrency: usize,
    /// Results directory (created; must not exist).
    #[arg(long)]
    pub out: PathBuf,
    /// Seconds of idle sampling before the timed phase (the held set's own cost).
    #[arg(long, default_value_t = 3)]
    pub idle_secs: u64,
    /// Seconds of steady sampling after the timed phase, with everything held.
    #[arg(long, default_value_t = 10)]
    pub steady_secs: u64,
    /// `perf record -a -g` over the timed phase, at this frequency (0: off).
    #[arg(long, default_value_t = 0)]
    pub perf_freq: u32,
    /// `perf stat -a` events over the timed and steady phases (empty: off).
    #[arg(long, default_value = "")]
    pub perf_events: String,
    /// How to run perf (it needs tracefs for kvm tracepoints).
    #[arg(long, default_value = "sudo -n perf")]
    pub perf_cmd: String,
    /// Time each op from the instance's spawn: process start and preparation included.
    #[arg(long)]
    pub cold: bool,
    /// Run without a cgroup of our own (CPU per op then counts other processes too).
    #[arg(long)]
    pub shared_cgroup: bool,
}

struct Inst {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
}

#[derive(Default, Clone)]
struct Counters {
    at: Option<Instant>,
    cgroup_usec: u64,
    busy_ticks: u64,
    ctxt: u64,
    irqs: BTreeMap<String, u64>,
}

fn read_counters(cgroup: &Option<PathBuf>) -> Result<Counters> {
    let mut c = Counters {
        at: Some(Instant::now()),
        ..Default::default()
    };
    if let Some(dir) = cgroup {
        let stat = std::fs::read_to_string(dir.join("cpu.stat"))?;
        c.cgroup_usec = stat
            .lines()
            .find_map(|l| l.strip_prefix("usage_usec "))
            .context("cpu.stat has no usage_usec")?
            .trim()
            .parse()?;
    }
    (c.busy_ticks, c.ctxt) = parse_stat(&std::fs::read_to_string("/proc/stat")?)?;
    c.irqs = parse_ipis(&std::fs::read_to_string("/proc/interrupts")?);
    Ok(c)
}

/// Busy ticks (user nice system irq softirq steal, not idle or iowait) and the context switch
/// count from /proc/stat.
fn parse_stat(text: &str) -> Result<(u64, u64)> {
    let (mut busy, mut ctxt) = (None, None);
    for line in text.lines() {
        let mut f = line.split_whitespace();
        match f.next() {
            Some("cpu") => {
                let v: Vec<u64> = f.filter_map(|x| x.parse().ok()).collect();
                anyhow::ensure!(v.len() >= 7, "short cpu line in /proc/stat: {line}");
                busy = Some(v[0] + v[1] + v[2] + v[5] + v[6] + v.get(7).copied().unwrap_or(0));
            }
            Some("ctxt") => ctxt = Some(f.next().unwrap_or("").parse()?),
            _ => {}
        }
    }
    Ok((
        busy.context("no cpu line in /proc/stat")?,
        ctxt.context("no ctxt line in /proc/stat")?,
    ))
}

/// Inter-processor interrupt totals by row: arm64 names them IPI0..IPIn, x86 RES, CAL, TLB.
fn parse_ipis(text: &str) -> BTreeMap<String, u64> {
    let mut irqs = BTreeMap::new();
    for line in text.lines() {
        let mut f = line.split_whitespace();
        let Some(label) = f.next() else { continue };
        let label = label.trim_end_matches(':');
        if label.starts_with("IPI") || matches!(label, "RES" | "CAL" | "TLB") {
            irqs.insert(
                label.to_string(),
                f.map_while(|x| x.parse::<u64>().ok()).sum(),
            );
        }
    }
    irqs
}

fn ticks_per_sec() -> f64 {
    unsafe { libc::sysconf(libc::_SC_CLK_TCK) as f64 }
}

/// Deltas between two counter reads, per op (or per second when `per` is the window).
fn delta(a: &Counters, b: &Counters, per: f64) -> Value {
    let secs = b.at.unwrap().duration_since(a.at.unwrap()).as_secs_f64();
    let irqs: BTreeMap<&String, f64> = b
        .irqs
        .iter()
        .map(|(k, v)| (k, (v - a.irqs.get(k).copied().unwrap_or(0)) as f64 / per))
        .collect();
    json!({
        "secs": secs,
        "cgroup_cpu_s": (b.cgroup_usec - a.cgroup_usec) as f64 / 1e6 / per,
        "host_busy_cpu_s": (b.busy_ticks - a.busy_ticks) as f64 / ticks_per_sec() / per,
        "ctxt": (b.ctxt - a.ctxt) as f64 / per,
        "ipi_total": irqs.values().sum::<f64>(),
        "ipi": irqs,
    })
}

fn own_cgroup() -> Result<PathBuf> {
    let text = std::fs::read_to_string("/proc/self/cgroup")?;
    let rel = text
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .context("no cgroup v2 entry in /proc/self/cgroup")?;
    Ok(PathBuf::from(format!("/sys/fs/cgroup{rel}")))
}

fn piece_argv(p: &PieceArgs) -> Vec<String> {
    let mut v = vec![
        "instance".to_string(),
        "--piece".into(),
        clap::ValueEnum::to_possible_value(&p.piece)
            .unwrap()
            .get_name()
            .to_string(),
        "--work-dir".into(),
        p.work_dir.display().to_string(),
        "--track-dirty-pages".into(),
        p.track_dirty_pages.to_string(),
        "--prober".into(),
        clap::ValueEnum::to_possible_value(&p.prober)
            .unwrap()
            .get_name()
            .to_string(),
    ];
    for (flag, value) in [
        ("--snapshot-dir", &p.snapshot_dir),
        ("--firecracker", &p.firecracker),
        ("--uffd-socket", &p.uffd_socket),
        ("--disk-shard-dir", &p.disk_shard_dir),
    ] {
        if let Some(value) = value {
            v.push(flag.into());
            v.push(value.display().to_string());
        }
    }
    v
}

async fn spawn(argv: &[String], log: &std::fs::File) -> Result<Inst> {
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.args(argv).env(
        crate::pieces::DRIVER_PID_ENV,
        std::process::id().to_string(),
    );
    spawn_command(cmd, log)
}

/// Start `cmd` as an instance: its stdin and stdout are the protocol, its stderr the log.
fn spawn_command(mut cmd: std::process::Command, log: &std::fs::File) -> Result<Inst> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log.try_clone()?));
    // No pre_exec: without one the standard library spawns with posix_spawn (a vfork), where
    // a fork of this many-threaded driver costs milliseconds and serializes on its mm. The
    // instance arms its own parent-death signal first thing (`pieces::instance`).
    let mut child = tokio::process::Command::from(cmd).spawn()?;
    let stdin = child.stdin.take().unwrap();
    let lines = BufReader::new(child.stdout.take().unwrap()).lines();
    Ok(Inst {
        child,
        stdin,
        lines,
    })
}

/// How long an instance that reported an error gets to tear down what it made and exit.
const TEARDOWN_AFTER_ERROR: Duration = Duration::from_secs(120);

impl Inst {
    /// Let an instance that failed finish its teardown and exit (it tears down before it
    /// reports the error, and exits after); kill it only if it does not within
    /// [`TEARDOWN_AFTER_ERROR`]. Killing it at once would leave its disk or network behind.
    async fn reap_after_error(&mut self) {
        if tokio::time::timeout(TEARDOWN_AFTER_ERROR, self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.start_kill();
            let _ = self.child.wait().await;
        }
    }

    /// The next protocol line; an `error` line or EOF fails.
    async fn expect(&mut self, word: &str) -> Result<Value> {
        let line = self
            .lines
            .next_line()
            .await?
            .with_context(|| format!("instance exited before '{word}'"))?;
        if let Some(rest) = line.strip_prefix(word) {
            let rest = rest.trim();
            return Ok(if rest.is_empty() {
                Value::Null
            } else {
                serde_json::from_str(rest)?
            });
        }
        bail!("instance said '{line}' instead of '{word}'")
    }
    async fn send(&mut self, word: &str) -> Result<()> {
        self.stdin.write_all(format!("{word}\n").as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }
}

/// Spawn and prepare `n` instances at `conc`; when `run`, also start each one (untimed).
async fn make(
    n: usize,
    conc: usize,
    run: bool,
    argv: &Arc<Vec<String>>,
    log: &Arc<std::fs::File>,
) -> Result<Vec<Inst>> {
    let sem = Arc::new(Semaphore::new(conc.max(1)));
    let mut set = JoinSet::new();
    for _ in 0..n {
        let (sem, argv, log) = (sem.clone(), argv.clone(), log.clone());
        set.spawn(async move {
            let _permit = sem.acquire_owned().await?;
            let mut inst = spawn(&argv, &log).await?;
            let made = async {
                inst.expect("ready").await?;
                if run {
                    inst.send("go").await?;
                    inst.expect("done").await?;
                }
                anyhow::Ok(())
            }
            .await;
            if let Err(error) = made {
                inst.reap_after_error().await;
                return Err(error);
            }
            Ok::<_, anyhow::Error>(inst)
        });
    }
    let mut out = Vec::with_capacity(n);
    while let Some(r) = set.join_next().await {
        out.push(r??);
    }
    Ok(out)
}

struct OpResult {
    inst: Option<Inst>,
    latency_ms: f64,
    child: Value,
    error: Option<String>,
}

/// Send `word` to every instance at `conc`, wait for `reply`, and time each.
async fn phase(
    insts: Vec<Inst>,
    conc: usize,
    word: &'static str,
    reply: &'static str,
) -> Vec<OpResult> {
    let sem = Arc::new(Semaphore::new(conc.max(1)));
    let mut set = JoinSet::new();
    for mut inst in insts {
        let sem = sem.clone();
        set.spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            let t = Instant::now();
            let r = async {
                inst.send(word).await?;
                inst.expect(reply).await
            }
            .await;
            let latency_ms = t.elapsed().as_secs_f64() * 1000.0;
            if reply == "torn" {
                let _ = inst.child.wait().await;
            }
            match r {
                Ok(child) => OpResult {
                    inst: Some(inst),
                    latency_ms,
                    child,
                    error: None,
                },
                Err(e) => {
                    inst.reap_after_error().await;
                    OpResult {
                        inst: None,
                        latency_ms,
                        child: Value::Null,
                        error: Some(format!("{e:#}")),
                    }
                }
            }
        });
    }
    let mut out = Vec::new();
    while let Some(r) = set.join_next().await {
        out.push(r.expect("phase task panicked"));
    }
    out
}

/// The timed phase with `--cold`: each op is the instance's spawn, its preparation and the
/// piece, so the process start and the preparation are part of what is measured.
async fn cold_phase(
    n: usize,
    conc: usize,
    argv: &Arc<Vec<String>>,
    log: &Arc<std::fs::File>,
) -> Vec<OpResult> {
    let sem = Arc::new(Semaphore::new(conc.max(1)));
    let mut set = JoinSet::new();
    for _ in 0..n {
        let (sem, argv, log) = (sem.clone(), argv.clone(), log.clone());
        set.spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            let t = Instant::now();
            let mut made = None;
            let r = async {
                let inst = made.insert(spawn(&argv, &log).await?);
                inst.expect("ready").await?;
                inst.send("go").await?;
                inst.expect("done").await
            }
            .await;
            let latency_ms = t.elapsed().as_secs_f64() * 1000.0;
            match r {
                Ok(child) => OpResult {
                    inst: made,
                    latency_ms,
                    child,
                    error: None,
                },
                Err(e) => {
                    if let Some(mut inst) = made {
                        inst.reap_after_error().await;
                    }
                    OpResult {
                        inst: None,
                        latency_ms,
                        child: Value::Null,
                        error: Some(format!("{e:#}")),
                    }
                }
            }
        });
    }
    let mut out = Vec::new();
    while let Some(r) = set.join_next().await {
        out.push(r.expect("phase task panicked"));
    }
    out
}

fn pct(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    sorted[((sorted.len() as f64 - 1.0) * q).round() as usize]
}

fn dist(mut v: Vec<f64>) -> Value {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    json!({"p50": pct(&v, 0.5), "p90": pct(&v, 0.9), "p99": pct(&v, 0.99), "max": pct(&v, 1.0), "n": v.len()})
}

fn summarize(results: &[OpResult], wall: f64, a: &Counters, b: &Counters, idle: &Value) -> Value {
    let ok: Vec<&OpResult> = results.iter().filter(|r| r.error.is_none()).collect();
    let n = ok.len().max(1) as f64;
    let mut stages: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for r in &ok {
        if let Some(map) = r.child.get("stages").and_then(|s| s.as_object()) {
            for (k, v) in map {
                stages
                    .entry(k.clone())
                    .or_default()
                    .push(v.as_f64().unwrap_or(f64::NAN));
            }
        }
    }
    let per_op = delta(a, b, n);
    // The held set's own cost over the window, from the idle sample, comes off each op.
    let idle_cg = idle["cgroup_cpu_s"].as_f64().unwrap_or(0.0);
    let idle_host = idle["host_busy_cpu_s"].as_f64().unwrap_or(0.0);
    json!({
        "ok": ok.len(),
        "failed": results.len() - ok.len(),
        "errors": results.iter().filter_map(|r| r.error.clone()).take(5).collect::<Vec<_>>(),
        "wall_s": wall,
        "ops_per_s": ok.len() as f64 / wall,
        "latency_ms": dist(ok.iter().map(|r| r.latency_ms).collect()),
        "op_ms": dist(ok.iter().filter_map(|r| r.child["ms"].as_f64()).collect()),
        "stages_ms": stages.into_iter().map(|(k, v)| (k, dist(v))).collect::<BTreeMap<_, _>>(),
        "per_op": per_op,
        "cgroup_cpu_s_per_op_net": per_op["cgroup_cpu_s"].as_f64().unwrap() - idle_cg * wall / n,
        "host_cpu_s_per_op_net": per_op["host_busy_cpu_s"].as_f64().unwrap() - idle_host * wall / n,
    })
}

/// A perf collector over a phase. Its stderr goes to `err`; a perf that exits on its own
/// before it is stopped, or exits non-zero when stopped, fails the run: a collector that
/// cannot run must say so.
struct Perf(Option<(std::process::Child, PathBuf)>);

impl Perf {
    fn start(cmd: &str, args: &[String], err: &Path) -> Result<Self> {
        use std::os::unix::process::CommandExt as _;
        let mut words = cmd.split_whitespace();
        let mut c = std::process::Command::new(words.next().context("empty perf command")?);
        c.args(words)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(err)?));
        // Its own process group: SIGINT reaches perf through sudo without reaching us.
        c.process_group(0);
        Ok(Perf(Some((c.spawn()?, err.to_path_buf()))))
    }
    fn stop(&mut self) -> Result<()> {
        let Some((mut child, err)) = self.0.take() else {
            return Ok(());
        };
        let exited_early = child.try_wait()?;
        if exited_early.is_none() {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(child.id() as i32),
                nix::sys::signal::Signal::SIGINT,
            );
        }
        let status = child.wait()?;
        if exited_early.is_some() || !status.success() {
            bail!(
                "the perf collector {} ({status}): {}",
                if exited_early.is_some() {
                    "exited before it was stopped"
                } else {
                    "failed"
                },
                std::fs::read_to_string(&err).unwrap_or_default().trim()
            );
        }
        Ok(())
    }
}

fn perf_stat(args: &DriveArgs, out: &Path, name: &str) -> Result<Perf> {
    if args.perf_events.is_empty() {
        return Ok(Perf(None));
    }
    Perf::start(
        &args.perf_cmd,
        &[
            "stat".into(),
            "-a".into(),
            "-x".into(),
            ",".into(),
            "-o".into(),
            out.join(name).display().to_string(),
            "-e".into(),
            args.perf_events.clone(),
        ],
        &out.join(format!("{name}.err")),
    )
}

pub async fn run(args: DriveArgs) -> Result<()> {
    std::fs::create_dir(&args.out)
        .with_context(|| format!("creating {} (it must not exist)", args.out.display()))?;
    let cgroup = if args.shared_cgroup {
        None
    } else {
        let dir = own_cgroup()?;
        let procs = std::fs::read_to_string(dir.join("cgroup.procs"))?;
        let others: Vec<&str> = procs
            .lines()
            .filter(|p| *p != std::process::id().to_string())
            .collect();
        if !others.is_empty() {
            bail!(
                "cgroup {} holds other processes ({:?}): run under `systemd-run --user --scope` \
                 (the Makefile target does) or pass --shared-cgroup",
                dir.display(),
                others
            );
        }
        Some(dir)
    };
    let log = Arc::new(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(args.out.join("instances.log"))?,
    );
    let argv = Arc::new(piece_argv(&args.piece));

    let t = Instant::now();
    let mut held = make(args.alive, args.prep_concurrency, true, &argv, &log).await?;
    let alive_s = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let prepared = if args.cold {
        Vec::new()
    } else {
        make(args.count, args.prep_concurrency, false, &argv, &log).await?
    };
    let prep_s = t.elapsed().as_secs_f64();

    let i0 = read_counters(&cgroup)?;
    tokio::time::sleep(Duration::from_secs(args.idle_secs)).await;
    let i1 = read_counters(&cgroup)?;
    let idle = delta(
        &i0,
        &i1,
        i1.at.unwrap().duration_since(i0.at.unwrap()).as_secs_f64(),
    );

    let mut record = if args.perf_freq > 0 {
        Perf::start(
            &args.perf_cmd,
            &[
                "record".into(),
                "-a".into(),
                "-g".into(),
                "-F".into(),
                args.perf_freq.to_string(),
                "-o".into(),
                args.out.join("perf.data").display().to_string(),
            ],
            &args.out.join("perf-record.err"),
        )?
    } else {
        Perf(None)
    };
    let mut stat = perf_stat(&args, &args.out, "perfstat-timed.csv")?;
    if record.0.is_some() || stat.0.is_some() {
        // perf needs a moment to open its events on 192 CPUs.
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let a = read_counters(&cgroup)?;
    let t = Instant::now();
    let results = if args.cold {
        cold_phase(args.count, args.concurrency, &argv, &log).await
    } else {
        phase(prepared, args.concurrency, "go", "done").await
    };
    let wall = t.elapsed().as_secs_f64();
    let b = read_counters(&cgroup)?;
    let mut perf_errors: Vec<String> = Vec::new();
    for collector in [&mut record, &mut stat] {
        if let Err(error) = collector.stop() {
            perf_errors.push(format!("{error:#}"));
        }
    }
    let timed = summarize(&results, wall, &a, &b, &idle);
    let mut ops = String::new();
    for r in &results {
        ops.push_str(
            &json!({"latency_ms": r.latency_ms, "child": r.child, "error": r.error}).to_string(),
        );
        ops.push('\n');
    }
    std::fs::write(args.out.join("ops.jsonl"), ops)?;
    held.extend(results.into_iter().filter_map(|r| r.inst));

    let mut stat = perf_stat(&args, &args.out, "perfstat-steady.csv")?;
    let s0 = read_counters(&cgroup)?;
    tokio::time::sleep(Duration::from_secs(args.steady_secs)).await;
    let s1 = read_counters(&cgroup)?;
    if let Err(error) = stat.stop() {
        perf_errors.push(format!("{error:#}"));
    }
    let steady_secs = s1.at.unwrap().duration_since(s0.at.unwrap()).as_secs_f64();
    let steady = delta(&s0, &s1, steady_secs);

    let held_n = held.len();
    let d0 = read_counters(&cgroup)?;
    let t = Instant::now();
    let torn = phase(held, args.concurrency, "exit", "torn").await;
    let down_wall = t.elapsed().as_secs_f64();
    let d1 = read_counters(&cgroup)?;
    let teardown = summarize(&torn, down_wall, &d0, &d1, &json!({}));

    let summary = json!({
        "piece": args.piece.piece,
        "count": args.count,
        "concurrency": args.concurrency,
        "alive": args.alive,
        "cold": args.cold,
        "uffd": args.piece.uffd_socket.is_some(),
        "cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "alive_make_s": alive_s,
        "prep_s": prep_s,
        "idle_per_s": idle,
        "timed": timed,
        "steady_held": held_n,
        "steady_per_s": steady,
        "steady_per_instance_per_s": delta(&s0, &s1, steady_secs * held_n.max(1) as f64),
        "teardown": teardown,
        "perf_errors": perf_errors,
    });
    std::fs::write(
        args.out.join("summary.json"),
        serde_json::to_string_pretty(&summary)?,
    )?;
    println!("{}", serde_json::to_string(&summary)?);
    let failed = summary["timed"]["failed"].as_u64().unwrap_or(0)
        + summary["teardown"]["failed"].as_u64().unwrap_or(0);
    if failed > 0 {
        bail!(
            "{failed} ops failed; see {}",
            args.out.join("instances.log").display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn stat_counts_busy_without_idle_or_iowait() {
        let text = "cpu  10 1 5 1000 50 2 3 4 0 0\ncpu0 10 1 5 1000 50 2 3 4 0 0\nctxt 777\n";
        assert_eq!(parse_stat(text).unwrap(), (10 + 1 + 5 + 2 + 3 + 4, 777));
        assert!(parse_stat("cpu 1 2 3\nctxt 1\n").is_err());
        assert!(parse_stat("ctxt 1\n").is_err());
    }

    #[test]
    fn ipis_are_summed_across_cpus_per_row() {
        let arm = "           CPU0       CPU1\n 11:   5   6   GICv3  27 Level  arch_timer\n\
                   IPI0:        3          4       Rescheduling interrupts\n\
                   IPI1:       10         20       Function call interrupts\n";
        let got = parse_ipis(arm);
        assert_eq!(got.get("IPI0"), Some(&7));
        assert_eq!(got.get("IPI1"), Some(&30));
        assert_eq!(got.len(), 2);
        let x86 =
            "RES:  1  2  Rescheduling interrupts\nCAL: 3 4 Function call interrupts\nNMI: 9 9 x\n";
        let got = parse_ipis(x86);
        assert_eq!((got["RES"], got["CAL"], got.len()), (3, 7, 2));
    }

    /// An instance that reports an error is given time to finish its teardown and exit: the
    /// driver must not kill it mid-teardown, which left clone disks behind.
    #[tokio::test]
    async fn a_failed_instance_finishes_its_teardown() {
        let dir = tempfile::tempdir().unwrap();
        let torn = dir.path().join("torn");
        let log = std::fs::File::create(dir.path().join("log")).unwrap();
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(format!(
            "echo ready; read go; echo 'error op: boom'; sleep 0.3; touch {}; exit 1",
            torn.display()
        ));
        let inst = spawn_command(cmd, &log).unwrap();
        let mut inst = inst;
        inst.expect("ready").await.unwrap();
        let results = phase(vec![inst], 1, "go", "done").await;
        assert_eq!(results.len(), 1);
        assert!(results[0].error.as_deref().unwrap().contains("boom"));
        assert!(
            torn.exists(),
            "the driver killed the instance before its teardown ended"
        );
    }

    /// A perf that cannot run fails the run with what it printed; one that runs until it is
    /// stopped and exits cleanly does not.
    #[test]
    fn a_perf_collector_that_fails_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let err = dir.path().join("perf.err");
        let mut failing = Perf::start(
            "sh",
            &["-c".into(), "echo no such event >&2; exit 3".into()],
            &err,
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let error = failing
            .stop()
            .expect_err("a perf that exited 3 is not a collector");
        assert!(format!("{error:#}").contains("no such event"), "{error:#}");

        // A handler set by the program itself: a shell cannot trap a SIGINT it inherited
        // ignored, which a test run from a background job does.
        let mut running = Perf::start(
            "python3",
            &[
                "-c".into(),
                "import signal, sys, time\n\
                 signal.signal(signal.SIGINT, lambda *_: sys.exit(0))\n\
                 time.sleep(60)"
                    .into(),
            ],
            &err,
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        running.stop().unwrap();
    }

    #[derive(clap::Parser)]
    struct Wrap {
        #[command(subcommand)]
        cmd: Sub,
    }
    #[derive(clap::Subcommand)]
    enum Sub {
        Instance(PieceArgs),
    }

    #[test]
    fn the_instance_argv_parses_back_to_the_same_piece_args() {
        let given = PieceArgs {
            piece: crate::pieces::Piece::Vmm,
            work_dir: "/w".into(),
            snapshot_dir: Some("/s".into()),
            firecracker: Some("/fc".into()),
            uffd_socket: Some("/u.sock".into()),
            disk_shard_dir: Some("/d".into()),
            track_dirty_pages: false,
            prober: crate::pieces::Prober::Nsenter,
        };
        let argv = std::iter::once("x".to_string()).chain(piece_argv(&given));
        let Sub::Instance(back) = Wrap::parse_from(argv).cmd;
        assert_eq!(format!("{given:?}"), format!("{back:?}"));
    }
}
