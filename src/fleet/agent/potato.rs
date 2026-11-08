//! Potato mode on a host (docs/fleet/ARCHITECTURE.md, "Potato mode"): the agent runs the
//! host's player VMs, puts each to sleep once it has thrown the ball and wakes it before the
//! ball comes round, and reports what the players say and what it did to them.
//!
//! The ball never passes through the agent, apart from a round's first throw: it goes from
//! one player VM to the next. The agent learns of it from the players' reports on
//! `127.0.0.1:7071`, which every player VM forwards to the host.
//!
//! Every player is a clone of one snapshot of an unassigned player (`potato-gold`), made
//! when the agent starts; a new player is restored from it and given its number over its
//! ball port. A player sleeps with `fcvm snapshot create --tag potato-<n> --stop`, its memory
//! in a tmpfs (`--memory-dir`, the agent's `--potato-sleep-memory-dir`), and wakes
//! with `fcvm snapshot run --snapshot potato-<n>`. Players start fresh with every agent: a
//! player is never woken from a snapshot an earlier agent left.

use super::addr::Prefix;
use super::vm::Vm;
use anyhow::{bail, Context, Result};
use fleet_core::potato::{PotatoEvent, PotatoKind, PotatoProof, PotatoRound};
use fleet_core::protocol::AgentMsg;
use fleet_potato::{Inbound, Report, BALL_PORT};
use std::collections::{BTreeMap, VecDeque};
use std::net::Ipv6Addr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch, Semaphore};

/// The port the players report on, on the host's loopback (each VM forwards it).
pub const REPORT_PORT: u16 = 7071;
/// The snapshot every player is a clone of.
pub const GOLD: &str = "potato-gold";
/// The gold VM's address: the last before the players'.
const GOLD_OFFSET: u32 = 0xfff;
/// How long a wake may take before it counts as failed.
const WAKE_DEADLINE: Duration = Duration::from_secs(60);
/// How long a first boot (the gold's, or a clone's assignment) may take.
const BOOT_DEADLINE: Duration = Duration::from_secs(600);
/// Restores of new players at once.
const BOOT_PARALLEL: usize = 8;
/// The lead is the slowest recent wake plus this, in holds.
const LEAD_MARGIN_MS: f64 = 150.0;
/// The wake time assumed before any wake was seen.
const FIRST_WAKE_MS: f64 = 1500.0;
/// Wakes the lead is computed over.
const RECENT_WAKES: usize = 20;

#[derive(Debug, Clone)]
pub struct PotatoCfg {
    pub fcvm: PathBuf,
    pub image: String,
    /// The most players this host runs.
    pub players: u32,
    pub prefix: Prefix,
    pub mem_mib: u32,
    /// `snapshot create --memory-dir` for a sleep; None keeps the memory on the store.
    pub sleep_memory_dir: Option<PathBuf>,
    /// The most the sleeps' memory files may hold in `sleep_memory_dir`, in MiB. A sleep that
    /// would go over it keeps its memory on the store instead.
    pub sleep_memory_cap_mib: u64,
}

impl PotatoCfg {
    fn tag(n: u32) -> String {
        format!("potato-{n}")
    }

    /// `fcvm snapshot create` for putting player `n`, whose fcvm is `pid`, to sleep, its memory
    /// in `memory_dir` when given.
    fn sleep_args(n: u32, pid: u32, memory_dir: Option<&std::path::Path>) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "snapshot".into(),
            "create".into(),
            "--pid".into(),
            pid.to_string(),
            "--tag".into(),
            Self::tag(n),
            "--stop".into(),
        ];
        if let Some(dir) = memory_dir {
            args.push("--memory-dir".into());
            args.push(dir.display().to_string());
        }
        args
    }

    fn gold_args(&self) -> Result<Vec<String>> {
        let addr = self.prefix.player_address(0)?;
        let gold = Ipv6Addr::from(u128::from(addr) - 0x1000 + u128::from(GOLD_OFFSET));
        Ok(vec![
            "podman".into(),
            "run".into(),
            "--name".into(),
            GOLD.into(),
            "--network".into(),
            "routed".into(),
            "--ipv6-prefix".into(),
            self.prefix.to_string(),
            "--ipv6-address".into(),
            gold.to_string(),
            "--mem".into(),
            self.mem_mib.to_string(),
            "--cpu".into(),
            "1".into(),
            "--forward-localhost".into(),
            REPORT_PORT.to_string(),
            self.image.clone(),
        ])
    }

    /// `fcvm snapshot run` of `snapshot` as player `n`.
    fn restore_args(&self, snapshot: &str, n: u32) -> Result<Vec<String>> {
        Ok(vec![
            "snapshot".into(),
            "run".into(),
            "--snapshot".into(),
            snapshot.into(),
            "--name".into(),
            Self::tag(n),
            "--ipv6-prefix".into(),
            self.prefix.to_string(),
            "--ipv6-address".into(),
            self.prefix.player_address(n)?.to_string(),
        ])
    }
}

/// What the agent is doing with a player.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// No snapshot from this agent: it must be restored from the gold.
    Missing,
    Booting,
    Awake,
    Sleeping,
    Asleep,
    Waking,
    Failed,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::Missing => "missing",
            Phase::Booting => "booting",
            Phase::Awake => "awake",
            Phase::Sleeping => "sleeping",
            Phase::Asleep => "asleep",
            Phase::Waking => "waking",
            Phase::Failed => "failed",
        }
    }
}

struct Player {
    phase: Phase,
    since: Instant,
    vm: Option<Vm>,
    /// It threw while its wake was still finishing (it accepts the ball before the agent
    /// hears that its wake is done): it sleeps as soon as the wake is.
    sleep_after_wake: bool,
}

pub(crate) enum Done {
    Gold(Result<()>),
    RetryGold,
    Booted(u32, Result<Option<Vm>>),
    Slept(u32, Result<BTreeMap<String, f64>>, f64),
    Woke(u32, Result<(Option<Vm>, BTreeMap<String, f64>)>, f64),
    Kicked(u64, Result<()>),
}

/// A player's report as the agent received it.
struct Heard {
    report: Report,
    at: u64,
}

/// The round the agent is playing.
struct Playing {
    order: PotatoRound,
    /// The latest catch's seq, once there has been one (`caught`).
    seq: u64,
    caught: bool,
    kicked: bool,
    /// The ball dropped (or the round was replaced): no more wakes.
    over: bool,
}

impl Playing {
    /// The hold the ball carries at `seq`.
    fn hold_ms(&self) -> u64 {
        let o = &self.order;
        let laps = self.seq / o.ring.len().max(1) as u64;
        let mut h = o.hold_ms;
        for _ in 0..laps.min(1000) {
            h = fleet_potato::shrink(h, o.shrink_permille, o.min_hold_ms);
        }
        match o.pace_ms {
            Some(p) => h.min(p.max(o.min_hold_ms)),
            None => h,
        }
    }
}

pub fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 10_000.0).round() / 10.0
}

/// One message to a player's ball port and its one-line answer.
async fn ask(addr: Ipv6Addr, msg: &Inbound, deadline: Duration) -> Result<String> {
    tokio::time::timeout(deadline, async {
        let mut c = TcpStream::connect((addr, BALL_PORT)).await?;
        c.set_nodelay(true)?;
        let mut line = serde_json::to_string(msg)?;
        line.push('\n');
        c.write_all(line.as_bytes()).await?;
        let mut answer = String::new();
        BufReader::new(c).read_line(&mut answer).await?;
        anyhow::Ok(answer)
    })
    .await
    .with_context(|| format!("[{addr}]:{BALL_PORT} did not answer in {deadline:?}"))?
}

