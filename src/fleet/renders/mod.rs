//! Pages mode on a host ("One VM per page"): the processes behind the wall.
//!
//! Two sides render the same replayed pages, each in a cpuset of its own:
//!
//! | cgroup under the unit | holds |
//! |---|---|
//! | `renders-fcvm` | the memory server and every clone (`fcvm snapshot run`, Firecracker, pasta, the namespace holder). One pool: nothing below it is pinned or limited |
//! | `renders-engines` | the Kitesurf engines, one child cgroup each (`e<n>`), for its memory |
//! | `renders-harness` | `replay-fcvm` and `replay-engines` (each side's replay processes) and `drivers` (the workers that drive the renders, and the VM the snapshot is made from). One pool like the sides: the three are cgroups of their own for their counters, on the whole harness set unless `--replay-cpus` confines the replays |
//!
//! The pure half is `fleet_core::renders_wall::Wall`: it is fed every completed render and
//! each side's cgroup counters and answers how many renders each side runs in flight, and
//! the report. This module starts and ends the processes and moves the lines between them.
//! The fleet agent (`agent::renders`) and `fcvm fleet renders-bench` both run it through
//! [`session`], the benchmark without a coordinator or viewers, so the benchmark's reference
//! figures and the live wall's come from one piece of code.
//!
//! Start, in order: sweep what an earlier module left; start the reaper ([`reap`]); make
//! the cgroups; start the replays; make the snapshot if it is absent; start the memory
//! server, the engines and the workers; render. Each stage is written to `--status-file`. Whatever
//! stage the module ends at, and however it ends, the reaper removes all of it.
//!
//! A render in flight is one whose answer is not in hand. A clone's teardown follows its
//! answer and is outside the render's time, so the fcvm side starts the next clone while
//! the last one is torn down: it has more workers than its most renders in flight. A
//! clone reserves its memory in 2 MiB pages when it starts, so the side starts one only
//! while the pool has room for it and counts the times it had to wait.

pub mod overlay;
pub mod reap;

use std::collections::BTreeMap;
use std::io::Write as _;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use fleet_core::renders::{
    Harness, OrderOverlay, RendersHost, RendersOrder, RendersPage, RendersReport,
};
use fleet_core::renders_search::SearchParams;
use fleet_core::renders_wall::{Completed, Wall, SIDES};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{mpsc, watch};

use crate::cli::FleetRendersBenchArgs;
use crate::fleet::cpuset::{self, CpuCgroup, Join, Unit};
use crate::fleet::now_ms;

const FCVM: usize = 0;
const KITE: usize = 1;

/// The module's cgroups under the unit, by name.
pub const CGROUPS: [&str; 3] = ["renders-fcvm", "renders-engines", "renders-harness"];

/// Engines started at once: an engine's start is a burst of several cores.
const ENGINE_STARTS_AT_ONCE: usize = 16;

/// After this many failed renders in a row a side starts nothing for a while.
const FAILURES_BEFORE_A_WAIT: u32 = 3;
const WAIT_AFTER_FAILURES: Duration = Duration::from_secs(1);

/// Whether a kernel release has the bridge teardown fix, without which a clone's teardown
/// stalls for seconds under load and no figure from the host is published: the
/// repository's own kernels (`-fcvm`), 6.18.60 and later of 6.18, 7.2 and later, and
/// Ubuntu's `7.0.0-1017-aws` and later.
pub fn kernel_has_bridge_fix(release: &str) -> bool {
    if release.contains("-fcvm") {
        return true;
    }
    let n: Vec<u64> = release
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .take(4)
        .filter_map(|s| s.parse().ok())
        .collect();
    let at = |i: usize| n.get(i).copied().unwrap_or(0);
    (at(0), at(1)) >= (7, 2)
        || (at(0) == 6 && at(1) == 18 && at(2) >= 60)
        || (at(0) == 7 && at(1) == 0 && at(2) == 0 && release.ends_with("-aws") && at(3) >= 1017)
}

/// A page's tile id from its address: the host and path, with every run of other
/// characters as one dash.
pub fn page_id(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let mut id = String::new();
    for c in rest.chars() {
        if c.is_ascii_alphanumeric() || c == '.' {
            id.push(c.to_ascii_lowercase());
        } else if !id.ends_with('-') {
            id.push('-');
        }
    }
    id.trim_matches('-').to_string()
}

/// Every host name the captures under `corpus` name, for the engines' name file.
fn corpus_hosts(corpus: &Path) -> Result<Vec<String>> {
    let mut hosts = std::collections::BTreeSet::new();
    for entry in
        std::fs::read_dir(corpus).with_context(|| format!("reading {}", corpus.display()))?
    {
        let index = entry?.path().join("index.json");
        let Ok(text) = std::fs::read_to_string(&index) else {
            continue;
        };
        let doc: Value =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", index.display()))?;
        for key in ["resources", "redirects"] {
            for address in doc[key].as_object().into_iter().flat_map(|o| o.keys()) {
                if let Some(host) = url::Url::parse(address)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_string))
                {
                    hosts.insert(host);
                }
            }
        }
    }
    anyhow::ensure!(
        !hosts.is_empty(),
        "no captured page under {}",
        corpus.display()
    );
    Ok(hosts.into_iter().collect())
}

/// `HugePages_Total` and the pages a new reservation can still take (`HugePages_Free` less
/// `HugePages_Rsvd`), from `/proc/meminfo`'s text.
fn hugepages(meminfo: &str) -> (u64, u64) {
    let field = |name: &str| {
        meminfo
            .lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
            .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
            .unwrap_or(0)
    };
    (
        field("HugePages_Total"),
        field("HugePages_Free").saturating_sub(field("HugePages_Rsvd")),
    )
}

/// Whether one more clone may start. A clone reserves `clone_pages` when its VMM maps its
/// memory, a moment after it is started, so what the kernel reports free lags the clones
/// just started. The pool must therefore hold the memory server's copy, every clone in
/// flight or in teardown (`busy`) and this one by count, and the kernel must also report
/// room for one.
fn pool_has_room(total: u64, room: u64, clone_pages: u64, busy: u64) -> bool {
    (busy + 2) * clone_pages <= total && room >= clone_pages
}

/// Workers of the fcvm side for `most` renders in flight: half as many again, and two more,
/// because a clone's teardown outlasts its answer and the next clone starts meanwhile.
fn fcvm_workers(most: u32) -> u32 {
    most + most / 2 + 2
}

/// The most renders in flight a pool of `total` 2 MiB pages carries with no wait: each of
/// the side's workers may hold a clone, and the memory server holds one clone's worth. A
/// side asked for more would sit at levels it never reaches and report them as its limit.
fn pool_cap(total: u64, clone_pages: u64) -> u32 {
    if clone_pages == 0 {
        return u32::MAX;
    }
    let clones = total / clone_pages;
    let mut most = 0u32;
    while u64::from(fcvm_workers(most + 1)) < clones {
        most += 1;
    }
    most
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// How much of its records a module keeps, when it is bounded (under the fleet agent, which
/// runs for as long as its host; the benchmark keeps a run's whole record as its evidence).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    /// `renders.jsonl` and `samples.jsonl` are rotated at these sizes ([`Records`]).
    pub renders_bytes: u64,
    pub samples_bytes: u64,
    /// The failed clones' logs kept, newest first ([`prune_clone_logs`]).
    pub failed_clone_logs: usize,
}

/// A records file of JSON lines. With a limit, a line that would take it past the limit
/// first renames it to `<name>.1` (replacing the one before) and starts a new one, so the file
/// and its previous one hold at most twice the limit and a reader sees whole lines.
struct Records {
    path: PathBuf,
    out: std::io::BufWriter<std::fs::File>,
    written: u64,
    limit: Option<u64>,
}

impl Records {
    fn create(path: PathBuf, limit: Option<u64>) -> std::io::Result<Self> {
        let out = std::io::BufWriter::new(std::fs::File::create(&path)?);
        Ok(Records {
            path,
            out,
            written: 0,
            limit,
        })
    }

    fn line(&mut self, line: &str) {
        let size = line.len() as u64 + 1;
        if self
            .limit
            .is_some_and(|limit| self.written > 0 && self.written + size > limit)
        {
            if let Err(e) = self.rotate() {
                tracing::warn!(target: "renders", "rotating {}: {e}", self.path.display());
            }
        }
        if writeln!(self.out, "{line}").is_ok() {
            self.written += size;
        }
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        self.out.flush()?;
        let mut previous = self.path.clone().into_os_string();
        previous.push(".1");
        std::fs::rename(&self.path, previous)?;
        self.out = std::io::BufWriter::new(std::fs::File::create(&self.path)?);
        self.written = 0;
        Ok(())
    }

    fn flush(&mut self) {
        let _ = self.out.flush();
    }
}

/// Remove the failed clones' logs under `dir` (one directory a worker) past the newest
/// `keep`. A file written in the last `settled` is a render's that is still under way and is
/// neither counted nor removed. Returns how many were removed.
fn prune_clone_logs(
    dir: &Path,
    keep: usize,
    settled: Duration,
    now: std::time::SystemTime,
) -> usize {
    let mut logs: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|worker| {
            std::fs::read_dir(worker.path())
                .into_iter()
                .flatten()
                .flatten()
        })
        .filter_map(|e| {
            let meta = e.metadata().ok().filter(|m| m.is_file())?;
            let modified = meta.modified().ok()?;
            let age = now.duration_since(modified).unwrap_or_default();
            (age >= settled).then(|| (modified, e.path()))
        })
        .collect();
    logs.sort_by(|a, b| b.0.cmp(&a.0));
    logs.iter()
        .skip(keep)
        .filter(|(_, path)| std::fs::remove_file(path).is_ok())
        .count()
}

/// Write `bytes` to `path` so a reader sees the old file or the new one.
fn write_by_rename(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Where the module is, for whoever watches it start and stop.
struct Status {
    path: Option<PathBuf>,
    /// A stage at which the module waits until it is ended (`--hold-at`): the teardown
    /// check ends a module at each stage, and a stage can last a millisecond.
    hold_at: Option<String>,
}

impl Status {
    async fn set(&self, stage: &str) {
        tracing::info!(target: "renders", stage, "pages mode");
        if let Some(path) = &self.path {
            let line = json!({"stage": stage, "pid": std::process::id(), "at": now_ms()});
            if let Err(e) = write_by_rename(path, format!("{line}\n").as_bytes()) {
                tracing::warn!(target: "renders", "writing {}: {e}", path.display());
            }
        }
        if self.hold_at.as_deref() == Some(stage) {
            std::future::pending::<()>().await;
        }
    }
}

fn log_file(dir: &Path, name: &str) -> Result<(PathBuf, std::fs::File)> {
    let path = dir.join(name);
    let file =
        std::fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
    Ok((path, file))
}

/// Start `cmd` in the cgroup at `join`, its output in `log`. It is killed when its handle
/// is dropped; what it started itself is the cgroup's and the reaper's to end.
fn spawn_in(join: &Join, mut cmd: std::process::Command, log: std::fs::File) -> Result<Child> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    cmd.stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    join.confine(&mut cmd);
    tokio::process::Command::from(cmd)
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {program} in {}", join.dir().display()))
}

fn tail(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().rev().take(6).collect();
    lines.into_iter().rev().collect::<Vec<_>>().join(" | ")
}

/// Wait until `log` holds `needle`. A process that exits first, or says nothing within
/// `deadline`, is an error that quotes the log's last lines.
async fn wait_for_line(
    what: &str,
    log: &Path,
    needle: &str,
    child: &mut Child,
    deadline: Duration,
) -> Result<()> {
    let until = Instant::now() + deadline;
    loop {
        if std::fs::read_to_string(log).is_ok_and(|t| t.contains(needle)) {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            bail!(
                "{what} exited ({status}) before it was ready: {}",
                tail(log)
            );
        }
        if Instant::now() >= until {
            bail!("{what} was not ready within {deadline:?}: {}", tail(log));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The body of an HTTP/1.x answer in `bytes`, once all of it is there: `Content-Length`
/// bytes after the head, or everything up to the last chunk of a chunked body (returned
/// as sent). `None` while more is to come; an answer with neither is whole at end of file.
fn http_body(bytes: &[u8], at_eof: bool) -> Option<String> {
    // The head is ASCII; the body is counted and cut in bytes and decoded afterwards.
    let split = bytes.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&bytes[..split]);
    let body = &bytes[split + 4..];
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    if !head.starts_with("HTTP/1.") {
        return at_eof.then(String::new);
    }
    let header = |name: &str| {
        head.lines().skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_string())
        })
    };
    if let Some(length) = header("content-length").and_then(|v| v.parse::<usize>().ok()) {
        return (body.len() >= length).then(|| text(&body[..length]));
    }
    if header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        return body.ends_with(b"0\r\n\r\n").then(|| text(body));
    }
    at_eof.then(|| text(body))
}

/// One HTTP GET: the body and how long the whole answer took. The answer is whole when its
/// length says so; a server that keeps the connection open is not waited for.
async fn http_get(
    addr: SocketAddr,
    host: &str,
    path: &str,
    limit: Duration,
) -> Option<(String, f64)> {
    let started = Instant::now();
    let body = tokio::time::timeout(limit, async {
        let mut stream = tokio::net::TcpStream::connect(addr).await.ok()?;
        let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.ok()?;
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 16384];
        loop {
            let n = stream.read(&mut chunk).await.ok()?;
            bytes.extend_from_slice(&chunk[..n]);
            if let Some(body) = http_body(&bytes, n == 0) {
                return Some(body);
            }
            if n == 0 {
                return None;
            }
        }
    })
    .await
    .ok()??;
    Some((body, started.elapsed().as_secs_f64() * 1000.0))
}

