//! The fleet state the coordinator holds. Everything here changes only through
//! [`crate::apply`].

use crate::scene::Scene;
use crate::tiling::{Half, Rect};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Fixed for the life of a coordinator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// The most tiles per half the page is asked to play.
    pub max_tiles: usize,
    /// Width / height of the whole image (both halves).
    pub image_aspect: f64,
    /// Width / height the tiling aims for.
    pub tile_aspect: f64,
    /// The most EC2 tiles the coordinator asks for. 0 runs the EC2 half dark.
    pub ec2_max: usize,
    /// Minimum milliseconds between two words from one viewer.
    pub word_interval_ms: u64,
    /// Minimum milliseconds between two lights from one viewer.
    pub light_interval_ms: u64,
    /// Minimum milliseconds between two committed `relaunch` intents from one viewer.
    pub relaunch_interval_ms: u64,
    /// Minimum milliseconds between two committed `relaunch_all` intents from one viewer.
    pub relaunch_all_interval_ms: u64,
    /// Bounds on `set_auto_relaunch`.
    pub auto_relaunch_min_secs: u64,
    pub auto_relaunch_max_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            max_tiles: 100,
            image_aspect: 32.0 / 9.0,
            tile_aspect: 16.0 / 9.0,
            ec2_max: 0,
            word_interval_ms: 5_000,
            light_interval_ms: 1_000,
            relaunch_interval_ms: 2_000,
            relaunch_all_interval_ms: 30_000,
            auto_relaunch_min_secs: 10,
            auto_relaunch_max_secs: 86_400,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gpu {
    pub sbdf: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Host {
    pub name: String,
    pub arch: String,
    pub gpus: Vec<Gpu>,
    pub capacity: usize,
    pub draining: bool,
    pub joined_at: u64,
}

/// What an agent (or the EC2 driver) last said about one slot.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Observed {
    /// `pending` until the first report.
    pub phase: String,
    /// The generation the phase refers to.
    pub generation: u64,
    pub vm: Option<String>,
    pub frame_url: Option<String>,
    pub timings: BTreeMap<String, u64>,
    pub error: Option<String>,
    pub fps: Option<f64>,
    pub gpu_stats: Option<serde_json::Value>,
}

/// One relaunch of one slot, from the click to its first frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Relaunch {
    pub generation: u64,
    pub clicked_at: u64,
    /// `viewer:<id>`, `relaunch_all` or `auto`.
    pub by: String,
    pub first_frame_at: Option<u64>,
    pub phases: BTreeMap<String, u64>,
}

/// A rendering unit: an fcvm slot (one GPU on a host) or an EC2 tile (one instance).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Slot {
    pub id: String,
    pub half: Half,
    pub host: Option<String>,
    pub gpu: Option<Gpu>,
    /// Desired generation; a relaunch increments it.
    pub generation: u64,
    pub created_at: u64,
    /// Place in its half's grid (0-based) and the rectangle there; `None` on standby or
    /// while its host drains.
    pub position: Option<usize>,
    pub rect: Option<Rect>,
    pub observed: Observed,
    pub last_relaunch: Option<Relaunch>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelaunchAll {
    pub started_at: u64,
    pub by: String,
    pub fcvm_done_at: Option<u64>,
    pub ec2_done_at: Option<u64>,
    /// The slot whose first frame closed each half.
    pub fcvm_last: Option<String>,
    pub ec2_last: Option<String>,
    /// Slots still to reach their first frame, with the generation they must reach.
    pub fcvm_pending: BTreeMap<String, u64>,
    pub ec2_pending: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Committed,
    Refused,
}

/// The text check's answer for a `word` intent, carried into its receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextCheck {
    /// `allow`, `refuse` or `error`.
    pub verdict: String,
    pub reason: String,
}

