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
    /// Potato mode's game (docs/fleet/ARCHITECTURE.md, "Potato mode").
    #[serde(default)]
    pub potato: crate::potato::PotatoConfig,
    /// Width / height of the whole image (both halves).
    pub image_aspect: f64,
    /// Width / height the tiling aims for.
    pub tile_aspect: f64,
    /// The most EC2 tiles the coordinator asks for. 0 runs the EC2 half dark.
    pub ec2_max: usize,
    /// Lambda tiles (`--lambda-tiles`): how many of the AWS half's `ec2_max` tiles are Lambda
    /// Managed Instances execution environments (one function per tile) instead of EC2
    /// instances. The half keeps `ec2_max` tiles; at most all of them are Lambda tiles. 0: every
    /// AWS tile is an EC2 instance.
    #[serde(default)]
    pub lambda_max: usize,
    /// Places on the fcvm side: with fewer live fcvm GPUs the rest are dark tiles, with
    /// more the side has one place per GPU. 0: one place per live fcvm GPU. GPUs that
    /// connected hosts run phones on are not wall places and come off this count, and so do
    /// quarantined wall GPUs of connected hosts.
    #[serde(default)]
    pub fcvm_tiles: usize,
    /// The fleet keeps more than one GPU host awake per region (`--awake-hosts`): while any
    /// fcvm GPU renders on the wall, the side has one place per rendering GPU, so a host that
    /// drains, is gone or has not joined yet leaves no dark places; the wall re-tiles coarser
    /// across the GPUs that render and finer again as the others join.
    #[serde(default)]
    pub fill_from_serving_hosts: bool,
    /// Minimum milliseconds between two words from one viewer.
    pub word_interval_ms: u64,
    /// Seconds with a viewer on the wall, after the last word has left the sky, before the
    /// wall writes a line of GPU history itself (a system word). 0: never.
    #[serde(default)]
    pub history_idle_secs: u64,
    /// Minimum milliseconds between two lights from one viewer.
    pub light_interval_ms: u64,
    /// Minimum milliseconds between two committed `relaunch` intents from one viewer.
    pub relaunch_interval_ms: u64,
    /// The EC2 half's aggregator frame WebSocket, when the coordinator runs one.
    #[serde(default)]
    pub ec2_aggregate_url: Option<String>,
    /// The EC2 half's aggregator's public addresses when it serves browsers with TLS (on
    /// 443 in both families): the coordinator task's own. Named `agg-ec2-<token>`.
    #[serde(default)]
    pub ec2_aggregate_ipv6: Option<String>,
    #[serde(default)]
    pub ec2_aggregate_ipv4: Option<String>,
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
    /// The coordinator launches and terminates the GPU hosts (or was told to sleep): the
    /// fleet sleeps after `idle_scale_down_secs` with no active viewer and wakes on one.
    #[serde(default)]
    pub power_managed: bool,
    #[serde(default = "ten_minutes")]
    pub idle_scale_down_secs: u64,
    /// With power management, the fleet also sleeps when no viewer has arrived for this
    /// long (`Power::last_arrival_at`), even while viewers stay connected. 0: never.
    #[serde(default = "one_hour")]
    pub sleep_without_new_viewer_secs: u64,
    /// The most the fleet may spend in one UTC day, in USD: at it the fleet sleeps until
    /// 00:00 UTC (`crate::spend`). `None`: no ceiling.
    #[serde(default)]
    pub daily_spend_usd: Option<f64>,
    /// With power management, weekly UTC windows in which the fleet stays awake with no
    /// viewers (`crate::warm`). Empty: none.
    #[serde(default)]
    pub warm_hours: Vec<crate::warm::Window>,
    /// The coordinator writes each launch's DNS names and a tile's frame URLs are its
    /// published names (`Slot::published`), not the URL its agent reported.
    #[serde(default)]
    pub names_published: bool,
    /// Phones mode: a borrowed phone returns after this long, and the same client address
    /// waits `phone_claim_cooldown_secs` after it to borrow again.
    #[serde(default = "two_minutes")]
    pub phone_claim_secs: u64,
    #[serde(default = "one_minute")]
    pub phone_claim_cooldown_secs: u64,
    /// Phones that may be lent at once. `None`: 2 or half the phones (rounded up), whichever
    /// is smaller.
    #[serde(default)]
    pub max_phones_lent: Option<usize>,
    /// Width / height of one phone's scene (its screen less what the app draws around it):
    /// the phones' image is laid out in these.
    #[serde(default = "phone_aspect")]
    pub phone_aspect: f64,
    /// Windows mode: a borrowed sandbox returns after this long, and the same client address
    /// waits `windows_claim_cooldown_secs` after it to borrow again.
    #[serde(default = "two_minutes")]
    pub windows_claim_secs: u64,
    #[serde(default = "one_minute")]
    pub windows_claim_cooldown_secs: u64,
    /// Sandboxes that may be lent at once. `None`: 2 or half the sandboxes (rounded up),
    /// whichever is smaller.
    #[serde(default)]
    pub max_windows_lent: Option<usize>,
    /// Minecraft mode: a borrowed server returns after this long, and the same client address
    /// waits `minecraft_claim_cooldown_secs` after it to borrow again.
    #[serde(default = "two_minutes")]
    pub minecraft_claim_secs: u64,
    #[serde(default = "one_minute")]
    pub minecraft_claim_cooldown_secs: u64,
    /// Servers that may be lent at once. `None`: 2 or half the servers (rounded up),
    /// whichever is smaller.
    #[serde(default)]
    pub max_minecraft_lent: Option<usize>,
}