/// Probe a player's ball port every 2 ms until it answers, up to `deadline`.
async fn until_listening(addr: Ipv6Addr, deadline: Duration) -> Result<Instant> {
    let end = Instant::now() + deadline;
    let mut last = String::new();
    while Instant::now() < end {
        match ask(addr, &Inbound::Probe, Duration::from_millis(250)).await {
            Ok(a)
                if matches!(
                    serde_json::from_str::<Report>(a.trim()),
                    Ok(Report::Probe { .. })
                ) =>
            {
                return Ok(Instant::now())
            }
            Ok(a) => last = a,
            Err(e) => last = format!("{e:#}"),
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    bail!(
        "[{addr}]:{BALL_PORT} never answered a probe: {}",
        last.trim()
    )
}

/// Run `fcvm <args>` to its end with fcvm's info log, and return what it printed.
async fn run_fcvm(fcvm: &std::path::Path, args: &[String]) -> Result<String> {
    let out = tokio::process::Command::new(fcvm)
        .args(args)
        .env("RUST_LOG", "fcvm=info")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .with_context(|| format!("starting {}", fcvm.display()))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if !out.status.success() {
        let tail: Vec<&str> = text.lines().rev().take(8).collect();
        bail!(
            "fcvm {} exited {}: {}",
            args.first().map(String::as_str).unwrap_or(""),
            out.status,
            tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
        );
    }
    Ok(text)
}

/// How long the agent waits for a slept player's fcvm to exit after `snapshot create --stop`.
const REAP_DEADLINE: Duration = Duration::from_secs(40);

/// The agent's own steps of a sleep, which together make up its `sleep_ms`: telling the
/// player its round is over (`ask_ms`, a sleep at a round's end only), `fcvm snapshot create
/// --stop` from start to exit (`create_ms`, which fcvm's own stages split), and waiting for
/// the player's fcvm to exit (`reap_ms`).
#[derive(Debug, Clone, Copy, PartialEq)]
struct SleepSteps {
    ask_ms: f64,
    create_ms: f64,
    reap_ms: f64,
}

impl SleepSteps {
    fn into_stages(self, mut stages: BTreeMap<String, f64>) -> BTreeMap<String, f64> {
        let tenth = |v: f64| (v * 10.0).round() / 10.0;
        stages.insert("ask_ms".into(), tenth(self.ask_ms));
        stages.insert("create_ms".into(), tenth(self.create_ms));
        stages.insert("reap_ms".into(), tenth(self.reap_ms));
        stages
    }

    fn describe(self) -> String {
        format!(
            "after {:.0} ms telling it the round is over, {:.0} ms in snapshot create and {:.0} ms waiting for its fcvm to exit",
            self.ask_ms, self.create_ms, self.reap_ms
        )
    }
}

/// The numbers fcvm's `snapshot create --stop` logs: the boundary's steps and the stop.
pub fn sleep_stages(log: &str) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    let ansi = |l: &str| {
        let mut s = String::with_capacity(l.len());
        let mut it = l.chars().peekable();
        while let Some(c) = it.next() {
            if c == '\u{1b}' {
                for d in it.by_ref() {
                    if d == 'm' {
                        break;
                    }
                }
            } else {
                s.push(c);
            }
        }
        s
    };
    for line in log.lines().map(ansi) {
        let fields: &[(&str, &str)] = if line.contains("snapshot boundary timings") {
            &[
                ("prepare_ms", "prepare_ms"),
                ("pause_ms", "pause_ms"),
                ("save_ms", "save_ms"),
                ("disk_ms", "disk_ms"),
            ]
        } else if line.contains("snapshot created successfully") {
            &[("stage_ms", "stage_ms"), ("publish_ms", "publish_ms")]
        } else if line.contains("stopped the VM after its snapshot") {
            &[("stop_ms", "stop_ms")]
        } else if line.contains("diff written in place") || line.contains("memory file written") {
            &[("written_bytes", "written_bytes")]
        } else {
            continue;
        };
        for (key, name) in fields {
            let pat = format!("{name}=");
            if let Some(v) = line
                .split_whitespace()
                .find_map(|w| w.strip_prefix(pat.as_str()))
                .and_then(|v| v.parse::<f64>().ok())
            {
                out.insert((*key).to_string(), v);
            }
        }
    }
    out
}

fn proof(p: &fleet_potato::Proof) -> PotatoProof {
    PotatoProof {
        colour: p.colour,
        counter: p.counter,
        chain: p.chain.clone(),
    }
}

/// Run potato mode until the agent stops. `orders` is the coordinator's round; every event
/// goes to `out`.
pub async fn run(
    cfg: Arc<PotatoCfg>,
    mut orders: watch::Receiver<Option<PotatoRound>>,
    out: mpsc::UnboundedSender<AgentMsg>,
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", REPORT_PORT))
        .await
        .with_context(|| format!("listening for player reports on 127.0.0.1:{REPORT_PORT}"))?;
    let (heard_tx, mut heard) = mpsc::unbounded_channel::<Heard>();
    tokio::spawn(serve_reports(listener, heard_tx));
    let (done_tx, mut done) = mpsc::unbounded_channel::<Done>();
    // A memory directory fcvm would refuse (another user's, or one others can write), or one
    // that cannot be emptied, leaves every sleep on the store rather than failing each one.
    let sleep_memory = SleepMemory::new(&cfg).filter(|memory| {
        let ready =
            crate::storage::disk::prepare_memory_dir(&memory.dir, &crate::paths::snapshot_dir())
                .and_then(|_| memory.clear());
        if let Err(e) = &ready {
            tracing::warn!(dir = %memory.dir.display(), error = %format!("{e:#}"),
                "potato: sleeps keep their memory on the store");
        }
        ready.is_ok()
    });
    let mut d = Driver {
        cfg: cfg.clone(),
        ops: Arc::new(FcvmOps {
            cfg: cfg.clone(),
            sleep_memory,
            boot_slots: Arc::new(Semaphore::new(BOOT_PARALLEL)),
            tasks: Default::default(),
        }),
        out,
        done: done_tx,
        players: (0..cfg.players)
            .map(|_| Player {
                phase: Phase::Missing,
                since: Instant::now(),
                vm: None,
                sleep_after_wake: false,
            })
            .collect(),
        gold_ready: false,
        wakes: VecDeque::new(),
        playing: None,
        late: None,
        ended: None,
    };
    d.make_gold();
    loop {
        tokio::select! {
            changed = orders.changed() => {
                if changed.is_err() {
                    break;
                }
                let order = orders.borrow_and_update().clone();
                d.order(order);
            }
            Some(h) = heard.recv() => d.heard(h),
            Some(x) = done.recv() => d.done(x),
        }
    }
    tracing::info!("potato mode stopping: every player's VM goes with the agent");
    // in-flight work first (a boot waiting for a slot would start a VM after this), then the
    // players' VMs, which stop when the driver drops them
    d.ops.shutdown();
    drop(d);
    Ok(())
}

async fn serve_reports(listener: tokio::net::TcpListener, tx: mpsc::UnboundedSender<Heard>) {
    loop {
        let Ok((conn, _)) = listener.accept().await else {
            continue;
        };
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(conn).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<Report>(&line) {
                    Ok(report) => {
                        let at = unix_ms();
                        // every report as read, so a report a player sent and the agent never
                        // read can be told from one the coordinator read late
                        tracing::debug!(target: "fcvm::fleet::potato_reports", at, report = %line, "potato report read");
                        if tx.send(Heard { report, at }).is_err() {
                            return;
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "a player's report is not JSON"),
                }
            }
        });
    }
}

/// The work the driver asks for, each finished by a [`Done`] on `done`: real fcvm commands
/// ([`FcvmOps`]), or a recorder in the driver's tests.
pub(crate) trait Ops: Send + Sync {
    fn make_gold(&self, done: mpsc::UnboundedSender<Done>);
    fn boot(&self, n: u32, colours: u32, done: mpsc::UnboundedSender<Done>);
    /// Put player `n` to sleep; with `stop`, first tell it that round is over, so the snapshot
    /// holds no throw of it.
    fn sleep(&self, n: u32, vm: Option<Vm>, stop: Option<u64>, done: mpsc::UnboundedSender<Done>);
    /// Restore player `n`, and once its ball port answers, tell it it woke (refusing balls of
    /// rounds below `round`).
    fn wake(&self, n: u32, round: Option<u64>, done: mpsc::UnboundedSender<Done>);
    fn kick(&self, order: PotatoRound, done: mpsc::UnboundedSender<Done>);
    fn pace(&self, n: u32, round: u64, hold_ms: u64);
    /// Abort every operation in flight; the agent is stopping.
    fn shutdown(&self);
}

/// Where sleeping players' memory goes, and its budget, counted from what the agent did
/// rather than read from the directory: each player whose last sleep put its memory there
/// holds one memory's worth until its next sleep replaces it, and each sleep in flight
/// holds one more until it ends. The directory is the agent's own, emptied when it starts.
pub(crate) struct SleepMemory {
    dir: PathBuf,
    /// One player's memory, in bytes.
    each: u64,
    /// The most the memory files may take, in bytes.
    cap: u64,
    state: std::sync::Mutex<SleepMemoryState>,
}

