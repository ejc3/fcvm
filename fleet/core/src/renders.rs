//! Pages mode ("One VM per page", `?mode=pages`): what a renders host reports and what the
//! coordinator keeps and orders.
//!
//! A renders host renders a fixed set of web pages from a local replay on two sides at once,
//! each on its own equal set of pinned CPUs: `fcvm` (every render restores a new microVM
//! running Chromium and discards it) and `kitesurf` (Kitesurf engines on workerd). Each side
//! searches for the number of renders in flight its CPUs carry. Everything measured is
//! measured on the host and reported here as numbers; the screenshots go from the host to
//! the page and never through the coordinator.
//!
//! The coordinator's part is small: it shows the host's last report, and it holds the one
//! piece of state a viewer sets, the overlay (a short text and a transform put into every
//! page before it is rendered). The overlay is acknowledged state: it lives in the stored
//! fleet state, so a receipt is sent only once a stored state holds it, and a restarted
//! coordinator still has it.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The two sides, as the keys of [`RendersReport::sides`].
pub const SIDE_FCVM: &str = "fcvm";
pub const SIDE_KITESURF: &str = "kitesurf";

/// The transforms both sides draw alike, by name. The style block behind each name is the
/// host's (a viewer never sends CSS).
pub const TRANSFORMS_BOTH: [&str; 6] = ["mirror", "flip", "rotate", "scale", "zoom", "skew"];
/// The transforms only Chromium draws: the replay puts them into the pages the `fcvm` side
/// fetches and leaves the other side's pages as they are.
pub const TRANSFORMS_CHROMIUM_ONLY: [&str; 3] = ["invert", "tilt", "blur"];

/// The most characters an overlay's text holds, after whitespace is collapsed.
pub const MAX_OVERLAY_CHARS: usize = 40;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RendersConfig {
    /// How long an overlay stays once set or changed.
    pub overlay_ms: u64,
    /// The least time between two overlay changes from one client.
    pub change_interval_ms: u64,
    /// The least time between two overlay changes from anyone: every change starts both
    /// sides' searches again.
    pub global_interval_ms: u64,
}

impl Default for RendersConfig {
    fn default() -> Self {
        RendersConfig {
            overlay_ms: 120_000,
            change_interval_ms: 30_000,
            global_interval_ms: 10_000,
        }
    }
}

/// One page of the wall: its tile's id and the live site its name links to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RendersPage {
    pub id: String,
    pub url: String,
}

/// What a host that runs this mode says of itself in its hello.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RendersHost {
    /// The pages it renders, in the wall's order.
    pub pages: Vec<RendersPage>,
    /// The pinned CPUs of each side.
    pub cpus: u32,
    /// Whether each side runs in a cpuset of exactly `cpus` host CPUs.
    #[serde(default, skip_serializing_if = "is_false")]
    pub confined: bool,
    /// What runs, for the page's `Setup` lists: the host's instance type and kernel, the
    /// fcvm commit, the Kitesurf commit and seal, and the like, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub build: BTreeMap<String, String>,
}

/// Where a side's search is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SearchPhase {
    /// Nothing rendered yet at one in flight: no baseline.
    #[default]
    Starting,
    /// Raising renders in flight a step at a time.
    Adding,
    /// Holding a level after a back-off, until the next probe.
    Holding,
}

/// Why a side stepped back from a level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpikeCause {
    /// Render time rose past the threshold against the page's own time at one in flight.
    RenderTime,
    /// The side's CPUs were oversubscribed (the kernel's CPU pressure for its cgroup).
    CpuPressure,
    /// A render failed.
    Failed,
}

/// The last back-off of a side's search: the level it left and what was measured there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackOff {
    pub from: u32,
    pub cause: SpikeCause,
    /// Render time at `from` over the time at one in flight (the median over the pages of
    /// each page's own ratio), when the level completed enough renders to have one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ratio: Option<f64>,
    /// The share of the dwell in which some task of the side waited for a CPU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pressure: Option<f64>,
    /// When (Unix ms, the host's clock).
    pub at: u64,
}

