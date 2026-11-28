//! What a coordinator restart keeps (`State::resumed`): a GPU claim waits for its page to
//! present the claim's token, and the open stop vote keeps the votes it answered.

use fleet_core::state::CLAIM_RESUME_MS;
use fleet_core::view::host_assignments;
use fleet_core::{apply, Config, Event, Outcome, State};
use serde_json::json;

fn config() -> Config {
    let mut base = serde_json::to_value(Config::default()).unwrap();
    base["max_tiles"] = json!(2);
    base["claim_secs"] = json!(600);
    serde_json::from_value(base).unwrap()
}

fn ev(v: serde_json::Value) -> Event {
    serde_json::from_value(v).unwrap()
}

fn hello(at: u64) -> Event {
    ev(
        json!({"at": at, "type": "agent", "host": "h1", "msg": {"type": "hello", "host": "h1",
        "arch": "x86_64", "capacity": 8, "gpus": [
            {"name": "Tesla T4", "sbdf": "0000:01:00.0"},
            {"name": "Tesla T4", "sbdf": "0000:02:00.0"},
            {"name": "Tesla T4", "sbdf": "0000:03:00.0"}]}}),
    )
}

fn running(at: u64, slot: &str, generation: u64) -> Event {
    ev(
        json!({"at": at, "type": "agent", "host": "h1", "msg": {"type": "slot", "id": slot,
        "phase": "running", "generation": generation, "frame_url": "ws://h1:20003/frames",
        "vm": "fleet-slot", "timings": {}}}),
    )
}

fn joined(at: u64, viewer: &str) -> Event {
    ev(json!({"at": at, "type": "viewer_joined", "viewer": viewer, "client": "198.51.100.20"}))
}

fn intent(at: u64, viewer: &str, id: &str, name: &str, args: serde_json::Value) -> Event {
    ev(
        json!({"at": at, "type": "intent", "viewer": viewer, "client": "198.51.100.20",
        "id": id, "name": name, "args": args, "secret": "token-a"}),
    )
}

fn tick(at: u64) -> Event {
    ev(json!({"at": at, "type": "tick"}))
}

fn run(s: State, events: Vec<Event>) -> State {
    events.iter().fold(s, apply)
}

/// Viewer v1 holds slot-3 (the standby GPU), its private VM running; the state as stored and
/// read back by a coordinator that restarted at `restart_at`.
fn claimed_then_restarted(restart_at: u64) -> (State, u64) {
    let s = run(
        State::new(config(), 0),
        vec![
            hello(1000),
            running(1500, "slot-1", 1),
            running(1500, "slot-2", 1),
            joined(2000, "v1"),
            intent(3000, "v1", "c1", "claim_gpu", json!({})),
        ],
    );
    assert_eq!(s.receipts["c1"].outcome, Outcome::Committed);
    let generation = s.fleet.slots[&3].claim.as_ref().unwrap().generation;
    let s = run(s, vec![running(4000, "slot-3", generation)]);
    let stored: State = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    (stored.resumed(config(), restart_at), generation)
}

