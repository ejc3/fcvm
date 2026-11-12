//! Minecraft mode (`--minecraft N`): N Pumpkin servers on a host, each an fcvm VM of the
//! `fleet/minecraft` image in routed mode at its own address, restored from one gold snapshot of
//! a server with its world loaded and its bots joined (docs/fleet/minecraft.md).
//!
//! The gold flow runs once per gold key when the agent starts, unless that snapshot is already
//! there: boot `mc-gold`, wait until its tile's `/health` is ready and the tick rate has read 20
//! for `steady` polls in a row, record how many times its bots had joined, and
//! `fcvm snapshot create --stop`. A server is `fcvm snapshot run` of that snapshot; it is up once
//! its bots have joined again (a restore retires the snapshot's sockets, so they rejoin) and
//! received a chunk.
//!
//! A fork (`AssignMinecraft::fork`) copies another server of this host while it runs: `fcvm
//! snapshot create` of the source's VM (which keeps running), the target's own VM stopped, the
//! copy restored at the target's address, and the copy's snapshot removed once it runs.
//!
//! One task per server ([`supervise`]) alone starts, watches and stops its VM. Every VM child is
//! armed with `PR_SET_PDEATHSIG` ([`Vm::spawn_args`]), so a dead agent leaves no VM behind.

use super::slot::AgentCfg;
use super::vm::Vm;
use anyhow::{bail, Context, Result};
use fleet_core::protocol::{
    AgentMsg, AssignMinecraft, AssignSlot, ForkFrom, SharedScene, SlotReport,
};
use std::collections::{BTreeMap, HashMap};
use std::net::{Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

/// The gold VM's name.
pub const GOLD_VM: &str = "mc-gold";
/// mctile's plain HTTP port in the VM (its `PORT`): the agent's `/health` and `/scene`.
pub const VIEW_PORT: u16 = 8080;
/// The tick rate a server must hold.
pub const HOLD_TPS: f64 = 19.5;
/// Bumped when the gold flow changes what the snapshot holds. 2: a gold made by an fc-agent
/// that keeps the guest's IPv4-mapped loopback connections across a restore, which is what
/// lets a restored tile count as up while its bots stay connected ([`Health::up`]).
pub const GOLD_FLOW: &str = "mc-gold-2";

#[derive(Debug, Clone)]
pub struct MinecraftCfg {
    pub count: u32,
    pub image: String,
    /// The image's id (`podman image inspect`), part of the gold key.
    pub image_id: String,
    /// The servers' name and version for the page (the image's labels).
    pub server: Option<String>,
    pub mem_mib: u32,
    pub cpus: u32,
    /// Bots in each tile (the image's `MCTILE_BOTS`).
    pub bots: u32,
    pub snapshot_dir: PathBuf,
    /// The certificate directory, when the agent has a certificate (`--tls-secret`).
    pub tls_dir: Option<PathBuf>,
    pub gold_deadline: Duration,
    /// A restore (or fork) to its bots' first chunk.
    pub ready_deadline: Duration,
    pub snapshot_deadline: Duration,
    pub poll: Duration,
    /// Polls in a row the gold server must be ready at 20 ticks a second.
    pub steady: u32,
}

impl MinecraftCfg {
    /// The gold snapshot's key: what it holds and the VM shape every clone inherits.
    pub fn gold_key(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        for part in [
            GOLD_FLOW,
            &self.image_id,
            &self.mem_mib.to_string(),
            &self.cpus.to_string(),
            &self.bots.to_string(),
            if self.tls_dir.is_some() {
                "tls"
            } else {
                "plain"
            },
        ] {
            h.update(part.as_bytes());
            h.update([0]);
        }
        hex::encode(&h.finalize()[..6])
    }

    pub fn gold_tag(&self) -> String {
        format!("mc-gold-{}", self.gold_key())
    }
}

/// Where a server lives on the host, by its index.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Place {
    /// Its routed address (`None` only in tests).
    pub ipv6: Option<Ipv6Addr>,
    /// Where the agent reaches its tile's plain HTTP.
    pub http: SocketAddr,
}

impl Place {
    pub fn routed(addr: Ipv6Addr) -> Place {
        Place {
            ipv6: Some(addr),
            http: SocketAddr::new(addr.into(), VIEW_PORT),
        }
    }
}

fn s(a: &[&str]) -> Vec<String> {
    a.iter().map(|x| x.to_string()).collect()
}

fn network_args(prefix: &str, place: &Place) -> Vec<String> {
    match place.ipv6 {
        Some(a) => s(&["--ipv6-prefix", prefix, "--ipv6-address", &a.to_string()]),
        None => Vec::new(),
    }
}

/// `fcvm podman run` of the gold server.
pub fn gold_argv(cfg: &MinecraftCfg, prefix: &str, place: &Place) -> Vec<String> {
    let mut a = s(&["podman", "run", "--name", GOLD_VM, "--network", "routed"]);
    a.extend(network_args(prefix, place));
    a.extend(s(&[
        "--mem",
        &cfg.mem_mib.to_string(),
        "--cpu",
        &cfg.cpus.to_string(),
        "--env",
        &format!("MCTILE_BOTS={}", cfg.bots),
    ]));
    if let Some(dir) = &cfg.tls_dir {
        a.extend(super::tls::vm_args(dir));
    }
    a.push(cfg.image.clone());
    a
}