fn ten_minutes() -> u64 {
    600
}
fn one_hour() -> u64 {
    3600
}
fn two_minutes() -> u64 {
    120
}
fn one_minute() -> u64 {
    60
}
/// Robot Park's surface, 720 x 1280: the app places its part of the image by the surface's
/// height (PROTOCOL.md, "Phone app").
pub fn phone_aspect() -> f64 {
    720.0 / 1280.0
}

/// One GPU host's way back up while the fleet wakes, by its EC2 instance id.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct HostWake {
    /// EC2 reported it pending, then running.
    pub pending_at: Option<u64>,
    pub booted_at: Option<u64>,
    /// The host marked its store ready (reported in the agent's hello).
    #[serde(default)]
    pub store_ready_at: Option<u64>,
    /// Its agent said hello (the agent starts once the host's store is mounted).
    pub agent_at: Option<u64>,
    /// Every one of its slots reported running.
    pub rendering_at: Option<u64>,
    /// The host's own boot times, from the agent's hello (`AgentMsg::Hello::boot`): Unix
    /// milliseconds on the host's clock, each kept only when it falls inside this wake.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub boot: BTreeMap<String, u64>,
}

/// One wake, measured from the first active viewer (or a warm window's start) to the complete
/// image.
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
    /// The last arrival: a new viewer connection (not a page reconnecting) or a connected
    /// page's `wake`. A state that records none counts from the coordinator's start.
    #[serde(default)]
    pub last_arrival_at: Option<u64>,
    /// Why the power state is not the viewer rules' alone: `budget` while the day's spend
    /// ceiling holds it asleep (`crate::spend`), `warm` while a warm window holds it awake
    /// (`crate::warm`). Each is set again from the clock at every event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
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
            last_arrival_at: None,
            reason: None,
        }
    }
}

impl Config {
    /// How many wall tiles may be lent at once with `n` tiles in each half.
    pub fn borrow_cap(&self, n: usize) -> usize {
        self.max_borrowed.unwrap_or_else(|| 2.min(n.div_ceil(10)))
    }

    /// How many of `n` phones may be lent at once.
    pub fn phone_lend_cap(&self, n: usize) -> usize {
        self.max_phones_lent.unwrap_or_else(|| 2.min(n.div_ceil(2)))
    }

    /// How many of `n` Windows sandboxes may be lent at once.
    pub fn windows_lend_cap(&self, n: usize) -> usize {
        self.max_windows_lent
            .unwrap_or_else(|| 2.min(n.div_ceil(2)))
    }