impl TextCheck {
    pub fn allowed(&self) -> bool {
        self.verdict == "allow"
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Receipt {
    pub id: String,
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub version: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check: Option<TextCheck>,
}

/// One line of the recent-actions list every viewer sees.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Action {
    pub at: u64,
    pub by: String,
    pub name: String,
    pub summary: String,
    pub outcome: Outcome,
}

pub const MAX_ACTIONS: usize = 50;
/// Intent ids remembered for idempotency. A retry arrives within seconds; this covers hours
/// of heavy use.
pub const MAX_RECEIPTS: usize = 20_000;

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Settings {
    pub auto_relaunch_secs: Option<u64>,
}

/// The part of the state compared after each event to decide whether the version moves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fleet {
    pub hosts: BTreeMap<String, Host>,
    /// fcvm slots by number (`slot-<n>`).
    pub slots: BTreeMap<u32, Slot>,
    /// EC2 tiles by number (`ec2-<n>`).
    pub ec2: BTreeMap<u32, Slot>,
    /// Rectangles per half.
    pub n: usize,
    pub scene: Scene,
    pub settings: Settings,
    pub relaunch_all: Option<RelaunchAll>,
    pub viewers: BTreeSet<String>,
    pub actions: VecDeque<Action>,
    /// Per viewer, the time of its last accepted word and light.
    pub last_word_at: BTreeMap<String, u64>,
    pub last_light_at: BTreeMap<String, u64>,
    pub last_relaunch_at: BTreeMap<String, u64>,
    pub last_relaunch_all_at: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub version: u64,
    pub config: Config,
    pub fleet: Fleet,
    pub receipts: BTreeMap<String, Receipt>,
    pub receipt_order: VecDeque<String>,
}

impl State {
    pub fn new(config: Config, now_ms: u64) -> Self {
        State {
            version: 0,
            config,
            fleet: Fleet {
                hosts: BTreeMap::new(),
                slots: BTreeMap::new(),
                ec2: BTreeMap::new(),
                n: 0,
                scene: Scene::new(now_ms),
                settings: Settings::default(),
                relaunch_all: None,
                viewers: BTreeSet::new(),
                actions: VecDeque::new(),
                last_word_at: BTreeMap::new(),
                last_light_at: BTreeMap::new(),
                last_relaunch_at: BTreeMap::new(),
                last_relaunch_all_at: BTreeMap::new(),
            },
            receipts: BTreeMap::new(),
            receipt_order: VecDeque::new(),
        }
    }

    /// The state a restarted coordinator resumes from: settings, scene, receipts, the
    /// action log and the version carry over; hosts, their slots and viewers do not,
    /// because no agent or browser connection survives the restart.
    pub fn resumed(mut self, config: Config) -> Self {
        self.config = config;
        let f = &mut self.fleet;
        f.hosts.clear();
        f.slots.clear();
        f.ec2.clear();
        f.n = 0;
        f.relaunch_all = None;
        f.viewers.clear();
        f.last_word_at.clear();
        f.last_light_at.clear();
        f.last_relaunch_at.clear();
        f.last_relaunch_all_at.clear();
        self.version += 1;
        self
    }

    pub fn receipt(&self, id: &str) -> Option<&Receipt> {
        self.receipts.get(id)
    }

    /// Find a slot by its id (`slot-3`, `ec2-2`) or by the tile it holds (`fcvm:1`,
    /// `ec2:4`, 1-based).
    pub fn find_slot(&self, name: &str) -> Option<(Half, u32)> {
        if let Some(k) = name.strip_prefix("fcvm:") {
            let pos = k.parse::<usize>().ok()?.checked_sub(1)?;
            return self
                .fleet
                .slots
                .iter()
                .find(|(_, s)| s.position == Some(pos))
                .map(|(n, _)| (Half::Fcvm, *n));
        }
        if let Some(k) = name.strip_prefix("ec2:") {
            let pos = k.parse::<usize>().ok()?.checked_sub(1)?;
            return self
                .fleet
                .ec2
                .iter()
                .find(|(_, s)| s.position == Some(pos))
                .map(|(n, _)| (Half::Ec2, *n));
        }
        if let Some(n) = name.strip_prefix("slot-").and_then(|n| n.parse().ok()) {
            return self.fleet.slots.contains_key(&n).then_some((Half::Fcvm, n));
        }
        if let Some(n) = name.strip_prefix("ec2-").and_then(|n| n.parse().ok()) {
            return self.fleet.ec2.contains_key(&n).then_some((Half::Ec2, n));
        }
        None
    }
}

pub fn slot_name(n: u32) -> String {
    format!("slot-{n}")
}

pub fn ec2_name(n: u32) -> String {
    format!("ec2-{n}")
}

/// Parse `slot-<n>` back to its number.
pub fn slot_number(id: &str) -> Option<u32> {
    id.strip_prefix("slot-")?.parse().ok()
}

pub fn ec2_number(id: &str) -> Option<u32> {
    id.strip_prefix("ec2-")?.parse().ok()
}
