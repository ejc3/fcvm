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
use super::ring_cpus::{Cpus, Holder, RingCgroup};
use super::vm::Vm;
use anyhow::{bail, Context, Result};
use fleet_core::potato::{CpuSlot, PotatoEvent, PotatoKind, PotatoProof, PotatoRound, RingStats};
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
/// The lead's margin: a player's wake is asked for the 95th percentile wake plus this long
/// before the ball is due.
const LEAD_MARGIN_MS: f64 = 150.0;
/// The wake time assumed before any wake was seen.
const FIRST_WAKE_MS: f64 = 1500.0;
/// The lead is computed over the wakes of this long. The latest wake is kept beyond it, so
/// a ring that paused starts from its last measurement.
const WAKE_WINDOW: Duration = Duration::from_secs(30);
/// How long an fcvm the agent stops has to exit after SIGTERM before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(30);
/// How long the agent waits, when it stops, for the ring's cgroup to empty.
const EMPTY_DEADLINE: Duration = Duration::from_secs(30);

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
    /// The ring's CPU allotment: at most this many machines are not asleep at once
    /// (`--potato-cpus`, at least [`super::ring_cpus::MIN_CPUS`]).
    pub cpus: u32,
    /// The cpuset the ring's processes run in, `cpus` host CPUs wide. None: the host cannot
    /// confine them, and the allotment only counts machines.
    pub cgroup: Option<Arc<RingCgroup>>,
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
    /// Whether a player in this phase is a machine that is not asleep: it has a VM, or one
    /// is starting or has not yet exited.
    fn holds_cpu(self) -> bool {
        matches!(
            self,
            Phase::Booting | Phase::Awake | Phase::Sleeping | Phase::Waking
        )
    }

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
    /// Asleep, within the lead, and no CPU was free: since when its wake has waited.
    wanted_since: Option<Instant>,
    /// When its wake (or boot) ended and it started catching; none once it goes to sleep.
    ready_at: Option<Instant>,
    /// It caught before the agent heard its wake had ended: when the throw to it began, kept
    /// until the wake's end gives the margin.
    margin_due: Option<Instant>,
    /// The round its wake was started for.
    woken_for: Option<u64>,
}

impl Player {
    fn new() -> Self {
        Player {
            phase: Phase::Missing,
            since: Instant::now(),
            vm: None,
            sleep_after_wake: false,
            wanted_since: None,
            ready_at: None,
            margin_due: None,
            woken_for: None,
        }
    }
}

/// Proof that an operation's fcvm has exited, or that it never started one. The driver gives
/// a player's CPU back only on a message that carries it, so no CPU is counted free while
/// the process that held it is still there.
pub(crate) struct Exited(());

impl Exited {
    /// Stop `vm` and wait for its fcvm to exit: SIGTERM, then SIGKILL after `grace`.
    async fn of(mut vm: Vm, grace: Duration) -> Exited {
        vm.stop(grace).await;
        Exited(())
    }

    /// No fcvm was started, or its exit was awaited where this is called.
    fn none() -> Exited {
        Exited(())
    }
}

/// A player that started catching: its VM, how the start split, and when it was ready.
pub(crate) struct Ready {
    vm: Option<Vm>,
    stages: BTreeMap<String, f64>,
    at: Instant,
}

/// An operation that failed, after its fcvm exited.
pub(crate) struct Failed {
    error: anyhow::Error,
    #[allow(dead_code)] // the proof is its presence
    exited: Exited,
}

/// An operation's end. Every variant that frees a CPU carries an [`Exited`].
pub(crate) enum Done {
    Gold(Result<()>, Exited),
    RetryGold,
    Booted(u32, Result<Ready, Failed>),
    Slept(u32, Result<BTreeMap<String, f64>>, f64, Exited),
    Woke(u32, Result<Ready, Failed>, f64),
    Kicked(u64, Result<()>),
}

/// A player's report as the agent received it.
struct Heard {
    report: Report,
    at: u64,
    /// When the agent read it, on the clock its wakes are timed by.
    read: Instant,
}

/// The round the agent is playing.
#[derive(Clone)]
struct Playing {
    order: PotatoRound,
    /// The latest catch's seq, once there has been one (`caught`).
    seq: u64,
    caught: bool,
    kicked: bool,
    /// The ball dropped (or the round was replaced): no more wakes.
    over: bool,
    /// When the holder of the latest catch starts its throw, and the seq that throw carries:
    /// the catch's read time plus the hold the catcher keeps.
    throw_due: Option<(u64, Instant)>,
}

