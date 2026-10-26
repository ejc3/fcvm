//! `fcvm fleet coord`: the multiplayer authority.
//!
//! One task, the [`Actor`], owns the fleet state. Browsers, agents, the EC2 driver and the
//! tick timer only send it commands; it stamps each event with the time, runs
//! `fleet_core::apply`, and fans the result out: a snapshot for browsers (a watch channel,
//! so a slow socket skips to the latest), an `assign`/`scene` diff for each agent, the EC2
//! assignment, and the state file. No other task reads or writes the state, so there is
//! nothing to lock and no order to race on.

pub mod agents;
pub mod authority;
pub mod aws;
pub mod cert;
pub mod dns;
pub mod ec2;
pub mod ec2_aws;
pub mod ecs;
pub mod live;
pub mod poster;
pub mod scale;
pub mod secrets;
pub mod sigv4;
pub mod store;
pub mod text_check;
pub mod turnstile;
pub mod wake_log;

use crate::cli::{FleetAggregateCodec, FleetCoordArgs, FleetEc2, FleetTextCheck};
use crate::fleet::aggregate::codec::{Codec, Ffmpeg, Raw};
use crate::fleet::aggregate::{Layout, TileSpec};
use crate::fleet::now_ms;
use anyhow::{bail, Context, Result};
use fleet_core::protocol::{
    AgentMsg, AssignSlot, BudgetBody, ClaimMsg, CoordMsg, ServerMsg, SharedScene,
};
use fleet_core::tiling::Rect;
use fleet_core::view::{ec2_assignment, host_assignments, host_regions, shared_scene};
use fleet_core::{apply, view, Config, Event, Receipt, State};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};

/// The latest browser snapshot, serialized once for every socket.
#[derive(Debug)]
pub struct Snapshot {
    pub version: u64,
    pub text: String,
}

pub enum Cmd {
    /// Apply an event; its `at` is replaced with the actor's clock. Replies with the
    /// receipt for an intent.
    Apply {
        event: Event,
        reply: Option<oneshot::Sender<Option<Receipt>>>,
    },
    /// The receipt already held for an intent id.
    Receipt {
        id: String,
        reply: oneshot::Sender<Option<Receipt>>,
    },
    /// Would a `word` be refused before its text check? (`fleet_core::precheck_word`)
    PrecheckWord {
        client: String,
        args: serde_json::Value,
        reply: oneshot::Sender<Option<String>>,
    },
    /// The words in the sky now (a new word is checked together with them).
    Sky {
        reply: oneshot::Sender<Vec<String>>,
    },
    /// The claim a viewer holds and, once its renderer runs, its frame URL.
    ClaimFor {
        viewer: String,
        reply: oneshot::Sender<Option<ClaimMsg>>,
    },
    /// The current camera hold and the aggregators to take its poster from.
    PosterPlan {
        reply: oneshot::Sender<Option<poster::Plan>>,
    },
    /// An agent connection's hello. Replies whether the connection may name that host.
    AgentHello {
        conn: u64,
        msg: AgentMsg,
        tx: mpsc::UnboundedSender<CoordMsg>,
        reply: oneshot::Sender<bool>,
    },
    AgentMsg {
        conn: u64,
        msg: AgentMsg,
    },
    AgentClosed {
        conn: u64,
    },
    /// Stop: publish the last snapshot, write the state file, then answer whether the last
    /// state is stored.
    Shutdown {
        ack: oneshot::Sender<bool>,
    },
}

struct AgentConn {
    host: String,
    tx: mpsc::UnboundedSender<CoordMsg>,
    last_assign: Option<(Vec<AssignSlot>, Option<Rect>)>,
    last_scene: Option<SharedScene>,
    last_budget: Option<BudgetBody>,
}

/// The channels the actor publishes on.
pub struct Outputs {
    pub snapshot: watch::Sender<Arc<Snapshot>>,
    pub ec2: watch::Sender<Vec<AssignSlot>>,
    /// The EC2 half's aggregator layout.
    pub ec2_layout: watch::Sender<Option<Layout>>,
    /// The fleet's power state (`awake`, `asleep`, `waking`).
    pub power: watch::Sender<String>,
    /// The shared scene, for the EC2 tiles' renderers.
    pub scene: watch::Sender<Option<SharedScene>>,
    /// The grid budget, for the EC2 tiles' renderers.
    pub budget: watch::Sender<Option<BudgetBody>>,
    /// The slots' DNS records, when the coordinator writes them (zone, wanted records).
    pub dns: Option<watch::Sender<Vec<dns::Fact>>>,
    /// The launch names' subzone, when this coordinator serves it: its records follow the
    /// published names.
    pub zone: Option<(
        Arc<authority::ZoneConfig>,
        watch::Sender<Arc<authority::Zone>>,
    )>,
    pub store: Option<store::StoreTx>,
}

/// Minimum time between two browser snapshots; versions in between are skipped.
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(50);
/// Minimum time between two state file writes.
const STORE_INTERVAL: Duration = Duration::from_secs(1);
/// How many times shutdown asks for the last state to be written, and the pause between.
const SHUTDOWN_WRITE_ATTEMPTS: u32 = 3;
const SHUTDOWN_WRITE_PAUSE: Duration = Duration::from_secs(1);

pub struct Actor {
    state: State,
    /// The EC2 aggregator as the coordinator reaches it (it runs as our child).
    ec2_aggregate_local: Option<String>,
    out: Outputs,
    agents: HashMap<u64, AgentConn>,
    last_at: u64,
    published: u64,
    stored: u64,
    /// Quarantines agents reported and have not been told are stored: host, GPU address,
    /// the report's id, and the state version that holds the report.
    unacked: Vec<(String, String, Option<String>, u64)>,
    clock: Box<dyn Fn() -> u64 + Send>,
}

