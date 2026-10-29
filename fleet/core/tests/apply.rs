//! Properties of `apply` over long generated sequences, beyond the hand-written vectors.

use fleet_core::protocol::{AgentMsg, SlotReport};
use fleet_core::state::Gpu;
use fleet_core::tiling::{Half, Rect};
use fleet_core::view::host_assignments;
use fleet_core::{apply, view, Config, Event, State};
use std::collections::BTreeSet;

fn hello(at: u64, host: &str, gpus: usize) -> Event {
    Event::Agent {
        at,
        host: host.into(),
        msg: AgentMsg::Hello {
            host: host.into(),
            arch: "x86_64".into(),
            gpus: (0..gpus)
                .map(|i| Gpu {
                    sbdf: format!("0000:{:02x}:00.0", i + 1),
                    name: "Tesla T4".into(),
                })
                .collect(),
            capacity: 8,
            store_ready_at: None,
            boot: Default::default(),
        },
    }
}

fn running(at: u64, host: &str, slot: &str) -> Event {
    Event::Agent {
        at,
        host: host.into(),
        msg: AgentMsg::Slot(SlotReport {
            id: slot.into(),
            phase: "running".into(),
            generation: 1,
            ..Default::default()
        }),
    }
}

/// The tiles exactly cover the image (each side as wide as its share of the live GPUs, one
/// tile per GPU), every host's tiles lie inside its region and the regions do not overlap,
/// every positioned slot holds a distinct tile, and no GPU backs two slots.
fn check_layout(state: &State) {
    let v = view(state);
    let (n, e) = (v.fleet.n, v.fleet.ec2_n);
    assert_eq!(v.tiles.len(), n + e);
    let eps = 1e-9;
    let overlap = |a: &Rect, b: &Rect| {
        let w = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
        let h = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
        w > eps && h > eps
    };
    let inside = |r: &Rect, a: &Rect| {
        r.x >= a.x - eps
            && r.y >= a.y - eps
            && r.x + r.w <= a.x + a.w + eps
            && r.y + r.h <= a.y + a.h + eps
    };
    let rects: Vec<Rect> = v.tiles.iter().map(|t| t.rect).collect();
    let area: f64 = rects.iter().map(|r| r.w * r.h).sum();
    if n + e > 0 {
        assert!((area - 1.0).abs() < 1e-9, "area {area}");
    }
    for (i, a) in rects.iter().enumerate() {
        for b in &rects[i + 1..] {
            assert!(!overlap(a, b), "{a:?} overlaps {b:?}");
        }
    }
    for t in &v.tiles {
        let side = match t.half {
            Half::Fcvm => v.layout.fcvm,
            Half::Ec2 => v.layout.ec2,
        }
        .expect("a side with tiles has a rectangle");
        assert!(inside(&t.rect, &side));
    }
    if n + e > 0 {
        let fw = v.layout.fcvm.map_or(0.0, |r| r.w);
        assert!(
            (fw * (n + e) as f64 - n as f64).abs() < 1e-9,
            "fcvm width {fw} for {n} of {}",
            n + e
        );
    }
    let regions: Vec<_> = v
        .aggregates
        .iter()
        .filter(|a| a.half == Half::Fcvm)
        .collect();
    for (i, a) in regions.iter().enumerate() {
        for b in &regions[i + 1..] {
            assert!(
                !overlap(&a.rect, &b.rect),
                "regions {} and {} overlap",
                a.id,
                b.id
            );
        }
        for id in &a.tiles {
            let t = v.tiles.iter().find(|t| &t.id == id).unwrap();
            assert!(inside(&t.rect, &a.rect), "{} is outside {}", id, a.id);
            assert_eq!(t.host.as_deref(), a.host.as_deref());
        }
    }
    let slots: Vec<_> = v.tiles.iter().filter_map(|t| t.slot.clone()).collect();
    let unique: BTreeSet<_> = slots.iter().collect();
    assert_eq!(slots.len(), unique.len(), "a slot holds two tiles");
    let mut gpus = BTreeSet::new();
    for (host, list) in host_assignments(state) {
        for a in list {
            assert!(gpus.insert((host.clone(), a.sbdf)), "a GPU backs two slots");
        }
    }
}