/// One level the search has measured: renders a second and the median render time there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CurvePoint {
    pub in_flight: u32,
    pub renders_per_s: f64,
    /// The mean over the pages of each page's median render time at the level.
    pub render_ms: f64,
}

/// The state of a side's search, as the page shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Search {
    /// Renders in flight now.
    pub in_flight: u32,
    pub phase: SearchPhase,
    /// The last back-off; none before the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backed_off: Option<BackOff>,
    /// The highest level that held without a spike; none before a side has found one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// What each level measured the last time it completed a dwell, by level.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub curve: Vec<CurvePoint>,
}

/// One side's figures over the host's window, all measured from completed renders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SideReport {
    pub search: Search,
    pub renders_per_s: f64,
    /// The render time over the window: the mean over the pages of each page's median (the
    /// time to render one of each page, divided by the pages); none with no render in it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_ms: Option<f64>,
    /// CPUs busy of the side's set, from its cgroup.
    pub cpus_busy: f64,
    /// The side's CPU time over the window divided by its renders (the mean).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_ms_per_render: Option<f64>,
    /// Renders completed and failed since `since`.
    pub done: u64,
    pub failed: u64,
    pub since: u64,
    /// Engine processes running and engines recycled since `since` (the `kitesurf` side).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engines: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recycles: Option<u64>,
    /// The live values of the side's `Setup` list, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub setup: BTreeMap<String, String>,
}

/// One tile: one page on one side.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TileReport {
    pub page: String,
    pub side: String,
    /// `waiting` (no render yet), `rendering`, `stopped` (repeated failures) or
    /// `unsupported` (the side does not render the page).
    pub state: String,
    /// Why, for a tile that is not rendering: the host's words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub renders_per_s: f64,
    /// The last completed render's time and when it completed (Unix ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_at: Option<u64>,
    pub done: u64,
    pub failed: u64,
}

/// The harness's own health: the replays, drivers and frame server must never be what
/// limits a side. The page shows none of it; the post-deploy smoke fails on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Harness {
    /// The share of the window in which some harness task waited for a CPU.
    pub cpu_pressure: f64,
    /// Each side's replay's response time to a probe request, in ms.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub replay_ms: BTreeMap<String, f64>,
}

/// A renders host's summary, sent every few seconds. The page gets per-render events and
/// screenshots from the host itself; this is what a page that cannot reach the host shows,
/// and what the smoke reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RendersReport {
    /// When the host made it (Unix ms, the host's clock).
    pub at: u64,
    /// The host's frame socket for viewers, and the IPv4-only name for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_url_v4: Option<String>,
    /// By side: `fcvm`, `kitesurf`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sides: BTreeMap<String, SideReport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiles: Vec<TileReport>,
    /// The number of the overlay order the replays serve now ([`OrderOverlay::seq`]); none
    /// while they serve the pages as captured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<Harness>,
    /// Why the host renders nothing, when it does not (no snapshot yet, a kernel without the
    /// bridge teardown fix, a seal that does not match): the host's words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withheld: Option<String>,
}

/// The overlay a viewer set: a text, a transform, or both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overlay {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform: Option<String>,
    /// The viewer whose change was last (its `/live` connection id), and when.
    pub by: String,
    pub set_at: u64,
    /// When it goes (Unix ms).
    pub until: u64,
    /// Counts every change of the overlay, a clear and an expiry included, so a host can
    /// say which one its replays serve.
    pub seq: u64,
}

/// The overlay as a host is ordered to serve it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderOverlay {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform: Option<String>,
    /// The transform goes into the `fcvm` side's pages only.
    #[serde(default, skip_serializing_if = "is_false")]
    pub chromium_only: bool,
    pub seq: u64,
}

/// What the coordinator orders a renders host to do. Sent whenever it changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RendersOrder {
    /// Render (the fleet is awake) or stop every clone and engine.
    pub run: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay: Option<OrderOverlay>,
}

/// One host of the mode: what its hello said and its last report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct HostState {
    pub hello: RendersHost,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<RendersReport>,
    /// When the coordinator received `report` (its own clock).
    #[serde(default)]
    pub report_at: u64,
}