pub fn snapshot_of(state: &State) -> Arc<Snapshot> {
    let msg = ServerMsg::Snapshot {
        version: state.version,
        state: serde_json::to_value(view(state)).expect("the view serializes"),
    };
    Arc::new(Snapshot {
        version: state.version,
        text: serde_json::to_string(&msg).expect("a snapshot serializes"),
    })
}

impl Actor {
    pub fn new(state: State, out: Outputs, clock: Box<dyn Fn() -> u64 + Send>) -> Self {
        let mut actor = Actor {
            ec2_aggregate_local: None,
            stored: state.version,
            published: state.version,
            state,
            out,
            agents: HashMap::new(),
            unacked: Vec::new(),
            last_at: 0,
            clock,
        };
        // A resumed state's launches are named, and the subzone holds them, before the first
        // snapshot.
        actor.name_launches();
        actor.update_zone();
        actor.published = actor.state.version;
        actor.out.snapshot.send_replace(snapshot_of(&actor.state));
        actor
    }

    /// With the subzone served here, give every launch that needs names new ones and publish
    /// them in the same step: the zone the server answers from is updated from this state
    /// before any snapshot or claim names them.
    fn name_launches(&mut self) {
        let Some(apex) = self.out.zone.as_ref().map(|(c, _)| c.apex.clone()) else {
            return;
        };
        for f in dns::facts(&self.state) {
            if !dns::needs_names(&apex, &f) {
                continue;
            }
            let names = dns::names(
                &apex,
                &dns::mint(&f.id),
                f.generation,
                f.ipv6.as_deref(),
                f.ipv4.as_deref(),
                f.v4_port,
            );
            tracing::info!(slot = %f.id, label = %names.label, "names published");
            let at = self.stamp();
            let state = std::mem::replace(&mut self.state, State::new(Config::default(), 0));
            self.state = apply(
                state,
                &Event::Published {
                    at,
                    slot: f.id.clone(),
                    names,
                },
            );
        }
    }

    /// The subzone's records, from the published names.
    fn update_zone(&mut self) {
        if let Some((cfg, tx)) = &self.out.zone {
            let z = authority::zone_of(&self.state, &cfg.apex);
            tx.send_if_modified(|cur| {
                if cur.same_records(&z) {
                    false
                } else {
                    *cur = Arc::new(z);
                    true
                }
            });
        }
    }

    /// Event time: the clock, never earlier than the previous event, so `apply` sees time
    /// move forward in the order it applies events.
    fn stamp(&mut self) -> u64 {
        self.last_at = (self.clock)().max(self.last_at);
        self.last_at
    }

    fn apply(&mut self, mut event: Event) {
        let at = self.stamp();
        set_at(&mut event, at);
        let before = self.state.version;
        let marks = wake_log::Marks::of(&self.state);
        let state = std::mem::replace(&mut self.state, State::new(Config::default(), 0));
        self.state = apply(state, &event);
        self.name_launches();
        wake_log::log(&wake_log::notes(&marks, &self.state, at));
        if self.state.version != before {
            self.dispatch_agents();
        }
    }

    /// Send each agent its assignment and the scene when they differ from what it last got.
    fn dispatch_agents(&mut self) {
        // First: every output below can name a launch, and its reader may look it up at once.
        self.update_zone();
        let assignments = host_assignments(&self.state);
        let regions = host_regions(&self.state);
        let scene = shared_scene(&self.state);
        self.out.scene.send_if_modified(|cur| {
            let changed = cur.as_ref() != Some(&scene);
            *cur = Some(scene.clone());
            changed
        });
        let budget = self.state.fleet.budget.body();
        self.out.budget.send_if_modified(|cur| {
            let changed = *cur != budget;
            *cur = budget;
            changed
        });
        for conn in self.agents.values_mut() {
            let mine = (
                assignments.get(&conn.host).cloned().unwrap_or_default(),
                regions.get(&conn.host).copied().flatten(),
            );
            // The scene goes first, so a slot's first scene post has it.
            if conn.last_scene.as_ref() != Some(&scene) {
                let _ = conn.tx.send(CoordMsg::Scene(scene.clone()));
                conn.last_scene = Some(scene.clone());
            }
            if conn.last_assign.as_ref() != Some(&mine) {
                let _ = conn.tx.send(CoordMsg::Assign {
                    slots: mine.0.clone(),
                    region: mine.1,
                });
                conn.last_assign = Some(mine);
            }
            if let Some(b) = budget.filter(|b| conn.last_budget != Some(*b)) {
                let _ = conn.tx.send(CoordMsg::Budget(b));
                conn.last_budget = Some(b);
            }
        }
        let f = &self.state.fleet;
        let ec2_layout = f.layout.ec2.map(|region| Layout {
            epoch_unix_ms: f.scene.epoch_unix_ms,
            image_aspect: self.state.config.image_aspect,
            region,
            tiles: f
                .ec2
                .values()
                .filter_map(|s| {
                    Some(TileSpec {
                        id: s.id.clone(),
                        rect: s.rect?,
                        // The tile's published IPv6 name, as viewers get it.
                        url: fleet_core::view::frame_urls(&self.state, s).0,
                    })
                })
                .collect(),
            generation: f.layout.generation,
            from_frame: f.layout.from_frame,
        });
        self.out.ec2_layout.send_if_modified(|cur| {
            let changed = *cur != ec2_layout;
            *cur = ec2_layout.clone();
            changed
        });
        let power = self.state.fleet.power.state.clone();
        self.out.power.send_if_modified(|cur| {
            let changed = *cur != power;
            *cur = power.clone();
            changed
        });
        if let Some(tx) = &self.out.dns {
            let facts = dns::facts(&self.state);
            tx.send_if_modified(|cur| {
                let changed = *cur != facts;
                *cur = facts;
                changed
            });
        }
        let ec2 = ec2_assignment(&self.state);
        self.out.ec2.send_if_modified(|cur| {
            if *cur == ec2 {
                false
            } else {
                *cur = ec2;
                true
            }
        });
    }