#[derive(Default)]
struct SleepMemoryState {
    /// Players whose last sleep put their memory in the directory.
    held: std::collections::BTreeSet<u32>,
    /// Sleeps writing there now.
    in_flight: u64,
}

/// One sleep's place in [`SleepMemory`]'s budget, given back when the sleep ends
/// ([`SleepReservation::finish`], or a drop for a sleep that did not finish).
struct SleepReservation {
    memory: Arc<SleepMemory>,
    player: u32,
    done: bool,
}

impl SleepReservation {
    /// The sleep ended. Its player is counted as holding a memory file whether or not the
    /// create succeeded: `snapshot create` can fail after it published (recording the VM's
    /// diff parent comes later), and a failure before publication leaves the player's earlier
    /// file, if any, in place. Counting one that is not there only sends a later sleep to the
    /// store; missing one that is there would let the directory outgrow its cap.
    fn finish(mut self) {
        let mut state = self.memory.state.lock().unwrap();
        state.in_flight -= 1;
        state.held.insert(self.player);
        self.done = true;
    }
}

impl Drop for SleepReservation {
    fn drop(&mut self) {
        if !self.done {
            self.memory.state.lock().unwrap().in_flight -= 1;
        }
    }
}

impl SleepMemory {
    fn new(cfg: &PotatoCfg) -> Option<Arc<Self>> {
        let dir = cfg.sleep_memory_dir.clone()?;
        Some(Arc::new(Self {
            dir,
            each: u64::from(cfg.mem_mib) << 20,
            cap: cfg.sleep_memory_cap_mib.saturating_mul(1 << 20),
            state: Default::default(),
        }))
    }

    /// Reserve room for player `n`'s sleep, if every memory held, every sleep in flight and
    /// this one fit under the cap. None: the sleep keeps its memory on the store, and a
    /// player whose memory was held stops being counted once that sleep replaces it.
    fn reserve(self: &Arc<Self>, n: u32) -> Option<SleepReservation> {
        let mut state = self.state.lock().unwrap();
        let after = (state.held.len() as u64 + state.in_flight + 1).saturating_mul(self.each);
        if after > self.cap {
            tracing::warn!(dir = %self.dir.display(), held = state.held.len(), in_flight = state.in_flight,
                cap_mib = self.cap >> 20, "potato: the sleep memory directory is at its cap; this sleep keeps its memory on the store");
            return None;
        }
        state.in_flight += 1;
        Some(SleepReservation {
            memory: self.clone(),
            player: n,
            done: false,
        })
    }

    /// A sleep of player `n` that kept its memory on the store published: the player's memory
    /// file in the directory, if it had one, went with the generation it replaced.
    fn left(&self, n: u32) {
        self.state.lock().unwrap().held.remove(&n);
    }

    /// Remove what an earlier agent's sleeps left in the directory (its players are not woken
    /// again, and a sleep killed before it published leaves a file nothing links to), so the
    /// count starts from an empty directory. Called before any sleep.
    fn clear(&self) -> Result<()> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.dir.display())),
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", self.dir.display()))?;
            let name = entry.file_name();
            if name
                .to_str()
                .is_some_and(|n| n.starts_with(crate::storage::disk::LINKED_MEMORY_PREFIX))
            {
                std::fs::remove_file(entry.path())
                    .with_context(|| format!("removing {}", entry.path().display()))?;
            }
        }
        Ok(())
    }
}

pub(crate) struct FcvmOps {
    cfg: Arc<PotatoCfg>,
    /// None: sleeps keep their memory on the store.
    sleep_memory: Option<Arc<SleepMemory>>,
    boot_slots: Arc<Semaphore>,
    /// Every operation in flight, so a shutdown can abort them: an aborted task drops the VM it
    /// holds, which sends its fcvm SIGTERM, and a boot still waiting for a slot starts nothing.
    tasks: std::sync::Mutex<tokio::task::JoinSet<()>>,
}

impl FcvmOps {
    fn spawn(&self, f: impl std::future::Future<Output = ()> + Send + 'static) {
        let mut tasks = self.tasks.lock().unwrap();
        while tasks.try_join_next().is_some() {}
        tasks.spawn(f);
    }
}

impl Ops for FcvmOps {
    fn shutdown(&self) {
        self.tasks.lock().unwrap().abort_all();
    }

    /// The gold snapshot: an unassigned player, booted and put to sleep.
    fn make_gold(&self, done: mpsc::UnboundedSender<Done>) {
        let cfg = self.cfg.clone();
        self.spawn(async move {
            let r = async {
                let args = cfg.gold_args()?;
                let gold = Vm::spawn_args(&cfg.fcvm, &args, GOLD)?;
                let addr: Ipv6Addr =
                    args[args.iter().position(|a| a == "--ipv6-address").unwrap() + 1].parse()?;
                until_listening(addr, BOOT_DEADLINE).await?;
                let mut gold = gold;
                let created = run_fcvm(
                    &cfg.fcvm,
                    &[
                        "snapshot".into(),
                        "create".into(),
                        "--pid".into(),
                        gold.pid.to_string(),
                        "--tag".into(),
                        GOLD.into(),
                        "--stop".into(),
                    ],
                )
                .await;
                gold.stop(Duration::from_secs(30)).await;
                created.map(|_| ())
            }
            .await;
            let _ = done.send(Done::Gold(r));
        });
    }

    fn boot(&self, n: u32, colours: u32, done: mpsc::UnboundedSender<Done>) {
        let cfg = self.cfg.clone();
        let slots = self.boot_slots.clone();
        self.spawn(async move {
            let r = async {
                let _slot = slots.acquire().await?;
                let addr = cfg.prefix.player_address(n)?;
                let vm =
                    Vm::spawn_args(&cfg.fcvm, &cfg.restore_args(GOLD, n)?, &PotatoCfg::tag(n))?;
                until_listening(addr, BOOT_DEADLINE).await?;
                let answer = ask(
                    addr,
                    &Inbound::Assign {
                        player: n,
                        colours: Some(colours),
                    },
                    Duration::from_secs(5),
                )
                .await?;
                if let Err(e) = fleet_potato::Reply::ok(&answer) {
                    bail!("player {n} refused its number: {e}");
                }
                Ok(Some(vm))
            }
            .await;
            let _ = done.send(Done::Booted(n, r));
        });
    }

    fn sleep(&self, n: u32, vm: Option<Vm>, stop: Option<u64>, done: mpsc::UnboundedSender<Done>) {
        let cfg = self.cfg.clone();
        let sleep_memory = self.sleep_memory.clone();
        self.spawn(async move {
            let started = Instant::now();
            if let (Some(round), Ok(addr)) = (stop, cfg.prefix.player_address(n)) {
                if let Err(e) = ask(addr, &Inbound::Stop { round }, Duration::from_secs(2)).await {
                    tracing::warn!(player = n, round, error = %format!("{e:#}"), "potato: a round's end did not reach a player");
                }
            }
            let asked = started.elapsed();
            let Some(mut vm) = vm else {
                let _ = done.send(Done::Slept(
                    n,
                    Err(anyhow::anyhow!("player {n} has no VM")),
                    0.0,
                ));
                return;
            };
            let create = Instant::now();
            let reservation = sleep_memory.as_ref().and_then(|m| m.reserve(n));
            let memory_dir = reservation.as_ref().map(|r| r.memory.dir.clone());
            let r = run_fcvm(&cfg.fcvm, &PotatoCfg::sleep_args(n, vm.pid, memory_dir.as_deref())).await;
            match (reservation, &sleep_memory) {
                (Some(reservation), _) => reservation.finish(),
                (None, Some(memory)) if r.is_ok() => memory.left(n),
                _ => {}
            }
            let created = create.elapsed();
            // --stop has stopped the VM's fcvm (also on failure); reap it
            let reap = Instant::now();
            let _ = tokio::time::timeout(REAP_DEADLINE, vm.exited()).await;
            let reaped = reap.elapsed();
            drop(vm);
            let steps = SleepSteps {
                ask_ms: ms(asked),
                create_ms: ms(created),
                reap_ms: ms(reaped),
            };
            let total = ms(started.elapsed());
            let _ = done.send(Done::Slept(
                n,
                match r {
                    Ok(log) => Ok(steps.into_stages(sleep_stages(&log))),
                    Err(e) => Err(e.context(steps.describe())),
                },
                total,
            ));
        });
    }

