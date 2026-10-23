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
    /// Its rectangle from `layout.from_frame` on.
    pub rect: Rect,
    /// Its rectangle before, when the current layout generation moved it (null for a tile
    /// that is new or did not move).
    pub previous_rect: Option<Rect>,
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
    /// Why the tile is not running when that is a fault.
    pub failure: Option<TileFailure>,
    pub last_relaunch: Option<RelaunchView>,
    /// While the tile's GPU is lent to a visitor's private session.
    pub borrowed: Option<BorrowedView>,
}

/// The current layout generation: from frame `from_frame` (scene time `from_t`) every
/// tile draws its `rect`; before it, a tile with a `previous_rect` drew that.
#[derive(Debug, Clone, Serialize)]
pub struct LayoutView {
    pub generation: u64,
    pub from_frame: i64,
    pub from_t: f64,
    /// Each side's rectangle; null for a side with no GPU.
    pub fcvm: Option<Rect>,
    pub ec2: Option<Rect>,
}

/// A tile's fault: `relaunch_failed` (its VM did not come back; the agent tries again at
/// `retry_at`).
#[derive(Debug, Clone, Serialize)]
pub struct TileFailure {
    pub kind: String,
    pub message: String,
    pub retry_at: Option<u64>,
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
    /// Tiles on the fcvm side and on the EC2 side (one per live GPU).
    pub n: usize,
    pub ec2_n: usize,
    /// EC2 tiles still waiting for an instance (capacity): not on the wall.
    pub ec2_waiting: usize,
    pub max_tiles: usize,
    /// The standby pool: GPUs beyond `max_tiles`, kept reset, scrubbed and bound with no
    /// VM, and how many of them have reported ready.
    pub standby: Vec<String>,
    pub standby_ready: usize,
    pub power: PowerView,
}

/// Totals since the fleet was first started.
#[derive(Debug, Clone, Serialize)]
pub struct CountersView {
    pub restores: u64,
    pub restores_last_hour: u64,
    pub scrubs: u64,
    pub scrub_failures: u64,
}

/// Wall tiles lent to visitors: at most `cap` at once; the next comes back at
/// `next_return_at`.
#[derive(Debug, Clone, Serialize)]
pub struct BorrowView {
    pub cap: usize,
    pub lent: usize,
    pub next_return_at: Option<u64>,
}

/// The latest race, as the page draws it: when it started, who started it (`schedule`
/// for the owner's cadence), each half's progress and, once done, its time.
#[derive(Debug, Clone, Serialize)]
pub struct RelaunchAllView {
    pub race: u64,
    pub started_at: u64,
    pub by: String,
    pub fcvm_done_at: Option<u64>,
    pub ec2_done_at: Option<u64>,
    /// Tiles still to reach their first frame.
    pub fcvm_pending: usize,
    pub ec2_pending: usize,
    /// Tiles timed in each half.
    pub fcvm_gpus: usize,
    pub ec2_gpus: usize,
    /// Each half's time once done: click to the last first frame, less EC2 capacity waits.
    pub fcvm_ms: Option<u64>,
    pub ec2_ms: Option<u64>,
    /// EC2 capacity waits finished so far (kept out of `ec2_ms`), and since when the
    /// half's current wait has run (null when none runs).
    pub ec2_capacity_wait_ms: u64,
    pub ec2_waiting_since: Option<u64>,
}

