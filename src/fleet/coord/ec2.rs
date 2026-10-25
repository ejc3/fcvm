//! The EC2 half: a driver trait (launch an instance from the tile's launch template,
//! wait for its renderer, terminate it) and the reconciler that drives it from the
//! coordinator's EC2 assignment. The AWS implementation is a follow-up; [`FakeEc2`]
//! stands in for it locally and in tests.
//!
//! A relaunch launches the replacement first and terminates the old instance once the
//! launch is accepted, so termination stays out of the measured time. The timings it
//! reports: `launch_ms` (request to accepted), `ready_ms` (accepted to the renderer's
//! health), `total_ms`, and `capacity_wait_ms`: how long the tile's generation waited for
//! EC2 to have capacity before the launch that was accepted. The race scoreboard leaves
//! that wait out of the EC2 half's time and reports it on its own.

use super::Handle;
use anyhow::Result;
use async_trait::async_trait;
use fleet_core::protocol::{
    boot_id, AssignSlot, BudgetBody, HealthReport, RendererScene, SharedScene, SlotReport,
};
use fleet_core::Event;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// How long a tile waits before asking EC2 again after a capacity refusal.
const CAPACITY_RETRY: Duration = Duration::from_secs(10);

/// How often a running tile's instance is checked.
const LIVENESS_EVERY: Duration = Duration::from_secs(15);

/// How many checks in a row a running tile's renderer may leave unanswered before the tile
/// is launched again (45 s at [`LIVENESS_EVERY`]): one lost probe is not a crash.
const RENDERER_SILENT_LIMIT: u32 = 3;

/// How long the EC2 side waits before trying a failed sweep again.
const SWEEP_RETRY: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct Launched {
    pub instance: String,
    /// The renderer's frame URL, once the instance has an address.
    pub frame_url: Option<String>,
    /// Its public addresses, once known (for its DNS names).
    pub ipv6: Option<String>,
    pub ipv4: Option<String>,
}

/// What a liveness check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Liveness {
    Alive,
    /// The instance is gone (a spot reclaim), or its renderer reports that it failed: the
    /// tile is launched again.
    Dead(String),
    /// The instance runs and its renderer did not answer: after
    /// [`RENDERER_SILENT_LIMIT`] such checks in a row the tile is launched again.
    Silent(String),
}

#[derive(Debug)]
pub enum LaunchError {
    /// EC2 has no spot capacity for the tile's types and zones right now.
    NoCapacity(String),
    Failed(anyhow::Error),
}

#[async_trait]
pub trait Ec2Driver: Send + Sync {
    /// Launch one instance for `tile` from its launch template.
    async fn launch(&self, tile: &str) -> Result<Launched, LaunchError>;
    /// Resolves when the instance's renderer answers its health check, with what is known
    /// of the instance by then (its addresses and frame URL).
    async fn ready(&self, tile: &str, launched: &Launched) -> Result<Launched>;
    async fn terminate(&self, instance: &str) -> Result<()>;
    /// Post the renderer's scene body (its rectangle and the shared scene) to the tile.
    async fn post_scene(&self, tile: &str, launched: &Launched, body: &RendererScene)
        -> Result<()>;
    /// Post the grid budget to the tile's renderer (`POST /budget`).
    async fn post_budget(&self, tile: &str, launched: &Launched, body: &BudgetBody) -> Result<()>;
    /// The renderer's `/health` reply.
    async fn health(&self, tile: &str, launched: &Launched) -> Result<serde_json::Value>;
    /// How long a tile waits before asking again after a capacity refusal.
    fn capacity_retry(&self) -> Duration {
        CAPACITY_RETRY
    }
    /// Terminate every tile instance of this fleet, whoever launched it, and return how many
    /// there were. The coordinator is the only owner of tiles and keeps no instance ids
    /// across a restart, so it runs this before its first launch and again at shutdown:
    /// an instance an earlier coordinator left running is billed and nothing manages it.
    async fn sweep(&self) -> Result<usize> {
        Ok(0)
    }
    /// Whether the instance still exists (not shutting down, terminated, stopping or
    /// stopped) and its renderer still serves. A running tile is checked every
    /// [`Ec2Driver::liveness_every`]; one that is gone (a spot reclaim) or whose renderer
    /// stopped is launched again. An error is taken as unknown.
    async fn alive(&self, _tile: &str, _launched: &Launched) -> Result<Liveness> {
        Ok(Liveness::Alive)
    }
    fn liveness_every(&self) -> Duration {
        LIVENESS_EVERY
    }
}