    fn wake(&self, n: u32, round: Option<u64>, done: mpsc::UnboundedSender<Done>) {
        let cfg = self.cfg.clone();
        self.spawn(async move {
            let started = Instant::now();
            let r = async {
                let vm = Vm::spawn_args(
                    &cfg.fcvm,
                    &cfg.restore_args(&PotatoCfg::tag(n), n)?,
                    &PotatoCfg::tag(n),
                )?;
                let addr = cfg.prefix.player_address(n)?;
                until_listening(addr, WAKE_DEADLINE).await?;
                // it catches only once told it woke: a report connection can fail for other
                // reasons, and a probe answers before the gate's last half opens
                let answer = ask(addr, &Inbound::Wake { round }, Duration::from_secs(5)).await?;
                if let Err(e) = fleet_potato::Reply::ok(&answer) {
                    bail!("player {n} refused its wake: {e}");
                }
                let listening = Instant::now();
                let mut stages = BTreeMap::new();
                if let Some(r) = vm.restored() {
                    stages.insert("restore_ms".to_string(), ms(r.at.duration_since(started)));
                    if let Some(a) = r.ack_ms {
                        stages.insert("ack_ms".to_string(), a as f64);
                    }
                }
                stages.insert(
                    "listen_ms".to_string(),
                    ms(listening.duration_since(started)),
                );
                Ok((Some(vm), stages))
            }
            .await;
            let _ = done.send(Done::Woke(n, r, ms(started.elapsed())));
        });
    }

    fn kick(&self, o: PotatoRound, done: mpsc::UnboundedSender<Done>) {
        let cfg = self.cfg.clone();
        self.spawn(async move {
            let r = async {
                let ring = o
                    .ring
                    .iter()
                    .map(|n| Ok(format!("[{}]:{BALL_PORT}", cfg.prefix.player_address(*n)?)))
                    .collect::<Result<Vec<_>>>()?;
                let ball = fleet_potato::Ball {
                    round: o.round,
                    seq: 0,
                    ring,
                    hold_ms: o.pace_ms.map_or(o.hold_ms, |p| o.hold_ms.min(p)),
                    shrink_permille: o.shrink_permille,
                    min_hold_ms: o.min_hold_ms,
                    flight_ms: o.flight_ms,
                    trail: Vec::new(),
                };
                let first = cfg.prefix.player_address(o.ring[0])?;
                let answer = ask(first, &Inbound::Ball(ball), Duration::from_secs(5)).await?;
                if let Err(e) = fleet_potato::Reply::ok(&answer) {
                    bail!("the first player refused the ball: {e}");
                }
                Ok(())
            }
            .await;
            let _ = done.send(Done::Kicked(o.round, r));
        });
    }

    fn pace(&self, n: u32, round: u64, hold_ms: u64) {
        let Ok(addr) = self.cfg.prefix.player_address(n) else {
            return;
        };
        self.spawn(async move {
            if let Err(e) = ask(
                addr,
                &Inbound::Pace { round, hold_ms },
                Duration::from_secs(2),
            )
            .await
            {
                tracing::debug!(error = %format!("{e:#}"), "a pace did not reach a player");
            }
        });
    }
}

struct Driver {
    cfg: Arc<PotatoCfg>,
    ops: Arc<dyn Ops>,
    out: mpsc::UnboundedSender<AgentMsg>,
    done: mpsc::UnboundedSender<Done>,
    players: Vec<Player>,
    gold_ready: bool,
    /// The latest wakes' durations, ms.
    wakes: VecDeque<f64>,
    playing: Option<Playing>,
    /// A dropped ball's catcher, its round and seq, and when the drop was heard, until it
    /// accepts again. Kept apart from the round, which the coordinator ends at the drop.
    late: Option<(u32, u64, u64, Instant)>,
    /// The last round that ended: a wake or boot that finishes after it puts its player to
    /// sleep with that round's `stop`, since it may have caught a ball of it meanwhile.
    ended: Option<u64>,
}

impl Driver {
    fn emit(&self, player: u32, kind: PotatoKind) {
        let _ = self.out.send(AgentMsg::Potato(PotatoEvent {
            at: unix_ms(),
            player,
            kind,
        }));
    }

    fn set(&mut self, n: u32, phase: Phase) {
        let p = &mut self.players[n as usize];
        p.phase = phase;
        p.since = Instant::now();
    }

    fn player_of(&self, addr: &str) -> Option<u32> {
        let ip: Ipv6Addr = addr
            .rsplit_once(':')
            .map(|(h, _)| h.trim_start_matches('[').trim_end_matches(']'))?
            .parse()
            .ok()?;
        (0..self.cfg.players).find(|n| self.cfg.prefix.player_address(*n).ok() == Some(ip))
    }

    fn make_gold(&mut self) {
        self.ops.make_gold(self.done.clone());
    }

    fn order(&mut self, order: Option<PotatoRound>) {
        let same =
            matches!((&self.playing, &order), (Some(p), Some(o)) if p.order.round == o.round);
        if same {
            // the same round: only the pace can change
            let (Some(p), Some(o)) = (self.playing.as_mut(), order) else {
                return;
            };
            if p.order.pace_ms != o.pace_ms {
                p.order.pace_ms = o.pace_ms;
                if let Some(hold) = o.pace_ms {
                    self.send_pace(o.round, hold);
                }
            }
            return;
        }
        // A new round, or none: the old one ends. Every awake player sleeps, so a ball still
        // in the air reaches nothing that accepts it.
        if let Some(mut p) = self.playing.take() {
            p.over = true;
            self.ended = Some(p.order.round);
            tracing::info!(round = p.order.round, "potato round ended");
            for n in 0..self.cfg.players {
                if self.players[n as usize].phase == Phase::Awake {
                    // one may hold the ball: its throw must not wait in the snapshot
                    self.sleep_after(n, Some(p.order.round));
                }
            }
        }
        let Some(order) = order else {
            return;
        };
        if let Some(bad) = order.ring.iter().find(|n| **n >= self.cfg.players) {
            tracing::warn!(
                player = bad,
                players = self.cfg.players,
                "a round names a player this host does not run; ignored"
            );
            return;
        }
        tracing::info!(
            round = order.round,
            players = order.ring.len(),
            hold_ms = order.hold_ms,
            "potato round"
        );
        self.playing = Some(Playing {
            order,
            seq: 0,
            caught: false,
            kicked: false,
            over: false,
        });
        self.advance();
    }

    /// Bring the round forward: boot missing players, wake those within the lead, and
    /// throw the first ball once the first player is awake.
    fn advance(&mut self) {
        let Some(p) = &self.playing else { return };
        if p.over || !self.gold_ready {
            return;
        }
        let ring = p.order.ring.clone();
        for n in &ring {
            if matches!(
                self.players[*n as usize].phase,
                Phase::Missing | Phase::Failed
            ) {
                self.boot(*n);
            }
        }
        self.schedule_wakes();
        let p = self.playing.as_ref().unwrap();
        let first = p.order.ring[0];
        if !p.kicked
            && ring
                .iter()
                .all(|n| !matches!(self.players[*n as usize].phase, Phase::Booting))
            && self.players[first as usize].phase == Phase::Awake
        {
            self.kick();
        }
    }

    /// The lead: how many catches ahead a sleeping player is woken.
    fn lead(&self, hold_ms: u64) -> usize {
        let slow = self
            .wakes
            .iter()
            .cloned()
            .reduce(f64::max)
            .unwrap_or(FIRST_WAKE_MS);
        ((slow + LEAD_MARGIN_MS) / hold_ms.max(1) as f64).ceil() as usize + 1
    }

    /// The players within the lead of the ball: the next `lead` catchers.
    fn within_lead(&self) -> Vec<u32> {
        let Some(p) = &self.playing else {
            return Vec::new();
        };
        if p.over {
            return Vec::new();
        }
        let n = p.order.ring.len();
        let lead = self.lead(p.hold_ms()).min(n.saturating_sub(1)).max(1);
        // until the first catch the next catcher is ring[0] itself
        let from = if p.caught { p.seq + 1 } else { 0 };
        (0..lead as u64)
            .map(|j| p.order.ring[((from + j) % n as u64) as usize])
            .collect()
    }

    /// Whether the round being played needs player `n` awake: it holds the ball (caught it
    /// last and has not thrown), or the ball reaches it within the lead.
    fn needed(&self, n: u32) -> bool {
        let Some(p) = self.playing.as_ref().filter(|p| !p.over) else {
            return false;
        };
        let holds = p.caught && p.order.ring[(p.seq % p.order.ring.len() as u64) as usize] == n;
        holds || self.within_lead().contains(&n)
    }