/// What an engine process is started from.
#[derive(Debug, Clone)]
struct EngineSpec {
    /// The directory of the build's `node`.
    nodebin: PathBuf,
    /// The Kitesurf repository.
    src: PathBuf,
    /// The bundle, by its real path: its assets are found relative to it.
    dist: PathBuf,
    /// The engines' name files (`hosts`, `nsswitch.conf`, `resolv.conf`).
    ns: PathBuf,
    /// The replay's CA, which the engines trust.
    ca: PathBuf,
    logs: PathBuf,
    port_base: u16,
}

/// Each engine sees the captured hosts at the replay's address and no resolver, through
/// name files mounted over `/etc` in a mount namespace of its own.
const ENGINE_SCRIPT: &str = r#"set -e
mount --bind "$1/hosts" /etc/hosts
mount --bind "$1/nsswitch.conf" /etc/nsswitch.conf
mount --bind "$1/resolv.conf" /etc/resolv.conf
cd "$2"
exec node scripts/wpt-engine.mjs serve --port "$3" --dist "$4""#;

impl EngineSpec {
    fn addr(&self, index: usize) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, self.port_base + index as u16))
    }

    /// Start engine `index` in its cgroup and wait until it answers `/health`. The time is
    /// from the spawn to that answer, in ms.
    async fn start(&self, index: usize, join: &Join) -> Result<(Child, f64)> {
        let started = Instant::now();
        let (log_path, log) = log_file(&self.logs, &format!("engine-{index}.log"))?;
        let mut cmd = std::process::Command::new("unshare");
        cmd.args([
            "--user",
            "--map-root-user",
            "--mount",
            "--",
            "/bin/sh",
            "-c",
            ENGINE_SCRIPT,
            "_",
        ])
        .arg(&self.ns)
        .arg(&self.src)
        .arg(self.addr(index).port().to_string())
        .arg(&self.dist)
        .env("PATH", format!("{}:/usr/bin:/bin", self.nodebin.display()))
        .env("NODE_EXTRA_CA_CERTS", &self.ca)
        .env("WRANGLER_SEND_METRICS", "false")
        .env("WRANGLER_LOG_PATH", self.logs.join("wrangler"))
        .env("CI", "1");
        let mut child = spawn_in(join, cmd, log)?;
        let until = started + Duration::from_secs(180);
        loop {
            if let Some(status) = child.try_wait()? {
                bail!(
                    "engine {index} exited ({status}) at start: {}",
                    tail(&log_path)
                );
            }
            if let Some((body, _)) = http_get(
                self.addr(index),
                "127.0.0.1",
                "/health",
                Duration::from_secs(2),
            )
            .await
            {
                if body.trim() == "kitesurf: OK" {
                    return Ok((child, started.elapsed().as_secs_f64() * 1000.0));
                }
            }
            if Instant::now() >= until {
                bail!("engine {index} never answered /health: {}", tail(&log_path));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// End engine `index` (every process in its cgroup) and start it again.
    async fn recycle(&self, index: usize, join: Join, old: Option<Child>) -> Result<(Child, f64)> {
        let started = Instant::now();
        std::fs::write(join.dir().join("cgroup.kill"), "1")
            .with_context(|| format!("ending engine {index}"))?;
        let until = started + Duration::from_secs(20);
        while cpuset::populated(join.dir()) {
            anyhow::ensure!(
                Instant::now() < until,
                "engine {index} outlived cgroup.kill"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if let Some(mut old) = old {
            let _ = old.wait().await;
        }
        let (child, _) = self.start(index, &join).await?;
        Ok((child, started.elapsed().as_secs_f64() * 1000.0))
    }
}

struct Engine {
    join: Join,
    child: Option<Child>,
    up: bool,
    /// Being ended and started again: for its memory (a recycle) or because it died or its
    /// renders kept failing (`restarting`).
    recycling: bool,
    restarting: bool,
    /// Renders that failed on it in a row.
    failed_in_a_row: u32,
    /// When to try again after a start that failed.
    retry_at: Option<Instant>,
    /// The anonymous memory at which it is started again; 0 never.
    recycle_at: u64,
}

/// The memory at which engine `index` of `count` is first started again. The engines start
/// together and render the same pages in rotation, so they would all reach the threshold
/// within minutes of each other, and a side that starts its engines again in one wave
/// loses a tenth of its rate while it does. The first thresholds are spread evenly from
/// half the threshold up to the whole of it; every later one is the whole.
fn first_recycle_at(index: usize, count: usize, threshold: u64) -> u64 {
    if threshold == 0 || count <= 1 {
        return threshold;
    }
    threshold / 2 + threshold / 2 * (index as u64 + 1) / count as u64
}

/// Whether an engine is started again now: its process exited, its renders failed
/// [`FAILURES_BEFORE_A_WAIT`] times in a row, or an earlier start failed and its retry is
/// due. Never while a start is under way.
fn engine_needs_a_start(
    up: bool,
    starting: bool,
    exited: bool,
    failed_in_a_row: u32,
    retry_due: bool,
) -> bool {
    if starting {
        return false;
    }
    if up {
        exited || failed_in_a_row >= FAILURES_BEFORE_A_WAIT
    } else {
        retry_due
    }
}

/// How long after a failed start an engine is tried again.
const ENGINE_RETRY: Duration = Duration::from_secs(5);

/// A job a worker carries.
struct Job {
    seq: u64,
    page: usize,
    level: u32,
    /// Whether the wall was told its answer (an fcvm worker says so before its teardown).
    answered: bool,
    inject: Option<String>,
}

struct Worker {
    child: Child,
    stdin: Option<ChildStdin>,
    ready: bool,
    job: Option<Job>,
}

enum Event {
    Line {
        side: usize,
        index: usize,
        line: String,
    },
    Gone {
        side: usize,
        index: usize,
    },
    Engine {
        index: usize,
        result: Result<(Child, f64)>,
    },
    Probe {
        side: usize,
        ms: Option<f64>,
    },
}

/// What the workers of a side are started with.
#[derive(Debug, Clone)]
struct WorkerSpec {
    bench: PathBuf,
    logs: PathBuf,
    timeout_s: u32,
    viewport: String,
    /// The fcvm side's: the binary, the memory server, the store, where a clone joins.
    fcvm: Option<(PathBuf, u32, PathBuf, PathBuf)>,
    run_id: String,
}

fn spawn_worker(
    spec: &WorkerSpec,
    side: usize,
    index: usize,
    join: &Join,
    tx: &mpsc::UnboundedSender<Event>,
) -> Result<Worker> {
    let name = SIDES[side];
    let (_, log) = log_file(&spec.logs, &format!("worker-{name}-{index}.log"))?;
    let mut cmd = std::process::Command::new("python3");
    cmd.arg(spec.bench.join("renders_worker.py"))
        .args(["--side", name, "--index", &index.to_string()])
        .arg("--bench")
        .arg(&spec.bench)
        .args(["--timeout", &spec.timeout_s.to_string()])
        .args(["--viewport", &spec.viewport])
        .env(crate::cgroup::PARENT_ENV, "none");
    if let Some((fcvm, serve_pid, data_root, procs)) = &spec.fcvm {
        cmd.args(["--run-id", &spec.run_id])
            .arg("--out-dir")
            .arg(spec.logs.join("clones"))
            .arg("--fcvm")
            .arg(fcvm)
            .args(["--serve-pid", &serve_pid.to_string()])
            .arg("--data-root")
            .arg(data_root)
            .arg("--cgroup-procs")
            .arg(procs);
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(log);
    join.confine(&mut cmd);
    let mut child = tokio::process::Command::from(cmd)
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting worker {index} of the {name} side"))?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().context("the worker's stdout")?;
    let tx = tx.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send(Event::Line { side, index, line }).is_err() {
                return;
            }
        }
        let _ = tx.send(Event::Gone { side, index });
    });
    Ok(Worker {
        child,
        stdin,
        ready: false,
        job: None,
    })
}

struct Side {
    group: CpuCgroup,
    drivers: Join,
    spec: WorkerSpec,
    workers: Vec<Worker>,
    /// Renders whose answer is not in hand.
    in_flight: u32,
    level: u32,
    cursor: usize,
    seq: u64,
    /// Times a clone was not started because the 2 MiB pool had no room for it.
    pool_waits: u64,
    /// Workers started again after one exited.
    respawns: u64,
    /// Failed renders in a row, and until when the side starts nothing after three: a side
    /// whose renders fail at once would otherwise fail thousands a minute.
    failed_in_a_row: u32,
    wait_until: Option<Instant>,
}

struct Module {
    pages: Vec<RendersPage>,
    wall: Wall,
    sides: Vec<Side>,
    engines: Vec<Engine>,
    engine_spec: Option<EngineSpec>,
    recycle_bytes: u64,
    recycles: u64,
    recycle_ms: Vec<f64>,
    /// Engines started again because they died or kept failing.
    restarts: u64,
    /// Engine starts under way (recycles and restarts).
    starting: tokio::task::JoinSet<()>,
    /// The host's kernel release, for every record.
    kernel: String,
    /// 2 MiB pages a clone reserves; 0 when the snapshot is not on huge pages.
    clone_pages: u64,
    fixed: Option<[u32; 2]>,
    tx: mpsc::UnboundedSender<Event>,
    records: Records,
    samples: Records,
    /// What the module keeps of its records; none keeps all of them.
    bounds: Option<Bounds>,
    /// The longest a render takes, after which a clone's log is written no more.
    render_timeout: Duration,
    harness: CpuCgroup,
    /// Its parts, by name, for their own counters: each side's replay and the drivers.
    harness_parts: Vec<(&'static str, CpuCgroup)>,
    /// The harness's CPU pressure counter over the last ticks: when (ms), usec.
    harness_pressure: std::collections::VecDeque<(u64, u64)>,
    /// Each side's replay's time to answer one request, as a client sees it; none when it
    /// did not answer in time. Taken by a task of its own, so a slow replay delays no
    /// decision.
    replay_ms: [Option<f64>; 2],
    out: PathBuf,
    /// The overlay the replays serve, and each side's file they read it from.
    served: Served,
    inject_files: Vec<PathBuf>,
    /// Where each report written to `report.json` also goes (the agent's connection).
    reports: Option<mpsc::UnboundedSender<RendersReport>>,
}

impl Module {
    fn next_page(&mut self, side: usize) -> Option<usize> {
        let n = self.pages.len();
        for _ in 0..n {
            let page = self.sides[side].cursor % n;
            self.sides[side].cursor += 1;
            if self.wall.renders_page(side, page) {
                return Some(page);
            }
        }
        None
    }

    /// The worker to give the side's next render, if one can take it now.
    fn pick(&mut self, side: usize) -> Option<usize> {
        if side == FCVM {
            if self.clone_pages > 0 {
                let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
                let (total, room) = hugepages(&meminfo);
                let busy = self.sides[side]
                    .workers
                    .iter()
                    .filter(|w| w.job.is_some())
                    .count() as u64;
                if !pool_has_room(total, room, self.clone_pages, busy) {
                    self.sides[side].pool_waits += 1;
                    return None;
                }
            }
            return self.sides[side]
                .workers
                .iter()
                .position(|w| w.ready && w.job.is_none());
        }
        for index in 0..self.sides[side].workers.len() {
            let w = &self.sides[side].workers[index];
            if !w.ready || w.job.is_some() {
                continue;
            }
            let e = &self.engines[index];
            if !e.up || e.recycling {
                continue;
            }
            let anon = cpuset::stat_field(&e.join.dir().join("memory.stat"), "anon").unwrap_or(0);
            if e.recycle_at > 0 && anon >= e.recycle_at {
                self.recycle(index);
                continue;
            }
            return Some(index);
        }
        None
    }

    fn recycle(&mut self, index: usize) {
        let Some(spec) = self.engine_spec.clone() else {
            return;
        };
        let e = &mut self.engines[index];
        e.recycling = true;
        e.up = false;
        let (join, old, tx) = (e.join.clone(), e.child.take(), self.tx.clone());
        // Kept, so the module can end every start that is under way before it lets its
        // reaper go: a start that ran on would put a process into a cgroup being removed.
        self.starting.spawn(async move {
            let result = spec.recycle(index, join, old).await;
            let _ = tx.send(Event::Engine { index, result });
        });
    }

    /// Start renders on `side` until it has its level in flight or no worker can take one.
    async fn dispatch(&mut self, side: usize) {
        while self.sides[side].in_flight < self.sides[side].level {
            if self.sides[side]
                .wait_until
                .is_some_and(|t| Instant::now() < t)
            {
                return;
            }
            let Some(index) = self.pick(side) else {
                return;
            };
            let Some(page) = self.next_page(side) else {
                return;
            };
            let inject = self.served.inject[side].clone();
            let s = &mut self.sides[side];
            s.seq += 1;
            let job = Job {
                seq: s.seq,
                page,
                level: s.level,
                answered: false,
                inject,
            };
            let mut line = json!({
                "seq": job.seq, "page": page, "url": self.pages[page].url, "level": job.level,
            });
            if side == KITE {
                if let Some(spec) = &self.engine_spec {
                    line["address"] = json!(spec.addr(index).to_string());
                }
            }
            let w = &mut s.workers[index];
            let sent = match w.stdin.as_mut() {
                Some(stdin) => stdin
                    .write_all(format!("{line}\n").as_bytes())
                    .await
                    .is_ok(),
                None => false,
            };
            if !sent {
                // Its exit is on the way as an event; nothing was started.
                w.ready = false;
                continue;
            }
            w.job = Some(job);
            s.in_flight += 1;
        }
    }

    /// Tell the wall a render's answer, once.
    fn answer(&mut self, side: usize, index: usize, ok: bool, ms: f64, error: Option<String>) {
        let s = &mut self.sides[side];
        let Some(job) = s.workers[index].job.as_mut().filter(|j| !j.answered) else {
            return;
        };
        job.answered = true;
        s.in_flight = s.in_flight.saturating_sub(1);
        if ok {
            s.failed_in_a_row = 0;
        } else {
            s.failed_in_a_row += 1;
            if s.failed_in_a_row >= FAILURES_BEFORE_A_WAIT {
                s.wait_until = Some(Instant::now() + WAIT_AFTER_FAILURES);
            }
        }
        self.wall.render(&Completed {
            side,
            page: job.page,
            at: now_ms(),
            ms,
            level: job.level,
            error: (!ok).then(|| error.unwrap_or_else(|| "failed".to_string())),
        });
    }

    fn record(&mut self, side: usize, job: &Job, row: &Value) {
        let line = json!({
            "side": SIDES[side], "seq": job.seq, "page": self.pages[job.page].id,
            "level": job.level, "at": now_ms(), "ok": row["ok"], "ms": row["ms"],
            "cpu_ms": row["cpu_ms"], "sha256": row["sha256"], "bytes": row["bytes"],
            "stage": row["stage"], "error": row["error"], "inject_sha256": job.inject,
            "detail": row["detail"],
        });
        self.records.line(&line.to_string());
    }

    async fn line(&mut self, side: usize, index: usize, line: &str) {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            tracing::warn!(target: "renders", side = SIDES[side], index, "a worker wrote {line:?}");
            return;
        };
        if row["ready"] == json!(true) {
            self.sides[side].workers[index].ready = true;
        } else {
            let seq = row["seq"].as_u64();
            if self.sides[side].workers[index].job.as_ref().map(|j| j.seq) != seq {
                tracing::warn!(target: "renders", side = SIDES[side], index, "a row for no job: {line}");
                return;
            }
            let ok = row["ok"] == json!(true);
            if side == KITE && row["answered"] != json!(true) {
                if let Some(e) = self.engines.get_mut(index) {
                    e.failed_in_a_row = if ok { 0 } else { e.failed_in_a_row + 1 };
                }
            }
            let ms = row["ms"].as_f64().unwrap_or(0.0);
            let error = row["error"].as_str().map(str::to_string);
            self.answer(side, index, ok, ms, error);
            if row["answered"] != json!(true) {
                if let Some(job) = self.sides[side].workers[index].job.take() {
                    self.record(side, &job, &row);
                }
            }
        }
        self.dispatch(side).await;
    }

    async fn handle(&mut self, event: Event) {
        match event {
            Event::Line { side, index, line } => self.line(side, index, &line).await,
            Event::Gone { side, index } => self.gone(side, index).await,
            Event::Engine { index, result } => self.engine(index, result).await,
            Event::Probe { side, ms } => self.replay_ms[side] = ms,
        }
    }

    /// A worker exited. Its render, if it carried one, failed; a new worker takes its place.
    async fn gone(&mut self, side: usize, index: usize) {
        self.answer(
            side,
            index,
            false,
            0.0,
            Some("the worker exited".to_string()),
        );
        if let Some(job) = self.sides[side].workers[index].job.take() {
            let row = json!({"ok": false, "error": "the worker exited"});
            self.record(side, &job, &row);
        }
        let s = &mut self.sides[side];
        let status = s.workers[index].child.try_wait().ok().flatten();
        tracing::warn!(target: "renders", side = SIDES[side], index, ?status, "a worker exited");
        s.respawns += 1;
        match spawn_worker(&s.spec, side, index, &s.drivers, &self.tx) {
            Ok(w) => s.workers[index] = w,
            Err(e) => {
                tracing::error!(target: "renders", side = SIDES[side], index, "no new worker: {e:#}");
                s.workers[index].ready = false;
                s.workers[index].stdin = None;
            }
        }
    }

    async fn engine(&mut self, index: usize, result: Result<(Child, f64)>) {
        // Take the starts that are over out of the set (the one that sent this may take a
        // moment more to finish and go with the next). One that panicked sent no result,
        // and its engine stays down: say so.
        while let Some(done) = self.starting.try_join_next() {
            if let Err(error) = done {
                tracing::error!(target: "renders", "an engine start ended without a result: {error}");
            }
        }
        let e = &mut self.engines[index];
        e.recycling = false;
        let restarting = std::mem::take(&mut e.restarting);
        match result {
            Ok((child, ms)) => {
                e.child = Some(child);
                e.up = true;
                e.failed_in_a_row = 0;
                e.retry_at = None;
                e.recycle_at = self.recycle_bytes;
                if restarting {
                    self.restarts += 1;
                } else {
                    self.recycles += 1;
                    self.recycle_ms.push(ms);
                }
            }
            Err(error) => {
                tracing::error!(target: "renders", index, "engine not started again: {error:#}");
                e.retry_at = Some(Instant::now() + ENGINE_RETRY);
            }
        }
        self.dispatch(KITE).await;
    }

    /// Once a second: read each side's counters, let the wall decide the levels, and write
    /// a sample.
    async fn tick(&mut self) {
        let now = now_ms();
        let mut pressure = [None; 2];
        let mut sample = json!({"at": now});
        for side in 0..2 {
            let s = &self.sides[side];
            let usage = s.group.usage_usec();
            pressure[side] = s.group.pressure_usec();
            if let Some(usage) = usage {
                self.wall.cpu(side, now, usage);
            }
            sample[SIDES[side]] = json!({
                "level": s.level, "in_flight": s.in_flight, "usage_usec": usage,
                "pressure_usec": pressure[side],
                "memory": std::fs::read_to_string(s.group.dir().join("memory.current"))
                    .ok()
                    .and_then(|v| v.trim().parse::<u64>().ok()),
                "busy_workers": s.workers.iter().filter(|w| w.job.is_some()).count(),
                "pool_waits": s.pool_waits, "respawns": s.respawns,
            });
        }
        // An engine that died, or whose renders keep failing, is started again.
        for index in 0..self.engines.len() {
            let e = &mut self.engines[index];
            let exited = e
                .child
                .as_mut()
                .is_some_and(|c| matches!(c.try_wait(), Ok(Some(_))));
            let retry_due = e.retry_at.is_some_and(|t| t <= Instant::now());
            if engine_needs_a_start(e.up, e.recycling, exited, e.failed_in_a_row, retry_due) {
                tracing::warn!(target: "renders", index, exited, failed = e.failed_in_a_row, "starting an engine again");
                e.restarting = true;
                e.retry_at = None;
                self.recycle(index);
            }
        }
        let up = self.engines.iter().filter(|e| e.up).count() as u32;
        if !self.engines.is_empty() {
            self.wall.engines(up, self.recycles);
        }
        let levels = match self.fixed {
            Some(fixed) => fixed,
            None => self.wall.step(now, pressure),
        };
        for (side, level) in levels.iter().enumerate() {
            self.sides[side].level = (*level).min(self.sides[side].workers.len() as u32);
        }
        let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let (pool, room) = hugepages(&meminfo);
        if let Some(p) = self.harness.pressure_usec() {
            self.harness_pressure.push_back((now, p));
            while self.harness_pressure.len() > 31 {
                self.harness_pressure.pop_front();
            }
        }
        sample["engines_up"] = json!(up);
        sample["recycles"] = json!(self.recycles);
        sample["restarts"] = json!(self.restarts);
        sample["hugepages"] = json!({"total": pool, "room": room});
        let parts: BTreeMap<&str, Value> = self
            .harness_parts
            .iter()
            .map(|(name, group)| {
                let counters = json!({
                    "pressure_usec": group.pressure_usec(), "usage_usec": group.usage_usec(),
                });
                (*name, counters)
            })
            .collect();
        sample["harness"] = json!({
            "pressure_usec": self.harness.pressure_usec(),
            "usage_usec": self.harness.usage_usec(),
            "replay_ms": {SIDES[0]: self.replay_ms[0], SIDES[1]: self.replay_ms[1]},
            "parts": parts,
        });
        self.samples.line(&sample.to_string());
        self.samples.flush();
        self.records.flush();
        self.dispatch(FCVM).await;
        self.dispatch(KITE).await;
    }

    fn harness_health(&self) -> Harness {
        let cpu_pressure = match (self.harness_pressure.front(), self.harness_pressure.back()) {
            (Some((t0, p0)), Some((t1, p1))) if t1 > t0 => {
                (p1 - p0) as f64 / 1000.0 / (t1 - t0) as f64
            }
            _ => 0.0,
        };
        Harness {
            cpu_pressure,
            replay_ms: (0..2)
                .filter_map(|i| Some((SIDES[i].to_string(), self.replay_ms[i]?)))
                .collect(),
        }
    }

    /// Under [`Bounds`], the failed clones' logs past the newest kept. A render's log is
    /// written for as long as the render and its teardown take; one untouched for twice the
    /// render's limit and a minute is settled.
    fn prune_clone_logs(&self, clones: &Path) {
        let Some(bounds) = self.bounds else {
            return;
        };
        let settled = self.render_timeout * 2 + Duration::from_secs(60);
        let removed = prune_clone_logs(
            clones,
            bounds.failed_clone_logs,
            settled,
            std::time::SystemTime::now(),
        );
        if removed > 0 {
            tracing::debug!(target: "renders", removed, "removed failed clones' logs past the newest kept");
        }
    }

    /// The coordinator's order, applied ([`apply_order`]).
    fn apply(&mut self, order: Option<&RendersOrder>, now: u64) -> Result<Applied> {
        apply_order(
            &mut self.wall,
            &mut self.served,
            &self.inject_files,
            order,
            now,
        )
    }

    /// The wall's report with the harness's health: what the agent sends the coordinator.
    fn wall_report(&self) -> RendersReport {
        let mut report = self.wall.report(now_ms(), None, None);
        report.harness = Some(self.harness_health());
        report
    }

    fn report(&self) -> Value {
        let report = self.wall_report();
        let mut doc = serde_json::to_value(&report).unwrap_or(Value::Null);
        // Held levels (`--levels`) run no search: the report says what is held.
        if let Some(fixed) = self.fixed {
            for (side, level) in fixed.iter().enumerate() {
                doc["sides"][SIDES[side]]["search"] =
                    json!({"in_flight": level, "phase": "holding"});
            }
        }
        let mut sorted = self.recycle_ms.clone();
        sorted.sort_by(|a, b| a.total_cmp(b));
        doc["module"] = json!({
            "kernel": self.kernel,
            "fixed_levels": self.fixed,
            "recycles": self.recycles,
            "engine_restarts": self.restarts,
            "recycle_ms_median": sorted.get(sorted.len() / 2),
            "pool_waits": self.sides[FCVM].pool_waits,
            "respawns": {SIDES[0]: self.sides[0].respawns, SIDES[1]: self.sides[1].respawns},
            "levels": {SIDES[0]: self.sides[0].level, SIDES[1]: self.sides[1].level},
        });
        doc
    }

    fn write_report(&self, name: &str) {
        if name == "report.json" {
            if let Some(reports) = &self.reports {
                let _ = reports.send(self.wall_report());
            }
        }
        let doc = self.report();
        if let Err(e) = write_by_rename(&self.out.join(name), format!("{doc:#}\n").as_bytes()) {
            tracing::warn!(target: "renders", "writing {name}: {e}");
        }
    }
}

/// `--setup side.name=value` as each side's map.
fn setup_values(given: &[String]) -> Result<[BTreeMap<String, String>; 2]> {
    let mut out = [BTreeMap::new(), BTreeMap::new()];
    for entry in given {
        let (key, value) = entry
            .split_once('=')
            .with_context(|| format!("--setup {entry:?} is not side.name=value"))?;
        let (side, name) = key
            .split_once('.')
            .with_context(|| format!("--setup {entry:?} is not side.name=value"))?;
        let i = SIDES
            .iter()
            .position(|s| *s == side)
            .with_context(|| format!("--setup {entry:?}: no side {side:?}"))?;
        out[i].insert(name.to_string(), value.to_string());
    }
    Ok(out)
}

/// Why levels held by `--levels` cannot be run, if they cannot: a side's level is above
/// the most it may run (what was asked for, lowered to what the 2 MiB pool carries for the
/// fcvm side). Held levels are reported as run, so one the side would be kept below is
/// refused.
fn fixed_levels_refusal(fixed: [u32; 2], most: [u32; 2]) -> Option<String> {
    (0..2).find(|&side| fixed[side] > most[side]).map(|side| {
        format!(
            "--levels asks the {} side for {} renders in flight; it may run {} at most",
            SIDES[side], fixed[side], most[side]
        )
    })
}

/// The harness's CPUs as the two replays' and the drivers': `each` for each replay and the
/// rest for the drivers. With `each` 0 nothing is split: all three run on the whole set,
/// in cgroups of their own for the counters.
fn split_harness(cpus: &[u32], each: usize) -> Result<[Vec<u32>; 3]> {
    if each == 0 {
        return Ok([cpus.to_vec(), cpus.to_vec(), cpus.to_vec()]);
    }
    anyhow::ensure!(
        cpus.len() > 2 * each,
        "{} harness CPUs leave none for the drivers beside two replays of {each}",
        cpus.len()
    );
    Ok([
        cpus[..each].to_vec(),
        cpus[each..2 * each].to_vec(),
        cpus[2 * each..].to_vec(),
    ])
}

/// The three CPU sets of `--cpus-fcvm`, `--cpus-engines` and `--cpus-harness`. The sides'
/// are equal in size.
fn cpu_sets(args: &FleetRendersBenchArgs) -> Result<[Vec<u32>; 3]> {
    let cpus = [&args.cpus_fcvm, &args.cpus_engines, &args.cpus_harness]
        .map(|list| cpuset::parse_cpu_list(list).filter(|c| !c.is_empty()));
    let [Some(cpus_fcvm), Some(cpus_engines), Some(cpus_harness)] = cpus else {
        bail!("a CPU list is empty or not a list");
    };
    anyhow::ensure!(
        cpus_fcvm.len() == cpus_engines.len(),
        "the sides have {} and {} CPUs: they must be equal",
        cpus_fcvm.len(),
        cpus_engines.len()
    );
    Ok([cpus_fcvm, cpus_engines, cpus_harness])
}

/// The module's options under the fleet agent: renders-bench's, each given to the agent with
/// `--renders-arg`. The benchmark's own (held levels, a run's length, an overlay) are refused:
/// under the agent the coordinator's orders say when to render and what to inject, and the
/// search sets the levels.
pub fn agent_args(argv: &[String]) -> Result<FleetRendersBenchArgs> {
    #[derive(clap::Parser)]
    #[command(name = "--renders-arg", no_binary_name = true)]
    struct Argv {
        #[command(flatten)]
        args: FleetRendersBenchArgs,
    }
    use clap::Parser as _;
    let args = Argv::try_parse_from(argv)
        .map_err(|e| anyhow::anyhow!("--renders-arg: {e}"))?
        .args;
    for (given, name) in [
        (args.levels.is_some(), "--levels"),
        (args.seconds != 0, "--seconds"),
        (args.overlay_text.is_some(), "--overlay-text"),
        (args.overlay_transform.is_some(), "--overlay-transform"),
    ] {
        anyhow::ensure!(
            !given,
            "--renders-arg {name} is the benchmark's: under the agent the coordinator's orders say when to render and what overlay to serve"
        );
    }
    cpu_sets(&args)?;
    Ok(args)
}

/// The sha256 of the engine bundle's `index.js`: the bundle by its content, not by where
/// this host keeps it.
fn bundle_sha256(dist: &Path) -> Result<String> {
    let index = std::fs::read(dist.join("index.js"))
        .with_context(|| format!("reading {}/index.js", dist.display()))?;
    Ok(sha256_hex(&index))
}

/// The engine bundle `args` names: `--kitesurf-dist`, or `<kitesurf>/dist`.
fn bundle_dir(args: &FleetRendersBenchArgs) -> Option<PathBuf> {
    args.kitesurf_dist
        .clone()
        .or_else(|| args.kitesurf.as_ref().map(|k| k.join("dist")))
}

/// What the host knows of its module before it runs, for the hello's `renders`.
pub struct HostFacts<'a> {
    /// The delegated unit's CPUs; none when this process is in no such unit.
    pub unit_cpus: Option<&'a [u32]>,
    pub kernel: &'a str,
    /// The EC2 instance type, on EC2.
    pub instance_type: Option<&'a str>,
    /// The snapshot's `config.json`, when it is made.
    pub snapshot: Option<&'a Value>,
    /// The engine bundle's digest ([`bundle_sha256`]), when the host has the bundle.
    pub bundle_sha256: Option<String>,
}

/// The hello's `renders`: the pages in the wall's order, the CPUs of each side, whether each
/// side runs in a cpuset of exactly those CPUs (the module confines both sides or does not run,
/// so: whether the unit holds every CPU it is given), and what runs for the `Setup` lists.
/// `vcpus` and `page_size` are the snapshot's when it is made, else what it will be made with;
/// the commits are `--setup fcvm.commit=` and `--setup kitesurf.commit=`.
pub fn hello(args: &FleetRendersBenchArgs, facts: &HostFacts) -> Result<RendersHost> {
    let cpus = cpu_sets(args)?;
    let setup = setup_values(&args.setup)?;
    let confined = facts
        .unit_cpus
        .is_some_and(|unit| cpus.iter().flatten().all(|c| unit.contains(c)));
    let meta = facts.snapshot.map(|c| &c["metadata"]);
    let vcpus = meta
        .and_then(|m| m["vcpu"].as_u64())
        .map_or(args.vcpus.to_string(), |v| v.to_string());
    let huge = meta.map_or(args.hugepages, |m| m["hugepages"] == json!(true));
    let mut build = BTreeMap::new();
    let mut put = |k: &str, v: Option<String>| {
        if let Some(v) = v.filter(|v| !v.is_empty()) {
            build.insert(k.to_string(), v);
        }
    };
    put("instance_type", facts.instance_type.map(str::to_string));
    put("kernel", Some(facts.kernel.to_string()));
    put("fcvm_commit", setup[FCVM].get("commit").cloned());
    if args.max_engines > 0 {
        put("kitesurf_commit", setup[KITE].get("commit").cloned());
        put("bundle_sha256", facts.bundle_sha256.clone());
        put("recycle_mib", Some(args.recycle_mib.to_string()));
    }
    put("viewport", Some(args.viewport.clone()));
    put("vcpus", Some(vcpus));
    put(
        "page_size",
        Some(if huge { "2 MiB" } else { "4 KiB" }.to_string()),
    );
    Ok(RendersHost {
        pages: args
            .urls
            .iter()
            .map(|url| RendersPage {
                id: page_id(url),
                url: url.clone(),
            })
            .collect(),
        cpus: cpus[0].len() as u32,
        confined,
        build,
    })
}

/// What the hello says of a module run with `args` on this host. Reads the snapshot's
/// `config.json` and the engine bundle when they are there.
pub fn this_host(args: &FleetRendersBenchArgs, instance_type: Option<&str>) -> Result<RendersHost> {
    let unit = Unit::for_this_process();
    if let Err(reason) = &unit {
        tracing::warn!(target: "renders", %reason, "pages mode: no cpuset for the sides; the module will not run");
    }
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let snapshot = std::fs::read_to_string(
        crate::paths::snapshot_dir()
            .join(&args.snapshot_tag)
            .join("config.json"),
    )
    .ok()
    .and_then(|t| serde_json::from_str::<Value>(&t).ok());
    let bundle = bundle_dir(args)
        .filter(|_| args.max_engines > 0)
        .and_then(|d| std::fs::canonicalize(d).ok())
        .and_then(|d| bundle_sha256(&d).ok());
    hello(
        args,
        &HostFacts {
            unit_cpus: unit.as_ref().ok().map(|u| u.cpus()),
            kernel: kernel.trim(),
            instance_type,
            snapshot: snapshot.as_ref(),
            bundle_sha256: bundle,
        },
    )
}

/// The overlay the replays serve now, and each side's injected bytes by their sha256.
#[derive(Debug, Default)]
struct Served {
    overlay: Option<OrderOverlay>,
    inject: [Option<String>; 2],
}

/// What an order does to a module that runs.
#[derive(Debug, PartialEq)]
enum Applied {
    Run,
    Stop,
}

/// Apply the coordinator's order. An overlay that is not the one the replays serve (by its
/// content and its number) is written to both sides' files by rename, so a render sees the old
/// bytes or the new ones whole, and the wall is told what the replays serve from then on. The
/// wall then takes the order: a new overlay starts both searches again. No order, or one that
/// says `run: false`, stops the module.
fn apply_order(
    wall: &mut Wall,
    served: &mut Served,
    files: &[PathBuf],
    order: Option<&RendersOrder>,
    now: u64,
) -> Result<Applied> {
    let Some(order) = order.filter(|o| o.run) else {
        wall.order(order.unwrap_or(&RendersOrder::default()), now);
        return Ok(Applied::Stop);
    };
    if served.overlay != order.overlay {
        for (side, file) in files.iter().enumerate().take(2) {
            let bytes = overlay::html(order.overlay.as_ref(), side);
            write_by_rename(file, bytes.as_bytes())
                .with_context(|| format!("writing {}", file.display()))?;
            served.inject[side] = (!bytes.is_empty()).then(|| sha256_hex(bytes.as_bytes()));
        }
        served.overlay = order.overlay.clone();
        wall.serving(served.overlay.as_ref().map(|o| o.seq));
    }
    wall.order(order, now);
    Ok(Applied::Run)
}

/// Resolves once the order no longer says run (a stop, or none), or its sender is gone (the
/// agent's connection ended). Every order it reads is marked seen.
async fn stop_ordered(orders: &mut watch::Receiver<Option<RendersOrder>>) -> &'static str {
    loop {
        if !orders.borrow_and_update().as_ref().is_some_and(|o| o.run) {
            return "an order to stop";
        }
        if orders.changed().await.is_err() {
            return "the end of the agent's connection";
        }
    }
}