impl Playing {
    /// The hold the ball carries at `seq`, as the players compute it (in microseconds), in ms.
    fn hold_ms(&self) -> u64 {
        let o = &self.order;
        let laps = self.seq / o.ring.len().max(1) as u64;
        let h = fleet_potato::ms_of(fleet_potato::hold_us_after(
            o.hold_ms,
            o.shrink_permille,
            o.min_hold_ms,
            laps,
        ));
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

/// `later - earlier` in ms, negative when `later` is the earlier one.
fn signed_ms(later: Instant, earlier: Instant) -> f64 {
    match later.checked_duration_since(earlier) {
        Some(d) => ms(d),
        None => -ms(earlier.duration_since(later)),
    }
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
async fn run_fcvm(
    fcvm: &std::path::Path,
    args: &[String],
    cgroup: Option<&RingCgroup>,
) -> Result<String> {
    let mut cmd = tokio::process::Command::new(fcvm);
    cmd.args(args)
        .env("RUST_LOG", "fcvm=info")
        .stdin(std::process::Stdio::null());
    if let Some(cgroup) = cgroup {
        cgroup.confine(cmd.as_std_mut());
    }
    let out = cmd
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
        players: (0..cfg.players).map(|_| Player::new()).collect(),
        cpus: Cpus::new(cfg.cpus),
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
    // The ring's cgroup is inside the agent's unit: what is left in it when the agent exits,
    // systemd kills with SIGKILL, and an fcvm killed that way leaves its VM's network behind.
    if let Some(cgroup) = &cfg.cgroup {
        if !cgroup.emptied(EMPTY_DEADLINE).await {
            tracing::warn!(
                "potato: processes are left in the ring's cgroup {EMPTY_DEADLINE:?} after its VMs were stopped"
            );
        }
    }
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
                        let read = Instant::now();
                        // every report as read, so a report a player sent and the agent never
                        // read can be told from one the coordinator read late
                        tracing::debug!(target: "fcvm::fleet::potato_reports", at, report = %line, "potato report read");
                        if tx.send(Heard { report, at, read }).is_err() {
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

/// Start an fcvm that runs one of the ring's VMs, in the ring's cgroup when the host has one.
fn start_vm(cfg: &PotatoCfg, args: &[String], slot: &str) -> Result<Vm> {
    Vm::spawn_args_in(&cfg.fcvm, args, slot, |cmd| {
        if let Some(cgroup) = &cfg.cgroup {
            cgroup.confine(cmd);
        }
    })
}

/// A failure before any fcvm was started.
fn unstarted(error: anyhow::Error) -> Failed {
    Failed {
        error,
        exited: Exited::none(),
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
            let started = cfg.gold_args().and_then(|args| {
                let addr: Ipv6Addr =
                    args[args.iter().position(|a| a == "--ipv6-address").unwrap() + 1].parse()?;
                Ok((start_vm(&cfg, &args, GOLD)?, addr))
            });
            let (gold, addr) = match started {
                Ok(x) => x,
                Err(e) => {
                    let _ = done.send(Done::Gold(Err(e), Exited::none()));
                    return;
                }
            };
            let created = async {
                until_listening(addr, BOOT_DEADLINE).await?;
                run_fcvm(
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
                    cfg.cgroup.as_deref(),
                )
                .await
                .map(|_| ())
            }
            .await;
            let exited = Exited::of(gold, STOP_GRACE).await;
            let _ = done.send(Done::Gold(created, exited));
        });
    }

    fn boot(&self, n: u32, colours: u32, done: mpsc::UnboundedSender<Done>) {
        let cfg = self.cfg.clone();
        let slots = self.boot_slots.clone();
        self.spawn(async move {
            let r = async {
                let _slot = slots.acquire().await.map_err(|e| unstarted(e.into()))?;
                let addr = cfg.prefix.player_address(n).map_err(unstarted)?;
                let args = cfg.restore_args(GOLD, n).map_err(unstarted)?;
                let vm = start_vm(&cfg, &args, &PotatoCfg::tag(n)).map_err(unstarted)?;
                let assigned = async {
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
                    Ok(Instant::now())
                }
                .await;
                match assigned {
                    Ok(at) => Ok(Ready {
                        vm: Some(vm),
                        stages: BTreeMap::new(),
                        at,
                    }),
                    // the CPU it held is free only once its fcvm is gone
                    Err(error) => Err(Failed {
                        error,
                        exited: Exited::of(vm, STOP_GRACE).await,
                    }),
                }
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
                    Exited::none(),
                ));
                return;
            };
            let create = Instant::now();
            let reservation = sleep_memory.as_ref().and_then(|m| m.reserve(n));
            let memory_dir = reservation.as_ref().map(|r| r.memory.dir.clone());
            let r = run_fcvm(
                &cfg.fcvm,
                &PotatoCfg::sleep_args(n, vm.pid, memory_dir.as_deref()),
                cfg.cgroup.as_deref(),
            )
            .await;
            match (reservation, &sleep_memory) {
                (Some(reservation), _) => reservation.finish(),
                (None, Some(memory)) if r.is_ok() => memory.left(n),
                _ => {}
            }
            let created = create.elapsed();
            // --stop has stopped the VM's fcvm (also on failure); reap it
            let reap = Instant::now();
            if tokio::time::timeout(REAP_DEADLINE, vm.exited())
                .await
                .is_err()
            {
                tracing::warn!(player = n, "potato: a slept player's fcvm had not exited after {REAP_DEADLINE:?}; stopping it");
            }
            // exited already, or stopped now: the player's CPU is free only once it is gone
            let exited = Exited::of(vm, STOP_GRACE).await;
            let reaped = reap.elapsed();
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
                exited,
            ));
        });
    }

    fn wake(&self, n: u32, round: Option<u64>, done: mpsc::UnboundedSender<Done>) {
        let cfg = self.cfg.clone();
        self.spawn(async move {
            let started = Instant::now();
            let r = async {
                let args = cfg.restore_args(&PotatoCfg::tag(n), n).map_err(unstarted)?;
                let addr = cfg.prefix.player_address(n).map_err(unstarted)?;
                let vm = start_vm(&cfg, &args, &PotatoCfg::tag(n)).map_err(unstarted)?;
                let listening = async {
                    until_listening(addr, WAKE_DEADLINE).await?;
                    // it catches only once told it woke: a report connection can fail for other
                    // reasons, and a probe answers before the gate's last half opens
                    let answer =
                        ask(addr, &Inbound::Wake { round }, Duration::from_secs(5)).await?;
                    if let Err(e) = fleet_potato::Reply::ok(&answer) {
                        bail!("player {n} refused its wake: {e}");
                    }
                    Ok(Instant::now())
                }
                .await;
                match listening {
                    Ok(at) => {
                        let mut stages = BTreeMap::new();
                        if let Some(r) = vm.restored() {
                            stages
                                .insert("restore_ms".to_string(), ms(r.at.duration_since(started)));
                            if let Some(a) = r.ack_ms {
                                stages.insert("ack_ms".to_string(), a as f64);
                            }
                        }
                        stages.insert("listen_ms".to_string(), ms(at.duration_since(started)));
                        Ok(Ready {
                            vm: Some(vm),
                            stages,
                            at,
                        })
                    }
                    // the CPU it held is free only once its fcvm is gone
                    Err(error) => Err(Failed {
                        error,
                        exited: Exited::of(vm, STOP_GRACE).await,
                    }),
                }
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
                let hold_ms = o.pace_ms.map_or(o.hold_ms, |p| o.hold_ms.min(p));
                let ball = fleet_potato::Ball {
                    round: o.round,
                    seq: 0,
                    ring,
                    hold_ms,
                    hold_us: Some(hold_ms * 1000),
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
    /// Which machines hold one of the ring's CPUs. `set` is its only writer for the players.
    cpus: Cpus,
    gold_ready: bool,
    /// The wakes of the last [`WAKE_WINDOW`] (and always the latest): when each ended, and
    /// how long it took, ms.
    wakes: VecDeque<(Instant, f64)>,
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

    /// The one place a player's phase changes, and with it whether it holds a CPU. A phase
    /// that holds one follows either a phase that held it too, or `boot` and `wake`, which
    /// take the CPU first and start nothing without it.
    fn set(&mut self, n: u32, phase: Phase) {
        if phase.holds_cpu() {
            let held = self.cpus.take(Holder::Player(n));
            debug_assert!(held, "player {n} is {phase:?} without a CPU");
        } else {
            self.cpus.release(Holder::Player(n));
        }
        let p = &mut self.players[n as usize];
        p.phase = phase;
        p.since = Instant::now();
        if !matches!(phase, Phase::Awake | Phase::Waking | Phase::Booting) {
            p.ready_at = None;
            p.margin_due = None;
        }
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
        // The gold VM is a machine that is not asleep. It is made before any player exists,
        // so a CPU is free.
        let held = self.cpus.take(Holder::Gold);
        debug_assert!(held, "the gold is made while players hold every CPU");
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
        // no round, no wake waiting for a CPU
        self.schedule_wakes();
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
            throw_due: None,
        });
        self.advance();
    }

    /// Bring the round forward: wake the players within the lead, boot missing players, and
    /// throw the first ball once every player exists and the first is awake. Each wake and
    /// boot takes a CPU, and one that finds none free waits for the next call.
    fn advance(&mut self) {
        let Some(p) = &self.playing else { return };
        if p.over || !self.gold_ready {
            return;
        }
        let ring = p.order.ring.clone();
        // wakes first: the ball is on its way to them
        self.schedule_wakes();
        // Then new players, those the ball reaches last first: they sleep once booted and
        // give their CPU to the next boot. Those within the lead boot last and stay awake.
        let lead = self.within_lead();
        let (near, far): (Vec<u32>, Vec<u32>) = ring
            .iter()
            .filter(|n| {
                matches!(
                    self.players[**n as usize].phase,
                    Phase::Missing | Phase::Failed
                )
            })
            .partition(|n| lead.contains(n));
        for n in far.into_iter().chain(near) {
            if !self.boot(n) {
                break;
            }
        }
        let p = self.playing.as_ref().unwrap();
        let first = p.order.ring[0];
        if !p.kicked
            && ring.iter().all(|n| {
                !matches!(
                    self.players[*n as usize].phase,
                    Phase::Missing | Phase::Failed | Phase::Booting
                )
            })
            && self.players[first as usize].phase == Phase::Awake
        {
            self.kick();
        }
    }

    /// Forget the wakes older than the window, apart from the latest.
    fn prune_wakes(&mut self, now: Instant) {
        while self.wakes.len() > 1
            && self
                .wakes
                .front()
                .is_some_and(|(at, _)| now.duration_since(*at) > WAKE_WINDOW)
        {
            self.wakes.pop_front();
        }
    }

    /// The 95th percentile (nearest rank) and the slowest of the wakes in the window, ms.
    fn wake_times(&self) -> Option<(f64, f64)> {
        let mut all: Vec<f64> = self.wakes.iter().map(|(_, w)| *w).collect();
        all.sort_by(f64::total_cmp);
        let rank = (all.len() as f64 * 0.95).ceil() as usize;
        Some((*all.get(rank.checked_sub(1)?)?, *all.last()?))
    }

    /// The lead: how many catches ahead of the ball a sleeping player's wake is asked for.
    /// By time it is the 95th percentile wake plus the margin, in holds, and one more. It
    /// is never more than the CPUs leave room for: one machine holds the ball and one is
    /// saving, so at most `cpus - 2` can be waking or ready ahead of it, and asking for more
    /// would only make wakes wait for a CPU the agent itself took.
    fn lead(&self, hold_ms: u64) -> usize {
        let slow = self.wake_times().map_or(FIRST_WAKE_MS, |(p95, _)| p95);
        let by_time = ((slow + LEAD_MARGIN_MS) / hold_ms.max(1) as f64).ceil() as usize + 1;
        by_time.min((self.cpus.cap() as usize).saturating_sub(2).max(1))
    }

    /// The lead of the round being played, which a ring has room for: fewer than its players.
    fn lead_now(&self) -> usize {
        let Some(p) = &self.playing else { return 0 };
        self.lead(p.hold_ms())
            .min(p.order.ring.len().saturating_sub(1))
            .max(1)
    }

    /// The players within the lead of the ball: the next `lead` catchers, nearest first.
    fn within_lead(&self) -> Vec<u32> {
        let Some(p) = &self.playing else {
            return Vec::new();
        };
        if p.over {
            return Vec::new();
        }
        let n = p.order.ring.len();
        // until the first catch the next catcher is ring[0] itself
        let from = if p.caught { p.seq + 1 } else { 0 };
        (0..self.lead_now() as u64)
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

    /// Wake the sleeping players within the lead, nearest the ball first, while a CPU is
    /// free. One that finds none waits, and its wait is measured from the first call that
    /// wanted it; one no longer within the lead stops waiting.
    fn schedule_wakes(&mut self) {
        let wanted = self.within_lead();
        for (n, p) in self.players.iter_mut().enumerate() {
            if p.wanted_since.is_some() && !wanted.contains(&(n as u32)) {
                p.wanted_since = None;
            }
        }
        let now = Instant::now();
        for k in wanted {
            if self.players[k as usize].phase == Phase::Asleep && !self.wake(k) {
                self.players[k as usize].wanted_since.get_or_insert(now);
            }
        }
    }

    /// Restore player `n` from the gold. False, with nothing started, when no CPU is free.
    fn boot(&mut self, n: u32) -> bool {
        if !self.cpus.take(Holder::Player(n)) {
            return false;
        }
        self.set(n, Phase::Booting);
        let colours = self
            .playing
            .as_ref()
            .map(|p| p.order.colours)
            .unwrap_or(fleet_potato::DEFAULT_COLOURS);
        self.ops.boot(n, colours, self.done.clone());
        true
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

    /// Wake player `n`. False, with nothing started, when no CPU is free.
    fn wake(&mut self, n: u32) -> bool {
        if !self.cpus.take(Holder::Player(n)) {
            return false;
        }
        let cpu_wait_ms = self.players[n as usize]
            .wanted_since
            .take()
            .map(|since| ms(since.elapsed()));
        self.set(n, Phase::Waking);
        self.emit(n, PotatoKind::Waking { cpu_wait_ms });
        let round = self.playing.as_ref().map(|p| p.order.round);
        self.players[n as usize].woken_for = round;
        self.ops.wake(n, round, self.done.clone());
        true
    }

    /// What the ledger holds as the page's CPU slots: the machine with the ball, those
    /// ready, waking, saving and booting, each group in ring order from the ball.
    fn slots(&self) -> Vec<CpuSlot> {
        let playing = self.playing.as_ref().filter(|p| p.caught);
        let holder = playing.map(|p| p.order.ring[(p.seq % p.order.ring.len() as u64) as usize]);
        let distance = |n: u32| {
            playing
                .and_then(|p| {
                    let len = p.order.ring.len();
                    let at = p.order.ring.iter().position(|r| *r == n)?;
                    Some((at + len - (p.seq % len as u64) as usize) % len)
                })
                .unwrap_or(usize::MAX)
        };
        let mut slots: Vec<(usize, usize, CpuSlot)> = self
            .cpus
            .holders()
            .filter_map(|h| match h {
                Holder::Player(n) => Some(n),
                Holder::Gold => None,
            })
            .filter_map(|n| {
                let (rank, phase) = match self.players[n as usize].phase {
                    Phase::Awake | Phase::Waking | Phase::Booting if holder == Some(n) => {
                        (0, "ball")
                    }
                    Phase::Awake => (1, "ready"),
                    Phase::Waking => (2, "waking"),
                    Phase::Sleeping => (3, "saving"),
                    Phase::Booting => (4, "booting"),
                    Phase::Missing | Phase::Asleep | Phase::Failed => return None,
                };
                Some((
                    rank,
                    distance(n),
                    CpuSlot {
                        player: n,
                        phase: phase.into(),
                    },
                ))
            })
            .collect();
        slots.sort_by_key(|(rank, distance, slot)| (*rank, *distance, slot.player));
        slots.into_iter().map(|(_, _, slot)| slot).collect()
    }

    /// The ring's CPUs as counted now, for a catch's report. Reading it starts the next
    /// peak.
    fn ring_stats(&mut self) -> RingStats {
        self.prune_wakes(Instant::now());
        let asleep = self.playing.as_ref().map_or(0, |p| {
            p.order
                .ring
                .iter()
                .filter(|n| self.players[**n as usize].phase == Phase::Asleep)
                .count()
        });
        RingStats {
            cpus_in_use: self.cpus.in_use(),
            cpus_peak: self.cpus.take_peak(),
            asleep: asleep as u32,
            lead: self.lead_now() as u32,
            slowest_wake_ms: self.wake_times().map(|(_, slowest)| slowest),
            slots: self.slots(),
            cpu_usec: self.cfg.cgroup.as_ref().and_then(|c| c.usage_usec()),
        }
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
        let read = h.read;
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
                hold_us,
            } => {
                let mut ring = None;
                let mut ready_margin_ms = None;
                if let Some(pl) = self.playing.as_mut().filter(|pl| pl.order.round == round) {
                    let newest = !pl.caught || seq >= pl.seq;
                    // when the throw that this catch ended began
                    let due = pl.throw_due.filter(|(s, _)| *s == seq).map(|(_, at)| at);
                    pl.seq = if pl.caught { pl.seq.max(seq) } else { seq };
                    pl.caught = true;
                    if newest {
                        // its own throw begins once it has held the ball
                        let hold = hold_us.map_or_else(
                            || Duration::from_millis(pl.hold_ms()),
                            Duration::from_micros,
                        );
                        pl.throw_due = Some((seq + 1, read + hold));
                    }
                    if let (Some(due), Some(catcher)) = (due, self.players.get_mut(player as usize))
                    {
                        match catcher.ready_at {
                            Some(ready) => ready_margin_ms = Some(signed_ms(due, ready)),
                            // the end of its wake is still on its way to the driver
                            None => catcher.margin_due = Some(due),
                        }
                    }
                    self.schedule_wakes();
                    // counted after this catch's own wake has taken its CPU
                    ring = Some(self.ring_stats());
                }
                send(
                    self,
                    player,
                    PotatoKind::Caught {
                        round,
                        seq,
                        proof: proof(&p),
                        hold_us,
                        ring,
                        ready_margin_ms,
                    },
                );
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
                let catcher_cpu_wait_ms = self
                    .players
                    .get(to as usize)
                    .and_then(|p| p.wanted_since)
                    .map(|since| ms(since.elapsed()));
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
                        catcher_cpu_wait_ms,
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
                // the ball is down: nobody needs to be awake, and no wake waits for a CPU
                for n in 0..self.cfg.players {
                    if self.players[n as usize].phase == Phase::Awake {
                        self.sleep(n);
                    }
                }
                self.schedule_wakes();
            }
            Report::Probe { .. } => {}
        }
    }

    fn done(&mut self, x: Done) {
        match x {
            Done::Gold(made, _exited) => {
                self.cpus.release(Holder::Gold);
                match made {
                    Ok(()) => {
                        tracing::info!("potato: the player snapshot is ready");
                        self.gold_ready = true;
                        self.advance();
                    }
                    Err(e) => {
                        tracing::error!(error = %format!("{e:#}"), "potato: making the player snapshot failed; retrying in 10 s");
                        let done = self.done.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_secs(10)).await;
                            let _ = done.send(Done::RetryGold);
                        });
                    }
                }
            }
            Done::RetryGold => self.make_gold(),
            Done::Booted(n, Ok(ready)) => {
                self.players[n as usize].vm = ready.vm;
                self.set(n, Phase::Awake);
                self.players[n as usize].ready_at = Some(ready.at);
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
            Done::Booted(n, Err(failed)) => {
                self.set(n, Phase::Failed);
                self.emit(
                    n,
                    PotatoKind::Failed {
                        stage: "boot".into(),
                        error: format!("{:#}", failed.error),
                    },
                );
                // its CPU is free for a wake that waited; the player is booted again when the
                // round next moves, not at once, so a boot that keeps failing is not a loop
                self.schedule_wakes();
            }
            Done::Slept(n, Ok(mut stages), total, _exited) => {
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
                // its CPU is free: a wake or a boot that waited for one starts
                self.advance();
            }
            Done::Slept(n, Err(e), _, _exited) => {
                // its snapshot may be stale or half written: it starts again from the gold
                self.set(n, Phase::Missing);
                self.emit(
                    n,
                    PotatoKind::Failed {
                        stage: "sleep".into(),
                        error: format!("{e:#}"),
                    },
                );
                self.schedule_wakes();
            }
            Done::Woke(n, Ok(ready), total) => {
                self.players[n as usize].vm = ready.vm;
                self.set(n, Phase::Awake);
                let now = Instant::now();
                self.wakes.push_back((now, total));
                self.prune_wakes(now);
                // it caught before this was heard: the margin of that catch
                let ready_margin_ms = self.players[n as usize]
                    .margin_due
                    .take()
                    .map(|due| signed_ms(due, ready.at));
                self.players[n as usize].ready_at = Some(ready.at);
                self.emit(
                    n,
                    PotatoKind::Woke {
                        wake_ms: total,
                        stages: ready.stages,
                        ready_margin_ms,
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
                // Its wake was started for an earlier round, and this one does not need it
                // awake yet. Left awake it would hold a CPU until the ball came round, and
                // with every CPU held that way the first catcher could never be woken.
                let early = self.players[n as usize].woken_for
                    != self.playing.as_ref().map(|p| p.order.round)
                    && !self.needed(n);
                if over || !in_ring || early {
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
            Done::Woke(n, Err(failed), _) => {
                self.set(n, Phase::Missing);
                self.emit(
                    n,
                    PotatoKind::Failed {
                        stage: "wake".into(),
                        error: format!("{:#}", failed.error),
                    },
                );
                self.schedule_wakes();
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
        driver_on(players, 8)
    }

    /// A player that started catching now.
    fn ready() -> Ready {
        Ready {
            vm: None,
            stages: BTreeMap::new(),
            at: Instant::now(),
        }
    }

    /// A driver with `cpus` CPUs for its ring.
    fn driver_on(
        players: u32,
        cpus: u32,
    ) -> (Driver, Arc<Recorder>, mpsc::UnboundedReceiver<AgentMsg>) {
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
                cpus,
                cgroup: None,
            }),
            ops: Arc::new(rec.clone()),
            out,
            done,
            players: (0..players).map(|_| Player::new()).collect(),
            cpus: Cpus::new(cpus),
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
        Heard {
            report,
            at: 1,
            read: Instant::now(),
        }
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
            hold_us: None,
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
        d.wakes.push_back((Instant::now(), wake_ms));
        rec.take();
        (d, rec, out)
    }

    #[test]
    fn a_round_boots_its_players_from_the_gold_and_throws_once_the_first_is_awake() {
        let (mut d, rec, _out) = driver(3);
        d.order(Some(round(1, vec![0, 1], 1000)));
        assert_eq!(rec.take(), Vec::<String>::new(), "nothing before the gold");
        d.done(Done::Gold(Ok(()), Exited::none()));
        assert_eq!(
            rec.take(),
            vec!["boot 1", "boot 0"],
            "only the ring's players, the one the ball reaches last first"
        );
        d.done(Done::Booted(1, Ok(ready())));
        assert_eq!(
            rec.take(),
            vec!["sleep 1"],
            "player 1 is beyond the lead of 1 until the first catch; player 0 is still booting"
        );
        d.done(Done::Booted(0, Ok(ready())));
        assert_eq!(rec.take(), vec!["kick 1"]);
        d.done(Done::Booted(2, Ok(ready())));
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
            d.set(n as u32, Phase::Awake);
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
            d.done(Done::Woke(n, Ok(ready()), 300.0));
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
        d.done(Done::Slept(0, Ok(BTreeMap::new()), 300.0, Exited::none()));
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
            Err(unstarted(anyhow::anyhow!(
                "the memory file of snapshot potato-0 is gone: /dev/shm/fcvm-potato/fcvm-memory-x.bin"
            ))),
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
        d.done(Done::Woke(0, Ok(ready()), 300.0));
        assert_eq!(rec.take(), vec!["kick 1"]);
        d.heard(caught(0, 1, 0));
        assert_eq!(rec.take(), vec!["wake 1"]);
        // player 1 accepts before its wake is done, catches, holds and throws
        d.heard(threw(1, 1, 1, 0));
        assert!(rec.take().is_empty(), "no sleep while the wake runs");
        d.done(Done::Woke(1, Ok(ready()), 300.0));
        assert_eq!(rec.take(), vec!["sleep 1"]);
    }

    #[test]
    fn a_round_that_ends_puts_every_awake_player_to_sleep_and_wakes_no_one() {
        let (mut d, rec, _out) = asleep(4, 300.0);
        d.order(Some(round(1, (0..4).collect(), 1000)));
        rec.take();
        d.done(Done::Woke(0, Ok(ready()), 300.0));
        d.done(Done::Woke(1, Ok(ready()), 300.0));
        rec.take();
        d.order(None);
        let mut slept = rec.take();
        slept.sort();
        assert_eq!(
            slept,
            vec!["stop 1 sleep 0", "stop 1 sleep 1"],
            "each is told the round is over first, so no snapshot holds a throw of it"
        );
        d.done(Done::Woke(2, Ok(ready()), 300.0));
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
        d.done(Done::Woke(0, Ok(ready()), 300.0));
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
        d.done(Done::Woke(1, Ok(ready()), 400.0));
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
            throw_due: None,
        };
        assert_eq!(p.hold_ms(), 1000);
        p.seq = 4;
        assert_eq!(p.hold_ms(), 900, "the hold the ball carries in lap 1");
        p.order.pace_ms = Some(500);
        assert_eq!(p.hold_ms(), 500);
        p.order.pace_ms = Some(10);
        assert_eq!(p.hold_ms(), 50, "never below the floor");
    }

    /// The agent computes the hold the players keep: a tenth off each lap from 400 ms must
    /// reach the 2 ms floor (cut in whole milliseconds it stops at 9 ms).
    #[test]
    fn the_hold_the_agent_expects_reaches_its_floor() {
        let p = Playing {
            order: PotatoRound {
                round: 1,
                ring: vec![0, 1],
                hold_ms: 400,
                shrink_permille: 100,
                min_hold_ms: 2,
                flight_ms: 100,
                colours: 12,
                pace_ms: None,
            },
            seq: 400,
            caught: true,
            kicked: true,
            over: false,
            throw_due: None,
        };
        assert_eq!(p.hold_ms(), 2);
    }

    #[test]
    fn a_new_player_outside_the_lead_goes_to_sleep_once_booted() {
        // 300 ms wakes plus the margin at a 200 ms hold: a lead of 4 catches
        let (mut d, rec, _out) = driver(8);
        d.wakes.push_back((Instant::now(), 300.0));
        d.order(Some(round(1, (0..8).collect(), 200)));
        d.done(Done::Gold(Ok(()), Exited::none()));
        assert_eq!(rec.take().len(), 8, "every player is booted");
        for n in 0..8 {
            d.done(Done::Booted(n, Ok(ready())));
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
        for n in 0..2 {
            d.done(Done::Woke(n, Ok(ready()), 300.0));
        }
        // player 3 is awake, beyond the lead: round 2 does not need it yet
        d.set(3, Phase::Awake);
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
        d.done(Done::Woke(0, Ok(ready()), 300.0));
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
                cpus: 8,
                cgroup: None,
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

    // ---- the CPU allotment ----------------------------------------------------------------

    /// Every potato event the driver has sent, oldest first.
    fn sent(out: &mut mpsc::UnboundedReceiver<AgentMsg>) -> Vec<PotatoEvent> {
        std::iter::from_fn(|| out.try_recv().ok())
            .filter_map(|m| match m {
                AgentMsg::Potato(e) => Some(e),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_gold_holds_a_cpu_while_it_is_made() {
        let (mut d, rec, _out) = driver_on(2, 3);
        d.make_gold();
        assert_eq!((rec.take(), d.cpus.in_use()), (vec!["gold".to_string()], 1));
        d.done(Done::Gold(Ok(()), Exited::none()));
        assert_eq!(d.cpus.in_use(), 0);
    }

    /// New players boot as CPUs allow, those the ball reaches last first: each sleeps once
    /// booted and its CPU goes to the next. The first ball waits for all of them.
    #[test]
    fn boots_take_cpus_and_the_far_players_boot_first() {
        let (mut d, rec, _out) = driver_on(5, 3);
        d.order(Some(round(1, (0..5).collect(), 1000)));
        d.make_gold();
        rec.take();
        d.done(Done::Gold(Ok(()), Exited::none()));
        assert_eq!(rec.take(), vec!["boot 1", "boot 2", "boot 3"], "3 CPUs");
        d.done(Done::Booted(1, Ok(ready())));
        assert_eq!(
            rec.take(),
            vec!["sleep 1"],
            "saving, it still holds its CPU"
        );
        assert_eq!(d.cpus.in_use(), 3);
        d.done(Done::Slept(1, Ok(BTreeMap::new()), 300.0, Exited::none()));
        assert_eq!(rec.take(), vec!["boot 4"]);
        for n in [2, 3, 4] {
            d.done(Done::Booted(n, Ok(ready())));
            d.done(Done::Slept(n, Ok(BTreeMap::new()), 300.0, Exited::none()));
        }
        let cmds = rec.take();
        assert_eq!(
            cmds.iter()
                .filter(|c| c.starts_with("boot"))
                .collect::<Vec<_>>(),
            vec!["boot 0"],
            "the player within the lead boots last: {cmds:?}"
        );
        assert!(!cmds.iter().any(|c| c.starts_with("kick")), "{cmds:?}");
        d.done(Done::Booted(0, Ok(ready())));
        assert_eq!(rec.take(), vec!["kick 1"], "it stays awake for the ball");
        assert_eq!(d.cpus.in_use(), 1);
    }

    /// With every CPU held, the next catcher's wake waits for one, and says how long it did.
    #[test]
    fn a_wake_waits_for_a_free_cpu_and_reports_its_wait() {
        let (mut d, rec, mut out) = driver_on(6, 3);
        d.gold_ready = true;
        for p in &mut d.players {
            p.phase = Phase::Asleep;
        }
        d.wakes.push_back((Instant::now(), 300.0));
        d.order(Some(round(1, (0..6).collect(), 200)));
        assert_eq!(rec.take(), vec!["wake 0"], "3 CPUs leave a lead of 1");
        d.done(Done::Woke(0, Ok(ready()), 300.0));
        d.heard(caught(0, 1, 0));
        assert_eq!(rec.take(), vec!["kick 1", "wake 1"]);
        d.done(Done::Woke(1, Ok(ready()), 300.0));
        d.heard(caught(1, 1, 1));
        d.heard(threw(0, 1, 1, 1));
        assert_eq!(rec.take(), vec!["wake 2", "sleep 0"]);
        d.done(Done::Woke(2, Ok(ready()), 300.0));
        sent(&mut out);
        // 0 is saving, 1 has thrown and 2 holds the ball: no CPU for player 3
        d.heard(caught(2, 1, 2));
        d.heard(threw(1, 1, 2, 2));
        assert_eq!(rec.take(), vec!["sleep 1"], "no wake: every CPU is held");
        assert_eq!(d.cpus.in_use(), 3);
        assert!(d.players[3].wanted_since.is_some());
        let stats = sent(&mut out)
            .into_iter()
            .find_map(|e| match e.kind {
                PotatoKind::Caught { ring, .. } => ring,
                _ => None,
            })
            .expect("a catch carries the ring's CPUs");
        assert_eq!((stats.cpus_in_use, stats.cpus_peak, stats.lead), (3, 3, 1));
        assert_eq!(stats.asleep, 3, "players 3, 4 and 5; player 3 waits asleep");
        std::thread::sleep(Duration::from_millis(15));
        d.done(Done::Slept(0, Ok(BTreeMap::new()), 300.0, Exited::none()));
        assert_eq!(
            rec.take(),
            vec!["wake 3"],
            "the freed CPU goes to the wake that waited"
        );
        let waits: Vec<(u32, Option<f64>)> = sent(&mut out)
            .into_iter()
            .filter_map(|e| match e.kind {
                PotatoKind::Waking { cpu_wait_ms } => Some((e.player, cpu_wait_ms)),
                _ => None,
            })
            .collect();
        assert_eq!(waits.len(), 1);
        assert_eq!(waits[0].0, 3);
        assert!(waits[0].1.is_some_and(|w| w >= 15.0), "{waits:?}");
        assert!(d.players[3].wanted_since.is_none());
    }

    /// A wake that did not wait says nothing of a wait, and a dropped ball names how long the
    /// catcher's wake had waited for a CPU.
    #[test]
    fn a_drop_names_the_wait_for_a_cpu() {
        let (mut d, rec, mut out) = driver_on(6, 3);
        d.gold_ready = true;
        for p in &mut d.players {
            p.phase = Phase::Asleep;
        }
        d.order(Some(round(1, (0..6).collect(), 200)));
        d.done(Done::Woke(0, Ok(ready()), 300.0));
        d.heard(caught(0, 1, 0));
        let first: Vec<Option<f64>> = sent(&mut out)
            .into_iter()
            .filter_map(|e| match e.kind {
                PotatoKind::Waking { cpu_wait_ms } => Some(cpu_wait_ms),
                _ => None,
            })
            .collect();
        assert_eq!(first, vec![None, None], "players 0 and 1 found a CPU free");
        d.done(Done::Woke(1, Ok(ready()), 300.0));
        d.heard(caught(1, 1, 1));
        d.heard(threw(0, 1, 1, 1));
        d.done(Done::Woke(2, Ok(ready()), 300.0));
        d.heard(caught(2, 1, 2));
        rec.take();
        std::thread::sleep(Duration::from_millis(10));
        d.heard(heard(Report::Dropped {
            player: 2,
            round: 1,
            seq: 3,
            to: addr(3),
            attempts: 2,
            reason: "connection refused".into(),
        }));
        let dropped = sent(&mut out)
            .into_iter()
            .find_map(|e| match e.kind {
                PotatoKind::Dropped {
                    catcher_phase,
                    catcher_cpu_wait_ms,
                    ..
                } => Some((catcher_phase, catcher_cpu_wait_ms)),
                _ => None,
            })
            .expect("the drop is reported");
        assert_eq!(dropped.0, "asleep");
        assert!(dropped.1.is_some_and(|w| w >= 10.0), "{dropped:?}");
        assert!(
            d.players.iter().all(|p| p.wanted_since.is_none()),
            "the ball is down: no wake waits"
        );
    }

    /// The lead comes from the 95th percentile of the wakes of the last 30 s, not from the
    /// slowest, and is never more than the CPUs leave room for.
    #[test]
    fn the_lead_is_the_95th_percentile_wake_over_the_window_capped_by_the_cpus() {
        let (mut d, _rec, _out) = driver_on(64, 64);
        let now = Instant::now();
        for _ in 0..19 {
            d.wakes.push_back((now, 100.0));
        }
        d.wakes.push_back((now, 2000.0));
        assert_eq!(d.wake_times(), Some((100.0, 2000.0)));
        assert_eq!(
            d.lead(50),
            6,
            "(100 + 150) / 50, and one: the slowest would ask for 44"
        );
        // a wake older than the window no longer counts
        let Some(old) = now.checked_sub(Duration::from_secs(31)) else {
            return; // a host up for less than the window
        };
        d.wakes.clear();
        d.wakes.push_back((old, 5000.0));
        d.wakes.push_back((now, 100.0));
        d.prune_wakes(now);
        assert_eq!(d.wake_times(), Some((100.0, 100.0)));
        // but the latest one is kept however old: a ring that paused starts from it
        d.wakes.clear();
        d.wakes.push_back((old, 300.0));
        d.prune_wakes(now);
        assert_eq!(d.lead(450), 2);
        d.wakes.clear();
        assert_eq!(d.lead(1650), 2, "no wake seen: 1500 ms assumed");
        let (d, _rec, _out) = driver_on(64, 5);
        assert_eq!(d.lead(1), 3, "5 CPUs: the ball, one saving, and 3 ahead");
        let (d, _rec, _out) = driver_on(64, 3);
        assert_eq!(d.lead(1), 1);
    }

    /// A catch says which machine holds each CPU.
    #[test]
    fn a_catch_names_the_machine_on_each_cpu() {
        let (mut d, rec, mut out) = asleep(8, 300.0);
        d.order(Some(round(1, (0..8).collect(), 200)));
        for n in 0..4 {
            d.done(Done::Woke(n, Ok(ready()), 300.0));
        }
        d.heard(caught(0, 1, 0));
        d.heard(threw(0, 1, 1, 1));
        sent(&mut out);
        d.heard(caught(1, 1, 1));
        rec.take();
        let stats = sent(&mut out)
            .into_iter()
            .find_map(|e| match e.kind {
                PotatoKind::Caught { ring, .. } => ring,
                _ => None,
            })
            .unwrap();
        let slots: Vec<(u32, &str)> = stats
            .slots
            .iter()
            .map(|s| (s.player, s.phase.as_str()))
            .collect();
        assert_eq!(
            slots,
            vec![
                (1, "ball"),
                (2, "ready"),
                (3, "ready"),
                (4, "waking"),
                (5, "waking"),
                (0, "saving")
            ]
        );
        assert_eq!(stats.cpus_in_use as usize, slots.len());
        assert_eq!(stats.asleep, 2);
        assert_eq!(stats.slowest_wake_ms, Some(300.0));
        assert_eq!(stats.cpu_usec, None, "no cpuset on this host");
    }

    /// The margin of a catch is when the throw to the catcher began (the catch before it,
    /// plus the hold its catcher kept) less when the catcher's wake ended.
    #[test]
    fn the_ready_margin_is_the_throw_start_less_the_end_of_the_wake() {
        let margin_of = |ready_after_ms: u64, woke_first: bool| {
            let (mut d, _rec, mut out) = asleep(2, 300.0);
            d.order(Some(round(1, vec![0, 1], 1000)));
            d.done(Done::Woke(0, Ok(ready()), 300.0));
            let base = Instant::now();
            // player 0 catches at `base` and keeps the ball 50 ms
            d.heard(Heard {
                report: Report::Caught {
                    player: 0,
                    round: 1,
                    seq: 0,
                    proof: proof(),
                    hold_us: Some(50_000),
                },
                at: 1,
                read: base,
            });
            let woke = |d: &mut Driver| {
                d.done(Done::Woke(
                    1,
                    Ok(Ready {
                        vm: None,
                        stages: BTreeMap::new(),
                        at: base + Duration::from_millis(ready_after_ms),
                    }),
                    300.0,
                ))
            };
            if woke_first {
                woke(&mut d);
            }
            sent(&mut out);
            d.heard(caught(1, 1, 1));
            if !woke_first {
                woke(&mut d);
            }
            let events = sent(&mut out);
            let at_catch = events.iter().find_map(|e| match &e.kind {
                PotatoKind::Caught {
                    ready_margin_ms, ..
                } => Some(*ready_margin_ms),
                _ => None,
            });
            let at_woke = events.iter().find_map(|e| match &e.kind {
                PotatoKind::Woke {
                    ready_margin_ms, ..
                } => Some(*ready_margin_ms),
                _ => None,
            });
            (at_catch.flatten(), at_woke.flatten())
        };
        assert_eq!(margin_of(20, true), (Some(30.0), None), "ready 30 ms early");
        assert_eq!(
            margin_of(80, true),
            (Some(-30.0), None),
            "the thrower waited 30 ms"
        );
        assert_eq!(
            margin_of(80, false),
            (None, Some(-30.0)),
            "the catch was read before the wake's end: the wake carries it"
        );
    }

    /// The first catch of a round follows no throw of a player: it has no margin.
    #[test]
    fn a_rounds_first_catch_has_no_margin() {
        let (mut d, _rec, mut out) = asleep(2, 300.0);
        d.order(Some(round(1, vec![0, 1], 1000)));
        d.done(Done::Woke(0, Ok(ready()), 300.0));
        sent(&mut out);
        d.heard(caught(0, 1, 0));
        let margin = sent(&mut out).into_iter().find_map(|e| match e.kind {
            PotatoKind::Caught {
                ready_margin_ms, ..
            } => Some(ready_margin_ms),
            _ => None,
        });
        assert_eq!(margin, Some(None));
    }

    /// An operation that failed tells the driver only once its fcvm is gone: the driver
    /// frees the player's CPU on that message.
    #[tokio::test]
    async fn a_failed_operation_reports_after_its_fcvm_has_exited() {
        // slow to stop: 0.3 s after SIGTERM, taking its own child with it
        let slow = "trap 'sleep 0.3; kill $!; exit 0' TERM; sleep 30 & wait";
        let vm = Vm::spawn_args(
            std::path::Path::new("/bin/sh"),
            &["-c".to_string(), slow.to_string()],
            "test",
        )
        .unwrap();
        let pid = nix::unistd::Pid::from_raw(vm.pid as i32);
        // let the shell install its trap
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _exited: Exited = Exited::of(vm, Duration::from_secs(10)).await;
        assert_eq!(
            nix::sys::signal::kill(pid, None),
            Err(nix::errno::Errno::ESRCH),
            "the process is gone, and reaped, when the proof exists"
        );
    }

    /// A wake started for a round that was then replaced: the player goes back to sleep
    /// unless the new round needs it awake now. With 3 CPUs, three such players left awake
    /// would hold them all while the new round's first catcher sleeps.
    #[test]
    fn a_wake_started_for_a_replaced_round_does_not_keep_its_cpu() {
        let (mut d, rec, _out) = driver_on(5, 3);
        d.gold_ready = true;
        for p in &mut d.players {
            p.phase = Phase::Asleep;
        }
        d.order(Some(round(1, (0..5).collect(), 200)));
        d.done(Done::Woke(0, Ok(ready()), 300.0));
        // players 1 and 2 catch before the agent hears that their wakes ended
        d.heard(caught(0, 1, 0));
        d.heard(caught(1, 1, 1));
        d.heard(threw(0, 1, 1, 1));
        d.done(Done::Slept(0, Ok(BTreeMap::new()), 300.0, Exited::none()));
        d.heard(caught(2, 1, 2));
        assert_eq!(
            (0..5).map(|n| d.players[n].phase).collect::<Vec<_>>(),
            vec![
                Phase::Asleep,
                Phase::Waking,
                Phase::Waking,
                Phase::Waking,
                Phase::Asleep
            ],
            "three wakes run, on the three CPUs"
        );
        rec.take();
        // the coordinator replaces the round: its first catcher is player 0, asleep
        d.order(Some(round(2, (0..5).collect(), 200)));
        assert!(rec.take().is_empty(), "no CPU for player 0 yet");
        assert!(d.players[0].wanted_since.is_some());
        for n in 1..=3 {
            d.done(Done::Woke(n, Ok(ready()), 300.0));
        }
        assert_eq!(
            rec.take(),
            vec!["stop 1 sleep 1", "stop 1 sleep 2", "stop 1 sleep 3"],
            "none is needed by round 2 yet"
        );
        d.done(Done::Slept(1, Ok(BTreeMap::new()), 300.0, Exited::none()));
        assert_eq!(
            rec.take(),
            vec!["wake 0"],
            "the first catcher gets the freed CPU"
        );
        // a wake the new round does need stays awake
        d.done(Done::Woke(0, Ok(ready()), 300.0));
        assert_eq!(rec.take(), vec!["kick 2"]);
    }

    // ---- every interleaving ---------------------------------------------------------------

    /// One thing that can happen to the driver.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Ev {
        Booted(u32, bool),
        Woke(u32, bool),
        Slept(u32, bool),
        /// The next catcher catches the ball.
        Catch,
        /// A thrower's report of a throw that was caught is read.
        Threw(u32, u64, u32),
        /// The holder's throw found nobody.
        Drop,
        /// The holder of an ended round's ball abandons its throw.
        StaleDrop(u32, u64, u64),
        NextRound,
        NoRound,
    }

    /// The driver and the world around it: the operations it started and has not been told
    /// the end of, and where the ball is.
    struct World {
        d: Driver,
        rec: Arc<Recorder>,
        ring: Vec<u32>,
        /// player -> `boot`, `wake` or `sleep`.
        ops: BTreeMap<u32, &'static str>,
        round: u64,
        /// The latest catch of the round being played: (seq, catcher).
        held: Option<(u64, u32)>,
        /// The first ball is in the air.
        kicked: bool,
        /// Throws caught whose report the agent has not read: (thrower, seq, catcher).
        threw: Vec<(u32, u64, u32)>,
        /// Players that have thrown and not been restored since: they refuse the ball.
        spent: std::collections::BTreeSet<u32>,
        stale: Option<(u32, u64, u64)>,
        rounds_left: u32,
    }

    impl World {
        fn new(players: u32, cpus: u32, rounds: u32) -> World {
            let (mut d, rec, out) = driver_on(players, cpus);
            std::mem::forget(out);
            d.gold_ready = true;
            let mut w = World {
                d,
                rec,
                ring: (0..players).collect(),
                ops: BTreeMap::new(),
                round: 0,
                held: None,
                kicked: false,
                threw: Vec::new(),
                spent: Default::default(),
                stale: None,
                rounds_left: rounds,
            };
            w.apply(Ev::NextRound);
            w
        }

        fn phase(&self, n: u32) -> Phase {
            self.d.players[n as usize].phase
        }

        /// A copy that goes its own way: a new driver with this one's state.
        fn fork(&self) -> World {
            let (mut d, rec, out) = driver_on(self.d.cfg.players, self.d.cpus.cap());
            std::mem::forget(out);
            for (to, from) in d.players.iter_mut().zip(&self.d.players) {
                *to = Player { vm: None, ..*from };
            }
            d.cpus = self.d.cpus.clone();
            d.gold_ready = self.d.gold_ready;
            d.wakes = self.d.wakes.clone();
            d.playing = self.d.playing.clone();
            d.late = self.d.late;
            d.ended = self.d.ended;
            World {
                d,
                rec,
                ring: self.ring.clone(),
                ops: self.ops.clone(),
                round: self.round,
                held: self.held,
                kicked: self.kicked,
                threw: self.threw.clone(),
                spent: self.spent.clone(),
                stale: self.stale,
                rounds_left: self.rounds_left,
            }
        }

        /// Whether player `n` would accept a ball: it has a VM that is up or coming up, and
        /// has not thrown since it was last restored.
        fn may_accept(&self, n: u32) -> bool {
            !self.spent.contains(&n)
                && matches!(self.phase(n), Phase::Awake | Phase::Waking | Phase::Booting)
        }

        fn next_catcher(&self) -> Option<(u64, u32)> {
            let n = self.ring.len() as u64;
            match self.held {
                Some((seq, _)) => Some((seq + 1, self.ring[((seq + 1) % n) as usize])),
                None if self.kicked => Some((0, self.ring[0])),
                None => None,
            }
        }

        fn enabled(&self, max_seq: u64) -> Vec<Ev> {
            let mut evs = Vec::new();
            for (n, op) in &self.ops {
                for ok in [true, false] {
                    evs.push(match *op {
                        "boot" => Ev::Booted(*n, ok),
                        "wake" => Ev::Woke(*n, ok),
                        _ => Ev::Slept(*n, ok),
                    });
                }
            }
            if let Some((seq, to)) = self.next_catcher() {
                if seq <= max_seq && self.may_accept(to) {
                    evs.push(Ev::Catch);
                }
                // a throw finds nobody unless the catcher is plainly awake and accepting
                if self.held.is_some() && !(self.phase(to) == Phase::Awake && self.may_accept(to)) {
                    evs.push(Ev::Drop);
                }
            }
            evs.extend(self.threw.iter().map(|(a, s, b)| Ev::Threw(*a, *s, *b)));
            if let Some((n, round, seq)) = self.stale {
                evs.push(Ev::StaleDrop(n, round, seq));
            }
            if self.rounds_left > 0 {
                evs.push(Ev::NextRound);
                evs.push(Ev::NoRound);
            }
            evs
        }

        fn end_round(&mut self) {
            if let Some((seq, by)) = self.held.take() {
                self.stale = Some((by, self.round, seq + 1));
            }
            self.kicked = false;
        }

        fn apply(&mut self, ev: Ev) {
            let fail = || unstarted(anyhow::anyhow!("failed"));
            match ev {
                Ev::Booted(n, ok) => {
                    self.ops.remove(&n);
                    let r = if ok { Ok(ready()) } else { Err(fail()) };
                    self.d.done(Done::Booted(n, r));
                }
                Ev::Woke(n, ok) => {
                    self.ops.remove(&n);
                    let r = if ok { Ok(ready()) } else { Err(fail()) };
                    self.d.done(Done::Woke(n, r, 300.0));
                }
                Ev::Slept(n, ok) => {
                    self.ops.remove(&n);
                    let r = if ok {
                        Ok(BTreeMap::new())
                    } else {
                        Err(anyhow::anyhow!("failed"))
                    };
                    self.d.done(Done::Slept(n, r, 300.0, Exited::none()));
                }
                Ev::Catch => {
                    let (seq, to) = self.next_catcher().unwrap();
                    if let Some((_, by)) = self.held {
                        self.threw.push((by, seq, to));
                        self.spent.insert(by);
                    }
                    self.held = Some((seq, to));
                    self.d.heard(caught(to, self.round, seq));
                }
                Ev::Threw(a, seq, b) => {
                    self.threw.retain(|t| *t != (a, seq, b));
                    self.d.heard(threw(a, self.round, seq, b));
                }
                Ev::Drop => {
                    let (seq, to) = self.next_catcher().unwrap();
                    let (_, by) = self.held.take().unwrap();
                    self.kicked = false;
                    self.d.heard(heard(Report::Dropped {
                        player: by,
                        round: self.round,
                        seq,
                        to: addr(to),
                        attempts: 1,
                        reason: "refused".into(),
                    }));
                }
                Ev::StaleDrop(n, round, seq) => {
                    self.stale = None;
                    self.d.heard(heard(Report::Dropped {
                        player: n,
                        round,
                        seq,
                        to: addr(self.ring[(seq % self.ring.len() as u64) as usize]),
                        attempts: 0,
                        reason: "the round is over".into(),
                    }));
                }
                Ev::NextRound => {
                    self.end_round();
                    self.rounds_left = self.rounds_left.saturating_sub(1);
                    self.round += 1;
                    self.threw.clear();
                    self.d
                        .order(Some(round(self.round, self.ring.clone(), 200)));
                }
                Ev::NoRound => {
                    self.end_round();
                    self.rounds_left = self.rounds_left.saturating_sub(1);
                    self.threw.clear();
                    self.d.order(None);
                }
            }
            // what the driver started
            for cmd in self.rec.take() {
                let mut words = cmd.split(' ').rev();
                let last = words.next().unwrap();
                let op = match cmd.split(' ').next().unwrap() {
                    "boot" => "boot",
                    "wake" => "wake",
                    "sleep" | "stop" => "sleep",
                    "kick" => {
                        if last.parse() == Ok(self.round) {
                            self.kicked = true;
                        }
                        continue;
                    }
                    _ => continue,
                };
                let n: u32 = last.parse().unwrap();
                if op != "sleep" {
                    self.spent.remove(&n);
                }
                assert!(
                    self.ops.insert(n, op).is_none(),
                    "{cmd}: player {n} already has an operation running"
                );
            }
        }

        /// What must hold after every event.
        fn check(&self, cpus: u32) -> Result<(), String> {
            let d = &self.d;
            let ensure = |ok: bool, what: String| if ok { Ok(()) } else { Err(what) };
            ensure(d.cpus.in_use() <= cpus, "more machines than CPUs".into())?;
            let mut not_asleep = 0;
            for (n, p) in d.players.iter().enumerate() {
                let n = n as u32;
                ensure(
                    d.cpus.holds(Holder::Player(n)) == p.phase.holds_cpu(),
                    format!("the ledger and player {n}, {:?}, disagree", p.phase),
                )?;
                let op = self.ops.get(&n).copied();
                let expected = match p.phase {
                    Phase::Booting => Some("boot"),
                    Phase::Waking => Some("wake"),
                    Phase::Sleeping => Some("sleep"),
                    _ => None,
                };
                ensure(
                    op == expected,
                    format!("player {n} is {:?} with operation {op:?}", p.phase),
                )?;
                // a machine exists for it: an operation on it runs, or it is awake
                if op.is_some() || p.phase == Phase::Awake {
                    not_asleep += 1;
                }
            }
            ensure(
                not_asleep == d.cpus.in_use() && not_asleep <= cpus,
                format!(
                    "{not_asleep} machines not asleep, {} in the ledger, {cpus} CPUs",
                    d.cpus.in_use()
                ),
            )?;
            // no CPU idles while a wake waits for one, and only such a wake waits
            let lead = d.within_lead();
            for (n, p) in d.players.iter().enumerate() {
                let wanted = lead.contains(&(n as u32)) && p.phase == Phase::Asleep;
                ensure(
                    !(wanted && d.cpus.has_free()),
                    format!("player {n} sleeps within the lead beside a free CPU"),
                )?;
                ensure(
                    p.wanted_since.is_some() == wanted,
                    format!(
                        "player {n} waits: {}, wanted: {wanted}",
                        p.wanted_since.is_some()
                    ),
                )?;
            }
            // and a CPU will come free for the next catcher: an operation is on its way out,
            // or the ball is held by a machine that sleeps once it has thrown
            if let Some(next) = lead.first().filter(|n| self.phase(**n) == Phase::Asleep) {
                ensure(
                    !self.ops.is_empty() || self.held.is_some(),
                    format!("player {next}, the next catcher, waits for a CPU that nothing frees"),
                )?;
            }
            Ok(())
        }

        fn key(&self) -> String {
            let d = &self.d;
            let players: Vec<_> = d
                .players
                .iter()
                .map(|p| (p.phase.name(), p.sleep_after_wake, p.wanted_since.is_some()))
                .collect();
            let playing = d
                .playing
                .as_ref()
                .map(|p| (p.order.round, p.seq, p.caught, p.kicked, p.over));
            format!(
                "{players:?} {playing:?} {:?} {:?} {:?} {} {:?} {:?} {:?} {:?} {} {}",
                d.ended,
                d.late.map(|l| (l.0, l.1, l.2)),
                self.ops,
                self.round,
                self.held,
                self.threw,
                self.spent,
                self.stale,
                self.kicked,
                self.rounds_left,
            )
        }
    }

    /// Drive the driver through every order of events its state allows (ends of boots, wakes
    /// and sleeps, each succeeding or failing; catches, throws read late, drops, a round ended
    /// or replaced at any point), and check the invariants after each. Returns the states
    /// seen.
    fn explore(players: u32, cpus: u32, rounds: u32, max_seq: u64) -> usize {
        let mut seen = std::collections::HashSet::new();
        // each state with the events that led to it, for the report of a broken invariant
        let mut queue = VecDeque::new();
        let start = World::new(players, cpus, rounds);
        start.check(cpus).unwrap();
        seen.insert(start.key());
        queue.push_back((start, Vec::<Ev>::new()));
        while let Some((at, path)) = queue.pop_front() {
            for ev in at.enabled(max_seq) {
                let mut next = at.fork();
                next.apply(ev);
                let mut longer = path.clone();
                longer.push(ev);
                if let Err(broken) = next.check(cpus) {
                    panic!("{broken}, after {longer:?}");
                }
                if seen.insert(next.key()) {
                    queue.push_back((next, longer));
                }
            }
        }
        seen.len()
    }

    /// No oversubscription, in every interleaving: machines not asleep never exceed the CPUs,
    /// the ledger agrees with what the driver started, no CPU idles while a wake waits, and
    /// nothing waits for a CPU that will not come free.
    #[test]
    fn machines_not_asleep_never_exceed_the_cpus_in_any_interleaving() {
        // (players, CPUs, rounds, catches per round). 3 CPUs are the fewest allowed, with more
        // players than CPUs so the ring always wants more than it has; two rounds let the
        // coordinator replace or end a round at any point of the first. A search that finds
        // few states has stopped searching.
        for (bound, at_least) in [
            ((4, 3, 2, 2), 150_000),
            ((4, 3, 1, 3), 80_000),
            ((4, 4, 1, 3), 40_000),
        ] {
            let (players, cpus, rounds, catches) = bound;
            let states = explore(players, cpus, rounds, catches);
            assert!(states >= at_least, "{bound:?}: only {states} states");
        }
        // POTATO_SEARCH=players,cpus,rounds,catches searches one more bound, for a deeper run
        // than the suite has time for (4,3,1,5 is 881,212 states).
        if let Ok(bound) = std::env::var("POTATO_SEARCH") {
            let b: Vec<u64> = bound.split(',').map(|x| x.parse().unwrap()).collect();
            let states = explore(b[0] as u32, b[1] as u32, b[2] as u32, b[3]);
            eprintln!("{bound}: {states} states");
        }
    }
}
