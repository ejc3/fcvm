//! The own-GPU autopilot through `apply`: a claim flies from its first frame, a drag or the
//! pause button stops it where the renderer has the camera, and the button resumes it.

use fleet_core::autopilot::{self, AHEAD_MIN_SECS, KEY_SECS, LOOP_SECS};
use fleet_core::scene::{Keyframe, Scene, MAX_KEYFRAMES};
use fleet_core::state::Claim;
use fleet_core::{apply, Config, Event, State};
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

fn slot(at: u64, generation: u64) -> Event {
    ev(
        json!({"at": at, "type": "agent", "host": "h1", "msg": {"type": "slot", "id": "slot-3",
        "phase": "running", "generation": generation, "frame_url": "ws://h1:20003/frames",
        "vm": "fleet-slot-3", "timings": {}}}),
    )
}

fn intent(at: u64, id: &str, name: &str, args: serde_json::Value) -> Event {
    ev(
        json!({"at": at, "type": "intent", "viewer": "v1", "client": "198.51.100.20",
        "id": id, "name": name, "args": args, "secret": "token-a"}),
    )
}

fn tick(at: u64) -> Event {
    ev(json!({"at": at, "type": "tick"}))
}

/// A fleet with a standby GPU and viewer v1 holding it, its private VM restoring.
fn claimed() -> State {
    let mut s = State::new(config(), 0);
    for e in [
        ev(
            json!({"at": 1000, "type": "agent", "host": "h1", "msg": {"type": "hello", "host": "h1",
            "arch": "x86_64", "capacity": 8, "gpus": [
                {"name": "Tesla T4", "sbdf": "0000:01:00.0"},
                {"name": "Tesla T4", "sbdf": "0000:02:00.0"},
                {"name": "Tesla T4", "sbdf": "0000:03:00.0"}]}}),
        ),
        slot(2000, 1),
        ev(json!({"at": 2500, "type": "viewer_joined", "viewer": "v1", "client": "198.51.100.20"})),
        intent(3000, "c1", "claim_gpu", json!({})),
    ] {
        s = apply(s, &e);
    }
    assert_eq!(s.receipts["c1"].reason, None);
    s
}

fn claim(s: &State) -> &Claim {
    s.fleet
        .slots
        .values()
        .find_map(|x| x.claim.as_ref())
        .expect("a claim")
}

fn t(s: &State, at: u64) -> f64 {
    s.fleet.scene.t_at(at)
}

fn pose_at(c: &Claim, t: f64) -> Keyframe {
    Scene {
        epoch_unix_ms: 0,
        camera: c.camera.clone(),
        time_of_day: 0.0,
        words: vec![],
        lights: vec![],
        weather: vec![],
    }
    .camera_at(t)
}