#[test]
fn a_claim_survives_a_restart_for_the_page_that_presents_its_token() {
    let (s, generation) = claimed_then_restarted(5000);
    assert_eq!(s.fleet.held_claims.len(), 1, "the claim is kept");
    let held = &s.fleet.held_claims[0];
    assert_eq!(
        (held.host.as_str(), held.sbdf.as_str()),
        ("h1", "0000:03:00.0")
    );
    assert_eq!(held.claim.viewer, "", "the old viewer id names nobody now");
    assert_eq!(held.claim.resume_by, Some(5000 + CLAIM_RESUME_MS));
    // The host says hello again: the claim goes back on its GPU's slot, at the generation
    // the host still runs, so nothing is relaunched.
    let s = run(s, vec![hello(6000), running(6500, "slot-3", generation)]);
    let slot = s
        .fleet
        .slots
        .values()
        .find(|x| x.gpu.as_ref().unwrap().sbdf == "0000:03:00.0")
        .unwrap();
    assert!(slot.claim.is_some());
    assert_eq!(slot.generation, generation);
    let a = host_assignments(&s)["h1"]
        .iter()
        .find(|a| a.id == slot.id)
        .unwrap()
        .clone();
    assert_eq!(a.generation, generation);
    assert!(
        a.private_camera.is_some(),
        "the private scene still goes to the VM"
    );
    // The page reconnects under a new viewer id and presents the token.
    let s = run(
        s,
        vec![
            joined(7000, "v-1"),
            intent(
                7100,
                "v-1",
                "r0",
                "resume_claim",
                json!({"token": "token-b"}),
            ),
            intent(
                7200,
                "v-1",
                "r1",
                "resume_claim",
                json!({"token": "token-a"}),
            ),
        ],
    );
    assert_eq!(s.receipts["r0"].outcome, Outcome::Refused, "a wrong token");
    assert_eq!(s.receipts["r1"].outcome, Outcome::Committed);
    let c = s
        .fleet
        .slots
        .values()
        .find_map(|x| x.claim.as_ref())
        .unwrap();
    assert_eq!((c.viewer.as_str(), c.resume_by), ("v-1", None));
    // The token never reaches the shared action list.
    let shown = s.fleet.actions.iter().find(|a| a.id == "r1").unwrap();
    assert_eq!(shown.args, json!({}));
    // Taken back, it lasts past the resume window, and its viewer drives it.
    let s = run(
        s,
        vec![
            tick(5000 + CLAIM_RESUME_MS + 1000),
            intent(
                5000 + CLAIM_RESUME_MS + 1100,
                "v-1",
                "cam",
                "claim_camera",
                json!({"drag": {"yaw_deg": 5.0, "pitch_deg": 0.0}}),
            ),
        ],
    );
    assert!(s.fleet.slots.values().any(|x| x.claim.is_some()));
    assert_eq!(s.receipts["cam"].outcome, Outcome::Committed);
}

#[test]
fn a_claim_no_page_takes_back_ends_when_the_resume_window_closes() {
    // On its slot once the host is back.
    let (s, _) = claimed_then_restarted(5000);
    let s = run(s, vec![hello(6000), tick(5000 + CLAIM_RESUME_MS - 1)]);
    assert!(s.fleet.slots.values().any(|x| x.claim.is_some()));
    let s = run(s, vec![tick(5000 + CLAIM_RESUME_MS)]);
    assert!(s.fleet.slots.values().all(|x| x.claim.is_none()));
    assert_eq!(
        s.fleet.last_claim_end.get("198.51.100.20"),
        Some(&(5000 + CLAIM_RESUME_MS))
    );
    // Still held, its host not back.
    let (s, _) = claimed_then_restarted(5000);
    let s = run(s, vec![tick(5000 + CLAIM_RESUME_MS)]);
    assert!(s.fleet.held_claims.is_empty());
    assert_eq!(
        s.fleet.last_claim_end.get("198.51.100.20"),
        Some(&(5000 + CLAIM_RESUME_MS))
    );
}

/// A spent day's budget ends a claim a restart carried over (its host not back yet), as it
/// ends a claim on a slot, and refuses a page taking a claim back while the fleet rests.
#[test]
fn a_spent_budget_ends_a_held_claim_and_refuses_its_resume() {
    let (mut s, _) = claimed_then_restarted(5000);
    assert_eq!(s.fleet.held_claims.len(), 1);
    s.config.daily_spend_usd = Some(10.0);
    // 100 USD an hour reaches the 10 USD ceiling within the trip lead at once.
    let s = run(
        s,
        vec![
            ev(json!({"at": 6000, "type": "burn", "usd_per_hour": 100.0, "instances": 3})),
            tick(7000),
        ],
    );
    assert!(fleet_core::spend::resting(&s, 7000));
    assert!(s.fleet.held_claims.is_empty(), "{:?}", s.fleet.held_claims);
    assert_eq!(s.fleet.last_claim_end.get("198.51.100.20"), Some(&6000));
    let s = run(
        s,
        vec![
            joined(8000, "v2"),
            intent(
                8000,
                "v2",
                "r1",
                "resume_claim",
                json!({"token": "token-a"}),
            ),
        ],
    );
    let r = s.receipt("r1").unwrap();
    assert_eq!(
        (r.outcome, r.reason.as_deref()),
        (Outcome::Refused, Some(fleet_core::spend::SPENT))
    );
}