    fn schedule_wakes(&mut self) {
        for k in self.within_lead() {
            if self.players[k as usize].phase == Phase::Asleep {
                self.wake(k);
            }
        }
    }

    fn boot(&mut self, n: u32) {
        self.set(n, Phase::Booting);
        let colours = self
            .playing
            .as_ref()
            .map(|p| p.order.colours)
            .unwrap_or(fleet_potato::DEFAULT_COLOURS);
        self.ops.boot(n, colours, self.done.clone());
    }

    fn sleep(&mut self, n: u32) {
        self.sleep_after(n, None);
    }

    /// Sleep `n`, telling it first that round `stop` is over.
    fn sleep_after(&mut self, n: u32, stop: Option<u64>) {
        let vm = self.players[n as usize].vm.take();
        self.set(n, Phase::Sleeping);
        self.ops.sleep(n, vm, stop, self.done.clone());
    }

    fn wake(&mut self, n: u32) {
        self.set(n, Phase::Waking);
        self.emit(n, PotatoKind::Waking {});
        let round = self.playing.as_ref().map(|p| p.order.round);
        self.ops.wake(n, round, self.done.clone());
    }

    fn kick(&mut self) {
        let Some(p) = self.playing.as_mut() else {
            return;
        };
        p.kicked = true;
        self.ops.kick(p.order.clone(), self.done.clone());
    }

    fn send_pace(&self, round: u64, hold_ms: u64) {
        let Some(p) = &self.playing else { return };
        for n in &p.order.ring {
            if self.players[*n as usize].phase == Phase::Awake {
                self.ops.pace(*n, round, hold_ms);
            }
        }
    }

    fn heard(&mut self, h: Heard) {
        let at = h.at;
        let send = |me: &Self, player: u32, kind| {
            let _ = me
                .out
                .send(AgentMsg::Potato(PotatoEvent { at, player, kind }));
        };
        match h.report {
            Report::Hello {
                player,
                colours,
                proof: p,
                connections,
                last_round,
                last_seq,
            } => send(
                self,
                player,
                PotatoKind::Hello {
                    proof: proof(&p),
                    colours,
                    connections,
                    last_round,
                    last_seq,
                },
            ),
            Report::Caught {
                player,
                round,
                seq,
                proof: p,
            } => {
                send(
                    self,
                    player,
                    PotatoKind::Caught {
                        round,
                        seq,
                        proof: proof(&p),
                    },
                );
                if let Some(pl) = self.playing.as_mut().filter(|pl| pl.order.round == round) {
                    pl.seq = if pl.caught { pl.seq.max(seq) } else { seq };
                    pl.caught = true;
                    self.schedule_wakes();
                }
            }
            Report::Threw {
                player,
                round,
                seq,
                to,
                attempts,
                wait_ms,
                send_ms,
            } => {
                let to = self.player_of(&to).unwrap_or(u32::MAX);
                send(
                    self,
                    player,
                    PotatoKind::Threw {
                        round,
                        seq,
                        to,
                        attempts,
                        wait_ms,
                        send_ms,
                    },
                );
                // one sleep per life, however many throws a player reports
                match self.players.get(player as usize).map(|p| p.phase) {
                    Some(Phase::Awake) => self.sleep(player),
                    Some(Phase::Waking | Phase::Booting) => {
                        self.players[player as usize].sleep_after_wake = true;
                    }
                    _ => {}
                }
            }
            Report::Dropped {
                player,
                round,
                seq,
                to,
                attempts,
                reason,
            } => {
                let to = self.player_of(&to).unwrap_or(u32::MAX);
                let (phase, since) = self
                    .players
                    .get(to as usize)
                    .map(|p| (p.phase.name(), ms(p.since.elapsed())))
                    .unwrap_or(("unknown", 0.0));
                send(
                    self,
                    player,
                    PotatoKind::Dropped {
                        round,
                        seq,
                        to,
                        attempts,
                        reason,
                        catcher_phase: phase.to_string(),
                        catcher_phase_ms: since,
                    },
                );
                let current = self
                    .playing
                    .as_ref()
                    .is_some_and(|pl| pl.order.round == round && !pl.over);
                if !current {
                    // a ball of a round that has ended (a throw it abandoned): its thrower
                    // goes to sleep unless the current round needs it awake. The report comes
                    // when the abandoned hold ends, which can be after the player's next wake,
                    // by when it may hold the current ball or be about to catch it.
                    if self.players.get(player as usize).map(|p| p.phase) == Some(Phase::Awake)
                        && !self.needed(player)
                    {
                        self.sleep(player);
                    }
                    return;
                }
                if let Some(pl) = self.playing.as_mut() {
                    pl.over = true;
                }
                self.late = Some((to, round, seq, Instant::now()));
                // the ball is down: nobody needs to be awake
                for n in 0..self.cfg.players {
                    if self.players[n as usize].phase == Phase::Awake {
                        self.sleep(n);
                    }
                }
            }
            Report::Probe { .. } => {}
        }
    }

