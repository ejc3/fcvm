//! The daily spend ceiling across a coordinator restart and a UTC day boundary.

use fleet_core::spend::{DAY_MS, SPENT};
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