/// One half of the scoreboard; null until a race of that half has finished. Only
/// measured numbers, capacity waits left out of the times.
#[derive(Debug, Clone, Serialize)]
pub struct HalfScore {
    /// Median over the last finished races (up to 20).
    pub median_ms: u64,
    pub last_ms: u64,
    /// Races the median is over.
    pub races: usize,
    /// Tiles timed in the last race.
    pub measured_on: usize,
    pub last_capacity_wait_ms: u64,
    pub last_race: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Scoreboard {
    pub fcvm: Option<HalfScore>,
    pub ec2: Option<HalfScore>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NudgeTarget {
    /// The viewpoint's name, what `nudge` sends as `target`.
    pub id: String,
    pub label: String,
    pub votes: usize,
}

/// The directed camera: where it can hold next, with this hold's votes.
#[derive(Debug, Clone, Serialize)]
pub struct NudgesView {
    pub targets: Vec<NudgeTarget>,
    /// The viewpoint the camera holds on now, or null while it flies.
    pub holding: Option<String>,
    /// While holding, when the camera leaves for the next hold (Unix ms).
    pub next_hold_at: Option<u64>,
    /// The next hold once the vote has closed, and when the camera arrives there.
    pub next: Option<String>,
    pub arrive_next_at: Option<u64>,
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
    /// A coordinator component is not working (the text check).
    CoordinatorDegraded {
        component: String,
        reason: String,
        since: u64,
    },
    /// A GPU out of rotation after a failed reset, scrub or attach, until an operator
    /// releases it.
    GpuQuarantined {
        host: String,
        sbdf: String,
        error: String,
        since: u64,
    },
}

/// The slot's rectangle before the current layout generation moved it.
fn moved_from(state: &State, s: &Slot) -> Option<Rect> {
    (s.rect_generation == state.fleet.layout.generation)
        .then_some(s.previous_rect)
        .flatten()
}

/// In the standby pool: an unplaced, unclaimed GPU on a host that is not draining, and not
/// quarantined.
fn in_standby(state: &State, s: &Slot) -> bool {
    let f = &state.fleet;
    s.position.is_none()
        && s.claim.is_none()
        && !crate::apply::is_quarantined(f, s)
        && s.host
            .as_ref()
            .and_then(|h| f.hosts.get(h))
            .is_some_and(|h| !h.draining)
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
    /// Connected viewers whose page is visible (or has not said).
    pub active: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct WakePhase {
    /// `instance_pending`, `booted`, `store_ready`, `agent_connected`, `tiles_rendering`.
    pub phase: String,
    pub at: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostWakeView {
    /// The EC2 instance id (the host's name once its agent connects).
    pub host: String,
    /// The phases reached so far, in order.
    pub phases: Vec<WakePhase>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PowerView {
    /// `awake`, `asleep` or `waking`.
    pub state: String,
    pub idle_since: Option<u64>,
    pub asleep_since: Option<u64>,
    /// When the current (or last) wake began: the first active viewer.
    pub woke_at: Option<u64>,
    /// While waking: each GPU host's progress.
    pub hosts: Vec<HostWakeView>,
    /// The last completed wake, and the last 20.
    pub last_wake: Option<Wake>,
    pub wakes: Vec<Wake>,
}

#[derive(Debug, Clone, Serialize)]
pub struct View {
    pub layout: LayoutView,
    /// The grid's render budget (pixel-samples per frame per tile) and the scene time it
    /// took effect at; null until every tile has reported a fit.
    pub budget: Option<u64>,
    pub budget_from_t: Option<f64>,
    pub tiles: Vec<TileView>,
    pub aggregates: Vec<AggregateView>,
    pub fleet: FleetView,
    pub scene: Scene,
    pub image_aspect: f64,
    /// When the owner's cadence races next, and until when `restart_now` is refused.
    pub next_race_at: Option<u64>,
    pub cooldown_until: Option<u64>,
    pub relaunch_all: Option<RelaunchAllView>,
    pub scoreboard: Scoreboard,
    /// The latest races, newest last (up to 10), with every tile's phases.
    pub races: Vec<Race>,
    pub nudges: NudgesView,
    pub borrow: BorrowView,
    /// Problems that are not one tile's: hosts reclaimed, coordinator components down.
    pub failures: Vec<FailureView>,
    pub claims: Vec<ClaimView>,
    pub counters: CountersView,
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
            previous_rect: None,
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
            failure: None,
            last_relaunch: None,
            borrowed: None,
        },
        Some(s) => TileView {
            id,
            half,
            rect,
            previous_rect: None,
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
            failure: (s.observed.phase == "failed" && s.observed.generation >= s.generation).then(
                || TileFailure {
                    kind: "relaunch_failed".into(),
                    message: s.observed.error.clone().unwrap_or_default(),
                    retry_at: s.observed.retry_at,
                },
            ),
        },
    }
}

/// The browser snapshot's `state`.
pub fn view(state: &State) -> View {
    let f = &state.fleet;
    let c = &state.config;
    let mut tiles = Vec::with_capacity(f.n + f.ec2_n);
    for (half, units) in [(Half::Fcvm, &f.slots), (Half::Ec2, &f.ec2)] {
        let mut placed: Vec<&Slot> = units.values().filter(|s| s.position.is_some()).collect();
        placed.sort_by_key(|s| s.position);
        for s in placed {
            let mut t = tile(
                format!("{}:{}", half.as_str(), s.position.unwrap() + 1),
                half,
                s.rect.expect("a placed slot has a rectangle"),
                Some(s),
            );
            t.previous_rect = (s.rect_generation == f.layout.generation)
                .then_some(s.previous_rect)
                .flatten();
            tiles.push(t);
        }
    }
    let pool: Vec<&Slot> = f.slots.values().filter(|s| in_standby(state, s)).collect();
    let standby = pool.iter().map(|s| s.id.clone()).collect();
    let standby_ready = pool
        .iter()
        .filter(|s| s.observed.phase == "standby")
        .count();
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
    if let (Some(url), Some(rect)) = (&c.ec2_aggregate_url, f.layout.ec2) {
        aggregates.push(AggregateView {
            id: "agg:ec2".into(),
            half: Half::Ec2,
            host: None,
            rect,
            frame_url: Some(url.clone()),
            tiles: (1..=f.ec2_n).map(|k| format!("ec2:{k}")).collect(),
        });
    }
    View {
        budget: f.budget.current,
        budget_from_t: f.budget.from_t,
        layout: LayoutView {
            generation: f.layout.generation,
            from_frame: f.layout.from_frame,
            from_t: f.layout.from_frame as f64 / 30.0,
            fcvm: f.layout.fcvm,
            ec2: f.layout.ec2,
        },
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
            ec2_n: f.ec2_n,
            ec2_waiting: f.ec2.values().filter(|s| s.observed.vm.is_none()).count(),
            max_tiles: c.max_tiles,
            standby,
            standby_ready,
            power: power_view(&f.power),
        },
        scene: f.scene.clone(),
        image_aspect: c.image_aspect,
        next_race_at: f.next_race_at,
        cooldown_until: f.cooldown_until,
        relaunch_all: f.races.back().map(relaunch_all),
        scoreboard: Scoreboard {
            fcvm: score(f.races.iter().map(|r| (r.id, &r.fcvm))),
            ec2: score(f.races.iter().map(|r| (r.id, &r.ec2))),
        },
        races: f.races.iter().rev().take(10).rev().cloned().collect(),
        nudges: nudges(&f.director, f.scene.epoch_unix_ms),
        borrow: borrow(state),
        failures: failures(state),
        counters: CountersView {
            restores: f.counters.restores,
            restores_last_hour: f.counters.restores_by_minute.iter().map(|(_, n)| n).sum(),
            scrubs: f.counters.scrubs,
            scrub_failures: f.counters.scrub_failures,
        },
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
            active: f.viewers.keys().filter(|v| !f.hidden.contains(*v)).count(),
        },

        actions: f.actions.iter().cloned().collect(),
    }
}

fn score<'a>(halves: impl DoubleEndedIterator<Item = (u64, &'a HalfRace)>) -> Option<HalfScore> {
    let done: Vec<(u64, &HalfRace)> = halves.filter(|(_, h)| h.time_ms.is_some()).collect();
    let (last_id, last) = *done.last()?;
    let mut times: Vec<u64> = done
        .iter()
        .rev()
        .take(crate::apply::SCOREBOARD_RACES)
        .map(|(_, h)| h.time_ms.unwrap())
        .collect();
    times.sort_unstable();
    let median = if times.len() % 2 == 1 {
        times[times.len() / 2]
    } else {
        (times[times.len() / 2 - 1] + times[times.len() / 2]) / 2
    };
    Some(HalfScore {
        median_ms: median,
        last_ms: last.time_ms.unwrap(),
        races: times.len(),
        measured_on: last.tiles.len(),
        last_capacity_wait_ms: last.capacity_wait_ms,
        last_race: last_id,
    })
}

