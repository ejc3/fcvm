//! Potato mode (docs/fleet/ARCHITECTURE.md, "Potato mode"): the messages between the
//! coordinator and the agent that runs a host's player VMs.
//!
//! The ball itself never passes through here: it goes from one player VM to the next. The
//! agent reports what the players say and what it does to them (sleep, wake), with the
//! host's clock; the coordinator tells the agent which round to play.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The round a host's players play. The agent reconciles to it like a slot assignment: a
/// round it has not played starts, and none stops the one it plays.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PotatoRound {
    pub round: u64,
    /// The players, by number, in throwing order. The agent throws the first ball to
    /// `ring[0]`.
    pub ring: Vec<u32>,
    /// The hold the round starts at, in ms.
    pub hold_ms: u64,
    /// How much the hold shrinks each lap, in thousandths.
    pub shrink_permille: u32,
    /// The hold never goes below this.
    pub min_hold_ms: u64,
    /// How long a thrower keeps trying to reach the catcher.
    pub flight_ms: u64,
    /// The colours in the ring of colours.
    pub colours: u32,
    /// A hold viewers asked for after the round started ("faster"): the agent passes it to
    /// the awake players, and a throw after it arrives holds no longer than this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pace_ms: Option<u64>,
}

/// One thing that happened to a player, on the host's clock (`at`, Unix ms).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PotatoEvent {
    pub at: u64,
    pub player: u32,
    #[serde(flatten)]
    pub kind: PotatoKind,
}

/// A player's colour, counter and chain (hex), as it reports them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PotatoProof {
    pub colour: u32,
    pub counter: u64,
    pub chain: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PotatoKind {
    /// The player said hello: on its first report connection (a new player), and on the
    /// first after each wake. `last_round` and `last_seq` name its last catch.
    Hello {
        #[serde(flatten)]
        proof: PotatoProof,
        colours: u32,
        connections: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_round: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_seq: Option<u64>,
    },
    /// `hold_us` is the hold the catcher keeps before it throws, in microseconds, after any
    /// pace cap; a player built before it reports none.
    Caught {
        round: u64,
        seq: u64,
        #[serde(flatten)]
        proof: PotatoProof,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hold_us: Option<u64>,
    },
    /// The catcher accepted the ball; `seq` is the ball's new seq, `to` its catcher.
    Threw {
        round: u64,
        seq: u64,
        to: u32,
        attempts: u32,
        wait_ms: f64,
        send_ms: f64,
    },
    /// Nothing accepted the ball within its flight window. `catcher_phase` is what the agent
    /// was doing with the catcher then (`asleep`, `sleeping`, `waking`, `awake`), and
    /// `catcher_phase_ms` how long it had been doing it.
    Dropped {
        round: u64,
        seq: u64,
        to: u32,
        attempts: u32,
        reason: String,
        catcher_phase: String,
        catcher_phase_ms: f64,
    },
    /// The player is asleep: its snapshot is written and its fcvm has exited. `stages`
    /// splits `sleep_ms` (prepare, pause, save, disk, stop, in ms, as fcvm logs them).
    Slept {
        sleep_ms: f64,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        stages: BTreeMap<String, f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        written_mib: Option<f64>,
    },
    /// The agent started restoring the player.
    Waking {},
    /// The player's ball port accepts connections again, `wake_ms` after the restore
    /// started. `stages` splits it (network, disk, load, guest, listen).
    Woke {
        wake_ms: f64,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        stages: BTreeMap<String, f64>,
    },
    /// The catcher of a dropped ball started accepting, `after_ms` after the ball dropped.
    Listening { round: u64, seq: u64, after_ms: f64 },
    /// A sleep, a wake or a first boot failed.
    Failed { stage: String, error: String },
}

// ---------------------------------------------------------------------------------------
// The game. Everything below is pure: time arrives in the events (`at`, Unix ms).
// ---------------------------------------------------------------------------------------

/// The game's settings (the coordinator's flags), with the defaults the measurement set
/// (docs/fleet/ARCHITECTURE.md, "Potato mode", "The starting pace").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PotatoConfig {
    /// The ladder's ring sizes, smallest first. Sizes above the host's players are skipped.
    pub sizes: Vec<u32>,
    /// A size starts at `budget_ms / (size - 1)` ms of hold, and never below `start_floor_ms`.
    pub budget_ms: u64,
    pub start_floor_ms: u64,
    /// The hold shrinks by this many thousandths every lap.
    pub shrink_permille: u32,
    pub min_hold_ms: u64,
    pub flight_ms: u64,
    pub colours: u32,
    /// After a drop, the next round starts this long after.
    pub restart_after_ms: u64,
    /// A catch not reported this long after its deadline (hold plus flight) ends the round.
    pub report_slack_ms: u64,
    /// One `potato_faster` per client this often; it shortens the hold by a fifth.
    pub faster_interval_ms: u64,
    /// One `potato_restart` per client this often, and none for anyone this long after a
    /// round started.
    pub restart_interval_ms: u64,
}

impl Default for PotatoConfig {
    fn default() -> Self {
        PotatoConfig {
            sizes: vec![2, 4, 8, 16, 32, 64, 96],
            budget_ms: 2000,
            start_floor_ms: 400,
            shrink_permille: 100,
            min_hold_ms: 2,
            flight_ms: 100,
            colours: 12,
            restart_after_ms: 5000,
            report_slack_ms: 3000,
            faster_interval_ms: 2000,
            restart_interval_ms: 10_000,
        }
    }
}

impl PotatoConfig {
    pub fn start_hold_ms(&self, size: u32) -> u64 {
        (self.budget_ms / u64::from(size.saturating_sub(1).max(1)))
            .max(self.start_floor_ms)
            .max(self.min_hold_ms)
    }
}

/// One size of the ladder. `record_hold_ms` is the shortest hold of a lap a round of this
/// size completed, ever; `broke` is what ended its latest round.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Rung {
    pub size: u32,
    #[serde(default)]
    pub record_hold_ms: Option<u64>,
    #[serde(default)]
    pub record_at: Option<u64>,
    #[serde(default)]
    pub broke: Option<Drop>,
    #[serde(default)]
    pub rounds: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub value: f64,
    pub size: u32,
    #[serde(default)]
    pub player: Option<u32>,
    pub at: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Records {
    /// The fastest full sleep and wake of a player in a round, ms.
    #[serde(default)]
    pub fastest_cycle_ms: Option<Record>,
    /// The most catches in one round.
    #[serde(default)]
    pub longest_round: Option<Record>,
}

/// Why the ball dropped, and the stage that was late.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Drop {
    pub at: u64,
    pub round: u64,
    pub size: u32,
    /// `not_listening`, `proof`, `<stage>_failed` (an agent's `failed` report: `boot`,
    /// `sleep`, `wake`, `kickoff`), `deadline`, `host_gone`.
    pub reason: String,
    /// The player at fault: the catcher nobody could reach, or the one whose proof failed.
    pub player: Option<u32>,
    pub thrower: Option<u32>,
    pub seq: u64,
    pub lap: u64,
    pub hold_ms: u64,
    /// The late stage (`asleep`, `sleeping`, `waking`, `proof`, ...) and how long the player
    /// had been in it when the ball dropped.
    pub stage: String,
    pub stage_ms: f64,
    /// When the catcher of a dropped ball did start accepting, ms after the drop.
    #[serde(default)]
    pub listening_after_ms: Option<f64>,
    /// The catcher's last sleep and wake, split into stages, ms.
    #[serde(default)]
    pub sleep_stages: BTreeMap<String, f64>,
    #[serde(default)]
    pub wake_stages: BTreeMap<String, f64>,
    pub detail: String,
}

