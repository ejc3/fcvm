//! One task per slot. It alone starts, watches and stops the slot's fcvm child, so no two
//! actors can ever disagree about the slot's VM.
//!
//! The loop compares the desired generation with the running VM's. Below it: tear the old
//! VM down, wait until its process has exited and nothing holds the GPU, start a new one,
//! wait for the renderer, post the scene, report `running`. At it: post scene changes,
//! read the renderer's health once a second, and notice the VM dying. A newer generation
//! that arrives during a relaunch is picked up when the relaunch ends, so it runs once.

use super::inventory::{holders, vfio_node};
use super::vm::{wait_restored, Vm, VmSpec};
use fleet_core::protocol::{AgentMsg, HealthReport, RendererScene, SharedScene, SlotReport};
use fleet_core::scene::Keyframe;
use fleet_core::tiling::Rect;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

/// Agent-wide settings every slot task reads.
#[derive(Debug, Clone)]
pub struct AgentCfg {
    pub fcvm: PathBuf,
    pub image: String,
    pub extra_args: Vec<String>,
    pub publish_ip: String,
    pub frame_host: String,
    pub sysfs_root: PathBuf,
    pub start_timeout: Duration,
    pub stop_grace: Duration,
    pub gpu_free_timeout: Duration,
    pub health_interval: Duration,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    /// A VM that ran this long before dying resets the backoff.
    pub stable_after: Duration,
}

impl AgentCfg {
    /// Where the agent reaches a renderer: the publish address, or loopback when the port
    /// is published on every address.
    pub fn health_host(&self) -> String {
        match self.publish_ip.as_str() {
            "0.0.0.0" => "127.0.0.1".into(),
            "::" | "[::]" => "[::1]".into(),
            ip if ip.contains(':') && !ip.starts_with('[') => format!("[{ip}]"),
            ip => ip.into(),
        }
    }
}

/// The renderer's frame WebSocket as a browser should open it. The one place the URL is
/// built, so the routed IPv6 name and TLS can replace it.
pub fn frame_url(frame_host: &str, port: u16) -> String {
    let host = if frame_host.contains(':') && !frame_host.starts_with('[') {
        format!("[{frame_host}]")
    } else {
        frame_host.to_string()
    };
    format!("ws://{host}:{port}/frames")
}

#[derive(Debug, Clone, PartialEq)]
pub struct Desired {
    pub generation: u64,
    pub rect: Option<Rect>,
    /// A private session (`claim_gpu`): draw the whole image with this camera.
    pub private_camera: Option<Vec<Keyframe>>,
}

/// The whole image, which a private session draws.
pub const FULL_IMAGE: Rect = Rect {
    x: 0.0,
    y: 0.0,
    w: 1.0,
    h: 1.0,
};

pub struct SlotCtx {
    pub id: String,
    pub sbdf: String,
    pub port: u16,
    pub cfg: Arc<AgentCfg>,
    pub out: mpsc::UnboundedSender<AgentMsg>,
    /// Held while the slot has a VM, so two slots can never run on one GPU.
    pub gpu_lock: Arc<tokio::sync::Mutex<()>>,
    pub http: reqwest::Client,
}

struct Running {
    vm: Vm,
    generation: u64,
    started: Instant,
    posted: Option<RendererScene>,
}

enum Failure {
    /// The slot was unassigned; stop without reporting.
    Removed,
    Failed(String),
}

impl SlotCtx {
    fn vm_name(&self) -> String {
        format!("fleet-{}", self.id)
    }

    fn report(
        &self,
        phase: &str,
        generation: u64,
        timings: &BTreeMap<String, u64>,
        error: Option<String>,
        retry_in: Option<Duration>,
    ) {
        let _ = self.out.send(AgentMsg::Slot(SlotReport {
            id: self.id.clone(),
            phase: phase.into(),
            generation,
            vm: Some(self.vm_name()),
            frame_url: Some(frame_url(&self.cfg.frame_host, self.port)),
            timings: timings.clone(),
            error,
            retry_in_ms: retry_in.map(ms),
        }));
    }

    fn health_url(&self, path: &str) -> String {
        format!("http://{}:{}{path}", self.cfg.health_host(), self.port)
    }

    async fn health(&self) -> Option<serde_json::Value> {
        let r = self
            .http
            .get(self.health_url("/health"))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .ok()?;
        if !r.status().is_success() {
            return None;
        }
        r.json().await.ok()
    }