    /// How many of `n` Minecraft servers may be lent at once.
    pub fn minecraft_lend_cap(&self, n: usize) -> usize {
        self.max_minecraft_lent
            .unwrap_or_else(|| 2.min(n.div_ceil(2)))
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            potato: Default::default(),
            max_tiles: 100,
            image_aspect: 16.0 / 9.0,
            tile_aspect: 16.0 / 9.0,
            ec2_max: 0,
            lambda_max: 0,
            fcvm_tiles: 0,
            fill_from_serving_hosts: false,
            word_interval_ms: 5_000,
            history_idle_secs: 0,
            light_interval_ms: 1_000,
            relaunch_interval_ms: 2_000,
            ec2_aggregate_url: None,
            ec2_aggregate_ipv6: None,
            ec2_aggregate_ipv4: None,
            race_interval_secs: None,
            restart_cooldown_secs: 60,
            camera_seed: 1,
            lights_per_client: 3,
            claim_secs: 60,
            claim_cooldown_secs: 120,
            max_borrowed: None,
            power_managed: false,
            idle_scale_down_secs: 600,
            sleep_without_new_viewer_secs: 3600,
            daily_spend_usd: None,
            warm_hours: Vec::new(),
            names_published: false,
            phone_claim_secs: 120,
            phone_claim_cooldown_secs: 60,
            max_phones_lent: None,
            phone_aspect: phone_aspect(),
            windows_claim_secs: 120,
            windows_claim_cooldown_secs: 60,
            max_windows_lent: None,
            minecraft_claim_secs: 120,
            minecraft_claim_cooldown_secs: 60,
            max_minecraft_lent: None,
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
    /// The aggregator's public addresses, as last reported (`AgentMsg::Aggregate`), and the
    /// port of its IPv4 path.
    #[serde(default)]
    pub aggregate_ipv6: Option<String>,
    #[serde(default)]
    pub aggregate_ipv4: Option<String>,
    #[serde(default)]
    pub aggregate_v4_port: Option<u16>,
    /// The aggregator's published DNS names (with `Config::names_published`), under the
    /// label `agg-<host>-<token>` (see [`aggregate_id`]).
    #[serde(default)]
    pub aggregate_published: Option<Published>,
    /// The take-over actions its phones carry out, as its hello listed them
    /// (`PhoneAction::offered`).
    #[serde(default)]
    pub phone_actions: Vec<String>,
    /// The take-over actions its Windows sandboxes carry out, as its hello listed them
    /// (`WindowsAction::offered`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows_actions: Vec<String>,
    /// Its Windows image, as its agent last reported it (`AgentMsg::WindowsImage`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows_image: Option<crate::windows::WindowsImage>,
    /// Its Minecraft servers' name and version, as its hello gave it (`minecraft_server`);
    /// none for a host that runs no server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minecraft_server: Option<String>,
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
    /// The VM's CPU and memory, stamped with the coordinator's receipt time.
    #[serde(default)]
    pub vm_stats: Option<crate::protocol::VmStats>,
    /// From the renderer's health: its budget fit and newest frame's scene time.
    #[serde(default)]
    pub budget_fit: Option<f64>,
    #[serde(default)]
    pub frame_t: Option<f64>,
    /// The slowest render over the renderer's last 2 s (`frames_2s.render_ms_max`).
    #[serde(default)]
    pub render_ms_max: Option<f64>,
    /// The budget at which the renderer draws every pixel at its drawn size, and the
    /// renderer process's `boot_id`.
    #[serde(default)]
    pub budget_every_pixel: Option<f64>,
    #[serde(default)]
    pub boot_id: Option<String>,
    /// When the agent will try a failed slot again.
    #[serde(default)]
    pub retry_at: Option<u64>,
    /// The slot's public addresses, as last reported, and the port its TLS is reached on at
    /// the IPv4 address (443 when absent).
    #[serde(default)]
    pub ipv6: Option<String>,
    #[serde(default)]
    pub ipv4: Option<String>,
    #[serde(default)]
    pub v4_port: Option<u16>,
    /// Where the slot runs, as last reported (`protocol::place_label`).
    #[serde(default)]
    pub place: Option<String>,
}

/// One launch's DNS names in the coordinator's subzone, answered from the moment they are
/// published: `<label>.<subzone>` (AAAA) and `<label>-v4.<subzone>` (A). A label is made for
/// one launch (or one aggregator's addresses) and never used again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Published {
    /// The launch (the slot's observed generation) the names belong to.
    pub generation: u64,
    pub label: String,
    #[serde(default)]
    pub ipv6: Option<String>,
    #[serde(default)]
    pub ipv4: Option<String>,
    /// The port of the IPv4 path (443 when absent), as the slot reported it.
    #[serde(default)]
    pub v4_port: Option<u16>,
    /// `wss://<label>.<zone>/frames`, with an IPv6 address.
    #[serde(default)]
    pub frame_url: Option<String>,
    /// `wss://<label>-v4.<zone>[:<port>]/frames`, with an IPv4 address.
    #[serde(default)]
    pub frame_url_v4: Option<String>,
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

/// A rendering unit: an fcvm slot (one GPU on a host), an EC2 tile (one instance) or a
/// Lambda tile (one execution environment).
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
    /// The newest launch whose DNS names were published (with `Config::names_published`).
    #[serde(default)]
    pub published: Option<Published>,
    pub last_relaunch: Option<Relaunch>,
    /// A viewer's private session on this (standby) GPU.
    #[serde(default)]
    pub claim: Option<Claim>,
    /// A phone's own state (`Fleet::phones` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone: Option<crate::phone::PhoneSlot>,
    /// A Windows sandbox's own state (`Fleet::windows` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows: Option<crate::windows::WindowsSlot>,
    /// A Minecraft server's own state (`Fleet::minecraft` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minecraft: Option<crate::minecraft::MinecraftSlot>,
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
    /// The camera flies the autopilot's loop (`crate::autopilot`) until the viewer pauses
    /// it or takes the camera; resuming flies back to the loop.
    #[serde(default)]
    pub autopilot: bool,
    /// Scene time at which the loop's phase is 0, once the flight has started (the first
    /// frame showed with the autopilot on); `None` while the camera is the viewer's.
    #[serde(default)]
    pub flight_t0: Option<f64>,
    /// The wall tile this GPU was drawing when it was lent (`fcvm:3`); `None` for a
    /// standby GPU. A borrowed tile keeps its place in the layout and is drawn black.
    #[serde(default)]
    pub borrowed: Option<String>,
    /// Set when a coordinator restart carried the claim over: its viewer's connection ended
    /// with the old process, so `viewer` is empty until a page presents the token
    /// (`resume_claim`), and the claim ends at this time if none has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_by: Option<u64>,
}

/// How long after a restart a claim waits for its viewer's page to reconnect and present its
/// token. A page reconnects within 0.5 to 10 s (`fleet/web/core/net.ts`).
pub const CLAIM_RESUME_MS: u64 = 30_000;

/// A GPU claim a restart carried over whose host's agent has not said hello again yet: it goes
/// back on the slot of that GPU when the agent does (`apply::hello`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeldClaim {
    pub host: String,
    pub sbdf: String,
    /// The number of the slot (`slot-<n>`) the claim was on. The GPU gets that number back
    /// when its host says hello, and no other GPU takes it meanwhile: the agent keys its VMs by
    /// slot id, and a GPU under a new id would have the claim's VM torn down.
    #[serde(default)]
    pub slot: Option<u32>,
    pub claim: Claim,
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
    /// Time spent waiting for EC2 spot capacity (or Lambda capacity, as the coordinator names
    /// it), kept out of the half's time.
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