/// `fcvm snapshot run` of `tag` as server `name`.
/// The command the guest runs in the tile's container once a restore has retired the
/// snapshot's sockets and before the clone takes connections: the tile drops the scene it was
/// copied with (a fork's source borrower among it) until the agent gives it its own.
pub const RESTORE_HOOK: &str = "mctile restored";

pub fn restore_argv(tag: &str, name: &str, prefix: &str, place: &Place) -> Vec<String> {
    let mut a = s(&["snapshot", "run", "--snapshot", tag, "--name", name]);
    a.extend(network_args(prefix, place));
    a.extend(s(&["--on-restore", RESTORE_HOOK]));
    a
}

/// A fork's snapshot, removed when it is dropped: after the copy runs, after it fails, and when
/// the relaunch is given up (the server unassigned) after the snapshot was made.
struct ForkSnapshot {
    dir: PathBuf,
    tag: String,
}

impl ForkSnapshot {
    /// Remove it now, once the copy has run (or failed to).
    async fn remove(mut self) {
        let tag = std::mem::take(&mut self.tag);
        remove_snapshot(&self.dir, &tag).await;
    }
}

impl Drop for ForkSnapshot {
    /// The relaunch was given up before `remove`: the removal waits for the snapshot's
    /// generation lock (another fork's create can hold it while it reads this snapshot as its
    /// parent), so it runs on a task of its own.
    fn drop(&mut self) {
        if self.tag.is_empty() {
            return;
        }
        let (dir, tag) = (self.dir.clone(), std::mem::take(&mut self.tag));
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move { remove_snapshot(&dir, &tag).await });
        }
    }
}

/// The fork snapshots an earlier agent left (one that died between a fork's snapshot and its
/// removal), and their lock files: removed when the agent starts, before any server runs, so no
/// fork of this agent can hold or wait on one of those locks.
pub async fn sweep_fork_snapshots(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut tags: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("mc-fork-"))
        .map(|n| n.trim_end_matches(".lock").to_string())
        .collect();
    tags.sort();
    tags.dedup();
    for tag in tags {
        remove_snapshot(dir, &tag).await;
        let lock = dir.join(format!("{tag}.lock"));
        // Unlinked while held, and only here: a create left by the earlier agent (never killed)
        // finishes before the removal above gets the lock, and no fork of this agent has begun.
        if let Ok(_held) =
            crate::commands::common::acquire_snapshot_dir_lock(&dir.join(&tag), true).await
        {
            if !dir.join(&tag).exists() {
                let _ = std::fs::remove_file(&lock);
            }
        }
    }
}

/// `fcvm snapshot create` of a running server; `stop` for the gold VM only.
pub fn snapshot_argv(pid: u32, tag: &str, stop: bool) -> Vec<String> {
    let mut a = s(&[
        "snapshot",
        "create",
        "--pid",
        &pid.to_string(),
        "--tag",
        tag,
    ]);
    if stop {
        a.push("--stop".into());
    }
    a
}

/// The fields of a tile's `/health` the agent reads.
#[derive(Debug, Clone, PartialEq, Default, serde::Deserialize)]
pub struct Health {
    #[serde(default)]
    pub ready: bool,
    #[serde(default)]
    pub tps: Option<f64>,
    #[serde(default)]
    pub sessions: u64,
    #[serde(default)]
    pub chunks: u64,
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub tile: String,
    #[serde(default)]
    pub bots: Option<BotCount>,
}

/// A tile's bots: connected, and how many it runs.
#[derive(Debug, Clone, PartialEq, Default, serde::Deserialize)]
pub struct BotCount {
    #[serde(default)]
    pub online: u64,
    #[serde(default)]
    pub wanted: u64,
}

impl Health {
    /// A restored tile is up once it answers with its map, a chunk and every bot connected. The
    /// restore keeps the guest's loopback connections, the bots' to Pumpkin among them (fc-agent's
    /// snapshot network boundary), so a tile restored from a [`GOLD_FLOW`] 2 snapshot is up as
    /// soon as it answers; a bot the restore did disconnect is waited for. (Before, the restore
    /// reset Pumpkin's end of each bot connection, and the tile waited about a second for the
    /// rejoin.)
    pub fn up(&self) -> bool {
        self.ready && self.chunks > 0 && self.bots.as_ref().is_some_and(|b| b.online >= b.wanted)
    }

    pub fn steady(&self) -> bool {
        self.ready && self.tps.is_some_and(|t| t >= HOLD_TPS)
    }
}

/// What a server should be running.
#[derive(Debug, Clone, PartialEq)]
pub struct Desired {
    pub generation: u64,
    pub clicked_at: Option<u64>,
    pub fork: Option<ForkFrom>,
    pub driver: Option<String>,
    pub driver_until: Option<u64>,
}

impl Desired {
    pub fn of(generation: u64, m: &AssignMinecraft) -> Desired {
        Desired {
            generation,
            clicked_at: m.clicked_at,
            fork: m.fork.clone(),
            driver: m.driver.clone(),
            driver_until: m.driver_until,
        }
    }
}

/// The scene a tile is given (`POST /scene`).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TileScene {
    pub tile: String,
    pub generation: u64,
    pub admit_keys: Vec<String>,
    pub driver: Option<String>,
    pub driver_until: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GoldState {
    Building,
    /// The snapshot's tag, and how many times its bots had joined.
    Ready(String, u64),
    Failed(String),
}

