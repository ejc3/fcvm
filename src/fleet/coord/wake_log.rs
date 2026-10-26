//! One log line per step of a wake, so each wake's breakdown is in the coordinator's log:
//! the wake starting, each GPU host's phases (instance pending and in service, its own boot
//! times from the agent's hello, the agent's hello, every slot rendering), each tile and
//! slot that reports running (with the timings it reported), and the fleet turning awake.
//! Times are milliseconds since the wake began (`since_wake_ms`); a host's boot times are on
//! the host's clock.
//!
//! [`Marks`] is what the log compares against: taken before an event is applied, and
//! [`notes`] lists what the event changed.

use fleet_core::state::HostWake;
use fleet_core::State;
use std::collections::BTreeMap;

/// The parts of the state the wake log follows.
#[derive(Debug, Clone, Default)]
pub struct Marks {
    state: String,
    woke_at: Option<u64>,
    hosts: BTreeMap<String, HostWake>,
    wakes: usize,
    /// Each slot and tile: (phase, generation).
    slots: BTreeMap<String, (String, u64)>,
}

impl Marks {
    pub fn of(s: &State) -> Marks {
        let f = &s.fleet;
        Marks {
            state: f.power.state.clone(),
            woke_at: f.power.woke_at,
            hosts: f.power.hosts.clone(),
            wakes: f.power.wakes.len(),
            slots: f
                .slots
                .values()
                .chain(f.ec2.values())
                .map(|s| {
                    (
                        s.id.clone(),
                        (s.observed.phase.clone(), s.observed.generation),
                    )
                })
                .collect(),
        }
    }
}