/// What the coordinator knows of a player: the state it last verified, and what the agent
/// is doing with it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlayerState {
    pub colour: u32,
    pub counter: u64,
    pub chain: String,
    pub phase: String,
    pub since: u64,
    #[serde(default)]
    pub last_sleep_ms: Option<f64>,
    #[serde(default)]
    pub last_wake_ms: Option<f64>,
    #[serde(default)]
    pub sleep_stages: BTreeMap<String, f64>,
    #[serde(default)]
    pub wake_stages: BTreeMap<String, f64>,
    /// A hello one catch ahead, waiting for that catch's report to confirm it.
    #[serde(default)]
    pub ahead: Option<PotatoProof>,
}

/// The ball in the air: thrown by `from` to `to` at `at`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Flight {
    pub from: u32,
    pub to: u32,
    pub at: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Round {
    pub id: u64,
    pub size: u32,
    pub ring: Vec<u32>,
    pub started_at: u64,
    pub start_hold_ms: u64,
    pub shrink_permille: u32,
    pub min_hold_ms: u64,
    pub flight_ms: u64,
    pub colours: u32,
    #[serde(default)]
    pub pace_ms: Option<u64>,
    /// The latest catch: its seq, who caught it and when.
    #[serde(default)]
    pub seq: Option<u64>,
    #[serde(default)]
    pub holder: Option<u32>,
    #[serde(default)]
    pub last_catch_at: Option<u64>,
    #[serde(default)]
    pub flight: Option<Flight>,
    /// The shortest hold of a lap this round completed.
    #[serde(default)]
    pub best_lap_hold_ms: Option<u64>,
    /// Every catch up to this seq is verified: a lap counts only once all its catches are,
    /// whatever order the players' reports arrive in.
    #[serde(default)]
    pub verified_through: Option<u64>,
    /// Verified catches beyond `verified_through`.
    #[serde(default)]
    pub verified_ahead: std::collections::BTreeSet<u64>,
    /// The longest hold a catch of each lap carried, as it was caught (a pace applied mid-lap
    /// leaves the lap credited with its slower holds).
    #[serde(default)]
    pub lap_holds: BTreeMap<u64, u64>,
    /// The hold the latest catcher reported it keeps, in microseconds; none from players
    /// built before they reported it.
    #[serde(default)]
    pub kept_us: Option<u64>,
    /// When the coordinator read each catch not yet behind a completed lap, by seq, and the
    /// last catch of the latest completed lap: what a lap's pace is measured from.
    #[serde(default)]
    pub catch_at: BTreeMap<u64, u64>,
    /// The completed laps, oldest first, at most the last [`LAPS_KEPT`].
    #[serde(default)]
    pub laps: Vec<Lap>,
    /// The fastest pace of a completed lap.
    #[serde(default)]
    pub best_pace: Option<f64>,
}

/// The laps of a round the view keeps.
pub const LAPS_KEPT: usize = 60;

/// A completed lap: its measured pace, in catches per second, and the longest hold a catch
/// of it carried.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lap {
    pub lap: u64,
    pub pace: f64,
    pub hold_ms: u64,
}

impl Round {
    /// The hold the ball carries, in ms, capped by a viewers' pace: the hold the latest
    /// catcher reported it keeps. Players built before they reported it (`hold_us`) shrink
    /// the hold a tenth per lap in whole milliseconds, and for them this is that arithmetic
    /// at `seq`: it stops at 9 ms, as those players do.
    pub fn hold_at(&self, seq: u64) -> u64 {
        let h = match self.kept_us {
            Some(us) => fleet_potato::ms_of(us),
            None => {
                let laps = seq / self.ring.len().max(1) as u64;
                let mut h = self.start_hold_ms;
                for _ in 0..laps.min(10_000) {
                    let cut = h * u64::from(self.shrink_permille.min(1000)) / 1000;
                    h = (h - cut).max(self.min_hold_ms);
                }
                h
            }
        };
        match self.pace_ms {
            Some(p) => h.min(p.max(self.min_hold_ms)),
            None => h,
        }
    }

    pub fn lap(&self, seq: u64) -> u64 {
        seq / self.ring.len().max(1) as u64
    }

    /// How long a thrower retries for, in ms: the flight window, or its hold when it reports
    /// one and that is shorter.
    pub fn flight_window_ms(&self) -> u64 {
        match self.kept_us {
            Some(us) => self.flight_ms.min(fleet_potato::ms_of(us)),
            None => self.flight_ms,
        }
    }

    /// The pace of completed lap `lap`, in catches per second, from the times the coordinator
    /// read its catches: the lap's catches over the time from the catch before its first to
    /// its last. Lap 0 has no catch before it: its last minus its first, over one catch fewer.
    fn lap_pace(&self, lap: u64) -> Option<f64> {
        let n = self.ring.len() as u64;
        let last = *self.catch_at.get(&((lap + 1) * n - 1))?;
        let before = (lap * n).checked_sub(1).and_then(|s| self.catch_at.get(&s));
        let (from, catches) = match before {
            Some(t) => (*t, n),
            None => (*self.catch_at.get(&(lap * n))?, n - 1),
        };
        let wall = last.checked_sub(from).filter(|w| *w > 0)?;
        (catches > 0).then(|| (catches as f64 * 100_000.0 / wall as f64).round() / 100.0)
    }

    /// When the next catch must have been reported by.
    pub fn deadline(&self, slack_ms: u64) -> Option<u64> {
        let seq = self.seq?;
        Some(self.last_catch_at? + self.hold_at(seq) + self.flight_ms + slack_ms)
    }
}

/// The potato game's state. `ladder`, `rung`, `records`, `round_count` and `last_drop` are
/// acknowledged state and stay in the coordinator's state file across a restart; the round,
/// the players and the host do not, because no agent connection survives one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Potato {
    #[serde(default)]
    pub ladder: Vec<Rung>,
    #[serde(default)]
    pub rung: usize,
    #[serde(default)]
    pub records: Records,
    #[serde(default)]
    pub round_count: u64,
    #[serde(default)]
    pub last_drop: Option<Drop>,
    #[serde(default)]
    pub round: Option<Round>,
    #[serde(default)]
    pub players: BTreeMap<u32, PlayerState>,
    /// The host running the players, and how many it runs.
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub capacity: u32,
    #[serde(default)]
    pub next_round_at: Option<u64>,
    #[serde(default)]
    pub last_faster_at: BTreeMap<String, u64>,
    #[serde(default)]
    pub last_restart_at: BTreeMap<String, u64>,
}

/// What an intent did.
#[derive(Debug, Clone, PartialEq)]
pub enum PotatoIntent {
    Done,
    Refused(String),
}

