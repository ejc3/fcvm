//! Properties of `apply` over long generated sequences, beyond the hand-written vectors.

use fleet_core::protocol::{AgentMsg, SlotReport};
use fleet_core::state::Gpu;
use fleet_core::tiling::{half_rect, Half, Rect};
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
        },
    }
}

/// Each half's tiles exactly cover it, the EC2 half mirrors the fcvm half, every host's
/// tiles lie inside its region and the regions do not overlap, every positioned slot holds
/// a distinct tile, and no GPU backs two slots.
fn check_layout(state: &State) {
    let v = view(state);
    let n = v.fleet.n;
    assert_eq!(v.tiles.len(), 2 * n);
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
    for (half, offset) in [(Half::Fcvm, 0), (Half::Ec2, n)] {
        let tiles: Vec<Rect> = v.tiles[offset..offset + n].iter().map(|t| t.rect).collect();
        let area: f64 = tiles.iter().map(|r| r.w * r.h).sum();
        if n > 0 {
            assert!((area - 0.5).abs() < 1e-9, "{half:?} area {area}");
        }
        for (i, a) in tiles.iter().enumerate() {
            assert!(inside(a, &half_rect(half)));
            for b in &tiles[i + 1..] {
                assert!(!overlap(a, b), "{a:?} overlaps {b:?}");
            }
        }
    }
    for k in 0..n {
        let (l, r) = (v.tiles[k].rect, v.tiles[n + k].rect);
        assert_eq!((l.x + 0.5, l.y, l.w, l.h), (r.x, r.y, r.w, r.h));
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
        s = apply(s, &hello(i * 10, &format!("host-{i}"), 1));
        let v = view(&s);
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
        if let Some(first) = v.tiles.first() {
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
