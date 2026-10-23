//! `fcvm fleet coord`: the multiplayer authority.
//!
//! One task, the [`Actor`], owns the fleet state. Browsers, agents, the EC2 driver and the
//! tick timer only send it commands; it stamps each event with the time, runs
//! `fleet_core::apply`, and fans the result out: a snapshot for browsers (a watch channel,
//! so a slow socket skips to the latest), an `assign`/`scene` diff for each agent, the EC2
//! assignment, and the state file. No other task reads or writes the state, so there is
//! nothing to lock and no order to race on.

pub mod agents;
pub mod aws;
pub mod ec2;
pub mod live;
pub mod poster;
pub mod scale;
pub mod sigv4;
pub mod store;
pub mod text_check;
pub mod turnstile;

use crate::cli::{FleetAggregateCodec, FleetCoordArgs, FleetEc2, FleetTextCheck};
use crate::fleet::aggregate::codec::{Codec, Ffmpeg, Raw};
use crate::fleet::aggregate::{Layout, TileSpec};
use crate::fleet::now_ms;
use anyhow::{bail, Context, Result};
use fleet_core::protocol::{AgentMsg, AssignSlot, ClaimMsg, CoordMsg, ServerMsg, SharedScene};
use fleet_core::tiling::{half_rect, Half, Rect};
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
    /// Stop: publish the last snapshot, write the state file, then answer.
    Shutdown {
        ack: oneshot::Sender<()>,
    },
}

struct AgentConn {
    host: String,
    tx: mpsc::UnboundedSender<CoordMsg>,
    last_assign: Option<(Vec<AssignSlot>, Option<Rect>)>,
    last_scene: Option<SharedScene>,
}

/// The channels the actor publishes on.
pub struct Outputs {
    pub snapshot: watch::Sender<Arc<Snapshot>>,
    pub ec2: watch::Sender<Vec<AssignSlot>>,
    /// The EC2 half's aggregator layout.
    pub ec2_layout: watch::Sender<Option<Layout>>,
    /// The fleet's power state (`awake`, `asleep`, `waking`).
    pub power: watch::Sender<String>,
    pub store: Option<store::StoreTx>,
}

/// Minimum time between two browser snapshots; versions in between are skipped.
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(50);
/// Minimum time between two state file writes.
const STORE_INTERVAL: Duration = Duration::from_secs(1);

pub struct Actor {
    state: State,
    /// The EC2 aggregator as the coordinator reaches it (it runs as our child).
    ec2_aggregate_local: Option<String>,
    out: Outputs,
    agents: HashMap<u64, AgentConn>,
    last_at: u64,
    published: u64,
    stored: u64,
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
        let published = state.version;
        out.snapshot.send_replace(snapshot_of(&state));
        Actor {
            ec2_aggregate_local: None,
            stored: state.version,
            state,
            out,
            agents: HashMap::new(),
            last_at: 0,
            published,
            clock,
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
        let state = std::mem::replace(&mut self.state, State::new(Config::default(), 0));
        self.state = apply(state, &event);
        if self.state.version != before {
            self.dispatch_agents();
        }
    }