impl Potato {
    /// The ladder for `cfg`, keeping every size's record.
    fn ensure_ladder(&mut self, cfg: &PotatoConfig) {
        for size in &cfg.sizes {
            if !self.ladder.iter().any(|r| r.size == *size) {
                self.ladder.push(Rung {
                    size: *size,
                    ..Rung::default()
                });
            }
        }
        self.ladder.sort_by_key(|r| r.size);
        if self.rung >= self.ladder.len() {
            self.rung = 0;
        }
    }

    /// The first rung from `from` on (wrapping) whose size the host runs; `from` itself,
    /// wrapped, when no host is connected.
    fn next_rung(&self, from: usize) -> usize {
        let n = self.ladder.len();
        if n == 0 {
            return 0;
        }
        (0..n)
            .map(|k| (from + k) % n)
            .find(|i| self.capacity == 0 || self.ladder[*i].size <= self.capacity)
            .unwrap_or(from % n)
    }

    /// A restarted coordinator: no agent is connected yet, so no round, players or host.
    pub fn resumed(&mut self) {
        self.round = None;
        self.players.clear();
        self.host = None;
        self.capacity = 0;
        self.next_round_at = None;
    }

    /// An agent said hello. A host with players becomes the potato host (the first one
    /// that connects); its players are all new.
    pub fn agent_hello(&mut self, host: &str, players: u32, at: u64, cfg: &PotatoConfig) {
        if players == 0 {
            if self.host.as_deref() == Some(host) {
                self.agent_gone(host, at, cfg);
            }
            return;
        }
        if self.host.as_deref().is_some_and(|h| h != host) {
            return;
        }
        // a new connection from the same host is a new agent: its players start fresh
        if self.host.is_some() {
            self.end_quietly();
        }
        self.ensure_ladder(cfg);
        self.host = Some(host.to_string());
        self.capacity = players;
        self.players.clear();
        self.next_round_at = Some(at + 1000);
    }

    /// The potato host disconnected. Its round ends with what it achieved counted (the size's
    /// rounds, what ended it, the longest round), and the ladder stays at that size, so a host
    /// that comes back plays it again.
    pub fn agent_gone(&mut self, host: &str, at: u64, cfg: &PotatoConfig) {
        if self.host.as_deref() != Some(host) {
            return;
        }
        if let Some(r) = &self.round {
            let seq = r.seq.unwrap_or(0);
            let rung = self.rung;
            self.drop_ball(
                at,
                "host_gone",
                None,
                None,
                seq,
                "host",
                0.0,
                "the host running the players disconnected".into(),
                cfg,
            );
            self.rung = rung;
        }
        self.round = None;
        self.players.clear();
        self.host = None;
        self.capacity = 0;
        self.next_round_at = None;
    }

    /// End the round without a drop or a record (a new agent, a viewer's restart).
    fn end_quietly(&mut self) {
        self.round = None;
    }

    /// The round the potato host's agent must play.
    pub fn order(&self) -> Option<PotatoRound> {
        let r = self.round.as_ref()?;
        Some(PotatoRound {
            round: r.id,
            ring: r.ring.clone(),
            hold_ms: r.start_hold_ms,
            shrink_permille: r.shrink_permille,
            min_hold_ms: r.min_hold_ms,
            flight_ms: r.flight_ms,
            colours: r.colours,
            pace_ms: r.pace_ms,
        })
    }

    /// Once a second: start a round when one is due, and end one whose catch is overdue.
    /// `awake` is false while the fleet sleeps: no round starts then.
    pub fn tick(&mut self, at: u64, awake: bool, cfg: &PotatoConfig) {
        if let Some(r) = &self.round {
            if let Some(deadline) = r.deadline(cfg.report_slack_ms) {
                if at > deadline {
                    let seq = r.seq.unwrap_or(0);
                    let next = r.ring[((seq + 1) % r.ring.len() as u64) as usize];
                    let detail = format!(
                        "no catch reported {} ms after the ball's deadline",
                        at - deadline + cfg.report_slack_ms
                    );
                    self.drop_ball(
                        at,
                        "deadline",
                        Some(next),
                        r.holder,
                        seq + 1,
                        "report",
                        (at - deadline) as f64,
                        detail,
                        cfg,
                    );
                }
            } else if at > r.started_at + 120_000 {
                // the first catch never came (players could not boot or wake)
                self.drop_ball(
                    at,
                    "deadline",
                    r.ring.first().copied(),
                    None,
                    0,
                    "start",
                    (at - r.started_at) as f64,
                    "the round's first catch was not reported within 120 s".into(),
                    cfg,
                );
            }
            return;
        }
        let due = self.next_round_at.is_some_and(|t| at >= t);
        if !due || !awake || self.host.is_none() || self.capacity < 2 {
            return;
        }
        self.ensure_ladder(cfg);
        // the next size this host can run
        let n = self.ladder.len();
        let Some(i) = (0..n)
            .map(|k| (self.rung + k) % n)
            .find(|i| self.ladder[*i].size <= self.capacity)
        else {
            return;
        };
        self.rung = i;
        let size = self.ladder[i].size;
        self.round_count += 1;
        self.round = Some(Round {
            id: self.round_count,
            size,
            ring: (0..size).collect(),
            started_at: at,
            start_hold_ms: cfg.start_hold_ms(size),
            shrink_permille: cfg.shrink_permille,
            min_hold_ms: cfg.min_hold_ms,
            flight_ms: cfg.flight_ms,
            colours: cfg.colours,
            pace_ms: None,
            seq: None,
            holder: None,
            last_catch_at: None,
            flight: None,
            best_lap_hold_ms: None,
            verified_through: None,
            verified_ahead: Default::default(),
            lap_holds: BTreeMap::new(),
            kept_us: None,
            catch_at: BTreeMap::new(),
            laps: Vec::new(),
            best_pace: None,
        });
        self.next_round_at = None;
    }

    #[allow(clippy::too_many_arguments)]
    fn drop_ball(
        &mut self,
        at: u64,
        reason: &str,
        player: Option<u32>,
        thrower: Option<u32>,
        seq: u64,
        stage: &str,
        stage_ms: f64,
        detail: String,
        cfg: &PotatoConfig,
    ) {
        let Some(r) = self.round.take() else { return };
        let (sleep_stages, wake_stages) = player
            .and_then(|p| self.players.get(&p))
            .map(|p| (p.sleep_stages.clone(), p.wake_stages.clone()))
            .unwrap_or_default();
        let d = Drop {
            at,
            round: r.id,
            size: r.size,
            reason: reason.into(),
            player,
            thrower,
            seq,
            lap: r.lap(seq),
            hold_ms: r.hold_at(seq),
            stage: stage.into(),
            stage_ms,
            listening_after_ms: None,
            sleep_stages,
            wake_stages,
            detail,
        };
        if let Some(rung) = self.ladder.iter_mut().find(|g| g.size == r.size) {
            rung.rounds += 1;
            rung.broke = Some(d.clone());
            if let Some(h) = r.best_lap_hold_ms {
                if rung.record_hold_ms.is_none_or(|old| h < old) {
                    rung.record_hold_ms = Some(h);
                    rung.record_at = Some(at);
                }
            }
        }
        let catches = r.verified_through.map_or(0, |s| s + 1);
        if catches > 0
            && self
                .records
                .longest_round
                .as_ref()
                .is_none_or(|old| catches as f64 > old.value)
        {
            self.records.longest_round = Some(Record {
                value: catches as f64,
                size: r.size,
                player: None,
                at,
            });
        }
        self.last_drop = Some(d);
        // up the ladder to the next size the host runs; after the last, back to the first
        self.rung = self.next_rung(self.rung + 1);
        self.next_round_at = Some(at + cfg.restart_after_ms);
    }