/// The mode's part of the fleet state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Renders {
    /// The hosts that run the mode, by host name. Not kept across a restart: agents say
    /// hello again.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hosts: BTreeMap<String, HostState>,
    /// The overlay viewers set. Kept across a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay: Option<Overlay>,
    /// The last overlay number given out.
    #[serde(default)]
    pub overlay_seq: u64,
    /// When each client last changed the overlay, and when anyone did.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub last_change_by: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_change_at: Option<u64>,
}

/// The overlay as the page shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OverlayView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transform: Option<String>,
    /// The transform is on the `fcvm` side's pages only: the two sides render different
    /// work while it is set, and the page compares nothing across them.
    pub chromium_only: bool,
    pub until: u64,
    pub seq: u64,
}

/// The names a viewer may pick a transform from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TransformsView {
    pub both: Vec<&'static str>,
    pub chromium_only: Vec<&'static str>,
}

/// `/live`'s `renders` section. Absent while no host runs the mode.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RendersView {
    pub host: String,
    pub pages: Vec<RendersPage>,
    pub cpus: u32,
    pub confined: bool,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub build: BTreeMap<String, String>,
    /// The host's last report and when the coordinator received it; none before the first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<RendersReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlay: Option<OverlayView>,
    pub transforms: TransformsView,
    pub max_overlay_chars: usize,
    /// The least seconds between two overlay changes from one viewer.
    pub change_interval_s: u64,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Whether `name` is a transform only Chromium draws; `None` when it is not a transform.
pub fn chromium_only(name: &str) -> Option<bool> {
    if TRANSFORMS_BOTH.contains(&name) {
        Some(false)
    } else if TRANSFORMS_CHROMIUM_ONLY.contains(&name) {
        Some(true)
    } else {
        None
    }
}

/// An overlay's text as it is kept: whitespace collapsed, no control characters, 1 to
/// [`MAX_OVERLAY_CHARS`] characters. Plain text: the host escapes it into the page.
pub fn normalise_overlay(text: &str) -> Result<String, String> {
    if text.chars().any(|c| c.is_control()) {
        return Err("text holds control characters".into());
    }
    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let n = t.chars().count();
    if n == 0 {
        return Err("text is empty".into());
    }
    if n > MAX_OVERLAY_CHARS {
        return Err(format!(
            "text is {n} characters; the limit is {MAX_OVERLAY_CHARS}"
        ));
    }
    Ok(t)
}

/// A change a viewer asks of the overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Text(String),
    Transform(String),
    Clear,
}

impl Renders {
    /// A host said hello. One that runs the mode is kept; one that does not is forgotten
    /// (an agent restarted without the mode).
    pub fn agent_hello(&mut self, host: &str, hello: Option<&RendersHost>) {
        match hello {
            Some(h) => {
                let state = self.hosts.entry(host.to_string()).or_default();
                state.hello = h.clone();
            }
            None => {
                self.hosts.remove(host);
            }
        }
    }

    pub fn agent_gone(&mut self, host: &str) {
        self.hosts.remove(host);
    }

    /// A host's report. One from a host that never said it runs the mode is dropped.
    pub fn report(&mut self, host: &str, report: &RendersReport, at: u64) {
        if let Some(h) = self.hosts.get_mut(host) {
            h.report = Some(report.clone());
            h.report_at = at;
        }
    }

    /// The overlay ends at its time.
    pub fn tick(&mut self, at: u64) {
        if self.overlay.as_ref().is_some_and(|o| at >= o.until) {
            self.overlay = None;
            self.overlay_seq += 1;
        }
    }

    /// Why `client` may not change the overlay now, if it may not.
    pub fn refusal(&self, client: &str, at: u64, cfg: &RendersConfig) -> Option<String> {
        if self.hosts.is_empty() {
            return Some("no host runs the pages now".into());
        }
        if let Some(&t) = self.last_change_by.get(client) {
            if at < t + cfg.change_interval_ms {
                return Some(format!(
                    "one change every {} s",
                    cfg.change_interval_ms.div_ceil(1000)
                ));
            }
        }
        if let Some(t) = self.last_change_at {
            if at < t + cfg.global_interval_ms {
                return Some(format!(
                    "the pages changed less than {} s ago; try again",
                    cfg.global_interval_ms.div_ceil(1000)
                ));
            }
        }
        None
    }