    fn publish(&mut self) {
        if self.state.version != self.published {
            self.published = self.state.version;
            self.out.snapshot.send_replace(snapshot_of(&self.state));
        }
    }

    /// Queue the state for the writer if it changed; the returned receiver resolves once it
    /// is written (at once when nothing needed writing), with whether the newest state is
    /// stored. With `barrier`, an unchanged state still waits for the writes already
    /// queued, and retries one that failed, so shutdown does not end while the periodic
    /// store's last write is in flight or lost.
    fn store(&mut self, barrier: bool) -> oneshot::Receiver<bool> {
        let (ack, done) = oneshot::channel();
        let Some(tx) = &self.out.store else {
            let _ = ack.send(true);
            return done;
        };
        if self.state.version == self.stored {
            // Nothing new to write; the writer retries a state whose write failed, so an
            // unchanged state that is not stored yet is tried again every tick.
            if barrier {
                let _ = tx.send(store::StoreCmd {
                    bytes: None,
                    ack: Some(ack),
                });
            } else {
                let _ = tx.send(store::StoreCmd {
                    bytes: None,
                    ack: None,
                });
                let _ = ack.send(true);
            }
            return done;
        }
        match serde_json::to_vec(&self.state) {
            Ok(bytes) => {
                let _ = tx.send(store::StoreCmd {
                    bytes: Some(Arc::new(bytes)),
                    ack: Some(ack),
                });
                self.stored = self.state.version;
            }
            Err(e) => {
                tracing::error!(error = %e, "cannot serialize the state");
                let _ = ack.send(false);
            }
        }
        done
    }

