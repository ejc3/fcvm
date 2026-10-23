//! Race gym controller (`docs/race-gym.md`).
//!
//! Actors are processes started with `FCVM_FAILPOINT=*:park:<dir>`, so every failpoint they
//! hit announces itself in `<dir>/<actor>-<pid>/` and waits for a go file (`failpoint`'s
//! `park` module). The controller releases one parked hit at a time and waits for that
//! actor's next event: it parks again, it records that it waits on a lock another actor
//! holds, or its process exits. The order of releases is the schedule:
//!
//! * lock-step (discovery): release each actor's oldest hit in turn;
//! * pair: hold actor `a` at its `i`-th hit and `b` at its `j`-th, release one of them to
//!   its end, then the other;
//! * walk: PCT over seeded actor priorities, with `depth - 1` priority drops;
//! * replay: the steps of a recorded run, each of which must produce its recorded event.
//!
//! A step with no event inside the step deadline fails the schedule as an unmodeled wait,
//! never passes it.

#![allow(dead_code)]

use anyhow::{bail, Context, Result};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub mod cold_run;

pub const FORMAT: u32 = 1;

/// The scenario a replay names, rebuilt from its recorded parameters.
pub fn scenario_from(name: &str, params: &serde_json::Value) -> Result<Box<dyn Scenario>> {
    match name {
        cold_run::NAME => Ok(Box::new(cold_run::ColdRun::new(serde_json::from_value(
            params.clone(),
        )?)?)),
        other => bail!("unknown race gym scenario {other:?}"),
    }
}