    /// Apply a viewer's change. The caller has checked a text with the text check.
    pub fn change(
        &mut self,
        viewer: &str,
        client: &str,
        change: &Change,
        at: u64,
        cfg: &RendersConfig,
    ) -> Result<String, String> {
        if let Some(reason) = self.refusal(client, at, cfg) {
            return Err(reason);
        }
        let (mut text, mut transform) = self
            .overlay
            .as_ref()
            .map(|o| (o.text.clone(), o.transform.clone()))
            .unwrap_or_default();
        let done = match change {
            Change::Text(t) => {
                text = Some(t.clone());
                "text"
            }
            Change::Transform(name) => {
                if chromium_only(name).is_none() {
                    return Err(format!("{name:?} is not a transform"));
                }
                transform = Some(name.clone());
                "transform"
            }
            Change::Clear => {
                if self.overlay.is_none() {
                    return Err("nothing is set".into());
                }
                text = None;
                transform = None;
                "clear"
            }
        };
        self.overlay_seq += 1;
        self.overlay = (text.is_some() || transform.is_some()).then(|| Overlay {
            text,
            transform,
            by: viewer.to_string(),
            set_at: at,
            until: at + cfg.overlay_ms,
            seq: self.overlay_seq,
        });
        self.last_change_by.insert(client.to_string(), at);
        self.last_change_at = Some(at);
        // The per-client times are only read inside the interval.
        let keep = cfg.change_interval_ms;
        self.last_change_by.retain(|_, t| at < *t + keep);
        Ok(done.into())
    }

    /// What a host of the mode is ordered to do.
    pub fn order(&self, awake: bool) -> RendersOrder {
        RendersOrder {
            run: awake,
            overlay: self.overlay.as_ref().map(|o| OrderOverlay {
                text: o.text.clone(),
                transform: o.transform.clone(),
                chromium_only: o
                    .transform
                    .as_deref()
                    .and_then(chromium_only)
                    .unwrap_or(false),
                seq: o.seq,
            }),
        }
    }

    /// A restarted coordinator: the hosts went with their connections; the overlay stays
    /// and ends at its time.
    pub fn resumed(&mut self) {
        self.hosts.clear();
    }