    pub fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Apply { event, reply } => {
                let id = match &event {
                    Event::Intent { id, .. } => Some(id.clone()),
                    _ => None,
                };
                self.apply(event);
                if let Some(reply) = reply {
                    let _ = reply.send(id.and_then(|id| self.state.receipt(&id).cloned()));
                }
            }
            Cmd::Receipt { id, reply } => {
                let _ = reply.send(self.state.receipt(&id).cloned());
            }
            Cmd::PrecheckWord {
                client,
                args,
                reply,
            } => {
                let at = self.stamp();
                let _ = reply.send(fleet_core::precheck_word(&self.state, &client, &args, at));
            }
            Cmd::Sky { reply } => {
                let _ = reply.send(fleet_core::apply::sky(&self.state));
            }
            Cmd::PosterPlan { reply } => {
                let f = &self.state.fleet;
                // Starting tiles (a race, a first launch) are black until they run.
                if !fleet_core::apply::image_settled(f) {
                    let _ = reply.send(None);
                    return;
                }
                let d = &f.director;
                // Only aggregators with a region: one without (a host whose GPUs are all
                // on standby, the EC2 side with no tile) never sends a frame, and the
                // capture waits for every source. A host aggregator is read at its
                // published name when it has one (IPv4 first): the URL its agent reported
                // can be an address only that host reaches.
                let mut sources: Vec<String> = f
                    .hosts
                    .values()
                    .filter(|h| h.region.is_some())
                    .filter_map(|h| {
                        let (v6, v4) = fleet_core::view::host_aggregate_urls(&self.state, h);
                        v4.or(v6).or_else(|| h.aggregate_url.clone())
                    })
                    .collect();
                if let Some(u) = self
                    .ec2_aggregate_local
                    .clone()
                    .or_else(|| self.state.config.ec2_aggregate_url.clone())
                    .filter(|_| f.layout.ec2.is_some())
                {
                    sources.push(u);
                }
                let plan = poster::Plan {
                    step: d.step,
                    arrive_t: d.arrive_t,
                    hold_until_t: d.hold_until(),
                    viewpoint: fleet_core::director::viewpoints()[d.current]
                        .name
                        .to_string(),
                    epoch_unix_ms: f.scene.epoch_unix_ms,
                    sources,
                };
                let _ = reply.send(Some(plan));
            }
            Cmd::ClaimFor { viewer, reply } => {
                let info = self.state.fleet.slots.values().find_map(|s| {
                    let c = s.claim.as_ref().filter(|c| c.viewer == viewer)?;
                    let ready =
                        s.observed.phase == "running" && s.observed.generation >= c.generation;
                    Some(ClaimMsg {
                        id: c.session.clone(),
                        tile: c.borrowed.clone(),
                        phase: if ready { "running" } else { "restoring" }.into(),
                        frame_url: if ready {
                            fleet_core::view::frame_urls(&self.state, s).0
                        } else {
                            None
                        },
                        frame_url_v4: if ready {
                            fleet_core::view::frame_urls(&self.state, s).1
                        } else {
                            None
                        },
                        token: c.token.clone(),
                        clicked_at: c.claimed_at,
                        first_frame_at: c.first_frame_at,
                        expires_at: c.expires_at,
                        gpu: s.gpu.as_ref().map(|g| g.name.clone()),
                        phases: if ready {
                            s.observed.timings.clone()
                        } else {
                            Default::default()
                        },
                        camera: c.camera.clone(),
                        ended: None,
                    })
                });
                let _ = reply.send(info);
            }
            Cmd::AgentHello {
                conn,
                msg,
                tx,
                reply,
            } => {
                let AgentMsg::Hello { host, .. } = &msg else {
                    let _ = reply.send(false);
                    return;
                };
                let renamed = self.agents.get(&conn).is_some_and(|a| &a.host != host);
                if renamed {
                    tracing::warn!(%host, conn, "refusing a hello that renames its connection");
                    let _ = reply.send(false);
                    return;
                }
                // A host that dials again (`--dial` after a network drop) can find its old
                // connection still held here: nothing is written to an agent between
                // assignments, so a half-open one is not noticed. The new connection
                // replaces it. The host's slots stay (no AgentGone), and the old
                // connection's later messages and close are ignored, as its id is gone.
                let stale: Vec<u64> = self
                    .agents
                    .iter()
                    .filter(|(c, a)| **c != conn && &a.host == host)
                    .map(|(c, _)| *c)
                    .collect();
                for c in stale {
                    tracing::warn!(%host, old = c, new = conn, "a new connection replaces the host's old one");
                    self.agents.remove(&c);
                }
                let host = host.clone();
                self.agents.insert(
                    conn,
                    AgentConn {
                        host: host.clone(),
                        tx,
                        last_assign: None,
                        last_scene: None,
                        last_budget: None,
                    },
                );
                let _ = reply.send(true);
                self.apply(Event::Agent { at: 0, host, msg });
                // A host whose hello changed nothing still needs its first assignment.
                self.dispatch_agents();
            }
            Cmd::AgentMsg { conn, msg } => {
                let Some(host) = self.agents.get(&conn).map(|a| a.host.clone()) else {
                    return;
                };
                let quarantined = match &msg {
                    AgentMsg::Quarantine { sbdf, report, .. } => {
                        Some((sbdf.clone(), report.clone()))
                    }
                    _ => None,
                };
                self.apply(Event::Agent {
                    at: 0,
                    host: host.clone(),
                    msg,
                });
                if let Some((sbdf, report)) = quarantined {
                    self.unacked.push((host, sbdf, report, self.state.version));
                }
            }
            Cmd::Shutdown { ack } => {
                // `run` handles shutdown; a direct call only acknowledges.
                let _ = ack.send(true);
            }
            Cmd::AgentClosed { conn } => {
                if let Some(a) = self.agents.remove(&conn) {
                    tracing::info!(host = %a.host, "agent gone");
                    self.apply(Event::AgentGone {
                        at: 0,
                        host: a.host,
                    });
                }
            }
        }
    }

    /// Tell each agent whose quarantine report a stored state of `version` holds that it
    /// is stored. A host whose connection is gone is not told: it replays the report on its
    /// next connection, which is acknowledged in turn.
    fn quarantines_stored(&mut self, version: u64) {
        let agents = &self.agents;
        self.unacked.retain(|(host, sbdf, report, v)| {
            if *v > version {
                return true;
            }
            if let Some(a) = agents.values().find(|a| &a.host == host) {
                let _ = a.tx.send(CoordMsg::QuarantineStored {
                    sbdf: sbdf.clone(),
                    report: report.clone(),
                });
            }
            false
        });
    }

    /// Run until every command sender is dropped.
    pub async fn run(mut self, mut rx: mpsc::Receiver<Cmd>) {
        let mut snap_tick = tokio::time::interval(SNAPSHOT_INTERVAL);
        snap_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut store_tick = tokio::time::interval(STORE_INTERVAL);
        store_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // A write whose outcome acknowledges quarantines: the state version it holds, and
        // whether it was stored.
        let mut quarantine_write: Option<(u64, oneshot::Receiver<bool>)> = None;
        let ack = loop {
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(Cmd::Shutdown { ack }) => break Some(ack),
                    Some(c) => self.handle(c),
                    None => break None,
                },
                _ = snap_tick.tick() => self.publish(),
                _ = store_tick.tick() => {
                    if quarantine_write.is_none() && !self.unacked.is_empty() {
                        // The barrier's answer says whether the newest state, which holds
                        // every report so far, is stored (also when it was written before).
                        let version = self.state.version;
                        quarantine_write = Some((version, self.store(true)));
                    } else {
                        drop(self.store(false));
                    }
                }
                (version, stored) = written(&mut quarantine_write) => {
                    quarantine_write = None;
                    if stored {
                        self.quarantines_stored(version);
                    }
                }
            }
        };
        self.publish();
        let mut stored = false;
        for attempt in 1..=SHUTDOWN_WRITE_ATTEMPTS {
            // A writer that is gone cannot store anything.
            stored = self.store(true).await.unwrap_or(false);
            if stored {
                break;
            }
            tracing::warn!(attempt, "the last state is not stored yet");
            if attempt < SHUTDOWN_WRITE_ATTEMPTS {
                tokio::time::sleep(SHUTDOWN_WRITE_PAUSE).await;
            }
        }
        if !stored {
            tracing::error!(
                attempts = SHUTDOWN_WRITE_ATTEMPTS,
                "the last state was not stored"
            );
        }
        if let Some(ack) = ack {
            let _ = ack.send(stored);
        }
    }
}

/// The outcome of the write in `w` (pending forever without one).
async fn written(w: &mut Option<(u64, oneshot::Receiver<bool>)>) -> (u64, bool) {
    match w {
        Some((version, rx)) => (*version, rx.await.unwrap_or(false)),
        None => std::future::pending().await,
    }
}