    async fn post_scene(&self, body: &RendererScene) -> Result<(), String> {
        let r = self
            .http
            .post(self.health_url("/scene"))
            .timeout(Duration::from_secs(5))
            .json(body)
            .send()
            .await
            .map_err(|e| format!("POST /scene: {e}"))?;
        if !r.status().is_success() {
            return Err(format!("POST /scene answered {}", r.status()));
        }
        Ok(())
    }
}

/// Has the renderer drawn a frame since `baseline`, the frame counter in the `/health` reply
/// read just before the scene was posted? A reply with `ready: false` has not. A renderer
/// without a frame counter has drawn once its reply carries a scene time `t`.
pub fn has_frame(health: &serde_json::Value, baseline: Option<u64>) -> bool {
    if health.get("ready").and_then(|r| r.as_bool()) == Some(false) {
        return false;
    }
    match (health.get("frame").and_then(|f| f.as_u64()), baseline) {
        (Some(f), Some(b)) => f > b,
        (Some(_), None) => false,
        (None, _) => health.get("t").is_some_and(|t| t.is_number()),
    }
}

fn frame_counter(health: &serde_json::Value) -> Option<u64> {
    health.get("frame").and_then(|f| f.as_u64())
}

/// The body to post for what the slot should draw, marking the scene read: its tile of
/// the shared scene, or for a private session the whole image with that session's camera.
fn renderer_body(
    scene: &mut watch::Receiver<Option<SharedScene>>,
    want: &Desired,
) -> Option<RendererScene> {
    let s = scene.borrow_and_update();
    let s = s.as_ref()?;
    match &want.private_camera {
        Some(camera) => Some(RendererScene {
            camera: camera.clone(),
            ..s.for_renderer(FULL_IMAGE)
        }),
        None => want.rect.map(|r| s.for_renderer(r)),
    }
}

/// Resolves when the slot's desired-state sender is dropped (the slot was unassigned).
async fn removed(desired: &mut watch::Receiver<Desired>) {
    while desired.changed().await.is_ok() {}
}

struct Backoff {
    next: Duration,
    min: Duration,
    max: Duration,
}

impl Backoff {
    fn take(&mut self) -> Duration {
        let d = self.next;
        self.next = (self.next * 2).min(self.max);
        d
    }
    fn reset(&mut self) {
        self.next = self.min;
    }
}

pub async fn supervise(
    ctx: SlotCtx,
    mut desired: watch::Receiver<Desired>,
    mut scene: watch::Receiver<Option<SharedScene>>,
) {
    let _gpu = ctx.gpu_lock.clone().lock_owned().await;
    let mut running: Option<Running> = None;
    let mut backoff = Backoff {
        next: ctx.cfg.backoff_min,
        min: ctx.cfg.backoff_min,
        max: ctx.cfg.backoff_max,
    };
    let mut retry_after: Option<Duration> = None;
    let mut health_tick = tokio::time::interval(ctx.cfg.health_interval);
    health_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let want = desired.borrow_and_update().clone();
        let behind = running
            .as_ref()
            .is_none_or(|r| r.generation < want.generation);
        if behind {
            if let Some(delay) = retry_after.take() {
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = removed(&mut desired) => break,
                }
            }
            match relaunch(
                &ctx,
                &mut running,
                want.generation,
                &mut desired,
                &mut scene,
            )
            .await
            {
                // Holds a test between the `running` report and the next pass, the window
                // where a rect change used to be read and dropped.
                Ok(()) => failpoint::hit_async("fleet.agent.slot_running").await,
                Err(Failure::Removed) => break,
                Err(Failure::Failed(e)) => {
                    tracing::warn!(slot = %ctx.id, generation = want.generation, error = %e, "relaunch failed");
                    let delay = backoff.take();
                    ctx.report(
                        "failed",
                        want.generation,
                        &BTreeMap::new(),
                        Some(e),
                        Some(delay),
                    );
                    retry_after = Some(delay);
                }
            }
            continue;
        }
        let r = running.as_mut().expect("not behind, so a VM runs");
        // A rect or scene that changed while the relaunch ran (its notification already
        // consumed above), or a post that failed, is posted here.
        post_if_changed(&ctx, r, &desired, &mut scene).await;
        tokio::select! {
            changed = desired.changed() => {
                if changed.is_err() {
                    break;
                }
                post_if_changed(&ctx, r, &desired, &mut scene).await;
            }
            changed = scene.changed() => {
                if changed.is_err() {
                    break;
                }
                post_if_changed(&ctx, r, &desired, &mut scene).await;
            }
            status = r.vm.exited() => {
                let ran = r.started.elapsed();
                let error = format!("fcvm exited ({status}) after {} s: {}", ran.as_secs(), r.vm.tail());
                tracing::warn!(slot = %ctx.id, %error, "VM died");
                if ran >= ctx.cfg.stable_after {
                    backoff.reset();
                }
                let delay = backoff.take();
                ctx.report("failed", r.generation, &BTreeMap::new(), Some(error), Some(delay));
                retry_after = Some(delay);
                // With no VM the next pass relaunches at the desired generation.
                running = None;
            }
            _ = health_tick.tick() => {
                if let Some(h) = ctx.health().await {
                    let _ = ctx.out.send(AgentMsg::Health(HealthReport {
                        id: ctx.id.clone(),
                        fps: h.get("fps").and_then(|f| f.as_f64()).unwrap_or(0.0),
                        gpu: h.get("gpu").cloned().unwrap_or(serde_json::Value::Null),
                    }));
                }
            }
        }
    }
    if let Some(mut r) = running.take() {
        r.vm.stop(ctx.cfg.stop_grace).await;
    }
}