/// `fcvm fleet renders-bench`: one session, ordered to run with the overlay its arguments
/// give, until a signal or `--seconds`.
pub async fn bench(args: FleetRendersBenchArgs) -> Result<()> {
    let overlay =
        (args.overlay_text.is_some() || args.overlay_transform.is_some()).then(|| OrderOverlay {
            text: args.overlay_text.clone(),
            chromium_only: args
                .overlay_transform
                .as_deref()
                .and_then(fleet_core::renders::chromium_only)
                .unwrap_or(false),
            transform: args.overlay_transform.clone(),
            seq: 1,
        });
    // Held until the session returns: the order never changes.
    let (_order, orders) = watch::channel(Some(RendersOrder { run: true, overlay }));
    let exe = std::env::current_exe().context("this binary's path")?;
    let (outcome, reaped) = session(&args, &exe, orders, None, Signals::EndTheModule, None).await?;
    println!("{}", serde_json::to_string(&reaped)?);
    outcome?;
    anyhow::ensure!(
        reaped.clean(),
        "the module left something behind: {reaped:?}"
    );
    Ok(())
}

/// Whether a module handles SIGTERM and SIGINT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signals {
    /// The benchmark: either signal ends the module, which then lets its reaper go.
    EndTheModule,
    /// The agent: no handler is registered, so either signal ends the process, and the
    /// reaper removes what the module made.
    LeaveToTheProcess,
}

