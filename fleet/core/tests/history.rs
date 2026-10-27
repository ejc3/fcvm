//! System words: the idle-gap schedule, viewer words winning over them, and the rotation.

use fleet_core::history;
use fleet_core::scene::{TextStyle, Word, MAX_WORD_CHARS};
use fleet_core::state::Outcome;
use fleet_core::{apply, view, Config, Event, State};
use std::collections::BTreeSet;

const GAP_SECS: u64 = 60;
const GAP_MS: u64 = GAP_SECS * 1000;

fn config() -> Config {
    Config {
        history_idle_secs: GAP_SECS,
        ..Config::default()
    }
}

fn joined(at: u64, viewer: &str) -> Event {
    Event::ViewerJoined {
        at,
        viewer: viewer.into(),
        client: Some(format!("198.51.100.{}", viewer.len())),
    }
}

fn tick(at: u64) -> Event {
    Event::Tick { at }
}

/// A viewer's word, checked against `sky`.
fn word(at: u64, viewer: &str, id: &str, text: &str, sky: &[&str]) -> Event {
    serde_json::from_value(serde_json::json!({
        "type": "intent", "at": at, "viewer": viewer, "client": format!("198.51.100.{}", viewer.len()),
        "id": id, "name": "word", "args": {"text": text},
        "check": {"verdict": "allow", "reason": "ok", "sky": sky},
    }))
    .unwrap()
}

/// Tick once a second from `from` to `to` (inclusive), returning the state and the time of
/// the first tick at which `stop` held.
fn run(
    mut s: State,
    from: u64,
    to: u64,
    step: u64,
    stop: impl Fn(&State) -> bool,
) -> (State, Option<u64>) {
    let mut at = from;
    while at <= to {
        s = apply(s, &tick(at));
        if stop(&s) {
            return (s, Some(at));
        }
        at += step;
    }
    (s, None)
}

fn system_words(s: &State) -> Vec<&Word> {
    s.fleet.scene.words.iter().filter(|w| w.system).collect()
}

fn newest(s: &State) -> &Word {
    s.fleet.scene.words.last().expect("a word")
}

#[test]
fn a_system_word_appears_after_the_idle_gap_when_nobody_types() {
    let mut s = State::new(config(), 0);
    s = apply(s, &joined(1_000, "v1"));
    // The first tick with a viewer starts the gap.
    s = apply(s, &tick(2_000));
    assert!(s.fleet.scene.words.is_empty());
    let (s, at) = run(s, 3_000, 200_000, 1_000, |s| {
        !s.fleet.scene.words.is_empty()
    });
    assert_eq!(at, Some(2_000 + GAP_MS));
    let w = newest(&s);
    assert!(w.system);
    assert_eq!(w.t, 62.0);
    assert!(history::lines().contains(&w.text.as_str()));
    // Marked on the wire and in the view, no receipt, no action crediting anyone.
    let v = serde_json::to_value(view(&s)).unwrap();
    assert_eq!(v["scene"]["words"][0]["system"], true);
    assert!(s.receipts.is_empty());
    assert!(s.fleet.actions.is_empty());
}

#[test]
fn nothing_is_written_to_an_empty_wall_or_when_the_setting_is_off() {
    let (s, at) = run(State::new(config(), 0), 1_000, 3_600_000, 1_000, |s| {
        !s.fleet.scene.words.is_empty()
    });
    assert_eq!(at, None, "no viewer, no word");
    assert_eq!(s.fleet.history_count, 0);
    let mut off = State::new(Config::default(), 0);
    off = apply(off, &joined(1_000, "v1"));
    let (off, at) = run(off, 2_000, 1_000_000, 1_000, |s| {
        !s.fleet.scene.words.is_empty()
    });
    assert_eq!(at, None);
    assert!(off.fleet.scene.words.is_empty());
    // A hidden page is not watching either.
    let mut hidden = State::new(config(), 0);
    hidden = apply(hidden, &joined(1_000, "v1"));
    hidden = apply(
        hidden,
        &Event::Visibility {
            at: 1_500,
            viewer: "v1".into(),
            visible: false,
        },
    );
    let (hidden, at) = run(hidden, 2_000, 1_000_000, 1_000, |s| {
        !s.fleet.scene.words.is_empty()
    });
    assert_eq!(at, None);
    assert!(hidden.fleet.scene.words.is_empty());
}