/// The running servers' fcvm pids and addresses, by index: what a fork snapshots.
type Running = Arc<Mutex<HashMap<u32, (u32, Place)>>>;

struct Handle {
    index: u32,
    desired: watch::Sender<Desired>,
    task: JoinHandle<()>,
}

/// The host's servers: the gold flow and one task per assigned server.
pub struct Minecraft {
    cfg: Arc<MinecraftCfg>,
    agent: Arc<AgentCfg>,
    prefix: String,
    places: Vec<Place>,
    gold_place: Place,
    out: mpsc::UnboundedSender<AgentMsg>,
    http: reqwest::Client,
    gold: watch::Receiver<GoldState>,
    scene: watch::Receiver<Option<SharedScene>>,
    running: Running,
    servers: BTreeMap<String, Handle>,
    retired: Vec<(u32, JoinHandle<()>)>,
    stop: watch::Sender<bool>,
    flow: JoinHandle<()>,
}

async fn stopping(mut stop: watch::Receiver<bool>) {
    while !*stop.borrow_and_update() {
        if stop.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

impl Minecraft {
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        cfg: Arc<MinecraftCfg>,
        agent: Arc<AgentCfg>,
        prefix: String,
        places: Vec<Place>,
        gold_place: Place,
        out: mpsc::UnboundedSender<AgentMsg>,
        http: reqwest::Client,
        scene: watch::Receiver<Option<SharedScene>>,
    ) -> Minecraft {
        sweep_fork_snapshots(&cfg.snapshot_dir).await;
        let (gold_tx, gold) = watch::channel(GoldState::Building);
        let (stop, stop_rx) = watch::channel(false);
        let flow = {
            let (cfg, agent, prefix, http) =
                (cfg.clone(), agent.clone(), prefix.clone(), http.clone());
            tokio::spawn(async move {
                loop {
                    let r =
                        gold_flow(&cfg, &agent, &prefix, &gold_place, &http, stop_rx.clone()).await;
                    match r {
                        Ok((tag, sessions)) => {
                            tracing::info!(target: "fcvm::fleet::minecraft", %tag, sessions, "the gold snapshot is ready");
                            gold_tx.send_replace(GoldState::Ready(tag, sessions));
                            break;
                        }
                        Err(e) => {
                            let e = format!("{e:#}");
                            tracing::error!(target: "fcvm::fleet::minecraft", error = %e, "the gold flow failed; trying again in 60 s");
                            gold_tx.send_replace(GoldState::Failed(e));
                        }
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(60)) => {}
                        _ = stopping(stop_rx.clone()) => return,
                    }
                }
                tokio::select! {
                    _ = gold_tx.closed() => {}
                    _ = stopping(stop_rx) => {}
                }
            })
        };
        Minecraft {
            cfg,
            agent,
            prefix,
            places,
            gold_place,
            out,
            http,
            gold,
            scene,
            running: Arc::default(),
            servers: BTreeMap::new(),
            retired: Vec::new(),
            stop,
            flow,
        }
    }

    /// Reconcile to the assignment's servers: start the new ones, tell the others what they
    /// should run, stop those no longer listed.
    pub fn assign(&mut self, list: &[AssignSlot]) {
        let keep: HashMap<&str, u32> = list
            .iter()
            .filter_map(|a| Some((a.id.as_str(), a.minecraft.as_ref()?.index)))
            .collect();
        let gone: Vec<String> = self
            .servers
            .iter()
            .filter(|(id, h)| keep.get(id.as_str()) != Some(&h.index))
            .map(|(id, _)| id.clone())
            .collect();
        for id in gone {
            let h = self.servers.remove(&id).unwrap();
            drop(h.desired);
            self.retired.push((h.index, h.task));
        }
        self.retired.retain(|(_, t)| !t.is_finished());
        for a in list {
            let Some(m) = &a.minecraft else { continue };
            let want = Desired::of(a.generation, m);
            if let Some(h) = self.servers.get(&a.id) {
                h.desired.send_if_modified(|d| {
                    // Generations only grow; an older assignment never moves a server back.
                    let next = if want.generation >= d.generation {
                        want.clone()
                    } else {
                        Desired {
                            generation: d.generation,
                            ..want.clone()
                        }
                    };
                    let changed = *d != next;
                    *d = next;
                    changed
                });
                continue;
            }
            let Some(place) = self.places.get(m.index as usize).copied() else {
                tracing::warn!(server = %a.id, index = m.index, "assigned a Minecraft server this host does not run");
                continue;
            };
            let (tx, rx) = watch::channel(want);
            let ctx = Server {
                id: a.id.clone(),
                index: m.index,
                place,
                prefix: self.prefix.clone(),
                cfg: self.cfg.clone(),
                agent: self.agent.clone(),
                out: self.out.clone(),
                http: self.http.clone(),
                scene: self.scene.clone(),
                running: self.running.clone(),
            };
            let (before, kept) = std::mem::take(&mut self.retired)
                .into_iter()
                .partition(|(i, _)| *i == m.index);
            self.retired = kept;
            let before = before.into_iter().map(|(_, t)| t).collect();
            let task = tokio::spawn(supervise(ctx, rx, self.gold.clone(), before));
            self.servers.insert(
                a.id.clone(),
                Handle {
                    index: m.index,
                    desired: tx,
                    task,
                },
            );
        }
    }

    /// Stop every server and the gold flow, and wait until each VM has exited.
    pub async fn stop(mut self) {
        self.stop.send_replace(true);
        for (_, h) in std::mem::take(&mut self.servers) {
            drop(h.desired);
            self.retired.push((h.index, h.task));
        }
        for (_, t) in self.retired {
            let _ = t.await;
        }
        let _ = self.gold_place;
        let mut flow = self.flow;
        if tokio::time::timeout(self.agent.stop_grace * 2, &mut flow)
            .await
            .is_err()
        {
            flow.abort();
            let _ = flow.await;
        }
    }
}