    /// A player's state stepped by a catch.
    fn stepped(prev: &PlayerState, round: u64, seq: u64, colours: u32) -> Option<(u32, String)> {
        let chain = fleet_potato::chain_from_hex(&prev.chain)?;
        let colour = fleet_potato::next_colour(prev.colour, colours);
        let next = fleet_potato::chain_step(&chain, round, seq, colour);
        Some((colour, fleet_potato::chain_hex(&next)))
    }

    /// An event from the potato host's agent, received at `at` on the coordinator's clock.
    /// Every time the game keeps is the coordinator's: the deadlines are measured against its
    /// ticks, and the host's clock (`e.at`) can differ from it.
    pub fn event(&mut self, host: &str, e: &PotatoEvent, at: u64, cfg: &PotatoConfig) {
        if self.host.as_deref() != Some(host) {
            return;
        }
        let colours = self.round.as_ref().map_or(cfg.colours, |r| r.colours);
        let in_ring = self
            .round
            .as_ref()
            .is_some_and(|r| r.ring.contains(&e.player));
        match &e.kind {
            PotatoKind::Hello {
                proof,
                connections,
                last_round,
                last_seq,
                colours: c,
            } => {
                let fresh = *connections == 1 && proof.counter == 0;
                match self.players.get_mut(&e.player) {
                    Some(p) if !fresh => {
                        let same = p.colour == proof.colour
                            && p.counter == proof.counter
                            && p.chain == proof.chain;
                        let ahead = match (last_round, last_seq) {
                            (Some(r), Some(s)) => {
                                Self::stepped(p, *r, *s, *c).is_some_and(|(colour, chain)| {
                                    colour == proof.colour
                                        && chain == proof.chain
                                        && proof.counter == p.counter + 1
                                })
                            }
                            _ => false,
                        };
                        if ahead {
                            p.ahead = Some(proof.clone());
                        } else if !same {
                            let detail = format!(
                                "woke with colour {} and {} catches; it slept with colour {} and {}",
                                proof.colour, proof.counter, p.colour, p.counter
                            );
                            p.colour = proof.colour;
                            p.counter = proof.counter;
                            p.chain = proof.chain.clone();
                            if in_ring {
                                let seq = self.round.as_ref().and_then(|r| r.seq).unwrap_or(0);
                                self.drop_ball(
                                    at,
                                    "proof",
                                    Some(e.player),
                                    None,
                                    seq,
                                    "proof",
                                    0.0,
                                    detail,
                                    cfg,
                                );
                            }
                        }
                    }
                    _ => {
                        self.players.insert(
                            e.player,
                            PlayerState {
                                colour: proof.colour,
                                counter: proof.counter,
                                chain: proof.chain.clone(),
                                phase: "awake".into(),
                                since: at,
                                last_sleep_ms: None,
                                last_wake_ms: None,
                                sleep_stages: BTreeMap::new(),
                                wake_stages: BTreeMap::new(),
                                ahead: None,
                            },
                        );
                    }
                }
            }
            PotatoKind::Caught {
                round,
                seq,
                proof,
                hold_us,
            } => {
                let Some(p) = self.players.get_mut(&e.player) else {
                    return;
                };
                let expected = Self::stepped(p, *round, *seq, colours);
                let ok = expected.as_ref().is_some_and(|(colour, chain)| {
                    *colour == proof.colour
                        && *chain == proof.chain
                        && proof.counter == p.counter + 1
                })
                    // a hello one catch ahead named this catch: it must be this proof
                    && p.ahead.as_ref().is_none_or(|a| a == proof);
                p.colour = proof.colour;
                p.counter = proof.counter;
                p.chain = proof.chain.clone();
                p.ahead = None;
                let current = self.round.as_ref().is_some_and(|r| r.id == *round);
                if !current {
                    return;
                }
                if !ok {
                    let detail = match expected {
                        Some((colour, _)) => format!(
                            "caught with colour {} where {} was due",
                            proof.colour, colour
                        ),
                        None => "its chain is not hex".into(),
                    };
                    self.drop_ball(
                        at,
                        "proof",
                        Some(e.player),
                        None,
                        *seq,
                        "proof",
                        0.0,
                        detail,
                        cfg,
                    );
                    return;
                }
                let r = self.round.as_mut().unwrap();
                let n = r.ring.len() as u64;
                let lap = r.lap(*seq);
                // the hold this catcher keeps: what it reports, or the arithmetic of players
                // that report none
                let hold = match hold_us {
                    Some(us) => fleet_potato::ms_of(*us),
                    None => r.hold_at(*seq),
                };
                let longest = r.lap_holds.entry(lap).or_insert(hold);
                *longest = (*longest).max(hold);
                r.catch_at.insert(*seq, at);
                // the catches verified without a gap; a lap counts once all of its are
                r.verified_ahead.insert(*seq);
                let mut completed = Vec::new();
                loop {
                    let next = r.verified_through.map_or(0, |v| v + 1);
                    if !r.verified_ahead.remove(&next) {
                        break;
                    }
                    r.verified_through = Some(next);
                    if (next + 1).is_multiple_of(n) {
                        completed.push(next / n);
                    }
                }
                for l in completed {
                    let Some(h) = r.lap_holds.get(&l).copied() else {
                        continue;
                    };
                    if r.best_lap_hold_ms.is_none_or(|b| h < b) {
                        r.best_lap_hold_ms = Some(h);
                    }
                    let pace = r.lap_pace(l);
                    // the lap's last catch stays: the next lap is measured from it
                    r.catch_at.retain(|s, _| *s + 1 >= (l + 1) * n);
                    if let Some(pace) = pace {
                        if r.best_pace.is_none_or(|b| pace > b) {
                            r.best_pace = Some(pace);
                        }
                        r.laps.push(Lap {
                            lap: l,
                            pace,
                            hold_ms: h,
                        });
                        if r.laps.len() > LAPS_KEPT {
                            let cut = r.laps.len() - LAPS_KEPT;
                            r.laps.drain(..cut);
                        }
                    }
                    // a size's record is acknowledged as soon as the lap is: it stays in the
                    // state file even if the coordinator restarts before the ball drops
                    let size = r.size;
                    if let Some(rung) = self.ladder.iter_mut().find(|g| g.size == size) {
                        if rung.record_hold_ms.is_none_or(|old| h < old) {
                            rung.record_hold_ms = Some(h);
                            rung.record_at = Some(at);
                        }
                    }
                }
                let r = self.round.as_mut().unwrap();
                // a catch reported late (reports come on each player's own connection) is
                // verified and counted, but the latest catch stays the newest one
                if r.seq.is_none_or(|s| *seq >= s) {
                    r.seq = Some(*seq);
                    r.holder = Some(e.player);
                    r.last_catch_at = Some(at);
                    r.flight = None;
                    r.kept_us = *hold_us;
                }
            }
            PotatoKind::Threw { round, to, .. } => {
                // the agent puts a player to sleep once it has thrown
                if let Some(p) = self.players.get_mut(&e.player) {
                    p.phase = "sleeping".into();
                    p.since = at;
                }
                if let Some(r) = self.round.as_mut().filter(|r| r.id == *round) {
                    r.flight = Some(Flight {
                        from: e.player,
                        to: *to,
                        at,
                    });
                }
            }
            PotatoKind::Dropped {
                round,
                seq,
                to,
                catcher_phase,
                catcher_phase_ms,
                reason,
                attempts,
            } => {
                if self.round.as_ref().is_some_and(|r| r.id == *round) {
                    let detail = format!(
                        "player {to} was {catcher_phase} when player {} threw ({attempts} tries in {} ms: {reason})",
                        e.player,
                        self.round.as_ref().map_or(0, |r| r.flight_window_ms())
                    );
                    self.drop_ball(
                        at,
                        "not_listening",
                        Some(*to),
                        Some(e.player),
                        *seq,
                        catcher_phase,
                        *catcher_phase_ms,
                        detail,
                        cfg,
                    );
                }
            }
            PotatoKind::Listening {
                round,
                seq,
                after_ms,
            } => {
                if let Some(d) = self
                    .last_drop
                    .as_mut()
                    .filter(|d| d.round == *round && d.seq == *seq)
                {
                    d.listening_after_ms = Some(*after_ms);
                }
                if let Some(rung) = self
                    .ladder
                    .iter_mut()
                    .find(|g| g.broke.as_ref().is_some_and(|d| d.round == *round))
                {
                    if let Some(d) = rung.broke.as_mut() {
                        d.listening_after_ms = Some(*after_ms);
                    }
                }
            }
            PotatoKind::Slept {
                sleep_ms, stages, ..
            } => {
                if let Some(p) = self.players.get_mut(&e.player) {
                    p.phase = "asleep".into();
                    p.since = at;
                    p.last_sleep_ms = Some(*sleep_ms);
                    p.sleep_stages = stages.clone();
                }
            }
            PotatoKind::Waking {} => {
                if let Some(p) = self.players.get_mut(&e.player) {
                    p.phase = "waking".into();
                    p.since = at;
                }
            }
            PotatoKind::Woke { wake_ms, stages } => {
                let size = self.round.as_ref().map(|r| r.size);
                if let Some(p) = self.players.get_mut(&e.player) {
                    // a player that caught and threw before its wake was reported done is
                    // put to sleep by the agent once the wake is: it stays `sleeping`
                    if p.phase != "sleeping" {
                        p.phase = "awake".into();
                        p.since = at;
                    }
                    p.last_wake_ms = Some(*wake_ms);
                    p.wake_stages = stages.clone();
                    if let (Some(sleep), Some(size), true) = (p.last_sleep_ms, size, in_ring) {
                        let cycle = sleep + wake_ms;
                        if self
                            .records
                            .fastest_cycle_ms
                            .as_ref()
                            .is_none_or(|r| cycle < r.value)
                        {
                            self.records.fastest_cycle_ms = Some(Record {
                                value: (cycle * 10.0).round() / 10.0,
                                size,
                                player: Some(e.player),
                                at,
                            });
                        }
                    }
                }
            }
            PotatoKind::Failed { stage, error } => {
                if let Some(p) = self.players.get_mut(&e.player) {
                    p.phase = "failed".into();
                    p.since = at;
                }
                if in_ring {
                    let seq = self.round.as_ref().and_then(|r| r.seq).unwrap_or(0);
                    self.drop_ball(
                        at,
                        &format!("{stage}_failed"),
                        Some(e.player),
                        None,
                        seq,
                        stage,
                        0.0,
                        error.clone(),
                        cfg,
                    );
                }
            }
        }
    }

