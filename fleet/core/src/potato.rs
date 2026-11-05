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
    Caught {
        round: u64,
        seq: u64,
        #[serde(flatten)]
        proof: PotatoProof,
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
}