/// Instances that exist only in memory: a launch is accepted after `launch_delay` and
/// ready after `ready_delay`. They have no renderer, so their tiles stay dark.
pub struct FakeEc2 {
    launch_delay: Duration,
    ready_delay: Duration,
    next: AtomicU64,
}

impl FakeEc2 {
    pub fn new(launch_delay: Duration, ready_delay: Duration) -> Self {
        FakeEc2 {
            launch_delay,
            ready_delay,
            next: AtomicU64::new(1),
        }
    }
}

#[async_trait]
impl Ec2Driver for FakeEc2 {
    async fn launch(&self, _tile: &str) -> Result<Launched, LaunchError> {
        tokio::time::sleep(self.launch_delay).await;
        Ok(Launched {
            instance: format!("fake-{}", self.next.fetch_add(1, Ordering::Relaxed)),
            frame_url: None,
            ipv6: None,
            ipv4: None,
        })
    }
    async fn ready(&self, _tile: &str, launched: &Launched) -> Result<Launched> {
        tokio::time::sleep(self.ready_delay).await;
        Ok(launched.clone())
    }
    async fn terminate(&self, _instance: &str) -> Result<()> {
        Ok(())
    }
    async fn post_scene(&self, _tile: &str, _l: &Launched, _body: &RendererScene) -> Result<()> {
        Ok(())
    }
    async fn post_budget(&self, _tile: &str, _l: &Launched, _body: &BudgetBody) -> Result<()> {
        Ok(())
    }
    async fn health(&self, _tile: &str, _l: &Launched) -> Result<serde_json::Value> {
        anyhow::bail!("a fake instance has no renderer")
    }
}

/// The running instance of each tile, for the scene poster.
/// The running instance of each tile, with the token of the tile task that put it there.
/// A tile that leaves the assignment and rejoins gets a new task while the old one can
/// still be waiting in `launch` or `ready`; the token keeps the old task from overwriting
/// or removing the new one's entry.
type Running = Arc<std::sync::Mutex<HashMap<String, (u64, Launched)>>>;

/// A distinct token for every tile task.
fn task_token() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Remove `id`'s entry if this task (`token`) put it there.
fn remove_own(running: &Running, id: &str, token: u64) {
    let mut r = running.lock().unwrap();
    if r.get(id).is_some_and(|(t, _)| *t == token) {
        r.remove(id);
    }
}