fn power_view(p: &Power) -> PowerView {
    PowerView {
        state: p.state.clone(),
        idle_since: p.idle_since,
        asleep_since: p.asleep_since,
        woke_at: p.woke_at,
        hosts: p
            .hosts
            .iter()
            .map(|(id, w)| HostWakeView {
                host: id.clone(),
                phases: [
                    ("instance_pending", w.pending_at),
                    ("booted", w.booted_at),
                    ("store_ready", w.store_ready_at),
                    ("agent_connected", w.agent_at),
                    ("tiles_rendering", w.rendering_at),
                ]
                .into_iter()
                .filter_map(|(phase, at)| {
                    Some(WakePhase {
                        phase: phase.into(),
                        at: at?,
                    })
                })
                .collect(),
            })
            .collect(),
        last_wake: p.wakes.back().cloned(),
        wakes: p.wakes.iter().cloned().collect(),
    }
}

fn relaunch_all(r: &Race) -> RelaunchAllView {
    let pending = |h: &HalfRace| {
        h.tiles
            .values()
            .filter(|t| t.first_frame_at.is_none())
            .count()
    };
    let waiting_since = r
        .ec2
        .tiles
        .values()
        .filter(|t| t.first_frame_at.is_none())
        .filter_map(|t| t.waiting_since)
        .min();
    RelaunchAllView {
        race: r.id,
        started_at: r.started_at,
        by: r.by.clone(),
        fcvm_done_at: r.fcvm.done_at,
        ec2_done_at: r.ec2.done_at,
        fcvm_pending: pending(&r.fcvm),
        ec2_pending: pending(&r.ec2),
        fcvm_gpus: r.fcvm.tiles.len(),
        ec2_gpus: r.ec2.tiles.len(),
        fcvm_ms: r.fcvm.time_ms,
        ec2_ms: r.ec2.time_ms,
        ec2_capacity_wait_ms: r
            .ec2
            .tiles
            .values()
            .map(|t| t.capacity_wait_ms)
            .max()
            .unwrap_or(0),
        ec2_waiting_since: waiting_since,
    }
}