#[test]
fn a_restarted_coordinator_gives_no_claim_to_a_new_viewer_with_the_old_id() {
    let (s, _) = claimed_then_restarted(5000);
    let s = run(
        s,
        vec![
            hello(6000),
            joined(7000, "v1"),
            intent(
                7100,
                "v1",
                "cam",
                "claim_camera",
                json!({"drag": {"yaw_deg": 5.0, "pitch_deg": 0.0}}),
            ),
        ],
    );
    assert_eq!(s.receipts["cam"].outcome, Outcome::Refused);
}

#[test]
fn a_claim_taken_back_before_its_host_returns_goes_on_its_slot_with_its_viewer() {
    let (s, _) = claimed_then_restarted(5000);
    let s = run(
        s,
        vec![
            joined(5500, "v-2"),
            intent(
                5600,
                "v-2",
                "r1",
                "resume_claim",
                json!({"token": "token-a"}),
            ),
        ],
    );
    assert_eq!(s.receipts["r1"].outcome, Outcome::Committed);
    assert_eq!(s.fleet.held_claims[0].claim.viewer, "v-2");
    let s = run(s, vec![hello(6000)]);
    assert!(s.fleet.held_claims.is_empty());
    let c = s
        .fleet
        .slots
        .values()
        .find_map(|x| x.claim.as_ref())
        .unwrap();
    assert_eq!(c.viewer, "v-2");
    // Its viewer leaving gives it back, as before the restart.
    let s = run(
        s,
        vec![ev(
            json!({"at": 7000, "type": "viewer_left", "viewer": "v-2"}),
        )],
    );
    assert!(s.fleet.slots.values().all(|x| x.claim.is_none()));
}

