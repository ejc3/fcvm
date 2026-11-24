//! Hot potato (docs/fleet/ARCHITECTURE.md, "Potato mode"): the wire types a player VM, the
//! host agent and the coordinator share, and the proof a player gives that its RAM survived
//! a sleep.
//!
//! A player holds three things in guest RAM and nowhere else: its colour (an index into a
//! ring of colours), a catch counter and a hash chain over every ball it has caught. On a
//! catch it steps all three. The coordinator replays the same step from the last values it
//! verified, so a player restored from a snapshot that lost or rolled back its memory
//! answers with the wrong colour, counter or chain.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The TCP port a player accepts the ball on, inside its VM.
pub const BALL_PORT: u16 = 7070;

/// How many colours the ring has unless the player is told otherwise.
pub const DEFAULT_COLOURS: u32 = 12;

/// The longest trail of colours a ball carries; older entries fall off the front.
pub const TRAIL_MAX: usize = 16;

/// A hash chain value, hex encoded on the wire.
pub type Chain = [u8; 32];

/// The chain a player starts with.
pub const CHAIN_ZERO: Chain = [0; 32];

/// The chain after catching ball `seq` of `round` and stepping to `colour`.
pub fn chain_step(prev: &Chain, round: u64, seq: u64, colour: u32) -> Chain {
    let mut h = Sha256::new();
    h.update(b"fcvm-potato-v1");
    h.update(prev);
    h.update(round.to_le_bytes());
    h.update(seq.to_le_bytes());
    h.update(colour.to_le_bytes());
    h.finalize().into()
}

/// The colour after `colour` in a ring of `colours`.
pub fn next_colour(colour: u32, colours: u32) -> u32 {
    (colour + 1) % colours.max(1)
}

pub fn chain_hex(c: &Chain) -> String {
    c.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn chain_from_hex(s: &str) -> Option<Chain> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// What a player's state is, as it reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proof {
    pub colour: u32,
    pub counter: u64,
    /// Hex of the chain.
    pub chain: String,
}

/// The ball. It travels from one player VM to the next over TCP, one JSON line, and nothing
/// relays it: the agent and coordinator only read the reports the players send.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ball {
    pub round: u64,
    /// Catches so far in this round; the catcher of `seq` is `ring[seq % ring.len()]`.
    pub seq: u64,
    /// Every player of the ring, as `[addr]:port` (or `host:port`), in throwing order.
    pub ring: Vec<String>,
    /// How long the catcher holds the ball before it throws, in ms: `hold_us` rounded, for a
    /// player built before `hold_us`.
    pub hold_ms: u64,
    /// The hold in microseconds, which is what shrinks: a tenth off 9 ms is 0 in whole
    /// milliseconds, so a hold kept in them stops at 9 ms and never reaches its floor. None
    /// on a ball from before this field; the hold is then `hold_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_us: Option<u64>,
    /// The fraction `hold_ms` shrinks by each lap, in parts per thousand.
    pub shrink_permille: u32,
    /// `hold_ms` never goes below this.
    pub min_hold_ms: u64,
    /// How long a thrower keeps trying to reach the catcher, from the moment it starts.
    pub flight_ms: u64,
    /// The colours of the last catchers, oldest first.
    #[serde(default)]
    pub trail: Vec<u32>,
}

impl Ball {
    pub fn lap(&self) -> u64 {
        self.seq / self.ring.len().max(1) as u64
    }

    /// The hold the ball carries, in microseconds.
    pub fn hold_us(&self) -> u64 {
        self.hold_us.unwrap_or(self.hold_ms * 1000)
    }

    /// The hold a catcher keeps, in microseconds: the ball's, capped by a viewers' pace (a hold
    /// in ms), never under the floor.
    pub fn kept_us(&self, pace_ms: Option<u64>) -> u64 {
        match pace_ms {
            Some(p) => self.hold_us().min(p.max(self.min_hold_ms) * 1000),
            None => self.hold_us(),
        }
    }

    /// How long a thrower that kept `kept_us` tries to reach the catcher: `flight_ms`, or the
    /// hold when that is shorter, so a lap is not completed at a hold the throws did not keep.
    pub fn flight_us(&self, kept_us: u64) -> u64 {
        (self.flight_ms * 1000).min(kept_us)
    }

    /// The ball the catcher of `self` throws: the next seq, the catcher's colour on the
    /// trail, and the hold shrunk when the throw starts a new lap.
    pub fn thrown(&self, colour: u32) -> Ball {
        let mut b = self.clone();
        b.seq += 1;
        b.trail.push(colour);
        if b.trail.len() > TRAIL_MAX {
            let cut = b.trail.len() - TRAIL_MAX;
            b.trail.drain(..cut);
        }
        let mut us = self.hold_us();
        if b.lap() != self.lap() {
            us = shrink_us(us, self.shrink_permille, self.min_hold_ms);
        }
        b.hold_us = Some(us);
        b.hold_ms = ms_of(us);
        b
    }
}

