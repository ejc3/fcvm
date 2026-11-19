//! What a coordinator restart keeps (`State::resumed`): the open stop vote keeps the votes it
//! answered.

use fleet_core::{apply, Config, Event, Outcome, State};
use serde_json::json;

fn ev(v: serde_json::Value) -> Event {
    serde_json::from_value(v).unwrap()
}

fn joined(at: u64, viewer: &str) -> Event {
    ev(json!({"at": at, "type": "viewer_joined", "viewer": viewer, "client": "198.51.100.20"}))
}

fn intent(at: u64, viewer: &str, id: &str, name: &str, args: serde_json::Value) -> Event {
    ev(
        json!({"at": at, "type": "intent", "viewer": viewer, "client": "198.51.100.20",
        "id": id, "name": name, "args": args}),
    )
}

fn run(s: State, events: Vec<Event>) -> State {
    events.iter().fold(s, apply)
}

#[test]
fn a_restart_keeps_the_votes_of_the_open_round() {
    let s = State::new(Config::default(), 0);
    let target = fleet_core::director::viewpoints()[s.fleet.director.offer[0]].name;
    let s = run(
        s,
        vec![
            joined(500, "v1"),
            intent(1000, "v1", "n1", "nudge", json!({"target": target})),
        ],
    );
    assert_eq!(s.receipts["n1"].outcome, Outcome::Committed);
    let before = s.fleet.director.clone();
    assert_eq!(before.votes.len(), 1);
    let stored: State = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    let s = stored.resumed(Config::default(), 2000);
    assert_eq!(
        s.fleet.director.votes, before.votes,
        "the answered vote is kept"
    );
    assert_eq!(s.fleet.director.offer, before.offer);
    // The round closes on that vote.
    let decide = s.fleet.director.decide_at();
    let s = apply(
        s,
        &Event::Tick {
            at: (decide * 1000.0) as u64 + 1,
        },
    );
    assert_eq!(
        s.fleet.director.next,
        Some(before.offer[0]),
        "the voted stop is next"
    );
}