fn dist(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

/// The same pose to a metre and a hundredth of a degree of field of view.
fn assert_same_pose(a: &Keyframe, b: &Keyframe, what: &str) {
    assert_near(a, b, 1e-3, what);
}

/// Within `km` (position and look point) and a tenth of a degree of field of view.
fn assert_near(a: &Keyframe, b: &Keyframe, km: f64, what: &str) {
    assert!(
        dist(a.pos, b.pos) < km && dist(a.look, b.look) < km && (a.fov_deg - b.fov_deg).abs() < 0.1,
        "{what}: {a:?} vs {b:?}"
    );
}

/// Running for 20 s from the first frame (reported at 8000), ticking every second.
fn flying() -> State {
    let mut s = apply(claimed(), &slot(8000, 2));
    for ms in (9000..=28000).step_by(1000) {
        s = apply(s, &tick(ms));
    }
    s
}

#[test]
fn a_claim_starts_on_autopilot_and_flies_from_its_first_frame() {
    let s = claimed();
    let c = claim(&s);
    assert!(c.autopilot, "a new claim is on autopilot");
    assert_eq!(c.flight_t0, None, "nothing flies before the first frame");
    assert_same_pose(
        &pose_at(c, t(&s, 7000)),
        &autopilot::pose(0.0),
        "waiting at the start",
    );

    let s = apply(s, &slot(8000, 2));
    let c = claim(&s);
    let t0 = c.flight_t0.expect("the first frame starts the flight");
    assert!((t0 - (t(&s, 8000) + autopilot::START_LEAD_SECS)).abs() < 1e-9);
    assert!(c.camera.last().unwrap().t >= t(&s, 8000) + AHEAD_MIN_SECS);

    let s = flying();
    let c = claim(&s);
    let now = t(&s, 28000);
    assert!(c.autopilot);
    assert!(
        c.camera.last().unwrap().t >= now + AHEAD_MIN_SECS,
        "keyframes stay ahead"
    );
    // 20 s in, the camera is on the loop where its phase says (between keyframes the
    // renderer's spline through them is within 10 m of the loop) and moving.
    assert_near(
        &pose_at(c, now),
        &autopilot::pose(now - t0),
        0.01,
        "on the loop",
    );
    let moved = dist(pose_at(c, now).pos, pose_at(c, now + 1.0).pos);
    assert!(moved > 0.3, "flying, {moved} km in a second");
}

#[test]
fn a_drag_takes_the_camera_where_it_is_and_stops_the_autopilot() {
    let s = flying();
    let before = claim(&s).clone();
    let at = 28400;
    let now = t(&s, at);
    let s = apply(
        s,
        &intent(
            at,
            "d1",
            "claim_camera",
            json!({"drag": {"yaw_deg": 10, "pitch_deg": 0}}),
        ),
    );
    assert_eq!(s.receipts["d1"].reason, None);
    let c = claim(&s);
    assert!(!c.autopilot, "input pauses the autopilot");
    assert_same_pose(
        &pose_at(c, now),
        &pose_at(&before, now),
        "the frame at the drag",
    );
    // It comes to rest by the drag's keyframe and stays there: nothing flies on.
    let end = c.camera.last().unwrap();
    assert_eq!(end.intent.as_deref(), Some("d1"));
    assert!(end.t <= now + autopilot::TAKEOVER_SECS + 1e-9);
    assert_same_pose(&pose_at(c, now + 3.0), &pose_at(c, now + 30.0), "held");
    // Ticks leave a paused camera alone.
    let s = apply(s, &tick(40000));
    assert_eq!(
        claim(&s).camera.last().unwrap().intent.as_deref(),
        Some("d1")
    );
}

#[test]
fn the_button_pauses_at_the_current_pose_and_resumes_the_flight() {
    let s = flying();
    let before = claim(&s).clone();
    let at = 28300;
    let now = t(&s, at);
    let s = apply(
        s,
        &intent(at, "p1", "claim_autopilot", json!({"on": false})),
    );
    assert_eq!(s.receipts["p1"].reason, None);
    let c = claim(&s);
    assert!(!c.autopilot);
    assert_same_pose(
        &pose_at(c, now),
        &pose_at(&before, now),
        "the frame at the pause",
    );
    let rest = pose_at(c, now + autopilot::PAUSE_SECS);
    assert_same_pose(&pose_at(c, now + 10.0), &rest, "at rest after the pause");
    assert!(
        dist(rest.pos, pose_at(c, now).pos) < 1.5,
        "rest is close by"
    );

    // Play: from where it rests, back onto the loop, flying again.
    let at = 33000;
    let now = t(&s, at);
    let held = pose_at(claim(&s), now);
    let s = apply(s, &intent(at, "p2", "claim_autopilot", json!({"on": true})));
    assert_eq!(s.receipts["p2"].reason, None);
    let c = claim(&s);
    assert!(c.autopilot);
    assert_same_pose(&pose_at(c, now), &held, "the frame at play");
    let t0 = c.flight_t0.expect("flying");
    let later = now + 10.0;
    assert_near(
        &pose_at(c, later),
        &autopilot::pose(later - t0),
        0.01,
        "back on the loop",
    );
    let s = apply(s, &tick(50000));
    assert!(claim(&s).camera.last().unwrap().t >= t(&s, 50000) + AHEAD_MIN_SECS);

    // The same state twice commits and changes nothing; without a claim it is refused.
    let s2 = apply(
        s.clone(),
        &intent(50100, "p3", "claim_autopilot", json!({"on": true})),
    );
    assert_eq!(s2.receipts["p3"].reason, None);
    assert_eq!(claim(&s2).camera, claim(&s).camera);
    let mut s3 = State::new(config(), 0);
    s3 = apply(
        s3,
        &ev(json!({"at": 1, "type": "viewer_joined", "viewer": "v1", "client": "198.51.100.20"})),
    );
    s3 = apply(
        s3,
        &intent(2, "p4", "claim_autopilot", json!({"on": false})),
    );
    assert!(s3.receipts["p4"]
        .reason
        .as_deref()
        .unwrap()
        .contains("GPU of your own"));
}

#[test]
fn a_pause_before_the_first_frame_keeps_the_camera_still() {
    let s = apply(
        claimed(),
        &intent(4000, "p1", "claim_autopilot", json!({"on": false})),
    );
    let s = apply(s, &slot(8000, 2));
    let c = claim(&s);
    assert!(!c.autopilot && c.flight_t0.is_none());
    assert_same_pose(
        &pose_at(c, t(&s, 20000)),
        &autopilot::pose(0.0),
        "still at the start",
    );
}

#[test]
fn a_long_flight_keeps_its_keyframes_bounded_and_on_the_grid() {
    let mut s = apply(claimed(), &slot(8000, 2));
    let t0 = claim(&s).flight_t0.unwrap();
    for ms in (9000..400_000).step_by(1000) {
        s = apply(s, &tick(ms));
        let c = claim(&s);
        let now = t(&s, ms);
        assert!(c.camera.len() <= MAX_KEYFRAMES);
        assert!(
            c.camera.windows(2).all(|w| w[0].t < w[1].t),
            "{:?}",
            c.camera
        );
        assert!(c.camera.last().unwrap().t >= now + AHEAD_MIN_SECS);
        assert!(c.camera.first().unwrap().t <= now - 2.0 * KEY_SECS);
    }
    // Several laps in, every flight keyframe is the loop's pose at its phase.
    let c = claim(&s);
    for k in c.camera.iter().filter(|k| k.t > t0) {
        let phase = k.t - t0;
        assert!(phase > 5.0 * LOOP_SECS);
        assert!(((phase / KEY_SECS) - (phase / KEY_SECS).round()).abs() < 1e-6);
        assert_same_pose(k, &autopilot::pose(phase), "on the grid");
    }
}

/// One lap of the flight as the renderer is given it (keyframes every `KEY_SECS`), the same
/// path paused part way, and the pose the coordinator computes every quarter second on
/// each. fleet/renderer/tests/test_host.cpp reads this file and checks that its own
/// `camera_at` gives the same poses (so a pause or a drag starts where the frames are) and
/// that the eye clears the ground on every sample. `FLEET_VECTORS_BLESS=1` rewrites it.
fn lap_fixture() -> serde_json::Value {
    let lap: Vec<Keyframe> = (-2..=(LOOP_SECS / KEY_SECS) as i64 + 2)
        .map(|k| autopilot::pose(k as f64 * KEY_SECS))
        .collect();
    let mut paused = Scene {
        epoch_unix_ms: 0,
        camera: lap.clone(),
        time_of_day: 0.0,
        words: vec![],
        lights: vec![],
        weather: vec![],
    };
    let to = paused.camera_at(20.3 + autopilot::PAUSE_SECS / 2.0);
    paused.steer_over(20.3, to, autopilot::PAUSE_SECS);
    let paused = paused.camera;
    let sample = |keys: &[Keyframe], from: f64, to: f64| -> Vec<serde_json::Value> {
        let s = Scene {
            epoch_unix_ms: 0,
            camera: keys.to_vec(),
            time_of_day: 0.0,
            words: vec![],
            lights: vec![],
            weather: vec![],
        };
        let n = ((to - from) / 0.25).round() as i64;
        (0..=n)
            .map(|i| {
                let t = from + i as f64 * 0.25;
                let k = s.camera_at(t);
                json!({"t": t, "pos": k.pos, "look": k.look, "fov_deg": k.fov_deg})
            })
            .collect()
    };
    json!({
        "description": "The own-GPU autopilot's lap and the same path paused at t = 20.3, with the camera the coordinator computes on each (fleet/core/tests/autopilot.rs writes this; fleet/renderer/tests/test_host.cpp checks it).",
        "loop_secs": LOOP_SECS,
        "lap": {"camera": lap.clone(), "samples": sample(&lap, 0.0, LOOP_SECS)},
        "paused": {"camera": paused.clone(), "samples": sample(&paused, 15.0, 25.0)},
    })
}

#[test]
fn the_lap_fixture_is_what_the_code_flies() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/autopilot_lap.json");
    // Compared as text: serde_json's default parser can round a long decimal differently
    // from the value that printed it.
    let want = serde_json::to_string_pretty(&lap_fixture()).unwrap() + "\n";
    if std::env::var("FLEET_VECTORS_BLESS").as_deref() == Ok("1") {
        std::fs::write(&path, &want).unwrap();
    }
    let got = std::fs::read_to_string(&path).expect("tests/autopilot_lap.json");
    assert!(
        got == want,
        "tests/autopilot_lap.json is stale: rerun with FLEET_VECTORS_BLESS=1"
    );
}