    /// Send each agent its assignment and the scene when they differ from what it last got.
    fn dispatch_agents(&mut self) {
        let assignments = host_assignments(&self.state);
        let regions = host_regions(&self.state);
        let scene = shared_scene(&self.state);
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
        }
        let f = &self.state.fleet;
        let ec2_layout = Layout {
            epoch_unix_ms: f.scene.epoch_unix_ms,
            image_aspect: self.state.config.image_aspect,
            region: half_rect(Half::Ec2),
            tiles: f
                .ec2
                .values()
                .filter_map(|s| {
                    Some(TileSpec {
                        id: s.id.clone(),
                        rect: s.rect?,
                        url: s.observed.frame_url.clone(),
                    })
                })
                .collect(),
        };
        self.out.ec2_layout.send_if_modified(|cur| {
            let changed = cur.as_ref() != Some(&ec2_layout);
            *cur = Some(ec2_layout.clone());
            changed
        });
        let power = self.state.fleet.power.state.clone();
        self.out.power.send_if_modified(|cur| {
            let changed = *cur != power;
            *cur = power.clone();
            changed
        });
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
    /// is on disk (at once when nothing needed writing).
    fn store(&mut self) -> oneshot::Receiver<()> {
        let (ack, done) = oneshot::channel();
        let Some(tx) = &self.out.store else {
            let _ = ack.send(());
            return done;
        };
        if self.state.version == self.stored {
            let _ = ack.send(());
            return done;
        }
        match serde_json::to_vec(&self.state) {
            Ok(bytes) => {
                let _ = tx.send(store::StoreCmd {
                    bytes: Arc::new(bytes),
                    ack: Some(ack),
                });
                self.stored = self.state.version;
            }
            Err(e) => {
                tracing::error!(error = %e, "cannot serialize the state");
                let _ = ack.send(());
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
                let d = &f.director;
                let mut sources: Vec<String> = f
                    .hosts
                    .values()
                    .filter_map(|h| h.aggregate_url.clone())
                    .collect();
                if let Some(u) = self
                    .ec2_aggregate_local
                    .clone()
                    .or_else(|| self.state.config.ec2_aggregate_url.clone())
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
                            s.observed.frame_url.clone()
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
                let taken = self
                    .agents
                    .iter()
                    .any(|(c, a)| *c != conn && &a.host == host);
                let renamed = self.agents.get(&conn).is_some_and(|a| &a.host != host);
                if taken || renamed {
                    tracing::warn!(%host, conn, taken, renamed, "refusing an agent hello");
                    let _ = reply.send(false);
                    return;
                }
                let host = host.clone();
                self.agents.insert(
                    conn,
                    AgentConn {
                        host: host.clone(),
                        tx,
                        last_assign: None,
                        last_scene: None,
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
                self.apply(Event::Agent { at: 0, host, msg });
            }
            Cmd::Shutdown { ack } => {
                // `run` handles shutdown; a direct call only acknowledges.
                let _ = ack.send(());
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

    /// Run until every command sender is dropped.
    pub async fn run(mut self, mut rx: mpsc::Receiver<Cmd>) {
        let mut snap_tick = tokio::time::interval(SNAPSHOT_INTERVAL);
        snap_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut store_tick = tokio::time::interval(STORE_INTERVAL);
        store_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let ack = loop {
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(Cmd::Shutdown { ack }) => break Some(ack),
                    Some(c) => self.handle(c),
                    None => break None,
                },
                _ = snap_tick.tick() => self.publish(),
                _ = store_tick.tick() => {
                    drop(self.store());
                }
            }
        };
        self.publish();
        let written = self.store();
        let _ = written.await;
        if let Some(ack) = ack {
            let _ = ack.send(());
        }
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
        | Event::ScaleInstances { at, .. } => *at = t,
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

    /// Stop the actor; returns once the last state is on disk.
    pub async fn shutdown(&self) {
        let (ack, done) = oneshot::channel();
        if self.cmds.send(Cmd::Shutdown { ack }).await.is_ok() {
            let _ = done.await;
        }
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

/// Start the actor, the tick timer and the EC2 driver; returns the handle the transports
/// use and the actor task.
pub fn start(
    state: State,
    store: Option<store::StoreTx>,
    ec2: Option<Arc<dyn ec2::Ec2Driver>>,
    ec2_aggregate_local: Option<String>,
    power: Option<scale::PowerConfig>,
) -> (Handle, tokio::task::JoinHandle<()>) {
    let (cmds, rx) = mpsc::channel(4096);
    let (snap_tx, snap_rx) = watch::channel(Arc::new(Snapshot {
        version: 0,
        text: String::new(),
    }));
    let (ec2_tx, ec2_rx) = watch::channel(Vec::new());
    let (ec2_layout_tx, ec2_layout_rx) = watch::channel(None);
    let (power_tx, power_rx) = watch::channel(state.fleet.power.state.clone());
    let actor = Actor::new(
        state,
        Outputs {
            snapshot: snap_tx,
            ec2: ec2_tx,
            ec2_layout: ec2_layout_tx,
            power: power_tx,
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
        tokio::spawn(ec2::run(driver, ec2_rx, handle.clone()));
    }
    if let Some(cfg) = power {
        tokio::spawn(scale::run(cfg, power_rx, handle.clone()));
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

pub async fn run(args: FleetCoordArgs) -> Result<()> {
    let config = Config {
        max_tiles: args.max_tiles,
        ec2_max: match args.ec2 {
            FleetEc2::None => 0,
            FleetEc2::Fake => args.max_tiles,
        },
        ec2_aggregate_url: args.ec2_aggregate_url.clone(),
        race_interval_secs: args.race_every,
        restart_cooldown_secs: args.restart_cooldown,
        camera_seed: args.camera_seed,
        max_borrowed: args.max_borrowed,
        power_managed: !args.scale_groups.is_empty(),
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
        None => (State::new(config, now_ms()), None),
    };
    let ec2: Option<Arc<dyn ec2::Ec2Driver>> = match args.ec2 {
        FleetEc2::None => None,
        FleetEc2::Fake => Some(Arc::new(ec2::FakeEc2::new(
            Duration::from_secs(2),
            Duration::from_secs(20),
        ))),
    };
    let ec2_local = args.ec2_aggregate_listen.as_ref().map(|l| {
        format!(
            "ws://{}/frames",
            crate::fleet::aggregate::child::connect_addr(l)
        )
    });
    let power = if args.scale_groups.is_empty() {
        None
    } else {
        Some(scale::PowerConfig {
            driver: Arc::new(scale::AwsAutoScaling::new()?),
            groups: args.scale_groups.clone(),
            awake_capacity: args.awake_capacity,
            poll: Duration::from_secs(5),
        })
    };
    let (handle, _actor) = start(state, store_tx, ec2, ec2_local, power);
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
    handle.shutdown().await;
    tracing::info!("coordinator stopped; state written");
    Ok(())
}