fn set_at(event: &mut Event, t: u64) {
    match event {
        Event::Intent { at, .. }
        | Event::ViewerJoined { at, .. }
        | Event::ViewerLeft { at, .. }
        | Event::Tick { at }
        | Event::Agent { at, .. }
        | Event::AgentGone { at, .. }
        | Event::Ec2 { at, .. }
        | Event::Degraded { at, .. }
        | Event::ReleaseQuarantine { at, .. }
        | Event::Visibility { at, .. }
        | Event::ScaleInstances { at, .. }
        | Event::Ec2Health { at, .. }
        | Event::Published { at, .. } => *at = t,
    }
}

/// Everything the coordinator's tasks share.
#[derive(Clone)]
pub struct Handle {
    pub cmds: mpsc::Sender<Cmd>,
    pub snapshot: watch::Receiver<Arc<Snapshot>>,
    pub ec2_layout: watch::Receiver<Option<Layout>>,
}

impl Handle {
    pub async fn apply(&self, event: Event) -> Option<Receipt> {
        let (tx, rx) = oneshot::channel();
        self.cmds
            .send(Cmd::Apply {
                event,
                reply: Some(tx),
            })
            .await
            .ok()?;
        rx.await.ok().flatten()
    }

    /// Stop the actor; returns once the last state is stored, or its writes have failed,
    /// with whether it is stored.
    pub async fn shutdown(&self) -> bool {
        let (ack, done) = oneshot::channel();
        if self.cmds.send(Cmd::Shutdown { ack }).await.is_err() {
            return false;
        }
        done.await.unwrap_or(false)
    }

    pub async fn send(&self, event: Event) {
        let _ = self.cmds.send(Cmd::Apply { event, reply: None }).await;
    }

    pub async fn receipt(&self, id: &str) -> Option<Receipt> {
        let (tx, rx) = oneshot::channel();
        self.cmds
            .send(Cmd::Receipt {
                id: id.to_string(),
                reply: tx,
            })
            .await
            .ok()?;
        rx.await.ok().flatten()
    }

