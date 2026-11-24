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
        /// What the agent counted when it read this catch; none from an agent without a CPU
        /// allotment.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ring: Option<RingStats>,
        /// How long before the throw to it began the catcher was ready, in ms; negative when
        /// the thrower had to wait. None for a round's first catch, and when the catch was
        /// read before the catcher's wake had ended (its `woke` then carries it).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ready_margin_ms: Option<f64>,
        /// When the agent read the report, on its monotonic clock: microseconds since the
        /// agent started. `at` is its wall clock, which can step. None from an agent built
        /// before it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mono_us: Option<u64>,
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
    /// `catcher_phase_ms` how long it had been doing it. `catcher_cpu_wait_ms`: how long the
    /// catcher's wake has waited for a free CPU (it is still asleep), or waited before it
    /// started (it is waking or awake). `catcher_late_ms`: the catcher is awake by now, and
    /// was ready this long after the thrower's flight window ended (0: between the thrower's
    /// last try and that end).
    Dropped {
        round: u64,
        seq: u64,
        to: u32,
        attempts: u32,
        reason: String,
        catcher_phase: String,
        catcher_phase_ms: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        catcher_cpu_wait_ms: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        catcher_late_ms: Option<f64>,
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
    /// The agent started restoring the player. `cpu_wait_ms`: the wake waited this long for
    /// a free CPU first; none when it did not wait.
    Waking {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cpu_wait_ms: Option<f64>,
    },
    /// The player's ball port accepts connections again, `wake_ms` after the restore
    /// started. `stages` splits it (network, disk, load, guest, listen).
    /// `ready_margin_ms`: as a catch's, when the player's catch was read before this.
    Woke {
        wake_ms: f64,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        stages: BTreeMap<String, f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ready_margin_ms: Option<f64>,
    },
    /// The catcher of a dropped ball started accepting, `after_ms` after the ball dropped.
    Listening { round: u64, seq: u64, after_ms: f64 },
    /// A sleep, a wake or a first boot failed.
    Failed { stage: String, error: String },
}

/// The ring's CPUs as the agent counted them when it read a catch (its own ledger, not a
/// reading of the coordinator's).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RingStats {
    /// Machines not asleep: awake, waking, booting, or going to sleep until the save has
    /// finished and the process has exited.
    pub cpus_in_use: u32,
    /// The most in use at any instant since the catch before this one.
    pub cpus_peak: u32,
    /// Players of the ring that are asleep.
    pub asleep: u32,
    /// How many catches ahead of the ball a sleeping player's wake is asked for.
    pub lead: u32,
    /// The slowest wake in the agent's window, ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slowest_wake_ms: Option<f64>,
    /// Which machine holds each CPU in use.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slots: Vec<CpuSlot>,
    /// The CPU time the ring's cpuset has used so far, microseconds; none when the ring is
    /// not confined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_usec: Option<u64>,
}

/// A machine that holds a CPU: `phase` is `ball` (caught and not yet thrown), `ready`,
/// `waking`, `saving` or `booting`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuSlot {
    pub player: u32,
    pub phase: String,
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

/// One size of the ladder. `record_pace` is the fastest measured pace, in catches per second,
/// a round of this size held for one full lap; `broke` is what ended its latest round.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Rung {
    pub size: u32,
    #[serde(default)]
    pub record_pace: Option<f64>,
    #[serde(default)]
    pub record_pace_at: Option<u64>,
    /// The record `record_pace` replaced; none until a record of this size has been beaten.
    #[serde(default)]
    pub record_prev_pace: Option<f64>,
    #[serde(default)]
    pub broke: Option<Drop>,
    #[serde(default)]
    pub rounds: u64,
    /// The hold the ball carried when the pace outran a catcher (it was still waking, waiting
    /// for a CPU or saving), for each of this size's latest rounds that ended so, oldest
    /// first, at most [`DROPS_KEPT`]: where the next round of the size starts from. A drop
    /// with another cause (a failed wake, a proof, a host that left) says nothing of the pace
    /// and is not here.
    #[serde(default)]
    pub drop_holds: Vec<u64>,
}

/// The drops of a size its next start is computed from.
pub const DROPS_KEPT: usize = 5;

impl Rung {
    fn new(size: u32) -> Self {
        Rung {
            size,
            ..Rung::default()
        }
    }

    /// The median hold of the size's recent drops.
    fn edge_hold_ms(&self) -> Option<u64> {
        let mut holds = self.drop_holds.clone();
        holds.sort_unstable();
        let mid = holds.len().checked_sub(1)? / 2;
        Some(if holds.len() % 2 == 1 {
            holds[mid]
        } else {
            (holds[mid] + holds[mid + 1]).div_ceil(2)
        })
    }
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
    /// What was measured to be the cause: `waking` (the catcher's wake was not finished in
    /// time), `cpu_wait` (its wake waited for a free CPU, and that wait is more than half of
    /// how late it was, or the wake had not started at all), `saving` (it was still going to
    /// sleep from its last throw) or `proof`. None for anything else; `reason` says what
    /// ended the round.
    #[serde(default)]
    pub cause: Option<String>,
    /// The cause's measurement, ms: for `waking` how late the catcher was (none until the
    /// agent reports it), for `cpu_wait` how long the wake waited, for `saving` how long the
    /// save had run.
    #[serde(default)]
    pub cause_ms: Option<f64>,
    /// How long the catcher's wake waited for a free CPU, whatever the cause; none when it
    /// did not wait.
    #[serde(default)]
    pub cpu_wait_ms: Option<f64>,
    /// The pace of the last completed lap when the ball dropped.
    #[serde(default)]
    pub pace: Option<f64>,
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
    /// How long its latest wake waited for a free CPU, ms; none when it did not wait.
    #[serde(default)]
    pub cpu_wait_ms: Option<f64>,
    /// At its latest catch, how long before the throw to it began it was ready, ms.
    #[serde(default)]
    pub ready_margin_ms: Option<f64>,
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
    /// Each catch not yet behind a completed lap, by seq, and the last catch of the latest
    /// completed lap: when the host's agent read it and the hold its catcher kept. What a
    /// lap's pace is measured from, and bounded by.
    #[serde(default)]
    pub catch_stamps: BTreeMap<u64, CatchStamp>,
    /// The completed laps, oldest first, at most the last [`LAPS_KEPT`].
    #[serde(default)]
    pub laps: Vec<Lap>,
    /// The fastest pace of a completed lap.
    #[serde(default)]
    pub best_pace: Option<f64>,
    /// What the agent counted at the latest catch.
    #[serde(default)]
    pub ring_stats: Option<RingStats>,
    /// The most CPUs in use during each lap not yet completed.
    #[serde(default)]
    pub lap_cpus_peak: BTreeMap<u64, u32>,
    /// The host's time (ms) and the cpuset's CPU time (microseconds) at each catch not yet
    /// behind a completed lap, and at the last catch of the latest completed lap.
    #[serde(default)]
    pub cpu_marks: BTreeMap<u64, (u64, u64)>,
    /// A lap of this round has beaten its size's record.
    #[serde(default)]
    pub beat_record: bool,
}

/// When the host's agent read a catch, and the hold its catcher kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatchStamp {
    /// The agent's wall clock, ms (the event's `at`). It can step.
    pub at: u64,
    /// The agent's monotonic clock, microseconds since it started; none from an agent built
    /// before it sent one.
    #[serde(default)]
    pub mono_us: Option<u64>,
    /// The hold the catcher kept, microseconds.
    pub hold_us: u64,
}

/// The laps of a round the view keeps.
pub const LAPS_KEPT: usize = 60;

/// A lap timed by the agent's wall clock (an agent that sends no monotonic stamp) is accepted
/// when its stamps span at least this share of the holds its catchers kept, in tenths: that
/// clock can step, and a report read late shortens the interval after it. A lap timed by the
/// monotonic clock must span all of them.
const PACE_WALL_TENTHS: u64 = 9;
/// What a lap may fall short of its bound by: the stamps' granularity, microseconds.
const PACE_SLACK_US: u64 = 1000;