impl HalfRace {
    /// A half with nothing in it: a race of a fleet that has no tile on that side.
    pub fn is_unused(&self) -> bool {
        *self == HalfRace::default()
    }
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
    /// The Lambda tiles' part, timed apart from the EC2 tiles' so each backing has its own
    /// race number; absent (unused) when the fleet has no Lambda tile.
    #[serde(default, skip_serializing_if = "HalfRace::is_unused")]
    pub lambda: HalfRace,
}

impl Race {
    /// Its three halves, fcvm, EC2 and Lambda.
    pub fn halves_mut(&mut self) -> [&mut HalfRace; 3] {
        [&mut self.fcvm, &mut self.ec2, &mut self.lambda]
    }

    pub fn half_mut(&mut self, half: Half) -> &mut HalfRace {
        match half {
            Half::Fcvm => &mut self.fcvm,
            Half::Ec2 => &mut self.ec2,
            Half::Lambda => &mut self.lambda,
        }
    }
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
    /// The intent's args, except a refused word's and any that are not an object (`{}`).
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
    /// The agent's report that quarantined it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<String>,
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
    /// Lambda tiles by number (`lam-<n>`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub lambda: BTreeMap<u32, Slot>,
    /// Live fcvm GPUs on the wall (at most `max_tiles`), at places `0..n`.
    pub n: usize,
    /// The fcvm side's places with no GPU (dark tiles), at places `n..`, in order.
    #[serde(default)]
    pub fcvm_dark: Vec<Rect>,
    /// Places of EC2 tiles on the AWS half: one per EC2 tile, with an instance or not.
    #[serde(default)]
    pub ec2_n: usize,
    /// Places of Lambda tiles on the AWS half, after its `ec2_n` EC2 tiles: one per Lambda
    /// tile, with an environment or not.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub lambda_n: usize,
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
    /// When each launch a viewer asked for was committed (a `relaunch` of an EC2 or a Lambda
    /// tile, and one per EC2 and Lambda tile of a `restart_now` race), oldest first, for the
    /// last `apply::EC2_LAUNCH_WINDOW_MS`. EC2 and Lambda launches share the one budget.
    #[serde(default, skip_serializing_if = "VecDeque::is_empty")]
    pub ec2_viewer_launches: VecDeque<u64>,
    pub last_claim_end: BTreeMap<String, u64>,
    /// System words placed so far: which line comes next, and in which style.
    #[serde(default)]
    pub history_count: u64,
    /// The time from which the wall may write its next system word: a word's hold and the
    /// idle gap after it, or the idle gap after a viewer arrives at an empty wall.
    #[serde(default)]
    pub history_ready_at: Option<u64>,
    pub claim_count: u64,
    /// Hosts reclaimed (a spot notice, or gone): since when, and when they went away. Kept
    /// for a minute after they are gone so the page can say so.
    pub reclaimed: BTreeMap<String, (u64, Option<u64>)>,
    #[serde(default)]
    pub counters: Counters,
    /// Quarantined GPUs by `gpu_key`.
    #[serde(default)]
    pub quarantined: BTreeMap<String, Quarantined>,
    /// Per `gpu_key`, the report of the quarantine an operator released last: an agent
    /// that replays that report (it was not told the release was stored) must not
    /// quarantine the GPU again.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub released_reports: BTreeMap<String, String>,
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
    /// Today's spend (`crate::spend`).
    #[serde(default)]
    pub spend: crate::spend::Spend,
    /// Coordinator components that are degraded: reason and since when.
    pub degraded: BTreeMap<String, (String, u64)>,
    /// The EC2 half's aggregator's published DNS names (`EC2_AGGREGATE_ID`).
    #[serde(default)]
    pub ec2_aggregate_published: Option<Published>,
    /// Phones mode: the phones (`phone-<n>`), apart from the wall's slots.
    #[serde(default)]
    pub phones: BTreeMap<u32, Slot>,
    #[serde(default)]
    pub phone_claim_count: u64,
    #[serde(default)]
    pub last_phone_claim_end: BTreeMap<String, u64>,
    #[serde(default)]
    pub phone_action_seq: u64,
    #[serde(default)]
    pub phones_zoom: crate::phone::PhonesZoom,
    #[serde(default)]
    pub last_phones_zoom_at: BTreeMap<String, u64>,
    /// Windows mode: the sandboxes (`win-<n>`), apart from the wall's slots and the phones.
    #[serde(default)]
    pub windows: BTreeMap<u32, Slot>,
    #[serde(default)]
    pub windows_claim_count: u64,
    #[serde(default)]
    pub last_windows_claim_end: BTreeMap<String, u64>,
    #[serde(default)]
    pub windows_action_seq: u64,
    /// Per client: its last reset of one sandbox, and of every sandbox.
    #[serde(default)]
    pub last_windows_reset_at: BTreeMap<String, u64>,
    #[serde(default)]
    pub last_windows_reset_all_at: BTreeMap<String, u64>,
    #[serde(default)]
    pub last_reset_all: Option<crate::windows::ResetAll>,
    /// Sandboxes that were lent when their host left or the coordinator restarted, by
    /// `windows::key` (host and index): the host may still run the borrower's desktop, so the
    /// sandbox is reset, and stays owed a reset until its host reports the fresh launch running.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub windows_to_reset: BTreeMap<String, crate::windows::ResetOwed>,
    /// Minecraft mode: the servers (`mc-<n>`), apart from the wall's slots, the phones and the
    /// Windows sandboxes.
    #[serde(default)]
    pub minecraft: BTreeMap<u32, Slot>,
    #[serde(default)]
    pub minecraft_claim_count: u64,
    #[serde(default)]
    pub last_minecraft_claim_end: BTreeMap<String, u64>,
    /// Per client: its last reset of one server, of every server, and its last fork.
    #[serde(default)]
    pub last_minecraft_reset_at: BTreeMap<String, u64>,
    #[serde(default)]
    pub last_minecraft_reset_all_at: BTreeMap<String, u64>,
    #[serde(default)]
    pub last_minecraft_fork_at: BTreeMap<String, u64>,
    #[serde(default)]
    pub last_minecraft_reset_all: Option<crate::minecraft::ResetAll>,
    #[serde(default)]
    pub last_minecraft_fork: Option<crate::minecraft::LastFork>,
    /// Servers that were lent when their host left or the coordinator restarted, by
    /// `minecraft::key`, as `windows_to_reset`: the host may still run the borrower's world.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub minecraft_to_reset: BTreeMap<String, crate::minecraft::ResetOwed>,
    /// Potato mode's game.
    #[serde(default)]
    pub potato: crate::potato::Potato,
    /// The GPU hosts the launcher keeps, by instance id (`Event::HostsExpected`); `None`
    /// when it cannot say, or the coordinator launches none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_hosts: Option<std::collections::BTreeSet<String>>,
    /// Every host whose agent registered, since this coordinator started or resumed.
    #[serde(default)]
    pub registrations: Registrations,
    /// GPU claims a restart carried over, waiting for their host's agent (see [`HeldClaim`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub held_claims: Vec<HeldClaim>,
}