/// One module from its start to its end, for the benchmark command and the fleet agent alike:
/// the kernel rule, the run directory's lock, the start's sweep, the reaper, the processes
/// ([`run`]) and the reaper's sweep. `orders` says whether to render and what overlay to serve;
/// the module ends when it says stop or its sender is gone. Each report the module writes is
/// also sent to `reports`. Only [`Signals::EndTheModule`] makes the module end at SIGTERM or SIGINT: a handler
/// stays registered for the life of the process, and the agent's SIGTERM must end the agent.
///
/// Returns how the module ended and what the reaper found. An error from before the reaper
/// started (nothing was made), or from reading the reaper's result, is returned as is.
pub async fn session(
    args: &FleetRendersBenchArgs,
    exe: &Path,
    mut orders: watch::Receiver<Option<RendersOrder>>,
    reports: Option<mpsc::UnboundedSender<RendersReport>>,
    signals: Signals,
    bounds: Option<Bounds>,
) -> Result<(Result<()>, reap::Reaped)> {
    let status = Status {
        path: args.status_file.clone(),
        hold_at: args.hold_at.clone(),
    };
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap_or_default()
        .trim()
        .to_string();
    anyhow::ensure!(
        kernel_has_bridge_fix(&release),
        "kernel {release} lacks the bridge teardown fix: no figure is recorded on it"
    );
    let [cpus_fcvm, cpus_engines, cpus_harness] = cpu_sets(args)?;
    let fixed = match args.levels.as_deref() {
        Some([f, k]) => Some([*f, *k]),
        Some(_) => bail!("--levels takes two numbers"),
        None => None,
    };
    let pages: Vec<RendersPage> = args
        .urls
        .iter()
        .map(|url| RendersPage {
            id: page_id(url),
            url: url.clone(),
        })
        .collect();
    let setup = setup_values(&args.setup)?;
    let unit =
        Unit::for_this_process().map_err(|e| anyhow::anyhow!("no cpuset for the sides: {e}"))?;
    let target = reap::Target {
        cgroups: CGROUPS.iter().map(|c| unit.dir().join(c)).collect(),
        run_dir: args.run_dir.clone(),
        snapshot_tag: args.snapshot_tag.clone(),
        deadline: Duration::from_secs(20),
    };
    // One module per run directory, and never a directory that is someone else's.
    let lock = reap::RunLock::take(&args.run_dir)?;
    lock.usable(&args.run_dir)?;
    // What a module that lost its host left, before anything is made.
    let left = reap::sweep(&target).await;
    anyhow::ensure!(
        left.clean(),
        "an earlier run's processes could not be removed: {left:?}"
    );
    // The path is this run's from here; the sweep that removes its directory ends that.
    lock.claim(&args.run_dir)?;
    std::fs::create_dir_all(&args.out)?;
    let logs = args.out.join("logs");
    std::fs::create_dir_all(logs.join("clones"))?;
    let reaper = reap::Reaper::start(exe, &target, Some(&args.out.join("teardown.json")), &lock)?;
    // Nothing is made before this line but the records' directory, and from it on the
    // reaper removes what is.
    status.set("start").await;
    let outcome = run(
        args,
        &status,
        &unit,
        [cpus_fcvm, cpus_engines, cpus_harness],
        pages,
        fixed,
        setup,
        exe,
        &logs,
        &release,
        &mut orders,
        reports,
        signals,
        bounds,
    )
    .await;
    status.set("stopping").await;
    // Every child handle is dropped by now and no engine start is under way; the reaper
    // ends what they started.
    let reaped = reaper.finish().await?;
    status
        .set(if reaped.clean() {
            "stopped"
        } else {
            "stopped-unclean"
        })
        .await;
    Ok((outcome, reaped))
}

