//! The daily spend ceiling across a coordinator restart and a UTC day boundary.

use fleet_core::protocol::{AgentMsg, SlotReport};
use fleet_core::spend::{DAY_MS, SPENT};
use fleet_core::state::Gpu;
use fleet_core::{apply, view, Config, Event, Outcome, State};

const HOUR: u64 = 3_600_000;

fn config() -> Config {
    Config {
        power_managed: true,
        sleep_without_new_viewer_secs: 0,
        daily_spend_usd: Some(750.0),
        ..Config::default()
    }
}

fn burn(at: u64, usd_per_hour: f64) -> Event {
    Event::Burn {
        at,
        usd_per_hour,
        instances: 3,
        by_backing: Default::default(),
    }
}

fn joined(at: u64, viewer: &str) -> Event {
    Event::ViewerJoined {
        at,
        viewer: viewer.into(),
        client: Some(format!("198.51.100.{}", viewer.len())),
        returning: false,
    }
}

fn wake(at: u64, viewer: &str, id: &str) -> Event {
    Event::Intent {
        at,
        viewer: viewer.into(),
        client: None,
        id: id.into(),
        name: "wake".into(),
        args: serde_json::json!({}),
        check: None,
        secret: None,
    }
}

/// What a restarted coordinator resumes from: the stored state, read back.
fn restart(state: &State, at: u64) -> State {
    let stored = serde_json::to_vec(state).unwrap();
    let read: State = serde_json::from_slice(&stored).unwrap();
    read.resumed(config(), at)
}

fn spend(state: &State) -> serde_json::Value {
    serde_json::to_value(view(state)).unwrap()["fleet"]["spend"].clone()
}

#[test]
fn the_drill_expires_at_midnight_even_when_the_marker_cannot_be_read_after_a_restart() {
    let day = 20_001;
    let at = day * DAY_MS + 1000;
    let s = apply(
        State::new(config(), at),
        &Event::BudgetDrill { at, day: Some(day) },
    );
    assert_eq!(spend(&s)["ceiling_usd"], 1.0);
    let s = restart(&s, at + 1);
    assert_eq!(spend(&s)["ceiling_usd"], 1.0);
    let s = apply(s, &burn(at + 2, 8.0));
    assert_eq!(s.fleet.power.reason.as_deref(), Some("budget"));
    let s = apply(
        s,
        &Event::BudgetDrill {
            at: at + 3,
            day: None,
        },
    );
    assert_eq!(spend(&s)["ceiling_usd"], 750.0);
    assert_eq!(s.fleet.power.reason.as_deref(), Some("budget"));
    let s = apply(
        s,
        &Event::BudgetDrill {
            at: at + 4,
            day: Some(day),
        },
    );
    let next = (day + 1) * DAY_MS;
    let s = apply(s, &Event::Tick { at: next });
    assert_eq!(spend(&s)["ceiling_usd"], 750.0);
    assert_eq!(s.fleet.power.reason, None);
    let s = apply(
        s,
        &Event::BudgetDrill {
            at: next + 1,
            day: Some(day),
        },
    );
    assert_eq!(spend(&s)["ceiling_usd"], 750.0);
}

#[test]
fn the_drill_never_raises_the_configured_ceiling_or_enables_an_absent_meter() {
    for (base, expected) in [
        (None, None),
        (Some(0.25), Some(0.25)),
        (Some(750.0), Some(1.0)),
    ] {
        let c = Config {
            daily_spend_usd: base,
            ..config()
        };
        let s = apply(
            State::new(c, DAY_MS),
            &Event::BudgetDrill {
                at: DAY_MS,
                day: Some(1),
            },
        );
        assert_eq!(s.fleet.spend.ceiling(base, 1), expected);
    }
}

#[test]
fn the_days_spend_survives_a_restart() {
    let day = 20_000 * DAY_MS;
    let mut s = State::new(config(), day);
    s = apply(s, &joined(day + 1000, "v1"));
    s = apply(s, &burn(day + 1000, 60.0));
    s = apply(s, &burn(day + 5 * HOUR + 1000, 60.0));
    assert_eq!(spend(&s)["usd_today"], 300.0);

    // Down for an hour: the hosts kept running, so that hour is charged at the last rate.
    let mut s = restart(&s, day + 6 * HOUR + 1000);
    s = apply(s, &burn(day + 6 * HOUR + 1000, 60.0));
    assert_eq!(spend(&s)["usd_today"], 360.0);
    assert_eq!(s.fleet.power.reason, None);

    // 6 more hours at 60 USD: 720, and 15 minutes more at 60 USD an hour is 735, under 750.
    // The report of 120 USD an hour reaches 750 and trips it.
    s = apply(s, &burn(day + 12 * HOUR + 1000, 120.0));
    assert_eq!(spend(&s)["usd_today"], 720.0);
    assert_eq!(s.fleet.power.state, "asleep");
    assert_eq!(s.fleet.power.reason.as_deref(), Some("budget"));

    // A restart while tripped keeps it asleep, and a wake from a page that reconnects to
    // the new process is refused.
    let mut s = restart(&s, day + 18 * HOUR);
    s = apply(s, &joined(day + 18 * HOUR, "v2"));
    s = apply(s, &wake(day + 18 * HOUR + 1, "v2", "w1"));
    assert_eq!(s.fleet.power.state, "asleep");
    assert_eq!(s.fleet.power.reason.as_deref(), Some("budget"));
    let r = s.receipt("w1").unwrap();
    assert_eq!(
        (r.outcome, r.reason.as_deref()),
        (Outcome::Refused, Some(SPENT))
    );
    assert_eq!(spend(&s)["reason"], "budget");
}