/// The hosts whose agent said hello to this coordinator, by name (the EC2 instance id), since
/// `since`: when this coordinator started or resumed, the earliest time it vouches for. The
/// release gate's no-agent verdict reads it (fleet/ami/release_gate.py): a host absent from it
/// never registered since `since`. A host whose agent is gone stays in it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registrations {
    pub since: u64,
    pub hosts: BTreeMap<String, Registration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    pub first_at: u64,
    pub last_at: u64,
}

impl Registrations {
    /// Hosts kept; past this the one registered longest ago goes, and `since` moves past its
    /// latest hello. A staging release has a handful of hosts, and an EC2 id is never reused.
    pub const MAX: usize = 256;

    pub fn registered(&mut self, host: &str, at: u64) {
        let r = self.hosts.entry(host.to_string()).or_insert(Registration {
            first_at: at,
            last_at: at,
        });
        r.last_at = r.last_at.max(at);
        while self.hosts.len() > Self::MAX {
            let (oldest, r) = self
                .hosts
                .iter()
                .min_by_key(|(name, r)| (r.last_at, (*name).clone()))
                .map(|(name, r)| (name.clone(), *r))
                .expect("over the bound means not empty");
            self.hosts.remove(&oldest);
            // The record no longer holds what that host did, so it vouches only for later: a
            // host absent from it must not have registered since `since`.
            self.since = self.since.max(r.last_at + 1);
        }
    }
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
        scene.time_of_day = director.time_of_day(0.0);
        scene.weather = director.weather();
        State {
            version: 0,
            fleet: Fleet {
                hosts: BTreeMap::new(),
                slots: BTreeMap::new(),
                ec2: BTreeMap::new(),
                lambda: BTreeMap::new(),
                n: 0,
                fcvm_dark: Vec::new(),
                ec2_n: 0,
                lambda_n: 0,
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
                ec2_viewer_launches: VecDeque::new(),
                last_claim_end: BTreeMap::new(),
                history_count: 0,
                history_ready_at: None,
                claim_count: 0,
                reclaimed: BTreeMap::new(),
                degraded: BTreeMap::new(),
                counters: Counters::default(),
                quarantined: BTreeMap::new(),
                released_reports: BTreeMap::new(),
                hidden: Default::default(),
                warming: Default::default(),
                power: Power {
                    last_arrival_at: Some(now_ms),
                    ..Power::default()
                },
                budget: Default::default(),
                spend: Default::default(),
                ec2_aggregate_published: None,
                phones: BTreeMap::new(),
                phone_claim_count: 0,
                last_phone_claim_end: BTreeMap::new(),
                phone_action_seq: 0,
                phones_zoom: Default::default(),
                last_phones_zoom_at: BTreeMap::new(),
                windows: BTreeMap::new(),
                windows_claim_count: 0,
                last_windows_claim_end: BTreeMap::new(),
                windows_action_seq: 0,
                last_windows_reset_at: BTreeMap::new(),
                last_windows_reset_all_at: BTreeMap::new(),
                last_reset_all: None,
                windows_to_reset: BTreeMap::new(),
                minecraft: BTreeMap::new(),
                minecraft_claim_count: 0,
                last_minecraft_claim_end: BTreeMap::new(),
                last_minecraft_reset_at: BTreeMap::new(),
                last_minecraft_reset_all_at: BTreeMap::new(),
                last_minecraft_fork_at: BTreeMap::new(),
                last_minecraft_reset_all: None,
                last_minecraft_fork: None,
                minecraft_to_reset: BTreeMap::new(),
                potato: Default::default(),
                expected_hosts: None,
                registrations: Registrations {
                    since: now_ms,
                    hosts: BTreeMap::new(),
                },
                held_claims: Vec::new(),
            },
            config,
            receipts: BTreeMap::new(),
            receipt_order: VecDeque::new(),
        }
    }

    /// The state a restarted coordinator resumes from. Everything a viewer was told is
    /// stored (the coordinator sends a receipt only once a stored state holds its change), so
    /// what follows is decided per item: what can be restored is restored, and what was bound
    /// to the old process, or to a connection that ended with it, is released.
    ///
    /// Kept: the scene, the director and the open stop vote (below), finished races,
    /// receipts, the action log, the per-address limits, GPU claims (below), quarantines and
    /// the version. Dropped: hosts and their slots, viewers and the rest listed below, because
    /// no agent or browser connection survives the restart; agents say hello again and report
    /// their slots, and pages reconnect.
    pub fn resumed(mut self, config: Config, now_ms: u64) -> Self {
        let f = &mut self.fleet;
        // the ladder and the records stay; the round and the players went with the agent
        f.potato.resumed();
        // Registrations it had not stored are lost, and agents reconnect only once it is back.
        f.registrations = Registrations {
            since: now_ms,
            hosts: BTreeMap::new(),
        };
        // Restored: a GPU claim. Its VM runs on its host under the agent, which outlives the
        // coordinator, and the claim carries a token only its viewer's page holds, so the page
        // that reconnects proves it is that viewer (`resume_claim`). The viewer id is cleared:
        // ids restart at v-1 with the process, and an old id would hand the claim to whichever
        // new page gets it. A claim whose time ran out while the coordinator was down ends as
        // the tick would have ended it.
        let mut held = std::mem::take(&mut f.held_claims);
        for (n, s) in f.slots.iter_mut() {
            let (Some(c), Some(host), Some(gpu)) = (s.claim.take(), &s.host, &s.gpu) else {
                continue;
            };
            if c.expires_at <= now_ms {
                f.last_claim_end.insert(c.client.clone(), c.expires_at);
                continue;
            }
            held.push(HeldClaim {
                host: host.clone(),
                sbdf: gpu.sbdf.clone(),
                slot: Some(*n),
                claim: c,
            });
        }
        held.retain(|h| {
            let keep = h.claim.expires_at > now_ms;
            if !keep {
                f.last_claim_end
                    .insert(h.claim.client.clone(), h.claim.expires_at);
            }
            keep
        });
        for h in &mut held {
            h.claim.viewer.clear();
            h.claim.resume_by = Some(now_ms + CLAIM_RESUME_MS);
        }
        f.held_claims = held;
        f.hosts.clear();
        f.slots.clear();
        // Released: a borrowed phone. Its borrower is known only by its connection id (no
        // token), so no page that reconnects can show it was the borrower; the borrow ends
        // as it would when that viewer left, and the phone is free when its host says hello.
        f.phones.clear();
        // Released, for the same reason: a lent sandbox. Its host may come back running the
        // borrower's desktop, so it is reset then, and the cooldown runs from now.
        crate::apply::forget_windows(f, |_| true, now_ms);
        // Released, for the same reason: a lent Minecraft server. Its host may come back
        // running the borrower's world.
        crate::apply::forget_minecraft(f, |_| true, now_ms);
        f.ec2.clear();
        f.lambda.clear();
        f.n = 0;
        f.fcvm_dark.clear();
        f.layout = LayoutGen::default();
        f.ec2_n = 0;
        f.lambda_n = 0;
        // Viewer ids restart with the process, so which were hidden means nothing now.
        f.viewers.clear();
        f.hidden.clear();
        f.degraded.clear();
        // The hour without a new viewer runs on across a restart: the pages that reconnect
        // to the new process are not arrivals.
        f.power.last_arrival_at.get_or_insert(now_ms);
        // The EC2 aggregator's names were for the previous coordinator's addresses.
        f.ec2_aggregate_published = None;
        // Released: a race the restart interrupted. Its times are the coordinator's sightings
        // of each tile's first frame, and those during the restart were not seen, so it can
        // never finish with a true time (and its slots come back at generation 1), on either
        // half. Its tiles keep running.
        let finished = |h: &HalfRace| h.done_at.is_some() || h.tiles.is_empty();
        f.races
            .retain(|r| finished(&r.fcvm) && finished(&r.ec2) && finished(&r.lambda));
        f.next_race_at = config.race_interval_secs.map(|s| now_ms + s * 1000);
        // Restored: the open stop vote, when the stored votes are for the round still open
        // (a vote is answered only once a stored state holds it). A stored vote whose offer is
        // not that round's describes a round already decided, and starts again.
        let now_t = f.scene.t_at(now_ms);
        f.director.resume_vote(now_t);
        self.config = config;
        self.version += 1;
        self
    }

    pub fn receipt(&self, id: &str) -> Option<&Receipt> {
        self.receipts.get(id)
    }

    /// Find a slot by its id (`slot-3`, `ec2-2`, `lam-1`) or by the tile it holds (`fcvm:1`,
    /// `ec2:4`, `lambda:2`, 1-based).
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
        if let Some(k) = name.strip_prefix("lambda:") {
            let pos = k.parse::<usize>().ok()?.checked_sub(1)?;
            return self
                .fleet
                .lambda
                .iter()
                .find(|(_, s)| s.position == Some(pos))
                .map(|(n, _)| (Half::Lambda, *n));
        }
        if let Some(n) = name.strip_prefix("slot-").and_then(|n| n.parse().ok()) {
            return self.fleet.slots.contains_key(&n).then_some((Half::Fcvm, n));
        }
        if let Some(n) = name.strip_prefix("ec2-").and_then(|n| n.parse().ok()) {
            return self.fleet.ec2.contains_key(&n).then_some((Half::Ec2, n));
        }
        if let Some(n) = lambda_number(name) {
            return self
                .fleet
                .lambda
                .contains_key(&n)
                .then_some((Half::Lambda, n));
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

pub fn lambda_name(n: u32) -> String {
    format!("lam-{n}")
}

/// The id a host's aggregator is named under in DNS, `agg-<host>`, when the host's name can
/// be part of a DNS label: lowercase letters, digits and inner hyphens, at most 40
/// characters (an EC2 instance id is 19), and not `ec2`, kept for the EC2 half's aggregator.
pub fn aggregate_id(host: &str) -> Option<String> {
    let ok = !host.is_empty()
        && host.len() <= 40
        && host != "ec2"
        && !host.starts_with('-')
        && !host.ends_with('-')
        && host
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    ok.then(|| format!("agg-{host}"))
}

/// The id the EC2 half's aggregator is named under in DNS.
pub const EC2_AGGREGATE_ID: &str = "agg-ec2";

/// An address in its canonical text (`2001:db8::2` for `2001:0db8::2`); text that is not an
/// address is kept as it is.
pub fn canonical_ip(a: &str) -> String {
    a.parse::<std::net::IpAddr>()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|_| a.to_string())
}

