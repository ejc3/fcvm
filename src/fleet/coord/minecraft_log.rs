//! One log line per Minecraft fork (target `fcvm::fleet::minecraft`): when the coordinator
//! asks for it, and when it ends, with the agent's times when the copy reached `running` or
//! with the reason it did not (the target's generation moved on, or the target left).
//!
//! [`Marks`] is taken before an event is applied, and [`notes`] lists what the event changed,
//! as in [`super::wake_log`].

use fleet_core::minecraft::LastFork;
use fleet_core::State;
use std::collections::BTreeMap;

/// A pending fork: its source, the click and who made it.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub from: String,
    pub clicked_at: u64,
    pub by: String,
}

/// The forks pending, by target and generation, and the last one that ran.
#[derive(Debug, Clone, Default)]
pub struct Marks {
    pending: BTreeMap<(String, u64), Pending>,
    last: Option<LastFork>,
}

impl Marks {
    pub fn of(s: &State) -> Marks {
        let f = &s.fleet;
        Marks {
            pending: f
                .minecraft
                .values()
                .filter_map(|s| {
                    let k = s.minecraft.as_ref()?.fork.as_ref()?;
                    Some((
                        (s.id.clone(), k.generation),
                        Pending {
                            from: k.slot.clone(),
                            clicked_at: k.clicked_at,
                            by: k.by.clone(),
                        },
                    ))
                })
                .collect(),
            last: f.last_minecraft_fork.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Note {
    Started {
        to: String,
        generation: u64,
        fork: Pending,
    },
    Ran(LastFork),
    /// A pending fork that ended without the copy running: `phase` is the target's last
    /// reported phase and `error` its error, or the target is gone.
    Dropped {
        to: String,
        generation: u64,
        fork: Pending,
        phase: Option<String>,
        error: Option<String>,
    },
}

pub fn notes(before: &Marks, after: &State) -> Vec<Note> {
    let now = Marks::of(after);
    let mut out = Vec::new();
    for ((to, generation), fork) in &now.pending {
        if !before.pending.contains_key(&(to.clone(), *generation)) {
            out.push(Note::Started {
                to: to.clone(),
                generation: *generation,
                fork: fork.clone(),
            });
        }
    }
    let ran = now
        .last
        .as_ref()
        .filter(|l| before.last.as_ref() != Some(*l));
    for ((to, generation), fork) in &before.pending {
        if now.pending.contains_key(&(to.clone(), *generation)) {
            continue;
        }
        if ran
            .is_some_and(|l| &l.to == to && l.from == fork.from && l.clicked_at == fork.clicked_at)
        {
            continue;
        }
        let target = after.fleet.minecraft.values().find(|s| &s.id == to);
        out.push(Note::Dropped {
            to: to.clone(),
            generation: *generation,
            fork: fork.clone(),
            phase: target.map(|s| s.observed.phase.clone()),
            error: target.and_then(|s| s.observed.error.clone()),
        });
    }
    if let Some(l) = ran {
        out.push(Note::Ran(l.clone()));
    }
    out
}

pub fn log(notes: &[Note]) {
    for n in notes {
        match n {
            Note::Started {
                to,
                generation,
                fork,
            } => tracing::info!(
                target: "fcvm::fleet::minecraft",
                from = %fork.from, %to, generation, by = %fork.by, clicked_at = fork.clicked_at,
                "fork asked for"
            ),
            Note::Ran(l) => tracing::info!(
                target: "fcvm::fleet::minecraft",
                from = %l.from, to = %l.to, by = %l.by, clicked_at = l.clicked_at,
                first_chunk_at = ?l.first_chunk_at, snapshot_ms = ?l.snapshot_ms,
                restore_ms = ?l.restore_ms, total_ms = ?l.total_ms,
                "fork running"
            ),
            Note::Dropped {
                to,
                generation,
                fork,
                phase,
                error,
            } => tracing::warn!(
                target: "fcvm::fleet::minecraft",
                from = %fork.from, %to, generation, by = %fork.by, clicked_at = fork.clicked_at,
                phase = ?phase, error = ?error,
                "fork ended before its copy ran"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_core::{apply, Config, Event};
    use serde_json::json;

    fn ev(v: serde_json::Value) -> Event {
        serde_json::from_value(v).unwrap()
    }

    fn running(at: u64, id: &str, generation: u64, timings: serde_json::Value) -> Event {
        ev(
            json!({"type": "agent", "at": at, "host": "h1", "msg": {"type": "slot", "id": id,
            "phase": "running", "generation": generation, "timings": timings,
            "first_chunk_at": at - 100}}),
        )
    }

    fn fork(at: u64, id: &str, slot: &str) -> Event {
        ev(
            json!({"type": "intent", "at": at, "viewer": "v1", "client": "198.51.100.9",
            "id": id, "name": "minecraft_fork", "args": {"slot": slot}}),
        )
    }

    /// Apply `e` and return what the log says about it.
    fn step(s: State, e: Event) -> (State, Vec<Note>) {
        let marks = Marks::of(&s);
        let s = apply(s, &e);
        let n = notes(&marks, &s);
        (s, n)
    }

    #[test]
    fn a_fork_is_logged_when_asked_for_and_when_its_copy_runs_or_is_dropped() {
        let mut s = State::new(Config::default(), 0);
        for e in [
            ev(
                json!({"type": "agent", "at": 1000, "host": "h1", "msg": {"type": "hello",
                "host": "h1", "arch": "aarch64", "gpus": [], "capacity": 0, "minecraft": 2}}),
            ),
            running(2000, "mc-1", 1, json!({})),
            running(2000, "mc-2", 1, json!({})),
        ] {
            s = apply(s, &e);
        }
        let (s, n) = step(s, fork(3000, "f1", "mc-1"));
        assert_eq!(
            n,
            vec![Note::Started {
                to: "mc-2".into(),
                generation: 2,
                fork: Pending {
                    from: "mc-1".into(),
                    clicked_at: 3000,
                    by: "viewer:v1".into()
                }
            }]
        );
        let (s, n) = step(
            s,
            running(
                5000,
                "mc-2",
                2,
                json!({"snapshot_ms": 600, "restore_ms": 900, "total_ms": 1900}),
            ),
        );
        let [Note::Ran(l)] = n.as_slice() else {
            panic!("{n:?}")
        };
        assert_eq!((l.from.as_str(), l.to.as_str()), ("mc-1", "mc-2"));
        assert_eq!(
            (l.snapshot_ms, l.restore_ms, l.total_ms, l.first_chunk_at),
            (Some(600), Some(900), Some(1900), Some(4900))
        );
        // Another fork, and a reset of its target before the copy runs.
        let (s, n) = step(s, fork(14000, "f2", "mc-2"));
        assert!(matches!(n.as_slice(), [Note::Started { to, .. }] if to == "mc-1"));
        let (s, n) = step(
            s,
            ev(json!({"type": "intent", "at": 14500, "viewer": "v2",
                "client": "198.51.100.10", "id": "r1", "name": "minecraft_reset",
                "args": {"slot": "mc-1"}})),
        );
        assert!(
            matches!(n.as_slice(), [Note::Dropped { to, generation: 2, phase: Some(p), .. }]
                if to == "mc-1" && p == "running"),
            "{n:?}"
        );
        // Another, then the target's host leaves before the copy runs.
        let s = apply(s, &running(15000, "mc-1", 3, json!({})));
        let (s, n) = step(s, fork(24000, "f3", "mc-2"));
        assert!(matches!(n.as_slice(), [Note::Started { to, .. }] if to == "mc-1"));
        let (_, n) = step(
            s,
            ev(json!({"type": "agent_gone", "at": 25000, "host": "h1"})),
        );
        assert!(
            matches!(n.as_slice(), [Note::Dropped { to, phase: None, .. }] if to == "mc-1"),
            "{n:?}"
        );
    }
}