#[test]
fn a_new_utc_day_starts_a_new_ledger_and_lets_the_fleet_wake() {
    let day = 20_001 * DAY_MS;
    let mut s = State::new(config(), day);
    s = apply(s, &joined(day + 1000, "v1"));
    s = apply(s, &burn(day + 1000, 3000.0));
    assert_eq!(s.fleet.power.reason.as_deref(), Some("budget"));
    // The hosts go; their last minutes are still billed into the next day.
    s = apply(s, &burn(day + 23 * HOUR, 2.0));
    s = apply(
        s,
        &Event::Tick {
            at: day + DAY_MS - 1,
        },
    );
    assert_eq!(s.fleet.power.state, "asleep");

    s = apply(
        s,
        &Event::Tick {
            at: day + DAY_MS + HOUR / 2,
        },
    );
    assert_eq!(s.fleet.power.reason, None);
    assert_eq!(s.fleet.power.state, "waking");
    let v = spend(&s);
    assert_eq!(v["usd_today"], 1.0);
    assert_eq!(v["reason"], serde_json::Value::Null);
    assert_eq!(v["day_start"], day + DAY_MS);
    s = apply(s, &wake(day + DAY_MS + HOUR, "v1", "w1"));
    assert_eq!(s.receipt("w1").unwrap().outcome, Outcome::Committed);
}

#[test]
fn a_tick_under_the_ceiling_keeps_the_version() {
    // Spend accrues at each meter report, not each second, so a burning fleet's ticks move
    // the version exactly as a fleet's without a ceiling do: no snapshot to every viewer or
    // stored-state write per second for the meter.
    let day = 20_002 * DAY_MS;
    let run = |config: Config| {
        let mut s = State::new(config, day);
        s = apply(s, &burn(day + 1000, 30.0));
        let (version, spend) = (s.version, s.fleet.spend.clone());
        for t in 2..60 {
            s = apply(s, &Event::Tick { at: day + t * 1000 });
        }
        let ticks = s.version - version;
        assert_eq!(s.fleet.spend, spend);
        // A report of nothing running, once it said so, leaves the ledger as it was.
        s = apply(s, &burn(day + 61_000, 0.0));
        let asleep = s.fleet.spend.clone();
        s = apply(s, &burn(day + 91_000, 0.0));
        assert_eq!(s.fleet.spend, asleep);
        ticks
    };
    let without = run(Config {
        daily_spend_usd: None,
        ..config()
    });
    assert_eq!(run(config()), without);
}

#[test]
fn a_claim_at_the_trip_point_is_refused_not_committed_and_undone() {
    // 8 USD spent at 4 USD an hour: the projection reaches the 10 USD ceiling 15 minutes
    // after the report, at the moment the claim arrives.
    let day = 20_003 * DAY_MS;
    let mut s = State::new(
        Config {
            max_tiles: 2,
            daily_spend_usd: Some(10.0),
            ..config()
        },
        day,
    );
    s = apply(
        s,
        &Event::Agent {
            at: day + 1000,
            host: "h1".into(),
            msg: AgentMsg::Hello {
                host: "h1".into(),
                arch: "x86_64".into(),
                gpus: (1..=3)
                    .map(|i| Gpu {
                        sbdf: format!("0000:0{i}:00.0"),
                        name: "Tesla T4".into(),
                    })
                    .collect(),
                capacity: 8,
                store_ready_at: None,
                boot: Default::default(),
                phones: vec![],
                phone_actions: vec![],
                potato_players: 0,
                potato_cpus: 0,
                potato_confined: false,
                windows: 0,
                windows_actions: Vec::new(),
                minecraft: 0,
                minecraft_server: None,
            },
        },
    );
    for n in 1..=3 {
        s = apply(
            s,
            &Event::Agent {
                at: day + 2000,
                host: "h1".into(),
                msg: AgentMsg::Slot(SlotReport {
                    id: format!("slot-{n}"),
                    phase: "running".into(),
                    generation: 1,
                    ..Default::default()
                }),
            },
        );
    }
    s = apply(s, &joined(day + 3000, "v1"));
    s = apply(s, &burn(day + 3000, 4.0));
    s = apply(s, &burn(day + 2 * HOUR + 3000, 4.0));
    assert_eq!(spend(&s)["usd_today"], 8.0);
    assert_eq!(s.fleet.power.state, "awake");
    s = apply(
        s,
        &Event::Intent {
            at: day + 2 * HOUR + 3000 + 15 * 60_000,
            viewer: "v1".into(),
            client: None,
            id: "c1".into(),
            name: "claim_gpu".into(),
            args: serde_json::json!({}),
            check: None,
            secret: Some("token-a".into()),
        },
    );
    let r = s.receipt("c1").unwrap();
    assert_eq!(
        (r.outcome, r.reason.as_deref()),
        (Outcome::Refused, Some(SPENT))
    );
    assert_eq!(s.fleet.power.reason.as_deref(), Some("budget"));
    assert!(s.fleet.last_claim_end.is_empty());
}