/// Post every running tile its renderer body whenever its rectangle, the layout or the
/// scene changes, and the grid budget after every scene and whenever it changes (retrying
/// a failed post once a second). Once a second it also reads each tile's health for the
/// budget (`Event::Ec2Health`). A `/health` whose `boot_id` differs from the one read
/// before means the renderer restarted on the same instance (its service restarts it) and
/// has lost its scene: the scene and the budget are posted again. The first scene post on
/// an instance reads `/health` first, so a restart between that read and any later one is
/// seen.
async fn post_scenes(
    driver: Arc<dyn Ec2Driver>,
    mut assign: watch::Receiver<Vec<AssignSlot>>,
    mut scene: watch::Receiver<Option<SharedScene>>,
    mut budget: watch::Receiver<Option<BudgetBody>>,
    running: Running,
    handle: Handle,
) {
    let mut posted: HashMap<String, (String, RendererScene)> = HashMap::new();
    let mut budget_posted: HashMap<String, (String, BudgetBody)> = HashMap::new();
    // Per tile: the instance and the renderer's boot id last read from its `/health`.
    let mut boots: HashMap<String, (String, Option<String>)> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut read_health = false;
    loop {
        let list = assign.borrow_and_update().clone();
        let shared = scene.borrow_and_update().clone();
        let grid = *budget.borrow_and_update();
        let now: Vec<(AssignSlot, Launched)> = {
            let r = running.lock().unwrap();
            list.into_iter()
                .filter_map(|a| r.get(&a.id).map(|(_, l)| (a, l.clone())))
                .collect()
        };
        if let Some(shared) = &shared {
            for (a, l) in now {
                let Some(rect) = a.rect else { continue };
                let known = boots.get(&a.id).is_some_and(|(i, _)| *i == l.instance);
                if read_health || !known {
                    match driver.health(&a.id, &l).await {
                        Ok(h) => {
                            let boot = boot_id(&h);
                            if known && boots[&a.id].1 != boot {
                                tracing::info!(tile = %a.id, instance = %l.instance, boot = ?boot, "the tile's renderer restarted; posting its scene again");
                                posted.remove(&a.id);
                                budget_posted.remove(&a.id);
                            }
                            boots.insert(a.id.clone(), (l.instance.clone(), boot));
                            if read_health {
                                let report = HealthReport::from_renderer(&a.id, &h);
                                handle.send(Event::Ec2Health { at: 0, report }).await;
                            }
                        }
                        // Unknown until a read succeeds: that read then counts as a
                        // restart, and the scene is posted again.
                        Err(_) if !known => {
                            boots.insert(a.id.clone(), (l.instance.clone(), None));
                        }
                        Err(_) => {}
                    }
                }
                let body = RendererScene {
                    layout_generation: a.layout_generation,
                    rect_from_frame: a.rect_from_frame,
                    previous_rect: a.previous_rect,
                    ..shared.for_renderer(rect)
                };
                if posted.get(&a.id) != Some(&(l.instance.clone(), body.clone())) {
                    match driver.post_scene(&a.id, &l, &body).await {
                        Ok(()) => {
                            posted.insert(a.id.clone(), (l.instance.clone(), body));
                            // A new scene clears the renderer's budget schedule.
                            budget_posted.remove(&a.id);
                        }
                        Err(e) => {
                            tracing::debug!(tile = %a.id, error = %format!("{e:#}"), "scene not posted yet");
                            continue;
                        }
                    }
                }
                if let Some(b) = grid {
                    if budget_posted.get(&a.id) != Some(&(l.instance.clone(), b)) {
                        match driver.post_budget(&a.id, &l, &b).await {
                            Ok(()) => {
                                budget_posted.insert(a.id.clone(), (l.instance.clone(), b));
                            }
                            Err(e) => {
                                tracing::debug!(tile = %a.id, error = %format!("{e:#}"), "budget not posted yet")
                            }
                        }
                    }
                }
            }
        }
        read_health = false;
        tokio::select! {
            r = assign.changed() => if r.is_err() { return },
            r = scene.changed() => if r.is_err() { return },
            r = budget.changed() => if r.is_err() { return },
            _ = tick.tick() => read_health = true,
        }
    }
}

/// Run one task per EC2 tile in the assignment; a tile that leaves the assignment has its
/// instance terminated.
pub async fn run(
    driver: Arc<dyn Ec2Driver>,
    mut assign: watch::Receiver<Vec<AssignSlot>>,
    scene: watch::Receiver<Option<SharedScene>>,
    budget: watch::Receiver<Option<BudgetBody>>,
    handle: Handle,
) {
    // No tile launches before the instances an earlier coordinator left are gone.
    loop {
        match driver.sweep().await {
            Ok(0) => break,
            Ok(n) => {
                tracing::info!(
                    instances = n,
                    "terminated tile instances left by an earlier coordinator"
                );
                break;
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "cannot sweep earlier tile instances; retrying");
                tokio::time::sleep(SWEEP_RETRY).await;
            }
        }
    }
    let running: Running = Arc::default();
    tokio::spawn(post_scenes(
        driver.clone(),
        assign.clone(),
        scene,
        budget,
        running.clone(),
        handle.clone(),
    ));
    let mut tiles: HashMap<String, watch::Sender<u64>> = HashMap::new();
    loop {
        let list = assign.borrow_and_update().clone();
        let want: BTreeMap<String, u64> = list.into_iter().map(|a| (a.id, a.generation)).collect();
        tiles.retain(|id, _| want.contains_key(id));
        for (id, generation) in want {
            match tiles.get(&id) {
                Some(tx) => {
                    tx.send_if_modified(|g| {
                        let changed = generation > *g;
                        *g = (*g).max(generation);
                        changed
                    });
                }
                None => {
                    let (tx, rx) = watch::channel(generation);
                    tokio::spawn(tile(
                        driver.clone(),
                        id.clone(),
                        rx,
                        handle.clone(),
                        running.clone(),
                    ));
                    tiles.insert(id, tx);
                }
            }
        }
        if assign.changed().await.is_err() {
            return;
        }
    }
}