impl Marks {
    /// `fleet_core::apply::wake_open` of the marked state.
    fn wake_open(&self) -> bool {
        self.woke_at.is_some()
            && (self.state == "waking"
                || self.hosts.is_empty()
                || self.hosts.values().any(|w| w.rendering_at.is_none()))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Note {
    WakeStarted,
    /// A host reached `phase` (`instance_pending`, `booted`, `agent_connected`,
    /// `tiles_rendering`).
    HostPhase {
        host: String,
        phase: &'static str,
        since_wake_ms: u64,
    },
    /// The host's own boot times from its hello, each since the wake began; `store_ready`
    /// is included.
    HostBoot {
        host: String,
        steps: BTreeMap<String, u64>,
    },
    /// A slot or tile reported running a new generation.
    Running {
        id: String,
        generation: u64,
        since_wake_ms: Option<u64>,
        timings: BTreeMap<String, u64>,
    },
    Awake {
        since_wake_ms: u64,
        gpus: usize,
    },
}

/// What applying one event changed, for the log; `at` is the event's time.
pub fn notes(before: &Marks, after: &State, at: u64) -> Vec<Note> {
    let p = &after.fleet.power;
    let mut out = Vec::new();
    if before.state != "waking" && p.state == "waking" {
        out.push(Note::WakeStarted);
    }
    let since = |t: u64| p.woke_at.map(|w| t.saturating_sub(w));
    if let Some(woke) = p.woke_at {
        for (host, w) in &p.hosts {
            let old = before.hosts.get(host);
            let phases: [(&'static str, Option<u64>, Option<u64>); 4] = [
                (
                    "instance_pending",
                    w.pending_at,
                    old.and_then(|o| o.pending_at),
                ),
                ("booted", w.booted_at, old.and_then(|o| o.booted_at)),
                ("agent_connected", w.agent_at, old.and_then(|o| o.agent_at)),
                (
                    "tiles_rendering",
                    w.rendering_at,
                    old.and_then(|o| o.rendering_at),
                ),
            ];
            for (phase, now, was) in phases {
                if let (Some(t), None) = (now, was) {
                    out.push(Note::HostPhase {
                        host: host.clone(),
                        phase,
                        since_wake_ms: t.saturating_sub(woke),
                    });
                }
                // The host's boot comes with its hello.
                if phase == "agent_connected" && now.is_some() && was.is_none() {
                    let mut steps: BTreeMap<String, u64> = w
                        .boot
                        .iter()
                        .map(|(k, t)| (k.clone(), t.saturating_sub(woke)))
                        .collect();
                    if let Some(t) = w.store_ready_at {
                        steps.insert("store_ready_at".into(), t.saturating_sub(woke));
                    }
                    if !steps.is_empty() {
                        out.push(Note::HostBoot {
                            host: host.clone(),
                            steps,
                        });
                    }
                }
            }
        }
    }
    let f = &after.fleet;
    for s in f.slots.values().chain(f.ec2.values()) {
        let o = &s.observed;
        if o.phase != "running" {
            continue;
        }
        let was = before.slots.get(&s.id);
        if was.is_some_and(|(ph, g)| ph == "running" && *g == o.generation) {
            continue;
        }
        out.push(Note::Running {
            id: s.id.clone(),
            generation: o.generation,
            since_wake_ms: (before.wake_open() || fleet_core::apply::wake_open(p))
                .then(|| since(at))
                .flatten(),
            timings: o.timings.clone(),
        });
    }
    if p.wakes.len() > before.wakes || (before.state == "waking" && p.state == "awake") {
        if let Some(w) = p.wakes.back() {
            out.push(Note::Awake {
                since_wake_ms: w.ms,
                gpus: w.gpus,
            });
        }
    }
    out
}

/// Log each note at info.
pub fn log(notes: &[Note]) {
    for n in notes {
        match n {
            Note::WakeStarted => tracing::info!("wake: started"),
            Note::HostPhase {
                host,
                phase,
                since_wake_ms,
            } => tracing::info!(%host, phase, since_wake_ms, "wake: host phase"),
            Note::HostBoot { host, steps } => {
                let steps = steps
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                tracing::info!(%host, %steps, "wake: host boot, ms since the wake began (host clock)")
            }
            Note::Running {
                id,
                generation,
                since_wake_ms,
                timings,
            } => {
                let timings = timings
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                match since_wake_ms {
                    Some(ms) => {
                        tracing::info!(tile = %id, generation, since_wake_ms = ms, %timings, "wake: tile running")
                    }
                    None => tracing::info!(tile = %id, generation, %timings, "tile running"),
                }
            }
            Note::Awake {
                since_wake_ms,
                gpus,
            } => tracing::info!(
                since_wake_ms,
                gpus,
                "wake: fleet awake (every placed tile rendering)"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_core::{apply, Config, Event};

    fn config() -> Config {
        Config {
            power_managed: true,
            idle_scale_down_secs: 60,
            ec2_max: 1,
            ..Config::default()
        }
    }

    fn step(s: State, e: serde_json::Value) -> (State, Vec<Note>) {
        let e: Event = serde_json::from_value(e).unwrap();
        let marks = Marks::of(&s);
        let at = match &e {
            Event::Tick { at } => *at,
            _ => serde_json::to_value(&e).unwrap()["at"].as_u64().unwrap(),
        };
        let s = apply(s, &e);
        let n = notes(&marks, &s, at);
        (s, n)
    }

    /// One line per step of a wake, each once, timed from the wake's start.
    #[test]
    fn a_wake_logs_each_step_once_with_its_time_since_the_wake_began() {
        let s = State::new(config(), 0);
        let (s, _) = step(
            s,
            serde_json::json!({"type": "viewer_joined", "at": 1000, "viewer": "v", "client": "192.0.2.1"}),
        );
        let (s, _) = step(
            s,
            serde_json::json!({"type": "visibility", "at": 1100, "viewer": "v", "visible": false}),
        );
        let (s, _) = step(s, serde_json::json!({"type": "tick", "at": 62000}));
        assert_eq!(s.fleet.power.state, "asleep");
        let (s, n) = step(
            s,
            serde_json::json!({"type": "visibility", "at": 100000, "viewer": "v", "visible": true}),
        );
        assert_eq!(n, vec![Note::WakeStarted]);
        let pending = serde_json::json!({"type": "scale_instances", "at": 116000, "instances": [{"id": "h1", "lifecycle": "Pending"}]});
        let (s, n) = step(s, pending.clone());
        assert_eq!(
            n,
            vec![Note::HostPhase {
                host: "h1".into(),
                phase: "instance_pending",
                since_wake_ms: 16000
            }]
        );
        // The same poll again logs nothing.
        let mut again = pending;
        again["at"] = 121000.into();
        let (s, n) = step(s, again);
        assert!(n.is_empty(), "{n:?}");
        let (s, n) = step(
            s,
            serde_json::json!({"type": "ec2", "at": 150000, "report": {"id": "ec2-1", "phase": "running", "generation": 1, "vm": "i", "frame_url": "ws://e/frames", "timings": {"launch_ms": 900, "total_ms": 49000}}}),
        );
        assert_eq!(
            n,
            vec![
                Note::Running {
                    id: "ec2-1".into(),
                    generation: 1,
                    since_wake_ms: Some(50000),
                    timings: [
                        ("launch_ms".to_string(), 900),
                        ("total_ms".to_string(), 49000)
                    ]
                    .into(),
                },
                Note::Awake {
                    since_wake_ms: 50000,
                    gpus: 1
                },
            ]
        );
        let (s, n) = step(
            s,
            serde_json::json!({"type": "agent", "at": 230000, "host": "h1", "msg": {
            "type": "hello", "host": "h1", "arch": "x86_64", "capacity": 1,
            "gpus": [{"sbdf": "0000:01:00.0", "name": "Tesla T4"}],
            "store_ready_at": 229000, "boot": {"kernel_started_at": 160000}}}),
        );
        assert_eq!(
            n,
            vec![
                Note::HostPhase {
                    host: "h1".into(),
                    phase: "agent_connected",
                    since_wake_ms: 130000
                },
                Note::HostBoot {
                    host: "h1".into(),
                    steps: [
                        ("kernel_started_at".to_string(), 60000),
                        ("store_ready_at".to_string(), 129000)
                    ]
                    .into(),
                },
            ]
        );
        let (_, n) = step(
            s,
            serde_json::json!({"type": "agent", "at": 240000, "host": "h1", "msg": {
            "type": "slot", "id": "slot-1", "phase": "running", "generation": 1, "vm": "fleet-slot-1",
            "frame_url": "ws://h1:1/frames", "timings": {"total_ms": 9000}}}),
        );
        assert_eq!(
            n,
            vec![
                Note::HostPhase {
                    host: "h1".into(),
                    phase: "tiles_rendering",
                    since_wake_ms: 140000
                },
                Note::Running {
                    id: "slot-1".into(),
                    generation: 1,
                    since_wake_ms: Some(140000),
                    timings: [("total_ms".to_string(), 9000)].into(),
                },
            ]
        );
    }
}