#[test]
fn hosts_joining_one_by_one_retile_every_n_up_to_max_tiles() {
    let config = Config {
        ec2_max: 100,
        ..Config::default()
    };
    let mut s = State::new(config, 0);
    for i in 1..=110u64 {
        let host = format!("host-{i}");
        s = apply(s, &hello(i * 10, &host, 1));
        if (2..=100).contains(&i) {
            // A GPU joining a wall that has tiles warms until its VM renders.
            let v = view(&s);
            assert_eq!(v.fleet.n, i as usize - 1);
            assert_eq!(v.fleet.warming, vec![format!("slot-{i}")]);
            assert!(host_assignments(&s)[&host][0].warm);
        }
        s = apply(s, &running(i * 10 + 1, &host, &format!("slot-{i}")));
        let v = view(&s);
        assert!(v.fleet.warming.is_empty());
        assert_eq!(v.fleet.n, (i as usize).min(100));
        assert_eq!(v.fleet.standby.len(), (i as usize).saturating_sub(100));
        // No EC2 instance has reported, so the right half is all dark EC2 tiles.
        assert!(v.tiles[v.fleet.n..].iter().all(|t| t.phase == "pending"));
        check_layout(&s);
    }
    // Hosts leave from the low end; the remaining slots keep their names and move up.
    for i in 1..=110u64 {
        s = apply(
            s,
            &Event::AgentGone {
                at: 10_000 + i,
                host: format!("host-{i}"),
            },
        );
        let v = view(&s);
        assert_eq!(v.fleet.n, (110 - i as usize).min(100));
        if let Some(first) = v.tiles.iter().find(|t| t.half == Half::Fcvm) {
            assert_eq!(
                first.slot.as_deref(),
                Some(format!("slot-{}", i + 1).as_str())
            );
        }
        check_layout(&s);
    }
}

#[test]
fn version_moves_exactly_when_the_view_or_a_receipt_changes() {
    let mut s = State::new(Config::default(), 0);
    let mut last = serde_json::to_value(view(&s)).unwrap();
    let mut events = vec![hello(1, "h", 4)];
    for t in 0..50u64 {
        events.push(Event::Tick { at: 100 + t });
        events.push(Event::Agent {
            at: 100 + t,
            host: "h".into(),
            msg: AgentMsg::Slot(SlotReport {
                id: format!("slot-{}", t % 4 + 1),
                phase: "running".into(),
                generation: 1,
                vm: Some("vm".into()),
                frame_url: None,
                timings: Default::default(),
                error: None,
                retry_in_ms: None,
                ipv6: None,
                ipv4: None,
                v4_port: None,
                place: None,
            }),
        });
    }
    for e in &events {
        let before = s.version;
        s = apply(s, e);
        let now = serde_json::to_value(view(&s)).unwrap();
        if now == last {
            assert_eq!(s.version, before, "version moved with no change: {e:?}");
        } else {
            assert_eq!(s.version, before + 1, "change without a version: {e:?}");
        }
        last = now;
    }
}