/// Report the tile's phase, unless its task is retired: once the tile has left the
/// assignment (its sender is gone) a task for its return may already report on the same
/// slot, and this task's report would overwrite it.
#[allow(clippy::too_many_arguments)]
async fn report(
    handle: &Handle,
    want: &watch::Receiver<u64>,
    id: &str,
    phase: &str,
    generation: u64,
    l: Option<&Launched>,
    timings: &BTreeMap<String, u64>,
    // What went wrong, and when the tile tries again.
    problem: Option<(String, Option<Duration>)>,
) {
    if want.has_changed().is_err() {
        return;
    }
    let (error, retry_in) = match problem {
        Some((e, r)) => (Some(e), r),
        None => (None, None),
    };
    handle
        .send(Event::Ec2 {
            at: 0,
            report: SlotReport {
                id: id.to_string(),
                phase: phase.into(),
                generation,
                vm: l.map(|l| l.instance.clone()),
                frame_url: l.and_then(|l| l.frame_url.clone()),
                timings: timings.clone(),
                error,
                retry_in_ms: retry_in.map(|d| d.as_millis() as u64),
                ipv6: l.and_then(|l| l.ipv6.clone()),
                ipv4: l.and_then(|l| l.ipv4.clone()),
                // A tile serves TLS on 443 at its own IPv4 address.
                v4_port: None,
            },
        })
        .await;
}

