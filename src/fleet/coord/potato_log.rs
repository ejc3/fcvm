//! One log line for each sleep and each wake of a potato player, and one for each round's
//! start and end, so a round can be read from the coordinator's log without the host's:
//! which player slept or woke when (start and end on the host's clock, Unix ms), how long it
//! took and its stages, and what ended the round.
//!
//! A sleep's or a wake's line is written when it ends, from the agent's `slept` and `woke`
//! reports, whose `at` is the end and whose `sleep_ms` or `wake_ms` gives the start. A sleep
//! still going when a round ends shows in that round's end line, as the stage of the player
//! the ball could not reach, and gets its own line when it does end.

use fleet_core::potato::{PotatoEvent, PotatoKind};
use fleet_core::State;
use std::collections::BTreeMap;

/// "ask 0.0, create 1201.3, prepare 17.6" from a stage map, in milliseconds.
fn stages_text(stages: &BTreeMap<String, f64>) -> String {
    stages
        .iter()
        .map(|(k, v)| format!("{} {v:.1}", k.strip_suffix("_ms").unwrap_or(k)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The start of a step `took_ms` long that ended at `at` (Unix ms).
fn start_of(at: u64, took_ms: f64) -> u64 {
    at.saturating_sub(took_ms.max(0.0).round() as u64)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    Slept {
        player: u32,
        started_at: u64,
        ended_at: u64,
        sleep_ms: f64,
        stages: String,
        written_mib: Option<f64>,
    },
    Woke {
        player: u32,
        started_at: u64,
        ended_at: u64,
        wake_ms: f64,
        stages: String,
    },
    Failed {
        player: u32,
        at: u64,
        stage: String,
        error: String,
    },
    Dropped {
        thrower: u32,
        at: u64,
        round: u64,
        seq: u64,
        to: u32,
        catcher_phase: String,
        catcher_phase_ms: f64,
        attempts: u32,
        reason: String,
    },
    Listening {
        player: u32,
        round: u64,
        seq: u64,
        after_ms: f64,
    },
}

/// The line an agent's report gets, if any. Hellos, catches, throws and wakes starting are
/// left to the debug log: the throws start the sleeps whose lines carry their start.
pub fn line(e: &PotatoEvent) -> Option<Line> {
    Some(match &e.kind {
        PotatoKind::Slept {
            sleep_ms,
            stages,
            written_mib,
        } => Line::Slept {
            player: e.player,
            started_at: start_of(e.at, *sleep_ms),
            ended_at: e.at,
            sleep_ms: *sleep_ms,
            stages: stages_text(stages),
            written_mib: *written_mib,
        },
        PotatoKind::Woke { wake_ms, stages } => Line::Woke {
            player: e.player,
            started_at: start_of(e.at, *wake_ms),
            ended_at: e.at,
            wake_ms: *wake_ms,
            stages: stages_text(stages),
        },
        PotatoKind::Failed { stage, error } => Line::Failed {
            player: e.player,
            at: e.at,
            stage: stage.clone(),
            error: error.clone(),
        },
        PotatoKind::Dropped {
            round,
            seq,
            to,
            attempts,
            reason,
            catcher_phase,
            catcher_phase_ms,
        } => Line::Dropped {
            thrower: e.player,
            at: e.at,
            round: *round,
            seq: *seq,
            to: *to,
            catcher_phase: catcher_phase.clone(),
            catcher_phase_ms: *catcher_phase_ms,
            attempts: *attempts,
            reason: reason.clone(),
        },
        PotatoKind::Listening {
            round,
            seq,
            after_ms,
        } => Line::Listening {
            player: e.player,
            round: *round,
            seq: *seq,
            after_ms: *after_ms,
        },
        PotatoKind::Hello { .. }
        | PotatoKind::Caught { .. }
        | PotatoKind::Threw { .. }
        | PotatoKind::Waking {} => return None,
    })
}

/// Write a report's line. `round` is the round being played when it arrived.
pub fn log_event(host: &str, round: Option<u64>, e: &PotatoEvent) {
    let Some(l) = line(e) else { return };
    match l {
        Line::Slept {
            player,
            started_at,
            ended_at,
            sleep_ms,
            stages,
            written_mib,
        } => tracing::info!(
            target: "fcvm::fleet::potato",
            %host, ?round, player, started_at, ended_at, sleep_ms, %stages, ?written_mib,
            "potato player slept"
        ),
        Line::Woke {
            player,
            started_at,
            ended_at,
            wake_ms,
            stages,
        } => tracing::info!(
            target: "fcvm::fleet::potato",
            %host, ?round, player, started_at, ended_at, wake_ms, %stages,
            "potato player woke"
        ),
        Line::Failed {
            player,
            at,
            stage,
            error,
        } => tracing::warn!(
            target: "fcvm::fleet::potato",
            %host, ?round, player, at, %stage, %error,
            "potato player step failed"
        ),
        Line::Dropped {
            thrower,
            at,
            round: ball_round,
            seq,
            to,
            catcher_phase,
            catcher_phase_ms,
            attempts,
            reason,
        } => tracing::info!(
            target: "fcvm::fleet::potato",
            %host, ?round, ball_round, seq, thrower, to, at, %catcher_phase, catcher_phase_ms, attempts, %reason,
            "potato ball not caught"
        ),
        Line::Listening {
            player,
            round: ball_round,
            seq,
            after_ms,
        } => tracing::info!(
            target: "fcvm::fleet::potato",
            %host, ?round, ball_round, seq, player, after_ms,
            "potato catcher of a dropped ball listening"
        ),
    }
}

/// The parts of the state the round lines follow.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Marks {
    round: Option<u64>,
    /// The last drop's (round, at).
    drop: Option<(u64, u64)>,
}

impl Marks {
    pub fn of(s: &State) -> Marks {
        let p = &s.fleet.potato;
        Marks {
            round: p.round.as_ref().map(|r| r.id),
            drop: p.last_drop.as_ref().map(|d| (d.round, d.at)),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RoundNote {
    /// The round being played stopped with no drop: a viewer's restart, or a new agent
    /// connection for the host.
    EndedWithoutDrop {
        round: u64,
    },
    Started {
        round: u64,
        size: u32,
        at: u64,
        hold_ms: u64,
    },
    Ended {
        round: u64,
        size: u32,
        at: u64,
        lap: u64,
        seq: u64,
        hold_ms: u64,
        reason: String,
        player: Option<u32>,
        thrower: Option<u32>,
        stage: String,
        stage_ms: f64,
        sleep_stages: String,
        wake_stages: String,
        detail: String,
    },
}

/// What an applied event changed about the rounds: a round that ended (with what ended it, or
/// with no drop at all) and one that started.
pub fn notes(before: &Marks, s: &State) -> Vec<RoundNote> {
    let p = &s.fleet.potato;
    let mut out = Vec::new();
    if let Some(d) = p
        .last_drop
        .as_ref()
        .filter(|d| before.drop != Some((d.round, d.at)))
    {
        out.push(RoundNote::Ended {
            round: d.round,
            size: d.size,
            at: d.at,
            lap: d.lap,
            seq: d.seq,
            hold_ms: d.hold_ms,
            reason: d.reason.clone(),
            player: d.player,
            thrower: d.thrower,
            stage: d.stage.clone(),
            stage_ms: d.stage_ms,
            sleep_stages: stages_text(&d.sleep_stages),
            wake_stages: stages_text(&d.wake_stages),
            detail: d.detail.clone(),
        });
    }
    // a round that stopped without a drop of its own: restarted, or ended by a new connection
    if let Some(old) = before.round {
        let dropped = p
            .last_drop
            .as_ref()
            .is_some_and(|d| d.round == old && before.drop != Some((d.round, d.at)));
        let going = p.round.as_ref().is_some_and(|r| r.id == old);
        if !dropped && !going {
            out.push(RoundNote::EndedWithoutDrop { round: old });
        }
    }
    if let Some(r) = p.round.as_ref().filter(|r| before.round != Some(r.id)) {
        out.push(RoundNote::Started {
            round: r.id,
            size: r.size,
            at: r.started_at,
            hold_ms: r.start_hold_ms,
        });
    }
    out
}

pub fn log_rounds(notes: &[RoundNote]) {
    for n in notes {
        match n {
            RoundNote::EndedWithoutDrop { round } => tracing::info!(
                target: "fcvm::fleet::potato",
                round,
                "potato round ended without a drop (a restart, or a new connection for its host)"
            ),
            RoundNote::Started {
                round,
                size,
                at,
                hold_ms,
            } => tracing::info!(
                target: "fcvm::fleet::potato",
                round, size, at, hold_ms,
                "potato round started"
            ),
            RoundNote::Ended {
                round,
                size,
                at,
                lap,
                seq,
                hold_ms,
                reason,
                player,
                thrower,
                stage,
                stage_ms,
                sleep_stages,
                wake_stages,
                detail,
            } => tracing::info!(
                target: "fcvm::fleet::potato",
                round, size, at, lap, seq, hold_ms, %reason, ?player, ?thrower, %stage, stage_ms,
                %sleep_stages, %wake_stages, %detail,
                "potato round ended"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(at: u64, player: u32, kind: PotatoKind) -> PotatoEvent {
        PotatoEvent { at, player, kind }
    }

    #[test]
    fn a_sleep_line_names_its_start_end_and_stages() {
        let stages = BTreeMap::from([
            ("ask_ms".to_string(), 0.4),
            ("create_ms".to_string(), 99_120.0),
            ("prepare_ms".to_string(), 17.6),
            ("reap_ms".to_string(), 3.2),
        ]);
        let l = line(&event(
            1_762_490_000_000,
            14,
            PotatoKind::Slept {
                sleep_ms: 99_400.0,
                stages,
                written_mib: Some(20.0),
            },
        ));
        assert_eq!(
            l,
            Some(Line::Slept {
                player: 14,
                started_at: 1_762_489_900_600,
                ended_at: 1_762_490_000_000,
                sleep_ms: 99_400.0,
                stages: "ask 0.4, create 99120.0, prepare 17.6, reap 3.2".into(),
                written_mib: Some(20.0),
            })
        );
    }

    #[test]
    fn a_wake_line_names_its_start_and_end() {
        let l = line(&event(
            5_000,
            3,
            PotatoKind::Woke {
                wake_ms: 1_200.0,
                stages: BTreeMap::from([("restore_ms".to_string(), 900.0)]),
            },
        ));
        assert_eq!(
            l,
            Some(Line::Woke {
                player: 3,
                started_at: 3_800,
                ended_at: 5_000,
                wake_ms: 1_200.0,
                stages: "restore 900.0".into(),
            })
        );
    }

    #[test]
    fn a_round_that_ends_gets_one_line_naming_the_late_stage() {
        let mut s = State::new(fleet_core::Config::default(), 0);
        let before = Marks::of(&s);
        s.fleet.potato.last_drop = Some(fleet_core::potato::Drop {
            at: 9_000,
            round: 30,
            size: 16,
            reason: "not_listening".into(),
            player: Some(14),
            thrower: Some(13),
            seq: 13,
            lap: 0,
            hold_ms: 400,
            stage: "sleeping".into(),
            stage_ms: 99_400.0,
            listening_after_ms: None,
            sleep_stages: BTreeMap::from([("create_ms".to_string(), 1_500.0)]),
            wake_stages: BTreeMap::new(),
            detail: "player 14 was sleeping when player 13 threw".into(),
        });
        let notes_now = notes(&before, &s);
        assert_eq!(
            notes_now,
            vec![RoundNote::Ended {
                round: 30,
                size: 16,
                at: 9_000,
                lap: 0,
                seq: 13,
                hold_ms: 400,
                reason: "not_listening".into(),
                player: Some(14),
                thrower: Some(13),
                stage: "sleeping".into(),
                stage_ms: 99_400.0,
                sleep_stages: "create 1500.0".into(),
                wake_stages: String::new(),
                detail: "player 14 was sleeping when player 13 threw".into(),
            }]
        );
        // the same drop seen again is not a second end
        assert_eq!(notes(&Marks::of(&s), &s), vec![]);
    }

    /// A round that stops without a drop (a viewer's restart, or a new agent connection for
    /// the host) still gets an end line, before the next round's start line.
    #[test]
    fn a_round_that_stops_without_a_drop_gets_an_end_line() {
        let s = State::new(fleet_core::Config::default(), 0);
        let before = Marks {
            round: Some(7),
            drop: None,
        };
        assert_eq!(
            notes(&before, &s),
            vec![RoundNote::EndedWithoutDrop { round: 7 }]
        );
        // a round still going ends nothing
        assert_eq!(notes(&Marks::of(&s), &s), vec![]);
    }

    #[test]
    fn catches_and_throws_get_no_line() {
        let threw = PotatoKind::Threw {
            round: 1,
            seq: 2,
            to: 3,
            attempts: 1,
            wait_ms: 0.0,
            send_ms: 0.0,
        };
        assert_eq!(line(&event(1, 2, threw)), None);
        assert_eq!(line(&event(1, 2, PotatoKind::Waking {})), None);
    }
}
