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
    /// The coordinator can scale the GPU hosts (it has auto scaling groups to set): the
    /// fleet sleeps after `idle_scale_down_secs` with no active viewer and wakes on one.
    #[serde(default)]
    pub power_managed: bool,
    #[serde(default = "ten_minutes")]
    pub idle_scale_down_secs: u64,
}

fn ten_minutes() -> u64 {
    600
}

/// One GPU host's way back up while the fleet wakes, by its EC2 instance id.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct HostWake {
    /// The auto scaling group reported it pending, then in service.
    pub pending_at: Option<u64>,
    pub booted_at: Option<u64>,
    /// The host marked its store ready (reported in the agent's hello).
    #[serde(default)]
    pub store_ready_at: Option<u64>,
    /// Its agent said hello (the agent starts once the host's store is mounted).
    pub agent_at: Option<u64>,
    /// Every one of its slots reported running.
    pub rendering_at: Option<u64>,
}

/// One wake, measured from the first active viewer to the complete image.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Wake {
    pub started_at: u64,
    /// Every placed tile of both halves was rendering.
    pub awake_at: u64,
    /// The tiles placed then, both halves.
    pub gpus: usize,
    pub ms: u64,
}

pub const MAX_WAKES: usize = 20;

/// Whether the GPU capacity is up.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Power {
    /// `awake`, `asleep` (scaled to zero) or `waking`.
    pub state: String,
    /// Since when no viewer has been active.
    pub idle_since: Option<u64>,
    pub asleep_since: Option<u64>,
    pub woke_at: Option<u64>,
    pub hosts: BTreeMap<String, HostWake>,
    pub wakes: VecDeque<Wake>,
}

impl Default for Power {
    fn default() -> Self {
        Power {
            state: "awake".into(),
            idle_since: None,
            asleep_since: None,
            woke_at: None,
            hosts: BTreeMap::new(),
            wakes: VecDeque::new(),
        }
    }
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
            power_managed: false,
            idle_scale_down_secs: 600,
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
    /// From the renderer's health: its budget fit and newest frame's scene time.
    #[serde(default)]
    pub budget_fit: Option<f64>,
    #[serde(default)]
    pub frame_t: Option<f64>,
    /// The slowest render over the renderer's last 2 s (`frames_2s.render_ms_max`).
    #[serde(default)]
    pub render_ms_max: Option<f64>,
    /// When the agent will try a failed slot again.
    #[serde(default)]
    pub retry_at: Option<u64>,
    /// The slot's public addresses, as last reported.
    #[serde(default)]
    pub ipv6: Option<String>,
    #[serde(default)]
    pub ipv4: Option<String>,
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
    /// Place in its side's grid (0-based) and the rectangle there; `None` on standby, while
    /// its host drains, or (EC2) before it has an instance.
    pub position: Option<usize>,
    pub rect: Option<Rect>,
    /// When the last layout change moved it: its rectangle before, and the layout
    /// generation that moved it.
    #[serde(default)]
    pub previous_rect: Option<Rect>,
    #[serde(default)]
    pub rect_generation: u64,
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
    /// While EC2 refuses capacity for it: since when.
    #[serde(default)]
    pub waiting_since: Option<u64>,
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

/// A layout generation. Each change of the tiling (a GPU joins or leaves either side) makes
/// a new one that takes effect at `from_frame` (frame k is scene time k/30): renderers
/// switch to their new rectangle at that frame, so every tile of a frame has one layout.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct LayoutGen {
    pub generation: u64,
    pub from_frame: i64,
    pub fcvm: Option<Rect>,
    pub ec2: Option<Rect>,
}

/// Frames allowed for a new layout's scene posts (coordinator to agent to renderer, or the
/// coordinator to an EC2 tile) to reach every renderer.
pub const POST_DELIVERY_FRAMES: i64 = 30;
/// Frames a renderer needs between the post's arrival and the layout's frame, to open the
/// new rectangle's encoders off its render thread (the renderer's round 10: at least 0.5 s).
pub const ENCODER_LEAD_FRAMES: i64 = 15;
/// Frames a new layout waits before it takes effect.
pub const LAYOUT_LEAD_FRAMES: i64 = POST_DELIVERY_FRAMES + ENCODER_LEAD_FRAMES;