async fn tile(
    driver: Arc<dyn Ec2Driver>,
    id: String,
    mut want: watch::Receiver<u64>,
    handle: Handle,
    running: Running,
) {
    let token = task_token();
    let mut current: Option<(Launched, u64)> = None;
    let mut retry = Duration::from_secs(1);
    // The generation that is waiting for capacity, and since when.
    let mut waiting: Option<(u64, Instant)> = None;
    loop {
        let generation = *want.borrow_and_update();
        if current.as_ref().is_none_or(|(_, g)| *g < generation) {
            remove_own(&running, &id, token);
            let t0 = Instant::now();
            let mut timings = BTreeMap::new();
            if waiting.is_some_and(|(g, _)| g != generation) {
                waiting = None;
            }
            if let Some((_, since)) = waiting {
                timings.insert(
                    "capacity_wait_ms".into(),
                    t0.duration_since(since).as_millis() as u64,
                );
            }
            report(
                &handle,
                &want,
                &id,
                "launching",
                generation,
                current.as_ref().map(|c| &c.0),
                &timings,
                None,
            )
            .await;
            match driver.launch(&id).await {
                Ok(l) => {
                    waiting = None;
                    timings.insert("launch_ms".into(), t0.elapsed().as_millis() as u64);
                    if let Some((old, _)) = current.take() {
                        let d = driver.clone();
                        tokio::spawn(async move {
                            if let Err(e) = d.terminate(&old.instance).await {
                                tracing::warn!(instance = %old.instance, error = %e, "terminate failed");
                            }
                        });
                    }
                    report(
                        &handle,
                        &want,
                        &id,
                        "starting",
                        generation,
                        Some(&l),
                        &timings,
                        None,
                    )
                    .await;
                    let accepted = Instant::now();
                    let l = match driver.ready(&id, &l).await {
                        // The tile left the assignment while its instance started (its
                        // sender is gone), and a task for its return may already own the
                        // entry: this instance is only terminated, below.
                        Ok(l) if want.has_changed().is_err() => l,
                        Ok(l) => {
                            running
                                .lock()
                                .unwrap()
                                .insert(id.clone(), (token, l.clone()));
                            timings
                                .insert("ready_ms".into(), accepted.elapsed().as_millis() as u64);
                            timings.insert("total_ms".into(), t0.elapsed().as_millis() as u64);
                            report(
                                &handle,
                                &want,
                                &id,
                                "running",
                                generation,
                                Some(&l),
                                &timings,
                                None,
                            )
                            .await;
                            retry = Duration::from_secs(1);
                            l
                        }
                        // The tile left while its instance started: only terminate it. A
                        // task for the tile's return may already run on the same slot.
                        Err(_) if want.has_changed().is_err() => {
                            if let Err(e) = driver.terminate(&l.instance).await {
                                tracing::warn!(instance = %l.instance, error = %e, "terminate failed");
                            }
                            break;
                        }
                        Err(e) => {
                            // The instance never served: terminate it and launch the
                            // generation again after the backoff.
                            let wait = retry;
                            retry = (retry * 2).min(Duration::from_secs(30));
                            report(
                                &handle,
                                &want,
                                &id,
                                "failed",
                                generation,
                                Some(&l),
                                &timings,
                                Some((format!("{e:#}"), Some(wait))),
                            )
                            .await;
                            if let Err(e) = driver.terminate(&l.instance).await {
                                tracing::warn!(instance = %l.instance, error = %e, "terminate failed");
                            }
                            if wait_or_removed(&mut want, wait).await {
                                break;
                            }
                            continue;
                        }
                    };
                    current = Some((l, generation));
                }
                Err(LaunchError::NoCapacity(why)) => {
                    let since = waiting.get_or_insert((generation, t0)).1;
                    timings.insert(
                        "capacity_wait_ms".into(),
                        since.elapsed().as_millis() as u64,
                    );
                    report(
                        &handle,
                        &want,
                        &id,
                        "waiting_capacity",
                        generation,
                        current.as_ref().map(|c| &c.0),
                        &timings,
                        Some((why, Some(driver.capacity_retry()))),
                    )
                    .await;
                    if wait_or_removed(&mut want, driver.capacity_retry()).await {
                        break;
                    }
                    continue;
                }
                Err(LaunchError::Failed(e)) => {
                    let wait = retry;
                    retry = (retry * 2).min(Duration::from_secs(30));
                    report(
                        &handle,
                        &want,
                        &id,
                        "failed",
                        generation,
                        current.as_ref().map(|c| &c.0),
                        &timings,
                        Some((format!("{e:#}"), Some(wait))),
                    )
                    .await;
                    if wait_or_removed(&mut want, wait).await {
                        break;
                    }
                    continue;
                }
            }
        }
        // Renderer checks left unanswered in a row.
        let mut silent = 0;
        let gone = loop {
            let check = tokio::time::sleep(driver.liveness_every());
            tokio::select! {
                r = want.changed() => {
                    if r.is_err() {
                        break None;
                    }
                    break Some(None);
                }
                _ = check => {
                    let Some((l, _)) = &current else { continue };
                    match driver.alive(&id, l).await {
                        Ok(Liveness::Alive) => silent = 0,
                        Ok(Liveness::Dead(why)) => break Some(Some(why)),
                        Ok(Liveness::Silent(why)) => {
                            silent += 1;
                            tracing::debug!(tile = %id, silent, error = %why, "tile renderer did not answer");
                            if silent >= RENDERER_SILENT_LIMIT {
                                break Some(Some(format!(
                                    "the renderer on {} did not answer {silent} checks in a row: {why}",
                                    l.instance
                                )));
                            }
                        }
                        Err(e) => tracing::debug!(tile = %id, error = %format!("{e:#}"), "tile liveness unknown"),
                    }
                }
            }
        };
        match gone {
            None => break,
            Some(None) => {}
            Some(Some(why)) => {
                // The instance went away under a running tile (a spot reclaim), or its
                // renderer stopped: report it, terminate the instance, and launch the same
                // generation again after the backoff.
                let (l, generation) = current.take().expect("checked a current instance");
                remove_own(&running, &id, token);
                let wait = retry;
                retry = (retry * 2).min(Duration::from_secs(30));
                report(
                    &handle,
                    &want,
                    &id,
                    "failed",
                    generation,
                    Some(&l),
                    &BTreeMap::new(),
                    Some((why, Some(wait))),
                )
                .await;
                if let Err(e) = driver.terminate(&l.instance).await {
                    tracing::warn!(instance = %l.instance, error = %e, "terminate failed");
                }
                if wait_or_removed(&mut want, wait).await {
                    break;
                }
            }
        }
    }
    remove_own(&running, &id, token);
    if let Some((l, _)) = current {
        if let Err(e) = driver.terminate(&l.instance).await {
            tracing::warn!(instance = %l.instance, error = %e, "terminate failed");
        }
    }
}

/// Sleep for `d`; true if the tile was removed meanwhile.
async fn wait_or_removed(want: &mut watch::Receiver<u64>, d: Duration) -> bool {
    let sleep = tokio::time::sleep(d);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => return false,
            r = want.changed() => if r.is_err() { return true },
        }
    }
}