async fn post_if_changed(
    ctx: &SlotCtx,
    r: &mut Running,
    desired: &watch::Receiver<Desired>,
    scene: &mut watch::Receiver<Option<SharedScene>>,
) {
    let want = desired.borrow().clone();
    let body = renderer_body(scene, &want);
    let Some(body) = body.filter(|b| Some(b) != r.posted.as_ref()) else {
        return;
    };
    match ctx.post_scene(&body).await {
        Ok(()) => r.posted = Some(body),
        Err(e) => tracing::warn!(slot = %ctx.id, error = %e, "scene not posted"),
    }
}

fn ms(d: Duration) -> u64 {
    d.as_millis() as u64
}

async fn wait_gpu_free(ctx: &SlotCtx) -> Result<(), String> {
    let Some(node) = vfio_node(&ctx.cfg.sysfs_root, &ctx.sbdf) else {
        return Ok(());
    };
    let deadline = Instant::now() + ctx.cfg.gpu_free_timeout;
    loop {
        let n = node.clone();
        let held = tokio::task::spawn_blocking(move || holders(&n))
            .await
            .unwrap_or_default();
        if held.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "GPU {} ({}) is still held by pid {:?} after {:?}",
                ctx.sbdf,
                node.display(),
                held,
                ctx.cfg.gpu_free_timeout
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn relaunch(
    ctx: &SlotCtx,
    running: &mut Option<Running>,
    generation: u64,
    desired: &mut watch::Receiver<Desired>,
    scene: &mut watch::Receiver<Option<SharedScene>>,
) -> Result<(), Failure> {
    let t0 = Instant::now();
    let mut timings = BTreeMap::new();
    if let Some(mut old) = running.take() {
        ctx.report("stopping", generation, &timings, None, None);
        old.vm.stop(ctx.cfg.stop_grace).await;
    }
    wait_gpu_free(ctx).await.map_err(Failure::Failed)?;
    timings.insert("teardown_ms".to_string(), ms(t0.elapsed()));

    ctx.report("restoring", generation, &timings, None, None);
    let spec = VmSpec {
        fcvm: ctx.cfg.fcvm.clone(),
        name: ctx.vm_name(),
        sbdf: ctx.sbdf.clone(),
        publish: format!("{}:{}:8080", ctx.cfg.publish_ip, ctx.port),
        extra_args: ctx.cfg.extra_args.clone(),
        image: ctx.cfg.image.clone(),
    };
    let mut vm = Vm::spawn(&spec, &ctx.id).map_err(|e| Failure::Failed(format!("{e:#}")))?;
    let spawned = Instant::now();
    let mut restored = vm.restored_watch();
    let deadline = tokio::time::sleep(ctx.cfg.start_timeout);
    tokio::pin!(deadline);
    let mut poll = tokio::time::interval(Duration::from_millis(100));
    let mut starting_at: Option<Instant> = None;
    let mut health_at: Option<Instant> = None;
    let mut baseline: Option<u64> = None;
    let mut posted: Option<RendererScene> = None;
    let fail = |vm: &Vm, why: String| Failure::Failed(format!("{why}; fcvm said: {}", vm.tail()));

    let first_frame = loop {
        tokio::select! {
            r = wait_restored(&mut restored), if starting_at.is_none() => {
                timings.insert("restore_ms".into(), ms(r.at - spawned));
                if let Some(a) = r.ack_ms { timings.insert("ack_ms".into(), a); }
                if let Some(g) = r.gpu_ms { timings.insert("gpu_ms".into(), g); }
                starting_at = Some(r.at);
                ctx.report("starting", generation, &timings, None, None);
            }
            status = vm.exited() => {
                let e = fail(&vm, format!("fcvm exited ({status}) before the renderer's first frame"));
                return Err(e);
            }
            _ = poll.tick() => {
                let Some(h) = ctx.health().await else { continue };
                if let Some(e) = h.get("error").and_then(|e| e.as_str()) {
                    let e = fail(&vm, format!("the renderer failed: {e}"));
                    vm.stop(ctx.cfg.stop_grace).await;
                    return Err(e);
                }
                if health_at.is_none() {
                    let now = Instant::now();
                    health_at = Some(now);
                    // Without a restore record (an fcvm path that prints none), the
                    // renderer answering is where `starting` begins.
                    let from = *starting_at.get_or_insert_with(|| {
                        ctx.report("starting", generation, &timings, None, None);
                        now
                    });
                    timings.insert("container_ms".into(), ms(now - from));
                }
                let want = desired.borrow().clone();
                let want = renderer_body(scene, &want);
                if let Some(body) = want.filter(|w| Some(w) != posted.as_ref()) {
                    if let Err(e) = ctx.post_scene(&body).await {
                        tracing::debug!(slot = %ctx.id, error = %e, "scene not posted yet");
                        continue;
                    }
                    posted = Some(body);
                    // Frames counted from here were drawn with this scene.
                    baseline = frame_counter(&h);
                    continue;
                }
                if baseline.is_none() {
                    baseline = frame_counter(&h);
                    if baseline.is_some() {
                        continue;
                    }
                }
                if has_frame(&h, baseline) {
                    let startup = |k: &str| h.pointer(&format!("/startup_ms/{k}")).and_then(|v| v.as_f64()).filter(|v| *v >= 0.0);
                    if let Some(v) = startup("gpu_ready") {
                        timings.insert("renderer_gpu_ready_ms".into(), v as u64);
                    }
                    if let Some(v) = startup("first_frame_drawn") {
                        timings.insert("renderer_first_frame_ms".into(), v as u64);
                    }
                    break Instant::now();
                }
            }
            _ = &mut deadline => {
                let e = fail(&vm, format!("no first frame {} s after fcvm started", ctx.cfg.start_timeout.as_secs()));
                vm.stop(ctx.cfg.stop_grace).await;
                return Err(e);
            }
            _ = removed(desired) => {
                vm.stop(ctx.cfg.stop_grace).await;
                return Err(Failure::Removed);
            }
        }
    };
    let health_at = health_at.expect("a frame was read from /health");
    timings.insert("first_frame_ms".into(), ms(first_frame - health_at));
    timings.insert("total_ms".into(), ms(t0.elapsed()));
    ctx.report("running", generation, &timings, None, None);
    *running = Some(Running {
        vm,
        generation,
        started: spawned,
        posted,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_url_brackets_ipv6_literals() {
        assert_eq!(frame_url("gpu-host", 20001), "ws://gpu-host:20001/frames");
        assert_eq!(
            frame_url("2001:db8::1", 20001),
            "ws://[2001:db8::1]:20001/frames"
        );
        assert_eq!(
            frame_url("[2001:db8::1]", 443),
            "ws://[2001:db8::1]:443/frames"
        );
    }

    #[test]
    fn a_frame_is_a_counter_past_the_baseline_or_a_scene_time() {
        use serde_json::json;
        // The fleet renderer: a frame counter that moved past the baseline, and ready.
        assert!(has_frame(
            &json!({"t": 1.0, "frame": 8, "ready": true}),
            Some(7)
        ));
        assert!(!has_frame(
            &json!({"t": 1.0, "frame": 7, "ready": true}),
            Some(7)
        ));
        assert!(!has_frame(
            &json!({"t": 0.0, "frame": 9, "ready": false}),
            Some(7)
        ));
        assert!(!has_frame(&json!({"t": 1.0, "frame": 9}), None));
        // A renderer without a counter: a scene time is a frame.
        assert!(has_frame(&json!({"fps": 30, "t": 12.5}), None));
        assert!(!has_frame(&json!({"fps": 0, "t": null}), None));
        assert!(!has_frame(&json!({"fps": 0}), None));
    }

    #[test]
    fn backoff_doubles_to_the_cap_and_resets() {
        let mut b = Backoff {
            next: Duration::from_secs(1),
            min: Duration::from_secs(1),
            max: Duration::from_secs(30),
        };
        let got: Vec<u64> = (0..7).map(|_| b.take().as_secs()).collect();
        assert_eq!(got, vec![1, 2, 4, 8, 16, 30, 30]);
        b.reset();
        assert_eq!(b.take().as_secs(), 1);
    }
}