#[test]
fn a_viewer_word_preempts_and_delays_the_next_system_word() {
    let mut s = State::new(config(), 0);
    s = apply(s, &joined(1_000, "v1"));
    s = apply(s, &tick(2_000));
    let (s, at) = run(s, 3_000, 200_000, 1_000, |s| {
        !s.fleet.scene.words.is_empty()
    });
    let first_at = at.unwrap();
    // The viewer types while the system word is up: theirs is the newest, so it is the
    // one drawn, and it commits (the system word is not part of the sky it was checked in).
    let typed = first_at + 5_000;
    let s = apply(s, &word(typed, "v1", "w1", "hello", &[]));
    assert_eq!(s.receipt("w1").map(|r| r.outcome), Some(Outcome::Committed));
    let w = newest(&s);
    assert_eq!(
        (w.text.as_str(), w.system, w.t),
        ("hello", false, typed as f64 / 1000.0)
    );
    // A system word does not return until the viewer's word has been up for its hold
    // (lockup: 90 s) and the gap has passed.
    let ready = typed + history::hold_ms(TextStyle::Lockup) + GAP_MS;
    let (s, at) = run(s, typed + 1_000, ready + 10_000, 1_000, |s| {
        newest(s).system
    });
    assert_eq!(at, Some(ready));
    assert_eq!(system_words(&s).len(), 1, "one system word at a time");
}

#[test]
fn the_rotation_shows_every_line_before_repeating() {
    let n = history::lines().len();
    let seed = config().camera_seed;
    let mut s = State::new(config(), 0);
    s = apply(s, &joined(1_000, "v1"));
    let mut shown: Vec<String> = Vec::new();
    let mut styles = Vec::new();
    let mut at = 2_000;
    while shown.len() < n + 3 {
        s = apply(s, &tick(at));
        if let Some(w) = system_words(&s).first() {
            if shown.len() as u64 != s.fleet.history_count {
                shown.push(w.text.clone());
                styles.push(w.text_style);
            }
        }
        at += 5_000;
    }
    assert_eq!(s.fleet.history_count, shown.len() as u64);
    let first_pass: BTreeSet<_> = shown[..n].iter().collect();
    assert_eq!(
        first_pass.len(),
        n,
        "a line repeated before the list ran out"
    );
    let listed: BTreeSet<&str> = history::lines().iter().copied().collect();
    let shown_first: BTreeSet<&str> = shown[..n].iter().map(String::as_str).collect();
    assert_eq!(shown_first, listed);
    // The next pass starts with a different line than the last one ended on.
    assert_ne!(shown[n - 1], shown[n]);
    // The order is the module's, which depends on the seed alone.
    for (i, text) in shown.iter().enumerate() {
        assert_eq!(text, history::lines()[history::index(seed, i as u64, n)]);
    }
    // All three styles are used.
    assert_eq!(
        styles
            .iter()
            .map(|s| format!("{s:?}"))
            .collect::<BTreeSet<_>>()
            .len(),
        3,
        "{styles:?}"
    );
}

#[test]
fn a_replay_of_the_same_events_gives_the_same_words() {
    let mut events = vec![joined(1_000, "v1")];
    events.extend((2..2_000u64).map(|i| tick(i * 1_000)));
    events.insert(500, word(400_000, "v1", "w1", "hi", &[]));
    events.sort_by_key(|e| e.at());
    let a = events.iter().fold(State::new(config(), 0), apply);
    let b = events.iter().fold(State::new(config(), 0), apply);
    assert_eq!(a, b);
    assert!(a.fleet.history_count > 3);
}

#[test]
fn every_line_fits_the_word_limit_in_a_scene() {
    for l in history::lines() {
        assert!(l.chars().count() <= MAX_WORD_CHARS, "{l:?}");
    }
}
