//! The fleet state the coordinator holds. Everything here changes only through
//! [`crate::apply`].

use crate::director::Director;
use crate::scene::{Keyframe, Scene};
use crate::tiling::{Half, Rect};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

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
    /// The EC2 half's aggregator frame WebSocket, when the coordinator runs one.
    #[serde(default)]
    pub ec2_aggregate_url: Option<String>,
    /// The owner's race cadence: a relaunch of every tile this often. `None`: no schedule.
    pub race_interval_secs: Option<u64>,
    /// After any race, `restart_now` is refused for this long, for everyone.
    pub restart_cooldown_secs: u64,
    /// Seed of the directed camera's default order.
    pub camera_seed: u64,
    /// Lights one client address may have in the scene; its oldest goes first.
    pub lights_per_client: usize,
    /// A claimed GPU returns to the pool after this long.
    pub claim_secs: u64,
    /// After a claim ends, the same client address waits this long to claim again.
    pub claim_cooldown_secs: u64,
    /// Wall tiles that may be lent to visitors at once. `None`: 2 or a tenth of the tiles
    /// (rounded up), whichever is smaller.
    #[serde(default)]
    pub max_borrowed: Option<usize>,
}

impl Config {
    /// How many wall tiles may be lent at once with `n` tiles in each half.
    pub fn borrow_cap(&self, n: usize) -> usize {
        self.max_borrowed.unwrap_or_else(|| 2.min(n.div_ceil(10)))
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            max_tiles: 100,
            image_aspect: 16.0 / 9.0,
            tile_aspect: 16.0 / 9.0,
            ec2_max: 0,
            word_interval_ms: 5_000,
            light_interval_ms: 1_000,
            relaunch_interval_ms: 2_000,
            ec2_aggregate_url: None,
            race_interval_secs: None,
            restart_cooldown_secs: 60,
            camera_seed: 1,
            lights_per_client: 3,
            claim_secs: 60,
            claim_cooldown_secs: 120,
            max_borrowed: None,
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
    /// The rectangle this host's tiles cover (its band), when any of them is placed.
    #[serde(default)]
    pub region: Option<Rect>,
    /// The host aggregator's frame WebSocket, once the agent reports it.
    #[serde(default)]
    pub aggregate_url: Option<String>,
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
    /// When the agent will try a failed slot again.
    #[serde(default)]
    pub retry_at: Option<u64>,
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
    /// A viewer's private session on this (standby) GPU.
    #[serde(default)]
    pub claim: Option<Claim>,
}

/// A viewer's own GPU: a standby slot restored for them with their own scene.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Claim {
    pub session: String,
    pub viewer: String,
    pub client: String,
    /// Given only to the claiming viewer; never in the shared snapshot.
    pub token: String,
    pub claimed_at: u64,
    pub expires_at: u64,
    /// The generation whose first frame ends the click-to-frame time.
    pub generation: u64,
    pub first_frame_at: Option<u64>,
    /// The private camera path (the session's own, steerable by its viewer).
    pub camera: Vec<Keyframe>,
    /// The wall tile this GPU was drawing when it was lent (`fcvm:3`); `None` for a
    /// standby GPU. A borrowed tile keeps its place in the layout and is drawn black.
    #[serde(default)]
    pub borrowed: Option<String>,
}

/// One tile's part in a race.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RaceTile {
    /// The tile it held when the race started (`fcvm:3`).
    pub tile: String,
    /// The generation whose first frame finishes it.
    pub generation: u64,
    /// When the coordinator saw that first frame reported.
    pub first_frame_at: Option<u64>,
    pub phases: BTreeMap<String, u64>,
    /// Time spent waiting for EC2 spot capacity, kept out of the half's time.
    pub capacity_wait_ms: u64,
}

/// One half of a race: every tile placed in it when the race started.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct HalfRace {
    /// By slot id.
    pub tiles: BTreeMap<String, RaceTile>,
    /// Tiles that left during the race (host reclaimed), not counted.
    pub dropped: usize,
    pub done_at: Option<u64>,
    /// Click to the last tile's first frame, less that tile's capacity wait: the half's
    /// headline time.
    pub time_ms: Option<u64>,
    /// The slot whose first frame finished the half.
    pub last: Option<String>,
    /// The longest capacity wait of any of its tiles.
    pub capacity_wait_ms: u64,
    /// Tiles lent to a visitor when the race started: they sit it out.
    #[serde(default)]
    pub sitting_out: Vec<String>,
}