/// A tick that only forgets per-client limits (here a light's) changes nothing anyone sees,
/// so it keeps the version; a coordinator ticking between two intents must not move it.
#[test]
fn forgetting_a_rate_limit_keeps_the_version() {
    let mut s = State::new(Config::default(), 0);
    s = apply(s, &hello(1, "h", 1));
    s = apply(
        s,
        &Event::ViewerJoined {
            at: 2,
            viewer: "v".into(),
            client: Some("192.0.2.7".into()),
        },
    );
    s = apply(
        s,
        &Event::Intent {
            at: 100,
            viewer: "v".into(),
            client: Some("192.0.2.7".into()),
            id: "l".into(),
            name: "light".into(),
            args: serde_json::json!({"pos": [1.0, 2.0, -90.0], "color": [1.0, 0.5, 0.0]}),
            check: None,
            secret: None,
        },
    );
    assert_eq!(
        s.receipt("l").map(|r| r.outcome),
        Some(fleet_core::state::Outcome::Committed)
    );
    let before = (s.version, serde_json::to_value(view(&s)).unwrap());
    s = apply(s, &Event::Tick { at: 1_200 });
    assert_eq!(
        serde_json::to_value(view(&s)).unwrap(),
        before.1,
        "the tick changed the view"
    );
    assert_eq!(
        s.version, before.0,
        "a tick that only forgot a limit moved the version"
    );
}

/// The EC2 half can complete the image (the fleet turns awake) before any GPU host's agent
/// has said hello, as it did on staging on 2026-10-25 (awake 53 s into a wake whose host
/// rendered at 150 s). The host's later phases still belong to that wake: its hello, its
/// store and boot times, and its tiles rendering are recorded after the fleet is awake.
#[test]
fn a_host_that_comes_up_after_the_fleet_is_awake_still_records_its_wake() {
    let config: Config = serde_json::from_value(serde_json::json!({
        "power_managed": true, "idle_scale_down_secs": 60, "ec2_max": 2,
        "race_interval_secs": 300, "restart_cooldown_secs": 1, "claim_secs": 120
    }))
    .map(|o: serde_json::Value| {
        let mut base = serde_json::to_value(Config::default()).unwrap();
        for (k, v) in o.as_object().unwrap() {
            base[k] = v.clone();
        }
        serde_json::from_value(base).unwrap()
    })
    .unwrap();
    let events: Vec<Event> = serde_json::from_value(serde_json::json!([
        {"type": "viewer_joined", "at": 1000, "viewer": "v1", "client": "192.0.2.70"},
        {"type": "visibility", "at": 1100, "viewer": "v1", "visible": false},
        {"type": "tick", "at": 62000},
        {"type": "visibility", "at": 100000, "viewer": "v1", "visible": true},
        {"type": "scale_instances", "at": 116000, "instances": [{"id": "h1", "lifecycle": "Pending"}]},
        {"type": "scale_instances", "at": 121000, "instances": [{"id": "h1", "lifecycle": "InService"}]},
        {"type": "ec2", "at": 150000, "report": {"id": "ec2-1", "phase": "running", "generation": 1, "vm": "instance-1", "frame_url": "ws://ec2-1.example:443/frames", "timings": {}}},
        {"type": "ec2", "at": 153000, "report": {"id": "ec2-2", "phase": "running", "generation": 1, "vm": "instance-2", "frame_url": "ws://ec2-2.example:443/frames", "timings": {}}},
    ]))
    .unwrap();
    let mut s = State::new(config, 0);
    for e in &events {
        s = apply(s, e);
    }
    assert_eq!(
        s.fleet.power.state, "awake",
        "the EC2 half completed the image"
    );
    let host = |s: &State| s.fleet.power.hosts.get("h1").cloned().unwrap();
    assert_eq!(host(&s).booted_at, Some(121000));
    let mut later = vec![Event::Agent {
        at: 230000,
        host: "h1".into(),
        msg: AgentMsg::Hello {
            host: "h1".into(),
            arch: "x86_64".into(),
            gpus: vec![Gpu {
                sbdf: "0000:01:00.0".into(),
                name: "Tesla T4".into(),
            }],
            capacity: 1,
            store_ready_at: Some(229000),
            boot: [
                ("kernel_started_at".to_string(), 160000),
                ("store_started_at".to_string(), 176000),
                // Before the wake began: not this wake's.
                ("agent_started_at".to_string(), 99000),
            ]
            .into(),
        },
    }];
    later.push(running(240000, "h1", "slot-1"));
    for e in &later {
        s = apply(s, e);
    }
    let w = host(&s);
    assert_eq!(w.agent_at, Some(230000));
    assert_eq!(w.store_ready_at, Some(229000));
    assert_eq!(
        w.boot,
        [
            ("kernel_started_at".to_string(), 160000),
            ("store_started_at".to_string(), 176000)
        ]
        .into()
    );
    assert_eq!(w.rendering_at, Some(240000));
    // The wake's record is closed once every host of it renders: a later reconnect of the
    // same agent changes nothing.
    let mut again = later[0].clone();
    if let Event::Agent { at, .. } = &mut again {
        *at = 900000;
    }
    s = apply(s, &again);
    assert_eq!(host(&s).agent_at, Some(230000));
}