/// Run a recorded replay; Err when it diverges or an oracle fails. The body of every case
/// `make race-gym EMIT=` adds to `tests/test_lifecycle_interleave.rs`.
pub async fn replay_case(json: &str) -> Result<()> {
    let replay: Replay = serde_json::from_str(json).context("parsing the replay")?;
    if replay.format != FORMAT {
        bail!("replay format {} (this gym reads {FORMAT})", replay.format);
    }
    become_subreaper()?;
    let scenario = scenario_from(&replay.scenario, &replay.params)?;
    let tag = format!("case{}", std::process::id());
    let root = std::env::temp_dir().join(format!("race-gym-{tag}"));
    let ctx = RunCtx {
        tag: tag.clone(),
        park_dir: root.join("park"),
        log_dir: root.clone(),
        slot: 0,
    };
    let result = run(
        scenario.as_ref(),
        &ctx,
        &Limits::default(),
        Plan::Replay(&replay.steps),
    )
    .await?;
    let answers_for = |f: &Failure| {
        f.oracle == "control"
            || replay.pin_oracles.is_empty()
            || replay.pin_oracles.contains(&f.oracle)
    };
    for f in result.failed.iter().filter(|f| !answers_for(f)) {
        println!("race gym case: not this case's oracle: {}", f.signature());
    }
    if result.failed.iter().any(answers_for) {
        bail!(
            "replay of {} ({:?}) failed: {:#?}\nlogs: {}",
            replay.scenario,
            replay.schedule,
            result.failed,
            root.display()
        );
    }
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

/// What one release produced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    /// The actor stopped at its next point (`point#hit`).
    Parked(String),
    /// The actor waits on a lock (the lock's path).
    Waiting(String),
    /// The actor's process exited with this code (-1: killed by a signal).
    Exited(i32),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub actor: usize,
    /// The hit released (`point#hit`).
    pub release: String,
    pub event: Event,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    Lockstep,
    Pair {
        a: usize,
        i: String,
        b: usize,
        j: String,
        first: usize,
    },
    Walk {
        seed: u64,
        depth: u32,
        steps_hint: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub oracle: String,
    pub actor: Option<usize>,
    pub detail: String,
}

impl Failure {
    pub fn new(oracle: &str, actor: Option<usize>, detail: impl Into<String>) -> Self {
        Self {
            oracle: oracle.to_string(),
            actor,
            detail: detail.into(),
        }
    }

    /// The failure with run-specific numbers and ids masked, so equal failures from
    /// different schedules group together.
    pub fn signature(&self) -> String {
        format!("{}: {}", self.oracle, mask(&self.detail))
    }
}

/// `text` with every run of hex digits that holds a decimal digit replaced by `#`: pids,
/// VM ids, content keys and timestamps differ between runs of one schedule.
pub fn mask(text: &str) -> String {
    let mut out = String::new();
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        let numeric =
            run.chars().all(|c| c.is_ascii_hexdigit()) && run.chars().any(|c| c.is_ascii_digit());
        if !run.is_empty() && numeric {
            out.push('#');
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

/// A recorded run: enough to rerun the same releases and check each step's event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replay {
    pub format: u32,
    pub scenario: String,
    pub fcvm_rev: String,
    pub params: serde_json::Value,
    pub schedule: Schedule,
    pub steps: Vec<Step>,
    pub failed: Vec<Failure>,
    /// The oracles an emitted case answers for (`make race-gym EMIT= PIN=outcome`): the
    /// case fails while one of them fails. Empty: every oracle. A divergence always fails.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pin_oracles: Vec<String>,
}

/// What a scenario provides: how to start each actor, and the oracles.
pub trait Scenario: Send + Sync {
    fn name(&self) -> &str;
    fn actors(&self) -> usize;
    /// Parameters recorded in a replay; `from_params` rebuilds the scenario from them.
    fn params(&self) -> serde_json::Value;
    /// The command for actor `idx`. The controller adds the park environment, the
    /// actor's log file, and the parent-death signal.
    fn command(&self, run: &RunCtx, idx: usize) -> Result<tokio::process::Command>;
    /// Checked at every step while the actors are stopped.
    fn step_oracles(&self, _run: &RunCtx) -> Vec<Failure> {
        Vec::new()
    }
    /// Checked after every actor has exited.
    fn end_oracles<'a>(
        &'a self,
        run: &'a RunCtx,
        exits: &'a [i32],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<Failure>> + Send + 'a>>;
}

/// Per-run paths and identity, handed to the scenario.
pub struct RunCtx {
    /// Distinguishes concurrent runs: actor names and content-key salts carry it.
    pub tag: String,
    pub park_dir: PathBuf,
    pub log_dir: PathBuf,
    /// Slot of this run among concurrent runs, for scenarios that hand out devices.
    pub slot: usize,
}

impl RunCtx {
    pub fn actor_log(&self, idx: usize) -> PathBuf {
        self.log_dir.join(format!("{}-a{idx}.log", self.tag))
    }
}

pub struct Limits {
    /// A release that produces no event within this fails the schedule.
    pub step_deadline: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        let secs = std::env::var("RACE_GYM_STEP_DEADLINE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(180);
        Self {
            step_deadline: Duration::from_secs(secs),
        }
    }
}

#[derive(Clone, Debug)]
struct Hit {
    label: String,
    go: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    /// Released and not heard from since.
    Running,
    Waiting,
    Exited(i32),
}

struct Actor {
    child: tokio::process::Child,
    /// Pid directories in the order they appeared; a hit from the n-th (n > 0) carries `@n`.
    pid_dirs: Vec<PathBuf>,
    seen: HashSet<PathBuf>,
    parked: Vec<Hit>,
    /// Events not yet consumed by a step, oldest first.
    pending: Vec<Event>,
    status: Status,
    trace: Vec<String>,
}

pub struct RunResult {
    pub steps: Vec<Step>,
    pub traces: Vec<Vec<String>>,
    pub failed: Vec<Failure>,
    /// For a pair schedule: whether both actors reached their targets.
    pub feasible: bool,
    pub elapsed: Duration,
}

fn parse_body(body: &str) -> BTreeMap<String, String> {
    body.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

struct Run<'a> {
    scenario: &'a dyn Scenario,
    ctx: &'a RunCtx,
    limits: &'a Limits,
    actors: Vec<Actor>,
    steps: Vec<Step>,
    failed: Vec<Failure>,
}

impl<'a> Run<'a> {
    async fn start(
        scenario: &'a dyn Scenario,
        ctx: &'a RunCtx,
        limits: &'a Limits,
    ) -> Result<Self> {
        std::fs::create_dir_all(&ctx.park_dir)?;
        std::fs::create_dir_all(&ctx.log_dir)?;
        let mut actors = Vec::new();
        for idx in 0..scenario.actors() {
            let mut cmd = scenario.command(ctx, idx)?;
            let log = std::fs::File::create(ctx.actor_log(idx))?;
            cmd.env(
                "FCVM_FAILPOINT",
                format!("*:park:{}", ctx.park_dir.display()),
            )
            .env("FCVM_RACE_ACTOR", format!("a{idx}"))
            .env("FCVM_RACE_PARENT", std::process::id().to_string())
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true);
            set_pdeathsig(&mut cmd);
            let child =
                spawn_recorded(&mut cmd).with_context(|| format!("spawning actor {idx}"))?;
            actors.push(Actor {
                child,
                pid_dirs: Vec::new(),
                seen: HashSet::new(),
                parked: Vec::new(),
                pending: Vec::new(),
                status: Status::Running,
                trace: Vec::new(),
            });
        }
        Ok(Self {
            scenario,
            ctx,
            limits,
            actors,
            steps: Vec::new(),
            failed: Vec::new(),
        })
    }

    /// Read every actor's new park files and exits into its pending events.
    fn scan(&mut self) -> Result<()> {
        let mut entries: Vec<PathBuf> = match std::fs::read_dir(&self.ctx.park_dir) {
            Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
            Err(_) => Vec::new(),
        };
        entries.sort();
        for (idx, actor) in self.actors.iter_mut().enumerate() {
            let prefix = format!("a{idx}-");
            for dir in &entries {
                let is_mine = dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix));
                if is_mine && !actor.pid_dirs.contains(dir) {
                    actor.pid_dirs.push(dir.clone());
                }
            }
            for (rank, dir) in actor.pid_dirs.clone().iter().enumerate() {
                let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| {
                        let n = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                        !n.starts_with('.') && (n.ends_with(".reached") || n.ends_with(".note"))
                    })
                    .collect();
                files.sort();
                for file in files {
                    if !actor.seen.insert(file.clone()) {
                        continue;
                    }
                    let body = parse_body(&std::fs::read_to_string(&file)?);
                    let name = file.file_name().unwrap().to_str().unwrap().to_string();
                    let seq = name.split('.').next().unwrap();
                    if name.ends_with(".reached") {
                        let mut label = format!(
                            "{}#{}",
                            body.get("point").context("reached file has no point")?,
                            body.get("hit").context("reached file has no hit")?
                        );
                        if rank > 0 {
                            label.push_str(&format!("@{rank}"));
                        }
                        actor.parked.push(Hit {
                            label: label.clone(),
                            go: dir.join(format!("{seq}.go")),
                        });
                        actor.trace.push(label.clone());
                        actor.pending.push(Event::Parked(label));
                    } else if body.get("note").map(String::as_str) == Some("lock.wait") {
                        // The lock's last two path components, ids masked: the path names
                        // this run's content key and VM ids, which a replay does not share.
                        let detail = body.get("detail").cloned().unwrap_or_default();
                        let parts: Vec<&str> = detail.rsplit('/').take(2).collect();
                        let short = parts.into_iter().rev().collect::<Vec<_>>().join("/");
                        actor.pending.push(Event::Waiting(mask(&short)));
                    }
                }
            }
            if !matches!(actor.status, Status::Exited(_)) {
                if let Some(status) = actor.child.try_wait()? {
                    let code = status.code().unwrap_or(-1);
                    // An exit is the last event; park files written before it come first.
                    actor.pending.push(Event::Exited(code));
                    actor.status = Status::Exited(code);
                }
            }
        }
        Ok(())
    }

    fn release(&mut self, idx: usize, label: &str) -> Result<()> {
        let actor = &mut self.actors[idx];
        let pos = actor
            .parked
            .iter()
            .position(|h| h.label == label)
            .with_context(|| format!("actor {idx} is not parked at {label}"))?;
        let hit = actor.parked.remove(pos);
        std::fs::write(&hit.go, b"").with_context(|| format!("writing {}", hit.go.display()))?;
        // Drop the released hit's own park and any earlier lock wait (the actor has parked
        // since, so it is over). Keep other parks: an actor with two tasks can already be
        // parked at its next hit, and that hit is the step's event when the released task
        // parks no more (the health monitor beside the main task).
        actor.pending.retain(|e| {
            matches!(e, Event::Parked(l) if *l != label) || matches!(e, Event::Exited(_))
        });
        if !matches!(actor.status, Status::Exited(_)) {
            actor.status = Status::Running;
        }
        Ok(())
    }

    /// Wait for actor `idx`'s first event after its release.
    async fn wait_event(&mut self, idx: usize) -> Result<Event> {
        let deadline = Instant::now() + self.limits.step_deadline;
        loop {
            self.scan()?;
            let actor = &mut self.actors[idx];
            if !actor.pending.is_empty() {
                let event = actor.pending.remove(0);
                if let Event::Waiting(_) = event {
                    if !matches!(actor.status, Status::Exited(_)) {
                        actor.status = Status::Waiting;
                    }
                }
                return Ok(event);
            }
            if Instant::now() >= deadline {
                bail!(
                    "UNMODELED WAIT: actor {idx} produced no event within {:?} of its release \
                     (no park, no lock-wait note, no exit); log: {}",
                    self.limits.step_deadline,
                    self.ctx.actor_log(idx).display()
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Nobody can be released: wait until any actor that is not exited parks or exits.
    async fn wait_any(&mut self) -> Result<()> {
        let deadline = Instant::now() + self.limits.step_deadline;
        loop {
            self.scan()?;
            let progressed = self.actors.iter().any(|a| !a.parked.is_empty())
                || self
                    .actors
                    .iter()
                    .all(|a| matches!(a.status, Status::Exited(_)));
            if progressed {
                return Ok(());
            }
            if Instant::now() >= deadline {
                let states: Vec<String> = self
                    .actors
                    .iter()
                    .enumerate()
                    .map(|(i, a)| format!("a{i}={:?}", a.status))
                    .collect();
                bail!(
                    "UNMODELED WAIT: no actor parked or exited within {:?} ({})",
                    self.limits.step_deadline,
                    states.join(" ")
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn all_exited(&self) -> bool {
        self.actors
            .iter()
            .all(|a| matches!(a.status, Status::Exited(_)))
    }

    /// Release `label` of actor `idx`, record the step, and run the step oracles.
    async fn step(&mut self, idx: usize, label: &str) -> Result<Event> {
        self.release(idx, label)?;
        let event = self.wait_event(idx).await?;
        self.steps.push(Step {
            actor: idx,
            release: label.to_string(),
            event: event.clone(),
        });
        for failure in self.scenario.step_oracles(self.ctx) {
            if !self.failed.contains(&failure) {
                self.failed.push(failure);
            }
        }
        Ok(event)
    }

    fn oldest(&self, idx: usize) -> Option<String> {
        self.actors[idx].parked.first().map(|h| h.label.clone())
    }

    fn is_parked_at(&self, idx: usize, label: &str) -> bool {
        self.actors[idx].parked.iter().any(|h| h.label == label)
    }

    /// Lock-step: release the oldest hit of the next actor (after `cursor`) that has one,
    /// skipping `held` actors. Returns false when none could be released.
    async fn lockstep_once(&mut self, cursor: &mut usize, held: &[usize]) -> Result<bool> {
        let n = self.actors.len();
        for k in 0..n {
            let idx = (*cursor + k) % n;
            if held.contains(&idx) {
                continue;
            }
            if let Some(label) = self.oldest(idx) {
                *cursor = (idx + 1) % n;
                self.step(idx, &label).await?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn finish_lockstep(&mut self, cursor: &mut usize) -> Result<()> {
        loop {
            self.scan()?;
            if self.all_exited() {
                return Ok(());
            }
            if !self.lockstep_once(cursor, &[]).await? {
                self.wait_any().await?;
            }
        }
    }

    async fn run_pair(
        &mut self,
        a: usize,
        i: &str,
        b: usize,
        j: &str,
        first: usize,
    ) -> Result<bool> {
        let mut cursor = 0;
        let target = |idx: usize| if idx == a { i } else { j };
        // Prefix: lock-step, holding each pair actor once it is parked at its target.
        loop {
            self.scan()?;
            let mut held = Vec::new();
            for idx in [a, b] {
                if self.is_parked_at(idx, target(idx)) {
                    held.push(idx);
                }
            }
            if held.len() == 2 {
                break;
            }
            // An actor that exited without reaching its target makes the pair infeasible.
            let gone = [a, b].iter().any(|&idx| {
                matches!(self.actors[idx].status, Status::Exited(_))
                    && !self.is_parked_at(idx, target(idx))
            });
            if gone {
                self.finish_lockstep(&mut cursor).await?;
                return Ok(false);
            }
            if !self.lockstep_once(&mut cursor, &held).await? {
                let blocked = self
                    .actors
                    .iter()
                    .enumerate()
                    .all(|(idx, act)| held.contains(&idx) || act.parked.is_empty());
                let waiting_on_held = blocked
                    && self.actors.iter().enumerate().all(|(idx, act)| {
                        held.contains(&idx)
                            || matches!(act.status, Status::Waiting | Status::Exited(_))
                    });
                if waiting_on_held {
                    // Everyone else waits on a lock a held actor owns: the targets cannot
                    // both be reached in this order.
                    self.finish_lockstep(&mut cursor).await?;
                    return Ok(false);
                }
                self.wait_any().await?;
            }
        }
        // Release `first` and run it alone until it exits or waits on the other.
        let second = if first == a { b } else { a };
        self.step(first, target(first)).await?;
        loop {
            self.scan()?;
            match self.actors[first].status {
                Status::Exited(_) | Status::Waiting => break,
                Status::Running => {}
            }
            match self.oldest(first) {
                Some(label) => {
                    self.step(first, &label).await?;
                }
                None => {
                    // Running without a parked hit: another task of the actor is still
                    // going. Wait for it.
                    self.wait_any().await?;
                    if self.actors[first].parked.is_empty()
                        && !matches!(self.actors[first].status, Status::Exited(_))
                    {
                        break;
                    }
                }
            }
        }
        self.step(second, target(second)).await?;
        self.finish_lockstep(&mut cursor).await?;
        Ok(true)
    }

    async fn run_walk(&mut self, seed: u64, depth: u32, steps_hint: usize) -> Result<()> {
        let mut rng = SmallRng::seed_from_u64(seed);
        let n = self.actors.len();
        // Distinct priorities: a seeded permutation of depth..depth+n (PCT keeps 1..depth for
        // the drops).
        let mut priority: Vec<i64> = (0..n as i64).map(|p| p + depth as i64).collect();
        for k in (1..n).rev() {
            let s = rng.gen_range(0..=k);
            priority.swap(k, s);
        }
        let hint = steps_hint.max(1);
        let mut drops: Vec<usize> = (1..depth).map(|_| rng.gen_range(1..=hint)).collect();
        drops.sort_unstable();
        let mut next_low = depth as i64 - 1;
        let mut count = 0usize;
        loop {
            self.scan()?;
            if self.all_exited() {
                return Ok(());
            }
            let pick = (0..n)
                .filter(|&idx| !self.actors[idx].parked.is_empty())
                .max_by_key(|&idx| priority[idx]);
            let Some(idx) = pick else {
                self.wait_any().await?;
                continue;
            };
            let label = self.oldest(idx).unwrap();
            self.step(idx, &label).await?;
            count += 1;
            while drops.first() == Some(&count) {
                drops.remove(0);
                priority[idx] = next_low;
                next_low -= 1;
            }
        }
    }

    async fn run_replay(&mut self, recorded: &[Step]) -> Result<()> {
        for (n, want) in recorded.iter().enumerate() {
            let deadline = Instant::now() + self.limits.step_deadline;
            loop {
                self.scan()?;
                if self.is_parked_at(want.actor, &want.release) {
                    break;
                }
                if Instant::now() >= deadline
                    || matches!(self.actors[want.actor].status, Status::Exited(_))
                {
                    bail!(
                        "REPLAY DIVERGED at step {n}: actor {} never parked at {} (parked: {:?}, status {:?})",
                        want.actor,
                        want.release,
                        self.actors[want.actor].parked.iter().map(|h| &h.label).collect::<Vec<_>>(),
                        self.actors[want.actor].status
                    );
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let event = self.step(want.actor, &want.release).await?;
            if event != want.event {
                bail!(
                    "REPLAY DIVERGED at step {n}: releasing actor {} at {} gave {:?}, recorded {:?}",
                    want.actor,
                    want.release,
                    event,
                    want.event
                );
            }
        }
        self.scan()?;
        if !self.all_exited() {
            bail!("REPLAY DIVERGED after the last recorded step: some actors have not exited");
        }
        Ok(())
    }

    /// Kill what is left after an error, so an aborted schedule leaves nothing running.
    async fn abort(&mut self) {
        for actor in &mut self.actors {
            if matches!(actor.status, Status::Exited(_)) {
                continue;
            }
            for hit in actor.parked.drain(..) {
                let _ = std::fs::write(&hit.go, b"");
            }
            let _ = actor.child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(60), actor.child.wait()).await;
        }
    }
}

fn set_pdeathsig(cmd: &mut tokio::process::Command) {
    // SAFETY: prctl is async-signal-safe; runs in the child between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Make this process the reaper of anything an actor orphans, so the leak oracle can find
/// it as a child of ours.
pub fn become_subreaper() -> Result<()> {
    // SAFETY: plain prctl on this process.
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) } != 0 {
        bail!(
            "PR_SET_CHILD_SUBREAPER: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

/// Every process the gym started itself (actors, and a scenario's own commands). Another
/// child of this process was orphaned by one of them and reparented here.
static SPAWNED: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

/// Spawn a process the gym starts itself and record it, under one hold of the registry
/// lock, so [`orphans`] (which holds the lock for its scan) never sees the process
/// unrecorded.
pub fn spawn_recorded(cmd: &mut tokio::process::Command) -> std::io::Result<tokio::process::Child> {
    let mut spawned = SPAWNED.lock().unwrap();
    let child = cmd.spawn()?;
    if let Some(pid) = child.id() {
        spawned.push(pid);
    }
    Ok(child)
}

/// Live (non-zombie) processes whose parent is this process and that the gym did not start
/// itself: orphans an actor left. Concurrent runs share this process, so a run's orphan can
/// be reported by another run; a serial replay settles which schedule it belongs to.
pub fn orphans() -> Vec<String> {
    let spawned = SPAWNED.lock().unwrap();
    let me = std::process::id().to_string();
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in rd.flatten() {
        let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else {
            continue;
        };
        let field = |k: &str| {
            status
                .lines()
                .find_map(|l| l.strip_prefix(k).map(|v| v.trim().to_string()))
                .unwrap_or_default()
        };
        let pid: u32 = field("Pid:").parse().unwrap_or(0);
        if field("PPid:") == me && !field("State:").starts_with('Z') && !spawned.contains(&pid) {
            out.push(format!("{} ({})", field("Pid:"), field("Name:")));
        }
    }
    out
}

pub enum Plan<'p> {
    Schedule(&'p Schedule),
    Replay(&'p [Step]),
}

/// Run one schedule to the end and evaluate the oracles.
pub async fn run(
    scenario: &dyn Scenario,
    ctx: &RunCtx,
    limits: &Limits,
    plan: Plan<'_>,
) -> Result<RunResult> {
    let started = Instant::now();
    let mut run = Run::start(scenario, ctx, limits).await?;
    let mut feasible = true;
    let driven: Result<()> = async {
        match plan {
            Plan::Schedule(Schedule::Lockstep) => run.finish_lockstep(&mut 0).await,
            Plan::Schedule(Schedule::Pair { a, i, b, j, first }) => {
                feasible = run.run_pair(*a, i, *b, j, *first).await?;
                Ok(())
            }
            Plan::Schedule(Schedule::Walk {
                seed,
                depth,
                steps_hint,
            }) => run.run_walk(*seed, *depth, *steps_hint).await,
            Plan::Replay(steps) => run.run_replay(steps).await,
        }
    }
    .await;
    if let Err(e) = driven {
        run.abort().await;
        let mut failed = run.failed.clone();
        failed.push(Failure::new("control", None, format!("{e:#}")));
        return Ok(RunResult {
            steps: run.steps,
            traces: run.actors.iter().map(|a| a.trace.clone()).collect(),
            failed,
            feasible,
            elapsed: started.elapsed(),
        });
    }
    let exits: Vec<i32> = run
        .actors
        .iter()
        .map(|a| match a.status {
            Status::Exited(c) => c,
            _ => -1,
        })
        .collect();
    let mut failed = run.failed.clone();
    failed.extend(scenario.end_oracles(ctx, &exits).await);
    Ok(RunResult {
        steps: run.steps,
        traces: run.actors.iter().map(|a| a.trace.clone()).collect(),
        failed,
        feasible,
        elapsed: started.elapsed(),
    })
}

/// Every pair schedule over two actors' traces, both release orders.
pub fn pair_schedules(trace_a: &[String], trace_b: &[String]) -> Vec<Schedule> {
    let mut out = Vec::new();
    for i in trace_a {
        for j in trace_b {
            for first in [0, 1] {
                out.push(Schedule::Pair {
                    a: 0,
                    i: i.clone(),
                    b: 1,
                    j: j.clone(),
                    first,
                });
            }
        }
    }
    out
}

pub fn git_rev() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// The last lines of a log that look like an error, for an outcome failure.
pub fn error_lines(log: &Path, n: usize) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("Error") || l.contains("ERROR"))
        .collect();
    let start = lines.len().saturating_sub(n);
    lines[start..]
        .iter()
        .map(|l| {
            // Drop the timestamp and level, keep the message, and bound its length.
            let msg = l.split_once(": ").map(|(_, m)| m).unwrap_or(l);
            msg.chars().take(400).collect::<String>()
        })
        .collect::<Vec<_>>()
        .join(" | ")
}