/// A relaunch of every tile, timed for both halves by the coordinator's clock.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Race {
    pub id: u64,
    pub started_at: u64,
    /// `schedule` or `viewer:<id>`.
    pub by: String,
    pub fcvm: HalfRace,
    pub ec2: HalfRace,
}

pub const MAX_RACES: usize = 50;

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
    /// The words already in the sky when this one was checked with them. A word whose sky
    /// changed before it was applied is refused, so no combination goes unchecked.
    #[serde(default)]
    pub sky: Vec<String>,
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
    /// The viewer that sent it (its `/live` connection id).
    pub viewer: String,
    /// The intent's client id.
    pub id: String,
    pub name: String,
    /// The intent's args, except a refused word's (`{}`).
    pub args: serde_json::Value,
    pub summary: String,
    pub outcome: Outcome,
}

pub const MAX_ACTIONS: usize = 50;
/// Intent ids remembered for idempotency. A retry arrives within seconds; this covers hours
/// of heavy use.
pub const MAX_RECEIPTS: usize = 20_000;

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
    /// The fcvm half's rectangles by position (the EC2 half's are the same, 0.5 to the
    /// right).
    #[serde(default)]
    pub layout: Vec<Rect>,
    pub scene: Scene,
    pub director: Director,
    /// Owners (client addresses) of `scene.lights`, index for index.
    pub light_owners: Vec<String>,
    /// The next scheduled race, and until when `restart_now` is refused.
    pub next_race_at: Option<u64>,
    pub cooldown_until: Option<u64>,
    /// The latest races, oldest first.
    pub races: VecDeque<Race>,
    pub race_count: u64,
    /// Connected viewers and the client address each acts from.
    pub viewers: BTreeMap<String, String>,
    pub actions: VecDeque<Action>,
    /// Per client address, the time of its last accepted word, light, relaunch, and the
    /// end of its last claim. Kept across reconnects.
    pub last_word_at: BTreeMap<String, u64>,
    pub last_light_at: BTreeMap<String, u64>,
    pub last_relaunch_at: BTreeMap<String, u64>,
    pub last_claim_end: BTreeMap<String, u64>,
    pub claim_count: u64,
    /// Hosts reclaimed (a spot notice, or gone): since when, and when they went away. Kept
    /// for a minute after they are gone so the page can say so.
    pub reclaimed: BTreeMap<String, (u64, Option<u64>)>,
    /// Coordinator components that are degraded: reason and since when.
    pub degraded: BTreeMap<String, (String, u64)>,
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
        let director = Director::new(config.camera_seed, 0.0);
        let mut scene = Scene::new(now_ms);
        scene.camera = director.camera();
        scene.time_of_day = crate::director::viewpoints()[director.current].time_of_day;
        State {
            version: 0,
            fleet: Fleet {
                hosts: BTreeMap::new(),
                slots: BTreeMap::new(),
                ec2: BTreeMap::new(),
                n: 0,
                layout: Vec::new(),
                scene,
                director,
                light_owners: Vec::new(),
                next_race_at: config.race_interval_secs.map(|s| now_ms + s * 1000),
                cooldown_until: None,
                races: VecDeque::new(),
                race_count: 0,
                viewers: BTreeMap::new(),
                actions: VecDeque::new(),
                last_word_at: BTreeMap::new(),
                last_light_at: BTreeMap::new(),
                last_relaunch_at: BTreeMap::new(),
                last_claim_end: BTreeMap::new(),
                claim_count: 0,
                reclaimed: BTreeMap::new(),
                degraded: BTreeMap::new(),
            },
            config,
            receipts: BTreeMap::new(),
            receipt_order: VecDeque::new(),
        }
    }

    /// The state a restarted coordinator resumes from: the scene, the director, the races,
    /// receipts, the action log, the per-address limits and the version carry over; hosts,
    /// their slots, claims and viewers do not, because no agent or browser connection
    /// survives the restart.
    pub fn resumed(mut self, config: Config, now_ms: u64) -> Self {
        let f = &mut self.fleet;
        f.hosts.clear();
        f.slots.clear();
        f.ec2.clear();
        f.n = 0;
        f.layout.clear();
        f.viewers.clear();
        f.degraded.clear();
        // A race interrupted by the restart cannot finish.
        f.races
            .retain(|r| r.fcvm.done_at.is_some() || r.fcvm.tiles.is_empty());
        f.next_race_at = config.race_interval_secs.map(|s| now_ms + s * 1000);
        self.config = config;
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