/// The frame a layout made at scene time `t` takes effect at: the first frame at or after
/// `t`, plus the delivery allowance and the renderer's encoder lead, so a post that arrives
/// within `POST_DELIVERY_FRAMES` leaves the renderer at least 0.5 s.
pub fn layout_from_frame(t: f64) -> i64 {
    (t * 30.0).ceil() as i64 + LAYOUT_LEAD_FRAMES
}

/// Totals since the fleet was first started (kept across coordinator restarts).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Counters {
    /// fcvm GPU VMs that reached their first frame (a restore, the wall's or a claim's).
    pub restores: u64,
    /// GPU scrubs the agents reported, and the ones that failed.
    pub scrubs: u64,
    pub scrub_failures: u64,
    /// Restores per minute (Unix minute, count) over the last hour.
    pub restores_by_minute: VecDeque<(u64, u64)>,
}

/// A GPU taken out of rotation after a failed reset, scrub or attach. It returns only by
/// an operator's `ReleaseQuarantine`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Quarantined {
    pub host: String,
    pub sbdf: String,
    pub error: String,
    pub since: u64,
}

/// The key of a GPU across hosts.
pub fn gpu_key(host: &str, sbdf: &str) -> String {
    format!("{host}/{sbdf}")
}

/// The part of the state compared after each event to decide whether the version moves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fleet {
    pub hosts: BTreeMap<String, Host>,
    /// fcvm slots by number (`slot-<n>`).
    pub slots: BTreeMap<u32, Slot>,
    /// EC2 tiles by number (`ec2-<n>`).
    pub ec2: BTreeMap<u32, Slot>,
    /// Tiles on the fcvm side (one per live fcvm GPU, at most `max_tiles`).
    pub n: usize,
    /// Tiles on the EC2 side (one per EC2 tile with an instance, at most `max_tiles`).
    #[serde(default)]
    pub ec2_n: usize,
    /// The current layout: its generation, the frame it takes effect at, the two sides.
    #[serde(default)]
    pub layout: LayoutGen,
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
    #[serde(default)]
    pub counters: Counters,
    /// Quarantined GPUs by `gpu_key`.
    #[serde(default)]
    pub quarantined: BTreeMap<String, Quarantined>,
    /// Viewers whose page reported it is hidden; they do not count as active.
    #[serde(default)]
    pub hidden: std::collections::BTreeSet<String>,
    /// fcvm slots whose VM is starting before they join the wall.
    #[serde(default)]
    pub warming: std::collections::BTreeSet<u32>,
    #[serde(default)]
    pub power: Power,
    /// The grid's render budget.
    #[serde(default)]
    pub budget: crate::budget::Budget,
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
                ec2_n: 0,
                layout: LayoutGen::default(),
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
                counters: Counters::default(),
                quarantined: BTreeMap::new(),
                hidden: Default::default(),
                warming: Default::default(),
                power: Power::default(),
                budget: Default::default(),
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
        f.layout = LayoutGen::default();
        f.ec2_n = 0;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_layout_leaves_every_renderer_half_a_second_after_its_post() {
        for t in [0.0, 0.004, 0.01, 0.034, 12.345, 861.999, 3600.5] {
            let k = layout_from_frame(t);
            let at = k as f64 / 30.0;
            // A post that left at `t` and arrived within the delivery allowance.
            let arrived = t + POST_DELIVERY_FRAMES as f64 / 30.0;
            assert!(
                at - arrived >= 0.5 - 1e-9,
                "t {t}: frame {k} is {} s after arrival",
                at - arrived
            );
        }
        assert_eq!(layout_from_frame(10.0), 345);
    }
}