/// Scene seconds as Unix milliseconds.
fn unix_ms(epoch_unix_ms: u64, t: f64) -> u64 {
    (epoch_unix_ms as f64 + t * 1000.0).round().max(0.0) as u64
}

fn nudges(d: &crate::director::Director, epoch_unix_ms: u64) -> NudgesView {
    let vps = crate::director::viewpoints();
    let tally = d.tally();
    NudgesView {
        targets: d
            .offer
            .iter()
            .map(|i| NudgeTarget {
                id: vps[*i].name.to_string(),
                label: vps[*i].label.to_string(),
                votes: tally.get(vps[*i].name).copied().unwrap_or(0),
            })
            .collect(),
        holding: (!d.flying).then(|| vps[d.current].name.to_string()),
        next_hold_at: (!d.flying).then(|| unix_ms(epoch_unix_ms, d.hold_until())),
        next: d.next.map(|n| vps[n].name.to_string()),
        arrive_next_at: d.arrive_next_at().map(|t| unix_ms(epoch_unix_ms, t)),
    }
}

fn borrow(state: &State) -> BorrowView {
    let lent: Vec<&Claim> = state
        .fleet
        .slots
        .values()
        .filter_map(|s| s.claim.as_ref())
        .filter(|c| c.borrowed.is_some())
        .collect();
    BorrowView {
        cap: state.config.borrow_cap(state.fleet.n),
        lent: lent.len(),
        next_return_at: lent.iter().map(|c| c.expires_at).min(),
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
    out.extend(f.quarantined.values().map(|q| FailureView::GpuQuarantined {
        host: q.host.clone(),
        sbdf: q.sbdf.clone(),
        error: q.error.clone(),
        since: q.since,
    }));
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
        // A quarantined GPU runs nothing: its slot leaves the assignment.
        if crate::apply::is_quarantined(&state.fleet, s) {
            continue;
        }
        if let Some(list) = out.get_mut(host) {
            list.push(AssignSlot {
                id: s.id.clone(),
                sbdf: gpu.sbdf.clone(),
                generation: s.generation,
                // A claimed GPU draws the whole image for its viewer, so it is not part of
                // its host's band (the aggregator draws the tile black).
                rect: if s.claim.is_some() { None } else { s.rect },
                private_camera: s.claim.as_ref().map(|c| c.camera.clone()),
                layout_generation: state.fleet.layout.generation,
                rect_from_frame: state.fleet.layout.from_frame,
                previous_rect: moved_from(state, s),
                standby: in_standby(state, s),
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
            layout_generation: state.fleet.layout.generation,
            rect_from_frame: state.fleet.layout.from_frame,
            previous_rect: moved_from(state, s),
            standby: false,
        })
        .collect()
}

pub fn shared_scene(state: &State) -> SharedScene {
    SharedScene::new(&state.fleet.scene, state.config.image_aspect)
}