#[test]
fn a_claim_whose_time_ran_out_while_the_coordinator_was_down_ends() {
    // claim_secs is 600: the claim from 3000 runs out at 603_000.
    let (s, _) = claimed_then_restarted(700_000);
    assert!(s.fleet.held_claims.is_empty());
    assert_eq!(s.fleet.last_claim_end.get("198.51.100.20"), Some(&603_000));
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

fn host_hello(at: u64, host: &str, sbdfs: &[&str]) -> Event {
    let gpus: Vec<serde_json::Value> = sbdfs
        .iter()
        .map(|s| json!({"name": "Tesla T4", "sbdf": s}))
        .collect();
    ev(
        json!({"at": at, "type": "agent", "host": host, "msg": {"type": "hello", "host": host,
        "arch": "x86_64", "capacity": 8, "gpus": gpus}}),
    )
}

/// Hosts say hello again in any order after a restart, and slot names are handed out lowest
/// first. The claimed GPU must come back under the slot id it had: the agent keys its VMs by
/// slot id and tears down a slot whose id now names another GPU, so a new id would end the
/// claim's VM. Its assignment keeps the same id, GPU and generation as before the restart.
#[test]
fn a_claimed_gpu_keeps_its_slot_id_when_hosts_return_in_another_order() {
    let s = run(
        State::new(config(), 0),
        vec![
            host_hello(1000, "h1", &["0000:01:00.0", "0000:02:00.0"]),
            host_hello(1100, "h2", &["0000:09:00.0"]),
            joined(2000, "v1"),
            intent(3000, "v1", "c1", "claim_gpu", json!({})),
        ],
    );
    assert_eq!(s.receipts["c1"].outcome, Outcome::Committed);
    let mine = |s: &State| {
        host_assignments(s)["h2"]
            .iter()
            .find(|a| a.sbdf == "0000:09:00.0")
            .map(|a| (a.id.clone(), a.generation))
            .unwrap()
    };
    let before = mine(&s);
    assert_eq!(before.0, "slot-3", "the claim is on h2's standby GPU");
    let stored: State = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    let s = stored.resumed(config(), 5000);
    // h2 first: lowest first would give its GPU slot-1.
    let s = run(
        s,
        vec![
            host_hello(6000, "h2", &["0000:09:00.0"]),
            host_hello(6100, "h1", &["0000:01:00.0", "0000:02:00.0"]),
        ],
    );
    assert_eq!(
        mine(&s),
        before,
        "the claimed GPU's assignment is unchanged"
    );
    let mut h1: Vec<String> = host_assignments(&s)["h1"]
        .iter()
        .map(|a| a.id.clone())
        .collect();
    h1.sort();
    assert_eq!(h1, ["slot-1", "slot-2"]);
    assert!(s.fleet.slots[&3].claim.is_some());
}

/// A resume at the end of its window is refused at once, before the tick that would end the
/// claim runs: the window is enforced at the intent's own time.
#[test]
fn a_resume_after_its_window_is_refused_before_the_next_tick() {
    let (s, _) = claimed_then_restarted(5000);
    let s = run(
        s,
        vec![
            hello(6000),
            joined(7000, "v-1"),
            intent(
                5000 + CLAIM_RESUME_MS,
                "v-1",
                "r1",
                "resume_claim",
                json!({"token": "token-a"}),
            ),
        ],
    );
    assert_eq!(s.receipts["r1"].outcome, Outcome::Refused);
}

/// A page whose address changed with its connection takes its claim back, and the claim then
/// counts against the new address: a second page there gets no other GPU.
#[test]
fn a_resumed_claim_counts_against_the_address_that_took_it_back() {
    let (s, _) = claimed_then_restarted(5000);
    let s = run(
        s,
        vec![
            hello(6000),
            ev(json!({"at": 7000, "type": "viewer_joined", "viewer": "v-1",
                "client": "203.0.113.7"})),
            ev(
                json!({"at": 7100, "type": "intent", "viewer": "v-1", "client": "203.0.113.7",
                "id": "r1", "name": "resume_claim", "args": {"token": "token-a"}}),
            ),
            ev(json!({"at": 7200, "type": "viewer_joined", "viewer": "v-2",
                "client": "203.0.113.7"})),
            ev(
                json!({"at": 7300, "type": "intent", "viewer": "v-2", "client": "203.0.113.7",
                "id": "c2", "name": "claim_gpu", "args": {}, "secret": "token-b"}),
            ),
        ],
    );
    assert_eq!(s.receipts["r1"].outcome, Outcome::Committed);
    assert_eq!(
        s.receipts["c2"].reason.as_deref(),
        Some("one GPU per client at a time")
    );
}

/// An address that already holds a GPU cannot take back a second one with another claim's
/// token.
#[test]
fn a_resume_is_refused_to_an_address_that_holds_another_gpu() {
    let gpus = |at: u64| {
        ev(
            json!({"at": at, "type": "agent", "host": "h1", "msg": {"type": "hello", "host": "h1",
            "arch": "x86_64", "capacity": 8, "gpus": [
                {"name": "Tesla T4", "sbdf": "0000:01:00.0"},
                {"name": "Tesla T4", "sbdf": "0000:02:00.0"},
                {"name": "Tesla T4", "sbdf": "0000:03:00.0"},
                {"name": "Tesla T4", "sbdf": "0000:04:00.0"}]}}),
        )
    };
    let claim = |at: u64, viewer: &str, client: &str, id: &str, token: &str| {
        ev(
            json!({"at": at, "type": "intent", "viewer": viewer, "client": client, "id": id,
            "name": "claim_gpu", "args": {}, "secret": token}),
        )
    };
    let s = run(
        State::new(config(), 0),
        vec![
            gpus(1000),
            joined(2000, "v1"),
            claim(3000, "v1", "198.51.100.20", "c1", "token-a"),
            ev(json!({"at": 3100, "type": "viewer_joined", "viewer": "v2",
                "client": "203.0.113.7"})),
            claim(3200, "v2", "203.0.113.7", "c2", "token-b"),
        ],
    );
    assert_eq!(s.receipts["c1"].outcome, Outcome::Committed);
    assert_eq!(s.receipts["c2"].outcome, Outcome::Committed);
    let stored: State = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    let s = run(
        stored.resumed(config(), 5000),
        vec![
            gpus(6000),
            ev(json!({"at": 7000, "type": "viewer_joined", "viewer": "v-1",
                "client": "203.0.113.7"})),
            ev(
                json!({"at": 7100, "type": "intent", "viewer": "v-1", "client": "203.0.113.7",
                "id": "r1", "name": "resume_claim", "args": {"token": "token-a"}}),
            ),
        ],
    );
    assert_eq!(
        s.receipts["r1"].reason.as_deref(),
        Some("one GPU per client at a time")
    );
}

/// A page that reconnects before it heard about its claim (the coordinator restarted between
/// storing the claim and telling the page) resends its claim_gpu under the same id and
/// presents the token it made with `resume_claim`. Either order of events ends with the claim
/// this page's: a stored claim is taken back with the token, and a claim the coordinator had
/// not stored is made again by the resent request, which the resume then finds this
/// connection's already.
#[test]
fn a_page_that_never_heard_of_its_claim_takes_it_back_with_its_own_token() {
    // Stored before the restart: the resent request gets its first receipt and binds nothing,
    // and the resume binds the claim.
    let (s, _) = claimed_then_restarted(5000);
    let s = run(
        s,
        vec![
            hello(6000),
            joined(7000, "v-1"),
            intent(7100, "v-1", "c1", "claim_gpu", json!({})),
            intent(
                7200,
                "v-1",
                "r1",
                "resume_claim",
                json!({"token": "token-a"}),
            ),
        ],
    );
    assert_eq!(s.receipts["r1"].outcome, Outcome::Committed);
    let c = s
        .fleet
        .slots
        .values()
        .find_map(|x| x.claim.as_ref())
        .unwrap();
    assert_eq!(c.viewer, "v-1");
    // Not stored before the restart: the resent request makes the claim again with the
    // page's token, and the resume is answered as done.
    let stored = run(
        State::new(config(), 0),
        vec![
            hello(1000),
            running(1500, "slot-1", 1),
            running(1500, "slot-2", 1),
        ],
    );
    let s = run(
        stored.resumed(config(), 5000),
        vec![
            hello(6000),
            joined(7000, "v-1"),
            intent(7100, "v-1", "c1", "claim_gpu", json!({})),
            intent(
                7200,
                "v-1",
                "r1",
                "resume_claim",
                json!({"token": "token-a"}),
            ),
        ],
    );
    assert_eq!(s.receipts["c1"].outcome, Outcome::Committed);
    assert_eq!(
        s.receipts["r1"].outcome,
        Outcome::Committed,
        "{:?}",
        s.receipts["r1"].reason
    );
    let c = s
        .fleet
        .slots
        .values()
        .find_map(|x| x.claim.as_ref())
        .unwrap();
    assert_eq!((c.viewer.as_str(), c.token.as_str()), ("v-1", "token-a"));
}

/// A wall loan a restart carried over counts toward the borrow cap while its host is away, so
/// no second tile is lent meanwhile and the cap holds when the host returns.
#[test]
fn a_held_wall_loan_counts_against_the_borrow_cap() {
    let mut cfg = serde_json::to_value(config()).unwrap();
    cfg["max_borrowed"] = json!(1);
    let cfg: Config = serde_json::from_value(cfg).unwrap();
    let host = |at: u64, name: &str| {
        ev(
            json!({"at": at, "type": "agent", "host": name, "msg": {"type": "hello",
            "host": name, "arch": "x86_64", "capacity": 8, "gpus": [
                {"name": "Tesla T4", "sbdf": "0000:01:00.0"},
                {"name": "Tesla T4", "sbdf": "0000:02:00.0"}]}}),
        )
    };
    let running_on = |at: u64, name: &str, slot: &str| {
        ev(
            json!({"at": at, "type": "agent", "host": name, "msg": {"type": "slot", "id": slot,
            "phase": "running", "generation": 1, "timings": {}}}),
        )
    };
    let claim = |at: u64, viewer: &str, client: &str, id: &str, token: &str| {
        ev(
            json!({"at": at, "type": "intent", "viewer": viewer, "client": client, "id": id,
            "name": "claim_gpu", "args": {}, "secret": token}),
        )
    };
    // h1's two GPUs are both on the wall, so the claim borrows one.
    let s = run(
        State::new(cfg.clone(), 0),
        vec![
            host(1000, "h1"),
            running_on(1500, "h1", "slot-1"),
            running_on(1500, "h1", "slot-2"),
            joined(2000, "v1"),
            claim(3000, "v1", "198.51.100.20", "c1", "token-a"),
        ],
    );
    assert_eq!(s.receipts["c1"].outcome, Outcome::Committed);
    assert!(s
        .fleet
        .slots
        .values()
        .any(|x| x.claim.as_ref().is_some_and(|c| c.borrowed.is_some())));
    let stored: State = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    // After the restart only h2 is back, its tiles running.
    let s = run(
        stored.resumed(cfg, 5000),
        vec![
            host(6000, "h2"),
            // slot-2 stays reserved for the held claim's GPU: h2 has slot-1 and slot-3.
            running_on(6500, "h2", "slot-1"),
            running_on(6500, "h2", "slot-3"),
            ev(json!({"at": 7000, "type": "viewer_joined", "viewer": "v-2",
                "client": "203.0.113.7"})),
            claim(7100, "v-2", "203.0.113.7", "c2", "token-b"),
        ],
    );
    assert_eq!(s.receipts["c2"].outcome, Outcome::Refused);
    assert!(
        s.receipts["c2"]
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("already lent")),
        "{:?}",
        s.receipts["c2"].reason
    );
}