/// A child that is not a VM (`fcvm snapshot create`), armed as every VM is.
fn child(program: &Path) -> tokio::process::Command {
    let mut c = std::process::Command::new(program);
    c.stdin(std::process::Stdio::null());
    super::vm::arm_pdeathsig(&mut c);
    let mut c = tokio::process::Command::from(c);
    c.kill_on_drop(true);
    c
}

async fn health(http: &reqwest::Client, place: &Place) -> Option<Health> {
    health_within(http, place, Duration::from_secs(2)).await
}

/// How long one `/health` attempt of a restore's wait may take. The wait's first attempts go
/// out while the clone is still being restored, and a SYN sent then is lost (nothing reads the
/// tap yet): with the 2 s of [`health`], every restore waited out that attempt and was timed at
/// 2.0 s whatever it took. A short attempt, retried every poll, sees the tile as soon as it
/// answers.
pub const UP_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

async fn health_within(http: &reqwest::Client, place: &Place, timeout: Duration) -> Option<Health> {
    let url = format!("http://{}/health", place.http);
    let r = http.get(url).timeout(timeout).send().await.ok()?;
    if !r.status().is_success() {
        return None;
    }
    r.json().await.ok()
}

/// `fcvm snapshot create`. Its process is never killed, not past the deadline and not when the
/// caller gives up on it (a relaunch dropped by an unassignment): killed after it paused the
/// source, it would leave a running server paused for good. Past the deadline it finishes on
/// its own and is reaped; a fork's snapshot it leaves is swept at the next agent start.
async fn snapshot(fcvm: &Path, pid: u32, tag: &str, stop: bool, deadline: Duration) -> Result<()> {
    let mut c = child(fcvm);
    c.kill_on_drop(false);
    let run = c.args(snapshot_argv(pid, tag, stop)).output();
    let out = tokio::time::timeout(deadline, run)
        .await
        .with_context(|| format!("fcvm snapshot create took over {} s", deadline.as_secs()))?
        .context("running fcvm snapshot create")?;
    if !out.status.success() {
        bail!(
            "fcvm snapshot create failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn snapshot_made(dir: &Path, tag: &str) -> bool {
    dir.join(tag).join("config.json").is_file()
}

/// The sessions count a gold snapshot was taken at, kept beside it by the agent.
fn sessions_file(dir: &Path, tag: &str) -> PathBuf {
    dir.join(format!("{tag}.minecraft.json"))
}

/// Remove a fork's snapshot as `fcvm snapshots delete` does: under its generation lock, which a
/// create holds while it reads the snapshot as its diff parent (a second fork of the same
/// source), so the removal waits for that create. The lock file stays: unlinked while held, the
/// next locker would lock a different file.
pub async fn remove_snapshot(dir: &Path, tag: &str) {
    if let Err(e) = crate::storage::snapshot::SnapshotManager::new(dir.to_path_buf())
        .delete_snapshot(tag)
        .await
    {
        tracing::warn!(target: "fcvm::fleet::minecraft", %tag, error = %format!("{e:#}"), "could not remove a fork's snapshot");
    }
}

/// Make (or reuse) the gold snapshot; its tag and the sessions count it was taken at. Every way
/// out stops the gold VM.
async fn gold_flow(
    cfg: &MinecraftCfg,
    agent: &AgentCfg,
    prefix: &str,
    place: &Place,
    http: &reqwest::Client,
    stop: watch::Receiver<bool>,
) -> Result<(String, u64)> {
    let tag = cfg.gold_tag();
    if snapshot_made(&cfg.snapshot_dir, &tag) {
        if let Some(n) = std::fs::read(sessions_file(&cfg.snapshot_dir, &tag))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v.get("sessions").and_then(|s| s.as_u64()))
        {
            tracing::info!(target: "fcvm::fleet::minecraft", %tag, "reusing the gold snapshot");
            return Ok((tag, n));
        }
    }
    let t0 = Instant::now();
    tracing::info!(target: "fcvm::fleet::minecraft", %tag, image = %cfg.image, "making the gold snapshot");
    let mut vm = Vm::spawn_args(&agent.fcvm, &gold_argv(cfg, prefix, place), GOLD_VM)?;
    let r = tokio::select! {
        r = gold_steps(cfg, agent, &tag, place, http, &mut vm) => r,
        _ = stopping(stop) => Err(anyhow::anyhow!("the agent is stopping")),
    };
    vm.stop(agent.stop_grace).await;
    let sessions = r?;
    if !snapshot_made(&cfg.snapshot_dir, &tag) {
        bail!(
            "fcvm snapshot create succeeded but {} has no config.json",
            cfg.snapshot_dir.join(&tag).display()
        );
    }
    std::fs::write(
        sessions_file(&cfg.snapshot_dir, &tag),
        serde_json::json!({"sessions": sessions}).to_string(),
    )?;
    tracing::info!(target: "fcvm::fleet::minecraft", %tag, secs = t0.elapsed().as_secs(), "gold snapshot made");
    Ok((tag, sessions))
}

async fn gold_steps(
    cfg: &MinecraftCfg,
    agent: &AgentCfg,
    tag: &str,
    place: &Place,
    http: &reqwest::Client,
    vm: &mut Vm,
) -> Result<u64> {
    let until = tokio::time::sleep(cfg.gold_deadline);
    tokio::pin!(until);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut steady = 0;
    let mut last = None;
    let sessions = loop {
        tokio::select! {
            _ = tick.tick() => {
                let h = health(http, place).await;
                steady = if h.as_ref().is_some_and(Health::steady) { steady + 1 } else { 0 };
                if steady >= cfg.steady {
                    break h.map(|h| h.sessions).unwrap_or(0);
                }
                last = h;
            }
            status = vm.exited() => bail!("fcvm exited ({status}) before the gold server was ready; fcvm said: {}", vm.tail()),
            _ = &mut until => bail!("the gold server was not ready at 20 TPS within {} s (last health {last:?}); fcvm said: {}", cfg.gold_deadline.as_secs(), vm.tail()),
        }
    };
    snapshot(&agent.fcvm, vm.pid, tag, true, cfg.snapshot_deadline).await?;
    Ok(sessions)
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

struct Server {
    id: String,
    index: u32,
    place: Place,
    prefix: String,
    cfg: Arc<MinecraftCfg>,
    agent: Arc<AgentCfg>,
    out: mpsc::UnboundedSender<AgentMsg>,
    http: reqwest::Client,
    scene: watch::Receiver<Option<SharedScene>>,
    running: Running,
}

struct Live {
    vm: Vm,
    generation: u64,
    started: Instant,
    /// The scene the tile holds.
    scene: Option<TileScene>,
}

impl Server {
    fn frame_url(&self) -> String {
        match &self.agent.frame_zone {
            Some(zone) => format!("wss://{}.{zone}/view", self.id),
            None => format!("ws://{}/view", self.place.http),
        }
    }

    fn report(
        &self,
        phase: &str,
        generation: u64,
        timings: BTreeMap<String, u64>,
        error: Option<String>,
        retry_in: Option<Duration>,
        first_chunk_at: Option<u64>,
    ) {
        let _ = self.out.send(AgentMsg::Slot(SlotReport {
            id: self.id.clone(),
            phase: phase.into(),
            generation,
            vm: Some(self.id.clone()),
            frame_url: Some(self.frame_url()),
            timings,
            error,
            retry_in_ms: retry_in.map(|d| d.as_millis() as u64),
            ipv6: self.place.ipv6.map(|a| a.to_string()),
            place: self.agent.place.clone(),
            first_chunk_at,
            ..Default::default()
        }));
    }

    fn scene_for(&self, want: &Desired, generation: u64) -> TileScene {
        TileScene {
            tile: self.id.clone(),
            generation,
            admit_keys: self
                .scene
                .borrow()
                .as_ref()
                .map(|s| s.admit_keys.clone())
                .unwrap_or_default(),
            driver: want.driver.clone(),
            driver_until: want.driver_until,
        }
    }

    /// Give the tile the scene when it holds another; what it holds afterwards.
    async fn sync_scene(&self, held: Option<TileScene>, want: TileScene) -> Option<TileScene> {
        if held.as_ref() == Some(&want) {
            return held;
        }
        let sent = self
            .agent
            .scene_token
            .authorize(self.http.post(format!("http://{}/scene", self.place.http)))
            .timeout(Duration::from_secs(5))
            .json(&want)
            .send()
            .await;
        match sent {
            Ok(r) if r.status().is_success() => Some(want),
            Ok(r) => {
                tracing::warn!(target: "fcvm::fleet::minecraft", server = %self.id, status = %r.status(), "the tile refused the scene");
                held
            }
            Err(e) => {
                tracing::debug!(target: "fcvm::fleet::minecraft", server = %self.id, error = %e, "the scene did not reach the tile");
                held
            }
        }
    }

    async fn stop(&self, live: &mut Live) {
        self.running.lock().unwrap().remove(&self.index);
        live.vm.stop(self.agent.stop_grace).await;
    }

    /// Wait until the restored tile is up: its bots joined after the snapshot and got a chunk.
    async fn wait_up(&self, vm: &mut Vm) -> std::result::Result<(), String> {
        let until = tokio::time::sleep(self.cfg.ready_deadline);
        tokio::pin!(until);
        let mut tick = tokio::time::interval(self.cfg.poll);
        let mut last = None;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let h = health_within(&self.http, &self.place, UP_PROBE_TIMEOUT).await;
                    if h.as_ref().is_some_and(Health::up) {
                        return Ok(());
                    }
                    last = h;
                }
                status = vm.exited() => return Err(format!("fcvm exited ({status}) before the server was up; fcvm said: {}", vm.tail())),
                _ = &mut until => return Err(format!("the server was not up {} s after its restore (last health {last:?}); fcvm said: {}", self.cfg.ready_deadline.as_secs(), vm.tail())),
            }
        }
    }

    /// Stop what runs and start `want`: a fork of another server when it asks for one and that
    /// server runs, else the gold snapshot.
    /// Stop what runs and start `want`: a copy of another server when it asks for a fork,
    /// `fork_ok` holds (no copy of this generation failed yet) and that server runs here, else
    /// the gold snapshot.
    async fn relaunch(
        &self,
        live: &mut Option<Live>,
        gold: &str,
        want: &Desired,
        fork_ok: bool,
    ) -> std::result::Result<Live, String> {
        let generation = want.generation;
        let t0 = Instant::now();
        let mut timings = BTreeMap::new();
        let source = want
            .fork
            .as_ref()
            .filter(|f| fork_ok && f.index != self.index)
            .and_then(|f| {
                self.running
                    .lock()
                    .unwrap()
                    .get(&f.index)
                    .copied()
                    .map(|(pid, place)| (f.clone(), pid, place))
            });
        // Dropped on every way out of this function: the copy's snapshot is removed.
        let mut fork_snapshot = None;
        let tag = match source {
            Some((f, pid, place)) => {
                self.report("forking", generation, BTreeMap::new(), None, None, None);
                let before = health(&self.http, &place).await.map(|h| h.sessions);
                let tag = format!("mc-fork-{}-{generation}", self.id);
                remove_snapshot(&self.cfg.snapshot_dir, &tag).await;
                let ts = Instant::now();
                snapshot(
                    &self.agent.fcvm,
                    pid,
                    &tag,
                    false,
                    self.cfg.snapshot_deadline,
                )
                .await
                .map_err(|e| format!("snapshotting {} for the fork: {e:#}", f.slot))?;
                fork_snapshot = Some(ForkSnapshot {
                    dir: self.cfg.snapshot_dir.clone(),
                    tag: tag.clone(),
                });
                timings.insert("snapshot_ms".to_string(), ts.elapsed().as_millis() as u64);
                tracing::info!(target: "fcvm::fleet::minecraft", server = %self.id, from = %f.slot, snapshot_ms = timings["snapshot_ms"], "forking");
                before.ok_or_else(|| format!("{} did not answer before its fork", f.slot))?;
                tag
            }
            None => {
                if want.fork.is_some() {
                    tracing::warn!(target: "fcvm::fleet::minecraft", server = %self.id, fork_ok, "no copy of the fork's source this time; restoring from the gold snapshot");
                }
                self.report("restoring", generation, BTreeMap::new(), None, None, None);
                gold.to_string()
            }
        };
        if let Some(mut old) = live.take() {
            self.stop(&mut old).await;
        }
        let tr = Instant::now();
        let argv = restore_argv(&tag, &self.id, &self.prefix, &self.place);
        let mut vm =
            Vm::spawn_args(&self.agent.fcvm, &argv, &self.id).map_err(|e| format!("{e:#}"))?;
        // The tile's scene as soon as it answers, posted beside the wait for its first chunk so
        // that neither holds up the other.
        let want_scene = self.scene_for(want, generation);
        let poster = async {
            loop {
                if let Some(h) = self.sync_scene(None, want_scene.clone()).await {
                    return (h, tr.elapsed().as_millis() as u64);
                }
                tokio::time::sleep(self.cfg.poll).await;
            }
        };
        let mut held = None;
        let up = {
            let wait = self.wait_up(&mut vm);
            tokio::pin!(wait);
            tokio::pin!(poster);
            loop {
                tokio::select! {
                    r = &mut wait => break r,
                    (h, ms) = &mut poster, if held.is_none() => {
                        held = Some(h);
                        timings.insert("restore_ms".to_string(), ms);
                    }
                }
            }
        };
        let first_chunk_at = unix_ms();
        // A copy that is not up may still be restoring, which holds its snapshot's generation
        // lock: it is stopped before the removal waits on that lock.
        if let Err(e) = up {
            vm.stop(self.agent.stop_grace).await;
            if let Some(f) = fork_snapshot.take() {
                f.remove().await;
            }
            return Err(e);
        }
        if let Some(f) = fork_snapshot.take() {
            f.remove().await;
        }
        let attempt_ms = tr.elapsed().as_millis() as u64;
        timings
            .entry("restore_ms".to_string())
            .or_insert(attempt_ms);
        timings.insert("first_chunk_ms".to_string(), attempt_ms);
        // From the click when there was one (the coordinator's clock, which NTP keeps near the
        // host's), else from the start of this launch.
        let total = want
            .clicked_at
            .map(|c| first_chunk_at.saturating_sub(c))
            .filter(|t| *t < 600_000)
            .unwrap_or(t0.elapsed().as_millis() as u64);
        timings.insert("total_ms".to_string(), total);
        if held.is_none() {
            held = self.sync_scene(None, want_scene).await;
        }
        self.running
            .lock()
            .unwrap()
            .insert(self.index, (vm.pid, self.place));
        tracing::info!(target: "fcvm::fleet::minecraft", server = %self.id, generation, forked = timings.contains_key("snapshot_ms"), ?timings, "server up");
        self.report(
            "running",
            generation,
            timings,
            None,
            None,
            Some(first_chunk_at),
        );
        Ok(Live {
            vm,
            generation,
            started: tr,
            scene: held,
        })
    }
}

