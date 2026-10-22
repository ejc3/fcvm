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
pub mod sigv4;
pub mod store;
pub mod text_check;

use crate::cli::{FleetCoordArgs, FleetEc2, FleetTextCheck};
use crate::fleet::now_ms;
use anyhow::{bail, Context, Result};
use fleet_core::protocol::{AgentMsg, AssignSlot, CoordMsg, ServerMsg, SharedScene};
use fleet_core::view::{ec2_assignment, host_assignments, shared_scene};
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
        viewer: String,
        args: serde_json::Value,
        reply: oneshot::Sender<Option<String>>,
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
    last_assign: Option<Vec<AssignSlot>>,
    last_scene: Option<SharedScene>,
}

/// The channels the actor publishes on.
pub struct Outputs {
    pub snapshot: watch::Sender<Arc<Snapshot>>,
    pub ec2: watch::Sender<Vec<AssignSlot>>,
    pub store: Option<store::StoreTx>,
}

/// Minimum time between two browser snapshots; versions in between are skipped.
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(50);
/// Minimum time between two state file writes.
const STORE_INTERVAL: Duration = Duration::from_secs(1);

pub struct Actor {
    state: State,
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
        let scene = shared_scene(&self.state);
        for conn in self.agents.values_mut() {
            let mine = assignments.get(&conn.host).cloned().unwrap_or_default();
            // The scene goes first, so a slot's first scene post has it.
            if conn.last_scene.as_ref() != Some(&scene) {
                let _ = conn.tx.send(CoordMsg::Scene(scene.clone()));
                conn.last_scene = Some(scene.clone());
            }
            if conn.last_assign.as_ref() != Some(&mine) {
                let _ = conn.tx.send(CoordMsg::Assign {
                    slots: mine.clone(),
                });
                conn.last_assign = Some(mine);
            }
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
                viewer,
                args,
                reply,
            } => {
                let at = self.stamp();
                let _ = reply.send(fleet_core::precheck_word(&self.state, &viewer, &args, at));
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
        | Event::Ec2 { at, .. } => *at = t,
    }
}

/// Everything the coordinator's tasks share.
#[derive(Clone)]
pub struct Handle {
    pub cmds: mpsc::Sender<Cmd>,
    pub snapshot: watch::Receiver<Arc<Snapshot>>,
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

    pub async fn precheck_word(&self, viewer: &str, args: &serde_json::Value) -> Option<String> {
        let (tx, rx) = oneshot::channel();
        self.cmds
            .send(Cmd::PrecheckWord {
                viewer: viewer.to_string(),
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
) -> (Handle, tokio::task::JoinHandle<()>) {
    let (cmds, rx) = mpsc::channel(4096);
    let (snap_tx, snap_rx) = watch::channel(Arc::new(Snapshot {
        version: 0,
        text: String::new(),
    }));
    let (ec2_tx, ec2_rx) = watch::channel(Vec::new());
    let actor = Actor::new(
        state,
        Outputs {
            snapshot: snap_tx,
            ec2: ec2_tx,
            store,
        },
        Box::new(now_ms),
    );
    let task = tokio::spawn(actor.run(rx));
    let handle = Handle {
        cmds,
        snapshot: snap_rx,
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
    (handle, task)
}

pub async fn run(args: FleetCoordArgs) -> Result<()> {
    let config = Config {
        max_tiles: args.max_tiles,
        ec2_max: match args.ec2 {
            FleetEc2::None => 0,
            FleetEc2::Fake => args.max_tiles,
        },
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
                    s.resumed(config)
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
    let (handle, _actor) = start(state, store_tx, ec2);
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
    let origins = args.allowed_origins;
    tokio::select! {
        r = live::serve(listener, handle.clone(), checker, origins) => r?,
        _ = term.recv() => tracing::info!("SIGTERM"),
        _ = int.recv() => tracing::info!("SIGINT"),
    }
    // The state file holds the last state before the process exits; the agents' commands
    // are killed when their handles drop, and their VMs follow them (PR_SET_PDEATHSIG).
    handle.shutdown().await;
    tracing::info!("coordinator stopped; state written");
    Ok(())
}
