//! `fcvm fleet coord`: the multiplayer authority.
//!
//! One task, the [`Actor`], owns the fleet state. Browsers, agents, the EC2 driver and the
//! tick timer only send it commands; it stamps each event with the time, runs
//! `fleet_core::apply`, and fans the result out: a snapshot for browsers (a watch channel,
//! so a slow socket skips to the latest), an `assign`/`scene` diff for each agent, the EC2
//! assignment, and the state file. No other task reads or writes the state, so there is
//! nothing to lock and no order to race on.

pub mod access_jwt;
pub mod admission;
pub mod agents;
pub mod authority;
pub mod aws;
pub mod broadcast;
pub mod cert;
pub mod dns;
pub mod ec2;
pub mod ec2_aws;
pub mod ecs;
pub mod hosts;
pub mod lambda;
pub mod lambda_front;
pub mod live;
pub mod metrics;
pub mod minecraft_log;
pub mod minecraft_status;
pub mod poster;
pub mod potato_log;
pub mod scene_token;
pub mod secrets;
pub mod sigv4;
pub mod spend;
pub mod store;
pub mod text_check;
pub mod turnstile;
pub mod wake_log;

use crate::cli::{FleetAggregateCodec, FleetCoordArgs, FleetEc2, FleetLambda, FleetTextCheck};
use crate::fleet::aggregate::codec::{Codec, Ffmpeg, Raw};
use crate::fleet::aggregate::{Layout, TileSpec};
use crate::fleet::now_ms;
use crate::fleet::tunnel;
use anyhow::{bail, Context, Result};
use fleet_core::protocol::{
    AgentMsg, AssignSlot, BudgetPlan, ClaimMsg, CoordMsg, ServerMsg, SharedScene,
};
use fleet_core::tiling::Rect;
use fleet_core::view::{
    ec2_assignment, host_assignments, host_regions, lambda_assignment, shared_scene,
};
use fleet_core::{apply, view, Config, Event, Receipt, State};
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
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
    /// The admission keys (hex, newest first) renderers and aggregators check direct
    /// tokens with; empty turns the check off.
    AdmitKeys(Vec<String>),
    /// The keys phone streamers check GPU mirror tokens with (hex, newest first).
    MirrorKeys(Vec<String>),
    /// Apply an event; its `at` is replaced with the actor's clock. Replies with the
    /// receipt for an intent: a committed one only once a stored state holds it (the reply
    /// is dropped if none ever does), a refused one at once.
    Apply {
        event: Event,
        reply: Option<oneshot::Sender<Option<Receipt>>>,
    },
    /// Apply an event as `Apply` does, and reply at once with the receipt and a receiver
    /// that says whether a stored state holds the receipt's change. The caller sends a
    /// committed receipt only after that receiver says true.
    Submit {
        event: Event,
        reply: oneshot::Sender<Submitted>,
    },
    /// The receipt already held for an intent id, when a stored state holds it.
    Receipt {
        id: String,
        reply: oneshot::Sender<Option<Receipt>>,
    },
    /// Would a `word` be refused before its text check? (`fleet_core::precheck_word`)
    PrecheckWord {
        client: String,
        /// `word`, or `renders_overlay` (pages mode's text).
        name: String,
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

/// An applied intent's receipt, and whether a stored state holds its change: true once a
/// write that holds it is stored, false (or the sender dropped) when the coordinator stops
/// without storing it. A refused receipt changes nothing and is resolved at once.
pub struct Submitted {
    pub receipt: Option<Receipt>,
    pub stored: oneshot::Receiver<bool>,
    /// For a committed `claim_gpu` or `resume_claim`: the claim as it is in the same actor
    /// turn that applied the intent, so a claim that ends before its receipt goes is still
    /// known to end.
    pub claim: Option<ClaimMsg>,
}

struct AgentConn {
    host: String,
    tx: mpsc::UnboundedSender<CoordMsg>,
    last_assign: Option<(Vec<AssignSlot>, Option<Rect>)>,
    last_scene: Option<SharedScene>,
    last_budget: Option<BudgetPlan>,
    /// Potato mode: the round this host was last told to play (`Some(None)`: told none).
    last_potato: Option<Option<fleet_core::potato::PotatoRound>>,
    /// The pages-mode order last sent to this host; none to a host that does not run it.
    last_renders: Option<fleet_core::renders::RendersOrder>,
}

/// The channels the actor publishes on.
pub struct Outputs {
    pub snapshot: watch::Sender<Arc<Snapshot>>,
    pub ec2: watch::Sender<Vec<AssignSlot>>,
    /// The Lambda tiles the Lambda driver runs (`lambda_assignment`).
    pub lambda: watch::Sender<Vec<AssignSlot>>,
    /// The EC2 half's aggregator layout.
    pub ec2_layout: watch::Sender<Option<Layout>>,
    /// The broadcast aggregator's layout: the whole image, or `None` when nothing shows.
    pub broadcast_layout: watch::Sender<Option<Layout>>,
    /// The fleet's power state (`awake`, `asleep`, `waking`).
    pub power: watch::Sender<String>,
    /// Spot interruption notices for the host launcher, by host name (the instance id of a
    /// host the coordinator launched).
    pub draining: DrainNotices,
    /// The shared scene, for the EC2 tiles' renderers.
    pub scene: watch::Sender<Option<SharedScene>>,
    /// The grid budget, for the EC2 tiles' renderers.
    pub budget: watch::Sender<Option<BudgetPlan>>,
    /// The launch names' subzone, when this coordinator serves it: its records follow the
    /// published names.
    pub zone: Option<Names>,
    pub store: Option<store::StoreTx>,
    /// Turns true once a write the store acknowledged confirms that this coordinator owns the
    /// state object (true from the start without a store); the EC2 driver waits for it.
    pub owned: watch::Sender<bool>,
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
    /// The admission keys every scene and layout carries (empty: no check).
    admit_keys: Vec<String>,
    /// The GPU mirror's keys, in every scene for the phones (see `SharedScene::mirror_keys`).
    mirror_keys: Vec<String>,
    out: Outputs,
    agents: HashMap<u64, AgentConn>,
    last_at: u64,
    published: u64,
    /// The newest state version queued for the writer.
    stored: u64,
    /// The newest state version a write the store acknowledged holds.
    durable: u64,
    /// Answers waiting for a stored state that holds their version (receipts, see
    /// `Cmd::Apply`), oldest first.
    waiting: Vec<(u64, oneshot::Sender<bool>)>,
    /// Quarantines agents reported and have not been told are stored: host, GPU address,
    /// the report's id, and the state version that holds the report.
    unacked: Vec<(String, String, Option<String>, u64)>,
    clock: Box<dyn Fn() -> u64 + Send>,
}

/// The keys the coordinator hands out at start: the direct-stream keys and the GPU mirror's.
/// Both always, with or without a broadcast: every viewer the coordinator admits gets a direct
/// token, so a renderer, aggregator, phone or server that checks one turns away only a client
/// that never came through `/live` (and its per-client limits). Without a broadcast every
/// connection is seated, so checking costs no viewer its picture.
pub fn admission_keys(gate: &admission::Gate) -> (Vec<String>, Vec<String>) {
    let key = gate.key.hex();
    (vec![key.clone()], vec![key])
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
            admit_keys: Vec::new(),
            mirror_keys: Vec::new(),
            stored: state.version,
            durable: state.version,
            waiting: Vec::new(),
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
        let Some(apex) = self.out.zone.as_ref().map(|n| n.cfg.apex.clone()) else {
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
        if let Some(n) = &self.out.zone {
            let z = authority::zone_of(&self.state, &n.cfg.apex);
            n.zone.send_if_modified(|cur| {
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
        let potato_marks = potato_log::Marks::of(&self.state);
        let minecraft_marks = minecraft_log::Marks::of(&self.state);
        let was_asleep = self.state.fleet.power.state == "asleep";
        let state = std::mem::replace(&mut self.state, State::new(Config::default(), 0));
        self.state = apply(state, &event);
        self.name_launches();
        wake_log::log(&wake_log::notes(&marks, &self.state, at));
        potato_log::log_rounds(&potato_log::notes(&potato_marks, &self.state));
        minecraft_log::log(&minecraft_log::notes(&minecraft_marks, &self.state));
        let f = &self.state.fleet;
        if !was_asleep && f.power.state == "asleep" {
            tracing::info!(
                viewers = f.viewers.len(),
                hidden = f.hidden.len(),
                last_arrival_at = ?f.power.last_arrival_at,
                reason = ?f.power.reason,
                spend_usd_today = f.spend.usd,
                "fleet asleep"
            );
        }
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
        let mut scene = shared_scene(&self.state);
        scene.admit_keys = self.admit_keys.clone();
        scene.mirror_keys = self.mirror_keys.clone();
        self.out.scene.send_if_modified(|cur| {
            let changed = cur.as_ref() != Some(&scene);
            *cur = Some(scene.clone());
            changed
        });
        let budget = self.state.fleet.budget.body();
        self.out.budget.send_if_modified(|cur| {
            let changed = *cur != budget;
            *cur = budget.clone();
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
                // Every assignment a host is sent, so a host that starts nothing after one
                // is told apart from one that was never sent it (issue 1437).
                let placed = mine.0.iter().filter(|a| a.rect.is_some()).count();
                let standby = mine.0.iter().filter(|a| a.standby).count();
                let warm = mine.0.iter().filter(|a| a.warm).count();
                tracing::info!(
                    host = %conn.host,
                    slots = mine.0.len(),
                    placed,
                    standby,
                    warm,
                    first = conn.last_assign.is_none(),
                    "assignment sent"
                );
                let _ = conn.tx.send(CoordMsg::Assign {
                    slots: mine.0.clone(),
                    region: mine.1,
                });
                conn.last_assign = Some(mine);
            }
            if let Some(b) = budget
                .clone()
                .filter(|b| conn.last_budget.as_ref() != Some(b))
            {
                let _ = conn.tx.send(CoordMsg::Budget(b.clone()));
                conn.last_budget = Some(b);
            }
            // Potato mode: the potato host's round, and none for a host that runs players
            // but is not the potato host.
            let potato = (self.state.fleet.potato.host.as_deref() == Some(conn.host.as_str()))
                .then(|| self.state.fleet.potato.order())
                .flatten();
            let runs_players = conn.last_potato.is_some()
                || self.state.fleet.potato.host.as_deref() == Some(conn.host.as_str());
            if runs_players && conn.last_potato.as_ref() != Some(&potato) {
                tracing::info!(host = %conn.host, round = ?potato.as_ref().map(|r| r.round), "potato round sent");
                let _ = conn.tx.send(CoordMsg::Potato {
                    round: potato.clone(),
                });
                conn.last_potato = Some(potato);
            }
            // Pages mode: only a host whose hello said it runs the mode gets an order (an
            // agent built before the mode ends its connection on a message it cannot parse).
            if self.state.fleet.renders.hosts.contains_key(&conn.host) {
                let awake = self.state.fleet.power.state != "asleep";
                let order = self.state.fleet.renders.order(awake);
                if conn.last_renders.as_ref() != Some(&order) {
                    tracing::info!(
                        host = %conn.host,
                        run = order.run,
                        overlay = ?order.overlay.as_ref().map(|o| o.seq),
                        "pages order sent"
                    );
                    let _ = conn.tx.send(CoordMsg::Renders {
                        order: order.clone(),
                    });
                    conn.last_renders = Some(order);
                }
            }
        }
        let f = &self.state.fleet;
        let ec2_layout = f.layout.ec2.map(|region| Layout {
            epoch_unix_ms: f.scene.epoch_unix_ms,
            image_aspect: self.state.config.image_aspect,
            region,
            // The AWS half is one grid of both backings: a Lambda tile's published name
            // reaches it through the coordinator's front and its tunnel.
            tiles: f
                .ec2
                .values()
                .chain(f.lambda.values())
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
            admit_keys: self.admit_keys.clone(),
        });
        self.out.ec2_layout.send_if_modified(|cur| {
            let changed = *cur != ec2_layout;
            *cur = ec2_layout.clone();
            changed
        });
        let broadcast_layout = broadcast::layout(&self.state).map(|l| Layout {
            admit_keys: self.admit_keys.clone(),
            ..l
        });
        self.out.broadcast_layout.send_if_modified(|cur| {
            let changed = *cur != broadcast_layout;
            *cur = broadcast_layout.clone();
            changed
        });
        let power = self.state.fleet.power.state.clone();
        self.out.power.send_if_modified(|cur| {
            let changed = *cur != power;
            *cur = power.clone();
            changed
        });
        self.out.draining.publish(
            self.state
                .fleet
                .hosts
                .iter()
                .filter(|(_, h)| h.draining)
                .map(|(name, _)| name.clone())
                .collect(),
        );
        let ec2 = ec2_assignment(&self.state);
        self.out.ec2.send_if_modified(|cur| {
            if *cur == ec2 {
                false
            } else {
                *cur = ec2;
                true
            }
        });
        let lambda = lambda_assignment(&self.state);
        self.out.lambda.send_if_modified(|cur| {
            if *cur == lambda {
                false
            } else {
                *cur = lambda;
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

    /// Resolves true once a write the store acknowledged holds state `version` (at once when
    /// one already does, or there is no store), false when the actor stops first.
    fn when_stored(&mut self, version: u64) -> oneshot::Receiver<bool> {
        let (tx, rx) = oneshot::channel();
        if self.out.store.is_none() || version <= self.durable {
            let _ = tx.send(true);
        } else {
            // An answer nobody waits for any more (its socket closed) goes: during a store
            // outage every reconnect submits its unanswered intents again.
            self.waiting.retain(|(_, w)| !w.is_closed());
            self.waiting.push((version, tx));
        }
        rx
    }

    /// Whether a stored state holds `receipt`: a committed receipt waits for one, since the
    /// viewer is told its change is made; a refused one changed nothing and is answered now.
    fn receipt_stored(&mut self, receipt: Option<&Receipt>) -> oneshot::Receiver<bool> {
        match receipt {
            Some(r) if r.outcome == fleet_core::Outcome::Committed => self.when_stored(r.version),
            _ => self.when_stored(0),
        }
    }

    /// Send `receipt` on `reply` once a stored state holds it; drop `reply` if none does.
    fn reply_stored(&mut self, reply: oneshot::Sender<Option<Receipt>>, receipt: Option<Receipt>) {
        let mut stored = self.receipt_stored(receipt.as_ref());
        if let Ok(true) = stored.try_recv() {
            let _ = reply.send(receipt);
            return;
        }
        // Once nobody awaits the reply (its socket closed), the wait ends, which closes the
        // answer `when_stored` keeps, so the next one dropped it.
        let mut reply = reply;
        tokio::spawn(async move {
            tokio::select! {
                s = &mut stored => {
                    if s == Ok(true) {
                        let _ = reply.send(receipt);
                    }
                }
                _ = reply.closed() => {}
            }
        });
    }

    /// A write the store acknowledged holds every version up to `version`: answer what waited
    /// for it.
    fn stored_through(&mut self, version: u64) {
        self.durable = self.durable.max(version);
        self.out.owned.send_if_modified(|o| {
            let was = *o;
            *o = true;
            !was
        });
        let durable = self.durable;
        let (done, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.waiting)
            .into_iter()
            .partition(|(v, _)| *v <= durable);
        self.waiting = rest;
        for (_, tx) in done {
            let _ = tx.send(true);
        }
        self.quarantines_stored(version);
    }

    fn intent_receipt(&mut self, event: Event) -> Option<Receipt> {
        let id = match &event {
            Event::Intent { id, .. } => Some(id.clone()),
            _ => None,
        };
        self.apply(event);
        id.and_then(|id| self.state.receipt(&id).cloned())
    }

    /// The claim `viewer` holds, as its page is told about it (`Cmd::ClaimFor`).
    fn claim_msg(&self, viewer: &str) -> Option<ClaimMsg> {
        let info = self.state.fleet.slots.values().find_map(|s| {
            let c = s.claim.as_ref().filter(|c| c.viewer == viewer)?;
            let ready = s.observed.phase == "running" && s.observed.generation >= c.generation;
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
                autopilot: c.autopilot,
                ended: None,
            })
        });
        // A claim a restart carried over whose GPU's host is not back yet: restoring.
        let info = info.or_else(|| {
            let c = self
                .state
                .fleet
                .held_claims
                .iter()
                .map(|h| &h.claim)
                .find(|c| c.viewer == viewer)?;
            Some(ClaimMsg {
                id: c.session.clone(),
                tile: c.borrowed.clone(),
                phase: "restoring".into(),
                frame_url: None,
                frame_url_v4: None,
                token: c.token.clone(),
                clicked_at: c.claimed_at,
                first_frame_at: c.first_frame_at,
                expires_at: c.expires_at,
                gpu: None,
                phases: Default::default(),
                camera: c.camera.clone(),
                autopilot: c.autopilot,
                ended: None,
            })
        });
        info
    }

    pub fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Apply { event, reply } => {
                let receipt = self.intent_receipt(event);
                if let Some(reply) = reply {
                    self.reply_stored(reply, receipt);
                }
            }
            Cmd::Submit { event, reply } => {
                let claims = match &event {
                    Event::Intent { viewer, name, .. }
                        if name == "claim_gpu" || name == "resume_claim" =>
                    {
                        Some(viewer.clone())
                    }
                    _ => None,
                };
                let receipt = self.intent_receipt(event);
                let stored = self.receipt_stored(receipt.as_ref());
                let claim = claims
                    .filter(|_| {
                        receipt
                            .as_ref()
                            .is_some_and(|r| r.outcome == fleet_core::Outcome::Committed)
                    })
                    .and_then(|viewer| self.claim_msg(&viewer));
                let _ = reply.send(Submitted {
                    receipt,
                    stored,
                    claim,
                });
            }
            Cmd::Receipt { id, reply } => {
                let receipt = self.state.receipt(&id).cloned();
                self.reply_stored(reply, receipt);
            }
            Cmd::PrecheckWord {
                client,
                name,
                args,
                reply,
            } => {
                let at = self.stamp();
                let _ = reply.send(if name == "renders_overlay" {
                    fleet_core::precheck_overlay(&self.state, &client, &args, at)
                } else {
                    fleet_core::precheck_word(&self.state, &client, &args, at)
                });
            }
            Cmd::Sky { reply } => {
                let _ = reply.send(fleet_core::apply::sky(&self.state));
            }
            Cmd::AdmitKeys(keys) => {
                self.admit_keys = keys;
                self.dispatch_agents();
            }
            Cmd::MirrorKeys(keys) => {
                self.mirror_keys = keys;
                self.dispatch_agents();
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
                // The poster reads the aggregators' public streams: with admission keys, as
                // a holder of a direct token, sent as each stream's first message.
                let admit = self
                    .admit_keys
                    .first()
                    .and_then(|h| admission::AdmitKey::from_hex(h))
                    .map(|key| {
                        key.direct_token(
                            "poster",
                            crate::fleet::now_ms() / 1000 + admission::DIRECT_TOKEN_SECS,
                        )
                    });
                let plan = poster::Plan {
                    step: d.step,
                    arrive_t: d.arrive_t,
                    hold_until_t: d.hold_until(),
                    viewpoint: fleet_core::director::viewpoints()[d.current]
                        .name
                        .to_string(),
                    epoch_unix_ms: f.scene.epoch_unix_ms,
                    sources,
                    admit,
                };
                let _ = reply.send(Some(plan));
            }
            Cmd::ClaimFor { viewer, reply } => {
                let _ = reply.send(self.claim_msg(&viewer));
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
                tracing::info!(%host, conn, "agent hello");
                self.agents.insert(
                    conn,
                    AgentConn {
                        host: host.clone(),
                        tx,
                        last_assign: None,
                        last_scene: None,
                        last_budget: None,
                        last_potato: None,
                        last_renders: None,
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
                if let AgentMsg::Potato(e) = &msg {
                    // the agent's stamp beside this read's, so a late report is told from a lost one
                    tracing::debug!(target: "fcvm::fleet::potato_reports", host = %host, player = e.player, agent_at = e.at, coord_at = now_ms(), kind = ?e.kind, "potato event received");
                    let round = self.state.fleet.potato.round.as_ref().map(|r| r.id);
                    potato_log::log_event(&host, round, e);
                }
                if let AgentMsg::WindowsImage(w) = &msg {
                    let was = self
                        .state
                        .fleet
                        .hosts
                        .get(&host)
                        .and_then(|h| h.windows_image.as_ref());
                    let news = |k: fn(&fleet_core::windows::WindowsImage) -> &Option<String>| {
                        k(w).as_ref()
                            .filter(|v| was.and_then(|o| k(o).as_ref()) != Some(*v))
                    };
                    if let Some(why) = news(|w| &w.refused) {
                        tracing::error!(%host, image = %w.id, refused = %why, "the host's Windows image is refused");
                    } else if let Some(why) = news(|w| &w.warning) {
                        tracing::warn!(%host, image = %w.id, warning = %why, "the host's Windows image is old");
                    }
                    if let Some(e) = news(|w| &w.gold_error) {
                        tracing::error!(%host, image = %w.id, error = %e, "the host's gold snapshot failed");
                    }
                }
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
        // Writes whose answers acknowledge receipts and quarantines, oldest first: the state
        // version each holds, and its answer. One write per tick answers every receipt and
        // report applied before it; the writer answers in the order it was asked.
        let mut inflight: VecDeque<(u64, oneshot::Receiver<bool>)> = VecDeque::new();
        // The first write, at once: S3's PUT is conditional on the object this coordinator
        // read, so its being stored confirms this coordinator owns the state, which the EC2
        // driver waits for before it sweeps or launches anything (`Outputs::owned`).
        if self.out.store.is_some() {
            self.stored = self.state.version.wrapping_add(1);
            let version = self.state.version;
            inflight.push_back((version, self.store(true)));
        }
        let ack = loop {
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(Cmd::Shutdown { ack }) => break Some(ack),
                    Some(c) => self.handle(c),
                    None => break None,
                },
                _ = snap_tick.tick() => self.publish(),
                _ = store_tick.tick() => {
                    // Ownership not yet confirmed waits on a write as an answer does: a
                    // retry of the failed start write must be answered to confirm it.
                    let waiting = !self.unacked.is_empty()
                        || !self.waiting.is_empty()
                        || !*self.out.owned.borrow();
                    if self.state.version != self.stored || (waiting && inflight.is_empty()) {
                        // A changed state, or an unchanged one something waits on: the
                        // answer says whether the newest state is stored (also when it was
                        // written before, or a failed write of it is retried now).
                        let version = self.state.version;
                        inflight.push_back((version, self.store(true)));
                    } else {
                        drop(self.store(false));
                    }
                }
                (version, stored) = written(&mut inflight) => {
                    if stored {
                        self.stored_through(version);
                    }
                }
            }
        };
        self.publish();
        let mut stored = false;
        for attempt in 1..=SHUTDOWN_WRITE_ATTEMPTS {
            // A writer that is gone cannot store anything.
            let version = self.state.version;
            stored = self.store(true).await.unwrap_or(false);
            if stored {
                self.stored_through(version);
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
        // What no stored state holds is never acknowledged.
        for (_, tx) in self.waiting.drain(..) {
            let _ = tx.send(false);
        }
        if let Some(ack) = ack {
            let _ = ack.send(stored);
        }
    }
}

/// The answer to the oldest write in `w`, which leaves it (pending forever without one).
async fn written(w: &mut VecDeque<(u64, oneshot::Receiver<bool>)>) -> (u64, bool) {
    match w.front_mut() {
        Some((version, rx)) => {
            let version = *version;
            let stored = rx.await.unwrap_or(false);
            w.pop_front();
            (version, stored)
        }
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
        | Event::Lambda { at, .. }
        | Event::Degraded { at, .. }
        | Event::ReleaseQuarantine { at, .. }
        | Event::Visibility { at, .. }
        | Event::ScaleInstances { at, .. }
        | Event::Burn { at, .. }
        | Event::BudgetDrill { at, .. }
        | Event::HostsExpected { at, .. }
        | Event::Ec2Health { at, .. }
        | Event::LambdaHealth { at, .. }
        | Event::Published { at, .. } => *at = t,
    }
}

/// Everything the coordinator's tasks share.
#[derive(Clone)]
pub struct Handle {
    pub cmds: mpsc::Sender<Cmd>,
    pub snapshot: watch::Receiver<Arc<Snapshot>>,
    /// The Lambda tiles assigned (the spend meter charges them).
    pub lambda: watch::Receiver<Vec<AssignSlot>>,
    pub ec2_layout: watch::Receiver<Option<Layout>>,
    pub broadcast_layout: watch::Receiver<Option<Layout>>,
    /// The hosts' Minecraft status rows, from the agents' connections to the pages' (never
    /// through the actor: they are no part of the state).
    pub minecraft_status: minecraft_status::MinecraftStatuses,
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

    /// Apply an intent and return its receipt at once, with whether a stored state holds it
    /// (`Cmd::Submit`).
    pub async fn submit(&self, event: Event) -> Option<Submitted> {
        let (tx, rx) = oneshot::channel();
        self.cmds
            .send(Cmd::Submit { event, reply: tx })
            .await
            .ok()?;
        rx.await.ok()
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

    /// Have renderers and aggregators check direct tokens made with these keys.
    pub async fn set_admit_keys(&self, keys: Vec<String>) {
        let _ = self.cmds.send(Cmd::AdmitKeys(keys)).await;
    }

    /// Have phone streamers check GPU mirror tokens made with these keys.
    pub async fn set_mirror_keys(&self, keys: Vec<String>) {
        let _ = self.cmds.send(Cmd::MirrorKeys(keys)).await;
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

    pub async fn precheck_word(
        &self,
        client: &str,
        name: &str,
        args: &serde_json::Value,
    ) -> Option<String> {
        let (tx, rx) = oneshot::channel();
        self.cmds
            .send(Cmd::PrecheckWord {
                client: client.to_string(),
                name: name.to_string(),
                args: args.clone(),
                reply: tx,
            })
            .await
            .ok()?;
        rx.await.ok().flatten()
    }
}

/// Spot interruption notices, each host's sent once to every host launcher (one per host
/// group) on a channel that keeps every notice until the launcher reads it. A watch of the
/// draining hosts would lose a notice whose host left the state (its agent disconnected)
/// before the launcher looked. A launcher keeps only the notices of its own instances.
#[derive(Default)]
pub struct DrainNotices {
    tx: Vec<mpsc::UnboundedSender<String>>,
    /// The hosts draining now whose notice was sent.
    sent: std::collections::BTreeSet<String>,
}

impl DrainNotices {
    /// A launcher's channel: every notice published from now on.
    pub fn subscribe(&mut self) -> mpsc::UnboundedReceiver<String> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.tx.push(tx);
        rx
    }

    /// Send the notice of every host in `draining` not sent yet.
    fn publish(&mut self, draining: std::collections::BTreeSet<String>) {
        for id in draining.difference(&self.sent) {
            for tx in &self.tx {
                let _ = tx.send(id.clone());
            }
        }
        self.sent = draining;
    }
}

/// The launch names' subzone, served by this coordinator ([`authority`]): names are
/// published as they are minted, and the zone the server answers from is sent on `zone`.
pub struct Names {
    pub cfg: Arc<authority::ZoneConfig>,
    pub zone: watch::Sender<Arc<authority::Zone>>,
}

/// Start the actor, the tick timer and the EC2 driver; returns the handle the transports
/// use and the actor task.
pub fn start(
    state: State,
    store: Option<store::StoreTx>,
    ec2: Option<Arc<dyn ec2::Ec2Driver>>,
    ec2_aggregate_local: Option<String>,
    power: Option<hosts::HostConfig>,
    names: Option<Names>,
) -> (Handle, tokio::task::JoinHandle<()>) {
    start_with_lambda(state, store, ec2, None, ec2_aggregate_local, power, names)
}

/// [`start`], also running the Lambda tiles with `lambda` (the EC2 reconciler over the
/// Lambda driver, [`lambda::LambdaDriver`]).
pub fn start_with_lambda(
    state: State,
    store: Option<store::StoreTx>,
    ec2: Option<Arc<dyn ec2::Ec2Driver>>,
    lambda: Option<Arc<dyn ec2::Ec2Driver>>,
    ec2_aggregate_local: Option<String>,
    power: Option<hosts::HostConfig>,
    names: Option<Names>,
) -> (Handle, tokio::task::JoinHandle<()>) {
    start_with_groups(
        state,
        store,
        ec2,
        lambda,
        ec2_aggregate_local,
        power.into_iter().collect(),
        names,
    )
}

/// [`start_with_lambda`] with any number of host groups (`groups`): the GPU hosts and pages
/// mode's host, each launched and terminated by its own [`hosts::run`] over the same power
/// state.
pub fn start_with_groups(
    state: State,
    store: Option<store::StoreTx>,
    ec2: Option<Arc<dyn ec2::Ec2Driver>>,
    lambda: Option<Arc<dyn ec2::Ec2Driver>>,
    ec2_aggregate_local: Option<String>,
    groups: Vec<hosts::HostConfig>,
    names: Option<Names>,
) -> (Handle, tokio::task::JoinHandle<()>) {
    let (cmds, rx) = mpsc::channel(4096);
    let (snap_tx, snap_rx) = watch::channel(Arc::new(Snapshot {
        version: 0,
        text: String::new(),
    }));
    let (ec2_tx, ec2_rx) = watch::channel(Vec::new());
    let (lambda_tx, lambda_rx) = watch::channel(Vec::new());
    let (ec2_layout_tx, ec2_layout_rx) = watch::channel(None);
    let (broadcast_layout_tx, broadcast_layout_rx) = watch::channel(None);
    let (power_tx, power_rx) = watch::channel(state.fleet.power.state.clone());
    let mut draining_tx = DrainNotices::default();
    let draining_rx: Vec<_> = groups.iter().map(|_| draining_tx.subscribe()).collect();
    let (scene_tx, scene_rx) = watch::channel(None);
    let (budget_tx, budget_rx) = watch::channel(state.fleet.budget.body());
    let (owned_tx, owned_rx) = watch::channel(store.is_none());
    let mut state = state;
    // With named launches, tiles' frame URLs are the launches' published names.
    state.config.names_published = names.is_some();
    let actor = Actor::new(
        state,
        Outputs {
            snapshot: snap_tx,
            ec2: ec2_tx,
            lambda: lambda_tx,
            ec2_layout: ec2_layout_tx,
            broadcast_layout: broadcast_layout_tx,
            power: power_tx,
            draining: draining_tx,
            scene: scene_tx,
            budget: budget_tx,
            zone: names,
            store,
            owned: owned_tx,
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
        lambda: lambda_rx.clone(),
        ec2_layout: ec2_layout_rx,
        broadcast_layout: broadcast_layout_rx,
        minecraft_status: Default::default(),
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
    if let Some(driver) = lambda {
        let mut owned = owned_rx.clone();
        let handle = handle.clone();
        let (scene_rx, budget_rx) = (scene_rx.clone(), budget_rx.clone());
        tokio::spawn(async move {
            // As the EC2 tiles below: nothing is swept or launched before this
            // coordinator owns the state object.
            if wait_owned(&mut owned).await {
                ec2::run(driver, lambda_rx, scene_rx, budget_rx, handle).await;
            }
        });
    }
    if let Some(driver) = ec2 {
        let mut owned = owned_rx;
        let handle = handle.clone();
        tokio::spawn(async move {
            // No sweep and no launch before this coordinator's first write is stored: one
            // that lost the state object to another coordinator would terminate that one's
            // tiles. Once owned, ec2::run sweeps and retries as before.
            if wait_owned(&mut owned).await {
                ec2::run(driver, ec2_rx, scene_rx, budget_rx, handle).await;
            }
        });
    }
    for (cfg, draining) in groups.into_iter().zip(draining_rx) {
        tokio::spawn(hosts::run(cfg, power_rx.clone(), draining, handle.clone()));
    }
    (handle, task)
}

/// Wait until a stored write confirms this coordinator owns the state object; false when
/// the actor stopped first.
async fn wait_owned(owned: &mut watch::Receiver<bool>) -> bool {
    owned.wait_for(|o| *o).await.is_ok()
}

/// Whether another writer has taken the state object (`store::LostWrite`) by now.
fn store_lost(lost: &Option<watch::Receiver<bool>>) -> bool {
    lost.as_ref().is_some_and(|l| *l.borrow())
}

/// Whether this coordinator still owns the state object at shutdown: its last write was
/// stored, and no write found another writer's object.
fn owns_the_state(stored: bool, lost: &Option<watch::Receiver<bool>>) -> bool {
    stored && !store_lost(lost)
}

/// Resolves once `lost` turns true (pending forever without one, or once its writer is gone).
async fn lost_the_store(lost: &mut Option<watch::Receiver<bool>>) {
    let Some(rx) = lost else {
        return std::future::pending().await;
    };
    if rx.wait_for(|l| *l).await.is_err() {
        std::future::pending::<()>().await;
    }
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

/// Whether the fleet sleeps with no active viewer: it launches its GPU hosts or pages mode's
/// host, or it was told to sleep on fixed hosts.
pub fn power_managed(args: &FleetCoordArgs) -> bool {
    !args.host_templates.is_empty() || !args.renders_templates.is_empty() || args.sleep_when_idle
}

/// The most on-demand hosts pages mode's group runs: its one host, when spot is refused.
pub const RENDERS_ON_DEMAND_MAX: u32 = 1;

/// The host groups the coordinator launches: the GPU hosts with `--host-template`, and pages
/// mode's host with `--renders-template` (one host while awake, in the templates' one
/// region). Each lists and launches only its own class's instances.
pub fn host_groups(args: &FleetCoordArgs) -> Result<Vec<hosts::HostConfig>> {
    let mut groups = Vec::new();
    if args.host_templates.is_empty() {
        anyhow::ensure!(
            args.host_subnets.is_empty(),
            "--host-subnet needs a --host-template"
        );
    } else {
        let driver = hosts::AwsHosts::new(
            &args.host_templates,
            &args.host_subnets,
            args.fleet_name.clone(),
        )?;
        groups.push(hosts::HostConfig {
            class: hosts::HostClass::Gpu,
            regions: driver.regions(),
            driver: Arc::new(driver),
            awake_hosts: args.awake_hosts,
            spread: args.host_spread,
            on_demand_max: args.host_on_demand_max,
            // While hosts come up or go away: one DescribeInstances per region.
            poll: Duration::from_secs(2),
            // While they run: a reclaimed spot host is replaced within this.
            liveness: Duration::from_secs(15),
            capacity_retry: Duration::from_secs(10),
            lost_launch_ttl: Duration::from_secs(120),
        });
    }
    if args.renders_templates.is_empty() {
        anyhow::ensure!(
            args.renders_subnets.is_empty(),
            "--renders-subnet needs a --renders-template"
        );
    } else {
        let driver = hosts::AwsHosts::for_class(
            &args.renders_templates,
            &args.renders_subnets,
            args.fleet_name.clone(),
            hosts::HostClass::Renders,
        )?;
        let regions = driver.regions();
        anyhow::ensure!(
            regions.len() == 1,
            "--renders-template names {}: pages mode runs one host, in one region",
            regions.join(", ")
        );
        groups.push(hosts::HostConfig {
            class: hosts::HostClass::Renders,
            regions,
            driver: Arc::new(driver),
            awake_hosts: 1,
            spread: hosts::HostSpread::None,
            on_demand_max: RENDERS_ON_DEMAND_MAX,
            poll: Duration::from_secs(2),
            liveness: Duration::from_secs(15),
            capacity_retry: Duration::from_secs(10),
            lost_launch_ttl: Duration::from_secs(120),
        });
    }
    Ok(groups)
}

/// The addresses `--admin-listen` binds: every address it names must be loopback. The endpoint
/// has no authentication (`POST /quarantine/release` returns a GPU to rotation), so on any
/// other address, a private network included, anyone who reaches it could undo a quarantine.
/// A name is resolved, and refused when any of its addresses is not loopback.
pub async fn admin_addresses(addr: &str) -> Result<Vec<SocketAddr>> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(addr)
        .await
        .with_context(|| format!("--admin-listen {addr} is not an address and port"))?
        .collect();
    if addrs.is_empty() {
        bail!("--admin-listen {addr} names no address");
    }
    if let Some(a) = addrs.iter().find(|a| !a.ip().is_loopback()) {
        bail!(
            "--admin-listen {addr} reaches {a}, which is not loopback: the admin endpoint has \
             no authentication, so it binds only 127.0.0.0/8 or ::1"
        );
    }
    Ok(addrs)
}

/// `--lambda-tiles` as the state's `lambda_max`, checked against the flags it needs.
pub fn lambda_tiles(args: &FleetCoordArgs) -> Result<usize> {
    match args.lambda {
        FleetLambda::None => {
            anyhow::ensure!(args.lambda_tiles == 0, "--lambda-tiles needs --lambda aws");
            Ok(0)
        }
        FleetLambda::Aws => {
            anyhow::ensure!(
                args.ec2 != FleetEc2::None,
                "--lambda aws takes its tiles out of the AWS half's --ec2-tiles: it needs --ec2"
            );
            anyhow::ensure!(
                args.lambda_tiles <= args.ec2_tiles,
                "--lambda-tiles {} is more than the AWS half's --ec2-tiles {}",
                args.lambda_tiles,
                args.ec2_tiles
            );
            anyhow::ensure!(
                args.ec2_aggregate_tls,
                "--lambda aws serves its tiles on the coordinator task's port 443: it needs --ec2-aggregate-tls"
            );
            Ok(args.lambda_tiles.min(args.max_tiles))
        }
    }
}

pub async fn run(args: FleetCoordArgs) -> Result<()> {
    // Before anything starts: a refused admin address stops the coordinator at once.
    let admin_addrs = match &args.admin_listen {
        Some(a) => Some(admin_addresses(a).await?),
        None => None,
    };
    // The host groups, checked before anything starts.
    let groups = host_groups(&args)?;
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
    let lambda_tiles = lambda_tiles(&args)?;
    let config = Config {
        lambda_max: lambda_tiles,
        max_tiles: args.max_tiles,
        fcvm_tiles: args.fcvm_tiles.min(args.max_tiles),
        fill_from_serving_hosts: args.awake_hosts > 1,
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
        sleep_without_new_viewer_secs: args.sleep_without_new_viewer,
        min_wake_remaining_secs: args.min_wake_remaining,
        daily_spend_usd: args.daily_spend_usd,
        warm_hours: args.warm_hours.clone(),
        history_idle_secs: args.history_idle,
        ..Config::default()
    };
    if config.max_tiles == 0 {
        bail!("--max-tiles must be at least 1");
    }
    if !config.warm_hours.is_empty() && !config.power_managed {
        bail!("--warm-hours needs a fleet that sleeps (--host-template or --sleep-when-idle)");
    }
    // What the certificate covers, where its DNS-01 records go, and the tiles' health zone.
    let tls = cert::plan(&args)?;
    let meter = spend::meter(&args)?;
    let checker: Arc<dyn text_check::TextChecker> = match args.text_check {
        FleetTextCheck::None => Arc::new(text_check::NoCheck),
        FleetTextCheck::Bedrock => {
            let model = args
                .text_check_model
                .context("--text-check bedrock needs --text-check-model")?;
            let region = args
                .text_check_region
                .context("--text-check bedrock needs --text-check-region")?;
            Arc::new(text_check::Guarded::new(Arc::new(
                text_check::Bedrock::new(region, model)?,
            )))
        }
    };
    // Turns true once another coordinator wrote the state object (`store::LostWrite`).
    let mut lost: Option<watch::Receiver<bool>> = None;
    let mut budget_drill = None;
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
                if meter.is_some() && args.fleet_name == "prod" {
                    budget_drill =
                        Some(store::S3Object::new(bucket, spend::DRILL_DAY_KEY, &region)?);
                }
                let obj = store::S3Object::new(bucket, &args.state_key, &region)?;
                let state = match obj.load().await? {
                    Some(s) => {
                        tracing::info!(bucket = %bucket, key = %args.state_key, version = s.version, "resuming from the state object");
                        s.resumed(config, now_ms())
                    }
                    None => State::new(config, now_ms()),
                };
                let (tx, lost_rx) = store::spawn_s3_writer(obj);
                lost = Some(lost_rx);
                (state, Some(tx))
            }
            None => (State::new(config, now_ms()), None),
        },
    };
    // The renderers' scene token, from the certificate secret (fleet/PROTOCOL.md, "Access").
    let scene_token = match &args.tls_secret {
        Some(secret) => {
            let secrets = secrets::Secrets::new(&secrets_region()?)?;
            let t = scene_token::ensure(&secrets, secret).await?;
            let held = scene_token::SceneToken::new(Some(t));
            tokio::spawn(scene_token::keep(secrets, secret.clone(), held.clone()));
            held
        }
        None => scene_token::SceneToken::default(),
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
                tls.tile_zone.clone(),
            )?;
            d.fleet = args.fleet_name.clone();
            d.pool_max = args.ec2_pool;
            d.pool_template = args.ec2_pool_template.clone();
            d.networks = ec2_aws::networks(&args.ec2_subnets, &args.ec2_security_group)?;
            d.scene_token = scene_token.clone();
            Some(Arc::new(d))
        }
    };
    // The Lambda tiles' tunnel endpoint and driver (`--lambda aws`).
    let lambda_endpoint = match args.lambda {
        FleetLambda::None => None,
        FleetLambda::Aws => {
            let token = scene_token.get().context(
                "--lambda aws keys the tiles' tunnel credentials with the scene token: it needs --tls-secret",
            )?;
            Some((
                tunnel::Endpoint::new(tunnel::derive_key(&token)),
                tunnel::derive_key(&token),
            ))
        }
    };
    let lambda: Option<Arc<dyn ec2::Ec2Driver>> = match &lambda_endpoint {
        None => None,
        Some((endpoint, key)) => {
            let zone = args
                .dns_subzone
                .clone()
                .context("--lambda aws names its tiles in --dns-subzone")?
                .trim_end_matches('.')
                .to_ascii_lowercase();
            let public = ec2_public
                .as_ref()
                .context("--lambda aws serves its tiles on the task's addresses: it needs --ec2-aggregate-tls")?;
            let plan = lambda::Plan {
                fleet: args.fleet_name.clone(),
                image: lambda::lane_image(
                    args.lambda_image
                        .as_deref()
                        .context("--lambda aws needs --lambda-image")?,
                )?,
                role: args
                    .lambda_role
                    .clone()
                    .context("--lambda aws needs --lambda-role")?,
                capacity_provider: args
                    .lambda_capacity_provider
                    .clone()
                    .context("--lambda aws needs --lambda-capacity-provider")?,
                tunnel: format!("{}.{zone}", lambda_front::TUNNEL_LABEL),
                zone,
                tls_secret: args
                    .tls_secret
                    .clone()
                    .context("--lambda aws needs --tls-secret")?,
                tls_secret_region: secrets_region()?,
                ipv6: public.ipv6.clone(),
                ipv4: public.ipv4.clone(),
            };
            Some(Arc::new(lambda::LambdaDriver::new(
                plan,
                Arc::new(lambda::LambdaApi::new(&args.lambda_region)?),
                endpoint.clone(),
                key.clone(),
                scene_token.clone(),
            )))
        }
    };
    let ec2_local = args.ec2_aggregate_listen.as_ref().map(|l| {
        format!(
            "ws://{}/frames",
            crate::fleet::aggregate::child::connect_addr(l)
        )
    });
    // With host templates the coordinator launches its GPU hosts, so a dialed agent must be on
    // one of them.
    let agent_identity: Option<Arc<dyn agents::AgentIdentity>> = (!groups.is_empty()).then(|| {
        Arc::new(agents::FleetHosts {
            groups: groups
                .iter()
                .map(|p| agents::FleetGroup {
                    class: p.class,
                    driver: p.driver.clone(),
                    regions: p.regions.clone(),
                })
                .collect(),
            fleet: args.fleet_name.clone(),
        }) as Arc<dyn agents::AgentIdentity>
    });
    let access = match &args.access_team_domain {
        Some(team) => {
            anyhow::ensure!(
                !args.access_aud.is_empty() || !args.agent_access_aud.is_empty(),
                "--access-team-domain needs --access-aud or --agent-access-aud"
            );
            Some(Arc::new(access_jwt::AccessCheck {
                jwt: access_jwt::AccessJwt::new(team)?,
                live: args.access_aud.clone(),
                agent: args.agent_access_aud.clone(),
            }))
        }
        None => None,
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
            let cfg = Arc::new(authority::ZoneConfig::new(
                sub,
                &args.dns_subzone_ns,
                tls.acme.clone(),
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
            Some(Names { cfg, zone: tx })
        }
        None => None,
    };
    if let Some(mut renewal) = tls.renewal {
        // A subzone certificate's records are answered by the subzone's server; a zone
        // certificate's are written in the zone with its Cloudflare token.
        let dns: Box<dyn cert::Dns01> = match tls.challenge {
            Some(txt) => Box::new(txt),
            None => {
                Box::new(cloudflare(dns_token.as_deref().context(
                    "--tls-secret needs --dns-token-secret (DNS-01 challenges)",
                )?)?)
            }
        };
        if let Some(d) = &args.acme_directory {
            renewal.directory = d.clone();
        }
        renewal.scene_token = scene_token.get();
        tokio::spawn(cert::run(
            renewal,
            secrets::Secrets::new(&secrets_region()?)?,
            dns,
        ));
    }
    // Both backings' tiles are swept at shutdown: the EC2 instances and the Lambda functions.
    let sweeps: Vec<(&str, Arc<dyn ec2::Ec2Driver>)> = ec2
        .clone()
        .map(|d| ("EC2", d))
        .into_iter()
        .chain(lambda.clone().map(|d| ("Lambda", d)))
        .collect();
    let (handle, _actor) =
        start_with_groups(state, store_tx, ec2, lambda, ec2_local, groups, names);
    if let Some(obj) = budget_drill {
        tokio::spawn(spend::watch_drill(obj, handle.clone()));
    }
    if let Some(m) = meter {
        tokio::spawn(spend::run(m, handle.clone()));
    }
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
            let source =
                tls::Source::Secret(secrets::Secrets::new(&secrets_region()?)?, secret.clone());
            tls::refresh(&source, &dir, &scene_token::SceneToken::default()).await?;
            tokio::spawn(tls::run(source, dir.clone(), Default::default()));
            argv.extend([
                "--tls-file".into(),
                dir.join(tls::FILE).to_string_lossy().into_owned(),
            ]);
            let mut public: Vec<std::net::SocketAddr> = Vec::new();
            if a.ipv4.is_some() {
                public.push(format!("0.0.0.0:{}", tls::PORT).parse()?);
            }
            if let Some(v6) = &a.ipv6 {
                public.push(format!("[{v6}]:{}", tls::PORT).parse()?);
            }
            match &lambda_endpoint {
                // Port 443 is the front's, which routes the aggregator's names to it on
                // loopback (`lambda_front`).
                Some((endpoint, _)) => {
                    let behind: std::net::SocketAddr =
                        format!("127.0.0.1:{}", lambda_front::AGGREGATOR_BEHIND).parse()?;
                    argv.extend(["--tls-listen".into(), behind.to_string()]);
                    let cert = crate::fleet::aggregate::tls::CertFile::load(&dir.join(tls::FILE))?;
                    tokio::spawn(cert.clone().watch(crate::fleet::aggregate::tls::RELOAD));
                    let zone = args
                        .dns_subzone
                        .clone()
                        .unwrap_or_default()
                        .trim_end_matches('.')
                        .to_ascii_lowercase();
                    let front =
                        lambda_front::Front::new(zone, endpoint.clone(), cert, Some(behind));
                    for addr in public {
                        let l = tokio::net::TcpListener::bind(addr)
                            .await
                            .with_context(|| format!("the Lambda front cannot listen on {addr}"))?;
                        tracing::info!(%addr, "the Lambda front listens");
                        tokio::spawn(front.clone().serve(l));
                    }
                }
                None => {
                    for addr in public {
                        argv.extend(["--tls-listen".into(), addr.to_string()]);
                    }
                }
            }
        }
        tokio::spawn(crate::fleet::aggregate::child::supervise(
            argv,
            listen.clone(),
            handle.ec2_layout.clone(),
            || {},
        ));
    }
    let mut gate = admission::Gate::open();
    let mut broadcast_probe: Option<Arc<dyn metrics::BroadcastSource>> = None;
    if let Some(listen) = &args.broadcast_listen {
        let json = match (&args.broadcast_secret, &args.broadcast_file) {
            (Some(secret), _) => secrets::Secrets::new(&secrets_region()?)?
                .get(secret)
                .await
                .with_context(|| format!("reading the broadcast secret {secret}"))?,
            (None, Some(path)) => std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?,
            (None, None) => {
                bail!("--broadcast-listen needs --broadcast-secret or --broadcast-file")
            }
        };
        let b = broadcast::BroadcastSecret::parse(&json)?;
        gate.playback = b.playback()?;
        match &b.channel_arn {
            Some(arn) => broadcast_probe = Some(Arc::new(metrics::IvsStream::new(arn)?)),
            None => {
                tracing::warn!("the broadcast secret names no channel_arn: no BroadcastLive metric")
            }
        }
        admission::check_direct_capacity(args.direct_capacity)?;
        gate.seats = admission::Seats::new(Some(args.direct_capacity));
        let mut argv = vec![
            std::env::current_exe()
                .context("locating this fcvm binary")?
                .to_string_lossy()
                .into_owned(),
            "fleet".into(),
            "aggregate".into(),
        ];
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
        let defaults = broadcast::default_args(cpus);
        tracing::info!(cpus, defaults = ?defaults, "broadcast aggregator settings");
        argv.extend(defaults.iter().map(|s| s.to_string()));
        argv.extend(args.broadcast_args.iter().cloned());
        argv.extend(["--push-url-env".into(), broadcast::PUSH_URL_ENV.into()]);
        tokio::spawn(crate::fleet::aggregate::child::supervise_with(
            argv,
            listen.clone(),
            handle.broadcast_layout.clone(),
            || {},
            crate::fleet::aggregate::child::Supervision {
                env: vec![(broadcast::PUSH_URL_ENV.into(), b.ingest_url)],
                only_with_layout: true,
            },
        ));
    }
    let cert_probe: Option<Arc<dyn metrics::CertSource>> = match &args.tls_secret {
        Some(id) => Some(Arc::new(metrics::SecretCert {
            secrets: secrets::Secrets::new(&secrets_region()?)?,
            id: id.clone(),
        })),
        None => None,
    };
    tokio::spawn(metrics::run(
        metrics::MetricsConfig {
            fleet: args.fleet_name.clone(),
            every: metrics::EVERY,
            cert: cert_probe,
            broadcast: broadcast_probe,
        },
        handle.snapshot.clone(),
        |line| println!("{line}"),
    ));
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
    if let Some(addrs) = &admin_addrs {
        // The checked addresses, not the name again: a second lookup could answer otherwise.
        let admin = tokio::net::TcpListener::bind(&addrs[..])
            .await
            .with_context(|| format!("listening on {addrs:?}"))?;
        eprintln!("fleet coordinator admin on {}", admin.local_addr()?);
        let h = handle.clone();
        tokio::spawn(async move {
            if let Err(e) = axum::serve(admin, admin_router(h)).await {
                tracing::error!(error = %e, "admin endpoint stopped");
            }
        });
    }
    let (turnstile_secret, turnstile_site_key) = match &args.turnstile_secret_id {
        Some(id) => {
            let json = secrets::Secrets::new(&secrets_region()?)?
                .get(id)
                .await
                .with_context(|| format!("reading the Turnstile secret {id}"))?;
            let (site, secret) = turnstile::parse_secret(&json)
                .with_context(|| format!("reading the Turnstile secret {id}"))?;
            (Some(secret), Some(site))
        }
        None => (args.turnstile_secret, args.turnstile_site_key),
    };
    let verifier: Arc<dyn turnstile::Verifier> = match turnstile_secret {
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
    // Direct streams are for viewers the coordinator admitted: renderers and aggregators check
    // the direct token made with this key, with or without a broadcast. The GPU mirror is
    // checked too: only a phone's holder receives it.
    let (admit, mirror) = admission_keys(&gate);
    handle.set_admit_keys(admit).await;
    handle.set_mirror_keys(mirror).await;
    let cfg = live::LiveConfig {
        checker,
        allowed_origins: args.allowed_origins,
        verifier,
        proof: match &access {
            Some(a) => a.clone() as Arc<dyn turnstile::RequestProof>,
            None => Arc::new(turnstile::NoProof),
        },
        trust_cf_connecting_ip: args.trust_cf_connecting_ip,
        access_viewer_address: args.access_viewer_address,
        turnstile_site_key,
        poster,
        gate: Arc::new(gate),
        sockets: live::SocketCaps::default(),
        access,
        agent_identity,
        // Baked into the coordinator's image (fleet/coord/Containerfile) from the commit it was built from.
        commit: std::env::var("FLEET_COMMIT").ok().filter(|c| !c.is_empty()),
    };
    tokio::select! {
        r = live::serve(listener, handle.clone(), cfg) => r?,
        _ = term.recv() => tracing::info!("SIGTERM"),
        _ = int.recv() => tracing::info!("SIGINT"),
        _ = lost_the_store(&mut lost) => {
            // Another coordinator holds the state now. Writing ours over it would undo what
            // it acknowledged, and the EC2 sweep below would terminate its tiles: stop here.
            tracing::error!(
                marker = store::LOST_WRITE_MARKER,
                "another coordinator writes the state object; stopping without writing or sweeping"
            );
            bail!("another coordinator writes the state object ({})", store::LOST_WRITE_MARKER);
        }
    }
    // The state file holds the last state before the process exits; the agents' commands
    // are killed when their handles drop, and their VMs follow them (PR_SET_PDEATHSIG).
    let stored = handle.shutdown().await;
    if stored {
        tracing::info!("coordinator stopped; state written");
    }
    // Another coordinator may have taken the state object while this one stopped (its
    // shutdown write answered 412): the EC2 sweep below would terminate that one's tiles.
    if store_lost(&lost) {
        tracing::error!(
            marker = store::LOST_WRITE_MARKER,
            "another coordinator writes the state object; stopping without sweeping"
        );
        bail!(
            "another coordinator writes the state object ({})",
            store::LOST_WRITE_MARKER
        );
    }
    // The next coordinator does not know these instances; terminate them now (it also
    // sweeps at its start, for a coordinator that was killed). Only while this coordinator
    // still owns the state object, which its stored last write confirms: a last write that
    // failed may have met another coordinator's object (a 412 whose check could not read
    // it), and that coordinator's tiles are the ones the sweep would terminate.
    let owns = owns_the_state(stored, &lost);
    if !owns && !sweeps.is_empty() {
        tracing::warn!(
            "the last state was not stored, so this coordinator may not own the state object; the tile sweep is left to the next coordinator's start"
        );
    }
    let sweeps = if owns { sweeps } else { Vec::new() };
    // Waited for before the process exits: a function or an instance left behind is billed
    // and nothing manages it until another coordinator starts. A Lambda sweep also refuses
    // every launch a tile task might still make.
    for (backing, d) in sweeps {
        match tokio::time::timeout(Duration::from_secs(20), d.sweep_at_shutdown()).await {
            Ok(Ok(n)) => tracing::info!(backing, tiles = n, "tiles removed"),
            Ok(Err(e)) => tracing::warn!(backing, error = %format!("{e:#}"), "tiles not removed"),
            Err(_) => tracing::warn!(backing, "tiles not removed within 20 s"),
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
    fn the_fleet_sleeps_with_host_templates_or_when_told_to() {
        assert!(!super::power_managed(&coord(&[])));
        assert!(super::power_managed(&coord(&["--sleep-when-idle"])));
        assert!(super::power_managed(&coord(&[
            "--host-template",
            "us-east-1:fcvm-fleet-host-x86_64",
            "--host-subnet",
            "us-east-1:subnet-0123456789abcdef0"
        ])));
    }

    /// The two limits of the arrival rule come from their flags or from the environment (the
    /// coordinator's task definition sets variables, not arguments). Which variable each
    /// reads, and its default, are read from the argument definitions: a test here does not
    /// set a variable in its own process (`crate::test_env`).
    #[test]
    fn the_arrival_limits_name_their_environment_variables_and_defaults() {
        let cli = <Cli as clap::CommandFactory>::command();
        let coord_cmd = cli
            .find_subcommand("fleet")
            .and_then(|f| f.find_subcommand("coord"))
            .expect("the fleet coord subcommand");
        for (id, long, env, default) in [
            (
                "sleep_without_new_viewer",
                "sleep-without-new-viewer",
                "FLEET_SLEEP_WITHOUT_NEW_VIEWER",
                "3600",
            ),
            (
                "min_wake_remaining",
                "min-wake-remaining",
                "FLEET_MIN_WAKE_REMAINING",
                "600",
            ),
        ] {
            let arg = coord_cmd
                .get_arguments()
                .find(|a| a.get_id() == id)
                .unwrap_or_else(|| panic!("no argument {id}"));
            assert_eq!(arg.get_long(), Some(long));
            assert_eq!(arg.get_env().and_then(|e| e.to_str()), Some(env), "{id}");
            let defaults: Vec<_> = arg
                .get_default_values()
                .iter()
                .map(|v| v.to_str())
                .collect();
            assert_eq!(defaults, [Some(default)], "{id}");
        }
        // The flags reach the fields the coordinator's config is built from.
        let a = coord(&[
            "--sleep-without-new-viewer",
            "7200",
            "--min-wake-remaining",
            "0",
        ]);
        assert_eq!(
            (a.sleep_without_new_viewer, a.min_wake_remaining),
            (7200, 0)
        );
    }

    /// A race launches every EC2 tile, so more EC2 tiles than the viewers' launch budget would
    /// leave `restart_now` refused for ever: such a count is refused when the coordinator starts.
    #[test]
    fn the_ec2_tile_count_fits_the_launch_budget() {
        let budget = fleet_core::apply::EC2_LAUNCH_BUDGET;
        assert_eq!(coord(&[]).ec2_tiles, 8);
        assert_eq!(
            coord(&["--ec2-tiles", &budget.to_string()]).ec2_tiles,
            budget
        );
        let over = (budget + 1).to_string();
        let argv = [
            "fcvm",
            "fleet",
            "coord",
            "--listen",
            "127.0.0.1:0",
            "--ec2-tiles",
            &over,
        ];
        let e = match Cli::try_parse_from(argv) {
            Ok(_) => panic!("--ec2-tiles {over} was accepted"),
            Err(e) => e.to_string(),
        };
        assert!(e.contains("no race could ever start"), "{e}");
    }

    #[test]
    fn warm_hours_parse_as_weekly_utc_windows() {
        let a = coord(&["--warm-hours", "Sat 17:00-23:00,Sun 17:00-21:00"]);
        let shown: Vec<String> = a.warm_hours.iter().map(|w| w.to_string()).collect();
        assert_eq!(shown, ["Sat 17:00-23:00", "Sun 17:00-21:00"]);
        assert!(coord(&[]).warm_hours.is_empty());
        let argv = [
            "fcvm",
            "fleet",
            "coord",
            "--listen",
            "127.0.0.1:0",
            "--warm-hours",
            "Saturday 17:00-23:00",
        ];
        assert!(Cli::try_parse_from(argv).is_err());
    }

    /// A fleet that never sleeps has nothing to keep warm: the flag is refused at start.
    #[tokio::test]
    async fn warm_hours_need_a_fleet_that_sleeps() {
        let e = super::run(coord(&["--warm-hours", "Sat 17:00-23:00"]))
            .await
            .expect_err("--warm-hours without power management was accepted");
        assert!(e.to_string().contains("--warm-hours needs"), "{e}");
    }

    /// With pages mode's templates the fleet sleeps (its host must not run with no viewer),
    /// and the coordinator builds a second host group from them: one host while awake, at
    /// most one on demand, in the one region its templates name. Without them nothing
    /// changes; templates in two regions, or a subnet without a template, are refused.
    #[test]
    fn the_renders_templates_make_a_second_host_group() {
        let renders = [
            "--renders-template",
            "us-east-2:fcvm-fleet-renders-arm64",
            "--renders-subnet",
            "us-east-2:subnet-0123456789abcdef1",
        ];
        assert!(super::power_managed(&coord(&renders)));
        let gpu = [
            "--host-template",
            "us-east-1:fcvm-fleet-host-x86_64",
            "--host-subnet",
            "us-east-1:subnet-0123456789abcdef0",
        ];
        let groups = super::host_groups(&coord(&gpu)).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].class, super::hosts::HostClass::Gpu);
        let mut both = gpu.to_vec();
        both.extend(renders);
        let groups = super::host_groups(&coord(&both)).unwrap();
        assert_eq!(groups.len(), 2);
        let r = &groups[1];
        assert_eq!(r.class, super::hosts::HostClass::Renders);
        assert_eq!(r.regions, ["us-east-2"]);
        assert_eq!((r.awake_hosts, r.on_demand_max), (1, 1));
        assert_eq!(r.spread, super::hosts::HostSpread::None);
        assert!(super::host_groups(&coord(&[])).unwrap().is_empty());
        let two_regions = [
            "--renders-template",
            "us-east-2:a,us-west-2:b",
            "--renders-subnet",
            "us-east-2:subnet-1,us-west-2:subnet-2",
        ];
        let e = super::host_groups(&coord(&two_regions))
            .err()
            .expect("two regions refused")
            .to_string();
        assert!(e.contains("one region"), "{e}");
        assert!(super::host_groups(&coord(&["--renders-subnet", "us-east-2:subnet-1"])).is_err());
    }

    #[test]
    fn the_on_demand_host_cap_is_at_most_eight() {
        assert_eq!(coord(&[]).host_on_demand_max, 8);
        assert_eq!(coord(&["--host-on-demand-max", "2"]).host_on_demand_max, 2);
        let argv = [
            "fcvm",
            "fleet",
            "coord",
            "--listen",
            "127.0.0.1:0",
            "--host-on-demand-max",
            "9",
        ];
        assert!(Cli::try_parse_from(argv).is_err());
    }
}

#[cfg(test)]
mod store_tests {
    use super::*;

    /// A coordinator that lost the state object to another writer while stopping does not
    /// sweep the EC2 tiles, which are the other coordinator's now; nor does one whose last
    /// write was not stored, since that write may have met the other coordinator's object
    /// without the check reading it (a 412 followed by a failed GET).
    #[test]
    fn the_ec2_sweep_runs_only_while_this_coordinator_owns_the_state() {
        let src = include_str!("mod.rs");
        let body = src
            .split("pub async fn run(args: FleetCoordArgs)")
            .nth(1)
            .expect("run");
        let shutdown = body.find("handle.shutdown().await").expect("the shutdown");
        let lost_check = body.find("if store_lost(&lost)").expect("the lost check");
        let guard = body
            .find("owns_the_state(stored, &lost)")
            .expect("the ownership check");
        let sweep = body.find("d.sweep_at_shutdown()").expect("the sweep");
        assert!(
            shutdown < lost_check && shutdown < guard && guard < sweep,
            "the sweep is not guarded by the ownership check after the shutdown write"
        );
        // The Lambda tiles' functions are swept too, by the same guarded sweep.
        let sweeps = body.find("let sweeps").expect("the drivers swept");
        assert!(
            body[sweeps..shutdown].contains("lambda.clone()"),
            "the Lambda driver is not among the drivers swept at shutdown"
        );
        let (tx, rx) = watch::channel(false);
        let lost = Some(rx);
        assert!(owns_the_state(true, &lost), "stored and not lost");
        assert!(!owns_the_state(false, &lost), "the last write not stored");
        assert!(owns_the_state(true, &None), "no S3 store: a stored write");
        tx.send(true).unwrap();
        assert!(store_lost(&lost));
        assert!(!owns_the_state(true, &lost), "another writer took it");
    }

    /// A committed claim comes back with its receipt, read in the turn that applied it: a
    /// second actor command (`ClaimFor`) could find it already ended, and leave the page with
    /// a receipt and no claim to be told ended.
    #[tokio::test]
    async fn a_submitted_claim_comes_back_with_its_receipt() {
        let out = Outputs {
            snapshot: watch::channel(snapshot_of(&State::new(Config::default(), 0))).0,
            ec2: watch::channel(Vec::new()).0,
            lambda: watch::channel(Vec::new()).0,
            ec2_layout: watch::channel(None).0,
            broadcast_layout: watch::channel(None).0,
            power: watch::channel(String::new()).0,
            draining: Default::default(),
            scene: watch::channel(None).0,
            budget: watch::channel(None).0,
            zone: None,
            store: None,
            owned: watch::channel(true).0,
        };
        let cfg = Config {
            max_tiles: 1,
            ..Config::default()
        };
        let mut actor = Actor::new(State::new(cfg, 0), out, Box::new(|| 1_000));
        let hello: Event = serde_json::from_value(serde_json::json!({"at": 0, "type": "agent",
            "host": "h1", "msg": {"type": "hello", "host": "h1", "arch": "x86_64",
            "capacity": 2, "gpus": [{"name": "Tesla T4", "sbdf": "0000:01:00.0"},
                                    {"name": "Tesla T4", "sbdf": "0000:02:00.0"}]}}))
        .unwrap();
        actor.handle(Cmd::Apply {
            event: hello,
            reply: None,
        });
        let (reply, rx) = oneshot::channel();
        actor.handle(Cmd::Submit {
            event: Event::Intent {
                at: 0,
                viewer: "v-1".into(),
                client: Some("198.51.100.9".into()),
                id: "c-0".into(),
                name: "claim_gpu".into(),
                args: serde_json::json!({}),
                check: None,
                secret: Some("0123456789abcdef0123456789abcdef".into()),
            },
            reply,
        });
        let s = rx.await.unwrap();
        assert_eq!(
            s.receipt.map(|r| r.outcome),
            Some(fleet_core::Outcome::Committed)
        );
        let c = s.claim.expect("the claim with its receipt");
        assert_eq!(c.id, "claim-1");
    }

    /// During a store outage the answers of closed sockets do not pile up: each reconnect
    /// submits its unanswered intents again, and the actor keeps only answers still awaited.
    #[tokio::test]
    async fn answers_nobody_awaits_are_not_kept() {
        let (tx, _rx) = mpsc::unbounded_channel::<store::StoreCmd>();
        let out = Outputs {
            snapshot: watch::channel(snapshot_of(&State::new(Config::default(), 0))).0,
            ec2: watch::channel(Vec::new()).0,
            lambda: watch::channel(Vec::new()).0,
            ec2_layout: watch::channel(None).0,
            broadcast_layout: watch::channel(None).0,
            power: watch::channel(String::new()).0,
            draining: Default::default(),
            scene: watch::channel(None).0,
            budget: watch::channel(None).0,
            zone: None,
            store: Some(tx),
            owned: watch::channel(false).0,
        };
        let cfg = Config {
            light_interval_ms: 0,
            ..Config::default()
        };
        let mut actor = Actor::new(State::new(cfg, 0), out, Box::new(|| 1_000));
        let light = |i: usize| Event::Intent {
            at: 0,
            viewer: "v-1".into(),
            client: Some("198.51.100.9".into()),
            id: format!("l-{i}"),
            name: "light".into(),
            args: serde_json::json!({"pos": [i as f64, 2.0, -90.0], "color": [1.0, 0.5, 0.0]}),
            check: None,
            secret: None,
        };
        let submit = |actor: &mut Actor, i: usize| {
            let (reply, rx) = oneshot::channel();
            actor.handle(Cmd::Submit {
                event: light(i),
                reply,
            });
            rx
        };
        let kept = submit(&mut actor, 0);
        // The same unstored intent submitted again by ten connections that each closed.
        for _ in 0..10 {
            drop(submit(&mut actor, 1));
        }
        let live = submit(&mut actor, 2);
        assert_eq!(
            actor.waiting.len(),
            2,
            "only answers still awaited are kept"
        );
        drop((kept, live));
        // The same through `Cmd::Apply`, whose reply waits in a task of its own (a `word`
        // intent's path): a reply nobody awaits any more ends that wait.
        for _ in 0..10 {
            let (reply, rx) = oneshot::channel();
            actor.handle(Cmd::Apply {
                event: light(3),
                reply: Some(reply),
            });
            drop(rx);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let live = submit(&mut actor, 4);
        assert_eq!(
            actor.waiting.len(),
            1,
            "a deferred reply nobody awaits keeps no answer"
        );
        drop(live);
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

#[cfg(test)]
mod admission_key_tests {
    use super::{admission, admission_keys};

    /// Every coordinator hands its key to the renderers and aggregators, so a direct stream
    /// needs a token from `/live` also without a broadcast; before, a fleet without one checked
    /// nothing and any client could open frame streams past every `/live` limit.
    #[test]
    fn direct_streams_are_checked_with_or_without_a_broadcast() {
        let open = admission::Gate::open();
        assert_eq!(open.seats.capacity(), None, "no broadcast");
        let (admit, mirror) = admission_keys(&open);
        assert_eq!(admit, vec![open.key.hex()]);
        assert_eq!(mirror, vec![open.key.hex()]);
        let mut broadcast = admission::Gate::open();
        broadcast.seats = admission::Seats::new(Some(500));
        assert_eq!(admission_keys(&broadcast).0, vec![broadcast.key.hex()]);
    }
}

#[cfg(test)]
mod admin_tests {
    use super::admin_addresses;

    /// The admin endpoint has no authentication, so it binds loopback only; any other address
    /// stops the coordinator before it starts.
    #[tokio::test]
    async fn the_admin_endpoint_binds_only_loopback() {
        for ok in ["127.0.0.1:9000", "[::1]:9000", "127.0.0.2:0"] {
            let a = admin_addresses(ok)
                .await
                .unwrap_or_else(|e| panic!("{ok}: {e:#}"));
            assert!(a.iter().all(|a| a.ip().is_loopback()), "{ok}: {a:?}");
        }
        for refused in [
            "0.0.0.0:9000",
            "[::]:9000",
            "10.0.0.5:9000",
            "192.0.2.1:9000",
            "[2001:db8::1]:9000",
            "[::ffff:127.0.0.1]:9000",
        ] {
            let e = admin_addresses(refused)
                .await
                .expect_err(refused)
                .to_string();
            assert!(e.contains("not loopback"), "{refused}: {e}");
        }
        assert!(admin_addresses("no-port").await.is_err());
    }
}
