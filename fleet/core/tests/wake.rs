//! Which pages wake a sleeping fleet (`fleet_core::view::page_wakes`): an arrival always,
//! a connected page that turns active again only while `min_wake_remaining_secs` of the
//! limit without a new viewer remain. Such a wake is no arrival, so open tabs cannot hold
//! the fleet past the limit of the last arrival.

use fleet_core::{apply, Config, Event, State};

const HOUR: u64 = 3_600_000;
/// 1970-01-03, a Saturday.
const SAT: u64 = 2 * 86_400_000;
/// The default minimum: 600 s.
const MIN: u64 = 600_000;
/// The first viewer arrives here, so its limit (the default 3600 s) ends at `DEADLINE`.
const ARRIVED: u64 = SAT + 5000;
const DEADLINE: u64 = ARRIVED + HOUR;

fn config() -> Config {
    Config {
        power_managed: true,
        idle_scale_down_secs: 60,
        ..Config::default()
    }
}

fn tick(at: u64) -> Event {
    Event::Tick { at }
}

fn joined(at: u64, viewer: &str, returning: bool) -> Event {
    Event::ViewerJoined {
        at,
        viewer: viewer.into(),
        client: Some("198.51.100.7".into()),
        returning,
    }
}

fn visible(at: u64, viewer: &str, visible: bool) -> Event {
    Event::Visibility {
        at,
        viewer: viewer.into(),
        visible,
    }
}

fn wake(at: u64, viewer: &str) -> Event {
    Event::Intent {
        at,
        viewer: viewer.into(),
        client: None,
        id: format!("w-{at}"),
        name: "wake".into(),
        args: serde_json::json!({}),
        check: None,
        secret: None,
    }
}

fn run(s: State, events: &[Event]) -> State {
    events.iter().fold(s, apply)
}

/// Asleep since 66 s for want of an active viewer: `v1` arrived at 5 s and was hidden at 6 s.
fn asleep_under_a_hidden_page(config: Config) -> State {
    let s = run(
        State::new(config, SAT),
        &[
            joined(ARRIVED, "v1", false),
            visible(SAT + 6000, "v1", false),
            tick(SAT + 66_000),
        ],
    );
    assert_eq!(s.fleet.power.state, "asleep");
    s
}

#[test]
fn a_page_shown_again_wakes_the_fleet_only_with_the_minimum_left() {
    for (left, wakes) in [(MIN + 1, true), (MIN, true), (MIN - 1, false), (1, false)] {
        let s = asleep_under_a_hidden_page(config());
        let s = apply(s, &visible(DEADLINE - left, "v1", true));
        let want = if wakes { "waking" } else { "asleep" };
        assert_eq!(s.fleet.power.state, want, "{left} ms of the limit left");
        // Shown again is no arrival either way: the limit ends where it did.
        assert_eq!(s.fleet.power.last_arrival_at, Some(ARRIVED));
        // Asleep under the active page until its Wake button, which is an arrival.
        let at = DEADLINE - left + 1000;
        let s = run(s, &[tick(at), wake(at + 1, "v1")]);
        assert_eq!(s.fleet.power.state, "waking");
        if !wakes {
            assert_eq!(s.fleet.power.woke_at, Some(at + 1));
        }
        assert_eq!(s.fleet.power.last_arrival_at, Some(at + 1));
    }
}

#[test]
fn a_reconnecting_page_follows_the_same_minimum_and_a_new_viewer_always_wakes() {
    // Asleep with nobody connected: the one page left.
    let empty = |at: u64| {
        let s = asleep_under_a_hidden_page(config());
        apply(
            s,
            &Event::ViewerLeft {
                at,
                viewer: "v1".into(),
            },
        )
    };
    for (left, wakes) in [(MIN, true), (MIN - 1, false)] {
        let at = DEADLINE - left;
        let s = apply(empty(at - 1000), &joined(at, "v2", true));
        let want = if wakes { "waking" } else { "asleep" };
        assert_eq!(s.fleet.power.state, want, "{left} ms of the limit left");
        assert_eq!(s.fleet.power.last_arrival_at, Some(ARRIVED));
    }
    // A new viewer is an arrival: the whole limit is ahead of it, so it wakes the fleet a
    // millisecond before the old limit ends, and after it.
    for at in [DEADLINE - 1, DEADLINE + 5000] {
        let s = apply(empty(at - 1000), &joined(at, "v2", false));
        assert_eq!(s.fleet.power.state, "waking");
        assert_eq!(s.fleet.power.last_arrival_at, Some(at));
    }
}