    pub async fn sky(&self) -> Vec<String> {
        let (tx, rx) = oneshot::channel();
        if self.cmds.send(Cmd::Sky { reply: tx }).await.is_err() {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    pub async fn poster_plan(&self) -> Option<poster::Plan> {
        let (tx, rx) = oneshot::channel();
        self.cmds.send(Cmd::PosterPlan { reply: tx }).await.ok()?;
        rx.await.ok().flatten()
    }

    pub async fn claim_for(&self, viewer: &str) -> Option<ClaimMsg> {
        let (tx, rx) = oneshot::channel();
        self.cmds
            .send(Cmd::ClaimFor {
                viewer: viewer.to_string(),
                reply: tx,
            })
            .await
            .ok()?;
        rx.await.ok().flatten()
    }

    pub async fn precheck_word(&self, client: &str, args: &serde_json::Value) -> Option<String> {
        let (tx, rx) = oneshot::channel();
        self.cmds
            .send(Cmd::PrecheckWord {
                client: client.to_string(),
                args: args.clone(),
                reply: tx,
            })
            .await
            .ok()?;
        rx.await.ok().flatten()
    }
}

/// How launches' DNS names are made.
pub enum Names {
    /// Written through the Cloudflare API and published once the zone's servers answer them.
    Cloudflare(dns::Cloudflare),
    /// Served by this coordinator as the subzone's authoritative server ([`authority`]):
    /// published as they are minted, with the zone sent on the channel.
    Subzone(
        Arc<authority::ZoneConfig>,
        watch::Sender<Arc<authority::Zone>>,
    ),
}

/// Start the actor, the tick timer and the EC2 driver; returns the handle the transports
/// use and the actor task.
pub fn start(
    state: State,
    store: Option<store::StoreTx>,
    ec2: Option<Arc<dyn ec2::Ec2Driver>>,
    ec2_aggregate_local: Option<String>,
    power: Option<scale::PowerConfig>,
    names: Option<Names>,
) -> (Handle, tokio::task::JoinHandle<()>) {
    let (cmds, rx) = mpsc::channel(4096);
    let (snap_tx, snap_rx) = watch::channel(Arc::new(Snapshot {
        version: 0,
        text: String::new(),
    }));
    let (ec2_tx, ec2_rx) = watch::channel(Vec::new());
    let (ec2_layout_tx, ec2_layout_rx) = watch::channel(None);
    let (power_tx, power_rx) = watch::channel(state.fleet.power.state.clone());
    let (scene_tx, scene_rx) = watch::channel(None);
    let (budget_tx, budget_rx) = watch::channel(state.fleet.budget.body());
    let mut state = state;
    // With named launches, tiles' frame URLs are the launches' published names.
    state.config.names_published = names.is_some();
    let (dns_writer, zone) = match names {
        Some(Names::Cloudflare(cf)) => (Some(cf), None),
        Some(Names::Subzone(cfg, tx)) => (None, Some((cfg, tx))),
        None => (None, None),
    };
    let dns_out = dns_writer.map(|cf| {
        let (tx, rx) = watch::channel(dns::facts(&state));
        (cf, tx, rx)
    });
    let actor = Actor::new(
        state,
        Outputs {
            snapshot: snap_tx,
            ec2: ec2_tx,
            ec2_layout: ec2_layout_tx,
            power: power_tx,
            scene: scene_tx,
            budget: budget_tx,
            dns: dns_out.as_ref().map(|(_, tx, _)| tx.clone()),
            zone,
            store,
        },
        Box::new(now_ms),
    );
    let actor = Actor {
        ec2_aggregate_local,
        ..actor
    };
    let task = tokio::spawn(actor.run(rx));
    let handle = Handle {
        cmds,
        snapshot: snap_rx,
        ec2_layout: ec2_layout_rx,
    };
    let tick = handle.clone();
    tokio::spawn(async move {
        let mut every = tokio::time::interval(Duration::from_secs(1));
        loop {
            every.tick().await;
            if tick
                .cmds
                .send(Cmd::Apply {
                    event: Event::Tick { at: 0 },
                    reply: None,
                })
                .await
                .is_err()
            {
                return;
            }
        }
    });
    if let Some(driver) = ec2 {
        tokio::spawn(ec2::run(
            driver,
            ec2_rx,
            scene_rx,
            budget_rx,
            handle.clone(),
        ));
    }
    if let Some(cfg) = power {
        tokio::spawn(scale::run(cfg, power_rx, handle.clone()));
    }
    if let Some((cf, _, rx)) = dns_out {
        tokio::spawn(dns::run(cf, rx, handle.clone()));
    }
    (handle, task)
}

/// The operator's endpoint: `POST /quarantine/release {"host", "sbdf"}` returns a
/// quarantined GPU to rotation.
pub fn admin_router(handle: Handle) -> axum::Router {
    #[derive(serde::Deserialize)]
    struct Release {
        host: String,
        sbdf: String,
    }
    axum::Router::new()
        .route(
            "/quarantine/release",
            axum::routing::post(
                |axum::extract::State(h): axum::extract::State<Handle>,
                 axum::Json(r): axum::Json<Release>| async move {
                    tracing::info!(host = %r.host, sbdf = %r.sbdf, "operator released a quarantined GPU");
                    h.send(Event::ReleaseQuarantine {
                        at: 0,
                        host: r.host,
                        sbdf: r.sbdf,
                    })
                    .await;
                    axum::Json(serde_json::json!({"ok": true}))
                },
            ),
        )
        .with_state(handle)
}

/// Whether the fleet sleeps with no active viewer: it scales its host groups, or it was told
/// to sleep on fixed hosts.
pub fn power_managed(args: &FleetCoordArgs) -> bool {
    !args.scale_groups.is_empty() || args.sleep_when_idle
}

pub async fn run(args: FleetCoordArgs) -> Result<()> {
    if args.ec2_aggregate_listen.is_some()
        && args.ec2_aggregate_url.is_none()
        && !args.ec2_aggregate_tls
    {
        bail!("--ec2-aggregate-listen needs --ec2-aggregate-url or --ec2-aggregate-tls (how browsers reach it)");
    }
    let secrets_region = || {
        args.secrets_region
            .clone()
            .or_else(|| std::env::var("AWS_REGION").ok())
            .context("reading a secret needs --secrets-region or AWS_REGION")
    };
    // The EC2 aggregator serving browsers: the task's own addresses, fixed for its life.
    let ec2_public = if args.ec2_aggregate_tls {
        let region = std::env::var("AWS_REGION")
            .context("--ec2-aggregate-tls reads the task's addresses in AWS_REGION")?;
        let a = ecs::task_addresses(&region).await?;
        tracing::info!(ipv6 = ?a.ipv6, ipv4 = ?a.ipv4, "the EC2 aggregator's public addresses");
        Some(a)
    } else {
        None
    };
    let config = Config {
        max_tiles: args.max_tiles,
        ec2_max: match args.ec2 {
            FleetEc2::None => 0,
            FleetEc2::Fake | FleetEc2::Aws => args.ec2_tiles.min(args.max_tiles),
        },
        ec2_aggregate_url: args.ec2_aggregate_url.clone(),
        ec2_aggregate_ipv6: ec2_public.as_ref().and_then(|a| a.ipv6.clone()),
        ec2_aggregate_ipv4: ec2_public.as_ref().and_then(|a| a.ipv4.clone()),
        race_interval_secs: args.race_every,
        restart_cooldown_secs: args.restart_cooldown,
        camera_seed: args.camera_seed,
        max_borrowed: args.max_borrowed,
        power_managed: power_managed(&args),
        idle_scale_down_secs: args.idle_scale_down,
        ..Config::default()
    };
    if config.max_tiles == 0 {
        bail!("--max-tiles must be at least 1");
    }
    let checker: Arc<dyn text_check::TextChecker> = match args.text_check {
        FleetTextCheck::None => Arc::new(text_check::NoCheck),
        FleetTextCheck::Bedrock => {
            let model = args
                .text_check_model
                .context("--text-check bedrock needs --text-check-model")?;
            let region = args
                .text_check_region
                .context("--text-check bedrock needs --text-check-region")?;
            Arc::new(text_check::Bedrock::new(region, model)?)
        }
    };
    let (state, store_tx) = match &args.state_file {
        Some(path) => {
            let state = match store::load(path)? {
                Some(s) => {
                    tracing::info!(path = %path.display(), version = s.version, "resuming from the state file");
                    s.resumed(config, now_ms())
                }
                None => State::new(config, now_ms()),
            };
            (state, Some(store::spawn_writer(path.clone())))
        }
        None => match &args.state_bucket {
            Some(bucket) => {
                let region = args
                    .state_region
                    .clone()
                    .or_else(|| std::env::var("AWS_REGION").ok())
                    .context("--state-bucket needs --state-region or AWS_REGION")?;
                let obj = store::S3Object::new(bucket, &args.state_key, &region)?;
                let state = match obj.load().await? {
                    Some(s) => {
                        tracing::info!(bucket = %bucket, key = %args.state_key, version = s.version, "resuming from the state object");
                        s.resumed(config, now_ms())
                    }
                    None => State::new(config, now_ms()),
                };
                (state, Some(store::spawn_s3_writer(obj)))
            }
            None => (State::new(config, now_ms()), None),
        },
    };
    let ec2: Option<Arc<dyn ec2::Ec2Driver>> = match args.ec2 {
        FleetEc2::None => None,
        FleetEc2::Fake => Some(Arc::new(ec2::FakeEc2::new(
            Duration::from_secs(2),
            Duration::from_secs(20),
        ))),
        FleetEc2::Aws => {
            let mut d = ec2_aws::AwsEc2::new(
                args.ec2_regions.clone(),
                args.ec2_template.clone(),
                args.ec2_on_demand_template.clone(),
                args.dns_zone.clone(),
            )?;
            d.fleet = args.fleet_name.clone();
            d.pool_max = args.ec2_pool;
            d.pool_template = args.ec2_pool_template.clone();
            d.networks = ec2_aws::networks(&args.ec2_subnets, &args.ec2_security_group)?;
            Some(Arc::new(d))
        }
    };
    let ec2_local = args.ec2_aggregate_listen.as_ref().map(|l| {
        format!(
            "ws://{}/frames",
            crate::fleet::aggregate::child::connect_addr(l)
        )
    });
    let on_demand = scale::pair_on_demand(&args.scale_groups, &args.on_demand_groups)?;
    let power = if args.scale_groups.is_empty() {
        None
    } else {
        Some(scale::PowerConfig {
            driver: Arc::new(scale::AwsAutoScaling::new()?),
            groups: args.scale_groups.clone(),
            awake_capacity: args.awake_capacity,
            // While the groups come up: a failed spot launch is seen at most 2 s after Auto
            // Scaling records it. Two or three describe calls per poll, only then.
            poll: Duration::from_secs(2),
            on_demand,
            spot_wait: Duration::from_secs(args.spot_wait),
        })
    };
    let dns_token = match &args.dns_token_secret {
        Some(name) => Some(
            secrets::Secrets::new(&secrets_region()?)?
                .get(name)
                .await?
                .trim()
                .to_string(),
        ),
        None => None,
    };
    let cloudflare = |token: &str| {
        dns::Cloudflare::new(
            token.to_string(),
            args.dns_zone.clone(),
            args.dns_zone_id.clone(),
        )
    };
    let names = match &args.dns_subzone {
        Some(sub) => {
            let parent = args.dns_zone.trim_end_matches('.').to_ascii_lowercase();
            if !sub
                .trim_end_matches('.')
                .to_ascii_lowercase()
                .ends_with(&format!(".{parent}"))
            {
                bail!("--dns-subzone {sub} is not below --dns-zone {parent}");
            }
            // The certificate's DNS-01 records for the subzone's wildcard live in the parent
            // zone, at the name the parent's own challenge uses.
            let cfg = Arc::new(authority::ZoneConfig::new(
                sub,
                &args.dns_subzone_ns,
                Some(&format!("_acme-challenge.{parent}")),
            )?);
            let (tx, rx) = watch::channel(Arc::new(authority::Zone::default()));
            let listen = args
                .dns_listen
                .context("--dns-subzone needs --dns-listen")?;
            let bound = authority::Authority {
                cfg: cfg.clone(),
                zone: rx,
            }
            .serve(listen, args.dns_rate_limit)
            .await?;
            tracing::info!(zone = %cfg.apex, listen = %bound, "serving the launch names' subzone");
            // Launch names written to the parent zone before the switch are referenced by
            // nothing now.
            if let Some(t) = &dns_token {
                tokio::spawn(dns::sweep(cloudflare(t)?));
            }
            Some(Names::Subzone(cfg, tx))
        }
        None => match &dns_token {
            Some(t) => {
                if args.dns_nameservers.is_empty() {
                    tracing::warn!(
                        "--dns-nameservers is empty: launches' names are published without confirming them at the zone's authoritative servers"
                    );
                }
                Some(Names::Cloudflare(cloudflare(t)?.with_confirm(Arc::new(
                    dns::Authoritative {
                        servers: args.dns_nameservers.clone(),
                    },
                ))))
            }
            None => None,
        },
    };
    if let Some(secret) = &args.tls_secret {
        let token = dns_token
            .as_deref()
            .context("--tls-secret needs --dns-token-secret (DNS-01 challenges)")?;
        let mut renewal = cert::Renewal::new(secret.clone(), args.dns_zone.clone());
        if let Some(sub) = &args.dns_subzone {
            renewal.extra.push(format!(
                "*.{}",
                sub.trim_end_matches('.').to_ascii_lowercase()
            ));
        }
        if let Some(d) = &args.acme_directory {
            renewal.directory = d.clone();
        }
        tokio::spawn(cert::run(
            renewal,
            secrets::Secrets::new(&secrets_region()?)?,
            cloudflare(token)?,
        ));
    }
    let ec2_sweep = ec2.clone();
    let (handle, _actor) = start(state, store_tx, ec2, ec2_local, power, names);
    if let Some(listen) = &args.ec2_aggregate_listen {
        let mut argv = vec![
            std::env::current_exe()
                .context("locating this fcvm binary")?
                .to_string_lossy()
                .into_owned(),
            "fleet".into(),
            "aggregate".into(),
        ];
        argv.extend(args.aggregate_args.iter().cloned());
        if let (Some(a), Some(secret)) = (&ec2_public, &args.tls_secret) {
            // The certificate file, written before the aggregator starts and kept current.
            use crate::fleet::agent::tls;
            let dir = args.ec2_aggregate_tls_dir.clone();
            let secrets = secrets::Secrets::new(&secrets_region()?)?;
            tls::refresh(&secrets, secret, &dir).await?;
            tokio::spawn(tls::run(secrets, secret.clone(), dir.clone()));
            argv.extend([
                "--tls-file".into(),
                dir.join(tls::FILE).to_string_lossy().into_owned(),
            ]);
            if a.ipv4.is_some() {
                argv.extend(["--tls-listen".into(), format!("0.0.0.0:{}", tls::PORT)]);
            }
            if let Some(v6) = &a.ipv6 {
                argv.extend(["--tls-listen".into(), format!("[{v6}]:{}", tls::PORT)]);
            }
        }
        tokio::spawn(crate::fleet::aggregate::child::supervise(
            argv,
            listen.clone(),
            handle.ec2_layout.clone(),
            || {},
        ));
    }
    for cmd in args.agent_cmds {
        let argv =
            shell_words::split(&cmd).with_context(|| format!("splitting --agent-cmd {cmd:?}"))?;
        if argv.is_empty() {
            bail!("--agent-cmd is empty");
        }
        tokio::spawn(agents::run_command(argv, handle.clone()));
    }
    let listener = tokio::net::TcpListener::bind(&args.listen)
        .await
        .with_context(|| format!("listening on {}", args.listen))?;
    tracing::info!(listen = %args.listen, "coordinator serving /live");
    eprintln!("fleet coordinator listening on {}", listener.local_addr()?);
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    if let Some(addr) = &args.admin_listen {
        let admin = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("listening on {addr}"))?;
        eprintln!("fleet coordinator admin on {}", admin.local_addr()?);
        let h = handle.clone();
        tokio::spawn(async move {
            if let Err(e) = axum::serve(admin, admin_router(h)).await {
                tracing::error!(error = %e, "admin endpoint stopped");
            }
        });
    }
    let verifier: Arc<dyn turnstile::Verifier> = match args.turnstile_secret {
        Some(secret) => Arc::new(turnstile::Siteverify::new(secret, None)?),
        None => Arc::new(turnstile::Off),
    };
    let poster = Arc::new(poster::Poster::default());
    let (decoder, encoder): (Arc<dyn Codec>, poster::StillEncoder) = match args.poster_codec {
        FleetAggregateCodec::Ffmpeg => (
            Arc::new(Ffmpeg {
                bin: args.ffmpeg.clone(),
            }),
            poster::StillEncoder::Jpeg {
                ffmpeg: args.ffmpeg.clone(),
            },
        ),
        FleetAggregateCodec::Raw => (Arc::new(Raw), poster::StillEncoder::Ppm),
    };
    tokio::spawn(poster::run(
        handle.clone(),
        poster.clone(),
        decoder,
        encoder,
    ));
    let cfg = live::LiveConfig {
        checker,
        allowed_origins: args.allowed_origins,
        verifier,
        trust_cf_connecting_ip: args.trust_cf_connecting_ip,
        turnstile_site_key: args.turnstile_site_key,
        poster,
    };
    tokio::select! {
        r = live::serve(listener, handle.clone(), cfg) => r?,
        _ = term.recv() => tracing::info!("SIGTERM"),
        _ = int.recv() => tracing::info!("SIGINT"),
    }
    // The state file holds the last state before the process exits; the agents' commands
    // are killed when their handles drop, and their VMs follow them (PR_SET_PDEATHSIG).
    let stored = handle.shutdown().await;
    if stored {
        tracing::info!("coordinator stopped; state written");
    }
    // The next coordinator does not know these instances; terminate them now (it also
    // sweeps at its start, for a coordinator that was killed).
    if let Some(d) = ec2_sweep {
        match tokio::time::timeout(Duration::from_secs(20), d.sweep()).await {
            Ok(Ok(n)) => tracing::info!(instances = n, "EC2 tiles terminated"),
            Ok(Err(e)) => tracing::warn!(error = %format!("{e:#}"), "EC2 tiles not terminated"),
            Err(_) => tracing::warn!("EC2 tiles not terminated within 20 s"),
        }
    }
    anyhow::ensure!(
        stored,
        "the coordinator stopped without storing its last state"
    );
    Ok(())
}

#[cfg(test)]
mod power_tests {
    use crate::cli::{Cli, Commands, FleetCommand};
    use clap::Parser;

