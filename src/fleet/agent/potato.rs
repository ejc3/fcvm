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
/// How long an fcvm that exited is given for its last lines to be read into its failure.
const TAIL_WAIT: Duration = Duration::from_millis(100);
/// How long the ring's VMs have to exit after SIGTERM when the agent stops; what is left is
/// killed.
const RING_STOP_GRACE: Duration = Duration::from_secs(20);
/// How long a killed ring is given to be gone. The ring's stop returns [`RING_STOP_GRACE`]
/// plus this after it began, whatever is left: well inside the agent unit's 60 s to stop.
const RING_KILL_WAIT: Duration = Duration::from_secs(5);

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
    /// How long its latest wake waited for a free CPU before it started, ms.
    cpu_waited_ms: Option<f64>,
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
            cpu_waited_ms: None,
        }
    }
}

/// Proof that an operation's fcvm has exited, or that it never started one. The driver gives
/// a player's CPU back only on a message that carries it, so no CPU is counted free while
/// the process that held it is still there.
pub(crate) struct Exited(());

impl Exited {
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
    /// The throw of the latest catch's holder: the seq it carries, when it starts (the catch's
    /// read time plus the hold the catcher keeps) and how long the thrower then tries (its
    /// flight window).
    throw_due: Option<(u64, Instant, Duration)>,
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
        let c = TcpStream::connect((addr, BALL_PORT)).await?;
        exchange(c, msg).await
    })
    .await
    .with_context(|| format!("[{addr}]:{BALL_PORT} did not answer in {deadline:?}"))?
}

/// `msg` over a connection to a player's ball port, and the player's one-line answer.
async fn exchange(mut c: TcpStream, msg: &Inbound) -> Result<String> {
    c.set_nodelay(true)?;
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    c.write_all(line.as_bytes()).await?;
    let mut answer = String::new();
    BufReader::new(c).read_line(&mut answer).await?;
    Ok(answer)
}

/// One probe of a player's ball port: its connect within the budget's connect timeout, and
/// once connected (the player listens) its answer within the budget's answer timeout.
async fn probe(
    addr: Ipv6Addr,
    budget: super::probe::Budget,
    connected: super::probe::Connected,
) -> Result<String> {
    let c = tokio::time::timeout(budget.connect, TcpStream::connect((addr, BALL_PORT)))
        .await
        .with_context(|| {
            format!(
                "[{addr}]:{BALL_PORT} did not accept a connection in {:?}",
                budget.connect
            )
        })??;
    connected.mark();
    tokio::time::timeout(budget.answer, exchange(c, &Inbound::Probe))
        .await
        .with_context(|| format!("[{addr}]:{BALL_PORT} did not answer in {:?}", budget.answer))?
}

/// How a player's ball port is probed (see [`super::probe`]): every 10 ms with 25 ms for a
/// connect in the first second, which is 3 sockets and 100 SYNs at a player that does not
/// answer a SYN, then one attempt at a time. A probe that connected has the 250 ms an attempt
/// had before for its answer.
const LISTEN_PACE: super::probe::Pace = super::probe::Pace {
    every: Duration::from_millis(10),
    fast_connect: Duration::from_millis(25),
    answer: Duration::from_millis(250),
};

/// Probe a player's ball port until it answers, up to `deadline`. A probe sent before the port
/// accepts is lost and its connect ends only at its timeout; the next does not wait for it in
/// the first second of the wait.
async fn until_listening(addr: Ipv6Addr, deadline: Duration) -> Result<Instant> {
    let mut probes = super::probe::Probes::new(LISTEN_PACE, |budget, connected| {
        probe(addr, budget, connected)
    });
    let end = tokio::time::sleep(deadline);
    tokio::pin!(end);
    // Answers are read in the order their probes were sent, so this is the newest.
    let mut last = String::new();
    loop {
        tokio::select! {
            answer = probes.next() => match answer {
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
            },
            _ = &mut end => bail!(
                "[{addr}]:{BALL_PORT} never answered a probe: {}",
                last.trim()
            ),
        }
    }
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
        .stdin(std::process::Stdio::null())
        // an operation that is aborted takes its command with it: no fcvm the agent started
        // runs on without the agent
        .kill_on_drop(true);
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
    let ops = Arc::new(FcvmOps {
        cfg: cfg.clone(),
        sleep_memory,
        boot_slots: Arc::new(Semaphore::new(BOOT_PARALLEL)),
        tasks: Default::default(),
        saves: Default::default(),
        aborted: Default::default(),
    });
    let mut d = Driver {
        epoch: Instant::now(),
        cfg: cfg.clone(),
        ops: ops.clone(),
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
    // in-flight work first (a boot waiting for a slot would start a VM after this); a save
    // goes on, and the ring's stop gives it the ring's time to finish
    d.ops.shutdown();
    let vms: Vec<Vm> = d.players.iter_mut().filter_map(|p| p.vm.take()).collect();
    drop(d);
    stop_ring(
        Some(&ops),
        vms,
        cfg.cgroup.as_deref(),
        RING_STOP_GRACE,
        RING_KILL_WAIT,
    )
    .await;
    Ok(())
}

/// Stop the ring's VMs within the ring's own time. One stuck player VM must not hold the
/// agent past the time systemd gives its unit, or systemd kills every VM of the host, the
/// wall's and the phones' included, without their teardown. So the whole stop has one
/// deadline, `grace` plus `kill_wait` from now, and returns by it whatever is left, naming
/// by pid the fcvm that had not exited.
///
/// Every fcvm the agent started for the ring ends here, by the agent's own handle on it:
/// - `vms`, the VMs the players held, and the VMs of the operations `shutdown` aborted (a
///   wake or a boot in flight), which those hand over instead of dropping: SIGTERM to each
///   at once, SIGKILL to one that has not exited after `grace`;
/// - the saves in flight: each has until `grace` to finish as it would have. One still running
///   then is aborted, which kills its `fcvm snapshot create`, and its VM gets half of
///   `kill_wait` after its SIGTERM before it is killed too.
///
/// Where the ring has a cgroup, whatever is still in it then is killed through it.
async fn stop_ring(
    ops: Option<&FcvmOps>,
    vms: Vec<Vm>,
    cgroup: Option<&RingCgroup>,
    grace: Duration,
    kill_wait: Duration,
) {
    let end = tokio::time::Instant::now() + grace;
    let last = end + kill_wait;
    let stopping = std::sync::Mutex::new(Vec::new());
    let all = stop_ring_by(ops, vms, cgroup, end, last, &stopping);
    tokio::pin!(all);
    if tokio::time::timeout_at(last, &mut all).await.is_ok() {
        return;
    }
    // before the stops are dropped: a pid is its fcvm's while the handle on it is held
    let left: Vec<u32> = stopping
        .lock()
        .unwrap()
        .iter()
        .copied()
        .filter(|pid| nix::sys::signal::kill(nix::unistd::Pid::from_raw(*pid as i32), None).is_ok())
        .collect();
    tracing::error!(
        ?left,
        "potato: the ring was not stopped {:?} after it was told to; the agent goes on without it. \
         `left` are the fcvm that had not exited, by pid",
        grace + kill_wait
    );
}

/// [`stop_ring`] without its deadline: SIGKILL at `end`, everything gone by `last`. Every
/// fcvm it is stopping is in `stopping`, by pid.
async fn stop_ring_by(
    ops: Option<&FcvmOps>,
    vms: Vec<Vm>,
    cgroup: Option<&RingCgroup>,
    end: tokio::time::Instant,
    last: tokio::time::Instant,
    stopping: &std::sync::Mutex<Vec<u32>>,
) {
    let until =
        |at: tokio::time::Instant| at.saturating_duration_since(tokio::time::Instant::now());
    // when what was only told to stop at `end` is killed
    let kill_at = end + (last - end) / 2;
    let mut stops = tokio::task::JoinSet::new();
    let mut stop = |mut vm: Vm, kill_at: tokio::time::Instant| {
        stopping.lock().unwrap().push(vm.pid);
        stops.spawn(async move {
            let grace = kill_at.saturating_duration_since(tokio::time::Instant::now());
            vm.stop(grace).await;
        });
    };
    for vm in vms {
        stop(vm, end);
    }
    if let Some(ops) = ops {
        for vm in ops.end_aborted().await {
            stop(vm, end);
        }
        for vm in ops.end_saves(end).await {
            stop(vm, kill_at);
        }
    }
    while stops.join_next().await.is_some() {}
    let Some(cgroup) = cgroup else { return };
    if cgroup.emptied(until(kill_at)).await {
        return;
    }
    tracing::warn!("potato: processes are left in the ring's cgroup after SIGTERM; killing them");
    if let Err(e) = cgroup.kill() {
        tracing::warn!(error = %e, "potato: the ring's cgroup could not be killed");
    }
    if !cgroup.emptied(until(last)).await {
        tracing::warn!("potato: the ring's cgroup is still populated after it was killed");
    }
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
    /// Every operation in flight but the saves, so a shutdown can abort them: a boot still
    /// waiting for a slot starts nothing, and a VM an aborted operation held goes to `aborted`.
    tasks: std::sync::Mutex<tokio::task::JoinSet<()>>,
    /// The saves in flight. A shutdown does not abort them: the ring's stop waits for them.
    saves: std::sync::Mutex<tokio::task::JoinSet<()>>,
    /// The VMs of operations that were aborted, for the ring's stop to end by their handles.
    aborted: Arc<std::sync::Mutex<Vec<Vm>>>,
}

/// Where the VMs an operation held go when it ends without ending them.
#[derive(Clone)]
struct Handover {
    /// For an operation that was aborted: the ring's stop ends these.
    aborted: Arc<std::sync::Mutex<Vec<Vm>>>,
    /// For an operation that panicked: [`FcvmOps::spawn_op`] ends these and fails the
    /// operation. One list an operation, so each failure waits for its own VM.
    lost: Arc<std::sync::Mutex<Vec<Vm>>>,
    /// The operation panicked. Its future is dropped after that is known, and what it still
    /// holds then goes to `lost` too.
    panicked: Arc<std::sync::atomic::AtomicBool>,
}

impl Handover {
    fn hold(&self, vm: Vm) -> Held {
        Held {
            vm: Some(vm),
            holder: self.clone(),
        }
    }
}

/// A VM an operation holds. An operation that is aborted or panics drops what it holds, and
/// a `Vm` that is dropped gets a SIGTERM nobody follows: its fcvm can outlive the agent, and
/// the player's CPU would be counted free with the machine still there. This hands the VM on
/// instead, to whoever waits for it and kills it if it has to.
struct Held {
    vm: Option<Vm>,
    holder: Handover,
}

impl Held {
    /// The VM, for an operation that gives it on running.
    fn into_vm(mut self) -> Vm {
        self.vm.take().expect("a held VM is taken once")
    }

    /// Stop the VM and wait for its fcvm to exit: SIGTERM, then SIGKILL after `grace`. An
    /// operation aborted while it waits here still hands the VM on.
    async fn ended(mut self, grace: Duration) -> Exited {
        self.stop(grace).await;
        drop(self.into_vm());
        Exited(())
    }
}

impl std::ops::Deref for Held {
    type Target = Vm;
    fn deref(&self) -> &Vm {
        self.vm.as_ref().expect("a held VM is taken once")
    }
}

impl std::ops::DerefMut for Held {
    fn deref_mut(&mut self) -> &mut Vm {
        self.vm.as_mut().expect("a held VM is taken once")
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let Some(vm) = self.vm.take() else { return };
        let panicked = std::thread::panicking()
            || self
                .holder
                .panicked
                .load(std::sync::atomic::Ordering::SeqCst);
        let to = if panicked {
            &self.holder.lost
        } else {
            &self.holder.aborted
        };
        // a poisoned list still takes the VM: it must not be dropped here
        to.lock().unwrap_or_else(|e| e.into_inner()).push(vm);
    }
}

/// What a panic carried, as text.
fn panic_text(panic: Box<dyn std::any::Any + Send>) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic that carried no text".into())
}