impl Published {
    /// Whether these names are for exactly these addresses and IPv4 port.
    pub fn names(&self, ipv6: Option<&str>, ipv4: Option<&str>, v4_port: Option<u16>) -> bool {
        self.ipv6 == ipv6.map(canonical_ip)
            && self.ipv4 == ipv4.map(canonical_ip)
            && self.v4_port == v4_port
    }
}

/// Parse `slot-<n>` back to its number.
pub fn slot_number(id: &str) -> Option<u32> {
    id.strip_prefix("slot-")?.parse().ok()
}

pub fn ec2_number(id: &str) -> Option<u32> {
    id.strip_prefix("ec2-")?.parse().ok()
}

pub fn lambda_number(id: &str) -> Option<u32> {
    id.strip_prefix("lam-")?.parse().ok()
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

pub fn phone_name(n: u32) -> String {
    format!("phone-{n}")
}

pub fn phone_number(id: &str) -> Option<u32> {
    id.strip_prefix("phone-")?.parse().ok()
}

pub fn windows_name(n: u32) -> String {
    format!("win-{n}")
}

pub fn windows_number(id: &str) -> Option<u32> {
    id.strip_prefix("win-")?.parse().ok()
}

pub fn minecraft_name(n: u32) -> String {
    format!("mc-{n}")
}

pub fn minecraft_number(id: &str) -> Option<u32> {
    id.strip_prefix("mc-")?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(host: &str, at: u64) -> crate::Event {
        serde_json::from_value(serde_json::json!({
            "at": at, "host": host, "type": "agent",
            "msg": {"type": "hello", "host": host, "arch": "x86_64", "capacity": 1,
                    "gpus": [{"name": "Tesla T4", "sbdf": "0000:01:00.0"}]}
        }))
        .unwrap()
    }

    #[test]
    fn every_agent_registration_is_recorded_from_the_coordinators_start() {
        // The release gate's no-agent verdict (fleet/ami/release_gate.py) needs positive
        // evidence that a host never registered: the record of every host whose agent did, since
        // a time the coordinator can vouch for.
        let mut s = State::new(Config::default(), 1000);
        s = crate::apply(s, &hello("i-0123456789abcdef0", 2000));
        s = crate::apply(s, &hello("i-0123456789abcdef0", 3000));
        s = crate::apply(s, &hello("i-1234567890abcdef0", 2500));
        assert_eq!(s.fleet.registrations.since, 1000);
        assert_eq!(
            s.fleet.registrations.hosts.get("i-0123456789abcdef0"),
            Some(&Registration {
                first_at: 2000,
                last_at: 3000
            })
        );
        let view = serde_json::to_value(crate::view(&s)).unwrap();
        assert_eq!(
            view["fleet"]["agents"],
            serde_json::json!({"since": 1000, "hosts": [
                {"name": "i-0123456789abcdef0", "first_at": 2000, "last_at": 3000},
                {"name": "i-1234567890abcdef0", "first_at": 2500, "last_at": 2500}]})
        );
        // An agent that is gone stays in the record: it registered.
        let gone: crate::Event = serde_json::from_value(serde_json::json!(
            {"at": 4000, "host": "i-0123456789abcdef0", "type": "agent_gone"}))
        .unwrap();
        s = crate::apply(s, &gone);
        assert!(s
            .fleet
            .registrations
            .hosts
            .contains_key("i-0123456789abcdef0"));
    }

    #[test]
    fn a_resumed_coordinator_starts_its_registration_record_again() {
        // A restart may lose registrations it had not stored, and agents reconnect only after
        // it is back: what it vouches for starts at the resume.
        let mut s = State::new(Config::default(), 1000);
        s = crate::apply(s, &hello("i-0123456789abcdef0", 2000));
        let resumed = s.resumed(Config::default(), 5000);
        assert_eq!(
            resumed.fleet.registrations,
            Registrations {
                since: 5000,
                hosts: Default::default()
            }
        );
    }

    #[test]
    fn the_registration_record_keeps_the_latest_hosts_within_its_bound() {
        let mut s = State::new(Config::default(), 1000);
        for i in 0..(Registrations::MAX as u64 + 20) {
            s = crate::apply(s, &hello(&format!("i-{i:017x}"), 2000 + i));
        }
        assert_eq!(s.fleet.registrations.hosts.len(), Registrations::MAX);
        // The record no longer vouches for the time the evicted hosts registered in: `since` moves
        // past the latest of them, so an evicted host never reads as one that never registered
        // (Codex on #1925).
        assert_eq!(s.fleet.registrations.since, 2000 + 19 + 1);
        assert!(!s
            .fleet
            .registrations
            .hosts
            .contains_key(&format!("i-{:017x}", 0)));
        assert!(s
            .fleet
            .registrations
            .hosts
            .contains_key(&format!("i-{:017x}", Registrations::MAX as u64 + 19)));
    }

    #[test]
    fn a_host_aggregator_is_named_only_when_its_host_name_fits_a_label() {
        assert_eq!(
            aggregate_id("i-0123456789abcdef0").as_deref(),
            Some("agg-i-0123456789abcdef0")
        );
        assert_eq!(aggregate_id("gpu-a").as_deref(), Some("agg-gpu-a"));
        for bad in [
            "",
            "ec2",
            "-gpu",
            "gpu-",
            "GPU-A",
            "gpu.example.com",
            "gpu_a",
            &"a".repeat(41),
        ] {
            assert_eq!(aggregate_id(bad), None, "{bad:?}");
        }
    }

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

    /// A race the restart interrupted cannot finish (its slots come back at generation 1),
    /// so it is dropped whichever half was unfinished, the Lambda side's too; a finished race
    /// is kept.
    #[test]
    fn a_resumed_coordinator_drops_a_race_with_either_half_unfinished() {
        let tile = |tile: &str| RaceTile {
            tile: tile.into(),
            generation: 2,
            first_frame_at: None,
            phases: BTreeMap::new(),
            capacity_wait_ms: 0,
            waiting_since: None,
        };
        let half = |slot: &str, done: Option<u64>| HalfRace {
            tiles: BTreeMap::from([(slot.to_string(), tile(slot))]),
            done_at: done,
            ..Default::default()
        };
        let race = |id, fcvm, ec2, lambda| Race {
            id,
            started_at: 100,
            by: "schedule".into(),
            fcvm,
            ec2,
            lambda,
        };
        let mut s = State::new(Config::default(), 1000);
        s.fleet.races.extend([
            race(
                1,
                half("slot-1", Some(200)),
                half("ec2-1", Some(300)),
                half("lam-1", Some(400)),
            ),
            race(
                2,
                half("slot-1", Some(200)),
                half("ec2-1", None),
                HalfRace::default(),
            ),
            race(
                3,
                half("slot-1", None),
                half("ec2-1", Some(300)),
                HalfRace::default(),
            ),
            race(
                4,
                half("slot-1", Some(200)),
                half("ec2-1", Some(300)),
                half("lam-1", None),
            ),
        ]);
        let resumed = s.resumed(Config::default(), 2000);
        let kept: Vec<u64> = resumed.fleet.races.iter().map(|r| r.id).collect();
        assert_eq!(kept, vec![1]);
        assert!(!crate::apply::race_running(&resumed.fleet));
    }

    #[test]
    fn a_resumed_coordinator_forgets_which_viewers_were_hidden() {
        // Viewer ids restart at v-1 with the process, so a hidden id kept across a
        // restart would mark a new viewer that gets the same id as inactive.
        let mut s = State::new(Config::default(), 1000);
        s.fleet.viewers.insert("v-1".into(), "198.51.100.1".into());
        s.fleet.hidden.insert("v-1".into());
        let resumed = s.resumed(Config::default(), 2000);
        assert!(resumed.fleet.viewers.is_empty());
        assert!(
            resumed.fleet.hidden.is_empty(),
            "{:?}",
            resumed.fleet.hidden
        );
    }

    #[test]
    fn a_resumed_coordinator_keeps_the_last_arrival() {
        // The pages that reconnect after a restart are not arrivals, so a restart must not
        // start the hour again; a state from before arrivals were kept counts from the start.
        let mut s = State::new(Config::default(), 1000);
        s.fleet.power.last_arrival_at = Some(1500);
        let resumed = s.clone().resumed(Config::default(), 9000);
        assert_eq!(resumed.fleet.power.last_arrival_at, Some(1500));
        s.fleet.power.last_arrival_at = None;
        assert_eq!(
            s.resumed(Config::default(), 9000)
                .fleet
                .power
                .last_arrival_at,
            Some(9000)
        );
    }
}