    /// `potato_faster`: the hold the ball carries now, less a fifth.
    pub fn faster(&mut self, client: &str, at: u64, cfg: &PotatoConfig) -> PotatoIntent {
        let Some(r) = self.round.as_mut() else {
            return PotatoIntent::Refused("no round is being played".into());
        };
        if self
            .last_faster_at
            .get(client)
            .is_some_and(|t| at < t + cfg.faster_interval_ms)
        {
            return PotatoIntent::Refused("one faster every 2 s".into());
        }
        let now = r.hold_at(r.seq.unwrap_or(0));
        if now <= r.min_hold_ms {
            return PotatoIntent::Refused("the hold is at its floor".into());
        }
        r.pace_ms = Some((now - now / 5).max(r.min_hold_ms));
        self.last_faster_at.insert(client.to_string(), at);
        PotatoIntent::Done
    }

    /// `potato_restart`: the current size starts again, without a record.
    pub fn restart(&mut self, client: &str, at: u64, cfg: &PotatoConfig) -> PotatoIntent {
        if self
            .last_restart_at
            .get(client)
            .is_some_and(|t| at < t + cfg.restart_interval_ms)
        {
            return PotatoIntent::Refused("one restart every 10 s".into());
        }
        match &self.round {
            Some(r) if at < r.started_at + cfg.restart_interval_ms => {
                return PotatoIntent::Refused("the round has only just started".into())
            }
            None if self.host.is_none() => {
                return PotatoIntent::Refused("no players are running".into())
            }
            _ => {}
        }
        self.round = None;
        self.next_round_at = Some(at + 1000);
        self.last_restart_at.insert(client.to_string(), at);
        PotatoIntent::Done
    }
}

