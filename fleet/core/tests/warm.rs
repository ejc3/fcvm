//! Warm hours across a coordinator restart: nothing about a window is stored, so the
//! restarted coordinator decides from the windows it was started with.

use fleet_core::{apply, Config, Event, State};

const HOUR: u64 = 3_600_000;
/// 1970-01-03, a Saturday.
const SAT: u64 = 2 * 86_400_000;

fn config(warm: &[&str]) -> Config {
    Config {
        power_managed: true,
        idle_scale_down_secs: 60,
        sleep_without_new_viewer_secs: 0,
        warm_hours: warm.iter().map(|w| w.parse().unwrap()).collect(),
        ..Config::default()
    }
}

fn tick(at: u64) -> Event {
    Event::Tick { at }
}

/// What a restarted coordinator resumes from: the stored state, read back.
fn restart(state: &State, config: Config, at: u64) -> State {
    let stored = serde_json::to_vec(state).unwrap();
    let read: State = serde_json::from_slice(&stored).unwrap();
    read.resumed(config, at)
}

#[test]
fn a_restart_inside_a_window_decides_from_the_flag_it_starts_with() {
    let warm = config(&["Sat 17:00-23:00"]);
    let mut s = State::new(warm.clone(), SAT);
    s = apply(s, &tick(SAT + 1000));
    s = apply(s, &tick(SAT + 2 * 60_000));
    assert_eq!(s.fleet.power.state, "asleep");
    s = apply(s, &tick(SAT + 17 * HOUR));
    assert_eq!(s.fleet.power.state, "waking");
    assert_eq!(s.fleet.power.reason.as_deref(), Some("warm"));

    // Restarted with the same windows an hour later, no viewer: it stays up.
    let mut kept = restart(&s, warm, SAT + 18 * HOUR);
    kept = apply(kept, &tick(SAT + 18 * HOUR));
    assert_eq!(kept.fleet.power.state, "waking");
    assert_eq!(kept.fleet.power.reason.as_deref(), Some("warm"));

    // Restarted without them, nothing stored keeps it up: the viewer rules decide, once
    // the pages of the old process have had their time to reconnect.
    let mut cold = restart(&s, config(&[]), SAT + 18 * HOUR);
    cold = apply(cold, &tick(SAT + 18 * HOUR));
    assert_eq!(cold.fleet.power.state, "waking");
    assert_eq!(cold.fleet.power.reason, None);
    let grace = fleet_core::state::RESUME_GRACE_MS;
    cold = apply(cold, &tick(SAT + 18 * HOUR + grace - 1));
    assert_eq!(cold.fleet.power.state, "waking");
    cold = apply(cold, &tick(SAT + 18 * HOUR + grace));
    assert_eq!(cold.fleet.power.state, "asleep");
    assert_eq!(cold.fleet.power.reason, None);
}

#[test]
fn the_windows_are_stored_in_the_config_as_written() {
    let c = config(&["sat 17:00-23:00", "Sun 22:00-02:00"]);
    let v = serde_json::to_value(&c).unwrap();
    assert_eq!(
        v["warm_hours"],
        serde_json::json!(["Sat 17:00-23:00", "Sun 22:00-02:00"])
    );
    let back: Config = serde_json::from_value(v).unwrap();
    assert_eq!(back, c);
}