// What a restart does to the power rules: nothing. The stored arrival stays, the pages
// that reconnect are not arrivals, and the fleet is up until the stored limit.

fn power_config() -> Config {
    let mut base = serde_json::to_value(Config::default()).unwrap();
    base["max_tiles"] = json!(2);
    base["power_managed"] = json!(true);
    base["idle_scale_down_secs"] = json!(60);
    base["sleep_without_new_viewer_secs"] = json!(1000);
    serde_json::from_value(base).unwrap()
}

#[test]
fn a_restart_keeps_an_awake_fleet_awake_until_the_stored_limit() {
    // Awake with one viewer that arrived at 5 s: the limit ends at 1005 s. Stored at 900 s.
    let stored = run(
        State::new(power_config(), 0),
        vec![
            hello(1000),
            running(2000, "slot-1", 1),
            running(2000, "slot-2", 1),
            running(2000, "slot-3", 1),
            joined(5000, "v1"),
            tick(900_000),
        ],
    );
    assert_eq!(stored.fleet.power.state, "awake");
    // The coordinator is killed and the next one starts at 950 s from what was stored.
    let read: State = serde_json::from_slice(&serde_json::to_vec(&stored).unwrap()).unwrap();
    let mut s = read.resumed(power_config(), 950_000);
    assert_eq!(s.fleet.power.state, "awake");
    assert_eq!(s.fleet.power.last_arrival_at, Some(5000));
    // Its first ticks, before any page is back, sleep nothing.
    s = run(s, vec![tick(950_500), tick(951_500)]);
    assert_eq!(s.fleet.power.state, "awake", "{:?}", s.fleet.power);
    // The host's agent and the page reconnect; the page says it connected before.
    s = run(
        s,
        vec![
            hello(952_000),
            running(952_001, "slot-1", 1),
            running(952_001, "slot-2", 1),
            running(952_001, "slot-3", 1),
            ev(
                json!({"at": 953_000, "type": "viewer_joined", "viewer": "v1",
                "client": "198.51.100.20", "returning": true}),
            ),
            tick(1_004_999),
        ],
    );
    assert_eq!(s.fleet.power.state, "awake");
    assert_eq!(s.fleet.power.last_arrival_at, Some(5000));
    // The stored limit, neither earlier nor later.
    s = run(s, vec![tick(1_005_000)]);
    assert_eq!(s.fleet.power.state, "asleep");
}