/// Reap the tasks of `set` that have ended. A panic that reached one is said at error: an
/// operation's own panic is caught in its task ([`FcvmOps::spawn_op`]) and never gets here.
fn reap_ended(set: &mut tokio::task::JoinSet<()>) {
    while let Some(ended) = set.try_join_next() {
        match ended {
            Err(panic) if panic.is_panic() => {
                tracing::error!(%panic, "potato: a task of the ring panicked")
            }
            _ => {}
        }
    }
}

impl FcvmOps {
    /// A task that holds no VM and ends no operation (telling a player a pace).
    fn spawn(&self, f: impl std::future::Future<Output = ()> + Send + 'static) {
        let mut tasks = self.tasks.lock().unwrap();
        reap_ended(&mut tasks);
        tasks.spawn(f);
    }

    fn holder(&self) -> Handover {
        Handover {
            aborted: self.aborted.clone(),
            lost: Default::default(),
            panicked: Default::default(),
        }
    }

    /// Run the operation `op`, which reports its own end on `done`. If it panics instead, it
    /// has reported nothing: the driver would wait for it for ever, its player Booting,
    /// Waking or Sleeping with a CPU. So the panic is said at error, the VM the operation
    /// held (through `holder`) is ended as a failure path ends one, and `failed` makes the
    /// operation's failure, which goes to `done` once that VM is gone.
    fn spawn_op(
        &self,
        save: bool,
        what: String,
        holder: Handover,
        done: mpsc::UnboundedSender<Done>,
        failed: impl FnOnce(anyhow::Error, Exited) -> Done + Send + 'static,
        op: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        use futures_util::FutureExt;
        let task = async move {
            let mut run = Box::pin(std::panic::AssertUnwindSafe(op).catch_unwind());
            let Err(panic) = run.as_mut().await else {
                return;
            };
            let panic = panic_text(panic);
            tracing::error!(operation = %what, %panic,
                "potato: an operation panicked; ending its VM and failing the operation");
            // whatever the operation still holds goes to `lost` when its future is dropped
            holder
                .panicked
                .store(true, std::sync::atomic::Ordering::SeqCst);
            drop(run);
            let lost = std::mem::take(&mut *holder.lost.lock().unwrap_or_else(|e| e.into_inner()));
            // held again, as an operation that has not panicked: if the agent stops while
            // they are ended here, they go to the ring's stop
            let again = Handover {
                aborted: holder.aborted.clone(),
                lost: Default::default(),
                panicked: Default::default(),
            };
            let held: Vec<Held> = lost.into_iter().map(|vm| again.hold(vm)).collect();
            for vm in held {
                vm.ended(STOP_GRACE).await;
            }
            let _ = done.send(failed(
                anyhow::anyhow!("the {what} panicked: {panic}"),
                Exited::none(),
            ));
        };
        let mut set = if save { &self.saves } else { &self.tasks }.lock().unwrap();
        reap_ended(&mut set);
        set.spawn(task);
    }

    /// Abort every operation but the saves, wait until they have ended, and return the VMs
    /// they held.
    async fn end_aborted(&self) -> Vec<Vm> {
        let mut tasks = std::mem::take(&mut *self.tasks.lock().unwrap());
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        std::mem::take(&mut *self.aborted.lock().unwrap())
    }

