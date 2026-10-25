//! Replayed vectors: each `tests/vectors/*.json` is a sequence of events and, after chosen
//! events, values the state must hold. The doc read by a check is
//! `{"version", "view", "receipts", "assign", "ec2_assign"}`, addressed by JSON pointer.
//!
//! A vector may also carry `final_view`, the whole browser view after the last event.
//! `FLEET_VECTORS_BLESS=1` rewrites `final_view` from the current code; review the diff.

use fleet_core::view::{ec2_assignment, host_assignments};
use fleet_core::{apply, view, Config, Event, State};
use serde_json::{json, Value};
use std::path::Path;

fn config_from(overrides: Option<&Value>) -> Config {
    let mut base = serde_json::to_value(Config::default()).unwrap();
    if let Some(Value::Object(o)) = overrides {
        for (k, v) in o {
            assert!(base.get(k).is_some(), "unknown config key {k}");
            base[k] = v.clone();
        }
    }
    serde_json::from_value(base).unwrap()
}

fn doc(state: &State) -> Value {
    json!({
        "version": state.version,
        "view": serde_json::to_value(view(state)).unwrap(),
        "receipts": serde_json::to_value(&state.receipts).unwrap(),
        "assign": serde_json::to_value(host_assignments(state)).unwrap(),
        "ec2_assign": serde_json::to_value(ec2_assignment(state)).unwrap(),
    })
}

fn run_vector(path: &Path) -> usize {
    let text = std::fs::read_to_string(path).unwrap();
    let mut vector: Value = serde_json::from_str(&text).unwrap();
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    let config = config_from(vector.get("config"));
    let start = vector["start_ms"].as_u64().unwrap_or(0);
    let events: Vec<Event> = serde_json::from_value(vector["events"].clone())
        .unwrap_or_else(|e| panic!("{name}: events do not parse: {e}"));
    let expects = vector["expect"].as_array().cloned().unwrap_or_default();
    let mut state = State::new(config.clone(), start);
    let mut checks = 0;
    let mut last_version = state.version;
    for (i, event) in events.iter().enumerate() {
        state = apply(state, event);
        assert!(
            state.version >= last_version,
            "{name}: version went back at event {i}"
        );
        last_version = state.version;
        let d = doc(&state);
        for e in expects
            .iter()
            .filter(|e| e["after"].as_u64() == Some(i as u64))
        {
            for (pointer, want) in e["checks"].as_object().expect("checks object") {
                let got = d.pointer(pointer).cloned().unwrap_or(Value::Null);
                assert_eq!(
                    &got,
                    want,
                    "{name}: after event {i} ({:?}), {pointer}",
                    e.get("note")
                );
                checks += 1;
            }
        }
    }
    // Replaying the same events from the start gives the same state.
    let replay = events.iter().fold(State::new(config, start), apply);
    assert_eq!(replay, state, "{name}: replay differs");

    let final_view = serde_json::to_value(view(&state)).unwrap();
    if std::env::var("FLEET_VECTORS_BLESS").as_deref() == Ok("1") {
        vector["final_view"] = final_view;
        std::fs::write(path, serde_json::to_string_pretty(&vector).unwrap() + "\n").unwrap();
    } else {
        assert_eq!(
            vector.get("final_view"),
            Some(&final_view),
            "{name}: final view differs (FLEET_VECTORS_BLESS=1 rewrites it)"
        );
    }
    checks
}

#[test]
fn replayed_vectors_hold() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vectors");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    assert!(
        paths.len() >= 8,
        "expected the vector set, found {}",
        paths.len()
    );
    for p in &paths {
        let checks = run_vector(p);
        assert!(checks > 0, "{} checks nothing", p.display());
        println!("{}: {checks} checks", p.display());
    }
}