/// Resolves when the server's desired-state sender is dropped (it was unassigned).
async fn removed(desired: &mut watch::Receiver<Desired>) {
    while desired.changed().await.is_ok() {}
}

async fn supervise(
    ctx: Server,
    mut desired: watch::Receiver<Desired>,
    mut gold: watch::Receiver<GoldState>,
    before: Vec<JoinHandle<()>>,
) {
    for mut old in before {
        if tokio::time::timeout(ctx.agent.stop_grace * 2, &mut old)
            .await
            .is_err()
        {
            old.abort();
            let _ = old.await;
        }
    }
    let mut live: Option<Live> = None;
    // The generation whose fork failed once: its next attempt restores the gold snapshot.
    let mut fork_failed: Option<u64> = None;
    let mut backoff = ctx.agent.backoff_min;
    let mut retry_after: Option<Duration> = None;
    let mut health_tick = tokio::time::interval(ctx.agent.health_interval);
    health_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut scene = ctx.scene.clone();
    let mut scene_open = true;
    loop {
        let want = desired.borrow_and_update().clone();
        if live.as_ref().is_none_or(|l| l.generation < want.generation) {
            let g = loop {
                let g = gold.borrow_and_update().clone();
                match &g {
                    GoldState::Ready(..) => break Some(g),
                    GoldState::Failed(e) => ctx.report(
                        "failed",
                        want.generation,
                        BTreeMap::new(),
                        Some(format!("no gold snapshot: {e}")),
                        None,
                        None,
                    ),
                    GoldState::Building => {}
                }
                tokio::select! {
                    r = gold.changed() => if r.is_err() { break None },
                    _ = removed(&mut desired) => break None,
                }
            };
            let Some(GoldState::Ready(tag, _)) = g else {
                break;
            };
            if let Some(delay) = retry_after.take() {
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = removed(&mut desired) => break,
                }
            }
            let want = desired.borrow_and_update().clone();
            let r = tokio::select! {
                r = ctx.relaunch(&mut live, &tag, &want, fork_failed != Some(want.generation)) => Some(r),
                _ = removed(&mut desired) => None,
            };
            match r {
                None => break,
                Some(Ok(l)) => live = Some(l),
                Some(Err(e)) => {
                    tracing::warn!(target: "fcvm::fleet::minecraft", server = %ctx.id, generation = want.generation, error = %e, "relaunch failed");
                    if want.fork.is_some() {
                        fork_failed = Some(want.generation);
                    }
                    let delay = backoff;
                    backoff = (backoff * 2).min(ctx.agent.backoff_max);
                    ctx.report(
                        "failed",
                        want.generation,
                        BTreeMap::new(),
                        Some(e),
                        Some(delay),
                        None,
                    );
                    retry_after = Some(delay);
                }
            }
            continue;
        }
        let l = live.as_mut().expect("not behind, so a server runs");
        let died = tokio::select! {
            changed = desired.changed() => {
                if changed.is_err() {
                    break;
                }
                let want = desired.borrow().clone();
                if want.generation <= l.generation {
                    // The driver changed: the tile takes it at once.
                    l.scene = ctx.sync_scene(l.scene.take(), ctx.scene_for(&want, l.generation)).await;
                }
                None
            }
            status = l.vm.exited() => Some(format!("fcvm exited ({status}): {}", l.vm.tail())),
            _ = health_tick.tick() => {
                // A scene the tile did not take, or a VM that came back without one, is offered again.
                let want = desired.borrow().clone();
                l.scene = ctx.sync_scene(l.scene.take(), ctx.scene_for(&want, l.generation)).await;
                None
            }
            changed = scene.changed(), if scene_open => {
                if changed.is_ok() {
                    scene.borrow_and_update();
                    let want = desired.borrow().clone();
                    l.scene = ctx.sync_scene(l.scene.take(), ctx.scene_for(&want, l.generation)).await;
                } else {
                    scene_open = false;
                }
                None
            }
        };
        if let Some(error) = died {
            let mut l = live.take().unwrap();
            let ran = l.started.elapsed();
            tracing::warn!(target: "fcvm::fleet::minecraft", server = %ctx.id, %error, "server died");
            ctx.stop(&mut l).await;
            if ran >= ctx.agent.stable_after {
                backoff = ctx.agent.backoff_min;
            }
            let delay = backoff;
            backoff = (backoff * 2).min(ctx.agent.backoff_max);
            ctx.report(
                "failed",
                l.generation,
                BTreeMap::new(),
                Some(error),
                Some(delay),
                None,
            );
            retry_after = Some(delay);
        }
    }
    if let Some(mut l) = live.take() {
        ctx.stop(&mut l).await;
    }
}