    /// The page's view: the first host by name (a stack runs one).
    pub fn view(&self, cfg: &RendersConfig) -> Option<RendersView> {
        let (host, h) = self.hosts.iter().next()?;
        Some(RendersView {
            host: host.clone(),
            pages: h.hello.pages.clone(),
            cpus: h.hello.cpus,
            confined: h.hello.confined,
            build: h.hello.build.clone(),
            report: h.report.clone(),
            report_at: h.report.as_ref().map(|_| h.report_at),
            overlay: self.overlay.as_ref().map(|o| OverlayView {
                text: o.text.clone(),
                transform: o.transform.clone(),
                chromium_only: o
                    .transform
                    .as_deref()
                    .and_then(chromium_only)
                    .unwrap_or(false),
                until: o.until,
                seq: o.seq,
            }),
            transforms: TransformsView {
                both: TRANSFORMS_BOTH.to_vec(),
                chromium_only: TRANSFORMS_CHROMIUM_ONLY.to_vec(),
            },
            max_overlay_chars: MAX_OVERLAY_CHARS,
            change_interval_s: cfg.change_interval_ms.div_ceil(1000),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> RendersHost {
        RendersHost {
            pages: vec![RendersPage {
                id: "example".into(),
                url: "https://example.com/".into(),
            }],
            cpus: 40,
            confined: true,
            build: BTreeMap::new(),
        }
    }

    fn with_host() -> Renders {
        let mut r = Renders::default();
        r.agent_hello("h1", Some(&host()));
        r
    }

    #[test]
    fn a_text_and_a_transform_are_one_overlay() {
        let cfg = RendersConfig::default();
        let mut r = with_host();
        assert_eq!(
            r.change("v1", "c1", &Change::Text("hello".into()), 1_000, &cfg),
            Ok("text".into())
        );
        assert_eq!(
            r.change(
                "v2",
                "c2",
                &Change::Transform("mirror".into()),
                20_000,
                &cfg
            ),
            Ok("transform".into())
        );
        let o = r.overlay.clone().unwrap();
        assert_eq!(o.text.as_deref(), Some("hello"));
        assert_eq!(o.transform.as_deref(), Some("mirror"));
        assert_eq!((o.by.as_str(), o.until, o.seq), ("v2", 140_000, 2));
        let order = r.order(true).overlay.unwrap();
        assert!(!order.chromium_only);
        assert_eq!(order.seq, 2);
    }

    #[test]
    fn a_chromium_only_transform_is_marked_in_the_order_and_the_view() {
        let cfg = RendersConfig::default();
        let mut r = with_host();
        r.change("v1", "c1", &Change::Transform("invert".into()), 1_000, &cfg)
            .unwrap();
        assert!(r.order(true).overlay.unwrap().chromium_only);
        assert!(r.view(&cfg).unwrap().overlay.unwrap().chromium_only);
    }

    #[test]
    fn a_name_outside_the_lists_is_refused_and_changes_nothing() {
        let cfg = RendersConfig::default();
        let mut r = with_host();
        let before = r.clone();
        assert!(r
            .change(
                "v1",
                "c1",
                &Change::Transform("filter:url(x)".into()),
                1_000,
                &cfg
            )
            .is_err());
        assert_eq!(r, before);
    }

    #[test]
    fn changes_are_limited_per_client_and_for_everyone() {
        let cfg = RendersConfig::default();
        let mut r = with_host();
        r.change("v1", "c1", &Change::Text("a".into()), 1_000, &cfg)
            .unwrap();
        // someone else, inside the interval for everyone
        assert!(r
            .change("v2", "c2", &Change::Text("b".into()), 5_000, &cfg)
            .unwrap_err()
            .contains("try again"));
        // the same client, past that one but inside its own
        assert_eq!(
            r.change("v1", "c1", &Change::Text("b".into()), 12_000, &cfg),
            Err("one change every 30 s".into())
        );
        assert!(r
            .change("v2", "c2", &Change::Text("b".into()), 12_000, &cfg)
            .is_ok());
    }

    #[test]
    fn the_overlay_ends_at_its_time_and_the_order_says_so() {
        let cfg = RendersConfig::default();
        let mut r = with_host();
        r.change("v1", "c1", &Change::Text("a".into()), 1_000, &cfg)
            .unwrap();
        r.tick(120_999);
        assert!(r.overlay.is_some());
        r.tick(121_000);
        assert!(r.overlay.is_none());
        assert_eq!(r.overlay_seq, 2);
        assert_eq!(
            r.order(true),
            RendersOrder {
                run: true,
                overlay: None
            }
        );
    }

    #[test]
    fn no_host_no_change_and_no_view() {
        let cfg = RendersConfig::default();
        let mut r = Renders::default();
        assert!(r.view(&cfg).is_none());
        assert_eq!(
            r.change("v1", "c1", &Change::Text("a".into()), 1_000, &cfg),
            Err("no host runs the pages now".into())
        );
    }

    #[test]
    fn a_report_from_a_host_without_the_mode_is_dropped() {
        let mut r = with_host();
        r.report("h2", &RendersReport::default(), 5);
        r.report(
            "h1",
            &RendersReport {
                at: 4,
                ..Default::default()
            },
            5,
        );
        assert_eq!(r.hosts.len(), 1);
        assert_eq!(r.hosts["h1"].report_at, 5);
    }

    #[test]
    fn overlay_text_is_plain_and_bounded() {
        assert_eq!(
            normalise_overlay("  hello   there "),
            Ok("hello there".into())
        );
        assert!(normalise_overlay("a\u{7}b").is_err());
        assert!(normalise_overlay("   ").is_err());
        assert!(normalise_overlay(&"x".repeat(MAX_OVERLAY_CHARS + 1)).is_err());
        // markup is text like any other: the host escapes it into the page
        assert_eq!(normalise_overlay("<b>hi</b>"), Ok("<b>hi</b>".into()));
    }
}