/// A hold of `us` microseconds shrunk by `permille` thousandths, never below `min_ms`
/// milliseconds.
pub fn shrink_us(us: u64, permille: u32, min_ms: u64) -> u64 {
    let cut = us * u64::from(permille.min(1000)) / 1000;
    (us - cut).max(min_ms * 1000)
}

/// The hold a ball that started at `start_ms` carries after `laps` laps, in microseconds.
pub fn hold_us_after(start_ms: u64, permille: u32, min_ms: u64, laps: u64) -> u64 {
    let floor = min_ms * 1000;
    let mut us = start_ms * 1000;
    // a hold that no longer shrinks (the floor, or a shrink of 0) ends the loop
    for _ in 0..laps {
        let next = shrink_us(us, permille, min_ms);
        if next == us {
            break;
        }
        us = next;
    }
    us.max(floor)
}

/// Microseconds as whole milliseconds, rounded.
pub fn ms_of(us: u64) -> u64 {
    (us + 500) / 1000
}

/// A message on a player's ball port.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Inbound {
    Ball(Ball),
    /// Answered with a `Probe` report on the same connection, then closed. The agent uses it
    /// to learn when a woken player accepts connections again.
    Probe,
    /// Gives an unassigned player (started without `POTATO_PLAYER`, or a clone of one) its
    /// number; it then chooses its colour and starts reporting. Every player of a host is a
    /// clone of one snapshot of an unassigned player. Refused once a player has a number.
    Assign {
        player: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        colours: Option<u32>,
    },
    /// A shorter hold for `round`, asked for after it started: the player's next throws in
    /// that round hold no longer than `hold_ms`. The ball keeps its own schedule.
    Pace {
        round: u64,
        hold_ms: u64,
    },
    /// The agent restored the player: it catches again. Balls of rounds below `round`, when
    /// given, are refused from now on. A player only learns it woke from this message: a
    /// report connection can fail for other reasons.
    Wake {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        round: Option<u64>,
    },
    /// Round `round` is over, and a sleep follows: every ball is refused until a `wake`, and
    /// a throw of the round still waiting for its hold is abandoned when the hold ends. The
    /// agent sends it before it puts a player to sleep at a round's end. A hold the snapshot
    /// paused ends after the next wake.
    Stop {
        round: u64,
    },
}

/// A player's one-line answer to a ball, an `assign`, a `pace`, a `wake` or a `stop`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    Ok,
    Refused { reason: String },
}

impl Reply {
    /// Whether `line` is an `ok`: the line decoded, never searched for the word, so a refusal
    /// whose reason says "ok" is a refusal.
    pub fn ok(line: &str) -> Result<(), String> {
        match serde_json::from_str::<Reply>(line.trim()) {
            Ok(Reply::Ok) => Ok(()),
            Ok(Reply::Refused { reason }) => Err(format!("refused: {reason}")),
            Err(e) => Err(format!("not an answer ({e}): {:?}", line.trim())),
        }
    }
}