/// The processes, from the cgroups to the last worker's start, and then the renders until the
/// order says stop, the agent's connection ends, a signal arrives (`signals`), `--seconds`
/// pass, or something fails. An order to stop during the start ends the start where it is:
/// its future is dropped, which kills every child it started; no task of it can start a
/// process afterwards ([`start`]).
#[allow(clippy::too_many_arguments)]
async fn run(
    args: &FleetRendersBenchArgs,
    status: &Status,
    unit: &Unit,
    cpus: [Vec<u32>; 3],
    pages: Vec<RendersPage>,
    fixed: Option<[u32; 2]>,
    setup: [BTreeMap<String, String>; 2],
    exe: &Path,
    logs: &Path,
    release: &str,
    orders: &mut watch::Receiver<Option<RendersOrder>>,
    reports: Option<mpsc::UnboundedSender<RendersReport>>,
    signals: Signals,
    bounds: Option<Bounds>,
) -> Result<()> {
    let start = start(
        args, status, unit, cpus, pages, fixed, setup, exe, logs, release, reports, bounds,
    );
    tokio::pin!(start);
    let Started {
        mut module,
        mut rx,
        early,
        mut serve,
        mut replays,
        addrs,
    } = tokio::select! {
        started = &mut start => started?,
        why = stop_ordered(orders) => {
            tracing::info!(target: "renders", "ending at {why}, before the module was started");
            return Ok(());
        }
    };
    // One request a second to each side's replay, timed from this side of the socket.
    for (side, addr) in addrs.iter().enumerate() {
        let addr = SocketAddr::from((*addr, args.replay_http_port));
        let tx = module.tx.clone();
        tokio::spawn(async move {
            loop {
                let ms = http_get(addr, "example.com", "/", Duration::from_millis(800))
                    .await
                    .map(|(_, ms)| ms);
                if tx.send(Event::Probe { side, ms }).is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }

    // Under the agent no handler is registered: one stays for the life of the process, and
    // the agent's SIGTERM must end the agent.
    let mut signals = if signals == Signals::EndTheModule {
        use tokio::signal::unix::{signal, SignalKind};
        Some((
            signal(SignalKind::terminate())?,
            signal(SignalKind::interrupt())?,
        ))
    } else {
        None
    };
    let mut every = tokio::time::interval(Duration::from_secs(1));
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let started = Instant::now();
    let mut ticks = 0u64;
    // The order as it stands now; a change from here on wakes the loop.
    let first = module.apply(orders.borrow_and_update().as_ref(), now_ms());
    for event in early {
        module.handle(event).await;
    }
    status.set("running").await;
    let ended: Result<&str> = async {
        if first? == Applied::Stop {
            return Ok("an order to stop");
        }
        Ok(loop {
        tokio::select! {
            _ = signal_received(&mut signals) => break "a signal",
            changed = orders.changed() => {
                if changed.is_err() {
                    break "the end of the agent's connection";
                }
                let order = orders.borrow_and_update().clone();
                if module.apply(order.as_ref(), now_ms())? == Applied::Stop {
                    break "an order to stop";
                }
            }
            event = rx.recv() => match event {
                Some(event) => module.handle(event).await,
                None => break "no worker left",
            },
            _ = every.tick() => {
                module.tick().await;
                ticks += 1;
                if ticks.is_multiple_of(5) {
                    module.write_report("report.json");
                    module.prune_clone_logs(&logs.join("clones"));
                }
                if args.seconds > 0 && started.elapsed() >= Duration::from_secs(args.seconds) {
                    break "its time";
                }
                if let Some(Ok(Some(exit))) = serve.as_mut().map(|s| s.try_wait()) {
                    bail!("the memory server exited ({exit}): {}", tail(&logs.join("serve.log")));
                }
                for (i, replay) in replays.iter_mut().enumerate() {
                    if let Ok(Some(exit)) = replay.try_wait() {
                        bail!("replay process {i} exited ({exit})");
                    }
                }
            }
        }
        })
    }
    .await;
    // Whatever ended the loop: no engine start goes on past this line.
    module.starting.shutdown().await;
    let ended = ended?;
    tracing::info!(target: "renders", "ending at {ended}");
    module.records.flush();
    module.samples.flush();
    module.write_report("report.json");
    module.write_report("summary.json");
    Ok(())
}

/// Resolves at SIGTERM or SIGINT when the handlers are registered, never when they are not.
async fn signal_received(
    signals: &mut Option<(tokio::signal::unix::Signal, tokio::signal::unix::Signal)>,
) {
    match signals {
        Some((term, int)) => {
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
        }
        None => std::future::pending().await,
    }
}

/// A module whose processes are started, for [`run`]'s loop.
struct Started {
    module: Module,
    rx: mpsc::UnboundedReceiver<Event>,
    /// Events that arrived while the workers were starting.
    early: Vec<Event>,
    serve: Option<Child>,
    replays: Vec<Child>,
    addrs: [Ipv4Addr; 2],
}

/// The start: the cgroups, the replays, the snapshot when it is absent, the memory server,
/// the engines and the workers. Dropping it before it returns ends all of it, with the
/// children killed on drop and the rest left to the reaper: the engines' first starts are
/// polled here rather than spawned, and the only tasks it spawns are the workers' output
/// readers, which end with the workers' output and start no process.
#[allow(clippy::too_many_arguments)]
async fn start(
    args: &FleetRendersBenchArgs,
    status: &Status,
    unit: &Unit,
    cpus: [Vec<u32>; 3],
    pages: Vec<RendersPage>,
    fixed: Option<[u32; 2]>,
    setup: [BTreeMap<String, String>; 2],
    exe: &Path,
    logs: &Path,
    release: &str,
    reports: Option<mpsc::UnboundedSender<RendersReport>>,
    bounds: Option<Bounds>,
) -> Result<Started> {
    let [cpus_fcvm, cpus_engines, cpus_harness] = cpus;
    let run_dir = &args.run_dir;
    std::fs::create_dir_all(run_dir)?;

    // The cgroups. Memory is counted for each side and each engine.
    unit.enable("memory").map_err(anyhow::Error::msg)?;
    let fcvm_group = CpuCgroup::create(unit, CGROUPS[0], &cpus_fcvm).map_err(anyhow::Error::msg)?;
    let engines_group =
        CpuCgroup::create(unit, CGROUPS[1], &cpus_engines).map_err(anyhow::Error::msg)?;
    engines_group.enable("memory").map_err(anyhow::Error::msg)?;
    let harness = CpuCgroup::create(unit, CGROUPS[2], &cpus_harness).map_err(anyhow::Error::msg)?;
    let [replay_f, replay_e, driver_cpus] =
        split_harness(&cpus_harness, args.replay_cpus as usize)?;
    let replay_groups = [
        harness
            .child("replay-fcvm", &replay_f)
            .map_err(anyhow::Error::msg)?,
        harness
            .child("replay-engines", &replay_e)
            .map_err(anyhow::Error::msg)?,
    ];
    let drivers_group = harness
        .child("drivers", &driver_cpus)
        .map_err(anyhow::Error::msg)?;
    let drivers = drivers_group.join(None).map_err(anyhow::Error::msg)?;
    let fcvm_join = fcvm_group.join(None).map_err(anyhow::Error::msg)?;
    status.set("cgroups").await;

    let want_fcvm = args.max_fcvm > 0;
    // The replays: several processes behind each side's address, one certificate a side.
    let addrs = [args.replay_addr_fcvm, args.replay_addr_engines];
    let mut replays: Vec<Child> = Vec::new();
    let mut inject_files = Vec::new();
    for side in 0..2 {
        let name = SIDES[side];
        let certs = run_dir.join(format!("certs-{name}"));
        std::fs::create_dir_all(&certs)?;
        let inject = run_dir.join(format!("inject-{name}.html"));
        std::fs::write(&inject, "")?;
        let join = replay_groups[side].join(None).map_err(anyhow::Error::msg)?;
        let mut starting = Vec::new();
        for p in 0..args.replay_procs.max(1) {
            let (log_path, log) = log_file(logs, &format!("replay-{name}-{p}.log"))?;
            let dns_port = if side == FCVM && p == 0 {
                args.replay_dns_port
            } else {
                0
            };
            let mut cmd = std::process::Command::new("python3");
            cmd.arg(args.bench.join("corpus_serve.py"))
                .arg("--root")
                .arg(&args.corpus)
                .args(["--port", &args.replay_http_port.to_string()])
                .args(["--tls-port", &args.replay_tls_port.to_string()])
                .args(["--http-addr", &addrs[side].to_string()])
                .args(["--dns-addr", &addrs[side].to_string()])
                .args(["--dns-port", &dns_port.to_string()])
                .args(["--answer-ip", "10.0.2.2", "--http11", "--reuse-port"])
                .arg("--tls-cert-dir")
                .arg(&certs)
                .arg("--inject-file")
                .arg(&inject)
                .arg("--listen-log")
                .arg(logs.join(format!("replay-{name}-{p}.listen")));
            starting.push((log_path, spawn_in(&join, cmd, log)?));
        }
        for (p, (log_path, mut child)) in starting.into_iter().enumerate() {
            wait_for_line(
                &format!("replay {p} of the {name} side"),
                &log_path,
                "replaying on",
                &mut child,
                Duration::from_secs(60),
            )
            .await?;
            replays.push(child);
        }
        inject_files.push(inject);
    }
    status.set("replays").await;

    // The snapshot, made here when it is absent: the benchmark's golden, which is the
    // image's Chromium at its health gate (warm, a DevTools round trip answered), with the
    // fcvm side's replay as the guest's resolver. The replays are up by now, so the guest
    // resolves through one while it starts.
    let snapshot = crate::paths::snapshot_dir().join(&args.snapshot_tag);
    if want_fcvm && !snapshot.join("config.json").exists() {
        status.set("golden").await;
        let (log_path, log) = log_file(logs, "golden.log")?;
        let mut cmd = std::process::Command::new(exe);
        cmd.args(["podman", "prepare", "--tag", &args.snapshot_tag])
            .args(args.hugepages.then_some("--hugepages"))
            .args(["--dns", "10.0.2.2", "--name"])
            .arg(format!("renders-gold-{}", std::process::id()))
            .args(["--cpu", &args.vcpus.to_string()])
            .args(["--mem", &args.mem_mib.to_string()])
            .args(["--network", "rootless", "--publish", "9222:9222"])
            .arg(&args.image)
            .env("RUST_LOG", "fcvm=info")
            // Its VM stays in this cgroup, so it ends with the module like everything else.
            .env(crate::cgroup::PARENT_ENV, "none");
        let mut child = spawn_in(&drivers, cmd, log)?;
        let done = child.wait().await?;
        anyhow::ensure!(
            done.success(),
            "making the snapshot ended {done}: {}",
            tail(&log_path)
        );
        anyhow::ensure!(
            snapshot.join("config.json").exists(),
            "fcvm podman prepare left no snapshot {}",
            args.snapshot_tag
        );
    }
    let mut clone_pages = 0;
    // The most clones in flight: what was asked for, or fewer when the pool carries fewer.
    let mut max_fcvm = args.max_fcvm;
    let mut fcvm_setup = setup[FCVM].clone();
    if want_fcvm {
        let config: Value =
            serde_json::from_str(&std::fs::read_to_string(snapshot.join("config.json"))?)?;
        let meta = &config["metadata"];
        let mib = meta["memory_mib"].as_u64().unwrap_or(0);
        if meta["hugepages"] == json!(true) {
            clone_pages = mib / 2;
        }
        fcvm_setup.insert("vcpus".into(), meta["vcpu"].to_string());
        fcvm_setup.insert("memory_mib".into(), mib.to_string());
        fcvm_setup.insert(
            "page_size".into(),
            if clone_pages > 0 { "2 MiB" } else { "4 KiB" }.into(),
        );
        fcvm_setup.insert("kernel".into(), release.to_string());
        let (pool, _) = hugepages(&std::fs::read_to_string("/proc/meminfo").unwrap_or_default());
        let carries = pool_cap(pool, clone_pages);
        anyhow::ensure!(
            carries >= 1,
            "the 2 MiB pool holds {pool} pages; one render in flight needs {} (the memory server's copy and {} clones of {clone_pages})",
            (u64::from(fcvm_workers(1)) + 1) * clone_pages,
            fcvm_workers(1)
        );
        if carries < max_fcvm {
            tracing::warn!(target: "renders", pool, asked = max_fcvm, carries, "the 2 MiB pool carries fewer renders in flight than asked for");
            max_fcvm = carries;
        }
        fcvm_setup.insert("pool_pages".into(), pool.to_string());
        fcvm_setup.insert("max_in_flight".into(), max_fcvm.to_string());
    }

    // The memory server.
    let mut serve: Option<Child> = None;
    let mut fcvm_spec = None;
    if want_fcvm {
        status.set("serve-starting").await;
        let (log_path, log) = log_file(logs, "serve.log")?;
        let mut cmd = std::process::Command::new(exe);
        cmd.args([
            "snapshot",
            "serve",
            &args.snapshot_tag,
            "--uffd-mode",
            &args.uffd_mode,
            "--uffd-prefetch",
            "on",
        ])
        .env("RUST_LOG", "warn,uffd=info")
        .env(crate::cgroup::PARENT_ENV, "none");
        let mut child = spawn_in(&fcvm_join, cmd, log)?;
        wait_for_line(
            "the memory server",
            &log_path,
            "Waiting for VMs",
            &mut child,
            Duration::from_secs(300),
        )
        .await?;
        let pid = child.id().context("the memory server's pid")?;
        fcvm_spec = Some((
            exe.to_path_buf(),
            pid,
            crate::paths::data_dir(),
            fcvm_join.procs(),
        ));
        fcvm_setup.insert("uffd_mode".into(), args.uffd_mode.clone());
        serve = Some(child);
    }
    status.set("serve").await;

    // The engines: one for every render the side may have in flight.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut engines = Vec::new();
    let mut engine_spec = None;
    let mut kite_setup = setup[KITE].clone();
    if args.max_engines > 0 {
        let kitesurf = args
            .kitesurf
            .as_ref()
            .context("--kitesurf is needed for the engines")?;
        let nodebin = std::fs::read_dir(kitesurf.join("tools"))
            .with_context(|| format!("reading {}/tools", kitesurf.display()))?
            .flatten()
            .map(|e| e.path().join("bin"))
            .find(|p| p.join("node").exists())
            .with_context(|| format!("no node under {}/tools", kitesurf.display()))?;
        let dist = args
            .kitesurf_dist
            .clone()
            .unwrap_or_else(|| kitesurf.join("dist"));
        let dist = std::fs::canonicalize(&dist)
            .with_context(|| format!("the bundle {}", dist.display()))?;
        let ns = run_dir.join("ns");
        std::fs::create_dir_all(&ns)?;
        let mut hosts = String::from("127.0.0.1 localhost\n::1 localhost\n");
        for host in corpus_hosts(&args.corpus)? {
            hosts.push_str(&format!("{} {host}\n", addrs[KITE]));
        }
        std::fs::write(ns.join("hosts"), hosts)?;
        std::fs::write(
            ns.join("nsswitch.conf"),
            "passwd: files\ngroup: files\nhosts: files\n",
        )?;
        std::fs::write(ns.join("resolv.conf"), "# no resolver\n")?;
        let spec = EngineSpec {
            nodebin,
            src: kitesurf.join("src"),
            dist: dist.clone(),
            ns,
            ca: run_dir.join(format!("certs-{}/ca.pem", SIDES[KITE])),
            logs: logs.to_path_buf(),
            port_base: args.engine_port_base,
        };
        status.set("engines-starting").await;
        let mut start_ms = Vec::new();
        for first in (0..args.max_engines as usize).step_by(ENGINE_STARTS_AT_ONCE) {
            // Polled here rather than spawned, so that dropping the start (an order to stop)
            // drops each engine's start with it and none runs on.
            let mut batch = Vec::new();
            for index in first..(first + ENGINE_STARTS_AT_ONCE).min(args.max_engines as usize) {
                let join = engines_group
                    .join(Some(&format!("e{index}")))
                    .map_err(anyhow::Error::msg)?;
                let (spec, j) = (spec.clone(), join.clone());
                batch.push(async move { spec.start(index, &j).await.map(|s| (index, s)) });
                engines.push(Engine {
                    join,
                    child: None,
                    up: false,
                    recycling: false,
                    restarting: false,
                    failed_in_a_row: 0,
                    retry_at: None,
                    recycle_at: first_recycle_at(
                        index,
                        args.max_engines as usize,
                        args.recycle_mib * 1024 * 1024,
                    ),
                });
            }
            for (index, (child, ms)) in futures_util::future::try_join_all(batch).await? {
                engines[index].child = Some(child);
                engines[index].up = true;
                start_ms.push(ms);
            }
        }
        start_ms.sort_by(|a, b| a.total_cmp(b));
        kite_setup.insert("engines".into(), engines.len().to_string());
        kite_setup.insert(
            "engine_start_ms_median".into(),
            format!("{:.0}", start_ms[start_ms.len() / 2]),
        );
        kite_setup.insert("recycle_mib".into(), args.recycle_mib.to_string());
        kite_setup.insert("kernel".into(), release.to_string());
        // The bundle by its content, not by where this host keeps it.
        let index = std::fs::read(dist.join("index.js"))
            .with_context(|| format!("reading {}/index.js", dist.display()))?;
        kite_setup.insert("bundle_sha256".into(), sha256_hex(&index));
        tracing::info!(target: "renders", bundle = %dist.display(), "the engines' bundle");
        engine_spec = Some(spec);
    }
    status.set("engines").await;

    // The workers. The fcvm side has half as many again as its most renders in flight,
    // and two more: a clone's teardown outlasts its answer.
    let run_id = format!("{:x}", now_ms() % 0xff_ffff);
    let worker_spec = |fcvm| WorkerSpec {
        bench: args.bench.clone(),
        logs: logs.to_path_buf(),
        timeout_s: args.timeout_s,
        viewport: args.viewport.clone(),
        fcvm,
        run_id: run_id.clone(),
    };
    let counts = [
        if want_fcvm { fcvm_workers(max_fcvm) } else { 0 },
        engines.len() as u32,
    ];
    let mut sides = Vec::new();
    for (side, group) in [fcvm_group, engines_group].into_iter().enumerate() {
        let spec = worker_spec(if side == FCVM {
            fcvm_spec.clone()
        } else {
            None
        });
        let mut workers = Vec::new();
        for index in 0..counts[side] as usize {
            workers.push(spawn_worker(&spec, side, index, &drivers, &tx)?);
        }
        sides.push(Side {
            group,
            drivers: drivers.clone(),
            spec,
            workers,
            in_flight: 0,
            level: 0,
            cursor: 0,
            seq: 0,
            pool_waits: 0,
            respawns: 0,
            failed_in_a_row: 0,
            wait_until: None,
        });
    }
    // Every worker says it is ready (its harness modules are imported) before a render
    // starts, so the first level's renders are not timed against a worker still starting.
    let mut waiting: usize = sides.iter().map(|s| s.workers.len()).sum();
    let ready_by = Instant::now() + Duration::from_secs(120);
    let mut early = Vec::new();
    while waiting > 0 {
        let event = tokio::time::timeout_at(ready_by.into(), rx.recv())
            .await
            .map_err(|_| anyhow::anyhow!("{waiting} workers were not ready within 120 s"))?;
        match event {
            Some(Event::Line { side, index, line })
                if serde_json::from_str::<Value>(&line)
                    .is_ok_and(|row| row["ready"] == json!(true)) =>
            {
                sides[side].workers[index].ready = true;
                waiting -= 1;
            }
            Some(Event::Gone { side, index }) => bail!(
                "worker {index} of the {} side exited at start: {}",
                SIDES[side],
                tail(&logs.join(format!("worker-{}-{index}.log", SIDES[side])))
            ),
            Some(other) => early.push(other),
            None => bail!("no worker left"),
        }
    }
    status.set("workers").await;

    let now = now_ms();
    let mut wall = Wall::new(
        pages.clone(),
        SearchParams::default(),
        u64::from(args.window_s) * 1000,
        now,
    );
    if let Some(refusal) = fixed.and_then(|f| fixed_levels_refusal(f, [max_fcvm, args.max_engines]))
    {
        bail!(refusal);
    }
    wall.cap(FCVM, max_fcvm.max(1));
    wall.cap(KITE, args.max_engines.max(1));
    let viewport = ("viewport".to_string(), args.viewport.clone());
    fcvm_setup.insert(viewport.0.clone(), viewport.1.clone());
    kite_setup.insert(viewport.0, viewport.1);
    for (side, values) in [fcvm_setup, kite_setup].into_iter().enumerate() {
        wall.setup(side, values);
    }
    let [replay_fcvm, replay_engines] = replay_groups;
    let harness_parts = vec![
        ("replay-fcvm", replay_fcvm),
        ("replay-engines", replay_engines),
        ("drivers", drivers_group),
    ];
    let module = Module {
        pages,
        wall,
        sides,
        engines,
        engine_spec,
        recycle_bytes: args.recycle_mib * 1024 * 1024,
        recycles: 0,
        recycle_ms: Vec::new(),
        restarts: 0,
        starting: tokio::task::JoinSet::new(),
        kernel: release.to_string(),
        clone_pages,
        fixed,
        tx,
        records: Records::create(
            args.out.join("renders.jsonl"),
            bounds.map(|b| b.renders_bytes),
        )?,
        samples: Records::create(
            args.out.join("samples.jsonl"),
            bounds.map(|b| b.samples_bytes),
        )?,
        bounds,
        render_timeout: Duration::from_secs(u64::from(args.timeout_s)),
        harness,
        harness_parts,
        harness_pressure: Default::default(),
        replay_ms: [None; 2],
        out: args.out.clone(),
        served: Served::default(),
        inject_files,
        reports,
    };
    Ok(Started {
        module,
        rx,
        early,
        serve,
        replays,
        addrs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The releases the design's rule names, and the ones around them.
    #[test]
    fn the_kernel_rule_refuses_7_0_0_1013() {
        for (release, good) in [
            ("7.0.0-1013-aws", false),
            ("7.0.0-1016-aws", false),
            ("7.0.0-1017-aws", true),
            ("7.0.0-1018-aws", true),
            ("7.0.0-2000-generic", false),
            ("7.1.9", false),
            ("7.2.0", true),
            ("7.3.3-fcvm-a4363837b43b", true),
            ("6.18.59", false),
            ("6.18.60", true),
            ("6.19.1", false),
            ("6.8.0-1021-aws", false),
            ("8.0.0", true),
            ("", false),
        ] {
            assert_eq!(kernel_has_bridge_fix(release), good, "{release}");
        }
    }

    /// `--levels F,K` is one argument of two numbers, as the launcher passes it.
    #[test]
    fn the_levels_are_one_argument_of_two_numbers() {
        use clap::Parser;
        let parse = |levels: &str| {
            crate::cli::Cli::try_parse_from([
                "fcvm",
                "fleet",
                "renders-bench",
                "--out",
                "o",
                "--run-dir",
                "r",
                "--bench",
                "b",
                "--corpus",
                "c",
                "--urls",
                "https://a.test/,https://b.test/",
                "--snapshot-tag",
                "t",
                "--levels",
                levels,
            ])
        };
        let cli = parse("16,36").unwrap();
        let crate::cli::Commands::Fleet(fleet) = cli.cmd else {
            panic!("not the fleet command");
        };
        let crate::cli::FleetCommand::RendersBench(args) = fleet.cmd else {
            panic!("not renders-bench");
        };
        assert_eq!(args.levels, Some(vec![16, 36]));
        assert_eq!(args.urls.len(), 2);
        assert!(parse("16,x").is_err());
    }

    /// An engine answers `/health` with a length and keeps the connection open: the answer
    /// is whole at its length, not at end of file.
    #[test]
    fn an_answer_is_whole_at_its_length() {
        let kept_open = b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\nContent-Type: text/plain\r\n\r\nkitesurf: OK\n";
        assert_eq!(
            http_body(kept_open, false).as_deref(),
            Some("kitesurf: OK\n")
        );
        assert_eq!(http_body(&kept_open[..60], false), None);
        assert_eq!(
            http_body(
                b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nokextra",
                false
            )
            .as_deref(),
            Some("ok")
        );
        // no length: whole at end of file only
        let closed = b"HTTP/1.0 200 OK\r\nServer: x\r\n\r\nhello";
        assert_eq!(http_body(closed, false), None);
        assert_eq!(http_body(closed, true).as_deref(), Some("hello"));
        let chunked =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        assert!(http_body(chunked, false).unwrap().contains("hello"));
        assert_eq!(http_body(&chunked[..50], false), None);
        assert_eq!(http_body(b"not http\r\n\r\n", true).as_deref(), Some(""));
    }

    /// The same over a socket: a server that answers and keeps the connection open.
    #[tokio::test]
    async fn a_get_does_not_wait_for_a_kept_connection_to_close() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\nkitesurf: OK\n")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let (body, ms) = http_get(addr, "127.0.0.1", "/health", Duration::from_secs(2))
            .await
            .expect("an answer within the limit");
        assert_eq!(body.trim(), "kitesurf: OK");
        assert!(ms < 1500.0, "{ms} ms");
        server.abort();
    }

    /// 36 engines at 4 GiB: first thresholds from just over 2 GiB to 4 GiB, evenly, the last
    /// at the threshold itself.
    #[test]
    fn the_engines_first_recycles_are_spread() {
        let gib = 1u64 << 30;
        let at: Vec<u64> = (0..36).map(|i| first_recycle_at(i, 36, 4 * gib)).collect();
        assert!(at.windows(2).all(|w| w[1] > w[0]));
        assert!(at[0] > 2 * gib && at[0] < 2 * gib + gib / 8, "{}", at[0]);
        assert_eq!(at[35], 4 * gib);
        let steps: Vec<u64> = at.windows(2).map(|w| w[1] - w[0]).collect();
        let (least, most) = (steps.iter().min().unwrap(), steps.iter().max().unwrap());
        assert!(most - least <= 1, "the steps differ: {least} to {most}");
        // one engine, or no recycle: nothing to spread
        assert_eq!(first_recycle_at(0, 1, 4 * gib), 4 * gib);
        assert_eq!(first_recycle_at(3, 36, 0), 0);
    }

    #[test]
    fn an_engine_is_started_again_when_it_died_or_keeps_failing() {
        // up and well
        assert!(!engine_needs_a_start(true, false, false, 0, false));
        assert!(!engine_needs_a_start(true, false, false, 2, false));
        // its process exited, or three renders in a row failed on it
        assert!(engine_needs_a_start(true, false, true, 0, false));
        assert!(engine_needs_a_start(true, false, false, 3, false));
        // down after a failed start: only when its retry is due
        assert!(!engine_needs_a_start(false, false, true, 9, false));
        assert!(engine_needs_a_start(false, false, false, 0, true));
        // never while a start is under way
        assert!(!engine_needs_a_start(true, true, true, 9, true));
        assert!(!engine_needs_a_start(false, true, true, 9, true));
    }

    /// 512 pages a clone: the memory server's copy, the clones busy and one more must fit
    /// by count, whatever the kernel says is free just now.
    #[test]
    fn a_clone_starts_only_while_the_pool_holds_every_reservation() {
        // 14,000 pages: the server and 26 clones, so a 26th may start beside 25 busy
        assert!(pool_has_room(14_000, 14_000, 512, 0));
        assert!(pool_has_room(14_000, 1_000, 512, 25));
        assert!(!pool_has_room(14_000, 14_000, 512, 26));
        // room by count, but the kernel reports less than one clone free
        assert!(!pool_has_room(14_000, 511, 512, 3));
        // the smallest pool the module starts with: the server, one in flight, one in teardown
        assert!(pool_has_room(1_536, 1_024, 512, 1));
        assert!(!pool_has_room(1_536, 1_024, 512, 2));
    }

    /// A body is counted and cut in bytes. An answer that ends inside a character (more is
    /// to come) is not whole, and nothing is sliced inside a character.
    #[test]
    fn an_answers_length_is_in_bytes() {
        let head = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n";
        // "a", "é" (2 bytes), then the first byte of another "é": 4 of the 5 bytes
        let mut partial = head.to_vec();
        partial.extend_from_slice(&[0x61, 0xC3, 0xA9, 0xC3]);
        assert_eq!(http_body(&partial, false), None);
        let mut whole = partial.clone();
        whole.push(0xA9);
        assert_eq!(http_body(&whole, false).as_deref(), Some("aéé"));
        // bytes that are not text at all still have a length
        let mut binary = b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n".to_vec();
        binary.extend_from_slice(&[0xFF, 0xFE, 0xFD, 0x00]);
        assert!(http_body(&binary, false).is_some());
    }

    /// The module starts the harness's programs with options and reads fields that the
    /// harness must have. Without them the module ends at its `replays` stage, or runs
    /// with no viewport and no early answer and says nothing.
    #[test]
    fn the_harness_has_what_the_module_passes_it() {
        let bench = Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/chromium");
        let read = |name: &str| std::fs::read_to_string(bench.join(name)).unwrap();
        let replay = read("corpus_serve.py");
        for option in [
            "--reuse-port",
            "--tls-cert-dir",
            "--inject-file",
            "--http11",
        ] {
            assert!(
                replay.contains(&format!("\"{option}\"")),
                "corpus_serve.py has no {option}"
            );
        }
        assert!(
            read("cdpdrive.py").contains("\"--viewport\""),
            "cdpdrive.py has no --viewport"
        );
        let reqbench = read("reqbench.py");
        assert!(
            reqbench.contains("on_answer"),
            "reqbench.py tells no caller of an answer"
        );
        assert!(
            reqbench.contains("viewport=getattr(args, \"viewport\""),
            "reqbench.py forwards no viewport to a clone's driver"
        );
        // and the worker asks for each of them
        let worker = read("renders_worker.py");
        for used in ["on_answer", "viewport=opts.viewport"] {
            assert!(
                worker.contains(used),
                "renders_worker.py does not use {used}"
            );
        }
    }

    /// 512 pages a clone. The pool holds the memory server's copy and a clone for each of
    /// the side's workers, or the side is capped at what it does hold.
    #[test]
    fn the_pool_caps_the_renders_in_flight_it_cannot_carry() {
        assert_eq!(fcvm_workers(40), 62);
        assert_eq!(pool_cap(34_000, 512), 42); // 66 clones: 63 workers and the server
        assert_eq!(pool_cap(14_000, 512), 16); // 27 clones: 26 workers and the server
        assert_eq!(pool_cap(2_048, 512), 1);
        assert_eq!(pool_cap(1_536, 512), 0);
        assert_eq!(pool_cap(0, 512), 0);
        assert_eq!(pool_cap(0, 0), u32::MAX);
        // at its cap every worker can hold a clone and the check never refuses one
        for (total, pages) in [(34_000u64, 512u64), (14_000, 512), (2_048, 512)] {
            let most = pool_cap(total, pages);
            let busy = u64::from(fcvm_workers(most)) - 1;
            assert!(pool_has_room(total, total, pages, busy), "{total}");
        }
    }

    #[test]
    fn held_levels_above_what_a_side_may_run_are_refused() {
        assert_eq!(fixed_levels_refusal([16, 36], [16, 48]), None);
        assert_eq!(fixed_levels_refusal([0, 36], [0, 36]), None);
        // the pool lowered the fcvm side's most to 16
        let refused = fixed_levels_refusal([40, 36], [16, 48]).unwrap();
        assert!(
            refused.contains("fcvm side for 40") && refused.contains("16 at most"),
            "{refused}"
        );
        let refused = fixed_levels_refusal([8, 49], [40, 48]).unwrap();
        assert!(refused.contains("kitesurf side for 49"), "{refused}");
    }

    /// An engine start under way when the module ends would put a process into a cgroup
    /// the reaper is removing. The starts are kept in one set, and the module ends them
    /// before it returns to let its reaper go.
    #[test]
    fn engine_starts_are_ended_before_the_reaper_is_let_go() {
        let source = include_str!("mod.rs");
        let recycle = &source[source.find("    fn recycle(&mut self").unwrap()..];
        let recycle = &recycle[..recycle.find("\n    }\n").unwrap()];
        assert!(recycle.contains("self.starting.spawn("));
        assert!(!recycle.contains("tokio::spawn("));
        // and a finished start leaves the set when its result is handled
        let handled = &source[source.find("    async fn engine(&mut self").unwrap()..];
        let handled = &handled[..handled.find("\n    }\n").unwrap()];
        assert!(handled.contains("self.starting.try_join_next()"));
        let run = &source[source.find("async fn run(").unwrap()..];
        // not this test's own text
        let run = &run[..run.find("#[cfg(test)]").unwrap()];
        let stop = run
            .find("module.starting.shutdown().await;")
            .expect("the starts are ended");
        let returned = run.find("let ended = ended?;").unwrap();
        assert!(stop < returned, "ended before an error leaves run()");
    }

    /// The run directory is claimed after the start's sweep (which ends an earlier run's
    /// claim with its directory) and before the reaper and anything made in the directory.
    #[test]
    fn the_run_directory_is_claimed_between_the_sweep_and_the_reaper() {
        let source = include_str!("mod.rs");
        let source = &source[..source.find("#[cfg(test)]").unwrap()];
        let at = |text: &str| {
            source
                .find(text)
                .unwrap_or_else(|| panic!("{text} is in the module's start"))
        };
        let usable = at("lock.usable(&args.run_dir)?;");
        let swept = at("reap::sweep(&target).await;");
        let claimed = at("lock.claim(&args.run_dir)?;");
        let reaper = at("reap::Reaper::start(");
        assert!(usable < swept && swept < claimed && claimed < reaper);
    }

    fn two_pages() -> Vec<RendersPage> {
        ["https://a.test/", "https://b.test/"]
            .iter()
            .map(|u| RendersPage {
                id: page_id(u),
                url: u.to_string(),
            })
            .collect()
    }

    fn order(run: bool, overlay: Option<(&str, &str, u64)>) -> RendersOrder {
        RendersOrder {
            run,
            overlay: overlay.map(|(text, transform, seq)| OrderOverlay {
                text: Some(text.to_string()),
                transform: Some(transform.to_string()),
                chromium_only: fleet_core::renders::chromium_only(transform).unwrap(),
                seq,
            }),
        }
    }

    /// An overlay order writes each side's bytes (the text escaped, a Chromium-only
    /// transform on the fcvm side alone), the report then names the order the replays serve,
    /// and both searches start again: the windows are emptied, the counts go on.
    #[test]
    fn an_overlay_order_is_written_for_each_side_and_restarts_the_search() {
        let dir = tempfile::tempdir().unwrap();
        let files = [dir.path().join("fcvm.html"), dir.path().join("kite.html")];
        for f in &files {
            std::fs::write(f, "").unwrap();
        }
        let mut wall = Wall::new(two_pages(), SearchParams::default(), 30_000, 0);
        let mut served = Served::default();
        let applied = apply_order(&mut wall, &mut served, &files, Some(&order(true, None)), 0);
        assert_eq!(applied.unwrap(), Applied::Run);
        assert_eq!(std::fs::read_to_string(&files[0]).unwrap(), "");
        for at in [1_000, 2_000] {
            wall.render(&Completed {
                side: FCVM,
                page: 0,
                at,
                ms: 300.0,
                level: 1,
                error: None,
            });
        }
        let before = wall.report(2_000, None, None);
        assert!(before.sides["fcvm"].renders_per_s > 0.0);
        assert_eq!(before.overlay_seq, None);

        let o = order(true, Some(("<b>hi</b> & 'you'", "invert", 7)));
        let applied = apply_order(&mut wall, &mut served, &files, Some(&o), 3_000);
        assert_eq!(applied.unwrap(), Applied::Run);
        let fcvm = std::fs::read_to_string(&files[0]).unwrap();
        let kite = std::fs::read_to_string(&files[1]).unwrap();
        assert_eq!(fcvm, overlay::html(o.overlay.as_ref(), FCVM));
        assert_eq!(kite, overlay::html(o.overlay.as_ref(), KITE));
        assert!(
            fcvm.contains("&lt;b&gt;hi&lt;/b&gt; &amp; &#39;you&#39;"),
            "{fcvm}"
        );
        assert!(!fcvm.contains("<b>"), "{fcvm}");
        assert!(fcvm.contains("invert(1)") && !kite.contains("invert(1)"));
        assert!(
            kite.contains("fcvm-overlay"),
            "the text goes to both sides: {kite}"
        );
        assert_eq!(served.inject[FCVM], Some(sha256_hex(fcvm.as_bytes())));
        assert_eq!(served.inject[KITE], Some(sha256_hex(kite.as_bytes())));
        let after = wall.report(3_000, None, None);
        assert_eq!(after.overlay_seq, Some(7));
        assert_eq!(after.sides["fcvm"].renders_per_s, 0.0);
        assert_eq!(after.sides["fcvm"].done, 2);
        assert_eq!(after.sides["fcvm"].since, 3_000);
        assert_eq!(
            after.sides["fcvm"].search.phase,
            fleet_core::renders::SearchPhase::Starting
        );

        // A clear goes back to the pages as captured.
        let applied = apply_order(
            &mut wall,
            &mut served,
            &files,
            Some(&order(true, None)),
            4_000,
        );
        assert_eq!(applied.unwrap(), Applied::Run);
        assert_eq!(std::fs::read_to_string(&files[1]).unwrap(), "");
        assert_eq!(served.inject, [None, None]);
        assert_eq!(wall.report(4_000, None, None).overlay_seq, None);
    }

    /// The same order again (the coordinator sends it at the start of every connection)
    /// writes nothing: the file is the one the replays already read.
    #[test]
    fn an_order_the_replays_already_serve_writes_nothing() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let files = [dir.path().join("fcvm.html"), dir.path().join("kite.html")];
        let mut wall = Wall::new(two_pages(), SearchParams::default(), 30_000, 0);
        let mut served = Served::default();
        let o = order(true, Some(("hello", "mirror", 3)));
        apply_order(&mut wall, &mut served, &files, Some(&o), 1_000).unwrap();
        let inode = std::fs::metadata(&files[0]).unwrap().ino();
        apply_order(&mut wall, &mut served, &files, Some(&o), 2_000).unwrap();
        assert_eq!(std::fs::metadata(&files[0]).unwrap().ino(), inode);
        // the same text under another number is another order
        let renumbered = order(true, Some(("hello", "mirror", 4)));
        apply_order(&mut wall, &mut served, &files, Some(&renumbered), 3_000).unwrap();
        assert_ne!(std::fs::metadata(&files[0]).unwrap().ino(), inode);
        assert_eq!(wall.report(3_000, None, None).overlay_seq, Some(4));
    }

    /// `run: false`, or no order, stops the module, and the wall runs nothing from then on.
    #[test]
    fn an_order_to_stop_stops_the_module() {
        let dir = tempfile::tempdir().unwrap();
        let files = [dir.path().join("f"), dir.path().join("k")];
        for stop in [Some(order(false, Some(("x", "flip", 2)))), None] {
            let mut wall = Wall::new(two_pages(), SearchParams::default(), 30_000, 0);
            let mut served = Served::default();
            apply_order(&mut wall, &mut served, &files, Some(&order(true, None)), 0).unwrap();
            assert_eq!(wall.step(1_000, [None, None]), [1, 1]);
            let applied = apply_order(&mut wall, &mut served, &files, stop.as_ref(), 2_000);
            assert_eq!(applied.unwrap(), Applied::Stop);
            assert_eq!(wall.step(3_000, [None, None]), [0, 0]);
            let report = wall.report(3_000, None, None);
            assert_eq!(report.sides["fcvm"].search.in_flight, 0);
        }
    }

    /// A start is ended by an order to stop or by the end of the agent's connection, and by
    /// nothing else: an overlay change while it starts is applied when the loop begins.
    #[tokio::test]
    async fn a_start_is_ended_by_a_stop_and_by_the_end_of_the_connection() {
        let quick = Duration::from_millis(200);
        let (tx, mut rx) = watch::channel(Some(order(true, None)));
        assert!(tokio::time::timeout(quick, stop_ordered(&mut rx))
            .await
            .is_err());
        tx.send_replace(Some(order(true, Some(("hi", "zoom", 1)))));
        assert!(tokio::time::timeout(quick, stop_ordered(&mut rx))
            .await
            .is_err());
        tx.send_replace(Some(order(false, None)));
        let why = tokio::time::timeout(quick, stop_ordered(&mut rx)).await;
        assert_eq!(why.unwrap(), "an order to stop");

        let (tx, mut rx) = watch::channel(Some(order(true, None)));
        drop(tx);
        let why = tokio::time::timeout(quick, stop_ordered(&mut rx)).await;
        assert_eq!(why.unwrap(), "the end of the agent's connection");

        let (_tx, mut rx) = watch::channel(None);
        let why = tokio::time::timeout(quick, stop_ordered(&mut rx)).await;
        assert_eq!(why.unwrap(), "an order to stop");
    }

    fn agent_argv(extra: &[&str]) -> Vec<String> {
        [
            "--out=o",
            "--run-dir=r",
            "--bench=b",
            "--corpus=c",
            "--urls=https://a.test/,https://b.test/",
            "--snapshot-tag=t",
        ]
        .iter()
        .chain(extra)
        .map(|s| s.to_string())
        .collect()
    }

    /// Under the agent the module takes renders-bench's options, and refuses the benchmark's
    /// own: the coordinator's orders say when to render and what to inject.
    #[test]
    fn the_agent_takes_the_benchs_options_and_refuses_its_own() {
        let args = agent_args(&agent_argv(&["--max-engines", "36", "--replay-cpus=4"])).unwrap();
        assert_eq!(args.max_engines, 36);
        assert_eq!(args.replay_cpus, 4);
        assert_eq!(args.urls.len(), 2);
        assert_eq!(args.cpus_fcvm, "0-39");
        for own in [
            "--levels=16,36",
            "--seconds=60",
            "--overlay-text=hi",
            "--overlay-transform=flip",
        ] {
            let refused = agent_args(&agent_argv(&[own])).unwrap_err().to_string();
            assert!(refused.contains("the benchmark's"), "{own}: {refused}");
        }
        assert!(agent_args(&agent_argv(&["--cpus-engines=40-78"])).is_err());
        assert!(agent_args(&agent_argv(&["--no-such-option"])).is_err());
        assert!(agent_args(&["--out=o".to_string()]).is_err());
    }

    /// The hello says what runs: the pages in order, each side's CPUs, whether the unit holds
    /// every CPU the sides are given, and the build, the snapshot's shape winning over the
    /// arguments once it is made.
    #[test]
    fn the_hello_says_what_the_module_runs() {
        let args = agent_args(&agent_argv(&[
            "--setup=fcvm.commit=abc123",
            "--setup=kitesurf.commit=def456",
            "--vcpus=2",
            "--hugepages=false",
        ]))
        .unwrap();
        let all: Vec<u32> = (0..96).collect();
        let facts = HostFacts {
            unit_cpus: Some(&all),
            kernel: "7.3.3-fcvm-test",
            instance_type: Some("example.metal"),
            snapshot: None,
            bundle_sha256: Some("feed".into()),
        };
        let host = hello(&args, &facts).unwrap();
        assert_eq!(
            host.pages.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            ["a.test", "b.test"]
        );
        assert_eq!(host.cpus, 40);
        assert!(host.confined);
        let want: BTreeMap<String, String> = [
            ("instance_type", "example.metal"),
            ("kernel", "7.3.3-fcvm-test"),
            ("fcvm_commit", "abc123"),
            ("kitesurf_commit", "def456"),
            ("bundle_sha256", "feed"),
            ("recycle_mib", "4096"),
            ("viewport", "1280x720"),
            ("vcpus", "2"),
            ("page_size", "4 KiB"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(host.build, want);

        // A unit without the harness's CPUs cannot confine the module; a made snapshot says
        // its own shape.
        let few: Vec<u32> = (0..80).collect();
        let config = json!({"metadata": {"vcpu": 4, "memory_mib": 1024, "hugepages": true}});
        let host = hello(
            &args,
            &HostFacts {
                unit_cpus: Some(&few),
                instance_type: None,
                snapshot: Some(&config),
                ..facts
            },
        )
        .unwrap();
        assert!(!host.confined);
        assert_eq!(host.build["vcpus"], "4");
        assert_eq!(host.build["page_size"], "2 MiB");
        assert!(!host.build.contains_key("instance_type"));
        let host = hello(
            &args,
            &HostFacts {
                unit_cpus: None,
                kernel: "k",
                instance_type: None,
                snapshot: None,
                bundle_sha256: None,
            },
        )
        .unwrap();
        assert!(!host.confined);
        // a module without engines says nothing of them
        let args = agent_args(&agent_argv(&[
            "--max-engines=0",
            "--setup=kitesurf.commit=x",
        ]))
        .unwrap();
        let host = hello(
            &args,
            &HostFacts {
                unit_cpus: None,
                kernel: "k",
                instance_type: None,
                snapshot: None,
                bundle_sha256: Some("feed".into()),
            },
        )
        .unwrap();
        for k in ["kitesurf_commit", "bundle_sha256", "recycle_mib"] {
            assert!(!host.build.contains_key(k), "{k}");
        }
    }

    /// The benchmark command and the agent go through one session, and in it the reaper is
    /// running before the module makes anything: a module whose process is killed outright
    /// leaves the reaper to remove what it made, under the agent as under the benchmark.
    #[test]
    fn both_callers_share_one_session_that_starts_the_reaper_first() {
        let source = include_str!("mod.rs");
        let source = &source[..source.find("#[cfg(test)]").unwrap()];
        let body = |name: &str| {
            let at = source
                .find(name)
                .unwrap_or_else(|| panic!("{name} is in the module"));
            let rest = &source[at..];
            &rest[..rest.find("\n}\n").unwrap()]
        };
        let bench = body("pub async fn bench(");
        assert!(bench.contains("session(&args, &exe, orders, None, Signals::EndTheModule, None)"));
        assert!(!bench.contains("run("));
        let session = body("pub async fn session(");
        let reaper = session.find("reap::Reaper::start(").unwrap();
        let run = session.find("let outcome = run(").unwrap();
        let finish = session.find("reaper.finish().await").unwrap();
        assert!(reaper < run && run < finish);
        // the only call of run() is the session's
        assert_eq!(source.matches("= run(\n").count(), 1);
        assert_eq!(
            source.matches("run(\n").count(),
            2,
            "the definition and the call"
        );
        // the agent starts the module through the session, with the signals and the bounds
        // it names (`agent::renders::AGENT_SIGNALS` and `AGENT_BOUNDS`, pinned by value there)
        let agent = include_str!("../agent/renders.rs");
        let agent = &agent[..agent.find("#[cfg(test)]").unwrap()];
        let calls: Vec<Vec<&str>> = agent
            .match_indices("crate::fleet::renders::session(")
            .map(|(at, call)| {
                let rest = &agent[at + call.len()..];
                let rest = rest[..rest.find(".await").unwrap()].trim_end();
                rest.strip_suffix(')')
                    .unwrap()
                    .split(',')
                    .map(str::trim)
                    .filter(|a| !a.is_empty())
                    .collect()
            })
            .collect();
        assert_eq!(
            calls.len(),
            1,
            "the agent's module is the session: {calls:?}"
        );
        assert_eq!(calls[0][4..], ["AGENT_SIGNALS", "Some(AGENT_BOUNDS)"]);
    }

    /// A start ended by an order to stop must leave nothing running: it spawns no task an
    /// engine start could run on in (the workers' output readers start nothing), and the
    /// signal handlers are the benchmark's alone.
    #[test]
    fn a_start_spawns_no_task_and_the_agent_registers_no_signal() {
        let source = include_str!("mod.rs");
        let source = &source[..source.find("#[cfg(test)]").unwrap()];
        let start = &source[source.find("\nasync fn start(\n").unwrap()..];
        let start = &start[..start.find("\n}\n").unwrap()];
        // (a worker's output reader is a task of the start, and starts nothing)
        for spawned in ["tokio::spawn(", ".spawn(async", "spawn_blocking("] {
            assert!(!start.contains(spawned), "the start uses {spawned}");
        }
        assert!(start.contains("try_join_all(batch)"));
        let run = &source[source.find("\nasync fn run(\n").unwrap()..];
        let run = &run[..run.find("\n}\n").unwrap()];
        let guard = run
            .find("let mut signals = if signals == Signals::EndTheModule {")
            .unwrap();
        let term = run.find("signal(SignalKind::terminate())").unwrap();
        let none = run.find("        None\n    };").unwrap();
        assert!(guard < term && term < none);
        assert_eq!(source.matches("SignalKind::terminate()").count(), 1);
    }

    /// A bounded records file is renamed to `.1` before a line would take it past its limit,
    /// so it and its previous file hold at most twice the limit, in whole lines; an
    /// unbounded one (the benchmark's) is never rotated.
    #[test]
    fn a_bounded_records_file_keeps_one_previous_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("renders.jsonl");
        let mut r = Records::create(path.clone(), Some(100)).unwrap();
        let line = |i: usize| format!("{{\"seq\":{i},\"pad\":\"{}\"}}", "x".repeat(20));
        for i in 0..40 {
            r.line(&line(i));
        }
        r.flush();
        let now = std::fs::read_to_string(&path).unwrap();
        let before = std::fs::read_to_string(dir.path().join("renders.jsonl.1")).unwrap();
        assert!(
            now.len() <= 100 && before.len() <= 100,
            "{} {}",
            now.len(),
            before.len()
        );
        assert!(now.ends_with(&format!("{}\n", line(39))), "{now}");
        // the previous file is the lines just before the current one, all whole
        let first_now: usize = now.lines().count();
        assert_eq!(
            before.lines().last().unwrap(),
            line(39 - first_now),
            "{before}"
        );
        assert!(before
            .lines()
            .chain(now.lines())
            .all(|l| l.ends_with("\"}")));
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(names.len(), 2);

        let path = dir.path().join("samples.jsonl");
        let mut r = Records::create(path.clone(), None).unwrap();
        for i in 0..40 {
            r.line(&line(i));
        }
        r.flush();
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 40);
        assert!(!dir.path().join("samples.jsonl.1").exists());
    }

    /// The failed clones' logs past the newest kept are removed, across the workers'
    /// directories; a log written lately is a render still under way and is left alone.
    #[test]
    fn the_failed_clones_logs_past_the_newest_kept_are_removed() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let dir = tempfile::tempdir().unwrap();
        let at = |s: u64| UNIX_EPOCH + Duration::from_secs(1_000_000 + s);
        let mut all = Vec::new();
        for (worker, ages) in [(0, [10u64, 30, 50]), (1, [20, 40, 995])] {
            let sub = dir.path().join(format!("logs-{worker}"));
            std::fs::create_dir(&sub).unwrap();
            for age in ages {
                let path = sub.join(format!("rb-{age}.log"));
                std::fs::write(&path, "x").unwrap();
                let f = std::fs::File::options().write(true).open(&path).unwrap();
                f.set_modified(at(age)).unwrap();
                all.push((age, path));
            }
        }
        let now = at(1_000);
        // settled: written over 60 s ago (995 is under way); keep the newest 3 of the five
        let removed = prune_clone_logs(dir.path(), 3, Duration::from_secs(60), now);
        assert_eq!(removed, 2);
        for (age, path) in &all {
            assert_eq!(path.exists(), ![10, 20].contains(age), "{age}");
        }
        assert_eq!(
            prune_clone_logs(dir.path(), 3, Duration::from_secs(60), now),
            0
        );
        assert_eq!(
            prune_clone_logs(
                &dir.path().join("absent"),
                0,
                Duration::ZERO,
                SystemTime::now()
            ),
            0
        );
    }

    #[test]
    fn a_page_id_is_its_host_and_path() {
        assert_eq!(page_id("https://example.com/"), "example.com");
        assert_eq!(
            page_id("https://todomvc.com/examples/react/dist/index.html"),
            "todomvc.com-examples-react-dist-index.html"
        );
        assert_eq!(
            page_id("https://www.RTP.pt/noticias/"),
            "www.rtp.pt-noticias"
        );
    }

    #[test]
    fn the_pool_has_room_for_what_is_free_and_not_reserved() {
        let meminfo = "MemTotal: 1 kB\nHugePages_Total:   30000\nHugePages_Free:    20000\nHugePages_Rsvd:     1536\nHugePages_Surp: 0\n";
        assert_eq!(hugepages(meminfo), (30000, 18464));
        assert_eq!(hugepages("MemTotal: 1 kB\n"), (0, 0));
        // more reserved than free: no room, not a wrapped number
        assert_eq!(
            hugepages("HugePages_Total: 10\nHugePages_Free: 2\nHugePages_Rsvd: 5\n"),
            (10, 0)
        );
    }

    #[test]
    fn the_harness_is_split_into_two_replays_and_the_drivers() {
        let cpus: Vec<u32> = (80..96).collect();
        let [f, e, d] = split_harness(&cpus, 4).unwrap();
        assert_eq!(f, (80..84).collect::<Vec<_>>());
        assert_eq!(e, (84..88).collect::<Vec<_>>());
        assert_eq!(d, (88..96).collect::<Vec<_>>());
        assert!(split_harness(&cpus, 8).is_err());
        assert!(split_harness(&[1, 2, 3], 2).is_err());
        // not split: one pool for all three
        let [f, e, d] = split_harness(&cpus, 0).unwrap();
        assert_eq!((f.len(), e.len(), d.len()), (16, 16, 16));
    }

    #[test]
    fn setup_values_are_by_side() {
        let v = setup_values(&["fcvm.commit=abc".into(), "kitesurf.build=opt".into()]).unwrap();
        assert_eq!(v[FCVM]["commit"], "abc");
        assert_eq!(v[KITE]["build"], "opt");
        assert!(setup_values(&["nobody.x=1".into()]).is_err());
        assert!(setup_values(&["fcvm.x".into()]).is_err());
    }

    #[test]
    fn the_captures_name_their_hosts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("a")).unwrap();
        std::fs::write(
            dir.path().join("a/index.json"),
            r#"{"url":"https://a.test/","resources":{"https://a.test/":{},"https://cdn.b.test/x.js":{}},"redirects":{"https://www.a.test/":"https://a.test/"}}"#,
        )
        .unwrap();
        assert_eq!(
            corpus_hosts(dir.path()).unwrap(),
            ["a.test", "cdn.b.test", "www.a.test"]
        );
        assert!(corpus_hosts(tempfile::tempdir().unwrap().path()).is_err());
    }
}
