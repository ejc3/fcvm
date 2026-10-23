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
use fleet_core::protocol::{AssignSlot, RendererScene, SharedScene, SlotReport};
use fleet_core::Event;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// How long a tile waits before asking EC2 again after a capacity refusal.
const CAPACITY_RETRY: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct Launched {
    pub instance: String,
    /// The renderer's frame URL, once the instance has an address.
    pub frame_url: Option<String>,
    /// Its public addresses, once known (for its DNS names).
    pub ipv6: Option<String>,
    pub ipv4: Option<String>,
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
    /// How long a tile waits before asking again after a capacity refusal.
    fn capacity_retry(&self) -> Duration {
        CAPACITY_RETRY
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
}

/// The running instance of each tile, for the scene poster.
type Running = Arc<std::sync::Mutex<HashMap<String, Launched>>>;

/// Post every running tile its renderer body whenever its rectangle, the layout or the
/// scene changes (and retry a failed post once a second).
async fn post_scenes(
    driver: Arc<dyn Ec2Driver>,
    mut assign: watch::Receiver<Vec<AssignSlot>>,
    mut scene: watch::Receiver<Option<SharedScene>>,
    running: Running,
) {
    let mut posted: HashMap<String, (String, RendererScene)> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        let list = assign.borrow_and_update().clone();
        let shared = scene.borrow_and_update().clone();
        let now: Vec<(AssignSlot, Launched)> = {
            let r = running.lock().unwrap();
            list.into_iter()
                .filter_map(|a| r.get(&a.id).cloned().map(|l| (a, l)))
                .collect()
        };
        if let Some(shared) = &shared {
            for (a, l) in now {
                let Some(rect) = a.rect else { continue };
                let body = RendererScene {
                    layout_generation: a.layout_generation,
                    rect_from_frame: a.rect_from_frame,
                    previous_rect: a.previous_rect,
                    ..shared.for_renderer(rect)
                };
                if posted.get(&a.id) == Some(&(l.instance.clone(), body.clone())) {
                    continue;
                }
                match driver.post_scene(&a.id, &l, &body).await {
                    Ok(()) => {
                        posted.insert(a.id.clone(), (l.instance.clone(), body));
                    }
                    Err(e) => {
                        tracing::debug!(tile = %a.id, error = %format!("{e:#}"), "scene not posted yet")
                    }
                }
            }
        }
        tokio::select! {
            r = assign.changed() => if r.is_err() { return },
            r = scene.changed() => if r.is_err() { return },
            _ = tick.tick() => {}
        }
    }
}

/// Run one task per EC2 tile in the assignment; a tile that leaves the assignment has its
/// instance terminated.
pub async fn run(
    driver: Arc<dyn Ec2Driver>,
    mut assign: watch::Receiver<Vec<AssignSlot>>,
    scene: watch::Receiver<Option<SharedScene>>,
    handle: Handle,
) {
    let running: Running = Arc::default();
    tokio::spawn(post_scenes(
        driver.clone(),
        assign.clone(),
        scene,
        running.clone(),
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

async fn report(
    handle: &Handle,
    id: &str,
    phase: &str,
    generation: u64,
    l: Option<&Launched>,
    timings: &BTreeMap<String, u64>,
    // What went wrong, and when the tile tries again.
    problem: Option<(String, Option<Duration>)>,
) {
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
    let mut current: Option<(Launched, u64)> = None;
    let mut retry = Duration::from_secs(1);
    // The generation that is waiting for capacity, and since when.
    let mut waiting: Option<(u64, Instant)> = None;
    loop {
        let generation = *want.borrow_and_update();
        if current.as_ref().is_none_or(|(_, g)| *g < generation) {
            running.lock().unwrap().remove(&id);
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
                        Ok(l) => {
                            running.lock().unwrap().insert(id.clone(), l.clone());
                            timings
                                .insert("ready_ms".into(), accepted.elapsed().as_millis() as u64);
                            timings.insert("total_ms".into(), t0.elapsed().as_millis() as u64);
                            report(
                                &handle,
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
                        Err(e) => {
                            report(
                                &handle,
                                &id,
                                "failed",
                                generation,
                                Some(&l),
                                &timings,
                                Some((format!("{e:#}"), None)),
                            )
                            .await;
                            l
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
        if want.changed().await.is_err() {
            break;
        }
    }
    running.lock().unwrap().remove(&id);
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