#[test]
fn a_tab_hidden_and_shown_again_for_ever_holds_the_fleet_no_longer_than_its_arrival() {
    let mut s = asleep_under_a_hidden_page(config());
    let mut at = SAT + 120_000;
    let mut wakes = 0;
    // Shown for a minute, hidden for two (the idle limit is one), for three hours.
    while at < SAT + 3 * HOUR {
        s = run(s, &[visible(at, "v1", true), tick(at + 1000)]);
        if s.fleet.power.state != "asleep" {
            wakes += 1;
            assert!(
                at + MIN <= DEADLINE,
                "woke at {at} with less than the minimum left before {DEADLINE}"
            );
        }
        s = run(
            s,
            &[
                tick(at + 59_000),
                visible(at + 60_000, "v1", false),
                tick(at + 121_000),
            ],
        );
        assert!(
            s.fleet.power.state == "asleep" || at + 121_000 < DEADLINE,
            "up at {} after the limit of the one arrival, {DEADLINE}",
            at + 121_000
        );
        assert_eq!(s.fleet.power.last_arrival_at, Some(ARRIVED));
        at += 180_000;
    }
    assert!(wakes > 0, "the loop never woke the fleet");
}

#[test]
fn a_limit_shorter_than_the_minimum_leaves_arrivals_and_no_limit_leaves_every_page() {
    // A 300 s limit under the 600 s minimum: a page shown again never wakes it, an arrival does.
    let short = Config {
        sleep_without_new_viewer_secs: 300,
        ..config()
    };
    let s = asleep_under_a_hidden_page(short);
    let s = apply(s, &visible(SAT + 70_000, "v1", true));
    assert_eq!(s.fleet.power.state, "asleep");
    let s = apply(s, &joined(SAT + 71_000, "v2", false));
    assert_eq!(s.fleet.power.state, "waking");
    // No limit (0): nothing to have left, so any active page wakes it, as before.
    let none = Config {
        sleep_without_new_viewer_secs: 0,
        ..config()
    };
    let s = asleep_under_a_hidden_page(none);
    let s = apply(s, &visible(SAT + 10 * HOUR, "v1", true));
    assert_eq!(s.fleet.power.state, "waking");
    // A minimum of 0: any time before the limit ends, as before the minimum existed.
    let zero = Config {
        min_wake_remaining_secs: 0,
        ..config()
    };
    let s = asleep_under_a_hidden_page(zero.clone());
    let s = apply(s, &visible(DEADLINE - 1, "v1", true));
    assert_eq!(s.fleet.power.state, "waking");
    let s = asleep_under_a_hidden_page(zero);
    let s = apply(s, &visible(DEADLINE, "v1", true));
    assert_eq!(s.fleet.power.state, "asleep");
}

#[test]
fn a_warm_window_wakes_the_fleet_whatever_is_left_of_the_limit() {
    // The window opens at 00:55, with 5 min 5 s of the limit left.
    let warm = Config {
        warm_hours: vec!["Sat 00:55-02:00".parse().unwrap()],
        ..config()
    };
    let s = asleep_under_a_hidden_page(warm);
    let s = apply(s, &visible(SAT + 54 * 60_000, "v1", true));
    assert_eq!(
        s.fleet.power.state, "asleep",
        "6 min 5 s left, no window yet"
    );
    let s = apply(s, &tick(SAT + 55 * 60_000));
    assert_eq!(s.fleet.power.state, "waking");
    assert_eq!(s.fleet.power.reason.as_deref(), Some("warm"));
    assert_eq!(s.fleet.power.last_arrival_at, Some(ARRIVED));
}

#[test]
fn a_fleet_asleep_for_the_budget_stays_asleep_under_any_page() {
    let capped = Config {
        daily_spend_usd: Some(100.0),
        ..config()
    };
    let burn = |at: u64| Event::Burn {
        at,
        usd_per_hour: 6000.0,
        instances: 3,
        by_backing: Default::default(),
    };
    // Awake and visible, the day's 100 USD spent within two minutes.
    let s = run(
        State::new(capped, SAT),
        &[
            joined(ARRIVED, "v1", false),
            burn(SAT + 6000),
            burn(SAT + 126_000),
        ],
    );
    assert_eq!(s.fleet.power.state, "asleep");
    assert_eq!(s.fleet.power.reason.as_deref(), Some("budget"));
    // Most of the limit is left: a page shown again, a reconnecting page and a new viewer
    // all leave it asleep, as before.
    let s = run(
        s,
        &[
            visible(SAT + 130_000, "v1", false),
            visible(SAT + 140_000, "v1", true),
            joined(SAT + 150_000, "v2", true),
            joined(SAT + 160_000, "v3", false),
            tick(SAT + 170_000),
        ],
    );
    assert_eq!(s.fleet.power.state, "asleep");
    assert_eq!(s.fleet.power.reason.as_deref(), Some("budget"));
}