/// A completed lap: its measured pace, in catches per second, and the longest hold a catch
/// of it carried.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lap {
    pub lap: u64,
    /// None: the lap's stamps put it faster than the holds its catchers kept allow (reports
    /// read in a burst), so it has no pace, sets no record and is not to be drawn.
    pub pace: Option<f64>,
    pub hold_ms: u64,
    /// The most CPUs in use at any instant of the lap; none without an allotment.
    #[serde(default)]
    pub cpus_peak: Option<u32>,
    /// Host CPUs the ring's cpuset used, averaged over the lap; none unless confined.
    #[serde(default)]
    pub cpus_busy: Option<f64>,
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

    /// The pace of completed lap `lap`, in catches per second, from the times the host's agent
    /// read its catches: the lap's catches over the time from the catch before its first to
    /// its last. Lap 0 has no catch before it: its last minus its first, over one catch fewer.
    ///
    /// The host's clocks, not the coordinator's: the coordinator stamps a message when it
    /// applies it, and a connection that stalls delivers a lap's catches in one burst. When
    /// every catch of the lap carries the agent's monotonic stamp the lap is timed by that;
    /// otherwise (an agent built before it) by the agent's wall clock, which every agent has
    /// sent since the first.
    ///
    /// Bounded by the game itself: each catcher keeps the ball for its hold before the next
    /// can catch, so the lap cannot have taken less than the holds its catchers kept. On
    /// the monotonic clock a lap that spans less than all of them (less 1 ms for the stamps)
    /// has no pace: its first stamp was read late. On the wall clock, which can also step,
    /// nine tenths are accepted, as before the monotonic stamp existed.
    fn lap_pace(&self, lap: u64) -> Option<f64> {
        let n = self.ring.len() as u64;
        let last = (lap + 1) * n - 1;
        let first = lap * n;
        let from = first
            .checked_sub(1)
            .filter(|s| self.catch_stamps.contains_key(s))
            .unwrap_or(first);
        let stamps = (from..=last)
            .map(|s| self.catch_stamps.get(&s))
            .collect::<Option<Vec<_>>>()?;
        let (start, end) = (stamps.first()?, stamps.last()?);
        let catches = last - from;
        // every catcher from `from` to the last but one held the ball before the next caught
        let held_us: u64 = stamps[..stamps.len() - 1].iter().map(|s| s.hold_us).sum();
        let monotonic = stamps.iter().map(|s| s.mono_us).collect::<Option<Vec<_>>>();
        let wall_us = match monotonic {
            Some(_) => {
                let wall_us = end.mono_us?.checked_sub(start.mono_us?)?;
                if wall_us + PACE_SLACK_US < held_us {
                    return None;
                }
                wall_us
            }
            None => {
                let wall_us = end.at.checked_sub(start.at)? * 1000;
                if (wall_us + PACE_SLACK_US) * 10 < held_us * PACE_WALL_TENTHS {
                    return None;
                }
                wall_us
            }
        };
        (catches > 0 && wall_us > 0)
            .then(|| (catches as f64 * 100_000_000.0 / wall_us as f64).round() / 100.0)
    }

    /// Host CPUs the ring used over completed lap `lap`: the cpuset's CPU time between the
    /// catch before the lap's first (lap 0: its first) and its last, over the host's time
    /// between them.
    fn lap_busy(&self, lap: u64) -> Option<f64> {
        let n = self.ring.len() as u64;
        let (to_at, to_usec) = *self.cpu_marks.get(&((lap + 1) * n - 1))?;
        let (from_at, from_usec) = *(lap * n)
            .checked_sub(1)
            .and_then(|s| self.cpu_marks.get(&s))
            .or_else(|| self.cpu_marks.get(&(lap * n)))?;
        let wall_ms = to_at.checked_sub(from_at).filter(|w| *w > 0)?;
        let used = to_usec.checked_sub(from_usec)?;
        Some((used as f64 / (wall_ms as f64 * 10.0)).round() / 100.0)
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
    /// The ladder being played: one rung for each of `sizes`, and any earlier size of the
    /// same allotment, kept for its record.
    #[serde(default)]
    pub ladder: Vec<Rung>,
    /// The sizes the ladder is played over, smallest first, derived from the potato host's
    /// allotment and players when it says hello.
    #[serde(default)]
    pub sizes: Vec<u32>,
    /// The size being played, or the next one.
    #[serde(default)]
    pub next_size: Option<u32>,
    /// Where a coordinator before `next_size` kept its place: an index into its ladder. Read
    /// once, by `resumed`, and never written.
    #[serde(default, rename = "rung", skip_serializing)]
    pub old_rung: usize,
    /// The ladders played under other allotments, by allotment (0: an agent that reports
    /// none). A record is a pace on K CPUs, so a host with another K gets its own ladder
    /// and does not erase this one.
    #[serde(default)]
    pub shelved: BTreeMap<u32, Vec<Rung>>,
    /// The sizes that have played a round since the potato host connected.
    #[serde(default)]
    pub played: std::collections::BTreeSet<u32>,
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
    /// The CPU allotment of the host that played the ladder last, and whether its ring was
    /// confined to that many host CPUs. None from an agent that reports no allotment.
    #[serde(default)]
    pub cpus: Option<u32>,
    #[serde(default)]
    pub confined: Option<bool>,
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
    /// The ladder's sizes for a host. With an allotment of K CPUs: 2K, doubling while below
    /// the host's players, then the players themselves (K = 8 and 96 players: 16, 32, 64,
    /// 96). A size of K or fewer can have nothing asleep, so the doubling starts above it;
    /// the players are the top rung whatever their number. Without an allotment: the
    /// configured sizes the host can run.
    pub fn sizes_for(cpus: Option<u32>, players: u32, cfg: &PotatoConfig) -> Vec<u32> {
        match cpus {
            Some(k) => {
                let mut sizes = Vec::new();
                let mut size = 2 * u64::from(k);
                while size < u64::from(players) {
                    sizes.push(size as u32);
                    size *= 2;
                }
                sizes.push(players);
                sizes
            }
            None => cfg
                .sizes
                .iter()
                .copied()
                .filter(|s| *s <= players)
                .collect(),
        }
    }

    /// The size after `size` on the ladder; after the last, the first.
    fn size_after(&self, size: u32) -> Option<u32> {
        self.sizes
            .iter()
            .find(|s| **s > size)
            .or(self.sizes.first())
            .copied()
    }

    /// The hold a round of `size` starts at: near where the size's recent rounds dropped the
    /// ball. 1.5 times the median hold of those drops for its first round since the host
    /// connected, 1.25 times for its later ones, and the configured start while the size has
    /// dropped no ball.
    fn start_hold_ms(&self, size: u32, cfg: &PotatoConfig) -> u64 {
        let edge = self
            .ladder
            .iter()
            .find(|r| r.size == size)
            .and_then(Rung::edge_hold_ms);
        let Some(edge) = edge else {
            return cfg.start_hold_ms(size);
        };
        let (num, den) = if self.played.contains(&size) {
            (5, 4)
        } else {
            (3, 2)
        };
        // Never above the configured start. A ring with one player that cannot keep up drops
        // every ball at whatever hold it started with, and each such drop would otherwise
        // raise the next start by a quarter, without end. The cap also bounds whatever a
        // state file holds.
        edge.saturating_mul(num)
            .div_ceil(den)
            .max(cfg.min_hold_ms)
            .min(cfg.start_hold_ms(size))
    }

    /// The latest drop was the pace outrunning a catcher: the next round of its size starts
    /// near the hold it dropped at.
    fn edge_found(&mut self, cfg: &PotatoConfig) {
        let Some((size, hold)) = self.last_drop.as_ref().map(|d| (d.size, d.hold_ms)) else {
            return;
        };
        if let Some(rung) = self.ladder.iter_mut().find(|g| g.size == size) {
            rung.drop_holds.push(hold.min(cfg.start_hold_ms(size)));
            while rung.drop_holds.len() > DROPS_KEPT {
                rung.drop_holds.remove(0);
            }
        }
    }

    /// Record what was measured of the latest drop, in the drop and in its rung's copy of it.
    fn drop_measured(&mut self, set: impl Fn(&mut Drop)) {
        let Some(d) = self.last_drop.as_mut() else {
            return;
        };
        set(d);
        let round = d.round;
        if let Some(b) = self
            .ladder
            .iter_mut()
            .filter_map(|g| g.broke.as_mut())
            .find(|b| b.round == round)
        {
            set(b);
        }
    }

    /// The cause of a drop on a catcher whose wake had started: it was ready `late_ms` after
    /// the ball needed it, and its wake had waited `wait_ms` for a free CPU first. The wait is
    /// the cause when it is the larger part of the lateness (more than half of it): without
    /// it the catcher would have been less than half as late, or on time. Otherwise the wake
    /// itself is. With the lateness not yet reported it is the wake, for now.
    fn late_cause(wait_ms: Option<f64>, late_ms: Option<f64>) -> (&'static str, Option<f64>) {
        match (wait_ms, late_ms) {
            (Some(wait), Some(late)) if 2.0 * wait > late => ("cpu_wait", Some(wait)),
            (_, late) => ("waking", late),
        }
    }

    /// A restarted coordinator: no agent is connected yet, so no round, players or host.
    pub fn resumed(&mut self) {
        // a state file from before `next_size` names its place by index
        if self.next_size.is_none() {
            self.next_size = self.ladder.get(self.old_rung).map(|r| r.size);
        }
        self.old_rung = 0;
        self.played.clear();
        self.round = None;
        self.players.clear();
        self.host = None;
        self.capacity = 0;
        self.next_round_at = None;
    }

    /// An agent said hello. A host with players becomes the potato host (the first one
    /// that connects); its players are all new.
    pub fn agent_hello(
        &mut self,
        host: &str,
        players: u32,
        cpus: u32,
        confined: bool,
        at: u64,
        cfg: &PotatoConfig,
    ) {
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
        let cpus = (cpus > 0).then_some(cpus);
        if cpus != self.cpus {
            // another allotment: this ladder is put away, and that one's brought back
            let ladder = std::mem::take(&mut self.ladder);
            if !ladder.is_empty() {
                self.shelved.insert(self.cpus.unwrap_or(0), ladder);
            }
            self.ladder = self.shelved.remove(&cpus.unwrap_or(0)).unwrap_or_default();
        }
        self.cpus = cpus;
        self.confined = self.cpus.map(|_| confined);
        self.sizes = Self::sizes_for(cpus, players, cfg);
        for size in &self.sizes {
            if !self.ladder.iter().any(|r| r.size == *size) {
                self.ladder.push(Rung::new(*size));
            }
        }
        self.ladder.sort_by_key(|r| r.size);
        self.played.clear();
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
            let size = r.size;
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
            self.next_size = Some(size);
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
        // the size the ladder is at, or the next one this host runs
        let at_or_after = self
            .next_size
            .and_then(|next| self.sizes.iter().find(|s| **s >= next));
        let Some(size) = at_or_after.or(self.sizes.first()).copied() else {
            return;
        };
        self.next_size = Some(size);
        let start_hold_ms = self.start_hold_ms(size, cfg);
        self.played.insert(size);
        self.round_count += 1;
        self.round = Some(Round {
            id: self.round_count,
            size,
            ring: (0..size).collect(),
            started_at: at,
            start_hold_ms,
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
            catch_stamps: BTreeMap::new(),
            laps: Vec::new(),
            best_pace: None,
            ring_stats: None,
            lap_cpus_peak: BTreeMap::new(),
            cpu_marks: BTreeMap::new(),
            beat_record: false,
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
            cause: (reason == "proof").then(|| "proof".into()),
            cause_ms: None,
            cpu_wait_ms: None,
            pace: r.laps.last().and_then(|l| l.pace),
            sleep_stages,
            wake_stages,
            detail,
        };
        if let Some(rung) = self.ladder.iter_mut().find(|g| g.size == r.size) {
            rung.rounds += 1;
            rung.broke = Some(d.clone());
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
        // up the ladder to the next size; after the last, back to the first
        self.next_size = self.size_after(r.size);
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
                                cpu_wait_ms: None,
                                ready_margin_ms: None,
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
                ring,
                ready_margin_ms,
                mono_us,
            } => {
                let Some(p) = self.players.get_mut(&e.player) else {
                    return;
                };
                p.ready_margin_ms = *ready_margin_ms;
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
                // the host's stamps: both ends of every interval are its own clock's
                r.catch_stamps.insert(
                    *seq,
                    CatchStamp {
                        at: e.at,
                        mono_us: *mono_us,
                        hold_us: hold_us.unwrap_or(hold * 1000),
                    },
                );
                if let Some(stats) = ring {
                    let peak = r.lap_cpus_peak.entry(lap).or_insert(0);
                    *peak = (*peak).max(stats.cpus_peak).max(stats.cpus_in_use);
                    if let Some(usec) = stats.cpu_usec {
                        // the host's clock: both ends of the interval are its own
                        r.cpu_marks.insert(*seq, (e.at, usec));
                    }
                }
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
                    // a lap without a pace has no time to average its CPU use over either
                    let cpus_busy = pace.and_then(|_| r.lap_busy(l));
                    let cpus_peak = r.lap_cpus_peak.remove(&l);
                    // the lap's last catch stays: the next lap is measured from it
                    r.catch_stamps.retain(|s, _| *s + 1 >= (l + 1) * n);
                    r.cpu_marks.retain(|s, _| *s + 1 >= (l + 1) * n);
                    if let Some(pace) = pace {
                        if r.best_pace.is_none_or(|b| pace > b) {
                            r.best_pace = Some(pace);
                        }
                    }
                    r.laps.push(Lap {
                        lap: l,
                        pace,
                        hold_ms: h,
                        cpus_peak,
                        cpus_busy,
                    });
                    if r.laps.len() > LAPS_KEPT {
                        let cut = r.laps.len() - LAPS_KEPT;
                        r.laps.drain(..cut);
                    }
                    // a size's record is acknowledged as soon as the lap is: it stays in the
                    // state file even if the coordinator restarts before the ball drops
                    let size = r.size;
                    let rung = self.ladder.iter_mut().find(|g| g.size == size);
                    if let (Some(pace), Some(rung)) = (pace, rung) {
                        if rung.record_pace.is_none_or(|old| pace > old) {
                            // what this round beat is the record that stood when it began,
                            // not its own earlier laps
                            if !r.beat_record {
                                rung.record_prev_pace = rung.record_pace;
                                r.beat_record = true;
                            }
                            rung.record_pace = Some(pace);
                            rung.record_pace_at = Some(at);
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
                    r.ring_stats = ring.clone();
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
                catcher_cpu_wait_ms,
                catcher_late_ms,
            } => {
                if self.round.as_ref().is_some_and(|r| r.id == *round) {
                    let window = self.round.as_ref().map_or(0, |r| r.flight_window_ms());
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
                    // the cause, from what the agent measured of the catcher
                    let wait = *catcher_cpu_wait_ms;
                    let cause = match catcher_phase.as_str() {
                        // its wake had not started: it was still waiting for a CPU
                        "asleep" => wait.map(|w| ("cpu_wait", Some(w))),
                        // How late it was arrives with the agent's `listening`. Until then: a
                        // wake that waited for a CPU longer than it has run so far was held
                        // up by the wait.
                        "waking" => Some(match wait {
                            Some(w) if w > *catcher_phase_ms => ("cpu_wait", Some(w)),
                            _ => Self::late_cause(wait, None),
                        }),
                        // It woke while the thrower was still trying, or just after: the drop
                        // was read with its wake already reported. It was late all the same.
                        "awake" if *catcher_phase_ms < window as f64 => {
                            Some(Self::late_cause(wait, *catcher_late_ms))
                        }
                        "sleeping" => Some(("saving", Some(*catcher_phase_ms))),
                        _ => None,
                    };
                    self.drop_measured(|d| {
                        d.cpu_wait_ms = wait;
                        if let Some((cause, cause_ms)) = cause {
                            d.cause = Some(cause.into());
                            d.cause_ms = cause_ms;
                        }
                    });
                    if cause.is_some() {
                        self.edge_found(cfg);
                    }
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
                    // its lateness is known now: the wake's wait for a CPU, or the wake itself
                    if d.stage == "waking" {
                        let (cause, cause_ms) = Self::late_cause(d.cpu_wait_ms, Some(*after_ms));
                        self.drop_measured(|d| {
                            d.cause = Some(cause.into());
                            d.cause_ms = cause_ms;
                        });
                    }
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
            PotatoKind::Waking { cpu_wait_ms } => {
                if let Some(p) = self.players.get_mut(&e.player) {
                    p.phase = "waking".into();
                    p.since = at;
                    p.cpu_wait_ms = *cpu_wait_ms;
                }
            }
            PotatoKind::Woke {
                wake_ms,
                stages,
                ready_margin_ms,
            } => {
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
                    if ready_margin_ms.is_some() {
                        p.ready_margin_ms = *ready_margin_ms;
                    }
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
    pub ladder: Vec<RungView>,
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
    /// K, the CPU allotment the ladder shown was played under, and whether the ring's
    /// machines were confined to that many host CPUs (a cpuset) or only counted. Null from
    /// an agent that reports no allotment.
    pub cpus: Option<u32>,
    pub confined: Option<bool>,
    /// The host running the players (its agent's name, as a tile's `host`); null while none is
    /// connected. The viewer-path smoke attributes its potato probe to it for the image
    /// release gate (fleet/coord/stage-smoke.py).
    pub host: Option<String>,
}

/// A size of the ladder as the page gets it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RungView {
    pub size: u32,
    /// The fastest measured pace, in catches per second, a round of this size held for one
    /// full lap on the ladder's `cpus`; null until one completes a lap.
    pub record_pace: Option<f64>,
    pub record_pace_at: Option<u64>,
    /// The record that stood before the round that set `record_pace`; null until a record
    /// of this size has been beaten.
    pub record_prev_pace: Option<f64>,
    pub broke: Option<Drop>,
    pub rounds: u64,
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
    /// Catches per second over the last completed lap, measured from the times the host's
    /// agent read the catches; null before a lap completes, and for a lap its catchers'
    /// holds do not allow.
    pub pace: Option<f64>,
    /// The fastest pace of a completed lap of this round.
    pub best_pace: Option<f64>,
    /// The completed laps, oldest first, at most the last [`LAPS_KEPT`].
    pub laps: Vec<Lap>,
    /// The agent's count at the latest catch: machines not asleep, the ring's players
    /// asleep, the lead in catches, the slowest wake in its window, and which machine holds
    /// each CPU. Null before a catch and from an agent without an allotment.
    pub cpus_in_use: Option<u32>,
    pub asleep: Option<u32>,
    pub lead: Option<u32>,
    pub slowest_wake_ms: Option<f64>,
    pub cpu_slots: Option<Vec<CpuSlot>>,
    /// Over the last completed lap: the most CPUs in use at any instant, and the host CPUs
    /// the ring's cpuset used on average (null unless confined).
    pub cpus_peak: Option<u32>,
    pub cpus_busy: Option<f64>,
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
    pub cpu_wait_ms: Option<f64>,
    pub ready_margin_ms: Option<f64>,
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
                pace: r.laps.last().and_then(|l| l.pace),
                best_pace: r.best_pace,
                laps: r.laps.clone(),
                cpus_in_use: r.ring_stats.as_ref().map(|s| s.cpus_in_use),
                asleep: r.ring_stats.as_ref().map(|s| s.asleep),
                lead: r.ring_stats.as_ref().map(|s| s.lead),
                slowest_wake_ms: r.ring_stats.as_ref().and_then(|s| s.slowest_wake_ms),
                cpu_slots: r.ring_stats.as_ref().map(|s| s.slots.clone()),
                cpus_peak: r.laps.last().and_then(|l| l.cpus_peak),
                cpus_busy: r.laps.last().and_then(|l| l.cpus_busy),
            }
        });
        Some(PotatoView {
            // the sizes being played; every rung when no host has said which (a state file
            // from before `sizes`)
            ladder: self
                .ladder
                .iter()
                .filter(|g| self.sizes.is_empty() || self.sizes.contains(&g.size))
                .map(|g| RungView {
                    size: g.size,
                    record_pace: g.record_pace,
                    record_pace_at: g.record_pace_at,
                    record_prev_pace: g.record_prev_pace,
                    broke: g.broke.clone(),
                    rounds: g.rounds,
                })
                .collect(),
            size: self
                .round
                .as_ref()
                .map(|r| r.size)
                .or(self.next_size)
                .or_else(|| self.sizes.first().copied()),
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
                    cpu_wait_ms: p.cpu_wait_ms,
                    ready_margin_ms: p.ready_margin_ms,
                })
                .collect(),
            records: self.records.clone(),
            last_drop: self.last_drop.clone(),
            next_round_at: self.next_round_at,
            colours: self.round.as_ref().map_or(cfg.colours, |r| r.colours),
            capacity: self.capacity,
            cpus: self.cpus,
            confined: self.confined,
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
                ring: None,
                ready_margin_ms: None,
                mono_us: None,
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
        assert_eq!(w.kind, PotatoKind::Waking { cpu_wait_ms: None });
    }

    #[test]
    fn a_catch_not_reported_by_its_deadline_drops_the_ball() {
        let cfg = PotatoConfig::default();
        let mut g = Potato::default();
        g.agent_hello("h", 2, 0, false, 0, &cfg);
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
                    ring: None,
                    ready_margin_ms: None,
                    mono_us: None,
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
        g.agent_hello("h", n, 0, false, 0, &cfg);
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
                ring: None,
                ready_margin_ms: None,
                mono_us: None,
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
            g.ladder.iter().find(|r| r.size == 2).unwrap().record_pace,
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

    #[test]
    fn a_completed_lap_is_a_record_before_the_ball_drops() {
        let (mut g, cfg, mut proofs) = game(2);
        let id = g.round.as_ref().unwrap().id;
        for (seq, p) in [(0, 0), (1, 1), (2, 0)] {
            let e = held(catch(&mut proofs, p, id, seq, 1100 + seq * 100, false), 100);
            g.event("h", &e, e.at, &cfg);
        }
        assert_eq!(
            g.ladder[0].record_pace,
            Some(10.0),
            "one catch 100 ms after the first, at a 100 ms hold"
        );
        g.resumed();
        assert_eq!(g.ladder[0].record_pace, Some(10.0));
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

    /// The pace is what the host's stamps of the catches say, not 1000 / hold and not the
    /// times the coordinator applied them: a connection that stalls delivers a lap in a burst.
    #[test]
    fn pace_is_measured_from_the_catch_times() {
        let (mut g, cfg, mut proofs) = game(2);
        let id = g.round.as_ref().unwrap().id;
        let view = |g: &Potato| g.view(&cfg).unwrap().round.unwrap();
        // every catch reaches the coordinator in one burst, a millisecond apart
        let mut applied = 5000;
        let mut play = |g: &mut Potato, seq: u64, host_at: u64, hold_ms: u64| {
            let e = held(
                catch(&mut proofs, (seq % 2) as u32, id, seq, host_at, false),
                hold_ms,
            );
            applied += 1;
            g.event("h", &e, applied, &cfg);
        };
        play(&mut g, 0, 1100, 200);
        assert_eq!(view(&g).pace, None, "no lap is complete");
        assert!(view(&g).laps.is_empty());
        play(&mut g, 1, 1300, 100);
        assert_eq!(
            view(&g).pace,
            Some(5.0),
            "one catch after the first, 200 ms later"
        );
        // lap 1 at twice the speed: 2 catches in the 200 ms after lap 0's last
        play(&mut g, 2, 1400, 100);
        play(&mut g, 3, 1500, 500);
        let v = view(&g);
        assert_eq!((v.pace, v.best_pace), (Some(10.0), Some(10.0)));
        assert_eq!(
            v.laps
                .iter()
                .map(|l| (l.lap, l.pace, l.hold_ms))
                .collect::<Vec<_>>(),
            vec![(0, Some(5.0), 200), (1, Some(10.0), 500)],
            "with the longest hold a catch of the lap carried"
        );
        // a slower lap is the pace now, and not the round's best
        play(&mut g, 4, 2000, 500);
        play(&mut g, 5, 2500, 500);
        let v = view(&g);
        assert_eq!((v.pace, v.best_pace), (Some(2.0), Some(10.0)));
        assert_eq!(g.ladder[0].record_pace, Some(10.0));
        assert_eq!(
            g.round.as_ref().unwrap().catch_stamps.len(),
            1,
            "only the last lap's last catch is kept to measure the next from"
        );
    }

    /// The catch `e`, read by the agent `ms` after it started, by its monotonic clock.
    fn mono(mut e: PotatoEvent, ms: u64) -> PotatoEvent {
        if let PotatoKind::Caught { mono_us, .. } = &mut e.kind {
            *mono_us = Some(ms * 1000);
        }
        e
    }

    /// With the agent's monotonic stamps a lap must span all of the holds its catchers kept.
    /// A lap whose first stamp was read late by a tenth of it used to pass the wall clock's
    /// nine tenths and set a record 11% above what the holds allow.
    #[test]
    fn a_lap_read_a_tenth_late_at_its_start_sets_no_record() {
        let (mut g, cfg, mut proofs) = ladder_game(4, 8);
        let id = g.round.as_ref().unwrap().id;
        let mut play = |g: &mut Potato, seq: u64, at: u64| {
            let e = held(
                catch(&mut proofs, (seq % 4) as u32, id, seq, at, false),
                100,
            );
            let e = mono(e, at - 1000);
            g.event("h", &e, at, &cfg);
        };
        let rung = |g: &Potato| g.view(&cfg).unwrap().ladder[0].record_pace;
        // lap 0 as played: a catch every 100 ms
        for (seq, at) in [(0, 1100), (1, 1200), (2, 1300)] {
            play(&mut g, seq, at);
        }
        // its last catch happened at 1400 and is read 40 ms late
        play(&mut g, 3, 1440);
        assert_eq!(
            rung(&g),
            Some(8.82),
            "3 catches in 340 ms: slower, and true to the stamps"
        );
        // lap 1 is measured from that late stamp: 360 ms for 400 ms of holds
        for (seq, at) in [(4, 1500), (5, 1600), (6, 1700), (7, 1800)] {
            play(&mut g, seq, at);
        }
        let v = g.view(&cfg).unwrap();
        let r = v.round.as_ref().unwrap();
        assert_eq!(
            r.laps[1].pace, None,
            "not 11.11 catches a second at 100 ms holds"
        );
        assert_eq!((r.pace, r.best_pace), (None, Some(8.82)));
        assert_eq!(rung(&g), Some(8.82));
        // a lap that spans its holds, to the millisecond, is timed
        for (seq, at) in [(8, 1900), (9, 2000), (10, 2100), (11, 2200)] {
            play(&mut g, seq, at);
        }
        assert_eq!(g.view(&cfg).unwrap().round.unwrap().pace, Some(10.0));
        assert_eq!(rung(&g), Some(10.0));
    }

    /// The agent's wall clock steps back in the middle of a lap (NTP). The monotonic stamps
    /// are right, and the lap has its true pace; by the wall clock it would have none, or a
    /// wrong one.
    #[test]
    fn a_backward_step_of_the_wall_clock_leaves_the_pace_true() {
        let (mut g, cfg, mut proofs) = ladder_game(4, 8);
        let id = g.round.as_ref().unwrap().id;
        // a catch every 100 ms by the monotonic clock; the wall clock steps back 5 s after
        // the second, and forward 250 ms before the last of lap 1
        let wall = [
            1100, 1200, 300, 400, // lap 0: its last stamp is before its first
            500, 600, 700, 1050, // lap 1: 650 ms by the wall clock, 400 ms in truth
        ];
        for (seq, wall_at) in wall.into_iter().enumerate() {
            let seq = seq as u64;
            let e = held(
                catch(&mut proofs, (seq % 4) as u32, id, seq, wall_at, false),
                100,
            );
            let e = mono(e, 100 + seq * 100);
            g.event("h", &e, 9000 + seq, &cfg);
        }
        let r = g.view(&cfg).unwrap().round.unwrap();
        assert_eq!(
            r.laps.iter().map(|l| l.pace).collect::<Vec<_>>(),
            vec![Some(10.0), Some(10.0)],
            "3 catches in 300 ms, then 4 in 400 ms"
        );
    }

    /// An agent that sends no monotonic stamp is timed by its wall clock under the rule from
    /// before the stamp existed, and a lap with a catch that has none falls back to it.
    #[test]
    fn a_lap_without_monotonic_stamps_keeps_the_wall_clocks_rule() {
        let (mut g, cfg, mut proofs) = ladder_game(4, 8);
        let id = g.round.as_ref().unwrap().id;
        for (seq, at) in [(0u64, 1100), (1, 1200), (2, 1300), (3, 1440)] {
            let e = held(
                catch(&mut proofs, (seq % 4) as u32, id, seq, at, false),
                100,
            );
            g.event("h", &e, at, &cfg);
        }
        // 360 ms for 400 ms of holds: within the wall clock's nine tenths
        for (seq, at) in [(4u64, 1500), (5, 1600), (6, 1700), (7, 1800)] {
            let e = held(
                catch(&mut proofs, (seq % 4) as u32, id, seq, at, false),
                100,
            );
            // one catch of the lap with a monotonic stamp does not make the lap monotonic
            let e = if seq == 6 { mono(e, 5) } else { e };
            g.event("h", &e, at, &cfg);
        }
        let r = g.view(&cfg).unwrap().round.unwrap();
        assert_eq!(r.laps[1].pace, Some(11.11));
    }

    /// A lap cannot be faster than the holds its catchers kept. One whose stamps say it was
    /// (the agent read the reports late, in a burst) has no pace: it sets no record, is not
    /// the round's best, and is listed without a pace so the page does not draw it.
    #[test]
    fn a_lap_the_holds_do_not_allow_has_no_pace_and_sets_no_record() {
        let (mut g, cfg, mut proofs) = ladder_game(4, 8);
        let id = g.round.as_ref().unwrap().id;
        let mut play = |g: &mut Potato, seq: u64, host_at: u64| {
            let e = held(
                catch(&mut proofs, (seq % 4) as u32, id, seq, host_at, false),
                100,
            );
            g.event("h", &e, host_at, &cfg);
        };
        // lap 0, read in a burst: four catches at 100 ms holds stamped within 2 ms
        for (seq, at) in [(0, 1100), (1, 1101), (2, 1101), (3, 1102)] {
            play(&mut g, seq, at);
        }
        let v = g.view(&cfg).unwrap();
        let r = v.round.as_ref().unwrap();
        assert_eq!(
            (r.pace, r.best_pace),
            (None, None),
            "not 1500 catches a second"
        );
        assert_eq!(r.laps.len(), 1);
        assert_eq!((r.laps[0].pace, r.laps[0].hold_ms), (None, 100));
        assert_eq!(v.ladder[0].record_pace, None);
        // lap 1 as it was played: a catch every 100 ms after the hold of the one before
        for (seq, at) in [(4, 1500), (5, 1600), (6, 1700)] {
            play(&mut g, seq, at);
        }
        // its last catch is read late, 350 ms after it happened: a slower lap, and plausible
        play(&mut g, 7, 2150);
        let v = g.view(&cfg).unwrap();
        assert_eq!(
            v.round.as_ref().unwrap().pace,
            Some(3.82),
            "4 catches in 1048 ms"
        );
        assert_eq!(v.ladder[0].record_pace, Some(3.82));
        // lap 2 is measured from that late stamp: its catches, played at 1900 to 2200, were
        // read at 2151 to 2200. Four catches in 50 ms at 100 ms holds did not happen.
        for (seq, at) in [(8, 2151), (9, 2152), (10, 2153), (11, 2200)] {
            play(&mut g, seq, at);
        }
        let v = g.view(&cfg).unwrap();
        let r = v.round.as_ref().unwrap();
        assert_eq!((r.pace, r.best_pace), (None, Some(3.82)));
        assert_eq!(
            r.laps.iter().map(|l| l.pace).collect::<Vec<_>>(),
            vec![None, Some(3.82), None]
        );
        assert_eq!(
            v.ladder[0].record_pace,
            Some(3.82),
            "80 catches a second is no record"
        );
        // a lap a little under its holds is the stamps' jitter, and is timed
        for (seq, at) in [(12, 2300), (13, 2400), (14, 2500), (15, 2580)] {
            play(&mut g, seq, at);
        }
        assert_eq!(
            g.view(&cfg).unwrap().round.unwrap().pace,
            Some(10.53),
            "4 catches in 380 ms, at least nine tenths of the 400 ms held"
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
        g.agent_gone("h", 2000, &cfg);
        assert_eq!(g.next_size, Some(2), "the same size next time");
        let size2 = &g.ladder[0];
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
        g.event(
            "h",
            &ev(1050, PotatoKind::Waking { cpu_wait_ms: None }),
            1050,
            &cfg,
        );
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
            ready_margin_ms: None,
        };
        g.event("h", &ev(2010, woke), 2010, &cfg);
        let p = &g.players[&1];
        assert_eq!((p.phase.as_str(), p.since), ("sleeping", 2000));
        assert_eq!(p.last_wake_ms, Some(320.0));
    }

    /// A game on a host whose agent reports a CPU allotment.
    fn game_on(n: u32, cpus: u32, confined: bool) -> (Potato, PotatoConfig, Vec<PotatoProof>) {
        let (mut g, cfg, proofs) = game(n);
        g.cpus = (cpus > 0).then_some(cpus);
        g.confined = g.cpus.map(|_| confined);
        (g, cfg, proofs)
    }

    fn stats(in_use: u32, peak: u32, cpu_usec: Option<u64>) -> RingStats {
        RingStats {
            cpus_in_use: in_use,
            cpus_peak: peak,
            asleep: 5,
            lead: 2,
            slowest_wake_ms: Some(310.5),
            slots: vec![
                CpuSlot {
                    player: 1,
                    phase: "ball".into(),
                },
                CpuSlot {
                    player: 0,
                    phase: "saving".into(),
                },
            ],
            cpu_usec,
        }
    }

    /// A catch with the agent's count, read by the host at `host_at`.
    #[allow(clippy::too_many_arguments)]
    fn counted(
        proofs: &mut [PotatoProof],
        id: u64,
        seq: u64,
        at: u64,
        host_at: u64,
        ring: RingStats,
    ) -> PotatoEvent {
        let mut e = held(catch(proofs, (seq % 2) as u32, id, seq, at, false), 100);
        e.at = host_at;
        if let PotatoKind::Caught { ring: r, .. } = &mut e.kind {
            *r = Some(ring);
        }
        e
    }

    /// The hello's allotment is published with the ladder, and stays when the host leaves.
    #[test]
    fn the_allotment_comes_from_the_hello() {
        let cfg = PotatoConfig::default();
        let mut g = Potato::default();
        g.agent_hello("h", 4, 8, true, 0, &cfg);
        let v = g.view(&cfg).unwrap();
        assert_eq!((v.cpus, v.confined), (Some(8), Some(true)));
        g.agent_gone("h", 10, &cfg);
        let v = g.view(&cfg).unwrap();
        assert_eq!((v.cpus, v.confined, v.capacity), (Some(8), Some(true), 0));
        // an agent built before the allotment reports none: nothing is claimed for it
        g.agent_hello("old", 4, 0, false, 20, &cfg);
        let v = g.view(&cfg).unwrap();
        assert_eq!((v.cpus, v.confined), (None, None));
        g.agent_gone("old", 30, &cfg);
        g.agent_hello("h", 4, 8, false, 40, &cfg);
        let v = g.view(&cfg).unwrap();
        assert_eq!(
            (v.cpus, v.confined),
            (Some(8), Some(false)),
            "counted, not confined"
        );
    }

    /// The round's CPU figures are the agent's count at the latest catch; a lap's peak is the
    /// most any of its catches reported, and its CPUs busy the cpuset's CPU time over the
    /// host's time between the catch before the lap and its last.
    #[test]
    fn the_rings_cpus_are_the_agents_count() {
        let (mut g, cfg, mut proofs) = game_on(2, 8, true);
        let id = g.round.as_ref().unwrap().id;
        let view = |g: &Potato| g.view(&cfg).unwrap().round.unwrap();
        let v = view(&g);
        assert_eq!(
            (v.cpus_in_use, v.cpu_slots.clone()),
            (None, None),
            "no catch yet"
        );
        // the coordinator reads each catch 5 s after the host did: only the host's times count
        let e = counted(&mut proofs, id, 0, 6100, 1100, stats(3, 4, Some(1_000_000)));
        g.event("h", &e, 6100, &cfg);
        let v = view(&g);
        assert_eq!(
            (v.cpus_in_use, v.asleep, v.lead),
            (Some(3), Some(5), Some(2))
        );
        assert_eq!(v.slowest_wake_ms, Some(310.5));
        assert_eq!(v.cpu_slots.as_ref().map(Vec::len), Some(2));
        assert_eq!(
            (v.cpus_peak, v.cpus_busy),
            (None, None),
            "no lap is complete"
        );
        // read late by the coordinator: the lap took 200 ms on the host, whose CPU time it is
        let e = counted(&mut proofs, id, 1, 6900, 1300, stats(2, 7, Some(1_300_000)));
        g.event("h", &e, 6900, &cfg);
        let v = view(&g);
        assert_eq!(
            v.cpus_peak,
            Some(7),
            "the most any catch of the lap reported"
        );
        assert_eq!(v.cpus_busy, Some(1.5), "300 ms of CPU in 200 ms");
        assert_eq!(v.cpus_in_use, Some(2));
        // lap 1 is measured from lap 0's last catch
        let e = counted(&mut proofs, id, 2, 7000, 1400, stats(5, 5, Some(1_400_000)));
        g.event("h", &e, 7000, &cfg);
        let e = counted(&mut proofs, id, 3, 7100, 1500, stats(4, 6, Some(1_700_000)));
        g.event("h", &e, 7100, &cfg);
        let v = view(&g);
        assert_eq!((v.cpus_peak, v.cpus_busy), (Some(6), Some(2.0)));
        assert_eq!(
            v.laps
                .iter()
                .map(|l| (l.lap, l.cpus_peak, l.cpus_busy))
                .collect::<Vec<_>>(),
            vec![(0, Some(7), Some(1.5)), (1, Some(6), Some(2.0))]
        );
        assert_eq!(g.round.as_ref().unwrap().cpu_marks.len(), 1);
        assert!(g.round.as_ref().unwrap().lap_cpus_peak.is_empty());
    }

    /// Counted but not confined: no CPU time is reported, so no CPUs busy are claimed.
    #[test]
    fn a_ring_that_is_not_confined_claims_no_cpus_busy() {
        let (mut g, cfg, mut proofs) = game_on(2, 8, false);
        let id = g.round.as_ref().unwrap().id;
        for seq in 0..2 {
            let e = counted(
                &mut proofs,
                id,
                seq,
                1100 + seq * 100,
                1100 + seq * 100,
                stats(3, 3, None),
            );
            g.event("h", &e, e.at, &cfg);
        }
        let v = g.view(&cfg).unwrap().round.unwrap();
        assert_eq!((v.cpus_peak, v.cpus_busy), (Some(3), None));
        assert_eq!(v.pace, Some(10.0));
    }

    /// An agent without an allotment sends none of it: every figure it would carry is null,
    /// and the game plays as before.
    #[test]
    fn an_agent_without_an_allotment_publishes_no_cpu_figures() {
        let (mut g, cfg, mut proofs) = game(2);
        let id = g.round.as_ref().unwrap().id;
        for seq in 0..2 {
            let e = held(
                catch(&mut proofs, seq as u32, id, seq, 1100 + seq * 100, false),
                100,
            );
            g.event("h", &e, e.at, &cfg);
        }
        let v = serde_json::to_value(g.view(&cfg).unwrap()).unwrap();
        for null in [
            "/cpus",
            "/confined",
            "/round/cpus_in_use",
            "/round/cpus_peak",
            "/round/cpus_busy",
            "/round/asleep",
            "/round/lead",
            "/round/slowest_wake_ms",
            "/round/cpu_slots",
            "/round/laps/0/cpus_peak",
            "/players/0/cpu_wait_ms",
            "/players/0/ready_margin_ms",
        ] {
            assert_eq!(v.pointer(null), Some(&serde_json::Value::Null), "{null}");
        }
        assert_eq!(v.pointer("/round/pace"), Some(&serde_json::json!(10.0)));
        // and its messages, which carry none of the new fields, are read
        for old in [
            serde_json::json!({"at": 1, "player": 0, "kind": "waking"}),
            serde_json::json!({"at": 1, "player": 0, "kind": "woke", "wake_ms": 300.0}),
            serde_json::json!({"at": 1, "player": 0, "kind": "dropped", "round": 1, "seq": 1, "to": 1,
                "attempts": 2, "reason": "refused", "catcher_phase": "waking", "catcher_phase_ms": 90.0}),
        ] {
            let e: PotatoEvent = serde_json::from_value(old.clone()).unwrap();
            assert_eq!(
                serde_json::to_value(&e).unwrap(),
                old,
                "nothing is added to it"
            );
        }
    }

    /// A player's wait for a CPU is its latest wake's, and its margin its latest catch's.
    #[test]
    fn a_wakes_wait_and_a_catchs_margin_are_the_players() {
        let (mut g, cfg, mut proofs) = game_on(2, 8, true);
        let id = g.round.as_ref().unwrap().id;
        let player = |g: &Potato, n: usize| g.view(&cfg).unwrap().players[n].clone();
        let ev = |player, kind| PotatoEvent {
            at: 1,
            player,
            kind,
        };
        g.event(
            "h",
            &ev(
                1,
                PotatoKind::Waking {
                    cpu_wait_ms: Some(42.5),
                },
            ),
            1050,
            &cfg,
        );
        assert_eq!(player(&g, 1).cpu_wait_ms, Some(42.5));
        let mut e = catch(&mut proofs, 0, id, 0, 1100, false);
        g.event("h", &e, 1100, &cfg);
        assert_eq!(player(&g, 0).ready_margin_ms, None, "a round's first catch");
        e = catch(&mut proofs, 1, id, 1, 1200, false);
        if let PotatoKind::Caught {
            ready_margin_ms, ..
        } = &mut e.kind
        {
            *ready_margin_ms = Some(212.0);
        }
        g.event("h", &e, 1200, &cfg);
        assert_eq!(player(&g, 1).ready_margin_ms, Some(212.0));
        // a catch read before its catcher's wake ended: the wake carries the margin
        e = catch(&mut proofs, 0, id, 2, 1300, false);
        g.event("h", &e, 1300, &cfg);
        assert_eq!(player(&g, 0).ready_margin_ms, None);
        let woke = |margin| PotatoKind::Woke {
            wake_ms: 300.0,
            stages: BTreeMap::new(),
            ready_margin_ms: margin,
        };
        g.event("h", &ev(0, woke(Some(-12.0))), 1310, &cfg);
        assert_eq!(player(&g, 0).ready_margin_ms, Some(-12.0));
        g.event("h", &ev(1, woke(None)), 1320, &cfg);
        assert_eq!(
            player(&g, 1).ready_margin_ms,
            Some(212.0),
            "a wake without one changes nothing"
        );
        // its next wake did not wait
        g.event(
            "h",
            &ev(1, PotatoKind::Waking { cpu_wait_ms: None }),
            1400,
            &cfg,
        );
        assert_eq!(player(&g, 1).cpu_wait_ms, None);
    }

    #[test]
    fn the_rungs_come_from_the_allotment_and_the_players() {
        let cfg = PotatoConfig::default();
        let sizes = |cpus, players| Potato::sizes_for(cpus, players, &cfg);
        assert_eq!(sizes(Some(8), 96), vec![16, 32, 64, 96]);
        assert_eq!(
            sizes(Some(8), 64),
            vec![16, 32, 64],
            "the players are not a rung twice"
        );
        assert_eq!(sizes(Some(8), 80), vec![16, 32, 64, 80]);
        assert_eq!(sizes(Some(3), 20), vec![6, 12, 20]);
        assert_eq!(sizes(Some(8), 17), vec![16, 17]);
        assert_eq!(
            sizes(Some(8), 4),
            vec![4],
            "at or below K: the players alone"
        );
        // an agent that reports no allotment plays the configured sizes it can run
        assert_eq!(sizes(None, 96), cfg.sizes);
        assert_eq!(sizes(None, 5), vec![2, 4]);
    }

    /// A game on a host of `players` and `cpus`, its first round started.
    fn ladder_game(players: u32, cpus: u32) -> (Potato, PotatoConfig, Vec<PotatoProof>) {
        let cfg = PotatoConfig::default();
        let mut g = Potato::default();
        g.agent_hello("h", players, cpus, true, 0, &cfg);
        let proofs = hellos(&mut g, players, &cfg);
        g.tick(1000, true, &cfg);
        (g, cfg, proofs)
    }

    /// Every player's first hello.
    fn hellos(g: &mut Potato, players: u32, cfg: &PotatoConfig) -> Vec<PotatoProof> {
        (0..players)
            .map(|p| {
                let proof = PotatoProof {
                    colour: p % 12,
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
                    cfg,
                );
                proof
            })
            .collect()
    }

    /// The catch `e`, its catcher reporting that it keeps the ball `hold_ms`.
    fn held(mut e: PotatoEvent, hold_ms: u64) -> PotatoEvent {
        if let PotatoKind::Caught { hold_us, .. } = &mut e.kind {
            *hold_us = Some(hold_ms * 1000);
        }
        e
    }

    /// One lap of the round being played, its catches `gap` ms apart and each held for `gap`,
    /// starting at `at`.
    fn lap(
        g: &mut Potato,
        cfg: &PotatoConfig,
        proofs: &mut [PotatoProof],
        from: u64,
        at: u64,
        gap: u64,
    ) {
        let (id, n) = {
            let r = g.round.as_ref().unwrap();
            (r.id, r.ring.len() as u64)
        };
        for i in 0..n {
            let seq = from + i;
            let e = held(
                catch(proofs, (seq % n) as u32, id, seq, at + i * gap, false),
                gap,
            );
            g.event("h", &e, e.at, cfg);
        }
    }

    /// The ball dropped for want of a catcher, as the agent reports it.
    fn dropped(g: &mut Potato, cfg: &PotatoConfig, at: u64, phase: &str, cpu_wait: Option<f64>) {
        dropped_with(g, cfg, at, phase, 90.0, cpu_wait, None)
    }

    /// As [`dropped`], with how long the catcher had been in its phase and, for a catcher
    /// that is awake by now, how late it woke.
    fn dropped_with(
        g: &mut Potato,
        cfg: &PotatoConfig,
        at: u64,
        phase: &str,
        phase_ms: f64,
        cpu_wait: Option<f64>,
        late: Option<f64>,
    ) {
        let (id, seq, n) = {
            let r = g.round.as_ref().unwrap();
            (r.id, r.seq.unwrap_or(0), r.ring.len() as u64)
        };
        let e = PotatoEvent {
            at,
            player: (seq % n) as u32,
            kind: PotatoKind::Dropped {
                round: id,
                seq: seq + 1,
                to: ((seq + 1) % n) as u32,
                attempts: 2,
                reason: "connection refused".into(),
                catcher_phase: phase.into(),
                catcher_phase_ms: phase_ms,
                catcher_cpu_wait_ms: cpu_wait,
                catcher_late_ms: late,
            },
        };
        g.event("h", &e, at, cfg);
    }

    /// A size's record is the fastest pace a round of it held for a full lap. The record it
    /// replaced is kept beside it, and is none until a record has been beaten: a round's own
    /// earlier laps are not a record it beat.
    #[test]
    fn a_record_is_the_fastest_pace_of_a_full_lap() {
        let (mut g, cfg, mut proofs) = ladder_game(4, 8);
        let rung = |g: &Potato| g.view(&cfg).unwrap().ladder[0].clone();
        assert_eq!(
            g.view(&cfg).unwrap().ladder.len(),
            1,
            "one rung: the 4 players"
        );
        assert_eq!(
            (rung(&g).record_pace, rung(&g).record_prev_pace),
            (None, None)
        );
        lap(&mut g, &cfg, &mut proofs, 0, 1100, 200);
        assert_eq!(
            rung(&g).record_pace,
            Some(5.0),
            "3 catches after the first in 600 ms"
        );
        assert_eq!(
            rung(&g).record_prev_pace,
            None,
            "the first record beat nothing"
        );
        // the last catcher of lap 0 holds 200 ms, then a catch every 100 ms
        lap(&mut g, &cfg, &mut proofs, 4, 1900, 100);
        assert_eq!(
            rung(&g).record_pace,
            Some(8.0),
            "4 catches in the 500 ms after lap 0"
        );
        assert_eq!(
            rung(&g).record_prev_pace,
            None,
            "the same round's earlier lap"
        );
        dropped(&mut g, &cfg, 2300, "waking", None);
        // the next round of the size: slower, the record stands
        g.tick(2300 + cfg.restart_after_ms, true, &cfg);
        assert_eq!(
            g.round.as_ref().unwrap().size,
            4,
            "one rung wraps to itself"
        );
        lap(&mut g, &cfg, &mut proofs, 0, 8000, 500);
        lap(&mut g, &cfg, &mut proofs, 4, 10_000, 50);
        assert_eq!(
            (rung(&g).record_pace, rung(&g).record_prev_pace),
            (Some(8.0), None)
        );
        // and faster: the record it beat is kept
        lap(&mut g, &cfg, &mut proofs, 8, 10_200, 50);
        assert_eq!(rung(&g).record_pace, Some(20.0), "4 catches in 200 ms");
        assert_eq!(rung(&g).record_prev_pace, Some(8.0));
        lap(&mut g, &cfg, &mut proofs, 12, 10_400, 25);
        assert_eq!(rung(&g).record_pace, Some(32.0), "4 catches in 125 ms");
        assert_eq!(
            rung(&g).record_prev_pace,
            Some(8.0),
            "what the round beat, not its last lap"
        );
        // acknowledged state: it stays across a restart
        g.resumed();
        assert_eq!(g.ladder[0].record_pace, Some(32.0));
        assert_eq!(g.ladder[0].record_prev_pace, Some(8.0));
    }

    /// A round starts near where the pace last outran its size: 1.5 times the median hold of
    /// the size's recent such drops for its first round since the host connected, 1.25 times
    /// after, the configured start while it has none, and never above the configured start.
    #[test]
    fn a_round_starts_near_where_its_size_dropped_the_ball() {
        let (mut g, cfg, _proofs) = ladder_game(4, 8);
        let start = |g: &Potato| g.round.as_ref().unwrap().start_hold_ms;
        let configured = cfg.start_hold_ms(4);
        assert_eq!((start(&g), configured), (666, 666), "no drop yet");
        let mut at = 1500;
        let mut next_round = |g: &mut Potato| {
            dropped(g, &cfg, at, "waking", None);
            at += cfg.restart_after_ms;
            g.tick(at, true, &cfg);
            at += 500;
        };
        next_round(&mut g);
        assert_eq!(
            g.ladder[0].drop_holds,
            vec![666],
            "it dropped at its starting hold"
        );
        assert_eq!(
            start(&g),
            666,
            "1.25 times 666 is above the configured start"
        );
        // the size has played on this connection: 1.25 times the median
        g.ladder[0].drop_holds = vec![120];
        next_round(&mut g);
        assert_eq!(g.ladder[0].drop_holds, vec![120, 666]);
        assert_eq!(start(&g), 492, "1.25 times the median of 120 and 666");
        g.ladder[0].drop_holds = vec![120];
        // a host that connects again starts each size further back, once
        g.agent_gone("h", at, &cfg);
        g.agent_hello("h", 4, 8, true, at + 100, &cfg);
        g.tick(at + 1200, true, &cfg);
        assert_eq!(start(&g), 180, "1.5 times 120");
        // the median of the last five
        at += 2000;
        g.ladder[0].drop_holds = vec![900, 100, 120, 110, 130, 140];
        dropped(&mut g, &cfg, at, "waking", None);
        assert_eq!(g.ladder[0].drop_holds, vec![120, 110, 130, 140, 180]);
        g.tick(at + cfg.restart_after_ms, true, &cfg);
        assert_eq!(start(&g), 163, "1.25 times the median, 130");
        // a round that ends another way says nothing of the edge
        g.agent_gone("h", at + 20_000, &cfg);
        assert_eq!(g.ladder[0].drop_holds, vec![120, 110, 130, 140, 180]);
    }

    /// One player whose wake never finishes in time drops every ball at whatever hold the
    /// round started with. Each such drop used to raise the next start by a quarter, without
    /// end, and the holds are kept in the state file: after an hour a ring started at minutes
    /// of hold. The start never passes the configured one, whatever a state file holds, and a
    /// drop the pace did not cause does not move it at all.
    #[test]
    fn a_ring_that_keeps_dropping_at_once_starts_no_slower_than_the_configured_start() {
        let (mut g, cfg, _proofs) = ladder_game(4, 8);
        let configured = cfg.start_hold_ms(4);
        let mut at = 1500;
        for _ in 0..50 {
            // the catcher of the first throw is still waking, at any hold
            dropped(&mut g, &cfg, at, "waking", None);
            at += cfg.restart_after_ms;
            g.tick(at, true, &cfg);
            at += 500;
            let r = g.round.as_ref().expect("the next round starts");
            assert!(
                r.start_hold_ms <= configured,
                "started at {} ms, the configured start is {configured}",
                r.start_hold_ms
            );
        }
        assert!(g.ladder[0].drop_holds.iter().all(|h| *h <= configured));
        // what an earlier coordinator wrote is bounded when it is used
        g.ladder[0].drop_holds = vec![3_600_000; 5];
        dropped(&mut g, &cfg, at, "waking", None);
        g.tick(at + cfg.restart_after_ms, true, &cfg);
        assert_eq!(g.round.as_ref().unwrap().start_hold_ms, configured);
        // drops the pace did not cause leave the history alone: a catcher awake all along
        // that refused, one asleep that nothing was waking, and a wake that failed
        g.ladder[0].drop_holds = vec![100];
        at += 10 * cfg.restart_after_ms;
        dropped_with(&mut g, &cfg, at, "awake", 5000.0, None, None);
        g.tick(at + cfg.restart_after_ms, true, &cfg);
        dropped(
            &mut g,
            &cfg,
            at + cfg.restart_after_ms + 100,
            "asleep",
            None,
        );
        g.tick(at + 2 * cfg.restart_after_ms + 100, true, &cfg);
        let failed = PotatoEvent {
            at: 1,
            player: 1,
            kind: PotatoKind::Failed {
                stage: "wake".into(),
                error: "gone".into(),
            },
        };
        g.event("h", &failed, at + 2 * cfg.restart_after_ms + 200, &cfg);
        assert_eq!(g.last_drop.as_ref().unwrap().reason, "wake_failed");
        assert_eq!(g.ladder[0].drop_holds, vec![100]);
    }

    /// A drop names its cause from what the agent measured of the catcher.
    #[test]
    fn a_drop_names_its_measured_cause() {
        let cause = |phase: &str, wait: Option<f64>| {
            let (mut g, cfg, _proofs) = ladder_game(4, 8);
            dropped(&mut g, &cfg, 1500, phase, wait);
            let d = g.last_drop.clone().unwrap();
            assert_eq!(
                g.ladder[0].broke.as_ref(),
                Some(&d),
                "the rung's drop is the same"
            );
            (g, cfg, d.cause, d.cause_ms)
        };
        let (_, _, c, ms) = cause("asleep", Some(140.5));
        assert_eq!((c.as_deref(), ms), (Some("cpu_wait"), Some(140.5)));
        let (_, _, c, ms) = cause("sleeping", None);
        assert_eq!((c.as_deref(), ms), (Some("saving"), Some(90.0)));
        let (_, _, c, ms) = cause("asleep", None);
        assert_eq!(
            (c, ms),
            (None, None),
            "no wake was asked for: nothing measured to name"
        );
        // still waking: how late arrives when the catcher does accept
        let (mut g, cfg, c, ms) = cause("waking", None);
        assert_eq!((c.as_deref(), ms), (Some("waking"), None));
        let d = g.last_drop.clone().unwrap();
        let listening = PotatoEvent {
            at: 1600,
            player: d.player.unwrap(),
            kind: PotatoKind::Listening {
                round: d.round,
                seq: d.seq,
                after_ms: 84.0,
            },
        };
        g.event("h", &listening, 1600, &cfg);
        assert_eq!(g.last_drop.as_ref().unwrap().cause_ms, Some(84.0));
        assert_eq!(g.ladder[0].broke.as_ref().unwrap().cause_ms, Some(84.0));
        // a proof that does not verify
        let (mut g, cfg, mut proofs) = ladder_game(4, 8);
        let id = g.round.as_ref().unwrap().id;
        let e = catch(&mut proofs, 0, id, 0, 1100, true);
        g.event("h", &e, 1100, &cfg);
        let d = g.last_drop.as_ref().unwrap();
        assert_eq!(
            (d.reason.as_str(), d.cause.as_deref(), d.cause_ms),
            ("proof", Some("proof"), None)
        );
        // the pace it had reached
        let (mut g, cfg, mut proofs) = ladder_game(4, 8);
        lap(&mut g, &cfg, &mut proofs, 0, 1100, 200);
        dropped(&mut g, &cfg, 2000, "waking", None);
        assert_eq!(g.last_drop.as_ref().unwrap().pace, Some(5.0));
    }

    /// A catcher's wake that waited for a CPU and then ran: the drop carries the wait, and the
    /// cause is the wait when it is more than half of how late the catcher was, else the wake.
    #[test]
    fn a_late_wake_is_the_waits_fault_when_the_wait_is_the_larger_part() {
        let late = |wait: Option<f64>, after_ms: f64| {
            let (mut g, cfg, _proofs) = ladder_game(4, 8);
            // 90 ms into its wake when the ball dropped
            dropped(&mut g, &cfg, 1500, "waking", wait);
            let d = g.last_drop.clone().unwrap();
            // how late it was is not reported yet: a wait longer than the wake has run
            let so_far = match wait {
                Some(w) if w > 90.0 => (Some("cpu_wait"), Some(w)),
                _ => (Some("waking"), None),
            };
            assert_eq!((d.cause.as_deref(), d.cause_ms), so_far);
            assert_eq!(d.cpu_wait_ms, wait);
            let listening = PotatoEvent {
                at: 1600,
                player: d.player.unwrap(),
                kind: PotatoKind::Listening {
                    round: d.round,
                    seq: d.seq,
                    after_ms,
                },
            };
            g.event("h", &listening, 1600, &cfg);
            let d = g.last_drop.clone().unwrap();
            assert_eq!(
                g.ladder[0].broke.as_ref(),
                Some(&d),
                "the rung's drop is the same"
            );
            assert_eq!(d.cpu_wait_ms, wait);
            (d.cause.unwrap(), d.cause_ms)
        };
        // it waited 100 ms and was 84 ms late: without the wait it would have been on time
        assert_eq!(late(Some(100.0), 84.0), ("cpu_wait".into(), Some(100.0)));
        // it waited 50 ms and was 84 ms late: more than half of it
        assert_eq!(late(Some(50.0), 84.0), ("cpu_wait".into(), Some(50.0)));
        // it waited 20 ms and was 84 ms late: the wake itself was the larger part
        assert_eq!(late(Some(20.0), 84.0), ("waking".into(), Some(84.0)));
        // exactly half is not the larger part
        assert_eq!(late(Some(42.0), 84.0), ("waking".into(), Some(84.0)));
        assert_eq!(late(None, 84.0), ("waking".into(), Some(84.0)));
        // a wake that waited 400 ms for a CPU and had run 30 ms when the ball dropped: the
        // drop says so at once, with the wait, and still does when the lateness arrives
        let (mut g, cfg, _proofs) = ladder_game(4, 8);
        dropped_with(&mut g, &cfg, 1500, "waking", 30.0, Some(400.0), None);
        let d = g.last_drop.clone().unwrap();
        assert_eq!(
            (d.cause.as_deref(), d.cause_ms, d.cpu_wait_ms),
            (Some("cpu_wait"), Some(400.0), Some(400.0))
        );
        let listening = PotatoEvent {
            at: 1600,
            player: d.player.unwrap(),
            kind: PotatoKind::Listening {
                round: d.round,
                seq: d.seq,
                after_ms: 60.0,
            },
        };
        g.event("h", &listening, 1600, &cfg);
        let d = g.last_drop.clone().unwrap();
        assert_eq!(
            (d.cause.as_deref(), d.cause_ms),
            (Some("cpu_wait"), Some(400.0))
        );
    }

    /// A drop read just after the catcher's wake was reported: the catcher is `awake`, for
    /// less than the thrower's flight window. It was late, by what the agent measured, and
    /// the drop says so instead of naming no cause.
    #[test]
    fn a_catcher_that_woke_within_the_flight_window_was_late() {
        let cause = |phase_ms: f64, wait: Option<f64>, late: Option<f64>| {
            let (mut g, cfg, _proofs) = ladder_game(4, 8);
            assert_eq!(g.round.as_ref().unwrap().flight_window_ms(), 100);
            dropped_with(&mut g, &cfg, 1500, "awake", phase_ms, wait, late);
            let d = g.last_drop.clone().unwrap();
            assert_eq!(g.ladder[0].broke.as_ref(), Some(&d));
            assert_eq!(d.stage, "awake", "the phase is reported as it was");
            (d.cause, d.cause_ms, d.cpu_wait_ms)
        };
        assert_eq!(
            cause(17.3, None, Some(3.5)),
            (Some("waking".into()), Some(3.5), None)
        );
        assert_eq!(
            cause(0.1, None, Some(0.0)),
            (Some("waking".into()), Some(0.0), None),
            "ready between the thrower's last try and the end of its window"
        );
        // its wake had waited for a CPU, for more than half of its lateness
        assert_eq!(
            cause(0.1, Some(150.0), Some(12.0)),
            (Some("cpu_wait".into()), Some(150.0), Some(150.0))
        );
        assert_eq!(
            cause(40.0, Some(5.0), Some(12.0)),
            (Some("waking".into()), Some(12.0), Some(5.0))
        );
        // an agent that does not report the lateness: late, by an amount not measured
        assert_eq!(cause(17.3, None, None), (Some("waking".into()), None, None));
        // awake for longer than a thrower tries: its wake is not why the ball dropped
        assert_eq!(cause(100.0, None, None), (None, None, None));
        assert_eq!(cause(5000.0, Some(150.0), None), (None, None, Some(150.0)));
    }

    /// A state file written before the ladder was by pace: it loads, keeps its place (by
    /// size), its round count and its drops, and shows no hold as a record.
    #[test]
    fn an_older_state_file_keeps_its_place_and_shows_no_hold_records() {
        let old = serde_json::json!({
            "ladder": [
                {"size": 2, "record_hold_ms": 1458, "record_at": 1762317000000u64, "rounds": 3, "broke": null},
                {"size": 4, "record_hold_ms": 599, "record_at": 1762317000000u64, "rounds": 3,
                 "broke": {"at": 7, "round": 40, "size": 4, "reason": "not_listening", "player": 1,
                           "thrower": 0, "seq": 11, "lap": 2, "hold_ms": 485, "stage": "waking",
                           "stage_ms": 90.0, "listening_after_ms": 84.0, "detail": "x"}},
                {"size": 8, "record_hold_ms": null, "record_at": null, "rounds": 2, "broke": null},
                {"size": 16, "record_hold_ms": 250, "record_at": 5, "rounds": 2, "broke": null},
                {"size": 32, "rounds": 0}, {"size": 64, "rounds": 0}, {"size": 96, "rounds": 0}
            ],
            "rung": 3,
            "records": {"fastest_cycle_ms": {"value": 650.0, "size": 2, "player": 0, "at": 9},
                        "longest_round": {"value": 151.0, "size": 64, "player": null, "at": 9}},
            "round_count": 41,
            "last_drop": null
        });
        let cfg = PotatoConfig::default();
        let mut g: Potato = serde_json::from_value(old).unwrap();
        g.resumed();
        assert_eq!(g.next_size, Some(16), "its place, by size");
        assert_eq!(g.round_count, 41);
        assert_eq!(
            g.records.longest_round.as_ref().map(|r| r.value),
            Some(151.0)
        );
        let v = serde_json::to_value(g.view(&cfg).unwrap()).unwrap();
        assert_eq!(v.pointer("/size"), Some(&serde_json::json!(16)));
        assert_eq!(v.pointer("/ladder/1/rounds"), Some(&serde_json::json!(3)));
        assert_eq!(
            v.pointer("/ladder/1/broke/hold_ms"),
            Some(&serde_json::json!(485))
        );
        assert_eq!(
            v.pointer("/ladder/1/broke/cause"),
            Some(&serde_json::Value::Null)
        );
        for rung in v["ladder"].as_array().unwrap() {
            assert_eq!(rung["record_pace"], serde_json::Value::Null);
            assert!(
                rung.get("record_hold_ms").is_none(),
                "a hold is not shown as a record"
            );
        }
        // what is written from now on has neither the index nor the holds
        let written = serde_json::to_string(&g).unwrap();
        assert!(
            !written.contains("record_hold_ms") && !written.contains("\"rung\""),
            "{written}"
        );
        let again: Potato = serde_json::from_str(&written).unwrap();
        assert_eq!(again, g);

        // an agent without an allotment plays that ladder on, from its size
        let mut uncapped = g.clone();
        uncapped.agent_hello("old", 96, 0, false, 100, &cfg);
        uncapped.tick(2000, true, &cfg);
        let r = uncapped.round.as_ref().unwrap();
        assert_eq!((r.size, r.id), (16, 42));
        assert_eq!(uncapped.ladder[1].rounds, 3);

        // an agent with one gets the ladder of its K: the rung of the same size, new records
        g.agent_hello("h", 96, 8, true, 100, &cfg);
        assert_eq!(g.sizes, vec![16, 32, 64, 96]);
        assert_eq!(
            g.view(&cfg)
                .unwrap()
                .ladder
                .iter()
                .map(|r| r.rounds)
                .sum::<u64>(),
            0
        );
        g.tick(2000, true, &cfg);
        let r = g.round.as_ref().unwrap();
        assert_eq!(
            (r.size, r.id),
            (16, 42),
            "the same size, and round numbers go on"
        );
        // the other ladder is kept, with what it played, for a host without an allotment
        assert_eq!(g.shelved[&0].iter().map(|r| r.rounds).sum::<u64>(), 10);
        g.agent_gone("h", 3000, &cfg);
        g.agent_hello("old", 96, 0, false, 4000, &cfg);
        assert_eq!(g.ladder.iter().map(|r| r.rounds).sum::<u64>(), 10);
        assert_eq!(
            g.shelved[&8].len(),
            4,
            "and the allotment's ladder is put away in turn"
        );
    }

    /// The ladder moves up on every drop and wraps, whatever the allotment.
    #[test]
    fn the_ladder_moves_up_on_each_drop() {
        let (mut g, cfg, _proofs) = ladder_game(40, 8);
        let mut played = Vec::new();
        let mut at = 1000;
        for _ in 0..4 {
            played.push(g.round.as_ref().unwrap().size);
            at += 500;
            dropped(&mut g, &cfg, at, "waking", None);
            at += cfg.restart_after_ms;
            g.tick(at, true, &cfg);
        }
        assert_eq!(played, vec![16, 32, 40, 16]);
        assert_eq!(g.view(&cfg).unwrap().size, Some(32));
    }
}