    /// Wait for the saves in flight until `end`, abort those still running then (which kills
    /// their `fcvm snapshot create`), and return the VMs they held.
    async fn end_saves(&self, end: tokio::time::Instant) -> Vec<Vm> {
        let mut saves = std::mem::take(&mut *self.saves.lock().unwrap());
        let finished =
            tokio::time::timeout_at(end, async { while saves.join_next().await.is_some() {} })
                .await;
        if finished.is_err() {
            tracing::warn!(
                left = saves.len(),
                "potato: saves were still running when the ring's time to stop ran out; ending them"
            );
        }
        saves.abort_all();
        while saves.join_next().await.is_some() {}
        std::mem::take(&mut *self.aborted.lock().unwrap())
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

/// Wait for `port`, which only a running `vm` can answer. If the VM's fcvm exits first (a
/// refused preflight, a snapshot that is gone), fail at once with its exit status and last
/// lines instead of waiting out the port's deadline, and say so at warn: nothing else logs
/// why a machine never came up.
async fn while_running<T>(
    vm: &mut Vm,
    what: &str,
    port: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    let status = tokio::select! {
        answered = port => return answered,
        status = vm.exited() => status,
    };
    // its output is read by other tasks: give its last lines a moment to arrive
    let waited = Instant::now();
    while !vm.has_tail() && waited.elapsed() < TAIL_WAIT {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let tail = vm.tail();
    let last: Vec<&str> = tail.lines().rev().take(4).collect();
    let tail = last.into_iter().rev().collect::<Vec<_>>().join(" | ");
    tracing::warn!(machine = what, %status, %tail, "potato: fcvm exited while its machine's port was awaited");
    bail!("{what}'s fcvm exited ({status}) before its port answered: {tail}")
}

/// A failure before any fcvm was started.
fn unstarted(error: anyhow::Error) -> Failed {
    Failed {
        error,
        exited: Exited::none(),
    }
}

impl Ops for FcvmOps {
    /// Abort every operation but the saves; [`stop_ring`] ends those and the VMs of these.
    fn shutdown(&self) {
        self.tasks.lock().unwrap().abort_all();
    }

    /// The gold snapshot: an unassigned player, booted and put to sleep.
    fn make_gold(&self, done: mpsc::UnboundedSender<Done>) {
        let cfg = self.cfg.clone();
        let holder = self.holder();
        let failed = |error, exited| Done::Gold(Err(error), exited);
        let op = "making of the player snapshot".to_string();
        self.spawn_op(
            false,
            op,
            holder.clone(),
            done.clone(),
            failed,
            async move {
                let started = cfg.gold_args().and_then(|args| {
                    let addr: Ipv6Addr = args
                        [args.iter().position(|a| a == "--ipv6-address").unwrap() + 1]
                        .parse()?;
                    Ok((holder.hold(start_vm(&cfg, &args, GOLD)?), addr))
                });
                let (mut gold, addr) = match started {
                    Ok(x) => x,
                    Err(e) => {
                        let _ = done.send(Done::Gold(Err(e), Exited::none()));
                        return;
                    }
                };
                let listening =
                    while_running(&mut gold, GOLD, until_listening(addr, BOOT_DEADLINE)).await;
                let created = async {
                    listening?;
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
                let exited = gold.ended(STOP_GRACE).await;
                let _ = done.send(Done::Gold(created, exited));
            },
        );
    }

    fn boot(&self, n: u32, colours: u32, done: mpsc::UnboundedSender<Done>) {
        let cfg = self.cfg.clone();
        let slots = self.boot_slots.clone();
        let holder = self.holder();
        let failed = move |error, exited| Done::Booted(n, Err(Failed { error, exited }));
        let op = format!("boot of player {n}");
        self.spawn_op(
            false,
            op,
            holder.clone(),
            done.clone(),
            failed,
            async move {
                let r = async {
                    let _slot = slots.acquire().await.map_err(|e| unstarted(e.into()))?;
                    let addr = cfg.prefix.player_address(n).map_err(unstarted)?;
                    let args = cfg.restore_args(GOLD, n).map_err(unstarted)?;
                    let mut vm =
                        holder.hold(start_vm(&cfg, &args, &PotatoCfg::tag(n)).map_err(unstarted)?);
                    let assigned = while_running(&mut vm, &PotatoCfg::tag(n), async {
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
                    })
                    .await;
                    match assigned {
                        Ok(at) => Ok(Ready {
                            vm: Some(vm.into_vm()),
                            stages: BTreeMap::new(),
                            at,
                        }),
                        // the CPU it held is free only once its fcvm is gone
                        Err(error) => Err(Failed {
                            error,
                            exited: vm.ended(STOP_GRACE).await,
                        }),
                    }
                }
                .await;
                let _ = done.send(Done::Booted(n, r));
            },
        );
    }

    fn sleep(&self, n: u32, vm: Option<Vm>, stop: Option<u64>, done: mpsc::UnboundedSender<Done>) {
        let cfg = self.cfg.clone();
        let sleep_memory = self.sleep_memory.clone();
        let holder = self.holder();
        // held from before the save is a task: one aborted before it first runs hands it over too
        let vm = vm.map(|vm| holder.hold(vm));
        let failed = move |error, exited| Done::Slept(n, Err(error), 0.0, exited);
        let op = format!("save of player {n}");
        self.spawn_op(true, op, holder, done.clone(), failed, async move {
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
            let exited = vm.ended(STOP_GRACE).await;
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
        let holder = self.holder();
        let failed = move |error, exited| Done::Woke(n, Err(Failed { error, exited }), 0.0);
        let op = format!("wake of player {n}");
        self.spawn_op(
            false,
            op,
            holder.clone(),
            done.clone(),
            failed,
            async move {
                let started = Instant::now();
                let r = async {
                    let args = cfg.restore_args(&PotatoCfg::tag(n), n).map_err(unstarted)?;
                    let addr = cfg.prefix.player_address(n).map_err(unstarted)?;
                    let mut vm =
                        holder.hold(start_vm(&cfg, &args, &PotatoCfg::tag(n)).map_err(unstarted)?);
                    let listening = while_running(&mut vm, &PotatoCfg::tag(n), async {
                        let probed = until_listening(addr, WAKE_DEADLINE).await?;
                        // it catches only once told it woke: a report connection can fail for other
                        // reasons, and a probe answers before the gate's last half opens
                        let answer = ask(addr, &Inbound::Wake { round }, Duration::from_secs(5))
                            .await
                            .with_context(|| {
                                format!(
                                "the wake message, {:.1} ms after the ball port answered a probe",
                                ms(probed.elapsed())
                            )
                            })?;
                        if let Err(e) = fleet_potato::Reply::ok(&answer) {
                            bail!("player {n} refused its wake: {e}");
                        }
                        Ok(Instant::now())
                    })
                    .await;
                    match listening {
                        Ok(at) => {
                            let mut stages = BTreeMap::new();
                            if let Some(r) = vm.restored() {
                                stages.insert(
                                    "restore_ms".to_string(),
                                    ms(r.at.duration_since(started)),
                                );
                                if let Some(a) = r.ack_ms {
                                    stages.insert("ack_ms".to_string(), a as f64);
                                }
                            }
                            stages.insert("listen_ms".to_string(), ms(at.duration_since(started)));
                            Ok(Ready {
                                vm: Some(vm.into_vm()),
                                stages,
                                at,
                            })
                        }
                        // the CPU it held is free only once its fcvm is gone
                        Err(error) => Err(Failed {
                            error,
                            exited: vm.ended(STOP_GRACE).await,
                        }),
                    }
                }
                .await;
                let _ = done.send(Done::Woke(n, r, ms(started.elapsed())));
            },
        );
    }

    fn kick(&self, o: PotatoRound, done: mpsc::UnboundedSender<Done>) {
        let cfg = self.cfg.clone();
        let round = o.round;
        let failed = move |error, _exited| Done::Kicked(round, Err(error));
        let op = format!("first throw of round {round}");
        self.spawn_op(false, op, self.holder(), done.clone(), failed, async move {
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
    /// When this agent's potato mode started: the zero of the monotonic stamps it sends.
    epoch: Instant,
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
        // A wake's wait for a CPU is that wake's: it ends with the life the wake began, when
        // the player sleeps, is lost or is booted again. `wake` sets it for the next.
        if !matches!(phase, Phase::Waking | Phase::Awake) {
            p.cpu_waited_ms = None;
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
        let in_play = p.kicked;
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
        // While the ball is in play a boot takes a CPU only if another stays free: the next
        // catch needs one for a wake, and a boot can hold its CPU for as long as a restore is
        // given (a player whose sleep failed is booted again in the middle of a round).
        // Before the first throw the boots take every CPU: nothing is waiting to wake.
        for n in far.into_iter().chain(near) {
            if in_play && self.cpus.in_use() + 1 >= self.cpus.cap() {
                break;
            }
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
        self.players[n as usize].cpu_waited_ms = cpu_wait_ms;
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
                    let due = pl
                        .throw_due
                        .filter(|(s, ..)| *s == seq)
                        .map(|(_, at, _)| at);
                    pl.seq = if pl.caught { pl.seq.max(seq) } else { seq };
                    pl.caught = true;
                    if newest {
                        // its own throw begins once it has held the ball
                        let hold = hold_us.map_or_else(
                            || Duration::from_millis(pl.hold_ms()),
                            Duration::from_micros,
                        );
                        let flight = hold.min(Duration::from_millis(pl.order.flight_ms));
                        pl.throw_due = Some((seq + 1, read + hold, flight));
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
                        // `at` is the wall clock, which can step: laps are timed by this
                        mono_us: Some(read.saturating_duration_since(self.epoch).as_micros() as u64),
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
                let catcher = self.players.get(to as usize);
                // how long the catcher's wake has waited for a CPU (it is still asleep), or
                // waited before it started (it is waking, or has just woken)
                let catcher_cpu_wait_ms = catcher.and_then(|p| match p.phase {
                    Phase::Asleep => p.wanted_since.map(|since| ms(since.elapsed())),
                    Phase::Waking | Phase::Awake => p.cpu_waited_ms,
                    _ => None,
                });
                // A catcher that is awake by now was ready this long after the thrower's flight
                // window ended; 0 when it was ready between the thrower's last try and that end.
                let flight_end = self
                    .playing
                    .as_ref()
                    .filter(|pl| pl.order.round == round)
                    .and_then(|pl| pl.throw_due)
                    .filter(|(s, ..)| *s == seq)
                    .map(|(_, start, flight)| start + flight);
                let catcher_late_ms = catcher
                    .filter(|p| p.phase == Phase::Awake)
                    .and_then(|p| Some(signed_ms(p.ready_at?, flight_end?).max(0.0)));
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
                        catcher_late_ms,
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
            epoch: Instant::now(),
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
            saves: Default::default(),
            aborted: Default::default(),
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
        // the ball drops on player 3 while its wake runs: the drop carries the wait of that
        // wake, though the wait is over
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
                    catcher_late_ms,
                    ..
                } => Some((catcher_phase, catcher_cpu_wait_ms, catcher_late_ms)),
                _ => None,
            })
            .expect("the drop is reported");
        assert_eq!((dropped.0.as_str(), dropped.2), ("waking", None));
        assert_eq!(dropped.1, waits[0].1, "the wait its wake reported");
    }

    /// A drop read after the catcher's wake was reported finds it awake: the drop says how
    /// long after the end of the thrower's flight window the catcher was ready.
    #[test]
    fn a_drop_on_a_catcher_that_has_just_woken_says_how_late_it_was() {
        let late = |ready_after_ms: u64| {
            let (mut d, _rec, mut out) = asleep(2, 300.0);
            d.order(Some(round(1, vec![0, 1], 1000)));
            d.done(Done::Woke(0, Ok(ready()), 300.0));
            let base = Instant::now();
            // player 0 catches at `base`, keeps the ball 50 ms and then tries for 50 ms (its
            // hold, shorter than the round's 100 ms flight): its window ends 100 ms after
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
            d.done(Done::Woke(
                1,
                Ok(Ready {
                    vm: None,
                    stages: BTreeMap::new(),
                    at: base + Duration::from_millis(ready_after_ms),
                }),
                300.0,
            ));
            sent(&mut out);
            d.heard(heard(Report::Dropped {
                player: 0,
                round: 1,
                seq: 1,
                to: addr(1),
                attempts: 20,
                reason: "refused".into(),
            }));
            sent(&mut out)
                .into_iter()
                .find_map(|e| match e.kind {
                    PotatoKind::Dropped {
                        catcher_phase,
                        catcher_cpu_wait_ms,
                        catcher_late_ms,
                        ..
                    } => Some((catcher_phase, catcher_cpu_wait_ms, catcher_late_ms)),
                    _ => None,
                })
                .expect("the drop is reported")
        };
        assert_eq!(late(130), ("awake".into(), None, Some(30.0)));
        assert_eq!(
            late(90),
            ("awake".into(), None, Some(0.0)),
            "ready inside the window, after the thrower's last try"
        );
    }

    /// A player whose wake waited for a CPU and then failed is booted again from the gold. A
    /// drop on it afterwards reports no wait: the wait was the lost wake's, not this life's.
    #[test]
    fn a_player_booted_again_reports_no_wait_of_an_earlier_wake() {
        let (mut d, rec, mut out) = asleep(2, 300.0);
        d.order(Some(round(1, vec![0, 1], 1000)));
        d.done(Done::Woke(0, Ok(ready()), 300.0));
        // player 1's wake has waited 20 ms for a CPU when the catch asks for it again
        let Some(waiting_since) = Instant::now().checked_sub(Duration::from_millis(20)) else {
            return;
        };
        d.players[1].wanted_since = Some(waiting_since);
        d.heard(caught(0, 1, 0));
        assert_eq!(rec.take(), vec!["wake 0", "kick 1", "wake 1"]);
        assert!(d.players[1].cpu_waited_ms.is_some_and(|w| w >= 20.0));
        // the wake fails, and the player is restored from the gold
        d.done(Done::Woke(1, Err(unstarted(anyhow::anyhow!("gone"))), 0.0));
        d.advance();
        assert_eq!(rec.take(), vec!["boot 1"]);
        d.done(Done::Booted(1, Ok(ready())));
        assert_eq!(
            d.players[1].phase,
            Phase::Awake,
            "the next catcher stays awake"
        );
        sent(&mut out);
        d.heard(heard(Report::Dropped {
            player: 0,
            round: 1,
            seq: 1,
            to: addr(1),
            attempts: 3,
            reason: "refused".into(),
        }));
        let wait = sent(&mut out).into_iter().find_map(|e| match e.kind {
            PotatoKind::Dropped {
                catcher_phase,
                catcher_cpu_wait_ms,
                ..
            } => Some((catcher_phase, catcher_cpu_wait_ms)),
            _ => None,
        });
        assert_eq!(wait, Some(("awake".to_string(), None)));
    }

    /// A catch carries when the agent read it on its monotonic clock, counted from the
    /// agent's start: the wall clock in `at` can step.
    #[test]
    fn a_catch_carries_the_agents_monotonic_stamp() {
        let (mut d, _rec, mut out) = asleep(2, 300.0);
        d.order(Some(round(1, vec![0, 1], 1000)));
        d.done(Done::Woke(0, Ok(ready()), 300.0));
        sent(&mut out);
        let stamps: Vec<Option<u64>> = [250u64, 1250]
            .into_iter()
            .enumerate()
            .map(|(seq, after_ms)| {
                d.heard(Heard {
                    report: Report::Caught {
                        player: seq as u32,
                        round: 1,
                        seq: seq as u64,
                        proof: proof(),
                        hold_us: None,
                    },
                    at: 1,
                    read: d.epoch + Duration::from_millis(after_ms),
                });
                sent(&mut out)
                    .into_iter()
                    .find_map(|e| match e.kind {
                        PotatoKind::Caught { mono_us, .. } => Some(mono_us),
                        _ => None,
                    })
                    .expect("the catch is sent on")
            })
            .collect();
        assert_eq!(stamps, vec![Some(250_000), Some(1_250_000)]);
    }

    /// While the ball is in play a boot leaves a CPU for the next wake. A player whose sleep
    /// failed is booted again in the middle of a round, and a boot can hold its CPU for as
    /// long as a restore is given.
    #[test]
    fn a_boot_during_a_round_leaves_a_cpu_for_the_next_wake() {
        let (mut d, rec, _out) = driver_on(6, 3);
        d.gold_ready = true;
        for p in &mut d.players {
            p.phase = Phase::Asleep;
        }
        d.wakes.push_back((Instant::now(), 300.0));
        d.order(Some(round(1, (0..6).collect(), 200)));
        d.done(Done::Woke(0, Ok(ready()), 300.0));
        d.heard(caught(0, 1, 0));
        assert_eq!(rec.take(), vec!["wake 0", "kick 1", "wake 1"]);
        // player 5's snapshot is gone (its sleep failed): it has to be restored from the gold
        d.set(5, Phase::Missing);
        d.advance();
        assert_eq!(
            (rec.take(), d.cpus.in_use()),
            (Vec::<String>::new(), 2),
            "the one free CPU is the next wake's"
        );
        d.heard(threw(0, 1, 1, 1));
        d.done(Done::Slept(0, Ok(BTreeMap::new()), 300.0, Exited::none()));
        assert_eq!(
            rec.take(),
            vec!["sleep 0", "boot 5"],
            "with two free, one goes to the boot"
        );
        assert!(d.cpus.has_free());
    }

    /// A ring VM that ignores SIGTERM is killed within the ring's own wait when the agent
    /// stops, so it cannot hold the agent until systemd kills the whole unit.
    #[tokio::test]
    async fn a_ring_vm_that_ignores_sigterm_is_killed_within_the_rings_own_wait() {
        // one process, which ends by itself if this test does not end it
        let dir = tempfile::tempdir().unwrap();
        let (vm, pid) = stand_in(dir.path(), "trap '' TERM; : > \"$READY\"; exec sleep 20").await;
        let started = Instant::now();
        stop_ring(
            None,
            vec![vm],
            None,
            Duration::from_millis(300),
            Duration::from_millis(100),
        )
        .await;
        assert_eq!(
            nix::sys::signal::kill(pid, None),
            Err(nix::errno::Errno::ESRCH),
            "the VM is gone when the ring's stop returns"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    /// A stand-in for a VM's fcvm: `/bin/sh -c script`, returned once the script has made the
    /// file `$READY`, which it does after installing its trap. No test waits a fixed time for
    /// a shell to get that far: a SIGTERM sent before the trap ends the shell at once.
    async fn stand_in(dir: &std::path::Path, script: &str) -> (Vm, nix::unistd::Pid) {
        let ready = dir.join("vm.ready");
        let vm = Vm::spawn_args_in(
            std::path::Path::new("/bin/sh"),
            &["-c".to_string(), script.to_string()],
            "test",
            |cmd| {
                cmd.env("READY", &ready);
            },
        )
        .unwrap();
        written(&ready).await;
        let pid = nix::unistd::Pid::from_raw(vm.pid as i32);
        (vm, pid)
    }

    /// Wait until a stand-in has made `file`, and return what it holds.
    async fn written(file: &std::path::Path) -> String {
        for _ in 0..3000 {
            if let Ok(s) = std::fs::read_to_string(file) {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{} was not written", file.display());
    }

    /// A stand-in for fcvm in `dir`. `snapshot create` and `snapshot run` behave as `create`
    /// and `run` say (shell). A script calls `ready` once its trap is installed, which
    /// publishes its pid as `create.pid` or `run.pid` (written whole, by a rename). Every
    /// process it leaves ends by itself within 30 s.
    fn fake_fcvm(dir: &std::path::Path, create: &str, run: &str) -> Arc<FcvmOps> {
        use std::os::unix::fs::PermissionsExt;
        let fcvm = dir.join("fcvm");
        std::fs::write(
            &fcvm,
            format!(
                "#!/bin/sh\nd=$(dirname \"$0\")\n\
                 ready() {{ echo $$ > \"$d/$op.tmp\" && mv \"$d/$op.tmp\" \"$d/$op.pid\"; }}\n\
                 case \"$1 $2\" in\n\
                 'snapshot create') op=create; {create} ;;\n\
                 'snapshot run') op=run; {run} ;;\nesac\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fcvm, std::fs::Permissions::from_mode(0o755)).unwrap();
        Arc::new(FcvmOps {
            cfg: Arc::new(PotatoCfg {
                fcvm,
                image: "img".into(),
                players: 4,
                prefix: Prefix::parse("2001:db8::/80").unwrap(),
                mem_mib: 256,
                sleep_memory_dir: None,
                sleep_memory_cap_mib: 0,
                cpus: 8,
                cgroup: None,
            }),
            sleep_memory: None,
            boot_slots: Arc::new(Semaphore::new(BOOT_PARALLEL)),
            tasks: Default::default(),
            saves: Default::default(),
            aborted: Default::default(),
        })
    }

    /// A VM that is slow to stop: it exits 0.3 s after SIGTERM, taking its own child with it.
    /// Its trap is installed and its child started before it says it is ready.
    const SLOW_TO_STOP: &str =
        "trap 'sleep 0.3; kill $!; exit 0' TERM; sleep 30 & : > \"$READY\"; wait";

    /// The pid the fcvm stand-in published in `file` when its script called `ready`.
    async fn pid_in(file: &std::path::Path) -> nix::unistd::Pid {
        let pid = written(file).await;
        nix::unistd::Pid::from_raw(pid.trim().parse().unwrap())
    }

    fn gone(pid: nix::unistd::Pid) -> bool {
        nix::sys::signal::kill(pid, None) == Err(nix::errno::Errno::ESRCH)
    }

    /// A save still running when the ring's time to stop runs out is ended by the agent's own
    /// handles: its `fcvm snapshot create` is killed, and so is the VM it was saving, whether
    /// or not the ring has a cgroup. Both used to be left to run on without the agent.
    #[tokio::test]
    async fn a_save_that_outlives_the_rings_stop_is_killed_with_its_vm() {
        let dir = tempfile::tempdir().unwrap();
        let ops = fake_fcvm(dir.path(), "trap '' TERM; ready; exec sleep 30", "exit 0");
        let (vm, vm_pid) =
            stand_in(dir.path(), "trap '' TERM; : > \"$READY\"; exec sleep 30").await;
        let (tx, _rx) = mpsc::unbounded_channel();
        ops.sleep(0, Some(vm), None, tx);
        let create = pid_in(&dir.path().join("create.pid")).await;
        assert!(!gone(create) && !gone(vm_pid));
        let started = Instant::now();
        ops.shutdown();
        assert!(!gone(create), "a shutdown does not abort a save");
        stop_ring(
            Some(&ops),
            Vec::new(),
            None,
            Duration::from_millis(400),
            Duration::from_millis(200),
        )
        .await;
        assert!(
            started.elapsed() >= Duration::from_millis(400),
            "the save had the ring's time to finish"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(
            gone(vm_pid),
            "the VM being saved is gone when the stop returns"
        );
        // the command was killed when its save was aborted; tokio reaps it a moment later
        for _ in 0..200 {
            if gone(create) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            gone(create),
            "fcvm snapshot create does not run on without the agent"
        );
    }

    /// A save that finishes within the ring's time is left to finish: its snapshot is whole,
    /// and nothing is killed.
    #[tokio::test]
    async fn a_save_in_flight_finishes_within_the_rings_stop() {
        let dir = tempfile::tempdir().unwrap();
        let ops = fake_fcvm(
            dir.path(),
            "ready; sleep 0.3; echo saved > \"$d/saved\"",
            "exit 0",
        );
        // the VM stops by itself, as one saved with --stop does
        let vm = Vm::spawn_args(
            std::path::Path::new("/bin/sh"),
            &["-c".to_string(), "sleep 0.5".to_string()],
            "test",
        )
        .unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        ops.sleep(0, Some(vm), None, tx);
        pid_in(&dir.path().join("create.pid")).await;
        ops.shutdown();
        stop_ring(
            Some(&ops),
            Vec::new(),
            None,
            Duration::from_secs(10),
            Duration::from_millis(200),
        )
        .await;
        assert!(
            dir.path().join("saved").exists(),
            "the save was not cut short"
        );
        assert!(
            matches!(rx.try_recv(), Ok(Done::Slept(0, Ok(_), _, _))),
            "and it ended as a save does"
        );
    }

    /// A wake in flight when the agent stops is aborted, and the VM it had started is ended by
    /// its handle: SIGTERM, then SIGKILL. It used to get the SIGTERM alone, and an fcvm that
    /// was slow to go outlived the agent.
    #[tokio::test]
    async fn a_wake_in_flight_is_ended_by_its_handle_when_the_agent_stops() {
        let dir = tempfile::tempdir().unwrap();
        let ops = fake_fcvm(dir.path(), "exit 0", "trap '' TERM; ready; exec sleep 30");
        let (tx, _rx) = mpsc::unbounded_channel();
        ops.wake(1, None, tx);
        let run = pid_in(&dir.path().join("run.pid")).await;
        assert!(!gone(run));
        ops.shutdown();
        stop_ring(
            Some(&ops),
            Vec::new(),
            None,
            Duration::from_millis(300),
            Duration::from_millis(100),
        )
        .await;
        assert!(
            gone(run),
            "the waking VM's fcvm is gone when the stop returns"
        );
    }

    /// What the players' handles do not reach is killed through the ring's cgroup when it is
    /// still there after the grace.
    #[tokio::test]
    async fn a_ring_cgroup_still_populated_after_the_grace_is_killed() {
        let root = tempfile::tempdir().unwrap();
        let unit = root.path().join("system.slice/agent.service");
        std::fs::create_dir_all(unit.join("agent")).unwrap();
        std::fs::create_dir_all(unit.join("potato")).unwrap();
        std::fs::write(unit.join("cgroup.controllers"), "cpuset\n").unwrap();
        std::fs::write(unit.join("cpuset.cpus.effective"), "0-7\n").unwrap();
        std::fs::write(unit.join("potato/cpuset.cpus.effective"), "4-7\n").unwrap();
        let ring = RingCgroup::create(
            root.path(),
            std::path::Path::new("system.slice/agent.service/agent"),
            &|_| true,
            4,
        )
        .unwrap();
        let events = unit.join("potato/cgroup.events");
        let killed = unit.join("potato/cgroup.kill");
        // empty within the grace: nothing is killed
        std::fs::write(&events, "populated 0\nfrozen 0\n").unwrap();
        let wait = Duration::from_millis(100);
        stop_ring(None, Vec::new(), Some(&ring), wait, wait).await;
        assert!(!killed.exists());
        // a process that outlives the grace
        std::fs::write(&events, "populated 1\nfrozen 0\n").unwrap();
        stop_ring(None, Vec::new(), Some(&ring), wait, wait).await;
        assert_eq!(std::fs::read_to_string(&killed).unwrap(), "1");
    }

    /// The agent tells the ring to stop before the slots, and waits for every mode at once:
    /// the slots, the ring, the sandboxes and the servers. No mode's wait comes out of
    /// another's time to stop in.
    #[test]
    fn every_mode_is_told_to_stop_before_any_is_waited_for() {
        let agent = include_str!("mod.rs");
        let at = |what: &str| {
            agent
                .find(what)
                .unwrap_or_else(|| panic!("{what:?} is not in mod.rs"))
        };
        let told = at("drop(potato_tx);");
        let slots_told = at("\"coordinator closed the connection; stopping every VM\"");
        assert!(told < slots_told, "the ring is told first");
        let after = &agent[slots_told..];
        let joined = &after[after
            .find("tokio::join!(")
            .expect("the modes' stops are awaited together")..];
        let joined = &joined[..joined.find(");").unwrap()];
        for mode in [
            "slots_stopped",
            "ring_stopped",
            "sandboxes_stopped",
            "servers_stopped",
        ] {
            assert!(
                joined.contains(mode),
                "{mode} is not awaited with the others"
            );
        }
        for sequential in [
            "w.stop().await;\n    }\n    if",
            "let _ = t.await;\n    }\n    if",
        ] {
            assert!(
                !after.contains(sequential),
                "a mode is waited for before the next is told"
            );
        }
    }

    /// An operation that panics has reported nothing: its player would stay Booting, Waking
    /// or Sleeping with a CPU for ever, and its VM would run on. Its VM is ended, and then the
    /// operation fails as any other does: the player is lost, its CPU is back, and the next
    /// round restores it from the gold.
    #[tokio::test]
    async fn an_operation_that_panics_is_failed_once_its_vm_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let ops = fake_fcvm(dir.path(), "exit 0", "exit 0");
        let (vm, pid) = stand_in(dir.path(), SLOW_TO_STOP).await;
        let holder = ops.holder();
        let held = holder.hold(vm);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let failed = |error, exited| Done::Woke(1, Err(Failed { error, exited }), 0.0);
        ops.spawn_op(
            false,
            "wake of player 1".into(),
            holder,
            tx,
            failed,
            async move {
                let _vm = held;
                tokio::task::yield_now().await;
                panic!("the port answered nonsense");
            },
        );
        let done = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("an operation that panicked still ends")
            .unwrap();
        assert!(gone(pid), "its VM is gone when its failure is reported");
        let Done::Woke(1, Err(wake), _) = &done else {
            panic!("the panic is not the wake's failure");
        };
        let error = format!("{:#}", wake.error);
        assert!(
            error.contains("wake of player 1 panicked: the port answered nonsense"),
            "{error}"
        );
        // the driver takes it as any failed wake
        let (mut d, rec, _out) = asleep(2, 300.0);
        d.order(Some(round(1, vec![0, 1], 1000)));
        d.done(Done::Woke(0, Ok(ready()), 300.0));
        d.heard(caught(0, 1, 0));
        assert_eq!(d.players[1].phase, Phase::Waking);
        assert!(d.cpus.holds(Holder::Player(1)));
        d.done(done);
        assert_eq!(d.players[1].phase, Phase::Missing);
        assert!(!d.cpus.holds(Holder::Player(1)), "its CPU is back");
        d.order(None);
        rec.take();
        d.order(Some(round(2, vec![0, 1], 1000)));
        let next = rec.take();
        assert!(
            next.contains(&"boot 1".to_string()),
            "the game goes on: {next:?}"
        );
    }

    /// An operation aborted while it ends a VM (a wake that failed, as the agent stops) hands
    /// the VM to the ring's stop like any other it held.
    #[tokio::test]
    async fn an_operation_aborted_while_it_ends_its_vm_hands_the_vm_over() {
        let dir = tempfile::tempdir().unwrap();
        let ops = fake_fcvm(dir.path(), "exit 0", "exit 0");
        // it outlives SIGTERM and says when one has reached it; it ends by itself in 30 s
        let (vm, pid) = stand_in(
            dir.path(),
            "trap ': > \"$READY.term\"' TERM; : > \"$READY\"; \
             i=0; while [ $i -lt 300 ]; do sleep 0.1; i=$((i+1)); done",
        )
        .await;
        let held = ops.holder().hold(vm);
        let ending = tokio::spawn(async move {
            held.ended(Duration::from_secs(60)).await;
        });
        // the operation has sent its SIGTERM and now waits out its 60 s
        written(&dir.path().join("vm.ready.term")).await;
        ending.abort();
        let _ = ending.await;
        assert!(!gone(pid));
        assert_eq!(
            ops.aborted.lock().unwrap().len(),
            1,
            "the VM is the ring's stop's to end"
        );
        stop_ring(
            Some(&ops),
            Vec::new(),
            None,
            Duration::from_millis(300),
            Duration::from_millis(200),
        )
        .await;
        assert!(gone(pid));
    }

    /// The ring's stop returns by its one deadline, the grace and the wait after the kill,
    /// whatever has not ended: here a save that never comes back to the runtime, so aborting
    /// it does nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_rings_stop_returns_by_its_deadline_whatever_is_stuck() {
        let dir = tempfile::tempdir().unwrap();
        let ops = fake_fcvm(dir.path(), "exit 0", "exit 0");
        let (tx, _rx) = mpsc::unbounded_channel();
        let failed = |error, exited| Done::Slept(0, Err(error), 0.0, exited);
        let (running_tx, running) = tokio::sync::oneshot::channel();
        ops.spawn_op(
            true,
            "save of player 0".into(),
            ops.holder(),
            tx,
            failed,
            async move {
                let _ = running_tx.send(());
                std::thread::sleep(Duration::from_secs(3));
            },
        );
        running.await.unwrap();
        let started = Instant::now();
        stop_ring(
            Some(&ops),
            Vec::new(),
            None,
            Duration::from_millis(200),
            Duration::from_millis(200),
        )
        .await;
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(400), "{took:?}");
        assert!(
            took < Duration::from_millis(1500),
            "the stop waited for the stuck save: {took:?}"
        );
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
        let dir = tempfile::tempdir().unwrap();
        let (vm, pid) = stand_in(dir.path(), SLOW_TO_STOP).await;
        let ops = fake_fcvm(dir.path(), "exit 0", "exit 0");
        let _exited: Exited = ops.holder().hold(vm).ended(Duration::from_secs(10)).await;
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

    /// An fcvm that exits while the agent waits for its machine's port (a refused preflight, a
    /// snapshot that is gone) ends the operation at once, with its exit status and last
    /// lines, not when the 60 s or 600 s the port is given runs out.
    #[tokio::test]
    async fn an_fcvm_that_exits_fails_its_operation_at_once() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fcvm = dir.path().join("fcvm");
        std::fs::write(
            &fcvm,
            "#!/bin/sh\necho 'Error: routed mode preflight check failed' >&2\nexit 3\n",
        )
        .unwrap();
        std::fs::set_permissions(&fcvm, std::fs::Permissions::from_mode(0o755)).unwrap();
        let ops = FcvmOps {
            cfg: Arc::new(PotatoCfg {
                fcvm,
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
            boot_slots: Arc::new(Semaphore::new(BOOT_PARALLEL)),
            tasks: Default::default(),
            saves: Default::default(),
            aborted: Default::default(),
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        ops.make_gold(tx.clone());
        ops.boot(0, 12, tx.clone());
        ops.wake(1, None, tx.clone());
        let mut failed = Vec::new();
        for _ in 0..3 {
            let done = tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .expect("an operation ends when its fcvm does")
                .unwrap();
            failed.push(match done {
                Done::Gold(Err(e), _) => format!("gold: {e:#}"),
                Done::Booted(0, Err(f)) => format!("boot: {:#}", f.error),
                Done::Woke(1, Err(f), _) => format!("wake: {:#}", f.error),
                _ => panic!("an operation whose fcvm exited did not fail"),
            });
        }
        ops.shutdown();
        failed.sort();
        assert_eq!(failed.len(), 3);
        for (e, op) in failed.iter().zip(["boot", "gold", "wake"]) {
            assert!(e.starts_with(op), "{e}");
            assert!(
                e.contains("exit status: 3") && e.contains("preflight check failed"),
                "the failure names fcvm's exit and its last lines: {e}"
            );
        }
    }

    // ---- every interleaving ---------------------------------------------------------------

    /// What a search covers.
    #[derive(Debug, Clone, Copy)]
    struct Bound {
        players: u32,
        cpus: u32,
        /// Rounds after the first: at any point the coordinator may replace the round or end it.
        rounds: u32,
        /// Catches per round.
        catches: u64,
        /// The hold of every round, ms. With no wake seen the lead is 1650 / hold catches and
        /// one more, capped at `cpus - 2`: 200 puts it at the cap, 2000 at 2.
        hold_ms: u64,
        /// Start with the gold still to make, and let its making fail once.
        gold: bool,
        /// A new round may leave player 0 out of its ring.
        rings: bool,
    }

    /// One thing that can happen to the driver.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Ev {
        Gold(bool),
        /// The 10 s after a gold that failed are over.
        RetryGold,
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
        /// A new round: of every player, or (true) without player 0.
        NextRound(bool),
        NoRound,
    }

    /// The driver and the world around it: the operations it started and has not been told
    /// the end of, and where the ball is.
    struct World {
        bound: Bound,
        d: Driver,
        rec: Arc<Recorder>,
        ring: Vec<u32>,
        /// player -> `boot`, `wake` or `sleep`.
        ops: BTreeMap<u32, &'static str>,
        /// The gold: `making`, or `retry` while the driver waits to make it again.
        gold: Option<&'static str>,
        gold_failures_left: u32,
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
        /// An invariant an event broke as it was applied.
        broken: Option<String>,
    }

    impl World {
        fn new(bound: Bound) -> World {
            let (mut d, rec, out) = driver_on(bound.players, bound.cpus);
            std::mem::forget(out);
            let mut w = World {
                bound,
                rec,
                ring: Vec::new(),
                ops: BTreeMap::new(),
                gold: None,
                gold_failures_left: u32::from(bound.gold),
                round: 0,
                held: None,
                kicked: false,
                threw: Vec::new(),
                spent: Default::default(),
                stale: None,
                rounds_left: bound.rounds + 1,
                broken: None,
                d: {
                    if bound.gold {
                        d.make_gold();
                    } else {
                        d.gold_ready = true;
                    }
                    d
                },
            };
            w.started();
            w.apply(Ev::NextRound(false));
            w
        }

        fn phase(&self, n: u32) -> Phase {
            self.d.players[n as usize].phase
        }

        /// A copy that goes its own way: a new driver with this one's state.
        fn fork(&self) -> World {
            let (mut d, rec, out) = driver_on(self.bound.players, self.bound.cpus);
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
                bound: self.bound,
                d,
                rec,
                ring: self.ring.clone(),
                ops: self.ops.clone(),
                gold: self.gold,
                gold_failures_left: self.gold_failures_left,
                round: self.round,
                held: self.held,
                kicked: self.kicked,
                threw: self.threw.clone(),
                spent: self.spent.clone(),
                stale: self.stale,
                rounds_left: self.rounds_left,
                broken: None,
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

        fn enabled(&self) -> Vec<Ev> {
            let mut evs = Vec::new();
            match self.gold {
                Some("making") => {
                    evs.push(Ev::Gold(true));
                    if self.gold_failures_left > 0 {
                        evs.push(Ev::Gold(false));
                    }
                }
                Some(_) => evs.push(Ev::RetryGold),
                None => {}
            }
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
                if seq < self.bound.catches && self.may_accept(to) {
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
                evs.push(Ev::NextRound(false));
                if self.bound.rings {
                    evs.push(Ev::NextRound(true));
                }
                evs.push(Ev::NoRound);
            }
            evs
        }

        fn end_round(&mut self) {
            if let Some((seq, by)) = self.held.take() {
                self.stale = Some((by, self.round, seq + 1));
            }
            self.kicked = false;
            self.rounds_left = self.rounds_left.saturating_sub(1);
            self.threw.clear();
        }

        fn apply(&mut self, ev: Ev) {
            let fail = || unstarted(anyhow::anyhow!("failed"));
            match ev {
                Ev::Gold(ok) => {
                    let made = if ok {
                        self.gold = None;
                        Ok(())
                    } else {
                        self.gold = Some("retry");
                        self.gold_failures_left -= 1;
                        Err(anyhow::anyhow!("failed"))
                    };
                    self.d.done(Done::Gold(made, Exited::none()));
                }
                Ev::RetryGold => {
                    self.gold = None;
                    self.d.done(Done::RetryGold);
                }
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
                Ev::NextRound(without_first) => {
                    self.end_round();
                    self.round += 1;
                    self.ring = (u32::from(without_first)..self.bound.players).collect();
                    self.d.order(Some(round(
                        self.round,
                        self.ring.clone(),
                        self.bound.hold_ms,
                    )));
                }
                Ev::NoRound => {
                    self.end_round();
                    self.d.order(None);
                }
            }
            let booted = self.started();
            // a boot started while the ball of the round now played is in the air must leave
            // a CPU free
            let in_play = self.d.playing.as_ref().is_some_and(|p| p.kicked && !p.over);
            if booted && in_play && !self.d.cpus.has_free() {
                self.broken = Some("a boot took the last CPU while the ball was in play".into());
            }
        }

        /// Take what the driver has started into the world. True when it started a boot.
        fn started(&mut self) -> bool {
            let mut booted = false;
            for cmd in self.rec.take() {
                let last = cmd.rsplit(' ').next().unwrap();
                let op = match cmd.split(' ').next().unwrap() {
                    "gold" => {
                        assert!(self.gold.is_none(), "the gold is made twice at once");
                        self.gold = Some("making");
                        continue;
                    }
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
                booted |= op == "boot";
                assert!(
                    self.ops.insert(n, op).is_none(),
                    "{cmd}: player {n} already has an operation running"
                );
            }
            booted
        }

        /// What must hold after every event.
        fn check(&self) -> Result<(), String> {
            let d = &self.d;
            let cpus = self.bound.cpus;
            let ensure = |ok: bool, what: String| if ok { Ok(()) } else { Err(what) };
            if let Some(broken) = &self.broken {
                return Err(broken.clone());
            }
            ensure(d.cpus.in_use() <= cpus, "more machines than CPUs".into())?;
            let making = self.gold == Some("making");
            ensure(
                d.cpus.holds(Holder::Gold) == making,
                format!("the ledger and the gold ({:?}) disagree", self.gold),
            )?;
            ensure(
                d.gold_ready || self.ops.is_empty(),
                "a player was started before the gold was made".into(),
            )?;
            let mut not_asleep = u32::from(making);
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
            // and a CPU will come free for the next catcher: an operation or the gold is on
            // its way out, or the ball is held by a machine that sleeps once it has thrown
            if let Some(next) = lead.first().filter(|n| self.phase(**n) == Phase::Asleep) {
                ensure(
                    !self.ops.is_empty() || self.held.is_some() || making,
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
                .map(|p| {
                    (
                        p.phase.name(),
                        p.sleep_after_wake,
                        p.wanted_since.is_some(),
                        p.woken_for,
                    )
                })
                .collect();
            let playing = d
                .playing
                .as_ref()
                .map(|p| (p.order.round, p.seq, p.caught, p.kicked, p.over));
            format!(
                "{players:?} {playing:?} {:?} {:?} {:?} {} {:?} {:?} {:?} {:?} {} {} {:?} {} {} {}",
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
                self.gold,
                self.gold_failures_left,
                d.gold_ready,
                self.ring.len(),
            )
        }
    }

    /// Drive the driver through every order of events its state allows within `bound` (the
    /// gold made, failing and made again; ends of boots, wakes and sleeps, each succeeding or
    /// failing; catches, throws read late, drops; a round ended or replaced at any point, by
    /// one of another ring), and check the invariants after each. Returns the states seen.
    fn explore(bound: Bound) -> usize {
        let mut seen = std::collections::HashSet::new();
        // each state with the events that led to it, for the report of a broken invariant
        let mut queue = VecDeque::new();
        let start = World::new(bound);
        start.check().unwrap();
        seen.insert(start.key());
        queue.push_back((start, Vec::<Ev>::new()));
        while let Some((at, path)) = queue.pop_front() {
            for ev in at.enabled() {
                let mut next = at.fork();
                next.apply(ev);
                let mut longer = path.clone();
                longer.push(ev);
                if let Err(broken) = next.check() {
                    panic!("{bound:?}: {broken}, after {longer:?}");
                }
                if seen.insert(next.key()) {
                    queue.push_back((next, longer));
                }
            }
            assert!(
                seen.len() < 4_000_000,
                "{bound:?}: more than 4,000,000 states; the bound is too wide for a test"
            );
        }
        seen.len()
    }

    /// Search `bound` and require at least `at_least` states: a search that finds few has
    /// stopped searching.
    fn searched(bound: Bound, at_least: usize) {
        let states = explore(bound);
        assert!(states >= at_least, "{bound:?}: only {states} states");
    }

    /// No oversubscription, in every interleaving: machines not asleep never exceed the CPUs,
    /// the ledger agrees with what the driver started (the gold included), no CPU idles
    /// while a wake waits, nothing waits for a CPU that will not come free, and a boot
    /// started while the ball is in play leaves a CPU for the next wake.
    ///
    /// Here on 3 CPUs, the fewest allowed, with 4 players: the gold made, failing and made
    /// again, and a round that the coordinator replaces or ends at any point, by one whose
    /// ring leaves player 0 out. 339,876 states. The four tests below search other bounds;
    /// they are separate tests so that they run side by side.
    ///
    /// A runtime, because a gold that fails starts the driver's 10 s timer; nothing here
    /// waits for it.
    #[tokio::test]
    async fn machines_not_asleep_never_exceed_the_cpus_in_any_interleaving() {
        // POTATO_SEARCH=players,cpus,rounds,catches,hold,gold,rings[;...] searches other
        // bounds instead, for a deeper run than the suite has time for, and prints what it
        // found (run it with --no-capture). 4,3,1,3,200,1,1 is 2,660,624 states.
        if let Ok(spec) = std::env::var("POTATO_SEARCH") {
            for one in spec.split(';') {
                let b: Vec<u64> = one.split(',').map(|x| x.parse().unwrap()).collect();
                let started = Instant::now();
                let states = explore(Bound {
                    players: b[0] as u32,
                    cpus: b[1] as u32,
                    rounds: b[2] as u32,
                    catches: b[3],
                    hold_ms: b[4],
                    gold: b[5] != 0,
                    rings: b[6] != 0,
                });
                eprintln!(
                    "POTATO_SEARCH {one}: {states} states in {:?}",
                    started.elapsed()
                );
            }
            return;
        }
        searched(
            Bound {
                players: 4,
                cpus: 3,
                rounds: 1,
                catches: 2,
                hold_ms: 200,
                gold: true,
                rings: true,
            },
            300_000,
        );
    }

    /// One round, with every player there already and the gold made.
    fn one_round(players: u32, cpus: u32, catches: u64, hold_ms: u64) -> Bound {
        Bound {
            players,
            cpus,
            rounds: 0,
            catches,
            hold_ms,
            gold: false,
            rings: false,
        }
    }

    /// Deeper into a round on 3 CPUs: four catches. 109,807 states.
    #[tokio::test]
    async fn the_cap_holds_four_catches_into_a_round_on_3_cpus() {
        searched(one_round(4, 3, 4, 200), 100_000);
    }

    /// 4 CPUs, a lead of 2. 164,384 states.
    #[tokio::test]
    async fn the_cap_holds_on_4_cpus() {
        searched(one_round(4, 4, 4, 200), 150_000);
    }

    /// 5 CPUs for 6 players, the lead at its cap of 3. 156,766 states.
    #[tokio::test]
    async fn the_cap_holds_on_5_cpus_with_the_lead_at_its_cap() {
        let bound = one_round(6, 5, 3, 200);
        let w = World::new(bound);
        assert_eq!(
            w.d.lead_now(),
            3,
            "1650 ms of wake in 200 ms holds, capped at 5 - 2"
        );
        searched(bound, 140_000);
    }

    /// 5 CPUs for 6 players at a hold so long that the lead, 2, is below its cap of 3: the
    /// agent leaves a CPU it could use. 51,350 states.
    #[tokio::test]
    async fn the_cap_holds_on_5_cpus_with_the_lead_below_its_cap() {
        let bound = one_round(6, 5, 2, 2000);
        let w = World::new(bound);
        assert_eq!(
            w.d.lead_now(),
            2,
            "1650 ms of wake in 2000 ms holds, and one"
        );
        searched(bound, 45_000);
    }
}
