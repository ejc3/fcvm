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
    /// `waiting_capacity` (dark), `relaunching` (desired generation not yet reported),
    /// or the slot's reported phase.
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
}

#[derive(Debug, Clone, Serialize)]
pub struct RelaunchAllView {
    pub started_at: u64,
    pub by: String,
    pub fcvm_done_at: Option<u64>,
    pub ec2_done_at: Option<u64>,
    pub fcvm_last: Option<String>,
    pub ec2_last: Option<String>,
    pub fcvm_pending: usize,
    pub ec2_pending: usize,
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
    pub settings: Settings,
    pub relaunch_all: Option<RelaunchAllView>,
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
        },
        Some(s) => TileView {
            id,
            half,
            rect,
            phase: tile_phase(s),
            slot: Some(s.id.clone()),
            frame_url: s.observed.frame_url.clone(),
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
        .filter(|s| s.position.is_none())
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
        },
        scene: f.scene.clone(),
        image_aspect: c.image_aspect,
        settings: f.settings.clone(),
        relaunch_all: f.relaunch_all.as_ref().map(|r| RelaunchAllView {
            started_at: r.started_at,
            by: r.by.clone(),
            fcvm_done_at: r.fcvm_done_at,
            ec2_done_at: r.ec2_done_at,
            fcvm_last: r.fcvm_last.clone(),
            ec2_last: r.ec2_last.clone(),
            fcvm_pending: r.fcvm_pending.len(),
            ec2_pending: r.ec2_pending.len(),
        }),
        viewers: ViewersView {
            count: f.viewers.len(),
        },
        actions: f.actions.iter().cloned().collect(),
    }
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
                rect: s.rect,
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
        })
        .collect()
}

pub fn shared_scene(state: &State) -> SharedScene {
    SharedScene::new(&state.fleet.scene, state.config.image_aspect)
}