fn pages_hello(at: u64) -> Event {
    ev(
        json!({"at": at, "type": "agent", "host": "r1", "msg": {"type": "hello", "host": "r1",
        "arch": "aarch64", "capacity": 0, "gpus": [], "renders": {"cpus": 40,
        "pages": [{"id": "example", "url": "https://example.com/"}]}}}),
    )
}

/// Pages mode's overlay is acknowledged state: the state as stored and read back by a
/// restarted coordinator still holds it, the returning host is ordered it with the number
/// it had, and it ends when it was going to.
#[test]
fn a_pages_overlay_survives_a_restart_and_still_ends_on_time() {
    let s = run(
        State::new(config(), 0),
        vec![
            pages_hello(1_000),
            intent(
                2_000,
                "v1",
                "o-1",
                "renders_transform",
                json!({"name": "mirror"}),
            ),
        ],
    );
    assert_eq!(s.receipts["o-1"].outcome, Outcome::Committed);
    let before = s.fleet.renders.overlay.clone().expect("the overlay is set");
    // Stored, the process gone, read back.
    let stored = serde_json::to_string(&s).unwrap();
    let s: State = serde_json::from_str(&stored).unwrap();
    let s = s.resumed(config(), 50_000);
    assert_eq!(s.fleet.renders.overlay, Some(before.clone()));
    // No host until one says hello again, so no view yet; the order is ready for it.
    assert!(s.fleet.renders.hosts.is_empty());
    let s = run(s, vec![pages_hello(51_000)]);
    let order = s.fleet.renders.order(true).overlay.expect("ordered again");
    assert_eq!(order.transform.as_deref(), Some("mirror"));
    assert_eq!(order.seq, before.seq);
    let v = serde_json::to_value(fleet_core::view(&s)).unwrap();
    assert_eq!(v["renders"]["overlay"]["until"], before.until);
    // The intent sent again gets its first receipt, and changes nothing.
    let s = run(
        s,
        vec![intent(
            52_000,
            "v1",
            "o-1",
            "renders_transform",
            json!({"name": "mirror"}),
        )],
    );
    assert_eq!(s.fleet.renders.overlay, Some(before.clone()));
    // It ends at its time, not later for the restart.
    let s = run(s, vec![tick(before.until - 1)]);
    assert!(s.fleet.renders.overlay.is_some());
    let s = run(s, vec![tick(before.until)]);
    assert!(s.fleet.renders.overlay.is_none());
    assert_eq!(s.fleet.renders.order(true).overlay, None);
}

