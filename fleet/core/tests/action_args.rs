//! The recent-actions list is in every viewer's snapshot. An action carries what was parsed
//! and committed, never the name or args as sent.
use fleet_core::{apply, Config, Event, State};
use serde_json::json;

const MARK: &str = "ZQXMARKERZQX";

fn run(events: Vec<serde_json::Value>) -> String {
    let mut s = State::new(Config::default(), 0);
    for e in events {
        let e: Event = serde_json::from_value(e).unwrap();
        s = apply(s, &e);
    }
    serde_json::to_string(&fleet_core::view(&s)).unwrap()
}

fn intent(id: &str, name: &str, args: serde_json::Value) -> serde_json::Value {
    json!({"type": "intent", "at": 1_000, "viewer": "v1", "client": "198.51.100.7",
        "id": id, "name": name, "args": args,
        "check": {"verdict": "allow", "reason": "", "sky": []}})
}

#[test]
fn an_unknown_intents_name_reaches_no_snapshot() {
    let v = run(vec![intent("u-1", &format!("nope_{MARK}"), json!({}))]);
    assert!(!v.contains(MARK), "{v}");
}

#[test]
fn an_unknown_intents_args_reach_no_snapshot() {
    let v = run(vec![intent("u-1", "nope", json!({"k": MARK, MARK: 1}))]);
    assert!(!v.contains(MARK), "{v}");
}

#[test]
fn a_committed_words_extra_key_reaches_no_snapshot() {
    let v = run(vec![intent(
        "w-1",
        "word",
        json!({"text": "hello", "extra": MARK}),
    )]);
    assert!(v.contains("hello"), "{v}");
    assert!(!v.contains(MARK), "{v}");
}

#[test]
fn a_refused_known_intents_args_are_not_shown() {
    let v = run(vec![intent("r-1", "relaunch", json!({"tile": MARK}))]);
    let v: serde_json::Value = serde_json::from_str(&v).unwrap();
    assert_eq!(v["actions"][0]["args"], json!({}), "{v}");
}

#[test]
fn a_committed_intent_shows_only_its_own_keys_and_a_words_text_as_parsed() {
    let v = run(vec![
        intent("c-1", "claim_autopilot", json!({"on": true, "extra": MARK})),
        intent("c-2", "word", json!({"text": "a   b"})),
    ]);
    let v: serde_json::Value = serde_json::from_str(&v).unwrap();
    assert!(!v.to_string().contains(MARK), "{v}");
    let word = &v["actions"][1];
    assert_eq!(word["args"]["text"], "a b", "{word}");
}