    fn coord(extra: &[&str]) -> crate::cli::FleetCoordArgs {
        let mut argv = vec!["fcvm", "fleet", "coord", "--listen", "127.0.0.1:0"];
        argv.extend_from_slice(extra);
        match Cli::try_parse_from(argv).expect("parses").cmd {
            Commands::Fleet(f) => match f.cmd {
                FleetCommand::Coord(a) => *a,
                _ => panic!("coord"),
            },
            _ => panic!("fleet"),
        }
    }

    #[test]
    fn the_fleet_sleeps_with_scale_groups_or_when_told_to() {
        assert!(!super::power_managed(&coord(&[])));
        assert!(super::power_managed(&coord(&["--sleep-when-idle"])));
        assert!(super::power_managed(&coord(&[
            "--scale-group",
            "us-east-1:fcvm-fleet-hosts"
        ])));
    }
}

#[cfg(test)]
mod zone_order_tests {
    /// Every output `dispatch_agents` sends can name a launch (the EC2 aggregator's layout
    /// carries the tiles' published URLs), and its reader may dial the name at once, so the
    /// served zone is updated before the first of them: a lookup that ran ahead of the zone
    /// would get an NXDOMAIN its resolver keeps for the negative TTL.
    #[test]
    fn the_zone_is_updated_before_any_output_that_can_name_a_launch() {
        let src = include_str!("mod.rs");
        let body = src
            .split("    fn dispatch_agents(&mut self) {")
            .nth(1)
            .expect("dispatch_agents")
            .split("\n    }\n")
            .next()
            .unwrap();
        let zone = body
            .find("self.update_zone();")
            .expect("dispatch_agents updates the zone");
        let first_send = body
            .find(".send")
            .expect("dispatch_agents sends its outputs");
        assert!(
            zone < first_send,
            "dispatch_agents sends an output before it updates the zone"
        );
    }
}