/// What a player tells the host agent, one JSON line each, on its report connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Report {
    /// Sent first on every report connection. A player opens a new one when its old one
    /// fails, which is what happens on every wake: the restore retires the connections the
    /// snapshot held.
    Hello {
        player: u32,
        colours: u32,
        #[serde(flatten)]
        proof: Proof,
        /// How many report connections this player has opened, this one included.
        connections: u64,
        /// The round and seq of the last ball this player caught. A hello can be one catch
        /// ahead of the reports a verifier has read: a sleep that pauses the VM between a
        /// catch and its report sends the report after the wake's hello.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_round: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_seq: Option<u64>,
    },
    Caught {
        player: u32,
        round: u64,
        seq: u64,
        #[serde(flatten)]
        proof: Proof,
        /// The hold the catcher keeps before it throws, in microseconds, after any pace cap.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hold_us: Option<u64>,
    },
    /// The catcher accepted the ball.
    Threw {
        player: u32,
        round: u64,
        seq: u64,
        to: String,
        attempts: u32,
        /// From the start of the throw to the first attempt that connected.
        wait_ms: f64,
        /// The connect and send of the attempt that landed.
        send_ms: f64,
    },
    /// Nothing accepted the ball within its flight window.
    Dropped {
        player: u32,
        round: u64,
        seq: u64,
        to: String,
        attempts: u32,
        reason: String,
    },
    /// The answer to a probe, on the probe's own connection; `player` is none until the
    /// player is assigned.
    Probe {
        player: Option<u32>,
        #[serde(flatten)]
        proof: Proof,
        /// Whether it catches the ball now: false from its throw until it is woken.
        #[serde(default)]
        accepting: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ball() -> Ball {
        Ball {
            round: 1,
            seq: 0,
            ring: vec!["a".into(), "b".into()],
            hold_ms: 1000,
            hold_us: None,
            shrink_permille: 100,
            min_hold_ms: 50,
            flight_ms: 500,
            trail: vec![],
        }
    }

    #[test]
    fn the_chain_depends_on_every_input() {
        let a = chain_step(&CHAIN_ZERO, 1, 2, 3);
        assert_ne!(a, chain_step(&CHAIN_ZERO, 1, 2, 4));
        assert_ne!(a, chain_step(&CHAIN_ZERO, 1, 3, 3));
        assert_ne!(a, chain_step(&CHAIN_ZERO, 2, 2, 3));
        assert_ne!(a, chain_step(&a, 1, 2, 3));
        assert_eq!(chain_from_hex(&chain_hex(&a)), Some(a));
    }

    /// The value fleet/potato/bench/potatobench.py's `chain_step` computes for the same
    /// inputs; the coordinator, the bench and the player must agree on the chain.
    #[test]
    fn the_chain_has_a_fixed_value() {
        assert_eq!(
            chain_hex(&chain_step(&CHAIN_ZERO, 1, 2, 3)),
            "d3a0dadfb470f592b76503c24a34c46dbae84f50d6ce04ebd2974b292ea5481d"
        );
    }

    #[test]
    fn a_reply_is_decoded_not_searched() {
        assert_eq!(Reply::ok("{\"type\":\"ok\"}\n"), Ok(()));
        assert!(Reply::ok("{\"type\":\"refused\",\"reason\":\"ok\"}").is_err());
        assert!(Reply::ok("ok").is_err());
        assert!(Reply::ok("").is_err());
    }

    #[test]
    fn colours_wrap() {
        assert_eq!(next_colour(11, 12), 0);
        assert_eq!(next_colour(3, 12), 4);
    }

    #[test]
    fn the_hold_shrinks_once_per_lap() {
        let b = ball().thrown(4);
        assert_eq!((b.seq, b.hold_ms, b.trail.clone()), (1, 1000, vec![4]));
        let c = b.thrown(5);
        assert_eq!((c.seq, c.lap(), c.hold_ms), (2, 1, 900));
        let d = c.thrown(6);
        assert_eq!((d.hold_ms, d.hold_us), (900, Some(900_000)));
        assert_eq!(shrink_us(55_000, 100, 50), 50_000);
        assert_eq!(hold_us_after(1000, 100, 50, 2), 810_000);
        assert_eq!(hold_us_after(1000, 100, 50, 1_000_000), 50_000);
    }

    /// A ball from a player built before `hold_us` carries only `hold_ms`.
    #[test]
    fn a_ball_without_microseconds_holds_its_milliseconds() {
        let old: Ball = serde_json::from_str(
            r#"{"round":1,"seq":0,"ring":["a","b"],"hold_ms":300,"shrink_permille":100,"min_hold_ms":2,"flight_ms":100}"#,
        )
        .unwrap();
        assert_eq!(old.hold_us(), 300_000);
        assert!(!serde_json::to_string(&old).unwrap().contains("hold_us"));
    }

    #[test]
    fn a_pace_caps_the_hold_kept_and_the_flight_is_no_longer_than_it() {
        let b = ball();
        assert_eq!(b.kept_us(None), 1_000_000);
        assert_eq!(b.kept_us(Some(200)), 200_000);
        assert_eq!(b.kept_us(Some(1)), 50_000, "never under the floor");
        assert_eq!(b.flight_us(1_000_000), 500_000, "the ball's flight window");
        assert_eq!(b.flight_us(200_000), 200_000, "the hold, when shorter");
    }

    /// A tenth off each lap, from 400 ms: cut in whole milliseconds the hold stops at 9 ms
    /// (a tenth of 9 is 0), and the floor is never reached.
    #[test]
    fn the_hold_reaches_its_floor() {
        let mut b = Ball {
            hold_ms: 400,
            min_hold_ms: 2,
            ..ball()
        };
        for c in 0..400 {
            b = b.thrown(c);
        }
        assert_eq!(b.hold_ms, 2, "200 laps of a tenth off each");
    }

    #[test]
    fn the_trail_keeps_the_latest() {
        let mut b = ball();
        for c in 0..40 {
            b = b.thrown(c);
        }
        assert_eq!(b.trail.len(), TRAIL_MAX);
        assert_eq!(*b.trail.last().unwrap(), 39);
    }

    #[test]
    fn reports_round_trip() {
        let r = Report::Caught {
            player: 3,
            round: 1,
            seq: 7,
            proof: Proof {
                colour: 2,
                counter: 9,
                chain: chain_hex(&CHAIN_ZERO),
            },
            hold_us: Some(250_000),
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.starts_with("{\"type\":\"caught\""), "{s}");
        assert_eq!(serde_json::from_str::<Report>(&s).unwrap(), r);
    }
}