/// The recent-actions list, which every viewer sees, shows of a pages intent only what the
/// coordinator parsed and committed: a refused transform's name is free text nobody checked,
/// and so is any other key a viewer puts beside `text` or `name`.
#[test]
fn a_pages_action_shows_only_what_was_parsed_and_committed() {
    let intent = |at: u64, client: &str, id: &str, name: &str, args: serde_json::Value| {
        ev(
            json!({"at": at, "type": "intent", "viewer": "v1", "client": client, "id": id,
            "name": name, "args": args, "check": {"verdict": "allow", "reason": ""}}),
        )
    };
    let s = run(
        State::new(config(), 0),
        vec![
            pages_hello(1_000),
            intent(
                2_000,
                "198.51.100.20",
                "t-1",
                "renders_transform",
                json!({"name": "so nasty"}),
            ),
            intent(
                3_000,
                "198.51.100.21",
                "o-1",
                "renders_overlay",
                json!({"text": "  hello   there ", "note": "so nasty"}),
            ),
            intent(
                20_000,
                "198.51.100.22",
                "t-2",
                "renders_transform",
                json!({"name": "mirror", "note": "so nasty"}),
            ),
            intent(
                40_000,
                "198.51.100.23",
                "c-1",
                "renders_clear",
                json!({"note": "so nasty"}),
            ),
        ],
    );
    assert_eq!(s.receipts["t-1"].outcome, Outcome::Refused);
    for id in ["o-1", "t-2", "c-1"] {
        assert_eq!(s.receipts[id].outcome, Outcome::Committed, "{id}");
    }
    let v = serde_json::to_value(fleet_core::view(&s)).unwrap();
    assert!(!v.to_string().contains("nasty"), "{v}");
    let shown: Vec<_> = v["actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| (a["id"].as_str().unwrap().to_string(), a["args"].clone()))
        .collect();
    assert_eq!(
        shown,
        [
            ("t-1".to_string(), json!({})),
            ("o-1".to_string(), json!({"text": "hello there"})),
            ("t-2".to_string(), json!({"name": "mirror"})),
            ("c-1".to_string(), json!({})),
        ]
    );
}

/// A text the check refused is in no viewer's snapshot: not as the overlay, and not in the
/// recent-actions list, which every viewer sees.
#[test]
fn a_refused_pages_overlay_text_reaches_no_snapshot() {
    let refused = ev(json!({"at": 2_000, "type": "intent", "viewer": "v1",
        "client": "198.51.100.20", "id": "o-1", "name": "renders_overlay",
        "args": {"text": "so nasty"},
        "check": {"verdict": "refuse", "reason": "not for this wall"}}));
    let s = run(State::new(config(), 0), vec![pages_hello(1_000), refused]);
    assert_eq!(s.receipts["o-1"].outcome, Outcome::Refused);
    let v = serde_json::to_string(&fleet_core::view(&s)).unwrap();
    assert!(!v.contains("nasty"), "{v}");
    // An allowed one is shown, as an allowed word is.
    let allowed = ev(json!({"at": 3_000, "type": "intent", "viewer": "v1",
        "client": "198.51.100.20", "id": "o-2", "name": "renders_overlay",
        "args": {"text": "hello"}, "check": {"verdict": "allow", "reason": ""}}));
    let s = run(s, vec![allowed]);
    let v = serde_json::to_value(fleet_core::view(&s)).unwrap();
    assert_eq!(v["renders"]["overlay"]["text"], "hello");
}