/// The image's id and the servers' name from its labels (`podman image inspect`).
pub async fn inspect_image(podman: &Path, image: &str) -> Result<(String, Option<String>)> {
    let out = child(podman)
        .args([
            "image",
            "inspect",
            "--format",
            "{{.Id}}\t{{index .Labels \"fcvm.minecraft.server\"}}",
            image,
        ])
        .output()
        .await
        .with_context(|| format!("running {} image inspect", podman.display()))?;
    if !out.status.success() {
        bail!(
            "{image} is not in podman's store: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let (id, label) = text.trim().split_once('\t').unwrap_or((text.trim(), ""));
    let label = label.trim();
    Ok((
        id.to_string(),
        (!label.is_empty() && label != "<no value>").then(|| label.to_string()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> MinecraftCfg {
        MinecraftCfg {
            count: 2,
            image: "localhost/fcvm-minecraft:fleet".into(),
            image_id: "abc".into(),
            server: None,
            mem_mib: 1024,
            cpus: 1,
            bots: 3,
            snapshot_dir: "/s".into(),
            tls_dir: Some("/run/fcvm-fleet/tls".into()),
            gold_deadline: Duration::from_secs(300),
            ready_deadline: Duration::from_secs(60),
            snapshot_deadline: Duration::from_secs(120),
            poll: Duration::from_millis(20),
            steady: 5,
        }
    }

    #[test]
    fn the_gold_key_follows_the_image_and_the_vm_shape() {
        let a = cfg();
        let mut b = cfg();
        assert_eq!(a.gold_key(), b.gold_key());
        b.image_id = "abd".into();
        assert_ne!(a.gold_key(), b.gold_key());
        let mut c = cfg();
        c.mem_mib = 2048;
        assert_ne!(a.gold_key(), c.gold_key());
        assert!(
            a.gold_tag().starts_with("mc-gold-") && a.gold_tag().len() == "mc-gold-".len() + 12
        );
    }

    #[test]
    fn the_gold_boots_routed_with_its_bots_and_the_certificate() {
        let p = Place::routed("2001:db8::1:2".parse().unwrap());
        let a = gold_argv(&cfg(), "2001:db8::/64", &p).join(" ");
        assert!(a.starts_with("podman run --name mc-gold --network routed --ipv6-prefix 2001:db8::/64 --ipv6-address 2001:db8::1:2"), "{a}");
        assert!(a.contains("--mem 1024 --cpu 1 --env MCTILE_BOTS=3"), "{a}");
        assert!(
            a.contains("--map /run/fcvm-fleet/tls:/run/fleet-tls:ro"),
            "{a}"
        );
        assert!(a.ends_with(" localhost/fcvm-minecraft:fleet"), "{a}");
    }

    #[test]
    fn a_server_restores_at_its_address_and_only_the_gold_snapshot_stops_its_source() {
        let p = Place::routed("2001:db8::7".parse().unwrap());
        assert_eq!(
            restore_argv("mc-gold-x", "mc-3", "2001:db8::/64", &p),
            s(&[
                "snapshot",
                "run",
                "--snapshot",
                "mc-gold-x",
                "--name",
                "mc-3",
                "--ipv6-prefix",
                "2001:db8::/64",
                "--ipv6-address",
                "2001:db8::7",
                "--on-restore",
                RESTORE_HOOK
            ])
        );
        assert_eq!(
            snapshot_argv(42, "mc-gold-x", true)
                .last()
                .map(String::as_str),
            Some("--stop")
        );
        // A fork's snapshot never stops the source it copies.
        assert!(!snapshot_argv(42, "mc-fork-mc-2-5", false)
            .iter()
            .any(|a| a == "--stop"));
    }

    #[test]
    fn a_restored_tile_is_up_once_it_answers_with_a_chunk_and_every_bot() {
        let h = |ready, chunks, online| Health {
            ready,
            chunks,
            bots: Some(BotCount { online, wanted: 3 }),
            ..Default::default()
        };
        assert!(h(true, 10, 3).up());
        assert!(
            !h(true, 10, 2).up(),
            "a bot the restore disconnected is waited for"
        );
        assert!(!h(true, 0, 3).up());
        assert!(!h(false, 10, 3).up());
        assert!(
            !Health {
                ready: true,
                chunks: 10,
                ..Default::default()
            }
            .up(),
            "a tile that names no bots"
        );
    }

    #[test]
    fn the_gold_must_hold_the_tick_rate() {
        let h = |ready, tps| Health {
            ready,
            tps,
            ..Default::default()
        };
        assert!(h(true, Some(19.9)).steady());
        assert!(!h(true, Some(19.0)).steady());
        assert!(!h(true, None).steady());
        assert!(!h(false, Some(20.0)).steady());
    }

    /// A fork's snapshot is removed under its generation lock: while a create holds it (reading
    /// the snapshot as a second fork's parent) the removal waits, and the lock file stays.
    #[tokio::test]
    async fn removing_a_fork_snapshot_waits_for_its_generation_lock() {
        let d = tempfile::tempdir().unwrap();
        let snap = d.path().join("mc-fork-mc-2-3");
        std::fs::create_dir_all(snap.join("disk")).unwrap();
        std::fs::write(snap.join("config.json"), "{}").unwrap();
        let held = crate::commands::common::acquire_snapshot_dir_lock(&snap, false)
            .await
            .unwrap();
        let dir = d.path().to_path_buf();
        let removal = tokio::spawn(async move { remove_snapshot(&dir, "mc-fork-mc-2-3").await });
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(snap.exists(), "removed while a create held its lock");
        drop(held);
        tokio::time::timeout(Duration::from_secs(10), removal)
            .await
            .unwrap()
            .unwrap();
        assert!(!snap.exists());
        assert!(d.path().join("mc-fork-mc-2-3.lock").exists());
        // Removing again is quiet.
        remove_snapshot(d.path(), "mc-fork-mc-2-3").await;
    }
}