/// Config defaults with `overrides` merged in by name.
fn config_with(overrides: serde_json::Value) -> Config {
    let mut base = serde_json::to_value(Config::default()).unwrap();
    for (k, v) in overrides.as_object().unwrap() {
        base[k] = v.clone();
    }
    serde_json::from_value(base).unwrap()
}

fn events(v: serde_json::Value) -> Vec<Event> {
    serde_json::from_value(v).unwrap()
}

fn half(x: f64) -> Option<Rect> {
    Some(Rect {
        x,
        y: 0.0,
        w: 0.5,
        h: 1.0,
    })
}

/// A race darkens every tile: the fcvm VMs stop and restore, the EC2 instances are
/// replaced. Both sides keep their configured widths and every tile keeps its place, so
/// the wall does not re-tile mid-race and the dark tiles stay where viewers see them.
#[test]
fn a_race_that_darkens_every_tile_keeps_both_sides_and_every_place() {
    let config = config_with(serde_json::json!({"ec2_max": 2, "fcvm_tiles": 2}));
    let mut s = State::new(config, 0);
    let mut setup = vec![hello(1000, "h1", 2)];
    setup.push(running(1100, "h1", "slot-1"));
    setup.push(running(1200, "h1", "slot-2"));
    setup.extend(events(serde_json::json!([
        {"type": "ec2", "at": 1300, "report": {"id": "ec2-1", "phase": "running", "generation": 1, "vm": "instance-1", "frame_url": "ws://ec2-1.example:443/frames", "timings": {}}},
        {"type": "ec2", "at": 1400, "report": {"id": "ec2-2", "phase": "running", "generation": 1, "vm": "instance-2", "frame_url": "ws://ec2-2.example:443/frames", "timings": {}}},
    ])));
    for e in &setup {
        s = apply(s, e);
    }
    let before = view(&s);
    assert_eq!(before.layout.fcvm, half(0.0));
    assert_eq!(before.layout.ec2, half(0.5));
    let places = |v: &fleet_core::view::View| -> Vec<(String, Rect)> {
        v.tiles.iter().map(|t| (t.id.clone(), t.rect)).collect()
    };
    assert_eq!(before.tiles.len(), 4);
    let race = events(serde_json::json!([
        {"type": "intent", "at": 2000, "viewer": "v1", "id": "ra", "name": "restart_now", "args": {}},
        {"type": "agent", "at": 2100, "host": "h1", "msg": {"type": "slot", "id": "slot-1", "phase": "stopping", "generation": 2}},
        {"type": "agent", "at": 2100, "host": "h1", "msg": {"type": "slot", "id": "slot-2", "phase": "restoring", "generation": 2}},
        {"type": "ec2", "at": 2200, "report": {"id": "ec2-1", "phase": "launching", "generation": 2, "vm": null, "frame_url": null, "timings": {}}},
        {"type": "ec2", "at": 2300, "report": {"id": "ec2-2", "phase": "waiting_capacity", "generation": 2, "vm": null, "frame_url": null, "timings": {}}},
    ]));
    for e in &race {
        s = apply(s, e);
        let v = view(&s);
        assert_eq!(
            v.layout.generation, before.layout.generation,
            "re-tiled mid-race"
        );
        assert_eq!(v.layout.fcvm, half(0.0));
        assert_eq!(v.layout.ec2, half(0.5));
        assert_eq!(places(&v), places(&before));
        check_layout(&s);
    }
    let v = view(&s);
    assert!(
        v.tiles.iter().all(|t| t.phase != "running"),
        "every tile is dark"
    );
    let ec2: Vec<_> = v.tiles.iter().filter(|t| t.half == Half::Ec2).collect();
    assert_eq!(
        ec2.iter()
            .map(|t| (t.slot.as_deref(), t.phase.as_str(), t.frame_url.is_none()))
            .collect::<Vec<_>>(),
        vec![
            (Some("ec2-1"), "launching", true),
            (Some("ec2-2"), "waiting_capacity", true)
        ]
    );
}