/// Potato mode in the browser's snapshot (`/view/potato`): absent until a host has run
/// players. No chain is in it; the page shows colours, phases and times.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PotatoView {
    pub ladder: Vec<Rung>,
    /// The size being played, or the next one.
    pub size: Option<u32>,
    pub round: Option<RoundView>,
    pub players: Vec<PlayerView>,
    pub records: Records,
    pub last_drop: Option<Drop>,
    pub next_round_at: Option<u64>,
    pub colours: u32,
    /// The players the potato host runs; 0 while none is connected.
    pub capacity: u32,
    /// The host running the players (its agent's name, as a tile's `host`); null while none is
    /// connected. The viewer-path smoke attributes its potato probe to it for the image
    /// release gate (fleet/coord/stage-smoke.py).
    pub host: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RoundView {
    pub id: u64,
    pub size: u32,
    pub ring: Vec<u32>,
    pub started_at: u64,
    /// The latest catch and its lap; none before the first.
    pub seq: Option<u64>,
    pub lap: u64,
    /// The hold the ball carries now, and what it started at.
    pub hold_ms: u64,
    pub start_hold_ms: u64,
    pub min_hold_ms: u64,
    pub flight_ms: u64,
    pub pace_ms: Option<u64>,
    /// The time a player has between its throw and its next catch: (size - 1) holds.
    pub turnaround_ms: u64,
    pub holder: Option<u32>,
    pub last_catch_at: Option<u64>,
    pub flight: Option<Flight>,
    pub best_lap_hold_ms: Option<u64>,
    /// Catches per second over the last completed lap, measured from the times the
    /// coordinator read the catches; null before a lap completes.
    pub pace: Option<f64>,
    /// The fastest pace of a completed lap of this round.
    pub best_pace: Option<f64>,
    /// The completed laps, oldest first, at most the last [`LAPS_KEPT`].
    pub laps: Vec<Lap>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlayerView {
    pub player: u32,
    pub colour: u32,
    pub counter: u64,
    /// `awake`, `sleeping`, `asleep`, `waking`, `failed`.
    pub phase: String,
    pub since: u64,
    pub last_sleep_ms: Option<f64>,
    pub last_wake_ms: Option<f64>,
}

impl Potato {
    pub fn view(&self, cfg: &PotatoConfig) -> Option<PotatoView> {
        if self.host.is_none() && self.ladder.is_empty() {
            return None;
        }
        let round = self.round.as_ref().map(|r| {
            let seq = r.seq.unwrap_or(0);
            let hold = r.hold_at(seq);
            RoundView {
                id: r.id,
                size: r.size,
                ring: r.ring.clone(),
                started_at: r.started_at,
                seq: r.seq,
                lap: r.lap(seq),
                hold_ms: hold,
                start_hold_ms: r.start_hold_ms,
                min_hold_ms: r.min_hold_ms,
                flight_ms: r.flight_ms,
                pace_ms: r.pace_ms,
                turnaround_ms: hold * u64::from(r.size.saturating_sub(1)),
                holder: r.holder,
                last_catch_at: r.last_catch_at,
                flight: r.flight.clone(),
                best_lap_hold_ms: r.best_lap_hold_ms,
                pace: r.laps.last().map(|l| l.pace),
                best_pace: r.best_pace,
                laps: r.laps.clone(),
            }
        });
        Some(PotatoView {
            ladder: self.ladder.clone(),
            size: self
                .round
                .as_ref()
                .map(|r| r.size)
                .or_else(|| self.ladder.get(self.rung).map(|g| g.size)),
            round,
            players: self
                .players
                .iter()
                .map(|(n, p)| PlayerView {
                    player: *n,
                    colour: p.colour,
                    counter: p.counter,
                    phase: p.phase.clone(),
                    since: p.since,
                    last_sleep_ms: p.last_sleep_ms,
                    last_wake_ms: p.last_wake_ms,
                })
                .collect(),
            records: self.records.clone(),
            last_drop: self.last_drop.clone(),
            next_round_at: self.next_round_at,
            colours: self.round.as_ref().map_or(cfg.colours, |r| r.colours),
            capacity: self.capacity,
            host: self.host.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_have_a_flat_wire_form() {
        let e = PotatoEvent {
            at: 5,
            player: 3,
            kind: PotatoKind::Caught {
                round: 1,
                seq: 7,
                proof: PotatoProof {
                    colour: 2,
                    counter: 4,
                    chain: "ab".into(),
                },
                hold_us: None,
            },
        };
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(
            s,
            r#"{"at":5,"player":3,"kind":"caught","round":1,"seq":7,"colour":2,"counter":4,"chain":"ab"}"#
        );
        assert_eq!(serde_json::from_str::<PotatoEvent>(&s).unwrap(), e);
        let w: PotatoEvent =
            serde_json::from_str(r#"{"at":1,"player":0,"kind":"waking"}"#).unwrap();
        assert_eq!(w.kind, PotatoKind::Waking {});
    }

    #[test]
    fn a_catch_not_reported_by_its_deadline_drops_the_ball() {
        let cfg = PotatoConfig::default();
        let mut g = Potato::default();
        g.agent_hello("h", 2, 0, &cfg);
        for n in 0..2 {
            g.event(
                "h",
                &PotatoEvent {
                    at: 10,
                    player: n,
                    kind: PotatoKind::Hello {
                        proof: PotatoProof {
                            colour: n,
                            counter: 0,
                            chain: "00".repeat(32),
                        },
                        colours: 12,
                        connections: 1,
                        last_round: None,
                        last_seq: None,
                    },
                },
                10,
                &cfg,
            );
        }
        g.tick(1000, true, &cfg);
        let r = g.round.clone().expect("a round");
        let colour = 1;
        let chain = fleet_potato::chain_hex(&fleet_potato::chain_step(
            &fleet_potato::CHAIN_ZERO,
            r.id,
            0,
            colour,
        ));
        g.event(
            "h",
            &PotatoEvent {
                at: 1100,
                player: 0,
                kind: PotatoKind::Caught {
                    round: r.id,
                    seq: 0,
                    proof: PotatoProof {
                        colour,
                        counter: 1,
                        chain,
                    },
                    hold_us: None,
                },
            },
            1100,
            &cfg,
        );
        // the next catch is due by the hold plus the flight plus the report slack
        let due = 1100 + r.hold_at(0) + r.flight_ms + cfg.report_slack_ms;
        g.tick(due, true, &cfg);
        assert!(g.round.is_some(), "on time");
        g.tick(due + 1, true, &cfg);
        let d = g.last_drop.as_ref().expect("a drop");
        assert_eq!(
            (d.reason.as_str(), d.player, d.seq),
            ("deadline", Some(1), 1)
        );
        assert!(g.round.is_none());
    }

    /// A host with `n` new players and a round of all of them started.
    fn game(n: u32) -> (Potato, PotatoConfig, Vec<PotatoProof>) {
        let cfg = PotatoConfig::default();
        let mut g = Potato::default();
        g.agent_hello("h", n, 0, &cfg);
        let mut proofs = Vec::new();
        for p in 0..n {
            let proof = PotatoProof {
                colour: p,
                counter: 0,
                chain: "00".repeat(32),
            };
            g.event(
                "h",
                &PotatoEvent {
                    at: 10,
                    player: p,
                    kind: PotatoKind::Hello {
                        proof: proof.clone(),
                        colours: 12,
                        connections: 1,
                        last_round: None,
                        last_seq: None,
                    },
                },
                10,
                &cfg,
            );
            proofs.push(proof);
        }
        g.tick(1000, true, &cfg);
        (g, cfg, proofs)
    }

    /// A player in the ring that cannot wake (its sleep's memory file is gone: the host
    /// rebooted, or the memory directory was cleared) drops the ball at once, naming the wake,
    /// and the next round is scheduled; nothing waits for a catch that cannot come.
    #[test]
    fn a_player_that_cannot_wake_drops_the_ball_and_the_next_round_is_scheduled() {
        let (mut g, cfg, _) = game(3);
        let r = g.round.clone().expect("a round");
        let failed = r.ring[1];
        g.event(
            "h",
            &PotatoEvent {
                at: 1200,
                player: failed,
                kind: PotatoKind::Failed {
                    stage: "wake".into(),
                    error: "the memory file of snapshot potato-1 is gone".into(),
                },
            },
            1200,
            &cfg,
        );
        let d = g.last_drop.as_ref().expect("a drop");
        assert_eq!((d.reason.as_str(), d.player), ("wake_failed", Some(failed)));
        assert!(g.round.is_none(), "the round ended");
        assert_eq!(g.next_round_at, Some(1200 + cfg.restart_after_ms));
        assert_eq!(g.players[&failed].phase, "failed");
    }

    /// Player `p`'s catch of `seq` in `round`, stepped from its proof (or a wrong colour).
    fn catch(
        proofs: &mut [PotatoProof],
        p: u32,
        round: u64,
        seq: u64,
        at: u64,
        wrong: bool,
    ) -> PotatoEvent {
        let last = &proofs[p as usize];
        let colour = (last.colour + 1 + u32::from(wrong)) % 12;
        let chain = fleet_potato::chain_from_hex(&last.chain).unwrap();
        let proof = PotatoProof {
            colour,
            counter: last.counter + 1,
            chain: fleet_potato::chain_hex(&fleet_potato::chain_step(&chain, round, seq, colour)),
        };
        proofs[p as usize] = proof.clone();
        PotatoEvent {
            at,
            player: p,
            kind: PotatoKind::Caught {
                round,
                seq,
                proof,
                hold_us: None,
            },
        }
    }

    /// Reports come on each player's own connection, so a later catch can be read before an
    /// earlier one. A lap counts only once all its catches are verified: when the delayed
    /// one fails, the lap earns nothing and the round's verified catches stop before it.
    #[test]
    fn a_lap_counts_only_once_every_catch_in_it_is_verified() {
        let (mut g, cfg, mut proofs) = game(4);
        let id = g.round.as_ref().unwrap().id;
        let c0 = catch(&mut proofs, 0, id, 0, 1100, false);
        let c1 = catch(&mut proofs, 1, id, 1, 1200, true);
        let c2 = catch(&mut proofs, 2, id, 2, 1300, false);
        let c3 = catch(&mut proofs, 3, id, 3, 1400, false);
        let c4 = catch(&mut proofs, 0, id, 4, 1500, false);
        for e in [&c0, &c2, &c3, &c4] {
            g.event("h", e, e.at, &cfg);
        }
        assert_eq!(
            g.round.as_ref().unwrap().best_lap_hold_ms,
            None,
            "catch 1 is not verified yet"
        );
        g.event("h", &c1, c1.at, &cfg);
        assert_eq!(
            g.last_drop.as_ref().map(|d| d.reason.as_str()),
            Some("proof")
        );
        assert_eq!(
            g.ladder
                .iter()
                .find(|r| r.size == 4)
                .unwrap()
                .record_hold_ms,
            None
        );
        assert_eq!(
            g.records.longest_round.as_ref().map(|r| r.value),
            Some(1.0),
            "only catch 0 counts"
        );
    }

    /// A hello one catch ahead names its catch; the catch's report must then carry that same
    /// state, not another step that also follows from the last verified one.
    #[test]
    fn a_catch_must_match_the_hello_that_ran_ahead_of_it() {
        let (mut g, cfg, mut proofs) = game(2);
        let id = g.round.as_ref().unwrap().id;
        let before = proofs[0].clone();
        // the hello names a catch of another round
        let other = catch(&mut proofs, 0, id + 1, 0, 1100, false);
        let PotatoKind::Caught { proof: ahead, .. } = other.kind.clone() else {
            unreachable!()
        };
        g.event(
            "h",
            &PotatoEvent {
                at: 1100,
                player: 0,
                kind: PotatoKind::Hello {
                    proof: ahead,
                    colours: 12,
                    connections: 2,
                    last_round: Some(id + 1),
                    last_seq: Some(0),
                },
            },
            1100,
            &cfg,
        );
        assert!(
            g.round.is_some(),
            "a hello one valid step ahead is not a mismatch"
        );
        proofs[0] = before;
        let real = catch(&mut proofs, 0, id, 0, 1150, false);
        g.event("h", &real, real.at, &cfg);
        assert_eq!(
            g.last_drop.as_ref().map(|d| d.reason.as_str()),
            Some("proof")
        );
    }

    /// A completed lap's record is acknowledged state at once: a coordinator that restarts
    /// before the ball drops keeps it.
    #[test]
    fn a_completed_lap_is_a_record_before_the_ball_drops() {
        let (mut g, cfg, mut proofs) = game(2);
        let id = g.round.as_ref().unwrap().id;
        for (seq, p) in [(0, 0), (1, 1), (2, 0)] {
            let e = catch(&mut proofs, p, id, seq, 1100 + seq * 100, false);
            g.event("h", &e, e.at, &cfg);
        }
        let start = cfg.start_hold_ms(2);
        assert_eq!(g.ladder[0].record_hold_ms, Some(start));
        g.resumed();
        assert_eq!(g.ladder[0].record_hold_ms, Some(start));
        assert!(g.round.is_none());
    }

    /// A pace applied in the middle of a lap leaves the lap credited with the slower hold its
    /// first catches used, not the faster one in effect when it closed.
    #[test]
    fn a_lap_is_credited_with_its_slowest_hold() {
        let (mut g, cfg, mut proofs) = game(2);
        let id = g.round.as_ref().unwrap().id;
        let start = cfg.start_hold_ms(2);
        let e = catch(&mut proofs, 0, id, 0, 1100, false);
        g.event("h", &e, e.at, &cfg);
        assert_eq!(g.faster("v", 1200, &cfg), PotatoIntent::Done);
        let paced = g.round.as_ref().unwrap().pace_ms.unwrap();
        assert!(paced < start);
        for (seq, p) in [(1, 1), (2, 0)] {
            let e = catch(&mut proofs, p, id, seq, 1300 + seq * 100, false);
            g.event("h", &e, e.at, &cfg);
        }
        assert_eq!(g.round.as_ref().unwrap().best_lap_hold_ms, Some(start));
    }

    /// The pace is what the catch times say, not 1000 / hold: a ring of 2 at a 2000 ms hold
    /// whose catches were read 200 ms apart ran at 5 catches a second.
    #[test]
    fn pace_is_measured_from_the_catch_times() {
        let (mut g, cfg, mut proofs) = game(2);
        let id = g.round.as_ref().unwrap().id;
        let view = |g: &Potato| g.view(&cfg).unwrap().round.unwrap();
        let mut play = |g: &mut Potato, seq: u64, at: u64| {
            let e = catch(&mut proofs, (seq % 2) as u32, id, seq, at, false);
            g.event("h", &e, e.at, &cfg);
        };
        play(&mut g, 0, 1100);
        assert_eq!(view(&g).pace, None, "no lap is complete");
        assert!(view(&g).laps.is_empty());
        play(&mut g, 1, 1300);
        let v = view(&g);
        assert_eq!(v.hold_ms, 2000);
        assert_eq!(v.pace, Some(5.0), "one catch after the first, 200 ms later");
        // lap 1 at twice the speed: 2 catches in the 200 ms after lap 0's last
        play(&mut g, 2, 1400);
        play(&mut g, 3, 1500);
        let v = view(&g);
        assert_eq!((v.pace, v.best_pace), (Some(10.0), Some(10.0)));
        assert_eq!(
            v.laps
                .iter()
                .map(|l| (l.lap, l.pace, l.hold_ms))
                .collect::<Vec<_>>(),
            vec![(0, 5.0, 2000), (1, 10.0, 1800)]
        );
        // a slower lap is the pace now, and not the round's best
        play(&mut g, 4, 2000);
        play(&mut g, 5, 2500);
        let v = view(&g);
        assert_eq!((v.pace, v.best_pace), (Some(2.0), Some(10.0)));
        assert_eq!(
            g.round.as_ref().unwrap().catch_at.len(),
            1,
            "only the last lap's last catch is kept to measure the next from"
        );
    }

    /// A lap is credited with the hold its catchers report they kept, in microseconds. The
    /// arithmetic of players that report none (a tenth off in whole ms) says 2000 here.
    #[test]
    fn a_lap_is_credited_with_the_hold_its_catchers_reported() {
        let (mut g, cfg, mut proofs) = game(2);
        let id = g.round.as_ref().unwrap().id;
        for (seq, us) in [(0, 9_400), (1, 9_400), (2, 8_460)] {
            let mut e = catch(
                &mut proofs,
                (seq % 2) as u32,
                id,
                seq,
                1100 + seq * 100,
                false,
            );
            if let PotatoKind::Caught { hold_us, .. } = &mut e.kind {
                *hold_us = Some(us);
            }
            g.event("h", &e, e.at, &cfg);
        }
        let r = g.round.as_ref().unwrap();
        assert_eq!(r.best_lap_hold_ms, Some(9));
        assert_eq!(g.ladder[0].record_hold_ms, Some(9));
        let v = g.view(&cfg).unwrap().round.unwrap();
        assert_eq!(
            (v.hold_ms, v.turnaround_ms),
            (8, 8),
            "the latest catcher's hold"
        );
        assert_eq!(
            r.flight_window_ms(),
            8,
            "a thrower retries for its hold when shorter"
        );
        assert_eq!(r.deadline(cfg.report_slack_ms), Some(1300 + 8 + 100 + 3000));
    }

    /// A player built before `hold_us` reports none: its catch still parses, and the hold is
    /// the one those players compute.
    #[test]
    fn a_catch_without_a_reported_hold_is_read() {
        let e: PotatoEvent = serde_json::from_value(serde_json::json!({
            "at": 1, "player": 0, "kind": "caught", "round": 1, "seq": 0,
            "colour": 1, "counter": 1, "chain": "00"
        }))
        .unwrap();
        assert!(matches!(e.kind, PotatoKind::Caught { hold_us: None, .. }));
        assert!(!serde_json::to_string(&e).unwrap().contains("hold_us"));
        let (g, _, _) = game(2);
        let mut r = g.round.clone().unwrap();
        r.start_hold_ms = 400;
        assert_eq!(
            r.hold_at(2 * 400),
            9,
            "where whole-millisecond shrinking stops"
        );
        r.kept_us = Some(2_000);
        assert_eq!(r.hold_at(2 * 400), 2);
    }

    /// The host's clock can be behind the coordinator's: a catch on time by the coordinator's
    /// clock is not late because the host stamped it earlier.
    #[test]
    fn deadlines_run_on_the_coordinators_clock() {
        let (mut g, cfg, mut proofs) = game(2);
        let id = g.round.as_ref().unwrap().id;
        let mut e = catch(&mut proofs, 0, id, 0, 0, false);
        let received = 100_000;
        e.at = received - 60_000; // the host's clock, a minute behind
        g.event("h", &e, received, &cfg);
        let r = g.round.as_ref().unwrap();
        let due = received + r.hold_at(0) + r.flight_ms + cfg.report_slack_ms;
        g.tick(due, true, &cfg);
        assert!(g.round.is_some(), "on time by the coordinator's clock");
        assert!(g.last_drop.is_none());
    }

    /// A late report of an earlier catch is counted, but the latest catch (its holder and time,
    /// which the deadline and the page use) stays the newest one.
    #[test]
    fn a_late_report_does_not_move_the_ball_back() {
        let (mut g, cfg, mut proofs) = game(4);
        let id = g.round.as_ref().unwrap().id;
        let c0 = catch(&mut proofs, 0, id, 0, 1100, false);
        let c1 = catch(&mut proofs, 1, id, 1, 1200, false);
        let c2 = catch(&mut proofs, 2, id, 2, 1300, false);
        g.event("h", &c0, 1100, &cfg);
        g.event("h", &c2, 1300, &cfg);
        g.event("h", &c1, 1350, &cfg);
        let r = g.round.as_ref().unwrap();
        assert_eq!(
            (r.seq, r.holder, r.last_catch_at),
            (Some(2), Some(2), Some(1300))
        );
        assert_eq!(r.verified_through, Some(2));
    }

    /// The host leaving ends its round with what it achieved counted, and the ladder stays at
    /// that size for a host that comes back.
    #[test]
    fn a_host_that_leaves_keeps_its_rounds_statistics_and_its_size() {
        let (mut g, cfg, mut proofs) = game(2);
        let id = g.round.as_ref().unwrap().id;
        for (seq, p) in [(0, 0), (1, 1), (2, 0)] {
            let e = catch(&mut proofs, p, id, seq, 1100 + seq * 100, false);
            g.event("h", &e, e.at, &cfg);
        }
        let rung = g.rung;
        g.agent_gone("h", 2000, &cfg);
        assert_eq!(g.rung, rung, "the same size next time");
        let size2 = &g.ladder[rung];
        assert_eq!(size2.rounds, 1);
        assert_eq!(
            size2.broke.as_ref().map(|d| d.reason.as_str()),
            Some("host_gone")
        );
        assert_eq!(g.records.longest_round.as_ref().map(|r| r.value), Some(3.0));
        assert!(g.round.is_none() && g.host.is_none() && g.next_round_at.is_none());
    }

    /// A player can accept, catch and throw before the agent reports its wake done; the agent
    /// then sleeps it as soon as the wake is. The late `woke` must not show it awake.
    #[test]
    fn a_throw_before_the_wake_is_reported_keeps_the_player_sleeping() {
        let (mut g, cfg, mut proofs) = game(2);
        let id = g.round.as_ref().unwrap().id;
        let ev = |at, kind| PotatoEvent {
            at,
            player: 1,
            kind,
        };
        g.event("h", &ev(1050, PotatoKind::Waking {}), 1050, &cfg);
        let c = catch(&mut proofs, 1, id, 1, 1100, false);
        g.event("h", &c, c.at, &cfg);
        let threw = PotatoKind::Threw {
            round: id,
            seq: 2,
            to: 0,
            attempts: 1,
            wait_ms: 900.0,
            send_ms: 1.0,
        };
        g.event("h", &ev(2000, threw), 2000, &cfg);
        let woke = PotatoKind::Woke {
            wake_ms: 320.0,
            stages: BTreeMap::new(),
        };
        g.event("h", &ev(2010, woke), 2010, &cfg);
        let p = &g.players[&1];
        assert_eq!((p.phase.as_str(), p.since), ("sleeping", 2000));
        assert_eq!(p.last_wake_ms, Some(320.0));
    }
}