    fn done(&mut self, x: Done) {
        match x {
            Done::Gold(Ok(())) => {
                tracing::info!("potato: the player snapshot is ready");
                self.gold_ready = true;
                self.advance();
            }
            Done::Gold(Err(e)) => {
                tracing::error!(error = %format!("{e:#}"), "potato: making the player snapshot failed; retrying in 10 s");
                let done = self.done.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    let _ = done.send(Done::RetryGold);
                });
            }
            Done::RetryGold => self.make_gold(),
            Done::Booted(n, Ok(vm)) => {
                self.players[n as usize].vm = vm;
                self.set(n, Phase::Awake);
                // a new player the ball does not reach soon sleeps until it does, so a large
                // ring does not start with every player awake
                // and one booted for a round that has ended, or that is not in this one, sleeps
                let later = !self.within_lead().contains(&n);
                if std::mem::take(&mut self.players[n as usize].sleep_after_wake) || later {
                    self.sleep_after(n, self.ended);
                    self.advance();
                    return;
                }
                self.advance();
            }
            Done::Booted(n, Err(e)) => {
                self.set(n, Phase::Failed);
                self.emit(
                    n,
                    PotatoKind::Failed {
                        stage: "boot".into(),
                        error: format!("{e:#}"),
                    },
                );
            }
            Done::Slept(n, Ok(mut stages), total) => {
                self.set(n, Phase::Asleep);
                let written = stages
                    .remove("written_bytes")
                    .map(|b| (b / 1048576.0 * 10.0).round() / 10.0);
                self.emit(
                    n,
                    PotatoKind::Slept {
                        sleep_ms: total,
                        stages,
                        written_mib: written,
                    },
                );
                self.schedule_wakes();
            }
            Done::Slept(n, Err(e), _) => {
                // its snapshot may be stale or half written: it starts again from the gold
                self.set(n, Phase::Missing);
                self.emit(
                    n,
                    PotatoKind::Failed {
                        stage: "sleep".into(),
                        error: format!("{e:#}"),
                    },
                );
            }
            Done::Woke(n, Ok((vm, stages)), total) => {
                self.players[n as usize].vm = vm;
                self.set(n, Phase::Awake);
                self.wakes.push_back(total);
                while self.wakes.len() > RECENT_WAKES {
                    self.wakes.pop_front();
                }
                self.emit(
                    n,
                    PotatoKind::Woke {
                        wake_ms: total,
                        stages,
                    },
                );
                let late = match self.late {
                    Some((k, round, seq, at)) if k == n => {
                        self.late = None;
                        Some((round, seq, at))
                    }
                    _ => None,
                };
                if let Some((round, seq, at)) = late {
                    self.emit(
                        n,
                        PotatoKind::Listening {
                            round,
                            seq,
                            after_ms: ms(at.elapsed()),
                        },
                    );
                }
                if std::mem::take(&mut self.players[n as usize].sleep_after_wake) {
                    self.sleep(n);
                    return;
                }
                let over = self.playing.as_ref().is_none_or(|p| p.over);
                let in_ring = self
                    .playing
                    .as_ref()
                    .is_some_and(|p| p.order.ring.contains(&n));
                if over || !in_ring {
                    // woken for a round that has ended: back to sleep, told the round is over
                    // first, since it may have caught a ball of it after its wake
                    self.sleep_after(n, self.ended);
                } else {
                    if let Some(hold) = self.playing.as_ref().and_then(|p| p.order.pace_ms) {
                        let round = self.playing.as_ref().unwrap().order.round;
                        self.send_pace(round, hold);
                    }
                    self.advance();
                }
            }
            Done::Woke(n, Err(e), _) => {
                self.set(n, Phase::Missing);
                self.emit(
                    n,
                    PotatoKind::Failed {
                        stage: "wake".into(),
                        error: format!("{e:#}"),
                    },
                );
            }
            Done::Kicked(round, Err(e)) => {
                tracing::warn!(round, error = %format!("{e:#}"), "potato: the first throw failed");
                if let Some(first) = self.playing.as_ref().map(|p| p.order.ring[0]) {
                    self.emit(
                        first,
                        PotatoKind::Failed {
                            stage: "kickoff".into(),
                            error: format!("{e:#}"),
                        },
                    );
                }
            }
            Done::Kicked(_, Ok(())) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records what the driver asks for; nothing runs.
    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<String>>);

    impl Recorder {
        fn take(&self) -> Vec<String> {
            std::mem::take(&mut self.0.lock().unwrap())
        }
        fn push(&self, s: String) {
            self.0.lock().unwrap().push(s);
        }
    }

    impl Ops for Arc<Recorder> {
        fn make_gold(&self, _: mpsc::UnboundedSender<Done>) {
            self.push("gold".into());
        }
        fn boot(&self, n: u32, _: u32, _: mpsc::UnboundedSender<Done>) {
            self.push(format!("boot {n}"));
        }
        fn sleep(&self, n: u32, _: Option<Vm>, stop: Option<u64>, _: mpsc::UnboundedSender<Done>) {
            match stop {
                Some(r) => self.push(format!("stop {r} sleep {n}")),
                None => self.push(format!("sleep {n}")),
            }
        }
        fn wake(&self, n: u32, _: Option<u64>, _: mpsc::UnboundedSender<Done>) {
            self.push(format!("wake {n}"));
        }
        fn kick(&self, o: PotatoRound, _: mpsc::UnboundedSender<Done>) {
            self.push(format!("kick {}", o.round));
        }
        fn pace(&self, n: u32, _: u64, hold_ms: u64) {
            self.push(format!("pace {n} {hold_ms}"));
        }
        fn shutdown(&self) {
            self.push("shutdown".into());
        }
    }

    fn driver(players: u32) -> (Driver, Arc<Recorder>, mpsc::UnboundedReceiver<AgentMsg>) {
        let rec = Arc::new(Recorder::default());
        let (out, out_rx) = mpsc::unbounded_channel();
        let (done, _done_rx) = mpsc::unbounded_channel();
        std::mem::forget(_done_rx);
        let d = Driver {
            cfg: Arc::new(PotatoCfg {
                fcvm: "fcvm".into(),
                image: "img".into(),
                players,
                prefix: Prefix::parse("2001:db8::/80").unwrap(),
                mem_mib: 256,
                sleep_memory_dir: None,
                sleep_memory_cap_mib: 0,
            }),
            ops: Arc::new(rec.clone()),
            out,
            done,
            players: (0..players)
                .map(|_| Player {
                    phase: Phase::Missing,
                    since: Instant::now(),
                    vm: None,
                    sleep_after_wake: false,
                })
                .collect(),
            gold_ready: false,
            wakes: VecDeque::new(),
            playing: None,
            late: None,
            ended: None,
        };
        (d, rec, out_rx)
    }

    fn round(id: u64, ring: Vec<u32>, hold_ms: u64) -> PotatoRound {
        PotatoRound {
            round: id,
            ring,
            hold_ms,
            shrink_permille: 0,
            min_hold_ms: 2,
            flight_ms: 100,
            colours: 12,
            pace_ms: None,
        }
    }

    fn addr(n: u32) -> String {
        format!("[2001:db8::{:x}]:7070", 0x1000 + n)
    }

    fn heard(report: Report) -> Heard {
        Heard { report, at: 1 }
    }

    fn proof() -> fleet_potato::Proof {
        fleet_potato::Proof {
            colour: 1,
            counter: 1,
            chain: "00".repeat(32),
        }
    }

    fn caught(player: u32, round: u64, seq: u64) -> Heard {
        heard(Report::Caught {
            player,
            round,
            seq,
            proof: proof(),
        })
    }

    fn threw(player: u32, round: u64, seq: u64, to: u32) -> Heard {
        heard(Report::Threw {
            player,
            round,
            seq,
            to: addr(to),
            attempts: 1,
            wait_ms: 0.0,
            send_ms: 1.0,
        })
    }

    /// A driver whose players are all asleep, with wakes of `wake_ms` seen.
    fn asleep(
        players: u32,
        wake_ms: f64,
    ) -> (Driver, Arc<Recorder>, mpsc::UnboundedReceiver<AgentMsg>) {
        let (mut d, rec, out) = driver(players);
        d.gold_ready = true;
        for p in &mut d.players {
            p.phase = Phase::Asleep;
        }
        d.wakes.push_back(wake_ms);
        rec.take();
        (d, rec, out)
    }

    #[test]
    fn a_round_boots_its_players_from_the_gold_and_throws_once_the_first_is_awake() {
        let (mut d, rec, _out) = driver(3);
        d.order(Some(round(1, vec![0, 1], 1000)));
        assert_eq!(rec.take(), Vec::<String>::new(), "nothing before the gold");
        d.done(Done::Gold(Ok(())));
        assert_eq!(
            rec.take(),
            vec!["boot 0", "boot 1"],
            "only the ring's players"
        );
        d.done(Done::Booted(1, Ok(None)));
        assert_eq!(
            rec.take(),
            vec!["sleep 1"],
            "player 1 is beyond the lead of 1 until the first catch; player 0 is still booting"
        );
        d.done(Done::Booted(0, Ok(None)));
        assert_eq!(rec.take(), vec!["kick 1"]);
        d.done(Done::Booted(2, Ok(None)));
        assert_eq!(
            rec.take(),
            vec!["sleep 2"],
            "the first ball is thrown once, and a player booted outside the ring sleeps"
        );
    }

    #[test]
    fn a_player_sleeps_once_after_it_throws() {
        let (mut d, rec, _out) = asleep(2, 300.0);
        d.order(Some(round(1, vec![0, 1], 1000)));
        rec.take();
        for n in 0..2 {
            d.players[n].phase = Phase::Awake;
        }
        d.heard(caught(0, 1, 0));
        d.heard(threw(0, 1, 1, 1));
        d.heard(threw(0, 1, 1, 1));
        assert_eq!(rec.take(), vec!["sleep 0"]);
    }

    #[test]
    fn a_sleeping_player_wakes_when_the_ball_is_within_its_lead() {
        // 300 ms wakes plus the 150 ms margin at a 200 ms hold: a lead of 4 catches
        let (mut d, rec, _out) = asleep(8, 300.0);
        d.order(Some(round(1, (0..8).collect(), 200)));
        assert_eq!(rec.take(), vec!["wake 0", "wake 1", "wake 2", "wake 3"]);
        for n in 0..4 {
            d.done(Done::Woke(n, Ok((None, BTreeMap::new())), 300.0));
        }
        assert_eq!(rec.take(), vec!["kick 1"]);
        d.heard(caught(0, 1, 0));
        assert_eq!(
            rec.take(),
            vec!["wake 4"],
            "the catch moves the lead on by one"
        );
        // a finished sleep looks at the lead again
        d.heard(threw(0, 1, 1, 1));
        assert_eq!(rec.take(), vec!["sleep 0"]);
        d.done(Done::Slept(0, Ok(BTreeMap::new()), 300.0));
        assert!(
            rec.take().is_empty(),
            "player 0 catches again in 8 catches, beyond the lead"
        );
    }

    /// A sleep whose memory is gone (its memory directory is a tmpfs, cleared by a reboot or by
    /// hand) cannot wake: the agent reports the wake failed and forgets the player, and the next
    /// round boots it again from the gold rather than trying the lost snapshot once more.
    #[test]
    fn a_player_whose_memory_is_gone_is_reported_and_booted_again_next_round() {
        let (mut d, rec, mut out) = asleep(2, 300.0);
        d.order(Some(round(1, vec![0, 1], 1000)));
        assert_eq!(rec.take(), vec!["wake 0"]);
        d.done(Done::Woke(
            0,
            Err(anyhow::anyhow!(
                "the memory file of snapshot potato-0 is gone: /dev/shm/fcvm-potato/fcvm-memory-x.bin"
            )),
            0.0,
        ));
        let failed = std::iter::from_fn(|| out.try_recv().ok()).find_map(|m| match m {
            AgentMsg::Potato(PotatoEvent {
                player: 0,
                kind: PotatoKind::Failed { stage, error },
                ..
            }) => Some((stage, error)),
            _ => None,
        });
        let (stage, error) = failed.expect("the failed wake is reported");
        assert_eq!(stage, "wake");
        assert!(error.contains("is gone"), "{error}");
        assert_eq!(d.players[0].phase, Phase::Missing);
        // the coordinator drops the ball and starts the next round
        d.order(None);
        rec.take();
        d.order(Some(round(2, vec![0, 1], 1000)));
        let next = rec.take();
        assert!(next.contains(&"boot 0".to_string()), "{next:?}");
        assert!(!next.contains(&"wake 0".to_string()), "{next:?}");
    }

    #[test]
    fn a_throw_reported_while_waking_sleeps_the_player_once_its_wake_is_done() {
        let (mut d, rec, _out) = asleep(2, 300.0);
        d.order(Some(round(1, vec![0, 1], 1000)));
        assert_eq!(rec.take(), vec!["wake 0"], "a ring of 2 has a lead of 1");
        d.done(Done::Woke(0, Ok((None, BTreeMap::new())), 300.0));
        assert_eq!(rec.take(), vec!["kick 1"]);
        d.heard(caught(0, 1, 0));
        assert_eq!(rec.take(), vec!["wake 1"]);
        // player 1 accepts before its wake is done, catches, holds and throws
        d.heard(threw(1, 1, 1, 0));
        assert!(rec.take().is_empty(), "no sleep while the wake runs");
        d.done(Done::Woke(1, Ok((None, BTreeMap::new())), 300.0));
        assert_eq!(rec.take(), vec!["sleep 1"]);
    }

    #[test]
    fn a_round_that_ends_puts_every_awake_player_to_sleep_and_wakes_no_one() {
        let (mut d, rec, _out) = asleep(4, 300.0);
        d.order(Some(round(1, (0..4).collect(), 1000)));
        rec.take();
        d.done(Done::Woke(0, Ok((None, BTreeMap::new())), 300.0));
        d.done(Done::Woke(1, Ok((None, BTreeMap::new())), 300.0));
        rec.take();
        d.order(None);
        let mut slept = rec.take();
        slept.sort();
        assert_eq!(
            slept,
            vec!["stop 1 sleep 0", "stop 1 sleep 1"],
            "each is told the round is over first, so no snapshot holds a throw of it"
        );
        d.done(Done::Woke(2, Ok((None, BTreeMap::new())), 300.0));
        assert_eq!(
            rec.take(),
            vec!["stop 1 sleep 2"],
            "a wake that finishes after the round goes back to sleep, told the round is over: it may have caught a ball of it"
        );
    }

    #[test]
    fn a_drop_names_what_the_agent_was_doing_with_the_catcher() {
        let (mut d, rec, mut out) = asleep(2, 300.0);
        d.order(Some(round(4, vec![0, 1], 1000)));
        d.done(Done::Woke(0, Ok((None, BTreeMap::new())), 300.0));
        d.heard(caught(0, 4, 0));
        assert_eq!(rec.take(), vec!["wake 0", "kick 4", "wake 1"]);
        d.heard(heard(Report::Dropped {
            player: 0,
            round: 4,
            seq: 1,
            to: addr(1),
            attempts: 3,
            reason: "connection timed out".into(),
        }));
        let mut kinds = Vec::new();
        while let Ok(AgentMsg::Potato(e)) = out.try_recv() {
            kinds.push(e.kind);
        }
        let dropped = kinds
            .iter()
            .find_map(|k| match k {
                PotatoKind::Dropped {
                    to, catcher_phase, ..
                } => Some((*to, catcher_phase.clone())),
                _ => None,
            })
            .expect("the drop is reported");
        assert_eq!(dropped, (1, "waking".to_string()));
        assert_eq!(
            rec.take(),
            vec!["sleep 0"],
            "with the ball down nobody stays awake"
        );
        // the coordinator ends the round before the catcher's wake finishes
        d.order(None);
        d.done(Done::Woke(1, Ok((None, BTreeMap::new())), 400.0));
        let listening = std::iter::from_fn(|| out.try_recv().ok()).any(|m| {
            matches!(
                m,
                AgentMsg::Potato(PotatoEvent {
                    kind: PotatoKind::Listening {
                        round: 4,
                        seq: 1,
                        ..
                    },
                    ..
                })
            )
        });
        assert!(
            listening,
            "the catcher's late wake is reported against the drop"
        );
    }

    #[test]
    fn sleep_stages_reads_the_boundary_and_the_stop() {
        let log = "2026-11-05T04:00:00Z  INFO fcvm::commands::common: snapshot boundary timings snapshot=potato-3 prepare_ms=20.1 pause_ms=0.3 save_ms=61.5 disk_ms=40.2 resume_ms=0\n\
                   2026-11-05T04:00:00Z  INFO fcvm::commands::common: diff written in place, building atomic update snapshot=potato-3 written_bytes=20971520 ranges=2\n\
                   2026-11-05T04:00:00Z  INFO fcvm::commands::common: snapshot created successfully snapshot=potato-3 snapshot_type=\"Diff\" disk=/s/potato-3/disk/disk.raw stage_ms=412.5 publish_ms=7218.3\n\
                   2026-11-05T04:00:00Z  INFO fcvm::commands::snapshot: stopped the VM after its snapshot pid=7 stop_ms=96.4 killed=false\n";
        let s = sleep_stages(log);
        assert_eq!(s["stage_ms"], 412.5);
        assert_eq!(s["publish_ms"], 7218.3);
        assert_eq!(s["prepare_ms"], 20.1);
        assert_eq!(s["save_ms"], 61.5);
        assert_eq!(s["disk_ms"], 40.2);
        assert_eq!(s["stop_ms"], 96.4);
        assert_eq!(s["written_bytes"], 20971520.0);
        assert!(!s.contains_key("resume_ms"), "a stopped VM is not resumed");
    }

    #[test]
    fn a_sleep_carries_the_agents_own_steps() {
        let steps = SleepSteps {
            ask_ms: 1.04,
            create_ms: 99_120.0,
            reap_ms: 40_000.0,
        };
        let s = steps.into_stages(BTreeMap::from([("stop_ms".to_string(), 96.4)]));
        assert_eq!(s["ask_ms"], 1.0);
        assert_eq!(s["create_ms"], 99_120.0);
        assert_eq!(s["reap_ms"], 40_000.0);
        assert_eq!(s["stop_ms"], 96.4);
        assert_eq!(
            steps.describe(),
            "after 1 ms telling it the round is over, 99120 ms in snapshot create and 40000 ms waiting for its fcvm to exit"
        );
    }

    #[test]
    fn the_hold_shrinks_per_lap_and_a_pace_caps_it() {
        let mut p = Playing {
            order: PotatoRound {
                round: 1,
                ring: vec![0, 1, 2, 3],
                hold_ms: 1000,
                shrink_permille: 100,
                min_hold_ms: 50,
                flight_ms: 100,
                colours: 12,
                pace_ms: None,
            },
            seq: 3,
            caught: true,
            kicked: true,
            over: false,
        };
        assert_eq!(p.hold_ms(), 1000);
        p.seq = 4;
        assert_eq!(p.hold_ms(), 900, "the hold the ball carries in lap 1");
        p.order.pace_ms = Some(500);
        assert_eq!(p.hold_ms(), 500);
        p.order.pace_ms = Some(10);
        assert_eq!(p.hold_ms(), 50, "never below the floor");
    }

    #[test]
    fn a_new_player_outside_the_lead_goes_to_sleep_once_booted() {
        // 300 ms wakes plus the margin at a 200 ms hold: a lead of 4 catches
        let (mut d, rec, _out) = driver(8);
        d.wakes.push_back(300.0);
        d.order(Some(round(1, (0..8).collect(), 200)));
        d.done(Done::Gold(Ok(())));
        assert_eq!(rec.take().len(), 8, "every player is booted");
        for n in 0..8 {
            d.done(Done::Booted(n, Ok(None)));
        }
        let mut asked = rec.take();
        asked.sort();
        assert_eq!(
            asked,
            vec!["kick 1", "sleep 4", "sleep 5", "sleep 6", "sleep 7"],
            "the players the ball reaches last sleep until their turn"
        );
    }

    /// A throw a player abandoned when its round ended can be reported after the next round
    /// started: only that thrower sleeps, and the current round goes on.
    #[test]
    fn a_drop_from_an_ended_round_leaves_the_current_one_alone() {
        let (mut d, rec, _out) = asleep(4, 300.0);
        d.order(Some(round(2, (0..4).collect(), 1000)));
        for n in 0..4 {
            d.done(Done::Woke(n, Ok((None, BTreeMap::new())), 300.0));
        }
        rec.take();
        d.heard(heard(Report::Dropped {
            player: 3,
            round: 1,
            seq: 9,
            to: addr(0),
            attempts: 0,
            reason: "the round is over".into(),
        }));
        assert_eq!(rec.take(), vec!["sleep 3"]);
        assert!(!d.playing.as_ref().unwrap().over, "round 2 goes on");
        assert!(d.late.is_none());
    }

    /// A throw abandoned when its round ended is reported when the hold ends, which can be
    /// after the player's next wake, by when it may hold the ball of the round after. That
    /// report must not put the holder to sleep: a sleeping holder throws nothing, and nobody
    /// wakes the holder, so the ball of the current round is lost until its deadline.
    #[test]
    fn a_stale_drop_does_not_sleep_the_holder_of_the_current_ball() {
        let (mut d, rec, _out) = asleep(4, 300.0);
        d.order(Some(round(2, vec![0, 1, 2, 3], 1000)));
        assert_eq!(rec.take(), vec!["wake 0", "wake 1"]);
        d.done(Done::Woke(0, Ok((None, BTreeMap::new())), 300.0));
        assert_eq!(rec.take(), vec!["kick 2"]);
        // player 0 catches the first ball of round 2 and holds it
        d.heard(caught(0, 2, 0));
        rec.take();
        // the hold of a round-1 ball it was paused in ends now: the throw is abandoned
        d.heard(heard(Report::Dropped {
            player: 0,
            round: 1,
            seq: 7,
            to: addr(1),
            attempts: 0,
            reason: "the round is over".into(),
        }));
        assert_eq!(
            rec.take(),
            Vec::<String>::new(),
            "player 0 holds round 2's ball and must stay awake to throw it"
        );
        assert_eq!(d.players[0].phase, Phase::Awake);
    }

    /// A sleep keeps the player's memory in the memory directory when the agent has one, so it
    /// commits no btrfs transaction, and on the store when it has none.
    #[test]
    fn a_sleep_puts_the_memory_in_the_sleep_memory_dir() {
        assert_eq!(
            PotatoCfg::sleep_args(3, 4242, Some(std::path::Path::new("/dev/shm/fcvm-potato"))),
            [
                "snapshot",
                "create",
                "--pid",
                "4242",
                "--tag",
                "potato-3",
                "--stop",
                "--memory-dir",
                "/dev/shm/fcvm-potato",
            ]
        );
        assert_eq!(
            PotatoCfg::sleep_args(3, 4242, None),
            ["snapshot", "create", "--pid", "4242", "--tag", "potato-3", "--stop"]
        );
    }

    /// The sleeps' memory stays under its cap: each player whose memory is held and each sleep
    /// in flight count once. A sleep over the cap keeps its memory on the store; a sleep that
    /// ends gives its room back, a published one holds it for its player (once, however often
    /// it sleeps), and a player re-slept on the store stops holding any.
    #[test]
    fn sleeps_keep_their_memory_under_the_cap() {
        let mib = 1u64 << 20;
        let memory = Arc::new(SleepMemory {
            dir: "/nonexistent".into(),
            each: mib,
            cap: 3 * mib,
            state: Default::default(),
        });
        let held = |m: &SleepMemory| m.state.lock().unwrap().held.len();
        memory.reserve(0).unwrap().finish();
        memory.reserve(1).unwrap().finish();
        assert_eq!(held(&memory), 2);
        // both players awake and sleeping again: one fits beside their two memories
        let a = memory.reserve(0).expect("2 held + 1 fits under 3");
        assert!(
            memory.reserve(1).is_none(),
            "2 held + 2 in flight is over 3"
        );
        a.finish();
        assert_eq!(
            held(&memory),
            2,
            "a player's next sleep replaces its memory"
        );
        let b = memory.reserve(1).expect("room again once the sleep ended");
        drop(b);
        assert_eq!(
            memory.state.lock().unwrap().in_flight,
            0,
            "a dropped sleep gives its room back"
        );
        memory.left(1);
        assert_eq!(
            held(&memory),
            1,
            "a player re-slept on the store holds no memory"
        );

        // the default cap, twice the players' memory, fits every player's memory and the next
        // sleep of each at once: a sleep in flight is counted once, however much it has written
        let memory = Arc::new(SleepMemory {
            dir: "/nonexistent".into(),
            each: mib,
            cap: 4 * mib,
            state: Default::default(),
        });
        memory.reserve(0).unwrap().finish();
        memory.reserve(1).unwrap().finish();
        let a = memory.reserve(0).expect("2 held + 1 in flight");
        let b = memory
            .reserve(1)
            .expect("2 held + 2 in flight fits the default cap of 4");
        a.finish();
        b.finish();
    }

    /// A sleep that reserved room in the memory directory leaves its player counted as holding
    /// a file whatever the create returned: `snapshot create` can publish and then fail on the
    /// VM's state, and a missed file would let the directory outgrow its cap. The sleep task has
    /// no fake fcvm to drive, so its call is pinned by its source.
    #[test]
    fn a_failed_sleep_still_counts_its_memory() {
        let source = include_str!("potato.rs");
        let ops = &source[source.find("impl Ops for FcvmOps {").unwrap()..];
        let sleep = &ops[ops
            .find("    fn sleep(&self, n: u32, vm: Option<Vm>")
            .unwrap()..];
        let sleep = &sleep[..sleep.find("\n    }\n").unwrap()];
        assert!(
            sleep.contains("(Some(reservation), _) => reservation.finish(),"),
            "the sleep must finish its reservation whatever the create returned"
        );
        let memory = Arc::new(SleepMemory {
            dir: "/nonexistent".into(),
            each: 1,
            cap: 2,
            state: Default::default(),
        });
        memory.reserve(0).unwrap().finish();
        assert_eq!(memory.state.lock().unwrap().held.len(), 1);
    }

    /// The agent starts from an empty memory directory: what an earlier agent's sleeps left,
    /// linked or not, is removed, and nothing else in it is.
    #[test]
    fn a_new_agent_empties_its_sleep_memory_directory() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = crate::storage::disk::LINKED_MEMORY_PREFIX;
        std::fs::write(dir.path().join(format!("{prefix}a.bin")), b"m").unwrap();
        std::fs::write(dir.path().join(format!("{prefix}b.bin")), b"m").unwrap();
        std::fs::write(dir.path().join("other"), b"o").unwrap();
        let memory = SleepMemory {
            dir: dir.path().to_path_buf(),
            each: 1,
            cap: 1,
            state: Default::default(),
        };
        memory.clear().unwrap();
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["other"]);
        SleepMemory {
            dir: dir.path().join("missing"),
            ..memory
        }
        .clear()
        .unwrap();
    }

    /// Shutdown aborts the work in flight: a boot still waiting for a slot never starts a VM
    /// once the agent is stopping.
    #[tokio::test]
    async fn a_shutdown_stops_a_boot_waiting_for_its_slot() {
        let ops = FcvmOps {
            cfg: Arc::new(PotatoCfg {
                fcvm: "/nonexistent/fcvm".into(),
                image: "img".into(),
                players: 2,
                prefix: Prefix::parse("2001:db8::/80").unwrap(),
                mem_mib: 256,
                sleep_memory_dir: None,
                sleep_memory_cap_mib: 0,
            }),
            sleep_memory: None,
            boot_slots: Arc::new(Semaphore::new(0)),
            tasks: Default::default(),
        };
        let (done, mut rx) = mpsc::unbounded_channel();
        ops.boot(0, 12, done);
        ops.shutdown();
        ops.boot_slots.add_permits(1);
        // with the task alive, the boot would now try to start fcvm and report its failure
        let heard = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
        assert!(
            !matches!(heard, Ok(Some(Done::Booted(..)))),
            "a boot ran after the shutdown"
        );
    }
}