/// A side with no live GPU still holds its configured half, as dark tiles with no slot and
/// no stream: before the fcvm host has said hello, and after it has gone.
#[test]
fn a_side_with_no_live_gpu_still_holds_its_half_as_dark_tiles() {
    let config = config_with(serde_json::json!({"ec2_max": 2, "fcvm_tiles": 2}));
    let mut s = State::new(config, 0);
    for e in events(serde_json::json!([
        {"type": "ec2", "at": 1300, "report": {"id": "ec2-1", "phase": "running", "generation": 1, "vm": "instance-1", "frame_url": "ws://ec2-1.example:443/frames", "timings": {}}},
        {"type": "ec2", "at": 1400, "report": {"id": "ec2-2", "phase": "running", "generation": 1, "vm": "instance-2", "frame_url": "ws://ec2-2.example:443/frames", "timings": {}}},
    ])) {
        s = apply(s, &e);
    }
    let dark = |s: &State| {
        let v = view(s);
        assert_eq!(v.layout.fcvm, half(0.0));
        assert_eq!(v.layout.ec2, half(0.5));
        check_layout(s);
        v.tiles
            .iter()
            .filter(|t| t.half == Half::Fcvm)
            .map(|t| {
                (
                    t.id.clone(),
                    t.phase.clone(),
                    t.slot.clone(),
                    t.frame_url.clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    let empty = |k: usize| {
        (
            format!("fcvm:{k}"),
            "waiting_capacity".to_string(),
            None,
            None,
        )
    };
    assert_eq!(
        dark(&s),
        vec![empty(1), empty(2)],
        "before the host's hello"
    );
    let generation = view(&s).layout.generation;
    s = apply(s, &hello(2000, "h1", 2));
    s = apply(s, &running(2100, "h1", "slot-1"));
    s = apply(s, &running(2200, "h1", "slot-2"));
    assert!(view(&s)
        .tiles
        .iter()
        .filter(|t| t.half == Half::Fcvm)
        .all(|t| t.slot.is_some()));
    s = apply(
        s,
        &Event::AgentGone {
            at: 3000,
            host: "h1".into(),
        },
    );
    assert_eq!(
        dark(&s),
        vec![empty(1), empty(2)],
        "after the host has gone"
    );
    assert!(view(&s).layout.generation > generation);
}

/// An EC2 tile without an instance holds its place but has no renderer to report a fit, so
/// it does not hold back the grid budget of the tiles that render.
#[test]
fn an_ec2_tile_waiting_for_capacity_does_not_hold_back_the_budget() {
    let config = config_with(serde_json::json!({"ec2_max": 2, "fcvm_tiles": 1}));
    let mut s = State::new(config, 0);
    let mut list = vec![hello(1000, "h1", 1)];
    list.extend(events(serde_json::json!([
        {"type": "agent", "at": 1100, "host": "h1", "msg": {"type": "slot", "id": "slot-1", "phase": "running", "generation": 1, "vm": "fleet-slot-1", "frame_url": "ws://h1:20001/frames", "timings": {}}},
        {"type": "ec2", "at": 1300, "report": {"id": "ec2-1", "phase": "running", "generation": 1, "vm": "i-1", "frame_url": "wss://ec2-1.example/frames", "timings": {}}},
        {"type": "ec2", "at": 1350, "report": {"id": "ec2-2", "phase": "waiting_capacity", "generation": 1, "vm": null, "frame_url": null, "timings": {}}},
        {"type": "agent", "at": 1400, "host": "h1", "msg": {"type": "health", "id": "slot-1", "budget_fit": 2000000.0, "fps": 30.0, "gpu": null, "t": 10.0}},
        {"type": "ec2_health", "at": 1500, "report": {"id": "ec2-1", "budget_fit": 1000000.0, "fps": 30.0, "gpu": null, "t": 10.1}},
        {"type": "tick", "at": 2000},
        {"type": "tick", "at": 4000},
    ])));
    for e in &list {
        s = apply(s, e);
    }
    let v = view(&s);
    assert_eq!(v.tiles.len(), 3);
    assert!(
        v.budget.is_some(),
        "the budget waits on a tile with no renderer"
    );
}

/// An EC2 place without an instance has no renderer: it is not a GPU of the wake that
/// completes while it waits, and an EC2 tile still launching does not hold the image (and
/// so the poster) as unsettled.
#[test]
fn an_ec2_place_without_an_instance_is_not_counted_as_a_live_gpu() {
    let config = config_with(serde_json::json!({
        "power_managed": true, "idle_scale_down_secs": 60, "ec2_max": 3
    }));
    let mut s = State::new(config, 0);
    for e in events(serde_json::json!([
        {"type": "viewer_joined", "at": 1000, "viewer": "v1", "client": "192.0.2.70"},
        {"type": "visibility", "at": 1100, "viewer": "v1", "visible": false},
        {"type": "tick", "at": 62000},
        {"type": "visibility", "at": 100000, "viewer": "v1", "visible": true},
        {"type": "ec2", "at": 110000, "report": {"id": "ec2-2", "phase": "waiting_capacity", "generation": 1, "vm": null, "frame_url": null, "timings": {}}},
        {"type": "ec2", "at": 111000, "report": {"id": "ec2-3", "phase": "waiting_capacity", "generation": 1, "vm": null, "frame_url": null, "timings": {}}},
        {"type": "ec2", "at": 150000, "report": {"id": "ec2-1", "phase": "running", "generation": 1, "vm": "instance-1", "frame_url": "ws://ec2-1.example:443/frames", "timings": {}}},
    ])) {
        s = apply(s, &e);
    }
    assert_eq!(s.fleet.power.state, "awake");
    assert_eq!(s.fleet.power.wakes.back().map(|w| w.gpus), Some(1));
    s = apply(
        s,
        &events(serde_json::json!([
            {"type": "ec2", "at": 160000, "report": {"id": "ec2-2", "phase": "launching", "generation": 1, "vm": null, "frame_url": null, "timings": {}}},
        ]))[0],
    );
    assert!(fleet_core::apply::image_settled(&s.fleet));
}

/// What a viewer's hover card is built from: a running EC2 tile names its GPU (from the
/// renderer's health), where it runs (the region, never the instance id) and its last launch.
#[test]
fn a_running_ec2_tile_reports_its_gpu_place_and_last_launch() {
    let config = config_with(serde_json::json!({"ec2_max": 1, "fcvm_tiles": 0}));
    let mut s = State::new(config, 0);
    for e in events(serde_json::json!([
        {"type": "tick", "at": 1000},
        {"type": "ec2", "at": 5000, "report": {"id": "ec2-1", "phase": "running", "generation": 1, "vm": "us-east-1/i-example", "frame_url": "wss://ec2-1.example/frames", "place": "EC2 in us-east-1", "timings": {"total_ms": 4000}}},
        {"type": "ec2_health", "at": 6000, "report": {"id": "ec2-1", "fps": 30.0, "gpu": {"name": "Tesla T4", "temp_c": 40.0}, "t": 1.0}},
    ])) {
        s = apply(s, &e);
    }
    let v = view(&s);
    let t = v
        .tiles
        .iter()
        .find(|t| t.id == "ec2:1")
        .expect("the EC2 tile");
    assert_eq!(t.gpu.as_ref().map(|g| g.name.as_str()), Some("Tesla T4"));
    assert_eq!(t.place.as_deref(), Some("EC2 in us-east-1"));
    let l = t
        .last_relaunch
        .as_ref()
        .expect("the wake's launch is recorded");
    assert_eq!((l.clicked_at, l.first_frame_at), (1000, Some(5000)));
    assert_eq!(l.by, "wake");
    assert!(!t.place.as_deref().unwrap().contains("i-example"));
}

/// An fcvm slot that came up at a wake was never relaunched, and still has a last launch.
#[test]
fn an_fcvm_tile_that_came_up_at_a_wake_has_a_last_launch_and_its_hosts_place() {
    let mut s = State::new(Config::default(), 0);
    s = apply(s, &hello(1000, "h1", 1));
    s = apply(
        s,
        &Event::Agent {
            at: 9000,
            host: "h1".into(),
            msg: AgentMsg::Slot(SlotReport {
                id: "slot-1".into(),
                phase: "running".into(),
                generation: 1,
                place: Some("EC2 in us-east-1".into()),
                timings: [("total_ms".to_string(), 3000)].into(),
                ..Default::default()
            }),
        },
    );
    let v = view(&s);
    let t = v
        .tiles
        .iter()
        .find(|t| t.half == Half::Fcvm && t.slot.is_some())
        .unwrap();
    assert_eq!(t.place.as_deref(), Some("EC2 in us-east-1"));
    assert_eq!(t.gpu.as_ref().map(|g| g.name.as_str()), Some("Tesla T4"));
    let l = t.last_relaunch.as_ref().expect("a last launch");
    assert_eq!((l.clicked_at, l.first_frame_at), (6000, Some(9000)));
}

#[test]
fn a_resumed_coordinator_does_not_count_a_decided_vote_again() {
    // A state stored before the stop vote stayed open: after a decision it kept the decided
    // round's offer and votes until the camera arrived, and the new code no longer clears them
    // on arrival, so the camera would fly to the stop it was at.
    let mut state = apply(State::new(Config::default(), 0), &Event::Tick { at: 8000 });
    let d = &mut state.fleet.director;
    let n = d.next.expect("decided at 8 s");
    d.offer.insert(0, n);
    d.offer.truncate(3);
    d.votes.insert(
        "192.0.2.1".into(),
        fleet_core::director::Vote {
            target: n,
            intent: "n1".into(),
            at: 0,
        },
    );
    let mut state = state.resumed(Config::default(), 9000);
    let arrive = state.fleet.director.arrive_next_at().unwrap();
    state = apply(
        state,
        &Event::Tick {
            at: (arrive * 1000.0) as u64,
        },
    );
    assert_eq!(state.fleet.director.current, n);
    let decide = state.fleet.director.decide_at();
    state = apply(
        state,
        &Event::Tick {
            at: (decide * 1000.0) as u64,
        },
    );
    assert_ne!(
        state.fleet.director.next,
        Some(n),
        "the camera would fly to where it is"
    );
}

/// The wall's camera as every renderer draws it from the scenes the coordinator published,
/// a tick a second for `secs` with camera seed `seed`: the union of the keyframes, in order,
/// and each stop the camera arrived at (scene time, viewpoint).
fn published_camera(seed: u64, secs: u64) -> (fleet_core::scene::Scene, Vec<(f64, usize)>) {
    let mut state = State::new(config_with(serde_json::json!({ "camera_seed": seed })), 0);
    let mut all: Vec<fleet_core::scene::Keyframe> = Vec::new();
    let mut stops = vec![(0.0, state.fleet.director.current)];
    for s in 1..=secs {
        let step = state.fleet.director.step;
        state = apply(state, &Event::Tick { at: s * 1000 });
        let d = &state.fleet.director;
        if d.step != step {
            stops.push((d.arrive_t, d.current));
        }
        for k in &state.fleet.scene.camera {
            if all.last().is_none_or(|l| k.t > l.t) {
                all.push(k.clone());
            }
        }
    }
    let mut scene = state.fleet.scene.clone();
    scene.camera = all;
    (scene, stops)
}

fn dist(p: [f64; 3], q: [f64; 3]) -> f64 {
    ((p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2)).sqrt()
}

#[test]
fn the_wall_camera_never_stands_still() {
    // Sampled every 0.1 s as the renderer interpolates the path: the camera may slow but
    // must not stop (moving under 5 m/s and turning under 0.2 degrees a second) for half a
    // second. The hold-based director stood still for 7 s at every viewpoint.
    let (scene, _) = published_camera(1, 900);
    let (mut still, mut longest, mut at) = (0.0f64, 0.0f64, 0.0);
    let mut t = 2.0;
    while t < 880.0 {
        let (a, b) = (scene.camera_at(t), scene.camera_at(t + 0.1));
        let speed = dist(a.pos, b.pos) / 0.1;
        let (va, vb) = (
            [
                a.look[0] - a.pos[0],
                a.look[1] - a.pos[1],
                a.look[2] - a.pos[2],
            ],
            [
                b.look[0] - b.pos[0],
                b.look[1] - b.pos[1],
                b.look[2] - b.pos[2],
            ],
        );
        let cos = (va[0] * vb[0] + va[1] * vb[1] + va[2] * vb[2])
            / (dist(va, [0.0; 3]) * dist(vb, [0.0; 3]));
        let turn = cos.clamp(-1.0, 1.0).acos().to_degrees() / 0.1;
        if speed < 0.005 && turn < 0.2 {
            still += 0.1;
            if still > longest {
                longest = still;
                at = t;
            }
        } else {
            still = 0.0;
        }
        t += 0.1;
    }
    assert!(
        longest < 0.5,
        "the camera stood still for {longest:.1} s, until t={at:.1}"
    );
}

#[test]
fn the_wall_camera_velocity_is_continuous_at_every_keyframe() {
    // Where a flight meets a stop (and everywhere else) the velocity just before a keyframe
    // equals the velocity just after it, to 0.5% of the speed there.
    let (scene, _) = published_camera(2, 900);
    let h = 1e-5;
    let vel = |t0: f64, t1: f64| {
        let (a, b) = (scene.camera_at(t0).pos, scene.camera_at(t1).pos);
        [
            (b[0] - a[0]) / (t1 - t0),
            (b[1] - a[1]) / (t1 - t0),
            (b[2] - a[2]) / (t1 - t0),
        ]
    };
    let ks: Vec<f64> = scene.camera.iter().map(|k| k.t).collect();
    for &t in &ks[2..ks.len() - 2] {
        let (before, after) = (vel(t - 2.0 * h, t - h), vel(t + h, t + 2.0 * h));
        let speed = dist(before, [0.0; 3]).max(dist(after, [0.0; 3])).max(0.02);
        let jump = dist(before, after) / speed;
        assert!(
            jump < 0.005,
            "a velocity jump of {:.1}% at t={t}",
            jump * 100.0
        );
    }
}

#[test]
fn the_wall_program_does_not_repeat_within_ten_minutes() {
    // The hold-based director flew one permutation of its eight viewpoints in a loop, so
    // the same stops came round in the same order every two and a half minutes.
    for seed in 1..=16 {
        let (_, stops) = published_camera(seed, 1800);
        for i in 0..stops.len() {
            for j in i + 1..stops.len() {
                if stops[j].0 - stops[i].0 > 600.0 || j + 4 > stops.len() {
                    continue;
                }
                let run = |k: usize| stops[k..k + 4].iter().map(|s| s.1).collect::<Vec<_>>();
                assert_ne!(
                    run(i),
                    run(j),
                    "seed {seed}: the stops from t={} come round again at t={}",
                    stops[i].0,
                    stops[j].0
                );
            }
        }
    }
}
