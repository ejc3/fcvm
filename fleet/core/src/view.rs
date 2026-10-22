//! What the coordinator sends out, derived from the state: the browser snapshot, each
//! host's slot assignment, the EC2 driver's assignment and the shared scene.

use crate::protocol::{AssignSlot, SharedScene};
use crate::scene::Scene;
use crate::state::*;
use crate::tiling::{Half, Rect};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize)]
pub struct TileView {
    /// The tile's place: `fcvm:<k>` or `ec2:<k>`, 1-based, row-major.
    pub id: String,
    pub half: Half,
    pub rect: Rect,
    /// `waiting_capacity` (dark), `borrowed` (lent to a visitor, drawn black),
    /// `relaunching` (desired generation not yet reported), or the slot's reported phase.
    pub phase: String,
    /// The slot behind the tile (`slot-3`, `ec2-2`); null when dark.
    pub slot: Option<String>,
    pub frame_url: Option<String>,
    pub gpu: Option<Gpu>,
    pub host: Option<String>,
    pub vm: Option<String>,
    pub generation: Option<u64>,
    pub fps: Option<f64>,
    pub gpu_stats: Option<serde_json::Value>,
    pub error: Option<String>,
    pub last_relaunch: Option<RelaunchView>,
    /// While the tile's GPU is lent to a visitor's private session.
    pub borrowed: Option<BorrowedView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BorrowedView {
    pub session: String,
    pub viewer: String,
    pub since: u64,
    pub until: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelaunchView {
    pub clicked_at: u64,
    pub by: String,
    pub first_frame_at: Option<u64>,
    pub phases: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostView {
    pub name: String,
    pub arch: String,
    pub gpus: Vec<Gpu>,
    pub draining: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FleetView {
    pub hosts: Vec<HostView>,
    /// GPUs across all hosts, draining ones included.
    pub gpus: usize,
    /// fcvm slots rendering or on standby (not draining).
    pub fcvm_gpus: usize,
    /// EC2 tiles holding an instance.
    pub ec2_instances: usize,
    /// Rectangles per half.
    pub n: usize,
    pub max_tiles: usize,
    pub standby: Vec<String>,
    /// Wall tiles lent to visitors now, and how many may be at once.
    pub borrowed: usize,
    pub borrow_cap: usize,
}

/// The race clock everyone sees.
#[derive(Debug, Clone, Serialize)]
pub struct RaceClockView {
    /// The owner's cadence; null when races run only on `restart_now`.
    pub interval_secs: Option<u64>,
    pub next_race_at: Option<u64>,
    /// `restart_now` is refused until then.
    pub cooldown_until: Option<u64>,
    pub cooldown_secs: u64,
    /// The race in progress, if any.
    pub running: Option<u64>,
}

/// One half of the scoreboard: only measured numbers, each with what it was measured on.
#[derive(Debug, Clone, Serialize)]
pub struct HalfScore {
    /// Click to the last tile's first frame (less EC2 capacity waits), in the latest race
    /// whose half finished.
    pub last_ms: Option<u64>,
    pub last_race: Option<u64>,
    /// Tiles timed in that race.
    pub last_measured_on: Option<usize>,
    pub last_capacity_wait_ms: Option<u64>,
    /// Median over the last finished races (up to 20).
    pub median_ms: Option<u64>,
    /// Races and the fewest and most tiles they were measured on.
    pub races: usize,
    pub measured_on_min: Option<usize>,
    pub measured_on_max: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Scoreboard {
    pub fcvm: HalfScore,
    pub ec2: HalfScore,
}

#[derive(Debug, Clone, Serialize)]
pub struct DirectorView {
    pub viewpoint: String,
    /// Scene seconds: arrived, holding until, the vote closes at.
    pub arrived_t: f64,
    pub hold_until_t: f64,
    pub decide_at_t: f64,
    /// The next hold, once the vote closed.
    pub next: Option<String>,
    pub arrive_next_t: Option<f64>,
    /// What `nudge` may name now, and the votes so far.
    pub offer: Vec<String>,
    pub votes: BTreeMap<String, usize>,
    pub seed: u64,
}

/// What can be wrong, each with its own kind so the page can label it.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FailureView {
    /// The host got a spot notice or went away; its tiles moved to the others.
    HostReclaimed {
        host: String,
        since: u64,
        retiling: bool,
    },
    /// A coordinator component is not working (the text check, the state file).
    CoordinatorDegraded {
        component: String,
        reason: String,
        since: u64,
    },
    /// A tile's VM failed; the agent tries again at `retry_at`.
    TileFailed {
        tile: String,
        slot: String,
        error: Option<String>,
        retry_at: Option<u64>,
    },
}

/// A viewer's own GPU, as everyone sees it (no frame URL, no token).
#[derive(Debug, Clone, Serialize)]
pub struct ClaimView {
    pub session: String,
    pub viewer: String,
    pub slot: String,
    pub gpu: Option<Gpu>,
    pub host: Option<String>,
    pub claimed_at: u64,
    pub expires_at: u64,
    pub first_frame_at: Option<u64>,
    /// Click to first frame.
    pub click_to_first_frame_ms: Option<u64>,
    /// The wall tile it borrowed (`fcvm:3`); null for a standby GPU.
    pub borrowed: Option<String>,
}

/// A region an aggregator encodes as one stream: a GPU host's band, or the EC2 half.
#[derive(Debug, Clone, Serialize)]
pub struct AggregateView {
    /// `agg:<host>` or `agg:ec2`.
    pub id: String,
    pub half: Half,
    pub host: Option<String>,
    pub rect: Rect,
    /// The aggregator's frame WebSocket; null until it is serving.
    pub frame_url: Option<String>,
    /// The tiles inside the region, by tile id.
    pub tiles: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ViewersView {
    pub count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct View {
    pub tiles: Vec<TileView>,
    pub aggregates: Vec<AggregateView>,
    pub fleet: FleetView,
    pub scene: Scene,
    pub image_aspect: f64,
    pub race: RaceClockView,
    pub scoreboard: Scoreboard,
    /// The latest races, newest last (up to 10), with every tile's phases.
    pub races: Vec<Race>,
    pub director: DirectorView,
    pub failures: Vec<FailureView>,
    pub claims: Vec<ClaimView>,
    pub viewers: ViewersView,
    pub actions: Vec<Action>,
}

fn tile_phase(s: &Slot) -> String {
    if s.observed.generation < s.generation && s.observed.phase != "pending" {
        "relaunching".into()
    } else {
        s.observed.phase.clone()
    }
}

fn tile(id: String, half: Half, rect: Rect, slot: Option<&Slot>) -> TileView {
    let lent = slot.and_then(|s| {
        let c = s.claim.as_ref().filter(|c| c.borrowed.is_some())?;
        Some(BorrowedView {
            session: c.session.clone(),
            viewer: c.viewer.clone(),
            since: c.claimed_at,
            until: c.expires_at,
        })
    });
    match slot {
        None => TileView {
            id,
            half,
            rect,
            phase: "waiting_capacity".into(),
            slot: None,
            frame_url: None,
            gpu: None,
            host: None,
            vm: None,
            generation: None,
            fps: None,
            gpu_stats: None,
            error: None,
            last_relaunch: None,
            borrowed: None,
        },
        Some(s) => TileView {
            id,
            half,
            rect,
            phase: if lent.is_some() {
                "borrowed".into()
            } else {
                tile_phase(s)
            },
            slot: Some(s.id.clone()),
            // The private VM's frames are for its viewer alone.
            frame_url: if lent.is_some() {
                None
            } else {
                s.observed.frame_url.clone()
            },
            gpu: s.gpu.clone(),
            host: s.host.clone(),
            vm: s.observed.vm.clone(),
            generation: Some(s.generation),
            fps: s.observed.fps,
            gpu_stats: s.observed.gpu_stats.clone(),
            error: s.observed.error.clone(),
            last_relaunch: s.last_relaunch.as_ref().map(|r| RelaunchView {
                clicked_at: r.clicked_at,
                by: r.by.clone(),
                first_frame_at: r.first_frame_at,
                phases: r.phases.clone(),
            }),
            borrowed: lent,
        },
    }
}

/// The browser snapshot's `state`.
pub fn view(state: &State) -> View {
    let f = &state.fleet;
    let c = &state.config;
    let mut tiles = Vec::with_capacity(f.n * 2);
    for half in [Half::Fcvm, Half::Ec2] {
        let dx = if half == Half::Ec2 { 0.5 } else { 0.0 };
        let rects: Vec<Rect> = f
            .layout
            .iter()
            .map(|r| Rect { x: r.x + dx, ..*r })
            .collect();
        let units = match half {
            Half::Fcvm => &f.slots,
            Half::Ec2 => &f.ec2,
        };
        for (k, rect) in rects.into_iter().enumerate() {
            let slot = units.values().find(|s| s.position == Some(k));
            tiles.push(tile(
                format!("{}:{}", half.as_str(), k + 1),
                half,
                rect,
                slot,
            ));
        }
    }
    let standby = f
        .slots
        .values()
        .filter(|s| s.position.is_none() && s.claim.is_none())
        .map(|s| s.id.clone())
        .collect();
    let fcvm_gpus = f
        .slots
        .values()
        .filter(|s| {
            s.host
                .as_ref()
                .and_then(|h| f.hosts.get(h))
                .is_some_and(|h| !h.draining)
        })
        .count();
    let tile_id = |half: Half, s: &Slot| s.position.map(|k| format!("{}:{}", half.as_str(), k + 1));
    let mut aggregates: Vec<AggregateView> = f
        .hosts
        .values()
        .filter_map(|h| {
            let rect = h.region?;
            let mut placed: Vec<(usize, String)> = f
                .slots
                .values()
                .filter(|s| s.host.as_deref() == Some(h.name.as_str()))
                .filter_map(|s| Some((s.position?, tile_id(Half::Fcvm, s)?)))
                .collect();
            placed.sort();
            Some(AggregateView {
                id: format!("agg:{}", h.name),
                half: Half::Fcvm,
                host: Some(h.name.clone()),
                rect,
                frame_url: h.aggregate_url.clone(),
                tiles: placed.into_iter().map(|(_, id)| id).collect(),
            })
        })
        .collect();
    aggregates.sort_by(|a, b| a.rect.y.total_cmp(&b.rect.y));
    if let Some(url) = &c.ec2_aggregate_url {
        aggregates.push(AggregateView {
            id: "agg:ec2".into(),
            half: Half::Ec2,
            host: None,
            rect: crate::tiling::half_rect(Half::Ec2),
            frame_url: Some(url.clone()),
            tiles: (1..=f.n).map(|k| format!("ec2:{k}")).collect(),
        });
    }
    View {
        tiles,
        aggregates,
        fleet: FleetView {
            hosts: f
                .hosts
                .values()
                .map(|h| HostView {
                    name: h.name.clone(),
                    arch: h.arch.clone(),
                    gpus: h.gpus.clone(),
                    draining: h.draining,
                })
                .collect(),
            gpus: f.hosts.values().map(|h| h.gpus.len()).sum(),
            fcvm_gpus,
            ec2_instances: f.ec2.values().filter(|s| s.observed.vm.is_some()).count(),
            n: f.n,
            max_tiles: c.max_tiles,
            standby,
            borrowed: f
                .slots
                .values()
                .filter(|s| s.claim.as_ref().is_some_and(|k| k.borrowed.is_some()))
                .count(),
            borrow_cap: c.borrow_cap(f.n),
        },
        scene: f.scene.clone(),
        image_aspect: c.image_aspect,
        race: RaceClockView {
            interval_secs: c.race_interval_secs,
            next_race_at: f.next_race_at,
            cooldown_until: f.cooldown_until,
            cooldown_secs: c.restart_cooldown_secs,
            running: f
                .races
                .back()
                .filter(|r| {
                    r.fcvm.done_at.is_none() && !r.fcvm.tiles.is_empty()
                        || r.ec2.done_at.is_none() && !r.ec2.tiles.is_empty()
                })
                .map(|r| r.id),
        },
        scoreboard: Scoreboard {
            fcvm: score(f.races.iter().map(|r| (r.id, &r.fcvm))),
            ec2: score(f.races.iter().map(|r| (r.id, &r.ec2))),
        },
        races: f.races.iter().rev().take(10).rev().cloned().collect(),
        director: director_view(&f.director),
        failures: failures(state),
        claims: f
            .slots
            .values()
            .filter_map(|s| {
                let k = s.claim.as_ref()?;
                Some(ClaimView {
                    session: k.session.clone(),
                    viewer: k.viewer.clone(),
                    slot: s.id.clone(),
                    gpu: s.gpu.clone(),
                    host: s.host.clone(),
                    claimed_at: k.claimed_at,
                    expires_at: k.expires_at,
                    first_frame_at: k.first_frame_at,
                    click_to_first_frame_ms: k.first_frame_at.map(|t| t - k.claimed_at),
                    borrowed: k.borrowed.clone(),
                })
            })
            .collect(),
        viewers: ViewersView {
            count: f.viewers.len(),
        },
        actions: f.actions.iter().cloned().collect(),
    }
}

fn score<'a>(halves: impl DoubleEndedIterator<Item = (u64, &'a HalfRace)>) -> HalfScore {
    let done: Vec<(u64, &HalfRace)> = halves.filter(|(_, h)| h.time_ms.is_some()).collect();
    let recent: Vec<&(u64, &HalfRace)> = done
        .iter()
        .rev()
        .take(crate::apply::SCOREBOARD_RACES)
        .collect();
    let mut times: Vec<u64> = recent.iter().map(|(_, h)| h.time_ms.unwrap()).collect();
    times.sort_unstable();
    let median = if times.is_empty() {
        None
    } else if times.len() % 2 == 1 {
        Some(times[times.len() / 2])
    } else {
        Some((times[times.len() / 2 - 1] + times[times.len() / 2]) / 2)
    };
    let last = done.last();
    HalfScore {
        last_ms: last.and_then(|(_, h)| h.time_ms),
        last_race: last.map(|(id, _)| *id),
        last_measured_on: last.map(|(_, h)| h.tiles.len()),
        last_capacity_wait_ms: last.map(|(_, h)| h.capacity_wait_ms),
        median_ms: median,
        races: recent.len(),
        measured_on_min: recent.iter().map(|(_, h)| h.tiles.len()).min(),
        measured_on_max: recent.iter().map(|(_, h)| h.tiles.len()).max(),
    }
}

fn director_view(d: &crate::director::Director) -> DirectorView {
    let vps = crate::director::viewpoints();
    DirectorView {
        viewpoint: vps[d.current].name.to_string(),
        arrived_t: d.arrive_t,
        hold_until_t: d.hold_until(),
        decide_at_t: d.decide_at(),
        next: d.next.map(|n| vps[n].name.to_string()),
        arrive_next_t: d.arrive_next_at(),
        offer: d.offer.iter().map(|i| vps[*i].name.to_string()).collect(),
        votes: d.tally(),
        seed: d.seed,
    }
}

fn failures(state: &State) -> Vec<FailureView> {
    let f = &state.fleet;
    let mut out: Vec<FailureView> = f
        .reclaimed
        .iter()
        .map(|(h, (since, _))| FailureView::HostReclaimed {
            host: h.clone(),
            since: *since,
            retiling: true,
        })
        .collect();
    out.extend(
        f.degraded
            .iter()
            .map(|(c, (r, since))| FailureView::CoordinatorDegraded {
                component: c.clone(),
                reason: r.clone(),
                since: *since,
            }),
    );
    for s in f.slots.values().chain(f.ec2.values()) {
        if s.observed.phase == "failed" {
            out.push(FailureView::TileFailed {
                tile: s
                    .position
                    .map(|k| format!("{}:{}", s.half.as_str(), k + 1))
                    .unwrap_or_else(|| "standby".into()),
                slot: s.id.clone(),
                error: s.observed.error.clone(),
                retry_at: s.observed.retry_at,
            });
        }
    }
    out
}

/// Each host's complete assignment.
pub fn host_assignments(state: &State) -> BTreeMap<String, Vec<AssignSlot>> {
    let mut out: BTreeMap<String, Vec<AssignSlot>> = state
        .fleet
        .hosts
        .keys()
        .map(|h| (h.clone(), Vec::new()))
        .collect();
    for s in state.fleet.slots.values() {
        let (Some(host), Some(gpu)) = (&s.host, &s.gpu) else {
            continue;
        };
        if let Some(list) = out.get_mut(host) {
            list.push(AssignSlot {
                id: s.id.clone(),
                sbdf: gpu.sbdf.clone(),
                generation: s.generation,
                // A claimed GPU draws the whole image for its viewer, so it is not part of
                // its host's band (the aggregator draws the tile black).
                rect: if s.claim.is_some() { None } else { s.rect },
                private_camera: s.claim.as_ref().map(|c| c.camera.clone()),
            });
        }
    }
    out
}

/// Each host's region (its aggregator's rectangle), when any of its slots is placed.
pub fn host_regions(state: &State) -> BTreeMap<String, Option<Rect>> {
    state
        .fleet
        .hosts
        .iter()
        .map(|(n, h)| (n.clone(), h.region))
        .collect()
}

/// The EC2 tiles the driver must run.
pub fn ec2_assignment(state: &State) -> Vec<AssignSlot> {
    state
        .fleet
        .ec2
        .values()
        .map(|s| AssignSlot {
            id: s.id.clone(),
            sbdf: String::new(),
            generation: s.generation,
            rect: s.rect,
            private_camera: None,
        })
        .collect()
}

pub fn shared_scene(state: &State) -> SharedScene {
    SharedScene::new(&state.fleet.scene, state.config.image_aspect)
}
